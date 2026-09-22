// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! i440fx chipset and the bhyve kernel-emulated device set.

use std::sync::Arc;

use anyhow::Context;
use slog::{info, Logger};
use vmm_core::intr_pins::LegacyPIC;
use vmm_core::machine::Machine;
use vmm_devices::bhyve::{
    BhyveAtPic, BhyveAtPit, BhyveHpet, BhyveIoApic, BhyveRtc,
};
use vmm_devices::chipset::i440fx::I440FxChipset;
use vmm_devices::uart::lpc::LpcUart;

/// Chipset and kernel-emulated device handles that must be kept alive
/// for the VM's lifetime.
pub struct ChipsetDevices {
    pub chipset: Arc<I440FxChipset>,
    pub pic: Arc<LegacyPIC>,
    pub atpic: Arc<BhyveAtPic>,
    pub atpit: Arc<BhyveAtPit>,
    pub hpet: Arc<BhyveHpet>,
    pub ioapic: Arc<BhyveIoApic>,
    pub rtc: Arc<BhyveRtc>,
}

/// Create the i440fx chipset, legacy PIC, and all bhyve kernel-emulated
/// devices (ATPIC, ATPIT, HPET, IOAPIC, RTC). Writes memory size to
/// RTC NVRAM and sets RTC time.
pub fn init_chipset(
    machine: &Machine,
    log: &Logger,
) -> anyhow::Result<ChipsetDevices> {
    let pic = LegacyPIC::new(
        machine.hdl().clone(),
        log.new(slog::o!("component" => "legacy-pic")),
    );

    let chipset = I440FxChipset::create(
        machine.bus_pio(),
        Some(machine.hdl().clone()),
        log.new(slog::o!("component" => "chipset")),
    );
    info!(log, "chipset initialized"; "type" => "i440fx");

    // The kernel emulates these. The handles only tie them to the VM.
    let atpic = BhyveAtPic::create();
    let atpit = BhyveAtPit::create();
    let hpet = BhyveHpet::create();
    let ioapic = BhyveIoApic::create();
    let rtc = BhyveRtc::create(machine.hdl().clone());

    // UEFI reads the memory size back out of RTC NVRAM.
    let (lowmem, highmem) = vmm_core::machine::split_memory(machine.mem_size());
    rtc.memsize_to_nvram(lowmem as u32, highmem as u64)
        .context("failed to write memory size to RTC NVRAM")?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    machine
        .hdl()
        .rtc_settime(now)
        .context("failed to set RTC time")?;
    info!(log, "RTC NVRAM configured";
        "lowmem_mb" => lowmem / (1024 * 1024),
        "highmem_mb" => highmem / (1024 * 1024),
    );

    Ok(ChipsetDevices {
        chipset,
        pic,
        atpic,
        atpit,
        hpet,
        ioapic,
        rtc,
    })
}

/// Attach one LPC UART at `base` on `irq`, with no backend yet.
///
/// An IRQ the PIC cannot route gets a no-op pin, so the port still
/// exists for a guest that polls it.
pub fn attach_uart(
    machine: &Machine,
    pic: &Arc<LegacyPIC>,
    base: u16,
    irq: u8,
) -> Arc<LpcUart> {
    let uart = LpcUart::new(pic.pin_or_noop(irq));
    uart.attach(machine.bus_pio(), base);
    uart
}
