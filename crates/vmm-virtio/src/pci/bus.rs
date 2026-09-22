// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Where this device sits on the PIO and MMIO buses, and the accesses
//! that arrive there.
//!
//! The guest moves the device with a config-space write to a BAR or to
//! the command register. `update_bar_registration` and
//! `update_mmio_registration` apply that write. `registered_bar`,
//! `registered_bar2` and `registered_bar4` record what is on the buses
//! now, and must match the buses exactly. A registration left after
//! the guest moves a BAR keeps decoding an address that the guest gave
//! to another device. So an update releases before it registers, and
//! holds the record across both. A failed register leaves the BAR
//! unmapped and logs it, because the alternative is a guest-triggered
//! panic.
//!
//! Accesses arrive at `bar_rw`, which routes BAR0 to the legacy
//! register file, BAR2 to the modern one and BAR4 to the MSI-X table
//! and PBA. `cfg_read` and `cfg_write` serve config space and try the
//! capability chain first. The chain works one dword at a time, so a
//! write narrower than a dword is read, merged and written whole. Raw
//! bytes put a field such as MSI-X Message Control at the wrong bit
//! position.

use std::sync::Arc;

use vmm_core::common::RWOp;
use vmm_core::mmio::MmioFn;
use vmm_core::pio::PioFn;

use vmm_devices::pci::bar::BarDefine;
use vmm_devices::pci::device::{DeviceState, PciDevice};
use vmm_devices::pci::BarN;

use super::{VirtioDevice, VirtioPciDevice};

impl<D: VirtioDevice> VirtioPciDevice<D> {
    /// Unregister the recorded BAR0 PIO base and clear the record.
    fn release_pio(&self, reg: &mut Option<u16>) {
        if let Some(port) = reg.take() {
            if let Err(e) = self.bus_pio.unregister(port) {
                slog::warn!(self.log, "virtio-pci: PIO unregister failed";
                    "bar" => ?BarN::BAR0,
                    "port" => format!("{port:#x}"),
                    "error" => %e);
            }
        }
    }

    /// Unregister a recorded MMIO base and clear the record.
    fn release_mmio(&self, bar: BarN, reg: &mut Option<u64>) {
        if let Some(addr) = reg.take() {
            if let Err(e) = self.bus_mmio.unregister(addr) {
                slog::warn!(self.log, "virtio-pci: MMIO unregister failed";
                    "bar" => ?bar,
                    "addr" => format!("{addr:#x}"),
                    "error" => %e);
            }
        }
    }

    /// Update the PIO bus registration for BAR0 from the current
    /// config state.
    fn update_bar_registration(self: &Arc<Self>, pci: &DeviceState) {
        let io_enabled = pci
            .command()
            .contains(vmm_devices::pci::bits::RegCmd::IO_EN);
        let bar_info = pci.bars().get(BarN::BAR0);

        let mut reg = self.registered_bar.lock().expect("bar lock poisoned");
        self.release_pio(&mut reg);

        if io_enabled {
            if let Some((BarDefine::Pio(size), addr)) = bar_info {
                let port = addr as u16;
                if port != 0 {
                    let dev = Arc::clone(self);
                    let handler: Arc<PioFn> =
                        Arc::new(move |offset: u16, rwo: RWOp<'_>| {
                            dev.bar_rw(BarN::BAR0, offset as usize, rwo);
                        });
                    match self.bus_pio.register(port, size, handler) {
                        Ok(()) => *reg = Some(port),
                        // The device now decodes nothing on BAR0.
                        Err(e) => {
                            slog::error!(self.log,
                                "virtio-pci: PIO register failed, BAR0 dark";
                                "bar" => ?BarN::BAR0,
                                "port" => format!("{port:#x}"),
                                "size" => size,
                                "error" => %e);
                        }
                    }
                }
            }
        }
    }

    /// Update MMIO bus registration for BAR2 and BAR4.
    fn update_mmio_registration(self: &Arc<Self>, pci: &DeviceState) {
        let mmio_enabled = pci
            .command()
            .contains(vmm_devices::pci::bits::RegCmd::MMIO_EN);

        // BAR2: modern transport config
        {
            let mut reg = self.registered_bar2.lock().expect("bar2 lock");
            self.release_mmio(BarN::BAR2, &mut reg);
            if mmio_enabled {
                self.register_mmio(BarN::BAR2, pci, &mut reg);
            }
        }

        // BAR4: MSI-X table + PBA
        if self.msix.is_some() {
            let mut reg = self.registered_bar4.lock().expect("bar4 lock");
            self.release_mmio(BarN::BAR4, &mut reg);
            if mmio_enabled {
                self.register_mmio(BarN::BAR4, pci, &mut reg);
            }
        }
    }

    /// Register one MMIO BAR and record its base on success.
    fn register_mmio(
        self: &Arc<Self>,
        bar: BarN,
        pci: &DeviceState,
        reg: &mut Option<u64>,
    ) {
        let Some((def, addr)) = pci.bars().get(bar) else {
            return;
        };
        if addr == 0 || !def.is_mmio() {
            return;
        }
        let size = def.size();
        let dev = Arc::clone(self);
        let handler: Arc<MmioFn> =
            Arc::new(move |offset: usize, rwo: RWOp<'_>| {
                dev.bar_rw(bar, offset, rwo);
            });
        match self.bus_mmio.register(addr, size, handler) {
            Ok(()) => *reg = Some(addr),
            // The device now decodes nothing on this BAR.
            Err(e) => {
                slog::error!(self.log,
                    "virtio-pci: MMIO register failed, BAR dark";
                    "bar" => ?bar,
                    "addr" => format!("{addr:#x}"),
                    "size" => size,
                    "error" => %e);
            }
        }
    }
}

impl<D: VirtioDevice> PciDevice for VirtioPciDevice<D> {
    fn cfg_read(&self, offset: u8, len: u8) -> u32 {
        // cap_chain_read works on aligned dwords, so align and then
        // extract the requested bytes.
        if offset >= 0x40 {
            let dword_off = offset & 0xFC;
            let byte_off = (offset & 0x03) as u32;
            if let Some(dword_val) = self.cap_chain_read(dword_off) {
                let mask = match len {
                    1 => 0xFF,
                    2 => 0xFFFF,
                    _ => 0xFFFF_FFFF,
                };
                return (dword_val >> (byte_off * 8)) & mask;
            }
        }
        self.pci_state
            .lock()
            .expect("pci lock poisoned")
            .cfg_read(offset, len)
    }

    fn cfg_write(&self, offset: u8, len: u8, val: u32) {
        // A sub-dword write is read, merged and passed as a full dword,
        // so cap_write sees each field at its bit position (for
        // example Message Control at bits 31:16).
        if offset >= 0x40 {
            let dword_off = offset & 0xFC;
            let byte_in_dword = (offset & 0x03) as u32;
            let dword_val = if len < 4 {
                let cur = self.cap_chain_read(dword_off).unwrap_or(0);
                let shift = byte_in_dword * 8;
                let mask = match len {
                    1 => 0xFFu32 << shift,
                    2 => 0xFFFFu32 << shift,
                    _ => 0xFFFF_FFFFu32,
                };
                (cur & !mask) | ((val << shift) & mask)
            } else {
                val
            };
            self.cap_chain_write(dword_off, dword_val);
        }
        let mut pci = self.pci_state.lock().expect("pci lock poisoned");
        pci.cfg_write(offset, len, val);

        // A write to a BAR or the command register can move the
        // device on the buses.
        let dword_off = offset & 0xFC;
        if dword_off == 0x04 || (0x10..=0x24).contains(&dword_off) {
            let weak = self.self_ref.lock().expect("self_ref lock");
            if let Some(arc_self) = weak.as_ref().and_then(|w| w.upgrade()) {
                drop(pci);
                let pci_ref = arc_self.pci_state.lock().expect("pci lock");
                arc_self.update_bar_registration(&pci_ref);
                arc_self.update_mmio_registration(&pci_ref);
            }
        }
    }

    fn bar_rw(&self, bar: BarN, offset: usize, rwo: RWOp<'_>) {
        // BAR4: MSI-X table + PBA access
        if bar == BarN::BAR4 {
            if let Some(ref msix) = self.msix {
                let pba_offset = msix.pba_offset();
                match rwo {
                    RWOp::Read(ro) => {
                        let val = if offset >= pba_offset {
                            msix.pba_read(offset - pba_offset)
                        } else {
                            msix.table_read(offset)
                        };
                        ro.write_dword(val)
                    }
                    RWOp::Write(wo) => {
                        let val = wo.read_dword();
                        if offset < pba_offset {
                            // Sample before the write removes the
                            // message from the pending array.
                            let session = self.intr.session();
                            let released =
                                msix.table_write_deferred(offset, val);
                            self.deliver_released(session, msix, released);
                        }
                    }
                }
            }
            return;
        }

        // BAR2: Modern transport config (MMIO)
        if bar == BarN::BAR2 {
            match rwo {
                RWOp::Read(ro) => {
                    let val = self.modern_bar_read(offset as u16, ro.len());
                    ro.write_dword(val)
                }
                RWOp::Write(wo) => {
                    let val = wo.read_dword();
                    self.modern_bar_write(offset as u16, val, wo.len());
                }
            }
            return;
        }

        // BAR0: Legacy transport config (PIO)
        if bar != BarN::BAR0 {
            return;
        }

        match rwo {
            RWOp::Read(ro) => {
                let val = self.bar_read(offset as u16, ro.len());
                ro.write_dword(val)
            }
            RWOp::Write(wo) => {
                let val = wo.read_dword();
                self.bar_write(offset as u16, val, wo.len());
            }
        }
    }

    fn detach_regions(&self) {
        {
            let mut reg =
                self.registered_bar.lock().expect("bar lock poisoned");
            self.release_pio(&mut reg);
        }
        {
            let mut reg = self.registered_bar2.lock().expect("bar2 lock");
            self.release_mmio(BarN::BAR2, &mut reg);
        }
        {
            let mut reg = self.registered_bar4.lock().expect("bar4 lock");
            self.release_mmio(BarN::BAR4, &mut reg);
        }
    }
}
