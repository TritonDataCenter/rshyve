// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::*;

/// The window `write_smbios_tables` has below the legacy ACPI tables.
const TEST_WINDOW: usize = 0xF2400 - 0xF1000;

fn smbios(config: &SmbiosConfig) -> (Vec<u8>, Vec<u8>) {
    generate_smbios(config, TEST_WINDOW).expect("table fits the window")
}

#[test]
fn entry_point_signature() {
    let config = SmbiosConfig {
        vm_name: "test-vm".to_string(),
        num_cpus: 1,
        memory_mb: 256,
        ..Default::default()
    };
    let (anchor, _tables) = smbios(&config);

    assert_eq!(anchor.len(), 31);
    assert_eq!(&anchor[0x00..0x04], b"_SM_");
    assert_eq!(&anchor[0x10..0x15], b"_DMI_");
}

#[test]
fn entry_point_checksums_valid() {
    let config = SmbiosConfig {
        vm_name: "test-vm".to_string(),
        num_cpus: 2,
        memory_mb: 1024,
        ..Default::default()
    };
    let (anchor, _) = smbios(&config);

    // Entry point checksum (bytes 0x00..0x10)
    let sum: u8 = anchor[0x00..0x10]
        .iter()
        .fold(0u8, |acc, &b| acc.wrapping_add(b));
    assert_eq!(sum, 0, "entry point checksum invalid");

    // Intermediate checksum (bytes 0x10..0x1F)
    let sum: u8 = anchor[0x10..0x1F]
        .iter()
        .fold(0u8, |acc, &b| acc.wrapping_add(b));
    assert_eq!(sum, 0, "intermediate checksum invalid");
}

#[test]
fn entry_point_version() {
    let config = SmbiosConfig {
        vm_name: "vm".to_string(),
        num_cpus: 1,
        memory_mb: 256,
        ..Default::default()
    };
    let (anchor, _) = smbios(&config);

    assert_eq!(anchor[0x06], 2); // major
    assert_eq!(anchor[0x07], 8); // minor
    assert_eq!(anchor[0x1E], 0x28); // BCD revision
}

#[test]
fn tables_end_with_type127() {
    let config = SmbiosConfig {
        vm_name: "vm".to_string(),
        num_cpus: 1,
        memory_mb: 256,
        ..Default::default()
    };
    let (_, tables) = smbios(&config);

    // Type 127 is type(1) + length(1) + handle(2) + double-NUL(2),
    // 6 bytes in total.
    assert!(tables.len() >= 6);
    let eot_start = tables.len() - 6;
    assert_eq!(tables[eot_start], SMBIOS_TYPE_EOT);
    assert_eq!(tables[eot_start + 1], 4); // length = header only

    assert_eq!(tables[tables.len() - 2], 0);
    assert_eq!(tables[tables.len() - 1], 0);
}

#[test]
fn table_count_matches_entry_point() {
    let config = SmbiosConfig {
        vm_name: "vm".to_string(),
        num_cpus: 4,
        memory_mb: 2048,
        ..Default::default()
    };
    let (anchor, _) = smbios(&config);

    let num_structs = u16::from_le_bytes([anchor[0x1C], anchor[0x1D]]);
    // Types: 0, 1, 2, 3, 4*4, 16, 17, 19, 32, 127 = 13
    assert_eq!(num_structs, 13);
}

#[test]
fn uuid_parsing() {
    let uuid = parse_uuid(Some("12345678-1234-5678-9abc-def012345678"));
    // First 4 bytes are LE of 0x12345678 → [0x78, 0x56, 0x34, 0x12]
    assert_eq!(uuid[0], 0x78);
    assert_eq!(uuid[1], 0x56);
    assert_eq!(uuid[2], 0x34);
    assert_eq!(uuid[3], 0x12);
    // Bytes 4-5 are LE of 0x1234 → [0x34, 0x12]
    assert_eq!(uuid[4], 0x34);
    assert_eq!(uuid[5], 0x12);
    // Bytes 6-7 are LE of 0x5678 → [0x78, 0x56]
    assert_eq!(uuid[6], 0x78);
    assert_eq!(uuid[7], 0x56);
    // Remaining bytes are big-endian
    assert_eq!(uuid[8], 0x9a);
    assert_eq!(uuid[9], 0xbc);
}

#[test]
fn uuid_none_returns_zeroes() {
    let uuid = parse_uuid(None);
    assert_eq!(uuid, [0u8; 16]);
}

#[test]
fn uuid_invalid_returns_zeroes() {
    let uuid = parse_uuid(Some("not-a-uuid"));
    assert_eq!(uuid, [0u8; 16]);
}

#[test]
fn structures_have_double_nul_termination() {
    let config = SmbiosConfig {
        vm_name: "test".to_string(),
        num_cpus: 1,
        memory_mb: 256,
        ..Default::default()
    };
    let (_, tables) = smbios(&config);

    // Walk structures and verify each ends with double-NUL
    let mut offset = 0;
    let mut count = 0;
    while offset < tables.len() {
        assert!(
            offset + 4 <= tables.len(),
            "not enough bytes for header at structure #{count}"
        );
        let stype = tables[offset];
        let slen = tables[offset + 1] as usize;
        assert!(
            slen >= 4,
            "structure #{count} (type {stype}) has length < 4"
        );

        // The string section starts after the formatted area.
        let mut pos = offset + slen;

        loop {
            assert!(
                pos + 1 < tables.len(),
                "ran off end of table at structure #{count} (type {stype})"
            );
            if tables[pos] == 0 && tables[pos + 1] == 0 {
                pos += 2; // skip past the double-NUL
                break;
            }
            pos += 1;
        }

        count += 1;
        offset = pos;

        if stype == SMBIOS_TYPE_EOT {
            break;
        }
    }

    assert!(count > 0, "no structures found");
}

#[test]
fn table_length_matches_entry_point() {
    let config = SmbiosConfig {
        vm_name: "vm".to_string(),
        num_cpus: 1,
        memory_mb: 512,
        ..Default::default()
    };
    let (anchor, tables) = smbios(&config);

    let recorded_len = u16::from_le_bytes([anchor[0x16], anchor[0x17]]);
    assert_eq!(recorded_len as usize, tables.len());
}

#[test]
fn an_empty_b_value_is_refused() {
    // An empty string emits a bare NUL, which is the double-NUL that
    // ends the structure: dmidecode then reads the strings that follow
    // as the next structure header.
    for key in [
        "manufacturer",
        "product",
        "version",
        "serial",
        "sku",
        "family",
    ] {
        let err = parse_smbios_flag(&format!("1,{key}="))
            .expect_err("an empty value has no SMBIOS encoding");
        assert!(
            matches!(err, SmbiosError::EmptyValue { key: k } if k == key),
            "{key}: {err}"
        );
    }
    // A value that is only a space is a legal SMBIOS string.
    let cfg = parse_smbios_flag("1,serial= ").expect("a space is a string");
    assert_eq!(cfg.serial.as_deref(), Some(" "));
}

#[test]
fn parse_keeps_the_values_it_knows_and_ignores_the_rest() {
    let cfg = parse_smbios_flag("1,manufacturer=MNX,product=vm,nope=x")
        .expect("valid values");
    assert_eq!(cfg.manufacturer.as_deref(), Some("MNX"));
    assert_eq!(cfg.product.as_deref(), Some("vm"));
    assert_eq!(cfg.version, None);
    // Other structure types carry no keys this generator reads.
    let cfg = parse_smbios_flag("3,serial=").expect("type 3 is ignored");
    assert_eq!(cfg.serial, None);
}

#[test]
fn a_vm_with_no_memory_is_refused() {
    // Type 17 and Type 19 describe 0..memory_mb, and `mem_kb - 1`
    // underflows into a 4 GiB range at memory_mb == 0.
    let config = SmbiosConfig {
        num_cpus: 1,
        memory_mb: 0,
        ..Default::default()
    };
    assert!(matches!(
        generate_smbios(&config, TEST_WINDOW),
        Err(SmbiosError::NoMemory)
    ));
}

#[test]
fn a_table_that_would_reach_the_legacy_acpi_tables_is_refused() {
    // Each of 64 vCPUs adds 70 bytes, and the -B strings are unbounded.
    // The legacy ACPI tables sit 5120 bytes above the structure table.
    let long = "x".repeat(300);
    let config = SmbiosConfig {
        num_cpus: 64,
        memory_mb: 1024,
        manufacturer: Some(long.clone()),
        product: Some(long.clone()),
        version: Some(long.clone()),
        serial: Some(long.clone()),
        sku: Some(long.clone()),
        family: Some(long),
        ..Default::default()
    };
    let err = generate_smbios(&config, TEST_WINDOW)
        .expect_err("the table must not run into the ACPI tables");
    let SmbiosError::TableTooLarge { len, max } = err else {
        panic!("wrong error: {err}");
    };
    assert_eq!(max, TEST_WINDOW);
    assert!(len > max, "{len} vs {max}");

    // The same VM without the operator strings still fits.
    let plain = SmbiosConfig {
        num_cpus: 64,
        memory_mb: 1024,
        ..Default::default()
    };
    let (_, tables) = smbios(&plain);
    assert!(tables.len() <= TEST_WINDOW);
}
