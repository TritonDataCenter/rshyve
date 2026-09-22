// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests for the backend ordering barrier.
//!
//! A write already inside its syscall when a ring is retired still
//! reaches the disk. It cannot touch guest memory, but it must not
//! overtake a write the next driver sends to the same sector.

use std::sync::atomic::AtomicBool;

use super::*;
use crate::access::RingTag;

/// Long enough that a barrier which does not hold new work back would
/// have let it through.
const LONG_ENOUGH_TO_OVERTAKE: Duration = Duration::from_millis(200);

/// A wait past this is a barrier holding work back, not a slow thread.
const HELD_BACK: Duration = Duration::from_secs(5);

/// The permission gate and the barrier, wired as the device wires them.
///
/// The barrier decides what order the disk sees. The gate decides whose
/// work is cancelled. A test that builds only one of them proves nothing
/// about either decision.
struct Rings {
    access: Arc<GuestAccess>,
    barrier: Arc<BackendBarrier>,
}

impl Rings {
    fn new(num_queues: usize) -> Self {
        Self {
            access: Arc::new(GuestAccess::new(num_queues)),
            barrier: Arc::new(BackendBarrier::new()),
        }
    }

    /// The tag a request dispatched on `queue` now would carry.
    fn tag(&self, queue: u16) -> RingTag {
        self.access
            .enter_current(queue)
            .expect("a generation is open")
            .tag()
    }

    /// Retire one ring, the way `queue_addr_set` does.
    fn retire(&self, queue: u16) {
        self.access.close_queue(queue);
        self.barrier.retire();
        self.access.drain();
        self.access.reopen(IntrSession::INITIAL);
    }
}

// The decisive ordering case: a write admitted before the retirement
// must land before any backend work of the next generation.
#[test]
fn a_stale_write_holds_new_generation_io_behind_it() {
    let rings = Rings::new(1);
    let stale = rings
        .barrier
        .admit(&rings.access, rings.tag(0), true)
        .expect("the first write is admitted");
    rings.retire(0);
    let fresh_tag = rings.tag(0);

    let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let fresh = {
        let barrier = Arc::clone(&rings.barrier);
        let access = Arc::clone(&rings.access);
        let order = Arc::clone(&order);
        thread::spawn(move || {
            let _ticket = barrier
                .admit(&access, fresh_tag, true)
                .expect("the new generation is admitted");
            order.lock().expect("order lock").push("new");
        })
    };

    thread::sleep(LONG_ENOUGH_TO_OVERTAKE);
    assert!(
        order.lock().expect("order lock").is_empty(),
        "new work reached the disk before the stale write finished"
    );

    order.lock().expect("order lock").push("old");
    drop(stale);
    fresh.join().expect("the new write never ran");

    assert_eq!(
        *order.lock().expect("order lock"),
        ["old", "new"],
        "the stale write did not land first"
    );
}

// Work the retirement cancelled is refused rather than run.
#[test]
fn a_cancelled_generation_is_refused() {
    let rings = Rings::new(1);
    let stale = rings.tag(0);
    rings.retire(0);
    assert!(
        rings.barrier.admit(&rings.access, stale, true).is_none(),
        "a request from the retired generation reached the disk"
    );
    assert!(rings
        .barrier
        .admit(&rings.access, rings.tag(0), true)
        .is_some());
}

// Reprogramming one ring must not cancel backend work on another. The
// driver gave up one ring, and a request on any other queue still owes
// the guest its status byte.
#[test]
fn retiring_one_queue_leaves_another_queues_work_admitted() {
    let rings = Rings::new(2);
    let other = rings.tag(1);
    rings.retire(0);

    assert!(
        rings.barrier.admit(&rings.access, other, true).is_some(),
        "queue 1's in-flight write was cancelled by queue 0's reprogram"
    );
}

// A read cannot put the wrong bytes in a sector, so one that never
// returns must not wedge the next generation's I/O.
#[test]
fn a_read_does_not_hold_the_next_generation_back() {
    let rings = Rings::new(1);
    let stuck = rings
        .barrier
        .admit(&rings.access, rings.tag(0), false)
        .expect("the read is admitted");
    rings.retire(0);
    let fresh_tag = rings.tag(0);

    // On another thread with a deadline. A barrier that counts the read
    // as a blocker makes this wait for a ticket the test still holds.
    // That is a deadlock, not a slow answer, so the test must report it
    // as a failure and not park on it.
    let admitted = Arc::new(AtomicBool::new(false));
    let probe = {
        let barrier = Arc::clone(&rings.barrier);
        let access = Arc::clone(&rings.access);
        let admitted = Arc::clone(&admitted);
        thread::spawn(move || {
            let ticket = barrier.admit(&access, fresh_tag, true);
            admitted.store(ticket.is_some(), Ordering::Release);
        })
    };

    let deadline = Instant::now() + HELD_BACK;
    while !probe.is_finished() {
        assert!(
            Instant::now() < deadline,
            "a read held the next generation's writes back"
        );
        thread::sleep(Duration::from_millis(1));
    }
    probe.join().expect("the probe finished");
    assert!(
        admitted.load(Ordering::Acquire),
        "a read held the next generation's writes back"
    );
    drop(stuck);
}

// A retirement must wake the waiters it has just cancelled. Their own
// generation is over, so they must give up now. Waiting for the
// stale write to end is not good enough: it may never end, and the
// worker would then sit on a ring the guest has already replaced.
#[test]
fn a_retire_wakes_the_waiters_it_cancelled() {
    let rings = Rings::new(1);
    // Held for the whole test, so nothing but a retirement can wake
    // anyone.
    let never_ends = rings
        .barrier
        .admit(&rings.access, rings.tag(0), true)
        .expect("the first write is in");
    rings.retire(0);
    let fresh_tag = rings.tag(0);

    let refused = Arc::new(AtomicBool::new(false));
    let waiter = {
        let barrier = Arc::clone(&rings.barrier);
        let access = Arc::clone(&rings.access);
        let refused = Arc::clone(&refused);
        thread::spawn(move || {
            let admitted = barrier.admit(&access, fresh_tag, true).is_some();
            refused.store(!admitted, Ordering::Release);
        })
    };
    thread::sleep(LONG_ENOUGH_TO_OVERTAKE);
    assert!(!waiter.is_finished(), "the waiter was never held back");

    // The only wake-up in the test. The stale write is still running.
    rings.retire(0);

    let deadline = Instant::now() + HELD_BACK;
    while !waiter.is_finished() {
        assert!(
            Instant::now() < deadline,
            "a retire left a cancelled waiter asleep"
        );
        thread::sleep(Duration::from_millis(1));
    }
    waiter.join().expect("the waiter woke");
    assert!(
        refused.load(Ordering::Acquire),
        "work from a retired generation was admitted"
    );
    drop(never_ends);
}

// A waiter must give up when its own generation ends while it waits.
#[test]
fn a_waiter_gives_up_when_its_session_ends() {
    let rings = Rings::new(1);
    let stale = rings
        .barrier
        .admit(&rings.access, rings.tag(0), true)
        .expect("the first write is admitted");
    rings.retire(0);
    let fresh_tag = rings.tag(0);

    let refused = Arc::new(AtomicBool::new(false));
    let waiter = {
        let barrier = Arc::clone(&rings.barrier);
        let access = Arc::clone(&rings.access);
        let refused = Arc::clone(&refused);
        thread::spawn(move || {
            let admitted = barrier.admit(&access, fresh_tag, true).is_some();
            refused.store(!admitted, Ordering::Release);
        })
    };

    thread::sleep(LONG_ENOUGH_TO_OVERTAKE);
    // A second retirement while the waiter is still held back.
    rings.retire(0);
    drop(stale);
    waiter.join().expect("the waiter never woke");

    assert!(
        refused.load(Ordering::Acquire),
        "work from a retired generation was admitted after it woke"
    );
}
