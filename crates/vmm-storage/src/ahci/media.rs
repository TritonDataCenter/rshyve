// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ISO media geometry. The arithmetic here checks host offsets and lengths
//! before the command engine does I/O.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::Arc;

use super::bits::{CD_BLOCK_SIZE, MAX_XFER_BYTES};

pub struct IsoMedia {
    file: File,
    blocks: u64,
    serial: [u8; 20],
}

impl IsoMedia {
    pub fn open(path: &Path) -> io::Result<Arc<Self>> {
        let file = OpenOptions::new().read(true).open(path)?;
        let len = file.metadata()?.len();
        let blocks = len / CD_BLOCK_SIZE;
        if len == 0 || blocks == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ISO image is empty",
            ));
        }

        Ok(Arc::new(Self {
            file,
            blocks,
            serial: media_serial(path),
        }))
    }

    pub fn blocks(&self) -> u64 {
        self.blocks
    }

    pub fn serial(&self) -> &[u8; 20] {
        &self.serial
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn read_range(&self, lba: u64, count: u32) -> Option<(u64, usize)> {
        checked_read_range(lba, count, self.blocks)
    }
}

pub fn checked_read_range(
    lba: u64,
    count: u32,
    blocks: u64,
) -> Option<(u64, usize)> {
    let count64 = u64::from(count);
    let end = lba.checked_add(count64)?;
    if end > blocks {
        return None;
    }
    let bytes = count64.checked_mul(CD_BLOCK_SIZE)?;
    if bytes > MAX_XFER_BYTES {
        return None;
    }
    let offset = lba.checked_mul(CD_BLOCK_SIZE)?;
    Some((offset, bytes as usize))
}

pub fn media_serial(path: &Path) -> [u8; 20] {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let mut hash = FNV_OFFSET_BASIS;
    for &byte in path.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }

    let value = hash & 0x0000_ffff_ffff_ffff;
    let mut serial = *b"BHYVE-0000-0000-0000";
    for (base, group_shift) in [(6, 32), (11, 16), (16, 0)] {
        for nibble in 0..4 {
            let shift = group_shift + (3 - nibble) * 4;
            serial[base + nibble] = HEX[((value >> shift) & 0xf) as usize];
        }
    }
    serial
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_range_rejects_past_end() {
        assert_eq!(checked_read_range(99, 2, 100), None);
    }

    #[test]
    fn read_range_accepts_exact_end() {
        assert_eq!(checked_read_range(98, 2, 100), Some((98 * 2048, 4096)));
    }

    #[test]
    fn read_range_rejects_lba_overflow() {
        assert_eq!(checked_read_range(u64::MAX, 1, u64::MAX), None);
    }

    #[test]
    fn read_range_rejects_count_overflow() {
        assert_eq!(checked_read_range(0, u32::MAX, u64::MAX), None);
    }

    #[test]
    fn read_range_rejects_over_max_xfer() {
        assert_eq!(checked_read_range(0, 1024, 100_000), None);
    }

    #[test]
    fn read_range_zero_count_is_ok() {
        assert_eq!(checked_read_range(5, 0, 100), Some((10_240, 0)));
    }

    #[test]
    fn serial_is_deterministic_and_well_formed() {
        let first = media_serial(Path::new("/tmp/first.iso"));
        let again = media_serial(Path::new("/tmp/first.iso"));
        let other = media_serial(Path::new("/tmp/other.iso"));

        assert_eq!(first, again);
        assert_ne!(first, other);
        assert_eq!(first.len(), 20);
        assert!(first.is_ascii());
        assert_eq!(&first[..6], b"BHYVE-");
        assert_eq!(first[5], b'-');
        assert_eq!(first[10], b'-');
        assert_eq!(first[15], b'-');
    }

    #[test]
    fn open_rejects_empty_file() {
        let file =
            tempfile::NamedTempFile::new().expect("create temporary ISO");
        let error = IsoMedia::open(file.path())
            .err()
            .expect("empty ISO must be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "ISO image is empty");
    }

    #[test]
    fn open_computes_block_count() {
        let file =
            tempfile::NamedTempFile::new().expect("create temporary ISO");
        file.as_file().set_len(4096).expect("size temporary ISO");

        let media = IsoMedia::open(file.path()).expect("open temporary ISO");
        assert_eq!(media.blocks(), 2);
    }

    #[test]
    fn open_floors_partial_trailing_block() {
        let file =
            tempfile::NamedTempFile::new().expect("create temporary ISO");
        file.as_file().set_len(4097).expect("size temporary ISO");

        let media = IsoMedia::open(file.path()).expect("open temporary ISO");
        assert_eq!(media.blocks(), 2);
    }
}
