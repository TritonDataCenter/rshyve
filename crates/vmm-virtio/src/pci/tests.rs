// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests for the virtio PCI transport.

use super::*;

// The tests drive the transport through the same trait and ops that a
// PCI bus uses.
use vmm_core::common::RWOp;
use vmm_devices::pci::device::PciDevice;

/// Minimal test VirtIO device for transport tests.
struct TestVirtioDevice {
    features: u64,
}

impl VirtioDevice for TestVirtioDevice {
    fn device_features(&self) -> u64 {
        self.features
    }

    fn set_features(&self, _features: u64) {}

    fn cfg_read(&self, offset: u16, _len: u8) -> u32 {
        // Return the offset, so a test can see which offset was read.
        u32::from(offset)
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

    fn reset(&self) {}
}

impl Lifecycle for TestVirtioDevice {
    fn type_name(&self) -> &'static str {
        "test-virtio"
    }
}

fn make_test_device() -> Arc<VirtioPciDevice<TestVirtioDevice>> {
    make_device_of_type(bits::VIRTIO_DEV_TYPE_BLOCK)
}

fn make_device_of_type(
    dev_type: u16,
) -> Arc<VirtioPciDevice<TestVirtioDevice>> {
    let physmap = Arc::new(PhysMap::new());
    let bus_pio = Arc::new(PioBus::new());
    let bus_mmio = Arc::new(MmioBus::new());
    VirtioPciDevice::new(
        TestVirtioDevice {
            features: 0x42
                | bits::VIRTIO_F_RING_EVENT_IDX
                | bits::VIRTIO_F_RING_INDIRECT_DESC,
        },
        dev_type,
        1,
        256,
        8, // 8 bytes of device config
        None,
        physmap,
        bus_pio,
        bus_mmio,
        None, // no MSI-X
    )
}

/// Build a device that has an MSI-X table, the way every real device
/// does. `MsiSink` exists so the table needs no live vmm handle.
fn make_device_with_msix(
) -> (Arc<VirtioPciDevice<TestVirtioDevice>>, Arc<MsixTable>) {
    struct NullSink;
    impl vmm_devices::pci::msix::MsiSink for NullSink {
        fn send(&self, _addr: u64, _data: u64) {}
    }
    let msix = Arc::new(MsixTable::new(2, Arc::new(NullSink)));
    let physmap = Arc::new(PhysMap::new());
    let bus_pio = Arc::new(PioBus::new());
    let bus_mmio = Arc::new(MmioBus::new());
    let dev = VirtioPciDevice::new(
        TestVirtioDevice { features: 0x42 },
        bits::VIRTIO_DEV_TYPE_BLOCK,
        1,
        256,
        8,
        None,
        physmap,
        bus_pio,
        bus_mmio,
        Some(Arc::clone(&msix)),
    );
    (dev, msix)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RestoreRingCall {
    queue_idx: u16,
    size: u16,
    desc: u64,
    avail: u64,
    used: u64,
    avail_idx: u16,
    used_idx: u16,
    msix_addr: u64,
    msix_data: u32,
}

#[derive(Debug, Default)]
struct TrackingVirtioDeviceState {
    set_features: Vec<u64>,
    notify_addrs: Vec<(u16, u64)>,
    restored_rings: Vec<RestoreRingCall>,
    reset_all_rings: usize,
    /// The queue index of every kick the transport let through.
    notified: Vec<u16>,
}

/// The step of a restore whose backend refuses it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RefusedStep {
    ResetRings,
    RingState,
}

struct TrackingVirtioDevice {
    features: u64,
    state: Mutex<TrackingVirtioDeviceState>,
    /// The restore step this backend refuses, if any.
    refuses: Option<RefusedStep>,
}

impl VirtioDevice for TrackingVirtioDevice {
    fn device_features(&self) -> u64 {
        self.features
    }

    fn set_features(&self, features: u64) {
        self.state
            .lock()
            .expect("tracking state lock")
            .set_features
            .push(features);
    }

    fn cfg_read(&self, offset: u16, _len: u8) -> u32 {
        u32::from(offset)
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

    fn notify_queue(
        &self,
        queue_idx: u16,
        _queues: &mut [VirtQueue],
        _physmap: &PhysMap,
    ) -> bool {
        self.state
            .lock()
            .expect("tracking state lock")
            .notified
            .push(queue_idx);
        false
    }

    fn reset_all_rings(
        &self,
    ) -> Result<(), vmm_devices::lifecycle::DeviceStateError> {
        self.state
            .lock()
            .expect("tracking state lock")
            .reset_all_rings += 1;
        self.refusal(RefusedStep::ResetRings)
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
        self.state
            .lock()
            .expect("tracking state lock")
            .restored_rings
            .push(RestoreRingCall {
                queue_idx,
                size,
                desc,
                avail,
                used,
                avail_idx,
                used_idx,
                msix_addr,
                msix_data,
            });
        self.refusal(RefusedStep::RingState)
    }

    fn set_notify_addrs(&self, pio_port: u16, mmio_addr: u64) {
        self.state
            .lock()
            .expect("tracking state lock")
            .notify_addrs
            .push((pio_port, mmio_addr));
    }

    fn reset(&self) {}
}

impl TrackingVirtioDevice {
    /// Fail `step` if this backend refuses it.
    fn refusal(
        &self,
        step: RefusedStep,
    ) -> Result<(), vmm_devices::lifecycle::DeviceStateError> {
        if self.refuses == Some(step) {
            return Err(vmm_devices::lifecycle::DeviceStateError::Invalid(
                format!("the backend refused {step:?}"),
            ));
        }
        Ok(())
    }
}

impl Lifecycle for TrackingVirtioDevice {
    fn type_name(&self) -> &'static str {
        "tracking-virtio"
    }
}

/// Guest memory the migration tests program their rings into. The
/// restore refuses a ring that is not mapped, so the addresses have to
/// be real.
pub(super) const MIGRATE_RING_GPA: u64 = 0x1000;
pub(super) const MIGRATE_RING_LEN: usize = 0x8000;

fn make_tracking_device() -> Arc<VirtioPciDevice<TrackingVirtioDevice>> {
    make_tracking_device_with_msix(None)
}

/// A device whose backend refuses one step of the restore.
fn make_refusing_tracking_device(
    step: RefusedStep,
) -> Arc<VirtioPciDevice<TrackingVirtioDevice>> {
    let physmap = Arc::new(
        PhysMap::new_anon(MIGRATE_RING_GPA, MIGRATE_RING_LEN)
            .expect("create guest memory"),
    );
    build_tracking_device(physmap, None, Some(step))
}

fn make_tracking_device_on(
    physmap: Arc<PhysMap>,
) -> Arc<VirtioPciDevice<TrackingVirtioDevice>> {
    make_tracking_device_full(physmap, None)
}

/// Restore refuses an unmapped ring, so this maps guest memory.
fn make_tracking_device_with_msix(
    msix: Option<Arc<MsixTable>>,
) -> Arc<VirtioPciDevice<TrackingVirtioDevice>> {
    let physmap = Arc::new(
        PhysMap::new_anon(MIGRATE_RING_GPA, MIGRATE_RING_LEN)
            .expect("create guest memory"),
    );
    make_tracking_device_full(physmap, msix)
}

fn make_tracking_device_full(
    physmap: Arc<PhysMap>,
    msix: Option<Arc<MsixTable>>,
) -> Arc<VirtioPciDevice<TrackingVirtioDevice>> {
    build_tracking_device(physmap, msix, None)
}

fn build_tracking_device(
    physmap: Arc<PhysMap>,
    msix: Option<Arc<MsixTable>>,
    refuses: Option<RefusedStep>,
) -> Arc<VirtioPciDevice<TrackingVirtioDevice>> {
    let bus_pio = Arc::new(PioBus::new());
    let bus_mmio = Arc::new(MmioBus::new());
    VirtioPciDevice::new(
        TrackingVirtioDevice {
            features: bits::VIRTIO_F_VERSION_1,
            state: Mutex::new(TrackingVirtioDeviceState::default()),
            refuses,
        },
        bits::VIRTIO_DEV_TYPE_BLOCK,
        1,
        256,
        8,
        None,
        physmap,
        bus_pio,
        bus_mmio,
        msix,
    )
}

/// A device that owns its ring cursors, as viona does.
///
/// `None` is a read that the kernel refused. Userspace has no copy of
/// cursors that a kernel backend moves, so the export must report the
/// error to its caller.
struct KernelCursorDevice(Option<(u16, u16)>);

impl VirtioDevice for KernelCursorDevice {
    fn device_features(&self) -> u64 {
        bits::VIRTIO_F_VERSION_1
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

    fn kernel_ring_indices(
        &self,
        queue_idx: u16,
    ) -> Result<Option<(u16, u16)>, vmm_devices::lifecycle::DeviceStateError>
    {
        self.0.map(Some).ok_or_else(|| {
            vmm_devices::lifecycle::DeviceStateError::Export(format!(
                "ring {queue_idx} state read failed"
            ))
        })
    }

    fn reset(&self) {}
}

impl Lifecycle for KernelCursorDevice {
    fn type_name(&self) -> &'static str {
        "kernel-cursor-virtio"
    }
}

fn make_kernel_cursor_device(
    indices: Option<(u16, u16)>,
) -> Arc<VirtioPciDevice<KernelCursorDevice>> {
    let physmap = Arc::new(
        PhysMap::new_anon(MIGRATE_RING_GPA, MIGRATE_RING_LEN)
            .expect("create guest memory"),
    );
    VirtioPciDevice::new(
        KernelCursorDevice(indices),
        bits::VIRTIO_DEV_TYPE_NET,
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

/// An MSI-X table that needs no live vmm handle.
pub(super) fn null_msix(count: u16) -> Arc<MsixTable> {
    struct NullSink;
    impl vmm_devices::pci::msix::MsiSink for NullSink {
        fn send(&self, _addr: u64, _data: u64) {}
    }
    Arc::new(MsixTable::new(count, Arc::new(NullSink)))
}

#[test]
fn pci_identity() {
    let dev = make_test_device();
    let id = dev.cfg_read(0x00, 4);
    assert_eq!(id & 0xFFFF, bits::VIRTIO_PCI_VENDOR_ID as u32);
    // Transitional device id assigned to virtio-blk.
    assert_eq!((id >> 16) & 0xFFFF, 0x1001);
}

#[test]
fn pci_identity_net() {
    let dev = make_device_of_type(bits::VIRTIO_DEV_TYPE_NET);
    let id = dev.cfg_read(0x00, 4);
    assert_eq!((id >> 16) & 0xFFFF, 0x1000);
}

#[test]
fn pci_identity_rng() {
    let dev = make_device_of_type(bits::VIRTIO_DEV_TYPE_RNG);
    let id = dev.cfg_read(0x00, 4);
    // 0x1003 is the virtio-console ID, not the entropy device ID.
    assert_eq!((id >> 16) & 0xFFFF, 0x1005);
}

#[test]
fn pci_identity_console() {
    let dev = make_device_of_type(bits::VIRTIO_DEV_TYPE_CONSOLE);
    let id = dev.cfg_read(0x00, 4);
    assert_eq!((id >> 16) & 0xFFFF, 0x1003);
    let class_reg = dev.cfg_read(0x08, 4);
    assert_eq!(
        (class_reg >> 24) & 0xFF,
        vmm_devices::pci::bits::CLASS_COMMUNICATION as u32
    );
    assert_eq!(
        (class_reg >> 16) & 0xFF,
        vmm_devices::pci::bits::SUBCLASS_COMMUNICATION_OTHER as u32
    );
}

#[test]
fn pci_class() {
    let dev = make_test_device();
    let class_reg = dev.cfg_read(0x08, 4);
    let class = (class_reg >> 24) & 0xFF;
    assert_eq!(class, vmm_devices::pci::bits::CLASS_STORAGE as u32);
}

/// Build a device with no device-specific config, as virtio-rng has.
fn make_device_without_config() -> Arc<VirtioPciDevice<TestVirtioDevice>> {
    let physmap = Arc::new(PhysMap::new());
    VirtioPciDevice::new(
        TestVirtioDevice { features: 0 },
        bits::VIRTIO_DEV_TYPE_RNG,
        1,
        64,
        0, // no device config, like virtio-rng
        None,
        physmap,
        Arc::new(PioBus::new()),
        Arc::new(MmioBus::new()),
        None,
    )
}

/// Walk the capability chain, returning (cfg_type, length) per virtio
/// capability in link order.
fn walk_virtio_caps<D: VirtioDevice>(
    dev: &VirtioPciDevice<D>,
) -> Vec<(u8, u32)> {
    let mut out = Vec::new();
    // Capabilities pointer at 0x34, low byte.
    let mut pos = (dev.cfg_read(0x34, 4) & 0xFF) as u8;
    // Bounded: a malformed chain must not spin the test forever.
    for _ in 0..16 {
        if pos == 0 {
            break;
        }
        let hdr = dev.cfg_read(pos, 4);
        let cap_id = (hdr & 0xFF) as u8;
        let next = ((hdr >> 8) & 0xFF) as u8;
        if cap_id == bits::PCI_CAP_ID_VNDR {
            let cfg_type = ((hdr >> 24) & 0xFF) as u8;
            let length = dev.cfg_read(pos + 12, 4);
            out.push((cfg_type, length));
        }
        pos = next;
    }
    out
}

/// A capability advertising length 0 is not ignored by Linux: its
/// `map_capability()` rejects `length <= start` and fails the whole
/// probe, so the device never binds. VIRTIO 1.3 4.1.4.6 makes the
/// device-config capability conditional on having such config.
#[test]
fn device_without_config_omits_the_device_cfg_cap() {
    let dev = make_device_without_config();
    let caps = walk_virtio_caps(&dev);

    assert!(
        !caps
            .iter()
            .any(|(t, _)| *t == bits::VIRTIO_PCI_CAP_DEVICE_CFG),
        "device with no config still advertises a device-cfg cap: {caps:?}"
    );
    // Whatever is advertised must be usable.
    for (cfg_type, length) in &caps {
        assert_ne!(*length, 0, "cap type {cfg_type} has length 0");
    }
    // The chain must still reach the caps a driver needs.
    for want in [
        bits::VIRTIO_PCI_CAP_COMMON_CFG,
        bits::VIRTIO_PCI_CAP_NOTIFY_CFG,
        bits::VIRTIO_PCI_CAP_ISR_CFG,
    ] {
        assert!(
            caps.iter().any(|(t, _)| *t == want),
            "cap type {want} missing from {caps:?}"
        );
    }
}

#[test]
fn device_with_config_keeps_the_device_cfg_cap() {
    let dev = make_test_device();
    let caps = walk_virtio_caps(&dev);
    let found = caps
        .iter()
        .find(|(t, _)| *t == bits::VIRTIO_PCI_CAP_DEVICE_CFG)
        .expect("device-cfg cap present");
    assert_eq!(
        found.1, 8,
        "device-cfg cap length should be the config size"
    );
}

#[test]
fn pci_subsystem() {
    let dev = make_test_device();
    let sub = dev.cfg_read(0x2C, 4);
    assert_eq!(sub & 0xFFFF, bits::VIRTIO_PCI_VENDOR_ID as u32);
    assert_eq!((sub >> 16) & 0xFFFF, bits::VIRTIO_DEV_TYPE_BLOCK as u32);
}

#[test]
fn bar0_is_pio() {
    let dev = make_test_device();
    // Write all-ones to BAR0 to probe
    dev.cfg_write(0x10, 4, 0xFFFF_FFFF);
    let bar = dev.cfg_read(0x10, 4);
    // Low bit should be 1 (PIO)
    assert_eq!(bar & 1, 1);
}

#[test]
fn device_features_read() {
    use vmm_core::common::ReadOp;
    let dev = make_test_device();
    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR0, 0x00, RWOp::Read(&mut ro));
    let val = u32::from_le_bytes([
        ro.buf()[0],
        ro.buf()[1],
        ro.buf()[2],
        ro.buf()[3],
    ]);
    // The legacy transport strips EVENT_IDX and INDIRECT_DESC from the
    // offer, which leaves 0x42.
    assert_eq!(val, 0x42);
}

#[test]
fn device_status_readwrite() {
    use vmm_core::common::{ReadOp, WriteOp};
    let dev = make_test_device();

    // 0x12 is the legacy device status register.
    let wo = WriteOp::from_buf(&[bits::STATUS_ACKNOWLEDGE]);
    dev.bar_rw(BarN::BAR0, 0x12, RWOp::Write(&wo));

    let mut ro = ReadOp::new(1);
    dev.bar_rw(BarN::BAR0, 0x12, RWOp::Read(&mut ro));
    assert_eq!(ro.buf()[0], bits::STATUS_ACKNOWLEDGE);
}

#[test]
fn device_status_zero_resets() {
    use vmm_core::common::{ReadOp, WriteOp};
    let dev = make_test_device();

    let wo =
        WriteOp::from_buf(&[bits::STATUS_ACKNOWLEDGE | bits::STATUS_DRIVER]);
    dev.bar_rw(BarN::BAR0, 0x12, RWOp::Write(&wo));

    // Write 0 to reset. The reset runs on this thread, so the write
    // returns with the device reset and no poll is necessary.
    let wo = WriteOp::from_buf(&[0]);
    dev.bar_rw(BarN::BAR0, 0x12, RWOp::Write(&wo));

    let mut ro = ReadOp::new(1);
    dev.bar_rw(BarN::BAR0, 0x12, RWOp::Read(&mut ro));
    assert_eq!(ro.buf()[0], 0);
}

#[test]
fn isr_read_clears() {
    use vmm_core::common::ReadOp;
    let dev = make_test_device();

    dev.isr_status
        .store(bits::ISR_QUEUE_INTR, std::sync::atomic::Ordering::Relaxed);

    // 0x13 is the legacy ISR register.
    let mut ro = ReadOp::new(1);
    dev.bar_rw(BarN::BAR0, 0x13, RWOp::Read(&mut ro));
    assert_eq!(ro.buf()[0], bits::ISR_QUEUE_INTR);

    // The first read cleared it.
    let mut ro = ReadOp::new(1);
    dev.bar_rw(BarN::BAR0, 0x13, RWOp::Read(&mut ro));
    assert_eq!(ro.buf()[0], 0);
}

#[test]
fn queue_select_and_size() {
    use vmm_core::common::{ReadOp, WriteOp};
    let dev = make_test_device();

    // Select queue 0 (0x0E), then read its size (0x0C).
    let wo = WriteOp::from_buf(&[0, 0]);
    dev.bar_rw(BarN::BAR0, 0x0E, RWOp::Write(&wo));

    let mut ro = ReadOp::new(2);
    dev.bar_rw(BarN::BAR0, 0x0C, RWOp::Read(&mut ro));
    let size = u16::from_le_bytes([ro.buf()[0], ro.buf()[1]]);
    assert_eq!(size, 256);

    // Select a queue that does not exist.
    let wo = WriteOp::from_buf(&[1, 0]);
    dev.bar_rw(BarN::BAR0, 0x0E, RWOp::Write(&wo));

    let mut ro = ReadOp::new(2);
    dev.bar_rw(BarN::BAR0, 0x0C, RWOp::Read(&mut ro));
    let size = u16::from_le_bytes([ro.buf()[0], ro.buf()[1]]);
    assert_eq!(size, 0);
}

#[test]
fn device_config_read() {
    use vmm_core::common::ReadOp;
    let dev = make_test_device();

    // Device-specific config starts at 0x14 without MSI-X.
    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR0, 0x14, RWOp::Read(&mut ro));
    let val = u32::from_le_bytes([
        ro.buf()[0],
        ro.buf()[1],
        ro.buf()[2],
        ro.buf()[3],
    ]);
    assert_eq!(val, 0);

    // 0x18 is device config offset 4.
    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR0, 0x18, RWOp::Read(&mut ro));
    let val = u32::from_le_bytes([
        ro.buf()[0],
        ro.buf()[1],
        ro.buf()[2],
        ro.buf()[3],
    ]);
    assert_eq!(val, 4);
}

#[test]
fn guest_features_write_read() {
    use vmm_core::common::{ReadOp, WriteOp};
    let dev = make_test_device();

    // The device offers 0x42.
    let wo = WriteOp::from_buf(&[0x42, 0x00, 0x00, 0x00]);
    dev.bar_rw(BarN::BAR0, 0x04, RWOp::Write(&wo));

    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR0, 0x04, RWOp::Read(&mut ro));
    let val = u32::from_le_bytes([
        ro.buf()[0],
        ro.buf()[1],
        ro.buf()[2],
        ro.buf()[3],
    ]);
    assert_eq!(val, 0x42);
}

#[test]
fn guest_features_masked_to_offered() {
    use vmm_core::common::{ReadOp, WriteOp};
    let dev = make_test_device();

    let wo = WriteOp::from_buf(&[0xFF, 0xFF, 0xFF, 0xFF]);
    dev.bar_rw(BarN::BAR0, 0x04, RWOp::Write(&wo));

    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR0, 0x04, RWOp::Read(&mut ro));
    let val = u32::from_le_bytes([
        ro.buf()[0],
        ro.buf()[1],
        ro.buf()[2],
        ro.buf()[3],
    ]);
    assert_eq!(val, 0x42, "guest features should be masked to offered 0x42");
}

#[test]
fn guest_features_write_unsupported_is_rejected() {
    use vmm_core::common::{ReadOp, WriteOp};
    let dev = make_test_device();

    // 0x100 is not offered.
    let wo = WriteOp::from_buf(&[0x00, 0x01, 0x00, 0x00]);
    dev.bar_rw(BarN::BAR0, 0x04, RWOp::Write(&wo));

    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR0, 0x04, RWOp::Read(&mut ro));
    let val = u32::from_le_bytes([
        ro.buf()[0],
        ro.buf()[1],
        ro.buf()[2],
        ro.buf()[3],
    ]);
    assert_eq!(val, 0x00);
}

#[test]
fn device_config_offset_without_msix() {
    let dev = make_test_device();
    assert_eq!(dev.device_config_offset(), bits::LEGACY_REG_DEVICE_CONFIG);
    assert_eq!(dev.device_config_offset(), 0x14);
}

#[test]
fn device_config_offset_msix_constant() {
    assert_eq!(bits::LEGACY_REG_DEVICE_CONFIG, 0x14);
    assert_eq!(bits::LEGACY_REG_DEVICE_CONFIG_MSIX, 0x18);
    // Two 16-bit vector registers.
    assert_eq!(
        bits::LEGACY_REG_DEVICE_CONFIG_MSIX - bits::LEGACY_REG_DEVICE_CONFIG,
        4,
    );
}

/// The legacy layout follows the MSI-X enable bit, not the presence of
/// the table. Every device has a table, so a driver that leaves MSI-X
/// off must find device config at 0x14. Otherwise it reads two 0xFFFF
/// vector registers in place of its data.
#[test]
fn a_driver_that_leaves_msix_off_reads_device_config_at_0x14() {
    let (dev, msix) = make_device_with_msix();
    assert!(!msix.is_enabled(), "MSI-X starts disabled");
    assert_eq!(dev.device_config_offset(), bits::LEGACY_REG_DEVICE_CONFIG);
    assert_eq!(dev.device_config_offset(), 0x14);
}

#[test]
fn a_driver_that_enables_msix_reads_device_config_at_0x18() {
    let (dev, msix) = make_device_with_msix();
    msix.set_enabled(true);
    assert_eq!(
        dev.device_config_offset(),
        bits::LEGACY_REG_DEVICE_CONFIG_MSIX
    );
    assert_eq!(dev.device_config_offset(), 0x18);
}

/// The vector registers exist only while the driver has MSI-X enabled.
/// While it is off, 0x14 is the first byte of device config.
#[test]
fn the_vector_registers_appear_only_once_msix_is_enabled() {
    let (dev, msix) = make_device_with_msix();

    // TestVirtioDevice::cfg_read returns the offset, so config byte 0
    // reads 0. A vector register at NO_VECTOR reads 0xFFFF.
    assert_eq!(
        dev.bar_read(0x14, 2),
        0,
        "0x14 gave the driver a vector register while MSI-X was off"
    );

    msix.set_enabled(true);
    assert_eq!(
        dev.bar_read(0x14, 2),
        0xFFFF,
        "with MSI-X on, 0x14 is the config vector register at NO_VECTOR"
    );
}

#[test]
fn queue_select_invalid_returns_zero_size() {
    use vmm_core::common::{ReadOp, WriteOp};
    let dev = make_test_device();

    // The device has one queue. Select queue 3.
    let wo = WriteOp::from_buf(&[3, 0]);
    dev.bar_rw(BarN::BAR0, 0x0E, RWOp::Write(&wo));

    let mut ro = ReadOp::new(2);
    dev.bar_rw(BarN::BAR0, 0x0C, RWOp::Read(&mut ro));
    let size = u16::from_le_bytes([ro.buf()[0], ro.buf()[1]]);
    assert_eq!(size, 0, "invalid queue index should report size 0");

    // 0x08 is the queue PFN register.
    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR0, 0x08, RWOp::Read(&mut ro));
    let pfn = u32::from_le_bytes([
        ro.buf()[0],
        ro.buf()[1],
        ro.buf()[2],
        ro.buf()[3],
    ]);
    assert_eq!(pfn, 0, "invalid queue index should report PFN 0");
}

#[test]
fn features_ok_set_and_readable() {
    use vmm_core::common::{ReadOp, WriteOp};
    let dev = make_test_device();

    // ACK, then DRIVER, then FEATURES_OK.
    let wo = WriteOp::from_buf(&[bits::STATUS_ACKNOWLEDGE]);
    dev.bar_rw(BarN::BAR0, 0x12, RWOp::Write(&wo));

    let wo =
        WriteOp::from_buf(&[bits::STATUS_ACKNOWLEDGE | bits::STATUS_DRIVER]);
    dev.bar_rw(BarN::BAR0, 0x12, RWOp::Write(&wo));

    let wo = WriteOp::from_buf(&[bits::STATUS_ACKNOWLEDGE
        | bits::STATUS_DRIVER
        | bits::STATUS_FEATURES_OK]);
    dev.bar_rw(BarN::BAR0, 0x12, RWOp::Write(&wo));

    let mut ro = ReadOp::new(1);
    dev.bar_rw(BarN::BAR0, 0x12, RWOp::Read(&mut ro));
    assert_ne!(
        ro.buf()[0] & bits::STATUS_FEATURES_OK,
        0,
        "FEATURES_OK should be set after valid feature negotiation"
    );
}

fn notified(dev: &VirtioPciDevice<TrackingVirtioDevice>) -> Vec<u16> {
    dev.device
        .state
        .lock()
        .expect("tracking state lock")
        .notified
        .clone()
}

#[test]
fn queue_notify_ignored_before_driver_ok() {
    use vmm_core::common::WriteOp;
    let dev = make_tracking_device();

    let wo = WriteOp::from_buf(&[0, 0]);
    dev.bar_rw(BarN::BAR0, 0x10, RWOp::Write(&wo));
    assert!(notified(&dev).is_empty(), "a kick reached the backend");
}

/// A kick on a queue with no ring must not reach the backend. The
/// backend builds its used-ring writer on the first kick and keeps it
/// for the session. A writer built on address zero never writes the
/// ring that the driver programs next.
#[test]
fn a_kick_before_the_ring_is_programmed_never_reaches_the_backend() {
    use vmm_core::common::WriteOp;
    const RAM_GPA: u64 = 0x4000;
    let physmap = Arc::new(
        PhysMap::new_anon(RAM_GPA, 0x4000).expect("create guest memory"),
    );
    let dev = make_tracking_device_on(physmap);
    let write = |reg: u16, val: &[u8]| {
        let wo = WriteOp::from_buf(val);
        dev.bar_rw(BarN::BAR0, usize::from(reg), RWOp::Write(&wo));
    };
    write(
        bits::LEGACY_REG_DEVICE_STATUS,
        &[bits::STATUS_ACKNOWLEDGE
            | bits::STATUS_DRIVER
            | bits::STATUS_DRIVER_OK],
    );

    write(bits::LEGACY_REG_QUEUE_NOTIFY, &[0, 0]);
    assert!(
        notified(&dev).is_empty(),
        "a kick reached an unprogrammed queue"
    );

    let pfn = (RAM_GPA / vmm_core::common::PAGE_SIZE as u64) as u32;
    write(bits::LEGACY_REG_QUEUE_PFN, &pfn.to_le_bytes());
    write(bits::LEGACY_REG_QUEUE_NOTIFY, &[0, 0]);
    assert_eq!(
        notified(&dev),
        vec![0],
        "the kick after programming is lost"
    );

    write(bits::LEGACY_REG_QUEUE_PFN, &[0, 0, 0, 0]);
    write(bits::LEGACY_REG_QUEUE_NOTIFY, &[0, 0]);
    assert_eq!(notified(&dev), vec![0], "a kick reached a retired queue");
}

#[test]
fn isr_cfg_change_bit() {
    use vmm_core::common::ReadOp;
    let dev = make_test_device();

    dev.isr_status
        .store(bits::ISR_CFG_CHANGE, std::sync::atomic::Ordering::Relaxed);

    let mut ro = ReadOp::new(1);
    dev.bar_rw(BarN::BAR0, 0x13, RWOp::Read(&mut ro));
    assert_eq!(ro.buf()[0], bits::ISR_CFG_CHANGE);

    let mut ro = ReadOp::new(1);
    dev.bar_rw(BarN::BAR0, 0x13, RWOp::Read(&mut ro));
    assert_eq!(ro.buf()[0], 0);
}

#[test]
fn isr_combined_bits() {
    use vmm_core::common::ReadOp;
    let dev = make_test_device();

    dev.isr_status.store(
        bits::ISR_QUEUE_INTR | bits::ISR_CFG_CHANGE,
        std::sync::atomic::Ordering::Relaxed,
    );

    let mut ro = ReadOp::new(1);
    dev.bar_rw(BarN::BAR0, 0x13, RWOp::Read(&mut ro));
    assert_eq!(ro.buf()[0], bits::ISR_QUEUE_INTR | bits::ISR_CFG_CHANGE,);
}

#[test]
fn device_features_write_ignored() {
    use vmm_core::common::{ReadOp, WriteOp};
    let dev = make_test_device();

    // The device features register (0x00) is read-only.
    let wo = WriteOp::from_buf(&[0xFF, 0xFF, 0xFF, 0xFF]);
    dev.bar_rw(BarN::BAR0, 0x00, RWOp::Write(&wo));

    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR0, 0x00, RWOp::Read(&mut ro));
    let val = u32::from_le_bytes([
        ro.buf()[0],
        ro.buf()[1],
        ro.buf()[2],
        ro.buf()[3],
    ]);
    assert_eq!(val, 0x42, "device features should be read-only");
}

mod modern;
mod wired;
