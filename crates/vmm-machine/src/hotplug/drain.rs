// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The thread each hotplug engine runs to answer the guest's ejects.
//!
//! Each engine owns one so no teardown ever runs on a vCPU thread. The
//! thread holds only a weak reference to its engine, so an engine that
//! is dropped without a shutdown still ends its thread.

use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use slog::{error, Logger};

use super::{lock, DRAIN_INTERVAL};

/// A drain thread and the flag that stops it.
pub(super) struct DrainThread {
    name: &'static str,
    stop: Arc<Stopper>,
    thread: Mutex<Option<JoinHandle<()>>>,
    log: Logger,
}

impl DrainThread {
    /// Spawn `name`, running `tick` on `target` every [`DRAIN_INTERVAL`]
    /// until stopped.
    ///
    /// A thread that cannot be spawned is logged with `missing` and the
    /// VM keeps running: an add still works, and an eject waits in the
    /// register file until something drains it.
    pub(super) fn spawn<T: Send + Sync + 'static>(
        name: &'static str,
        target: &Arc<T>,
        tick: impl Fn(&T) + Send + 'static,
        log: &Logger,
        missing: &'static str,
    ) -> Self {
        let stop = Arc::new(Stopper::default());
        let target = Arc::downgrade(target);
        let loop_stop = Arc::clone(&stop);
        let spawned = thread::Builder::new()
            .name(name.into())
            .spawn(move || drain_loop(&target, &loop_stop, tick));
        let thread = match spawned {
            Ok(handle) => Some(handle),
            Err(e) => {
                error!(log, "{missing}"; "thread" => name, "error" => %e);
                None
            }
        };
        Self {
            name,
            stop,
            thread: Mutex::new(thread),
            log: log.clone(),
        }
    }

    /// Whether the thread has been joined, or never started.
    pub(super) fn is_stopped(&self) -> bool {
        lock(&self.thread).is_none()
    }

    /// Stop the thread and wait up to `budget` for it.
    ///
    /// Bounded because a tick can be inside a device release that does
    /// not return, and teardown calls this one step short of the VM
    /// destroy. An abandoned thread holds only a weak reference to its
    /// engine, so it ends on its own once the engine is dropped.
    pub(super) fn shutdown(&self, budget: Duration) {
        self.stop.stop();
        let handle = lock(&self.thread).take();
        if let Some(handle) = handle {
            if !vmm_core::thread::join_bounded(vec![handle], budget) {
                error!(self.log, "the drain thread did not return inside \
                    its budget"; "thread" => self.name,
                    "budget_secs" => budget.as_secs());
            }
        }
    }
}

fn drain_loop<T>(target: &Weak<T>, stop: &Stopper, tick: impl Fn(&T)) {
    while !stop.wait(DRAIN_INTERVAL) {
        let Some(target) = target.upgrade() else {
            return;
        };
        tick(&target);
    }
}

/// The drain thread's shutdown flag, with the wait that reads it.
#[derive(Default)]
struct Stopper {
    stopped: Mutex<bool>,
    changed: Condvar,
}

impl Stopper {
    fn stop(&self) {
        *lock(&self.stopped) = true;
        self.changed.notify_all();
    }

    /// Wait up to `timeout`, and report whether the engine must stop.
    fn wait(&self, timeout: Duration) -> bool {
        let stopped = lock(&self.stopped);
        if *stopped {
            return true;
        }
        match self.changed.wait_timeout(stopped, timeout) {
            Ok((stopped, _)) => *stopped,
            // A poisoned wait still carries the flag, and the flag is
            // the whole state.
            Err(e) => *e.into_inner().0,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn null_log() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    fn ticking() -> (Arc<AtomicUsize>, DrainThread) {
        let ticks = Arc::new(AtomicUsize::new(0));
        let thread = DrainThread::spawn(
            "drain-test",
            &ticks,
            |ticks: &AtomicUsize| {
                ticks.fetch_add(1, Ordering::Relaxed);
            },
            &null_log(),
            "no test drain thread",
        );
        (ticks, thread)
    }

    #[test]
    fn the_drain_thread_stops_on_request() {
        let (_ticks, thread) = ticking();
        // Returns, which is the whole test: a thread that ignored the
        // flag would hang the join.
        thread.shutdown(Duration::from_secs(5));
    }

    /// A tick inside a device release that never returns must not hold
    /// teardown one step short of the VM destroy.
    #[test]
    fn a_tick_that_never_returns_does_not_hold_the_shutdown() {
        let wedged = Arc::new(());
        let thread = DrainThread::spawn(
            "drain-wedged",
            &wedged,
            |_: &()| loop {
                thread::sleep(Duration::from_millis(50));
            },
            &null_log(),
            "no test drain thread",
        );

        let started = std::time::Instant::now();
        thread.shutdown(Duration::from_millis(100));

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the shutdown took {:?}",
            started.elapsed(),
        );
    }

    #[test]
    fn the_drain_thread_stops_when_the_engine_is_dropped() {
        // No shutdown call: a leaked engine must not leave a thread
        // polling the register file forever.
        let (ticks, thread) = ticking();
        let handle = lock(&thread.thread).take().expect("spawned");
        drop(ticks);

        handle.join().expect("the drain thread ends");
    }
}
