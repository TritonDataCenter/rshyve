// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The reset measured where the guest sees it, not at the drain.
//!
//! `GuestAccess::drain` is one step of `VirtioDevice::reset`. A test of
//! the drain alone cannot see a wait that comes after it, so every
//! measurement here brackets the whole call.

use super::*;

/// A reset that takes longer than this waited for something other
/// than a bounded memory copy.
const WHOLE_RESET_BUDGET: Duration = Duration::from_secs(1);

/// Run `reset` on its own thread and return how long it took.
///
/// A broken reset waits for a park that only the caller can release.
/// The caller releases it after the deadline, so a broken reset gives a
/// failed assertion, not a hung run.
fn time_reset(fs: &VirtioFs, release: &dyn Fn()) -> Duration {
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| VirtioDevice::reset(fs));
        let start = Instant::now();
        while !worker.is_finished() && start.elapsed() < WHOLE_RESET_BUDGET {
            std::thread::sleep(Duration::from_micros(20));
        }
        let waited = start.elapsed();
        release();
        waited
    })
}

// A worker is held inside the interrupt callback, in place of an
// injection the kernel has not finished. The reset must not queue behind
// it: the illumos driver reclaims the ring when its one register write
// returns.
#[test]
fn a_reset_does_not_wait_for_interrupt_injection() {
    let (physmap, mut queues, fs) = ring(8, 1);
    seed_fuse_init(&physmap);
    fs.start().expect("start");

    let hold = Arc::new(AtomicBool::new(true));
    let (_raised, inside) = counting_interrupt(&fs, &hold);

    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
    let deadline = Instant::now() + PARK_BUDGET;
    while !inside.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        inside.load(Ordering::Acquire),
        "the worker never raised the interrupt"
    );

    let waited = time_reset(&fs, &|| hold.store(false, Ordering::Release));

    assert!(
        waited < WHOLE_RESET_BUDGET,
        "the reset waited {waited:?} behind an interrupt injection"
    );
}

// The guest controls how much stale work waits when it writes 0 to
// DEVICE_STATUS. A full queue of it must not make the reset longer, and
// none of it may reach the ring after: the driver can reclaim that
// memory when the write returns.
#[test]
fn a_full_queue_of_stale_work_does_not_lengthen_the_reset() {
    const BACKLOG: u16 = FS_QUEUE_SIZE_DEFAULT;
    let (physmap, mut queues, fs) = ring(BACKLOG, BACKLOG);
    let req = usize::from(FS_REQUEST_QUEUE);
    fs.start().expect("start");

    // Park the worker so a full queue piles up behind it, as a guest
    // can arrange before it writes the reset register.
    park_the_worker(&fs);
    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
    assert_eq!(
        fs.inflight.load(Ordering::Acquire),
        usize::from(BACKLOG),
        "the backlog never reached the worker"
    );

    let waited = time_reset(&fs, &|| ());

    // Release the worker on the backlog. Every request in it names the
    // ended session.
    fs.resume();
    let deadline = Instant::now() + BACKLOG_BUDGET;
    while fs.inflight.load(Ordering::Acquire) != 0 && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(1));
    }

    assert!(
        waited < WHOLE_RESET_BUDGET,
        "a backlog of {BACKLOG} held the resetting vCPU for {waited:?}"
    );
    assert_eq!(
        fs.inflight.load(Ordering::Acquire),
        0,
        "the backlog never retired"
    );
    assert_eq!(
        queues[req].read_used_ring_idx(&physmap),
        0,
        "a stale request published into the ring the reset gave back"
    );
}

// A reset never waits for the backing store: a host filesystem that
// stops answering must not hold a vCPU.
#[test]
fn a_parked_backing_store_does_not_lengthen_the_reset() {
    let (physmap, mut queues, fs) = ring(8, 1);
    seed_fuse_init(&physmap);
    fs.start().expect("start");

    let park = ParkedBackend::install(&fs);
    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
    park.wait_until_inside();

    let waited = time_reset(&fs, &|| park.release());

    assert!(
        waited < WHOLE_RESET_BUDGET,
        "the reset waited {waited:?} behind the backing store"
    );
}

// The notify path runs on the vCPU under the transport's register lock,
// which a reset on another vCPU must take before it closes admission.
// An injection here would let a guest delay a reset. So this path
// returns the heads it answers to the transport, which drops the lock
// first.
#[test]
fn the_notify_path_hands_the_interrupt_to_the_transport() {
    let (physmap, mut queues, fs) = ring(8, 1);
    // A chain that runs off the descriptor table, so the walk answers
    // the head itself.
    physmap
        .lookup(DESC_GPA, 16)
        .expect("mapped request descriptor")
        .write(&VirtqDesc {
            addr: REQ_GPA,
            len: 64,
            flags: crate::bits::VRING_DESC_F_NEXT,
            next: 8,
        })
        .expect("write request descriptor");
    fs.start().expect("start");

    let raised = Arc::new(AtomicBool::new(false));
    {
        let raised = Arc::clone(&raised);
        fs.interrupt
            .install(BackendIntr::detached(move |_session, _queue| {
                raised.store(true, Ordering::Release)
            }));
    }

    let asked = fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);

    assert_eq!(
        queues[usize::from(FS_REQUEST_QUEUE)].read_used_ring_idx(&physmap),
        1,
        "the head the walk answered never reached the used ring"
    );
    assert!(asked, "the transport was not asked to raise the interrupt");
    assert!(
        !raised.load(Ordering::Acquire),
        "the notify path injected an interrupt under the transport lock"
    );
}

// The worker's check is one load, and a whole reset can run after it
// passes. The session stamped on the completion handler keeps a reply
// off the next driver: the transport checks it in the same step that
// admits the delivery.
#[test]
fn a_completion_from_the_closed_session_raises_nothing() {
    let (_physmap, queues, fs) = ring(8, 0);
    let req = usize::from(FS_REQUEST_QUEUE);
    let gate = Arc::new(IntrGate::new());
    let seen = Arc::new(Mutex::new(Vec::new()));
    fs.interrupt
        .install(BackendIntr::recording(Arc::clone(&gate), Arc::clone(&seen)));

    // Built as a queue notification builds it: inside a session.
    let handler = || {
        let session = fs
            .access
            .enter_current(FS_REQUEST_QUEUE)
            .expect("a generation is open");
        fs.get_completion(&session, FS_REQUEST_QUEUE, &queues[req])
    };
    let stale = handler();

    // The transport's reset order: end the session, reset the backend,
    // reopen admission.
    gate.end_session();
    VirtioDevice::reset(&fs);
    gate.reopen();

    stale.signal();
    assert!(
        seen.lock().expect("record lock").is_empty(),
        "a reply of the closed session interrupted the driver that followed"
    );

    // A reply of the current session must get through, or the guest
    // waits indefinitely. This is the worse of the two failures.
    handler().signal();
    assert_eq!(
        seen.lock().expect("record lock").len(),
        1,
        "the driver that is running got no interrupt for its reply"
    );
}
