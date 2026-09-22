// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The two tables one export keeps: an inode per name the guest holds,
//! and a handle per open file or directory.
//!
//! Both are bounded. Every entry pins a host descriptor, or the memory
//! of a directory snapshot, until the guest releases it.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::RawFd;
use std::sync::Arc;

use super::{errno, PtError, PtResult};

/// Bounds on the host state one guest may hold through this device.
///
/// Every cached inode of a regular file or directory, and every open
/// file handle, pins one host fd until the guest sends FORGET or
/// RELEASE. Past the process fd limit, the VMM's control socket,
/// console and hotplug paths fail too. So the tables are capped, and
/// the guest sees ENFILE or EMFILE as on any full system.
#[derive(Debug, Clone, Copy)]
pub struct PtLimits {
    /// Cached inodes, root included.
    pub max_inodes: usize,
    /// Open file and directory handles together.
    pub max_handles: usize,
    /// Bytes of directory snapshots across every open directory handle.
    pub max_snapshot_bytes: usize,
}

impl Default for PtLimits {
    /// Sized against the 65536 fds an illumos process may hold after
    /// [`super::raise_fd_limit`]: inodes and file handles together take
    /// at most 24576, and the VMM keeps the rest.
    fn default() -> Self {
        Self {
            max_inodes: 16384,
            max_handles: 8192,
            max_snapshot_bytes: 64 * 1024 * 1024,
        }
    }
}

/// File-type bits, widened to the type FUSE puts on the wire.
///
/// `libc`'s `S_IF*` constants have type `mode_t`, which is 16-bit on some
/// targets and 32-bit on others. Widen them once here, not at each call
/// site.
#[allow(clippy::unnecessary_cast)]
pub(super) mod ifmt {
    pub const MASK: u32 = libc::S_IFMT as u32;
    pub const REG: u32 = libc::S_IFREG as u32;
    pub const DIR: u32 = libc::S_IFDIR as u32;
    pub const LNK: u32 = libc::S_IFLNK as u32;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FileType {
    Regular,
    Directory,
    Symlink,
}

impl FileType {
    pub(super) fn from_mode(mode: u32) -> Option<Self> {
        match mode & ifmt::MASK {
            m if m == ifmt::REG => Some(FileType::Regular),
            m if m == ifmt::DIR => Some(FileType::Directory),
            m if m == ifmt::LNK => Some(FileType::Symlink),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(super) struct Inode {
    /// `None` for a symlink, which is reached through its parent and name.
    pub(super) fd: Option<RawFd>,
    pub(super) ftype: FileType,
    pub(super) dev: u64,
    pub(super) ino: u64,
    pub(super) lookups: u64,
    /// Parent nodeid of a symlink. The symlink holds an extra lookup on
    /// the parent to keep the parent's fd open.
    pub(super) sym_parent: Option<u64>,
    pub(super) sym_name: Option<CString>,
}

pub(super) struct InodeTable {
    pub(super) by_id: HashMap<u64, Inode>,
    pub(super) by_devino: HashMap<(u64, u64), u64>,
    pub(super) next_id: u64,
}

impl InodeTable {
    pub(super) fn new() -> Self {
        Self {
            by_id: HashMap::new(),
            by_devino: HashMap::new(),
            // nodeid 1 is the root.
            next_id: 2,
        }
    }

    pub(super) fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id < 2 {
            // On wrap-around, skip 0 and the root's 1.
            self.next_id = 2;
        }
        id
    }
}

// ---------------------------------------------------------------------------
// Handle table (open files + dirs)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(super) enum Handle {
    File {
        fd: RawFd,
    },
    /// Shared with every READDIR, so a listing copies the pointer, not
    /// the directory.
    Dir {
        entries: Arc<[DirSnapshot]>,
        bytes: usize,
    },
}

#[derive(Debug, Clone)]
pub(super) struct DirSnapshot {
    pub(super) name: CString,
    pub(super) ino: u64,
}

impl DirSnapshot {
    /// What one entry costs against [`PtLimits::max_snapshot_bytes`].
    pub(super) fn cost(&self) -> usize {
        size_of::<Self>() + self.name.as_bytes_with_nul().len()
    }
}

pub(super) struct HandleTable {
    pub(super) by_id: HashMap<u64, Handle>,
    pub(super) next_id: u64,
    /// Sum of `bytes` over every `Handle::Dir`.
    pub(super) snapshot_bytes: usize,
    limits: PtLimits,
}

impl HandleTable {
    pub(super) fn new(limits: PtLimits) -> Self {
        Self {
            by_id: HashMap::new(),
            next_id: 1,
            snapshot_bytes: 0,
            limits,
        }
    }

    pub(super) fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id == 0 {
            self.next_id = 1;
        }
        id
    }

    /// Room left for another directory snapshot.
    pub(super) fn snapshot_budget(&self) -> usize {
        self.limits
            .max_snapshot_bytes
            .saturating_sub(self.snapshot_bytes)
    }

    /// Add a handle, or refuse it and close what it carries.
    pub(super) fn insert(&mut self, handle: Handle) -> PtResult<u64> {
        if self.by_id.len() >= self.limits.max_handles {
            if let Handle::File { fd } = handle {
                // SAFETY: `handle` was moved into this function and is
                // refused, so it never reaches `by_id` and this frame is
                // the descriptor's only owner.
                unsafe { libc::close(fd) };
            }
            return Err(errno(libc::EMFILE));
        }
        if let Handle::Dir { bytes, .. } = &handle {
            if *bytes > self.snapshot_budget() {
                return Err(errno(libc::ENOMEM));
            }
            self.snapshot_bytes += *bytes;
        }
        let id = self.alloc_id();
        self.by_id.insert(id, handle);
        Ok(id)
    }

    pub(super) fn remove(&mut self, fh: u64) -> PtResult<Handle> {
        let handle =
            self.by_id.remove(&fh).ok_or(PtError::UnknownHandle(fh))?;
        if let Handle::Dir { bytes, .. } = &handle {
            self.snapshot_bytes = self.snapshot_bytes.saturating_sub(*bytes);
        }
        Ok(handle)
    }
}
