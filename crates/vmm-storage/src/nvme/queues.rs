// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Queue state and completion posting.

use std::io;

use vmm_core::mem::MemCtx;
use vmm_devices::pci::msix::MsixTable;

use super::bits::*;

/// One queue as the driver created it.
///
/// The driver can delete a queue and create it again in the same slot,
/// so work in flight names the instance it was submitted against. A
/// completion whose instance is gone belongs to a queue the driver has
/// freed.
pub(super) type QueueInstance = u64;

pub(super) struct SubQueue {
    /// Guest physical address of the queue base.
    pub(super) base: u64,
    /// Number of entries, not 0-based.
    pub(super) size: u16,
    /// Consumer side, advanced by the controller.
    pub(super) head: u16,
    /// Set by the guest through the doorbell.
    pub(super) tail: u16,
    pub(super) cq_id: u16,
    pub(super) instance: QueueInstance,
}

pub(super) struct CompQueue {
    /// Guest physical address of the queue base.
    pub(super) base: u64,
    /// Number of entries, not 0-based.
    pub(super) size: u16,
    /// Set by the guest through the doorbell.
    pub(super) head: u16,
    /// Producer side, advanced by the controller.
    pub(super) tail: u16,
    /// Phase tag. It toggles on each wrap of the CQ.
    pub(super) phase: bool,
    /// MSI-X interrupt vector for this CQ.
    pub(super) iv: u16,
    /// Whether the driver asked for an interrupt on this CQ.
    pub(super) ien: bool,
    pub(super) instance: QueueInstance,
}

pub(super) fn io_queue_index(qid: u16) -> Option<usize> {
    let idx = usize::from(qid.checked_sub(1)?);
    (idx < MAX_IO_QUEUES).then_some(idx)
}

pub(super) struct NvmeState {
    // Controller registers
    pub(super) cap: u64,
    pub(super) cc: u32,
    pub(super) csts: u32,
    pub(super) aqa: u32,
    pub(super) asq_base: u64,
    pub(super) acq_base: u64,

    // Queues
    pub(super) admin_sq: Option<SubQueue>,
    pub(super) admin_cq: Option<CompQueue>,
    pub(super) io_sqs: [Option<SubQueue>; MAX_IO_QUEUES],
    pub(super) io_cqs: [Option<CompQueue>; MAX_IO_QUEUES],

    // Pre-built Identify data
    pub(super) ctrl_ident: [u8; IDENT_CTRL_SIZE],
    pub(super) ns_ident: [u8; IDENT_NS_SIZE],

    // Configuration
    pub(super) num_io_queues: u16,

    /// Source of [`QueueInstance`] values. Never reset, so an instance
    /// is unique for the life of the controller.
    pub(super) instances: QueueInstance,

    /// Whether a fence deadline passed with a worker still able to
    /// reach guest memory. Sticky: see [`Self::fail_fence`].
    pub(super) fence_failed: bool,
}

impl NvmeState {
    /// Take the queues away from the driver and from anything in
    /// flight against them.
    pub(super) fn retire_queues(&mut self) {
        self.admin_sq = None;
        self.admin_cq = None;
        for sq in &mut self.io_sqs {
            *sq = None;
        }
        for cq in &mut self.io_cqs {
            *cq = None;
        }
    }

    pub(super) fn next_instance(&mut self) -> QueueInstance {
        self.instances += 1;
        self.instances
    }

    /// Record a fence deadline that passed with a transfer still in
    /// the backend.
    ///
    /// The controller is fatal from here and never comes back up. The
    /// worker that missed the deadline can still read or write the
    /// guest pages that a queue created after this would name, so
    /// nothing may be admitted again.
    pub(super) fn fail_fence(&mut self) {
        self.fence_failed = true;
        self.csts |= CSTS_CFS;
    }

    pub(super) fn set_shst(&mut self, shst: u32) {
        self.csts = (self.csts & !(CSTS_SHST_MASK << CSTS_SHST_SHIFT))
            | (shst << CSTS_SHST_SHIFT);
    }
}

/// Post a completion queue entry to guest memory and fire MSI-X.
///
/// An error means the CQE could not be written, which loses the
/// completion: the caller reports a fatal controller status.
pub(super) fn post_completion(
    mem: &MemCtx,
    msix: &MsixTable,
    cq_opt: &mut Option<CompQueue>,
    cdw0: u32,
    sqid: u16,
    sq_head: u16,
    cid: u16,
    status: u16,
) -> io::Result<()> {
    let cq = match cq_opt.as_mut() {
        Some(cq) => cq,
        None => return Ok(()),
    };

    // The CQ is full when (tail + 1) % size == head.
    let next_tail = if cq.tail + 1 >= cq.size {
        0
    } else {
        cq.tail + 1
    };
    if next_tail == cq.head {
        // A full CQ drops the completion. The guest must drain the queue.
        return Ok(());
    }

    let cqe_offset = u64::from(cq.tail) * CQE_SIZE as u64;
    let Some(cqe_gpa) = cq.base.checked_add(cqe_offset) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "CQ base wraps",
        ));
    };

    let mut cqe = [0u8; CQE_SIZE];

    // DW0: command-specific result
    cqe[CQE_DW0..CQE_DW0 + 4].copy_from_slice(&cdw0.to_le_bytes());
    // DW1: reserved (already zero)

    // DW2: SQ head pointer + SQ ID
    cqe[CQE_SQHD..CQE_SQHD + 2].copy_from_slice(&sq_head.to_le_bytes());
    cqe[CQE_SQID..CQE_SQID + 2].copy_from_slice(&sqid.to_le_bytes());

    // DW3: CID and status. Status bits 15:1 hold the status code and
    // bit 0 the phase tag.
    cqe[CQE_CID..CQE_CID + 2].copy_from_slice(&cid.to_le_bytes());

    let status_with_phase = (status << 1) | if cq.phase { 1 } else { 0 };
    cqe[CQE_STATUS..CQE_STATUS + 2]
        .copy_from_slice(&status_with_phase.to_le_bytes());

    mem.write(cqe_gpa, &cqe)?;

    let (iv, ien) = (cq.iv, cq.ien);
    cq.tail += 1;
    if cq.tail >= cq.size {
        cq.tail = 0;
        cq.phase = !cq.phase;
    }

    if ien {
        msix.fire(iv);
    }
    Ok(())
}

/// Whether a guest-supplied queue base may be used.
///
/// Queues are contiguous (CAP.CQR is set), so the base must be page
/// aligned. Zero is never a queue.
pub(super) fn valid_queue_base(prp1: u64) -> bool {
    prp1 != 0 && (prp1 & 0xFFF) == 0
}

#[cfg(test)]
impl Default for NvmeState {
    fn default() -> Self {
        Self {
            cap: 0,
            cc: 0,
            csts: 0,
            aqa: 0,
            asq_base: 0,
            acq_base: 0,
            admin_sq: None,
            admin_cq: None,
            io_sqs: Default::default(),
            io_cqs: Default::default(),
            ctrl_ident: [0u8; IDENT_CTRL_SIZE],
            ns_ident: [0u8; IDENT_NS_SIZE],
            num_io_queues: MAX_IO_QUEUES as u16,
            instances: 0,
            fence_failed: false,
        }
    }
}
