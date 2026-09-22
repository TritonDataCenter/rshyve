// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::*;
use vmm_core::common::{ReadOp, WriteOp};

/// Counts the GPE bits a register file raises.
#[derive(Default)]
struct RecordingSink {
    raised: Mutex<Vec<GpeBit>>,
}

impl RecordingSink {
    fn raised(&self) -> Vec<GpeBit> {
        self.raised.lock().expect("sink lock poisoned").clone()
    }
}

impl HotplugEventSink for RecordingSink {
    fn raise(&self, bit: GpeBit) {
        self.raised.lock().expect("sink lock poisoned").push(bit);
    }
}

fn test_log() -> Logger {
    Logger::root(slog::Discard, slog::o!())
}

fn block(max_cpus: u32) -> (Arc<CpuHotplug>, Arc<RecordingSink>) {
    let sink = Arc::new(RecordingSink::default());
    let cpus = CpuHotplug::new(max_cpus, sink.clone(), test_log());
    cpus.set_boot_cpus(1);
    (cpus, sink)
}

fn read(cpus: &CpuHotplug, offset: usize, len: usize) -> u64 {
    let mut ro = ReadOp::new(len);
    cpus.pio_rw(offset, RWOp::Read(&mut ro));
    let mut bytes = [0u8; 8];
    bytes[..len].copy_from_slice(ro.buf());
    u64::from_le_bytes(bytes)
}

/// Read from a buffer that already holds a value, so a handler
/// that answers by leaving the buffer alone is visible. `ReadOp`
/// arrives zeroed, so a plain `read` cannot tell the two apart.
fn read_poisoned(cpus: &CpuHotplug, offset: usize, len: usize) -> u64 {
    let mut ro = ReadOp::new(len);
    ro.write_u64(u64::MAX);
    cpus.pio_rw(offset, RWOp::Read(&mut ro));
    let mut bytes = [0u8; 8];
    bytes[..len].copy_from_slice(ro.buf());
    u64::from_le_bytes(bytes)
}

fn write(cpus: &CpuHotplug, offset: usize, len: usize, value: u64) {
    let bytes = value.to_le_bytes();
    let wo = WriteOp::from_buf(&bytes[..len]);
    cpus.pio_rw(offset, RWOp::Write(&wo));
}

fn select(cpus: &CpuHotplug, cpu_id: u32) {
    write(cpus, OFF_SELECTOR_W, DWORD, u64::from(cpu_id));
}

fn flags(cpus: &CpuHotplug, cpu_id: u32) -> u8 {
    select(cpus, cpu_id);
    read(cpus, OFF_FLAGS_RW, BYTE) as u8
}

/// Run the guest's side of an add: scan for the event and
/// acknowledge it, the way the `CSCN` method does.
fn acknowledge_insert(cpus: &CpuHotplug, cpu_id: u32) {
    select(cpus, cpu_id);
    write(cpus, OFF_FLAGS_RW, BYTE, u64::from(CTL_CLEAR_INSERT));
}

fn acknowledge_remove(cpus: &CpuHotplug, cpu_id: u32) {
    select(cpus, cpu_id);
    write(cpus, OFF_FLAGS_RW, BYTE, u64::from(CTL_CLEAR_REMOVE));
}

fn eject(cpus: &CpuHotplug, cpu_id: u32) {
    select(cpus, cpu_id);
    write(cpus, OFF_FLAGS_RW, BYTE, u64::from(CTL_EJECT));
}

#[test]
fn the_boot_cpu_reads_as_enabled_with_no_event() {
    let (cpus, sink) = block(4);

    assert_eq!(flags(&cpus, 0), STS_ENABLED);
    assert_eq!(flags(&cpus, 1), 0);
    assert!(sink.raised().is_empty());
}

#[test]
fn notify_added_raises_the_gpe_bit_and_sets_the_insert_event() {
    let (cpus, sink) = block(4);

    cpus.notify_added(2);

    assert_eq!(sink.raised(), vec![GpeBit::Cpu]);
    assert_eq!(flags(&cpus, 2), STS_ENABLED | STS_INSERT_EVENT);
}

#[test]
fn notify_removed_raises_the_gpe_bit_and_sets_the_remove_event() {
    let (cpus, sink) = block(4);
    cpus.notify_added(2);

    cpus.notify_removed(2);

    assert_eq!(sink.raised(), vec![GpeBit::Cpu, GpeBit::Cpu]);
    assert_eq!(
        flags(&cpus, 2),
        STS_ENABLED | STS_INSERT_EVENT | STS_REMOVE_EVENT,
    );
}

#[test]
fn notify_removed_refuses_the_boot_cpu() {
    let (cpus, sink) = block(4);

    cpus.notify_removed(0);

    assert!(sink.raised().is_empty());
    assert_eq!(flags(&cpus, 0), STS_ENABLED);
}

#[test]
fn a_host_event_for_a_slot_that_does_not_exist_is_dropped() {
    let (cpus, sink) = block(2);

    cpus.notify_added(9);
    cpus.notify_removed(u32::MAX);

    assert!(sink.raised().is_empty());
}

#[test]
fn an_add_cancels_a_removal_the_guest_has_not_finished() {
    let (cpus, _sink) = block(4);
    cpus.notify_added(1);
    cpus.notify_removed(1);

    cpus.notify_added(1);
    eject(&cpus, 1);

    assert!(cpus.take_eject_requests().is_empty());
    assert_eq!(flags(&cpus, 1), STS_ENABLED | STS_INSERT_EVENT);
}

#[test]
fn an_out_of_range_selector_reads_as_zero() {
    let (cpus, _sink) = block(4);
    cpus.notify_added(1);

    for selector in [4u32, 5, 0x1000, u32::MAX] {
        select(&cpus, selector);

        assert_eq!(cpus.selected_index(), None, "{selector:#x}");
        assert_eq!(read(&cpus, OFF_CMD_DATA2_R, DWORD), 0);
        assert_eq!(read(&cpus, OFF_FLAGS_RW, BYTE), 0);
        assert_eq!(read(&cpus, OFF_CMD_DATA_RW, DWORD), 0);
    }
}

#[test]
fn an_out_of_range_selector_makes_every_other_write_a_noop() {
    let (cpus, _sink) = block(4);
    cpus.notify_added(1);
    cpus.notify_removed(1);
    let before = cpus.slot(1).expect("slot 1 exists");

    select(&cpus, u32::MAX);
    write(&cpus, OFF_FLAGS_RW, BYTE, u64::from(CTL_EJECT));
    write(&cpus, OFF_FLAGS_RW, BYTE, u64::from(CTL_CLEAR_INSERT));
    write(&cpus, OFF_COMMAND_W, BYTE, u64::from(CMD_GET_CPU_ID));
    write(&cpus, OFF_CMD_DATA_RW, DWORD, 0xDEAD_BEEF);

    assert_eq!(cpus.slot(1), Some(before));
    assert!(cpus.take_eject_requests().is_empty());
    // A valid selector brings the block back, as the ABI requires.
    assert_eq!(
        flags(&cpus, 1),
        STS_ENABLED | STS_INSERT_EVENT | STS_REMOVE_EVENT,
    );
}

#[test]
fn every_selector_and_offset_is_safe() {
    let (cpus, _sink) = block(4);

    for selector in [0u32, 3, 4, u32::MAX / 2, u32::MAX] {
        select(&cpus, selector);
        for offset in 0..usize::from(CPU_HOTPLUG_LEN) + 4 {
            for len in [1usize, 2, 4, 8] {
                read(&cpus, offset, len);
                write(&cpus, offset, len, u64::MAX);
            }
        }
    }
}

#[test]
fn a_bad_access_width_reads_zero_and_writes_nothing() {
    let (cpus, _sink) = block(4);
    cpus.notify_added(1);
    cpus.notify_removed(1);
    select(&cpus, 1);
    let before = cpus.slot(1).expect("slot 1 exists");

    // The flags register is a byte and the data registers are
    // DWORDs. Every other width is undefined by the ABI.
    assert_eq!(read(&cpus, OFF_FLAGS_RW, 4), 0);
    assert_eq!(read(&cpus, OFF_CMD_DATA_RW, 1), 0);
    assert_eq!(read(&cpus, OFF_CMD_DATA2_R, 2), 0);

    write(&cpus, OFF_FLAGS_RW, 4, u64::from(CTL_EJECT));
    write(&cpus, OFF_SELECTOR_W, 1, 3);
    write(&cpus, OFF_COMMAND_W, 4, u64::from(CMD_GET_CPU_ID));

    assert_eq!(cpus.slot(1), Some(before));
    assert!(cpus.take_eject_requests().is_empty());
    assert_eq!(cpus.selected_index(), Some(1));
}

#[test]
fn reserved_offsets_read_zero() {
    let (cpus, _sink) = block(4);
    cpus.notify_added(1);
    select(&cpus, 1);

    for offset in [0x5usize, 0x6, 0x7, 0x9, 0xA, 0xB] {
        assert_eq!(read(&cpus, offset, BYTE), 0, "offset {offset:#x}");
    }
}

#[test]
fn the_boot_cpu_cannot_be_ejected() {
    let (cpus, _sink) = block(4);
    // Even with a removal wrongly recorded against slot 0.
    cpus.notify_removed(0);

    eject(&cpus, 0);

    assert!(cpus.take_eject_requests().is_empty());
    assert_eq!(flags(&cpus, 0), STS_ENABLED);
}

#[test]
fn an_eject_the_host_did_not_ask_for_is_refused() {
    let (cpus, _sink) = block(4);
    cpus.notify_added(2);

    eject(&cpus, 2);

    assert!(cpus.take_eject_requests().is_empty());
    assert_eq!(flags(&cpus, 2) & STS_ENABLED, STS_ENABLED);
}

#[test]
fn an_eject_of_an_absent_cpu_is_refused() {
    let (cpus, _sink) = block(4);

    eject(&cpus, 3);

    assert!(cpus.take_eject_requests().is_empty());
}

#[test]
fn a_pending_removal_survives_the_acknowledge() {
    let (cpus, _sink) = block(4);
    cpus.notify_added(1);
    cpus.notify_removed(1);

    // What the CSCN method does: notify, then clear the event.
    acknowledge_remove(&cpus, 1);
    assert_eq!(flags(&cpus, 1) & STS_REMOVE_EVENT, 0);

    // The guest runs _EJ0 only after it has taken the CPU offline.
    eject(&cpus, 1);

    assert_eq!(cpus.take_eject_requests(), vec![1]);
    assert_eq!(flags(&cpus, 1), STS_INSERT_EVENT);
}

#[test]
fn an_eject_is_recorded_once() {
    let (cpus, _sink) = block(4);
    cpus.notify_added(1);
    cpus.notify_removed(1);

    eject(&cpus, 1);
    eject(&cpus, 1);

    assert_eq!(cpus.take_eject_requests(), vec![1]);
    assert!(cpus.take_eject_requests().is_empty());
}

#[test]
fn the_insert_acknowledge_clears_only_the_insert_event() {
    let (cpus, _sink) = block(4);
    cpus.notify_added(1);
    cpus.notify_removed(1);

    acknowledge_insert(&cpus, 1);

    assert_eq!(flags(&cpus, 1), STS_ENABLED | STS_REMOVE_EVENT);
}

#[test]
fn an_acknowledge_bit_beats_the_eject_bit_in_the_same_write() {
    let (cpus, _sink) = block(4);
    cpus.notify_added(1);
    cpus.notify_removed(1);
    select(&cpus, 1);

    write(
        &cpus,
        OFF_FLAGS_RW,
        BYTE,
        u64::from(CTL_CLEAR_REMOVE | CTL_EJECT),
    );

    assert!(cpus.take_eject_requests().is_empty());
    assert_eq!(flags(&cpus, 1) & STS_REMOVE_EVENT, 0);
}

#[test]
fn the_firmware_eject_handover_is_refused() {
    let (cpus, _sink) = block(4);
    cpus.notify_added(1);
    cpus.notify_removed(1);
    select(&cpus, 1);

    write(&cpus, OFF_FLAGS_RW, BYTE, u64::from(CTL_FW_EJECT));

    assert!(cpus.take_eject_requests().is_empty());
    // Status bit 4 stays clear, so no guest waits for a firmware
    // eject that will never happen.
    assert_eq!(
        flags(&cpus, 1),
        STS_ENABLED | STS_INSERT_EVENT | STS_REMOVE_EVENT,
    );
}

#[test]
fn the_next_event_command_selects_a_cpu_with_an_event() {
    let (cpus, _sink) = block(4);
    cpus.notify_added(3);

    select(&cpus, 0);
    write(&cpus, OFF_COMMAND_W, BYTE, u64::from(CMD_GET_NEXT_EVENT));

    assert_eq!(cpus.selected_index(), Some(3));
    assert_eq!(read(&cpus, OFF_CMD_DATA_RW, DWORD), 3);
    // The modern interface is detected by this register reading 0.
    assert_eq!(read(&cpus, OFF_CMD_DATA2_R, DWORD), 0);
}

#[test]
fn the_next_event_command_wraps_and_stops() {
    let (cpus, _sink) = block(4);
    cpus.notify_added(1);

    select(&cpus, 2);
    write(&cpus, OFF_COMMAND_W, BYTE, u64::from(CMD_GET_NEXT_EVENT));

    assert_eq!(cpus.selected_index(), Some(1));
}

#[test]
fn the_next_event_command_holds_the_selector_when_nothing_is_pending() {
    let (cpus, _sink) = block(4);

    select(&cpus, 2);
    write(&cpus, OFF_COMMAND_W, BYTE, u64::from(CMD_GET_NEXT_EVENT));

    assert_eq!(cpus.selected_index(), Some(2));
    assert_eq!(read(&cpus, OFF_CMD_DATA_RW, DWORD), 2);
}

#[test]
fn the_cpu_id_command_returns_the_apic_id() {
    let (cpus, _sink) = block(4);

    select(&cpus, 3);
    write(&cpus, OFF_COMMAND_W, BYTE, u64::from(CMD_GET_CPU_ID));

    // The MADT and the _MAT buffer give slot n APIC id n.
    assert_eq!(read(&cpus, OFF_CMD_DATA_RW, DWORD), 3);
    assert_eq!(read(&cpus, OFF_CMD_DATA2_R, DWORD), 0);
}

#[test]
fn a_command_outside_the_abi_is_ignored() {
    let (cpus, _sink) = block(4);
    select(&cpus, 3);
    write(&cpus, OFF_COMMAND_W, BYTE, u64::from(CMD_GET_CPU_ID));

    write(&cpus, OFF_COMMAND_W, BYTE, 0xFF);

    // The last accepted command still drives the data register.
    assert_eq!(read(&cpus, OFF_CMD_DATA_RW, DWORD), 3);
}

#[test]
fn the_ost_commands_store_what_the_guest_reports() {
    let (cpus, _sink) = block(4);
    select(&cpus, 2);

    write(&cpus, OFF_COMMAND_W, BYTE, u64::from(CMD_OST_EVENT));
    write(&cpus, OFF_CMD_DATA_RW, DWORD, 0x0000_0103);
    write(&cpus, OFF_COMMAND_W, BYTE, u64::from(CMD_OST_STATUS));
    write(&cpus, OFF_CMD_DATA_RW, DWORD, 0x8000_0001);

    let slot = cpus.slot(2).expect("slot 2 exists");
    assert_eq!(slot.ost_event, 0x0000_0103);
    assert_eq!(slot.ost_status, 0x8000_0001);
    // Neither command answers a read.
    assert_eq!(read(&cpus, OFF_CMD_DATA_RW, DWORD), 0);
}

#[test]
fn the_slot_count_is_clamped() {
    let sink = Arc::new(RecordingSink::default());
    let cpus = CpuHotplug::new(u32::MAX, sink, test_log());

    select(&cpus, u32::try_from(MAX_CPU_SLOTS).expect("fits a u32"));
    assert_eq!(cpus.selected_index(), None);

    select(&cpus, u32::try_from(MAX_CPU_SLOTS - 1).expect("fits a u32"));
    assert_eq!(cpus.selected_index(), Some(MAX_CPU_SLOTS - 1));
}

#[test]
fn a_block_with_no_slots_answers_nothing() {
    let sink = Arc::new(RecordingSink::default());
    let cpus = CpuHotplug::new(0, sink.clone(), test_log());

    cpus.set_boot_cpus(1);
    cpus.notify_added(0);
    select(&cpus, 0);
    write(&cpus, OFF_COMMAND_W, BYTE, u64::from(CMD_GET_NEXT_EVENT));

    assert_eq!(cpus.selected_index(), None);
    assert_eq!(read(&cpus, OFF_FLAGS_RW, BYTE), 0);
    assert!(sink.raised().is_empty());
}

#[test]
fn set_boot_cpus_marks_only_the_boot_slots() {
    let sink = Arc::new(RecordingSink::default());
    let cpus = CpuHotplug::new(4, sink.clone(), test_log());

    cpus.set_boot_cpus(2);

    assert_eq!(flags(&cpus, 0), STS_ENABLED);
    assert_eq!(flags(&cpus, 1), STS_ENABLED);
    assert_eq!(flags(&cpus, 2), 0);
    assert!(sink.raised().is_empty());
}

#[test]
fn set_boot_cpus_past_the_last_slot_is_clamped() {
    let sink = Arc::new(RecordingSink::default());
    let cpus = CpuHotplug::new(2, sink, test_log());

    cpus.set_boot_cpus(u32::MAX);

    assert_eq!(flags(&cpus, 0), STS_ENABLED);
    assert_eq!(flags(&cpus, 1), STS_ENABLED);
    assert_eq!(cpus.slot(2), None);
}

/// A refused read must overwrite the buffer, not leave it. The
/// exit structure is reused, so whatever it held would otherwise
/// go back to the guest.
#[test]
fn a_refused_read_cannot_return_a_stale_buffer() {
    let (cpus, _sink) = block(4);
    cpus.set_boot_cpus(1);

    for offset in 0..usize::from(CPU_HOTPLUG_LEN) {
        for len in [1usize, 2, 4, 8] {
            let value = read_poisoned(&cpus, offset, len);
            // Only the three registers the ABI defines answer.
            let defined = matches!(
                (offset, len),
                (OFF_CMD_DATA2_R, DWORD)
                    | (OFF_FLAGS_RW, BYTE)
                    | (OFF_CMD_DATA_RW, DWORD)
            );
            if !defined {
                assert_eq!(
                    value, 0,
                    "offset {offset} width {len} leaked a stale buffer",
                );
            }
        }
    }

    // An out of range selector must not turn a defined register
    // into a stale read either.
    write(&cpus, OFF_SELECTOR_W, DWORD, u64::from(u32::MAX));
    for offset in 0..usize::from(CPU_HOTPLUG_LEN) {
        for len in [1usize, 2, 4, 8] {
            assert_eq!(
                read_poisoned(&cpus, offset, len),
                0,
                "a bad selector leaked at offset {offset} width {len}",
            );
        }
    }
}
