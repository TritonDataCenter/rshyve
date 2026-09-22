// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Device-agnostic VM construction.
//!
//! Everything here is plumbing: no bootrom, no varstore, no vTPM, no
//! CPUID. Each binary keeps that in its own `create_machine` tail.
//! `HyperV::new` needs `machine.physmap()` and then contributes CPUID
//! leaves, so CPUID stays outside this seam.

use anyhow::Context;
use slog::{info, Logger};

use vmm_core::hdl::CreateOpts;
use vmm_core::machine::{Builder, Machine, MachineSetup};

/// Everything `build_machine` needs that is not derivable from the VM.
pub struct MachineOpts<'a> {
    pub vm_name: &'a str,
    pub num_cpus: u32,
    pub mem_size: usize,
    pub create_opts: CreateOpts,
    /// Startup line, such as "rshyve starting". Each binary names
    /// itself, so shared plumbing does not have to know which one it is
    /// building for.
    pub banner: &'a str,
    /// Version banner. The commit macro reads a variable that only the
    /// binary's build.rs sets, so the caller resolves it and passes the
    /// result in.
    pub version: &'a str,
}

/// The device-agnostic core of a created VM.
pub struct MachineBuild {
    pub machine: Machine,
    pub api_version: u32,
}

/// Pre-finalize hook for a caller with nothing to place in guest memory
/// before the VM runs.
pub fn no_pre_finalize(_setup: &mut MachineSetup) -> anyhow::Result<()> {
    Ok(())
}

/// Build and finalize the VM, then apply CPU topology.
///
/// `pre_finalize` runs while `MachineSetup` still exists. That window is
/// the only mutable `PhysMap` access, so anything written into guest
/// memory before the VM runs must be written there.
pub fn build_machine(
    opts: &MachineOpts<'_>,
    pre_finalize: &mut dyn FnMut(&mut MachineSetup) -> anyhow::Result<()>,
    log: &Logger,
) -> anyhow::Result<MachineBuild> {
    info!(log, "{}", opts.banner; "version" => opts.version);
    info!(log, "creating VM";
        "name" => opts.vm_name,
        "cpus" => opts.num_cpus,
        "memory_mb" => opts.mem_size / (1024 * 1024),
        "track_dirty" => opts.create_opts.track_dirty,
    );

    let mut setup = Builder::new(opts.vm_name)
        .opts(opts.create_opts.clone())
        .cpus(opts.num_cpus)
        .memory(opts.mem_size)
        .build()
        .context("failed to create VM")?;

    pre_finalize(&mut setup)?;

    let machine = setup.finalize();

    let api_version = match machine.hdl().api_version() {
        Ok(ver) => {
            info!(log, "VM ready";
                "api_version" => ver,
                "total_memory" => machine.total_mapped_memory(),
                "vcpus" => machine.num_cpus(),
            );
            ver
        }
        Err(e) => {
            anyhow::bail!("failed to query bhyve API version: {}", e);
        }
    };

    // The kernel needs the topology for its LAPIC and timer setup.
    // maxcpus is 0 because the kernel ignores it and uses VM_MAXCPU.
    machine
        .hdl()
        .set_topology(1, opts.num_cpus as u16, 1, 0)
        .context("failed to set CPU topology")?;

    Ok(MachineBuild {
        machine,
        api_version,
    })
}

/// Probe the host TSC frequency in Hz from CPUID leaf 0x16, falling
/// back to leaf 0x15 (TSC/core ratio).
///
/// Returns `None` on non-x86_64 builds or when the host does not
/// expose the frequency CPUID leaves.
pub fn host_tsc_frequency_hz() -> Option<u64> {
    #[cfg(target_arch = "x86_64")]
    {
        let leaf16 = std::arch::x86_64::__cpuid(0);
        if leaf16.eax >= 0x16 {
            let r = std::arch::x86_64::__cpuid(0x16);
            // EAX = base frequency in MHz.
            if r.eax != 0 {
                return Some(r.eax as u64 * 1_000_000);
            }
        }
        if leaf16.eax >= 0x15 {
            // Leaf 0x15: EBX/EAX = TSC/core ratio, ECX = nominal core
            // crystal Hz. Skip if any field is zero.
            let r = std::arch::x86_64::__cpuid(0x15);
            if r.eax != 0 && r.ebx != 0 && r.ecx != 0 {
                return Some(r.ecx as u64 * r.ebx as u64 / r.eax as u64);
            }
        }
        None
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use vmm_core::hdl::CreateOpts;
    use vmm_core::machine::MachineSetup;

    use super::{no_pre_finalize, MachineOpts};

    #[test]
    fn the_caller_resolves_the_version() {
        let version = format!("{}-{}", "0.1.0", "deadbee");
        let opts = MachineOpts {
            vm_name: "seam-test",
            num_cpus: 2,
            mem_size: 512 * 1024 * 1024,
            create_opts: CreateOpts::default(),
            banner: "seam-test starting",
            version: &version,
        };
        assert_eq!(opts.version, "0.1.0-deadbee");
    }

    #[test]
    fn pre_finalize_accepts_a_capturing_closure_and_the_no_op() {
        // rshyve's callback writes the bootrom's varfile into state it
        // captures, so the seam must take FnMut, not fn.
        let mut captured: Option<u64> = None;
        {
            let mut cb = |_setup: &mut MachineSetup| -> anyhow::Result<()> {
                captured = Some(0xF000_0000);
                Ok(())
            };
            let _: &mut dyn FnMut(&mut MachineSetup) -> anyhow::Result<()> =
                &mut cb;
        }
        assert!(captured.is_none(), "callback must not run without a VM");

        let mut noop = no_pre_finalize;
        let _: &mut dyn FnMut(&mut MachineSetup) -> anyhow::Result<()> =
            &mut noop;
    }
}
