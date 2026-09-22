// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-vCPU execution context.
//!
//! Each vCPU runs in its own OS thread and calls [`Vcpu::enter`] in a
//! loop. The kernel returns control on VM exits (I/O, MMIO, MSR, halt,
//! etc.), which the caller dispatches to the device handler.
//!
//! # Thread safety
//!
//! The bhyve kernel enforces that only one thread may call VM_RUN for
//! a given vCPU at a time. This type does not enforce that invariant
//! in Rust. The caller must ensure single-threaded access per vCPU.

use std::sync::Arc;

use bhyve_api::{seg_desc, vm_exit, vm_reg_name, VRS_RUN};

use crate::exits::{VmEntry, VmExit};
use crate::hdl::VmmHdl;

/// Why a vCPU could not be activated.
#[derive(Debug, thiserror::Error)]
pub enum ActivateError {
    /// The kernel answered EBUSY: the id is active already, or the VM
    /// was suspended. Nothing here can tell the two apart.
    #[error("the vCPU is active or the VM is suspended")]
    Busy,
    /// The kernel answered EINVAL: no such vCPU on this VM.
    #[error("the vCPU id is out of range")]
    OutOfRange,
    #[error(transparent)]
    Os(std::io::Error),
}

/// Why an entry into the guest returned no exit.
#[derive(Debug, thiserror::Error)]
pub enum EnterError {
    /// A signal landed. Enter again.
    #[error("the entry was interrupted by a signal")]
    Interrupted,
    /// The VM is paused. Wait, then enter again.
    #[error("the VM is paused")]
    Paused,
    #[error(transparent)]
    Os(std::io::Error),
}

/// Represents a single virtual CPU.
///
/// Provides methods to activate, configure registers, and run the vCPU.
/// The vCPU must be activated before it can be run.
pub struct Vcpu {
    id: i32,
    hdl: Arc<VmmHdl>,
}

impl Vcpu {
    pub(crate) fn new(id: i32, hdl: Arc<VmmHdl>) -> Self {
        Self { id, hdl }
    }

    /// Create a vCPU handle for use in a dedicated thread.
    ///
    /// The handle shares the VmmHdl, so it can move to a thread that
    /// calls [`enter`] in a loop.
    pub fn new_for_thread(id: i32, hdl: Arc<VmmHdl>) -> Self {
        Self { id, hdl }
    }

    pub fn id(&self) -> i32 {
        self.id
    }

    pub fn hdl(&self) -> &Arc<VmmHdl> {
        &self.hdl
    }

    /// Activate this vCPU in the kernel.
    ///
    /// Must be called before [`enter`]. The kernel allocates per-vCPU
    /// state on activation.
    pub fn activate(&self) -> Result<(), ActivateError> {
        self.hdl
            .vcpu_activate(self.id)
            .map_err(|e| match e.raw_os_error() {
                Some(libc::EBUSY) => ActivateError::Busy,
                Some(libc::EINVAL) => ActivateError::OutOfRange,
                _ => ActivateError::Os(e),
            })
    }

    // ---------------------------------------------------------------
    // Register access
    // ---------------------------------------------------------------

    /// Set a guest register value.
    pub fn set_reg(&self, reg: vm_reg_name, val: u64) -> std::io::Result<()> {
        self.hdl.vcpu_set_reg(self.id, reg, val)
    }

    /// Inject a `#GP(0)` (general protection) exception on the next
    /// re-entry. Used for spec-mandated MSR access faults, for example
    /// a write of a Hyper-V MSR with reserved bits set.
    ///
    /// The kernel does not rewind RIP: the WRMSR/RDMSR is already
    /// decoded at this point, and the guest expects to see the fault at
    /// the next instruction (TLFS 14.6).
    pub fn inject_gp(&self) -> std::io::Result<()> {
        self.hdl.vcpu_inject_exception(self.id, 13, Some(0), false)
    }

    /// Get a guest register value.
    pub fn get_reg(&self, reg: vm_reg_name) -> std::io::Result<u64> {
        self.hdl.vcpu_get_reg(self.id, reg)
    }

    /// Get the vCPU run state and SIPI vector.
    pub fn get_run_state(&self) -> std::io::Result<(u32, u8)> {
        self.hdl.vcpu_run_state(self.id)
    }

    /// Set the vCPU run state (e.g., VRS_RUN to mark it runnable).
    pub fn set_run_state(&self, state: u32) -> std::io::Result<()> {
        self.set_run_state_full(state, 0)
    }

    /// Set the vCPU run state with SIPI vector.
    pub fn set_run_state_full(
        &self,
        state: u32,
        sipi_vector: u8,
    ) -> std::io::Result<()> {
        self.hdl.vcpu_set_run_state(self.id, state, sipi_vector)
    }

    /// Reset all vCPU state to x86 power-on defaults.
    ///
    /// Issues `VM_RESET_CPU`, which sets all registers, segment
    /// descriptors and pending interrupts to their x86 reset values.
    pub fn reboot_state(&self) -> std::io::Result<()> {
        self.hdl.vcpu_reset(self.id)
    }

    /// Set a guest segment descriptor (CS, DS, SS, ES, FS, GS, TR, LDTR,
    /// GDTR, IDTR).
    pub fn set_segment_desc(
        &self,
        reg: vm_reg_name,
        desc: &seg_desc,
    ) -> std::io::Result<()> {
        self.hdl.vcpu_set_segment_desc(self.id, reg, desc)
    }

    /// Get a guest segment descriptor (CS, DS, SS, ES, FS, GS, TR, LDTR,
    /// GDTR, IDTR).
    pub fn get_segment_desc(
        &self,
        reg: vm_reg_name,
    ) -> std::io::Result<seg_desc> {
        self.hdl.vcpu_get_segment_desc(self.id, reg)
    }

    /// Set up the BSP (bootstrap processor, vCPU 0) for initial boot.
    ///
    /// Initializes x86 reset state, then sets:
    /// - Run state = RUN
    /// - RIP = 0xFFF0 (reset vector offset within CS segment)
    ///
    /// After `VM_RESET_CPU`, CS base is 0xFFFF_0000, so the first
    /// instruction fetched is at physical address 0xFFFF_FFF0. That is
    /// the x86 reset vector, and the bootrom must cover it.
    pub fn setup_bsp(&self) -> std::io::Result<()> {
        self.reboot_state()?;
        self.set_run_state(VRS_RUN)?;
        self.set_reg(vm_reg_name::VM_REG_GUEST_RIP, 0xfff0)?;
        Ok(())
    }

    // ---------------------------------------------------------------
    // Execution
    // ---------------------------------------------------------------

    /// Enter the guest with a specific entry command and return the
    /// parsed VM exit.
    ///
    /// The caller builds a [`VmEntry`] (usually `VmEntry::Run` for the
    /// first entry, then `VmEntry::InoutFulfill` or
    /// `VmEntry::MmioFulfill` after it handles an exit). This method
    /// returns at the next exit.
    pub fn enter(
        &self,
        entry: &VmEntry,
        api_version: u32,
    ) -> Result<VmExit, EnterError> {
        let mut raw_exit = vm_exit::default();
        let mut raw_entry =
            entry.to_raw(self.id, &mut raw_exit as *mut vm_exit);

        // Safety: raw_entry.exit_data points at raw_exit, which is
        // stack-local and outlives the call.
        unsafe { self.hdl.vcpu_run(&mut raw_entry) }.map_err(|e| {
            match e.raw_os_error() {
                Some(libc::EINTR) => EnterError::Interrupted,
                Some(libc::EBUSY) => EnterError::Paused,
                _ => EnterError::Os(e),
            }
        })?;

        Ok(VmExit::parse(&raw_exit, api_version))
    }
}
