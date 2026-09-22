// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The bounded halt shared by the devices that serve a unix socket.
//!
//! # Invariant
//!
//! Halt returns inside the budget it declares, and nothing in it waits
//! on a host peer. The budget bounds each join for its whole length, so
//! one stuck thread cannot hold teardown open.
//!
//! Every thread ends on the shutdown flag. Halt never connects to its
//! own socket to wake the accept thread, for two reasons:
//! - illumos `tl_conn_req` (`uts/common/io/tl.c`) refuses that connect
//!   with ECONNREFUSED when the backlog is full.
//! - A connection accepted during halt is a peer that gets no service.

use std::path::Path;
use std::thread::JoinHandle;
use std::time::Duration;

use slog::{debug, Logger};

/// How long one set of threads gets to end.
///
/// The teardown sweep halts every device before `VM_DESTROY_SELF` and
/// gives each halt its own budget. A halt that stays inside it is never
/// abandoned part-way.
pub(crate) const HALT_BUDGET: Duration = Duration::from_secs(2);

/// How many joins run one after the other: the accept thread, then
/// the readers.
pub(crate) const HALT_JOIN_ROUNDS: u32 = 2;

/// The deadline teardown must allow for one halt: every join round at
/// the full budget.
pub(crate) fn halt_budget() -> Duration {
    HALT_BUDGET.saturating_mul(HALT_JOIN_ROUNDS)
}

/// End the accept thread, then the peers and their readers, then the
/// socket file.
///
/// The caller has already raised its shutdown flag. `disconnect` shuts
/// down every peer socket, which wakes the readers blocked in
/// `read(2)`, and returns their threads. It runs after the accept
/// thread returns, so no new reader can appear. A reader the shutdown
/// did not reach is in a read that nothing here can cancel. It stays
/// running and teardown continues.
pub(crate) fn halt_socket_device(
    log: &Logger,
    device: &str,
    accept: Option<JoinHandle<()>>,
    disconnect: impl FnOnce() -> Vec<JoinHandle<()>>,
    socket_path: &Path,
) {
    if let Some(handle) = accept {
        if !vmm_core::thread::join_bounded(vec![handle], HALT_BUDGET) {
            debug!(log, "halt left the accept thread running";
                "device" => device);
        }
    }

    if !vmm_core::thread::join_bounded(disconnect(), HALT_BUDGET) {
        debug!(log, "halt left a reader running"; "device" => device);
    }

    // The socket may already be gone if the path was reused.
    let _ = std::fs::remove_file(socket_path);
}
