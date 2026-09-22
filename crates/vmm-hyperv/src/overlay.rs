// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest-physical "overlay" pages.
//!
//! Some Hyper-V MSRs (HYPERCALL, REFERENCE_TSC) carry a guest physical
//! page number. The hypervisor must fill that page with a 4 KiB
//! payload. TLFS requires the original guest contents back when the
//! guest disables the overlay or moves it to a different page.
//!
//! A smaller alternative to Propolis' `OverlayManager`: one independent
//! overlay per kind (hypercall, reference TSC). Two kinds at the same
//! PFN do not stack. TLFS allows that only as a guest bug, and here the
//! latest writer wins.

use std::sync::Arc;

use vmm_core::mem::PhysMap;

pub use vmm_core::common::PAGE_SIZE;

/// One installed overlay. `original` holds the 4 KiB the guest had
/// there, for restore on remove or relocate.
#[derive(Clone)]
pub struct Overlay {
    pub gpa: u64,
    pub original: Box<[u8; PAGE_SIZE]>,
}

/// Install `payload` at `gpa`, returning the saved original page.
///
/// Uses volatile access through `SubMapping`, so concurrent vCPU
/// accesses are well-defined.
pub fn install(
    physmap: &Arc<PhysMap>,
    gpa: u64,
    payload: &[u8; PAGE_SIZE],
) -> Result<Overlay, OverlayError> {
    if gpa & (PAGE_SIZE as u64 - 1) != 0 {
        return Err(OverlayError::Misaligned(gpa));
    }
    let map = physmap
        .lookup(gpa, PAGE_SIZE)
        .ok_or(OverlayError::Unmapped(gpa))?;

    let mut original = Box::new([0u8; PAGE_SIZE]);
    map.read_bytes(original.as_mut_slice())
        .map_err(OverlayError::Io)?;
    map.write_bytes(payload).map_err(OverlayError::Io)?;

    Ok(Overlay { gpa, original })
}

/// Restore an overlay's original page contents and discard it.
///
/// Best effort: if the GPA is no longer mapped, the restore is skipped.
/// No guest state is left there to corrupt.
pub fn remove(physmap: &Arc<PhysMap>, ov: &Overlay) {
    if let Some(map) = physmap.lookup(ov.gpa, PAGE_SIZE) {
        let _ = map.write_bytes(ov.original.as_slice());
    }
}

/// Restore the old page, then install at `new_gpa`.
pub fn relocate(
    physmap: &Arc<PhysMap>,
    old: Overlay,
    new_gpa: u64,
    payload: &[u8; PAGE_SIZE],
) -> Result<Overlay, OverlayError> {
    remove(physmap, &old);
    install(physmap, new_gpa, payload)
}

#[derive(Debug, thiserror::Error)]
pub enum OverlayError {
    #[error("overlay GPA {0:#x} is not page-aligned")]
    Misaligned(u64),
    #[error("overlay GPA {0:#x} is not mapped to guest RAM")]
    Unmapped(u64),
    #[error("guest memory I/O: {0}")]
    Io(#[from] std::io::Error),
}
