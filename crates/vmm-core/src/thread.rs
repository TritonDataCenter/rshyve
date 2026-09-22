// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Thread joins that a wedged thread cannot hold open.

use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Join every handle, giving up after `budget`. Returns false when the
/// budget expired.
///
/// The budget bounds the join itself, not whether the next join starts.
/// A loop that only tests a deadline before each `join()` gives no bound
/// at all, because one wedged thread holds teardown open for as long as
/// it stays wedged. The joining is therefore done on a helper thread and
/// waited for on a channel.
///
/// A thread that misses the budget is left running. Nothing in userspace
/// can revoke a syscall it is blocked in, so the caller finishes its
/// teardown and lets process exit collect it.
pub fn join_bounded(handles: Vec<JoinHandle<()>>, budget: Duration) -> bool {
    if handles.is_empty() {
        return true;
    }
    let (tx, rx) = mpsc::channel();
    let worker =
        thread::Builder::new()
            .name("join-bounded".into())
            .spawn(move || {
                for handle in handles {
                    drop(handle.join());
                }
                let _ = tx.send(());
            });
    if worker.is_err() {
        return false;
    }

    matches!(rx.recv_timeout(budget), Ok(()))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use super::*;

    #[test]
    fn every_finished_thread_is_joined() {
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        let handle = thread::spawn(move || flag.store(true, Ordering::Release));

        assert!(join_bounded(vec![handle], Duration::from_secs(5)));
        assert!(done.load(Ordering::Acquire));
    }

    #[test]
    fn an_empty_set_needs_no_helper_thread() {
        assert!(join_bounded(Vec::new(), Duration::from_millis(0)));
    }

    /// The budget has to bound the join, not just gate whether it
    /// starts. A deadline tested before each `join()` would block here
    /// for the whole life of the wedged thread.
    #[test]
    fn a_wedged_thread_does_not_hold_the_join_open() {
        let stop = Arc::new(AtomicBool::new(false));
        let wedged = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !wedged.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(5));
            }
        });

        let start = std::time::Instant::now();
        let joined = join_bounded(vec![handle], Duration::from_millis(100));
        let waited = start.elapsed();

        assert!(!joined, "the wedged thread cannot have been joined");
        assert!(waited < Duration::from_secs(2), "waited {waited:?}");
        stop.store(true, Ordering::Release);
    }
}
