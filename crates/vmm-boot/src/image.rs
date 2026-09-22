// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Boot-image handle with protocol auto-detection.

//! `--kernel` alone is enough: the protocol comes out of the file, so
//! there is no `--boot` flag for the two binaries to keep in sync, and
//! no way to pair one protocol's entry point with the other's loader.

use std::path::Path;

use anyhow::{bail, Context, Result};

use vmm_core::mem::MemCtx;

use crate::elf::ELF_MAGIC;
use crate::{BootVcpu, ImageBytes, InitrdImage, KernelImage, PvhKernel};

const BZIMAGE_MAGIC_OFFSET: usize = 0x202;
/// "HdrS".
const BZIMAGE_MAGIC: u32 = 0x5372_6448;
/// End of the bzImage setup header. A shorter file cannot hold one.
const BZIMAGE_MIN_LEN: usize = 0x290;

/// Which boot protocol an image uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootProtocol {
    /// Linux bzImage, 64-bit entry.
    Bzimage,
    /// Xen PVH, 32-bit entry.
    Pvh,
}

/// Classify a boot image from its first bytes.
///
/// An ELF with no Xen note is rejected later by `PvhKernel::from_bytes`.
/// There is no fall-through to bzImage: a guessed protocol puts the
/// guest at an entry point that does not match its register state, which
/// shows up as an unexplained triple fault.
pub fn detect_boot_protocol(data: &[u8]) -> Result<BootProtocol> {
    if data.len() >= ELF_MAGIC.len() && &data[..ELF_MAGIC.len()] == ELF_MAGIC {
        return Ok(BootProtocol::Pvh);
    }
    if data.len() > BZIMAGE_MIN_LEN {
        let magic = u32::from_le_bytes(
            data[BZIMAGE_MAGIC_OFFSET..BZIMAGE_MAGIC_OFFSET + 4]
                .try_into()
                .expect("a 4-byte slice is a 4-byte array"),
        );
        if magic == BZIMAGE_MAGIC {
            return Ok(BootProtocol::Bzimage);
        }
    }
    bail!(
        "unrecognized kernel image: no ELF magic at 0 and no bzImage \
         HdrS magic at {BZIMAGE_MAGIC_OFFSET:#x}"
    )
}

/// A kernel image, already classified and parsed.
#[derive(Debug)]
pub enum BootImage {
    /// Linux bzImage, 64-bit long-mode entry.
    Bzimage(KernelImage),
    /// PVH-capable ELF64, 32-bit protected-mode entry.
    Pvh(PvhKernel),
}

impl BootImage {
    /// Read the file once, classify it, and parse it.
    pub fn open(path: &Path) -> Result<Self> {
        let data = ImageBytes::map(path)?;
        let protocol = detect_boot_protocol(&data)
            .with_context(|| format!("kernel: {}", path.display()))?;
        match protocol {
            BootProtocol::Bzimage => {
                Ok(Self::Bzimage(KernelImage::from_bytes(data)?))
            }
            BootProtocol::Pvh => Ok(Self::Pvh(PvhKernel::from_bytes(data)?)),
        }
    }

    /// Which protocol this image boots with.
    pub fn protocol(&self) -> BootProtocol {
        match self {
            Self::Bzimage(_) => BootProtocol::Bzimage,
            Self::Pvh(_) => BootProtocol::Pvh,
        }
    }

    /// Guest physical entry point, widened to 64 bits for logging.
    pub fn entry_point(&self) -> u64 {
        match self {
            Self::Bzimage(k) => k.entry_point_64(),
            Self::Pvh(k) => u64::from(k.entry_point_32()),
        }
    }

    /// Place the kernel, cmdline, initrd and boot info in guest RAM.
    ///
    /// Must run after the ACPI tables are written: `acpi_rsdp_addr` has
    /// to already point at a real RSDP.
    pub fn load(
        &self,
        mem: &MemCtx,
        cmdline: &str,
        initrd: Option<&InitrdImage>,
        mem_size: usize,
        acpi_rsdp_addr: u64,
    ) -> Result<u64> {
        match self {
            Self::Bzimage(k) => crate::load_kernel(
                mem,
                k,
                cmdline,
                initrd,
                mem_size,
                acpi_rsdp_addr,
            ),
            Self::Pvh(k) => crate::load_pvh_kernel(
                mem,
                k,
                cmdline,
                initrd,
                mem_size,
                acpi_rsdp_addr,
            )
            .map(u64::from),
        }
    }

    /// Program the BSP for this image's entry convention.
    ///
    /// The entry point travels inside the image, so a caller cannot
    /// pair one protocol's entry with the other's register state.
    pub fn setup_bsp<V: BootVcpu + ?Sized>(&self, vcpu: &V) -> Result<()> {
        match self {
            Self::Bzimage(k) => {
                crate::setup_direct_boot_bsp(vcpu, k.entry_point_64())
            }
            Self::Pvh(k) => crate::setup_pvh_bsp(vcpu, k.entry_point_32()),
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal bzImage: only the fields `KernelImage::from_bytes` reads.
    fn bzimage_bytes() -> Vec<u8> {
        let mut img = vec![0u8; 0x2_0000];
        // "HdrS" at 0x202, protocol 0x020F, XLF_KERNEL_64, 4 setup
        // sectors, pref_address 0x0100_0000.
        img[0x202..0x206].copy_from_slice(&0x5372_6448u32.to_le_bytes());
        img[0x206..0x208].copy_from_slice(&0x020Fu16.to_le_bytes());
        img[0x236..0x238].copy_from_slice(&1u16.to_le_bytes());
        img[0x1F1] = 4;
        img[0x258..0x260].copy_from_slice(&0x0100_0000u64.to_le_bytes());
        img
    }

    /// Minimal PVH ELF64: one PT_NOTE holding the Xen 32-bit entry
    /// note, one PT_LOAD.
    fn pvh_elf_bytes() -> Vec<u8> {
        let mut notes = Vec::new();
        notes.extend_from_slice(&4u32.to_le_bytes());
        notes.extend_from_slice(&4u32.to_le_bytes());
        notes.extend_from_slice(&18u32.to_le_bytes());
        notes.extend_from_slice(b"Xen\0");
        notes.extend_from_slice(&0x0010_0100u32.to_le_bytes());

        let mut img = vec![0u8; 0x400];
        img[0..4].copy_from_slice(b"\x7fELF");
        img[4] = 2;
        img[5] = 1;
        img[0x12..0x14].copy_from_slice(&0x3Eu16.to_le_bytes());
        img[0x20..0x28].copy_from_slice(&64u64.to_le_bytes());
        img[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        img[0x38..0x3A].copy_from_slice(&2u16.to_le_bytes());
        // phdr 0: PT_NOTE at 0x100. phdr 1: PT_LOAD at 0x200 -> 0x100000.
        img[64..68].copy_from_slice(&4u32.to_le_bytes());
        img[72..80].copy_from_slice(&0x100u64.to_le_bytes());
        img[96..104].copy_from_slice(&(notes.len() as u64).to_le_bytes());
        img[104..112].copy_from_slice(&(notes.len() as u64).to_le_bytes());
        img[120..124].copy_from_slice(&1u32.to_le_bytes());
        img[128..136].copy_from_slice(&0x200u64.to_le_bytes());
        img[144..152].copy_from_slice(&0x10_0000u64.to_le_bytes());
        img[152..160].copy_from_slice(&8u64.to_le_bytes());
        img[160..168].copy_from_slice(&8u64.to_le_bytes());
        img[0x100..0x100 + notes.len()].copy_from_slice(&notes);
        img
    }

    #[test]
    fn detects_a_bzimage_by_its_hdrs_magic() {
        assert_eq!(
            detect_boot_protocol(&bzimage_bytes()).expect("detects"),
            BootProtocol::Bzimage
        );
    }

    #[test]
    fn detects_pvh_by_the_elf_magic() {
        assert_eq!(
            detect_boot_protocol(&pvh_elf_bytes()).expect("detects"),
            BootProtocol::Pvh
        );
    }

    #[test]
    fn rejects_a_file_that_is_neither() {
        let err = detect_boot_protocol(&[0u8; 0x1000])
            .expect_err("an unrecognized image must be an error");
        let msg = err.to_string();
        assert!(msg.contains("HdrS"), "got: {msg}");
        assert!(msg.contains("ELF"), "got: {msg}");
    }

    #[test]
    fn never_falls_back_to_bzimage_for_a_short_file() {
        // A 16-byte file has no 0x202 offset to read.
        assert!(detect_boot_protocol(&[0u8; 16]).is_err());
    }

    /// The two protocols leave the guest in states that share no
    /// registers, so a crossed dispatch is a triple fault, not a
    /// warning. `setup_bsp` is the only place the pairing is decided.
    #[test]
    fn setup_bsp_programs_the_registers_of_the_detected_protocol() {
        use bhyve_api::vm_reg_name::*;

        use crate::recording::{reg, RecordingVcpu};

        let bz = BootImage::Bzimage(
            KernelImage::from_bytes(bzimage_bytes()).expect("bzImage"),
        );
        let cpu = RecordingVcpu::default();
        bz.setup_bsp(&cpu).expect("the recorder never fails");
        let writes = cpu.writes();
        // Long mode: CR0.PG set, and RIP at the 64-bit entry.
        assert_eq!(writes[1], reg(VM_REG_GUEST_CR0, 0x8000_0031));
        assert!(writes.contains(&reg(VM_REG_GUEST_RIP, 0x0100_0200)));
        // bzImage passes boot_params in RSI.
        assert!(writes.contains(&reg(VM_REG_GUEST_RSI, 0x7000)));

        let pvh = BootImage::Pvh(
            PvhKernel::from_bytes(pvh_elf_bytes()).expect("PVH ELF"),
        );
        let cpu = RecordingVcpu::default();
        pvh.setup_bsp(&cpu).expect("the recorder never fails");
        let writes = cpu.writes();
        // Protected mode: paging off, and RIP at the 32-bit entry.
        assert_eq!(writes[1], reg(VM_REG_GUEST_CR0, 0x11));
        assert!(writes.contains(&reg(VM_REG_GUEST_RIP, 0x0010_0100)));
        // PVH passes hvm_start_info in EBX, and leaves RSI zero.
        assert!(writes.contains(&reg(VM_REG_GUEST_RBX, 0x1000)));
        assert!(writes.contains(&reg(VM_REG_GUEST_RSI, 0)));
    }

    #[test]
    fn entry_point_follows_the_detected_protocol() {
        let bz = BootImage::Bzimage(
            KernelImage::from_bytes(bzimage_bytes()).expect("bzImage"),
        );
        assert_eq!(bz.protocol(), BootProtocol::Bzimage);
        assert_eq!(bz.entry_point(), 0x0100_0200);

        let pvh = BootImage::Pvh(
            PvhKernel::from_bytes(pvh_elf_bytes()).expect("PVH ELF"),
        );
        assert_eq!(pvh.protocol(), BootProtocol::Pvh);
        assert_eq!(pvh.entry_point(), 0x0010_0100);
    }
}
