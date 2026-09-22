// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The PID 1 path: mounts a virtio-fs share and reports on it, boots a
//! container image from it, or checks what the host hot-added to the
//! running VM, then powers the VM off.
//!
//! The same binary is also staged inside the share as `exec-probe`. The
//! PID selects the role: only the kernel starts init as PID 1, so any
//! other PID means the `exec_from_share` check ran this copy from the
//! share, and it exits with [`PROBE_STATUS`].
//!
//! Settings come from the kernel command line:
//!
//! | parameter | default | meaning |
//! |---|---|---|
//! | `virtiofs.tag=` | `testfs` | mount tag the device advertises |
//! | `virtiofs.mode=` | `rw` | `ro` asserts every mutation gets EROFS |
//! | `virtiofs.role=` | `checks` | `container` boots the share as a rootfs, `hotplug` checks hot-added resources |
//! | `virtiofs.entry=` | none | run this script instead of the image's entrypoint |

use std::ffi::CString;
use std::path::Path;

use crate::checks::PROBE_STATUS;
use crate::cmdline::param;
use crate::hotplug::HotplugSpec;
use crate::report::Report;
use crate::{checks, container, hotplug_checks, mount, report};

const MOUNT_POINT: &str = "/mnt";
const DEFAULT_TAG: &str = "testfs";

pub fn main() -> ! {
    // Started from the share by exec_from_share, not by the kernel.
    if unsafe { libc::getpid() } != 1 {
        println!("{} PROBE running from the share", report::PREFIX);
        std::process::exit(PROBE_STATUS);
    }

    // devtmpfs first: the initramfs has an empty /dev, so /dev/console
    // does not exist until this mount.
    let _ = mount::simple("devtmpfs", "/dev", "devtmpfs");
    let _ = redirect_stdio();
    let _ = mount::simple("proc", "/proc", "proc");
    let _ = mount::simple("sysfs", "/sys", "sysfs");

    let mut rep = Report::new();
    // CLOCK_MONOTONIC counts from boot, so this is the guest
    // boot-to-init time. Printk timestamps need a console on the kernel
    // command line, and writing to that console slows the boot.
    rep.note(&format!("boot_to_init_us={}", monotonic_us()));
    match run(&mut rep) {
        Ok(()) => {}
        Err(e) => rep.record("fatal", report::Outcome::Fail(e)),
    }
    rep.finish();

    // Never return from PID 1: the kernel panics when init exits.
    poweroff();
}

fn run(rep: &mut Report) -> Result<(), String> {
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let tag = param(&cmdline, "virtiofs.tag")
        .unwrap_or_else(|| DEFAULT_TAG.to_string());
    let role = param(&cmdline, "virtiofs.role")
        .unwrap_or_else(|| "checks".to_string());
    if role == "hotplug" {
        // No share and no tag: this role checks what the host adds to a
        // running VM, and its kernel has no virtio-fs driver.
        let spec = HotplugSpec::from_cmdline(&cmdline)?;
        rep.note(&format!("role=hotplug {}", spec.summary()));
        hotplug_checks::run_hotplug_checks(rep, &spec);
        return Ok(());
    }
    if role == "container" {
        // With no override the image entrypoint runs, as in production.
        // The harness sets an override to run its checks instead.
        let entry = param(&cmdline, "virtiofs.entry");
        rep.note(&format!(
            "tag={tag} role=container entry={}",
            entry.as_deref().unwrap_or("<image entrypoint>")
        ));
        let status = container::run(rep, &tag, entry.as_deref())?;
        return if status == 0 {
            Ok(())
        } else {
            Err(format!("entry script exited {status}"))
        };
    }
    if role != "checks" {
        return Err(format!(
            "virtiofs.role={role}, want checks, container or hotplug"
        ));
    }
    let mode =
        param(&cmdline, "virtiofs.mode").unwrap_or_else(|| "rw".to_string());
    let read_only = match mode.as_str() {
        "ro" => true,
        "rw" => false,
        other => return Err(format!("virtiofs.mode={other}, want ro or rw")),
    };
    rep.note(&format!("tag={tag} mode={mode}"));

    std::fs::create_dir_all(MOUNT_POINT)
        .map_err(|e| format!("mkdir {MOUNT_POINT}: {e}"))?;

    // The mount is the first check: nothing after it can run if the
    // driver did not bind or the tag does not match.
    let mnt = Path::new(MOUNT_POINT);
    rep.run("mount", || {
        mount::simple(&tag, MOUNT_POINT, "virtiofs")
            .map(|()| format!("virtiofs tag={tag} on {MOUNT_POINT}"))
    });
    if !is_mounted(mnt) {
        return Err("share is not mounted; skipping the rest".to_string());
    }

    if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
        if let Some(line) = mounts.lines().find(|l| l.contains(" /mnt ")) {
            rep.note(&format!("mount line: {line}"));
        }
    }

    // Started first so the host has the whole run to connect. Joined
    // after the other checks.
    let listener = checks::spawn_vsock_listener(checks::VSOCK_LISTEN_PORT);

    checks::run_rng_checks(rep);
    checks::run_vsock_checks(rep, checks::VSOCK_ECHO_PORT);
    checks::run_read_checks(rep, mnt);
    checks::run_write_checks(rep, mnt, read_only);

    rep.record(
        "vsock_host_to_guest",
        match listener.join() {
            Ok(Ok(detail)) => report::Outcome::Pass(detail),
            Ok(Err(why)) => report::Outcome::Fail(why),
            Err(_) => report::Outcome::Fail("listener panicked".to_string()),
        },
    );

    // Reported after the checks so the counts cover the whole workload.
    // The lines also name the interrupt path: PCI-MSIX when the guest
    // has CONFIG_PCI_MSI, IO-APIC when it uses INTx.
    if let Ok(irqs) = std::fs::read_to_string("/proc/interrupts") {
        for line in irqs.lines().filter(|l| l.contains("virtio")) {
            rep.note(&format!("irq:{}", line.trim()));
        }
    }

    // Unmount so the device sees the guest release its handles. That is
    // the only in-band proof that teardown works.
    rep.run("unmount", || {
        let c = CString::new(MOUNT_POINT).unwrap();
        if unsafe { libc::umount(c.as_ptr()) } != 0 {
            return Err(format!("umount: {}", std::io::Error::last_os_error()));
        }
        Ok("share unmounted".to_string())
    });
    Ok(())
}

fn is_mounted(mnt: &Path) -> bool {
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let want = format!(" {} ", mnt.display());
    mounts
        .lines()
        .any(|l| l.contains(&want) && l.contains("virtiofs"))
}

/// Point stdio at /dev/console so every report line reaches the serial
/// port the VMM mirrors to its stdout.
fn redirect_stdio() -> Result<(), String> {
    let path = CString::new("/dev/console").unwrap();
    let fd =
        unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if fd < 0 {
        return Err(format!(
            "open /dev/console: {}",
            std::io::Error::last_os_error()
        ));
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

/// Microseconds since boot.
fn monotonic_us() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return 0;
    }
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1_000
}

fn poweroff() -> ! {
    let _ = std::fs::write("/proc/sys/kernel/sysrq", "1");
    let _ = std::fs::write("/proc/sysrq-trigger", "o");
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_POWER_OFF);
    }
    loop {
        unsafe { libc::pause() };
    }
}
