// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Physical capability layout, and the policy for guest config-space
//! writes.
//!
//! The illumos PPT driver applies no filter of its own: `PPT_CFG_WRITE`
//! is a bare `pci_config_put32`. So [`classify_cfg_write`] is the only
//! gate between a guest and the real device's config space, and it
//! denies by default.

use std::io::{Error, ErrorKind, Result};

use vmm_devices::pci::bits;

use super::{
    CFG_BAR0, CFG_BAR5, CFG_CAP_FIRST, CFG_CAP_PTR, CFG_COMMAND,
    CFG_HEADER_TYPE, CFG_INTERRUPT_LINE, CFG_STATUS, MSI_MSG_CTRL_64BIT,
    MSI_MSG_CTRL_OFF, MSI_MSG_CTRL_PVM,
};

/// Capability layout read from the physical device at bind time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct CapLayout {
    /// MSI capability offset. 0 means the device has no MSI.
    pub(super) msi_cap_off: u8,
    /// Length of the MSI capability register block in bytes.
    pub(super) msi_cap_len: u8,
    /// MSI-X capability offset. 0 means the device has no MSI-X.
    pub(super) msix_cap_off: u8,
}

/// Length of the MSI capability register block.
///
/// PCI 3.0 section 6.8.1: a 64-bit message address and per-vector
/// masking each make the block longer.
pub(super) fn msi_cap_len(msg_ctrl: u16) -> u8 {
    match (
        (msg_ctrl & MSI_MSG_CTRL_64BIT) != 0,
        (msg_ctrl & MSI_MSG_CTRL_PVM) != 0,
    ) {
        (false, false) => 10,
        (true, false) => 14,
        (false, true) => 20,
        (true, true) => 24,
    }
}

/// Number of MSI vectors to ask the kernel for.
///
/// Multiple Message Enable is a power-of-two exponent in bits 6:4. The
/// count is clamped to what the kernel says the device has, because it
/// comes from the guest.
pub(super) fn msi_numvec(msg_ctrl: u16, enabled: bool, limit: i32) -> i32 {
    if !enabled {
        // 0 tells ppt_setup_msi to tear the interrupts down.
        return 0;
    }
    let exponent = u32::from((msg_ctrl >> 4) & 0x7);
    let requested = 1i32 << exponent;
    requested.min(limit.max(0))
}

/// Read a little-endian u16 from a cached config space. Short reads
/// give 0, because the cache comes from hardware and must not panic.
fn read_cfg_u16(phys_cfg: &[u8; 256], idx: usize) -> u16 {
    let lo = phys_cfg.get(idx).copied().unwrap_or(0);
    let hi = phys_cfg.get(idx.wrapping_add(1)).copied().unwrap_or(0);
    u16::from_le_bytes([lo, hi])
}

/// Walk the capability list of a cached config space.
///
/// The list comes from hardware, so the walk is bounded and every index
/// is checked. A malformed or cyclic chain must not panic the VMM. An
/// MSI block that runs off the end of config space is not recorded:
/// it cannot be emulated, and nothing else may serve it.
pub(super) fn scan_capabilities(phys_cfg: &[u8; 256]) -> CapLayout {
    let mut caps = CapLayout::default();
    let mut ptr = phys_cfg[CFG_CAP_PTR];
    // The capability area holds at most 48 four-byte headers, so this
    // ends even on a cyclic chain.
    for _ in 0..48 {
        if ptr < CFG_CAP_FIRST {
            break;
        }
        let idx = usize::from(ptr);
        let Some(&cap_id) = phys_cfg.get(idx) else {
            break;
        };
        match cap_id {
            bits::CAP_ID_MSI => {
                let ctrl =
                    read_cfg_u16(phys_cfg, idx + usize::from(MSI_MSG_CTRL_OFF));
                let len = msi_cap_len(ctrl);
                if idx + usize::from(len) <= phys_cfg.len() {
                    caps.msi_cap_off = ptr;
                    caps.msi_cap_len = len;
                }
            }
            bits::CAP_ID_MSIX => caps.msix_cap_off = ptr,
            _ => {}
        }
        let Some(&next) = phys_cfg.get(idx + 1) else {
            break;
        };
        ptr = next;
    }
    caps
}

/// Why an MSI-X capable device cannot be passed through.
const MSIX_REFUSAL: &str = "device advertises MSI-X, which this VMM cannot \
     isolate. The MSI-X table sits in a BAR that is mapped straight into the \
     guest, and illumos VT-d remaps DMA but not interrupts, so the guest \
     could make the device raise any vector on the host APIC. Use a device \
     that has MSI or INTx only.";

/// Refuse a device this VMM cannot isolate.
pub(super) fn check_passthrough_supported(caps: &CapLayout) -> Result<()> {
    if caps.msix_cap_off != 0 {
        return Err(Error::new(ErrorKind::Unsupported, MSIX_REFUSAL));
    }
    Ok(())
}

// ── Config-space write policy ──────────────────────────────────────

/// Where a guest config-space write is allowed to land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CfgWrite {
    /// Emulated command register. The merged 16-bit value also goes to
    /// the device, because bus mastering must reach the hardware.
    Command,
    /// Emulated BAR in `DeviceState`. It never reaches the device.
    Bar,
    /// Emulated MSI capability. It never reaches the device.
    MsiCap,
    /// Emulated header byte: the interrupt line, or the header type
    /// the bus sets when a second function joins the slot.
    Header,
    /// Dropped. Nothing reaches the device.
    Deny,
}

/// Is the access inside the MSI capability block?
///
/// The end is rounded up to a dword, because a driver reads and writes
/// config space one dword at a time and the next capability always
/// starts on a dword boundary.
pub(super) fn in_cap(offset: u8, caps: &CapLayout) -> bool {
    if caps.msi_cap_off == 0 || caps.msi_cap_len == 0 {
        return false;
    }
    let start = usize::from(caps.msi_cap_off);
    let end = (start + usize::from(caps.msi_cap_len)).div_ceil(4) * 4;
    usize::from(offset) >= start && usize::from(offset) < end
}

/// Decide where a guest config-space write goes.
///
/// Deny is the default, as it is in Linux VFIO. A register with no
/// handler must not reach the hardware: PCIe DEVCTL carries Initiate
/// FLR, PMCSR moves the device to D3, the Expansion ROM BAR points a
/// hardware decode window at host physical memory, and a vendor
/// capability can do anything at all. The illumos PPT driver adds no
/// filter of its own, so this function is the only gate.
pub(super) fn classify_cfg_write(
    offset: u8,
    len: u8,
    caps: &CapLayout,
) -> CfgWrite {
    // A config access is 1, 2 or 4 bytes and stays inside one dword.
    if !matches!(len, 1 | 2 | 4) {
        return CfgWrite::Deny;
    }
    if usize::from(offset & 0x03) + usize::from(len) > 4 {
        return CfgWrite::Deny;
    }

    match offset & 0xFC {
        // Status, the high half of this dword, is write-1-to-clear on
        // the hardware and stays read-only to the guest. A dword write
        // that starts at the command register is still allowed, because
        // that is how a driver usually sets it: `cfg_write_command`
        // sends the low 16 bits and nothing else.
        CFG_COMMAND if offset < CFG_STATUS => CfgWrite::Command,
        CFG_BAR0..=CFG_BAR5 => CfgWrite::Bar,
        CFG_INTERRUPT_LINE if offset == CFG_INTERRUPT_LINE => CfgWrite::Header,
        0x0C if offset == CFG_HEADER_TYPE && len == 1 => CfgWrite::Header,
        _ if in_cap(offset, caps) => CfgWrite::MsiCap,
        _ => CfgWrite::Deny,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fake physical config space with known values.
    fn fake_phys_cfg() -> [u8; 256] {
        let mut cfg = [0u8; 256];

        // Vendor ID
        cfg[0] = 0xDE;
        cfg[1] = 0x10;
        // Device ID
        cfg[2] = 0x30;
        cfg[3] = 0x23;

        // Command = 0x0000 (disabled)
        cfg[4] = 0x00;
        cfg[5] = 0x00;
        // Status = 0x0010 (CAP_LIST)
        cfg[6] = 0x10;
        cfg[7] = 0x00;

        // Revision = 0xA1
        cfg[8] = 0xA1;
        // Prog IF = 0x00
        cfg[9] = 0x00;
        // Subclass = 0x02 (3D controller)
        cfg[0x0A] = 0x02;
        // Class = 0x03 (Display)
        cfg[0x0B] = 0x03;

        // Sub-Vendor ID = 0x10DE
        cfg[0x2C] = 0xDE;
        cfg[0x2D] = 0x10;
        // Sub-Device ID = 0x16C0
        cfg[0x2E] = 0xC0;
        cfg[0x2F] = 0x16;

        // Capabilities pointer = 0x60
        cfg[0x34] = 0x60;

        // MSI-X capability at offset 0x60:
        // Cap ID = 0x11 (MSI-X)
        cfg[0x60] = 0x11;
        // Next pointer = 0x70 (MSI)
        cfg[0x61] = 0x70;
        // Message Control: table size = 31 (32 vectors, N-1), enable=0
        cfg[0x62] = 0x1F;
        cfg[0x63] = 0x00;
        // Table Offset/BIR: BAR1, offset 0
        cfg[0x64] = 0x01; // BIR = 1
        cfg[0x65] = 0x00;
        cfg[0x66] = 0x00;
        cfg[0x67] = 0x00;
        // PBA Offset/BIR: BAR1, offset 0x1000
        cfg[0x68] = 0x01; // BIR = 1
        cfg[0x69] = 0x10;
        cfg[0x6A] = 0x00;
        cfg[0x6B] = 0x00;

        // MSI capability at offset 0x70:
        // Cap ID = 0x05 (MSI)
        cfg[0x70] = 0x05;
        // Next pointer = 0x00 (end of list)
        cfg[0x71] = 0x00;
        // Message Control
        cfg[0x72] = 0x00;
        cfg[0x73] = 0x00;

        cfg
    }

    /// `fake_phys_cfg` with the MSI-X capability unlinked from the
    /// chain, so the list holds MSI only.
    fn fake_phys_cfg_msi_only() -> [u8; 256] {
        let mut cfg = fake_phys_cfg();
        cfg[0x34] = 0x70;
        cfg[0x60] = 0x00;
        cfg
    }
    #[test]
    fn scan_finds_both_capabilities() {
        let caps = scan_capabilities(&fake_phys_cfg());
        assert_eq!(caps.msix_cap_off, 0x60);
        assert_eq!(caps.msi_cap_off, 0x70);
        // Message Control 0x0000: 32-bit address, no per-vector mask.
        assert_eq!(caps.msi_cap_len, 10);
    }

    #[test]
    fn msi_cap_len_follows_message_control() {
        assert_eq!(msi_cap_len(0x0000), 10);
        assert_eq!(msi_cap_len(MSI_MSG_CTRL_64BIT), 14);
        assert_eq!(msi_cap_len(MSI_MSG_CTRL_PVM), 20);
        assert_eq!(msi_cap_len(MSI_MSG_CTRL_64BIT | MSI_MSG_CTRL_PVM), 24);
    }

    #[test]
    fn capability_walk_survives_a_cycle() {
        let mut cfg = fake_phys_cfg();
        cfg[0x34] = 0x40;
        cfg[0x40] = 0x09; // vendor specific
        cfg[0x41] = 0x40; // points at itself
        let caps = scan_capabilities(&cfg);
        assert_eq!(caps.msi_cap_off, 0);
        assert_eq!(caps.msix_cap_off, 0);
    }

    #[test]
    fn capability_walk_survives_a_pointer_at_the_end_of_config_space() {
        let mut cfg = fake_phys_cfg();
        cfg[0x34] = 0xFF;
        cfg[0xFF] = bits::CAP_ID_MSI;
        let caps = scan_capabilities(&cfg);
        // The block runs off the end, so it cannot be emulated and is
        // not recorded. The read is short, not a panic.
        assert_eq!(caps.msi_cap_off, 0);
        assert_eq!(caps.msi_cap_len, 0);
    }

    /// The MSI block never reaches past 0xFF, so every offset inside it
    /// is a valid config offset.
    #[test]
    fn msi_block_that_fits_at_the_top_is_kept() {
        let mut cfg = fake_phys_cfg();
        cfg[0x34] = 0xF4;
        cfg[0xF4] = bits::CAP_ID_MSI;
        let caps = scan_capabilities(&cfg);
        assert_eq!(caps.msi_cap_off, 0xF4);
        assert_eq!(classify_cfg_write(0xFC, 4, &caps), CfgWrite::MsiCap);
        assert_eq!(caps.msi_cap_len, 10);
    }

    /// An MSI-X capable device must not attach. Its MSI-X table is
    /// mapped into the guest and illumos does no interrupt remapping.
    #[test]
    fn msix_capable_device_is_refused() {
        let caps = scan_capabilities(&fake_phys_cfg());
        let err = check_passthrough_supported(&caps)
            .expect_err("an MSI-X device must be refused");
        assert_eq!(err.kind(), ErrorKind::Unsupported);
        assert!(err.to_string().contains("MSI-X"), "{err}");
    }

    #[test]
    fn device_without_msix_is_accepted() {
        let caps = scan_capabilities(&fake_phys_cfg_msi_only());
        assert_eq!(caps.msix_cap_off, 0);
        assert!(check_passthrough_supported(&caps).is_ok());
    }

    /// PCIe DEVCTL bit 15 is Initiate Function Level Reset. A forwarded
    /// write resets a device the host still owns.
    #[test]
    fn cfg_write_denies_pcie_devctl() {
        let caps = scan_capabilities(&fake_phys_cfg_msi_only());
        assert_eq!(classify_cfg_write(0x88, 2, &caps), CfgWrite::Deny);
    }

    /// PMCSR holds the power state. A forwarded write moves the device
    /// to D3 and the host loses it.
    #[test]
    fn cfg_write_denies_pmcsr() {
        let caps = scan_capabilities(&fake_phys_cfg_msi_only());
        assert_eq!(classify_cfg_write(0x44, 2, &caps), CfgWrite::Deny);
    }

    /// The Expansion ROM BAR is a hardware decode window in host
    /// physical space, not an emulated register.
    #[test]
    fn cfg_write_denies_expansion_rom_bar() {
        let caps = scan_capabilities(&fake_phys_cfg_msi_only());
        assert_eq!(classify_cfg_write(0x30, 4, &caps), CfgWrite::Deny);
    }

    /// Status is write-1-to-clear on the hardware.
    #[test]
    fn cfg_write_denies_the_status_half_of_the_command_dword() {
        let caps = scan_capabilities(&fake_phys_cfg_msi_only());
        assert_eq!(classify_cfg_write(0x06, 2, &caps), CfgWrite::Deny);
    }

    /// A dword write at the command register is how most drivers set
    /// it. It must reach the handler, which drops the status half.
    #[test]
    fn cfg_write_allows_a_dword_write_at_the_command_register() {
        let caps = scan_capabilities(&fake_phys_cfg_msi_only());
        assert_eq!(classify_cfg_write(0x04, 4, &caps), CfgWrite::Command);
    }

    #[test]
    fn cfg_write_denies_an_odd_width() {
        let caps = scan_capabilities(&fake_phys_cfg_msi_only());
        assert_eq!(classify_cfg_write(0x04, 3, &caps), CfgWrite::Deny);
    }

    #[test]
    fn cfg_write_denies_an_access_that_crosses_a_dword() {
        let caps = scan_capabilities(&fake_phys_cfg_msi_only());
        assert_eq!(classify_cfg_write(0x02, 4, &caps), CfgWrite::Deny);
    }

    /// The MSI capability must not be a window into the capability that
    /// follows it. A 32-bit block with no per-vector mask is 10 bytes,
    /// so it owns 0x70 through 0x7B and nothing beyond.
    #[test]
    fn cfg_write_denies_past_the_end_of_the_msi_capability() {
        let caps = scan_capabilities(&fake_phys_cfg_msi_only());
        assert_eq!(classify_cfg_write(0x78, 4, &caps), CfgWrite::MsiCap);
        assert_eq!(classify_cfg_write(0x7C, 4, &caps), CfgWrite::Deny);
    }

    /// A device with no MSI capability has no capability write path.
    #[test]
    fn cfg_write_denies_the_msi_block_when_the_device_has_no_msi() {
        let caps = CapLayout::default();
        assert_eq!(classify_cfg_write(0x74, 4, &caps), CfgWrite::Deny);
    }

    #[test]
    fn cfg_write_allows_the_registers_with_a_handler() {
        let caps = scan_capabilities(&fake_phys_cfg_msi_only());
        assert_eq!(classify_cfg_write(0x04, 2, &caps), CfgWrite::Command);
        assert_eq!(classify_cfg_write(0x10, 4, &caps), CfgWrite::Bar);
        assert_eq!(classify_cfg_write(0x24, 4, &caps), CfgWrite::Bar);
        assert_eq!(classify_cfg_write(0x3C, 1, &caps), CfgWrite::Header);
        assert_eq!(classify_cfg_write(0x0E, 1, &caps), CfgWrite::Header);
        assert_eq!(classify_cfg_write(0x74, 4, &caps), CfgWrite::MsiCap);
    }

    /// BIST and the cache line register share the dword with the
    /// header type. Only the header type byte has a handler.
    #[test]
    fn cfg_write_denies_the_rest_of_the_header_type_dword() {
        let caps = scan_capabilities(&fake_phys_cfg_msi_only());
        assert_eq!(classify_cfg_write(0x0C, 4, &caps), CfgWrite::Deny);
        assert_eq!(classify_cfg_write(0x0F, 1, &caps), CfgWrite::Deny);
        assert_eq!(classify_cfg_write(0x0E, 2, &caps), CfgWrite::Deny);
    }

    /// A guest that clears MSI Enable must tear the vectors down, not
    /// leave them armed.
    #[test]
    fn msi_numvec_is_zero_when_the_guest_disables_msi() {
        // Multiple Message Enable = 2, so four vectors when enabled.
        let ctrl = 2u16 << 4;
        assert_eq!(msi_numvec(ctrl, false, 8), 0);
        assert_eq!(msi_numvec(ctrl, true, 8), 4);
    }

    /// Multiple Message Enable is guest-supplied and must not ask the
    /// kernel for more vectors than the device has.
    #[test]
    fn msi_numvec_is_clamped_to_the_kernel_limit() {
        // Multiple Message Enable = 5, so 32 vectors asked for.
        let ctrl = 5u16 << 4;
        assert_eq!(msi_numvec(ctrl, true, 4), 4);
        assert_eq!(msi_numvec(ctrl, true, 0), 0);
        assert_eq!(msi_numvec(ctrl, true, -1), 0);
    }
}
