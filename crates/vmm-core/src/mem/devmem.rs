// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A memory segment whose guest mapping is under device control.

use std::io::Result;
use std::sync::{Arc, Mutex};

use crate::hdl::VmmHdl;

use super::mapping::{Mapping, Prot, SubMapping};

/// A named bhyve memory segment whose guest mapping is under device control.
///
/// A device BAR backed by this segment is served by EPT without an MMIO exit.
/// This is required for wide stores because the kernel MMIO exit carries only
/// one `u64` and cannot represent a 16-byte framebuffer store.
pub struct DevMemSeg {
    segid: i32,
    len: usize,
    host: Arc<Mapping>,
    hdl: Option<Arc<VmmHdl>>,
    mapped_gpa: Mutex<Option<u64>>,
}

impl DevMemSeg {
    /// Allocate the segment and map only its host view.
    pub fn new(
        hdl: Arc<VmmHdl>,
        segid: i32,
        name: &str,
        len: usize,
    ) -> Result<Self> {
        hdl.create_memseg(segid, len, name)?;
        let offset = hdl.devmem_offset(segid)?;
        let host = Arc::new(Mapping::new(len, Prot::RW, &hdl, offset)?);

        Ok(Self {
            segid,
            len,
            host,
            hdl: Some(hdl),
            mapped_gpa: Mutex::new(None),
        })
    }

    /// Create anonymous backing without a guest for unit tests.
    ///
    /// [`Self::map_at`] and [`Self::unmap`] are no-ops for this segment.
    #[doc(hidden)]
    pub fn new_anon(len: usize) -> Result<Self> {
        Ok(Self {
            segid: -1,
            len,
            host: Arc::new(Mapping::anon(len)?),
            hdl: None,
            mapped_gpa: Mutex::new(None),
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Return a guest-visible view of the host mapping.
    pub fn view(&self) -> SubMapping {
        SubMapping::new(&self.host)
    }

    /// Return the GPA where the segment is currently mapped, if any.
    pub fn mapped_gpa(&self) -> Option<u64> {
        *self
            .mapped_gpa
            .lock()
            .expect("devmem mapping lock poisoned")
    }

    /// Map the segment into the guest physical address space.
    ///
    /// Mapping at the current GPA is idempotent. Kernel mapping errors are
    /// returned because a guest-selected GPA may collide with another region.
    pub fn map_at(&self, gpa: u64) -> Result<()> {
        let Some(hdl) = &self.hdl else {
            return Ok(());
        };

        let mut mapped = self
            .mapped_gpa
            .lock()
            .expect("devmem mapping lock poisoned");
        if *mapped == Some(gpa) {
            return Ok(());
        }

        hdl.map_memseg(self.segid, gpa, self.len, 0, Prot::RW)?;
        *mapped = Some(gpa);
        Ok(())
    }

    /// Remove the segment's current guest mapping, if any.
    ///
    /// The recorded GPA and length are reused because the kernel requires the
    /// pair to match the original mapping exactly.
    pub fn unmap(&self) -> Result<()> {
        let Some(hdl) = &self.hdl else {
            return Ok(());
        };

        let Some(gpa) = self
            .mapped_gpa
            .lock()
            .expect("devmem mapping lock poisoned")
            .take()
        else {
            return Ok(());
        };

        hdl.munmap_memseg(gpa, self.len)
    }
}
