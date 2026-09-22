// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Common types for I/O operations on PIO and MMIO buses.

/// Standard page size in bytes (4 KiB).
pub const PAGE_SIZE: usize = 4096;

/// Number of bits to shift to convert between addresses and page numbers.
pub const PAGE_SHIFT: usize = 12;

/// An I/O read operation.
///
/// Represents a device read where the device writes a value into the buffer
/// for the guest to consume. This is a hot-path type used on every PIO/MMIO
/// exit, so it is kept small and `Copy`.
#[derive(Clone, Copy, Debug)]
pub struct ReadOp {
    /// Buffer to hold the read value (little-endian).
    buf: [u8; 8],
    /// Number of valid bytes (1, 2, 4, or 8).
    len: u8,
}

impl ReadOp {
    /// Create a new read operation for the given byte width.
    ///
    /// The buffer is initialized to zero. `len` should be 1, 2, 4, or 8.
    ///
    /// # Panics
    ///
    /// Panics if `len` is 0 or greater than 8.
    #[inline]
    pub fn new(len: usize) -> Self {
        assert!(len > 0 && len <= 8);
        Self {
            buf: [0u8; 8],
            len: len as u8,
        }
    }

    /// Write a `u8` value into the buffer.
    #[inline]
    pub fn write_u8(&mut self, val: u8) {
        self.buf[0] = val;
    }

    /// Write a `u16` value into the buffer in little-endian byte order.
    #[inline]
    pub fn write_u16(&mut self, val: u16) {
        let bytes = val.to_le_bytes();
        self.buf[..2].copy_from_slice(&bytes);
    }

    /// Write a `u32` value into the buffer in little-endian byte order.
    #[inline]
    pub fn write_u32(&mut self, val: u32) {
        let bytes = val.to_le_bytes();
        self.buf[..4].copy_from_slice(&bytes);
    }

    /// Write a `u64` value into the buffer in little-endian byte order.
    #[inline]
    pub fn write_u64(&mut self, val: u64) {
        let bytes = val.to_le_bytes();
        self.buf[..8].copy_from_slice(&bytes);
    }

    /// Answer this read from the dword register value `val`, as seen
    /// from byte `byte_off` of that register.
    ///
    /// A one or two byte access at a sub-dword offset gets the bytes it
    /// named. An access wider than a dword gets the whole value.
    pub fn write_dword_at(&mut self, val: u32, byte_off: usize) {
        let shifted = val >> ((byte_off & 0x3) * 8);
        match self.len() {
            1 => self.write_u8(shifted as u8),
            2 => self.write_u16(shifted as u16),
            _ => self.write_u32(shifted),
        }
    }

    /// Answer this read from a dword register value at its own width.
    pub fn write_dword(&mut self, val: u32) {
        self.write_dword_at(val, 0);
    }

    /// The valid portion of the buffer.
    pub fn buf(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }

    /// Get the operation width in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// Always false: the constructor forbids a zero width.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// An I/O write operation.
///
/// Represents a device write where the guest has provided data for the device
/// to consume. This is a hot-path type used on every PIO/MMIO exit, so it is
/// kept small and `Copy`.
#[derive(Clone, Copy, Debug)]
pub struct WriteOp {
    /// Buffer holding the written value (little-endian).
    buf: [u8; 8],
    /// Number of valid bytes (1, 2, 4, or 8).
    len: u8,
}

impl WriteOp {
    /// Create a write operation from the given byte slice.
    ///
    /// # Panics
    ///
    /// Panics if `data` is empty or longer than 8 bytes.
    #[inline]
    pub fn from_buf(data: &[u8]) -> Self {
        assert!(!data.is_empty() && data.len() <= 8);
        let mut buf = [0u8; 8];
        buf[..data.len()].copy_from_slice(data);
        Self {
            buf,
            len: data.len() as u8,
        }
    }

    /// Read a `u8` value from the buffer.
    #[inline]
    pub fn read_u8(&self) -> u8 {
        self.buf[0]
    }

    /// Read a `u16` value from the buffer in little-endian byte order.
    #[inline]
    pub fn read_u16(&self) -> u16 {
        u16::from_le_bytes([self.buf[0], self.buf[1]])
    }

    /// Read a `u32` value from the buffer in little-endian byte order.
    #[inline]
    pub fn read_u32(&self) -> u32 {
        u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]])
    }

    /// The value written, widened to a dword.
    #[inline]
    pub fn read_dword(&self) -> u32 {
        match self.len() {
            1 => u32::from(self.read_u8()),
            2 => u32::from(self.read_u16()),
            _ => self.read_u32(),
        }
    }

    /// Read a `u64` value from the buffer in little-endian byte order.
    pub fn read_u64(&self) -> u64 {
        u64::from_le_bytes(self.buf)
    }

    /// Get the operation width in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// Always false: the constructor forbids a zero width.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Get the valid portion of the buffer.
    #[inline]
    pub fn buf(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }
}

/// A read or write I/O operation.
///
/// Device handlers receive this enum so they can handle both directions
/// with a single function signature. The contained reference is to a
/// `ReadOp` or `WriteOp` which the handler reads from or writes to.
pub enum RWOp<'a> {
    Read(&'a mut ReadOp),
    Write(&'a WriteOp),
}

impl RWOp<'_> {
    #[inline]
    pub fn is_read(&self) -> bool {
        matches!(self, RWOp::Read(_))
    }

    #[inline]
    pub fn is_write(&self) -> bool {
        matches!(self, RWOp::Write(_))
    }

    /// The operation width in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        match self {
            RWOp::Read(ro) => ro.len(),
            RWOp::Write(wo) => wo.len(),
        }
    }

    /// Returns `true` if the operation has zero width.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn read_op_u8() {
        let mut op = ReadOp::new(1);
        assert_eq!(op.len(), 1);
        assert_eq!(op.buf(), &[0]);

        op.write_u8(0x42);
        assert_eq!(op.buf(), &[0x42]);
    }

    #[test]
    fn read_op_u16() {
        let mut op = ReadOp::new(2);
        op.write_u16(0x1234);
        assert_eq!(op.buf(), &[0x34, 0x12]);
    }

    #[test]
    fn read_op_u32() {
        let mut op = ReadOp::new(4);
        op.write_u32(0xDEAD_BEEF);
        assert_eq!(op.buf(), &[0xEF, 0xBE, 0xAD, 0xDE]);
    }

    #[test]
    fn read_op_u64() {
        let mut op = ReadOp::new(8);
        op.write_u64(0x0102_0304_0506_0708);
        assert_eq!(op.buf(), &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
    }

    #[test]
    fn write_op_u8() {
        let op = WriteOp::from_buf(&[0xAB]);
        assert_eq!(op.len(), 1);
        assert_eq!(op.read_u8(), 0xAB);
    }

    #[test]
    fn write_op_u16() {
        let op = WriteOp::from_buf(&[0x34, 0x12]);
        assert_eq!(op.len(), 2);
        assert_eq!(op.read_u16(), 0x1234);
    }

    #[test]
    fn write_op_u32() {
        let op = WriteOp::from_buf(&[0xEF, 0xBE, 0xAD, 0xDE]);
        assert_eq!(op.len(), 4);
        assert_eq!(op.read_u32(), 0xDEAD_BEEF);
    }

    #[test]
    fn write_op_u64() {
        let op = WriteOp::from_buf(&[
            0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01,
        ]);
        assert_eq!(op.len(), 8);
        assert_eq!(op.read_u64(), 0x0102_0304_0506_0708);
    }

    #[test]
    fn read_op_is_copy() {
        let mut op = ReadOp::new(4);
        op.write_u32(0x1234);
        let op2 = op; // Copy
        assert_eq!(op.buf(), op2.buf());
    }

    #[test]
    fn write_op_is_copy() {
        let op = WriteOp::from_buf(&[1, 2, 3, 4]);
        let op2 = op; // Copy
        assert_eq!(op.buf(), op2.buf());
    }

    #[test]
    #[should_panic]
    fn read_op_zero_len_panics() {
        let _ = ReadOp::new(0);
    }

    #[test]
    #[should_panic]
    fn read_op_too_large_panics() {
        let _ = ReadOp::new(9);
    }

    #[test]
    #[should_panic]
    fn write_op_empty_panics() {
        let _ = WriteOp::from_buf(&[]);
    }

    #[test]
    fn page_constants() {
        assert_eq!(PAGE_SIZE, 1 << PAGE_SHIFT);
    }

    #[test]
    fn rwop_read_variant() {
        let mut ro = ReadOp::new(4);
        let rwop = RWOp::Read(&mut ro);
        assert!(rwop.is_read());
        assert!(!rwop.is_write());
        assert_eq!(rwop.len(), 4);
    }

    #[test]
    fn rwop_write_variant() {
        let wo = WriteOp::from_buf(&[0x01, 0x02]);
        let rwop = RWOp::Write(&wo);
        assert!(!rwop.is_read());
        assert!(rwop.is_write());
        assert_eq!(rwop.len(), 2);
    }
}
