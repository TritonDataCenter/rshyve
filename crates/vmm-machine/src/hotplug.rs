// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Run-time PCI hot-add and hot-remove.
//!
//! The register files in [`vmm_devices::hotplug`] run on a vCPU thread
//! inside an I/O exit, so they only record what the guest asked for.
//! This is the other half: one thread that owns every add and every
//! removal, so no teardown ever runs on a vCPU thread and no vCPU ever
//! blocks on a backing file.
//!
//! # Add
//!
//! An operator names a `-s` spec. The slot is checked before anything
//! is allocated, the device is built through the binary's own catalog,
//! and only then does the guest hear about it.
//!
//! # Remove
//!
//! A removal is a REQUEST, never an order. `request_remove` marks the
//! slot and asks the guest to run `_EJ0`. It tears nothing down and
//! returns at once. The guest answers when its driver has let go. A
//! guest that never answers leaves the slot in
//! [`SlotState::RemovePending`] for the life of the VM, which is the
//! correct outcome: the device is still in use. Nothing here forces a
//! removal, and no backing file is closed before the guest is done with
//! it.
//!
//! # Seams
//!
//! [`HotplugEngine::start`] takes the binary's catalog closure, the same
//! one the boot-time `-s` pass uses, so a control request can express
//! nothing that argv cannot. The PCI bus and the VM handle are read out
//! of the [`OwnedPciCtx`] rather than passed in, so the bus a device is
//! detached from is always the bus its catalog attached it to.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use slog::{debug, error, info, warn, Logger};

use vmm_devices::hotplug::pci::{is_hotpluggable_slot, PciHotplug};
use vmm_devices::lifecycle::prepare_unplug;
use vmm_devices::pci::{Bdf, PciBus};
use vmm_devices::IndicatedState;

use crate::devspec;
use crate::parse::parse_bdf;
use crate::pause::{VmPause, VmPauseGate};
use crate::pci::{
    CreatedPciDevice, OwnedPciCtx, PciDeviceCtx, PciDeviceHandle,
};
use crate::registry::{
    DeviceRegistry, RegisteredDevice, RegistryError, SlotState,
};
use crate::vcpu::VcpuFleet;
use crate::vcpus::{VcpuRegistry, VcpuSetup};

use self::cpu::{CpuHotplugEngine, CpuHotplugError, CpuOnline};
use self::drain::DrainThread;
use self::mem::MemHotplugEngine;
use self::unplug::{unplug_bounded, Unplug, UNPLUG_HALT_SLACK};

/// How long a device gets to drain before an eject is abandoned.
///
/// A device that is still doing DMA must keep its slot, so the budget
/// expiring aborts the removal instead of forcing it.
const UNPLUG_BUDGET: Duration = Duration::from_secs(5);

/// How long [`HotplugEngine::close`] waits for the drain thread.
///
/// A thread abandoned inside a device release must not hold the
/// teardown one step short of the VM destroy.
const DRAIN_JOIN_BUDGET: Duration = Duration::from_secs(5);

/// How often the engine looks for an eject the guest has run.
const DRAIN_INTERVAL: Duration = Duration::from_millis(100);

/// A PCI config read of an empty slot: the bus float.
const NO_DEVICE: u32 = 0xFFFF_FFFF;

/// Why a hotplug request was refused.
///
/// Hand-written rather than derived: this crate carries no `thiserror`
/// dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotplugError {
    /// The VM did not ask for the ACPI hotplug interface.
    Disabled,
    /// The spec is not `slot,driver[,config]`, or names an address a
    /// hot-add cannot use.
    BadSpec(String),
    /// Slot 0 is the host bridge and slot 1 is the LPC bridge. Neither
    /// gets an `_EJ0` method, so neither can be plugged.
    FixedSlot(u8),
    /// Something already holds the slot.
    SlotTaken(Bdf),
    /// Another device holds the id this spec would take.
    DuplicateId(String),
    /// The catalog refused the spec or could not build the device.
    Create(String),
    /// The catalog built no PCI device. `hostbridge` and `lpc` do that,
    /// because the chipset already owns them, and so does a driver name
    /// the catalog does not know.
    NothingBuilt(String),
    /// The registry refused the record.
    Registry(RegistryError),
    /// No device holds this id.
    NoSuchDevice(String),
    /// The device came from argv. Only a hot-added device can be
    /// removed, because only its slot was ever advertised as ejectable.
    NotHotpluggable(String),
    /// The slot is already on its way out.
    NotRemovable { id: String, state: SlotState },
    /// The VM has stopped. Teardown has already taken its list of
    /// devices, so a device added now would never be halted and its
    /// `vmm_drv` lease would park the VM destroy.
    Closed,
    /// No thread is draining ejects, so the guest's `_EJ0` would never
    /// be acted on and the slot would stay `RemovePending` for the life
    /// of the VM.
    NotDrained,
}

impl fmt::Display for HotplugError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HotplugError::Disabled => {
                f.write_str("this VM was not started with --hotplug")
            }
            HotplugError::Closed => f.write_str("the VM is shutting down"),
            HotplugError::NotDrained => f.write_str(
                "this VM has no hotplug drain thread, so a removal could \
                 not be finished",
            ),
            HotplugError::BadSpec(reason) => write!(f, "bad spec: {reason}"),
            HotplugError::FixedSlot(slot) => {
                write!(f, "PCI slot {slot} is fixed and cannot be plugged")
            }
            HotplugError::SlotTaken(bdf) => {
                write!(f, "PCI slot {bdf} is already in use")
            }
            HotplugError::DuplicateId(id) => {
                write!(f, "device id {id} is already registered")
            }
            HotplugError::Create(reason) => {
                write!(f, "cannot create the device: {reason}")
            }
            HotplugError::NothingBuilt(driver) => {
                write!(f, "driver {driver} builds no device to put in a slot")
            }
            HotplugError::Registry(e) => write!(f, "{e}"),
            HotplugError::NoSuchDevice(id) => {
                write!(f, "no device with id {id}")
            }
            HotplugError::NotHotpluggable(id) => {
                write!(f, "device {id} was not hot-added and cannot be removed")
            }
            HotplugError::NotRemovable { id, state } => {
                write!(f, "device {id} is {state}")
            }
        }
    }
}

impl std::error::Error for HotplugError {}

/// Builds one device from its spec. The engine's seam onto a binary's
/// catalog.
type BuildDevice<'a> = dyn FnMut(&str) -> anyhow::Result<CreatedPciDevice> + 'a;

/// The binary's catalog, as the engine holds it.
pub type HotplugFactory = Box<
    dyn FnMut(&str, &PciDeviceCtx<'_>) -> anyhow::Result<CreatedPciDevice>
        + Send,
>;

/// Everything an add or a removal touches, with no live VM in it.
///
/// Split out from [`HotplugEngine`] because [`OwnedPciCtx`] needs a
/// `Machine`, and this half must stay testable.
struct Core {
    registry: Arc<DeviceRegistry>,
    bus: Arc<PciBus>,
    regs: Arc<PciHotplug>,
    vm: Arc<dyn VmPause>,
    budget: Duration,
    /// How much longer than `budget` one unplug may take, to cover the
    /// unbounded `halt()` at the end of it.
    halt_slack: Duration,
    /// Set by [`HotplugEngine::close`] once the guest has stopped.
    /// Every entry point reads it, the way `VcpuThreads::close` refuses
    /// a late CPU add.
    closed: AtomicBool,
    /// False when the drain thread was refused at start. An add still
    /// works. A removal does not, because nothing would act on the
    /// guest's `_EJ0`.
    drains: AtomicBool,
    log: Logger,
}

impl Core {
    /// Add one device, then tell the guest.
    ///
    /// `build` runs only after the slot is known to be free, so a
    /// refused request opens no file, allocates no memory segment and
    /// takes no MSI-X vector.
    fn add(
        &self,
        spec: &str,
        build: &mut BuildDevice<'_>,
    ) -> Result<String, HotplugError> {
        if self.is_closed() {
            return Err(HotplugError::Closed);
        }
        let bdf = hotplug_bdf(spec)?;
        let id = devspec::device_id(spec);
        self.check_free(&id, bdf)?;

        let (pci, lifecycle) =
            build(spec).map_err(|e| HotplugError::Create(format!("{e:#}")))?;
        // A slot holds a PCI device. Without one there is nothing for
        // the guest's device check to find, so the record would name a
        // slot that reads as empty.
        if pci.is_none() {
            // Dropping the handle does not join a worker, so a backend
            // that got as far as starting one would hold its backing
            // file open for the life of the VM.
            if let Some(lifecycle) = &lifecycle {
                if let Err(e) = prepare_unplug(lifecycle, self.budget) {
                    warn!(self.log, "a refused backend did not stop";
                        "spec" => spec, "error" => format!("{e:?}"));
                }
            }
            return Err(HotplugError::NothingBuilt(
                devspec::parts(spec).driver.to_string(),
            ));
        }

        let mut record = RegisteredDevice::new(
            id.clone(),
            Some(bdf),
            pci.clone(),
            lifecycle.clone(),
            Some(spec.to_string()),
        );
        record.hotpluggable = true;
        if let Err(e) = self.registry.insert(record) {
            // A half-added device is worse than a refused one: roll the
            // whole thing back before the guest is ever told.
            warn!(self.log, "rolling back a hot-add the registry refused";
                "spec" => spec, "error" => %e);
            self.roll_back(bdf, pci.as_ref(), lifecycle.as_ref());
            return Err(HotplugError::Registry(e));
        }

        // Last: the guest must not find the device before the VMM has a
        // record of it.
        self.regs.notify_added(bdf.dev());
        info!(self.log, "device added"; "id" => &id, "bdf" => %bdf);
        Ok(id)
    }

    /// Ask the guest to give a device up. Tears nothing down.
    fn request_remove(&self, id: &str) -> Result<(), HotplugError> {
        if self.is_closed() {
            return Err(HotplugError::Closed);
        }
        if !self.drains.load(Ordering::Acquire) {
            // Marking the slot would ask the guest for a device nothing
            // could then take: it would report remove-pending for the
            // life of the VM and be replayed on the next boot.
            return Err(HotplugError::NotDrained);
        }
        let device = self
            .registry
            .get_by_id(id)
            .ok_or_else(|| HotplugError::NoSuchDevice(id.to_string()))?;
        let Some(bdf) = device.bdf.filter(|_| device.hotpluggable) else {
            return Err(HotplugError::NotHotpluggable(id.to_string()));
        };

        match device.state {
            SlotState::Present => self
                .registry
                .set_state(id, SlotState::Present, SlotState::RemovePending)
                .map_err(HotplugError::Registry)?,
            // The guest may have missed the first request, or its
            // driver may not have been bound yet. Asking again raises
            // the same event for the same slot and changes nothing
            // else.
            SlotState::RemovePending => {}
            state => {
                return Err(HotplugError::NotRemovable {
                    id: id.to_string(),
                    state,
                })
            }
        }

        self.regs.notify_removed(bdf.dev());
        info!(self.log, "device removal requested"; "id" => id, "bdf" => %bdf);
        Ok(())
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Act on every eject the guest has run since the last pass.
    fn drain_ejects(&self) {
        // The guest chooses the moment it runs `_EJ0`, so it can pick
        // the instant the VM powers off. An eject started then would
        // pause, quiesce and halt the same device the teardown sweep is
        // halting, from a second thread.
        if self.is_closed() {
            return;
        }
        for slot in self.regs.take_eject_requests() {
            self.eject(slot);
        }
    }

    /// Finish one eject the guest has already run.
    fn eject(&self, slot: u8) {
        let Some(bdf) = Bdf::new(0, slot, 0) else {
            return;
        };
        let Some(device) = self.registry.get_by_bdf(bdf) else {
            // The register file only reports slots it was told hold a
            // device, so this is a slot whose teardown already ran.
            debug!(self.log, "eject names an empty slot"; "slot" => slot);
            return;
        };

        if device.state != SlotState::RemovePending {
            // The guest ran `_EJ0` on its own. A device leaves only when
            // an operator asks, so put the slot back in front of the
            // guest instead of tearing it out.
            debug!(self.log, "refused an eject no operator asked for";
                "id" => &device.id, "state" => %device.state);
            self.regs.notify_added(slot);
            return;
        }

        let Some(lifecycle) = device.lifecycle.clone() else {
            // passthru has no Lifecycle, so nothing can report that its
            // DMA has stopped. It keeps its slot.
            warn!(self.log, "eject aborted: the device cannot be quiesced";
                "id" => &device.id);
            self.abort_eject(&device.id, slot, None);
            return;
        };

        // Read before the quiesce, not after: `prepare_unplug` pauses
        // the device itself unless it is paused already, so this is the
        // only point that can tell whose pause it is. Resuming one the
        // operator paused would start it behind them, and `Indicator`
        // refuses a resume from any state but `Pause`.
        let paused_here =
            lifecycle.lifecycle_state() != Some(IndicatedState::Pause);

        match unplug_bounded(
            &lifecycle,
            self.budget,
            self.budget.saturating_add(self.halt_slack),
            &self.log,
        ) {
            Unplug::Done => {}
            // Never forced. The device keeps running, keeps its slot and
            // keeps its backing file. `prepare_unplug` already paused
            // it, so the abort has to undo that: a device left paused
            // never serves the guest again, which is a silent hang
            // rather than a refused removal.
            Unplug::Kept { reason, paused } => {
                warn!(self.log, "eject aborted: the device did not quiesce";
                    "id" => &device.id, "error" => reason);
                self.abort_eject(
                    &device.id,
                    slot,
                    (paused_here && paused).then_some(&lifecycle),
                );
                return;
            }

            // The release is still running on a thread this one let go
            // of. Nothing may touch the slot: a worker can still be
            // inside a transport callback on the device that is leaving,
            // and its state reads `Halt` from the first instant of a
            // release that has not happened.
            Unplug::InFlight => {
                error!(self.log, "eject abandoned: the device release did \
                    not return inside its budget";
                    "id" => &device.id, "bdf" => %bdf);
                if let Err(e) = self.registry.set_state(
                    &device.id,
                    SlotState::RemovePending,
                    SlotState::Ejecting,
                ) {
                    error!(self.log, "an abandoned eject left the slot as \
                        it was"; "id" => &device.id, "error" => %e);
                }
                return;
            }
        }

        // The compare-and-swap comes after the quiesce, not before it:
        // `SlotState` has no edge back out of `Ejecting`, so a slot
        // moved first could never be walked back when the quiesce
        // fails. It still guards the one step that cannot be undone,
        // which is the detach below.
        if let Err(e) = self.registry.set_state(
            &device.id,
            SlotState::RemovePending,
            SlotState::Ejecting,
        ) {
            error!(self.log, "eject stopped: the slot moved";
                "id" => &device.id, "error" => %e);
            return;
        }

        if let Err(e) = self.detach_paused(bdf, device.pci.as_ref()) {
            // The slot stays `Ejecting`: the device is quiesced and
            // halted, so it serves the guest no more, but its regions
            // are still registered and a vCPU may be inside one.
            error!(self.log, "eject stopped: the VM would not pause for \
                the bus change"; "id" => &device.id,
                "error" => format!("{e:#}"));
            return;
        }
        if let Err(e) = self.registry.set_state(
            &device.id,
            SlotState::Ejecting,
            SlotState::Absent,
        ) {
            error!(self.log, "eject finished on a slot that moved";
                "id" => &device.id, "error" => %e);
        }
        // Dropping the record drops the last handles, which closes the
        // backing file.
        self.registry.remove_by_id(&device.id);
        info!(self.log, "device removed"; "id" => &device.id, "bdf" => %bdf);
    }

    /// Give a device back after a removal that will not happen.
    ///
    /// The device keeps its slot, so three things have to be undone:
    /// the pause `prepare_unplug` took, the slot state the operator's
    /// request set, and the `ejectable` bit `take_eject_requests`
    /// cleared. Without the last one no later `_EJ0` would be accepted
    /// and the device could never be removed at all.
    fn abort_eject(
        &self,
        id: &str,
        slot: u8,
        resume: Option<&Arc<dyn vmm_devices::Lifecycle>>,
    ) {
        if let Some(lifecycle) = resume {
            lifecycle.resume();
        }
        if let Err(e) = self.registry.set_state(
            id,
            SlotState::RemovePending,
            SlotState::Present,
        ) {
            // Not fatal: the slot is reported as it is, and the device
            // is running either way.
            debug!(self.log, "an abandoned removal left the slot as it was";
                "id" => id, "error" => %e);
        }
        self.regs.notify_added(slot);
    }

    /// Refuse a slot or an id that is already in use.
    ///
    /// The bus is checked as well as the registry: the chipset attaches
    /// devices the registry never sees.
    fn check_free(&self, id: &str, bdf: Bdf) -> Result<(), HotplugError> {
        if self.registry.get_by_id(id).is_some() {
            return Err(HotplugError::DuplicateId(id.to_string()));
        }
        if self.registry.get_by_bdf(bdf).is_some() {
            return Err(HotplugError::SlotTaken(bdf));
        }
        if self.bus.config_read(&bdf, 0, 4) != NO_DEVICE {
            return Err(HotplugError::SlotTaken(bdf));
        }
        Ok(())
    }

    /// Undo a hot-add that got as far as building the device.
    ///
    /// The workers are stopped before the bus handles go, so the last
    /// reference really is the last one and the backing file closes.
    fn roll_back(
        &self,
        bdf: Bdf,
        pci: Option<&PciDeviceHandle>,
        lifecycle: Option<&Arc<dyn vmm_devices::Lifecycle>>,
    ) {
        if let Some(lifecycle) = lifecycle {
            if let Err(e) = prepare_unplug(lifecycle, self.budget) {
                // The device is new and the guest was never told about
                // it, so nothing is using it. Report it and carry on:
                // leaving it on the bus with no record is worse.
                warn!(self.log, "a rolled-back device did not quiesce";
                    "bdf" => %bdf, "error" => format!("{e:?}"));
            }
        }
        if let Err(e) = self.detach_paused(bdf, pci) {
            error!(self.log, "a rolled-back device is still on the bus";
                "bdf" => %bdf, "error" => format!("{e:#}"));
        }
    }

    /// Take a device off the bus with no vCPU inside a bus handler.
    ///
    /// The pause is what makes the region unregister safe: a vCPU can
    /// be part-way through a BAR access on the device that is leaving,
    /// so a pause that fails aborts the detach rather than stripping a
    /// live device. The pause is a [`VmPauseGate`] hold, so an operator
    /// pause already in force is not a failure and is not resumed here.
    fn detach_paused(
        &self,
        bdf: Bdf,
        pci: Option<&PciDeviceHandle>,
    ) -> anyhow::Result<()> {
        self.vm.pause().map_err(|e| {
            e.context(format!("no pause for the bus change at {bdf}"))
        })?;

        if let Some(device) = pci {
            device.detach_regions();
        }
        // The bus hands its handle back. Dropping it here is what
        // releases the reference the bus held.
        drop(self.bus.detach(&bdf));

        if let Err(e) = self.vm.resume() {
            // The regions are already gone, so the change stands. A VM
            // that will not resume is a kernel level failure this path
            // cannot undo.
            error!(self.log, "failed to resume the VM after a bus change";
                "bdf" => %bdf, "error" => format!("{e:#}"));
        }
        Ok(())
    }
}

/// The add and remove engine, and the thread that owns teardown.
pub struct HotplugEngine {
    core: Arc<Core>,
    /// The pause the bus change takes. Held here so the control plane
    /// can take the same one.
    gate: Arc<VmPauseGate>,
    ctx: Arc<OwnedPciCtx>,
    /// The binary's catalog. `Mutex` because it is `FnMut` and two
    /// control connections can ask at once.
    factory: Mutex<HotplugFactory>,
    thread: DrainThread,
}

impl HotplugEngine {
    /// Start the engine and its drain thread.
    ///
    /// `factory` must be the same catalog closure the boot-time `-s`
    /// pass uses, so a control request can express nothing that argv
    /// cannot.
    pub fn start(
        registry: Arc<DeviceRegistry>,
        ctx: Arc<OwnedPciCtx>,
        pci_regs: Arc<PciHotplug>,
        factory: HotplugFactory,
        log: Logger,
    ) -> Arc<Self> {
        // The bus and the VM handle come out of the context the catalog
        // builds against, so a device can never be detached from a bus
        // other than the one it was attached to.
        let (bus, gate) = {
            let borrowed = PciDeviceCtx::borrow_from(&ctx);
            (
                Arc::clone(borrowed.chipset.pci_bus()),
                VmPauseGate::over_hdl(Arc::clone(borrowed.vmm_hdl)),
            )
        };

        let core = Arc::new(Core {
            registry,
            bus,
            regs: pci_regs,
            vm: Arc::clone(&gate) as Arc<dyn VmPause>,
            budget: UNPLUG_BUDGET,
            halt_slack: UNPLUG_HALT_SLACK,
            closed: AtomicBool::new(false),
            drains: AtomicBool::new(true),
            log: log.clone(),
        });
        let thread = DrainThread::spawn(
            "hotplug",
            &core,
            Core::drain_ejects,
            &log,
            "no hotplug thread; ejects will not be drained",
        );
        if thread.is_stopped() {
            core.drains.store(false, Ordering::Release);
        }

        Arc::new(Self {
            core,
            gate,
            ctx,
            factory: Mutex::new(factory),
            thread,
        })
    }

    /// Add one `slot,driver[,config]` device and return its id.
    pub fn add_device(&self, spec: &str) -> Result<String, HotplugError> {
        // No panic on a control path: the factory is a closure, and a
        // panic inside it cannot leave a half-written value behind.
        let mut factory = lock(&self.factory);
        let ctx = PciDeviceCtx::borrow_from(&self.ctx);
        self.core.add(spec, &mut |spec| factory(spec, &ctx))
    }

    /// Ask the guest to give a device up. Returns as soon as the guest
    /// has been asked.
    pub fn request_remove(&self, id: &str) -> Result<(), HotplugError> {
        self.core.request_remove(id)
    }

    /// The pause this engine takes when it changes the bus.
    ///
    /// The kernel keeps one pause flag per instance, so every userspace
    /// path that stops the whole VM has to share this count.
    pub fn pause_gate(&self) -> Arc<VmPauseGate> {
        Arc::clone(&self.gate)
    }

    /// Refuse every later add and removal, and stop the drain thread.
    ///
    /// Teardown calls this before it takes its list of devices. An add
    /// that landed after that list attaches a device the halt sweep
    /// never sees, and the `vmm_drv` lease it holds then parks
    /// `VM_DESTROY_SELF` in `vmm_lease_block` until the watchdog ends
    /// the process.
    pub fn close(&self) {
        self.core.closed.store(true, Ordering::Release);
        self.thread.shutdown(DRAIN_JOIN_BUDGET);
    }

    /// Stop the drain thread and wait for it.
    pub fn shutdown(&self) {
        self.thread.shutdown(DRAIN_JOIN_BUDGET);
    }
}

impl Drop for HotplugEngine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Take a lock, tolerating poison.
///
/// Every value behind these locks is one field that a panic elsewhere
/// cannot leave half written, and a hotplug request must not take the
/// VM down.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The address a hot-add spec names, if a hot-add can use it.
///
/// Rejects everything the PCI hotplug register file cannot describe:
/// its registers hold one bit per slot on bus 0, and the AML gives an
/// `_EJ0` method to function 0 alone.
fn hotplug_bdf(spec: &str) -> Result<Bdf, HotplugError> {
    let devspec::SpecParts { slot, driver, .. } = devspec::parts(spec);
    if driver.is_empty() {
        return Err(HotplugError::BadSpec(format!(
            "'{spec}' needs slot,driver[,config]"
        )));
    }
    let bdf = parse_bdf(slot).ok_or_else(|| {
        HotplugError::BadSpec(format!("'{slot}' is not a PCI address"))
    })?;
    if bdf.bus() != 0 || bdf.func() != 0 {
        return Err(HotplugError::BadSpec(format!(
            "hot-add takes bus 0 function 0, not '{slot}'"
        )));
    }
    if !is_hotpluggable_slot(bdf.dev()) {
        return Err(HotplugError::FixedSlot(bdf.dev()));
    }
    Ok(bdf)
}

/// The `-s` specs a restart has to replay.
///
/// Only hot-added devices: an argv device already has its own `-s`
/// entry, complete with the bootindex the registry does not keep.
///
/// A slot whose teardown has begun is dropped. `Absent` never reaches
/// here, because `eject` sets it and removes the record on the same
/// thread, so `Ejecting` is the state that has to be filtered: the
/// device is gone or going, and replaying its spec would open its
/// backing file again in the next run.
pub fn hotplug_specs(registry: &DeviceRegistry) -> Vec<String> {
    registry
        .list()
        .into_iter()
        .filter(|device| {
            device.hotpluggable
                && !matches!(
                    device.state,
                    SlotState::Absent | SlotState::Ejecting
                )
        })
        .filter_map(|device| device.spec)
        .collect()
}

/// The CPU hot-add engine and the CPU set it adds to.
///
/// The engine answers an add and nothing else, so a control plane that
/// has to report which CPUs are running reads the set. Both are held
/// here, so no caller can hold one without the other and describe a
/// machine that does not exist.
///
/// The set is the [`CpuOnline`] trait and not [`VcpuRegistry`] itself,
/// for the reason the engine gives: every step of a real add is an
/// ioctl, so a control plane can only be tested against a stand-in.
#[derive(Clone)]
pub struct CpuSlots {
    engine: Arc<CpuHotplugEngine>,
    vcpus: Arc<dyn CpuOnline>,
}

impl CpuSlots {
    pub fn new(
        engine: Arc<CpuHotplugEngine>,
        vcpus: Arc<dyn CpuOnline>,
    ) -> Self {
        Self { engine, vcpus }
    }

    /// The same, over the registry a running VM has.
    pub fn from_registry(
        engine: Arc<CpuHotplugEngine>,
        vcpus: Arc<VcpuRegistry>,
    ) -> Self {
        Self::new(engine, vcpus as Arc<dyn CpuOnline>)
    }

    /// Bring one CPU online on the running VM.
    pub fn add(&self, id: u32) -> Result<(), CpuHotplugError> {
        self.engine.add_cpu(id)
    }

    /// How many CPUs the boot path brought online, ids 0 upward.
    pub fn boot_cpus(&self) -> u32 {
        self.vcpus.boot_cpus()
    }

    /// Every CPU slot the tables describe, boot CPUs included.
    pub fn max_cpus(&self) -> u32 {
        self.vcpus.possible_cpus()
    }

    /// Whether the CPU in `id` is running.
    pub fn is_online(&self, id: u32) -> bool {
        self.vcpus.is_online(id)
    }

    /// The ids a failed add spent. There is no `vm_deactivate_cpu`, so
    /// no later add can take one back.
    pub fn consumed(&self) -> Vec<u32> {
        self.engine.consumed_cpus()
    }

    fn shutdown(&self) {
        self.engine.shutdown();
    }
}

/// Start the CPU hot-add engine when the VM has the register file for
/// one. The registry it needs is built from the boot fleet.
pub fn start_cpu_hotplug(
    regs: Option<&crate::pm::HotplugRegisters>,
    setup: VcpuSetup,
    fleet: &VcpuFleet,
    log: &Logger,
) -> Option<CpuSlots> {
    let regs = regs.and_then(|regs| regs.cpu.clone())?;
    let vcpus = Arc::new(VcpuRegistry::from_fleet(
        setup,
        fleet,
        log.new(slog::o!("component" => "vcpus")),
    ));
    let engine = CpuHotplugEngine::start(
        Arc::clone(&vcpus),
        regs,
        log.new(slog::o!("component" => "hotplug-cpu")),
    );
    Some(CpuSlots::from_registry(engine, vcpus))
}

/// Every hot-add engine one VM has.
///
/// `None` means the VM was started without the option that engine
/// needs, so a request for it is refused and never queued. The two
/// binaries build this the same way, which is why it lives here and
/// not in either control plane.
#[derive(Clone, Default)]
pub struct HotplugEngines {
    /// PCI add and remove. Needs `--hotplug`.
    pub pci: Option<Arc<HotplugEngine>>,
    /// CPU add. Needs `--hotplug` and a `maxcpus` above the boot count.
    pub cpu: Option<CpuSlots>,
    /// Memory add. Needs `--hotplug` and `-o hotplug.maxmem`.
    pub mem: Option<Arc<MemHotplugEngine>>,
}

impl HotplugEngines {
    /// The pause every path that stops the whole VM must take.
    ///
    /// The PCI engine's gate when the VM has one, because that engine is
    /// the only other userspace owner of the kernel's single pause flag.
    /// Without the engine there is no second owner, so a fresh gate over
    /// the handle is the whole count.
    pub fn pause_gate(
        &self,
        hdl: &Arc<vmm_core::hdl::VmmHdl>,
    ) -> Arc<VmPauseGate> {
        match self.pci.as_ref() {
            Some(pci) => pci.pause_gate(),
            None => VmPauseGate::over_hdl(Arc::clone(hdl)),
        }
    }

    /// Refuse every later request and stop every drain thread.
    ///
    /// Called from inside the event loop, before teardown takes its
    /// list of devices. A request that arrives after this finds every
    /// engine closed: PCI on its own flag, CPU on the vCPU roster.
    pub fn close(&self) {
        if let Some(pci) = self.pci.as_ref() {
            pci.close();
        }
        if let Some(cpu) = self.cpu.as_ref() {
            cpu.shutdown();
        }
        if let Some(mem) = self.mem.as_ref() {
            mem.shutdown();
        }
    }

    /// Stop every drain thread and wait for it.
    pub fn shutdown(&self) {
        if let Some(pci) = self.pci.as_ref() {
            pci.shutdown();
        }
        if let Some(cpu) = self.cpu.as_ref() {
            cpu.shutdown();
        }
        if let Some(mem) = self.mem.as_ref() {
            mem.shutdown();
        }
    }
}

pub mod cpu;
mod drain;
pub mod mem;
mod unplug;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod engine_tests {
    use super::*;

    #[test]
    fn engines_a_vm_did_not_ask_for_are_absent() {
        // The default. Every control plane refuses a request rather
        // than queueing it for an engine that does not exist.
        let engines = HotplugEngines::default();

        assert!(engines.pci.is_none());
        assert!(engines.cpu.is_none());
        assert!(engines.mem.is_none());
        // A shutdown of nothing must be quiet, because teardown runs it
        // on every VM.
        engines.shutdown();
    }
}
