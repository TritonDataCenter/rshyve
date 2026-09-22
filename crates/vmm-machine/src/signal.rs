// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The shutdown watchdog: the SIGTERM escalation, and the clock every
//! teardown is held to.
//!
//! The static, the `extern "C"` handler that writes it, and the poller
//! that reads it live together: a `static` cannot be split across a
//! crate boundary, and a signal handler cannot capture.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use slog::{error, info, warn, Logger};

use vmm_core::hdl::{SuspendHow, VmmHdl};
use vmm_core::machine::Machine;

/// Atomic flag set by the SIGTERM signal handler.
static SIGTERM_RECEIVED: AtomicBool = AtomicBool::new(false);

/// How often the poller reads the flag the handler writes.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long the guest gets to act on the stop request.
const GRACEFUL_BUDGET: Duration = Duration::from_secs(30);

/// How long the hard stop gets before the process is ended.
///
/// This covers a VMM that never reached its teardown at all. A teardown
/// that is running is held to [`TEARDOWN_DEADLINE`] instead, because
/// its own work scales with the device count and this constant cannot.
const HARD_BUDGET: Duration = Duration::from_secs(20);

/// How long the last log line gets to leave the async drain.
///
/// The terminal exit is the raw syscall, which does not flush anything,
/// and the line it writes first is the only record of why the VMM was
/// ended.
const LOG_DRAIN_GRACE: Duration = Duration::from_millis(250);

/// Exit code for a VMM that outlived its own shutdown.
///
/// Distinct from the guest-caused codes `main` returns, because this
/// one says the VMM itself would not stop.
const WEDGED_EXIT: i32 = 4;

/// When a running teardown must be finished by.
///
/// A clock, not a flag: teardown ends in `VM_DESTROY_SELF`. On illumos
/// that ioctl purges the vmm_drv holds and then waits for every lease
/// to break in an untimed `cv_wait` (`vmm_sol_dev.c`, `vmm_drv_purge`).
/// A lease that never breaks holds the ioctl for as long as it is held.
/// Teardown therefore cannot be trusted to end on its own, and the
/// escalation must keep a clock on it. A flag that stood the escalation
/// down would hand a stuck lease the whole process.
static TEARDOWN_DEADLINE: Mutex<Option<Instant>> = Mutex::new(None);

/// How a shutdown reaches the VM.
///
/// Indirected through a trait so the escalation is testable without a
/// live /dev/vmm handle, and so a VM with a control plane can stop
/// through it. Only the control plane knows whether an operator has
/// the VM paused.
pub trait GuestStop: Send + Sync + 'static {
    /// Ask the guest to power off.
    fn request_poweroff(&self) -> io::Result<()>;

    /// Stop the VM whether the guest agreed or not.
    fn halt(&self) -> io::Result<()>;
}

/// Stop backed only by the VM handle.
///
/// Right for a VM with no control socket, which is the only kind that
/// cannot be paused: both `pause` and the migration that pauses arrive
/// over that socket.
pub struct HdlStop(Arc<VmmHdl>);

/// The `source` a suspend that did not come from a guest CPU carries.
///
/// `VM_SUSPEND` records it as the requesting vCPU, and the kernel reads
/// -1 as "a device or the VMM". Zero names guest CPU 0, so a stop the
/// operator asked for would be reported against a CPU that did not ask.
pub const SUSPEND_SOURCE_VMM: i32 = -1;

impl HdlStop {
    pub fn new(hdl: Arc<VmmHdl>) -> Self {
        Self(hdl)
    }
}

impl GuestStop for HdlStop {
    fn request_poweroff(&self) -> io::Result<()> {
        self.0
            .suspend(SuspendHow::PowerOff, SUSPEND_SOURCE_VMM)
            .map(drop)
    }

    fn halt(&self) -> io::Result<()> {
        self.0
            .suspend(SuspendHow::Halt, SUSPEND_SOURCE_VMM)
            .map(drop)
    }
}

/// Signal handler for SIGTERM. It only sets the atomic flag. The
/// shutdown runs in the poller thread, because a handler may call only
/// async-signal-safe functions.
extern "C" fn sigterm_handler(_sig: libc::c_int) {
    SIGTERM_RECEIVED.store(true, Ordering::Relaxed);
}

/// Give the running teardown a deadline the escalation holds it to.
///
/// Teardown calls this before its first wait, with a budget sized from
/// the work it is about to do. A later call can only push the deadline
/// out: shortening one would cut a running flush short.
pub fn arm_teardown_deadline(budget: Duration) {
    // A budget too large to add to the clock leaves the deadline
    // unarmed, which keeps the fixed escalation rather than granting an
    // unbounded one.
    let Some(deadline) = Instant::now().checked_add(budget) else {
        return;
    };
    let mut armed = TEARDOWN_DEADLINE
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if armed.is_none_or(|current| deadline > current) {
        *armed = Some(deadline);
    }
}

/// The deadline a running teardown armed, if any.
fn teardown_deadline() -> Option<Instant> {
    // A panic in a caller must not take the escalation with it, so the
    // poison is stepped over rather than raised.
    *TEARDOWN_DEADLINE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Borrow the flag the handler writes.
///
/// A caller that must re-read the flag at a later decision point takes
/// the borrow. A caller that only needs the value now calls
/// [`sigterm_received`].
pub fn sigterm_flag() -> &'static AtomicBool {
    &SIGTERM_RECEIVED
}

/// Whether the SIGTERM handler has fired.
pub fn sigterm_received() -> bool {
    SIGTERM_RECEIVED.load(Ordering::Relaxed)
}

/// Which shutdown the VMM reached first.
enum Shutdown {
    /// SIGTERM arrived and nothing has been asked of the guest yet.
    Signal,
    /// Teardown is running and armed the clock it must finish inside.
    Teardown(Instant),
}

/// Wait until the VMM starts to shut down, either way.
///
/// The deadline is read before the flag. A SIGTERM that lands on a
/// teardown that is already running must watch that teardown, because
/// the guest it would ask to stop has stopped.
fn wait_for_shutdown(
    wait: &impl Fn(Duration),
    signalled: &impl Fn() -> bool,
    deadline_of: &impl Fn() -> Option<Instant>,
) -> Shutdown {
    loop {
        if let Some(deadline) = deadline_of() {
            return Shutdown::Teardown(deadline);
        }
        if signalled() {
            return Shutdown::Signal;
        }
        wait(POLL_INTERVAL);
    }
}

/// Watch every shutdown, and end a VMM that outlives its own.
///
/// Not the signal alone: a guest that powers itself off runs the same
/// teardown, over the same devices and the same `VM_DESTROY_SELF`, and
/// it is the common path on a node. It needs the same watch.
fn watchdog(
    vm: &dyn GuestStop,
    wait: impl Fn(Duration),
    now: impl Fn() -> Instant,
    signalled: impl Fn() -> bool,
    deadline_of: impl Fn() -> Option<Instant>,
    give_up: impl FnOnce(),
    log: &Logger,
) {
    match wait_for_shutdown(&wait, &signalled, &deadline_of) {
        // The guest has stopped already, so it is asked for nothing.
        Shutdown::Teardown(deadline) => {
            watch_teardown(&wait, &now, &deadline_of, deadline, give_up, log)
        }
        Shutdown::Signal => {
            info!(log, "SIGTERM received, stopping the guest");
            shutdown_sequence(vm, wait, now, deadline_of, give_up, log)
        }
    }
}

/// Run one SIGTERM shutdown to its end.
///
/// Each wait is only reached while the VMM is still alive: a VM that
/// stopped ends the event loop, `main` returns, and the process exit
/// takes this thread with it. So reaching a later step means the step
/// before it did not work.
fn shutdown_sequence(
    vm: &dyn GuestStop,
    wait: impl Fn(Duration),
    now: impl Fn() -> Instant,
    deadline_of: impl Fn() -> Option<Instant>,
    give_up: impl FnOnce(),
    log: &Logger,
) {
    // The first step is a suspend, not an ACPI power button: the
    // kernel turns VM_SUSPEND_POWEROFF straight into a suspend exit on
    // every vCPU, so the guest is not asked and gets no flush window.
    // The wait that follows is for the run loop to see the exit and
    // for teardown to take over, not for the guest to answer.
    if let Err(e) = vm.request_poweroff() {
        warn!(log, "the stop request did not reach the VM"; "error" => %e);
    }
    let mut armed = wait_for_teardown(&wait, &deadline_of, GRACEFUL_BUDGET);

    if armed.is_none() {
        warn!(log, "the guest did not stop in time; halting it";
            "waited_secs" => GRACEFUL_BUDGET.as_secs());
        if let Err(e) = vm.halt() {
            warn!(log, "the halt did not reach the VM"; "error" => %e);
        }
        armed = wait_for_teardown(&wait, &deadline_of, HARD_BUDGET);
    }

    match armed {
        Some(deadline) => {
            watch_teardown(&wait, &now, &deadline_of, deadline, give_up, log)
        }
        None => end_process("the VMM never reached its teardown", give_up, log),
    }
}

/// Wait for `budget`, stopping early once teardown arms its clock.
///
/// The wait is sliced so that a teardown starting near the end of a
/// budget is watched, and not cut off by the step that follows it.
fn wait_for_teardown(
    wait: &impl Fn(Duration),
    deadline_of: &impl Fn() -> Option<Instant>,
    budget: Duration,
) -> Option<Instant> {
    let mut left = budget;
    loop {
        if let Some(deadline) = deadline_of() {
            return Some(deadline);
        }
        if left.is_zero() {
            return None;
        }
        let slice = left.min(POLL_INTERVAL);
        wait(slice);
        left -= slice;
    }
}

/// Hold a running teardown to the deadline it armed.
///
/// A thread of its own: the watchdog is not the thread that drives
/// teardown, so it still reaches the terminal exit while that thread is
/// parked in an ioctl. A watchdog on the teardown thread would be
/// parked with it.
///
/// The loop ends because both `wait` and `now` move the same clock: a
/// sleep of a non-zero slice always advances a monotonic clock.
fn watch_teardown(
    wait: &impl Fn(Duration),
    now: &impl Fn() -> Instant,
    deadline_of: &impl Fn() -> Option<Instant>,
    armed: Instant,
    give_up: impl FnOnce(),
    log: &Logger,
) {
    info!(log, "teardown is running; the escalation keeps a clock on it";
        "budget_secs" => armed.saturating_duration_since(now()).as_secs());
    loop {
        let deadline = deadline_of().unwrap_or(armed);
        let left = deadline.saturating_duration_since(now());
        if left.is_zero() {
            break;
        }
        wait(left.min(POLL_INTERVAL));
    }

    end_process("teardown did not finish inside its deadline", give_up, log);
}

/// End a VMM that will not stop on its own.
///
/// Nothing is left to try. A node must not keep the memory of a VM that
/// will not stop, so the process goes without its teardown. Unflushed
/// device writes are power-loss equivalent and inside the
/// volatile-write-cache contract the block devices advertise.
///
/// This ends every wedge that userspace can break. It cannot end one
/// the kernel holds. `VM_DESTROY_SELF` waits for the vmm_drv leases in
/// an untimed `cv_wait` (`vmm_sol_dev.c`, `vmm_lease_block`), and a
/// thread parked there is not `T_WAKEABLE`, so `pokelwps` (`lwp.c`)
/// steps over it and `exitlwps` waits for it. `_exit` does not reach
/// that thread either.
///
/// The exit usually frees the process all the same, for a different
/// reason. `proc_exit` (`exit.c`) sets `SEXITING` before it calls
/// `exitlwps`, viona ring workers are LWPs of this same process, and
/// `vring_need_bail_ext` (`viona_ring.c`) tests that flag. A worker that
/// reaches the test drops its lease, and the destroy that waited for the
/// lease returns.
///
/// It does not free the process when a worker cannot reach that test:
/// one inside the mac perimeter, or in the untimed `cv_wait` of
/// `viona_tx_wait_outstanding` (`viona_tx.c`), which neither heeds
/// signals nor rechecks the bail. Only a kernel that bounds those waits
/// closes that case. The log line below is the record of it.
fn end_process(reason: &'static str, give_up: impl FnOnce(), log: &Logger) {
    error!(log, "the VMM outlived its own shutdown; ending the process";
        "reason" => reason,
        "exit_code" => WEDGED_EXIT);
    give_up();
}

/// End the process without the exit path a wedged VMM can block in.
///
/// `exit` runs the atexit handlers and flushes stdio, either of which
/// can wait on a lock a stuck thread holds. `_exit` is the syscall
/// alone. It is not a guarantee: see [`end_process`] for the one wedge
/// that no exit escapes.
fn terminal_exit() {
    // The async log drain hands the last line to another thread, and
    // `_exit` does not wait for it.
    thread::sleep(LOG_DRAIN_GRACE);
    // SAFETY: `_exit` takes an int, does not return, and reads no Rust
    // state. It is the async-signal-safe half of `exit`.
    unsafe { libc::_exit(WEDGED_EXIT) }
}

/// Install the SIGTERM handler and start the shutdown watchdog.
///
/// The watchdog covers both ways a VMM shuts down: the signal, and a
/// guest that stops itself.
pub fn install_sigterm_handler(machine: &Machine, log: &Logger) {
    install_sigterm_handler_with(
        Arc::new(HdlStop::new(machine.hdl().clone())),
        log,
    )
}

/// The same install over a caller-supplied stop.
pub fn install_sigterm_handler_with(stop: Arc<dyn GuestStop>, log: &Logger) {
    let log_sig = log.clone();
    // SAFETY: the handler only stores an atomic, which is
    // async-signal-safe, and the cast is the one `signal` takes.
    let installed = unsafe {
        libc::signal(
            libc::SIGTERM,
            sigterm_handler as *const () as libc::sighandler_t,
        )
    };
    if installed == libc::SIG_ERR {
        // SIGTERM keeps its default action, so `vmadm stop` kills the
        // VMM outright: no teardown, no device halt, and the instance
        // is left to autodestruct.
        error!(log, "SIGTERM handler not installed; a stop will kill the \
            VMM with no teardown";
            "error" => %io::Error::last_os_error());
    }
    thread::Builder::new()
        .name("shutdown-watchdog".into())
        .spawn(move || {
            watchdog(
                stop.as_ref(),
                thread::sleep,
                Instant::now,
                sigterm_received,
                teardown_deadline,
                terminal_exit,
                &log_sig,
            );
        })
        .expect("failed to spawn the shutdown watchdog thread");
}

#[cfg(test)]
mod tests;
