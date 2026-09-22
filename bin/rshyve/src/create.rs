// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VM creation: the kernel instance, guest RAM, the boot images, CPUID
//! and the firmware-adjacent devices that need mutable guest memory.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use slog::{info, o, warn, Logger};

use vmm_boot::direct;
use vmm_config::Cli;
use vmm_core::hdl::CreateOpts;
use vmm_core::machine::{Machine, MachineSetup};
use vmm_devices::acpi;
use vmm_machine::{build_machine, host_tsc_frequency_hz, MachineOpts};

use crate::{bootrom, host_state, varstore};

/// Result of `create_machine`: the finalized Machine and the metadata
/// that later stages need.
pub struct MachineCreation {
    pub machine: Machine,
    pub api_version: u32,
    pub num_cpus: u32,
    pub mem_size: usize,
    pub kernel_image: Option<vmm_boot::BootImage>,
    pub initrd_image: Option<direct::InitrdImage>,
    pub hyperv: Option<Arc<vmm_hyperv::HyperV>>,
    pub varstore: Option<Arc<varstore::VarStore>>,
    /// TPM2 table and DSDT device description when --vtpm is enabled.
    pub tpm_acpi: Option<acpi::TpmAcpi>,
    /// The CPUID table every boot vCPU was given, or `None` for a pure
    /// host-passthrough VM. A CPU brought online later must get the same
    /// table. Two feature sets on one machine cause guest faults that are
    /// very hard to diagnose.
    pub cpuid: Option<vmm_core::cpuid::CpuidTable>,
    /// The baseline of the CPUID table. Migration uses it to describe the
    /// guest's features.
    pub cpu_baseline: vmm_core::cpuid::CpuBaseline,
    pub host_local_state: host_state::HostLocalState,
}

/// Build the VM, load bootrom/kernel images, finalize memory, and apply
/// CPU topology and baseline masking.
pub fn create_machine(
    cli: &Cli,
    log: &Logger,
) -> anyhow::Result<MachineCreation> {
    host_state::validate_startup(cli)?;
    // Reject a misspelled hotplug key here. Otherwise the VM starts with
    // no hotplug window and no error.
    cli.check_hotplug_options()?;

    let num_cpus = cli.num_cpus()?;
    let mem_size = cli.mem_size()?;

    // Validate the boot images and find the bootrom before the VM
    // exists. None of this touches guest memory, so a bad path fails
    // without leaving a VM behind.
    let direct_boot_mode = cli.kernel.is_some();

    let kernel_image = if let Some(ref kpath) = cli.kernel {
        info!(log, "direct boot mode"; "kernel" => kpath.display().to_string());
        let img = vmm_boot::BootImage::open(kpath)?;
        info!(log, "kernel validated";
            "protocol" => format!("{:?}", img.protocol()),
            "entry" => format!("{:#x}", img.entry_point()),
        );
        Some(img)
    } else {
        None
    };

    let initrd_image = if let Some(ref ipath) = cli.initrd {
        Some(direct::InitrdImage::open(ipath)?)
    } else {
        None
    };

    let bootrom_spec = if direct_boot_mode {
        None
    } else {
        Some(bootrom::find_bootrom_spec(&cli.lpc)?)
    };
    if bootrom_spec.is_some() {
        bootrom::log_bootrom_diagnostics(&cli.lpc, log);
    }
    if let Some(spec) = &bootrom_spec {
        anyhow::ensure!(
            !(cli.migrate_listen.is_some() && spec.vars.is_some()),
            "--migrate-listen with a bootrom variable store is not supported: the varstore is host-file state that live migration does not carry",
        );
    }

    let version = crate::version_string();
    let opts = MachineOpts {
        vm_name: &cli.vm_name,
        num_cpus,
        mem_size,
        create_opts: CreateOpts {
            force: true,
            use_reservoir: cli.use_reservoir(),
            track_dirty: !cli.no_track_dirty,
        },
        banner: "rshyve starting",
        version: &version,
    };

    // Load the ROM in the pre-finalize hook: it is the only window with
    // mutable PhysMap access.
    let rom_to_load = bootrom_spec
        .as_ref()
        .filter(|_| cli.migrate_listen.is_none());
    let mut varfile: Option<(varstore::VarFile, u64)> = None;
    let built = build_machine(
        &opts,
        &mut |setup| {
            if let Some(spec) = rom_to_load {
                varfile = load_bootrom(setup, spec, log)?;
            }
            Ok(())
        },
        log,
    )?;
    let machine = built.machine;
    let api_version = built.api_version;

    // Mask CPU features so the VM can migrate between hosts.
    let cpu_baseline: vmm_core::cpuid::CpuBaseline =
        cli.cpu_baseline.parse()?;
    if cpu_baseline != vmm_core::cpuid::CpuBaseline::Host {
        info!(log, "applying CPU baseline"; "baseline" => &cli.cpu_baseline);
    }

    // Both branches keep the table because a vCPU brought online later
    // replays it.
    let (hyperv, cpuid) = if cli.hyperv {
        let tsc_freq_hz = cli.tsc_freq_hz.unwrap_or_else(|| {
            host_tsc_frequency_hz().unwrap_or_else(|| {
                warn!(log, "host TSC freq probe failed, reference-TSC page will be invalid; \
                            pass --tsc-freq-hz to override");
                0
            })
        });
        let features = vmm_hyperv::Features {
            reference_tsc: tsc_freq_hz != 0,
            reset: true,
            frequencies: true,
            tsc_freq_hz,
        };
        info!(log, "enabling Hyper-V enlightenments";
            "tsc_freq_hz" => tsc_freq_hz,
            "reference_tsc" => features.reference_tsc,
        );
        let hv = vmm_hyperv::HyperV::new(
            log.new(o!("module" => "hyperv")),
            machine.physmap().clone(),
            features,
        );
        let mut extra = Vec::new();
        hv.add_cpuid(&mut extra);
        let table = vmm_core::cpuid::apply_cpuid_table(
            machine.hdl(),
            num_cpus,
            cpu_baseline,
            &extra,
            log,
        )
        .context("failed to apply CPUID with Hyper-V leaves")?;
        (Some(hv), table)
    } else {
        let table = vmm_core::cpuid::apply_cpuid_table(
            machine.hdl(),
            num_cpus,
            cpu_baseline,
            &[],
            log,
        )
        .context("failed to apply CPU baseline")?;
        (None, table)
    };

    let tpm_acpi = if cli.vtpm {
        let state_dir = cli.vtpm_state_dir.clone().unwrap_or_else(|| {
            PathBuf::from("/var/db/rshyve")
                .join(&cli.vm_name)
                .join("tpm")
        });
        let tpm = vmm_tpm::Tpm::new(
            state_dir.clone(),
            log.new(o!("module" => "vtpm")),
        )
        .with_context(|| {
            format!(
                "failed to initialize vTPM (state_dir={})",
                state_dir.display(),
            )
        })?;

        // The handler holds its own Arc to the Crb device, so vCPU
        // dispatch reaches it with no lock held in the bus path.
        let crb = tpm.crb.clone();
        machine
            .bus_mmio()
            .register(
                vmm_tpm::CRB_BASE,
                vmm_tpm::CRB_REGION_LEN,
                Arc::new(move |offset, op| crb.handle(offset, op)),
            )
            .context("registering vTPM CRB MMIO region")?;
        info!(log, "vTPM CRB MMIO region registered";
            "base" => format!("{:#x}", vmm_tpm::CRB_BASE),
            "len" => format!("{:#x}", vmm_tpm::CRB_REGION_LEN),
        );
        if let Some(spec) = &bootrom_spec {
            if bootrom::should_warn_missing_tcg2(&spec.rom) {
                warn!(log, "vTPM requires TCG2-capable firmware; {} leaves the guest TPM uninitialized", spec.rom.display();
                    "recommended_firmware" => "BHYVE_UEFI_CODE.fd",
                );
            }
        }

        Some(acpi::TpmAcpi {
            table: vmm_tpm::build_tpm2_table(),
            device: acpi::TpmDevice {
                crb_base: u32::try_from(vmm_tpm::CRB_BASE)
                    .expect("CRB_BASE below 4 GiB"),
                crb_len: u32::try_from(vmm_tpm::CRB_REGION_LEN)
                    .expect("CRB_REGION_LEN fits u32"),
            },
        })
    } else {
        None
    };

    let varstore = varfile
        .map(|(vf, gpa)| -> anyhow::Result<Arc<varstore::VarStore>> {
            let vs = Arc::new(varstore::VarStore::new(
                vf,
                gpa,
                log.new(o!("module" => "varstore")),
            ));
            let gpa = vs.gpa();
            let len = vs.len();
            let handler = Arc::clone(&vs);
            machine
                .bus_mmio()
                .register(
                    gpa,
                    len as u64,
                    Arc::new(move |offset, op| handler.handle(offset, op)),
                )
                .context("registering UEFI variable store MMIO region")?;
            info!(log, "UEFI variable store registered";
                "base" => format!("{gpa:#x}"),
                "len" => format!("{len:#x}"));
            Ok(vs)
        })
        .transpose()?;
    let host_local_state =
        host_state::HostLocalState::new(tpm_acpi.is_some(), varstore.is_some());

    Ok(MachineCreation {
        machine,
        api_version,
        num_cpus,
        mem_size,
        kernel_image,
        initrd_image,
        hyperv,
        varstore,
        tpm_acpi,
        cpuid,
        cpu_baseline,
        host_local_state,
    })
}

/// Load the bootrom image into guest memory (pre-finalize).
fn load_bootrom(
    setup: &mut MachineSetup,
    spec: &bootrom::BootromSpec,
    log: &Logger,
) -> anyhow::Result<Option<(varstore::VarFile, u64)>> {
    info!(log, "loading bootrom"; "path" => spec.rom.display().to_string());
    let rom = bootrom::BootromFile::open(&spec.rom)?;
    let varfile = spec
        .vars
        .as_deref()
        .map(varstore::VarFile::open)
        .transpose()?;
    let var_size = varfile.as_ref().map(|file| file.len()).unwrap_or(0);
    let layout = bootrom::compute_layout(rom.size(), var_size)?;
    bootrom::log_flash_layout(rom.path(), spec.vars.as_deref(), &layout, log);
    let hdl = setup.hdl().clone();
    bootrom::load_bootrom(setup.map_mut(), &hdl, &rom, &layout)?;
    varfile
        .map(|file| {
            let gpa = layout.var_gpa.context(
                "variable store layout is missing its guest address",
            )?;
            Ok((file, gpa))
        })
        .transpose()
}
