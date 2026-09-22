// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Device tests: the halt, the guards that keep calls off a destroyed
//! link, and the migration calls.
//!
//! The device holds a recording link in the slot where illumos puts the
//! kernel handle, so these tests also run on the target.

use std::sync::{mpsc, Arc};
use std::time::Duration;

use vmm_core::mem::PhysMap;
use vmm_devices::Lifecycle;

use super::{null_log, steps_of, Hold, Op, Step, Steps, TestLink};
use crate::queue::VirtQueue;
use crate::viona::halt::Poller;
use crate::viona::{
    RingState, VirtioViona, ETHERADDRL, NET_NUM_QUEUES, NET_QUEUE_SIZE,
    VIRTIO_NET_F_MAC,
};
use crate::VirtioDevice;
use viona_api::LinkOps;

/// How long a test waits before it declares a thread stuck.
///
/// A timeout is a failure, never a pass. A loaded machine does not
/// reach it.
const STUCK: Duration = Duration::from_secs(15);

const TEST_MAC: [u8; ETHERADDRL] = [0x02, 0x08, 0x20, 0xaa, 0xbb, 0xcc];

impl TestLink {
    /// Record the calls `device` makes with its lock held.
    fn watch(&self, device: &Arc<VirtioViona>) {
        self.device
            .set(Arc::downgrade(device))
            .expect("the link watches one device");
    }

    fn locked_calls(&self) -> Vec<Step> {
        self.locked.lock().expect("locked lock").clone()
    }
}

/// A device whose kernel calls go to `link`.
///
/// Uses the constructor that illumos uses after it opens a link.
pub(super) fn device_with(link: Arc<dyn LinkOps>) -> VirtioViona {
    VirtioViona::with_link(link, TEST_MAC, VIRTIO_NET_F_MAC)
}

/// A device with negotiated features and every ring ready.
pub(super) fn running_device(link: Arc<dyn LinkOps>) -> VirtioViona {
    let device = device_with(link);
    {
        let mut inner = device.inner.lock().expect("viona lock");
        inner.negotiated_features = u64::from(VIRTIO_NET_F_MAC);
        inner.ring_state = [RingState::Ready; NET_NUM_QUEUES];
    }
    device
}

/// A queue with guest ring addresses.
pub(super) fn addressed_queue() -> VirtQueue {
    let mut queue = VirtQueue::new(NET_QUEUE_SIZE);
    queue.set_addr_modern(0x1_0000, 0x2_0000, 0x3_0000);
    queue
}

pub(super) fn notify(device: &VirtioViona, queue_idx: u16) {
    let mut queues = [
        VirtQueue::new(NET_QUEUE_SIZE),
        VirtQueue::new(NET_QUEUE_SIZE),
    ];
    let physmap = PhysMap::new();
    VirtioDevice::notify_queue(device, queue_idx, &mut queues, &physmap);
}

/// A poll thread that records when it stops.
///
/// A pipe close wakes it, as for the real thread. The sleep before the
/// record makes the order visible: a halt that drops the poller instead
/// of joining it reaches the assertion first.
fn recording_poller(steps: &Steps) -> Poller {
    use std::os::fd::AsRawFd;

    let (stop, wake) = std::io::pipe().expect("the test can open a pipe");
    let steps = Arc::clone(steps);
    let thread = std::thread::Builder::new()
        .name("poller-stub".into())
        .spawn(move || {
            let mut pfd = libc::pollfd {
                fd: stop.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `pfd` is one live `pollfd` and the count says one,
            // so poll reads and writes only that element. The thread owns
            // `stop`, so its descriptor stays open.
            while unsafe { libc::poll(&mut pfd, 1, 10_000) } < 0 {
                let err = std::io::Error::last_os_error();
                assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
            }
            std::thread::sleep(Duration::from_millis(200));
            steps.lock().expect("steps lock").push(Step::PollerStopped);
        })
        .expect("the test can spawn a thread");
    Poller { wake, thread }
}

#[test]
fn a_halt_issues_the_delete_that_frees_the_vmm_hold() {
    // VM_DESTROY_SELF waits for every vmm_drv hold in an untimed
    // cv_wait. VNA_IOC_DELETE releases the viona hold.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));

    device.halt();

    assert_eq!(
        steps_of(&steps),
        [Step::RingReset(0), Step::RingReset(1), Step::Delete],
    );
}

#[test]
fn a_halt_stops_the_poll_thread_before_it_deletes_the_link() {
    // The poll thread reads the viona descriptor and calls into the PCI
    // transport, so a concurrent delete leaves it a destroyed link. This
    // is the Propolis order, tested through the whole halt.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));
    device.inner.lock().expect("viona lock").poller =
        Some(recording_poller(&steps));

    device.halt();

    assert_eq!(
        steps_of(&steps),
        [
            Step::PollerStopped,
            Step::RingReset(0),
            Step::RingReset(1),
            Step::Delete,
        ],
    );
}

#[test]
fn a_halt_holds_no_device_lock_while_it_stops_the_link() {
    // A hot unplug halts a device the guest still uses
    // (`vmm_devices::lifecycle::prepare_unplug`). Every halt call is an
    // untimed kernel wait, and cfg_read, notify_queue, queue_addr_set
    // and reset take this lock. A vCPU must not wait for the teardown.
    let steps = Steps::default();
    let link = Arc::new(TestLink::new(&steps));
    let device = Arc::new(device_with(link.clone()));
    link.watch(&device);

    device.halt();

    assert_eq!(
        steps_of(&steps),
        [Step::RingReset(0), Step::RingReset(1), Step::Delete],
    );
    assert!(
        link.locked_calls().is_empty(),
        "the halt held the device lock through {:?}",
        link.locked_calls(),
    );
}

#[test]
fn a_config_read_does_not_wait_behind_the_link_delete() {
    // The same property from the guest side: a vCPU reads the device
    // config while the halt is in the delete.
    let steps = Steps::default();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let device =
        Arc::new(device_with(Arc::new(TestLink::new(&steps).holding(Hold {
            entered: entered_tx,
            release: release_rx,
        }))));

    let halting = {
        let device = Arc::clone(&device);
        std::thread::Builder::new()
            .name("halt".into())
            .spawn(move || device.halt())
            .expect("the test can spawn a thread")
    };
    entered_rx
        .recv_timeout(STUCK)
        .expect("the halt reached the delete");

    // The read runs on its own thread, so a lock held by the halt makes
    // the test fail instead of hang.
    let (read_tx, read_rx) = mpsc::channel();
    let reader = {
        let device = Arc::clone(&device);
        std::thread::Builder::new()
            .name("cfg-read".into())
            .spawn(move || {
                let mac = device.cfg_read(0, 4);
                read_tx.send(mac).expect("the test waits for the read");
            })
            .expect("the test can spawn a thread")
    };
    let observed = read_rx.recv_timeout(STUCK);

    release_tx.send(()).expect("the delete waits for this");
    halting.join().expect("the halting thread left");
    reader.join().expect("the reading thread left");

    assert_eq!(
        observed.expect("a config read waited out the link delete"),
        u32::from_le_bytes([0x02, 0x08, 0x20, 0xaa]),
    );
}

#[test]
fn a_running_device_kicks_the_ring_the_guest_named() {
    // A kick that always names ring 0 leaves the guest transmits in the
    // ring and stalls the link.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));

    notify(&device, 1);

    assert_eq!(steps_of(&steps), [Step::RingKick(1)]);
}

/// The guest picks this index, and `ring_kick` bounds it without trust
/// in the transport. Indexing `ring_state` past its end panics the vCPU
/// thread while it holds the device mutex, and every later `expect` on
/// that mutex panics too.
#[test]
fn a_kick_naming_a_queue_that_does_not_exist_is_refused() {
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));

    for idx in [NET_NUM_QUEUES as u16, 2, 3, 255, u16::MAX] {
        notify(&device, idx);
    }

    assert_eq!(
        steps_of(&steps),
        [],
        "a queue index the device does not have reached the link",
    );
}

/// A missing bound causes more than one panic: it poisons the mutex for
/// every later caller.
#[test]
fn a_kick_from_a_hostile_guest_leaves_the_device_usable() {
    let steps = Steps::default();
    let device = Arc::new(running_device(Arc::new(TestLink::new(&steps))));

    // A vCPU thread, as the transport calls in.
    let vcpu = Arc::clone(&device);
    std::thread::spawn(move || notify(&vcpu, u16::MAX))
        .join()
        .expect("a guest queue index must not panic the vCPU thread");

    assert!(
        device.inner.lock().is_ok(),
        "the device mutex was poisoned, so every other caller panics",
    );
    notify(&device, 1);
    assert_eq!(
        steps_of(&steps),
        [Step::RingKick(1)],
        "a real kick stopped working after the hostile one",
    );
}

/// The guest accepts features and kicks before it writes a ring
/// address. Viona starts a ring worker in `ring_init`, so an
/// unprogrammed ring has none, and the kick dereferences a NULL in the
/// mac layer.
///
/// The features guard does not cover it: the spec order has the guest
/// set FEATURES_OK first.
#[test]
fn a_kick_before_the_guest_programmed_a_ring_is_refused() {
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));

    VirtioDevice::set_features(&device, u64::from(VIRTIO_NET_F_MAC));
    for ring in 0..NET_NUM_QUEUES as u16 {
        notify(&device, ring);
    }

    assert_eq!(
        steps_of(&steps),
        [Step::SetFeatures(u64::from(VIRTIO_NET_F_MAC))],
        "a kick reached a ring the kernel has no worker for",
    );
}

/// The guard is per ring. A guest that programs one ring and kicks the
/// other reaches the same NULL.
#[test]
fn a_kick_reaches_only_the_ring_the_guest_programmed() {
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));
    VirtioDevice::set_features(&device, u64::from(VIRTIO_NET_F_MAC));
    VirtioDevice::queue_addr_set(&device, 0, &addressed_queue());
    steps.lock().expect("steps lock").clear();

    notify(&device, 0);
    notify(&device, 1);

    assert_eq!(steps_of(&steps), [Step::RingKick(0)]);
}

#[test]
fn a_halt_leaves_no_ring_a_kick_can_reach() {
    // The halt drops the device lock before it destroys the link, so a
    // vCPU can reach notify_queue during the delete. It must find no
    // ready ring.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));

    device.halt();
    steps.lock().expect("steps lock").clear();
    notify(&device, 1);

    let inner = device.inner.lock().expect("viona lock");
    assert_eq!(inner.ring_state, [RingState::Init; NET_NUM_QUEUES]);
    assert_eq!(inner.negotiated_features, 0);
    drop(inner);
    assert_eq!(
        steps_of(&steps),
        [],
        "a kick reached the link after the delete",
    );
}

#[test]
fn a_queue_address_write_programs_that_ring() {
    // The kernel worker reads the addresses the guest wrote. A wrong
    // ring or address points it at guest memory the driver never gave.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));

    VirtioDevice::queue_addr_set(&device, 1, &addressed_queue());

    assert_eq!(
        steps_of(&steps),
        [Step::RingInit {
            ring: 1,
            size: NET_QUEUE_SIZE,
            desc: 0x1_0000,
            avail: 0x2_0000,
            used: 0x3_0000,
        }],
    );
    assert_eq!(
        device.inner.lock().expect("viona lock").ring_state[1],
        RingState::Ready,
    );
}

/// A legacy driver retires a ring by writing `QUEUE_PFN = 0` and then
/// frees the pages. A kernel ring left running uses memory the guest
/// reused.
#[test]
fn retiring_a_ring_resets_it_in_the_kernel() {
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));

    // The result of `set_addr_legacy(0)`: no addresses.
    VirtioDevice::queue_addr_set(&device, 1, &VirtQueue::new(NET_QUEUE_SIZE));

    assert_eq!(
        steps_of(&steps),
        [Step::RingReset(1)],
        "the kernel kept the ring the driver retired",
    );
    assert_eq!(
        device.inner.lock().expect("viona lock").ring_state[1],
        RingState::Init,
        "a retired ring stayed ready, so the next kick would reach it",
    );
}

/// The kernel refuses `RING_INIT` unless the ring is reset, so a ring
/// reprogrammed without a device reset must be reset first.
#[test]
fn reprogramming_a_ring_resets_it_before_programming_again() {
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));

    VirtioDevice::queue_addr_set(&device, 1, &addressed_queue());

    assert_eq!(
        steps_of(&steps),
        [
            Step::RingReset(1),
            Step::RingInit {
                ring: 1,
                size: NET_QUEUE_SIZE,
                desc: 0x1_0000,
                avail: 0x2_0000,
                used: 0x3_0000,
            },
        ],
        "the ring was programmed over while the kernel still ran it",
    );
    assert_eq!(
        device.inner.lock().expect("viona lock").ring_state[1],
        RingState::Ready,
    );
}

#[test]
fn a_halted_device_programs_no_ring() {
    // The halt destroys the link with the device lock dropped, so a
    // guest write can arrive during the halt. A ring init then would
    // program a destroyed link and mark the ring ready for a kick.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));
    device.halt();
    steps.lock().expect("steps lock").clear();

    VirtioDevice::queue_addr_set(&device, 1, &addressed_queue());

    assert_eq!(
        steps_of(&steps),
        [],
        "a halted device programmed a ring in the kernel",
    );
    assert_eq!(
        device.inner.lock().expect("viona lock").ring_state,
        [RingState::Init; NET_NUM_QUEUES],
    );
}

#[test]
fn a_running_device_takes_a_feature_write() {
    // VERSION_1 selects the modern ring layout in viona.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));

    VirtioDevice::set_features(&device, u64::from(VIRTIO_NET_F_MAC));

    assert_eq!(
        steps_of(&steps),
        [Step::SetFeatures(u64::from(VIRTIO_NET_F_MAC))],
    );
}

#[test]
fn a_halted_device_takes_no_feature_write() {
    // `set_features` issues an ioctl under the device guard. A halt
    // dropped that guard and is in an untimed kernel call, so a vCPU
    // here would block in the kernel with the guard held, and the next
    // vCPU would block on the guard: a hot-unplug stall.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));
    device.halt();
    steps.lock().expect("steps lock").clear();

    VirtioDevice::set_features(&device, u64::from(VIRTIO_NET_F_MAC));

    assert_eq!(
        steps_of(&steps),
        [],
        "a halted device sent a feature write to the link",
    );
    assert_eq!(
        device.inner.lock().expect("viona lock").negotiated_features,
        0,
        "a halted device took a feature write",
    );
}

#[test]
fn a_second_halt_does_not_delete_again() {
    // The delete waits for each ring worker to stop. A second delete
    // repeats that wait for no purpose.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));

    device.halt();
    device.halt();

    assert_eq!(
        steps_of(&steps)
            .iter()
            .filter(|s| **s == Step::Delete)
            .count(),
        1
    );
}

#[test]
fn a_failed_delete_still_leaves_the_device_halted() {
    // Teardown continues to the destroy, so a failure must not leave
    // the device ready for a second halt.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::Delete]);
    let device = device_with(Arc::new(link));

    device.halt();
    device.halt();

    assert_eq!(
        steps_of(&steps)
            .iter()
            .filter(|s| **s == Step::Delete)
            .count(),
        1
    );
}

#[test]
fn a_halt_drops_a_deferred_poll() {
    // A migration restore can arrive after the halt. A deferred poll
    // thread started then would read a destroyed link.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));
    device.defer_intr_poll(null_log());

    device.halt();

    let inner = device.inner.lock().expect("viona lock");
    assert!(inner.halted);
    assert!(
        inner.deferred_poll.is_none(),
        "the halt left a deferred poll behind",
    );
}

#[test]
fn a_halted_device_refuses_a_deferred_poll() {
    // A deferred poll stored after the halt must leave nothing for
    // start_poll_deferred.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));
    device.halt();

    device.defer_intr_poll(null_log());

    assert!(
        device
            .inner
            .lock()
            .expect("viona lock")
            .deferred_poll
            .is_none(),
        "a halted device took a deferred poll",
    );
}

#[test]
fn a_reset_returns_every_ring_to_the_kernel() {
    // The guest writes 0 to the device status. No kick may reach a ring
    // until the driver programs it again, and the kernel worker must
    // stop before the guest reuses that memory.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));

    VirtioDevice::reset(&device);

    assert_eq!(steps_of(&steps), [Step::RingReset(0), Step::RingReset(1)]);
    let inner = device.inner.lock().expect("viona lock");
    assert_eq!(inner.negotiated_features, 0);
    assert_eq!(inner.ring_state, [RingState::Init; NET_NUM_QUEUES]);
}

#[test]
fn a_halted_device_resets_no_ring() {
    // As for the feature write: the halt holds no device lock during an
    // untimed kernel call, so a vCPU can get here. The halt already
    // reset the rings.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));
    device.halt();
    steps.lock().expect("steps lock").clear();

    VirtioDevice::reset(&device);

    assert_eq!(
        steps_of(&steps),
        [],
        "a halted device sent a ring reset to the link",
    );
}

#[test]
fn a_deferred_start_kicks_the_rings_a_restore_programmed() {
    // Setting the ring state sets the indices but does not always wake
    // the kernel worker. Without this kick a migrated guest gets no
    // traffic until it kicks a ring, which an idle rx ring never does.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));
    device.defer_intr_poll(null_log());

    device.start_poll_deferred();
    // The halt joins the poll thread, so the test leaves none running.
    device.halt();

    assert_eq!(
        steps_of(&steps)
            .iter()
            .take_while(|step| **step != Step::RingReset(0))
            .copied()
            .collect::<Vec<_>>(),
        [Step::RingKick(0), Step::RingKick(1)],
        "a deferred start left the restored rings unkicked",
    );
}

#[test]
fn a_deferred_start_kicks_no_ring_the_guest_has_not_programmed() {
    // The kernel refuses a kick to a ring with no worker. An older
    // viona dereferences a NULL in the mac layer.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));
    device.defer_intr_poll(null_log());

    device.start_poll_deferred();
    device.halt();

    assert_eq!(
        steps_of(&steps),
        [Step::RingReset(0), Step::RingReset(1), Step::Delete],
        "a deferred start kicked a ring that was never programmed",
    );
}

#[test]
fn a_halted_device_starts_no_deferred_poll() {
    // Two guards keep a migration restore off a destroyed link: the
    // halt clears the deferred start, and this refuses one left behind.
    // The test sets it by hand because the first guard blocks every
    // other path.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));
    device.halt();
    steps.lock().expect("steps lock").clear();
    device.inner.lock().expect("viona lock").deferred_poll = Some(null_log());

    device.start_poll_deferred();

    let inner = device.inner.lock().expect("viona lock");
    assert!(inner.poller.is_none(), "a halted device started a poller");
    assert!(
        inner.deferred_poll.is_none(),
        "the refusal left the deferred start in place",
    );
    assert_eq!(
        steps_of(&steps),
        [],
        "a halted device kicked a ring after the delete",
    );
}

#[test]
fn a_migration_source_reads_the_indices_the_kernel_reports() {
    // The destination resumes from these two. A swapped pair replays
    // descriptors the guest reclaimed, and the guest driver reports
    // "id N is not a head!".
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));

    assert_eq!(
        VirtioDevice::kernel_ring_indices(&device, 1).expect("reads"),
        Some((0x1112, 0x2223)),
    );
}

#[test]
fn a_used_cursor_of_zero_is_still_the_kernel_answer() {
    // Both cursors are wrapping u16 counters. An export that read zero
    // as "no value" would use the userspace cursors, which viona never
    // updates, and the destination would resume from an old point.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps).wrapped_used()));

    assert_eq!(
        VirtioDevice::kernel_ring_indices(&device, 1).expect("reads"),
        Some((0x1112, 0)),
    );
}

#[test]
fn a_halted_device_reads_no_ring_state() {
    // An export can run while the halt is in the delete, which holds no
    // device lock. An ioctl then would wait for that untimed kernel call
    // with the lock every vCPU needs.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));
    device.halt();
    steps.lock().expect("steps lock").clear();

    device
        .kernel_ring_state(1)
        .expect_err("a destroyed link has no state to read");

    assert_eq!(
        steps_of(&steps),
        [],
        "a ring state read reached the link after the delete",
    );
}

#[test]
fn a_migration_source_reads_the_state_of_the_ring_it_names() {
    // A read that always named ring 0 would give every ring the ring 0
    // indices.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));

    assert_eq!(
        device.kernel_ring_state(1).expect("reads"),
        (0x1112, 0x2223),
    );

    assert_eq!(steps_of(&steps), [Step::RingGetState(1)]);
}

#[test]
fn a_restore_refuses_indices_no_running_ring_can_have_left() {
    // The payload comes from an unauthenticated peer. More chains in
    // flight than the ring size makes viona treat consumed entries as
    // new and give the driver descriptors it reclaimed.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));

    VirtioDevice::restore_ring_state(
        &device,
        1,
        NET_QUEUE_SIZE,
        0x1_0000,
        0x2_0000,
        0x3_0000,
        0,
        900,
        0,
        0,
    )
    .expect_err("an impossible pair of indices must fail the restore");

    assert_eq!(
        steps_of(&steps),
        [],
        "a ring with more chains in flight than descriptors was programmed",
    );
}

#[test]
fn a_restore_takes_indices_that_wrapped() {
    // Both cursors are wrapping u16 counters. A check without wrapping
    // arithmetic would refuse a ring paused across a wrap, and the
    // migrated guest would lose the queue.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));

    VirtioDevice::restore_ring_state(
        &device,
        1,
        NET_QUEUE_SIZE,
        0x1_0000,
        0x2_0000,
        0x3_0000,
        3,
        u16::MAX - 5,
        0,
        0,
    )
    .expect("a ring whose indices wrapped is restored");

    assert!(
        steps_of(&steps)
            .iter()
            .any(|step| matches!(step, Step::RingSetState(_))),
        "a ring whose indices wrapped was refused",
    );
}

#[test]
fn a_restore_programs_the_ring_and_its_interrupt() {
    // Addresses and indices go in together, because ring_init sets the
    // indices to 0. Features come first: VERSION_1 selects the modern
    // ring layout for the addresses. The kick makes the kernel worker
    // process what the source left available.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));

    VirtioDevice::restore_ring_state(
        &device,
        1,
        NET_QUEUE_SIZE,
        0x1_0000,
        0x2_0000,
        0x3_0000,
        7,
        5,
        0xFEE0_0000,
        0x21,
    )
    .expect("a sane payload restores");

    assert_eq!(
        steps_of(&steps),
        [
            Step::SetFeatures(u64::from(VIRTIO_NET_F_MAC)),
            Step::RingSetState(viona_api::vioc_ring_state {
                vrs_index: 1,
                vrs_avail_idx: 7,
                vrs_used_idx: 5,
                vrs_qsize: NET_QUEUE_SIZE,
                vrs_qaddr_desc: 0x1_0000,
                vrs_qaddr_avail: 0x2_0000,
                vrs_qaddr_used: 0x3_0000,
            }),
            Step::RingSetMsi {
                ring: 1,
                addr: 0xFEE0_0000,
                msg: 0x21,
            },
            Step::RingKick(1),
        ],
    );
    assert_eq!(
        device.inner.lock().expect("viona lock").ring_state[1],
        RingState::Ready,
    );
}

#[test]
fn a_halted_device_restores_no_ring() {
    // A restore that races the halt must not program a destroyed link
    // or mark the ring ready. It returns an error.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));
    device.halt();
    steps.lock().expect("steps lock").clear();

    VirtioDevice::restore_ring_state(
        &device,
        1,
        NET_QUEUE_SIZE,
        0x1_0000,
        0x2_0000,
        0x3_0000,
        7,
        5,
        0,
        0,
    )
    .expect_err("a destroyed link restores nothing");

    assert_eq!(
        steps_of(&steps),
        [],
        "a halted device programmed a ring in the kernel",
    );
}

#[test]
fn an_export_pauses_every_ring_before_it_reads_them() {
    // A running ring moves both cursors during the read. A rollback
    // from an earlier read programs cursors the kernel already passed
    // and replays the descriptors between them.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));

    VirtioDevice::pause_rings_for_export(&device).expect("pauses");

    assert_eq!(
        steps_of(&steps),
        [
            Step::RingPause(0),
            Step::RingGetState(0),
            Step::RingPause(1),
            Step::RingGetState(1),
        ],
    );
}

#[test]
fn an_export_that_cannot_read_a_paused_ring_fails_the_migration() {
    // A paused ring needs a reset and a state write to restart, and no
    // state was saved. Neither the export nor the rollback may succeed.
    let steps = Steps::default();
    let device = running_device(Arc::new(
        TestLink::new(&steps).failing(&[Op::RingGetState]),
    ));

    VirtioDevice::pause_rings_for_export(&device)
        .expect_err("a ring with no saved state must fail the migration");
    VirtioDevice::resume_rings_after_migration(&device)
        .expect_err("a ring that cannot be restarted must be reported");
}

#[test]
fn a_rollback_puts_every_paused_ring_back_to_work() {
    // A paused ring is VRS_STOP, and viona returns EBUSY for a guest
    // kick to it. Only a reset and a state write restart it. Otherwise
    // the source guest resumes with a dead NIC.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));

    VirtioDevice::pause_rings_for_export(&device).expect("pauses");
    steps.lock().expect("steps").clear();

    VirtioDevice::resume_rings_after_migration(&device).expect("resumes");

    let seen = steps_of(&steps);
    assert!(
        seen.contains(&Step::RingReset(0))
            && seen.contains(&Step::RingReset(1)),
        "{seen:?}",
    );
    assert_eq!(
        seen.iter()
            .filter(|s| matches!(s, Step::RingSetState { .. }))
            .count(),
        2,
        "both rings get their saved state back: {seen:?}",
    );
}

#[test]
fn a_ring_that_will_not_pause_fails_the_migration() {
    // Otherwise the export reads indices that still move.
    let steps = Steps::default();
    let device = running_device(Arc::new(
        TestLink::new(&steps).failing(&[Op::RingPause]),
    ));

    VirtioDevice::pause_rings_for_export(&device)
        .expect_err("a ring that stays running must fail the migration");
}

#[test]
fn a_restore_stops_every_ring_before_it_programs_them() {
    // A running worker would read the ring while the restore changes its
    // addresses.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));

    VirtioDevice::reset_all_rings(&device).expect("stops every ring");

    assert_eq!(
        steps_of(&steps),
        [
            Step::RingPause(0),
            Step::RingReset(0),
            Step::RingPause(1),
            Step::RingReset(1),
        ],
    );
}

#[test]
fn a_halted_device_pauses_no_ring() {
    // An export can run while the halt is in the delete, which holds no
    // device lock. A pause then could wait for that untimed kernel call
    // with the lock every vCPU needs.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));
    device.halt();
    steps.lock().expect("steps lock").clear();

    VirtioDevice::pause_rings_for_export(&device)
        .expect("a halted device has nothing to pause");

    assert_eq!(
        steps_of(&steps),
        [],
        "a halted device paused a ring in the kernel",
    );
}

#[test]
fn a_halted_device_resets_no_ring_for_a_restore() {
    // The same window on the destination. The halt already reset every
    // ring and destroyed the link.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));
    device.halt();
    steps.lock().expect("steps lock").clear();

    VirtioDevice::reset_all_rings(&device)
        .expect("a halted device has nothing left to stop");

    assert_eq!(
        steps_of(&steps),
        [],
        "a halted device reset a ring in the kernel",
    );
}

#[test]
fn a_halted_device_sends_no_notification_address() {
    // The restore programs these one step before the guarded poll
    // start. The same window applies: the link is destroyed and the
    // ioctl would run under the device lock.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));
    device.halt();
    steps.lock().expect("steps lock").clear();

    VirtioDevice::set_notify_addrs(&device, 0x2000, 0xC000_0000);

    assert_eq!(
        steps_of(&steps),
        [],
        "a halted device gave the kernel a notification address",
    );
}

#[test]
fn a_halted_device_refuses_a_promiscuous_mode_change() {
    // The one guarded link call that returns an error. Success would
    // tell a zone with allow_mac_spoofing that its guest receives every
    // frame, on a destroyed link.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));
    device.halt();
    steps.lock().expect("steps lock").clear();

    device
        .set_promisc(true)
        .expect_err("a halted device has no link to set");

    assert_eq!(
        steps_of(&steps),
        [],
        "a halted device sent a promiscuous mode to the link",
    );
}

#[test]
fn a_notification_address_reaches_the_kernel() {
    // With these the kernel receives the guest kick with no vCPU exit to
    // userspace. A wrong port or address loses the kick and stalls the
    // ring.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));

    VirtioDevice::set_notify_addrs(&device, 0x2000, 0xC000_0000);

    assert_eq!(
        steps_of(&steps),
        [
            Step::SetNotifyIop(0x2000),
            Step::SetNotifyMmio {
                addr: 0xC000_0000,
                size: 0x1000,
            },
        ],
    );
}

#[test]
fn an_unprogrammed_notification_address_is_not_sent() {
    // A transport with no legacy BAR passes port 0, and one with no
    // modern BAR passes address 0. The kernel would treat either as a
    // real address.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));

    VirtioDevice::set_notify_addrs(&device, 0, 0);

    assert_eq!(steps_of(&steps), []);
}

#[test]
fn promiscuous_mode_follows_the_zone_config() {
    // A VM with allow_mac_spoofing needs every frame on the physical
    // link. A VM without it must not see other VMs' traffic.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));

    device.set_promisc(true).expect("the test link answers");
    device.set_promisc(false).expect("the test link answers");

    assert_eq!(
        steps_of(&steps),
        [
            Step::SetPromisc(viona_api::PromiscMode::All),
            Step::SetPromisc(viona_api::PromiscMode::None),
        ],
    );
}
