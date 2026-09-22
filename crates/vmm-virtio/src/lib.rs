// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VirtIO device framework.
//!
//! Implements the VirtIO specification for guest-facing virtual devices
//! on a transitional (legacy + modern) PCI transport. It provides:
//!
//! - [`VirtioDevice`] trait for backend device implementations
//! - [`queue::VirtQueue`] - split virtqueue ring buffer
//! - [`pci::VirtioPciDevice`] - PCI transport wrapping a VirtioDevice
//! - Backends: [`VirtioBlock`], [`VirtioConsole`], [`VirtioFs`],
//!   [`VirtioRng`], [`VirtioViona`] and [`vsock`]
//!
//! # Architecture
//!
//! ```text
//! +-----------------------+
//! |  VirtioPciDevice<D>   |  <-- PciDevice trait (attached to PciBus)
//! |  +-----------------+  |
//! |  | BAR0 / BAR2     |  |  <-- legacy PIO / modern MMIO registers
//! |  | VirtQueue(s)    |  |  <-- split virtqueue in guest memory
//! |  +-----------------+  |
//! |  |  VirtioDevice D |  |  <-- backend (e.g., VirtioBlock)
//! |  +-----------------+  |
//! +-----------------------+
//! ```

mod access;
pub mod attach;
pub mod bits;
pub mod block;
pub mod chain_io;
pub mod console;
pub mod fs;
pub mod pci;
pub mod queue;
pub mod rng;
pub(crate) mod socket_accept;
mod socket_halt;
pub mod viona;
pub mod vsock;

pub use block::VirtioBlock;
pub use console::VirtioConsole;
pub use fs::VirtioFs;
pub use pci::VirtioPciDevice;
pub use queue::VirtQueue;
pub use rng::VirtioRng;
pub use viona::VirtioViona;

use vmm_core::mem::PhysMap;

/// Read up to four little-endian bytes of a device config at `offset`.
///
/// The guest selects the offset and the width, so the sum is checked,
/// and every byte past the end of `cfg` reads as zero.
pub fn cfg_read_bytes(cfg: &[u8], offset: u16, len: u8) -> u32 {
    let mut out = 0u32;
    for i in 0..usize::from(len).min(4) {
        let Some(byte) = usize::from(offset)
            .checked_add(i)
            .and_then(|off| cfg.get(off))
        else {
            break;
        };
        out |= u32::from(*byte) << (i * 8);
    }
    out
}

pub(crate) use queue::queue_drain_should_stop;

/// Trait implemented by VirtIO backend devices.
///
/// A VirtioDevice supplies the device-specific logic (features, config
/// space, request processing). The PCI transport ([`VirtioPciDevice`])
/// handles the PCI identity, the BAR layout and virtqueue management.
pub trait VirtioDevice: Send + Sync + 'static {
    /// The device feature bits. The legacy transport sees only the low
    /// 32.
    fn device_features(&self) -> u64;

    /// Called when the guest writes the negotiated features.
    fn set_features(&self, features: u64);

    /// Read from device-specific configuration space.
    ///
    /// `offset` is relative to the start of the device-specific region
    /// (BAR0 offset 0x14 or 0x18 on the legacy transport, BAR2 page 1
    /// on the modern transport).
    fn cfg_read(&self, offset: u16, len: u8) -> u32;

    /// Write to device-specific configuration space.
    ///
    /// The transport holds its lock during this call, so the write
    /// cannot land during a reset. An implementation must not take
    /// that lock again or call
    /// [`VirtioPciDevice::signal_device_needs_reset`], because that
    /// deadlocks.
    ///
    /// [`VirtioPciDevice::signal_device_needs_reset`]:
    ///     crate::pci::VirtioPciDevice::signal_device_needs_reset
    fn cfg_write(&self, offset: u16, val: u32, len: u8);

    /// Process a single request from a virtqueue.
    ///
    /// Called when the guest notifies a queue. The device walks the
    /// descriptor chain at `head`, does the request, and returns the
    /// number of bytes written to device-writable buffers.
    fn process_queue(
        &self,
        queue_idx: u16,
        queue: &mut VirtQueue,
        head: u16,
        physmap: &PhysMap,
    ) -> u32;

    /// Handle a queue notification from the guest.
    ///
    /// The default pops available descriptors, calls
    /// [`process_queue`](Self::process_queue) for each, pushes used
    /// ring entries, and returns `true` when an interrupt is due.
    ///
    /// Kernel-accelerated devices (e.g., viona) override this to send
    /// the notification to the kernel driver.
    fn notify_queue(
        &self,
        queue_idx: u16,
        queues: &mut [VirtQueue],
        physmap: &PhysMap,
    ) -> bool {
        let queue = match queues.get_mut(queue_idx as usize) {
            Some(q) => q,
            None => return false,
        };

        let old_used_idx = queue.read_used_idx(physmap);
        let max_requests = usize::from(queue.size());
        let popped = queue.drain_avail(physmap, max_requests, |queue, head| {
            let written = self.process_queue(queue_idx, queue, head, physmap);
            queue.push_used(physmap, head, written);
        });
        popped > 0 && queue.should_notify_guest(physmap, old_used_idx)
    }

    /// Called when the guest programs or retires a queue's ring.
    ///
    /// Kernel-accelerated devices tell the kernel where the ring is.
    /// Devices that keep a used-ring writer per queue drop it here,
    /// because the writer is bound to the old ring. The default does
    /// nothing.
    fn queue_addr_set(&self, _queue_idx: u16, _queue: &VirtQueue) {}

    /// The `(avail_idx, used_idx)` a kernel backend holds for one ring.
    ///
    /// `None` means this device keeps no cursors of its own, so the
    /// export reads them from the [`VirtQueue`] and guest memory. A
    /// backend that owns them returns `Some` for every live ring. Zero
    /// is a valid value in either position, because both cursors are
    /// wrapping u16 counters. An error fails the export, because the
    /// userspace cursors cannot replace the ones the kernel moved.
    fn kernel_ring_indices(
        &self,
        _queue_idx: u16,
    ) -> Result<Option<(u16, u16)>, vmm_devices::lifecycle::DeviceStateError>
    {
        Ok(None)
    }

    /// Stop every ring before a migration restore programs them.
    ///
    /// A worker left running on the destination reads a ring whose
    /// addresses change under it, so a failure here must fail the
    /// restore.
    fn reset_all_rings(
        &self,
    ) -> Result<(), vmm_devices::lifecycle::DeviceStateError> {
        Ok(())
    }

    /// Program one ring's addresses, cursors and MSI-X message from a
    /// migration payload.
    ///
    /// The source commits the migration on this result, so every
    /// state the backend could not program must reach the caller. A
    /// false success gives the guest a NIC that moves nothing or
    /// raises no interrupt, and no source remains to go back to.
    fn restore_ring_state(
        &self,
        _queue_idx: u16,
        _size: u16,
        _desc: u64,
        _avail: u64,
        _used: u64,
        _avail_idx: u16,
        _used_idx: u16,
        _msix_addr: u64,
        _msix_data: u32,
    ) -> Result<(), vmm_devices::lifecycle::DeviceStateError> {
        Ok(())
    }

    /// Pause kernel ring threads for migration export.
    ///
    /// Runs before the vCPUs pause, so no ring entry is consumed
    /// between the pause and the export. A failure fails the
    /// migration, because the indices the export reads still move.
    fn pause_rings_for_export(
        &self,
    ) -> Result<(), vmm_devices::lifecycle::DeviceStateError> {
        Ok(())
    }

    /// Restart the rings that [`Self::pause_rings_for_export`] stopped,
    /// after a migration that failed after the pause.
    ///
    /// A paused kernel ring is `VRS_STOP`, and viona answers a guest
    /// kick on it with EBUSY. Without this, the source guest resumes
    /// with a NIC whose rings never move again.
    fn resume_rings_after_migration(
        &self,
    ) -> Result<(), vmm_devices::lifecycle::DeviceStateError> {
        Ok(())
    }

    /// Program notification addresses after migration.
    fn set_notify_addrs(&self, _pio_port: u16, _mmio_addr: u64) {}

    /// Start the deferred interrupt poll thread after a migration
    /// restore. Kernel-accelerated devices (viona) skip this thread at
    /// device creation.
    fn start_poll_deferred(&self) {}

    /// Reset the device to its initial state.
    ///
    /// The transport calls this from the vCPU that wrote
    /// `DEVICE_STATUS`, and publishes 0 when it returns. A legacy
    /// driver does not poll for that 0: illumos frees the ring DMA
    /// immediately after the write. So this must not return while any
    /// worker can still read or write the guest ring or a guest chain.
    /// The transport cannot check that.
    ///
    /// It must NOT wait for backend I/O. A backing-store operation
    /// owns only host memory. Wait only for the guest-memory accesses
    /// (memcpys and ring publication), and let the I/O finish into host
    /// buffers that a later epoch discards. This keeps the wait short
    /// enough to run on a vCPU.
    fn reset(&self);
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::queue::VirtqDesc;
    use super::*;

    const DESC_GPA: u64 = 0x1000;
    const AVAIL_GPA: u64 = 0x1100;
    const USED_GPA: u64 = 0x1200;

    struct TestDevice {
        processed: AtomicUsize,
        max_expected: usize,
    }

    impl TestDevice {
        fn new(max_expected: usize) -> Self {
            Self {
                processed: AtomicUsize::new(0),
                max_expected,
            }
        }

        fn processed(&self) -> usize {
            self.processed.load(Ordering::Relaxed)
        }
    }

    impl VirtioDevice for TestDevice {
        fn device_features(&self) -> u64 {
            0
        }

        fn set_features(&self, _features: u64) {}

        fn cfg_read(&self, _offset: u16, _len: u8) -> u32 {
            0
        }

        fn cfg_write(&self, _offset: u16, _val: u32, _len: u8) {}

        fn process_queue(
            &self,
            _queue_idx: u16,
            _queue: &mut VirtQueue,
            _head: u16,
            _physmap: &PhysMap,
        ) -> u32 {
            let processed = self.processed.fetch_add(1, Ordering::Relaxed) + 1;
            assert!(processed <= self.max_expected, "queue drain exceeded cap");
            0
        }

        fn reset(&self) {}
    }

    fn configured_queue(size: u16, avail_idx: u16) -> (PhysMap, VirtQueue) {
        let physmap =
            PhysMap::new_anon(DESC_GPA, 0x3000).expect("create queue memory");
        let mut queue = VirtQueue::new(size);
        queue.set_addr_modern(DESC_GPA, AVAIL_GPA, USED_GPA);
        queue.set_event_idx(true);

        physmap
            .lookup(AVAIL_GPA + 2, 2)
            .expect("mapped avail index")
            .write::<u16>(&avail_idx)
            .expect("write avail index");
        for idx in 0..size {
            physmap
                .lookup(AVAIL_GPA + 4 + u64::from(idx) * 2, 2)
                .expect("mapped avail entry")
                .write::<u16>(&idx)
                .expect("write avail entry");
            physmap
                .lookup(DESC_GPA + u64::from(idx) * 16, 16)
                .expect("mapped descriptor")
                .write(&VirtqDesc {
                    addr: 0,
                    len: 0,
                    flags: 0,
                    next: 0,
                })
                .expect("write descriptor");
        }

        (physmap, queue)
    }

    #[test]
    fn notify_queue_terminates_when_avail_ring_entry_is_unmapped() {
        const REGION_GPA: u64 = 0x4000;
        const REGION_SIZE: usize = 0x1000;
        const REGION_END: u64 = REGION_GPA + REGION_SIZE as u64;

        let physmap = PhysMap::new_anon(REGION_GPA, REGION_SIZE)
            .expect("create guest memory");
        let mut queue = VirtQueue::new(2);
        queue.set_addr_modern(REGION_GPA, REGION_END - 4, REGION_GPA + 0x100);
        queue.set_event_idx(true);
        physmap
            .lookup(REGION_END - 2, 2)
            .expect("mapped avail index")
            .write::<u16>(&1)
            .expect("write avail index");

        let before = queue.last_avail_idx();
        assert_eq!(queue.pop_avail(&physmap), None);
        queue.update_used_event(&physmap);

        assert_eq!(queue.last_avail_idx(), before);
        assert!(queue.has_new_avail(&physmap));
        assert!(
            queue_drain_should_stop(&queue, before, false, &physmap),
            "the EVENT_IDX re-check must stop when pop_avail makes no progress",
        );

        // End to end: notify_queue must return. Run it on a worker, so
        // a hang fails this test on a timeout and does not stop the
        // suite.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("virtio-drain-timeout".into())
            .spawn(move || {
                let device = TestDevice::new(0);
                let mut queues = [queue];
                device.notify_queue(0, &mut queues, &physmap);
                let _ = tx.send(());
            })
            .expect("spawn drain worker");
        rx.recv_timeout(std::time::Duration::from_secs(10)).expect(
            "notify_queue must return when the avail ring entry is unmapped",
        );
    }

    #[test]
    fn default_notify_queue_is_capped_at_queue_size() {
        let (physmap, queue) = configured_queue(2, 4);
        let device = TestDevice::new(2);
        let mut queues = [queue];

        device.notify_queue(0, &mut queues, &physmap);

        assert_eq!(device.processed(), 2);
        assert_eq!(queues[0].last_avail_idx(), 2);
        assert!(queues[0].has_new_avail(&physmap));
        // The cap exit must still arm the kick for the entries left.
        let avail_event_gpa = USED_GPA + 4 + u64::from(queues[0].size()) * 8;
        let avail_event = physmap
            .lookup(avail_event_gpa, 2)
            .expect("mapped avail event")
            .read::<u16>()
            .expect("read avail event");
        assert_eq!(avail_event, 2);
    }

    #[test]
    fn default_notify_queue_drains_ring_and_updates_avail_event() {
        let (physmap, queue) = configured_queue(4, 2);
        let device = TestDevice::new(4);
        let mut queues = [queue];

        device.notify_queue(0, &mut queues, &physmap);

        let avail_event_gpa = USED_GPA + 4 + u64::from(queues[0].size()) * 8;
        let avail_event = physmap
            .lookup(avail_event_gpa, 2)
            .expect("mapped avail event")
            .read::<u16>()
            .expect("read avail event");
        assert_eq!(device.processed(), 2);
        assert_eq!(queues[0].last_avail_idx(), 2);
        assert_eq!(queues[0].read_used_ring_idx(&physmap), 2);
        assert_eq!(avail_event, 2);
    }
}
