// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! rshyve: a Rust bhyve VMM with live migration support.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;

use anyhow::Context;
use slog::{info, o, warn, Logger};

use vmm_boot::direct;
use vmm_config::Cli;
use vmm_core::machine::Machine;
use vmm_core::msr::MsrHandler;
use vmm_devices::InputBroker;
use vmm_machine::{
    DeviceRegistry, HotplugEngine, HotplugEngines, RegisteredDevice,
    RunOutcome, VcpuSetup,
};

mod bootorder;
mod bootrom;
mod control;
mod create;
mod host_state;
mod input;
mod mdata;
mod msix;
mod pcidev;
mod peercred;
mod reboot;
mod secret;
mod serial;
mod varstore;

use create::{create_machine, MachineCreation};
use input::setup_ps2;
use reboot::reexec_for_reboot;

/// Kernel command line used when neither `--cmdline` nor
/// `--cmdline-base64` is given. It names `ttyS0`, which is why COM1 is
/// always attached.
use vmm_config::DEFAULT_CMDLINE;

/// The guest kernel command line, from whichever spelling was given.
///
/// Both spellings resolve through `Cli::kernel_cmdline`, which is also
/// where the length bound and the NUL and control-byte checks live.
/// Reading `cli.cmdline` here instead would accept
/// `--cmdline-base64` and then boot the guest without it.
fn resolved_cmdline(cli: &Cli) -> anyhow::Result<String> {
    Ok(cli
        .kernel_cmdline()?
        .unwrap_or_else(|| DEFAULT_CMDLINE.to_string()))
}

/// If a kernel is given for direct boot, load it into guest RAM.
fn load_direct_boot_kernel(
    machine: &Machine,
    cli: &Cli,
    kernel_image: &Option<vmm_boot::BootImage>,
    initrd_image: &Option<direct::InitrdImage>,
    mem_size: usize,
    acpi_tables: vmm_machine::AcpiTables,
    log: &Logger,
) -> anyhow::Result<()> {
    if let Some(ref kernel) = kernel_image {
        let cmdline = resolved_cmdline(cli)?;
        let acpi_rsdp_addr = acpi_tables.rsdp_addr();
        let entry = kernel.load(
            machine.memctx(),
            &cmdline,
            initrd_image.as_ref(),
            mem_size,
            acpi_rsdp_addr,
        )?;
        info!(log, "kernel loaded for direct boot";
            "protocol" => format!("{:?}", kernel.protocol()),
            "entry" => format!("{:#x}", entry),
            "cmdline" => cmdline,
        );
    }
    Ok(())
}

/// Every device a migration carries state for, with the PCI address
/// that identifies it on the wire.
fn migrate_devices(
    registry: &DeviceRegistry,
) -> Vec<(
    vmm_migrate::codec::DeviceIdentity,
    Arc<dyn vmm_devices::Lifecycle>,
)> {
    registry
        .list()
        .into_iter()
        .filter_map(|slot| {
            let bdf = slot.bdf?;
            let device = slot.lifecycle?;
            Some((
                vmm_migrate::codec::DeviceIdentity {
                    bdf: bdf.into(),
                    kind: device.type_name().to_string(),
                },
                device,
            ))
        })
        .collect()
}

/// Handle migration destination mode: activate vCPUs, take a migration
/// connection, and import VM state from the source.
fn run_migration_destination(
    machine: &Machine,
    listen_addr: &str,
    num_cpus: u32,
    mem_size: usize,
    registry: &DeviceRegistry,
    cpu_baseline: vmm_core::cpuid::CpuBaseline,
    hyperv: Option<Arc<vmm_hyperv::HyperV>>,
    log: &Logger,
) -> anyhow::Result<()> {
    info!(log, "migration destination mode"; "listen" => listen_addr);

    // Activate and reset vCPUs. The reset initializes the VMCS/VMCB
    // host-state fields. The migration import then overwrites the
    // guest-state fields with the source's values.
    for vcpu in machine.vcpus() {
        vcpu.activate().with_context(|| {
            format!("failed to activate vCPU {}", vcpu.id())
        })?;
        vcpu.reboot_state()
            .with_context(|| format!("failed to reset vCPU {}", vcpu.id()))?;
    }

    let devices = migrate_devices(registry);
    let config = vmm_migrate::destination::DestConfig {
        num_cpus,
        mem_size: mem_size as u64,
        cpu_baseline,
        devices: devices.iter().map(|(ident, _)| ident.clone()).collect(),
        deadlines: vmm_migrate::wire::Deadlines::default(),
        // The control socket's migrate-dest carries the override. A VM
        // started this way has no operator to ask mid-migration.
        allow_cpu_feature_mismatch: false,
    };

    let by_bdf = devices.clone();
    let for_resume: Vec<Arc<dyn vmm_devices::Lifecycle>> =
        registry.lifecycle_devices();
    let hooks = vmm_migrate::destination::DestHooks {
        restore: Box::new(move |bdf, state| {
            let device = by_bdf
                .iter()
                .find(|(ident, _)| ident.bdf == bdf)
                .map(|(_, device)| device)
                .ok_or_else(|| {
                    vmm_devices::lifecycle::DeviceStateError::Invalid(format!(
                        "no device at {bdf}"
                    ))
                })?;
            device.restore_migrate_state(state)
        }),
        resume: Box::new(move || {
            for dev in &for_resume {
                dev.resume();
            }
        }),
        hyperv: Box::new(move |state| match hyperv.as_ref() {
            Some(hyperv) => {
                hyperv.import_state(state);
                true
            }
            // The payload carries an enlightenment this VM has no
            // handler for, so the guest would find its hypercall page
            // gone. A mismatch, not a silent drop.
            None => false,
        }),
    };

    let status = Arc::new(std::sync::Mutex::new(
        vmm_migrate::MigrationStatus::default(),
    ));
    let memctx = vmm_core::mem::MemCtx::new(machine.physmap().clone());
    let hdl = machine.hdl().clone();

    // One driver serves both stream kinds. Only a Unix socket works
    // inside a bhyve zone, where /dev/poll is unavailable. The GZ agent
    // bridges it to the peer.
    if listen_addr.starts_with('/') || listen_addr.ends_with(".sock") {
        let stream = connect_migration_socket(listen_addr, log)?;
        vmm_migrate::destination::run_destination(
            stream, hdl, &memctx, config, hooks, status, log,
        )
        .context("migration import failed")?;
    } else {
        let listener = std::net::TcpListener::bind(listen_addr)
            .with_context(|| format!("bind {listen_addr}"))?;
        info!(log, "waiting for migration"; "addr" => listen_addr);
        let (stream, peer) = listener.accept().context("accept failed")?;
        info!(log, "migration connection"; "peer" => %peer);
        vmm_migrate::destination::run_destination(
            stream, hdl, &memctx, config, hooks, status, log,
        )
        .context("migration import failed")?;
    }

    // The run state was restored per vCPU during the import. A vCPU in
    // HLT or INIT stays there and the kernel resumes it when needed.
    // The interrupt kick happens after the vCPU threads are spawned:
    // one injected before the spawn is lost.
    info!(log, "migration import complete, starting vCPUs");
    Ok(())
}

/// Connect to the agent's socket, waiting for it to appear.
///
/// The agent and the VMM start together, so the socket may not exist
/// for the first moments.
fn connect_migration_socket(
    path: &str,
    log: &Logger,
) -> anyhow::Result<std::os::unix::net::UnixStream> {
    const RETRIES: u32 = 30;
    info!(log, "connecting to migration socket"; "path" => path);
    for _ in 0..RETRIES {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(stream) => {
                info!(log, "connected to migration socket");
                return Ok(stream);
            }
            Err(_) => thread::sleep(std::time::Duration::from_millis(500)),
        }
    }
    anyhow::bail!("no migration socket at {path} after {RETRIES} tries")
}

/// What the control socket leaves behind for the rest of `run`.
#[derive(Default)]
struct ControlSocket {
    /// The listener. Dropping it stops the accept loop.
    listener: Option<control::ControlListener>,
    /// How SIGTERM stops the VM. `None` without a control socket,
    /// which is the only kind of VM that cannot be paused.
    stop: Option<Arc<dyn vmm_machine::signal::GuestStop>>,
}

/// Optionally spawn the control socket listener thread.
fn spawn_control_socket(
    machine: &Machine,
    cli: &Cli,
    num_cpus: u32,
    max_cpus: u32,
    mem_size: usize,
    cpu_baseline: vmm_core::cpuid::CpuBaseline,
    vcpu_metrics: &[Arc<vmm_core::metrics::VcpuMetrics>],
    registry: Arc<DeviceRegistry>,
    hotplug: HotplugEngines,
    migration: control::MigrationInputs,
    log: &Logger,
) -> anyhow::Result<ControlSocket> {
    let Some(sock_path) = cli.control_socket.as_ref() else {
        return Ok(ControlSocket::default());
    };
    let ctrl = control::VmController::new(
        machine.hdl().clone(),
        machine.physmap().clone(),
        cli.vm_name.clone(),
        num_cpus,
        max_cpus,
        mem_size,
        vcpu_metrics.to_vec(),
        registry,
        hotplug,
        cli.pci_slot.clone(),
        cli.lpc.clone(),
        cpu_baseline,
        migration,
        log.clone(),
    );
    let stop = control::sigterm_stop(Arc::clone(&ctrl));
    let listener = control::spawn_control_thread(sock_path, ctrl, log.clone())?;
    info!(log, "control socket listening";
        "path" => listener.path().display().to_string());
    Ok(ControlSocket {
        listener: Some(listener),
        stop: Some(stop),
    })
}

/// What `run` hands `main`.
///
/// The hot-added specs travel with the outcome because only `run` can
/// see the registry, and only `main` has argv to rebuild.
struct RunResult {
    outcome: RunOutcome,
    /// `-s` specs for the devices an operator added while the VM ran.
    hotplug_specs: Vec<String>,
}

fn run(cli: Cli, log: &Logger) -> anyhow::Result<RunResult> {
    // Read once. The MADT slot count, the CPU register file and the
    // hot-add engine all read this, so the guest cannot be told about
    // a slot no engine can fill.
    let vm_opts = vmm_machine::VmOpts::from_cli(&cli)?;
    // The hot-add window is built before the VM exists, so a window an
    // operator cannot use costs no boot.
    let mem_window = vmm_machine::mem_hotplug_window(&vm_opts)?;

    // Phase 1: Create VM, load bootrom/kernel, finalize memory
    let creation = create_machine(&cli, log)?;
    let MachineCreation {
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
    } = creation;
    let max_cpus = vm_opts.max_cpus;

    // Phase 2: Initialize chipset and kernel-emulated devices
    let chipset_devs = vmm_machine::init_chipset(&machine, log)?;

    // Phase 3: Timers, PM, pvpanic
    let timer_pm_devs = vmm_machine::setup_timers_and_pm(
        &machine,
        vmm_machine::HotplugOpts::from_vm(&vm_opts),
        log,
    )?;

    let input = Arc::new(InputBroker::default());

    // Phase 4: Serial ports and mdata agent
    let (_com1, _com2) =
        serial::setup_serial(&machine, &chipset_devs.pic, &cli, log)?;
    let _ps2 = setup_ps2(
        &machine,
        &chipset_devs.pic,
        Arc::clone(&timer_pm_devs.suspend_sink),
        &input,
        log,
    );

    // Phase 5: fw_cfg, ACPI tables, SMBIOS, E820, boot order
    let (_fwcfg, acpi_tables) = vmm_machine::setup_fwcfg_and_acpi(
        &machine,
        &vm_opts,
        num_cpus,
        mem_size,
        bootorder::build_bootorder(&cli.pci_slot)?,
        tpm_acpi.clone(),
        log,
    )?;

    // Phase 6: PCI devices
    let (registry, has_ahci_cd, pci_ctx) = pcidev::init_pci_devices(
        &machine,
        &chipset_devs.chipset,
        &cli,
        num_cpus,
        &input,
        log,
    )?;
    // The PM phase runs before the registry exists, so its handles are
    // pushed here. Without this the GPE0 block takes no part in reset
    // and its migration payload is never asked for.
    vmm_machine::register_pm_devices(&registry, &timer_pm_devs)
        .context("failed to register the PM devices")?;

    if let Some(vs) = varstore.clone() {
        // The firmware phase builds the variable store, so it never
        // passes through the `-s` factory and has to be registered here.
        registry
            .insert(RegisteredDevice::new(
                "varstore",
                None,
                None,
                Some(vs as Arc<dyn vmm_devices::Lifecycle>),
                None,
            ))
            .context("failed to register the UEFI variable store")?;
    }

    // Phase 7: Direct boot kernel loading (after ACPI tables are ready)
    load_direct_boot_kernel(
        &machine,
        &cli,
        &kernel_image,
        &initrd_image,
        mem_size,
        acpi_tables,
        log,
    )?;

    // Phase 8: vCPU activation (migration or normal)
    if let Some(ref listen_addr) = cli.migrate_listen {
        run_migration_destination(
            &machine,
            listen_addr,
            num_cpus,
            mem_size,
            &registry,
            cpu_baseline,
            hyperv.clone(),
            log,
        )?;
    } else if let Some(ref kernel) = kernel_image {
        // The closure is a named local because `BspEntry::Custom` only
        // borrows it.
        let setup_bsp = |bsp: &vmm_core::vcpu::Vcpu| kernel.setup_bsp(bsp);
        vmm_machine::activate_vcpus(
            &machine,
            cli.vmexit_on_hlt,
            vmm_machine::BspEntry::Custom(&setup_bsp),
            log,
        )?;
    } else {
        vmm_machine::activate_vcpus(
            &machine,
            cli.vmexit_on_hlt,
            vmm_machine::BspEntry::Firmware,
            log,
        )?;
    }

    // The guest can run now, so tell the brand. `vmadm create` waits on
    // this rename, not on the process, and gives up after 300 s. C
    // bhyve calls `mark_provisioned` at the same point in `main`
    // (bhyverun.c).
    vmm_machine::provision::mark_provisioned(log);

    // Phase 9: Spawn vCPU threads. The fleet, not the fixed set,
    // because a CPU added later joins the same event channel and the
    // same roster.
    let fleet = vmm_machine::spawn_vcpu_fleet(
        &machine,
        num_cpus,
        api_version,
        hyperv.clone().map(|hv| hv as Arc<dyn MsrHandler>),
        log,
    )
    .context("failed to start the boot vCPU threads")?;
    let vcpu_metrics = fleet.metrics.clone();

    // Phase 10: wake the guest's virtio drivers once the vCPUs run.
    //
    // The restore raises an interrupt per live queue, but that one
    // fires before the vCPUs start and a driver that is not running
    // cannot take it. This second one reaches a running driver, which
    // is what makes it process the caught-up used ring and post new
    // receive buffers.
    if cli.migrate_listen.is_some() {
        let devs = registry.lifecycle_devices();
        let kick_log = log.clone();
        if let Err(e) = thread::Builder::new()
            .name("post-migrate-virtio-kick".into())
            .spawn(move || {
                thread::sleep(std::time::Duration::from_millis(100));
                for dev in &devs {
                    dev.post_restore_kick();
                }
            })
        {
            // The guest keeps its rings. It may not see the entries in
            // them until it kicks a queue itself.
            warn!(kick_log, "could not start the post-migration kick thread";
                "error" => %e);
        }
    }

    // Phase 11: Hotplug engines. Each owns the thread that answers the
    // guest, so no teardown ever runs on a vCPU thread.
    let pci_hotplug = timer_pm_devs.hotplug.as_ref().map(|regs| {
        HotplugEngine::start(
            Arc::clone(&registry),
            pci_ctx,
            Arc::clone(&regs.pci),
            pcidev::hotplug_factory(&cli, num_cpus),
            log.new(o!("component" => "hotplug")),
        )
    });
    let cpu_hotplug = vmm_machine::start_cpu_hotplug(
        timer_pm_devs.hotplug.as_ref(),
        VcpuSetup {
            max_cpus,
            boot_cpus: num_cpus,
            vmexit_on_hlt: cli.vmexit_on_hlt,
            cpuid: cpuid.clone(),
        },
        &fleet,
        log,
    );
    let hotplug = HotplugEngines {
        pci: pci_hotplug,
        cpu: cpu_hotplug,
        mem: vmm_machine::start_mem_hotplug(
            mem_window,
            &machine,
            timer_pm_devs.hotplug.as_ref(),
            log,
        )?,
    };

    // Phase 12: Control socket
    let control_socket = spawn_control_socket(
        &machine,
        &cli,
        num_cpus,
        max_cpus,
        mem_size,
        cpu_baseline,
        &vcpu_metrics,
        Arc::clone(&registry),
        hotplug.clone(),
        control::MigrationInputs {
            has_ahci_cd,
            hyperv: hyperv.clone(),
            host_local_state,
        },
        log,
    )?;

    // Phase 13: Signal handling. The control-plane stop is preferred
    // because only it knows whether the VM is parked on the kernel's
    // pause, where a suspend alone is never seen.
    let stop = control_socket.stop.clone().unwrap_or_else(|| {
        Arc::new(vmm_machine::signal::HdlStop::new(machine.hdl().clone()))
    });
    vmm_machine::signal::install_sigterm_handler_with(stop, log);

    // Phase 14: Main event loop and cleanup
    let outcome = vmm_machine::run_fleet_event_loop(
        &machine,
        fleet,
        &hotplug,
        control_socket
            .listener
            .as_ref()
            .map(|listener| listener.path().to_path_buf()),
        &registry,
        log,
    );

    // Stop taking commands before the engines go: a request that lands
    // after the VM is destroyed has nothing left to act on.
    drop(control_socket);
    hotplug.shutdown();

    // Read after the engine stops, so a teardown that was in flight has
    // finished and cannot leave a departed device in the list.
    let hotplug_specs = vmm_machine::hotplug_specs(&registry);
    outcome.map(|outcome| RunResult {
        outcome,
        hotplug_specs,
    })
}

/// How long the exit path waits for the console writer thread. Bounded
/// so a consumer that stopped reading cannot hold the VMM open.
const CONSOLE_FLUSH_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);

/// Version string including git commit hash (set by build.rs).
fn version_string() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let commit = env!("VMM_GIT_COMMIT");
    format!("{version} ({commit})")
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    let exe = std::env::current_exe().unwrap_or_else(|_| {
        args.first().map(PathBuf::from).unwrap_or_default()
    });

    if vmm_machine::entry::version_requested(&args) {
        println!("rshyve {}", version_string());
        return ExitCode::SUCCESS;
    }
    vmm_machine::entry::register_probes("rshyve");

    let cli = Cli::parse_named("rshyve");
    let log = vmm_machine::entry::setup_logger("rshyve");

    let result = run(cli, &log);
    // The console writer thread holds the guest's last bytes. Both the
    // reexec below and a plain exit would drop them.
    if !vmm_devices::uart::backend::flush_stdout(CONSOLE_FLUSH_TIMEOUT) {
        warn!(log, "console output was still draining at exit");
    }
    match result {
        Ok(RunResult {
            outcome: RunOutcome::Reboot,
            hotplug_specs,
        }) => reexec_for_reboot(
            &exe,
            &args,
            &hotplug_specs,
            vmm_machine::sigterm_flag(),
            &log,
        ),
        Ok(RunResult { outcome, .. }) => {
            vmm_machine::entry::exit_code(outcome, &log)
        }
        Err(e) => {
            slog::error!(log, "fatal error"; "error" => format!("{:#}", e));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::FromArgMatches;

    fn cli_from(args: &[&str]) -> Cli {
        let matches = Cli::command_named("rshyve")
            .try_get_matches_from(args)
            .expect("argv parses");
        Cli::from_arg_matches(&matches).expect("from_arg_matches")
    }

    /// The bhyve zone brand splits its only pass-through attr on
    /// whitespace, so a command line with spaces can only reach this
    /// binary base64-encoded. Reading `cli.cmdline` directly accepts
    /// the flag and boots the guest without it.
    ///
    /// Mutation this kills: `cli.cmdline.as_deref().unwrap_or(DEFAULT)`
    /// in place of the resolver, which returns the default here.
    #[test]
    fn the_base64_command_line_reaches_the_guest() {
        let cli = cli_from(&[
            "rshyve",
            "--cmdline-base64",
            "cm9vdD0vZGV2L3ZkYTEgcnc=",
            "vm",
        ]);
        assert_eq!(
            resolved_cmdline(&cli).expect("resolves"),
            "root=/dev/vda1 rw",
        );
    }

    /// Mutation this kills: dropping the resolver's `?` so a command
    /// line that fails validation is passed to the loader anyway.
    #[test]
    fn a_control_byte_in_the_command_line_stops_the_boot() {
        let cli =
            cli_from(&["rshyve", "--cmdline", "root=/dev/vda1\x07", "vm"]);
        resolved_cmdline(&cli).expect_err("a control byte must be refused");
    }

    /// Without a command line the guest still has to be told where its
    /// console is, or a direct boot produces no output at all.
    #[test]
    fn no_command_line_at_all_names_the_serial_console() {
        let cli = cli_from(&["rshyve", "vm"]);
        assert_eq!(resolved_cmdline(&cli).expect("resolves"), DEFAULT_CMDLINE);
        assert!(DEFAULT_CMDLINE.contains("ttyS0"), "console is not ttyS0");
    }

    /// The binary must not carry its own copy of the boot images.
    /// This coerces only if the parameter types are the ones `vmm_boot`
    /// owns, so a reintroduced binary-local module is a compile error,
    /// not a silent second copy.
    ///
    /// The paths stay fully qualified so it fails on a type mismatch,
    /// not on name resolution. The import added alongside makes that
    /// qualification redundant under this workspace's
    /// `unused_qualifications` lint, so allow it here.
    #[test]
    #[allow(unused_qualifications)]
    fn boot_types_come_from_vmm_boot() {
        let _load: fn(
            &Machine,
            &Cli,
            &Option<vmm_boot::BootImage>,
            &Option<vmm_boot::direct::InitrdImage>,
            usize,
            vmm_machine::AcpiTables,
            &Logger,
        ) -> anyhow::Result<()> = load_direct_boot_kernel;

        // BSP setup travels with the image, so no caller can pair one
        // protocol's entry point with the other's register state.
        let _bsp: fn(
            &vmm_boot::BootImage,
            &vmm_core::vcpu::Vcpu,
        ) -> anyhow::Result<()> = vmm_boot::BootImage::setup_bsp;
    }

    #[test]
    fn bootorder_indexes_ahci_cd() {
        let specs = vec!["5,ahci-cd,/x.iso,bootindex=1".to_string()];

        assert_eq!(
            bootorder::build_bootorder(&specs).unwrap(),
            b"/pci@i0cf8/pci@5,0\n"
        );
    }
}
