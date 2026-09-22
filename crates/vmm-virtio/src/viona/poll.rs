// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The interrupt poll thread.
//!
//! Viona completes I/O in the kernel and signals readiness on the
//! device descriptor. One thread per device waits for that signal and
//! raises the guest interrupt. The halt stops the thread before it
//! destroys the link, because the thread reads the link.
//!
//! The wait is a link call ([`LinkOps::wait_intr`]), so a test can run
//! the loop with its own link.

use std::sync::Arc;

use viona_api::{IntrWait, LinkOps};

use super::{VirtioViona, NET_NUM_QUEUES};
use crate::pci::intr::IntrSlot;

// The poll thread raises one interrupt per ring, and the transport has
// one MSI-X vector per queue. A ring with no queue would raise on no
// vector: silence on MSI-X, and an INTx line that nothing lowers.
const _: () = assert!(viona_api::VIONA_VQ_MAX as usize == NET_NUM_QUEUES);

impl VirtioViona {
    /// Start the interrupt polling thread.
    ///
    /// The thread clears each pending ring interrupt and raises the
    /// per-queue interrupt (0=rx, 1=tx), so the PCI transport selects
    /// the MSI-X vector.
    pub fn start_intr_poll(&self, log: slog::Logger) {
        let mut inner = self.inner.lock().expect("viona lock");
        inner.log = Some(log.clone());
        if inner.halted {
            return;
        }
        let intr = Arc::clone(&self.interrupt);
        Self::poller_start(&mut inner, intr, log);
    }

    /// Spawn the interrupt poll thread and record it for the halt.
    ///
    /// A spawn failure panics. A viona device with no poll thread
    /// delivers no interrupts, and both failures are resource
    /// exhaustion at boot.
    fn poller_start(
        inner: &mut super::VionaInner,
        intr: Arc<IntrSlot>,
        log: slog::Logger,
    ) {
        // The halt stops only one thread. A second thread on the same
        // descriptor would keep running.
        if inner.poller.is_some() {
            slog::warn!(
                log,
                "a viona interrupt poll thread is running already"
            );
            return;
        }

        // The pipe is the only way to stop a thread in an untimed wait.
        let (stop, wake) =
            std::io::pipe().expect("failed to open the viona poll stop pipe");
        // The thread holds the link, not a bare descriptor, so a device
        // dropped without a halt cannot close the fd under it.
        let link = inner.link();
        let thread = std::thread::Builder::new()
            .name("viona-intr-poll".into())
            .spawn(move || {
                viona_intr_poll_loop(link.as_ref(), stop, &intr, log);
            })
            .expect("failed to spawn viona-intr-poll thread");
        inner.poller = Some(super::halt::Poller { wake, thread });
    }

    /// Hold the logger for a deferred start of the poll thread.
    ///
    /// On a migration destination the rings are not programmed at
    /// device creation, and a poll thread then would make them EBUSY.
    /// Call `start_poll_deferred()` after the ring state is restored.
    pub fn defer_intr_poll(&self, log: slog::Logger) {
        let mut inner = self.inner.lock().expect("viona lock");
        inner.log = Some(log.clone());
        if inner.halted {
            return;
        }
        inner.deferred_poll = Some(log);
    }

    /// Start the deferred interrupt poll thread.
    ///
    /// Call after the migration restore programs the ring state, MSI-X
    /// vectors and notification addresses.
    pub fn start_poll_deferred(&self) {
        use super::RingState;

        let mut inner = self.inner.lock().expect("viona lock");
        // A restore after the halt must not poll a destroyed link.
        if inner.halted {
            inner.deferred_poll = None;
            return;
        }
        let Some(log) = inner.deferred_poll.take() else {
            return;
        };
        slog::info!(log, "starting deferred viona interrupt poll thread");
        let intr = Arc::clone(&self.interrupt);
        Self::poller_start(&mut inner, intr, log);

        // Setting the ring state sets the indices but may not wake the
        // kernel ring worker. A kick after the poll thread starts makes
        // the worker process available entries.
        for ring in 0..NET_NUM_QUEUES as u16 {
            if inner.ring_state[ring as usize] != RingState::Ready {
                continue;
            }
            if let Err(e) = inner.link_ref().ring_kick(ring) {
                slog::warn!(inner.log(), "viona re-kick failed after restore";
                    "ring" => ring, "error" => %e);
            }
        }
    }
}

/// Deliver the interrupts one poll wakeup found pending.
///
/// The session is read before the clear. The clear gives this thread
/// the notification, and a full device reset can run between the clear
/// and the raise. The reset releases the kernel rings but does not stop
/// this thread, so the next driver can own the session at the raise. A
/// session read after the clear would raise an interrupt for a ring the
/// new driver never armed: an INTx level nothing lowers, or an MSI-X
/// message to a vector it just programmed. Read before, the transport
/// refuses the stale notification.
pub(super) fn deliver_pending(
    link: &dyn LinkOps,
    slot: &IntrSlot,
    log: &slog::Logger,
) {
    let Some(intr) = slot.get() else {
        // No transport yet, so no driver armed a ring.
        return;
    };
    let session = intr.session();

    let status = match link.intr_status() {
        Ok(status) => status,
        Err(err) => {
            slog::warn!(log, "VNA_IOC_INTR_POLL failed"; "error" => %err);
            return;
        }
    };

    // Raise per ring so the PCI transport selects the MSI-X vector, or
    // uses shared INTx.
    for (ring, pending) in status.iter().enumerate() {
        if *pending == 0 {
            continue;
        }
        let ring = ring as u16;
        // Until this clear, the kernel signals the same interrupt again
        // and this loop spins.
        if let Err(err) = link.ring_intr_clr(ring) {
            slog::warn!(log, "viona interrupt clear failed";
                "ring" => ring, "error" => %err);
        }
        intr.raise(session, ring);
    }
}

/// Interrupt poll loop. Each pending wakeup goes to [`deliver_pending`].
///
/// `stop` is the read end of the halt pipe. The wait has no timeout, so
/// only that descriptor ends this loop on demand.
pub(super) fn viona_intr_poll_loop(
    link: &dyn LinkOps,
    stop: std::io::PipeReader,
    slot: &IntrSlot,
    log: slog::Logger,
) {
    loop {
        match link.wait_intr(&stop) {
            Ok(IntrWait::Pending) => deliver_pending(link, slot, &log),
            // The halt closed the stop pipe, or the link is gone.
            Ok(IntrWait::Stopped) => return,
            // The descriptor is closed or the error is fatal. Another
            // wait would spin on the same error.
            Err(err) => {
                slog::warn!(log, "the viona interrupt wait failed";
                    "error" => %err);
                return;
            }
        }
    }
}
