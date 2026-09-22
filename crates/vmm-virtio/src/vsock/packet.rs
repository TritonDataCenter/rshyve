// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The virtio-vsock packet header (virtio 1.3 §5.10.6).
//!
//! Encoded field by field, not by a struct cast. The wire layout is 44
//! bytes with no padding, and a `repr(C)` struct with the same fields
//! aligns its `u64`s to 8 and occupies 48. The manual encode also keeps
//! the little-endian conversion explicit.

/// Wire size of `struct virtio_vsock_hdr`.
pub const HDR_SIZE: usize = 44;

// Context IDs. Anything below the first guest id is reserved.
/// The hypervisor itself.
pub const CID_HYPERVISOR: u64 = 0;
/// Loopback within one endpoint.
pub const CID_LOCAL: u64 = 1;
/// The host side of this device.
pub const CID_HOST: u64 = 2;
/// Lowest context id a guest may be given.
pub const CID_GUEST_MIN: u64 = 3;

/// Connection type. Only streams are implemented.
pub const TYPE_STREAM: u16 = 1;

// Operations (virtio 1.3 §5.10.6.1).
pub const OP_INVALID: u16 = 0;
pub const OP_REQUEST: u16 = 1;
pub const OP_RESPONSE: u16 = 2;
pub const OP_RST: u16 = 3;
pub const OP_SHUTDOWN: u16 = 4;
pub const OP_RW: u16 = 5;
pub const OP_CREDIT_UPDATE: u16 = 6;
pub const OP_CREDIT_REQUEST: u16 = 7;

// Flags carried by OP_SHUTDOWN.
/// The peer will read no more.
pub const SHUTDOWN_RCV: u32 = 1;
/// The peer will send no more.
pub const SHUTDOWN_SEND: u32 = 2;

/// A parsed packet header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VsockHdr {
    pub src_cid: u64,
    pub dst_cid: u64,
    pub src_port: u32,
    pub dst_port: u32,
    /// Payload bytes following this header.
    pub len: u32,
    pub type_: u16,
    pub op: u16,
    pub flags: u32,
    /// Size of the sender's receive buffer.
    pub buf_alloc: u32,
    /// Bytes the sender has consumed from the peer's stream.
    pub fwd_cnt: u32,
}

impl VsockHdr {
    /// Decode a header, or `None` if `buf` is too short.
    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < HDR_SIZE {
            return None;
        }
        let u32_at = |o: usize| {
            u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]])
        };
        let u16_at = |o: usize| u16::from_le_bytes([buf[o], buf[o + 1]]);
        let u64_at = |o: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[o..o + 8]);
            u64::from_le_bytes(b)
        };
        Some(Self {
            src_cid: u64_at(0),
            dst_cid: u64_at(8),
            src_port: u32_at(16),
            dst_port: u32_at(20),
            len: u32_at(24),
            type_: u16_at(28),
            op: u16_at(30),
            flags: u32_at(32),
            buf_alloc: u32_at(36),
            fwd_cnt: u32_at(40),
        })
    }

    pub fn to_bytes(&self) -> [u8; HDR_SIZE] {
        let mut b = [0u8; HDR_SIZE];
        b[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        b[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        b[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        b[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        b[24..28].copy_from_slice(&self.len.to_le_bytes());
        b[28..30].copy_from_slice(&self.type_.to_le_bytes());
        b[30..32].copy_from_slice(&self.op.to_le_bytes());
        b[32..36].copy_from_slice(&self.flags.to_le_bytes());
        b[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        b[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        b
    }

    /// Build a reply on the connection this header arrived on.
    ///
    /// Addressed back to the sender, and always sourced from
    /// [`CID_HOST`]. The Linux driver drops a packet from any other CID,
    /// so a reply sourced from the guest's `dst_cid` never arrives.
    pub fn reply(&self, op: u16) -> Self {
        Self {
            src_cid: CID_HOST,
            dst_cid: self.src_cid,
            src_port: self.dst_port,
            dst_port: self.src_port,
            len: 0,
            type_: TYPE_STREAM,
            op,
            flags: 0,
            buf_alloc: 0,
            fwd_cnt: 0,
        }
    }
}

/// The two ports naming one connection, from the host's point of view.
///
/// One device serves one guest, so the guest CID is not part of the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnKey {
    pub guest_port: u32,
    pub host_port: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> VsockHdr {
        VsockHdr {
            src_cid: CID_HOST,
            dst_cid: 3,
            src_port: 0xdead_beef,
            dst_port: 1024,
            len: 0x1234,
            type_: TYPE_STREAM,
            op: OP_RW,
            flags: 0,
            buf_alloc: 64 * 1024,
            fwd_cnt: 42,
        }
    }

    #[test]
    fn header_is_44_bytes_on_the_wire() {
        assert_eq!(HDR_SIZE, 44);
        assert_eq!(sample().to_bytes().len(), 44);
    }

    #[test]
    fn round_trips() {
        let h = sample();
        assert_eq!(VsockHdr::from_bytes(&h.to_bytes()), Some(h));
    }

    #[test]
    fn fields_land_at_the_offsets_the_driver_expects() {
        let b = sample().to_bytes();
        assert_eq!(&b[0..8], &2u64.to_le_bytes(), "src_cid at 0");
        assert_eq!(&b[8..16], &3u64.to_le_bytes(), "dst_cid at 8");
        assert_eq!(&b[16..20], &0xdead_beefu32.to_le_bytes(), "src_port at 16");
        assert_eq!(&b[20..24], &1024u32.to_le_bytes(), "dst_port at 20");
        assert_eq!(&b[24..28], &0x1234u32.to_le_bytes(), "len at 24");
        assert_eq!(&b[28..30], &TYPE_STREAM.to_le_bytes(), "type at 28");
        assert_eq!(&b[30..32], &OP_RW.to_le_bytes(), "op at 30");
        assert_eq!(&b[32..36], &0u32.to_le_bytes(), "flags at 32");
        assert_eq!(&b[36..40], &65536u32.to_le_bytes(), "buf_alloc at 36");
        assert_eq!(&b[40..44], &42u32.to_le_bytes(), "fwd_cnt at 40");
    }

    #[test]
    fn a_short_buffer_decodes_to_nothing() {
        assert_eq!(VsockHdr::from_bytes(&[0u8; HDR_SIZE - 1]), None);
        assert!(VsockHdr::from_bytes(&[0u8; HDR_SIZE]).is_some());
    }

    /// Bytes past the header are payload and must not change the decode.
    #[test]
    fn trailing_payload_is_ignored_by_the_decoder() {
        let mut buf = sample().to_bytes().to_vec();
        buf.extend_from_slice(b"payload");
        assert_eq!(VsockHdr::from_bytes(&buf), Some(sample()));
    }

    /// A reply goes to the sender's ports from the host CID. The guest
    /// picks `dst_cid`, and the driver drops a reply that echoes it.
    #[test]
    fn a_reply_answers_the_sender_from_the_host_cid() {
        let from_guest = VsockHdr {
            src_cid: 3,
            dst_cid: 9999,
            ..sample()
        };
        let r = from_guest.reply(OP_RESPONSE);
        assert_eq!(r.src_cid, CID_HOST);
        assert_eq!(r.dst_cid, 3);
        assert_eq!(r.src_port, 1024);
        assert_eq!(r.dst_port, 0xdead_beef);
        assert_eq!(r.op, OP_RESPONSE);
        assert_eq!(r.len, 0);
    }

    #[test]
    fn reserved_cids_are_below_the_first_guest_id() {
        for reserved in [CID_HYPERVISOR, CID_LOCAL, CID_HOST] {
            assert!(reserved < CID_GUEST_MIN);
        }
    }
}
