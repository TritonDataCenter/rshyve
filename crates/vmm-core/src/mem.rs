// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest physical memory management.
//!
//! # Safety model
//!
//! vCPUs and DMA-capable devices can access guest memory at the same
//! time. A Rust reference (`&` or `&mut`) to guest memory would break
//! Rust's aliasing rules, because a vCPU can read or write at any time.
//! No code forms one.
//!
//! [`Mapping`] (an owned mmap region) and [`SubMapping`] (a borrowed
//! view) operate only through raw pointer operations (`read_volatile`,
//! `write_volatile`, `copy_nonoverlapping`).
//!
//! A [`SubMapping`] holds an `Arc` of its `Mapping`, so the memory stays
//! mapped while any view exists.
//!
//! # Why every region is mapped twice
//!
//! The kernel gives two ways to reach the pages of a memory segment,
//! and they do not behave the same.
//!
//! A mapping of the guest physical address space (`vm_segmap_space` in
//! `uts/intel/io/vmm/vmm_vm.c`) faults through `segvmm_fault_space`,
//! which holds each page with `vmc_hold` and releases it with
//! `vmp_release`. That release is the one place that sets the dirty bit
//! in the nested page tables, and the dirty bitmap is all that live
//! migration has to find pages the VMM changed.
//!
//! A mapping of the devmem object (`vm_segmap_obj`) faults through
//! `segvmm_fault_obj`, which does a bare `hat_devload` on the raw page
//! frame. The kernel never sees a write through it.
//!
//! So each region keeps both: [`PhysMap::lookup`] hands out the tracked
//! view for device emulation, and [`PhysMap::lookup_untracked`] hands
//! out the devmem view for the boot-image loaders, which run before any
//! vCPU and before a migration can start. This mirrors Propolis, whose
//! `map_guest` and `map_seg` pair this is adapted from.

use std::io::{Error, ErrorKind, Result};
use std::sync::{Arc, RwLock};

use crate::common::PAGE_SIZE;
use crate::hdl::VmmHdl;

mod ctx;
mod devmem;
mod io;
mod mapping;
mod segid;

pub use ctx::MemCtx;
pub use devmem::DevMemSeg;
pub use io::GuestIoVec;
use mapping::Mapping;
pub use mapping::{Prot, SubMapping};
pub use segid::{SegidAlloc, VM_MAX_MEMSEGS};

/// Type of a guest physical memory region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionKind {
    /// Read/write/execute RAM.
    Ram,
    /// Read/execute ROM (e.g., bootrom).
    Rom,
    /// MMIO reservation (no backing memory).
    Mmio,
}

/// A region in the guest physical address space.
struct Region {
    gpa: u64,
    len: usize,
    kind: RegionKind,
    /// Mapping of the guest physical address space. A write through
    /// this one is what the kernel marks dirty. See the module comment.
    tracked: TrackedRun,
    /// Mapping of the memory segment itself. Always writable, and
    /// outside the kernel's dirty tracking.
    direct: Arc<Mapping>,
}

impl Region {
    /// The first guest address past this region.
    fn end(&self) -> u64 {
        self.gpa + self.len as u64
    }
}

/// One mapping over a run of abutting RAM regions.
///
/// Hot-added RAM abuts the region below it, and one slot abuts the
/// next. A guest buffer over such a seam is ordinary, because
/// `blk_rq_map_sg` merges physically adjacent pages into one
/// scatter-gather entry. One mapping over the whole run is what lets
/// [`PhysMap::lookup`] answer that buffer with a single [`SubMapping`],
/// which is all its callers can use. Without it virtio-blk reports a
/// disk fault for legal memory.
///
/// The kernel allows this because the guest physical address space is
/// linear: `vm_segmap_space` takes any page-aligned range and
/// `segvmm_fault_space` resolves each page on its own, so one mapping
/// may cross several memory segments.
///
/// A region of any other kind gets a run of itself. A ROM carries the
/// guest's read-only protection and a reservation has no backing.
#[derive(Clone)]
struct TrackedRun {
    /// The guest address the mapping starts at.
    gpa: u64,
    mapping: Arc<Mapping>,
    /// What `lookup` lets a device do to the run. A ROM's run is the
    /// writable devmem mapping, so the guest protection is applied
    /// here, not by the mmap.
    prot: Prot,
}

impl TrackedRun {
    /// The first guest address past this run.
    fn end(&self) -> u64 {
        self.gpa + self.mapping.len as u64
    }
}

/// The two host mappings of one region.
struct RegionViews {
    tracked: TrackedRun,
    direct: Arc<Mapping>,
}

/// Start of the 32-bit MMIO hole.
///
/// This is the default lowmem limit in `machine.rs`. Everything from
/// here to 4 GiB belongs to PCI BARs, the APICs and the firmware flash
/// window, so no RAM may be placed in it at run time.
pub const MMIO_HOLE_BASE: u64 = 0xC000_0000;

/// End of the 32-bit MMIO hole, which is the 4 GiB line.
pub const MMIO_HOLE_END: u64 = 0x1_0000_0000;

/// Where a claim is allowed to sit.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Placement {
    /// Boot time. The 32-bit hole is legal, because the boot ROM, its
    /// variable store and the firmware scratch RAM all live there.
    Firmware,
    /// Run time. RAM only, so the hole stays clear for the PCI BARs,
    /// the APICs and the flash window.
    Ram,
}

/// What is published in the guest physical address space, and what is
/// spoken for by an add that has not finished.
#[derive(Default)]
struct AddressSpace {
    /// Published regions, sorted by GPA.
    regions: Vec<Region>,
    /// `[gpa, end)` ranges held by an add still in progress.
    ///
    /// A claim keeps its place while the kernel work runs outside the
    /// lock, so two adds can never map the same guest addresses.
    claims: Vec<(u64, u64)>,
}

impl AddressSpace {
    /// Reject a `[gpa, end)` range that touches anything already here.
    ///
    /// Both the boot path and the run-time path go through this, so
    /// there is one overlap rule and not two.
    fn check_free(&self, gpa: u64, end: u64) -> Result<()> {
        for r in &self.regions {
            let r_end = r.gpa + r.len as u64;
            if gpa < r_end && end > r.gpa {
                return Err(overlaps(gpa, end, r.gpa, r_end, "existing"));
            }
        }
        for &(c_gpa, c_end) in &self.claims {
            if gpa < c_end && end > c_gpa {
                return Err(overlaps(gpa, end, c_gpa, c_end, "claimed"));
            }
        }
        Ok(())
    }
}

/// Manages the guest physical address space.
///
/// Tracks all memory regions (RAM, ROM, MMIO) and provides GPA-to-mapping
/// translation. Regions are non-overlapping and sorted by GPA.
pub struct PhysMap {
    space: RwLock<AddressSpace>,
    /// Held for a whole publish, so the run a region joins cannot grow
    /// between the moment its mapping is made and the moment the
    /// regions adopt it. `lookup` never takes this, so no guest memory
    /// access waits on an add.
    publish: std::sync::Mutex<()>,
    next_segid: i32,
}

impl Default for PhysMap {
    fn default() -> Self {
        Self::new()
    }
}

impl PhysMap {
    pub fn new() -> Self {
        Self {
            space: RwLock::new(AddressSpace::default()),
            publish: std::sync::Mutex::new(()),
            next_segid: 0,
        }
    }

    /// Device tests need real `PhysMap::lookup` boundaries without creating a
    /// kernel VM, which is unavailable on non-illumos test hosts.
    #[doc(hidden)]
    pub fn new_anon(gpa: u64, len: usize) -> Result<Self> {
        let map = Self::new();
        map.add_region_anon(gpa, len, RegionKind::Ram)?;
        Ok(map)
    }

    /// Next unused kernel memory-segment ID.
    pub fn next_segid(&self) -> i32 {
        self.next_segid
    }

    /// Add a RAM region backed by a new bhyve memory segment.
    pub fn add_ram(
        &mut self,
        hdl: &VmmHdl,
        gpa: u64,
        len: usize,
    ) -> Result<()> {
        self.add_region(hdl, gpa, len, RegionKind::Ram, Prot::RWX)
    }

    /// Add a ROM region and a writable RAM region that share one kernel
    /// segment.
    ///
    /// The RAM comes from the tail of the segment, past the ROM image,
    /// and gets its own guest mapping at `ram_gpa`. The two mappings
    /// cover disjoint parts of the segment, so no byte is visible at
    /// two guest addresses.
    ///
    /// This exists to save a segment ID. The kernel gives one VM only
    /// `VM_MAX_MEMSEGS` of them, and `vm_mmap_memseg` puts no limit on
    /// how many mappings one segment may have.
    pub fn add_rom_with_ram_tail(
        &mut self,
        hdl: &VmmHdl,
        rom_gpa: u64,
        rom_len: usize,
        ram_gpa: u64,
        ram_len: usize,
    ) -> Result<()> {
        let seg_len = rom_len.checked_add(ram_len).ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "ROM and RAM tail lengths overflow",
            )
        })?;
        let ram_off = i64::try_from(rom_len).map_err(|_| {
            Error::new(ErrorKind::InvalidInput, "ROM length exceeds i64")
        })?;

        let segid = self.next_segid;
        let mut created = false;
        let result = (|| -> Result<()> {
            // Both ranges are claimed before the segment exists, so a
            // bad layout cannot spend a segment the kernel never gives
            // back.
            let rom = self.claim(
                rom_gpa,
                rom_len,
                RegionKind::Rom,
                guest_prot_for(RegionKind::Rom),
                Placement::Firmware,
            )?;
            let ram = self.claim(
                ram_gpa,
                ram_len,
                RegionKind::Ram,
                guest_prot_for(RegionKind::Ram),
                Placement::Firmware,
            )?;
            hdl.create_memseg(segid, seg_len, &seg_name(segid))?;
            created = true;
            rom.commit(hdl, segid, 0)?;
            ram.commit(hdl, segid, ram_off)
        })();

        if created {
            self.next_segid += 1;
        }
        result
    }

    fn add_region(
        &mut self,
        hdl: &VmmHdl,
        gpa: u64,
        len: usize,
        kind: RegionKind,
        guest_prot: Prot,
    ) -> Result<()> {
        let segid = self.next_segid;
        let mut created = false;
        let result = (|| -> Result<()> {
            let claim =
                self.claim(gpa, len, kind, guest_prot, Placement::Firmware)?;
            hdl.create_memseg(segid, len, &seg_name(segid))?;
            created = true;
            claim.commit(hdl, segid, 0)
        })();

        // There is no ioctl that frees a segment, so one that was
        // created is spent whether or not its mapping worked.
        if created {
            self.next_segid += 1;
        }
        result
    }

    /// Hold a range of the guest address space for a run-time RAM add.
    ///
    /// The claim is taken under the write lock and runs the same
    /// overlap rule the boot path uses, so two concurrent adds cannot
    /// both pass. Nothing is allocated yet: drop the claim to give the
    /// range back, or commit it once the segment exists.
    pub fn claim_ram(&self, gpa: u64, len: usize) -> Result<RamClaim<'_>> {
        self.claim(gpa, len, RegionKind::Ram, Prot::RWX, Placement::Ram)
    }

    /// Take a range, or report why it cannot be had.
    fn claim(
        &self,
        gpa: u64,
        len: usize,
        kind: RegionKind,
        guest_prot: Prot,
        placement: Placement,
    ) -> Result<RamClaim<'_>> {
        let end = region_end(gpa, len)?;

        if placement == Placement::Ram {
            // The kernel would answer EINVAL. Say why instead.
            if !gpa.is_multiple_of(PAGE_SIZE as u64)
                || !len.is_multiple_of(PAGE_SIZE)
            {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("RAM [{gpa:#x}, {end:#x}) is not page aligned"),
                ));
            }
            // The hole holds the PCI BARs, the APICs, the boot ROM and
            // its variable store. RAM placed there would be shadowed by
            // a BAR or would sit under the firmware flash window.
            if gpa < MMIO_HOLE_END && end > MMIO_HOLE_BASE {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "RAM [{gpa:#x}, {end:#x}) runs into the 32-bit \
                         MMIO hole [{MMIO_HOLE_BASE:#x}, \
                         {MMIO_HOLE_END:#x})",
                    ),
                ));
            }
        }

        let mut space = self.write_space();
        space.check_free(gpa, end)?;
        space.claims.push((gpa, end));

        Ok(RamClaim {
            map: self,
            gpa,
            len,
            kind,
            guest_prot,
            live: true,
        })
    }

    /// Give a claimed range back.
    fn release_claim(&self, gpa: u64) {
        let mut space = self.write_space();
        if let Some(idx) = space.claims.iter().position(|&(c, _)| c == gpa) {
            space.claims.swap_remove(idx);
        }
    }

    /// Publish a region backed by anonymous memory.
    ///
    /// Tests on a host with no kernel VM need real `lookup` boundaries.
    /// This is the multi-region form of [`Self::new_anon`].
    ///
    /// A region on its own gets one buffer for both views, as they
    /// alias the same pages on a real VM. A region that joins a run
    /// needs a second buffer for the run, which a real VM does not:
    /// there the run mapping covers the same pages again.
    #[doc(hidden)]
    pub fn add_region_anon(
        &self,
        gpa: u64,
        len: usize,
        kind: RegionKind,
    ) -> Result<()> {
        let publish = self.publish_gate();
        let prot = guest_prot_for(kind);
        let claim = self.claim(gpa, len, kind, prot, Placement::Firmware)?;
        let direct = Arc::new(Mapping::anon(len)?);
        let (run_gpa, run_len) = self.run_bounds(gpa, len, kind);
        let tracked = if (run_gpa, run_len) == (gpa, len) {
            TrackedRun {
                gpa,
                mapping: Arc::clone(&direct),
                prot,
            }
        } else {
            TrackedRun {
                gpa: run_gpa,
                mapping: Arc::new(Mapping::anon(run_len)?),
                prot,
            }
        };
        self.publish_claim(publish, claim, RegionViews { tracked, direct });
        Ok(())
    }

    /// Publish an anonymous region whose two views do not alias.
    ///
    /// Only this makes the two views tell apart without a kernel VM. A
    /// real VM maps the same pages twice, so nothing outside a test may
    /// depend on the views holding different bytes.
    #[cfg(test)]
    fn add_region_split_anon(&self, gpa: u64, len: usize) -> Result<()> {
        let publish = self.publish_gate();
        let claim = self.claim(
            gpa,
            len,
            RegionKind::Ram,
            Prot::RWX,
            Placement::Firmware,
        )?;
        self.publish_claim(
            publish,
            claim,
            RegionViews {
                tracked: TrackedRun {
                    gpa,
                    mapping: Arc::new(Mapping::anon(len)?),
                    prot: Prot::RWX,
                },
                direct: Arc::new(Mapping::anon(len)?),
            },
        );
        Ok(())
    }

    /// Turn one claim into a published region, in one write-lock section.
    ///
    /// The range was checked when the claim was taken and the claim has
    /// held it since, so there is nothing left to refuse here. The list
    /// stays sorted by GPA for the binary search in `lookup`.
    ///
    /// Takes the publish gate the caller built the run under, so no
    /// other add can have grown the run in between.
    fn publish_claim(
        &self,
        _publish: std::sync::MutexGuard<'_, ()>,
        mut claim: RamClaim<'_>,
        views: RegionViews,
    ) {
        let mut space = self.write_space();
        if let Some(idx) =
            space.claims.iter().position(|&(c, _)| c == claim.gpa)
        {
            space.claims.swap_remove(idx);
        }
        claim.live = false;

        let insert_idx = space
            .regions
            .binary_search_by_key(&claim.gpa, |r| r.gpa)
            .unwrap_or_else(|idx| idx);

        space.regions.insert(
            insert_idx,
            Region {
                gpa: claim.gpa,
                len: claim.len,
                kind: claim.kind,
                tracked: views.tracked.clone(),
                direct: views.direct,
            },
        );
        Self::adopt_run(&mut space, &views.tracked);
    }

    /// Hold off any other publish. Always taken before the region lock.
    fn publish_gate(&self) -> std::sync::MutexGuard<'_, ()> {
        self.publish.lock().expect("physmap publish lock poisoned")
    }

    /// The guest addresses one tracked mapping has to cover for a new
    /// region at `[gpa, gpa + len)`.
    ///
    /// The caller holds the publish gate, so the answer stays true
    /// until it publishes. See [`TrackedRun`].
    fn run_bounds(
        &self,
        gpa: u64,
        len: usize,
        kind: RegionKind,
    ) -> (u64, usize) {
        if kind != RegionKind::Ram {
            return (gpa, len);
        }

        let space = self.read_space();
        let mut start = gpa;
        let mut end = gpa + len as u64;
        let ram = |r: &&Region| r.kind == RegionKind::Ram;

        // Regions do not overlap, so at most one meets each end, and
        // the walk gives up ground it can never take back.
        while let Some(below) =
            space.regions.iter().filter(ram).find(|r| r.end() == start)
        {
            start = below.gpa;
        }
        while let Some(above) =
            space.regions.iter().filter(ram).find(|r| r.gpa == end)
        {
            end = above.end();
        }

        // `end - start` spans published regions plus this claim, and
        // every one of them was checked against `usize` when it was
        // taken.
        (start, (end - start) as usize)
    }

    /// Give every region of `run` the run's mapping.
    ///
    /// The regions inside the bounds are exactly the ones the run was
    /// built from, because the caller holds the publish gate.
    fn adopt_run(space: &mut AddressSpace, run: &TrackedRun) {
        let end = run.end();
        for region in space.regions.iter_mut() {
            if region.gpa >= run.gpa && region.end() <= end {
                region.tracked = run.clone();
            }
        }
    }

    fn read_space(&self) -> std::sync::RwLockReadGuard<'_, AddressSpace> {
        self.space.read().expect("physmap regions lock poisoned")
    }

    fn write_space(&self) -> std::sync::RwLockWriteGuard<'_, AddressSpace> {
        self.space.write().expect("physmap regions lock poisoned")
    }

    /// Look up the region containing a guest physical address and return
    /// a lifetime-bounded [`SubMapping`] into it.
    ///
    /// The returned SubMapping is offset to start at `gpa` with length
    /// `len`. Returns `None` if no region contains the full range.
    ///
    /// This is the view every device gets. Writes through it are
    /// visible to the kernel's dirty page tracking, which is what live
    /// migration reads. See the module comment.
    ///
    /// A range that runs from one RAM region into the next is answered,
    /// because both share one mapping. See [`TrackedRun`].
    ///
    /// The view carries the guest's protection, so a device cannot DMA
    /// into a ROM. Propolis refuses the same in `region_covered`.
    pub fn lookup(&self, gpa: u64, len: usize) -> Option<SubMapping> {
        let end = gpa.checked_add(len as u64)?;

        // The guard is released when this call returns: `SubMapping` owns
        // an `Arc<Mapping>` and borrows nothing from the region list.
        let space = self.read_space();
        let region = Self::region_at(&space.regions, gpa)?;
        let run = &region.tracked;

        if end > run.end() {
            return None;
        }

        let offset = (gpa - run.gpa) as usize;
        SubMapping::new(&run.mapping)
            .subregion(offset, len)
            .map(|view| view.restrict(run.prot))
    }

    /// Look up a range in the mapping of the memory segment itself.
    ///
    /// The kernel does not see writes through this view, so a live
    /// migration would leave the page behind. Only for writes that
    /// finish before any vCPU runs, and for reads, which must not
    /// disturb the dirty bitmap. Propolis calls this pair
    /// `direct_writable_region` and `direct_readable_region`.
    ///
    /// This view is one kernel memory segment, so unlike [`Self::lookup`]
    /// it stops at the end of the region.
    pub fn lookup_untracked(&self, gpa: u64, len: usize) -> Option<SubMapping> {
        let end = gpa.checked_add(len as u64)?;

        let space = self.read_space();
        let region = Self::region_at(&space.regions, gpa)?;

        if end > region.end() {
            return None;
        }

        let offset = (gpa - region.gpa) as usize;
        SubMapping::new(&region.direct).subregion(offset, len)
    }

    /// The region holding `gpa`, by binary search over the sorted list.
    fn region_at(regions: &[Region], gpa: u64) -> Option<&Region> {
        let idx = regions
            .binary_search_by(|r| {
                if gpa < r.gpa {
                    std::cmp::Ordering::Greater
                } else if gpa >= r.end() {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .ok()?;
        regions.get(idx)
    }

    /// Get a summary of all memory regions (for migration/diagnostics).
    pub fn regions(&self) -> Vec<(u64, usize, RegionKind)> {
        self.read_space()
            .regions
            .iter()
            .map(|r| (r.gpa, r.len, r.kind))
            .collect()
    }

    /// Total mapped memory in bytes (excluding MMIO reservations).
    pub fn total_memory(&self) -> usize {
        self.read_space()
            .regions
            .iter()
            .filter(|r| r.kind != RegionKind::Mmio)
            .map(|r| r.len)
            .sum()
    }

    pub fn num_regions(&self) -> usize {
        self.read_space().regions.len()
    }
}

/// A held range of the guest physical address space.
///
/// Nothing is allocated for it yet. Drop it to give the range back, or
/// [`commit`](Self::commit) it once the kernel segment exists.
#[must_use = "a dropped claim gives the range back"]
pub struct RamClaim<'a> {
    map: &'a PhysMap,
    gpa: u64,
    len: usize,
    kind: RegionKind,
    guest_prot: Prot,
    live: bool,
}

impl std::fmt::Debug for RamClaim<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RamClaim")
            .field("gpa", &format_args!("{:#x}", self.gpa))
            .field("len", &self.len)
            .field("kind", &self.kind)
            .finish()
    }
}

impl RamClaim<'_> {
    /// The guest address this claim holds.
    pub fn gpa(&self) -> u64 {
        self.gpa
    }

    /// Map `segid` at the claimed address and publish the region.
    ///
    /// `segoff` is the offset into the segment, which lets one segment
    /// back several regions.
    pub fn commit(self, hdl: &VmmHdl, segid: i32, segoff: i64) -> Result<()> {
        if segoff < 0 || !(segoff as u64).is_multiple_of(PAGE_SIZE as u64) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("segment offset {segoff} is not a page offset"),
            ));
        }
        let (map, gpa, len) = (self.map, self.gpa, self.len);

        // Held until the regions adopt the run, so no other add can
        // grow it in between.
        let publish = map.publish_gate();
        let (run_gpa, run_len) = map.run_bounds(gpa, len, self.kind);

        // The tracked mapping comes before the kernel mapping. The
        // kernel faults its pages in one at a time on first use, so a
        // mapping of guest addresses that are not backed yet is legal,
        // and one that cannot be made costs nothing to abandon.
        let tracked =
            tracked_view(hdl, run_gpa, run_len, self.kind, self.guest_prot)?;

        hdl.map_memseg(segid, gpa, len, segoff, self.guest_prot)?;

        // The guest can reach the memory from here on, so a failure has
        // to take the mapping back out. A guest mapping the physical map
        // does not know about would be handed out a second time.
        match devmem_view(hdl, segid, segoff, len) {
            Ok(direct) => {
                // A region the guest cannot write has nothing to track.
                // It keeps one view, with the guest's protection on it
                // so that device DMA cannot write what the guest cannot.
                // The bootrom loader writes through `lookup_untracked`.
                let tracked = tracked.unwrap_or_else(|| TrackedRun {
                    gpa,
                    mapping: Arc::clone(&direct),
                    prot: self.guest_prot,
                });
                map.publish_claim(
                    publish,
                    self,
                    RegionViews { tracked, direct },
                );
                Ok(())
            }
            Err(e) => match hdl.munmap_memseg(gpa, len) {
                Ok(()) => Err(e),
                Err(unmap) => {
                    // The guest still reaches memory with no region
                    // behind it. Keeping the claim for the life of the
                    // VM stops the range being handed out again.
                    std::mem::forget(self);
                    Err(Error::other(format!(
                        "{e}; the guest mapping at {gpa:#x} could not be \
                         taken back ({unmap}), so the range is spent",
                    )))
                }
            },
        }
    }
}

/// Map the memory segment itself into this process.
///
/// The kernel does not track writes through this mapping. See the
/// module comment.
fn devmem_view(
    hdl: &VmmHdl,
    segid: i32,
    segoff: i64,
    len: usize,
) -> Result<Arc<Mapping>> {
    let base = hdl.devmem_offset(segid)?;
    let offset = base.checked_add(segoff).ok_or_else(|| {
        Error::new(ErrorKind::InvalidInput, "device memory offset overflows")
    })?;
    Ok(Arc::new(Mapping::new(len, Prot::RW, hdl, offset)?))
}

/// Map a run of the guest physical address space into this process.
///
/// This is the view the kernel tracks. `None` for a region the guest
/// cannot write: there is nothing to track.
fn tracked_view(
    hdl: &VmmHdl,
    gpa: u64,
    len: usize,
    kind: RegionKind,
    guest_prot: Prot,
) -> Result<Option<TrackedRun>> {
    if kind != RegionKind::Ram {
        return Ok(None);
    }
    let offset = i64::try_from(gpa).map_err(|_| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("guest address {gpa:#x} exceeds i64"),
        )
    })?;
    Ok(Some(TrackedRun {
        gpa,
        mapping: Arc::new(Mapping::new(len, guest_prot, hdl, offset)?),
        prot: guest_prot,
    }))
}

/// The protection the guest gets on a region of `kind`.
fn guest_prot_for(kind: RegionKind) -> Prot {
    match kind {
        RegionKind::Rom => Prot::READ | Prot::EXEC,
        RegionKind::Ram | RegionKind::Mmio => Prot::RWX,
    }
}

impl Drop for RamClaim<'_> {
    fn drop(&mut self) {
        if self.live {
            self.map.release_claim(self.gpa);
        }
    }
}

/// The kernel name of a memory segment.
fn seg_name(segid: i32) -> String {
    format!("seg{segid}")
}

/// Validate a region request and return its exclusive end GPA.
fn region_end(gpa: u64, len: usize) -> Result<u64> {
    if len == 0 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "region length must be non-zero",
        ));
    }
    let len = u64::try_from(len).map_err(|_| {
        Error::new(ErrorKind::InvalidInput, "region length exceeds u64")
    })?;
    gpa.checked_add(len).ok_or_else(|| {
        Error::new(ErrorKind::InvalidInput, "region overflows address space")
    })
}

/// The error a range that is already spoken for gets.
fn overlaps(gpa: u64, end: u64, o_gpa: u64, o_end: u64, what: &str) -> Error {
    Error::new(
        ErrorKind::AlreadyExists,
        format!(
            "region [{gpa:#x}, {end:#x}) overlaps with {what} \
             [{o_gpa:#x}, {o_end:#x})",
        ),
    )
}

#[cfg(test)]
mod tests;
