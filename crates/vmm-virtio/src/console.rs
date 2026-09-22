// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Single-port VirtIO console (device type 3).
//!
//! The host side is a Unix domain socket, one client at a time. The
//! guest sees one port and gets `/dev/hvc0`.
//!
//! No `VIRTIO_CONSOLE_F_MULTIPORT` and no `VIRTIO_CONSOLE_F_SIZE`: with
//! neither feature the control queues do not exist and the device needs
//! exactly two queues, which fits under the transport's four-queue cap.
//! The device config region is still advertised at its full spec size,
//! because the modern Linux driver checks `offset + len` against the
//! advertised length before it reads.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context;
use slog::{debug, warn};
use vmm_core::mem::PhysMap;
use vmm_core::unixsock::{bind_restricted, write_all_bounded, SocketPolicy};

use super::queue::{ChainBuf, VirtQueue, VirtioCompletion};
use super::VirtioDevice;
use crate::access::{GuestAccess, Session};
use crate::pci::intr::{BackendIntr, IntrSlot};
use crate::socket_accept;
use vmm_devices::lifecycle::{IndicatedState, Indicator};
use vmm_devices::{Lifecycle, QuiesceGate};

/// receiveq and transmitq. Under the transport's four-queue cap.
pub const CONSOLE_NUM_QUEUES: usize = 2;
/// receiveq: host to guest.
pub const CONSOLE_RX_QUEUE: u16 = 0;
/// transmitq: guest to host.
pub const CONSOLE_TX_QUEUE: u16 = 1;
/// Must be a power of two: `VirtQueue::new` panics otherwise.
pub const CONSOLE_QUEUE_SIZE: u16 = 128;
/// Full `virtio_console_config`: cols, rows, max_nr_ports, emerg_wr.
///
/// Every field reads zero, but the whole struct is advertised because
/// the modern Linux driver bounds-checks against this length.
pub const CONSOLE_CONFIG_SIZE: u16 = 12;
/// One vector per queue plus one for config change.
pub const CONSOLE_MSIX_VECTORS: u16 = 3;
/// Host bytes buffered while the guest has posted no RX chain.
///
/// A cap is required: without one a fast host writer grows VMM memory
/// without limit while the guest is not reading.
pub const CONSOLE_RX_BACKLOG_MAX: usize = 64 * 1024;

/// Guest output is copied through a fixed buffer, so a descriptor
/// length the guest picked never becomes an allocation size.
const TX_CHUNK: usize = 4096;
/// Bytes of guest output one kick may copy.
///
/// The drain runs inline on the vCPU under the transport lock, and the
/// guest picks the descriptor lengths and the number of chains. A ring
/// of 128 chains, each with 128 descriptors of 4 GiB, is tens of
/// terabytes to copy for one notification. The vCPU stays in the VMM,
/// every vCPU that touches this device's registers waits on the lock,
/// and a pause or teardown that needs that vCPU waits too. Output past
/// the bound is dropped, as it is when no client is attached.
const TX_BYTES_PER_KICK: usize = 1024 * 1024;
/// How long a client may stall before it loses its console.
///
/// The transmit queue drains on the vCPU thread, so an unbounded write
/// would let an idle terminal hold a guest CPU. `write_all_bounded`
/// supplies the end: `SO_SNDTIMEO` reports success on illumos and
/// bounds nothing.
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_millis(250);
/// State shared with the accept and reader threads.
struct ConsoleShared {
    socket_path: PathBuf,
    physmap: Arc<PhysMap>,
    /// The attached client, if any. TX writes go here. Held behind an
    /// `Arc` so a write does not need the slot locked while it runs.
    client: Mutex<Option<Arc<UnixStream>>>,
    /// Host bytes with no guest RX chain to land in yet.
    backlog: Mutex<VecDeque<u8>>,
    /// Guest RX chains with no host bytes to fill them yet.
    pending_rx: Mutex<VecDeque<(u16, Vec<ChainBuf>)>>,
    rx_completion: Mutex<Option<Arc<VirtioCompletion>>>,
    /// Permission to touch the guest's ring and chains.
    ///
    /// The reader thread delivers on its own, so a reset has to take
    /// the ring back from it rather than from a worker it dispatched.
    access: Arc<GuestAccess>,
    interrupt: Arc<IntrSlot>,
    /// Reader threads, one per client that has been attached.
    readers: Mutex<Vec<JoinHandle<()>>>,
    shutdown: AtomicBool,
    gate: Arc<QuiesceGate>,
    /// Set while the backlog is over its cap, so the warning is emitted
    /// once per overflow burst rather than once per read.
    overflowed: AtomicBool,
    log: slog::Logger,
}

impl ConsoleShared {
    /// Drop the used-ring writer and the stashed chains. Chains are
    /// dropped, not completed: the guest abandons its ring and reposts.
    fn forget_rx_ring(&self) {
        *self.rx_completion.lock().expect("rx completion lock") = None;
        self.pending_rx.lock().expect("pending rx lock").clear();
    }

    /// Move host bytes into posted guest RX chains and publish them.
    fn deliver_rx(&self) {
        // A paused device must not touch guest memory. Bytes stay in
        // the backlog and are delivered on resume.
        if self.gate.is_paused() {
            return;
        }
        // Held across every copy and used entry below. The transport
        // publishes status 0 as soon as `reset` returns, and the illumos
        // driver then reclaims the ring DMA at once. So a delivery that
        // started before the reset must finish inside it. The stash and
        // the backlog cap bound this work, so a vCPU can wait for it.
        let Some(_session) = self.access.enter_current(CONSOLE_RX_QUEUE) else {
            return;
        };
        let completion = {
            let guard = self.rx_completion.lock().expect("rx completion lock");
            match guard.as_ref() {
                Some(c) => Arc::clone(c),
                None => return,
            }
        };

        loop {
            let mut backlog = self.backlog.lock().expect("backlog lock");
            if backlog.is_empty() {
                return;
            }
            let next = {
                let mut pending =
                    self.pending_rx.lock().expect("pending rx lock");
                pending.pop_front()
            };
            let Some((head, bufs)) = next else {
                return;
            };

            let mut written = 0u32;
            for buf in &bufs {
                let ChainBuf::Writable { addr, len } = buf else {
                    continue;
                };
                let want = (*len as usize).min(backlog.len());
                if want == 0 {
                    break;
                }
                // Look the buffer up before the bytes leave the
                // backlog, so an unmapped chain does not eat them.
                let Some(sub) = self.physmap.lookup(*addr, want) else {
                    break;
                };
                let chunk: Vec<u8> = backlog.drain(..want).collect();
                if sub.write_bytes(&chunk).is_err() {
                    break;
                }
                written = written.saturating_add(want as u32);
            }
            drop(backlog);
            completion.complete(head, written);
        }
    }

    /// Write guest output to the attached client, if there is one.
    fn write_host(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let client = self.client.lock().expect("client lock").clone();
        let Some(stream) = client else {
            // No client attached. Console output is best-effort.
            return;
        };
        // The slot stays unlocked for the write. A terminal that
        // stopped reading must not hold the vCPU draining the
        // transmit queue, or every thread that needs the slot.
        if let Err(error) =
            write_all_bounded(&stream, data, CLIENT_WRITE_TIMEOUT)
        {
            warn!(self.log, "virtio-console client write failed";
                "error" => %error);
            self.detach_client(&stream);
        }
    }

    /// Drop `stream` as the client and shut it down, so that its
    /// reader thread wakes and returns.
    ///
    /// The slot is cleared only while `stream` still holds it: a newer
    /// client may have taken it over during the write that failed.
    fn detach_client(&self, stream: &Arc<UnixStream>) {
        let mut guard = self.client.lock().expect("client lock");
        if guard.as_ref().is_some_and(|held| Arc::ptr_eq(held, stream)) {
            guard.take();
        }
        drop(guard);
        // The peer may already be gone. That needs no action.
        let _ = stream.shutdown(Shutdown::Both);
    }

    /// Install `stream` as the client and shut down the one it
    /// replaces, so that its reader thread wakes and returns.
    fn replace_client(&self, stream: Option<UnixStream>) {
        let previous = std::mem::replace(
            &mut *self.client.lock().expect("client lock"),
            stream.map(Arc::new),
        );
        if let Some(old) = previous {
            // The peer may already be gone. That needs no action.
            let _ = old.shutdown(Shutdown::Both);
        }
    }

    /// Join the readers whose clients have gone, so a long-lived
    /// console does not accumulate thread handles.
    fn reap_readers(&self) {
        let done = {
            let mut readers = self.readers.lock().expect("reader lock");
            let (done, alive): (Vec<_>, Vec<_>) =
                readers.drain(..).partition(JoinHandle::is_finished);
            *readers = alive;
            done
        };
        for handle in done {
            // A panicked reader must not abort the accept loop.
            let _ = handle.join();
        }
    }

    /// Take over as the single client and start reading from it.
    fn attach_client(self: &Arc<Self>, stream: UnixStream) {
        // A secondary bound only. It works on other platforms. On
        // illumos it reports success and does nothing, so
        // `write_all_bounded` polls instead.
        if let Err(error) = stream.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT))
        {
            warn!(self.log, "virtio-console write timeout not set";
                "error" => %error);
        }
        let reader = match stream.try_clone() {
            Ok(r) => r,
            Err(error) => {
                warn!(self.log, "virtio-console client clone failed";
                    "error" => %error);
                return;
            }
        };

        self.replace_client(Some(stream));
        self.reap_readers();

        let worker = Arc::clone(self);
        match std::thread::Builder::new()
            .name("virtio-console-rx".into())
            .spawn(move || reader_loop(worker, reader))
        {
            Ok(handle) => {
                self.readers.lock().expect("reader lock").push(handle)
            }
            Err(error) => {
                warn!(self.log, "virtio-console reader spawn failed";
                    "error" => %error);
                self.replace_client(None);
            }
        }
    }
}

fn accept_loop(shared: Arc<ConsoleShared>, listener: UnixListener) {
    let on_client = |stream| shared.attach_client(stream);
    socket_accept::accept_loop(
        listener,
        &shared.shutdown,
        &shared.log,
        "virtio-console",
        on_client,
    );
}

fn reader_loop(shared: Arc<ConsoleShared>, mut stream: UnixStream) {
    let mut buf = [0u8; 4096];
    loop {
        if shared.shutdown.load(Ordering::Acquire) {
            break;
        }
        let n = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                continue
            }
            Err(_) => break,
        };
        {
            let mut backlog = shared.backlog.lock().expect("backlog lock");
            let room = CONSOLE_RX_BACKLOG_MAX.saturating_sub(backlog.len());
            let take = n.min(room);
            if take < n {
                if !shared.overflowed.swap(true, Ordering::Relaxed) {
                    warn!(shared.log,
                        "virtio-console RX backlog full, dropping input";
                        "dropped" => n.saturating_sub(take));
                }
            } else {
                shared.overflowed.store(false, Ordering::Relaxed);
            }
            backlog.extend(&buf[..take]);
        }
        shared.deliver_rx();
    }
    // The client slot is not cleared here: a newer client may already
    // own it, and the write path drops a dead one on its own.
}

/// Single-port VirtIO console backed by a Unix domain socket.
pub struct VirtioConsole {
    shared: Arc<ConsoleShared>,
    accept: Mutex<Option<JoinHandle<()>>>,
    indicator: Indicator,
}

impl VirtioConsole {
    /// Bind the host socket and start accepting clients.
    ///
    /// Returns by value: `VirtioPciDevice::new` takes the backend by
    /// value, so clone [`interrupt`](Self::interrupt) and read
    /// [`config_size`](Self::config_size) before the move.
    pub fn new(
        socket_path: &Path,
        physmap: Arc<PhysMap>,
        log: slog::Logger,
    ) -> anyhow::Result<Self> {
        // 0600 socket in a 0700 directory, same policy as the control
        // socket. Do not use a bare UnixListener::bind here.
        let listener = bind_restricted(socket_path, SocketPolicy::default())
            .with_context(|| {
                format!("bind virtio-console socket {}", socket_path.display())
            })?;

        // Zero registered workers: the accept and reader threads block
        // on the socket, outside the gate, and a console holds no
        // durable in-flight work. The gate's job here is `is_paused`.
        let shared = Arc::new(ConsoleShared {
            socket_path: socket_path.to_path_buf(),
            physmap,
            client: Mutex::new(None),
            backlog: Mutex::new(VecDeque::new()),
            pending_rx: Mutex::new(VecDeque::new()),
            rx_completion: Mutex::new(None),
            access: Arc::new(GuestAccess::new(CONSOLE_NUM_QUEUES)),
            interrupt: Arc::new(IntrSlot::new()),
            readers: Mutex::new(Vec::new()),
            shutdown: AtomicBool::new(false),
            gate: Arc::new(QuiesceGate::new(0)),
            overflowed: AtomicBool::new(false),
            log,
        });

        let accept_shared = Arc::clone(&shared);
        let accept = std::thread::Builder::new()
            .name("virtio-console-accept".into())
            .spawn(move || accept_loop(accept_shared, listener))
            .context("spawn virtio-console accept thread")?;

        Ok(Self {
            shared,
            accept: Mutex::new(Some(accept)),
            indicator: Indicator::new(),
        })
    }

    /// Install the transport interrupt path once the transport exists.
    pub fn install_interrupt(&self, intr: Arc<BackendIntr>) {
        self.shared.interrupt.install(intr);
    }

    pub const fn config_size(&self) -> u16 {
        CONSOLE_CONFIG_SIZE
    }

    pub fn socket_path(&self) -> &Path {
        &self.shared.socket_path
    }

    /// Get or create the completion handler for the RX ring.
    ///
    /// The caller's permission stamps the handler with the session its
    /// ring belongs to. Permission is refused for the whole of a
    /// reset, so a handler built here cannot reach the slot the next
    /// driver finds.
    fn ensure_rx_completion(
        &self,
        access: &Session<'_>,
        queue: &VirtQueue,
    ) -> Arc<VirtioCompletion> {
        let mut slot = self
            .shared
            .rx_completion
            .lock()
            .expect("rx completion lock");
        if let Some(completion) = slot.as_ref() {
            return Arc::clone(completion);
        }
        let intr = Arc::clone(&self.shared.interrupt);
        let completion = VirtioCompletion::new(
            queue,
            Arc::clone(&self.shared.physmap),
            access.intr(),
            move |session| {
                intr.raise(session, CONSOLE_RX_QUEUE);
            },
        );
        // Force the first completions through EVENT_IDX suppression so
        // an idle guest console always wakes on its first input.
        completion.set_force_interrupt(32);
        *slot = Some(Arc::clone(&completion));
        completion
    }

    /// Pop RX chains and hold them until host bytes arrive.
    fn stash_rx(&self, queue: &mut VirtQueue, physmap: &PhysMap) {
        let Some(session) = self.shared.access.enter_current(CONSOLE_RX_QUEUE)
        else {
            // A reset owns the ring. Its queue state goes with it.
            return;
        };
        let completion = self.ensure_rx_completion(&session, queue);

        // A stashed chain completes only when host bytes land in it, so
        // the stash needs its own bound. Otherwise a guest can publish
        // one descriptor again and again and grow VMM memory without
        // limit.
        let max_requests = usize::from(queue.size());
        let mut refused: Vec<u16> = Vec::new();
        let mut pending =
            self.shared.pending_rx.lock().expect("pending rx lock");
        queue.drain_avail(physmap, max_requests, |queue, head| {
            if pending.len() >= max_requests {
                refused.push(head);
                return;
            }
            match queue.collect_chain(physmap, head) {
                Some(chain) => pending.push_back((head, chain)),
                None => refused.push(head),
            }
        });
        drop(pending);
        // `pop_avail` already took these heads off the available ring,
        // so each must go on the used ring. A lost head costs the guest
        // a descriptor for the life of the device, and no abort path
        // recovers it.
        if !refused.is_empty() {
            debug!(self.shared.log, "virtio-console refused rx chains";
                "count" => refused.len());
            for head in refused {
                completion.complete(head, 0);
            }
        }

        // Dropped first: `deliver_rx` takes its own permission, and a
        // second read lock behind a waiting writer deadlocks.
        drop(session);
        self.shared.deliver_rx();
    }

    /// End the session that owns the RX ring, wait for the deliveries
    /// already inside it, and drop everything that named that ring.
    ///
    /// Admission closes before the wait, so a refused delivery never
    /// takes the lock. `std::sync::RwLock` gives the writer no
    /// priority. Without this, a fast host peer could starve a vCPU
    /// that waits here.
    fn retire_rx_ring(&self) {
        // The RX ring is the only one a delivery ever writes, so its
        // generation is the whole of what this takes back.
        self.shared.access.close_queue(CONSOLE_RX_QUEUE);
        self.shared.access.drain();
        self.shared.forget_rx_ring();
        self.shared.access.reopen(
            self.shared
                .interrupt
                .next_session(self.shared.access.intr_session()),
        );
    }

    /// Drain guest output inline on the vCPU thread.
    fn drain_tx(&self, queue: &mut VirtQueue, physmap: &PhysMap) -> bool {
        let old_used_idx = queue.read_used_idx(physmap);
        let max_requests = usize::from(queue.size());
        let mut chunk = [0u8; TX_CHUNK];
        let mut budget = TX_BYTES_PER_KICK;

        let popped = queue.drain_avail(physmap, max_requests, |queue, head| {
            if let Some(chain) = queue.collect_chain(physmap, head) {
                for buf in &chain {
                    let ChainBuf::Readable { addr, len } = buf else {
                        continue;
                    };
                    self.write_chain_buf(
                        physmap,
                        *addr,
                        *len,
                        &mut chunk,
                        &mut budget,
                    );
                }
            }
            // Heads go on the used ring after the budget is spent too.
            // The ring must drain, and only the copying is bounded.
            queue.push_used(physmap, head, 0);
        });

        popped > 0 && queue.should_notify_guest(physmap, old_used_idx)
    }

    /// Copy one device-readable buffer out to the client, in chunks,
    /// spending `budget` as it goes.
    fn write_chain_buf(
        &self,
        physmap: &PhysMap,
        addr: u64,
        len: u32,
        chunk: &mut [u8; TX_CHUNK],
        budget: &mut usize,
    ) {
        let mut remaining = (len as usize).min(*budget);
        let mut offset = 0u64;
        while remaining > 0 {
            let n = remaining.min(TX_CHUNK);
            let Some(gpa) = addr.checked_add(offset) else {
                return;
            };
            let Some(sub) = physmap.lookup(gpa, n) else {
                return;
            };
            if sub.read_bytes(&mut chunk[..n]).is_err() {
                return;
            }
            self.shared.write_host(&chunk[..n]);
            *budget -= n;
            offset = offset.saturating_add(n as u64);
            remaining = remaining.saturating_sub(n);
        }
    }
}

impl VirtioDevice for VirtioConsole {
    fn device_features(&self) -> u64 {
        // Ring features only. The transport adds VERSION_1 and strips
        // these two on the legacy path.
        super::bits::VIRTIO_F_RING_EVENT_IDX
            | super::bits::VIRTIO_F_RING_INDIRECT_DESC
    }

    fn set_features(&self, _features: u64) {
        // The console reads no feature bit of its own. The transport
        // tells the queues what was negotiated.
    }

    fn cfg_read(&self, _offset: u16, _len: u8) -> u32 {
        // cols, rows, max_nr_ports and emerg_wr all read zero.
        0
    }

    fn cfg_write(&self, _offset: u16, _val: u32, _len: u8) {}

    fn process_queue(
        &self,
        _queue_idx: u16,
        _queue: &mut VirtQueue,
        _head: u16,
        _physmap: &PhysMap,
    ) -> u32 {
        0 // not called: notify_queue is overridden
    }

    fn notify_queue(
        &self,
        queue_idx: u16,
        queues: &mut [VirtQueue],
        physmap: &PhysMap,
    ) -> bool {
        let Some(queue) = queues.get_mut(queue_idx as usize) else {
            return false;
        };
        match queue_idx {
            CONSOLE_RX_QUEUE => {
                self.stash_rx(queue, physmap);
                // The completion handler raises the interrupt later.
                false
            }
            CONSOLE_TX_QUEUE => self.drain_tx(queue, physmap),
            _ => false,
        }
    }

    /// The handler and the stashed chains both name the ring they
    /// came from, so a ring the driver programs again starts empty.
    fn queue_addr_set(&self, queue_idx: u16, _queue: &VirtQueue) {
        if queue_idx == CONSOLE_RX_QUEUE {
            self.retire_rx_ring();
        }
    }

    fn reset(&self) {
        self.retire_rx_ring();
        // The host backlog survives a guest reset. Those bytes came from
        // the client and are still owed to the guest.
    }
}

impl Lifecycle for VirtioConsole {
    fn type_name(&self) -> &'static str {
        "virtio-console"
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
        self.shared.gate.pause();
    }

    fn is_quiesced(&self) -> bool {
        // This does not block. `halt` joins the threads.
        self.shared.gate.is_quiesced()
    }

    fn resume(&self) {
        self.shared.gate.resume();
        self.indicator.resume();
        // Bytes buffered during the pause are owed to the guest.
        self.shared.deliver_rx();
    }

    /// The halt joins the accept thread for one budget, then the reader
    /// threads for another, so its real deadline is twice the constant.
    fn halt_budget(&self) -> Duration {
        crate::socket_halt::halt_budget()
    }

    fn halt(&self) {
        self.indicator.halt();
        self.shared.shutdown.store(true, Ordering::Release);
        let accept = self.accept.lock().expect("accept lock").take();
        crate::socket_halt::halt_socket_device(
            &self.shared.log,
            "virtio-console",
            accept,
            || {
                self.shared.replace_client(None);
                self.shared
                    .readers
                    .lock()
                    .expect("reader lock")
                    .drain(..)
                    .collect()
            },
            &self.shared.socket_path,
        );
    }
}

#[cfg(test)]
mod tests;
