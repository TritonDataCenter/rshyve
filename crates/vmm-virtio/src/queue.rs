// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The split virtqueue (VirtIO 1.3 sec 2.7).
//!
//! A queue has three regions in guest memory:
//! - Descriptor table: the buffer descriptors.
//! - Available ring: the heads the driver offers to the device.
//! - Used ring: the heads the device returns to the driver.
//!
//! # Safety
//!
//! All guest memory access goes through [`PhysMap::lookup`], which
//! validates the GPA. The queue size bounds every descriptor chain, so a
//! hostile guest cannot make the device loop forever.

use std::sync::atomic::{fence, Ordering};

use slog::debug;

use vmm_core::common::PAGE_SIZE;
use vmm_core::mem::PhysMap;

use super::bits;

/// The largest queue size the spec allows.
pub const MAX_QUEUE_SIZE: u16 = 32768;

/// One 16-byte virtqueue descriptor as laid out in guest memory
/// (VirtIO 1.3 sec 2.7.5).
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct VirtqDesc {
    pub addr: u64,
    pub len: u32,
    /// NEXT, WRITE and INDIRECT.
    pub flags: u16,
    /// The next descriptor, valid only when NEXT is set.
    pub next: u16,
}

impl VirtqDesc {
    pub fn has_next(&self) -> bool {
        self.flags & bits::VRING_DESC_F_NEXT != 0
    }

    /// True for a device-writable buffer.
    pub fn is_write(&self) -> bool {
        self.flags & bits::VRING_DESC_F_WRITE != 0
    }

    /// True when the descriptor points to an indirect table.
    pub fn is_indirect(&self) -> bool {
        self.flags & bits::VRING_DESC_F_INDIRECT != 0
    }
}

/// A buffer segment within a descriptor chain, classified by direction.
#[derive(Debug)]
pub enum ChainBuf {
    /// The guest wrote it for the device to read.
    Readable { addr: u64, len: u32 },
    /// The device writes it for the guest to read.
    Writable { addr: u64, len: u32 },
}

/// One of the three rings a split virtqueue is made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ring {
    Desc,
    Avail,
    Used,
}

impl Ring {
    /// The alignment VirtIO 1.3 sec 2.7 fixes for the ring.
    const fn align(self) -> u64 {
        match self {
            Ring::Desc => 16,
            Ring::Avail => 2,
            Ring::Used => 4,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Ring::Desc => "desc",
            Ring::Avail => "avail",
            Ring::Used => "used",
        }
    }
}

/// Why the device refuses the ring addresses a guest programmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueAddrError {
    /// The address is zero, so the guest never programmed the ring.
    Unset(Ring),
    /// The address breaks the alignment the spec fixes for the ring.
    Misaligned { ring: Ring, addr: u64 },
    /// The ring is not mapped guest memory for its whole size.
    Unmapped { ring: Ring, addr: u64, len: usize },
}

impl std::fmt::Display for QueueAddrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unset(ring) => {
                write!(f, "{} ring address is zero", ring.name())
            }
            Self::Misaligned { ring, addr } => write!(
                f,
                "{} ring at {addr:#x} is not aligned to {}",
                ring.name(),
                ring.align(),
            ),
            Self::Unmapped { ring, addr, len } => write!(
                f,
                "{} ring at {addr:#x} is not {len} bytes of guest memory",
                ring.name(),
            ),
        }
    }
}

/// Why the device refuses the ring size a driver wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueSizeError {
    /// Zero, not a power of two, or larger than the device offered.
    Invalid { size: u16, max: u16 },
    /// The ring is programmed. Its size is fixed until a reset.
    Live,
}

impl std::fmt::Display for QueueSizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid { size, max } => write!(
                f,
                "queue size {size} is not a power of two in 1..={max}"
            ),
            Self::Live => write!(f, "queue size written on a programmed ring"),
        }
    }
}

/// A split virtqueue.
///
/// The guest sets the ring addresses during device init, through the
/// legacy PFN register or the modern per-ring address registers.
pub struct VirtQueue {
    /// The ring size in use. A modern driver may shrink it below
    /// `max_size` before it enables the queue (sec 4.1.4.3.2).
    size: u16,
    /// The size the device offers, and returns to on reset.
    max_size: u16,
    desc_addr: u64,
    avail_addr: u64,
    used_addr: u64,
    /// The next available ring index the device takes.
    last_avail_idx: u16,
    /// Device-side copy of the used ring index.
    ///
    /// The guest can change `used_idx` in shared memory at any time. A
    /// device that read it back would put entries in the slots the
    /// guest chose.
    shadow_used_idx: u16,
    /// VIRTIO_F_RING_EVENT_IDX is negotiated.
    event_idx: bool,
    /// VIRTIO_F_RING_INDIRECT_DESC is negotiated.
    indirect_supported: bool,
    /// The address check, cached from the first use of the rings.
    addrs_ok: Option<bool>,
    /// Where a refusal is reported. Silent until a transport sets one.
    log: Option<slog::Logger>,
}

impl VirtQueue {
    /// # Panics
    ///
    /// Panics if `size` is 0, not a power of 2, or exceeds MAX_QUEUE_SIZE.
    pub fn new(size: u16) -> Self {
        assert!(
            size > 0 && size.is_power_of_two(),
            "queue size must be power of 2"
        );
        assert!(size <= MAX_QUEUE_SIZE, "queue size exceeds maximum");
        Self {
            size,
            max_size: size,
            desc_addr: 0,
            avail_addr: 0,
            used_addr: 0,
            last_avail_idx: 0,
            shadow_used_idx: 0,
            event_idx: false,
            indirect_supported: false,
            addrs_ok: None,
            log: None,
        }
    }

    /// Report refusals through `log`.
    pub fn set_log(&mut self, log: slog::Logger) {
        self.log = Some(log);
    }

    pub fn size(&self) -> u16 {
        self.size
    }

    /// The largest ring this queue offers. A reset returns to it.
    pub fn max_size(&self) -> u16 {
        self.max_size
    }

    /// Take the ring size a modern driver wrote.
    ///
    /// The driver can only shrink the ring, and only before it programs
    /// the addresses. The device accesses `size` entries, so a size that
    /// changes under a live ring walks past the memory the driver
    /// allocated.
    pub fn set_size(&mut self, size: u16) -> Result<(), QueueSizeError> {
        if size == 0 || !size.is_power_of_two() || size > self.max_size {
            return Err(QueueSizeError::Invalid {
                size,
                max: self.max_size,
            });
        }
        if self.is_configured() {
            return Err(QueueSizeError::Live);
        }
        self.size = size;
        Ok(())
    }

    /// True once the guest programmed a descriptor table address.
    pub fn is_configured(&self) -> bool {
        self.desc_addr != 0
    }

    /// Set VIRTIO_F_RING_EVENT_IDX notification suppression.
    ///
    /// With the feature, the device writes `avail_event` to tell the
    /// guest when to kick. It reads `used_event` from the guest to
    /// decide when to raise an interrupt.
    pub fn set_event_idx(&mut self, enabled: bool) {
        // The feature adds the suppression word to each ring, so a
        // cached verdict applies to a shorter ring.
        if self.event_idx != enabled {
            self.addrs_ok = None;
        }
        self.event_idx = enabled;
    }

    pub fn event_idx_enabled(&self) -> bool {
        self.event_idx
    }

    pub fn indirect_supported(&self) -> bool {
        self.indirect_supported
    }

    pub fn set_indirect_supported(&mut self, supported: bool) {
        self.indirect_supported = supported;
    }

    pub fn avail_addr(&self) -> u64 {
        self.avail_addr
    }

    /// Read the driver's `avail_idx` from guest memory.
    ///
    /// Migration export uses it to capture the true number of available
    /// buffers. The value can be ahead of the kernel's cached index.
    pub fn read_avail_idx(&self, physmap: &PhysMap) -> u16 {
        if self.avail_addr == 0 {
            return 0;
        }
        let gpa = self.avail_addr + 2;
        physmap
            .lookup(gpa, 2)
            .and_then(|sub| sub.read::<u16>().ok())
            .unwrap_or(0)
    }

    /// Set the ring addresses from a legacy page frame number.
    ///
    /// The legacy layout (VirtIO 1.3 sec 2.7.2) puts all three rings in
    /// one contiguous area:
    /// - Descriptors at PFN * PAGE_SIZE.
    /// - Available ring directly after the descriptors.
    /// - Used ring at the next page boundary after the available ring.
    pub fn set_addr_legacy(&mut self, pfn: u32) {
        self.addrs_ok = None;
        if pfn == 0 {
            self.desc_addr = 0;
            self.avail_addr = 0;
            self.used_addr = 0;
            self.last_avail_idx = 0;
            self.shadow_used_idx = 0;
            return;
        }

        let base = u64::from(pfn) * PAGE_SIZE as u64;

        self.desc_addr = base;

        self.avail_addr = base + u64::from(self.size) * 16;

        // avail: flags(2) + idx(2) + ring(2*size) + used_event(2)
        let avail_end = self.avail_addr + 6 + u64::from(self.size) * 2;
        self.used_addr = align_up(avail_end, PAGE_SIZE as u64);
    }

    /// Set the three ring addresses the modern transport gives.
    pub fn set_addr_modern(&mut self, desc: u64, avail: u64, used: u64) {
        self.addrs_ok = None;
        self.desc_addr = desc;
        self.avail_addr = avail;
        self.used_addr = used;
        self.last_avail_idx = 0;
        self.shadow_used_idx = 0;
    }

    /// Restore `last_avail_idx` on migration.
    pub fn set_last_avail_idx(&mut self, idx: u16) {
        self.last_avail_idx = idx;
    }

    /// Restore `shadow_used_idx` on migration.
    pub fn set_shadow_used_idx(&mut self, idx: u16) {
        self.shadow_used_idx = idx;
    }

    /// Read the used ring `idx` from guest memory.
    ///
    /// A device that completes through `VirtioCompletion` (virtio-blk)
    /// never updates `shadow_used_idx`. Only guest memory holds its
    /// used index.
    pub fn read_used_ring_idx(&self, physmap: &PhysMap) -> u16 {
        if self.used_addr == 0 {
            return 0;
        }
        let gpa = self.used_addr + 2;
        physmap
            .lookup(gpa, 2)
            .and_then(|sub| sub.read::<u16>().ok())
            .unwrap_or(0)
    }

    pub fn desc_addr(&self) -> u64 {
        self.desc_addr
    }

    pub fn used_addr(&self) -> u64 {
        self.used_addr
    }

    pub fn last_avail_idx(&self) -> u16 {
        self.last_avail_idx
    }

    pub fn shadow_used_idx(&self) -> u16 {
        self.shadow_used_idx
    }

    /// The legacy PFN for this queue.
    pub fn pfn(&self) -> u32 {
        if self.desc_addr == 0 {
            0
        } else {
            (self.desc_addr / PAGE_SIZE as u64) as u32
        }
    }

    pub fn reset(&mut self) {
        self.addrs_ok = None;
        self.size = self.max_size;
        self.desc_addr = 0;
        self.avail_addr = 0;
        self.used_addr = 0;
        self.last_avail_idx = 0;
        self.shadow_used_idx = 0;
    }

    /// Bytes the descriptor table occupies: 16 per entry (sec 2.7.5).
    fn desc_table_bytes(&self) -> usize {
        usize::from(self.size) * 16
    }

    /// Bytes of the available ring the device can reach: flags, idx
    /// and one 2-byte entry per descriptor (sec 2.7.6).
    fn avail_ring_bytes(&self) -> usize {
        4 + usize::from(self.size) * 2 + self.event_field_bytes()
    }

    /// Bytes of the used ring the device can reach: flags, idx and one
    /// 8-byte entry per descriptor (sec 2.7.8).
    fn used_ring_bytes(&self) -> usize {
        4 + usize::from(self.size) * 8 + self.event_field_bytes()
    }

    /// The suppression field each ring has only under EVENT_IDX.
    /// Without the feature the device never reads that far, so a check
    /// that includes it refuses a ring that works.
    fn event_field_bytes(&self) -> usize {
        if self.event_idx {
            2
        } else {
            0
        }
    }

    /// Check the three ring addresses against the guest memory map.
    ///
    /// A misaligned or unmapped ring fails every device access to it.
    /// Nothing else detects this: the device takes heads off the
    /// available ring and completes none, while the guest still reads
    /// DRIVER_OK.
    pub fn validate_addrs(
        &self,
        physmap: &PhysMap,
    ) -> Result<(), QueueAddrError> {
        for (ring, addr, len) in [
            (Ring::Desc, self.desc_addr, self.desc_table_bytes()),
            (Ring::Avail, self.avail_addr, self.avail_ring_bytes()),
            (Ring::Used, self.used_addr, self.used_ring_bytes()),
        ] {
            if addr == 0 {
                return Err(QueueAddrError::Unset(ring));
            }
            if !addr.is_multiple_of(ring.align()) {
                return Err(QueueAddrError::Misaligned { ring, addr });
            }
            // `lookup` refuses a range that wraps or leaves the mapped
            // run. The range is the reach of every later access.
            if physmap.lookup(addr, len).is_none() {
                return Err(QueueAddrError::Unmapped { ring, addr, len });
            }
        }
        Ok(())
    }

    /// Whether the rings passed [`Self::validate_addrs`], checked once
    /// and cached.
    ///
    /// The verdict holds until the guest programs the rings again. A
    /// queue that failed cannot start to work, and a check on every
    /// kick lets a guest flood the log.
    fn addrs_usable(&mut self, physmap: &PhysMap) -> bool {
        if let Some(ok) = self.addrs_ok {
            return ok;
        }
        match self.validate_addrs(physmap) {
            Ok(()) => {
                self.addrs_ok = Some(true);
                true
            }
            Err(error) => {
                self.addrs_ok = Some(false);
                if let Some(log) = &self.log {
                    debug!(log, "virtqueue refused"; "reason" => %error);
                }
                false
            }
        }
    }

    /// Whether the queue refused the addresses the guest programmed.
    ///
    /// The transport reports this as DEVICE_NEEDS_RESET. A driver has
    /// no other way to learn that the device stopped.
    pub fn needs_reset(&self) -> bool {
        self.addrs_ok == Some(false)
    }

    /// `None` if `idx` is out of bounds or the GPA is unmapped.
    pub fn read_desc(&self, physmap: &PhysMap, idx: u16) -> Option<VirtqDesc> {
        if idx >= self.size {
            return None;
        }
        let offset = u64::from(idx) * 16;
        let gpa = self.desc_addr + offset;
        let sub = physmap.lookup(gpa, 16)?;
        sub.read::<VirtqDesc>().ok()
    }

    /// Take the next head from the available ring, or `None` if the
    /// ring is empty.
    pub fn pop_avail(&mut self, physmap: &PhysMap) -> Option<u16> {
        if !self.is_configured() {
            return None;
        }
        // The check is here, not in the address setters, because only
        // a caller with the memory map knows where the rings land.
        if !self.addrs_usable(physmap) {
            return None;
        }

        let avail_idx_gpa = self.avail_addr + 2;
        let sub = physmap.lookup(avail_idx_gpa, 2)?;
        let avail_idx: u16 = sub.read::<u16>().ok()?;

        // Descriptor reads must see the writes the guest made before it
        // stored avail_idx (VirtIO 1.3 sec 2.7.13).
        fence(Ordering::Acquire);

        if self.last_avail_idx == avail_idx {
            return None;
        }

        let ring_offset = u64::from(self.last_avail_idx % self.size) * 2;
        let ring_entry_gpa = self.avail_addr + 4 + ring_offset;
        let sub = physmap.lookup(ring_entry_gpa, 2)?;
        let desc_idx: u16 = sub.read::<u16>().ok()?;

        self.last_avail_idx = self.last_avail_idx.wrapping_add(1);

        if desc_idx >= self.size {
            return None;
        }

        Some(desc_idx)
    }

    /// Take heads and give each to `each`, until the ring is empty or
    /// `max` heads are taken. Returns the number taken.
    ///
    /// Every exit arms `avail_event`, the cap exit too. With EVENT_IDX
    /// the guest kicks only when it adds at the index the device asked
    /// for (sec 2.7.7.2). A stale threshold is below every index the
    /// guest adds at from now on, so the device never gets a kick
    /// again. A drain that fills its cap is common: Linux posts a whole
    /// ring of receive buffers when it probes.
    ///
    /// After the arm, the drain reads `avail_idx` again. The guest
    /// judged an entry added between the drain and the arm against the
    /// old threshold, and may never kick for it (sec 2.7.10).
    pub fn drain_avail(
        &mut self,
        physmap: &PhysMap,
        max: usize,
        mut each: impl FnMut(&mut VirtQueue, u16),
    ) -> usize {
        let mut popped = 0usize;
        loop {
            let before = self.last_avail_idx;
            while popped < max {
                let Some(head) = self.pop_avail(physmap) else {
                    break;
                };
                popped += 1;
                each(self, head);
            }
            self.update_used_event(physmap);
            if queue_drain_should_stop(self, before, popped >= max, physmap) {
                return popped;
            }
        }
    }

    /// Write `avail_event` in the used ring to suppress guest kicks.
    ///
    /// Call it after the drain, not per head. It sets `avail_event` to
    /// `last_avail_idx`, so the guest kicks for the next entry it adds.
    /// `avail_event` follows the used entries: flags(2) + idx(2) +
    /// entries(8*N) (VirtIO 1.3 sec 2.7.7).
    ///
    /// The SeqCst fence after the write makes the caller's
    /// `has_new_avail()` check see the guest's latest `avail_idx`.
    /// Without it, a store-load reorder can miss entries the guest added
    /// between the drain and this write (VirtIO 1.3 sec 2.7.10, QEMU
    /// `smp_mb()` in `virtio_queue_split_set_notification`).
    pub fn update_used_event(&self, physmap: &PhysMap) {
        if !self.event_idx {
            return;
        }
        let avail_event_gpa = self.used_addr + 4 + u64::from(self.size) * 8;
        if let Some(sub) = physmap.lookup(avail_event_gpa, 2) {
            let _ = sub.write::<u16>(&self.last_avail_idx);
        }
        fence(Ordering::SeqCst);
    }

    /// Whether new entries arrived after `update_used_event()`.
    ///
    /// The device must process them: the guest possibly read the stale
    /// `avail_event` and did not kick (VirtIO 1.3 sec 2.7.10). The
    /// SeqCst fence in `update_used_event()` makes this read see the
    /// guest's latest `avail_idx`.
    pub fn has_new_avail(&self, physmap: &PhysMap) -> bool {
        if !self.event_idx || !self.is_configured() {
            return false;
        }
        let avail_idx_gpa = self.avail_addr + 2;
        match physmap
            .lookup(avail_idx_gpa, 2)
            .and_then(|sub| sub.read::<u16>().ok())
        {
            Some(avail_idx) => avail_idx != self.last_avail_idx,
            None => false,
        }
    }

    /// The used ring index from the device-side shadow.
    ///
    /// It does not read guest memory, so the guest cannot change
    /// notification decisions through it.
    pub fn read_used_idx(&self, _physmap: &PhysMap) -> u16 {
        self.shadow_used_idx
    }

    /// Whether the guest expects an interrupt after used ring updates.
    ///
    /// With EVENT_IDX, it reads `used_event` from the available ring
    /// and applies the wrapping comparison of VirtIO 1.3 sec 2.7.7.1.
    /// Without EVENT_IDX, it checks VIRTQ_AVAIL_F_NO_INTERRUPT.
    ///
    /// `old_used_idx` is the used index before the device started to
    /// process. The current used index comes from the device-side
    /// shadow.
    pub fn should_notify_guest(
        &self,
        physmap: &PhysMap,
        old_used_idx: u16,
    ) -> bool {
        if !self.event_idx {
            if self.avail_addr != 0 {
                if let Some(sub) = physmap.lookup(self.avail_addr, 2) {
                    if let Ok(flags) = sub.read::<u16>() {
                        return flags & bits::VIRTQ_AVAIL_F_NO_INTERRUPT == 0;
                    }
                }
            }
            return true;
        }

        // The used_idx writes from push_used must be visible to the
        // guest before the read of used_event (QEMU smp_mb() in
        // virtio_split_should_notify()).
        fence(Ordering::SeqCst);

        let new_used_idx = self.read_used_idx(physmap);

        // Layout: flags(2) + idx(2) + ring(2*N) + used_event(2)
        let used_event_gpa = self.avail_addr + 4 + u64::from(self.size) * 2;
        let used_event = match physmap
            .lookup(used_event_gpa, 2)
            .and_then(|sub| sub.read::<u16>().ok())
        {
            Some(v) => v,
            None => return true, // Unreadable: interrupt.
        };

        vring_need_event(used_event, new_used_idx, old_used_idx)
    }

    /// Push an entry onto the used ring.
    ///
    /// `desc_idx` is the head. `len` is the number of bytes the device
    /// wrote to device-writable buffers. The index comes from the
    /// device-side shadow, never from shared memory.
    pub fn push_used(&mut self, physmap: &PhysMap, desc_idx: u16, len: u32) {
        let used_idx = self.shadow_used_idx;

        // Entry: id (u32) + len (u32)
        let ring_offset = u64::from(used_idx % self.size) * 8;
        let entry_gpa = self.used_addr + 4 + ring_offset;
        if let Some(sub) = physmap.lookup(entry_gpa, 8) {
            let _ = sub.write::<u32>(&(u32::from(desc_idx)));
            if let Some(len_sub) = sub.subregion(4, 4) {
                let _ = len_sub.write::<u32>(&len);
            }
        }

        // The guest must see the entry before the new used_idx
        // (VirtIO 1.3 sec 2.7.8).
        fence(Ordering::Release);

        self.shadow_used_idx = used_idx.wrapping_add(1);
        let used_idx_gpa = self.used_addr + 2;
        if let Some(sub) = physmap.lookup(used_idx_gpa, 2) {
            let _ = sub.write::<u16>(&self.shadow_used_idx);
        }
    }

    /// The used ring address and queue size, for a completion path that
    /// does not hold the `VirtQueue`.
    pub fn used_ring_info(&self) -> (u64, u16) {
        (self.used_addr, self.size)
    }

    /// Walk the indirect table of `len` bytes at `gpa`.
    ///
    /// The table holds `len / 16` descriptors linked by NEXT. INDIRECT
    /// inside the table is invalid (VirtIO 1.3 sec 2.7.5.3.1).
    ///
    /// `budget` is how many more descriptors the chain can name. A
    /// chain cannot be longer than the ring (sec 2.7.5.3.1), and the
    /// direct descriptors before the table count towards that limit.
    fn walk_indirect_table(
        &self,
        physmap: &PhysMap,
        gpa: u64,
        len: u32,
        budget: u16,
    ) -> Option<Vec<ChainBuf>> {
        let num_descs = len as usize / 16;
        if num_descs == 0 || !(len as usize).is_multiple_of(16) {
            return None;
        }
        if num_descs > usize::from(budget) {
            return None;
        }

        let mut bufs = Vec::with_capacity(num_descs);
        let mut idx: usize = 0;
        let mut iterations: usize = 0;

        loop {
            if idx >= num_descs || iterations >= num_descs {
                return None; // Out of bounds, or a cycle.
            }
            iterations += 1;

            let desc_gpa = gpa.checked_add((idx as u64).checked_mul(16)?)?;
            let sub = physmap.lookup(desc_gpa, 16)?;
            let desc: VirtqDesc = sub.read::<VirtqDesc>().ok()?;

            if desc.is_indirect() {
                return None;
            }

            if desc.is_write() {
                bufs.push(ChainBuf::Writable {
                    addr: desc.addr,
                    len: desc.len,
                });
            } else {
                bufs.push(ChainBuf::Readable {
                    addr: desc.addr,
                    len: desc.len,
                });
            }

            if desc.has_next() {
                idx = desc.next as usize;
            } else {
                break;
            }
        }

        Some(bufs)
    }

    /// Collect the buffers of the chain that starts at `head`.
    ///
    /// The queue size bounds the chain length, so a hostile guest
    /// cannot make it loop. `None` if any descriptor is unreadable or
    /// invalid.
    pub fn collect_chain(
        &self,
        physmap: &PhysMap,
        head: u16,
    ) -> Option<Vec<ChainBuf>> {
        let mut bufs = Vec::new();
        let mut idx = head;
        let mut count = 0u16;

        loop {
            // A chain cannot have more entries than the queue has
            // descriptors.
            if count >= self.size {
                return None;
            }

            let desc = self.read_desc(physmap, idx)?;
            count += 1;

            if desc.is_indirect() {
                if !self.indirect_supported {
                    return None;
                }
                // INDIRECT + NEXT is invalid (VirtIO 1.3 sec 2.7.5.3.1)
                if desc.has_next() {
                    return None;
                }
                // Sec 2.7.5.3.1: the device must accept zero or more
                // direct descriptors followed by one indirect one. The
                // buffers before the table belong to the same request,
                // so they stay in the result. The table ends the chain.
                let budget = self.size - count + 1;
                bufs.extend(self.walk_indirect_table(
                    physmap, desc.addr, desc.len, budget,
                )?);
                break;
            }

            if desc.is_write() {
                bufs.push(ChainBuf::Writable {
                    addr: desc.addr,
                    len: desc.len,
                });
            } else {
                bufs.push(ChainBuf::Readable {
                    addr: desc.addr,
                    len: desc.len,
                });
            }

            if desc.has_next() {
                idx = desc.next;
            } else {
                break;
            }
        }

        Some(bufs)
    }
}

/// Whether a drain that just armed `avail_event` is finished.
///
/// It stops when the last pass made no progress (an unreadable entry
/// otherwise makes it spin), when it hit its cap, or when the guest
/// added nothing during the arm.
pub(crate) fn queue_drain_should_stop(
    queue: &VirtQueue,
    before: u16,
    hit_request_cap: bool,
    physmap: &PhysMap,
) -> bool {
    queue.last_avail_idx() == before
        || hit_request_cap
        || !queue.has_new_avail(physmap)
}

/// The wrapping comparison for notification suppression (VirtIO 1.3
/// sec 2.7.7.1): true when the move from `old_idx` to `new_idx` crosses
/// `event_idx`. Linux and QEMU call it `vring_need_event()`.
#[inline]
fn vring_need_event(event_idx: u16, new_idx: u16, old_idx: u16) -> bool {
    new_idx.wrapping_sub(event_idx).wrapping_sub(1)
        < new_idx.wrapping_sub(old_idx)
}

/// `align` must be a power of 2.
fn align_up(val: u64, align: u64) -> u64 {
    (val + align - 1) & !(align - 1)
}

mod completion;
pub use completion::VirtioCompletion;

#[cfg(test)]
mod tests;
