// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Kernel state export/import via the vmm-data interface.
//!
//! State travels as raw class blobs, tagged with the class and version
//! of the source kernel. The kernel does not convert between struct
//! versions, so the destination checks every blob against its own class
//! table before it writes any blob. See [`checks`].

use bhyve_api::{
    vdi_field_entry_v1, vdi_version_entry_v1, vm_reg_name, VDC_ATPIC,
    VDC_ATPIT, VDC_HPET, VDC_IOAPIC, VDC_LAPIC, VDC_MSR, VDC_PM_TIMER, VDC_RTC,
};
use slog;
use std::mem::size_of;
use std::sync::Arc;
use vmm_core::hdl::VmmHdl;
use vmm_core::vcpu::Vcpu;

use crate::codec::{
    DevicePayload, DeviceState, MigrateError, VcpuStatePayload,
};

mod checks;
mod time;

pub use time::{export_time_data, import_time_data};

use checks::{
    check_device_state, parse_vcpu_regs, read_class_layouts, RegWrite,
};

/// System devices to export and import: (name, class, version).
const SYSTEM_DEVICES: &[(&str, u16, u16)] = &[
    ("ioapic", VDC_IOAPIC, 1),
    ("atpit", VDC_ATPIT, 1),
    ("atpic", VDC_ATPIC, 1),
    ("hpet", VDC_HPET, 1),
    ("pm_timer", VDC_PM_TIMER, 1),
    ("rtc", VDC_RTC, 2),
];

/// Guest registers to export and import for each vCPU.
const VCPU_REGS: &[vm_reg_name] = &[
    vm_reg_name::VM_REG_GUEST_RAX,
    vm_reg_name::VM_REG_GUEST_RBX,
    vm_reg_name::VM_REG_GUEST_RCX,
    vm_reg_name::VM_REG_GUEST_RDX,
    vm_reg_name::VM_REG_GUEST_RSI,
    vm_reg_name::VM_REG_GUEST_RDI,
    vm_reg_name::VM_REG_GUEST_RBP,
    vm_reg_name::VM_REG_GUEST_RSP,
    vm_reg_name::VM_REG_GUEST_R8,
    vm_reg_name::VM_REG_GUEST_R9,
    vm_reg_name::VM_REG_GUEST_R10,
    vm_reg_name::VM_REG_GUEST_R11,
    vm_reg_name::VM_REG_GUEST_R12,
    vm_reg_name::VM_REG_GUEST_R13,
    vm_reg_name::VM_REG_GUEST_R14,
    vm_reg_name::VM_REG_GUEST_R15,
    vm_reg_name::VM_REG_GUEST_RIP,
    vm_reg_name::VM_REG_GUEST_RFLAGS,
    vm_reg_name::VM_REG_GUEST_CR0,
    vm_reg_name::VM_REG_GUEST_CR3,
    vm_reg_name::VM_REG_GUEST_CR2,
    vm_reg_name::VM_REG_GUEST_CR4,
    vm_reg_name::VM_REG_GUEST_DR7,
    vm_reg_name::VM_REG_GUEST_EFER,
    vm_reg_name::VM_REG_GUEST_XCR0,
    vm_reg_name::VM_REG_GUEST_CS,
    vm_reg_name::VM_REG_GUEST_DS,
    vm_reg_name::VM_REG_GUEST_ES,
    vm_reg_name::VM_REG_GUEST_FS,
    vm_reg_name::VM_REG_GUEST_GS,
    vm_reg_name::VM_REG_GUEST_SS,
    vm_reg_name::VM_REG_GUEST_TR,
    vm_reg_name::VM_REG_GUEST_LDTR,
];

/// Segment descriptor registers that use VM_GET/SET_SEGMENT_DESCRIPTOR
/// instead of VM_GET/SET_REGISTER.
const VCPU_SEG_DESCS: &[vm_reg_name] = &[
    vm_reg_name::VM_REG_GUEST_CS,
    vm_reg_name::VM_REG_GUEST_DS,
    vm_reg_name::VM_REG_GUEST_ES,
    vm_reg_name::VM_REG_GUEST_FS,
    vm_reg_name::VM_REG_GUEST_GS,
    vm_reg_name::VM_REG_GUEST_SS,
    vm_reg_name::VM_REG_GUEST_TR,
    vm_reg_name::VM_REG_GUEST_LDTR,
    vm_reg_name::VM_REG_GUEST_GDTR,
    vm_reg_name::VM_REG_GUEST_IDTR,
];

/// Export a vCPU's registers and segment descriptors.
///
/// Format, all little-endian:
///
/// - Per entry of [`VCPU_REGS`]: `[id: u32][value: u64]` (12 bytes).
/// - Marker `[0xFFFF_FFFF: u32]`, then `[count: u64]`.
/// - Per entry of [`VCPU_SEG_DESCS`]:
///   `[id: u32][base: u64][limit: u32][access: u32]` (20 bytes).
fn export_vcpu_regs(vcpu: &Vcpu) -> Result<Vec<u8>, MigrateError> {
    let mut data =
        Vec::with_capacity(VCPU_REGS.len() * 12 + VCPU_SEG_DESCS.len() * 20);

    for &reg in VCPU_REGS {
        let val = vcpu.get_reg(reg).map_err(|e| {
            MigrateError::VmmData(format!("get_reg {:?}: {e}", reg))
        })?;
        data.extend_from_slice(&(reg as u32).to_le_bytes());
        data.extend_from_slice(&val.to_le_bytes());
    }

    // The marker separates the descriptors from the plain registers.
    let seg_marker = 0xFFFF_FFFFu32;
    data.extend_from_slice(&seg_marker.to_le_bytes());
    data.extend_from_slice(&(VCPU_SEG_DESCS.len() as u64).to_le_bytes());

    for &reg in VCPU_SEG_DESCS {
        let desc = vcpu.get_segment_desc(reg).map_err(|e| {
            MigrateError::VmmData(format!("get_segment_desc {:?}: {e}", reg))
        })?;
        data.extend_from_slice(&(reg as u32).to_le_bytes());
        data.extend_from_slice(&desc.base.to_le_bytes());
        data.extend_from_slice(&desc.limit.to_le_bytes());
        data.extend_from_slice(&desc.access.to_le_bytes());
    }

    Ok(data)
}

/// Read a system-wide vmm-data class as raw bytes.
fn read_class_all(
    hdl: &VmmHdl,
    class: u16,
    version: u16,
) -> Result<Vec<u8>, MigrateError> {
    hdl.data_op(class, version).read_all::<u8>().map_err(|e| {
        MigrateError::VmmData(format!(
            "read_all class={class} version={version}: {e:?}"
        ))
    })
}

/// Read a per-vCPU vmm-data class as raw bytes.
fn read_class_vcpu(
    hdl: &VmmHdl,
    class: u16,
    version: u16,
    vcpuid: i32,
) -> Result<Vec<u8>, MigrateError> {
    hdl.data_op(class, version)
        .for_vcpu(vcpuid)
        .read_all::<u8>()
        .map_err(|e| {
            MigrateError::VmmData(format!(
            "read_vcpu class={class} version={version} vcpu={vcpuid}: {e:?}"
        ))
        })
}

fn write_class_all(
    hdl: &VmmHdl,
    class: u16,
    version: u16,
    data: &[u8],
) -> Result<(), MigrateError> {
    hdl.data_op(class, version).write_many(data).map_err(|e| {
        MigrateError::VmmData(format!(
            "write class={class} version={version}: {e:?}"
        ))
    })
}

/// The kernel writes that a state import performs.
///
/// Production uses [`KernelSink`]. Tests use a recording sink to check
/// the import decisions without a live bhyve instance.
trait ImportSink {
    fn write_class(
        &self,
        class: u16,
        version: u16,
        vcpuid: Option<i32>,
        data: &[u8],
    ) -> Result<(), MigrateError>;

    /// Segment selectors are not written here. The plain register list
    /// already holds CS through LDTR.
    fn write_reg(
        &self,
        vcpuid: i32,
        write: RegWrite,
    ) -> Result<(), MigrateError>;

    fn set_run_state(
        &self,
        vcpuid: i32,
        run_state: u32,
        sipi_vector: u8,
    ) -> Result<(), MigrateError>;
}

/// Applies imported state to a live bhyve instance.
struct KernelSink<'a> {
    hdl: &'a Arc<VmmHdl>,
}

impl KernelSink<'_> {
    fn vcpu(&self, vcpuid: i32) -> Vcpu {
        Vcpu::new_for_thread(vcpuid, Arc::clone(self.hdl))
    }
}

impl ImportSink for KernelSink<'_> {
    fn write_class(
        &self,
        class: u16,
        version: u16,
        vcpuid: Option<i32>,
        data: &[u8],
    ) -> Result<(), MigrateError> {
        let op = self.hdl.data_op(class, version);
        let op = match vcpuid {
            Some(id) => op.for_vcpu(id),
            None => op,
        };
        op.write_many(data).map_err(|e| {
            MigrateError::State(format!(
                "write class={class} version={version} vcpu={vcpuid:?}: {e:?}"
            ))
        })
    }

    fn write_reg(
        &self,
        vcpuid: i32,
        write: RegWrite,
    ) -> Result<(), MigrateError> {
        let vcpu = self.vcpu(vcpuid);
        match write {
            RegWrite::Reg(reg, value) => {
                vcpu.set_reg(reg, value).map_err(|e| {
                    MigrateError::State(format!(
                        "vcpu {vcpuid}: set {reg:?}={value:#x}: {e}"
                    ))
                })
            }
            RegWrite::Seg(reg, base, limit, access) => {
                let desc = bhyve_api::seg_desc {
                    base,
                    limit,
                    access,
                };
                vcpu.set_segment_desc(reg, &desc).map_err(|e| {
                    MigrateError::State(format!(
                        "vcpu {vcpuid}: set {reg:?} descriptor: {e}"
                    ))
                })
            }
        }
    }

    fn set_run_state(
        &self,
        vcpuid: i32,
        run_state: u32,
        sipi_vector: u8,
    ) -> Result<(), MigrateError> {
        self.vcpu(vcpuid)
            .set_run_state_full(run_state, sipi_vector)
            .map_err(|e| {
                MigrateError::VmmData(format!(
                    "set_run_state vcpu={vcpuid} state={run_state:#x}: {e}"
                ))
            })
    }
}

/// Read a per-vCPU field-entry class and serialize it for the wire as
/// `[vfe_ident: u32][_pad: u32][vfe_value: u64]` per entry,
/// little-endian.
fn read_field_entries(
    hdl: &VmmHdl,
    class: u16,
    vcpuid: i32,
    what: &str,
) -> Result<Vec<u8>, MigrateError> {
    let entries = hdl
        .data_op(class, 1)
        .for_vcpu(vcpuid)
        .read_all::<vdi_field_entry_v1>()
        .map_err(|e| {
            MigrateError::State(format!("export {what} vcpu={vcpuid}: {e:?}"))
        })?;
    let mut buf =
        Vec::with_capacity(entries.len() * size_of::<vdi_field_entry_v1>());
    for entry in &entries {
        buf.extend_from_slice(&entry.vfe_ident.to_le_bytes());
        buf.extend_from_slice(&entry._pad.to_le_bytes());
        buf.extend_from_slice(&entry.vfe_value.to_le_bytes());
    }
    Ok(buf)
}

/// Export all kernel device state: per-vCPU and system devices.
///
/// The VM must be paused for a consistent snapshot. vCPU registers use
/// individual get calls, because the vmm-data bulk interface is less
/// reliable for field-based classes such as VDC_REGISTER.
pub fn export_device_state(
    hdl: &Arc<VmmHdl>,
    num_cpus: u32,
    log: &slog::Logger,
) -> Result<DeviceState, MigrateError> {
    let mut vcpus = Vec::with_capacity(num_cpus as usize);
    for vcpuid in 0..num_cpus as i32 {
        let vcpu = Vcpu::new_for_thread(vcpuid, Arc::clone(hdl));

        let registers = export_vcpu_regs(&vcpu)?;

        // Every class below is required. A snapshot that drops one
        // resumes the guest with default state for that class.
        let lapic = read_class_vcpu(hdl, VDC_LAPIC, 1, vcpuid)?;
        let msrs = read_field_entries(hdl, VDC_MSR, vcpuid, "MSR")?;
        let vmm_arch = read_field_entries(
            hdl,
            bhyve_api::VDC_VMM_ARCH,
            vcpuid,
            "VMM_ARCH",
        )?;

        // FPU/XSAVE state is not transferred. The VM_GET_FPU and
        // VM_SET_FPU bindings exist.
        let fpu = Vec::new();

        // Run state restores an AP that is in HLT or INIT.
        let (run_state, sipi_vector) = vcpu.get_run_state().map_err(|e| {
            MigrateError::VmmData(format!("get_run_state vcpu={vcpuid}: {e}"))
        })?;

        slog::debug!(log, "vcpu state exported";
            "vcpu" => vcpuid,
            "lapic_bytes" => lapic.len(),
            "msr_bytes" => msrs.len(),
            "vmm_arch_bytes" => vmm_arch.len(),
            "run_state" => format!("{run_state:#x}"),
        );

        vcpus.push(VcpuStatePayload {
            vcpuid,
            registers,
            msrs,
            fpu,
            lapic,
            vmm_arch,
            run_state,
            sipi_vector,
        });
    }

    // A class that fails to read fails the export: the destination
    // cannot tell a failed read from an absent class.
    let mut devices = Vec::with_capacity(SYSTEM_DEVICES.len());
    for &(name, class, version) in SYSTEM_DEVICES {
        let data = read_class_all(hdl, class, version).map_err(|e| {
            MigrateError::State(format!("export device {name}: {e}"))
        })?;
        devices.push(DevicePayload {
            name: name.to_string(),
            class,
            version,
            data,
        });
    }

    Ok(DeviceState {
        vcpus,
        devices,
        emulated: Vec::new(),
        hyperv: None,
    })
}

/// Import all kernel device state: per-vCPU and system devices.
///
/// Time data must already be imported. The VM must be paused.
pub fn import_device_state(
    hdl: &Arc<VmmHdl>,
    state: &DeviceState,
    num_cpus: u32,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    let layouts = read_class_layouts(hdl)?;
    import_device_state_into(
        &KernelSink { hdl },
        &layouts,
        state,
        num_cpus,
        log,
    )
}

/// Check the payload against the local kernel, then apply all of it.
///
/// Every write failure is fatal. The caller aborts the migration, and
/// the source VM stays alive.
fn import_device_state_into(
    sink: &dyn ImportSink,
    layouts: &[vdi_version_entry_v1],
    state: &DeviceState,
    num_cpus: u32,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    check_device_state(state, layouts, num_cpus)?;

    // The payload carries no FPU/XSAVE state.
    slog::warn!(log, "importing without FPU/XSAVE state");

    // System devices go first, in the local order. The IOAPIC must be
    // ready before the LAPICs, and the peer controls the payload order.
    for &(name, class, _) in SYSTEM_DEVICES {
        let Some(dev) = state.devices.iter().find(|d| d.class == class) else {
            return Err(MigrateError::State(format!(
                "{name}: class {class} state is missing from the payload"
            )));
        };
        sink.write_class(dev.class, dev.version, None, &dev.data)?;
    }

    // Per vCPU: MSRs before the registers for GS_BASE, KERNEL_GS_BASE,
    // STAR and LSTAR. LAPIC before the registers so timer state is
    // correct. VMM_ARCH last, because it carries pending interrupts.
    for vcpu in &state.vcpus {
        let id = vcpu.vcpuid;
        sink.write_class(VDC_MSR, 1, Some(id), &vcpu.msrs)?;
        sink.write_class(VDC_LAPIC, 1, Some(id), &vcpu.lapic)?;
        for write in parse_vcpu_regs(id, &vcpu.registers)? {
            sink.write_reg(id, write)?;
        }
        // VRS_HALT is 0, so a halted vCPU must be restored too.
        sink.set_run_state(id, vcpu.run_state, vcpu.sipi_vector)?;
        sink.write_class(bhyve_api::VDC_VMM_ARCH, 1, Some(id), &vcpu.vmm_arch)?;
        slog::debug!(log, "vcpu state imported";
            "vcpu" => id,
            "msr_bytes" => vcpu.msrs.len(),
            "run_state" => format!("{:#x}", vcpu.run_state),
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::checks::fixtures::*;
    use super::*;
    use std::cell::RefCell;

    /// Records what an import applied, so the import decisions can be
    /// checked without a live bhyve instance.
    #[derive(Default)]
    struct RecordingSink {
        classes: RefCell<Vec<(u16, u16, Option<i32>, usize)>>,
        regs: RefCell<Vec<(i32, String)>>,
        run_states: RefCell<Vec<(i32, u32, u8)>>,
    }

    impl RecordingSink {
        fn wrote_class(&self, class: u16, vcpuid: Option<i32>) -> bool {
            self.classes
                .borrow()
                .iter()
                .any(|&(c, _, v, _)| c == class && v == vcpuid)
        }

        fn applied_nothing(&self) -> bool {
            self.classes.borrow().is_empty()
                && self.regs.borrow().is_empty()
                && self.run_states.borrow().is_empty()
        }
    }

    impl ImportSink for RecordingSink {
        fn write_class(
            &self,
            class: u16,
            version: u16,
            vcpuid: Option<i32>,
            data: &[u8],
        ) -> Result<(), MigrateError> {
            self.classes.borrow_mut().push((
                class,
                version,
                vcpuid,
                data.len(),
            ));
            Ok(())
        }

        fn write_reg(
            &self,
            vcpuid: i32,
            write: RegWrite,
        ) -> Result<(), MigrateError> {
            self.regs.borrow_mut().push((vcpuid, format!("{write:?}")));
            Ok(())
        }

        fn set_run_state(
            &self,
            vcpuid: i32,
            run_state: u32,
            sipi_vector: u8,
        ) -> Result<(), MigrateError> {
            self.run_states
                .borrow_mut()
                .push((vcpuid, run_state, sipi_vector));
            Ok(())
        }
    }

    fn test_log() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    fn import(
        state: &DeviceState,
    ) -> (RecordingSink, Result<(), MigrateError>) {
        import_with_layouts(state, &kernel_layouts())
    }

    fn import_with_num_cpus(
        state: &DeviceState,
        num_cpus: u32,
    ) -> (RecordingSink, Result<(), MigrateError>) {
        let sink = RecordingSink::default();
        let result = import_device_state_into(
            &sink,
            &kernel_layouts(),
            state,
            num_cpus,
            &test_log(),
        );
        (sink, result)
    }

    fn import_with_layouts(
        state: &DeviceState,
        layouts: &[vdi_version_entry_v1],
    ) -> (RecordingSink, Result<(), MigrateError>) {
        let sink = RecordingSink::default();
        let num_cpus = state.vcpus.len() as u32;
        let result = import_device_state_into(
            &sink,
            layouts,
            state,
            num_cpus,
            &test_log(),
        );
        (sink, result)
    }

    #[test]
    fn the_vcpu_payload_count_must_equal_the_vm_cpu_count() {
        // The kernel accepts any id below VM_MAXCPU. An extra payload
        // writes state into a vCPU with no thread. A missing payload
        // leaves that AP at reset.
        let mut state = good_state();
        state.vcpus.push(vcpu_payload(1));
        let (sink, result) = import_with_num_cpus(&state, 1);
        assert!(result.is_err(), "two payloads on a 1-CPU VM must fail");
        assert!(sink.applied_nothing());

        let (sink, result) = import_with_num_cpus(&good_state(), 2);
        assert!(result.is_err(), "one payload on a 2-CPU VM must fail");
        assert!(sink.applied_nothing());
    }

    #[test]
    fn a_vcpu_id_past_the_cpu_count_is_refused() {
        let mut state = good_state();
        state.vcpus = vec![vcpu_payload(17)];
        let (sink, result) = import_with_num_cpus(&state, 1);
        let error = result.map(|_| ()).expect_err("vcpu 17 on a 1-CPU VM");
        assert!(error.to_string().contains("outside 0..1"), "{error}");
        assert!(sink.applied_nothing());
    }

    #[test]
    fn system_devices_are_non_empty() {
        assert!(!SYSTEM_DEVICES.is_empty());
    }

    #[test]
    fn system_device_classes_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for &(_, class, _) in SYSTEM_DEVICES {
            assert!(
                seen.insert(class),
                "duplicate system device class: {class}"
            );
        }
    }

    #[test]
    fn well_formed_payload_imports() {
        let (sink, result) = import(&good_state());
        assert!(result.is_ok(), "{result:?}");
        assert!(sink.wrote_class(VDC_MSR, Some(0)));
        assert!(sink.wrote_class(VDC_LAPIC, Some(0)));
        assert!(sink.wrote_class(bhyve_api::VDC_VMM_ARCH, Some(0)));
        for &(_, class, _) in SYSTEM_DEVICES {
            assert!(sink.wrote_class(class, None), "class {class} not written");
        }
        assert_eq!(sink.regs.borrow().len(), 2);
        assert_eq!(sink.run_states.borrow().len(), 1);
    }

    /// A destination kernel with a different `vdi_field_entry_v1` size
    /// sees an MSR blob that is not a whole number of entries. The
    /// import must fail, not resume the guest with default MSRs.
    #[test]
    fn unaligned_msr_blob_fails_import() {
        let mut state = good_state();
        let mut msrs = field_entries(3);
        msrs.truncate(msrs.len() - 5);
        state.vcpus[0].msrs = msrs;

        let (sink, result) = import(&state);
        let err = result
            .map(|_| ())
            .expect_err("unaligned MSR blob must fail");
        let text = err.to_string();
        assert!(text.contains("MSR"), "{text}");
        assert!(sink.applied_nothing(), "state applied before the check");
    }

    /// The local kernel's element size is the authority.
    #[test]
    fn msr_entry_size_mismatch_fails_import() {
        let mut layouts = kernel_layouts();
        for e in layouts.iter_mut() {
            if e.vve_class == VDC_MSR {
                e.vve_len_per_item = 8;
            }
        }
        let (sink, result) = import_with_layouts(&good_state(), &layouts);
        assert!(result.is_err(), "entry size mismatch must fail");
        assert!(sink.applied_nothing());
    }

    #[test]
    fn missing_class_version_fails_import() {
        let mut layouts = kernel_layouts();
        for e in layouts.iter_mut() {
            if e.vve_class == VDC_LAPIC {
                e.vve_version = 2;
            }
        }
        let (sink, result) = import_with_layouts(&good_state(), &layouts);
        let err = result
            .map(|_| ())
            .expect_err("unknown LAPIC version must fail");
        assert!(err.to_string().contains("LAPIC"), "{err}");
        assert!(sink.applied_nothing());
    }

    #[test]
    fn wrong_fixed_length_fails_import() {
        let mut state = good_state();
        state.devices[0].data.push(0);
        let (sink, result) = import(&state);
        assert!(result.is_err(), "wrong fixed length must fail");
        assert!(sink.applied_nothing());
    }

    #[test]
    fn unknown_device_class_fails_import() {
        let mut state = good_state();
        state.devices.push(DevicePayload {
            name: "mystery".to_string(),
            class: 0xFFFF,
            version: 1,
            data: vec![0u8; TEST_DEV_LEN],
        });
        let (sink, result) = import(&state);
        assert!(result.is_err(), "unknown device class must fail");
        assert!(sink.applied_nothing());
    }

    #[test]
    fn missing_system_device_fails_import() {
        let mut state = good_state();
        state.devices.pop();
        let (sink, result) = import(&state);
        assert!(result.is_err(), "missing device class must fail");
        assert!(sink.applied_nothing());
    }

    #[test]
    fn empty_lapic_fails_import() {
        let mut state = good_state();
        state.vcpus[0].lapic.clear();
        let (sink, result) = import(&state);
        assert!(result.is_err(), "missing LAPIC state must fail");
        assert!(sink.applied_nothing());
    }

    #[test]
    fn truncated_segment_descriptors_fail_import() {
        let mut state = good_state();
        let mut blob = default_reg_blob();
        blob.truncate(blob.len() - 4);
        state.vcpus[0].registers = blob;
        let (sink, result) = import(&state);
        assert!(result.is_err(), "truncated register blob must fail");
        assert!(sink.applied_nothing());
    }

    #[test]
    fn duplicate_vcpu_id_fails_import() {
        let mut state = good_state();
        state.vcpus.push(vcpu_payload(0));
        let (sink, result) = import(&state);
        assert!(result.is_err(), "duplicate vcpu id must fail");
        assert!(sink.applied_nothing());
    }

    /// VRS_HALT is 0. A halted vCPU must still get its run state, or it
    /// resumes in the default state of the new destination vCPU.
    #[test]
    fn halted_run_state_is_applied() {
        let mut state = good_state();
        state.vcpus[0].run_state = bhyve_api::VRS_HALT;
        let (sink, result) = import(&state);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            sink.run_states.borrow().as_slice(),
            &[(0, bhyve_api::VRS_HALT, 0)]
        );
    }
}
