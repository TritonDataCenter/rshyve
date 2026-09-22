// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest exit handling, device quiesce, and VM teardown.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use slog::{error, info, warn, Logger};
use vmm_core::exits::Suspend;
use vmm_core::machine::Machine;
use vmm_devices::quiesce::{wait_all_quiesced_named, NamedDevice};

use crate::hotplug::HotplugEngines;
use crate::registry::DeviceRegistry;
use crate::vcpu::VcpuFleet;
use crate::vcpu_tasks::VcpuEvent;

const QUIESCE_BUDGET: Duration = Duration::from_secs(5);
const VCPU_JOIN_BUDGET: Duration = Duration::from_secs(5);

/// How long one device gets to flush its backing store.
///
/// `flush_backing` is one synchronous call into a backend, so no budget
/// here can interrupt it. What the count buys is the size of the clock
/// the terminal watchdog is given, so a VM with many disks is not ended
/// while it is still writing.
const FLUSH_BUDGET_PER_DEVICE: Duration = Duration::from_secs(2);

/// Slack over a device's own halt deadline.
///
/// A device that polls its own deadline returns just after it, not on
/// it, and then does its last cleanup. virtio-fs polls in 10 ms steps
/// and then closes the files the guest held open. A second covers both
/// by a wide margin, and more only delays the destroy.
const HALT_OVERRUN_SLACK: Duration = Duration::from_secs(1);

/// How many times the sweep asks the system for a halt thread.
///
/// A refused spawn is usually transient at teardown, and the guest can
/// help cause one: every console client it opens takes a reader thread,
/// and the console halt that ran a moment ago is what gives those
/// threads back. Asking again is cheap and often works.
const HALT_SPAWN_ATTEMPTS: u32 = 3;

/// How long the sweep waits between those attempts.
const HALT_SPAWN_RETRY_PAUSE: Duration = Duration::from_millis(50);

/// How long one device gets to give up its in-kernel and thread state.
///
/// [`halt_devices`] runs each `halt` on a thread of its own and stops
/// waiting after the value returned here, so it bounds the halt phase
/// and does not only size a clock.
///
/// The device declares the deadline it holds itself to, and the sweep
/// adds `slack` on top of it. The budget is not a constant here: a copy
/// of another crate's deadline, such as `FS_HALT_BUDGET`, can drift from
/// the original with no compiler error and no failed test.
///
/// The result is capped at [`TEARDOWN_BUDGET_MAX`]. The device supplies
/// the value, so one device must not be able to hold the sweep for
/// longer than the whole teardown may claim. Past the cap the terminal
/// watchdog would end the process anyway, so the wait buys nothing.
///
/// Nothing here can interrupt a halt that overruns. viona's is the one
/// that matters: `VNA_IOC_DELETE` waits for each ring worker to stop,
/// in a `cv_wait` that does not heed signals. The budget only decides
/// when the sweep stops waiting for it and goes on to the destroy.
fn halt_budget_for(
    device: &dyn vmm_devices::Lifecycle,
    slack: Duration,
) -> Duration {
    device
        .halt_budget()
        .saturating_add(slack)
        .min(TEARDOWN_BUDGET_MAX)
}

/// How long `VM_DESTROY_SELF` gets before teardown stops waiting on it.
///
/// Teardown cannot call it unbounded: on illumos the ioctl purges the
/// vmm_drv holds and waits for every lease to break in an untimed
/// `cv_wait` (`vmm_sol_dev.c`, `vmm_drv_purge`). A lease that never
/// breaks holds the calling thread for as long as it is held, and no
/// signal, close or exit reaches a thread parked there.
const DESTROY_BUDGET: Duration = Duration::from_secs(15);

/// The least teardown gets, whatever the device count.
///
/// A smaller budget would cut off a slow but working flush.
const TEARDOWN_BUDGET_MIN: Duration = Duration::from_secs(60);

/// The most teardown may claim from the terminal watchdog.
///
/// A device count large enough to overflow the sum must not disarm the
/// watchdog by asking for a deadline it can never reach.
///
/// Past 34 lifecycle devices that take the default halt budget, this
/// ceiling is shorter than the worst-case bounded sweep, so the watchdog
/// can end a teardown that is still working. A 40-disk VM is possible.
/// The trade is accepted, and a test pins the count. The worst case
/// needs every device to spend its whole flush budget and its whole halt
/// budget, which no real block flush and no returning halt does, and a
/// node must not hold the memory of a VM that will not stop for longer
/// than five minutes. A device that reports a shorter
/// [`vmm_devices::Lifecycle::halt_budget`] raises the count.
const TEARDOWN_BUDGET_MAX: Duration = Duration::from_secs(300);

/// How often the event loop rechecks a roster that can grow.
///
/// A fixed vCPU set is finished when the event channel closes, because
/// the threads hold the only senders. A roster keeps one more sender
/// for the CPU that is not added yet, so the channel never closes and
/// the loop asks the roster whether any thread is left instead.
const ROSTER_POLL: Duration = Duration::from_millis(250);

const ROSTER_POISONED: &str = "vCPU roster lock poisoned";

/// Why the guest stopped running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Reboot,
    PowerOff,
    Halt,
    TripleFault(i32),
    /// The guest triggered a VM exit that userspace cannot emulate.
    GuestFault,
}

/// Pause every device, wait for the backends to go quiet, then flush.
#[cfg(test)]
fn quiesce_devices(
    devices: &[Arc<dyn vmm_devices::Lifecycle>],
    log: &Logger,
) -> vmm_devices::QuiesceReport {
    let named = devices
        .iter()
        .map(|device| NamedDevice {
            id: None,
            device: device.clone(),
        })
        .collect::<Vec<_>>();
    quiesce_named(&named, log)
}

/// Snapshot the registry as named lifecycle handles.
///
/// The handles are cloned out, so no registry lock is held while a
/// device runs. Public because the control path pauses the same set and
/// must name a stuck device the same way.
pub fn registry_named_devices(registry: &DeviceRegistry) -> Vec<NamedDevice> {
    registry
        .list()
        .into_iter()
        .filter_map(|slot| {
            slot.lifecycle.map(|device| NamedDevice {
                id: Some(slot.id),
                device,
            })
        })
        .collect()
}

fn quiesce_named(
    devices: &[NamedDevice],
    log: &Logger,
) -> vmm_devices::QuiesceReport {
    for named in devices {
        // Teardown may follow an operator pause. Keep Indicator's transition
        // validation strict while making repeated quiescence idempotent.
        if named.device.lifecycle_state()
            != Some(vmm_devices::IndicatedState::Pause)
        {
            named.device.pause();
        }
    }

    let report = wait_all_quiesced_named(devices, QUIESCE_BUDGET);
    if !report.is_empty() {
        warn!(log, "devices did not quiesce before teardown";
            "devices" => ?report.names());
    }

    for named in devices {
        if let Err(error) = named
            .device
            .flush_backing(vmm_devices::FlushIntent::BestEffort)
        {
            warn!(log, "device flush incomplete at teardown";
                "device" => named.device.type_name(),
                "id" => named.id.as_deref(),
                "error" => ?error);
        }
    }

    report
}

/// Halt every device, giving back the in-kernel state it holds.
///
/// This must run before the destroy: viona takes a `vmm_drv` hold for
/// the life of its link, and `VM_DESTROY_SELF` waits for every hold to
/// be released in an untimed `cv_wait` (`vmm_sol_dev.c`,
/// `vmm_drv_purge` and `vmm_lease_block`). viona gives that hold back
/// in `VNA_IOC_DELETE`, which its `Lifecycle::halt` issues. A halt that
/// runs after the destroy, or not at all, leaves the hold in place and
/// the destroy waits on a thread no signal can reach.
///
/// A device already in `Halt` is skipped, because a hot-unplug halted
/// it before teardown started and a second halt has nothing to give
/// back. A device that refuses the transition is reported: an operator
/// `resume` between the quiesce and here puts its workers back to work.
///
/// Every halt is bounded. Several halts join threads, and one that does
/// not return would hold the sweep short of the destroy, which is the
/// wedge the sweep exists to prevent. No single device may cost the VM
/// its destroy, so each halt gets the deadline it declared plus `slack`,
/// and no more. See [`halt_budget_for`].
fn halt_devices(devices: &[NamedDevice], slack: Duration, log: &Logger) {
    for named in devices {
        if named.device.lifecycle_state()
            == Some(vmm_devices::IndicatedState::Halt)
        {
            continue;
        }
        let budget = halt_budget_for(named.device.as_ref(), slack);
        if !halt_bounded(&named.device, budget, log) {
            error!(log, "device halt did not return; going on to the VM destroy";
                "device" => named.device.type_name(),
                "id" => named.id.as_deref(),
                "budget_ms" => budget.as_millis() as u64);
            // The state is not read here: the halt still holds whatever
            // locks it took, and this thread must not wait on one.
            continue;
        }
        if let Some(state) = named.device.lifecycle_state() {
            if state != vmm_devices::IndicatedState::Halt {
                warn!(log, "device refused to halt before the VM destroy";
                    "device" => named.device.type_name(),
                    "id" => named.id.as_deref(),
                    "state" => ?state);
            }
        }
    }
}

/// Work handed to a thread the sweep may then abandon.
type HaltWork = Box<dyn FnOnce() + Send + 'static>;

/// How the sweep gets a thread for one halt.
///
/// A function and not a direct spawn so the refused case is testable.
/// That case is the one that must not put an untimed ioctl on the
/// teardown thread, and no test can exhaust the real thread limit.
type HaltSpawn<'a> = &'a dyn Fn(HaltWork) -> io::Result<()>;

/// Start one halt on a thread nothing joins.
///
/// The handle is dropped on purpose: a halt that outlives its budget is
/// left running, so there is never anything to join.
fn spawn_halt(work: HaltWork) -> io::Result<()> {
    thread::Builder::new()
        .name("device-halt".into())
        .spawn(work)
        .map(drop)
}

/// Halt one device, and stop waiting on it after `budget`.
///
/// Returns false when the halt did not return inside the budget. The
/// thread is left running: nothing in userspace can revoke a syscall a
/// thread is blocked in, and `VNA_IOC_DELETE` is one such.
///
/// The thread does not contain a panic in the shipped VMM. The release
/// profile sets `panic = "abort"`, so a device that panics in `halt`
/// ends the process there, and the `VM_SET_AUTODESTRUCT` the machine
/// builder armed is what reclaims the instance. Where the build
/// unwinds, the panic reaches this thread as a closed channel and the
/// sweep goes on.
fn halt_bounded(
    device: &Arc<dyn vmm_devices::Lifecycle>,
    budget: Duration,
    log: &Logger,
) -> bool {
    halt_bounded_with(&spawn_halt, device, budget, HALT_SPAWN_RETRY_PAUSE, log)
}

/// The same halt over a caller-supplied spawn.
fn halt_bounded_with(
    spawn: HaltSpawn<'_>,
    device: &Arc<dyn vmm_devices::Lifecycle>,
    budget: Duration,
    retry_pause: Duration,
    log: &Logger,
) -> bool {
    let (tx, rx) = mpsc::channel();
    let mut started = false;
    for attempt in 1..=HALT_SPAWN_ATTEMPTS {
        let halting = Arc::clone(device);
        let done = tx.clone();
        match spawn(Box::new(move || {
            halting.halt();
            // A closed channel means the sweep stopped waiting, which
            // is the case this bound exists for.
            let _sweep_gave_up = done.send(());
        })) {
            Ok(()) => {
                started = true;
                break;
            }
            Err(e) => {
                warn!(log, "could not start a device halt thread";
                    "device" => device.type_name(),
                    "attempt" => attempt,
                    "error" => %e);
                if attempt < HALT_SPAWN_ATTEMPTS {
                    thread::sleep(retry_pause);
                }
            }
        }
    }
    // The sweep keeps no sender, so the wait below ends when the halt
    // thread goes, whether it sent or panicked.
    drop(tx);

    if !started {
        // The halt is skipped, not run on this thread: `VNA_IOC_DELETE`
        // is untimed and heeds no signal, so a halt here has no bound at
        // all and would hold the sweep one step short of the VM destroy.
        // The guest can help cause the refusal it would follow, one
        // thread per console client it opens, so a guest that cannot
        // wedge the destroy could still wedge this.
        //
        // Skipping the halt does not make the destroy wedge certain.
        // The hold it leaves makes `vmm_drv_purge` wait, and the purge
        // asks each lessee to break its lease. A viona ring worker that
        // reaches `vring_need_bail_ext` (`viona_ring.c`) breaks it. The
        // wedge needs a worker that cannot reach that test. So this
        // raises the risk of the wedge but does not make it certain,
        // and the destroy keeps its own budget behind it.
        error!(log, "no thread for the device halt; leaving its state to the VM destroy";
            "device" => device.type_name(),
            "attempts" => HALT_SPAWN_ATTEMPTS);
        return false;
    }

    match rx.recv_timeout(budget) {
        Ok(()) => true,
        // The thread went without sending, so the halt panicked. It is
        // over either way, and the sweep goes on to the next device.
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            error!(log, "the device halt panicked";
                "device" => device.type_name());
            true
        }
        Err(mpsc::RecvTimeoutError::Timeout) => false,
    }
}

/// The vCPU threads that are running, which can grow at run time.
///
/// The boot set is fixed, but a CPU brought online while the guest
/// runs adds a thread, so the join at teardown cannot work from a
/// snapshot taken at boot.
///
/// There is no matching remove. illumos has no `vm_deactivate_cpu`:
/// `active_cpus` is only ever set, and only `vm_init` (`vmm.c`) clears
/// it, wholesale, on `VM_REINIT`. CPU hot-REMOVE is therefore not
/// possible at the kernel boundary, and this type does not pretend
/// otherwise.
pub struct VcpuThreads {
    inner: Mutex<Roster>,
}

struct Roster {
    handles: Vec<JoinHandle<()>>,
    /// Teardown has taken the handles. A later add must be refused, or
    /// its thread would never be joined.
    closed: bool,
}

impl VcpuThreads {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Roster {
                handles: Vec::new(),
                closed: false,
            }),
        })
    }

    /// Record a thread to join at teardown.
    ///
    /// After [`close`](Self::close) the handle comes back instead, so
    /// the caller can undo the spawn rather than leak the thread.
    pub fn push(&self, handle: JoinHandle<()>) -> Result<(), JoinHandle<()>> {
        let mut roster = self.inner.lock().expect(ROSTER_POISONED);
        if roster.closed {
            return Err(handle);
        }
        // A rolled-back add leaves a finished handle behind. Dropping
        // it here keeps a long-running VM from collecting one per
        // refused add.
        roster.handles.retain(|handle| !handle.is_finished());
        roster.handles.push(handle);
        Ok(())
    }

    /// Recorded threads that have not finished.
    pub fn live(&self) -> usize {
        let roster = self.inner.lock().expect(ROSTER_POISONED);
        roster.handles.iter().filter(|h| !h.is_finished()).count()
    }

    /// Recorded threads, finished ones included.
    pub fn len(&self) -> usize {
        self.inner.lock().expect(ROSTER_POISONED).handles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_closed(&self) -> bool {
        self.inner.lock().expect(ROSTER_POISONED).closed
    }

    /// Take every handle and refuse later pushes.
    pub fn close(&self) -> Vec<JoinHandle<()>> {
        let mut roster = self.inner.lock().expect(ROSTER_POISONED);
        roster.closed = true;
        std::mem::take(&mut roster.handles)
    }
}

/// The vCPU threads an event loop waits on.
enum VcpuSet<'a> {
    /// A set made at boot. The event channel closing means every
    /// thread has gone.
    Fixed(Vec<JoinHandle<()>>),
    /// A set the control plane can add to while the guest runs.
    Roster(&'a Arc<VcpuThreads>),
}

impl VcpuSet<'_> {
    /// Wait for the next vCPU event, or `None` when no vCPU thread is
    /// left to send one.
    fn next_event(
        &self,
        event_rx: &mpsc::Receiver<VcpuEvent>,
    ) -> Option<VcpuEvent> {
        match self {
            VcpuSet::Fixed(_) => event_rx.recv().ok(),
            VcpuSet::Roster(threads) => loop {
                match event_rx.recv_timeout(ROSTER_POLL) {
                    Ok(event) => return Some(event),
                    Err(mpsc::RecvTimeoutError::Disconnected) => return None,
                    // Every exit path of a vCPU run loop sends an
                    // event first, so this only catches a thread that
                    // died without one, a panic for instance.
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if threads.live() == 0 {
                            return None;
                        }
                    }
                }
            },
        }
    }

    fn join(self, budget: Duration) -> bool {
        match self {
            VcpuSet::Fixed(handles) => join_vcpus_bounded(handles, budget),
            VcpuSet::Roster(threads) => {
                join_vcpus_bounded(threads.close(), budget)
            }
        }
    }
}

/// Join every vCPU thread, giving up after `budget`.
///
/// Returns false when the budget expired. A wedged vCPU thread must not
/// hold the bhyve instance open, so the caller destroys the VM anyway.
fn join_vcpus_bounded(handles: Vec<JoinHandle<()>>, budget: Duration) -> bool {
    vmm_core::thread::join_bounded(handles, budget)
}

/// The per-device half of the teardown clock.
///
/// Summed and not multiplied, because each device declares the halt
/// deadline it holds itself to. A VM of block devices that return at
/// once therefore asks for a shorter clock than a VM with a virtio-fs
/// share on it.
fn device_work(devices: &[NamedDevice]) -> Duration {
    devices.iter().fold(Duration::ZERO, |total, named| {
        total
            .saturating_add(FLUSH_BUDGET_PER_DEVICE)
            .saturating_add(halt_budget_for(
                named.device.as_ref(),
                HALT_OVERRUN_SLACK,
            ))
    })
}

/// The clock the terminal watchdog holds this teardown to.
///
/// Quiesce, the vCPU join and the destroy cannot run past their own
/// budgets. The flush can: it is one synchronous backend call per
/// device, so a VM with many disks needs more time than a VM with one,
/// and no constant can know which it is.
fn teardown_budget(devices: &[NamedDevice]) -> Duration {
    teardown_budget_for(device_work(devices))
}

/// The same clock over a device total the caller already has.
fn teardown_budget_for(device_work: Duration) -> Duration {
    QUIESCE_BUDGET
        .saturating_add(device_work)
        .saturating_add(VCPU_JOIN_BUDGET)
        .saturating_add(DESTROY_BUDGET)
        .clamp(TEARDOWN_BUDGET_MIN, TEARDOWN_BUDGET_MAX)
}

/// Destroy the VM, and stop waiting on the call after `budget`.
///
/// The ioctl is not bounded by anything userspace holds, so it runs on
/// its own thread and the rest of teardown does not wait past the
/// budget. A thread that misses the budget keeps its handle, because
/// the kernel is still using the file descriptor behind it.
///
/// Returns false when the destroy did not return. A destroy that is
/// only slow still ends, and the `VM_SET_AUTODESTRUCT` the machine
/// builder armed reclaims the instance when the fd closes at process
/// exit.
///
/// A destroy parked in the lease purge usually ends as well, by a
/// route that is not obvious. `proc_exit` (`exit.c`) sets `SEXITING`
/// before it calls `exitlwps`. viona ring workers are LWPs of this same
/// process (`viona_create_worker` in `viona_ring.c` passes `curproc`),
/// and `vring_need_bail_ext` tests that flag. A worker that reaches the
/// test bails and drops its lease, and the purge that waited for the
/// lease returns. Process exit does not reach the parked destroy thread
/// itself: `cv_wait` leaves it neither wakeable nor waiting, so
/// `pokelwps` (`lwp.c`) steps over it and `exitlwps` waits for it.
///
/// It does not end when a ring worker cannot reach that test. viona halt
/// and ring reset are the remaining boundary: a worker inside the mac
/// perimeter, or in `viona_tx_wait_outstanding`, which is an untimed
/// `cv_wait` that neither heeds signals nor rechecks the bail
/// (`viona_tx.c`). [`halt_devices`] runs that halt before the destroy so
/// the purge has no lease left to wait for. This bound is the belt
/// behind it. See [`crate::signal`].
///
/// The destroy is a closure so that a call which never returns is
/// testable without a live /dev/vmm handle.
fn destroy_bounded_with(
    destroy: impl FnOnce() -> io::Result<()> + Send + 'static,
    budget: Duration,
    log: &Logger,
) -> bool {
    let (tx, rx) = mpsc::channel();
    let spawned =
        thread::Builder::new()
            .name("vm-destroy".into())
            .spawn(move || {
                // A closed channel means teardown stopped waiting, which is
                // the wedge this bound exists for. The send result says
                // nothing the receiver does not already know.
                drop(tx.send(destroy()));
            });
    if let Err(e) = spawned {
        // A destroy on this thread would have no bound at all.
        error!(log, "could not start the VM destroy; leaving it to autodestruct";
            "error" => %e);
        return false;
    }

    match rx.recv_timeout(budget) {
        Ok(Ok(())) => true,
        // The call returned, so teardown is over even though it failed.
        Ok(Err(e)) => {
            warn!(log, "failed to destroy VM"; "error" => %e);
            true
        }
        // The ioctl freezes every vCPU and then drains the vmm_drv
        // leases. Name both, because either can hold it.
        Err(mpsc::RecvTimeoutError::Timeout) => {
            error!(log, "the VM destroy did not return inside its budget";
                "likely_cause" => "a vCPU that will not freeze, or an unbroken vmm_drv lease",
                "budget_secs" => budget.as_secs());
            false
        }
        // The thread went without sending, so nothing says the ioctl
        // ran. Report it the same way: the instance is not known gone.
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            error!(log, "the VM destroy thread ended with no result");
            false
        }
    }
}

/// Run the event loop over a whole fleet, growable when the VM has a
/// CPU hot-add engine and fixed otherwise.
///
/// The registry keeps its own sender, so the growable loop reads
/// "every thread has gone" off the roster. Without a registry the drop
/// of the fleet's sender here is what lets the channel close.
pub fn run_fleet_event_loop(
    machine: &Machine,
    fleet: VcpuFleet,
    hotplug: &HotplugEngines,
    control_cleanup: Option<PathBuf>,
    registry: &DeviceRegistry,
    log: &Logger,
) -> anyhow::Result<RunOutcome> {
    let VcpuFleet {
        events,
        sender,
        threads,
        ..
    } = fleet;
    drop(sender);
    let vcpus = if hotplug.cpu.is_some() {
        VcpuSet::Roster(&threads)
    } else {
        VcpuSet::Fixed(threads.close())
    };
    run_event_loop_with(
        machine,
        events,
        vcpus,
        control_cleanup,
        registry,
        || hotplug.close(),
        log,
    )
}

fn run_event_loop_with(
    machine: &Machine,
    event_rx: mpsc::Receiver<VcpuEvent>,
    vcpus: VcpuSet<'_>,
    control_cleanup: Option<PathBuf>,
    registry: &DeviceRegistry,
    close_hotplug: impl FnOnce(),
    log: &Logger,
) -> anyhow::Result<RunOutcome> {
    let outcome = outcome_of(vcpus.next_event(&event_rx), log);
    // Before the snapshot below, not after the sweep: a device added
    // during the quiesce window would not be in the list, so nothing
    // would halt it, and the `vmm_drv` lease it holds would park
    // `VM_DESTROY_SELF` in `vmm_lease_block`.
    close_hotplug();
    let hdl = machine.hdl().clone();
    shutdown_after_exit(
        vcpus,
        &registry_named_devices(registry),
        control_cleanup.as_deref(),
        move || hdl.destroy(),
        crate::signal::arm_teardown_deadline,
        log,
    );
    outcome
}

/// Everything teardown does once the guest has stopped.
///
/// The order prevents the destroy wedge:
///
/// 1. Arm the clock, before any step that cannot be interrupted.
/// 2. Quiesce, so no device is still writing.
/// 3. Join the vCPU threads, so no guest access races the halt.
/// 4. Halt the devices, which gives back the in-kernel state.
/// 5. Destroy the VM.
///
/// Step 4 before step 5 is what keeps the destroy out of
/// `vmm_lease_block`. See [`halt_devices`].
///
/// Every step runs under the clock armed in step 1, and no step waits
/// past a budget: `VNA_IOC_DELETE` blocks untimed and does not heed
/// signals, so a halt that will not return must not hold this thread.
///
/// Split from [`run_event_loop_with`] so the order is testable with no
/// live /dev/vmm handle.
fn shutdown_after_exit(
    vcpus: VcpuSet<'_>,
    devices: &[NamedDevice],
    control_cleanup: Option<&Path>,
    destroy: impl FnOnce() -> io::Result<()> + Send + 'static,
    arm_deadline: impl FnOnce(Duration),
    log: &Logger,
) {
    // The flush and the halt below are synchronous backend calls, one
    // per device, and the destroy after them is unbounded in the
    // kernel. None can be interrupted from here, so the terminal
    // watchdog keeps a clock across the whole teardown and is given a
    // budget that scales with the work.
    arm_deadline(teardown_budget(devices));

    // A wedged backend must not prevent release of the bhyve instance.
    let _report = quiesce_named(devices, log);

    if !vcpus.join(VCPU_JOIN_BUDGET) {
        warn!(log, "vCPU threads did not exit; destroying VM anyway");
    }

    halt_devices(devices, HALT_OVERRUN_SLACK, log);

    // The socket path may already be gone.
    if let Some(path) = control_cleanup {
        let _ = std::fs::remove_file(path);
    }

    // The crash paths need no destroy: the machine builder arms
    // VM_SET_AUTODESTRUCT, so the kernel reclaims the instance when the fd
    // closes. This explicit destroy is the deterministic commit point,
    // ordered after quiesce, vCPU join and device halt.
    if destroy_bounded_with(destroy, DESTROY_BUDGET, log) {
        info!(log, "VM shutdown complete");
    }
}

fn outcome_of(
    event: Option<VcpuEvent>,
    log: &Logger,
) -> anyhow::Result<RunOutcome> {
    match event {
        Some(VcpuEvent::Suspended { vcpu_id, kind }) => {
            info!(log, "VM suspended";
                "vcpu" => vcpu_id,
                "kind" => ?kind,
            );
            Ok(match kind {
                Suspend::Reset => RunOutcome::Reboot,
                Suspend::PowerOff => RunOutcome::PowerOff,
                Suspend::Halt => RunOutcome::Halt,
                Suspend::TripleFault(src) => RunOutcome::TripleFault(src),
            })
        }
        Some(VcpuEvent::Error {
            vcpu_id,
            error: err,
        }) => {
            error!(log, "vCPU error";
                "vcpu" => vcpu_id,
                "error" => &err,
            );
            Ok(RunOutcome::GuestFault)
        }
        None => {
            warn!(log, "all vCPU threads exited without events");
            Ok(RunOutcome::Halt)
        }
    }
}

#[cfg(test)]
mod tests;
