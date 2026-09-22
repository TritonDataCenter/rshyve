// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Memory hotplug AML, driven by GPE bit 3.
//!
//! The AML here is the guest half of [`crate::hotplug::mem`]. It
//! declares:
//!
//! * `MHPC`, a container that reserves the register block, maps it
//!   through an `OperationRegion`, and holds the methods every slot
//!   device calls.
//! * one `PNP0C80` memory device per slot, `MP00` through `MP07`.
//! * `MTFY`, which turns a slot number into a `Notify`, and `MSCN`,
//!   which walks every slot and notifies the ones with a pending
//!   insert event.
//!
//! `_E03` runs `MSCN`, so one SCI drains every slot.
//!
//! # The `_CRS` template
//!
//! A slot's window is not known when the table is built, so `MCRS`
//! declares a `QWordMemory` descriptor with the widest legal window
//! and then overwrites three of its fields from the register block.
//! The overwrites are `CreateDWordField` at fixed byte offsets into
//! the template buffer, so an offset that is off by even one byte
//! gives the guest a window it misreads without any error. The offsets
//! are pinned by a test against the exact template bytes.

use acpi_tables::{aml, Aml as _};

use crate::acpi::AcpiConfig;
use crate::aml::{Aml, AmlValue};
use crate::hotplug::mem::{MAX_SLOTS, MEM_HOTPLUG_IO_BASE, MEM_HOTPLUG_IO_LEN};

/// Path of the hotplug controller's mutex.
const MLCK_PATH: &str = "\\_SB_.MHPC.MLCK";

/// Path of the slot selector, which every other register follows.
const MSEL_PATH: &str = "\\_SB_.MHPC.MSEL";

/// Path of the slot enabled bit.
const MES_PATH: &str = "\\_SB_.MHPC.MES_";

/// Path of the slot insert event bit.
const MINS_PATH: &str = "\\_SB_.MHPC.MINS";

/// Path of the slot eject request bit.
const MEJ_PATH: &str = "\\_SB_.MHPC.MEJ_";

/// Paths of the base and length halves, low then high.
const MRBL_PATH: &str = "\\_SB_.MHPC.MRBL";
const MRBH_PATH: &str = "\\_SB_.MHPC.MRBH";
const MRLL_PATH: &str = "\\_SB_.MHPC.MRLL";
const MRLH_PATH: &str = "\\_SB_.MHPC.MRLH";

/// Path of the slot scan method, which `_E03` runs.
const MSCN_PATH: &str = "\\_SB_.MHPC.MSCN";

use super::{NOTIFY_DEVICE_CHECK, STA_PRESENT};

/// `_STA` for the controller: present, enabled and functioning, but
/// not shown to the user.
const STA_HIDDEN: u8 = 0x0B;

/// Names of the slot devices, `MP00` through `MP07`.
///
/// `acpi_tables::Path::new` panics on a segment that is not four
/// characters, so the names are literals and are never built from a
/// slot number at run time.
const SLOT_NAMES: [&str; MAX_SLOTS] = [
    "MP00", "MP01", "MP02", "MP03", "MP04", "MP05", "MP06", "MP07",
];

/// Highest address the `_CRS` template can describe.
///
/// The template is overwritten in place, so it starts as the widest
/// legal window. ACPI 6.5, section 6.4.3.5.1 needs maximum + 1 to
/// equal length, which rules out `u64::MAX` as the maximum.
const CRS_TEMPLATE_MAX: u64 = u64::MAX - 1;

/// Byte offsets of the `QWordMemory` halves inside the template.
///
/// ACPI 6.5, table 6.44 lays the descriptor out as a tag byte, a two
/// byte length, three flag bytes, then five 8 byte values: granularity,
/// minimum, maximum, translation, and length. So the minimum starts at
/// byte 14 and each later value is 8 bytes further on. The names are
/// the halves the guest sees.
const CRS_FIELDS: [(&str, usize); 6] = [
    ("MINL", 14),
    ("MINH", 18),
    ("MAXL", 22),
    ("MAXH", 26),
    ("LENL", 38),
    ("LENH", 42),
];

/// Length of the register block, as the `_CRS` descriptor holds it.
const IO_LEN_BYTE: u8 = MEM_HOTPLUG_IO_LEN as u8;

const _: () = assert!(MEM_HOTPLUG_IO_LEN <= u8::MAX as u16);

/// Emit the memory hotplug controller and its slot devices.
///
/// A VM without hotplug gets nothing. Every VM with hotplug gets
/// [`MAX_SLOTS`] slots, because `AcpiConfig` carries no memory window
/// or slot count. A slot with nothing behind it answers `_STA` with
/// zero, so the guest ignores it.
pub(super) fn emit_devices(sb: &mut Aml, cfg: &AcpiConfig) {
    if !cfg.hotplug {
        return;
    }
    sb.device("MHPC", |dev| {
        emit_registers(dev);
        emit_methods(dev);
        emit_slots(dev);
    });
}

/// Emit the body of the `_E03` handler.
pub(super) fn gpe_body(m: &mut Aml, cfg: &AcpiConfig) {
    if !cfg.hotplug {
        return;
    }
    aml::MethodCall::new(MSCN_PATH.into(), vec![]).to_aml_bytes(m);
}

/// Emit the controller's identity, its resources, and the fields over
/// the register block.
fn emit_registers(dev: &mut Aml) {
    dev.name_val("_HID", AmlValue::DWord(Aml::eisa_id("PNP0A06")));
    // A string keeps this _UID apart from every integer one the other
    // PNP0A06 devices in this DSDT hold. ACPI 6.5, section 6.1.12
    // needs a unique _UID for every device that shares a _HID.
    dev.name_val("_UID", AmlValue::String("Memory hotplug resources"));
    dev.name_val("_STA", AmlValue::Byte(STA_HIDDEN));
    // Without the reservation the guest can hand the ports to another
    // driver, which then writes over the slot selector.
    dev.name_resource_template("_CRS", |resources| {
        resources.io_resource(
            MEM_HOTPLUG_IO_BASE,
            MEM_HOTPLUG_IO_BASE,
            1,
            IO_LEN_BYTE,
        );
    });

    // MSCN, MCRS and MEJ0 all select a slot and then read or write the
    // registers that follow it. The mutex is what stops one from
    // moving the selector under another.
    aml::Mutex::new("MLCK".into(), 0).to_aml_bytes(dev);
    aml::OpRegion::new(
        "MHPR".into(),
        aml::OpRegionSpace::SystemIO,
        &usize::from(MEM_HOTPLUG_IO_BASE),
        &usize::from(MEM_HOTPLUG_IO_LEN),
    )
    .to_aml_bytes(dev);

    // The block reads and writes different registers at the same
    // addresses, so it takes one field group per direction.
    aml::Field::new(
        "MHPR".into(),
        aml::FieldAccessType::DWord,
        aml::FieldLockRule::NoLock,
        aml::FieldUpdateRule::Preserve,
        vec![
            aml::FieldEntry::Named(*b"MRBL", 32),
            aml::FieldEntry::Named(*b"MRBH", 32),
            aml::FieldEntry::Named(*b"MRLL", 32),
            aml::FieldEntry::Named(*b"MRLH", 32),
        ],
    )
    .to_aml_bytes(dev);

    // The status bits share one byte at offset 0x14. ByteAcc with
    // WriteAsZeroes means a write of one bit does not read the byte
    // back first, and leaves the other bits clear.
    aml::Field::new(
        "MHPR".into(),
        aml::FieldAccessType::Byte,
        aml::FieldLockRule::NoLock,
        aml::FieldUpdateRule::WriteAsZeroes,
        vec![
            // 160 bits: skip the five DWord registers ahead of them.
            aml::FieldEntry::Reserved(160),
            aml::FieldEntry::Named(*b"MES_", 1),
            aml::FieldEntry::Named(*b"MINS", 1),
            // The VMM never sets MRMV: this kernel cannot take memory
            // back, so MSCN has no remove branch to run. The bit is
            // declared to keep the byte layout plain.
            aml::FieldEntry::Named(*b"MRMV", 1),
            aml::FieldEntry::Named(*b"MEJ_", 1),
        ],
    )
    .to_aml_bytes(dev);

    aml::Field::new(
        "MHPR".into(),
        aml::FieldAccessType::DWord,
        aml::FieldLockRule::NoLock,
        aml::FieldUpdateRule::Preserve,
        vec![
            aml::FieldEntry::Named(*b"MSEL", 32),
            aml::FieldEntry::Named(*b"MOEV", 32),
            aml::FieldEntry::Named(*b"MOSC", 32),
        ],
    )
    .to_aml_bytes(dev);
}

/// Emit the methods the slot devices and `_E03` call.
fn emit_methods(dev: &mut Aml) {
    emit_mtfy(dev);
    emit_mscn(dev);
    emit_mrst(dev);
    emit_mcrs(dev);
    emit_mej0(dev);
}

/// `MTFY(slot, value)`: notify the device for `slot`.
fn emit_mtfy(dev: &mut Aml) {
    dev.method("MTFY", 2, false, |m| {
        for (slot, name) in SLOT_NAMES.iter().enumerate() {
            aml::If::new(
                &aml::Equal::new(&aml::Arg(0), &slot),
                vec![&aml::Notify::new(&aml::Path::new(name), &aml::Arg(1))],
            )
            .to_aml_bytes(m);
        }
    });
}

/// `MSCN()`: notify every slot that has a pending insert event.
///
/// The mutex is held for the whole walk, because each step moves the
/// shared selector.
fn emit_mscn(dev: &mut Aml) {
    dev.method("MSCN", 0, false, |m| {
        aml::Acquire::new(MLCK_PATH.into(), 0xFFFF).to_aml_bytes(m);
        aml::Store::new(&aml::Local(0), &aml::ZERO).to_aml_bytes(m);
        aml::While::new(
            &aml::LessThan::new(&aml::Local(0), &MAX_SLOTS),
            vec![
                &aml::Store::new(&aml::Path::new(MSEL_PATH), &aml::Local(0)),
                &aml::If::new(
                    &aml::Equal::new(&aml::Path::new(MINS_PATH), &aml::ONE),
                    vec![
                        &aml::MethodCall::new(
                            "MTFY".into(),
                            vec![&aml::Local(0), &NOTIFY_DEVICE_CHECK],
                        ),
                        // Writing one clears the insert event.
                        &aml::Store::new(&aml::Path::new(MINS_PATH), &aml::ONE),
                    ],
                ),
                &aml::Add::new(&aml::Local(0), &aml::Local(0), &aml::ONE),
            ],
        )
        .to_aml_bytes(m);
        aml::Release::new(MLCK_PATH.into()).to_aml_bytes(m);
    });
}

/// `MRST(slot)`: the `_STA` of one slot.
fn emit_mrst(dev: &mut Aml) {
    dev.method("MRST", 1, false, |m| {
        aml::Acquire::new(MLCK_PATH.into(), 0xFFFF).to_aml_bytes(m);
        aml::Store::new(&aml::Path::new(MSEL_PATH), &aml::Arg(0))
            .to_aml_bytes(m);
        aml::Store::new(&aml::Local(0), &aml::ZERO).to_aml_bytes(m);
        aml::If::new(
            &aml::Equal::new(&aml::Path::new(MES_PATH), &aml::ONE),
            vec![&aml::Store::new(&aml::Local(0), &STA_PRESENT)],
        )
        .to_aml_bytes(m);
        aml::Release::new(MLCK_PATH.into()).to_aml_bytes(m);
        aml::Return::new(&aml::Local(0)).to_aml_bytes(m);
    });
}

/// `MCRS(slot)`: the `_CRS` of one slot.
///
/// Serialized because it declares `MR64`. Two threads inside one
/// method that declares a named object would try to create the same
/// name twice.
fn emit_mcrs(dev: &mut Aml) {
    dev.method("MCRS", 1, true, |m| {
        aml::Acquire::new(MLCK_PATH.into(), 0xFFFF).to_aml_bytes(m);
        aml::Store::new(&aml::Path::new(MSEL_PATH), &aml::Arg(0))
            .to_aml_bytes(m);
        emit_crs_template(m);
        for (name, offset) in CRS_FIELDS {
            aml::CreateDWordField::new(
                &aml::Path::new(name),
                &aml::Path::new("MR64"),
                &offset,
            )
            .to_aml_bytes(m);
        }

        aml::Store::new(&aml::Path::new("MINH"), &aml::Path::new(MRBH_PATH))
            .to_aml_bytes(m);
        aml::Store::new(&aml::Path::new("MINL"), &aml::Path::new(MRBL_PATH))
            .to_aml_bytes(m);
        aml::Store::new(&aml::Path::new("LENH"), &aml::Path::new(MRLH_PATH))
            .to_aml_bytes(m);
        aml::Store::new(&aml::Path::new("LENL"), &aml::Path::new(MRLL_PATH))
            .to_aml_bytes(m);

        // maximum = minimum + length - 1, done in 32 bit halves
        // because each half is its own buffer field. Every Store
        // truncates to 32 bits, which is what makes the two carry
        // tests below work.
        aml::Add::new(
            &aml::Path::new("MAXL"),
            &aml::Path::new("MINL"),
            &aml::Path::new("LENL"),
        )
        .to_aml_bytes(m);
        aml::Add::new(
            &aml::Path::new("MAXH"),
            &aml::Path::new("MINH"),
            &aml::Path::new("LENH"),
        )
        .to_aml_bytes(m);
        // The low half wrapped, so carry into the high half.
        aml::If::new(
            &aml::LessThan::new(
                &aml::Path::new("MAXL"),
                &aml::Path::new("MINL"),
            ),
            vec![&aml::Add::new(
                &aml::Path::new("MAXH"),
                &aml::Path::new("MAXH"),
                &aml::ONE,
            )],
        )
        .to_aml_bytes(m);
        // The low half is zero, so subtracting one borrows from the
        // high half.
        aml::If::new(
            &aml::LessThan::new(&aml::Path::new("MAXL"), &aml::ONE),
            vec![&aml::Subtract::new(
                &aml::Path::new("MAXH"),
                &aml::Path::new("MAXH"),
                &aml::ONE,
            )],
        )
        .to_aml_bytes(m);
        aml::Subtract::new(
            &aml::Path::new("MAXL"),
            &aml::Path::new("MAXL"),
            &aml::ONE,
        )
        .to_aml_bytes(m);

        aml::Release::new(MLCK_PATH.into()).to_aml_bytes(m);
        aml::Return::new(&aml::Path::new("MR64")).to_aml_bytes(m);
    });
}

/// `MEJ0(slot)`: ask the VMM to take one slot back.
///
/// The VMM records the request and refuses it: this kernel cannot free
/// a memory segment. The method still has to exist, because a guest
/// runs `_EJ0` on its own and expects a namespace object.
fn emit_mej0(dev: &mut Aml) {
    dev.method("MEJ0", 1, false, |m| {
        aml::Acquire::new(MLCK_PATH.into(), 0xFFFF).to_aml_bytes(m);
        aml::Store::new(&aml::Path::new(MSEL_PATH), &aml::Arg(0))
            .to_aml_bytes(m);
        aml::Store::new(&aml::Path::new(MEJ_PATH), &aml::ONE).to_aml_bytes(m);
        aml::Release::new(MLCK_PATH.into()).to_aml_bytes(m);
    });
}

/// Emit `Name(MR64, ResourceTemplate() { QWordMemory(...) })`.
///
/// Kept on its own so a test can pin the exact bytes the offsets in
/// [`CRS_FIELDS`] point into.
fn emit_crs_template(sink: &mut Aml) {
    aml::Name::new(
        "MR64".into(),
        &aml::ResourceTemplate::new(vec![&aml::AddressSpace::new_memory(
            aml::AddressSpaceCacheable::Cacheable,
            true,
            0u64,
            CRS_TEMPLATE_MAX,
            None,
        )]),
    )
    .to_aml_bytes(sink);
}

/// Emit one `PNP0C80` device per slot.
fn emit_slots(dev: &mut Aml) {
    for (slot, name) in SLOT_NAMES.iter().enumerate() {
        // SLOT_NAMES holds MAX_SLOTS entries, and MAX_SLOTS is checked
        // against u8::MAX where it is declared.
        let uid = slot as u8;
        dev.device(name, |slot_dev| {
            slot_dev.name_val("_HID", AmlValue::DWord(Aml::eisa_id("PNP0C80")));
            slot_dev.name_val("_UID", AmlValue::Byte(uid));
            slot_dev.method("_STA", 0, false, |m| {
                aml::Return::new(&aml::MethodCall::new(
                    "MRST".into(),
                    vec![&aml::Path::new("_UID")],
                ))
                .to_aml_bytes(m);
            });
            slot_dev.method("_CRS", 0, false, |m| {
                aml::Return::new(&aml::MethodCall::new(
                    "MCRS".into(),
                    vec![&aml::Path::new("_UID")],
                ))
                .to_aml_bytes(m);
            });
            slot_dev.method("_EJ0", 1, false, |m| {
                aml::MethodCall::new(
                    "MEJ0".into(),
                    vec![&aml::Path::new("_UID")],
                )
                .to_aml_bytes(m);
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::aml::walk_aml;

    /// Where the buffer data starts in the AML `emit_crs_template`
    /// writes: NameOp, four name bytes, BufferOp, a one byte PkgLength,
    /// and a two byte size. The `CRS_FIELDS` offsets count from there.
    const BUFFER_DATA: usize = 9;

    fn hotplug_cfg() -> AcpiConfig {
        AcpiConfig::boot_only(1).with_hotplug(true)
    }

    fn devices() -> Vec<u8> {
        let mut aml = Aml::new();
        emit_devices(&mut aml, &hotplug_cfg());
        aml.into_bytes()
    }

    /// The controller, the slot devices, and the `_E03` handler that
    /// drives them, as one term list.
    fn hotplug_aml() -> Vec<u8> {
        let mut aml = Aml::new();
        aml.scope("\\_SB", |sb| emit_devices(sb, &hotplug_cfg()));
        aml.scope("\\_GPE", |gpe| {
            gpe.method("_E03", 0, false, |m| gpe_body(m, &hotplug_cfg()));
        });
        aml.into_bytes()
    }

    fn occurrences(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .filter(|window| *window == needle)
            .count()
    }

    /// The AML encoding of an absolute three segment path.
    fn encoded(path: &str) -> Vec<u8> {
        let mut bytes = vec![0x5C, 0x2F, 0x03];
        for segment in path.trim_start_matches('\\').split('.') {
            bytes.extend_from_slice(segment.as_bytes());
        }
        bytes
    }

    fn crs_offset(name: &str) -> usize {
        CRS_FIELDS
            .iter()
            .find(|(field, _)| *field == name)
            .expect("field is in CRS_FIELDS")
            .1
    }

    fn store_dword(buffer: &mut [u8], at: usize, value: u32) {
        buffer[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn load_qword(buffer: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(buffer[at..at + 8].try_into().expect("8 bytes"))
    }

    #[test]
    fn nothing_is_emitted_without_hotplug() {
        let cfg = AcpiConfig::boot_only(1);
        let mut aml = Aml::new();

        emit_devices(&mut aml, &cfg);
        gpe_body(&mut aml, &cfg);

        assert!(aml.is_empty());
    }

    #[test]
    fn the_hotplug_aml_passes_the_structural_walker() {
        walk_aml(&hotplug_aml()).expect("every PkgLength must close");
    }

    /// The `_CRS` template is overwritten in place at fixed offsets, so
    /// a byte that moves silently gives the guest the wrong window.
    #[test]
    fn the_crs_template_bytes_are_pinned() {
        let mut aml = Aml::new();
        emit_crs_template(&mut aml);
        let bytes = aml.into_bytes();

        let mut expected = Vec::new();
        // Name(MR64, Buffer(48) { ... }).
        expected.extend_from_slice(&[0x08, b'M', b'R', b'6', b'4']);
        expected.extend_from_slice(&[0x11, 0x33, 0x0A, 0x30]);
        // QWordMemory: tag, a 43 byte body, resource type memory,
        // MinFixed and MaxFixed, then cacheable and read/write.
        expected.extend_from_slice(&[0x8A, 0x2B, 0x00, 0x00, 0x0C, 0x03]);
        expected.extend_from_slice(&0u64.to_le_bytes()); // granularity
        expected.extend_from_slice(&0u64.to_le_bytes()); // minimum
        expected.extend_from_slice(&CRS_TEMPLATE_MAX.to_le_bytes()); // maximum
        expected.extend_from_slice(&0u64.to_le_bytes()); // translation
        expected.extend_from_slice(&u64::MAX.to_le_bytes()); // length
        expected.extend_from_slice(&[0x79, 0x00]); // EndTag

        assert_eq!(bytes, expected);
        assert_eq!(bytes.len(), BUFFER_DATA + 48);
    }

    /// Patch the template the way `MCRS` does, then read it back the
    /// way ACPI 6.5, table 6.44 says a guest does.
    #[test]
    fn the_crs_field_offsets_address_the_descriptor_halves() {
        assert_eq!(
            CRS_FIELDS,
            [
                ("MINL", 14),
                ("MINH", 18),
                ("MAXL", 22),
                ("MAXH", 26),
                ("LENL", 38),
                ("LENH", 42),
            ],
        );

        let mut aml = Aml::new();
        emit_crs_template(&mut aml);
        let mut buffer = aml.into_bytes()[BUFFER_DATA..].to_vec();

        let base: u64 = 0x0000_0004_8000_0000;
        let len: u64 = 0x0000_0002_4000_0000;
        let max = base + len - 1;
        for (field, value) in [
            ("MINL", base as u32),
            ("MINH", (base >> 32) as u32),
            ("MAXL", max as u32),
            ("MAXH", (max >> 32) as u32),
            ("LENL", len as u32),
            ("LENH", (len >> 32) as u32),
        ] {
            store_dword(&mut buffer, crs_offset(field), value);
        }

        // Descriptor body: granularity at 6, minimum at 14, maximum at
        // 22, translation at 30, length at 38.
        assert_eq!(load_qword(&buffer, 6), 0, "granularity");
        assert_eq!(load_qword(&buffer, 14), base, "minimum");
        assert_eq!(load_qword(&buffer, 22), max, "maximum");
        assert_eq!(load_qword(&buffer, 30), 0, "translation");
        assert_eq!(load_qword(&buffer, 38), len, "length");
    }

    #[test]
    fn the_create_field_ops_use_the_pinned_offsets() {
        let aml = devices();

        for (name, offset) in CRS_FIELDS {
            // CreateDWordField(MR64, offset, name).
            let mut op = vec![0x8A, b'M', b'R', b'6', b'4', 0x0A];
            op.push(u8::try_from(offset).expect("offset fits in a byte"));
            op.extend_from_slice(name.as_bytes());
            assert_eq!(occurrences(&aml, &op), 1, "{name}");
        }
    }

    #[test]
    fn every_slot_gets_a_memory_device() {
        let aml = devices();
        let hid = Aml::eisa_id("PNP0C80").to_le_bytes();

        assert_eq!(occurrences(&aml, &hid), MAX_SLOTS);
        for (slot, name) in SLOT_NAMES.iter().enumerate() {
            // The device declaration and the Notify in MTFY.
            assert_eq!(occurrences(&aml, name.as_bytes()), 2, "{name}");
            // Name(_UID, Byte(slot)).
            let uid = [
                b'_',
                b'U',
                b'I',
                b'D',
                0x0A,
                u8::try_from(slot).expect("slot fits in a byte"),
            ];
            assert_eq!(occurrences(&aml, &uid), 1, "{name}");
        }
    }

    #[test]
    fn the_register_block_is_reserved_and_mapped() {
        let aml = devices();

        // IO descriptor: tag, Decode16, min, max, align, length.
        let base = MEM_HOTPLUG_IO_BASE.to_le_bytes();
        let descriptor = [
            0x47,
            0x01,
            base[0],
            base[1],
            base[0],
            base[1],
            1,
            IO_LEN_BYTE,
        ];
        assert_eq!(occurrences(&aml, &descriptor), 1);

        // OperationRegion(MHPR, SystemIO, 0x0A00, 0x18).
        let region = [
            0x5B,
            0x80,
            b'M',
            b'H',
            b'P',
            b'R',
            0x01,
            0x0B,
            base[0],
            base[1],
            0x0A,
            IO_LEN_BYTE,
        ];
        assert_eq!(occurrences(&aml, &region), 1);
    }

    #[test]
    fn the_status_byte_fields_sit_at_offset_twenty() {
        let aml = devices();

        // Field(MHPR, ByteAcc, NoLock, WriteAsZeroes) with a 160 bit
        // gap, then four one bit registers.
        let field = [
            b'M', b'H', b'P', b'R', 0x41, 0x00, 0x40, 0x0A, b'M', b'E', b'S',
            b'_', 0x01, b'M', b'I', b'N', b'S', 0x01, b'M', b'R', b'M', b'V',
            0x01, b'M', b'E', b'J', b'_', 0x01,
        ];
        assert_eq!(occurrences(&aml, &field), 1);
    }

    #[test]
    fn every_method_selects_a_slot_before_it_reads_one() {
        let aml = devices();

        // MSCN, MRST, MCRS and MEJ0 each write the selector once.
        assert_eq!(occurrences(&aml, &encoded(MSEL_PATH)), 4);
        // MRST reads the enabled bit.
        assert_eq!(occurrences(&aml, &encoded(MES_PATH)), 1);
        // MSCN tests the insert event, then writes one to clear it.
        assert_eq!(occurrences(&aml, &encoded(MINS_PATH)), 2);
        // MEJ0 asks for the eject.
        assert_eq!(occurrences(&aml, &encoded(MEJ_PATH)), 1);
        // MCRS copies both halves of the base and of the length.
        for path in [MRBL_PATH, MRBH_PATH, MRLL_PATH, MRLH_PATH] {
            assert_eq!(occurrences(&aml, &encoded(path)), 1, "{path}");
        }
    }

    #[test]
    fn every_method_releases_the_mutex_it_took() {
        let aml = devices();
        let mutex = encoded(MLCK_PATH);

        let mut acquires = Vec::new();
        acquires.extend_from_slice(&[0x5B, 0x23]);
        acquires.extend_from_slice(&mutex);
        let mut releases = Vec::new();
        releases.extend_from_slice(&[0x5B, 0x27]);
        releases.extend_from_slice(&mutex);

        // MSCN, MRST, MCRS and MEJ0.
        assert_eq!(occurrences(&aml, &acquires), 4);
        assert_eq!(occurrences(&aml, &releases), 4);
    }

    #[test]
    fn the_gpe_body_calls_the_scan_method() {
        let mut aml = Aml::new();
        gpe_body(&mut aml, &hotplug_cfg());

        assert_eq!(aml.into_bytes(), encoded(MSCN_PATH));
    }

    #[test]
    fn the_controller_uid_is_not_an_integer() {
        let aml = devices();

        // Two other PNP0A06 devices in this DSDT hold integer _UIDs.
        // A string cannot collide with either of them.
        let uid = b"_UID\x0dMemory hotplug resources\x00";
        assert_eq!(occurrences(&aml, uid), 1);
    }
}
