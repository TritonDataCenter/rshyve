// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! NVMe register, command, and data structure definitions.
//!
//! Layouts follow NVMe 1.0e. All multi-byte fields are little-endian,
//! the x86 native byte order.

// ── Controller register offsets (BAR0) ──────────────────────────

pub const REG_CAP: usize = 0x00; // Controller Capabilities (8 bytes)
pub const REG_VS: usize = 0x08; // Version (4 bytes)
pub const REG_INTMS: usize = 0x0C; // Interrupt Mask Set (4 bytes)
pub const REG_INTMC: usize = 0x10; // Interrupt Mask Clear (4 bytes)
pub const REG_CC: usize = 0x14; // Controller Configuration (4 bytes)
pub const REG_CSTS: usize = 0x1C; // Controller Status (4 bytes)
pub const REG_AQA: usize = 0x24; // Admin Queue Attributes (4 bytes)
pub const REG_ASQ: usize = 0x28; // Admin Submission Queue Base (8 bytes)
pub const REG_ACQ: usize = 0x30; // Admin Completion Queue Base (8 bytes)
pub const REG_DOORBELL_BASE: usize = 0x1000; // Start of doorbell registers

pub const NVME_VS_1_0: u32 = 0x0001_0000;

// ── CAP register fields ────────────────────────────────────────

/// Maximum Queue Entries Supported (0-based, 16 bits)
pub const CAP_MQES_SHIFT: u64 = 0;
pub const CAP_MQES_MASK: u64 = 0xFFFF;
/// Contiguous Queues Required
pub const CAP_CQR: u64 = 1 << 16;
/// Timeout (in 500ms units, 8 bits at bits 31:24)
pub const CAP_TO_SHIFT: u64 = 24;
/// Doorbell Stride (4 bits at bits 35:32)
pub const CAP_DSTRD_SHIFT: u64 = 32;
/// NVM Command Set supported
pub const CAP_CSS_NVM: u64 = 1 << 37;
/// Memory Page Size Minimum (4 bits at bits 51:48)
pub const CAP_MPSMIN_SHIFT: u64 = 48;
/// Memory Page Size Maximum (4 bits at bits 55:52)
pub const CAP_MPSMAX_SHIFT: u64 = 52;

// ── CC register fields ─────────────────────────────────────────

/// Enable (bit 0)
pub const CC_EN: u32 = 1 << 0;
/// I/O Command Set Selected (bits 6:4). 0 = NVM.
pub const CC_CSS_SHIFT: u32 = 4;
pub const CC_CSS_MASK: u32 = 0x7;
/// Memory Page Size (bits 10:7). 0 = 4K.
pub const CC_MPS_SHIFT: u32 = 7;
pub const CC_MPS_MASK: u32 = 0xF;
/// Arbitration Mechanism (bits 13:11). 0 = Round Robin.
pub const CC_AMS_SHIFT: u32 = 11;
pub const CC_AMS_MASK: u32 = 0x7;
/// Shutdown Notification (bits 15:14)
pub const CC_SHN_SHIFT: u32 = 14;
pub const CC_SHN_MASK: u32 = 0x3;
/// I/O Submission Queue Entry Size (bits 19:16), log2. Must be 6 (64 bytes).
pub const CC_IOSQES_SHIFT: u32 = 16;
pub const CC_IOSQES_MASK: u32 = 0xF;
/// I/O Completion Queue Entry Size (bits 23:20), log2. Must be 4 (16 bytes).
pub const CC_IOCQES_SHIFT: u32 = 20;
pub const CC_IOCQES_MASK: u32 = 0xF;

pub const SQE_SIZE_LOG2: u32 = 6; // 64 bytes
pub const CQE_SIZE_LOG2: u32 = 4; // 16 bytes

// ── CSTS register fields ───────────────────────────────────────

/// Ready (bit 0)
pub const CSTS_RDY: u32 = 1 << 0;
/// Controller Fatal Status (bit 1)
pub const CSTS_CFS: u32 = 1 << 1;
/// Shutdown Status (bits 3:2)
pub const CSTS_SHST_SHIFT: u32 = 2;
pub const CSTS_SHST_MASK: u32 = 0x3;
pub const SHST_NORMAL: u32 = 0;
pub const SHST_OCCURRING: u32 = 1;
pub const SHST_COMPLETE: u32 = 2;

// ── AQA register fields ───────────────────────────────────────

/// Admin Submission Queue Size (bits 11:0, 0-based)
pub const AQA_ASQS_MASK: u32 = 0xFFF;
/// Admin Completion Queue Size (bits 27:16, 0-based)
pub const AQA_ACQS_SHIFT: u32 = 16;
pub const AQA_ACQS_MASK: u32 = 0xFFF;

// ── Admin command opcodes ──────────────────────────────────────

pub const ADMIN_OPC_DELETE_IO_SQ: u8 = 0x00;
pub const ADMIN_OPC_CREATE_IO_SQ: u8 = 0x01;
pub const ADMIN_OPC_GET_LOG_PAGE: u8 = 0x02;
pub const ADMIN_OPC_DELETE_IO_CQ: u8 = 0x04;
pub const ADMIN_OPC_CREATE_IO_CQ: u8 = 0x05;
pub const ADMIN_OPC_IDENTIFY: u8 = 0x06;
pub const ADMIN_OPC_ABORT: u8 = 0x08;
pub const ADMIN_OPC_SET_FEATURES: u8 = 0x09;
pub const ADMIN_OPC_GET_FEATURES: u8 = 0x0A;

// ── NVM I/O command opcodes ────────────────────────────────────

pub const NVM_OPC_FLUSH: u8 = 0x00;
pub const NVM_OPC_WRITE: u8 = 0x01;
pub const NVM_OPC_READ: u8 = 0x02;

// ── Identify CNS values ────────────────────────────────────────

pub const IDENTIFY_CNS_NAMESPACE: u8 = 0x00;
pub const IDENTIFY_CNS_CONTROLLER: u8 = 0x01;

// ── Log page identifiers, NVMe 1.0e section 5.10 ───────────────

pub const LID_ERROR_INFO: u8 = 0x01;
pub const LID_SMART: u8 = 0x02;
pub const LID_FIRMWARE_SLOT: u8 = 0x03;

/// One Error Information log entry.
pub const LOG_ERROR_SIZE: usize = 64;
/// SMART / Health Information.
pub const LOG_SMART_SIZE: usize = 512;
/// Firmware Slot Information.
pub const LOG_FIRMWARE_SIZE: usize = 512;

/// Composite temperature this controller reports, in Kelvin.
///
/// Zero reads as absolute zero and puts some drivers into a thermal
/// warning path.
pub const LOG_SMART_TEMP_K: u16 = 300;

/// Build log page `lid`, or `None` when this controller has no such log.
pub fn build_log_page(lid: u8) -> Option<Vec<u8>> {
    match lid {
        // No error has ever been logged, so the entry is all zero.
        LID_ERROR_INFO => Some(vec![0u8; LOG_ERROR_SIZE]),
        LID_SMART => {
            let mut data = vec![0u8; LOG_SMART_SIZE];
            data[1..3].copy_from_slice(&LOG_SMART_TEMP_K.to_le_bytes());
            data[3] = 100; // Available spare, percent
            data[4] = 10; // Available spare threshold, percent
            Some(data)
        }
        LID_FIRMWARE_SLOT => {
            let mut data = vec![0u8; LOG_FIRMWARE_SIZE];
            data[0] = 1; // Active firmware: slot 1
            Some(data)
        }
        _ => None,
    }
}

// ── Feature IDs ────────────────────────────────────────────────

pub const FEAT_ARBITRATION: u8 = 0x01;
pub const FEAT_POWER_MGMT: u8 = 0x02;
pub const FEAT_TEMP_THRESHOLD: u8 = 0x04;
pub const FEAT_ERROR_RECOVERY: u8 = 0x05;
pub const FEAT_VOLATILE_WC: u8 = 0x06;
pub const FEAT_NUM_QUEUES: u8 = 0x07;
pub const FEAT_INTR_COALESCING: u8 = 0x08;
pub const FEAT_INTR_VECTOR_CFG: u8 = 0x09;
pub const FEAT_WRITE_ATOMICITY: u8 = 0x0A;
pub const FEAT_ASYNC_EVENT_CFG: u8 = 0x0B;

// ── Status codes ───────────────────────────────────────────────

pub const SC_SUCCESS: u16 = 0x0000;
pub const SC_INVALID_OPCODE: u16 = 0x0001;
pub const SC_INVALID_FIELD: u16 = 0x0002;
pub const SC_DATA_XFER_ERROR: u16 = 0x0004;
pub const SC_INTERNAL_ERROR: u16 = 0x0006;
pub const SC_INVALID_NS: u16 = 0x000B;
/// I/O command set specific generic status, NVMe 1.4 figure 127.
pub const SC_WRITE_TO_RO_RANGE: u16 = 0x0082;

// Command specific status, NVMe 1.4 figure 128. Bits 10:8 carry the
// status code type, so these read as SCT=1, SC=n.
pub const SC_CQ_INVALID: u16 = 0x0100;
pub const SC_INVALID_QUEUE_ID: u16 = 0x0101;
pub const SC_INVALID_QUEUE_SIZE: u16 = 0x0102;
pub const SC_INVALID_INTR_VECTOR: u16 = 0x0108;
pub const SC_INVALID_LOG_PAGE: u16 = 0x0109;

/// Do Not Retry flag in status
pub const SC_DNR: u16 = 1 << 14;

// ── Submission Queue Entry (64 bytes) ──────────────────────────

pub const SQE_SIZE: usize = 64;

/// Offsets within a Submission Queue Entry.
pub const SQE_OPC: usize = 0; // Opcode (byte)
pub const SQE_FLAGS: usize = 1; // Fused + PSDT
pub const SQE_CID: usize = 2; // Command ID (u16)
pub const SQE_NSID: usize = 4; // Namespace ID (u32)
pub const SQE_PRP1: usize = 24; // PRP Entry 1 (u64)
pub const SQE_PRP2: usize = 32; // PRP Entry 2 (u64)
pub const SQE_CDW10: usize = 40; // Command DWord 10 (u32)
pub const SQE_CDW11: usize = 44; // Command DWord 11 (u32)
pub const SQE_CDW12: usize = 48; // Command DWord 12 (u32)
pub const SQE_CDW13: usize = 52; // Command DWord 13 (u32)
pub const SQE_CDW14: usize = 56; // Command DWord 14 (u32)
pub const SQE_CDW15: usize = 60; // Command DWord 15 (u32)

// ── Completion Queue Entry (16 bytes) ──────────────────────────

pub const CQE_SIZE: usize = 16;

pub const CQE_DW0: usize = 0; // Command-specific result
pub const CQE_DW1: usize = 4; // Reserved
pub const CQE_SQHD: usize = 8; // SQ Head Pointer (u16)
pub const CQE_SQID: usize = 10; // SQ Identifier (u16)
pub const CQE_CID: usize = 12; // Command Identifier (u16)
pub const CQE_STATUS: usize = 14; // Status Field (u16, includes phase bit)

// ── Create I/O Queue command fields (CDW11) ────────────────────

/// Physically Contiguous. Always set: CAP.CQR requires it.
pub const CQ_PC: u32 = 1 << 0;
/// Interrupts Enabled for this completion queue.
pub const CQ_IEN: u32 = 1 << 1;

// ── Queue limits ───────────────────────────────────────────────

pub const MAX_IO_QUEUES: usize = 15;
pub const MAX_QUEUE_SIZE: u16 = 4096;
pub const ADMIN_QUEUE_ID: u16 = 0;

/// MDTS as log2 pages: 2 MiB, the C bhyve value.
pub const MDTS_LOG2_PAGES: u8 = 9;
/// The largest transfer one command may name.
pub const MAX_XFER_BYTES: u64 = (1 << MDTS_LOG2_PAGES) * 4096;

// ── MSI-X ──────────────────────────────────────────────────────

/// MSI-X vectors: the admin queue, every I/O queue the controller
/// allows, and one the driver may ask for but never gets a queue for.
pub const NVME_MSIX_COUNT: u16 = MAX_IO_QUEUES as u16 + 2;

// ── BAR sizes ──────────────────────────────────────────────────

/// Controller registers and doorbells: the 0x1000 doorbell base plus 8
/// bytes per queue pair (SQ tail and CQ head). 0x1000 + 16*2*4 = 0x1080,
/// rounded up to 16K.
pub const BAR0_SIZE: u64 = 0x4000;

// ── PCI identity ───────────────────────────────────────────────

pub const PCI_VENDOR_ID: u16 = 0xFB5D; // bhyve NVMe vendor
pub const PCI_DEVICE_ID: u16 = 0x0A0A; // bhyve NVMe device
pub const PCI_CLASS_STORAGE: u8 = 0x01;
pub const PCI_SUBCLASS_NVM: u8 = 0x08;
pub const PCI_PROGIF_NVME: u8 = 0x02;

// ── Identify Controller data (4096 bytes) ──────────────────────

pub const IDENT_CTRL_SIZE: usize = 4096;

/// Build an Identify Controller data structure.
///
/// `cntlid` must be unique within the subsystem.
pub fn build_identify_controller(
    serial: &str,
    model: &str,
    firmware: &str,
    max_namespaces: u32,
    mdts: u8,
    cntlid: u16,
) -> [u8; IDENT_CTRL_SIZE] {
    let mut data = [0u8; IDENT_CTRL_SIZE];

    // PCI Vendor ID (bytes 0-1)
    data[0..2].copy_from_slice(&PCI_VENDOR_ID.to_le_bytes());
    // PCI Subsystem Vendor ID (bytes 2-3)
    data[2..4].copy_from_slice(&PCI_VENDOR_ID.to_le_bytes());

    // Serial Number (bytes 4-23, ASCII, space-padded)
    let sn = format!("{:<20}", serial);
    data[4..24].copy_from_slice(&sn.as_bytes()[..20]);

    // Model Number (bytes 24-63, ASCII, space-padded)
    let mn = format!("{:<40}", model);
    data[24..64].copy_from_slice(&mn.as_bytes()[..40]);

    // Firmware Revision (bytes 64-71, ASCII, space-padded)
    let fr = format!("{:<8}", firmware);
    data[64..72].copy_from_slice(&fr.as_bytes()[..8]);

    // CNTLID - Controller ID (bytes 78-79)
    data[78..80].copy_from_slice(&cntlid.to_le_bytes());

    // MDTS - Maximum Data Transfer Size (byte 77)
    data[77] = mdts;

    // OACS - Optional Admin Command Support (bytes 256-257)
    // 0 = no optional admin commands
    data[256..258].copy_from_slice(&0u16.to_le_bytes());

    // SQES - Submission Queue Entry Size (byte 512)
    // Required = 6 (64 bytes), Maximum = 6
    data[512] = (SQE_SIZE_LOG2 as u8) | ((SQE_SIZE_LOG2 as u8) << 4);

    // CQES - Completion Queue Entry Size (byte 513)
    // Required = 4 (16 bytes), Maximum = 4
    data[513] = (CQE_SIZE_LOG2 as u8) | ((CQE_SIZE_LOG2 as u8) << 4);

    // NN - Number of Namespaces (bytes 516-519)
    data[516..520].copy_from_slice(&max_namespaces.to_le_bytes());

    // VWC - Volatile Write Cache (byte 525)
    // Bit 0 = volatile write cache present
    data[525] = 1;

    data
}

// ── Identify Namespace data (4096 bytes) ────────────────────────

pub const IDENT_NS_SIZE: usize = 4096;

/// Build an Identify Namespace data structure.
pub fn build_identify_namespace(
    total_blocks: u64,
    block_size: u32,
    read_only: bool,
) -> [u8; IDENT_NS_SIZE] {
    let mut data = [0u8; IDENT_NS_SIZE];

    // NSZE - Namespace Size (bytes 0-7)
    data[0..8].copy_from_slice(&total_blocks.to_le_bytes());
    // NCAP - Namespace Capacity (bytes 8-15)
    data[8..16].copy_from_slice(&total_blocks.to_le_bytes());
    // NUSE - Namespace Utilization (bytes 16-23)
    data[16..24].copy_from_slice(&total_blocks.to_le_bytes());

    // NSATTR - Namespace Attributes (byte 99), bit 0 = write protected
    if read_only {
        data[99] = 1;
    }

    // NLBAF - Number of LBA Formats (byte 25), 0-based
    data[25] = 0; // 1 format (index 0)

    // FLBAS - Formatted LBA Size (byte 26)
    // bits 3:0 = index of current LBA format = 0
    data[26] = 0;

    // LBA Format 0 (bytes 128-131)
    // bits 23:16 = LBADS (LBA Data Size, log2)
    // bits 1:0 = RP (Relative Performance) = 0 (best)
    let lbads = block_size.trailing_zeros() as u8; // e.g., 512 → 9, 4096 → 12
    data[128..132].copy_from_slice(&(u32::from(lbads) << 16).to_le_bytes());

    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqe_cqe_sizes() {
        assert_eq!(SQE_SIZE, 64);
        assert_eq!(CQE_SIZE, 16);
        assert_eq!(1 << SQE_SIZE_LOG2, SQE_SIZE as u32);
        assert_eq!(1 << CQE_SIZE_LOG2, CQE_SIZE as u32);
    }

    #[test]
    fn identify_controller_cntlid_survives_a_one_character_serial() {
        let data = build_identify_controller("X", "VMM NVMe", "1.0", 1, 5, 7);
        let cntlid = u16::from_le_bytes(data[78..80].try_into().unwrap());
        assert_eq!(cntlid, 7);
    }

    #[test]
    fn identify_controller_serial() {
        let data =
            build_identify_controller("SN001", "VMM NVMe", "1.0", 1, 5, 2);
        let sn = std::str::from_utf8(&data[4..24]).unwrap();
        assert!(sn.starts_with("SN001"));
    }

    #[test]
    fn identify_namespace_capacity() {
        let data = build_identify_namespace(1000, 512, false);
        let nsze = u64::from_le_bytes(data[0..8].try_into().unwrap());
        assert_eq!(nsze, 1000);
    }

    #[test]
    fn identify_namespace_block_size() {
        let data = build_identify_namespace(1000, 4096, false);
        let lbaf0 = u32::from_le_bytes(data[128..132].try_into().unwrap());
        let lbads = (lbaf0 >> 16) & 0xFF;
        assert_eq!(lbads, 12); // log2(4096) = 12
    }

    #[test]
    fn doorbell_base_offset() {
        assert_eq!(REG_DOORBELL_BASE, 0x1000);
    }
}
