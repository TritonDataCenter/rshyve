// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Transport tests that build a device on real buses or real guest
//! memory: BAR moves and teardown, a blocking backend reset, and ring
//! address validation.

use super::*;

// -- BAR teardown tests --

const TEST_BAR0_PORT: u16 = 0x2000;
const TEST_BAR2_ADDR: u64 = 0xE000_0000;

/// Build a device and return the buses it registers on.
fn make_device_with_buses() -> (
    Arc<VirtioPciDevice<TestVirtioDevice>>,
    Arc<PioBus>,
    Arc<MmioBus>,
) {
    let bus_pio = Arc::new(PioBus::new());
    let bus_mmio = Arc::new(MmioBus::new());
    let dev = VirtioPciDevice::new(
        TestVirtioDevice { features: 0 },
        bits::VIRTIO_DEV_TYPE_BLOCK,
        1,
        256,
        8,
        None,
        Arc::new(PhysMap::new()),
        Arc::clone(&bus_pio),
        Arc::clone(&bus_mmio),
        None,
    );
    (dev, bus_pio, bus_mmio)
}

/// Program BAR0 and BAR2, then enable IO and MMIO decoding.
fn enable_bars(dev: &Arc<VirtioPciDevice<TestVirtioDevice>>) {
    dev.cfg_write(0x10, 4, u32::from(TEST_BAR0_PORT));
    dev.cfg_write(0x18, 4, TEST_BAR2_ADDR as u32);
    let cmd = vmm_devices::pci::bits::RegCmd::IO_EN
        | vmm_devices::pci::bits::RegCmd::MMIO_EN;
    dev.cfg_write(0x04, 2, u32::from(cmd.bits()));
    assert!(dev.registered_bar.lock().expect("bar lock").is_some());
    assert!(dev.registered_bar2.lock().expect("bar2 lock").is_some());
}

/// Every live BAR handler holds a strong self reference, so the
/// device cannot drop while one stays registered.
#[test]
fn bar_registration_holds_a_strong_self_reference() {
    let (dev, _bus_pio, _bus_mmio) = make_device_with_buses();
    assert_eq!(Arc::strong_count(&dev), 1);
    enable_bars(&dev);
    // One handler for BAR0 (PIO) and one for BAR2 (MMIO).
    assert_eq!(Arc::strong_count(&dev), 3);
}

/// A backend whose reset blocks until the test releases it. It models a
/// backend that must drain its guest-memory accesses.
struct BlockingResetDevice {
    entered: std::sync::mpsc::Sender<()>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    /// The thread that ran the reset.
    reset_thread: Mutex<Option<std::thread::ThreadId>>,
}

impl VirtioDevice for BlockingResetDevice {
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
        0
    }

    fn reset(&self) {
        *self.reset_thread.lock().expect("reset thread lock") =
            Some(std::thread::current().id());
        // A closed channel means the test already finished.
        let _ = self.entered.send(());
        let _ = self
            .release
            .lock()
            .expect("release lock")
            .recv_timeout(std::time::Duration::from_secs(10));
    }
}

impl Lifecycle for BlockingResetDevice {
    fn type_name(&self) -> &'static str {
        "blocking-reset"
    }
}

// A backend reset must drain its I/O workers before it returns, and
// the driver polls DEVICE_STATUS until it reads 0 (VirtIO 1.3 sec
// 4.1.4.3.1). Holding the transport lock across that wait stalls
// every other vCPU that touches this device.
#[test]
fn a_slow_backend_reset_does_not_hold_the_transport_lock() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use vmm_core::common::{ReadOp, WriteOp};

    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let dev = VirtioPciDevice::new(
        BlockingResetDevice {
            entered: entered_tx,
            release: Mutex::new(release_rx),
            reset_thread: Mutex::new(None),
        },
        bits::VIRTIO_DEV_TYPE_BLOCK,
        1,
        256,
        8,
        None,
        Arc::new(PhysMap::new()),
        Arc::new(PioBus::new()),
        Arc::new(MmioBus::new()),
        None,
    );

    let writer = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || {
            let wo = WriteOp::from_buf(&[0]);
            dev.bar_rw(BarN::BAR0, 0x12, RWOp::Write(&wo));
        })
    };
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the backend reset ran");

    let reader = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || {
            let mut ro = ReadOp::new(1);
            dev.bar_rw(BarN::BAR0, 0x12, RWOp::Read(&mut ro));
        })
    };
    let deadline = Instant::now() + Duration::from_millis(500);
    while !reader.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let served = reader.is_finished();

    release_tx.send(()).expect("release the reset");
    writer.join().expect("status write");
    reader.join().expect("status read");
    assert!(served, "a register read blocked behind the backend reset");
}

// A legacy driver reclaims the ring DMA directly after its one write to
// DEVICE_STATUS. The transport lock is released across the backend
// drain, so other registers stay readable. Until the drain ends, a
// second vCPU must not read 0 or program the device again. The writing
// vCPU must find the reset finished when its write returns.
#[test]
fn a_draining_reset_is_not_visible_to_another_vcpu() {
    use std::sync::mpsc;
    use std::time::Duration;
    use vmm_core::common::{ReadOp, WriteOp};

    const READY: u8 = bits::STATUS_ACKNOWLEDGE
        | bits::STATUS_DRIVER
        | bits::STATUS_FEATURES_OK;

    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let dev = VirtioPciDevice::new(
        BlockingResetDevice {
            entered: entered_tx,
            release: Mutex::new(release_rx),
            reset_thread: Mutex::new(None),
        },
        bits::VIRTIO_DEV_TYPE_BLOCK,
        1,
        256,
        8,
        None,
        Arc::new(PhysMap::new()),
        Arc::new(PioBus::new()),
        Arc::new(MmioBus::new()),
        None,
    );

    let status = |dev: &Arc<VirtioPciDevice<BlockingResetDevice>>| {
        let mut ro = ReadOp::new(1);
        dev.bar_rw(BarN::BAR0, 0x12, RWOp::Read(&mut ro));
        ro.buf()[0]
    };
    let write_status = |dev: &Arc<VirtioPciDevice<BlockingResetDevice>>,
                        val: u8| {
        let wo = WriteOp::from_buf(&[val]);
        dev.bar_rw(BarN::BAR0, 0x12, RWOp::Write(&wo));
    };

    write_status(&dev, READY);
    let writer = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || {
            let wo = WriteOp::from_buf(&[0]);
            dev.bar_rw(BarN::BAR0, 0x12, RWOp::Write(&wo));
            std::thread::current().id()
        })
    };
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the backend reset ran");

    assert_ne!(
        status(&dev),
        0,
        "the guest saw the reset finish while the backend was draining"
    );
    // A second vCPU that initialises the device here would get the
    // completion built on the ring that the drain still writes.
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);
    assert_eq!(
        status(&dev) & bits::STATUS_DRIVER_OK,
        0,
        "a re-init landed while the backend was still draining"
    );

    release_tx.send(()).expect("release the reset");
    let vcpu = writer.join().expect("status write");
    // The write ran the whole reset, so no poll is necessary. An
    // illumos driver reads no status here.
    assert_eq!(
        *dev.device.reset_thread.lock().expect("reset thread lock"),
        Some(vcpu),
        "the backend reset ran off the vCPU that wrote DEVICE_STATUS"
    );
    assert_eq!(status(&dev), 0, "the write returned before the reset ended");
}

/// A BAR the guest moves must stop decoding at its old address.
///
/// A guest can reassign BARs at any time and give the old window to
/// another function. A stale registration would make this device serve
/// accesses aimed at that function. The registration record and the bus
/// must agree at every step, so an update releases before it registers.
#[test]
fn moving_a_bar_takes_the_old_window_off_the_bus() {
    const MOVED_BAR0_PORT: u16 = 0x3000;
    const MOVED_BAR2_ADDR: u64 = 0xE100_0000;

    let (dev, bus_pio, bus_mmio) = make_device_with_buses();
    enable_bars(&dev);

    // A live window reads 0: legacy DEVICE_FEATURES is 0 for this
    // backend, and modern DEVICE_FEATURE_SELECT starts at 0. An
    // unmapped window reads all ones.
    assert_eq!(bus_pio.handle_in(TEST_BAR0_PORT, 1), 0, "BAR0 is not live");
    assert_eq!(
        bus_mmio.handle_read(TEST_BAR2_ADDR, 4),
        0,
        "BAR2 is not live"
    );

    dev.cfg_write(0x10, 4, u32::from(MOVED_BAR0_PORT));
    dev.cfg_write(0x18, 4, MOVED_BAR2_ADDR as u32);

    assert_eq!(
        bus_pio.handle_in(TEST_BAR0_PORT, 1),
        0xFF,
        "BAR0 still decodes the port the guest gave up"
    );
    assert_eq!(
        bus_mmio.handle_read(TEST_BAR2_ADDR, 4),
        u64::from(u32::MAX),
        "BAR2 still decodes the address the guest gave up"
    );

    // The device answers only at the new windows.
    assert_eq!(
        bus_pio.handle_in(MOVED_BAR0_PORT, 1),
        0,
        "BAR0 does not decode where the guest put it"
    );
    assert_eq!(
        bus_mmio.handle_read(MOVED_BAR2_ADDR, 4),
        0,
        "BAR2 does not decode where the guest put it"
    );

    // One handler per BAR, as before the move. A leaked registration
    // holds an extra strong reference.
    assert_eq!(Arc::strong_count(&dev), 3);
}

#[test]
fn detach_regions_lets_the_device_drop() {
    let (dev, bus_pio, bus_mmio) = make_device_with_buses();
    enable_bars(&dev);

    let weak = Arc::downgrade(&dev);
    dev.detach_regions();
    // A second call must not unregister a base twice.
    dev.detach_regions();
    assert_eq!(Arc::strong_count(&dev), 1);

    drop(dev);
    assert!(weak.upgrade().is_none(), "device outlived its last handle");

    // The buses must no longer decode the old BAR ranges.
    assert_eq!(bus_pio.handle_in(TEST_BAR0_PORT, 1), 0xFF);
    assert_eq!(bus_mmio.handle_read(TEST_BAR2_ADDR, 4), u64::from(u32::MAX));
}

// -- Ring address validation --

/// Guest memory for one 256-entry split ring: 4 KiB of descriptors,
/// then the available and used rings.
const RING_GPA: u64 = 0x4000;
const RING_BYTES: usize = 0x4000;

/// A transport with mapped guest memory, so a ring can use an address
/// the device can reach.
fn device_with_ram() -> Arc<VirtioPciDevice<TestVirtioDevice>> {
    let physmap = Arc::new(
        PhysMap::new_anon(RING_GPA, RING_BYTES).expect("create guest memory"),
    );
    VirtioPciDevice::new(
        TestVirtioDevice { features: 0 },
        bits::VIRTIO_DEV_TYPE_BLOCK,
        1,
        256,
        8,
        None,
        physmap,
        Arc::new(PioBus::new()),
        Arc::new(MmioBus::new()),
        None,
    )
}

/// Program queue 0's three ring addresses through the modern common
/// config, then write QUEUE_ENABLE.
fn enable_queue_0(
    dev: &Arc<VirtioPciDevice<TestVirtioDevice>>,
    desc: u64,
    avail: u64,
    used: u64,
) {
    use vmm_core::common::WriteOp;
    let write = |offset: u16, val: u32| {
        let wo = WriteOp::from_buf(&val.to_le_bytes());
        dev.bar_rw(BarN::BAR2, offset.into(), RWOp::Write(&wo));
    };
    write(bits::COMMON_CFG_QUEUE_SELECT, 0);
    write(bits::COMMON_CFG_QUEUE_DESC_LO, desc as u32);
    write(bits::COMMON_CFG_QUEUE_DESC_HI, (desc >> 32) as u32);
    write(bits::COMMON_CFG_QUEUE_AVAIL_LO, avail as u32);
    write(bits::COMMON_CFG_QUEUE_AVAIL_HI, (avail >> 32) as u32);
    write(bits::COMMON_CFG_QUEUE_USED_LO, used as u32);
    write(bits::COMMON_CFG_QUEUE_USED_HI, (used >> 32) as u32);
    write(bits::COMMON_CFG_QUEUE_ENABLE, 1);
}

fn device_status(dev: &Arc<VirtioPciDevice<TestVirtioDevice>>) -> u8 {
    dev.virtio_state.lock().expect("virtio lock").status
}

fn queue_0_enabled(dev: &Arc<VirtioPciDevice<TestVirtioDevice>>) -> bool {
    dev.virtio_state.lock().expect("virtio lock").queue_enabled[0]
}

/// The control case: the device accepts a reachable ring and does not
/// set DEVICE_NEEDS_RESET.
#[test]
fn a_queue_whose_rings_are_mapped_is_enabled() {
    let dev = device_with_ram();

    enable_queue_0(&dev, RING_GPA, RING_GPA + 0x1000, RING_GPA + 0x2000);

    assert!(queue_0_enabled(&dev));
    assert_eq!(device_status(&dev) & bits::STATUS_DEVICE_NEEDS_RESET, 0);
}

/// A ring outside mapped memory fails every device access while the
/// driver still reads DRIVER_OK. The device refuses it at enable time
/// and raises DEVICE_NEEDS_RESET.
#[test]
fn a_queue_enable_over_unmapped_memory_is_refused() {
    let dev = device_with_ram();

    // The used ring needs 4 + 8 * 256 bytes and there is no RAM there.
    enable_queue_0(&dev, RING_GPA, RING_GPA + 0x1000, RING_GPA + 0x3F00);

    assert!(!queue_0_enabled(&dev), "the queue must not go live");
    assert_ne!(
        device_status(&dev) & bits::STATUS_DEVICE_NEEDS_RESET,
        0,
        "the driver was never told the device stopped",
    );
}

/// The same refusal for a ring address that breaks the spec alignment.
#[test]
fn a_queue_enable_with_a_misaligned_ring_is_refused() {
    let dev = device_with_ram();

    enable_queue_0(&dev, RING_GPA + 1, RING_GPA + 0x1000, RING_GPA + 0x2000);

    assert!(!queue_0_enabled(&dev));
    assert_ne!(device_status(&dev) & bits::STATUS_DEVICE_NEEDS_RESET, 0);
}

/// The legacy transport programs one PFN. The device refuses a PFN
/// whose computed rings leave guest memory.
#[test]
fn a_legacy_pfn_outside_guest_memory_is_refused() {
    use vmm_core::common::WriteOp;
    let dev = device_with_ram();

    let wo = WriteOp::from_buf(&0u32.to_le_bytes());
    dev.bar_rw(
        BarN::BAR0,
        bits::LEGACY_REG_QUEUE_SELECT.into(),
        RWOp::Write(&wo),
    );
    // Page 8 is past the 16 KiB this device maps.
    let wo = WriteOp::from_buf(&8u32.to_le_bytes());
    dev.bar_rw(
        BarN::BAR0,
        bits::LEGACY_REG_QUEUE_PFN.into(),
        RWOp::Write(&wo),
    );

    assert_ne!(device_status(&dev) & bits::STATUS_DEVICE_NEEDS_RESET, 0);
    assert!(!dev.virtio_state.lock().expect("virtio lock").queues[0]
        .is_configured());
}

/// A legacy PFN the device can reach still programs the ring.
#[test]
fn a_legacy_pfn_inside_guest_memory_is_accepted() {
    use vmm_core::common::WriteOp;
    let dev = device_with_ram();

    let wo = WriteOp::from_buf(&0u32.to_le_bytes());
    dev.bar_rw(
        BarN::BAR0,
        bits::LEGACY_REG_QUEUE_SELECT.into(),
        RWOp::Write(&wo),
    );
    let pfn = (RING_GPA / vmm_core::common::PAGE_SIZE as u64) as u32;
    let wo = WriteOp::from_buf(&pfn.to_le_bytes());
    dev.bar_rw(
        BarN::BAR0,
        bits::LEGACY_REG_QUEUE_PFN.into(),
        RWOp::Write(&wo),
    );

    assert_eq!(device_status(&dev) & bits::STATUS_DEVICE_NEEDS_RESET, 0);
    assert!(
        dev.virtio_state.lock().expect("virtio lock").queues[0].is_configured()
    );
}

/// Legacy has no QUEUE_ENABLE, so a non-zero PFN makes a queue live.
/// If the export marks the queue disabled, the destination does not
/// restore or wake it, and a migrated legacy guest loses its disk.
#[test]
fn a_legacy_pfn_makes_its_queue_live_for_a_migration_export() {
    use vmm_core::common::WriteOp;
    let dev = device_with_ram();

    let wo = WriteOp::from_buf(&0u32.to_le_bytes());
    dev.bar_rw(
        BarN::BAR0,
        bits::LEGACY_REG_QUEUE_SELECT.into(),
        RWOp::Write(&wo),
    );
    let pfn = (RING_GPA / vmm_core::common::PAGE_SIZE as u64) as u32;
    let wo = WriteOp::from_buf(&pfn.to_le_bytes());
    dev.bar_rw(
        BarN::BAR0,
        bits::LEGACY_REG_QUEUE_PFN.into(),
        RWOp::Write(&wo),
    );

    assert!(queue_0_enabled(&dev));
    let exported = dev.export_migrate_queues().expect("exports");
    assert_eq!(
        exported.queues.len(),
        1,
        "the programmed queue was not exported"
    );
    assert!(
        exported.queues[0].live,
        "a programmed legacy queue exported as disabled",
    );

    // A driver writes PFN 0 to retire the ring.
    let wo = WriteOp::from_buf(&0u32.to_le_bytes());
    dev.bar_rw(
        BarN::BAR0,
        bits::LEGACY_REG_QUEUE_PFN.into(),
        RWOp::Write(&wo),
    );

    assert!(!queue_0_enabled(&dev));
    assert!(
        dev.export_migrate_queues()
            .expect("exports")
            .queues
            .is_empty(),
        "a retired ring was still carried",
    );
}

/// A queue programmed outside the enable path, as a migration restore
/// does, is checked only when the device first reads the ring. The
/// device must then tell the driver, not leave it waiting on a dead
/// queue.
#[test]
fn a_queue_that_refuses_its_rings_on_first_use_raises_needs_reset() {
    use vmm_core::common::WriteOp;
    let dev = device_with_ram();
    {
        let mut vs = dev.virtio_state.lock().expect("virtio lock");
        // Past the mapped 16 KiB, so every ring access fails.
        vs.queues[0].set_addr_modern(
            RING_GPA + 0x3000,
            RING_GPA + 0x3800,
            RING_GPA + 0x3C00,
        );
        vs.queue_enabled[0] = true;
        vs.status = bits::STATUS_DRIVER_OK;
    }

    let wo = WriteOp::from_buf(&0u32.to_le_bytes());
    dev.bar_rw(BarN::BAR2, 0x2000, RWOp::Write(&wo));

    assert_ne!(
        device_status(&dev) & bits::STATUS_DEVICE_NEEDS_RESET,
        0,
        "the driver was never told the queue had stopped",
    );
}

/// The same first use, over the legacy BAR. The two notify paths are
/// separate code, and illumos guests use only this one.
#[test]
fn a_legacy_first_use_of_a_refused_queue_raises_needs_reset() {
    use vmm_core::common::WriteOp;
    let dev = device_with_ram();
    {
        let mut vs = dev.virtio_state.lock().expect("virtio lock");
        // Past the mapped 16 KiB, so every ring access fails.
        vs.queues[0].set_addr_modern(
            RING_GPA + 0x3000,
            RING_GPA + 0x3800,
            RING_GPA + 0x3C00,
        );
        vs.queue_enabled[0] = true;
        vs.status = bits::STATUS_DRIVER_OK;
    }

    let wo = WriteOp::from_buf(&0u16.to_le_bytes());
    dev.bar_rw(
        BarN::BAR0,
        bits::LEGACY_REG_QUEUE_NOTIFY.into(),
        RWOp::Write(&wo),
    );

    assert_ne!(
        device_status(&dev) & bits::STATUS_DEVICE_NEEDS_RESET,
        0,
        "a legacy guest was never told the queue had stopped",
    );
}

/// Records every MSI-X message the transport delivers.
struct RecordingSink(Mutex<Vec<(u64, u64)>>);

impl vmm_devices::pci::msix::MsiSink for RecordingSink {
    fn send(&self, addr: u64, data: u64) {
        self.0.lock().expect("recording lock").push((addr, data));
    }
}

/// A device with one unmasked MSI-X vector, set as the config vector as
/// a modern driver sets it.
fn device_with_msix() -> (
    Arc<VirtioPciDevice<TestVirtioDevice>>,
    Arc<RecordingSink>,
    Arc<MsixTable>,
) {
    use vmm_core::common::WriteOp;

    let sink = Arc::new(RecordingSink(Mutex::new(Vec::new())));
    let msix = Arc::new(MsixTable::new(1, Arc::clone(&sink) as _));
    msix.set_enabled(true);
    msix.write_entry(0, 0xFEE0_0000, 0x4021);
    // Clear the vector mask the entry starts with.
    msix.table_write(0x0C, 0);

    let physmap = Arc::new(
        PhysMap::new_anon(RING_GPA, RING_BYTES).expect("create guest memory"),
    );
    let dev = VirtioPciDevice::new(
        TestVirtioDevice { features: 0 },
        bits::VIRTIO_DEV_TYPE_BLOCK,
        1,
        256,
        8,
        None,
        physmap,
        Arc::new(PioBus::new()),
        Arc::new(MmioBus::new()),
        Some(Arc::clone(&msix)),
    );
    let wo = WriteOp::from_buf(&0u32.to_le_bytes());
    dev.bar_rw(
        BarN::BAR2,
        bits::COMMON_CFG_MSIX_CONFIG.into(),
        RWOp::Write(&wo),
    );
    (dev, sink, msix)
}

/// DEVICE_NEEDS_RESET must reach the driver as a config change does. A
/// modern guest with MSI-X never reads the ISR, so an INTx-only signal
/// tells it nothing and leaves the shared pin high for the life of the
/// VM. The device raises it once, because the guest can repeat the
/// refused write.
#[test]
fn a_refused_queue_raises_one_msix_message_however_often_it_repeats() {
    let (dev, sink, _msix) = device_with_msix();

    for _ in 0..16 {
        // Past the mapped 16 KiB, so the rings cannot be reached.
        enable_queue_0(
            &dev,
            RING_GPA + 0x3000,
            RING_GPA + 0x3800,
            RING_GPA + 0x3C00,
        );
    }

    assert_ne!(device_status(&dev) & bits::STATUS_DEVICE_NEEDS_RESET, 0);
    assert_eq!(
        sink.0.lock().expect("recording lock").len(),
        1,
        "the needs-reset signal must reach an MSI-X driver exactly once",
    );
}
