// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! AHCI, ATA and SCSI constants, kept apart from mutable controller state so
//! the guest-visible values are reviewable on their own.

pub const ABAR_SIZE: u32 = 0x1000;
pub const AHCI_OFFSET: usize = 0x100;
pub const AHCI_STEP: usize = 0x80;
pub const NUM_PORTS: usize = 1;
pub const NUM_SLOTS: usize = 32;
pub const CL_ENTRY_SIZE: usize = 32;
pub const CL_SIZE: usize = 1024;
pub const FIS_AREA_SIZE: usize = 256;
pub const CMD_TBL_ACMD_OFF: usize = 0x40;
pub const CMD_TBL_PRDT_OFF: usize = 0x80;
pub const ACMD_LEN: usize = 16;
pub const PRD_ENTRY_SIZE: usize = 16;
pub const DBCMASK: u32 = 0x003F_FFFF;
/// Bounds allocation and the PRD walk before any guest memory access.
pub const MAX_PRDTL: u16 = 512;
pub const MAX_XFER_BYTES: u64 = 1 << 20;
pub const CD_BLOCK_SIZE: u64 = 2048;

pub const HBA_CAP: usize = 0x00;
pub const HBA_GHC: usize = 0x04;
pub const HBA_IS: usize = 0x08;
pub const HBA_PI: usize = 0x0C;
pub const HBA_VS: usize = 0x10;
pub const HBA_CCC_CTL: usize = 0x14;
pub const HBA_CCC_PORTS: usize = 0x18;
pub const HBA_EM_LOC: usize = 0x1C;
pub const HBA_EM_CTL: usize = 0x20;
pub const HBA_CAP2: usize = 0x24;
pub const HBA_BOHC: usize = 0x28;

/// SNCQ, Partial, and Slumber stay clear because their command and power-state
/// semantics are not implemented. CLO lets storahci recover from BSY without
/// COMRESET, and SAM stays clear so GHC.AE remains writable.
pub const CAP_VALUE: u32 = 0x8130_1F00;
pub const CAP2_VALUE: u32 = 0;
pub const VS_VALUE: u32 = 0x0001_0300;
pub const PI_VALUE: u32 = 0x0000_0001;

pub const GHC_HR: u32 = 1 << 0;
pub const GHC_IE: u32 = 1 << 1;
pub const GHC_AE: u32 = 1 << 31;
pub const GHC_RESET: u32 = GHC_AE;

pub const PX_CLB: usize = 0x00;
pub const PX_CLBU: usize = 0x04;
pub const PX_FB: usize = 0x08;
pub const PX_FBU: usize = 0x0C;
pub const PX_IS: usize = 0x10;
pub const PX_IE: usize = 0x14;
pub const PX_CMD: usize = 0x18;
pub const PX_TFD: usize = 0x20;
pub const PX_SIG: usize = 0x24;
pub const PX_SSTS: usize = 0x28;
pub const PX_SCTL: usize = 0x2C;
pub const PX_SERR: usize = 0x30;
pub const PX_SACT: usize = 0x34;
pub const PX_CI: usize = 0x38;
pub const PX_SNTF: usize = 0x3C;
pub const PX_FBS: usize = 0x40;
pub const PX_DEVSLP: usize = 0x44;

pub const PXCMD_ST: u32 = 1 << 0;
pub const PXCMD_SUD: u32 = 1 << 1;
pub const PXCMD_POD: u32 = 1 << 2;
pub const PXCMD_CLO: u32 = 1 << 3;
pub const PXCMD_FRE: u32 = 1 << 4;
pub const PXCMD_CCS_MASK: u32 = 0x1F00;
pub const PXCMD_CCS_SHIFT: u32 = 8;
pub const PXCMD_FR: u32 = 1 << 14;
pub const PXCMD_CR: u32 = 1 << 15;
pub const PXCMD_CPS: u32 = 1 << 16;
pub const PXCMD_APSTE: u32 = 1 << 23;
pub const PXCMD_ATAPI: u32 = 1 << 24;
pub const PXCMD_DLAE: u32 = 1 << 25;
pub const PXCMD_ALPE: u32 = 1 << 26;
pub const PXCMD_ASP: u32 = 1 << 27;
pub const PXCMD_ICC_MASK: u32 = 0xF000_0000;
pub const PXCMD_WMASK: u32 = 0xFF80_001F;
pub const PXCMD_RESET: u32 = 0x0001_0006;

pub const PXIS_DHRS: u32 = 1 << 0;
pub const PXIS_PSS: u32 = 1 << 1;
pub const PXIS_TFES: u32 = 1 << 30;
pub const PXIE_WMASK: u32 = 0xFDC0_00FF;

/// DRDY is clear because packet commands report ATAPI readiness. BSY or DRQ
/// at reset makes storahci wait through its recovery timeout.
pub const PXTFD_RESET_ATAPI: u32 = 0x0000_0130;
pub const PXSIG_ATAPI: u32 = 0xEB14_0101;
pub const PXSSTS_RESET: u32 = 0x0000_0133;
pub const PXSCTL_SPD_MASK: u32 = 0x0000_00F0;
pub const PXSCTL_DET_MASK: u32 = 0x0000_000F;
pub const PXSCTL_DET_COMRESET: u32 = 1;

pub const FIS_OFF_DSFIS: usize = 0x00;
pub const FIS_OFF_PSFIS: usize = 0x20;
pub const FIS_OFF_RFIS: usize = 0x40;
pub const FIS_OFF_SDBFIS: usize = 0x58;
pub const FIS_OFF_UFIS: usize = 0x60;

pub const FIS_TYPE_REGH2D: u8 = 0x27;
pub const FIS_TYPE_REGD2H: u8 = 0x34;
pub const FIS_TYPE_PIOSETUP: u8 = 0x5F;
pub const FIS_D2H_LEN: usize = 20;
pub const FIS_PIOSETUP_LEN: usize = 20;

pub const ATA_S_ERROR: u32 = 0x01;
pub const ATA_S_DRQ: u32 = 0x08;
pub const ATA_S_DSC: u32 = 0x10;
pub const ATA_S_DMA: u32 = 0x20;
pub const ATA_S_READY: u32 = 0x40;
pub const ATA_S_BUSY: u32 = 0x80;
pub const ATA_E_ABORT: u8 = 0x04;
pub const ATA_I_CMD: u8 = 0x01;
pub const ATA_I_IN: u8 = 0x02;
pub const TFD_OK: u32 = 0x0050;
pub const TFD_ABORT: u32 = 0x0441;

pub const fn tfd_check_condition(sense_key: u8) -> u32 {
    ((sense_key as u32) << 12) | 0x41
}

pub const ATA_NOP: u8 = 0x00;
pub const ATA_STANDBY_IMMEDIATE: u8 = 0xE0;
pub const ATA_IDLE_IMMEDIATE: u8 = 0xE1;
pub const ATA_STANDBY_CMD: u8 = 0xE2;
pub const ATA_IDLE_CMD: u8 = 0xE3;
pub const ATA_CHECK_POWER_MODE: u8 = 0xE5;
pub const ATA_SLEEP: u8 = 0xE6;
pub const ATA_ATA_IDENTIFY: u8 = 0xEC;
pub const ATA_SETFEATURES: u8 = 0xEF;
pub const ATA_ATAPI_IDENTIFY: u8 = 0xA1;
pub const ATA_PACKET_CMD: u8 = 0xA0;
pub const ATA_SMART_CMD: u8 = 0xB0;
pub const ATA_SECURITY_FREEZE_LOCK: u8 = 0xF5;
pub const ATA_READ_VERIFY: u8 = 0x40;
pub const ATA_READ_VERIFY48: u8 = 0x42;

pub const SCSI_TEST_UNIT_READY: u8 = 0x00;
pub const SCSI_REQUEST_SENSE: u8 = 0x03;
pub const SCSI_INQUIRY: u8 = 0x12;
pub const SCSI_START_STOP_UNIT: u8 = 0x1B;
pub const SCSI_PREVENT_ALLOW: u8 = 0x1E;
pub const SCSI_READ_CAPACITY: u8 = 0x25;
pub const SCSI_READ_10: u8 = 0x28;
pub const SCSI_READ_TOC: u8 = 0x43;
pub const SCSI_GET_EVENT_STATUS: u8 = 0x4A;
pub const SCSI_MODE_SENSE_10: u8 = 0x5A;
pub const SCSI_REPORT_LUNS: u8 = 0xA0;
pub const SCSI_READ_12: u8 = 0xA8;

pub const SENSE_NOT_READY: u8 = 0x02;
pub const SENSE_ILLEGAL_REQUEST: u8 = 0x05;
pub const SENSE_UNIT_ATTENTION: u8 = 0x06;

pub const ASC_INVALID_OPCODE: u8 = 0x20;
pub const ASC_LBA_OUT_OF_RANGE: u8 = 0x21;
pub const ASC_INVALID_FIELD_IN_CDB: u8 = 0x24;
pub const ASC_MEDIA_REMOVAL_PREVENTED: u8 = 0x53;
pub const ASC_SAVING_PARAMS_NOT_SUPPORTED: u8 = 0x39;

pub const MODEPAGE_RW_ERROR_RECOVERY: u8 = 0x01;
pub const MODEPAGE_CD_CAPABILITIES: u8 = 0x2A;

pub use vmm_devices::pci::bits::PCI_VENDOR_INTEL;
pub const PCI_DEVICE_ICH8_AHCI: u16 = 0x2821;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_value_field_decomposition() {
        assert_eq!(CAP_VALUE & 0x1F, 0);
        assert_eq!((CAP_VALUE >> 8) & 0x1F, 31);
        assert_eq!((CAP_VALUE >> 20) & 0xF, 3);
        assert_ne!(CAP_VALUE & (1 << 31), 0);
        assert_eq!(CAP_VALUE & (1 << 30), 0);
        assert_ne!(CAP_VALUE & (1 << 24), 0);
    }

    #[test]
    fn pxcmd_wmask_matches_named_bits() {
        assert_eq!(
            PXCMD_WMASK,
            PXCMD_ST
                | PXCMD_SUD
                | PXCMD_POD
                | PXCMD_CLO
                | PXCMD_FRE
                | PXCMD_APSTE
                | PXCMD_ATAPI
                | PXCMD_DLAE
                | PXCMD_ALPE
                | PXCMD_ASP
                | PXCMD_ICC_MASK
        );
    }

    #[test]
    fn pxie_wmask_excludes_reserved_bit25() {
        assert_eq!(PXIE_WMASK & (1 << 25), 0);
        assert_eq!(PXIE_WMASK, 0xFDC0_00FF);
    }

    #[test]
    fn atapi_reset_tfd_has_no_drdy() {
        assert_eq!(PXTFD_RESET_ATAPI & ATA_S_READY, 0);
        assert_eq!(PXTFD_RESET_ATAPI & (ATA_S_BUSY | ATA_S_DRQ), 0);
    }

    #[test]
    fn tfd_check_condition_encodes_sense_key() {
        let value = tfd_check_condition(SENSE_ILLEGAL_REQUEST);
        assert_eq!(value, 0x5041);
        assert_eq!((value >> 12) & 0xF, u32::from(SENSE_ILLEGAL_REQUEST));
    }

    #[test]
    fn abar_size_covers_port_block() {
        assert!(AHCI_OFFSET + NUM_PORTS * AHCI_STEP <= ABAR_SIZE as usize);
        assert!(ABAR_SIZE.is_power_of_two());
    }
}
