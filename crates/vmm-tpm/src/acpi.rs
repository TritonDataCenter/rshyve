// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! TPM2 ACPI table builder (TCG ACPI Specification, Family 2.0 Rev 1.4).
//!
//! Windows 11 / Server 2022 detect TPM 2.0 via this table:
//!   * Signature "TPM2"
//!   * Start Method = 7 (CRB) → tells the OS to drive the device by
//!     writing the START bit in the Control Area
//!   * Control Area Address = `CRB_BASE + 0x40` (the CTRL_REQ register;
//!     the rest of the CRB layout is fixed offset from there)

use crate::crb::CRB_BASE;

/// Header + body layout of the TPM2 ACPI table (Rev 4).
///
/// Total length is 76 bytes: a 36-byte header and a 40-byte body. The
/// body holds 12 bytes of Start Method Specific Parameters, which CRB
/// leaves zero.
const TPM2_TABLE_LEN: u32 = 76;

const SIGNATURE: &[u8; 4] = b"TPM2";
const REVISION: u8 = 4;
const OEM_ID: &[u8; 6] = b"VMMNEW";
const OEM_TABLE_ID: &[u8; 8] = b"VMMTPM2 ";
const OEM_REVISION: u32 = 1;
const CREATOR_ID: &[u8; 4] = b"VMM ";
const CREATOR_REVISION: u32 = 1;

const PLATFORM_CLASS_CLIENT: u16 = 0;
/// Per TCG ACPI Spec table 7-6: 7 == Command Response Buffer
/// interface, which the `crb` module implements.
const START_METHOD_CRB: u32 = 7;

/// Build the 76-byte TPM2 ACPI table with the ACPI checksum filled in.
///
/// The caller places this buffer in guest memory at a known GPA and
/// adds it to the RSDT/XSDT entry list.
pub fn build_tpm2_table() -> Vec<u8> {
    let mut t = vec![0u8; TPM2_TABLE_LEN as usize];

    // ACPI standard 36-byte header.
    t[0..4].copy_from_slice(SIGNATURE);
    t[4..8].copy_from_slice(&TPM2_TABLE_LEN.to_le_bytes());
    t[8] = REVISION;
    t[9] = 0; // checksum, filled in below
    t[10..16].copy_from_slice(OEM_ID);
    t[16..24].copy_from_slice(OEM_TABLE_ID);
    t[24..28].copy_from_slice(&OEM_REVISION.to_le_bytes());
    t[28..32].copy_from_slice(CREATOR_ID);
    t[32..36].copy_from_slice(&CREATOR_REVISION.to_le_bytes());

    // TPM2 body (offsets relative to start of table).
    //   36..38  Platform Class (u16): 0 = client
    //   38..40  Reserved (u16)
    //   40..48  Address of Control Area (u64) = CRB_BASE + 0x40
    //   48..52  Start Method (u32)
    //   52..64  Start Method Specific Parameters (12 bytes, zeroed for CRB)
    //   64..68  Log Area Minimum Length (u32)
    //   68..76  Log Area Start Address (u64), 0 means "no log"
    t[36..38].copy_from_slice(&PLATFORM_CLASS_CLIENT.to_le_bytes());
    let control_area = CRB_BASE + 0x40;
    t[40..48].copy_from_slice(&control_area.to_le_bytes());
    t[48..52].copy_from_slice(&START_METHOD_CRB.to_le_bytes());
    // Start Method Specific Parameters stay zero. The Log Area stays
    // zero too: UEFI firmware allocates and tracks the event log.

    // ACPI checksum: bytes sum to 0 (mod 256).
    let mut sum: u8 = 0;
    for b in t.iter() {
        sum = sum.wrapping_add(*b);
    }
    t[9] = (0u8).wrapping_sub(sum);
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_fields_present() {
        let t = build_tpm2_table();
        assert_eq!(&t[0..4], SIGNATURE);
        assert_eq!(
            u32::from_le_bytes(t[4..8].try_into().unwrap()),
            TPM2_TABLE_LEN
        );
        assert_eq!(t[8], REVISION);
        assert_eq!(&t[10..16], OEM_ID);
        assert_eq!(&t[16..24], OEM_TABLE_ID);
    }

    #[test]
    fn checksum_zeroes() {
        let t = build_tpm2_table();
        let sum: u32 = t.iter().map(|&b| b as u32).sum();
        assert_eq!(sum & 0xFF, 0, "ACPI checksum mismatch");
    }

    #[test]
    fn control_area_address_correct() {
        let t = build_tpm2_table();
        let addr = u64::from_le_bytes(t[40..48].try_into().unwrap());
        assert_eq!(addr, CRB_BASE + 0x40);
    }

    #[test]
    fn start_method_is_crb() {
        let t = build_tpm2_table();
        let sm = u32::from_le_bytes(t[48..52].try_into().unwrap());
        assert_eq!(sm, 7, "Start Method must be 7 (CRB) for Windows 11");
    }
}
