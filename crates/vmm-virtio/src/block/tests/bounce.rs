// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests for the host buffer a request is bounced through.
//!
//! The buffer is what takes guest memory out of the syscall. A stalled
//! `pwrite` resumes against host bytes, so it cannot put the next
//! tenant of a recycled page on the disk, and a stalled `pread` cannot
//! fill one.

use std::io::Write;
use std::os::unix::fs::FileExt;

use super::*;

const HDR_GPA: u64 = 0x2000;
const STATUS_GPA: u64 = 0x2100;
/// Stands in for a page the driver frees and the guest reuses.
const RECYCLED_GPA: u64 = 0x3000;
const SPARE_GPA: u64 = 0x3400;
const SECTOR: usize = 512;

fn guest_memory() -> Arc<PhysMap> {
    Arc::new(PhysMap::new_anon(0x1000, 0x8000).expect("guest memory"))
}

fn peek(physmap: &PhysMap, gpa: u64, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    physmap
        .lookup(gpa, len)
        .expect("mapped guest range")
        .read_bytes(&mut out)
        .expect("read guest range");
    out
}

/// Header, one data segment, status byte.
fn chain(data: u64, len: u32, writable: bool) -> Vec<ChainBuf> {
    let payload = if writable {
        ChainBuf::Writable { addr: data, len }
    } else {
        ChainBuf::Readable { addr: data, len }
    };
    vec![
        ChainBuf::Readable {
            addr: HDR_GPA,
            len: BLK_REQ_HEADER_SIZE as u32,
        },
        payload,
        ChainBuf::Writable {
            addr: STATUS_GPA,
            len: 1,
        },
    ]
}

fn request(
    ctx: &WorkerCtx,
    physmap: &Arc<PhysMap>,
    rtype: u32,
    sector: u64,
    chain: Vec<ChainBuf>,
) -> BlkIoRequest {
    let mut queue = VirtQueue::new(8);
    queue.set_addr_modern(0x1000, 0x1100, 0x1200);
    BlkIoRequest {
        rtype,
        sector,
        chain,
        head: 0,
        completion: detached_completion(&queue, physmap),
        tag: current_tag(&ctx.access, 0),
    }
}

fn backing(bytes: &[u8]) -> File {
    let mut file = tempfile::tempfile().expect("create tempfile");
    file.write_all(bytes).expect("fill the backing file");
    file.flush().expect("flush the backing file");
    file
}

fn read_backing(file: &File, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    file.read_exact_at(&mut out, 0).expect("read back");
    out
}

fn writable_disk(capacity: u64) -> DiskLimits {
    DiskLimits {
        capacity,
        read_only: false,
        nodelete: false,
    }
}

// The decisive write case. The driver recycles the data page between
// the snapshot and the syscall. The disk must hold what the guest
// asked to write, not what the page's next tenant put there.
#[test]
fn a_write_reaches_the_disk_from_its_snapshot() {
    let physmap = guest_memory();
    physmap
        .lookup(RECYCLED_GPA, SECTOR)
        .expect("mapped data page")
        .write_bytes(&[0xAA; SECTOR])
        .expect("stage the payload");
    let ctx = worker_ctx(&physmap);
    let file = backing(&[0u8; SECTOR]);

    {
        let physmap = Arc::clone(&physmap);
        *ctx.parks.at_backend.lock().expect("park lock") =
            Some(Arc::new(move || {
                physmap
                    .lookup(RECYCLED_GPA, SECTOR)
                    .expect("mapped data page")
                    .write_bytes(&[0xBB; SECTOR])
                    .expect("recycle the page");
            }));
    }

    let req = request(
        &ctx,
        &physmap,
        bits::VIRTIO_BLK_T_OUT,
        0,
        chain(RECYCLED_GPA, SECTOR as u32, false),
    );
    let done =
        run_request(&file, &writable_disk(1), &ctx, &mut Bounce::new(), &req);

    assert_eq!(done, Some((0, 1)));
    assert_eq!(
        read_backing(&file, SECTOR),
        vec![0xAA; SECTOR],
        "the write read the recycled page instead of its own snapshot"
    );
}

// The decisive read case. A read parked at the disk while a reset
// lands and the guest reuses the page must copy nothing back.
#[test]
fn a_read_parked_across_a_reset_lands_nothing_in_a_recycled_page() {
    let physmap = guest_memory();
    let ctx = worker_ctx(&physmap);
    let file = backing(&[0xAB; SECTOR]);

    {
        let physmap = Arc::clone(&physmap);
        let session = ctx.clone();
        *ctx.parks.at_backend.lock().expect("park lock") =
            Some(Arc::new(move || {
                end_session(&session);
                physmap
                    .lookup(RECYCLED_GPA, SECTOR)
                    .expect("mapped data page")
                    .write_bytes(&[0xBB; SECTOR])
                    .expect("recycle the page");
            }));
    }

    let req = request(
        &ctx,
        &physmap,
        bits::VIRTIO_BLK_T_IN,
        0,
        chain(RECYCLED_GPA, SECTOR as u32, true),
    );
    let done =
        run_request(&file, &writable_disk(1), &ctx, &mut Bounce::new(), &req);

    assert!(done.is_none(), "a closed session published a used entry");
    assert_eq!(
        peek(&physmap, RECYCLED_GPA, SECTOR),
        vec![0xBB; SECTOR],
        "the read wrote disk data into a recycled page"
    );
}

// The buffer is reused, so a request that cannot fill it must copy
// none of it. Otherwise the guest gets the last request's data.
#[test]
fn a_short_read_copies_nothing_into_the_guest() {
    let physmap = guest_memory();
    physmap
        .lookup(SPARE_GPA, SECTOR)
        .expect("mapped data page")
        .write_bytes(&[0x11; SECTOR])
        .expect("stage the second page");
    let ctx = worker_ctx(&physmap);
    // Two sectors of capacity, one sector of file: the second read
    // passes the range check and then runs off the end.
    let file = backing(&[0xAB; SECTOR]);
    let limits = writable_disk(2);
    let mut bounce = Bounce::new();

    let first = request(
        &ctx,
        &physmap,
        bits::VIRTIO_BLK_T_IN,
        0,
        chain(RECYCLED_GPA, SECTOR as u32, true),
    );
    assert_eq!(
        run_request(&file, &limits, &ctx, &mut bounce, &first),
        Some((0, SECTOR as u32 + 1))
    );
    assert_eq!(peek(&physmap, RECYCLED_GPA, SECTOR), vec![0xAB; SECTOR]);

    let second = request(
        &ctx,
        &physmap,
        bits::VIRTIO_BLK_T_IN,
        1,
        chain(SPARE_GPA, SECTOR as u32, true),
    );
    assert_eq!(
        run_request(&file, &limits, &ctx, &mut bounce, &second),
        Some((0, 1)),
        "a read past the end of the file must report only a status byte"
    );
    assert_eq!(
        peek(&physmap, STATUS_GPA, 1),
        vec![bits::VIRTIO_BLK_S_IOERR]
    );
    assert_eq!(
        peek(&physmap, SPARE_GPA, SECTOR),
        vec![0x11; SECTOR],
        "the short read copied the last request's bytes into the guest"
    );
}

// A request larger than the buffer is split, and every byte of every
// segment must still reach its own place.
#[test]
fn a_request_past_the_buffer_moves_every_byte() {
    const SEG: usize = 128 * 1024;
    const SEGS: usize = 3;
    const TOTAL: usize = SEG * SEGS;
    assert!(TOTAL > io::BOUNCE_BYTES, "the split path is not exercised");

    let physmap =
        Arc::new(PhysMap::new_anon(0x100000, 0x100000).expect("guest memory"));
    let base = 0x100000u64;
    let hdr = base;
    let status = base + 0x1000;
    let data = base + 0x10000;

    // A pattern that changes across every segment boundary.
    let disk: Vec<u8> = (0..TOTAL).map(|i| (i / 4096) as u8).collect();
    let file = backing(&disk);

    let ctx = worker_ctx(&physmap);
    let limits = writable_disk((TOTAL / 512) as u64);
    let mut queue = VirtQueue::new(8);
    queue.set_addr_modern(base + 0x2000, base + 0x3000, base + 0x4000);

    let read_chain: Vec<ChainBuf> = std::iter::once(ChainBuf::Readable {
        addr: hdr,
        len: BLK_REQ_HEADER_SIZE as u32,
    })
    .chain((0..SEGS).map(|i| ChainBuf::Writable {
        addr: data + (i * SEG) as u64,
        len: SEG as u32,
    }))
    .chain(std::iter::once(ChainBuf::Writable {
        addr: status,
        len: 1,
    }))
    .collect();

    let req = BlkIoRequest {
        rtype: bits::VIRTIO_BLK_T_IN,
        sector: 0,
        chain: read_chain,
        head: 7,
        completion: detached_completion(&queue, &physmap),
        tag: current_tag(&ctx.access, 0),
    };
    let mut bounce = Bounce::new();
    assert_eq!(
        run_request(&file, &limits, &ctx, &mut bounce, &req),
        Some((7, TOTAL as u32 + 1))
    );
    assert_eq!(peek(&physmap, data, TOTAL), disk, "a split read lost bytes");

    // Write the same bytes back to a fresh file through the same split.
    let out = backing(&vec![0u8; TOTAL]);
    let write_chain: Vec<ChainBuf> = std::iter::once(ChainBuf::Readable {
        addr: hdr,
        len: BLK_REQ_HEADER_SIZE as u32,
    })
    .chain((0..SEGS).map(|i| ChainBuf::Readable {
        addr: data + (i * SEG) as u64,
        len: SEG as u32,
    }))
    .chain(std::iter::once(ChainBuf::Writable {
        addr: status,
        len: 1,
    }))
    .collect();
    let req = BlkIoRequest {
        rtype: bits::VIRTIO_BLK_T_OUT,
        sector: 0,
        chain: write_chain,
        head: 8,
        completion: detached_completion(&queue, &physmap),
        tag: current_tag(&ctx.access, 0),
    };
    assert_eq!(
        run_request(&out, &limits, &ctx, &mut bounce, &req),
        Some((8, 1))
    );
    assert_eq!(read_backing(&out, TOTAL), disk, "a split write lost bytes");
}

// The advertised seg_max and size_max bound nothing until they are
// checked, and the guest builds the chain.
#[test]
fn a_chain_past_the_advertised_limits_is_refused() {
    // Header, `segs` data segments of `len` bytes, status byte.
    let build = |segs: usize, len: u32| -> Vec<ChainBuf> {
        std::iter::once(ChainBuf::Readable {
            addr: HDR_GPA,
            len: BLK_REQ_HEADER_SIZE as u32,
        })
        .chain((0..segs).map(|_| ChainBuf::Writable {
            addr: RECYCLED_GPA,
            len,
        }))
        .chain(std::iter::once(ChainBuf::Writable {
            addr: STATUS_GPA,
            len: 1,
        }))
        .collect()
    };

    assert_eq!(
        io::span(&build(io::MAX_SEGMENTS, SIZE_MAX)),
        Some(io::MAX_REQ_BYTES)
    );
    assert_eq!(
        io::span(&build(io::MAX_SEGMENTS + 1, 512)),
        None,
        "a chain past seg_max was accepted"
    );
    assert_eq!(
        io::span(&build(1, SIZE_MAX + 1)),
        None,
        "a segment past size_max was accepted"
    );
}
