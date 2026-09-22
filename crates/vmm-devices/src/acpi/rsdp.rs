// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Root pointer and root tables: RSDP, RSDT and XSDT.

use super::{
    fix_checksum, fix_checksum_at, write_header, ACPI_HDR_SIZE, OEM_ID,
};

pub(super) fn build_rsdt(table_addrs: &[u32]) -> Vec<u8> {
    let total = ACPI_HDR_SIZE + table_addrs.len() * 4;
    let mut buf = Vec::with_capacity(total);

    write_header(&mut buf, b"RSDT", total as u32, 1);
    for &addr in table_addrs {
        buf.extend_from_slice(&addr.to_le_bytes());
    }

    assert_eq!(buf.len(), total);
    fix_checksum(&mut buf, 0, total);
    buf
}

pub(super) fn build_xsdt(table_addrs: &[u64]) -> Vec<u8> {
    let total = ACPI_HDR_SIZE + table_addrs.len() * 8;
    let mut buf = Vec::with_capacity(total);

    write_header(&mut buf, b"XSDT", total as u32, 1);
    for &addr in table_addrs {
        buf.extend_from_slice(&addr.to_le_bytes());
    }

    assert_eq!(buf.len(), total);
    fix_checksum(&mut buf, 0, total);
    buf
}

// ── RSDP ──────────────────────────────────────────────────────────

pub(super) const RSDP_SIZE: usize = 36;

pub(super) fn build_rsdp(rsdt_addr: u32, xsdt_addr: u64) -> Vec<u8> {
    let mut buf = vec![0u8; RSDP_SIZE];

    // offset 0: Signature "RSD PTR " (8 bytes)
    buf[0..8].copy_from_slice(b"RSD PTR ");
    // offset 8: Checksum (covers first 20 bytes, fixed later)
    buf[8] = 0;
    // offset 9: OEM ID (6 bytes)
    buf[9..15].copy_from_slice(OEM_ID);
    // offset 15: Revision (2 for ACPI 2.0+)
    buf[15] = 2;
    // offset 16: RSDT address (u32 LE)
    buf[16..20].copy_from_slice(&rsdt_addr.to_le_bytes());
    // offset 20: Length (u32 LE) = 36 for RSDP v2
    buf[20..24].copy_from_slice(&(RSDP_SIZE as u32).to_le_bytes());
    // offset 24: XSDT address (u64 LE)
    buf[24..32].copy_from_slice(&xsdt_addr.to_le_bytes());
    // offset 32: Extended checksum (covers all 36 bytes, fixed later)
    buf[32] = 0;
    // offset 33..36: reserved
    buf[33] = 0;
    buf[34] = 0;
    buf[35] = 0;

    // Fix checksum for first 20 bytes (stored at offset 8)
    fix_checksum_at(&mut buf, 8, 0, 20);
    // Fix extended checksum for all 36 bytes (stored at offset 32)
    fix_checksum_at(&mut buf, 32, 0, RSDP_SIZE);

    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acpi::test_support::verify_checksum;

    #[test]
    fn rsdt_checksum() {
        let rsdt = build_rsdt(&[0x1000, 0x2000, 0x3000]);
        assert_eq!(&rsdt[0..4], b"RSDT");
        assert!(verify_checksum(&rsdt));
        // Should have 3 pointers after header
        assert_eq!(rsdt.len(), ACPI_HDR_SIZE + 12);
    }

    #[test]
    fn xsdt_checksum() {
        let xsdt = build_xsdt(&[0x1000, 0x2000, 0x3000]);
        assert_eq!(&xsdt[0..4], b"XSDT");
        assert!(verify_checksum(&xsdt));
        // Should have 3 pointers (8 bytes each) after header
        assert_eq!(xsdt.len(), ACPI_HDR_SIZE + 24);
    }

    #[test]
    fn rsdp_checksums() {
        let rsdp = build_rsdp(0x1000, 0x2000);
        assert_eq!(rsdp.len(), RSDP_SIZE);
        assert_eq!(&rsdp[0..8], b"RSD PTR ");

        // First 20 bytes must checksum to 0
        let sum20: u8 =
            rsdp[..20].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum20, 0);

        // All 36 bytes must checksum to 0
        let sum36: u8 = rsdp.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum36, 0);
    }

    #[test]
    fn rsdp_revision() {
        let rsdp = build_rsdp(0, 0);
        assert_eq!(rsdp[15], 2); // ACPI 2.0+
    }
}
