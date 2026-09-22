// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The host socket: the accept loop, the opening verb, and the reader
//! thread each connection gets.
//!
//! # Invariant
//!
//! Every thread here ends on the shutdown flag alone. The listener
//! polls instead of blocking in `accept`, a peer that sends no verb is
//! dropped on the handshake budget, and a blocked reader wakes on its
//! own poll. Halt never connects to a socket to wake a thread and never
//! waits on a host peer.

use std::io::{self, Read};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use slog::{debug, warn};
use vmm_core::unixsock::write_all_bounded;

use super::shared::{VsockShared, HOST_WRITE_TIMEOUT};
use crate::vsock::control::{
    read_line_bounded, run_control_session, LineRead, CONTROL_IDLE_BUDGET,
    HANDSHAKE_BUDGET,
};
use crate::vsock::packet::ConnKey;

/// Bytes read from a host socket in one go.
const HOST_READ_CHUNK: usize = 64 * 1024;
/// Pause before retrying a host read when the guest is not draining.
const BACKPRESSURE_PAUSE: Duration = Duration::from_millis(2);
/// How long a blocked host read waits before it checks the shutdown
/// flag. Without it, the reader wakes only on socket shutdown, and halt
/// spends its whole budget.
const HOST_READ_POLL: Duration = Duration::from_millis(250);
/// Longest CONNECT line accepted, so a client that sends no newline
/// cannot make the device read without bound.
pub(super) const CONNECT_LINE_MAX: usize = 64;

/// Accept host-initiated connections and parse their CONNECT line.
pub(super) fn accept_loop(shared: Arc<VsockShared>, listener: UnixListener) {
    let on_client = |stream: UnixStream| {
        // Each accepted connection costs a thread until its peer sends a
        // CONNECT line, which it may never do.
        let Some(slot) = shared.reserve_reader() else {
            debug!(shared.log, "virtio-vsock refused a peer";
                "reason" => "reader table full");
            let _ = stream.shutdown(Shutdown::Both);
            return;
        };
        let conn_shared = Arc::clone(&shared);
        // One thread per pending connection: the CONNECT read blocks,
        // and a silent client must not block the others.
        match std::thread::Builder::new()
            .name("vsock-connect".into())
            .spawn(move || handle_host_connect(conn_shared, stream))
        {
            // The dropped slot is given back, and the dropped closure
            // closes the peer socket, so the peer reads end of file.
            Err(error) => warn!(shared.log,
                "virtio-vsock connect thread not started"; "error" => %error),
            Ok(handle) => slot.fill(handle),
        }
    };
    crate::socket_accept::accept_loop(
        listener,
        &shared.shutdown,
        &shared.log,
        "virtio-vsock",
        on_client,
    );
}

/// The verb a host peer sent on its first line.
pub(super) enum HostVerb {
    /// Firecracker's `CONNECT <port>`: stream to a guest port.
    Connect(u32),
    /// `CONTROL`: the VMM answers this connection and the guest mux
    /// never sees it.
    Control,
    Unknown,
}

pub(super) fn classify_host_line(line: &str) -> HostVerb {
    if let Some(port) = parse_connect_line(line) {
        return HostVerb::Connect(port);
    }
    if line.trim_end_matches(['\r', '\n']).trim() == "CONTROL" {
        return HostVerb::Control;
    }
    HostVerb::Unknown
}

/// Read the opening line and act on the verb it names.
pub(super) fn handle_host_connect(
    shared: Arc<VsockShared>,
    mut stream: UnixStream,
) {
    // This connection is not in `sockets` yet, so the halt sweep cannot
    // wake it. The reader's own poll and the handshake budget stop a
    // silent peer from parking this thread.
    let LineRead::Line(line) = read_line_bounded(
        &mut stream,
        &shared.shutdown,
        CONNECT_LINE_MAX,
        Some(Instant::now() + HANDSHAKE_BUDGET),
    ) else {
        return;
    };
    match classify_host_line(&line) {
        HostVerb::Connect(port) => start_host_connection(shared, stream, port),
        HostVerb::Control => match shared.reserve_control_session() {
            Some(_session) => run_control_session(
                &shared.control,
                &shared.shutdown,
                stream,
                &shared.log,
                CONTROL_IDLE_BUDGET,
            ),
            None => {
                warn!(shared.log, "virtio-vsock CONTROL refused";
                    "reason" => "too many sessions");
                // Best effort: the peer may already be gone.
                let _ = write_all_bounded(
                    &stream,
                    b"ERR too many control sessions\n",
                    HOST_WRITE_TIMEOUT,
                );
            }
        },
        HostVerb::Unknown => {
            // Best effort: the peer may already be gone. Bounded, so a
            // peer that never reads cannot hold this thread and its
            // reader slot for the life of the VM.
            let _ = write_all_bounded(
                &stream,
                b"ERROR: expected CONNECT <port>\n",
                HOST_WRITE_TIMEOUT,
            );
        }
    }
}

/// Join a host peer to a guest port and stream until it closes.
pub(super) fn start_host_connection(
    shared: Arc<VsockShared>,
    stream: UnixStream,
    port: u32,
) {
    let s = stream;
    // The table is capped. Reserve the slot before the OK, so a peer is
    // never told OK for a connection the mux refused. The guest cannot
    // reach the connection yet.
    let Some(key) = shared.mux.reserve_host_connect(port) else {
        // Best effort: the peer may already be gone.
        let _ = write_all_bounded(
            &s,
            b"ERROR: too many connections\n",
            HOST_WRITE_TIMEOUT,
        );
        return;
    };
    // This thread owns the socket until the REQUEST is queued. After
    // that, a vCPU may be in write_host on this socket. Two writers
    // would interleave the OK line with guest payload, and each would
    // use the space the other's poll reported. So the OK line goes out
    // first. The socket goes in the table before the REQUEST, because
    // payload for a key with no socket has nowhere to go.
    if write_all_bounded(
        &s,
        format!("OK {}\n", key.host_port).as_bytes(),
        HOST_WRITE_TIMEOUT,
    )
    .is_err()
    {
        shared.mux.forget(key);
        return;
    }
    let socket = shared.register_socket(key, s);
    shared.mux.announce_host_connect(key);
    shared.deliver_rx();
    reader_loop(shared, key, socket);
}

/// Read from a connection's host socket until it closes.
pub(super) fn reader_loop(
    shared: Arc<VsockShared>,
    key: ConnKey,
    stream: Arc<UnixStream>,
) {
    // Arm the poll here, not inherit the handshake's timeout. An
    // unexpected timeout reads as a dead peer, and no timeout makes the
    // shutdown check unreachable.
    let polled = stream.set_read_timeout(Some(HOST_READ_POLL)).is_ok();
    let mut buf = vec![0u8; HOST_READ_CHUNK];
    loop {
        if shared.shutdown.load(Ordering::Acquire) {
            break;
        }
        let n = match (&*stream).read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            // An idle poll, not a dead peer. Loop to check the shutdown
            // flag again.
            Err(e)
                if polled
                    && matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
            {
                continue
            }
            Err(_) => break,
        };

        // The guest's window and the backlog each bound one pass, so one
        // read can take several passes. The mux removes what it took, so
        // `data` holds exactly the bytes the guest has not been given.
        let mut data = buf[..n].to_vec();
        loop {
            shared.mux.queue_host_data(key, &mut data);
            shared.deliver_rx();
            if data.is_empty() {
                break;
            }
            if shared.shutdown.load(Ordering::Acquire) {
                return;
            }
            // A guest packet reopens the window, and new RX chains free
            // backlog room. Both happen on other threads, so wait
            // instead of spinning. A guest that never reads stalls only
            // this connection.
            std::thread::sleep(BACKPRESSURE_PAUSE);
        }
    }

    // Only for the socket this reader served. The guest picks the key
    // and can reopen it while this thread wakes, and the new connection
    // must not die with this one.
    if shared.release_socket(key, &stream) {
        shared.mux.queue_host_close(key);
        shared.deliver_rx();
    }
}

/// Parse Firecracker's `CONNECT <port>` line.
pub fn parse_connect_line(line: &str) -> Option<u32> {
    let rest = line
        .trim_end_matches(['\r', '\n'])
        .strip_prefix("CONNECT ")?;
    rest.trim().parse().ok()
}
