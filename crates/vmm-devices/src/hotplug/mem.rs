// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Memory hotplug register file at PIO 0x0A00.
//!
//! The layout is the QEMU guest ABI, QEMU `docs/specs/acpi_mem_hotplug.rst`.
//! An unmodified guest driver already speaks it, and the AML in
//! [`crate::acpi::hotplug::mem`] is written against the same document.
//!
//! The block is read and write asymmetric: the same 24 bytes hold one
//! set of registers for reads and a different set for writes. All of
//! them apply to the slot named by the last selector the guest wrote.
//!
//! # Hot remove
//!
//! Memory can be added but never taken back. The illumos kernel has no
//! `VM_FREE_MEMSEG` ioctl, and `vm_free_memseg` has only destroy path
//! callers, so a segment lives until the VM goes away. The eject path
//! only records the guest request. The caller reports the request and
//! refuses it. Nothing in this file may assume that removal works.

use std::sync::{Arc, Mutex};

use slog::Logger;
use vmm_core::common::RWOp;

use crate::acpi_gpe::{GpeBit, HotplugEventSink};

/// Base I/O port of the memory hotplug register block.
pub const MEM_HOTPLUG_IO_BASE: u16 = 0x0A00;

/// Length of the register block in bytes.
pub const MEM_HOTPLUG_IO_LEN: u16 = 0x18;

/// Largest slot count this VMM can back.
///
/// The illumos kernel gives one VM at most `VM_MAX_MEMSEGS` (5) memory
/// segments and `VM_MAX_MEMMAPS` (8) guest physical mappings. Boot RAM
/// already spends some of both, so a slot past this cap could never
/// get a real segment behind it. Advertising one would give the guest
/// a DIMM that can never arrive.
pub const MAX_SLOTS: usize = 8;

const _: () = assert!(MAX_SLOTS <= u8::MAX as usize);

// ── Read registers ────────────────────────────────────────────────

/// Low 32 bits of the slot base address.
const REG_ADDR_LOW: usize = 0x00;
/// High 32 bits of the slot base address.
const REG_ADDR_HIGH: usize = 0x04;
/// Low 32 bits of the slot length.
const REG_SIZE_LOW: usize = 0x08;
/// High 32 bits of the slot length.
const REG_SIZE_HIGH: usize = 0x0C;
/// NUMA proximity domain of the slot.
const REG_PROXIMITY: usize = 0x10;
/// Slot status bits.
const REG_STATUS: usize = 0x14;

// ── Write registers, at the same addresses ────────────────────────

/// Slot selector. Every other register applies to the slot it names.
const REG_SELECTOR: usize = 0x00;
/// `_OST` event code the guest reports.
const REG_OST_EVENT: usize = 0x04;
/// `_OST` status code the guest reports.
const REG_OST_STATUS: usize = 0x08;
/// Slot control bits.
const REG_CONTROL: usize = 0x14;

// ── Bits of the status and control registers ──────────────────────

/// Read: the slot holds memory the guest may use.
const STATUS_ENABLED: u8 = 1 << 0;
/// Read: the slot has an insert event the guest has not handled.
const STATUS_INSERTING: u8 = 1 << 1;
/// Read: the slot has a remove event the guest has not handled.
const STATUS_REMOVING: u8 = 1 << 2;

/// Write: clear the insert event.
const CONTROL_CLEAR_INSERT: u32 = 1 << 1;
/// Write: clear the remove event.
const CONTROL_CLEAR_REMOVE: u32 = 1 << 2;
/// Write: the guest ran `_EJ0` and wants the slot back.
const CONTROL_EJECT: u32 = 1 << 3;

/// Why a slot cannot take a memory region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MemHotplugError {
    #[error("slot {slot} is past the {slots} slots this VM has")]
    NoSuchSlot { slot: usize, slots: usize },
    #[error("slot {slot} needs a region of non-zero length")]
    EmptyRegion { slot: usize },
    #[error(
        "slot {slot} at {base:#x} with length {len:#x} runs past the end \
         of the address space"
    )]
    RegionWraps { slot: usize, base: u64, len: u64 },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Slot {
    base: u64,
    len: u64,
    enabled: bool,
    inserting: bool,
    removing: bool,
    ost_event: u32,
    ost_status: u32,
}

impl Slot {
    /// The byte the guest reads from [`REG_STATUS`].
    fn status(&self) -> u8 {
        let mut bits = 0;
        if self.enabled {
            bits |= STATUS_ENABLED;
        }
        if self.inserting {
            bits |= STATUS_INSERTING;
        }
        if self.removing {
            bits |= STATUS_REMOVING;
        }
        bits
    }
}

struct Inner {
    /// The selected slot, or `None` when the guest wrote a selector
    /// that names no slot.
    ///
    /// The bounds check happens on the write, so nothing downstream
    /// ever holds a guest number that could index past the slots.
    selected: Option<usize>,
    slots: Vec<Slot>,
    /// Slots the guest asked to eject, in the order it asked.
    ejects: Vec<usize>,
}

/// The memory hotplug register block.
pub struct MemHotplug {
    log: Logger,
    sink: Arc<dyn HotplugEventSink>,
    inner: Mutex<Inner>,
}

impl MemHotplug {
    /// Create a register block with `slots` memory slots.
    ///
    /// A count above [`MAX_SLOTS`] is capped, because the kernel could
    /// not back the extra slots. Read the count back with
    /// [`slots`](Self::slots) instead of assuming the asked-for value:
    /// the AML must describe the same number of slots as this block.
    pub fn new(
        slots: usize,
        sink: Arc<dyn HotplugEventSink>,
        log: Logger,
    ) -> Arc<Self> {
        let capped = slots.min(MAX_SLOTS);
        if capped != slots {
            slog::warn!(log, "memory hotplug: slot count capped";
                "asked" => slots, "slots" => capped, "limit" => MAX_SLOTS);
        }
        Arc::new(Self {
            log,
            sink,
            inner: Mutex::new(Inner {
                selected: None,
                slots: vec![Slot::default(); capped],
                ejects: Vec::new(),
            }),
        })
    }

    /// How many slots this block describes.
    pub fn slots(&self) -> usize {
        self.inner
            .lock()
            .expect("mem hotplug lock poisoned")
            .slots
            .len()
    }

    /// Put a memory region in `slot` and mark the slot enabled.
    ///
    /// This only fills the registers. Call
    /// [`notify_added`](Self::notify_added) after the region is really
    /// mapped, or the guest reads a `_CRS` for memory that is not
    /// there yet.
    pub fn set_slot(
        &self,
        slot: usize,
        base: u64,
        len: u64,
    ) -> Result<(), MemHotplugError> {
        if len == 0 {
            return Err(MemHotplugError::EmptyRegion { slot });
        }
        // The guest turns base and len into a QWordMemory descriptor
        // whose maximum is base + len - 1. A region that wraps would
        // give it a descriptor that covers the whole address space.
        if base.checked_add(len).is_none() {
            return Err(MemHotplugError::RegionWraps { slot, base, len });
        }

        let mut inner = self.inner.lock().expect("mem hotplug lock poisoned");
        let slots = inner.slots.len();
        let Some(entry) = inner.slots.get_mut(slot) else {
            return Err(MemHotplugError::NoSuchSlot { slot, slots });
        };
        entry.base = base;
        entry.len = len;
        entry.enabled = true;
        Ok(())
    }

    /// Raise an insert event for `slot` and take the SCI.
    ///
    /// The guest runs `_E03`, which scans every slot and issues a
    /// Device Check for the ones with the insert bit set.
    pub fn notify_added(&self, slot: usize) {
        {
            let mut inner =
                self.inner.lock().expect("mem hotplug lock poisoned");
            let slots = inner.slots.len();
            let Some(entry) = inner.slots.get_mut(slot) else {
                slog::warn!(self.log, "memory hotplug: no such slot";
                    "slot" => slot, "slots" => slots);
                return;
            };
            if !entry.enabled {
                slog::warn!(self.log,
                    "memory hotplug: slot has no region yet";
                    "slot" => slot);
                return;
            }
            entry.inserting = true;
        }
        // The sink takes its own lock. Raising outside ours means the
        // two are never held at the same time, in either order.
        self.sink.raise(GpeBit::Memory);
    }

    /// The `_OST` codes the guest reported for one slot.
    #[cfg(test)]
    fn ost(&self, slot: usize) -> Option<(u32, u32)> {
        let inner = self.inner.lock().expect("mem hotplug lock poisoned");
        inner
            .slots
            .get(slot)
            .map(|entry| (entry.ost_event, entry.ost_status))
    }

    /// Take the eject requests the guest has made since the last call.
    ///
    /// Removal is not possible on this kernel, so the caller reports
    /// the request and refuses it. See the module comment.
    pub fn take_eject_requests(&self) -> Vec<usize> {
        let mut inner = self.inner.lock().expect("mem hotplug lock poisoned");
        std::mem::take(&mut inner.ejects)
    }

    /// Handle one guest access to the register block.
    pub fn pio_rw(&self, offset: usize, rwo: RWOp<'_>) {
        match rwo {
            RWOp::Read(ro) => {
                let width = ro.len();
                // One write_u64 covers all eight buffer bytes, so a
                // refused read cannot hand back a stale byte.
                ro.write_u64(self.read(offset, width));
            }
            RWOp::Write(wo) => self.write(offset, wo.len(), wo.read_u32()),
        }
    }

    /// Value for a read of `width` bytes at `offset`.
    fn read(&self, offset: usize, width: usize) -> u64 {
        // ACPI reaches this block through 1, 2 and 4 byte accesses.
        // Anything else cannot fit a register, and the ABI says an
        // undocumented read is all ones.
        let Some((base, len)) = read_register_at(offset) else {
            return u64::MAX;
        };
        if !matches!(width, 1 | 2 | 4) {
            return u64::MAX;
        }
        // The guest controls offset and width.
        let end = match offset.checked_add(width) {
            Some(end) => end,
            None => return u64::MAX,
        };
        if end > base + len {
            return u64::MAX;
        }

        let inner = self.inner.lock().expect("mem hotplug lock poisoned");
        // A selector that names no slot reads zero across the block.
        // `get` is what keeps a hostile selector from indexing.
        let Some(slot) =
            inner.selected.and_then(|index| inner.slots.get(index))
        else {
            return 0;
        };

        let register: u32 = match base {
            REG_ADDR_LOW => low32(slot.base),
            REG_ADDR_HIGH => high32(slot.base),
            REG_SIZE_LOW => low32(slot.len),
            REG_SIZE_HIGH => high32(slot.len),
            // One NUMA node, so every slot is in domain 0.
            REG_PROXIMITY => 0,
            REG_STATUS => u32::from(slot.status()),
            _ => return u64::MAX,
        };

        // The check above puts offset - base below len, and no
        // register is wider than 4 bytes, so the shift stays inside
        // the register.
        let shift = (offset - base) * 8;
        u64::from((register >> shift) & width_mask(width))
    }

    /// Apply a write of `width` bytes at `offset`.
    fn write(&self, offset: usize, width: usize, value: u32) {
        let Some((base, len)) = write_register_at(offset) else {
            slog::trace!(self.log, "memory hotplug: write to a reserved byte";
                "offset" => offset, "width" => width);
            return;
        };
        // These registers are write only, so a partial write has no
        // old value to merge into. Dropping it is the only answer that
        // cannot leave half a slot number in the selector.
        if offset != base || width != len {
            slog::trace!(self.log, "memory hotplug: partial write dropped";
                "offset" => offset, "width" => width);
            return;
        }

        let mut inner = self.inner.lock().expect("mem hotplug lock poisoned");

        if base == REG_SELECTOR {
            let slots = inner.slots.len();
            let selected =
                usize::try_from(value).ok().filter(|index| *index < slots);
            if selected.is_none() {
                slog::debug!(self.log,
                    "memory hotplug: selector names no slot";
                    "selector" => value, "slots" => slots);
            }
            inner.selected = selected;
            return;
        }

        // Every other register needs a slot. A guest that selected
        // nothing gets no-ops, which is what the ABI asks for.
        let Some(index) = inner.selected else {
            return;
        };

        let eject = {
            let Some(slot) = inner.slots.get_mut(index) else {
                return;
            };
            match base {
                REG_OST_EVENT => {
                    slot.ost_event = value;
                    false
                }
                REG_OST_STATUS => {
                    slot.ost_status = value;
                    false
                }
                REG_CONTROL => control(slot, value),
                _ => false,
            }
        };

        // The slot count bounds this list, so a guest that runs _EJ0 in
        // a loop cannot grow it. The drain thread empties the list, so
        // the dedup holds only until its next pass. The guest can reach
        // this line without limit, thus it logs at debug level.
        if eject && !inner.ejects.contains(&index) {
            inner.ejects.push(index);
            slog::debug!(self.log,
                "memory hotplug: guest asked to eject a slot";
                "slot" => index);
        }
    }
}

/// Apply a control register write. Returns whether the guest asked to
/// eject the slot.
///
/// The bits are tested in the order QEMU tests them, so a guest that
/// sets more than one gets the same single action from both VMMs.
fn control(slot: &mut Slot, value: u32) -> bool {
    if value & CONTROL_CLEAR_INSERT != 0 {
        slot.inserting = false;
    } else if value & CONTROL_CLEAR_REMOVE != 0 {
        slot.removing = false;
    } else if value & CONTROL_EJECT != 0 {
        // An empty slot has nothing to give back.
        return slot.enabled;
    }
    false
}

/// The read register holding `offset`, as (first byte, length).
fn read_register_at(offset: usize) -> Option<(usize, usize)> {
    match offset {
        0x00..=0x03 => Some((REG_ADDR_LOW, 4)),
        0x04..=0x07 => Some((REG_ADDR_HIGH, 4)),
        0x08..=0x0B => Some((REG_SIZE_LOW, 4)),
        0x0C..=0x0F => Some((REG_SIZE_HIGH, 4)),
        0x10..=0x13 => Some((REG_PROXIMITY, 4)),
        0x14 => Some((REG_STATUS, 1)),
        // 0x15 to 0x17 are reserved, and so is everything past the
        // block.
        _ => None,
    }
}

/// The write register holding `offset`, as (first byte, length).
fn write_register_at(offset: usize) -> Option<(usize, usize)> {
    match offset {
        0x00..=0x03 => Some((REG_SELECTOR, 4)),
        0x04..=0x07 => Some((REG_OST_EVENT, 4)),
        0x08..=0x0B => Some((REG_OST_STATUS, 4)),
        0x14 => Some((REG_CONTROL, 1)),
        // 0x0C to 0x13 and 0x15 to 0x17 are reserved on write.
        _ => None,
    }
}

/// Mask of the bytes a read of `width` bytes returns.
const fn width_mask(width: usize) -> u32 {
    match width {
        1 => 0xFF,
        2 => 0xFFFF,
        _ => u32::MAX,
    }
}

const fn low32(value: u64) -> u32 {
    value as u32
}

const fn high32(value: u64) -> u32 {
    (value >> 32) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    use slog::Drain as _;
    use vmm_core::common::{ReadOp, WriteOp};

    /// A slot window well above the 4 GiB line, so both halves of the
    /// base and of the length carry bits.
    const TEST_BASE: u64 = 0x0000_0004_8000_0000;
    const TEST_LEN: u64 = 0x0000_0002_4000_0000;

    /// Records every general purpose event the block raises.
    struct RecordingSink {
        raised: Mutex<Vec<GpeBit>>,
    }

    impl RecordingSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                raised: Mutex::new(Vec::new()),
            })
        }

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

    /// Counts the records a block emits, so a test can show that a
    /// path a guest drives in a loop does not log in that loop.
    struct CountingDrain(Arc<AtomicUsize>);

    impl slog::Drain for CountingDrain {
        type Ok = ();
        type Err = slog::Never;

        fn log(
            &self,
            record: &slog::Record<'_>,
            _values: &slog::OwnedKVList,
        ) -> Result<(), Self::Err> {
            if record.level().is_at_least(slog::Level::Info) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }
    }

    fn counting_block(
        slots: usize,
    ) -> (Arc<MemHotplug>, Arc<RecordingSink>, Arc<AtomicUsize>) {
        let sink = RecordingSink::new();
        let as_sink: Arc<dyn HotplugEventSink> = sink.clone();
        let count = Arc::new(AtomicUsize::new(0));
        let log = Logger::root(CountingDrain(count.clone()).fuse(), slog::o!());
        (MemHotplug::new(slots, as_sink, log), sink, count)
    }

    fn block(slots: usize) -> (Arc<MemHotplug>, Arc<RecordingSink>) {
        let sink = RecordingSink::new();
        let as_sink: Arc<dyn HotplugEventSink> = sink.clone();
        (MemHotplug::new(slots, as_sink, test_log()), sink)
    }

    /// A block with every slot, slot 0 holding the test window.
    fn with_slot_zero() -> (Arc<MemHotplug>, Arc<RecordingSink>) {
        let (mem, sink) = block(MAX_SLOTS);
        mem.set_slot(0, TEST_BASE, TEST_LEN).expect("slot 0 exists");
        (mem, sink)
    }

    fn read(mem: &MemHotplug, offset: usize, width: usize) -> u64 {
        let mut ro = ReadOp::new(width);
        mem.pio_rw(offset, RWOp::Read(&mut ro));
        let mut bytes = [0u8; 8];
        bytes[..width].copy_from_slice(ro.buf());
        u64::from_le_bytes(bytes)
    }

    /// Read from a buffer that already holds a pattern, so a handler
    /// that answers by leaving the buffer alone is visible. `ReadOp`
    /// arrives zeroed, so a plain `read` cannot tell the two apart.
    fn read_poisoned(mem: &MemHotplug, offset: usize, width: usize) -> u64 {
        let mut ro = ReadOp::new(width);
        ro.write_u64(0xA5A5_A5A5_A5A5_A5A5);
        mem.pio_rw(offset, RWOp::Read(&mut ro));
        let mut bytes = [0u8; 8];
        bytes[..width].copy_from_slice(ro.buf());
        u64::from_le_bytes(bytes)
    }

    fn write_bytes(mem: &MemHotplug, offset: usize, bytes: &[u8]) {
        let wo = WriteOp::from_buf(bytes);
        mem.pio_rw(offset, RWOp::Write(&wo));
    }

    fn write(mem: &MemHotplug, offset: usize, width: usize, value: u32) {
        write_bytes(mem, offset, &value.to_le_bytes()[..width]);
    }

    fn select(mem: &MemHotplug, slot: u32) {
        write(mem, REG_SELECTOR, 4, slot);
    }

    /// What a refused access of `width` bytes reads: all ones, in the
    /// bytes the bus takes back.
    fn all_ones(width: usize) -> u64 {
        let mut bytes = [0xFFu8; 8];
        bytes[width..].fill(0);
        u64::from_le_bytes(bytes)
    }

    #[test]
    fn new_caps_the_slot_count_at_the_kernel_limit() {
        let (mem, _sink) = block(64);
        assert_eq!(mem.slots(), MAX_SLOTS);

        let (mem, _sink) = block(2);
        assert_eq!(mem.slots(), 2);

        let (mem, _sink) = block(0);
        assert_eq!(mem.slots(), 0);
    }

    #[test]
    fn set_slot_rejects_a_slot_past_the_cap() {
        let (mem, _sink) = block(64);

        assert_eq!(
            mem.set_slot(MAX_SLOTS, TEST_BASE, TEST_LEN),
            Err(MemHotplugError::NoSuchSlot {
                slot: MAX_SLOTS,
                slots: MAX_SLOTS,
            }),
        );
        assert_eq!(
            mem.set_slot(usize::MAX, TEST_BASE, TEST_LEN),
            Err(MemHotplugError::NoSuchSlot {
                slot: usize::MAX,
                slots: MAX_SLOTS,
            }),
        );
        assert!(mem.set_slot(MAX_SLOTS - 1, TEST_BASE, TEST_LEN).is_ok());
    }

    #[test]
    fn set_slot_rejects_a_region_the_guest_cannot_describe() {
        let (mem, _sink) = block(MAX_SLOTS);

        assert_eq!(
            mem.set_slot(0, TEST_BASE, 0),
            Err(MemHotplugError::EmptyRegion { slot: 0 }),
        );
        // base + len - 1 is the maximum in the _CRS descriptor, so a
        // region that runs off the end of the address space would give
        // the guest a window that covers everything.
        assert_eq!(
            mem.set_slot(0, u64::MAX, 2),
            Err(MemHotplugError::RegionWraps {
                slot: 0,
                base: u64::MAX,
                len: 2,
            }),
        );
        assert!(mem.set_slot(0, u64::MAX - 1, 1).is_ok());
    }

    #[test]
    fn a_base_and_length_round_trip_through_the_registers() {
        let (mem, _sink) = block(MAX_SLOTS);
        mem.set_slot(3, TEST_BASE, TEST_LEN).expect("slot 3 exists");
        select(&mem, 3);

        assert_eq!(read(&mem, REG_ADDR_LOW, 4), u64::from(low32(TEST_BASE)));
        assert_eq!(read(&mem, REG_ADDR_HIGH, 4), u64::from(high32(TEST_BASE)));
        assert_eq!(read(&mem, REG_SIZE_LOW, 4), u64::from(low32(TEST_LEN)));
        assert_eq!(read(&mem, REG_SIZE_HIGH, 4), u64::from(high32(TEST_LEN)));
        // One NUMA node.
        assert_eq!(read(&mem, REG_PROXIMITY, 4), 0);
        assert_eq!(read(&mem, REG_STATUS, 1), u64::from(STATUS_ENABLED));

        // A narrow read takes its bytes from inside the register.
        let low = low32(TEST_BASE).to_le_bytes();
        assert_eq!(read(&mem, REG_ADDR_LOW + 1, 1), u64::from(low[1]));
        assert_eq!(
            read(&mem, REG_ADDR_LOW + 2, 2),
            u64::from(u16::from_le_bytes([low[2], low[3]])),
        );

        // Another slot is still empty, so nothing leaks between slots.
        select(&mem, 4);
        assert_eq!(read(&mem, REG_ADDR_LOW, 4), 0);
        assert_eq!(read(&mem, REG_STATUS, 1), 0);
    }

    #[test]
    fn an_out_of_range_selector_reads_zero_and_drops_writes() {
        let (mem, _sink) = with_slot_zero();
        mem.notify_added(0);

        select(&mem, u32::MAX);

        for offset in [
            REG_ADDR_LOW,
            REG_ADDR_HIGH,
            REG_SIZE_LOW,
            REG_SIZE_HIGH,
            REG_PROXIMITY,
        ] {
            assert_eq!(read(&mem, offset, 4), 0, "offset {offset:#x}");
        }
        assert_eq!(read(&mem, REG_STATUS, 1), 0);

        // Every write that needs a slot is a no-op.
        write(&mem, REG_CONTROL, 1, CONTROL_CLEAR_INSERT);
        write(&mem, REG_CONTROL, 1, CONTROL_EJECT);
        write(&mem, REG_OST_EVENT, 4, 3);
        assert!(mem.take_eject_requests().is_empty());

        // Slot 0 kept the state it had before the bad selector.
        select(&mem, 0);
        assert_eq!(
            read(&mem, REG_STATUS, 1),
            u64::from(STATUS_ENABLED | STATUS_INSERTING),
        );
        assert_eq!(mem.ost(0), Some((0, 0)));
    }

    #[test]
    fn a_selector_past_the_configured_slots_is_out_of_range() {
        let (mem, _sink) = block(2);
        mem.set_slot(1, TEST_BASE, TEST_LEN).expect("slot 1 exists");

        select(&mem, 1);
        assert_eq!(read(&mem, REG_ADDR_LOW, 4), u64::from(low32(TEST_BASE)));

        // Slot 2 exists in no VM this block describes, even though it
        // is below MAX_SLOTS.
        select(&mem, 2);
        assert_eq!(read(&mem, REG_ADDR_LOW, 4), 0);
    }

    #[test]
    fn notify_added_raises_the_memory_gpe_bit() {
        let (mem, sink) = block(MAX_SLOTS);
        mem.set_slot(1, TEST_BASE, TEST_LEN).expect("slot 1 exists");

        mem.notify_added(1);

        assert_eq!(sink.raised(), vec![GpeBit::Memory]);
        select(&mem, 1);
        assert_eq!(
            read(&mem, REG_STATUS, 1),
            u64::from(STATUS_ENABLED | STATUS_INSERTING),
        );
    }

    #[test]
    fn notify_added_ignores_a_slot_with_nothing_behind_it() {
        let (mem, sink) = block(MAX_SLOTS);

        // Never given a region.
        mem.notify_added(0);
        // Past the last slot.
        mem.notify_added(MAX_SLOTS);
        mem.notify_added(usize::MAX);

        assert!(sink.raised().is_empty());
        select(&mem, 0);
        assert_eq!(read(&mem, REG_STATUS, 1), 0);
    }

    #[test]
    fn the_guest_clears_the_insert_event() {
        let (mem, _sink) = with_slot_zero();
        mem.notify_added(0);
        select(&mem, 0);

        write(&mem, REG_CONTROL, 1, CONTROL_CLEAR_INSERT);

        assert_eq!(read(&mem, REG_STATUS, 1), u64::from(STATUS_ENABLED));
    }

    #[test]
    fn the_remove_event_is_never_set() {
        let (mem, _sink) = with_slot_zero();
        mem.notify_added(0);
        select(&mem, 0);

        // This kernel cannot free a memory segment, so nothing in this
        // VMM raises a remove event. Clearing it is still accepted.
        write(&mem, REG_CONTROL, 1, CONTROL_CLEAR_REMOVE);

        let status = read(&mem, REG_STATUS, 1);
        assert_eq!(status & u64::from(STATUS_REMOVING), 0);
        assert_eq!(status & u64::from(STATUS_INSERTING), 0x02);
    }

    #[test]
    fn an_eject_request_is_recorded_once_and_tears_nothing_down() {
        let (mem, _sink) = block(MAX_SLOTS);
        mem.set_slot(2, TEST_BASE, TEST_LEN).expect("slot 2 exists");
        select(&mem, 2);

        // A guest that spams _EJ0 must not grow the request list.
        for _ in 0..64 {
            write(&mem, REG_CONTROL, 1, CONTROL_EJECT);
        }

        assert_eq!(mem.take_eject_requests(), vec![2]);
        assert!(mem.take_eject_requests().is_empty());
        // The slot still holds its memory. The caller refuses the
        // request.
        assert_eq!(read(&mem, REG_STATUS, 1), u64::from(STATUS_ENABLED));
        assert_eq!(read(&mem, REG_ADDR_LOW, 4), u64::from(low32(TEST_BASE)));
    }

    #[test]
    fn an_eject_of_an_empty_slot_is_ignored() {
        let (mem, _sink) = block(MAX_SLOTS);
        select(&mem, 5);

        write(&mem, REG_CONTROL, 1, CONTROL_EJECT);

        assert!(mem.take_eject_requests().is_empty());
    }

    #[test]
    fn the_guest_reports_its_ost_codes() {
        let (mem, _sink) = with_slot_zero();
        select(&mem, 0);

        write(&mem, REG_OST_EVENT, 4, 3);
        write(&mem, REG_OST_STATUS, 4, 0x8100_0002);

        assert_eq!(mem.ost(0), Some((3, 0x8100_0002)));
        assert_eq!(mem.ost(1), Some((0, 0)));
    }

    #[test]
    fn a_bad_width_reads_all_ones_and_writes_nothing() {
        let (mem, _sink) = with_slot_zero();
        select(&mem, 0);

        // The block decodes 1, 2 and 4 byte accesses only.
        assert_eq!(read(&mem, REG_ADDR_LOW, 8), all_ones(8));
        assert_eq!(read(&mem, REG_STATUS, 8), all_ones(8));

        // An eight byte write cannot be split across two registers.
        write_bytes(&mem, REG_SELECTOR, &[0xFF; 8]);
        assert_eq!(read(&mem, REG_ADDR_LOW, 4), u64::from(low32(TEST_BASE)));
    }

    #[test]
    fn a_reserved_offset_reads_all_ones() {
        let (mem, _sink) = with_slot_zero();
        select(&mem, 0);

        for offset in [0x15, 0x16, 0x17, 0x18, 0xFFFF, usize::MAX] {
            assert_eq!(
                read(&mem, offset, 1),
                all_ones(1),
                "offset {offset:#x}"
            );
        }
        // An access that starts inside a register but leaves it.
        assert_eq!(read(&mem, 0x02, 4), all_ones(4));
        assert_eq!(read(&mem, REG_STATUS, 2), all_ones(2));
    }

    #[test]
    fn a_partial_or_reserved_write_is_dropped() {
        let (mem, _sink) = with_slot_zero();
        mem.set_slot(1, TEST_BASE, TEST_LEN).expect("slot 1 exists");
        select(&mem, 1);

        // Half of a selector cannot be merged with anything, because
        // the register is write only.
        write(&mem, REG_SELECTOR, 1, 0);
        write(&mem, REG_SELECTOR + 1, 1, 0);
        write(&mem, REG_SELECTOR, 2, 0);
        // Reserved on write.
        write(&mem, 0x0C, 4, 0xFFFF_FFFF);
        write(&mem, 0x10, 4, 0xFFFF_FFFF);
        write(&mem, 0x15, 1, 0xFF);
        // A wide write to the status byte.
        write(&mem, REG_CONTROL, 4, CONTROL_EJECT);

        // Slot 1 is still selected, still enabled, and never ejected.
        assert_eq!(read(&mem, REG_STATUS, 1), u64::from(STATUS_ENABLED));
        assert!(mem.take_eject_requests().is_empty());
    }

    #[test]
    fn a_block_with_no_slots_answers_every_access_safely() {
        let (mem, _sink) = block(0);

        select(&mem, 0);
        assert_eq!(read(&mem, REG_ADDR_LOW, 4), 0);
        assert_eq!(read(&mem, REG_STATUS, 1), 0);
        write(&mem, REG_CONTROL, 1, CONTROL_EJECT);
        assert!(mem.take_eject_requests().is_empty());
    }

    #[test]
    fn the_status_byte_reports_each_bit_in_its_abi_place() {
        assert_eq!(STATUS_ENABLED, 1);
        assert_eq!(STATUS_INSERTING, 2);
        assert_eq!(STATUS_REMOVING, 4);

        let slot = Slot {
            enabled: true,
            inserting: true,
            removing: true,
            ..Slot::default()
        };
        assert_eq!(slot.status(), 0x07);
        assert_eq!(Slot::default().status(), 0);
    }

    /// A guest that runs `_EJ0` in a loop must not be able to make the
    /// VMM write a log record per iteration.
    #[test]
    fn a_repeated_eject_does_not_flood_the_log() {
        let (mem, sink, records) = counting_block(MAX_SLOTS);
        mem.set_slot(0, TEST_BASE, TEST_LEN).expect("slot 0 exists");
        drop(sink);

        select(&mem, 0);
        for _ in 0..1000 {
            write(&mem, REG_CONTROL, 1, CONTROL_EJECT);
        }

        assert_eq!(mem.take_eject_requests(), vec![0], "one request only");
        assert!(
            records.load(Ordering::Relaxed) <= 1,
            "1000 ejects wrote {} records",
            records.load(Ordering::Relaxed)
        );
    }

    /// Every access must overwrite the buffer. The exit structure is
    /// reused, so a handler that returns without writing hands the
    /// guest whatever the last exit left there.
    #[test]
    fn no_read_can_return_a_stale_buffer() {
        let (mem, _sink) = with_slot_zero();
        let poison = 0xA5A5_A5A5_A5A5_A5A5u64;

        // Past the block, inside the reserved bytes, and at every
        // defined register, at every width the bus can present.
        for offset in 0..0x40usize {
            for width in [1usize, 2, 4, 8] {
                let mut bytes = [0u8; 8];
                bytes[..width].copy_from_slice(&poison.to_le_bytes()[..width]);
                let stale = u64::from_le_bytes(bytes);
                assert_ne!(
                    read_poisoned(&mem, offset, width),
                    stale,
                    "offset {offset} width {width} leaked a stale buffer",
                );
            }
        }

        // And with no slot selected, which is the path that returns
        // early before it reaches a register.
        write(&mem, REG_SELECTOR, 4, u32::MAX);
        for offset in 0..0x40usize {
            for width in [1usize, 2, 4, 8] {
                let mut bytes = [0u8; 8];
                bytes[..width].copy_from_slice(&poison.to_le_bytes()[..width]);
                assert_ne!(
                    read_poisoned(&mem, offset, width),
                    u64::from_le_bytes(bytes),
                    "a bad selector leaked at offset {offset} width {width}",
                );
            }
        }
    }
}
