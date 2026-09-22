// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Boot a container image served over virtio-fs.
//!
//! The share holds an OCI image rootfs and is mounted read-only. A tmpfs
//! is the writable layer, so guest writes do not reach the host export,
//! and the VMs that share that export stay isolated.
//!
//! The share holds `rootfs/`, the image tree, and `container.json`, the
//! runtime config derived from the image OCI config. The config is next
//! to the tree, not in it, so the container cannot see or change it.
//!
//! The hand-off is `switch_root`, as in busybox: move the
//! pseudo-filesystems into the new root, move the new root onto `/`,
//! then `chroot`. `pivot_root` does not work when the current root is
//! the initramfs.

use std::ffi::CString;
use std::path::Path;

use crate::mount;
use crate::report::Report;
use crate::spec::ContainerSpec;

/// Where the whole share is mounted.
const SHARE: &str = "/share";
/// The image tree inside the share.
const IMAGE: &str = "/share/rootfs";
/// The runtime config inside the share.
const SPEC: &str = "/share/container.json";
const OVER: &str = "/over";
const NEWROOT: &str = "/newroot";
/// Interpreter for the `virtiofs.entry=` test override. The normal path
/// execs the image entrypoint directly, so an image without a shell
/// still boots.
const SHELL: &str = "/bin/sh";

/// Assemble the root filesystem and run the image's command inside it.
///
/// `entry_override`, when set, replaces the image command. The harness
/// uses it to run its checks against any image.
///
/// Returns the command exit status. The caller powers the VM off. This
/// never execs in place, so a failing command still reports.
pub fn run(
    rep: &mut Report,
    tag: &str,
    entry_override: Option<&str>,
) -> Result<i32, String> {
    for dir in [SHARE, OVER, NEWROOT] {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("mkdir {dir}: {e}"))?;
    }

    rep.run("mount_share", || {
        // MS_RDONLY in addition to the device `ro`: the guest does not
        // trust the host configuration.
        mount::mount(tag, SHARE, "virtiofs", libc::MS_RDONLY, None)
            .map(|()| format!("virtiofs tag={tag} on {SHARE} ro"))
    });
    if !Path::new(IMAGE).join("etc").exists() && !Path::new(IMAGE).exists() {
        return Err(format!("{IMAGE} does not look like a rootfs"));
    }

    // Checked here, not inside the image: after switch_root the share is
    // outside the new root, so /proc/mounts no longer lists it.
    rep.run("image_is_on_virtiofs", || {
        let mounts = std::fs::read_to_string("/proc/mounts")
            .map_err(|e| format!("read /proc/mounts: {e}"))?;
        let want = format!(" {SHARE} ");
        if mounts
            .lines()
            .any(|l| l.contains(&want) && l.contains("virtiofs"))
        {
            Ok(format!("{SHARE} is virtiofs"))
        } else {
            Err(format!("{SHARE} is not a virtiofs mount"))
        }
    });

    // Read before switch_root, while the share is reachable. The
    // override replaces only the argv: env, workdir and user still come
    // from the image, so the override path tests them too.
    let spec = match std::fs::read_to_string(SPEC) {
        Ok(raw) => ContainerSpec::parse(&raw)?,
        Err(e) if entry_override.is_some() => {
            rep.note(&format!("no {SPEC} ({e}); using defaults"));
            ContainerSpec::default()
        }
        Err(e) => return Err(format!("read {SPEC}: {e}")),
    };
    // Resolve accounts while the image still has its own path.
    let (uid, gid) = spec.resolve_user(Path::new(IMAGE))?;

    rep.run("mount_writable_layer", || {
        mount::mount("tmpfs", OVER, "tmpfs", 0, None)?;
        for d in ["upper", "work"] {
            let p = format!("{OVER}/{d}");
            std::fs::create_dir_all(&p)
                .map_err(|e| format!("mkdir {p}: {e}"))?;
        }
        Ok("tmpfs upper and work layers".to_string())
    });

    rep.run("mount_overlay", || {
        let data = format!(
            "lowerdir={IMAGE},upperdir={OVER}/upper,workdir={OVER}/work"
        );
        mount::mount("overlay", NEWROOT, "overlay", 0, Some(&data))
            .map(|()| "overlay merged on the image".to_string())
    });

    rep.run("switch_root", || {
        switch_root().map(|()| "/ is the image".to_string())
    });

    let (argv, env) = match entry_override {
        Some(entry) => (
            vec![SHELL.to_string(), entry.to_string()],
            spec.environment(),
        ),
        None => (spec.argv()?, spec.environment()),
    };
    rep.note(&format!(
        "exec {argv:?} as {uid}:{gid} in {}",
        spec.workdir()
    ));

    // From here the report lines come from inside the image. The
    // console fd was opened before the switch, so it stays valid.
    run_command(&argv, &env, spec.workdir(), uid, gid)
}

/// Move the pseudo-filesystems into the new root, then make it `/`.
fn switch_root() -> Result<(), String> {
    for dir in ["dev", "proc", "sys"] {
        let target = format!("{NEWROOT}/{dir}");
        // An image usually has these as empty directories, but it does
        // not have to.
        std::fs::create_dir_all(&target)
            .map_err(|e| format!("mkdir {target}: {e}"))?;
        mount::move_to(&format!("/{dir}"), &target)?;
    }

    chdir(NEWROOT)?;
    mount::move_to(".", "/")?;
    let dot = CString::new(".").unwrap();
    if unsafe { libc::chroot(dot.as_ptr()) } != 0 {
        return Err(format!("chroot: {}", std::io::Error::last_os_error()));
    }
    chdir("/")?;
    Ok(())
}

fn chdir(path: &str) -> Result<(), String> {
    let c = CString::new(path).map_err(|_| "path has NUL".to_string())?;
    if unsafe { libc::chdir(c.as_ptr()) } != 0 {
        return Err(format!(
            "chdir {path}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Run `argv` as `uid:gid` in `workdir`, and wait for it.
fn run_command(
    argv: &[String],
    env: &[String],
    workdir: &str,
    uid: u32,
    gid: u32,
) -> Result<i32, String> {
    let argv_c = to_cstrings(argv, "argv")?;
    let env_c = to_cstrings(env, "env")?;
    let mut argv_p: Vec<*const libc::c_char> =
        argv_c.iter().map(|c| c.as_ptr()).collect();
    argv_p.push(std::ptr::null());
    let mut env_p: Vec<*const libc::c_char> =
        env_c.iter().map(|c| c.as_ptr()).collect();
    env_p.push(std::ptr::null());
    let workdir_c =
        CString::new(workdir).map_err(|_| "workdir has NUL".to_string())?;

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork: {}", std::io::Error::last_os_error()));
    }
    if pid == 0 {
        // Child. No allocation after fork, and every failure must
        // _exit: a return would run the rest of the harness twice.
        unsafe {
            if libc::chdir(workdir_c.as_ptr()) != 0 {
                libc::_exit(125);
            }
            // Group first: after setuid the process can no longer set
            // the group.
            if gid != 0 && libc::setgid(gid) != 0 {
                libc::_exit(126);
            }
            if uid != 0 && libc::setuid(uid) != 0 {
                libc::_exit(126);
            }
            libc::execve(argv_c[0].as_ptr(), argv_p.as_ptr(), env_p.as_ptr());
            libc::_exit(127)
        };
    }

    let mut status: libc::c_int = 0;
    if unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
        return Err(format!("waitpid: {}", std::io::Error::last_os_error()));
    }
    if libc::WIFSIGNALED(status) {
        return Err(format!(
            "{} killed by signal {}",
            argv[0],
            libc::WTERMSIG(status)
        ));
    }
    if !libc::WIFEXITED(status) {
        return Err(format!("{} did not exit: status {status}", argv[0]));
    }
    let code = libc::WEXITSTATUS(status);
    match code {
        125 => Err(format!("chdir to {workdir} failed")),
        126 => Err(format!("could not become {uid}:{gid}")),
        127 => Err(format!("exec {} failed", argv[0])),
        _ => Ok(code),
    }
}

fn to_cstrings(v: &[String], what: &str) -> Result<Vec<CString>, String> {
    v.iter()
        .map(|s| {
            CString::new(s.as_str())
                .map_err(|_| format!("{what} entry has NUL: {s}"))
        })
        .collect()
}
