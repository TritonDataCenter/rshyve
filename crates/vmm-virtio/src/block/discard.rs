// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! DISCARD and WRITE_ZEROES payload handling (VirtIO 1.3, 5.2.6.2).
//!
//! The two commands share a payload but not their guarantees. After
//! WRITE_ZEROES the range MUST read back as zeros, so it writes. After
//! DISCARD the data is unspecified, so a validated request may complete
//! without touching the backing store.
//!
//! An OK for WRITE_ZEROES without a write corrupts data. ext4 routes
//! inode-table initialisation and unwritten-extent conversion through
//! `sb_issue_zeroout`, which has no fallback when the device reports
//! success. The guest then keeps whatever the backing store held.
//!
//! [`plan`] reads guest memory and runs under guest-access permission.
//! [`zero`] reaches the disk and must run without it.

use std::fs::File;
use std::os::unix::fs::FileExt;

use vmm_core::mem::PhysMap;

use super::DiskLimits;
use crate::bits;
use crate::queue::ChainBuf;

/// Size of one `struct virtio_blk_discard_write_zeroes`.
const SEG_SIZE: usize = 16;

/// Segments accepted in one request of either kind. Published as
/// `max_discard_seg` and `max_write_zeroes_seg`.
pub(super) const MAX_SEG: u32 = 8;

/// Sectors per DISCARD segment (2 GiB). DISCARD does no I/O, so this
/// only bounds how much a guest can name in one segment.
pub(super) const MAX_DISCARD_SECTORS: u32 = 4 * 1024 * 1024;

/// Sectors per WRITE_ZEROES segment (8 MiB). Each segment costs a
/// bounded loop of `pwrite`, so keep one request short enough that it
/// does not hold an I/O worker for long.
pub(super) const MAX_WRITE_ZEROES_SECTORS: u32 = 16 * 1024;

/// The only flag defined for WRITE_ZEROES. It is not legal on DISCARD.
const FLAG_UNMAP: u32 = 1 << 0;

/// Bytes written per `pwrite` in the zero loop.
const ZERO_CHUNK: usize = 128 * 1024;

/// Ranges named by one request, held in host memory.
///
/// Fixed size so that reading a payload never allocates: the whole of
/// [`plan`] runs under guest-access permission, and a reset waits for
/// it.
pub(super) struct Ranges {
    ranges: [(u64, u32); MAX_SEG as usize],
    len: usize,
}

impl Ranges {
    fn iter(&self) -> impl Iterator<Item = &(u64, u32)> {
        self.ranges[..self.len].iter()
    }
}

/// What one DISCARD or WRITE_ZEROES request asks for.
pub(super) enum Plan {
    /// Nothing left to do at the backing store: the status is settled.
    Settled(u8),
    /// Ranges the request must zero.
    Zero(Ranges),
}

/// Copy the request payload out of guest memory.
///
/// The payload is copied because a vCPU can rewrite guest memory after
/// any check made against it in place.
fn read_payload<'a>(
    chain: &[ChainBuf],
    physmap: &PhysMap,
    out: &'a mut [u8; SEG_SIZE * MAX_SEG as usize],
) -> Option<&'a [u8]> {
    let mut used = 0usize;
    for buf in &chain[1..chain.len() - 1] {
        // A device-writable data buffer has no meaning here, and reading
        // one would let the guest name a range it never supplied.
        let ChainBuf::Readable { addr, len } = buf else {
            return None;
        };
        let n = *len as usize;
        let end = used.checked_add(n)?;
        if end > out.len() {
            return None;
        }
        let sub = physmap.lookup(*addr, n)?;
        sub.read_bytes(&mut out[used..end]).ok()?;
        used = end;
    }
    Some(&out[..used])
}

/// Validate one DISCARD or WRITE_ZEROES request against guest memory.
///
/// The caller must hold guest-access permission for the request's ring.
pub(super) fn plan(
    limits: &DiskLimits,
    rtype: u32,
    chain: &[ChainBuf],
    physmap: &PhysMap,
) -> Plan {
    let write_zeroes = rtype == bits::VIRTIO_BLK_T_WRITE_ZEROES;

    if limits.read_only {
        return Plan::Settled(bits::VIRTIO_BLK_S_IOERR);
    }
    if !write_zeroes && limits.nodelete {
        return Plan::Settled(bits::VIRTIO_BLK_S_UNSUPP);
    }
    // Header, at least one payload buffer, status byte.
    if chain.len() < 3 {
        return Plan::Settled(bits::VIRTIO_BLK_S_IOERR);
    }

    let mut raw = [0u8; SEG_SIZE * MAX_SEG as usize];
    let Some(payload) = read_payload(chain, physmap, &mut raw) else {
        return Plan::Settled(bits::VIRTIO_BLK_S_IOERR);
    };
    if payload.is_empty() || !payload.len().is_multiple_of(SEG_SIZE) {
        return Plan::Settled(bits::VIRTIO_BLK_S_IOERR);
    }

    let max_sectors = if write_zeroes {
        MAX_WRITE_ZEROES_SECTORS
    } else {
        MAX_DISCARD_SECTORS
    };

    // The length check above leaves no remainder.
    let (segments, _) = payload.as_chunks::<SEG_SIZE>();
    let mut out = Ranges {
        ranges: [(0, 0); MAX_SEG as usize],
        len: 0,
    };
    for raw in segments {
        let sector =
            u64::from_le_bytes(raw[..8].try_into().expect("8 byte field"));
        let num_sectors =
            u32::from_le_bytes(raw[8..12].try_into().expect("4 byte field"));
        let flags =
            u32::from_le_bytes(raw[12..16].try_into().expect("4 byte field"));

        // UNMAP is defined for WRITE_ZEROES only, and every other bit is
        // reserved. OK for an unimplemented flag tells the guest that an
        // action occurred when it did not.
        let allowed = if write_zeroes { FLAG_UNMAP } else { 0 };
        if flags & !allowed != 0 {
            return Plan::Settled(bits::VIRTIO_BLK_S_UNSUPP);
        }
        if num_sectors > max_sectors {
            return Plan::Settled(bits::VIRTIO_BLK_S_IOERR);
        }
        if sector
            .checked_add(u64::from(num_sectors))
            .is_none_or(|end| end > limits.capacity)
        {
            return Plan::Settled(bits::VIRTIO_BLK_S_IOERR);
        }
        out.ranges[out.len] = (sector, num_sectors);
        out.len += 1;
    }

    // The data in a discarded range is unspecified (VirtIO 1.3, 5.2.6.2),
    // so a validated DISCARD is complete here. WRITE_ZEROES must read
    // back as zeros, so it always writes. UNMAP only lets the device
    // also deallocate. It never relaxes the zero guarantee.
    if !write_zeroes {
        return Plan::Settled(bits::VIRTIO_BLK_S_OK);
    }
    Plan::Zero(out)
}

/// Zero every range the plan named. Reaches the disk, so the caller
/// must hold no guest-access permission.
pub(super) fn zero(file: &File, ranges: &Ranges) -> u8 {
    for &(sector, num_sectors) in ranges.iter() {
        if !write_zero_range(file, sector, num_sectors) {
            return bits::VIRTIO_BLK_S_IOERR;
        }
    }
    bits::VIRTIO_BLK_S_OK
}

/// Write zeros over `num_sectors` sectors starting at `sector`.
///
/// illumos has no `fallocate(2)`. `fcntl(F_FREESP)` covers regular files
/// only and a zvol needs a hand-built `DKIOCFREE` list, so the answer
/// that is correct for both backing stores is a bounded loop of
/// `pwrite`. The caller has already bounded the range.
fn write_zero_range(file: &File, sector: u64, num_sectors: u32) -> bool {
    let Some(mut offset) = sector.checked_mul(bits::SECTOR_SIZE) else {
        return false;
    };
    let Some(mut remaining) =
        u64::from(num_sectors).checked_mul(bits::SECTOR_SIZE)
    else {
        return false;
    };

    let zeros = vec![0u8; ZERO_CHUNK];
    while remaining > 0 {
        let chunk = remaining.min(ZERO_CHUNK as u64) as usize;
        if i64::try_from(offset).is_err()
            || file.write_all_at(&zeros[..chunk], offset).is_err()
        {
            return false;
        }
        offset += chunk as u64;
        remaining -= chunk as u64;
    }
    true
}
