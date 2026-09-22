// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What these tests establish, and what they do not.
//!
//! They drive the watchdog on a fake clock, so they show which step
//! runs, in which order, and that the terminal step is reached inside
//! the armed deadline. `give_up` is a recording closure here, not
//! `_exit`, so no test in this file shows that the process exits or
//! that the VM is reclaimed.
//!
//! Only a lab test on illumos can show that. It has to hold a vmm_drv
//! lease open (a viona link that does not release), let the guest power
//! off, and then watch for the process to leave the process table and
//! for the instance to leave `/dev/vmm`. That test is also how the
//! kernel-held wedge in [`super::end_process`] is seen: there the
//! process stays, and only the log line reports it.

use std::sync::Mutex;

use super::*;

/// One test owns the whole flag, because a second test that wrote it
/// would race this one.
#[test]
fn handler_write_is_visible_through_both_accessors() {
    // A static cannot be split across a crate boundary, and the
    // handler cannot capture, so the accessors must hand back the
    // same cell the handler writes.
    assert!(std::ptr::eq(sigterm_flag(), &SIGTERM_RECEIVED));
    assert!(!sigterm_received());

    sigterm_handler(libc::SIGTERM);

    assert!(sigterm_received());
    assert!(sigterm_flag().load(Ordering::Relaxed));
    SIGTERM_RECEIVED.store(false, Ordering::Relaxed);
}

/// One test owns the whole deadline, for the same reason.
#[test]
fn an_armed_deadline_only_moves_outward() {
    assert!(teardown_deadline().is_none());

    arm_teardown_deadline(Duration::from_secs(60));
    let far = teardown_deadline().expect("the first arm sets the deadline");

    // A second, smaller budget must not cut a running flush short.
    arm_teardown_deadline(Duration::from_secs(1));

    assert_eq!(teardown_deadline(), Some(far));
    *TEARDOWN_DEADLINE
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = None;
}

/// Records every step a shutdown takes, in order.
struct RecordingStop {
    steps: Mutex<Vec<String>>,
    poweroff_refused: bool,
    halt_refused: bool,
}

impl RecordingStop {
    fn new() -> Self {
        Self {
            steps: Mutex::new(Vec::new()),
            poweroff_refused: false,
            halt_refused: false,
        }
    }

    fn push(&self, step: &str) {
        self.steps
            .lock()
            .expect("steps lock")
            .push(step.to_string());
    }

    fn steps(&self) -> Vec<String> {
        self.steps.lock().expect("steps lock").clone()
    }
}

fn refused() -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::PermissionDenied))
}

impl GuestStop for RecordingStop {
    fn request_poweroff(&self) -> io::Result<()> {
        self.push("poweroff");
        if self.poweroff_refused {
            return refused();
        }
        Ok(())
    }

    fn halt(&self) -> io::Result<()> {
        self.push("halt");
        if self.halt_refused {
            return refused();
        }
        Ok(())
    }
}

fn null_log() -> Logger {
    Logger::root(slog::Discard, slog::o!())
}

/// A clock the test moves, so a deadline is reached without waiting.
///
/// The escalation slices its waits, so a real clock would make every
/// one of these tests as slow as the budget it asserts.
struct FakeClock {
    base: Instant,
    elapsed: Mutex<Duration>,
}

impl FakeClock {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            elapsed: Mutex::new(Duration::ZERO),
        }
    }

    fn now(&self) -> Instant {
        self.base + self.elapsed()
    }

    fn elapsed(&self) -> Duration {
        *self.elapsed.lock().expect("clock lock")
    }

    fn advance(&self, by: Duration) {
        *self.elapsed.lock().expect("clock lock") += by;
    }
}

/// What one driven shutdown did.
struct Run {
    steps: Vec<String>,
    waited: Duration,
    ended: bool,
}

/// Drive one sequence on a fake clock, with no teardown running.
fn drive(stop: &RecordingStop) -> Run {
    drive_with(stop, |_, _| {})
}

/// The same, with a hook that runs on every wait slice and may arm a
/// teardown deadline.
fn drive_with(
    stop: &RecordingStop,
    on_wait: impl Fn(&FakeClock, &Mutex<Option<Instant>>),
) -> Run {
    let clock = FakeClock::new();
    let deadline: Mutex<Option<Instant>> = Mutex::new(None);
    let gave_up = Mutex::new(false);

    shutdown_sequence(
        stop,
        |slice| {
            clock.advance(slice);
            on_wait(&clock, &deadline);
        },
        || clock.now(),
        || *deadline.lock().expect("deadline lock"),
        || *gave_up.lock().expect("gave up lock") = true,
        &null_log(),
    );

    let ended = *gave_up.lock().expect("gave up lock");
    Run {
        steps: stop.steps(),
        waited: clock.elapsed(),
        ended,
    }
}

/// Most waits one watchdog run may take before the test gives up on
/// it. The longest legitimate run is the teardown ceiling, which is
/// far below this.
const WAIT_CAP: usize = 10_000;

/// Drive the whole watchdog, signal included, on a fake clock.
///
/// The hook runs on every wait slice and is what raises the signal or
/// arms the deadline, because a watchdog that is given neither waits
/// for as long as the VMM lives.
fn drive_watchdog(
    stop: &RecordingStop,
    on_wait: impl Fn(&FakeClock, &Mutex<Option<Instant>>, &Mutex<bool>),
) -> Run {
    let clock = FakeClock::new();
    let deadline: Mutex<Option<Instant>> = Mutex::new(None);
    let signalled = Mutex::new(false);
    let gave_up = Mutex::new(false);
    let waits = std::cell::Cell::new(0usize);

    watchdog(
        stop,
        |slice| {
            // A watchdog that never reaches an exit would park the
            // whole test run here, because the clock only moves when
            // it waits. The cap makes that one failing test instead.
            waits.set(waits.get() + 1);
            assert!(
                waits.get() <= WAIT_CAP,
                "the watchdog never reached its terminal step: \
                 {} waits, {:?} of fake time",
                waits.get(),
                clock.elapsed()
            );
            clock.advance(slice);
            on_wait(&clock, &deadline, &signalled);
        },
        || clock.now(),
        || *signalled.lock().expect("signal lock"),
        || *deadline.lock().expect("deadline lock"),
        || *gave_up.lock().expect("gave up lock") = true,
        &null_log(),
    );

    let ended = *gave_up.lock().expect("gave up lock");
    Run {
        steps: stop.steps(),
        waited: clock.elapsed(),
        ended,
    }
}

/// Arm the deadline once, `budget` out from the current fake time.
fn arm_once(
    clock: &FakeClock,
    deadline: &Mutex<Option<Instant>>,
    budget: Duration,
) {
    let mut armed = deadline.lock().expect("deadline lock");
    if armed.is_none() {
        *armed = Some(clock.now() + budget);
    }
}

#[test]
fn a_stop_asks_before_it_forces() {
    // The poweroff suspend is the softer of the two: the run loop ends
    // on it and teardown quiesces and flushes the devices. The halt
    // skips that, so it must come second and only after a wait.
    let stop = RecordingStop::new();

    let run = drive(&stop);

    assert_eq!(run.steps, ["poweroff".to_string(), "halt".to_string()]);
    assert_eq!(run.waited, GRACEFUL_BUDGET + HARD_BUDGET);
    assert!(run.ended, "a VMM that outlived its own shutdown must end");
}

#[test]
fn a_refused_request_still_reaches_the_halt() {
    let mut stop = RecordingStop::new();
    stop.poweroff_refused = true;

    let run = drive(&stop);

    assert!(run.steps.contains(&"halt".to_string()));
    assert!(run.ended);
}

#[test]
fn a_refused_halt_still_ends_the_process() {
    // Nothing is left to try, and a node must not keep a VM that will
    // not stop.
    let mut stop = RecordingStop::new();
    stop.halt_refused = true;

    let run = drive(&stop);

    assert!(run.ended);
}

#[test]
fn a_running_teardown_is_watched_and_not_halted() {
    // The halt skips the device flush, so a teardown that is already
    // running must not be interrupted by one.
    let stop = RecordingStop::new();
    let budget = Duration::from_secs(40);

    let run = drive_with(&stop, |clock, deadline| {
        arm_once(clock, deadline, budget);
    });

    assert_eq!(run.steps, ["poweroff".to_string()]);
    assert!(
        run.waited >= budget,
        "the escalation cut the teardown short after {:?}",
        run.waited
    );
}

#[test]
fn a_teardown_that_never_finishes_still_ends_the_process() {
    // Teardown ends in VM_DESTROY_SELF, which on illumos waits for
    // every vmm_drv lease to break through an untimed cv_wait. A
    // teardown that never returns must not keep the process, so the
    // escalation holds it to the deadline it armed.
    let stop = RecordingStop::new();
    let budget = Duration::from_secs(40);

    let run = drive_with(&stop, |clock, deadline| {
        arm_once(clock, deadline, budget);
    });

    assert!(run.ended, "a teardown that never finishes must be ended");
    assert!(
        run.waited <= budget + POLL_INTERVAL * 2,
        "the terminal path was reached late, after {:?}",
        run.waited
    );
}

#[test]
fn a_deadline_armed_late_in_a_budget_is_still_watched() {
    // A teardown that starts in the last slice of the graceful budget
    // would be halted by the next step if the wait were one call.
    let stop = RecordingStop::new();
    let budget = Duration::from_secs(40);

    let run = drive_with(&stop, |clock, deadline| {
        if clock.elapsed() + POLL_INTERVAL >= GRACEFUL_BUDGET {
            arm_once(clock, deadline, budget);
        }
    });

    assert_eq!(run.steps, ["poweroff".to_string()]);
    assert!(run.ended);
}

#[test]
fn a_guest_that_stops_itself_is_watched_without_a_signal() {
    // A guest powering itself off is the common shutdown on a node. It
    // reaches the same teardown and the same VM_DESTROY_SELF, so it
    // needs the same clock.
    let stop = RecordingStop::new();
    let budget = Duration::from_secs(40);

    let run = drive_watchdog(&stop, |clock, deadline, _signalled| {
        arm_once(clock, deadline, budget);
    });

    assert!(run.steps.is_empty(), "a stopped guest is asked for nothing");
    assert!(run.ended, "a guest-initiated teardown must be watched");
    assert!(
        run.waited <= budget + POLL_INTERVAL * 2,
        "the terminal path was reached late, after {:?}",
        run.waited
    );
}

#[test]
fn a_signal_on_a_running_teardown_does_not_restart_it() {
    // The halt skips the device flush. A SIGTERM that lands after the
    // guest stopped must not take a running teardown back to the
    // request-and-halt escalation.
    let stop = RecordingStop::new();
    let budget = Duration::from_secs(40);

    let run = drive_watchdog(&stop, |clock, deadline, signalled| {
        arm_once(clock, deadline, budget);
        *signalled.lock().expect("signal lock") = true;
    });

    assert!(run.steps.is_empty());
    assert!(run.ended);
}

#[test]
fn a_signal_with_no_teardown_runs_the_full_escalation() {
    // Nothing has stopped the guest, so the watchdog has to ask and
    // then force.
    let stop = RecordingStop::new();

    let run = drive_watchdog(&stop, |_clock, _deadline, signalled| {
        *signalled.lock().expect("signal lock") = true;
    });

    assert_eq!(run.steps, ["poweroff".to_string(), "halt".to_string()]);
    assert!(run.ended);
}

#[test]
fn the_hard_budget_outlasts_the_teardown_it_waits_on() {
    // Teardown gets 5 s to quiesce the devices and 5 s to join the
    // vCPU threads, plus a per-device flush this constant cannot
    // size. The armed deadline covers the rest. This only has to
    // outlast the two fixed halves.
    assert!(HARD_BUDGET > Duration::from_secs(10));
}
