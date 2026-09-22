// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A device reset against the register writes and interrupts that race
//! it.
//!
//! The reset runs on the vCPU that wrote DEVICE_STATUS and returns with
//! the backend off the ring. A legacy driver never polls: illumos
//! reclaims the ring DMA directly after its one write. The transport
//! lock is released across the backend reset, so another vCPU can read
//! the device while it is half reset. These tests check that window:
//!
//! - A write decides whether to run while it holds the transport lock.
//! - A second vCPU does not see the reset finish early.
//! - No event from the old session reaches the next driver.

mod intr;

use super::*;

use vmm_core::common::RWOp;
use vmm_devices::pci::device::PciDevice;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::ThreadId;
use std::time::{Duration, Instant};
use vmm_core::common::{ReadOp, WriteOp};

/// A wait this long means a thread is wedged, not slow.
const WEDGED: Duration = Duration::from_secs(10);

/// Time given to a broken build to run ahead of a parked thread.
const SETTLE: Duration = Duration::from_millis(200);

/// A reset that takes longer than this waited for work of a size the
/// guest controls, not only for the work the transport admitted.
const RESET_BUDGET: Duration = Duration::from_secs(1);

/// Status a driver holds just before it sets DRIVER_OK.
const READY: u8 =
    bits::STATUS_ACKNOWLEDGE | bits::STATUS_DRIVER | bits::STATUS_FEATURES_OK;

/// What the transport asked of the backend while the drain ran.
#[derive(Default)]
struct BackendLog {
    draining: bool,
    during_drain: Vec<&'static str>,
}

/// A backend whose reset blocks until the test releases it. It models a
/// backend that must drain its guest-memory accesses.
struct DrainDevice {
    log: Mutex<BackendLog>,
    entered: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
    resets: AtomicUsize,
    /// The thread that ran the last reset.
    reset_thread: Mutex<Option<ThreadId>>,
    /// False while a reset runs, the window in which a worker can still
    /// write the ring.
    off_the_ring: AtomicBool,
}

impl DrainDevice {
    fn note(&self, call: &'static str) {
        let mut log = self.log.lock().expect("backend log poisoned");
        if log.draining {
            log.during_drain.push(call);
        }
    }

    fn resets(&self) -> usize {
        self.resets.load(Ordering::Acquire)
    }

    fn reset_thread(&self) -> Option<ThreadId> {
        *self.reset_thread.lock().expect("reset thread poisoned")
    }

    fn off_the_ring(&self) -> bool {
        self.off_the_ring.load(Ordering::Acquire)
    }
}

impl VirtioDevice for DrainDevice {
    fn device_features(&self) -> u64 {
        0xF | bits::VIRTIO_F_VERSION_1
    }

    fn set_features(&self, _features: u64) {
        self.note("set_features");
    }

    fn cfg_read(&self, _offset: u16, _len: u8) -> u32 {
        0
    }

    fn cfg_write(&self, _offset: u16, _val: u32, _len: u8) {
        self.note("cfg_write");
    }

    fn queue_addr_set(&self, _queue_idx: u16, _queue: &VirtQueue) {
        self.note("queue_addr_set");
    }

    fn process_queue(
        &self,
        _queue_idx: u16,
        _queue: &mut VirtQueue,
        _head: u16,
        _physmap: &PhysMap,
    ) -> u32 {
        self.note("process_queue");
        0
    }

    fn reset(&self) {
        self.resets.fetch_add(1, Ordering::AcqRel);
        *self.reset_thread.lock().expect("reset thread poisoned") =
            Some(std::thread::current().id());
        self.off_the_ring.store(false, Ordering::Release);
        self.log.lock().expect("backend log poisoned").draining = true;
        self.entered.send(()).expect("the test watches the drain");
        self.release
            .lock()
            .expect("drain release poisoned")
            .recv_timeout(WEDGED)
            .expect("the test never released the drain");
        self.log.lock().expect("backend log poisoned").draining = false;
        self.off_the_ring.store(true, Ordering::Release);
    }
}

impl Lifecycle for DrainDevice {
    fn type_name(&self) -> &'static str {
        "drain-device"
    }
}

type Dev = Arc<VirtioPciDevice<DrainDevice>>;

fn make_device() -> (Dev, mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let dev = VirtioPciDevice::new(
        DrainDevice {
            log: Mutex::default(),
            entered: entered_tx,
            release: Mutex::new(release_rx),
            resets: AtomicUsize::new(0),
            reset_thread: Mutex::new(None),
            off_the_ring: AtomicBool::new(true),
        },
        bits::VIRTIO_DEV_TYPE_BLOCK,
        1,
        256,
        8,
        None,
        Arc::new(PhysMap::new()),
        Arc::new(PioBus::new()),
        Arc::new(MmioBus::new()),
        None,
    );
    (dev, entered_rx, release_tx)
}

/// Build a one-shot park. The parked thread reports that it arrived,
/// then waits for the test to release it.
fn park() -> (
    Arc<dyn Fn() + Send + Sync>,
    mpsc::Receiver<()>,
    mpsc::Sender<()>,
) {
    let (arrived_tx, arrived_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel();
    let go_rx = Mutex::new(go_rx);
    let hook: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        arrived_tx.send(()).expect("the test watches this park");
        go_rx
            .lock()
            .expect("park release poisoned")
            .recv_timeout(WEDGED)
            .expect("the test never released this park");
    });
    (hook, arrived_rx, go_tx)
}

fn status(dev: &Dev) -> u8 {
    let mut ro = ReadOp::new(1);
    dev.bar_rw(
        BarN::BAR0,
        usize::from(bits::LEGACY_REG_DEVICE_STATUS),
        RWOp::Read(&mut ro),
    );
    ro.buf()[0]
}

/// Read and clear the ISR, the way a legacy driver's handler does.
fn isr(dev: &Dev) -> u8 {
    let mut ro = ReadOp::new(1);
    dev.bar_rw(
        BarN::BAR0,
        usize::from(bits::LEGACY_REG_ISR_STATUS),
        RWOp::Read(&mut ro),
    );
    ro.buf()[0]
}

fn write_status(dev: &Dev, val: u8) {
    let wo = WriteOp::from_buf(&[val]);
    dev.bar_rw(
        BarN::BAR0,
        usize::from(bits::LEGACY_REG_DEVICE_STATUS),
        RWOp::Write(&wo),
    );
}

fn modern_status(dev: &Dev) -> u8 {
    let mut ro = ReadOp::new(1);
    dev.bar_rw(
        BarN::BAR2,
        usize::from(bits::COMMON_CFG_DEVICE_STATUS),
        RWOp::Read(&mut ro),
    );
    ro.buf()[0]
}

fn write_legacy(dev: &Dev, reg: u16, val: u32) {
    let wo = WriteOp::from_buf(&val.to_le_bytes());
    dev.bar_rw(BarN::BAR0, usize::from(reg), RWOp::Write(&wo));
}

fn write_modern(dev: &Dev, offset: u16, val: u32) {
    let wo = WriteOp::from_buf(&val.to_le_bytes());
    dev.bar_rw(BarN::BAR2, usize::from(offset), RWOp::Write(&wo));
}

fn write_modern_status(dev: &Dev, val: u8) {
    write_modern(dev, bits::COMMON_CFG_DEVICE_STATUS, u32::from(val));
}

fn during_drain(dev: &Dev) -> Vec<&'static str> {
    dev.device
        .log
        .lock()
        .expect("backend log poisoned")
        .during_drain
        .clone()
}

/// Reset a device in one register write, with no poll, and check that
/// the write did the whole reset.
///
/// A watcher samples the device while the write runs, so the test fails
/// on a backend that never entered its reset. The watcher also releases
/// the backend, so a reset on the writing thread cannot deadlock.
fn assert_one_write_resets(
    write_status_reg: fn(&Dev, u8),
    read_status_reg: fn(&Dev) -> u8,
) {
    let (dev, entered, release) = make_device();
    write_status_reg(&dev, READY | bits::STATUS_DRIVER_OK);

    let watcher = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || {
            entered.recv_timeout(WEDGED).expect("the backend drained");
            let seen = (read_status_reg(&dev), dev.device.off_the_ring());
            release.send(()).expect("release the drain");
            seen
        })
    };

    // The driver: one write to DEVICE_STATUS and nothing else.
    write_status_reg(&dev, 0);

    assert_eq!(
        dev.device.reset_thread(),
        Some(std::thread::current().id()),
        "the backend reset ran off the vCPU that wrote DEVICE_STATUS"
    );
    assert!(
        dev.device.off_the_ring(),
        "the write returned while the backend could still reach the ring"
    );
    assert_eq!(
        read_status_reg(&dev),
        0,
        "the reset was not finished when the write returned"
    );
    assert_eq!(dev.device.resets(), 1);

    let (mid_status, mid_off) = watcher.join().expect("the watcher finished");
    assert_ne!(
        mid_status, 0,
        "a second vCPU read status 0 while the backend held the ring"
    );
    assert!(!mid_off, "the backend never entered its reset");
}

// The illumos guest depends on this. virtio_legacy_device_reset_locked()
// is one write to DEVICE_STATUS with no poll. virtio_shutdown() then
// empties the queues and reclaims the DMA memory. So the write must not
// return until the backend released every guest-memory access. This
// fails if the reset moves off the writing vCPU.
#[test]
fn one_legacy_status_write_resets_the_device() {
    assert_one_write_resets(write_status, status);
}

// A modern driver polls for 0, but the reset uses the same thread and
// the same order.
#[test]
fn one_modern_status_write_resets_the_device() {
    assert_one_write_resets(write_modern_status, modern_status);
}

// A write must test the reset latch while it holds the transport lock.
// Otherwise a reset can start between the test and the lock, and the
// write lands during the backend drain.
#[test]
fn a_write_tests_the_reset_latch_under_the_transport_lock() {
    let (dev, _entered, _release) = make_device();
    let (hook, arrived, go) = park();
    dev.park_write_after_latch_check(hook);

    let writer = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || write_status(&dev, READY))
    };
    arrived
        .recv_timeout(WEDGED)
        .expect("the write reached its latch check");

    // A reset needs this lock to close the latch. While the write holds
    // it, no reset can start between the check and the write.
    let locked_out = dev.virtio_state.try_lock().is_err();

    go.send(()).expect("release the write");
    writer.join().expect("the write finished");
    assert!(locked_out, "the latch check ran outside the transport lock");
    assert_eq!(status(&dev), READY);
}

// A check outside the lock allows this: a write passes the check, a
// reset starts, and the write runs while the backend drains.
#[test]
fn a_write_parked_at_the_latch_check_cannot_land_during_a_drain() {
    let (dev, entered, release) = make_device();
    write_status(&dev, READY);

    let (hook, arrived, go) = park();
    dev.park_write_after_latch_check(hook);

    let writer = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || {
            write_legacy(&dev, bits::LEGACY_REG_GUEST_FEATURES, 0xF)
        })
    };
    arrived
        .recv_timeout(WEDGED)
        .expect("the write reached its latch check");

    let resetter = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || write_status(&dev, 0))
    };
    // Only a build that splits the check from the write drains here.
    // Otherwise the reset waits for the lock that the parked write
    // holds.
    let drained_early = entered.recv_timeout(SETTLE).is_ok();

    go.send(()).expect("release the write");
    writer.join().expect("the write finished");
    entered.recv_timeout(WEDGED).expect("the backend drained");
    release.send(()).expect("release the drain");
    resetter.join().expect("the reset finished");

    assert_eq!(
        during_drain(&dev),
        Vec::<&str>::new(),
        "a register write reached the backend during the drain"
    );
    assert!(
        !drained_early,
        "a reset drained the backend while a write held the transport lock"
    );
}

// Every write path is shut on both transports while the backend
// drains. A write here would get a ring that the drain still writes.
#[test]
fn every_write_path_is_shut_while_the_backend_drains() {
    let (dev, entered, release) = make_device();
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);

    let resetter = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || write_status(&dev, 0))
    };
    entered.recv_timeout(WEDGED).expect("the backend drained");

    write_status(&dev, READY | bits::STATUS_DRIVER_OK);
    write_legacy(&dev, bits::LEGACY_REG_GUEST_FEATURES, 0xF);
    write_legacy(&dev, bits::LEGACY_REG_QUEUE_PFN, 1);
    write_modern_status(&dev, READY | bits::STATUS_DRIVER_OK);
    write_modern(&dev, bits::COMMON_CFG_DRIVER_FEATURE, 0xF);
    // Device-specific config, then the notify register.
    write_modern(&dev, 0x1000, 0xFF);
    write_modern(&dev, 0x2000, 0);

    assert_ne!(status(&dev), 0, "the guest saw the reset finish mid-drain");
    assert_eq!(
        status(&dev) & bits::STATUS_DRIVER_OK,
        0,
        "a re-init landed while the backend was still draining"
    );

    release.send(()).expect("release the drain");
    resetter.join().expect("the reset finished");

    assert_eq!(
        during_drain(&dev),
        Vec::<&str>::new(),
        "a register write reached the backend during the drain"
    );
    assert_eq!(status(&dev), 0);
}

// A reset requested during a drain is dropped. Running it after the
// first would drain a device that the guest already stopped.
#[test]
fn a_second_reset_during_a_drain_is_dropped() {
    let (dev, entered, release) = make_device();
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);

    let resetter = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || write_status(&dev, 0))
    };
    entered.recv_timeout(WEDGED).expect("the backend drained");
    // A second vCPU requests a reset.
    write_status(&dev, 0);

    release.send(()).expect("release the drain");
    resetter.join().expect("the reset finished");
    assert!(
        entered.recv_timeout(SETTLE).is_err(),
        "a second drain ran behind the first"
    );
    assert_eq!(dev.device.resets(), 1);

    // The next session can reset too, so the first reset opened the
    // latch again.
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);
    let resetter = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || write_status(&dev, 0))
    };
    entered
        .recv_timeout(WEDGED)
        .expect("the next session's reset reached the backend");
    release.send(()).expect("release the second drain");
    resetter.join().expect("the second reset finished");
    assert_eq!(dev.device.resets(), 2);
    assert_eq!(status(&dev), 0);
}

// A driver with no MSI-X reads only the ISR, so a refused queue must
// set it.
#[test]
fn a_refused_queue_tells_an_intx_driver() {
    let (dev, _entered, _release) = make_device();

    // No guest memory is mapped, so the ring is refused.
    write_legacy(&dev, bits::LEGACY_REG_QUEUE_PFN, 1);

    assert_ne!(
        status(&dev) & bits::STATUS_DEVICE_NEEDS_RESET,
        0,
        "the driver was never told the queue had stopped"
    );
    assert_ne!(
        isr(&dev) & bits::ISR_CFG_CHANGE,
        0,
        "the driver was never interrupted to read that status"
    );
}

// The refusal cannot hold the transport lock across its interrupt: an
// MSI-X write under that lock stalls every other register access. So a
// whole reset can run in the gap. The interrupt must not then reach a
// session that never saw the change, on a line the reset lowered.
#[test]
fn a_refusal_overtaken_by_a_reset_raises_no_interrupt() {
    let (dev, entered, release) = make_device();
    let (hook, arrived, go) = park();
    dev.park_before_config_interrupt(hook);

    let refuser = {
        let dev = Arc::clone(&dev);
        // No guest memory is mapped, so the ring is refused.
        std::thread::spawn(move || {
            write_legacy(&dev, bits::LEGACY_REG_QUEUE_PFN, 1)
        })
    };
    arrived
        .recv_timeout(WEDGED)
        .expect("the refusal reached its interrupt");

    let resetter = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || write_status(&dev, 0))
    };
    entered.recv_timeout(WEDGED).expect("the backend drained");
    release.send(()).expect("release the drain");
    resetter.join().expect("the reset finished");
    write_status(&dev, READY);

    go.send(()).expect("release the refusal");
    refuser.join().expect("the refusal finished");
    assert_eq!(
        isr(&dev),
        0,
        "a refusal from the old session interrupted the new one"
    );
    assert_eq!(
        status(&dev),
        READY,
        "the old session's refusal marked the new session's status"
    );
}

// A driver that probes an unused device writes 0 to a status that is
// already 0. `readable_status` must still report a non-zero value: a
// second vCPU that reads 0 here frees a ring the backend still holds.
#[test]
fn a_reset_from_status_zero_still_reads_non_zero_while_it_drains() {
    let (dev, entered, release) = make_device();
    assert_eq!(status(&dev), 0);

    let resetter = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || write_status(&dev, 0))
    };
    entered.recv_timeout(WEDGED).expect("the backend drained");

    assert_ne!(
        status(&dev),
        0,
        "a second vCPU saw the reset finish while the backend drained"
    );
    assert_ne!(
        modern_status(&dev),
        0,
        "the modern status read reported the reset finished"
    );

    release.send(()).expect("release the drain");
    resetter.join().expect("the reset finished");
    write_status(&dev, bits::STATUS_ACKNOWLEDGE);
    assert_eq!(status(&dev), bits::STATUS_ACKNOWLEDGE);
}
