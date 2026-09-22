// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PCI slot hotplug AML, driven by GPE bit 1.
//!
//! The AML here is the guest half of [`crate::hotplug::pci`]. It
//! declares:
//!
//! * `PHPR`, a container that reserves the register block, maps it
//!   through an `OperationRegion`, and holds the `PCEJ` method every
//!   slot calls to ask for an eject.
//! * one slot device per hotpluggable slot, under the host bridge.
//! * `DVNT`, which turns a register bitmap into `Notify` calls, and
//!   `PCNT`, which reads both event registers once and hands each one
//!   to `DVNT`.
//!
//! `_E01` runs `PCNT`, so one SCI drains both registers.
//!
//! The slot devices sit under the host bridge and not directly under
//! `\_SB`. `_ADR` names a device on the bus that encloses it, so an OS
//! hotplug driver looks for these under the bridge and finds nothing
//! anywhere else.

use acpi_tables::{aml, Aml as _};

use crate::acpi::AcpiConfig;
use crate::aml::{Aml, AmlValue};
use crate::hotplug::pci::{
    is_hotpluggable_slot, PCI_HOTPLUG_ADDR, PCI_HOTPLUG_LEN, PCI_SLOTS,
};

/// The PCI host bridge, as [`crate::acpi::dsdt`] names it.
const HOST_BRIDGE: &str = "PC00";

/// Path of the slot scan method, under the host bridge.
const PCNT_PATH: &str = "\\_SB_.PC00.PCNT";

/// Path of the hotplug controller's mutex.
const BLCK_PATH: &str = "\\_SB_.PHPR.BLCK";

/// Path of the eject method every slot device calls.
const PCEJ_PATH: &str = "\\_SB_.PHPR.PCEJ";

/// Path of the slot-up register.
const PCIU_PATH: &str = "\\_SB_.PHPR.PCIU";

/// Path of the slot-down register.
const PCID_PATH: &str = "\\_SB_.PHPR.PCID";

use super::{NOTIFY_DEVICE_CHECK, NOTIFY_EJECT_REQUEST};

/// Names of the slot devices, `S000` through `S031`.
///
/// `acpi_tables::Path::new` panics on a segment that is not four
/// characters, so the names are literals and are never built from a
/// slot number at run time.
const SLOT_NAMES: [&str; PCI_SLOTS as usize] = [
    "S000", "S001", "S002", "S003", "S004", "S005", "S006", "S007", "S008",
    "S009", "S010", "S011", "S012", "S013", "S014", "S015", "S016", "S017",
    "S018", "S019", "S020", "S021", "S022", "S023", "S024", "S025", "S026",
    "S027", "S028", "S029", "S030", "S031",
];

/// Emit the PCI slot hotplug controller and its slot devices.
pub(super) fn emit_devices(sb: &mut Aml, _cfg: &AcpiConfig) {
    emit_controller(sb);
    sb.scope(HOST_BRIDGE, |bus| {
        for (slot, name) in hotpluggable_slots() {
            emit_slot(bus, slot, name);
        }
        emit_dvnt(bus);
        emit_pcnt(bus);
    });
}

/// Emit the body of the `_E01` handler.
pub(super) fn gpe_body(m: &mut Aml, _cfg: &AcpiConfig) {
    aml::MethodCall::new(PCNT_PATH.into(), vec![]).to_aml_bytes(m);
}

/// Every slot that gets a device, paired with its AML name.
fn hotpluggable_slots() -> impl Iterator<Item = (u8, &'static str)> {
    SLOT_NAMES.iter().enumerate().filter_map(|(index, name)| {
        let slot = u8::try_from(index).ok()?;
        is_hotpluggable_slot(slot).then_some((slot, *name))
    })
}

/// Emit `PHPR`, the hotplug controller.
fn emit_controller(sb: &mut Aml) {
    sb.device("PHPR", |dev| {
        dev.name_val("_HID", AmlValue::DWord(Aml::eisa_id("PNP0A06")));
        // The GPE0 reservation is the other PNP0A06 device and holds
        // _UID zero. ACPI 6.5, section 6.1.12 needs a unique _UID for
        // every device that shares a _HID.
        dev.name_val("_UID", AmlValue::One);
        // Present, enabled and functional, but not shown to the user.
        dev.name_val("_STA", AmlValue::Byte(0x0B));
        // Without the reservation the guest can hand the ports to
        // another driver, which then writes over the event registers.
        dev.name_resource_template("_CRS", |resources| {
            resources.io_resource(
                PCI_HOTPLUG_ADDR,
                PCI_HOTPLUG_ADDR,
                1,
                PCI_HOTPLUG_LEN,
            );
        });

        // PCEJ and PCNT both touch the register block, and either can
        // run on any CPU. The mutex is what keeps one from reading an
        // event register while the other is mid-eject.
        aml::Mutex::new("BLCK".into(), 0).to_aml_bytes(dev);
        aml::OpRegion::new(
            "PCST".into(),
            aml::OpRegionSpace::SystemIO,
            &usize::from(PCI_HOTPLUG_ADDR),
            &usize::from(PCI_HOTPLUG_LEN),
        )
        .to_aml_bytes(dev);
        // DWordAcc: the block decodes 4 byte accesses only.
        aml::Field::new(
            "PCST".into(),
            aml::FieldAccessType::DWord,
            aml::FieldLockRule::NoLock,
            aml::FieldUpdateRule::WriteAsZeroes,
            vec![
                aml::FieldEntry::Named(*b"PCIU", 32),
                aml::FieldEntry::Named(*b"PCID", 32),
                aml::FieldEntry::Named(*b"B0EJ", 32),
            ],
        )
        .to_aml_bytes(dev);

        // PCEJ(slot): B0EJ = 1 << slot.
        aml::Method::new(
            "PCEJ".into(),
            1,
            true,
            vec![
                &aml::Acquire::new(BLCK_PATH.into(), 0xFFFF),
                &aml::ShiftLeft::new(
                    &aml::Path::new("B0EJ"),
                    &aml::ONE,
                    &aml::Arg(0),
                ),
                &aml::Release::new(BLCK_PATH.into()),
            ],
        )
        .to_aml_bytes(dev);
    });
}

/// Emit one slot device.
///
/// Slots 0 and 1 never reach here, so the host bridge and the LPC
/// bridge get no `_EJ0` and the guest cannot ask to eject either.
fn emit_slot(bus: &mut Aml, slot: u8, name: &'static str) {
    bus.device(name, |dev| {
        dev.name_val("_SUN", AmlValue::Byte(slot));
        // Device number in the high word, function 0 in the low word.
        dev.name_val("_ADR", AmlValue::DWord(u32::from(slot) << 16));
        aml::Method::new(
            "_EJ0".into(),
            1,
            true,
            vec![&aml::MethodCall::new(
                PCEJ_PATH.into(),
                vec![&aml::Path::new("_SUN")],
            )],
        )
        .to_aml_bytes(dev);
    });
}

/// Emit `DVNT(bitmap, value)`, which notifies every slot in `bitmap`.
fn emit_dvnt(bus: &mut Aml) {
    bus.method("DVNT", 2, true, |m| {
        for (slot, name) in hotpluggable_slots() {
            // The slot is below PCI_SLOTS, so the mask stays in range.
            let mask: u32 = 1u32 << slot;
            aml::And::new(&aml::Local(0), &aml::Arg(0), &mask).to_aml_bytes(m);
            aml::If::new(
                &aml::Equal::new(&aml::Local(0), &mask),
                vec![&aml::Notify::new(&aml::Path::new(name), &aml::Arg(1))],
            )
            .to_aml_bytes(m);
        }
    });
}

/// Emit `PCNT()`, which drains both event registers.
///
/// Reading PCIU or PCID clears it, so each register is read once and
/// the value is handed straight to `DVNT`.
fn emit_pcnt(bus: &mut Aml) {
    bus.method("PCNT", 0, true, |m| {
        aml::Acquire::new(BLCK_PATH.into(), 0xFFFF).to_aml_bytes(m);
        aml::MethodCall::new(
            "DVNT".into(),
            vec![&aml::Path::new(PCIU_PATH), &NOTIFY_DEVICE_CHECK],
        )
        .to_aml_bytes(m);
        aml::MethodCall::new(
            "DVNT".into(),
            vec![&aml::Path::new(PCID_PATH), &NOTIFY_EJECT_REQUEST],
        )
        .to_aml_bytes(m);
        aml::Release::new(BLCK_PATH.into()).to_aml_bytes(m);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acpi::dsdt::build_dsdt;
    use crate::acpi::ACPI_HDR_SIZE;
    use crate::aml::walk_aml;
    use crate::hotplug::pci::FIRST_HOTPLUG_SLOT;

    fn hotplug_cfg() -> AcpiConfig {
        AcpiConfig::boot_only(1).with_hotplug(true)
    }

    fn devices() -> Vec<u8> {
        let mut aml = Aml::new();
        aml.scope("\\_SB", |sb| emit_devices(sb, &hotplug_cfg()));
        aml.into_bytes()
    }

    fn occurrences(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .filter(|window| *window == needle)
            .count()
    }

    /// A path as it appears in the byte stream, not as it is written.
    fn encoded(path: &str) -> Vec<u8> {
        let mut aml = Aml::new();
        aml::Path::new(path).to_aml_bytes(&mut aml);
        aml.into_bytes()
    }

    #[test]
    fn the_hotplug_dsdt_is_structurally_valid() {
        let dsdt = build_dsdt(&hotplug_cfg());
        walk_aml(&dsdt[ACPI_HDR_SIZE..]).unwrap();
    }

    #[test]
    fn the_emitted_devices_are_structurally_valid() {
        walk_aml(&devices()).unwrap();
    }

    /// Device(S002) exactly as a guest sees it. The slot device is the
    /// whole guest contract for one slot, so it is pinned byte for
    /// byte and not just searched for.
    const SLOT_2_AML: [u8; 50] = [
        0x5B, 0x82, 0x30, b'S', b'0', b'0', b'2', // Device(S002)
        0x08, b'_', b'S', b'U', b'N', 0x0A, 0x02, // Name(_SUN, 2)
        0x08, b'_', b'A', b'D', b'R', 0x0C, 0x00, 0x00, 0x02,
        0x00, // Name(_ADR, 0x20000)
        0x14, 0x19, b'_', b'E', b'J', b'0', 0x09, // Method(_EJ0, 1, Ser)
        0x5C, 0x2F, 0x03, b'_', b'S', b'B', b'_', b'P', b'H', b'P', b'R', b'P',
        b'C', b'E', b'J', // \_SB_.PHPR.PCEJ
        b'_', b'S', b'U', b'N', // (_SUN)
    ];

    #[test]
    fn a_slot_device_is_byte_for_byte_stable() {
        let aml = devices();
        let at = aml
            .windows(4)
            .position(|window| window == b"S002")
            .expect("the first hotpluggable slot");
        // The NameSeg follows DeviceOp and a one byte PkgLength.
        let start = at - 3;

        assert_eq!(&aml[start..start + SLOT_2_AML.len()], &SLOT_2_AML[..]);
    }

    #[test]
    fn the_chipset_slots_have_no_eject_method() {
        let aml = devices();

        assert_eq!(occurrences(&aml, b"S000"), 0);
        assert_eq!(occurrences(&aml, b"S001"), 0);
        // One _EJ0 per hotpluggable slot and no more.
        assert_eq!(
            occurrences(&aml, b"_EJ0"),
            usize::from(PCI_SLOTS - FIRST_HOTPLUG_SLOT),
        );
    }

    #[test]
    fn every_hotpluggable_slot_gets_a_device_and_a_notify() {
        let aml = devices();

        for (slot, name) in hotpluggable_slots() {
            // Once for the Device declaration and once in DVNT.
            assert_eq!(
                occurrences(&aml, name.as_bytes()),
                2,
                "slot {slot} is not both declared and notified",
            );
        }
    }

    #[test]
    fn the_scan_path_names_the_host_bridge() {
        // Path::new only accepts four character segments, so these
        // paths are literals. Keep them in step with the bridge name.
        assert_eq!(PCNT_PATH, format!("\\_SB_.{HOST_BRIDGE}.PCNT"));
    }

    #[test]
    fn the_slot_devices_land_under_the_host_bridge() {
        let plain = build_dsdt(&AcpiConfig::boot_only(1));
        let hotplug = build_dsdt(&hotplug_cfg());

        // The bridge Device in the DSDT, plus the Scope opened here.
        assert_eq!(occurrences(&plain, HOST_BRIDGE.as_bytes()), 1);
        assert_eq!(occurrences(&hotplug, HOST_BRIDGE.as_bytes()), 3);
    }

    #[test]
    fn the_gpe_handler_runs_the_scan_method() {
        let plain = build_dsdt(&AcpiConfig::boot_only(1));
        let hotplug = build_dsdt(&hotplug_cfg());

        assert_eq!(occurrences(&plain, b"PCNT"), 0);
        // The declaration, the call from _E01, and the path constant
        // check above proves the call names this bridge.
        assert_eq!(occurrences(&hotplug, b"PCNT"), 2);
    }

    #[test]
    fn the_controller_reserves_the_register_block() {
        let aml = devices();
        let base = PCI_HOTPLUG_ADDR.to_le_bytes();
        // IO descriptor: tag, Decode16, min, max, alignment, length.
        let descriptor = [
            0x47,
            0x01,
            base[0],
            base[1],
            base[0],
            base[1],
            1,
            PCI_HOTPLUG_LEN,
        ];

        assert_eq!(occurrences(&aml, &descriptor), 1);
    }

    #[test]
    fn the_operation_region_covers_the_register_block() {
        let aml = devices();
        // OpRegion(PCST, SystemIO, 0xAE00, 12): ExtOp, OpRegionOp,
        // NameSeg, region space, WordConst offset, ByteConst length.
        let region = [
            0x5B,
            0x80,
            b'P',
            b'C',
            b'S',
            b'T',
            0x01,
            0x0B,
            PCI_HOTPLUG_ADDR.to_le_bytes()[0],
            PCI_HOTPLUG_ADDR.to_le_bytes()[1],
            0x0A,
            PCI_HOTPLUG_LEN,
        ];

        assert_eq!(occurrences(&aml, &region), 1);
    }

    #[test]
    fn the_field_declares_three_dword_registers() {
        let aml = devices();
        // Field(PCST, DWordAcc, NoLock, WriteAsZeroes) with each named
        // register 32 bits wide, in register block order.
        let field = [
            b'P', b'C', b'S', b'T', 0x43, b'P', b'C', b'I', b'U', 0x20, b'P',
            b'C', b'I', b'D', 0x20, b'B', b'0', b'E', b'J', 0x20,
        ];

        assert_eq!(occurrences(&aml, &field), 1);
    }

    /// The `_UID` value that follows each `needle` in the stream.
    ///
    /// Every emitter writes `_HID` and then `_UID`, so the first
    /// `_UID` after a `_HID` belongs to the same device.
    fn uids_after(stream: &[u8], needle: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(hit) = stream[from..]
            .windows(needle.len())
            .position(|w| w == needle)
        {
            let at = from + hit;
            from = at + needle.len();
            let Some(uid) = stream[from..]
                .windows(4)
                .position(|w| w == b"_UID")
                .map(|p| from + p + 4)
            else {
                continue;
            };
            // ACPI 6.5, section 20.2.3: the data object that follows.
            let value = match stream[uid] {
                op @ (0x00 | 0x01) => vec![op],
                0x0A => stream[uid..uid + 2].to_vec(),
                0x0B => stream[uid..uid + 3].to_vec(),
                0x0C => stream[uid..uid + 5].to_vec(),
                0x0D => {
                    let end = uid
                        + 1
                        + stream[uid + 1..]
                            .iter()
                            .position(|b| *b == 0)
                            .expect("terminated string");
                    stream[uid..=end].to_vec()
                }
                op => panic!("unhandled _UID data object {op:#04x}"),
            };
            out.push(value);
        }
        out
    }

    #[test]
    fn every_pnp0a06_container_has_a_distinct_uid() {
        let dsdt = build_dsdt(&hotplug_cfg());
        let container = Aml::eisa_id("PNP0A06").to_le_bytes();

        // The GPE0 reservation plus one controller per hotplug resource
        // kind. The count can change. The _UIDs must stay unique.
        let uids = uids_after(&dsdt, &container);
        assert!(uids.len() >= 2, "no controller to check: {uids:?}");

        // ACPI 6.5, section 6.1.12: devices that share a _HID need a
        // unique _UID. A clash makes the guest name one of them twice.
        let mut seen = uids.clone();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), uids.len(), "_UID clash among {uids:?}");

        // The reservation holds Zero, so this controller must not.
        assert_eq!(uids_after(&devices(), &container), vec![vec![0x01u8]]);
    }

    #[test]
    fn the_eject_method_writes_the_slot_bit() {
        let aml = devices();
        let blck = encoded(BLCK_PATH);

        // Acquire(BLCK, 0xFFFF), ShiftLeft(One, Arg0, B0EJ),
        // Release(BLCK).
        let mut body = vec![0x5B, 0x23];
        body.extend_from_slice(&blck);
        body.extend_from_slice(&[
            0xFF, 0xFF, 0x79, 0x01, 0x68, b'B', b'0', b'E', b'J', 0x5B, 0x27,
        ]);
        body.extend_from_slice(&blck);

        assert_eq!(occurrences(&aml, &body), 1);
    }

    #[test]
    fn the_scan_method_drains_both_registers_once() {
        let aml = devices();

        assert_eq!(occurrences(&aml, &encoded(PCIU_PATH)), 1);
        assert_eq!(occurrences(&aml, &encoded(PCID_PATH)), 1);
        // The declaration and the two calls from PCNT.
        assert_eq!(occurrences(&aml, b"DVNT"), 3);
    }

    #[test]
    fn hotplug_aml_is_gated_on_the_configuration() {
        let plain = build_dsdt(&AcpiConfig::boot_only(1));

        for name in [b"PHPR", b"PCST", b"B0EJ", b"DVNT"] {
            assert_eq!(
                occurrences(&plain, name),
                0,
                "{} leaked into a non-hotplug DSDT",
                std::str::from_utf8(name).unwrap(),
            );
        }
    }
}
