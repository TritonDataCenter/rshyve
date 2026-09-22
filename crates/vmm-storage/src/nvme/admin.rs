// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Admin command set.

use std::sync::Arc;

use vmm_core::mem::MemCtx;

use super::bits::*;
use super::prp::write_to_prp;
use super::queues::{
    io_queue_index, post_completion, CompQueue, NvmeState, SubQueue,
};
use super::NvmeController;

impl NvmeController {
    /// Process admin SQ doorbell: parse and execute admin commands inline.
    pub(super) fn admin_sq_doorbell(&self, new_tail: u16) {
        let mem = MemCtx::new(Arc::clone(&self.physmap));
        let mut st = self.state.lock().expect("nvme: state lock");

        match &mut st.admin_sq {
            Some(sq) => {
                if new_tail >= sq.size {
                    return;
                }
                sq.tail = new_tail;
            }
            None => return,
        }

        // Borrow admin_sq again on each pass: process_admin_cmd borrows
        // all of NvmeState, so no admin_sq borrow may span the call.
        loop {
            let (sqe_gpa, sq_head, sq_size) = {
                let sq = match &st.admin_sq {
                    Some(sq) => sq,
                    None => return,
                };
                if sq.head == sq.tail {
                    break;
                }
                let sqe_offset = u64::from(sq.head) * SQE_SIZE as u64;
                (sq.base.checked_add(sqe_offset), sq.head, sq.size)
            };

            let mut sqe_buf = [0u8; SQE_SIZE];
            let fetched = match sqe_gpa {
                Some(gpa) => mem.read(gpa, &mut sqe_buf).is_ok(),
                None => false,
            };
            if !fetched {
                // Advance head past the entry that cannot be read.
                if let Some(sq) = &mut st.admin_sq {
                    sq.head = (sq.head + 1) % sq.size;
                }
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
            let cdw11 = u32::from_le_bytes(
                sqe_buf[SQE_CDW11..SQE_CDW11 + 4]
                    .try_into()
                    .expect("4 bytes"),
            );

            let sq_head = (sq_head + 1) % sq_size;
            if let Some(sq) = &mut st.admin_sq {
                sq.head = sq_head;
            }

            let (status, cdw0, fence) = self.process_admin_cmd(
                &mut st, &mem, opcode, nsid, prp1, prp2, cdw10, cdw11,
            );

            // Held across the completion: the driver takes a Delete
            // Queue completion as licence to reuse the queue's pages.
            let _fence = if fence {
                drop(st);
                let held = self.fence_workers(self.fences.reset);
                st = self.state.lock().expect("nvme: state lock");
                if held.is_none() {
                    st.fail_fence();
                }
                held
            } else {
                None
            };

            if let Err(error) = post_completion(
                &mem,
                &self.msix,
                &mut st.admin_cq,
                cdw0,
                ADMIN_QUEUE_ID,
                sq_head,
                cid,
                status,
            ) {
                // The driver will never see this command finish.
                slog::error!(self.log, "nvme: admin CQE write failed";
                    "error" => %error);
                st.csts |= CSTS_CFS;
            }
        }
    }

    /// Process a single admin command.
    ///
    /// Returns the status, the command-specific result, and whether the
    /// completion must wait for the worker pool: Delete Queue may only
    /// complete once the commands it took away have finished.
    fn process_admin_cmd(
        &self,
        st: &mut NvmeState,
        mem: &MemCtx,
        opcode: u8,
        nsid: u32,
        prp1: u64,
        prp2: u64,
        cdw10: u32,
        cdw11: u32,
    ) -> (u16, u32, bool) {
        let deletes_a_queue =
            matches!(opcode, ADMIN_OPC_DELETE_IO_SQ | ADMIN_OPC_DELETE_IO_CQ);
        let (status, cdw0) = match opcode {
            ADMIN_OPC_IDENTIFY => {
                self.admin_identify(st, mem, nsid, prp1, prp2, cdw10)
            }
            ADMIN_OPC_CREATE_IO_CQ => {
                self.admin_create_io_cq(st, prp1, cdw10, cdw11)
            }
            ADMIN_OPC_CREATE_IO_SQ => {
                self.admin_create_io_sq(st, prp1, cdw10, cdw11)
            }
            ADMIN_OPC_DELETE_IO_SQ => self.admin_delete_io_sq(st, cdw10),
            ADMIN_OPC_DELETE_IO_CQ => self.admin_delete_io_cq(st, cdw10),
            ADMIN_OPC_GET_FEATURES => Self::admin_get_features(st, cdw10),
            ADMIN_OPC_SET_FEATURES => {
                Self::admin_set_features(st, cdw10, cdw11)
            }
            ADMIN_OPC_ABORT => {
                // Abort always succeeds with CDW0 bit 0 clear: the command
                // was not found and not aborted.
                (SC_SUCCESS, 0)
            }
            ADMIN_OPC_GET_LOG_PAGE => {
                Self::admin_get_log_page(mem, prp1, prp2, cdw10)
            }
            _ => (SC_INVALID_OPCODE | (SC_DNR), 0),
        };
        (status, cdw0, deletes_a_queue && status == SC_SUCCESS)
    }

    /// Get Log Page.
    ///
    /// NUMD is 12 bits of dwords, NVMe 1.0e section 5.10. NVMe 1.2 split
    /// it into NUMDL and NUMDU. This controller reports 1.0, so the upper
    /// bits of CDW10 stay reserved.
    fn admin_get_log_page(
        mem: &MemCtx,
        prp1: u64,
        prp2: u64,
        cdw10: u32,
    ) -> (u16, u32) {
        let lid = (cdw10 & 0xFF) as u8;
        let dwords = ((cdw10 >> 16) & 0xFFF) as usize + 1;
        let Some(log) = build_log_page(lid) else {
            return (SC_INVALID_LOG_PAGE | SC_DNR, 0);
        };

        // The host asks for whole dwords. Any part past the end of the
        // log reads as zero.
        let mut data = vec![0u8; dwords * 4];
        let from_log = data.len().min(log.len());
        data[..from_log].copy_from_slice(&log[..from_log]);

        if write_to_prp(mem, prp1, prp2, &data).is_err() {
            return (SC_DATA_XFER_ERROR, 0);
        }
        (SC_SUCCESS, 0)
    }

    /// Identify command (CNS 0 = namespace, CNS 1 = controller).
    fn admin_identify(
        &self,
        st: &NvmeState,
        mem: &MemCtx,
        nsid: u32,
        prp1: u64,
        prp2: u64,
        cdw10: u32,
    ) -> (u16, u32) {
        let cns = (cdw10 & 0xFF) as u8;
        let data: &[u8] = match cns {
            IDENTIFY_CNS_NAMESPACE => {
                if nsid != 1 {
                    return (SC_INVALID_NS | SC_DNR, 0);
                }
                &st.ns_ident
            }
            IDENTIFY_CNS_CONTROLLER => &st.ctrl_ident,
            _ => return (SC_INVALID_FIELD | SC_DNR, 0),
        };

        if write_to_prp(mem, prp1, prp2, data).is_err() {
            return (SC_DATA_XFER_ERROR, 0);
        }
        (SC_SUCCESS, 0)
    }

    /// Create I/O Completion Queue.
    pub(super) fn admin_create_io_cq(
        &self,
        st: &mut NvmeState,
        prp1: u64,
        cdw10: u32,
        cdw11: u32,
    ) -> (u16, u32) {
        let qid = (cdw10 & 0xFFFF) as u16;
        let qsize = ((cdw10 >> 16) & 0xFFFF) + 1; // Convert 0-based to 1-based
        let iv = ((cdw11 >> 16) & 0xFFFF) as u16;
        let ien = (cdw11 & CQ_IEN) != 0;

        let Some(idx) = io_queue_index(qid) else {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        };
        if qid > st.num_io_queues {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        }
        if qsize < 2 || qsize > u32::from(MAX_QUEUE_SIZE) {
            return (SC_INVALID_QUEUE_SIZE | SC_DNR, 0);
        }
        let qsize = qsize as u16;

        if st.io_cqs[idx].is_some() {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        }
        // A vector the table cannot hold would leave the driver polling
        // a queue that never raises.
        if iv >= NVME_MSIX_COUNT {
            return (SC_INVALID_INTR_VECTOR | SC_DNR, 0);
        }
        if !self.queue_base_ok(prp1, qsize.into(), CQE_SIZE) {
            return (SC_INVALID_FIELD | SC_DNR, 0);
        }

        let instance = st.next_instance();
        st.io_cqs[idx] = Some(CompQueue {
            base: prp1,
            size: qsize,
            head: 0,
            tail: 0,
            phase: true,
            iv,
            ien,
            instance,
        });

        (SC_SUCCESS, 0)
    }

    /// Create I/O Submission Queue.
    pub(super) fn admin_create_io_sq(
        &self,
        st: &mut NvmeState,
        prp1: u64,
        cdw10: u32,
        cdw11: u32,
    ) -> (u16, u32) {
        let qid = (cdw10 & 0xFFFF) as u16;
        let qsize = ((cdw10 >> 16) & 0xFFFF) + 1;
        let cq_id = ((cdw11 >> 16) & 0xFFFF) as u16;

        let Some(idx) = io_queue_index(qid) else {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        };
        if qid > st.num_io_queues {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        }
        if qsize < 2 || qsize > u32::from(MAX_QUEUE_SIZE) {
            return (SC_INVALID_QUEUE_SIZE | SC_DNR, 0);
        }
        let qsize = qsize as u16;

        if st.io_sqs[idx].is_some() {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        }

        let Some(cq_idx) = io_queue_index(cq_id) else {
            return (SC_CQ_INVALID | SC_DNR, 0);
        };
        if st.io_cqs[cq_idx].is_none() {
            return (SC_CQ_INVALID | SC_DNR, 0);
        }

        if !self.queue_base_ok(prp1, qsize.into(), SQE_SIZE) {
            return (SC_INVALID_FIELD | SC_DNR, 0);
        }

        let instance = st.next_instance();
        st.io_sqs[idx] = Some(SubQueue {
            base: prp1,
            size: qsize,
            head: 0,
            tail: 0,
            cq_id,
            instance,
        });

        (SC_SUCCESS, 0)
    }

    /// Delete I/O Submission Queue.
    fn admin_delete_io_sq(&self, st: &mut NvmeState, cdw10: u32) -> (u16, u32) {
        let qid = (cdw10 & 0xFFFF) as u16;
        let Some(idx) = io_queue_index(qid) else {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        };
        if st.io_sqs[idx].is_none() {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        }
        st.io_sqs[idx] = None;
        (SC_SUCCESS, 0)
    }

    /// Delete I/O Completion Queue.
    fn admin_delete_io_cq(&self, st: &mut NvmeState, cdw10: u32) -> (u16, u32) {
        let qid = (cdw10 & 0xFFFF) as u16;
        let Some(idx) = io_queue_index(qid) else {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        };
        if st.io_cqs[idx].is_none() {
            return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
        }
        // A CQ that an SQ still names cannot be deleted.
        for sq in st.io_sqs.iter().flatten() {
            if sq.cq_id == qid {
                return (SC_INVALID_QUEUE_ID | SC_DNR, 0);
            }
        }
        st.io_cqs[idx] = None;
        (SC_SUCCESS, 0)
    }

    /// Get Features command.
    pub(super) fn admin_get_features(st: &NvmeState, cdw10: u32) -> (u16, u32) {
        let fid = (cdw10 & 0xFF) as u8;
        match fid {
            FEAT_NUM_QUEUES => {
                // CDW0 bits 31:16 = num CQs allocated (0-based)
                // CDW0 bits 15:0  = num SQs allocated (0-based)
                let nq = u32::from(st.num_io_queues.saturating_sub(1));
                (SC_SUCCESS, nq | (nq << 16))
            }
            FEAT_VOLATILE_WC => {
                // Report the volatile write cache as enabled.
                (SC_SUCCESS, 1)
            }
            FEAT_TEMP_THRESHOLD | FEAT_ERROR_RECOVERY | FEAT_POWER_MGMT
            | FEAT_ARBITRATION | FEAT_INTR_COALESCING
            | FEAT_INTR_VECTOR_CFG | FEAT_WRITE_ATOMICITY
            | FEAT_ASYNC_EVENT_CFG => (SC_SUCCESS, 0),
            _ => (SC_INVALID_FIELD | SC_DNR, 0),
        }
    }

    /// Set Features command.
    pub(super) fn admin_set_features(
        st: &mut NvmeState,
        cdw10: u32,
        cdw11: u32,
    ) -> (u16, u32) {
        let fid = (cdw10 & 0xFF) as u8;
        match fid {
            FEAT_NUM_QUEUES => {
                // The guest requests N SQs and N CQs, 0-based in CDW11.
                let requested_sq = (cdw11 & 0xFFFF) + 1;
                let requested_cq = ((cdw11 >> 16) & 0xFFFF) + 1;
                let max = MAX_IO_QUEUES as u32;
                let alloc_sq = requested_sq.min(max);
                let alloc_cq = requested_cq.min(max);
                st.num_io_queues = alloc_sq.min(alloc_cq) as u16;
                let result_sq = alloc_sq.saturating_sub(1);
                let result_cq = alloc_cq.saturating_sub(1);
                (SC_SUCCESS, result_sq | (result_cq << 16))
            }
            FEAT_VOLATILE_WC => (SC_SUCCESS, 0),
            FEAT_TEMP_THRESHOLD | FEAT_ERROR_RECOVERY | FEAT_POWER_MGMT
            | FEAT_ARBITRATION | FEAT_INTR_COALESCING
            | FEAT_INTR_VECTOR_CFG | FEAT_WRITE_ATOMICITY
            | FEAT_ASYNC_EVENT_CFG => (SC_SUCCESS, 0),
            _ => (SC_INVALID_FIELD | SC_DNR, 0),
        }
    }
}
