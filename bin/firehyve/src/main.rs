// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! firehyve: a microVM built on the shared vmm crates.
//!
//! Direct-kernel boot, virtio devices only. No UEFI, no migration, no
//! control socket, no PCI passthrough.

mod console;
mod control;
mod devices;

use std::ffi::OsString;
use std::process::ExitCode;
use std::sync::Arc;

use slog::{error, info, warn};

use vmm_config::Cli;
use vmm_machine::{DeviceRegistry, HotplugEngine, HotplugEngines, VcpuSetup};
use vmm_virtio::vsock::control::{ControlSink, ControlSlot};

/// Kernel command line used when `--cmdline` is absent. It names
/// `ttyS0`, which is why COM1 is always attached.
use vmm_config::DEFAULT_CMDLINE;

/// How long the exit path waits for the console writer thread. Bounded
/// so a consumer that stopped reading cannot hold the VMM open.
const CONSOLE_FLUSH_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);

fn version_string() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let commit = env!("FIREHYVE_GIT_COMMIT");
    format!("{version} ({commit})")
}

/// Reject every shared-grammar flag firehyve cannot honor, so an
/// operator learns at startup instead of from a guest that misbehaves.
///
/// An option the bhyve zone brand puts on every zone is accepted and
/// logged, because a refusal means an unmodified brand can never start
/// this binary. An option that changes guest behaviour is still refused.
fn validate_cli(cli: &Cli, log: &slog::Logger) -> anyhow::Result<()> {
    anyhow::ensure!(
        cli.migrate_listen.is_none(),
        "firehyve does not support --migrate-listen; live migration is rshyve only"
    );
    anyhow::ensure!(
        cli.control_socket.is_none(),
        "firehyve does not support --control-socket"
    );
    anyhow::ensure!(
        !cli.vtpm && cli.vtpm_state_dir.is_none(),
        "firehyve does not support --vtpm; vTPM is rshyve only"
    );
    anyhow::ensure!(
        !cli.hyperv && cli.tsc_freq_hz.is_none(),
        "firehyve does not support --hyperv; enlightenments are rshyve only"
    );
    anyhow::ensure!(
        cli.mdata_nics.is_none()
            && cli.mdata_resolvers.is_none()
            && cli.mdata_ssh_keys.is_none()
            && cli.mdata_root_pw.is_none(),
        "firehyve does not support the --mdata-* flags; there is no COM2 agent"
    );
    // `boot.c` appends `-l bootrom,<rom>` to every bhyve zone. It uses
    // a default ROM when the zonecfg attr is unset, and nothing turns
    // this off. firehyve boots the kernel directly, so nothing loads
    // from the ROM and guest behaviour does not change. A refusal
    // blocks the unmodified brand, so the entry is accepted and logged.
    for entry in cli.lpc.iter().filter(|a| a.starts_with("bootrom,")) {
        warn!(log, "bootrom ignored";
            "requested" => entry.trim_start_matches("bootrom,"),
            "reason" => "firehyve boots the kernel directly and has no \
                         UEFI path",
        );
    }
    // A misspelled hotplug key otherwise gives a VM with no hot-add
    // window and no error.
    cli.check_hotplug_options()?;
    console::report_serial_backends(&cli.lpc, log);
    // attach_pci_devices takes bootindex-stripped specs, and a
    // direct-booted kernel has no firmware boot order for an index to
    // steer. Accepting the token would silently drop it.
    if let Some(spec) = cli
        .pci_slot
        .iter()
        .find(|s| s.split(',').any(|f| f.starts_with("bootindex=")))
    {
        anyhow::bail!(
            "firehyve boots the kernel directly and has no boot order; \
             remove bootindex from '-s {spec}'"
        );
    }
    anyhow::ensure!(
        cli.kernel.is_some(),
        "firehyve has no UEFI path; --kernel is required"
    );
    Ok(())
}

/// Arm every vsock control slot with the machine inventory.
///
/// Returns the sink so the caller can report that the interface is up,
/// and `None` when the machine has no vsock device to serve it on.
fn install_control_sink(
    slots: &[ControlSlot],
    registry: &Arc<DeviceRegistry>,
    hotplug: HotplugEngines,
    num_cpus: u32,
    max_cpus: u32,
    mem_size: usize,
) -> Option<Arc<dyn ControlSink>> {
    if slots.is_empty() {
        return None;
    }
    let sink: Arc<dyn ControlSink> = Arc::new(control::Inventory::new(
        Arc::clone(registry),
        hotplug,
        num_cpus,
        max_cpus,
        mem_size,
    ));
    for slot in slots {
        slot.install(Arc::clone(&sink));
    }
    Some(sink)
}

/// The phase sequence. Ordering is load-bearing: ACPI tables are written
/// before the kernel is loaded because the loader is handed the RSDP
/// address, and the SIGTERM handler is installed after the vCPU threads
/// exist so a power-off reaches a running VM.
fn run(
    cli: Cli,
    log: &slog::Logger,
) -> anyhow::Result<vmm_machine::RunOutcome> {
    // Phase 1: reject what this binary cannot honor. The hot-add
    // window is checked before the VM exists, so a bad window costs no
    // boot.
    validate_cli(&cli, log)?;
    // Resolved before the VM exists, so a malformed or duplicate
    // command line costs no VM. The bhyve zone brand splits its only
    // pass-through attr on whitespace, so it can carry only
    // `--cmdline-base64`.
    let cmdline = cli
        .kernel_cmdline()?
        .unwrap_or_else(|| DEFAULT_CMDLINE.to_string());
    // Read once. The MADT slot count, the CPU register file and the
    // hot-add engine all use it, so the guest never sees a slot that
    // no engine can fill.
    let vm_opts = vmm_machine::VmOpts::from_cli(&cli)?;
    let mem_window = vmm_machine::mem_hotplug_window(&vm_opts)?;

    // Phase 2: create the VM. No bootrom, so nothing goes into guest
    // memory before finalize.
    let num_cpus = vm_opts.num_cpus;
    let max_cpus = vm_opts.max_cpus;
    let mem_size = vm_opts.mem_size;
    let version = version_string();
    let opts = vmm_machine::MachineOpts {
        vm_name: &cli.vm_name,
        num_cpus,
        mem_size,
        create_opts: vmm_core::hdl::CreateOpts {
            force: true,
            use_reservoir: cli.use_reservoir(),
            // Dirty tracking exists to serve migration, which this
            // binary does not have.
            track_dirty: false,
        },
        banner: "firehyve starting",
        version: &version,
    };
    let mut pre_finalize = vmm_machine::no_pre_finalize;
    let build = vmm_machine::build_machine(&opts, &mut pre_finalize, log)?;
    let machine = build.machine;

    // CPUID stays outside build_machine because rshyve adds Hyper-V
    // leaves at this point. firehyve applies the baseline only.
    let baseline: vmm_core::cpuid::CpuBaseline = cli.cpu_baseline.parse()?;
    // Keep the table: a CPU added later must get the same one as the
    // boot CPUs.
    let cpuid = vmm_core::cpuid::apply_cpuid_table(
        machine.hdl(),
        num_cpus,
        baseline,
        &[],
        log,
    )
    .map_err(|e| anyhow::anyhow!("failed to apply CPU baseline: {e}"))?;

    // Phase 3: chipset and kernel-emulated devices.
    let chipset_devs = vmm_machine::init_chipset(&machine, log)?;

    // Phase 4: timers, PM, pvpanic.
    let timer_pm = vmm_machine::setup_timers_and_pm(
        &machine,
        vmm_machine::HotplugOpts::from_vm(&vm_opts),
        log,
    )?;

    // Phase 5: serial console, then the COM2 marker channel.
    let _com1 =
        console::setup_console(&machine, &chipset_devs.pic, &cli.lpc, log)?;
    let _com2 = console::setup_com2(&machine, &chipset_devs.pic, log)?;

    // Phase 6: fw_cfg, ACPI, SMBIOS. No boot order, no TPM2 table.
    let (_fwcfg, acpi) = vmm_machine::setup_fwcfg_and_acpi(
        &machine,
        &vm_opts,
        num_cpus,
        mem_size,
        Vec::new(),
        None,
        log,
    )?;

    // Phase 7: PCI devices. An unknown driver name is an error, not a
    // silent omission.
    let input = Arc::new(vmm_devices::InputBroker::default());
    let owned_ctx = Arc::new(vmm_machine::pci::OwnedPciCtx::new(
        &machine,
        &chipset_devs.chipset,
        &input,
        log,
    ));
    let ctx = vmm_machine::PciDeviceCtx::borrow_from(&owned_ctx);
    // The factory borrows the slot list, so it ends with this block.
    let mut control_slots: Vec<ControlSlot> = Vec::new();
    let attachment = {
        let mut factory = |spec: &str, ctx: &vmm_machine::PciDeviceCtx<'_>| {
            devices::create_pci_device(spec, num_cpus, ctx, &mut control_slots)
        };
        vmm_machine::attach_pci_devices(&ctx, &cli.pci_slot, &mut factory)?
    };

    // The PM phase runs before the registry exists, so its handles are
    // registered here. Without them the GPE0 block is not reset.
    vmm_machine::register_pm_devices(&attachment.registry, &timer_pm)?;

    // The hotplug engine owns the thread that finishes an eject, so no
    // teardown ever runs on a vCPU thread.
    let pci_hotplug = timer_pm.hotplug.as_ref().map(|regs| {
        HotplugEngine::start(
            Arc::clone(&attachment.registry),
            owned_ctx,
            Arc::clone(&regs.pci),
            devices::hotplug_factory(num_cpus),
            log.new(slog::o!("component" => "hotplug")),
        )
    });
    let mem_hotplug = vmm_machine::start_mem_hotplug(
        mem_window,
        &machine,
        timer_pm.hotplug.as_ref(),
        log,
    )?;

    // Phase 8: load the kernel, then enter the BSP at its entry point.
    let kernel_path = cli
        .kernel
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("--kernel is required"))?;
    // The image file sets the boot protocol, so there is no --boot flag.
    // Reading the image and copying it into guest memory are the two
    // largest costs on the boot path, and each has a different fix. So
    // each is timed separately.
    let t_read = std::time::Instant::now();
    let kernel = vmm_boot::BootImage::open(kernel_path)?;
    let initrd = cli
        .initrd
        .as_deref()
        .map(vmm_boot::InitrdImage::open)
        .transpose()?;
    let read_us = t_read.elapsed().as_micros() as u64;

    let t_load = std::time::Instant::now();
    let entry = kernel.load(
        machine.memctx(),
        &cmdline,
        initrd.as_ref(),
        mem_size,
        acpi.rsdp_addr(),
    )?;
    info!(log, "kernel loaded";
        "protocol" => format!("{:?}", kernel.protocol()),
        "entry" => format!("{entry:#x}"),
        "read_us" => read_us,
        "load_us" => t_load.elapsed().as_micros() as u64,
        "cmdline" => &cmdline,
    );

    // `BspEntry::Custom` only borrows the closure, so it needs a name.
    let setup_bsp = |bsp: &vmm_core::vcpu::Vcpu| kernel.setup_bsp(bsp);
    vmm_machine::activate_vcpus(
        &machine,
        cli.vmexit_on_hlt,
        vmm_machine::BspEntry::Custom(&setup_bsp),
        log,
    )?;

    // The guest can run now, so tell the brand. `vmadm create` waits on
    // this rename, not on the process, and stops after 300 s. C bhyve
    // calls `mark_provisioned` at the same point in `main`, after it
    // starts the vCPUs. The rename needs file access, so it must stay
    // before any privilege drop.
    vmm_machine::provision::mark_provisioned(log);

    // Phase 9: run. Use the fleet, not a fixed set, because a CPU added
    // later must join the same event channel and roster.
    let fleet = vmm_machine::spawn_vcpu_fleet(
        &machine,
        num_cpus,
        build.api_version,
        None,
        log,
    )
    .map_err(|e| {
        anyhow::anyhow!("failed to start the boot vCPU threads: {e}")
    })?;
    let cpu_hotplug = vmm_machine::start_cpu_hotplug(
        timer_pm.hotplug.as_ref(),
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
        mem: mem_hotplug,
    };

    // firehyve has no control socket, so the inventory is served on the
    // vsock host socket. The registry is complete only after the attach
    // pass, and the CPU engine exists only after the fleet. Until this
    // point an empty slot refuses each request, so no peer gets a
    // partial answer.
    if install_control_sink(
        &control_slots,
        &attachment.registry,
        hotplug.clone(),
        num_cpus,
        max_cpus,
        mem_size,
    )
    .is_some()
    {
        info!(log, "vsock CONTROL enabled";
            "sockets" => control_slots.len());
    }

    vmm_machine::install_sigterm_handler(&machine, log);
    let outcome = vmm_machine::run_fleet_event_loop(
        &machine,
        fleet,
        &hotplug,
        None,
        &attachment.registry,
        log,
    );

    // The vsock control sink holds a handle. Without this call the
    // drain threads outlive the event loop.
    hotplug.shutdown();
    outcome
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();

    if vmm_machine::entry::version_requested(&args) {
        println!("firehyve {}", version_string());
        return ExitCode::SUCCESS;
    }
    vmm_machine::entry::register_probes("firehyve");

    let cli = Cli::parse_named("firehyve");
    let log = vmm_machine::entry::setup_logger("firehyve");

    let outcome = run(cli, &log);
    // The guest's last COM2 bytes carry fhrun's payload status, and a
    // writer thread holds them. An exit before the flush loses them.
    if !vmm_devices::uart::backend::flush_stdout(CONSOLE_FLUSH_TIMEOUT) {
        warn!(log, "console output was still draining at exit");
    }
    match outcome {
        Ok(outcome) => vmm_machine::entry::exit_code(outcome, &log),
        Err(e) => {
            error!(log, "fatal error"; "error" => format!("{e:#}"));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{FromArgMatches, Parser};

    /// The zone brand reads the exit code: `boot.c` sets
    /// ZONE_ATTR_INITRESTART0 on every bhyve zone, so exit 0 restarts
    /// the guest and anything else halts the zone. The tests in
    /// `vmm_machine::entry` pin every value. This test checks the
    /// function this binary calls, not a copy of it.
    #[test]
    fn a_powered_off_guest_does_not_exit_zero() {
        use vmm_machine::RunOutcome;
        let log = quiet();
        assert_eq!(
            vmm_machine::entry::exit_code(RunOutcome::Reboot, &log),
            ExitCode::SUCCESS
        );
        for outcome in [
            RunOutcome::PowerOff,
            RunOutcome::Halt,
            RunOutcome::TripleFault(0),
            RunOutcome::GuestFault,
        ] {
            assert_ne!(
                vmm_machine::entry::exit_code(outcome, &log),
                ExitCode::SUCCESS,
                "{outcome:?} would restart the zone"
            );
        }
    }

    fn cli_from(args: &[&str]) -> Cli {
        let matches = Cli::command_named("firehyve")
            .try_get_matches_from(args)
            .expect("argv parses");
        Cli::from_arg_matches(&matches).expect("from_arg_matches")
    }

    /// A logger that keeps nothing. The tests that care what was
    /// logged live in `console`, which has a recording drain.
    fn quiet() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    /// The MADT slot count, the CPU register file and the hot-add engine
    /// must describe one machine. Each reads `VmOpts`. If the CPU
    /// ceiling reaches only some of them, the register file and the
    /// tables disagree.
    #[test]
    fn one_cpu_ceiling_feeds_the_registers_and_the_engine() {
        let cli = Cli::try_parse_from([
            "firehyve",
            "-c",
            "cpus=2,maxcpus=8",
            "--hotplug",
            "-m",
            "512M",
            "vm",
        ])
        .expect("a hotplug command line");
        let opts = vmm_machine::VmOpts::from_cli(&cli).expect("vm options");

        assert_eq!(opts.num_cpus, 2);
        assert_eq!(opts.max_cpus, 8);

        // The register file the guest reads.
        let hotplug = vmm_machine::pm::HotplugOpts::from_vm(&opts);
        assert_eq!(hotplug.max_cpus, opts.max_cpus);
        assert_eq!(hotplug.num_cpus, opts.num_cpus);
        assert!(hotplug.enabled);

        // The engine that brings a vCPU online.
        let setup = VcpuSetup {
            max_cpus: opts.max_cpus,
            boot_cpus: opts.num_cpus,
            vmexit_on_hlt: cli.vmexit_on_hlt,
            cpuid: None,
        };
        assert_eq!(setup.max_cpus, hotplug.max_cpus);
        assert_eq!(setup.boot_cpus, hotplug.num_cpus);
    }

    #[test]
    fn a_machine_with_no_vsock_device_arms_no_control_sink() {
        let registry = Arc::new(DeviceRegistry::new());
        assert!(install_control_sink(
            &[],
            &registry,
            HotplugEngines::default(),
            2,
            2,
            1 << 30,
        )
        .is_none());
    }

    #[test]
    fn every_vsock_slot_is_armed_with_the_machine_inventory() {
        // Two slots: with a second `-s ...,virtio-vsock`, both sockets
        // must answer.
        let registry = Arc::new(DeviceRegistry::new());
        registry
            .insert(vmm_machine::RegisteredDevice::new(
                "virtio-vsock@5",
                vmm_machine::parse_bdf("5"),
                None,
                None,
                None,
            ))
            .expect("insert");
        let slots = vec![ControlSlot::default(), ControlSlot::default()];

        let sink = install_control_sink(
            &slots,
            &registry,
            HotplugEngines::default(),
            2,
            2,
            1 << 30,
        )
        .expect("armed");

        for slot in &slots {
            assert!(slot.sink().is_some(), "a slot was left empty");
        }
        // The sink reads the registry the attach pass filled, not a copy.
        assert_eq!(
            vmm_virtio::vsock::control::answer_line(
                sink.as_ref(),
                "device-list"
            ),
            "OK 1 virtio-vsock@5,0.5.0,present"
        );
        assert_eq!(
            vmm_virtio::vsock::control::answer_line(sink.as_ref(), "cpu-list"),
            "OK 2 0,present,boot 1,present,boot"
        );
    }

    #[test]
    fn kernel_is_required() {
        let cli = cli_from(&["firehyve", "guest"]);
        let err = validate_cli(&cli, &quiet()).expect_err("no --kernel");
        assert!(err.to_string().contains("--kernel"), "got: {err}");
    }

    #[test]
    fn migration_flags_are_rejected() {
        let cli = cli_from(&[
            "firehyve",
            "--kernel",
            "/tmp/vmlinux",
            "--migrate-listen",
            "0.0.0.0:4567",
            "guest",
        ]);
        let err =
            validate_cli(&cli, &quiet()).expect_err("migration is rshyve only");
        assert!(err.to_string().contains("--migrate-listen"), "got: {err}");
    }

    #[test]
    fn control_socket_is_rejected() {
        let cli = cli_from(&[
            "firehyve",
            "--kernel",
            "/tmp/vmlinux",
            "--control-socket",
            "/tmp/ctl.sock",
            "guest",
        ]);
        let err = validate_cli(&cli, &quiet()).expect_err("no control socket");
        assert!(err.to_string().contains("--control-socket"), "got: {err}");
    }

    /// `boot.c` appends `-l bootrom,<rom>` to every bhyve zone, and
    /// nothing turns this off. A refusal means an unmodified brand can
    /// never start firehyve. The kernel boots directly, so the ROM is
    /// unused.
    ///
    /// Mutation this kills: a bail on a bootrom entry.
    #[test]
    fn the_bootrom_the_brand_always_appends_is_accepted() {
        let cli = cli_from(&[
            "firehyve",
            "--kernel",
            "/tmp/vmlinux",
            "-l",
            "bootrom,/usr/share/bhyve/BHYVE_UEFI.fd",
            "guest",
        ]);
        validate_cli(&cli, &quiet()).expect("the brand always sends this");
    }

    #[test]
    fn a_bootindex_token_is_rejected() {
        // attach_pci_devices takes bootindex-stripped specs, and a
        // direct-booted kernel has no firmware boot order to steer.
        let cli = cli_from(&[
            "firehyve",
            "--kernel",
            "/tmp/vmlinux",
            "-s",
            "4,virtio-blk,/tmp/d.img,bootindex=1",
            "guest",
        ]);
        let err = validate_cli(&cli, &quiet()).expect_err("no boot order");
        assert!(err.to_string().contains("bootindex"), "got: {err}");
    }

    /// The `com1` zonecfg attr is `/dev/zconsole` on every bhyve zone
    /// tritond-vmadm builds, so this entry must not be refused.
    ///
    /// Mutation this kills: refusing a com1 backend that is not stdio.
    #[test]
    fn the_console_backend_the_brand_emits_is_accepted() {
        let cli = cli_from(&[
            "firehyve",
            "--kernel",
            "/tmp/vmlinux",
            "-l",
            "com1,/dev/zconsole",
            "guest",
        ]);
        validate_cli(&cli, &quiet()).expect("the brand always sends this");
    }

    /// The argv `boot.c` builds for a bhyve zone must start firehyve.
    ///
    /// Every element here comes from a real zone:
    /// - `boot.c` always appends `-l bootrom,...`.
    /// - `com1` and `com2` come from zonecfg attrs that tritond-vmadm
    ///   always sets.
    /// - The kernel, initrd and command line arrive through
    ///   `bhyve_extra_opts`. The brand splits it on space and tab, so
    ///   the command line is base64.
    ///
    /// Mutation this kills: a refusal of any one of the three.
    #[test]
    fn the_argv_the_bhyve_brand_builds_is_accepted() {
        let cmdline = "root=/dev/ram0 init=/init console=ttyS0 panic=-1";
        // The literal is the value tritond-vmadm puts in the attr. A
        // literal keeps the base64 crate out of this binary's
        // dependency graph, which deny-firehyve.toml controls.
        let encoded =
            "cm9vdD0vZGV2L3JhbTAgaW5pdD0vaW5pdCBjb25zb2xlPXR0eVMwIHBhbmljPS0x";
        // The brand's split breaks the plain form.
        assert!(cmdline.split([' ', '\t']).count() > 1);
        assert_eq!(encoded.split([' ', '\t']).count(), 1);

        let cli = cli_from(&[
            "firehyve",
            "-c",
            "1",
            "-m",
            "2048",
            "-s",
            "0,hostbridge",
            "-s",
            "31,lpc",
            "-l",
            "bootrom,/usr/share/bhyve/BHYVE_UEFI.fd",
            "-l",
            "com1,/dev/zconsole",
            "-l",
            "com2,socket,/tmp/vm.ttyb",
            "-H",
            "-B",
            "1,manufacturer=Joyent",
            "--kernel",
            "/vmlinux",
            "--cmdline-base64",
            encoded,
            "3fe75ae1-8624-4764-80a3-2f4a332f3604",
        ]);

        validate_cli(&cli, &quiet()).expect("the brand's argv must boot");
        assert_eq!(cli.kernel_cmdline().unwrap().as_deref(), Some(cmdline));
    }

    /// Only the options the brand forces are accepted. An option that
    /// changes guest behaviour is still refused.
    ///
    /// Mutation this kills: accepting more than the two options the
    /// brand forces on every zone.
    #[test]
    fn the_refusals_that_protect_guest_behaviour_are_kept() {
        let base = ["firehyve", "--kernel", "/tmp/vmlinux"];
        let cases: [(&str, &[&str]); 4] = [
            ("--vtpm", &["--vtpm"]),
            ("--hyperv", &["--hyperv"]),
            ("--mdata-", &["--mdata-ssh-keys", "ssh-rsa AAAA"]),
            ("--control-socket", &["--control-socket", "/tmp/c.sock"]),
        ];
        for (want, extra) in cases {
            let mut argv = base.to_vec();
            argv.extend_from_slice(extra);
            argv.push("guest");
            let cli = cli_from(&argv);
            let err = validate_cli(&cli, &quiet())
                .expect_err("must stay refused")
                .to_string();
            assert!(err.contains(want), "for {want} got: {err}");
        }
    }

    #[test]
    fn a_plain_direct_boot_command_line_validates() {
        let cli = cli_from(&[
            "firehyve",
            "-c",
            "2",
            "-m",
            "512M",
            "-s",
            "4,virtio-blk,/tmp/d.img",
            "-l",
            "com1,stdio",
            "--kernel",
            "/tmp/vmlinux",
            "guest",
        ]);
        validate_cli(&cli, &quiet()).expect("accepted");
    }
}
