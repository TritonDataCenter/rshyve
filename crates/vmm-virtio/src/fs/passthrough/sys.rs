// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The syscalls this server makes, and the conversions around them.
//!
//! Every descriptor is opened `O_CLOEXEC`, because the VMM re-execs
//! itself on a guest reboot, and `O_NOFOLLOW`, because the guest, not
//! the server, resolves symlinks in the export.

use std::ffi::{CStr, CString};
use std::os::fd::RawFd;
use std::path::Path;

use super::fuse::{self, FuseAttr};
use super::{errno, last_os, FileType, PtError, PtResult};

pub(super) fn path_cstring(p: &Path) -> PtResult<CString> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        CString::new(p.as_os_str().as_bytes()).map_err(|_| {
            PtError::InvalidName(p.to_string_lossy().into_owned().into_bytes())
        })
    }
    #[cfg(not(unix))]
    {
        CString::new(p.to_string_lossy().into_owned())
            .map_err(|_| PtError::InvalidName(Vec::new()))
    }
}

/// Validate a single path component.
///
/// Refuses with EINVAL a name that is empty, `.` or `..`, or holds a
/// `/` or a NUL. A `CStr` cannot hold a NUL, but a byte slice can.
pub fn validate_name(name: &[u8]) -> PtResult<()> {
    if name.is_empty()
        || name == b"."
        || name == b".."
        || name.contains(&b'/')
        || name.contains(&0)
    {
        return Err(PtError::InvalidName(name.to_vec()));
    }
    Ok(())
}

/// Raise the soft `RLIMIT_NOFILE` to the hard limit.
///
/// The default soft limit is a few thousand and each cached inode costs
/// one fd. A guest that lists a large tree would reach it before
/// [`PtLimits`] does, and every fd-consuming path in the VMM would then
/// fail. Returns the soft limit now in force.
pub fn raise_fd_limit() -> std::io::Result<libc::rlim_t> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `lim` is a live `rlimit` this frame owns, and getrlimit
    // writes at most that one struct through the pointer.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let target = fd_limit_target(lim.rlim_max);
    if lim.rlim_cur >= target {
        return Ok(lim.rlim_cur);
    }
    lim.rlim_cur = target;
    // SAFETY: the same live struct, which setrlimit only reads.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(target)
}

/// macOS refuses a soft `RLIMIT_NOFILE` above `OPEN_MAX` even when the
/// hard limit is unlimited.
#[cfg(target_os = "macos")]
fn fd_limit_target(hard: libc::rlim_t) -> libc::rlim_t {
    hard.min(10240)
}

#[cfg(not(target_os = "macos"))]
fn fd_limit_target(hard: libc::rlim_t) -> libc::rlim_t {
    hard
}

/// Translate the guest's Linux open flags. Unknown bits get EINVAL.
pub(super) fn parse_open_flags(flags: u32) -> PtResult<fuse::OpenFlags> {
    fuse::OpenFlags::from_linux(flags).map_err(|_| errno(libc::EINVAL))
}

pub(super) fn fstat(fd: RawFd) -> PtResult<libc::stat> {
    // SAFETY: `libc::stat` is a repr(C) struct of integers and arrays
    // of them, so all zeroes is a valid instance.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is live for the call and fstat writes only that one
    // struct. A descriptor that is no longer open answers EBADF and
    // writes nothing.
    let rc = unsafe { libc::fstat(fd, &mut st) };
    if rc != 0 {
        return Err(last_os());
    }
    Ok(st)
}

pub(super) fn fstatat_nofollow(
    parent: RawFd,
    name: &CStr,
) -> PtResult<libc::stat> {
    // SAFETY: as in `fstat`, all zeroes is a valid `libc::stat`.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `name` is a `CStr`, so it is NUL-terminated, and the
    // caller keeps it alive past the call. fstatat writes only `st`.
    let rc = unsafe {
        libc::fstatat(parent, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW)
    };
    if rc != 0 {
        return Err(last_os());
    }
    Ok(st)
}

/// The `(dev, ino)` pair that names one host file.
///
/// `dev_t` and `ino_t` differ in width between illumos and the
/// development hosts, so the widening happens here once.
#[allow(clippy::unnecessary_cast)]
pub(super) fn dev_ino(st: &libc::stat) -> (u64, u64) {
    (st.st_dev as u64, st.st_ino as u64)
}

/// Open the child the caller probed, and hand back the stat of what was
/// really opened.
///
/// The open resolves the name again, and anything with write access to
/// the export can swap the file between probe and open. The descriptor
/// is the authority for type and identity, and a mismatch is ESTALE. A
/// symlink has no descriptor, so its probe stands.
pub(super) fn open_child(
    parent_fd: RawFd,
    name: &CStr,
    probe: &libc::stat,
    ftype: FileType,
) -> PtResult<(Option<RawFd>, libc::stat)> {
    if ftype == FileType::Symlink {
        return Ok((None, *probe));
    }
    let flags = if ftype == FileType::Directory {
        libc::O_RDONLY | libc::O_DIRECTORY
    } else {
        libc::O_RDONLY
    };
    let fd = openat_nofollow(parent_fd, name, flags)?;
    let opened = fstat(fd).inspect_err(|_| {
        // SAFETY: `openat_nofollow` returned `fd` on the line above and
        // no table holds it yet, so this frame is its only owner.
        unsafe { libc::close(fd) };
    })?;
    if FileType::from_mode(opened.st_mode as u32) != Some(ftype)
        || opened.st_dev != probe.st_dev
        || opened.st_ino != probe.st_ino
    {
        // SAFETY: `fd` never left this frame, so nothing else can close
        // it and the number cannot yet name another file.
        unsafe { libc::close(fd) };
        return Err(errno(libc::ESTALE));
    }
    Ok((Some(fd), opened))
}

/// Open `name` under `parent`, never following a final symlink and
/// never waiting.
///
/// `O_NONBLOCK` keeps the single FUSE worker running: without it, a FIFO
/// in the export blocks the open until a peer appears. The flag has no
/// effect on regular files and directories, and the caller checks what
/// it opened.
pub(super) fn openat_nofollow(
    parent: RawFd,
    name: &CStr,
    flags: libc::c_int,
) -> PtResult<RawFd> {
    // SAFETY: `name` is NUL-terminated and the caller owns it past the
    // call, which only reads it.
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(last_os());
    }
    Ok(fd)
}

// `libc::stat` field widths change between targets: `st_mode` and
// `st_nlink` are 16-bit on macOS and 32-bit on illumos, and `st_rdev`
// signedness differs. Keep every cast so all targets build.
#[allow(clippy::unnecessary_cast)]
pub fn stat_to_attr(st: &libc::stat) -> FuseAttr {
    FuseAttr {
        ino: st.st_ino as u64,
        size: st.st_size as u64,
        blocks: st.st_blocks as u64,
        atime: st.st_atime as u64,
        mtime: st.st_mtime as u64,
        ctime: st.st_ctime as u64,
        atimensec: nsec(st.st_atime_nsec),
        mtimensec: nsec(st.st_mtime_nsec),
        ctimensec: nsec(st.st_ctime_nsec),
        mode: st.st_mode as u32,
        nlink: st.st_nlink as u32,
        uid: st.st_uid as u32,
        gid: st.st_gid as u32,
        rdev: st.st_rdev as u32,
        blksize: st.st_blksize as u32,
        flags: 0,
    }
}

fn nsec(val: i64) -> u32 {
    (val as u32).min(999_999_999)
}

/// Re-open a file by its existing descriptor, with new flags.
///
/// The open goes through the procfs magic symlink, so no name resolves
/// again and the new descriptor is the same file. On illumos `pr_open`
/// for `PR_FD` refuses a mode wider than the source descriptor's unless
/// the caller is privileged, so a read-write share needs the VMM to run
/// as root. No `O_NOFOLLOW`: the magic symlink would trip ELOOP.
pub(super) fn reopen_fd(fd: RawFd, flags: libc::c_int) -> PtResult<RawFd> {
    let path = CString::new(format!("/proc/self/fd/{fd}")).unwrap();
    // SAFETY: `path` owns its NUL-terminated bytes for the whole call,
    // which only reads them.
    let new_fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
    if new_fd < 0 {
        return Err(last_os());
    }
    Ok(new_fd)
}

/// Build a timespec for futimens given FUSE setattr inputs.
pub(super) fn build_timespec(
    has_explicit: bool,
    want_now: bool,
    sec: u64,
    nsec: u32,
) -> libc::timespec {
    if want_now {
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_NOW,
        }
    } else if has_explicit {
        libc::timespec {
            tv_sec: sec as libc::time_t,
            tv_nsec: nsec as libc::c_long,
        }
    } else {
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        }
    }
}

// readdir signals an error only through errno, so the caller must clear
// it first. `std::io::Error` can read errno but cannot clear it.
#[cfg(any(target_os = "illumos", target_os = "solaris"))]
fn errno_ptr() -> *mut libc::c_int {
    // SAFETY: the accessor takes no argument and returns the address of
    // this thread's own errno, which libc keeps for the life of the
    // thread.
    unsafe { libc::___errno() }
}
#[cfg(target_os = "linux")]
fn errno_ptr() -> *mut libc::c_int {
    // SAFETY: as above, the address of this thread's own errno.
    unsafe { libc::__errno_location() }
}
#[cfg(target_os = "macos")]
fn errno_ptr() -> *mut libc::c_int {
    // SAFETY: as above, the address of this thread's own errno.
    unsafe { libc::__error() }
}

pub(super) fn set_errno(v: i32) {
    // SAFETY: `errno_ptr` gives this thread's own errno slot. It is
    // aligned, initialized, and not freed while the thread runs.
    unsafe { *errno_ptr() = v };
}

pub(super) fn get_errno() -> i32 {
    // SAFETY: the same slot, read from the thread that owns it.
    unsafe { *errno_ptr() }
}
