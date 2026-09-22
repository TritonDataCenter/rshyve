// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Direct Linux kernel boot (Firecracker-style).
//!
//! Loads a Linux bzImage directly into guest RAM, sets up boot_params
//! (zero page), identity-mapped page tables, GDT, and configures the
//! BSP vCPU for 64-bit long mode entry. No UEFI firmware runs.
//!
//! # Guest memory layout
//!
//! ```text
//! 0x0000_0500  GDT (24 bytes: null + code64 + data64)
//! 0x0000_7000  boot_params / zero page (4 KiB)
//! 0x0000_9000  PML4 (4 KiB)
//! 0x0000_A000  PDPT (4 KiB)
//! 0x0000_B000  PD0..PD3 (16 KiB, covers 0-4GB)
//! 0x0002_0000  kernel command line (up to 4 KiB)
//! pref_address kernel protected-mode code (typically 0x0100_0000)
//! high mem     initrd (page-aligned, below lowmem ceiling)
//! ```

use std::fmt;
use std::path::Path;

use anyhow::{ensure, Context, Result};

use crate::bsp::{
    self, FlatSegments, CR0_ET, CR0_PE, GDT_DATA, RFLAGS_RESERVED_BIT1,
    SEG_TYPE_CODE_XRA,
};
use crate::{BootVcpu, ImageBytes};
use vmm_core::mem::MemCtx;
use vmm_devices::acpi;

// ── Guest physical addresses ────────────────────────────────────────

const GPA_GDT: u64 = 0x500;
/// `load_kernel` writes the zero page here and `setup_direct_boot_bsp`
/// puts the address in `RSI`. A BSP setup outside this crate has to
/// name the same address, so it is public.
pub const GPA_BOOT_PARAMS: u64 = 0x7000;
const GPA_PML4: u64 = 0x9000;
const GPA_PDPT: u64 = 0xA000;
const GPA_PD_BASE: u64 = 0xB000;
/// Where `load_kernel` writes the kernel command line. Public so a
/// caller can read back, or rewrite, the string it passed.
pub const GPA_CMDLINE: u64 = 0x2_0000;
/// Longest command line `load_kernel` accepts, NUL included. Public so
/// a caller can reject an over-long string before it loads anything.
pub const CMDLINE_MAX: usize = 4096;

/// The zero page is one 4 KiB page.
const BOOT_PARAMS_BYTES: u64 = 4096;

/// PML4, PDPT and one PD per GB of the identity map.
const PAGE_TABLE_BYTES: u64 = 6 * 4096;

/// The low guest memory `load_kernel` claims, as `(base, len)` pairs in
/// ascending order.
///
/// A boot path outside this crate puts its own tables in low memory and
/// must avoid these ranges. Only the footprint is a promise: the GDT
/// and page table addresses inside it stay private, so the layout can
/// still change.
pub const DIRECT_BOOT_RESERVED: [(u64, u64); 4] = [
    (GPA_GDT, GDT_SIZE as u64),
    (GPA_BOOT_PARAMS, BOOT_PARAMS_BYTES),
    (GPA_PML4, PAGE_TABLE_BYTES),
    (GPA_CMDLINE, CMDLINE_MAX as u64),
];

// ── bzImage setup_header offsets ────────────────────────────────────

const SETUP_HEADER_OFFSET: usize = 0x1F1;
const HEADER_MAGIC_OFFSET: usize = 0x202;
const HEADER_MAGIC: u32 = 0x5372_6448; // "HdrS"
const VERSION_OFFSET: usize = 0x206;
const PREF_ADDRESS_OFFSET: usize = 0x258;
const XLOADFLAGS_OFFSET: usize = 0x236;
const XLF_KERNEL_64: u16 = 1;
/// Dword at 0x238: the longest command line this kernel will read.
/// Valid from protocol 2.06; x86 kernels advertise
/// `COMMAND_LINE_SIZE - 1`, which is 2047 today.
const CMDLINE_SIZE_OFFSET: usize = 0x238;
/// The limit before 2.06 added the field, for an image that leaves it 0.
const CMDLINE_SIZE_LEGACY: u32 = 255;
const SETUP_HEADER_END: usize = 0x290;

// ── boot_params (zero page) offsets ────────────────────────────────

/// Byte at 0x1E8: number of E820 entries.
const BP_E820_ENTRIES: usize = 0x1E8;
/// Dword at 0x210: type_of_loader.
const BP_TYPE_OF_LOADER: usize = 0x210;
/// Dword at 0x218: ramdisk_image (initrd GPA).
const BP_RAMDISK_IMAGE: usize = 0x218;
/// Dword at 0x21C: ramdisk_size.
const BP_RAMDISK_SIZE: usize = 0x21C;
/// Dword at 0x228: cmd_line_ptr.
const BP_CMD_LINE_PTR: usize = 0x228;
/// Qword at 0x070: acpi_rsdp_addr (protocol >= 2.14).
const BP_ACPI_RSDP_ADDR: usize = 0x070;
/// E820 map starts at offset 0x2D0 in boot_params.
const BP_E820_MAP: usize = 0x2D0;
/// Each E820 entry in boot_params is 20 bytes.
const BP_E820_ENTRY_SIZE: usize = 20;
/// Max E820 entries the kernel accepts.
const BP_E820_MAX: usize = 128;

// ── Page table bits ────────────────────────────────────────────────

const PTE_PRESENT: u64 = 1 << 0;
const PTE_WRITABLE: u64 = 1 << 1;
const PTE_PAGE_SIZE: u64 = 1 << 7; // 2MB large page

// ── x86 control register bits ──────────────────────────────────────

const CR0_NE: u64 = 1 << 5; // Numeric Error (native FPU error reporting)
const CR0_PG: u64 = 1 << 31; // Paging

const CR4_PAE: u64 = 1 << 5; // Physical Address Extension

const EFER_LME: u64 = 1 << 8; // Long Mode Enable
const EFER_LMA: u64 = 1 << 10; // Long Mode Active

// ── GDT entries ────────────────────────────────────────────────────
//
// 64-bit GDT entry format (8 bytes):
//   bits 0-15:  limit [15:0]
//   bits 16-39: base [23:0]
//   bits 40-47: access byte (type, S, DPL, P)
//   bits 48-51: limit [19:16]
//   bits 52-55: flags (AVL, L, DB, G)
//   bits 56-63: base [31:24]

/// 64-bit code segment: Base=0, Limit=0xFFFFF, Type=0x0B (exec/read/accessed),
/// S=1, DPL=0, P=1, L=1, G=1
const GDT_CODE64: u64 = 0x00AF_9B00_0000_FFFF;

/// GDT byte size: 3 entries (null + code + data) * 8 bytes each.
const GDT_SIZE: usize = 24;

/// GDTR limit = GDT_SIZE - 1.
const GDTR_LIMIT: u32 = (GDT_SIZE as u32) - 1;

// ── Parsed bzImage ──────────────────────────────────────────────────

/// A parsed Linux bzImage ready for direct loading.
pub struct KernelImage {
    data: ImageBytes,
    /// Number of 512-byte setup sectors (from setup_header).
    setup_sects: usize,
    /// Protocol version (e.g., 0x020F).
    version: u16,
    /// Preferred load address (typically 0x100_0000).
    pref_address: u64,
    /// Longest command line this kernel reads, from `cmdline_size`.
    cmdline_size: u32,
}

// Debug leaves out the image bytes. A derived one prints a whole
// multi-megabyte kernel into every panic message.
impl fmt::Debug for KernelImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KernelImage")
            .field("bytes", &self.data.len())
            .field("setup_sects", &self.setup_sects)
            .field("version", &format_args!("{:#06x}", self.version))
            .field("pref_address", &format_args!("{:#x}", self.pref_address))
            .field("cmdline_size", &self.cmdline_size)
            .finish()
    }
}

impl KernelImage {
    /// Open and validate a bzImage file.
    pub fn open(path: &Path) -> Result<Self> {
        Self::from_bytes(ImageBytes::map(path)?)
    }

    /// Validate an already-read bzImage.
    pub fn from_bytes(data: impl Into<ImageBytes>) -> Result<Self> {
        let data = data.into();
        ensure!(
            data.len() > SETUP_HEADER_END,
            "kernel too small to be a bzImage"
        );

        let magic = u32::from_le_bytes(
            data[HEADER_MAGIC_OFFSET..HEADER_MAGIC_OFFSET + 4]
                .try_into()
                .unwrap(),
        );
        ensure!(
            magic == HEADER_MAGIC,
            "not a valid bzImage (missing HdrS magic at 0x202)"
        );

        let version = u16::from_le_bytes(
            data[VERSION_OFFSET..VERSION_OFFSET + 2].try_into().unwrap(),
        );
        ensure!(
            version >= 0x020C,
            "boot protocol {:#06x} too old (need >= 0x020C for 64-bit entry)",
            version,
        );

        let xloadflags = u16::from_le_bytes(
            data[XLOADFLAGS_OFFSET..XLOADFLAGS_OFFSET + 2]
                .try_into()
                .unwrap(),
        );
        ensure!(
            xloadflags & XLF_KERNEL_64 != 0,
            "kernel does not support 64-bit entry (xloadflags={:#06x})",
            xloadflags,
        );

        let setup_sects = data[SETUP_HEADER_OFFSET] as usize;
        let setup_sects = if setup_sects == 0 { 4 } else { setup_sects };

        // setup_sects is one byte out of the image and it scales the
        // protected-mode offset. Bound it before anything slices there.
        let kernel_offset = setup_sects
            .checked_add(1)
            .and_then(|sects| sects.checked_mul(512))
            .context("setup_sects overflows the protected-mode offset")?;
        ensure!(
            data.len() > kernel_offset,
            "bzImage truncated: setup_sects={setup_sects} implies a \
             {kernel_offset}-byte header but the file is {} bytes",
            data.len(),
        );

        let pref_address = u64::from_le_bytes(
            data[PREF_ADDRESS_OFFSET..PREF_ADDRESS_OFFSET + 8]
                .try_into()
                .unwrap(),
        );

        let cmdline_size = u32::from_le_bytes(
            data[CMDLINE_SIZE_OFFSET..CMDLINE_SIZE_OFFSET + 4]
                .try_into()
                .unwrap(),
        );
        let cmdline_size = if cmdline_size == 0 {
            CMDLINE_SIZE_LEGACY
        } else {
            cmdline_size
        };

        Ok(Self {
            data,
            setup_sects,
            version,
            pref_address,
            cmdline_size,
        })
    }

    /// Longest command line this kernel will read, in bytes.
    ///
    /// The kernel truncates at its own `COMMAND_LINE_SIZE` without a
    /// word, so a longer line loses its tail (an `init=` at the end,
    /// for example). `CMDLINE_MAX` is the guest-memory ceiling and
    /// applies as well.
    pub fn cmdline_limit(&self) -> usize {
        (self.cmdline_size as usize).min(CMDLINE_MAX - 1)
    }

    /// 64-bit entry point address.
    pub fn entry_point_64(&self) -> u64 {
        self.pref_address + 0x200
    }

    /// Offset of protected-mode kernel in the file.
    ///
    /// `from_bytes` proved this offset is inside the image, so the
    /// slice in `kernel_bytes` cannot go out of bounds.
    fn kernel_offset(&self) -> usize {
        (self.setup_sects + 1) * 512
    }

    /// Protected-mode kernel bytes.
    fn kernel_bytes(&self) -> &[u8] {
        &self.data[self.kernel_offset()..]
    }

    /// Setup header bytes (to embed in boot_params).
    fn setup_header(&self) -> &[u8] {
        &self.data[SETUP_HEADER_OFFSET..SETUP_HEADER_END]
    }
}

/// A loaded initrd image.
pub struct InitrdImage {
    data: ImageBytes,
}

impl InitrdImage {
    pub fn open(path: &Path) -> Result<Self> {
        Self::from_bytes(ImageBytes::map(path)?)
    }

    pub fn from_bytes(data: impl Into<ImageBytes>) -> Result<Self> {
        let data = data.into();
        ensure!(!data.is_empty(), "initrd is empty");
        Ok(Self { data })
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Raw image bytes, for a loader in another module.
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }
}

// ── Loading ─────────────────────────────────────────────────────────

/// Load kernel, initrd, boot_params, page tables, and GDT into guest memory.
///
/// Returns the kernel entry point address for BSP setup.
pub fn load_kernel(
    mem: &MemCtx,
    kernel: &KernelImage,
    cmdline: &str,
    initrd: Option<&InitrdImage>,
    mem_size: usize,
    acpi_rsdp_addr: u64,
) -> Result<u64> {
    // The 64-bit entry point is pref_address + 0x200, so the
    // protected-mode code must sit at pref_address.
    let kernel_bytes = kernel.kernel_bytes();
    let kernel_gpa = kernel.pref_address;
    // write_bulk, not write: the image runs to megabytes and the vCPU
    // threads do not exist yet, so no one can see a torn copy.
    mem.write_bulk(kernel_gpa, kernel_bytes)
        .context("failed to write kernel to guest memory")?;

    let cmdline_limit = kernel.cmdline_limit();
    ensure!(
        cmdline.len() <= cmdline_limit,
        "cmdline is {} bytes and this kernel reads at most {cmdline_limit}",
        cmdline.len(),
    );
    let mut cmdline_buf = cmdline.as_bytes().to_vec();
    cmdline_buf.push(0); // NUL terminate
    mem.write(GPA_CMDLINE, &cmdline_buf)
        .context("failed to write cmdline")?;

    let initrd_addr = if let Some(rd) = initrd {
        let load_end = kernel_gpa
            .checked_add(kernel_bytes.len() as u64)
            .context("kernel load range overflows the address space")?;
        let rd_addr =
            crate::initrd::place_initrd(mem_size, rd.data.len(), load_end)?;
        mem.write_bulk(rd_addr, &rd.data)
            .context("failed to write initrd")?;
        Some((rd_addr, rd.data.len()))
    } else {
        None
    };

    write_page_tables(mem)?;

    write_gdt(mem)?;

    write_boot_params(mem, kernel, initrd_addr, mem_size, acpi_rsdp_addr)?;

    Ok(kernel.entry_point_64())
}

/// Write identity-mapped page tables covering 0-4 GB.
///
/// Uses 2 MB large pages for minimal page count:
/// - 1 PML4 page at 0x9000
/// - 1 PDPT page at 0xA000
/// - 4 PD pages at 0xB000-0xEFFF (one per GB)
fn write_page_tables(mem: &MemCtx) -> Result<()> {
    // PML4[0] → PDPT
    let mut pml4 = [0u8; 4096];
    let pml4_entry = GPA_PDPT | PTE_PRESENT | PTE_WRITABLE;
    pml4[..8].copy_from_slice(&pml4_entry.to_le_bytes());
    mem.write(GPA_PML4, &pml4).context("PML4")?;

    // PDPT[0..3] → PD0..PD3
    let mut pdpt = [0u8; 4096];
    for i in 0..4u64 {
        let pd_addr = GPA_PD_BASE + i * 4096;
        let entry = pd_addr | PTE_PRESENT | PTE_WRITABLE;
        let off = (i as usize) * 8;
        pdpt[off..off + 8].copy_from_slice(&entry.to_le_bytes());
    }
    mem.write(GPA_PDPT, &pdpt).context("PDPT")?;

    // PD pages: 512 entries each, 2 MB per entry
    for gb in 0..4u64 {
        let mut pd = [0u8; 4096];
        for i in 0..512u64 {
            let phys = gb * (1 << 30) + i * (2 << 20);
            let entry = phys | PTE_PRESENT | PTE_WRITABLE | PTE_PAGE_SIZE;
            let off = (i as usize) * 8;
            pd[off..off + 8].copy_from_slice(&entry.to_le_bytes());
        }
        let gpa = GPA_PD_BASE + gb * 4096;
        mem.write(gpa, &pd).context("PD")?;
    }

    Ok(())
}

/// Write a minimal GDT: null + 64-bit code + 64-bit data.
fn write_gdt(mem: &MemCtx) -> Result<()> {
    let mut gdt = [0u8; GDT_SIZE];

    // Entry 0: null descriptor (all zeros)

    // Entry 1 (selector GDT_SEL_CODE64): 64-bit code segment
    gdt[8..16].copy_from_slice(&GDT_CODE64.to_le_bytes());

    // Entry 2 (selector GDT_SEL_DATA64): 64-bit data segment
    gdt[16..24].copy_from_slice(&GDT_DATA.to_le_bytes());

    mem.write(GPA_GDT, &gdt).context("GDT")?;
    Ok(())
}

/// Assemble and write the boot_params zero page.
fn write_boot_params(
    mem: &MemCtx,
    kernel: &KernelImage,
    initrd: Option<(u64, usize)>,
    mem_size: usize,
    acpi_rsdp_addr: u64,
) -> Result<()> {
    let mut bp = [0u8; BOOT_PARAMS_BYTES as usize];

    // Start from the image's own setup_header to keep the kernel's fields.
    let hdr = kernel.setup_header();
    let hdr_dest = SETUP_HEADER_OFFSET;
    bp[hdr_dest..hdr_dest + hdr.len()].copy_from_slice(hdr);

    // 0xFF: a loader with no assigned ID.
    bp[BP_TYPE_OF_LOADER] = 0xFF;

    bp[BP_CMD_LINE_PTR..BP_CMD_LINE_PTR + 4]
        .copy_from_slice(&(GPA_CMDLINE as u32).to_le_bytes());

    if let Some((addr, size)) = initrd {
        bp[BP_RAMDISK_IMAGE..BP_RAMDISK_IMAGE + 4]
            .copy_from_slice(&(addr as u32).to_le_bytes());
        bp[BP_RAMDISK_SIZE..BP_RAMDISK_SIZE + 4]
            .copy_from_slice(&(size as u32).to_le_bytes());
    }

    // acpi_rsdp_addr needs protocol 2.14 (0x020E) or later.
    if kernel.version >= 0x020E && acpi_rsdp_addr != 0 {
        bp[BP_ACPI_RSDP_ADDR..BP_ACPI_RSDP_ADDR + 8]
            .copy_from_slice(&acpi_rsdp_addr.to_le_bytes());
    }

    let entries = acpi::build_e820_entries(mem_size);
    let count = entries.len().min(BP_E820_MAX);
    bp[BP_E820_ENTRIES] = count as u8;
    for (i, entry) in entries.iter().take(count).enumerate() {
        let off = BP_E820_MAP + i * BP_E820_ENTRY_SIZE;
        bp[off..off + 8].copy_from_slice(&entry.addr.to_le_bytes());
        bp[off + 8..off + 16].copy_from_slice(&entry.size.to_le_bytes());
        bp[off + 16..off + 20].copy_from_slice(&entry.type_.to_le_bytes());
    }

    mem.write(GPA_BOOT_PARAMS, &bp).context("boot_params")?;
    Ok(())
}

// ── BSP setup ───────────────────────────────────────────────────────

/// Configure vCPU 0 for 64-bit long mode entry into the kernel.
pub fn setup_direct_boot_bsp<V: BootVcpu + ?Sized>(
    vcpu: &V,
    entry_point: u64,
) -> Result<()> {
    use bhyve_api::{
        vm_reg_name::*, SEG_ACCESS_G, SEG_ACCESS_L, SEG_ACCESS_P, SEG_ACCESS_S,
    };

    vcpu.reboot_state().context("reboot_state")?;

    // Control registers for 64-bit long mode
    vcpu.set_reg(VM_REG_GUEST_CR0, CR0_PG | CR0_NE | CR0_ET | CR0_PE)?;
    vcpu.set_reg(VM_REG_GUEST_CR3, GPA_PML4)?;
    vcpu.set_reg(VM_REG_GUEST_CR4, CR4_PAE)?;
    vcpu.set_reg(VM_REG_GUEST_EFER, EFER_LMA | EFER_LME)?;

    bsp::program_flat_segments(
        vcpu,
        &FlatSegments {
            // L, not D/B: this is a 64-bit code segment.
            code_access: SEG_ACCESS_P
                | SEG_ACCESS_S
                | SEG_ACCESS_L
                | SEG_ACCESS_G
                | SEG_TYPE_CODE_XRA,
            gdt_base: GPA_GDT,
            gdt_limit: GDTR_LIMIT,
        },
    )?;

    vcpu.set_reg(VM_REG_GUEST_RIP, entry_point)?;
    vcpu.set_reg(VM_REG_GUEST_RFLAGS, RFLAGS_RESERVED_BIT1)?;
    // The Linux 64-bit entry contract fixes RSI only.
    vcpu.set_reg(VM_REG_GUEST_RSI, GPA_BOOT_PARAMS)?;
    bsp::zero_gprs(vcpu, VM_REG_GUEST_RSI)?;

    vcpu.set_run_state(bhyve_api::VRS_RUN)?;

    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use vmm_core::mem::PhysMap;

    use crate::recording::Write;

    /// Where a bzImage is loaded by default. The boot scratch ranges
    /// below must all stop before it.
    const GPA_KERNEL: u64 = 0x10_0000;

    const MIB: usize = 1024 * 1024;

    /// Minimal bzImage carrying `cmdline_size`, loaded at 1 MiB so a
    /// small anonymous guest holds it.
    fn bzimage(cmdline_size: u32) -> Vec<u8> {
        let mut img = vec![0u8; 0x2_0000];
        img[HEADER_MAGIC_OFFSET..HEADER_MAGIC_OFFSET + 4]
            .copy_from_slice(&HEADER_MAGIC.to_le_bytes());
        img[VERSION_OFFSET..VERSION_OFFSET + 2]
            .copy_from_slice(&0x020Fu16.to_le_bytes());
        img[XLOADFLAGS_OFFSET..XLOADFLAGS_OFFSET + 2]
            .copy_from_slice(&XLF_KERNEL_64.to_le_bytes());
        img[SETUP_HEADER_OFFSET] = 4;
        img[PREF_ADDRESS_OFFSET..PREF_ADDRESS_OFFSET + 8]
            .copy_from_slice(&(GPA_KERNEL).to_le_bytes());
        img[CMDLINE_SIZE_OFFSET..CMDLINE_SIZE_OFFSET + 4]
            .copy_from_slice(&cmdline_size.to_le_bytes());
        img
    }

    fn load(kernel: &KernelImage, cmdline: &str) -> Result<u64> {
        let map = PhysMap::new_anon(0, 32 * MIB).expect("anon guest RAM");
        let mem = MemCtx::new(Arc::new(map));
        load_kernel(&mem, kernel, cmdline, None, 32 * MIB, 0xF2400)
    }

    #[test]
    fn the_command_line_bound_is_the_kernels_own() {
        // The kernel truncates at COMMAND_LINE_SIZE with no error, so a
        // longer line loses its tail.
        let kernel = KernelImage::from_bytes(bzimage(2047)).expect("parses");
        assert_eq!(kernel.cmdline_limit(), 2047);

        load(&kernel, &"a".repeat(2047)).expect("at the limit");
        let err = load(&kernel, &"a".repeat(2048))
            .expect_err("past the kernel's own limit");
        assert!(err.to_string().contains("2047"), "{err}");
    }

    #[test]
    fn a_kernel_with_no_cmdline_size_gets_the_pre_2_06_limit() {
        let kernel = KernelImage::from_bytes(bzimage(0)).expect("parses");
        assert_eq!(kernel.cmdline_limit(), 255);
        assert!(load(&kernel, &"a".repeat(256)).is_err());
    }

    #[test]
    fn the_guest_memory_ceiling_still_caps_a_boastful_kernel() {
        // A kernel may advertise more than the page the loader reserves.
        let kernel =
            KernelImage::from_bytes(bzimage(u32::MAX)).expect("parses");
        assert_eq!(kernel.cmdline_limit(), CMDLINE_MAX - 1);
    }

    #[test]
    fn page_table_structure() {
        let mut pml4 = [0u8; 4096];
        let entry = GPA_PDPT | PTE_PRESENT | PTE_WRITABLE;
        pml4[..8].copy_from_slice(&entry.to_le_bytes());
        let read_back = u64::from_le_bytes(pml4[..8].try_into().unwrap());
        assert_eq!(read_back & !0xFFF, GPA_PDPT);
        assert_ne!(read_back & PTE_PRESENT, 0);
        assert_ne!(read_back & PTE_WRITABLE, 0);
    }

    #[test]
    fn gdt_entries() {
        let mut gdt = [0u8; GDT_SIZE];
        gdt[8..16].copy_from_slice(&GDT_CODE64.to_le_bytes());
        gdt[16..24].copy_from_slice(&GDT_DATA.to_le_bytes());

        // Null descriptor
        assert_eq!(&gdt[..8], &[0u8; 8]);
        // Code64: check L bit (bit 53) is set, DB bit (bit 54) is clear
        assert_ne!(
            GDT_CODE64 & (1 << 53),
            0,
            "L bit must be set for 64-bit code"
        );
        assert_eq!(
            GDT_CODE64 & (1 << 54),
            0,
            "DB bit must be clear for 64-bit code"
        );
        // Data64: check DB bit is set
        assert_ne!(
            GDT_DATA & (1 << 54),
            0,
            "DB bit must be set for data segment"
        );
    }

    #[test]
    fn boot_params_e820_layout() {
        assert_eq!(BP_E820_MAP, 0x2D0);
        assert_eq!(BP_E820_ENTRY_SIZE, 20);
        // The full map must fit in the 4 KiB zero page.
        assert!(BP_E820_MAP + BP_E820_MAX * BP_E820_ENTRY_SIZE <= 4096);
    }

    #[test]
    fn cmdline_max_fits_in_page() {
        assert!(CMDLINE_MAX <= 4096);
    }

    #[test]
    fn kernel_image_rejects_a_truncated_bzimage() {
        let mut img = vec![0u8; 0x300];
        img[HEADER_MAGIC_OFFSET..HEADER_MAGIC_OFFSET + 4]
            .copy_from_slice(&HEADER_MAGIC.to_le_bytes());
        img[VERSION_OFFSET..VERSION_OFFSET + 2]
            .copy_from_slice(&0x020Fu16.to_le_bytes());
        img[XLOADFLAGS_OFFSET..XLOADFLAGS_OFFSET + 2]
            .copy_from_slice(&XLF_KERNEL_64.to_le_bytes());
        // 255 setup sectors implies a 131072-byte header in a 768-byte file.
        img[SETUP_HEADER_OFFSET] = 255;
        let err = KernelImage::from_bytes(img)
            .expect_err("a truncated bzImage must error, not panic");
        assert!(err.to_string().contains("bzImage truncated"), "got: {err}");
    }

    /// Every register write `setup_direct_boot_bsp` makes, in order.
    ///
    /// The values are literals on purpose. Sharing an expression with
    /// the code under test would let one edit change both sides and
    /// prove nothing. The guest sees this sequence, so a reorder or a
    /// changed bit is a boot regression whatever else still compiles.
    #[test]
    fn direct_boot_bsp_writes_a_fixed_register_sequence() {
        use bhyve_api::vm_reg_name::*;

        use crate::recording::{assert_sequence, desc, reg, RecordingVcpu};

        // P|S|L|G|0x0B, P|S|DB|G|0x03, P|0x0B, and UNUSABLE.
        const CODE64_ACCESS: u32 = 0xA09B;
        const DATA64_ACCESS: u32 = 0xC093;
        const TSS_BUSY64_ACCESS: u32 = 0x8B;
        const LDTR_ACCESS: u32 = 0x1_0000;
        const ENTRY: u64 = 0x0100_0200;

        let cpu = RecordingVcpu::default();
        setup_direct_boot_bsp(&cpu, ENTRY).expect("the recorder never fails");

        assert_sequence(
            &cpu.writes(),
            &[
                Write::Reboot,
                reg(VM_REG_GUEST_CR0, 0x8000_0031), // PG|NE|ET|PE
                reg(VM_REG_GUEST_CR3, 0x9000),
                reg(VM_REG_GUEST_CR4, 0x20),   // PAE
                reg(VM_REG_GUEST_EFER, 0x500), // LMA|LME
                desc(VM_REG_GUEST_CS, 0, u32::MAX, CODE64_ACCESS),
                reg(VM_REG_GUEST_CS, 0x08),
                desc(VM_REG_GUEST_DS, 0, u32::MAX, DATA64_ACCESS),
                reg(VM_REG_GUEST_DS, 0x10),
                desc(VM_REG_GUEST_ES, 0, u32::MAX, DATA64_ACCESS),
                reg(VM_REG_GUEST_ES, 0x10),
                desc(VM_REG_GUEST_FS, 0, u32::MAX, DATA64_ACCESS),
                reg(VM_REG_GUEST_FS, 0x10),
                desc(VM_REG_GUEST_GS, 0, u32::MAX, DATA64_ACCESS),
                reg(VM_REG_GUEST_GS, 0x10),
                desc(VM_REG_GUEST_SS, 0, u32::MAX, DATA64_ACCESS),
                reg(VM_REG_GUEST_SS, 0x10),
                desc(VM_REG_GUEST_GDTR, 0x500, 23, 0),
                desc(VM_REG_GUEST_TR, 0, 0xFFFF, TSS_BUSY64_ACCESS),
                reg(VM_REG_GUEST_TR, 0),
                desc(VM_REG_GUEST_LDTR, 0, 0, LDTR_ACCESS),
                reg(VM_REG_GUEST_LDTR, 0),
                desc(VM_REG_GUEST_IDTR, 0, 0xFFFF, 0),
                reg(VM_REG_GUEST_RIP, ENTRY),
                reg(VM_REG_GUEST_RFLAGS, 0x02),
                reg(VM_REG_GUEST_RSI, 0x7000), // boot_params
                reg(VM_REG_GUEST_RAX, 0),
                reg(VM_REG_GUEST_RBX, 0),
                reg(VM_REG_GUEST_RCX, 0),
                reg(VM_REG_GUEST_RDX, 0),
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

    /// `DIRECT_BOOT_RESERVED` is the promise an outside boot path reads
    /// instead of the private addresses, so it has to cover every one
    /// of them.
    #[test]
    fn reserved_ranges_cover_every_address_the_loader_writes() {
        let last_pd = GPA_PD_BASE + 3 * 4096;
        for addr in [
            GPA_GDT,
            GPA_BOOT_PARAMS,
            GPA_PML4,
            GPA_PDPT,
            last_pd,
            GPA_CMDLINE,
        ] {
            assert!(
                DIRECT_BOOT_RESERVED
                    .iter()
                    .any(|&(base, len)| addr >= base && addr < base + len),
                "{addr:#x} is written but not reserved"
            );
        }

        let mut end = 0;
        for (base, len) in DIRECT_BOOT_RESERVED {
            assert!(base >= end, "{base:#x} overlaps or is out of order");
            end = base + len;
        }
        // The last range must stop below the kernel load address.
        assert!(end <= GPA_KERNEL);
    }

    #[test]
    fn guest_addresses_dont_overlap() {
        assert!(GPA_GDT + GDT_SIZE as u64 <= GPA_BOOT_PARAMS);
        assert!(GPA_BOOT_PARAMS + 4096 <= GPA_PML4);
        // PML4 + PDPT + 4 PDs = 24 KiB, 0x9000..0xF000.
        assert!(GPA_PML4 + 6 * 4096 <= GPA_CMDLINE);
        assert!(GPA_CMDLINE + CMDLINE_MAX as u64 <= GPA_KERNEL);
    }
}
