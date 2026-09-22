// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PCI device attachment seam.
//!
//! The machine layer must attach devices without naming one. Each binary
//! supplies its own catalog closure. This module supplies the plumbing
//! that closure needs and the loop that drives it.

use std::sync::Arc;

use slog::{error, Logger};

use vmm_core::hdl::VmmHdl;
use vmm_core::machine::Machine;
use vmm_core::mem::{PhysMap, SegidAlloc};
use vmm_core::mmio::MmioBus;
use vmm_core::pio::PioBus;
use vmm_devices::chipset::i440fx::I440FxChipset;
use vmm_devices::InputBroker;

use crate::devspec;
use crate::parse::parse_bdf;
use crate::registry::{DeviceRegistry, RegisteredDevice, RegistryError};

pub type PciDeviceHandle = Arc<dyn vmm_devices::pci::PciDevice>;
pub type LifecycleHandle = Arc<dyn vmm_devices::Lifecycle>;

/// One device produced by a catalog closure.
///
/// All four shapes are legal. `(Some, None)` is passthru, which carries no
/// `Lifecycle`. `(None, None)` is a driver the chipset already created,
/// such as hostbridge or lpc.
pub type CreatedPciDevice = (Option<PciDeviceHandle>, Option<LifecycleHandle>);

/// The pair for a device that is both on the bus and in the lifecycle
/// sweep, which is every emulated device.
pub fn created<D>(dev: Arc<D>) -> CreatedPciDevice
where
    D: vmm_devices::pci::PciDevice + vmm_devices::Lifecycle,
{
    (
        Some(Arc::clone(&dev) as PciDeviceHandle),
        Some(dev as LifecycleHandle),
    )
}

/// A binary's device catalog. `FnMut` so the closure can hold state.
/// `dyn` so this module stays object-safe and is not monomorphized once
/// per binary.
pub type PciDeviceFactory<'a> =
    dyn FnMut(&str, &PciDeviceCtx<'_>) -> anyhow::Result<CreatedPciDevice> + 'a;

/// Machine-owned plumbing a device catalog needs.
///
/// Carries no CLI config. `num_vcpus`, `vm_name` and the
/// migration-destination flag are captured by each binary's closure, so
/// this layer stays ignorant of `-s` grammar.
pub struct PciDeviceCtx<'a> {
    pub chipset: &'a I440FxChipset,
    pub bus_pio: &'a Arc<PioBus>,
    pub bus_mmio: &'a Arc<MmioBus>,
    pub physmap: &'a Arc<PhysMap>,
    pub vmm_hdl: &'a Arc<VmmHdl>,
    pub segids: &'a SegidAlloc,
    pub input: &'a Arc<InputBroker>,
    pub log: &'a Logger,
}

impl<'a> PciDeviceCtx<'a> {
    pub fn new(
        machine: &'a Machine,
        chipset: &'a I440FxChipset,
        input: &'a Arc<InputBroker>,
        log: &'a Logger,
    ) -> Self {
        Self {
            chipset,
            bus_pio: machine.bus_pio(),
            bus_mmio: machine.bus_mmio(),
            physmap: machine.physmap(),
            vmm_hdl: machine.hdl(),
            segids: machine.segids(),
            input,
            log,
        }
    }

    /// Borrow an [`OwnedPciCtx`], so a catalog closure keeps its shape.
    pub fn borrow_from(owned: &'a OwnedPciCtx) -> Self {
        Self {
            chipset: &owned.chipset,
            bus_pio: &owned.bus_pio,
            bus_mmio: &owned.bus_mmio,
            physmap: &owned.physmap,
            vmm_hdl: &owned.vmm_hdl,
            segids: &owned.segids,
            input: &owned.input,
            log: &owned.log,
        }
    }
}

/// The same plumbing, owned.
///
/// [`PciDeviceCtx`] borrows a `Machine` that lives on the startup stack,
/// so no device can be built after startup returns. This form outlives
/// that stack frame and hands out a borrowed context on demand.
pub struct OwnedPciCtx {
    chipset: Arc<I440FxChipset>,
    bus_pio: Arc<PioBus>,
    bus_mmio: Arc<MmioBus>,
    physmap: Arc<PhysMap>,
    vmm_hdl: Arc<VmmHdl>,
    segids: Arc<SegidAlloc>,
    input: Arc<InputBroker>,
    log: Logger,
}

impl OwnedPciCtx {
    pub fn new(
        machine: &Machine,
        chipset: &Arc<I440FxChipset>,
        input: &Arc<InputBroker>,
        log: &Logger,
    ) -> Self {
        Self {
            chipset: chipset.clone(),
            bus_pio: machine.bus_pio().clone(),
            bus_mmio: machine.bus_mmio().clone(),
            physmap: machine.physmap().clone(),
            vmm_hdl: machine.hdl().clone(),
            segids: machine.segids().clone(),
            input: input.clone(),
            log: log.clone(),
        }
    }
}

/// Handles collected from one pass over the device specs.
///
/// Carries no AHCI-CD flag: that is computed by re-parsing the spec
/// string for a driver name, which device-agnostic plumbing cannot do.
#[derive(Default)]
pub struct PciAttachment {
    /// Every device the pass created, addressable by id and BDF at run
    /// time. The one record the binaries read: a boot-time snapshot
    /// cannot show a device that hotplug added or removed.
    pub registry: Arc<DeviceRegistry>,
}

fn push_created(
    spec: &str,
    created: CreatedPciDevice,
    out: &mut PciAttachment,
) -> Result<(), RegistryError> {
    let (pci, lifecycle) = created;
    // hostbridge and lpc: the chipset already created them, so there is
    // nothing to hold.
    if pci.is_none() && lifecycle.is_none() {
        return Ok(());
    }

    out.registry.insert(RegisteredDevice::new(
        devspec::device_id(spec),
        parse_bdf(devspec::parts(spec).slot),
        pci,
        lifecycle,
        Some(spec.to_string()),
    ))
}

/// The attach loop, with the context already bound into `create`.
///
/// Split out because a `PciDeviceCtx` needs a live VM, so this is the
/// only shape a test can reach.
fn attach_each(
    specs: &[String],
    log: &Logger,
    mut create: impl FnMut(&str) -> anyhow::Result<CreatedPciDevice>,
) -> anyhow::Result<PciAttachment> {
    let mut out = PciAttachment::default();
    for spec in specs {
        match create(spec) {
            Ok(created) => {
                // A device the registry refuses is a mis-specified
                // machine, so stop the pass like a creation failure.
                if let Err(e) = push_created(spec, created, &mut out) {
                    error!(log, "failed to register PCI device";
                        "spec" => spec,
                        "error" => %e,
                    );
                    return Err(anyhow::Error::new(e));
                }
            }
            Err(e) => {
                error!(log, "failed to create PCI device";
                    "spec" => spec,
                    "error" => format!("{:#}", e),
                );
                return Err(e);
            }
        }
    }
    Ok(out)
}

/// Run `factory` over already-bootindex-stripped `slot,driver[,config]`
/// specs, in order, and collect the handles.
pub fn attach_pci_devices(
    ctx: &PciDeviceCtx<'_>,
    specs: &[String],
    factory: &mut PciDeviceFactory<'_>,
) -> anyhow::Result<PciAttachment> {
    attach_each(specs, ctx.log, |spec| factory(spec, ctx))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use slog::Logger;

    use vmm_core::common::RWOp;
    use vmm_core::mem::SegidAlloc;
    use vmm_devices::pci::{BarN, PciDevice};
    use vmm_devices::Lifecycle;

    use super::{
        attach_each, push_created, CreatedPciDevice, LifecycleHandle,
        PciAttachment, PciDeviceHandle,
    };
    use crate::registry::{RegistryError, SlotState};

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

    fn pci() -> PciDeviceHandle {
        Arc::new(StubPci)
    }

    fn lifecycle() -> LifecycleHandle {
        Arc::new(StubLifecycle)
    }

    fn null_log() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    /// PCI handles, lifecycle handles, registered slots.
    fn collect(created: CreatedPciDevice) -> (usize, usize, usize) {
        let mut out = PciAttachment::default();
        push_created("4,stub,/disk", created, &mut out)
            .expect("one device never collides");
        (
            out.registry.pci_devices().len(),
            out.registry.lifecycle_devices().len(),
            out.registry.len(),
        )
    }

    #[test]
    fn normal_device_carries_both_handles() {
        assert_eq!(collect((Some(pci()), Some(lifecycle()))), (1, 1, 1));
    }

    #[test]
    fn passthru_shape_registers_a_pci_handle_only() {
        // passthru carries no Lifecycle and is never quiesced, paused or
        // migrated. The factory contract must keep permitting this.
        assert_eq!(collect((Some(pci()), None)), (1, 0, 1));
    }

    #[test]
    fn chipset_owned_driver_contributes_nothing() {
        // hostbridge and lpc: the chipset already created them.
        assert_eq!(collect((None, None)), (0, 0, 0));
    }

    #[test]
    fn lifecycle_without_a_pci_handle_is_kept() {
        // A backend with no PCI face must still be registered, or it
        // could never be quiesced or paused.
        assert_eq!(collect((None, Some(lifecycle()))), (0, 1, 1));
    }

    #[test]
    fn the_registry_records_the_id_bdf_and_spec() {
        let mut out = PciAttachment::default();
        push_created(
            "4:1,virtio-blk,/disk",
            (Some(pci()), Some(lifecycle())),
            &mut out,
        )
        .expect("first device");

        let dev = out
            .registry
            .get_by_id("virtio-blk@4:1")
            .expect("id follows driver@slot");
        let bdf = dev.bdf.expect("the slot field parses");
        assert_eq!((bdf.bus(), bdf.dev(), bdf.func()), (0, 4, 1));
        assert_eq!(dev.spec.as_deref(), Some("4:1,virtio-blk,/disk"));
        assert_eq!(dev.state, SlotState::Present);
        assert!(!dev.hotpluggable);
    }

    #[test]
    fn the_id_and_the_bdf_read_the_same_slot_field() {
        // Both must read one trimmed slot. A spec that names a slot the
        // id shows but the BDF drops would be unreachable by address.
        let mut out = PciAttachment::default();
        push_created(" 4 ,virtio-blk", (Some(pci()), None), &mut out)
            .expect("insert");

        let dev = out.registry.get_by_id("virtio-blk@4").expect("id");
        assert_eq!(dev.bdf, vmm_devices::pci::Bdf::new(0, 4, 0));
    }

    #[test]
    fn two_specs_at_one_slot_are_refused() {
        // The bus rejects the second attach. The registry is the second
        // guard, and it must not silently keep two slot 4 devices.
        let specs =
            vec!["4,virtio-blk,/a".to_string(), "4,virtio-net,/b".to_string()];

        let Err(err) = attach_each(&specs, &null_log(), |_spec| {
            Ok((Some(pci()), Some(lifecycle())))
        }) else {
            panic!("the second slot 4 spec must fail");
        };

        assert_eq!(
            err.downcast_ref::<RegistryError>(),
            Some(&RegistryError::DuplicateBdf(
                vmm_devices::pci::Bdf::new(0, 4, 0).expect("slot 4")
            ))
        );
    }

    #[test]
    fn every_spec_reaches_the_factory_in_order() {
        // Slot order decides PCI enumeration order, so the loop must not
        // reorder or skip.
        let specs = vec![
            "0,hostbridge".to_string(),
            "4,virtio-blk,/disk".to_string(),
            "5,virtio-rnd".to_string(),
        ];
        let mut seen: Vec<String> = Vec::new();
        let attached = attach_each(&specs, &null_log(), |spec| {
            seen.push(spec.to_string());
            Ok((Some(pci()), Some(lifecycle())))
        })
        .expect("factory never fails here");
        assert_eq!(seen, specs);
        assert_eq!(attached.registry.pci_devices().len(), 3);
        assert_eq!(attached.registry.lifecycle_devices().len(), 3);
        let ids: Vec<String> =
            attached.registry.list().into_iter().map(|d| d.id).collect();
        assert_eq!(ids, ["hostbridge@0", "virtio-blk@4", "virtio-rnd@5"]);
    }

    #[test]
    fn a_failed_device_stops_the_remaining_specs() {
        // A half-built machine must not run, so the first error aborts
        // the pass and no later spec is created.
        let specs = vec![
            "4,virtio-blk,/disk".to_string(),
            "5,nvme,/missing".to_string(),
            "6,virtio-rnd".to_string(),
        ];
        let mut seen: Vec<String> = Vec::new();
        let Err(err) = attach_each(&specs, &null_log(), |spec| {
            seen.push(spec.to_string());
            anyhow::ensure!(!spec.contains("nvme"), "failed to open disk");
            Ok((Some(pci()), None))
        }) else {
            panic!("the nvme spec must fail");
        };
        assert_eq!(err.to_string(), "failed to open disk");
        assert_eq!(seen, ["4,virtio-blk,/disk", "5,nvme,/missing"]);
    }

    #[test]
    fn an_arc_segid_alloc_keeps_one_counter() {
        // OwnedPciCtx shares the machine's allocator by Arc. A copy would
        // restart the counter and hand two devices the same segment ID,
        // so the kernel would reject the second VM_ALLOC_MEMSEG.
        let owned = Arc::new(SegidAlloc::new(0));
        let shared = owned.clone();
        assert!(Arc::ptr_eq(&owned, &shared));
        let first = owned.alloc().expect("segment 0 is free");
        let second = shared.alloc().expect("segment 1 is free");
        assert_ne!(first, second);
    }
}
