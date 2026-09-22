// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the machine reads from the command line.
//!
//! The one module in this crate that knows the argv shape. Every phase
//! takes a [`VmOpts`], so the ACPI tables, the PM port claims, and the
//! hotplug engines describe the same machine and none of them can read
//! a flag the others did not.

use vmm_config::Cli;

/// The command-line decisions the machine phases read, taken once.
#[derive(Debug, Clone)]
pub struct VmOpts {
    pub vm_name: String,
    pub uuid: Option<String>,
    pub num_cpus: u32,
    /// Every CPU slot the MADT describes. Equal to `num_cpus` unless
    /// the operator asked for spare slots.
    pub max_cpus: u32,
    pub mem_size: usize,
    /// Whether the guest gets the ACPI hotplug interface at all.
    pub hotplug: bool,
    /// The memory ceiling a hot-add may reach, when one was asked for.
    pub max_mem: Option<u64>,
    pub mem_slot_size: Option<u64>,
    /// The raw `-B` SMBIOS flag, parsed by the firmware phase.
    pub smbios: Option<String>,
}

impl VmOpts {
    /// Read every value once. Each refusal here is an operator mistake
    /// that must cost no VM.
    pub fn from_cli(cli: &Cli) -> anyhow::Result<Self> {
        Ok(Self {
            vm_name: cli.vm_name.clone(),
            uuid: cli.uuid.clone(),
            num_cpus: cli.num_cpus()?,
            max_cpus: cli.max_cpus()?,
            mem_size: cli.mem_size()?,
            hotplug: cli.hotplug,
            max_mem: cli.max_mem()?,
            mem_slot_size: cli.mem_slot_size()?,
            smbios: cli.smbios.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(argv: &[&str]) -> anyhow::Result<VmOpts> {
        let cli =
            Cli::try_parse_named("rshyve", argv).expect("valid argv parses");
        VmOpts::from_cli(&cli)
    }

    #[test]
    fn only_an_operator_publishes_the_hotplug_interface() {
        // Without --hotplug the FADT keeps GPE0_BLK = 0 and the GPE0,
        // PCI and CPU ports stay free for the guest.
        assert!(!opts(&["rshyve", "guest"]).expect("parses").hotplug);
        assert!(
            opts(&["rshyve", "--hotplug", "guest"])
                .expect("parses")
                .hotplug
        );
    }

    #[test]
    fn a_window_without_the_reservoir_is_refused_here() {
        // The gate lives in Cli::max_mem, and this is the one place the
        // binaries read it, so it has to carry the refusal through.
        let error = opts(&[
            "rshyve",
            "--hotplug",
            "-o",
            "hotplug.maxmem=1G",
            "-o",
            "hotplug.memslot=1G",
            "guest",
        ])
        .expect_err("no -S");
        assert!(error.to_string().contains("-S"), "{error}");
    }
}
