// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VirtIO block device.
//!
//! One invariant holds the design together: a backing-store operation
//! owns only host memory, and every guest-memory access needs
//! permission for the ring generation it was authorised under, held
//! until that access finishes. So a reset closes admission, ends every
//! generation, waits for the short guest-access sections, and returns.
//! It never waits for the disk. A driver that programs one ring again
//! retires that ring's generation alone.
//!
//! Requests that reach the disk run on worker threads. Only the ones
//! that touch guest memory alone, such as GET_ID, run inline on the
//! vCPU.
//!
//! All completions go through `VirtioCompletion`, one Mutex-protected
//! used ring writer. Separate write indexes on the inline and async
//! paths corrupt the used index.

use std::fs::File;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use vmm_devices::blkdev::DiskCache;
use vmm_devices::lifecycle::Indicator;
use vmm_devices::{FlushError, FlushIntent, Lifecycle, QuiesceGate};

use vmm_core::mem::PhysMap;

#[usdt::provider(provider = "vmm")]
mod probes {
    fn vioblk_notify(queue_idx: u16, num_requests: u32) {}
    fn vioblk_inline(head: u16, rtype: u32, sector: u64) {}
    fn vioblk_dispatch(head: u16, rtype: u32, sector: u64) {}
    fn vioblk_complete(head: u16, status: u8) {}
    fn vioblk_reset_drain(queue_idx: u32, generation: u64) {}
}

use super::bits;
use super::queue::{ChainBuf, VirtQueue, VirtioCompletion};
use super::VirtioDevice;

mod access;
mod discard;
mod io;
#[cfg(test)]
mod tests;
mod worker;

use crate::access::{GuestAccess, RingTag, Session};
use crate::pci::intr::{BackendIntr, IntrSlot};
use access::BackendBarrier;
use io::{parse_header, write_status};
use worker::{blk_io_worker, needs_backend, BlkIoRequest, WorkerCtx};

/// Default I/O workers for a single-queue virtio-blk device.
///
/// This matches the propolis-server default. bhyve's 16 workers are one
/// pool for all disks. These workers are per disk, as in propolis. Eight
/// give deep queues enough parallel syscalls without contention on the
/// `VirtioCompletion` mutex.
const NUM_WORKERS: usize = 8;
/// Block config size from `capacity` through `write_zeroes_may_unmap`
/// (VirtIO 1.3 §5.2.4). It must match the `cfg_read` buffer, or the
/// Linux modern driver's `BUG_ON(offset + len > device_len)` fires.
pub const BLK_CONFIG_SIZE_EXT: u16 = 60;
const BLK_REQ_HEADER_SIZE: usize = 16;

/// Maximum size of any single segment (128K).
const SIZE_MAX: u32 = 128 * 1024;
/// Maximum number of segments in a single request.
const SEG_MAX: u32 = 126;
/// VirtIO block device serial ID length.
const VIRTIO_BLK_ID_BYTES: usize = 20;

#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct VirtioBlkReqHdr {
    rtype: u32,
    _reserved: u32,
    sector: u64,
}

#[derive(Debug, Clone)]
pub struct VirtioBlockOpts {
    pub read_only: bool,
    pub nodelete: bool,
    pub sector_size: u32,
    /// Number of virtqueues. `None` means "caller decides" (typically
    /// `min(num_vcpus, 4)`). Capped at 8: a zvol has one SPA sync
    /// pipeline, so more queues add worker threads and no throughput.
    pub num_queues: Option<u16>,
}

impl Default for VirtioBlockOpts {
    fn default() -> Self {
        Self {
            read_only: false,
            nodelete: false,
            sector_size: 512,
            num_queues: None,
        }
    }
}

/// Backing-store properties a request is checked against. Copied into
/// each I/O worker so a request can be bounded without the device handle.
#[derive(Debug, Clone, Copy)]
struct DiskLimits {
    capacity: u64,
    read_only: bool,
    nodelete: bool,
}

/// VirtIO block device.
pub struct VirtioBlock {
    capacity: u64,
    sector_size: u32,
    read_only: bool,
    /// Suppress DISCARD, matching bhyve's `nodelete` block-device option.
    nodelete: bool,
    num_queues: u16,
    workers_per_queue: usize,
    /// Indexed by `queue_idx * workers_per_queue + worker_idx`.
    io_txs: Vec<mpsc::Sender<BlkIoRequest>>,
    /// Per-queue round-robin counter for dispatching to workers.
    next_worker: Vec<AtomicUsize>,
    physmap: Arc<PhysMap>,
    /// The transport interrupt path, installed after the transport
    /// exists.
    interrupt: Arc<IntrSlot>,
    /// Per-queue completion handlers, created on first notification.
    completions: Mutex<Vec<Option<Arc<VirtioCompletion>>>>,
    /// Held for its lifetime: it owns the zvol write-cache setting.
    _cache: DiskCache,
    /// The backing store. Shared with every worker, so the descriptor
    /// outlives a device that is dropped while a syscall is running.
    /// A worker holding a bare fd number would resume against whatever
    /// the host reopened on it.
    file: Arc<File>,
    indicator: Indicator,
    gate: Arc<QuiesceGate>,
    /// Shared with every I/O worker.
    ctx: WorkerCtx,
}

impl VirtioBlock {
    /// Install the transport interrupt path once the transport exists.
    pub fn install_interrupt(&self, intr: Arc<BackendIntr>) {
        self.interrupt.install(intr);
    }

    pub fn new(
        file: File,
        opts: &VirtioBlockOpts,
        physmap: Arc<PhysMap>,
        num_queues: u16,
    ) -> std::io::Result<Self> {
        let metadata = file.metadata()?;
        // VirtIO 1.3 sec 5.2.4: capacity is in 512-byte sectors
        // whatever `blk_size` is, and sec 5.2.6 puts the request
        // sector in the same units. `sector_size` is the logical block
        // size the guest is told to align to, not the unit of either.
        let capacity = metadata.len() / bits::SECTOR_SIZE;
        let read_only = opts.read_only;
        let file = Arc::new(file);

        // Enable the zvol write-back cache (DKIOCSETWCE=1) before any
        // I/O, so the first guest writes already use the fast path.
        let cache = DiskCache::new(Arc::clone(&file), read_only);

        let limits = DiskLimits {
            capacity,
            read_only,
            nodelete: opts.nodelete,
        };
        let workers_per_queue = if num_queues <= 1 { NUM_WORKERS } else { 2 };
        let total_workers = num_queues as usize * workers_per_queue;
        let gate = Arc::new(QuiesceGate::new(total_workers));
        let ctx = WorkerCtx {
            physmap: Arc::clone(&physmap),
            gate: Arc::clone(&gate),
            access: Arc::new(GuestAccess::new(num_queues as usize)),
            backend: Arc::new(BackendBarrier::new()),
            #[cfg(test)]
            parks: Arc::new(worker::WorkerParks::default()),
        };
        let mut io_txs = Vec::with_capacity(total_workers);
        let mut next_worker = Vec::with_capacity(num_queues as usize);

        for q in 0..num_queues {
            for i in 0..workers_per_queue {
                let (tx, rx) = mpsc::channel::<BlkIoRequest>();
                let ctx = ctx.clone();
                let file = Arc::clone(&file);

                thread::Builder::new()
                    .name(format!("virtio-blk-q{q}-io-{i}"))
                    .spawn(move || {
                        blk_io_worker(rx, file, limits, ctx);
                    })
                    .expect("failed to spawn virtio-blk I/O worker");

                io_txs.push(tx);
            }
            next_worker.push(AtomicUsize::new(0));
        }

        let completions = (0..num_queues as usize).map(|_| None).collect();

        Ok(Self {
            capacity,
            sector_size: opts.sector_size,
            read_only: opts.read_only,
            nodelete: opts.nodelete,
            num_queues,
            workers_per_queue,
            io_txs,
            next_worker,
            physmap,
            interrupt: Arc::new(IntrSlot::new()),
            completions: Mutex::new(completions),
            _cache: cache,
            file,
            indicator: Indicator::new(),
            gate,
            ctx,
        })
    }

    pub fn num_queues(&self) -> u16 {
        self.num_queues
    }

    pub fn config_size(&self) -> u16 {
        BLK_CONFIG_SIZE_EXT
    }

    /// Return a device ID string for GET_ID requests.
    fn device_id(&self) -> [u8; VIRTIO_BLK_ID_BYTES] {
        let mut id = [0u8; VIRTIO_BLK_ID_BYTES];
        let s = b"vmm-virtio-blk";
        let n = s.len().min(VIRTIO_BLK_ID_BYTES);
        id[..n].copy_from_slice(&s[..n]);
        id
    }

    /// Return the queue's `VirtioCompletion`, and create it on first
    /// use.
    ///
    /// The caller's permission covers the used-index read here and the
    /// session stamped on the handler. A reset refuses permission for
    /// its whole duration, so neither can reach the slot the next
    /// driver finds.
    fn get_completion(
        &self,
        access: &Session<'_>,
        queue_idx: u16,
        queue: &VirtQueue,
    ) -> Arc<VirtioCompletion> {
        let mut slots = self.completions.lock().expect("completion lock");
        let idx = queue_idx as usize;
        if idx < slots.len() {
            if let Some(ref c) = slots[idx] {
                return Arc::clone(c);
            }
        }

        let intr = Arc::clone(&self.interrupt);
        // The handler carries the ring it belongs to. MSI-X gives each
        // virtqueue its own vector, and the guest driver only looks at
        // the ring that vector names, so a completion raised on any
        // other index is never collected.
        let interrupt_fn = move |session| {
            intr.raise(session, queue_idx);
        };

        let c = VirtioCompletion::new(
            queue,
            Arc::clone(&self.physmap),
            access.intr(),
            interrupt_fn,
        );
        // Force an interrupt for the first 32 completions. After a
        // migration, EVENT_IDX suppression can otherwise hide
        // completions from the guest and stall its disk I/O.
        c.set_force_interrupt(32);
        if idx < slots.len() {
            slots[idx] = Some(Arc::clone(&c));
        }
        c
    }

    /// End the generations that own the retired rings, wait for the
    /// guest-memory sections still inside them, and drop their
    /// used-ring writers.
    ///
    /// `queue` names the one ring a driver programmed again, or is
    /// `None` for a device reset. A reprogrammed queue ends only its own
    /// generation and writer. A request in flight on another queue
    /// names a ring the driver still owns, and discarding it leaves
    /// that request unanswered forever.
    ///
    /// Admission closes for every queue before the wait. A section
    /// refused before it takes the lock never queues on it.
    /// `std::sync::RwLock` gives the writer no priority, and the guest
    /// controls how much stale work waits, so the guest must not be
    /// able to starve this.
    ///
    /// This does not wait for the disk. A backend operation owns only
    /// host memory, so it is retired and finishes into buffers the next
    /// generation discards. The wait covers bounded copies and ring
    /// walks, so a vCPU can run it.
    fn retire_rings(&self, queue: Option<u16>) {
        match queue {
            Some(idx) => {
                let generation = self.ctx.access.close_queue(idx);
                probes::vioblk_reset_drain!(|| (u32::from(idx), generation));
            }
            None => {
                self.ctx.access.close_all();
                probes::vioblk_reset_drain!(|| (u32::MAX, 0));
            }
        }
        self.ctx.backend.retire();
        self.ctx.access.drain();

        {
            let mut slots = self.completions.lock().expect("lock");
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
        self.ctx.access.reopen(
            self.interrupt.next_session(self.ctx.access.intr_session()),
        );
    }

    /// Check one chain and return its request header.
    ///
    /// On refusal the status byte is written and the used length the
    /// guest is owed is returned. A refused head still goes back on the
    /// used ring: `pop_avail` has already taken it out of the available
    /// ring, and virtio-blk has no abort path to recover it.
    fn check_request(
        &self,
        chain: &[ChainBuf],
        physmap: &PhysMap,
    ) -> Result<VirtioBlkReqHdr, u32> {
        let refuse = |chain: &[ChainBuf]| {
            write_status(physmap, chain, bits::VIRTIO_BLK_S_IOERR)
        };
        if chain.len() < 2 {
            return Err(refuse(chain));
        }
        let Some(header) = parse_header(chain, physmap) else {
            return Err(refuse(chain));
        };
        // Segment count and segment size are the guest's to pick, so
        // the advertised limits hold only where they are checked.
        let Some(data_bytes) = io::span(chain) else {
            return Err(refuse(chain));
        };

        if header.rtype == bits::VIRTIO_BLK_T_IN
            || header.rtype == bits::VIRTIO_BLK_T_OUT
        {
            // A request with no data buffer would pass the sector range
            // check whatever sector it named, because 0 + 0 is never
            // past the capacity.
            if data_bytes == 0 {
                return Err(refuse(chain));
            }
            let sectors_needed =
                (data_bytes as u64).div_ceil(bits::SECTOR_SIZE);
            if header
                .sector
                .checked_add(sectors_needed)
                .is_none_or(|end| end > self.capacity)
            {
                return Err(refuse(chain));
            }
        }
        Ok(header)
    }

    /// Run a request that touches guest memory and nothing else.
    ///
    /// Returns (status, used_ring_len).
    fn local_request(
        &self,
        header: &VirtioBlkReqHdr,
        chain: &[ChainBuf],
        physmap: &PhysMap,
    ) -> (u8, u32) {
        match header.rtype {
            bits::VIRTIO_BLK_T_GET_ID => {
                // The data segment is the chain without its header and
                // status byte, the same span `io::fold` and
                // `discard::read_payload` use. The ID must not go into
                // the status descriptor: the used length would count
                // that byte twice, and `write_status` would overwrite
                // the first byte of the ID.
                let Some(data) = chain
                    .len()
                    .checked_sub(1)
                    .and_then(|end| chain.get(1..end))
                else {
                    return (bits::VIRTIO_BLK_S_IOERR, 1);
                };
                let id = self.device_id();
                for buf in data {
                    let ChainBuf::Writable { addr, len } = buf else {
                        continue;
                    };
                    // A driver may pad the chain, so an empty buffer is
                    // not the answer's buffer.
                    let n = (*len as usize).min(VIRTIO_BLK_ID_BYTES);
                    if n == 0 {
                        continue;
                    }
                    // Report a used length only for bytes the device
                    // wrote. Otherwise the guest reads stale buffer
                    // contents as the device ID.
                    let wrote = physmap
                        .lookup(*addr, n)
                        .is_some_and(|sub| sub.write_bytes(&id[..n]).is_ok());
                    return if wrote {
                        (bits::VIRTIO_BLK_S_OK, n as u32 + 1)
                    } else {
                        (bits::VIRTIO_BLK_S_IOERR, 1)
                    };
                }
                (bits::VIRTIO_BLK_S_IOERR, 1)
            }
            _ => (bits::VIRTIO_BLK_S_UNSUPP, 1),
        }
    }

    /// Hand a request to one of the queue's workers.
    fn dispatch(
        &self,
        tag: RingTag,
        completion: &Arc<VirtioCompletion>,
        head: u16,
        header: &VirtioBlkReqHdr,
        chain: Vec<ChainBuf>,
    ) {
        probes::vioblk_dispatch!(|| (head, header.rtype, header.sector));
        let queue_idx = usize::from(tag.queue_idx());
        let base = queue_idx * self.workers_per_queue;
        let idx = self.next_worker[queue_idx].fetch_add(1, Ordering::Relaxed)
            % self.workers_per_queue;
        // A closed channel means the worker is gone during shutdown.
        // The request carries its ring, so nothing else has to undo.
        let _ = self.io_txs[base + idx].send(BlkIoRequest {
            rtype: header.rtype,
            sector: header.sector,
            chain,
            head,
            completion: Arc::clone(completion),
            tag,
        });
    }
}

impl VirtioDevice for VirtioBlock {
    fn device_features(&self) -> u64 {
        let mut f = bits::VIRTIO_BLK_F_FLUSH
            | bits::VIRTIO_BLK_F_SIZE_MAX
            | bits::VIRTIO_BLK_F_SEG_MAX;
        if self.read_only {
            f |= bits::VIRTIO_BLK_F_RO;
        }
        if self.sector_size != 512 {
            f |= bits::VIRTIO_BLK_F_BLK_SIZE;
        }
        if !self.read_only {
            f |= bits::VIRTIO_BLK_F_WRITE_ZEROES;
            if !self.nodelete {
                f |= bits::VIRTIO_BLK_F_DISCARD;
            }
        }
        if self.num_queues > 1 {
            f |= bits::VIRTIO_BLK_F_MQ;
        }
        // The modern transport advertises the ring features. The legacy
        // transport strips them, because EVENT_IDX deadlocks a legacy
        // Linux 6.14 guest.
        f |= bits::VIRTIO_F_RING_EVENT_IDX;
        f |= bits::VIRTIO_F_RING_INDIRECT_DESC;
        f
    }

    fn set_features(&self, _features: u64) {
        // Each offered bit changes what the guest may ask for, not how
        // the device answers. The transport tells the queues what was
        // negotiated.
    }

    fn cfg_read(&self, offset: u16, len: u8) -> u32 {
        // Layout is `struct virtio_blk_config` (VirtIO 1.3 §5.2.4).
        let mut config = [0u8; 60];
        config[..8].copy_from_slice(&self.capacity.to_le_bytes());
        config[0x08..0x0C].copy_from_slice(&SIZE_MAX.to_le_bytes());
        config[0x0C..0x10].copy_from_slice(&SEG_MAX.to_le_bytes());
        // blk_size
        config[0x14..0x18].copy_from_slice(&self.sector_size.to_le_bytes());
        config[0x22..0x24].copy_from_slice(&self.num_queues.to_le_bytes());
        // DISCARD and WRITE_ZEROES limits (VirtIO 1.3 sec 5.2.4). Linux
        // reads a zero here as "no limit" (drivers/block/virtio_blk.c),
        // so every field a negotiated feature implies must be non-zero.
        if !self.read_only {
            if !self.nodelete {
                config[0x24..0x28].copy_from_slice(
                    &discard::MAX_DISCARD_SECTORS.to_le_bytes(),
                );
                config[0x28..0x2C]
                    .copy_from_slice(&discard::MAX_SEG.to_le_bytes());
                // Alignment is counted in 512-byte sectors.
                let align = u64::from(self.sector_size)
                    .div_ceil(bits::SECTOR_SIZE)
                    .max(1) as u32;
                config[0x2C..0x30].copy_from_slice(&align.to_le_bytes());
            }
            config[0x30..0x34].copy_from_slice(
                &discard::MAX_WRITE_ZEROES_SECTORS.to_le_bytes(),
            );
            config[0x34..0x38].copy_from_slice(&discard::MAX_SEG.to_le_bytes());
            // WRITE_ZEROES writes zeros and never deallocates.
            config[0x38] = 0;
        }
        crate::cfg_read_bytes(&config, offset, len)
    }

    fn cfg_write(&self, _: u16, _: u32, _: u8) {}

    /// Pop available chains, run or dispatch each one, and complete
    /// them.
    ///
    /// Every used-ring write goes through `VirtioCompletion`, never
    /// `VirtQueue::push_used`. Two paths that track the used index
    /// independently make it diverge.
    ///
    /// The body runs under one guest-access session and makes no
    /// backing-store call. A reset that waits for it waits only for
    /// ring walks and short copies, bounded by the queue size.
    ///
    /// The return value asks the transport to raise the interrupt. This
    /// runs under the transport's register lock, and a reset on another
    /// vCPU must take that lock to start. An injection here would put a
    /// kernel call on the reset's path, so the transport raises it after
    /// it drops the lock. Workers hold no transport lock and raise their
    /// own completions.
    fn notify_queue(
        &self,
        queue_idx: u16,
        queues: &mut [VirtQueue],
        physmap: &PhysMap,
    ) -> bool {
        let queue = match queues.get_mut(queue_idx as usize) {
            Some(q) => q,
            None => return false,
        };

        let Some(session) = self.ctx.access.enter_current(queue_idx) else {
            // A retirement owns the ring. Its queue state goes with it.
            return false;
        };
        let completion = self.get_completion(&session, queue_idx, queue);
        let tag = session.tag();

        // Bound the work retained by one notification to the queue size.
        let max_requests = usize::from(queue.size());
        let mut requests: Vec<(u16, VirtioBlkReqHdr, Vec<ChainBuf>)> =
            Vec::with_capacity(max_requests);
        // Results already finished, published together at the end.
        let mut finished: Vec<(u16, u32)> = Vec::new();

        queue.drain_avail(physmap, max_requests, |queue, head| {
            let chain = match queue.collect_chain(physmap, head) {
                Some(c) => c,
                None => {
                    // `pop_avail` already took this head off the
                    // available ring, so it must go on the used ring.
                    // A dropped head loses the descriptor for the life
                    // of the device: virtio-blk has no abort path.
                    finished.push((head, 0));
                    return;
                }
            };

            let header = match self.check_request(&chain, physmap) {
                Ok(header) => header,
                Err(len) => {
                    finished.push((head, len));
                    return;
                }
            };

            if needs_backend(header.rtype) {
                requests.push((head, header, chain));
                return;
            }

            probes::vioblk_inline!(|| (head, header.rtype, header.sector));
            let (status, written) =
                self.local_request(&header, &chain, physmap);
            write_status(physmap, &chain, status);
            probes::vioblk_complete!(|| (head, status));
            finished.push((head, written));
        });

        probes::vioblk_notify!(|| (
            queue_idx,
            (finished.len() + requests.len()) as u32
        ));

        let raise = completion.publish_batch(&finished);
        drop(session);

        for (head, header, chain) in requests {
            self.dispatch(tag, &completion, head, &header, chain);
        }

        raise
    }

    fn process_queue(
        &self,
        _: u16,
        _: &mut VirtQueue,
        _: u16,
        _: &PhysMap,
    ) -> u32 {
        0 // not called: notify_queue is overridden
    }

    /// The handler snapshots the ring it was built on, so a ring the
    /// driver programs again needs a new one. A worker still holding
    /// the old handler finishes into the old ring, which the driver
    /// gave up when it moved.
    fn queue_addr_set(&self, queue_idx: u16, _queue: &VirtQueue) {
        // A driver may program a ring again without a device reset.
        // Dropping the cached writer is not sufficient: a worker that
        // holds it can still publish into the old ring. So this queue's
        // generation ends too.
        self.retire_rings(Some(queue_idx));
    }

    /// Reset the device. Return only when no worker can write into the
    /// guest memory this ring used.
    ///
    /// The illumos driver treats one register write as the whole reset.
    /// `virtio_legacy_device_reset_locked` does not poll, and
    /// `virtio_main.c` states the contract: "when we reset the device,
    /// it should immediately stop using any DMA memory we have
    /// previously passed to it". `virtio_shutdown` then reclaims those
    /// buffers. So this is synchronous.
    ///
    /// Synchronous is safe because this waits neither for the disk nor
    /// for interrupt delivery. Admission closes, every generation ends,
    /// and the wait covers only guest-memory sections. Each is a
    /// bounded copy or ring walk, so the wait has no deadline.
    ///
    /// Ending every generation stops further raises for the old
    /// session. A raise already past that check is abandoned, not
    /// waited for: injection is a kernel call, and this runs on a vCPU.
    fn reset(&self) {
        self.retire_rings(None);
    }
}

impl Lifecycle for VirtioBlock {
    fn type_name(&self) -> &'static str {
        "virtio-blk"
    }

    fn lifecycle_state(
        &self,
    ) -> Option<vmm_devices::lifecycle::IndicatedState> {
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
        self.gate.is_quiesced()
    }

    fn resume(&self) {
        self.gate.resume();
        self.indicator.resume();
    }

    fn halt(&self) {
        self.indicator.halt();
    }

    fn flush_backing(&self, intent: FlushIntent) -> Result<(), FlushError> {
        vmm_devices::quiesce::flush_file(
            &self.gate,
            &self.file,
            intent,
            "virtio-blk",
        )
    }
}
