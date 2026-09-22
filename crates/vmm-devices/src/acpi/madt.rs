// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! MADT (Multiple APIC Description Table).

use super::{
    fix_checksum, write_header, AcpiConfig, ACPI_HDR_SIZE, IOAPIC_ADDR,
    LAPIC_ADDR, SCI_INT,
};

// MADT entry types
const MADT_TYPE_LOCAL_APIC: u8 = 0;
const MADT_TYPE_IO_APIC: u8 = 1;
const MADT_TYPE_INT_OVERRIDE: u8 = 2;
const MADT_TYPE_LOCAL_APIC_NMI: u8 = 4;

// MADT flags
const MADT_PCAT_COMPAT: u32 = 1;

// ACPI 6.x Local APIC Flags. Enabled means the CPU is usable now.
// Online Capable without Enabled means the slot is present but offline,
// so the guest keeps room for it and does not try to boot it.
const MADT_LAPIC_ENABLED: u32 = 1 << 0;
const MADT_LAPIC_ONLINE_CAPABLE: u32 = 1 << 1;

// Interrupt polarity/trigger
const MADT_POLARITY_ACTIVE_HIGH: u16 = 0x01;
const MADT_POLARITY_ACTIVE_LOW: u16 = 0x03;
const MADT_TRIGGER_EDGE: u16 = 0x04;
const MADT_TRIGGER_LEVEL: u16 = 0x0C;

pub(super) fn build_madt(cfg: &AcpiConfig) -> Vec<u8> {
    let madt_specific = 8usize; // LAPIC addr (4) + flags (4)

    // A caller-supplied count must not wrap the size calculation.
    let lapic_entries = (cfg.max_cpus as usize)
        .checked_mul(8)
        .expect("CPU count overflows the MADT size");
    let ioapic_entry = 12usize;
    let irq_overrides = 2 * 10usize;
    let lapic_nmi = 6usize;
    let total = ACPI_HDR_SIZE
        + madt_specific
        + lapic_entries
        + ioapic_entry
        + irq_overrides
        + lapic_nmi;

    let mut buf = Vec::with_capacity(total);

    write_header(&mut buf, b"APIC", total as u32, 1);

    // MADT-specific fields (after header)
    // Local APIC address (u32 LE)
    buf.extend_from_slice(&LAPIC_ADDR.to_le_bytes());
    // Flags (u32 LE) - PCAT_COMPAT
    buf.extend_from_slice(&MADT_PCAT_COMPAT.to_le_bytes());

    // Local APIC entries: one per advertised CPU slot. The slots above
    // the boot count are offline so the guest sizes its per-CPU state
    // for a later hot-add without booting a CPU that does not exist.
    for id in 0..cfg.max_cpus {
        let flags = if id < cfg.num_cpus {
            MADT_LAPIC_ENABLED
        } else {
            MADT_LAPIC_ONLINE_CAPABLE
        };
        buf.push(MADT_TYPE_LOCAL_APIC); // type
        buf.push(8); // length
        buf.push(id as u8); // processor ID
        buf.push(id as u8); // APIC ID
        buf.extend_from_slice(&flags.to_le_bytes()); // flags
    }

    // I/O APIC entry
    buf.push(MADT_TYPE_IO_APIC); // type
    buf.push(12); // length
    buf.push(0); // I/O APIC ID
    buf.push(0); // reserved
    buf.extend_from_slice(&IOAPIC_ADDR.to_le_bytes()); // address
    buf.extend_from_slice(&0u32.to_le_bytes()); // GSI base

    // Interrupt override: IRQ 0 -> GSI 2 (edge, active-high)
    buf.push(MADT_TYPE_INT_OVERRIDE); // type
    buf.push(10); // length
    buf.push(0); // bus (ISA)
    buf.push(0); // source IRQ
    buf.extend_from_slice(&2u32.to_le_bytes()); // GSI
    let inti_flags = MADT_POLARITY_ACTIVE_HIGH | MADT_TRIGGER_EDGE;
    buf.extend_from_slice(&inti_flags.to_le_bytes());

    // Interrupt override: SCI IRQ 9 -> GSI 9 (level, active-low)
    buf.push(MADT_TYPE_INT_OVERRIDE); // type
    buf.push(10); // length
    buf.push(0); // bus (ISA)
    buf.push(SCI_INT as u8); // source IRQ
    buf.extend_from_slice(&(SCI_INT as u32).to_le_bytes()); // GSI
    let inti_flags = MADT_POLARITY_ACTIVE_LOW | MADT_TRIGGER_LEVEL;
    buf.extend_from_slice(&inti_flags.to_le_bytes());

    // Local APIC NMI: LINT1 for all CPUs
    buf.push(MADT_TYPE_LOCAL_APIC_NMI); // type
    buf.push(6); // length
    buf.push(0xFF); // processor ID (all)
    let nmi_flags = MADT_POLARITY_ACTIVE_HIGH | MADT_TRIGGER_EDGE;
    buf.extend_from_slice(&nmi_flags.to_le_bytes()); // flags
    buf.push(1); // LINT# (LINT1)

    assert_eq!(buf.len(), total);
    fix_checksum(&mut buf, 0, total);

    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acpi::test_support::verify_checksum;

    /// First byte of the first Local APIC entry.
    const LAPIC_START: usize = ACPI_HDR_SIZE + 8;

    fn lapic_flags(madt: &[u8], id: usize) -> u32 {
        let base = LAPIC_START + id * 8 + 4;
        u32::from_le_bytes(madt[base..base + 4].try_into().unwrap())
    }

    #[test]
    fn madt_checksum() {
        let madt = build_madt(&AcpiConfig::boot_only(4));
        assert_eq!(&madt[0..4], b"APIC");
        assert!(verify_checksum(&madt));
    }

    #[test]
    fn madt_cpu_entries() {
        let madt = build_madt(&AcpiConfig::boot_only(4));
        for i in 0..4u8 {
            let base = LAPIC_START + i as usize * 8;
            assert_eq!(madt[base], MADT_TYPE_LOCAL_APIC);
            assert_eq!(madt[base + 1], 8); // length
            assert_eq!(madt[base + 2], i); // processor ID
            assert_eq!(madt[base + 3], i); // APIC ID
        }
    }

    #[test]
    fn madt_ioapic_entry() {
        let madt = build_madt(&AcpiConfig::boot_only(1));
        // After header + MADT-specific (44) + 1 LAPIC (8) = 52
        let ioapic_offset = 52;
        assert_eq!(madt[ioapic_offset], MADT_TYPE_IO_APIC);
        assert_eq!(madt[ioapic_offset + 1], 12);
        let addr = u32::from_le_bytes([
            madt[ioapic_offset + 4],
            madt[ioapic_offset + 5],
            madt[ioapic_offset + 6],
            madt[ioapic_offset + 7],
        ]);
        assert_eq!(addr, IOAPIC_ADDR);
    }

    /// With max_cpus == num_cpus the MADT must match the boot-only table
    /// byte for byte.
    #[test]
    fn madt_without_hotplug_slots_is_byte_for_byte_unchanged() {
        let explicit = build_madt(&AcpiConfig::new(4, 4).expect("4 of 4"));
        assert_eq!(build_madt(&AcpiConfig::boot_only(4)), explicit);
        // Layout: 36 header + 8 MADT + 4*8 LAPIC + 12 IOAPIC
        // + 2*10 overrides + 6 NMI.
        assert_eq!(explicit.len(), 114);
        assert_eq!(explicit[9], 0x53, "checksum byte moved");
        assert!(verify_checksum(&explicit));
        for id in 0..4 {
            assert_eq!(lapic_flags(&explicit, id), MADT_LAPIC_ENABLED);
        }
    }

    #[test]
    fn madt_hotplug_slots_are_present_but_offline() {
        let madt = build_madt(&AcpiConfig::new(2, 8).expect("2 of 8"));
        // 8 slots, so 4 entries more than the golden above.
        assert_eq!(madt.len(), 114 + 4 * 8);
        for id in 0..8usize {
            let base = LAPIC_START + id * 8;
            assert_eq!(madt[base], MADT_TYPE_LOCAL_APIC);
            assert_eq!(madt[base + 1], 8);
            assert_eq!(madt[base + 2], id as u8);
            assert_eq!(madt[base + 3], id as u8);
            let flags = lapic_flags(&madt, id);
            if id < 2 {
                assert_eq!(flags, MADT_LAPIC_ENABLED, "boot CPU {id}");
            } else {
                // Enabled=0 with Online Capable=1: the guest reserves the
                // slot and does not try to start it.
                assert_eq!(flags, MADT_LAPIC_ONLINE_CAPABLE, "slot {id}");
                assert_eq!(flags & MADT_LAPIC_ENABLED, 0, "slot {id}");
            }
        }
        assert!(verify_checksum(&madt));
    }
}
