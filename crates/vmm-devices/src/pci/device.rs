// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PCI device trait and standard Type 0 config space handler.
//!
//! [`DeviceState`] manages the standard 64-byte configuration header.

use vmm_core::common::RWOp;

use super::bar::{BarDefine, BarWriteResult, Bars};
use super::bits;
use super::BarN;
use crate::lifecycle::MigratePciState;

/// An emulated PCI device.
///
/// [`DeviceState`] handles the standard header fields. Devices add only
/// their own behavior.
pub trait PciDevice: Send + Sync + 'static {
    /// `offset` is 0x00..0xFF and `len` is 1, 2 or 4. The value is in
    /// the low bits.
    fn cfg_read(&self, offset: u8, len: u8) -> u32;

    /// `val` is in the low bits.
    fn cfg_write(&self, offset: u8, len: u8, val: u32);

    /// `offset` is relative to the start of `bar`.
    fn bar_rw(&self, bar: BarN, offset: usize, rwo: RWOp<'_>);

    /// Unregister every bus region this device has registered.
    ///
    /// A BAR handler holds a strong reference to its device, and the
    /// device holds the bus, so the device cannot drop while a handler
    /// is registered. Call this before you release the last handle to
    /// the device, for example on hot-unplug or VM teardown.
    ///
    /// Each implementation must clear the recorded bases, so a second
    /// call does nothing.
    fn detach_regions(&self) {}
}

/// Replay a migrated PCI header: the BAR writes from
/// [`DeviceState::bar_replay_writes`], then the command register, so
/// the bus registrations come back in the order the guest made them.
pub fn replay_migrate_pci_state(
    dev: &dyn PciDevice,
    bar_writes: &[(u8, u32)],
    command: u16,
) {
    for &(offset, value) in bar_writes {
        dev.cfg_write(offset, 4, value);
    }
    dev.cfg_write(bits::REG_COMMAND, 2, u32::from(command));
}

/// Static identification fields.
#[derive(Debug, Clone, Default)]
pub struct DeviceIdent {
    pub vendor_id: u16,
    pub device_id: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub revision: u8,
    pub sub_vendor_id: u16,
    pub sub_device_id: u16,
}

/// The standard PCI Type 0 configuration header (64 bytes).
///
/// Enforces PCI semantics: read-only fields, the BAR sizing protocol,
/// and command register masking.
pub struct DeviceState {
    ident: DeviceIdent,
    command: bits::RegCmd,
    bars: Bars,
    intr_line: u8,
    intr_pin: u8,
    header_type: u8,
    /// 0 means no capabilities.
    cap_ptr: u8,
}

impl DeviceState {
    /// The command register starts as `INTX_DIS`, no BAR is defined,
    /// and the header is Type 0.
    pub fn new(ident: DeviceIdent) -> Self {
        Self {
            ident,
            command: bits::RegCmd::default(),
            bars: Bars::new(),
            intr_line: 0xFF,
            intr_pin: 0,
            header_type: bits::HEADER_TYPE_DEVICE,
            cap_ptr: 0,
        }
    }

    pub fn define_bar(&mut self, bar: BarN, def: BarDefine) {
        self.bars.define(bar, def);
    }

    /// 0 = none, 1 = INTA, 2 = INTB, and so on.
    pub fn set_intr_pin(&mut self, pin: u8) {
        self.intr_pin = pin;
    }

    pub fn set_intr_line(&mut self, irq: u8) {
        self.intr_line = irq;
    }

    /// A non-zero pointer also sets CAP_LIST in the status register.
    pub fn set_cap_ptr(&mut self, offset: u8) {
        self.cap_ptr = offset;
    }

    pub fn bars(&self) -> &Bars {
        &self.bars
    }

    /// The command bits and BAR bases a migration carries.
    pub fn migrate_state(&self) -> MigratePciState {
        let mut bar_addrs = [0u64; 6];
        for (idx, addr) in bar_addrs.iter_mut().enumerate() {
            let bar = BarN::try_from(idx as u8).expect("valid BAR index");
            *addr = self.bars.get(bar).map(|(_, value)| value).unwrap_or(0);
        }
        MigratePciState {
            command: self.command.bits(),
            bar_addrs,
        }
    }

    /// The config writes that put `bar_addrs` back, as (offset, value)
    /// pairs. A 64-bit BAR takes two. The caller replays them through
    /// its own `cfg_write`, so bus registrations follow.
    pub fn bar_replay_writes(&self, bar_addrs: &[u64; 6]) -> Vec<(u8, u32)> {
        let mut writes = Vec::with_capacity(7);
        for (idx, addr) in bar_addrs.iter().copied().enumerate() {
            let bar = BarN::try_from(idx as u8).expect("valid BAR index");
            let Some((def, _)) = self.bars.get(bar) else {
                continue;
            };
            let offset = bits::REG_BAR0 + (idx as u8 * 4);
            match def {
                BarDefine::Pio(_) | BarDefine::Mmio(_) => {
                    writes.push((offset, addr as u32));
                }
                BarDefine::Mmio64(_) => {
                    writes.push((offset, addr as u32));
                    writes.push((offset + 4, (addr >> 32) as u32));
                }
            }
        }
        writes
    }

    /// Apply a guest write to one BAR register.
    ///
    /// This is the only path that changes a BAR base after definition,
    /// so every caller gets the same masking and remap result.
    pub fn bar_write(&mut self, bar: BarN, val: u32) -> Option<BarWriteResult> {
        self.bars.reg_write(bar, val)
    }

    pub fn command(&self) -> bits::RegCmd {
        self.command
    }

    /// Read from the standard header. Reserved fields, unimplemented
    /// fields and offsets past the header read as 0.
    pub fn cfg_read(&self, offset: u8, len: u8) -> u32 {
        if offset >= bits::LEN_CFG_STD as u8 {
            return 0;
        }

        let dword_off = offset & 0xFC;
        let byte_off = (offset & 0x03) as u32;

        let dword_val = match dword_off {
            // 0x00: Vendor ID (low 16) | Device ID (high 16)
            0x00 => {
                u32::from(self.ident.vendor_id)
                    | (u32::from(self.ident.device_id) << 16)
            }
            // 0x04: Command (low 16) | Status (high 16)
            0x04 => {
                let mut status = bits::RegStatus::empty();
                if self.cap_ptr != 0 {
                    status |= bits::RegStatus::CAP_LIST;
                }
                u32::from(self.command.bits())
                    | (u32::from(status.bits()) << 16)
            }
            // 0x08: Revision (8) | Prog IF (8) | Subclass (8) | Class (8)
            0x08 => {
                u32::from(self.ident.revision)
                    | (u32::from(self.ident.prog_if) << 8)
                    | (u32::from(self.ident.subclass) << 16)
                    | (u32::from(self.ident.class) << 24)
            }
            // 0x0C: Cache Line Size | Latency Timer | Header Type | BIST
            0x0C => u32::from(self.header_type) << 16,
            // 0x10-0x24: BARs 0-5
            bar_off @ 0x10..=0x24 if (bar_off & 0x03) == 0 => {
                let bar_idx = (bar_off - 0x10) / 4;
                if let Ok(bar_n) = BarN::try_from(bar_idx) {
                    self.bars.reg_read(bar_n)
                } else {
                    0
                }
            }
            // 0x28: Cardbus CIS Pointer
            0x28 => 0,
            // 0x2C: Sub-Vendor ID (low 16) | Sub-Device ID (high 16)
            0x2C => {
                u32::from(self.ident.sub_vendor_id)
                    | (u32::from(self.ident.sub_device_id) << 16)
            }
            // 0x30: Expansion ROM Base Address
            0x30 => 0,
            // 0x34: Capabilities Pointer (low 8) | Reserved (24)
            0x34 => u32::from(self.cap_ptr),
            // 0x38: Reserved
            0x38 => 0,
            // 0x3C: Intr Line (8) | Intr Pin (8) | Min Grant (8) | Max Lat (8)
            0x3C => u32::from(self.intr_line) | (u32::from(self.intr_pin) << 8),
            _ => 0,
        };

        (dword_val >> (byte_off * 8)) & size_mask(len)
    }

    /// Write to the standard header. Writes to read-only fields (vendor
    /// ID, device ID, class and others) are ignored.
    pub fn cfg_write(&mut self, offset: u8, len: u8, val: u32) {
        if offset >= bits::LEN_CFG_STD as u8 {
            return;
        }

        let dword_off = offset & 0xFC;
        let byte_off = (offset & 0x03) as u32;

        match dword_off {
            // 0x04: Command register (low 16 bits writable)
            0x04 if byte_off < 2 => {
                let shift = byte_off * 8;
                let mask = size_mask(len) << shift;
                let old_bits = u32::from(self.command.bits());
                let new_bits = (old_bits & !mask) | ((val << shift) & mask);
                let writable = bits::RegCmd::IO_EN
                    | bits::RegCmd::MMIO_EN
                    | bits::RegCmd::BUSMSTR_EN
                    | bits::RegCmd::INTX_DIS;
                self.command = bits::RegCmd::from_bits_truncate(
                    (new_bits as u16) & writable.bits()
                        | self.command.bits() & !writable.bits(),
                );
                // The status register (0x06) is write-1-to-clear. It has
                // no clearable bit here, so its writes are ignored.
            }
            // 0x10-0x24: BARs
            bar_off @ 0x10..=0x24
                if (bar_off & 0x03) == 0 && byte_off == 0 && len == 4 =>
            {
                // BAR writes must be dword-aligned and full-width.
                let bar_idx = (bar_off - 0x10) / 4;
                if let Ok(bar_n) = BarN::try_from(bar_idx) {
                    self.bar_write(bar_n, val);
                }
            }
            // 0x3C: Interrupt Line
            0x3C if byte_off == 0 && len >= 1 => {
                self.intr_line = val as u8;
            }
            // All other fields are read-only.
            _ => {}
        }
    }
}

fn size_mask(len: u8) -> u32 {
    match len {
        1 => 0xFF,
        2 => 0xFFFF,
        4 => 0xFFFF_FFFF,
        _ => 0xFFFF_FFFF,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ident() -> DeviceIdent {
        DeviceIdent {
            vendor_id: 0x8086,
            device_id: 0x1237,
            class: bits::CLASS_BRIDGE,
            subclass: bits::SUBCLASS_BRIDGE_HOST,
            prog_if: 0,
            revision: 0x02,
            sub_vendor_id: 0xFB5D,
            sub_device_id: 0x0001,
        }
    }

    #[test]
    fn read_vendor_device_id() {
        let state = DeviceState::new(test_ident());
        let val = state.cfg_read(0x00, 4);
        assert_eq!(val & 0xFFFF, 0x8086);
        assert_eq!((val >> 16) & 0xFFFF, 0x1237);
    }

    #[test]
    fn read_vendor_id_word() {
        let state = DeviceState::new(test_ident());
        let val = state.cfg_read(0x00, 2);
        assert_eq!(val, 0x8086);
    }

    #[test]
    fn read_device_id_word() {
        let state = DeviceState::new(test_ident());
        let val = state.cfg_read(0x02, 2);
        assert_eq!(val, 0x1237);
    }

    #[test]
    fn read_class_subclass() {
        let state = DeviceState::new(test_ident());
        let val = state.cfg_read(0x08, 4);
        assert_eq!(val & 0xFF, 0x02);
        assert_eq!((val >> 24) & 0xFF, bits::CLASS_BRIDGE as u32);
        assert_eq!((val >> 16) & 0xFF, bits::SUBCLASS_BRIDGE_HOST as u32);
    }

    #[test]
    fn read_header_type() {
        let state = DeviceState::new(test_ident());
        let val = state.cfg_read(0x0E, 1);
        assert_eq!(val, bits::HEADER_TYPE_DEVICE as u32);
    }

    #[test]
    fn the_guest_cannot_rewrite_the_header_type() {
        // PCI makes it read-only. A guest that sets 0x01 makes every
        // DeviceState device report itself as a bridge.
        let mut state = DeviceState::new(test_ident());
        for (off, len, val) in [
            (0x0Eu8, 1u8, 0x81u32),
            (0x0E, 2, 0x817F),
            (0x0C, 4, 0x817F_0000),
        ] {
            state.cfg_write(off, len, val);
            assert_eq!(
                state.cfg_read(0x0E, 1),
                u32::from(bits::HEADER_TYPE_DEVICE),
                "write at {off:#x} width {len}"
            );
        }
    }

    #[test]
    fn command_register_write_read() {
        let mut state = DeviceState::new(test_ident());
        state.cfg_write(0x04, 2, 0x0003);
        let val = state.cfg_read(0x04, 2);
        assert_eq!(val & 0x03, 0x03);
    }

    #[test]
    fn command_register_masks_reserved_bits() {
        let mut state = DeviceState::new(test_ident());
        state.cfg_write(0x04, 2, 0xFFFF);
        let cmd = state.cfg_read(0x04, 2);
        let expected = bits::RegCmd::IO_EN
            | bits::RegCmd::MMIO_EN
            | bits::RegCmd::BUSMSTR_EN
            | bits::RegCmd::INTX_DIS;
        assert_eq!(cmd, expected.bits() as u32);
    }

    #[test]
    fn bar_sizing_protocol() {
        let mut state = DeviceState::new(test_ident());
        state.define_bar(BarN::BAR0, BarDefine::Mmio(0x1000));

        state.cfg_write(0x10, 4, 0xFFFF_FFFF);
        let readback = state.cfg_read(0x10, 4);
        // ~(0x1000 - 1), with the 32-bit MMIO type bits 0b000.
        assert_eq!(readback, 0xFFFF_F000);
    }

    #[test]
    fn bar_address_write() {
        let mut state = DeviceState::new(test_ident());
        state.define_bar(BarN::BAR0, BarDefine::Pio(0x100));

        state.cfg_write(0x10, 4, 0x3000);
        let val = state.cfg_read(0x10, 4);
        // A PIO BAR has the low bit set.
        assert_eq!(val, 0x3001);
    }

    #[test]
    fn intr_line_write_read() {
        let mut state = DeviceState::new(test_ident());
        state.cfg_write(0x3C, 1, 0x0A);
        let val = state.cfg_read(0x3C, 1);
        assert_eq!(val, 0x0A);
    }

    #[test]
    fn intr_pin_read_only() {
        let mut state = DeviceState::new(test_ident());
        state.set_intr_pin(1); // INTA
        let val = state.cfg_read(0x3D, 1);
        assert_eq!(val, 1);
    }

    #[test]
    fn sub_vendor_device_id() {
        let state = DeviceState::new(test_ident());
        let val = state.cfg_read(0x2C, 4);
        assert_eq!(val & 0xFFFF, 0xFB5D);
        assert_eq!((val >> 16) & 0xFFFF, 0x0001);
    }

    #[test]
    fn read_only_fields_not_writable() {
        let mut state = DeviceState::new(test_ident());
        state.cfg_write(0x00, 2, 0xBEEF);
        assert_eq!(state.cfg_read(0x00, 2), 0x8086);
    }

    #[test]
    fn beyond_std_header_returns_zero() {
        let state = DeviceState::new(test_ident());
        assert_eq!(state.cfg_read(0x40, 4), 0);
        assert_eq!(state.cfg_read(0x80, 4), 0);
    }

    #[test]
    fn empty_bar_reads_zero() {
        let state = DeviceState::new(test_ident());
        assert_eq!(state.cfg_read(0x10, 4), 0);
        assert_eq!(state.cfg_read(0x14, 4), 0);
    }
}
