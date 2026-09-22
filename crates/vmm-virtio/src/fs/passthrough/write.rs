// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The operations that change the export.
//!
//! Each one is refused on a read-only share before it touches the host,
//! and each name is validated before it reaches a syscall.

use std::ffi::{CStr, CString};

use super::fuse::{self, FuseAttr, FuseEntryOut};
use super::sys::{
    build_timespec, dev_ino, fstat, openat_nofollow, parse_open_flags,
    stat_to_attr, validate_name,
};
use super::tables::{ifmt, FileType, Handle, Inode};
use super::{errno, last_os, Passthrough, PtError, PtResult};

impl Passthrough {
    /// FUSE_CREATE: atomic create-and-open. `flags` is the guest's Linux
    /// `O_*` value.
    pub fn create(
        &self,
        parent: u64,
        name: &CStr,
        flags: u32,
        mode: u32,
        umask: u32,
    ) -> PtResult<(FuseEntryOut, u64, bool)> {
        validate_name(name.to_bytes())?;
        if self.read_only {
            return Err(PtError::ReadOnly);
        }
        let flags = parse_open_flags(flags)?;

        let parent_fd = self.parent_fd(parent)?;

        // O_EXCL is the only creation flag passed through: guests use it
        // to take lock files. O_NONBLOCK because O_CREAT opens an existing
        // name, which can be a FIFO, and without the flag the open waits
        // for a peer and blocks the single FUSE worker.
        let mut open_flags = flags.host_io_flags()
            | libc::O_CREAT
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | libc::O_CLOEXEC;
        if flags.excl {
            open_flags |= libc::O_EXCL;
        }
        let effective_mode = (mode & !umask) & 0o7777;

        // SAFETY: `name` is a NUL-terminated `CStr` the caller owns
        // past the call, which only reads it. The variadic mode argument
        // is required because `open_flags` carries O_CREAT.
        let fh_fd = unsafe {
            libc::openat(
                parent_fd,
                name.as_ptr(),
                open_flags,
                effective_mode as libc::c_int,
            )
        };
        if fh_fd < 0 {
            return Err(last_os());
        }
        let st = fstat(fh_fd).inspect_err(|_| {
            // SAFETY: `fh_fd` is the descriptor the openat above
            // returned and no table holds it yet, so this frame is its
            // only owner.
            unsafe { libc::close(fh_fd) };
        })?;
        // O_CREAT opens whatever the name already holds, so check the
        // type on the descriptor.
        if FileType::from_mode(st.st_mode as u32) != Some(FileType::Regular) {
            // SAFETY: `fh_fd` is still this frame's alone; the handle
            // table never saw it.
            unsafe { libc::close(fh_fd) };
            return Err(PtError::UnsupportedType);
        }

        // A second, O_RDONLY fd for the inode cache. The name resolves
        // again, and anything with write access to the export can swap
        // it in between, so both descriptors must be the same file.
        let ino_fd = openat_nofollow(parent_fd, name, libc::O_RDONLY)
            .inspect_err(|_| {
                // SAFETY: the second open failed, so `fh_fd` is the one
                // descriptor this frame still owns.
                unsafe { libc::close(fh_fd) };
            })?;
        let ino_st = fstat(ino_fd).inspect_err(|_| {
            // SAFETY: both descriptors were opened above and neither is
            // in a table yet, so this frame owns them both and each is
            // closed once.
            unsafe {
                libc::close(ino_fd);
                libc::close(fh_fd);
            }
        })?;
        if ino_st.st_dev != st.st_dev || ino_st.st_ino != st.st_ino {
            // SAFETY: as above, this frame is the only owner of both.
            unsafe {
                libc::close(ino_fd);
                libc::close(fh_fd);
            }
            return Err(errno(libc::ESTALE));
        }

        let (dev, ino) = dev_ino(&st);
        let nodeid = {
            let mut inodes = self.inodes.lock().unwrap();
            if let Some(&existing) = inodes.by_devino.get(&(dev, ino)) {
                // Without O_EXCL, two racing creates can reach the same
                // file. Keep the cached entry's fd.
                // SAFETY: the cached entry keeps its own descriptor,
                // and `ino_fd` was never inserted, so this frame is its
                // only owner.
                unsafe { libc::close(ino_fd) };
                if let Some(entry) = inodes.by_id.get_mut(&existing) {
                    entry.lookups = entry.lookups.saturating_add(1);
                }
                existing
            } else {
                if inodes.by_id.len() >= self.limits.max_inodes {
                    drop(inodes);
                    // SAFETY: neither descriptor reached a table, so
                    // this frame owns both and closes each once.
                    unsafe {
                        libc::close(ino_fd);
                        libc::close(fh_fd);
                    }
                    return Err(errno(libc::ENFILE));
                }
                let id = inodes.alloc_id();
                inodes.by_id.insert(
                    id,
                    Inode {
                        fd: Some(ino_fd),
                        ftype: FileType::Regular,
                        dev,
                        ino,
                        lookups: 1,
                        sym_parent: None,
                        sym_name: None,
                    },
                );
                inodes.by_devino.insert((dev, ino), id);
                id
            }
        };

        let entry = FuseEntryOut {
            nodeid,
            generation: 0,
            entry_valid: 1,
            attr_valid: 1,
            entry_valid_nsec: 0,
            attr_valid_nsec: 0,
            attr: stat_to_attr(&st),
        };

        // The inode entry above holds a lookup the guest will FORGET
        // whatever happens to the handle, so a refused handle leaves
        // the table consistent.
        let fh = self
            .handles
            .lock()
            .unwrap()
            .insert(Handle::File { fd: fh_fd })?;

        // FUSE_CREATE has no "found existing" case: O_CREAT without
        // O_EXCL opens the existing file, so this is always true.
        Ok((entry, fh, true))
    }

    /// FUSE_MKNOD: restricted to regular files.
    pub fn mknod(
        &self,
        parent: u64,
        name: &CStr,
        mode: u32,
        _rdev: u32,
        umask: u32,
    ) -> PtResult<FuseEntryOut> {
        validate_name(name.to_bytes())?;
        if self.read_only {
            return Err(PtError::ReadOnly);
        }
        let ftype = mode & ifmt::MASK;
        if ftype != 0 && ftype != ifmt::REG {
            return Err(PtError::UnsupportedType);
        }
        let parent_fd = self.parent_fd(parent)?;
        let effective_mode = (mode & !umask) & 0o7777;
        // SAFETY: `name` is NUL-terminated and owned by the caller past
        // the call. O_CREAT is set, so the variadic mode is required.
        let fd = unsafe {
            libc::openat(
                parent_fd,
                name.as_ptr(),
                libc::O_CREAT
                    | libc::O_EXCL
                    | libc::O_RDONLY
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC,
                effective_mode as libc::c_int,
            )
        };
        if fd < 0 {
            return Err(last_os());
        }
        // SAFETY: the node is on disk now and `fd` was only used to
        // create it, so this frame is its only owner. `lookup` opens the
        // name again for the inode table.
        unsafe { libc::close(fd) };
        self.lookup(parent, name)
    }

    /// FUSE_MKDIR.
    pub fn mkdir(
        &self,
        parent: u64,
        name: &CStr,
        mode: u32,
        umask: u32,
    ) -> PtResult<FuseEntryOut> {
        validate_name(name.to_bytes())?;
        if self.read_only {
            return Err(PtError::ReadOnly);
        }
        let parent_fd = self.parent_fd(parent)?;
        let effective_mode = (mode & !umask) & 0o7777;
        // SAFETY: `name` is NUL-terminated and outlives the call, which
        // only reads it.
        let rc = unsafe {
            libc::mkdirat(
                parent_fd,
                name.as_ptr(),
                effective_mode as libc::mode_t,
            )
        };
        if rc != 0 {
            return Err(last_os());
        }
        self.lookup(parent, name)
    }

    /// FUSE_UNLINK.
    pub fn unlink(&self, parent: u64, name: &CStr) -> PtResult<()> {
        validate_name(name.to_bytes())?;
        if self.read_only {
            return Err(PtError::ReadOnly);
        }
        let parent_fd = self.parent_fd(parent)?;
        // SAFETY: `name` is NUL-terminated and outlives the call, which
        // only reads it.
        let rc = unsafe { libc::unlinkat(parent_fd, name.as_ptr(), 0) };
        if rc != 0 {
            return Err(last_os());
        }
        Ok(())
    }

    /// FUSE_RMDIR.
    pub fn rmdir(&self, parent: u64, name: &CStr) -> PtResult<()> {
        validate_name(name.to_bytes())?;
        if self.read_only {
            return Err(PtError::ReadOnly);
        }
        let parent_fd = self.parent_fd(parent)?;
        // SAFETY: `name` is NUL-terminated and outlives the call, which
        // only reads it.
        let rc = unsafe {
            libc::unlinkat(parent_fd, name.as_ptr(), libc::AT_REMOVEDIR)
        };
        if rc != 0 {
            return Err(last_os());
        }
        Ok(())
    }

    /// FUSE_RENAME / FUSE_RENAME2.
    ///
    /// `_flags` is the RENAME_* mask from rename2. renameat takes no
    /// flags, so the caller answers ENOSYS to a non-zero mask.
    pub fn rename(
        &self,
        old_parent: u64,
        old_name: &CStr,
        new_parent: u64,
        new_name: &CStr,
        _flags: u32,
    ) -> PtResult<()> {
        validate_name(old_name.to_bytes())?;
        validate_name(new_name.to_bytes())?;
        if self.read_only {
            return Err(PtError::ReadOnly);
        }
        let old_pfd = self.parent_fd(old_parent)?;
        let new_pfd = self.parent_fd(new_parent)?;
        // SAFETY: both names are NUL-terminated `CStr`s the caller owns
        // past the call, which only reads them.
        let rc = unsafe {
            libc::renameat(
                old_pfd,
                old_name.as_ptr(),
                new_pfd,
                new_name.as_ptr(),
            )
        };
        if rc != 0 {
            return Err(last_os());
        }
        Ok(())
    }

    /// FUSE_SYMLINK.
    pub fn symlink(
        &self,
        parent: u64,
        name: &CStr,
        target: &CStr,
    ) -> PtResult<FuseEntryOut> {
        validate_name(name.to_bytes())?;
        if self.read_only {
            return Err(PtError::ReadOnly);
        }
        if target.to_bytes().is_empty() {
            return Err(PtError::InvalidName(Vec::new()));
        }
        let parent_fd = self.parent_fd(parent)?;
        // SAFETY: `target` and `name` are NUL-terminated `CStr`s the
        // caller owns past the call, which only reads them.
        let rc = unsafe {
            libc::symlinkat(target.as_ptr(), parent_fd, name.as_ptr())
        };
        if rc != 0 {
            return Err(last_os());
        }
        self.lookup(parent, name)
    }

    /// FUSE_LINK: a hard link.
    pub fn link(
        &self,
        old_nodeid: u64,
        new_parent: u64,
        new_name: &CStr,
    ) -> PtResult<FuseEntryOut> {
        validate_name(new_name.to_bytes())?;
        if self.read_only {
            return Err(PtError::ReadOnly);
        }
        let old_fd = self.with_inode(old_nodeid, |ino| {
            if ino.ftype != FileType::Regular {
                return Err(PtError::UnsupportedType);
            }
            ino.fd.ok_or_else(|| errno(libc::EBADF))
        })?;
        let new_parent_fd = self.parent_fd(new_parent)?;

        // Link through the descriptor, not a name in the old parent, so
        // no name resolves again and the source cannot change.
        let path = CString::new(format!("/proc/self/fd/{old_fd}")).unwrap();
        // SAFETY: `path` and `new_name` are NUL-terminated and live for
        // the whole call, which only reads them.
        let rc = unsafe {
            libc::linkat(
                libc::AT_FDCWD,
                path.as_ptr(),
                new_parent_fd,
                new_name.as_ptr(),
                libc::AT_SYMLINK_FOLLOW,
            )
        };
        if rc != 0 {
            return Err(last_os());
        }
        self.lookup(new_parent, new_name)
    }

    /// FUSE_SETATTR.
    ///
    /// Applies the fields that `valid` selects, through the open file
    /// handle if the guest gives one, else through the inode fd.
    pub fn setattr(
        &self,
        nodeid: u64,
        valid: u32,
        fh: Option<u64>,
        size: u64,
        mode: u32,
        uid: u32,
        gid: u32,
        atime: (u64, u32, bool), // (sec, nsec, now)
        mtime: (u64, u32, bool),
    ) -> PtResult<FuseAttr> {
        if self.read_only {
            return Err(PtError::ReadOnly);
        }

        let fd = if let Some(h) = fh {
            let handles = self.handles.lock().unwrap();
            match handles.by_id.get(&h) {
                Some(Handle::File { fd, .. }) => *fd,
                _ => return Err(PtError::UnknownHandle(h)),
            }
        } else {
            self.with_inode(nodeid, |ino| {
                ino.fd.ok_or_else(|| errno(libc::EBADF))
            })?
        };

        if valid & fuse::FATTR_MODE != 0 {
            let m = (mode & 0o7777) as libc::mode_t;
            // SAFETY: fchmod takes no pointer. `fd` names a descriptor
            // the handle or inode table holds open, and only the FUSE
            // worker closes one of those.
            let rc = unsafe { libc::fchmod(fd, m) };
            if rc != 0 {
                return Err(last_os());
            }
        }
        if valid & (fuse::FATTR_UID | fuse::FATTR_GID) != 0 {
            let u = if valid & fuse::FATTR_UID != 0 {
                uid as libc::uid_t
            } else {
                (-1i32) as libc::uid_t
            };
            let g = if valid & fuse::FATTR_GID != 0 {
                gid as libc::gid_t
            } else {
                (-1i32) as libc::gid_t
            };
            // SAFETY: as above, no pointer crosses the boundary and the
            // descriptor is held open by a table.
            let rc = unsafe { libc::fchown(fd, u, g) };
            if rc != 0 {
                return Err(last_os());
            }
        }
        if valid & fuse::FATTR_SIZE != 0 {
            // SAFETY: as above, no pointer crosses the boundary and the
            // descriptor is held open by a table.
            let rc = unsafe { libc::ftruncate(fd, size as libc::off_t) };
            if rc != 0 {
                return Err(last_os());
            }
        }
        if valid
            & (fuse::FATTR_ATIME
                | fuse::FATTR_MTIME
                | fuse::FATTR_ATIME_NOW
                | fuse::FATTR_MTIME_NOW)
            != 0
        {
            let a = build_timespec(
                valid & fuse::FATTR_ATIME != 0,
                atime.2,
                atime.0,
                atime.1,
            );
            let m = build_timespec(
                valid & fuse::FATTR_MTIME != 0,
                mtime.2,
                mtime.0,
                mtime.1,
            );
            let times = [a, m];
            // SAFETY: `times` is a live two-element array, which is
            // exactly what futimens reads, and it only reads it.
            let rc = unsafe { libc::futimens(fd, times.as_ptr()) };
            if rc != 0 {
                return Err(last_os());
            }
        }

        let st = fstat(fd)?;
        Ok(stat_to_attr(&st))
    }

    /// FUSE_WRITE.
    pub fn write(&self, fh: u64, offset: u64, data: &[u8]) -> PtResult<u32> {
        if self.read_only {
            return Err(PtError::ReadOnly);
        }
        let fd = {
            let handles = self.handles.lock().unwrap();
            match handles.by_id.get(&fh) {
                Some(Handle::File { fd, .. }) => *fd,
                Some(_) => return Err(errno(libc::EISDIR)),
                None => return Err(PtError::UnknownHandle(fh)),
            }
        };
        // SAFETY: `data` is a live slice and `data.len()` bounds what
        // pwrite may read from it. The call does not keep the pointer.
        let n = unsafe {
            libc::pwrite(
                fd,
                data.as_ptr() as *const libc::c_void,
                data.len(),
                offset as libc::off_t,
            )
        };
        if n < 0 {
            return Err(last_os());
        }
        Ok(n as u32)
    }

    /// FUSE_FSYNC / FUSE_FSYNCDIR.
    pub fn fsync(&self, fh: u64, datasync: bool) -> PtResult<()> {
        let fd = {
            let handles = self.handles.lock().unwrap();
            match handles.by_id.get(&fh) {
                Some(Handle::File { fd, .. }) => *fd,
                Some(Handle::Dir { .. }) => {
                    // A directory handle is a snapshot with no fd.
                    return Ok(());
                }
                None => return Err(PtError::UnknownHandle(fh)),
            }
        };
        let rc = if datasync {
            #[cfg(any(
                target_os = "linux",
                target_os = "illumos",
                target_os = "solaris"
            ))]
            // SAFETY: fdatasync takes no pointer, and the handle table
            // holds this descriptor open.
            unsafe {
                libc::fdatasync(fd)
            }
            #[cfg(not(any(
                target_os = "linux",
                target_os = "illumos",
                target_os = "solaris"
            )))]
            // SAFETY: the host has no fdatasync; fsync takes no pointer
            // and the handle table holds this descriptor open.
            unsafe {
                libc::fsync(fd)
            }
        } else {
            // SAFETY: fsync takes no pointer, and the handle table holds
            // this descriptor open.
            unsafe { libc::fsync(fd) }
        };
        if rc != 0 {
            return Err(last_os());
        }
        Ok(())
    }
}
