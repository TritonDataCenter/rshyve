// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The calls a device makes on an open link.
//!
//! One trait covers the whole kernel side of a viona device. The device
//! holds an `Arc<dyn LinkOps>`: the open [`VionaFd`] in production, a
//! test link in a test.
//!
//! Each method sends its ioctl directly, so the test that reads the
//! ioctl reads the code the device calls.
//!
//! The trait lives in this crate because the recording channel, the
//! only handle that works without a kernel, is `#[cfg(test)]` here. The
//! tests in this crate drive these methods, and a production build
//! cannot construct a recording handle.

use std::io::{PipeReader, Result};
use std::os::fd::AsRawFd;

use crate::{
    vioc_intr_poll, vioc_notify_mmio, vioc_ring_init_modern, vioc_ring_msi,
    vioc_ring_state, PromiscMode, VionaFd, VIONA_VQ_MAX, VNA_IOC_DELETE,
    VNA_IOC_INTR_POLL, VNA_IOC_RING_GET_STATE, VNA_IOC_RING_INIT_MODERN,
    VNA_IOC_RING_INTR_CLR, VNA_IOC_RING_KICK, VNA_IOC_RING_PAUSE,
    VNA_IOC_RING_RESET, VNA_IOC_RING_SET_MSI, VNA_IOC_RING_SET_STATE,
    VNA_IOC_SET_FEATURES, VNA_IOC_SET_NOTIFY_IOP, VNA_IOC_SET_NOTIFY_MMIO,
    VNA_IOC_SET_PROMISC,
};

#[cfg(test)]
mod test;

/// The poll band in which viona signals a pending interrupt.
///
/// The wait requests and tests the same constant, so the two cannot
/// differ. Ordinary readability (`POLLIN`) means nothing here.
const INTR_BAND: libc::c_short = libc::POLLRDBAND;

/// The `VNA_IOC_INTR_POLL` output: the pending state of every ring, in
/// ring order.
pub type IntrStatus = [u32; VIONA_VQ_MAX as usize];

/// The result of one wait for the kernel interrupt signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntrWait {
    /// One or more rings have a pending interrupt.
    Pending,
    /// The halt asked the thread to exit, or the link is gone. Nothing
    /// more arrives.
    Stopped,
}

/// The kernel side of a viona device.
///
/// The trait covers what the callers need, not one method per ioctl.
pub trait LinkOps: Send + Sync {
    /// Program one ring's addresses in the modern (virtio 1.0) layout.
    ///
    /// The kernel starts a worker for the ring, so nothing may call
    /// this for a link the halt destroyed. The ring indices go to 0, so
    /// a migration destination uses [`Self::ring_set_state`] instead.
    fn ring_init(
        &self,
        ring: u16,
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
    ) -> Result<()>;

    /// Forward the guest kick for one ring to its kernel worker.
    ///
    /// An unprogrammed ring has no worker, and the kernel refuses it.
    fn ring_kick(&self, ring: u16) -> Result<()>;

    /// Return one ring to its reset state.
    ///
    /// [`Self::delete`] also resets every ring, in a wait that ignores
    /// signals. That wait returns at once for a ring already reset, so
    /// each reset here removes one wait from the delete. This wait is
    /// interruptible and can return `EINTR`.
    fn ring_reset(&self, ring: u16) -> Result<()>;

    /// Stop one ring's worker and keep its state.
    ///
    /// A migration source pauses each ring before it reads the indices,
    /// so the kernel does not change them.
    fn ring_pause(&self, ring: u16) -> Result<()>;

    /// Program one ring's addresses and its indices together.
    ///
    /// A migration destination resumes the kernel where the source
    /// stopped. [`Self::ring_init`] cannot.
    fn ring_set_state(&self, state: &vioc_ring_state) -> Result<()>;

    /// Read one ring's addresses and indices.
    ///
    /// A migration source sends the indices to the destination.
    fn ring_get_state(&self, ring: u16) -> Result<vioc_ring_state>;

    /// Route one ring's interrupt to an MSI-X message.
    ///
    /// The kernel then sends the message itself, and the userspace poll
    /// thread raises nothing for that ring.
    fn ring_set_msi(&self, ring: u16, addr: u64, msg: u64) -> Result<()>;

    /// Give the kernel the features the guest accepted.
    ///
    /// `VIRTIO_F_VERSION_1` selects the modern ring layout.
    fn set_features(&self, features: u64) -> Result<()>;

    /// Read which rings have an interrupt pending.
    ///
    /// One entry per ring, non-zero for a ring with pending work.
    fn intr_status(&self) -> Result<IntrStatus>;

    /// Clear one ring's pending interrupt.
    ///
    /// Until this runs, the kernel signals the same interrupt again and
    /// the poll thread spins. The clear also gives the notification to
    /// the poll thread for delivery.
    fn ring_intr_clr(&self, ring: u16) -> Result<()>;

    /// Wait until the kernel has interrupts pending, or the halt
    /// closes the write end of `stop`.
    ///
    /// The wait has no deadline, so only that descriptor ends it on
    /// demand.
    fn wait_intr(&self, stop: &PipeReader) -> Result<IntrWait>;

    /// Name the I/O port the guest writes to kick a ring.
    ///
    /// The kernel then receives the kick directly, with no vCPU exit to
    /// userspace.
    fn set_notify_iop(&self, port: u16) -> Result<()>;

    /// Name the memory address the guest writes to kick a ring.
    ///
    /// The modern transport counterpart of [`Self::set_notify_iop`].
    fn set_notify_mmio(&self, addr: u64, size: u32) -> Result<()>;

    /// Set which traffic the link passes to the guest.
    fn set_promisc(&self, mode: PromiscMode) -> Result<()>;

    /// Destroy the link, which releases its `vmm_drv` hold.
    ///
    /// Teardown must issue this. Viona holds the VM for the life of the
    /// link (`vmm_drv_hold`). `VM_DESTROY_SELF` purges the holds, then
    /// `vmm_drv_purge` waits for every lease to break in an untimed
    /// `cv_wait`. Releasing the last hold first clears `VMM_HELD`, and
    /// the purge skips that wait.
    ///
    /// The call waits for each ring worker to stop, in a `cv_wait` that
    /// ignores signals (`viona_ring_reset`). Run it only where a lost
    /// thread is acceptable.
    fn delete(&self) -> Result<()>;
}

/// The link a VM runs on.
///
/// Not gated on illumos: only illumos can open the handle, and the
/// portable build lints and type-checks what production sends.
impl LinkOps for VionaFd {
    fn ring_init(
        &self,
        ring: u16,
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
    ) -> Result<()> {
        let mut rim = vioc_ring_init_modern {
            rim_index: ring,
            rim_qsize: size,
            rim_qaddr_desc: desc,
            rim_qaddr_avail: avail,
            rim_qaddr_used: used,
            ..Default::default()
        };
        // Safety: the command copies in one vioc_ring_init_modern, and
        // that is what `rim` is.
        unsafe { self.ioctl(VNA_IOC_RING_INIT_MODERN, &mut rim) }?;
        Ok(())
    }

    fn ring_kick(&self, ring: u16) -> Result<()> {
        self.ioctl_usize(VNA_IOC_RING_KICK, ring as usize)?;
        Ok(())
    }

    fn ring_reset(&self, ring: u16) -> Result<()> {
        self.ioctl_usize(VNA_IOC_RING_RESET, ring as usize)?;
        Ok(())
    }

    fn ring_pause(&self, ring: u16) -> Result<()> {
        self.ioctl_usize(VNA_IOC_RING_PAUSE, ring as usize)?;
        Ok(())
    }

    fn ring_set_state(&self, state: &vioc_ring_state) -> Result<()> {
        let mut state = *state;
        // Safety: the command copies in one vioc_ring_state, and that
        // is what `state` is.
        unsafe { self.ioctl(VNA_IOC_RING_SET_STATE, &mut state) }?;
        Ok(())
    }

    fn ring_get_state(&self, ring: u16) -> Result<vioc_ring_state> {
        let mut state = vioc_ring_state {
            vrs_index: ring,
            ..Default::default()
        };
        // Safety: the command copies out one vioc_ring_state, and that
        // is what `state` is.
        unsafe { self.ioctl(VNA_IOC_RING_GET_STATE, &mut state) }?;
        Ok(state)
    }

    fn ring_set_msi(&self, ring: u16, addr: u64, msg: u64) -> Result<()> {
        let mut msi = vioc_ring_msi {
            rm_index: ring,
            rm_addr: addr,
            rm_msg: msg,
            ..Default::default()
        };
        // Safety: the command copies in one vioc_ring_msi, and that is
        // what `msi` is.
        unsafe { self.ioctl(VNA_IOC_RING_SET_MSI, &mut msi) }?;
        Ok(())
    }

    fn set_features(&self, features: u64) -> Result<()> {
        let mut feat = features;
        // Safety: the command copies in one u64, and that is what
        // `feat` is.
        unsafe { self.ioctl(VNA_IOC_SET_FEATURES, &mut feat) }?;
        Ok(())
    }

    fn intr_status(&self) -> Result<IntrStatus> {
        let mut poll_data = vioc_intr_poll::default();
        // Safety: the command copies out one vioc_intr_poll, and that
        // is what `poll_data` is.
        unsafe { self.ioctl(VNA_IOC_INTR_POLL, &mut poll_data) }?;
        Ok(poll_data.vip_status)
    }

    fn ring_intr_clr(&self, ring: u16) -> Result<()> {
        self.ioctl_usize(VNA_IOC_RING_INTR_CLR, ring as usize)?;
        Ok(())
    }

    /// Wait for `POLLRDBAND`, viona's signal that interrupts are
    /// pending.
    fn wait_intr(&self, stop: &PipeReader) -> Result<IntrWait> {
        loop {
            let mut pfds = [
                libc::pollfd {
                    fd: self.as_raw_fd(),
                    events: INTR_BAND,
                    revents: 0,
                },
                libc::pollfd {
                    fd: stop.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // Safety: both descriptors are open for this call, and the
            // count matches the array.
            let ret = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };
            if ret < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                // The device is closed or the error is fatal.
                return Err(err);
            }
            match poll_verdict(pfds[0].revents, pfds[1].revents) {
                Some(end) => return Ok(end),
                // With no deadline, poll(2) returns only for a requested
                // event, and each has a verdict. A wakeup here would
                // repeat at once.
                None => {
                    return Err(std::io::Error::other(
                        "the viona wait woke with no event it asked for",
                    ))
                }
            }
        }
    }

    fn set_notify_iop(&self, port: u16) -> Result<()> {
        self.ioctl_usize(VNA_IOC_SET_NOTIFY_IOP, port as usize)?;
        Ok(())
    }

    fn set_notify_mmio(&self, addr: u64, size: u32) -> Result<()> {
        let mut vim = vioc_notify_mmio {
            vim_address: addr,
            vim_size: size,
            ..Default::default()
        };
        // Safety: the command copies in one vioc_notify_mmio, and that
        // is what `vim` is.
        unsafe { self.ioctl(VNA_IOC_SET_NOTIFY_MMIO, &mut vim) }?;
        Ok(())
    }

    /// The mode goes by value: `viona_ioc_set_promisc` copies nothing
    /// in and casts the data argument to its enum. A pointer would set
    /// the mode to an address.
    fn set_promisc(&self, mode: PromiscMode) -> Result<()> {
        self.ioctl_usize(VNA_IOC_SET_PROMISC, mode as usize)?;
        Ok(())
    }

    fn delete(&self) -> Result<()> {
        self.ioctl_usize(VNA_IOC_DELETE, 0)?;
        Ok(())
    }
}

/// The meaning of one `poll(2)` wakeup, or `None` for a wakeup with no
/// known event.
///
/// `link` and `stop` are the events each descriptor reported. The stop
/// descriptor is checked first: the halt closes the write end, so it
/// stays ready, and the poll thread must exit before it touches a link
/// that the halt destroys next.
///
/// The caller treats `None` as an error, because every requested event
/// has a verdict here.
fn poll_verdict(link: libc::c_short, stop: libc::c_short) -> Option<IntrWait> {
    if stop != 0 {
        return Some(IntrWait::Stopped);
    }
    if link & INTR_BAND != 0 {
        return Some(IntrWait::Pending);
    }
    // None of these clears by itself, so another wait would spin.
    if link & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        return Some(IntrWait::Stopped);
    }
    None
}
