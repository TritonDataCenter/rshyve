// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! virtio-fs: FUSE over a virtqueue, with a host passthrough backend.
//!
//! No DAX and no shared-memory window: every reply travels through the
//! request queue, so the guest sees a plain FUSE transport.

pub mod fuse;
pub mod passthrough;
pub mod server;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vmm_core::mem::PhysMap;

use self::passthrough::Passthrough;
use self::server::FuseServer;
use super::queue::{ChainBuf, VirtQueue, VirtioCompletion};
use super::VirtioDevice;
use crate::access::{GuestAccess, RingTag, Session};
use crate::pci::intr::{BackendIntr, IntrSlot};
use vmm_devices::lifecycle::{IndicatedState, Indicator};
use vmm_devices::{FlushError, FlushIntent, Lifecycle, QuiesceGate};

/// Queue 0 is hiprio, queues 1..=N are request queues (VirtIO 1.3 5.11).
pub const FS_NUM_QUEUES: usize = 2;
/// Hiprio queue. One worker serves both queues, so a FUSE_INTERRUPT or
/// FORGET waits behind whatever request is already in flight.
pub const FS_HIPRIO_QUEUE: u16 = 0;
pub const FS_REQUEST_QUEUE: u16 = 1;
pub const FS_NUM_REQUEST_QUEUES: u32 = 1;
/// Full `struct virtio_fs_config`: tag[36] + num_request_queues +
/// notify_buf_size. Sized to the whole struct because Linux bounds-checks
/// `offset + len` against the advertised device-config length.
pub const FS_CONFIG_SIZE: u16 = 44;
pub const FS_TAG_LEN: usize = 36;
pub const FS_QUEUE_SIZE_DEFAULT: u16 = 128;
pub const FS_QUEUE_SIZE_MIN: u16 = 8;
pub const FS_QUEUE_SIZE_MAX: u16 = 1024;
/// One MSI-X vector per queue, plus one for a config change.
pub const FS_MSIX_VECTORS: u16 = FS_NUM_QUEUES as u16 + 1;
/// Completions forced through while the guest's EVENT_IDX threshold is
/// still settling, so an early reply always wakes the fuse driver.
const FS_FORCE_INTERRUPTS: u32 = 32;
/// Budget for the worker to finish its request during halt.
const FS_HALT_BUDGET: Duration = Duration::from_secs(5);

#[cfg(test)]
type ParkSlot = Mutex<Option<Arc<dyn Fn() + Send + Sync>>>;

/// Park points a test uses to pin a reset against the worker.
#[cfg(test)]
#[derive(Default)]
struct FsParks {
    /// Entered where the backing store runs, with no guest access held.
    in_backend: ParkSlot,
}

#[cfg(test)]
fn run_park(slot: &ParkSlot) {
    let hook = slot.lock().expect("park lock poisoned").take();
    if let Some(hook) = hook {
        hook();
    }
}

/// Parsed `-s <slot>,virtio-fs,<config>` options.
#[derive(Debug, Clone)]
pub struct VirtioFsOpts {
    /// Mount tag the guest sees in device config.
    pub tag: String,
    /// Host directory to export.
    pub path: std::path::PathBuf,
    pub read_only: bool,
    /// Requested per-queue size, normalized by [`clamp_queue_size`].
    pub queue_size: u16,
}

impl Default for VirtioFsOpts {
    fn default() -> Self {
        Self {
            tag: String::new(),
            path: std::path::PathBuf::new(),
            read_only: false,
            queue_size: FS_QUEUE_SIZE_DEFAULT,
        }
    }
}

/// Normalize a requested queue size to something `VirtQueue::new` accepts:
/// clamped to 8..=1024, then rounded up to a power of two.
///
/// `VirtQueue::new` panics on a non-power-of-two size, so the catalog must
/// pass this result to `VirtioPciDevice::new`, not the raw user value.
pub fn clamp_queue_size(requested: u16) -> u16 {
    let n = requested.clamp(FS_QUEUE_SIZE_MIN, FS_QUEUE_SIZE_MAX);
    n.next_power_of_two().min(FS_QUEUE_SIZE_MAX)
}

/// One descriptor chain handed to the FUSE worker.
struct FsRequest {
    head: u16,
    bufs: Vec<ChainBuf>,
    completion: Arc<VirtioCompletion>,
    /// Ring and generation this request was queued under. The worker
    /// drops the request when it no longer matches: the chain and the
    /// completion both point at a ring the driver has since torn down.
    tag: RingTag,
}

pub struct VirtioFs {
    tag: [u8; FS_TAG_LEN],
    server: Arc<FuseServer>,
    physmap: Arc<PhysMap>,
    negotiated_features: Mutex<u64>,
    /// Per-queue completion handlers, created lazily on first notification.
    completions: Mutex<Vec<Option<Arc<VirtioCompletion>>>>,
    work_tx: Mutex<Option<mpsc::Sender<FsRequest>>>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    shutdown: Arc<AtomicBool>,
    gate: Arc<QuiesceGate>,
    /// See [`FsWorkerCtx::access`].
    access: Arc<GuestAccess>,
    /// Requests the guest is still owed. See [`FsWorkerCtx::inflight`].
    inflight: Arc<AtomicUsize>,
    /// See [`FsWorkerCtx::session_retired`].
    session_retired: Arc<AtomicBool>,
    #[cfg(test)]
    parks: Arc<FsParks>,
    indicator: Indicator,
    log: slog::Logger,
    /// The transport interrupt path. Clone this Arc BEFORE moving the
    /// device into `VirtioPciDevice::new`.
    interrupt: Arc<IntrSlot>,
}

impl VirtioFs {
    /// Install the transport interrupt path once the transport exists.
    pub fn install_interrupt(&self, intr: Arc<BackendIntr>) {
        self.interrupt.install(intr);
    }

    /// Open the passthrough root, build the FUSE server, and spawn the
    /// worker thread.
    pub fn new(
        opts: &VirtioFsOpts,
        physmap: Arc<PhysMap>,
        log: slog::Logger,
    ) -> anyhow::Result<Self> {
        // Process-wide, and idempotent, so every device may do it.
        match passthrough::raise_fd_limit() {
            Ok(soft) => {
                slog::debug!(log, "virtio-fs fd limit"; "soft" => soft)
            }
            Err(error) => slog::warn!(
                log,
                "virtio-fs could not raise the fd limit"; "error" => %error
            ),
        }
        let pt = Passthrough::new(&opts.path, opts.read_only).map_err(|e| {
            anyhow::anyhow!(
                "open passthrough root '{}': {}",
                opts.path.display(),
                std::io::Error::from_raw_os_error(e.to_errno())
            )
        })?;
        let server = Arc::new(FuseServer::new(Arc::new(pt)));

        // The options struct is public, so a caller can reach here past
        // parse_fs_config. Refuse a tag the config field cannot hold
        // rather than panic in copy_from_slice.
        let bytes = opts.tag.as_bytes();
        anyhow::ensure!(
            bytes.len() <= FS_TAG_LEN,
            "virtio-fs tag '{}' exceeds {} bytes",
            opts.tag,
            FS_TAG_LEN
        );
        let mut tag = [0u8; FS_TAG_LEN];
        tag[..bytes.len()].copy_from_slice(bytes);

        let ctx = FsWorkerCtx {
            server: Arc::clone(&server),
            physmap: Arc::clone(&physmap),
            gate: Arc::new(QuiesceGate::new(1)),
            shutdown: Arc::new(AtomicBool::new(false)),
            access: Arc::new(GuestAccess::new(FS_NUM_QUEUES)),
            inflight: Arc::new(AtomicUsize::new(0)),
            session_retired: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            parks: Arc::new(FsParks::default()),
        };
        let (tx, rx) = mpsc::channel::<FsRequest>();

        let worker = std::thread::Builder::new()
            .name("virtio-fs".to_string())
            .spawn({
                let ctx = ctx.clone();
                move || fs_worker(rx, ctx)
            })?;

        Ok(Self {
            tag,
            server,
            physmap,
            negotiated_features: Mutex::new(0),
            completions: Mutex::new(vec![None; FS_NUM_QUEUES]),
            work_tx: Mutex::new(Some(tx)),
            worker: Mutex::new(Some(worker)),
            shutdown: ctx.shutdown,
            gate: ctx.gate,
            access: ctx.access,
            inflight: ctx.inflight,
            session_retired: ctx.session_retired,
            #[cfg(test)]
            parks: ctx.parks,
            indicator: Indicator::new(),
            log,
            interrupt: Arc::new(IntrSlot::new()),
        })
    }

    /// Get or create the completion handler for one queue.
    ///
    /// The caller's session stamps the handler. A reset refuses every
    /// session, so the next driver never finds a handler that carries
    /// the ended session.
    fn get_completion(
        &self,
        access: &Session<'_>,
        queue_idx: u16,
        queue: &VirtQueue,
    ) -> Arc<VirtioCompletion> {
        let mut slots = self.completions.lock().expect("completion lock");
        let idx = usize::from(queue_idx);
        if let Some(Some(c)) = slots.get(idx) {
            return Arc::clone(c);
        }

        let intr = Arc::clone(&self.interrupt);
        let c = VirtioCompletion::new(
            queue,
            Arc::clone(&self.physmap),
            access.intr(),
            move |session| {
                intr.raise(session, queue_idx);
            },
        );
        c.set_force_interrupt(FS_FORCE_INTERRUPTS);
        if let Some(slot) = slots.get_mut(idx) {
            *slot = Some(Arc::clone(&c));
        }
        c
    }

    /// End the retired generations, wait out the guest-memory sections
    /// already inside them, and drop the used-ring writers they name.
    ///
    /// `queue` names the one ring a driver programmed again, or `None`
    /// for a device reset. Dropping the cached writer is not sufficient:
    /// a request already with the worker holds its own writer, its chain
    /// and a generation that was valid when it left the vCPU. Ending
    /// that generation and draining its copies keeps the reply out of
    /// pages the guest has taken back.
    ///
    /// Admission closes before the wait. `std::sync::RwLock` gives the
    /// writer no priority, and the guest controls how much stale work
    /// waits, so a section must be refused before it queues on the lock.
    ///
    /// The backing store is not waited for. A request inside it
    /// finishes into host memory, fails the generation check, and
    /// writes nothing.
    fn retire_rings(&self, queue: Option<u16>) {
        match queue {
            Some(idx) => {
                self.access.close_queue(idx);
            }
            None => self.access.close_all(),
        }
        self.access.drain();

        {
            let mut slots = self.completions.lock().expect("completion lock");
            match queue {
                Some(idx) => {
                    if let Some(slot) = slots.get_mut(usize::from(idx)) {
                        *slot = None;
                    }
                }
                None => {
                    for slot in slots.iter_mut() {
                        *slot = None;
                    }
                }
            }
        }
        self.access
            .reopen(self.interrupt.next_session(self.access.intr_session()));
    }
}

/// State the FUSE worker shares with the device.
#[derive(Clone)]
struct FsWorkerCtx {
    server: Arc<FuseServer>,
    physmap: Arc<PhysMap>,
    gate: Arc<QuiesceGate>,
    shutdown: Arc<AtomicBool>,
    /// Permission to touch the guest's ring and chains, and the ring
    /// generation every request is tagged with (see [`FsRequest::tag`]).
    ///
    /// The worker holds a session around each copy to or from a chain
    /// and around the used entry that follows. [`VirtioDevice::reset`]
    /// closes admission and drains those sections.
    ///
    /// No section may wait on the backing store, park on the quiesce
    /// gate, or raise an interrupt. A vCPU waits for the drain, so
    /// each section must end on its own.
    access: Arc<GuestAccess>,
    /// Requests the guest is still owed. The worker decrements it after
    /// it publishes the used entry. `reset` does not read it.
    inflight: Arc<AtomicUsize>,
    /// Set by a reset, cleared by the worker after it closes the retired
    /// session's fd tables.
    ///
    /// A flag, not a generation compare: a reset that lands before the
    /// worker samples the generation would look like the worker's own
    /// session.
    session_retired: Arc<AtomicBool>,
    #[cfg(test)]
    parks: Arc<FsParks>,
}

/// Drop every request still queued and remove them from `inflight`,
/// because after a halt no one answers them.
fn abandon_queued(
    rx: &mpsc::Receiver<FsRequest>,
    inflight: &AtomicUsize,
    held: usize,
) {
    let mut count = held;
    while rx.try_recv().is_ok() {
        count += 1;
    }
    if count != 0 {
        inflight.fetch_sub(count, Ordering::AcqRel);
    }
}

fn fs_worker(rx: mpsc::Receiver<FsRequest>, ctx: FsWorkerCtx) {
    use std::sync::mpsc::RecvTimeoutError;

    loop {
        let req = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(r) => Some(r),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        ctx.gate.park_if_paused();
        if ctx.shutdown.load(Ordering::Acquire) {
            let held = usize::from(req.is_some());
            drop(req);
            abandon_queued(&rx, &ctx.inflight, held);
            return;
        }
        // Before the request is served, so a request from the new
        // session never sees the old session's nodeids and handles.
        reap_retired_session(&ctx);
        let Some(req) = req else { continue };

        serve_request(&ctx, &req);
        drop(req);
        ctx.inflight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Close the fd tables of a driver session that a reset ended.
///
/// The reset only marks the tables. Until `halt` joins this thread, it
/// is the only thread that closes a guest-held fd, so no syscall can be
/// running on a descriptor it closes. A number closed under a running
/// syscall goes to the next thread in the process that opens a file.
fn reap_retired_session(ctx: &FsWorkerCtx) {
    if ctx.session_retired.swap(false, Ordering::AcqRel) {
        ctx.server.passthrough().clear();
    }
}

/// Run one FUSE request and publish its reply.
///
/// Guest memory is touched twice, each time inside a session: the
/// request copy out, then the reply copy in with its used entry.
/// `MAX_MSG_SIZE` and the chain's writable window bound both copies, so
/// a reset never waits long.
///
/// `FuseServer::run` holds no session. It touches host memory only, so
/// a host filesystem that never answers stops only this device's I/O.
///
/// Nothing here may park on the quiesce gate. A worker parked inside a
/// session would wedge the reset of a paused device.
fn serve_request(ctx: &FsWorkerCtx, req: &FsRequest) {
    let call = {
        // A reset already invalidated this chain and its completion.
        // The check comes before the lock: a refused request must not
        // be able to queue on the lock a resetting vCPU waits for.
        let Some(_session) = ctx.access.enter(req.tag) else {
            return;
        };
        ctx.server.read_request(&req.bufs, &ctx.physmap)
    };

    #[cfg(test)]
    run_park(&ctx.parks.in_backend);
    let reply = ctx.server.run(&call);

    let raise = {
        // A reset landed while the backing store ran. The reply is
        // host memory, so dropping it here writes nothing into the
        // guest.
        let Some(_session) = ctx.access.enter(req.tag) else {
            return;
        };
        let len = ctx.server.write_reply(&req.bufs, &ctx.physmap, reply);
        req.completion.publish_batch(&[(req.head, len)])
    };
    // Delivery is an ioctl and can block, so it runs outside the
    // session. Otherwise the resetting vCPU would wait behind it.
    if raise {
        ctx.access.deliver(req.tag, || req.completion.signal());
    }
}

impl VirtioDevice for VirtioFs {
    fn device_features(&self) -> u64 {
        // VERSION_1 is added by the transport, and the transport strips
        // both of these on the legacy path.
        super::bits::VIRTIO_F_RING_INDIRECT_DESC
            | super::bits::VIRTIO_F_RING_EVENT_IDX
    }

    fn set_features(&self, features: u64) {
        // The modern transport calls this twice, once per 32-bit half, so
        // store the value and never act on it here.
        *self.negotiated_features.lock().expect("features lock") = features;
    }

    fn cfg_read(&self, offset: u16, len: u8) -> u32 {
        // The tag, then num_request_queues. notify_buf_size and
        // everything past the config read 0.
        let mut cfg = [0u8; FS_TAG_LEN + 4];
        cfg[..FS_TAG_LEN].copy_from_slice(&self.tag);
        cfg[FS_TAG_LEN..].copy_from_slice(&FS_NUM_REQUEST_QUEUES.to_le_bytes());
        crate::cfg_read_bytes(&cfg, offset, len)
    }

    fn cfg_write(&self, _offset: u16, _val: u32, _len: u8) {
        // virtio_fs_config is read-only.
    }

    fn process_queue(
        &self,
        _queue_idx: u16,
        _queue: &mut VirtQueue,
        _head: u16,
        _physmap: &PhysMap,
    ) -> u32 {
        // notify_queue is overridden; completion is asynchronous.
        0
    }

    /// The handler snapshots its ring, so a reprogrammed ring needs a new
    /// handler, and the requests built from the old ring retire with it.
    fn queue_addr_set(&self, queue_idx: u16, _queue: &VirtQueue) {
        self.retire_rings(Some(queue_idx));
    }

    /// Collect the guest's chains and hand them to the worker.
    ///
    /// The ring walk runs under one guest-access session, so a reset
    /// takes the ring back from it as from the worker. The session also
    /// supplies each request's generation tag, so no request carries a
    /// generation that ended while it was built.
    ///
    /// Returns whether the heads answered inline need an interrupt.
    /// The transport raises it after it drops its register lock.
    fn notify_queue(
        &self,
        queue_idx: u16,
        queues: &mut [VirtQueue],
        physmap: &PhysMap,
    ) -> bool {
        let Some(queue) = queues.get_mut(usize::from(queue_idx)) else {
            return false;
        };
        let Some(tx) = self.work_tx.lock().expect("work lock").clone() else {
            return false;
        };
        let Some(session) = self.access.enter_current(queue_idx) else {
            // A reset owns the ring. Its queue state goes with it.
            return false;
        };
        let completion = self.get_completion(&session, queue_idx, queue);
        let tag = session.tag();

        // A guest can keep advancing avail_idx during the EVENT_IDX
        // re-check. The counter lives outside the re-check loop so one
        // notification takes at most a queue's worth of work.
        let max_requests = usize::from(queue.size());
        // Heads the worker never answers, published in one used-ring
        // write at the end.
        let mut finished: Vec<(u16, u32)> = Vec::new();
        let mut popped = 0usize;

        loop {
            let before = queue.last_avail_idx();

            while popped < max_requests {
                let Some(head) = queue.pop_avail(physmap) else {
                    break;
                };
                popped += 1;
                let Some(bufs) = queue.collect_chain(physmap, head) else {
                    // A malformed chain completes with zero bytes so the
                    // ring index does not stall behind it.
                    finished.push((head, 0));
                    continue;
                };
                let req = FsRequest {
                    head,
                    bufs,
                    completion: Arc::clone(&completion),
                    tag,
                };
                // Count the request before the worker can receive it.
                self.inflight.fetch_add(1, Ordering::AcqRel);
                // Unlike virtio-blk, the send stays in the session: a
                // refused head must reach the used ring, and the queue
                // size caps the number of sends.
                if tx.send(req).is_err() {
                    self.inflight.fetch_sub(1, Ordering::AcqRel);
                    finished.push((head, 0));
                }
            }

            // Arm the kick on every exit path, the cap included. The
            // worker answers off the vCPU, so nothing else revisits this
            // ring. An unarmed `avail_event` at the cap tells the guest
            // to suppress its next kick, and the device is never
            // notified again. At the cap every descriptor is
            // outstanding, so the guest's next post is the index armed
            // here.
            let hit_request_cap = popped >= max_requests;
            queue.update_used_event(physmap);
            // The no-progress check stops a guest-triggered vCPU hang in
            // this re-check loop.
            if super::queue_drain_should_stop(
                queue,
                before,
                hit_request_cap,
                physmap,
            ) {
                break;
            }
        }

        let raise = completion.publish_batch(&finished);
        drop(session);

        // As in virtio-blk, the transport raises the interrupt for heads
        // answered here. A raise here runs under the transport's
        // register lock, which a reset on another vCPU must take before
        // it closes admission, so the guest could put an injection in
        // front of a reset. The worker raises its own replies off the
        // vCPU.
        raise
    }

    /// Reset the device, and return once nothing can write into the
    /// ring the driver is about to free.
    ///
    /// This runs on the vCPU that wrote DEVICE_STATUS, so it must be
    /// complete on return and short. The illumos legacy driver gives no
    /// grace: `virtio_legacy_device_reset_locked` is one register write
    /// with no poll, and `virtio_shutdown` reclaims the DMA buffers
    /// directly after it.
    ///
    /// Admission closes and every generation ends, which drops every
    /// request the worker has not started. The drain covers only the two
    /// bounded chain copies, so it ends on its own and needs no deadline.
    /// A request inside the backing store finishes into host memory,
    /// fails the generation check, and writes nothing.
    fn reset(&self) {
        *self.negotiated_features.lock().expect("features lock") = 0;
        // Only mark the fd tables: closing them here could pull a
        // descriptor from under a syscall the worker is in. The mark is
        // set before admission reopens, so no request of the next
        // session sees the old tables.
        self.session_retired.store(true, Ordering::Release);
        self.retire_rings(None);
    }
}

impl Lifecycle for VirtioFs {
    fn type_name(&self) -> &'static str {
        "virtio-fs"
    }

    fn lifecycle_state(&self) -> Option<IndicatedState> {
        Some(self.indicator.state())
    }

    fn start(&self) -> anyhow::Result<()> {
        self.indicator.start();
        Ok(())
    }

    fn pause(&self) {
        self.indicator.pause();
        self.gate.pause();
    }

    fn is_quiesced(&self) -> bool {
        // MUST NOT BLOCK: a plain atomic load. Joining happens in halt.
        self.gate.is_quiesced()
    }

    fn resume(&self) {
        self.gate.resume();
        self.indicator.resume();
    }

    /// The worker join in `halt` polls this deadline, so teardown must
    /// wait at least as long. Both read the same constant.
    fn halt_budget(&self) -> Duration {
        FS_HALT_BUDGET
    }

    fn halt(&self) {
        self.indicator.halt();
        self.shutdown.store(true, Ordering::Release);
        // Drop the sender so the worker's recv disconnects.
        *self.work_tx.lock().expect("work lock") = None;
        self.gate.resume();

        let mut worker_gone = true;
        if let Some(handle) = self.worker.lock().expect("worker lock").take() {
            let deadline = Instant::now() + FS_HALT_BUDGET;
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                worker_gone = false;
                slog::warn!(
                    self.log,
                    "virtio-fs worker did not exit within its budget"
                );
            }
        }

        // Drop never runs while the binary's device vectors hold the
        // Arc, so release guest-held fds here. A worker past its budget
        // can still use these fds, and a number closed under it can go
        // to an unrelated thread, so a leak is the lesser fault.
        if worker_gone {
            self.server.passthrough().clear();
        }
    }

    fn flush_backing(&self, intent: FlushIntent) -> Result<(), FlushError> {
        match intent {
            FlushIntent::Durable => self
                .server
                .passthrough()
                .sync_all()
                .map_err(FlushError::Sync),
            // A passthrough export writes through to host files with
            // pwrite; there is no device-level writeback cache to lose.
            FlushIntent::BestEffort => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests;
