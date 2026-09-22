// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Release of the in-kernel link before the VM is destroyed.

use std::io::PipeWriter;
use std::thread::JoinHandle;

use vmm_devices::Lifecycle;

use viona_api::LinkOps;

use super::{RingState, VirtioViona, NET_NUM_QUEUES};

/// The interrupt poll thread, while one is running.
pub(super) struct Poller {
    /// Write end of the pipe that stops the thread.
    ///
    /// The thread waits in `poll(2)` on the viona descriptor with no
    /// timeout, so only a second descriptor can wake it. Closing this
    /// end makes that descriptor ready.
    pub(super) wake: PipeWriter,
    pub(super) thread: JoinHandle<()>,
}

/// Wake a running poll thread and join it.
pub(super) fn stop_poller(poller: Option<Poller>, log: &slog::Logger) {
    let Some(poller) = poller else {
        return;
    };
    drop(poller.wake);
    if poller.thread.join().is_err() {
        slog::error!(log, "the viona interrupt poll thread panicked");
    }
}

/// Reset every ring, then destroy the link.
///
/// `viona_ioc_delete` resets the rings itself, in a wait that ignores
/// signals. `viona_ring_reset` returns at once for a ring that is
/// already reset, so each reset here removes one wait from the delete.
/// Nothing in this process signals the teardown thread, so this order
/// does not make the halt interruptible.
///
/// A ring reset failure is logged and the loop continues: teardown must
/// reach the delete. The result is the delete's. The delete releases
/// the `vmm_drv` hold, and it is the one call that proves every ring
/// stopped.
pub(super) fn halt_link(
    link: &dyn LinkOps,
    log: &slog::Logger,
) -> std::io::Result<()> {
    for ring in 0..NET_NUM_QUEUES as u16 {
        if let Err(e) = link.ring_reset(ring) {
            slog::warn!(log, "viona ring reset failed during halt";
                "ring" => ring, "error" => %e);
        }
    }

    link.delete()
}

impl VirtioViona {
    /// Stop the poll thread and destroy the link, once.
    ///
    /// The result is the delete's. `VNA_IOC_DELETE` (`viona_ioc_delete`)
    /// resets every ring in a wait that ignores signals and cannot fail
    /// once it starts. So `Ok` proves no kernel thread uses the guest
    /// ring pages.
    pub(super) fn destroy_link(&self) -> std::io::Result<()> {
        let (link, log, poller) = {
            let mut inner = self.inner.lock().expect("viona lock poisoned");
            // Viona treats a second delete as a no-op, but it repeats
            // the wait inside the delete.
            if inner.halted {
                return Ok(());
            }
            inner.halted = true;
            // A deferred start after the destroy would poll a dead link.
            inner.deferred_poll = None;
            // Set before the lock is dropped, so a kick that races the
            // delete finds no ready ring.
            inner.ring_state = [RingState::Init; NET_NUM_QUEUES];
            inner.negotiated_features = 0;
            (inner.link(), inner.log(), inner.poller.take())
        };

        // The poll thread reads the viona descriptor and calls into the
        // PCI transport, so it must stop before the link is destroyed.
        // The join also needs the device lock dropped: the thread can be
        // in that callback, and it takes transport locks.
        stop_poller(poller, &log);
        halt_link(link.as_ref(), &log)
    }

    /// Stop the kernel the one way that cannot fail, after a ring
    /// refused to reset.
    ///
    /// For a ring viona holds, `viona_ring_reset` returns only 0 or
    /// `EINTR`, and `EINTR` is retried. Any other error means the link
    /// is closing: `viona_ioctl` returns `ENXIO` for a destroyed link or
    /// a VM hold marked for release. `VNA_IOC_DELETE` is exempt from
    /// that check and stops every ring, so it is the only call left
    /// that proves the guest can take back its ring pages.
    ///
    /// The caller waits for that delete, so the guest never sees the
    /// ring as free while the kernel holds it. The NIC is dead after,
    /// which the guest can detect and survive.
    pub(super) fn destroy_stopped_ring(&self, ring: u16, err: &std::io::Error) {
        let log = self.inner.lock().expect("viona lock poisoned").log();
        slog::error!(log, "viona ring reset refused; destroying the link";
            "ring" => ring, "error" => %err);
        let Err(e) = self.destroy_link() else {
            return;
        };
        // Nothing else can stop the ring, and the guest reclaims the
        // pages when this returns. Process exit stops the kernel:
        // `viona_close` resets every ring in a wait that ignores signals.
        slog::error!(log, "viona link delete refused; the kernel may still \
            reach the guest ring pages, so this process cannot continue";
            "error" => %e);
        std::process::abort();
    }
}

impl Lifecycle for VirtioViona {
    fn type_name(&self) -> &'static str {
        "virtio-viona"
    }

    /// Release the in-kernel link before the VM is destroyed.
    ///
    /// Viona takes a `vmm_drv` hold for the life of its link.
    /// `VM_DESTROY_SELF` marks every hold for release, then
    /// `vmm_drv_purge` waits for the leases to break in an untimed
    /// `cv_wait`. Releasing the last hold first clears `VMM_HELD`, and
    /// the purge skips that wait. The order follows Propolis: stop the
    /// poller, reset the rings, then delete the link.
    ///
    /// This call has no time limit. The delete waits for each ring
    /// worker to stop and ignores signals, so it must run on the
    /// teardown thread that the shutdown watchdog times.
    ///
    /// A hot unplug also halts, with the guest running
    /// (`vmm_devices::lifecycle::prepare_unplug`). So the device lock is
    /// dropped first: `cfg_read`, `notify_queue`, `queue_addr_set` and
    /// `reset` take the same lock, and a vCPU in one of them must not
    /// wait for an untimed kernel call.
    fn halt(&self) {
        let Err(e) = self.destroy_link() else {
            return;
        };
        // An error, not a warning: VM_DESTROY_SELF waits for the hold
        // this call releases, so a failed delete predicts a stuck
        // destroy. Teardown continues because it must reach
        // VM_DESTROY_SELF.
        let log = self.inner.lock().expect("viona lock poisoned").log();
        slog::error!(log, "viona link delete failed; the VM destroy may wait \
            for its hold";
            "error" => %e);
    }
}
