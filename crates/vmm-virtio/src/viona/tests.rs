// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Every kernel call the device makes goes through
//! [`LinkOps`](viona_api::LinkOps). A recording link replaces the
//! driver, so a test reads each command and ring the device sent, on
//! every platform. `viona_api` tests the ioctl encoding.
//!
//! Nothing here issues an ioctl. That needs an open `/dev/viona`, which
//! is a lab test.

use std::io::{PipeReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use viona_api::{vioc_ring_state, IntrStatus, IntrWait, LinkOps, PromiscMode};

use super::halt::{halt_link, stop_poller, Poller};
use super::VirtioViona;

mod device;
mod errors;
mod intr;

/// One kind of kernel call the device makes.
///
/// A test names the calls the kernel refuses. The unit is one call,
/// because each call has its own error branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    RingInit,
    RingKick,
    RingReset,
    RingPause,
    RingSetState,
    RingGetState,
    RingSetMsi,
    SetFeatures,
    IntrStatus,
    RingIntrClr,
    SetNotifyIop,
    SetNotifyMmio,
    SetPromisc,
    WaitIntr,
    Delete,
}

/// One call the device made into the kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    RingInit {
        ring: u16,
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
    },
    RingKick(u16),
    RingReset(u16),
    RingPause(u16),
    RingSetState(vioc_ring_state),
    RingGetState(u16),
    RingSetMsi {
        ring: u16,
        addr: u64,
        msg: u64,
    },
    RingIntrClr(u16),
    SetFeatures(u64),
    SetNotifyIop(u16),
    SetNotifyMmio {
        addr: u64,
        size: u32,
    },
    SetPromisc(PromiscMode),
    Delete,
    /// The poll thread left.
    PollerStopped,
}

/// The kernel calls the device made, in order.
type Steps = Arc<Mutex<Vec<Step>>>;

/// Where the poll thread stops while a test acts.
///
/// `Poll` catches a session read after the pending query. `Clear`
/// catches a session read at the raise. Only a read before both passes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ParkAt {
    /// Inside `VNA_IOC_INTR_POLL`, before the pending rings are known.
    Poll,
    /// After the ring's pending interrupt is cleared. The clear gives
    /// this thread the notification.
    Clear,
}

/// A one-shot stop point that the test releases.
struct Park {
    at: ParkAt,
    arrived: mpsc::Sender<()>,
    go: mpsc::Receiver<()>,
}

/// A delete that waits until the test releases it.
///
/// On illumos the delete is an untimed kernel wait. This lets a test
/// act while the halt is in that wait.
struct Hold {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

/// A wait this long means a thread is stuck, not slow.
const WEDGED: Duration = Duration::from_secs(15);

/// A link that records the calls the device makes.
///
/// It replaces the whole in-kernel side, so call order and error
/// handling are testable on every platform. Each test enables only
/// what it reads.
struct TestLink {
    steps: Steps,
    /// The calls the kernel refuses.
    failing: Vec<Op>,
    /// Holds `delete` until the test releases it.
    delete_hold: Mutex<Option<Hold>>,
    /// The device checked for a held lock, once it exists.
    device: OnceLock<Weak<VirtioViona>>,
    /// The calls that ran with the device lock held.
    locked: Mutex<Vec<Step>>,
    /// What `intr_status` reports pending, one entry per ring.
    status: IntrStatus,
    /// Where the poll thread stops, once.
    park: Mutex<Option<Park>>,
    /// A call that a signal interrupts, and how many times remain.
    interrupted: Mutex<Option<(Op, usize)>>,
    /// `ring_get_state` reports a used cursor that wrapped to zero.
    wrapped_used: bool,
}

impl TestLink {
    fn new(steps: &Steps) -> Self {
        Self {
            steps: Arc::clone(steps),
            failing: Vec::new(),
            delete_hold: Mutex::new(None),
            device: OnceLock::new(),
            locked: Mutex::new(Vec::new()),
            status: IntrStatus::default(),
            park: Mutex::new(None),
            interrupted: Mutex::new(None),
            wrapped_used: false,
        }
    }

    /// A link whose kernel refuses `ops` and accepts the rest.
    fn failing(mut self, ops: &[Op]) -> Self {
        self.failing = ops.to_vec();
        self
    }

    /// A link whose kernel returns `EINTR` on the first `times` calls to
    /// `op`, then accepts it.
    ///
    /// `viona_ioc_ring_reset` waits interruptibly, so the caller must
    /// retry. It is not a refusal.
    fn interrupted_on(self, op: Op, times: usize) -> Self {
        *self.interrupted.lock().expect("interrupted lock") = Some((op, times));
        self
    }

    /// Record one call, then return the result the test set.
    ///
    /// A refused call is recorded too: the device still made it.
    fn answer(&self, op: Op, step: Step) -> std::io::Result<()> {
        self.push(step);
        self.result(op)
    }

    /// The result of one call.
    fn result(&self, op: Op) -> std::io::Result<()> {
        let mut interrupted =
            self.interrupted.lock().expect("interrupted lock");
        if let Some((signalled, left)) = interrupted.as_mut() {
            if *signalled == op && *left > 0 {
                *left -= 1;
                return Err(std::io::Error::from(
                    std::io::ErrorKind::Interrupted,
                ));
            }
        }
        drop(interrupted);
        if self.failing.contains(&op) {
            return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        }
        Ok(())
    }

    /// A link whose delete waits for `hold`.
    fn holding(self, hold: Hold) -> Self {
        *self.delete_hold.lock().expect("hold lock") = Some(hold);
        self
    }

    /// A link whose used cursor wrapped to zero.
    fn wrapped_used(mut self) -> Self {
        self.wrapped_used = true;
        self
    }

    /// A link that reports `ring` pending on every poll.
    fn pending(mut self, ring: usize) -> Self {
        self.status[ring] = 1;
        self
    }

    /// A link that stops the poll thread once, at `park.at`.
    fn parking(self, park: Park) -> Self {
        *self.park.lock().expect("park lock") = Some(park);
        self
    }

    fn push(&self, step: Step) {
        if let Some(device) = self.device.get().and_then(Weak::upgrade) {
            // A failed `try_lock` means a thread holds the device lock
            // during this kernel call.
            if device.inner.try_lock().is_err() {
                self.locked.lock().expect("locked lock").push(step);
            }
        }
        self.steps.lock().expect("steps lock").push(step);
    }

    /// Stop the caller here, if this is the step the test named.
    ///
    /// One shot: later calls on the same link continue normally.
    fn maybe_park(&self, at: ParkAt) {
        let park = {
            let mut slot = self.park.lock().expect("park lock");
            match slot.as_ref() {
                Some(park) if park.at == at => slot.take(),
                _ => None,
            }
        };
        let Some(park) = park else {
            return;
        };
        park.arrived.send(()).expect("the test watches this park");
        park.go
            .recv_timeout(WEDGED)
            .expect("the test never released this park");
    }
}

impl LinkOps for TestLink {
    fn ring_init(
        &self,
        ring: u16,
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
    ) -> std::io::Result<()> {
        self.answer(
            Op::RingInit,
            Step::RingInit {
                ring,
                size,
                desc,
                avail,
                used,
            },
        )
    }

    fn ring_kick(&self, ring: u16) -> std::io::Result<()> {
        self.answer(Op::RingKick, Step::RingKick(ring))
    }

    fn ring_reset(&self, ring: u16) -> std::io::Result<()> {
        self.answer(Op::RingReset, Step::RingReset(ring))
    }

    fn ring_pause(&self, ring: u16) -> std::io::Result<()> {
        self.answer(Op::RingPause, Step::RingPause(ring))
    }

    fn ring_set_state(&self, state: &vioc_ring_state) -> std::io::Result<()> {
        self.answer(Op::RingSetState, Step::RingSetState(*state))
    }

    fn ring_get_state(&self, ring: u16) -> std::io::Result<vioc_ring_state> {
        self.push(Step::RingGetState(ring));
        self.result(Op::RingGetState)?;
        // Distinct values, neither equal to the ring, so a caller that
        // returns the wrong field or swaps the pair fails.
        Ok(vioc_ring_state {
            vrs_index: ring,
            vrs_avail_idx: 0x1111 + ring,
            vrs_used_idx: if self.wrapped_used { 0 } else { 0x2222 + ring },
            ..Default::default()
        })
    }

    fn ring_set_msi(
        &self,
        ring: u16,
        addr: u64,
        msg: u64,
    ) -> std::io::Result<()> {
        self.answer(Op::RingSetMsi, Step::RingSetMsi { ring, addr, msg })
    }

    fn set_features(&self, features: u64) -> std::io::Result<()> {
        self.answer(Op::SetFeatures, Step::SetFeatures(features))
    }

    fn intr_status(&self) -> std::io::Result<IntrStatus> {
        self.maybe_park(ParkAt::Poll);
        self.result(Op::IntrStatus)?;
        Ok(self.status)
    }

    fn ring_intr_clr(&self, ring: u16) -> std::io::Result<()> {
        let refused = self.answer(Op::RingIntrClr, Step::RingIntrClr(ring));
        self.maybe_park(ParkAt::Clear);
        refused
    }

    fn wait_intr(&self, stop: &PipeReader) -> std::io::Result<IntrWait> {
        self.result(Op::WaitIntr)?;
        // The halt closes the write end, so this read returns 0 bytes.
        // A test link never signals pending work here.
        let mut byte = [0u8; 1];
        let mut stop = stop;
        match stop.read(&mut byte) {
            Ok(0) => Ok(IntrWait::Stopped),
            other => panic!("the stop pipe answered {other:?}"),
        }
    }

    fn set_notify_iop(&self, port: u16) -> std::io::Result<()> {
        self.answer(Op::SetNotifyIop, Step::SetNotifyIop(port))
    }

    fn set_notify_mmio(&self, addr: u64, size: u32) -> std::io::Result<()> {
        self.answer(Op::SetNotifyMmio, Step::SetNotifyMmio { addr, size })
    }

    fn set_promisc(&self, mode: PromiscMode) -> std::io::Result<()> {
        self.answer(Op::SetPromisc, Step::SetPromisc(mode))
    }

    fn delete(&self) -> std::io::Result<()> {
        self.push(Step::Delete);
        if let Some(hold) = self.delete_hold.lock().expect("hold lock").take() {
            hold.entered
                .send(())
                .expect("the test waits for the delete");
            hold.release
                .recv_timeout(WEDGED)
                .expect("the test released the delete");
        }
        self.result(Op::Delete)
    }
}

fn steps_of(steps: &Steps) -> Vec<Step> {
    steps.lock().expect("steps lock").clone()
}

/// The rings whose pending interrupt the device cleared, in order.
fn cleared(steps: &Steps) -> Vec<u16> {
    steps_of(steps)
        .iter()
        .filter_map(|step| match step {
            Step::RingIntrClr(ring) => Some(*ring),
            _ => None,
        })
        .collect()
}

fn null_log() -> slog::Logger {
    slog::Logger::root(slog::Discard, slog::o!())
}

#[test]
fn a_halt_resets_every_ring_before_it_deletes_the_link() {
    // The delete's own ring wait ignores signals, and returns at once
    // for a ring that is already reset.
    let steps = Steps::default();
    let link = TestLink::new(&steps);

    halt_link(&link, &null_log()).expect("the delete succeeds");

    assert_eq!(
        steps_of(&steps),
        [Step::RingReset(0), Step::RingReset(1), Step::Delete],
    );
}

#[test]
fn a_failed_delete_reaches_the_caller() {
    // Teardown must reach VM_DESTROY_SELF, so nothing is unwound. The
    // result still carries the failure: only the delete proves every
    // kernel ring stopped, and a caller that escalated to it must know.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::Delete]);

    halt_link(&link, &null_log()).expect_err("the kernel refused the delete");

    assert_eq!(steps_of(&steps).last(), Some(&Step::Delete));
}

#[test]
fn stopping_a_poller_wakes_the_thread_and_waits_for_it() {
    // The poll thread waits with no timeout, so only closing the write
    // end can end it. The stop must not return before the thread exits:
    // the link is deleted next, and the thread reads it.
    use std::os::fd::AsRawFd;

    let (stop, wake) = std::io::pipe().expect("the test can open a pipe");
    let woken = Arc::new(AtomicBool::new(false));
    let gone = Arc::new(AtomicBool::new(false));
    let thread = {
        let woken = Arc::clone(&woken);
        let gone = Arc::clone(&gone);
        std::thread::Builder::new()
            .name("poller-stub".into())
            .spawn(move || {
                let mut pfd = libc::pollfd {
                    fd: stop.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // A closed write end leaves the descriptor ready, so
                // this returns once. The timeout only stops a broken
                // wake from hanging the test.
                // SAFETY: `pfd` is one live `pollfd` and the count says
                // one, so poll reads and writes only that element. The
                // thread owns `stop`, so its descriptor stays open.
                while unsafe { libc::poll(&mut pfd, 1, 10_000) } < 0 {
                    let err = std::io::Error::last_os_error();
                    assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
                }
                woken.store(pfd.revents != 0, Ordering::SeqCst);
                // A stop that drops the handle instead of joining
                // returns inside this window.
                std::thread::sleep(Duration::from_millis(200));
                gone.store(true, Ordering::SeqCst);
            })
            .expect("the test can spawn a thread")
    };

    stop_poller(Some(Poller { wake, thread }), &null_log());

    assert!(
        woken.load(Ordering::SeqCst),
        "the stop pipe never became ready",
    );
    assert!(
        gone.load(Ordering::SeqCst),
        "the stop returned before the thread left",
    );
}
