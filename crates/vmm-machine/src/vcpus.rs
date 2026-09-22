// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Run-time vCPU registry.
//!
//! Boot brings up a fixed set of vCPUs. This registry brings one more
//! online while the guest runs, replaying every per-vCPU step the boot
//! path took so a late CPU is not subtly different from its siblings.
//!
//! # Add only
//!
//! There is no `remove_vcpu`, by design. illumos has no
//! `vm_deactivate_cpu`: `vm_activate_cpu` only ever sets `active_cpus`,
//! and the one place that clears it is `vm_init`, which zeroes the whole
//! set on `VM_REINIT` (both in `vmm.c`). CPU hot-REMOVE is therefore not
//! possible at the kernel boundary. The guest-facing eject path in
//! `vmm_devices::hotplug::cpu` records a request, and the host can stop
//! scheduling work on the CPU, but the kernel keeps it active until the
//! VM is reinitialised.
//!
//! # Why activation is lazy
//!
//! A CPU is activated at the moment of the request, never in advance.
//! `vm_handle_hlt` in `vmm.c` declares the VM halted only when
//! `halted_cpus` equals `active_cpus`. A vCPU that is active but that
//! the guest has never onlined sits in `VRS_HALT`, never reaches the HLT
//! accounting, and so never joins `halted_cpus`. Pre-activating a spare
//! CPU would stop `VM_SUSPEND_HALT` firing for the life of the VM.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use slog::{debug, info, warn, Logger};
use vmm_core::cpuid::CpuidTable;
use vmm_core::metrics::VcpuMetrics;
use vmm_core::vcpu::{ActivateError, Vcpu};

use crate::teardown::VcpuThreads;
use crate::vcpu_tasks::{self, VcpuEvent, VcpuThreadCtx};

/// The kernel's own vCPU ceiling.
///
/// `vm_activate_cpu` refuses `vcpuid >= vm->maxcpus`, and
/// `vm_set_topology` re-pins `maxcpus` to `VM_MAXCPU` whatever the
/// caller asks for (both in `vmm.c`). The guest-visible possible-CPU
/// count therefore lives entirely in the MADT and fw_cfg, which this
/// tree owns, and this constant is only the hard kernel limit.
const KERNEL_MAX_CPUS: u32 = vmm_core::VM_MAXCPU;
// vmm_config cannot link the kernel ABI, so it carries its own copy.
const _: () = assert!(KERNEL_MAX_CPUS == vmm_config::MAX_VCPUS);
// The same for the command line: vmm_config refuses at parse time
// what the loader would refuse when it writes guest memory.
const _: () = assert!(vmm_config::CMDLINE_MAX == vmm_boot::direct::CMDLINE_MAX);

const BOOK_POISONED: &str = "vCPU registry lock poisoned";

/// Reason a vCPU could not be brought online.
///
/// Hand-written rather than derived: this crate carries no `thiserror`
/// dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VcpuError {
    /// The id names no CPU this machine describes.
    OutOfRange { id: u32, max_cpus: u32 },
    /// This registry already brought the CPU online.
    AlreadyOnline(u32),
    /// The kernel answered EBUSY.
    ///
    /// `vm_activate_cpu` uses EBUSY for three cases, in this order: the
    /// id is active already, the VM was suspended before the set, and
    /// the VM was suspended after it. Only the middle one leaves the CPU
    /// inactive, and nothing here can tell them apart, so the id is
    /// treated as spent.
    Suspended(u32),
    /// The kernel refused the activation for some other reason.
    Activate { id: u32, error: String },
    /// A per-vCPU boot step could not be replayed.
    Setup {
        id: u32,
        step: &'static str,
        error: String,
    },
    /// The vCPU thread could not be made.
    Spawn { id: u32, error: String },
    /// Teardown has taken the thread roster, so nothing can start.
    ShuttingDown(u32),
    /// The CPU is active but its thread has gone.
    ThreadGone(u32),
}

impl VcpuError {
    /// True when the kernel was left with no extra active CPU, so the
    /// registry can give the id back.
    ///
    /// An allowlist of the errors that happen BEFORE `VM_ACTIVATE_CPU`,
    /// not a list of the ones that happen after. There is no
    /// `vm_deactivate_cpu`, so an id that reached the kernel can never
    /// be taken back, and a new variant must default to "spent".
    ///
    /// [`VcpuError::Suspended`] is outside the list. `vm_activate_cpu`
    /// sets `active_cpus` and THEN answers EBUSY if the VM was suspended
    /// in the meantime, leaving the bit set. EBUSY also means the id was
    /// active already. Only the earlier suspend test returns before the
    /// set, and the caller cannot tell the three apart, so EBUSY is read
    /// as "the CPU may be active".
    pub fn left_cpu_inactive(&self) -> bool {
        matches!(
            self,
            VcpuError::OutOfRange { .. }
                | VcpuError::AlreadyOnline(_)
                | VcpuError::Setup { .. }
                | VcpuError::Spawn { .. }
                | VcpuError::ShuttingDown(_)
        )
    }
}

impl fmt::Display for VcpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VcpuError::OutOfRange { id, max_cpus } => {
                write!(f, "vCPU {id} is not one of the {max_cpus} CPU slots")
            }
            VcpuError::AlreadyOnline(id) => {
                write!(f, "vCPU {id} is already online")
            }
            VcpuError::Suspended(id) => write!(
                f,
                "vCPU {id} was refused: the VM is suspended, or the CPU \
                 is already active. The kernel may have activated it, \
                 so the id cannot be reused",
            ),
            VcpuError::Activate { id, error } => {
                write!(f, "failed to activate vCPU {id}: {error}")
            }
            VcpuError::Setup { id, step, error } => {
                write!(f, "failed to set {step} on vCPU {id}: {error}")
            }
            VcpuError::Spawn { id, error } => {
                write!(f, "failed to start the vCPU {id} thread: {error}")
            }
            VcpuError::ShuttingDown(id) => {
                write!(f, "vCPU {id} was refused: the VM is shutting down")
            }
            VcpuError::ThreadGone(id) => write!(
                f,
                "vCPU {id} is active but its thread has gone; \
                 the kernel cannot deactivate it",
            ),
        }
    }
}

impl std::error::Error for VcpuError {}

/// What a late vCPU needs to match its siblings.
pub struct VcpuSetup {
    /// Every CPU slot the MADT and fw_cfg describe. An id at or above
    /// this is refused before the kernel sees it.
    pub max_cpus: u32,
    /// The CPUs the boot path already brought online, ids 0 upward.
    pub boot_cpus: u32,
    /// Whether the boot path set `VM_CAP_HALT_EXIT`.
    pub vmexit_on_hlt: bool,
    /// The table `apply_cpuid_table` programmed at boot, or `None` for
    /// a pure host-passthrough VM that programmed nothing.
    pub cpuid: Option<CpuidTable>,
}

/// Which ids are online, and the metrics of the CPUs added since boot.
///
/// Separate from the registry so the bookkeeping can be tested with no
/// live VM: every other step of an add is an ioctl.
struct VcpuBook {
    max_cpus: u32,
    online: BTreeSet<u32>,
    metrics: BTreeMap<u32, Arc<VcpuMetrics>>,
}

impl VcpuBook {
    fn new(max_cpus: u32, boot_cpus: u32) -> Self {
        Self {
            max_cpus,
            online: (0..boot_cpus.min(max_cpus)).collect(),
            metrics: BTreeMap::new(),
        }
    }

    /// Check `id` and reserve it, returning the bhyve vCPU id.
    ///
    /// The reservation happens before any ioctl, so two concurrent
    /// adds of one id cannot both reach `VM_ACTIVATE_CPU`.
    fn claim(&mut self, id: u32) -> Result<i32, VcpuError> {
        let vcpu_id = check_id(id, self.max_cpus)?;
        if !self.online.insert(id) {
            return Err(VcpuError::AlreadyOnline(id));
        }
        Ok(vcpu_id)
    }

    fn release(&mut self, id: u32) {
        self.online.remove(&id);
        self.metrics.remove(&id);
    }

    fn record(&mut self, id: u32, metrics: Arc<VcpuMetrics>) {
        self.metrics.insert(id, metrics);
    }
}

/// Check a vCPU id against this machine and against the kernel.
fn check_id(id: u32, max_cpus: u32) -> Result<i32, VcpuError> {
    let ceiling = max_cpus.min(KERNEL_MAX_CPUS);
    if id >= ceiling {
        return Err(VcpuError::OutOfRange {
            id,
            max_cpus: ceiling,
        });
    }
    // A vCPU id is an i32 in every bhyve ioctl. The ceiling is well
    // inside i32, so this cannot fail, but no `as` cast reaches the
    // kernel from here.
    i32::try_from(id).map_err(|_| VcpuError::OutOfRange {
        id,
        max_cpus: ceiling,
    })
}

/// Brings a CPU online on a running VM.
///
/// `Machine.vcpus` is built once in `finalize` and handed out only as
/// `&[Vcpu]`, so the registry mints its own handles with
/// `Vcpu::new_for_thread`. A `Vcpu` is an id and a shared `VmmHdl`, so
/// a minted handle is the same thing the boot path holds. The registry
/// never mutates the `Machine`.
pub struct VcpuRegistry {
    setup: VcpuSetup,
    ctx: VcpuThreadCtx,
    threads: Arc<VcpuThreads>,
    events: Sender<VcpuEvent>,
    log: Logger,
    book: Mutex<VcpuBook>,
}

impl VcpuRegistry {
    pub fn new(
        setup: VcpuSetup,
        ctx: VcpuThreadCtx,
        threads: Arc<VcpuThreads>,
        events: Sender<VcpuEvent>,
        log: Logger,
    ) -> Self {
        let book = VcpuBook::new(setup.max_cpus, setup.boot_cpus);
        Self {
            setup,
            ctx,
            threads,
            events,
            log,
            book: Mutex::new(book),
        }
    }

    /// Build from the fleet [`crate::vcpu::spawn_vcpu_fleet`] returned.
    pub fn from_fleet(
        setup: VcpuSetup,
        fleet: &crate::vcpu::VcpuFleet,
        log: Logger,
    ) -> Self {
        Self::new(
            setup,
            fleet.ctx.clone(),
            Arc::clone(&fleet.threads),
            fleet.sender.clone(),
            log,
        )
    }

    /// CPU slots this machine describes, boot CPUs included.
    pub fn possible_cpus(&self) -> u32 {
        self.setup.max_cpus.min(KERNEL_MAX_CPUS)
    }

    /// How many CPUs the boot path brought online, ids 0 upward.
    ///
    /// A caller refuses a hot-add of one of these before it reaches the
    /// registry: the AML gives no boot CPU an insert event, so the
    /// guest would never hear about it.
    pub fn boot_cpus(&self) -> u32 {
        self.setup.boot_cpus
    }

    /// The ids that are online, in ascending order.
    pub fn online(&self) -> Vec<u32> {
        self.book
            .lock()
            .expect(BOOK_POISONED)
            .online
            .iter()
            .copied()
            .collect()
    }

    pub fn is_online(&self, id: u32) -> bool {
        self.book.lock().expect(BOOK_POISONED).online.contains(&id)
    }

    /// Bring one CPU online on the running VM.
    ///
    /// The guest still has to accept it: a hot-added CPU is an
    /// application processor, so it waits in `VRS_HALT` until the guest
    /// sends INIT/SIPI, exactly as a boot AP does. Nothing here resets
    /// it. `Vcpu::setup_bsp` calls `reboot_state()`, and that on an AP
    /// clears the state the guest's bring-up depends on. See
    /// `crate::vcpu::uefi_reset_targets`.
    pub fn add_vcpu(&self, id: u32) -> Result<(), VcpuError> {
        let vcpu_id = self.book.lock().expect(BOOK_POISONED).claim(id)?;

        match self.bring_online(id, vcpu_id) {
            Ok(()) => {
                info!(self.log, "vCPU brought online"; "vcpu" => id);
                Ok(())
            }
            Err(error) => {
                if error.left_cpu_inactive() {
                    self.book.lock().expect(BOOK_POISONED).release(id);
                } else {
                    // The kernel has no way to undo an activation, and
                    // this error cannot prove the activation did not
                    // happen, so the id stays claimed and a second add
                    // is refused.
                    warn!(self.log, "vCPU id spent: the kernel may have \
                        activated it";
                        "vcpu" => id, "error" => %error);
                }
                Err(error)
            }
        }
    }

    /// The steps of an add, in the one order that can be rolled back.
    ///
    /// The thread is made and parked first, then the per-vCPU boot
    /// steps are replayed, and activation is last. Every step before
    /// activation is undone by dropping `signal`, which lets the parked
    /// thread exit. Activation has no inverse, so nothing fallible
    /// follows it except the release of the gate.
    fn bring_online(&self, id: u32, vcpu_id: i32) -> Result<(), VcpuError> {
        if self.threads.is_closed() {
            return Err(VcpuError::ShuttingDown(id));
        }

        let metrics = Arc::new(VcpuMetrics::new());
        let (signal, gate) = vcpu_tasks::start_gate();
        let handle = vcpu_tasks::spawn_one(
            &self.ctx,
            vcpu_id,
            Arc::clone(&metrics),
            self.events.clone(),
            Some(gate),
            &self.log,
        )
        .map_err(|error| VcpuError::Spawn {
            id,
            error: error.to_string(),
        })?;
        if let Err(handle) = self.threads.push(handle) {
            // Teardown started between the check and the push. Drop
            // the signal so the parked thread exits, and join it: the
            // roster will not, because it is closed.
            drop(signal);
            if handle.join().is_err() {
                debug!(self.log, "the refused vCPU thread panicked";
                    "vcpu" => id);
            }
            return Err(VcpuError::ShuttingDown(id));
        }

        let vcpu = Vcpu::new_for_thread(vcpu_id, Arc::clone(&self.ctx.hdl));
        self.replay_boot_setup(id, vcpu_id, &vcpu)?;

        vcpu.activate().map_err(|error| match error {
            ActivateError::Busy => VcpuError::Suspended(id),
            ActivateError::OutOfRange => VcpuError::OutOfRange {
                id,
                max_cpus: self.possible_cpus(),
            },
            ActivateError::Os(error) => VcpuError::Activate {
                id,
                error: error.to_string(),
            },
        })?;

        if !signal.release() {
            return Err(VcpuError::ThreadGone(id));
        }

        self.book.lock().expect(BOOK_POISONED).record(id, metrics);
        Ok(())
    }

    /// Replay every per-vCPU step the boot path took.
    ///
    /// A missed step gives a CPU that behaves differently from its
    /// siblings, which is worse than one that fails to start. Both steps
    /// run before activation: neither ioctl asks whether the vCPU is
    /// active (`vm_set_cpuid` in `vmm_cpuid.c`, `vm_set_capability` in
    /// `vmm.c`).
    fn replay_boot_setup(
        &self,
        id: u32,
        vcpu_id: i32,
        vcpu: &Vcpu,
    ) -> Result<(), VcpuError> {
        if let Some(table) = &self.setup.cpuid {
            table.apply_to(&self.ctx.hdl, vcpu_id).map_err(|error| {
                VcpuError::Setup {
                    id,
                    step: "CPUID",
                    error: error.to_string(),
                }
            })?;
        }

        crate::vcpu::set_halt_exit(vcpu, self.setup.vmexit_on_hlt).map_err(
            |error| VcpuError::Setup {
                id,
                step: "HALT_EXIT",
                error: format!("{error:#}"),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{check_id, VcpuBook, VcpuError, KERNEL_MAX_CPUS};

    #[test]
    fn an_id_past_the_slot_count_is_refused() {
        assert_eq!(
            check_id(4, 4),
            Err(VcpuError::OutOfRange { id: 4, max_cpus: 4 }),
        );
        assert_eq!(check_id(3, 4), Ok(3));
    }

    #[test]
    fn an_id_past_the_kernel_ceiling_is_refused() {
        // vm_activate_cpu refuses vcpuid >= vm->maxcpus, and
        // vm_set_topology re-pins maxcpus to VM_MAXCPU whatever the
        // caller asks for, so a bigger MADT cannot widen this.
        let id = KERNEL_MAX_CPUS;
        assert_eq!(
            check_id(id, KERNEL_MAX_CPUS * 4),
            Err(VcpuError::OutOfRange {
                id,
                max_cpus: KERNEL_MAX_CPUS,
            }),
        );
        assert_eq!(check_id(KERNEL_MAX_CPUS - 1, u32::MAX), Ok(63));
    }

    #[test]
    fn no_slot_means_no_id_is_valid() {
        assert!(check_id(0, 0).is_err());
    }

    #[test]
    fn the_boot_set_starts_online() {
        let book = VcpuBook::new(8, 2);

        assert_eq!(book.online.iter().copied().collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn more_boot_cpus_than_slots_cannot_claim_a_slot_that_is_not_there() {
        let book = VcpuBook::new(2, 8);

        assert_eq!(book.online.iter().copied().collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn a_boot_cpu_cannot_be_added_again() {
        let mut book = VcpuBook::new(8, 2);

        assert_eq!(book.claim(1), Err(VcpuError::AlreadyOnline(1)));
    }

    #[test]
    fn a_claim_is_refused_the_second_time() {
        // The claim lands before any ioctl, so two concurrent adds of
        // one id cannot both reach VM_ACTIVATE_CPU.
        let mut book = VcpuBook::new(8, 1);

        assert_eq!(book.claim(4), Ok(4));
        assert_eq!(book.claim(4), Err(VcpuError::AlreadyOnline(4)));
    }

    #[test]
    fn a_released_claim_can_be_taken_again() {
        // A failed add that left the kernel with no active CPU must
        // give the id back, or a retry is refused forever.
        let mut book = VcpuBook::new(8, 1);
        assert_eq!(book.claim(4), Ok(4));

        book.release(4);

        assert_eq!(book.claim(4), Ok(4));
    }

    #[test]
    fn releasing_an_id_drops_its_metrics() {
        let mut book = VcpuBook::new(8, 1);
        book.claim(4).expect("slot 4 is free");
        book.record(
            4,
            std::sync::Arc::new(vmm_core::metrics::VcpuMetrics::new()),
        );

        book.release(4);

        assert!(book.metrics.is_empty());
    }

    #[test]
    fn an_out_of_range_claim_takes_no_slot() {
        let mut book = VcpuBook::new(2, 1);

        assert!(book.claim(9).is_err());
        assert_eq!(book.online.len(), 1, "only the boot CPU is online");
    }

    #[test]
    fn only_an_error_before_the_kernel_call_gives_the_id_back() {
        // There is no vm_deactivate_cpu, so an id the kernel MAY have
        // activated must stay claimed even though the add failed.
        for error in [
            VcpuError::OutOfRange { id: 4, max_cpus: 8 },
            VcpuError::AlreadyOnline(4),
            VcpuError::Setup {
                id: 4,
                step: "CPUID",
                error: "EIO".into(),
            },
            VcpuError::Spawn {
                id: 4,
                error: "no threads".into(),
            },
            VcpuError::ShuttingDown(4),
        ] {
            assert!(error.left_cpu_inactive(), "{error:?} is before the ioctl");
        }

        // ThreadGone means the activation went through. EBUSY cannot
        // prove it did not: vm_activate_cpu sets active_cpus and only
        // then answers EBUSY for a VM that was suspended in the
        // meantime. An unknown errno from the ioctl layer says nothing
        // either way.
        for error in [
            VcpuError::ThreadGone(4),
            VcpuError::Suspended(4),
            VcpuError::Activate {
                id: 4,
                error: "EIO".into(),
            },
        ] {
            assert!(!error.left_cpu_inactive(), "{error:?} may have activated");
        }
    }

    #[test]
    fn every_error_says_which_cpu_it_is_about() {
        let errors = [
            VcpuError::OutOfRange { id: 7, max_cpus: 4 },
            VcpuError::AlreadyOnline(7),
            VcpuError::Suspended(7),
            VcpuError::Activate {
                id: 7,
                error: "EIO".into(),
            },
            VcpuError::Setup {
                id: 7,
                step: "CPUID",
                error: "EIO".into(),
            },
            VcpuError::Spawn {
                id: 7,
                error: "EAGAIN".into(),
            },
            VcpuError::ShuttingDown(7),
            VcpuError::ThreadGone(7),
        ];

        for error in errors {
            assert!(
                error.to_string().contains('7'),
                "{error:?} does not name the vCPU",
            );
        }
    }
}
