// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VirtIO PCI transport (legacy + modern).
//!
//! Wraps a [`VirtioDevice`] and exposes it as a **transitional** PCI
//! device that supports the legacy (pre-1.0) and the modern (1.0+) PCI
//! transport.
//!
//! # Transitional device layout
//!
//! - **BAR0** (PIO): Legacy virtio config registers
//! - **BAR2** (MMIO, 16 KB): Modern config structures (4 pages):
//!   - Page 0 (0x0000): Common configuration
//!   - Page 1 (0x1000): Device-specific configuration
//!   - Page 2 (0x2000): Notification register
//!   - Page 3 (0x3000): ISR status register
//! - **BAR4** (MMIO): MSI-X table + PBA (when MSI-X present)
//!
//! PCI capabilities advertise the modern config structures. A modern
//! guest negotiates `VIRTIO_F_VERSION_1` (bit 32) and uses BAR2. A
//! legacy guest uses BAR0. Only the modern path advertises `EVENT_IDX`
//! and `INDIRECT_DESC`.
//!
//! # Where the parts are
//!
//! This module keeps what every path shares: the register state in
//! `VirtioPciState`, the reset latch in that state, and the reset
//! protocol that closes and opens the latch. Each submodule owns one
//! thing:
//!
//! - `bus`: the addresses this device decodes, and the accesses to
//!   them.
//! - `legacy`: the pre-1.0 register file on BAR0.
//! - `modern`: the capability chain and the BAR2 register file.
//! - `intr`: every interrupt this transport delivers, ordered against
//!   its own reset.
//! - `migrate`: the migration wire format, and the order in which a
//!   restore applies it.

use std::sync::{Arc, Mutex};

use vmm_core::mem::PhysMap;
use vmm_core::mmio::MmioBus;
use vmm_core::pio::PioBus;

use vmm_devices::pci::bar::BarDefine;
use vmm_devices::pci::device::{DeviceIdent, DeviceState};
use vmm_devices::pci::msix::MsixTable;
use vmm_devices::pci::{BarN, IntrPin, LintrCfg};
use vmm_devices::DeviceMigrateState;

use super::bits;
use super::queue::VirtQueue;
use super::VirtioDevice;

mod bus;
pub mod intr;
mod legacy;
mod migrate;
mod modern;

use intr::IntrGate;
use modern::CapOffsets;

/// Maximum number of virtqueues per device.
const MAX_QUEUES: usize = 4;

/// State shared between the PCI device and the VirtIO transport.
struct VirtioPciState {
    /// Selected queue index for configuration.
    queue_select: u16,
    /// Device status register.
    status: u8,
    /// Features the driver negotiated. The legacy transport sees only
    /// the low 32 bits.
    guest_features: u64,
    queues: Vec<VirtQueue>,
    /// MSI-X config vector (0xFFFF = no vector).
    config_msix_vector: u16,
    /// Per-queue MSI-X vectors (0xFFFF = no vector).
    queue_msix_vectors: Vec<u16>,

    // -- Modern transport state --
    /// Selects which 32-bit half of device features to read (0=low, 1=high).
    device_feature_select: u32,
    /// Selects which 32-bit half of driver features to write.
    driver_feature_select: u32,
    /// Whether the driver made each queue live.
    ///
    /// A modern driver writes QUEUE_ENABLE. A legacy driver has no such
    /// register, so a non-zero QUEUE_PFN sets the flag. Kicks are gated
    /// on the ring addresses, not on this flag. A migration export
    /// reads this flag to select the queues it carries.
    queue_enabled: Vec<bool>,
    /// Per-queue descriptor table addresses (modern transport).
    queue_desc: Vec<u64>,
    /// Per-queue available ring addresses (modern transport).
    queue_avail: Vec<u64>,
    /// Per-queue used ring addresses (modern transport).
    queue_used: Vec<u64>,
    /// Configuration generation counter.
    config_generation: u8,

    /// Set while a backend reset drains its guest-memory accesses.
    ///
    /// The drain runs without the transport lock so register reads
    /// still get served. Thus another vCPU can reach the device while
    /// it is half reset. The latch is in the locked state, so every
    /// write path tests it and changes the registers in one critical
    /// section. A write that finds the latch closed is dropped.
    resetting: bool,
}

impl VirtioPciState {
    fn new(num_queues: usize, queue_size: u16) -> Self {
        let queues = (0..num_queues)
            .map(|_| VirtQueue::new(queue_size))
            .collect();
        Self {
            queue_select: 0,
            status: 0,
            guest_features: 0,
            queues,
            config_msix_vector: bits::VIRTIO_MSI_NO_VECTOR,
            queue_msix_vectors: vec![bits::VIRTIO_MSI_NO_VECTOR; num_queues],
            device_feature_select: 0,
            driver_feature_select: 0,
            queue_enabled: vec![false; num_queues],
            queue_desc: vec![0; num_queues],
            queue_avail: vec![0; num_queues],
            queue_used: vec![0; num_queues],
            config_generation: 0,
            resetting: false,
        }
    }

    fn selected_queue(&self) -> Option<&VirtQueue> {
        self.queues.get(self.queue_select as usize)
    }

    fn selected_queue_mut(&mut self) -> Option<&mut VirtQueue> {
        self.queues.get_mut(self.queue_select as usize)
    }

    /// The legacy transport sees only the low 32 feature bits.
    fn guest_features_u32(&self) -> u32 {
        self.guest_features as u32
    }

    /// Tell the queues what the driver negotiated.
    ///
    /// Apply the ring features once, where negotiation ends, and never
    /// under a live ring. A `VirtioCompletion` copies EVENT_IDX when it
    /// is built. A later change makes it judge kicks by a rule that its
    /// queue does not use.
    fn apply_features(&mut self) {
        let event_idx =
            self.guest_features & bits::VIRTIO_F_RING_EVENT_IDX != 0;
        let indirect =
            self.guest_features & bits::VIRTIO_F_RING_INDIRECT_DESC != 0;
        for q in &mut self.queues {
            q.set_event_idx(event_idx);
            q.set_indirect_supported(indirect);
        }
    }

    fn reset(&mut self) {
        self.queue_select = 0;
        self.status = 0;
        self.guest_features = 0;
        self.config_msix_vector = bits::VIRTIO_MSI_NO_VECTOR;
        self.queue_msix_vectors.fill(bits::VIRTIO_MSI_NO_VECTOR);
        for q in &mut self.queues {
            q.reset();
        }
        self.device_feature_select = 0;
        self.driver_feature_select = 0;
        self.queue_enabled.fill(false);
        self.queue_desc.fill(0);
        self.queue_avail.fill(0);
        self.queue_used.fill(0);
        // `resetting` does not change here. The reset path owns the
        // latch and opens it only after every field above is published.
    }

    /// The device status the driver may read.
    ///
    /// During a reset, this read comes from a vCPU other than the one
    /// that asked for the reset. A driver frees the vring when it reads
    /// 0 (VirtIO 1.3 sec 4.1.4.3.1), then writes ACKNOWLEDGE. Neither
    /// is safe while the backend drains. A driver that probes an unused
    /// device resets it from status 0, so without this bit the status
    /// reads 0 for the whole drain.
    fn readable_status(&self) -> u8 {
        if self.resetting {
            self.status | bits::STATUS_ACKNOWLEDGE
        } else {
            self.status
        }
    }
}

/// One-shot park points a test installs to hold a thread at a named
/// step of the reset protocol. The first thread to reach a point takes
/// the hook and runs it. Thus a test can pin an interleaving that
/// otherwise depends on the scheduler.
#[cfg(test)]
#[derive(Default)]
struct TestParks {
    /// In a register write path, after the reset-latch check passes
    /// and before the write changes transport state.
    write_checked: ParkSlot,
    /// After the transport lock drops and before the config interrupt
    /// is delivered. A reset can run in this window.
    config_interrupt_pending: ParkSlot,
    /// Between the session sample for an MSI-X unmask and the delivery
    /// of what the unmask released. A whole reset can run here.
    released_delivery_pending: ParkSlot,
    /// In the post-restore kick, after it drops the transport lock. A
    /// whole reset can run here.
    post_restore_kick_pending: ParkSlot,
}

#[cfg(test)]
type ParkSlot = Mutex<Option<Arc<dyn Fn() + Send + Sync>>>;

/// Run a park point, if a test installed one, and clear it.
#[cfg(test)]
fn run_park(slot: &ParkSlot) {
    let hook = slot.lock().expect("park lock poisoned").take();
    if let Some(hook) = hook {
        hook();
    }
}

/// PCI device that wraps a VirtIO backend.
///
/// Implements [`PciDevice`](vmm_devices::pci::device::PciDevice) so
/// the PCI bus can attach it. Transitional device: BAR0 (PIO) for the
/// legacy transport, BAR2 (MMIO) for the modern transport.
pub struct VirtioPciDevice<D: VirtioDevice> {
    pci_state: Mutex<DeviceState>,
    virtio_state: Mutex<VirtioPciState>,
    device: D,
    /// Workers set bits with `fetch_or` and a guest read clears them
    /// with `swap(0)`, so workers and vCPUs do not contend on a lock.
    isr_status: std::sync::atomic::AtomicU8,
    intr_pin: Option<Arc<dyn IntrPin>>,
    /// `None` means legacy INTx only.
    msix: Option<Arc<MsixTable>>,
    /// Per-queue MSI-X vectors, copied from the guest's register
    /// writes. The interrupt path reads them without `virtio_state`,
    /// because completion callbacks can fire inside `notify_queue`
    /// while `virtio_state` is held.
    msix_queue_vectors: Vec<std::sync::atomic::AtomicU16>,
    msix_config_vector: std::sync::atomic::AtomicU16,
    physmap: Arc<PhysMap>,
    bus_pio: Arc<PioBus>,
    bus_mmio: Arc<MmioBus>,
    /// BAR0 PIO base port that is registered now.
    registered_bar: Mutex<Option<u16>>,
    /// BAR2 MMIO base address that is registered now.
    registered_bar2: Mutex<Option<u64>>,
    /// BAR4 MMIO base address that is registered now.
    registered_bar4: Mutex<Option<u64>>,
    /// Weak self-reference for BAR registration closures.
    self_ref: Mutex<Option<std::sync::Weak<Self>>>,
    log: slog::Logger,
    /// Set after the first refused queue is logged at `warn`. The
    /// refusing write is guest driven and repeatable, so later reports
    /// go to `debug`.
    queue_refusal_reported: std::sync::atomic::AtomicBool,
    /// Orders every interrupt this device delivers against its own
    /// reset.
    intr: Arc<IntrGate>,
    #[cfg(test)]
    parks: TestParks,
    /// Capability chain offsets in PCI config space.
    cap_offsets: CapOffsets,
    /// Size of the device-specific config region, for the capability.
    dev_config_size: u16,
    num_queues: u16,
}

impl<D: VirtioDevice> VirtioPciDevice<D> {
    /// The backend behind this transport.
    pub fn device(&self) -> &D {
        &self.device
    }

    /// Create a transitional (legacy + modern) VirtIO PCI device.
    ///
    /// # Arguments
    ///
    /// - `device` - The VirtIO backend implementation
    /// - `dev_type` - VirtIO device type (e.g., VIRTIO_DEV_TYPE_BLOCK)
    /// - `num_queues` - Number of virtqueues
    /// - `queue_size` - Size of each virtqueue (must be power of 2)
    /// - `dev_config_size` - Size of device-specific config region
    /// - `lintr_cfg` - Legacy interrupt pin configuration
    /// - `physmap` - Guest physical memory map
    /// - `bus_pio` - PIO bus for BAR0
    /// - `bus_mmio` - MMIO bus for BAR2 (modern config) and BAR4 (MSI-X)
    /// - `msix` - Optional MSI-X table
    ///
    /// Bus registration failures go to a discarding logger. Use
    /// [`new_with_logger`](Self::new_with_logger) to see them.
    pub fn new(
        device: D,
        dev_type: u16,
        num_queues: usize,
        queue_size: u16,
        dev_config_size: u16,
        lintr_cfg: Option<LintrCfg>,
        physmap: Arc<PhysMap>,
        bus_pio: Arc<PioBus>,
        bus_mmio: Arc<MmioBus>,
        msix: Option<Arc<MsixTable>>,
    ) -> Arc<Self> {
        Self::new_with_logger(
            device,
            dev_type,
            num_queues,
            queue_size,
            dev_config_size,
            lintr_cfg,
            physmap,
            bus_pio,
            bus_mmio,
            msix,
            slog::Logger::root(slog::Discard, slog::o!()),
        )
    }

    /// Same as [`new`](Self::new), with a logger for bus registration
    /// diagnostics.
    pub fn new_with_logger(
        device: D,
        dev_type: u16,
        num_queues: usize,
        queue_size: u16,
        dev_config_size: u16,
        lintr_cfg: Option<LintrCfg>,
        physmap: Arc<PhysMap>,
        bus_pio: Arc<PioBus>,
        bus_mmio: Arc<MmioBus>,
        msix: Option<Arc<MsixTable>>,
        log: slog::Logger,
    ) -> Arc<Self> {
        assert!(
            num_queues <= MAX_QUEUES,
            "too many queues (max {})",
            MAX_QUEUES
        );

        // A transitional device uses a legacy-range PCI device id and
        // advertises virtio PCI capabilities for modern discovery.
        let pci_device_id = bits::transitional_device_id(dev_type);
        let (pci_class, pci_subclass) = bits::pci_class_for(dev_type);

        let ident = DeviceIdent {
            vendor_id: bits::VIRTIO_PCI_VENDOR_ID,
            device_id: pci_device_id,
            class: pci_class,
            subclass: pci_subclass,
            prog_if: 0,
            revision: 0,
            sub_vendor_id: bits::VIRTIO_PCI_VENDOR_ID,
            sub_device_id: dev_type,
        };

        let mut pci_state = DeviceState::new(ident);

        // BAR0: PIO for legacy virtio config space
        let common_size = if msix.is_some() {
            bits::LEGACY_COMMON_SIZE_MSIX
        } else {
            bits::LEGACY_COMMON_SIZE
        };
        let bar_size = common_size + dev_config_size;
        let bar_size_aligned = bar_size.next_power_of_two().max(32);
        pci_state.define_bar(BarN::BAR0, BarDefine::Pio(bar_size_aligned));

        // BAR2: MMIO for modern transport config (16 KB, 4 pages)
        pci_state
            .define_bar(BarN::BAR2, BarDefine::Mmio(bits::MODERN_BAR_SIZE));

        // BAR4: MMIO for MSI-X table + PBA
        if let Some(ref msix_table) = msix {
            let msix_bar_size = msix_table.bar_size() as u32;
            pci_state.define_bar(
                BarN::BAR4,
                BarDefine::Mmio(msix_bar_size.next_power_of_two()),
            );
        }

        // Capability chain: virtio PCI caps (+ MSI-X if present)
        let cap_offsets = CapOffsets::new(msix.is_some());
        pci_state.set_cap_ptr(cap_offsets.first);

        let intr_pin = if let Some((pin_id, pin)) = lintr_cfg {
            pci_state.set_intr_pin(pin_id as u8);
            pci_state.set_intr_line(10);
            Some(pin)
        } else {
            None
        };

        let msix_queue_vectors: Vec<std::sync::atomic::AtomicU16> = (0
            ..num_queues)
            .map(|_| {
                std::sync::atomic::AtomicU16::new(bits::VIRTIO_MSI_NO_VECTOR)
            })
            .collect();

        let dev = Arc::new(Self {
            pci_state: Mutex::new(pci_state),
            virtio_state: Mutex::new(VirtioPciState::new(
                num_queues, queue_size,
            )),
            device,
            isr_status: std::sync::atomic::AtomicU8::new(0),
            intr_pin,
            msix,
            msix_queue_vectors,
            msix_config_vector: std::sync::atomic::AtomicU16::new(
                bits::VIRTIO_MSI_NO_VECTOR,
            ),
            physmap,
            bus_pio,
            bus_mmio,
            registered_bar: Mutex::new(None),
            registered_bar2: Mutex::new(None),
            registered_bar4: Mutex::new(None),
            self_ref: Mutex::new(None),
            log,
            queue_refusal_reported: std::sync::atomic::AtomicBool::new(false),
            intr: Arc::new(IntrGate::new()),
            #[cfg(test)]
            parks: TestParks::default(),
            cap_offsets,
            dev_config_size,
            num_queues: num_queues as u16,
        });
        *dev.self_ref.lock().expect("self_ref lock") =
            Some(Arc::downgrade(&dev));
        dev
    }

    /// Take the transport lock, unless a reset is draining.
    ///
    /// Every guest write goes through here. The latch is part of the
    /// locked state, so the test and the lock are one step. A write
    /// either runs before the reset closes the latch or is dropped.
    fn lock_for_write(
        &self,
    ) -> Option<std::sync::MutexGuard<'_, VirtioPciState>> {
        let vs = self.virtio_state.lock().expect("virtio lock poisoned");
        if vs.resetting {
            return None;
        }
        #[cfg(test)]
        run_park(&self.parks.write_checked);
        Some(vs)
    }

    /// Whether a kick may reach the backend.
    ///
    /// A queue with no ring has nothing to drain. Also, a backend that
    /// builds its used-ring writer on the first kick keeps it for the
    /// whole session. A kick before the ring is set builds the writer
    /// on address zero, and the device never writes the real ring.
    fn can_notify(vs: &VirtioPciState, queue_idx: u16) -> bool {
        vs.status & bits::STATUS_DRIVER_OK != 0
            && vs
                .queues
                .get(usize::from(queue_idx))
                .is_some_and(VirtQueue::is_configured)
    }

    /// Whether a queue refused the ring addresses on its first use.
    ///
    /// A queue programmed outside the enable path, for example by a
    /// migration restore, is checked only when the device first reads
    /// the ring. A refused queue does no more work, so the driver must
    /// be told instead of left waiting.
    fn queue_needs_reset(vs: &VirtioPciState, queue_idx: u16) -> bool {
        vs.queues
            .get(queue_idx as usize)
            .is_some_and(|q| q.needs_reset())
    }

    /// Refuse a queue the guest just programmed and tell it why.
    ///
    /// The status bit is set under the guard that refused the queue.
    /// The guard is then consumed, so the caller cannot hold it across
    /// the interrupt. If DEVICE_NEEDS_RESET is already set, nothing
    /// happens: the write is guest driven and repeatable without limit,
    /// so one write must not raise one interrupt. The refusal is logged
    /// at `warn` once per device, because a guest can reset and
    /// reprogram in a loop.
    fn refuse_queue(
        &self,
        vs: std::sync::MutexGuard<'_, VirtioPciState>,
        idx: usize,
        error: Option<crate::queue::QueueAddrError>,
    ) {
        if !self.set_needs_reset(vs) {
            return;
        }
        if let Some(error) = error {
            let first = !self
                .queue_refusal_reported
                .swap(true, std::sync::atomic::Ordering::Relaxed);
            if first {
                slog::warn!(self.log, "virtio-pci refused a queue";
                    "queue" => idx, "reason" => %error);
            } else {
                slog::debug!(self.log, "virtio-pci refused a queue";
                    "queue" => idx, "reason" => %error);
            }
        }
    }

    /// Apply a non-zero DEVICE_STATUS write, on either transport.
    ///
    /// VirtIO 1.3 sec 2.1.2: the driver must not clear a status bit.
    /// If it could, a driver could toggle FEATURES_OK and renegotiate
    /// under programmed rings, or clear DEVICE_NEEDS_RESET and continue
    /// on a stopped device. The only way back is a write of 0, which
    /// the caller handles.
    ///
    /// Negotiation ends at the FEATURES_OK transition (sec 2.2.1), so
    /// the backend and the rings get the features there. A legacy
    /// driver never writes that bit, so `legacy` applies its features
    /// at the feature register write.
    fn set_status(
        &self,
        mut vs: std::sync::MutexGuard<'_, VirtioPciState>,
        new_status: u8,
    ) {
        if new_status & vs.status != vs.status {
            self.set_needs_reset(vs);
            return;
        }
        let gained = new_status & !vs.status;
        vs.status = new_status;
        if gained & bits::STATUS_FEATURES_OK != 0 {
            vs.apply_features();
            let features = vs.guest_features;
            self.device.set_features(features);
        }
    }

    /// Set DEVICE_NEEDS_RESET and tell the driver, once.
    ///
    /// Returns false when the bit is already set. The write that gets
    /// here is guest driven and repeatable, so one write must not raise
    /// one interrupt. The bit is set under the guard, which is then
    /// consumed. Set after the guard drops, the bit could land in a
    /// reset drain or on the session after the reset.
    fn set_needs_reset(
        &self,
        mut vs: std::sync::MutexGuard<'_, VirtioPciState>,
    ) -> bool {
        if vs.status & bits::STATUS_DEVICE_NEEDS_RESET != 0 {
            return false;
        }
        vs.status |= bits::STATUS_DEVICE_NEEDS_RESET;
        // Sample under the guard that wrote the bit, so a reset that
        // clears the bit always comes after this sample.
        let session = self.intr.session();
        drop(vs);
        self.raise_config_interrupt_in(session);
        true
    }

    /// Install the one-shot park a register write reaches once its
    /// reset-latch check has passed.
    #[cfg(test)]
    fn park_write_after_latch_check(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.parks.write_checked.lock().expect("park lock poisoned") =
            Some(hook);
    }

    /// Install the one-shot park an MSI-X unmask reaches once it has
    /// taken the message out of the pending array.
    #[cfg(test)]
    fn park_before_released_delivery(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self
            .parks
            .released_delivery_pending
            .lock()
            .expect("park lock poisoned") = Some(hook);
    }

    /// Install the one-shot park the post-restore kick reaches once it
    /// has dropped the transport lock.
    #[cfg(test)]
    fn park_before_restore_kick(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self
            .parks
            .post_restore_kick_pending
            .lock()
            .expect("park lock poisoned") = Some(hook);
    }

    /// Install the one-shot park a config interrupt reaches once its
    /// caller has dropped the transport lock.
    #[cfg(test)]
    fn park_before_config_interrupt(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self
            .parks
            .config_interrupt_pending
            .lock()
            .expect("park lock poisoned") = Some(hook);
    }

    /// Reset the device, on the vCPU that wrote DEVICE_STATUS.
    ///
    /// A legacy driver writes the register once and does not poll.
    /// illumos `virtio_legacy_device_reset_locked()` is that one write,
    /// and `virtio_shutdown()` frees the ring DMA immediately after it.
    /// Thus the write must not return while the backend can still reach
    /// guest memory. A modern driver polls for 0 (VirtIO 1.3 sec
    /// 4.1.4.3.1), and the same order satisfies it.
    ///
    /// The wait is short by design. A backing-store operation owns only
    /// host memory. The backend reset ends the request epoch and waits
    /// only for the guest-memory accesses that hold it, not for the
    /// I/O behind them.
    ///
    /// The latch closes first because the backend reset runs without
    /// the transport lock. While the latch is closed, every register
    /// write is dropped and `readable_status` is non-zero. Thus a
    /// second vCPU cannot act on a half-reset device.
    ///
    /// The caller holds the transport lock, so the latch closes in the
    /// critical section that tested it.
    fn reset_device(&self, mut vs: std::sync::MutexGuard<'_, VirtioPciState>) {
        if vs.resetting {
            return;
        }
        vs.resetting = true;
        vs.status &= !bits::STATUS_DRIVER_OK;
        vs.queue_enabled.fill(false);
        // The driver session ends here, and interrupt admission shuts.
        // An interrupt from an earlier event must not reach the next
        // driver. Admission stays shut for all of `finish_reset`, so
        // the set that the reset waits for cannot grow.
        self.intr.end_session();
        drop(vs);

        self.finish_reset();
    }

    /// Finish the reset the guest asked for, and publish the result.
    ///
    /// The backend wait has no deadline. It covers only guest-memory
    /// accesses, so it is bounded by design. Also, a device that stops
    /// waiting cannot take back a write that a worker already started
    /// into the guest chain.
    fn finish_reset(&self) {
        self.device.reset();

        // Wait for every delivery this session admitted. Admission
        // shut before the backend reset, so the set is at most one
        // injection per thread already inside, and a guest cannot add
        // threads.
        //
        // The count is the only bound. The set is finite, but each
        // member can take long. Both injection ioctls take the VM lock
        // as readers, and an illumos reader yields to a waiting writer.
        // Memory hot-add holds that writer for hundreds of ms per GiB,
        // so a reset beside a hot-add waits for it. Measured: 1.3 us
        // idle, 17.9 ms beside a 256 MiB hot-add.
        // A deadline does not help. Admission is tested before the
        // ioctl, so a dispatched injection cannot be recalled, and an
        // early return puts it in the next driver session.
        self.intr.settle();

        // The next driver must find the line deasserted, or it sees a
        // spurious assertion when it enables the IOAPIC pin. The ISR
        // and the pin change together, so a driver on another vCPU
        // cannot read the ISR between them.
        {
            let _line = self.intr.line();
            self.isr_status
                .store(0, std::sync::atomic::Ordering::Relaxed);
            self.lower_interrupt();
        }
        // Otherwise messages that the old session left pending go out
        // on the next unmask, which is a guest write this reset does
        // not gate.
        if let Some(ref msix) = self.msix {
            msix.clear_pending();
        }

        let mut vs = self.virtio_state.lock().expect("virtio lock poisoned");
        vs.reset();
        self.msix_config_vector.store(
            bits::VIRTIO_MSI_NO_VECTOR,
            std::sync::atomic::Ordering::Relaxed,
        );
        for a in &self.msix_queue_vectors {
            a.store(
                bits::VIRTIO_MSI_NO_VECTOR,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        // Status 0 and the open latch become visible together. The
        // driver writes ACKNOWLEDGE next, and that write must not find
        // the latch closed. Interrupt admission opens in the same
        // critical section, so no register write gets through before
        // deliveries are admitted again.
        vs.resetting = false;
        self.intr.reopen();
        drop(vs);
    }

    /// Signal the guest that the device needs a reset.
    ///
    /// A reset in progress stops the device anyway, so the bit is
    /// dropped instead of written onto the next session.
    pub fn signal_device_needs_reset(&self) {
        let Some(vs) = self.lock_for_write() else {
            return;
        };
        self.set_needs_reset(vs);
    }
}

// ---------------------------------------------------------------------------
// Lifecycle delegation: VM lifecycle calls go to the inner device.
// ---------------------------------------------------------------------------

use vmm_devices::migrate::Migrator;
use vmm_devices::Lifecycle;
impl<D: VirtioDevice + Lifecycle> Lifecycle for VirtioPciDevice<D> {
    fn type_name(&self) -> &'static str {
        Lifecycle::type_name(&self.device)
    }

    fn lifecycle_state(
        &self,
    ) -> Option<vmm_devices::lifecycle::IndicatedState> {
        Lifecycle::lifecycle_state(&self.device)
    }

    fn start(&self) -> anyhow::Result<()> {
        Lifecycle::start(&self.device)
    }

    fn pause(&self) {
        Lifecycle::pause(&self.device)
    }

    fn is_quiesced(&self) -> bool {
        Lifecycle::is_quiesced(&self.device)
    }

    fn resume(&self) {
        Lifecycle::resume(&self.device)
    }

    fn reset(&self) {
        Lifecycle::reset(&self.device)
    }

    fn halt(&self) {
        Lifecycle::halt(&self.device);
    }

    fn pause_for_migration(
        &self,
    ) -> Result<(), vmm_devices::lifecycle::DeviceStateError> {
        self.device.pause_rings_for_export()
    }

    fn flush_backing(
        &self,
        intent: vmm_devices::FlushIntent,
    ) -> Result<(), vmm_devices::FlushError> {
        Lifecycle::flush_backing(&self.device, intent)
    }

    fn post_restore_kick(&self) {
        // Raise an interrupt on each enabled queue after the vCPUs
        // run, so the guest's NAPI handler runs and refills rx buffers.
        //
        // The session is sampled under the lock. This runs once, on its
        // own thread, about 100 ms after the vCPUs start, so the guest
        // can reset the device between the queue read and the raise. A
        // reset takes this lock to end the session. Thus the sampled
        // session names the driver whose queues were read, and a raise
        // for an ended session is refused, not sent to the next driver.
        let vs = self.virtio_state.lock().expect("virtio lock");
        let session = self.intr.session();
        let enabled: Vec<u16> = (0..self.num_queues)
            .filter(|&idx| {
                vs.queue_enabled.get(idx as usize).copied().unwrap_or(false)
            })
            .collect();
        drop(vs);
        #[cfg(test)]
        run_park(&self.parks.post_restore_kick_pending);
        for idx in enabled {
            self.raise_queue_interrupt_in(session, idx);
        }
    }

    fn migrate(&'_ self) -> Migrator<'_> {
        Lifecycle::migrate(&self.device)
    }

    fn export_migrate_state(
        &self,
    ) -> Result<
        Option<DeviceMigrateState>,
        vmm_devices::lifecycle::DeviceStateError,
    > {
        Ok(Some(DeviceMigrateState::Virtio(
            self.export_migrate_queues()?,
        )))
    }

    fn restore_migrate_state(
        &self,
        state: &DeviceMigrateState,
    ) -> Result<(), vmm_devices::lifecycle::DeviceStateError> {
        match state {
            DeviceMigrateState::Virtio(state) => {
                self.restore_migrate_queues(state)
            }
            other => Err(vmm_devices::lifecycle::DeviceStateError::WrongKind {
                want: "virtio",
                got: other.kind(),
            }),
        }
    }

    fn resume_after_migration(
        &self,
    ) -> Result<(), vmm_devices::lifecycle::DeviceStateError> {
        self.device.resume_rings_after_migration()
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod reset_tests;
