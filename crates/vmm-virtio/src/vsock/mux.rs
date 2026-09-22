// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The connection table and the guest-to-host dispatch.
//!
//! One device serves one guest, so the two ports alone name a
//! connection. Packets for the guest wait here until the guest posts an
//! RX chain.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use super::conn::{State, VsockConn};
use super::packet::{
    ConnKey, VsockHdr, CID_HOST, OP_REQUEST, OP_RESPONSE, OP_RST, OP_RW,
    OP_SHUTDOWN, SHUTDOWN_RCV, SHUTDOWN_SEND, TYPE_STREAM,
};

/// Bytes of guest-bound payload buffered before the device stops
/// reading host sockets. Without a cap a fast host writer grows VMM
/// memory without limit while the guest is not reading.
pub const RX_BACKLOG_MAX: usize = 1024 * 1024;

/// Live connections one device holds. A host peer and the guest can both
/// open them, and each costs a table entry, a host socket and a reader
/// thread. Without a cap either end can exhaust the VMM.
/// cloud-hypervisor caps its muxer at 1023 for the same reason.
pub const MAX_CONNS: usize = 1024;

/// Packets queued for the guest, of any kind.
///
/// [`RX_BACKLOG_MAX`] bounds payload bytes, and a control packet has
/// none. A guest that never posts an RX chain can make the device queue
/// replies faster than the queue drains, so control packets need this
/// separate bound. Deep enough for several per live connection.
pub const RX_PACKETS_MAX: usize = MAX_CONNS * 4;

/// First host port given to host-initiated connections, above the ports
/// a caller usually names.
const EPHEMERAL_PORT_BASE: u32 = 49152;

/// A packet waiting for an RX chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxPacket {
    pub hdr: VsockHdr,
    pub payload: Vec<u8>,
}

impl RxPacket {
    /// A header-only packet. Every control packet is one.
    pub fn control(hdr: VsockHdr) -> Self {
        Self {
            hdr,
            payload: Vec::new(),
        }
    }
}

/// What the device should do with a packet the guest sent.
#[derive(Debug, PartialEq, Eq)]
pub enum GuestAction {
    /// Nothing further.
    None,
    /// Write this payload to the connection's host socket.
    Deliver(ConnKey, Vec<u8>),
    /// Close the host socket for this connection and forget it.
    Close(ConnKey),
    /// A guest-initiated connection to a host port.
    Connect(ConnKey, u32),
}

/// Packets waiting for an RX chain, and the payload bytes they hold.
///
/// One lock: both caps are checked and applied atomically against the
/// queue and its byte count.
#[derive(Default)]
struct RxQueue {
    q: VecDeque<RxPacket>,
    bytes: usize,
}

pub struct VsockMux {
    guest_cid: u64,
    conns: Mutex<HashMap<ConnKey, VsockConn>>,
    rx: Mutex<RxQueue>,
    next_host_port: AtomicU32,
}

impl VsockMux {
    pub fn new(guest_cid: u64) -> Self {
        Self {
            guest_cid,
            conns: Mutex::new(HashMap::new()),
            rx: Mutex::new(RxQueue::default()),
            next_host_port: AtomicU32::new(EPHEMERAL_PORT_BASE),
        }
    }

    pub fn guest_cid(&self) -> u64 {
        self.guest_cid
    }

    /// Queue a packet for the guest. Returns false when the backlog is
    /// full, which tells the caller to stop reading its host socket.
    pub fn push_rx(&self, pkt: RxPacket) -> bool {
        let mut rx = self.rx.lock().expect("rx lock");
        // The packet cap holds for every kind of packet. A queue this
        // deep means the guest stopped draining and its connections are
        // already wedged, so a drop costs nothing more. cloud-hypervisor
        // drops an RST on a full queue for the same reason.
        if rx.q.len() >= RX_PACKETS_MAX {
            return false;
        }
        // Below that cap, a control packet is always admitted: a dropped
        // RST or RESPONSE wedges its connection.
        if !pkt.payload.is_empty()
            && rx.bytes.saturating_add(pkt.payload.len()) > RX_BACKLOG_MAX
        {
            return false;
        }
        rx.bytes = rx.bytes.saturating_add(pkt.payload.len());
        rx.q.push_back(pkt);
        true
    }

    /// Next packet for the guest, if any.
    pub fn pop_rx(&self) -> Option<RxPacket> {
        let mut rx = self.rx.lock().expect("rx lock");
        let pkt = rx.q.pop_front()?;
        rx.bytes = rx.bytes.saturating_sub(pkt.payload.len());
        Some(pkt)
    }

    /// Put a packet back at the head of the queue, keeping stream
    /// order, when a chain was too small for all of it.
    ///
    /// No cap applies: the packet was just popped, so the queue returns
    /// to a size it already held.
    pub fn requeue_rx_front(&self, pkt: RxPacket) {
        let mut rx = self.rx.lock().expect("rx lock");
        rx.bytes = rx.bytes.saturating_add(pkt.payload.len());
        rx.q.push_front(pkt);
    }

    pub fn rx_is_empty(&self) -> bool {
        self.rx.lock().expect("rx lock").q.is_empty()
    }

    /// Register a host-initiated connection under an exact key and
    /// queue its REQUEST.
    ///
    /// Production allocates the host port through
    /// [`Self::reserve_host_connect`]. This lets a test name the
    /// connection.
    #[cfg(test)]
    pub fn start_host_connect(&self, key: ConnKey) -> bool {
        {
            let mut conns = self.conns.lock().expect("conns lock");
            if conns.contains_key(&key) || conns.len() >= MAX_CONNS {
                return false;
            }
            conns.insert(key, VsockConn::new(State::Connecting));
        }
        self.announce_host_connect(key);
        true
    }

    /// Take a slot for a host-initiated connection to `guest_port`, and
    /// name it.
    ///
    /// The guest cannot reach the connection until
    /// [`Self::announce_host_connect`] queues its REQUEST, so until then
    /// the caller alone owns the host socket. Returns None when the
    /// table is at [`MAX_CONNS`].
    ///
    /// A key already in the table is skipped, not replaced. The guest
    /// names both ends of a connection it opens, so it can hold a key in
    /// this range, and an overwrite would merge two connections.
    pub fn reserve_host_connect(&self, guest_port: u32) -> Option<ConnKey> {
        let mut conns = self.conns.lock().expect("conns lock");
        if conns.len() >= MAX_CONNS {
            return None;
        }
        for _ in 0..=MAX_CONNS {
            let host_port = self.next_host_port.fetch_add(1, Ordering::Relaxed);
            let key = ConnKey {
                guest_port,
                host_port,
            };
            if let Entry::Vacant(slot) = conns.entry(key) {
                slot.insert(VsockConn::new(State::Connecting));
                return Some(key);
            }
        }
        None
    }

    /// Tell the guest about a slot taken by
    /// [`Self::reserve_host_connect`].
    ///
    /// After this call any thread's `deliver_rx` may give the REQUEST to
    /// the guest, so the caller must be finished with the host socket.
    pub fn announce_host_connect(&self, key: ConnKey) {
        let mut hdr = self.hdr_for(key, OP_REQUEST);
        self.stamp_credit(key, &mut hdr);
        self.push_rx(RxPacket::control(hdr));
    }

    /// Put this connection's receive window on a packet.
    ///
    /// Every header carries the credit fields (virtio 1.3 §5.10.6.3), and
    /// the guest reads them from the packet that opens the connection. A
    /// zero window there stops a guest that speaks first until the host
    /// sends payload, which may never happen.
    fn stamp_credit(&self, key: ConnKey, hdr: &mut VsockHdr) {
        let conns = self.conns.lock().expect("conns lock");
        if let Some(conn) = conns.get(&key) {
            let (buf_alloc, fwd_cnt) = conn.our_credit();
            hdr.buf_alloc = buf_alloc;
            hdr.fwd_cnt = fwd_cnt;
        }
    }

    /// Header addressed from the host side of `key` to the guest.
    fn hdr_for(&self, key: ConnKey, op: u16) -> VsockHdr {
        VsockHdr {
            src_cid: CID_HOST,
            dst_cid: self.guest_cid,
            src_port: key.host_port,
            dst_port: key.guest_port,
            len: 0,
            type_: TYPE_STREAM,
            op,
            flags: 0,
            buf_alloc: 0,
            fwd_cnt: 0,
        }
    }

    /// Queue as much host payload as the guest's credit and the backlog
    /// allow, and remove the queued bytes from `data`.
    ///
    /// The guest drops bytes that arrive past its receive window, which
    /// leaves a hole in the stream. The window is
    /// [`VsockConn::peer_credit`], which fails closed.
    ///
    /// `data` is the caller's retry buffer. A caller that offers the same
    /// buffer until it is empty sends every byte exactly once, with no
    /// count to subtract.
    ///
    /// Lock order: the connection lock, then the queue lock.
    pub fn queue_host_data(&self, key: ConnKey, data: &mut Vec<u8>) {
        let mut conns = self.conns.lock().expect("conns lock");
        let Some(conn) = conns.get_mut(&key) else {
            // No window opens for an unknown connection. Keeping the
            // bytes would park the caller for the life of the VM.
            data.clear();
            return;
        };
        let take = (conn.peer_credit() as usize).min(data.len());
        if take == 0 {
            // The window is shut. Every guest packet carries fresh
            // credit, so the next one can open it.
            return;
        }
        let (buf_alloc, fwd_cnt) = conn.our_credit();
        let mut hdr = self.hdr_for(key, OP_RW);
        hdr.len = take as u32;
        hdr.buf_alloc = buf_alloc;
        hdr.fwd_cnt = fwd_cnt;
        if !self.push_rx(RxPacket {
            hdr,
            payload: data[..take].to_vec(),
        }) {
            // The backlog is full: spend no credit and take nothing.
            return;
        }
        // Only queued bytes count against the window.
        conn.record_sent(take as u32);
        drop(conns);
        data.drain(..take);
    }

    /// Tell the guest the host end has gone.
    pub fn queue_host_close(&self, key: ConnKey) {
        let mut conns = self.conns.lock().expect("conns lock");
        let Some(conn) = conns.get_mut(&key) else {
            return;
        };
        conn.close_local_send();
        let closed = conn.is_closed();
        drop(conns);

        let mut hdr = self.hdr_for(key, OP_SHUTDOWN);
        hdr.flags = SHUTDOWN_RCV | SHUTDOWN_SEND;
        self.push_rx(RxPacket::control(hdr));
        if closed {
            self.forget(key);
        }
    }

    pub fn forget(&self, key: ConnKey) {
        self.conns.lock().expect("conns lock").remove(&key);
    }

    pub fn conn_count(&self) -> usize {
        self.conns.lock().expect("conns lock").len()
    }

    /// Apply one packet from the guest, queueing any reply it is owed.
    pub fn on_guest_packet(
        &self,
        hdr: &VsockHdr,
        payload: Vec<u8>,
    ) -> GuestAction {
        // The guest names the connection from its own side.
        let key = ConnKey {
            guest_port: hdr.src_port,
            host_port: hdr.dst_port,
        };

        // This device routes only packets addressed to the host CID.
        if hdr.dst_cid != CID_HOST {
            self.push_rx(RxPacket::control(hdr.reply(OP_RST)));
            return GuestAction::None;
        }

        let mut conns = self.conns.lock().expect("conns lock");

        if hdr.op == OP_REQUEST {
            // Refuse a duplicate, or a connection past the cap. Each one
            // costs the device a host socket and a reader thread.
            if conns.contains_key(&key) || conns.len() >= MAX_CONNS {
                drop(conns);
                self.push_rx(RxPacket::control(hdr.reply(OP_RST)));
                return GuestAction::None;
            }
            let mut conn = VsockConn::new(State::Established);
            conn.absorb_credit(hdr);
            conns.insert(key, conn);
            drop(conns);
            return GuestAction::Connect(key, hdr.dst_port);
        }

        let Some(conn) = conns.get_mut(&key) else {
            // Unknown connection. RST, not silence, so the guest stops
            // retrying.
            drop(conns);
            if hdr.op != OP_RST {
                self.push_rx(RxPacket::control(hdr.reply(OP_RST)));
            }
            return GuestAction::None;
        };

        let reply = conn.on_packet(hdr);
        let closed = conn.is_closed();
        let accepts = conn.accepts_guest_data();
        if hdr.op == OP_RW {
            conn.record_forwarded(payload.len() as u32);
        }
        drop(conns);

        if let Some(op) = reply {
            self.push_rx(RxPacket::control(hdr.reply(op)));
        }
        if closed {
            self.forget(key);
            return GuestAction::Close(key);
        }
        if hdr.op == OP_RW && accepts && !payload.is_empty() {
            return GuestAction::Deliver(key, payload);
        }
        GuestAction::None
    }

    /// Accept a guest-initiated connection the host side agreed to.
    pub fn accept_guest_connect(&self, key: ConnKey) {
        if !self.conns.lock().expect("conns lock").contains_key(&key) {
            return;
        }
        let mut hdr = self.hdr_for(key, OP_RESPONSE);
        self.stamp_credit(key, &mut hdr);
        self.push_rx(RxPacket::control(hdr));
    }

    /// Refuse a guest-initiated connection the host side could not make.
    pub fn refuse_guest_connect(&self, key: ConnKey) {
        self.forget(key);
        self.push_rx(RxPacket::control(self.hdr_for(key, OP_RST)));
    }
}

#[cfg(test)]
mod tests;
