// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests for a multiqueue device: interrupt routing, and what one
//! queue's retirement does to the others.
//!
//! MSI-X gives each virtqueue its own vector, and a Linux guest binds
//! one handler per vector that examines only its own ring. A
//! completion signalled under the wrong queue index therefore reaches
//! no handler at all, and the request never finishes. INTx hides this,
//! because the shared handler walks every ring on the device.

use std::sync::atomic::AtomicBool;

use super::*;

const MQ_QUEUE_SIZE: u16 = 4;
/// First queue's ring memory. Later queues follow at `MQ_STRIDE`.
const MQ_BASE_GPA: u64 = 0x1000;
const MQ_STRIDE: u64 = 0x1000;
const MQ_SECTORS: u64 = 8;
/// Backing-store fill, so a completed read is distinguishable from an
/// untouched payload buffer.
const MQ_FILL: u8 = 0xAB;

/// Guest addresses of one queue's ring and its single request.
struct MqRing {
    desc: u64,
    avail: u64,
    used: u64,
    header: u64,
    status: u64,
    payload: u64,
}

fn mq_ring(queue_idx: u16) -> MqRing {
    let base = MQ_BASE_GPA + u64::from(queue_idx) * MQ_STRIDE;
    MqRing {
        desc: base,
        avail: base + 0x100,
        used: base + 0x200,
        header: base + 0x300,
        status: base + 0x310,
        payload: base + 0x400,
    }
}

struct MqHarness {
    physmap: Arc<PhysMap>,
    queues: Vec<VirtQueue>,
    block: VirtioBlock,
    /// What the interrupt gate admitted, in order.
    seen: Arc<Mutex<Vec<(IntrSession, u16)>>>,
}

/// A device of `num_queues` request queues, each with its own ring and
/// a recording interrupt path behind it.
fn mq_harness(num_queues: u16) -> MqHarness {
    use std::io::Write;

    let span = MQ_STRIDE * u64::from(num_queues) + MQ_STRIDE;
    let physmap = Arc::new(
        PhysMap::new_anon(MQ_BASE_GPA, span as usize).expect("queue mem"),
    );

    let queues = (0..num_queues)
        .map(|q| {
            let ring = mq_ring(q);
            let mut queue = VirtQueue::new(MQ_QUEUE_SIZE);
            queue.set_addr_modern(ring.desc, ring.avail, ring.used);
            queue.set_event_idx(true);
            queue
        })
        .collect();

    let mut file = tempfile::tempfile().expect("create tempfile");
    file.write_all(&vec![MQ_FILL; (MQ_SECTORS * 512) as usize])
        .expect("fill backing file");
    file.flush().expect("flush backing file");

    let opts = VirtioBlockOpts::default();
    let block = VirtioBlock::new(file, &opts, Arc::clone(&physmap), num_queues)
        .expect("create VirtioBlock");

    let seen = Arc::new(Mutex::new(Vec::new()));
    block.interrupt.install(BackendIntr::recording(
        Arc::new(IntrGate::new()),
        Arc::clone(&seen),
    ));

    MqHarness {
        physmap,
        queues,
        block,
        seen,
    }
}

/// Publish a one-sector read on `queue_idx` and drive it to
/// completion.
///
/// A read reaches the backing store, so it is dispatched to a worker
/// and completed there. That is the path every real disk request
/// takes, and the only one that raises its own interrupt.
fn read_on_queue(h: &mut MqHarness, queue_idx: u16) {
    post_read_on_queue(h, queue_idx);
    await_read_on_queue(h, queue_idx);
    // The worker publishes the used entry and raises after it, so the
    // record is not there yet when the ring says the read is done.
    let seen = Arc::clone(&h.seen);
    wait_for("the completion raised no interrupt", || {
        !seen.lock().expect("record lock").is_empty()
    });
}

/// Publish a one-sector read on `queue_idx` and kick the device,
/// without waiting for it to finish.
fn post_read_on_queue(h: &mut MqHarness, queue_idx: u16) {
    let ring = mq_ring(queue_idx);

    let header = VirtioBlkReqHdr {
        rtype: bits::VIRTIO_BLK_T_IN,
        _reserved: 0,
        sector: 0,
    };
    h.physmap
        .lookup(ring.header, BLK_REQ_HEADER_SIZE)
        .expect("mapped request header")
        .write(&header)
        .expect("write request header");
    poke(&h.physmap, ring.status, &[0xFF]);
    poke(&h.physmap, ring.payload, &[0u8; 512]);

    let descs = [
        VirtqDesc {
            addr: ring.header,
            len: BLK_REQ_HEADER_SIZE as u32,
            flags: bits::VRING_DESC_F_NEXT,
            next: 1,
        },
        VirtqDesc {
            addr: ring.payload,
            len: 512,
            flags: bits::VRING_DESC_F_NEXT | bits::VRING_DESC_F_WRITE,
            next: 2,
        },
        VirtqDesc {
            addr: ring.status,
            len: 1,
            flags: bits::VRING_DESC_F_WRITE,
            next: 0,
        },
    ];
    for (i, desc) in descs.iter().enumerate() {
        h.physmap
            .lookup(ring.desc + (i as u64) * 16, 16)
            .expect("mapped descriptor")
            .write(desc)
            .expect("write descriptor");
    }

    poke(&h.physmap, ring.avail + 4, &0u16.to_le_bytes());
    poke(&h.physmap, ring.avail + 2, &1u16.to_le_bytes());

    h.block.notify_queue(queue_idx, &mut h.queues, &h.physmap);
}

/// Wait for `queue_idx`'s read to finish and check what the guest sees.
fn await_read_on_queue(h: &MqHarness, queue_idx: u16) {
    let ring = mq_ring(queue_idx);

    let physmap = Arc::clone(&h.physmap);
    let used = ring.used;
    wait_for("the read never completed", || {
        let mut idx = [0u8; 2];
        physmap
            .lookup(used + 2, 2)
            .expect("mapped used index")
            .read_bytes(&mut idx)
            .expect("read used index");
        u16::from_le_bytes(idx) == 1
    });

    let mut status = [0u8; 1];
    h.physmap
        .lookup(ring.status, 1)
        .expect("mapped status byte")
        .read_bytes(&mut status)
        .expect("read status byte");
    assert_eq!(
        status[0],
        bits::VIRTIO_BLK_S_OK,
        "queue {queue_idx} read failed"
    );

    let mut data = [0u8; 512];
    h.physmap
        .lookup(ring.payload, 512)
        .expect("mapped payload")
        .read_bytes(&mut data)
        .expect("read payload");
    assert!(
        data.iter().all(|&b| b == MQ_FILL),
        "queue {queue_idx} got no disk data, so the request never ran"
    );
}

/// Take and clear the queue indexes recorded so far.
fn drain_seen(h: &MqHarness) -> Vec<u16> {
    let mut seen = h.seen.lock().expect("record lock");
    let indexes = seen.iter().map(|&(_, q)| q).collect();
    seen.clear();
    indexes
}

// The interrupt a worker raises names a virtqueue, and under MSI-X
// that index alone picks the vector. The guest's handler for queue N
// reads ring N and nothing else, so the index must be the queue the
// request was submitted on. Linux blk-mq puts vCPU k's I/O on hardware
// queue k, so an index fixed at 0 stalls every guest with two or more
// vCPUs.
#[test]
fn a_completion_raises_on_the_queue_it_was_submitted_on() {
    const QUEUES: u16 = 4;
    let mut h = mq_harness(QUEUES);

    for queue_idx in 0..QUEUES {
        read_on_queue(&mut h, queue_idx);
        assert_eq!(
            drain_seen(&h),
            vec![queue_idx],
            "a read submitted on queue {queue_idx} interrupted another queue"
        );
    }
}

// With one queue, index 0 is correct by accident. Keep it covered, so a
// multiqueue change cannot break the single-queue case.
#[test]
fn a_single_queue_completion_still_raises_on_queue_zero() {
    let mut h = mq_harness(1);
    read_on_queue(&mut h, 0);
    assert_eq!(drain_seen(&h), vec![0]);
}

// A driver programs each ring separately, so queue 0's addresses can be
// written while queue 1 has a request at the disk. That request names a
// ring the driver never gave up and still owes the guest a used entry
// and a status byte. A device-wide retirement here discards it, and the
// guest waits for it for ever.
#[test]
fn reprogramming_one_queue_keeps_anothers_request() {
    // Free, mapped ring memory past both queues' own.
    const NEW_DESC: u64 = MQ_BASE_GPA + 2 * MQ_STRIDE;
    const NEW_AVAIL: u64 = NEW_DESC + 0x100;
    const NEW_USED: u64 = NEW_DESC + 0x200;

    let mut h = mq_harness(2);

    // Hold queue 1's read at the disk, with no guest-access permission
    // taken, which is where a real request spends its time.
    let parked = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    {
        let parked = Arc::clone(&parked);
        let release = Arc::clone(&release);
        *h.block
            .ctx
            .parks
            .at_backend
            .lock()
            .expect("park lock poisoned") = Some(Arc::new(move || {
            parked.store(true, Ordering::Release);
            while !release.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
        }));
    }

    post_read_on_queue(&mut h, 1);
    wait_for("queue 1's read never reached the disk", || {
        parked.load(Ordering::Acquire)
    });

    // Queue 0 is reprogrammed while that read is still out.
    h.queues[0].set_addr_modern(NEW_DESC, NEW_AVAIL, NEW_USED);
    h.block.queue_addr_set(0, &h.queues[0]);

    release.store(true, Ordering::Release);
    await_read_on_queue(&h, 1);
    assert_eq!(
        drain_seen(&h),
        vec![1],
        "queue 1's completion raised on the wrong queue, or not at all"
    );
}
