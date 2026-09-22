// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! QEMU table loader protocol.
//!
//! The UEFI firmware reads `etc/table-loader` from fw_cfg. It contains
//! an array of 128-byte commands that tell the firmware how to allocate,
//! link, and checksum ACPI tables in guest memory.
//!
//! For the loader protocol, pointer fields in the blobs must contain
//! the OFFSET of the target within the source blob, not a GPA. The
//! firmware adds the allocated guest address of the source blob to
//! produce the final GPA.

use super::rsdp::RSDP_SIZE;
use super::{AcpiLayout, ACPI_HDR_SIZE};

/// Size of each loader command entry.
const LOADER_ENTRY_SIZE: usize = 128;

/// Maximum name length in loader commands (same as fw_cfg).
const LOADER_MAX_NAME: usize = 56;

/// Loader command types.
const LOADER_CMD_ALLOC: u32 = 1;
const LOADER_CMD_ADD_POINTER: u32 = 2;
const LOADER_CMD_ADD_CHECKSUM: u32 = 3;

/// Memory zones for ALLOC.
const ZONE_HIGH: u8 = 1;
const ZONE_FSEG: u8 = 2;

/// fw_cfg blob names.
const TABLES_BLOB_NAME: &str = "etc/acpi/tables";
const RSDP_BLOB_NAME: &str = "etc/acpi/rsdp";

/// Write a NUL-terminated name into a fixed-size buffer.
///
/// Panics if `name` is too long (>= `LOADER_MAX_NAME` bytes).
fn write_loader_name(buf: &mut [u8; LOADER_MAX_NAME], name: &str) {
    let bytes = name.as_bytes();
    assert!(
        bytes.len() < LOADER_MAX_NAME,
        "loader name too long: {} (max {})",
        name,
        LOADER_MAX_NAME - 1,
    );
    buf[..bytes.len()].copy_from_slice(bytes);
    // Remaining bytes are already 0 (NUL padding)
}

/// Build an ALLOC loader command.
fn loader_alloc_cmd(
    name: &str,
    alignment: u32,
    zone: u8,
) -> [u8; LOADER_ENTRY_SIZE] {
    let mut entry = [0u8; LOADER_ENTRY_SIZE];

    // offset 0: command (u32 LE)
    entry[0..4].copy_from_slice(&LOADER_CMD_ALLOC.to_le_bytes());

    // offset 4: name (56 bytes, NUL-padded)
    let mut name_buf = [0u8; LOADER_MAX_NAME];
    write_loader_name(&mut name_buf, name);
    entry[4..4 + LOADER_MAX_NAME].copy_from_slice(&name_buf);

    // offset 60: alignment (u32 LE)
    entry[60..64].copy_from_slice(&alignment.to_le_bytes());

    // offset 64: zone (u8)
    entry[64] = zone;

    entry
}

/// Build an ADD_POINTER loader command.
///
/// The firmware reads `size` bytes from `dest_name` at `offset`,
/// adds the guest address of `src_name`'s allocated blob, and writes
/// the result back.
fn loader_add_pointer_cmd(
    dest_name: &str,
    src_name: &str,
    offset: u32,
    size: u8,
) -> [u8; LOADER_ENTRY_SIZE] {
    let mut entry = [0u8; LOADER_ENTRY_SIZE];

    // offset 0: command (u32 LE)
    entry[0..4].copy_from_slice(&LOADER_CMD_ADD_POINTER.to_le_bytes());

    // offset 4: dest_name (56 bytes)
    let mut dest_buf = [0u8; LOADER_MAX_NAME];
    write_loader_name(&mut dest_buf, dest_name);
    entry[4..4 + LOADER_MAX_NAME].copy_from_slice(&dest_buf);

    // offset 60: src_name (56 bytes)
    let mut src_buf = [0u8; LOADER_MAX_NAME];
    write_loader_name(&mut src_buf, src_name);
    entry[60..60 + LOADER_MAX_NAME].copy_from_slice(&src_buf);

    // offset 116: offset (u32 LE)
    entry[116..120].copy_from_slice(&offset.to_le_bytes());

    // offset 120: size (u8)
    entry[120] = size;

    entry
}

/// Build an ADD_CHECKSUM loader command.
///
/// The firmware computes a checksum over `name[start..start+len]`
/// and stores it at `name[cksum_offset]` such that the sum of all
/// bytes in the range is 0.
fn loader_add_checksum_cmd(
    name: &str,
    cksum_offset: u32,
    start: u32,
    len: u32,
) -> [u8; LOADER_ENTRY_SIZE] {
    let mut entry = [0u8; LOADER_ENTRY_SIZE];

    // offset 0: command (u32 LE)
    entry[0..4].copy_from_slice(&LOADER_CMD_ADD_CHECKSUM.to_le_bytes());

    // offset 4: name (56 bytes)
    let mut name_buf = [0u8; LOADER_MAX_NAME];
    write_loader_name(&mut name_buf, name);
    entry[4..4 + LOADER_MAX_NAME].copy_from_slice(&name_buf);

    // offset 60: checksum offset (u32 LE)
    entry[60..64].copy_from_slice(&cksum_offset.to_le_bytes());

    // offset 64: start (u32 LE)
    entry[64..68].copy_from_slice(&start.to_le_bytes());

    // offset 68: length (u32 LE)
    entry[68..72].copy_from_slice(&len.to_le_bytes());

    entry
}

/// Generate the `etc/table-loader` fw_cfg entry.
pub fn generate_table_loader(layout: &AcpiLayout) -> Vec<u8> {
    let mut loader = Vec::new();

    // ── 1. ALLOC commands ──────────────────────────────────────
    // These come first: the firmware copies each blob into guest
    // memory when it processes ALLOC.

    // Tables blob: 64-byte aligned, above 1 MiB.
    loader.extend_from_slice(&loader_alloc_cmd(
        TABLES_BLOB_NAME,
        0x40,
        ZONE_HIGH,
    ));

    // RSDP: 16-byte aligned, in the FSEG zone where a legacy BIOS
    // scan looks for it. OVMF does not require FSEG.
    loader.extend_from_slice(&loader_alloc_cmd(
        RSDP_BLOB_NAME,
        0x10,
        ZONE_FSEG,
    ));

    // ── 2. ADD_POINTER: RSDP → tables blob ────────────────────

    loader.extend_from_slice(&loader_add_pointer_cmd(
        RSDP_BLOB_NAME,
        TABLES_BLOB_NAME,
        16, // offset of rsdt_addr in RSDP
        4,  // 32-bit pointer
    ));

    loader.extend_from_slice(&loader_add_pointer_cmd(
        RSDP_BLOB_NAME,
        TABLES_BLOB_NAME,
        24, // offset of xsdt_addr in RSDP
        8,  // 64-bit pointer
    ));

    // ── 3. ADD_POINTER: FADT → FACS, DSDT ─────────────────────

    // FADT.firmware_ctrl (offset 36 in FADT, 4-byte pointer to FACS)
    loader.extend_from_slice(&loader_add_pointer_cmd(
        TABLES_BLOB_NAME,
        TABLES_BLOB_NAME,
        (layout.fadt_offset + 36) as u32,
        4,
    ));

    // FADT.dsdt (offset 40 in FADT, 4-byte pointer to DSDT)
    loader.extend_from_slice(&loader_add_pointer_cmd(
        TABLES_BLOB_NAME,
        TABLES_BLOB_NAME,
        (layout.fadt_offset + 40) as u32,
        4,
    ));

    // FADT.x_firmware_ctrl (offset 132 in FADT, 8-byte pointer to FACS)
    loader.extend_from_slice(&loader_add_pointer_cmd(
        TABLES_BLOB_NAME,
        TABLES_BLOB_NAME,
        (layout.fadt_offset + 132) as u32,
        8,
    ));

    // FADT.x_dsdt (offset 140 in FADT, 8-byte pointer to DSDT)
    loader.extend_from_slice(&loader_add_pointer_cmd(
        TABLES_BLOB_NAME,
        TABLES_BLOB_NAME,
        (layout.fadt_offset + 140) as u32,
        8,
    ));

    // ── 4. ADD_POINTER: RSDT entries → FADT, MADT, HPET, SPCR,
    // [TPM2]. The count must match the RSDT body that
    // `generate_acpi_layout_inner` builds. Otherwise the firmware
    // patches the wrong slots.

    let mut sdt_entry_count = 4; // FADT, MADT, HPET, SPCR
    if layout.tpm2_size > 0 {
        sdt_entry_count += 1;
    }

    for i in 0..sdt_entry_count {
        loader.extend_from_slice(&loader_add_pointer_cmd(
            TABLES_BLOB_NAME,
            TABLES_BLOB_NAME,
            (layout.rsdt_offset + ACPI_HDR_SIZE + i * 4) as u32,
            4,
        ));
    }

    // ── 5. ADD_POINTER: XSDT entries, the same set as RSDT, 8 bytes.
    for i in 0..sdt_entry_count {
        loader.extend_from_slice(&loader_add_pointer_cmd(
            TABLES_BLOB_NAME,
            TABLES_BLOB_NAME,
            (layout.xsdt_offset + ACPI_HDR_SIZE + i * 8) as u32,
            8,
        ));
    }

    // ── 6. ADD_CHECKSUM commands ──────────────────────────────
    // A table with a standard ACPI header has its checksum at offset 9.
    // The checksum covers the entire table.

    // RSDP has two checksums:
    //   offset 8: covers bytes 0..20 (legacy RSDP v1 portion)
    //   offset 32: covers bytes 0..36 (full RSDP v2)
    loader.extend_from_slice(&loader_add_checksum_cmd(
        RSDP_BLOB_NAME,
        8,
        0,
        20,
    ));
    loader.extend_from_slice(&loader_add_checksum_cmd(
        RSDP_BLOB_NAME,
        32,
        0,
        RSDP_SIZE as u32,
    ));

    loader.extend_from_slice(&loader_add_checksum_cmd(
        TABLES_BLOB_NAME,
        (layout.fadt_offset + 9) as u32,
        layout.fadt_offset as u32,
        layout.fadt_size as u32,
    ));

    loader.extend_from_slice(&loader_add_checksum_cmd(
        TABLES_BLOB_NAME,
        (layout.dsdt_offset + 9) as u32,
        layout.dsdt_offset as u32,
        layout.dsdt_size as u32,
    ));

    loader.extend_from_slice(&loader_add_checksum_cmd(
        TABLES_BLOB_NAME,
        (layout.madt_offset + 9) as u32,
        layout.madt_offset as u32,
        layout.madt_size as u32,
    ));

    loader.extend_from_slice(&loader_add_checksum_cmd(
        TABLES_BLOB_NAME,
        (layout.hpet_offset + 9) as u32,
        layout.hpet_offset as u32,
        layout.hpet_size as u32,
    ));

    loader.extend_from_slice(&loader_add_checksum_cmd(
        TABLES_BLOB_NAME,
        (layout.spcr_offset + 9) as u32,
        layout.spcr_offset as u32,
        layout.spcr_size as u32,
    ));

    if layout.tpm2_size > 0 {
        loader.extend_from_slice(&loader_add_checksum_cmd(
            TABLES_BLOB_NAME,
            (layout.tpm2_offset + 9) as u32,
            layout.tpm2_offset as u32,
            layout.tpm2_size as u32,
        ));
    }

    loader.extend_from_slice(&loader_add_checksum_cmd(
        TABLES_BLOB_NAME,
        (layout.rsdt_offset + 9) as u32,
        layout.rsdt_offset as u32,
        layout.rsdt_size as u32,
    ));

    loader.extend_from_slice(&loader_add_checksum_cmd(
        TABLES_BLOB_NAME,
        (layout.xsdt_offset + 9) as u32,
        layout.xsdt_offset as u32,
        layout.xsdt_size as u32,
    ));

    // The FACS has no SDT header and so no checksum.

    loader
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acpi::test_support::TEST_TPM_DEVICE;
    use crate::acpi::{
        generate_acpi_layout, generate_acpi_layout_with_tpm2, TpmAcpi,
    };
    use std::mem::size_of;

    #[test]
    fn loader_entry_size_is_128() {
        assert_eq!(LOADER_ENTRY_SIZE, 128);
        assert_eq!(size_of::<[u8; LOADER_ENTRY_SIZE]>(), 128,);
    }

    #[test]
    fn loader_alloc_cmd_format() {
        let cmd = loader_alloc_cmd("etc/acpi/tables", 0x40, ZONE_HIGH);
        assert_eq!(cmd.len(), 128);

        // Command type
        let cmd_type = u32::from_le_bytes(cmd[0..4].try_into().unwrap());
        assert_eq!(cmd_type, LOADER_CMD_ALLOC);

        // Name
        assert_eq!(&cmd[4..4 + 15], b"etc/acpi/tables");
        assert_eq!(cmd[4 + 15], 0); // NUL terminated

        // Alignment
        let align = u32::from_le_bytes(cmd[60..64].try_into().unwrap());
        assert_eq!(align, 0x40);

        // Zone
        assert_eq!(cmd[64], ZONE_HIGH);

        // Rest should be zero
        for &b in &cmd[65..] {
            assert_eq!(b, 0);
        }
    }

    #[test]
    fn loader_add_pointer_cmd_format() {
        let cmd =
            loader_add_pointer_cmd("etc/acpi/rsdp", "etc/acpi/tables", 16, 4);
        assert_eq!(cmd.len(), 128);

        let cmd_type = u32::from_le_bytes(cmd[0..4].try_into().unwrap());
        assert_eq!(cmd_type, LOADER_CMD_ADD_POINTER);

        // dest_name
        assert_eq!(&cmd[4..4 + 13], b"etc/acpi/rsdp");
        // src_name
        assert_eq!(&cmd[60..60 + 15], b"etc/acpi/tables");

        // offset
        let off = u32::from_le_bytes(cmd[116..120].try_into().unwrap());
        assert_eq!(off, 16);

        // size
        assert_eq!(cmd[120], 4);
    }

    #[test]
    fn loader_add_checksum_cmd_format() {
        let cmd = loader_add_checksum_cmd("etc/acpi/rsdp", 8, 0, 20);
        assert_eq!(cmd.len(), 128);

        let cmd_type = u32::from_le_bytes(cmd[0..4].try_into().unwrap());
        assert_eq!(cmd_type, LOADER_CMD_ADD_CHECKSUM);

        // name
        assert_eq!(&cmd[4..4 + 13], b"etc/acpi/rsdp");

        // checksum offset
        let cksum_off = u32::from_le_bytes(cmd[60..64].try_into().unwrap());
        assert_eq!(cksum_off, 8);

        // start
        let start = u32::from_le_bytes(cmd[64..68].try_into().unwrap());
        assert_eq!(start, 0);

        // length
        let length = u32::from_le_bytes(cmd[68..72].try_into().unwrap());
        assert_eq!(length, 20);
    }

    #[test]
    fn loader_total_size_is_multiple_of_128() {
        let layout = generate_acpi_layout(4);
        let loader = generate_table_loader(&layout);
        assert_eq!(loader.len() % LOADER_ENTRY_SIZE, 0);
        assert!(!loader.is_empty());
    }

    #[test]
    fn loader_has_expected_command_count() {
        let layout = generate_acpi_layout(2);
        let loader = generate_table_loader(&layout);
        let count = loader.len() / LOADER_ENTRY_SIZE;

        // Expected commands:
        //   2 ALLOC (tables, rsdp)
        //   2 ADD_POINTER (RSDP -> RSDT, RSDP -> XSDT)
        //   4 ADD_POINTER (FADT -> FACS x2, FADT -> DSDT x2)
        //   4 ADD_POINTER (RSDT entries: FADT, MADT, HPET, SPCR)
        //   4 ADD_POINTER (XSDT entries: FADT, MADT, HPET, SPCR)
        //   2 ADD_CHECKSUM (RSDP v1, RSDP v2)
        //   7 ADD_CHECKSUM (FADT, DSDT, MADT, HPET, SPCR, RSDT, XSDT)
        // Total: 25
        assert_eq!(count, 25, "expected 25 loader commands, got {}", count);
    }

    #[test]
    fn layout_with_tpm2_includes_table_and_routes_pointers() {
        // A 76-byte table with only the signature and length set. It
        // keeps the vmm-tpm crate out of this test.
        let mut tpm2 = vec![0u8; 76];
        tpm2[0..4].copy_from_slice(b"TPM2");
        tpm2[4..8].copy_from_slice(&76u32.to_le_bytes());

        let layout = generate_acpi_layout_with_tpm2(
            2,
            None,
            Some(TpmAcpi {
                table: tpm2.clone(),
                device: TEST_TPM_DEVICE,
            }),
        );
        assert_eq!(layout.tpm2_size, 76);
        assert!(layout.tpm2_offset > layout.spcr_offset);

        // TPM2 must land in the tables blob and keep its signature.
        assert_eq!(
            &layout.tables[layout.tpm2_offset..layout.tpm2_offset + 4],
            b"TPM2"
        );

        // TPM2 adds one RSDT and one XSDT entry. The entry count is
        // (length - header) / entry size.
        let rsdt_len = u32::from_le_bytes(
            layout.tables[layout.rsdt_offset + 4..layout.rsdt_offset + 8]
                .try_into()
                .unwrap(),
        ) as usize;
        let rsdt_entry_count = (rsdt_len - ACPI_HDR_SIZE) / 4;
        assert_eq!(rsdt_entry_count, 5, "FADT, MADT, HPET, SPCR, TPM2");

        let xsdt_len = u32::from_le_bytes(
            layout.tables[layout.xsdt_offset + 4..layout.xsdt_offset + 8]
                .try_into()
                .unwrap(),
        ) as usize;
        let xsdt_entry_count = (xsdt_len - ACPI_HDR_SIZE) / 8;
        assert_eq!(xsdt_entry_count, 5);

        // Loader picks up the extra entries: +1 RSDT pointer fixup,
        // +1 XSDT pointer fixup, +1 TPM2 checksum command.
        let loader = generate_table_loader(&layout);
        let count = loader.len() / LOADER_ENTRY_SIZE;
        assert_eq!(count, 28, "expected 28 loader commands with TPM2");
    }

    #[test]
    fn loader_allocs_come_first() {
        let layout = generate_acpi_layout(1);
        let loader = generate_table_loader(&layout);

        let cmd0 = u32::from_le_bytes(loader[0..4].try_into().unwrap());
        let cmd1 = u32::from_le_bytes(loader[128..132].try_into().unwrap());
        assert_eq!(cmd0, LOADER_CMD_ALLOC);
        assert_eq!(cmd1, LOADER_CMD_ALLOC);

        let cmd2 = u32::from_le_bytes(loader[256..260].try_into().unwrap());
        assert_ne!(cmd2, LOADER_CMD_ALLOC);
    }

    #[test]
    #[should_panic(expected = "loader name too long")]
    fn loader_name_too_long_panics() {
        let long_name = "a".repeat(56); // exactly 56 = too long (max 55 + NUL)
        loader_alloc_cmd(&long_name, 1, ZONE_HIGH);
    }

    #[test]
    fn loader_simulated_firmware_pointer_patching() {
        // Simulate what the firmware does: allocate blobs, then
        // patch pointers by adding the base address.
        let layout = generate_acpi_layout(2);

        let tables_base: u64 = 0x7FFE_0000;
        let mut tables = layout.tables.clone();

        // Patch FADT.firmware_ctrl: currently holds facs_offset,
        // firmware adds tables_base
        let off = layout.fadt_offset + 36;
        let val = u32::from_le_bytes(tables[off..off + 4].try_into().unwrap());
        assert_eq!(val as usize, layout.facs_offset);
        let patched = val as u64 + tables_base;
        tables[off..off + 4].copy_from_slice(&(patched as u32).to_le_bytes());

        assert_eq!(patched, tables_base + layout.facs_offset as u64,);

        // Same for RSDT entry 0 (FADT)
        let off = layout.rsdt_offset + ACPI_HDR_SIZE;
        let val = u32::from_le_bytes(tables[off..off + 4].try_into().unwrap());
        assert_eq!(val as usize, layout.fadt_offset);
        let patched = val as u64 + tables_base;
        assert_eq!(patched, tables_base + layout.fadt_offset as u64,);
    }
}
