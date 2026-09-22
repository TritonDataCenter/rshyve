// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! I/O submission and execution.

use std::fs::File;
use std::io;
use std::sync::Arc;

use vmm_core::mem::{GuestIoVec, MemCtx};

use super::bits::*;
use super::prp::prp_to_iovec;
use super::queues::{io_queue_index, NvmeState, QueueInstance};
use super::{Geometry, NvmeController};

/// The store a namespace is backed by.
///
/// A trait rather than `File` so a test can hold a transfer open and
/// watch what a reset or a shutdown does with it.
pub trait IoBackend: Send + Sync + 'static {
    fn read_at(&self, iov: &GuestIoVec, offset: u64) -> io::Result<usize>;
    fn write_at(&self, iov: &GuestIoVec, offset: u64) -> io::Result<usize>;
    fn sync(&self) -> io::Result<()>;
}

impl IoBackend for File {
    fn read_at(&self, iov: &GuestIoVec, offset: u64) -> io::Result<usize> {
        iov.read_from(self, offset)
    }

    fn write_at(&self, iov: &GuestIoVec, offset: u64) -> io::Result<usize> {
        iov.write_to(self, offset)
    }

    fn sync(&self) -> io::Result<()> {
        self.sync_data()
    }
}

/// One NVM command handed to a worker.
pub(super) struct IoRequest {
    pub(super) opcode: u8,
    pub(super) nsid: u32,
    pub(super) slba: u64,
    pub(super) nlb: u32,
    pub(super) prp1: u64,
    pub(super) prp2: u64,
    pub(super) sqid: u16,
    pub(super) cid: u16,
    pub(super) cq_id: u16,
    /// The queue instances this command was submitted against. A
    /// request whose instances are gone names pages and a CQ slot the
    /// driver has taken back.
    pub(super) sq_instance: QueueInstance,
    pub(super) cq_instance: QueueInstance,
    /// Force Unit Access. Identify advertises VWC=1, so a FUA write
    /// must reach stable storage before its completion.
    pub(super) fua: bool,
}

impl NvmeController {
    /// Process I/O SQ doorbell: parse SQEs and submit to worker channel.
    pub(super) fn io_sq_doorbell(&self, qid: u16, new_tail: u16) {
        let mem = MemCtx::new(Arc::clone(&self.physmap));
        let mut st = self.state.lock().expect("nvme: state lock");

        let Some(idx) = io_queue_index(qid) else {
            return;
        };
        let Some((cq_id, sq_instance)) =
            st.io_sqs[idx].as_ref().map(|sq| (sq.cq_id, sq.instance))
        else {
            return;
        };
        // Nothing submitted here can complete without its CQ.
        let Some(cq_instance) = io_queue_index(cq_id)
            .and_then(|cq_idx| st.io_cqs[cq_idx].as_ref())
            .map(|cq| cq.instance)
        else {
            return;
        };

        let Some(sq) = st.io_sqs[idx].as_mut() else {
            return;
        };
        if new_tail >= sq.size {
            return;
        }
        sq.tail = new_tail;

        let mut requests = Vec::new();

        while sq.head != sq.tail {
            let sqe_offset = u64::from(sq.head) * SQE_SIZE as u64;
            let mut sqe_buf = [0u8; SQE_SIZE];
            let fetched = match sq.base.checked_add(sqe_offset) {
                Some(gpa) => mem.read(gpa, &mut sqe_buf).is_ok(),
                None => false,
            };
            if !fetched {
                sq.head = (sq.head + 1) % sq.size;
                continue;
            }

            let opcode = sqe_buf[SQE_OPC];
            let cid =
                u16::from_le_bytes([sqe_buf[SQE_CID], sqe_buf[SQE_CID + 1]]);
            let nsid = u32::from_le_bytes(
                sqe_buf[SQE_NSID..SQE_NSID + 4].try_into().expect("4 bytes"),
            );
            let prp1 = u64::from_le_bytes(
                sqe_buf[SQE_PRP1..SQE_PRP1 + 8].try_into().expect("8 bytes"),
            );
            let prp2 = u64::from_le_bytes(
                sqe_buf[SQE_PRP2..SQE_PRP2 + 8].try_into().expect("8 bytes"),
            );
            let cdw10 = u32::from_le_bytes(
                sqe_buf[SQE_CDW10..SQE_CDW10 + 4]
                    .try_into()
                    .expect("4 bytes"),
            );
            let cdw12 = u32::from_le_bytes(
                sqe_buf[SQE_CDW12..SQE_CDW12 + 4]
                    .try_into()
                    .expect("4 bytes"),
            );

            // NVM Read and Write: CDW10-11 hold the 64-bit SLBA and CDW12
            // bits 15:0 the 0-based NLB.
            let slba = u64::from(cdw10)
                | (u64::from(u32::from_le_bytes(
                    sqe_buf[SQE_CDW11..SQE_CDW11 + 4]
                        .try_into()
                        .expect("4 bytes"),
                )) << 32);
            let nlb = (cdw12 & 0xFFFF) + 1;
            // CDW12 bit 30 is Force Unit Access.
            let fua = (cdw12 & (1u32 << 30)) != 0;

            sq.head = (sq.head + 1) % sq.size;

            requests.push(IoRequest {
                opcode,
                nsid,
                slba,
                nlb,
                prp1,
                prp2,
                sqid: qid,
                cid,
                cq_id,
                sq_instance,
                cq_instance,
                fua,
            });
        }

        drop(st);

        let tx_guard = self.io_tx.lock().expect("nvme: io_tx lock");
        let Some(tx) = tx_guard.as_ref() else {
            slog::warn!(self.log, "nvme: I/O dropped, the workers are gone";
                "count" => requests.len());
            return;
        };
        for req in requests {
            if let Err(error) = tx.send(req) {
                slog::warn!(self.log, "nvme: I/O dropped, the workers are gone";
                    "cid" => error.0.cid);
            }
        }
    }
}

/// Whether the queues a request named are still the ones the driver
/// has.
///
/// A request that outlived its queue must not touch guest memory or
/// post a completion: those pages and that CQ slot belong to whatever
/// the driver put there next.
pub(super) fn queues_current(st: &NvmeState, req: &IoRequest) -> bool {
    let sq_live = io_queue_index(req.sqid)
        .and_then(|idx| st.io_sqs[idx].as_ref())
        .is_some_and(|sq| sq.instance == req.sq_instance);
    let cq_live = io_queue_index(req.cq_id)
        .and_then(|idx| st.io_cqs[idx].as_ref())
        .is_some_and(|cq| cq.instance == req.cq_instance);
    sq_live && cq_live
}

/// Execute a single I/O request (Read, Write, or Flush).
///
/// Returns the NVMe status code for the completion entry.
pub(super) fn process_io_request(
    req: &IoRequest,
    backend: &dyn IoBackend,
    read_only: bool,
    mem: &MemCtx,
    geometry: Geometry,
) -> u16 {
    if req.nsid != 1 {
        return SC_INVALID_NS | SC_DNR;
    }

    let Geometry {
        block_size,
        total_blocks,
    } = geometry;

    let byte_count = match u64::from(req.nlb).checked_mul(u64::from(block_size))
    {
        Some(n) if n <= MAX_XFER_BYTES => n as usize,
        _ => return SC_INVALID_FIELD | SC_DNR,
    };
    let byte_offset = match req.slba.checked_mul(u64::from(block_size)) {
        Some(n) => n,
        None => return SC_INVALID_FIELD | SC_DNR,
    };
    if req
        .slba
        .checked_add(u64::from(req.nlb))
        .is_none_or(|end| end > total_blocks)
    {
        return SC_INVALID_FIELD | SC_DNR;
    }

    match req.opcode {
        NVM_OPC_READ => {
            let Ok(iov) = prp_to_iovec(mem, req.prp1, req.prp2, byte_count)
            else {
                return SC_DATA_XFER_ERROR;
            };
            match backend.read_at(&iov, byte_offset) {
                Ok(n) if n == byte_count => SC_SUCCESS,
                _ => SC_DATA_XFER_ERROR,
            }
        }
        NVM_OPC_WRITE => {
            if read_only {
                return SC_WRITE_TO_RO_RANGE | SC_DNR;
            }
            let Ok(iov) = prp_to_iovec(mem, req.prp1, req.prp2, byte_count)
            else {
                return SC_DATA_XFER_ERROR;
            };
            match backend.write_at(&iov, byte_offset) {
                Ok(n) if n == byte_count => {}
                _ => return SC_DATA_XFER_ERROR,
            }
            if req.fua && backend.sync().is_err() {
                return SC_INTERNAL_ERROR;
            }
            SC_SUCCESS
        }
        NVM_OPC_FLUSH => {
            if backend.sync().is_err() {
                return SC_INTERNAL_ERROR;
            }
            SC_SUCCESS
        }
        _ => SC_INVALID_OPCODE | SC_DNR,
    }
}
