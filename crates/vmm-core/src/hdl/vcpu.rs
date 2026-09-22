// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The per-vCPU ioctls.
//!
//! [`crate::vcpu::Vcpu`] is the public face of one vCPU. It carries the
//! vCPU id and calls these, so the raw ioctl never leaves the handle.

use std::io::Result;

use bhyve_api::{
    ioctls, seg_desc, vcpu_reset_kind, vm_entry, vm_exception, vm_reg_name,
    vm_register, vm_run_state, vm_seg_desc, vm_vcpu_reset,
};

use super::VmmHdl;

impl VmmHdl {
    /// Allocate the kernel state for `cpuid`. Runs once per vCPU.
    pub(crate) fn vcpu_activate(&self, cpuid: i32) -> Result<()> {
        let mut cpu = cpuid;
        // Safety: VM_ACTIVATE_CPU reads one int.
        unsafe { self.inner.ioctl(ioctls::VM_ACTIVATE_CPU, &mut cpu) }?;
        Ok(())
    }

    pub(crate) fn vcpu_set_reg(
        &self,
        cpuid: i32,
        reg: vm_reg_name,
        val: u64,
    ) -> Result<()> {
        let mut vr = vm_register {
            cpuid,
            regnum: reg as i32,
            regval: val,
        };
        // Safety: vm_register is the struct VM_SET_REGISTER reads.
        unsafe { self.inner.ioctl(ioctls::VM_SET_REGISTER, &mut vr) }?;
        Ok(())
    }

    pub(crate) fn vcpu_get_reg(
        &self,
        cpuid: i32,
        reg: vm_reg_name,
    ) -> Result<u64> {
        let mut vr = vm_register {
            cpuid,
            regnum: reg as i32,
            regval: 0,
        };
        // Safety: vm_register is the struct VM_GET_REGISTER fills.
        unsafe { self.inner.ioctl(ioctls::VM_GET_REGISTER, &mut vr) }?;
        Ok(vr.regval)
    }

    /// Queue an exception for the next entry. `error_code` of `None`
    /// pushes no error code. `restart` rewinds RIP so the faulting
    /// instruction runs again.
    pub(crate) fn vcpu_inject_exception(
        &self,
        cpuid: i32,
        vector: i32,
        error_code: Option<u32>,
        restart: bool,
    ) -> Result<()> {
        let mut exc = vm_exception {
            cpuid,
            vector,
            error_code: error_code.unwrap_or(0),
            error_code_valid: i32::from(error_code.is_some()),
            restart_instruction: i32::from(restart),
        };
        // Safety: vm_exception is the struct VM_INJECT_EXCEPTION reads.
        unsafe { self.inner.ioctl(ioctls::VM_INJECT_EXCEPTION, &mut exc) }?;
        Ok(())
    }

    /// The run state and SIPI vector of `cpuid`.
    pub(crate) fn vcpu_run_state(&self, cpuid: i32) -> Result<(u32, u8)> {
        let mut rs = vm_run_state {
            vcpuid: cpuid,
            state: 0,
            sipi_vector: 0,
            _pad: [0; 3],
        };
        // Safety: vm_run_state is the struct VM_GET_RUN_STATE fills.
        unsafe { self.inner.ioctl(ioctls::VM_GET_RUN_STATE, &mut rs) }?;
        Ok((rs.state, rs.sipi_vector))
    }

    pub(crate) fn vcpu_set_run_state(
        &self,
        cpuid: i32,
        state: u32,
        sipi_vector: u8,
    ) -> Result<()> {
        let mut rs = vm_run_state {
            vcpuid: cpuid,
            state,
            sipi_vector,
            _pad: [0; 3],
        };
        // Safety: vm_run_state is the struct VM_SET_RUN_STATE reads.
        unsafe { self.inner.ioctl(ioctls::VM_SET_RUN_STATE, &mut rs) }?;
        Ok(())
    }

    /// Put `cpuid` in its power-on state.
    pub(crate) fn vcpu_reset(&self, cpuid: i32) -> Result<()> {
        let mut vvr = vm_vcpu_reset {
            vcpuid: cpuid,
            kind: vcpu_reset_kind::VRK_RESET as u32,
        };
        // Safety: vm_vcpu_reset is the struct VM_RESET_CPU reads.
        unsafe { self.inner.ioctl(ioctls::VM_RESET_CPU, &mut vvr) }?;
        Ok(())
    }

    pub(crate) fn vcpu_set_segment_desc(
        &self,
        cpuid: i32,
        reg: vm_reg_name,
        desc: &seg_desc,
    ) -> Result<()> {
        let mut vsd = vm_seg_desc {
            cpuid,
            regnum: reg as i32,
            desc: *desc,
        };
        // Safety: vm_seg_desc is the struct VM_SET_SEGMENT_DESCRIPTOR
        // reads.
        unsafe {
            self.inner
                .ioctl(ioctls::VM_SET_SEGMENT_DESCRIPTOR, &mut vsd)
        }?;
        Ok(())
    }

    pub(crate) fn vcpu_get_segment_desc(
        &self,
        cpuid: i32,
        reg: vm_reg_name,
    ) -> Result<seg_desc> {
        let mut vsd = vm_seg_desc {
            cpuid,
            regnum: reg as i32,
            desc: seg_desc::default(),
        };
        // Safety: vm_seg_desc is the struct VM_GET_SEGMENT_DESCRIPTOR
        // fills.
        unsafe {
            self.inner
                .ioctl(ioctls::VM_GET_SEGMENT_DESCRIPTOR, &mut vsd)
        }?;
        Ok(vsd.desc)
    }

    /// Run the vCPU `entry` names until it exits.
    ///
    /// # Safety
    ///
    /// `entry.exit_data` must point at a `vm_exit` that outlives the
    /// call; the kernel writes the exit there.
    pub(crate) unsafe fn vcpu_run(&self, entry: &mut vm_entry) -> Result<()> {
        self.inner.ioctl(ioctls::VM_RUN, entry)?;
        Ok(())
    }
}
