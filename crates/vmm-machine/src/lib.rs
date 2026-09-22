// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Machine plumbing shared by every VMM binary in this workspace.
//!
//! This crate holds the phase BODIES: guest memory, chipset, PCI bus,
//! interrupt routing, firmware table emission, vCPU spawn, event loop
//! and teardown. It names no concrete PCI driver type. Each binary keeps
//! its own `run()` naming the phase SEQUENCE, because that ordering is
//! load-bearing.

pub mod build;
pub mod chipset;
pub mod devspec;
pub mod entry;
pub mod firmware;
pub mod hotplug;
pub mod inventory;
pub mod opts;
pub mod parse;
pub mod pause;
pub mod pci;
pub mod pm;
pub mod provision;
pub mod registry;
pub mod signal;
pub mod teardown;
pub mod vcpu;
pub mod vcpu_tasks;
pub mod vcpus;

pub use build::{
    build_machine, host_tsc_frequency_hz, no_pre_finalize, MachineBuild,
    MachineOpts,
};
pub use chipset::{attach_uart, init_chipset, ChipsetDevices};
pub use firmware::{
    setup_fwcfg_and_acpi, write_legacy_acpi_tables, write_smbios_tables,
    AcpiTables,
};
pub use hotplug::cpu::{CpuHotplugEngine, CpuHotplugError, CpuOnline};
pub use hotplug::mem::hot_add_window;
pub use hotplug::mem::{
    MemHotplugEngine, MemHotplugError, MemWindow, DEFAULT_SLOT_SIZE,
};
pub use hotplug::{
    hotplug_specs, start_cpu_hotplug, CpuSlots, HotplugEngine, HotplugEngines,
    HotplugError, HotplugFactory,
};
pub use opts::VmOpts;
pub use parse::{find_lpc_device, parse_bdf};
pub use pause::{VmPause, VmPauseGate};
pub use pci::{
    attach_pci_devices, created, CreatedPciDevice, LifecycleHandle,
    PciAttachment, PciDeviceCtx, PciDeviceFactory, PciDeviceHandle,
};
pub use pm::{
    mem_hotplug_window, register_pm_devices, setup_timers_and_pm,
    start_mem_hotplug, Gpe0, HotplugOpts, HotplugRegisters, TimerAndPmDevices,
};
pub use registry::{
    DeviceRegistry, RegisteredDevice, RegistryError, SlotState,
};
pub use signal::{
    install_sigterm_handler, sigterm_flag, sigterm_received, SUSPEND_SOURCE_VMM,
};
pub use teardown::{
    registry_named_devices, run_fleet_event_loop, RunOutcome, VcpuThreads,
};
pub use vcpu::{
    activate_one, activate_uefi_bsp_only, activate_vcpus,
    check_aps_awaiting_sipi, set_halt_exit, spawn_vcpu_fleet,
    uefi_reset_targets, BspEntry, VcpuFleet,
};
pub use vcpu_tasks::{
    spawn_boot_threads, spawn_one, start_gate, StartGate, StartSignal,
    VcpuEvent, VcpuThreadCtx,
};
pub use vcpus::{VcpuError, VcpuRegistry, VcpuSetup};
