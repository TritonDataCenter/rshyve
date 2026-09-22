// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `fhrun` manifest: what binary to run and how it looks from outside.
//!
//! `consoles` is a generic virtio-console list. It carries a host socket
//! path and a guest device node, and no protocol semantics. A caller that
//! speaks its own protocol over one of these tags it with `role` and puts
//! its payload in `guest_metadata`, which fhrun forwards to the guest
//! without inspecting it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The user-supplied recipe for one fhrun invocation.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Manifest {
    /// Human-readable name. Becomes the VM name, so it must fit
    /// `/dev/vmm/<name>` rules (1..=127 chars, no NUL).
    pub name: String,

    /// Host path to the Linux ELF binary that runs inside the guest.
    pub bin: PathBuf,

    /// argv[1..]. argv[0] is `bin`'s basename.
    #[serde(default)]
    pub args: Vec<String>,

    #[serde(default)]
    pub env: BTreeMap<String, String>,

    #[serde(default = "default_workdir")]
    pub workdir: String,

    #[serde(default = "default_vcpus")]
    pub vcpus: usize,

    /// Memory size string. Parsed by the VMM, not by fhrun.
    #[serde(default = "default_mem")]
    pub memory: String,

    /// Host path to the kernel. The boot protocol is detected from the
    /// file, so there is no protocol field here.
    pub kernel: PathBuf,

    /// Host path to the static `fhrun-init` binary. Becomes `/init`.
    pub init: PathBuf,

    /// Extra files copied into the guest rootfs. The key is the relative
    /// in-guest path. The value is the host path to copy from.
    #[serde(default)]
    pub extra_files: BTreeMap<String, PathBuf>,

    /// Single-NIC alias. Always becomes `eth0`.
    #[serde(default)]
    pub net: Option<NetConfig>,

    /// Additional NICs, in `eth1..` order.
    #[serde(default)]
    pub nics: Vec<NetConfig>,

    /// Virtio-console channels, in declaration order.
    #[serde(default)]
    pub consoles: Vec<ConsoleConfig>,

    /// Opaque payload forwarded to the guest spec. fhrun does not read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_metadata: Option<serde_json::Value>,

    /// Path to the VMM binary. The `firehyve` alias keeps older
    /// manifests parsing.
    #[serde(default = "default_vmm", alias = "firehyve")]
    pub vmm: PathBuf,

    /// Appended to the fhrun-owned kernel cmdline. Use sparingly.
    #[serde(default)]
    pub kernel_extra_cmdline: String,
}

/// One NIC: a viona attachment on the host, an `ethN` in the guest.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NetConfig {
    /// Host vnic name.
    pub vnic: String,
    /// MAC address `xx:xx:xx:xx:xx:xx`. Validated for shape only. The
    /// address itself lives on the host vnic, so the guest ignores it.
    pub mac: String,
    /// In-guest IPv4 address in CIDR form.
    pub ip: String,
    /// Optional default gateway. Only the first NIC with one installs
    /// a default route.
    #[serde(default)]
    pub gateway: Option<String>,
    /// Opaque operator label.
    #[serde(default)]
    pub role: Option<String>,
}

/// One virtio-console channel. Both paths are optional because their
/// defaults come from the console's index, which serde cannot see.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ConsoleConfig {
    /// Host Unix socket. Defaults to `<runtime_dir>/console<N>.sock`.
    #[serde(default)]
    pub socket_path: Option<PathBuf>,
    /// Guest device node. Defaults to `/dev/hvc<N>`.
    #[serde(default)]
    pub guest_device: Option<String>,
    /// Opaque label so a caller can say which channel is which.
    #[serde(default)]
    pub role: Option<String>,
}

/// A [`ConsoleConfig`] with every default filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConsole {
    pub index: usize,
    pub socket_path: PathBuf,
    pub guest_device: String,
    pub role: Option<String>,
}

/// The trimmed spec handed to the in-guest init at
/// `/firehyve-spec.json`. Host-only fields are stripped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestSpec {
    pub bin: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub workdir: String,
    #[serde(default)]
    pub nics: Vec<NetConfig>,
    #[serde(default)]
    pub consoles: Vec<GuestConsole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Guest-visible half of a console. The host socket path is stripped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestConsole {
    pub guest_device: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

/// Refuse an `extra_files` key the cpio name cannot carry as written.
///
/// A NUL ends the name early, so the guest would get a different file
/// from the one named. `..` is resolved by the kernel's unpacker against
/// the archive root, so it cannot escape, but it still puts the file
/// somewhere the manifest does not say.
fn check_archive_name(dest: &str) -> anyhow::Result<()> {
    if dest.is_empty() {
        anyhow::bail!("extra_files key must not be empty");
    }
    if dest.starts_with('/') {
        anyhow::bail!("extra_files key must be relative (got {dest})");
    }
    if dest.as_bytes().contains(&0) {
        anyhow::bail!("extra_files key must not contain NUL (got {dest:?})");
    }
    if dest.split('/').any(|part| part == ".." || part == ".") {
        anyhow::bail!(
            "extra_files key must not contain '.' or '..' (got {dest})"
        );
    }
    Ok(())
}

fn default_workdir() -> String {
    "/".to_string()
}

fn default_vcpus() -> usize {
    1
}

fn default_mem() -> String {
    "128M".to_string()
}

fn default_vmm() -> PathBuf {
    PathBuf::from("firehyve")
}

/// Guest device node for console `index`, honoring an explicit override.
fn console_guest_device(index: usize, cfg: &ConsoleConfig) -> String {
    cfg.guest_device
        .clone()
        .unwrap_or_else(|| format!("/dev/hvc{index}"))
}

/// Maximum consoles. Matches the PCI slot window the VMM reserves.
const MAX_CONSOLES: usize = 2;

/// Maximum NICs. Matches the PCI slot window the VMM reserves.
const MAX_NICS: usize = 4;

impl Manifest {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let data = std::fs::read_to_string(path).map_err(|e| {
            anyhow::anyhow!("read manifest {}: {e}", path.display())
        })?;
        let m: Manifest = serde_json::from_str(&data).map_err(|e| {
            anyhow::anyhow!("parse manifest {}: {e}", path.display())
        })?;
        m.validate()?;
        Ok(m)
    }

    /// Every NIC in declaration order: legacy `net` first, then `nics`.
    pub fn all_nics(&self) -> impl Iterator<Item = &NetConfig> {
        self.net.iter().chain(self.nics.iter())
    }

    /// Every console with its defaults filled in. Empty when the
    /// manifest declares none: there is no implicit console.
    pub fn resolved_consoles(
        &self,
        runtime_dir: &Path,
    ) -> Vec<ResolvedConsole> {
        self.consoles
            .iter()
            .enumerate()
            .map(|(index, cfg)| ResolvedConsole {
                index,
                socket_path: cfg.socket_path.clone().unwrap_or_else(|| {
                    runtime_dir.join(format!("console{index}.sock"))
                }),
                guest_device: console_guest_device(index, cfg),
                role: cfg.role.clone(),
            })
            .collect()
    }

    /// Project the manifest down to the in-guest spec. Takes no runtime
    /// directory, so `--emit-initramfs` works with no VM running.
    pub fn to_guest_spec(&self, in_guest_bin: &str) -> GuestSpec {
        GuestSpec {
            bin: in_guest_bin.to_string(),
            args: self.args.clone(),
            env: self.env.clone(),
            workdir: self.workdir.clone(),
            nics: self.all_nics().cloned().collect(),
            consoles: self
                .consoles
                .iter()
                .enumerate()
                .map(|(index, cfg)| GuestConsole {
                    guest_device: console_guest_device(index, cfg),
                    role: cfg.role.clone(),
                })
                .collect(),
            metadata: self.guest_metadata.clone(),
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.name.is_empty() || self.name.len() > 127 {
            anyhow::bail!("manifest.name must be 1..=127 chars");
        }
        if self.name.as_bytes().contains(&0) {
            anyhow::bail!("manifest.name must not contain NUL");
        }
        if self.vcpus == 0 {
            anyhow::bail!("manifest.vcpus must be >= 1");
        }
        if !self.bin.is_file() {
            anyhow::bail!("manifest.bin not a file: {}", self.bin.display());
        }
        if !self.kernel.is_file() {
            anyhow::bail!(
                "manifest.kernel not a file: {}",
                self.kernel.display()
            );
        }
        if !self.init.is_file() {
            anyhow::bail!("manifest.init not a file: {}", self.init.display());
        }
        for (dest, src) in &self.extra_files {
            check_archive_name(dest)?;
            if !src.is_file() {
                anyhow::bail!(
                    "extra_files[{dest}] missing on host: {}",
                    src.display()
                );
            }
        }
        let mut total = 0usize;
        for n in self.all_nics() {
            total += 1;
            if n.vnic.is_empty() {
                anyhow::bail!("nic.vnic must not be empty");
            }
            if n.mac.split(':').count() != 6 {
                anyhow::bail!(
                    "nic.mac must be xx:xx:xx:xx:xx:xx (got {})",
                    n.mac
                );
            }
            if !n.ip.contains('/') {
                anyhow::bail!(
                    "nic.ip must be CIDR form a.b.c.d/N (got {})",
                    n.ip
                );
            }
        }
        if total > MAX_NICS {
            anyhow::bail!("too many NICs: max {MAX_NICS}, got {total}");
        }
        if self.consoles.len() > MAX_CONSOLES {
            anyhow::bail!(
                "too many consoles: max {MAX_CONSOLES}, got {}",
                self.consoles.len()
            );
        }
        let mut seen_devices: BTreeSet<String> = BTreeSet::new();
        for (index, cfg) in self.consoles.iter().enumerate() {
            if let Some(socket) = &cfg.socket_path {
                if socket.as_os_str().is_empty() {
                    anyhow::bail!(
                        "consoles[{index}].socket_path must not be empty"
                    );
                }
                if let Some(parent) = socket.parent() {
                    if !parent.as_os_str().is_empty() && !parent.is_dir() {
                        anyhow::bail!(
                            "consoles[{index}].socket_path parent must be a directory: {}",
                            parent.display()
                        );
                    }
                }
            }
            let device = console_guest_device(index, cfg);
            if device.is_empty() {
                anyhow::bail!(
                    "consoles[{index}].guest_device must not be empty"
                );
            }
            if !seen_devices.insert(device.clone()) {
                anyhow::bail!("duplicate console guest_device: {device}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Serial number of the next test directory in this process.
    ///
    /// The clock cannot separate these. Two test threads that start
    /// together read the same `SystemTime`, because macOS reports it in
    /// steps coarser than the gap between the threads, and the second
    /// `create_dir` then fails with EEXIST. A counter always differs.
    /// The pid keeps two concurrent test processes apart.
    static DIR_SERIAL: AtomicUsize = AtomicUsize::new(0);

    struct TestFiles {
        dir: PathBuf,
        bin: PathBuf,
        kernel: PathBuf,
        init: PathBuf,
    }

    impl TestFiles {
        fn new() -> Self {
            let serial = DIR_SERIAL.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "fhrun-manifest-test-{}-{serial}",
                std::process::id()
            ));
            std::fs::create_dir(&dir).expect("create test dir");
            let bin = dir.join("bin");
            let kernel = dir.join("kernel");
            let init = dir.join("init");
            std::fs::write(&bin, b"bin").expect("write bin");
            std::fs::write(&kernel, b"kernel").expect("write kernel");
            std::fs::write(&init, b"init").expect("write init");
            Self {
                dir,
                bin,
                kernel,
                init,
            }
        }
    }

    impl Drop for TestFiles {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn manifest(files: &TestFiles) -> Manifest {
        Manifest {
            name: "edge".to_string(),
            bin: files.bin.clone(),
            args: Vec::new(),
            env: BTreeMap::new(),
            workdir: default_workdir(),
            vcpus: default_vcpus(),
            memory: default_mem(),
            kernel: files.kernel.clone(),
            init: files.init.clone(),
            extra_files: BTreeMap::new(),
            net: None,
            nics: Vec::new(),
            consoles: Vec::new(),
            guest_metadata: None,
            vmm: default_vmm(),
            kernel_extra_cmdline: String::new(),
        }
    }

    /// The key becomes a cpio name verbatim. A NUL ends it early and
    /// `..` or `.` moves the file, so the guest would not get the path
    /// the manifest names.
    ///
    /// Mutation this kills: checking only the leading slash.
    #[test]
    fn validate_rejects_an_extra_files_key_a_cpio_name_cannot_carry() {
        let files = TestFiles::new();
        for key in ["", "/abs", "a\0b", "../x", "a/../b", "./x", "a/."] {
            let mut m = manifest(&files);
            m.extra_files.insert(key.to_string(), files.bin.clone());
            let err = m
                .validate()
                .expect_err(&format!("key {key:?} must be refused"));
            assert!(
                err.to_string().contains("extra_files key"),
                "{key:?}: {err}"
            );
        }
    }

    #[test]
    fn validate_accepts_a_nested_extra_files_key() {
        let files = TestFiles::new();
        let mut m = manifest(&files);
        m.extra_files
            .insert("lib/x86_64/libc.so".to_string(), files.bin.clone());

        m.validate().expect("a nested relative key is fine");
    }

    #[test]
    fn consoles_default_to_indexed_socket_and_device() {
        let files = TestFiles::new();
        let mut m = manifest(&files);
        m.consoles = vec![ConsoleConfig::default(), ConsoleConfig::default()];
        m.validate().expect("two default consoles validate");

        let resolved = m.resolved_consoles(Path::new("/run/fhrun"));

        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].index, 0);
        assert_eq!(
            resolved[0].socket_path,
            PathBuf::from("/run/fhrun/console0.sock")
        );
        assert_eq!(resolved[0].guest_device, "/dev/hvc0");
        assert_eq!(
            resolved[1].socket_path,
            PathBuf::from("/run/fhrun/console1.sock")
        );
        assert_eq!(resolved[1].guest_device, "/dev/hvc1");
    }

    #[test]
    fn explicit_console_fields_are_preserved() {
        let files = TestFiles::new();
        // The parent of an explicit socket path must already exist, so
        // this uses the per-test directory rather than a fixed path.
        let socket = files.dir.join("control.sock");
        let mut m = manifest(&files);
        m.consoles = vec![ConsoleConfig {
            socket_path: Some(socket.clone()),
            guest_device: Some("/dev/hvc3".to_string()),
            role: Some("control".to_string()),
        }];
        m.validate().expect("explicit console validates");

        let resolved = m.resolved_consoles(Path::new("/run/fhrun"));

        assert_eq!(resolved[0].socket_path, socket);
        assert_eq!(resolved[0].guest_device, "/dev/hvc3");
        assert_eq!(resolved[0].role.as_deref(), Some("control"));
    }

    #[test]
    fn validate_rejects_socket_path_under_a_missing_directory() {
        let files = TestFiles::new();
        let mut m = manifest(&files);
        m.consoles = vec![ConsoleConfig {
            socket_path: Some(files.dir.join("absent").join("c.sock")),
            guest_device: None,
            role: None,
        }];

        let err = m.validate().expect_err("missing parent is rejected");

        assert!(
            err.to_string().contains("parent must be a directory"),
            "{err}"
        );
    }

    #[test]
    fn validate_rejects_duplicate_guest_device() {
        let files = TestFiles::new();
        let mut m = manifest(&files);
        m.consoles = vec![
            ConsoleConfig {
                socket_path: None,
                guest_device: Some("/dev/hvc0".to_string()),
                role: None,
            },
            ConsoleConfig {
                socket_path: None,
                guest_device: Some("/dev/hvc0".to_string()),
                role: None,
            },
        ];

        let err = m
            .validate()
            .expect_err("duplicate guest_device is rejected");

        assert!(
            err.to_string().contains("duplicate console guest_device"),
            "{err}"
        );
    }

    #[test]
    fn validate_rejects_more_than_two_consoles() {
        let files = TestFiles::new();
        let mut m = manifest(&files);
        m.consoles = vec![
            ConsoleConfig::default(),
            ConsoleConfig::default(),
            ConsoleConfig::default(),
        ];

        let err = m.validate().expect_err("three consoles is rejected");

        assert!(err.to_string().contains("too many consoles"), "{err}");
    }

    #[test]
    fn guest_spec_carries_consoles_and_metadata() {
        let files = TestFiles::new();
        let mut m = manifest(&files);
        m.consoles = vec![ConsoleConfig {
            socket_path: Some(files.dir.join("x.sock")),
            guest_device: None,
            role: Some("control".to_string()),
        }];
        m.guest_metadata = Some(serde_json::json!({"tenant": "acme"}));

        let spec = m.to_guest_spec("/app");

        assert_eq!(spec.bin, "/app");
        assert_eq!(spec.consoles.len(), 1);
        assert_eq!(spec.consoles[0].guest_device, "/dev/hvc0");
        assert_eq!(spec.consoles[0].role.as_deref(), Some("control"));
        assert_eq!(spec.metadata.as_ref().expect("metadata")["tenant"], "acme");
    }

    #[test]
    fn guest_spec_carries_no_host_only_fields() {
        let files = TestFiles::new();
        let mut m = manifest(&files);
        m.consoles = vec![ConsoleConfig {
            socket_path: Some(files.dir.join("x.sock")),
            guest_device: None,
            role: None,
        }];

        let json = serde_json::to_value(m.to_guest_spec("/app"))
            .expect("serialize guest spec");

        assert!(json.get("dataplane").is_none(), "{json}");
        assert!(json["consoles"][0].get("socket_path").is_none(), "{json}");
    }

    #[test]
    fn guest_spec_json_matches_init_contract() {
        let files = TestFiles::new();
        let mut m = manifest(&files);
        m.nics = vec![NetConfig {
            vnic: "vnic0".to_string(),
            mac: "02:00:00:00:00:01".to_string(),
            ip: "10.0.0.5/24".to_string(),
            gateway: Some("10.0.0.1".to_string()),
            role: None,
        }];
        m.consoles = vec![ConsoleConfig {
            socket_path: Some(files.dir.join("x.sock")),
            guest_device: None,
            role: Some("control".to_string()),
        }];
        m.guest_metadata = Some(serde_json::json!({"tenant": "acme"}));

        let v = serde_json::to_value(m.to_guest_spec("/app"))
            .expect("serialize guest spec");

        // The exact keys tools/fhrun-init/src/spec.rs declares. That
        // binary builds out of its own workspace, so a rename here
        // cannot fail at compile time there.
        assert_eq!(v["bin"], "/app");
        assert!(v["args"].is_array(), "{v}");
        assert!(v["env"].is_object(), "{v}");
        assert_eq!(v["workdir"], "/");
        assert_eq!(v["nics"][0]["vnic"], "vnic0");
        assert_eq!(v["nics"][0]["ip"], "10.0.0.5/24");
        assert_eq!(v["nics"][0]["gateway"], "10.0.0.1");
        assert_eq!(v["consoles"][0]["guest_device"], "/dev/hvc0");
        assert_eq!(v["consoles"][0]["role"], "control");
        // The host socket path must never reach the guest.
        assert!(v["consoles"][0].get("socket_path").is_none(), "{v}");
        // Opaque, and undeclared in fhrun-init, so serde skips it there.
        assert_eq!(v["metadata"]["tenant"], "acme");
    }

    #[test]
    fn vmm_field_accepts_legacy_firehyve_key() {
        let m: Manifest = serde_json::from_str(
            r#"{"name":"x","bin":"/b","kernel":"/k","init":"/i","firehyve":"/opt/firehyve"}"#,
        )
        .expect("parse legacy manifest");

        assert_eq!(m.vmm, PathBuf::from("/opt/firehyve"));
    }

    #[test]
    fn vmm_field_defaults_to_bare_firehyve() {
        let m: Manifest = serde_json::from_str(
            r#"{"name":"x","bin":"/b","kernel":"/k","init":"/i"}"#,
        )
        .expect("parse minimal manifest");

        assert_eq!(m.vmm, PathBuf::from("firehyve"));
        assert!(m.consoles.is_empty());
    }
}
