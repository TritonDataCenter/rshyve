// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use vmm_core::common::{RWOp, ReadOp, WriteOp, PAGE_SIZE};
use vmm_core::mem::{MemCtx, PhysMap};
use vmm_core::mmio::MmioBus;
use vmm_devices::pci::device::PciDevice;
use vmm_devices::pci::msix::MsiSink;
use vmm_devices::pci::BarN;
use vmm_devices::{Lifecycle, QuiesceGate};

use super::bits::*;
use super::queues::NvmeState;
use super::{FenceBudgets, NvmeController, NvmeParts, FENCE_BUDGETS};

mod admin;
mod io;
mod lifecycle;
mod migrate;

/// Guest RAM the harness maps: enough for queues and a 2 MiB transfer.
const GUEST_RAM: usize = 8 << 20;
const BACKING_BLOCKS: u64 = 4096;
const ASQ: u64 = 0x10000;
const ACQ: u64 = 0x11000;
const ADMIN_QDEPTH: u16 = 16;
const WAIT: Duration = Duration::from_secs(5);

/// Fence budgets short enough for a test to outlast. The production
/// budgets are seconds long, and a test of a missed deadline must wait
/// one out.
const SHORT_FENCES: FenceBudgets = FenceBudgets {
    reset: Duration::from_millis(100),
    shutdown: Duration::from_millis(100),
};

struct RecordingSink(Mutex<Vec<(u64, u64)>>);

impl MsiSink for RecordingSink {
    fn send(&self, addr: u64, data: u64) {
        self.0.lock().expect("sink lock").push((addr, data));
    }
}

/// One controller over anonymous guest RAM and a patterned temp file.
///
/// Byte `i` of the backing file is `(i / 512) as u8 ^ (i as u8)`, so a
/// read that lands in the wrong page is visible.
struct Harness {
    ctrl: Arc<NvmeController>,
    physmap: Arc<PhysMap>,
    sink: Arc<RecordingSink>,
    admin_tail: u16,
    admin_cq_head: usize,
    admin_phase: bool,
    next_cid: u16,
}

fn backing_byte(i: u64) -> u8 {
    ((i / 512) as u8) ^ (i as u8)
}

fn backing_file() -> std::fs::File {
    let mut file = tempfile::tempfile().expect("temp backing file");
    let bytes: Vec<u8> = (0..BACKING_BLOCKS * 512).map(backing_byte).collect();
    file.write_all(&bytes).expect("fill backing file");
    file
}

/// A backend whose transfers the test controls.
///
/// A held transfer stands in for a slow disk: the test can stop every
/// worker inside the backend and then do to the controller whatever a
/// guest could do while its I/O is outstanding.
struct TestBackend {
    file: Arc<std::fs::File>,
    delay_us: AtomicU64,
    state: Mutex<BackendState>,
    cv: Condvar,
}

#[derive(Default)]
struct BackendState {
    held: bool,
    /// Transfers that have entered the backend, ever.
    started: usize,
    /// Transfers inside the backend now, held ones included.
    in_flight: usize,
}

impl TestBackend {
    fn new(file: Arc<std::fs::File>) -> Arc<Self> {
        Arc::new(Self {
            file,
            delay_us: AtomicU64::new(0),
            state: Mutex::new(BackendState::default()),
            cv: Condvar::new(),
        })
    }

    fn set_delay(&self, delay: Duration) {
        self.delay_us
            .store(delay.as_micros() as u64, Ordering::Relaxed);
    }

    /// Stop every transfer inside the backend until [`Self::release`].
    fn hold(&self) {
        self.state.lock().expect("backend lock").held = true;
    }

    fn release(&self) {
        let mut st = self.state.lock().expect("backend lock");
        st.held = false;
        self.cv.notify_all();
    }

    /// Transfers that have entered the backend, ever.
    fn started(&self) -> usize {
        self.state.lock().expect("backend lock").started
    }

    fn in_flight(&self) -> usize {
        self.state.lock().expect("backend lock").in_flight
    }

    /// Block until `n` transfers are inside the backend.
    fn wait_in_flight(&self, n: usize) {
        let mut st = self.state.lock().expect("backend lock");
        let start = Instant::now();
        while st.in_flight < n {
            let (next, timeout) = self
                .cv
                .wait_timeout(st, Duration::from_millis(50))
                .expect("backend lock");
            st = next;
            assert!(
                !timeout.timed_out() || start.elapsed() < WAIT,
                "only {} of {n} transfers started",
                st.in_flight
            );
        }
    }

    fn enter(&self) {
        let mut st = self.state.lock().expect("backend lock");
        st.started += 1;
        st.in_flight += 1;
        self.cv.notify_all();
        while st.held {
            st = self.cv.wait(st).expect("backend lock");
        }
        drop(st);
        let delay = self.delay_us.load(Ordering::Relaxed);
        if delay != 0 {
            std::thread::sleep(Duration::from_micros(delay));
        }
    }

    fn leave(&self) {
        let mut st = self.state.lock().expect("backend lock");
        st.in_flight -= 1;
        self.cv.notify_all();
    }
}

impl super::io::IoBackend for TestBackend {
    fn read_at(
        &self,
        iov: &vmm_core::mem::GuestIoVec,
        offset: u64,
    ) -> std::io::Result<usize> {
        self.enter();
        let done = iov.read_from(&self.file, offset);
        self.leave();
        done
    }

    fn write_at(
        &self,
        iov: &vmm_core::mem::GuestIoVec,
        offset: u64,
    ) -> std::io::Result<usize> {
        self.enter();
        let done = iov.write_to(&self.file, offset);
        self.leave();
        done
    }

    fn sync(&self) -> std::io::Result<()> {
        self.enter();
        let done = self.file.sync_data();
        self.leave();
        done
    }
}

impl Harness {
    /// A harness whose transfers the returned backend controls.
    fn held() -> (Self, Arc<TestBackend>) {
        Self::held_with(FENCE_BUDGETS)
    }

    /// The same harness, with fence budgets a held transfer outlasts.
    fn impatient() -> (Self, Arc<TestBackend>) {
        Self::held_with(SHORT_FENCES)
    }

    fn held_with(fences: FenceBudgets) -> (Self, Arc<TestBackend>) {
        let file = Arc::new(backing_file());
        let backend = TestBackend::new(Arc::clone(&file));
        let h = Self::build(
            file,
            Arc::clone(&backend) as Arc<dyn super::io::IoBackend>,
            false,
            fences,
        );
        (h, backend)
    }

    fn new() -> Self {
        let file = Arc::new(backing_file());
        Self::with_backend(Arc::clone(&file), file)
    }

    fn read_only() -> Self {
        let file = Arc::new(backing_file());
        Self::build(Arc::clone(&file), file, true, FENCE_BUDGETS)
    }

    fn with_backend(
        file: Arc<std::fs::File>,
        backend: Arc<dyn super::io::IoBackend>,
    ) -> Self {
        Self::build(file, backend, false, FENCE_BUDGETS)
    }

    fn build(
        file: Arc<std::fs::File>,
        backend: Arc<dyn super::io::IoBackend>,
        read_only: bool,
        fences: FenceBudgets,
    ) -> Self {
        let physmap =
            Arc::new(PhysMap::new_anon(0, GUEST_RAM).expect("anon guest RAM"));
        let sink = Arc::new(RecordingSink(Mutex::new(Vec::new())));
        let ctrl = NvmeController::build(NvmeParts {
            file,
            backend,
            read_only,
            instance_id: 1,
            physmap: Arc::clone(&physmap),
            msi: Arc::clone(&sink) as Arc<dyn MsiSink>,
            bus_mmio: Arc::new(MmioBus::new()),
            fences,
            log: slog::Logger::root(slog::Discard, slog::o!()),
        });
        ctrl.msix.set_enabled(true);
        for v in 0..NVME_MSIX_COUNT {
            ctrl.msix.write_entry(v, 0xFEE0_0000, u64::from(v));
        }
        Self {
            ctrl,
            physmap,
            sink,
            admin_tail: 0,
            admin_cq_head: 0,
            admin_phase: true,
            next_cid: 1,
        }
    }

    fn mem(&self) -> MemCtx {
        MemCtx::new(Arc::clone(&self.physmap))
    }

    fn fill(&self, gpa: u64, len: usize, byte: u8) {
        self.mem()
            .write(gpa, &vec![byte; len])
            .expect("fill guest RAM");
    }

    fn read_guest(&self, gpa: u64, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        self.mem().read(gpa, &mut buf).expect("read guest RAM");
        buf
    }

    fn reg_write32(&self, offset: usize, val: u32) {
        let wo = WriteOp::from_buf(&val.to_le_bytes());
        self.ctrl.bar_rw(BarN::BAR0, offset, RWOp::Write(&wo));
    }

    fn reg_write64(&self, offset: usize, val: u64) {
        let wo = WriteOp::from_buf(&val.to_le_bytes());
        self.ctrl.bar_rw(BarN::BAR0, offset, RWOp::Write(&wo));
    }

    fn reg_read32(&self, offset: usize) -> u32 {
        let mut ro = ReadOp::new(4);
        self.ctrl.bar_rw(BarN::BAR0, offset, RWOp::Read(&mut ro));
        u32::from_le_bytes(ro.buf().try_into().expect("four bytes"))
    }

    fn sq_doorbell(&self, qid: u16, tail: u16) {
        self.reg_write32(
            REG_DOORBELL_BASE + usize::from(qid) * 8,
            u32::from(tail),
        );
    }

    fn cq_doorbell(&self, qid: u16, head: u16) {
        self.reg_write32(
            REG_DOORBELL_BASE + usize::from(qid) * 8 + 4,
            u32::from(head),
        );
    }

    /// Program the admin queues and set CC.EN with the entry sizes a
    /// real driver writes.
    ///
    /// The controller builds fresh admin queues, so the harness starts
    /// its own head, tail and phase over too.
    fn enable(&mut self) {
        self.admin_tail = 0;
        self.admin_cq_head = 0;
        self.admin_phase = true;
        self.reg_write32(
            REG_AQA,
            u32::from(ADMIN_QDEPTH - 1) | (u32::from(ADMIN_QDEPTH - 1) << 16),
        );
        self.reg_write64(REG_ASQ, ASQ);
        self.reg_write64(REG_ACQ, ACQ);
        self.reg_write32(
            REG_CC,
            CC_EN
                | (SQE_SIZE_LOG2 << CC_IOSQES_SHIFT)
                | (CQE_SIZE_LOG2 << CC_IOCQES_SHIFT),
        );
    }

    fn csts(&self) -> u32 {
        self.reg_read32(REG_CSTS)
    }

    /// Submit one admin command and return its completion.
    fn admin(&mut self, sqe: Sqe) -> Cqe {
        let cid = self.next_cid;
        self.next_cid += 1;
        let entry = ASQ + u64::from(self.admin_tail) * SQE_SIZE as u64;
        self.mem()
            .write(entry, &sqe.encode(cid))
            .expect("write SQE");
        self.admin_tail = (self.admin_tail + 1) % ADMIN_QDEPTH;
        self.sq_doorbell(ADMIN_QUEUE_ID, self.admin_tail);

        let slot = ACQ + self.admin_cq_head as u64 * CQE_SIZE as u64;
        let cqe = Cqe::decode(&self.read_guest(slot, CQE_SIZE));
        assert_eq!(cqe.phase, self.admin_phase, "admin CQE phase");
        assert_eq!(cqe.cid, cid, "admin CQE cid");
        self.admin_cq_head += 1;
        if self.admin_cq_head == usize::from(ADMIN_QDEPTH) {
            self.admin_cq_head = 0;
            self.admin_phase = !self.admin_phase;
        }
        self.cq_doorbell(ADMIN_QUEUE_ID, self.admin_cq_head as u16);
        cqe
    }

    /// Create I/O queue pair `qid` of `depth` entries with the SQ at
    /// `sq_base` and the CQ at `cq_base`, MSI-X vector `qid`.
    fn create_io_queues(
        &mut self,
        qid: u16,
        depth: u16,
        sq_base: u64,
        cq_base: u64,
    ) {
        self.create_io_queues_with_irq(qid, depth, sq_base, cq_base, true);
    }

    /// The same pair, with `ien` deciding whether the CQ raises an
    /// interrupt. A Linux poll queue clears it.
    fn create_io_queues_with_irq(
        &mut self,
        qid: u16,
        depth: u16,
        sq_base: u64,
        cq_base: u64,
        ien: bool,
    ) {
        let irq = if ien { CQ_IEN } else { 0 };
        let cqe = self.admin(Sqe {
            opcode: ADMIN_OPC_CREATE_IO_CQ,
            prp1: cq_base,
            cdw10: u32::from(qid) | (u32::from(depth - 1) << 16),
            cdw11: (u32::from(qid) << 16) | irq | CQ_PC,
            ..Sqe::default()
        });
        assert_eq!(cqe.status(), Status::SUCCESS, "create CQ");
        let cqe = self.admin(Sqe {
            opcode: ADMIN_OPC_CREATE_IO_SQ,
            prp1: sq_base,
            cdw10: u32::from(qid) | (u32::from(depth - 1) << 16),
            cdw11: (u32::from(qid) << 16) | CQ_PC,
            ..Sqe::default()
        });
        assert_eq!(cqe.status(), Status::SUCCESS, "create SQ");
    }

    /// Poll a CQ slot until its phase bit reads `phase`.
    fn wait_cqe(&self, cq_base: u64, index: u16, phase: bool) -> Cqe {
        let slot = cq_base + u64::from(index) * CQE_SIZE as u64;
        let start = Instant::now();
        loop {
            let cqe = Cqe::decode(&self.read_guest(slot, CQE_SIZE));
            if cqe.phase == phase {
                return cqe;
            }
            assert!(start.elapsed() < WAIT, "no completion in {WAIT:?}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Set the mask bit in the MSI-X vector control of `vector`.
    fn mask_vector(&self, vector: u16) {
        let offset = usize::from(vector) * 16 + 12;
        let wo = WriteOp::from_buf(&1u32.to_le_bytes());
        self.ctrl.bar_rw(BarN::BAR4, offset, RWOp::Write(&wo));
    }

    /// The MSI-X pending bit array, first dword.
    fn pba(&self) -> u32 {
        let mut ro = ReadOp::new(4);
        self.ctrl.bar_rw(
            BarN::BAR4,
            self.ctrl.msix.pba_offset(),
            RWOp::Read(&mut ro),
        );
        u32::from_le_bytes(ro.buf().try_into().expect("four bytes"))
    }

    fn msi_count(&self) -> usize {
        self.sink.0.lock().expect("sink lock").len()
    }
}

#[derive(Default, Clone, Copy)]
struct Sqe {
    opcode: u8,
    nsid: u32,
    prp1: u64,
    prp2: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
}

impl Sqe {
    fn encode(&self, cid: u16) -> [u8; SQE_SIZE] {
        let mut b = [0u8; SQE_SIZE];
        b[SQE_OPC] = self.opcode;
        b[SQE_CID..SQE_CID + 2].copy_from_slice(&cid.to_le_bytes());
        b[SQE_NSID..SQE_NSID + 4].copy_from_slice(&self.nsid.to_le_bytes());
        b[SQE_PRP1..SQE_PRP1 + 8].copy_from_slice(&self.prp1.to_le_bytes());
        b[SQE_PRP2..SQE_PRP2 + 8].copy_from_slice(&self.prp2.to_le_bytes());
        b[SQE_CDW10..SQE_CDW10 + 4].copy_from_slice(&self.cdw10.to_le_bytes());
        b[SQE_CDW11..SQE_CDW11 + 4].copy_from_slice(&self.cdw11.to_le_bytes());
        b[SQE_CDW12..SQE_CDW12 + 4].copy_from_slice(&self.cdw12.to_le_bytes());
        b
    }

    fn read(slba: u64, nlb: u16, prp1: u64, prp2: u64) -> Self {
        Self {
            opcode: NVM_OPC_READ,
            nsid: 1,
            prp1,
            prp2,
            cdw10: slba as u32,
            cdw11: (slba >> 32) as u32,
            cdw12: u32::from(nlb - 1),
        }
    }

    fn write(slba: u64, nlb: u16, prp1: u64, prp2: u64) -> Self {
        Self {
            opcode: NVM_OPC_WRITE,
            ..Self::read(slba, nlb, prp1, prp2)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Status {
    sct: u8,
    sc: u8,
    dnr: bool,
}

impl Status {
    const SUCCESS: Self = Self {
        sct: 0,
        sc: 0,
        dnr: false,
    };

    /// The status a `SC_*` constant encodes, as the guest decodes it.
    fn of(code: u16) -> Self {
        Self {
            sct: ((code >> 8) & 0x7) as u8,
            sc: (code & 0xFF) as u8,
            dnr: code & SC_DNR != 0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Cqe {
    dw0: u32,
    sq_head: u16,
    sqid: u16,
    cid: u16,
    raw_status: u16,
    phase: bool,
}

impl Cqe {
    fn decode(b: &[u8]) -> Self {
        let raw = u16::from_le_bytes([b[CQE_STATUS], b[CQE_STATUS + 1]]);
        Self {
            dw0: u32::from_le_bytes(
                b[CQE_DW0..CQE_DW0 + 4].try_into().expect("dw0"),
            ),
            sq_head: u16::from_le_bytes([b[CQE_SQHD], b[CQE_SQHD + 1]]),
            sqid: u16::from_le_bytes([b[CQE_SQID], b[CQE_SQID + 1]]),
            cid: u16::from_le_bytes([b[CQE_CID], b[CQE_CID + 1]]),
            raw_status: raw,
            phase: raw & 1 != 0,
        }
    }

    fn status(&self) -> Status {
        Status {
            sct: ((self.raw_status >> 9) & 0x7) as u8,
            sc: ((self.raw_status >> 1) & 0xFF) as u8,
            dnr: self.raw_status & (1 << 15) != 0,
        }
    }
}

const SQ1: u64 = 0x30000;
const CQ1: u64 = 0x40000;
const IO_QDEPTH: u16 = 64;

/// Submit `sqe` on I/O queue 1 at `tail` and ring the doorbell.
fn io_submit(h: &mut Harness, tail: &mut u16, sqe: Sqe, cid: u16) {
    let entry = SQ1 + u64::from(*tail) * SQE_SIZE as u64;
    h.mem().write(entry, &sqe.encode(cid)).expect("write SQE");
    *tail = (*tail + 1) % IO_QDEPTH;
    h.sq_doorbell(1, *tail);
}

/// Write a PRP list entry at `gpa`.
fn put_prp(h: &Harness, gpa: u64, entry: u64) {
    h.mem().write(gpa, &entry.to_le_bytes()).expect("PRP entry");
}
