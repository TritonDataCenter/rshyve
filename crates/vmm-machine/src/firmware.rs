// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! fw_cfg, ACPI, and SMBIOS table emission.

use std::sync::Arc;

use slog::{info, warn, Logger};
use vmm_core::machine::Machine;
use vmm_devices::fwcfg::FwCfg;
use vmm_devices::{acpi, smbios};

use crate::opts::VmOpts;

/// Linux scans 0xF0000-0xFFFFF for the "_SM_" entry point signature.
const SMBIOS_EP_GPA: u64 = 0xF0000;
const SMBIOS_TABLE_GPA: u64 = 0xF1000;

/// Proof that the ACPI tables are already in guest memory.
///
/// Only `setup_fwcfg_and_acpi` constructs one. A direct-boot or PVH
/// kernel loader takes it, so the ACPI-before-kernel ordering that
/// `run()` relies on becomes a compile error to reverse instead of a
/// comment.
#[derive(Debug, Clone, Copy)]
pub struct AcpiTables {
    rsdp_addr: u64,
}

impl AcpiTables {
    /// The one construction path. `write_legacy_acpi_tables` puts the
    /// tables at bhyve's fixed base, so the token names that address.
    fn at_bhyve_base() -> Self {
        Self {
            rsdp_addr: acpi::BHYVE_ACPI_BASE,
        }
    }

    /// Guest physical address of the RSDP, for the kernel loader's
    /// `acpi_rsdp_addr` argument.
    pub fn rsdp_addr(&self) -> u64 {
        self.rsdp_addr
    }
}

/// The fw_cfg NB_CPUS and MAX_CPUS words.
///
/// OVMF sizes its per-CPU structures from MAX_CPUS, so that word follows
/// the MADT slot count and not the boot CPU count.
fn fw_cfg_cpu_counts(cfg: &acpi::AcpiConfig) -> (Vec<u8>, Vec<u8>) {
    (
        (cfg.num_cpus as u16).to_le_bytes().to_vec(),
        (cfg.max_cpus as u16).to_le_bytes().to_vec(),
    )
}

/// Set up the fw_cfg device with the E820 memory map, ACPI tables (both
/// table-loader protocol and legacy GPA), SMBIOS tables, and boot order.
///
/// `bootorder` arrives already encoded, so this crate never parses `-s`
/// device specifications.
pub fn setup_fwcfg_and_acpi(
    machine: &Machine,
    opts: &VmOpts,
    num_cpus: u32,
    mem_size: usize,
    bootorder: Vec<u8>,
    tpm_acpi: Option<acpi::TpmAcpi>,
    log: &Logger,
) -> anyhow::Result<(Arc<FwCfg>, AcpiTables)> {
    // A CPU ceiling the kernel cannot honour must stop the boot here,
    // not reach the guest as a MADT full of slots it cannot use.
    // The FADT GPE0_BLK field, the DSDT `\_GPE` methods and the I/O
    // port claims in `pm::setup_timers_and_pm` must agree, so all of
    // them read `opts.hotplug`.
    let acpi_cfg = acpi::AcpiConfig::new(num_cpus, opts.max_cpus)?
        .with_hotplug(opts.hotplug);

    let fwcfg = FwCfg::new();
    fwcfg.attach(machine.bus_pio());
    info!(log, "fw_cfg attached"; "selector_port" => "0x510", "data_port" => "0x511");

    // OVMF reads these for SMP initialization.
    let (nb_cpus, max_cpus) = fw_cfg_cpu_counts(&acpi_cfg);
    fwcfg.insert_legacy(0x05, nb_cpus); // NB_CPUS
    fwcfg.insert_legacy(0x0F, max_cpus); // MAX_CPUS

    let e820_data = acpi::build_e820(machine.mem_size());
    fwcfg.insert_named("etc/e820", e820_data);
    info!(log, "E820 memory map added to fw_cfg";
        "mem_size" => machine.mem_size(),
    );

    // The layout produces two separate blobs (tables + rsdp) with
    // blob-relative offsets that the UEFI firmware patches through the
    // etc/table-loader commands. With --vtpm on, the TPM2 ACPI table is
    // appended so OVMF surfaces the device to the guest.
    let acpi_layout = acpi::generate_acpi_layout_for_config(
        &acpi_cfg,
        None,
        tpm_acpi.clone(),
    );
    info!(log, "ACPI layout generated for table-loader protocol";
        "tables_size" => acpi_layout.tables.len(),
        "rsdp_size" => acpi_layout.rsdp.len(),
        "num_cpus" => num_cpus,
        "max_cpus" => acpi_cfg.max_cpus,
        "with_tpm2" => tpm_acpi.is_some(),
    );

    fwcfg.insert_named("etc/acpi/tables", acpi_layout.tables.clone());
    info!(log, "ACPI fw_cfg entry added"; "name" => "etc/acpi/tables",
        "size" => acpi_layout.tables.len());
    fwcfg.insert_named("etc/acpi/rsdp", acpi_layout.rsdp.clone());
    info!(log, "ACPI fw_cfg entry added"; "name" => "etc/acpi/rsdp",
        "size" => acpi_layout.rsdp.len());

    let table_loader = acpi::generate_table_loader(&acpi_layout);
    let loader_cmds = table_loader.len() / 128;
    fwcfg.insert_named("etc/table-loader", table_loader);
    info!(log, "ACPI table-loader registered";
        "commands" => loader_cmds,
    );

    fwcfg.insert_named("bootorder", bootorder);
    info!(log, "boot order configured via fw_cfg");

    // Threading the same TPM description here keeps the BIOS-scan and
    // UEFI views in sync.
    write_legacy_acpi_tables(machine, &acpi_cfg, tpm_acpi, log);

    write_smbios_tables(machine, &fwcfg, opts, num_cpus, mem_size, log)?;

    Ok((fwcfg, AcpiTables::at_bhyve_base()))
}

/// Generate legacy GPA-based ACPI tables and place their RSDP in the BIOS
/// scan area for compatibility with both UEFI and BIOS discovery paths.
pub fn write_legacy_acpi_tables(
    machine: &Machine,
    acpi_cfg: &acpi::AcpiConfig,
    tpm_acpi: Option<acpi::TpmAcpi>,
    log: &Logger,
) {
    let acpi_tables = acpi::generate_acpi_tables_for_config(acpi_cfg, tpm_acpi);

    // A second RSDP at 0xE0000 is easier for a scanning guest to find.
    let rsdp_size = 36;
    if acpi_tables.data.len() >= rsdp_size {
        if let Err(e) = machine
            .memctx()
            .write(0xE0000, &acpi_tables.data[..rsdp_size])
        {
            warn!(log, "failed to write RSDP at 0xE0000"; "error" => %e);
        } else {
            info!(log, "RSDP written at 0xE0000");
        }
    }

    // The full tables go to the bhyve legacy base for non-UEFI and
    // direct-mapped scenarios.
    if let Err(e) = machine
        .memctx()
        .write(acpi::BHYVE_ACPI_BASE, &acpi_tables.data)
    {
        warn!(log, "failed to write ACPI tables to guest memory (may be OK if region not mapped)";
            "gpa" => format!("{:#x}", acpi::BHYVE_ACPI_BASE),
            "error" => %e,
        );
    } else {
        info!(log, "ACPI tables written to guest memory";
            "gpa" => format!("{:#x}", acpi::BHYVE_ACPI_BASE),
            "size" => acpi_tables.data.len(),
        );
    }
}

/// Generate SMBIOS tables, add them to fw_cfg, and write them to the
/// legacy scan area in guest memory.
pub fn write_smbios_tables(
    machine: &Machine,
    fwcfg: &FwCfg,
    opts: &VmOpts,
    num_cpus: u32,
    mem_size: usize,
    log: &Logger,
) -> anyhow::Result<()> {
    let smbios_config = if let Some(ref b_flag) = opts.smbios {
        let mut cfg = smbios::parse_smbios_flag(b_flag)?;
        cfg.vm_name = opts.vm_name.clone();
        cfg.uuid = opts.uuid.clone();
        cfg.num_cpus = num_cpus;
        cfg.memory_mb = (mem_size / (1024 * 1024)) as u64;
        cfg
    } else {
        smbios::SmbiosConfig {
            vm_name: opts.vm_name.clone(),
            uuid: opts.uuid.clone(),
            num_cpus,
            memory_mb: (mem_size / (1024 * 1024)) as u64,
            manufacturer: None,
            product: None,
            version: None,
            serial: None,
            sku: None,
            family: None,
        }
    };
    // The structure table has to stay clear of the legacy ACPI tables
    // the loader writes right above it.
    let window = (acpi::BHYVE_ACPI_BASE - SMBIOS_TABLE_GPA) as usize;
    let (smbios_anchor, smbios_tables) =
        smbios::generate_smbios(&smbios_config, window)?;
    fwcfg.insert_named("etc/smbios/smbios-anchor", smbios_anchor.clone());
    fwcfg.insert_named("etc/smbios/smbios-tables", smbios_tables.clone());
    info!(log, "SMBIOS tables added to fw_cfg";
        "anchor_size" => smbios_anchor.len(),
        "tables_size" => smbios_tables.len(),
    );

    // Write the tables first, then the entry point that points at them.
    if let Err(e) = machine.memctx().write(SMBIOS_TABLE_GPA, &smbios_tables) {
        warn!(log, "failed to write SMBIOS tables to guest memory"; "error" => %e);
    }

    let mut ep = smbios_anchor;
    if ep.len() >= 0x1F {
        // 0x18: structure table address (4 bytes, little-endian)
        ep[0x18..0x1C]
            .copy_from_slice(&(SMBIOS_TABLE_GPA as u32).to_le_bytes());

        // Recompute the intermediate checksum (bytes 0x10..0x1F)
        ep[0x15] = 0;
        let sum: u8 =
            ep[0x10..0x1F].iter().fold(0u8, |a, &b| a.wrapping_add(b));
        ep[0x15] = 0u8.wrapping_sub(sum);

        // Recompute the entry point checksum (bytes 0x00..0x10)
        ep[0x04] = 0;
        let sum: u8 =
            ep[0x00..0x10].iter().fold(0u8, |a, &b| a.wrapping_add(b));
        ep[0x04] = 0u8.wrapping_sub(sum);
    }

    if let Err(e) = machine.memctx().write(SMBIOS_EP_GPA, &ep) {
        warn!(log, "failed to write SMBIOS entry point to guest memory"; "error" => %e);
    } else {
        info!(log, "SMBIOS written to guest memory";
            "entry_point" => format!("{:#x}", SMBIOS_EP_GPA),
            "tables" => format!("{:#x}", SMBIOS_TABLE_GPA),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use vmm_devices::acpi::BHYVE_ACPI_BASE;

    use super::{
        fw_cfg_cpu_counts, AcpiTables, SMBIOS_EP_GPA, SMBIOS_TABLE_GPA,
    };

    #[test]
    fn acpi_tables_token_names_the_bhyve_rsdp_base() {
        // The token is the kernel loader's proof that the tables are
        // already in guest memory, so it must carry the same address
        // write_legacy_acpi_tables placed them at.
        assert_eq!(AcpiTables::at_bhyve_base().rsdp_addr(), BHYVE_ACPI_BASE);
    }

    #[test]
    fn fw_cfg_max_cpus_follows_the_madt_slot_count() {
        use vmm_devices::acpi::AcpiConfig;

        let cfg = AcpiConfig::new(2, 8).expect("2 boot CPUs of 8 slots");
        let (nb_cpus, max_cpus) = fw_cfg_cpu_counts(&cfg);
        assert_eq!(nb_cpus, 2u16.to_le_bytes());
        assert_eq!(max_cpus, 8u16.to_le_bytes());

        // A VM with no spare slots gets two equal words.
        let (nb_cpus, max_cpus) = fw_cfg_cpu_counts(&AcpiConfig::boot_only(4));
        assert_eq!(nb_cpus, 4u16.to_le_bytes());
        assert_eq!(nb_cpus, max_cpus);
    }

    #[test]
    fn smbios_entry_point_precedes_its_tables() {
        // Linux scans 0xF0000..0xFFFFF for "_SM_". The entry point must
        // sit below the table blob it points at, in the same window.
        assert!(SMBIOS_EP_GPA < SMBIOS_TABLE_GPA);
        assert!(SMBIOS_TABLE_GPA < 0x10_0000);
    }
}
