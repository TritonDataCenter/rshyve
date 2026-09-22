// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The CPUID table a VM programs through `VM_SET_CPUID`.
//!
//! The kernel's own CPUID handling (`vmm_cpuid.c`, the legacy path)
//! rewrites the leaves that depend on the VM: the APIC IDs, the
//! topology counts and the XSAVE limits. It does none of that for an
//! explicit table, so this module has to. The table is built once from
//! the host, with the topology of this VM in it, and every vCPU gets
//! a copy with its own APIC ID.
//!
//! A baseline other than `Host` also masks the features above it, for
//! migration between hosts of different generations.

use bhyve_api::{
    vcpu_cpuid_entry, VCC_FLAG_INTEL_FALLBACK, VCE_FLAG_MATCH_INDEX,
};

use slog;

use crate::hdl::VmmHdl;

/// CPU baseline profiles for migration compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuBaseline {
    /// No masking: the host's features, as they are.
    Host,
    /// Mask out AVX-512 and related features.
    /// Compatible with any x86-64 host that has at least AVX2.
    Avx2,
    /// Mask out AVX-512, AVX2, FMA, BMI1/2, and other Haswell+ features.
    /// Maximum compatibility: SSE4.2 baseline.
    Sse42,
}

/// A `--cpu-baseline` value this build does not know.
#[derive(Debug, thiserror::Error)]
#[error("unknown CPU baseline '{0}' (valid: host, avx2, no-avx512, sse42)")]
pub struct UnknownCpuBaseline(String);

impl std::str::FromStr for CpuBaseline {
    type Err = UnknownCpuBaseline;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "host" | "native" => Ok(Self::Host),
            "avx2" | "no-avx512" => Ok(Self::Avx2),
            "sse42" | "sse4.2" | "westmere" => Ok(Self::Sse42),
            _ => Err(UnknownCpuBaseline(s.to_string())),
        }
    }
}

/// The four registers one CPUID query returns.
type Regs = (u32, u32, u32, u32);

/// A source of CPUID answers, so a table can be built from a recorded
/// host in tests.
pub trait CpuidSource {
    fn cpuid(&self, leaf: u32, subleaf: u32) -> Regs;

    /// The XCR0 bits the host itself has enabled, or `None` where that
    /// cannot be read. The kernel only lets a guest use those.
    fn enabled_xcr0(&self) -> Option<u64>;
}

/// The CPU this process runs on.
pub struct HostCpu;

impl CpuidSource for HostCpu {
    fn cpuid(&self, leaf: u32, subleaf: u32) -> Regs {
        #[cfg(target_arch = "x86_64")]
        {
            let r = std::arch::x86_64::__cpuid_count(leaf, subleaf);
            (r.eax, r.ebx, r.ecx, r.edx)
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = (leaf, subleaf);
            (0, 0, 0, 0)
        }
    }

    fn enabled_xcr0(&self) -> Option<u64> {
        #[cfg(target_arch = "x86_64")]
        {
            let (_, _, ecx1, _) = self.cpuid(1, 0);
            if ecx1 & LEAF1_ECX_OSXSAVE == 0 {
                return None;
            }
            // SAFETY: XGETBV(0) only faults when CR4.OSXSAVE is clear,
            // and the OSXSAVE bit above says the OS has set it.
            Some(unsafe { std::arch::x86_64::_xgetbv(0) })
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            None
        }
    }
}

// Leaf 1 bits the table sets or clears, as the kernel legacy path does.
const LEAF1_ECX_MONITOR: u32 = 1 << 3;
const LEAF1_ECX_VMX: u32 = 1 << 5;
const LEAF1_ECX_SMX: u32 = 1 << 6;
const LEAF1_ECX_EST: u32 = 1 << 7;
const LEAF1_ECX_TM2: u32 = 1 << 8;
const LEAF1_ECX_FMA: u32 = 1 << 12;
const LEAF1_ECX_PDCM: u32 = 1 << 15;
const LEAF1_ECX_TSC_DEADLINE: u32 = 1 << 24;
#[cfg(target_arch = "x86_64")]
const LEAF1_ECX_OSXSAVE: u32 = 1 << 27;
const LEAF1_ECX_AVX: u32 = 1 << 28;
const LEAF1_ECX_F16C: u32 = 1 << 29;
const LEAF1_ECX_RDRAND: u32 = 1 << 30;
const LEAF1_ECX_HYPERVISOR: u32 = 1 << 31;
const LEAF1_EDX_MCE: u32 = 1 << 7;
const LEAF1_EDX_MTRR: u32 = 1 << 12;
const LEAF1_EDX_MCA: u32 = 1 << 14;
const LEAF1_EDX_DS: u32 = 1 << 21;
const LEAF1_EDX_ACPI: u32 = 1 << 22;
const LEAF1_EDX_HTT: u32 = 1 << 28;
const LEAF1_EDX_TM: u32 = 1 << 29;
const LEAF1_EBX_APIC_ID_SHIFT: u32 = 24;
const LEAF1_EBX_LOGICAL_COUNT_SHIFT: u32 = 16;

/// Leaf 4 EAX: cache level, sharing count and cores per package.
const LEAF4_EAX_LEVEL_SHIFT: u32 = 5;
const LEAF4_EAX_SHARING_SHIFT: u32 = 14;
const LEAF4_EAX_CORES_SHIFT: u32 = 26;
const LEAF4_EAX_TYPE_MASK: u32 = 0x1F;
const LEAF4_EAX_STATIC_MASK: u32 = 0x3FF;

/// Leaf 0xB ECX[15:8] level types.
const LEAF_B_TYPE_SMT: u32 = 1;
const LEAF_B_TYPE_CORE: u32 = 2;

/// The XSAVE area up to and including the SSE state.
const XSAVE_LEGACY_SIZE: u32 = 576;
/// The XCR0 components with a fixed place in the legacy area.
const XCR0_LEGACY: u64 = 0b11;
const XCR0_AVX: u64 = 1 << 2;
const XCR0_AVX512: u64 = (1 << 5) | (1 << 6) | (1 << 7);
const XCR0_AMX: u64 = (1 << 17) | (1 << 18);

// Leaf 7 bits.
const LEAF7_EBX_BMI1: u32 = 1 << 3;
const LEAF7_EBX_AVX2: u32 = 1 << 5;
const LEAF7_EBX_BMI2: u32 = 1 << 8;
const LEAF7_EBX_AVX512: u32 = (1 << 16)
    | (1 << 17)
    | (1 << 21)
    | (1 << 26)
    | (1 << 27)
    | (1 << 28)
    | (1 << 30)
    | (1 << 31);
const LEAF7_ECX_AVX512: u32 =
    (1 << 1) | (1 << 6) | (1 << 11) | (1 << 12) | (1 << 14);
const LEAF7_EDX_AVX512_AMX: u32 =
    (1 << 2) | (1 << 3) | (1 << 8) | (1 << 23) | (1 << 24) | (1 << 25);
/// Leaf 7.1 EAX: AVX-VNNI (4), AVX-512_BF16 (5), AVX-IFMA (23).
const LEAF7_1_EAX_VECTOR: u32 = (1 << 4) | (1 << 5) | (1 << 23);

/// The feature bits a baseline takes away from the host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FeatureMask {
    leaf1_ecx: u32,
    leaf7_ebx: u32,
    leaf7_ecx: u32,
    leaf7_edx: u32,
    leaf7_1_eax: u32,
    xcr0: u64,
}

impl FeatureMask {
    fn for_baseline(baseline: CpuBaseline) -> Self {
        match baseline {
            CpuBaseline::Host => Self::default(),
            CpuBaseline::Avx2 => Self {
                leaf7_ebx: LEAF7_EBX_AVX512,
                leaf7_ecx: LEAF7_ECX_AVX512,
                leaf7_edx: LEAF7_EDX_AVX512_AMX,
                leaf7_1_eax: LEAF7_1_EAX_VECTOR,
                xcr0: XCR0_AVX512 | XCR0_AMX,
                ..Self::default()
            },
            CpuBaseline::Sse42 => Self {
                leaf1_ecx: LEAF1_ECX_AVX
                    | LEAF1_ECX_FMA
                    | LEAF1_ECX_F16C
                    | LEAF1_ECX_RDRAND,
                leaf7_ebx: LEAF7_EBX_AVX512
                    | LEAF7_EBX_BMI1
                    | LEAF7_EBX_AVX2
                    | LEAF7_EBX_BMI2,
                leaf7_ecx: LEAF7_ECX_AVX512,
                leaf7_edx: LEAF7_EDX_AVX512_AMX,
                leaf7_1_eax: LEAF7_1_EAX_VECTOR,
                xcr0: XCR0_AVX | XCR0_AVX512 | XCR0_AMX,
            },
        }
    }
}

fn entry(
    func: u32,
    index: u32,
    flags: u32,
    (eax, ebx, ecx, edx): Regs,
) -> vcpu_cpuid_entry {
    vcpu_cpuid_entry {
        vce_function: func,
        vce_index: index,
        vce_flags: flags,
        vce_eax: eax,
        vce_ebx: ebx,
        vce_ecx: ecx,
        vce_edx: edx,
        _pad: 0,
    }
}

fn is_intel((_, ebx, ecx, edx): Regs) -> bool {
    ebx == 0x756e_6547 && edx == 0x4965_6e69 && ecx == 0x6c65_746e
}

/// `ceil(log2(n))`: the bits an ID needs to count `n` things.
fn id_width(n: u32) -> u32 {
    n.next_power_of_two().trailing_zeros()
}

/// Build the table for `baseline` from `host`, for a VM of `num_cpus`
/// vCPUs in one socket with one thread per core, which is the topology
/// `set_topology` programs.
///
/// The entries are sorted for the kernel. The leaves that vary by
/// vCPU still carry vCPU 0. [`specialize_for_vcpu`] fills in the rest.
fn build_table(
    host: &dyn CpuidSource,
    baseline: CpuBaseline,
    num_cpus: u32,
) -> Vec<vcpu_cpuid_entry> {
    let mask = FeatureMask::for_baseline(baseline);
    let mut entries = Vec::with_capacity(64);

    let leaf0 = host.cpuid(0, 0);
    let max_leaf = leaf0.0;
    let intel = is_intel(leaf0);
    entries.push(entry(0, 0, 0, leaf0));

    entries.push(entry(1, 0, 0, leaf1(host.cpuid(1, 0), &mask, num_cpus)));

    if intel {
        entries.push(entry(2, 0, 0, host.cpuid(2, 0)));
    }

    if intel && max_leaf >= 4 {
        for idx in 0..8u32 {
            let regs = host.cpuid(4, idx);
            if regs.0 & LEAF4_EAX_TYPE_MASK == 0 {
                break;
            }
            let regs = leaf4(regs, num_cpus);
            entries.push(entry(4, idx, VCE_FLAG_MATCH_INDEX, regs));
        }
    }

    if max_leaf >= 5 {
        entries.push(entry(5, 0, 0, host.cpuid(5, 0)));
    }
    if max_leaf >= 6 {
        entries.push(entry(6, 0, 0, host.cpuid(6, 0)));
    }

    if max_leaf >= 7 {
        let (eax7, ebx7, ecx7, edx7) = host.cpuid(7, 0);
        let regs = (
            eax7,
            ebx7 & !mask.leaf7_ebx,
            ecx7 & !mask.leaf7_ecx,
            edx7 & !mask.leaf7_edx,
        );
        entries.push(entry(7, 0, VCE_FLAG_MATCH_INDEX, regs));
        if eax7 >= 1 {
            let (a, b, c, d) = host.cpuid(7, 1);
            let regs = (a & !mask.leaf7_1_eax, b, c, d);
            entries.push(entry(7, 1, VCE_FLAG_MATCH_INDEX, regs));
        }
    }

    // Leaves 0xA and 0x15 are left to the empty entry, as the kernel
    // zeroes them: the guest gets no PMU, and no crystal clock it could
    // derive the local APIC frequency from.

    if max_leaf >= 0xB {
        for (idx, regs) in leaf_b(num_cpus, 0).into_iter().enumerate() {
            entries.push(entry(0xB, idx as u32, VCE_FLAG_MATCH_INDEX, regs));
        }
    }

    if max_leaf >= 0xD {
        entries.extend(leaf_d(host, &mask));
    }

    if max_leaf >= 0x16 {
        entries.push(entry(0x16, 0, 0, host.cpuid(0x16, 0)));
    }

    let (max_ext, _, _, _) = host.cpuid(0x8000_0000, 0);
    entries.push(entry(0x8000_0000, 0, 0, (max_ext, 0, 0, 0)));
    for leaf in 0x8000_0001..=0x8000_0007 {
        if max_ext >= leaf {
            entries.push(entry(leaf, 0, 0, host.cpuid(leaf, 0)));
        }
    }
    if max_ext >= 0x8000_0008 {
        let regs = leaf_8000_0008(host.cpuid(0x8000_0008, 0), num_cpus);
        entries.push(entry(0x8000_0008, 0, 0, regs));
    }
    if !intel && max_ext >= 0x8000_001E {
        entries.push(entry(0x8000_001E, 0, 0, leaf_8000_001e(0)));
    }

    entries.sort_by(vcpu_cpuid_entry::eval_sort);
    entries
}

/// Leaf 1 with the VM's logical processor count, the hypervisor bit,
/// and the features the kernel never lets a guest use.
fn leaf1(
    (eax, ebx, ecx, edx): Regs,
    mask: &FeatureMask,
    num_cpus: u32,
) -> Regs {
    let ebx = (ebx & 0x0000_FFFF)
        | ((num_cpus & 0xFF) << LEAF1_EBX_LOGICAL_COUNT_SHIFT);
    // MONITOR/MWAIT and TSC-deadline timers are not emulated. VMX, SMX
    // and the power and debug features would trap or misbehave.
    let ecx = (ecx
        & !(LEAF1_ECX_MONITOR
            | LEAF1_ECX_VMX
            | LEAF1_ECX_SMX
            | LEAF1_ECX_EST
            | LEAF1_ECX_TM2
            | LEAF1_ECX_PDCM
            | LEAF1_ECX_TSC_DEADLINE
            | mask.leaf1_ecx))
        | LEAF1_ECX_HYPERVISOR;
    // Windows refuses to boot without MCA, MCE and MTRR.
    let edx = (edx & !(LEAF1_EDX_DS | LEAF1_EDX_ACPI | LEAF1_EDX_TM))
        | LEAF1_EDX_MCA
        | LEAF1_EDX_MCE
        | LEAF1_EDX_MTRR
        | LEAF1_EDX_HTT;
    (eax, ebx, ecx, edx)
}

/// Leaf 4 with this VM's sharing: L1 and L2 belong to one core, L3
/// and above to the whole package.
fn leaf4((eax, ebx, ecx, edx): Regs, num_cpus: u32) -> Regs {
    let level = (eax >> LEAF4_EAX_LEVEL_SHIFT) & 0x7;
    let sharing = if level >= 3 { num_cpus - 1 } else { 0 };
    let eax = (eax & LEAF4_EAX_STATIC_MASK)
        | (sharing << LEAF4_EAX_SHARING_SHIFT)
        | ((num_cpus - 1) << LEAF4_EAX_CORES_SHIFT);
    (eax, ebx, ecx, edx)
}

/// Leaf 0xB for `vcpuid`: one thread per core, every core in one
/// package. Subleaf 2 is the "no such level" answer the SDM asks for,
/// so a guest walking up stops there and still reads its x2APIC ID.
fn leaf_b(num_cpus: u32, vcpuid: u32) -> [Regs; 3] {
    [
        (0, 1, LEAF_B_TYPE_SMT << 8, vcpuid),
        (
            id_width(num_cpus),
            num_cpus & 0xFFFF,
            (LEAF_B_TYPE_CORE << 8) | 1,
            vcpuid,
        ),
        (0, 0, 2, vcpuid),
    ]
}

/// Leaf 0x8000_0008 with this VM's core count and APIC ID width.
fn leaf_8000_0008((eax, ebx, ecx, edx): Regs, num_cpus: u32) -> Regs {
    let ecx =
        (ecx & !0xF0FF) | ((num_cpus - 1) & 0xFF) | (id_width(num_cpus) << 12);
    (eax, ebx, ecx, edx)
}

/// Leaf 0x8000_001E for `vcpuid` on AMD: core `vcpuid`, one thread,
/// one node.
fn leaf_8000_001e(vcpuid: u32) -> Regs {
    (vcpuid, vcpuid & 0xFF, 0, 0)
}

/// Leaf 0xD: the XSAVE components the guest may enable, the area they
/// need, and one subleaf for each of them.
///
/// The kernel patches EBX of subleaves 0 and 1 at run time from the
/// guest's XCR0, and nothing else, so every other value has to be
/// right here. A component the guest can enable but cannot size, or a
/// maximum too small for the components advertised, makes Linux
/// disable XSAVE.
fn leaf_d(host: &dyn CpuidSource, mask: &FeatureMask) -> Vec<vcpu_cpuid_entry> {
    let (eax0, ebx0, _, edx0) = host.cpuid(0xD, 0);
    let mut xcr0 = (u64::from(edx0) << 32 | u64::from(eax0)) & !mask.xcr0;
    if let Some(enabled) = host.enabled_xcr0() {
        xcr0 &= enabled;
    }
    let (eax1, ebx1, ecx1, edx1) = host.cpuid(0xD, 1);
    let xss = u64::from(edx1) << 32 | u64::from(ecx1);

    let mut components = Vec::new();
    let mut max_size = XSAVE_LEGACY_SIZE;
    for bit in 2..64u32 {
        let in_xcr0 = xcr0 & (1 << bit) != 0;
        if !in_xcr0 && xss & (1 << bit) == 0 {
            continue;
        }
        let regs = host.cpuid(0xD, bit);
        let (size, offset) = (regs.0, regs.1);
        if size == 0 {
            continue;
        }
        if in_xcr0 {
            max_size = max_size.max(offset.saturating_add(size));
        }
        components.push(entry(0xD, bit, VCE_FLAG_MATCH_INDEX, regs));
    }

    let mut entries = vec![
        entry(
            0xD,
            0,
            VCE_FLAG_MATCH_INDEX,
            (xcr0 as u32, ebx0, max_size, (xcr0 >> 32) as u32),
        ),
        entry(0xD, 1, VCE_FLAG_MATCH_INDEX, (eax1, ebx1, ecx1, edx1)),
    ];
    entries.extend(components);
    entries
}

/// Give `entries` the APIC IDs of `vcpuid`.
///
/// The kernel's explicit path does not touch these (`vmm_cpuid.c`,
/// `cpuid_apply_runtime_reg_state`), and a guest that sees the same ID
/// on every CPU treats the machine as one core.
fn specialize_for_vcpu(entries: &mut [vcpu_cpuid_entry], vcpuid: u32) {
    for e in entries.iter_mut() {
        match e.vce_function {
            1 => {
                e.vce_ebx = (e.vce_ebx & 0x00FF_FFFF)
                    | ((vcpuid & 0xFF) << LEAF1_EBX_APIC_ID_SHIFT);
            }
            0xB => e.vce_edx = vcpuid,
            0x8000_001E => {
                let (a, b, c, d) = leaf_8000_001e(vcpuid);
                (e.vce_eax, e.vce_ebx, e.vce_ecx, e.vce_edx) = (a, b, c, d);
            }
            _ => {}
        }
    }
}

/// The CPUID table one VM programs, built once and replayed per vCPU.
///
/// A vCPU brought online after boot has to get the table its siblings
/// got. Two feature sets on one machine is a guest fault that is very
/// hard to read back, so the table is a value the caller keeps, not a
/// loop only the boot path can run.
#[derive(Clone)]
pub struct CpuidTable {
    entries: Vec<vcpu_cpuid_entry>,
    flags: u32,
}

impl CpuidTable {
    /// Build the table for `baseline` plus any vendor-defined `extra`
    /// entries, or `None` when there is nothing to program.
    ///
    /// `None` means a pure native passthrough: the kernel answers
    /// CPUID from the host, so no vCPU needs an override.
    ///
    /// When `baseline == Host` and `extra` is non-empty, this builds a
    /// full host table first so the kernel does not lose the standard
    /// leaves: `set_cpuid` replaces the kernel table wholesale, so a
    /// partial override cannot be shipped.
    pub fn build(
        baseline: CpuBaseline,
        extra: &[vcpu_cpuid_entry],
        num_cpus: u32,
    ) -> Option<Self> {
        Self::build_from(&HostCpu, baseline, extra, num_cpus)
    }

    /// [`Self::build`] from any CPUID source.
    pub fn build_from(
        host: &dyn CpuidSource,
        baseline: CpuBaseline,
        extra: &[vcpu_cpuid_entry],
        num_cpus: u32,
    ) -> Option<Self> {
        if baseline == CpuBaseline::Host && extra.is_empty() {
            return None;
        }
        let mut entries = build_table(host, baseline, num_cpus.max(1));
        if !extra.is_empty() {
            entries.extend_from_slice(extra);
            entries.sort_by(vcpu_cpuid_entry::eval_sort);
        }

        let flags = if is_intel(host.cpuid(0, 0)) {
            VCC_FLAG_INTEL_FALLBACK
        } else {
            0
        };
        Some(Self { entries, flags })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The `VCC_*` flags this table is programmed with.
    pub fn flags(&self) -> u32 {
        self.flags
    }

    /// The entries `vcpuid` is programmed with.
    pub fn entries_for(&self, vcpuid: u32) -> Vec<vcpu_cpuid_entry> {
        let mut entries = self.entries.clone();
        specialize_for_vcpu(&mut entries, vcpuid);
        entries
    }

    /// Program the table into one vCPU.
    ///
    /// `vm_set_cpuid` in `vmm_cpuid.c` does not need the vCPU to be
    /// active, so a hot-add can program the table before the activation
    /// it cannot undo.
    pub fn apply_to(&self, hdl: &VmmHdl, vcpuid: i32) -> std::io::Result<()> {
        let id = u32::try_from(vcpuid).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("vCPU id {vcpuid} is negative"),
            )
        })?;
        let mut entries = self.entries_for(id);
        hdl.set_cpuid(vcpuid, &mut entries, self.flags)
    }
}

/// Build the table and program every boot vCPU with it, handing back
/// the table for the vCPUs that come later.
pub fn apply_cpuid_table(
    hdl: &VmmHdl,
    num_cpus: u32,
    baseline: CpuBaseline,
    extra: &[vcpu_cpuid_entry],
    log: &slog::Logger,
) -> std::io::Result<Option<CpuidTable>> {
    let Some(table) = CpuidTable::build(baseline, extra, num_cpus) else {
        return Ok(None);
    };

    slog::info!(log, "applying CPUID";
        "baseline" => ?baseline,
        "entries" => table.len(),
        "extra" => extra.len(),
        "vcpus" => num_cpus,
    );

    let last = i32::try_from(num_cpus).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{num_cpus} vCPUs does not fit the bhyve vCPU id"),
        )
    })?;
    for vcpuid in 0..last {
        table.apply_to(hdl, vcpuid)?;
    }

    Ok(Some(table))
}

/// The host's key feature bits, as a guest on `baseline` sees them.
///
/// Returns (leaf1_ecx, leaf1_edx, leaf7_ebx, leaf7_ecx, leaf7_edx,
/// xcr0). This is what a migration preamble compares.
pub fn query_masked_features(
    baseline: CpuBaseline,
) -> (u32, u32, u32, u32, u32, u32) {
    let (ecx1, edx1, ebx7, ecx7, edx7, xcr0) = query_host_features();
    let mask = FeatureMask::for_baseline(baseline);
    (
        ecx1 & !mask.leaf1_ecx,
        edx1,
        ebx7 & !mask.leaf7_ebx,
        ecx7 & !mask.leaf7_ecx,
        edx7 & !mask.leaf7_edx,
        xcr0 & !(mask.xcr0 as u32),
    )
}

/// The host's key feature bits: (leaf1_ecx, leaf1_edx, leaf7_ebx,
/// leaf7_ecx, leaf7_edx, xcr0).
pub fn query_host_features() -> (u32, u32, u32, u32, u32, u32) {
    let host = HostCpu;
    let (_, _, ecx1, edx1) = host.cpuid(1, 0);

    let (max_leaf, _, _, _) = host.cpuid(0, 0);
    let (ebx7, ecx7, edx7) = if max_leaf >= 7 {
        let (_, b, c, d) = host.cpuid(7, 0);
        (b, c, d)
    } else {
        (0, 0, 0)
    };

    let xcr0 = if max_leaf >= 0xD {
        host.cpuid(0xD, 0).0
    } else {
        XCR0_LEGACY as u32
    };

    (ecx1, edx1, ebx7, ecx7, edx7, xcr0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A recorded Intel host: 0xD leaves 0..=0x1F, AVX and AVX-512
    /// state, one extra subleaf-2 (AVX) at offset 576 and the AVX-512
    /// components after it.
    struct FakeHost {
        leaves: HashMap<(u32, u32), Regs>,
        enabled_xcr0: Option<u64>,
    }

    impl FakeHost {
        fn intel() -> Self {
            let mut leaves = HashMap::new();
            leaves
                .insert((0, 0), (0x1F, 0x756e_6547, 0x6c65_746e, 0x4965_6e69));
            // 16 logical CPUs on the host, APIC ID 7 on the recording CPU.
            leaves.insert(
                (1, 0),
                (0x000A_0655, 0x0710_0800, 0xFFFF_FFFF, 0xBFEB_FBFF),
            );
            leaves.insert((4, 0), (0x7C00_4121, 0, 0, 0)); // L1d: level 1
            leaves.insert((4, 1), (0x7C00_4122, 0, 0, 0)); // L1i
            leaves.insert((4, 2), (0x7C00_4143, 0, 0, 0)); // L2
            leaves.insert((4, 3), (0x7C0F_C163, 0, 0, 0)); // L3
            leaves.insert((4, 4), (0, 0, 0, 0));
            leaves.insert((7, 0), (1, 0xFFFF_FFFF, 0xFFFF_FFFF, 0xFFFF_FFFF));
            leaves.insert((7, 1), (0xFFFF_FFFF, 0, 0, 0));
            // XCR0 supported: x87, SSE, AVX, opmask, ZMM_Hi256, Hi16_ZMM,
            // PKRU (9), and AMX (17, 18).
            let xcr0: u32 = 0b11 | (1 << 2) | (7 << 5) | (1 << 9) | (3 << 17);
            leaves.insert((0xD, 0), (xcr0, 2696, 11008, 0));
            leaves.insert((0xD, 1), (0xF, 2440, 0x100, 0)); // XSS: PT (8)
            leaves.insert((0xD, 2), (256, 576, 0, 0));
            leaves.insert((0xD, 5), (64, 1088, 0, 0));
            leaves.insert((0xD, 6), (512, 1152, 0, 0));
            leaves.insert((0xD, 7), (1024, 1664, 0, 0));
            leaves.insert((0xD, 8), (128, 0, 1, 0)); // PT, supervisor
            leaves.insert((0xD, 9), (8, 2688, 0, 0));
            leaves.insert((0xD, 17), (64, 2752, 0, 0));
            leaves.insert((0xD, 18), (8192, 2816, 0, 0));
            leaves.insert((0x8000_0000, 0), (0x8000_0008, 0, 0, 0));
            leaves.insert((0x8000_0008, 0), (0x3030, 0, 0xF0FF, 0));
            Self {
                leaves,
                enabled_xcr0: None,
            }
        }

        fn amd() -> Self {
            let mut host = Self::intel();
            host.leaves
                .insert((0, 0), (0x10, 0x6874_7541, 0x444d_4163, 0x6974_6e65));
            host.leaves.insert((0x8000_0000, 0), (0x8000_001F, 0, 0, 0));
            host
        }
    }

    impl CpuidSource for FakeHost {
        fn cpuid(&self, leaf: u32, subleaf: u32) -> Regs {
            self.leaves
                .get(&(leaf, subleaf))
                .copied()
                .unwrap_or_default()
        }

        fn enabled_xcr0(&self) -> Option<u64> {
            self.enabled_xcr0
        }
    }

    fn find(t: &CpuidTable, func: u32, idx: u32) -> vcpu_cpuid_entry {
        *t.entries
            .iter()
            .find(|e| e.vce_function == func && e.vce_index == idx)
            .unwrap_or_else(|| panic!("no entry for {func:#x}.{idx}"))
    }

    fn find_for(
        t: &CpuidTable,
        vcpu: u32,
        func: u32,
        idx: u32,
    ) -> vcpu_cpuid_entry {
        t.entries_for(vcpu)
            .into_iter()
            .find(|e| e.vce_function == func && e.vce_index == idx)
            .unwrap_or_else(|| panic!("no entry for {func:#x}.{idx}"))
    }

    #[test]
    fn a_host_baseline_with_no_extra_entries_programs_nothing() {
        // The kernel answers CPUID from the host, so no vCPU needs an
        // override and a late vCPU needs no replay either.
        assert!(CpuidTable::build_from(
            &FakeHost::intel(),
            CpuBaseline::Host,
            &[],
            2
        )
        .is_none());
    }

    #[test]
    fn a_host_baseline_with_extra_entries_keeps_the_standard_leaves() {
        // VM_SET_CPUID replaces the kernel table wholesale, so an
        // extra leaf on its own would hide every standard leaf.
        let extra = entry(0x4000_0000, 0, 0, (0x4000_0001, 0, 0, 0));
        let table = CpuidTable::build_from(
            &FakeHost::intel(),
            CpuBaseline::Host,
            &[extra],
            2,
        )
        .expect("extra entries need a table");

        assert!(table.len() > 1);
        assert!(table.entries.iter().any(|e| e.vce_function == 0x4000_0000));
        assert!(table.entries.iter().any(|e| e.vce_function == 0));
    }

    #[test]
    fn every_vcpu_gets_its_own_apic_id() {
        // The explicit kernel path does not patch the IDs, and Linux
        // 6.9+ logs "APIC ID mismatch" and folds the topology to one
        // core when every CPU reports the same one.
        let table = CpuidTable::build_from(
            &FakeHost::intel(),
            CpuBaseline::Host,
            &[entry(0x4000_0000, 0, 0, (0, 0, 0, 0))],
            4,
        )
        .unwrap();

        for vcpu in 0..4 {
            let l1 = find_for(&table, vcpu, 1, 0);
            assert_eq!(l1.vce_ebx >> 24, vcpu, "leaf 1 APIC ID");
            for sub in 0..3 {
                assert_eq!(
                    find_for(&table, vcpu, 0xB, sub).vce_edx,
                    vcpu,
                    "leaf 0xB x2APIC ID"
                );
            }
        }
        // The host's own APIC ID (7) is gone from every copy.
        assert_ne!(find_for(&table, 0, 1, 0).vce_ebx >> 24, 7);
    }

    #[test]
    fn the_topology_leaves_describe_the_vm_not_the_host() {
        let host = FakeHost::intel();
        let table =
            CpuidTable::build_from(&host, CpuBaseline::Avx2, &[], 6).unwrap();

        let l1 = find(&table, 1, 0);
        assert_eq!(
            (l1.vce_ebx >> 16) & 0xFF,
            6,
            "logical processors per package"
        );
        assert_ne!(l1.vce_edx & LEAF1_EDX_HTT, 0);
        assert_ne!(l1.vce_ecx & LEAF1_ECX_HYPERVISOR, 0);
        assert_eq!(
            l1.vce_ecx
                & (LEAF1_ECX_VMX | LEAF1_ECX_MONITOR | LEAF1_ECX_TSC_DEADLINE),
            0
        );

        // Leaf 0xB: no SMT, six cores in the package, three ID bits.
        let b0 = find(&table, 0xB, 0);
        assert_eq!(
            (b0.vce_eax, b0.vce_ebx, b0.vce_ecx >> 8),
            (0, 1, LEAF_B_TYPE_SMT)
        );
        let b1 = find(&table, 0xB, 1);
        assert_eq!(
            (b1.vce_eax, b1.vce_ebx, b1.vce_ecx >> 8),
            (3, 6, LEAF_B_TYPE_CORE)
        );
        let b2 = find(&table, 0xB, 2);
        assert_eq!((b2.vce_eax, b2.vce_ebx, b2.vce_ecx), (0, 0, 2));

        // Leaf 4: L1/L2 private to a core, L3 shared by all six.
        let l1d = find(&table, 4, 0);
        assert_eq!((l1d.vce_eax >> 14) & 0xFFF, 0);
        assert_eq!(l1d.vce_eax >> 26, 5);
        let l3 = find(&table, 4, 3);
        assert_eq!((l3.vce_eax >> 14) & 0xFFF, 5);

        // 0x8000_0008: five more cores, three ID bits.
        let ext8 = find(&table, 0x8000_0008, 0);
        assert_eq!(ext8.vce_ecx & 0xFF, 5);
        assert_eq!((ext8.vce_ecx >> 12) & 0xF, 3);
    }

    #[test]
    fn an_amd_host_gets_a_per_vcpu_leaf_8000_001e() {
        let table =
            CpuidTable::build_from(&FakeHost::amd(), CpuBaseline::Avx2, &[], 2)
                .unwrap();
        assert_eq!(table.flags(), 0);
        let e = find_for(&table, 1, 0x8000_001E, 0);
        assert_eq!((e.vce_eax, e.vce_ebx & 0xFF), (1, 1));
        assert!(
            table.entries.iter().all(|e| e.vce_function != 2),
            "leaf 2 is Intel only"
        );
    }

    #[test]
    fn leaf_d_lists_every_component_the_guest_may_enable() {
        // Linux reads the size of every enabled component. A zero size
        // fails paranoid_xstate_size_valid and XSAVE is disabled.
        let table = CpuidTable::build_from(
            &FakeHost::intel(),
            CpuBaseline::Host,
            &[entry(0x4000_0000, 0, 0, (0, 0, 0, 0))],
            2,
        )
        .unwrap();

        let d0 = find(&table, 0xD, 0);
        let xcr0 = u64::from(d0.vce_eax) | u64::from(d0.vce_edx) << 32;
        for bit in [2u32, 5, 6, 7, 9, 17, 18] {
            assert_ne!(xcr0 & (1 << bit), 0, "component {bit} advertised");
            let sub = find(&table, 0xD, bit);
            assert_ne!(sub.vce_eax, 0, "component {bit} has a size");
        }
        // The supervisor component from XSS is described too.
        assert_eq!(find(&table, 0xD, 8).vce_ecx & 1, 1);
        // The maximum is the end of the highest user component, AMX
        // tile data at 2816 + 8192.
        assert_eq!(d0.vce_ecx, 2816 + 8192);
    }

    #[test]
    fn a_masked_baseline_drops_the_components_with_the_features() {
        let table = CpuidTable::build_from(
            &FakeHost::intel(),
            CpuBaseline::Avx2,
            &[],
            2,
        )
        .unwrap();

        let d0 = find(&table, 0xD, 0);
        assert_eq!(d0.vce_eax & 0xE0, 0, "AVX-512 state is gone");
        assert_eq!(d0.vce_eax & (3 << 17), 0, "AMX state is gone");
        assert_ne!(d0.vce_eax & (1 << 2), 0, "AVX state stays");
        assert_ne!(d0.vce_eax & (1 << 9), 0, "PKRU stays");
        assert!(table.entries.iter().all(
            |e| !(e.vce_function == 0xD && (5..=7).contains(&e.vce_index))
        ));
        // PKRU ends at 2688 + 8. The masked AVX-512 components do not
        // count.
        assert_eq!(d0.vce_ecx, 2696);

        let l7 = find(&table, 7, 0);
        assert_eq!(l7.vce_ebx & LEAF7_EBX_AVX512, 0);
        assert_ne!(l7.vce_ebx & LEAF7_EBX_AVX2, 0);
        assert_eq!(find(&table, 7, 1).vce_eax & LEAF7_1_EAX_VECTOR, 0);

        let sse = CpuidTable::build_from(
            &FakeHost::intel(),
            CpuBaseline::Sse42,
            &[],
            2,
        )
        .unwrap();
        let d0 = find(&sse, 0xD, 0);
        assert_eq!(d0.vce_eax & (1 << 2), 0, "AVX state is gone");
        assert_eq!(d0.vce_ecx, 2696);
        assert_eq!(find(&sse, 1, 0).vce_ecx & LEAF1_ECX_AVX, 0);
        assert_eq!(find(&sse, 7, 0).vce_ebx & LEAF7_EBX_AVX2, 0);
    }

    #[test]
    fn the_guest_gets_no_xsave_state_the_host_did_not_enable() {
        // The kernel only allows what is in the host's own XCR0.
        let mut host = FakeHost::intel();
        host.enabled_xcr0 = Some(0b111);
        let table = CpuidTable::build_from(
            &host,
            CpuBaseline::Host,
            &[entry(0x4000_0000, 0, 0, (0, 0, 0, 0))],
            2,
        )
        .unwrap();
        let d0 = find(&table, 0xD, 0);
        assert_eq!(d0.vce_eax, 0b111);
        assert_eq!(d0.vce_ecx, 576 + 256);
    }

    #[test]
    fn the_pmu_and_crystal_leaves_stay_hidden() {
        // The kernel zeroes 0xA and 0x15 so a guest cannot derive the
        // local APIC frequency. The table leaves them to the empty entry.
        let table = CpuidTable::build_from(
            &FakeHost::intel(),
            CpuBaseline::Avx2,
            &[],
            2,
        )
        .unwrap();
        assert!(table
            .entries
            .iter()
            .all(|e| e.vce_function != 0xA && e.vce_function != 0x15));
    }

    #[test]
    fn entries_stay_sorted_after_extra_entries_are_folded_in() {
        // The kernel needs the table in eval order.
        let extra = entry(0x4000_0000, 0, 0, (1, 0, 0, 0));
        let table = CpuidTable::build_from(
            &FakeHost::intel(),
            CpuBaseline::Avx2,
            &[extra],
            2,
        )
        .expect("a masked baseline always programs a table");

        let mut sorted = table.entries.clone();
        sorted.sort_by(vcpu_cpuid_entry::eval_sort);
        let keys = |v: &[vcpu_cpuid_entry]| {
            v.iter()
                .map(|e| (e.vce_function, e.vce_index))
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(&table.entries), keys(&sorted));
    }

    #[test]
    fn id_width_counts_the_bits_an_id_needs() {
        assert_eq!(id_width(1), 0);
        assert_eq!(id_width(2), 1);
        assert_eq!(id_width(3), 2);
        assert_eq!(id_width(64), 6);
    }
}
