// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Migration protocol implementation.
//!
//! Each wire-format change bumps [`PROTOCOL_RON`], and both ends refuse
//! any other version. There is no compatibility window: a peer that
//! reads a field with a different meaning resumes the guest on wrong
//! state.

/// Protocol identifier for the RON migration format.
///
/// v2 adds the NVMe completion queue interrupt-enable bit. Without it,
/// the destination cannot tell a polled queue from an interrupting one.
///
/// v1 adds device identity to each device state record. Without it,
/// two virtio devices with the same queue count can restore each
/// other's rings.
pub const PROTOCOL_RON: &str = "vmm-migrate-ron/2";

/// Page size for dirty tracking (4 KiB).
pub use vmm_core::common::PAGE_SIZE;

/// Pages per byte in the dirty bitmap.
pub const PAGES_PER_BYTE: usize = 8;

/// Bitmap size in bytes for a memory region of `region_len` bytes.
pub fn bitmap_size(region_len: usize) -> usize {
    let num_pages = region_len.div_ceil(PAGE_SIZE);
    num_pages.div_ceil(PAGES_PER_BYTE)
}

/// Iterator over set bits in a dirty page bitmap.
pub struct PageIter<'a> {
    bitmap: &'a [u8],
    base_gpa: u64,
    bit_index: usize,
    total_bits: usize,
}

impl<'a> PageIter<'a> {
    pub fn new(bitmap: &'a [u8], base_gpa: u64, region_len: usize) -> Self {
        let total_bits = region_len / PAGE_SIZE;
        Self {
            bitmap,
            base_gpa,
            bit_index: 0,
            total_bits,
        }
    }
}

impl<'a> Iterator for PageIter<'a> {
    /// Returns the GPA of the next dirty page.
    type Item = u64;

    fn next(&mut self) -> Option<Self::Item> {
        while self.bit_index < self.total_bits {
            let byte_idx = self.bit_index / 8;
            let bit_idx = self.bit_index % 8;
            self.bit_index += 1;

            if byte_idx < self.bitmap.len()
                && (self.bitmap[byte_idx] & (1 << bit_idx)) != 0
            {
                let page_offset =
                    (self.bit_index - 1) as u64 * PAGE_SIZE as u64;
                return Some(self.base_gpa + page_offset);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bitmap_size() {
        assert_eq!(bitmap_size(4096), 1);
        assert_eq!(bitmap_size(4096 * 8), 1);
        assert_eq!(bitmap_size(4096 * 9), 2);
        assert_eq!(bitmap_size(1024 * 1024), 32); // 1 MiB = 256 pages = 32 bytes
    }

    #[test]
    fn test_page_iter_empty() {
        let bitmap = [0u8; 1];
        let iter = PageIter::new(&bitmap, 0, PAGE_SIZE * 8);
        assert_eq!(iter.count(), 0);
    }

    #[test]
    fn test_page_iter_all_dirty() {
        let bitmap = [0xFFu8; 1];
        let pages: Vec<u64> =
            PageIter::new(&bitmap, 0, PAGE_SIZE * 8).collect();
        assert_eq!(pages.len(), 8);
        assert_eq!(pages[0], 0);
        assert_eq!(pages[7], 7 * PAGE_SIZE as u64);
    }

    #[test]
    fn test_page_iter_with_base() {
        let bitmap = [0x01u8]; // only first page dirty
        let pages: Vec<u64> =
            PageIter::new(&bitmap, 0x1000_0000, PAGE_SIZE * 8).collect();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0], 0x1000_0000);
    }
}
