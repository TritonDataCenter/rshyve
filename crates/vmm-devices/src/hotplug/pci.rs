// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PCI slot hotplug register file.
//!
//! A 12 byte I/O block at [`PCI_HOTPLUG_ADDR`] holding three 32 bit
//! registers, one bit per PCI slot. The layout is the QEMU guest ABI
//! (QEMU `docs/specs/acpi_pci_hotplug.rst`) without the removability
//! register, and the AML in [`crate::acpi::hotplug::pci`] is written
//! against it:
//!
//! | Offset | Name | Direction | Meaning                        |
//! |--------|------|-----------|--------------------------------|
//! | 0      | PCIU | read      | slots that came up, read clears|
//! | 4      | PCID | read      | slots to remove, read clears   |
//! | 8      | B0EJ | write     | slots the guest wants ejected  |
//!
//! Read clears because the register is an event queue, not a state
//! word. The guest's `_E01` handler reads each register once per SCI
//! and turns the bits into `Notify` calls. A bit that survives the read
//! notifies the guest a second time on the next event.
//!
//! This tree has one flat i440fx bus, so there is no segment select
//! register. cloud-hypervisor puts `PSEG` at offset 12 for its
//! multi-segment topology.

use std::sync::{Arc, Mutex};

use slog;
use vmm_core::common::RWOp;
use vmm_core::pio::PioFn;

use crate::acpi_gpe::{GpeBit, HotplugEventSink};

/// Base I/O port of the PCI hotplug register file.
pub const PCI_HOTPLUG_ADDR: u16 = 0xAE00;

/// Length of the register file in bytes.
pub const PCI_HOTPLUG_LEN: u8 = 12;

/// Slots on the bus. One bit per slot fills a 32 bit register exactly.
pub const PCI_SLOTS: u8 = 32;

/// First slot a guest may hot-add to or eject.
///
/// Slot 0 is the host bridge and slot 1 is the LPC bridge. A guest that
/// could eject either would take out its own chipset, so both are
/// refused here and get no `_EJ0` method in the AML.
pub const FIRST_HOTPLUG_SLOT: u8 = 2;

/// Width of every register in the block.
const REGISTER_WIDTH: usize = 4;

/// Offset of PCIU, the slot-up bitmap.
const PCIU_OFFSET: usize = 0;
/// Offset of PCID, the slot-down bitmap.
const PCID_OFFSET: usize = 4;
/// Offset of B0EJ, the eject request bitmap.
const B0EJ_OFFSET: usize = 8;

const _: () = assert!(
    B0EJ_OFFSET + REGISTER_WIDTH == PCI_HOTPLUG_LEN as usize,
    "the register block must end at the last register",
);

/// Whether the guest may hot-add to or eject from `slot`.
pub const fn is_hotpluggable_slot(slot: u8) -> bool {
    slot >= FIRST_HOTPLUG_SLOT && slot < PCI_SLOTS
}

/// The slot's bit in PCIU, PCID and B0EJ.
///
/// `None` for a slot off the bus or for the two chipset slots, which
/// keeps every shift below inside a `u32`.
const fn slot_mask(slot: u8) -> Option<u32> {
    if !is_hotpluggable_slot(slot) {
        return None;
    }
    Some(1u32 << slot)
}

/// The PCI slot hotplug register file.
pub struct PciHotplug {
    log: slog::Logger,
    sink: Arc<dyn HotplugEventSink>,
    inner: Mutex<PciHotplugInner>,
}

#[derive(Default)]
struct PciHotplugInner {
    /// PCIU: slots the VMM filled since the guest last read.
    up: u32,
    /// PCID: slots the VMM asks the guest to give back.
    down: u32,
    /// Slots that hold a device the guest is allowed to eject.
    ///
    /// An eject for any other slot is dropped. Without this a guest
    /// could queue teardown for a slot the VMM never populated.
    ejectable: u32,
    /// Slots the guest asked to eject and nothing has drained yet.
    ///
    /// A bitmap, not a queue: a guest can write B0EJ as often as it
    /// likes, and a queue would grow without bound.
    eject_pending: u32,
}

impl PciHotplug {
    /// Create the register file.
    ///
    /// `sink` is the GPE0 block. This does not claim the port. The
    /// caller attaches [`PciHotplug::pio_handler`] to the PIO bus.
    pub fn new(
        sink: Arc<dyn HotplugEventSink>,
        log: slog::Logger,
    ) -> Arc<Self> {
        Arc::new(Self {
            log,
            sink,
            inner: Mutex::new(PciHotplugInner::default()),
        })
    }

    /// A PIO bus handler for the block.
    pub fn pio_handler(self: &Arc<Self>) -> Arc<PioFn> {
        let device = Arc::clone(self);
        Arc::new(move |offset: u16, rwo: RWOp<'_>| {
            device.pio_rw(usize::from(offset), rwo);
        })
    }

    /// Report that `slot` now holds a device.
    ///
    /// Sets the slot's PCIU bit and raises the PCI general purpose
    /// event. The guest answers with a device check on that slot.
    pub fn notify_added(&self, slot: u8) {
        let Some(mask) = slot_mask(slot) else {
            slog::warn!(self.log, "refused a hot-add of a fixed PCI slot";
                "slot" => slot);
            return;
        };

        {
            let mut inner = self.lock();
            inner.up |= mask;
            // The slot is filled, so a pending request to give it back
            // no longer describes anything.
            inner.down &= !mask;
            inner.ejectable |= mask;
        }

        // The sink takes the GPE0 lock and drives the SCI pin. Raising
        // it outside `inner` keeps the two locks from ever nesting.
        self.sink.raise(GpeBit::Pci);
    }

    /// Ask the guest to give up the device in `slot`.
    ///
    /// Sets the slot's PCID bit and raises the PCI general purpose
    /// event. The guest answers with an eject request, unbinds its
    /// driver, and runs `_EJ0`, which writes B0EJ. The slot stays
    /// ejectable until then, because the device is still there.
    pub fn notify_removed(&self, slot: u8) {
        let Some(mask) = slot_mask(slot) else {
            slog::warn!(self.log, "refused a hot-remove of a fixed PCI slot";
                "slot" => slot);
            return;
        };

        {
            let mut inner = self.lock();
            inner.down |= mask;
            inner.up &= !mask;
        }

        self.sink.raise(GpeBit::Pci);
    }

    /// Take the slots the guest asked to eject.
    ///
    /// The caller owns teardown for every slot returned. Each slot
    /// stops being ejectable here, so a guest cannot queue a second
    /// teardown for a slot whose first one is still running.
    pub fn take_eject_requests(&self) -> Vec<u8> {
        let pending = {
            let mut inner = self.lock();
            let pending = std::mem::take(&mut inner.eject_pending);
            inner.ejectable &= !pending;
            pending
        };

        let mut slots = Vec::with_capacity(pending.count_ones() as usize);
        let mut remaining = pending;
        while remaining != 0 {
            // `trailing_zeros` of a non-zero u32 is 0..=31, so the slot
            // number fits a u8 and the shift stays in the register.
            let slot = remaining.trailing_zeros();
            remaining &= !(1u32 << slot);
            slots.push(slot as u8);
        }
        slots
    }

    /// Handle one guest access to the register file.
    ///
    /// Every refusal below logs at debug. A guest picks the offset and
    /// the width, so it can drive these paths as fast as it can trap,
    /// and a louder level would let it flood the log.
    pub fn pio_rw(&self, offset: usize, rwo: RWOp<'_>) {
        match rwo {
            RWOp::Read(ro) => {
                // Answer zero before deciding anything. A refused read
                // that left the buffer alone would hand the guest
                // whatever the exit structure held.
                ro.write_u64(0);

                if ro.len() != REGISTER_WIDTH {
                    slog::debug!(self.log, "refused a PCI hotplug read";
                        "offset" => offset, "width" => ro.len());
                    return;
                }

                match offset {
                    PCIU_OFFSET => {
                        let mut inner = self.lock();
                        ro.write_u32(inner.up);
                        inner.up = 0;
                    }
                    PCID_OFFSET => {
                        let mut inner = self.lock();
                        ro.write_u32(inner.down);
                        inner.down = 0;
                    }
                    // B0EJ answers zero: the eject is a request the
                    // guest writes, and nothing reads back from it.
                    B0EJ_OFFSET => {}
                    _ => {
                        slog::debug!(self.log,
                            "refused a PCI hotplug read at a bad offset";
                            "offset" => offset);
                    }
                }
            }
            RWOp::Write(wo) => {
                if wo.len() != REGISTER_WIDTH {
                    slog::debug!(self.log, "refused a PCI hotplug write";
                        "offset" => offset, "width" => wo.len());
                    return;
                }

                match offset {
                    B0EJ_OFFSET => self.request_ejects(wo.read_u32()),
                    PCIU_OFFSET | PCID_OFFSET => {
                        slog::debug!(self.log,
                            "dropped a write to a read-only register";
                            "offset" => offset);
                    }
                    _ => {
                        slog::debug!(self.log,
                            "refused a PCI hotplug write at a bad offset";
                            "offset" => offset);
                    }
                }
            }
        }
    }

    /// Record every slot in `bitmap` the guest is allowed to eject.
    ///
    /// This runs on a vCPU thread inside a PIO exit, so it only
    /// records. Teardown needs the PCI config lock, which another vCPU
    /// can already hold, and it can block on a backing file. Both would
    /// stall the guest inside an `_EJ0` method.
    fn request_ejects(&self, bitmap: u32) {
        let mut remaining = bitmap;
        let mut accepted = 0u32;
        let mut refused = 0u32;

        {
            let mut inner = self.lock();
            while remaining != 0 {
                // `trailing_zeros` of a non-zero u32 is 0..=31, so the
                // slot is inherently on the bus and indexes nothing.
                let slot = remaining.trailing_zeros();
                let mask = 1u32 << slot;
                remaining &= !mask;

                if slot < u32::from(FIRST_HOTPLUG_SLOT)
                    || inner.ejectable & mask == 0
                {
                    refused |= mask;
                    continue;
                }

                inner.eject_pending |= mask;
                accepted |= mask;
            }
        }

        if refused != 0 {
            slog::debug!(self.log, "refused a PCI eject request";
                "slots" => format!("{refused:#010x}"));
        }
        if accepted != 0 {
            slog::debug!(self.log, "queued a PCI eject request";
                "slots" => format!("{accepted:#010x}"));
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PciHotplugInner> {
        self.inner.lock().expect("pci hotplug lock poisoned")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use slog::Drain as _;

    use super::*;
    use vmm_core::common::{ReadOp, WriteOp};

    /// Records every event raised, so a test can assert on the SCI
    /// source without building a GPE0 block and an interrupt pin.
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

    /// Counts the records the register file emits, so a test can show
    /// that a path a guest drives in a loop does not log in that loop.
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

    fn counting_device() -> (Arc<PciHotplug>, Arc<AtomicUsize>) {
        let sink = Arc::new(RecordingSink::default());
        let count = Arc::new(AtomicUsize::new(0));
        let log =
            slog::Logger::root(CountingDrain(count.clone()).fuse(), slog::o!());
        (PciHotplug::new(sink, log), count)
    }

    fn device() -> (Arc<PciHotplug>, Arc<RecordingSink>) {
        let sink = Arc::new(RecordingSink::default());
        let log = slog::Logger::root(slog::Discard, slog::o!());
        (PciHotplug::new(sink.clone(), log), sink)
    }

    /// Read `width` bytes at `offset`, starting from a poisoned buffer
    /// so a handler that never writes is visible.
    fn read(hp: &PciHotplug, offset: usize, width: usize) -> [u8; 8] {
        let mut ro = ReadOp::new(width);
        ro.write_u64(u64::MAX);
        hp.pio_rw(offset, RWOp::Read(&mut ro));
        let mut out = [0u8; 8];
        out[..width].copy_from_slice(ro.buf());
        out
    }

    fn read_u32(hp: &PciHotplug, offset: usize) -> u32 {
        let buf = read(hp, offset, 4);
        u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])
    }

    fn write_u32(hp: &PciHotplug, offset: usize, value: u32) {
        let wo = WriteOp::from_buf(&value.to_le_bytes());
        hp.pio_rw(offset, RWOp::Write(&wo));
    }

    #[test]
    fn reading_pciu_clears_it() {
        let (hp, _sink) = device();
        hp.notify_added(4);

        assert_eq!(read_u32(&hp, PCIU_OFFSET), 1 << 4);
        assert_eq!(read_u32(&hp, PCIU_OFFSET), 0);
    }

    #[test]
    fn reading_pcid_clears_it() {
        let (hp, _sink) = device();
        hp.notify_added(7);
        hp.notify_removed(7);

        assert_eq!(read_u32(&hp, PCID_OFFSET), 1 << 7);
        assert_eq!(read_u32(&hp, PCID_OFFSET), 0);
    }

    #[test]
    fn reading_one_register_does_not_clear_the_other() {
        let (hp, _sink) = device();
        hp.notify_added(5);
        hp.notify_added(6);
        hp.notify_removed(6);

        assert_eq!(read_u32(&hp, PCID_OFFSET), 1 << 6);
        assert_eq!(read_u32(&hp, PCIU_OFFSET), 1 << 5);
    }

    #[test]
    fn a_hot_add_clears_a_pending_removal_of_the_same_slot() {
        let (hp, _sink) = device();
        hp.notify_added(9);
        hp.notify_removed(9);
        hp.notify_added(9);

        assert_eq!(read_u32(&hp, PCID_OFFSET), 0);
        assert_eq!(read_u32(&hp, PCIU_OFFSET), 1 << 9);
    }

    #[test]
    fn b0ej_reads_as_zero() {
        let (hp, _sink) = device();
        hp.notify_added(3);
        write_u32(&hp, B0EJ_OFFSET, 1 << 3);

        assert_eq!(read_u32(&hp, B0EJ_OFFSET), 0);
        // The read must not be a drain either.
        assert_eq!(hp.take_eject_requests(), vec![3]);
    }

    #[test]
    fn b0ej_records_only_slots_that_hold_a_device() {
        let (hp, _sink) = device();
        hp.notify_added(6);

        // Slot 6 was filled, slot 7 never was.
        write_u32(&hp, B0EJ_OFFSET, (1 << 6) | (1 << 7));

        assert_eq!(hp.take_eject_requests(), vec![6]);
    }

    #[test]
    fn b0ej_ignores_the_chipset_slots() {
        let (hp, _sink) = device();

        write_u32(&hp, B0EJ_OFFSET, 0xFFFF_FFFF);

        // Slot 0 is the host bridge and slot 1 is the LPC bridge, and
        // nothing else was ever filled.
        assert!(hp.take_eject_requests().is_empty());
    }

    #[test]
    fn b0ej_yields_every_slot_exactly_once() {
        let (hp, _sink) = device();
        let slots: Vec<u8> = vec![2, 3, 15, 30, 31];
        let mut bitmap = 0u32;
        for slot in &slots {
            hp.notify_added(*slot);
            bitmap |= 1u32 << *slot;
        }

        // A guest can write the same request as often as it likes.
        write_u32(&hp, B0EJ_OFFSET, bitmap);
        write_u32(&hp, B0EJ_OFFSET, bitmap);

        assert_eq!(hp.take_eject_requests(), slots);
        assert!(hp.take_eject_requests().is_empty());
    }

    #[test]
    fn a_drained_slot_cannot_be_ejected_twice() {
        let (hp, _sink) = device();
        hp.notify_added(12);
        write_u32(&hp, B0EJ_OFFSET, 1 << 12);
        assert_eq!(hp.take_eject_requests(), vec![12]);

        // Teardown is running. A second request must not queue it again.
        write_u32(&hp, B0EJ_OFFSET, 1 << 12);

        assert!(hp.take_eject_requests().is_empty());
    }

    #[test]
    fn a_zero_write_to_b0ej_is_a_noop() {
        let (hp, _sink) = device();
        hp.notify_added(8);

        write_u32(&hp, B0EJ_OFFSET, 0);

        assert!(hp.take_eject_requests().is_empty());
    }

    #[test]
    fn writes_to_the_read_only_registers_are_dropped() {
        let (hp, _sink) = device();
        hp.notify_added(4);

        write_u32(&hp, PCIU_OFFSET, 0xFFFF_FFFF);
        write_u32(&hp, PCID_OFFSET, 0xFFFF_FFFF);

        assert_eq!(read_u32(&hp, PCIU_OFFSET), 1 << 4);
        assert_eq!(read_u32(&hp, PCID_OFFSET), 0);
    }

    #[test]
    fn a_bad_width_read_leaves_a_zeroed_buffer() {
        let (hp, _sink) = device();
        hp.notify_added(4);

        for width in [1usize, 2, 8] {
            assert_eq!(
                read(&hp, PCIU_OFFSET, width),
                [0u8; 8],
                "width {width} leaked buffer content",
            );
        }
        // The refused reads must not have drained PCIU.
        assert_eq!(read_u32(&hp, PCIU_OFFSET), 1 << 4);
    }

    #[test]
    fn an_unaligned_read_leaves_a_zeroed_buffer() {
        let (hp, _sink) = device();
        hp.notify_added(4);

        for offset in [1usize, 2, 3, 5, 9, 11, usize::MAX] {
            assert_eq!(
                read(&hp, offset, 4),
                [0u8; 8],
                "offset {offset} leaked buffer content",
            );
        }
        assert_eq!(read_u32(&hp, PCIU_OFFSET), 1 << 4);
    }

    #[test]
    fn a_bad_width_or_offset_write_is_dropped() {
        let (hp, _sink) = device();
        hp.notify_added(4);

        for width in [1usize, 2, 8] {
            let wo = WriteOp::from_buf(&[0xFFu8; 8][..width]);
            hp.pio_rw(B0EJ_OFFSET, RWOp::Write(&wo));
        }
        for offset in [9usize, 10, 11, usize::MAX] {
            write_u32(&hp, offset, 1 << 4);
        }

        assert!(hp.take_eject_requests().is_empty());
    }

    #[test]
    fn notify_added_raises_the_gpe_bit() {
        let (hp, sink) = device();

        hp.notify_added(2);

        assert_eq!(sink.raised(), vec![GpeBit::Pci]);
    }

    #[test]
    fn notify_removed_raises_the_gpe_bit() {
        let (hp, sink) = device();

        hp.notify_removed(2);

        assert_eq!(sink.raised(), vec![GpeBit::Pci]);
    }

    #[test]
    fn a_fixed_slot_raises_nothing() {
        let (hp, sink) = device();

        for slot in [0u8, 1, PCI_SLOTS, u8::MAX] {
            hp.notify_added(slot);
            hp.notify_removed(slot);
        }

        assert!(sink.raised().is_empty());
        assert_eq!(read_u32(&hp, PCIU_OFFSET), 0);
        assert_eq!(read_u32(&hp, PCID_OFFSET), 0);
    }

    #[test]
    fn every_slot_number_is_answered_without_a_panic() {
        let (hp, _sink) = device();

        for slot in 0..=u8::MAX {
            hp.notify_added(slot);
            hp.notify_removed(slot);
        }
        write_u32(&hp, B0EJ_OFFSET, 0xFFFF_FFFF);

        let ejected = hp.take_eject_requests();
        assert_eq!(ejected.len(), usize::from(PCI_SLOTS - FIRST_HOTPLUG_SLOT));
        assert_eq!(ejected.first(), Some(&FIRST_HOTPLUG_SLOT));
        assert_eq!(ejected.last(), Some(&(PCI_SLOTS - 1)));
    }

    #[test]
    fn slot_mask_covers_the_hotpluggable_slots_only() {
        assert_eq!(slot_mask(0), None);
        assert_eq!(slot_mask(1), None);
        assert_eq!(slot_mask(FIRST_HOTPLUG_SLOT), Some(1 << 2));
        assert_eq!(slot_mask(PCI_SLOTS - 1), Some(1 << 31));
        assert_eq!(slot_mask(PCI_SLOTS), None);
        assert_eq!(slot_mask(u8::MAX), None);
    }

    /// Every refusal here is one a guest reaches with a port write, so
    /// none of them may write a record. A guest that spins on a bad
    /// offset, a bad width, or an eject of a slot it does not own
    /// would otherwise fill the log as fast as it can trap.
    #[test]
    fn a_guest_cannot_flood_the_log_from_the_register_block() {
        let (hp, records) = counting_device();

        for _ in 0..200 {
            // A bad width, at every offset the block decodes.
            for offset in 0..usize::from(PCI_HOTPLUG_LEN) {
                read(&hp, offset, 1);
                let wo = WriteOp::from_buf(&[0xFF]);
                hp.pio_rw(offset, RWOp::Write(&wo));
            }
            // A good width at an offset the block does not decode.
            read_u32(&hp, 0x40);
            write_u32(&hp, 0x40, u32::MAX);
            // An eject of every slot, none of which holds a device.
            write_u32(&hp, B0EJ_OFFSET, u32::MAX);
        }

        assert!(hp.take_eject_requests().is_empty(), "nothing was ejectable");
        assert_eq!(
            records.load(Ordering::Relaxed),
            0,
            "the guest drove the log from the register block",
        );
    }
}
