// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bounded waits on one descriptor.
//!
//! A thread that owns a shutdown flag must wake on a timer to read it,
//! and a listener or console that has died must be told apart from one
//! that is quiet, or the thread spins. Both answers come from `poll`,
//! and this is the one place its flags are read.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::time::Duration;

/// What a wait for readability ended with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Readiness {
    /// Nothing arrived within the budget. A signal counts as this too,
    /// so the caller re-reads its own flags and waits again.
    Idle,
    /// A read will not block.
    Readable,
    /// The descriptor reports an error, a hangup, or is not open.
    /// It reports the same on every call, so the caller must leave.
    Gone,
}

/// Wait up to `budget` for `fd` to become readable.
pub fn wait_readable(fd: BorrowedFd<'_>, budget: Duration) -> Readiness {
    let revents = match wait(fd, libc::POLLIN, budget) {
        Ok(Some(revents)) => revents,
        Ok(None) => return Readiness::Idle,
        // A failed poll fails the same way next time.
        Err(_) => return Readiness::Gone,
    };
    if revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
        Readiness::Gone
    } else if revents & libc::POLLIN != 0 {
        Readiness::Readable
    } else {
        Readiness::Idle
    }
}

/// Wait up to `budget` for `fd` to take a write.
///
/// An error condition reports true as well, so that the write which
/// follows names the error rather than this guessing at it.
pub fn wait_writable(fd: BorrowedFd<'_>, budget: Duration) -> io::Result<bool> {
    Ok(wait(fd, libc::POLLOUT, budget)?.is_some_and(|revents| {
        revents
            & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)
            != 0
    }))
}

/// The events `fd` reports within `budget`, or `None` on a timeout or
/// a signal.
fn wait(
    fd: BorrowedFd<'_>,
    events: libc::c_short,
    budget: Duration,
) -> io::Result<Option<libc::c_short>> {
    // At least one millisecond: a shorter budget would round to a poll
    // that returns at once, and the caller would spin on it.
    let timeout_ms = budget.as_millis().clamp(1, i32::MAX as u128) as i32;
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events,
        revents: 0,
    };
    // Safety: `pfd` is one pollfd and the count says so.
    let ready = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(None);
        }
        return Err(error);
    }
    if ready == 0 {
        return Ok(None);
    }
    Ok(Some(pfd.revents))
}

#[cfg(test)]
mod tests {
    use std::io::{pipe, Write};
    use std::os::fd::AsFd;

    use super::*;

    const BUDGET: Duration = Duration::from_millis(20);

    #[test]
    fn a_quiet_pipe_is_idle() {
        let (rx, _tx) = pipe().expect("pipe");
        assert_eq!(wait_readable(rx.as_fd(), BUDGET), Readiness::Idle);
    }

    #[test]
    fn a_pipe_with_bytes_is_readable() {
        let (rx, mut tx) = pipe().expect("pipe");
        tx.write_all(b"x").expect("write");
        assert_eq!(wait_readable(rx.as_fd(), BUDGET), Readiness::Readable);
    }

    #[test]
    fn a_pipe_whose_writer_left_is_gone() {
        let (rx, tx) = pipe().expect("pipe");
        drop(tx);
        assert_eq!(wait_readable(rx.as_fd(), BUDGET), Readiness::Gone);
    }

    #[test]
    fn an_empty_pipe_takes_a_write() {
        let (_rx, tx) = pipe().expect("pipe");
        assert!(wait_writable(tx.as_fd(), BUDGET).expect("poll"));
    }
}
