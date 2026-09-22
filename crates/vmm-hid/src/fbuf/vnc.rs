// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The VNC server that streams the framebuffer over a unix socket.
//!
//! One thread serves the socket: it accepts one client at a time, polls
//! that client for RFB messages and sends a frame when one is asked
//! for. The pixels come from the devmem host view of BAR1.

use std::io::{self, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use vmm_core::unixsock::accept::accept_loop;
use vmm_core::unixsock::write_all_bounded;

use super::rfb::{
    self, ClientMessage, PixelFormat, RectHeader, ENCODING_DESKTOP_SIZE,
    ENCODING_RAW,
};
use super::{
    scale_abs, unpack_height, unpack_width, Framebuffer, BYTES_PER_PIXEL,
    VNC_WRITE_TIMEOUT,
};

/// VNC update interval (~30 fps).
const VNC_FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// How long one poll for a client message waits. The same thread sends
/// frames, so this bounds how late a frame can be.
const CLIENT_POLL: Duration = Duration::from_millis(5);

/// Desktop name sent to VNC clients.
const VNC_DESKTOP_NAME: &str = "bhyve framebuffer";

/// Main VNC server loop running in a dedicated thread.
///
/// One client at a time. The shared accept loop polls the listener, so
/// `shutdown` is seen within its tick and not only when the next client
/// arrives and leaves.
pub(super) fn server_loop(dev: Arc<Framebuffer>, listener: UnixListener) {
    slog::info!(dev.log, "VNC server listening"; "path" => ?dev.vnc_path);

    let log = dev.log.clone();
    accept_loop(listener, &dev.shutdown, &log, "vnc", |stream| {
        slog::info!(dev.log, "VNC client connected");
        if let Err(e) = handle_vnc_client(&dev, stream) {
            if !is_expected_disconnect(&e) {
                slog::warn!(dev.log, "VNC client error"; "error" => %e);
            }
        }
        slog::info!(dev.log, "VNC client disconnected");
    });

    let _ = std::fs::remove_file(&dev.vnc_path);
}

/// Check if an error is an expected client disconnect.
fn is_expected_disconnect(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
    )
}

/// What this client was told, and what it can be told later.
///
/// ServerInit carries the resolution once. A guest mode change after
/// that would otherwise put a rectangle larger than the client's
/// framebuffer on the wire, which most viewers answer by dropping the
/// connection.
struct ClientView {
    width: u16,
    height: u16,
    /// Whether the client listed the DesktopSize pseudo-encoding.
    desktop_size: bool,
}

/// Dispatch a VNC message and report whether it requested a display update.
fn dispatch_client_message(
    dev: &Framebuffer,
    view: &mut ClientView,
    msg: ClientMessage,
) -> bool {
    match msg {
        ClientMessage::FbUpdateRequest { .. } => true,
        ClientMessage::SetPixelFormat(pf) => {
            if pf.bits_per_pixel != 32 {
                slog::warn!(dev.log, "client requested unsupported pixel format";
                    "bpp" => pf.bits_per_pixel);
            }
            false
        }
        ClientMessage::SetEncodings(encs) => {
            if !encs.contains(&ENCODING_RAW) {
                slog::warn!(dev.log, "client encodings do not include Raw, updates may not display correctly");
            }
            view.desktop_size = encs.contains(&ENCODING_DESKTOP_SIZE);
            false
        }
        ClientMessage::KeyEvent { down, key } => {
            dev.input.key_event(down, key);
            false
        }
        ClientMessage::PointerEvent { button_mask, x, y } => {
            // The tablet only clamps values, so scaling must use fbuf's
            // current resolution before the event reaches the input broker.
            let res = dev.resolution.load(Ordering::Acquire);
            let sx = scale_abs(x, unpack_width(res));
            let sy = scale_abs(y, unpack_height(res));
            dev.input.pointer_event(button_mask, sx, sy);
            false
        }
        ClientMessage::CutText(_) => false,
    }
}

/// Handle a single VNC client connection.
fn handle_vnc_client(
    dev: &Arc<Framebuffer>,
    stream: UnixStream,
) -> io::Result<()> {
    // Set a read timeout to detect dead clients.
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    // This bounds a send on most platforms. On illumos it reports success
    // and does nothing, so `VncWriter` polls instead. A failure here does
    // not refuse the client.
    if let Err(e) = stream.set_write_timeout(Some(VNC_WRITE_TIMEOUT)) {
        slog::warn!(dev.log, "VNC write timeout not set"; "error" => %e);
    }

    let read_stream = stream.try_clone()?;
    let mut writer = VncWriter::new(&stream, VNC_WRITE_TIMEOUT);
    let mut reader = BufReader::new(read_stream);
    let mut framebuffer_scratch = Vec::new();

    // --- RFB handshake ---

    rfb::handshake_version(&mut CombinedStream {
        reader: &mut reader,
        writer: &mut writer,
    })?;

    rfb::handshake_security(
        &mut CombinedStream {
            reader: &mut reader,
            writer: &mut writer,
        },
        dev.vnc_password.as_deref(),
    )?;

    let _shared_flag = rfb::read_client_init(&mut reader)?;

    // ServerInit
    let res = dev.resolution.load(Ordering::Acquire);
    let width = unpack_width(res);
    let height = unpack_height(res);
    let pf = PixelFormat::xrgb8888();
    rfb::send_server_init(&mut writer, width, height, &pf, VNC_DESKTOP_NAME)?;
    let mut view = ClientView {
        width,
        height,
        desktop_size: false,
    };

    // One thread serves both halves: it polls for client messages with a
    // short read timeout and sends a frame when one was asked for.
    let mut update_requested = false;
    stream.set_read_timeout(Some(CLIENT_POLL))?;

    while !dev.shutdown.load(Ordering::Relaxed) {
        match rfb::read_client_message(&mut reader) {
            Ok(Some(msg)) => {
                if dispatch_client_message(dev, &mut view, msg) {
                    update_requested = true;
                }
            }
            // EOF: client disconnected
            Ok(None) => break,
            Err(e) if rfb::is_read_timeout(&e) => {}
            Err(e) => return Err(e),
        }

        if std::mem::take(&mut update_requested) {
            match send_framebuffer_update(
                dev,
                &mut writer,
                &mut view,
                &mut framebuffer_scratch,
            ) {
                Ok(()) => {}
                Err(e) if is_expected_disconnect(&e) => break,
                Err(e) => return Err(e),
            }
        }

        thread::sleep(VNC_FRAME_INTERVAL);
    }

    let _ = stream.shutdown(Shutdown::Both);

    Ok(())
}

/// Send a full framebuffer update to the VNC client.
///
/// A guest mode change since ServerInit is announced first when the
/// client offered DesktopSize. A client that did not gets a rectangle
/// no larger than the framebuffer it was told about, because anything
/// larger is a protocol violation that most viewers drop.
fn send_framebuffer_update(
    dev: &Arc<Framebuffer>,
    writer: &mut VncWriter<'_>,
    view: &mut ClientView,
    scratch: &mut Vec<u8>,
) -> io::Result<()> {
    let res = dev.resolution.load(Ordering::Acquire);
    let width = unpack_width(res);
    let height = unpack_height(res);

    if width == 0 || height == 0 {
        return Ok(());
    }

    let resized = (width, height) != (view.width, view.height);
    if resized && view.desktop_size {
        rfb::write_fb_update_header(writer, 1)?;
        writer.write_all(
            &RectHeader {
                x: 0,
                y: 0,
                width,
                height,
                encoding: ENCODING_DESKTOP_SIZE,
            }
            .to_bytes(),
        )?;
        view.width = width;
        view.height = height;
    }

    // The rectangle may not exceed the client's framebuffer, and the
    // pixel data may not exceed the devmem segment.
    let send_w = width.min(view.width);
    let send_h = height.min(view.height);
    let stride = usize::from(width) * BYTES_PER_PIXEL as usize;
    let max_rows = dev.fb.len().checked_div(stride).unwrap_or(0);
    let send_h = send_h.min(u16::try_from(max_rows).unwrap_or(u16::MAX));
    if send_w == 0 || send_h == 0 {
        return Ok(());
    }

    // Rows are `stride` bytes apart in the segment, so a narrower
    // rectangle is copied row by row.
    let row_bytes = usize::from(send_w) * BYTES_PER_PIXEL as usize;
    scratch.resize(row_bytes * usize::from(send_h), 0);
    let view_bytes = dev.fb.view();
    for row in 0..usize::from(send_h) {
        let src =
            view_bytes
                .subregion(row * stride, row_bytes)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "framebuffer update exceeds devmem bounds",
                    )
                })?;
        src.copy_out(&mut scratch[row * row_bytes..][..row_bytes])?;
    }

    rfb::write_fb_update_header(writer, 1)?;
    let rect = RectHeader {
        x: 0,
        y: 0,
        width: send_w,
        height: send_h,
        encoding: ENCODING_RAW,
    };
    writer.write_all(&rect.to_bytes())?;
    writer.write_all_direct(scratch)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Client socket writer
// ---------------------------------------------------------------------------

/// Buffered writer for a VNC client socket, with a bound on how long
/// one send waits.
///
/// `BufWriter` cannot bound a send. On illumos a write to a full AF_UNIX
/// socket blocks forever: `SO_SNDTIMEO` reports success and bounds
/// nothing, and `O_NONBLOCK` is a file-status flag that the reader clone
/// of this socket shares. `write_all_bounded` polls for space instead.
///
/// Only protocol headers go through the buffer. Pixel data is far too
/// large to copy again, so it goes to the socket from the caller's own
/// storage through [`VncWriter::write_all_direct`].
struct VncWriter<'a> {
    stream: &'a UnixStream,
    buf: Vec<u8>,
    budget: Duration,
}

impl<'a> VncWriter<'a> {
    fn new(stream: &'a UnixStream, budget: Duration) -> Self {
        Self {
            stream,
            buf: Vec::new(),
            budget,
        }
    }

    /// Send the buffer, then `data`, without copying `data` first.
    fn write_all_direct(&mut self, data: &[u8]) -> io::Result<()> {
        self.flush()?;
        write_all_bounded(self.stream, data, self.budget)
    }
}

impl Write for VncWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let result = write_all_bounded(self.stream, &self.buf, self.budget);
        // A send that gave up left part of the buffer on the socket, so
        // the bytes must not be sent again. The caller drops the client.
        self.buf.clear();
        result
    }
}

// ---------------------------------------------------------------------------
// CombinedStream helper for handshake
// ---------------------------------------------------------------------------

/// A reader and a writer joined into one `Read + Write` stream for the
/// handshake functions.
struct CombinedStream<'a, R: Read, W: Write> {
    reader: &'a mut BufReader<R>,
    writer: &'a mut W,
}

impl<R: Read, W: Write> Read for CombinedStream<'_, R, W> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

impl<R: Read, W: Write> Write for CombinedStream<'_, R, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use vmm_devices::{InputBroker, Lifecycle};

    use crate::fbuf::pack_resolution;
    use crate::fbuf::tests::{
        make_test_dev, make_test_dev_at, make_test_dev_with_input,
        RecordingKeyboard,
    };

    #[test]
    fn key_message_reaches_input_broker() {
        let input = Arc::new(InputBroker::default());
        let keyboard = Arc::new(RecordingKeyboard::default());
        input.set_keyboard(keyboard.clone());
        let dev = make_test_dev_with_input(input);

        assert!(!dispatch_client_message(
            &dev,
            &mut announced(1024, 768),
            ClientMessage::KeyEvent {
                down: true,
                key: 0xff0d,
            },
        ));
        assert_eq!(keyboard.events(), vec![(true, 0xff0d)]);
    }

    /// Read what one update put on the wire: the rectangle headers.
    fn sent_rects(
        dev: &Arc<Framebuffer>,
        view: &mut ClientView,
    ) -> Vec<RectHeader> {
        use std::io::Read;

        let (mut peer, server) =
            UnixStream::pair().expect("create a VNC socket pair");
        // A frame is megabytes, far more than the socket buffer, so the
        // peer has to drain while the update is written.
        let drain = thread::spawn(move || {
            let mut wire = Vec::new();
            peer.read_to_end(&mut wire).expect("read the update");
            wire
        });

        let mut writer = VncWriter::new(&server, Duration::from_secs(5));
        let mut scratch = Vec::new();
        send_framebuffer_update(dev, &mut writer, view, &mut scratch)
            .expect("the peer reads everything");
        writer.flush().expect("flush the update");
        drop(server);
        let wire = drain.join().expect("the draining thread");
        let mut rects = Vec::new();
        let mut at = 0;
        while at + 4 <= wire.len() {
            let count = u16::from_be_bytes([wire[at + 2], wire[at + 3]]);
            at += 4;
            for _ in 0..count {
                let r = &wire[at..at + 12];
                let rect = RectHeader {
                    x: u16::from_be_bytes([r[0], r[1]]),
                    y: u16::from_be_bytes([r[2], r[3]]),
                    width: u16::from_be_bytes([r[4], r[5]]),
                    height: u16::from_be_bytes([r[6], r[7]]),
                    encoding: i32::from_be_bytes([r[8], r[9], r[10], r[11]]),
                };
                at += 12;
                if rect.encoding == ENCODING_RAW {
                    at += usize::from(rect.width)
                        * usize::from(rect.height)
                        * BYTES_PER_PIXEL as usize;
                }
                rects.push(rect);
            }
        }
        rects
    }

    #[test]
    fn a_client_without_desktop_size_never_gets_a_bigger_rectangle() {
        // ServerInit carried 1024x768. A rectangle larger than that is a
        // protocol violation and most viewers drop the connection.
        let dev = make_test_dev();
        dev.resolution
            .store(pack_resolution(1920, 1080), Ordering::Release);

        let mut view = announced(1024, 768);
        let rects = sent_rects(&dev, &mut view);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].encoding, ENCODING_RAW);
        assert_eq!((rects[0].width, rects[0].height), (1024, 768));
        assert_eq!((view.width, view.height), (1024, 768));
    }

    #[test]
    fn a_client_with_desktop_size_is_told_the_new_resolution() {
        let dev = make_test_dev();
        dev.resolution
            .store(pack_resolution(800, 600), Ordering::Release);

        let mut view = ClientView {
            width: 1024,
            height: 768,
            desktop_size: true,
        };
        let rects = sent_rects(&dev, &mut view);
        assert_eq!(rects.len(), 2, "{rects:?}");
        assert_eq!(rects[0].encoding, ENCODING_DESKTOP_SIZE);
        assert_eq!((rects[0].width, rects[0].height), (800, 600));
        assert_eq!(rects[1].encoding, ENCODING_RAW);
        assert_eq!((rects[1].width, rects[1].height), (800, 600));
        assert_eq!((view.width, view.height), (800, 600));

        // The next update is a plain rectangle: the size is now known.
        let rects = sent_rects(&dev, &mut view);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].encoding, ENCODING_RAW);
    }

    #[test]
    fn set_encodings_records_whether_the_client_takes_desktop_size() {
        let dev = make_test_dev();
        let mut view = announced(1024, 768);
        dispatch_client_message(
            &dev,
            &mut view,
            ClientMessage::SetEncodings(vec![
                ENCODING_RAW,
                ENCODING_DESKTOP_SIZE,
            ]),
        );
        assert!(view.desktop_size);
        dispatch_client_message(
            &dev,
            &mut view,
            ClientMessage::SetEncodings(vec![ENCODING_RAW]),
        );
        assert!(!view.desktop_size);
    }

    /// Halt has to end the VNC thread, or it keeps streaming devmem to a
    /// connected client until the process exits.
    #[test]
    fn halt_stops_the_vnc_thread() {
        use std::time::Instant;

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("vnc.sock");
        let listener = vmm_core::unixsock::bind_restricted(
            &path,
            vmm_core::unixsock::SocketPolicy::default(),
        )
        .expect("bind the VNC socket");

        let dev =
            make_test_dev_at(Arc::new(InputBroker::default()), path.clone());
        let server = Arc::clone(&dev);
        let handle = thread::Builder::new()
            .spawn(move || server_loop(server, listener))
            .expect("spawn the VNC server");
        *dev.vnc_thread.lock().expect("fbuf: vnc thread lock") = Some(handle);

        let started = Instant::now();
        Lifecycle::halt(dev.as_ref());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "halt waited {:?}",
            started.elapsed()
        );
        assert!(dev.shutdown.load(Ordering::Acquire));
        assert!(dev
            .vnc_thread
            .lock()
            .expect("fbuf: vnc thread lock")
            .is_none());
        assert!(!path.exists(), "the socket file is unlinked");
    }

    /// A VNC client that stops reading must not hold the VNC thread.
    ///
    /// The thread also runs the accept loop, so a stuck write costs
    /// every later viewer as well.
    #[test]
    fn a_client_that_never_reads_does_not_hold_the_vnc_thread() {
        use std::os::unix::net::UnixStream;
        use std::time::Instant;

        let dev = make_test_dev();
        let (_peer, server) =
            UnixStream::pair().expect("create a VNC socket pair");
        let mut writer = VncWriter::new(&server, Duration::from_millis(200));
        let mut scratch = Vec::new();

        let started = Instant::now();
        let error = send_framebuffer_update(
            &dev,
            &mut writer,
            &mut announced(1024, 768),
            &mut scratch,
        )
        .expect_err("a client that reads nothing must not be waited on");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the update took {:?}",
            started.elapsed()
        );
    }

    /// A client that was told `width` x `height` and offered no
    /// DesktopSize.
    fn announced(width: u16, height: u16) -> ClientView {
        ClientView {
            width,
            height,
            desktop_size: false,
        }
    }
}
