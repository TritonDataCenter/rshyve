// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unix socket listener: connection limits, framing and peer authorisation.

use std::io::{self, BufRead, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use slog::{info, warn, Logger};
use vmm_core::unixsock::{accept, write_all_bounded};

use super::dispatch::dispatch;
use super::protocol::{Command, Response};
use super::{ControlError, VmController};
use crate::peercred::PeerCred;

const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_CONNECTIONS: usize = 8;
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How long one response may wait for the client to take it.
///
/// A connection holds one of `MAX_CONNECTIONS` slots for its whole
/// life. A peer that stops reading would keep its slot for ever, and
/// eight such peers would take the control socket away from every
/// later client, including the one that stops or migrates the VM.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// A running control listener.
///
/// Holding this keeps the accept loop alive, and dropping it closes it.
/// Teardown has to be able to stop taking commands: a `device-add` that
/// lands after the guest has stopped attaches a device the teardown
/// sweep has already snapshotted past.
pub struct ControlListener {
    path: PathBuf,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl ControlListener {
    /// The socket file, for the caller that removes it at teardown.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stop accepting and wait for the accept thread.
    ///
    /// Connections already in service continue. Each one has its own
    /// read and write deadline.
    pub fn close(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            drop(handle.join());
        }
    }
}

impl Drop for ControlListener {
    fn drop(&mut self) {
        self.close();
    }
}

/// Spawn the control socket listener thread.
///
/// Binds a Unix domain socket at `path`, removing any stale socket file
/// first.
pub fn spawn_control_thread(
    path: &Path,
    ctrl: Arc<VmController>,
    log: Logger,
) -> io::Result<ControlListener> {
    let listener = vmm_core::unixsock::bind_restricted(
        path,
        vmm_core::unixsock::SocketPolicy::default(),
    )?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let thread_shutdown = Arc::clone(&shutdown);
    let handle = thread::Builder::new()
        .name("control".into())
        .spawn(move || {
            let connection_log = log.clone();
            serve_connections(listener, &thread_shutdown, log, move |stream| {
                if let Err(e) = handle_connection(&ctrl, &stream, &connection_log) {
                    warn!(connection_log, "control connection error"; "error" => %e);
                }
            });
        })?;

    Ok(ControlListener {
        path: path.to_path_buf(),
        shutdown,
        handle: Some(handle),
    })
}

struct ConnectionGuard {
    active: Arc<AtomicUsize>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

fn acquire_connection(active: &Arc<AtomicUsize>) -> Option<ConnectionGuard> {
    active
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            (current < MAX_CONNECTIONS).then_some(current + 1)
        })
        .ok()?;
    Some(ConnectionGuard {
        active: Arc::clone(active),
    })
}

fn set_connection_deadlines(
    stream: &UnixStream,
    log: &Logger,
) -> io::Result<()> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    // A second guard only. It bounds a send where SO_SNDTIMEO works.
    // On illumos it reports success and does nothing, so responses go
    // through `write_all_bounded`. A failure here does not refuse the
    // client.
    if let Err(e) = stream.set_write_timeout(Some(WRITE_TIMEOUT)) {
        warn!(log, "control write timeout not set"; "error" => %e);
    }
    Ok(())
}

/// Accept clients until `shutdown`, one thread each.
///
/// The shared accept loop retries a transient error such as `EMFILE`
/// with a backoff. If the loop ended, the control socket would be gone
/// for the life of the VM.
fn serve_connections<F>(
    listener: UnixListener,
    shutdown: &AtomicBool,
    log: Logger,
    handler: F,
) where
    F: Fn(UnixStream) + Send + Sync + 'static,
{
    let active = Arc::new(AtomicUsize::new(0));
    let handler = Arc::new(handler);

    accept::accept_loop(listener, shutdown, &log, "control", |stream| {
        if let Err(e) = set_connection_deadlines(&stream, &log) {
            warn!(log, "failed to set control connection deadlines"; "error" => %e);
            return;
        }

        let Some(connection_guard) = acquire_connection(&active) else {
            if let Err(e) = write_error_response(
                &stream,
                "too many connections",
                WRITE_TIMEOUT,
            ) {
                warn!(log, "failed to reject excess control connection"; "error" => %e);
            }
            return;
        };

        let handler = Arc::clone(&handler);
        let thread_log = log.clone();
        if let Err(e) = thread::Builder::new()
            .name("control-connection".into())
            .spawn(move || {
                let _guard = connection_guard;
                handler(stream);
            })
        {
            warn!(thread_log, "failed to spawn control connection thread"; "error" => %e);
        }
    });
}

/// illumos global zone id. A global-zone peer already has full authority
/// over this zone, so refusing it would protect nothing and would break
/// a global-zone orchestrator, which drives the control socket of a VMM
/// inside a zone.
const GLOBAL_ZONEID: i32 = 0;

fn peer_is_authorized(peer: &PeerCred, euid: u32, zoneid: Option<i32>) -> bool {
    if peer.uid != euid {
        return false;
    }
    match (peer.zoneid, zoneid) {
        (Some(peer_zone), Some(own_zone)) => {
            peer_zone == own_zone || peer_zone == GLOBAL_ZONEID
        }
        // Platforms without zones: the uid check is the whole policy.
        (None, None) => true,
        // One side reported a zone and the other did not. Fail closed.
        _ => false,
    }
}

fn request_too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "control request exceeds maximum size",
    )
}

/// Read one newline-delimited request. `buf` never grows past the cap.
/// Returns `false` only at EOF before any byte of a request.
fn read_request<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> io::Result<bool> {
    buf.clear();

    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(!buf.is_empty());
        }

        if let Some(newline) = available.iter().position(|byte| *byte == b'\n')
        {
            let consumed = newline + 1;
            if consumed > MAX_REQUEST_BYTES - buf.len() {
                return Err(request_too_large());
            }
            buf.extend_from_slice(&available[..consumed]);
            reader.consume(consumed);
            buf.pop();
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(true);
        }

        if available.len() > MAX_REQUEST_BYTES - buf.len() {
            return Err(request_too_large());
        }
        let consumed = available.len();
        buf.extend_from_slice(available);
        reader.consume(consumed);
    }
}

/// Send one framed response, or give up after `budget`.
///
/// The two usual ways to bound this send do not work on illumos.
/// `SO_SNDTIMEO` reports success and bounds nothing, and `O_NONBLOCK`
/// is a file-status flag that the `BufReader` clone of this socket
/// shares, so it would break the request reads. `write_all_bounded`
/// polls for space instead.
fn write_response(
    stream: &UnixStream,
    payload: &[u8],
    budget: Duration,
) -> io::Result<()> {
    write_all_bounded(stream, payload, budget)
}

fn write_error_response(
    stream: &UnixStream,
    message: &str,
    budget: Duration,
) -> Result<(), ControlError> {
    let response = Response::err(message);
    let mut out = serde_json::to_vec(&response)?;
    out.push(b'\n');
    write_response(stream, &out, budget)?;
    Ok(())
}

/// Handle a single client connection: read JSON lines, dispatch, respond.
fn handle_connection(
    ctrl: &Arc<VmController>,
    stream: &UnixStream,
    log: &Logger,
) -> Result<(), ControlError> {
    let peer = crate::peercred::peer_cred(stream)?;
    // SAFETY: geteuid takes no arguments, touches no memory and cannot
    // fail.
    let euid = unsafe { libc::geteuid() };
    let zoneid = crate::peercred::process_zoneid()?;
    if !peer_is_authorized(&peer, euid, zoneid) {
        warn!(log, "rejected control connection"; "uid" => peer.uid);
        return Ok(());
    }

    let mut reader = BufReader::new(stream);
    let mut request = Vec::with_capacity(256);

    loop {
        match read_request(&mut reader, &mut request) {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                write_error_response(
                    stream,
                    "request too large",
                    WRITE_TIMEOUT,
                )?;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
        if request.is_empty() {
            continue;
        }

        match serde_json::from_slice::<Command>(&request) {
            Ok(cmd) => {
                info!(log, "control command"; "command" => cmd.name());

                // The metrics commands answer with raw text, not a
                // serialized `Response`.
                let raw = match &cmd {
                    Command::Metrics => {
                        let uptime = ctrl.start_time.elapsed().as_secs();
                        let vcpus: Vec<String> = ctrl
                            .vcpu_metrics
                            .iter()
                            .enumerate()
                            .map(|(i, v)| v.to_json(i as i32))
                            .collect();
                        Some(format!(
                            r#"{{"success":true,"uptime_secs":{},"vcpus":[{}]}}"#,
                            uptime,
                            vcpus.join(","),
                        ))
                    }
                    Command::MetricsPrometheus => {
                        let mut out = format!(
                            "# HELP vmm_uptime_seconds VMM uptime\n\
                             # TYPE vmm_uptime_seconds gauge\n\
                             vmm_uptime_seconds {}\n",
                            ctrl.start_time.elapsed().as_secs(),
                        );
                        for (i, v) in ctrl.vcpu_metrics.iter().enumerate() {
                            out.push_str(&v.to_prometheus(i as i32));
                        }
                        Some(out)
                    }
                    _ => None,
                };

                let mut out = if let Some(raw) = raw {
                    raw.into_bytes()
                } else {
                    let response = dispatch(ctrl, cmd);
                    serde_json::to_vec(&response)?
                };
                out.push(b'\n');
                write_response(stream, &out, WRITE_TIMEOUT)?;
            }
            Err(e) => {
                warn!(log, "invalid control command";
                    "error" => %e,
                    "bytes" => request.len(),
                );
                write_error_response(stream, "invalid command", WRITE_TIMEOUT)?;
            }
        }
    }

    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use std::sync::mpsc;

    #[test]
    fn read_request_rejects_oversize_line() {
        let mut input = vec![b'x'; MAX_REQUEST_BYTES];
        input.push(b'\n');
        let mut reader = BufReader::with_capacity(1024, Cursor::new(input));
        let mut request = Vec::new();

        let error = read_request(&mut reader, &mut request)
            .expect_err("oversize request must be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(request.len() <= MAX_REQUEST_BYTES);
    }

    #[test]
    fn read_request_accepts_max_size_line() {
        let mut input = vec![b'x'; MAX_REQUEST_BYTES - 1];
        input.push(b'\n');
        let mut reader = BufReader::with_capacity(1024, Cursor::new(input));
        let mut request = Vec::new();

        let present = read_request(&mut reader, &mut request)
            .expect("maximum-size request must be accepted");

        assert!(present);
        assert_eq!(request.len(), MAX_REQUEST_BYTES - 1);
    }

    #[test]
    fn peer_authorization_checks_uid_and_zone() {
        let local = PeerCred {
            uid: 42,
            gid: 7,
            zoneid: None,
        };
        let foreign_uid = PeerCred {
            uid: 43,
            gid: 7,
            zoneid: None,
        };
        let same_zone = PeerCred {
            uid: 42,
            gid: 7,
            zoneid: Some(3),
        };
        let foreign_zone = PeerCred {
            uid: 42,
            gid: 7,
            zoneid: Some(4),
        };

        assert!(peer_is_authorized(&local, 42, None));
        assert!(!peer_is_authorized(&foreign_uid, 42, None));
        assert!(peer_is_authorized(&same_zone, 42, Some(3)));
        assert!(!peer_is_authorized(&foreign_zone, 42, Some(3)));
    }

    #[test]
    fn global_zone_peer_is_authorized_against_a_zoned_vmm() {
        // A migration orchestrator runs in the GZ and drives the control
        // socket of a VMM inside a zone. Rejecting it would break migration.
        let gz_peer = PeerCred {
            uid: 0,
            gid: 0,
            zoneid: Some(GLOBAL_ZONEID),
        };
        assert!(peer_is_authorized(&gz_peer, 0, Some(7)));

        // A sibling zone is still refused.
        let sibling = PeerCred {
            uid: 0,
            gid: 0,
            zoneid: Some(8),
        };
        assert!(!peer_is_authorized(&sibling, 0, Some(7)));

        // A GZ peer with the wrong uid is still refused.
        let wrong_uid = PeerCred {
            uid: 1,
            gid: 0,
            zoneid: Some(GLOBAL_ZONEID),
        };
        assert!(!peer_is_authorized(&wrong_uid, 0, Some(7)));
    }

    #[test]
    fn mismatched_zone_reporting_fails_closed() {
        let zoned = PeerCred {
            uid: 0,
            gid: 0,
            zoneid: Some(0),
        };
        assert!(!peer_is_authorized(&zoned, 0, None));

        let unzoned = PeerCred {
            uid: 0,
            gid: 0,
            zoneid: None,
        };
        assert!(!peer_is_authorized(&unzoned, 0, Some(0)));
    }

    /// A client that stops reading must not hold its connection, and
    /// its slot, for ever.
    ///
    /// `SO_SNDTIMEO` reports success on illumos and bounds nothing, so
    /// without the poll in `write_all_bounded` eight such clients would
    /// take the control socket away from everyone else.
    #[test]
    fn a_client_that_never_reads_does_not_hold_its_slot() {
        use std::time::Instant;

        let (_peer, server) =
            UnixStream::pair().expect("create a control socket pair");
        let budget = Duration::from_millis(200);

        // Fill the socket, the way pipelined commands whose answers
        // nobody reads would.
        let error = write_response(&server, &vec![b'x'; 8 << 20], budget)
            .expect_err("a peer that reads nothing must not be waited on");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);

        // The next response is small and still has no room, which is
        // where a plain write would stop for good.
        let started = Instant::now();
        let error =
            write_error_response(&server, "too many connections", budget)
                .expect_err("a full socket must not be waited on");
        assert!(
            matches!(&error, ControlError::Io(e)
                if e.kind() == io::ErrorKind::TimedOut),
            "unexpected error: {error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the response took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn second_client_is_served_while_first_is_idle() {
        static NEXT_SOCKET: AtomicUsize = AtomicUsize::new(0);

        let id = NEXT_SOCKET.fetch_add(1, Ordering::Relaxed);
        let temp_dir = std::env::temp_dir().join(format!(
            "vmm-control-concurrency-{}-{id}",
            std::process::id(),
        ));
        std::fs::create_dir(&temp_dir).expect("create control test directory");
        let path = temp_dir.join("control.sock");
        let listener =
            UnixListener::bind(&path).expect("bind control test socket");
        let log = Logger::root(slog::Discard, slog::o!());
        let first_connection = Arc::new(AtomicBool::new(true));
        let first_for_handler = Arc::clone(&first_connection);
        let (started_tx, started_rx) = mpsc::channel();

        let shutdown = Arc::new(AtomicBool::new(false));
        let accept_shutdown = Arc::clone(&shutdown);
        let accepting = thread::spawn(move || {
            serve_connections(listener, &accept_shutdown, log, move |stream| {
                if first_for_handler.swap(false, Ordering::AcqRel) {
                    started_tx.send(()).expect("signal first connection start");
                }
                let mut reader = BufReader::new(&stream);
                let mut request = Vec::new();
                if matches!(read_request(&mut reader, &mut request), Ok(true)) {
                    let mut writer = &stream;
                    writer.write_all(b"ok\n").expect("write test response");
                }
            });
        });

        let _idle = UnixStream::connect(&path).expect("connect idle client");
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("idle client handler must start");

        let mut second =
            UnixStream::connect(&path).expect("connect second client");
        second
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set test read timeout");
        second.write_all(b"status\n").expect("write second request");
        let mut response = String::new();
        BufReader::new(&second)
            .read_line(&mut response)
            .expect("read second response");

        assert_eq!(response, "ok\n");

        // A listener that could not be stopped would hold the socket
        // open for the life of the process.
        shutdown.store(true, Ordering::Release);
        accepting.join().expect("the accept thread must end");

        std::fs::remove_file(&path).expect("remove control test socket");
        std::fs::remove_dir(&temp_dir).expect("remove control test directory");
    }
}
