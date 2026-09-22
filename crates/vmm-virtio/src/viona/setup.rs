// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Link open and kernel notification setup.

use std::os::fd::BorrowedFd;
use std::sync::{Arc, Mutex};

use viona_api::{LinkOps, PromiscMode};

use super::{
    PausedRing, RingState, VionaInner, VirtioViona, ETHERADDRL, NET_NUM_QUEUES,
};
use crate::pci::intr::IntrSlot;

impl VirtioViona {
    /// Create a new viona-backed virtio-net device.
    ///
    /// `vnic_name` is the VNIC (for example "vnic0"). `vm` is the VMM
    /// instance the link serves.
    ///
    /// # Errors
    ///
    /// Fails if the platform has no viona driver, if the VNIC link ID
    /// does not resolve, or if the viona device does not open and bind
    /// to the VM.
    pub fn new(vnic_name: &str, vm: BorrowedFd<'_>) -> std::io::Result<Self> {
        let (link, mac_addr, dev_features) = open_link(vnic_name, vm)?;
        Ok(Self::with_link(link, mac_addr, dev_features))
    }

    /// A device around one link, before the guest has programmed it.
    pub(super) fn with_link(
        link: Arc<dyn LinkOps>,
        mac_addr: [u8; ETHERADDRL],
        dev_features: u32,
    ) -> Self {
        Self {
            inner: Mutex::new(VionaInner {
                link,
                mac_addr,
                dev_features,
                negotiated_features: 0,
                paused_rings: [PausedRing::Running; NET_NUM_QUEUES],
                ring_state: [RingState::Init; NET_NUM_QUEUES],
                kick_refused: [false; NET_NUM_QUEUES],
                deferred_poll: None,
                poller: None,
                log: None,
                halted: false,
            }),
            interrupt: Arc::new(IntrSlot::new()),
        }
    }

    /// Program the notification I/O port and MMIO address in the kernel.
    ///
    /// The kernel then receives the guest ring kick directly, with no
    /// vCPU exit to userspace.
    pub fn set_notify_addrs(&self, pio_port: u16, mmio_addr: u64) {
        let inner = self.inner.lock().expect("viona lock");
        // A migration restore can arrive after the halt. The halt holds
        // no device lock while it destroys the link, so an ioctl here
        // could wait for an untimed kernel call with the lock held.
        if inner.halted {
            return;
        }
        if pio_port != 0 {
            if let Err(e) = inner.link_ref().set_notify_iop(pio_port) {
                slog::warn!(inner.log(), "viona set notify port failed";
                    "port" => pio_port, "error" => %e);
            }
        }
        if mmio_addr != 0 {
            let rc = inner.link_ref().set_notify_mmio(mmio_addr, 0x1000);
            if let Err(e) = rc {
                slog::warn!(inner.log(), "viona set notify mmio failed";
                    "addr" => mmio_addr, "error" => %e);
            }
        }
    }

    /// Enable promiscuous mode on the viona device.
    ///
    /// With `enable`, the VNIC receives all physical traffic. VMs with
    /// `allow_mac_spoofing=true` in the zone config need this.
    pub fn set_promisc(&self, enable: bool) -> std::io::Result<()> {
        let mode = if enable {
            PromiscMode::All
        } else {
            PromiscMode::None
        };
        let inner = self.inner.lock().expect("viona lock");
        // Unlike the other halted guards, this one returns an error: the
        // caller must not believe the guest receives every frame.
        if inner.halted {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "the viona link is destroyed",
            ));
        }
        inner.link_ref().set_promisc(mode)
    }
}

/// Open the kernel link for `vnic_name` and read what it offers.
///
/// Returns the features with the link because the device does not hold
/// the handle yet.
fn open_link(
    vnic_name: &str,
    vm: BorrowedFd<'_>,
) -> std::io::Result<(Arc<dyn LinkOps>, [u8; ETHERADDRL], u32)> {
    let vnic = dladm::Handle::open()?.vnic(vnic_name)?;

    let viona_fd = viona_api::VionaFd::new(vnic.link_id, vm).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!(
                "failed to create viona instance for '{}' (link_id={}): {}",
                vnic_name, vnic.link_id, e
            ),
        )
    })?;

    let dev_features = viona_fd.get_features().map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("VNA_IOC_GET_FEATURES failed: {}", e),
        )
    })?;

    // The legacy transport carries only the low 32 bits.
    Ok((Arc::new(viona_fd), vnic.mac_addr, dev_features as u32))
}
