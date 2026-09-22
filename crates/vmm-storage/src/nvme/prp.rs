// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PRP walking, NVMe 1.4 section 4.3.
//!
//! PRP1 may start anywhere in a page and covers to the end of it. If
//! the rest fits in one page, PRP2 names that page and must be page
//! aligned. Otherwise PRP2 names a PRP list: it may start anywhere in
//! its page, ends at the page boundary, and its last entry chains to
//! the next list while data remains.

use std::io;

use vmm_core::common::PAGE_SIZE;
use vmm_core::mem::{GuestIoVec, MemCtx};

const PAGE_OFFSET: u64 = PAGE_SIZE as u64 - 1;
const ENTRIES_PER_LIST: u64 = PAGE_SIZE as u64 / 8;

fn invalid(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what)
}

enum Next {
    Prp1,
    Prp2,
    List { base: u64, idx: u64 },
    Done,
}

/// The segments a command's PRPs name, produced one at a time.
struct PrpWalk<'a> {
    mem: &'a MemCtx,
    prp1: u64,
    prp2: u64,
    remain: usize,
    next: Next,
}

impl<'a> PrpWalk<'a> {
    fn new(mem: &'a MemCtx, prp1: u64, prp2: u64, len: usize) -> Self {
        Self {
            mem,
            prp1,
            prp2,
            remain: len,
            next: if len == 0 { Next::Done } else { Next::Prp1 },
        }
    }

    fn list_entry(&self, base: u64, idx: u64) -> io::Result<u64> {
        let gpa = base
            .checked_add(idx.checked_mul(8).ok_or_else(|| invalid("PRP list"))?)
            .ok_or_else(|| invalid("PRP list address wraps"))?;
        let mut buf = [0u8; 8];
        self.mem.read(gpa, &mut buf)?;
        Ok(u64::from_le_bytes(buf))
    }

    /// The next `(gpa, len)` segment, or `None` when the transfer is
    /// covered.
    fn next_segment(&mut self) -> io::Result<Option<(u64, usize)>> {
        let (gpa, len, next) = match self.next {
            Next::Done => return Ok(None),
            Next::Prp1 => {
                if self.prp1 & 3 != 0 {
                    return Err(invalid("PRP1 is not dword aligned"));
                }
                let offset = (self.prp1 & PAGE_OFFSET) as usize;
                let len = (PAGE_SIZE - offset).min(self.remain);
                let after = self.remain - len;
                let next = if after == 0 {
                    Next::Done
                } else if after <= PAGE_SIZE {
                    Next::Prp2
                } else {
                    if self.prp2 & 7 != 0 {
                        return Err(invalid("PRP list is not qword aligned"));
                    }
                    Next::List {
                        base: self.prp2 & !PAGE_OFFSET,
                        idx: (self.prp2 & PAGE_OFFSET) / 8,
                    }
                };
                (self.prp1, len, next)
            }
            Next::Prp2 => {
                if self.prp2 & PAGE_OFFSET != 0 {
                    return Err(invalid("PRP2 has a page offset"));
                }
                (self.prp2, self.remain, Next::Done)
            }
            Next::List { base, idx } => {
                let entry = self.list_entry(base, idx)?;
                if entry & PAGE_OFFSET != 0 {
                    return Err(invalid("PRP list entry has a page offset"));
                }
                if self.remain <= PAGE_SIZE {
                    (entry, self.remain, Next::Done)
                } else if idx + 1 < ENTRIES_PER_LIST {
                    (entry, PAGE_SIZE, Next::List { base, idx: idx + 1 })
                } else {
                    // The last entry of a list points at the next list.
                    self.next = Next::List {
                        base: entry,
                        idx: 0,
                    };
                    return self.next_segment();
                }
            }
        };
        self.remain -= len;
        self.next = next;
        Ok(Some((gpa, len)))
    }
}

/// Pin the pages a transfer of `total_len` bytes names.
pub(super) fn prp_to_iovec(
    mem: &MemCtx,
    prp1: u64,
    prp2: u64,
    total_len: usize,
) -> io::Result<GuestIoVec> {
    let mut iov = GuestIoVec::with_capacity(16);
    let mut walk = PrpWalk::new(mem, prp1, prp2, total_len);
    while let Some((gpa, len)) = walk.next_segment()? {
        let sub = mem
            .lookup(gpa, len)
            .ok_or_else(|| invalid("PRP names unmapped memory"))?;
        iov.push(sub);
    }
    Ok(iov)
}

/// Write `data` to the pages the PRPs name.
pub(super) fn write_to_prp(
    mem: &MemCtx,
    prp1: u64,
    prp2: u64,
    data: &[u8],
) -> io::Result<()> {
    let mut walk = PrpWalk::new(mem, prp1, prp2, data.len());
    let mut offset = 0;
    while let Some((gpa, len)) = walk.next_segment()? {
        mem.write(gpa, &data[offset..offset + len])?;
        offset += len;
    }
    Ok(())
}
