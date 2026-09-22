// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CPU hot-add.
//!
//! The register file in [`vmm_devices::hotplug::cpu`] runs on a vCPU
//! thread inside an I/O exit, so it only records what the guest asked
//! for. This is the other half: one engine that owns every add, and one
//! thread that answers the ejects the guest runs.
//!
//! # Add only
//!
//! illumos has no `vm_deactivate_cpu`. `vm_activate_cpu` only ever sets
//! `active_cpus`, and the one place that clears it is `vm_init`, which
//! zeroes the whole set on `VM_REINIT` (both in `vmm.c`). CPU hot-REMOVE
//! is therefore not possible at the kernel boundary, so
//! [`CpuHotplugEngine`] has no remove call and the drain refuses every
//! eject instead of performing it.
//!
//! # Why the id can be consumed
//!
//! `VM_ACTIVATE_CPU` has no inverse. An add that reaches the kernel and
//! then fails leaves the CPU active with nothing running it, and no
//! later call can undo that. Such an id is recorded and refused for the
//! life of the VM, rather than retried into a second active CPU with no
//! thread.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::{Arc, Mutex};

use slog::{debug, error, info, warn, Logger};

use vmm_devices::hotplug::cpu::CpuHotplug;

use super::drain::DrainThread;
use super::lock;
use crate::vcpus::{VcpuError, VcpuRegistry};

/// Why a CPU hot-add was refused.
///
/// Hand-written rather than derived: this crate carries no `thiserror`
/// dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CpuHotplugError {
    /// The id names no CPU slot this machine describes.
    OutOfRange { id: u32, max_cpus: u32 },
    /// The id belongs to a CPU the boot path already brought online.
    /// The AML gives no boot CPU an insert event, so nothing would
    /// reach the guest.
    BootCpu(u32),
    /// The CPU is running already.
    AlreadyOnline(u32),
    /// An earlier add activated the CPU in the kernel and then failed.
    /// There is no `vm_deactivate_cpu`, so the id cannot be retried.
    Consumed(u32),
    /// The registry refused the add.
    Online(VcpuError),
}

impl fmt::Display for CpuHotplugError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CpuHotplugError::OutOfRange { id, max_cpus } => {
                write!(f, "vCPU {id} is not one of the {max_cpus} CPU slots")
            }
            CpuHotplugError::BootCpu(id) => {
                write!(f, "vCPU {id} is a boot CPU and is online already")
            }
            CpuHotplugError::AlreadyOnline(id) => {
                write!(f, "vCPU {id} is online already")
            }
            CpuHotplugError::Consumed(id) => write!(
                f,
                "vCPU {id} was consumed by a failed add: the kernel \
                 cannot deactivate a vCPU, so the id cannot be reused",
            ),
            CpuHotplugError::Online(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for CpuHotplugError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CpuHotplugError::Online(error) => Some(error),
            _ => None,
        }
    }
}

/// Brings one CPU online on the running VM.
///
/// A trait, not [`VcpuRegistry`] itself, so the order of an add, its
/// rollback and the eject refusal can be tested with no live VM: every
/// step of a real add is an ioctl.
pub trait CpuOnline: Send + Sync {
    /// Every CPU slot this machine describes, boot CPUs included.
    fn possible_cpus(&self) -> u32;
    /// How many CPUs the boot path brought online, ids 0 upward.
    fn boot_cpus(&self) -> u32;
    fn is_online(&self, id: u32) -> bool;
    fn add_vcpu(&self, id: u32) -> Result<(), VcpuError>;
}

impl CpuOnline for VcpuRegistry {
    fn possible_cpus(&self) -> u32 {
        VcpuRegistry::possible_cpus(self)
    }

    fn boot_cpus(&self) -> u32 {
        VcpuRegistry::boot_cpus(self)
    }

    fn is_online(&self, id: u32) -> bool {
        VcpuRegistry::is_online(self, id)
    }

    fn add_vcpu(&self, id: u32) -> Result<(), VcpuError> {
        VcpuRegistry::add_vcpu(self, id)
    }
}

/// Everything an add or an eject touches, with no live VM in it.
///
/// Split out from [`CpuHotplugEngine`] because the drain thread holds a
/// [`Weak`](std::sync::Weak) to it, and because this half must stay
/// testable.
struct Core {
    vcpus: Arc<dyn CpuOnline>,
    regs: Arc<CpuHotplug>,
    /// Ids the kernel activated for an add that then failed.
    ///
    /// A set of ids, never a `Vec` indexed by one: an id is an operator
    /// value and reaches no slice here.
    consumed: Mutex<BTreeSet<u32>>,
    log: Logger,
}

impl Core {
    /// Bring one CPU online, then tell the guest.
    fn add_cpu(&self, id: u32) -> Result<(), CpuHotplugError> {
        // Every refusal lands before the kernel is asked, so a bad
        // request activates nothing and consumes no id.
        self.check_addable(id)?;

        // Activation is LAZY, at the moment of the request, and a spare
        // CPU is never activated in advance. `vm_handle_hlt` in `vmm.c`
        // declares the VM halted only when `halted_cpus` equals
        // `active_cpus`. A vCPU the kernel has activated but the guest
        // has never onlined sits in `VRS_HALT`, never reaches the HLT
        // accounting, and so never joins `halted_cpus`. Pre-activating a
        // spare CPU would stop `VM_SUSPEND_HALT` firing for the life of
        // the VM.
        if let Err(error) = self.vcpus.add_vcpu(id) {
            return Err(self.roll_back(id, error));
        }

        // Last: the guest must not look for a CPU the kernel has not
        // activated. The register file describes `max_cpus` slots and
        // the registry stops at the kernel's 64, so this id always
        // names a slot the guest can see.
        self.regs.notify_added(id);
        info!(self.log, "vCPU hot-added"; "cpu" => id);
        Ok(())
    }

    /// Refuse an id the machine, the boot set or an earlier add owns.
    fn check_addable(&self, id: u32) -> Result<(), CpuHotplugError> {
        let max_cpus = self.vcpus.possible_cpus();
        if id >= max_cpus {
            return Err(CpuHotplugError::OutOfRange { id, max_cpus });
        }
        if id < self.vcpus.boot_cpus() {
            return Err(CpuHotplugError::BootCpu(id));
        }
        if lock(&self.consumed).contains(&id) {
            return Err(CpuHotplugError::Consumed(id));
        }
        if self.vcpus.is_online(id) {
            return Err(CpuHotplugError::AlreadyOnline(id));
        }
        Ok(())
    }

    /// Decide whether a failed add gives its id back.
    ///
    /// The kernel activation itself CANNOT be rolled back: there is no
    /// `vm_deactivate_cpu`. So an add that got as far as
    /// `VM_ACTIVATE_CPU` and then failed consumes the id for the life
    /// of the VM, and the next request for it is refused with
    /// [`CpuHotplugError::Consumed`] rather than retried. Every step
    /// before the activation is undone by the registry, which releases
    /// the claim, so those ids stay usable.
    ///
    /// Nothing has to be undone in the register file: `notify_added`
    /// runs only after the add succeeds, so a failed add left the slot
    /// reading absent, which is what it is.
    fn roll_back(&self, id: u32, error: VcpuError) -> CpuHotplugError {
        if error.left_cpu_inactive() {
            warn!(self.log, "CPU hot-add failed; the id can be retried";
                "cpu" => id, "error" => %error);
        } else {
            lock(&self.consumed).insert(id);
            error!(self.log, "CPU hot-add left the id consumed";
                "cpu" => id, "error" => %error);
        }
        CpuHotplugError::Online(error)
    }

    /// Refuse every eject the guest has run since the last pass.
    ///
    /// A guest can drive this path, so both log sites are debug.
    fn drain_ejects(&self) {
        for id in self.regs.take_eject_requests() {
            self.refuse_eject(id);
        }
    }

    /// Answer one eject. The CPU keeps running.
    ///
    /// `request_eject` has already cleared the slot's enable bit, so
    /// the guest now reads `_STA` as absent for a CPU the kernel still
    /// counts in `active_cpus` and a VMM thread still runs. Putting the
    /// slot back is the honest answer: the CPU IS there. It also ends
    /// the exchange, because an eject needs a `pending_removal` that
    /// only the host sets and `notify_added` clears.
    fn refuse_eject(&self, id: u32) {
        if !self.vcpus.is_online(id) {
            // Re-advertising here would claim a CPU that is not
            // running, which is the same lie in the other direction.
            debug!(self.log, "eject names a CPU that is not online";
                "cpu" => id);
            return;
        }
        debug!(self.log, "refused a CPU eject: the kernel cannot \
            deactivate a vCPU"; "cpu" => id);
        self.regs.notify_added(id);
    }
}

/// The CPU hot-add engine, and the thread that answers ejects.
pub struct CpuHotplugEngine {
    core: Arc<Core>,
    thread: DrainThread,
}

impl CpuHotplugEngine {
    /// Start the engine and its drain thread.
    ///
    /// `regs` must be the register file the PM phase attached, so the
    /// slots the guest reads and the CPUs this engine adds are the same
    /// set.
    pub fn start(
        vcpus: Arc<VcpuRegistry>,
        regs: Arc<CpuHotplug>,
        log: Logger,
    ) -> Arc<Self> {
        Self::start_online(vcpus as Arc<dyn CpuOnline>, regs, log)
    }

    /// The same start against any [`CpuOnline`].
    pub fn start_online(
        vcpus: Arc<dyn CpuOnline>,
        regs: Arc<CpuHotplug>,
        log: Logger,
    ) -> Arc<Self> {
        let core = Arc::new(Core {
            vcpus,
            regs,
            consumed: Mutex::new(BTreeSet::new()),
            log: log.clone(),
        });
        let thread = DrainThread::spawn(
            "hotplug-cpu",
            &core,
            Core::drain_ejects,
            &log,
            "no CPU hotplug thread; ejects will not be answered",
        );

        Arc::new(Self { core, thread })
    }

    /// Bring one CPU online on the running VM.
    pub fn add_cpu(&self, id: u32) -> Result<(), CpuHotplugError> {
        self.core.add_cpu(id)
    }

    /// The ids no add can take again, in ascending order.
    pub fn consumed_cpus(&self) -> Vec<u32> {
        lock(&self.core.consumed).iter().copied().collect()
    }

    /// Stop the drain thread and wait for it.
    pub fn shutdown(&self) {
        self.thread.shutdown(super::DRAIN_JOIN_BUDGET);
    }
}

impl Drop for CpuHotplugEngine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use vmm_core::common::{RWOp, ReadOp, WriteOp};
    use vmm_devices::acpi_gpe::{GpeBit, HotplugEventSink};

    // The guest ABI of the register block, as a guest drives it. Named
    // here because the constants are private to `vmm_devices`.
    const OFF_SELECTOR: usize = 0x0;
    const OFF_FLAGS: usize = 0x4;
    const STS_ENABLED: u8 = 1 << 0;
    const STS_INSERT_EVENT: u8 = 1 << 1;
    const CTL_EJECT: u8 = 1 << 3;

    /// Counts the GPE bits the register file raises.
    #[derive(Default)]
    struct RecordingSink {
        raised: Mutex<Vec<GpeBit>>,
    }

    impl RecordingSink {
        fn raised(&self) -> Vec<GpeBit> {
            lock(&self.raised).clone()
        }
    }

    impl HotplugEventSink for RecordingSink {
        fn raise(&self, bit: GpeBit) {
            lock(&self.raised).push(bit);
        }
    }

    /// A stand-in registry, because every step of a real add is an
    /// ioctl.
    struct FakeCpus {
        possible: u32,
        boot: u32,
        online: Mutex<BTreeSet<u32>>,
        /// What the next adds answer, taken from the back.
        answers: Mutex<Vec<VcpuError>>,
        /// Every id that reached the registry, in order.
        adds: Mutex<Vec<u32>>,
    }

    impl FakeCpus {
        fn new(possible: u32, boot: u32) -> Arc<Self> {
            Arc::new(Self {
                possible,
                boot,
                online: Mutex::new((0..boot.min(possible)).collect()),
                answers: Mutex::new(Vec::new()),
                adds: Mutex::new(Vec::new()),
            })
        }

        fn answer_with(self: &Arc<Self>, error: VcpuError) {
            lock(&self.answers).push(error);
        }

        fn adds(&self) -> Vec<u32> {
            lock(&self.adds).clone()
        }
    }

    impl CpuOnline for FakeCpus {
        fn possible_cpus(&self) -> u32 {
            self.possible
        }

        fn boot_cpus(&self) -> u32 {
            self.boot
        }

        fn is_online(&self, id: u32) -> bool {
            lock(&self.online).contains(&id)
        }

        fn add_vcpu(&self, id: u32) -> Result<(), VcpuError> {
            lock(&self.adds).push(id);
            match lock(&self.answers).pop() {
                None => {
                    lock(&self.online).insert(id);
                    Ok(())
                }
                Some(error) => {
                    // The registry keeps the claim when the kernel
                    // activated the CPU, because nothing undoes that.
                    if !error.left_cpu_inactive() {
                        lock(&self.online).insert(id);
                    }
                    Err(error)
                }
            }
        }
    }

    fn test_log() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    /// One engine over a fake registry, plus the pieces to inspect.
    struct Harness {
        engine: Arc<CpuHotplugEngine>,
        cpus: Arc<FakeCpus>,
        regs: Arc<CpuHotplug>,
        sink: Arc<RecordingSink>,
    }

    fn harness(possible: u32, boot: u32) -> Harness {
        let cpus = FakeCpus::new(possible, boot);
        let sink = Arc::new(RecordingSink::default());
        let regs =
            CpuHotplug::new(possible, Arc::clone(&sink) as Arc<_>, test_log());
        regs.set_boot_cpus(boot);
        let engine = CpuHotplugEngine::start_online(
            Arc::clone(&cpus) as Arc<dyn CpuOnline>,
            Arc::clone(&regs),
            test_log(),
        );
        Harness {
            engine,
            cpus,
            regs,
            sink,
        }
    }

    /// The status byte of one slot, as the guest's `CSTA` reads it.
    fn status(regs: &CpuHotplug, id: u32) -> u8 {
        let selector = u32::to_le_bytes(id);
        regs.pio_rw(OFF_SELECTOR, RWOp::Write(&WriteOp::from_buf(&selector)));
        let mut ro = ReadOp::new(1);
        regs.pio_rw(OFF_FLAGS, RWOp::Read(&mut ro));
        ro.buf()[0]
    }

    /// Run the guest's `_EJ0` on one slot.
    fn guest_ejects(regs: &CpuHotplug, id: u32) {
        let selector = u32::to_le_bytes(id);
        regs.pio_rw(OFF_SELECTOR, RWOp::Write(&WriteOp::from_buf(&selector)));
        regs.pio_rw(OFF_FLAGS, RWOp::Write(&WriteOp::from_buf(&[CTL_EJECT])));
    }

    #[test]
    fn an_add_brings_the_cpu_online_and_then_tells_the_guest() {
        let h = harness(8, 2);

        h.engine.add_cpu(4).expect("slot 4 is free");

        assert_eq!(h.cpus.adds(), [4], "the registry was asked once");
        assert!(h.cpus.is_online(4));
        let status = status(&h.regs, 4);
        assert_eq!(status & STS_ENABLED, STS_ENABLED, "the slot reads absent");
        assert_eq!(status & STS_INSERT_EVENT, STS_INSERT_EVENT);
    }

    #[test]
    fn an_add_raises_the_cpu_gpe_bit() {
        // Bit 2 runs the guest's `_E02` handler, which scans the slots.
        // Without it the guest never looks at the register file again.
        let h = harness(8, 2);

        h.engine.add_cpu(4).expect("slot 4 is free");

        assert_eq!(h.sink.raised(), [GpeBit::Cpu]);
    }

    #[test]
    fn an_id_past_the_last_slot_is_refused_before_the_kernel() {
        let h = harness(8, 2);

        assert_eq!(
            h.engine.add_cpu(8),
            Err(CpuHotplugError::OutOfRange { id: 8, max_cpus: 8 }),
        );
        assert_eq!(
            h.engine.add_cpu(u32::MAX),
            Err(CpuHotplugError::OutOfRange {
                id: u32::MAX,
                max_cpus: 8,
            }),
        );
        assert!(h.cpus.adds().is_empty(), "the kernel was asked anyway");
        assert!(h.sink.raised().is_empty(), "the guest was told anyway");
    }

    #[test]
    fn a_boot_cpu_cannot_be_hot_added() {
        // A boot CPU is running before the guest reads the DSDT, so an
        // insert event for it describes something that never happened.
        let h = harness(8, 2);

        assert_eq!(h.engine.add_cpu(0), Err(CpuHotplugError::BootCpu(0)));
        assert_eq!(h.engine.add_cpu(1), Err(CpuHotplugError::BootCpu(1)));
        assert!(h.cpus.adds().is_empty());
        assert!(h.sink.raised().is_empty());
    }

    #[test]
    fn a_vm_with_no_spare_slot_can_add_nothing() {
        // `pm::HotplugOpts::has_cpu_slots` claims no register file when
        // max_cpus is not above the boot count, and the DSDT emits no
        // controller. The engine has to make the same cut, or the two
        // halves disagree about what a guest can be told.
        let h = harness(4, 4);

        for id in 0..5 {
            assert!(h.engine.add_cpu(id).is_err(), "id {id} was accepted");
        }
        assert!(h.cpus.adds().is_empty());
        assert!(h.sink.raised().is_empty());
    }

    #[test]
    fn a_cpu_that_is_running_cannot_be_added_again() {
        let h = harness(8, 2);
        h.engine.add_cpu(4).expect("slot 4 is free");

        assert_eq!(h.engine.add_cpu(4), Err(CpuHotplugError::AlreadyOnline(4)),);
        assert_eq!(h.cpus.adds(), [4], "the second add reached the kernel");
    }

    #[test]
    fn an_add_that_left_the_kernel_alone_can_be_retried() {
        // A spawn failure never reached VM_ACTIVATE_CPU, so the id is
        // still free and a retry must work.
        let h = harness(8, 2);
        h.cpus.answer_with(VcpuError::Spawn {
            id: 4,
            error: "EAGAIN".into(),
        });

        let refused = h.engine.add_cpu(4).expect_err("the spawn failed");
        assert!(matches!(refused, CpuHotplugError::Online(_)));
        assert!(
            h.sink.raised().is_empty(),
            "the guest was told of a failure"
        );
        assert_eq!(status(&h.regs, 4) & STS_ENABLED, 0, "the slot is not free");

        h.engine.add_cpu(4).expect("the id was given back");
        assert_eq!(h.cpus.adds(), [4, 4]);
    }

    #[test]
    fn an_id_the_kernel_consumed_is_refused_for_good() {
        // ThreadGone means VM_ACTIVATE_CPU went through and the thread
        // did not. There is no vm_deactivate_cpu, so the id is spent.
        let h = harness(8, 2);
        h.cpus.answer_with(VcpuError::ThreadGone(4));

        let refused = h.engine.add_cpu(4).expect_err("the thread has gone");
        assert_eq!(refused, CpuHotplugError::Online(VcpuError::ThreadGone(4)),);

        assert_eq!(h.engine.add_cpu(4), Err(CpuHotplugError::Consumed(4)));
        assert_eq!(h.cpus.adds(), [4], "a consumed id reached the kernel");
        assert_eq!(h.engine.consumed_cpus(), [4]);
        // A neighbouring id is untouched.
        h.engine.add_cpu(5).expect("slot 5 is free");
    }

    #[test]
    fn an_ebusy_from_the_kernel_also_spends_the_id() {
        // vm_activate_cpu sets active_cpus and only then answers EBUSY
        // for a VM that was suspended in the meantime. The CPU may be
        // active, so the id cannot go back into the pool.
        let h = harness(8, 2);
        h.cpus.answer_with(VcpuError::Suspended(4));

        h.engine.add_cpu(4).expect_err("the kernel said EBUSY");

        assert_eq!(h.engine.add_cpu(4), Err(CpuHotplugError::Consumed(4)));
        assert_eq!(h.cpus.adds(), [4], "a spent id reached the kernel once");
        assert_eq!(h.engine.consumed_cpus(), [4]);
    }

    #[test]
    fn an_eject_is_refused_and_the_cpu_keeps_its_slot() {
        let h = harness(8, 2);
        h.engine.add_cpu(4).expect("slot 4 is free");
        h.regs.notify_removed(4);
        guest_ejects(&h.regs, 4);
        assert_eq!(
            status(&h.regs, 4) & STS_ENABLED,
            0,
            "the register file did not take the eject",
        );

        h.engine.core.drain_ejects();

        assert_eq!(
            status(&h.regs, 4) & STS_ENABLED,
            STS_ENABLED,
            "a CPU the kernel still runs reads as gone",
        );
        assert!(h.cpus.is_online(4), "nothing may take a vCPU down");
    }

    #[test]
    fn a_refused_eject_cannot_be_repeated_by_the_guest() {
        // The refusal clears `pending_removal`, so the next `_EJ0`
        // finds nothing to do and the exchange ends.
        let h = harness(8, 2);
        h.engine.add_cpu(4).expect("slot 4 is free");
        h.regs.notify_removed(4);
        guest_ejects(&h.regs, 4);
        h.engine.core.drain_ejects();

        guest_ejects(&h.regs, 4);

        assert!(h.regs.take_eject_requests().is_empty());
        assert_eq!(status(&h.regs, 4) & STS_ENABLED, STS_ENABLED);
    }

    #[test]
    fn an_eject_of_a_cpu_that_is_not_online_is_not_advertised_back() {
        // The register file is told about a slot the registry never
        // brought online. Re-advertising it would claim a CPU that no
        // thread runs.
        let h = harness(8, 2);
        h.regs.notify_added(4);
        h.regs.notify_removed(4);
        guest_ejects(&h.regs, 4);

        h.engine.core.drain_ejects();

        assert_eq!(status(&h.regs, 4) & STS_ENABLED, 0);
    }

    #[test]
    fn the_drain_takes_every_eject_in_one_pass() {
        let h = harness(8, 1);
        for id in [1, 2, 3] {
            h.engine.add_cpu(id).expect("the slot is free");
            h.regs.notify_removed(id);
            guest_ejects(&h.regs, id);
        }

        h.engine.core.drain_ejects();

        for id in [1, 2, 3] {
            assert_eq!(
                status(&h.regs, id) & STS_ENABLED,
                STS_ENABLED,
                "CPU {id} was left reading as gone",
            );
        }
        assert!(h.regs.take_eject_requests().is_empty());
    }

    #[test]
    fn shutdown_ends_the_drain_thread() {
        let h = harness(8, 2);

        h.engine.shutdown();

        assert!(h.engine.thread.is_stopped());
        // A second shutdown, and the one in Drop, must both be quiet.
        h.engine.shutdown();
    }

    #[test]
    fn every_error_says_which_cpu_it_is_about() {
        let errors = [
            CpuHotplugError::OutOfRange { id: 7, max_cpus: 4 },
            CpuHotplugError::BootCpu(7),
            CpuHotplugError::AlreadyOnline(7),
            CpuHotplugError::Consumed(7),
            CpuHotplugError::Online(VcpuError::ThreadGone(7)),
        ];

        for error in errors {
            assert!(
                error.to_string().contains('7'),
                "{error:?} does not name the vCPU",
            );
        }
    }
}
