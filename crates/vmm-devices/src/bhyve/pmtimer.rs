// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bhyve kernel-emulated ACPI PM timer.
//!
//! The PM timer runs in the kernel. Userspace sets its PIO port with
//! the VM_PMTMR_LOCATE ioctl.

use std::sync::Arc;

use vmm_core::hdl::VmmHdl;

/// The ACPI PM base port, as C bhyve's `IO_PMTMR`.
pub const PMBASE_DEFAULT: u16 = 0x400;

const PM_TMR_OFFSET: u16 = 0x08;

pub struct BhyvePmTimer {
    hdl: Arc<VmmHdl>,
    port: u16,
}

impl BhyvePmTimer {
    pub fn create(hdl: Arc<VmmHdl>, pmbase: u16) -> Arc<Self> {
        let port = pmbase + PM_TMR_OFFSET;
        Arc::new(Self { hdl, port })
    }

    /// Tell the kernel which PIO port the PM timer uses. Call this
    /// before the vCPUs start. The kernel then answers reads of the
    /// port without an exit to userspace.
    pub fn attach(&self) -> std::io::Result<()> {
        self.hdl.pmtmr_locate(self.port)
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}
