// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fixtures shared by the ACPI submodule tests.

use super::{TpmAcpi, TpmDevice};

pub(super) const TEST_TPM_DEVICE: TpmDevice = TpmDevice {
    crb_base: 0xFED4_0000,
    crb_len: 0x1000,
};
const TPM_HID_AML: &[u8] = b"\x0dMSFT0101\0";
pub(super) const TPM_DEVICE_AML: [u8; 55] = [
    0x5B, 0x82, 0x35, b'T', b'P', b'M', b'_', 0x08, b'_', b'H', b'I', b'D',
    0x0D, b'M', b'S', b'F', b'T', b'0', b'1', b'0', b'1', 0x00, 0x08, b'_',
    b'S', b'T', b'A', 0x0A, 0x0F, 0x08, b'_', b'C', b'R', b'S', 0x11, 0x14,
    0x0C, 0x0E, 0x00, 0x00, 0x00, 0x86, 0x09, 0x00, 0x01, 0x00, 0x00, 0xD4,
    0xFE, 0x00, 0x10, 0x00, 0x00, 0x79, 0x00,
];

/// Verify that the sum of all bytes in a range is 0 mod 256.
pub(super) fn verify_checksum(data: &[u8]) -> bool {
    data.iter().fold(0u8, |acc, &b| acc.wrapping_add(b)) == 0
}

pub(super) fn test_tpm_acpi() -> TpmAcpi {
    let mut table = vec![0u8; 76];
    table[0..4].copy_from_slice(b"TPM2");
    table[4..8].copy_from_slice(&76u32.to_le_bytes());
    let control_area = u64::from(TEST_TPM_DEVICE.crb_base) + 0x40;
    table[40..48].copy_from_slice(&control_area.to_le_bytes());
    TpmAcpi {
        table,
        device: TEST_TPM_DEVICE,
    }
}

pub(super) fn contains_tpm_hid(buf: &[u8]) -> bool {
    buf.windows(TPM_HID_AML.len())
        .any(|window| window == TPM_HID_AML)
}

pub(super) fn tpm_crs_window(buf: &[u8], expected_base: u32) -> (u32, u32) {
    for descriptor in buf.windows(12) {
        if descriptor[0] != 0x86 {
            continue;
        }
        let base = u32::from_le_bytes(descriptor[4..8].try_into().unwrap());
        if base != expected_base {
            continue;
        }
        assert_eq!(u16::from_le_bytes(descriptor[1..3].try_into().unwrap()), 9,);
        let len = u32::from_le_bytes(descriptor[8..12].try_into().unwrap());
        return (base, len);
    }
    panic!("TPM Memory32Fixed descriptor not found");
}
