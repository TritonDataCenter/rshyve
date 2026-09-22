// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `fhrun-init`: the minimal PID 1 for a fhrun microVM.
//!
//! Responsibilities, in order:
//! 1. Mount `/proc`, `/sys`, and make `/dev` usable.
//! 2. Read `/firehyve-spec.json`.
//! 3. Bring up each NIC the spec describes, with its IPv4 address and,
//!    for the first NIC that names one, a default route.
//! 4. Fork and `execve()` the payload binary at `spec.bin` with
//!    `spec.args` and the given environment. argv[0] is the binary's
//!    basename. Init stays PID 1 and reaps the payload and every
//!    orphan it leaves.
//! 5. Write the payload's wait status to `/dev/ttyS1` (COM2) as
//!    `fhrun-init: payload exit <code>` or `fhrun-init: payload signal
//!    <n>`, then trigger ACPI poweroff. `fhrun` reads that marker from
//!    the VMM's stdout and exits with the payload's status. An init
//!    error powers off with no marker, which `fhrun` reports as its own
//!    failure.
//!
//! The payload must not be PID 1 itself: when PID 1 exits the kernel
//! panics and, under `panic=-1`, reboots, so the VMM would report a
//! reset and the status would be lost.
//!
//! Cross-compile target: `x86_64-unknown-linux-musl`. The init must be
//! statically linked because the guest rootfs has no dynamic loader.

mod net;
mod spec;

use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};

use crate::spec::GuestSpec;

const SPEC_PATH: &str = "/firehyve-spec.json";

/// Exit code the forked child uses when `execve` fails, following the
/// shell convention: 127 when the binary is missing, 126 otherwise.
const EXEC_NOT_FOUND: i32 = 127;
const EXEC_FAILED: i32 = 126;

fn main() -> ! {
    // The earliest point userspace can signal the host from, so it
    // brackets the same interval Firecracker's spec measures ("API call
    // to start of /sbin/init"). devtmpfs must be mounted first because
    // the initramfs ships an empty /dev, so /dev/ttyS1 does not exist
    // yet. The kernel populates devtmpfs from already registered
    // devices, so the mount costs microseconds. The later
    // mount_essentials() call gets EBUSY here and treats it as success.
    let _ = do_mount("devtmpfs", "/dev", "devtmpfs", 0);
    ttys1_ping("fhrun-init: init-start\n");

    match run() {
        Ok(status) => {
            let marker = status.marker();
            klog(&marker);
            ttys1_ping(&format!("{marker}\n"));
        }
        Err(e) => klog(&format!("fhrun-init: fatal: {e}")),
    }
    // Never return from PID 1: the kernel panics when init exits.
    poweroff();
}

/// How the payload ended, as `waitpid` reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PayloadStatus {
    Exited(i32),
    Signaled(i32),
}

impl PayloadStatus {
    fn from_wait_status(status: libc::c_int) -> Option<Self> {
        if libc::WIFEXITED(status) {
            Some(Self::Exited(libc::WEXITSTATUS(status)))
        } else if libc::WIFSIGNALED(status) {
            Some(Self::Signaled(libc::WTERMSIG(status)))
        } else {
            None
        }
    }

    /// The line `fhrun` parses. Keep it in step with
    /// `tools/fhrun/src/status.rs`.
    fn marker(self) -> String {
        match self {
            Self::Exited(code) => format!("fhrun-init: payload exit {code}"),
            Self::Signaled(sig) => format!("fhrun-init: payload signal {sig}"),
        }
    }
}

fn run() -> Result<PayloadStatus, String> {
    ttys1_ping("fhrun-init: stage-run\n");
    klog("fhrun-init: hello");
    ttys1_ping("fhrun-init: stage-klog\n");

    mount_essentials()?;
    ttys1_ping("fhrun-init: stage-mounts\n");
    redirect_stdio()?;
    ttys1_ping("fhrun-init: stage-stdio\n");

    let spec = load_spec(Path::new(SPEC_PATH))?;
    ttys1_ping("fhrun-init: stage-spec\n");
    klog(&format!(
        "fhrun-init: spec loaded: bin={} argc={} env={}",
        spec.bin,
        spec.args.len(),
        spec.env.len()
    ));

    if let Err(e) = net::bring_up_lo() {
        klog(&format!("fhrun-init: warn: {e}"));
    }
    // Bring each NIC up in declaration order: spec.nics[0] is eth0,
    // spec.nics[1] is eth1, and so on. Only the first NIC with a
    // `gateway` installs a default route. Linux honors one default per
    // table, so later gateways are ignored.
    let mut default_route_set = false;
    for (idx, nic) in spec.nics.iter().enumerate() {
        let name = format!("eth{idx}");
        let want_default = !default_route_set && nic.gateway.is_some();
        net::configure_iface(&name, nic, want_default)
            .map_err(|e| format!("net {name}: {e}"))?;
        if want_default {
            default_route_set = true;
        }
        klog(&format!(
            "fhrun-init: {name} up {} (role={})",
            nic.ip,
            nic.role.as_deref().unwrap_or("-")
        ));
    }

    // Consoles need no setup: devtmpfs creates the device nodes. They
    // are logged so an operator can match a guest node to the host
    // socket fhrun printed.
    for console in &spec.consoles {
        klog(&format!(
            "fhrun-init: console {} (role={})",
            console.guest_device,
            console.role.as_deref().unwrap_or("-")
        ));
    }

    ttys1_ping("fhrun-init: stage-net\n");
    chdir(&spec.workdir)?;

    // Ping ttyS1 (COM2) right before the payload starts. Init writes
    // the tty device directly, not through printk, so the marker is
    // available for host-side timing even when the cmdline has no
    // `console=`.
    signal_ready();

    run_payload(&spec)
}

/// Fork the payload, then reap children until the payload is among
/// them.
///
/// Every orphan is re-parented to PID 1, so the wait loop takes any
/// child and only stops on the payload's pid. Children still alive
/// after that die with the poweroff.
fn run_payload(spec: &GuestSpec) -> Result<PayloadStatus, String> {
    let argv = exec_argv(spec)?;
    // SAFETY: init has one thread, so no lock can be held across the
    // fork and the child may allocate before it calls `execve`.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork: {}", std::io::Error::last_os_error()));
    }
    if pid == 0 {
        exec_child(spec, &argv);
    }
    klog(&format!("fhrun-init: payload pid {pid}: {}", spec.bin));

    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: `status` is a valid out pointer for the call.
        let reaped = unsafe { libc::waitpid(-1, &mut status, 0) };
        if reaped < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(format!("waitpid: {e}"));
        }
        if reaped != pid {
            continue;
        }
        let Some(outcome) = PayloadStatus::from_wait_status(status) else {
            // Stopped or continued: the payload is still there.
            continue;
        };
        reap_orphans();
        return Ok(outcome);
    }
}

/// Collect every child that has already exited, without waiting on the
/// ones that have not.
fn reap_orphans() {
    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: `status` is a valid out pointer for the call.
        let reaped = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if reaped <= 0 {
            return;
        }
    }
}

/// Best-effort readiness ping on /dev/ttyS1 (COM2). Boot never fails on
/// it: a missing device just skips the marker.
fn signal_ready() {
    ttys1_ping("fhrun-init: ready\n");
}

/// Raw, best-effort write of `msg` to /dev/ttyS1 (COM2), for host-side
/// timing markers.
fn ttys1_ping(msg: &str) {
    // The port is opened once and the fd is reused for every marker. A
    // second open() of the still-open port does not deliver, and the fd
    // can never be closed anyway: the 8250 release path drains the TX
    // FIFO, and the virtual UART never signals transmission-complete,
    // so the last close() would hang forever. Init holds it until
    // poweroff. O_CLOEXEC keeps the marker channel out of the payload.
    // That close is not the last one, so it does not reach the release
    // path.
    static FD: AtomicI32 = AtomicI32::new(-2);

    let mut fd = FD.load(Ordering::Relaxed);
    if fd == -2 {
        let path = CString::new("/dev/ttyS1").unwrap();
        // O_NONBLOCK matters here: a blocking open() on a serial device
        // waits for DCD before it returns, and the virtual UART never
        // asserts it. Without this flag fhrun-init hangs forever right
        // here, before it reaches the payload.
        fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_WRONLY
                    | libc::O_NOCTTY
                    | libc::O_NONBLOCK
                    | libc::O_CLOEXEC,
            )
        };
        FD.store(fd, Ordering::Relaxed);
    }
    if fd < 0 {
        return;
    }
    unsafe {
        libc::write(fd, msg.as_ptr() as *const libc::c_void, msg.len());
    }
}

/// Wire up stdin/stdout/stderr to /dev/console before the payload is
/// forked, so its prints land on the serial port the host mirrors.
/// Without this the kernel opens stdio for PID 1 only, and the child
/// inherits raw fds that need not be a tty.
fn redirect_stdio() -> Result<(), String> {
    let path = CString::new("/dev/console").unwrap();
    let fd =
        unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if fd < 0 {
        let e = std::io::Error::last_os_error();
        klog(&format!(
            "fhrun-init: open /dev/console: {e} (skipping redirect)"
        ));
        return Ok(());
    }
    unsafe {
        libc::dup2(fd, 0);
        libc::dup2(fd, 1);
        libc::dup2(fd, 2);
        if fd > 2 {
            libc::close(fd);
        }
    }
    Ok(())
}

fn mount_essentials() -> Result<(), String> {
    // /proc and /sys are required for sysrq and interface enumeration.
    // /dev needs devtmpfs so /dev/console and /dev/tty* exist. Init
    // mounts it rather than relying on CONFIG_DEVTMPFS_MOUNT.
    do_mount("proc", "/proc", "proc", 0)?;
    do_mount("sysfs", "/sys", "sysfs", 0)?;
    do_mount("devtmpfs", "/dev", "devtmpfs", 0)?;
    Ok(())
}

fn do_mount(
    src: &str,
    target: &str,
    fstype: &str,
    flags: u64,
) -> Result<(), String> {
    let src_c = CString::new(src).unwrap();
    let target_c = CString::new(target).unwrap();
    let fstype_c = CString::new(fstype).unwrap();
    let rc = unsafe {
        libc::mount(
            src_c.as_ptr(),
            target_c.as_ptr(),
            fstype_c.as_ptr(),
            flags,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        // EBUSY means the target is already mounted, by the kernel or by
        // the early devtmpfs mount in `main`. Treat it as success.
        if e.raw_os_error() == Some(libc::EBUSY) {
            return Ok(());
        }
        return Err(format!("mount {fstype} on {target}: {e}"));
    }
    Ok(())
}

fn load_spec(path: &Path) -> Result<GuestSpec, String> {
    let raw = std::fs::read(path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let s: GuestSpec = serde_json::from_slice(&raw)
        .map_err(|e| format!("parse {}: {e}", path.display()))?;
    Ok(s)
}

fn chdir(dir: &str) -> Result<(), String> {
    let c = CString::new(dir).unwrap();
    let rc = unsafe { libc::chdir(c.as_ptr()) };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        klog(&format!(
            "fhrun-init: chdir({dir}) failed: {e}; falling back to /"
        ));
        let root = CString::new("/").unwrap();
        unsafe { libc::chdir(root.as_ptr()) };
    }
    Ok(())
}

/// The C strings `execve` takes, built before the fork so the child
/// allocates nothing.
struct ExecArgv {
    bin: CString,
    argv: Vec<CString>,
    envp: Vec<CString>,
}

fn exec_argv(spec: &GuestSpec) -> Result<ExecArgv, String> {
    let bin = CString::new(spec.bin.as_str())
        .map_err(|_| "bin path has NUL".to_string())?;
    let basename = Path::new(&spec.bin)
        .file_name()
        .map(|s| s.as_bytes().to_vec())
        .unwrap_or_else(|| spec.bin.as_bytes().to_vec());
    let argv0 =
        CString::new(basename).map_err(|_| "bin path has NUL".to_string())?;

    let mut argv: Vec<CString> = Vec::with_capacity(spec.args.len() + 1);
    argv.push(argv0);
    for a in &spec.args {
        argv.push(
            CString::new(a.as_str()).map_err(|_| "arg has NUL".to_string())?,
        );
    }
    let envp = build_env(&spec.env)?;
    Ok(ExecArgv { bin, argv, envp })
}

/// The forked child: replace this process with the payload, or exit
/// with the shell's code for a failed exec.
fn exec_child(spec: &GuestSpec, argv: &ExecArgv) -> ! {
    let mut argv_p: Vec<*const libc::c_char> =
        argv.argv.iter().map(|c| c.as_ptr()).collect();
    argv_p.push(std::ptr::null());
    let mut envp_p: Vec<*const libc::c_char> =
        argv.envp.iter().map(|c| c.as_ptr()).collect();
    envp_p.push(std::ptr::null());

    // SAFETY: every pointer comes from a live `CString` in `argv`, and
    // both arrays end in NULL.
    unsafe {
        libc::execve(argv.bin.as_ptr(), argv_p.as_ptr(), envp_p.as_ptr());
    }
    let e = std::io::Error::last_os_error();
    klog(&format!("fhrun-init: execve {}: {e}", spec.bin));
    let code = if e.raw_os_error() == Some(libc::ENOENT) {
        EXEC_NOT_FOUND
    } else {
        EXEC_FAILED
    };
    // SAFETY: `_exit` takes no resources and never returns.
    unsafe { libc::_exit(code) }
}

fn build_env(env: &BTreeMap<String, String>) -> Result<Vec<CString>, String> {
    let mut out = Vec::with_capacity(env.len() + 4);
    for (k, v) in env {
        if k.contains('=') {
            return Err(format!("env key {k} contains '='"));
        }
        let entry = format!("{k}={v}");
        out.push(
            CString::new(entry).map_err(|_| "env contains NUL".to_string())?,
        );
    }
    // Default PATH when the manifest gives none.
    if !env.contains_key("PATH") {
        out.push(CString::new("PATH=/usr/local/bin:/usr/bin:/bin").unwrap());
    }
    Ok(out)
}

fn klog(msg: &str) {
    // Best-effort write to /dev/kmsg so the message appears on the
    // serial console, which the VMM mirrors to its stdout.
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg")
    {
        use std::io::Write;
        let _ = writeln!(f, "{msg}");
    } else {
        // Fall back to stderr so the message is still visible during
        // early boot, before /dev is wired up. `eprintln!` panics on
        // a failed write, and a panic in PID 1 is a kernel panic.
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), "{msg}");
    }
}

fn poweroff() -> ! {
    klog("fhrun-init: poweroff");
    // sysrq 'o' is poweroff. It needs CONFIG_MAGIC_SYSRQ and sysrq on.
    let _ = std::fs::write("/proc/sys/kernel/sysrq", "1");
    let _ = std::fs::write("/proc/sysrq-trigger", "o");
    // Fallback: the reboot syscall with LINUX_REBOOT_CMD_POWER_OFF.
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_POWER_OFF);
    }
    // If control still reaches here, spin: the kernel panics when init
    // exits.
    loop {
        unsafe { libc::pause() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_status(code: i32, sig: i32) -> libc::c_int {
        (code << 8) | sig
    }

    #[test]
    fn an_exit_is_reported_as_its_code() {
        let status = PayloadStatus::from_wait_status(wait_status(3, 0));
        assert_eq!(status, Some(PayloadStatus::Exited(3)));
        assert_eq!(status.unwrap().marker(), "fhrun-init: payload exit 3");
    }

    #[test]
    fn a_signal_death_is_reported_as_its_number() {
        let status = PayloadStatus::from_wait_status(wait_status(0, 9));
        assert_eq!(status, Some(PayloadStatus::Signaled(9)));
        assert_eq!(status.unwrap().marker(), "fhrun-init: payload signal 9");
    }

    #[test]
    fn a_stop_is_not_an_end() {
        // WIFSTOPPED: low byte 0x7f, signal in the next byte.
        let stopped = 0x7f | (libc::SIGSTOP << 8);
        assert_eq!(PayloadStatus::from_wait_status(stopped), None);
    }
}
