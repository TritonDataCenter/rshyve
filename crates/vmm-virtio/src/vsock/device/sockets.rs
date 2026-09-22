// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The two per-connection tables: a host socket for each `ConnKey`, and
//! the threads that serve them.
//!
//! # Invariants
//!
//! A connection is named by its socket, not by its key. The guest picks
//! the key, so it can close a connection and reopen the same key while
//! an unlocked write or a sleeping reader still holds the old socket.
//! A caller that acts on a connection names the socket it served, and
//! [`VsockShared::release_socket`] refuses a key another connection
//! has taken.
//!
//! The reader table bounds the thread count. The mux connection cap
//! cannot: a peer that sends no CONNECT line holds a thread the mux
//! does not know about.

use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use slog::debug;
use vmm_core::unixsock::write_all_bounded;

use super::shared::{VsockShared, HOST_WRITE_TIMEOUT};
use crate::vsock::control::MAX_CONTROL_SESSIONS;
use crate::vsock::mux::MAX_CONNS;
use crate::vsock::packet::ConnKey;

impl VsockShared {
    /// Write payload to a connection's host socket.
    pub(super) fn write_host(&self, key: ConnKey, data: &[u8]) {
        let socket = self
            .sockets
            .lock()
            .expect("sockets lock")
            .get(&key)
            .cloned();
        let Some(stream) = socket else {
            // The mux still holds the connection, so the guest thinks it
            // is live. A silent drop leaves a hole in its stream.
            self.mux.queue_host_close(key);
            self.deliver_rx();
            return;
        };
        // The table stays unlocked for the write. A slow peer must not
        // hold the other vCPUs, or the halt sweep, behind it.
        if let Err(error) = write_all_bounded(&stream, data, HOST_WRITE_TIMEOUT)
        {
            debug!(self.log, "virtio-vsock host write gave up";
                "error" => %error);
            // To the guest, a peer that stopped reading for
            // HOST_WRITE_TIMEOUT is dead. Tell the guest here: the
            // reader cannot, because its socket is out of the table.
            if self.release_socket(key, &stream) {
                self.mux.queue_host_close(key);
                self.deliver_rx();
            }
        }
    }

    /// Take `key` out of the table and shut its socket down, so that
    /// this connection's reader wakes.
    ///
    /// Returns whether the entry still held this socket. Only then may
    /// the caller speak for the connection. A key another connection has
    /// taken, or one already released, returns false.
    pub(super) fn release_socket(
        &self,
        key: ConnKey,
        stream: &Arc<UnixStream>,
    ) -> bool {
        let mut sockets = self.sockets.lock().expect("sockets lock");
        let ours = sockets
            .get(&key)
            .is_some_and(|held| Arc::ptr_eq(held, stream));
        if ours {
            sockets.remove(&key);
        }
        drop(sockets);
        // The peer may already be gone.
        let _ = stream.shutdown(Shutdown::Both);
        ours
    }

    /// Put a host socket in the table, ready to be written to.
    ///
    /// The caller gives the returned handle to the reader thread, so at
    /// exit the reader can prove the table still holds its socket.
    pub(super) fn register_socket(
        &self,
        key: ConnKey,
        stream: UnixStream,
    ) -> Arc<UnixStream> {
        // Defence in depth only. On illumos `SO_SNDTIMEO` reports
        // success and does nothing, so `write_all_bounded` polls.
        if let Err(error) = stream.set_write_timeout(Some(HOST_WRITE_TIMEOUT)) {
            debug!(self.log, "virtio-vsock write timeout not set";
                "error" => %error);
        }
        let stream = Arc::new(stream);
        self.sockets
            .lock()
            .expect("sockets lock")
            .insert(key, Arc::clone(&stream));
        stream
    }

    pub(super) fn drop_socket(&self, key: ConnKey) {
        if let Some(stream) =
            self.sockets.lock().expect("sockets lock").remove(&key)
        {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    /// Every key the table holds now, for the halt sweep.
    pub(super) fn socket_keys(&self) -> Vec<ConnKey> {
        self.sockets
            .lock()
            .expect("sockets lock")
            .keys()
            .copied()
            .collect()
    }

    /// Take a slot for one more thread, or refuse when the table is
    /// full.
    ///
    /// The check and the reservation are one step, so the accept loop and
    /// guest connections share one bound and two callers cannot take the
    /// same free slot. Finished handles are dropped here, because
    /// otherwise only halt drains them, and a peer that connects and
    /// disconnects in a loop grows the table for the life of the VM.
    pub(super) fn reserve_reader(&self) -> Option<ReaderSlot<'_>> {
        let mut table = self.readers.lock().expect("reader lock");
        table.handles.retain(|h| !h.is_finished());
        if table.handles.len() + table.reserved >= MAX_CONNS {
            return None;
        }
        table.reserved += 1;
        Some(ReaderSlot {
            readers: &self.readers,
            filled: false,
        })
    }

    /// Take a place among the CONTROL sessions, or refuse.
    ///
    /// CONTROL shares the reader table with CONNECT, and a session may
    /// idle. Without a separate quota, a few idle CONTROL peers take
    /// every slot.
    pub(super) fn reserve_control_session(
        &self,
    ) -> Option<ControlSessionSlot<'_>> {
        let mut held = self.control_sessions.load(Ordering::Acquire);
        loop {
            if held >= MAX_CONTROL_SESSIONS {
                return None;
            }
            match self.control_sessions.compare_exchange_weak(
                held,
                held + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(ControlSessionSlot(&self.control_sessions))
                }
                Err(now) => held = now,
            }
        }
    }

    /// Record a thread in the reader table.
    ///
    /// Test only. Production reserves a slot first and fills it.
    #[cfg(test)]
    pub(super) fn track_reader(&self, handle: JoinHandle<()>) {
        self.reserve_reader().expect("a reader slot").fill(handle);
    }

    /// Reader threads the table still holds.
    #[cfg(test)]
    pub(super) fn reader_count(&self) -> usize {
        self.readers.lock().expect("reader lock").handles.len()
    }

    /// Take every reader handle, for the halt join.
    pub(super) fn take_readers(&self) -> Vec<JoinHandle<()>> {
        std::mem::take(&mut self.readers.lock().expect("reader lock").handles)
    }
}

/// One place in the CONTROL session quota.
pub(super) struct ControlSessionSlot<'a>(&'a AtomicUsize);

impl Drop for ControlSessionSlot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The reader threads and the slots promised to threads not yet
/// started.
#[derive(Default)]
pub(super) struct ReaderTable {
    handles: Vec<JoinHandle<()>>,
    reserved: usize,
}

/// A slot taken by [`VsockShared::reserve_reader`].
///
/// Either the thread fills it, or the drop gives the slot back, so a
/// failed spawn leaks no slot.
pub(super) struct ReaderSlot<'a> {
    readers: &'a Mutex<ReaderTable>,
    filled: bool,
}

impl ReaderSlot<'_> {
    pub(super) fn fill(mut self, handle: JoinHandle<()>) {
        let mut table = self.readers.lock().expect("reader lock");
        table.handles.push(handle);
        table.reserved -= 1;
        drop(table);
        self.filled = true;
    }
}

impl Drop for ReaderSlot<'_> {
    fn drop(&mut self) {
        if self.filled {
            return;
        }
        self.readers.lock().expect("reader lock").reserved -= 1;
    }
}
