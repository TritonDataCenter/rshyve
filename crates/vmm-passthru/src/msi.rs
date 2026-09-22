// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The emulated MSI capability.
//!
//! The physical MSI registers belong to the kernel: `ppt_setup_msi`
//! programs a host address and a host vector there. A guest write that
//! reached them would make the device post to a guest-chosen host
//! address, and illumos VT-d does not remap interrupts. So the guest
//! gets a shadow of the capability block, as in upstream bhyve. Writes
//! land in the shadow, reads come from it, and only the values it holds
//! go to `VM_PPTDEV_MSI`.

use super::cfg::msi_numvec;
use super::{
    MSI_MSG_CTRL_64BIT, MSI_MSG_CTRL_ENABLE, MSI_MSG_CTRL_OFF, MSI_MSG_CTRL_PVM,
};

/// The longest MSI capability block: 64-bit address with per-vector
/// masking.
pub(crate) const MSI_CAP_MAX_LEN: usize = 24;

/// Multiple Message Capable, bits 3:1 of Message Control.
const MSI_MSG_CTRL_MMC_MASK: u16 = 0x7 << 1;
/// Multiple Message Enable, bits 6:4 of Message Control.
const MSI_MSG_CTRL_MME_MASK: u16 = 0x7 << 4;

/// What the kernel is asked to program.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MsiProgram {
    pub addr: u64,
    pub data: u64,
    pub numvec: i32,
}

/// The guest-visible MSI capability block.
pub(crate) struct MsiCap {
    off: u8,
    len: u8,
    regs: [u8; MSI_CAP_MAX_LEN],
    /// Which bits of each byte a guest write may change.
    writable: [u8; MSI_CAP_MAX_LEN],
    vec_limit: i32,
}

impl MsiCap {
    /// Build the shadow from the physical capability at `off`.
    ///
    /// `None` when the device has no MSI capability or the block runs
    /// off the end of config space, which no real device does.
    pub(crate) fn from_cfg(
        phys_cfg: &[u8; 256],
        off: u8,
        len: u8,
        vec_limit: i32,
    ) -> Option<Self> {
        let start = usize::from(off);
        let block_len = usize::from(len);
        if off == 0 || block_len == 0 || block_len > MSI_CAP_MAX_LEN {
            return None;
        }
        let block = phys_cfg.get(start..start + block_len)?;

        // Only the ID, the next pointer and Message Control carry over.
        // The address and data hold the host's APIC id and vector, and
        // a reset device shows zeros there anyway.
        let mut regs = [0u8; MSI_CAP_MAX_LEN];
        regs[..4].copy_from_slice(&block[..4]);

        let mut ctrl = msg_ctrl(&regs);
        // The guest starts with MSI off and one vector, whatever the
        // host left in the hardware.
        ctrl &= !(MSI_MSG_CTRL_ENABLE | MSI_MSG_CTRL_MME_MASK);
        ctrl = clamp_mmc(ctrl, vec_limit);
        set_msg_ctrl(&mut regs, ctrl);

        let is_64bit = ctrl & MSI_MSG_CTRL_64BIT != 0;
        let has_pvm = ctrl & MSI_MSG_CTRL_PVM != 0;
        let mut writable = [0u8; MSI_CAP_MAX_LEN];
        // Message Control: Enable and Multiple Message Enable.
        writable[2] = 0x71;
        // Message Address, bits 1:0 are reserved.
        writable[4..8].copy_from_slice(&[0xFC, 0xFF, 0xFF, 0xFF]);
        let data_off = if is_64bit {
            writable[8..12].fill(0xFF);
            12
        } else {
            8
        };
        writable[data_off..data_off + 2].fill(0xFF);
        if has_pvm {
            // Mask Bits are writable. Pending Bits are not.
            let mask_off = data_off + 4;
            writable[mask_off..mask_off + 4].fill(0xFF);
        }

        Some(Self {
            off,
            len,
            regs,
            writable,
            vec_limit,
        })
    }

    /// Read `len` bytes at config offset `offset`. Bytes outside the
    /// block read as zero, as the reserved tail of a capability does.
    pub(crate) fn read(&self, offset: u8, len: u8) -> u32 {
        let mut val = 0u32;
        for i in 0..usize::from(len).min(4) {
            let byte = self.byte_at(usize::from(offset) + i);
            val |= u32::from(byte) << (i * 8);
        }
        val
    }

    /// Merge a guest write into the shadow.
    ///
    /// Only the writable bits change. The capability ID and the next
    /// pointer are read-only, so a dword write at the start of the
    /// block changes Message Control alone.
    pub(crate) fn write(&mut self, offset: u8, len: u8, val: u32) {
        let bytes = val.to_le_bytes();
        for (i, byte) in bytes.iter().enumerate().take(usize::from(len).min(4))
        {
            let Some(idx) = self.index_of(usize::from(offset) + i) else {
                continue;
            };
            let mask = self.writable[idx];
            self.regs[idx] = (self.regs[idx] & !mask) | (byte & mask);
        }
    }

    /// The values the kernel must hold for the guest's current
    /// configuration. Disabled is one value, whatever the address and
    /// data registers hold, so a driver filling them in before it
    /// enables MSI causes no kernel call.
    pub(crate) fn program(&self) -> MsiProgram {
        let ctrl = msg_ctrl(&self.regs);
        let enabled = ctrl & MSI_MSG_CTRL_ENABLE != 0;
        let numvec = msi_numvec(ctrl, enabled, self.vec_limit);
        if numvec == 0 {
            return MsiProgram::default();
        }
        let addr_lo = u32::from_le_bytes([
            self.regs[4],
            self.regs[5],
            self.regs[6],
            self.regs[7],
        ]);
        let (addr, data) = if ctrl & MSI_MSG_CTRL_64BIT != 0 {
            let addr_hi = u32::from_le_bytes([
                self.regs[8],
                self.regs[9],
                self.regs[10],
                self.regs[11],
            ]);
            let data = u16::from_le_bytes([self.regs[12], self.regs[13]]);
            (u64::from(addr_lo) | (u64::from(addr_hi) << 32), data)
        } else {
            let data = u16::from_le_bytes([self.regs[8], self.regs[9]]);
            (u64::from(addr_lo), data)
        };
        MsiProgram {
            addr,
            data: u64::from(data),
            numvec,
        }
    }

    fn index_of(&self, cfg_offset: usize) -> Option<usize> {
        let idx = cfg_offset.checked_sub(usize::from(self.off))?;
        (idx < usize::from(self.len)).then_some(idx)
    }

    fn byte_at(&self, cfg_offset: usize) -> u8 {
        self.index_of(cfg_offset).map_or(0, |idx| self.regs[idx])
    }
}

fn msg_ctrl(regs: &[u8; MSI_CAP_MAX_LEN]) -> u16 {
    let off = usize::from(MSI_MSG_CTRL_OFF);
    u16::from_le_bytes([regs[off], regs[off + 1]])
}

fn set_msg_ctrl(regs: &mut [u8; MSI_CAP_MAX_LEN], ctrl: u16) {
    let off = usize::from(MSI_MSG_CTRL_OFF);
    regs[off..off + 2].copy_from_slice(&ctrl.to_le_bytes());
}

/// Lower Multiple Message Capable to what the kernel can allocate, so
/// the guest never asks for vectors it cannot have.
fn clamp_mmc(ctrl: u16, vec_limit: i32) -> u16 {
    let mut mmc = (ctrl & MSI_MSG_CTRL_MMC_MASK) >> 1;
    while mmc > 0 && i64::from(1u32 << mmc) > i64::from(vec_limit) {
        mmc -= 1;
    }
    (ctrl & !MSI_MSG_CTRL_MMC_MASK) | (mmc << 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 32-bit MSI capability at 0x70 as the host left it: enabled,
    /// two vectors, pointed at a host APIC and vector.
    fn host_programmed_cfg() -> [u8; 256] {
        let mut cfg = [0u8; 256];
        cfg[0x70] = 0x05;
        cfg[0x71] = 0x00;
        // Enable, MME = 1, MMC = 3 (eight vectors capable).
        let ctrl: u16 = MSI_MSG_CTRL_ENABLE | (1 << 4) | (3 << 1);
        cfg[0x72..0x74].copy_from_slice(&ctrl.to_le_bytes());
        cfg[0x74..0x78].copy_from_slice(&0xFEE0_3000u32.to_le_bytes());
        cfg[0x78..0x7A].copy_from_slice(&0x00A5u16.to_le_bytes());
        cfg
    }

    #[test]
    fn shadow_hides_the_host_programming() {
        let cap =
            MsiCap::from_cfg(&host_programmed_cfg(), 0x70, 10, 8).expect("cap");
        let ctrl = cap.read(0x72, 2) as u16;
        assert_eq!(ctrl & MSI_MSG_CTRL_ENABLE, 0, "enable cleared");
        assert_eq!(ctrl & MSI_MSG_CTRL_MME_MASK, 0, "MME cleared");
        assert_eq!((ctrl & MSI_MSG_CTRL_MMC_MASK) >> 1, 3, "MMC kept");
        assert_eq!(cap.read(0x74, 4), 0, "host address not copied");
        assert_eq!(cap.read(0x78, 2), 0, "host vector not copied");
        assert_eq!(cap.program(), MsiProgram::default());
    }

    #[test]
    fn capable_count_is_clamped_to_the_kernel_limit() {
        let cap =
            MsiCap::from_cfg(&host_programmed_cfg(), 0x70, 10, 2).expect("cap");
        let ctrl = cap.read(0x72, 2) as u16;
        assert_eq!((ctrl & MSI_MSG_CTRL_MMC_MASK) >> 1, 1);

        let cap =
            MsiCap::from_cfg(&host_programmed_cfg(), 0x70, 10, 0).expect("cap");
        let ctrl = cap.read(0x72, 2) as u16;
        assert_eq!(ctrl & MSI_MSG_CTRL_MMC_MASK, 0);
    }

    #[test]
    fn writes_change_only_the_writable_bits() {
        let mut cap =
            MsiCap::from_cfg(&host_programmed_cfg(), 0x70, 10, 8).expect("cap");
        // A dword write at the block start carries the ID and next
        // pointer, which must not change, and the enable bit.
        cap.write(0x70, 4, 0xFFFF_FFFF);
        assert_eq!(cap.read(0x70, 1), 0x05, "cap id");
        assert_eq!(cap.read(0x71, 1), 0x00, "next pointer");
        let ctrl = cap.read(0x72, 2) as u16;
        assert_eq!(ctrl & 0xFF8E, 3 << 1, "only Enable and MME moved");
        assert_eq!(ctrl & 0x71, 0x71);

        cap.write(0x74, 4, 0xFEE0_0003);
        assert_eq!(cap.read(0x74, 4), 0xFEE0_0000, "reserved bits stay 0");
        cap.write(0x78, 2, 0x1234);
        assert_eq!(cap.read(0x78, 2), 0x1234);
    }

    #[test]
    fn program_follows_the_shadow_not_the_hardware() {
        let mut cap =
            MsiCap::from_cfg(&host_programmed_cfg(), 0x70, 10, 8).expect("cap");
        cap.write(0x74, 4, 0xFEE0_1000);
        cap.write(0x78, 2, 0x0031);
        // Enable with MME = 2 (four vectors).
        cap.write(0x72, 2, u32::from(MSI_MSG_CTRL_ENABLE | (2 << 4)));
        assert_eq!(
            cap.program(),
            MsiProgram {
                addr: 0xFEE0_1000,
                data: 0x31,
                numvec: 4
            }
        );
        cap.write(0x72, 2, 0);
        assert_eq!(cap.program(), MsiProgram::default());
    }

    #[test]
    fn sixty_four_bit_layout_moves_the_data_register() {
        let mut cfg = [0u8; 256];
        cfg[0x70] = 0x05;
        let ctrl = MSI_MSG_CTRL_64BIT | MSI_MSG_CTRL_PVM;
        cfg[0x72..0x74].copy_from_slice(&ctrl.to_le_bytes());
        let mut cap = MsiCap::from_cfg(&cfg, 0x70, 24, 1).expect("cap");
        cap.write(0x74, 4, 0xFEE0_0000);
        cap.write(0x78, 4, 0x0000_0001);
        cap.write(0x7C, 2, 0x0042);
        cap.write(0x80, 4, 0xFFFF_FFFF);
        cap.write(0x84, 4, 0xFFFF_FFFF);
        cap.write(0x72, 2, u32::from(MSI_MSG_CTRL_ENABLE));
        let p = cap.program();
        assert_eq!(p.addr, 0x1_FEE0_0000);
        assert_eq!(p.data, 0x42);
        assert_eq!(p.numvec, 1);
        assert_eq!(cap.read(0x80, 4), 0xFFFF_FFFF, "mask bits writable");
        assert_eq!(cap.read(0x84, 4), 0, "pending bits read-only");
    }

    #[test]
    fn a_block_past_the_end_of_config_space_is_refused() {
        let mut cfg = [0u8; 256];
        cfg[0xF8] = 0x05;
        assert!(MsiCap::from_cfg(&cfg, 0xF8, 10, 1).is_none());
        assert!(MsiCap::from_cfg(&cfg, 0, 10, 1).is_none());
    }

    #[test]
    fn accesses_at_the_top_of_config_space_do_not_overflow() {
        let mut cfg = [0u8; 256];
        cfg[0xF4] = 0x05;
        let mut cap = MsiCap::from_cfg(&cfg, 0xF4, 10, 1).expect("cap");
        cap.write(0xFC, 4, 0xFFFF_FFFF);
        // 0xFC..0xFE is Message Data. 0xFE..0x100 lies past the block.
        assert_eq!(cap.read(0xFC, 4), 0x0000_FFFF);
        assert_eq!(cap.read(0xFE, 2), 0);
    }
}
