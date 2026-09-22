// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unit tests for the split virtqueue.

use std::sync::Arc;

use super::*;

const INDIRECT_TABLE_GPA: u64 = 0x1000;

fn indirect_physmap(num_descs: usize) -> PhysMap {
    PhysMap::new_anon(INDIRECT_TABLE_GPA, num_descs * 16)
        .expect("create anonymous indirect table memory")
}

fn write_indirect_desc(physmap: &PhysMap, idx: usize, desc: VirtqDesc) {
    physmap
        .lookup(INDIRECT_TABLE_GPA + (idx as u64 * 16), 16)
        .expect("mapped indirect descriptor")
        .write(&desc)
        .expect("write indirect descriptor");
}

#[test]
fn pop_avail_failing_lookup_does_not_advance_last_avail_idx() {
    const REGION_GPA: u64 = 0x4000;
    const REGION_SIZE: usize = 0x1000;
    const REGION_END: u64 = REGION_GPA + REGION_SIZE as u64;

    let physmap = PhysMap::new_anon(REGION_GPA, REGION_SIZE)
        .expect("create guest memory");
    let mut queue = VirtQueue::new(2);
    queue.set_addr_modern(REGION_GPA, REGION_END - 4, REGION_GPA + 0x100);
    physmap
        .lookup(REGION_END - 2, 2)
        .expect("mapped avail index")
        .write::<u16>(&1)
        .expect("write avail index");

    let before = queue.last_avail_idx();
    assert_eq!(queue.pop_avail(&physmap), None);
    assert_eq!(queue.last_avail_idx(), before);
}

#[test]
fn new_queue_defaults() {
    let q = VirtQueue::new(256);
    assert_eq!(q.size(), 256);
    assert!(!q.is_configured());
    assert_eq!(q.pfn(), 0);
}

#[test]
fn set_addr_legacy_computes_regions() {
    let mut q = VirtQueue::new(256);
    // PFN 1 => base = 4096
    q.set_addr_legacy(1);
    assert!(q.is_configured());
    assert_eq!(q.pfn(), 1);

    assert_eq!(q.desc_addr, 4096);
    // The avail ring follows the 256-entry descriptor table.
    assert_eq!(q.avail_addr, 4096 + 256 * 16);
    // The used ring starts at the next page boundary after the avail ring.
    let avail_end = q.avail_addr + 6 + 256 * 2;
    let expected_used = align_up(avail_end, PAGE_SIZE as u64);
    assert_eq!(q.used_addr, expected_used);
}

#[test]
fn set_addr_legacy_zero_resets() {
    let mut q = VirtQueue::new(128);
    q.set_addr_legacy(1);
    assert!(q.is_configured());

    q.set_addr_legacy(0);
    assert!(!q.is_configured());
    assert_eq!(q.desc_addr, 0);
    assert_eq!(q.avail_addr, 0);
    assert_eq!(q.used_addr, 0);
}

#[test]
fn reset_clears_state() {
    let mut q = VirtQueue::new(64);
    q.set_addr_legacy(10);
    q.reset();
    assert!(!q.is_configured());
    assert_eq!(q.pfn(), 0);
}

#[test]
#[should_panic(expected = "power of 2")]
fn non_power_of_two_panics() {
    let _ = VirtQueue::new(100);
}

#[test]
#[should_panic(expected = "power of 2")]
fn zero_size_panics() {
    let _ = VirtQueue::new(0);
}

#[test]
fn align_up_works() {
    assert_eq!(align_up(0, 4096), 0);
    assert_eq!(align_up(1, 4096), 4096);
    assert_eq!(align_up(4096, 4096), 4096);
    assert_eq!(align_up(4097, 4096), 8192);
}

#[test]
fn virtq_desc_flags() {
    let desc = VirtqDesc {
        addr: 0x1000,
        len: 512,
        flags: bits::VRING_DESC_F_NEXT | bits::VRING_DESC_F_WRITE,
        next: 1,
    };
    assert!(desc.has_next());
    assert!(desc.is_write());
    assert!(!desc.is_indirect());

    let desc2 = VirtqDesc {
        addr: 0x2000,
        len: 1,
        flags: 0,
        next: 0,
    };
    assert!(!desc2.has_next());
    assert!(!desc2.is_write());
    assert!(!desc2.is_indirect());
}

#[test]
fn virtq_desc_indirect_flag() {
    let desc = VirtqDesc {
        addr: 0x3000,
        len: 256,
        flags: bits::VRING_DESC_F_INDIRECT,
        next: 0,
    };
    assert!(desc.is_indirect());
    assert!(!desc.has_next());
    assert!(!desc.is_write());

    let desc2 = VirtqDesc {
        addr: 0x4000,
        len: 128,
        flags: bits::VRING_DESC_F_INDIRECT | bits::VRING_DESC_F_WRITE,
        next: 0,
    };
    assert!(desc2.is_indirect());
    assert!(desc2.is_write());
}

#[test]
fn indirect_table_longer_than_queue_size_is_rejected() {
    // Every descriptor is backed and walkable, so the size bound is
    // the only thing that can reject this table.
    let q = VirtQueue::new(4);
    let physmap = indirect_physmap(5);
    for idx in 0..5 {
        write_indirect_desc(
            &physmap,
            idx,
            VirtqDesc {
                addr: 0x2000 + (idx as u64 * 0x1000),
                len: 64,
                flags: if idx == 4 { 0 } else { bits::VRING_DESC_F_NEXT },
                next: idx as u16 + 1,
            },
        );
    }

    assert!(q
        .walk_indirect_table(&physmap, INDIRECT_TABLE_GPA, 5 * 16, q.size())
        .is_none());

    // Same table, queue large enough to admit it: now it walks.
    let q = VirtQueue::new(8);
    assert_eq!(
        q.walk_indirect_table(&physmap, INDIRECT_TABLE_GPA, 5 * 16, q.size())
            .expect("table within queue size")
            .len(),
        5
    );
}

#[test]
fn indirect_table_within_queue_size_still_walks() {
    let q = VirtQueue::new(4);
    let physmap = indirect_physmap(3);
    write_indirect_desc(
        &physmap,
        0,
        VirtqDesc {
            addr: 0x2000,
            len: 64,
            flags: bits::VRING_DESC_F_NEXT,
            next: 1,
        },
    );
    write_indirect_desc(
        &physmap,
        1,
        VirtqDesc {
            addr: 0x3000,
            len: 128,
            flags: bits::VRING_DESC_F_NEXT | bits::VRING_DESC_F_WRITE,
            next: 2,
        },
    );
    write_indirect_desc(
        &physmap,
        2,
        VirtqDesc {
            addr: 0x4000,
            len: 32,
            flags: 0,
            next: 0,
        },
    );

    let bufs = q
        .walk_indirect_table(&physmap, INDIRECT_TABLE_GPA, 3 * 16, q.size())
        .expect("valid indirect table");

    assert_eq!(bufs.len(), 3);
    assert!(matches!(
        bufs[0],
        ChainBuf::Readable {
            addr: 0x2000,
            len: 64
        }
    ));
    assert!(matches!(
        bufs[1],
        ChainBuf::Writable {
            addr: 0x3000,
            len: 128
        }
    ));
    assert!(matches!(
        bufs[2],
        ChainBuf::Readable {
            addr: 0x4000,
            len: 32
        }
    ));
}

/// VirtIO 1.3 sec 2.7.5.3.1: the device must handle zero or more
/// normal chained descriptors followed by a single indirect one.
/// Returning the table alone drops the buffers ahead of it, so a
/// device parses the request header out of whatever the table's first
/// entry happens to be.
#[test]
fn a_direct_descriptor_before_an_indirect_table_keeps_its_buffer() {
    const DESC_GPA: u64 = 0x2000;
    let physmap = PhysMap::new_anon(INDIRECT_TABLE_GPA, 0x2000)
        .expect("create queue memory");
    let mut q = VirtQueue::new(4);
    q.set_addr_modern(DESC_GPA, DESC_GPA + 0x100, DESC_GPA + 0x200);
    q.set_indirect_supported(true);

    let write_desc = |idx: u64, desc: VirtqDesc| {
        physmap
            .lookup(DESC_GPA + idx * 16, 16)
            .expect("mapped descriptor")
            .write(&desc)
            .expect("write descriptor");
    };
    // A request header, then a table holding the rest of the request.
    write_desc(
        0,
        VirtqDesc {
            addr: 0x2800,
            len: 16,
            flags: bits::VRING_DESC_F_NEXT,
            next: 1,
        },
    );
    write_desc(
        1,
        VirtqDesc {
            addr: INDIRECT_TABLE_GPA,
            len: 2 * 16,
            flags: bits::VRING_DESC_F_INDIRECT,
            next: 0,
        },
    );
    write_indirect_desc(
        &physmap,
        0,
        VirtqDesc {
            addr: 0x2900,
            len: 512,
            flags: bits::VRING_DESC_F_NEXT | bits::VRING_DESC_F_WRITE,
            next: 1,
        },
    );
    write_indirect_desc(
        &physmap,
        1,
        VirtqDesc {
            addr: 0x2b00,
            len: 1,
            flags: bits::VRING_DESC_F_WRITE,
            next: 0,
        },
    );

    let bufs = q.collect_chain(&physmap, 0).expect("a mixed chain walks");

    assert_eq!(bufs.len(), 3, "the direct header was dropped");
    assert!(matches!(
        bufs[0],
        ChainBuf::Readable {
            addr: 0x2800,
            len: 16
        }
    ));
    assert!(matches!(
        bufs[1],
        ChainBuf::Writable {
            addr: 0x2900,
            len: 512
        }
    ));
    assert!(matches!(
        bufs[2],
        ChainBuf::Writable {
            addr: 0x2b00,
            len: 1
        }
    ));
}

/// A chain may name no more descriptors than the ring holds (sec
/// 2.7.5.3.1), and the direct descriptors ahead of a table count
/// towards that: three direct buffers plus a two-entry table is five
/// descriptors, one too many for a four-entry ring.
#[test]
fn direct_and_indirect_descriptors_share_one_queue_size_budget() {
    const DESC_GPA: u64 = 0x2000;
    let physmap = PhysMap::new_anon(INDIRECT_TABLE_GPA, 0x2000)
        .expect("create queue memory");
    let mut q = VirtQueue::new(4);
    q.set_addr_modern(DESC_GPA, DESC_GPA + 0x100, DESC_GPA + 0x200);
    q.set_indirect_supported(true);

    for idx in 0..3u64 {
        physmap
            .lookup(DESC_GPA + idx * 16, 16)
            .expect("mapped descriptor")
            .write(&VirtqDesc {
                addr: 0x2800 + idx * 0x40,
                len: 16,
                flags: bits::VRING_DESC_F_NEXT,
                next: idx as u16 + 1,
            })
            .expect("write descriptor");
    }
    physmap
        .lookup(DESC_GPA + 3 * 16, 16)
        .expect("mapped descriptor")
        .write(&VirtqDesc {
            addr: INDIRECT_TABLE_GPA,
            len: 2 * 16,
            flags: bits::VRING_DESC_F_INDIRECT,
            next: 0,
        })
        .expect("write descriptor");
    for idx in 0..2 {
        write_indirect_desc(
            &physmap,
            idx,
            VirtqDesc {
                addr: 0x2900 + (idx as u64 * 0x40),
                len: 64,
                flags: if idx == 0 { bits::VRING_DESC_F_NEXT } else { 0 },
                next: 1,
            },
        );
    }

    assert!(
        q.collect_chain(&physmap, 0).is_none(),
        "three direct buffers plus a two-entry table is five descriptors",
    );
}

#[test]
fn indirect_len_not_multiple_of_16_rejected() {
    let q = VirtQueue::new(4);
    let physmap = indirect_physmap(2);

    assert!(q
        .walk_indirect_table(&physmap, INDIRECT_TABLE_GPA, 17, q.size())
        .is_none());
}

#[test]
fn indirect_cycle_terminates_and_returns_none() {
    let q = VirtQueue::new(4);
    let physmap = indirect_physmap(3);
    for idx in 0..3 {
        write_indirect_desc(
            &physmap,
            idx,
            VirtqDesc {
                addr: 0x2000 + (idx as u64 * 0x1000),
                len: 64,
                flags: bits::VRING_DESC_F_NEXT,
                next: if idx == 2 { 0 } else { idx as u16 + 1 },
            },
        );
    }

    assert!(q
        .walk_indirect_table(&physmap, INDIRECT_TABLE_GPA, 3 * 16, q.size())
        .is_none());
}

// -- EVENT_IDX tests --

#[test]
fn vring_need_event_crossing_threshold() {
    // new_idx crosses event_idx: should interrupt
    // old=0, new=1, event=0: (1-0-1=0) < (1-0=1) → true
    assert!(vring_need_event(0, 1, 0));
    // old=5, new=10, event=7: (10-7-1=2) < (10-5=5) → true
    assert!(vring_need_event(7, 10, 5));
}

#[test]
fn vring_need_event_threshold_already_passed() {
    // event_idx already behind old_idx: no interrupt
    // old=5, new=10, event=4: (10-4-1=5) < (10-5=5) → false
    assert!(!vring_need_event(4, 10, 5));
    // old=0, new=1, event=1: (1-1-1=65535) < (1-0=1) → false
    assert!(!vring_need_event(1, 1, 0));
}

#[test]
fn vring_need_event_no_new_work() {
    // old == new (no work done): never interrupt
    assert!(!vring_need_event(3, 5, 5));
    assert!(!vring_need_event(0, 0, 0));
}

#[test]
fn vring_need_event_wrapping() {
    // Wrapping around u16::MAX
    // old=65535, new=0, event=65535: (0-65535-1=0) < (0-65535=1) → true
    assert!(vring_need_event(65535, 0, 65535));
    // old=65535, new=0, event=0: (0-0-1=65535) < (0-65535=1) → false
    assert!(!vring_need_event(0, 0, 65535));
    // old=65534, new=1, event=65535: (1-65535-1=1) < (1-65534=3) → true
    assert!(vring_need_event(65535, 1, 65534));
}

#[test]
fn vring_need_event_single_step() {
    // Exact threshold crossing: old=N, new=N+1, event=N → interrupt
    for n in [0u16, 1, 100, 255, 65534, 65535] {
        assert!(
            vring_need_event(n, n.wrapping_add(1), n),
            "should interrupt at threshold {n}",
        );
    }
}

// VirtIO 1.3 sec 2.7.10: after the device writes `used_event` it
// re-reads `avail_idx`, because the guest may have decided not to kick
// on the stale threshold it saw. Each of the three answers is read out
// of real guest memory: an early return that reported the wrong one
// would either re-walk a ring nobody posted to or drop the entries the
// guest posted in the race window, and the guest would wait for ever.
#[test]
fn has_new_avail_says_no_when_event_idx_is_off() {
    let (physmap, mut queue) =
        posted_queue(RING_GPA, RING_GPA + 0x100, RING_GPA + 0x200);
    queue.set_event_idx(false);

    // The guest has posted an entry the device has not seen, so the
    // only thing that can hold the answer down is the feature bit.
    assert_eq!(queue.last_avail_idx(), 0);
    assert!(!queue.has_new_avail(&physmap));
}

#[test]
fn has_new_avail_says_no_on_a_queue_the_driver_never_programmed() {
    let physmap =
        PhysMap::new_anon(RING_GPA, RING_BYTES).expect("create guest memory");
    let mut queue = VirtQueue::new(RING_DESCS);
    queue.set_event_idx(true);

    assert!(!queue.is_configured());
    assert!(!queue.has_new_avail(&physmap));
}

// The answer the other two are the exceptions to. Without this one a
// `has_new_avail` that always says no passes both of them.
#[test]
fn has_new_avail_sees_an_entry_the_device_has_not_walked() {
    let (physmap, mut queue) =
        posted_queue(RING_GPA, RING_GPA + 0x100, RING_GPA + 0x200);
    queue.set_event_idx(true);

    assert!(queue.has_new_avail(&physmap));
    assert_eq!(queue.pop_avail(&physmap), Some(0));
    assert!(!queue.has_new_avail(&physmap));
}

// -- Ring address validation --

const RING_GPA: u64 = 0x4000;
const RING_BYTES: usize = 0x1000;
const RING_DESCS: u16 = 16;

/// A queue whose descriptor 0 is one readable buffer, with that
/// descriptor posted once in the available ring.
fn posted_queue(desc: u64, avail: u64, used: u64) -> (PhysMap, VirtQueue) {
    let physmap =
        PhysMap::new_anon(RING_GPA, RING_BYTES).expect("create guest memory");
    let mut queue = VirtQueue::new(RING_DESCS);
    queue.set_addr_modern(desc, avail, used);

    physmap
        .lookup(RING_GPA, 16)
        .expect("mapped descriptor")
        .write(&VirtqDesc {
            addr: RING_GPA + 0x800,
            len: 64,
            flags: 0,
            next: 0,
        })
        .expect("write descriptor");
    physmap
        .lookup(avail + 2, 2)
        .expect("mapped avail index")
        .write::<u16>(&1)
        .expect("write avail index");
    physmap
        .lookup(avail + 4, 2)
        .expect("mapped avail entry")
        .write::<u16>(&0)
        .expect("write avail entry");
    (physmap, queue)
}

/// The used ring runs off the end of RAM, so no completion can ever
/// reach the guest. The queue must refuse it rather than pop work it
/// can never finish.
#[test]
fn a_used_ring_past_guest_memory_is_refused() {
    let (physmap, mut queue) =
        posted_queue(RING_GPA, RING_GPA + 0x100, RING_GPA + 0xF80);

    assert_eq!(queue.pop_avail(&physmap), None);
    assert_eq!(queue.last_avail_idx(), 0);
}

/// VirtIO 1.3 sec 2.7 puts the descriptor table on a 16 byte boundary.
/// Off it, every descriptor read fails its alignment check, so the
/// device would pop heads it can never walk.
#[test]
fn a_misaligned_descriptor_table_is_refused() {
    let (physmap, mut queue) =
        posted_queue(RING_GPA + 8, RING_GPA + 0x100, RING_GPA + 0x200);

    assert_eq!(queue.pop_avail(&physmap), None);
}

/// The rings a working guest programs must still be accepted.
#[test]
fn a_ring_inside_guest_memory_still_pops() {
    let (physmap, mut queue) =
        posted_queue(RING_GPA, RING_GPA + 0x100, RING_GPA + 0x200);

    assert_eq!(queue.pop_avail(&physmap), Some(0));
    assert_eq!(queue.last_avail_idx(), 1);
}

/// Counts records, so a test can show a guest cannot drive the log.
struct CountingDrain(Arc<std::sync::atomic::AtomicUsize>);

impl slog::Drain for CountingDrain {
    type Ok = ();
    type Err = slog::Never;

    fn log(
        &self,
        _record: &slog::Record<'_>,
        _values: &slog::OwnedKVList,
    ) -> Result<(), slog::Never> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// The refusal is reported, and reported once: a guest that keeps
/// kicking a broken ring must not be able to fill the log with it.
#[test]
fn a_refused_ring_is_logged_once_however_often_the_guest_kicks() {
    let (physmap, mut queue) =
        posted_queue(RING_GPA, RING_GPA + 0x100, RING_GPA + 0xF80);
    let records = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let log =
        slog::Logger::root(CountingDrain(Arc::clone(&records)), slog::o!());
    queue.set_log(log.clone());

    // slog drops `debug!` at compile time in a release build, so ask
    // this logger whether the level survives rather than assuming the
    // profile. The refusal is logged at debug on purpose: it is
    // guest-reachable, and a guest must not be able to fill the log.
    slog::debug!(log, "probe");
    let debug_reaches_the_drain = records.load(Ordering::Relaxed) > 0;
    records.store(0, Ordering::Relaxed);

    for _ in 0..64 {
        assert_eq!(queue.pop_avail(&physmap), None);
    }

    let seen = records.load(Ordering::Relaxed);
    // The property that holds in either profile: 64 kicks, not 64 records.
    assert!(seen <= 1, "64 kicks produced {seen} records");
    if debug_reaches_the_drain {
        assert_eq!(seen, 1, "the refusal was never reported");
    }
    assert!(queue.needs_reset());
}

/// The refusal lifts when the guest programs the rings again, so a
/// driver that resets and retries is not locked out.
#[test]
fn reprogramming_the_rings_clears_the_refusal() {
    let (physmap, mut queue) =
        posted_queue(RING_GPA, RING_GPA + 0x100, RING_GPA + 0xF80);
    assert_eq!(queue.pop_avail(&physmap), None);
    assert!(queue.needs_reset());

    queue.set_addr_modern(RING_GPA, RING_GPA + 0x100, RING_GPA + 0x200);

    assert!(!queue.needs_reset());
    assert_eq!(queue.pop_avail(&physmap), Some(0));
}

#[test]
fn validation_names_the_ring_that_is_wrong() {
    let physmap =
        PhysMap::new_anon(RING_GPA, RING_BYTES).expect("create guest memory");
    let mut queue = VirtQueue::new(RING_DESCS);

    queue.set_addr_modern(0, RING_GPA + 0x100, RING_GPA + 0x200);
    assert_eq!(
        queue.validate_addrs(&physmap),
        Err(QueueAddrError::Unset(Ring::Desc)),
    );

    queue.set_addr_modern(RING_GPA, RING_GPA + 0x101, RING_GPA + 0x200);
    assert_eq!(
        queue.validate_addrs(&physmap),
        Err(QueueAddrError::Misaligned {
            ring: Ring::Avail,
            addr: RING_GPA + 0x101,
        }),
    );

    queue.set_addr_modern(RING_GPA, RING_GPA + 0x100, RING_GPA + 0x202);
    assert_eq!(
        queue.validate_addrs(&physmap),
        Err(QueueAddrError::Misaligned {
            ring: Ring::Used,
            addr: RING_GPA + 0x202,
        }),
    );

    queue.set_addr_modern(RING_GPA, RING_GPA + 0x100, RING_GPA + 0x200);
    assert_eq!(queue.validate_addrs(&physmap), Ok(()));
}

/// `used_event` sits past the end of the available ring and only
/// EVENT_IDX makes the device read it. A ring that stops just short of
/// it works without the feature, so only the feature may refuse it.
#[test]
fn the_suppression_field_must_be_mapped_under_event_idx() {
    let avail = RING_GPA + RING_BYTES as u64 - 4 - u64::from(RING_DESCS) * 2;
    let (physmap, mut queue) = posted_queue(RING_GPA, avail, RING_GPA + 0x200);

    assert_eq!(queue.validate_addrs(&physmap), Ok(()));

    queue.set_event_idx(true);
    assert!(matches!(
        queue.validate_addrs(&physmap),
        Err(QueueAddrError::Unmapped {
            ring: Ring::Avail,
            ..
        }),
    ));
}

/// The verdict is cached, and negotiating EVENT_IDX moves what the
/// device reads, so the cache must be dropped with it. A guest that
/// writes DRIVER_FEATURE after QUEUE_ENABLE would otherwise keep a
/// verdict taken without the suppression field, and the transport
/// would never be told to refuse the queue.
#[test]
fn negotiating_event_idx_rechecks_a_ring_already_accepted() {
    let avail = RING_GPA + RING_BYTES as u64 - 4 - u64::from(RING_DESCS) * 2;
    let (physmap, mut queue) = posted_queue(RING_GPA, avail, RING_GPA + 0x200);

    // The first pop caches the verdict the transport later reads.
    assert_eq!(queue.pop_avail(&physmap), Some(0));
    assert!(!queue.needs_reset());

    queue.set_event_idx(true);

    assert_eq!(queue.pop_avail(&physmap), None);
    assert!(
        queue.needs_reset(),
        "the queue kept a verdict taken before the feature moved its reach"
    );
}

/// The legacy PFN layout must pass the same check: it is what the
/// bhyve guests in the field program, and EVENT_IDX is stripped from
/// that transport, so the rings carry no suppression field.
#[test]
fn the_legacy_pfn_layout_is_accepted() {
    const BASE: u64 = 0x8000;
    // What a driver allocates for 128 descriptors: two pages.
    let physmap = PhysMap::new_anon(BASE, 0x2000).expect("create guest memory");
    let mut queue = VirtQueue::new(128);
    queue.set_addr_legacy((BASE / PAGE_SIZE as u64) as u32);

    assert_eq!(queue.validate_addrs(&physmap), Ok(()));
}
