// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! RFB (Remote Framebuffer) protocol codec for VNC.
//!
//! The subset of RFB 3.8 (RFC 6143) that the VNC server uses: version
//! handshake, security negotiation, server init, framebuffer updates with
//! Raw encoding, and client message parsing.

use std::io::{self, BufRead, Read, Write};
use std::time::{Duration, Instant};

use des::cipher::{BlockEncrypt, KeyInit};
use des::Des;

// ---------------------------------------------------------------------------
// Protocol version
// ---------------------------------------------------------------------------

/// RFB protocol version string sent by the server.
pub const RFB_VERSION: &[u8; 12] = b"RFB 003.008\n";

// ---------------------------------------------------------------------------
// Security types
// ---------------------------------------------------------------------------

/// No authentication required.
pub const SECURITY_NONE: u8 = 1;

/// VNC password authentication (DES challenge-response).
pub const SECURITY_VNC_AUTH: u8 = 2;

/// SecurityResult: OK.
pub const SECURITY_RESULT_OK: u32 = 0;

/// SecurityResult: Failed.
pub const SECURITY_RESULT_FAILED: u32 = 1;

// ---------------------------------------------------------------------------
// Server-to-client message types
// ---------------------------------------------------------------------------

pub const SERVER_FRAMEBUFFER_UPDATE: u8 = 0;
pub const SERVER_SET_COLOUR_MAP: u8 = 1;
pub const SERVER_BELL: u8 = 2;
pub const SERVER_CUT_TEXT: u8 = 3;

// ---------------------------------------------------------------------------
// Client-to-server message types
// ---------------------------------------------------------------------------

pub const CLIENT_SET_PIXEL_FORMAT: u8 = 0;
pub const CLIENT_SET_ENCODINGS: u8 = 2;
pub const CLIENT_FB_UPDATE_REQUEST: u8 = 3;
pub const CLIENT_KEY_EVENT: u8 = 4;
pub const CLIENT_POINTER_EVENT: u8 = 5;
pub const CLIENT_CUT_TEXT: u8 = 6;

// ---------------------------------------------------------------------------
// Encoding types
// ---------------------------------------------------------------------------

pub const ENCODING_RAW: i32 = 0;

/// DesktopSize pseudo-encoding: a client that lists it accepts a new
/// framebuffer size in a rectangle header.
pub const ENCODING_DESKTOP_SIZE: i32 = -223;

/// How long a message body may take once its type byte is consumed.
///
/// The caller polls for the type byte with a short timeout. After that
/// the framing is committed, so a timeout mid-body cannot be reported
/// as "no data": the remainder has to arrive or the client is dead.
const BODY_DEADLINE: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// PixelFormat
// ---------------------------------------------------------------------------

/// RFB pixel format descriptor (16 bytes on the wire).
///
/// The server uses XRGB8888: 32 bpp, depth 24, little-endian, true-color,
/// with R at bits 16-23, G at bits 8-15, B at bits 0-7.
#[derive(Debug, Clone, Copy)]
pub struct PixelFormat {
    pub bits_per_pixel: u8,
    pub depth: u8,
    pub big_endian: u8,
    pub true_color: u8,
    pub red_max: u16,
    pub green_max: u16,
    pub blue_max: u16,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
}

impl PixelFormat {
    /// Default pixel format: XRGB8888, little-endian.
    pub fn xrgb8888() -> Self {
        Self {
            bits_per_pixel: 32,
            depth: 24,
            big_endian: 0,
            true_color: 1,
            red_max: 255,
            green_max: 255,
            blue_max: 255,
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
        }
    }

    /// Serialize the pixel format to 16 bytes (wire format).
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut buf = [0u8; 16];
        buf[0] = self.bits_per_pixel;
        buf[1] = self.depth;
        buf[2] = self.big_endian;
        buf[3] = self.true_color;
        buf[4..6].copy_from_slice(&self.red_max.to_be_bytes());
        buf[6..8].copy_from_slice(&self.green_max.to_be_bytes());
        buf[8..10].copy_from_slice(&self.blue_max.to_be_bytes());
        buf[10] = self.red_shift;
        buf[11] = self.green_shift;
        buf[12] = self.blue_shift;
        // buf[13..16] padding
        buf
    }

    /// Parse a pixel format from 16 bytes (wire format).
    pub fn from_bytes(buf: &[u8; 16]) -> Self {
        Self {
            bits_per_pixel: buf[0],
            depth: buf[1],
            big_endian: buf[2],
            true_color: buf[3],
            red_max: u16::from_be_bytes([buf[4], buf[5]]),
            green_max: u16::from_be_bytes([buf[6], buf[7]]),
            blue_max: u16::from_be_bytes([buf[8], buf[9]]),
            red_shift: buf[10],
            green_shift: buf[11],
            blue_shift: buf[12],
        }
    }
}

// ---------------------------------------------------------------------------
// Handshake helpers
// ---------------------------------------------------------------------------

/// Perform the RFB version handshake (server side).
///
/// Sends the server version string, reads the client version string,
/// and returns the client's minor version number.
pub fn handshake_version<S: Read + Write>(stream: &mut S) -> io::Result<u16> {
    stream.write_all(RFB_VERSION)?;
    stream.flush()?;

    // Read client version (exactly 12 bytes: "RFB 003.XXX\n")
    let mut client_ver = [0u8; 12];
    stream.read_exact(&mut client_ver)?;

    // The server accepts any 3.x client but always speaks 3.8.
    let minor_str = std::str::from_utf8(&client_ver[8..11]).unwrap_or("008");
    let minor = minor_str.parse::<u16>().unwrap_or(8);

    Ok(minor)
}

/// Perform the security handshake (server side).
///
/// Sends the security type list and reads the client's selection.
/// If `password` is `Some`, offers VncAuth; otherwise offers None.
/// Returns the selected security type.
pub fn handshake_security<S: Read + Write>(
    stream: &mut S,
    password: Option<&str>,
) -> io::Result<u8> {
    if let Some(_pw) = password {
        // Only offer VncAuth when password is configured
        stream.write_all(&[1, SECURITY_VNC_AUTH])?;
    } else {
        // Offer only None
        stream.write_all(&[1, SECURITY_NONE])?;
    }
    stream.flush()?;

    // Read client's selected security type (1 byte)
    let mut sel = [0u8; 1];
    stream.read_exact(&mut sel)?;
    let selected = sel[0];

    match selected {
        SECURITY_NONE if password.is_none() => {
            // SecurityResult: OK (only when no password configured)
            stream.write_all(&SECURITY_RESULT_OK.to_be_bytes())?;
            stream.flush()?;
            Ok(SECURITY_NONE)
        }
        SECURITY_NONE => {
            // Client tried to bypass VNC auth
            stream.write_all(&SECURITY_RESULT_FAILED.to_be_bytes())?;
            stream.flush()?;
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "authentication required",
            ))
        }
        SECURITY_VNC_AUTH => {
            if let Some(pw) = password {
                vnc_auth_challenge(stream, pw)?;
                Ok(SECURITY_VNC_AUTH)
            } else {
                // The client selected VncAuth, which was not offered.
                stream.write_all(&SECURITY_RESULT_FAILED.to_be_bytes())?;
                stream.flush()?;
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "vnc auth not available",
                ))
            }
        }
        _ => {
            stream.write_all(&SECURITY_RESULT_FAILED.to_be_bytes())?;
            stream.flush()?;
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported security type: {}", selected),
            ))
        }
    }
}

/// VNC DES challenge-response authentication.
///
/// Sends a 16-byte challenge, reads the 16-byte response, and verifies
/// it against the password using DES encryption.
fn vnc_auth_challenge<S: Read + Write>(
    stream: &mut S,
    password: &str,
) -> io::Result<()> {
    // Generate a random 16-byte challenge from the OS CSPRNG.
    let mut challenge = [0u8; 16];
    getrandom::fill(&mut challenge).map_err(io::Error::other)?;

    stream.write_all(&challenge)?;
    stream.flush()?;

    let mut response = [0u8; 16];
    stream.read_exact(&mut response)?;

    let expected = vnc_des_encrypt(password, &challenge);

    if response == expected {
        stream.write_all(&SECURITY_RESULT_OK.to_be_bytes())?;
        stream.flush()?;
        Ok(())
    } else {
        stream.write_all(&SECURITY_RESULT_FAILED.to_be_bytes())?;
        stream.flush()?;
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "vnc auth failed",
        ))
    }
}

/// VNC DES encryption: encrypt the challenge with the password.
///
/// Per the RFB protocol, the password is truncated/padded to 8 bytes,
/// each byte is bit-reversed (VNC convention), and the result is used
/// as a DES-ECB key to encrypt each 8-byte half of the 16-byte challenge.
fn vnc_des_encrypt(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    // Prepare 8-byte key from password (padded with zeros / truncated)
    let mut key = [0u8; 8];
    for (i, b) in password.bytes().take(8).enumerate() {
        // VNC reverses the bits in each key byte
        key[i] = reverse_bits(b);
    }

    let cipher = Des::new_from_slice(&key).expect("DES key is always 8 bytes");
    let mut result = [0u8; 16];
    result.copy_from_slice(challenge);

    // Encrypt each 8-byte half separately (RFB protocol)
    let (first, second) = result.split_at_mut(8);
    cipher.encrypt_block(first.into());
    cipher.encrypt_block(second.into());

    result
}

/// Reverse the bits in a byte (VNC DES key mangling).
fn reverse_bits(mut b: u8) -> u8 {
    let mut result = 0u8;
    for _ in 0..8 {
        result = (result << 1) | (b & 1);
        b >>= 1;
    }
    result
}

/// Send the ServerInit message.
///
/// Contains framebuffer dimensions, pixel format, and desktop name.
pub fn send_server_init<W: Write>(
    w: &mut W,
    width: u16,
    height: u16,
    pf: &PixelFormat,
    name: &str,
) -> io::Result<()> {
    // Width (2) + Height (2) + PixelFormat (16) + name-length (4) + name
    w.write_all(&width.to_be_bytes())?;
    w.write_all(&height.to_be_bytes())?;
    w.write_all(&pf.to_bytes())?;
    let name_bytes = name.as_bytes();
    w.write_all(&(name_bytes.len() as u32).to_be_bytes())?;
    w.write_all(name_bytes)?;
    w.flush()
}

/// Read the ClientInit message (1 byte: shared-flag).
pub fn read_client_init<R: Read>(r: &mut R) -> io::Result<u8> {
    let mut buf = [0u8; 1];
    r.read_exact(&mut buf)?;
    Ok(buf[0])
}

// ---------------------------------------------------------------------------
// Server-to-client message writers
// ---------------------------------------------------------------------------

/// Header for a FramebufferUpdate rectangle.
#[derive(Debug, Clone, Copy)]
pub struct RectHeader {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub encoding: i32,
}

impl RectHeader {
    /// Serialize to 12 bytes (wire format).
    pub fn to_bytes(&self) -> [u8; 12] {
        let mut buf = [0u8; 12];
        buf[0..2].copy_from_slice(&self.x.to_be_bytes());
        buf[2..4].copy_from_slice(&self.y.to_be_bytes());
        buf[4..6].copy_from_slice(&self.width.to_be_bytes());
        buf[6..8].copy_from_slice(&self.height.to_be_bytes());
        buf[8..12].copy_from_slice(&self.encoding.to_be_bytes());
        buf
    }
}

/// Write a FramebufferUpdate message header.
///
/// After this, the caller writes `num_rects` rectangle headers + pixel data.
pub fn write_fb_update_header<W: Write>(
    w: &mut W,
    num_rects: u16,
) -> io::Result<()> {
    // message-type (1) + padding (1) + number-of-rectangles (2)
    let mut hdr = [0u8; 4];
    hdr[0] = SERVER_FRAMEBUFFER_UPDATE;
    // hdr[1] = padding
    hdr[2..4].copy_from_slice(&num_rects.to_be_bytes());
    w.write_all(&hdr)
}

// ---------------------------------------------------------------------------
// Client-to-server message readers
// ---------------------------------------------------------------------------

/// A parsed client message.
#[derive(Debug)]
pub enum ClientMessage {
    /// SetPixelFormat: the client wants a different pixel format.
    SetPixelFormat(PixelFormat),
    /// SetEncodings: list of encoding types the client supports.
    SetEncodings(Vec<i32>),
    /// FramebufferUpdateRequest.
    FbUpdateRequest {
        incremental: bool,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    /// KeyEvent.
    KeyEvent { down: bool, key: u32 },
    /// PointerEvent.
    PointerEvent { button_mask: u8, x: u16, y: u16 },
    /// ClientCutText.
    CutText(String),
}

/// Whether an error is the socket's read timeout rather than a fault.
pub fn is_read_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// Fill `buf` completely, riding out the socket's read timeout.
///
/// `read_exact` leaves an unspecified amount consumed when it fails, so
/// it cannot be retried. This keeps its own count and only gives up
/// once `deadline` has passed with no progress at all.
fn read_body<R: Read>(
    r: &mut R,
    buf: &mut [u8],
    deadline: Instant,
) -> io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "client closed mid-message",
                ))
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if is_read_timeout(&e) => {
                if Instant::now() >= deadline {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Read and parse a single client message.
///
/// Returns `None` on EOF (client disconnected).
pub fn read_client_message<R: BufRead>(
    r: &mut R,
) -> io::Result<Option<ClientMessage>> {
    let mut msg_type = [0u8; 1];
    match r.read_exact(&mut msg_type) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    // The type byte is gone from the stream, so every read below has to
    // finish: a caller that treated a mid-body timeout as "no data"
    // would parse the next body byte as a message type.
    let deadline = Instant::now() + BODY_DEADLINE;

    match msg_type[0] {
        CLIENT_SET_PIXEL_FORMAT => {
            // 3 bytes padding + 16 bytes pixel format
            let mut buf = [0u8; 19];
            read_body(r, &mut buf, deadline)?;
            let pf_bytes: [u8; 16] = buf[3..19].try_into().expect("16 bytes");
            Ok(Some(ClientMessage::SetPixelFormat(
                PixelFormat::from_bytes(&pf_bytes),
            )))
        }
        CLIENT_SET_ENCODINGS => {
            // 1 byte padding + 2 bytes count
            let mut hdr = [0u8; 3];
            read_body(r, &mut hdr, deadline)?;
            let count = u16::from_be_bytes([hdr[1], hdr[2]]) as usize;
            // Cap at 64 encodings to prevent resource exhaustion from
            // a malicious client sending count=65535.
            let capped = count.min(64);
            let mut encodings = Vec::with_capacity(capped);
            for i in 0..count {
                let mut enc = [0u8; 4];
                read_body(r, &mut enc, deadline)?;
                if i < capped {
                    encodings.push(i32::from_be_bytes(enc));
                }
                // Drain remaining bytes to keep the stream in sync
            }
            Ok(Some(ClientMessage::SetEncodings(encodings)))
        }
        CLIENT_FB_UPDATE_REQUEST => {
            // 1 byte incremental + 2*4 bytes x,y,w,h
            let mut buf = [0u8; 9];
            read_body(r, &mut buf, deadline)?;
            Ok(Some(ClientMessage::FbUpdateRequest {
                incremental: buf[0] != 0,
                x: u16::from_be_bytes([buf[1], buf[2]]),
                y: u16::from_be_bytes([buf[3], buf[4]]),
                width: u16::from_be_bytes([buf[5], buf[6]]),
                height: u16::from_be_bytes([buf[7], buf[8]]),
            }))
        }
        CLIENT_KEY_EVENT => {
            // 1 byte down-flag + 2 bytes padding + 4 bytes key
            let mut buf = [0u8; 7];
            read_body(r, &mut buf, deadline)?;
            Ok(Some(ClientMessage::KeyEvent {
                down: buf[0] != 0,
                key: u32::from_be_bytes([buf[3], buf[4], buf[5], buf[6]]),
            }))
        }
        CLIENT_POINTER_EVENT => {
            // 1 byte button-mask + 2 bytes x + 2 bytes y
            let mut buf = [0u8; 5];
            read_body(r, &mut buf, deadline)?;
            Ok(Some(ClientMessage::PointerEvent {
                button_mask: buf[0],
                x: u16::from_be_bytes([buf[1], buf[2]]),
                y: u16::from_be_bytes([buf[3], buf[4]]),
            }))
        }
        CLIENT_CUT_TEXT => {
            // 3 bytes padding + 4 bytes length + text
            let mut hdr = [0u8; 7];
            read_body(r, &mut hdr, deadline)?;
            let len =
                u32::from_be_bytes([hdr[3], hdr[4], hdr[5], hdr[6]]) as usize;
            // Cap at 1 MiB to avoid OOM
            let capped = len.min(1024 * 1024);
            let mut text_buf = vec![0u8; capped];
            read_body(r, &mut text_buf, deadline)?;
            // Drain the bytes past the cap to keep the stream in sync.
            if len > capped {
                let mut drain = vec![0u8; 4096];
                let mut remaining = len - capped;
                while remaining > 0 {
                    let chunk = remaining.min(drain.len());
                    read_body(r, &mut drain[..chunk], deadline)?;
                    remaining -= chunk;
                }
            }
            let text = String::from_utf8_lossy(&text_buf).into_owned();
            Ok(Some(ClientMessage::CutText(text)))
        }
        other => {
            // The length of an unknown message is not known, so the rest
            // of the stream cannot be parsed.
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown RFB client message type: {}", other),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    #[test]
    fn pixel_format_roundtrip() {
        let pf = PixelFormat::xrgb8888();
        let bytes = pf.to_bytes();
        let pf2 = PixelFormat::from_bytes(&bytes);
        assert_eq!(pf2.bits_per_pixel, 32);
        assert_eq!(pf2.depth, 24);
        assert_eq!(pf2.big_endian, 0);
        assert_eq!(pf2.true_color, 1);
        assert_eq!(pf2.red_max, 255);
        assert_eq!(pf2.green_max, 255);
        assert_eq!(pf2.blue_max, 255);
        assert_eq!(pf2.red_shift, 16);
        assert_eq!(pf2.green_shift, 8);
        assert_eq!(pf2.blue_shift, 0);
    }

    #[test]
    fn reverse_bits_smoke() {
        assert_eq!(reverse_bits(0b1000_0000), 0b0000_0001);
        assert_eq!(reverse_bits(0b1010_0101), 0b1010_0101);
        assert_eq!(reverse_bits(0xFF), 0xFF);
        assert_eq!(reverse_bits(0x00), 0x00);
    }

    #[test]
    fn rect_header_to_bytes() {
        let hdr = RectHeader {
            x: 0,
            y: 0,
            width: 1024,
            height: 768,
            encoding: ENCODING_RAW,
        };
        let bytes = hdr.to_bytes();
        assert_eq!(bytes.len(), 12);
        assert_eq!(u16::from_be_bytes([bytes[4], bytes[5]]), 1024);
        assert_eq!(u16::from_be_bytes([bytes[6], bytes[7]]), 768);
        assert_eq!(
            i32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            0
        );
    }

    #[test]
    fn version_handshake() {
        let client_data = b"RFB 003.008\n";
        let mut stream = Cursor::new(Vec::new());
        stream.get_mut().extend_from_slice(client_data);
        stream.set_position(0);

        let mut server_out = Vec::new();
        server_out.extend_from_slice(RFB_VERSION);

        let minor_str = std::str::from_utf8(&client_data[8..11]).expect("utf8");
        let minor = minor_str.parse::<u16>().expect("parse minor");
        assert_eq!(minor, 8);
    }

    #[test]
    fn parse_fb_update_request() {
        // A FramebufferUpdateRequest message, type byte first.
        let mut data = Vec::new();
        data.push(CLIENT_FB_UPDATE_REQUEST); // msg type
        data.push(1); // incremental
        data.extend_from_slice(&0u16.to_be_bytes()); // x
        data.extend_from_slice(&0u16.to_be_bytes()); // y
        data.extend_from_slice(&1024u16.to_be_bytes()); // width
        data.extend_from_slice(&768u16.to_be_bytes()); // height

        let mut reader = BufReader::new(Cursor::new(data));
        let msg = read_client_message(&mut reader)
            .expect("read")
            .expect("some");
        match msg {
            ClientMessage::FbUpdateRequest {
                incremental,
                x,
                y,
                width,
                height,
            } => {
                assert!(incremental);
                assert_eq!(x, 0);
                assert_eq!(y, 0);
                assert_eq!(width, 1024);
                assert_eq!(height, 768);
            }
            _ => panic!("expected FbUpdateRequest"),
        }
    }

    #[test]
    fn parse_key_event() {
        let mut data = Vec::new();
        data.push(CLIENT_KEY_EVENT);
        data.push(1); // down
        data.extend_from_slice(&[0, 0]); // padding
        data.extend_from_slice(&0x0041u32.to_be_bytes()); // key 'A'

        let mut reader = BufReader::new(Cursor::new(data));
        let msg = read_client_message(&mut reader)
            .expect("read")
            .expect("some");
        match msg {
            ClientMessage::KeyEvent { down, key } => {
                assert!(down);
                assert_eq!(key, 0x41);
            }
            _ => panic!("expected KeyEvent"),
        }
    }

    #[test]
    fn parse_pointer_event() {
        let mut data = Vec::new();
        data.push(CLIENT_POINTER_EVENT);
        data.push(0x01); // button mask (left button)
        data.extend_from_slice(&100u16.to_be_bytes()); // x
        data.extend_from_slice(&200u16.to_be_bytes()); // y

        let mut reader = BufReader::new(Cursor::new(data));
        let msg = read_client_message(&mut reader)
            .expect("read")
            .expect("some");
        match msg {
            ClientMessage::PointerEvent { button_mask, x, y } => {
                assert_eq!(button_mask, 0x01);
                assert_eq!(x, 100);
                assert_eq!(y, 200);
            }
            _ => panic!("expected PointerEvent"),
        }
    }

    #[test]
    fn eof_returns_none() {
        let data: Vec<u8> = vec![];
        let mut reader = BufReader::new(Cursor::new(data));
        let result = read_client_message(&mut reader).expect("no error");
        assert!(result.is_none());
    }

    #[test]
    fn unknown_msg_type_is_error() {
        let data = vec![0xFF_u8]; // unknown type
        let mut reader = BufReader::new(Cursor::new(data));
        let result = read_client_message(&mut reader);
        assert!(result.is_err());
    }

    #[test]
    fn vnc_des_encrypt_known_vector() {
        // The VNC key for "password" with bit-reversed bytes:
        //   'p'=0x70 -> 0x0E, 'a'=0x61 -> 0x86, 's'=0x73 -> 0xCE,
        //   's'=0x73 -> 0xCE, 'w'=0x77 -> 0xEE, 'o'=0x6F -> 0xF6,
        //   'r'=0x72 -> 0x4E, 'd'=0x64 -> 0x26
        //
        // The result must be real DES, not XOR, and DES decryption must
        // give back the original challenge.
        let challenge: [u8; 16] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B,
            0x0C, 0x0D, 0x0E, 0x0F, 0x10,
        ];
        let result = vnc_des_encrypt("password", &challenge);

        // The result must differ from a plain XOR with the key.
        let mut xor_result = [0u8; 16];
        let key_bytes = b"password";
        for i in 0..16 {
            let kb = if i % 8 < key_bytes.len() {
                reverse_bits(key_bytes[i % 8])
            } else {
                0
            };
            xor_result[i] = challenge[i] ^ kb;
        }
        assert_ne!(
            result, xor_result,
            "DES output must differ from trivial XOR"
        );

        // Verify DES decryption recovers the challenge (round-trip)
        use des::cipher::BlockDecrypt;
        let mut key = [0u8; 8];
        for (i, b) in b"password".iter().take(8).enumerate() {
            key[i] = reverse_bits(*b);
        }
        let cipher = Des::new_from_slice(&key).unwrap();
        let mut decrypted = result;
        let (first, second) = decrypted.split_at_mut(8);
        cipher.decrypt_block(first.into());
        cipher.decrypt_block(second.into());
        assert_eq!(
            decrypted, challenge,
            "DES round-trip must recover challenge"
        );
    }

    #[test]
    fn vnc_des_encrypt_short_password() {
        // Password shorter than 8 bytes: remaining key bytes are zero
        let challenge: [u8; 16] = [0xAA; 16];
        let result = vnc_des_encrypt("abc", &challenge);
        // Must not panic and must produce a valid 16-byte result
        assert_eq!(result.len(), 16);
        // Must differ from the input challenge (DES changes it)
        assert_ne!(result, challenge);
    }

    #[test]
    fn vnc_des_encrypt_empty_password() {
        // Empty password: all key bytes are zero
        let challenge: [u8; 16] = [0x55; 16];
        let result = vnc_des_encrypt("", &challenge);
        assert_eq!(result.len(), 16);
        // Even with an all-zero key, DES transforms the input
        assert_ne!(result, challenge);
    }

    /// A socket that hands out its bytes in the pieces a script names,
    /// with a read timeout wherever the script says so.
    enum Step {
        Data(Vec<u8>),
        Timeout,
    }

    struct Stuttering(std::collections::VecDeque<Step>);

    impl Read for Stuttering {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.0.pop_front() {
                None => Ok(0),
                Some(Step::Timeout) => {
                    Err(io::Error::from(io::ErrorKind::WouldBlock))
                }
                Some(Step::Data(data)) => {
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    if n < data.len() {
                        self.0.push_front(Step::Data(data[n..].to_vec()));
                    }
                    Ok(n)
                }
            }
        }
    }

    #[test]
    fn a_read_timeout_mid_message_does_not_lose_the_framing() {
        // The type byte is consumed before the body arrives. Reporting
        // the timeout to the caller there would make the next call read
        // a body byte as a message type and drop the client.
        use std::io::BufReader;

        let pointer = |mask: u8, x: u16, y: u16| {
            let mut m = vec![CLIENT_POINTER_EVENT, mask];
            m.extend_from_slice(&x.to_be_bytes());
            m.extend_from_slice(&y.to_be_bytes());
            m
        };
        let first = pointer(1, 0x0102, 0x0304);
        let second = pointer(0, 0x0506, 0x0708);

        let mut reader = BufReader::new(Stuttering(
            [
                Step::Data(first[..1].to_vec()),
                Step::Timeout,
                Step::Data(first[1..3].to_vec()),
                Step::Timeout,
                Step::Data(first[3..].to_vec()),
                Step::Data(second.clone()),
            ]
            .into(),
        ));

        assert!(matches!(
            read_client_message(&mut reader).expect("first message"),
            Some(ClientMessage::PointerEvent {
                button_mask: 1,
                x: 0x0102,
                y: 0x0304
            })
        ));
        // The stream is still framed, so the next message parses.
        assert!(matches!(
            read_client_message(&mut reader).expect("second message"),
            Some(ClientMessage::PointerEvent {
                button_mask: 0,
                x: 0x0506,
                y: 0x0708
            })
        ));
        assert!(read_client_message(&mut reader)
            .expect("clean EOF")
            .is_none());
    }

    #[test]
    fn a_client_that_stops_mid_message_is_an_error_not_a_message() {
        use std::io::BufReader;

        let mut reader = BufReader::new(Stuttering(
            [Step::Data(vec![CLIENT_POINTER_EVENT, 1])].into(),
        ));
        let err = read_client_message(&mut reader)
            .expect_err("a half message is not a message");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
