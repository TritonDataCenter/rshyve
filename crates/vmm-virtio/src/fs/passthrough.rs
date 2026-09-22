// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Host filesystem passthrough for virtio-fs.
//!
//! # Security model
//!
//! The export's root is one directory descriptor taken at construction,
//! and every later access is an `*at()` call relative to a descriptor
//! this module already holds. The host never resolves a whole path the
//! guest sent, so a name cannot lead out of the export.
//!
//! * `O_CLOEXEC` on every descriptor. The VMM replaces its own process
//!   image to reboot a guest, and an inherited fd would leak into the
//!   new image.
//! * `O_NOFOLLOW` and `AT_SYMLINK_NOFOLLOW` on every access. The guest
//!   resolves symlinks, never the host.
//! * Regular files, directories and symlinks only. A device, FIFO or
//!   socket node is refused.
//! * A name is refused when it is empty, `.` or `..`, or holds a `/` or
//!   a NUL.
//!
//! An inode entry pins a descriptor until the guest FORGETs it, and a
//! handle pins one until the guest RELEASEs it. The guest controls both,
//! so both tables are bounded (see [`PtLimits`]).
//!
//! # Layout
//!
//! [`tables`] holds the two caches, [`sys`] the syscalls, [`write`] the
//! operations that change the export, and this file the read path and
//! the state they share.

use std::ffi::CStr;
use std::os::fd::RawFd;
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::fuse::{self, FuseAttr, FuseEntryOut, FuseStatfsOut};

mod sys;
mod tables;
mod write;

#[cfg(test)]
mod tests;

use self::sys::{
    dev_ino, fstat, fstatat_nofollow, get_errno, open_child, openat_nofollow,
    parse_open_flags, path_cstring, reopen_fd, set_errno, stat_to_attr,
};
use self::tables::{
    DirSnapshot, FileType, Handle, HandleTable, Inode, InodeTable,
};

pub use self::sys::{raise_fd_limit, validate_name};
pub use self::tables::PtLimits;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum PtError {
    #[error("io error: {0}")]
    Io(std::io::Error),
    #[error("invalid name: {0:?}")]
    InvalidName(Vec<u8>),
    #[error("unknown nodeid {0}")]
    UnknownNode(u64),
    #[error("unknown handle {0}")]
    UnknownHandle(u64),
    #[error("read-only passthrough: operation not allowed")]
    ReadOnly,
    #[error("unsupported file type")]
    UnsupportedType,
}

impl PtError {
    /// The positive host errno. The FUSE reply carries its negation.
    pub fn to_errno(&self) -> i32 {
        match self {
            PtError::Io(e) => e.raw_os_error().unwrap_or(libc::EIO),
            PtError::InvalidName(_) => libc::EINVAL,
            PtError::UnknownNode(_) => libc::ESTALE,
            PtError::UnknownHandle(_) => libc::EBADF,
            PtError::ReadOnly => libc::EROFS,
            PtError::UnsupportedType => libc::ENOTSUP,
        }
    }
}

type PtResult<T> = Result<T, PtError>;

/// The directory itself, for an `openat` relative to its own fd.
const DOT_DIR: &CStr = c".";

fn last_os() -> PtError {
    PtError::Io(std::io::Error::last_os_error())
}

fn errno(code: i32) -> PtError {
    PtError::Io(std::io::Error::from_raw_os_error(code))
}

// ---------------------------------------------------------------------------
// Passthrough core
// ---------------------------------------------------------------------------

pub struct Passthrough {
    read_only: bool,
    limits: PtLimits,
    inodes: Mutex<InodeTable>,
    handles: Mutex<HandleTable>,
}

impl Passthrough {
    pub fn new(path: &Path, read_only: bool) -> PtResult<Self> {
        Self::with_limits(path, read_only, PtLimits::default())
    }

    pub fn with_limits(
        path: &Path,
        read_only: bool,
        limits: PtLimits,
    ) -> PtResult<Self> {
        let cpath = path_cstring(path)?;
        // SAFETY: `cpath` owns its NUL-terminated bytes for the whole
        // call, which only reads them.
        let fd = unsafe {
            libc::open(
                cpath.as_ptr(),
                libc::O_RDONLY
                    | libc::O_DIRECTORY
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(last_os());
        }

        let mut inodes = InodeTable::new();
        let (dev, ino) = dev_ino(&fstat(fd)?);
        let root = Inode {
            fd: Some(fd),
            ftype: FileType::Directory,
            dev,
            ino,
            lookups: u64::MAX / 2, // pin forever
            sym_parent: None,
            sym_name: None,
        };
        inodes
            .by_devino
            .insert((root.dev, root.ino), fuse::FUSE_ROOT_ID);
        inodes.by_id.insert(fuse::FUSE_ROOT_ID, root);

        Ok(Self {
            read_only,
            limits,
            inodes: Mutex::new(inodes),
            handles: Mutex::new(HandleTable::new(limits)),
        })
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    // -------------------------------------------------------------------
    // FUSE operations
    // -------------------------------------------------------------------

    pub fn lookup(&self, parent: u64, name: &CStr) -> PtResult<FuseEntryOut> {
        let name_bytes = name.to_bytes();
        validate_name(name_bytes)?;

        let parent_fd = self.with_inode(parent, |ino| match ino.fd {
            Some(fd) if ino.ftype == FileType::Directory => Ok(fd),
            _ => Err(errno(libc::ENOTDIR)),
        })?;

        // Probe with fstatat first to find symlinks: on illumos an
        // O_NOFOLLOW open of a symlink fails with ELOOP.
        let probe = fstatat_nofollow(parent_fd, name)?;
        let ftype = FileType::from_mode(probe.st_mode as u32)
            .ok_or(PtError::UnsupportedType)?;

        let (fd_opt, st) = open_child(parent_fd, name, &probe, ftype)?;

        // A known (dev, ino) reuses its nodeid, so the guest's cache
        // stays consistent.
        let (dev, ino) = dev_ino(&st);

        let mut inodes = self.inodes.lock().unwrap();
        let nodeid = if let Some(&existing) = inodes.by_devino.get(&(dev, ino))
        {
            // Keep the existing entry's fd.
            if let Some(fd) = fd_opt {
                // SAFETY: `open_child` returned this descriptor and no
                // table took it, so this frame is its only owner.
                unsafe { libc::close(fd) };
            }
            if let Some(entry) = inodes.by_id.get_mut(&existing) {
                entry.lookups = entry.lookups.saturating_add(1);
            }
            existing
        } else {
            if inodes.by_id.len() >= self.limits.max_inodes {
                if let Some(fd) = fd_opt {
                    // SAFETY: the entry was never inserted, so nothing
                    // but this frame holds the descriptor.
                    unsafe { libc::close(fd) };
                }
                return Err(errno(libc::ENFILE));
            }
            let id = inodes.alloc_id();
            let (sym_parent, sym_name) = if ftype == FileType::Symlink {
                (Some(parent), Some(name.to_owned()))
            } else {
                (None, None)
            };
            if let Some(sp) = sym_parent {
                if let Some(p) = inodes.by_id.get_mut(&sp) {
                    p.lookups = p.lookups.saturating_add(1);
                }
            }
            let new = Inode {
                fd: fd_opt,
                ftype,
                dev,
                ino,
                lookups: 1,
                sym_parent,
                sym_name,
            };
            inodes.by_id.insert(id, new);
            inodes.by_devino.insert((dev, ino), id);
            id
        };

        Ok(FuseEntryOut {
            nodeid,
            generation: 0,
            entry_valid: 1,
            attr_valid: 1,
            entry_valid_nsec: 0,
            attr_valid_nsec: 0,
            attr: stat_to_attr(&st),
        })
    }

    pub fn forget(&self, nodeid: u64, nlookup: u64) {
        if nodeid == fuse::FUSE_ROOT_ID {
            return;
        }
        let mut inodes = self.inodes.lock().unwrap();
        let Some(entry) = inodes.by_id.get_mut(&nodeid) else {
            return;
        };
        if nlookup >= entry.lookups {
            let fd = entry.fd;
            let dev = entry.dev;
            let ino = entry.ino;
            let parent = entry.sym_parent;
            inodes.by_id.remove(&nodeid);
            inodes.by_devino.remove(&(dev, ino));
            if let Some(fd) = fd {
                // SAFETY: the entry is out of both tables, so this is the
                // last owner. Only the FUSE worker closes a guest-held fd,
                // so no syscall can be running on it.
                unsafe { libc::close(fd) };
            }
            if let Some(p) = parent {
                drop(inodes);
                self.forget(p, 1);
            }
        } else {
            entry.lookups -= nlookup;
        }
    }

    pub fn getattr(&self, nodeid: u64) -> PtResult<FuseAttr> {
        let st = self.stat_node(nodeid)?;
        Ok(stat_to_attr(&st))
    }

    pub fn statfs(&self, nodeid: u64) -> PtResult<FuseStatfsOut> {
        let fd = self.with_inode(nodeid, |ino| {
            ino.fd.ok_or_else(|| errno(libc::EBADF))
        })?;
        // SAFETY: `libc::statvfs` is a repr(C) struct of integers, so
        // all zeroes is a valid instance.
        let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `vfs` is live for the call and fstatvfs writes only
        // that struct. A descriptor the worker has since closed answers
        // EBADF and writes nothing.
        let rc = unsafe { libc::fstatvfs(fd, &mut vfs) };
        if rc != 0 {
            return Err(last_os());
        }
        Ok(FuseStatfsOut {
            blocks: vfs.f_blocks as u64,
            bfree: vfs.f_bfree as u64,
            bavail: vfs.f_bavail as u64,
            files: vfs.f_files as u64,
            ffree: vfs.f_ffree as u64,
            bsize: vfs.f_bsize as u32,
            namelen: vfs.f_namemax as u32,
            frsize: vfs.f_frsize as u32,
            padding: 0,
            spare: [0; 6],
        })
    }

    /// FUSE_ACCESS: checks only that the node exists.
    ///
    /// The Linux virtio_fs mount forces `default_permissions`, so the
    /// guest kernel checks the mode bits from GETATTR attributes.
    pub fn access(&self, nodeid: u64, _mask: u32) -> PtResult<()> {
        self.stat_node(nodeid)?;
        Ok(())
    }

    pub fn readlink(&self, nodeid: u64) -> PtResult<Vec<u8>> {
        let (parent_fd, name) = self.with_inode(nodeid, |ino| {
            if ino.ftype != FileType::Symlink {
                return Err(errno(libc::EINVAL));
            }
            let parent = ino.sym_parent.ok_or_else(|| errno(libc::EIO))?;
            let name = ino.sym_name.clone().ok_or_else(|| errno(libc::EIO))?;
            Ok((parent, name))
        })?;
        let pfd = self.with_inode(parent_fd, |p| {
            p.fd.ok_or_else(|| errno(libc::EBADF))
        })?;

        let mut buf = vec![0u8; libc::PATH_MAX as usize];
        // SAFETY: `name` is a NUL-terminated `CString` this frame
        // holds, and `buf.len()` bounds the write to the PATH_MAX bytes
        // `buf` really owns. The return is clamped to that length below.
        let n = unsafe {
            libc::readlinkat(
                pfd,
                name.as_ptr(),
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
            )
        };
        if n < 0 {
            return Err(last_os());
        }
        buf.truncate(n as usize);
        Ok(buf)
    }

    /// FUSE_OPEN. `flags` is the guest's Linux `O_*` value.
    pub fn open(&self, nodeid: u64, flags: u32) -> PtResult<u64> {
        let flags = parse_open_flags(flags)?;
        // A read-only share answers EROFS like a read-only mount.
        // `mutates` includes O_TRUNC, which truncates in any access mode.
        if self.read_only && flags.mutates() {
            return Err(PtError::ReadOnly);
        }

        let new_fd = self.with_inode(nodeid, |ino| {
            if ino.ftype != FileType::Regular {
                return Err(errno(libc::EISDIR));
            }
            let fd = ino.fd.ok_or_else(|| errno(libc::EBADF))?;
            // Data-path flags only. `fd` fixes the file, so O_CREAT,
            // O_EXCL, O_DIRECTORY and O_NOFOLLOW do not apply.
            reopen_fd(fd, flags.host_io_flags())
        })?;

        self.handles
            .lock()
            .unwrap()
            .insert(Handle::File { fd: new_fd })
    }

    pub fn release(&self, fh: u64) -> PtResult<()> {
        let handle = self.handles.lock().unwrap().remove(fh)?;
        match handle {
            Handle::File { fd, .. } => {
                // SAFETY: `remove` took the handle out of the table, so
                // this frame owns the descriptor alone.
                unsafe { libc::close(fd) };
            }
            Handle::Dir { .. } => {
                return Err(errno(libc::EBADF));
            }
        }
        Ok(())
    }

    pub fn read(&self, fh: u64, offset: u64, size: u32) -> PtResult<Vec<u8>> {
        let fd = {
            let handles = self.handles.lock().unwrap();
            match handles.by_id.get(&fh) {
                Some(Handle::File { fd, .. }) => *fd,
                Some(_) => return Err(errno(libc::EISDIR)),
                None => return Err(PtError::UnknownHandle(fh)),
            }
        };
        let mut buf = vec![0u8; size as usize];
        // SAFETY: `buf` is a live allocation of `size` bytes and
        // `buf.len()` bounds what pread may write into it. The return is
        // clamped to that length below.
        let n = unsafe {
            libc::pread(
                fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                offset as libc::off_t,
            )
        };
        if n < 0 {
            return Err(last_os());
        }
        buf.truncate(n as usize);
        Ok(buf)
    }

    pub fn opendir(&self, nodeid: u64) -> PtResult<u64> {
        let dir_fd = self.with_inode(nodeid, |ino| {
            if ino.ftype != FileType::Directory {
                return Err(errno(libc::ENOTDIR));
            }
            let fd = ino.fd.ok_or_else(|| errno(libc::EBADF))?;
            // Not `dup`: a shared open file description shares the
            // directory offset, which readdir leaves at EOF for every
            // later OPENDIR of the inode. Opening "." gives a new one.
            openat_nofollow(fd, DOT_DIR, libc::O_RDONLY | libc::O_DIRECTORY)
        })?;

        // SAFETY: `dir_fd` is the descriptor `openat_nofollow` just
        // returned and nothing else holds it, so fdopendir may take it.
        let dirp = unsafe { libc::fdopendir(dir_fd) };
        if dirp.is_null() {
            // SAFETY: fdopendir failed, so it did not take `dir_fd` and
            // this frame still owns it.
            unsafe { libc::close(dir_fd) };
            return Err(last_os());
        }

        // Check against the budget while reading, so a directory that is
        // too big is refused before it is held in memory. The FUSE worker
        // is the only caller, so the budget cannot shrink under this
        // loop. `insert` checks again.
        let budget = self.handles.lock().unwrap().snapshot_budget();
        let mut bytes = 0usize;
        let mut entries = Vec::new();
        loop {
            set_errno(0);
            // SAFETY: `dirp` is the stream fdopendir returned. Every
            // path that closes it returns at once, so it is open here.
            let entp = unsafe { libc::readdir(dirp) };
            if entp.is_null() {
                let errno = get_errno();
                if errno != 0 {
                    // SAFETY: `dirp` is still open and this is its only
                    // close on the way out.
                    unsafe { libc::closedir(dirp) };
                    return Err(PtError::Io(
                        std::io::Error::from_raw_os_error(errno),
                    ));
                }
                break;
            }

            // SAFETY: readdir returned non-null, so `entp` points at a
            // `dirent` the stream owns. The borrow ends with this
            // iteration, before the next readdir may reuse the storage.
            let dirent = unsafe { &*entp };
            let name_ptr = dirent.d_name.as_ptr() as *const libc::c_char;
            // SAFETY: `d_name` is a NUL-terminated array inside that
            // same `dirent`, so the string is in bounds and outlives the
            // `CStr`, which is copied out before the next readdir.
            let name_cstr = unsafe { CStr::from_ptr(name_ptr) };
            let name_bytes = name_cstr.to_bytes();
            if name_bytes == b"." || name_bytes == b".." {
                // The FUSE client supplies these itself.
                continue;
            }
            let entry = DirSnapshot {
                name: name_cstr.to_owned(),
                ino: dirent.d_ino as u64,
            };
            bytes = bytes.saturating_add(entry.cost());
            if bytes > budget {
                // SAFETY: `dirp` is still open and this is its only
                // close on the way out.
                unsafe { libc::closedir(dirp) };
                return Err(errno(libc::ENOMEM));
            }
            entries.push(entry);
        }

        // SAFETY: `dirp` is open here and closed once. closedir also
        // closes the descriptor fdopendir took, so `dir_fd` needs no
        // close of its own.
        unsafe { libc::closedir(dirp) };

        self.handles.lock().unwrap().insert(Handle::Dir {
            entries: Arc::from(entries),
            bytes,
        })
    }

    pub fn releasedir(&self, fh: u64) -> PtResult<()> {
        let handle = self.handles.lock().unwrap().remove(fh)?;
        match handle {
            Handle::Dir { .. } => Ok(()),
            Handle::File { fd, .. } => {
                // SAFETY: `remove` took the handle out of the table, so
                // this frame owns the descriptor alone.
                unsafe { libc::close(fd) };
                Err(errno(libc::EBADF))
            }
        }
    }

    /// Iterate entries from a directory handle starting at `offset`.
    ///
    /// The callback returns `false` to stop, for example when the reply
    /// buffer is full.
    pub fn readdir_each<F: FnMut(u64, &CStr, u32, u64) -> bool>(
        &self,
        fh: u64,
        offset: u64,
        mut f: F,
    ) -> PtResult<()> {
        let handles = self.handles.lock().unwrap();
        let snap = match handles.by_id.get(&fh) {
            Some(Handle::Dir { entries, .. }) => Arc::clone(entries),
            Some(_) => return Err(errno(libc::ENOTDIR)),
            None => return Err(PtError::UnknownHandle(fh)),
        };
        drop(handles);

        // `offset` is the index of the next entry the client wants. Each
        // entry's `off` is the index after it.
        let start = offset as usize;
        for (idx, entry) in snap.iter().enumerate().skip(start) {
            let next_off = (idx + 1) as u64;
            // illumos `struct dirent` has no `d_type`, and a stat per
            // entry is costly. The guest stats what it opens, and
            // READDIRPLUS carries full attributes.
            const DT_UNKNOWN: u32 = 0;
            if !f(entry.ino, entry.name.as_c_str(), DT_UNKNOWN, next_off) {
                break;
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------

    fn stat_node(&self, nodeid: u64) -> PtResult<libc::stat> {
        let (kind, fd_opt, parent_name) = self.with_inode(nodeid, |ino| {
            Ok((
                ino.ftype,
                ino.fd,
                ino.sym_parent.and_then(|p| {
                    ino.sym_name.as_ref().map(|n| (p, n.clone()))
                }),
            ))
        })?;

        match (kind, fd_opt, parent_name) {
            (_, Some(fd), _) => fstat(fd),
            (FileType::Symlink, None, Some((parent, name))) => {
                let pfd = self.with_inode(parent, |p| {
                    p.fd.ok_or_else(|| errno(libc::EBADF))
                })?;
                fstatat_nofollow(pfd, name.as_c_str())
            }
            _ => Err(errno(libc::ESTALE)),
        }
    }

    fn with_inode<R, F>(&self, nodeid: u64, f: F) -> PtResult<R>
    where
        F: FnOnce(&Inode) -> PtResult<R>,
    {
        let inodes = self.inodes.lock().unwrap();
        let ino = inodes
            .by_id
            .get(&nodeid)
            .ok_or(PtError::UnknownNode(nodeid))?;
        f(ino)
    }

    fn parent_fd(&self, nodeid: u64) -> PtResult<RawFd> {
        self.with_inode(nodeid, |ino| match ino.fd {
            Some(fd) if ino.ftype == FileType::Directory => Ok(fd),
            Some(_) => Err(errno(libc::ENOTDIR)),
            None => Err(errno(libc::EBADF)),
        })
    }

    /// Close every guest-held fd and empty the inode and handle tables.
    /// Idempotent.
    ///
    /// The pinned root inode stays: it holds the export's own
    /// descriptor, which the device needs to serve after a guest reset.
    pub fn clear(&self) {
        let mut handles = self
            .handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for h in handles.by_id.values() {
            if let Handle::File { fd, .. } = h {
                // SAFETY: the handle table is the only owner, and it is
                // emptied below, so no fd closes twice. Callers run this on
                // the FUSE worker or after joining it, so no syscall is in
                // flight on these fds.
                unsafe { libc::close(*fd) };
            }
        }
        handles.by_id.clear();
        handles.next_id = 1;
        handles.snapshot_bytes = 0;
        drop(handles);

        let mut inodes = self
            .inodes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inodes.by_id.retain(|id, ino| {
            if *id == fuse::FUSE_ROOT_ID {
                return true;
            }
            if let Some(fd) = ino.fd {
                // SAFETY: `retain` drops the entry that owns this
                // descriptor, so the close is its last use. The root is
                // returned above and keeps its own fd.
                unsafe { libc::close(fd) };
            }
            false
        });
        inodes.by_devino.retain(|_, id| *id == fuse::FUSE_ROOT_ID);
        inodes.next_id = 2;
    }

    /// fsync every open regular-file handle. Attempts all of them and
    /// reports the first failure.
    pub fn sync_all(&self) -> std::io::Result<()> {
        let handles = self
            .handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut first: Option<std::io::Error> = None;
        for h in handles.by_id.values() {
            if let Handle::File { fd, .. } = h {
                // SAFETY: fsync takes no pointer, and the handle table
                // holds this descriptor open under the lock this loop
                // still owns.
                if unsafe { libc::fsync(*fd) } != 0 && first.is_none() {
                    first = Some(std::io::Error::last_os_error());
                }
            }
        }
        match first {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Drop for Passthrough {
    fn drop(&mut self) {
        // A panic inside Drop during an unwind aborts the process, so a
        // poisoned mutex must not be unwrapped here.
        let inodes = self
            .inodes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for ino in inodes.by_id.values() {
            if let Some(fd) = ino.fd {
                // SAFETY: `Drop` has the only reference to the tables,
                // so each descriptor is closed once and nothing can be
                // using it afterwards.
                unsafe { libc::close(fd) };
            }
        }
        let handles = self
            .handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for h in handles.by_id.values() {
            if let Handle::File { fd, .. } = h {
                // SAFETY: as above, the last owner of each descriptor.
                unsafe { libc::close(*fd) };
            }
        }
    }
}
