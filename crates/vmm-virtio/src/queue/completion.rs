// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Used ring completion for asynchronous virtio devices.

use std::sync::atomic::{fence, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use vmm_core::mem::PhysMap;

use super::{bits, vring_need_event, VirtQueue};
use crate::pci::intr::IntrSession;

/// Thread-safe used ring writer for async virtio I/O, one per queue.
///
/// It owns the used ring write index of its virtqueue. Every
/// completion, inline or from a worker, must go through it. Do not mix
/// it with `VirtQueue::push_used()`.
///
/// This is the Propolis `Mutex<VqUsed>` pattern:
/// - The `Mutex<u16>` serializes used ring writes, so each entry is
///   complete before the index moves.
/// - A release fence before the index write makes the guest see the
///   entry before the new index.
/// - The interrupt fires after the Mutex is released.
pub struct VirtioCompletion {
    used_addr: u64,
    avail_addr: u64,
    queue_size: u16,
    write_idx: Mutex<u16>,
    physmap: Arc<PhysMap>,
    /// The transport session this handler was built in.
    ///
    /// A device drops its handlers on reset and builds new ones in the
    /// next session. So every used entry this handler publishes belongs
    /// to this session. [`Self::signal`] gives it to the transport,
    /// which refuses a raise for a session that ended.
    session: IntrSession,
    interrupt: Box<dyn Fn(IntrSession) + Send + Sync>,
    /// Whether VIRTIO_F_RING_EVENT_IDX is negotiated.
    event_idx: bool,
    /// The number of completions that still interrupt without
    /// condition. Migration sets it so the guest processes completions
    /// that EVENT_IDX would suppress.
    force_intr_count: AtomicU32,
}

impl VirtioCompletion {
    /// Create a completion handler for a configured virtqueue.
    ///
    /// The start position is the used index in guest memory. From then
    /// on, only this handler writes the used ring index.
    pub fn new(
        queue: &VirtQueue,
        physmap: Arc<PhysMap>,
        session: IntrSession,
        interrupt: impl Fn(IntrSession) + Send + Sync + 'static,
    ) -> Arc<Self> {
        let (used_addr, queue_size) = queue.used_ring_info();
        let avail_addr = queue.avail_addr();
        let idx_gpa = used_addr + 2;
        let initial_idx = physmap
            .lookup(idx_gpa, 2)
            .and_then(|sub| sub.read::<u16>().ok())
            .unwrap_or(0);

        let event_idx = queue.event_idx_enabled();
        Arc::new(Self {
            used_addr,
            avail_addr,
            queue_size,
            write_idx: Mutex::new(initial_idx),
            physmap,
            session,
            interrupt: Box::new(interrupt),
            event_idx,
            force_intr_count: AtomicU32::new(0),
        })
    }

    /// Complete one request and raise the interrupt if the guest wants
    /// one. Any thread can call it.
    #[inline]
    pub fn complete(&self, desc_idx: u16, len: u32) {
        if self.publish_batch(&[(desc_idx, len)]) {
            self.signal();
        }
    }

    /// Publish used entries and report whether the guest wants an
    /// interrupt.
    ///
    /// Every step touches guest memory: the used entries, the used
    /// index and the driver's suppression fields. A device that guards
    /// guest access must hold that permission across the whole call,
    /// and must call [`Self::signal`] outside it.
    ///
    /// The Mutex is held across all writes, so no other thread can
    /// interleave entries.
    pub fn publish_batch(&self, entries: &[(u16, u32)]) -> bool {
        if entries.is_empty() {
            return false;
        }
        let old_idx;
        let new_idx;
        {
            let mut idx = self.write_idx.lock().expect("completion lock");
            old_idx = *idx;
            for &(desc_idx, len) in entries {
                self.write_used_entry(&mut idx, desc_idx, len);
            }
            new_idx = *idx;
        }
        self.should_interrupt(old_idx, new_idx)
    }

    /// Raise the interrupt a publish asked for.
    ///
    /// It is separate from the publish so a device can deliver outside
    /// the permission that wrote the ring. Delivery can block, and
    /// VirtIO 1.3 sec 2.4.1 forbids queue interaction after a reset
    /// completes. So the raise names the session of this handler, not
    /// the current one. The transport refuses a thread that stalled
    /// here across a whole reset, and the next driver gets no stray
    /// interrupt.
    #[inline]
    pub fn signal(&self) {
        (self.interrupt)(self.session);
    }

    /// Make the next `count` completions interrupt without EVENT_IDX
    /// suppression. Migration uses it so the guest processes stale
    /// used entries.
    pub fn set_force_interrupt(&self, count: u32) {
        self.force_intr_count.store(count, Ordering::Release);
    }

    /// Whether the guest needs an interrupt.
    ///
    /// With EVENT_IDX, interrupt only when the move from `old_idx` to
    /// `new_idx` crosses the guest's `used_event` (VirtIO 1.3 sec
    /// 2.7.7.1). Without EVENT_IDX, interrupt unless the guest set
    /// VIRTQ_AVAIL_F_NO_INTERRUPT.
    fn should_interrupt(&self, old_idx: u16, new_idx: u16) -> bool {
        // One read-modify-write claims a forced interrupt. Two threads
        // that both read the last count and both subtract it wrap the
        // counter to u32::MAX, and every later completion interrupts.
        let forced = self
            .force_intr_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_sub(1)
            })
            .is_ok();
        if forced {
            return true;
        }
        if !self.event_idx {
            if self.avail_addr != 0 {
                if let Some(sub) = self.physmap.lookup(self.avail_addr, 2) {
                    if let Ok(flags) = sub.read::<u16>() {
                        return flags & bits::VIRTQ_AVAIL_F_NO_INTERRUPT == 0;
                    }
                }
            }
            return true;
        }

        // The used_idx write in write_used_entry must be visible to the
        // guest before the read of used_event. A store-load reorder
        // reads a stale used_event and skips an interrupt the guest
        // expects (QEMU smp_mb() in virtio_split_should_notify()).
        fence(Ordering::SeqCst);

        // Layout: flags(2) + idx(2) + ring(2*N) + used_event(2)
        let used_event_gpa =
            self.avail_addr + 4 + u64::from(self.queue_size) * 2;
        let used_event = match self
            .physmap
            .lookup(used_event_gpa, 2)
            .and_then(|sub| sub.read::<u16>().ok())
        {
            Some(v) => v,
            None => return true, // Unreadable: interrupt.
        };

        vring_need_event(used_event, new_idx, old_idx)
    }

    /// Write one entry and advance the index. The caller holds the
    /// Mutex.
    fn write_used_entry(&self, idx: &mut u16, desc_idx: u16, len: u32) {
        let slot = u64::from(*idx % self.queue_size);
        let entry_gpa = self.used_addr + 4 + slot * 8;

        if let Some(sub) = self.physmap.lookup(entry_gpa, 8) {
            // An unmapped ring has no recovery.
            let _ = sub.write::<u32>(&u32::from(desc_idx));
            if let Some(len_sub) = sub.subregion(4, 4) {
                let _ = len_sub.write::<u32>(&len);
            }
        }

        // The guest reads the index first, then the entry.
        fence(Ordering::Release);

        *idx = idx.wrapping_add(1);
        let idx_gpa = self.used_addr + 2;
        if let Some(sub) = self.physmap.lookup(idx_gpa, 2) {
            let _ = sub.write::<u16>(idx);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    use super::*;

    const AVAIL_GPA: u64 = 0x1100;
    const QUEUE_SIZE: u16 = 8;
    const RACERS: usize = 8;
    const FORCED: u32 = 4;

    /// A completion for a ring whose driver has pushed `used_event`
    /// out of reach, so only the force counter can ask for an
    /// interrupt.
    fn suppressed(physmap: &Arc<PhysMap>) -> Arc<VirtioCompletion> {
        let mut queue = VirtQueue::new(QUEUE_SIZE);
        queue.set_addr_modern(0x1000, AVAIL_GPA, 0x1200);
        queue.set_event_idx(true);
        physmap
            .lookup(AVAIL_GPA + 4 + u64::from(QUEUE_SIZE) * 2, 2)
            .expect("mapped used_event")
            .write::<u16>(&60_000)
            .expect("write used_event");
        VirtioCompletion::new(
            &queue,
            Arc::clone(physmap),
            IntrSession::INITIAL,
            |_| {},
        )
    }

    // Threads can read the same forced count and each subtract it. A
    // plain subtract wraps the counter to u32::MAX, and the guest loses
    // EVENT_IDX suppression on every later completion. N counts must
    // force N interrupts and no more.
    #[test]
    fn a_race_cannot_force_more_interrupts_than_were_asked_for() {
        let physmap =
            Arc::new(PhysMap::new_anon(0x1000, 0x1000).expect("queue memory"));
        let completion = suppressed(&physmap);

        for round in 0..512 {
            completion.set_force_interrupt(FORCED);
            let forced = AtomicUsize::new(0);
            let ready = AtomicUsize::new(0);
            let go = AtomicBool::new(false);
            std::thread::scope(|s| {
                for _ in 0..RACERS {
                    s.spawn(|| {
                        ready.fetch_add(1, Ordering::Release);
                        while !go.load(Ordering::Acquire) {
                            std::hint::spin_loop();
                        }
                        if completion.should_interrupt(0, 1) {
                            forced.fetch_add(1, Ordering::Relaxed);
                        }
                    });
                }
                while ready.load(Ordering::Acquire) < RACERS {
                    std::hint::spin_loop();
                }
                go.store(true, Ordering::Release);
            });
            let n = forced.load(Ordering::Relaxed);
            assert_eq!(
                n, FORCED as usize,
                "round {round} forced {n} interrupts, not {FORCED}"
            );
        }
    }
}
