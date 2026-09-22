// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The order backend work keeps across a ring retirement.
//!
//! Permission to touch guest memory is `crate::access::GuestAccess`,
//! shared with virtio-fs and virtio-vsock. Only virtio-blk needs what
//! is here: it runs several workers per queue, so a write admitted
//! under one driver can still overtake the next driver's write to the
//! same sector.

use std::sync::{Condvar, Mutex, PoisonError};

use crate::access::{GuestAccess, RingTag};

/// Keeps backend work in retirement order.
///
/// A write already inside `pwrite` when a ring is retired still
/// reaches the disk. It reads a host snapshot, so it cannot touch
/// guest memory, but it can still overtake a write the next driver
/// sends to the same sector. New work waits behind those.
///
/// The order is device-wide even though generations are per queue.
/// VirtIO promises no order between queues while a driver runs, but
/// the driver on either side of a retirement is a different one, and a
/// write from the old one must not land on top of the new one's.
///
/// This never gates the retirement itself. A disk that never answers
/// must be able to stop I/O progress while the reset still returns.
pub(super) struct BackendBarrier {
    state: Mutex<BarrierState>,
    settled: Condvar,
}

struct BarrierState {
    /// Bumped by every retirement. A ticket carries the value it was
    /// admitted under, so it comes off the count it went onto.
    epoch: u64,
    /// Running operations that change the disk and were admitted since
    /// the last retirement.
    live_writes: usize,
    /// Running operations that change the disk and were admitted
    /// before it. New work waits for these.
    stale_writes: usize,
}

/// One admitted backend operation, counted until it is dropped.
pub(super) struct Ticket<'a> {
    barrier: &'a BackendBarrier,
    epoch: u64,
    writes: bool,
}

impl BackendBarrier {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(BarrierState {
                epoch: 0,
                live_writes: 0,
                stale_writes: 0,
            }),
            settled: Condvar::new(),
        }
    }

    /// Admit one backend operation for `tag`.
    ///
    /// `writes` marks an operation that changes the disk, which is the
    /// only kind a later operation must wait for. Returns `None` for
    /// work whose ring has been retired, and the wait is re-tested
    /// against that on every wake, so a request never sits here on a
    /// generation the guest has already replaced.
    ///
    /// `access` is read rather than entered: this wait is unbounded,
    /// and a caller holding guest-access permission across it would
    /// park the vCPU that drains for a whole disk operation.
    pub(super) fn admit(
        &self,
        access: &GuestAccess,
        tag: RingTag,
        writes: bool,
    ) -> Option<Ticket<'_>> {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if !access.is_current(tag) {
                return None;
            }
            if st.stale_writes == 0 {
                break;
            }
            st = self
                .settled
                .wait(st)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if writes {
            st.live_writes += 1;
        }
        Some(Ticket {
            barrier: self,
            epoch: st.epoch,
            writes,
        })
    }

    /// End the current epoch. Operations still running become the set
    /// later work waits for.
    ///
    /// Call this from every retirement, a single reprogrammed queue
    /// included: the generation check in [`Self::admit`] is what
    /// decides whose work is cancelled, and this only decides what
    /// order the disk sees.
    pub(super) fn retire(&self) {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        st.epoch += 1;
        st.stale_writes += st.live_writes;
        st.live_writes = 0;
        // Wake the waiters that must now give up: their generation may
        // have ended even if nothing stale is left.
        self.settled.notify_all();
    }
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        if !self.writes {
            return;
        }
        let mut st = self
            .barrier
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if st.epoch == self.epoch {
            st.live_writes -= 1;
        } else {
            st.stale_writes -= 1;
            if st.stale_writes == 0 {
                self.barrier.settled.notify_all();
            }
        }
    }
}
