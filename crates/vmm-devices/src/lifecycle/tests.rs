// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::*;

use std::sync::atomic::AtomicBool;

#[test]
fn valid_transitions() {
    let ind = Indicator::new();
    assert_eq!(ind.state(), IndicatedState::Init);

    ind.start();
    assert_eq!(ind.state(), IndicatedState::Run);

    ind.pause();
    assert_eq!(ind.state(), IndicatedState::Pause);

    ind.resume();
    assert_eq!(ind.state(), IndicatedState::Run);

    ind.pause();
    ind.halt();
    assert_eq!(ind.state(), IndicatedState::Halt);
}

#[test]
fn init_to_pause_for_migration() {
    let ind = Indicator::new();
    ind.pause(); // Init -> Pause is valid (migration target)
    assert_eq!(ind.state(), IndicatedState::Pause);
}

#[test]
fn init_to_halt_is_refused() {
    // A device must be quiesced before it is halted, so the machine has
    // no edge here. The refusal leaves the state where it was.
    let ind = Indicator::new();

    let refused = ind
        .request(IndicatedState::Halt)
        .expect_err("Init -> Halt is not a legal edge");

    assert_eq!(refused.from, IndicatedState::Init);
    assert_eq!(ind.state(), IndicatedState::Init);
}

#[test]
fn run_to_halt_is_refused() {
    let ind = Indicator::new();
    ind.start();

    let refused = ind
        .request(IndicatedState::Halt)
        .expect_err("Run -> Halt is not a legal edge");

    assert_eq!(refused.from, IndicatedState::Run);
    assert_eq!(ind.state(), IndicatedState::Run);
}

#[test]
fn request_reports_whether_it_took_the_edge() {
    let ind = Indicator::new();
    ind.start();

    assert_eq!(ind.request(IndicatedState::Pause), Ok(true));
    assert_eq!(ind.request(IndicatedState::Pause), Ok(false));
}

const ALL_STATES: [IndicatedState; 4] = [
    IndicatedState::Init,
    IndicatedState::Run,
    IndicatedState::Pause,
    IndicatedState::Halt,
];

fn indicator_in(state: IndicatedState) -> Indicator {
    let ind = Indicator::new();
    match state {
        IndicatedState::Init => {}
        IndicatedState::Run => ind.start(),
        IndicatedState::Pause => ind.pause(),
        IndicatedState::Halt => {
            ind.pause();
            ind.halt();
        }
    }
    assert_eq!(ind.state(), state);
    ind
}

#[test]
fn try_transition_rejects_every_illegal_edge() {
    for from in ALL_STATES {
        for to in ALL_STATES {
            let ind = indicator_in(from);
            let result = ind.try_transition(to);

            if IndicatedState::valid_transition(from, to) {
                assert!(result.is_ok(), "{from:?} -> {to:?}");
                assert_eq!(ind.state(), to);
            } else {
                assert_eq!(
                    result.expect_err("illegal edge must not be taken"),
                    InvalidTransition { from, to },
                );
                assert_eq!(ind.state(), from, "{from:?} -> {to:?}");
            }
        }
    }
}

#[test]
fn try_transition_reports_run_to_halt_without_panicking() {
    let ind = indicator_in(IndicatedState::Run);

    let error = ind
        .try_transition(IndicatedState::Halt)
        .expect_err("Run -> Halt is not a legal edge");

    assert_eq!(error.from, IndicatedState::Run);
    assert_eq!(error.to, IndicatedState::Halt);
    assert_eq!(ind.state(), IndicatedState::Run);
}

struct UnplugTestDevice {
    indicator: Indicator,
    quiesced: AtomicBool,
    pauses: AtomicUsize,
    halts: AtomicUsize,
    flushes: AtomicUsize,
    flush_fails: bool,
}

impl UnplugTestDevice {
    fn new(quiesced: bool) -> Self {
        Self {
            indicator: Indicator::new(),
            quiesced: AtomicBool::new(quiesced),
            pauses: AtomicUsize::new(0),
            halts: AtomicUsize::new(0),
            flushes: AtomicUsize::new(0),
            flush_fails: false,
        }
    }

    fn count(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::Acquire)
    }
}

impl Lifecycle for UnplugTestDevice {
    fn type_name(&self) -> &'static str {
        "unplug-test"
    }

    fn lifecycle_state(&self) -> Option<IndicatedState> {
        Some(self.indicator.state())
    }

    fn is_quiesced(&self) -> bool {
        self.quiesced.load(Ordering::Acquire)
    }

    fn start(&self) -> anyhow::Result<()> {
        self.indicator.start();
        Ok(())
    }

    fn pause(&self) {
        self.pauses.fetch_add(1, Ordering::AcqRel);
        self.indicator.pause();
    }

    fn halt(&self) {
        self.halts.fetch_add(1, Ordering::AcqRel);
        self.indicator.halt();
    }

    fn flush_backing(&self, intent: FlushIntent) -> Result<(), FlushError> {
        assert_eq!(intent, FlushIntent::BestEffort);
        self.flushes.fetch_add(1, Ordering::AcqRel);
        if self.flush_fails {
            return Err(FlushError::NotQuiesced(self.type_name()));
        }
        Ok(())
    }
}

const UNPLUG_BUDGET: Duration = Duration::from_millis(50);

#[test]
fn unplug_halts_a_running_device_that_quiesces() {
    let device = Arc::new(UnplugTestDevice::new(true));
    device.start().expect("start unplug test device");
    let dev: Arc<dyn Lifecycle> = device.clone();

    prepare_unplug(&dev, UNPLUG_BUDGET).expect("unplug quiesced device");

    assert_eq!(UnplugTestDevice::count(&device.pauses), 1);
    assert_eq!(UnplugTestDevice::count(&device.flushes), 1);
    assert_eq!(UnplugTestDevice::count(&device.halts), 1);
    assert_eq!(device.indicator.state(), IndicatedState::Halt);
}

#[test]
fn unplug_handles_a_device_that_never_started() {
    let device = Arc::new(UnplugTestDevice::new(true));
    assert_eq!(device.indicator.state(), IndicatedState::Init);
    let dev: Arc<dyn Lifecycle> = device.clone();

    prepare_unplug(&dev, UNPLUG_BUDGET).expect("unplug device in Init");

    assert_eq!(UnplugTestDevice::count(&device.pauses), 1);
    assert_eq!(device.indicator.state(), IndicatedState::Halt);
}

#[test]
fn unplug_does_not_pause_an_already_paused_device() {
    let device = Arc::new(UnplugTestDevice::new(true));
    device.pause();
    let dev: Arc<dyn Lifecycle> = device.clone();

    prepare_unplug(&dev, UNPLUG_BUDGET).expect("unplug paused device");

    assert_eq!(UnplugTestDevice::count(&device.pauses), 1);
    assert_eq!(device.indicator.state(), IndicatedState::Halt);
}

#[test]
fn unplug_leaves_a_stuck_device_running() {
    let device = Arc::new(UnplugTestDevice::new(false));
    device.start().expect("start unplug test device");
    let dev: Arc<dyn Lifecycle> = device.clone();
    let started = Instant::now();

    let error = prepare_unplug(&dev, UNPLUG_BUDGET)
        .expect_err("a device with in-flight work must not be halted");

    assert!(matches!(
        error,
        UnplugError::NotQuiesced {
            device: "unplug-test"
        }
    ));
    assert!(started.elapsed() < UNPLUG_BUDGET * 4);
    assert_eq!(UnplugTestDevice::count(&device.flushes), 0);
    assert_eq!(UnplugTestDevice::count(&device.halts), 0);
    assert_eq!(device.indicator.state(), IndicatedState::Pause);
}

#[test]
fn unplug_does_not_halt_a_device_whose_flush_failed() {
    let mut device = UnplugTestDevice::new(true);
    device.flush_fails = true;
    let device = Arc::new(device);
    let dev: Arc<dyn Lifecycle> = device.clone();

    let error = prepare_unplug(&dev, UNPLUG_BUDGET)
        .expect_err("a failed flush must not halt the device");

    assert!(matches!(error, UnplugError::Flush { .. }));
    assert_eq!(UnplugTestDevice::count(&device.halts), 0);
    assert_eq!(device.indicator.state(), IndicatedState::Pause);
}

#[test]
fn unplug_of_a_halted_device_is_idempotent() {
    let device = Arc::new(UnplugTestDevice::new(true));
    let dev: Arc<dyn Lifecycle> = device.clone();
    prepare_unplug(&dev, UNPLUG_BUDGET).expect("first unplug");

    prepare_unplug(&dev, UNPLUG_BUDGET).expect("second unplug");

    assert_eq!(UnplugTestDevice::count(&device.halts), 1);
    assert_eq!(UnplugTestDevice::count(&device.flushes), 1);
}

// ── A raced lifecycle request must not abort the VMM ─────────────────

/// A device whose quiesce poll is a second thread that resumes it.
///
/// This is the operator `resume` that lands between the quiesce wait
/// and the halt of an in-flight unplug.
struct ResumedUnderUnplug {
    indicator: Indicator,
    halts: AtomicUsize,
}

impl Lifecycle for ResumedUnderUnplug {
    fn type_name(&self) -> &'static str {
        "resumed-under-unplug"
    }

    fn lifecycle_state(&self) -> Option<IndicatedState> {
        Some(self.indicator.state())
    }

    fn is_quiesced(&self) -> bool {
        if self.indicator.state() == IndicatedState::Pause {
            self.indicator.resume();
        }
        true
    }

    fn pause(&self) {
        self.indicator.pause();
    }

    fn halt(&self) {
        self.halts.fetch_add(1, Ordering::AcqRel);
        self.indicator.halt();
    }
}

#[test]
fn a_repeated_pause_request_is_satisfied_not_fatal() {
    let ind = Indicator::new();
    ind.start();

    ind.pause();
    ind.pause();

    assert_eq!(ind.state(), IndicatedState::Pause);
}

#[test]
fn eight_threads_can_ask_one_device_to_pause() {
    let ind = Arc::new(Indicator::new());
    ind.start();

    let racers = (0..8)
        .map(|_| {
            let ind = Arc::clone(&ind);
            thread::spawn(move || ind.pause())
        })
        .collect::<Vec<_>>();

    for racer in racers {
        racer.join().expect("a losing pause must not kill the VMM");
    }
    assert_eq!(ind.state(), IndicatedState::Pause);
}

#[test]
fn a_repeated_resume_request_is_satisfied_not_fatal() {
    let ind = Indicator::new();
    ind.start();
    ind.pause();

    ind.resume();
    ind.resume();

    assert_eq!(ind.state(), IndicatedState::Run);
}

#[test]
fn a_repeated_halt_request_is_satisfied_not_fatal() {
    let ind = Indicator::new();
    ind.pause();
    ind.halt();

    ind.halt();

    assert_eq!(ind.state(), IndicatedState::Halt);
}

#[test]
fn pausing_a_halted_device_is_refused_not_fatal() {
    // An unplug halts a device before the registry drops it. A control
    // `pause` that walks the registry in between must not abort.
    let ind = indicator_in(IndicatedState::Halt);

    ind.pause();

    assert_eq!(ind.state(), IndicatedState::Halt);
}

#[test]
fn resuming_a_halted_device_is_refused_not_fatal() {
    let ind = indicator_in(IndicatedState::Halt);

    ind.resume();

    assert_eq!(ind.state(), IndicatedState::Halt);
}

#[test]
fn halting_a_running_device_is_refused_not_fatal() {
    let ind = indicator_in(IndicatedState::Run);

    ind.halt();

    assert_eq!(ind.state(), IndicatedState::Run);
}

#[test]
fn unplug_refuses_to_halt_a_device_resumed_under_it() {
    let device = Arc::new(ResumedUnderUnplug {
        indicator: Indicator::new(),
        halts: AtomicUsize::new(0),
    });
    let dev: Arc<dyn Lifecycle> = device.clone();

    let error = prepare_unplug(&dev, UNPLUG_BUDGET)
        .expect_err("a device that is running again must not be halted");

    assert!(
        matches!(
            error,
            UnplugError::Refused {
                device: "resumed-under-unplug",
                state: IndicatedState::Run,
            }
        ),
        "{error:?}",
    );
    assert_eq!(device.indicator.state(), IndicatedState::Run);
    // A halted device reported as refused makes the caller's abort path
    // resume a device whose worker is gone.
    assert_eq!(
        device.halts.load(Ordering::Acquire),
        0,
        "the unplug halted a device it went on to report as refused",
    );
}
