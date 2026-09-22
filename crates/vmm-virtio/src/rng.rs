// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VirtIO entropy (RNG) device.
//!
//! The guest posts writable buffers on one virtqueue. The device fills
//! them from the host kernel CSPRNG. Guest entropy quality depends only
//! on that source.

use vmm_core::mem::PhysMap;

use super::queue::{ChainBuf, VirtQueue};
use super::VirtioDevice;
use vmm_devices::Lifecycle;

/// Guest memory is filled through a fixed buffer, so a descriptor
/// length the guest picked never becomes an allocation size.
const CHUNK: usize = 4096;

/// Entropy one descriptor chain may ask for.
///
/// A chain can name as many descriptors as the ring holds, each with a
/// guest `u32` length, so one chain can ask for terabytes. The Linux
/// hwrng core reads tens of bytes at a time, so no real driver reaches
/// this limit.
const BYTES_PER_CHAIN: usize = 64 * 1024;

/// Entropy one notification may produce.
///
/// The vCPU drains the queue inline under the transport lock. Every
/// other vCPU that touches this device's registers waits for it. So
/// does a pause or teardown that needs that vCPU to leave the exit
/// handler.
const BYTES_PER_KICK: usize = 512 * 1024;

/// Chains one notification may serve.
const CHAINS_PER_KICK: usize = 64;

/// VirtIO entropy device (type 4).
pub struct VirtioRng;

impl Default for VirtioRng {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtioRng {
    pub fn new() -> Self {
        Self
    }

    /// Fill the writable buffers of one chain, spending `budget`.
    ///
    /// Returns the number of bytes written, which goes in the used
    /// ring. A short fill is how the protocol reports a buffer the
    /// device cannot fill: the driver reads the used length and asks
    /// again.
    fn fill_chain(
        queue: &VirtQueue,
        head: u16,
        physmap: &PhysMap,
        budget: &mut usize,
    ) -> u32 {
        let Some(chain) = queue.collect_chain(physmap, head) else {
            return 0;
        };

        let mut chunk = [0u8; CHUNK];
        let mut written = 0u32;

        for buf in &chain {
            let ChainBuf::Writable { addr, len } = buf else {
                continue;
            };
            let mut remaining = (*len as usize).min(*budget);
            let mut offset = 0u64;

            while remaining > 0 {
                let n = remaining.min(CHUNK);

                // Map before generating: entropy for unmapped memory is
                // host work with no guest effect, and a guest can name a
                // 4 GiB unmapped buffer.
                let Some(gpa) = addr.checked_add(offset) else {
                    return written;
                };
                let Some(sub) = physmap.lookup(gpa, n) else {
                    return written;
                };
                if getrandom::fill(&mut chunk[..n]).is_err() {
                    return written;
                }
                if sub.write_bytes(&chunk[..n]).is_err() {
                    return written;
                }

                offset += n as u64;
                remaining -= n;
                *budget -= n;
                written = written.saturating_add(n as u32);
            }
        }

        written
    }
}

impl VirtioDevice for VirtioRng {
    fn device_features(&self) -> u64 {
        // Ring features: legacy transport strips these in pci.rs.
        super::bits::VIRTIO_F_RING_EVENT_IDX
            | super::bits::VIRTIO_F_RING_INDIRECT_DESC
    }

    fn set_features(&self, _features: u64) {}

    fn cfg_read(&self, _offset: u16, _len: u8) -> u32 {
        0 // No device-specific config
    }

    fn cfg_write(&self, _offset: u16, _val: u32, _len: u8) {}

    fn process_queue(
        &self,
        _queue_idx: u16,
        queue: &mut VirtQueue,
        head: u16,
        physmap: &PhysMap,
    ) -> u32 {
        let mut budget = BYTES_PER_CHAIN;
        Self::fill_chain(queue, head, physmap, &mut budget)
    }

    /// The default drain serves a full ring of chains, each up to
    /// [`BYTES_PER_CHAIN`]. This drain shares one per-kick budget
    /// across the chains.
    fn notify_queue(
        &self,
        queue_idx: u16,
        queues: &mut [VirtQueue],
        physmap: &PhysMap,
    ) -> bool {
        let Some(queue) = queues.get_mut(usize::from(queue_idx)) else {
            return false;
        };

        let old_used_idx = queue.read_used_idx(physmap);
        let mut budget = BYTES_PER_KICK;
        let popped = queue.drain_avail(physmap, CHAINS_PER_KICK, |q, head| {
            let mut chain_budget = budget.min(BYTES_PER_CHAIN);
            let written = Self::fill_chain(q, head, physmap, &mut chain_budget);
            // `fill_chain` spends at most `chain_budget`, which is at
            // most `budget`, so this cannot underflow.
            budget -= written as usize;
            q.push_used(physmap, head, written);
        });

        popped > 0 && queue.should_notify_guest(physmap, old_used_idx)
    }

    fn reset(&self) {}
}

impl Lifecycle for VirtioRng {
    fn type_name(&self) -> &'static str {
        "virtio-rng"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits;
    use crate::queue::VirtqDesc;

    const DESC_GPA: u64 = 0x1000;
    const AVAIL_GPA: u64 = 0x1200;
    const USED_GPA: u64 = 0x1400;
    const DATA_GPA: u64 = 0x2000;
    const UNMAPPED_GPA: u64 = 0x9000_0000;
    const REGION: usize = 0x2_1000;

    fn write_desc(physmap: &PhysMap, idx: u16, desc: VirtqDesc) {
        physmap
            .lookup(DESC_GPA + u64::from(idx) * 16, 16)
            .expect("mapped descriptor")
            .write(&desc)
            .expect("write descriptor");
    }

    fn writable(addr: u64, len: u32, next: u16, chained: bool) -> VirtqDesc {
        VirtqDesc {
            addr,
            len,
            flags: bits::VRING_DESC_F_WRITE
                | if chained { bits::VRING_DESC_F_NEXT } else { 0 },
            next,
        }
    }

    /// A ring of `size` one-descriptor chains, each naming the same
    /// `len` bytes of data, with `avail` of them posted.
    fn posted_queue(size: u16, len: u32, avail: u16) -> (PhysMap, VirtQueue) {
        let physmap =
            PhysMap::new_anon(DESC_GPA, REGION).expect("create guest memory");
        let mut queue = VirtQueue::new(size);
        queue.set_addr_modern(DESC_GPA, AVAIL_GPA, USED_GPA);
        queue.set_event_idx(true);

        for idx in 0..size {
            write_desc(&physmap, idx, writable(DATA_GPA, len, 0, false));
            physmap
                .lookup(AVAIL_GPA + 4 + u64::from(idx) * 2, 2)
                .expect("mapped avail entry")
                .write::<u16>(&idx)
                .expect("write avail entry");
        }
        physmap
            .lookup(AVAIL_GPA + 2, 2)
            .expect("mapped avail index")
            .write::<u16>(&avail)
            .expect("write avail index");

        (physmap, queue)
    }

    fn used_len(physmap: &PhysMap, slot: u16) -> u32 {
        physmap
            .lookup(USED_GPA + 4 + u64::from(slot) * 8 + 4, 4)
            .expect("mapped used entry")
            .read::<u32>()
            .expect("read used length")
    }

    #[test]
    fn features_include_event_idx_and_indirect() {
        let rng = VirtioRng::new();
        let f = rng.device_features();
        assert_ne!(f & bits::VIRTIO_F_RING_EVENT_IDX, 0);
        assert_ne!(f & bits::VIRTIO_F_RING_INDIRECT_DESC, 0);
    }

    #[test]
    fn config_read_returns_zero() {
        let rng = VirtioRng::new();
        assert_eq!(rng.cfg_read(0, 4), 0);
        assert_eq!(rng.cfg_read(4, 1), 0);
    }

    #[test]
    fn process_queue_fills_the_writable_buffer() {
        let (physmap, mut queue) = posted_queue(4, 64, 1);
        let rng = VirtioRng::new();

        assert_eq!(rng.process_queue(0, &mut queue, 0, &physmap), 64);

        let mut bytes = [0u8; 64];
        physmap
            .lookup(DATA_GPA, 64)
            .expect("mapped data")
            .read_bytes(&mut bytes)
            .expect("read data");
        assert!(
            bytes.iter().any(|b| *b != 0),
            "no entropy reached the guest buffer",
        );
    }

    /// A descriptor length is a guest `u32` and a chain can name as
    /// many as the ring holds. Without a cap, one chain asks the host
    /// CSPRNG for terabytes on the vCPU thread.
    #[test]
    fn one_chain_spends_at_most_its_byte_budget() {
        let over = u32::try_from(BYTES_PER_CHAIN).expect("fits") * 3;
        let (physmap, mut queue) = posted_queue(4, over, 1);
        let rng = VirtioRng::new();

        assert_eq!(
            rng.process_queue(0, &mut queue, 0, &physmap) as usize,
            BYTES_PER_CHAIN,
        );
    }

    /// An unmapped buffer must end the chain. Otherwise an unmapped
    /// 4 GiB descriptor costs 4 GiB of CSPRNG output, and the used
    /// length counts bytes no guest buffer received.
    #[test]
    fn an_unmapped_buffer_stops_the_chain() {
        let (physmap, mut queue) = posted_queue(4, 8, 1);
        write_desc(&physmap, 0, writable(DATA_GPA, 8, 1, true));
        write_desc(&physmap, 1, writable(UNMAPPED_GPA, 8, 2, true));
        write_desc(&physmap, 2, writable(DATA_GPA + 64, 8, 0, false));
        let rng = VirtioRng::new();

        assert_eq!(
            rng.process_queue(0, &mut queue, 0, &physmap),
            8,
            "the chain continued past a buffer the device could not map",
        );
    }

    /// One kick drains a full ring of chains, so a per-chain bound
    /// alone lets a guest multiply it by the ring size.
    #[test]
    fn one_kick_spends_at_most_its_byte_budget() {
        const CHAINS: u16 = 16;
        let per_chain = u32::try_from(BYTES_PER_CHAIN).expect("fits");
        let (physmap, queue) = posted_queue(CHAINS, per_chain, CHAINS);
        let rng = VirtioRng::new();
        let mut queues = [queue];

        rng.notify_queue(0, &mut queues, &physmap);

        let total: usize = (0..CHAINS)
            .map(|slot| used_len(&physmap, slot) as usize)
            .sum();
        assert_eq!(total, BYTES_PER_KICK);
        assert_eq!(
            queues[0].read_used_ring_idx(&physmap),
            CHAINS,
            "every head must go back, budget or not",
        );
    }
}
