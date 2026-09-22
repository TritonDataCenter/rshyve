// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Memory hot-add.
//!
//! The illumos kernel has no VM_FREE_MEMSEG, so this is add only.
//!
//! # Why one segment per add
//!
//! `vm_alloc_memseg` takes the memory out of the reservoir there and
//! then: `vmmr_alloc` in `vmm_reservoir.c` drops `vmmr_free_sz` by the
//! whole segment length before it returns. Reserving one segment the
//! size of the whole window would therefore spend all of
//! `hotplug.maxmem` at boot, and a guest that pays for its ceiling up
//! front may as well have booted with it. So each add gets its own
//! segment, sized to what the operator asked for, and the host pays only
//! for memory the guest really has.
//!
//! The price is the segment count. `VM_MAX_MEMSEGS` in `vmm.c` gives one
//! VM five, and boot spends some of them. A UEFI VM with a framebuffer
//! and more than 3 GiB spends four, which leaves one add. Direct boot
//! leaves three. [`MemHotplugError::NoSegment`] says so plainly instead
//! of letting the kernel answer EINVAL.
//!
//! # Why nothing has to stop the guest
//!
//! `VM_ALLOC_MEMSEG` and `VM_MMAP_MEMSEG` carry no run-state check
//! (`vm_alloc_memseg` and `vm_mmap_memseg` in `vmm.c`). They are
//! write-lock ioctls (`LOCK_WRITE_HOLD` in `vmmdev_do_ioctl`), so the
//! kernel drives every vCPU out with a `VM_EXITCODE_BOGUS` exit
//! (`vcpu_bailout_checks`) and freezes it for the length of the call.
//! [`crate::vcpu_tasks`] treats that exit as a plain re-entry, so the
//! run loop needs no change.
//!
//! # Hot remove
//!
//! There is no `VM_FREE_MEMSEG` ioctl, and `vm_free_memseg` in `vmm.c`
//! has only destroy-path callers. A segment lives until the VM goes
//! away. The engine reports every eject the guest asks for and refuses
//! it. Nothing here may be built as though removal works.

use std::fmt;
use std::sync::{Arc, Mutex};

use slog::{debug, error, info, Logger};

use vmm_core::hdl::VmmHdl;
use vmm_core::mem::{PhysMap, SegidAlloc, MMIO_HOLE_BASE, MMIO_HOLE_END};
use vmm_devices::hotplug::mem::MAX_SLOTS as MEM_SLOTS;
use vmm_devices::hotplug::mem::{
    MemHotplug, MemHotplugError as SlotError, MAX_SLOTS,
};

use super::drain::DrainThread;
use super::lock;

/// Guest page size. A mapping the kernel accepts is a whole number of
/// these.
const PAGE_SIZE: u64 = vmm_core::common::PAGE_SIZE as u64;

/// Default size of one hot-add slot.
///
/// Linux uses 128 MiB memory blocks on x86-64 and will not online a
/// part of one, so a smaller slot would give the guest memory it cannot
/// use.
pub const DEFAULT_SLOT_SIZE: u64 = 128 * 1024 * 1024;

/// Why a memory hot-add was refused.
///
/// Hand-written rather than derived: this crate carries no `thiserror`
/// dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemHotplugError {
    /// The VM was not started with a hot-add window.
    Disabled,
    /// The window an operator asked for cannot be used.
    BadWindow(String),
    /// An add of no memory.
    EmptyRequest,
    /// Every slot the window describes is full.
    NoSlots { slots: usize },
    /// The window has no room left for this add.
    WindowFull { asked: u64, free: u64 },
    /// The kernel has no memory segment left. See the module comment.
    NoSegment { limit: i32 },
    /// The kernel or the physical map refused the mapping.
    Map(String),
    /// The register file refused the slot.
    Registers(String),
    /// The engine is shut down.
    Stopped,
}

impl fmt::Display for MemHotplugError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => {
                f.write_str("this VM was not started with a memory window")
            }
            Self::BadWindow(reason) => write!(f, "bad memory window: {reason}"),
            Self::EmptyRequest => f.write_str("an add needs a size"),
            Self::NoSlots { slots } => {
                write!(f, "all {slots} memory slots are full")
            }
            Self::WindowFull { asked, free } => write!(
                f,
                "the memory window has {free:#x} bytes left, not {asked:#x}",
            ),
            Self::NoSegment { limit } => write!(
                f,
                "this VM has spent all {limit} of its kernel memory segments",
            ),
            Self::Map(reason) => {
                write!(f, "cannot map the memory: {reason}")
            }
            Self::Registers(reason) => {
                write!(f, "cannot fill the memory slot: {reason}")
            }
            Self::Stopped => f.write_str("the memory engine is shut down"),
        }
    }
}

impl std::error::Error for MemHotplugError {}

/// The guest address range hot-added memory is placed in.
///
/// An operator sets it with `-o hotplug.maxmem`. It sits above the 4 GiB
/// line, past every RAM region the VM booted with, so a slot can never
/// land in the 32-bit MMIO hole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemWindow {
    base: u64,
    size: u64,
    slot_size: u64,
}

impl MemWindow {
    /// Describe a window, or say why it cannot be used.
    pub fn new(
        base: u64,
        size: u64,
        slot_size: u64,
    ) -> Result<Self, MemHotplugError> {
        let bad = |reason: String| Err(MemHotplugError::BadWindow(reason));

        if slot_size == 0 || !slot_size.is_multiple_of(PAGE_SIZE) {
            return bad(format!("slot size {slot_size:#x} is not page sized"));
        }
        if size == 0 || !size.is_multiple_of(slot_size) {
            return bad(format!(
                "size {size:#x} is not a multiple of the {slot_size:#x} slot",
            ));
        }
        if !base.is_multiple_of(PAGE_SIZE) {
            return bad(format!("base {base:#x} is not page aligned"));
        }
        // Below this line the window would run into the PCI BARs, the
        // APICs or the firmware flash window.
        if base < MMIO_HOLE_END {
            return bad(format!(
                "base {base:#x} is below the {MMIO_HOLE_END:#x} line",
            ));
        }
        if base.checked_add(size).is_none() {
            return bad(format!(
                "[{base:#x}, +{size:#x}) runs off the address space",
            ));
        }

        Ok(Self {
            base,
            size,
            slot_size,
        })
    }

    pub fn base(&self) -> u64 {
        self.base
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn slot_size(&self) -> u64 {
        self.slot_size
    }

    /// How many slots the window describes.
    ///
    /// Never more than [`MAX_SLOTS`]: the register file has no more,
    /// and the kernel could not back them.
    pub fn slots(&self) -> usize {
        let by_size = (self.size / self.slot_size).min(MAX_SLOTS as u64);
        by_size as usize
    }

    /// Round a request up to a whole number of slots.
    fn round_up(&self, bytes: u64) -> Result<u64, MemHotplugError> {
        if bytes == 0 {
            return Err(MemHotplugError::EmptyRequest);
        }
        let slots = bytes.div_ceil(self.slot_size);
        slots
            .checked_mul(self.slot_size)
            .ok_or(MemHotplugError::WindowFull {
                asked: bytes,
                free: self.size,
            })
    }
}

/// Puts one slice of guest RAM in place.
///
/// A trait, not the handle itself, so the slot and window bookkeeping
/// can be tested without a live VM.
pub trait MemMapper: Send + Sync {
    /// Allocate a kernel segment of `len` bytes and map it at `gpa`.
    fn add_ram(
        &self,
        segid: i32,
        gpa: u64,
        len: usize,
    ) -> Result<(), MapFailure>;
}

/// Why an add failed, and what it spent on the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapFailure {
    pub error: MemHotplugError,
    /// Whether `VM_ALLOC_MEMSEG` created the segment. There is no ioctl
    /// that frees one, so a created segment is gone for the life of the
    /// VM and its ID must never be handed out again.
    pub segment_spent: bool,
}

/// The real mapper: a kernel segment published in the physical map.
struct KernelMapper {
    physmap: Arc<PhysMap>,
    hdl: Arc<VmmHdl>,
}

impl MemMapper for KernelMapper {
    fn add_ram(
        &self,
        segid: i32,
        gpa: u64,
        len: usize,
    ) -> Result<(), MapFailure> {
        let failed = |e: std::io::Error, segment_spent: bool| MapFailure {
            error: MemHotplugError::Map(e.to_string()),
            segment_spent,
        };

        // Claim the range before the segment exists. A refused range
        // must not spend a segment the kernel can never give back.
        let claim = self
            .physmap
            .claim_ram(gpa, len)
            .map_err(|e| failed(e, false))?;
        self.hdl
            .create_memseg(segid, len, &format!("hotmem{segid}"))
            .map_err(|e| failed(e, false))?;
        claim
            .commit(&self.hdl, segid, 0)
            .map_err(|e| failed(e, true))
    }
}

/// What the drain thread needs.
///
/// Split out so the thread holds a weak reference and ends when the
/// engine is dropped without a shutdown.
struct Drain {
    regs: Arc<MemHotplug>,
    log: Logger,
}

impl Drain {
    /// Refuse every eject the guest has run since the last pass.
    fn refuse_ejects(&self) {
        for slot in self.regs.take_eject_requests() {
            // A guest can run `_EJ0` as often as it likes, so this is
            // debug: it must not be able to drive the log.
            debug!(self.log, "refused a memory eject: this kernel cannot \
                free a segment"; "slot" => slot);
        }
    }
}

/// What one add changes. Behind one lock, so an add is all or nothing.
#[derive(Default)]
struct Filled {
    /// Bytes handed out from the front of the window.
    used: u64,
    /// The next unused slot.
    next_slot: usize,
}

/// The memory hot-add engine, and the thread that refuses removals.
pub struct MemHotplugEngine {
    mapper: Arc<dyn MemMapper>,
    segids: Arc<SegidAlloc>,
    regs: Arc<MemHotplug>,
    window: MemWindow,
    /// Slots this engine will use: the window and the register file
    /// have to agree, and the smaller of the two wins.
    slots: usize,
    filled: Mutex<Filled>,
    /// The drain thread holds a weak reference to this. Keeping the
    /// strong one here is what stops the thread ending early.
    drain: Arc<Drain>,
    thread: DrainThread,
    log: Logger,
}

impl MemHotplugEngine {
    /// Start the engine and the thread that answers ejects.
    pub fn start(
        physmap: Arc<PhysMap>,
        hdl: Arc<VmmHdl>,
        segids: Arc<SegidAlloc>,
        regs: Arc<MemHotplug>,
        window: MemWindow,
        log: Logger,
    ) -> Arc<Self> {
        let mapper = Arc::new(KernelMapper { physmap, hdl });
        Self::start_with(mapper, segids, regs, window, log)
    }

    fn start_with(
        mapper: Arc<dyn MemMapper>,
        segids: Arc<SegidAlloc>,
        regs: Arc<MemHotplug>,
        window: MemWindow,
        log: Logger,
    ) -> Arc<Self> {
        // The AML describes the register file's slots. A window that
        // claims more would advertise a DIMM the guest cannot see.
        let slots = window.slots().min(regs.slots());
        if slots != window.slots() {
            error!(log, "memory window has more slots than the register file";
                "window" => window.slots(), "registers" => regs.slots());
        }

        let drain = Arc::new(Drain {
            regs: Arc::clone(&regs),
            log: log.clone(),
        });
        let thread = DrainThread::spawn(
            "hotplug-mem",
            &drain,
            Drain::refuse_ejects,
            &log,
            "no memory hotplug thread; ejects go unanswered",
        );

        Arc::new(Self {
            mapper,
            segids,
            regs,
            window,
            slots,
            filled: Mutex::new(Filled::default()),
            drain,
            thread,
            log,
        })
    }

    /// The window this engine hands out.
    pub fn window(&self) -> MemWindow {
        self.window
    }

    /// How many slots are filled.
    pub fn slots_used(&self) -> usize {
        lock(&self.filled).next_slot
    }

    /// Bytes handed to the guest so far.
    pub fn bytes_added(&self) -> u64 {
        lock(&self.filled).used
    }

    /// Give the guest `bytes` more memory and return the slot it went
    /// in.
    ///
    /// The size is rounded up to a whole slot. Every limit is checked
    /// before the first ioctl, so a refused request allocates nothing.
    pub fn add_memory(&self, bytes: u64) -> Result<usize, MemHotplugError> {
        let len = self.window.round_up(bytes)?;

        // One add at a time. The counters below and the kernel segment
        // must not disagree, and an add is rare enough that serialising
        // costs nothing.
        let mut filled = lock(&self.filled);

        let slot = filled.next_slot;
        if slot >= self.slots {
            return Err(MemHotplugError::NoSlots { slots: self.slots });
        }
        // Saturating, not plain: every add below checks against `free`
        // first, so `used` can never pass `size`, and an underflow here
        // would hand out an address outside the window.
        let free = self.window.size.saturating_sub(filled.used);
        if len > free {
            return Err(MemHotplugError::WindowFull { asked: len, free });
        }
        let base = self
            .window
            .base
            .checked_add(filled.used)
            .ok_or(MemHotplugError::WindowFull { asked: len, free })?;
        let len_usize = usize::try_from(len).map_err(|_| {
            MemHotplugError::BadWindow(format!(
                "a slot of {len:#x} bytes does not fit this host's usize",
            ))
        })?;

        // Last of the cheap checks. Reporting exhaustion here gives the
        // operator a reason instead of an EINVAL from the kernel.
        let segid = self.segids.alloc().ok_or(MemHotplugError::NoSegment {
            limit: vmm_core::mem::VM_MAX_MEMSEGS,
        })?;

        // The slot and the window offset are only spent once the memory
        // is really there. A failed add leaves both for the next one.
        if let Err(failure) = self.mapper.add_ram(segid, base, len_usize) {
            // An ID whose segment was never created can go back. One
            // whose segment exists cannot: the kernel has no free.
            let reclaimed =
                !failure.segment_spent && self.segids.release(segid);
            error!(self.log, "memory hot-add failed";
                "slot" => slot, "base" => format_args!("{base:#x}"),
                "len" => len, "segid_reclaimed" => reclaimed,
                "error" => %failure.error);
            return Err(failure.error);
        }
        filled.next_slot = slot + 1;
        filled.used += len;
        drop(filled);

        // The registers before the event: the guest reads `_CRS` from
        // them as soon as it sees the device check.
        if let Err(e) = self.regs.set_slot(slot, base, len) {
            // The memory is mapped and the segment cannot be freed, so
            // the slot stays spent. Say so rather than reporting a
            // success the guest will never see.
            error!(self.log, "memory is mapped but its slot was refused";
                "slot" => slot, "error" => %e);
            return Err(MemHotplugError::Registers(slot_error(e)));
        }
        self.regs.notify_added(slot);

        info!(self.log, "memory added"; "slot" => slot,
            "base" => format_args!("{base:#x}"), "len" => len);
        Ok(slot)
    }

    /// Answer every eject the guest has run.
    ///
    /// The drain thread calls this on a timer, and [`Self::shutdown`]
    /// calls it once more so a request that arrived last still gets an
    /// answer.
    pub fn refuse_ejects(&self) {
        self.drain.refuse_ejects();
    }

    /// Stop the drain thread and wait for it.
    pub fn shutdown(&self) {
        self.thread.shutdown(super::DRAIN_JOIN_BUDGET);
    }
}

impl Drop for MemHotplugEngine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Render a register file error without pulling its type into the API.
fn slot_error(e: SlotError) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests;

/// The guest address window that hot-added memory goes in.
///
/// It starts above every RAM region the VM booted with. The machine
/// builder maps `[0, min(mem, MMIO_HOLE_BASE))` and, for a bigger
/// guest, `[MMIO_HOLE_END, MMIO_HOLE_END + mem - MMIO_HOLE_BASE)`, so
/// the first free address is the top of that second range. The window
/// is therefore always above 4 GiB and can never land in the 32-bit
/// MMIO hole.
///
/// `slot_size` has to divide `max_mem`, and the quotient has to fit the
/// [`MEM_SLOTS`] slots the register file and the AML describe. Both are
/// refused here rather than silently clamped, because a window that
/// claims more slots than the guest has would report free memory no add
/// can reach.
///
/// The base is computed from `mem_size` because this runs before the VM
/// exists. If that layout ever changed, the first add would be REFUSED
/// and not misplaced: `PhysMap::claim_ram` checks the range against
/// every published region before any ioctl runs.
///
/// The kernel puts a second, lower ceiling on this: one VM gets five
/// memory segments (`VM_MAX_MEMSEGS`) and boot spends some of them, so
/// the real number of adds can be below the slot count. The engine
/// reports that as it happens, because what boot spent depends on the
/// devices.
pub fn hot_add_window(
    mem_size: usize,
    max_mem: u64,
    slot_size: u64,
) -> Result<MemWindow, MemHotplugError> {
    let bad = |reason: String| Err(MemHotplugError::BadWindow(reason));

    if slot_size == 0 {
        return bad("a slot size of 0 describes no memory".to_string());
    }
    let Ok(mem_size) = u64::try_from(mem_size) else {
        return bad(format!("{mem_size} bytes of RAM does not fit a u64"));
    };
    let highmem = mem_size.saturating_sub(MMIO_HOLE_BASE);
    let Some(base) = MMIO_HOLE_END.checked_add(highmem) else {
        return bad(format!("{mem_size} bytes of RAM leaves no window"));
    };
    if !max_mem.is_multiple_of(slot_size) {
        return bad(format!(
            "a window of {max_mem:#x} does not divide into {slot_size:#x} slots",
        ));
    }
    let slots = max_mem / slot_size;
    if slots > MEM_SLOTS as u64 {
        return bad(format!(
            "a {max_mem:#x} window of {slot_size:#x} slots needs {slots} \
             slots, and the guest has {MEM_SLOTS}; raise the slot size",
        ));
    }

    MemWindow::new(base, max_mem, slot_size)
}

/// The hot-add window, which `hotplug/mem/tests.rs` does not cover
/// because it is arithmetic and needs no engine.
#[cfg(test)]
mod window_tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;
    const SLOT: u64 = 128 * 1024 * 1024;

    #[test]
    fn the_window_starts_above_every_ram_region_the_vm_booted_with() {
        // A window that overlapped high RAM would map a second segment
        // over memory the guest is already using.
        let small = hot_add_window(1024 * 1024 * 1024, GIB, SLOT)
            .expect("1 GiB of RAM is all low memory");
        assert_eq!(small.base(), MMIO_HOLE_END);

        // 4 GiB of RAM is 3 GiB low and 1 GiB high, so high RAM ends at
        // 5 GiB and the window starts there.
        let big = hot_add_window(4 * 1024 * 1024 * 1024, GIB, SLOT)
            .expect("4 GiB of RAM has a high half");
        assert_eq!(big.base(), MMIO_HOLE_END + GIB);
    }

    #[test]
    fn the_window_never_reaches_the_mmio_hole() {
        for gib in [1u64, 3, 4, 8, 64] {
            let window = hot_add_window((gib * GIB) as usize, GIB, SLOT)
                .unwrap_or_else(|e| panic!("{gib} GiB: {e}"));
            assert!(
                window.base() >= MMIO_HOLE_END,
                "{gib} GiB put the window at {:#x}",
                window.base(),
            );
        }
    }

    #[test]
    fn a_window_the_guest_has_no_slots_for_is_refused() {
        // 8 GiB of 128 MiB slots wants 64 slots and the AML declares 8.
        // Clamping instead would report free memory no add can reach.
        let refused =
            hot_add_window(GIB as usize, 8 * GIB, SLOT).expect_err("64 slots");
        assert!(
            refused.to_string().contains("raise the slot size"),
            "{refused}",
        );

        // The same window in slots the guest has.
        let window = hot_add_window(GIB as usize, 8 * GIB, GIB)
            .expect("8 slots of 1 GiB");
        assert_eq!(window.slots(), 8);
    }

    #[test]
    fn a_window_the_slot_size_does_not_divide_is_refused() {
        let refused = hot_add_window(GIB as usize, GIB + 1, SLOT)
            .expect_err("not a whole number of slots");
        assert!(refused.to_string().contains("does not divide"), "{refused}");

        assert!(hot_add_window(GIB as usize, GIB, 0).is_err());
    }
}
