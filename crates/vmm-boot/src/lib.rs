// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest boot protocols shared by every binary in this workspace.

mod bsp;
pub mod bytes;
pub mod direct;
pub mod elf;
pub mod image;
pub mod initrd;
pub mod pvh;

use bhyve_api::{seg_desc, vm_reg_name};
use vmm_core::vcpu::Vcpu;

// Cross-crate callers name these at the crate root. Re-export here so
// the module layout inside this crate can change later without
// breaking any of them.
pub use bytes::ImageBytes;
pub use direct::{
    load_kernel, setup_direct_boot_bsp, InitrdImage, KernelImage,
};
pub use elf::PvhKernel;
pub use image::{detect_boot_protocol, BootImage, BootProtocol};
pub use initrd::{place_initrd, LOWMEM_LIMIT};
pub use pvh::{load_pvh_kernel, setup_pvh_bsp};

/// The vCPU operations a boot protocol needs to place a guest at its
/// entry point.
///
/// A boot protocol writes registers and then makes the vCPU runnable.
/// It never runs the guest, so these four are the whole surface. A vCPU
/// from a different hypervisor that supplies them boots the same kernel
/// through the same code, with no copy of the protocol.
///
/// The registers keep bhyve's `vm_reg_name` and `seg_desc` names, so the
/// trait is generic over the vCPU but not over the register names.
///
/// `std::io::Result` is what [`Vcpu`] already returns, so its
/// implementation only forwards, and `?` in a protocol body still
/// widens the error into `anyhow::Error`.
///
/// The methods take `&self` because every caller holds a shared
/// reference to the vCPU. An implementation that batches its register
/// writes must use interior mutability.
pub trait BootVcpu {
    /// Reset the vCPU to the x86 power-on state.
    fn reboot_state(&self) -> std::io::Result<()>;

    /// Set one guest register.
    fn set_reg(&self, reg: vm_reg_name, val: u64) -> std::io::Result<()>;

    /// Set one guest segment descriptor.
    fn set_segment_desc(
        &self,
        reg: vm_reg_name,
        desc: &seg_desc,
    ) -> std::io::Result<()>;

    /// Set the run state, for example `bhyve_api::VRS_RUN`.
    fn set_run_state(&self, state: u32) -> std::io::Result<()>;
}

impl BootVcpu for Vcpu {
    fn reboot_state(&self) -> std::io::Result<()> {
        Vcpu::reboot_state(self)
    }

    fn set_reg(&self, reg: vm_reg_name, val: u64) -> std::io::Result<()> {
        Vcpu::set_reg(self, reg, val)
    }

    fn set_segment_desc(
        &self,
        reg: vm_reg_name,
        desc: &seg_desc,
    ) -> std::io::Result<()> {
        Vcpu::set_segment_desc(self, reg, desc)
    }

    fn set_run_state(&self, state: u32) -> std::io::Result<()> {
        Vcpu::set_run_state(self, state)
    }
}

/// A [`BootVcpu`] that records every write in order.
///
/// The boot protocols are the one part of this crate that cannot run
/// against a real vCPU in a unit test: `Vcpu` needs `/dev/vmm`. The
/// recorder gives the protocol tests the sequence the guest actually
/// sees.
#[cfg(test)]
pub(crate) mod recording {
    use std::sync::Mutex;

    use bhyve_api::{seg_desc, vm_reg_name};

    use super::BootVcpu;

    /// One vCPU write. The register is its `Debug` name, so a failing
    /// assert names the register instead of a discriminant.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Write {
        Reboot,
        /// Register, value.
        Reg(String, u64),
        /// Register, base, limit, access.
        Desc(String, u64, u32, u32),
        RunState(u32),
    }

    pub(crate) fn reg(reg: vm_reg_name, val: u64) -> Write {
        Write::Reg(format!("{reg:?}"), val)
    }

    pub(crate) fn desc(
        reg: vm_reg_name,
        base: u64,
        limit: u32,
        access: u32,
    ) -> Write {
        Write::Desc(format!("{reg:?}"), base, limit, access)
    }

    /// Report the first write that differs, then the count. A whole-vec
    /// `assert_eq!` buries the one bad entry in fifty good ones.
    pub(crate) fn assert_sequence(got: &[Write], want: &[Write]) {
        for (i, (g, w)) in got.iter().zip(want).enumerate() {
            assert_eq!(g, w, "write {i}");
        }
        assert_eq!(got.len(), want.len(), "write count");
    }

    /// The `Mutex` shows that `&self` is enough for an implementation
    /// that keeps state.
    #[derive(Default)]
    pub(crate) struct RecordingVcpu {
        writes: Mutex<Vec<Write>>,
    }

    impl RecordingVcpu {
        pub(crate) fn writes(&self) -> Vec<Write> {
            self.writes.lock().expect("recorder lock").clone()
        }

        fn push(&self, write: Write) -> std::io::Result<()> {
            self.writes.lock().expect("recorder lock").push(write);
            Ok(())
        }
    }

    impl BootVcpu for RecordingVcpu {
        fn reboot_state(&self) -> std::io::Result<()> {
            self.push(Write::Reboot)
        }

        fn set_reg(&self, name: vm_reg_name, val: u64) -> std::io::Result<()> {
            self.push(reg(name, val))
        }

        fn set_segment_desc(
            &self,
            name: vm_reg_name,
            sd: &seg_desc,
        ) -> std::io::Result<()> {
            self.push(desc(name, sd.base, sd.limit, sd.access))
        }

        fn set_run_state(&self, state: u32) -> std::io::Result<()> {
            self.push(Write::RunState(state))
        }
    }
}
