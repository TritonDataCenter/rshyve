// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests for the virtio-console device.

use super::*;
use crate::bits;
use crate::pci::intr::IntrGate;
use crate::queue::VirtqDesc;
use crate::socket_halt::{HALT_BUDGET, HALT_JOIN_ROUNDS};
use std::io::Write;
use std::sync::atomic::AtomicUsize;
use std::sync::{mpsc, MutexGuard};
use std::time::{Duration, Instant};
use vmm_core::mem::PhysMap;
use vmm_devices::Lifecycle;

/// `bind_restricted` sets the process umask around its `bind`, so a
/// directory created by another thread in that window loses its
/// execute bit and every later bind inside it fails. Hold this over
/// the temporary directory and the bind together.
static BIND_LOCK: Mutex<()> = Mutex::new(());

const DESC_GPA: u64 = 0x1000;
const AVAIL_GPA: u64 = 0x1100;
const USED_GPA: u64 = 0x1200;
const DATA_GPA: u64 = 0x2000;
const DATA_B_GPA: u64 = 0x2100;

fn test_logger() -> slog::Logger {
    slog::Logger::root(slog::Discard, slog::o!())
}

fn bind_lock() -> MutexGuard<'static, ()> {
    // A panicking test must not cascade into every later one.
    BIND_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn console_in_tempdir(
    physmap: Arc<PhysMap>,
) -> (tempfile::TempDir, VirtioConsole) {
    let _guard = bind_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("console.sock");
    let console = VirtioConsole::new(&sock, physmap, test_logger())
        .expect("create console");
    (dir, console)
}

/// One configured queue whose descriptor 0 points at `DATA_GPA`.
fn make_queue(
    queue_size: u16,
    avail_idx: u16,
    flags: u16,
    len: u32,
) -> (Arc<PhysMap>, VirtQueue) {
    let physmap = Arc::new(
        PhysMap::new_anon(DESC_GPA, 0x2000).expect("create queue memory"),
    );
    let mut queue = VirtQueue::new(queue_size);
    queue.set_addr_modern(DESC_GPA, AVAIL_GPA, USED_GPA);
    queue.set_event_idx(true);

    write_desc(&physmap, 0, DATA_GPA, len, flags);
    physmap
        .lookup(AVAIL_GPA + 2, 2)
        .expect("mapped avail index")
        .write::<u16>(&avail_idx)
        .expect("write avail index");
    for idx in 0..queue_size {
        physmap
            .lookup(AVAIL_GPA + 4 + u64::from(idx) * 2, 2)
            .expect("mapped avail entry")
            .write::<u16>(&0)
            .expect("write avail entry");
    }
    (physmap, queue)
}

/// One configured queue whose single descriptor is a 4-byte
/// device-readable buffer holding "ping".
fn make_tx_queue(queue_size: u16, avail_idx: u16) -> (Arc<PhysMap>, VirtQueue) {
    let (physmap, queue) = make_queue(queue_size, avail_idx, 0, 4);
    physmap
        .lookup(DATA_GPA, 4)
        .expect("mapped payload")
        .write_bytes(b"ping")
        .expect("write payload");
    (physmap, queue)
}

fn write_desc(physmap: &PhysMap, idx: u64, addr: u64, len: u32, flags: u16) {
    physmap
        .lookup(DESC_GPA + idx * 16, 16)
        .expect("mapped descriptor")
        .write(&VirtqDesc {
            addr,
            len,
            flags,
            next: 0,
        })
        .expect("write descriptor");
}

fn publish_avail(physmap: &PhysMap, slot: u64, desc: u16, avail_idx: u16) {
    physmap
        .lookup(AVAIL_GPA + 4 + slot * 2, 2)
        .expect("mapped avail entry")
        .write::<u16>(&desc)
        .expect("write avail entry");
    physmap
        .lookup(AVAIL_GPA + 2, 2)
        .expect("mapped avail index")
        .write::<u16>(&avail_idx)
        .expect("write avail index");
}

fn read_bytes(physmap: &PhysMap, gpa: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    physmap
        .lookup(gpa, len)
        .expect("mapped region")
        .read_bytes(&mut buf)
        .expect("read region");
    buf
}

/// The used ring entry at `slot`, as (descriptor id, length).
fn read_used_entry(physmap: &PhysMap, slot: u64) -> (u32, u32) {
    let entry = physmap
        .lookup(USED_GPA + 4 + slot * 8, 8)
        .expect("mapped used entry");
    let id = entry.read::<u32>().expect("read used id");
    let len = entry
        .subregion(4, 4)
        .expect("used length")
        .read::<u32>()
        .expect("read used length");
    (id, len)
}

fn wait_for(mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if ready() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

#[test]
fn tx_notify_drains_ring_and_updates_avail_event() {
    let (physmap, tx_queue) = make_tx_queue(4, 2);
    let avail_event_gpa =
        tx_queue.used_addr() + 4 + u64::from(tx_queue.size()) * 8;
    let (_dir, console) = console_in_tempdir(Arc::clone(&physmap));

    // TX is queue index 1, so the slice needs both slots.
    let mut queues = [VirtQueue::new(4), tx_queue];
    console.notify_queue(CONSOLE_TX_QUEUE, &mut queues, &physmap);

    assert_eq!(queues[1].last_avail_idx(), 2);
    assert_eq!(queues[1].read_used_ring_idx(&physmap), 2);
    assert_eq!(
        physmap
            .lookup(avail_event_gpa, 2)
            .expect("mapped avail event")
            .read::<u16>()
            .expect("read avail event"),
        2,
    );
    console.pause();
    console.halt();
}

#[test]
fn rx_notify_stashes_chains_without_completing_them() {
    let (physmap, rx_queue) = make_tx_queue(4, 2);
    let (_dir, console) = console_in_tempdir(Arc::clone(&physmap));

    let mut queues = [rx_queue, VirtQueue::new(4)];
    let notify = console.notify_queue(CONSOLE_RX_QUEUE, &mut queues, &physmap);

    // No host bytes are waiting, so nothing may be published.
    assert!(!notify);
    assert_eq!(queues[0].last_avail_idx(), 2);
    assert_eq!(queues[0].read_used_ring_idx(&physmap), 0);
    console.pause();
    console.halt();
}

/// Taking the guest's whole receive ring in one pass must still arm
/// the kick. Linux posts a full ring at probe, so this is the first
/// pass, and a device that leaves `avail_event` behind the ring is
/// never kicked again: it spends the chains it holds and then holds
/// every host byte for a guest it cannot reach.
#[test]
fn a_ring_taken_whole_still_arms_the_next_kick() {
    let (physmap, rx_queue) = make_queue(4, 4, bits::VRING_DESC_F_WRITE, 8);
    let avail_event_gpa =
        rx_queue.used_addr() + 4 + u64::from(rx_queue.size()) * 8;
    let (_dir, console) = console_in_tempdir(Arc::clone(&physmap));

    let mut queues = [rx_queue, VirtQueue::new(4)];
    console.notify_queue(CONSOLE_RX_QUEUE, &mut queues, &physmap);

    assert_eq!(
        console.shared.pending_rx.lock().expect("pending").len(),
        4,
        "the whole ring should be stashed",
    );
    assert_eq!(
        physmap
            .lookup(avail_event_gpa, 2)
            .expect("mapped avail event")
            .read::<u16>()
            .expect("read avail event"),
        4,
        "avail_event left behind the ring: the guest will never kick again",
    );
    console.pause();
    console.halt();
}

/// The reader thread copies host bytes into a guest chain and
/// publishes a used entry, all outside the vCPU. The transport
/// publishes status 0 the moment `reset` returns, and the illumos
/// driver reclaims the ring DMA on the next line, so a delivery
/// already inside the ring must finish before the reset does.
#[test]
fn a_reset_waits_for_a_delivery_already_inside_the_ring() {
    let (physmap, rx_queue) = make_queue(4, 1, bits::VRING_DESC_F_WRITE, 8);
    let (_dir, console) = console_in_tempdir(Arc::clone(&physmap));
    let mut queues = [rx_queue, VirtQueue::new(4)];
    console.notify_queue(CONSOLE_RX_QUEUE, &mut queues, &physmap);

    // Stands in for a reader thread inside `deliver_rx`.
    let held = console
        .shared
        .access
        .enter_current(CONSOLE_RX_QUEUE)
        .expect("a delivery is admitted");
    let finished = AtomicBool::new(false);

    std::thread::scope(|scope| {
        scope.spawn(|| {
            VirtioDevice::reset(&console);
            finished.store(true, Ordering::Release);
        });
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !finished.load(Ordering::Acquire),
            "the reset returned while a delivery still held the ring",
        );
        drop(held);
    });

    assert!(finished.load(Ordering::Acquire));
    // And the delivery that follows finds no ring to write.
    assert!(console.shared.rx_completion.lock().expect("slot").is_none());
    console.halt();
}

#[test]
fn host_bytes_reach_guest_memory() {
    let (physmap, rx_queue) = make_queue(4, 1, bits::VRING_DESC_F_WRITE, 8);
    let (dir, console) = console_in_tempdir(Arc::clone(&physmap));
    let raised = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&raised);
    console.install_interrupt(BackendIntr::detached(
        move |_session, _queue| {
            counter.fetch_add(1, Ordering::Release);
        },
    ));

    let mut queues = [rx_queue, VirtQueue::new(4)];
    assert!(!console.notify_queue(CONSOLE_RX_QUEUE, &mut queues, &physmap));
    assert_eq!(queues[0].read_used_ring_idx(&physmap), 0);

    let mut client = UnixStream::connect(dir.path().join("console.sock"))
        .expect("connect console client");
    client.write_all(b"hi").expect("write host bytes");
    client.flush().expect("flush host bytes");

    assert!(
        wait_for(|| queues[0].read_used_ring_idx(&physmap) == 1),
        "host bytes were never published to the guest",
    );
    assert_eq!(read_bytes(&physmap, DATA_GPA, 2), b"hi");
    assert_eq!(read_used_entry(&physmap, 0), (0, 2));
    assert!(raised.load(Ordering::Acquire) >= 1);

    console.pause();
    console.halt();
}

/// The transmit drain runs on the vCPU under the transport lock, and
/// the guest picks the descriptor lengths. One kick over a ring of
/// 4 GiB descriptors is tens of terabytes of copying, during which the
/// vCPU never leaves the VMM and every other vCPU that touches this
/// device's registers waits behind it.
#[test]
fn a_transmit_kick_copies_at_most_its_byte_budget() {
    const REGION: usize = 4 * 1024 * 1024;
    const OVERSIZED: u32 = 3 * 1024 * 1024;

    let physmap = Arc::new(
        PhysMap::new_anon(DESC_GPA, REGION).expect("create guest memory"),
    );
    let mut tx_queue = VirtQueue::new(4);
    tx_queue.set_addr_modern(DESC_GPA, AVAIL_GPA, USED_GPA);
    tx_queue.set_event_idx(true);
    write_desc(&physmap, 0, DATA_GPA, OVERSIZED, 0);
    publish_avail(&physmap, 0, 0, 1);

    let (_dir, console) = console_in_tempdir(Arc::clone(&physmap));
    let (peer, device_end) = UnixStream::pair().expect("socketpair");
    console.shared.replace_client(Some(device_end));

    // Drain the peer so no write blocks, and count what arrives.
    let reader = std::thread::spawn(move || {
        let mut peer = peer;
        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0usize;
        while let Ok(n) = peer.read(&mut buf) {
            if n == 0 {
                break;
            }
            total += n;
        }
        total
    });

    let mut queues = [VirtQueue::new(4), tx_queue];
    console.notify_queue(CONSOLE_TX_QUEUE, &mut queues, &physmap);

    // Shuts the device end down, so the reader sees the end of input.
    console.shared.replace_client(None);
    let copied = reader.join().expect("the reader finished");

    assert_eq!(
        copied, TX_BYTES_PER_KICK,
        "one kick copied {copied} bytes of guest output",
    );
    assert_eq!(
        queues[1].read_used_ring_idx(&physmap),
        1,
        "the head never went back, so the ring stalled",
    );
    console.halt();
}

#[test]
fn reset_drops_posted_rx_chains() {
    let (physmap, rx_queue) = make_queue(4, 1, bits::VRING_DESC_F_WRITE, 8);
    let (dir, console) = console_in_tempdir(Arc::clone(&physmap));
    let mut queues = [rx_queue, VirtQueue::new(4)];
    console.notify_queue(CONSOLE_RX_QUEUE, &mut queues, &physmap);

    VirtioDevice::reset(&console);

    // A second chain, into a different buffer. Host bytes must land
    // here, which they cannot do if the dropped chain still leads.
    write_desc(&physmap, 1, DATA_B_GPA, 8, bits::VRING_DESC_F_WRITE);
    publish_avail(&physmap, 1, 1, 2);
    console.notify_queue(CONSOLE_RX_QUEUE, &mut queues, &physmap);

    let mut client = UnixStream::connect(dir.path().join("console.sock"))
        .expect("connect console client");
    client.write_all(b"hi").expect("write host bytes");
    client.flush().expect("flush host bytes");

    assert!(
        wait_for(|| queues[0].read_used_ring_idx(&physmap) == 1),
        "host bytes were never published to the guest",
    );
    assert_eq!(read_used_entry(&physmap, 0), (1, 2));
    assert_eq!(read_bytes(&physmap, DATA_B_GPA, 2), b"hi");
    assert_eq!(read_bytes(&physmap, DATA_GPA, 8), vec![0u8; 8]);

    console.pause();
    console.halt();
}

/// A client that stops reading must not hold the vCPU that is writing
/// to it. `write_host` runs on the transmit queue drain, so an
/// unbounded write there wedges the guest CPU, and a write done with
/// the client slot locked wedges every thread that needs the slot.
#[test]
fn a_console_client_that_never_reads_does_not_hold_the_writer() {
    let (_dir, console) = console_in_tempdir(Arc::new(PhysMap::new()));
    // The peer end is never read from, so the socket buffer fills and
    // stays full for as long as this test holds it.
    let (_peer, device_end) = UnixStream::pair().expect("socketpair");
    console.shared.replace_client(Some(device_end));

    let writer = {
        let shared = Arc::clone(&console.shared);
        std::thread::spawn(move || {
            shared.write_host(&vec![0u8; 8 * 1024 * 1024]);
        })
    };
    assert!(
        wait_for(|| writer.is_finished()),
        "the write never gave up on a client that stopped reading"
    );
    writer.join().expect("join the writer");
    assert!(
        console.shared.client.lock().expect("client lock").is_none(),
        "the wedged client kept the console slot"
    );

    console.pause();
    console.halt();
}

/// A client that reads, but too slowly to take the payload inside the
/// budget, must lose the console like one that reads nothing. The
/// write makes progress here, so a bound that only watched the first
/// blocked write would never fire.
#[test]
fn a_console_client_that_reads_too_slowly_loses_the_slot() {
    let (_dir, console) = console_in_tempdir(Arc::new(PhysMap::new()));
    let (peer, device_end) = UnixStream::pair().expect("socketpair");
    console.shared.replace_client(Some(device_end));

    // Small reads, spaced out, so the socket buffer frees a little at
    // a time and the write can never catch up with the payload.
    let taken = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&taken);
    let slow = std::thread::spawn(move || {
        let mut peer = peer;
        let mut buf = [0u8; 512];
        loop {
            std::thread::sleep(CLIENT_WRITE_TIMEOUT / 4);
            match peer.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => counted.fetch_add(n, Ordering::Release),
            };
        }
    });

    let writer = {
        let shared = Arc::clone(&console.shared);
        std::thread::spawn(move || {
            shared.write_host(&vec![0u8; 8 * 1024 * 1024]);
        })
    };
    assert!(
        wait_for(|| writer.is_finished()),
        "the write never gave up on a client that could not keep up"
    );
    writer.join().expect("join the writer");
    assert!(
        taken.load(Ordering::Acquire) > 0,
        "the client took nothing, so this did not test a partial write"
    );
    assert!(
        console.shared.client.lock().expect("client lock").is_none(),
        "the client the write gave up on kept the console slot"
    );
    slow.join().expect("join the slow client");

    console.pause();
    console.halt();
}

#[test]
fn quiesce_query_is_non_blocking() {
    let (_dir, console) = console_in_tempdir(Arc::new(PhysMap::new()));
    let device: Arc<dyn Lifecycle> = Arc::new(console);

    device.pause();
    let probe = Arc::clone(&device);
    assert!(vmm_devices_testsupport::assert_query_non_blocking(
        move || probe.is_quiesced()
    ));
    device.resume();
    device.pause();
    device.halt();
}

#[test]
fn virtio_console_is_documented_for_operators() {
    // Built from CARGO_MANIFEST_DIR, not from this file's depth, so
    // the check survives a move of the module into another crate.
    let devices = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/devices.md"
    ));
    assert!(
        devices.contains("| `virtio-console` |"),
        "docs/devices.md needs a virtio-console row in the PCI table"
    );
    assert!(
        devices.contains("### virtio-console"),
        "docs/devices.md needs a virtio-console section"
    );
    assert!(
        devices.contains("/dev/hvc0"),
        "docs/devices.md must name the guest device node"
    );
    let cli =
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/cli.md"));
    assert!(
        cli.contains("virtio-console"),
        "docs/cli.md needs a virtio-console -s example"
    );
}

#[test]
fn features_are_ring_only() {
    let (_dir, console) = console_in_tempdir(Arc::new(PhysMap::new()));
    let f = console.device_features();
    assert_ne!(f & bits::VIRTIO_F_RING_EVENT_IDX, 0);
    assert_ne!(f & bits::VIRTIO_F_RING_INDIRECT_DESC, 0);
    // No F_SIZE, no MULTIPORT, no EMERG_WRITE, and VERSION_1 is
    // the transport's to add.
    assert_eq!(f & bits::VIRTIO_F_VERSION_1, 0);
    assert_eq!(console.config_size(), CONSOLE_CONFIG_SIZE);
    console.pause();
    console.halt();
}

/// A chain the walker refuses must still go back on the used ring.
/// `pop_avail` already took its head out of the available ring, so
/// dropping it burns one descriptor for the life of the device. Enough
/// of them and the receive ring runs dry and the console goes deaf.
#[test]
fn a_refused_rx_chain_returns_its_descriptor() {
    let (physmap, rx_queue) = make_queue(4, 1, bits::VRING_DESC_F_WRITE, 8);
    // NEXT past the end of the descriptor table: the walker refuses it.
    physmap
        .lookup(DESC_GPA, 16)
        .expect("mapped descriptor")
        .write(&VirtqDesc {
            addr: DATA_GPA,
            len: 8,
            flags: bits::VRING_DESC_F_WRITE | bits::VRING_DESC_F_NEXT,
            next: 7,
        })
        .expect("write descriptor");
    let (_dir, console) = console_in_tempdir(Arc::clone(&physmap));

    let mut queues = [rx_queue, VirtQueue::new(4)];
    console.notify_queue(CONSOLE_RX_QUEUE, &mut queues, &physmap);

    assert_eq!(
        queues[0].read_used_ring_idx(&physmap),
        1,
        "the refused head never went back to the guest",
    );
    assert_eq!(read_used_entry(&physmap, 0), (0, 0));
    assert!(console
        .shared
        .pending_rx
        .lock()
        .expect("pending rx")
        .is_empty());
    console.pause();
    console.halt();
}

/// A thread the halt cannot wake must not hold the teardown sweep.
///
/// `halt` runs on every teardown, one device at a time, ahead of
/// `VM_DESTROY_SELF`. The readers sit in a blocking `read(2)` that only
/// `shutdown(2)` on their client ends, and nothing in userspace can
/// revoke a syscall a thread is already in. So the join itself carries
/// the bound.
#[test]
fn a_thread_the_halt_cannot_wake_does_not_hold_it() {
    let (_dir, console) = console_in_tempdir(Arc::new(PhysMap::new()));
    let wedged = Arc::new(AtomicBool::new(false));

    // Neither thread holds a socket, so nothing `halt` does reaches
    // them. Only the bound can end the wait.
    let accept = {
        let stop = Arc::clone(&wedged);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };
    let reader = {
        let stop = Arc::clone(&wedged);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };
    *console.accept.lock().expect("accept lock") = Some(accept);
    console
        .shared
        .readers
        .lock()
        .expect("reader lock")
        .push(reader);

    // The halt runs on its own thread so an unbounded join reports a
    // failure here instead of parking the whole test run.
    let console = Arc::new(console);
    let (tx, rx) = mpsc::channel();
    let halting = {
        let console = Arc::clone(&console);
        std::thread::spawn(move || {
            console.pause();
            console.halt();
            let _ = tx.send(());
        })
    };
    // Both sets are joined in turn, so the halt may take two budgets.
    let returned = rx.recv_timeout(HALT_BUDGET * 4);

    wedged.store(true, Ordering::Release);
    halting.join().expect("join the halting thread");
    assert!(
        returned.is_ok(),
        "a thread the halt could not wake held it: {returned:?}",
    );
}

// The RX handler and the transport session are two pieces of state a
// reset changes one after the other, and the client reader thread can
// be between them. Bytes delivered on the handler the reset dropped
// must not interrupt the driver that followed. The vsock RX path has
// the same shape. Both call sites of the rule need a test, or a revert
// at the untested one passes the suite.
#[test]
fn an_rx_completion_from_the_closed_session_raises_nothing() {
    let (physmap, rx_queue) = make_queue(4, 1, bits::VRING_DESC_F_WRITE, 8);
    let (_dir, console) = console_in_tempdir(Arc::clone(&physmap));
    let gate = Arc::new(IntrGate::new());
    let seen = Arc::new(Mutex::new(Vec::new()));
    console.install_interrupt(BackendIntr::recording(
        Arc::clone(&gate),
        Arc::clone(&seen),
    ));

    let session = console
        .shared
        .access
        .enter_current(CONSOLE_RX_QUEUE)
        .expect("a delivery is admitted");
    let stale = console.ensure_rx_completion(&session, &rx_queue);
    drop(session);

    // The whole reset, in the order the transport runs it: the session
    // ends first, then the backend resets, then admission reopens.
    gate.end_session();
    VirtioDevice::reset(&console);
    gate.reopen();

    stale.signal();
    assert!(
        seen.lock().expect("record lock").is_empty(),
        "input from the closed session interrupted the driver that followed"
    );

    // The worse failure of the two: input for the current session must
    // get through, or the guest waits for ever.
    let session = console
        .shared
        .access
        .enter_current(CONSOLE_RX_QUEUE)
        .expect("a delivery is admitted");
    console.ensure_rx_completion(&session, &rx_queue).signal();
    drop(session);
    assert_eq!(
        seen.lock().expect("record lock").len(),
        1,
        "the driver that is running got no interrupt for its input"
    );

    console.halt();
}

// The halt joins the accept thread and then the readers, one after
// the other, so the deadline it holds is the product and not the
// constant. Teardown derives its own bound from what this reports. A
// device that under-reports is cut off inside its own wait.
#[test]
fn the_declared_halt_budget_covers_both_joins_the_halt_runs() {
    let (_dir, console) = console_in_tempdir(Arc::new(PhysMap::new()));
    assert_eq!(
        Lifecycle::halt_budget(&console),
        HALT_BUDGET * HALT_JOIN_ROUNDS,
    );
    assert!(
        Lifecycle::halt_budget(&console) > HALT_BUDGET,
        "one join's budget does not cover two joins run in sequence",
    );
    console.halt();
}
