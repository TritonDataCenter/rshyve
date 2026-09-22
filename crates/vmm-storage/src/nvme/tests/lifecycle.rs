// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pause, reset and teardown.

use super::*;

/// Keep ringing the I/O SQ 1 doorbell until `stop` is set.
///
/// The channel never empties. An operator pause must survive this load.
fn steady_submitter(
    ctrl: Arc<NvmeController>,
    physmap: Arc<PhysMap>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mem = MemCtx::new(physmap);
        let mut tail = 0u16;
        let mut cid = 1u16;
        while !stop.load(Ordering::Relaxed) {
            let sqe = Sqe::read(0, 1, 0x70000, 0);
            mem.write(
                SQ1 + u64::from(tail) * SQE_SIZE as u64,
                &sqe.encode(cid),
            )
            .expect("write SQE");
            tail = (tail + 1) % IO_QDEPTH;
            cid = cid.wrapping_add(1);
            let wo = WriteOp::from_buf(&u32::from(tail).to_le_bytes());
            ctrl.bar_rw(BarN::BAR0, REG_DOORBELL_BASE + 8, RWOp::Write(&wo));
            std::thread::sleep(Duration::from_micros(200));
        }
    })
}

#[test]
fn pause_quiesces_the_workers_under_steady_guest_io() {
    let (mut h, backend) = Harness::held();
    // Slower than the submitter, so the channel keeps growing.
    backend.set_delay(Duration::from_millis(5));
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    let stop = Arc::new(AtomicBool::new(false));
    let submitter = steady_submitter(
        Arc::clone(&h.ctrl),
        Arc::clone(&h.physmap),
        Arc::clone(&stop),
    );
    backend.wait_in_flight(1);

    h.ctrl.pause();
    let quiesced = h.ctrl.gate.wait_quiesced(Duration::from_secs(2));

    stop.store(true, Ordering::Relaxed);
    submitter.join().expect("submitter");
    backend.set_delay(Duration::ZERO);
    h.ctrl.resume();
    assert!(quiesced, "pause did not quiesce the workers under load");
}

/// Release the backend once a reset reaches its drain.
///
/// The reset blocks the vCPU thread that wrote the register, so another
/// thread must release the transfer it waits for. Returns whether a
/// drain started.
fn release_on_drain(
    gate: Arc<QuiesceGate>,
    backend: Arc<TestBackend>,
) -> std::thread::JoinHandle<bool> {
    std::thread::spawn(move || {
        let start = Instant::now();
        let mut drained = false;
        while start.elapsed() < WAIT {
            if gate.drain_count() > 0 {
                drained = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        backend.release();
        drained
    })
}

#[test]
fn a_reset_waits_for_the_transfer_it_interrupts() {
    let (mut h, backend) = Harness::held();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    backend.hold();
    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::read(8, 8, 0x50000, 0x52000), 0x77);
    backend.wait_in_flight(1);

    let releaser =
        release_on_drain(Arc::clone(&h.ctrl.gate), Arc::clone(&backend));
    h.reg_write32(REG_CC, 0);
    let quiet = backend.in_flight();
    let drained = releaser.join().expect("releaser");

    assert!(drained, "the reset did not drain the workers");
    assert_eq!(quiet, 0, "CC.EN cleared with a transfer still in flight");
    assert_eq!(h.csts() & CSTS_RDY, 0);
    assert_eq!(backend.started(), 1);
    // The interrupted command never completes: its queues are gone.
    assert_eq!(h.read_guest(CQ1, CQE_SIZE), vec![0u8; CQE_SIZE]);
}

#[test]
fn shutdown_drains_before_it_reports_complete() {
    let (mut h, backend) = Harness::held();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    backend.hold();
    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::write(0, 1, 0x50000, 0), 0x61);
    backend.wait_in_flight(1);

    let releaser =
        release_on_drain(Arc::clone(&h.ctrl.gate), Arc::clone(&backend));
    h.reg_write32(
        REG_CC,
        CC_EN
            | (SQE_SIZE_LOG2 << CC_IOSQES_SHIFT)
            | (CQE_SIZE_LOG2 << CC_IOCQES_SHIFT)
            | (1 << CC_SHN_SHIFT),
    );
    let quiet = backend.in_flight();
    let drained = releaser.join().expect("releaser");

    assert!(drained, "the shutdown did not drain the workers");
    assert_eq!(quiet, 0, "SHST completed with a transfer still in flight");
    assert_eq!(
        (h.csts() >> CSTS_SHST_SHIFT) & CSTS_SHST_MASK,
        SHST_COMPLETE
    );
    assert_eq!(h.csts() & CSTS_CFS, 0);
    // The queues are gone, so a further doorbell does nothing.
    assert_eq!(backend.started(), 1);
    h.sq_doorbell(1, 1);
    assert_eq!(backend.started(), 1);
}

/// A reset whose fence runs out must not report the controller ready
/// for reuse. The worker still inside the backend holds guest pages
/// that the driver takes back when RDY clears.
#[test]
fn a_reset_that_times_out_leaves_the_controller_fatal() {
    let (mut h, backend) = Harness::impatient();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    backend.hold();
    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::read(8, 8, 0x50000, 0x52000), 0x77);
    backend.wait_in_flight(1);

    h.reg_write32(REG_CC, 0);

    assert_eq!(backend.in_flight(), 1, "the transfer ended on its own");
    assert_ne!(h.csts() & CSTS_RDY, 0, "RDY cleared over a live transfer");
    assert_ne!(h.csts() & CSTS_CFS, 0, "a failed reset reported no fault");

    // Admission stays closed: the controller does not come back up.
    h.enable();
    assert_ne!(h.csts() & CSTS_CFS, 0, "the fatal status was cleared");
    h.sq_doorbell(1, 1);
    assert_eq!(
        backend.started(),
        1,
        "I/O was admitted after a failed reset"
    );

    backend.release();
}

/// SHST complete is the driver's licence to cut power and hand the
/// pages a transfer names to something else, so a fence that runs out
/// must not report it.
#[test]
fn a_shutdown_that_times_out_does_not_report_complete() {
    let (mut h, backend) = Harness::impatient();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    backend.hold();
    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::write(0, 1, 0x50000, 0), 0x61);
    backend.wait_in_flight(1);

    h.reg_write32(
        REG_CC,
        CC_EN
            | (SQE_SIZE_LOG2 << CC_IOSQES_SHIFT)
            | (CQE_SIZE_LOG2 << CC_IOCQES_SHIFT)
            | (1 << CC_SHN_SHIFT),
    );

    assert_eq!(backend.in_flight(), 1, "the transfer ended on its own");
    assert_ne!(
        (h.csts() >> CSTS_SHST_SHIFT) & CSTS_SHST_MASK,
        SHST_COMPLETE,
        "shutdown reported complete over a live transfer"
    );
    assert_ne!(
        h.csts() & CSTS_CFS,
        0,
        "a failed shutdown reported no fault"
    );

    backend.release();
}

#[test]
fn a_reset_drops_queued_io_instead_of_completing_it_into_new_queues() {
    let (mut h, backend) = Harness::held();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    // Park the workers so the command stays in the channel across the
    // reset, the way a busy controller leaves work behind.
    h.ctrl.pause();
    assert!(h.ctrl.gate.wait_quiesced(WAIT), "workers did not park");
    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::read(8, 8, 0x50000, 0x52000), 0x99);

    h.reg_write32(REG_CC, 0);
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    h.ctrl.resume();

    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::read(0, 1, 0x50000, 0), 0x21);
    let cqe = h.wait_cqe(CQ1, 0, true);
    assert_eq!(cqe.cid, 0x21, "a command from before the reset completed");
    assert_eq!(cqe.status(), Status::SUCCESS);
    assert_eq!(backend.started(), 1, "the dropped command still ran");
}

#[test]
fn deleting_and_recreating_an_io_queue_drops_the_old_commands() {
    let (mut h, backend) = Harness::held();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    h.ctrl.pause();
    assert!(h.ctrl.gate.wait_quiesced(WAIT), "workers did not park");
    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::read(8, 8, 0x50000, 0x52000), 0x99);

    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_DELETE_IO_SQ,
        cdw10: 1,
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::SUCCESS, "delete SQ");
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_CREATE_IO_SQ,
        prp1: SQ1,
        cdw10: 1 | (u32::from(IO_QDEPTH - 1) << 16),
        cdw11: (1 << 16) | CQ_PC,
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::SUCCESS, "recreate SQ");
    h.ctrl.resume();

    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::read(0, 1, 0x50000, 0), 0x21);
    let cqe = h.wait_cqe(CQ1, 0, true);
    assert_eq!(cqe.cid, 0x21, "a command from the deleted queue completed");
    assert_eq!(backend.started(), 1, "the dropped command still ran");
}

#[test]
fn a_reset_drops_the_pending_msix_messages() {
    let mut h = Harness::new();
    h.mask_vector(0);
    h.enable();
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_GET_FEATURES,
        cdw10: u32::from(FEAT_NUM_QUEUES),
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::SUCCESS);
    assert_eq!(h.pba() & 1, 1, "the masked admin vector should be pending");
    assert_eq!(h.msi_count(), 0);

    h.reg_write32(REG_CC, 0);

    assert_eq!(h.pba() & 1, 0, "a reset left a message pending");
}

#[test]
fn halt_joins_the_io_workers() {
    let (h, backend) = Harness::held();
    let with_workers = Arc::strong_count(&backend);
    assert!(
        with_workers > crate::nvme::NUM_WORKERS,
        "the workers should hold the backend"
    );

    h.ctrl.halt();

    assert_eq!(
        Arc::strong_count(&backend),
        with_workers - crate::nvme::NUM_WORKERS,
        "halt left workers running"
    );
}

#[test]
fn dropping_the_controller_exits_the_io_workers() {
    let (h, backend) = Harness::held();
    let with_workers = Arc::strong_count(&backend);

    drop(h);

    assert_eq!(
        Arc::strong_count(&backend),
        with_workers - crate::nvme::NUM_WORKERS,
        "the worker pool kept the controller alive"
    );
}

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
