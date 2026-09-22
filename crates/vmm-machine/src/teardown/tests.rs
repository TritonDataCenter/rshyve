// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What these tests establish, and what they do not.
//!
//! They drive the real teardown sequence over recording devices
//! and a destroy closure, so they show which step runs and in
//! which order. No test here opens /dev/vmm, so none of them shows
//! that the kernel releases the instance, that the process leaves
//! the process table, or that `VNA_IOC_DELETE` returns on a live
//! link. Only a lab test on illumos shows that: boot a VM with a
//! viona NIC, power it off from inside the guest, and watch for
//! the instance to leave `/dev/vmm` and the VMM process to exit
//! with no watchdog line behind it.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use slog::Logger;
use vmm_devices::quiesce::NamedDevice;
use vmm_devices::Lifecycle as _;

use super::{
    destroy_bounded_with, device_work, halt_bounded_with, halt_budget_for,
    halt_devices, join_vcpus_bounded, quiesce_devices, shutdown_after_exit,
    spawn_halt, teardown_budget, teardown_budget_for, HaltWork, RunOutcome,
    VcpuSet, VcpuThreads, DESTROY_BUDGET, FLUSH_BUDGET_PER_DEVICE,
    HALT_OVERRUN_SLACK, HALT_SPAWN_ATTEMPTS, QUIESCE_BUDGET, ROSTER_POLL,
    TEARDOWN_BUDGET_MAX, TEARDOWN_BUDGET_MIN, VCPU_JOIN_BUDGET,
};
use crate::vcpu_tasks::VcpuEvent;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use vmm_devices::lifecycle::DEFAULT_HALT_BUDGET;

struct TrackedDevice {
    indicator: vmm_devices::Indicator,
}

impl vmm_devices::Lifecycle for TrackedDevice {
    fn type_name(&self) -> &'static str {
        "tracked-test-device"
    }

    fn lifecycle_state(&self) -> Option<vmm_devices::IndicatedState> {
        Some(self.indicator.state())
    }

    fn start(&self) -> anyhow::Result<()> {
        self.indicator.start();
        Ok(())
    }

    fn pause(&self) {
        self.indicator.pause();
    }
}

struct NeverQuiesces;

impl vmm_devices::Lifecycle for NeverQuiesces {
    fn type_name(&self) -> &'static str {
        "never"
    }

    fn is_quiesced(&self) -> bool {
        false
    }

    fn flush_backing(
        &self,
        _intent: vmm_devices::FlushIntent,
    ) -> Result<(), vmm_devices::FlushError> {
        Err(vmm_devices::FlushError::NotQuiesced("never"))
    }
}

fn null_log() -> Logger {
    Logger::root(slog::Discard, slog::o!())
}

/// The teardown steps one run took, in order.
type Steps = Arc<Mutex<Vec<&'static str>>>;

fn push(steps: &Steps, step: &'static str) {
    steps.lock().expect("steps lock").push(step);
}

fn steps_of(steps: &Steps) -> Vec<&'static str> {
    steps.lock().expect("steps lock").clone()
}

/// A device that records which lifecycle calls teardown made.
struct RecordingDevice {
    indicator: vmm_devices::Indicator,
    steps: Steps,
    flush_fails: bool,
}

impl RecordingDevice {
    fn new(steps: &Steps) -> Self {
        Self {
            indicator: vmm_devices::Indicator::new(),
            steps: Arc::clone(steps),
            flush_fails: false,
        }
    }

    fn failing_flush(steps: &Steps) -> Self {
        Self {
            flush_fails: true,
            ..Self::new(steps)
        }
    }
}

impl vmm_devices::Lifecycle for RecordingDevice {
    fn type_name(&self) -> &'static str {
        "recording-test-device"
    }

    fn lifecycle_state(&self) -> Option<vmm_devices::IndicatedState> {
        Some(self.indicator.state())
    }

    fn pause(&self) {
        push(&self.steps, "pause");
        self.indicator.pause();
    }

    fn halt(&self) {
        push(&self.steps, "halt");
        self.indicator.halt();
    }

    fn is_quiesced(&self) -> bool {
        !self.flush_fails
    }

    fn flush_backing(
        &self,
        _intent: vmm_devices::FlushIntent,
    ) -> Result<(), vmm_devices::FlushError> {
        push(&self.steps, "flush");
        if self.flush_fails {
            return Err(vmm_devices::FlushError::NotQuiesced("recording"));
        }
        Ok(())
    }
}

/// Run one teardown over `devices` and report the steps it took.
///
/// The arm and the destroy are recorded in the same list as the
/// device calls, so the assertions are about one order and not
/// about three.
fn run_teardown(devices: &[NamedDevice], steps: &Steps) -> Option<Duration> {
    let armed = Mutex::new(None);
    let arm_steps = Arc::clone(steps);
    let destroy_steps = Arc::clone(steps);

    shutdown_after_exit(
        VcpuSet::Fixed(Vec::new()),
        devices,
        None,
        move || {
            push(&destroy_steps, "destroy");
            Ok(())
        },
        |budget| {
            push(&arm_steps, "arm");
            *armed.lock().expect("armed lock") = Some(budget);
        },
        &null_log(),
    );

    let budget = *armed.lock().expect("armed lock");
    budget
}

/// A device whose halt does not return until the test releases it,
/// standing in for one joining a thread that will not stop.
struct BlockingHalt {
    steps: Steps,
    release: Mutex<mpsc::Receiver<()>>,
    budget: Duration,
}

impl vmm_devices::Lifecycle for BlockingHalt {
    fn type_name(&self) -> &'static str {
        "blocking-halt-test-device"
    }

    fn halt_budget(&self) -> Duration {
        self.budget
    }

    fn halt(&self) {
        push(&self.steps, "blocked-halt");
        // The recv ends only when the test drops the sender, which
        // is how the stand-in halt is released.
        let _released =
            self.release.lock().expect("release lock").recv().is_ok();
    }
}

/// A device that declares a halt deadline and holds the sweep for
/// `waits` before its halt returns, the shape virtio-fs has.
struct DeclaredHalt {
    steps: Steps,
    name: &'static str,
    budget: Duration,
    waits: Duration,
}

impl vmm_devices::Lifecycle for DeclaredHalt {
    fn type_name(&self) -> &'static str {
        self.name
    }

    fn halt_budget(&self) -> Duration {
        self.budget
    }

    fn halt(&self) {
        thread::sleep(self.waits);
        // Recorded after the wait, so the step appears only for a
        // halt the sweep really waited out.
        push(&self.steps, self.name);
    }
}

/// A device whose halt reports which thread ran it.
///
/// The wait is what makes a halt run on the sweep thread visible as
/// elapsed time as well as as a thread id. It is short on purpose:
/// a test that hangs to show a missing bound proves nothing.
struct HaltThreadWitness {
    ran_on: Mutex<Option<thread::ThreadId>>,
    waits: Duration,
}

impl vmm_devices::Lifecycle for HaltThreadWitness {
    fn type_name(&self) -> &'static str {
        "halt-thread-witness-test-device"
    }

    fn halt_budget(&self) -> Duration {
        Duration::ZERO
    }

    fn halt(&self) {
        *self.ran_on.lock().expect("witness lock") =
            Some(thread::current().id());
        thread::sleep(self.waits);
    }
}

/// A device that declares a halt deadline and nothing else.
struct DeclaresBudget(Duration);

impl vmm_devices::Lifecycle for DeclaresBudget {
    fn type_name(&self) -> &'static str {
        "declares-budget-test-device"
    }

    fn halt_budget(&self) -> Duration {
        self.0
    }
}

/// `count` devices that each declare the default halt deadline.
fn default_devices(count: usize) -> Vec<NamedDevice> {
    (0..count)
        .map(|_| NamedDevice {
            id: None,
            device: Arc::new(DeclaresBudget(DEFAULT_HALT_BUDGET)),
        })
        .collect()
}

/// A device whose halt panics.
struct PanickingHalt {
    steps: Steps,
}

impl vmm_devices::Lifecycle for PanickingHalt {
    fn type_name(&self) -> &'static str {
        "panicking-halt-test-device"
    }

    fn halt(&self) {
        push(&self.steps, "panic-halt");
        panic!("a device halt that panics must not unwind the sweep");
    }
}

fn named(device: Arc<dyn vmm_devices::Lifecycle>) -> Vec<NamedDevice> {
    vec![NamedDevice {
        id: Some("net0".to_string()),
        device,
    }]
}

/// Name a device for a sweep that walks more than one.
fn one(id: &str, device: Arc<dyn vmm_devices::Lifecycle>) -> NamedDevice {
    NamedDevice {
        id: Some(id.to_string()),
        device,
    }
}

#[test]
fn teardown_halts_the_devices_before_it_destroys_the_vm() {
    // VM_DESTROY_SELF waits for every vmm_drv hold to be released,
    // in an untimed cv_wait. viona gives its hold back in halt, so
    // a halt after the destroy is a halt that never runs.
    let steps = Steps::default();
    let devices = named(Arc::new(RecordingDevice::new(&steps)));

    run_teardown(&devices, &steps);

    assert_eq!(
        steps_of(&steps),
        ["arm", "pause", "flush", "halt", "destroy"]
    );
}

#[test]
fn the_deadline_is_armed_before_the_first_step_that_cannot_be_stopped() {
    // The quiesce, the flush and the halt all run on this thread
    // and none can be interrupted. A guest that powers itself off
    // takes this same path with no signal behind it, so the arm is
    // the only thing that puts a clock on it.
    let steps = Steps::default();
    let devices = named(Arc::new(RecordingDevice::new(&steps)));

    let armed = run_teardown(&devices, &steps);

    assert_eq!(steps_of(&steps).first(), Some(&"arm"));
    assert_eq!(armed, Some(TEARDOWN_BUDGET_MIN));
}

#[test]
fn a_device_that_will_not_flush_is_still_halted_and_destroyed() {
    // Failing to give back kernel state must not itself keep the
    // instance alive, so nothing in the sweep is allowed to stop
    // teardown short of the destroy.
    let steps = Steps::default();
    let devices = named(Arc::new(RecordingDevice::failing_flush(&steps)));

    run_teardown(&devices, &steps);

    assert_eq!(
        steps_of(&steps),
        ["arm", "pause", "flush", "halt", "destroy"]
    );
}

#[test]
fn a_device_halted_before_teardown_is_not_halted_again() {
    // A hot-unplug halts the device it ejects. Halting it a second
    // time would repeat a wait that has nothing left to give back.
    let steps = Steps::default();
    let device = Arc::new(RecordingDevice::new(&steps));
    device.pause();
    device.halt();
    steps.lock().expect("steps lock").clear();
    let devices = named(device);

    run_teardown(&devices, &steps);

    let steps = steps_of(&steps);
    assert!(!steps.contains(&"halt"), "the halt ran twice: {steps:?}");
    assert_eq!(steps, ["arm", "pause", "flush", "destroy"]);
}

#[test]
fn a_halt_that_never_returns_does_not_stop_the_sweep() {
    // Several device halts join threads. One that does not return must
    // not cost the VM its destroy.
    let steps = Steps::default();
    let (release, blocked) = mpsc::channel::<()>();
    let devices = vec![
        one(
            "stuck",
            Arc::new(BlockingHalt {
                steps: Arc::clone(&steps),
                release: Mutex::new(blocked),
                budget: Duration::ZERO,
            }),
        ),
        one("net0", Arc::new(RecordingDevice::new(&steps))),
    ];
    let slack = Duration::from_millis(50);

    // The sweep runs on a thread of its own so that dropping the
    // per-device bound fails this test rather than wedging the
    // whole run. A test that can hang proves nothing.
    let (done_tx, done_rx) = mpsc::channel();
    let sweep = thread::spawn(move || {
        halt_devices(&devices, slack, &null_log());
        // A closed channel means the test already gave up.
        let _test_gave_up = done_tx.send(());
    });

    let finished = done_rx.recv_timeout(slack * 20).is_ok();
    drop(release);
    sweep.join().expect("the sweep thread should not panic");

    assert!(finished, "a stuck device halt held the sweep");
    assert_eq!(steps_of(&steps), ["blocked-halt", "halt"]);
}

#[test]
fn a_halt_the_sweep_cannot_start_never_runs_on_the_sweep_thread() {
    // Running it here would put an unbounded stop one step short of the
    // VM destroy: `VNA_IOC_DELETE` is untimed and heeds no signal. The
    // refusal is guest reachable, because every console client the guest
    // opens takes a reader thread.
    let witness = Arc::new(HaltThreadWitness {
        ran_on: Mutex::new(None),
        waits: Duration::from_millis(300),
    });
    let device: Arc<dyn vmm_devices::Lifecycle> = witness.clone();
    let sweep = thread::current().id();
    let refused = |_work: HaltWork| -> std::io::Result<()> {
        Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
    };
    let started = Instant::now();

    let returned = halt_bounded_with(
        &refused,
        &device,
        Duration::from_secs(30),
        Duration::ZERO,
        &null_log(),
    );
    let waited = started.elapsed();

    assert!(!returned, "a halt that never started did not return");
    let ran_on = *witness.ran_on.lock().expect("witness lock");
    assert_eq!(
        ran_on, None,
        "the halt ran on {ran_on:?}; the sweep thread is {sweep:?}",
    );
    assert!(
        waited < Duration::from_millis(200),
        "the sweep waited {waited:?} on a halt it could not start",
    );
}

#[test]
fn the_sweep_asks_again_for_a_halt_thread_the_system_refused() {
    // A refusal at teardown is usually transient: the console halt
    // that ran a moment ago gave its reader threads back. Giving up
    // on the first one would drop the halt that hands viona's
    // vmm_drv hold back.
    // A literal, not the constant this test pins. Counting the
    // refusals with the value under test passes any value.
    const REFUSALS: usize = 2;
    assert!(
        HALT_SPAWN_ATTEMPTS as usize > REFUSALS,
        "the sweep must ask more than {REFUSALS} times",
    );
    let steps = Steps::default();
    let device: Arc<dyn vmm_devices::Lifecycle> =
        Arc::new(RecordingDevice::new(&steps));
    let asked = AtomicUsize::new(0);
    let flaky = |work: HaltWork| -> std::io::Result<()> {
        if asked.fetch_add(1, Ordering::AcqRel) < REFUSALS {
            return Err(std::io::Error::from(std::io::ErrorKind::WouldBlock));
        }
        spawn_halt(work)
    };

    let returned = halt_bounded_with(
        &flaky,
        &device,
        Duration::from_secs(5),
        Duration::ZERO,
        &null_log(),
    );

    assert!(returned, "the sweep gave up before a thread was granted");
    assert_eq!(asked.load(Ordering::Acquire), REFUSALS + 1);
    assert_eq!(steps_of(&steps), ["halt"]);
}

#[test]
fn a_halt_that_panics_does_not_stop_the_sweep() {
    // This pins the unwinding build only. The release profile sets
    // panic = "abort", so there the panic ends the process and
    // VM_SET_AUTODESTRUCT reclaims the instance. The panic message
    // this test prints is expected.
    let steps = Steps::default();
    let devices = vec![
        one(
            "bad",
            Arc::new(PanickingHalt {
                steps: Arc::clone(&steps),
            }),
        ),
        one("net0", Arc::new(RecordingDevice::new(&steps))),
    ];

    run_teardown(&devices, &steps);

    assert_eq!(
        steps_of(&steps),
        ["arm", "pause", "flush", "panic-halt", "halt", "destroy"]
    );
}

#[test]
fn quiesce_skips_device_that_is_already_paused() {
    // Teardown after an operator pause must not pause again: the
    // device is already quiesced and pausing it twice would hide a
    // real double pause behind a satisfied request.
    let device = Arc::new(TrackedDevice {
        indicator: vmm_devices::Indicator::new(),
    });
    device.start().expect("test device should start");
    device.pause();
    let devices: Vec<Arc<dyn vmm_devices::Lifecycle>> = vec![device.clone()];

    let report = quiesce_devices(&devices, &null_log());

    assert!(report.is_empty());
    assert_eq!(
        device.lifecycle_state(),
        Some(vmm_devices::IndicatedState::Pause),
    );
}

#[test]
fn quiesce_reports_device_that_misses_deadline() {
    let devices: Vec<Arc<dyn vmm_devices::Lifecycle>> =
        vec![Arc::new(NeverQuiesces)];
    let started = Instant::now();

    let report = quiesce_devices(&devices, &null_log());

    assert_eq!(report.stuck, vec!["never"]);
    assert!(started.elapsed() <= QUIESCE_BUDGET * 2);
}

#[test]
fn join_vcpus_bounded_returns_true_with_no_threads() {
    assert!(join_vcpus_bounded(Vec::new(), Duration::from_secs(1)));
}

#[test]
fn vcpu_join_timeout_is_bounded() {
    // A wedged vCPU thread must not hold the bhyve instance open.
    let budget = Duration::from_millis(20);
    let handle = thread::spawn(move || thread::sleep(budget * 5));
    let started = Instant::now();

    let joined = join_vcpus_bounded(vec![handle], budget);

    assert!(!joined);
    assert!(started.elapsed() <= budget * 2);
}

#[test]
fn run_outcome_distinguishes_triple_fault_sources() {
    assert_ne!(RunOutcome::TripleFault(0), RunOutcome::TripleFault(1));
    assert_eq!(RunOutcome::Halt, RunOutcome::Halt);
}

/// A thread that runs until its channel is dropped, standing in
/// for a vCPU thread inside the guest.
fn parked_thread() -> (mpsc::Sender<()>, thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<()>();
    // The recv fails when the sender is dropped, which is how
    // the test stops the stand-in thread.
    let handle = thread::spawn(move || if rx.recv().is_ok() {});
    (tx, handle)
}

/// Wait for a roster to report `want` live threads, or give up.
fn wait_for_live(threads: &VcpuThreads, want: usize) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if threads.live() == want {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    false
}

#[test]
fn a_roster_grows_while_the_guest_runs() {
    // The boot set is fixed, but a CPU added later has a thread
    // that teardown still has to join.
    let threads = VcpuThreads::new();
    let (boot, boot_handle) = parked_thread();
    threads
        .push(boot_handle)
        .expect("a fresh roster accepts a thread");
    assert_eq!(threads.len(), 1);

    let (late, late_handle) = parked_thread();
    threads
        .push(late_handle)
        .expect("an open roster accepts a thread");
    assert_eq!(threads.len(), 2);

    drop(boot);
    drop(late);
    assert!(join_vcpus_bounded(threads.close(), Duration::from_secs(5)));
}

#[test]
fn a_closed_roster_hands_the_handle_back() {
    // A thread pushed after teardown took the handles would never
    // be joined, so the add has to be refused and undone.
    let threads = VcpuThreads::new();
    assert!(threads.close().is_empty());
    assert!(threads.is_closed());

    let (park, handle) = parked_thread();
    let refused = threads.push(handle).expect_err("a closed roster refuses");
    drop(park);
    refused.join().expect("the refused thread should not panic");
    assert!(threads.is_empty());
}

#[test]
fn a_roster_drops_the_threads_a_failed_add_left_behind() {
    // A rolled-back add leaves a finished handle. Without the
    // prune, a long-running VM collects one per refused add.
    let threads = VcpuThreads::new();
    let (park, handle) = parked_thread();
    drop(park);
    threads
        .push(handle)
        .expect("a fresh roster accepts a thread");
    assert!(wait_for_live(&threads, 0));

    let (still_parked, live_handle) = parked_thread();
    threads
        .push(live_handle)
        .expect("an open roster accepts a thread");

    assert_eq!(threads.len(), 1, "the finished handle was dropped");
    drop(still_parked);
    assert!(join_vcpus_bounded(threads.close(), Duration::from_secs(5)));
}

#[test]
fn a_fixed_set_stops_when_the_event_channel_closes() {
    let (tx, rx) = mpsc::channel::<VcpuEvent>();
    drop(tx);

    assert!(VcpuSet::Fixed(Vec::new()).next_event(&rx).is_none());
}

#[test]
fn a_roster_stops_when_no_thread_is_left() {
    // The registry holds a sender for the CPU that is not added
    // yet, so the channel never closes. The roster is what says
    // that every vCPU thread has gone.
    let threads = VcpuThreads::new();
    let (park, handle) = parked_thread();
    threads
        .push(handle)
        .expect("a fresh roster accepts a thread");
    let (_registry_sender, rx) = mpsc::channel::<VcpuEvent>();

    drop(park);
    assert!(wait_for_live(&threads, 0));
    let started = Instant::now();
    let event = VcpuSet::Roster(&threads).next_event(&rx);

    assert!(event.is_none());
    assert!(started.elapsed() <= ROSTER_POLL * 8);
}

#[test]
fn a_destroy_that_never_returns_does_not_hold_teardown_open() {
    // VM_DESTROY_SELF waits for every vmm_drv lease to break, in an
    // untimed cv_wait. Teardown must stop waiting on it, and must
    // not report the shutdown as complete.
    let (park, blocked) = mpsc::channel::<()>();
    let budget = Duration::from_millis(50);
    let started = Instant::now();

    let destroyed = destroy_bounded_with(
        move || {
            // The recv ends only when the test drops the sender,
            // which is how the stand-in call is released.
            blocked.recv().ok();
            Ok(())
        },
        budget,
        &null_log(),
    );

    assert!(!destroyed, "a destroy that never returned is not a success");
    assert!(started.elapsed() <= budget * 20);
    drop(park);
}

#[test]
fn a_destroy_that_returns_is_reported_either_way() {
    // The call came back, so teardown is over. An error from the
    // kernel is logged, not waited on again.
    assert!(destroy_bounded_with(|| Ok(()), QUIESCE_BUDGET, &null_log()));
    assert!(destroy_bounded_with(
        || Err(std::io::Error::from(std::io::ErrorKind::InvalidInput)),
        QUIESCE_BUDGET,
        &null_log(),
    ));
}

#[test]
fn the_teardown_budget_grows_with_the_device_count() {
    // The flush and the halt are one call per device each, so the
    // clock a 20-disk VM needs is not the clock a 10-disk VM
    // needs. Both counts sit between the floor and the ceiling,
    // where the sum is what decides the budget.
    let few = teardown_budget(&default_devices(10));
    let many = teardown_budget(&default_devices(20));

    assert!(few < many);
    assert_eq!(
        many - few,
        (FLUSH_BUDGET_PER_DEVICE + DEFAULT_HALT_BUDGET + HALT_OVERRUN_SLACK)
            * 10,
    );
}

#[test]
fn the_halt_budget_outlasts_the_slowest_device_halt() {
    // A device that polls its own deadline returns just after it,
    // not on it, and then does its last cleanup. A sweep bound at
    // the same value would abandon a halt one poll from returning,
    // and the state that halt gives back would stay held.
    //
    // Both sides of the comparison are the device's own value.
    for declared in [
        Duration::ZERO,
        DEFAULT_HALT_BUDGET,
        // A device that raises its own deadline to 30 s.
        Duration::from_secs(30),
    ] {
        let device = DeclaresBudget(declared);
        assert!(
            halt_budget_for(&device, HALT_OVERRUN_SLACK) > declared,
            "the sweep leaves {declared:?} no room to overrun",
        );
    }

    // The device supplies the value, so one that declares a deadline
    // no clock can hold must not hold the sweep for longer than the
    // whole teardown may claim.
    assert_eq!(
        halt_budget_for(&DeclaresBudget(Duration::MAX), HALT_OVERRUN_SLACK),
        TEARDOWN_BUDGET_MAX,
    );
}

#[test]
fn the_armed_clock_grows_with_the_halt_a_device_declares() {
    // The watchdog ends a teardown at the armed deadline. A device
    // that waits longer than the default has to widen that clock,
    // or the sweep is cut off inside a bound it was told to hold.
    let raised = Duration::from_secs(30);

    let widened = device_work(&[one("fs", Arc::new(DeclaresBudget(raised)))])
        - device_work(&default_devices(1));

    assert_eq!(widened, raised - DEFAULT_HALT_BUDGET);
}

#[test]
fn the_armed_clock_covers_the_sweep_it_bounds() {
    // Every step but the flush is bounded, so the clock the
    // watchdog holds must be at least the sum of those bounds.
    // A shorter one would end a teardown that was still working.
    let bounded_work = |devices: u32| {
        QUIESCE_BUDGET
            + (FLUSH_BUDGET_PER_DEVICE
                + DEFAULT_HALT_BUDGET
                + HALT_OVERRUN_SLACK)
                * devices
            + VCPU_JOIN_BUDGET
            + DESTROY_BUDGET
    };
    for devices in [0u32, 1, 8, 34] {
        assert!(
            teardown_budget(&default_devices(devices as usize))
                >= bounded_work(devices),
            "the clock is short of the work for {devices} devices",
        );
    }

    // Past that count the ceiling decides, so the watchdog can end
    // a sweep that is still working. The ceiling stays, because a
    // deadline that can never be reached arms nothing, and the
    // trade is stated at TEARDOWN_BUDGET_MAX.
    assert_eq!(teardown_budget(&default_devices(35)), TEARDOWN_BUDGET_MAX);
    assert!(bounded_work(35) > TEARDOWN_BUDGET_MAX);
}

#[test]
fn a_teardown_budget_stays_inside_its_bounds() {
    // Under the floor the terminal exit would cut off a slow flush. Over
    // the ceiling the deadline could never be reached, which disarms the
    // watchdog the budget exists to arm.
    assert_eq!(teardown_budget(&[]), TEARDOWN_BUDGET_MIN);
    // A device declares its own budget, so the sum has to saturate
    // rather than wrap on a value no clock can hold.
    assert_eq!(
        teardown_budget(&[one(
            "greedy",
            Arc::new(DeclaresBudget(Duration::MAX))
        )]),
        TEARDOWN_BUDGET_MAX,
    );
    assert_eq!(teardown_budget_for(Duration::MAX), TEARDOWN_BUDGET_MAX);
}

#[test]
fn the_sweep_waits_out_the_budget_a_device_declares() {
    // The bound is the deadline the device declares. A device that
    // declares a long one must not be abandoned mid-halt.
    let steps = Steps::default();
    let (release, blocked) = mpsc::channel::<()>();
    let devices = vec![
        one(
            "slow",
            Arc::new(DeclaredHalt {
                steps: Arc::clone(&steps),
                name: "slow",
                budget: Duration::from_millis(800),
                waits: Duration::from_millis(200),
            }),
        ),
        one(
            "stuck",
            Arc::new(BlockingHalt {
                steps: Arc::clone(&steps),
                release: Mutex::new(blocked),
                budget: Duration::ZERO,
            }),
        ),
    ];
    // The sweep runs on a thread of its own so that a bound which
    // stopped working fails this test rather than hanging the run.
    let (done_tx, done_rx) = mpsc::channel();
    let sweep = thread::spawn(move || {
        halt_devices(&devices, Duration::from_millis(20), &null_log());
        // A closed channel means the test already gave up.
        let _test_gave_up = done_tx.send(());
    });

    // The device that declared nothing gets the slack alone, so a
    // sweep holding every device to one large constant sits on the
    // blocked halt until this gives up.
    let finished = done_rx.recv_timeout(Duration::from_secs(3)).is_ok();
    let seen = steps_of(&steps);
    drop(release);
    sweep.join().expect("the sweep thread should not panic");

    assert!(finished, "the sweep sat on a halt that declared no budget");
    assert!(
        seen.contains(&"slow"),
        "the sweep abandoned a halt inside the deadline the device \
         declared: {seen:?}",
    );
}

#[test]
fn a_roster_returns_the_event_a_late_thread_sends() {
    let threads = VcpuThreads::new();
    let (park, handle) = parked_thread();
    threads
        .push(handle)
        .expect("a fresh roster accepts a thread");
    let (sender, rx) = mpsc::channel::<VcpuEvent>();

    sender
        .send(VcpuEvent::Error {
            vcpu_id: 3,
            error: "late vCPU".into(),
        })
        .expect("the test holds the receiver");
    let event = VcpuSet::Roster(&threads).next_event(&rx);

    assert!(matches!(event, Some(VcpuEvent::Error { vcpu_id: 3, .. })));
    drop(park);
    assert!(join_vcpus_bounded(threads.close(), Duration::from_secs(5)));
}

#[test]
fn a_halt_the_budget_ran_out_on_is_reported_as_not_returned() {
    // The sweep decides two things on this answer: whether to log the
    // overrun, and whether to read the device state afterwards. That
    // read is refused for a halt still running, because the halt still
    // holds whatever locks it took. A timeout reported as a return
    // sends the sweep down the path meant for a halt that finished.
    //
    // Mutation this kills: `Timeout => true`.
    let steps = Steps::default();
    let (release, blocked) = mpsc::channel::<()>();
    let device: Arc<dyn vmm_devices::Lifecycle> = Arc::new(BlockingHalt {
        steps: Arc::clone(&steps),
        release: Mutex::new(blocked),
        budget: Duration::ZERO,
    });

    // On its own thread, so a bound that stopped working fails this
    // test instead of parking the run.
    let (done_tx, done_rx) = mpsc::channel();
    let caller = thread::spawn(move || {
        let returned = halt_bounded_with(
            &spawn_halt,
            &device,
            Duration::from_millis(100),
            Duration::ZERO,
            &null_log(),
        );
        // A closed channel means the test already gave up.
        let _test_gave_up = done_tx.send(returned);
    });

    let returned = done_rx.recv_timeout(Duration::from_secs(3));
    drop(release);
    caller.join().expect("the caller thread should not panic");

    assert_eq!(
        returned,
        Ok(false),
        "a halt the budget ran out on was reported as returned",
    );
    assert_eq!(steps_of(&steps), vec!["blocked-halt"]);
}
