// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Kernel memory-segment IDs.

use std::sync::atomic::{AtomicI32, Ordering};

/// `VM_MAX_MEMSEGS` from illumos `uts/intel/io/vmm/vmm.c`.
pub const VM_MAX_MEMSEGS: i32 = 5;

/// Hands out kernel memory-segment IDs after the [`super::PhysMap`] is
/// sealed.
///
/// This does not live in `PhysMap` because `MachineSetup::finalize` freezes
/// that map before devices are constructed.
pub struct SegidAlloc {
    next: AtomicI32,
}

impl SegidAlloc {
    /// Start allocating at the next unused segment ID from `PhysMap`.
    pub fn new(start: i32) -> Self {
        Self {
            next: AtomicI32::new(start),
        }
    }

    /// Give back an ID whose segment was never created.
    ///
    /// Only the ID handed out last can come back, which is all a failed
    /// allocation needs. Reports whether it was taken: another caller
    /// may have allocated in between, and then the ID stays spent.
    ///
    /// Never call this for a segment `VM_ALLOC_MEMSEG` created. The
    /// kernel has no ioctl that frees one, so reusing the ID would
    /// answer `EEXIST` or, worse, hand out memory that is already in
    /// use.
    pub fn release(&self, segid: i32) -> bool {
        let Some(next) = segid.checked_add(1) else {
            return false;
        };
        self.next
            .compare_exchange(next, segid, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }

    /// Allocate a segment ID, or return `None` at the kernel limit.
    ///
    /// Reporting exhaustion here gives startup a useful error instead
    /// of an unexplained `EINVAL` from `VM_ALLOC_MEMSEG`.
    pub fn alloc(&self) -> Option<i32> {
        self.next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                if next < VM_MAX_MEMSEGS {
                    Some(next + 1)
                } else {
                    None
                }
            })
            .ok()
    }
}
