// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Device lifecycle trait, after the propolis `Lifecycle` contract.
//!
//! # State machine
//!
//! ```text
//!   Init ──→ Run ──→ Pause ──→ Run   (normal pause/resume)
//!                       │
//!                       └──→ Halt     (shutdown)
//!
//!   Init ──→ Pause                    (migration target: import before start)
//! ```

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::migrate::Migrator;

/// Durability required from a device backing-file flush.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushIntent {
    /// Migration: the fsync barrier MUST cover every write the guest saw
    /// completed. A loud failure is better than an unsynced disk.
    Durable,
    /// Teardown: a lost unflushed write is equal to a power loss. The
    /// guest accepts that because the devices advertise a volatile write
    /// cache (NVMe VWC=1, `VIRTIO_BLK_F_FLUSH`). C bhyve calls `exit(0)`
    /// here with no flush.
    BestEffort,
}

/// Failure to quiesce or sync a device backing file.
#[derive(Debug, thiserror::Error)]
pub enum FlushError {
    /// Device workers did not quiesce before the intent-specific deadline.
    #[error("workers did not quiesce in time")]
    NotQuiesced(&'static str),
    /// The backing file could not be cloned or synced.
    #[error("sync failed: {0}")]
    Sync(std::io::Error),
    /// A best-effort sync outlived its teardown watchdog.
    #[error("the sync outlived its watchdog")]
    TimedOut,
}

/// The deadline a device halt holds itself to when it does not say.
///
/// This value covers every halt in the tree that waits on purpose:
/// virtio-fs polls its worker for 5 s (`FS_HALT_BUDGET`), and
/// virtio-vsock and virtio-console use `HALT_BUDGET` (2 s) twice.
///
/// The default is the largest of these, not the smallest, because
/// teardown bounds each halt from the outside. A bound shorter than the
/// device's own deadline abandons the halt one poll before it returns,
/// and the in-kernel state that the halt releases stays held.
pub const DEFAULT_HALT_BUDGET: Duration = Duration::from_secs(5);

mod state;

pub use state::{
    DeviceMigrateState, DeviceStateError, DeviceStatePayload,
    HypervMigrateState, MigratePciState, NvmeMigrateCq, NvmeMigrateSq,
    NvmeMigrateState, VirtioMigrateQueue, VirtioMigrateState, WireBdf,
};

/// General trait for emulated devices in the system.
///
/// Stateless devices can use the defaults. Devices with background tasks
/// or internal state **must** implement at least `pause` and
/// `is_quiesced`.
pub trait Lifecycle: Send + Sync + 'static {
    /// Unique name for devices of a given type.
    fn type_name(&self) -> &'static str;

    /// Return the device's tracked lifecycle state, if it has one.
    ///
    /// Devices backed by an [`Indicator`] expose its state so
    /// coordinators do not request a transition that is already done.
    fn lifecycle_state(&self) -> Option<IndicatedState> {
        None
    }

    /// Returns true once the device's background work has drained.
    ///
    /// MUST NOT BLOCK. Callers poll this against a deadline, and a wait
    /// inside this method makes that deadline unenforceable. Implement
    /// it as an atomic load. Join threads in `halt()`.
    fn is_quiesced(&self) -> bool {
        true
    }

    /// Called just before the vCPU threads start.
    fn start(&self) -> anyhow::Result<()> {
        Ok(())
    }

    /// A paused device must stop producing work but must accept and
    /// queue new work from other devices.
    fn pause(&self) {}

    fn resume(&self) {}

    /// Reset to cold-start state. Only called on paused devices.
    fn reset(&self) {}

    /// The instance is stopping. Only called on paused devices.
    fn halt(&self) {}

    /// The deadline this device's [`halt`](Lifecycle::halt) holds
    /// itself to.
    ///
    /// Teardown bounds every halt by this value plus its own slack. A
    /// device that polls a deadline inside `halt` must report the same
    /// deadline here. If it reports less than it waits, teardown
    /// abandons it one poll short of returning, and the resources the
    /// halt releases stay held.
    ///
    /// A device whose halt returns at once can keep the default. A
    /// device that waits for longer MUST report it here: nothing else
    /// tells teardown to wait.
    fn halt_budget(&self) -> Duration {
        DEFAULT_HALT_BUDGET
    }

    fn migrate(&'_ self) -> Migrator<'_> {
        Migrator::Empty
    }

    /// Stop the kernel-side ring workers before the vCPUs pause, so
    /// no entry is consumed between the pause and the export. Only a
    /// device with rings in the kernel (viona) has anything to do.
    /// A failure fails the migration.
    fn pause_for_migration(&self) -> Result<(), DeviceStateError> {
        Ok(())
    }

    /// Flush any device-level writeback cache to durable storage.
    ///
    /// Migration calls this with [`FlushIntent::Durable`] after
    /// `hdl.pause()` quiesces the vCPUs and before the device state
    /// export and the ZFS barrier. Teardown uses
    /// [`FlushIntent::BestEffort`].
    ///
    /// With zvol `DKIOCSETWCE=1` (set by `blkdev::DiskCache`), writes
    /// complete from the zvol cache before they reach the pool. A
    /// `zfs send` incremental captures only pool state, so the
    /// destination loses any write not synced before the send.
    ///
    /// Implementors must let in-flight worker writes complete before
    /// the sync, so the barrier covers every write the guest saw
    /// complete.
    fn flush_backing(&self, _intent: FlushIntent) -> Result<(), FlushError> {
        Ok(())
    }

    /// The state a migration carries for this device, or `None` for
    /// a device with nothing to carry. A device that cannot read its
    /// state answers with an error, which fails the migration.
    fn export_migrate_state(
        &self,
    ) -> Result<Option<DeviceMigrateState>, DeviceStateError> {
        Ok(None)
    }

    /// Take the state this device exported on the source. The caller
    /// matched the payload to this device by identity, so a payload of
    /// another kind is an error, and so is one with values this device
    /// cannot hold.
    fn restore_migrate_state(
        &self,
        state: &DeviceMigrateState,
    ) -> Result<(), DeviceStateError> {
        Err(DeviceStateError::WrongKind {
            want: "none",
            got: state.kind(),
        })
    }

    /// Put back whatever [`Self::pause_for_migration`] stopped, after
    /// a migration that failed once the guest was already paused.
    ///
    /// `resume` alone is not enough for a device whose rings live in
    /// the kernel: the rings stay stopped and the guest resumes with a
    /// dead device. A failure is reported but does not stop the
    /// rollback, which must reach every other device.
    fn resume_after_migration(&self) -> Result<(), DeviceStateError> {
        Ok(())
    }

    /// Wake rings and start interrupt polling after
    /// `restore_migrate_state`.
    fn post_restore_kick(&self) {}
}

/// A transition the [`IndicatedState`] machine does not allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidTransition {
    pub from: IndicatedState,
    pub to: IndicatedState,
}

/// Atomic, validated tracking of [`Lifecycle`] states.
///
/// Two families of mutators share one validated core:
///
/// * [`Indicator::start`], [`Indicator::pause`], [`Indicator::resume`] and
///   [`Indicator::halt`] name a wanted end state. They never block. A
///   device already in that state is a satisfied request, because two
///   threads can both correctly ask one device to pause. A request the
///   machine does not allow leaves the state alone. The caller reads
///   [`Indicator::state`] when the outcome matters.
/// * [`Indicator::request`] does the same and returns the outcome, for a
///   caller that must know whether the edge was taken.
/// * [`Indicator::try_transition`] enforces the strict machine: a
///   repeated request is an error. Use it where the caller owns both the
///   device and the current state.
///
/// None of them panic. The release profile sets `panic = "abort"`, so a
/// panic here kills every guest on the host, and a lifecycle request
/// that races another thread is ordinary concurrency, not a bug.
#[derive(Default)]
pub struct Indicator(AtomicUsize);

impl Indicator {
    pub const fn new() -> Self {
        Self(AtomicUsize::new(IndicatedState::Init as usize))
    }

    /// Ask for `Run` from `Init` or `Pause`. A no-op when running.
    pub fn start(&self) {
        self.request_or_leave(IndicatedState::Run);
    }

    /// Ask for `Pause` from `Init` or `Run`. A no-op when paused.
    pub fn pause(&self) {
        self.request_or_leave(IndicatedState::Pause);
    }

    /// Ask for `Run` from `Pause`. A no-op when running.
    pub fn resume(&self) {
        self.request_or_leave(IndicatedState::Run);
    }

    /// Ask for `Halt` from `Pause`. A no-op when halted.
    pub fn halt(&self) {
        self.request_or_leave(IndicatedState::Halt);
    }

    /// Ask for `want` and drop a refusal.
    ///
    /// A refused request does not move the state, so
    /// [`Indicator::state`] still reports the truth. This shape exists
    /// because `Lifecycle::pause`, `resume` and `halt` return `()`.
    fn request_or_leave(&self, want: IndicatedState) {
        let _refused = self.request(want);
    }

    /// Drive the device toward `want` and say what happened.
    ///
    /// `Ok(true)` means this call took the edge. `Ok(false)` means the
    /// device was in `want` already, so the request is met and the
    /// caller must not undo a transition it did not make. `Err` means
    /// the machine has no edge from the current state to `want`, and
    /// the state is untouched.
    pub fn request(
        &self,
        want: IndicatedState,
    ) -> Result<bool, InvalidTransition> {
        match self.try_transition(want) {
            Ok(()) => Ok(true),
            // Reread rather than trusting the reported `from`: another
            // thread may have reached `want` since the failed compare.
            Err(refused) if self.state() == want => {
                let _lost_race = refused;
                Ok(false)
            }
            Err(refused) => Err(refused),
        }
    }

    pub fn state(&self) -> IndicatedState {
        IndicatedState::from_usize(self.0.load(Ordering::Acquire))
    }

    /// Attempt a state transition. The compare-exchange validates the
    /// old state before it stores the new one.
    pub fn try_transition(
        &self,
        new: IndicatedState,
    ) -> Result<(), InvalidTransition> {
        loop {
            let old_raw = self.0.load(Ordering::Acquire);
            let old = IndicatedState::from_usize(old_raw);

            if !IndicatedState::valid_transition(old, new) {
                return Err(InvalidTransition { from: old, to: new });
            }

            match self.0.compare_exchange_weak(
                old_raw,
                new as usize,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                // The state changed after the load. Validate again.
                Err(_) => continue,
            }
        }
    }
}

/// Current state held in [`Indicator`].
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[repr(usize)]
pub enum IndicatedState {
    Init = 0,
    Run = 1,
    /// Quiesced.
    Pause = 2,
    /// Terminal.
    Halt = 3,
}

impl IndicatedState {
    /// `Init -> Pause` lets a migration target import state before it
    /// starts. `Run -> Halt` is not allowed: a device must pause first.
    const fn valid_transition(old: Self, new: Self) -> bool {
        matches!(
            (old, new),
            (Self::Init, Self::Run)
                | (Self::Init, Self::Pause)
                | (Self::Run, Self::Pause)
                | (Self::Pause, Self::Run)
                | (Self::Pause, Self::Halt)
        )
    }

    fn from_usize(raw: usize) -> Self {
        match raw {
            0 => Self::Init,
            1 => Self::Run,
            2 => Self::Pause,
            3 => Self::Halt,
            _ => panic!(
                "corrupted IndicatedState value: {raw} \
                 (expected 0..=3, possible memory corruption)"
            ),
        }
    }
}

/// Poll interval while an unplug waits for a device to go quiet.
const UNPLUG_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Failure to make a device safe to remove.
///
/// No variant halts the device, so the caller can retry
/// [`prepare_unplug`] or leave the device attached.
#[derive(Debug)]
pub enum UnplugError {
    /// The device still had work in flight when the budget expired.
    NotQuiesced { device: &'static str },
    /// The device could not flush its backing store.
    Flush {
        device: &'static str,
        source: FlushError,
    },
    /// The device left `Pause` during the unplug, so the halt was
    /// refused.
    ///
    /// An operator `resume` between the quiesce wait and the halt puts
    /// the workers back to work. A halt there would stop a device with
    /// live DMA.
    Refused {
        device: &'static str,
        state: IndicatedState,
    },
}

/// Make a device safe to detach at runtime, then halt it.
///
/// Pauses the device, waits up to `budget` for its background work to
/// drain, flushes the backing store best effort, and halts. The halt
/// occurs only after the device reports quiescence, so an unplug never
/// removes in-flight DMA from the guest. There is no force path.
///
/// The pause is conditional so a paused device does not run its
/// worker-stopping code twice. `Init` is the usual start state: nothing
/// calls [`Lifecycle::start`], so an Indicator-backed device stays in
/// `Init` for the life of the VM. `Init -> Pause` is legal.
///
/// The halt result is checked. If an operator `resume` lands between
/// the quiesce wait and the halt, [`Indicator`] refuses `Run -> Halt`
/// and the device stays live. The unplug then reports
/// [`UnplugError::Refused`] so the caller does not detach its BARs.
///
/// # The halt is not bounded
///
/// `budget` bounds only the quiesce wait. viona's halt has no limit:
/// `VNA_IOC_DELETE` waits for each ring worker in a `cv_wait` that
/// ignores signals. Teardown abandons a stuck halt (`halt_bounded` in
/// vmm-machine) because only the VM destroy runs after it. An unplug
/// cannot: the eject caller would detach the BARs while a viona poll
/// thread is still inside a transport callback, and the state read
/// below would run under a halt that still holds its locks.
///
/// Callers must run this off the vCPU threads and off any thread that
/// must stay responsive. The eject path uses its drain thread. The
/// hot-add rollback runs it on the control-socket thread.
pub fn prepare_unplug(
    dev: &Arc<dyn Lifecycle>,
    budget: Duration,
) -> Result<(), UnplugError> {
    let device = dev.type_name();
    let state = dev.lifecycle_state();

    // A halted device has released its resources.
    if state == Some(IndicatedState::Halt) {
        return Ok(());
    }

    if state != Some(IndicatedState::Pause) {
        dev.pause();
    }

    if !wait_device_quiesced(dev.as_ref(), budget) {
        return Err(UnplugError::NotQuiesced { device });
    }

    // Decide the refusal before the halt. Each device's halt ignores
    // the Indicator's answer and stops its worker anyway, so a state
    // read after the halt reports a refusal for a destroyed device, and
    // the caller's abort path resumes a device with no worker. A resume
    // between this read and the halt still gets that outcome. To close
    // the window, each device's halt must obey its Indicator.
    //
    // A device with no Indicator reports nothing and its halt stands.
    if let Some(state) = dev.lifecycle_state() {
        if state != IndicatedState::Pause {
            return Err(UnplugError::Refused { device, state });
        }
    }

    dev.flush_backing(FlushIntent::BestEffort)
        .map_err(|source| UnplugError::Flush { device, source })?;

    dev.halt();
    match dev.lifecycle_state() {
        None | Some(IndicatedState::Halt) => Ok(()),
        Some(state) => Err(UnplugError::Refused { device, state }),
    }
}

/// Poll `is_quiesced` against a deadline. The loop sleeps between loads
/// so the budget stays enforceable.
fn wait_device_quiesced(dev: &dyn Lifecycle, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if dev.is_quiesced() {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        thread::sleep(UNPLUG_POLL_INTERVAL.min(deadline - now));
    }
}

#[cfg(test)]
mod tests;
