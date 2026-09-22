// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Vectored disk transfers straight into and out of guest memory.
//!
//! A storage device that maps a guest scatter list one page at a time
//! needs one `preadv` or `pwritev` over all of them. Building that
//! iovec list is the only reason a device would want the raw host
//! pointer of a [`SubMapping`], so the list is built here and the
//! pointer never leaves the module.

use std::fs::File;
use std::io::{Error, ErrorKind, Result};
use std::os::fd::AsRawFd;

use super::mapping::{Prot, SubMapping};

/// The most entries one `preadv` or `pwritev` accepts (illumos
/// `IOV_MAX`).
const IOV_MAX: usize = 1024;

/// A guest scatter list held ready for one disk transfer.
///
/// Each entry pins its pages for as long as the list lives, so a
/// syscall still running when the device goes away lands on memory
/// that is still mapped.
#[derive(Default)]
pub struct GuestIoVec {
    subs: Vec<SubMapping>,
    len: usize,
}

impl GuestIoVec {
    pub fn with_capacity(entries: usize) -> Self {
        Self {
            subs: Vec::with_capacity(entries),
            len: 0,
        }
    }

    /// Append one mapped run of guest bytes.
    pub fn push(&mut self, sub: SubMapping) {
        self.len += sub.len();
        self.subs.push(sub);
    }

    /// Bytes the whole list covers.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.subs.is_empty()
    }

    /// Fill the list from `file` at `offset`. Returns the bytes moved,
    /// which is short of [`Self::len`] only at end of file.
    pub fn read_from(&self, file: &File, offset: u64) -> Result<usize> {
        self.check_prot(
            Prot::WRITE,
            "read into a mapping that is not writable",
        )?;
        self.transfer(file, offset, |fd, iov, at| {
            // Safety: every iovec names pinned guest memory the caller
            // has permission to write, and `iov` outlives the call.
            unsafe { libc::preadv(fd, iov.as_ptr(), iov.len() as i32, at) }
        })
    }

    /// Write the list to `file` at `offset`. Returns the bytes moved.
    pub fn write_to(&self, file: &File, offset: u64) -> Result<usize> {
        self.check_prot(
            Prot::READ,
            "write from a mapping that is not readable",
        )?;
        self.transfer(file, offset, |fd, iov, at| {
            // Safety: every iovec names pinned guest memory the caller
            // has permission to read, and `iov` outlives the call.
            unsafe { libc::pwritev(fd, iov.as_ptr(), iov.len() as i32, at) }
        })
    }

    fn check_prot(&self, need: Prot, what: &'static str) -> Result<()> {
        if self.subs.iter().all(|sub| sub.prot.contains(need)) {
            Ok(())
        } else {
            Err(Error::new(ErrorKind::PermissionDenied, what))
        }
    }

    /// Run `syscall` over the list in `IOV_MAX` sized pieces until the
    /// list is done or a piece comes back short.
    fn transfer(
        &self,
        file: &File,
        offset: u64,
        syscall: impl Fn(i32, &[libc::iovec], libc::off_t) -> isize,
    ) -> Result<usize> {
        let fd = file.as_raw_fd();
        let mut done = 0usize;
        let mut at = offset;
        for piece in self.subs.chunks(IOV_MAX) {
            let want: usize = piece.iter().map(SubMapping::len).sum();
            let iov: Vec<libc::iovec> = piece
                .iter()
                .map(|sub| libc::iovec {
                    // Safety: the pointer is only handed to the kernel,
                    // never dereferenced here.
                    iov_base: unsafe { sub.as_ptr() }.cast(),
                    iov_len: sub.len(),
                })
                .collect();
            let off = libc::off_t::try_from(at).map_err(|_| {
                Error::new(ErrorKind::InvalidInput, "offset is out of range")
            })?;
            let n = loop {
                let n = syscall(fd, &iov, off);
                if n >= 0 {
                    break n as usize;
                }
                let e = Error::last_os_error();
                if e.kind() != ErrorKind::Interrupted {
                    return Err(e);
                }
            };
            done += n;
            if n < want {
                break;
            }
            at = at.checked_add(n as u64).ok_or_else(|| {
                Error::new(ErrorKind::InvalidInput, "offset is out of range")
            })?;
        }
        Ok(done)
    }
}
