// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PVH direct boot (Xen/Firecracker-style).
//!
//! The kernel is entered in 32-bit protected mode with paging off and
//! `EBX` holding the `hvm_start_info` address. The kernel's own PVH head
//! code builds page tables and enters long mode.
//!
//! # Guest memory layout
//!
//! ```text
//! 0x0000_1000  hvm_start_info (56 bytes)
//! 0x0000_1100  memory map entries
//! 0x0000_1800  module list (one entry: the initrd)
//! 0x0000_6000  GDT (null + 32-bit code + 32-bit data)
//! 0x0002_0000  kernel command line (shared with direct boot)
//! <from ELF>   kernel segments, at each p_paddr
//! high mem     initrd (page-aligned, below the lowmem ceiling)
//! ```

use anyhow::{ensure, Context, Result};

use vmm_core::mem::MemCtx;
use vmm_devices::acpi;

use crate::bsp::{
    self, FlatSegments, CR0_ET, CR0_PE, GDT_DATA, RFLAGS_RESERVED_BIT1,
    SEG_TYPE_CODE_XRA,
};
use crate::direct::{CMDLINE_MAX, GPA_CMDLINE};
use crate::{BootVcpu, InitrdImage, PvhKernel};

// ── Guest physical addresses ────────────────────────────────────────

const PVH_GPA_START_INFO: u64 = 0x1000;
const PVH_GPA_MEMMAP: u64 = 0x1100;
const PVH_GPA_MODLIST: u64 = 0x1800;
const PVH_GPA_GDT: u64 = 0x6000;
/// Shared with direct boot. The two paths are never both active.
const PVH_GPA_CMDLINE: u64 = GPA_CMDLINE;
/// Entries the window between the memmap and the module list holds.
const PVH_MEMMAP_MAX: usize = 64;

// ── hvm_start_info ABI ──────────────────────────────────────────────

/// `XEN_HVM_START_MAGIC_VALUE` from Xen's `arch-x86/hvm/start_info.h`.
pub const HVM_START_MAGIC: u32 = 0x336e_c578;
pub const HVM_START_INFO_SIZE: usize = 56;
pub const HVM_MEMMAP_ENTRY_SIZE: usize = 24;
pub const HVM_MODLIST_ENTRY_SIZE: usize = 32;

const HVM_START_VERSION: u32 = 1;

/// `struct hvm_start_info`, version 1.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct HvmStartInfo {
    pub magic: u32,
    pub version: u32,
    pub flags: u32,
    pub nr_modules: u32,
    pub modlist_paddr: u64,
    pub cmdline_paddr: u64,
    pub rsdp_paddr: u64,
    pub memmap_paddr: u64,
    pub memmap_entries: u32,
    pub reserved: u32,
}

const _: () = assert!(size_of::<HvmStartInfo>() == HVM_START_INFO_SIZE);

impl HvmStartInfo {
    pub fn to_le_bytes(&self) -> [u8; HVM_START_INFO_SIZE] {
        let mut out = [0u8; HVM_START_INFO_SIZE];
        out[0..4].copy_from_slice(&self.magic.to_le_bytes());
        out[4..8].copy_from_slice(&self.version.to_le_bytes());
        out[8..12].copy_from_slice(&self.flags.to_le_bytes());
        out[12..16].copy_from_slice(&self.nr_modules.to_le_bytes());
        out[16..24].copy_from_slice(&self.modlist_paddr.to_le_bytes());
        out[24..32].copy_from_slice(&self.cmdline_paddr.to_le_bytes());
        out[32..40].copy_from_slice(&self.rsdp_paddr.to_le_bytes());
        out[40..48].copy_from_slice(&self.memmap_paddr.to_le_bytes());
        out[48..52].copy_from_slice(&self.memmap_entries.to_le_bytes());
        out[52..56].copy_from_slice(&self.reserved.to_le_bytes());
        out
    }
}

/// `struct hvm_memmap_table_entry`. `type_` carries the E820 type verbatim.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct HvmMemmapTableEntry {
    pub addr: u64,
    pub size: u64,
    pub type_: u32,
    pub reserved: u32,
}

const _: () =
    assert!(size_of::<HvmMemmapTableEntry>() == HVM_MEMMAP_ENTRY_SIZE);

impl HvmMemmapTableEntry {
    pub fn to_le_bytes(&self) -> [u8; HVM_MEMMAP_ENTRY_SIZE] {
        let mut out = [0u8; HVM_MEMMAP_ENTRY_SIZE];
        out[0..8].copy_from_slice(&self.addr.to_le_bytes());
        out[8..16].copy_from_slice(&self.size.to_le_bytes());
        out[16..20].copy_from_slice(&self.type_.to_le_bytes());
        out[20..24].copy_from_slice(&self.reserved.to_le_bytes());
        out
    }
}

/// `struct hvm_modlist_entry`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct HvmModlistEntry {
    pub paddr: u64,
    pub size: u64,
    pub cmdline_paddr: u64,
    pub reserved: u64,
}

const _: () = assert!(size_of::<HvmModlistEntry>() == HVM_MODLIST_ENTRY_SIZE);

impl HvmModlistEntry {
    pub fn to_le_bytes(&self) -> [u8; HVM_MODLIST_ENTRY_SIZE] {
        let mut out = [0u8; HVM_MODLIST_ENTRY_SIZE];
        out[0..8].copy_from_slice(&self.paddr.to_le_bytes());
        out[8..16].copy_from_slice(&self.size.to_le_bytes());
        out[16..24].copy_from_slice(&self.cmdline_paddr.to_le_bytes());
        out[24..32].copy_from_slice(&self.reserved.to_le_bytes());
        out
    }
}

// ── GDT ─────────────────────────────────────────────────────────────

/// 32-bit code: base 0, limit 0xFFFFF, type 0x0B, S=1, P=1, D/B=1, G=1.
const GDT_CODE32: u64 = 0x00CF_9B00_0000_FFFF;
const PVH_GDT_SIZE: usize = 24;
const PVH_GDTR_LIMIT: u32 = (PVH_GDT_SIZE as u32) - 1;

// ── Loading ─────────────────────────────────────────────────────────

/// Load a PVH kernel, its cmdline, optional initrd, GDT and boot info.
///
/// Returns the 32-bit entry point. Must run after the ACPI tables are
/// placed, because `acpi_rsdp_addr` is only meaningful once they are.
pub fn load_pvh_kernel(
    mem: &MemCtx,
    kernel: &PvhKernel,
    cmdline: &str,
    initrd: Option<&InitrdImage>,
    mem_size: usize,
    acpi_rsdp_addr: u64,
) -> Result<u32> {
    // 1. Copy each PT_LOAD segment to its p_paddr. Only p_filesz bytes
    //    are written; the BSS tail relies on guest RAM starting zeroed.
    let image = kernel.bytes();
    for seg in kernel.segments() {
        let end = seg
            .file_offset
            .checked_add(seg.filesz)
            .context("PT_LOAD segment overflows the file")?;
        // Bound the segment against the image before either path runs,
        // so a header claiming more than the file holds is refused here
        // rather than part-way through filling guest memory.
        image
            .get(seg.file_offset..end)
            .context("PT_LOAD segment lies outside the image")?;

        match kernel.source() {
            // Read straight from the file into guest RAM. This skips a
            // second copy through the VMM's address space.
            Some(file) => mem
                .load_from_file(
                    seg.paddr,
                    file,
                    seg.file_offset as u64,
                    seg.filesz,
                )
                .context("failed to load a PT_LOAD segment")?,
            // No file behind the image, so copy from the bytes in hand.
            None => {
                let bytes = &image[seg.file_offset..end];
                mem.write_bulk(seg.paddr, bytes)
                    .context("failed to write a PT_LOAD segment")?;
            }
        }
    }

    // 2. Command line, NUL terminated.
    ensure!(
        cmdline.len() < CMDLINE_MAX,
        "cmdline too long (max {})",
        CMDLINE_MAX
    );
    let mut cmdline_buf = cmdline.as_bytes().to_vec();
    cmdline_buf.push(0);
    mem.write(PVH_GPA_CMDLINE, &cmdline_buf)
        .context("failed to write cmdline")?;

    // 3. Initrd, with the same checked placement direct boot uses.
    let module = if let Some(rd) = initrd {
        let rd_addr =
            crate::place_initrd(mem_size, rd.len(), kernel.load_end())?;
        mem.write_bulk(rd_addr, rd.as_bytes())
            .context("failed to write initrd")?;
        Some(HvmModlistEntry {
            paddr: rd_addr,
            size: rd.len() as u64,
            cmdline_paddr: 0,
            reserved: 0,
        })
    } else {
        None
    };

    // 4. GDT for the PVH trampoline.
    let mut gdt = [0u8; PVH_GDT_SIZE];
    gdt[8..16].copy_from_slice(&GDT_CODE32.to_le_bytes());
    gdt[16..24].copy_from_slice(&GDT_DATA.to_le_bytes());
    mem.write(PVH_GPA_GDT, &gdt)
        .context("failed to write GDT")?;

    // 5. Memory map, built once from the shared E820 table.
    let e820 = acpi::build_e820_entries(mem_size);
    ensure!(
        e820.len() <= PVH_MEMMAP_MAX,
        "E820 map has {} entries; the PVH window holds {PVH_MEMMAP_MAX}",
        e820.len()
    );
    let mut memmap = Vec::with_capacity(e820.len() * HVM_MEMMAP_ENTRY_SIZE);
    for entry in &e820 {
        // E820Entry is packed, so copy each field out before use.
        let (addr, size, type_) = (entry.addr, entry.size, entry.type_);
        memmap.extend_from_slice(
            &HvmMemmapTableEntry {
                addr,
                size,
                type_,
                reserved: 0,
            }
            .to_le_bytes(),
        );
    }
    mem.write(PVH_GPA_MEMMAP, &memmap)
        .context("failed to write the PVH memory map")?;

    if let Some(entry) = module {
        mem.write(PVH_GPA_MODLIST, &entry.to_le_bytes())
            .context("failed to write the PVH module list")?;
    }

    // 6. hvm_start_info last: it points at everything above.
    let start_info = HvmStartInfo {
        magic: HVM_START_MAGIC,
        version: HVM_START_VERSION,
        flags: 0,
        nr_modules: u32::from(module.is_some()),
        modlist_paddr: if module.is_some() { PVH_GPA_MODLIST } else { 0 },
        cmdline_paddr: PVH_GPA_CMDLINE,
        rsdp_paddr: acpi_rsdp_addr,
        memmap_paddr: PVH_GPA_MEMMAP,
        memmap_entries: e820.len() as u32,
        reserved: 0,
    };
    mem.write(PVH_GPA_START_INFO, &start_info.to_le_bytes())
        .context("failed to write hvm_start_info")?;

    Ok(kernel.entry_point_32())
}

// ── BSP setup ───────────────────────────────────────────────────────

/// Configure vCPU 0 for PVH entry: 32-bit protected mode, paging off.
///
/// PVH passes `hvm_start_info` in `EBX`. `ESI` carrying a boot pointer is
/// the bzImage convention and is wrong here. The register state differs
/// from `setup_direct_boot_bsp`, so this is a separate function.
pub fn setup_pvh_bsp<V: BootVcpu + ?Sized>(
    vcpu: &V,
    entry32: u32,
) -> Result<()> {
    use bhyve_api::{
        vm_reg_name::*, SEG_ACCESS_DB, SEG_ACCESS_G, SEG_ACCESS_P, SEG_ACCESS_S,
    };

    vcpu.reboot_state().context("reboot_state")?;

    vcpu.set_reg(VM_REG_GUEST_CR0, CR0_PE | CR0_ET)?;
    vcpu.set_reg(VM_REG_GUEST_CR3, 0)?;
    vcpu.set_reg(VM_REG_GUEST_CR4, 0)?;
    vcpu.set_reg(VM_REG_GUEST_EFER, 0)?;

    bsp::program_flat_segments(
        vcpu,
        &FlatSegments {
            // D/B, not L: the PVH entry is 32-bit protected mode.
            code_access: SEG_ACCESS_P
                | SEG_ACCESS_S
                | SEG_ACCESS_DB
                | SEG_ACCESS_G
                | SEG_TYPE_CODE_XRA,
            gdt_base: PVH_GPA_GDT,
            gdt_limit: PVH_GDTR_LIMIT,
        },
    )?;

    vcpu.set_reg(VM_REG_GUEST_RIP, u64::from(entry32))?;
    vcpu.set_reg(VM_REG_GUEST_RFLAGS, RFLAGS_RESERVED_BIT1)?;
    // The PVH ABI uses EBX, not ESI.
    vcpu.set_reg(VM_REG_GUEST_RBX, PVH_GPA_START_INFO)?;
    bsp::zero_gprs(vcpu, VM_REG_GUEST_RBX)?;

    vcpu.set_run_state(bhyve_api::VRS_RUN)?;
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use vmm_core::mem::PhysMap;

    const MIB: usize = 1024 * 1024;
    const PHOFF: usize = 64;
    const PHDR_LEN: usize = 56;
    const NOTE_OFF: usize = 0x100;
    const LOAD_OFF: usize = 0x200;
    const LOAD_PADDR: u64 = 0x10_0000;
    const ENTRY32: u32 = 0x0010_0100;
    const LOAD_BYTES: [u8; 8] = [0xAA, 0xBB, 0xCC, 0xDD, 1, 2, 3, 4];

    fn write_phdr(
        img: &mut [u8],
        idx: usize,
        ty: u32,
        off: u64,
        pa: u64,
        sz: u64,
    ) {
        let at = PHOFF + idx * PHDR_LEN;
        img[at..at + 4].copy_from_slice(&ty.to_le_bytes());
        img[at + 8..at + 16].copy_from_slice(&off.to_le_bytes());
        img[at + 24..at + 32].copy_from_slice(&pa.to_le_bytes());
        img[at + 32..at + 40].copy_from_slice(&sz.to_le_bytes());
        img[at + 40..at + 48].copy_from_slice(&sz.to_le_bytes());
    }

    /// Synthetic PVH ELF64: one PT_NOTE with the Xen entry, one PT_LOAD.
    fn build_pvh_elf() -> Vec<u8> {
        let mut notes = Vec::new();
        notes.extend_from_slice(&4u32.to_le_bytes()); // n_namesz
        notes.extend_from_slice(&4u32.to_le_bytes()); // n_descsz
        notes.extend_from_slice(&18u32.to_le_bytes()); // n_type
        notes.extend_from_slice(b"Xen\0");
        notes.extend_from_slice(&ENTRY32.to_le_bytes());

        let mut img = vec![0u8; 0x400];
        img[0..4].copy_from_slice(b"\x7fELF");
        img[4] = 2; // ELFCLASS64
        img[5] = 1; // ELFDATA2LSB
        img[0x12..0x14].copy_from_slice(&0x3Eu16.to_le_bytes());
        img[0x20..0x28].copy_from_slice(&(PHOFF as u64).to_le_bytes());
        img[0x36..0x38].copy_from_slice(&(PHDR_LEN as u16).to_le_bytes());
        img[0x38..0x3A].copy_from_slice(&2u16.to_le_bytes());

        write_phdr(&mut img, 0, 4, NOTE_OFF as u64, 0, notes.len() as u64);
        write_phdr(
            &mut img,
            1,
            1,
            LOAD_OFF as u64,
            LOAD_PADDR,
            LOAD_BYTES.len() as u64,
        );

        img[NOTE_OFF..NOTE_OFF + notes.len()].copy_from_slice(&notes);
        img[LOAD_OFF..LOAD_OFF + LOAD_BYTES.len()].copy_from_slice(&LOAD_BYTES);
        img
    }

    #[test]
    fn abi_struct_sizes_match_the_xen_layout() {
        assert_eq!(size_of::<HvmStartInfo>(), HVM_START_INFO_SIZE);
        assert_eq!(size_of::<HvmMemmapTableEntry>(), HVM_MEMMAP_ENTRY_SIZE);
        assert_eq!(size_of::<HvmModlistEntry>(), HVM_MODLIST_ENTRY_SIZE);
        // Pin XEN_HVM_START_MAGIC_VALUE. The guest rejects any other value.
        assert_eq!(HVM_START_MAGIC, 0x336e_c578);
    }

    #[test]
    fn pvh_layout_no_overlap() {
        assert!(
            PVH_GPA_START_INFO + HVM_START_INFO_SIZE as u64 <= PVH_GPA_MEMMAP
        );
        assert!(
            PVH_GPA_MEMMAP + (PVH_MEMMAP_MAX * HVM_MEMMAP_ENTRY_SIZE) as u64
                <= PVH_GPA_MODLIST
        );
        assert!(PVH_GPA_MODLIST + HVM_MODLIST_ENTRY_SIZE as u64 <= PVH_GPA_GDT);
        assert!(PVH_GPA_GDT + PVH_GDT_SIZE as u64 <= PVH_GPA_CMDLINE);
    }

    #[test]
    fn gdt_is_32_bit_protected_mode() {
        // D/B set and L clear is what makes this 32-bit, not 64-bit.
        assert_ne!(GDT_CODE32 & (1 << 54), 0, "D/B must be set");
        assert_eq!(GDT_CODE32 & (1 << 53), 0, "L must be clear");
        assert_ne!(GDT_DATA & (1 << 54), 0, "D/B must be set");
    }

    /// Every register write `setup_pvh_bsp` makes, in order.
    ///
    /// The values are literals on purpose: an expression shared with
    /// the code under test would move with it and prove nothing. PVH
    /// enters in 32-bit protected mode with paging off and the boot
    /// pointer in `EBX`, so any drift here is a silent triple fault.
    #[test]
    fn pvh_bsp_writes_a_fixed_register_sequence() {
        use bhyve_api::vm_reg_name::*;

        use crate::recording::{
            assert_sequence, desc, reg, RecordingVcpu, Write,
        };

        // P|S|DB|G|0x0B, P|S|DB|G|0x03, P|0x0B, and UNUSABLE.
        const CODE32_ACCESS: u32 = 0xC09B;
        const DATA32_ACCESS: u32 = 0xC093;
        const TSS_BUSY_ACCESS: u32 = 0x8B;
        const LDTR_ACCESS: u32 = 0x1_0000;

        let cpu = RecordingVcpu::default();
        setup_pvh_bsp(&cpu, ENTRY32).expect("the recorder never fails");

        assert_sequence(
            &cpu.writes(),
            &[
                Write::Reboot,
                reg(VM_REG_GUEST_CR0, 0x11), // ET|PE, paging off
                reg(VM_REG_GUEST_CR3, 0),
                reg(VM_REG_GUEST_CR4, 0),
                reg(VM_REG_GUEST_EFER, 0),
                desc(VM_REG_GUEST_CS, 0, u32::MAX, CODE32_ACCESS),
                reg(VM_REG_GUEST_CS, 0x08),
                desc(VM_REG_GUEST_DS, 0, u32::MAX, DATA32_ACCESS),
                reg(VM_REG_GUEST_DS, 0x10),
                desc(VM_REG_GUEST_ES, 0, u32::MAX, DATA32_ACCESS),
                reg(VM_REG_GUEST_ES, 0x10),
                desc(VM_REG_GUEST_FS, 0, u32::MAX, DATA32_ACCESS),
                reg(VM_REG_GUEST_FS, 0x10),
                desc(VM_REG_GUEST_GS, 0, u32::MAX, DATA32_ACCESS),
                reg(VM_REG_GUEST_GS, 0x10),
                desc(VM_REG_GUEST_SS, 0, u32::MAX, DATA32_ACCESS),
                reg(VM_REG_GUEST_SS, 0x10),
                desc(VM_REG_GUEST_GDTR, 0x6000, 23, 0),
                desc(VM_REG_GUEST_TR, 0, 0xFFFF, TSS_BUSY_ACCESS),
                reg(VM_REG_GUEST_TR, 0),
                desc(VM_REG_GUEST_LDTR, 0, 0, LDTR_ACCESS),
                reg(VM_REG_GUEST_LDTR, 0),
                desc(VM_REG_GUEST_IDTR, 0, 0xFFFF, 0),
                reg(VM_REG_GUEST_RIP, ENTRY32 as u64),
                reg(VM_REG_GUEST_RFLAGS, 0x02),
                reg(VM_REG_GUEST_RBX, 0x1000), // hvm_start_info, not RSI
                reg(VM_REG_GUEST_RAX, 0),
                reg(VM_REG_GUEST_RCX, 0),
                reg(VM_REG_GUEST_RDX, 0),
                reg(VM_REG_GUEST_RSI, 0),
                reg(VM_REG_GUEST_RDI, 0),
                reg(VM_REG_GUEST_RBP, 0),
                reg(VM_REG_GUEST_RSP, 0),
                reg(VM_REG_GUEST_R8, 0),
                reg(VM_REG_GUEST_R9, 0),
                reg(VM_REG_GUEST_R10, 0),
                reg(VM_REG_GUEST_R11, 0),
                reg(VM_REG_GUEST_R12, 0),
                reg(VM_REG_GUEST_R13, 0),
                reg(VM_REG_GUEST_R14, 0),
                reg(VM_REG_GUEST_R15, 0),
                Write::RunState(2), // VRS_RUN
            ],
        );
    }

    #[test]
    fn load_writes_segments_and_start_info() {
        let map = PhysMap::new_anon(0, 32 * MIB).expect("anon guest RAM");
        let mem = MemCtx::new(Arc::new(map));
        let kernel = PvhKernel::from_bytes(build_pvh_elf()).expect("parses");

        let entry = load_pvh_kernel(
            &mem,
            &kernel,
            "console=ttyS0",
            None,
            32 * MIB,
            0xF2400,
        )
        .expect("PVH load");
        assert_eq!(entry, ENTRY32);

        let mut seg = [0u8; 8];
        mem.read(LOAD_PADDR, &mut seg).expect("read segment");
        assert_eq!(seg, LOAD_BYTES);

        let mut si = [0u8; HVM_START_INFO_SIZE];
        mem.read(PVH_GPA_START_INFO, &mut si)
            .expect("read start_info");
        assert_eq!(
            u32::from_le_bytes([si[0], si[1], si[2], si[3]]),
            HVM_START_MAGIC
        );
        assert_eq!(u32::from_le_bytes([si[4], si[5], si[6], si[7]]), 1);
        assert_eq!(u32::from_le_bytes([si[12], si[13], si[14], si[15]]), 0);
        assert_eq!(
            u64::from_le_bytes(si[24..32].try_into().expect("8 bytes")),
            PVH_GPA_CMDLINE
        );
        assert_eq!(
            u64::from_le_bytes(si[32..40].try_into().expect("8 bytes")),
            0xF2400
        );
        assert_eq!(
            u64::from_le_bytes(si[40..48].try_into().expect("8 bytes")),
            PVH_GPA_MEMMAP
        );
        let entries =
            u32::from_le_bytes([si[48], si[49], si[50], si[51]]) as usize;
        assert_eq!(entries, acpi::build_e820_entries(32 * MIB).len());
    }

    #[test]
    fn load_places_an_initrd_and_records_the_module() {
        let map = PhysMap::new_anon(0, 32 * MIB).expect("anon guest RAM");
        let mem = MemCtx::new(Arc::new(map));
        let kernel = PvhKernel::from_bytes(build_pvh_elf()).expect("parses");
        let rd = InitrdImage::from_bytes(vec![0x5Au8; 4096]).expect("initrd");

        load_pvh_kernel(&mem, &kernel, "quiet", Some(&rd), 32 * MIB, 0xF2400)
            .expect("PVH load");

        let mut si = [0u8; HVM_START_INFO_SIZE];
        mem.read(PVH_GPA_START_INFO, &mut si)
            .expect("read start_info");
        assert_eq!(u32::from_le_bytes([si[12], si[13], si[14], si[15]]), 1);
        assert_eq!(
            u64::from_le_bytes(si[16..24].try_into().expect("8 bytes")),
            PVH_GPA_MODLIST
        );

        let mut modlist = [0u8; HVM_MODLIST_ENTRY_SIZE];
        mem.read(PVH_GPA_MODLIST, &mut modlist)
            .expect("read modlist");
        let rd_addr = u64::from_le_bytes(modlist[0..8].try_into().expect("8"));
        assert_eq!(rd_addr & 0xFFF, 0, "the initrd must be page-aligned");
        assert_eq!(
            u64::from_le_bytes(modlist[8..16].try_into().expect("8")),
            4096
        );

        let mut head = [0u8; 4];
        mem.read(rd_addr, &mut head).expect("read initrd");
        assert_eq!(head, [0x5A; 4]);
    }

    #[test]
    fn load_rejects_an_initrd_that_would_overlap_the_kernel() {
        let map = PhysMap::new_anon(0, 32 * MIB).expect("anon guest RAM");
        let mem = MemCtx::new(Arc::new(map));
        let kernel = PvhKernel::from_bytes(build_pvh_elf()).expect("parses");
        let rd = InitrdImage::from_bytes(vec![0u8; 64 * MIB]).expect("initrd");

        let err = load_pvh_kernel(
            &mem,
            &kernel,
            "console=ttyS0",
            Some(&rd),
            32 * MIB,
            0xF2400,
        )
        .expect_err("a 64 MiB initrd cannot fit under a 32 MiB ceiling");
        assert!(
            err.to_string().contains("initrd larger than low memory"),
            "got: {err}"
        );
    }

    #[test]
    fn load_rejects_a_cmdline_that_does_not_fit() {
        let map = PhysMap::new_anon(0, 32 * MIB).expect("anon guest RAM");
        let mem = MemCtx::new(Arc::new(map));
        let kernel = PvhKernel::from_bytes(build_pvh_elf()).expect("parses");

        let err = load_pvh_kernel(
            &mem,
            &kernel,
            &"a".repeat(CMDLINE_MAX),
            None,
            32 * MIB,
            0xF2400,
        )
        .expect_err("an oversized cmdline must be rejected");
        assert!(err.to_string().contains("cmdline too long"), "got: {err}");
    }
}
