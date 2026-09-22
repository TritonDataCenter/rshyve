// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::ptr::NonNull;

use super::*;

/// Create a test SubMapping backed by anonymous mmap (no VmmHdl needed).
fn make_test_mapping(size: usize) -> Arc<Mapping> {
    Arc::new(Mapping::anon(size).unwrap())
}

#[test]
fn segid_alloc_stops_at_kernel_limit() {
    let alloc = SegidAlloc::new(3);
    assert_eq!(alloc.alloc(), Some(3));
    assert_eq!(alloc.alloc(), Some(4));
    assert_eq!(alloc.alloc(), None);

    let exhausted = SegidAlloc::new(VM_MAX_MEMSEGS);
    assert_eq!(exhausted.alloc(), None);
}

#[test]
fn copy_out_roundtrip_and_bounds() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let pattern: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    sub.write_bytes(&pattern).unwrap();

    let mut copied = vec![0; pattern.len()];
    sub.copy_out(&mut copied).unwrap();
    assert_eq!(copied, pattern);

    let mut oversized = vec![0; 4097];
    let err = sub.copy_out(&mut oversized).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[test]
fn copy_in_matches_write_bytes() {
    // copy_in is the fast path the boot loaders take. It must land the
    // same bytes as the volatile loop, or a kernel image loads corrupt.
    let pattern: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();

    let volatile_mapping = make_test_mapping(4096);
    let volatile_sub = SubMapping::new(&volatile_mapping);
    volatile_sub.write_bytes(&pattern).unwrap();
    let mut via_write_bytes = vec![0; pattern.len()];
    volatile_sub.copy_out(&mut via_write_bytes).unwrap();

    let bulk_mapping = make_test_mapping(4096);
    let bulk_sub = SubMapping::new(&bulk_mapping);
    bulk_sub.copy_in(&pattern).unwrap();
    let mut via_copy_in = vec![0; pattern.len()];
    bulk_sub.copy_out(&mut via_copy_in).unwrap();

    assert_eq!(via_copy_in, via_write_bytes);
    assert_eq!(via_copy_in, pattern);
}

#[test]
fn copy_in_rejects_oversized_source() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let err = sub.copy_in(&vec![0u8; 4097]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[test]
fn copy_in_checks_write_permission() {
    let mapping = make_test_mapping(4096);
    let mut write_only = SubMapping::new(&mapping);
    write_only.prot = Prot::WRITE;
    assert!(write_only.copy_in(&[0; 16]).is_ok());

    let mut read_only = SubMapping::new(&mapping);
    read_only.prot = Prot::READ;
    let err = read_only.copy_in(&[0; 16]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::PermissionDenied);
}

/// Write `payload` to a scratch file and hand back the open handle.
fn scratch_file(name: &str, payload: &[u8]) -> std::fs::File {
    use std::io::Write;
    let dir = std::env::temp_dir().join("vmm-core-mem-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(payload).unwrap();
    f.sync_all().unwrap();
    drop(f);
    std::fs::File::open(&path).unwrap()
}

#[test]
fn read_exact_from_fills_the_mapping() {
    let payload: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    let file = scratch_file("full.bin", &payload);

    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    sub.read_exact_from(&file, 0, payload.len()).unwrap();

    let mut got = vec![0; payload.len()];
    sub.copy_out(&mut got).unwrap();
    assert_eq!(got, payload);
}

#[test]
fn read_exact_from_honors_the_file_offset() {
    let payload: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();
    let file = scratch_file("offset.bin", &payload);

    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    sub.read_exact_from(&file, 512, 512).unwrap();

    let mut got = vec![0; 512];
    sub.copy_out(&mut got).unwrap();
    assert_eq!(got, &payload[512..1024]);
}

#[test]
fn read_exact_from_rejects_a_segment_past_end_of_file() {
    // A PT_LOAD claiming more bytes than its file holds must fail, not
    // fill part of the range and leave the tail as it was. Silently
    // short-filling would leak whatever the guest page held before.
    let file = scratch_file("short.bin", &[0xAB; 100]);

    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let err = sub.read_exact_from(&file, 0, 200).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
}

#[test]
fn read_exact_from_rejects_a_length_past_the_mapping() {
    let file = scratch_file("big.bin", &[0xCD; 8192]);

    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let err = sub.read_exact_from(&file, 0, 4097).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[test]
fn read_exact_from_checks_write_permission() {
    let file = scratch_file("perm.bin", &[0xEF; 64]);

    let mapping = make_test_mapping(4096);
    let mut read_only = SubMapping::new(&mapping);
    read_only.prot = Prot::READ;
    let err = read_only.read_exact_from(&file, 0, 64).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::PermissionDenied);
}

#[test]
fn copy_out_checks_read_permission() {
    let mapping = make_test_mapping(4096);
    let mut read_only = SubMapping::new(&mapping);
    read_only.prot = Prot::READ;
    assert!(read_only.copy_out(&mut [0; 16]).is_ok());

    let mut write_only = SubMapping::new(&mapping);
    write_only.prot = Prot::WRITE;
    let err = write_only.copy_out(&mut [0; 16]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::PermissionDenied);
}

#[test]
fn anonymous_devmem_segment_is_guestless() {
    let segment = DevMemSeg::new_anon(0x1000).unwrap();
    assert_eq!(segment.len(), 0x1000);
    assert_eq!(segment.mapped_gpa(), None);
    segment.map_at(0x8000_0000).unwrap();
    assert_eq!(segment.mapped_gpa(), None);
    segment.unmap().unwrap();
    assert_eq!(segment.mapped_gpa(), None);

    let data = [0x12, 0x34, 0x56, 0x78];
    let view = segment.view();
    view.write_bytes(&data).unwrap();
    let mut copied = [0; 4];
    view.copy_out(&mut copied).unwrap();
    assert_eq!(copied, data);
}

#[test]
fn read_aligned_u32() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let val = sub.read::<u32>();
    assert!(val.is_ok());
}

#[test]
fn read_unaligned_u32_fails() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    // Offset by 1 byte to create misalignment for u32 (align=4)
    let unaligned = sub.subregion(1, 4).unwrap();
    let result = unaligned.read::<u32>();
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
}

#[test]
fn write_unaligned_u32_fails() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let unaligned = sub.subregion(1, 4).unwrap();
    let result = unaligned.write::<u32>(&42);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
}

#[test]
fn read_aligned_u16_at_offset() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    // Offset by 2 is still aligned for u16 (align=2)
    let aligned = sub.subregion(2, 2).unwrap();
    assert!(aligned.read::<u16>().is_ok());
}

#[test]
fn read_unaligned_u16_fails() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    // Offset by 1 is misaligned for u16 (align=2)
    let unaligned = sub.subregion(1, 2).unwrap();
    assert!(unaligned.read::<u16>().is_err());
}

#[test]
fn read_u8_always_aligned() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    // u8 has align=1, so any offset works
    let at_offset_1 = sub.subregion(1, 1).unwrap();
    assert!(at_offset_1.read::<u8>().is_ok());
}

#[test]
fn read_write_bytes_ignores_alignment() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let unaligned = sub.subregion(1, 4).unwrap();
    let mut buf = [0u8; 4];
    assert!(unaligned.read_bytes(&mut buf).is_ok());
    assert!(unaligned.write_bytes(&[1, 2, 3, 4]).is_ok());
}

#[test]
fn read_write_roundtrip() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    sub.write::<u32>(&0xDEAD_BEEF).unwrap();
    let val = sub.read::<u32>().unwrap();
    assert_eq!(val, 0xDEAD_BEEF);
}

#[test]
fn subregion_bounds_check() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    // Exactly at end: should succeed
    assert!(sub.subregion(4092, 4).is_some());
    // Past end: should fail
    assert!(sub.subregion(4093, 4).is_none());
    // Overflow: should fail
    assert!(sub.subregion(usize::MAX, 1).is_none());
}

#[test]
fn read_too_small_fails() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let tiny = sub.subregion(0, 2).unwrap();
    assert!(tiny.read::<u32>().is_err());
}

#[test]
fn write_u32_too_small_fails() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let tiny = sub.subregion(0, 2).unwrap();
    let result = tiny.write::<u32>(&0xDEAD);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
}

#[test]
fn read_u16_aligned_roundtrip() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let region = sub.subregion(0, 2).unwrap();
    region.write::<u16>(&0xBEEF).unwrap();
    let val = region.read::<u16>().unwrap();
    assert_eq!(val, 0xBEEF);
}

#[test]
fn read_u16_at_aligned_offset_roundtrip() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    // Offset 4 is aligned for u16
    let region = sub.subregion(4, 2).unwrap();
    region.write::<u16>(&0xCAFE).unwrap();
    let val = region.read::<u16>().unwrap();
    assert_eq!(val, 0xCAFE);
}

#[test]
fn write_bytes_read_bytes_roundtrip() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let data = [0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE];
    sub.write_bytes(&data).unwrap();
    let mut buf = [0u8; 8];
    sub.read_bytes(&mut buf).unwrap();
    assert_eq!(buf, data);
}

#[test]
fn write_bytes_read_bytes_at_offset() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    // Write at an unaligned offset via subregion
    let region = sub.subregion(3, 5).unwrap();
    let data = [1, 2, 3, 4, 5];
    region.write_bytes(&data).unwrap();
    let mut buf = [0u8; 5];
    region.read_bytes(&mut buf).unwrap();
    assert_eq!(buf, data);
}

#[test]
fn read_bytes_exceeds_bounds() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let tiny = sub.subregion(0, 4).unwrap();
    let mut buf = [0u8; 8];
    let result = tiny.read_bytes(&mut buf);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
}

#[test]
fn write_bytes_exceeds_bounds() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let tiny = sub.subregion(0, 4).unwrap();
    let result = tiny.write_bytes(&[0u8; 8]);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
}

#[test]
fn subregion_exact_end() {
    let mapping = make_test_mapping(256);
    let sub = SubMapping::new(&mapping);
    // Exactly the full mapping
    assert!(sub.subregion(0, 256).is_some());
    // One byte past the end
    assert!(sub.subregion(0, 257).is_none());
    // Last byte
    assert!(sub.subregion(255, 1).is_some());
    // Zero-length at end
    assert!(sub.subregion(256, 0).is_some());
}

#[test]
fn subregion_nested() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let outer = sub.subregion(100, 200).unwrap();
    let inner = outer.subregion(50, 100).unwrap();
    assert_eq!(inner.len(), 100);
    // Inner subregion cannot exceed outer
    assert!(outer.subregion(150, 100).is_none());
}

#[test]
fn read_from_write_only_mapping_fails() {
    let raw = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    assert_ne!(raw, libc::MAP_FAILED);
    let ptr = NonNull::new(raw as *mut u8).unwrap();
    let mapping = Arc::new(Mapping {
        ptr,
        len: 4096,
        prot: Prot::WRITE,
    });
    let sub = SubMapping::new(&mapping);
    let result = sub.read::<u32>();
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().kind(), ErrorKind::PermissionDenied);
}

#[test]
fn write_to_read_only_mapping_fails() {
    let raw = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    assert_ne!(raw, libc::MAP_FAILED);
    let ptr = NonNull::new(raw as *mut u8).unwrap();
    let mapping = Arc::new(Mapping {
        ptr,
        len: 4096,
        prot: Prot::READ,
    });
    let sub = SubMapping::new(&mapping);
    let result = sub.write::<u32>(&42);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().kind(), ErrorKind::PermissionDenied);
}

#[test]
fn len_and_is_empty() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    assert_eq!(sub.len(), 4096);
    assert!(!sub.is_empty());

    let zero = sub.subregion(0, 0).unwrap();
    assert_eq!(zero.len(), 0);
    assert!(zero.is_empty());
}

#[test]
fn phys_map_empty() {
    let map = PhysMap::new();
    assert_eq!(map.num_regions(), 0);
    assert_eq!(map.total_memory(), 0);
    assert!(map.regions().is_empty());
    assert!(map.lookup(0, 1).is_none());
    assert!(map.lookup(0x1000, 4096).is_none());
}

/// Add a region without a kernel VM, which no test host outside illumos has.
fn insert_ram(map: &PhysMap, gpa: u64, len: usize) -> Result<()> {
    map.add_region_anon(gpa, len, RegionKind::Ram)
}

#[test]
fn region_accounting_skips_mmio() {
    let map = PhysMap::new();
    insert_ram(&map, 0, 0x2000).unwrap();
    map.add_region_anon(0x4000, 0x1000, RegionKind::Mmio)
        .unwrap();
    map.add_region_anon(0x8000, 0x1000, RegionKind::Rom)
        .unwrap();

    assert_eq!(map.num_regions(), 3);
    assert_eq!(map.total_memory(), 0x3000);
    assert_eq!(
        map.regions(),
        vec![
            (0, 0x2000, RegionKind::Ram),
            (0x4000, 0x1000, RegionKind::Mmio),
            (0x8000, 0x1000, RegionKind::Rom),
        ]
    );
    assert!(map.lookup(0x8000, 0x1000).is_some());
    assert!(map.lookup(0x2000, 1).is_none());
}

#[test]
fn overlapping_region_is_rejected() {
    let map = PhysMap::new();
    insert_ram(&map, 0x1000, 0x1000).unwrap();

    let err = insert_ram(&map, 0x1800, 0x1000).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    assert_eq!(map.num_regions(), 1);

    // Abutting ranges do not overlap.
    insert_ram(&map, 0x2000, 0x1000).unwrap();
    assert_eq!(map.num_regions(), 2);
}

#[test]
fn concurrent_overlapping_adds_admit_exactly_one() {
    // The overlap check and the insert must be one write-lock section.
    // Two adds that both pass the check would leave the region list with
    // aliasing entries, so `lookup` could hand out the wrong mapping.
    let map = Arc::new(PhysMap::new());
    let barrier = Arc::new(std::sync::Barrier::new(2));

    let handles: Vec<_> = [0x1000u64, 0x1800]
        .into_iter()
        .map(|gpa| {
            let map = Arc::clone(&map);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                insert_ram(&map, gpa, 0x1000)
            })
        })
        .collect();

    let results: Vec<Result<()>> =
        handles.into_iter().map(|h| h.join().unwrap()).collect();

    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    let err = results.into_iter().find_map(|r| r.err()).unwrap();
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    assert_eq!(map.num_regions(), 1);
}

#[test]
fn lookup_runs_while_a_region_is_added() {
    // `lookup` is on the guest memory path. It must not hold the region
    // lock across the returned SubMapping, or a concurrent add deadlocks
    // every vCPU.
    const LOOKUPS: usize = 2000;
    const READERS: usize = 2;

    let map = Arc::new(PhysMap::new());
    insert_ram(&map, 0, 0x1000).unwrap();

    let barrier = Arc::new(std::sync::Barrier::new(READERS + 1));
    let (done_tx, done_rx) = std::sync::mpsc::channel();

    for _ in 0..READERS {
        let map = Arc::clone(&map);
        let barrier = Arc::clone(&barrier);
        let done = done_tx.clone();
        std::thread::spawn(move || {
            barrier.wait();
            for _ in 0..LOOKUPS {
                let sub = map.lookup(0, 16).expect("region 0 is mapped");
                assert_eq!(sub.len(), 16);
            }
            let _ = done.send(());
        });
    }
    drop(done_tx);

    barrier.wait();
    for i in 1..=8u64 {
        insert_ram(&map, i * 0x1000, 0x1000).unwrap();
    }

    // Join by channel so a deadlocked reader fails the test instead of
    // hanging the run forever.
    for _ in 0..READERS {
        done_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("lookup deadlocked against a concurrent add");
    }
    assert_eq!(map.num_regions(), 9);
    assert_eq!(map.total_memory(), 9 * 0x1000);
}

#[test]
fn a_claim_holds_its_range_until_it_is_dropped() {
    let map = PhysMap::new();
    let claim = map.claim_ram(0x1_0000_0000, 0x10_0000).unwrap();
    assert_eq!(claim.gpa(), 0x1_0000_0000);
    // No region yet: the memory is not there until the claim commits.
    assert_eq!(map.num_regions(), 0);
    assert!(map.lookup(0x1_0000_0000, 16).is_none());

    let err = map.claim_ram(0x1_0008_0000, 0x10_0000).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    // A published region is refused against a claim too.
    let err = insert_ram(&map, 0x1_0000_0000, 0x1000).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);

    drop(claim);
    let retaken = map.claim_ram(0x1_0008_0000, 0x10_0000).unwrap();
    assert_eq!(retaken.gpa(), 0x1_0008_0000);
}

#[test]
fn concurrent_runtime_claims_admit_exactly_one() {
    // Two hot-adds that both passed the overlap check would map two
    // segments at one guest address.
    let map = Arc::new(PhysMap::new());
    let barrier = Arc::new(std::sync::Barrier::new(2));

    let handles: Vec<_> = [0x1_0000_0000u64, 0x1_0000_8000]
        .into_iter()
        .map(|gpa| {
            let map = Arc::clone(&map);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                // Keep the claim alive: a claim that is dropped at once
                // would let the loser through and pass by luck.
                map.claim_ram(gpa, 0x1_0000).map(std::mem::forget)
            })
        })
        .collect();

    let results: Vec<Result<()>> =
        handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    let err = results.into_iter().find_map(|r| r.err()).unwrap();
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
}

#[test]
fn a_runtime_claim_refuses_the_mmio_hole() {
    let map = PhysMap::new();

    for (gpa, len) in [
        // Wholly inside the hole.
        (MMIO_HOLE_BASE, 0x10_0000usize),
        // The boot ROM window, at the top of the hole.
        (0xFFE2_0000, 0x1E_0000),
        // The firmware scratch RAM below the ROM.
        (0xFCC0_0000, 0x40_0000),
        // Crossing into the hole from below.
        (MMIO_HOLE_BASE - 0x1000, 0x2000),
        // Crossing out of the hole at 4 GiB.
        (MMIO_HOLE_END - 0x1000, 0x2000),
    ] {
        let err = map.claim_ram(gpa, len).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput, "{gpa:#x}");
    }

    // Abutting the hole on either side is fine.
    let below = map.claim_ram(MMIO_HOLE_BASE - 0x1000, 0x1000).unwrap();
    let above = map.claim_ram(MMIO_HOLE_END, 0x1000).unwrap();
    assert_eq!(below.gpa(), MMIO_HOLE_BASE - 0x1000);
    assert_eq!(above.gpa(), MMIO_HOLE_END);
}

#[test]
fn a_runtime_claim_refuses_a_range_the_kernel_would_refuse() {
    let map = PhysMap::new();

    assert_eq!(
        map.claim_ram(0x1_0000_0000, 0).unwrap_err().kind(),
        ErrorKind::InvalidInput,
    );
    assert_eq!(
        map.claim_ram(u64::MAX - 0x1000, 0x2000).unwrap_err().kind(),
        ErrorKind::InvalidInput,
    );
    // Page alignment, which `vm_mmap_memseg` answers with EINVAL.
    assert_eq!(
        map.claim_ram(0x1_0000_0800, 0x1000).unwrap_err().kind(),
        ErrorKind::InvalidInput,
    );
    assert_eq!(
        map.claim_ram(0x1_0000_0000, 0x800).unwrap_err().kind(),
        ErrorKind::InvalidInput,
    );
}

/// Every entry point that takes a range has to refuse one already
/// taken. A second rule would drift from the first, and the run-time
/// path is the one a control socket can reach.
#[test]
fn every_path_that_takes_a_range_refuses_one_already_taken() {
    const TAKEN: u64 = 0x2000_0000;
    const LEN: usize = 0x2000;
    // Overlaps the tail of the claimed range rather than matching it,
    // so an equality check would not be enough to refuse it.
    const OVERLAP: u64 = TAKEN + 0x1000;

    let refused = |what: &str, r: Result<()>| {
        let err = r.expect_err("a claimed range was taken twice");
        assert_eq!(
            err.kind(),
            ErrorKind::AlreadyExists,
            "{what} refused for the wrong reason",
        );
    };

    let map = PhysMap::new();
    map.add_region_anon(TAKEN, LEN, RegionKind::Ram)
        .expect("first claim");

    refused(
        "add_region_anon",
        map.add_region_anon(OVERLAP, LEN, RegionKind::Ram),
    );
    refused("claim_ram", map.claim_ram(OVERLAP, LEN).map(|_| ()));
}

#[test]
fn a_claim_over_published_ram_is_refused() {
    let map = PhysMap::new();
    insert_ram(&map, 0x1_0000_0000, 0x1_0000).unwrap();

    let err = map.claim_ram(0x1_0000_0000, 0x1000).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    let past = map.claim_ram(0x1_0001_0000, 0x1000).unwrap();
    assert_eq!(past.gpa(), 0x1_0001_0000);
}

#[test]
fn a_segid_that_was_never_used_goes_back() {
    let alloc = SegidAlloc::new(0);
    let first = alloc.alloc().unwrap();
    assert!(alloc.release(first));
    assert_eq!(alloc.alloc(), Some(first));

    // Only the last one handed out comes back.
    let second = alloc.alloc().unwrap();
    assert!(!alloc.release(first));
    assert!(alloc.release(second));

    // A release cannot push the counter past the kernel limit.
    let full = SegidAlloc::new(VM_MAX_MEMSEGS);
    assert_eq!(full.alloc(), None);
    assert!(full.release(VM_MAX_MEMSEGS - 1));
    assert_eq!(full.alloc(), Some(VM_MAX_MEMSEGS - 1));
    assert!(!full.release(i32::MAX));
}

#[test]
fn lookup_hands_out_the_tracked_view() {
    // Every device write to guest RAM goes through `lookup`. The kernel
    // marks a page dirty only when the write goes through the guest
    // physical mapping, so a `lookup` that hands out the devmem mapping
    // loses that page from a live migration.
    let map = PhysMap::new();
    map.add_region_split_anon(0, 0x2000).unwrap();

    map.lookup(0, 4)
        .unwrap()
        .write_bytes(&[1, 2, 3, 4])
        .unwrap();

    let mut tracked = [0u8; 4];
    map.lookup(0, 4).unwrap().read_bytes(&mut tracked).unwrap();
    assert_eq!(tracked, [1, 2, 3, 4]);

    // The two views of a split anonymous region do not alias, which is
    // what lets a test tell them apart. A real VM maps the same pages
    // twice, so both views read the same bytes there.
    let mut direct = [0u8; 4];
    map.lookup_untracked(0, 4)
        .unwrap()
        .read_bytes(&mut direct)
        .unwrap();
    assert_eq!(direct, [0, 0, 0, 0]);
}

#[test]
fn the_boot_loaders_write_through_the_untracked_view() {
    // `write_bulk` and `load_from_file` run before any vCPU starts, so
    // they have nothing to report to migration and take the cheaper
    // devmem mapping.
    let map = Arc::new(PhysMap::new());
    map.add_region_split_anon(0, 0x2000).unwrap();
    let mem = MemCtx::new(Arc::clone(&map));

    mem.write_bulk(0, &[7; 8]).unwrap();

    let mut direct = [0u8; 8];
    map.lookup_untracked(0, 8)
        .unwrap()
        .read_bytes(&mut direct)
        .unwrap();
    assert_eq!(direct, [7; 8]);

    let mut tracked = [0u8; 8];
    map.lookup(0, 8).unwrap().read_bytes(&mut tracked).unwrap();
    assert_eq!(tracked, [0; 8]);
}

#[test]
fn a_bulk_read_of_guest_memory_leaves_the_tracked_view_alone() {
    // A read fault on the tracked mapping marks the page dirty, because
    // `segvmm_fault_space` holds every page writable. The migration
    // source reads all of RAM each pass, so a tracked read would report
    // all of RAM dirty again and the migration would never converge.
    let map = Arc::new(PhysMap::new());
    map.add_region_split_anon(0, 0x2000).unwrap();
    let mem = MemCtx::new(Arc::clone(&map));

    map.lookup(0, 4)
        .unwrap()
        .write_bytes(&[9, 9, 9, 9])
        .unwrap();
    map.lookup_untracked(0, 4)
        .unwrap()
        .write_bytes(&[1, 2, 3, 4])
        .unwrap();

    let mut got = [0u8; 4];
    mem.read(0, &mut got).unwrap();
    assert_eq!(got, [1, 2, 3, 4]);
}

#[test]
fn a_rom_region_refuses_writes_through_lookup() {
    // A guest cannot write its ROM, so a device it drives must not be
    // able to DMA into it either. The bootrom loader takes the
    // untracked view, which stays writable.
    let map = PhysMap::new();
    map.add_region_anon(0x1000, 0x1000, RegionKind::Rom)
        .unwrap();

    let err = map
        .lookup(0x1000, 4)
        .unwrap()
        .write_bytes(&[1, 2, 3, 4])
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::PermissionDenied);
    let err = map
        .lookup(0x1000, 4)
        .unwrap()
        .copy_in(&[1, 2, 3, 4])
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::PermissionDenied);

    map.lookup_untracked(0x1000, 4)
        .unwrap()
        .write_bytes(&[1, 2, 3, 4])
        .unwrap();

    let mut got = [0u8; 4];
    map.lookup(0x1000, 4).unwrap().read_bytes(&mut got).unwrap();
    assert_eq!(got, [1, 2, 3, 4]);
}

#[test]
fn a_ram_region_stays_writable_through_lookup() {
    let map = PhysMap::new();
    map.add_region_anon(0x1000, 0x1000, RegionKind::Ram)
        .unwrap();

    map.lookup(0x1000, 4)
        .unwrap()
        .write_bytes(&[1, 2, 3, 4])
        .unwrap();
}

#[test]
fn lookup_answers_a_range_across_two_abutting_ram_regions() {
    // Hot-added RAM abuts the region below it, and one slot abuts the
    // next. A guest buffer over the seam is ordinary: `blk_rq_map_sg`
    // merges physically adjacent pages into one scatter-gather entry.
    // A `lookup` that refused it would make virtio-blk report a disk
    // fault for legal memory.
    let map = PhysMap::new();
    insert_ram(&map, 0x1_0000_0000, 0x2000).unwrap();
    insert_ram(&map, 0x1_0000_2000, 0x2000).unwrap();

    let across = map
        .lookup(0x1_0000_1000, 0x2000)
        .expect("a buffer over the seam is legal memory");
    across.write_bytes(&[0xAB; 0x2000]).unwrap();

    // The bytes past the seam belong to the second region.
    let mut got = [0u8; 4];
    map.lookup(0x1_0000_2000, 4)
        .unwrap()
        .read_bytes(&mut got)
        .unwrap();
    assert_eq!(got, [0xAB; 4]);
}

#[test]
fn a_region_added_later_joins_the_run_below_it() {
    // Each hot-add slot abuts the one before it, so the run has to grow
    // with every add and not only on the first.
    let map = PhysMap::new();
    insert_ram(&map, 0x1_0000_0000, 0x1000).unwrap();
    insert_ram(&map, 0x1_0000_1000, 0x1000).unwrap();
    insert_ram(&map, 0x1_0000_2000, 0x1000).unwrap();

    assert!(map.lookup(0x1_0000_0000, 0x3000).is_some());
    assert!(map.lookup(0x1_0000_0800, 0x2000).is_some());
    // Nothing is mapped past the last region.
    assert!(map.lookup(0x1_0000_2800, 0x1000).is_none());
}

#[test]
fn a_region_added_below_a_run_takes_the_run_with_it() {
    // Boot maps low memory first, but nothing promises that order.
    let map = PhysMap::new();
    insert_ram(&map, 0x1_0000_1000, 0x1000).unwrap();
    insert_ram(&map, 0x1_0000_2000, 0x1000).unwrap();
    insert_ram(&map, 0x1_0000_0000, 0x1000).unwrap();

    assert!(map.lookup(0x1_0000_0000, 0x3000).is_some());
}

#[test]
fn a_run_stops_at_a_region_of_another_kind() {
    // Only RAM regions share one mapping. A ROM carries the guest's
    // read-only protection and a reservation has no backing at all.
    let map = PhysMap::new();
    insert_ram(&map, 0x1_0000_0000, 0x1000).unwrap();
    map.add_region_anon(0x1_0000_1000, 0x1000, RegionKind::Rom)
        .unwrap();
    insert_ram(&map, 0x1_0000_2000, 0x1000).unwrap();

    assert!(map.lookup(0x1_0000_0800, 0x1000).is_none());
    assert!(map.lookup(0x1_0000_1800, 0x1000).is_none());
}

#[test]
fn the_untracked_view_stops_at_the_region_boundary() {
    // The devmem mapping is one kernel memory segment. It cannot span
    // two of them, so this view stops at the region end.
    let map = PhysMap::new();
    insert_ram(&map, 0x1_0000_0000, 0x1000).unwrap();
    insert_ram(&map, 0x1_0000_1000, 0x1000).unwrap();

    assert!(map.lookup(0x1_0000_0800, 0x1000).is_some());
    assert!(map.lookup_untracked(0x1_0000_0800, 0x1000).is_none());
}

#[test]
fn two_adds_of_abutting_ram_still_end_in_one_run() {
    // The run is read and adopted under one gate. Two adds that both
    // read the region list before either published would each build a
    // run of one, and the seam between them would refuse a lookup.
    let map = Arc::new(PhysMap::new());
    let barrier = Arc::new(std::sync::Barrier::new(2));

    let handles: Vec<_> = [0x1_0000_1000u64, 0x1_0000_2000]
        .into_iter()
        .map(|gpa| {
            let map = Arc::clone(&map);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                insert_ram(&map, gpa, 0x1000)
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap().unwrap();
    }

    assert!(map.lookup(0x1_0000_1800, 0x1000).is_some());
}
