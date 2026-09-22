// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The CONTROL verb on the vsock host socket.
//!
//! A host peer that sends `CONNECT <port>` is joined to a guest port. A
//! peer that sends `CONTROL` is answered by the VMM itself, and nothing
//! it sends reaches the guest mux. firehyve has no other control socket,
//! so this is how an operator reads the machine.
//!
//! # Grammar
//!
//! ```text
//! open     = "CONTROL" LF
//! banner   = "OK control" LF
//! request  = verb *( SP argument ) LF
//! response = ( "OK" *( SP field ) | "ERR" SP reason ) LF
//! field    = value *( "," value )
//! ```
//!
//! One request per line, one response line back. A list answer is
//! `OK <count>` followed by one field per record, and a record is its
//! values joined by commas, so a client splits on whitespace and then on
//! commas. No value carries a space or a comma.
//!
//! ```text
//! device-list -> OK 2 virtio-blk@4,0.4.0,present varstore,-,present
//! cpu-list    -> OK 2 0,present 1,present
//! mem-list    -> OK 1 boot,1073741824,present
//! ```
//!
//! # Trust
//!
//! The socket is 0600 in a 0700 directory, so the peer is the VMM's own
//! user. The input is still hostile: the request line is bounded, nothing
//! parsed from it indexes anything, and an unknown or malformed verb
//! closes the connection with one error line and changes no state.

use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slog::warn;

use vmm_core::unixsock::write_all_bounded;
use vmm_devices::pci::msix::MsixTable;
use vmm_devices::pci::LintrCfg;

use super::device::{
    VirtioVsock, VSOCK_MSIX_VECTORS, VSOCK_NUM_QUEUES, VSOCK_QUEUE_SIZE,
};
use crate::attach::VirtioAttachCtx;
use crate::bits;
use crate::pci::VirtioPciDevice;

/// Longest request accepted. A peer that never sends a newline must not
/// make the device read without bound. Wider than the CONNECT line
/// because the `device-add` argument is a whole `-s` spec.
const CONTROL_LINE_MAX: usize = 512;

/// How long a blocked read waits before it looks at the shutdown flag.
/// Without this, halt would wait for the peer instead of the guest.
pub(super) const LINE_POLL: Duration = Duration::from_millis(250);

/// How long a response line has to reach the peer.
///
/// This thread holds one of the capped reader slots. `SO_SNDTIMEO` bounds
/// nothing on illumos, so without this budget a peer that never reads
/// holds the slot for the life of the VM. Enough such peers block every
/// later connection, CONTROL included.
const RESPONSE_BUDGET: Duration = Duration::from_millis(500);

/// How long a peer has to send its opening verb.
///
/// Reader threads are capped, and only the shutdown flag frees a slot
/// held by a peer that connects and says nothing. Enough such peers
/// block every later connection, CONTROL included, for the life of the
/// VM.
pub(super) const HANDSHAKE_BUDGET: Duration = Duration::from_secs(2);

/// How long an established session may stay silent before it closes.
///
/// The session thread holds one of the capped reader slots that every
/// later CONNECT needs. Minutes, not seconds: an operator tool may hold
/// a session open between commands, and a lost session costs only a
/// reconnect.
pub(super) const CONTROL_IDLE_BUDGET: Duration = Duration::from_secs(300);

/// How many CONTROL sessions may run at once.
///
/// The idle budget alone does not stop starvation: a peer that sends one
/// byte inside every budget keeps its slot for ever. The quota is small
/// because one operator tool is the expected caller.
pub(super) const MAX_CONTROL_SESSIONS: usize = 4;

/// Longest value written back for one field. An id comes from the `-s`
/// arguments, so an operator can make it long.
pub const FIELD_MAX: usize = 64;

/// Longest refusal written back. A refusal can quote a value the peer
/// sent, so the line stays bounded.
pub const REASON_MAX: usize = 200;

/// An empty value renders as this, so every record keeps its shape.
const NO_VALUE: &str = "-";

/// One request, as the grammar names it.
///
/// Arguments are parsed here, so a sink never splits text, and a typo is
/// answered as a typo whatever the sink supports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlRequest {
    DeviceList,
    CpuList,
    MemList,
    /// Add the device one `-s` spec names.
    DeviceAdd(String),
    /// Ask the guest to give up the device with this id.
    DeviceRemove(String),
    CpuAdd(u32),
    /// Give the guest this many more bytes.
    MemAdd(u64),
    /// Named so the answer can say why the platform refuses it, rather
    /// than reading as a typo.
    CpuRemove,
    MemRemove,
}

impl ControlRequest {
    /// Parse one request line. The error is the reason the line is
    /// refused, ready for the wire.
    pub fn parse(line: &str) -> Result<Self, String> {
        let mut fields = line.split_whitespace();
        // A match guard cannot advance the iterator, so read all three
        // first. A `-s` spec has no space, so one field is a whole spec
        // and a second field makes the request malformed.
        let verb = fields.next().unwrap_or("");
        let argument = fields.next();
        let extra = fields.next();

        match (verb, argument, extra) {
            ("device-list", None, _) => Ok(Self::DeviceList),
            ("cpu-list", None, _) => Ok(Self::CpuList),
            ("mem-list", None, _) => Ok(Self::MemList),
            ("device-add", Some(spec), None) => {
                Ok(Self::DeviceAdd(spec.to_string()))
            }
            ("device-remove", Some(id), None) => {
                Ok(Self::DeviceRemove(id.to_string()))
            }
            // The id is never used as an index: the engine range-checks
            // it as well.
            ("cpu-add", Some(id), None) => id
                .parse()
                .map(Self::CpuAdd)
                .map_err(|_| format!("{} is not a CPU id", value(id))),
            ("mem-add", Some(bytes), None) => {
                bytes.parse().map(Self::MemAdd).map_err(|_| {
                    format!("{} is not a size in bytes", value(bytes))
                })
            }
            ("cpu-remove", _, _) => Ok(Self::CpuRemove),
            ("mem-remove", _, _) => Ok(Self::MemRemove),
            ("", _, _) => Err("empty request".to_string()),
            ("device-list" | "cpu-list" | "mem-list", Some(_), _) => {
                Err(format!("{} takes no argument", value(verb)))
            }
            (
                "device-add" | "device-remove" | "cpu-add" | "mem-add",
                None,
                _,
            ) => Err(format!("{} needs one argument", value(verb))),
            (
                "device-add" | "device-remove" | "cpu-add" | "mem-add",
                Some(_),
                Some(_),
            ) => Err(format!("{} takes one argument", value(verb))),
            _ => Err(format!("unknown command {}", value(verb))),
        }
    }
}

/// One answer. A record is its values joined by commas, and the reply
/// is `OK` and the records joined by spaces, or `ERR` and a reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlReply {
    Ok(Vec<Vec<String>>),
    Refused(String),
}

impl ControlReply {
    /// `OK` and one record.
    pub fn record(values: impl IntoIterator<Item = String>) -> Self {
        Self::Ok(vec![values.into_iter().collect()])
    }

    /// `OK <count>` and then one record each.
    pub fn list(records: Vec<Vec<String>>) -> Self {
        let mut out = Vec::with_capacity(records.len() + 1);
        out.push(vec![records.len().to_string()]);
        out.extend(records);
        Self::Ok(out)
    }

    pub fn refused(reason: impl Into<String>) -> Self {
        Self::Refused(reason.into())
    }

    /// The response line, with every value and the reason bounded and
    /// stripped down to what a terminal can show.
    pub fn render(&self) -> String {
        match self {
            Self::Ok(records) => {
                let mut out = String::from("OK");
                for record in records {
                    out.push(' ');
                    let record: Vec<String> =
                        record.iter().map(|v| value(v)).collect();
                    out.push_str(&record.join(","));
                }
                out
            }
            Self::Refused(reason) => format!("ERR {}", clean_reason(reason)),
        }
    }
}

/// One value, safe to put on the wire.
///
/// The grammar separates records by spaces and values by commas, and an
/// operator reads the answer on a terminal. So a value that came from
/// the command line keeps only plain characters, and the length is
/// capped.
fn value(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .take(FIELD_MAX)
        .map(|c| {
            if c.is_ascii_alphanumeric()
                || matches!(c, '-' | '_' | '.' | '@' | ':')
            {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        NO_VALUE.to_string()
    } else {
        cleaned
    }
}

/// One refusal, safe to put on the wire.
///
/// A refusal can quote a value the peer sent, so the text is capped and
/// stripped down to printable ASCII. Spaces stay: the grammar makes a
/// refusal one reason, not a record.
fn clean_reason(reason: &str) -> String {
    reason
        .chars()
        .take(REASON_MAX)
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Answers one control request.
///
/// Implemented by the VMM binary, which owns the device registry and the
/// CPU and memory configuration. The device knows the framing and
/// nothing else.
pub trait ControlSink: Send + Sync {
    fn handle(&self, request: ControlRequest) -> ControlReply;
}

/// One request line in, one response line out, with no newline.
pub fn answer_line(sink: &dyn ControlSink, line: &str) -> String {
    match ControlRequest::parse(line.trim()) {
        Ok(request) => sink.handle(request).render(),
        Err(reason) => ControlReply::refused(reason).render(),
    }
}

/// The device's sink, filled in after the device exists.
///
/// The sink reads the device registry, and the registry is not complete
/// until every device is attached, so the binary installs it once the
/// attach pass returns. Until then a CONTROL request is refused.
#[derive(Clone, Default)]
pub struct ControlSlot {
    sink: Arc<Mutex<Option<Arc<dyn ControlSink>>>>,
}

impl ControlSlot {
    /// Install the sink. Called once, at startup.
    pub fn install(&self, sink: Arc<dyn ControlSink>) {
        // No panic on a host-facing path. A panic cannot leave the one
        // `Option` half written, so install through the poison rather
        // than drop the sink.
        let mut slot = self.sink.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some(sink);
    }

    /// The installed sink, or `None` while the slot is empty.
    pub fn sink(&self) -> Option<Arc<dyn ControlSink>> {
        // No panic on a host-facing path: a poisoned lock reads as an
        // empty slot, which the caller refuses cleanly.
        self.sink.lock().ok().and_then(|guard| guard.clone())
    }
}

/// Build a virtio-vsock device and its PCI transport, and hand back the
/// control slot.
///
/// The same wiring as [`crate::attach::virtio_vsock`], plus the slot.
/// The device moves into the transport, so the slot is only reachable
/// here.
pub fn attach_with_control(
    socket_path: &Path,
    guest_cid: u64,
    lintr: Option<LintrCfg>,
    ctx: &VirtioAttachCtx<'_>,
    log: &slog::Logger,
) -> anyhow::Result<(Arc<VirtioPciDevice<VirtioVsock>>, ControlSlot)> {
    let vsock = VirtioVsock::new(
        socket_path,
        guest_cid,
        Arc::clone(ctx.physmap),
        log.clone(),
    )?;
    let cfg_size = vsock.config_size();
    let control = vsock.control_slot();

    let msix = Arc::new(MsixTable::new(VSOCK_MSIX_VECTORS, ctx.msi_sink()));

    let pci_dev = VirtioPciDevice::new(
        vsock,
        bits::VIRTIO_DEV_TYPE_VSOCK,
        VSOCK_NUM_QUEUES,
        VSOCK_QUEUE_SIZE,
        cfg_size,
        lintr,
        Arc::clone(ctx.physmap),
        Arc::clone(ctx.bus_pio),
        Arc::clone(ctx.bus_mmio),
        Some(msix),
    );

    // Only the receive queue completes out of band; the transport
    // raises for the transmit queue on its own.
    pci_dev.device().install_interrupt(pci_dev.backend_intr());

    Ok((pci_dev, control))
}

/// What one read of a peer's line produced.
pub(super) enum LineRead {
    Line(String),
    TooLong,
    NotUtf8,
    Closed,
}

/// Read one line from a host peer, bounded in length and in time.
///
/// The reader arms [`LINE_POLL`] on the stream itself, not the caller.
/// A connection the device has not registered yet is out of reach of
/// the halt sweep, so a silent peer would otherwise park the thread for
/// the life of the VM.
///
/// `deadline` bounds the whole read, which the poll does not. Without
/// one, a silent peer holds the thread until the VM shuts down.
///
/// Read one byte at a time, not through a `BufReader`. A `BufReader`
/// reads a whole block and drops any bytes the peer pipelined after the
/// newline.
pub(super) fn read_line_bounded(
    stream: &mut UnixStream,
    shutdown: &AtomicBool,
    max: usize,
    deadline: Option<Instant>,
) -> LineRead {
    if stream.set_read_timeout(Some(LINE_POLL)).is_err() {
        return LineRead::Closed;
    }
    let mut line: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if shutdown.load(Ordering::Acquire) {
            return LineRead::Closed;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return LineRead::Closed;
        }
        match stream.read(&mut byte) {
            Ok(0) => return LineRead::Closed,
            Ok(_) => {
                if byte[0] == b'\n' {
                    return match String::from_utf8(line) {
                        Ok(text) => LineRead::Line(text),
                        Err(_) => LineRead::NotUtf8,
                    };
                }
                if line.len() >= max {
                    return LineRead::TooLong;
                }
                line.push(byte[0]);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(_) => return LineRead::Closed,
        }
    }
}

/// One line of a sink's answer.
///
/// An embedded newline would put two lines on the wire for one request
/// and desynchronise every later exchange, so only the first line goes
/// out.
fn framed(answer: &str) -> &str {
    answer.split(['\r', '\n']).next().unwrap_or("")
}

/// Send one response line, and give up on a peer that will not take it.
///
/// The newline goes in the same write as the line, so one response
/// costs one budget and a stalled peer cannot leave a partial line.
///
/// [`write_all_bounded`] polls for space and then sends, so a second
/// writer on the same socket breaks the bound. [`run_control_session`]
/// takes the socket by value, this module never clones it, and the
/// CONTROL verb keeps it out of the device socket table. The `&mut`
/// enforces the single writer.
fn write_line(stream: &mut UnixStream, line: &str) -> io::Result<()> {
    let mut out = Vec::with_capacity(line.len() + 1);
    out.extend_from_slice(line.as_bytes());
    out.push(b'\n');
    write_all_bounded(stream, &out, RESPONSE_BUDGET)
}

/// Serve control requests until the peer closes or the device halts.
pub(super) fn run_control_session(
    slot: &ControlSlot,
    shutdown: &AtomicBool,
    mut stream: UnixStream,
    log: &slog::Logger,
    idle_budget: Duration,
) {
    // Each refusal writes one line and closes. The write is best effort:
    // the peer may already be gone.
    let Some(sink) = slot.sink() else {
        warn!(log, "vsock CONTROL refused; no control sink installed");
        let _ = write_line(&mut stream, "ERR control interface not available");
        return;
    };
    if write_line(&mut stream, "OK control").is_err() {
        return;
    }

    loop {
        // This thread holds a capped reader slot, so idle time between
        // requests is bounded.
        match read_line_bounded(
            &mut stream,
            shutdown,
            CONTROL_LINE_MAX,
            Some(Instant::now() + idle_budget),
        ) {
            LineRead::Line(request) => {
                let answer = answer_line(sink.as_ref(), &request);
                if write_line(&mut stream, framed(&answer)).is_err() {
                    return;
                }
            }
            // A truncated request cannot be resynchronised, so the
            // connection ends rather than acting on half a line.
            LineRead::TooLong => {
                let _ = write_line(&mut stream, "ERR request line too long");
                return;
            }
            LineRead::NotUtf8 => {
                let _ = write_line(&mut stream, "ERR request is not text");
                return;
            }
            LineRead::Closed => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::thread;

    use super::*;

    /// Assert the server ended the session.
    ///
    /// On Linux, a close with unread bytes in the receive queue sends
    /// RST, so the peer reads ConnectionReset. illumos and macOS report
    /// a clean end of file. The server does not drain a hostile peer's
    /// input before it closes, so both results mean closed.
    fn assert_session_closed(reader: &mut impl BufRead) {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            other => panic!("expected a closed session, got {other:?}"),
        }
    }

    /// Names the request it gets, so a test sees what the device passed.
    struct EchoSink;

    impl ControlSink for EchoSink {
        fn handle(&self, request: ControlRequest) -> ControlReply {
            match request {
                ControlRequest::DeviceList => {
                    ControlReply::record(["device-list".to_string()])
                }
                other => ControlReply::refused(format!("{other:?}")),
            }
        }
    }

    /// Answers with a value that holds a newline.
    struct TwoLineSink;

    impl ControlSink for TwoLineSink {
        fn handle(&self, _request: ControlRequest) -> ControlReply {
            ControlReply::record(["first\nsmuggled".to_string()])
        }
    }

    /// Answers with more than a socket buffer holds, so the write
    /// cannot finish until the peer reads.
    struct BulkSink;

    impl ControlSink for BulkSink {
        fn handle(&self, _request: ControlRequest) -> ControlReply {
            ControlReply::list(vec![vec!["x".repeat(FIELD_MAX)]; 20_000])
        }
    }

    fn null_log() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    /// Run a session on one end of a socket pair. Return the other end
    /// and the thread.
    fn session(
        slot: &ControlSlot,
        shutdown: Arc<AtomicBool>,
    ) -> (UnixStream, thread::JoinHandle<()>) {
        let (client, server) = UnixStream::pair().expect("socketpair");
        let slot = slot.clone();
        let handle = thread::spawn(move || {
            run_control_session(
                &slot,
                &shutdown,
                server,
                &null_log(),
                CONTROL_IDLE_BUDGET,
            );
        });
        (client, handle)
    }

    /// The same, with an idle budget the test can outlast.
    fn session_with_idle(
        slot: &ControlSlot,
        shutdown: Arc<AtomicBool>,
        idle: Duration,
    ) -> (UnixStream, thread::JoinHandle<()>) {
        let (client, server) = UnixStream::pair().expect("socketpair");
        let slot = slot.clone();
        let handle = thread::spawn(move || {
            run_control_session(&slot, &shutdown, server, &null_log(), idle);
        });
        (client, handle)
    }

    fn installed(sink: Arc<dyn ControlSink>) -> ControlSlot {
        let slot = ControlSlot::default();
        slot.install(sink);
        slot
    }

    #[test]
    fn control_with_no_sink_is_refused_and_closed() {
        let slot = ControlSlot::default();
        let (client, handle) = session(&slot, Arc::new(AtomicBool::new(false)));

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).expect("one line");

        assert_eq!(line, "ERR control interface not available\n");
        // Refused means closed: no second line follows.
        line.clear();
        assert_eq!(reader.read_line(&mut line).expect("eof"), 0);
        handle.join().expect("session thread");
    }

    #[test]
    fn a_request_round_trips_through_the_sink() {
        let slot = installed(Arc::new(EchoSink));
        let (mut client, handle) =
            session(&slot, Arc::new(AtomicBool::new(false)));

        write_line(&mut client, "device-list").expect("request");
        let mut reader = BufReader::new(client.try_clone().expect("clone"));
        let mut banner = String::new();
        reader.read_line(&mut banner).expect("banner");
        let mut answer = String::new();
        reader.read_line(&mut answer).expect("answer");

        assert_eq!(banner, "OK control\n");
        assert_eq!(answer, "OK device-list\n");
        drop(client);
        drop(reader);
        handle.join().expect("session thread");
    }

    #[test]
    fn an_empty_request_still_gets_one_answer() {
        // One response per request: a bare newline gets an answer too.
        let slot = installed(Arc::new(EchoSink));
        let (mut client, handle) =
            session(&slot, Arc::new(AtomicBool::new(false)));

        write_line(&mut client, "   ").expect("request");
        let mut reader = BufReader::new(client.try_clone().expect("clone"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("banner");
        line.clear();
        reader.read_line(&mut line).expect("answer");

        assert_eq!(line, "ERR empty request\n");
        drop(client);
        drop(reader);
        handle.join().expect("session thread");
    }

    #[test]
    fn an_over_long_request_is_refused_and_closed() {
        let slot = installed(Arc::new(EchoSink));
        let (mut client, handle) =
            session(&slot, Arc::new(AtomicBool::new(false)));

        // No newline: only the cap ends the read.
        client
            .write_all(&vec![b'x'; CONTROL_LINE_MAX * 2])
            .expect("request");
        let mut reader = BufReader::new(client.try_clone().expect("clone"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("banner");
        line.clear();
        reader.read_line(&mut line).expect("answer");

        assert_eq!(line, "ERR request line too long\n");
        // The rest of the request is unread, so this close can be RST.
        assert_session_closed(&mut reader);
        handle.join().expect("session thread");
    }

    #[test]
    fn a_request_that_is_not_text_is_refused() {
        let slot = installed(Arc::new(EchoSink));
        let (mut client, handle) =
            session(&slot, Arc::new(AtomicBool::new(false)));

        client.write_all(&[0xff, 0xfe, b'\n']).expect("request");
        let mut reader = BufReader::new(client.try_clone().expect("clone"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("banner");
        line.clear();
        reader.read_line(&mut line).expect("answer");

        assert_eq!(line, "ERR request is not text\n");
        handle.join().expect("session thread");
    }

    #[test]
    fn a_two_line_answer_cannot_desynchronise_the_stream() {
        let slot = installed(Arc::new(TwoLineSink));
        let (mut client, handle) =
            session(&slot, Arc::new(AtomicBool::new(false)));

        write_line(&mut client, "device-list").expect("request");
        let mut reader = BufReader::new(client.try_clone().expect("clone"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("banner");
        line.clear();
        reader.read_line(&mut line).expect("answer");

        assert_eq!(line, "OK first_smuggled\n");
        drop(client);
        drop(reader);
        handle.join().expect("session thread");
    }

    /// A peer that asks and never reads must not keep the session.
    ///
    /// The session thread holds a capped reader slot, so an unbounded
    /// write here blocks every other client for the life of the VM.
    #[test]
    fn a_peer_that_never_reads_its_answer_loses_the_session() {
        let slot = installed(Arc::new(BulkSink));
        let (mut client, handle) =
            session(&slot, Arc::new(AtomicBool::new(false)));

        // Nothing reads the socket: the banner and the answer fill it.
        client.write_all(b"device-list\n").expect("request");

        // Join on another thread so a session that never ends fails the
        // test instead of parking it.
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(handle.join().is_ok());
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(true),
            "the session must end on its write budget, not on the peer"
        );

        // Keep the peer open until here. A close would end the write on
        // a broken pipe, which is not the case under test.
        drop(client);
    }

    #[test]
    fn halt_ends_an_idle_session() {
        // The reader polls the flag, so an open session does not block
        // teardown.
        let slot = installed(Arc::new(EchoSink));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (client, handle) = session(&slot, Arc::clone(&shutdown));

        let mut reader = BufReader::new(client);
        let mut banner = String::new();
        reader.read_line(&mut banner).expect("banner");
        shutdown.store(true, Ordering::Release);

        handle.join().expect("session thread ends on the flag");
    }

    /// A silent CONTROL peer must give back its capped reader slot.
    /// Otherwise enough such peers make every later CONNECT fail.
    #[test]
    fn an_idle_session_loses_its_slot() {
        let slot = installed(Arc::new(EchoSink));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (client, handle) = session_with_idle(
            &slot,
            Arc::clone(&shutdown),
            Duration::from_millis(50),
        );

        let mut reader = BufReader::new(client);
        let mut banner = String::new();
        reader.read_line(&mut banner).expect("banner");
        assert_eq!(banner.trim_end(), "OK control");

        // Nothing is sent and the flag stays clear: only the idle budget
        // can end this.
        assert_session_closed(&mut reader);
        handle
            .join()
            .expect("session thread ends on its idle budget");
        assert!(!shutdown.load(Ordering::Acquire));
    }

    /// A peer that sends nothing must not park the thread.
    ///
    /// The reader arms its own poll timeout. A connection not yet in the
    /// socket table is out of reach of the halt sweep, so nothing else
    /// wakes it.
    #[test]
    fn a_silent_peer_does_not_park_the_reader() {
        let (_client, mut server) = UnixStream::pair().expect("socketpair");
        let shutdown = Arc::new(AtomicBool::new(false));

        let flag = Arc::clone(&shutdown);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            flag.store(true, Ordering::Release);
        });

        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let got = read_line_bounded(
                &mut server,
                &shutdown,
                CONTROL_LINE_MAX,
                None,
            );
            let _ = tx.send(matches!(got, LineRead::Closed));
        });

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(true),
            "the read must end on the flag, not on the peer"
        );
    }

    /// The CONNECT line cap is far shorter than the control request cap.
    #[test]
    fn the_line_cap_is_the_callers_to_pick() {
        let (mut client, mut server) = UnixStream::pair().expect("socketpair");
        let shutdown = Arc::new(AtomicBool::new(false));
        client.write_all(b"12345678\n").expect("write");

        assert!(matches!(
            read_line_bounded(&mut server, &shutdown, 4, None),
            LineRead::TooLong
        ));
    }

    #[test]
    fn only_the_first_line_of_an_answer_is_framed() {
        assert_eq!(framed("OK 1 a,b,c"), "OK 1 a,b,c");
        assert_eq!(framed("OK first\nERR smuggled"), "OK first");
        assert_eq!(framed("OK first\r\nERR smuggled"), "OK first");
        assert_eq!(framed("\nERR smuggled"), "");
    }

    #[test]
    fn a_poisoned_slot_still_takes_the_sink() {
        let slot = ControlSlot::default();
        let poisoner = slot.clone();
        thread::spawn(move || {
            let _guard = poisoner.sink.lock().expect("lock");
            panic!("poison the slot");
        })
        .join()
        .expect_err("the worker panics on purpose");

        slot.install(Arc::new(EchoSink));

        // A poisoned lock must not kill the VMM or lose the sink.
        let held = slot.sink.lock().unwrap_or_else(|e| e.into_inner());
        assert!(held.is_some());
    }

    #[test]
    fn an_empty_slot_hands_back_no_sink() {
        let slot = ControlSlot::default();
        assert!(slot.sink().is_none());
        slot.install(Arc::new(EchoSink));
        assert!(slot.sink().is_some());
        // The clone shares the slot: the binary installs into a clone.
        assert!(slot.clone().sink().is_some());
    }
}
