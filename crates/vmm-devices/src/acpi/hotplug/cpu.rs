// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CPU hotplug AML, driven by GPE bit 2.
//!
//! This is the guest-visible half of [`crate::hotplug::cpu`]. Names
//! and register layout follow QEMU's modern CPU hotplug interface, so
//! a guest driver that already knows the interface works with no
//! change.
//!
//! `\_SB.PRES` owns the register block and a mutex. `\_SB.CPUS` holds
//! one device per CPU slot plus the methods those devices call. The
//! `_E02` handler runs `CSCN`, which walks the slots and turns each
//! latched event into a `Notify`.

use acpi_tables::aml;
use acpi_tables::Aml as _;

use crate::acpi::AcpiConfig;
use crate::aml::{Aml, AmlValue};
use crate::hotplug::cpu::{CPU_HOTPLUG_LEN, CPU_HOTPLUG_PORT, MAX_CPU_SLOTS};

/// The device that owns the register block.
const CONTROLLER: &str = "PRES";
/// The container that holds the CPU devices.
const CONTAINER: &str = "CPUS";
/// The operation region over the register block.
const REGION: &str = "PRST";

// Absolute paths, so a method can name a field from anywhere in the
// namespace. Every segment is 4 characters, which `aml::Path` needs.
const LOCK: &str = "\\_SB_.PRES.CPLK";
const SELECTOR: &str = "\\_SB_.PRES.CSEL";
const ENABLED: &str = "\\_SB_.PRES.CPEN";
const INSERT_EVENT: &str = "\\_SB_.PRES.CINS";
const REMOVE_EVENT: &str = "\\_SB_.PRES.CRMV";
const EJECT: &str = "\\_SB_.PRES.CEJ0";
const SCAN: &str = "\\_SB_.CPUS.CSCN";

// Method names inside the container.
const STATUS_METHOD: &str = "CSTA";
const NOTIFY_METHOD: &str = "CTFY";
const EJECT_METHOD: &str = "CEJ0";
const SCAN_METHOD: &str = "CSCN";

use super::{NOTIFY_DEVICE_CHECK, NOTIFY_EJECT_REQUEST, STA_PRESENT};

/// Wait forever for the register block mutex.
const NO_TIMEOUT: u16 = 0xFFFF;

/// Emit the CPU hotplug controller and its slot devices.
pub(super) fn emit_devices(sb: &mut Aml, cfg: &AcpiConfig) {
    let slots = cpu_slots(cfg);
    if slots.is_empty() {
        return;
    }
    emit_controller(sb);
    emit_container(sb, &slots);
}

/// Emit the body of the `_E02` handler.
pub(super) fn gpe_body(m: &mut Aml, cfg: &AcpiConfig) {
    if cpu_slots(cfg).is_empty() {
        return;
    }
    aml::MethodCall::new(SCAN.into(), vec![]).to_aml_bytes(m);
}

/// The CPU slot ids to describe.
///
/// Empty when no CPU can ever be added, because a controller for an
/// event the VMM cannot raise is dead weight in every DSDT.
fn cpu_slots(cfg: &AcpiConfig) -> Vec<u8> {
    if cfg.max_cpus <= cfg.num_cpus {
        return Vec::new();
    }
    let asked = usize::try_from(cfg.max_cpus).unwrap_or(MAX_CPU_SLOTS);
    // The register file clamps to the same limit, so the two halves
    // always describe the same slots.
    (0..=u8::MAX).take(asked.min(MAX_CPU_SLOTS)).collect()
}

/// The AML name of a CPU slot. Always 4 characters, because the ids
/// stop below 0x100.
fn cpu_name(id: u8) -> String {
    format!("C{id:03X}")
}

/// Emit `Device(PRES)`: the register block and its mutex.
fn emit_controller(sb: &mut Aml) {
    sb.device(CONTROLLER, |dev| {
        dev.name_val("_HID", AmlValue::DWord(Aml::eisa_id("PNP0A06")));
        dev.name_val("_UID", AmlValue::String("CPU hotplug resources"));
        // A scan and a status read are both select-then-access, so
        // they have to exclude each other.
        aml::Mutex::new("CPLK".into(), 0).to_aml_bytes(dev);

        // The reservation stops the guest giving the ports to another
        // driver, which would then write over the selector.
        dev.name_resource_template("_CRS", |res| {
            let len = u8::try_from(CPU_HOTPLUG_LEN).unwrap_or(u8::MAX);
            res.io_resource(CPU_HOTPLUG_PORT, CPU_HOTPLUG_PORT, 1, len);
        });

        aml::OpRegion::new(
            REGION.into(),
            aml::OpRegionSpace::SystemIO,
            &usize::from(CPU_HOTPLUG_PORT),
            &usize::from(CPU_HOTPLUG_LEN),
        )
        .to_aml_bytes(dev);

        // The status and control byte at offset 4, then the command
        // byte at offset 5.
        aml::Field::new(
            REGION.into(),
            aml::FieldAccessType::Byte,
            aml::FieldLockRule::NoLock,
            aml::FieldUpdateRule::WriteAsZeroes,
            vec![
                aml::FieldEntry::Reserved(32),
                aml::FieldEntry::Named(*b"CPEN", 1),
                aml::FieldEntry::Named(*b"CINS", 1),
                aml::FieldEntry::Named(*b"CRMV", 1),
                aml::FieldEntry::Named(*b"CEJ0", 1),
                // Bit 4 is QEMU's firmware eject handover. This VMM
                // has no SMI firmware to finish it, so the bit stays
                // reserved and no guest driver can reach it.
                aml::FieldEntry::Reserved(4),
                aml::FieldEntry::Named(*b"CCMD", 8),
            ],
        )
        .to_aml_bytes(dev);

        // The selector at offset 0 and the command data at offset 8.
        aml::Field::new(
            REGION.into(),
            aml::FieldAccessType::DWord,
            aml::FieldLockRule::NoLock,
            aml::FieldUpdateRule::Preserve,
            vec![
                aml::FieldEntry::Named(*b"CSEL", 32),
                aml::FieldEntry::Reserved(32),
                aml::FieldEntry::Named(*b"CDAT", 32),
            ],
        )
        .to_aml_bytes(dev);
    });
}

/// Emit `Device(CPUS)`: the methods and one device per slot.
fn emit_container(sb: &mut Aml, slots: &[u8]) {
    sb.device(CONTAINER, |cpus| {
        cpus.name_val("_HID", AmlValue::String("ACPI0010"));
        cpus.name_val("_CID", AmlValue::DWord(Aml::eisa_id("PNP0A05")));

        emit_status_method(cpus);
        emit_notify_method(cpus, slots);
        emit_eject_method(cpus);
        emit_scan_method(cpus, slots);

        for id in slots {
            emit_cpu_device(cpus, *id);
        }
    });
}

/// `Method(CSTA, 1)`: read the enable bit of the slot in Arg0.
fn emit_status_method(parent: &mut Aml) {
    parent.method(STATUS_METHOD, 1, true, |m| {
        aml::Acquire::new(LOCK.into(), NO_TIMEOUT).to_aml_bytes(m);
        aml::Store::new(&aml::Path::new(SELECTOR), &aml::Arg(0))
            .to_aml_bytes(m);
        aml::Store::new(&aml::Local(0), &aml::ZERO).to_aml_bytes(m);
        aml::If::new(
            &aml::Equal::new(&aml::Path::new(ENABLED), &aml::ONE),
            vec![&aml::Store::new(&aml::Local(0), &STA_PRESENT)],
        )
        .to_aml_bytes(m);
        aml::Release::new(LOCK.into()).to_aml_bytes(m);
        aml::Return::new(&aml::Local(0)).to_aml_bytes(m);
    });
}

/// `Method(CTFY, 2)`: `Notify` the slot in Arg0 with the value in Arg1.
///
/// A `Notify` needs the device by name, and AML has no way to build a
/// name at run time, so the dispatch is a chain of comparisons.
fn emit_notify_method(parent: &mut Aml, slots: &[u8]) {
    parent.method(NOTIFY_METHOD, 2, false, |m| {
        for id in slots {
            let name = cpu_name(*id);
            aml::If::new(
                &aml::Equal::new(&aml::Arg(0), id),
                vec![&aml::Notify::new(&aml::Path::new(&name), &aml::Arg(1))],
            )
            .to_aml_bytes(m);
        }
    });
}

/// `Method(CEJ0, 1)`: ask the VMM to eject the slot in Arg0.
fn emit_eject_method(parent: &mut Aml) {
    parent.method(EJECT_METHOD, 1, true, |m| {
        aml::Acquire::new(LOCK.into(), NO_TIMEOUT).to_aml_bytes(m);
        aml::Store::new(&aml::Path::new(SELECTOR), &aml::Arg(0))
            .to_aml_bytes(m);
        aml::Store::new(&aml::Path::new(EJECT), &aml::ONE).to_aml_bytes(m);
        aml::Release::new(LOCK.into()).to_aml_bytes(m);
    });
}

/// `Method(CSCN, 0)`: walk the slots and report every latched event.
///
/// The acknowledge of a remove event happens here, before the guest
/// runs `_EJ0`. The register file keeps its own record of the pending
/// removal so the later eject is not refused.
fn emit_scan_method(parent: &mut Aml, slots: &[u8]) {
    let count = slots.len();
    parent.method(SCAN_METHOD, 0, true, |m| {
        aml::Acquire::new(LOCK.into(), NO_TIMEOUT).to_aml_bytes(m);
        aml::Store::new(&aml::Local(0), &aml::ZERO).to_aml_bytes(m);
        aml::While::new(
            &aml::LessThan::new(&aml::Local(0), &count),
            vec![
                &aml::Store::new(&aml::Path::new(SELECTOR), &aml::Local(0)),
                &aml::If::new(
                    &aml::Equal::new(&aml::Path::new(INSERT_EVENT), &aml::ONE),
                    vec![
                        &aml::MethodCall::new(
                            NOTIFY_METHOD.into(),
                            vec![&aml::Local(0), &NOTIFY_DEVICE_CHECK],
                        ),
                        &aml::Store::new(
                            &aml::Path::new(INSERT_EVENT),
                            &aml::ONE,
                        ),
                    ],
                ),
                &aml::If::new(
                    &aml::Equal::new(&aml::Path::new(REMOVE_EVENT), &aml::ONE),
                    vec![
                        &aml::MethodCall::new(
                            NOTIFY_METHOD.into(),
                            vec![&aml::Local(0), &NOTIFY_EJECT_REQUEST],
                        ),
                        &aml::Store::new(
                            &aml::Path::new(REMOVE_EVENT),
                            &aml::ONE,
                        ),
                    ],
                ),
                &aml::Add::new(&aml::Local(0), &aml::Local(0), &aml::ONE),
            ],
        )
        .to_aml_bytes(m);
        aml::Release::new(LOCK.into()).to_aml_bytes(m);
    });
}

/// Emit one CPU slot device.
///
/// Slot 0 gets no `_EJ0`. It holds the boot CPU, and guest and host
/// cannot resynchronise after it goes.
fn emit_cpu_device(parent: &mut Aml, id: u8) {
    let name = cpu_name(id);
    parent.device(&name, |dev| {
        dev.name_val("_HID", AmlValue::String("ACPI0007"));
        dev.name_val("_UID", AmlValue::Byte(id));

        dev.method("_STA", 0, false, |m| {
            aml::Return::new(&aml::MethodCall::new(
                STATUS_METHOD.into(),
                vec![&id],
            ))
            .to_aml_bytes(m);
        });

        // Linux builds a CPU it is about to bring online from _MAT, so
        // every slot needs one even while the MADT reports it offline.
        aml::Name::new("_MAT".into(), &aml::BufferData::new(lapic_entry(id)))
            .to_aml_bytes(dev);

        if id != 0 {
            dev.method("_EJ0", 1, false, |m| {
                aml::MethodCall::new(EJECT_METHOD.into(), vec![&id])
                    .to_aml_bytes(m);
            });
        }
    });
}

/// A MADT Processor Local APIC entry for one slot.
///
/// The layout matches `acpi::madt`, and the APIC id matches the one
/// the register file reports for command 3. The Enabled flag is always
/// set: `_MAT` describes the CPU the guest is about to start, not the
/// state it is in now.
fn lapic_entry(id: u8) -> Vec<u8> {
    let mut entry = Vec::with_capacity(8);
    entry.push(0); // Processor Local APIC
    entry.push(8); // entry length
    entry.push(id); // ACPI processor id
    entry.push(id); // APIC id
    entry.extend_from_slice(&1u32.to_le_bytes()); // Enabled
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acpi::dsdt::build_dsdt;
    use crate::acpi::ACPI_HDR_SIZE;
    use crate::aml::walk_aml;

    fn hotplug_cfg(num_cpus: u32, max_cpus: u32) -> AcpiConfig {
        AcpiConfig::new(num_cpus, max_cpus)
            .expect("a valid CPU count")
            .with_hotplug(true)
    }

    fn devices(cfg: &AcpiConfig) -> Vec<u8> {
        let mut sb = Aml::new();
        emit_devices(&mut sb, cfg);
        sb.into_bytes()
    }

    fn handler(cfg: &AcpiConfig) -> Vec<u8> {
        let mut m = Aml::new();
        gpe_body(&mut m, cfg);
        m.into_bytes()
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[test]
    fn nothing_is_emitted_when_no_slot_is_spare() {
        for cpus in [1u32, 2, 8] {
            let cfg = hotplug_cfg(cpus, cpus);

            assert!(devices(&cfg).is_empty(), "{cpus} CPUs");
            // The handler has to stay empty too. A call to a method
            // that was never declared hangs the guest inside ACPICA.
            assert!(handler(&cfg).is_empty(), "{cpus} CPUs");
        }
    }

    #[test]
    fn a_spare_slot_emits_the_controller_and_the_container() {
        let cfg = hotplug_cfg(2, 8);
        let aml = devices(&cfg);

        walk_aml(&aml).expect("the hotplug devices should parse");
        for name in [b"PRES", b"CPUS", b"PRST", b"CPLK", b"CSEL", b"CPEN"] {
            assert!(contains(&aml, name), "{:?} is missing", name);
        }
        assert!(handler(&cfg).ends_with(b"CSCN"));
    }

    #[test]
    fn the_controller_reserves_the_register_block() {
        let aml = devices(&hotplug_cfg(2, 8));

        // IO(Decode16, 0xAF00, 0xAF00, 1, 12)
        let descriptor = [
            0x47u8,
            0x01,
            0x00,
            0xAF,
            0x00,
            0xAF,
            0x01,
            u8::try_from(CPU_HOTPLUG_LEN).expect("the block fits a byte"),
        ];
        assert!(contains(&aml, &descriptor));
    }

    #[test]
    fn every_spare_slot_gets_a_device() {
        let aml = devices(&hotplug_cfg(2, 8));

        for id in 0u8..8 {
            assert!(contains(&aml, cpu_name(id).as_bytes()), "slot {id}");
        }
        assert!(!contains(&aml, cpu_name(8).as_bytes()));
    }

    #[test]
    fn the_boot_cpu_has_no_eject_method() {
        let mut boot = Aml::new();
        emit_cpu_device(&mut boot, 0);
        let boot = boot.into_bytes();

        let mut spare = Aml::new();
        emit_cpu_device(&mut spare, 1);
        let spare = spare.into_bytes();

        assert!(contains(&boot, b"C000"));
        assert!(!contains(&boot, b"_EJ0"));
        assert!(contains(&spare, b"C001"));
        assert!(contains(&spare, b"_EJ0"));
    }

    #[test]
    fn the_slot_count_is_clamped_to_the_register_file_limit() {
        let mut cfg = hotplug_cfg(1, 2);
        // AcpiConfig::new refuses this, but the fields are public.
        cfg.max_cpus = u32::MAX;

        assert_eq!(cpu_slots(&cfg).len(), MAX_CPU_SLOTS);
        // A longer name would not be a legal 4 character NameSeg.
        assert_eq!(cpu_name(u8::MAX).len(), 4);
    }

    #[test]
    fn the_controller_uid_is_a_string() {
        let aml = devices(&hotplug_cfg(2, 8));

        // ACPI 6.5, section 6.1.12: every device that shares a _HID
        // needs its own _UID. The other PNP0A06 devices in this DSDT
        // hold an integer or another string, so a string here cannot
        // collide with any of them.
        assert!(contains(&aml, b"_UID\x0dCPU hotplug resources\x00"));
    }

    #[test]
    fn the_mat_buffer_matches_the_madt_entry() {
        assert_eq!(lapic_entry(3), vec![0, 8, 3, 3, 1, 0, 0, 0]);
    }

    #[test]
    fn a_hotplug_dsdt_is_structurally_valid() {
        for (num_cpus, max_cpus) in [(2u32, 8u32), (1, 2), (4, 64)] {
            let dsdt = build_dsdt(&hotplug_cfg(num_cpus, max_cpus));

            walk_aml(&dsdt[ACPI_HDR_SIZE..])
                .unwrap_or_else(|e| panic!("{num_cpus}/{max_cpus}: {e}"));
            assert!(contains(&dsdt, b"CSCN"));
        }
    }

    #[test]
    fn a_dsdt_with_no_spare_slot_carries_no_cpu_aml() {
        let dsdt = build_dsdt(&hotplug_cfg(4, 4));

        walk_aml(&dsdt[ACPI_HDR_SIZE..]).expect("the DSDT should parse");
        for name in [b"PRES", b"CPUS", b"CSCN", b"CSTA"] {
            assert!(!contains(&dsdt, name), "{:?} leaked", name);
        }
    }
}
