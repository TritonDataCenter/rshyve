// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests for the device reset path.
//!
//! The illumos driver treats one register write as the whole reset and
//! reclaims the ring straight after it, so the reset is synchronous. It
//! is safe there because it waits for guest-memory sections only: never
//! for the disk.

use std::sync::atomic::AtomicBool;

use super::*;

/// A reset that takes longer than this is waiting on something other
/// than a memory copy.
const RESET_BUDGET: Duration = Duration::from_secs(1);

/// A window long enough for the work under test to have gone wrong.
const LONG_ENOUGH: Duration = Duration::from_millis(200);

/// Hold a worker at the point where it is about to call the disk.
///
/// Returns the flag a test sets to let it go. This stands in for a
/// backing store that never answers.
fn park_at_backend(ctx: &WorkerCtx) -> (Arc<AtomicBool>, Arc<AtomicBool>) {
    let reached = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let hook_reached = Arc::clone(&reached);
    let hook_release = Arc::clone(&release);
    *ctx.parks.at_backend.lock().expect("park lock") =
        Some(Arc::new(move || {
            hook_reached.store(true, Ordering::Release);
            while !hook_release.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
        }));
    (reached, release)
}

/// Hold the raising thread inside the interrupt callback.
///
/// Returns the flag that reports the raise and the flag that lets it
/// go. This stands in for an injection the kernel has not finished.
fn park_in_the_interrupt(
    blk: &VirtioBlock,
) -> (Arc<AtomicBool>, Arc<AtomicBool>) {
    let raised = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let hook_raised = Arc::clone(&raised);
    let hook_release = Arc::clone(&release);
    blk.interrupt.install(BackendIntr::detached(move |_, _| {
        hook_raised.store(true, Ordering::Release);
        while !hook_release.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
    }));
    (raised, release)
}

/// A one-request WRITE_ZEROES ring the worker completes on its own.
fn worker_harness() -> DwzHarness {
    dwz_harness(&DwzCase {
        segs: &[encode_seg(0, 8, 0)],
        ..Default::default()
    })
}

/// A one-request WRITE ring on an eight-sector disk.
fn write_harness(payload: &[u8]) -> DwzHarness {
    dwz_harness(&DwzCase {
        rtype: bits::VIRTIO_BLK_T_OUT,
        payload: Some(payload),
        ..Default::default()
    })
}

// Admission must close before the drain starts. A section refused by
// the admission check never queues on the lock, so stale work cannot
// starve the resetting writer: `std::sync::RwLock` gives a writer no
// priority, and a guest picks how much work is queued.
#[test]
fn admission_closes_before_the_drain() {
    let access = GuestAccess::new(1);
    let live = access.enter_current(0).expect("a section is inside");
    let tag = live.tag();

    access.close_all();

    assert!(
        access.enter(tag).is_none(),
        "a stale request queued on the lock the reset needs"
    );
    assert!(
        access.enter_current(0).is_none(),
        "a fresh request queued on the lock the reset needs"
    );

    drop(live);
    access.drain();
    access.reopen(IntrSession::INITIAL);
    assert!(
        access.enter_current(0).is_some(),
        "the next driver is shut out"
    );
}

// The wait covers the guest-memory sections, so it must not end while
// one is live.
#[test]
fn a_reset_waits_for_a_live_guest_access_section() {
    let access = GuestAccess::new(1);
    let live = access.enter_current(0).expect("a section is inside");
    let finished = AtomicBool::new(false);

    thread::scope(|scope| {
        scope.spawn(|| {
            access.close_all();
            access.drain();
            finished.store(true, Ordering::Release);
        });

        thread::sleep(LONG_ENOUGH);
        assert!(
            !finished.load(Ordering::Acquire),
            "the reset finished while a section could still write the ring"
        );
        drop(live);
    });

    assert!(finished.load(Ordering::Acquire), "the reset never finished");
}

// A disk that never answers must not delay the reset at all.
#[test]
fn a_reset_does_not_wait_for_the_backing_store() {
    let mut h = write_harness(&[0xAA; 512]);
    let (reached, release) = park_at_backend(&h.block.ctx);

    h.block.notify_queue(0, &mut h.queues, &h.physmap);
    wait_for("the worker never reached the backing store", || {
        reached.load(Ordering::Acquire)
    });

    // The reset runs on another thread under a deadline. A reset that
    // waits on the disk is waiting for a park that only this thread
    // releases, so waiting for it here would hang the run instead of
    // reporting the failure.
    let done = Arc::new(AtomicBool::new(false));
    let waited = {
        let block = &h.block;
        let finished = Arc::clone(&done);
        let release = Arc::clone(&release);
        thread::scope(move |s| {
            s.spawn(move || {
                VirtioDevice::reset(block);
                finished.store(true, Ordering::Release);
            });
            let start = Instant::now();
            while !done.load(Ordering::Acquire)
                && start.elapsed() < RESET_BUDGET
            {
                thread::sleep(Duration::from_micros(20));
            }
            let waited = start.elapsed();
            // Let a reset that should not have waited finish, so the
            // scope joins it rather than parking on it.
            release.store(true, Ordering::Release);
            waited
        })
    };

    assert!(
        waited < RESET_BUDGET,
        "the reset waited {waited:?} on the backing store"
    );

    // The write the reset could not cancel still lands, from its host
    // snapshot. Wait for it, so the harness outlives the syscall.
    wait_for("the parked write never reached the disk", || {
        read_backing(&h, 512).iter().all(|&b| b == 0xAA)
    });
}

// A device can be dropped while a worker is still inside a syscall,
// on an unplug or a teardown. The worker must not resume against a
// bare descriptor number: the host reopens those, and the write would
// land in whatever took it over.
#[test]
fn a_syscall_outlives_the_device_that_started_it() {
    use std::os::unix::fs::FileExt;

    let mut h = write_harness(&[0xAA; 512]);
    let (reached, release) = park_at_backend(&h.block.ctx);

    h.block.notify_queue(0, &mut h.queues, &h.physmap);
    wait_for("the worker never reached the backing store", || {
        reached.load(Ordering::Acquire)
    });

    let backing = h.backing.try_clone().expect("clone the backing file");
    drop(h.block);
    release.store(true, Ordering::Release);

    wait_for(
        "the write did not survive the device that started it",
        || {
            let mut head = [0u8; 512];
            backing.read_exact_at(&mut head, 0).is_ok()
                && head.iter().all(|&b| b == 0xAA)
        },
    );
}

// Injection is a call into the kernel with no latency bound, and the
// reset runs on the vCPU that wrote DEVICE_STATUS. The illumos driver
// reclaims the ring the moment that write returns, so the reset must not
// be able to queue behind an injection. The test times the whole reset,
// not only the drain, because a wait can sit after the drain.
#[test]
fn a_reset_does_not_wait_for_interrupt_injection() {
    let mut h = worker_harness();
    let (raised, release) = park_in_the_interrupt(&h.block);

    h.block.notify_queue(0, &mut h.queues, &h.physmap);
    wait_for("the worker never raised the interrupt", || {
        raised.load(Ordering::Acquire)
    });

    // The reset runs on another thread under a deadline. One that waits
    // on the injection is waiting for a park only this thread releases,
    // so waiting for it here would hang the run instead of failing it.
    let done = Arc::new(AtomicBool::new(false));
    let waited = {
        let block = &h.block;
        let finished = Arc::clone(&done);
        let release = Arc::clone(&release);
        thread::scope(move |s| {
            s.spawn(move || {
                VirtioDevice::reset(block);
                finished.store(true, Ordering::Release);
            });
            let start = Instant::now();
            while !done.load(Ordering::Acquire)
                && start.elapsed() < RESET_BUDGET
            {
                thread::sleep(Duration::from_micros(20));
            }
            let waited = start.elapsed();
            // Let a reset that should not have waited finish, so the
            // scope joins it rather than parking on it.
            release.store(true, Ordering::Release);
            waited
        })
    };

    assert!(
        waited < RESET_BUDGET,
        "the reset waited {waited:?} behind an interrupt injection"
    );
}

// The hostile case, through the ring. A guest can claim it posted more
// than it did, and keep claiming from a second vCPU while the first
// walks the ring. One notification must do at most a ring's worth of
// work, whatever the guest claims. A reset racing that walk must get
// through in the time one walk takes.
#[test]
fn a_guest_that_overclaims_the_ring_does_not_hold_the_resetting_vcpu() {
    const RING: u16 = 64;
    const AVAIL_IDX_GPA: u64 = 0x1102;
    const USED_IDX_GPA: u64 = 0x1202;
    let (physmap, queue, blk) = make_get_id_queue(RING, RING);
    let mut queues = [queue];
    let stop = AtomicBool::new(false);
    let done = AtomicBool::new(false);

    let used_idx = || -> u16 {
        physmap
            .lookup(USED_IDX_GPA, 2)
            .expect("mapped used index")
            .read::<u16>()
            .expect("read used index")
    };

    // Nothing is asserted inside the scope: an assertion there would
    // unwind with the guest thread still claiming, and the scope would
    // park on it instead of failing.
    let (did, waited) = thread::scope(|s| {
        // The second vCPU. Every avail entry names the same head, so
        // this needs only the index, and it keeps that index half a
        // ring ahead of wherever the device has got to. Its own
        // deadline is the backstop for a panic that skips `stop`.
        s.spawn(|| {
            let slot = physmap
                .lookup(AVAIL_IDX_GPA, 2)
                .expect("mapped avail index");
            let deadline = Instant::now() + Duration::from_secs(10);
            while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
                let ahead = used_idx().wrapping_add(1 << 14);
                slot.write::<u16>(&ahead).expect("write avail index");
            }
        });

        // One notification, against a claim that never runs out.
        blk.notify_queue(0, &mut queues, &physmap);
        let did = used_idx();

        // Now the same walk racing a reset. The reset runs on its own
        // thread under a deadline: one a guest can hold would park the
        // run instead of failing it.
        s.spawn(|| {
            blk.notify_queue(0, &mut queues, &physmap);
        });
        s.spawn(|| {
            VirtioDevice::reset(&blk);
            done.store(true, Ordering::Release);
        });
        let start = Instant::now();
        while !done.load(Ordering::Acquire) && start.elapsed() < RESET_BUDGET {
            thread::sleep(Duration::from_micros(20));
        }
        let waited = start.elapsed();
        stop.store(true, Ordering::Release);
        (did, waited)
    });

    assert!(
        did <= RING,
        "one notification did {did} requests on a ring of {RING}"
    );
    assert!(
        waited < RESET_BUDGET,
        "a guest overclaiming the ring held the resetting vCPU for {waited:?}"
    );
}

// The notify path runs under the transport's register lock, and a
// reset on another vCPU must take that lock before it can start. So
// an interrupt injected here would put a call into the kernel on the
// reset's path. The device asks the transport to raise it instead: the
// transport drops the lock first.
#[test]
fn the_notify_path_hands_the_interrupt_to_the_transport() {
    let (physmap, queue, blk) = make_get_id_queue(8, 5);
    let mut queues = [queue];
    let raised = Arc::new(AtomicBool::new(false));
    {
        let raised = Arc::clone(&raised);
        blk.interrupt.install(BackendIntr::detached(move |_, _| {
            raised.store(true, Ordering::Release)
        }));
    }

    let asked = blk.notify_queue(0, &mut queues, &physmap);

    assert_eq!(
        queues[0].read_used_ring_idx(&physmap),
        5,
        "the inline requests did not complete"
    );
    assert!(asked, "the transport was not asked to raise the interrupt");
    assert!(
        !raised.load(Ordering::Acquire),
        "the notify path injected an interrupt under the transport lock"
    );
}

// The other half of abandoning an injection. A reset cannot take back
// a call already inside the kernel, so it must stop the raises that
// have not started: VirtIO 1.3 sec 2.4.1 forbids queue interaction once
// a reset is complete, and the next driver would see an assertion for
// a ring it never used.
#[test]
fn an_old_session_interrupt_is_dropped_after_the_reset_returns() {
    let mut h = worker_harness();

    let raised = Arc::new(AtomicBool::new(false));
    {
        let raised = Arc::clone(&raised);
        h.block
            .interrupt
            .install(BackendIntr::detached(move |_, _| {
                raised.store(true, Ordering::Release)
            }));
    }

    // Hold the worker between the publication and the raise. That is
    // the gap a reset overtakes.
    let at_raise = Arc::new(AtomicBool::new(false));
    let past_raise = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    {
        let reached = Arc::clone(&at_raise);
        let passed = Arc::clone(&past_raise);
        let release = Arc::clone(&release);
        *h.block.ctx.parks.before_raise.lock().expect("park lock") =
            Some(Arc::new(move || {
                reached.store(true, Ordering::Release);
                while !release.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
                passed.store(true, Ordering::Release);
            }));
    }

    h.block.notify_queue(0, &mut h.queues, &h.physmap);
    wait_for("the worker never reached the raise", || {
        at_raise.load(Ordering::Acquire)
    });

    VirtioDevice::reset(&h.block);
    release.store(true, Ordering::Release);
    wait_for("the worker never left the park", || {
        past_raise.load(Ordering::Acquire)
    });
    thread::sleep(LONG_ENOUGH);

    assert!(
        !raised.load(Ordering::Acquire),
        "an interrupt for the session the reset ended reached the driver"
    );
}

// A guest that writes 0 to DEVICE_STATUS reclaims the vring straight
// after. A worker still holding a pre-reset chain must not run its I/O
// and must not publish into a ring the guest has recycled.
#[test]
fn a_worker_drops_requests_from_a_closed_session() {
    const RECYCLED_GPA: u64 = 0x2200;
    const SECTOR: usize = 512;

    let physmap =
        Arc::new(PhysMap::new_anon(0x1000, 0x2000).expect("queue memory"));
    let mut queue = VirtQueue::new(8);
    queue.set_addr_modern(0x1000, 0x1100, 0x1200);
    let completion = detached_completion(&queue, &physmap);
    let ctx = worker_ctx(&physmap);

    // The requests are built for the generation the reset then ends.
    let tag = current_tag(&ctx.access, 0);
    let (tx, rx) = mpsc::channel();
    for head in 0..5u16 {
        tx.send(BlkIoRequest {
            rtype: bits::VIRTIO_BLK_T_IN,
            sector: 0,
            chain: vec![
                ChainBuf::Readable {
                    addr: 0x2000,
                    len: BLK_REQ_HEADER_SIZE as u32,
                },
                ChainBuf::Writable {
                    addr: RECYCLED_GPA,
                    len: SECTOR as u32,
                },
                ChainBuf::Writable {
                    addr: 0x2100,
                    len: 1,
                },
            ],
            head,
            completion: Arc::clone(&completion),
            tag,
        })
        .expect("queue the request");
    }
    end_session(&ctx);
    drop(tx);

    let mut file = tempfile::tempfile().expect("create tempfile");
    std::io::Write::write_all(&mut file, &[0xAB; SECTOR])
        .expect("fill the backing file");
    let file = Arc::new(file);
    let limits = DiskLimits {
        capacity: 1,
        read_only: true,
        nodelete: true,
    };
    thread::spawn(move || blk_io_worker(rx, file, limits, ctx))
        .join()
        .expect("the worker exits when the channel disconnects");

    assert_eq!(
        queue.read_used_ring_idx(&physmap),
        0,
        "a request from the closed session published a used entry"
    );
    let mut recycled = [0u8; SECTOR];
    physmap
        .lookup(RECYCLED_GPA, SECTOR)
        .expect("mapped recycled page")
        .read_bytes(&mut recycled)
        .expect("read the recycled page");
    assert_eq!(
        recycled, [0u8; SECTOR],
        "a stale read wrote disk data into a recycled page"
    );
}

// A reset installs a fresh VirtioCompletion, and a pass publishes
// through the completion its first request named. A batch that spanned
// a reset would send the next driver's results to the ring the guest
// just freed, which drops them: the request is live and never
// completes. So a request from the other side of a reset starts its
// own pass.
#[test]
fn a_worker_batch_does_not_span_a_reset() {
    const HDR_GPA: u64 = 0x2000;
    const STATUS_GPA: u64 = 0x2100;

    let physmap =
        Arc::new(PhysMap::new_anon(0x1000, 0x2000).expect("queue memory"));
    physmap
        .lookup(HDR_GPA, BLK_REQ_HEADER_SIZE)
        .expect("mapped request header")
        .write(&VirtioBlkReqHdr {
            rtype: bits::VIRTIO_BLK_T_FLUSH,
            _reserved: 0,
            sector: 0,
        })
        .expect("write request header");

    let ring = |desc, avail, used| {
        let mut queue = VirtQueue::new(8);
        queue.set_addr_modern(desc, avail, used);
        let completion = detached_completion(&queue, &physmap);
        (queue, completion)
    };
    let (stale_queue, stale) = ring(0x1000, 0x1100, 0x1200);
    let (live_queue, live) = ring(0x1300, 0x1400, 0x1500);

    let ctx = worker_ctx(&physmap);
    let stale_tag = current_tag(&ctx.access, 0);
    end_session(&ctx);
    let live_tag = current_tag(&ctx.access, 0);

    let request =
        |head, completion: &Arc<VirtioCompletion>, tag| BlkIoRequest {
            rtype: bits::VIRTIO_BLK_T_FLUSH,
            sector: 0,
            chain: vec![
                ChainBuf::Readable {
                    addr: HDR_GPA,
                    len: BLK_REQ_HEADER_SIZE as u32,
                },
                ChainBuf::Writable {
                    addr: STATUS_GPA,
                    len: 1,
                },
            ],
            head,
            completion: Arc::clone(completion),
            tag,
        };

    // The queue the guest left behind, then the one it built next.
    let (tx, rx) = mpsc::channel();
    tx.send(request(0, &stale, stale_tag)).expect("send stale");
    tx.send(request(1, &live, live_tag)).expect("send live");
    tx.send(request(2, &live, live_tag)).expect("send live");
    drop(tx);

    let file = Arc::new(tempfile::tempfile().expect("create tempfile"));
    let limits = DiskLimits {
        capacity: 1,
        read_only: true,
        nodelete: true,
    };
    thread::spawn(move || blk_io_worker(rx, file, limits, ctx))
        .join()
        .expect("the worker exits when the channel disconnects");

    assert_eq!(
        stale_queue.read_used_ring_idx(&physmap),
        0,
        "a stale request reached the pre-reset used ring"
    );
    assert_eq!(
        live_queue.read_used_ring_idx(&physmap),
        2,
        "the next driver's requests were published through the ring the \
         reset took away, so they never completed"
    );
}

// A result whose guest side is finished still must not be published
// once the reset has taken the ring away.
#[test]
fn a_reset_between_the_copy_and_the_publication_publishes_nothing() {
    let mut h = dwz_harness(&DwzCase {
        segs: &[encode_seg(0, 8, 0)],
        ..Default::default()
    });
    {
        let ctx = h.block.ctx.clone();
        *h.block.ctx.parks.before_publish.lock().expect("park lock") =
            Some(Arc::new(move || {
                end_session(&ctx);
            }));
    }

    h.block.notify_queue(0, &mut h.queues, &h.physmap);

    // The disk work ran, so the worker did reach the publication step.
    wait_for("the request never reached the backing store", || {
        read_backing(&h, 8 * 512).iter().all(|&b| b == 0)
    });
    thread::sleep(LONG_ENOUGH);
    assert_eq!(
        h.queues[0].read_used_ring_idx(&h.physmap),
        0,
        "a used entry landed in the ring the reset took away"
    );
}

// Publishing and raising are separate steps because the raise must
// happen outside the permission that wrote the ring. Delivery can
// block, and the drain a synchronous reset waits for covers every live
// permission, so an interrupt raised inside one puts the reset behind
// it. The reset latency bound depends on this.
#[test]
fn an_interrupt_is_raised_outside_the_guest_access_section() {
    let physmap =
        Arc::new(PhysMap::new_anon(0x1000, 0x2000).expect("queue memory"));
    let mut queue = VirtQueue::new(8);
    queue.set_addr_modern(0x1000, 0x1100, 0x1200);

    let raising = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let completion = {
        let raising = Arc::clone(&raising);
        let release = Arc::clone(&release);
        VirtioCompletion::new(
            &queue,
            Arc::clone(&physmap),
            IntrSession::INITIAL,
            move |_| {
                raising.store(true, Ordering::Release);
                while !release.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
            },
        )
    };

    let ctx = worker_ctx(&physmap);
    let access = Arc::clone(&ctx.access);
    let tag = current_tag(&ctx.access, 0);
    let (tx, rx) = mpsc::channel();
    tx.send(BlkIoRequest {
        rtype: bits::VIRTIO_BLK_T_FLUSH,
        sector: 0,
        chain: vec![
            ChainBuf::Readable {
                addr: 0x2000,
                len: BLK_REQ_HEADER_SIZE as u32,
            },
            ChainBuf::Writable {
                addr: 0x2100,
                len: 1,
            },
        ],
        head: 0,
        completion,
        tag,
    })
    .expect("queue the request");
    drop(tx);

    let file = Arc::new(tempfile::tempfile().expect("create tempfile"));
    let limits = DiskLimits {
        capacity: 1,
        read_only: false,
        nodelete: true,
    };
    let worker = thread::spawn(move || blk_io_worker(rx, file, limits, ctx));
    wait_for("the worker never raised the interrupt", || {
        raising.load(Ordering::Acquire)
    });

    // The interrupt is held. A drain that waits for it is the failure,
    // so it runs on its own thread under a deadline.
    let waited = {
        let release = Arc::clone(&release);
        thread::scope(move |sc| {
            let drainer = sc.spawn(move || access.drain());
            let start = Instant::now();
            while !drainer.is_finished() && start.elapsed() < RESET_BUDGET {
                thread::sleep(Duration::from_millis(1));
            }
            let waited = start.elapsed();
            // Let a wrongly held interrupt go, so the scope joins the
            // drain rather than parking on it.
            release.store(true, Ordering::Release);
            waited
        })
    };
    worker.join().expect("the worker finished");

    assert!(
        waited < RESET_BUDGET,
        "the drain waited {waited:?} behind an interrupt raised inside a \
         guest-access section"
    );
}

// An interrupt the closed session asked for must not reach the driver
// that follows. VirtIO 1.3 sec 2.4.1 forbids queue interaction once a
// reset is complete.
#[test]
fn an_interrupt_from_a_closed_session_is_dropped() {
    let access = GuestAccess::new(1);
    let stale = current_tag(&access, 0);
    access.close_all();
    access.drain();
    access.reopen(IntrSession::INITIAL);
    let fresh = current_tag(&access, 0);

    let fired = AtomicBool::new(false);
    access.deliver(stale, || fired.store(true, Ordering::Release));
    assert!(
        !fired.load(Ordering::Acquire),
        "an interrupt fired for the session the reset ended"
    );

    access.deliver(fresh, || fired.store(true, Ordering::Release));
    assert!(fired.load(Ordering::Acquire), "the next driver gets none");
}

// The reset waits on guest access, which a worker holds only while it
// is using the ring, so it must never park the workers to find out.
#[test]
fn reset_never_parks_the_workers() {
    let (physmap, queue, blk) = make_get_id_queue(8, 5);
    let mut queues = [queue];

    blk.notify_queue(0, &mut queues, &physmap);
    VirtioDevice::reset(&blk);

    assert_eq!(
        blk.gate.drain_count(),
        0,
        "reset parked the workers to drain them"
    );
    assert!(!blk.gate.is_paused(), "reset left the workers parked");
}

// A reset raised while the device is already paused for migration must
// leave it paused: resuming here would let workers run behind the
// migration's back.
#[test]
fn reset_keeps_a_migration_pause() {
    let mut h = dwz_harness(&DwzCase {
        segs: &[encode_seg(0, 8, 0)],
        ..Default::default()
    });
    park_workers(&h.block);
    h.block.notify_queue(0, &mut h.queues, &h.physmap);

    VirtioDevice::reset(&h.block);

    assert!(h.block.gate.is_paused(), "reset cleared a migration pause");
    assert!(h.block.is_quiesced());
}

// The reset must leave the device able to serve the next driver, or
// its first request never completes.
#[test]
fn reset_leaves_the_workers_runnable() {
    let mut h = dwz_harness(&DwzCase {
        segs: &[encode_seg(0, 8, 0)],
        ..Default::default()
    });

    VirtioDevice::reset(&h.block);
    assert!(!h.block.gate.is_paused(), "reset left the workers parked");

    assert_eq!(dwz_run(&mut h), bits::VIRTIO_BLK_S_OK);
    assert!(read_backing(&h, 8 * 512).iter().all(|&b| b == 0));
}

// A read run inline holds the transport lock for as long as the disk
// takes, and a reset waits behind that lock. So every backing-store read
// goes to a worker.
#[test]
fn a_backing_store_read_never_runs_inline() {
    let mut h = dwz_harness(&DwzCase {
        rtype: bits::VIRTIO_BLK_T_IN,
        payload: Some(&[0u8; 512]),
        payload_writable: true,
        ..Default::default()
    });
    park_workers(&h.block);

    h.block.notify_queue(0, &mut h.queues, &h.physmap);

    assert_eq!(
        h.queues[0].read_used_ring_idx(&h.physmap),
        0,
        "a backing-store read ran on the notifying thread"
    );

    h.block.resume();
    wait_for("the dispatched read never completed", || {
        h.queues[0].read_used_ring_idx(&h.physmap) == 1
    });
}

/// Park every I/O worker, so a later dispatch is observable: the
/// requests sit in the worker channels instead of racing to completion.
fn park_workers(blk: &VirtioBlock) {
    blk.pause();
    wait_for("workers did not park within the budget", || {
        blk.is_quiesced()
    });
}

/// Build a completion handler the way a queue notification does, under
/// permission taken from `blk`.
fn completion_for(
    blk: &VirtioBlock,
    queue: &VirtQueue,
) -> Arc<VirtioCompletion> {
    let session = blk.ctx.access.enter_current(0).expect("a session is open");
    blk.get_completion(&session, 0, queue)
}

// The ring generation and the transport session are separate counters.
// A worker's check is one load. A whole reset can run after it passes,
// and "is admission open now" then says yes again. The session the
// completion handler was built in tells the two drivers apart, and the
// transport tests it in the same step that admits the delivery.
#[test]
fn a_completion_from_the_closed_session_raises_nothing() {
    let (_physmap, queue, blk) = make_get_id_queue(8, 0);
    let gate = Arc::new(IntrGate::new());
    let seen = Arc::new(Mutex::new(Vec::new()));
    blk.interrupt
        .install(BackendIntr::recording(Arc::clone(&gate), Arc::clone(&seen)));

    let stale = completion_for(&blk, &queue);

    // The whole reset, in the order the transport runs it: the session
    // ends first, then the backend resets, then admission reopens.
    gate.end_session();
    VirtioDevice::reset(&blk);
    gate.reopen();

    stale.signal();
    assert!(
        seen.lock().expect("record lock").is_empty(),
        "a completion of the closed session interrupted the driver that \
         followed"
    );

    // The worse failure of the two: a completion of the current session
    // must get through, or the guest waits for ever.
    let fresh = completion_for(&blk, &queue);
    fresh.signal();
    assert_eq!(
        seen.lock().expect("record lock").len(),
        1,
        "the driver that is running got no interrupt for its completion"
    );
}
