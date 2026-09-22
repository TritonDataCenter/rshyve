// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The accept loop shared by everything here that serves a unix socket.
//!
//! The loop needs a wait it can leave. A blocking `accept` ends only
//! when a client arrives, so halt would have to connect to the socket
//! itself to wake it, and halt must not depend on a peer. That connect
//! is also not dependable: illumos `tl_conn_req` refuses it with
//! ECONNREFUSED once the listener's backlog is full
//! (uts/common/io/tl.c), which leaves the accept thread asleep. So the
//! listener is non-blocking and the loop polls it on a tick,
//! re-reading the shutdown flag each time.

use std::io;
use std::net::Shutdown;
use std::os::fd::AsFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::poll::{wait_readable, Readiness};
use slog::{debug, warn, Logger};

/// How long the accept thread waits before it re-reads the shutdown
/// flag. This bounds how long halt waits for that thread to return.
const ACCEPT_POLL: Duration = Duration::from_millis(50);
/// Pause after a failed `accept`, so a persistent error such as EMFILE
/// cannot spin the accept thread at full speed.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// Hand back an accepted socket the readers can use, or nothing.
///
/// The listener is non-blocking so the accept loop can poll. On BSD and
/// illumos an accepted socket inherits that flag. Every reader on this
/// socket arms a read timeout and treats `WouldBlock` as an idle poll,
/// so a socket that kept the flag would spin a thread at full speed
/// instead of waiting. A socket the flag cannot be cleared on is closed
/// rather than served.
pub fn accepted_blocking(stream: UnixStream) -> Option<UnixStream> {
    match stream.set_nonblocking(false) {
        Ok(()) => Some(stream),
        Err(_) => {
            let _ = stream.shutdown(Shutdown::Both);
            None
        }
    }
}

/// Accept clients until `shutdown`, handing each usable socket to
/// `on_client`.
///
/// A persistent accept failure is reported once per run of failures.
/// It is seen again on every pass, and a peer that can make one happen
/// must not be able to drive the log with it.
pub fn accept_loop(
    listener: UnixListener,
    shutdown: &AtomicBool,
    log: &Logger,
    who: &'static str,
    mut on_client: impl FnMut(UnixStream),
) {
    if let Err(error) = listener.set_nonblocking(true) {
        // Without the flag this thread would block in `accept` and only
        // a client could end it. Serving no connection is the lesser
        // fault: a device that cannot be torn down holds the whole
        // teardown sweep.
        warn!(log, "listener setup failed";
            "socket" => who, "error" => %error);
        return;
    }

    let mut reported = false;
    while !shutdown.load(Ordering::Acquire) {
        match wait_readable(listener.as_fd(), ACCEPT_POLL) {
            Readiness::Readable => {}
            Readiness::Idle => continue,
            // A dead listener reports its condition on every call, so
            // the loop must leave rather than spin on it.
            Readiness::Gone => {
                warn!(log, "listener is unusable"; "socket" => who);
                return;
            }
        }
        let stream = match listener.accept() {
            Ok((stream, _)) => {
                reported = false;
                stream
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                continue
            }
            Err(error) => {
                if !reported {
                    debug!(log, "accept failed";
                        "socket" => who, "error" => %error);
                    reported = true;
                }
                std::thread::sleep(ACCEPT_BACKOFF);
                continue;
            }
        };
        if let Some(stream) = accepted_blocking(stream) {
            on_client(stream);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;
    use std::time::Instant;

    use super::*;

    /// A socket path this test owns, removed when it ends.
    struct TestSocket(std::path::PathBuf);

    impl TestSocket {
        fn new(tag: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "vmm-accept-{tag}-{}-{id}.sock",
                std::process::id(),
            )))
        }

        fn bind(&self) -> UnixListener {
            let _ = std::fs::remove_file(&self.0);
            UnixListener::bind(&self.0).expect("bind a test socket")
        }
    }

    impl Drop for TestSocket {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn null_log() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    /// Halt must not have to connect to the socket to end this thread.
    #[test]
    fn the_loop_returns_when_shutdown_is_set() {
        let socket = TestSocket::new("shutdown");
        let listener = socket.bind();
        let shutdown = AtomicBool::new(true);

        let started = Instant::now();
        accept_loop(listener, &shutdown, &null_log(), "test", |_| {
            panic!("no client connected");
        });

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the loop took {:?}",
            started.elapsed(),
        );
    }

    /// A peer that disconnects before the accept is one accept error on
    /// illumos. One error must not take the listener away for good.
    #[test]
    fn a_peer_that_resets_before_accept_does_not_end_the_loop() {
        let socket = TestSocket::new("reset");
        let listener = socket.bind();
        let shutdown = Arc::new(AtomicBool::new(false));

        let served = Arc::new(AtomicUsize::new(0));
        let accept_served = Arc::clone(&served);
        let accept_shutdown = Arc::clone(&shutdown);
        let thread = std::thread::spawn(move || {
            accept_loop(
                listener,
                &accept_shutdown,
                &null_log(),
                "test",
                move |mut stream| {
                    accept_served.fetch_add(1, Ordering::Release);
                    let mut request = [0u8; 4];
                    if stream.read_exact(&mut request).is_ok() {
                        let _ = stream.write_all(b"ok");
                    }
                },
            );
        });

        for _ in 0..8 {
            drop(UnixStream::connect(&socket.0).expect("connect and reset"));
        }

        let mut client =
            UnixStream::connect(&socket.0).expect("connect a real client");
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set a test read timeout");
        client.write_all(b"ping").expect("write a request");
        let mut answer = [0u8; 2];
        client.read_exact(&mut answer).expect("read the answer");

        assert_eq!(&answer, b"ok");
        assert!(served.load(Ordering::Acquire) > 0);

        shutdown.store(true, Ordering::Release);
        thread.join().expect("the accept thread must end");
    }
}
