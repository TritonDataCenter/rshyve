// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Migration payload validation: what a peer-supplied controller state
//! must satisfy before any of it reaches the live queues.

use super::*;
use vmm_devices::lifecycle::{NvmeMigrateCq, NvmeMigrateSq};

use crate::nvme::check_nvme_state;

struct NvmeQuiesceTestDevice {
    gate: Arc<QuiesceGate>,
}

impl Lifecycle for NvmeQuiesceTestDevice {
    fn type_name(&self) -> &'static str {
        "nvme"
    }

    fn is_quiesced(&self) -> bool {
        self.gate.is_quiesced()
    }
}

/// A controller payload with one admin queue pair and one I/O
/// queue pair, all of it valid.
fn good_payload() -> vmm_devices::lifecycle::NvmeMigrateState {
    vmm_devices::lifecycle::NvmeMigrateState {
        cc: CC_EN,
        csts: 1,
        aqa: 0x003F_003F,
        asq_base: 0x1000,
        acq_base: 0x2000,
        admin_sq: Some(NvmeMigrateSq {
            base: 0x1000,
            size: 64,
            head: 3,
            tail: 5,
            cq_id: 0,
        }),
        admin_cq: Some(NvmeMigrateCq {
            base: 0x2000,
            size: 64,
            head: 3,
            tail: 5,
            phase: true,
            iv: 0,
            ien: true,
        }),
        io_sqs: vec![Some(NvmeMigrateSq {
            base: 0x3000,
            size: 256,
            head: 0,
            tail: 0,
            cq_id: 1,
        })],
        io_cqs: vec![Some(NvmeMigrateCq {
            base: 0x4000,
            size: 256,
            head: 0,
            tail: 0,
            phase: true,
            iv: 1,
            ien: true,
        })],
        num_io_queues: 1,
        pci: Default::default(),
        msix: Default::default(),
    }
}

#[test]
fn a_sane_controller_payload_is_accepted() {
    check_nvme_state(&good_payload()).expect("a sane payload");
}

#[test]
fn a_queue_size_of_zero_is_refused() {
    // The doorbell handlers take `% size`. Zero divides by zero, and a
    // release build aborts on it.
    let mut payload = good_payload();
    payload.admin_sq.as_mut().expect("admin sq").size = 0;
    let error = check_nvme_state(&payload).expect_err("size 0");
    assert!(error.to_string().contains("outside 2..="), "{error}");

    let mut payload = good_payload();
    payload.admin_cq.as_mut().expect("admin cq").size = 1;
    check_nvme_state(&payload).expect_err("size 1 is below the minimum");
}

#[test]
fn a_queue_size_past_the_maximum_is_refused() {
    let mut payload = good_payload();
    payload.io_sqs[0].as_mut().expect("io sq").size = MAX_QUEUE_SIZE;
    check_nvme_state(&payload).expect("the maximum itself is fine");
    payload.io_sqs[0].as_mut().expect("io sq").size = u16::MAX;
    check_nvme_state(&payload).expect_err("past the maximum");
}

#[test]
fn a_cursor_outside_the_ring_is_refused() {
    // `while sq.head != sq.tail` walks guest memory until the cursor
    // wraps.
    let mut payload = good_payload();
    payload.admin_sq.as_mut().expect("admin sq").head = 64;
    let error = check_nvme_state(&payload).expect_err("head == size");
    assert!(error.to_string().contains("outside a 64-entry"), "{error}");

    let mut payload = good_payload();
    payload.io_cqs[0].as_mut().expect("io cq").tail = 999;
    check_nvme_state(&payload).expect_err("tail past the ring");
}

#[test]
fn an_unaligned_or_zero_queue_base_is_refused() {
    let mut payload = good_payload();
    payload.io_sqs[0].as_mut().expect("io sq").base = 0x3001;
    let error = check_nvme_state(&payload).expect_err("unaligned base");
    assert!(error.to_string().contains("page-aligned"), "{error}");

    let mut payload = good_payload();
    payload.admin_cq.as_mut().expect("admin cq").base = 0;
    check_nvme_state(&payload).expect_err("a zero base");
}

#[test]
fn a_submission_queue_whose_completion_queue_is_absent_is_refused() {
    // Completions go through a `None` slot, or into another queue's
    // ring.
    let mut payload = good_payload();
    payload.io_cqs[0] = None;
    let error = check_nvme_state(&payload).expect_err("missing CQ 1");
    assert!(error.to_string().contains("does not create"), "{error}",);

    let mut payload = good_payload();
    payload.io_sqs[0].as_mut().expect("io sq").cq_id = 9;
    check_nvme_state(&payload).expect_err("a CQ id past the table");
}

#[test]
fn an_interrupt_vector_the_msix_table_cannot_hold_is_refused() {
    // Create I/O Completion Queue refuses these, and `MsixTable::fire`
    // drops them silently, so the imported queue never raises.
    let mut payload = good_payload();
    payload.io_cqs[0].as_mut().expect("io cq").iv = NVME_MSIX_COUNT - 1;
    check_nvme_state(&payload).expect("the last vector is fine");

    payload.io_cqs[0].as_mut().expect("io cq").iv = NVME_MSIX_COUNT;
    let error = check_nvme_state(&payload).expect_err("iv past the table");
    assert!(error.to_string().contains("interrupt vector"), "{error}");

    let mut payload = good_payload();
    payload.admin_cq.as_mut().expect("admin cq").iv = u16::MAX;
    check_nvme_state(&payload).expect_err("an admin vector past the table");
}

/// Linux gives its poll queues no interrupt, so a migration that
/// restores one as interrupting sends the destination guest a message
/// it never armed a handler for.
#[test]
fn a_polled_completion_queue_stays_polled_across_a_migration() {
    let mut src = Harness::new();
    src.enable();
    src.create_io_queues_with_irq(1, IO_QDEPTH, SQ1, CQ1, false);
    let state = src
        .ctrl
        .export_migrate_state()
        .expect("export")
        .expect("an enabled controller has state");

    let mut dst = Harness::new();
    dst.ctrl.restore_migrate_state(&state).expect("restore");

    let before = dst.msi_count();
    let mut tail = 0;
    io_submit(&mut dst, &mut tail, Sqe::read(0, 1, 0x50000, 0), 0x31);
    let cqe = dst.wait_cqe(CQ1, 0, true);

    assert_eq!(cqe.cid, 0x31);
    assert_eq!(cqe.status(), Status::SUCCESS);
    assert_eq!(dst.msi_count(), before, "a polled CQ raised an interrupt");
}

#[test]
fn more_queues_than_the_controller_has_is_refused() {
    let mut payload = good_payload();
    payload.io_sqs = vec![None; MAX_IO_QUEUES + 1];
    let error = check_nvme_state(&payload).expect_err("too many SQs");
    assert!(error.to_string().contains("of each"), "{error}");
}

#[test]
fn an_out_of_range_io_queue_count_is_refused() {
    let mut payload = good_payload();
    payload.num_io_queues = 0;
    check_nvme_state(&payload).expect_err("zero queues");
    payload.num_io_queues = MAX_IO_QUEUES as u16 + 1;
    check_nvme_state(&payload).expect_err("past the maximum");
}

#[test]
fn quiesce_query_is_non_blocking_with_active_worker() {
    let device: Arc<dyn Lifecycle> = Arc::new(NvmeQuiesceTestDevice {
        gate: Arc::new(QuiesceGate::new(1)),
    });

    let probe = Arc::clone(&device);
    assert!(!vmm_devices_testsupport::assert_query_non_blocking(
        move || probe.is_quiesced()
    ));
}

#[test]
fn cap_register_value() {
    let mqes = MAX_QUEUE_SIZE - 1;
    let cap: u64 = (u64::from(mqes) << CAP_MQES_SHIFT)
        | CAP_CQR
        | (1u64 << CAP_TO_SHIFT)
        | CAP_CSS_NVM;
    // MQES is bits 15:0.
    assert_eq!((cap & CAP_MQES_MASK) as u16, mqes);
    // CQR is bit 16.
    assert_ne!(cap & CAP_CQR, 0);
    // CSS NVM is bit 37.
    assert_ne!(cap & CAP_CSS_NVM, 0);
}

#[test]
fn cqe_phase_bit_encoding() {
    // The phase tag is bit 0 of the CQE status field.
    let status: u16 = SC_SUCCESS;
    let phase = true;
    let encoded = (status << 1) | if phase { 1 } else { 0 };
    assert_eq!(encoded & 1, 1); // phase set
    assert_eq!(encoded >> 1, SC_SUCCESS);
}

#[test]
fn doorbell_index_calculation() {
    // SQ tail doorbell for queue 0 is at offset 0x1000
    let offset = REG_DOORBELL_BASE;
    let db_offset = offset - REG_DOORBELL_BASE;
    let db_index = db_offset / 4;
    let qid = db_index / 2;
    let is_sq = db_index.is_multiple_of(2);
    assert_eq!(qid, 0);
    assert!(is_sq);

    // CQ head doorbell for queue 0 is at offset 0x1004
    let offset = REG_DOORBELL_BASE + 4;
    let db_offset = offset - REG_DOORBELL_BASE;
    let db_index = db_offset / 4;
    let qid = db_index / 2;
    let is_sq = db_index.is_multiple_of(2);
    assert_eq!(qid, 0);
    assert!(!is_sq);

    // SQ tail doorbell for queue 1 is at offset 0x1008
    let offset = REG_DOORBELL_BASE + 8;
    let db_offset = offset - REG_DOORBELL_BASE;
    let db_index = db_offset / 4;
    let qid = db_index / 2;
    let is_sq = db_index.is_multiple_of(2);
    assert_eq!(qid, 1);
    assert!(is_sq);
}

#[test]
fn prp_single_page() {
    // A transfer that fits in the rest of the PRP1 page uses only PRP1.
    let offset_in_page = 0x100usize;
    let _prp1_addr = 0x1000u64 + offset_in_page as u64;
    let first_len = (PAGE_SIZE - offset_in_page).min(512);
    assert_eq!(first_len, 512); // 512 < 3840
}

#[test]
fn status_code_encoding() {
    assert_eq!(SC_DNR, 1 << 14);
    let status = SC_INVALID_OPCODE | SC_DNR;
    assert_ne!(status & SC_DNR, 0);
    assert_eq!(status & !SC_DNR, SC_INVALID_OPCODE);
}

#[test]
fn create_io_sq_naming_cq_zero_is_rejected() {
    let h = Harness::new();
    let mut state = NvmeState::default();
    let qid = 1u32;
    let qsize = 1u32 << 16;
    let cq_id = 0u32 << 16;

    let status =
        h.ctrl
            .admin_create_io_sq(&mut state, 0x1000, qid | qsize, cq_id);

    // CQ 0 is the admin queue, so no I/O CQ answers to it. NVMe 1.4
    // figure 128 calls that Completion Queue Invalid, not Invalid Field.
    assert_eq!(status, (SC_CQ_INVALID | SC_DNR, 0));
}

#[test]
fn num_queues_max_request_stays_nonzero() {
    let mut state = NvmeState::default();

    let set_result = NvmeController::admin_set_features(
        &mut state,
        u32::from(FEAT_NUM_QUEUES),
        u32::MAX,
    );

    let allocated = (MAX_IO_QUEUES as u32) - 1;
    assert_eq!(set_result, (SC_SUCCESS, allocated | (allocated << 16)));
    assert_eq!(state.num_io_queues, MAX_IO_QUEUES as u16);
    assert_ne!(state.num_io_queues, 0);
    assert_eq!(
        NvmeController::admin_get_features(&state, u32::from(FEAT_NUM_QUEUES),),
        set_result
    );
}

#[test]
fn create_io_queues_reject_max_qsize_without_overflow() {
    let cdw10 = 1 | (u32::from(u16::MAX) << 16);

    let h = Harness::new();

    let mut cq_state = NvmeState::default();
    assert_eq!(
        h.ctrl.admin_create_io_cq(&mut cq_state, 0x1000, cdw10, 0),
        (SC_INVALID_QUEUE_SIZE | SC_DNR, 0)
    );

    let mut sq_state = NvmeState::default();
    assert_eq!(
        h.ctrl
            .admin_create_io_sq(&mut sq_state, 0x1000, cdw10, 1 << 16),
        (SC_INVALID_QUEUE_SIZE | SC_DNR, 0)
    );
}

#[test]
fn sector_overflow_checked_mul() {
    // An SLBA near u64::MAX times a 512-byte block overflows.
    let slba: u64 = u64::MAX / 512 + 1;
    let block_size: u32 = 512;
    assert!(slba.checked_mul(u64::from(block_size)).is_none());
}

#[test]
fn sector_bounds_checked_add() {
    let slba: u64 = u64::MAX;
    let nlb: u16 = 1;
    assert!(slba.checked_add(u64::from(nlb)).is_none());
}

#[test]
fn nlb_overflow_checked_mul() {
    // A 16-bit NLB times a 32-bit block size always fits in u64, so only
    // byte_offset can overflow.
    let nlb: u16 = u16::MAX;
    let block_size: u32 = u32::MAX;
    assert!((nlb as u64).checked_mul(u64::from(block_size)).is_some());
    let nlb: u16 = 128;
    let block_size: u32 = 512;
    assert_eq!((nlb as u64).checked_mul(u64::from(block_size)), Some(65536));
}
