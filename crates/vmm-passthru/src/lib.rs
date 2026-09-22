// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PCI passthrough (PPT) device.
//!
//! Gives a real PCI function to the guest. MMIO BARs are mapped into
//! guest physical address space with EPT (`VM_MAP_PPTDEV_MMIO`), so
//! guest MMIO reaches the hardware with no VM exit. I/O BARs are
//! trapped on the PIO bus and relayed through `PPT_BAR_READ` and
//! `PPT_BAR_WRITE`. Config space is interposed, deny by default: the
//! command register, the BARs, the interrupt line, the header type and
//! the MSI capability have a handler, and every other write is dropped.
//! The MSI capability is emulated in full (see [`msi`]). Only its
//! shadow values reach the kernel through `VM_PPTDEV_MSI`.
//!
//! # Limits
//!
//! **An MSI-X capable device is refused.** `apply_bar` maps a full BAR,
//! and on an MSI-X device that BAR holds the physical MSI-X table.
//! illumos VT-d remaps DMA but not interrupts, so a guest that writes
//! that table makes the device raise a guest-chosen vector on the host
//! APIC. Linux gates the same hole behind `allow_unsafe_interrupts`.
//! This crate refuses the device instead.
//!
//! **An MMIO BAR smaller than a page is refused.** The kernel maps
//! whole pages, and a sub-page BAR shares its page with whatever the
//! platform placed next to it. The PPT driver relays only I/O BARs, so
//! such a BAR cannot be trapped either.

use std::io::{Error, ErrorKind, Result};
use std::sync::{Arc, Mutex, Weak};

use bhyve_api::{PCI_ADDR_IO, PCI_ADDR_MEM32, PCI_ADDR_MEM64};
use slog::{warn, Logger};

use vmm_core::common::RWOp;
use vmm_core::hdl::VmmHdl;
use vmm_core::pio::{PioBus, PioFn};

use vmm_devices::pci::bar::{BarDefine, BAR_COUNT};
use vmm_devices::pci::device::{DeviceIdent, DeviceState, PciDevice};
use vmm_devices::pci::BarN;

mod cfg;
mod msi;
mod ppt;

use cfg::{
    check_passthrough_supported, classify_cfg_write, scan_capabilities,
    CapLayout, CfgWrite,
};
use msi::{MsiCap, MsiProgram};
use ppt::{BarInfo, KernelPpt, PptOps};

/// MSI capability: Message Control register offset within capability.
const MSI_MSG_CTRL_OFF: u8 = 2;
/// MSI Message Control: MSI Enable.
const MSI_MSG_CTRL_ENABLE: u16 = 1 << 0;
/// MSI Message Control: 64-bit address capable.
const MSI_MSG_CTRL_64BIT: u16 = 1 << 7;
/// MSI Message Control: per-vector masking capable.
const MSI_MSG_CTRL_PVM: u16 = 1 << 8;

const PCI_CMD_IO_EN: u16 = 1 << 0;
const PCI_CMD_MMIO_EN: u16 = 1 << 1;

const CFG_COMMAND: u8 = 0x04;
const CFG_STATUS: u8 = 0x06;
const CFG_HEADER_TYPE: u8 = 0x0E;
const CFG_BAR0: u8 = 0x10;
const CFG_BAR5: u8 = 0x24;
const CFG_CAP_PTR: usize = 0x34;
const CFG_INTERRUPT_LINE: u8 = 0x3C;
const CFG_INTERRUPT_PIN: usize = 0x3D;
/// The first offset a capability may live at.
const CFG_CAP_FIRST: u8 = 0x40;

/// The kernel maps BARs in whole pages.
const PAGE_SIZE: u64 = 4096;

/// Everything a guest access can change, under one lock so a command
/// write and a BAR write from two vCPUs cannot interleave.
struct Emulated {
    /// Standard header: BAR addresses, interrupt line, header type.
    state: DeviceState,
    /// The full command register as the guest wrote it.
    command: u16,
    /// Guest windows the kernel holds mapped, by BAR: `(gpa, len)`.
    mapped: [Option<(u64, u64)>; BAR_COUNT],
    /// Ports registered on the PIO bus, by BAR.
    ports: [Option<u16>; BAR_COUNT],
    msi: Option<MsiCap>,
    /// What the kernel last accepted through `VM_PPTDEV_MSI`.
    msi_programmed: MsiProgram,
}

/// PCI passthrough device.
pub struct PciPassthru {
    ppt: Box<dyn PptOps>,
    /// Physical BAR layout from the kernel, fixed for the device's life.
    bars: [BarInfo; BAR_COUNT],
    /// Physical config space at bind time.
    phys_cfg: [u8; 256],
    caps: CapLayout,
    bus_pio: Arc<PioBus>,
    /// For the PIO handlers, which must not keep the device alive.
    weak: Weak<Self>,
    inner: Mutex<Emulated>,
    log: Logger,
}

impl PciPassthru {
    /// Open `ppt_path` (such as `/dev/ppt0`), bind it to the VM and
    /// build the guest-visible device.
    ///
    /// Returns [`ErrorKind::Unsupported`] for a device this VMM cannot
    /// isolate. See the module documentation.
    pub fn new(
        ppt_path: &str,
        hdl: Arc<VmmHdl>,
        bus_pio: Arc<PioBus>,
        log: Logger,
    ) -> Result<Arc<Self>> {
        let ppt = KernelPpt::open(ppt_path, hdl, log.clone())?;
        Self::attach(Box::new(ppt), bus_pio, log)
    }

    /// Build the device over any backend. A failure drops the backend,
    /// which gives the physical device back to the host.
    fn attach(
        ppt: Box<dyn PptOps>,
        bus_pio: Arc<PioBus>,
        log: Logger,
    ) -> Result<Arc<Self>> {
        let limits = ppt.limits()?;

        let mut phys_cfg = [0u8; 256];
        for off in (0..256usize).step_by(4) {
            let val = ppt.cfg_read(off as u8, 4)?;
            phys_cfg[off..off + 4].copy_from_slice(&val.to_le_bytes());
        }

        // Refuse the device before any BAR is mapped or any interrupt
        // resource is claimed.
        let caps = scan_capabilities(&phys_cfg);
        check_passthrough_supported(&caps)?;

        let mut bars = [BarInfo::default(); BAR_COUNT];
        for (i, bar) in bars.iter_mut().enumerate() {
            *bar = ppt.bar_query(i)?.unwrap_or_default();
        }

        let mut state = DeviceState::new(DeviceIdent {
            vendor_id: u16::from_le_bytes([phys_cfg[0], phys_cfg[1]]),
            device_id: u16::from_le_bytes([phys_cfg[2], phys_cfg[3]]),
            revision: phys_cfg[8],
            prog_if: phys_cfg[9],
            subclass: phys_cfg[0x0A],
            class: phys_cfg[0x0B],
            sub_vendor_id: u16::from_le_bytes([phys_cfg[0x2C], phys_cfg[0x2D]]),
            sub_device_id: u16::from_le_bytes([phys_cfg[0x2E], phys_cfg[0x2F]]),
        });
        for (i, info) in bars.iter().enumerate() {
            if let Some(def) = bar_define(i, info)? {
                let bar_n = BarN::try_from(i as u8)
                    .map_err(|_| Error::from(ErrorKind::InvalidData))?;
                state.define_bar(bar_n, def);
            }
        }
        state.set_cap_ptr(phys_cfg[CFG_CAP_PTR]);
        state.set_intr_pin(phys_cfg[CFG_INTERRUPT_PIN]);
        state.set_intr_line(phys_cfg[usize::from(CFG_INTERRUPT_LINE)]);
        state.cfg_write(
            CFG_HEADER_TYPE,
            1,
            u32::from(phys_cfg[usize::from(CFG_HEADER_TYPE)]),
        );

        let msi = MsiCap::from_cfg(
            &phys_cfg,
            caps.msi_cap_off,
            caps.msi_cap_len,
            limits.msi,
        );

        Ok(Arc::new_cyclic(|weak| Self {
            ppt,
            bars,
            phys_cfg,
            caps,
            bus_pio,
            weak: weak.clone(),
            inner: Mutex::new(Emulated {
                state,
                command: 0,
                mapped: [None; BAR_COUNT],
                ports: [None; BAR_COUNT],
                msi,
                msi_programmed: MsiProgram::default(),
            }),
            log,
        }))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Emulated> {
        self.inner.lock().expect("passthru lock")
    }

    fn cached_dword(&self, dword_off: u8) -> u32 {
        let i = usize::from(dword_off);
        u32::from_le_bytes([
            self.phys_cfg[i],
            self.phys_cfg[i + 1],
            self.phys_cfg[i + 2],
            self.phys_cfg[i + 3],
        ])
    }

    // ── BAR windows ────────────────────────────────────────────────

    /// Bring the kernel window or PIO registration for one BAR in line
    /// with the guest's BAR address and decode enables.
    ///
    /// A window that will not close is left recorded and nothing new
    /// is opened for that BAR, so the guest never has two.
    fn apply_bar(&self, inner: &mut Emulated, idx: usize) {
        let info = self.bars[idx];
        let Ok(bar_n) = BarN::try_from(idx as u8) else {
            return;
        };
        let Some((_def, addr)) = inner.state.bars().get(bar_n) else {
            return;
        };
        let enabled = match info.bar_type {
            PCI_ADDR_IO => inner.command & PCI_CMD_IO_EN != 0,
            PCI_ADDR_MEM32 | PCI_ADDR_MEM64 => {
                inner.command & PCI_CMD_MMIO_EN != 0
            }
            _ => false,
        };
        let want = (enabled && addr != 0).then_some(addr);

        if info.bar_type == PCI_ADDR_IO {
            let have = inner.ports[idx];
            let want = want.and_then(|a| u16::try_from(a).ok());
            if have == want {
                return;
            }
            if let Some(port) = have {
                if let Err(e) = self.bus_pio.unregister(port) {
                    warn!(self.log, "passthru: PIO BAR unregister failed";
                        "bar" => idx, "port" => format!("{port:#x}"),
                        "error" => %e);
                    return;
                }
                inner.ports[idx] = None;
            }
            if let Some(port) = want {
                let Ok(len) = u16::try_from(info.size) else {
                    return;
                };
                let weak = self.weak.clone();
                let handler: Arc<PioFn> = Arc::new(move |offset, rwo| {
                    if let Some(dev) = weak.upgrade() {
                        dev.bar_rw(bar_n, usize::from(offset), rwo);
                    }
                });
                match self.bus_pio.register(port, len, handler) {
                    Ok(()) => inner.ports[idx] = Some(port),
                    Err(e) => {
                        warn!(self.log, "passthru: PIO BAR register failed";
                            "bar" => idx, "port" => format!("{port:#x}"),
                            "error" => %e);
                    }
                }
            }
            return;
        }

        let have = inner.mapped[idx];
        let want = want.map(|gpa| (gpa, info.size));
        if have == want {
            return;
        }
        if let Some((gpa, len)) = have {
            if let Err(e) = self.ppt.unmap_mmio(gpa, len) {
                warn!(self.log, "passthru: BAR unmap failed, window still open";
                    "bar" => idx, "gpa" => format!("{gpa:#x}"), "len" => len,
                    "error" => %e);
                return;
            }
            inner.mapped[idx] = None;
        }
        if let Some((gpa, len)) = want {
            match self.ppt.map_mmio(gpa, info.hpa, len) {
                Ok(()) => inner.mapped[idx] = Some((gpa, len)),
                Err(e) => {
                    warn!(self.log, "passthru: BAR map failed, guest cannot \
                        reach the device";
                        "bar" => idx, "gpa" => format!("{gpa:#x}"), "len" => len,
                        "error" => %e);
                }
            }
        }
    }

    fn apply_all_bars(&self, inner: &mut Emulated) {
        for idx in 0..BAR_COUNT {
            self.apply_bar(inner, idx);
        }
    }

    /// Close every window and registration. Idempotent.
    fn release_regions(&self, inner: &mut Emulated) {
        inner.command &= !(PCI_CMD_IO_EN | PCI_CMD_MMIO_EN);
        self.apply_all_bars(inner);
    }

    // ── Config-space write handlers ────────────────────────────────

    /// Merge a guest write into the command register.
    ///
    /// Bus mastering and the two decode enables must reach the device,
    /// so this is the one header register written through.
    fn cfg_write_command(&self, offset: u8, len: u8, val: u32) {
        let mut inner = self.lock();
        let new_cmd =
            merge_bytes(u32::from(inner.command), offset, len, val) as u16;
        inner.command = new_cmd;

        if let Err(e) = self.ppt.cfg_write(CFG_COMMAND, 2, u32::from(new_cmd)) {
            warn!(self.log, "passthru: command register write failed";
                "value" => format!("{new_cmd:#06x}"), "error" => %e);
        }
        self.apply_all_bars(&mut inner);
    }

    /// Apply a guest BAR write to the emulated header, then remap.
    ///
    /// The value never reaches the hardware BAR. The device keeps the
    /// host physical address the platform gave it.
    fn cfg_write_bar(&self, offset: u8, len: u8, val: u32) {
        let Ok(bar_n) = BarN::try_from(((offset & 0xFC) - CFG_BAR0) / 4) else {
            return;
        };
        let mut inner = self.lock();
        let current = inner.state.bars().reg_read(bar_n);
        let merged = merge_bytes(current, offset, len, val);
        let Some(result) = inner.state.bar_write(bar_n, merged) else {
            return;
        };
        self.apply_bar(&mut inner, result.bar as usize);
    }

    /// Update the MSI shadow and, when the effective values change,
    /// the kernel. Nothing here touches the physical capability.
    fn cfg_write_msi_cap(&self, offset: u8, len: u8, val: u32) {
        let mut inner = self.lock();
        let Some(msi) = inner.msi.as_mut() else {
            return;
        };
        msi.write(offset, len, val);
        let want = msi.program();
        if want == inner.msi_programmed {
            return;
        }
        match self.ppt.setup_msi(want.addr, want.data, want.numvec) {
            Ok(()) => inner.msi_programmed = want,
            Err(e) => {
                warn!(self.log, "passthru: MSI setup failed, device keeps \
                    its previous interrupt state";
                    "numvec" => want.numvec, "error" => %e);
            }
        }
    }
}

impl PciDevice for PciPassthru {
    fn cfg_read(&self, offset: u8, len: u8) -> u32 {
        let dword_off = offset & 0xFC;
        let dword = match dword_off {
            // Identity and class come from the bind-time cache.
            0x00 | 0x08 | 0x2C => self.cached_dword(dword_off),
            // Status is live hardware state. The command is what the
            // guest wrote.
            CFG_COMMAND => {
                let cmd = self.lock().command;
                let status = self.ppt.cfg_read(CFG_STATUS, 2).unwrap_or(0);
                u32::from(cmd) | (status << 16)
            }
            // Header type is emulated, because the bus sets its
            // multi-function bit. The other three bytes are static
            // hardware values.
            0x0C => {
                let header = self.lock().state.cfg_read(dword_off, 4);
                (self.cached_dword(dword_off) & 0xFF00_FFFF)
                    | (header & 0x00FF_0000)
            }
            CFG_BAR0..=CFG_BAR5 => self.lock().state.cfg_read(dword_off, 4),
            // Cardbus CIS, Expansion ROM and the reserved dword. The
            // ROM BAR holds a host physical address, and no window is
            // ever opened for it.
            0x28 | 0x30 | 0x38 => 0,
            0x34 => u32::from(self.phys_cfg[CFG_CAP_PTR]),
            // Line and pin are emulated. Min_Gnt and Max_Lat are static.
            0x3C => {
                let header = self.lock().state.cfg_read(dword_off, 4);
                (self.cached_dword(dword_off) & 0xFFFF_0000)
                    | (header & 0x0000_FFFF)
            }
            _ if cfg::in_cap(offset, &self.caps) => {
                return self
                    .lock()
                    .msi
                    .as_ref()
                    .map_or(0, |msi| msi.read(offset, len));
            }
            // Anything else reads the real device. A config read has
            // no side effect on the host.
            _ => {
                return self.ppt.cfg_read(offset, len).unwrap_or(0xFFFF_FFFF);
            }
        };
        extract_bytes(dword, offset, len)
    }

    fn cfg_write(&self, offset: u8, len: u8, val: u32) {
        match classify_cfg_write(offset, len, &self.caps) {
            CfgWrite::Command => self.cfg_write_command(offset, len, val),
            CfgWrite::Bar => self.cfg_write_bar(offset, len, val),
            CfgWrite::MsiCap => self.cfg_write_msi_cap(offset, len, val),
            CfgWrite::Header => self.lock().state.cfg_write(offset, len, val),
            CfgWrite::Deny => {
                warn!(self.log, "passthru: config write dropped";
                    "offset" => format!("{offset:#04x}"),
                    "len" => len,
                    "value" => format!("{val:#010x}"));
            }
        }
    }

    /// Relay a trapped I/O BAR access. MMIO BARs never reach here:
    /// EPT maps them straight to hardware.
    fn bar_rw(&self, bar: BarN, offset: usize, rwo: RWOp<'_>) {
        let idx = bar as usize;
        let info = self.bars[idx];
        let width = rwo.len();
        let in_range = info.bar_type == PCI_ADDR_IO
            && offset.checked_add(width).is_some_and(|end| {
                u64::try_from(end).is_ok_and(|e| e <= info.size)
            });
        let Ok(offset) = u32::try_from(offset) else {
            return;
        };
        let width_u8 = width as u8;

        match rwo {
            RWOp::Read(ro) => {
                let val = if in_range {
                    self.ppt.bar_read(idx, offset, width_u8).unwrap_or(u32::MAX)
                } else {
                    // An undecoded PCI read returns all ones.
                    u32::MAX
                };
                match width {
                    1 => ro.write_u8(val as u8),
                    2 => ro.write_u16(val as u16),
                    _ => ro.write_u32(val),
                }
            }
            RWOp::Write(wo) => {
                if !in_range {
                    return;
                }
                let data = wo.read_dword();
                if let Err(e) = self.ppt.bar_write(idx, offset, width_u8, data)
                {
                    warn!(self.log, "passthru: BAR I/O write failed";
                        "bar" => idx, "off" => offset, "error" => %e);
                }
            }
        }
    }

    fn detach_regions(&self) {
        let mut inner = self.lock();
        self.release_regions(&mut inner);
    }
}

impl Drop for PciPassthru {
    fn drop(&mut self) {
        // The windows go before the backend's unbind.
        let mut inner = self.lock();
        self.release_regions(&mut inner);
    }
}

/// The emulated BAR for a physical one, or an error for a BAR this
/// VMM cannot give to a guest.
fn bar_define(idx: usize, info: &BarInfo) -> Result<Option<BarDefine>> {
    if info.size == 0 {
        return Ok(None);
    }
    let unsupported = |why: &str| {
        Error::new(ErrorKind::Unsupported, format!("BAR{idx}: {why}"))
    };
    if !info.size.is_power_of_two() {
        return Err(unsupported("size is not a power of two"));
    }
    match info.bar_type {
        PCI_ADDR_IO => {
            let size = u16::try_from(info.size)
                .map_err(|_| unsupported("I/O BAR is larger than 64 KiB"))?;
            Ok(Some(BarDefine::Pio(size)))
        }
        PCI_ADDR_MEM32 | PCI_ADDR_MEM64 => {
            if info.size < PAGE_SIZE || !info.hpa.is_multiple_of(PAGE_SIZE) {
                return Err(unsupported(
                    "MMIO BAR is smaller than a page and cannot be \
                     mapped without exposing its neighbours",
                ));
            }
            if info.bar_type == PCI_ADDR_MEM32 {
                let size = u32::try_from(info.size).map_err(|_| {
                    unsupported("32-bit BAR is larger than 4 GiB")
                })?;
                Ok(Some(BarDefine::Mmio(size)))
            } else {
                Ok(Some(BarDefine::Mmio64(info.size)))
            }
        }
        _ => Err(unsupported("unknown BAR type")),
    }
}

/// Merge a 1, 2 or 4 byte write at `offset` into a dword.
fn merge_bytes(current: u32, offset: u8, len: u8, val: u32) -> u32 {
    let shift = u32::from(offset & 0x03) * 8;
    let mask = size_mask(len) << shift;
    (current & !mask) | ((val << shift) & mask)
}

/// Extract bytes from a dword value based on the access offset and length.
fn extract_bytes(dword: u32, offset: u8, len: u8) -> u32 {
    let byte_off = u32::from(offset & 0x03);
    (dword >> (byte_off * 8)) & size_mask(len)
}

fn size_mask(len: u8) -> u32 {
    match len {
        1 => 0xFF,
        2 => 0xFFFF,
        _ => 0xFFFF_FFFF,
    }
}

#[cfg(test)]
mod tests;
