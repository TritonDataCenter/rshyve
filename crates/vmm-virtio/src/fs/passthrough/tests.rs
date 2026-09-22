// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unit tests for the passthrough export.

use std::ffi::CString;

use super::sys::validate_name;
use super::tables::ifmt;
use super::*;
use fuse::linux_oflags as lx;

/// The host flags on an open file handle's descriptor.
#[cfg(unix)]
fn handle_fl(pt: &Passthrough, fh: u64) -> libc::c_int {
    let handles = pt.handles.lock().expect("handles");
    let Some(Handle::File { fd }) = handles.by_id.get(&fh) else {
        panic!("no file handle {fh}");
    };
    // SAFETY: the handle-table guard above keeps `fd` open, and F_GETFL
    // takes no pointer.
    let fl = unsafe { libc::fcntl(*fd, libc::F_GETFL) };
    assert!(fl >= 0, "F_GETFL failed");
    fl
}

// Linux O_APPEND is 0o2000, which illumos reads as O_EXCL. Host
// constants would lose the append and refuse a name created since
// LOOKUP.
#[cfg(unix)]
#[test]
fn o_append_on_the_wire_reaches_the_host_descriptor() {
    let dir = private_dir();
    let pt = Passthrough::new(dir.path(), false).expect("open temp");
    let name = CString::new("log").expect("name");
    let flags = lx::O_WRONLY | lx::O_CREAT | lx::O_APPEND;
    let (_, fh, _) = pt
        .create(fuse::FUSE_ROOT_ID, &name, flags, 0o644, 0)
        .expect("create");
    assert_ne!(handle_fl(&pt, fh) & libc::O_APPEND, 0, "append lost");

    let flags = lx::O_WRONLY | lx::O_CREAT;
    let (_, fh, _) = pt
        .create(fuse::FUSE_ROOT_ID, &name, flags, 0o644, 0)
        .expect("reopen");
    assert_eq!(handle_fl(&pt, fh) & libc::O_APPEND, 0, "append invented");
}

/// A private directory for one test.
///
/// `tempfile::tempdir` applies the process umask, and the console tests
/// change the umask from another thread. A directory created in that
/// window loses its search bit, and every `openat` below it fails with
/// EACCES. So set the mode explicitly.
#[cfg(unix)]
fn private_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::set_permissions(
        dir.path(),
        std::fs::Permissions::from_mode(0o700),
    )
    .expect("restore tempdir mode");
    dir
}

#[test]
fn reject_empty_name() {
    assert!(validate_name(b"").is_err());
}

#[test]
fn reject_dot_names() {
    assert!(validate_name(b".").is_err());
    assert!(validate_name(b"..").is_err());
}

#[test]
fn reject_slash() {
    assert!(validate_name(b"a/b").is_err());
    assert!(validate_name(b"/").is_err());
    assert!(validate_name(b"../etc/passwd").is_err());
}

#[test]
fn reject_nul() {
    assert!(validate_name(b"a\x00b").is_err());
}

#[test]
fn accept_ordinary_names() {
    assert!(validate_name(b"foo").is_ok());
    assert!(validate_name(b"foo.txt").is_ok());
    assert!(validate_name(b"file with spaces").is_ok());
    assert!(validate_name(b".hidden").is_ok()); // dotfiles are fine
}

#[cfg(unix)]
#[test]
fn passthrough_root_opens() {
    let dir = private_dir();
    let pt = Passthrough::new(dir.path(), true).expect("open temp");
    let attr = pt.getattr(fuse::FUSE_ROOT_ID).expect("getattr root");
    assert_eq!(attr.mode & ifmt::MASK, ifmt::DIR);
}

// `clear` runs on guest reset and on VM teardown. It must close every
// guest-held fd but keep the export's own descriptor, which the
// FUSE_ROOT_ID entry holds.
#[cfg(unix)]
#[test]
fn clear_closes_handles_and_keeps_root() {
    let dir = private_dir();
    let pt = Passthrough::new(dir.path(), true).expect("open temp");

    let fh = pt.opendir(fuse::FUSE_ROOT_ID).expect("opendir root");
    assert!(pt.releasedir(fh).is_ok());

    let fh2 = pt.opendir(fuse::FUSE_ROOT_ID).expect("opendir root again");
    pt.clear();

    assert!(matches!(pt.releasedir(fh2), Err(PtError::UnknownHandle(_))));
    // Root inode survives, so the device still serves after a reset.
    let attr = pt.getattr(fuse::FUSE_ROOT_ID).expect("root still open");
    assert_eq!(attr.mode & ifmt::MASK, ifmt::DIR);
}

/// Every name a directory handle serves, in snapshot order.
#[cfg(unix)]
fn listing(pt: &Passthrough, fh: u64) -> Vec<Vec<u8>> {
    let mut names = Vec::new();
    pt.readdir_each(fh, 0, |_, name, _, _| {
        names.push(name.to_bytes().to_vec());
        true
    })
    .expect("readdir");
    names
}

// A `dup` of the cached inode fd shares its directory offset. The first
// snapshot reads the directory to EOF, and every later OPENDIR then
// sees nothing.
#[cfg(unix)]
#[test]
fn a_second_opendir_of_the_same_directory_sees_the_same_entries() {
    let dir = private_dir();
    for name in ["a", "b", "c"] {
        std::fs::write(dir.path().join(name), b"x").expect("seed");
    }
    let pt = Passthrough::new(dir.path(), true).expect("open temp");

    let first = pt.opendir(fuse::FUSE_ROOT_ID).expect("first opendir");
    let mut seen_first = listing(&pt, first);
    let second = pt.opendir(fuse::FUSE_ROOT_ID).expect("second opendir");
    let mut seen_second = listing(&pt, second);

    seen_first.sort();
    seen_second.sort();
    assert_eq!(seen_first, [b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    assert_eq!(seen_second, seen_first, "the second snapshot differs");
}

/// Make a FIFO in `dir`, or say the host would not.
#[cfg(unix)]
fn mkfifo(dir: &Path, name: &str) {
    let path = path_cstring(&dir.join(name)).expect("fifo path");
    // SAFETY: `path` owns its NUL-terminated bytes for the whole call,
    // which only reads them.
    let rc = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "mkfifo: {}", std::io::Error::last_os_error());
}

/// Run `body` on a thread and fail if it has not returned in time.
///
/// A blocking `open` would hang the test run. The deadline makes it a
/// failure.
#[cfg(unix)]
fn within<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(body());
    });
    rx.recv_timeout(std::time::Duration::from_secs(5))
        .expect("the call never returned: it is waiting on a FIFO peer")
}

// A share can hold a FIFO. Linux sends CREATE for a name it has not
// cached, and O_CREAT opens whatever is there. Without O_NONBLOCK the
// open waits for a peer and blocks the single FUSE worker.
#[cfg(unix)]
#[test]
fn a_create_over_a_fifo_is_refused_without_waiting() {
    let dir = private_dir();
    mkfifo(dir.path(), "p");
    let pt = Passthrough::new(dir.path(), false).expect("open temp");
    let refused = within(move || {
        pt.create(
            fuse::FUSE_ROOT_ID,
            &CString::new("p").expect("name"),
            lx::O_WRONLY | lx::O_CREAT,
            0o644,
            0,
        )
        .err()
        .map(|e| e.to_errno())
    });
    // ENXIO: with O_NONBLOCK, a write-only open of a FIFO with no
    // reader fails at once. ENOTSUP: the host allows the open and the
    // type check refuses it. Both answers reach the guest.
    assert!(
        matches!(refused, Some(libc::ENXIO | libc::ENOTSUP)),
        "unexpected refusal: {refused:?}"
    );
}

// The same name through LOOKUP. The type check uses the probe, so the
// guest is refused before anything is opened.
#[cfg(unix)]
#[test]
fn a_lookup_of_a_fifo_is_refused_without_waiting() {
    let dir = private_dir();
    mkfifo(dir.path(), "p");
    let pt = Passthrough::new(dir.path(), false).expect("open temp");
    let refused = within(move || {
        pt.lookup(fuse::FUSE_ROOT_ID, &CString::new("p").expect("name"))
            .err()
            .map(|e| e.to_errno())
    });
    assert_eq!(refused, Some(libc::ENOTSUP));
}

// The probe and the open resolve the name separately. A type mismatch
// on the descriptor means the name changed between them.
#[cfg(unix)]
#[test]
fn a_child_that_changed_type_between_probe_and_open_is_stale() {
    let dir = private_dir();
    mkfifo(dir.path(), "p");
    let parent = Passthrough::new(dir.path(), false).expect("open temp");
    let parent_fd = parent
        .inodes
        .lock()
        .unwrap()
        .by_id
        .get(&fuse::FUSE_ROOT_ID)
        .and_then(|ino| ino.fd)
        .expect("root fd");
    let probe = fstat(parent_fd).expect("stat the root");
    let refused = within(move || {
        open_child(
            parent_fd,
            &CString::new("p").expect("name"),
            &probe,
            FileType::Regular,
        )
        .err()
        .map(|e| e.to_errno())
    });
    assert_eq!(refused, Some(libc::ESTALE));
}

#[cfg(unix)]
fn limited(dir: &tempfile::TempDir, limits: PtLimits) -> Passthrough {
    Passthrough::with_limits(dir.path(), false, limits).expect("open temp")
}

#[cfg(unix)]
fn name(s: &str) -> CString {
    CString::new(s).expect("name")
}

// Every cached inode pins a host fd until the guest FORGETs it.
#[cfg(unix)]
#[test]
fn the_inode_table_is_capped_and_forget_makes_room() {
    let dir = private_dir();
    for n in ["a", "b"] {
        std::fs::write(dir.path().join(n), b"x").expect("seed");
    }
    // Root takes one of the two.
    let pt = limited(
        &dir,
        PtLimits {
            max_inodes: 2,
            ..PtLimits::default()
        },
    );

    let a = pt.lookup(fuse::FUSE_ROOT_ID, &name("a")).expect("a fits");
    let refused = pt.lookup(fuse::FUSE_ROOT_ID, &name("b")).unwrap_err();
    assert_eq!(refused.to_errno(), libc::ENFILE);
    // A hit on a cached inode costs no fd and is still served.
    pt.lookup(fuse::FUSE_ROOT_ID, &name("a")).expect("a again");

    pt.forget(a.nodeid, 2);
    pt.lookup(fuse::FUSE_ROOT_ID, &name("b"))
        .expect("b fits now");
}

#[cfg(unix)]
#[test]
fn create_is_held_to_the_inode_cap() {
    let dir = private_dir();
    let pt = limited(
        &dir,
        PtLimits {
            max_inodes: 1,
            ..PtLimits::default()
        },
    );
    let flags = lx::O_WRONLY | lx::O_CREAT;
    let refused = pt
        .create(fuse::FUSE_ROOT_ID, &name("n"), flags, 0o644, 0)
        .unwrap_err();
    assert_eq!(refused.to_errno(), libc::ENFILE);
    assert!(pt.handles.lock().unwrap().by_id.is_empty());
}

#[cfg(unix)]
#[test]
fn the_handle_table_is_capped_and_release_makes_room() {
    let dir = private_dir();
    let pt = limited(
        &dir,
        PtLimits {
            max_handles: 1,
            ..PtLimits::default()
        },
    );
    let fh = pt.opendir(fuse::FUSE_ROOT_ID).expect("first fits");
    let refused = pt.opendir(fuse::FUSE_ROOT_ID).unwrap_err();
    assert_eq!(refused.to_errno(), libc::EMFILE);
    pt.releasedir(fh).expect("release");
    pt.opendir(fuse::FUSE_ROOT_ID).expect("fits again");
}

// OPENDIR snapshots a whole directory, and the guest controls how many
// snapshots it holds.
#[cfg(unix)]
#[test]
fn directory_snapshots_are_held_to_a_byte_budget() {
    let dir = private_dir();
    for i in 0..8 {
        std::fs::write(dir.path().join(format!("f{i}")), b"x").expect("seed");
    }
    // Room for one snapshot of this directory, not two.
    let one = 8 * (size_of::<DirSnapshot>() + 3);
    let pt = limited(
        &dir,
        PtLimits {
            max_snapshot_bytes: one + one / 2,
            ..PtLimits::default()
        },
    );
    let fh = pt.opendir(fuse::FUSE_ROOT_ID).expect("first fits");
    let refused = pt.opendir(fuse::FUSE_ROOT_ID).unwrap_err();
    assert_eq!(refused.to_errno(), libc::ENOMEM);
    pt.releasedir(fh).expect("release");
    pt.opendir(fuse::FUSE_ROOT_ID).expect("fits again");
    pt.clear();
    assert_eq!(pt.handles.lock().unwrap().snapshot_bytes, 0);
}

#[cfg(unix)]
#[test]
fn readdir_shares_one_snapshot_across_reads() {
    let dir = private_dir();
    std::fs::write(dir.path().join("f"), b"x").expect("seed");
    let pt = Passthrough::new(dir.path(), true).expect("open temp");
    let fh = pt.opendir(fuse::FUSE_ROOT_ID).expect("opendir");
    assert_eq!(listing(&pt, fh), listing(&pt, fh));
    let handles = pt.handles.lock().unwrap();
    let Some(Handle::Dir { entries, .. }) = handles.by_id.get(&fh) else {
        panic!("no dir handle");
    };
    assert_eq!(Arc::strong_count(entries), 1, "a listing kept a copy");
}

#[cfg(unix)]
#[test]
fn the_fd_limit_is_raised_to_the_hard_limit() {
    let soft = raise_fd_limit().expect("raise");
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `lim` is a live `rlimit` this frame owns, and getrlimit
    // writes at most that one struct through the pointer.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) }, 0);
    assert_eq!(lim.rlim_cur, soft);
    #[cfg(not(target_os = "macos"))]
    assert_eq!(lim.rlim_cur, lim.rlim_max);
}

#[cfg(unix)]
#[test]
fn clear_is_idempotent() {
    let dir = private_dir();
    let pt = Passthrough::new(dir.path(), true).expect("open temp");
    pt.clear();
    pt.clear();
    assert!(pt.getattr(fuse::FUSE_ROOT_ID).is_ok());
}

#[cfg(unix)]
#[test]
fn sync_all_with_no_handles_is_ok() {
    let dir = private_dir();
    let pt = Passthrough::new(dir.path(), true).expect("open temp");
    assert!(pt.sync_all().is_ok());
}

// Covers the open-file branch of both methods, and proves `clear` drops
// the devino index with the inode table. A stale index entry would give
// a later lookup a nodeid that no longer exists.
#[cfg(unix)]
#[test]
fn clear_and_sync_all_cover_file_handles() {
    let dir = private_dir();
    let pt = Passthrough::new(dir.path(), false).expect("open temp");
    let name = CString::new("data.bin").expect("name");

    let (entry, fh, _) = pt
        .create(fuse::FUSE_ROOT_ID, &name, lx::O_RDWR, 0o644, 0)
        .expect("create");
    assert!(entry.nodeid > fuse::FUSE_ROOT_ID);
    pt.sync_all().expect("sync open handle");

    pt.clear();
    assert!(matches!(pt.release(fh), Err(PtError::UnknownHandle(_))));

    // The file is still on disk, so a new lookup must succeed with a live
    // nodeid.
    let again = pt.lookup(fuse::FUSE_ROOT_ID, &name).expect("re-lookup");
    pt.getattr(again.nodeid).expect("re-looked-up node is live");
}
