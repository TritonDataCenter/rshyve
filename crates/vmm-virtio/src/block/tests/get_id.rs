// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! GET_ID, the one request the device answers inline on the vCPU.
//!
//! Every other request takes its data from the chain without the header
//! and the status byte, and these pin GET_ID to that same span.

use super::*;

/// On a `[header, status]` chain the first writable buffer is the status
/// descriptor. A GET_ID that takes it puts the answer into the byte that
/// reports it, and the used length counts that byte twice.
#[test]
fn a_get_id_without_a_data_segment_reports_an_error() {
    const DESC_GPA: u64 = 0x1000;
    const USED_GPA: u64 = 0x1200;
    const STATUS_GPA: u64 = 0x3800;

    let (physmap, queue, block) = make_get_id_queue(4, 1);
    let mut queues = [queue];

    // Point the header's NEXT at a one-byte status descriptor, so the
    // chain names no data at all.
    physmap
        .lookup(DESC_GPA + 16, 16)
        .expect("mapped status descriptor")
        .write(&VirtqDesc {
            addr: STATUS_GPA,
            len: 1,
            flags: bits::VRING_DESC_F_WRITE,
            next: 0,
        })
        .expect("write status descriptor");
    physmap
        .lookup(STATUS_GPA, 1)
        .expect("mapped status byte")
        .write_bytes(&[0xEE])
        .expect("write status byte");

    block.notify_queue(0, &mut queues, &physmap);

    let entry = physmap.lookup(USED_GPA + 4, 8).expect("mapped used entry");
    assert_eq!(
        entry
            .subregion(4, 4)
            .expect("used length")
            .read::<u32>()
            .expect("read used length"),
        1,
        "the used length counted the status byte as data",
    );
    let mut status = [0u8; 1];
    physmap
        .lookup(STATUS_GPA, 1)
        .expect("mapped status byte")
        .read_bytes(&mut status)
        .expect("read status byte");
    assert_eq!(
        status[0],
        bits::VIRTIO_BLK_S_IOERR,
        "the device answered a request it could not place",
    );
}

/// Overwrite one descriptor of the harness chain.
fn set_desc(physmap: &PhysMap, base: u64, idx: u64, desc: VirtqDesc) {
    physmap
        .lookup(base + idx * 16, 16)
        .expect("mapped descriptor")
        .write(&desc)
        .expect("write descriptor");
}

/// Read one used-ring entry's `(id, len)`.
fn used_entry(physmap: &PhysMap, used_gpa: u64, slot: u64) -> (u32, u32) {
    let entry = physmap
        .lookup(used_gpa + 4 + slot * 8, 8)
        .expect("mapped used entry");
    (
        entry.read::<u32>().expect("used id"),
        entry
            .subregion(4, 4)
            .expect("used length")
            .read::<u32>()
            .expect("read used length"),
    )
}

fn status_byte(physmap: &PhysMap, gpa: u64) -> u8 {
    let mut byte = [0u8; 1];
    physmap
        .lookup(gpa, 1)
        .expect("mapped status byte")
        .read_bytes(&mut byte)
        .expect("read status byte");
    byte[0]
}

/// The shape a driver actually posts: header, one data buffer, the
/// status byte. No other test covers it.
#[test]
fn a_get_id_answers_into_the_data_segment() {
    const DESC_GPA: u64 = 0x1000;
    const USED_GPA: u64 = 0x1200;
    const HEADER_GPA: u64 = 0x2000;
    const DATA_GPA: u64 = 0x3000;
    const STATUS_GPA: u64 = 0x3800;

    let (physmap, queue, block) = make_get_id_queue(4, 1);
    set_desc(
        &physmap,
        DESC_GPA,
        0,
        VirtqDesc {
            addr: HEADER_GPA,
            len: BLK_REQ_HEADER_SIZE as u32,
            flags: bits::VRING_DESC_F_NEXT,
            next: 1,
        },
    );
    set_desc(
        &physmap,
        DESC_GPA,
        1,
        VirtqDesc {
            addr: DATA_GPA,
            len: VIRTIO_BLK_ID_BYTES as u32,
            flags: bits::VRING_DESC_F_WRITE | bits::VRING_DESC_F_NEXT,
            next: 2,
        },
    );
    set_desc(
        &physmap,
        DESC_GPA,
        2,
        VirtqDesc {
            addr: STATUS_GPA,
            len: 1,
            flags: bits::VRING_DESC_F_WRITE,
            next: 0,
        },
    );
    let mut queues = [queue];

    block.notify_queue(0, &mut queues, &physmap);

    assert_eq!(status_byte(&physmap, STATUS_GPA), bits::VIRTIO_BLK_S_OK);
    assert_eq!(
        used_entry(&physmap, USED_GPA, 0),
        (0, VIRTIO_BLK_ID_BYTES as u32 + 1),
        "the used length must count the id and the status byte",
    );
    let mut id = [0u8; VIRTIO_BLK_ID_BYTES];
    physmap
        .lookup(DATA_GPA, VIRTIO_BLK_ID_BYTES)
        .expect("mapped data buffer")
        .read_bytes(&mut id)
        .expect("read data buffer");
    assert_eq!(&id[..14], b"vmm-virtio-blk");
}

/// A used length the device did not write tells the guest that bytes of
/// device id are in a buffer that still holds whatever was there
/// before, which is an information leak rather than a wrong length.
#[test]
fn a_get_id_into_unmapped_memory_reports_an_error() {
    const DESC_GPA: u64 = 0x1000;
    const USED_GPA: u64 = 0x1200;
    const STATUS_GPA: u64 = 0x3800;
    /// Past the one region the harness maps.
    const UNMAPPED_GPA: u64 = 0x9000;

    let (physmap, queue, block) = make_get_id_queue(4, 1);
    set_desc(
        &physmap,
        DESC_GPA,
        1,
        VirtqDesc {
            addr: UNMAPPED_GPA,
            len: VIRTIO_BLK_ID_BYTES as u32,
            flags: bits::VRING_DESC_F_WRITE | bits::VRING_DESC_F_NEXT,
            next: 2,
        },
    );
    set_desc(
        &physmap,
        DESC_GPA,
        2,
        VirtqDesc {
            addr: STATUS_GPA,
            len: 1,
            flags: bits::VRING_DESC_F_WRITE,
            next: 0,
        },
    );
    let mut queues = [queue];

    block.notify_queue(0, &mut queues, &physmap);

    assert_eq!(status_byte(&physmap, STATUS_GPA), bits::VIRTIO_BLK_S_IOERR);
    assert_eq!(
        used_entry(&physmap, USED_GPA, 0),
        (0, 1),
        "the device reported id bytes it never wrote",
    );
}

/// Nothing stops a driver padding its chain, and the answer does not
/// belong in an empty buffer. A device that takes it leaves the real
/// buffer untouched.
#[test]
fn a_get_id_skips_an_empty_data_buffer() {
    const DESC_GPA: u64 = 0x1000;
    const USED_GPA: u64 = 0x1200;
    const DATA_GPA: u64 = 0x3000;
    const STATUS_GPA: u64 = 0x3800;

    let (physmap, queue, block) = make_get_id_queue(4, 1);
    set_desc(
        &physmap,
        DESC_GPA,
        1,
        VirtqDesc {
            addr: DATA_GPA,
            len: 0,
            flags: bits::VRING_DESC_F_WRITE | bits::VRING_DESC_F_NEXT,
            next: 2,
        },
    );
    set_desc(
        &physmap,
        DESC_GPA,
        2,
        VirtqDesc {
            addr: DATA_GPA + 0x100,
            len: VIRTIO_BLK_ID_BYTES as u32,
            flags: bits::VRING_DESC_F_WRITE | bits::VRING_DESC_F_NEXT,
            next: 3,
        },
    );
    set_desc(
        &physmap,
        DESC_GPA,
        3,
        VirtqDesc {
            addr: STATUS_GPA,
            len: 1,
            flags: bits::VRING_DESC_F_WRITE,
            next: 0,
        },
    );
    let mut queues = [queue];

    block.notify_queue(0, &mut queues, &physmap);

    assert_eq!(status_byte(&physmap, STATUS_GPA), bits::VIRTIO_BLK_S_OK);
    assert_eq!(
        used_entry(&physmap, USED_GPA, 0),
        (0, VIRTIO_BLK_ID_BYTES as u32 + 1),
    );
    let mut id = [0u8; 14];
    physmap
        .lookup(DATA_GPA + 0x100, 14)
        .expect("mapped data buffer")
        .read_bytes(&mut id)
        .expect("read data buffer");
    assert_eq!(&id, b"vmm-virtio-blk");
}
