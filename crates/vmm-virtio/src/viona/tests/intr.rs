// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Poll thread delivery against a reset that ends the driver session.
//!
//! The viona reset releases the kernel rings but does not stop or drain
//! this thread. The thread can hold a notification from one driver while
//! the guest resets the device and the next driver arms its interrupts.
//! A delivery then signals a ring the new driver never armed: an INTx
//! level nothing lowers, or an MSI-X message to the vector that driver
//! just programmed.
//!
//! Both paths are tested because they fail differently, and because a
//! reset leaves every MSI-X vector at `NO_VECTOR`: silence for that
//! reason proves nothing. Each test ends with a delivery on the same
//! ring in the running session. A session check that refuses too much
//! drops live interrupts and hangs the guest.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use vmm_core::common::{RWOp, ReadOp, WriteOp};
use vmm_core::mem::PhysMap;
use vmm_core::mmio::MmioBus;
use vmm_core::pio::PioBus;
use vmm_devices::pci::device::PciDevice;
use vmm_devices::pci::msix::{MsiSink, MsixTable};
use vmm_devices::pci::{BarN, INTxPinID, IntrPin};

use super::device::device_with;
use super::{cleared, null_log, Park, ParkAt, Steps, TestLink};
use crate::bits;
use crate::pci::intr::IntrSlot;
use crate::pci::VirtioPciDevice;
use crate::viona::poll::deliver_pending;
use crate::viona::{
    VirtioViona, NET_CONFIG_SIZE, NET_NUM_QUEUES, NET_QUEUE_SIZE,
};
use viona_api::LinkOps;

/// A wait this long means a thread is stuck, not slow.
const WEDGED: Duration = Duration::from_secs(15);

/// Driver status just before DRIVER_OK.
const READY: u8 =
    bits::STATUS_ACKNOWLEDGE | bits::STATUS_DRIVER | bits::STATUS_FEATURES_OK;

/// The vector a driver maps the rx ring to.
const QUEUE_VECTOR: u16 = 1;

/// The rx ring, which the kernel reports pending.
const PENDING_RING: usize = 0;

/// The MSI-X message of that vector.
const MSG_ADDR: u64 = 0xFEE0_0000;
const MSG_DATA: u64 = 0x21;

/// An interrupt pin that counts the transport assertions.
struct TestPin {
    level: AtomicBool,
    asserts: AtomicUsize,
}

impl TestPin {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            level: AtomicBool::new(false),
            asserts: AtomicUsize::new(0),
        })
    }

    fn asserts(&self) -> usize {
        self.asserts.load(Ordering::Acquire)
    }
}

impl IntrPin for TestPin {
    fn assert(&self) {
        self.asserts.fetch_add(1, Ordering::AcqRel);
        self.level.store(true, Ordering::Release);
    }

    fn deassert(&self) {
        self.level.store(false, Ordering::Release);
    }

    fn is_asserted(&self) -> bool {
        self.level.load(Ordering::Acquire)
    }
}

/// A sink that records the MSI messages sent.
struct TestSink {
    sent: Mutex<Vec<(u64, u64)>>,
}

impl TestSink {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            sent: Mutex::new(Vec::new()),
        })
    }

    fn sent(&self) -> Vec<(u64, u64)> {
        self.sent.lock().expect("sink lock").clone()
    }
}

impl MsiSink for TestSink {
    fn send(&self, addr: u64, data: u64) {
        self.sent.lock().expect("sink lock").push((addr, data));
    }
}

/// Write a legacy BAR0 register as a guest does.
fn write_legacy_on(dev: &Dev, reg: u16, val: &[u8]) {
    let wo = WriteOp::from_buf(val);
    dev.bar_rw(BarN::BAR0, usize::from(reg), RWOp::Write(&wo));
}

type Dev = Arc<VirtioPciDevice<VirtioViona>>;

/// A viona device on a test pin, its poll link, and the interrupt
/// targets.
struct Fixture {
    dev: Dev,
    link: Arc<TestLink>,
    /// The kernel calls the device made, clears included.
    steps: Steps,
    intr: Arc<IntrSlot>,
    pin: Arc<TestPin>,
    /// Set when the driver in this fixture uses MSI-X.
    sink: Option<Arc<TestSink>>,
    arrived: mpsc::Receiver<()>,
    go: mpsc::Sender<()>,
}

impl Fixture {
    /// Build a device whose poll thread stops at `park_at`.
    ///
    /// `msix` selects the driver interrupt path. Each test runs on both.
    fn new(park_at: ParkAt, msix: bool) -> Self {
        let (arrived_tx, arrived_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel();
        let steps = Steps::default();
        let link = Arc::new(
            TestLink::new(&steps).pending(PENDING_RING).parking(Park {
                at: park_at,
                arrived: arrived_tx,
                go: go_rx,
            }),
        );

        let sink = msix.then(TestSink::new);
        let table = sink.as_ref().map(|sink| {
            let table = Arc::new(MsixTable::new(
                NET_NUM_QUEUES as u16 + 1,
                Arc::clone(sink) as Arc<dyn MsiSink>,
            ));
            table.set_enabled(true);
            // A programmed vector, so `fire` has a target.
            table.write_entry(QUEUE_VECTOR, MSG_ADDR, MSG_DATA);
            table
        });

        let device = device_with(Arc::clone(&link) as Arc<dyn LinkOps>);
        let intr = device.interrupt_slot();
        let pin = TestPin::new();
        let dev = VirtioPciDevice::new(
            device,
            bits::VIRTIO_DEV_TYPE_NET,
            NET_NUM_QUEUES,
            NET_QUEUE_SIZE,
            NET_CONFIG_SIZE,
            Some((INTxPinID::IntA, Arc::clone(&pin) as Arc<dyn IntrPin>)),
            Arc::new(PhysMap::new()),
            Arc::new(PioBus::new()),
            Arc::new(MmioBus::new()),
            table,
        );
        dev.device().install_interrupt(dev.backend_intr());

        Self {
            dev,
            link,
            steps,
            intr,
            pin,
            sink,
            arrived: arrived_rx,
            go: go_tx,
        }
    }

    fn write_legacy(&self, reg: u16, val: &[u8]) {
        write_legacy_on(&self.dev, reg, val);
    }

    fn write_status(&self, val: u8) {
        self.write_legacy(bits::LEGACY_REG_DEVICE_STATUS, &[val]);
    }

    /// Read and clear the ISR, as a legacy driver handler does.
    fn isr(&self) -> u8 {
        let mut ro = ReadOp::new(1);
        self.dev.bar_rw(
            BarN::BAR0,
            usize::from(bits::LEGACY_REG_ISR_STATUS),
            RWOp::Read(&mut ro),
        );
        ro.buf()[0]
    }

    /// Bring a driver up, as the guest does.
    ///
    /// This includes the MSI-X vector: a reset leaves every vector at
    /// `NO_VECTOR`, and silence for that reason would pass every
    /// assertion here without a session check.
    fn arm(&self) {
        self.write_status(READY | bits::STATUS_DRIVER_OK);
        if self.sink.is_some() {
            self.write_legacy(
                bits::LEGACY_REG_QUEUE_SELECT,
                &(PENDING_RING as u16).to_le_bytes(),
            );
            self.write_legacy(
                bits::LEGACY_REG_MSIX_QUEUE_VECTOR,
                &QUEUE_VECTOR.to_le_bytes(),
            );
        }
    }

    /// Run one poll wakeup on its own thread, as the poll thread does.
    fn spawn_poller(&self) -> std::thread::JoinHandle<()> {
        let link = Arc::clone(&self.link);
        let intr = Arc::clone(&self.intr);
        std::thread::Builder::new()
            .name("viona-intr-poll-stub".into())
            .spawn(move || {
                deliver_pending(link.as_ref(), &intr, &null_log());
            })
            .expect("the test can spawn a thread")
    }

    /// Deliver one wakeup on this thread, after the stop point is used.
    fn deliver_here(&self) {
        deliver_pending(self.link.as_ref(), &self.intr, &null_log());
    }

    fn wait_parked(&self) {
        self.arrived
            .recv_timeout(WEDGED)
            .expect("the poll thread reached its park");
    }

    fn release(&self) {
        self.go.send(()).expect("release the poll thread");
    }

    /// Interrupts that reached the guest, on either path.
    fn delivered(&self) -> usize {
        let messages = self.sink.as_ref().map_or(0, |sink| sink.sent().len());
        self.pin.asserts() + messages
    }
}

/// A full device reset occurs between the poll thread session read and
/// its raise. Nothing may reach the next driver, and the same ring must
/// still reach the running driver.
fn a_reset_refuses_the_notification_it_overtook(park_at: ParkAt, msix: bool) {
    let fix = Fixture::new(park_at, msix);
    fix.arm();

    let poller = fix.spawn_poller();
    fix.wait_parked();

    // The guest resets the device and the next driver arms its
    // interrupts while the poll thread is stopped. A reset that waited
    // for this thread would never return.
    fix.write_status(0);
    fix.arm();

    fix.release();
    poller.join().expect("the poll thread finished");

    assert_eq!(
        fix.delivered(),
        0,
        "a notification collected before the reset reached the next driver",
    );
    assert_eq!(fix.isr(), 0, "the old session set the ISR");
    assert!(
        !fix.pin.is_asserted(),
        "the old session left the line asserted",
    );

    // The same device, ring and vector. Only the session differs, so
    // this separates a refusal from a device that cannot interrupt.
    fix.deliver_here();
    assert_eq!(
        fix.delivered(),
        1,
        "the running session's own notification was dropped",
    );
    assert_eq!(
        cleared(&fix.steps),
        [PENDING_RING as u16, PENDING_RING as u16],
        "the kernel's pending interrupt was not cleared each wakeup",
    );
}

#[test]
fn a_reset_refuses_a_notification_parked_at_the_poll_on_intx() {
    a_reset_refuses_the_notification_it_overtook(ParkAt::Poll, false);
}

#[test]
fn a_reset_refuses_a_notification_parked_at_the_poll_on_msix() {
    a_reset_refuses_the_notification_it_overtook(ParkAt::Poll, true);
}

#[test]
fn a_reset_refuses_a_notification_parked_at_the_clear_on_intx() {
    a_reset_refuses_the_notification_it_overtook(ParkAt::Clear, false);
}

#[test]
fn a_reset_refuses_a_notification_parked_at_the_clear_on_msix() {
    a_reset_refuses_the_notification_it_overtook(ParkAt::Clear, true);
}

#[test]
fn a_notification_in_the_running_session_reaches_an_intx_driver() {
    // A session check that drops this interrupt stalls the ring.
    let fix = Fixture::new(ParkAt::Poll, false);
    fix.arm();
    fix.release();

    fix.deliver_here();

    assert_eq!(fix.pin.asserts(), 1, "the interrupt never reached the pin");
    assert!(fix.pin.is_asserted(), "the line was not left asserted");
    assert_eq!(
        fix.isr(),
        bits::ISR_QUEUE_INTR,
        "the driver's handler would read a zero ISR",
    );
}

#[test]
fn a_notification_in_the_running_session_reaches_an_msix_driver() {
    // The same on the modern guest path. MSI-X has no level, so the
    // message is the evidence.
    let fix = Fixture::new(ParkAt::Poll, true);
    fix.arm();
    fix.release();

    fix.deliver_here();

    assert_eq!(
        fix.sink.as_ref().expect("this fixture is on MSI-X").sent(),
        [(MSG_ADDR, MSG_DATA)],
        "the interrupt never reached the vector the driver programmed",
    );
    assert_eq!(
        fix.pin.asserts(),
        0,
        "an MSI-X driver was sent an INTx assertion it will never lower",
    );
}

/// How long a reset may take while a poll thread is stopped.
///
/// Not a latency limit. It separates "returned" from "waits for the
/// stopped thread".
const RESET_BOUND: Duration = Duration::from_secs(5);

/// The reset runs on its own thread and must finish while the poll
/// thread is still stopped.
///
/// This proves the stop point is before admission. `finish_reset`
/// waits for an admitted delivery, so a poller stopped after admission
/// holds the reset, and this test fails instead of hanging. A reset
/// never stops or drains the poll thread, so a reset that waited for it
/// would wait for a kernel poll with no deadline.
fn a_reset_completes_while_the_poll_thread_is_held(
    park_at: ParkAt,
    msix: bool,
) {
    let fix = Fixture::new(park_at, msix);
    fix.arm();

    let poller = fix.spawn_poller();
    fix.wait_parked();

    // A separate thread, as a guest vCPU is. Nothing releases the stop
    // point until it returns.
    let resetter = {
        let dev = Arc::clone(&fix.dev);
        std::thread::Builder::new()
            .name("guest-vcpu-stub".into())
            .spawn(move || {
                write_legacy_on(&dev, bits::LEGACY_REG_DEVICE_STATUS, &[0]);
            })
            .expect("the test can spawn a thread")
    };
    let deadline = std::time::Instant::now() + RESET_BOUND;
    while !resetter.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    let reset_returned = resetter.is_finished();

    // The next driver arms, vector included, before the poll thread
    // continues. A reset leaves every vector at NO_VECTOR, so arming
    // after would drop a new-session delivery in the routing, and the
    // MSI-X test would pass with the session check removed.
    fix.arm();

    // Released in all cases, so a wrong build fails instead of hanging.
    fix.release();
    poller.join().expect("the poll thread finished");
    resetter.join().expect("the reset finished");
    assert!(
        reset_returned,
        "the reset waited on a poll thread parked before admission",
    );

    assert_eq!(
        fix.delivered(),
        0,
        "a notification collected before the reset reached the next driver",
    );
    assert_eq!(fix.isr(), 0, "the old session set the ISR");
    assert!(!fix.pin.is_asserted(), "the old session left the line up");

    // The same device, ring and vector: only the session differs.
    // Without this, silence is not evidence of a session check.
    fix.deliver_here();
    assert_eq!(
        fix.delivered(),
        1,
        "the running session's own notification was dropped",
    );
    // A refused notification must still clear the kernel pending bit,
    // or viona signals it again and this thread spins.
    assert_eq!(
        cleared(&fix.steps),
        [PENDING_RING as u16, PENDING_RING as u16],
        "the kernel's pending interrupt was not cleared each wakeup",
    );
}

#[test]
fn a_reset_on_another_thread_finishes_over_a_parked_poller_on_intx() {
    a_reset_completes_while_the_poll_thread_is_held(ParkAt::Poll, false);
}

#[test]
fn a_reset_on_another_thread_finishes_over_a_parked_poller_on_msix() {
    a_reset_completes_while_the_poll_thread_is_held(ParkAt::Poll, true);
}

#[test]
fn a_reset_on_another_thread_finishes_over_a_poller_at_the_clear_on_intx() {
    a_reset_completes_while_the_poll_thread_is_held(ParkAt::Clear, false);
}

#[test]
fn a_reset_on_another_thread_finishes_over_a_poller_at_the_clear_on_msix() {
    a_reset_completes_while_the_poll_thread_is_held(ParkAt::Clear, true);
}
