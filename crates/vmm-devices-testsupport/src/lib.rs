// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Test support shared by the device crates.
//!
//! This is a dev-dependency, never a dependency. A `#[cfg(test)]` item
//! does not exist when its crate is built as a dependency, and a cargo
//! feature here would unify into a release graph.
//!
//! Each device crate lists `vmm_devices_testsupport.workspace = true`
//! in its own `[dev-dependencies]`.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Deadline that [`assert_query_non_blocking`] enforces.
///
/// A device whose query is slower but still non-blocking must say why
/// before it loosens this.
pub const QUERY_BUDGET: Duration = Duration::from_millis(250);

/// Run `query` on another thread and panic if it misses [`QUERY_BUDGET`].
///
/// This is the only automated guard on the rule that `is_quiesced` must
/// not block. A device that waits inside that method makes the caller's
/// teardown deadline unenforceable.
///
/// It takes a closure, not a `&Arc<dyn Lifecycle>`. Naming `Lifecycle`
/// needs a dependency on the device crate. The callers are
/// `#[cfg(test)]` modules inside that crate, so the trait object would
/// come from a second copy of it and fail to typecheck.
///
/// Returns the value the worker thread observed, so the caller need not
/// query the device a second time.
pub fn assert_query_non_blocking<F>(query: F) -> bool
where
    F: FnOnce() -> bool + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        // After the budget expires the receiver is gone. A panic in a
        // detached worker only adds noise.
        let _ = tx.send(query());
    });
    rx.recv_timeout(QUERY_BUDGET)
        .expect("is_quiesced must not block")
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn returns_the_value_the_worker_observed() {
        let flag = Arc::new(AtomicBool::new(false));

        let probe = Arc::clone(&flag);
        assert!(!assert_query_non_blocking(
            move || probe.load(Ordering::Acquire)
        ));

        flag.store(true, Ordering::Release);
        let probe = Arc::clone(&flag);
        assert!(assert_query_non_blocking(
            move || probe.load(Ordering::Acquire)
        ));
    }

    #[test]
    #[should_panic(expected = "is_quiesced must not block")]
    fn a_query_that_blocks_past_the_budget_panics() {
        assert_query_non_blocking(|| {
            thread::sleep(QUERY_BUDGET * 4);
            true
        });
    }
}
