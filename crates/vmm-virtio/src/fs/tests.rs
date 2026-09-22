// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unit tests for the virtio-fs device.
//!
//! The transport publishes DEVICE_STATUS 0 when `reset` returns, and the
//! driver frees the vring when it reads that 0. So "`reset` has not
//! returned" means "the guest does not know the reset is complete".

use super::*;

mod intr;
mod reset;

use std::sync::Condvar;
use std::time::Instant;

use crate::pci::intr::{IntrGate, IntrSession};
use crate::queue::VirtqDesc;
use vmm_devices::Lifecycle;

/// Deadline on every wait that a parked backend can hold. A device that
/// wrongly waits on the backing store fails the test instead of hanging
/// the run. A loaded runner does not reach this, only a real fault.
const PARK_BUDGET: Duration = Duration::from_secs(10);

/// Longer than a plausible two-second drain deadline. A reset that
/// returns inside this window has a deadline it must not have.
const PAST_THE_OLD_DRAIN_BUDGET: Duration = Duration::from_millis(2500);

/// Budget for a stale backlog to retire. Work refused before it takes a
/// lock needs only a scheduling delay, so a backlog still counted after
/// this queued on the reset.
const BACKLOG_BUDGET: Duration = Duration::from_secs(2);

/// A drain that takes longer than this is waiting on something other
/// than a memory copy.
const DRAIN_BUDGET: Duration = Duration::from_secs(1);

/// Hold guest access as the worker does across a chain copy.
fn hold_guest_access(fs: &VirtioFs) -> Session<'_> {
    fs.access
        .enter_current(FS_REQUEST_QUEUE)
        .expect("a generation is open")
}

#[derive(Default)]
struct ParkState {
    /// The worker is inside the stand-in call.
    inside: bool,
    /// The test has let it out.
    released: bool,
    /// The stand-in call has returned.
    left: bool,
}

/// Stands in for a backing-store call that has not returned.
///
/// The device runs this in place of `FuseServer::run`, so it holds no
/// guest access while it waits. Every wait here has a deadline, so a
/// reset that blocks on the backing store fails the test.
#[derive(Default)]
struct ParkedBackend {
    state: Mutex<ParkState>,
    signal: Condvar,
}

impl ParkedBackend {
    /// Arm the stand-in on `fs`. It answers the next request only.
    fn install(fs: &VirtioFs) -> Arc<Self> {
        let this = Arc::new(Self::default());
        let hook = Arc::clone(&this);
        *fs.parks.in_backend.lock().expect("park lock") =
            Some(Arc::new(move || hook.park()));
        this
    }

    fn park(&self) {
        let mut state = self.state.lock().expect("park state");
        state.inside = true;
        self.signal.notify_all();
        while !state.released {
            let (next, timed_out) = self
                .signal
                .wait_timeout(state, PARK_BUDGET)
                .expect("park state");
            state = next;
            if timed_out.timed_out() {
                break;
            }
        }
        state.left = true;
        self.signal.notify_all();
    }

    /// Block until the worker is inside the stand-in call.
    fn wait_until_inside(&self) {
        let mut state = self.state.lock().expect("park state");
        while !state.inside {
            let (next, timed_out) = self
                .signal
                .wait_timeout(state, PARK_BUDGET)
                .expect("park state");
            state = next;
            assert!(
                !timed_out.timed_out(),
                "the worker never reached the backing store"
            );
        }
    }

    /// Whether the stand-in call is still running.
    fn still_running(&self) -> bool {
        !self.state.lock().expect("park state").left
    }

    /// Let the stand-in call return, and wait until it has.
    fn release(&self) {
        let mut state = self.state.lock().expect("park state");
        state.released = true;
        self.signal.notify_all();
        while !state.left {
            let (next, timed_out) = self
                .signal
                .wait_timeout(state, PARK_BUDGET)
                .expect("park state");
            state = next;
            assert!(!timed_out.timed_out(), "the backing store never returned");
        }
    }
}

/// Park the worker and block until it is there.
///
/// Until it resumes, the worker cannot observe what the test does, so
/// an assertion in between tests the caller only.
fn park_the_worker(fs: &VirtioFs) {
    fs.pause();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fs.is_quiesced() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        fs.is_quiesced(),
        "the worker did not park within its budget"
    );
}

/// Block until the worker owes the guest nothing.
fn wait_until_idle(fs: &VirtioFs) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs.inflight.load(Ordering::Acquire) != 0 && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        fs.inflight.load(Ordering::Acquire),
        0,
        "the worker never finished with the request"
    );
}

/// Block until the reset thread is inside the drain.
///
/// Admission closes before the drain and reopens after it, and the
/// caller holds a session, so once admission is closed the reset is in
/// the wait. A fixed sleep could let the assertions run after the reset
/// finished.
fn wait_until_reset_blocks(fs: &VirtioFs, finished: &AtomicBool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs.access.is_open() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!fs.access.is_open(), "the reset never started");
    assert!(
        !finished.load(Ordering::Acquire),
        "the reset finished while the worker still held guest access"
    );
}

const DESC_GPA: u64 = 0x1000;
const AVAIL_GPA: u64 = 0x1100;
const USED_GPA: u64 = 0x1200;
const REQ_GPA: u64 = 0x2000;
const RESP_GPA: u64 = 0x2800;
const RESP_LEN: usize = 256;
/// Byte the reply window carries before a reply can reach it.
const REPLY_SENTINEL: u8 = 0xA5;

fn test_logger() -> slog::Logger {
    slog::Logger::root(slog::Discard, slog::o!())
}

fn test_fs(physmap: Arc<PhysMap>) -> VirtioFs {
    let opts = VirtioFsOpts {
        tag: "shared".to_string(),
        path: std::env::temp_dir(),
        read_only: true,
        queue_size: FS_QUEUE_SIZE_DEFAULT,
    };
    VirtioFs::new(&opts, physmap, test_logger()).expect("open export")
}

/// Build a two-descriptor chain and an avail ring that publishes it
/// `avail_idx` times, mirroring `block.rs`'s ring fixture.
fn ring(
    queue_size: u16,
    avail_idx: u16,
) -> (Arc<PhysMap>, [VirtQueue; FS_NUM_QUEUES], VirtioFs) {
    let physmap = Arc::new(
        PhysMap::new_anon(DESC_GPA, 0x3000).expect("create queue memory"),
    );
    let mut queue = VirtQueue::new(queue_size);
    queue.set_addr_modern(DESC_GPA, AVAIL_GPA, USED_GPA);
    queue.set_event_idx(true);

    physmap
        .lookup(DESC_GPA, 16)
        .expect("mapped request descriptor")
        .write(&VirtqDesc {
            addr: REQ_GPA,
            len: 64,
            flags: crate::bits::VRING_DESC_F_NEXT,
            next: 1,
        })
        .expect("write request descriptor");
    physmap
        .lookup(DESC_GPA + 16, 16)
        .expect("mapped reply descriptor")
        .write(&VirtqDesc {
            addr: RESP_GPA,
            len: RESP_LEN as u32,
            flags: crate::bits::VRING_DESC_F_WRITE,
            next: 0,
        })
        .expect("write reply descriptor");

    physmap
        .lookup(AVAIL_GPA + 2, 2)
        .expect("mapped avail index")
        .write::<u16>(&avail_idx)
        .expect("write avail index");
    for idx in 0..queue_size {
        physmap
            .lookup(AVAIL_GPA + 4 + u64::from(idx) * 2, 2)
            .expect("mapped avail entry")
            .write::<u16>(&0)
            .expect("write avail entry");
    }

    let fs = test_fs(Arc::clone(&physmap));
    // Queue 0 is hiprio and stays unconfigured.
    (physmap, [VirtQueue::new(queue_size), queue], fs)
}

/// Write a FUSE_INIT into the chain's readable segment, so the reply
/// the worker would publish has a body worth catching.
fn seed_fuse_init(physmap: &PhysMap) {
    let hdr = fuse::FuseInHeader {
        len: (fuse::FUSE_IN_HEADER_SIZE + size_of::<fuse::FuseInitIn>()) as u32,
        opcode: fuse::FUSE_INIT,
        unique: 1,
        nodeid: 0,
        uid: 0,
        gid: 0,
        pid: 0,
        padding: 0,
    };
    let mut buf = fuse::bytes_of(&hdr);
    fuse::push_val(
        &mut buf,
        &fuse::FuseInitIn {
            major: 7,
            minor: 33,
            max_readahead: 0x2_0000,
            flags: 0,
        },
    );
    physmap
        .lookup(REQ_GPA, buf.len())
        .expect("mapped request")
        .write_bytes(&buf)
        .expect("seed the request");
}

/// Fill the chain's writable segment with a sentinel, so a reply that
/// lands there after a reset is visible.
fn seed_reply_window(physmap: &PhysMap) {
    physmap
        .lookup(RESP_GPA, RESP_LEN)
        .expect("mapped reply window")
        .write_bytes(&[REPLY_SENTINEL; RESP_LEN])
        .expect("seed the reply window");
}

fn reply_window(physmap: &PhysMap) -> Vec<u8> {
    let mut out = vec![0u8; RESP_LEN];
    physmap
        .lookup(RESP_GPA, RESP_LEN)
        .expect("mapped reply window")
        .read_bytes(&mut out)
        .expect("read the reply window");
    out
}

fn avail_event(physmap: &PhysMap, queue: &VirtQueue) -> u16 {
    let gpa = queue.used_addr() + 4 + u64::from(queue.size()) * 8;
    physmap
        .lookup(gpa, 2)
        .expect("mapped avail event")
        .read::<u16>()
        .expect("read avail event")
}

// VirtQueue::new panics on a non-power-of-two size, so the catalog must
// pass clamp_queue_size's result to VirtioPciDevice::new.
#[test]
fn queue_size_is_clamped_and_rounded_up() {
    assert_eq!(clamp_queue_size(0), FS_QUEUE_SIZE_MIN);
    assert_eq!(clamp_queue_size(8), 8);
    assert_eq!(clamp_queue_size(100), 128);
    assert_eq!(clamp_queue_size(u16::MAX), FS_QUEUE_SIZE_MAX);
    for n in [0u16, 1, 7, 8, 9, 100, 512, 1023, 1024, 4096, u16::MAX] {
        let q = clamp_queue_size(n);
        assert!(q.is_power_of_two(), "{q} is not a power of two");
        assert!((FS_QUEUE_SIZE_MIN..=FS_QUEUE_SIZE_MAX).contains(&q));
    }
}

// Linux's virtio_cread bounds-checks offset+len against the advertised
// device-config length, so FS_CONFIG_SIZE covers the whole spec struct.
#[test]
fn config_size_covers_tag_and_both_trailing_fields() {
    assert_eq!(FS_CONFIG_SIZE as usize, FS_TAG_LEN + 4 + 4);
    assert_eq!(FS_CONFIG_SIZE, 44);
}

#[test]
fn a_tag_the_config_field_cannot_hold_is_refused_by_new() {
    let opts = VirtioFsOpts {
        tag: "t".repeat(FS_TAG_LEN + 1),
        path: std::env::temp_dir(),
        read_only: true,
        queue_size: FS_QUEUE_SIZE_DEFAULT,
    };
    let Err(err) =
        VirtioFs::new(&opts, Arc::new(PhysMap::new()), test_logger())
    else {
        panic!("an oversized tag must not build a device");
    };
    assert!(
        err.to_string()
            .ends_with(&format!("exceeds {FS_TAG_LEN} bytes")),
        "got: {err}"
    );
}

#[test]
fn queue_count_fits_the_transport_limit() {
    // VirtioPciDevice::new asserts num_queues <= MAX_QUEUES (4).
    assert!(FS_NUM_QUEUES <= 4);
    assert_eq!(FS_HIPRIO_QUEUE, 0);
    assert_eq!(FS_REQUEST_QUEUE, 1);
}

#[test]
fn device_config_serves_the_tag_then_num_request_queues() {
    let fs = test_fs(Arc::new(PhysMap::new()));

    let mut tag = Vec::new();
    for off in 0..FS_TAG_LEN as u16 {
        tag.push(fs.cfg_read(off, 1) as u8);
    }
    assert_eq!(&tag[..6], b"shared");
    assert!(tag[6..].iter().all(|b| *b == 0));

    assert_eq!(fs.cfg_read(36, 4), FS_NUM_REQUEST_QUEUES);
    assert_eq!(fs.cfg_read(40, 4), 0, "notify_buf_size");
    assert_eq!(fs.cfg_read(FS_CONFIG_SIZE, 4), 0, "past the config");

    let feat = fs.device_features();
    assert_ne!(feat & crate::bits::VIRTIO_F_RING_EVENT_IDX, 0);
    assert_ne!(feat & crate::bits::VIRTIO_F_RING_INDIRECT_DESC, 0);
}

// The used ring belongs to VirtioCompletion, so notify_queue drains the
// avail ring and updates avail_event without a call to push_used.
#[test]
fn notify_queue_drains_the_ring_and_never_pushes_used() {
    let (physmap, mut queues, fs) = ring(8, 2);
    let req = usize::from(FS_REQUEST_QUEUE);

    assert!(!fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap));

    assert_eq!(queues[req].last_avail_idx(), 2);
    assert_eq!(avail_event(&physmap, &queues[req]), 2);
    assert_eq!(queues[req].read_used_idx(&physmap), 0);
}

// A guest can keep advancing avail_idx during the EVENT_IDX re-check.
// One notification must retain at most one queue's worth of work.
//
// The cap exit still arms the kick. Otherwise the guest suppresses the
// notification the device waits for, and the queue stalls for the life
// of the driver.
#[test]
fn notify_queue_caps_collection_at_queue_size() {
    const AVAIL_EVENT_SENTINEL: u16 = 0x55aa;

    // Four entries keep has_new_avail() true after this two-entry queue
    // takes one queue's worth.
    let (physmap, mut queues, fs) = ring(2, 4);
    let req = usize::from(FS_REQUEST_QUEUE);
    let gpa = queues[req].used_addr() + 4 + u64::from(queues[req].size()) * 8;
    physmap
        .lookup(gpa, 2)
        .expect("mapped avail event")
        .write::<u16>(&AVAIL_EVENT_SENTINEL)
        .expect("write avail event sentinel");

    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);

    assert_eq!(queues[req].last_avail_idx(), queues[req].size());
    assert!(queues[req].has_new_avail(&physmap));
    assert_eq!(
        avail_event(&physmap, &queues[req]),
        queues[req].last_avail_idx(),
        "the cap exit left the guest's kick suppressed"
    );
    assert_eq!(queues[req].read_used_idx(&physmap), 0);
}

#[test]
fn reset_clears_the_negotiated_features_and_completions() {
    let (physmap, mut queues, fs) = ring(8, 1);
    fs.set_features(crate::bits::VIRTIO_F_RING_EVENT_IDX);
    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
    assert!(fs.completions.lock().expect("completions")
        [usize::from(FS_REQUEST_QUEUE)]
    .is_some());

    VirtioDevice::reset(&fs);

    assert_eq!(*fs.negotiated_features.lock().expect("features"), 0);
    assert!(fs
        .completions
        .lock()
        .expect("completions")
        .iter()
        .all(Option::is_none));
}

// A guest that writes 0 to DEVICE_STATUS frees the vring directly
// after. A reply to a request already queued must not reach the used
// ring the guest has recycled.
#[test]
fn reset_drops_a_request_queued_before_it() {
    let (physmap, mut queues, fs) = ring(8, 1);
    let req = usize::from(FS_REQUEST_QUEUE);

    // Park the worker so the request stays in the channel.
    park_the_worker(&fs);

    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
    assert_eq!(
        queues[req].read_used_ring_idx(&physmap),
        0,
        "the parked worker must not have run yet"
    );

    VirtioDevice::reset(&fs);
    fs.resume();

    // The worker decrements the count as it drops the stale request, so
    // zero means it has handled it. A timed window would pass on a
    // runner that never scheduled the worker.
    wait_until_idle(&fs);
    assert_eq!(
        queues[req].read_used_ring_idx(&physmap),
        0,
        "the worker published into the pre-reset used ring"
    );
}

// A reset must leave the worker's gate open, or the next driver's first
// request never completes.
#[test]
fn reset_leaves_the_worker_runnable() {
    let (physmap, mut queues, fs) = ring(8, 1);
    let req = usize::from(FS_REQUEST_QUEUE);

    VirtioDevice::reset(&fs);
    assert!(!fs.gate.is_paused(), "reset left the worker parked");

    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);

    let deadline = Instant::now() + Duration::from_secs(5);
    while queues[req].read_used_ring_idx(&physmap) == 0
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        queues[req].read_used_ring_idx(&physmap),
        1,
        "a post-reset request never completed"
    );
}

// A guest resets its virtio devices several times during boot, and the
// worker takes up to 50 ms to reach its park point. The reset waits only
// on guest access, so it must never park the worker.
#[test]
fn reset_never_parks_the_worker() {
    let (physmap, mut queues, fs) = ring(8, 1);

    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
    wait_until_idle(&fs);

    VirtioDevice::reset(&fs);

    assert_eq!(
        fs.gate.drain_count(),
        0,
        "reset parked the worker to drain it"
    );
    assert!(!fs.gate.is_paused(), "reset left the worker parked");
}

// A reset must not close the fd tables: the worker holds raw fds from
// them across a FUSE syscall, and a number closed under it goes to the
// next thread that opens a file. The worker reaps them when it sees the
// mark.
#[test]
fn the_worker_reaps_the_fd_tables_a_reset_retired() {
    let dir = tempfile::tempdir().expect("export directory");
    // `bind_restricted` changes the process umask while it binds, and
    // the vsock tests run in parallel, so the creation mode is not
    // reliable.
    std::fs::set_permissions(
        dir.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("export directory mode");
    std::fs::write(dir.path().join("f"), b"hello").expect("seed file");
    let opts = VirtioFsOpts {
        tag: "shared".to_string(),
        path: dir.path().to_path_buf(),
        read_only: true,
        queue_size: FS_QUEUE_SIZE_DEFAULT,
    };
    let fs = VirtioFs::new(&opts, Arc::new(PhysMap::new()), test_logger())
        .expect("open export");
    fs.start().expect("start");

    let name = std::ffi::CString::new("f").expect("file name");
    let entry = fs
        .server
        .passthrough()
        .lookup(fuse::FUSE_ROOT_ID, &name)
        .expect("look the file up");

    // The worker is parked, so the reset cannot reap the tables itself
    // and the mark must outlive it.
    park_the_worker(&fs);
    VirtioDevice::reset(&fs);
    fs.server
        .passthrough()
        .getattr(entry.nodeid)
        .expect("the reset closed an fd the worker could still hold");

    fs.resume();
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs.server.passthrough().getattr(entry.nodeid).is_ok()
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        fs.server.passthrough().getattr(entry.nodeid).is_err(),
        "the worker never reaped the retired session's fd tables"
    );
}

// A worker inside a copy has already passed its generation check. The
// reset must not finish while the worker holds the shared side. It needs
// no deadline, because the sections under that guard are bounded
// copies.
#[test]
fn a_reset_waits_out_a_copy_into_the_chain() {
    let fs = test_fs(Arc::new(PhysMap::new()));
    fs.start().expect("start");

    let access = hold_guest_access(&fs);
    let finished = AtomicBool::new(false);
    let fs_ref = &fs;
    let finished_ref = &finished;

    std::thread::scope(|scope| {
        scope.spawn(move || {
            VirtioDevice::reset(fs_ref);
            finished_ref.store(true, Ordering::Release);
        });

        wait_until_reset_blocks(&fs, &finished);

        let until = Instant::now() + PAST_THE_OLD_DRAIN_BUDGET;
        while Instant::now() < until {
            assert!(
                !finished.load(Ordering::Acquire),
                "the reset told the guest it was done while the worker \
                 could still write into the ring"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        drop(access);
    });

    assert!(
        finished.load(Ordering::Acquire),
        "the reset never finished after the worker let the ring go"
    );
}

// The guest drives the reset wait, as often as it likes. A pause from a
// migration or an operator that lands during the wait belongs to the
// control plane, and the reset must not release it.
#[test]
fn a_pause_that_lands_inside_a_reset_survives_it() {
    let fs = test_fs(Arc::new(PhysMap::new()));
    fs.start().expect("start");

    let access = hold_guest_access(&fs);
    let finished = AtomicBool::new(false);
    let fs_ref = &fs;
    let finished_ref = &finished;

    std::thread::scope(|scope| {
        scope.spawn(move || {
            VirtioDevice::reset(fs_ref);
            finished_ref.store(true, Ordering::Release);
        });

        // Pause only once the reset is inside the wait.
        wait_until_reset_blocks(&fs, &finished);
        fs.pause();

        drop(access);
    });

    assert!(
        finished.load(Ordering::Acquire),
        "the reset never finished after the worker let the ring go"
    );
    assert!(
        fs.gate.is_paused(),
        "the reset released a pause the control plane owns"
    );
}

// A backing-store call owns only host memory, so a reset can end the
// driver session while it runs and nothing it produces reaches the
// guest. The worker has already copied the request out and passed its
// first generation check.
#[test]
fn a_backend_parked_across_a_reset_writes_nothing_into_the_ring() {
    let (physmap, mut queues, fs) = ring(8, 1);
    let req = usize::from(FS_REQUEST_QUEUE);
    seed_fuse_init(&physmap);
    seed_reply_window(&physmap);
    fs.start().expect("start");

    let hold = Arc::new(AtomicBool::new(false));
    let (raised, _inside) = counting_interrupt(&fs, &hold);
    let backend = ParkedBackend::install(&fs);
    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
    backend.wait_until_inside();

    VirtioDevice::reset(&fs);
    backend.release();
    wait_until_idle(&fs);

    assert_eq!(
        queues[req].read_used_ring_idx(&physmap),
        0,
        "the reply was published into the ring the guest had freed"
    );
    assert!(
        reply_window(&physmap).iter().all(|b| *b == REPLY_SENTINEL),
        "the reply landed in guest pages the reset had released"
    );
    assert_eq!(
        raised.load(Ordering::Acquire),
        0,
        "the closed session's interrupt reached the next driver"
    );
}

// A driver can program one ring again without a device reset, and it
// frees the old ring directly. A request already with the worker holds
// the old chain and its own writer, so dropping the cached writer alone
// lets the reply reach pages the guest has taken back.
#[test]
fn a_reprogrammed_queue_keeps_a_stale_reply_out_of_the_old_ring() {
    // Free, mapped ring memory past the fixture's own.
    const NEW_DESC_GPA: u64 = 0x3000;
    const NEW_AVAIL_GPA: u64 = 0x3100;
    const NEW_USED_GPA: u64 = 0x3200;

    let (physmap, mut queues, fs) = ring(8, 1);
    let req = usize::from(FS_REQUEST_QUEUE);
    seed_fuse_init(&physmap);
    seed_reply_window(&physmap);
    fs.start().expect("start");

    let backend = ParkedBackend::install(&fs);
    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
    backend.wait_until_inside();

    queues[req].set_addr_modern(NEW_DESC_GPA, NEW_AVAIL_GPA, NEW_USED_GPA);
    fs.queue_addr_set(FS_REQUEST_QUEUE, &queues[req]);

    backend.release();
    wait_until_idle(&fs);

    let old_used_idx = physmap
        .lookup(USED_GPA + 2, 2)
        .expect("mapped old used index")
        .read::<u16>()
        .expect("read old used index");
    assert_eq!(
        old_used_idx, 0,
        "the reply was published into the ring the driver gave up"
    );
    assert!(
        reply_window(&physmap).iter().all(|b| *b == REPLY_SENTINEL),
        "the reply landed in guest pages the reprogram had released"
    );
    assert_eq!(
        queues[req].read_used_ring_idx(&physmap),
        0,
        "a request of the old ring was answered on the new one"
    );
}

// The illumos legacy driver writes the reset register once and does not
// poll, so the reset runs on the vCPU. A host filesystem that never
// answers must not hold that vCPU.
#[test]
fn a_reset_does_not_wait_for_the_backend() {
    let (physmap, mut queues, fs) = ring(8, 1);
    seed_fuse_init(&physmap);
    fs.start().expect("start");

    let backend = ParkedBackend::install(&fs);
    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
    backend.wait_until_inside();

    VirtioDevice::reset(&fs);

    assert!(
        backend.still_running(),
        "the reset waited for the backing store"
    );
    backend.release();
}

// A halt abandons the request the worker holds and every one queued
// behind it. The count must drop them all.
#[test]
fn halt_clears_the_count_of_every_request_it_abandons() {
    let (physmap, mut queues, fs) = ring(8, 3);
    fs.start().expect("start");

    // Park the worker so the requests queue behind it.
    park_the_worker(&fs);

    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
    assert_eq!(fs.inflight.load(Ordering::Acquire), 3);

    fs.halt();

    assert_eq!(
        fs.inflight.load(Ordering::Acquire),
        0,
        "halt left abandoned requests on the count"
    );
}

// halt runs only from the paused state, and it must join the worker it
// unparks.
#[test]
fn halt_joins_the_worker_after_a_pause() {
    let fs = test_fs(Arc::new(PhysMap::new()));
    fs.start().expect("start");
    park_the_worker(&fs);

    fs.halt();
    assert!(fs.worker.lock().expect("worker lock").is_none());
    assert_eq!(fs.lifecycle_state(), Some(IndicatedState::Halt));
}

/// Install a queue interrupt that counts and, while `hold` is set,
/// blocks inside the callback.
fn counting_interrupt(
    fs: &VirtioFs,
    hold: &Arc<AtomicBool>,
) -> (Arc<AtomicUsize>, Arc<AtomicBool>) {
    let raised = Arc::new(AtomicUsize::new(0));
    let inside = Arc::new(AtomicBool::new(false));
    let hook = {
        let raised = Arc::clone(&raised);
        let inside = Arc::clone(&inside);
        let hold = Arc::clone(hold);
        BackendIntr::detached(move |_session, _queue| {
            raised.fetch_add(1, Ordering::AcqRel);
            inside.store(true, Ordering::Release);
            while hold.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    fs.interrupt.install(hook);
    (raised, inside)
}

// The guest controls how much stale work is queued when it writes 0 to
// DEVICE_STATUS. None of it may queue on the lock the resetting vCPU
// waits for: `std::sync::RwLock` gives the writer no priority, so
// readers could pass the lock among themselves and hold the vCPU.
//
// A request refused at admission never publishes, so it never reaches
// the interrupt gate either.
#[test]
fn a_stale_backlog_never_queues_on_the_reset_drain() {
    const BACKLOG: u16 = FS_QUEUE_SIZE_DEFAULT;
    let (physmap, mut queues, fs) = ring(BACKLOG, BACKLOG);
    let req = usize::from(FS_REQUEST_QUEUE);
    fs.start().expect("start");

    // Park the worker so a full queue piles up behind it.
    park_the_worker(&fs);
    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
    assert_eq!(
        fs.inflight.load(Ordering::Acquire),
        usize::from(BACKLOG),
        "the backlog never reached the worker"
    );

    // A reset, stopped inside its drain: admission closed, every
    // generation ended, and the exclusive side held.
    fs.access.close_all();
    let drain = fs.access.hold_drain();
    fs.resume();

    let deadline = Instant::now() + BACKLOG_BUDGET;
    while fs.inflight.load(Ordering::Acquire) != 0 && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    let left = fs.inflight.load(Ordering::Acquire);
    // Release the drain before the assert, so a backlog that wrongly
    // queued on it does not wedge the teardown.
    drop(drain);
    fs.access.reopen(IntrSession::INITIAL);

    assert_eq!(
        left, 0,
        "{left} stale requests queued on the lock the reset waits for"
    );
    assert_eq!(
        queues[req].read_used_ring_idx(&physmap),
        0,
        "a stale request published into the ring the guest had freed"
    );
}

// The raise must happen outside the session that wrote the ring.
// Delivery is an ioctl and can block, and the reset drain covers every
// live session, so a raise inside one puts the resetting vCPU behind
// it.
#[test]
fn an_interrupt_is_raised_outside_the_guest_access_section() {
    let (physmap, mut queues, fs) = ring(8, 1);
    seed_fuse_init(&physmap);
    fs.start().expect("start");

    let hold = Arc::new(AtomicBool::new(true));
    let (_raised, inside) = counting_interrupt(&fs, &hold);

    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
    let deadline = Instant::now() + PARK_BUDGET;
    while !inside.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        inside.load(Ordering::Acquire),
        "the worker never raised the interrupt"
    );

    // The interrupt is held. A drain that waits for it fails, so the
    // drain runs on its own thread under a deadline.
    let waited = {
        let access = &fs.access;
        let hold = &hold;
        std::thread::scope(move |scope| {
            let drainer = scope.spawn(move || access.drain());
            let start = Instant::now();
            while !drainer.is_finished() && start.elapsed() < DRAIN_BUDGET {
                std::thread::sleep(Duration::from_millis(1));
            }
            let waited = start.elapsed();
            // Release a wrongly held interrupt so the scope can join.
            hold.store(false, Ordering::Release);
            waited
        })
    };

    assert!(
        waited < DRAIN_BUDGET,
        "the drain waited {waited:?} behind an interrupt raised inside a \
         guest-access section"
    );
}

// A DAX window is guest-accessible memory that no session covers. The
// guest keeps the mapping across a device reset, so the reset cannot
// take it back. The device offers no window, and its config must say so.
#[test]
fn the_device_publishes_no_dax_window() {
    let fs = test_fs(Arc::new(PhysMap::new()));

    // notify_buf_size, the last field of virtio_fs_config. A DAX device
    // also needs a shared-memory region capability, which this transport
    // never builds.
    assert_eq!(fs.cfg_read(40, 4), 0);
    assert_eq!(FS_CONFIG_SIZE as usize, FS_TAG_LEN + 4 + 4);
}

// The vCPU half of the same gate. A notification during a reset must
// not walk the ring or queue work against it. The transport serialises
// the two writes, but the device must not depend on that.
#[test]
fn a_notification_refused_by_a_reset_touches_nothing() {
    let (physmap, mut queues, fs) = ring(8, 1);
    let req = usize::from(FS_REQUEST_QUEUE);
    fs.start().expect("start");

    // A reset has taken the ring. The drain is not held: this thread
    // calls in, and a gate that wrongly took the reader lock would wait
    // on itself instead of failing.
    fs.access.close_all();

    let did_work = fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);

    fs.access.reopen(IntrSession::INITIAL);

    assert!(!did_work);
    assert_eq!(
        queues[req].last_avail_idx(),
        0,
        "a notification walked a ring the reset had taken"
    );
    assert_eq!(
        fs.inflight.load(Ordering::Acquire),
        0,
        "a notification queued work against a ring the reset had taken"
    );
}

// Teardown bounds each halt by this value. A device that waits longer
// than it declares is abandoned before it returns, and what the halt
// releases stays held.
#[test]
fn the_declared_halt_budget_is_the_deadline_the_halt_polls() {
    let fs = test_fs(Arc::new(PhysMap::new()));
    assert_eq!(Lifecycle::halt_budget(&fs), FS_HALT_BUDGET);
}
