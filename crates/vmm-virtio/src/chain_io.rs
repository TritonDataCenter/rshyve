// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Sequential `io::Read`/`io::Write` views over one descriptor chain.
//!
//! FUSE frames a request as a byte stream, but a chain is a list of
//! guest-physical segments. These adapters give the stream without a
//! second guest-memory path: every access still goes through
//! `PhysMap::lookup` and the volatile `SubMapping` accessors.

use std::io::{Read, Write};

use vmm_core::mem::{PhysMap, SubMapping};

use super::queue::ChainBuf;

fn unmapped() -> std::io::Error {
    std::io::Error::from(std::io::ErrorKind::AddrNotAvailable)
}

/// The half of a chain that a cursor walks.
///
/// A guest can interleave readable and writable descriptors. Each cursor
/// stays in one half, so the device cannot read a segment it must write,
/// or write a segment the guest owns.
#[derive(Clone, Copy)]
enum Half {
    Readable,
    Writable,
}

impl Half {
    fn segment(self, buf: &ChainBuf) -> Option<(u64, u32)> {
        match (self, buf) {
            (Half::Readable, ChainBuf::Readable { addr, len })
            | (Half::Writable, ChainBuf::Writable { addr, len }) => {
                Some((*addr, *len))
            }
            _ => None,
        }
    }
}

/// Position within one half of a chain.
///
/// The address and length of every segment come from the guest, so all
/// arithmetic on them is checked or saturating.
struct Cursor<'a> {
    bufs: &'a [ChainBuf],
    physmap: &'a PhysMap,
    half: Half,
    idx: usize,
    off: usize,
}

impl<'a> Cursor<'a> {
    fn new(bufs: &'a [ChainBuf], physmap: &'a PhysMap, half: Half) -> Self {
        let mut this = Self {
            bufs,
            physmap,
            half,
            idx: 0,
            off: 0,
        };
        this.advance();
        this
    }

    /// Move to the next segment of this half that still has room.
    fn advance(&mut self) {
        while let Some(buf) = self.bufs.get(self.idx) {
            match self.half.segment(buf) {
                Some((_, len)) if self.off < len as usize => return,
                _ => {
                    self.idx = self.idx.saturating_add(1);
                    self.off = 0;
                }
            }
        }
    }

    /// Bytes of this half that the cursor has not passed yet.
    fn remaining(&self) -> usize {
        self.bufs
            .iter()
            .enumerate()
            .skip(self.idx)
            .filter_map(|(i, buf)| {
                let (_, len) = self.half.segment(buf)?;
                let used = if i == self.idx { self.off } else { 0 };
                Some((len as usize).saturating_sub(used))
            })
            .fold(0usize, usize::saturating_add)
    }

    /// Every byte of this half, passed or not.
    fn total(&self) -> usize {
        self.bufs
            .iter()
            .filter_map(|buf| Some(self.half.segment(buf)?.1 as usize))
            .fold(0usize, usize::saturating_add)
    }

    /// Map at most `max` bytes at the cursor.
    ///
    /// `None` means this half has no bytes left. `read_exact` and
    /// `write_all` turn that into their own end-of-stream errors.
    fn map(
        &mut self,
        max: usize,
    ) -> std::io::Result<Option<(SubMapping, usize)>> {
        self.advance();
        let Some((addr, len)) = self
            .bufs
            .get(self.idx)
            .and_then(|buf| self.half.segment(buf))
        else {
            return Ok(None);
        };
        let n = (len as usize).saturating_sub(self.off).min(max);
        if n == 0 {
            return Ok(None);
        }
        let gpa = addr.checked_add(self.off as u64).ok_or_else(unmapped)?;
        let sub = self.physmap.lookup(gpa, n).ok_or_else(unmapped)?;
        Ok(Some((sub, n)))
    }

    /// Record `n` bytes moved at the cursor.
    fn consume(&mut self, n: usize) {
        self.off = self.off.saturating_add(n);
    }
}

/// Reader over the `ChainBuf::Readable` segments of one chain.
pub struct ChainReader<'a> {
    cursor: Cursor<'a>,
}

impl<'a> ChainReader<'a> {
    pub fn new(bufs: &'a [ChainBuf], physmap: &'a PhysMap) -> Self {
        Self {
            cursor: Cursor::new(bufs, physmap, Half::Readable),
        }
    }

    /// Bytes still unread across all remaining readable segments.
    pub fn remaining(&self) -> usize {
        self.cursor.remaining()
    }
}

impl Read for ChainReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let Some((sub, n)) = self.cursor.map(buf.len())? else {
            return Ok(0);
        };
        sub.read_bytes(&mut buf[..n])?;
        self.cursor.consume(n);
        Ok(n)
    }
}

/// Writer over the `ChainBuf::Writable` segments of one chain.
pub struct ChainWriter<'a> {
    cursor: Cursor<'a>,
    written: u32,
}

impl<'a> ChainWriter<'a> {
    pub fn new(bufs: &'a [ChainBuf], physmap: &'a PhysMap) -> Self {
        Self {
            cursor: Cursor::new(bufs, physmap, Half::Writable),
            written: 0,
        }
    }

    /// Total writable bytes in the chain.
    pub fn capacity(&self) -> usize {
        self.cursor.total()
    }

    /// Bytes that reached guest memory. The caller puts this value in
    /// the used ring `len` field.
    pub fn written(&self) -> u32 {
        self.written
    }
}

impl Write for ChainWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let Some((sub, n)) = self.cursor.map(buf.len())? else {
            return Ok(0);
        };
        sub.write_bytes(&buf[..n])?;
        self.cursor.consume(n);
        let moved = u32::try_from(n).unwrap_or(u32::MAX);
        self.written = self.written.saturating_add(moved);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // Writes are already volatile stores into guest RAM.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    const GPA: u64 = 0x10_0000;
    const LEN: usize = 0x4000;

    fn anon() -> PhysMap {
        PhysMap::new_anon(GPA, LEN).expect("anon region")
    }

    fn put(physmap: &PhysMap, gpa: u64, data: &[u8]) {
        physmap
            .lookup(gpa, data.len())
            .expect("mapped")
            .write_bytes(data)
            .expect("seed guest memory");
    }

    // A hostile guest can interleave readable and writable descriptors.
    // The reader must never see a device-writable segment.
    #[test]
    fn reader_skips_writable_and_concatenates_in_chain_order() {
        let physmap = anon();
        put(&physmap, GPA, b"abcd");
        put(&physmap, GPA + 0x200, b"efgh");
        let bufs = vec![
            ChainBuf::Readable { addr: GPA, len: 4 },
            ChainBuf::Writable {
                addr: GPA + 0x100,
                len: 64,
            },
            ChainBuf::Readable {
                addr: GPA + 0x200,
                len: 4,
            },
        ];

        let mut r = ChainReader::new(&bufs, &physmap);
        assert_eq!(r.remaining(), 8);
        let mut out = [0u8; 8];
        r.read_exact(&mut out).expect("read_exact");
        assert_eq!(&out, b"abcdefgh");
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn reader_short_chain_yields_unexpected_eof() {
        let physmap = anon();
        put(&physmap, GPA, b"ab");
        let bufs = vec![ChainBuf::Readable { addr: GPA, len: 2 }];

        let mut r = ChainReader::new(&bufs, &physmap);
        let mut out = [0u8; 8];
        let err = r.read_exact(&mut out).expect_err("short chain");
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn reader_unmapped_gpa_is_addr_not_available() {
        let physmap = anon();
        let bufs = vec![ChainBuf::Readable {
            addr: 0xDEAD_0000,
            len: 4,
        }];

        let mut r = ChainReader::new(&bufs, &physmap);
        let mut out = [0u8; 4];
        let err = r.read(&mut out).expect_err("unmapped");
        assert_eq!(err.kind(), std::io::ErrorKind::AddrNotAvailable);
    }

    #[test]
    fn writer_skips_readable_and_reports_written() {
        let physmap = anon();
        let bufs = vec![
            ChainBuf::Readable { addr: GPA, len: 16 },
            ChainBuf::Writable {
                addr: GPA + 0x400,
                len: 4,
            },
            ChainBuf::Writable {
                addr: GPA + 0x800,
                len: 4,
            },
        ];

        let mut w = ChainWriter::new(&bufs, &physmap);
        assert_eq!(w.capacity(), 8);
        w.write_all(b"abcdefgh").expect("write_all");
        w.flush().expect("flush");
        assert_eq!(w.written(), 8);

        let mut a = [0u8; 4];
        let mut b = [0u8; 4];
        physmap
            .lookup(GPA + 0x400, 4)
            .expect("m")
            .read_bytes(&mut a)
            .expect("r");
        physmap
            .lookup(GPA + 0x800, 4)
            .expect("m")
            .read_bytes(&mut b)
            .expect("r");
        assert_eq!(&a, b"abcd");
        assert_eq!(&b, b"efgh");
    }

    // A writable window shorter than the reply must surface as WriteZero,
    // which the FUSE server treats as a short-body write.
    #[test]
    fn writer_past_capacity_yields_write_zero() {
        let physmap = anon();
        let bufs = vec![ChainBuf::Writable {
            addr: GPA + 0x400,
            len: 2,
        }];

        let mut w = ChainWriter::new(&bufs, &physmap);
        let err = w.write_all(b"abcd").expect_err("overrun");
        assert_eq!(err.kind(), std::io::ErrorKind::WriteZero);
        assert_eq!(w.written(), 2);
    }
}
