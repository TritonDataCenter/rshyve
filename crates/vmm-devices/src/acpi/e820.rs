// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! E820 memory map, shared by fw_cfg and direct boot.

use std::mem::size_of;

/// E820 memory type constants.
pub const E820_TYPE_RAM: u32 = 1;
pub const E820_TYPE_RESERVED: u32 = 2;
#[allow(dead_code)]
pub const E820_TYPE_ACPI: u32 = 3;
#[allow(dead_code)]
pub const E820_TYPE_NVS: u32 = 4;

/// An E820 memory map entry (20 bytes, packed).
///
/// Matches the format expected by QEMU/OVMF via fw_cfg "etc/e820".
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct E820Entry {
    pub addr: u64,
    pub size: u64,
    pub type_: u32,
}

/// VGA memory hole start.
const E820_VGA_BASE: u64 = 0xA_0000;
/// ROM area end (1 MiB boundary).
const E820_ROM_END: u64 = 0x10_0000;
/// 4 GiB boundary where high memory starts.
const FOUR_GIB: u64 = vmm_core::mem::MMIO_HOLE_END;
/// Default lowmem limit (below MMIO hole).
const LOWMEM_LIMIT: u64 = vmm_core::mem::MMIO_HOLE_BASE;

/// Build structured E820 entries from total VM memory size.
///
/// Returns the entry list for reuse by both fw_cfg and direct boot
/// (Linux boot_params zero page).
pub fn build_e820_entries(mem_size: usize) -> Vec<E820Entry> {
    let mut entries = Vec::with_capacity(4);

    let lowmem = (mem_size as u64).min(LOWMEM_LIMIT);
    let highmem = (mem_size as u64).saturating_sub(LOWMEM_LIMIT);

    // Low RAM below VGA hole
    if lowmem > 0 {
        let low_end = lowmem.min(E820_VGA_BASE);
        if low_end > 0 {
            entries.push(E820Entry {
                addr: 0,
                size: low_end,
                type_: E820_TYPE_RAM,
            });
        }
    }

    // Reserved: VGA + ROM area [0xA0000, 0x100000)
    entries.push(E820Entry {
        addr: E820_VGA_BASE,
        size: E820_ROM_END - E820_VGA_BASE,
        type_: E820_TYPE_RESERVED,
    });

    // Low RAM above ROM area (if any)
    if lowmem > E820_ROM_END {
        entries.push(E820Entry {
            addr: E820_ROM_END,
            size: lowmem - E820_ROM_END,
            type_: E820_TYPE_RAM,
        });
    }

    // High RAM above 4 GiB
    if highmem > 0 {
        entries.push(E820Entry {
            addr: FOUR_GIB,
            size: highmem,
            type_: E820_TYPE_RAM,
        });
    }

    entries
}

/// Build E820 memory map as bytes for fw_cfg "etc/e820".
pub fn build_e820(mem_size: usize) -> Vec<u8> {
    let entries = build_e820_entries(mem_size);
    let mut buf = Vec::with_capacity(entries.len() * size_of::<E820Entry>());
    for entry in &entries {
        buf.extend_from_slice(&entry.addr.to_le_bytes());
        buf.extend_from_slice(&entry.size.to_le_bytes());
        buf.extend_from_slice(&entry.type_.to_le_bytes());
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn e820_small_vm() {
        // 256 MiB VM: all low memory, no high memory
        let mem = 256 * 1024 * 1024;
        let data = build_e820(mem);
        let entry_size = 20;

        // Low RAM, reserved, low RAM above 1 MiB.
        assert_eq!(data.len(), 3 * entry_size);

        // First entry: [0, 0xA0000) RAM
        let addr0 = u64::from_le_bytes(data[0..8].try_into().unwrap());
        let size0 = u64::from_le_bytes(data[8..16].try_into().unwrap());
        let type0 = u32::from_le_bytes(data[16..20].try_into().unwrap());
        assert_eq!(addr0, 0);
        assert_eq!(size0, 0xA_0000);
        assert_eq!(type0, E820_TYPE_RAM);

        // Second entry: [0xA0000, 0x100000) Reserved
        let addr1 = u64::from_le_bytes(data[20..28].try_into().unwrap());
        let type1 = u32::from_le_bytes(data[36..40].try_into().unwrap());
        assert_eq!(addr1, 0xA_0000);
        assert_eq!(type1, E820_TYPE_RESERVED);

        // Third entry: [0x100000, lowmem) RAM
        let addr2 = u64::from_le_bytes(data[40..48].try_into().unwrap());
        let size2 = u64::from_le_bytes(data[48..56].try_into().unwrap());
        let type2 = u32::from_le_bytes(data[56..60].try_into().unwrap());
        assert_eq!(addr2, 0x10_0000);
        assert_eq!(size2, mem as u64 - 0x10_0000);
        assert_eq!(type2, E820_TYPE_RAM);
    }

    #[test]
    fn e820_large_vm() {
        // 4 GiB VM: has high memory above 4 GiB
        let mem = 4 * 1024 * 1024 * 1024usize;
        let data = build_e820(mem);
        let entry_size = 20;

        assert_eq!(data.len(), 4 * entry_size);

        let last_off = 3 * entry_size;
        let addr = u64::from_le_bytes(
            data[last_off..last_off + 8].try_into().unwrap(),
        );
        let size = u64::from_le_bytes(
            data[last_off + 8..last_off + 16].try_into().unwrap(),
        );
        let type_ = u32::from_le_bytes(
            data[last_off + 16..last_off + 20].try_into().unwrap(),
        );
        assert_eq!(addr, FOUR_GIB);
        assert_eq!(size, mem as u64 - LOWMEM_LIMIT);
        assert_eq!(type_, E820_TYPE_RAM);
    }

    #[test]
    fn e820_no_overlaps() {
        let mem = 2 * 1024 * 1024 * 1024usize;
        let data = build_e820(mem);
        let entry_size = 20;
        let count = data.len() / entry_size;

        for i in 1..count {
            let prev_addr = u64::from_le_bytes(
                data[(i - 1) * entry_size..(i - 1) * entry_size + 8]
                    .try_into()
                    .unwrap(),
            );
            let prev_size = u64::from_le_bytes(
                data[(i - 1) * entry_size + 8..(i - 1) * entry_size + 16]
                    .try_into()
                    .unwrap(),
            );
            let cur_addr = u64::from_le_bytes(
                data[i * entry_size..i * entry_size + 8].try_into().unwrap(),
            );
            assert!(
                prev_addr + prev_size <= cur_addr,
                "E820 entries overlap: entry {} ends at {:#x} but entry {} starts at {:#x}",
                i - 1, prev_addr + prev_size, i, cur_addr,
            );
        }
    }
}
