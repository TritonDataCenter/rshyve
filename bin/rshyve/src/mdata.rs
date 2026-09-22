// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SmartOS metadata agent for guest configuration.
//!
//! Serves the mdata V2 protocol on a UART (COM2). It gives the guest its
//! network config, hostname, SSH keys and user-script.
//!
//! # Protocol (V2)
//!
//! Request:  `V2 <body_len> <crc32_hex> <reqid> <command> [<b64_arg>]\n`
//! Response: `V2 <body_len> <crc32_hex> <reqid> <status> [<b64_data>]\n`
//!
//! Negotiation: client sends `NEGOTIATE V2\n`, server responds `V2_OK\n`
//!
//! # What reaches the log
//!
//! The store holds the root password and the SSH keys, and every byte
//! of a request comes from the guest. No request line, key value or
//! response body is ever logged. Key names go out at debug, and every
//! line the guest can provoke is rate limited per site, so a guest that
//! writes to COM2 in a loop cannot flood the host log.

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use slog::{debug, info, Logger};
use vmm_core::ratelimit::TokenBucket;
use vmm_devices::uart::lpc::LpcUart;

use crate::serial::TxQueue;

/// Longest request line the agent buffers. A longer line is refused
/// whole, not processed truncated.
const MAX_LINE: usize = 4096;

/// How long one response may wait for the guest to drain the RX FIFO
/// before the rest of it is dropped.
const RESPONSE_BUDGET: Duration = Duration::from_secs(2);

/// Bytes the guest may have in flight on COM2 before the sink drops.
const TX_QUEUE_BYTES: usize = 2 * MAX_LINE;

/// Key/value store the agent answers from. Read-only once built.
pub struct MdataStore {
    entries: BTreeMap<String, String>,
}

impl MdataStore {
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.entries.insert(key.into(), value.into());
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(|s| s.as_str())
    }

    pub fn keys(&self) -> Vec<&str> {
        self.entries.keys().map(|k| k.as_str()).collect()
    }
}

/// Start the metadata agent on COM2.
///
/// The UART's TX sink feeds a bounded queue the agent thread blocks on,
/// so an idle agent costs no wakeups and a flooding guest costs
/// dropped bytes, not memory.
pub fn start_mdata_agent(
    uart: Arc<LpcUart>,
    store: Arc<MdataStore>,
    log: Logger,
) -> io::Result<()> {
    let queue = TxQueue::install(&uart, TX_QUEUE_BYTES);
    thread::Builder::new()
        .name("mdata-agent".into())
        .spawn(move || {
            mdata_loop(queue, &uart, &store, &log);
        })
        .map(drop)
}

/// The log sites a guest can provoke, each with its own budget.
struct Limits {
    bad_line: TokenBucket,
    bad_frame: TokenBucket,
    slow_guest: TokenBucket,
    dropped: TokenBucket,
}

impl Limits {
    fn new() -> Self {
        let per_minute =
            |burst| TokenBucket::new(burst, Duration::from_secs(60));
        Self {
            bad_line: per_minute(5),
            bad_frame: per_minute(5),
            slow_guest: per_minute(2),
            dropped: per_minute(2),
        }
    }
}

/// The agent's parser state between lines.
struct Session {
    line: Vec<u8>,
    /// The line is longer than [`MAX_LINE`]. It is refused when it ends.
    overlong: bool,
    v2: bool,
}

impl Session {
    fn new() -> Self {
        Self {
            line: Vec::with_capacity(256),
            overlong: false,
            v2: false,
        }
    }

    /// Take one byte from the guest. `Some` when a line is complete.
    fn push(&mut self, byte: u8) -> Option<Line> {
        match byte {
            b'\n' => {
                let text = String::from_utf8_lossy(&self.line).into_owned();
                let overlong = self.overlong;
                self.line.clear();
                self.overlong = false;
                Some(Line { text, overlong })
            }
            b'\r' => None,
            // Only printable ASCII is a request. Control bytes are the
            // firmware probing the port.
            b if (0x20..0x7F).contains(&b) => {
                if self.line.len() < MAX_LINE {
                    self.line.push(b);
                } else {
                    self.overlong = true;
                }
                None
            }
            _ => None,
        }
    }
}

struct Line {
    text: String,
    overlong: bool,
}

fn mdata_loop(
    queue: TxQueue,
    uart: &LpcUart,
    store: &MdataStore,
    log: &Logger,
) {
    let mut session = Session::new();
    let limits = Limits::new();

    info!(log, "mdata agent waiting for guest requests on COM2");

    while let Ok(byte) = queue.bytes.recv() {
        let Some(line) = session.push(byte) else {
            continue;
        };
        let dropped = queue.take_dropped();
        if dropped > 0 && limits.dropped.take() {
            debug!(log, "mdata: guest output dropped"; "bytes" => dropped);
        }
        let response = answer(&mut session, line, store, log, &limits);
        if let Err(sent) = send_line(uart, &response, RESPONSE_BUDGET) {
            if limits.slow_guest.take() {
                debug!(log, "mdata: response abandoned, guest not reading COM2";
                    "sent" => sent, "len" => response.len());
            }
        }
    }
}

/// The reply to one line. Never logs the line or the reply.
fn answer(
    session: &mut Session,
    line: Line,
    store: &MdataStore,
    log: &Logger,
    limits: &Limits,
) -> String {
    if line.overlong {
        if limits.bad_line.take() {
            debug!(log, "mdata: request line over the limit refused";
                "limit" => MAX_LINE);
        }
        return if session.v2 {
            build_v2_response(NO_REQID, "FAILURE", None)
        } else {
            "invalid command".to_string()
        };
    }
    if line.text.is_empty() {
        // A bare newline is the client's serial reset probe.
        return "invalid command".to_string();
    }
    if line.text == "NEGOTIATE V2" {
        session.v2 = true;
        debug!(log, "mdata: V2 negotiated");
        return "V2_OK".to_string();
    }
    if session.v2 && line.text.starts_with("V2 ") {
        return process_v2_frame(&line.text, store, log, limits);
    }
    process_v1_command(&line.text, store, log, limits)
}

/// Send a line and its terminator to the guest through COM2 RX.
fn send_line(
    uart: &LpcUart,
    line: &str,
    budget: Duration,
) -> Result<(), usize> {
    let mut out = Vec::with_capacity(line.len() + 1);
    out.extend_from_slice(line.as_bytes());
    out.push(b'\n');
    crate::serial::input_bytes(uart, &out, budget)
}

/// The request id a reply carries when the request had no usable one.
const NO_REQID: &str = "00000000";

/// A parsed V2 frame body.
#[derive(Debug)]
struct Frame<'a> {
    reqid: &'a str,
    command: &'a str,
    arg: Option<&'a str>,
}

/// Why a V2 frame was refused. Carries no guest bytes.
#[derive(Debug, PartialEq, Eq)]
enum FrameError {
    Fields,
    Length,
    Crc,
    Reqid,
}

/// Check the framing of `V2 <len> <crc> <body>` and split the body.
fn parse_v2_frame(line: &str) -> Result<Frame<'_>, FrameError> {
    let mut parts = line.splitn(4, ' ');
    let _v2 = parts.next();
    let (Some(len), Some(crc), Some(body)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return Err(FrameError::Fields);
    };
    let len: usize = len.parse().map_err(|_| FrameError::Length)?;
    if len != body.len() {
        return Err(FrameError::Length);
    }
    let crc = u32::from_str_radix(crc, 16).map_err(|_| FrameError::Crc)?;
    if crc != crc32(body) {
        return Err(FrameError::Crc);
    }

    let mut body_parts = body.splitn(3, ' ');
    let reqid = body_parts.next().unwrap_or("");
    if !is_reqid(reqid) {
        return Err(FrameError::Reqid);
    }
    Ok(Frame {
        reqid,
        command: body_parts.next().unwrap_or(""),
        arg: body_parts.next(),
    })
}

/// Eight hex digits, as the protocol defines the request id.
fn is_reqid(s: &str) -> bool {
    s.len() == 8 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Process a V2 framed request.
fn process_v2_frame(
    line: &str,
    store: &MdataStore,
    log: &Logger,
    limits: &Limits,
) -> String {
    let frame = match parse_v2_frame(line) {
        Ok(frame) => frame,
        Err(reason) => {
            if limits.bad_frame.take() {
                debug!(log, "mdata: V2 frame refused"; "reason" => ?reason);
            }
            return build_v2_response(NO_REQID, "FAILURE", None);
        }
    };

    match frame.command {
        "GET" => {
            let Some(key) = frame.arg.and_then(b64_decode) else {
                if limits.bad_frame.take() {
                    debug!(log, "mdata: V2 GET without a decodable key");
                }
                return build_v2_response(frame.reqid, "FAILURE", None);
            };
            let value = store.get(&key);
            log_get(log, limits, &key, value.is_some());
            match value {
                Some(value) => {
                    build_v2_response(frame.reqid, "SUCCESS", Some(value))
                }
                None => build_v2_response(frame.reqid, "NOTFOUND", None),
            }
        }
        "KEYS" => {
            debug!(log, "mdata: V2 KEYS");
            let keys = store.keys().join("\n");
            build_v2_response(frame.reqid, "SUCCESS", Some(&keys))
        }
        // The store is read-only.
        "PUT" | "DELETE" => build_v2_response(frame.reqid, "FAILURE", None),
        other => {
            if limits.bad_frame.take() {
                debug!(log, "mdata: unknown V2 command"; "len" => other.len());
            }
            build_v2_response(frame.reqid, "FAILURE", None)
        }
    }
}

/// Build a V2 response frame.
fn build_v2_response(reqid: &str, status: &str, data: Option<&str>) -> String {
    let body = match data {
        Some(d) if !d.is_empty() => {
            format!("{} {} {}", reqid, status, b64_encode(d))
        }
        _ => format!("{} {}", reqid, status),
    };
    let crc = crc32(&body);
    format!("V2 {} {:08x} {}", body.len(), crc, body)
}

/// Process a V1 command (fallback).
fn process_v1_command(
    line: &str,
    store: &MdataStore,
    log: &Logger,
    limits: &Limits,
) -> String {
    let (cmd, arg) = line.split_once(' ').unwrap_or((line, ""));

    match cmd {
        "GET" => {
            let key = arg.trim();
            if key.is_empty() {
                return "invalid command".to_string();
            }
            let value = store.get(key);
            log_get(log, limits, key, value.is_some());
            match value {
                Some(value) => {
                    let mut resp = String::from("SUCCESS\n");
                    for line in value.lines() {
                        if line.starts_with('.') {
                            resp.push('.');
                        }
                        resp.push_str(line);
                        resp.push('\n');
                    }
                    resp.push('.');
                    resp
                }
                None => "NOTFOUND".to_string(),
            }
        }
        "KEYS" => {
            debug!(log, "mdata: V1 KEYS");
            let mut resp = String::from("SUCCESS\n");
            for key in store.keys() {
                resp.push_str(key);
                resp.push('\n');
            }
            resp.push('.');
            resp
        }
        _ => {
            if limits.bad_line.take() {
                debug!(log, "mdata: unknown V1 command"; "len" => line.len());
            }
            "invalid command".to_string()
        }
    }
}

/// Record which key was asked for.
///
/// A key the store holds is one of a fixed set of names an operator
/// needs to see. Anything else is guest bytes, and a guest is free to
/// put a password there, so only its length goes out.
fn log_get(log: &Logger, limits: &Limits, key: &str, found: bool) {
    if found {
        debug!(log, "mdata: GET"; "key" => key);
    } else if limits.bad_line.take() {
        debug!(log, "mdata: GET of a key the store does not hold";
            "len" => key.len());
    }
}

fn b64_encode(s: &str) -> String {
    BASE64.encode(s)
}

fn b64_decode(s: &str) -> Option<String> {
    String::from_utf8(BASE64.decode(s).ok()?).ok()
}

/// The V2 frame checksum: CRC32 as zlib computes it.
fn crc32(data: &str) -> u32 {
    crc32fast::hash(data.as_bytes())
}

/// Build the metadata store from zone config / CLI flags.
pub fn build_store_from_zone_config(
    hostname: &str,
    uuid: Option<&str>,
    nics_json: Option<&str>,
    resolvers_json: Option<&str>,
    ssh_keys: Option<&str>,
    root_pw: Option<&str>,
) -> MdataStore {
    let mut store = MdataStore::new();

    store.set("sdc:hostname", hostname);
    // cloud-init's SmartOS datasource does not persist without a uuid.
    store.set("sdc:uuid", uuid.unwrap_or(hostname));

    if let Some(nics) = nics_json {
        store.set("sdc:nics", nics);
    }

    // No key at all when the operator named none: the guest then keeps
    // its own resolver config instead of being sent to a third party.
    if let Some(resolvers) = resolvers_json {
        store.set("sdc:resolvers", resolvers);
    }

    if let Some(keys) = ssh_keys {
        store.set("root_authorized_keys", keys);
    }

    if let Some(pw) = root_pw {
        store.set("root_pw", pw);
    }

    // Keys that cloud-init's SmartOS datasource expects to exist.
    store.set("sdc:maintain_resolvers", "true");
    store.set("sdc:routes", "[]");
    store.set("sdc:dns_domain", "");
    store.set("user-script", "");
    store.set("user-data", "");
    store.set("sdc:vendor-data", "");
    store.set("sdc:operator-script", "");

    store
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use slog::{Drain, KV};
    use vmm_core::pio::PioBus;
    use vmm_devices::uart::lpc::COM2_BASE;

    use super::*;

    const SECRET: &str = "hunter2-very-secret";

    fn store() -> MdataStore {
        let mut store = MdataStore::new();
        store.set("root_pw", SECRET);
        store.set("test-key", "test-value");
        store
    }

    fn null_log() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    /// Every message and every key/value pair a logger emitted.
    #[derive(Default)]
    struct Capture(Mutex<Vec<String>>);

    struct Collect<'a>(&'a mut Vec<String>);

    impl slog::Serializer for Collect<'_> {
        fn emit_arguments(
            &mut self,
            key: slog::Key,
            val: &std::fmt::Arguments<'_>,
        ) -> slog::Result {
            self.0.push(format!("{key}={val}"));
            Ok(())
        }
    }

    /// Owns the capture so the drain type is local to this crate.
    struct CaptureDrain(Arc<Capture>);

    impl Drain for CaptureDrain {
        type Ok = ();
        type Err = slog::Never;

        fn log(
            &self,
            record: &slog::Record<'_>,
            values: &slog::OwnedKVList,
        ) -> Result<(), slog::Never> {
            let mut lines = self.0 .0.lock().expect("capture lock");
            lines.push(format!("{}", record.msg()));
            let mut out = Vec::new();
            record
                .kv()
                .serialize(record, &mut Collect(&mut out))
                .expect("serialize");
            values
                .serialize(record, &mut Collect(&mut out))
                .expect("serialize");
            lines.extend(out);
            Ok(())
        }
    }

    fn capturing_log() -> (Logger, Arc<Capture>) {
        let capture = Arc::new(Capture::default());
        let log =
            Logger::root(CaptureDrain(Arc::clone(&capture)).fuse(), slog::o!());
        (log, capture)
    }

    fn frame(body: &str) -> String {
        format!("V2 {} {:08x} {}", body.len(), crc32(body), body)
    }

    fn get_frame(key: &str) -> String {
        frame(&format!("dc4fae17 GET {}", b64_encode(key)))
    }

    fn parse_response(resp: &str) -> (usize, u32, &str) {
        let mut parts = resp.splitn(4, ' ');
        assert_eq!(parts.next(), Some("V2"));
        let len = parts.next().unwrap().parse().unwrap();
        let crc = u32::from_str_radix(parts.next().unwrap(), 16).unwrap();
        (len, crc, parts.next().unwrap())
    }

    #[test]
    fn crc32_known_value() {
        assert_eq!(crc32("123456789"), 0xCBF4_3926);
    }

    #[test]
    fn b64_roundtrip() {
        let orig = "hello world";
        let decoded = b64_decode(&b64_encode(orig)).unwrap();
        assert_eq!(decoded, orig);
    }

    #[test]
    fn v2_response_format() {
        let resp = build_v2_response("aabbccdd", "SUCCESS", Some("test"));
        let (len, crc, body) = parse_response(&resp);
        assert_eq!(body.len(), len);
        assert_eq!(crc32(body), crc);
    }

    #[test]
    fn a_well_formed_v2_get_is_answered() {
        let resp = process_v2_frame(
            &get_frame("test-key"),
            &store(),
            &null_log(),
            &Limits::new(),
        );
        let (_, _, body) = parse_response(&resp);
        assert_eq!(
            body,
            format!("dc4fae17 SUCCESS {}", b64_encode("test-value"))
        );
    }

    #[test]
    fn a_v2_frame_with_the_wrong_length_is_refused() {
        let body = format!("dc4fae17 GET {}", b64_encode("test-key"));
        let line =
            format!("V2 {} {:08x} {}", body.len() + 1, crc32(&body), body);

        assert_eq!(parse_v2_frame(&line).unwrap_err(), FrameError::Length);
        let resp =
            process_v2_frame(&line, &store(), &null_log(), &Limits::new());
        assert_eq!(parse_response(&resp).2, "00000000 FAILURE");
    }

    #[test]
    fn a_v2_frame_with_the_wrong_crc_is_refused() {
        let body = format!("dc4fae17 GET {}", b64_encode("test-key"));
        let line = format!("V2 {} 00000000 {}", body.len(), body);

        assert_eq!(parse_v2_frame(&line).unwrap_err(), FrameError::Crc);
        let resp =
            process_v2_frame(&line, &store(), &null_log(), &Limits::new());
        assert!(resp.ends_with("00000000 FAILURE"), "{resp}");
    }

    #[test]
    fn a_v2_frame_without_a_body_is_refused() {
        assert_eq!(
            parse_v2_frame("V2 5 abcd").unwrap_err(),
            FrameError::Fields
        );
        assert_eq!(parse_v2_frame("V2").unwrap_err(), FrameError::Fields);
    }

    #[test]
    fn a_request_id_that_is_not_eight_hex_digits_is_refused() {
        for reqid in ["dc4fae1", "dc4fae178", "zz4fae17", ""] {
            let line = frame(&format!("{reqid} GET {}", b64_encode("k")));
            assert_eq!(
                parse_v2_frame(&line).unwrap_err(),
                FrameError::Reqid,
                "{reqid:?}",
            );
        }
        assert!(is_reqid("DC4FAE17"));
    }

    #[test]
    fn a_key_that_is_not_base64_or_not_utf8_is_refused() {
        let limits = Limits::new();
        for arg in ["not*base64", &BASE64.encode([0xffu8, 0xfe])] {
            let line = frame(&format!("dc4fae17 GET {arg}"));
            let resp = process_v2_frame(&line, &store(), &null_log(), &limits);
            assert_eq!(parse_response(&resp).2, "dc4fae17 FAILURE");
        }
    }

    #[test]
    fn an_unknown_v2_command_and_a_put_are_refused_with_the_request_id() {
        let limits = Limits::new();
        for body in ["dc4fae17 FROB", "dc4fae17 PUT a", "dc4fae17 DELETE a"] {
            let resp =
                process_v2_frame(&frame(body), &store(), &null_log(), &limits);
            assert_eq!(parse_response(&resp).2, "dc4fae17 FAILURE", "{body}");
        }
    }

    #[test]
    fn a_missing_key_is_notfound() {
        let resp = process_v2_frame(
            &get_frame("missing"),
            &store(),
            &null_log(),
            &Limits::new(),
        );
        assert_eq!(parse_response(&resp).2, "dc4fae17 NOTFOUND");
    }

    #[test]
    fn v1_get_existing() {
        let resp = process_v1_command(
            "GET test-key",
            &store(),
            &null_log(),
            &Limits::new(),
        );
        assert_eq!(resp, "SUCCESS\ntest-value\n.");
    }

    #[test]
    fn v1_get_missing() {
        let resp = process_v1_command(
            "GET missing",
            &store(),
            &null_log(),
            &Limits::new(),
        );
        assert_eq!(resp, "NOTFOUND");
    }

    #[test]
    fn a_line_over_the_limit_is_refused_whole() {
        // Truncating it would process a frame the guest never sent.
        let mut session = Session::new();
        session.v2 = true;
        for _ in 0..MAX_LINE + 10 {
            assert!(session.push(b'A').is_none());
        }
        let line = session.push(b'\n').expect("a line");
        assert!(line.overlong);
        assert_eq!(line.text.len(), MAX_LINE);

        let resp =
            answer(&mut session, line, &store(), &null_log(), &Limits::new());
        assert_eq!(parse_response(&resp).2, "00000000 FAILURE");

        // The next line starts clean.
        let next = session.push(b'\n').expect("a line");
        assert!(!next.overlong);
        assert!(next.text.is_empty());
    }

    #[test]
    fn control_bytes_and_carriage_returns_are_not_part_of_a_line() {
        let mut session = Session::new();
        for b in b"\x00GET\r x\x7f" {
            assert!(session.push(*b).is_none());
        }
        let line = session.push(b'\n').expect("a line");
        assert_eq!(line.text, "GET x");
    }

    #[test]
    fn a_session_answers_the_reset_probe_and_negotiates_v2() {
        let mut session = Session::new();
        let limits = Limits::new();
        let log = null_log();
        let line = |text: &str| Line {
            text: text.to_string(),
            overlong: false,
        };

        assert_eq!(
            answer(&mut session, line(""), &store(), &log, &limits),
            "invalid command"
        );
        assert_eq!(
            answer(&mut session, line("NEGOTIATE V2"), &store(), &log, &limits),
            "V2_OK"
        );
        assert!(session.v2);
        let resp = answer(
            &mut session,
            line(&get_frame("test-key")),
            &store(),
            &log,
            &limits,
        );
        assert!(resp.starts_with("V2 "), "{resp}");
    }

    #[test]
    fn no_secret_reaches_the_log() {
        let (log, capture) = capturing_log();
        let mut session = Session::new();
        let limits = Limits::new();
        let store = store();
        let secret_b64 = b64_encode(SECRET);
        let lines = [
            "NEGOTIATE V2".to_string(),
            get_frame("root_pw"),
            // A guest that writes a value where a key goes.
            get_frame(SECRET),
            frame(&format!("dc4fae17 {SECRET}")),
            format!("GET {SECRET}"),
            format!("{SECRET} junk"),
            "V2 1 0 x".to_string(),
        ];
        for text in lines {
            answer(
                &mut session,
                Line {
                    text,
                    overlong: false,
                },
                &store,
                &log,
                &limits,
            );
        }

        let logged = capture.0.lock().expect("capture lock").join("\n");
        assert!(!logged.contains(SECRET), "{logged}");
        assert!(!logged.contains(&secret_b64), "{logged}");
        assert!(!logged.contains("test-value"), "{logged}");
        // The key name is what an operator needs to see.
        assert!(logged.contains("key=root_pw"), "{logged}");
    }

    #[test]
    fn a_flood_of_bad_lines_is_logged_a_bounded_number_of_times() {
        let (log, capture) = capturing_log();
        let mut session = Session::new();
        let limits = Limits::new();
        let store = store();
        for _ in 0..100 {
            answer(
                &mut session,
                Line {
                    text: "FROB".to_string(),
                    overlong: false,
                },
                &store,
                &log,
                &limits,
            );
        }

        let logged = capture.0.lock().expect("capture lock");
        let count = logged
            .iter()
            .filter(|line| line.contains("unknown V1 command"))
            .count();
        assert!(count > 0 && count <= 5, "{count}");
    }

    #[test]
    fn a_response_carries_its_terminator_into_the_rx_fifo() {
        let pin = Arc::new(vmm_core::intr_pins::NoOpPin);
        let uart = LpcUart::new(pin);
        let bus = PioBus::new();
        uart.attach(&bus, COM2_BASE);

        send_line(&uart, "V2_OK", RESPONSE_BUDGET).expect("a short reply");

        let read: Vec<u8> =
            (0..6).map(|_| bus.handle_in(COM2_BASE, 1) as u8).collect();
        assert_eq!(read, b"V2_OK\n");
    }

    #[test]
    fn guest_bytes_reach_the_agent_through_the_sink_not_a_poll() {
        let pin = Arc::new(vmm_core::intr_pins::NoOpPin);
        let bus = PioBus::new();
        let uart = LpcUart::new(pin);
        uart.attach(&bus, COM2_BASE);
        let queue = TxQueue::install(&uart, 8);

        for b in b"GET x\n" {
            bus.handle_out(COM2_BASE, 1, u32::from(*b));
        }

        let mut got = Vec::new();
        while let Ok(b) = queue.bytes.try_recv() {
            got.push(b);
        }
        assert_eq!(got, b"GET x\n");
        // The sink took them, so the TX FIFO has nothing left to poll.
        assert_eq!(uart.output_byte(), None);
    }
}
