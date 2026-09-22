// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! firehyve's answer to the vsock CONTROL verb.
//!
//! firehyve has no control socket, so it serves the operator's
//! inventory on the vsock host socket. `vmm_virtio::vsock::control`
//! owns the grammar. This file supplies the commands:
//!
//! ```text
//! device-list                  -> OK 2 virtio-blk@4,0.4.0,present ...
//! cpu-list                     -> OK 4 0,present,boot 2,absent,hotplug ...
//! mem-list                     -> OK 1 boot,1073741824,present
//! device-add 5,virtio-blk,/d   -> OK virtio-blk@5
//! device-remove virtio-blk@5   -> OK virtio-blk@5,remove-pending
//! cpu-add 2                    -> OK 2,present
//! mem-add 134217728            -> OK slot0,present
//! ```
//!
//! `cpu-list` gives one record per CPU slot, `id,state,kind`. `kind`
//! separates a boot CPU from a slot a hot-add can take, and `state` is
//! `present`, `absent`, or `consumed` for an id a failed add spent.
//!
//! `mem-list` gives `key,value,state` records. Without a window that is
//! the one boot record. With one it also reports `window`, `slot-size`,
//! `slot-count`, `slots-used` and `added`, so an operator can see how
//! much of the window is left. `value` is a byte count except in the
//! two slot counts.
//!
//! `mem-add` takes BYTES, not the `-m` size grammar, so it means the
//! same thing on both transports. The engine rounds up to a whole slot.
//!
//! # Add only
//!
//! `cpu-remove` and `mem-remove` do nothing. illumos has no
//! `vm_deactivate_cpu` and no `VM_FREE_MEMSEG`, so the kernel cannot
//! do either removal. Both verbs are recognised and refused with that
//! reason, because "unknown command" does not say why.
//!
//! # Trust
//!
//! SECURITY: `device-add` opens a host file the peer names. This is the
//! same file-open authority argv has, and no wider: the spec goes
//! through the catalog closure the `-s` pass uses, so the peer can
//! reach no driver and no path that a command line cannot. Nothing
//! here authorises the peer. The host socket does: `vmm_core::unixsock`
//! binds it 0600 inside a 0700 directory, so the peer is this
//! process's own user. rshyve's control socket also checks the peer's
//! uid and zoneid through peercred.

use std::sync::Arc;

use vmm_machine::inventory::{
    cpu_inventory, MemWindowReport, CPU_HOTPLUG_OFF, MEM_HOTPLUG_OFF,
    NO_CPU_REMOVE, NO_MEM_REMOVE,
};
use vmm_machine::{DeviceRegistry, HotplugEngines, HotplugError};
use vmm_virtio::vsock::control::{ControlReply, ControlRequest, ControlSink};

/// Placeholder for a device that is not on the PCI bus.
const NO_BDF: &str = "-";

/// The machine, as the control commands see it.
pub struct Inventory {
    registry: Arc<DeviceRegistry>,
    /// Each engine is `None` unless the VM started with the option it
    /// needs. Without that option the guest has no hot-add interface.
    hotplug: HotplugEngines,
    num_cpus: u32,
    /// Every CPU slot the MADT describes. Equal to `num_cpus` unless
    /// `-c maxcpus=` asked for more.
    max_cpus: u32,
    mem_size: usize,
}

impl Inventory {
    pub fn new(
        registry: Arc<DeviceRegistry>,
        hotplug: HotplugEngines,
        num_cpus: u32,
        max_cpus: u32,
        mem_size: usize,
    ) -> Self {
        Self {
            registry,
            hotplug,
            num_cpus,
            max_cpus,
            mem_size,
        }
    }

    /// Build one device and report the id `device-remove` takes.
    fn device_add(&self, spec: &str) -> ControlReply {
        let Some(engine) = self.hotplug.pci.as_ref() else {
            return ControlReply::refused(HotplugError::Disabled.to_string());
        };
        match engine.add_device(spec) {
            Ok(id) => ControlReply::record([id]),
            Err(e) => ControlReply::refused(e.to_string()),
        }
    }

    /// Ask the guest to give a device up.
    ///
    /// The answer says the request was recorded, not that the device is
    /// gone. Only the guest can finish an eject. If the guest never
    /// runs `_EJ0`, the slot stays pending for the life of the VM.
    fn device_remove(&self, id: &str) -> ControlReply {
        let Some(engine) = self.hotplug.pci.as_ref() else {
            return ControlReply::refused(HotplugError::Disabled.to_string());
        };
        match engine.request_remove(id) {
            Ok(()) => ControlReply::record([
                id.to_string(),
                vmm_machine::SlotState::RemovePending.to_string(),
            ]),
            Err(e) => ControlReply::refused(e.to_string()),
        }
    }

    fn device_list(&self) -> ControlReply {
        let records = self
            .registry
            .list()
            .into_iter()
            .map(|device| {
                let bdf = device
                    .bdf
                    .map_or_else(|| NO_BDF.to_string(), |bdf| bdf.to_string());
                vec![device.id, bdf, device.state.to_string()]
            })
            .collect();
        ControlReply::list(records)
    }

    /// Bring one CPU online, and report the id that is now running.
    fn cpu_add(&self, id: u32) -> ControlReply {
        let Some(cpus) = self.hotplug.cpu.as_ref() else {
            return ControlReply::refused(CPU_HOTPLUG_OFF);
        };
        match cpus.add(id) {
            Ok(()) => {
                ControlReply::record([id.to_string(), "present".to_string()])
            }
            Err(e) => ControlReply::refused(e.to_string()),
        }
    }

    /// Give the guest more memory, and report the slot it went in.
    ///
    /// The size is BYTES. The engine rounds it up to a whole slot.
    fn mem_add(&self, bytes: u64) -> ControlReply {
        let Some(engine) = self.hotplug.mem.as_ref() else {
            return ControlReply::refused(MEM_HOTPLUG_OFF);
        };
        match engine.add_memory(bytes) {
            Ok(slot) => ControlReply::record([
                format!("slot{slot}"),
                "present".to_string(),
            ]),
            Err(e) => ControlReply::refused(e.to_string()),
        }
    }

    /// One record per CPU slot: `id,state,kind`.
    fn cpu_list(&self) -> ControlReply {
        let inv = cpu_inventory(
            self.num_cpus,
            self.max_cpus,
            self.hotplug.cpu.as_ref(),
        );
        let records = inv
            .slots
            .iter()
            .map(|slot| {
                vec![
                    slot.id.to_string(),
                    slot.state.to_string(),
                    slot.kind.to_string(),
                ]
            })
            .collect();
        ControlReply::list(records)
    }

    /// The boot memory, and the hot-add window when the VM has one.
    fn mem_list(&self) -> ControlReply {
        let boot = vec![
            "boot".to_string(),
            self.mem_size.to_string(),
            "present".to_string(),
        ];
        let Some(engine) = self.hotplug.mem.as_ref() else {
            return ControlReply::list(vec![boot]);
        };

        let mut records = vec![boot];
        records.extend(window_records(&MemWindowReport::of(engine)));
        ControlReply::list(records)
    }
}

/// What `mem-list` says about the hot-add window.
///
/// Split from the engine so a test can check the wire shape. Every part
/// of a real engine is an ioctl on a live VM.
fn window_records(report: &MemWindowReport) -> Vec<Vec<String>> {
    let record = |name: &str, value: String| {
        vec![name.to_string(), value, "present".to_string()]
    };
    vec![
        record("window", report.bytes.to_string()),
        record("slot-size", report.slot_bytes.to_string()),
        record("slot-count", report.slots.to_string()),
        record("slots-used", report.slots_used.to_string()),
        record("added", report.added_bytes.to_string()),
    ]
}

impl ControlSink for Inventory {
    fn handle(&self, request: ControlRequest) -> ControlReply {
        match request {
            ControlRequest::DeviceList => self.device_list(),
            ControlRequest::CpuList => self.cpu_list(),
            ControlRequest::MemList => self.mem_list(),
            ControlRequest::DeviceAdd(spec) => self.device_add(&spec),
            ControlRequest::DeviceRemove(id) => self.device_remove(&id),
            ControlRequest::CpuAdd(id) => self.cpu_add(id),
            ControlRequest::MemAdd(bytes) => self.mem_add(bytes),
            ControlRequest::CpuRemove => ControlReply::refused(NO_CPU_REMOVE),
            ControlRequest::MemRemove => ControlReply::refused(NO_MEM_REMOVE),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use std::collections::BTreeSet;

    use vmm_machine::{
        CpuHotplugEngine, CpuOnline, CpuSlots, DeviceRegistry,
        RegisteredDevice, VcpuError,
    };

    use super::*;
    use vmm_virtio::vsock::control::{answer_line, FIELD_MAX, REASON_MAX};

    fn line(inv: &Inventory, request: &str) -> String {
        answer_line(inv, request)
    }

    /// A machine with no hot-add engine, the default. Without
    /// `--hotplug` the guest has no hot-add interface.
    fn inventory(registry: DeviceRegistry) -> Inventory {
        Inventory::new(
            Arc::new(registry),
            HotplugEngines::default(),
            2,
            2,
            1_073_741_824,
        )
    }

    /// A stand-in CPU set. Every step of a real add is an ioctl, so a
    /// control-plane test needs this fake.
    struct FakeCpus {
        possible: u32,
        boot: u32,
        online: Mutex<BTreeSet<u32>>,
        refuse: Option<VcpuError>,
    }

    impl FakeCpus {
        fn new(possible: u32, boot: u32) -> Arc<Self> {
            Arc::new(Self {
                possible,
                boot,
                online: Mutex::new((0..boot).collect()),
                refuse: None,
            })
        }

        fn refusing(possible: u32, boot: u32, error: VcpuError) -> Arc<Self> {
            Arc::new(Self {
                possible,
                boot,
                online: Mutex::new((0..boot).collect()),
                refuse: Some(error),
            })
        }

        fn online(&self) -> std::sync::MutexGuard<'_, BTreeSet<u32>> {
            self.online.lock().expect("test lock")
        }
    }

    impl CpuOnline for FakeCpus {
        fn possible_cpus(&self) -> u32 {
            self.possible
        }

        fn boot_cpus(&self) -> u32 {
            self.boot
        }

        fn is_online(&self, id: u32) -> bool {
            self.online().contains(&id)
        }

        fn add_vcpu(&self, id: u32) -> Result<(), VcpuError> {
            if let Some(error) = self.refuse.clone() {
                return Err(error);
            }
            self.online().insert(id);
            Ok(())
        }
    }

    /// Discards every general-purpose event. These tests read the
    /// answer on the wire, not the guest's SCI.
    struct SilentSink;

    impl vmm_devices::acpi_gpe::HotplugEventSink for SilentSink {
        fn raise(&self, _bit: vmm_devices::acpi_gpe::GpeBit) {}
    }

    fn cpu_slots(cpus: Arc<FakeCpus>) -> CpuSlots {
        let log = slog::Logger::root(slog::Discard, slog::o!());
        let regs = vmm_devices::hotplug::cpu::CpuHotplug::new(
            cpus.possible_cpus(),
            Arc::new(SilentSink),
            log.clone(),
        );
        regs.set_boot_cpus(cpus.boot_cpus());
        let engine = CpuHotplugEngine::start_online(
            Arc::clone(&cpus) as Arc<dyn CpuOnline>,
            regs,
            log,
        );
        CpuSlots::new(engine, cpus)
    }

    fn inventory_with_cpus(cpus: Arc<FakeCpus>) -> Inventory {
        Inventory::new(
            Arc::new(DeviceRegistry::new()),
            HotplugEngines {
                cpu: Some(cpu_slots(Arc::clone(&cpus))),
                ..HotplugEngines::default()
            },
            cpus.boot_cpus(),
            cpus.possible_cpus(),
            1_073_741_824,
        )
    }

    fn with_devices(specs: &[(&str, Option<&str>)]) -> DeviceRegistry {
        let registry = DeviceRegistry::new();
        for (id, slot) in specs {
            registry
                .insert(RegisteredDevice::new(
                    *id,
                    slot.and_then(vmm_machine::parse_bdf),
                    None,
                    None,
                    None,
                ))
                .expect("insert");
        }
        registry
    }

    #[test]
    fn device_list_reports_id_bdf_and_state() {
        let inv = inventory(with_devices(&[
            ("virtio-blk@4", Some("4")),
            ("virtio-vsock@5", Some("5")),
        ]));

        assert_eq!(
            line(&inv, "device-list"),
            "OK 2 virtio-blk@4,0.4.0,present virtio-vsock@5,0.5.0,present"
        );
    }

    #[test]
    fn a_device_with_no_pci_face_reports_a_placeholder_bdf() {
        // Every record has three values, so a client can split it
        // without a per-device field count.
        let inv = inventory(with_devices(&[("varstore", None)]));

        assert_eq!(line(&inv, "device-list"), "OK 1 varstore,-,present");
    }

    #[test]
    fn an_empty_registry_answers_with_a_zero_count() {
        let inv = inventory(DeviceRegistry::new());
        assert_eq!(line(&inv, "device-list"), "OK 0");
    }

    #[test]
    fn cpu_list_reports_one_record_per_boot_cpu() {
        let inv = inventory(DeviceRegistry::new());
        assert_eq!(
            line(&inv, "cpu-list"),
            "OK 2 0,present,boot 1,present,boot"
        );
    }

    #[test]
    fn cpu_list_reports_the_madt_slots_even_with_no_engine() {
        // -c cpus=2,maxcpus=4 without --hotplug still gives the guest
        // four LAPIC entries, so cpu-list shows all four. cpu-add says
        // that the absent slots cannot be filled.
        let inv = Inventory::new(
            Arc::new(DeviceRegistry::new()),
            HotplugEngines::default(),
            2,
            4,
            1_073_741_824,
        );

        assert_eq!(
            line(&inv, "cpu-list"),
            "OK 4 0,present,boot 1,present,boot 2,absent,hotplug \
             3,absent,hotplug"
        );
        assert_eq!(
            line(&inv, "cpu-add 2"),
            "ERR CPU hot-add needs --hotplug and -c cpus=N,maxcpus=M",
        );
    }

    #[test]
    fn cpu_list_reports_every_slot_a_hot_add_can_take() {
        let inv = inventory_with_cpus(FakeCpus::new(4, 2));

        assert_eq!(
            line(&inv, "cpu-list"),
            "OK 4 0,present,boot 1,present,boot 2,absent,hotplug \
             3,absent,hotplug"
        );
    }

    #[test]
    fn cpu_list_shows_a_cpu_that_was_hot_added() {
        let inv = inventory_with_cpus(FakeCpus::new(4, 2));

        assert_eq!(line(&inv, "cpu-add 3"), "OK 3,present");

        let listed = line(&inv, "cpu-list");
        assert!(listed.contains("3,present,hotplug"), "{listed}");
        assert!(listed.contains("2,absent,hotplug"), "{listed}");
    }

    #[test]
    fn cpu_list_shows_an_id_a_failed_add_spent() {
        // ThreadGone means VM_ACTIVATE_CPU succeeded and the thread
        // did not start. illumos cannot deactivate a vCPU, so the id is
        // spent, and cpu-list must show it.
        let inv = inventory_with_cpus(FakeCpus::refusing(
            4,
            2,
            VcpuError::ThreadGone(3),
        ));

        assert!(line(&inv, "cpu-add 3").starts_with("ERR "));

        let listed = line(&inv, "cpu-list");
        assert!(listed.contains("3,consumed,hotplug"), "{listed}");
        assert_eq!(
            line(&inv, "cpu-add 3"),
            "ERR vCPU 3 was consumed by a failed add: the kernel cannot \
             deactivate a vCPU, so the id cannot be reused"
        );
    }

    #[test]
    fn a_cpu_id_the_machine_does_not_have_is_refused() {
        let inv = inventory_with_cpus(FakeCpus::new(4, 2));

        assert_eq!(
            line(&inv, "cpu-add 4"),
            "ERR vCPU 4 is not one of the 4 CPU slots"
        );
        assert_eq!(
            line(&inv, "cpu-add 0"),
            "ERR vCPU 0 is a boot CPU and is online already"
        );
    }

    #[test]
    fn a_cpu_id_that_is_not_a_number_is_refused() {
        // The id is parsed before the engine lookup, so a typo gets
        // the same answer whatever options the VM started with.
        let inv = inventory_with_cpus(FakeCpus::new(4, 2));

        assert_eq!(line(&inv, "cpu-add two"), "ERR two is not a CPU id");
        assert_eq!(line(&inv, "cpu-add -1"), "ERR -1 is not a CPU id");
        assert_eq!(
            line(&inv, "cpu-add 99999999999999999999"),
            "ERR 99999999999999999999 is not a CPU id",
        );
    }

    #[test]
    fn a_hostile_cpu_id_cannot_reach_the_operator_terminal() {
        let inv = inventory(DeviceRegistry::new());

        let line = line(&inv, "cpu-add \u{1b}[2Jwiped");

        assert_eq!(line, "ERR __2Jwiped is not a CPU id");
        assert!(!line.contains('\n'));
    }

    #[test]
    fn mem_list_reports_the_boot_memory() {
        let inv = inventory(DeviceRegistry::new());
        assert_eq!(line(&inv, "mem-list"), "OK 1 boot,1073741824,present");
    }

    #[test]
    fn mem_list_reports_the_window_the_slots_and_what_is_used() {
        // The engine needs a live VM, so the wire shape is checked
        // against the numbers a live engine reports.
        let window = vmm_machine::hot_add_window(
            1024 * 1024 * 1024,
            4 * 1024 * 1024 * 1024,
            1024 * 1024 * 1024,
        )
        .expect("4 slots of 1 GiB");

        let records = window_records(&MemWindowReport::new(
            &window,
            1,
            1024 * 1024 * 1024,
        ));

        assert_eq!(
            ControlReply::list(records).render(),
            "OK 5 window,4294967296,present slot-size,1073741824,present \
             slot-count,4,present slots-used,1,present \
             added,1073741824,present",
        );
    }

    #[test]
    fn a_memory_size_that_is_not_a_number_is_refused() {
        let inv = inventory(DeviceRegistry::new());

        assert_eq!(
            line(&inv, "mem-add 512M"),
            "ERR 512M is not a size in bytes",
        );
        assert_eq!(line(&inv, "mem-add -1"), "ERR -1 is not a size in bytes");
    }

    #[test]
    fn a_removal_says_why_this_platform_cannot_do_it() {
        // "unknown command" reads as a typo the operator can fix.
        let inv = inventory(DeviceRegistry::new());

        assert_eq!(
            line(&inv, "cpu-remove 3"),
            "ERR cpu-remove is not supported: illumos has no \
             vm_deactivate_cpu, so an active vCPU cannot be taken back"
        );
        assert_eq!(
            line(&inv, "mem-remove slot0"),
            "ERR mem-remove is not supported: illumos has no \
             VM_FREE_MEMSEG, so a memory segment lives until the VM does"
        );
    }

    #[test]
    fn an_unknown_verb_is_refused() {
        let inv = inventory(DeviceRegistry::new());
        assert_eq!(
            line(&inv, "device-plug 4,virtio-blk,/d.img"),
            "ERR unknown command device-plug"
        );
        assert_eq!(
            line(&inv, "DEVICE-LIST"),
            "ERR unknown command DEVICE-LIST"
        );
    }

    #[test]
    fn a_vm_without_hotplug_refuses_an_add_or_a_remove() {
        // The default. Without --hotplug the guest has no hot-add
        // interface, so the request is refused and nothing is opened.
        let inv = inventory(DeviceRegistry::new());
        let refused = "ERR this VM was not started with --hotplug";

        assert_eq!(line(&inv, "device-add 5,virtio-blk,/d.img"), refused);
        assert_eq!(line(&inv, "device-remove virtio-blk@5"), refused);
    }

    #[test]
    fn a_vm_without_hotplug_refuses_a_cpu_or_memory_add() {
        // The two engines need different options, so each answer names
        // the option the operator must add.
        let inv = inventory(DeviceRegistry::new());

        assert_eq!(
            line(&inv, "cpu-add 3"),
            "ERR CPU hot-add needs --hotplug and -c cpus=N,maxcpus=M",
        );
        assert_eq!(
            line(&inv, "mem-add 134217728"),
            "ERR memory hot-add needs --hotplug and -o hotplug.maxmem=SIZE",
        );
    }

    #[test]
    fn a_vm_with_cpu_slots_still_refuses_a_memory_add() {
        // Different options start the two engines, so one engine must
        // not answer for the other.
        let inv = inventory_with_cpus(FakeCpus::new(4, 2));

        assert_eq!(
            line(&inv, "mem-add 134217728"),
            "ERR memory hot-add needs --hotplug and -o hotplug.maxmem=SIZE",
        );
    }

    #[test]
    fn a_hotplug_verb_needs_exactly_one_argument() {
        // A device-add with no spec must not read as an empty spec. A
        // second field means the request was quoted wrongly.
        let inv = inventory(DeviceRegistry::new());

        assert_eq!(
            line(&inv, "device-add"),
            "ERR device-add needs one argument"
        );
        assert_eq!(
            line(&inv, "device-remove"),
            "ERR device-remove needs one argument"
        );
        assert_eq!(
            line(&inv, "device-add 5,virtio-blk, /d.img"),
            "ERR device-add takes one argument"
        );
        assert_eq!(line(&inv, "cpu-add"), "ERR cpu-add needs one argument");
        assert_eq!(line(&inv, "mem-add"), "ERR mem-add needs one argument");
        assert_eq!(
            line(&inv, "mem-add 128 MiB"),
            "ERR mem-add takes one argument"
        );
    }

    #[test]
    fn a_refusal_cannot_reach_the_operator_terminal() {
        // An error quotes the spec the peer sent. Without sanitising, a
        // control sequence in it reaches the terminal that shows the
        // answer.
        let line = ControlReply::refused(
            HotplugError::BadSpec("\u{1b}[2Jwiped\nOK 0".to_string())
                .to_string(),
        )
        .render();

        // The escape introducer is gone, so the rest is inert text.
        assert_eq!(line, "ERR bad spec: _[2Jwiped_OK 0");
        assert!(!line.contains('\n'), "a refusal cannot add a line");
    }

    #[test]
    fn a_long_refusal_is_capped() {
        let long = "a".repeat(REASON_MAX * 2);
        let line =
            ControlReply::refused(HotplugError::BadSpec(long).to_string())
                .render();

        assert_eq!(line.len(), REASON_MAX + "ERR ".len());
    }

    #[test]
    fn a_read_only_verb_takes_no_argument() {
        // If the argument is ignored, `device-list <id>` looks like a
        // filter that works.
        let inv = inventory(DeviceRegistry::new());
        assert_eq!(
            line(&inv, "device-list all"),
            "ERR device-list takes no argument"
        );
    }

    #[test]
    fn a_blank_request_is_refused() {
        let inv = inventory(DeviceRegistry::new());
        assert_eq!(line(&inv, ""), "ERR empty request");
    }

    #[test]
    fn a_hostile_id_cannot_forge_a_record_boundary() {
        // The id comes from a `-s` argument. A space or a comma in it
        // adds a field or a value that a client reads as a second
        // device.
        let inv = inventory(with_devices(&[("evil id,0.0.0,absent", None)]));

        assert_eq!(
            line(&inv, "device-list"),
            "OK 1 evil_id_0.0.0_absent,-,present"
        );
    }

    #[test]
    fn a_hostile_id_cannot_reach_the_operator_terminal() {
        // Without sanitising, an escape sequence in an id reaches the
        // terminal that reads the answer.
        let inv = inventory(with_devices(&[("\u{1b}[2Jwiped", None)]));

        assert_eq!(line(&inv, "device-list"), "OK 1 __2Jwiped,-,present");
    }

    #[test]
    fn a_long_id_is_capped() {
        let long = "a".repeat(FIELD_MAX * 2);
        let inv = inventory(with_devices(&[(long.as_str(), None)]));

        let answer = line(&inv, "device-list");
        assert_eq!(answer, format!("OK 1 {},-,present", "a".repeat(FIELD_MAX)));
    }

    #[test]
    fn an_unknown_verb_is_echoed_safely() {
        let inv = inventory(DeviceRegistry::new());
        assert_eq!(line(&inv, "\u{1b}[2J"), "ERR unknown command __2J");
    }
}
