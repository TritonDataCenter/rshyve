// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::thread;

use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};

use vmm_core::common::{RWOp, ReadOp, WriteOp};
use vmm_core::mem::RegionKind;
use vmm_devices::acpi_gpe::{GpeBit, HotplugEventSink};

/// The guest ABI of the register block, from QEMU's
/// `docs/specs/acpi_mem_hotplug.rst`. Named here because the
/// constants in `vmm_devices` are private to that crate.
const REG_ADDR_LOW: usize = 0x00;
const REG_ADDR_HIGH: usize = 0x04;
const REG_SIZE_LOW: usize = 0x08;
const REG_SIZE_HIGH: usize = 0x0C;
const REG_STATUS: usize = 0x14;
const REG_SELECTOR: usize = 0x00;
const REG_CONTROL: usize = 0x14;
const STATUS_ENABLED: u64 = 1 << 0;
const STATUS_INSERTING: u64 = 1 << 1;
const CONTROL_EJECT: u32 = 1 << 3;

/// A window well above the 4 GiB line, so both halves of the base
/// carry bits.
const BASE: u64 = 0x0000_0004_0000_0000;
const SLOT: u64 = DEFAULT_SLOT_SIZE;

fn test_log() -> Logger {
    Logger::root(slog::Discard, slog::o!())
}

/// Records every general purpose event the register block raises.
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

/// What one add asked the mapper for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mapped {
    segid: i32,
    gpa: u64,
    len: usize,
}

/// A mapper with no kernel behind it.
///
/// `fail_next` makes exactly one add fail, so a test can show what
/// a failure leaves behind.
#[derive(Default)]
struct FakeMapper {
    calls: Mutex<Vec<Mapped>>,
    fail_next: Mutex<Option<MapFailure>>,
}

impl FakeMapper {
    fn calls(&self) -> Vec<Mapped> {
        self.calls.lock().expect("fake mapper poisoned").clone()
    }

    fn fail_next(&self, segment_spent: bool) {
        *self.fail_next.lock().expect("fake mapper poisoned") =
            Some(MapFailure {
                error: MemHotplugError::Map("no kernel here".into()),
                segment_spent,
            });
    }
}

impl MemMapper for FakeMapper {
    fn add_ram(
        &self,
        segid: i32,
        gpa: u64,
        len: usize,
    ) -> Result<(), MapFailure> {
        if let Some(failure) =
            self.fail_next.lock().expect("fake mapper poisoned").take()
        {
            return Err(failure);
        }
        self.calls
            .lock()
            .expect("fake mapper poisoned")
            .push(Mapped { segid, gpa, len });
        Ok(())
    }
}

/// A mapper with the real overlap rule behind it and no kernel.
///
/// [`FakeMapper`] answers every add, so it cannot show what happens
/// when a window lands on memory the guest already has. This one runs
/// the same `PhysMap::claim_ram` the run-time path takes, and keeps the
/// claim for good instead of running the ioctls that would publish it.
struct ClaimingMapper {
    physmap: Arc<PhysMap>,
    granted: Mutex<Vec<u64>>,
}

impl ClaimingMapper {
    fn new(physmap: Arc<PhysMap>) -> Arc<Self> {
        Arc::new(Self {
            physmap,
            granted: Mutex::new(Vec::new()),
        })
    }

    fn granted(&self) -> Vec<u64> {
        self.granted
            .lock()
            .expect("claiming mapper poisoned")
            .clone()
    }
}

impl MemMapper for ClaimingMapper {
    fn add_ram(
        &self,
        _segid: i32,
        gpa: u64,
        len: usize,
    ) -> Result<(), MapFailure> {
        // A refused range must never spend a segment: there is no
        // ioctl that frees one.
        let claim =
            self.physmap.claim_ram(gpa, len).map_err(|e| MapFailure {
                error: MemHotplugError::Map(e.to_string()),
                segment_spent: false,
            })?;
        // Held, not dropped: a granted range must stay taken, or a
        // later add could be given it a second time.
        std::mem::forget(claim);
        self.granted
            .lock()
            .expect("claiming mapper poisoned")
            .push(gpa);
        Ok(())
    }
}

struct Harness {
    engine: Arc<MemHotplugEngine>,
    mapper: Arc<FakeMapper>,
    regs: Arc<MemHotplug>,
    sink: Arc<RecordingSink>,
}

fn harness(window: MemWindow, first_segid: i32) -> Harness {
    let sink = Arc::new(RecordingSink::default());
    let regs = MemHotplug::new(
        MAX_SLOTS,
        Arc::clone(&sink) as Arc<dyn HotplugEventSink>,
        test_log(),
    );
    let mapper = Arc::new(FakeMapper::default());
    let engine = MemHotplugEngine::start_with(
        Arc::clone(&mapper) as Arc<dyn MemMapper>,
        Arc::new(SegidAlloc::new(first_segid)),
        Arc::clone(&regs),
        window,
        test_log(),
    );
    Harness {
        engine,
        mapper,
        regs,
        sink,
    }
}

/// A window with room for four slots and one free segment, which is
/// what a UEFI VM with a framebuffer and more than 3 GiB has.
fn one_segment_harness() -> Harness {
    let window = MemWindow::new(BASE, 4 * SLOT, SLOT).expect("a legal window");
    harness(window, vmm_core::mem::VM_MAX_MEMSEGS - 1)
}

fn read(regs: &MemHotplug, offset: usize, width: usize) -> u64 {
    let mut ro = ReadOp::new(width);
    regs.pio_rw(offset, RWOp::Read(&mut ro));
    let mut bytes = [0u8; 8];
    bytes[..width].copy_from_slice(ro.buf());
    u64::from_le_bytes(bytes)
}

fn write(regs: &MemHotplug, offset: usize, value: u32) {
    let wo = WriteOp::from_buf(&value.to_le_bytes());
    regs.pio_rw(offset, RWOp::Write(&wo));
}

/// Run `_EJ0` on the selected slot, as the AML does.
///
/// The control register is ONE byte wide, and the register file drops a
/// write of any other width. A four-byte write here would be discarded
/// and the test would pass without an eject ever happening.
fn guest_ejects(regs: &MemHotplug, slot: u32) {
    select(regs, slot);
    let wo = WriteOp::from_buf(&[CONTROL_EJECT as u8]);
    regs.pio_rw(REG_CONTROL, RWOp::Write(&wo));
}

fn select(regs: &MemHotplug, slot: u32) {
    write(regs, REG_SELECTOR, slot);
}

/// The base and length the guest reads back for one slot.
fn slot_window(regs: &MemHotplug, slot: u32) -> (u64, u64) {
    select(regs, slot);
    let base =
        read(regs, REG_ADDR_LOW, 4) | (read(regs, REG_ADDR_HIGH, 4) << 32);
    let len =
        read(regs, REG_SIZE_LOW, 4) | (read(regs, REG_SIZE_HIGH, 4) << 32);
    (base, len)
}

fn slot_status(regs: &MemHotplug, slot: u32) -> u64 {
    select(regs, slot);
    read(regs, REG_STATUS, 1)
}

// ── The window ────────────────────────────────────────────────

#[test]
fn a_window_below_the_four_gib_line_is_refused() {
    // Anything lower runs into the PCI BARs, the APICs or the
    // firmware flash window.
    for base in [0, 0x8000_0000, MMIO_HOLE_END - SLOT] {
        assert!(matches!(
            MemWindow::new(base, SLOT, SLOT),
            Err(MemHotplugError::BadWindow(_)),
        ));
    }
    MemWindow::new(MMIO_HOLE_END, SLOT, SLOT).expect("4 GiB is legal");
}

#[test]
fn a_window_the_kernel_would_refuse_is_refused_here() {
    for (base, size, slot) in [
        // A slot the kernel cannot map.
        (BASE, SLOT, 0),
        (BASE, SLOT, 0x800),
        // A size that is not a whole number of slots.
        (BASE, 0, SLOT),
        (BASE, SLOT + 0x1000, SLOT),
        // A base that is not page aligned.
        (BASE + 1, SLOT, SLOT),
        // A window that runs off the end of the address space.
        (0xFFFF_FFFF_C000_0000, 0x8000_0000, SLOT),
    ] {
        assert!(
            matches!(
                MemWindow::new(base, size, slot),
                Err(MemHotplugError::BadWindow(_)),
            ),
            "{base:#x} {size:#x} {slot:#x}",
        );
    }
}

#[test]
fn a_window_never_describes_more_slots_than_the_kernel_can_back() {
    let window = MemWindow::new(BASE, 64 * SLOT, SLOT).expect("a legal window");
    assert_eq!(window.slots(), MAX_SLOTS);

    let small = MemWindow::new(BASE, 3 * SLOT, SLOT).expect("a legal window");
    assert_eq!(small.slots(), 3);
}

#[test]
fn a_request_is_rounded_up_to_a_whole_slot() {
    let window = MemWindow::new(BASE, 4 * SLOT, SLOT).expect("a legal window");
    assert_eq!(window.round_up(1), Ok(SLOT));
    assert_eq!(window.round_up(SLOT), Ok(SLOT));
    assert_eq!(window.round_up(SLOT + 1), Ok(2 * SLOT));
    assert_eq!(window.round_up(0), Err(MemHotplugError::EmptyRequest));
}

// ── The add path ──────────────────────────────────────────────

#[test]
fn an_add_maps_the_memory_then_tells_the_guest() {
    let h = harness(
        MemWindow::new(BASE, 4 * SLOT, SLOT).expect("a legal window"),
        0,
    );

    assert_eq!(h.engine.add_memory(SLOT), Ok(0));

    assert_eq!(
        h.mapper.calls(),
        vec![Mapped {
            segid: 0,
            gpa: BASE,
            len: SLOT as usize,
        }],
    );
    assert_eq!(slot_window(&h.regs, 0), (BASE, SLOT));
    assert_eq!(slot_status(&h.regs, 0), STATUS_ENABLED | STATUS_INSERTING,);
    assert_eq!(h.sink.raised(), vec![GpeBit::Memory]);
    assert_eq!(h.engine.slots_used(), 1);
    assert_eq!(h.engine.bytes_added(), SLOT);
}

#[test]
fn slots_are_handed_out_in_order_with_no_gaps() {
    let h = harness(
        MemWindow::new(BASE, 4 * SLOT, SLOT).expect("a legal window"),
        0,
    );

    assert_eq!(h.engine.add_memory(SLOT), Ok(0));
    assert_eq!(h.engine.add_memory(2 * SLOT), Ok(1));
    assert_eq!(h.engine.add_memory(SLOT), Ok(2));

    let bases: Vec<u64> = h.mapper.calls().iter().map(|c| c.gpa).collect();
    assert_eq!(bases, vec![BASE, BASE + SLOT, BASE + 3 * SLOT]);
    assert_eq!(h.engine.bytes_added(), 4 * SLOT);
}

#[test]
fn the_slot_cap_stops_an_add_before_any_ioctl() {
    // Three slots of room, so the cap bites before the window does.
    let h = harness(
        MemWindow::new(BASE, 3 * SLOT, SLOT).expect("a legal window"),
        0,
    );

    for slot in 0..3 {
        assert_eq!(h.engine.add_memory(SLOT), Ok(slot));
    }
    assert_eq!(
        h.engine.add_memory(SLOT),
        Err(MemHotplugError::NoSlots { slots: 3 }),
    );
    assert_eq!(h.mapper.calls().len(), 3);
}

#[test]
fn the_total_cap_stops_an_add_before_any_ioctl() {
    let h = harness(
        MemWindow::new(BASE, 4 * SLOT, SLOT).expect("a legal window"),
        0,
    );

    assert_eq!(h.engine.add_memory(3 * SLOT), Ok(0));
    assert_eq!(
        h.engine.add_memory(2 * SLOT),
        Err(MemHotplugError::WindowFull {
            asked: 2 * SLOT,
            free: SLOT,
        }),
    );
    // The window still has room for exactly one more slot.
    assert_eq!(h.mapper.calls().len(), 1);
    assert_eq!(h.engine.add_memory(SLOT), Ok(1));
}

#[test]
fn a_failed_add_leaves_its_slot_and_its_segment_for_the_next_one() {
    let h = harness(
        MemWindow::new(BASE, 4 * SLOT, SLOT).expect("a legal window"),
        0,
    );
    // Nothing was created, so the segment ID goes back.
    h.mapper.fail_next(false);

    assert!(matches!(
        h.engine.add_memory(SLOT),
        Err(MemHotplugError::Map(_)),
    ));
    assert_eq!(h.engine.slots_used(), 0);
    assert_eq!(h.engine.bytes_added(), 0);
    assert!(h.mapper.calls().is_empty());
    // Nothing was advertised, so the guest never saw a slot.
    assert_eq!(h.sink.raised(), Vec::new());
    assert_eq!(slot_status(&h.regs, 0), 0);

    // The retry takes the same slot, the same address and the same
    // segment ID.
    assert_eq!(h.engine.add_memory(SLOT), Ok(0));
    assert_eq!(
        h.mapper.calls(),
        vec![Mapped {
            segid: 0,
            gpa: BASE,
            len: SLOT as usize,
        }],
    );
}

#[test]
fn a_failure_past_the_allocation_keeps_the_segment_spent() {
    let h = harness(
        MemWindow::new(BASE, 4 * SLOT, SLOT).expect("a legal window"),
        0,
    );
    // The kernel created the segment, and it has no free.
    h.mapper.fail_next(true);

    assert!(matches!(
        h.engine.add_memory(SLOT),
        Err(MemHotplugError::Map(_)),
    ));
    // The slot comes back, the segment ID does not.
    assert_eq!(h.engine.slots_used(), 0);
    assert_eq!(h.engine.add_memory(SLOT), Ok(0));
    assert_eq!(h.mapper.calls()[0].segid, 1);
}

#[test]
fn segment_exhaustion_is_reported_and_not_a_panic() {
    // One segment left, which is what UEFI plus a framebuffer plus
    // more than 3 GiB leaves.
    let h = one_segment_harness();

    assert_eq!(h.engine.add_memory(SLOT), Ok(0));
    assert_eq!(
        h.engine.add_memory(SLOT),
        Err(MemHotplugError::NoSegment {
            limit: vmm_core::mem::VM_MAX_MEMSEGS,
        }),
    );
    // The refused add spent no slot and no address.
    assert_eq!(h.engine.slots_used(), 1);
    assert_eq!(h.engine.bytes_added(), SLOT);
}

#[test]
fn an_engine_never_uses_a_slot_the_register_file_lacks() {
    let sink = Arc::new(RecordingSink::default());
    // Two slots in the register file, eight in the window.
    let regs = MemHotplug::new(
        2,
        Arc::clone(&sink) as Arc<dyn HotplugEventSink>,
        test_log(),
    );
    let mapper = Arc::new(FakeMapper::default());
    let engine = MemHotplugEngine::start_with(
        Arc::clone(&mapper) as Arc<dyn MemMapper>,
        Arc::new(SegidAlloc::new(0)),
        regs,
        MemWindow::new(BASE, 8 * SLOT, SLOT).expect("a legal window"),
        test_log(),
    );

    assert_eq!(engine.add_memory(SLOT), Ok(0));
    assert_eq!(engine.add_memory(SLOT), Ok(1));
    assert_eq!(
        engine.add_memory(SLOT),
        Err(MemHotplugError::NoSlots { slots: 2 }),
    );
}

#[test]
fn concurrent_adds_take_different_slots_and_addresses() {
    let h = harness(
        MemWindow::new(BASE, 4 * SLOT, SLOT).expect("a legal window"),
        0,
    );
    let barrier = Arc::new(std::sync::Barrier::new(4));

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let engine = Arc::clone(&h.engine);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                engine.add_memory(SLOT)
            })
        })
        .collect();

    let mut slots: Vec<usize> = handles
        .into_iter()
        .map(|handle| handle.join().expect("no panic in an add"))
        .collect::<Result<Vec<_>, _>>()
        .expect("the window has room for four");
    slots.sort_unstable();
    assert_eq!(slots, vec![0, 1, 2, 3]);

    let mut bases: Vec<u64> = h.mapper.calls().iter().map(|c| c.gpa).collect();
    bases.sort_unstable();
    assert_eq!(
        bases,
        vec![BASE, BASE + SLOT, BASE + 2 * SLOT, BASE + 3 * SLOT],
    );
}

// ── The eject path ────────────────────────────────────────────

#[test]
fn a_window_that_lands_on_real_ram_is_refused_and_not_misplaced() {
    // The window base is computed before the VM exists, from the RAM
    // layout the machine builder is expected to use. If that layout
    // ever moved, the window would sit on memory the guest already
    // has. The claim in PhysMap is the backstop: the FIRST add has to
    // be refused, never placed on top of live RAM.
    let physmap = Arc::new(PhysMap::new());
    // High RAM that reaches past the window base, as a bigger guest
    // than the window was built for would have.
    physmap
        .add_region_anon(BASE - SLOT, 2 * SLOT as usize, RegionKind::Ram)
        .expect("the map is empty");

    let mapper = ClaimingMapper::new(Arc::clone(&physmap));
    let regs = MemHotplug::new(
        MAX_SLOTS,
        Arc::new(RecordingSink::default()) as Arc<dyn HotplugEventSink>,
        test_log(),
    );
    let segids = Arc::new(SegidAlloc::new(0));
    let engine = MemHotplugEngine::start_with(
        Arc::clone(&mapper) as Arc<dyn MemMapper>,
        Arc::clone(&segids),
        Arc::clone(&regs),
        MemWindow::new(BASE, 4 * SLOT, SLOT).expect("a legal window"),
        test_log(),
    );

    let refused = engine.add_memory(SLOT).expect_err("BASE is taken");
    assert!(
        matches!(&refused, MemHotplugError::Map(reason)
            if reason.contains("overlaps")),
        "{refused}",
    );
    assert!(mapper.granted().is_empty(), "a range was handed out anyway");
    assert_eq!(engine.slots_used(), 0, "a refused add spent a slot");
    assert_eq!(engine.bytes_added(), 0);
    assert_eq!(slot_status(&regs, 0) & STATUS_ENABLED, 0);
    // The segment id goes back: nothing was created.
    assert_eq!(segids.alloc(), Some(0));

    // The window is handed out from the front and a refused add spends
    // nothing, so the next add asks for the same range and is refused
    // the same way. Skipping past the live RAM would quietly give the
    // guest less memory than the window advertises.
    let again = engine.add_memory(SLOT).expect_err("BASE is still taken");
    assert_eq!(format!("{again}"), format!("{refused}"));
    assert!(mapper.granted().is_empty());
}

#[test]
fn a_window_whose_tail_meets_real_ram_stops_at_the_ram() {
    // The same backstop, one slot from the end: every add below the
    // live RAM works and the one that reaches it is refused.
    let physmap = Arc::new(PhysMap::new());
    physmap
        .add_region_anon(BASE + 3 * SLOT, SLOT as usize, RegionKind::Ram)
        .expect("the map is empty");

    let mapper = ClaimingMapper::new(Arc::clone(&physmap));
    let engine = MemHotplugEngine::start_with(
        Arc::clone(&mapper) as Arc<dyn MemMapper>,
        Arc::new(SegidAlloc::new(0)),
        MemHotplug::new(
            MAX_SLOTS,
            Arc::new(RecordingSink::default()) as Arc<dyn HotplugEventSink>,
            test_log(),
        ),
        MemWindow::new(BASE, 4 * SLOT, SLOT).expect("a legal window"),
        test_log(),
    );

    for slot in 0..3 {
        assert_eq!(engine.add_memory(SLOT), Ok(slot));
    }
    engine.add_memory(SLOT).expect_err("the last slot is taken");

    assert_eq!(mapper.granted(), [BASE, BASE + SLOT, BASE + 2 * SLOT]);
    assert_eq!(engine.slots_used(), 3);
}

#[test]
fn an_eject_is_refused_and_tears_nothing_down() {
    let h = harness(
        MemWindow::new(BASE, 4 * SLOT, SLOT).expect("a legal window"),
        0,
    );
    assert_eq!(h.engine.add_memory(SLOT), Ok(0));

    // The guest runs `_EJ0`, and the request really reaches the file.
    guest_ejects(&h.regs, 0);
    h.engine.refuse_ejects();
    assert!(
        h.regs.take_eject_requests().is_empty(),
        "the refusal left the request in the queue",
    );

    // The memory is still there and the slot is still enabled.
    assert_eq!(slot_window(&h.regs, 0), (BASE, SLOT));
    assert_eq!(slot_status(&h.regs, 0) & STATUS_ENABLED, STATUS_ENABLED);
    assert_eq!(h.engine.slots_used(), 1);
    assert_eq!(h.engine.bytes_added(), SLOT);
    // Nothing was unmapped: the kernel cannot free a segment.
    assert_eq!(h.mapper.calls().len(), 1);
}

#[test]
fn a_guest_cannot_drive_the_log_with_ejects() {
    let count = Arc::new(AtomicUsize::new(0));
    let sink = Arc::new(RecordingSink::default());
    // The register file gets the counting logger too. The drain empties
    // the eject list every pass, so the file's own dedup only holds
    // until the next one and its record has to be debug as well.
    let regs = MemHotplug::new(
        MAX_SLOTS,
        Arc::clone(&sink) as Arc<dyn HotplugEventSink>,
        counting_log(Arc::clone(&count)),
    );
    let drain = Drain {
        regs: Arc::clone(&regs),
        log: counting_log(Arc::clone(&count)),
    };
    regs.set_slot(0, BASE, SLOT).expect("slot 0 exists");
    // set_slot is a host action and may say so. Only what the guest can
    // repeat is counted.
    count.store(0, Ordering::Relaxed);

    for _ in 0..1000 {
        guest_ejects(&regs, 0);
        drain.refuse_ejects();
    }
    assert_eq!(count.load(Ordering::Relaxed), 0);
}

/// Counts records at info level and above.
fn counting_log(count: Arc<AtomicUsize>) -> Logger {
    use slog::Drain as _;

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

    Logger::root(CountingDrain(count).fuse(), slog::o!())
}
