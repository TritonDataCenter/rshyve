// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VirtIO network device backed by the viona kernel driver.
//!
//! Viona processes packets in the kernel. Userspace does these tasks:
//!
//! 1. Open `/dev/viona` and bind it to a VNIC and the VMM fd.
//! 2. Program the virtqueue ring addresses into the kernel.
//! 3. Forward queue notifications (kicks) to the kernel.
//! 4. Supply the virtio-net device config (MAC address, status).
//!
//! Queue 0 is RX. Queue 1 is TX.
//!
//! - [`halt`]: the teardown order, which gives the VM its hold back
//! - [`poll`]: the interrupt poll thread
//! - [`migrate`]: the ring state a migration moves
//! - [`setup`]: opening the link and pointing the kernel at it
//!
//! Every kernel call goes through [`viona_api::LinkOps`] on every
//! platform. On illumos an open `VionaFd` implements it. Tests supply
//! their own link and run the same device code.

use std::sync::{Arc, Mutex};

use vmm_core::mem::PhysMap;

use super::pci::intr::{BackendIntr, IntrSlot};
use super::queue::VirtQueue;
use super::VirtioDevice;
use viona_api::LinkOps;

mod halt;
mod migrate;
mod poll;
mod setup;

#[cfg(test)]
mod tests;

/// Size of the virtio-net device config space.
///
/// Layout: `[0..6]` MAC address, `[6..8]` status (u16).
pub const NET_CONFIG_SIZE: u16 = 8;

/// Number of virtqueues: RX + TX.
pub const NET_NUM_QUEUES: usize = 2;

pub const NET_QUEUE_SIZE: u16 = 256;

const ETHERADDRL: usize = 6;

/// Attempts at a ring reset before its error is accepted.
///
/// Only `EINTR` is retried, and each signal delivery interrupts the
/// kernel wait once. The bound stops a signal storm. It does not wait
/// for the kernel.
const RESET_ATTEMPTS: usize = 8;

// virtio-net feature bits (virtio 1.3 §5.1.3).
const VIRTIO_NET_F_MAC: u32 = 1 << 5;
const VIRTIO_NET_F_STATUS: u32 = 1 << 16;

const VIRTIO_NET_S_LINK_UP: u16 = 1;

/// Per-ring initialization state.
///
/// A kick to a ring in `Init` is dropped to prevent a kernel NULL
/// pointer dereference.
#[derive(Clone, Copy, Debug, PartialEq)]
enum RingState {
    /// No addresses programmed.
    Init,
    /// `ring_init` succeeded. The kernel holds the addresses.
    Ready,
    /// The kernel refused to run this ring.
    ///
    /// No ioctl goes to it until a device reset. A refusal does not
    /// heal on its own, and the guest controls the rate of kicks and
    /// ring writes. An ioctl and a log line per refusal would let the
    /// guest flood the host log.
    ///
    /// A ring the kernel refuses to *retire* never gets this state.
    /// The kernel still holds its pages, so the link is destroyed.
    Error,
}

/// What a migration export left a kernel ring in.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum PausedRing {
    /// No export stopped this ring, or a rollback restarted it.
    #[default]
    Running,
    /// Stopped, with the state a rollback programs back into it.
    Saved(viona_api::vioc_ring_state),
    /// Stopped, with no state to program back.
    ///
    /// Only a reset and a state write restart a paused ring, so this
    /// ring stays stopped. A rollback reports it as an error, because
    /// the guest NIC cannot move again.
    Lost,
}

/// VirtIO network device backed by the illumos viona kernel driver.
///
/// Only illumos can open a link, so [`VirtioViona::new`] fails
/// elsewhere. Everything above the link is the same code on every
/// platform.
pub struct VirtioViona {
    inner: Mutex<VionaInner>,

    /// The transport interrupt path, installed after the transport
    /// exists.
    ///
    /// Outside `inner` because the poll thread reads it on every wakeup
    /// and must not wait on the device lock. The halt drops that lock
    /// across an untimed kernel call.
    interrupt: Arc<IntrSlot>,
}

/// Device state under the device lock.
struct VionaInner {
    /// The in-kernel link. On illumos, the open `/dev/viona` handle.
    ///
    /// Shared, so the halt keeps the link after it drops the device
    /// lock. The halt makes untimed kernel calls.
    link: Arc<dyn LinkOps>,

    mac_addr: [u8; ETHERADDRL],

    /// Features the kernel driver offers.
    dev_features: u32,

    /// Features the guest negotiated.
    negotiated_features: u64,

    /// Per-ring initialization state (RX=0, TX=1).
    ring_state: [RingState; NET_NUM_QUEUES],

    /// The last kick to this ring was refused.
    ///
    /// Limits the warning to one line per run of refusals, whatever the
    /// guest kick rate.
    kick_refused: [bool; NET_NUM_QUEUES],
    /// Each ring's kernel state when a migration paused it.
    ///
    /// A migration that fails after the pause must restart the rings.
    /// A kick cannot: the kernel returns EBUSY for a stopped ring. Only
    /// the saved addresses and indices can.
    paused_rings: [PausedRing; NET_NUM_QUEUES],

    /// On a migration destination, the logger for the poll thread that
    /// starts after the ring state is restored.
    deferred_poll: Option<slog::Logger>,

    poller: Option<halt::Poller>,

    /// The logger the interrupt wiring carried.
    ///
    /// `VirtioViona::new` gets none, so a halt reports through the
    /// first logger the device received.
    log: Option<slog::Logger>,

    /// The link is destroyed. Nothing may program it again.
    halted: bool,
}

impl VionaInner {
    /// The link, by value, so a caller can drop the device lock and
    /// keep it.
    fn link(&self) -> Arc<dyn LinkOps> {
        Arc::clone(&self.link)
    }

    /// The link, borrowed for one call under the device lock.
    fn link_ref(&self) -> &dyn LinkOps {
        self.link.as_ref()
    }

    /// Where a halt reports, discarding when the device got no logger.
    fn log(&self) -> slog::Logger {
        self.log
            .clone()
            .unwrap_or_else(|| slog::Logger::root(slog::Discard, slog::o!()))
    }
}

impl VirtioViona {
    /// Install the transport interrupt path once the transport exists.
    pub fn install_interrupt(&self, intr: Arc<BackendIntr>) {
        self.interrupt.install(intr);
    }

    /// The slot the poll thread reads, for a test that runs the thread
    /// body by hand.
    #[cfg(test)]
    pub(super) fn interrupt_slot(&self) -> Arc<IntrSlot> {
        Arc::clone(&self.interrupt)
    }

    /// Kick an in-kernel virtqueue ring.
    ///
    /// A kick to an out-of-range queue, before feature negotiation, or
    /// to a ring without addresses is dropped. This prevents a NULL
    /// pointer dereference in the kernel mac layer.
    fn ring_kick(inner: &mut VionaInner, queue_idx: u16) {
        let idx = queue_idx as usize;
        if idx >= NET_NUM_QUEUES {
            return;
        }
        if inner.negotiated_features == 0 {
            return;
        }
        if inner.ring_state[idx] != RingState::Ready {
            return;
        }
        match inner.link_ref().ring_kick(queue_idx) {
            Ok(()) => inner.kick_refused[idx] = false,
            // The kernel refuses a kick to a ring it does not run, and
            // a guest can spin on QUEUE_NOTIFY, so log once per run.
            // No quarantine: a migration export pauses the kernel rings
            // but leaves them `Ready`, and an aborted export must not
            // leave a dead NIC.
            Err(e) => {
                if !inner.kick_refused[idx] {
                    inner.kick_refused[idx] = true;
                    slog::warn!(inner.log(), "viona ring kick refused";
                        "ring" => queue_idx, "error" => %e);
                }
            }
        }
    }

    /// Reset one kernel ring, retrying while a signal interrupts it.
    ///
    /// `viona_ioc_ring_reset` waits interruptibly, so `EINTR` means
    /// only that this thread took a signal. It is not a refusal, and
    /// treating it as one would strand a ring the kernel can release.
    fn ring_reset_uninterrupted(
        inner: &VionaInner,
        queue_idx: u16,
    ) -> std::io::Result<()> {
        for _ in 1..RESET_ATTEMPTS {
            match inner.link_ref().ring_reset(queue_idx) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                rc => return rc,
            }
        }
        inner.link_ref().ring_reset(queue_idx)
    }

    /// Program the ring addresses for a queue into the kernel.
    ///
    /// A legacy driver retires a ring by writing `QUEUE_PFN = 0`, and
    /// the kernel refuses `RING_INIT` on a ring that is not reset. So a
    /// running kernel ring is reset first, whatever the driver wrote.
    /// Otherwise the kernel keeps using pages the guest reclaimed.
    ///
    /// Returns a ring the kernel refused to reset. The guest reclaims
    /// the pages of a ring it retires, so the caller must stop the link.
    fn ring_init(
        inner: &mut VionaInner,
        queue_idx: u16,
        queue: &VirtQueue,
    ) -> Option<(u16, std::io::Error)> {
        let idx = queue_idx as usize;
        if idx >= NET_NUM_QUEUES || inner.ring_state[idx] == RingState::Error {
            return None;
        }

        if inner.ring_state[idx] == RingState::Ready {
            if let Err(e) = Self::ring_reset_uninterrupted(inner, queue_idx) {
                return Some((queue_idx, e));
            }
            inner.ring_state[idx] = RingState::Init;
        }

        if !queue.is_configured() {
            return None;
        }

        let rc = inner.link_ref().ring_init(
            queue_idx,
            queue.size(),
            queue.desc_addr(),
            queue.avail_addr(),
            queue.used_addr(),
        );
        match rc {
            Ok(()) => inner.ring_state[idx] = RingState::Ready,
            // Userspace validated these addresses, so the driver cannot
            // fix a refusal by writing them again. The kernel started no
            // worker and holds no guest pages: quarantine until reset.
            Err(e) => {
                slog::warn!(inner.log(),
                    "viona ring init refused, quarantining";
                    "ring" => queue_idx, "error" => %e);
                inner.ring_state[idx] = RingState::Error;
            }
        }
        None
    }

    /// Set negotiated features in the kernel driver.
    ///
    /// A migration restore needs the error: `VERSION_1` selects the
    /// ring layout it restores, so a refusal is not only a warning.
    fn set_kernel_features(
        inner: &VionaInner,
        features: u64,
    ) -> std::io::Result<()> {
        inner.link_ref().set_features(features)
    }
}

impl VirtioDevice for VirtioViona {
    fn device_features(&self) -> u64 {
        let inner = self.inner.lock().expect("viona lock poisoned");
        let mut features = u64::from(inner.dev_features);
        features |= u64::from(VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS);
        // Viona runs the rings in the kernel and does its own
        // notification suppression, so userspace does not offer
        // EVENT_IDX.
        features &= !super::bits::VIRTIO_F_RING_EVENT_IDX;
        features
    }

    fn set_features(&self, features: u64) {
        let mut inner = self.inner.lock().expect("viona lock poisoned");
        // A halt in progress dropped this lock and is in an untimed
        // kernel call. An ioctl here waits for that call while it holds
        // this guard, and every other vCPU waits for the guard: a
        // hot-unplug stall.
        if inner.halted {
            return;
        }
        inner.negotiated_features = features;
        // The guest can write the register again, so log the refusal
        // and let the driver decide.
        if let Err(e) = Self::set_kernel_features(&inner, features) {
            slog::warn!(inner.log(), "viona set features failed";
                "features" => features, "error" => %e);
        }
    }

    fn cfg_read(&self, offset: u16, len: u8) -> u32 {
        let inner = self.inner.lock().expect("viona lock poisoned");

        let status = VIRTIO_NET_S_LINK_UP;
        let cfg: [u8; 8] = [
            inner.mac_addr[0],
            inner.mac_addr[1],
            inner.mac_addr[2],
            inner.mac_addr[3],
            inner.mac_addr[4],
            inner.mac_addr[5],
            (status & 0xFF) as u8,
            (status >> 8) as u8,
        ];

        crate::cfg_read_bytes(&cfg, offset, len)
    }

    fn cfg_write(&self, _offset: u16, _val: u32, _len: u8) {
        // virtio-net config is read-only.
    }

    fn process_queue(
        &self,
        _queue_idx: u16,
        _queue: &mut VirtQueue,
        _head: u16,
        _physmap: &PhysMap,
    ) -> u32 {
        // Not called: `notify_queue` is overridden and the kernel
        // processes the descriptors.
        0
    }

    fn notify_queue(
        &self,
        queue_idx: u16,
        _queues: &mut [VirtQueue],
        _physmap: &PhysMap,
    ) -> bool {
        let mut inner = self.inner.lock().expect("viona lock poisoned");
        Self::ring_kick(&mut inner, queue_idx);

        // The kernel raises the interrupts, not the PCI transport.
        false
    }

    fn kernel_ring_indices(
        &self,
        queue_idx: u16,
    ) -> Result<Option<(u16, u16)>, vmm_devices::lifecycle::DeviceStateError>
    {
        // Never `None`: viona owns both cursors of every ring it runs,
        // and the userspace copies are not valid substitutes.
        self.kernel_ring_state(queue_idx).map(Some)
    }

    fn reset_all_rings(
        &self,
    ) -> Result<(), vmm_devices::lifecycle::DeviceStateError> {
        self.reset_all_rings()
    }

    fn restore_ring_state(
        &self,
        queue_idx: u16,
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
        avail_idx: u16,
        used_idx: u16,
        msix_addr: u64,
        msix_data: u32,
    ) -> Result<(), vmm_devices::lifecycle::DeviceStateError> {
        self.ring_set_state(
            queue_idx, size, desc, avail, used, avail_idx, used_idx, msix_addr,
            msix_data,
        )
    }

    fn pause_rings_for_export(
        &self,
    ) -> Result<(), vmm_devices::lifecycle::DeviceStateError> {
        self.pause_rings()
    }

    fn resume_rings_after_migration(
        &self,
    ) -> Result<(), vmm_devices::lifecycle::DeviceStateError> {
        self.resume_rings()
    }

    fn set_notify_addrs(&self, pio_port: u16, mmio_addr: u64) {
        self.set_notify_addrs(pio_port, mmio_addr);
    }

    fn start_poll_deferred(&self) {
        self.start_poll_deferred();
    }

    fn queue_addr_set(&self, queue_idx: u16, queue: &VirtQueue) {
        let stuck = {
            let mut inner = self.inner.lock().expect("viona lock poisoned");
            // The halt destroys the link with this lock dropped, so a
            // guest write can arrive during the halt. A ring marked
            // `Ready` then sends the next kick to a destroyed link.
            if inner.halted {
                return;
            }
            Self::ring_init(&mut inner, queue_idx, queue)
        };

        // A legacy driver frees the ring DMA after it writes
        // QUEUE_PFN = 0, so a ring the kernel refused to reset takes the
        // link with it.
        let Some((ring, err)) = stuck else {
            return;
        };
        self.destroy_stopped_ring(ring, &err);
    }

    fn reset(&self) {
        let stuck = {
            let mut inner = self.inner.lock().expect("viona lock poisoned");
            // As in `set_features`: no ioctl under this guard after the
            // halt released it. The halt already cleared the fields
            // below, and nothing sets them again.
            if inner.halted {
                return;
            }
            inner.negotiated_features = 0;
            // A device reset ends a quarantine, so warn again if the
            // kernel refuses the new rings.
            inner.kick_refused = [false; NET_NUM_QUEUES];

            // Stop every kernel worker before the guest programs the
            // rings again. The transport publishes DEVICE_STATUS 0 when
            // this returns, and an illumos driver then frees the ring
            // DMA at once. A ring not proved stopped stops the link.
            let mut stuck = None;
            for ring in 0..NET_NUM_QUEUES as u16 {
                match Self::ring_reset_uninterrupted(&inner, ring) {
                    Ok(()) => inner.ring_state[ring as usize] = RingState::Init,
                    Err(e) => {
                        stuck = Some((ring, e));
                        break;
                    }
                }
            }
            stuck
        };

        // The device lock is dropped first. The destroy joins the poll
        // thread and makes an untimed kernel call, and a vCPU that holds
        // this guard through that blocks every other vCPU.
        let Some((ring, err)) = stuck else {
            return;
        };
        self.destroy_stopped_ring(ring, &err);
    }
}
