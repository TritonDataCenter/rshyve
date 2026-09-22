// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The bound the eject path puts on a device release.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use slog::{error, warn, Logger};

use vmm_devices::lifecycle::prepare_unplug;

/// How much longer than its drain budget one unplug may take.
///
/// `prepare_unplug` ends in `halt()`, which it does not bound and
/// cannot: viona's `VNA_IOC_DELETE` is an untimed `cv_wait` that heeds
/// no signal. The slack covers an ordinary release. Past it the drain
/// thread stops waiting so every later eject is still answered.
pub(super) const UNPLUG_HALT_SLACK: Duration = Duration::from_secs(5);

/// What a bounded unplug preparation ended in.
pub(super) enum Unplug {
    /// The device gave up its resources. The slot can be taken apart.
    Done,
    /// The device keeps its slot and goes on running. `paused` is true
    /// when the preparation got as far as pausing it, so the abort has
    /// to undo that.
    Kept { reason: String, paused: bool },
    /// Past its budget with the release still in flight.
    InFlight,
}

/// Run [`prepare_unplug`] with a bound on it.
///
/// `prepare_unplug` bounds the drain wait and then calls `halt()`, which
/// it cannot bound: viona's `VNA_IOC_DELETE` waits for each ring worker
/// in a `cv_wait` that heeds no signal. Run inline, one such halt parks
/// the drain thread, so every later eject on the VM goes unanswered and
/// teardown's join never returns. The work therefore goes on a thread
/// this one can let go of, the way the teardown halt sweep does.
pub(super) fn unplug_bounded(
    device: &Arc<dyn vmm_devices::Lifecycle>,
    drain_budget: Duration,
    total_budget: Duration,
    log: &Logger,
) -> Unplug {
    let (tx, rx) = mpsc::channel();
    let work = Arc::clone(device);
    let spawned =
        thread::Builder::new()
            .name("device-unplug".into())
            .spawn(move || {
                // A closed channel means the caller stopped waiting, which
                // is the case this bound exists for.
                drop(tx.send(prepare_unplug(&work, drain_budget)));
            });
    if let Err(e) = spawned {
        // Nothing ran, so the device is still running and keeps its
        // slot. Running the unplug here instead would put the untimed
        // halt on the drain thread, which is what this avoids.
        warn!(log, "could not start a device unplug thread";
            "device" => device.type_name(), "error" => %e);
        return Unplug::Kept {
            reason: format!("no unplug thread: {e}"),
            paused: false,
        };
    }

    match rx.recv_timeout(total_budget) {
        Ok(Ok(())) => Unplug::Done,
        Ok(Err(e)) => Unplug::Kept {
            reason: format!("{e:?}"),
            paused: true,
        },
        Err(RecvTimeoutError::Timeout) => Unplug::InFlight,
        // The thread went without sending, so the release neither
        // finished nor reported. Treat it as in flight: the slot is the
        // thing that must not be touched either way.
        Err(RecvTimeoutError::Disconnected) => {
            error!(log, "the device unplug thread ended with no result";
                "device" => device.type_name());
            Unplug::InFlight
        }
    }
}
