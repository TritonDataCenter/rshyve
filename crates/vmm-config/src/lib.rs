// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CLI argument parsing and configuration for the VMM.
//!
//! Accepts the bhyve flags the zone brand's `boot.c` builds (`-H -U -B
//! -c -m -A -S -s -l -o`) and adds the migration and direct-boot flags.
//!
//! Three C bhyve flags are deliberately absent, so clap refuses them
//! instead of this VMM ignoring them: `-D` (destroy on power-off) would
//! change the lifecycle the zone brand relies on, and `-k` and
//! `--json-config` name configuration files nothing here reads, so a
//! VM would boot with defaults and no word about it. An operator who
//! puts one in `bhyve_extra_opts` gets an error, not a surprise.

use std::path::PathBuf;

use clap::Parser;

/// Most vCPUs a bhyve VM can have, matching the kernel's VM_MAXCPU.
/// Repeated here because this crate does not link bhyve_api. vmm_machine
/// checks at compile time that the two agree.
pub const MAX_VCPUS: u32 = 64;

/// Longest kernel command line this VMM accepts.
///
/// It mirrors `vmm_boot::direct::CMDLINE_MAX`, which is the bound the
/// loader applies when it writes the string into guest memory. It is
/// repeated here so a bad value fails when the arguments are parsed,
/// before the VM exists.
pub const CMDLINE_MAX: usize = 4096;

/// The kernel command line a direct boot gets when the operator gives
/// none. Both binaries serve the console on COM1.
pub const DEFAULT_CMDLINE: &str = "console=ttyS0 earlyprintk=serial";

/// Prefix of the `-o` keys this tree owns.
///
/// bhyve's `-o` is a free key space and vmadm passes keys nothing here
/// reads, so an unknown key is ignored. A key under this prefix belongs
/// to this VMM, so a typo in one is an error: a dropped `hotplug.maxmen`
/// would give the operator a VM with no window and no reason why. See
/// [`Cli::check_hotplug_options`].
const HOTPLUG_PREFIX: &str = "hotplug.";

/// `-o hotplug.maxmem=SIZE`: the guest address window that hot-added
/// memory is placed in.
pub const OPT_MAXMEM: &str = "hotplug.maxmem";

/// `-o hotplug.memslot=SIZE`: the size of one hot-add memory slot.
pub const OPT_MEMSLOT: &str = "hotplug.memslot";

/// The key C bhyve reads to draw guest memory from the VMM reservoir.
///
/// C bhyve's `bhyverun.c` turns it into `VCF_RESERVOIR_MEM`. The control
/// plane uses this key, because the bhyve brand splices
/// `bhyve_extra_opts` onto the argv and no zonecfg attribute reaches
/// this VMM.
pub const OPT_USE_RESERVOIR: &str = "memory.use_reservoir";

/// Decode the base64 form of the kernel command line.
///
/// The alphabet is the standard one with padding, which is what
/// `attr_base64` in tritond-vmadm produces. A different alphabet is
/// refused rather than guessed at.
fn decode_cmdline_base64(encoded: &str) -> anyhow::Result<String> {
    use base64::Engine as _;

    // The decoded length is bounded below, but bound the input first so
    // an oversized argument is refused before it is allocated.
    anyhow::ensure!(
        encoded.len() <= CMDLINE_MAX * 2,
        "--cmdline-base64 is too long ({} bytes)",
        encoded.len(),
    );
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encoded.as_bytes())
        .map_err(|e| anyhow::anyhow!("--cmdline-base64 is not base64: {e}"))?;
    String::from_utf8(raw).map_err(|e| {
        anyhow::anyhow!("--cmdline-base64 does not decode to UTF-8: {e}")
    })
}

/// Refuse a kernel command line this VMM must not give to a guest.
///
/// The string is copied into guest memory and terminated with a NUL, so
/// an embedded NUL would silently truncate it. Control bytes are
/// refused for the same reason: the guest kernel splits its command
/// line on whitespace and gives no meaning to the other control codes,
/// so a control byte in this string is always an error at the source.
fn check_cmdline(cmdline: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        cmdline.len() < CMDLINE_MAX,
        "kernel command line is too long ({} bytes, max {})",
        cmdline.len(),
        CMDLINE_MAX - 1,
    );
    if let Some((idx, bad)) = cmdline
        .char_indices()
        .find(|(_, c)| *c == '\u{7f}' || c.is_control())
    {
        anyhow::bail!(
            "kernel command line has a control byte {:#04x} at offset {}",
            bad as u32,
            idx,
        );
    }
    Ok(())
}

/// Parse a memory size string into bytes.
///
/// Accepts suffixes: K/k (KiB), M/m (MiB), G/g (GiB), T/t (TiB).
/// A bare number without suffix is treated as mebibytes (matching
/// bhyve's convention where `-m 1024` means 1 GiB).
pub fn parse_mem_size(s: &str) -> anyhow::Result<usize> {
    let s = s.trim();
    anyhow::ensure!(!s.is_empty(), "empty memory size");

    let (num_str, multiplier) = match s.as_bytes().last() {
        Some(b'k' | b'K') => (&s[..s.len() - 1], 1024usize),
        Some(b'm' | b'M') => (&s[..s.len() - 1], 1024 * 1024),
        Some(b'g' | b'G') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        Some(b't' | b'T') => (&s[..s.len() - 1], 1024 * 1024 * 1024 * 1024),
        _ => (s, 1024 * 1024),
    };

    let num: usize = num_str
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid memory size '{s}': {e}"))?;

    num.checked_mul(multiplier)
        .ok_or_else(|| anyhow::anyhow!("memory size '{s}' overflows"))
}

/// Refuse a vCPU count the kernel cannot give.
///
/// `VM_MAXCPU` is the ceiling. Without this the count reaches the
/// kernel and comes back as a bare EINVAL from VM_SET_TOPOLOGY.
fn check_cpu_count(n: u32) -> anyhow::Result<u32> {
    anyhow::ensure!(n > 0, "CPU count must be > 0");
    anyhow::ensure!(
        n <= MAX_VCPUS,
        "CPU count {n} is above the kernel limit of {MAX_VCPUS}",
    );
    Ok(n)
}

/// Split a `-c key=value,...` specification.
///
/// `sockets`, `cores` and `threads` are refused rather than dropped:
/// this VMM programs one socket with one thread per core, so accepting
/// them would give the guest a topology the operator did not ask for.
fn cpu_spec_keys(spec: &str) -> anyhow::Result<Vec<(&str, &str)>> {
    let mut pairs = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        let Some((key, val)) = part.split_once('=') else {
            anyhow::bail!("-c '{part}' is not key=value");
        };
        match key {
            "cpus" | "maxcpus" => pairs.push((key, val)),
            "sockets" | "cores" | "threads" => anyhow::bail!(
                "-c {key}= is not implemented; this VMM programs one \
                 socket with one thread per core, so use cpus=N"
            ),
            _ => anyhow::bail!("-c has no '{key}' key"),
        }
    }
    Ok(pairs)
}

/// A Rust bhyve VMM with live migration support.
#[derive(Parser, Debug)]
#[command(about)]
pub struct Cli {
    /// CPU count: N, or cpus=N[,maxcpus=M]. maxcpus sets the hotplug
    /// ceiling. sockets, cores and threads are refused.
    #[arg(short = 'c')]
    pub cpus: Option<String>,

    /// Memory size: bare number = MiB (bhyve compat), or use suffix
    /// K/M/G/T. Examples: "256M", "2G", "1024" (= 1 GiB)
    #[arg(short = 'm')]
    pub memory: Option<String>,

    /// PCI device: slot,driver[,config...]  (repeatable)
    #[arg(short = 's', action = clap::ArgAction::Append)]
    pub pci_slot: Vec<String>,

    /// LPC device: device,config  (repeatable)
    #[arg(short = 'l', action = clap::ArgAction::Append)]
    pub lpc: Vec<String>,

    /// VM UUID
    #[arg(short = 'U')]
    pub uuid: Option<String>,

    /// SMBIOS info: type,key=value,...
    #[arg(short = 'B')]
    pub smbios: Option<String>,

    /// Config option: key=value  (repeatable). Keys this VMM reads:
    /// memory.use_reservoir=true, the same as -S. hotplug.maxmem=SIZE,
    /// the memory window an operator can hot-add into, which needs
    /// --hotplug and -S. hotplug.memslot=SIZE, the size of one hot-add
    /// slot (default 128M, at most 8 slots). Other keys are ignored.
    #[arg(short = 'o', action = clap::ArgAction::Append)]
    pub config_option: Vec<String>,

    /// VM exit on HLT instruction
    #[arg(short = 'H')]
    pub vmexit_on_hlt: bool,

    /// Draw guest memory from the VMM reservoir.
    ///
    /// C bhyve's `-S` means `memory.wired`, which this VMM does not
    /// implement. See [`Cli::use_reservoir`].
    #[arg(short = 'S')]
    pub wire_memory: bool,

    /// Create ACPI tables. The zone brand passes this flag. The tables
    /// are always built, so the flag changes nothing.
    #[arg(short = 'A')]
    pub acpi: bool,

    /// Control socket path for migration/management
    #[arg(long)]
    pub control_socket: Option<PathBuf>,

    /// Turn off dirty page tracking, for benchmarking or a VM that will
    /// never migrate. Tracking is on otherwise.
    #[arg(long = "no-track-dirty", action = clap::ArgAction::SetTrue)]
    pub no_track_dirty: bool,

    /// Direct boot: kernel image, a Linux bzImage or a PVH ELF.
    /// The protocol is read from the file. No UEFI firmware runs.
    #[arg(long)]
    pub kernel: Option<PathBuf>,

    /// Direct boot: kernel command line
    #[arg(long)]
    pub cmdline: Option<String>,

    /// Direct boot: kernel command line, base64 (standard alphabet,
    /// padded).
    ///
    /// The bhyve zone brand carries extra arguments in the
    /// `bhyve_extra_opts` zonecfg attr and splits that attr on space and
    /// tab (`boot.c`). A command line with spaces therefore cannot go
    /// through `--cmdline`. `attr_base64` in tritond-vmadm produces this
    /// form.
    ///
    /// Use this flag or `--cmdline`, not both.
    #[arg(long = "cmdline-base64")]
    pub cmdline_base64: Option<String>,

    /// Direct boot: initrd/initramfs path
    #[arg(long)]
    pub initrd: Option<PathBuf>,

    /// Migration destination: listen for incoming migration on this address
    /// (e.g., "0.0.0.0:4567"). When set, the VM is created with RAM but
    /// does NOT start vCPUs. It waits for migration data.
    #[arg(long)]
    pub migrate_listen: Option<String>,

    /// Network config for mdata agent (JSON, e.g. from zone config).
    /// Format: '[{"ip":"10.0.0.5","netmask":"255.255.255.0","gateway":"10.0.0.1","primary":true}]'
    #[arg(long)]
    pub mdata_nics: Option<String>,

    /// DNS resolvers for mdata agent (JSON array, e.g. '["8.8.8.8"]')
    #[arg(long)]
    pub mdata_resolvers: Option<String>,

    /// SSH authorized keys for mdata agent (raw keys text)
    #[arg(long)]
    pub mdata_ssh_keys: Option<String>,

    /// Root password for mdata agent (plaintext, set via cloud-init).
    ///
    /// Every process in the zone, and the global zone, can read this
    /// with `ps` or `pargs`. Prefer --mdata-root-pw-file.
    #[arg(long)]
    pub mdata_root_pw: Option<String>,

    /// File holding the root password for the mdata agent.
    ///
    /// Read once at start, without its trailing newline. Takes
    /// precedence over --mdata-root-pw, and keeps the password out of
    /// argv.
    #[arg(long)]
    pub mdata_root_pw_file: Option<String>,

    /// CPU baseline profile for migration compatibility.
    /// Masks out CPU features above the specified level so VMs can
    /// migrate between hosts with different CPU generations.
    ///
    /// Values: "host" (no masking, default), "avx2" (mask AVX-512),
    /// "sse42" (mask AVX-512 + AVX2, maximum compatibility).
    /// Alias: "no-avx512" = "avx2".
    #[arg(long, default_value = "host")]
    pub cpu_baseline: String,

    /// Enable Microsoft Hyper-V enlightenments (Tier 1: CPUID
    /// identification, hypercall stub, reference TSC, VP_INDEX,
    /// reset and crash MSRs). Required for good Windows guest
    /// performance and stability. Inert for Linux guests.
    #[arg(long)]
    pub hyperv: bool,

    /// Guest TSC frequency in Hz, used to compute the reference-TSC
    /// page scaling factor when --hyperv is set. Defaults to the
    /// host TSC frequency probed via CPUID 0x15/0x16. Set explicitly
    /// when migrating between hosts with different TSC frequencies.
    #[arg(long)]
    pub tsc_freq_hz: Option<u64>,

    /// Enable a virtual TPM 2.0 device backed by the in-process
    /// libtpms emulator. Required for Windows 11 / Server 2022+
    /// installs and for any guest that uses BitLocker, measured
    /// boot, or attestation. Inert for guests that do not look for
    /// a TPM. Exposes a TCG CRB MMIO interface at 0xFED40000 plus
    /// the TPM2 ACPI table.
    #[arg(long)]
    pub vtpm: bool,

    /// Directory for the vTPM's persistent NV state. Defaults to
    /// `/var/db/rshyve/<vm_name>/tpm`. The directory is created if
    /// missing. Existing state is reused, so guest TPM keys survive
    /// VMM restarts.
    #[arg(long)]
    pub vtpm_state_dir: Option<PathBuf>,

    /// Publish the ACPI hotplug interface: the GPE0 block, the PCI, CPU
    /// and memory register files, and the matching AML. Off by default.
    /// Without it the ACPI tables have no hotplug AML and those I/O
    /// ports stay free.
    #[arg(long)]
    pub hotplug: bool,

    /// VM name
    pub vm_name: String,
}

impl Cli {
    /// Build the clap command presenting `name` as the program name.
    ///
    /// One derive serves two binaries, so the program name cannot be
    /// baked into the attribute.
    pub fn command_named(name: &'static str) -> clap::Command {
        use clap::CommandFactory;
        Self::command().name(name).bin_name(name)
    }

    /// Parse argv, presenting `name` in help output and error messages.
    pub fn parse_named(name: &'static str) -> Self {
        use clap::FromArgMatches;
        let matches = Self::command_named(name).get_matches();
        Self::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
    }

    /// Parse `argv` and report a bad command line instead of exiting.
    ///
    /// [`parse_named`](Self::parse_named) ends the process on a parse
    /// error, which no caller outside `main` can survive. Crates that
    /// hold a `Cli` but do not depend on clap build one through here.
    pub fn try_parse_named<I, T>(
        name: &'static str,
        argv: I,
    ) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        use clap::FromArgMatches;
        let matches = Self::command_named(name).try_get_matches_from(argv)?;
        Self::from_arg_matches(&matches)
    }

    /// Parse the CPU count from the -c argument.
    ///
    /// Returns an error if the argument is present but unparseable, or
    /// names a topology this VMM does not program.
    pub fn num_cpus(&self) -> anyhow::Result<u32> {
        let Some(spec) = &self.cpus else {
            return Ok(1);
        };
        if let Ok(n) = spec.parse::<u32>() {
            return check_cpu_count(n);
        }
        let mut cpus = None;
        for (key, val) in cpu_spec_keys(spec)? {
            if key == "cpus" {
                let n: u32 = val.parse().map_err(|e| {
                    anyhow::anyhow!("invalid cpus value '{val}': {e}")
                })?;
                cpus = Some(check_cpu_count(n)?);
            }
        }
        cpus.ok_or_else(|| {
            anyhow::anyhow!("could not parse CPU specification: '{spec}'")
        })
    }

    /// Parse the CPU ceiling from the -c argument's `maxcpus` key.
    ///
    /// Defaults to the boot CPU count, so a VM that does not ask for
    /// hotplug slots gets none. The ACPI tables size the guest's per-CPU
    /// state from this, so a value the kernel cannot honour is an error
    /// here and not a bad MADT the guest finds at boot.
    pub fn max_cpus(&self) -> anyhow::Result<u32> {
        let num_cpus = self.num_cpus()?;
        let Some(spec) = &self.cpus else {
            return Ok(num_cpus);
        };
        if spec.parse::<u32>().is_ok() {
            return Ok(num_cpus);
        }
        for (key, val) in cpu_spec_keys(spec)? {
            if key != "maxcpus" {
                continue;
            }
            let n: u32 = val.parse().map_err(|e| {
                anyhow::anyhow!("invalid maxcpus value '{val}': {e}")
            })?;
            anyhow::ensure!(
                n >= num_cpus,
                "maxcpus {n} is below the {num_cpus} boot CPUs",
            );
            anyhow::ensure!(
                n <= MAX_VCPUS,
                "maxcpus {n} is above the kernel limit of {MAX_VCPUS}",
            );
            return Ok(n);
        }
        Ok(num_cpus)
    }

    /// Resolve the kernel command line from its plain and base64 forms.
    ///
    /// The two forms are alternatives, so supplying both is an error.
    ///
    /// The result goes to the guest kernel, so it is checked here: the
    /// length is bounded, and NUL and control bytes are refused. A NUL
    /// would truncate the string the loader writes, and control bytes
    /// have no meaning to a kernel command line parser.
    pub fn kernel_cmdline(&self) -> anyhow::Result<Option<String>> {
        let resolved = match (&self.cmdline, &self.cmdline_base64) {
            (Some(_), Some(_)) => anyhow::bail!(
                "--cmdline and --cmdline-base64 are alternatives; \
                 supply one, not both"
            ),
            (Some(plain), None) => plain.clone(),
            (None, Some(encoded)) => decode_cmdline_base64(encoded)?,
            (None, None) => return Ok(None),
        };
        check_cmdline(&resolved)?;
        Ok(Some(resolved))
    }

    /// Parse the memory size from the -m argument.
    ///
    /// Returns an error if the argument is present but unparseable.
    pub fn mem_size(&self) -> anyhow::Result<usize> {
        match &self.memory {
            None => Ok(256 * 1024 * 1024),
            Some(s) => parse_mem_size(s),
        }
    }

    /// The value of one `-o key=value`. The last one given wins.
    pub fn config_opt(&self, key: &str) -> Option<&str> {
        self.config_option
            .iter()
            .rev()
            .filter_map(|opt| opt.split_once('='))
            .find(|(k, _)| k.trim() == key)
            .map(|(_, value)| value.trim())
    }

    /// Whether guest memory comes from the VMM reservoir.
    ///
    /// Two spellings, because two callers use different ones. The tools
    /// and docs in this tree pass `-S`. The control plane passes `-o
    /// memory.use_reservoir=true`, which is what C bhyve reads in
    /// `bhyverun.c`, and it passes no `-S`. If only `-S` were read, a
    /// control-plane VM would get transient memory after the agent grew
    /// and charged the reservoir for it.
    ///
    /// This VMM does not implement C bhyve's `-S`, which means
    /// `memory.wired` there (`bhyverun_machdep.c`). Here `-S` means the
    /// reservoir.
    pub fn use_reservoir(&self) -> bool {
        self.wire_memory
            || self
                .config_opt(OPT_USE_RESERVOIR)
                .is_some_and(|v| v.eq_ignore_ascii_case("true"))
    }

    /// Refuse a `hotplug.` key this tree does not read.
    ///
    /// Every other `-o` key is left alone, because bhyve's `-o` is a
    /// free key space that this VMM shares with its callers.
    pub fn check_hotplug_options(&self) -> anyhow::Result<()> {
        for opt in &self.config_option {
            let (key, has_value) = match opt.split_once('=') {
                Some((key, _)) => (key.trim(), true),
                None => (opt.trim(), false),
            };
            if !key.starts_with(HOTPLUG_PREFIX) {
                continue;
            }
            anyhow::ensure!(
                key == OPT_MAXMEM || key == OPT_MEMSLOT,
                "unknown option '-o {key}'; this VMM reads \
                 {OPT_MAXMEM} and {OPT_MEMSLOT}",
            );
            anyhow::ensure!(has_value, "'-o {key}' needs a value");
        }
        Ok(())
    }

    /// The memory hot-add window from `-o hotplug.maxmem=SIZE`.
    ///
    /// `-m` keeps the bhyve grammar, because every caller passes it and
    /// C bhyve must parse it too. The window goes in the repeatable `-o`
    /// key space instead.
    ///
    /// # Why `-S` is required
    ///
    /// A hot-add runs `VM_ALLOC_MEMSEG` on a live VM, and that ioctl
    /// takes the kernel's write lock, which freezes every vCPU until it
    /// returns. With reservoir memory, one test measured the hold at 233
    /// to 270 ms per GiB. Without it the same call also creates and
    /// zeroes every page inside the lock, which alone measured about
    /// 400 ms per GiB at VM create. That adds to a guest-visible stall,
    /// so the window is refused without reservoir memory.
    pub fn max_mem(&self) -> anyhow::Result<Option<u64>> {
        let Some(text) = self.config_opt(OPT_MAXMEM) else {
            return Ok(None);
        };
        let bytes = mem_bytes(OPT_MAXMEM, text)?;
        anyhow::ensure!(bytes > 0, "'-o {OPT_MAXMEM}' needs a size above 0");
        anyhow::ensure!(
            self.hotplug,
            "'-o {OPT_MAXMEM}' needs --hotplug: without it the guest \
             has no memory hotplug AML to answer on",
        );
        anyhow::ensure!(
            self.use_reservoir(),
            "'-o {OPT_MAXMEM}' needs reservoir memory, from -S or \
             '-o {OPT_USE_RESERVOIR}=true': without it a hot-add also \
             creates and zeroes every page while the vCPUs are frozen, \
             about 400 ms per GiB",
        );
        Ok(Some(bytes))
    }

    /// The hot-add slot size from `-o hotplug.memslot=SIZE`.
    ///
    /// `None` means the caller uses its own default. The window and the
    /// slot have to divide, which the window builder checks.
    pub fn mem_slot_size(&self) -> anyhow::Result<Option<u64>> {
        let Some(text) = self.config_opt(OPT_MEMSLOT) else {
            return Ok(None);
        };
        let bytes = mem_bytes(OPT_MEMSLOT, text)?;
        anyhow::ensure!(bytes > 0, "'-o {OPT_MEMSLOT}' needs a size above 0");
        Ok(Some(bytes))
    }
}

/// One `-o` size, as the byte count a guest address space is measured
/// in.
///
/// `u64`, not `usize`: these are guest addresses and lengths, and the
/// memory window API takes them as `u64`. Converting once here keeps
/// every caller free of a width cast.
fn mem_bytes(key: &str, text: &str) -> anyhow::Result<u64> {
    let bytes =
        parse_mem_size(text).map_err(|e| anyhow::anyhow!("-o {key}: {e}"))?;
    u64::try_from(bytes)
        .map_err(|_| anyhow::anyhow!("-o {key}: {bytes} bytes is too large"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli_from(argv: &[&str]) -> Cli {
        Cli::try_parse_named("rshyve", argv).expect("valid argv parses")
    }

    #[test]
    fn max_cpus_defaults_to_the_boot_count() {
        assert_eq!(cli_from(&["rshyve", "guest"]).max_cpus().unwrap(), 1);
        assert_eq!(
            cli_from(&["rshyve", "-c", "4", "guest"])
                .max_cpus()
                .unwrap(),
            4,
        );
    }

    #[test]
    fn a_topology_this_vmm_does_not_program_is_refused() {
        // Dropping sockets= or threads= would give the guest a topology
        // the operator did not ask for, with no error.
        for spec in [
            "cpus=4,sockets=1",
            "cpus=4,cores=2,threads=1",
            "sockets=2,cores=2",
            "threads=2",
        ] {
            let cli = cli_from(&["rshyve", "-c", spec, "guest"]);
            assert!(cli.num_cpus().is_err(), "{spec}");
            assert!(cli.max_cpus().is_err(), "{spec}");
        }
        // An unknown key is refused too, so a typo is not a silent default.
        let typo = cli_from(&["rshyve", "-c", "cpu=4", "guest"]);
        assert!(typo.num_cpus().is_err());
    }

    #[test]
    fn a_boot_cpu_count_above_the_kernel_limit_is_refused() {
        // Without this, VM_SET_TOPOLOGY answers with a bare EINVAL.
        for spec in ["65", "cpus=65", "0", "cpus=0"] {
            let cli = cli_from(&["rshyve", "-c", spec, "guest"]);
            assert!(cli.num_cpus().is_err(), "{spec}");
        }
        assert_eq!(
            cli_from(&["rshyve", "-c", "64", "guest"])
                .num_cpus()
                .unwrap(),
            MAX_VCPUS,
        );
    }

    #[test]
    fn the_flags_this_vmm_does_not_implement_are_refused() {
        // Accepting and ignoring -D, -k or --json-config would boot a VM
        // with defaults and a lifecycle the operator did not ask for.
        for flag in [
            vec!["rshyve", "-D", "guest"],
            vec!["rshyve", "-k", "/tmp/vm.conf", "guest"],
            vec!["rshyve", "--json-config", "/tmp/vm.json", "guest"],
        ] {
            assert!(Cli::try_parse_named("rshyve", &flag).is_err(), "{flag:?}");
        }
        // The flags the zone brand does pass still parse.
        let cli = cli_from(&["rshyve", "-H", "-A", "-S", "guest"]);
        assert!(cli.acpi && cli.wire_memory && cli.vmexit_on_hlt);
    }

    #[test]
    fn dirty_tracking_is_on_unless_it_is_turned_off() {
        assert!(!cli_from(&["rshyve", "guest"]).no_track_dirty);
        assert!(
            cli_from(&["rshyve", "--no-track-dirty", "guest"]).no_track_dirty
        );
        // There is no --track-dirty: tracking is on by default.
        assert!(Cli::try_parse_named(
            "rshyve",
            ["rshyve", "--track-dirty", "guest"]
        )
        .is_err());
    }

    #[test]
    fn max_cpus_reads_the_maxcpus_key() {
        let cli = cli_from(&["rshyve", "-c", "cpus=2,maxcpus=8", "guest"]);
        assert_eq!(cli.num_cpus().unwrap(), 2);
        assert_eq!(cli.max_cpus().unwrap(), 8);

        let equal = cli_from(&["rshyve", "-c", "cpus=4,maxcpus=4", "guest"]);
        assert_eq!(equal.max_cpus().unwrap(), 4);
    }

    #[test]
    fn max_cpus_rejects_unusable_ceilings() {
        // A ceiling below the boot count would leave a booted CPU with no
        // MADT slot.
        let below = cli_from(&["rshyve", "-c", "cpus=4,maxcpus=2", "guest"]);
        assert!(below.max_cpus().is_err());

        let over = cli_from(&["rshyve", "-c", "cpus=2,maxcpus=65", "guest"]);
        assert!(over.max_cpus().is_err());

        let junk = cli_from(&["rshyve", "-c", "cpus=2,maxcpus=x", "guest"]);
        assert!(junk.max_cpus().is_err());
    }

    #[test]
    fn hotplug_is_off_unless_asked_for() {
        // The FADT, the DSDT and the I/O port claims all read this one
        // flag. Defaulting it on would change the tables of every VM
        // that already runs.
        assert!(!cli_from(&["rshyve", "guest"]).hotplug);
        assert!(cli_from(&["rshyve", "--hotplug", "guest"]).hotplug);
    }

    #[test]
    fn a_config_option_is_read_by_key_with_the_last_one_winning() {
        let cli = cli_from(&[
            "rshyve",
            "-o",
            "hotplug.maxmem=2G",
            "-o",
            "hotplug.maxmem=4G",
            "guest",
        ]);

        assert_eq!(cli.config_opt(OPT_MAXMEM), Some("4G"));
        assert_eq!(cli.config_opt("hotplug.memslot"), None);
    }

    #[test]
    fn a_config_option_this_vmm_does_not_read_is_left_alone() {
        // bhyve's -o is a free key space that vmadm also writes into.
        let cli = cli_from(&["rshyve", "-o", "acpi.tables=on", "guest"]);

        cli.check_hotplug_options()
            .expect("a foreign key is not ours");
        assert_eq!(cli.config_opt("acpi.tables"), Some("on"));
    }

    #[test]
    fn no_hotplug_refusal_carries_a_run_of_spaces() {
        // A `\` line continuation eats the newline AND the indent that
        // follows it. A message rebuilt without one keeps that indent
        // and reaches the operator as a gap.
        let typo = cli_from(&["rshyve", "-o", "hotplug.maxmen=4G", "guest"]);
        let no_hotplug =
            cli_from(&["rshyve", "-S", "-o", "hotplug.maxmem=4G", "guest"]);
        let no_reservoir = cli_from(&[
            "rshyve",
            "--hotplug",
            "-o",
            "hotplug.maxmem=4G",
            "guest",
        ]);

        for message in [
            typo.check_hotplug_options().unwrap_err().to_string(),
            no_hotplug.max_mem().unwrap_err().to_string(),
            no_reservoir.max_mem().unwrap_err().to_string(),
        ] {
            assert!(!message.contains("  "), "{message}");
        }
    }

    #[test]
    fn a_misspelled_hotplug_option_is_refused() {
        // Ignoring it would give the operator a VM with no window and
        // no reason why.
        let typo = cli_from(&["rshyve", "-o", "hotplug.maxmen=4G", "guest"]);
        assert!(typo.check_hotplug_options().is_err());

        let bare = cli_from(&["rshyve", "-o", "hotplug.maxmem", "guest"]);
        assert!(bare.check_hotplug_options().is_err());

        let good = cli_from(&[
            "rshyve",
            "-o",
            "hotplug.maxmem=4G",
            "-o",
            "hotplug.memslot=256M",
            "guest",
        ]);
        good.check_hotplug_options().expect("both keys are ours");
    }

    #[test]
    fn a_vm_with_no_maxmem_gets_no_window() {
        let cli = cli_from(&["rshyve", "guest"]);
        assert_eq!(cli.max_mem().unwrap(), None);
        assert_eq!(cli.mem_slot_size().unwrap(), None);
    }

    #[test]
    fn maxmem_reads_the_shared_memory_size_grammar() {
        let cli = cli_from(&[
            "rshyve",
            "--hotplug",
            "-S",
            "-o",
            "hotplug.maxmem=4G",
            "-o",
            "hotplug.memslot=256M",
            "guest",
        ]);

        assert_eq!(cli.max_mem().unwrap(), Some(4 * 1024 * 1024 * 1024));
        assert_eq!(cli.mem_slot_size().unwrap(), Some(256 * 1024 * 1024));
    }

    #[test]
    fn maxmem_needs_the_hotplug_interface() {
        // Without --hotplug the DSDT has no memory controller, so the
        // guest could never be told about a slot.
        let cli =
            cli_from(&["rshyve", "-S", "-o", "hotplug.maxmem=4G", "guest"]);

        let error = cli.max_mem().expect_err("no --hotplug");
        assert!(error.to_string().contains("--hotplug"), "{error}");
    }

    /// The control plane never passes -S. It passes the key C bhyve
    /// reads in `bhyverun.c`, spliced onto the argv through the brand's
    /// `bhyve_extra_opts`. If only -S were read, the guest would get
    /// transient memory, about 400 ms per GiB, after the agent grew and
    /// charged the reservoir for it, with no error.
    #[test]
    fn the_reservoir_option_the_control_plane_sends_is_read() {
        let cli = cli_from(&[
            "rshyve",
            "-c",
            "1",
            "-m",
            "128M",
            "-o",
            "memory.use_reservoir=true",
            "vm",
        ]);
        assert!(!cli.wire_memory, "the control plane sends no -S");
        assert!(cli.use_reservoir(), "the reservoir request was dropped");
    }

    #[test]
    fn dash_s_still_asks_for_the_reservoir() {
        let cli = cli_from(&["rshyve", "-S", "-c", "1", "-m", "128M", "vm"]);
        assert!(cli.use_reservoir());
    }

    #[test]
    fn no_request_means_no_reservoir() {
        let cli = cli_from(&["rshyve", "-c", "1", "-m", "128M", "vm"]);
        assert!(!cli.use_reservoir());
        // A value that is not true must not be read as one.
        let off = cli_from(&[
            "rshyve",
            "-c",
            "1",
            "-m",
            "128M",
            "-o",
            "memory.use_reservoir=false",
            "vm",
        ]);
        assert!(!off.use_reservoir());
    }

    #[test]
    fn maxmem_needs_reservoir_memory() {
        // VM_ALLOC_MEMSEG runs under the kernel write lock, which
        // freezes every vCPU. Off the reservoir that call also creates
        // and zeroes every page, about 400 ms per GiB.
        let cli = cli_from(&[
            "rshyve",
            "--hotplug",
            "-o",
            "hotplug.maxmem=4G",
            "guest",
        ]);

        let error = cli.max_mem().expect_err("no -S");
        assert!(error.to_string().contains("-S"), "{error}");
    }

    #[test]
    fn a_maxmem_that_is_not_a_size_is_refused() {
        for value in ["hotplug.maxmem=x", "hotplug.maxmem=0"] {
            let cli =
                cli_from(&["rshyve", "--hotplug", "-S", "-o", value, "guest"]);
            assert!(cli.max_mem().is_err(), "{value} was accepted");
        }
        let cli = cli_from(&[
            "rshyve",
            "--hotplug",
            "-S",
            "-o",
            "hotplug.memslot=0",
            "guest",
        ]);
        assert!(cli.mem_slot_size().is_err());
    }

    #[test]
    fn the_bhyve_memory_flag_still_parses_on_its_own() {
        // Every caller passes -m, so the window uses -o and not a second
        // -m field.
        let cli = cli_from(&["rshyve", "-m", "2G", "guest"]);
        assert_eq!(cli.mem_size().unwrap(), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn parse_mem_suffixes() {
        assert_eq!(parse_mem_size("256M").unwrap(), 256 * 1024 * 1024);
        assert_eq!(parse_mem_size("2G").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_mem_size("512k").unwrap(), 512 * 1024);
        // Bare number = MiB
        assert_eq!(parse_mem_size("1024").unwrap(), 1024 * 1024 * 1024);
    }

    #[test]
    fn parse_mem_errors() {
        assert!(parse_mem_size("").is_err());
        assert!(parse_mem_size("abc").is_err());
        assert!(parse_mem_size("-1M").is_err());
    }

    #[test]
    fn parse_named_uses_the_binary_name() {
        let cli = Cli::try_parse_named(
            "firehyve",
            ["firehyve", "-c", "2", "-m", "512M", "guest"],
        )
        .expect("valid argv parses");
        assert_eq!(cli.vm_name, "guest");
        assert_eq!(cli.num_cpus().unwrap(), 2);
        assert_eq!(cli.mem_size().unwrap(), 512 * 1024 * 1024);
    }

    #[test]
    fn parse_named_reports_the_binary_name_in_errors() {
        let err = Cli::try_parse_named("firehyve", ["firehyve"])
            .expect_err("vm_name is required");
        assert!(
            err.to_string().contains("firehyve"),
            "error should name the binary, got: {err}"
        );
    }

    /// The guest command line has spaces, and the only channel the
    /// bhyve zone brand offers splits on space and tab. So the base64
    /// form has to survive a full round trip.
    ///
    /// Mutation this kills: returning the encoded string unchanged, or
    /// dropping the decode. The alphabet is pinned separately, because
    /// a realistic ASCII command line never encodes to `+` or `/`.
    #[test]
    fn the_base64_command_line_round_trips() {
        use base64::Engine as _;

        let want = "root=/dev/ram0 init=/init console=ttyS0 panic=-1";
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(want.as_bytes());
        // The value has spaces, which is the whole reason for the flag.
        assert!(want.contains(' '));
        // And the encoding has none, so the brand's split cannot break it.
        assert!(!encoded.contains(' ') && !encoded.contains('\t'));

        let cli = cli_from(&["rshyve", "--cmdline-base64", &encoded, "guest"]);
        assert_eq!(cli.kernel_cmdline().unwrap().as_deref(), Some(want));
    }

    /// `attr_base64` in tritond-vmadm produces the standard alphabet.
    /// `+` and `/` are exactly the two characters the URL-safe
    /// alphabet renames, so a value that uses both pins the decoder.
    ///
    /// Mutation this kills: decoding with the URL-safe alphabet.
    #[test]
    fn the_standard_base64_alphabet_is_the_one_decoded() {
        // base64("|>~qi?") == "fD5+cWk/", which uses both + and /.
        let cli =
            cli_from(&["rshyve", "--cmdline-base64", "fD5+cWk/", "guest"]);
        assert_eq!(cli.kernel_cmdline().unwrap().as_deref(), Some("|>~qi?"));

        // The URL-safe spelling of the same bytes must not decode, or
        // the two alphabets would both be accepted and disagree.
        let cli =
            cli_from(&["rshyve", "--cmdline-base64", "fD5-cWk_", "guest"]);
        assert!(cli.kernel_cmdline().is_err(), "URL-safe must not decode");
    }

    /// Mutation this kills: preferring one form when both are present
    /// instead of refusing.
    #[test]
    fn a_command_line_that_is_both_plain_and_encoded_is_refused() {
        let cli = cli_from(&[
            "rshyve",
            "--cmdline",
            "console=ttyS0",
            "--cmdline-base64",
            "Y29uc29sZT10dHlTMA==",
            "guest",
        ]);
        let err = cli.kernel_cmdline().expect_err("two forms");
        assert!(err.to_string().contains("--cmdline-base64"), "got: {err}");
    }

    /// This string reaches the guest kernel command line. The loader
    /// NUL-terminates it, so an embedded NUL would silently truncate
    /// the rest.
    ///
    /// Mutation this kills: dropping the control-byte scan in
    /// check_cmdline.
    #[test]
    fn a_control_byte_in_the_command_line_is_refused() {
        use base64::Engine as _;

        for raw in ["console=ttyS0\u{0}rest", "a\nb", "a\u{7f}b"] {
            let encoded = base64::engine::general_purpose::STANDARD
                .encode(raw.as_bytes());
            let cli =
                cli_from(&["rshyve", "--cmdline-base64", &encoded, "guest"]);
            let err = cli
                .kernel_cmdline()
                .expect_err("a control byte must be refused");
            assert!(
                err.to_string().contains("control byte"),
                "for {raw:?} got: {err}"
            );
        }

        // The plain form takes the same scan, so neither channel is a
        // way around it.
        let cli = cli_from(&["rshyve", "--cmdline", "a\u{7f}b", "guest"]);
        assert!(cli.kernel_cmdline().is_err(), "plain form is not scanned");
    }

    /// Mutation this kills: bounding only the encoded input, or not
    /// bounding at all.
    #[test]
    fn an_oversized_command_line_is_refused_in_both_forms() {
        use base64::Engine as _;

        let long = "a".repeat(CMDLINE_MAX);
        let cli = cli_from(&["rshyve", "--cmdline", &long, "guest"]);
        assert!(cli.kernel_cmdline().is_err(), "plain form is not bounded");

        let encoded =
            base64::engine::general_purpose::STANDARD.encode(long.as_bytes());
        let cli = cli_from(&["rshyve", "--cmdline-base64", &encoded, "guest"]);
        assert!(cli.kernel_cmdline().is_err(), "base64 form is not bounded");

        // One byte below the bound still passes, so the check is a
        // bound and not a blanket refusal.
        let ok = "a".repeat(CMDLINE_MAX - 1);
        let cli = cli_from(&["rshyve", "--cmdline", &ok, "guest"]);
        assert!(cli.kernel_cmdline().is_ok());
    }

    /// Mutation this kills: accepting any string as base64 and passing
    /// the undecoded bytes through.
    #[test]
    fn a_value_that_is_not_base64_is_refused() {
        let cli =
            cli_from(&["rshyve", "--cmdline-base64", "not base64!", "guest"]);
        assert!(cli.kernel_cmdline().is_err());
    }

    #[test]
    fn a_plain_command_line_is_unchanged_and_absence_is_none() {
        let cli = cli_from(&["rshyve", "--cmdline", "console=ttyS0", "guest"]);
        assert_eq!(
            cli.kernel_cmdline().unwrap().as_deref(),
            Some("console=ttyS0")
        );
        assert_eq!(
            cli_from(&["rshyve", "guest"]).kernel_cmdline().unwrap(),
            None
        );
    }
}
