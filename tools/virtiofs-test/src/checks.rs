// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The checks run against a mounted virtio-fs share.
//!
//! `tools/virtiofs-stage.sh` stages the fixture on the host. The two
//! files must agree on names, sizes and content.

use std::ffi::CString;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use crate::report::{Check, Outcome, Report};

/// One mutating check, so the read-only table has a nameable type.
type CheckFn = fn(&Path) -> Check;

/// Size of `big.bin` in the fixture. Larger than the device's 1 MiB
/// per-request body cap, so a full read must span several FUSE_READs.
pub const BIG_LEN: u64 = 4 * 1024 * 1024;
/// Bytes the guest writes in the read-write path. Larger than the 512
/// KiB max_write the device advertises, so a write is split too.
const WRITE_LEN: u64 = 1024 * 1024;
const HELLO: &[u8] = b"hello from the host\n";
const DEEP: &[u8] = b"deep\n";
/// Exit status the exec probe returns. Any other status means the
/// binary on the share did not run to completion.
pub const PROBE_STATUS: i32 = 42;
const FUSE_SUPER_MAGIC: u64 = 0x6573_5546;

/// Deterministic fill byte for offset `i`, shared with the staging
/// script. 251 is prime, so the pattern does not align with any power
/// of two block size and a misplaced chunk cannot go unnoticed.
pub fn pat(i: u64) -> u8 {
    (i % 251) as u8
}

/// Entropy source the virtio-rnd driver registers.
const HWRNG: &str = "/dev/hwrng";
const HWRNG_NAME: &str = "/sys/class/misc/hw_random/rng_current";

/// Checks for the virtio-rnd device, attached next to the share. The
/// device is independent of the filesystem. The checks share a VM only
/// to save a boot.
pub fn run_rng_checks(rep: &mut Report) {
    rep.run("rng_driver_bound", || {
        let current = std::fs::read_to_string(HWRNG_NAME)
            .map_err(|e| format!("read {HWRNG_NAME}: {e}"))?;
        let current = current.trim();
        if current != "virtio_rng.0" && !current.starts_with("virtio") {
            return Err(format!("current rng is '{current}', not virtio"));
        }
        Ok(format!("current rng is {current}"))
    });

    rep.run("rng_produces_entropy", || {
        let mut f = std::fs::File::open(HWRNG)
            .map_err(|e| format!("open {HWRNG}: {e}"))?;
        let mut buf = [0u8; 256];
        f.read_exact(&mut buf).map_err(errstr)?;

        // Not a statistical test. It catches a device that returns a
        // constant, or a zero-filled buffer that nothing wrote.
        let distinct = {
            let mut seen = [false; 256];
            for b in buf {
                seen[b as usize] = true;
            }
            seen.iter().filter(|s| **s).count()
        };
        if distinct < 32 {
            return Err(format!("only {distinct} distinct byte values in 256"));
        }

        // Two reads must differ, or the device is replaying one buffer.
        let mut second = [0u8; 256];
        f.read_exact(&mut second).map_err(errstr)?;
        if second == buf {
            return Err("two reads returned identical bytes".to_string());
        }
        Ok(format!(
            "256 bytes, {distinct} distinct values, reads differ"
        ))
    });
}

/// Host CID, per the vsock addressing rules.
const VSOCK_CID_HOST: u32 = 2;
/// Port the harness expects an echo server on, host side.
pub const VSOCK_ECHO_PORT: u32 = 5555;
/// Port the guest listens on for the host to dial. This is the
/// direction a CRI shim uses: the host opens a session into the guest.
pub const VSOCK_LISTEN_PORT: u32 = 1234;

/// Start listening on `port` and echo the first connection.
///
/// Returns a handle with the result. Started before the other checks so
/// the host has the whole run to connect, and joined at the end.
pub fn spawn_vsock_listener(port: u32) -> std::thread::JoinHandle<Check> {
    std::thread::spawn(move || vsock_listen_and_echo(port))
}

fn vsock_listen_and_echo(port: u32) -> Check {
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(format!("socket(AF_VSOCK): {}", last_err()));
    }
    let listener = OwnedFd(fd);

    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    addr.svm_port = port;
    // Any CID: the guest does not know its own until the device says so.
    addr.svm_cid = libc::VMADDR_CID_ANY;
    let len = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
    if unsafe {
        libc::bind(
            listener.0,
            &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
            len,
        )
    } != 0
    {
        return Err(format!("bind vsock:{port}: {}", last_err()));
    }
    if unsafe { libc::listen(listener.0, 4) } != 0 {
        return Err(format!("listen: {}", last_err()));
    }

    // Wait for the host with a limit: a run where the host never
    // connects must report that, not hang the VM until the harness
    // times out.
    let mut pfd = libc::pollfd {
        fd: listener.0,
        events: libc::POLLIN,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut pfd, 1, VSOCK_ACCEPT_TIMEOUT_MS) };
    if rc == 0 {
        return Err("no host connection within the timeout".to_string());
    }
    if rc < 0 {
        return Err(format!("poll: {}", last_err()));
    }

    let conn = unsafe {
        libc::accept(listener.0, std::ptr::null_mut(), std::ptr::null_mut())
    };
    if conn < 0 {
        return Err(format!("accept: {}", last_err()));
    }
    let conn = OwnedFd(conn);

    let mut buf = [0u8; 256];
    let n = unsafe {
        libc::read(conn.0, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
    };
    if n <= 0 {
        return Err(format!("read from host: {}", last_err()));
    }
    let n = n as usize;
    let sent =
        unsafe { libc::write(conn.0, buf.as_ptr() as *const libc::c_void, n) };
    if sent != n as isize {
        return Err(format!("echo write: {}", last_err()));
    }
    Ok(format!("host dialed in, {n} bytes echoed back"))
}

/// How long the guest waits for the host to dial in.
const VSOCK_ACCEPT_TIMEOUT_MS: libc::c_int = 20_000;

/// Checks for virtio-vsock: the guest dials out to the host and expects
/// its bytes back, which exercises both directions of the device and
/// the mux's guest-initiated connect path.
pub fn run_vsock_checks(rep: &mut Report, port: u32) {
    rep.run("vsock_device_present", || {
        // The vsock core registers this misc device when a transport
        // binds. If it is absent, the driver never attached.
        if !Path::new("/dev/vsock").exists() {
            return Err("/dev/vsock is missing".to_string());
        }
        Ok("/dev/vsock present".to_string())
    });

    rep.run("vsock_round_trip", || vsock_round_trip(port));
    rep.run("vsock_bulk_transfer", || vsock_bulk(port));
}

/// Bytes pushed through the echo server in the bulk check. Larger than
/// one RX chain and larger than a connection's credit window, so the
/// transfer only completes if splitting, credit and the backpressure
/// path all work.
const VSOCK_BULK_LEN: usize = 512 * 1024;

/// Echo a large buffer, reading and writing at once.
///
/// The halves run concurrently: writing everything first would fill the
/// socket buffers in both directions and deadlock with the echo server.
fn vsock_bulk(port: u32) -> Check {
    let sock = vsock_connect(port)?;
    let read_fd = sock.0;

    let reader = std::thread::spawn(move || {
        let mut got = vec![0u8; VSOCK_BULK_LEN];
        let mut n = 0usize;
        while n < VSOCK_BULK_LEN {
            let r = unsafe {
                libc::read(
                    read_fd,
                    got[n..].as_mut_ptr() as *mut libc::c_void,
                    VSOCK_BULK_LEN - n,
                )
            };
            if r <= 0 {
                break;
            }
            n += r as usize;
        }
        (got, n)
    });

    // pat() is the filesystem check generator, so a misplaced chunk
    // shows as a byte offset, not only "differs".
    let out: Vec<u8> = (0..VSOCK_BULK_LEN).map(|i| pat(i as u64)).collect();
    let mut sent = 0usize;
    while sent < out.len() {
        let n = unsafe {
            libc::write(
                sock.0,
                out[sent..].as_ptr() as *const libc::c_void,
                out.len() - sent,
            )
        };
        if n <= 0 {
            return Err(format!("write at {sent}: {}", last_err()));
        }
        sent += n as usize;
    }

    let (got, n) = reader.join().map_err(|_| "reader panicked".to_string())?;
    if n != VSOCK_BULK_LEN {
        return Err(format!("echoed {n} of {VSOCK_BULK_LEN} bytes"));
    }
    if let Some(bad) = mismatch(&got, 0) {
        return Err(format!("byte {bad} differs after echo"));
    }
    Ok(format!("{VSOCK_BULK_LEN} bytes echoed intact"))
}

/// Open a stream to the host on `port`.
fn vsock_connect(port: u32) -> Result<OwnedFd, String> {
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(format!("socket(AF_VSOCK): {}", last_err()));
    }
    let sock = OwnedFd(fd);
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    addr.svm_port = port;
    addr.svm_cid = VSOCK_CID_HOST;
    let rc = unsafe {
        libc::connect(
            sock.0,
            &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(format!("connect to host:{port}: {}", last_err()));
    }
    Ok(sock)
}

/// Connect to the host, send a probe, and require it back verbatim.
fn vsock_round_trip(port: u32) -> Check {
    let sock = vsock_connect(port)?;

    const PROBE: &[u8] = b"vsock-probe-0123456789
";
    let sent = unsafe {
        libc::write(sock.0, PROBE.as_ptr() as *const libc::c_void, PROBE.len())
    };
    if sent != PROBE.len() as isize {
        return Err(format!("short write: {sent} of {}", PROBE.len()));
    }

    let mut buf = [0u8; 64];
    let mut got = 0usize;
    while got < PROBE.len() {
        let n = unsafe {
            libc::read(
                sock.0,
                buf[got..].as_mut_ptr() as *mut libc::c_void,
                buf.len() - got,
            )
        };
        if n < 0 {
            return Err(format!("read: {}", last_err()));
        }
        if n == 0 {
            return Err(format!(
                "host closed after {got} of {} bytes",
                PROBE.len()
            ));
        }
        got += n as usize;
    }
    if &buf[..got] != PROBE {
        return Err(format!("echo mismatch: {:?}", &buf[..got]));
    }
    Ok(format!("{got} bytes echoed by the host"))
}

/// Closes its descriptor on drop, so an early return cannot leak one.
struct OwnedFd(i32);

impl Drop for OwnedFd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// Checks that only read. These run against a read-only share too.
pub fn run_read_checks(rep: &mut Report, mnt: &Path) {
    rep.run("statfs", || statfs_is_fuse(mnt));
    rep.run("read_small", || read_small(mnt));
    rep.run("stat_metadata", || stat_metadata(mnt));
    rep.run("readdir_root", || readdir_root(mnt));
    rep.run("readlink", || readlink(mnt));
    rep.run("nested_lookup", || nested_lookup(mnt));
    rep.run("read_large", || read_large(mnt));
    rep.run("pread_offset", || pread_offset(mnt));
    rep.run("mmap_read", || mmap_read(mnt));
    rep.run("exec_from_share", || exec_from_share(mnt));
}

/// Checks that mutate. On a read-write share each must succeed; on a
/// read-only share each must be refused with EROFS.
pub fn run_write_checks(rep: &mut Report, mnt: &Path, read_only: bool) {
    let cases: Vec<(&str, CheckFn)> = vec![
        ("create_write_read", create_write_read),
        ("mkdir_rmdir", mkdir_rmdir),
        ("rename", rename),
        ("unlink", unlink),
        ("symlink_create", symlink_create),
        ("chmod", chmod),
        ("open_o_trunc", open_o_trunc),
    ];
    for (name, f) in cases {
        if read_only {
            rep.record(name, expect_erofs(name, f(mnt)));
        } else {
            rep.run(name, || f(mnt));
        }
    }
    // Meaningful only with a mutation, so a read-only share skips it
    // instead of expecting EROFS from the first listing.
    if !read_only {
        rep.run("readdir_after_change", || readdir_after_change(mnt));
    }
}

/// On a read-only share a mutation must fail with EROFS. A different
/// errno means the guard fired for the wrong reason. Success means it
/// did not fire.
fn expect_erofs(name: &str, got: Check) -> Outcome {
    match got {
        Ok(d) => Outcome::Fail(format!("ro share allowed the mutation: {d}")),
        Err(e) if e.contains("EROFS") || e.contains("Read-only") => {
            Outcome::Pass(format!("{name} refused with EROFS"))
        }
        Err(e) => Outcome::Fail(format!("expected EROFS, got: {e}")),
    }
}

fn statfs_is_fuse(mnt: &Path) -> Check {
    let path = CString::new(mnt.as_os_str().as_encoded_bytes())
        .map_err(|_| "mount path has NUL".to_string())?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(path.as_ptr(), &mut st) };
    if rc != 0 {
        return Err(format!("statfs: {}", last_err()));
    }
    let magic = st.f_type as u64;
    if magic != FUSE_SUPER_MAGIC {
        return Err(format!("f_type=0x{magic:x}, want 0x{FUSE_SUPER_MAGIC:x}"));
    }
    Ok(format!("f_type=0x{magic:x} bsize={}", st.f_bsize))
}

fn read_small(mnt: &Path) -> Check {
    let got = std::fs::read(mnt.join("hello.txt")).map_err(errstr)?;
    if got != HELLO {
        return Err(format!(
            "content mismatch: {:?}",
            String::from_utf8_lossy(&got)
        ));
    }
    Ok(format!("{} bytes exact", got.len()))
}

fn stat_metadata(mnt: &Path) -> Check {
    let md = std::fs::metadata(mnt.join("hello.txt")).map_err(errstr)?;
    if md.len() != HELLO.len() as u64 {
        return Err(format!("size {}, want {}", md.len(), HELLO.len()));
    }
    if !md.is_file() {
        return Err("not a regular file".to_string());
    }
    Ok(format!(
        "size={} mode={:o}",
        md.len(),
        md.permissions().mode() & 0o7777
    ))
}

fn readdir_root(mnt: &Path) -> Check {
    let mut names: Vec<String> = Vec::new();
    for ent in std::fs::read_dir(mnt).map_err(errstr)? {
        names.push(
            ent.map_err(errstr)?
                .file_name()
                .to_string_lossy()
                .into_owned(),
        );
    }
    names.sort();
    for want in [
        "big.bin",
        "empty",
        "exec-probe",
        "hello.txt",
        "link-to-hello",
        "sub",
    ] {
        if !names.iter().any(|n| n == want) {
            return Err(format!("missing {want} in {names:?}"));
        }
    }
    Ok(format!("{} entries", names.len()))
}

/// List a directory, change it, and list it again.
///
/// The second listing catches a server whose OPENDIR does not start at
/// the beginning of the directory. Linux serves a listing from its page
/// cache while the directory mtime is unchanged, so the mutation forces
/// the second OPENDIR and READDIR to the server. A server that returns
/// nothing makes the directory look empty for the life of the inode.
fn readdir_after_change(mnt: &Path) -> Check {
    let dir = mnt.join("readdir-twice");
    std::fs::create_dir(&dir).map_err(errstr)?;
    for name in ["one", "two"] {
        std::fs::write(dir.join(name), b"x").map_err(errstr)?;
    }
    let first = listing(&dir)?;
    if first.len() != 2 {
        return Err(format!("first listing is {first:?}"));
    }

    std::fs::write(dir.join("three"), b"x").map_err(errstr)?;
    let second = listing(&dir)?;
    if second.len() != 3 {
        return Err(format!("second listing is {second:?}, want 3 entries"));
    }
    for want in &first {
        if !second.contains(want) {
            return Err(format!("{want} vanished from {second:?}"));
        }
    }

    std::fs::remove_dir_all(&dir).map_err(errstr)?;
    Ok("3 entries after the change".to_string())
}

fn listing(dir: &Path) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    for ent in std::fs::read_dir(dir).map_err(errstr)? {
        names.push(
            ent.map_err(errstr)?
                .file_name()
                .to_string_lossy()
                .into_owned(),
        );
    }
    names.sort();
    Ok(names)
}

fn readlink(mnt: &Path) -> Check {
    let target =
        std::fs::read_link(mnt.join("link-to-hello")).map_err(errstr)?;
    if target != Path::new("hello.txt") {
        return Err(format!("target {target:?}, want hello.txt"));
    }
    // Following the link exercises LOOKUP through the resolved name.
    let got = std::fs::read(mnt.join("link-to-hello")).map_err(errstr)?;
    if got != HELLO {
        return Err("content through the symlink differs".to_string());
    }
    Ok("target and content match".to_string())
}

fn nested_lookup(mnt: &Path) -> Check {
    let got = std::fs::read(mnt.join("sub/nested/deep.txt")).map_err(errstr)?;
    if got != DEEP {
        return Err(format!("content {:?}", String::from_utf8_lossy(&got)));
    }
    Ok("sub/nested/deep.txt matches".to_string())
}

fn read_large(mnt: &Path) -> Check {
    let mut f = std::fs::File::open(mnt.join("big.bin")).map_err(errstr)?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut off: u64 = 0;
    loop {
        let n = f.read(&mut buf).map_err(errstr)?;
        if n == 0 {
            break;
        }
        if let Some(bad) = mismatch(&buf[..n], off) {
            return Err(format!("byte {} wrong", off + bad));
        }
        off += n as u64;
    }
    if off != BIG_LEN {
        return Err(format!("read {off} bytes, want {BIG_LEN}"));
    }
    Ok(format!("{off} bytes verified"))
}

fn pread_offset(mnt: &Path) -> Check {
    // Three quarters in, so the read starts in a later chunk than a
    // sequential reader faults in.
    let off = BIG_LEN / 4 * 3 + 12345;
    let mut f = std::fs::File::open(mnt.join("big.bin")).map_err(errstr)?;
    f.seek(SeekFrom::Start(off)).map_err(errstr)?;
    let mut buf = [0u8; 4096];
    f.read_exact(&mut buf).map_err(errstr)?;
    if let Some(bad) = mismatch(&buf, off) {
        return Err(format!("byte {} wrong", off + bad));
    }
    Ok(format!("4096 bytes at offset {off}"))
}

/// This virtio-fs has no DAX window, so the guest page cache, filled by
/// FUSE_READ, serves an mmap. An `execve` from the share uses the same
/// path, so this separate check isolates a mapping fault from an exec
/// failure.
fn mmap_read(mnt: &Path) -> Check {
    let f = std::fs::File::open(mnt.join("big.bin")).map_err(errstr)?;
    let len = BIG_LEN as usize;
    let addr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            f.as_raw_fd(),
            0,
        )
    };
    if addr == libc::MAP_FAILED {
        return Err(format!("mmap: {}", last_err()));
    }
    let map = unsafe { std::slice::from_raw_parts(addr as *const u8, len) };
    // One byte per page, plus the last byte, proves the mapping is
    // backed correctly without a second pass over 4 MiB.
    let mut bad: Option<u64> = None;
    for off in (0..BIG_LEN).step_by(4096) {
        if map[off as usize] != pat(off) {
            bad = Some(off);
            break;
        }
    }
    if bad.is_none() && map[len - 1] != pat(BIG_LEN - 1) {
        bad = Some(BIG_LEN - 1);
    }
    unsafe { libc::munmap(addr, len) };
    match bad {
        Some(off) => Err(format!("mapped byte {off} wrong")),
        None => Ok(format!("{} pages sampled", len / 4096)),
    }
}

/// Run a binary from the share. This check decides whether virtio-fs
/// can serve a container image at all.
fn exec_from_share(mnt: &Path) -> Check {
    let probe = mnt.join("exec-probe");
    let md = std::fs::metadata(&probe).map_err(errstr)?;
    if md.permissions().mode() & 0o111 == 0 {
        return Err("exec-probe is not executable".to_string());
    }
    let path = CString::new(probe.as_os_str().as_encoded_bytes())
        .map_err(|_| "probe path has NUL".to_string())?;

    // A failed execve reports through a pipe, not the exit status: an
    // errno and an exit status share the 0..255 range.
    let mut pipefd = [0i32; 2];
    if unsafe { libc::pipe(pipefd.as_mut_ptr()) } != 0 {
        return Err(format!("pipe: {}", last_err()));
    }
    let (rd, wr) = (pipefd[0], pipefd[1]);

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe {
            libc::close(rd);
            libc::close(wr);
        }
        return Err(format!("fork: {}", last_err()));
    }
    if pid == 0 {
        unsafe { libc::close(rd) };
        let argv = [path.as_ptr(), std::ptr::null()];
        let envp = [std::ptr::null()];
        unsafe { libc::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
        // execve returns only on failure. This is the forked child: it
        // must not return to the parent control flow, and it must not
        // allocate.
        let errno =
            std::io::Error::last_os_error().raw_os_error().unwrap_or(-1);
        let bytes = errno.to_ne_bytes();
        unsafe {
            libc::write(wr, bytes.as_ptr() as *const libc::c_void, bytes.len());
            libc::_exit(127)
        };
    }
    unsafe { libc::close(wr) };

    // Both processes close the write end once the child execs or exits,
    // so this read cannot block forever.
    let mut errbuf = [0u8; 4];
    let got = unsafe {
        libc::read(rd, errbuf.as_mut_ptr() as *mut libc::c_void, errbuf.len())
    };
    unsafe { libc::close(rd) };

    let mut status: libc::c_int = 0;
    if unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
        return Err(format!("waitpid: {}", last_err()));
    }
    if got == 4 {
        let errno = i32::from_ne_bytes(errbuf);
        let e = std::io::Error::from_raw_os_error(errno);
        return Err(format!("execve off the share: {e} (errno {errno})"));
    }
    if libc::WIFSIGNALED(status) {
        return Err(format!(
            "probe killed by signal {}",
            libc::WTERMSIG(status)
        ));
    }
    if !libc::WIFEXITED(status) {
        return Err(format!("probe did not exit: status {status}"));
    }
    let code = libc::WEXITSTATUS(status);
    if code != PROBE_STATUS {
        return Err(format!("probe exit {code}, want {PROBE_STATUS}"));
    }
    Ok(format!("probe ran off the share, exit {code}"))
}

fn create_write_read(mnt: &Path) -> Check {
    let path = mnt.join("guest-write.bin");
    let mut f = std::fs::File::create(&path).map_err(errstr)?;
    let mut buf = vec![0u8; 128 * 1024];
    let mut off: u64 = 0;
    while off < WRITE_LEN {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = pat(off + i as u64);
        }
        f.write_all(&buf).map_err(errstr)?;
        off += buf.len() as u64;
    }
    f.sync_all().map_err(errstr)?;
    drop(f);

    let got = std::fs::read(&path).map_err(errstr)?;
    if got.len() as u64 != WRITE_LEN {
        return Err(format!("read back {} bytes, want {WRITE_LEN}", got.len()));
    }
    if let Some(bad) = mismatch(&got, 0) {
        return Err(format!("byte {bad} wrong on read back"));
    }
    std::fs::remove_file(&path).map_err(errstr)?;
    Ok(format!("{WRITE_LEN} bytes round-tripped"))
}

fn mkdir_rmdir(mnt: &Path) -> Check {
    let dir = mnt.join("guest-dir");
    std::fs::create_dir(&dir).map_err(errstr)?;
    if !std::fs::metadata(&dir).map_err(errstr)?.is_dir() {
        return Err("created path is not a directory".to_string());
    }
    std::fs::remove_dir(&dir).map_err(errstr)?;
    if std::fs::metadata(&dir).is_ok() {
        return Err("directory survived rmdir".to_string());
    }
    Ok("created and removed".to_string())
}

fn rename(mnt: &Path) -> Check {
    let from = mnt.join("guest-rename-a");
    let to = mnt.join("guest-rename-b");
    std::fs::write(&from, b"rename me\n").map_err(errstr)?;
    std::fs::rename(&from, &to).map_err(errstr)?;
    if std::fs::metadata(&from).is_ok() {
        return Err("source survived the rename".to_string());
    }
    let got = std::fs::read(&to).map_err(errstr)?;
    std::fs::remove_file(&to).map_err(errstr)?;
    if got != b"rename me\n" {
        return Err("content changed across the rename".to_string());
    }
    Ok("renamed with content intact".to_string())
}

fn unlink(mnt: &Path) -> Check {
    let path = mnt.join("guest-unlink");
    std::fs::write(&path, b"x").map_err(errstr)?;
    std::fs::remove_file(&path).map_err(errstr)?;
    if std::fs::metadata(&path).is_ok() {
        return Err("file survived unlink".to_string());
    }
    Ok("removed".to_string())
}

fn symlink_create(mnt: &Path) -> Check {
    let link = mnt.join("guest-link");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink("hello.txt", &link).map_err(errstr)?;
    let got = std::fs::read(&link).map_err(errstr)?;
    std::fs::remove_file(&link).map_err(errstr)?;
    if got != HELLO {
        return Err("content through the new symlink differs".to_string());
    }
    Ok("symlink created and followed".to_string())
}

fn chmod(mnt: &Path) -> Check {
    let path = mnt.join("guest-chmod");
    std::fs::write(&path, b"m").map_err(errstr)?;
    let mut perm = std::fs::metadata(&path).map_err(errstr)?.permissions();
    perm.set_mode(0o640);
    std::fs::set_permissions(&path, perm).map_err(errstr)?;
    let got = std::fs::metadata(&path)
        .map_err(errstr)?
        .permissions()
        .mode()
        & 0o7777;
    std::fs::remove_file(&path).map_err(errstr)?;
    if got != 0o640 {
        return Err(format!("mode {got:o}, want 640"));
    }
    Ok("mode 640 applied".to_string())
}

/// O_TRUNC mutates even when the access mode is O_RDONLY, so a
/// read-only share must refuse it.
fn open_o_trunc(mnt: &Path) -> Check {
    let path = mnt.join("hello.txt");
    let c = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| "path has NUL".to_string())?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_TRUNC) };
    if fd < 0 {
        return Err(format!("open O_RDONLY|O_TRUNC: {}", last_err()));
    }
    unsafe { libc::close(fd) };
    // On a read-write share the truncation is correct, so restore the
    // fixture for a later run against the same export.
    std::fs::write(&path, HELLO).map_err(errstr)?;
    Ok("O_TRUNC accepted on a rw share".to_string())
}

/// Index of the first byte in `buf` that does not match the pattern for
/// its absolute offset, relative to the start of `buf`.
fn mismatch(buf: &[u8], base: u64) -> Option<u64> {
    buf.iter()
        .enumerate()
        .find(|(i, b)| **b != pat(base + *i as u64))
        .map(|(i, _)| i as u64)
}

fn errstr(e: std::io::Error) -> String {
    match e.raw_os_error() {
        Some(libc::EROFS) => format!("EROFS ({e})"),
        _ => e.to_string(),
    }
}

pub fn last_err() -> String {
    errstr(std::io::Error::last_os_error())
}
