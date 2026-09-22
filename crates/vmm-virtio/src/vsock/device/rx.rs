// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Host to guest: queued packets into the receive ring.
//!
//! # Invariant
//!
//! One pairing pass at a time. Two parallel passes can complete chains
//! in either order, and the guest reads the used ring in order, so an
//! RW byte stream would be reassembled wrong. A split packet adds a
//! second hazard: the packet behind the requeued tail can overtake it.

use std::sync::Arc;

use slog::debug;
use vmm_core::mem::PhysMap;

use super::shared::VsockShared;
use super::{VirtioVsock, VSOCK_RX_QUEUE};
use crate::pci::intr::IntrSession;
use crate::queue::{ChainBuf, VirtQueue, VirtioCompletion};
use crate::queue_drain_should_stop;
use crate::vsock::mux::RxPacket;
use crate::vsock::packet::HDR_SIZE;

impl VsockShared {
    /// Move queued packets into posted RX chains and publish them.
    pub(super) fn deliver_rx(&self) {
        // A paused device must not touch guest memory. Packets go out
        // on resume.
        if self.gate.is_paused() {
            return;
        }
        // Raise outside the ring permission: the raise is a call into
        // the kernel, and a reset must not wait on it. The handler names
        // its session, so the transport refuses a raise from before a
        // reset.
        if let Some(completion) = self.pair_rx() {
            completion.signal();
        }
    }

    /// Pair every queued packet with a posted chain, returning the
    /// handler that owes the guest an interrupt.
    fn pair_rx(&self) -> Option<Arc<VirtioCompletion>> {
        let _delivery = self.delivery.lock().expect("delivery lock");
        let mut raise = None;
        loop {
            if self.mux.rx_is_empty() {
                return raise;
            }
            // Enter per packet and read the handler inside: a reset
            // drains this section before it drops the handler, so the
            // handler found here belongs to the current driver.
            let Some(_session) = self.access.enter_current(VSOCK_RX_QUEUE)
            else {
                return raise;
            };
            let completion = {
                let guard =
                    self.rx_completion.lock().expect("rx completion lock");
                match guard.as_ref() {
                    Some(c) => Arc::clone(c),
                    None => return raise,
                }
            };
            let next = {
                let mut pending =
                    self.pending_rx.lock().expect("pending rx lock");
                pending.pop_front()
            };
            let Some((head, bufs)) = next else {
                return raise;
            };
            let Some(pkt) = self.mux.pop_rx() else {
                // Nothing to send: return the chain to the stash rather
                // than complete it empty.
                self.pending_rx
                    .lock()
                    .expect("pending rx lock")
                    .push_front((head, bufs));
                return raise;
            };

            let written = self.write_packet(&bufs, pkt);
            if completion.publish_batch(&[(head, written)]) {
                raise = Some(completion);
            }
        }
    }

    /// Write one packet into a chain, and split it when the chain is too
    /// small. RW carries a byte stream, so a split is legal. The rest
    /// goes back to the front of the queue.
    fn write_packet(&self, bufs: &[ChainBuf], mut pkt: RxPacket) -> u32 {
        let capacity: usize = bufs
            .iter()
            .filter_map(|b| match b {
                ChainBuf::Writable { len, .. } => Some(*len as usize),
                _ => None,
            })
            .sum();
        if capacity < HDR_SIZE {
            // Too small for a header. Complete the chain empty and keep
            // the packet for a larger chain.
            self.mux.requeue_rx_front(pkt);
            return 0;
        }

        let room = capacity - HDR_SIZE;
        if pkt.payload.len() > room {
            let rest = pkt.payload.split_off(room);
            let mut tail = pkt.clone();
            tail.payload = rest;
            tail.hdr.len = tail.payload.len() as u32;
            self.mux.requeue_rx_front(tail);
        }
        pkt.hdr.len = pkt.payload.len() as u32;

        let mut blob = Vec::with_capacity(HDR_SIZE + pkt.payload.len());
        blob.extend_from_slice(&pkt.hdr.to_bytes());
        blob.extend_from_slice(&pkt.payload);

        let mut written = 0usize;
        for buf in bufs {
            let ChainBuf::Writable { addr, len } = buf else {
                continue;
            };
            let remaining = blob.len() - written;
            if remaining == 0 {
                break;
            }
            let want = (*len as usize).min(remaining);
            let Some(sub) = self.physmap.lookup(*addr, want) else {
                break;
            };
            if sub.write_bytes(&blob[written..written + want]).is_err() {
                break;
            }
            written += want;
        }
        written as u32
    }
}

impl VirtioVsock {
    /// Get or create the completion handler for the RX ring.
    ///
    /// The session is read and the handler stored under one lock, and
    /// [`crate::VirtioDevice::reset`] drops the handler under the same
    /// lock after the transport ends its session. So the stored handler
    /// names either the ending session, and is dropped, or the next one.
    pub(super) fn ensure_rx_completion(
        &self,
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
        let session = self.shared.interrupt.next_session(IntrSession::INITIAL);
        let completion = VirtioCompletion::new(
            queue,
            Arc::clone(&self.shared.physmap),
            session,
            move |session| {
                intr.raise(session, VSOCK_RX_QUEUE);
            },
        );
        // Force the first completions through EVENT_IDX suppression so
        // an idle guest always wakes on the first packet.
        completion.set_force_interrupt(32);
        *slot = Some(Arc::clone(&completion));
        completion
    }

    /// Pop RX chains and hold them until a packet needs one.
    pub(super) fn stash_rx(&self, queue: &mut VirtQueue, physmap: &PhysMap) {
        let completion = self.ensure_rx_completion(queue);

        // A stashed chain completes only when a packet lands in it, so
        // the stash needs its own bound. Otherwise a guest can publish
        // one descriptor again and again and grow VMM memory.
        let max_requests = usize::from(queue.size());
        let mut popped = 0usize;
        loop {
            let before = queue.last_avail_idx();
            let mut refused: Vec<u16> = Vec::new();
            {
                let mut pending =
                    self.shared.pending_rx.lock().expect("pending rx lock");
                while popped < max_requests && pending.len() < max_requests {
                    let Some(head) = queue.pop_avail(physmap) else {
                        break;
                    };
                    popped += 1;
                    let Some(chain) = queue.collect_chain(physmap, head) else {
                        refused.push(head);
                        continue;
                    };
                    pending.push_back((head, chain));
                }
            }
            // `pop_avail` already took these heads, so each must go back
            // on the used ring. A lost head costs the guest a descriptor
            // for the life of the device.
            if !refused.is_empty() {
                debug!(self.shared.log, "virtio-vsock refused rx chains";
                    "count" => refused.len());
                for head in refused {
                    completion.complete(head, 0);
                }
            }
            // Arm the kick on every path out, the request cap included.
            // Only a guest kick refills the stash. Linux fills the ring
            // at probe, so the first pass hits the cap. With
            // `avail_event` unarmed the device is never kicked again,
            // spends its chains, and then holds every packet. New
            // connections hang too, because a RESPONSE needs a chain.
            let hit_request_cap = popped >= max_requests;
            queue.update_used_event(physmap);
            if queue_drain_should_stop(queue, before, hit_request_cap, physmap)
            {
                break;
            }
        }
        self.shared.deliver_rx();
    }
}
