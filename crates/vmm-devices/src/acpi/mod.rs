// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ACPI table generation for bhyve-based VMs.
//!
//! Generates the minimum set of ACPI tables required to boot a UEFI
//! guest: RSDP, RSDT, XSDT, FADT, FACS, DSDT, MADT, HPET, SPCR, and an
//! optional TPM2.
//!
//! Tables are laid out contiguously in guest physical memory starting
//! at [`BHYVE_ACPI_BASE`] (0xF2400). The UEFI firmware discovers them
//! via the RSDP, or via fw_cfg file entries.
//!
//! # E820 memory map
//!
//! The [`build_e820`] function generates an E820 table suitable for
//! inclusion in the fw_cfg device as "etc/e820".

mod config;
mod dsdt;
mod e820;
mod facs;
mod fadt;
mod hotplug;
mod loader;
mod madt;
mod rsdp;
mod tables;
#[cfg(test)]
mod test_support;

pub use config::{AcpiConfig, AcpiConfigError, TpmAcpi, TpmDevice};
pub use e820::{
    build_e820, build_e820_entries, E820Entry, E820_TYPE_ACPI, E820_TYPE_NVS,
    E820_TYPE_RAM, E820_TYPE_RESERVED,
};
pub use loader::generate_table_loader;

use dsdt::build_dsdt;
use facs::build_facs;
use fadt::build_fadt;
use madt::build_madt;
use rsdp::{build_rsdp, build_rsdt, build_xsdt, RSDP_SIZE};
use tables::{build_hpet, build_spcr};

/// Guest physical address where ACPI tables are placed.
pub const BHYVE_ACPI_BASE: u64 = 0xF2400;

// ── Well-known hardware addresses ─────────────────────────────────

const LAPIC_ADDR: u32 = 0xFEE0_0000;
const IOAPIC_ADDR: u32 = 0xFEC0_0000;
const HPET_ADDR: u32 = 0xFED0_0000;

// ── ACPI PM I/O ports (must match BhyvePmTimer / kernel config) ───

const PMBASE: u32 = crate::bhyve::pmtimer::PMBASE_DEFAULT as u32;
const PM1A_EVT_ADDR: u32 = PMBASE;
const PM1A_CNT_ADDR: u32 = PMBASE + 0x04;
/// Must match the port given to `VM_PMTMR_LOCATE`.
const IO_PMTMR: u32 = PMBASE + 0x08;

const SCI_INT: u16 = 9;

const RESET_REG_PORT: u64 = 0x0CF9;

// ── ACPI table header ─────────────────────────────────────────────

const ACPI_HDR_SIZE: usize = 36;

const OEM_ID: &[u8; 6] = b"BHYVE ";
const OEM_TABLE_ID: &[u8; 8] = b"BVXSDT  ";
const CREATOR_ID: &[u8; 4] = b"BHYV";

/// Write an ACPI SDT header into a buffer.
///
/// The checksum is 0. Call [`fix_checksum`] after the full table is
/// written.
fn write_header(
    buf: &mut Vec<u8>,
    signature: &[u8; 4],
    length: u32,
    revision: u8,
) {
    let start = buf.len();
    buf.extend_from_slice(signature); // 0: signature
    buf.extend_from_slice(&length.to_le_bytes()); // 4: length
    buf.push(revision); // 8: revision
    buf.push(0); // 9: checksum (filled later)
    buf.extend_from_slice(OEM_ID); // 10: oem_id
    buf.extend_from_slice(OEM_TABLE_ID); // 16: oem_table_id
    buf.extend_from_slice(&1u32.to_le_bytes()); // 24: oem_revision
    buf.extend_from_slice(CREATOR_ID); // 28: creator_id
    buf.extend_from_slice(&1u32.to_le_bytes()); // 32: creator_revision
    debug_assert_eq!(buf.len() - start, ACPI_HDR_SIZE);
}

/// Fix the checksum byte so that the sum of all bytes in
/// `buf[start..start+len]` is 0 mod 256.
fn fix_checksum(buf: &mut [u8], start: usize, len: usize) {
    let sum: u8 = buf[start..start + len]
        .iter()
        .fold(0u8, |acc, &b| acc.wrapping_add(b));
    // An SDT header holds its checksum at offset 9.
    buf[start + 9] = 0u8.wrapping_sub(sum);
}

/// Like [`fix_checksum`], for a checksum that is not at offset 9.
fn fix_checksum_at(
    buf: &mut [u8],
    cksum_offset: usize,
    start: usize,
    len: usize,
) {
    buf[cksum_offset] = 0;
    let sum: u8 = buf[start..start + len]
        .iter()
        .fold(0u8, |acc, &b| acc.wrapping_add(b));
    buf[cksum_offset] = 0u8.wrapping_sub(sum);
}

// ── Generic Address Structure (GAS) ──────────────────────────────

/// Write an ACPI Generic Address Structure (12 bytes).
fn write_gas(
    buf: &mut Vec<u8>,
    address_space: u8,
    bit_width: u8,
    bit_offset: u8,
    access_size: u8,
    address: u64,
) {
    buf.push(address_space);
    buf.push(bit_width);
    buf.push(bit_offset);
    buf.push(access_size);
    buf.extend_from_slice(&address.to_le_bytes());
}

fn write_gas_zero(buf: &mut Vec<u8>) {
    buf.extend_from_slice(&[0u8; 12]);
}

// GAS address space IDs
const ACPI_AS_SYSTEM_IO: u8 = 1;
// Access width constants
const ACPI_GAS_BYTE: u8 = 1;
const ACPI_GAS_WORD: u8 = 2;
const ACPI_GAS_DWORD: u8 = 3;
#[allow(dead_code)]
const ACPI_GAS_UNDEF: u8 = 0;

/// Result of ACPI table generation (legacy: GPA-based pointers).
pub struct AcpiTables {
    /// All tables concatenated, to be written at BHYVE_ACPI_BASE.
    pub data: Vec<u8>,
    /// Individual table entries for fw_cfg (name, data).
    pub fwcfg_entries: Vec<(&'static str, Vec<u8>)>,
}

/// Generate all ACPI tables for the VM (legacy GPA-based layout).
///
/// Tables are laid out contiguously starting at `BHYVE_ACPI_BASE`.
/// Returns the concatenated blob and individual table data for
/// fw_cfg registration.
pub fn generate_acpi_tables(num_cpus: u32) -> AcpiTables {
    generate_acpi_tables_inner(&AcpiConfig::boot_only(num_cpus), None)
}

/// Variant of `generate_acpi_tables` that includes a TPM2 ACPI table and
/// its matching DSDT device node. Used when the VM is booted with `--vtpm`.
pub fn generate_acpi_tables_with_tpm2(
    num_cpus: u32,
    tpm: Option<TpmAcpi>,
) -> AcpiTables {
    generate_acpi_tables_for_config(&AcpiConfig::boot_only(num_cpus), tpm)
}

/// Variant of `generate_acpi_tables` driven by an [`AcpiConfig`], so the
/// MADT can advertise more CPU slots than the VM boots with.
pub fn generate_acpi_tables_for_config(
    cfg: &AcpiConfig,
    tpm: Option<TpmAcpi>,
) -> AcpiTables {
    let tpm_device = tpm.as_ref().map(|t| t.device);
    let tpm2_table = tpm.map(|t| t.table);
    generate_acpi_tables_inner(&cfg.clone().with_tpm(tpm_device), tpm2_table)
}

fn generate_acpi_tables_inner(
    cfg: &AcpiConfig,
    tpm2_table: Option<Vec<u8>>,
) -> AcpiTables {
    let base = BHYVE_ACPI_BASE;
    let mut blob = Vec::new();

    // Tables are built in dependency order: FADT needs the FACS and
    // DSDT GPAs, and RSDT/XSDT need the GPAs of the tables they list.
    //
    // Layout order in memory:
    //   RSDP | FACS | DSDT | FADT | MADT | HPET | SPCR | [TPM2] | RSDT | XSDT

    // The RSDP slot is filled last.
    let rsdp_offset = blob.len();
    blob.extend_from_slice(&[0u8; RSDP_SIZE]);
    let rsdp_gpa = base + rsdp_offset as u64;

    let facs_offset = blob.len();
    let facs = build_facs();
    blob.extend_from_slice(&facs);
    let facs_gpa = base + facs_offset as u64;

    let dsdt_offset = blob.len();
    let dsdt = build_dsdt(cfg);
    blob.extend_from_slice(&dsdt);
    let dsdt_gpa = base + dsdt_offset as u64;

    let fadt_offset = blob.len();
    let fadt = build_fadt(facs_gpa as u32, dsdt_gpa as u32, cfg);
    blob.extend_from_slice(&fadt);
    let fadt_gpa = base + fadt_offset as u64;

    let madt_offset = blob.len();
    let madt = build_madt(cfg);
    blob.extend_from_slice(&madt);
    let madt_gpa = base + madt_offset as u64;

    let hpet_offset = blob.len();
    let hpet = build_hpet();
    blob.extend_from_slice(&hpet);
    let hpet_gpa = base + hpet_offset as u64;

    // SPCR (serial console redirection to COM1)
    let spcr_offset = blob.len();
    let spcr = build_spcr();
    blob.extend_from_slice(&spcr);
    let spcr_gpa = base + spcr_offset as u64;

    // TPM2 goes before RSDT/XSDT so that its GPA is known when the
    // entry lists are built.
    let tpm2_gpa: Option<u64> = tpm2_table.as_ref().map(|bytes| {
        let off = blob.len();
        blob.extend_from_slice(bytes);
        base + off as u64
    });

    let mut rsdt_entries: Vec<u32> = vec![
        fadt_gpa as u32,
        madt_gpa as u32,
        hpet_gpa as u32,
        spcr_gpa as u32,
    ];
    let mut xsdt_entries: Vec<u64> =
        vec![fadt_gpa, madt_gpa, hpet_gpa, spcr_gpa];
    if let Some(g) = tpm2_gpa {
        rsdt_entries.push(g as u32);
        xsdt_entries.push(g);
    }

    let rsdt_offset = blob.len();
    let rsdt = build_rsdt(&rsdt_entries);
    blob.extend_from_slice(&rsdt);
    let rsdt_gpa = base + rsdt_offset as u64;

    let xsdt_offset = blob.len();
    let xsdt = build_xsdt(&xsdt_entries);
    blob.extend_from_slice(&xsdt);
    let xsdt_gpa = base + xsdt_offset as u64;

    let rsdp = build_rsdp(rsdt_gpa as u32, xsdt_gpa);
    blob[rsdp_offset..rsdp_offset + RSDP_SIZE].copy_from_slice(&rsdp);

    let fwcfg_entries =
        vec![("etc/acpi/rsdp", rsdp), ("etc/acpi/tables", blob.clone())];

    let _ = rsdp_gpa; // used only to place in memory

    AcpiTables {
        data: blob,
        fwcfg_entries,
    }
}

/// Layout information for ACPI tables in the fw_cfg blobs.
///
/// `tables` holds every table except the RSDP, concatenated. `rsdp`
/// is a separate 36-byte blob. Pointer fields hold offsets into
/// `tables`, not guest physical addresses. Each `*_offset` field is a
/// byte offset into `tables` and each `*_size` field is that table's
/// length.
pub struct AcpiLayout {
    pub tables: Vec<u8>,
    pub rsdp: Vec<u8>,
    pub facs_offset: usize,
    pub facs_size: usize,
    pub dsdt_offset: usize,
    pub dsdt_size: usize,
    pub fadt_offset: usize,
    pub fadt_size: usize,
    pub madt_offset: usize,
    pub madt_size: usize,
    pub hpet_offset: usize,
    pub hpet_size: usize,
    pub rsdt_offset: usize,
    pub rsdt_size: usize,
    pub spcr_offset: usize,
    pub spcr_size: usize,
    /// Zero when no TPM is configured. Use `tpm2_size` to test for the
    /// TPM2 table.
    pub tpm2_offset: usize,
    /// Zero when the TPM2 table is not present.
    pub tpm2_size: usize,
    pub xsdt_offset: usize,
    pub xsdt_size: usize,
}

/// Generate ACPI tables with blob-relative offsets for the table
/// loader protocol.
///
/// Unlike [`generate_acpi_tables`], this produces two separate blobs:
/// - `tables`: all tables except RSDP, with pointer fields set to
///   blob-relative offsets (the firmware adds the allocated address)
/// - `rsdp`: the RSDP structure, with rsdt_addr/xsdt_addr set to
///   the offset of RSDT/XSDT within the tables blob
///
/// Checksums are 0. The firmware computes them with ADD_CHECKSUM
/// commands.
pub fn generate_acpi_layout(num_cpus: u32) -> AcpiLayout {
    generate_acpi_layout_inner(&AcpiConfig::boot_only(num_cpus), None, None)
}

/// Generate ACPI layout with an optional TPM2 ACPI table and matching DSDT
/// device node. The TPM2 table goes between SPCR and RSDT, and the RSDT
/// and XSDT list it.
pub fn generate_acpi_layout_with_tpm2(
    num_cpus: u32,
    dsdt_override: Option<Vec<u8>>,
    tpm: Option<TpmAcpi>,
) -> AcpiLayout {
    generate_acpi_layout_for_config(
        &AcpiConfig::boot_only(num_cpus),
        dsdt_override,
        tpm,
    )
}

/// Variant of `generate_acpi_layout` driven by an [`AcpiConfig`], so the
/// MADT can advertise more CPU slots than the VM boots with.
///
/// The TPM2 table and the DSDT node come from one [`TpmAcpi`], so the
/// device in `cfg` is replaced by the one that owns the table.
pub fn generate_acpi_layout_for_config(
    cfg: &AcpiConfig,
    dsdt_override: Option<Vec<u8>>,
    tpm: Option<TpmAcpi>,
) -> AcpiLayout {
    let tpm_device = tpm.as_ref().map(|t| t.device);
    let tpm2_table = tpm.map(|t| t.table);
    generate_acpi_layout_inner(
        &cfg.clone().with_tpm(tpm_device),
        dsdt_override,
        tpm2_table,
    )
}

fn generate_acpi_layout_inner(
    cfg: &AcpiConfig,
    dsdt_override: Option<Vec<u8>>,
    tpm2_table: Option<Vec<u8>>,
) -> AcpiLayout {
    assert!(
        dsdt_override.is_none() || cfg.tpm.is_none(),
        "DSDT override cannot be combined with a vTPM",
    );
    let mut tables = Vec::new();

    // Layout order in the tables blob:
    //   FACS | DSDT | FADT | MADT | HPET | SPCR | [TPM2] | RSDT | XSDT
    //
    // Pointer fields use blob-relative offsets (not GPAs). Every table
    // checksum is 0 because the loader computes it. The FACS has no
    // standard checksum.

    let facs_offset = tables.len();
    let facs = build_facs();
    let facs_size = facs.len();
    tables.extend_from_slice(&facs);

    let dsdt_offset = tables.len();
    let dsdt = dsdt_override.unwrap_or_else(|| build_dsdt(cfg));
    let dsdt_size = dsdt.len();
    tables.extend_from_slice(&dsdt);

    // FADT pointer fields:
    //   offset 36: FIRMWARE_CTRL (u32) -> facs_offset
    //   offset 40: DSDT (u32) -> dsdt_offset
    //   offset 132: X_FIRMWARE_CTRL (u64) -> facs_offset
    //   offset 140: X_DSDT (u64) -> dsdt_offset
    let fadt_offset = tables.len();
    let fadt = build_fadt(facs_offset as u32, dsdt_offset as u32, cfg);
    let fadt_size = fadt.len();
    tables.extend_from_slice(&fadt);
    // build_fadt writes the u32 arguments into the 64-bit fields too.
    // Write them again as u64 blob offsets.
    let x_facs_off = fadt_offset + 132;
    tables[x_facs_off..x_facs_off + 8]
        .copy_from_slice(&(facs_offset as u64).to_le_bytes());
    let x_dsdt_off = fadt_offset + 140;
    tables[x_dsdt_off..x_dsdt_off + 8]
        .copy_from_slice(&(dsdt_offset as u64).to_le_bytes());
    tables[fadt_offset + 9] = 0;

    let madt_offset = tables.len();
    let madt = build_madt(cfg);
    let madt_size = madt.len();
    tables.extend_from_slice(&madt);
    tables[madt_offset + 9] = 0;

    let hpet_offset = tables.len();
    let hpet = build_hpet();
    let hpet_size = hpet.len();
    tables.extend_from_slice(&hpet);
    tables[hpet_offset + 9] = 0;

    // SPCR (serial console redirection to COM1)
    let spcr_offset = tables.len();
    let spcr = build_spcr();
    let spcr_size = spcr.len();
    tables.extend_from_slice(&spcr);
    tables[spcr_offset + 9] = 0;

    // TPM2 goes before RSDT/XSDT so that its offset is known when the
    // entry lists are built.
    let (tpm2_offset, tpm2_size) = match &tpm2_table {
        Some(bytes) => {
            let off = tables.len();
            tables.extend_from_slice(bytes);
            tables[off + 9] = 0;
            (off, bytes.len())
        }
        None => (0, 0),
    };

    // TPM2 is listed only when present. Otherwise its zero offset is a
    // dangling entry into the FACS.
    let mut rsdt_entries: Vec<u32> = vec![
        fadt_offset as u32,
        madt_offset as u32,
        hpet_offset as u32,
        spcr_offset as u32,
    ];
    let mut xsdt_entries: Vec<u64> = vec![
        fadt_offset as u64,
        madt_offset as u64,
        hpet_offset as u64,
        spcr_offset as u64,
    ];
    if tpm2_size > 0 {
        rsdt_entries.push(tpm2_offset as u32);
        xsdt_entries.push(tpm2_offset as u64);
    }

    let rsdt_offset = tables.len();
    let rsdt = build_rsdt(&rsdt_entries);
    let rsdt_size = rsdt.len();
    tables.extend_from_slice(&rsdt);
    tables[rsdt_offset + 9] = 0;

    let xsdt_offset = tables.len();
    let xsdt = build_xsdt(&xsdt_entries);
    let xsdt_size = xsdt.len();
    tables.extend_from_slice(&xsdt);
    tables[xsdt_offset + 9] = 0;

    let mut rsdp = build_rsdp(rsdt_offset as u32, xsdt_offset as u64);
    rsdp[8] = 0;
    rsdp[32] = 0;

    tables[dsdt_offset + 9] = 0;

    AcpiLayout {
        tables,
        rsdp,
        facs_offset,
        facs_size,
        dsdt_offset,
        dsdt_size,
        fadt_offset,
        fadt_size,
        madt_offset,
        madt_size,
        hpet_offset,
        hpet_size,
        spcr_offset,
        spcr_size,
        tpm2_offset,
        tpm2_size,
        rsdt_offset,
        rsdt_size,
        xsdt_offset,
        xsdt_size,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acpi::test_support::{
        contains_tpm_hid, test_tpm_acpi, tpm_crs_window, TEST_TPM_DEVICE,
    };

    #[test]
    fn layout_tpm2_and_dsdt_agree() {
        let layout =
            generate_acpi_layout_with_tpm2(2, None, Some(test_tpm_acpi()));
        assert!(layout.tpm2_size > 0);
        let dsdt = &layout.tables
            [layout.dsdt_offset..layout.dsdt_offset + layout.dsdt_size];
        assert!(contains_tpm_hid(dsdt));
        let (base, len) = tpm_crs_window(dsdt, TEST_TPM_DEVICE.crb_base);
        let control_area = u64::from_le_bytes(
            layout.tables[layout.tpm2_offset + 40..layout.tpm2_offset + 48]
                .try_into()
                .unwrap(),
        );
        assert!(control_area >= u64::from(base));
        assert!(control_area + 0x38 <= u64::from(base) + u64::from(len));

        let without_tpm = generate_acpi_layout_with_tpm2(2, None, None);
        assert_eq!(without_tpm.tpm2_size, 0);
        let dsdt = &without_tpm.tables[without_tpm.dsdt_offset
            ..without_tpm.dsdt_offset + without_tpm.dsdt_size];
        assert!(!contains_tpm_hid(dsdt));
    }

    #[test]
    fn legacy_tables_carry_tpm_node() {
        let tables = generate_acpi_tables_with_tpm2(2, Some(test_tpm_acpi()));
        assert!(tables.data.windows(4).any(|window| window == b"TPM2"));
        assert!(contains_tpm_hid(&tables.data));

        let without_tpm = generate_acpi_tables_with_tpm2(2, None);
        assert!(!without_tpm.data.windows(4).any(|window| window == b"TPM2"));
        assert!(!contains_tpm_hid(&without_tpm.data));
    }

    #[test]
    #[should_panic(expected = "DSDT override cannot be combined with a vTPM")]
    fn layout_rejects_tpm_with_dsdt_override() {
        generate_acpi_layout_with_tpm2(
            1,
            Some(build_dsdt(&AcpiConfig::boot_only(1))),
            Some(test_tpm_acpi()),
        );
    }

    #[test]
    fn acpi_config_rejects_unbootable_cpu_counts() {
        assert_eq!(
            AcpiConfig::new(0, 4).unwrap_err(),
            AcpiConfigError::NoBootCpu,
        );
        assert_eq!(
            AcpiConfig::new(4, 2).unwrap_err(),
            AcpiConfigError::MaxBelowBoot {
                num_cpus: 4,
                max_cpus: 2,
            },
        );
        let limit = vmm_core::VM_MAXCPU;
        assert_eq!(
            AcpiConfig::new(2, limit + 1).unwrap_err(),
            AcpiConfigError::AboveKernelLimit {
                max_cpus: limit + 1,
                limit,
            },
        );
        let cfg = AcpiConfig::new(2, 8).expect("2 boot CPUs of 8 slots");
        assert_eq!(cfg.num_cpus, 2);
        assert_eq!(cfg.max_cpus, 8);
        assert!(cfg.tpm.is_none());
        assert!(!cfg.hotplug, "hotplug is opt-in");
    }

    /// With max_cpus == num_cpus the config path must produce the same
    /// bytes as the bare-count path.
    #[test]
    fn config_without_hotplug_slots_matches_the_legacy_entry_points() {
        let cfg = AcpiConfig::new(4, 4).expect("4 of 4");
        let legacy = generate_acpi_layout(4);
        let from_cfg = generate_acpi_layout_for_config(&cfg, None, None);
        assert_eq!(legacy.tables, from_cfg.tables);
        assert_eq!(legacy.rsdp, from_cfg.rsdp);
        assert_eq!(legacy.madt_size, from_cfg.madt_size);

        assert_eq!(
            generate_acpi_tables(4).data,
            generate_acpi_tables_for_config(&cfg, None).data,
        );
    }

    #[test]
    fn hotplug_slots_only_grow_the_madt() {
        let boot_only = generate_acpi_layout(2);
        let with_slots = generate_acpi_layout_for_config(
            &AcpiConfig::new(2, 8).expect("2 of 8"),
            None,
            None,
        );
        // 6 spare slots at 8 bytes each.
        assert_eq!(with_slots.madt_size, boot_only.madt_size + 48);
        assert_eq!(with_slots.dsdt_size, boot_only.dsdt_size);
        assert_eq!(with_slots.fadt_size, boot_only.fadt_size);
        // Spare CPU slots alone do not turn on the GPE0 block.
        assert_eq!(
            &with_slots.tables[with_slots.fadt_offset + 80..][..4],
            [0; 4]
        );
    }

    /// Every VM ships with hotplug off, so these bytes reach real
    /// guests. A changed digest is a guest-visible change and needs a
    /// reason, not a re-blessed constant. FNV-1a is used because the
    /// standard library hasher is not stable across toolchains.
    #[test]
    fn tables_without_hotplug_are_pinned() {
        let fnv = |bytes: &[u8]| {
            bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
            })
        };
        let layout = generate_acpi_layout(2);
        let tables = (layout.tables.len(), fnv(&layout.tables));
        assert_eq!(tables, (3301, 0x875d_3c87_2388_c502));
        let rsdp = (layout.rsdp.len(), fnv(&layout.rsdp));
        assert_eq!(rsdp, (RSDP_SIZE, 0x5ee2_e132_9368_597e));
    }

    /// Hotplug adds AML to the DSDT and fills the GPE0 fields of the
    /// FADT, which `fadt` covers. It must leave every other table alone.
    #[test]
    fn hotplug_changes_only_the_dsdt_and_the_fadt() {
        let cfg = AcpiConfig::new(2, 2).expect("2 of 2");
        let plain = generate_acpi_layout_for_config(&cfg, None, None);
        let on = cfg.with_hotplug(true);
        let hotplug = generate_acpi_layout_for_config(&on, None, None);
        assert!(hotplug.dsdt_size > plain.dsdt_size);
        assert_eq!(plain.fadt_size, hotplug.fadt_size);

        let others = |l: &AcpiLayout| {
            [
                (l.facs_offset, l.facs_size),
                (l.madt_offset, l.madt_size),
                (l.hpet_offset, l.hpet_size),
                (l.spcr_offset, l.spcr_size),
            ]
            .map(|(at, size)| l.tables[at..at + size].to_vec())
        };
        assert_eq!(others(&plain), others(&hotplug));
    }

    #[test]
    fn full_table_generation() {
        let tables = generate_acpi_tables(4);
        assert!(!tables.data.is_empty());
        assert_eq!(tables.fwcfg_entries.len(), 2);
        assert_eq!(tables.fwcfg_entries[0].0, "etc/acpi/rsdp");
        assert_eq!(tables.fwcfg_entries[1].0, "etc/acpi/tables");
    }

    #[test]
    fn full_table_rsdp_valid() {
        let tables = generate_acpi_tables(2);

        let rsdp = &tables.data[..RSDP_SIZE];
        assert_eq!(&rsdp[0..8], b"RSD PTR ");

        let sum20: u8 =
            rsdp[..20].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum20, 0);
        let sum36: u8 = rsdp.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum36, 0);
    }

    #[test]
    fn layout_rsdp_separate_from_tables() {
        let layout = generate_acpi_layout(2);
        assert_eq!(layout.rsdp.len(), RSDP_SIZE);
        assert_eq!(&layout.rsdp[0..8], b"RSD PTR ");
        assert_ne!(&layout.tables[0..8], b"RSD PTR ");
        assert_eq!(&layout.tables[0..4], b"FACS");
    }

    #[test]
    fn layout_table_signatures() {
        let layout = generate_acpi_layout(2);
        let t = &layout.tables;

        assert_eq!(&t[layout.facs_offset..layout.facs_offset + 4], b"FACS");
        assert_eq!(&t[layout.dsdt_offset..layout.dsdt_offset + 4], b"DSDT");
        assert_eq!(&t[layout.fadt_offset..layout.fadt_offset + 4], b"FACP");
        assert_eq!(&t[layout.madt_offset..layout.madt_offset + 4], b"APIC");
        assert_eq!(&t[layout.hpet_offset..layout.hpet_offset + 4], b"HPET");
        assert_eq!(&t[layout.rsdt_offset..layout.rsdt_offset + 4], b"RSDT");
        assert_eq!(&t[layout.xsdt_offset..layout.xsdt_offset + 4], b"XSDT");
    }

    #[test]
    fn layout_fadt_has_blob_offsets() {
        let layout = generate_acpi_layout(1);
        let t = &layout.tables;

        // FADT.firmware_ctrl (offset 36) should be facs_offset
        let facs_ptr = u32::from_le_bytes(
            t[layout.fadt_offset + 36..layout.fadt_offset + 40]
                .try_into()
                .unwrap(),
        );
        assert_eq!(facs_ptr as usize, layout.facs_offset);

        // FADT.dsdt (offset 40) should be dsdt_offset
        let dsdt_ptr = u32::from_le_bytes(
            t[layout.fadt_offset + 40..layout.fadt_offset + 44]
                .try_into()
                .unwrap(),
        );
        assert_eq!(dsdt_ptr as usize, layout.dsdt_offset);

        // FADT.x_firmware_ctrl (offset 132) should be facs_offset
        let x_facs = u64::from_le_bytes(
            t[layout.fadt_offset + 132..layout.fadt_offset + 140]
                .try_into()
                .unwrap(),
        );
        assert_eq!(x_facs as usize, layout.facs_offset);

        // FADT.x_dsdt (offset 140) should be dsdt_offset
        let x_dsdt = u64::from_le_bytes(
            t[layout.fadt_offset + 140..layout.fadt_offset + 148]
                .try_into()
                .unwrap(),
        );
        assert_eq!(x_dsdt as usize, layout.dsdt_offset);
    }

    #[test]
    fn layout_rsdt_has_blob_offsets() {
        let layout = generate_acpi_layout(2);
        let t = &layout.tables;

        // RSDT entries at offset 36 (after header): 3 x u32
        let entry0 = u32::from_le_bytes(
            t[layout.rsdt_offset + 36..layout.rsdt_offset + 40]
                .try_into()
                .unwrap(),
        );
        let entry1 = u32::from_le_bytes(
            t[layout.rsdt_offset + 40..layout.rsdt_offset + 44]
                .try_into()
                .unwrap(),
        );
        let entry2 = u32::from_le_bytes(
            t[layout.rsdt_offset + 44..layout.rsdt_offset + 48]
                .try_into()
                .unwrap(),
        );
        assert_eq!(entry0 as usize, layout.fadt_offset);
        assert_eq!(entry1 as usize, layout.madt_offset);
        assert_eq!(entry2 as usize, layout.hpet_offset);
    }

    #[test]
    fn layout_xsdt_has_blob_offsets() {
        let layout = generate_acpi_layout(2);
        let t = &layout.tables;

        // XSDT entries at offset 36 (after header): 3 x u64
        let entry0 = u64::from_le_bytes(
            t[layout.xsdt_offset + 36..layout.xsdt_offset + 44]
                .try_into()
                .unwrap(),
        );
        let entry1 = u64::from_le_bytes(
            t[layout.xsdt_offset + 44..layout.xsdt_offset + 52]
                .try_into()
                .unwrap(),
        );
        let entry2 = u64::from_le_bytes(
            t[layout.xsdt_offset + 52..layout.xsdt_offset + 60]
                .try_into()
                .unwrap(),
        );
        assert_eq!(entry0 as usize, layout.fadt_offset);
        assert_eq!(entry1 as usize, layout.madt_offset);
        assert_eq!(entry2 as usize, layout.hpet_offset);
    }

    #[test]
    fn layout_rsdp_has_blob_offsets() {
        let layout = generate_acpi_layout(2);

        // RSDP.rsdt_addr (offset 16) should be rsdt_offset
        let rsdt_addr =
            u32::from_le_bytes(layout.rsdp[16..20].try_into().unwrap());
        assert_eq!(rsdt_addr as usize, layout.rsdt_offset);

        // RSDP.xsdt_addr (offset 24) should be xsdt_offset
        let xsdt_addr =
            u64::from_le_bytes(layout.rsdp[24..32].try_into().unwrap());
        assert_eq!(xsdt_addr as usize, layout.xsdt_offset);
    }

    #[test]
    fn layout_checksums_are_zeroed() {
        let layout = generate_acpi_layout(2);
        let t = &layout.tables;

        assert_eq!(layout.rsdp[8], 0, "RSDP v1 checksum should be 0");
        assert_eq!(layout.rsdp[32], 0, "RSDP v2 checksum should be 0");

        assert_eq!(t[layout.dsdt_offset + 9], 0, "DSDT checksum should be 0");
        assert_eq!(t[layout.fadt_offset + 9], 0, "FADT checksum should be 0");
        assert_eq!(t[layout.madt_offset + 9], 0, "MADT checksum should be 0");
        assert_eq!(t[layout.hpet_offset + 9], 0, "HPET checksum should be 0");
        assert_eq!(t[layout.rsdt_offset + 9], 0, "RSDT checksum should be 0");
        assert_eq!(t[layout.xsdt_offset + 9], 0, "XSDT checksum should be 0");
    }
}
