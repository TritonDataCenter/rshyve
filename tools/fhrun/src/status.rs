// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The payload status `fhrun-init` reports, and the exit code it
//! becomes.
//!
//! Init writes one marker line to COM2 after it reaps the payload, and
//! firehyve mirrors COM2 to its stdout. The launcher pumps that stdout
//! through [`pump`], which copies every byte to fhrun's own stdout and
//! keeps the last marker it sees. The last one wins because init
//! writes after the payload is gone. A payload that prints a marker of
//! its own only spoofs its own exit status, which it can set anyway.
//!
//! The text is fixed by `PayloadStatus::marker` in
//! `tools/fhrun-init/src/main.rs`. Change both together.

use std::io::{self, Read, Write};

/// The marker prefix, shared with `fhrun-init`.
const MARKER_PREFIX: &str = "fhrun-init: payload ";

/// A line longer than this keeps only its tail, which is where a
/// marker written after unterminated payload output lands.
const LINE_TAIL: usize = 256;
const LINE_CAP: usize = 4096;

/// How the payload ended, as init reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadStatus {
    Exited(u8),
    Signaled(u8),
}

impl PayloadStatus {
    /// The exit code a shell would give: the code itself, or 128 plus
    /// the signal number.
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Exited(code) => code,
            Self::Signaled(sig) => 128u8.saturating_add(sig),
        }
    }
}

/// Parse one console line. Anything before the prefix is ignored, so a
/// marker that follows unterminated payload output still parses.
pub fn parse_marker(line: &str) -> Option<PayloadStatus> {
    let at = line.rfind(MARKER_PREFIX)?;
    let mut words = line[at + MARKER_PREFIX.len()..].split_whitespace();
    let kind = words.next()?;
    let number: u8 = words.next()?.parse().ok()?;
    match kind {
        "exit" => Some(PayloadStatus::Exited(number)),
        "signal" => Some(PayloadStatus::Signaled(number)),
        _ => None,
    }
}

/// Splits a byte stream into lines and keeps the last marker seen.
#[derive(Default)]
struct LineScanner {
    line: Vec<u8>,
    last: Option<PayloadStatus>,
}

impl LineScanner {
    fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if b == b'\n' {
                if let Some(status) =
                    parse_marker(&String::from_utf8_lossy(&self.line))
                {
                    self.last = Some(status);
                }
                self.line.clear();
                continue;
            }
            if self.line.len() >= LINE_CAP {
                let keep = self.line.len() - LINE_TAIL;
                self.line.drain(..keep);
            }
            self.line.push(b);
        }
    }
}

/// Copy `reader` to `writer` until EOF and return the last marker.
///
/// A `writer` that fails is abandoned, and the copy goes on as a
/// drain: the VMM must not block on a stdout nobody reads, and the
/// marker still has to be found.
pub fn pump<R: Read, W: Write>(
    mut reader: R,
    mut writer: W,
) -> io::Result<Option<PayloadStatus>> {
    let mut scanner = LineScanner::default();
    let mut buf = [0u8; 4096];
    let mut writer_open = true;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if writer_open {
            writer_open = writer
                .write_all(&buf[..n])
                .and_then(|()| writer.flush())
                .is_ok();
        }
        scanner.feed(&buf[..n]);
    }
    Ok(scanner.last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exit_marker_gives_the_payload_code() {
        assert_eq!(
            parse_marker("fhrun-init: payload exit 3"),
            Some(PayloadStatus::Exited(3))
        );
        assert_eq!(PayloadStatus::Exited(3).exit_code(), 3);
    }

    #[test]
    fn a_signal_marker_maps_like_a_shell() {
        assert_eq!(
            parse_marker("fhrun-init: payload signal 9\r"),
            Some(PayloadStatus::Signaled(9))
        );
        assert_eq!(PayloadStatus::Signaled(9).exit_code(), 137);
    }

    #[test]
    fn a_marker_after_unterminated_output_still_parses() {
        assert_eq!(
            parse_marker("no newline herefhrun-init: payload exit 1"),
            Some(PayloadStatus::Exited(1))
        );
    }

    #[test]
    fn other_init_lines_are_not_markers() {
        for line in [
            "fhrun-init: ready",
            "fhrun-init: payload pid 2: /app",
            "fhrun-init: payload exit",
            "fhrun-init: payload exit x",
            "fhrun-init: payload exit 300",
            "fhrun-init: payload halted 1",
        ] {
            assert_eq!(parse_marker(line), None, "{line}");
        }
    }

    #[test]
    fn the_pump_copies_everything_and_keeps_the_last_marker() {
        let input = b"boot\nfhrun-init: payload exit 7\npartial                      fhrun-init: payload signal 2\ntail";
        let mut out = Vec::new();
        let status = pump(&input[..], &mut out).expect("pump");
        assert_eq!(out, input);
        assert_eq!(status, Some(PayloadStatus::Signaled(2)));
    }

    #[test]
    fn no_marker_means_no_status() {
        let mut out = Vec::new();
        let status = pump(&b"boot\nbye\n"[..], &mut out).expect("pump");
        assert_eq!(status, None);
    }

    #[test]
    fn a_long_line_keeps_the_marker_at_its_tail() {
        let mut input = vec![b'x'; LINE_CAP * 3];
        input.extend_from_slice(b"fhrun-init: payload exit 5\n");
        let status = pump(&input[..], io::sink()).expect("pump");
        assert_eq!(status, Some(PayloadStatus::Exited(5)));
    }

    /// A closed stdout must not end the copy: the marker is still
    /// needed and the VMM must not block on its pipe.
    #[test]
    fn a_failed_writer_does_not_stop_the_scan() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let status = pump(&b"a\nfhrun-init: payload exit 4\n"[..], Broken)
            .expect("pump");
        assert_eq!(status, Some(PayloadStatus::Exited(4)));
    }
}
