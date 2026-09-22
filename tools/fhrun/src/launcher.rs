// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Spawn the VMM after preparing its initramfs and argv.
//!
//! The VMM is driven by the shared `vmm-config` argv grammar, not by a
//! JSON config file: one grammar means a manifest change that the VMM
//! cannot honor fails at parse time instead of at boot.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{Context, Result};

use crate::initramfs;
use crate::manifest::{Manifest, ResolvedConsole};
use crate::status::{self, PayloadStatus};

/// PID of the running VMM child, read by the signal handler.
/// 0 means there is no child yet.
static CHILD_PID: AtomicI32 = AtomicI32::new(0);

/// First PCI slot for a viona NIC.
pub const PCI_SLOT_NIC_BASE: u8 = 7;
/// Last PCI slot for a viona NIC.
pub const PCI_SLOT_NIC_MAX: u8 = 10;
/// First PCI slot for a virtio-console.
pub const PCI_SLOT_CONSOLE_BASE: u8 = 15;
/// Last PCI slot for a virtio-console.
pub const PCI_SLOT_CONSOLE_MAX: u8 = 16;

/// A fully built VMM command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmmInvocation {
    pub program: PathBuf,
    /// `OsString` because kernel, initrd and socket paths need not be
    /// UTF-8.
    pub args: Vec<OsString>,
    pub consoles: Vec<ResolvedConsole>,
}

pub struct LaunchOutcome {
    /// Exit code from the VMM. `None` when it died from a signal.
    pub vmm_exit_code: Option<i32>,
    /// What init reported for the payload. `None` when the VMM ended
    /// before init wrote its marker.
    pub payload: Option<PayloadStatus>,
}

/// The fhrun-owned kernel cmdline.
///
/// `tsc=reliable` is required: without it Linux can fall back to HPET,
/// which turns every `ktime_get()` into an MMIO trap to the host.
pub fn build_kernel_cmdline(manifest: &Manifest) -> String {
    let mut cmdline = String::from(
        "console=ttyS0 earlyprintk=ttyS0 root=/dev/ram0 \
         init=/init rdinit=/init panic=-1 fhrun=1 tsc=reliable",
    );
    if !manifest.kernel_extra_cmdline.is_empty() {
        cmdline.push(' ');
        cmdline.push_str(&manifest.kernel_extra_cmdline);
    }
    cmdline
}

/// The `index`-th slot of the window `base..=max`.
///
/// `kind` names the devices in the error. Bounding the index before the
/// sum keeps the arithmetic in range.
fn slot_in_window(
    kind: &str,
    base: u8,
    max: u8,
    index: usize,
) -> Result<usize> {
    let span = max
        .checked_sub(base)
        .with_context(|| format!("bad PCI slot window {base}..={max}"))?;
    anyhow::ensure!(
        index <= usize::from(span),
        "too many {kind} for PCI slots {base}..={max}"
    );
    Ok(usize::from(base) + index)
}

/// Build the VMM argv from the manifest.
///
/// The manifest's own validation already caps NIC and console counts,
/// so the slot checks here only catch a `Manifest` built in code.
pub fn build_vmm_invocation(
    manifest: &Manifest,
    initramfs_path: &Path,
    runtime_dir: &Path,
) -> Result<VmmInvocation> {
    let mut args: Vec<OsString> = vec![
        OsString::from("-c"),
        OsString::from(manifest.vcpus.to_string()),
        OsString::from("-m"),
        OsString::from(&manifest.memory),
        OsString::from("--kernel"),
        manifest.kernel.clone().into_os_string(),
        OsString::from("--initrd"),
        initramfs_path.to_path_buf().into_os_string(),
        OsString::from("--cmdline"),
        OsString::from(build_kernel_cmdline(manifest)),
        // An fhrun VM never migrates, so it needs no dirty bitmap.
        OsString::from("--no-track-dirty"),
        OsString::from("-l"),
        OsString::from("com1,stdio"),
    ];

    for (index, nic) in manifest.all_nics().enumerate() {
        let slot =
            slot_in_window("NICs", PCI_SLOT_NIC_BASE, PCI_SLOT_NIC_MAX, index)?;
        args.push(OsString::from("-s"));
        args.push(OsString::from(format!(
            "{slot},virtio-net-viona,{}",
            nic.vnic
        )));
    }

    let consoles = manifest.resolved_consoles(runtime_dir);
    for console in &consoles {
        let slot = slot_in_window(
            "consoles",
            PCI_SLOT_CONSOLE_BASE,
            PCI_SLOT_CONSOLE_MAX,
            console.index,
        )?;
        args.push(OsString::from("-s"));
        // The device config is the bare socket path. A `key=` prefix
        // would become part of the path the device binds.
        let mut spec = OsString::from(format!("{slot},virtio-console,"));
        spec.push(console.socket_path.as_os_str());
        args.push(spec);
    }

    // Positional VM name goes last.
    args.push(OsString::from(&manifest.name));

    Ok(VmmInvocation {
        program: manifest.vmm.clone(),
        args,
        consoles,
    })
}

pub fn launch(manifest: &Manifest) -> Result<LaunchOutcome> {
    let tmp = TempDir::new("fhrun")?;
    let initramfs_path = tmp.path().join("initramfs.cpio");

    let blob = initramfs::build(manifest).context("build initramfs")?;
    std::fs::write(&initramfs_path, &blob)
        .with_context(|| format!("write {}", initramfs_path.display()))?;

    let inv = build_vmm_invocation(manifest, &initramfs_path, tmp.path())?;
    for console in &inv.consoles {
        eprintln!(
            "fhrun: console{} {} -> {} (role {})",
            console.index,
            console.socket_path.display(),
            console.guest_device,
            console.role.as_deref().unwrap_or("none"),
        );
    }

    // SIGINT and SIGTERM stay blocked until the handler is installed and
    // the child pid is readable. Without this an operator's Ctrl-C in
    // that window either ends fhrun and orphans the VMM, or reaches a
    // handler that reads pid 0 and drops the signal. `Command` resets
    // the mask in the child, so the VMM starts with both deliverable.
    let mask = BlockedSignals::block(&[libc::SIGINT, libc::SIGTERM]);

    // The VMM's stdout is piped, not inherited: the payload's status
    // travels on it as a COM2 marker line, so fhrun has to read it.
    let mut child: Child = Command::new(&inv.program)
        .args(&inv.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawn vmm: {}", inv.program.display()))?;

    CHILD_PID.store(child.id() as i32, Ordering::SeqCst);
    install_signal_handlers();
    drop(mask);
    let vmm_stdout = child.stdout.take().context("vmm stdout is not piped")?;
    let pump = std::thread::Builder::new()
        .name("vmm-stdout".to_string())
        .spawn(move || status::pump(vmm_stdout, std::io::stdout()))
        .context("spawn the stdout pump")?;
    let vmm_status = child.wait().context("wait for vmm")?;
    CHILD_PID.store(0, Ordering::SeqCst);
    // The pump ends at EOF, which the VMM's exit closes.
    let payload = match pump.join() {
        Ok(Ok(payload)) => payload,
        Ok(Err(e)) => {
            eprintln!("fhrun: reading vmm stdout: {e}");
            None
        }
        Err(_) => {
            eprintln!("fhrun: the stdout pump panicked");
            None
        }
    };

    Ok(LaunchOutcome {
        vmm_exit_code: vmm_status.code(),
        payload,
    })
}

// -- signals ----------------------------------------------------------

fn install_signal_handlers() {
    // The two-step cast (function to *const (), then to an integer)
    // avoids the `function_casts_as_integer` warning.
    let h = forward_signal as *const () as libc::sighandler_t;
    // SAFETY: `forward_signal` reads one atomic and calls `kill`, both
    // of which are async-signal-safe.
    unsafe {
        libc::signal(libc::SIGINT, h);
        libc::signal(libc::SIGTERM, h);
    }
}

/// Signals held pending for the life of this value.
struct BlockedSignals(libc::sigset_t);

impl BlockedSignals {
    fn block(signals: &[libc::c_int]) -> Self {
        // SAFETY: both sets are whole `sigset_t` values owned here, and
        // `pthread_sigmask` only reads `set` and writes `old`.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for sig in signals {
                libc::sigaddset(&mut set, *sig);
            }
            let mut old: libc::sigset_t = std::mem::zeroed();
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
            Self(old)
        }
    }
}

impl Drop for BlockedSignals {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the mask this value replaced.
        unsafe {
            libc::pthread_sigmask(
                libc::SIG_SETMASK,
                &self.0,
                std::ptr::null_mut(),
            );
        }
    }
}

extern "C" fn forward_signal(sig: libc::c_int) {
    let pid = CHILD_PID.load(Ordering::SeqCst);
    if pid != 0 {
        // Translate INT to TERM so the VMM runs its ACPI-poweroff path.
        // It registers SIGTERM only.
        let target = if sig == libc::SIGINT {
            libc::SIGTERM
        } else {
            sig
        };
        unsafe {
            libc::kill(pid, target);
        }
    }
}

// -- temp dir ---------------------------------------------------------

/// A temp directory removed on drop.
pub struct TempDir(tempfile::TempDir);

impl TempDir {
    fn new(prefix: &str) -> Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix(&format!("{prefix}-"))
            .tempdir()
            .context("create temp dir")?;
        Ok(Self(dir))
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ConsoleConfig, Manifest, NetConfig};
    use std::collections::BTreeMap;

    fn manifest() -> Manifest {
        Manifest {
            name: "edge".to_string(),
            bin: PathBuf::from("/bin/app"),
            args: Vec::new(),
            env: BTreeMap::new(),
            workdir: "/".to_string(),
            vcpus: 2,
            memory: "256M".to_string(),
            kernel: PathBuf::from("/kernel"),
            init: PathBuf::from("/init"),
            extra_files: BTreeMap::new(),
            net: None,
            nics: Vec::new(),
            consoles: Vec::new(),
            guest_metadata: None,
            vmm: PathBuf::from("firehyve"),
            kernel_extra_cmdline: String::new(),
        }
    }

    fn nic(vnic: &str) -> NetConfig {
        NetConfig {
            vnic: vnic.to_string(),
            mac: "02:00:00:00:00:01".to_string(),
            ip: "10.0.0.5/24".to_string(),
            gateway: None,
            role: None,
        }
    }

    fn arg_strings(inv: &VmmInvocation) -> Vec<String> {
        inv.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    static USR2_SEEN: AtomicI32 = AtomicI32::new(0);

    extern "C" fn count_usr2(sig: libc::c_int) {
        USR2_SEEN.fetch_add(1, Ordering::SeqCst);
        let _ = sig;
    }

    /// The spawn window relies on a blocked signal staying pending
    /// until the guard is dropped. SIGUSR2 stands in for SIGINT so the
    /// test harness keeps its own disposition.
    ///
    /// Mutation this kills: dropping the guard's `Drop`, or blocking
    /// nothing.
    #[test]
    fn a_signal_raised_while_blocked_arrives_after_the_guard_goes() {
        let h = count_usr2 as *const () as libc::sighandler_t;
        // SAFETY: the handler touches one atomic, which is
        // async-signal-safe.
        unsafe { libc::signal(libc::SIGUSR2, h) };

        let guard = BlockedSignals::block(&[libc::SIGUSR2]);
        // SAFETY: raise takes a signal number and nothing else.
        unsafe { libc::raise(libc::SIGUSR2) };
        assert_eq!(
            USR2_SEEN.load(Ordering::SeqCst),
            0,
            "a blocked signal was delivered"
        );

        drop(guard);

        assert_eq!(
            USR2_SEEN.load(Ordering::SeqCst),
            1,
            "the pending signal was lost"
        );
    }

    #[test]
    fn invocation_round_trips_through_vmm_config() {
        use clap::Parser;

        let mut m = manifest();
        m.nics = vec![nic("vnic0")];
        m.consoles = vec![ConsoleConfig::default()];

        let inv = build_vmm_invocation(
            &m,
            Path::new("/tmp/x/initramfs.cpio"),
            Path::new("/tmp/x"),
        )
        .expect("build invocation");

        let argv = std::iter::once(OsString::from("firehyve"))
            .chain(inv.args.iter().cloned());
        let cli = vmm_config::Cli::parse_from(argv);

        assert_eq!(cli.vm_name, "edge");
        assert_eq!(cli.num_cpus().expect("cpus"), 2);
        assert_eq!(cli.mem_size().expect("mem"), 256 * 1024 * 1024);
        assert_eq!(cli.kernel.as_deref(), Some(Path::new("/kernel")));
        assert_eq!(
            cli.initrd.as_deref(),
            Some(Path::new("/tmp/x/initramfs.cpio"))
        );
        assert_eq!(
            cli.cmdline.as_deref(),
            Some(build_kernel_cmdline(&m).as_str())
        );
        assert!(cli.no_track_dirty);
        assert_eq!(cli.lpc, vec!["com1,stdio".to_string()]);
        assert_eq!(
            cli.pci_slot,
            vec![
                "7,virtio-net-viona,vnic0".to_string(),
                "15,virtio-console,/tmp/x/console0.sock".to_string(),
            ]
        );
    }

    // The console device config is a bare socket path. `vmm_virtio`'s
    // `parse_console_config` takes the whole config field as the path,
    // so a `socket=` prefix would become part of the path.
    #[test]
    fn console_spec_is_parsed_back_by_the_device_catalog() {
        let mut m = manifest();
        m.consoles = vec![ConsoleConfig::default()];

        let inv = build_vmm_invocation(
            &m,
            Path::new("/tmp/x/initramfs.cpio"),
            Path::new("/tmp/x"),
        )
        .expect("build invocation");
        let args = arg_strings(&inv);
        let spec = args
            .iter()
            .find(|a| a.contains("virtio-console"))
            .expect("console spec");
        let config = spec
            .splitn(3, ',')
            .nth(2)
            .expect("console spec has a config field");

        assert_eq!(config, "/tmp/x/console0.sock");
    }

    #[test]
    fn consoles_get_sequential_slots_and_default_sockets() {
        let mut m = manifest();
        m.consoles = vec![ConsoleConfig::default(), ConsoleConfig::default()];

        let inv = build_vmm_invocation(
            &m,
            Path::new("/tmp/x/initramfs.cpio"),
            Path::new("/tmp/x"),
        )
        .expect("build invocation");
        let args = arg_strings(&inv);

        assert!(args
            .contains(&"15,virtio-console,/tmp/x/console0.sock".to_string()));
        assert!(args
            .contains(&"16,virtio-console,/tmp/x/console1.sock".to_string()));
        assert_eq!(inv.consoles.len(), 2);
        assert_eq!(inv.consoles[1].guest_device, "/dev/hvc1");
    }

    #[test]
    fn explicit_console_socket_path_is_used() {
        let mut m = manifest();
        m.consoles = vec![ConsoleConfig {
            socket_path: Some(PathBuf::from("/var/run/triton/control.sock")),
            guest_device: None,
            role: None,
        }];

        let inv = build_vmm_invocation(
            &m,
            Path::new("/tmp/x/initramfs.cpio"),
            Path::new("/tmp/x"),
        )
        .expect("build invocation");
        let args = arg_strings(&inv);

        assert!(args.contains(
            &"15,virtio-console,/var/run/triton/control.sock".to_string()
        ));
    }

    #[test]
    fn no_consoles_emits_no_console_device() {
        let inv = build_vmm_invocation(
            &manifest(),
            Path::new("/tmp/x/initramfs.cpio"),
            Path::new("/tmp/x"),
        )
        .expect("build invocation");
        let args = arg_strings(&inv);

        assert!(!args.iter().any(|a| a.contains("virtio-console")));
        assert!(inv.consoles.is_empty());
    }

    #[test]
    fn nic_slots_start_at_seven_and_are_capped() {
        let mut m = manifest();
        m.nics = vec![nic("a"), nic("b"), nic("c"), nic("d"), nic("e")];

        let err = build_vmm_invocation(
            &m,
            Path::new("/tmp/x/initramfs.cpio"),
            Path::new("/tmp/x"),
        )
        .expect_err("five NICs exhaust the slot window");

        assert!(err.to_string().contains("PCI slots"), "{err}");
    }

    #[test]
    fn cmdline_appends_kernel_extra() {
        let mut m = manifest();
        m.kernel_extra_cmdline = "quiet".to_string();

        let cmdline = build_kernel_cmdline(&m);

        assert!(cmdline.starts_with("console=ttyS0 earlyprintk=ttyS0"));
        assert!(cmdline.contains("tsc=reliable"));
        assert!(cmdline.ends_with(" quiet"));
    }
}
