// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unit tests for the connection table and the guest-bound queue.

use super::*;
use crate::vsock::conn::DEFAULT_BUF_ALLOC;
use crate::vsock::packet::OP_CREDIT_UPDATE;

const GUEST_CID: u64 = 3;

fn mux() -> VsockMux {
    VsockMux::new(GUEST_CID)
}

fn guest_hdr(op: u16, guest_port: u32, host_port: u32) -> VsockHdr {
    VsockHdr {
        src_cid: GUEST_CID,
        dst_cid: CID_HOST,
        src_port: guest_port,
        dst_port: host_port,
        len: 0,
        type_: TYPE_STREAM,
        op,
        flags: 0,
        buf_alloc: 64 * 1024,
        fwd_cnt: 0,
    }
}

fn drain(m: &VsockMux) -> Vec<RxPacket> {
    let mut out = Vec::new();
    while let Some(p) = m.pop_rx() {
        out.push(p);
    }
    out
}

/// Bytes one reader pass offers the mux, as `HOST_READ_CHUNK` does.
const CHUNK: usize = 64 * 1024;

/// A window far larger than the backlog, for tests of the backlog
/// rather than credit. A guest may advertise this and read nothing.
const HUGE_WINDOW: u32 = 64 * 1024 * 1024;

/// A host-initiated connection the guest has answered, advertising
/// `window` bytes of receive buffer.
fn established(m: &VsockMux, key: ConnKey, window: u32) {
    assert!(m.start_host_connect(key));
    let mut h = guest_hdr(OP_RESPONSE, key.guest_port, key.host_port);
    h.buf_alloc = window;
    m.on_guest_packet(&h, vec![]);
    drain(m);
}

/// What the guest sends as it reads: its window and the running count
/// of host bytes it consumed.
fn credit_update(m: &VsockMux, key: ConnKey, window: u32, fwd_cnt: u32) {
    let mut h = guest_hdr(OP_CREDIT_UPDATE, key.guest_port, key.host_port);
    h.buf_alloc = window;
    h.fwd_cnt = fwd_cnt;
    m.on_guest_packet(&h, vec![]);
}

/// Offer `data` once, as one reader pass does, and report the bytes
/// the mux took.
fn offer(m: &VsockMux, key: ConnKey, data: &mut Vec<u8>) -> usize {
    let before = data.len();
    m.queue_host_data(key, data);
    before - data.len()
}

/// Offer chunks until a pass takes nothing, and report the total.
fn fill_backlog(m: &VsockMux, key: ConnKey) -> usize {
    let mut sent = 0usize;
    loop {
        let took = offer(m, key, &mut vec![0u8; CHUNK]);
        if took == 0 {
            return sent;
        }
        sent += took;
    }
}

/// Offer `total` bytes in reader-sized chunks and drain the queue
/// between passes, as a guest that takes packets but reads none. Stop
/// at the first pass that takes nothing.
fn offer_draining(m: &VsockMux, key: ConnKey, total: usize) -> usize {
    let mut sent = 0usize;
    while sent < total {
        let took = offer(m, key, &mut vec![0u8; (total - sent).min(CHUNK)]);
        drain(m);
        if took == 0 {
            break;
        }
        sent += took;
    }
    sent
}

/// A host close before the guest answers must free the entry.
/// Otherwise a peer that connects and hangs up in a loop fills the
/// table, and the cap refuses every later connection for the life of
/// the VM.
#[test]
fn a_host_close_before_the_guest_answers_frees_the_slot() {
    let m = mux();
    let key = ConnKey {
        guest_port: 1024,
        host_port: 1,
    };
    assert!(m.start_host_connect(key));
    assert_eq!(m.conn_count(), 1);

    m.queue_host_close(key);

    assert_eq!(
        m.conn_count(),
        0,
        "a connection the guest never answered was never reclaimed"
    );
}

/// The guest takes its send window from the REQUEST on a host-initiated
/// connection. A zero window stops a guest that speaks first until the
/// host sends payload.
#[test]
fn a_request_carries_the_window_the_guest_may_send_into() {
    let m = mux();
    let key = ConnKey {
        guest_port: 1024,
        host_port: 50000,
    };
    assert!(m.start_host_connect(key));

    let pkts = drain(&m);
    assert_eq!(pkts[0].hdr.op, OP_REQUEST);
    assert_eq!(
        pkts[0].hdr.buf_alloc, DEFAULT_BUF_ALLOC,
        "a REQUEST that advertises no window"
    );
}

#[test]
fn a_host_connect_queues_a_request_to_the_guest() {
    let m = mux();
    let key = ConnKey {
        guest_port: 1024,
        host_port: 50000,
    };
    m.start_host_connect(key);

    let pkts = drain(&m);
    assert_eq!(pkts.len(), 1);
    assert_eq!(pkts[0].hdr.op, OP_REQUEST);
    assert_eq!(pkts[0].hdr.src_cid, CID_HOST);
    assert_eq!(pkts[0].hdr.dst_cid, GUEST_CID);
    assert_eq!(pkts[0].hdr.dst_port, 1024);
    assert_eq!(m.conn_count(), 1);
}

#[test]
fn the_guest_response_establishes_it() {
    let m = mux();
    let key = ConnKey {
        guest_port: 1024,
        host_port: 50000,
    };
    m.start_host_connect(key);
    drain(&m);

    let act = m.on_guest_packet(&guest_hdr(OP_RESPONSE, 1024, 50000), vec![]);
    assert_eq!(act, GuestAction::None);
    assert!(drain(&m).is_empty(), "an accepted response needs no reply");
}

#[test]
fn guest_payload_is_delivered_to_the_host_socket() {
    let m = mux();
    let key = ConnKey {
        guest_port: 1024,
        host_port: 50000,
    };
    m.start_host_connect(key);
    m.on_guest_packet(&guest_hdr(OP_RESPONSE, 1024, 50000), vec![]);
    drain(&m);

    let mut h = guest_hdr(OP_RW, 1024, 50000);
    h.len = 5;
    let act = m.on_guest_packet(&h, b"hello".to_vec());
    assert_eq!(act, GuestAction::Deliver(key, b"hello".to_vec()));
}

#[test]
fn a_guest_request_opens_a_connection_outward() {
    let m = mux();
    let act = m.on_guest_packet(&guest_hdr(OP_REQUEST, 1024, 5000), vec![]);
    assert_eq!(
        act,
        GuestAction::Connect(
            ConnKey {
                guest_port: 1024,
                host_port: 5000
            },
            5000
        )
    );
    assert_eq!(m.conn_count(), 1);
}

#[test]
fn accepting_a_guest_connect_answers_with_response() {
    let m = mux();
    m.on_guest_packet(&guest_hdr(OP_REQUEST, 1024, 5000), vec![]);
    let key = ConnKey {
        guest_port: 1024,
        host_port: 5000,
    };
    m.accept_guest_connect(key);

    let pkts = drain(&m);
    assert_eq!(pkts.len(), 1);
    assert_eq!(pkts[0].hdr.op, OP_RESPONSE);
    assert_ne!(pkts[0].hdr.buf_alloc, 0, "must advertise a window");
}

#[test]
fn refusing_a_guest_connect_answers_with_rst() {
    let m = mux();
    m.on_guest_packet(&guest_hdr(OP_REQUEST, 1024, 5000), vec![]);
    let key = ConnKey {
        guest_port: 1024,
        host_port: 5000,
    };
    m.refuse_guest_connect(key);

    let pkts = drain(&m);
    assert_eq!(pkts.len(), 1);
    assert_eq!(pkts[0].hdr.op, OP_RST);
    assert_eq!(m.conn_count(), 0);
}

/// A packet for a connection nobody knows must be answered, or the
/// guest retries forever.
#[test]
fn an_unknown_connection_gets_rst() {
    let m = mux();
    let act = m.on_guest_packet(&guest_hdr(OP_RW, 7, 9), b"x".to_vec());
    assert_eq!(act, GuestAction::None);
    let pkts = drain(&m);
    assert_eq!(pkts.len(), 1);
    assert_eq!(pkts[0].hdr.op, OP_RST);
}

/// An RST for an unknown connection must not be answered with
/// another RST, or two peers ping-pong forever.
#[test]
fn an_rst_for_an_unknown_connection_is_dropped() {
    let m = mux();
    m.on_guest_packet(&guest_hdr(OP_RST, 7, 9), vec![]);
    assert!(drain(&m).is_empty());
}

/// Only the host CID is routable. The refusal must come from the host
/// CID: the Linux driver drops a packet from any other source, so an
/// RST from the CID the guest named is lost and the guest keeps
/// retrying.
#[test]
fn a_packet_for_another_cid_is_refused() {
    let m = mux();
    let mut h = guest_hdr(OP_REQUEST, 1024, 5000);
    h.dst_cid = 99;
    let act = m.on_guest_packet(&h, vec![]);
    assert_eq!(act, GuestAction::None);
    let refusal = &drain(&m)[0].hdr;
    assert_eq!(refusal.op, OP_RST);
    assert_eq!(refusal.src_cid, CID_HOST, "the guest will drop this RST");
    assert_eq!(refusal.dst_cid, GUEST_CID);
    assert_eq!(m.conn_count(), 0, "no connection for a foreign CID");
}

#[test]
fn a_duplicate_request_is_refused_without_disturbing_the_original() {
    let m = mux();
    m.on_guest_packet(&guest_hdr(OP_REQUEST, 1024, 5000), vec![]);
    drain(&m);
    let act = m.on_guest_packet(&guest_hdr(OP_REQUEST, 1024, 5000), vec![]);
    assert_eq!(act, GuestAction::None);
    assert_eq!(drain(&m)[0].hdr.op, OP_RST);
    assert_eq!(m.conn_count(), 1);
}

#[test]
fn shutdown_from_the_guest_closes_and_reports() {
    let m = mux();
    m.on_guest_packet(&guest_hdr(OP_REQUEST, 1024, 5000), vec![]);
    drain(&m);
    let key = ConnKey {
        guest_port: 1024,
        host_port: 5000,
    };

    let mut h = guest_hdr(OP_SHUTDOWN, 1024, 5000);
    h.flags = SHUTDOWN_RCV | SHUTDOWN_SEND;
    assert_eq!(m.on_guest_packet(&h, vec![]), GuestAction::Close(key));
    assert_eq!(m.conn_count(), 0);
}

/// A guest may advertise any window, so credit alone lets a guest that
/// claims a large buffer grow VMM memory while it reads nothing.
#[test]
fn the_rx_backlog_is_bounded() {
    let m = mux();
    let key = ConnKey {
        guest_port: 1024,
        host_port: 50000,
    };
    established(&m, key, HUGE_WINDOW);

    let mut queued = 0usize;
    for _ in 0..64 {
        queued += offer(&m, key, &mut vec![0u8; CHUNK]);
    }
    assert!(queued > 0, "some data should be accepted");
    assert!(queued < 64 * CHUNK, "the backlog must refuse eventually");
    assert!(queued <= RX_BACKLOG_MAX + CHUNK);
}

/// A dropped control packet wedges its connection, so the byte cap
/// does not apply to control packets.
#[test]
fn control_packets_bypass_the_backlog_cap() {
    let m = mux();
    let key = ConnKey {
        guest_port: 1024,
        host_port: 50000,
    };
    established(&m, key, HUGE_WINDOW);
    fill_backlog(&m, key);

    assert!(
        m.push_rx(RxPacket::control(m.hdr_for(key, OP_RST))),
        "a control packet must still be admitted when full"
    );
}

#[test]
fn popping_frees_backlog_room() {
    let m = mux();
    let key = ConnKey {
        guest_port: 1024,
        host_port: 50000,
    };
    established(&m, key, HUGE_WINDOW);
    fill_backlog(&m, key);

    drain(&m);
    assert_eq!(
        offer(&m, key, &mut vec![0u8; CHUNK]),
        CHUNK,
        "room must come back once the guest has taken the packets"
    );
}

/// Each connection costs a host socket and a reader thread, so the
/// guest must stop at a cap.
#[test]
fn the_guest_cannot_grow_the_connection_table_without_bound() {
    let m = mux();
    for port in 0..(MAX_CONNS as u32 + 64) {
        m.on_guest_packet(&guest_hdr(OP_REQUEST, port, 5000), vec![]);
        drain(&m);
    }
    assert_eq!(m.conn_count(), MAX_CONNS);

    // The one over the cap is refused, and told so.
    let act = m.on_guest_packet(&guest_hdr(OP_REQUEST, 900_000, 5000), vec![]);
    assert_eq!(act, GuestAction::None);
    assert_eq!(drain(&m)[0].hdr.op, OP_RST);
}

/// The same cap, driven by a host peer that connects again and again.
#[test]
fn host_connects_stop_at_the_connection_cap() {
    let m = mux();
    for port in 0..MAX_CONNS as u32 {
        assert!(m.start_host_connect(ConnKey {
            guest_port: 1024,
            host_port: 50_000 + port,
        }));
        drain(&m);
    }

    assert!(!m.start_host_connect(ConnKey {
        guest_port: 1024,
        host_port: 40_000,
    }));
    assert_eq!(m.conn_count(), MAX_CONNS);
    assert!(
        drain(&m).is_empty(),
        "a refused connect must not ask the guest to open one"
    );
}

/// The byte cap never sees a control packet. A guest that stops
/// draining must still not grow the queue with packets the device
/// answers.
#[test]
fn control_packets_cannot_grow_the_rx_queue_without_bound() {
    let m = mux();
    for port in 0..(RX_PACKETS_MAX as u32 * 2) {
        m.on_guest_packet(&guest_hdr(OP_RW, port, 9), vec![]);
    }
    assert_eq!(m.conn_count(), 0, "none of these open a connection");
    assert_eq!(drain(&m).len(), RX_PACKETS_MAX);
}

/// A guest names both ends of a connection it opens, so it can take a
/// key in the ephemeral range. The allocator must skip that key. A
/// replaced entry merges the host peer and the guest connection, and
/// the first reader to exit closes both.
#[test]
fn a_host_port_the_guest_already_holds_is_skipped() {
    let m = mux();
    let a = m.reserve_host_connect(1024).expect("first");
    assert_ne!(a.host_port, 0);
    assert!(a.host_port >= EPHEMERAL_PORT_BASE);

    // The guest opens the key this allocator would hand out next.
    let taken = a.host_port + 1;
    m.on_guest_packet(&guest_hdr(OP_REQUEST, 2048, taken), vec![]);
    assert_eq!(m.conn_count(), 2);

    let b = m.reserve_host_connect(2048).expect("second");
    assert_ne!(b.host_port, taken, "the guest's connection was overwritten");
    assert_eq!(m.conn_count(), 3, "a live connection was replaced");
}

/// The guest drops bytes past its published window, so the device must
/// not send past it.
#[test]
fn the_host_sends_no_more_than_the_guest_advertised() {
    let m = mux();
    let key = ConnKey {
        guest_port: 1024,
        host_port: 50000,
    };
    const WINDOW: u32 = 4096;
    established(&m, key, WINDOW);

    let mut data = vec![7u8; CHUNK];
    assert_eq!(offer(&m, key, &mut data), WINDOW as usize);
    assert_eq!(
        data.len(),
        CHUNK - WINDOW as usize,
        "the rest must stay with the caller, not be dropped"
    );

    let pkts = drain(&m);
    let queued: usize = pkts.iter().map(|p| p.payload.len()).sum();
    assert_eq!(queued, WINDOW as usize, "sent past the guest's window");
    for p in &pkts {
        assert_eq!(p.hdr.len as usize, p.payload.len(), "len is a lie");
        assert_eq!(p.hdr.buf_alloc, DEFAULT_BUF_ALLOC, "our own window");
    }
}

/// A guest that advertises no window gets nothing, and the bytes wait.
/// Fails if an empty window admits one more packet, or if the bytes
/// that do not fit are discarded.
#[test]
fn zero_credit_queues_nothing_and_keeps_the_bytes() {
    let m = mux();
    let key = ConnKey {
        guest_port: 1024,
        host_port: 50000,
    };
    established(&m, key, 0);

    let mut data = vec![1u8; 1024];
    assert_eq!(offer(&m, key, &mut data), 0);
    assert_eq!(data.len(), 1024, "bytes were dropped, not held");
    assert!(drain(&m).is_empty(), "nothing may go with no credit");
}

/// A host peer may write when it gets its OK line, before the guest
/// answers the REQUEST. The guest has no window yet, so nothing goes.
#[test]
fn nothing_goes_before_the_guest_has_answered() {
    let m = mux();
    let key = ConnKey {
        guest_port: 1024,
        host_port: 50000,
    };
    assert!(m.start_host_connect(key));
    drain(&m);

    let mut data = vec![9u8; 512];
    assert_eq!(offer(&m, key, &mut data), 0);
    assert!(drain(&m).is_empty(), "payload before the guest answered");
}

/// 512 KiB of echo through the window Linux advertises, the size the
/// guest test harness uses. The caller offers one buffer until it is
/// empty. Fails if queued bytes stay in the buffer (duplicates), if
/// unqueued bytes leave it (loss), or if the mux takes all or nothing
/// (no progress).
#[test]
fn a_chunk_larger_than_the_guest_window_crosses_once_and_in_order() {
    let m = mux();
    let key = ConnKey {
        guest_port: 1024,
        host_port: 50000,
    };
    const WINDOW: u32 = DEFAULT_BUF_ALLOC;
    established(&m, key, WINDOW);

    let source: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
    let mut data = source.clone();
    let mut got: Vec<u8> = Vec::new();

    // The reader loop, with a guest that reads as it goes.
    for _ in 0..64 {
        m.queue_host_data(key, &mut data);
        for pkt in drain(&m) {
            got.extend_from_slice(&pkt.payload);
        }
        if data.is_empty() {
            break;
        }
        credit_update(&m, key, WINDOW, got.len() as u32);
    }

    assert!(data.is_empty(), "the transfer never finished");
    assert_eq!(got.len(), source.len(), "the guest saw a different count");
    assert_eq!(got, source, "the guest read a different stream");
}

/// Credit comes back only when the guest says it has read. Fails if the
/// window is read once and held.
#[test]
fn credit_returning_lets_the_rest_flow() {
    let m = mux();
    let key = ConnKey {
        guest_port: 1024,
        host_port: 50000,
    };
    const WINDOW: u32 = 4096;
    established(&m, key, WINDOW);

    let mut data = vec![3u8; 3 * WINDOW as usize];
    assert_eq!(offer(&m, key, &mut data), WINDOW as usize);
    drain(&m);

    assert_eq!(
        offer(&m, key, &mut data),
        0,
        "the window opened without the guest saying it had read"
    );

    credit_update(&m, key, WINDOW, WINDOW);
    assert_eq!(
        offer(&m, key, &mut data),
        WINDOW as usize,
        "credit did not come back"
    );
    assert_eq!(data.len(), WINDOW as usize);
}

/// Credit is spent only on bytes the guest sees. Counting a packet the
/// backlog refused shuts the window on unsent bytes and stalls the
/// connection. Fails if the send is recorded before the queue takes
/// the packet.
#[test]
fn a_packet_the_backlog_refused_spends_no_credit() {
    let m = mux();
    let key = ConnKey {
        guest_port: 1024,
        host_port: 50000,
    };
    // Wider than the backlog, so the backlog refuses first and the
    // window is still open when it does.
    const WINDOW: u32 = 2 * RX_BACKLOG_MAX as u32;
    established(&m, key, WINDOW);

    let filled = fill_backlog(&m, key);
    assert!(filled > 0 && filled <= RX_BACKLOG_MAX);

    // None of these offers reaches the guest, so none counts against
    // the window.
    for _ in 0..8 {
        assert_eq!(offer(&m, key, &mut vec![0u8; CHUNK]), 0);
    }
    drain(&m);

    // The guest took the packets and read none, so the window left is
    // what it advertised less what went.
    let rest = offer_draining(&m, key, WINDOW as usize);
    assert_eq!(
        filled + rest,
        WINDOW as usize,
        "credit was spent on packets the guest never saw"
    );
}

/// An unknown connection never opens a window, so the bytes must go.
/// A held buffer parks the reader thread until shutdown.
#[test]
fn payload_for_a_forgotten_connection_does_not_park_the_reader() {
    let m = mux();
    let key = ConnKey {
        guest_port: 1024,
        host_port: 50000,
    };
    let mut data = vec![5u8; 64];
    m.queue_host_data(key, &mut data);
    assert!(data.is_empty(), "the reader would wait for a dead window");
    assert!(drain(&m).is_empty(), "nowhere to send it");
}

/// A guest that reports consuming more bytes than it was sent must not
/// get past the window it advertised. `peer_credit` fails closed, and
/// the mux must honour that.
#[test]
fn a_lying_guest_gets_no_more_than_it_advertised() {
    const WINDOW: u32 = 4096;
    // Just past the window, far past it, and near the wrap.
    for lie in [WINDOW + 1, 1_000_000, u32::MAX - 1024] {
        let m = mux();
        let key = ConnKey {
            guest_port: 1024,
            host_port: 50000,
        };
        established(&m, key, WINDOW);
        credit_update(&m, key, WINDOW, lie);

        let mut data = vec![2u8; CHUNK];
        let took = offer(&m, key, &mut data);
        assert!(
            took <= WINDOW as usize,
            "a fwd_cnt of {lie} bought {took} bytes of window"
        );
    }
}

/// The guest's send window reopens only when a host packet carries the
/// host `fwd_cnt`. A payload packet with a zero count stalls the guest
/// after one window. Fails if the mux does not count guest bytes, or
/// does not stamp the count on host payload.
#[test]
fn payload_tells_the_guest_how_much_of_its_stream_we_took() {
    let m = mux();
    let key = ConnKey {
        guest_port: 1024,
        host_port: 50000,
    };
    const WINDOW: u32 = 4096;
    const FROM_GUEST: usize = 3000;
    established(&m, key, WINDOW);

    let mut h = guest_hdr(OP_RW, key.guest_port, key.host_port);
    h.buf_alloc = WINDOW;
    m.on_guest_packet(&h, vec![0u8; FROM_GUEST]);
    drain(&m);

    let mut data = vec![1u8; 512];
    assert_eq!(offer(&m, key, &mut data), 512);

    let pkts = drain(&m);
    assert_eq!(pkts.len(), 1);
    assert_eq!(pkts[0].hdr.op, OP_RW);
    assert_eq!(
        pkts[0].hdr.fwd_cnt, FROM_GUEST as u32,
        "the guest's send window never reopens"
    );
    assert_eq!(pkts[0].hdr.buf_alloc, DEFAULT_BUF_ALLOC, "our own window");
}

/// A guest publishes its window on its REQUEST and may send nothing
/// more before it reads, as with a host service that speaks first.
/// Fails if the connection ignores the REQUEST's credit, which keeps
/// the window shut.
#[test]
fn a_guest_request_opens_the_window_it_names() {
    let m = mux();
    let key = ConnKey {
        guest_port: 1024,
        host_port: 5000,
    };
    const WINDOW: u32 = 8192;
    let mut h = guest_hdr(OP_REQUEST, key.guest_port, key.host_port);
    h.buf_alloc = WINDOW;
    m.on_guest_packet(&h, vec![]);
    m.accept_guest_connect(key);
    drain(&m);

    let mut data = vec![4u8; CHUNK];
    assert_eq!(
        offer(&m, key, &mut data),
        WINDOW as usize,
        "the window the guest named on its REQUEST was dropped"
    );
}
