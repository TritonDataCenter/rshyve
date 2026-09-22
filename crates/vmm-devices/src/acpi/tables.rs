// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Small fixed-format tables: HPET and SPCR.

use super::{
    fix_checksum, write_gas, write_header, ACPI_AS_SYSTEM_IO, ACPI_GAS_BYTE,
    HPET_ADDR,
};

const HPET_TABLE_SIZE: u32 = 56;

pub(super) fn build_hpet() -> Vec<u8> {
    let mut buf = Vec::with_capacity(HPET_TABLE_SIZE as usize);

    // SDT header
    write_header(&mut buf, b"HPET", HPET_TABLE_SIZE, 1);

    // offset 36: Event Timer Block ID (u32 LE): hardware rev ID,
    // comparator count, counter size. Left 0.
    buf.extend_from_slice(&0u32.to_le_bytes());

    // offset 40: Base Address (GAS, 12 bytes) - MMIO at HPET_ADDR
    buf.push(0); // address_space = system memory
    buf.push(0); // bit_width
    buf.push(0); // bit_offset
    buf.push(0); // access_size (legacy)
    buf.extend_from_slice(&(HPET_ADDR as u64).to_le_bytes());

    // offset 52: Sequence Number (u8)
    buf.push(0);
    // offset 53: Minimum Clock Tick (u16 LE)
    buf.extend_from_slice(&0u16.to_le_bytes());
    // offset 55: Page Protection and OEM Attribute (u8)
    buf.push(0x04); // ACPI_HPET_PAGE_PROTECT4

    assert_eq!(buf.len(), HPET_TABLE_SIZE as usize);
    fix_checksum(&mut buf, 0, HPET_TABLE_SIZE as usize);

    buf
}

// ── SPCR (Serial Port Console Redirection) ──────────────────────

/// SPCR table size (80 bytes: 36 header + 44 body).
const SPCR_TABLE_SIZE: u32 = 80;

pub(super) fn build_spcr() -> Vec<u8> {
    let mut buf = Vec::with_capacity(SPCR_TABLE_SIZE as usize);

    write_header(&mut buf, b"SPCR", SPCR_TABLE_SIZE, 1);

    // offset 36: Interface Type (1B) = 16550 compatible
    buf.push(0x00); // Full 16550

    // offset 37-39: Reserved
    buf.extend_from_slice(&[0u8; 3]);

    // offset 40: Base Address (GAS, 12 bytes) - I/O port 0x3F8
    write_gas(&mut buf, ACPI_AS_SYSTEM_IO, 8, 0, ACPI_GAS_BYTE, 0x3F8);

    // offset 52: Interrupt Type (1B)
    buf.push(0x01); // Dual-8259 compatible

    // offset 53: IRQ (1B) - ISA IRQ 4
    buf.push(4);
    // offset 54-57: Global System Interrupt (4B LE)
    buf.extend_from_slice(&4u32.to_le_bytes());
    // offset 58: Baud Rate (1B) - 7 = 115200
    buf.push(7);
    // offset 59: Parity (1B) - 0 = No parity
    buf.push(0);
    // offset 60: Stop Bits (1B) - 1 = 1 stop bit
    buf.push(1);
    // offset 61: Flow Control (1B) - 0 = None
    buf.push(0);
    // offset 62: Terminal Type (1B) - 0 = VT100
    buf.push(0);
    // offset 63: Language (1B) - Reserved
    buf.push(0);
    // offset 64-65: PCI Device ID (2B) - 0xFFFF = not PCI
    buf.extend_from_slice(&0xFFFFu16.to_le_bytes());
    // offset 66-67: PCI Vendor ID (2B) - 0xFFFF
    buf.extend_from_slice(&0xFFFFu16.to_le_bytes());
    // offset 68: PCI Bus (1B)
    buf.push(0);
    // offset 69: PCI Device (1B)
    buf.push(0);
    // offset 70: PCI Function (1B)
    buf.push(0);
    // offset 71-74: PCI Flags (4B)
    buf.extend_from_slice(&0u32.to_le_bytes());
    // offset 75: PCI Segment (1B)
    buf.push(0);
    // offset 76-79: Reserved (4B)
    buf.extend_from_slice(&0u32.to_le_bytes());

    assert_eq!(buf.len(), SPCR_TABLE_SIZE as usize);
    fix_checksum(&mut buf, 0, SPCR_TABLE_SIZE as usize);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acpi::test_support::verify_checksum;

    #[test]
    fn hpet_checksum() {
        let hpet = build_hpet();
        assert_eq!(hpet.len(), HPET_TABLE_SIZE as usize);
        assert_eq!(&hpet[0..4], b"HPET");
        assert!(verify_checksum(&hpet));
    }
}
