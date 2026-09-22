// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the ACPI tables describe, before any byte is written.
//!
//! One [`AcpiConfig`] drives the MADT, the FADT and the DSDT, so a
//! TPM2 table with no DSDT node, or a GPE block with no handler, is
//! unrepresentable.

/// MMIO resources exposed by the DSDT TPM device node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TpmDevice {
    pub crb_base: u32,
    pub crb_len: u32,
}

/// ACPI-visible description of a configured vTPM. The TPM2 table and the
/// DSDT device node are both derived from this one value, so "table but no
/// node" is unrepresentable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TpmAcpi {
    pub table: Vec<u8>,
    pub device: TpmDevice,
}

/// What the generated tables must describe.
///
/// The MADT advertises `max_cpus` processor slots. Only the first
/// `num_cpus` are enabled, so a guest can size its per-CPU state once and
/// accept a hot-added CPU later without a reboot.
#[derive(Debug, Clone)]
pub struct AcpiConfig {
    /// Boot CPUs. These carry the MADT Enabled flag.
    pub num_cpus: u32,
    /// Advertised CPU slots. Never below `num_cpus`.
    pub max_cpus: u32,
    /// MMIO window of the configured vTPM, if any.
    pub tpm: Option<TpmDevice>,
    /// Publish the GPE0 block and emit the `\\_GPE` handlers.
    pub hotplug: bool,
}

/// Why an [`AcpiConfig`] cannot describe a bootable VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AcpiConfigError {
    #[error("a VM needs at least one boot CPU")]
    NoBootCpu,
    #[error("max_cpus {max_cpus} is below the {num_cpus} boot CPUs")]
    MaxBelowBoot { num_cpus: u32, max_cpus: u32 },
    #[error("max_cpus {max_cpus} is above the kernel limit of {limit}")]
    AboveKernelLimit { max_cpus: u32, limit: u32 },
}

impl AcpiConfig {
    /// Describe `num_cpus` boot CPUs in `max_cpus` slots.
    ///
    /// A count the kernel cannot honour is an error here, where the
    /// caller can still report it, and not a bad table the guest finds
    /// at boot.
    pub fn new(num_cpus: u32, max_cpus: u32) -> Result<Self, AcpiConfigError> {
        if num_cpus == 0 {
            return Err(AcpiConfigError::NoBootCpu);
        }
        if max_cpus < num_cpus {
            return Err(AcpiConfigError::MaxBelowBoot { num_cpus, max_cpus });
        }
        let limit = vmm_core::VM_MAXCPU;
        if max_cpus > limit {
            return Err(AcpiConfigError::AboveKernelLimit { max_cpus, limit });
        }
        Ok(Self {
            num_cpus,
            max_cpus,
            tpm: None,
            hotplug: false,
        })
    }

    /// Advertise exactly the boot CPUs: no spare slots.
    ///
    /// The entry points that take a bare CPU count cannot report an
    /// error, so this skips validation and reproduces their tables byte
    /// for byte.
    pub fn boot_only(num_cpus: u32) -> Self {
        Self {
            num_cpus,
            max_cpus: num_cpus,
            tpm: None,
            hotplug: false,
        }
    }

    /// Attach the vTPM MMIO window that the DSDT node describes.
    pub fn with_tpm(mut self, tpm: Option<TpmDevice>) -> Self {
        self.tpm = tpm;
        self
    }

    /// Ask for the CPU hotplug AML and GPE block.
    pub fn with_hotplug(mut self, hotplug: bool) -> Self {
        self.hotplug = hotplug;
        self
    }
}
