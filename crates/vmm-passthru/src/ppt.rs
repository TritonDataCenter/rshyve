// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The kernel PPT interface behind a trait.
//!
//! The device logic never touches an ioctl directly, so a test can run
//! it against a recorded backend on a host with no `/dev/pptN`.

use std::fs::File;
use std::io::{ErrorKind, Result};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::Arc;

use bhyve_api::{ioctls, ppt_bar_io, ppt_bar_query, ppt_cfg_io};
use slog::{warn, Logger};

use vmm_core::hdl::{PptdevLimits, VmmHdl};

/// One physical BAR as the kernel reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct BarInfo {
    /// `PCI_ADDR_IO`, `PCI_ADDR_MEM32` or `PCI_ADDR_MEM64`.
    pub bar_type: u32,
    /// Host physical base, or the host port for an I/O BAR.
    pub hpa: u64,
    /// Size in bytes.
    pub size: u64,
}

/// What the device needs from a bound PPT device.
pub(crate) trait PptOps: Send + Sync {
    fn cfg_read(&self, offset: u8, width: u8) -> Result<u32>;
    /// Raw `pci_config_put*`. The kernel applies no filter, so every
    /// caller goes through `classify_cfg_write` first.
    fn cfg_write(&self, offset: u8, width: u8, data: u32) -> Result<()>;
    /// `None` for a slot that holds no BAR, including the high half of
    /// a 64-bit BAR.
    fn bar_query(&self, idx: usize) -> Result<Option<BarInfo>>;
    fn bar_read(&self, bar: usize, offset: u32, width: u8) -> Result<u32>;
    fn bar_write(
        &self,
        bar: usize,
        offset: u32,
        width: u8,
        data: u32,
    ) -> Result<()>;
    fn limits(&self) -> Result<PptdevLimits>;
    fn map_mmio(&self, gpa: u64, hpa: u64, len: u64) -> Result<()>;
    fn unmap_mmio(&self, gpa: u64, len: u64) -> Result<()>;
    /// `numvec` of 0 tears the vectors down.
    fn setup_msi(&self, addr: u64, data: u64, numvec: i32) -> Result<()>;
}

/// A `/dev/pptN` bound to a VM. Dropping it gives the device back.
pub(crate) struct KernelPpt {
    ppt: File,
    hdl: Arc<VmmHdl>,
    log: Logger,
}

impl KernelPpt {
    pub(crate) fn open(
        path: &str,
        hdl: Arc<VmmHdl>,
        log: Logger,
    ) -> Result<Self> {
        // std opens close-on-exec, so a guest-reset re-exec does not
        // inherit the device.
        let ppt = File::options().read(true).write(true).open(path)?;
        hdl.bind_pptdev(ppt.as_fd())?;
        Ok(Self { ppt, hdl, log })
    }

    fn fd(&self) -> BorrowedFd<'_> {
        self.ppt.as_fd()
    }
}

impl Drop for KernelPpt {
    fn drop(&mut self) {
        if let Err(e) = self.hdl.unbind_pptdev(self.fd()) {
            // Nothing else can run here, so the record is the only
            // remedy: the device stays with the VM until it goes.
            warn!(self.log, "passthru: unbind failed, device still held";
                "error" => %e);
        }
    }
}

impl PptOps for KernelPpt {
    fn cfg_read(&self, offset: u8, width: u8) -> Result<u32> {
        let mut io = ppt_cfg_io {
            pci_off: u64::from(offset),
            pci_width: u32::from(width),
            pci_data: 0,
        };
        // SAFETY: ppt_cfg_io is the struct PPT_CFG_READ takes.
        unsafe { ioctl_on_fd(self.fd(), ioctls::PPT_CFG_READ, &mut io) }?;
        Ok(io.pci_data)
    }

    fn cfg_write(&self, offset: u8, width: u8, data: u32) -> Result<()> {
        let mut io = ppt_cfg_io {
            pci_off: u64::from(offset),
            pci_width: u32::from(width),
            pci_data: data,
        };
        // SAFETY: ppt_cfg_io is the struct PPT_CFG_WRITE takes.
        unsafe { ioctl_on_fd(self.fd(), ioctls::PPT_CFG_WRITE, &mut io) }?;
        Ok(())
    }

    fn bar_query(&self, idx: usize) -> Result<Option<BarInfo>> {
        let mut query = ppt_bar_query {
            pbq_baridx: u32::try_from(idx)
                .map_err(|_| ErrorKind::InvalidInput)?,
            pbq_type: 0,
            pbq_base: 0,
            pbq_size: 0,
        };
        // SAFETY: ppt_bar_query is the struct PPT_BAR_QUERY takes.
        match unsafe {
            ioctl_on_fd(self.fd(), ioctls::PPT_BAR_QUERY, &mut query)
        } {
            Ok(_) => Ok(Some(BarInfo {
                bar_type: query.pbq_type,
                hpa: query.pbq_base,
                size: query.pbq_size,
            })),
            // The kernel answers ENOENT for a slot with no BAR.
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn bar_read(&self, bar: usize, offset: u32, width: u8) -> Result<u32> {
        let mut io = ppt_bar_io {
            pbi_bar: u32::try_from(bar).map_err(|_| ErrorKind::InvalidInput)?,
            pbi_off: offset,
            pbi_width: u32::from(width),
            pbi_data: 0,
        };
        // SAFETY: ppt_bar_io is the struct PPT_BAR_READ takes.
        unsafe { ioctl_on_fd(self.fd(), ioctls::PPT_BAR_READ, &mut io) }?;
        Ok(io.pbi_data)
    }

    fn bar_write(
        &self,
        bar: usize,
        offset: u32,
        width: u8,
        data: u32,
    ) -> Result<()> {
        let mut io = ppt_bar_io {
            pbi_bar: u32::try_from(bar).map_err(|_| ErrorKind::InvalidInput)?,
            pbi_off: offset,
            pbi_width: u32::from(width),
            pbi_data: data,
        };
        // SAFETY: ppt_bar_io is the struct PPT_BAR_WRITE takes.
        unsafe { ioctl_on_fd(self.fd(), ioctls::PPT_BAR_WRITE, &mut io) }?;
        Ok(())
    }

    fn limits(&self) -> Result<PptdevLimits> {
        self.hdl.pptdev_limits(self.fd())
    }

    fn map_mmio(&self, gpa: u64, hpa: u64, len: u64) -> Result<()> {
        let len = usize::try_from(len).map_err(|_| ErrorKind::InvalidInput)?;
        self.hdl.map_pptdev_mmio(self.fd(), gpa, hpa, len)
    }

    fn unmap_mmio(&self, gpa: u64, len: u64) -> Result<()> {
        let len = usize::try_from(len).map_err(|_| ErrorKind::InvalidInput)?;
        self.hdl.unmap_pptdev_mmio(self.fd(), gpa, len)
    }

    fn setup_msi(&self, addr: u64, data: u64, numvec: i32) -> Result<()> {
        self.hdl.pptdev_msi(self.fd(), addr, data, numvec)
    }
}

/// Issue an ioctl on the PPT device fd.
///
/// # Safety
///
/// `data` must point at the struct the `cmd` ioctl takes.
unsafe fn ioctl_on_fd<T>(
    fd: BorrowedFd<'_>,
    cmd: i32,
    data: &mut T,
) -> Result<i32> {
    vmm_api_common::ioctl(fd.as_raw_fd(), cmd, std::ptr::from_mut(data).cast())
}
