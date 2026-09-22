// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ACPI PM timer, PM1 and GPE0 registers, the hotplug register files,
//! ISA IRQ trigger modes, and pvpanic.

use std::sync::Arc;

use anyhow::Context;
use slog::{info, warn, Logger};
use vmm_core::common::RWOp;
use vmm_core::intr_pins::{IntrPin, LegacyPIC};
use vmm_core::machine::Machine;
use vmm_core::pio::{PioBus, PioFn};
use vmm_devices::acpi_gpe::{AcpiGpe, HotplugEventSink, GPE0_BLK_ADDR};
use vmm_devices::acpi_pm::{AcpiPm, HdlSuspendSink, SuspendSink};
use vmm_devices::bhyve::pmtimer::{BhyvePmTimer, PMBASE_DEFAULT};
use vmm_devices::hotplug::cpu::{
    CpuHotplug, CPU_HOTPLUG_LEN, CPU_HOTPLUG_PORT,
};
use vmm_devices::hotplug::mem::{
    MemHotplug, MAX_SLOTS as MEM_HOTPLUG_SLOTS, MEM_HOTPLUG_IO_BASE,
    MEM_HOTPLUG_IO_LEN,
};
use vmm_devices::hotplug::pci::{
    PciHotplug, PCI_HOTPLUG_ADDR, PCI_HOTPLUG_LEN,
};
use vmm_devices::pvpanic::{PvPanic, PVPANIC_IOPORT};

use crate::hotplug::mem::hot_add_window;
use crate::hotplug::mem::{MemHotplugEngine, MemWindow, DEFAULT_SLOT_SIZE};
use crate::opts::VmOpts;
use crate::registry::{DeviceRegistry, RegisteredDevice, RegistryError};

/// ISA IRQ the SCI is delivered on. The FADT publishes the same number.
const SCI_IRQ: u8 = 9;

/// The GPE0 block and the PIC that carries its SCI.
///
/// One unit, because a `LegacyPin` holds only a weak reference to its
/// PIC: a GPE0 block whose PIC was dropped raises no interrupt.
pub struct Gpe0 {
    pub block: Arc<AcpiGpe>,
    pub sci_pic: Arc<LegacyPIC>,
}

/// The three hotplug register files, once their ports are claimed.
///
/// Each one only records what the guest asked for. The draining and the
/// teardown belong to [`crate::hotplug::HotplugEngine`], on its own
/// thread.
pub struct HotplugRegisters {
    pub pci: Arc<PciHotplug>,
    /// `None` when the VM has no spare CPU slot. The CPU AML is emitted
    /// only when `max_cpus` is above the boot count, so a VM without
    /// spare slots reserves no CPU port and must not claim one.
    pub cpu: Option<Arc<CpuHotplug>>,
    pub mem: Arc<MemHotplug>,
}

/// What the PM phase needs to know about hotplug.
///
/// Built from the one [`VmOpts`] the ACPI tables also read, so the
/// ports this file claims and the ports the DSDT reserves cannot
/// disagree.
#[derive(Debug, Clone, Copy)]
pub struct HotplugOpts {
    pub enabled: bool,
    pub num_cpus: u32,
    pub max_cpus: u32,
}

impl HotplugOpts {
    pub fn from_vm(opts: &VmOpts) -> Self {
        Self {
            enabled: opts.hotplug,
            num_cpus: opts.num_cpus,
            max_cpus: opts.max_cpus,
        }
    }

    /// Whether the DSDT describes CPU slots, and so whether the CPU
    /// register file may claim its ports.
    ///
    /// Mirrors `acpi::hotplug::cpu::cpu_slots`, which emits nothing
    /// when no CPU can ever be added.
    ///
    /// `crate::hotplug::cpu::CpuHotplugEngine` makes the same cut from
    /// the other side: it refuses every id at or above `max_cpus` and
    /// every id below the boot count, so a VM where the two are equal
    /// can add no CPU and needs no register file.
    fn has_cpu_slots(&self) -> bool {
        self.enabled && self.max_cpus > self.num_cpus
    }
}

/// The memory hot-add window this VM asked for, or `None`.
///
/// Read from the same [`VmOpts`] the CPU counts and the ACPI tables
/// read, so the window an engine hands out and the slots the AML
/// describes cannot disagree.
///
/// Call this BEFORE the VM is created. Every refusal here is an
/// operator mistake, and finding one after the VM exists wastes a boot.
pub fn mem_hotplug_window(opts: &VmOpts) -> anyhow::Result<Option<MemWindow>> {
    let Some(max_mem) = opts.max_mem else {
        return Ok(None);
    };
    let slot_size = opts.mem_slot_size.unwrap_or(DEFAULT_SLOT_SIZE);
    let window = hot_add_window(opts.mem_size, max_mem, slot_size)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Some(window))
}

/// Start the memory hot-add engine, or nothing when the VM asked for no
/// window.
///
/// `-o hotplug.maxmem` already requires `--hotplug`, so a window with no
/// register file is a contradiction and not a silent omission.
pub fn start_mem_hotplug(
    window: Option<MemWindow>,
    machine: &Machine,
    regs: Option<&HotplugRegisters>,
    log: &Logger,
) -> anyhow::Result<Option<Arc<MemHotplugEngine>>> {
    let Some(window) = window else {
        return Ok(None);
    };
    let regs = regs.ok_or_else(|| {
        anyhow::anyhow!("a memory window needs the hotplug register files")
    })?;
    info!(log, "memory hot-add window";
        "base" => format!("{:#x}", window.base()),
        "bytes" => window.size(),
        "slots" => window.slots(),
    );
    Ok(Some(MemHotplugEngine::start(
        Arc::clone(machine.physmap()),
        Arc::clone(machine.hdl()),
        Arc::clone(machine.segids()),
        Arc::clone(&regs.mem),
        window,
        log.new(slog::o!("component" => "hotplug-mem")),
    )))
}

/// Timer and power-management device handles kept alive for the VM's
/// lifetime.
pub struct TimerAndPmDevices {
    pub pmtimer: Arc<BhyvePmTimer>,
    pub acpi_pm: Arc<AcpiPm>,
    /// `None` unless the VM publishes the ACPI hotplug interface.
    pub gpe: Option<Gpe0>,
    /// `None` for the same reason: the register files answer only the
    /// AML that a hotplug VM emits.
    pub hotplug: Option<HotplugRegisters>,
    pub pvpanic: Arc<PvPanic>,
    pub suspend_sink: Arc<dyn SuspendSink>,
}

/// Claim the GPE0 register block, or nothing at all.
///
/// `hotplug` must be the same `VmOpts::hotplug` the FADT and the DSDT
/// read through [`crate::firmware::setup_fwcfg_and_acpi`]. Without
/// hotplug the FADT publishes GPE0_BLK = 0 and the DSDT reserves no
/// port, while it does advertise 0x0D00-0xFFFF as the PCI I/O window.
/// 0xAFE0 is inside that window, so a VM that claimed the port would
/// hold an address the guest is told it may give to an I/O BAR.
fn attach_gpe0(
    bus_pio: &PioBus,
    hotplug: bool,
    sci: Arc<dyn IntrPin>,
    log: &Logger,
) -> anyhow::Result<Option<Arc<AcpiGpe>>> {
    if !hotplug {
        return Ok(None);
    }
    let gpe = AcpiGpe::create_and_attach(
        bus_pio,
        sci,
        log.new(slog::o!("component" => "acpi-gpe")),
    )
    .context("failed to attach the GPE0 block")?;
    info!(log, "GPE0 block attached";
        "port" => format!("{:#x}", GPE0_BLK_ADDR));
    Ok(Some(gpe))
}

/// Claim the three hotplug register blocks, or nothing at all.
///
/// The PCI and CPU blocks sit inside the PCI I/O window the DSDT hands
/// the guest, so they are claimed under the same condition as the GPE0
/// block: only when the tables reserve them. The memory block at 0x0A00
/// is below the window, but it follows the same gate so that a VM
/// without hotplug claims none of these ports.
fn attach_hotplug_registers(
    bus_pio: &PioBus,
    opts: HotplugOpts,
    gpe: &Arc<AcpiGpe>,
    log: &Logger,
) -> anyhow::Result<HotplugRegisters> {
    let sink: Arc<dyn HotplugEventSink> = Arc::clone(gpe) as Arc<_>;

    let pci = PciHotplug::new(
        Arc::clone(&sink),
        log.new(slog::o!("component" => "hotplug-pci")),
    );
    register_block(
        bus_pio,
        PCI_HOTPLUG_ADDR,
        u16::from(PCI_HOTPLUG_LEN),
        pci.pio_handler(),
        "pci",
    )?;

    let cpu = opts
        .has_cpu_slots()
        .then(|| {
            let cpu = CpuHotplug::new(
                opts.max_cpus,
                Arc::clone(&sink),
                log.new(slog::o!("component" => "hotplug-cpu")),
            );
            // Without this, every slot answers `_STA` with zero and the
            // guest boots seeing no CPU at all.
            cpu.set_boot_cpus(opts.num_cpus);
            let handler = {
                let cpu = Arc::clone(&cpu);
                Arc::new(move |offset: u16, rwo: RWOp<'_>| {
                    cpu.pio_rw(usize::from(offset), rwo);
                }) as Arc<PioFn>
            };
            register_block(
                bus_pio,
                CPU_HOTPLUG_PORT,
                CPU_HOTPLUG_LEN,
                handler,
                "cpu",
            )
            .map(|()| cpu)
        })
        .transpose()?;

    let mem = MemHotplug::new(
        MEM_HOTPLUG_SLOTS,
        sink,
        log.new(slog::o!("component" => "hotplug-mem")),
    );
    let handler = {
        let mem = Arc::clone(&mem);
        Arc::new(move |offset: u16, rwo: RWOp<'_>| {
            mem.pio_rw(usize::from(offset), rwo);
        }) as Arc<PioFn>
    };
    register_block(
        bus_pio,
        MEM_HOTPLUG_IO_BASE,
        MEM_HOTPLUG_IO_LEN,
        handler,
        "mem",
    )?;

    info!(log, "hotplug register files attached";
        "pci" => format!("{PCI_HOTPLUG_ADDR:#x}"),
        "cpu" => cpu.is_some().then(|| format!("{CPU_HOTPLUG_PORT:#x}")),
        "mem" => format!("{MEM_HOTPLUG_IO_BASE:#x}"),
        "cpu_slots" => opts.max_cpus,
        "boot_cpus" => opts.num_cpus,
    );
    Ok(HotplugRegisters { pci, cpu, mem })
}

fn register_block(
    bus_pio: &PioBus,
    port: u16,
    len: u16,
    handler: Arc<PioFn>,
    name: &str,
) -> anyhow::Result<()> {
    bus_pio.register(port, len, handler).map_err(|e| {
        anyhow::anyhow!("hotplug-{name}: cannot claim port {port:#x}: {e}")
    })
}

/// Put the PM devices that carry reset or migration state into the
/// registry.
///
/// The registry is built by the PCI attach pass, which runs after this
/// phase, so a binary calls this once that pass returns. Without it the
/// GPE0 block takes no part in reset and its migration payload is never
/// asked for.
///
/// The three hotplug register files are not here: none of them
/// implements [`vmm_devices::Lifecycle`], so the registry has nothing to
/// hold. They keep their latched events across a guest reset, which the
/// re-exec in `rshyve` makes moot, because the replacement process
/// builds new ones.
pub fn register_pm_devices(
    registry: &DeviceRegistry,
    devs: &TimerAndPmDevices,
) -> Result<(), RegistryError> {
    register_gpe0(registry, devs.gpe.as_ref().map(|gpe| &gpe.block))
}

fn register_gpe0(
    registry: &DeviceRegistry,
    gpe: Option<&Arc<AcpiGpe>>,
) -> Result<(), RegistryError> {
    let Some(block) = gpe else {
        return Ok(());
    };
    registry.insert(RegisteredDevice::new(
        "acpi-gpe",
        None,
        None,
        Some(Arc::clone(block) as Arc<dyn vmm_devices::Lifecycle>),
        None,
    ))
}

/// Configure the ACPI PM timer, ISA IRQ trigger modes for SCI and PCI,
/// the ACPI PM1 and GPE0 registers, the hotplug register files, and the
/// pvpanic PIO device.
pub fn setup_timers_and_pm(
    machine: &Machine,
    opts: HotplugOpts,
    log: &Logger,
) -> anyhow::Result<TimerAndPmDevices> {
    let pmtimer = BhyvePmTimer::create(machine.hdl().clone(), PMBASE_DEFAULT);
    pmtimer.attach().context("failed to configure PM timer")?;

    // The SCI is level-triggered.
    machine
        .hdl()
        .isa_set_irq_trigger(9, true)
        .context("failed to set SCI IRQ trigger mode")?;

    // PCI INTx is level-triggered. Firmware routes PIRQ links A-D to
    // IRQs 10 and 11 through the PIR registers.
    machine
        .hdl()
        .isa_set_irq_trigger(10, true)
        .context("failed to set IRQ 10 trigger mode")?;
    machine
        .hdl()
        .isa_set_irq_trigger(11, true)
        .context("failed to set IRQ 11 trigger mode")?;
    info!(log, "PM timer attached"; "port" => format!("{:#x}", pmtimer.port()));

    let suspend_sink: Arc<dyn SuspendSink> = HdlSuspendSink::new(
        machine.hdl().clone(),
        log.new(slog::o!("component" => "acpi-pm")),
    );
    let acpi_pm = AcpiPm::create_and_attach(
        machine.bus_pio(),
        Arc::clone(&suspend_sink),
        log.new(slog::o!("component" => "acpi-pm")),
    )
    .context("failed to attach ACPI PM registers")?;
    info!(log, "ACPI PM registers attached"; "pmbase" => format!("{:#x}", 0x400));

    // The SCI keeps its own PIC. IRQ 9 is the SCI's alone: the legacy
    // devices take IRQs 1, 3, 4 and 12, and firmware routes PCI INTx to
    // IRQ 10 or 11, so no shared level count is lost here.
    let sci_pic = LegacyPIC::new(
        machine.hdl().clone(),
        log.new(slog::o!("component" => "sci-pic")),
    );
    let sci: Arc<dyn IntrPin> = sci_pic
        .pin_handle(SCI_IRQ)
        .with_context(|| format!("IRQ {SCI_IRQ} cannot carry the SCI"))?;
    // The PIC follows the block it feeds: a VM with no GPE0 block drops
    // both here.
    let gpe = attach_gpe0(machine.bus_pio(), opts.enabled, sci, log)?;
    let hotplug = gpe
        .as_ref()
        .map(|block| {
            attach_hotplug_registers(machine.bus_pio(), opts, block, log)
        })
        .transpose()?;
    let gpe = gpe.map(|block| Gpe0 { block, sci_pic });

    // pvpanic device: the guest writes to port 0x505 on kernel panic
    let pvpanic = PvPanic::new(log.clone());
    {
        let pvpanic = Arc::clone(&pvpanic);
        let handler: Arc<PioFn> = Arc::new(move |_offset, rwo| match rwo {
            RWOp::Read(ro) => {
                ro.write_u8(pvpanic.pio_read());
            }
            RWOp::Write(wo) => {
                pvpanic.pio_write(wo.read_u8());
            }
        });
        if let Err(e) = machine.bus_pio().register(PVPANIC_IOPORT, 1, handler) {
            warn!(log, "failed to register pvpanic PIO handler"; "error" => %e);
        }
    }

    Ok(TimerAndPmDevices {
        pmtimer,
        acpi_pm,
        gpe,
        hotplug,
        pvpanic,
        suspend_sink,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use vmm_devices::acpi_gpe::{GpeBit, GPE0_BLK_LEN};

    /// A pin with no PIC behind it. The port claim is what is under
    /// test, not interrupt delivery.
    struct TestPin(Mutex<bool>);

    impl IntrPin for TestPin {
        fn assert(&self) {
            *self.0.lock().expect("pin lock poisoned") = true;
        }

        fn deassert(&self) {
            *self.0.lock().expect("pin lock poisoned") = false;
        }

        fn is_asserted(&self) -> bool {
            *self.0.lock().expect("pin lock poisoned")
        }
    }

    fn test_log() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    fn opts_from(argv: &[&str]) -> VmOpts {
        let cli = vmm_config::Cli::try_parse_named("rshyve", argv)
            .expect("valid argv parses");
        VmOpts::from_cli(&cli).expect("the options are legal")
    }

    #[test]
    fn a_vm_that_asked_for_no_window_gets_none() {
        let opts = opts_from(&["rshyve", "--hotplug", "-S", "guest"]);
        assert!(mem_hotplug_window(&opts).expect("no window").is_none());
    }

    #[test]
    fn the_window_is_built_from_the_same_opts_the_tables_read() {
        let opts = opts_from(&[
            "rshyve",
            "--hotplug",
            "-S",
            "-m",
            "4G",
            "-o",
            "hotplug.maxmem=4G",
            "-o",
            "hotplug.memslot=1G",
            "guest",
        ]);

        let window = mem_hotplug_window(&opts)
            .expect("the window is legal")
            .expect("a window was asked for");

        // 4 GiB of RAM is 3 GiB low and 1 GiB high, so high RAM ends at
        // 5 GiB and the window starts there.
        assert_eq!(window.base(), 0x1_4000_0000);
        assert_eq!(window.size(), 4 * 1024 * 1024 * 1024);
        assert_eq!(window.slots(), 4);
    }

    #[test]
    fn a_window_the_guest_could_not_reach_is_refused_before_the_vm_exists() {
        // Four GiB of the 128 MiB default slot needs 32 slots and the
        // AML declares eight. Finding this after the VM is created
        // would waste a boot.
        let opts = opts_from(&[
            "rshyve",
            "--hotplug",
            "-S",
            "-o",
            "hotplug.maxmem=4G",
            "guest",
        ]);

        let error = mem_hotplug_window(&opts).expect_err("32 slots");
        assert!(error.to_string().contains("raise the slot size"), "{error}");
    }

    fn opts(enabled: bool, num_cpus: u32, max_cpus: u32) -> HotplugOpts {
        HotplugOpts {
            enabled,
            num_cpus,
            max_cpus,
        }
    }

    /// The bus, the pin, and whatever the attach produced.
    struct Attached {
        bus: PioBus,
        pin: Arc<TestPin>,
        gpe: Option<Arc<AcpiGpe>>,
        regs: Option<HotplugRegisters>,
    }

    fn attach(opts: HotplugOpts) -> Attached {
        let bus = PioBus::new();
        let pin = Arc::new(TestPin(Mutex::new(false)));
        let sci: Arc<dyn IntrPin> = Arc::clone(&pin) as Arc<_>;
        let gpe = attach_gpe0(&bus, opts.enabled, sci, &test_log())
            .expect("the GPE0 block should attach");
        let regs = gpe.as_ref().map(|block| {
            attach_hotplug_registers(&bus, opts, block, &test_log())
                .expect("the register files should attach")
        });
        Attached {
            bus,
            pin,
            gpe,
            regs,
        }
    }

    /// True when nothing holds `port`.
    ///
    /// Read back through a claim, not through a read: a register file
    /// answers an undefined offset with all ones, which is also what an
    /// unclaimed port reads as.
    fn unclaimed(bus: &PioBus, port: u16) -> bool {
        let noop: Arc<PioFn> = Arc::new(|_, _| {});
        match bus.register(port, 1, noop) {
            Ok(()) => {
                bus.unregister(port).expect("the claim above went through");
                true
            }
            Err(_) => false,
        }
    }

    fn block_is_claimed(bus: &PioBus, base: u16, len: u16) -> bool {
        (0..len).all(|offset| !unclaimed(bus, base + offset))
    }

    fn block_is_free(bus: &PioBus, base: u16, len: u16) -> bool {
        (0..len).all(|offset| unclaimed(bus, base + offset))
    }

    /// Every block this file can claim, and its length.
    const BLOCKS: [(u16, u16); 4] = [
        (GPE0_BLK_ADDR, GPE0_BLK_LEN as u16),
        (PCI_HOTPLUG_ADDR, PCI_HOTPLUG_LEN as u16),
        (CPU_HOTPLUG_PORT, CPU_HOTPLUG_LEN),
        (MEM_HOTPLUG_IO_BASE, MEM_HOTPLUG_IO_LEN),
    ];

    #[test]
    fn a_vm_without_hotplug_claims_no_port() {
        let attached = attach(opts(false, 2, 8));

        assert!(attached.gpe.is_none());
        assert!(attached.regs.is_none());
        for (base, len) in BLOCKS {
            assert!(
                block_is_free(&attached.bus, base, len),
                "port {base:#x} is claimed",
            );
        }
    }

    #[test]
    fn a_vm_with_hotplug_claims_the_gpe0_block() {
        let attached = attach(opts(true, 2, 8));

        assert!(attached.gpe.is_some());
        assert!(block_is_claimed(
            &attached.bus,
            GPE0_BLK_ADDR,
            u16::from(GPE0_BLK_LEN)
        ));
        // Exactly the block: the byte after it stays free.
        assert!(unclaimed(
            &attached.bus,
            GPE0_BLK_ADDR + u16::from(GPE0_BLK_LEN)
        ));
    }

    #[test]
    fn a_hotplug_vm_claims_the_pci_and_memory_register_files() {
        let attached = attach(opts(true, 2, 8));

        assert!(block_is_claimed(
            &attached.bus,
            PCI_HOTPLUG_ADDR,
            u16::from(PCI_HOTPLUG_LEN)
        ));
        assert!(block_is_claimed(
            &attached.bus,
            MEM_HOTPLUG_IO_BASE,
            MEM_HOTPLUG_IO_LEN
        ));
        // Exactly the blocks the AML reserves.
        assert!(unclaimed(
            &attached.bus,
            PCI_HOTPLUG_ADDR + u16::from(PCI_HOTPLUG_LEN)
        ));
        assert!(unclaimed(
            &attached.bus,
            MEM_HOTPLUG_IO_BASE + MEM_HOTPLUG_IO_LEN
        ));
    }

    #[test]
    fn the_cpu_register_file_follows_the_cpu_slots_the_dsdt_describes() {
        // acpi::hotplug::cpu emits no controller when max_cpus is not
        // above the boot count, so its ports are not reserved and this
        // file must leave them free.
        let no_slots = attach(opts(true, 4, 4));
        assert!(no_slots.regs.expect("hotplug is on").cpu.is_none());
        assert!(block_is_free(
            &no_slots.bus,
            CPU_HOTPLUG_PORT,
            CPU_HOTPLUG_LEN
        ));

        let slots = attach(opts(true, 2, 8));
        assert!(slots.regs.expect("hotplug is on").cpu.is_some());
        assert!(block_is_claimed(
            &slots.bus,
            CPU_HOTPLUG_PORT,
            CPU_HOTPLUG_LEN
        ));
    }

    /// Read the status byte of one CPU slot through the bus.
    fn cpu_slot_status(bus: &PioBus, slot: u32) -> u8 {
        bus.handle_out(CPU_HOTPLUG_PORT, 4, slot);
        bus.handle_in(CPU_HOTPLUG_PORT + 4, 1) as u8
    }

    #[test]
    fn the_boot_cpus_are_present_in_the_register_file() {
        // CpuHotplug::new marks every slot absent. Without the
        // set_boot_cpus call the guest reads _STA as zero for every CPU
        // and boots with none of them visible.
        const ENABLED: u8 = 1 << 0;
        const INSERT_EVENT: u8 = 1 << 1;
        let attached = attach(opts(true, 2, 8));

        for cpu in 0..2 {
            let status = cpu_slot_status(&attached.bus, cpu);
            assert_eq!(status & ENABLED, ENABLED, "boot CPU {cpu} is absent");
            // A boot CPU was never inserted, so it raises no event.
            assert_eq!(status & INSERT_EVENT, 0, "CPU {cpu} looks hot-added");
        }
        for cpu in 2..8 {
            assert_eq!(
                cpu_slot_status(&attached.bus, cpu) & ENABLED,
                0,
                "spare slot {cpu} is not empty",
            );
        }
    }

    #[test]
    fn every_spare_slot_a_hot_add_can_take_is_in_the_register_file() {
        // The hot-add engine hands out ids up to max_cpus - 1. A
        // register file built with fewer slots would drop the notify,
        // and the guest would never learn about the new CPU.
        const ENABLED: u8 = 1 << 0;
        let attached = attach(opts(true, 2, 8));
        let cpu = attached
            .regs
            .expect("hotplug is on")
            .cpu
            .expect("there are spare slots");

        cpu.notify_added(7);

        assert_eq!(
            cpu_slot_status(&attached.bus, 7) & ENABLED,
            ENABLED,
            "the last spare slot is not described",
        );
    }

    #[test]
    fn a_slot_event_reaches_the_sci_through_the_gpe0_block() {
        // The register files hold a HotplugEventSink, not a pin. This is
        // the proof that the sink they were given is the GPE0 block that
        // drives the SCI.
        let attached = attach(opts(true, 2, 8));
        let regs = attached.regs.expect("hotplug is on");
        // The line follows status & enable, so the guest has to enable
        // the bit first.
        attached.bus.handle_out(
            GPE0_BLK_ADDR + u16::from(GPE0_BLK_LEN) / 2,
            1,
            u32::from(GpeBit::Pci.mask()),
        );
        assert!(!attached.pin.is_asserted());

        regs.pci.notify_added(4);

        assert!(attached.pin.is_asserted(), "the SCI stayed low");
    }

    #[test]
    fn the_gpe0_block_sits_inside_the_pci_io_window() {
        // The reason the claim is gated. The DSDT hands the guest
        // 0x0D00-0xFFFF for I/O BARs, and these ports are in it, unlike
        // every other port this VMM claims.
        for (base, _len) in [BLOCKS[0], BLOCKS[1], BLOCKS[2]] {
            assert!((0x0D00..=0xFFFF).contains(&base));
        }
    }

    #[test]
    fn the_registry_entry_follows_the_gpe0_block() {
        // Registering it is what makes the AcpiGpe reset and migration
        // hooks reachable. A VM without hotplug has no block, so it
        // registers nothing and its device list does not change.
        let registry = DeviceRegistry::new();
        register_gpe0(&registry, None).expect("nothing to register");
        assert!(registry.is_empty());

        let attached = attach(opts(true, 2, 8));
        let block = attached.gpe.expect("hotplug is on");
        register_gpe0(&registry, Some(&block)).expect("one entry");

        let entry = registry.get_by_id("acpi-gpe").expect("registered");
        assert!(entry.bdf.is_none(), "the GPE0 block is not on the PCI bus");
        assert_eq!(registry.lifecycle_devices().len(), 1);
        assert_eq!(
            registry.lifecycle_devices()[0].type_name(),
            "acpi-gpe",
            "the registry must hold the block itself",
        );
    }
}
