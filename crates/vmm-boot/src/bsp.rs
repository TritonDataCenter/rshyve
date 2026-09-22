// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The BSP register state both boot protocols program.
//!
//! Linux 64-bit entry fixes only RSI and the PVH ABI fixes only EBX.
//! Everything else is the same flat, paging-agnostic segment model and
//! the same "every other GPR is zero" choice, so it lives here once.

use anyhow::Result;
use bhyve_api::{
    seg_desc, vm_reg_name, vm_reg_name::*, SEG_ACCESS_P, SEG_ACCESS_UNUSABLE,
};

use crate::BootVcpu;

// ── Segment access type bits ────────────────────────────────────────

/// Execute/Read/Accessed (code segment).
pub(crate) const SEG_TYPE_CODE_XRA: u32 = 0x0B;
/// Read/Write/Accessed (data segment).
pub(crate) const SEG_TYPE_DATA_RWA: u32 = 0x03;
/// Busy TSS. VMX refuses entry with a TR that is not present.
pub(crate) const SEG_TYPE_TSS_BUSY: u32 = 0x0B;

// ── Control and flag register bits ──────────────────────────────────

pub(crate) const CR0_PE: u64 = 1 << 0; // Protection Enable
pub(crate) const CR0_ET: u64 = 1 << 4; // Extension Type (387 coprocessor)

/// RFLAGS bit 1 is architecturally reserved as 1.
pub(crate) const RFLAGS_RESERVED_BIT1: u64 = 0x02;

/// Data segment descriptor, identical in both GDTs: base 0, limit
/// 0xFFFFF, type 0x03, S=1, P=1, D/B=1, G=1.
pub(crate) const GDT_DATA: u64 = 0x00CF_9300_0000_FFFF;

/// GDT selector for the code segment (entry 1).
pub(crate) const GDT_SEL_CODE: u64 = 0x08;
/// GDT selector for the data segment (entry 2).
pub(crate) const GDT_SEL_DATA: u64 = 0x10;

/// The segment state a boot protocol chooses.
///
/// Only the CS access byte and the GDT location differ between the two:
/// long mode sets L, protected mode sets D/B.
pub(crate) struct FlatSegments {
    pub code_access: u32,
    pub gdt_base: u64,
    pub gdt_limit: u32,
}

/// Program CS, the five data segments, GDTR, TR, LDTR and IDTR.
///
/// The order of the writes is part of the contract the recorder tests
/// pin, because a vCPU rejects some intermediate states.
pub(crate) fn program_flat_segments<V: BootVcpu + ?Sized>(
    vcpu: &V,
    seg: &FlatSegments,
) -> Result<()> {
    use bhyve_api::{SEG_ACCESS_DB, SEG_ACCESS_G, SEG_ACCESS_S};

    vcpu.set_segment_desc(
        VM_REG_GUEST_CS,
        &seg_desc {
            base: 0,
            limit: u32::MAX,
            access: seg.code_access,
        },
    )?;
    vcpu.set_reg(VM_REG_GUEST_CS, GDT_SEL_CODE)?;

    let data_desc = seg_desc {
        base: 0,
        limit: u32::MAX,
        access: SEG_ACCESS_P
            | SEG_ACCESS_S
            | SEG_ACCESS_DB
            | SEG_ACCESS_G
            | SEG_TYPE_DATA_RWA,
    };
    for reg in [
        VM_REG_GUEST_DS,
        VM_REG_GUEST_ES,
        VM_REG_GUEST_FS,
        VM_REG_GUEST_GS,
        VM_REG_GUEST_SS,
    ] {
        vcpu.set_segment_desc(reg, &data_desc)?;
        vcpu.set_reg(reg, GDT_SEL_DATA)?;
    }

    vcpu.set_segment_desc(
        VM_REG_GUEST_GDTR,
        &seg_desc {
            base: seg.gdt_base,
            limit: seg.gdt_limit,
            access: 0,
        },
    )?;

    // VMX needs a present TR and an unusable LDTR before entry.
    vcpu.set_segment_desc(
        VM_REG_GUEST_TR,
        &seg_desc {
            base: 0,
            limit: u16::MAX as u32,
            access: SEG_ACCESS_P | SEG_TYPE_TSS_BUSY,
        },
    )?;
    vcpu.set_reg(VM_REG_GUEST_TR, 0)?;
    vcpu.set_segment_desc(
        VM_REG_GUEST_LDTR,
        &seg_desc {
            base: 0,
            limit: 0,
            access: SEG_ACCESS_UNUSABLE,
        },
    )?;
    vcpu.set_reg(VM_REG_GUEST_LDTR, 0)?;
    // No IDT: the kernel builds its own.
    vcpu.set_segment_desc(
        VM_REG_GUEST_IDTR,
        &seg_desc {
            base: 0,
            limit: u16::MAX as u32,
            access: 0,
        },
    )?;
    Ok(())
}

/// Every general-purpose register, in the order both protocols write.
const GPRS: [vm_reg_name; 16] = [
    VM_REG_GUEST_RAX,
    VM_REG_GUEST_RBX,
    VM_REG_GUEST_RCX,
    VM_REG_GUEST_RDX,
    VM_REG_GUEST_RSI,
    VM_REG_GUEST_RDI,
    VM_REG_GUEST_RBP,
    VM_REG_GUEST_RSP,
    VM_REG_GUEST_R8,
    VM_REG_GUEST_R9,
    VM_REG_GUEST_R10,
    VM_REG_GUEST_R11,
    VM_REG_GUEST_R12,
    VM_REG_GUEST_R13,
    VM_REG_GUEST_R14,
    VM_REG_GUEST_R15,
];

/// Zero every GPR except the one the boot protocol fixes.
///
/// Both entry contracts leave every other GPR unspecified, and zeroing
/// them is the deterministic choice: a guest that reads one gets the
/// same value on every host and after every reboot.
pub(crate) fn zero_gprs<V: BootVcpu + ?Sized>(
    vcpu: &V,
    keep: vm_reg_name,
) -> Result<()> {
    for reg in GPRS {
        if reg as i32 == keep as i32 {
            continue;
        }
        vcpu.set_reg(reg, 0)?;
    }
    Ok(())
}
