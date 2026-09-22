// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Hyper-V CPUID leaf and MSR constants.
//!
//! Derived from Microsoft's Hypervisor Top-Level Functional Specification
//! (TLFS) v6.0b. Naming follows the TLFS where possible.

use bitflags::bitflags;

/// Highest hypervisor CPUID leaf populated.
///
/// TLFS requires at least 0x4000_0005 for any Hyper-V-compatible
/// hypervisor. 0x4000_0006 advertises hardware features (the
/// frequencies leaf).
pub const HYPERV_MAX_CPUID_LEAF: u32 = 0x4000_0006;

/// CPUID leaf 0x4000_0000: hypervisor identification.
///
/// eax = highest valid hypervisor CPUID leaf.
/// ebx/ecx/edx = 12-byte vendor signature. Linux accepts only
/// "Microsoft Hv" here, so this is always the Microsoft signature.
pub const HV_CPUID_VENDOR_EBX: u32 = 0x7263_694D; // "Micr"
pub const HV_CPUID_VENDOR_ECX: u32 = 0x666F_736F; // "osof"
pub const HV_CPUID_VENDOR_EDX: u32 = 0x7648_2074; // "t Hv"

/// CPUID leaf 0x4000_0001: interface signature. eax = "Hv#1".
pub const HV_CPUID_INTERFACE_EAX: u32 = 0x3123_7648;

bitflags! {
    /// CPUID leaf 0x4000_0003 EAX: per-MSR access privilege flags.
    ///
    /// Only the bits in use are listed. The other TLFS bits gate
    /// features that are not implemented (synic, stimer, virtual APIC
    /// and others) and must stay clear.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct HvLeaf3Eax: u32 {
        const PARTITION_REFERENCE_COUNTER = 1 << 1;
        const HYPERCALL                  = 1 << 5;
        const VP_INDEX                   = 1 << 6;
        const SYSTEM_RESET               = 1 << 7;
        const FREQUENCIES                = 1 << 8;
        const PARTITION_REFERENCE_TSC    = 1 << 9;
    }
}

bitflags! {
    /// CPUID leaf 0x4000_0003 EDX: miscellaneous feature availability.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct HvLeaf3Edx: u32 {
        /// Linux and Windows both test this before they write the
        /// crash MSRs, so the BSOD payload depends on it.
        const GUEST_CRASH_MSRS = 1 << 10;
    }
}

bitflags! {
    /// CPUID leaf 0x4000_0004 EAX: recommended guest behaviors.
    ///
    /// `RELAXED_TIMING` disables Windows' CLOCK_WATCHDOG bugcheck on
    /// slow vCPUs. Always recommended for non-Hyper-V hypervisors.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct HvLeaf4Eax: u32 {
        const USE_HYPERCALL_FOR_TLB_FLUSH      = 1 << 1;
        const USE_HYPERCALL_FOR_REMOTE_TLB     = 1 << 2;
        const USE_MSR_FOR_APIC                 = 1 << 3;
        const USE_MSR_FOR_RESET                = 1 << 4;
        const RELAXED_TIMING                   = 1 << 5;
    }
}

// ---- MSR addresses ----

pub const HV_X64_MSR_GUEST_OS_ID: u32 = 0x4000_0000;
pub const HV_X64_MSR_HYPERCALL: u32 = 0x4000_0001;
pub const HV_X64_MSR_VP_INDEX: u32 = 0x4000_0002;
pub const HV_X64_MSR_RESET: u32 = 0x4000_0003;

pub const HV_X64_MSR_TIME_REF_COUNT: u32 = 0x4000_0020;
pub const HV_X64_MSR_REFERENCE_TSC: u32 = 0x4000_0021;

/// Crash dump MSRs. Windows writes BSOD parameters here before the
/// triple fault, then sets the NOTIFY bit on `CRASH_CTL`. The
/// parameters are logged and otherwise opaque.
pub const HV_X64_MSR_CRASH_P0: u32 = 0x4000_0100;
pub const HV_X64_MSR_CRASH_P1: u32 = 0x4000_0101;
pub const HV_X64_MSR_CRASH_P2: u32 = 0x4000_0102;
pub const HV_X64_MSR_CRASH_P3: u32 = 0x4000_0103;
pub const HV_X64_MSR_CRASH_P4: u32 = 0x4000_0104;
pub const HV_X64_MSR_CRASH_CTL: u32 = 0x4000_0105;

/// Bit 63 of CRASH_CTL. The guest sets it when the crash data is
/// published.
pub const HV_CRASH_CTL_NOTIFY: u64 = 1 << 63;

// ---- Quick predicates ----

/// True if `msr` is a Hyper-V synthetic MSR this crate handles. The
/// central MSR dispatcher uses it to skip the enlightenment.
pub const fn is_hyperv_msr(msr: u32) -> bool {
    matches!(
        msr,
        HV_X64_MSR_GUEST_OS_ID
            | HV_X64_MSR_HYPERCALL
            | HV_X64_MSR_VP_INDEX
            | HV_X64_MSR_RESET
            | HV_X64_MSR_TIME_REF_COUNT
            | HV_X64_MSR_REFERENCE_TSC
            | HV_X64_MSR_CRASH_P0
            | HV_X64_MSR_CRASH_P1
            | HV_X64_MSR_CRASH_P2
            | HV_X64_MSR_CRASH_P3
            | HV_X64_MSR_CRASH_P4
            | HV_X64_MSR_CRASH_CTL
    )
}
