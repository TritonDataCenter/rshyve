// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Convenience access to guest memory through a shared [`PhysMap`].

use std::io::{Error, ErrorKind, Result};
use std::sync::Arc;

use super::{PhysMap, RegionKind, SubMapping};

/// Context for accessing guest memory.
///
/// Wraps a shared reference to the PhysMap and provides convenience
/// methods for reading/writing guest physical addresses.
///
/// All memory access is bounds-checked and uses volatile operations
/// appropriate for concurrently-mutable guest memory.
pub struct MemCtx {
    map: Arc<PhysMap>,
}

/// The error an unmapped range gets.
fn unmapped(gpa: u64, len: usize) -> Error {
    Error::new(
        ErrorKind::AddrNotAvailable,
        format!("GPA {gpa:#x} len {len} not mapped"),
    )
}

impl MemCtx {
    pub fn new(map: Arc<PhysMap>) -> Self {
        Self { map }
    }

    /// Copy bytes out of guest memory at the given GPA.
    ///
    /// Takes the untracked view. A read fault on the tracked mapping
    /// marks the page dirty, because `segvmm_fault_space` holds every
    /// page with the segment's protection, which is writable. The
    /// migration source reads all of RAM on each pass, so a tracked
    /// read would report all of RAM dirty again and the migration
    /// would never converge.
    ///
    /// Device emulation also reads through here, so mind the bound the
    /// untracked view carries: it stops at the end of one region, while
    /// [`PhysMap::lookup`] spans a run of abutting RAM. Every caller
    /// reads inside one page, and regions are page aligned, so none can
    /// meet a seam. A caller that needs more than a page at once must
    /// take [`Self::lookup`] instead.
    ///
    /// Returns an error if the GPA range is not mapped.
    pub fn read(&self, gpa: u64, buf: &mut [u8]) -> Result<()> {
        let sub = self
            .map
            .lookup_untracked(gpa, buf.len())
            .ok_or_else(|| unmapped(gpa, buf.len()))?;
        sub.read_bytes(buf)
    }

    /// Write bytes to guest memory at the given GPA.
    ///
    /// Returns an error if the GPA range is not mapped or not writable.
    pub fn write(&self, gpa: u64, data: &[u8]) -> Result<()> {
        let sub = self
            .map
            .lookup(gpa, data.len())
            .ok_or_else(|| unmapped(gpa, data.len()))?;
        sub.write_bytes(data)
    }

    /// Bulk write to guest memory at the given GPA.
    ///
    /// Uses [`SubMapping::copy_in`], so it carries that method's
    /// constraint: only for writes no vCPU can observe partially. Meant
    /// for the multi-megabyte boot-path writes, which all happen before
    /// the vCPU threads start. It therefore takes the untracked view:
    /// no migration can be running yet, and the devmem mapping is the
    /// cheaper one to fault in.
    pub fn write_bulk(&self, gpa: u64, data: &[u8]) -> Result<()> {
        let sub = self
            .map
            .lookup_untracked(gpa, data.len())
            .ok_or_else(|| unmapped(gpa, data.len()))?;
        sub.copy_in(data)
    }

    /// Load `len` bytes at `file_offset` in `file` into guest memory at
    /// `gpa`.
    ///
    /// The GPA range is validated as mapped and writable before any
    /// byte is read, so a segment header cannot steer the read outside
    /// guest RAM. Uses [`SubMapping::read_exact_from`] and carries its
    /// constraint: only before the vCPU threads start. Takes the
    /// untracked view for the reason given on [`Self::write_bulk`].
    pub fn load_from_file(
        &self,
        gpa: u64,
        file: &std::fs::File,
        file_offset: u64,
        len: usize,
    ) -> Result<()> {
        let sub = self
            .map
            .lookup_untracked(gpa, len)
            .ok_or_else(|| unmapped(gpa, len))?;
        sub.read_exact_from(file, file_offset, len)
    }

    /// Look up a GPA range and return a SubMapping for direct access.
    ///
    /// The returned SubMapping holds its mapping alive, so it cannot
    /// outlive the memory it points at.
    pub fn lookup(&self, gpa: u64, len: usize) -> Option<SubMapping> {
        self.map.lookup(gpa, len)
    }

    /// Look up a GPA range in the untracked view.
    ///
    /// See [`PhysMap::lookup_untracked`] for what a caller gives up.
    pub fn lookup_untracked(&self, gpa: u64, len: usize) -> Option<SubMapping> {
        self.map.lookup_untracked(gpa, len)
    }

    /// Get all memory regions for migration dirty page tracking.
    pub fn regions(&self) -> Vec<(u64, usize, RegionKind)> {
        self.map.regions()
    }
}
