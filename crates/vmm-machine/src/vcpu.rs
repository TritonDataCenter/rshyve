// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! vCPU activation and thread spawn.

use std::io;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use slog::{error, info, warn, Logger};
use vmm_core::hdl::SuspendHow;
use vmm_core::machine::Machine;
use vmm_core::metrics::VcpuMetrics;
use vmm_core::msr::MsrHandler;
use vmm_core::vcpu::Vcpu;

use crate::signal::SUSPEND_SOURCE_VMM;
use crate::teardown::VcpuThreads;
use crate::vcpu_tasks::{self, VcpuEvent, VcpuThreadCtx};

/// The boot-strap processor. Every other vCPU is an application
/// processor and waits for a SIPI from this one.
const BSP_ID: i32 = 0;

/// How the boot-strap processor is entered.
pub enum BspEntry<'a> {
    /// Firmware reset vector, that is UEFI. Resets the BSP only.
    Firmware,
    /// The caller programs the BSP itself: direct boot, PVH.
    Custom(&'a dyn Fn(&Vcpu) -> anyhow::Result<()>),
}

/// Activate every vCPU, then program the BSP for the chosen entry.
pub fn activate_vcpus(
    machine: &Machine,
    vmexit_on_hlt: bool,
    bsp: BspEntry<'_>,
    log: &Logger,
) -> anyhow::Result<()> {
    activate_all_vcpus(machine, vmexit_on_hlt)?;

    match bsp {
        BspEntry::Firmware => activate_uefi_bsp_only(machine, log)?,
        BspEntry::Custom(setup) => {
            if let Some(bsp) = machine.vcpus().first() {
                setup(bsp).context("failed to set up BSP")?;
            }
            info!(log, "BSP configured by caller");
        }
    }

    Ok(())
}

/// Activate every vCPU and set the per-vCPU capabilities.
///
/// This touches no reset state and no run state. Keeping activation
/// apart from reset is what makes the AP asymmetry below auditable.
fn activate_all_vcpus(
    machine: &Machine,
    vmexit_on_hlt: bool,
) -> anyhow::Result<()> {
    for vcpu in machine.vcpus() {
        activate_one(vcpu, vmexit_on_hlt)?;
    }
    Ok(())
}

/// Bring one vCPU online: activate it, then set its capabilities.
///
/// The bulk boot path is this, in a loop. A vCPU added while the guest
/// runs sets the capability FIRST and activates last, because
/// `VM_ACTIVATE_CPU` has no inverse. See [`crate::vcpus::VcpuRegistry`].
pub fn activate_one(vcpu: &Vcpu, vmexit_on_hlt: bool) -> anyhow::Result<()> {
    vcpu.activate()
        .with_context(|| format!("failed to activate vCPU {}", vcpu.id()))?;
    set_halt_exit(vcpu, vmexit_on_hlt)
}

/// Set `VM_CAP_HALT_EXIT` on one vCPU, or leave it alone.
///
/// `vm_set_capability` in `vmm.c` does not ask whether the vCPU is
/// active, so a hot-add can run this before the activation it cannot
/// undo.
pub fn set_halt_exit(vcpu: &Vcpu, vmexit_on_hlt: bool) -> anyhow::Result<()> {
    if !vmexit_on_hlt {
        return Ok(());
    }
    vcpu.hdl().set_halt_exit(vcpu.id(), true).with_context(|| {
        format!("failed to set HALT_EXIT on vCPU {}", vcpu.id())
    })
}

/// Which vCPUs the UEFI boot path resets.
///
/// Only the BSP, whatever the CPU count. `Vcpu::setup_bsp` calls
/// `reboot_state()`, and that on an AP clears the state the guest's
/// INIT/SIPI bring-up depends on, which costs hundreds of seconds of
/// SMP boot time. Do not widen this list.
pub fn uefi_reset_targets(num_cpus: u32) -> Vec<i32> {
    // A vCPU id is an i32 in the bhyve API. Ids ascend, so stopping at
    // the first one that does not fit keeps the list total.
    (0..num_cpus)
        .map_while(|id| i32::try_from(id).ok())
        .filter(|id| *id == BSP_ID)
        .collect()
}

/// Reset the BSP for firmware entry, and only the BSP.
pub fn activate_uefi_bsp_only(
    machine: &Machine,
    log: &Logger,
) -> anyhow::Result<()> {
    for id in uefi_reset_targets(machine.num_cpus()) {
        let index = usize::try_from(id)
            .map_err(|_| anyhow!("reset target vCPU {id} is negative"))?;
        let vcpu = machine
            .vcpus()
            .get(index)
            .ok_or_else(|| anyhow!("reset target vCPU {id} does not exist"))?;
        vcpu.setup_bsp()
            .with_context(|| format!("failed to set up BSP vCPU {id}"))?;
    }

    check_aps_awaiting_sipi(machine, log);
    Ok(())
}

/// Post-condition for [`activate_uefi_bsp_only`]: every AP still waits
/// for SIPI, that is `VRS_RUN` is clear.
///
/// This warns and does not fail. No bhyve API version has been checked
/// for how it reports AP run state at this point, so a hard error could
/// break a UEFI boot that works today. Make it an error once a live
/// host confirms the state.
pub fn check_aps_awaiting_sipi(machine: &Machine, log: &Logger) {
    for vcpu in machine.vcpus() {
        if vcpu.id() == BSP_ID {
            continue;
        }
        match vcpu.get_run_state() {
            Ok((state, _sipi_vector)) if state & bhyve_api::VRS_RUN != 0 => {
                warn!(log, "AP is runnable before SIPI";
                    "vcpu" => vcpu.id(),
                    "run_state" => format!("{state:#x}"),
                );
            }
            Ok(_) => {}
            Err(error) => {
                warn!(log, "failed to read AP run state";
                    "vcpu" => vcpu.id(),
                    "error" => %error,
                );
            }
        }
    }
}

/// The running vCPU set, plus everything needed to grow it.
///
/// A binary that wants CPU hot-add hands `sender`, `threads` and `ctx`
/// to a [`crate::vcpus::VcpuRegistry`].
pub struct VcpuFleet {
    /// The event channel the run loop reads.
    pub events: mpsc::Receiver<VcpuEvent>,
    /// A sender for a thread started later.
    ///
    /// Holding this keeps the channel open, so an event loop over a
    /// fleet cannot use the channel closing to mean "every thread has
    /// gone". `VcpuSet::Roster` asks the roster instead.
    pub sender: mpsc::Sender<VcpuEvent>,
    /// The join handles, which the registry adds to.
    pub threads: Arc<VcpuThreads>,
    /// Per-vCPU metrics for the boot set, in id order.
    pub metrics: Vec<Arc<VcpuMetrics>>,
    /// What a thread started later needs.
    pub ctx: VcpuThreadCtx,
}

/// How long an abandoned boot set gets to leave the kernel.
const BOOT_ABORT_JOIN: Duration = Duration::from_secs(5);

/// Spawn the boot vCPU threads and keep the parts a later vCPU needs.
///
/// `msr_handler` is a trait object so this crate does not name the crate
/// that supplies the synthetic MSRs. Only rshyve has one.
///
/// A refused thread fails the boot rather than aborting the process. The
/// boot vCPUs are activated before this call, so the ones already
/// started are running guest code: the halt below is what gets them out
/// of the kernel before the caller unwinds past the machine.
pub fn spawn_vcpu_fleet(
    machine: &Machine,
    num_cpus: u32,
    api_version: u32,
    msr_handler: Option<Arc<dyn MsrHandler>>,
    log: &Logger,
) -> io::Result<VcpuFleet> {
    info!(log, "starting vCPU threads");

    let metrics: Vec<Arc<VcpuMetrics>> = (0..num_cpus)
        .map(|_| Arc::new(VcpuMetrics::new()))
        .collect();

    let ctx = VcpuThreadCtx::from_machine(machine, api_version, msr_handler);
    let (sender, events) = mpsc::channel();
    let threads = VcpuThreads::new();

    let started = match vcpu_tasks::spawn_boot_threads(
        &ctx,
        machine.vcpus(),
        &metrics,
        &sender,
        log,
    ) {
        Ok(started) => started,
        Err(refused) => {
            error!(log, "a boot vCPU thread was refused; stopping the \
                 partly started boot set";
                "vcpu" => refused.vcpu,
                "started" => refused.started.len(),
                "error" => %refused.source);
            if let Err(e) =
                machine.hdl().suspend(SuspendHow::Halt, SUSPEND_SOURCE_VMM)
            {
                error!(log, "the partly started boot set was not halted";
                    "error" => %e);
            }
            if !vmm_core::thread::join_bounded(refused.started, BOOT_ABORT_JOIN)
            {
                error!(log, "a boot vCPU thread did not leave the kernel");
            }
            return Err(refused.source);
        }
    };

    for handle in started {
        // A roster is only closed by teardown, and teardown cannot run
        // before the threads it joins were made.
        threads
            .push(handle)
            .expect("a fresh roster accepts every boot thread");
    }

    Ok(VcpuFleet {
        events,
        sender,
        threads,
        metrics,
        ctx,
    })
}

#[cfg(test)]
mod tests {
    use super::uefi_reset_targets;

    #[test]
    fn uefi_reset_targets_selects_the_bsp_and_no_ap() {
        // Resetting an AP clears the state the guest's INIT/SIPI
        // bring-up depends on, which costs hundreds of seconds of SMP
        // boot time.
        for num_cpus in [1u32, 2, 8, 64] {
            let targets = uefi_reset_targets(num_cpus);
            assert_eq!(targets, vec![0], "only the BSP is reset");

            let untouched: Vec<i32> = (0..num_cpus as i32)
                .filter(|id| !targets.contains(id))
                .collect();
            assert_eq!(
                untouched,
                (1..num_cpus as i32).collect::<Vec<i32>>(),
                "every AP stays in INIT, waiting for SIPI",
            );
        }
    }

    #[test]
    fn uefi_reset_targets_derives_from_the_cpu_count() {
        // A target list that ignores its argument can name a vCPU that
        // does not exist, and it cannot describe the AP complement.
        assert!(uefi_reset_targets(0).is_empty());
    }
}
