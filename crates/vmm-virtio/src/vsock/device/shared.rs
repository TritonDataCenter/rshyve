// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The state the accept thread, the reader threads and the vCPUs all
//! reach.
//!
//! # Invariant
//!
//! No lock here is held across a wait on a host peer. A write clones
//! the socket handle and releases the table before it sends, and the
//! interrupt path takes the raise out of its slot before it calls it.
//! So a peer that stops reading holds only its own connection, never
//! the vCPUs or the halt sweep.

use std::collections::{HashMap, VecDeque};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vmm_core::mem::PhysMap;
use vmm_devices::QuiesceGate;

use crate::access::GuestAccess;
use crate::pci::intr::IntrSlot;
use crate::queue::{ChainBuf, VirtioCompletion};
use crate::vsock::control::ControlSlot;
use crate::vsock::mux::VsockMux;
use crate::vsock::packet::ConnKey;

use super::sockets::ReaderTable;

/// How long a write to a host peer may take before the device gives
/// up on it.
///
/// `write_host` runs on a vCPU thread with the transport lock held, so
/// an unbounded write stalls every vCPU that touches this device.
/// `write_all_bounded` enforces the bound, because `SO_SNDTIMEO`
/// reports success on illumos and bounds nothing.
pub(super) const HOST_WRITE_TIMEOUT: Duration = Duration::from_millis(500);

pub(super) struct VsockShared {
    pub(super) mux: VsockMux,
    pub(super) physmap: Arc<PhysMap>,
    /// Permission to write the driver's receive ring.
    ///
    /// Delivery runs on reader and accept threads, outside the
    /// transport's register lock. The driver frees the vring when
    /// `reset` returns, so a write that began before the reset must not
    /// land after it. A delivery holds this across the chain copy and
    /// the used entry. Nothing inside waits on a host peer, so the vCPU
    /// drain is bounded.
    pub(super) access: GuestAccess,
    /// RX chains the guest has posted, waiting for a packet.
    pub(super) pending_rx: Mutex<VecDeque<(u16, Vec<ChainBuf>)>>,
    pub(super) rx_completion: Mutex<Option<Arc<VirtioCompletion>>>,
    pub(super) interrupt: Arc<IntrSlot>,
    /// Serialises RX delivery. Held for a whole pairing pass.
    pub(super) delivery: Mutex<()>,
    /// Host socket per live connection. The `Arc` lets a write run with
    /// the table unlocked.
    pub(super) sockets: Mutex<HashMap<ConnKey, Arc<UnixStream>>>,
    /// Answers CONTROL requests. Empty until the binary installs a sink.
    pub(super) control: ControlSlot,
    /// CONTROL sessions running now, held to `MAX_CONTROL_SESSIONS`.
    pub(super) control_sessions: AtomicUsize,
    pub(super) socket_path: PathBuf,
    /// Threads serving one host connection each, and slots reserved for
    /// threads not yet started. Bounds the thread count for host peers
    /// and guest connections alike.
    pub(super) readers: Mutex<ReaderTable>,
    pub(super) shutdown: Arc<AtomicBool>,
    pub(super) gate: Arc<QuiesceGate>,
    pub(super) log: slog::Logger,
}
