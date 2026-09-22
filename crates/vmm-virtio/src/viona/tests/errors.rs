// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Device behaviour when the kernel refuses a call.
//!
//! Each kernel call has its own failure branch. These tests check the
//! next action, not the warning. For example, a ring marked ready after
//! a refused program sends the next kick to a kernel with no worker,
//! and a loop that stops at the first failure never reaches the delete.

use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use vmm_devices::Lifecycle;

use super::device::{addressed_queue, device_with, notify, running_device};
use super::{cleared, null_log, steps_of, Op, Step, Steps, TestLink};
use crate::pci::intr::{BackendIntr, IntrSlot};
use crate::viona::halt::halt_link;
use crate::viona::poll::{deliver_pending, viona_intr_poll_loop};
use crate::viona::{
    RingState, NET_NUM_QUEUES, NET_QUEUE_SIZE, VIRTIO_NET_F_MAC,
};
use crate::VirtioDevice;

/// A wait this long means a thread is stuck, not slow.
const WEDGED: Duration = Duration::from_secs(15);

/// The ring the restore tests program.
const RESTORED_RING: u16 = 1;

/// The guest addresses of that ring.
const DESC: u64 = 0x1_0000;
const AVAIL: u64 = 0x2_0000;
const USED: u64 = 0x3_0000;

/// The MSI-X message the migrated driver programmed for it.
const MSG_ADDR: u64 = 0xFEE0_0000;
const MSG_DATA: u32 = 0x21;

/// The state one restore programs.
fn restored_state() -> viona_api::vioc_ring_state {
    viona_api::vioc_ring_state {
        vrs_index: RESTORED_RING,
        vrs_avail_idx: 7,
        vrs_used_idx: 5,
        vrs_qsize: NET_QUEUE_SIZE,
        vrs_qaddr_desc: DESC,
        vrs_qaddr_avail: AVAIL,
        vrs_qaddr_used: USED,
    }
}

/// Restore one ring, as a migration destination does.
fn restore(
    device: &crate::viona::VirtioViona,
) -> Result<(), vmm_devices::lifecycle::DeviceStateError> {
    VirtioDevice::restore_ring_state(
        device,
        RESTORED_RING,
        NET_QUEUE_SIZE,
        DESC,
        AVAIL,
        USED,
        7,
        5,
        MSG_ADDR,
        MSG_DATA,
    )
}

/// An interrupt path that records the rings it raised.
fn wired_interrupt() -> (IntrSlot, Arc<Mutex<Vec<u16>>>) {
    let raised = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&raised);
    let slot = IntrSlot::new();
    slot.install(BackendIntr::detached(move |_session, ring| {
        sink.lock().expect("raised lock").push(ring);
    }));
    (slot, raised)
}

#[test]
fn a_ring_the_kernel_refused_to_program_is_not_ready() {
    // The ready flag lets the next kick through. A refused ring has no
    // worker: the kernel refuses the kick, or an older viona
    // dereferences a NULL in the mac layer. Userspace validated these
    // addresses, so a second write cannot help. The ring stays
    // quarantined until a device reset.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::RingInit]);
    let device = device_with(Arc::new(link));

    VirtioDevice::queue_addr_set(&device, 1, &addressed_queue());
    VirtioDevice::queue_addr_set(&device, 1, &addressed_queue());

    assert_eq!(
        device.inner.lock().expect("viona lock").ring_state,
        [RingState::Init, RingState::Error],
        "a ring the kernel refused was marked ready",
    );
    assert_eq!(
        steps_of(&steps).len(),
        1,
        "a quarantined ring kept issuing ioctls, one per guest write",
    );
}

/// `viona_ioc_ring_reset` waits interruptibly, so `EINTR` means only
/// that this thread took a signal. Treating it as a refusal would
/// quarantine a working NIC and leave the ring unprogrammed.
#[test]
fn a_reset_a_signal_interrupted_is_tried_again() {
    let steps = Steps::default();
    let link = TestLink::new(&steps).interrupted_on(Op::RingReset, 2);
    let device = running_device(Arc::new(link));

    VirtioDevice::queue_addr_set(&device, RESTORED_RING, &addressed_queue());

    assert_eq!(
        steps_of(&steps),
        [
            Step::RingReset(RESTORED_RING),
            Step::RingReset(RESTORED_RING),
            Step::RingReset(RESTORED_RING),
            Step::RingInit {
                ring: RESTORED_RING,
                size: NET_QUEUE_SIZE,
                desc: DESC,
                avail: AVAIL,
                used: USED,
            },
        ],
        "the reset gave up on a signal",
    );
    assert_eq!(
        device.inner.lock().expect("viona lock").ring_state
            [RESTORED_RING as usize],
        RingState::Ready,
    );
}

/// A device reset retries `EINTR`. Destroying the link for a signal
/// would cost a working guest its NIC for the life of the VM.
#[test]
fn a_device_reset_a_signal_interrupted_keeps_the_link() {
    let steps = Steps::default();
    let link = TestLink::new(&steps).interrupted_on(Op::RingReset, 2);
    let device = running_device(Arc::new(link));

    VirtioDevice::reset(&device);

    assert!(
        !steps_of(&steps).contains(&Step::Delete),
        "a signal cost the guest its link",
    );
    let inner = device.inner.lock().expect("viona lock");
    assert!(!inner.halted);
    assert_eq!(inner.ring_state, [RingState::Init; NET_NUM_QUEUES]);
}

/// A `DEVICE_STATUS` write of 0 publishes status 0 when the backend
/// returns, and an illumos driver then frees the ring pages at once. A
/// kernel ring the reset did not stop keeps using them. Only
/// `VNA_IOC_DELETE` stops it: it resets every ring in a wait that
/// ignores signals and cannot fail once it starts. A quarantine would
/// tell the guest its pages are free while the kernel still uses them.
#[test]
fn a_device_reset_the_kernel_refused_destroys_the_link() {
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::RingReset]);
    let device = running_device(Arc::new(link));

    VirtioDevice::reset(&device);

    assert_eq!(
        steps_of(&steps).last(),
        Some(&Step::Delete),
        "the reset published completion without stopping the kernel",
    );
    let inner = device.inner.lock().expect("viona lock");
    assert!(inner.halted, "the link was destroyed but the device ran on");
    assert_eq!(
        inner.ring_state,
        [RingState::Init; NET_NUM_QUEUES],
        "a destroyed link kept a ring the next kick could reach",
    );
    drop(inner);

    // Nothing reaches the kernel after the destroy.
    steps.lock().expect("steps lock").clear();
    VirtioDevice::queue_addr_set(&device, 1, &addressed_queue());
    notify(&device, 1);
    assert_eq!(
        steps_of(&steps),
        [],
        "a destroyed link was programmed or kicked again",
    );
}

/// The same hazard for a single ring. A legacy driver retires a ring by
/// writing `QUEUE_PFN = 0` and then frees the ring DMA, so a ring the
/// kernel refused to reset takes the link with it.
#[test]
fn a_retire_the_kernel_refused_destroys_the_link() {
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::RingReset]);
    let device = running_device(Arc::new(link));

    VirtioDevice::queue_addr_set(&device, 1, &addressed_queue());

    assert_eq!(
        steps_of(&steps).last(),
        Some(&Step::Delete),
        "the kernel kept a ring whose pages the guest reclaims",
    );
    assert!(
        device.inner.lock().expect("viona lock").halted,
        "the link was destroyed but the device ran on",
    );
}

/// The kernel refuses a kick to a ring it does not run, and a guest can
/// spin on `QUEUE_NOTIFY`. The latch limits the log to one line per run
/// of refusals.
///
/// The ring stays in service: a migration export pauses the kernel
/// rings but leaves them `Ready`, so a ring retired on a refused kick
/// would leave a dead NIC after an aborted export.
#[test]
fn refused_kicks_warn_once_and_leave_the_ring_in_service() {
    const KICKS: usize = 8;
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::RingKick]);
    let device = running_device(Arc::new(link));

    for _ in 0..KICKS {
        notify(&device, 1);
    }

    let inner = device.inner.lock().expect("viona lock");
    assert!(
        inner.kick_refused[1],
        "the latch is open, so every later refusal writes its own line",
    );
    assert_eq!(
        inner.ring_state[1],
        RingState::Ready,
        "a refused kick took the ring out of service",
    );
    drop(inner);
    assert_eq!(steps_of(&steps).len(), KICKS);
}

#[test]
fn a_ring_state_the_kernel_refused_programs_nothing_after_it() {
    // The restore stops at the first refusal and reports it. Otherwise
    // it would mark the ring ready, route an MSI-X message to a ring the
    // kernel does not have, and kick a worker that never started.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::RingSetState]);
    let device = device_with(Arc::new(link));
    device.inner.lock().expect("viona lock").negotiated_features =
        u64::from(VIRTIO_NET_F_MAC);

    restore(&device).expect_err("a refused ring state must fail the restore");

    assert_eq!(
        steps_of(&steps),
        [
            Step::SetFeatures(u64::from(VIRTIO_NET_F_MAC)),
            Step::RingSetState(restored_state()),
        ],
        "the restore carried on past a ring the kernel refused",
    );
    assert_eq!(
        device.inner.lock().expect("viona lock").ring_state,
        [RingState::Init; NET_NUM_QUEUES],
        "a ring the kernel refused was marked ready",
    );
}

#[test]
fn a_message_the_kernel_refused_fails_the_restore() {
    // A live device can use the poll thread for this ring interrupt, so
    // there a refused message is a warning. During a migration the
    // source still runs, so the restore fails and the migration returns
    // to the source instead of giving the guest a queue with no signal.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::RingSetMsi]);
    let device = running_device(Arc::new(link));

    restore(&device).expect_err("a refused message must fail the restore");

    assert_eq!(
        steps_of(&steps).last(),
        Some(&Step::RingSetMsi {
            ring: RESTORED_RING,
            addr: MSG_ADDR,
            msg: u64::from(MSG_DATA),
        }),
        "the restore carried on past a message the kernel refused",
    );
}

#[test]
fn features_the_kernel_refused_fail_the_restore() {
    // VERSION_1 selects the modern ring layout for the addresses.
    // Without it the kernel reads the wrong guest memory.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::SetFeatures]);
    let device = running_device(Arc::new(link));

    restore(&device).expect_err("refused features must fail the restore");

    assert_eq!(
        steps_of(&steps),
        [Step::SetFeatures(u64::from(VIRTIO_NET_F_MAC))],
        "the restore programmed a ring after the features were refused",
    );
}

#[test]
fn a_ring_the_kernel_would_not_stop_fails_a_restore() {
    // A running worker reads a ring whose addresses the restore
    // changes, so the refusal fails the migration.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::RingPause]);
    let device = running_device(Arc::new(link));

    VirtioDevice::reset_all_rings(&device)
        .expect_err("a ring that will not stop must fail the restore");
}

#[test]
fn a_halt_deletes_the_link_though_every_ring_reset_fails() {
    // The delete releases the vmm_drv hold. A loop that stopped at the
    // first refused reset would leave VM_DESTROY_SELF waiting for that
    // hold in an untimed cv_wait.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::RingReset]);

    halt_link(&link, &null_log())
        .expect("the delete stops the rings the resets could not");

    assert_eq!(
        steps_of(&steps),
        [Step::RingReset(0), Step::RingReset(1), Step::Delete],
    );
}

#[test]
fn a_ring_state_read_the_kernel_refused_fails_the_export() {
    // Only the kernel has the indices: viona never updates the
    // userspace pair. An invented pair would resume the destination
    // from a point the ring passed.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::RingGetState]);
    let device = device_with(Arc::new(link));

    device
        .kernel_ring_state(1)
        .expect_err("the kernel refused the read");
    VirtioDevice::kernel_ring_indices(&device, 1)
        .expect_err("a refused read must fail the export");
}

#[test]
fn a_promiscuous_mode_the_kernel_refused_reaches_the_caller() {
    // The zone asked for every frame on the physical link. A false
    // success would leave a guest with allow_mac_spoofing without its
    // traffic and with no error.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::SetPromisc]);
    let device = device_with(Arc::new(link));

    device
        .set_promisc(true)
        .expect_err("the kernel refused the mode");
}

#[test]
fn a_notify_port_the_kernel_refused_does_not_stop_the_window() {
    // The two are independent, and a modern guest kicks through the
    // memory window. Stopping at the first refusal would lose the
    // in-kernel kick on the path that still works.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::SetNotifyIop]);
    let device = device_with(Arc::new(link));

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
fn a_deferred_start_kicks_every_ring_though_a_kick_fails() {
    // A refused kick on one ring must not skip the other. On a migrated
    // guest an unkicked ring delivers nothing until the guest kicks it,
    // which an idle rx ring never does.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::RingKick]);
    let device = running_device(Arc::new(link));
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
    );
}

#[test]
fn a_status_the_kernel_refused_raises_nothing() {
    // The status tells the poll thread which ring the kernel signalled.
    // Without it, a raise would signal a ring with no completion. On
    // INTx that is a level the driver handler cannot clear.
    let steps = Steps::default();
    let link = TestLink::new(&steps).pending(0).failing(&[Op::IntrStatus]);
    let (slot, raised) = wired_interrupt();

    deliver_pending(&link, &slot, &null_log());

    assert!(
        raised.lock().expect("raised lock").is_empty(),
        "a wakeup with no status raised an interrupt",
    );
    assert_eq!(
        steps_of(&steps),
        [],
        "a wakeup with no status cleared a ring",
    );
}

#[test]
fn an_interrupt_the_kernel_would_not_clear_is_still_raised() {
    // The clear stops viona from signalling the same interrupt again.
    // The raise is the completion the guest waits for. A refused clear
    // costs a spin. A dropped raise would cost the guest its I/O.
    let steps = Steps::default();
    let link = TestLink::new(&steps).pending(0).failing(&[Op::RingIntrClr]);
    let (slot, raised) = wired_interrupt();

    deliver_pending(&link, &slot, &null_log());

    assert_eq!(cleared(&steps), [0], "the clear was never tried");
    assert_eq!(
        *raised.lock().expect("raised lock"),
        [0],
        "a refused clear dropped the guest's interrupt",
    );
}

#[test]
fn a_wait_the_kernel_refused_ends_the_poll_thread() {
    // A wait that returns an error returns it again at once. A loop
    // that continued would spin on one core, and the halt would join a
    // thread that never exits.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::WaitIntr]);
    let (slot, raised) = wired_interrupt();
    let (stop, _wake) = std::io::pipe().expect("the test can open a pipe");

    // On its own thread, so a spinning loop fails the test instead of
    // hanging it.
    let (done, left) = mpsc::channel();
    std::thread::Builder::new()
        .name("poll-loop".into())
        .spawn(move || {
            viona_intr_poll_loop(&link, stop, &slot, null_log());
            done.send(()).expect("the test waits for this");
        })
        .expect("the test can spawn a thread");

    left.recv_timeout(WEDGED)
        .expect("the poll thread left after the wait failed");
    assert!(
        raised.lock().expect("raised lock").is_empty(),
        "a failed wait delivered an interrupt",
    );
}
