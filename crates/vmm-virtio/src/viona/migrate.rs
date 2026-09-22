// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ring state a migration reads out and puts back.
//!
//! The source pauses each ring and reads its indices. The destination
//! programs the addresses and the indices together.
//! [`super::VirtioViona::ring_init`] cannot, because it sets the indices
//! to 0.

use vmm_devices::lifecycle::DeviceStateError;

use super::{PausedRing, RingState, VirtioViona, NET_NUM_QUEUES};

impl VirtioViona {
    /// Stop every ring worker so the indices stay constant, and keep
    /// each ring's state for a rollback.
    ///
    /// A ring that did not pause keeps consuming available entries while
    /// the export reads its indices, so the failure fails the migration.
    pub(super) fn pause_rings(&self) -> Result<(), DeviceStateError> {
        let mut inner = self.inner.lock().expect("viona lock");
        // The halt drops the device lock during an untimed kernel call.
        // A pause here would wait for that call while it holds the lock
        // every vCPU needs.
        if inner.halted {
            return Ok(());
        }
        for ring in 0..NET_NUM_QUEUES as u16 {
            if let Err(e) = inner.link_ref().ring_pause(ring) {
                return Err(DeviceStateError::Export(format!(
                    "viona ring {ring} pause failed: {e}"
                )));
            }
            // Read after the pause. A running ring moves both cursors, so
            // a rollback from an earlier read programs cursors the kernel
            // already passed and replays the descriptors between them.
            let idx = usize::from(ring);
            match inner.link_ref().ring_get_state(ring) {
                Ok(state) => inner.paused_rings[idx] = PausedRing::Saved(state),
                Err(e) => {
                    // Nothing can restart the ring: a paused ring needs a
                    // reset and a state write, and the kernel refused the
                    // state.
                    inner.paused_rings[idx] = PausedRing::Lost;
                    return Err(DeviceStateError::Export(format!(
                        "viona ring {ring} state read failed: {e}"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Restart the rings that [`Self::pause_rings`] stopped.
    ///
    /// A paused ring is `VRS_STOP` in the kernel, and
    /// `viona_ioc_ring_kick` returns EBUSY for it, so a guest kick cannot
    /// restart it. Only a reset and a state write can. This writes the
    /// state that the pause saved.
    pub(super) fn resume_rings(&self) -> Result<(), DeviceStateError> {
        let mut inner = self.inner.lock().expect("viona lock");
        if inner.halted {
            return Ok(());
        }
        let mut failed: Vec<String> = Vec::new();
        for ring in 0..NET_NUM_QUEUES as u16 {
            let idx = usize::from(ring);
            let state = match std::mem::take(&mut inner.paused_rings[idx]) {
                PausedRing::Running => continue,
                // No saved state, so the source guest keeps a dead ring.
                PausedRing::Lost => {
                    failed.push(format!("ring {ring} has no saved state"));
                    continue;
                }
                PausedRing::Saved(state) => state,
            };
            // The guest ran between the pause and the abort and can have
            // reset the device. Then the driver programs the rings again.
            if inner.ring_state[idx] != RingState::Ready {
                continue;
            }
            if let Err(e) = inner.link_ref().ring_reset(ring) {
                failed.push(format!("ring {ring} reset: {e}"));
                continue;
            }
            if let Err(e) = inner.link_ref().ring_set_state(&state) {
                failed.push(format!("ring {ring} set state: {e}"));
                continue;
            }
            Self::ring_kick(&mut inner, ring);
        }
        if failed.is_empty() {
            return Ok(());
        }
        Err(DeviceStateError::Invalid(format!(
            "viona rings stayed paused: {}",
            failed.join("; ")
        )))
    }

    /// Pause and reset all viona rings. Call before `ring_set_state`.
    ///
    /// A ring the kernel still runs would read its addresses while the
    /// restore writes new ones, so a refusal fails the migration.
    pub fn reset_all_rings(&self) -> Result<(), DeviceStateError> {
        let inner = self.inner.lock().expect("viona lock");
        // The halt already reset every ring, and it holds no device lock
        // while it destroys the link.
        if inner.halted {
            return Ok(());
        }
        for ring in 0..NET_NUM_QUEUES as u16 {
            if let Err(e) = inner.link_ref().ring_pause(ring) {
                return Err(DeviceStateError::Invalid(format!(
                    "viona ring {ring} pause failed: {e}"
                )));
            }
            if let Err(e) = inner.link_ref().ring_reset(ring) {
                return Err(DeviceStateError::Invalid(format!(
                    "viona ring {ring} reset failed: {e}"
                )));
            }
        }
        Ok(())
    }

    /// Restore ring state after migration.
    ///
    /// Programs the ring addresses and indices into the kernel.
    /// `ring_init` sets the indices to 0.
    ///
    /// Every failure goes to the caller. The source commits the
    /// migration on this result and discards its VM, so a false success
    /// leaves the guest a NIC that nothing can repair.
    pub fn ring_set_state(
        &self,
        queue_idx: u16,
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
        avail_idx: u16,
        used_idx: u16,
        msix_addr: u64,
        msix_data: u32,
    ) -> Result<(), DeviceStateError> {
        let mut inner = self.inner.lock().expect("viona lock");
        // A restore that races the halt must not program a destroyed
        // link.
        if inner.halted {
            return Err(DeviceStateError::Invalid(format!(
                "viona ring {queue_idx} restore after the link was destroyed"
            )));
        }
        if !indices_hold(size, avail_idx, used_idx) {
            return Err(DeviceStateError::Invalid(format!(
                "viona ring {queue_idx} indices cannot come from a running \
                 ring: size {size}, avail_idx {avail_idx}, used_idx {used_idx}"
            )));
        }

        // VERSION_1 sets `l_modern` in the kernel.
        if inner.negotiated_features != 0 {
            let features = inner.negotiated_features;
            Self::set_kernel_features(&inner, features).map_err(|e| {
                // VERSION_1 selects the modern ring layout that the
                // addresses below use. Without it the kernel reads the
                // wrong memory.
                DeviceStateError::Invalid(format!(
                    "viona features {features:#x} refused: {e}"
                ))
            })?;
        }

        // vrs_avail_idx sets vr_cur_aidx, the kernel consumption cursor.
        // The kernel computes:
        //   num_avail = guest_memory_avail_idx - vr_cur_aidx
        //
        // Use the source avail_idx, not used_idx. The guest driver can
        // have reclaimed the descriptors between used_idx and avail_idx,
        // and a replay makes the guest report "id N is not a head!".
        //
        // If avail_idx == used_idx, the guest driver posts buffers again
        // after the post-restore interrupt.
        let state = viona_api::vioc_ring_state {
            vrs_index: queue_idx,
            vrs_avail_idx: avail_idx,
            vrs_used_idx: used_idx,
            vrs_qsize: size,
            vrs_qaddr_desc: desc,
            vrs_qaddr_avail: avail,
            vrs_qaddr_used: used,
        };
        if let Err(e) = inner.link_ref().ring_set_state(&state) {
            return Err(DeviceStateError::Invalid(format!(
                "viona ring {queue_idx} set state failed: {e}"
            )));
        }

        let idx = queue_idx as usize;
        if idx < NET_NUM_QUEUES {
            inner.ring_state[idx] = RingState::Ready;
        }

        if msix_addr != 0 {
            let msi_rc = inner.link_ref().ring_set_msi(
                queue_idx,
                msix_addr,
                u64::from(msix_data),
            );
            if let Err(e) = msi_rc {
                return Err(DeviceStateError::Invalid(format!(
                    "viona ring {queue_idx} set msi failed: {e}"
                )));
            }
        }

        Self::ring_kick(&mut inner, queue_idx);
        let log = inner.log();
        slog::info!(log, "viona ring state restored";
            "ring" => queue_idx, "avail_idx" => avail_idx,
            "used_idx" => used_idx, "msi_addr" => msix_addr);
        Ok(())
    }

    /// Read one ring's `(avail_idx, used_idx)` for a migration export.
    ///
    /// Only the kernel has the indices. Viona never updates the indices
    /// in [`VirtQueue`], so a failure here fails the export.
    ///
    /// [`VirtQueue`]: crate::queue::VirtQueue
    pub fn kernel_ring_state(
        &self,
        queue_idx: u16,
    ) -> Result<(u16, u16), DeviceStateError> {
        let inner = self.inner.lock().expect("viona lock");
        // The halt holds no device lock while it destroys the link, so
        // an ioctl here could wait for an untimed kernel call with the
        // lock every vCPU needs.
        if inner.halted {
            return Err(DeviceStateError::Export(format!(
                "viona ring {queue_idx} state read after the link was \
                 destroyed"
            )));
        }
        match inner.link_ref().ring_get_state(queue_idx) {
            Ok(state) => Ok((state.vrs_avail_idx, state.vrs_used_idx)),
            Err(e) => Err(DeviceStateError::Export(format!(
                "viona ring {queue_idx} state read failed: {e}"
            ))),
        }
    }
}

/// Whether a migration payload's indices can have come from a running
/// ring.
///
/// `avail_idx` is the kernel consumption cursor and `used_idx` the
/// completion cursor, so their difference is the number of descriptor
/// chains in flight. That number cannot exceed the ring size. Both
/// cursors are wrapping u16 counters, so the difference wraps too.
///
/// The payload comes from an unauthenticated peer, so the pair can be
/// any value. Too many chains in flight make viona treat consumed
/// available entries as new and give the guest driver descriptors it
/// reclaimed. The driver reports "id N is not a head!".
fn indices_hold(size: u16, avail_idx: u16, used_idx: u16) -> bool {
    avail_idx.wrapping_sub(used_idx) <= size
}
