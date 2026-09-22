// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The NVM command set and the PRP walk it drives.

use super::*;

#[test]
fn io_read_through_prp1_and_prp2_returns_the_backing_bytes() {
    let mut h = Harness::new();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    let prp1 = 0x50800;
    let prp2 = 0x52000;
    h.fill(0x50000, 3 * PAGE_SIZE, 0xAA);
    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::read(8, 8, prp1, prp2), 0x77);
    let cqe = h.wait_cqe(CQ1, 0, true);
    assert_eq!(cqe.status(), Status::SUCCESS);
    assert_eq!(cqe.cid, 0x77);
    assert_eq!(cqe.sqid, 1);
    assert_eq!(cqe.sq_head, 1);

    let expect: Vec<u8> = (8 * 512..16 * 512).map(backing_byte).collect();
    assert_eq!(h.read_guest(prp1, 0x800), expect[..0x800]);
    assert_eq!(h.read_guest(prp2, 0x800), expect[0x800..]);
    assert!(h.read_guest(0x51000, PAGE_SIZE).iter().all(|&b| b == 0xAA));
    assert_eq!(h.msi_count(), 3, "two admin completions and one I/O");
}

#[test]
fn io_write_reaches_the_backing_file() {
    let file = Arc::new(backing_file());
    let mut h = Harness::with_backend(
        Arc::clone(&file),
        Arc::clone(&file) as Arc<dyn crate::nvme::io::IoBackend>,
    );
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    h.fill(0x50000, 512, 0x5A);
    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::write(100, 1, 0x50000, 0), 1);
    assert_eq!(h.wait_cqe(CQ1, 0, true).status(), Status::SUCCESS);

    use std::os::unix::fs::FileExt;
    let mut buf = [0u8; 512];
    file.read_exact_at(&mut buf, 100 * 512).expect("read back");
    assert!(buf.iter().all(|&b| b == 0x5A));
}

#[test]
fn io_read_follows_a_prp_list_that_chains_at_the_page_end() {
    let mut h = Harness::new();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    // The list starts one entry short of its page end, so the walker
    // must treat that last entry as a chain to the second list.
    let first_page = 0x60000;
    let list_a = 0x61FF8;
    let list_b = 0x62000;
    let (page_b, page_c) = (0x63000, 0x64000);
    h.fill(0x60000, 5 * PAGE_SIZE, 0xAA);
    put_prp(&h, list_a, list_b);
    put_prp(&h, list_b, page_b);
    put_prp(&h, list_b + 8, page_c);

    let mut tail = 0;
    io_submit(
        &mut h,
        &mut tail,
        Sqe::read(0, 24, first_page, list_a),
        0x33,
    );
    assert_eq!(h.wait_cqe(CQ1, 0, true).status(), Status::SUCCESS);

    let expect: Vec<u8> = (0..24 * 512).map(backing_byte).collect();
    assert_eq!(h.read_guest(first_page, PAGE_SIZE), expect[..PAGE_SIZE]);
    assert_eq!(
        h.read_guest(page_b, PAGE_SIZE),
        expect[PAGE_SIZE..2 * PAGE_SIZE]
    );
    assert_eq!(h.read_guest(page_c, PAGE_SIZE), expect[2 * PAGE_SIZE..]);
    // The page holding the first list is untouched past its entry.
    assert!(h.read_guest(0x61000, 0xFF8).iter().all(|&b| b == 0xAA));
}

#[test]
fn prp2_with_a_page_offset_is_refused() {
    let mut h = Harness::new();
    h.enable();
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_IDENTIFY,
        prp1: 0x20800,
        prp2: 0x22008,
        cdw10: u32::from(IDENTIFY_CNS_CONTROLLER),
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::of(SC_DATA_XFER_ERROR));
    assert!(h.read_guest(0x22000, PAGE_SIZE).iter().all(|&b| b == 0));
}

#[test]
fn prp_list_entry_with_a_page_offset_is_refused() {
    let mut h = Harness::new();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    let list = 0x61000;
    h.fill(0x60000, 5 * PAGE_SIZE, 0xAA);
    put_prp(&h, list, 0x63040);
    put_prp(&h, list + 8, 0x64000);

    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::read(0, 24, 0x60000, list), 0x34);
    let cqe = h.wait_cqe(CQ1, 0, true);
    assert_eq!(cqe.status(), Status::of(SC_DATA_XFER_ERROR));
    // Nothing was transferred: the walk fails before any lookup.
    assert!(h.read_guest(0x60000, PAGE_SIZE).iter().all(|&b| b == 0xAA));
}

#[test]
fn a_read_only_namespace_reports_write_protection() {
    let mut h = Harness::read_only();
    h.enable();
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_IDENTIFY,
        nsid: 1,
        prp1: 0x20000,
        prp2: 0x21000,
        cdw10: u32::from(IDENTIFY_CNS_NAMESPACE),
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::SUCCESS);
    assert_eq!(h.read_guest(0x20000 + 99, 1), vec![1], "NSATTR");

    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::write(0, 1, 0x50000, 0), 0x51);
    let cqe = h.wait_cqe(CQ1, 0, true);
    assert_eq!(cqe.status(), Status::of(SC_WRITE_TO_RO_RANGE | SC_DNR));
}

#[test]
fn io_read_past_the_namespace_is_refused() {
    let mut h = Harness::new();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    let mut tail = 0;
    io_submit(
        &mut h,
        &mut tail,
        Sqe::read(BACKING_BLOCKS - 1, 2, 0x50000, 0),
        1,
    );
    let cqe = h.wait_cqe(CQ1, 0, true);
    assert_eq!(cqe.status(), Status::of(SC_INVALID_FIELD | SC_DNR));
}

#[test]
fn command_specific_status_reaches_the_guest_with_sct_one() {
    let mut h = Harness::new();
    h.enable();
    // An SQ whose CQ does not exist.
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_CREATE_IO_SQ,
        prp1: SQ1,
        cdw10: 1 | (u32::from(IO_QDEPTH - 1) << 16),
        cdw11: 3 << 16,
        ..Sqe::default()
    });
    assert_eq!(
        cqe.status(),
        Status {
            sct: 1,
            sc: 0x00,
            dnr: true
        }
    );
    // A queue id past the allocation.
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_CREATE_IO_CQ,
        prp1: CQ1,
        cdw10: 16 | (u32::from(IO_QDEPTH - 1) << 16),
        cdw11: 1 << 16,
        ..Sqe::default()
    });
    assert_eq!(
        cqe.status(),
        Status {
            sct: 1,
            sc: 0x01,
            dnr: true
        }
    );
    // A queue of one entry.
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_CREATE_IO_CQ,
        prp1: CQ1,
        cdw10: 1,
        cdw11: 1 << 16,
        ..Sqe::default()
    });
    assert_eq!(
        cqe.status(),
        Status {
            sct: 1,
            sc: 0x02,
            dnr: true
        }
    );
}
