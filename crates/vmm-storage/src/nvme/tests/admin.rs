// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Registers, the admin queue and the admin command set.

use super::*;

#[test]
fn enable_sets_rdy_and_identify_lands_through_prp1_and_prp2() {
    let mut h = Harness::new();
    h.enable();
    assert_eq!(h.csts() & CSTS_RDY, CSTS_RDY);

    // PRP1 starts mid-page, so the 4 KiB structure straddles PRP2.
    let prp1 = 0x20800;
    let prp2 = 0x22000;
    h.fill(0x20000, 3 * PAGE_SIZE, 0xAA);
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_IDENTIFY,
        prp1,
        prp2,
        cdw10: u32::from(IDENTIFY_CNS_CONTROLLER),
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::SUCCESS);
    assert_eq!(cqe.sqid, ADMIN_QUEUE_ID);
    assert_eq!(cqe.sq_head, 1);
    assert_eq!(cqe.dw0, 0);
    assert_eq!(h.msi_count(), 1, "one MSI for the admin CQ");

    let first = h.read_guest(prp1, 0x800);
    assert_eq!(&first[4..24], b"TRITON-NVME-0001    ");
    assert_eq!(first[77], MDTS_LOG2_PAGES);
    assert_eq!(
        first[512],
        SQE_SIZE_LOG2 as u8 | ((SQE_SIZE_LOG2 as u8) << 4)
    );
    let second = h.read_guest(prp2, 0x800);
    assert_eq!(first[525], 1, "VWC");
    assert!(
        second.iter().all(|&b| b == 0),
        "upper half of Identify is zero"
    );
    // Nothing past the end of the PRP1 page and nothing past the data.
    assert!(h.read_guest(0x21000, PAGE_SIZE).iter().all(|&b| b == 0xAA));
    assert!(h.read_guest(prp2 + 0x800, 0x800).iter().all(|&b| b == 0xAA));
}

#[test]
fn enable_refuses_a_configuration_the_controller_cannot_honour() {
    for cc in [
        // An I/O SQ entry size the Identify data does not offer.
        CC_EN | (7 << CC_IOSQES_SHIFT) | (CQE_SIZE_LOG2 << CC_IOCQES_SHIFT),
        // A memory page size past CAP.MPSMAX.
        CC_EN
            | (SQE_SIZE_LOG2 << CC_IOSQES_SHIFT)
            | (CQE_SIZE_LOG2 << CC_IOCQES_SHIFT)
            | (1 << CC_MPS_SHIFT),
        // A command set this controller does not implement.
        CC_EN
            | (SQE_SIZE_LOG2 << CC_IOSQES_SHIFT)
            | (CQE_SIZE_LOG2 << CC_IOCQES_SHIFT)
            | (1 << CC_CSS_SHIFT),
    ] {
        let h = Harness::new();
        h.reg_write32(
            REG_AQA,
            u32::from(ADMIN_QDEPTH - 1) | (u32::from(ADMIN_QDEPTH - 1) << 16),
        );
        h.reg_write64(REG_ASQ, ASQ);
        h.reg_write64(REG_ACQ, ACQ);
        h.reg_write32(REG_CC, cc);

        assert_eq!(h.csts() & CSTS_RDY, 0, "CC {cc:#x} reported ready");
        assert_eq!(h.csts() & CSTS_CFS, CSTS_CFS, "CC {cc:#x}");
    }
}

#[test]
fn enable_refuses_an_admin_queue_that_is_not_mapped() {
    let h = Harness::new();
    h.reg_write32(
        REG_AQA,
        u32::from(ADMIN_QDEPTH - 1) | (u32::from(ADMIN_QDEPTH - 1) << 16),
    );
    // Page aligned but past the end of guest RAM.
    h.reg_write64(REG_ASQ, (GUEST_RAM as u64) + 0x1000);
    h.reg_write64(REG_ACQ, ACQ);
    h.reg_write32(
        REG_CC,
        CC_EN
            | (SQE_SIZE_LOG2 << CC_IOSQES_SHIFT)
            | (CQE_SIZE_LOG2 << CC_IOCQES_SHIFT),
    );

    assert_eq!(h.csts() & CSTS_RDY, 0);
    assert_eq!(h.csts() & CSTS_CFS, CSTS_CFS);
}

#[test]
fn admin_cq_wraps_and_flips_phase() {
    let mut h = Harness::new();
    h.enable();
    for _ in 0..ADMIN_QDEPTH + 2 {
        let cqe = h.admin(Sqe {
            opcode: ADMIN_OPC_GET_FEATURES,
            cdw10: u32::from(FEAT_NUM_QUEUES),
            ..Sqe::default()
        });
        assert_eq!(cqe.status(), Status::SUCCESS);
    }
    // The harness checks the phase on every entry. Past the wrap the
    // controller must write phase 0.
    assert!(!h.admin_phase);
    assert_eq!(h.admin_cq_head, 2);
}

#[test]
fn admin_doorbell_beyond_queue_size_is_ignored() {
    let mut h = Harness::new();
    h.enable();
    let entry = ASQ;
    let sqe = Sqe {
        opcode: ADMIN_OPC_GET_FEATURES,
        cdw10: u32::from(FEAT_NUM_QUEUES),
        ..Sqe::default()
    };
    h.mem().write(entry, &sqe.encode(7)).expect("write SQE");
    h.sq_doorbell(ADMIN_QUEUE_ID, ADMIN_QDEPTH);
    assert_eq!(h.read_guest(ACQ, CQE_SIZE), vec![0u8; CQE_SIZE]);
    assert_eq!(h.msi_count(), 0);
}

#[test]
fn get_log_page_stops_at_the_end_of_the_prp1_page() {
    let mut h = Harness::new();
    h.enable();

    // 256 bytes left in the PRP1 page, so half the log goes to PRP2.
    let prp1 = 0x20F00;
    let prp2 = 0x22000;
    h.fill(0x20000, 4 * PAGE_SIZE, 0xAA);
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_GET_LOG_PAGE,
        prp1,
        prp2,
        cdw10: u32::from(LID_SMART) | (((LOG_SMART_SIZE as u32 / 4) - 1) << 16),
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::SUCCESS);

    let expect = build_log_page(LID_SMART).expect("SMART log");
    assert_eq!(h.read_guest(prp1, 256), expect[..256]);
    assert_eq!(h.read_guest(prp2, 256), expect[256..]);
    // The page after PRP1's is untouched.
    assert!(h.read_guest(0x21000, PAGE_SIZE).iter().all(|&b| b == 0xAA));
    // The rest of the PRP2 page is also untouched.
    assert!(h.read_guest(prp2 + 256, 256).iter().all(|&b| b == 0xAA));
}

#[test]
fn get_log_page_refuses_a_log_the_controller_does_not_have() {
    let mut h = Harness::new();
    h.enable();
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_GET_LOG_PAGE,
        prp1: 0x20000,
        cdw10: 0x7F | (0x7F << 16),
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::of(SC_INVALID_LOG_PAGE | SC_DNR));
    assert!(h.read_guest(0x20000, 512).iter().all(|&b| b == 0));
}

#[test]
fn status_code_encoding() {
    assert_eq!(SC_DNR, 1 << 14);
    let status = SC_INVALID_OPCODE | SC_DNR;
    assert_ne!(status & SC_DNR, 0);
    assert_eq!(status & !SC_DNR, SC_INVALID_OPCODE);
}

#[test]
fn create_io_cq_with_an_unbacked_interrupt_vector_is_rejected() {
    let mut h = Harness::new();
    h.enable();
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_CREATE_IO_CQ,
        prp1: CQ1,
        cdw10: 1 | (u32::from(IO_QDEPTH - 1) << 16),
        cdw11: (u32::from(NVME_MSIX_COUNT) << 16) | CQ_IEN | CQ_PC,
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::of(SC_INVALID_INTR_VECTOR | SC_DNR));
}

#[test]
fn a_polled_completion_queue_raises_no_interrupt() {
    let mut h = Harness::new();
    h.enable();
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_CREATE_IO_CQ,
        prp1: CQ1,
        cdw10: 1 | (u32::from(IO_QDEPTH - 1) << 16),
        cdw11: (1 << 16) | CQ_PC,
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::SUCCESS);
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_CREATE_IO_SQ,
        prp1: SQ1,
        cdw10: 1 | (u32::from(IO_QDEPTH - 1) << 16),
        cdw11: (1 << 16) | CQ_PC,
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::SUCCESS);

    let admin_msi = h.msi_count();
    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::read(0, 1, 0x50000, 0), 0x41);
    assert_eq!(h.wait_cqe(CQ1, 0, true).cid, 0x41);
    assert_eq!(h.msi_count(), admin_msi, "IEN was clear");
}

#[test]
fn create_io_queue_past_the_allocation_is_rejected() {
    let mut h = Harness::new();
    h.enable();
    // Ask for four queue pairs, 0-based in CDW11.
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_SET_FEATURES,
        cdw10: u32::from(FEAT_NUM_QUEUES),
        cdw11: 3 | (3 << 16),
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::SUCCESS);

    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_CREATE_IO_CQ,
        prp1: CQ1,
        cdw10: 5 | (u32::from(IO_QDEPTH - 1) << 16),
        cdw11: (1 << 16) | CQ_IEN | CQ_PC,
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::of(SC_INVALID_QUEUE_ID | SC_DNR));
}

#[test]
fn create_io_sq_naming_the_admin_cq_is_rejected() {
    let mut h = Harness::new();
    h.enable();
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_CREATE_IO_SQ,
        prp1: SQ1,
        cdw10: 1 | (u32::from(IO_QDEPTH - 1) << 16),
        cdw11: CQ_PC,
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::of(SC_CQ_INVALID | SC_DNR));
}

#[test]
fn create_io_queue_larger_than_the_controller_allows_is_rejected() {
    let mut h = Harness::new();
    h.enable();
    // 0-based, so this asks for 65536 entries.
    let cdw10 = 1 | (u32::from(u16::MAX) << 16);
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_CREATE_IO_CQ,
        prp1: CQ1,
        cdw10,
        cdw11: CQ_IEN | CQ_PC,
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::of(SC_INVALID_QUEUE_SIZE | SC_DNR));
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_CREATE_IO_SQ,
        prp1: SQ1,
        cdw10,
        cdw11: 1 << 16 | CQ_PC,
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::of(SC_INVALID_QUEUE_SIZE | SC_DNR));
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
