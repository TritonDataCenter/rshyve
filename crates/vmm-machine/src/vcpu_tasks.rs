// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! vCPU thread management.
//!
//! Each vCPU runs on a dedicated OS thread, executing a run loop that:
//! 1. Enters the guest via `vcpu.enter()`
//! 2. Parses the VM exit
//! 3. Dispatches to the PIO or MMIO bus
//! 4. Constructs the re-entry command
//! 5. Repeats
//!
//! The loop terminates on suspend events (halt, reset, triple-fault)
//! which are sent to the main thread via a channel.

use std::io;
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;

use bhyve_api::vm_reg_name;
use slog::{debug, error, info, warn, Logger};

#[usdt::provider(provider = "vmm")]
mod probes {
    fn vm_entry(vcpuid: u32) {}
    fn vm_exit(vcpuid: u32, rip: u64, exit_code: u32) {}
    fn vcpu_suspended(vcpuid: u32, kind: u8) {}
}

use vmm_core::exits::{
    InoutReq, InoutRes, MmioReq, MmioRes, Suspend, VmEntry, VmExitKind,
};
use vmm_core::hdl::{SuspendHow, SuspendOutcome, VmmHdl};
use vmm_core::metrics::VcpuMetrics;
use vmm_core::mmio::MmioBus;
use vmm_core::msr::{MsrHandler, RdmsrOutcome, WrmsrOutcome};
use vmm_core::pio::PioBus;
use vmm_core::vcpu::{EnterError, Vcpu};

/// Events sent from vCPU threads to the main thread.
#[derive(Debug)]
pub enum VcpuEvent {
    /// vCPU suspended (halt, reset, or triple-fault).
    Suspended { vcpu_id: i32, kind: Suspend },
    /// vCPU encountered a fatal error.
    Error { vcpu_id: i32, error: String },
}

fn diagnostic_reg(vcpu: &Vcpu, reg: vm_reg_name) -> String {
    match vcpu.get_reg(reg) {
        Ok(value) => format!("{value:#018x}"),
        Err(error) => format!("unavailable: {error}"),
    }
}

fn diagnostic_cs(vcpu: &Vcpu) -> String {
    match vcpu.get_segment_desc(vm_reg_name::VM_REG_GUEST_CS) {
        Ok(desc) => format!(
            "base={:#018x} limit={:#010x} access={:#010x}",
            desc.base, desc.limit, desc.access,
        ),
        Err(error) => format!("unavailable: {error}"),
    }
}

fn fatal_exit(
    vcpu: &Vcpu,
    vcpu_id: i32,
    hdl: &VmmHdl,
    events: &Sender<VcpuEvent>,
    reason: String,
    log: &Logger,
) {
    error!(log, "fatal vCPU exit";
        "reason" => &reason,
        "rax" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_RAX),
        "rbx" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_RBX),
        "rcx" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_RCX),
        "rdx" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_RDX),
        "rsi" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_RSI),
        "rdi" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_RDI),
        "rbp" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_RBP),
        "r8" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_R8),
        "r9" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_R9),
        "r10" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_R10),
        "r11" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_R11),
        "r12" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_R12),
        "r13" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_R13),
        "r14" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_R14),
        "r15" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_R15),
        "rip" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_RIP),
        "rsp" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_RSP),
        "rflags" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_RFLAGS),
        "cr0" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_CR0),
        "cr2" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_CR2),
        "cr3" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_CR3),
        "cr4" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_CR4),
        "efer" => diagnostic_reg(vcpu, vm_reg_name::VM_REG_GUEST_EFER),
        "cs" => diagnostic_cs(vcpu),
    );

    match hdl.suspend(SuspendHow::Halt, vcpu_id) {
        Ok(SuspendOutcome::Requested) => {}
        Ok(SuspendOutcome::AlreadyLatched) => {
            debug!(log, "VM suspend already latched";
                "how" => %SuspendHow::Halt);
        }
        Err(error) => {
            error!(log, "failed to halt VM after fatal vCPU exit";
                "error" => %error);
        }
    }

    // The receiver can be gone only after shutdown has already begun.
    let _ = events.send(VcpuEvent::Error {
        vcpu_id,
        error: reason,
    });
}

fn should_discard_once(rip: u64, last: &mut Option<u64>) -> bool {
    if *last == Some(rip) {
        false
    } else {
        *last = Some(rip);
        true
    }
}

/// Everything a vCPU thread needs that is the same for every vCPU.
///
/// One value, cloned per thread, so a vCPU started after boot is given
/// the same buses, handle and MSR handler as its siblings.
#[derive(Clone)]
pub struct VcpuThreadCtx {
    pub bus_pio: Arc<PioBus>,
    pub bus_mmio: Arc<MmioBus>,
    pub hdl: Arc<VmmHdl>,
    pub api_version: u32,
    pub msr_handler: Option<Arc<dyn MsrHandler>>,
}

impl VcpuThreadCtx {
    pub fn from_machine(
        machine: &vmm_core::machine::Machine,
        api_version: u32,
        msr_handler: Option<Arc<dyn MsrHandler>>,
    ) -> Self {
        Self {
            bus_pio: Arc::clone(machine.bus_pio()),
            bus_mmio: Arc::clone(machine.bus_mmio()),
            hdl: Arc::clone(machine.hdl()),
            api_version,
            msr_handler,
        }
    }
}

/// Holds a fresh vCPU thread outside the guest until the kernel has
/// the CPU.
///
/// illumos has no `vm_deactivate_cpu`, so `VM_ACTIVATE_CPU` cannot be
/// undone. The thread is therefore made and parked first, and
/// activation is the last step of an add that can fail. Dropping the
/// [`StartSignal`] instead of releasing it tells the parked thread to
/// exit without ever entering the guest, which is the whole rollback.
pub struct StartGate {
    rx: mpsc::Receiver<()>,
}

impl StartGate {
    /// Block until the caller releases the gate. False means the
    /// signal was dropped and the thread must exit.
    pub fn wait(self) -> bool {
        self.rx.recv().is_ok()
    }
}

/// The release side of a [`StartGate`].
pub struct StartSignal {
    tx: Sender<()>,
}

impl StartSignal {
    /// Let the parked thread enter the guest. False means the thread
    /// is already gone.
    pub fn release(self) -> bool {
        self.tx.send(()).is_ok()
    }
}

/// Make a parked-thread gate and its release signal.
pub fn start_gate() -> (StartSignal, StartGate) {
    let (tx, rx) = mpsc::channel();
    (StartSignal { tx }, StartGate { rx })
}

/// Spawn one vCPU thread and run the exit-dispatch loop in it.
///
/// With a `gate`, the thread parks until the caller releases it. That
/// is what a hot-add uses: the spawn, which can fail, happens before
/// the activation, which cannot be undone.
pub fn spawn_one(
    ctx: &VcpuThreadCtx,
    vcpu_id: i32,
    metrics: Arc<VcpuMetrics>,
    events: Sender<VcpuEvent>,
    gate: Option<StartGate>,
    log: &Logger,
) -> io::Result<thread::JoinHandle<()>> {
    let vcpu_log = log.new(slog::o!("vcpu" => vcpu_id));

    let bus_pio = Arc::clone(&ctx.bus_pio);
    let bus_mmio = Arc::clone(&ctx.bus_mmio);
    let vmm_hdl = Arc::clone(&ctx.hdl);
    let api_version = ctx.api_version;
    let msr_handler = ctx.msr_handler.clone();
    let thread_vcpu = Vcpu::new_for_thread(vcpu_id, Arc::clone(&ctx.hdl));

    thread::Builder::new()
        .name(format!("vcpu-{}", vcpu_id))
        .spawn(move || {
            if let Some(gate) = gate {
                if !gate.wait() {
                    debug!(vcpu_log, "vCPU thread released before entry");
                    return;
                }
            }
            vcpu_run_loop(
                &thread_vcpu,
                &bus_pio,
                &bus_mmio,
                &vmm_hdl,
                api_version,
                &metrics,
                msr_handler.as_deref(),
                &events,
                &vcpu_log,
            );
        })
}

/// A boot set that could not be started whole.
///
/// Carries the threads that did start, because the boot vCPUs are
/// activated before the spawn: every one of them is already running
/// guest code, and the caller has to stop them before it unwinds past
/// the machine they run on.
pub struct BootSpawnError {
    /// The threads already running, in id order.
    pub started: Vec<thread::JoinHandle<()>>,
    /// The vCPU whose thread was refused.
    pub vcpu: i32,
    pub source: io::Error,
}

/// Spawn a thread for the whole boot set of vCPUs.
///
/// No thread parks: the boot set is activated before this call. The
/// metrics are zipped, not indexed, so a short list cannot panic.
pub fn spawn_boot_threads(
    ctx: &VcpuThreadCtx,
    vcpus: &[Vcpu],
    vcpu_metrics: &[Arc<VcpuMetrics>],
    events: &Sender<VcpuEvent>,
    log: &Logger,
) -> Result<Vec<thread::JoinHandle<()>>, BootSpawnError> {
    let mut started = Vec::with_capacity(vcpus.len());
    for (vcpu, metrics) in vcpus.iter().zip(vcpu_metrics.iter()) {
        match spawn_one(
            ctx,
            vcpu.id(),
            Arc::clone(metrics),
            events.clone(),
            None,
            log,
        ) {
            Ok(handle) => started.push(handle),
            Err(source) => {
                return Err(BootSpawnError {
                    started,
                    vcpu: vcpu.id(),
                    source,
                })
            }
        }
    }
    Ok(started)
}

/// The main vCPU execution loop.
///
/// Runs until a suspend event occurs or a fatal error is encountered.
fn vcpu_run_loop(
    vcpu: &Vcpu,
    bus_pio: &PioBus,
    bus_mmio: &MmioBus,
    hdl: &VmmHdl,
    api_version: u32,
    metrics: &VcpuMetrics,
    msr_handler: Option<&dyn MsrHandler>,
    events: &Sender<VcpuEvent>,
    log: &Logger,
) {
    let vcpu_id = vcpu.id();

    let mut next_entry = VmEntry::Run;
    let mut discarded_at: Option<u64> = None;
    info!(log, "vCPU run loop starting"; "vcpu" => vcpu_id);

    loop {
        probes::vm_entry!(|| vcpu_id as u32);
        let exit = match vcpu.enter(&next_entry, api_version) {
            Ok(exit) => exit,
            Err(EnterError::Interrupted) => {
                next_entry = VmEntry::Run;
                continue;
            }
            Err(EnterError::Paused) => {
                // resume() unblocks the next entry.
                thread::sleep(std::time::Duration::from_millis(10));
                next_entry = VmEntry::Run;
                continue;
            }
            Err(e) => {
                error!(log, "VM_RUN failed"; "error" => %e);
                // The receiver is gone only during shutdown.
                let _ = events.send(VcpuEvent::Error {
                    vcpu_id,
                    error: e.to_string(),
                });
                return;
            }
        };

        probes::vm_exit!(|| (vcpu_id as u32, exit.rip, exit.kind.exit_code()));
        metrics.record_exit(&exit.kind);

        next_entry = match exit.kind {
            VmExitKind::Bogus | VmExitKind::Debug => VmEntry::Run,

            VmExitKind::Inout(ref req) => {
                let res = handle_inout(req, bus_pio);
                VmEntry::InoutFulfill(res)
            }

            VmExitKind::Mmio(req) => {
                let res = handle_mmio(&req, bus_mmio);
                VmEntry::MmioFulfill(res)
            }

            VmExitKind::Rdmsr(code) => {
                handle_rdmsr(vcpu, vcpu_id, code, msr_handler, log);
                VmEntry::Run
            }

            VmExitKind::Wrmsr(code, val) => {
                handle_wrmsr(vcpu, vcpu_id, code, val, hdl, msr_handler, log);
                VmEntry::Run
            }

            VmExitKind::Suspended(detail) => {
                info!(log, "vCPU suspended";
                    "kind" => ?detail.kind,
                    "when_ns" => detail.when.as_nanos(),
                );
                // The receiver is gone only during shutdown.
                let _ = events.send(VcpuEvent::Suspended {
                    vcpu_id,
                    kind: detail.kind,
                });
                return;
            }

            VmExitKind::InstEmul { inst, num_valid } => {
                let valid_len = usize::from(num_valid).min(inst.len());
                let inst_hex: String = inst[..valid_len]
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<Vec<_>>()
                    .join(" ");
                if should_discard_once(exit.rip, &mut discarded_at) {
                    // A userspace-fallback exit leaves kernel decode state
                    // pending until it is explicitly discarded.
                    warn!(log, "discarding an instruction the kernel could not emulate";
                        "probe" => "inst_emul_discard",
                        "rip" => format!("{:#x}", exit.rip),
                        "inst" => &inst_hex);
                    VmEntry::DiscardInstr
                } else {
                    fatal_exit(
                        vcpu,
                        vcpu_id,
                        hdl,
                        events,
                        format!(
                            "unsupported instruction at RIP {:#x}: [{}]",
                            exit.rip, inst_hex,
                        ),
                        log,
                    );
                    return;
                }
            }

            VmExitKind::Hlt => {
                // The kernel handles HLT itself, because HALT_EXIT is off
                // by default from API v6. An exit that reaches here needs
                // no action, so re-enter.
                VmEntry::Run
            }

            VmExitKind::VmxError(status) => {
                fatal_exit(
                    vcpu,
                    vcpu_id,
                    hdl,
                    events,
                    format!("VMX error, status {}", status),
                    log,
                );
                return;
            }

            VmExitKind::SvmError(code) => {
                fatal_exit(
                    vcpu,
                    vcpu_id,
                    hdl,
                    events,
                    format!("SVM error, code {}", code),
                    log,
                );
                return;
            }

            VmExitKind::Paging(gpa, fault_type) => {
                fatal_exit(
                    vcpu,
                    vcpu_id,
                    hdl,
                    events,
                    format!(
                        "unhandled paging fault at GPA {:#x}, fault type {}",
                        gpa, fault_type,
                    ),
                    log,
                );
                return;
            }

            VmExitKind::Unknown(code) => {
                fatal_exit(
                    vcpu,
                    vcpu_id,
                    hdl,
                    events,
                    format!("unknown exit code {}", code),
                    log,
                );
                return;
            }
        };
    }
}

/// Dispatch an IN/OUT instruction to the PIO bus.
fn handle_inout(req: &InoutReq, bus: &PioBus) -> InoutRes {
    match req {
        InoutReq::In(io) => {
            let val = bus.handle_in(io.port, io.bytes);
            InoutRes::In(*io, val)
        }
        InoutReq::Out(io, val) => {
            bus.handle_out(io.port, io.bytes, *val);
            InoutRes::Out(*io)
        }
    }
}

/// Dispatch an MMIO access to the MMIO bus.
fn handle_mmio(req: &MmioReq, bus: &MmioBus) -> MmioRes {
    match req {
        MmioReq::Read(r) => {
            let data = bus.handle_read(r.addr, r.bytes);
            MmioRes::Read {
                addr: r.addr,
                data,
                bytes: r.bytes,
            }
        }
        MmioReq::Write(w) => {
            bus.handle_write(w.addr, w.bytes, w.data);
            MmioRes::Write {
                addr: w.addr,
                bytes: w.bytes,
            }
        }
    }
}

fn write_msr_result(vcpu: &Vcpu, value: u64) {
    let lo = value as u32 as u64;
    let hi = (value >> 32) as u32 as u64;
    let _ = vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RAX, lo);
    let _ = vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RDX, hi);
}

fn handle_rdmsr(
    vcpu: &Vcpu,
    vcpu_id: i32,
    code: u32,
    msr_handler: Option<&dyn MsrHandler>,
    log: &Logger,
) {
    if let Some(handler) = msr_handler {
        match handler.rdmsr(vcpu_id, code) {
            RdmsrOutcome::Handled(value) => {
                write_msr_result(vcpu, value);
                return;
            }
            RdmsrOutcome::GpException => {
                if let Err(e) = vcpu.inject_gp() {
                    warn!(log, "failed to inject #GP for rdmsr"; "msr" => format!("{:#x}", code), "error" => %e);
                }
                return;
            }
            RdmsrOutcome::NotHandled => { /* fall through */ }
        }
    }
    debug!(log, "unhandled rdmsr"; "msr" => format!("{:#x}", code));
    write_msr_result(vcpu, 0);
}

fn handle_wrmsr(
    vcpu: &Vcpu,
    vcpu_id: i32,
    code: u32,
    val: u64,
    hdl: &VmmHdl,
    msr_handler: Option<&dyn MsrHandler>,
    log: &Logger,
) {
    if let Some(handler) = msr_handler {
        match handler.wrmsr(vcpu_id, code, val) {
            WrmsrOutcome::Handled => return,
            WrmsrOutcome::GpException => {
                if let Err(e) = vcpu.inject_gp() {
                    warn!(log, "failed to inject #GP for wrmsr"; "msr" => format!("{:#x}", code), "error" => %e);
                }
                return;
            }
            WrmsrOutcome::Reset => {
                if let Err(e) = hdl.suspend(SuspendHow::Reset, vcpu_id) {
                    warn!(log, "reset suspend failed"; "error" => %e);
                }
                return;
            }
            WrmsrOutcome::NotHandled => { /* fall through */ }
        }
    }
    debug!(log, "unhandled wrmsr"; "msr" => format!("{:#x}", code), "val" => format!("{:#x}", val));
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::{should_discard_once, start_gate};

    #[test]
    fn instruction_is_discarded_once_per_repeating_rip() {
        let mut last = None;

        assert!(should_discard_once(0x1234, &mut last));
        assert!(!should_discard_once(0x1234, &mut last));
    }

    #[test]
    fn a_released_gate_lets_the_thread_enter() {
        let (signal, gate) = start_gate();
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            tx.send(gate.wait()).expect("the test holds the receiver");
        });

        assert!(signal.release());
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(true));
        worker.join().expect("gate thread should not panic");
    }

    #[test]
    fn a_dropped_signal_makes_the_thread_exit() {
        // This is the whole rollback for a failed add: there is no
        // vm_deactivate_cpu, so the thread must never enter a guest
        // whose CPU the kernel refused.
        let (signal, gate) = start_gate();
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            tx.send(gate.wait()).expect("the test holds the receiver");
        });

        drop(signal);
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(false));
        worker.join().expect("gate thread should not panic");
    }

    #[test]
    fn releasing_a_gate_whose_thread_is_gone_reports_false() {
        // An activated CPU with no thread breaks the kernel halt
        // count, so the caller has to hear about it.
        let (signal, gate) = start_gate();
        drop(gate);

        assert!(!signal.release());
    }
}
