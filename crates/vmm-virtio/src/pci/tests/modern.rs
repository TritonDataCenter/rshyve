// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests for the modern (VirtIO 1.0) half of the transport: the
//! capability chain, the common config registers and the feature bits.

use super::*;
use vmm_devices::DeviceMigrateState;

#[test]
fn bar2_is_mmio() {
    let dev = make_test_device();
    // Write all-ones to probe the BAR size.
    dev.cfg_write(0x18, 4, 0xFFFF_FFFF);
    let bar = dev.cfg_read(0x18, 4);
    // Low bit 0 = MMIO (not PIO)
    assert_eq!(bar & 1, 0, "BAR2 should be MMIO");
    // Size mask for 16KB: ~(0x4000 - 1) = 0xFFFF_C000
    assert_eq!(bar & 0xFFFF_C000, 0xFFFF_C000, "BAR2 size should be 16KB");
}

/// Ring addresses for the migration tests, inside the anonymous region
/// that the test devices map.
const DESC: u64 = MIGRATE_RING_GPA;
const AVAIL: u64 = MIGRATE_RING_GPA + 0x2000;
const USED: u64 = MIGRATE_RING_GPA + 0x4000;

fn exported(
    dev: &VirtioPciDevice<impl VirtioDevice + Lifecycle>,
) -> vmm_devices::lifecycle::VirtioMigrateState {
    match dev.export_migrate_state().expect("export cannot fail") {
        Some(DeviceMigrateState::Virtio(state)) => state,
        other => panic!("a virtio transport exports virtio state: {other:?}"),
    }
}

fn live_queue() -> vmm_devices::lifecycle::VirtioMigrateQueue {
    vmm_devices::lifecycle::VirtioMigrateQueue {
        queue_idx: 0,
        queue_size: 256,
        desc_addr: DESC,
        avail_addr: AVAIL,
        used_addr: USED,
        live: true,
        avail_idx: 7,
        used_idx: 5,
        msix_vector: bits::VIRTIO_MSI_NO_VECTOR,
    }
}

fn transport(
    features: u64,
    queues: Vec<vmm_devices::lifecycle::VirtioMigrateQueue>,
) -> vmm_devices::lifecycle::VirtioMigrateState {
    vmm_devices::lifecycle::VirtioMigrateState {
        status: bits::STATUS_DRIVER_OK
            | bits::STATUS_FEATURES_OK
            | bits::STATUS_DRIVER
            | bits::STATUS_ACKNOWLEDGE,
        features,
        config_msix_vector: bits::VIRTIO_MSI_NO_VECTOR,
        pci: Default::default(),
        msix: None,
        queues,
    }
}

#[test]
fn export_virtio_state_includes_pci_transport_state() {
    let dev = make_test_device();
    {
        let mut vs = dev.virtio_state.lock().expect("virtio lock");
        vs.queues[0].set_addr_modern(0x1000, 0x2000, 0x3000);
        vs.queue_enabled[0] = true;
    }

    let command = vmm_devices::pci::bits::RegCmd::IO_EN
        | vmm_devices::pci::bits::RegCmd::MMIO_EN
        | vmm_devices::pci::bits::RegCmd::BUSMSTR_EN
        | vmm_devices::pci::bits::RegCmd::INTX_DIS;
    dev.cfg_write(vmm_devices::pci::bits::REG_BAR0, 4, 0xC120);
    dev.cfg_write(vmm_devices::pci::bits::REG_BAR2, 4, 0xFEC0_0000);
    dev.cfg_write(
        vmm_devices::pci::bits::REG_COMMAND,
        2,
        u32::from(command.bits()),
    );

    let state = exported(&dev);
    assert_eq!(state.queues.len(), 1);
    assert_eq!(state.pci.command, command.bits());
    assert_eq!(state.pci.bar_addrs[BarN::BAR0 as usize], 0xC120);
    assert_eq!(state.pci.bar_addrs[BarN::BAR2 as usize], 0xFEC0_0000);
    assert_eq!(state.pci.bar_addrs[BarN::BAR4 as usize], 0);
}

/// Program one queue with userspace cursors that the kernel never
/// moved, so an export that uses them is visible.
fn kernel_cursor_transport(
    indices: Option<(u16, u16)>,
) -> Arc<VirtioPciDevice<KernelCursorDevice>> {
    let dev = make_kernel_cursor_device(indices);
    let mut vs = dev.virtio_state.lock().expect("virtio lock");
    vs.queues[0].set_addr_modern(DESC, AVAIL, USED);
    vs.queues[0].set_last_avail_idx(9);
    vs.queues[0].set_shadow_used_idx(3);
    vs.queue_enabled[0] = true;
    drop(vs);
    dev
}

#[test]
fn an_export_takes_a_kernel_used_cursor_of_zero() {
    // Both cursors are u16 values that wrap through zero, so zero is a
    // valid used cursor. An export that reads zero as "no answer" uses
    // the stale userspace cursors. The destination then resumes from an
    // old point and replays descriptors.
    let dev = kernel_cursor_transport(Some((0x1234, 0)));

    let state = exported(&dev);

    assert_eq!(state.queues.len(), 1);
    assert_eq!(
        (state.queues[0].avail_idx, state.queues[0].used_idx),
        (0x1234, 0),
    );
}

#[test]
fn an_export_fails_when_the_kernel_cursors_cannot_be_read() {
    // Only the kernel has the cursors. A payload without them makes the
    // destination ring resume from the wrong point, so the migration
    // must fail.
    let dev = kernel_cursor_transport(None);

    dev.export_migrate_state()
        .expect_err("a refused cursor read must fail the export");
}

#[test]
fn a_restore_fails_when_the_backend_cannot_stop_its_rings() {
    // A worker left running on the destination reads a ring whose
    // addresses the restore overwrites.
    let dev = make_refusing_tracking_device(RefusedStep::ResetRings);

    dev.restore_migrate_state(&DeviceMigrateState::Virtio(transport(
        bits::VIRTIO_F_VERSION_1,
        vec![live_queue()],
    )))
    .expect_err("a backend that cannot stop its rings must fail the restore");
}

#[test]
fn a_restore_fails_when_the_backend_refuses_a_ring_state() {
    // The source commits on this answer and then discards its VM. A
    // false success leaves the guest a dead NIC and no source to return
    // to.
    let dev = make_refusing_tracking_device(RefusedStep::RingState);

    dev.restore_migrate_state(&DeviceMigrateState::Virtio(transport(
        bits::VIRTIO_F_VERSION_1,
        vec![live_queue()],
    )))
    .expect_err("a refused ring state must fail the restore");
}

#[test]
fn restore_virtio_state_replays_pci_transport_state() {
    let dev = make_tracking_device();
    let command = vmm_devices::pci::bits::RegCmd::IO_EN
        | vmm_devices::pci::bits::RegCmd::MMIO_EN
        | vmm_devices::pci::bits::RegCmd::BUSMSTR_EN
        | vmm_devices::pci::bits::RegCmd::INTX_DIS;
    let mut state = transport(bits::VIRTIO_F_VERSION_1, vec![live_queue()]);
    state.pci = vmm_devices::lifecycle::MigratePciState {
        command: command.bits(),
        bar_addrs: [0xC120, 0, 0xFEC0_0000, 0, 0, 0],
    };

    dev.restore_migrate_state(&DeviceMigrateState::Virtio(state))
        .expect("a sane payload restores");

    assert_eq!(dev.cfg_read(vmm_devices::pci::bits::REG_BAR0, 4), 0xC121);
    assert_eq!(
        dev.cfg_read(vmm_devices::pci::bits::REG_BAR2, 4),
        0xFEC0_0000
    );
    assert_eq!(
        dev.cfg_read(vmm_devices::pci::bits::REG_COMMAND, 2),
        u32::from(command.bits()),
    );

    let tracked = dev.device.state.lock().expect("tracking state lock");
    assert_eq!(tracked.reset_all_rings, 1);
    assert_eq!(tracked.set_features, vec![bits::VIRTIO_F_VERSION_1]);
    assert_eq!(
        tracked.restored_rings,
        vec![RestoreRingCall {
            queue_idx: 0,
            size: 256,
            desc: DESC,
            avail: AVAIL,
            used: USED,
            avail_idx: 7,
            used_idx: 5,
            msix_addr: 0,
            msix_data: 0,
        }],
    );
    assert_eq!(
        tracked.notify_addrs,
        vec![(
            0xC120 + bits::LEGACY_REG_QUEUE_NOTIFY,
            0xFEC0_0000 + bits::MODERN_BAR_NOTIFY_OFFSET as u64,
        )],
    );
}

#[test]
fn restore_virtio_state_propagates_ring_feature_flags() {
    let dev = make_tracking_device();
    let features = bits::VIRTIO_F_VERSION_1
        | bits::VIRTIO_F_RING_EVENT_IDX
        | bits::VIRTIO_F_RING_INDIRECT_DESC;

    dev.restore_migrate_state(&DeviceMigrateState::Virtio(transport(
        features,
        vec![live_queue()],
    )))
    .expect("a sane payload restores");

    let vs = dev.virtio_state.lock().expect("lock");
    assert!(
        vs.queues[0].event_idx_enabled(),
        "event_idx must be propagated to queues during restore",
    );
    assert!(
        vs.queues[0].indirect_supported(),
        "indirect_supported must be propagated to queues during restore",
    );
}

#[test]
fn a_legacy_queue_migrates_live_and_gets_its_backend_restored() {
    // The legacy transport has no enable register: a QUEUE_PFN write
    // makes the ring live. An export that uses only `queue_enabled`
    // marks every legacy queue dead, and the destination skips the
    // backend restore and the wake.
    let source = make_tracking_device();
    {
        let mut vs = source.virtio_state.lock().expect("virtio lock");
        // A legacy driver never negotiates VERSION_1.
        vs.guest_features = 0x42;
        vs.queues[0].set_addr_modern(DESC, AVAIL, USED);
        assert!(!vs.queue_enabled[0], "legacy never sets the enable flag");
    }

    let state = exported(&source);
    assert_eq!(state.queues.len(), 1);
    assert!(state.queues[0].live, "a configured legacy queue is live");

    let dest = make_tracking_device();
    dest.restore_migrate_state(&DeviceMigrateState::Virtio(state))
        .expect("a legacy payload restores");
    let tracked = dest.device.state.lock().expect("tracking state lock");
    assert_eq!(
        tracked.restored_rings.len(),
        1,
        "the backend must be told about a live legacy queue",
    );
}

#[test]
fn a_modern_queue_the_driver_never_enabled_migrates_dead() {
    let dev = make_tracking_device();
    {
        let mut vs = dev.virtio_state.lock().expect("virtio lock");
        vs.guest_features = bits::VIRTIO_F_VERSION_1;
        vs.queues[0].set_addr_modern(DESC, AVAIL, USED);
        vs.queue_enabled[0] = false;
    }
    let state = exported(&dev);
    assert!(!state.queues[0].live);
}

#[test]
fn restore_keeps_the_status_byte_the_driver_left() {
    // A synthesised DRIVER_OK tells a driver that never finished the
    // handshake that the device is ready.
    let dev = make_tracking_device();
    let mut state = transport(bits::VIRTIO_F_VERSION_1, Vec::new());
    state.status = bits::STATUS_ACKNOWLEDGE | bits::STATUS_DRIVER;

    dev.restore_migrate_state(&DeviceMigrateState::Virtio(state))
        .expect("restores");

    let vs = dev.virtio_state.lock().expect("lock");
    assert_eq!(vs.status, bits::STATUS_ACKNOWLEDGE | bits::STATUS_DRIVER);
}

#[test]
fn an_intx_guest_stays_on_intx_after_a_migration() {
    // A forced MSI-X enable routes every queue interrupt to NO_VECTOR
    // and moves device config from 0x14 to 0x18. A legacy read of
    // capacity then returns the vector registers.
    let msix = null_msix(2);
    let dev = make_tracking_device_with_msix(Some(Arc::clone(&msix)));
    assert!(!msix.is_enabled());

    let mut state = transport(bits::VIRTIO_F_VERSION_1, vec![live_queue()]);
    state.msix = Some(null_msix(2).export_state());

    dev.restore_migrate_state(&DeviceMigrateState::Virtio(state))
        .expect("restores");

    assert!(
        !msix.is_enabled(),
        "a source that never enabled MSI-X must not arrive with it on",
    );
}

#[test]
fn restore_carries_the_config_vector() {
    // Without it the driver never sees a config change, so it misses a
    // DEVICE_NEEDS_RESET after the migration.
    let msix = null_msix(2);
    let dev = make_tracking_device_with_msix(Some(Arc::clone(&msix)));
    let mut state = transport(bits::VIRTIO_F_VERSION_1, Vec::new());
    state.config_msix_vector = 1;
    let mut table = null_msix(2).export_state();
    table.enabled = true;
    state.msix = Some(table);

    dev.restore_migrate_state(&DeviceMigrateState::Virtio(state))
        .expect("restores");

    assert!(msix.is_enabled(), "an MSI-X source arrives with it on");
    assert_eq!(exported(&dev).config_msix_vector, 1);
}

#[test]
fn a_payload_for_another_kind_of_device_is_refused() {
    let dev = make_tracking_device();
    let error = dev
        .restore_migrate_state(&DeviceMigrateState::Nvme(Default::default()))
        .expect_err("an NVMe payload is not this device's");
    assert!(
        error.to_string().contains("this device is virtio"),
        "{error}"
    );
}

#[test]
fn a_queue_past_the_device_is_refused_and_nothing_is_applied() {
    let dev = make_tracking_device();
    let mut queue = live_queue();
    queue.queue_idx = 7;
    let error = dev
        .restore_migrate_state(&DeviceMigrateState::Virtio(transport(
            bits::VIRTIO_F_VERSION_1,
            vec![queue],
        )))
        .expect_err("queue 7 on a 1-queue device");
    assert!(error.to_string().contains("past this device"), "{error}");
    assert_eq!(
        dev.device.state.lock().expect("lock").restored_rings.len(),
        0,
        "a refused payload programs no ring",
    );
}

#[test]
fn a_duplicate_queue_is_refused() {
    let dev = make_tracking_device();
    dev.restore_migrate_state(&DeviceMigrateState::Virtio(transport(
        bits::VIRTIO_F_VERSION_1,
        vec![live_queue(), live_queue()],
    )))
    .expect_err("queue 0 twice");
}

/// A modern driver may shrink a ring below the offered size (VirtIO 1.3
/// sec 4.1.4.3.2), and the source exports the size it ran. The
/// destination must accept that size and call `set_size`. Otherwise it
/// walks 256 entries of a 128-entry ring.
#[test]
fn a_ring_the_driver_shrank_migrates_at_the_size_it_ran() {
    let source = make_tracking_device();
    // The driver writes the size before the addresses.
    write_modern_u16(&source, bits::COMMON_CFG_QUEUE_SELECT, 0);
    write_modern_u16(&source, bits::COMMON_CFG_QUEUE_SIZE, 128);
    {
        let mut vs = source.virtio_state.lock().expect("virtio lock");
        vs.guest_features = bits::VIRTIO_F_VERSION_1;
        vs.queues[0].set_addr_modern(DESC, AVAIL, USED);
        vs.queue_enabled[0] = true;
        vs.status = bits::STATUS_ACKNOWLEDGE
            | bits::STATUS_DRIVER
            | bits::STATUS_FEATURES_OK
            | bits::STATUS_DRIVER_OK;
    }

    let state = exported(&source);
    assert_eq!(
        state.queues[0].queue_size, 128,
        "the export reported a ring the source never ran"
    );

    let dest = make_tracking_device();
    dest.restore_migrate_state(&DeviceMigrateState::Virtio(state))
        .expect("a 128-entry ring on a 256-entry device restores");

    assert_eq!(
        dest.virtio_state.lock().expect("virtio lock").queues[0].size(),
        128,
        "the destination would walk past the ring the guest allocated",
    );
    assert_eq!(
        dest.device.state.lock().expect("lock").restored_rings[0].size,
        128,
        "the backend was told a ring size the guest never programmed",
    );

    // The restored ring is live: a guest kick reaches the backend, and
    // a second export keeps the size for a chained migration.
    let wo = vmm_core::common::WriteOp::from_buf(&0u16.to_le_bytes());
    dest.bar_rw(
        BarN::BAR0,
        usize::from(bits::LEGACY_REG_QUEUE_NOTIFY),
        RWOp::Write(&wo),
    );
    assert_eq!(
        notified(&dest),
        vec![0],
        "a kick never reached the restored ring"
    );
    assert_eq!(exported(&dest).queues[0].queue_size, 128);
}

#[test]
fn a_queue_larger_than_the_device_offers_is_refused() {
    let dev = make_tracking_device();
    let mut queue = live_queue();
    queue.queue_size = 512;
    let error = dev
        .restore_migrate_state(&DeviceMigrateState::Virtio(transport(
            bits::VIRTIO_F_VERSION_1,
            vec![queue],
        )))
        .expect_err("512 against this device's 256");
    assert!(error.to_string().contains("1..=256"), "{error}");
    assert_eq!(
        dev.device.state.lock().expect("lock").restored_rings.len(),
        0,
        "a refused payload programs no ring",
    );
}

#[test]
fn a_queue_size_that_is_not_a_power_of_two_is_refused() {
    for bad in [0u16, 48, 255] {
        let dev = make_tracking_device();
        let mut queue = live_queue();
        queue.queue_size = bad;
        let error = dev
            .restore_migrate_state(&DeviceMigrateState::Virtio(transport(
                bits::VIRTIO_F_VERSION_1,
                vec![queue],
            )))
            .expect_err("a ring no driver could have programmed");
        assert!(error.to_string().contains("1..=256"), "{error}");
    }
}

#[test]
fn more_chains_in_flight_than_the_ring_holds_is_refused() {
    let dev = make_tracking_device();
    let mut queue = live_queue();
    queue.used_idx = 0;
    queue.avail_idx = 300;
    let error = dev
        .restore_migrate_state(&DeviceMigrateState::Virtio(transport(
            bits::VIRTIO_F_VERSION_1,
            vec![queue],
        )))
        .expect_err("300 chains on a 256-descriptor ring");
    assert!(error.to_string().contains("in flight"), "{error}");
}

#[test]
fn an_unmapped_ring_is_refused() {
    let dev = make_tracking_device();
    let mut queue = live_queue();
    queue.desc_addr = 0xDEAD_0000;
    let error = dev
        .restore_migrate_state(&DeviceMigrateState::Virtio(transport(
            bits::VIRTIO_F_VERSION_1,
            vec![queue],
        )))
        .expect_err("a ring outside guest memory");
    assert!(error.to_string().contains("rings"), "{error}");
}

#[test]
fn a_vector_past_the_table_is_refused() {
    let msix = null_msix(2);
    let dev = make_tracking_device_with_msix(Some(msix));
    let mut queue = live_queue();
    queue.msix_vector = 9;
    let mut state = transport(bits::VIRTIO_F_VERSION_1, vec![queue]);
    state.msix = Some(null_msix(2).export_state());
    let error = dev
        .restore_migrate_state(&DeviceMigrateState::Virtio(state))
        .expect_err("vector 9 in a 2-entry table");
    assert!(error.to_string().contains("past the 2-entry"), "{error}");
}

#[test]
fn a_payload_with_no_msix_table_is_refused_by_an_msix_device() {
    let msix = null_msix(2);
    let dev = make_tracking_device_with_msix(Some(msix));
    let state = transport(bits::VIRTIO_F_VERSION_1, Vec::new());
    dev.restore_migrate_state(&DeviceMigrateState::Virtio(state))
        .expect_err("this device has a table and the payload has none");
}

#[test]
fn cap_chain_starts_at_0x40() {
    let dev = make_test_device();
    // The capabilities pointer is at 0x34.
    let cap_ptr = dev.cfg_read(0x34, 1);
    assert_eq!(cap_ptr, 0x40, "cap chain should start at 0x40");
}

#[test]
fn cap_chain_byte_reads() {
    let dev = make_test_device();
    // Byte read at 0x40 → cap_vndr = 0x09
    assert_eq!(dev.cfg_read(0x40, 1), 0x09, "byte read: cap_vndr");
    // Byte read at 0x41 → cap_next = 0x50 (notify cap)
    assert_eq!(dev.cfg_read(0x41, 1), 0x50, "byte read: cap_next");
    // Byte read at 0x42 → cap_len = 16
    assert_eq!(dev.cfg_read(0x42, 1), 16, "byte read: cap_len");
    // Byte read at 0x43 → cfg_type = 1 (common)
    assert_eq!(dev.cfg_read(0x43, 1), 1, "byte read: cfg_type");
}

#[test]
fn cap_chain_common_cfg() {
    let dev = make_test_device();
    // No MSI-X → common cap at 0x40
    let dword0 = dev.cfg_read(0x40, 4);
    // Byte 0: cap_vndr = 0x09
    assert_eq!(dword0 & 0xFF, 0x09, "cap_vndr should be 0x09");
    // Byte 3: cfg_type = 1 (common)
    assert_eq!((dword0 >> 24) & 0xFF, 1, "cfg_type should be COMMON_CFG");
    // Bar field at offset+4
    let dword1 = dev.cfg_read(0x44, 4);
    assert_eq!(
        dword1 & 0xFF,
        bits::MODERN_BAR_IDX as u32,
        "common cap should point to BAR2"
    );
    // Offset within BAR at offset+8
    let dword2 = dev.cfg_read(0x48, 4);
    assert_eq!(
        dword2,
        bits::MODERN_BAR_COMMON_OFFSET,
        "common cap offset should be 0"
    );
}

#[test]
fn cap_chain_notify_cfg() {
    let dev = make_test_device();
    // No MSI-X → notify cap at 0x50
    let dword0 = dev.cfg_read(0x50, 4);
    assert_eq!(dword0 & 0xFF, 0x09);
    assert_eq!((dword0 >> 24) & 0xFF, 2, "cfg_type should be NOTIFY_CFG");
    // Offset within BAR
    let offset = dev.cfg_read(0x58, 4);
    assert_eq!(offset, bits::MODERN_BAR_NOTIFY_OFFSET);
    // Multiplier (extra dword at cap+16)
    let mult = dev.cfg_read(0x60, 4);
    assert_eq!(mult, 0, "notify_off_multiplier should be 0");
}

#[test]
fn cap_chain_last_is_device_cfg() {
    let dev = make_test_device();
    // No MSI-X → device cap at 0x74
    let dword0 = dev.cfg_read(0x74, 4);
    assert_eq!(dword0 & 0xFF, 0x09);
    assert_eq!((dword0 >> 24) & 0xFF, 4, "cfg_type should be DEVICE_CFG");
    // Next pointer should be 0 (last in chain)
    assert_eq!((dword0 >> 8) & 0xFF, 0, "last cap next should be 0");
}

#[test]
fn modern_features_include_version_1() {
    use vmm_core::common::{ReadOp, WriteOp};
    let dev = make_test_device();

    // Select the high 32 bits, then read device_feature.
    let wo = WriteOp::from_buf(&[1, 0, 0, 0]);
    dev.bar_rw(BarN::BAR2, 0x00, RWOp::Write(&wo));

    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR2, 0x04, RWOp::Read(&mut ro));
    let high = u32::from_le_bytes([
        ro.buf()[0],
        ro.buf()[1],
        ro.buf()[2],
        ro.buf()[3],
    ]);
    // VERSION_1 = bit 32 → bit 0 of high word
    assert_ne!(high & 1, 0, "VERSION_1 should be in modern features");
}

#[test]
fn modern_features_include_event_idx() {
    use vmm_core::common::ReadOp;
    let dev = make_test_device();

    // The low 32 bits are selected by default.
    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR2, 0x04, RWOp::Read(&mut ro));
    let low = u32::from_le_bytes([
        ro.buf()[0],
        ro.buf()[1],
        ro.buf()[2],
        ro.buf()[3],
    ]);
    // EVENT_IDX = bit 29
    assert_ne!(low & (1 << 29), 0, "EVENT_IDX should be in modern features");
    // INDIRECT_DESC = bit 28
    assert_ne!(
        low & (1 << 28),
        0,
        "INDIRECT_DESC should be in modern features"
    );
}

#[test]
fn legacy_features_no_event_idx() {
    use vmm_core::common::ReadOp;
    let dev = make_test_device();

    // Legacy device_features is at BAR0 offset 0x00.
    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR0, 0x00, RWOp::Read(&mut ro));
    let val = u32::from_le_bytes([
        ro.buf()[0],
        ro.buf()[1],
        ro.buf()[2],
        ro.buf()[3],
    ]);
    // EVENT_IDX is bit 29.
    assert_eq!(
        val & (1 << 29),
        0,
        "EVENT_IDX must NOT appear in legacy features"
    );
}

#[test]
fn modern_num_queues() {
    use vmm_core::common::ReadOp;
    let dev = make_test_device();

    // num_queues is at common config offset 0x12.
    let mut ro = ReadOp::new(2);
    dev.bar_rw(BarN::BAR2, 0x12, RWOp::Read(&mut ro));
    let nq = u16::from_le_bytes([ro.buf()[0], ro.buf()[1]]);
    assert_eq!(nq, 1, "should report 1 queue");
}

#[test]
fn modern_device_status() {
    use vmm_core::common::{ReadOp, WriteOp};
    let dev = make_test_device();

    // device_status is at common config offset 0x14.
    let wo = WriteOp::from_buf(&[bits::STATUS_ACKNOWLEDGE]);
    dev.bar_rw(BarN::BAR2, 0x14, RWOp::Write(&wo));

    let mut ro = ReadOp::new(1);
    dev.bar_rw(BarN::BAR2, 0x14, RWOp::Read(&mut ro));
    assert_eq!(ro.buf()[0], bits::STATUS_ACKNOWLEDGE);
}

fn read_modern_u16<D: VirtioDevice>(dev: &VirtioPciDevice<D>, off: u16) -> u16 {
    use vmm_core::common::ReadOp;
    let mut ro = ReadOp::new(2);
    dev.bar_rw(BarN::BAR2, usize::from(off), RWOp::Read(&mut ro));
    u16::from_le_bytes([ro.buf()[0], ro.buf()[1]])
}

fn write_modern_u16<D: VirtioDevice>(
    dev: &VirtioPciDevice<D>,
    off: u16,
    val: u16,
) {
    use vmm_core::common::WriteOp;
    let wo = WriteOp::from_buf(&val.to_le_bytes());
    dev.bar_rw(BarN::BAR2, usize::from(off), RWOp::Write(&wo));
}

fn modern_status(dev: &VirtioPciDevice<TestVirtioDevice>) -> u8 {
    read_modern_u16(dev, bits::COMMON_CFG_DEVICE_STATUS) as u8
}

fn write_modern_u32(
    dev: &VirtioPciDevice<TestVirtioDevice>,
    off: u16,
    val: u32,
) {
    use vmm_core::common::WriteOp;
    let wo = WriteOp::from_buf(&val.to_le_bytes());
    dev.bar_rw(BarN::BAR2, usize::from(off), RWOp::Write(&wo));
}

fn read_modern_u32(dev: &VirtioPciDevice<TestVirtioDevice>, off: u16) -> u32 {
    use vmm_core::common::ReadOp;
    let mut ro = ReadOp::new(4);
    dev.bar_rw(BarN::BAR2, usize::from(off), RWOp::Read(&mut ro));
    u32::from_le_bytes([ro.buf()[0], ro.buf()[1], ro.buf()[2], ro.buf()[3]])
}

fn queue_event_idx(dev: &VirtioPciDevice<TestVirtioDevice>) -> bool {
    dev.virtio_state.lock().expect("virtio lock").queues[0].event_idx_enabled()
}

/// Features are final at FEATURES_OK (VirtIO 1.3 sec 2.2.1). Only a
/// reset renegotiates. A feature write after FEATURES_OK would change
/// ring handling under live rings: `VirtioCompletion` copies EVENT_IDX
/// when it is built, so it and its queue would use different kick
/// rules.
#[test]
fn a_feature_write_after_features_ok_changes_nothing() {
    let dev = make_test_device();
    let event_idx = bits::VIRTIO_F_RING_EVENT_IDX as u32;

    write_modern_u32(&dev, bits::COMMON_CFG_DRIVER_FEATURE, event_idx);
    write_modern_u16(
        &dev,
        bits::COMMON_CFG_DEVICE_STATUS,
        u16::from(
            bits::STATUS_ACKNOWLEDGE
                | bits::STATUS_DRIVER
                | bits::STATUS_FEATURES_OK,
        ),
    );
    assert!(
        queue_event_idx(&dev),
        "FEATURES_OK did not reach the queues"
    );

    write_modern_u32(&dev, bits::COMMON_CFG_DRIVER_FEATURE, 0);

    assert_eq!(
        read_modern_u32(&dev, bits::COMMON_CFG_DRIVER_FEATURE),
        event_idx,
        "the device renegotiated after FEATURES_OK",
    );
    assert!(queue_event_idx(&dev), "a live queue lost EVENT_IDX");
}

/// VirtIO 1.3 sec 2.1.2: the driver must not clear a status bit.
/// Otherwise a driver can clear DEVICE_NEEDS_RESET and use a stopped
/// device, or toggle FEATURES_OK to renegotiate. Only a write of 0
/// clears bits.
#[test]
fn clearing_a_status_bit_is_refused_and_asks_for_a_reset() {
    let dev = make_test_device();
    let live = bits::STATUS_ACKNOWLEDGE | bits::STATUS_DRIVER;
    write_modern_u16(&dev, bits::COMMON_CFG_DEVICE_STATUS, u16::from(live));

    write_modern_u16(
        &dev,
        bits::COMMON_CFG_DEVICE_STATUS,
        u16::from(bits::STATUS_ACKNOWLEDGE),
    );

    let status = modern_status(&dev);
    assert_eq!(status & live, live, "a status bit was cleared");
    assert_ne!(
        status & bits::STATUS_DEVICE_NEEDS_RESET,
        0,
        "the driver was not told the device stopped",
    );

    // A write of 0 still resets and clears the bit.
    write_modern_u16(&dev, bits::COMMON_CFG_DEVICE_STATUS, 0);
    assert_eq!(modern_status(&dev), 0);
}

/// A driver may shrink a ring before it enables it (VirtIO 1.3 sec
/// 4.1.4.3.2). The device then walks only that many entries. A size the
/// device did not offer, or not a power of two, sets DEVICE_NEEDS_RESET.
#[test]
fn a_modern_driver_can_shrink_the_ring_and_a_bad_size_is_refused() {
    let dev = make_test_device();
    assert_eq!(read_modern_u16(&dev, bits::COMMON_CFG_QUEUE_SIZE), 256);

    write_modern_u16(&dev, bits::COMMON_CFG_QUEUE_SIZE, 64);
    assert_eq!(read_modern_u16(&dev, bits::COMMON_CFG_QUEUE_SIZE), 64);
    assert_eq!(
        dev.virtio_state.lock().expect("virtio lock").queues[0].size(),
        64,
        "the device kept its own size for the ring walk",
    );
    assert_eq!(modern_status(&dev) & bits::STATUS_DEVICE_NEEDS_RESET, 0);

    for bad in [0u16, 48, 512] {
        write_modern_u16(&dev, bits::COMMON_CFG_QUEUE_SIZE, bad);
        assert_eq!(
            read_modern_u16(&dev, bits::COMMON_CFG_QUEUE_SIZE),
            64,
            "size {bad} was taken",
        );
    }
    assert_ne!(modern_status(&dev) & bits::STATUS_DEVICE_NEEDS_RESET, 0);

    // A reset restores the offered size.
    write_modern_u16(&dev, bits::COMMON_CFG_DEVICE_STATUS, 0);
    assert_eq!(read_modern_u16(&dev, bits::COMMON_CFG_QUEUE_SIZE), 256);
}
