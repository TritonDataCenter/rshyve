// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Run-time device registry.
//!
//! One shared record of the device set. Hotplug needs a record that a
//! control request can add to and remove from while the guest runs.

use std::fmt;
use std::sync::{Arc, RwLock};

use vmm_devices::pci::{Bdf, PciDevice};
use vmm_devices::Lifecycle;

/// Where a slot is in the eject sequence.
///
/// An operator request marks the slot `RemovePending`, the guest
/// acknowledges by starting the eject, and the slot reaches `Absent`
/// when the device is detached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    Present,
    RemovePending,
    Ejecting,
    Absent,
}

impl SlotState {
    /// True for the legal steps of the eject sequence.
    ///
    /// `Present` -> `RemovePending` -> `Ejecting` -> `Absent`, one step
    /// at a time, plus `RemovePending` -> `Present` for a removal that
    /// was abandoned. Every other pair is refused, a repeat of the
    /// current state included, so a guest that writes the eject
    /// register twice cannot start two teardowns of one device: only
    /// `RemovePending` -> `Ejecting` reaches teardown, and a slot leaves
    /// `RemovePending` once.
    fn can_advance_to(self, next: SlotState) -> bool {
        matches!(
            (self, next),
            (SlotState::Present, SlotState::RemovePending)
                | (SlotState::RemovePending, SlotState::Present)
                | (SlotState::RemovePending, SlotState::Ejecting)
                | (SlotState::Ejecting, SlotState::Absent)
        )
    }
}

impl fmt::Display for SlotState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            SlotState::Present => "present",
            SlotState::RemovePending => "remove-pending",
            SlotState::Ejecting => "ejecting",
            SlotState::Absent => "absent",
        };
        f.write_str(name)
    }
}

/// Reason a registry request was refused.
///
/// Hand-written rather than derived: this crate carries no `thiserror`
/// dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// Another device holds this id.
    DuplicateId(String),
    /// Another device holds this BDF.
    DuplicateBdf(Bdf),
    /// No device has this id.
    NoSuchDevice(String),
    /// The slot moved between the read and the write.
    StateMismatch {
        id: String,
        expected: SlotState,
        found: SlotState,
    },
    /// The eject sequence has no such step.
    IllegalTransition {
        id: String,
        from: SlotState,
        to: SlotState,
    },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegistryError::DuplicateId(id) => {
                write!(f, "device id {id} is already registered")
            }
            RegistryError::DuplicateBdf(bdf) => {
                write!(f, "a device is already registered at BDF {bdf}")
            }
            RegistryError::NoSuchDevice(id) => {
                write!(f, "no device with id {id}")
            }
            RegistryError::StateMismatch {
                id,
                expected,
                found,
            } => {
                write!(f, "device {id} is {found}, not the expected {expected}")
            }
            RegistryError::IllegalTransition { id, from, to } => {
                write!(f, "device {id} cannot move from {from} to {to}")
            }
        }
    }
}

impl std::error::Error for RegistryError {}

/// One device in the machine.
///
/// All four handle shapes are legal: a PCI device with no `Lifecycle` is
/// passthru, and a `Lifecycle` with no PCI handle is a backend such as
/// the UEFI variable store.
#[derive(Clone)]
pub struct RegisteredDevice {
    /// Stable, unique and operator-facing.
    pub id: String,
    /// `None` for a device that is not on the PCI bus.
    pub bdf: Option<Bdf>,
    pub pci: Option<Arc<dyn PciDevice>>,
    pub lifecycle: Option<Arc<dyn Lifecycle>>,
    /// The `-s` spec that created the device. Reboot rebuilds its argv
    /// from these.
    pub spec: Option<String>,
    pub hotpluggable: bool,
    pub state: SlotState,
}

impl RegisteredDevice {
    /// A present, not hot-pluggable device, the shape the boot path
    /// creates.
    pub fn new(
        id: impl Into<String>,
        bdf: Option<Bdf>,
        pci: Option<Arc<dyn PciDevice>>,
        lifecycle: Option<Arc<dyn Lifecycle>>,
        spec: Option<String>,
    ) -> Self {
        Self {
            id: id.into(),
            bdf,
            pci,
            lifecycle,
            spec,
            hotpluggable: false,
            state: SlotState::Present,
        }
    }
}

impl fmt::Debug for RegisteredDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The handles are trait objects with no Debug, so report only
        // whether they are there.
        f.debug_struct("RegisteredDevice")
            .field("id", &self.id)
            .field("bdf", &self.bdf)
            .field("pci", &self.pci.is_some())
            .field("lifecycle", &self.lifecycle.is_some())
            .field("spec", &self.spec)
            .field("hotpluggable", &self.hotpluggable)
            .field("state", &self.state)
            .finish()
    }
}

/// Every device in the machine, in one place, mutable at run time.
///
/// # Locking
///
/// The lock order for the whole codebase is: registry -> chipset/PciBus
/// -> buses -> physmap. Take the locks in that order, never the reverse.
///
/// No accessor holds the registry lock across a call into a device.
/// Every accessor that returns handles clones them out and drops the
/// lock first. A device call can re-enter the registry, an eject
/// completion for one, and a held lock would deadlock it.
pub struct DeviceRegistry {
    inner: RwLock<Vec<RegisteredDevice>>,
}

impl Default for DeviceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceRegistry {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Vec::new()),
        }
    }

    /// Register a device, refusing a duplicate id or BDF.
    pub fn insert(
        &self,
        device: RegisteredDevice,
    ) -> Result<(), RegistryError> {
        let mut devices =
            self.inner.write().expect("DeviceRegistry lock poisoned");
        if devices.iter().any(|d| d.id == device.id) {
            return Err(RegistryError::DuplicateId(device.id));
        }
        if let Some(bdf) = device.bdf {
            if devices.iter().any(|d| d.bdf == Some(bdf)) {
                return Err(RegistryError::DuplicateBdf(bdf));
            }
        }
        devices.push(device);
        Ok(())
    }

    /// Remove a device and return it, so the caller owns the last
    /// handles after the lock is dropped.
    pub fn remove_by_id(&self, id: &str) -> Option<RegisteredDevice> {
        let mut devices =
            self.inner.write().expect("DeviceRegistry lock poisoned");
        let index = devices.iter().position(|d| d.id == id)?;
        Some(devices.remove(index))
    }

    pub fn get_by_id(&self, id: &str) -> Option<RegisteredDevice> {
        let devices = self.inner.read().expect("DeviceRegistry lock poisoned");
        devices.iter().find(|d| d.id == id).cloned()
    }

    pub fn get_by_bdf(&self, bdf: Bdf) -> Option<RegisteredDevice> {
        let devices = self.inner.read().expect("DeviceRegistry lock poisoned");
        devices.iter().find(|d| d.bdf == Some(bdf)).cloned()
    }

    /// Advance one slot through the eject sequence.
    ///
    /// A compare-and-swap: the write lands only when the slot still
    /// holds `expected` and the step is legal. A second eject request
    /// for one device is therefore a no-op, not a second teardown.
    pub fn set_state(
        &self,
        id: &str,
        expected: SlotState,
        next: SlotState,
    ) -> Result<(), RegistryError> {
        let mut devices =
            self.inner.write().expect("DeviceRegistry lock poisoned");
        let device = devices
            .iter_mut()
            .find(|d| d.id == id)
            .ok_or_else(|| RegistryError::NoSuchDevice(id.to_string()))?;
        if device.state != expected {
            return Err(RegistryError::StateMismatch {
                id: device.id.clone(),
                expected,
                found: device.state,
            });
        }
        if !expected.can_advance_to(next) {
            return Err(RegistryError::IllegalTransition {
                id: device.id.clone(),
                from: expected,
                to: next,
            });
        }
        device.state = next;
        Ok(())
    }

    /// Snapshot of the lifecycle handles.
    ///
    /// A clone, so the caller drives pause, quiesce and flush with no
    /// registry lock held.
    pub fn lifecycle_devices(&self) -> Vec<Arc<dyn Lifecycle>> {
        let devices = self.inner.read().expect("DeviceRegistry lock poisoned");
        devices.iter().filter_map(|d| d.lifecycle.clone()).collect()
    }

    /// Snapshot of the PCI handles, cloned for the same reason.
    pub fn pci_devices(&self) -> Vec<Arc<dyn PciDevice>> {
        let devices = self.inner.read().expect("DeviceRegistry lock poisoned");
        devices.iter().filter_map(|d| d.pci.clone()).collect()
    }

    /// Read-only view of every slot, in insert order.
    pub fn list(&self) -> Vec<RegisteredDevice> {
        let devices = self.inner.read().expect("DeviceRegistry lock poisoned");
        devices.clone()
    }

    pub fn len(&self) -> usize {
        self.inner
            .read()
            .expect("DeviceRegistry lock poisoned")
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use vmm_core::common::RWOp;
    use vmm_devices::pci::BarN;

    use super::{
        Bdf, DeviceRegistry, Lifecycle, PciDevice, RegisteredDevice,
        RegistryError, SlotState,
    };
    use std::sync::Arc;

    struct StubPci;

    impl PciDevice for StubPci {
        fn cfg_read(&self, _offset: u8, _len: u8) -> u32 {
            0
        }
        fn cfg_write(&self, _offset: u8, _len: u8, _val: u32) {}
        fn bar_rw(&self, _bar: BarN, _offset: usize, _rwo: RWOp<'_>) {}
    }

    struct StubLifecycle;

    impl Lifecycle for StubLifecycle {
        fn type_name(&self) -> &'static str {
            "stub"
        }
    }

    fn pci() -> Arc<dyn PciDevice> {
        Arc::new(StubPci)
    }

    fn lifecycle() -> Arc<dyn Lifecycle> {
        Arc::new(StubLifecycle)
    }

    fn bdf(dev: u8) -> Bdf {
        Bdf::new(0, dev, 0).expect("device number is in range")
    }

    fn device(id: &str, dev: u8) -> RegisteredDevice {
        RegisteredDevice::new(
            id,
            Some(bdf(dev)),
            Some(pci()),
            Some(lifecycle()),
            Some(format!("{dev},stub")),
        )
    }

    const STATES: [SlotState; 4] = [
        SlotState::Present,
        SlotState::RemovePending,
        SlotState::Ejecting,
        SlotState::Absent,
    ];

    #[test]
    fn insert_rejects_a_duplicate_id() {
        let registry = DeviceRegistry::new();
        registry.insert(device("blk@4", 4)).expect("first insert");

        let err = registry
            .insert(device("blk@4", 5))
            .expect_err("the id is taken");

        assert_eq!(err, RegistryError::DuplicateId("blk@4".to_string()));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn insert_rejects_a_duplicate_bdf() {
        let registry = DeviceRegistry::new();
        registry.insert(device("blk@4", 4)).expect("first insert");

        let err = registry
            .insert(device("net@4", 4))
            .expect_err("the BDF is taken");

        assert_eq!(err, RegistryError::DuplicateBdf(bdf(4)));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn devices_without_a_bdf_do_not_collide() {
        // None is "not on the PCI bus", not one shared address.
        let registry = DeviceRegistry::new();
        for id in ["varstore", "tpm"] {
            registry
                .insert(RegisteredDevice::new(
                    id,
                    None,
                    None,
                    Some(lifecycle()),
                    None,
                ))
                .expect("no BDF to collide");
        }
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn a_lifecycle_without_a_pci_handle_is_registered() {
        // The varstore shape. Dropping it here is why rshyve has to push
        // the handle by hand.
        let registry = DeviceRegistry::new();
        registry
            .insert(RegisteredDevice::new(
                "varstore",
                None,
                None,
                Some(lifecycle()),
                None,
            ))
            .expect("insert varstore");

        assert_eq!(registry.lifecycle_devices().len(), 1);
        assert!(registry.pci_devices().is_empty());
        assert!(registry.get_by_id("varstore").is_some());
    }

    #[test]
    fn passthru_registers_a_pci_handle_with_no_lifecycle() {
        let registry = DeviceRegistry::new();
        registry
            .insert(RegisteredDevice::new(
                "passthru@6",
                Some(bdf(6)),
                Some(pci()),
                None,
                Some("6,passthru".to_string()),
            ))
            .expect("insert passthru");

        assert_eq!(registry.pci_devices().len(), 1);
        assert!(registry.lifecycle_devices().is_empty());
    }

    #[test]
    fn lookup_by_id_and_bdf_finds_the_same_slot() {
        let registry = DeviceRegistry::new();
        registry.insert(device("blk@4", 4)).expect("insert");

        let by_id = registry.get_by_id("blk@4").expect("id lookup");
        let by_bdf = registry.get_by_bdf(bdf(4)).expect("BDF lookup");

        assert_eq!(by_id.id, by_bdf.id);
        assert_eq!(by_id.spec.as_deref(), Some("4,stub"));
        assert!(registry.get_by_id("absent").is_none());
        assert!(registry.get_by_bdf(bdf(9)).is_none());
    }

    #[test]
    fn remove_frees_the_id_and_the_bdf() {
        let registry = DeviceRegistry::new();
        registry.insert(device("blk@4", 4)).expect("insert");

        let removed = registry.remove_by_id("blk@4").expect("remove");

        assert_eq!(removed.id, "blk@4");
        assert!(registry.is_empty());
        assert!(registry.remove_by_id("blk@4").is_none());
        registry
            .insert(device("blk@4", 4))
            .expect("the id and BDF are free again");
    }

    #[test]
    fn the_legal_state_walk_runs_to_absent() {
        let registry = DeviceRegistry::new();
        registry.insert(device("blk@4", 4)).expect("insert");

        for pair in STATES.windows(2) {
            registry
                .set_state("blk@4", pair[0], pair[1])
                .expect("each step of the eject sequence is legal");
            assert_eq!(
                registry.get_by_id("blk@4").expect("device").state,
                pair[1]
            );
        }
    }

    #[test]
    fn an_abandoned_removal_walks_back_to_present() {
        // A device that will not quiesce keeps its slot and keeps
        // running, so the slot must stop reporting a removal that is
        // not going to happen.
        let registry = DeviceRegistry::new();
        registry.insert(device("blk@4", 4)).expect("insert");
        registry
            .set_state("blk@4", SlotState::Present, SlotState::RemovePending)
            .expect("requested");

        registry
            .set_state("blk@4", SlotState::RemovePending, SlotState::Present)
            .expect("the removal was abandoned");

        assert_eq!(
            registry.get_by_id("blk@4").expect("device").state,
            SlotState::Present,
        );
        // And the operator can ask again.
        registry
            .set_state("blk@4", SlotState::Present, SlotState::RemovePending)
            .expect("a second request");
    }

    #[test]
    fn every_other_transition_is_refused() {
        for from in STATES {
            for to in STATES {
                let legal = matches!(
                    (from, to),
                    (SlotState::Present, SlotState::RemovePending)
                        | (SlotState::RemovePending, SlotState::Present)
                        | (SlotState::RemovePending, SlotState::Ejecting)
                        | (SlotState::Ejecting, SlotState::Absent)
                );
                if legal {
                    continue;
                }

                let registry = DeviceRegistry::new();
                let mut dev = device("blk@4", 4);
                dev.state = from;
                registry.insert(dev).expect("insert");

                let err = registry
                    .set_state("blk@4", from, to)
                    .expect_err("only the eject sequence is legal");

                assert_eq!(
                    err,
                    RegistryError::IllegalTransition {
                        id: "blk@4".to_string(),
                        from,
                        to,
                    }
                );
                assert_eq!(
                    registry.get_by_id("blk@4").expect("device").state,
                    from
                );
            }
        }
    }

    #[test]
    fn a_second_eject_request_is_a_no_op() {
        // A hostile guest writing the eject register twice must not
        // start a second teardown.
        let registry = DeviceRegistry::new();
        registry.insert(device("blk@4", 4)).expect("insert");
        registry
            .set_state("blk@4", SlotState::Present, SlotState::RemovePending)
            .expect("first request");

        let err = registry
            .set_state("blk@4", SlotState::Present, SlotState::RemovePending)
            .expect_err("the slot already moved");

        assert_eq!(
            err,
            RegistryError::StateMismatch {
                id: "blk@4".to_string(),
                expected: SlotState::Present,
                found: SlotState::RemovePending,
            }
        );
        assert_eq!(
            registry.get_by_id("blk@4").expect("device").state,
            SlotState::RemovePending
        );
    }

    #[test]
    fn set_state_reports_an_unknown_id() {
        let registry = DeviceRegistry::new();

        let err = registry
            .set_state("ghost", SlotState::Present, SlotState::RemovePending)
            .expect_err("no such device");

        assert_eq!(err, RegistryError::NoSuchDevice("ghost".to_string()));
    }

    #[test]
    fn list_keeps_insert_order() {
        // PCI enumeration order follows slot order, so the view must not
        // reorder.
        let registry = DeviceRegistry::new();
        for (id, dev) in [("a@4", 4), ("b@5", 5), ("c@6", 6)] {
            registry.insert(device(id, dev)).expect("insert");
        }

        let ids: Vec<String> =
            registry.list().into_iter().map(|d| d.id).collect();

        assert_eq!(ids, ["a@4", "b@5", "c@6"]);
    }

    #[test]
    fn concurrent_insert_and_remove_do_not_deadlock() {
        let registry = Arc::new(DeviceRegistry::new());
        let (tx, rx) = mpsc::channel();
        let worker = Arc::clone(&registry);

        thread::Builder::new()
            .name("registry-churn".into())
            .spawn(move || {
                let handles: Vec<_> = (0u8..8)
                    .map(|slot| {
                        let registry = Arc::clone(&worker);
                        thread::spawn(move || {
                            let id = format!("dev@{slot}");
                            for _ in 0..200 {
                                let inserted =
                                    registry.insert(device(&id, slot));
                                // Readers run against a moving set.
                                drop(registry.lifecycle_devices());
                                drop(registry.pci_devices());
                                drop(registry.get_by_bdf(bdf(slot)));
                                if inserted.is_ok() {
                                    assert!(registry
                                        .remove_by_id(&id)
                                        .is_some());
                                }
                            }
                        })
                    })
                    .collect();
                for handle in handles {
                    handle.join().expect("churn thread");
                }
                // Only reached when every worker joined, so the test
                // thread is still waiting on the receiver.
                tx.send(()).expect("the test holds the receiver");
            })
            .expect("spawn churn driver");

        assert!(
            rx.recv_timeout(Duration::from_secs(30)).is_ok(),
            "registry churn deadlocked"
        );
        assert!(registry.is_empty());
    }
}
