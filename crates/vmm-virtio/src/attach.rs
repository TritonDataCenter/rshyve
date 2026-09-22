// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared virtio device construction for the binaries' device catalogs.
//!
//! Both binaries build the same virtio devices. Only the set of
//! accepted driver names differs. The construction is here so the
//! interrupt back-wire, which needs the transport, has one
//! implementation.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;

use vmm_core::hdl::VmmHdl;
use vmm_core::mem::PhysMap;
use vmm_core::mmio::MmioBus;
use vmm_core::pio::PioBus;

use vmm_devices::pci::msix::{HdlMsiSink, MsiSink, MsixTable};
use vmm_devices::pci::LintrCfg;

use super::bits;
use super::block::{VirtioBlock, VirtioBlockOpts};
use super::console::{
    VirtioConsole, CONSOLE_MSIX_VECTORS, CONSOLE_NUM_QUEUES, CONSOLE_QUEUE_SIZE,
};
use super::fs::{
    VirtioFs, VirtioFsOpts, FS_CONFIG_SIZE, FS_MSIX_VECTORS, FS_NUM_QUEUES,
};
use super::pci::VirtioPciDevice;
use super::rng::VirtioRng;
use super::viona;
use super::viona::VirtioViona;

/// Machine-owned handles every virtio device needs. The field types
/// match `PciDeviceCtx`, so a catalog builds one from plain borrows.
pub struct VirtioAttachCtx<'a> {
    pub physmap: &'a Arc<PhysMap>,
    pub bus_pio: &'a Arc<PioBus>,
    pub bus_mmio: &'a Arc<MmioBus>,
    pub hdl: &'a Arc<VmmHdl>,
}

impl VirtioAttachCtx<'_> {
    /// The MSI-X delivery path for a device built from this context.
    ///
    /// The raw handle stays in the context because viona needs the VM
    /// file descriptor. All other devices use only this sink.
    pub fn msi_sink(&self) -> Arc<dyn MsiSink> {
        HdlMsiSink::new(Arc::clone(self.hdl))
    }
}

/// Build a virtio-blk device and its PCI transport.
///
/// The caller routes the INTx pin before this call and attaches the
/// returned device to the chipset after it, so this function names no
/// chipset type.
pub fn virtio_blk(
    path: &Path,
    opts: &VirtioBlockOpts,
    num_vcpus: u32,
    lintr: Option<LintrCfg>,
    ctx: &VirtioAttachCtx<'_>,
) -> anyhow::Result<Arc<VirtioPciDevice<VirtioBlock>>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(!opts.read_only)
        .open(path)
        .with_context(|| format!("failed to open disk: {}", path.display()))?;

    // A zvol has one SPA sync pipeline, so more than about 4 queues
    // gives no more throughput.
    let num_queues: u16 = opts
        .num_queues
        .unwrap_or_else(|| std::cmp::min(num_vcpus as u16, 4).max(1));

    let blk = VirtioBlock::new(file, opts, Arc::clone(ctx.physmap), num_queues)
        .with_context(|| {
            format!("failed to create virtio-blk for: {}", path.display())
        })?;

    let cfg_size = blk.config_size();
    // One MSI-X vector per queue, plus one for config change.
    let msix = Arc::new(MsixTable::new(num_queues + 1, ctx.msi_sink()));

    let pci_dev = VirtioPciDevice::new(
        blk,
        bits::VIRTIO_DEV_TYPE_BLOCK,
        num_queues as usize,
        128,
        cfg_size,
        lintr,
        Arc::clone(ctx.physmap),
        Arc::clone(ctx.bus_pio),
        Arc::clone(ctx.bus_mmio),
        Some(msix),
    );

    // Workers raise through the transport so the interrupt takes the
    // MSI-X vector when the guest enabled it, and INTx otherwise.
    pci_dev.device().install_interrupt(pci_dev.backend_intr());

    Ok(pci_dev)
}

/// Build a viona device and its PCI transport on `vnic_name`.
///
/// `defer_poll` holds the in-kernel interrupt poll thread until the
/// ring state is restored. A binary with no migration path passes
/// `false`.
pub fn virtio_viona(
    vnic_name: &str,
    promiscphys: bool,
    lintr: Option<LintrCfg>,
    defer_poll: bool,
    ctx: &VirtioAttachCtx<'_>,
    log: &slog::Logger,
) -> anyhow::Result<Arc<VirtioPciDevice<VirtioViona>>> {
    anyhow::ensure!(
        !vnic_name.is_empty(),
        "virtio-net-viona requires a VNIC name"
    );

    use std::os::fd::AsFd;
    let dev =
        VirtioViona::new(vnic_name, ctx.hdl.as_fd()).with_context(|| {
            format!("failed to create viona device for VNIC: {vnic_name}")
        })?;

    if promiscphys {
        dev.set_promisc(true).with_context(|| {
            format!("failed to set promiscphys on {vnic_name}")
        })?;
    }

    // One MSI-X vector per queue (rx, tx), plus one for config change.
    let msix = Arc::new(MsixTable::new(
        viona::NET_NUM_QUEUES as u16 + 1,
        ctx.msi_sink(),
    ));

    // The poll thread reads the interrupt slot on every wakeup, so it
    // can start before the transport exists.
    let intr_log = log.new(slog::o!("component" => "viona-intr"));
    if defer_poll {
        dev.defer_intr_poll(intr_log);
    } else {
        dev.start_intr_poll(intr_log);
    }

    let pci_dev = VirtioPciDevice::new(
        dev,
        bits::VIRTIO_DEV_TYPE_NET,
        viona::NET_NUM_QUEUES,
        viona::NET_QUEUE_SIZE,
        viona::NET_CONFIG_SIZE,
        lintr,
        Arc::clone(ctx.physmap),
        Arc::clone(ctx.bus_pio),
        Arc::clone(ctx.bus_mmio),
        Some(msix),
    );

    pci_dev.device().install_interrupt(pci_dev.backend_intr());

    Ok(pci_dev)
}

/// Build a virtio-console device and its PCI transport from a socket
/// path.
///
/// The interrupt path comes from the transport, so the device gets it
/// only after `VirtioPciDevice::new` returns.
pub fn virtio_console(
    socket_path: &Path,
    lintr: Option<LintrCfg>,
    ctx: &VirtioAttachCtx<'_>,
    log: &slog::Logger,
) -> anyhow::Result<Arc<VirtioPciDevice<VirtioConsole>>> {
    let console =
        VirtioConsole::new(socket_path, Arc::clone(ctx.physmap), log.clone())?;
    let cfg_size = console.config_size();

    // One vector per queue, plus one for config change.
    let msix = Arc::new(MsixTable::new(CONSOLE_MSIX_VECTORS, ctx.msi_sink()));

    let pci_dev = VirtioPciDevice::new(
        console,
        bits::VIRTIO_DEV_TYPE_CONSOLE,
        CONSOLE_NUM_QUEUES,
        CONSOLE_QUEUE_SIZE,
        cfg_size,
        lintr,
        Arc::clone(ctx.physmap),
        Arc::clone(ctx.bus_pio),
        Arc::clone(ctx.bus_mmio),
        Some(msix),
    );

    // Only the receive queue completes out of band. The transport
    // raises for the transmit queue itself.
    pci_dev.device().install_interrupt(pci_dev.backend_intr());

    Ok(pci_dev)
}

/// Build a virtio-fs device and its PCI transport from parsed options.
///
/// `queue_size` must already come from
/// [`super::fs::clamp_queue_size`]: `VirtQueue::new` panics on a size
/// that is not a power of two.
///
/// The interrupt path comes from the transport, so the device gets it
/// only after `VirtioPciDevice::new` returns.
pub fn virtio_fs(
    opts: &VirtioFsOpts,
    queue_size: u16,
    lintr: Option<LintrCfg>,
    ctx: &VirtioAttachCtx<'_>,
    log: &slog::Logger,
) -> anyhow::Result<Arc<VirtioPciDevice<VirtioFs>>> {
    let fs = VirtioFs::new(opts, Arc::clone(ctx.physmap), log.clone())?;

    let msix = Arc::new(MsixTable::new(FS_MSIX_VECTORS, ctx.msi_sink()));

    let pci_dev = VirtioPciDevice::new(
        fs,
        bits::VIRTIO_DEV_TYPE_FS,
        FS_NUM_QUEUES,
        queue_size,
        FS_CONFIG_SIZE,
        lintr,
        Arc::clone(ctx.physmap),
        Arc::clone(ctx.bus_pio),
        Arc::clone(ctx.bus_mmio),
        Some(msix),
    );

    // Both queues complete out of band, so the raise carries the queue
    // index and each queue raises its own MSI-X vector.
    pci_dev.device().install_interrupt(pci_dev.backend_intr());

    Ok(pci_dev)
}

/// Build a virtio-rng device and its PCI transport.
pub fn virtio_rng(
    lintr: Option<LintrCfg>,
    ctx: &VirtioAttachCtx<'_>,
) -> Arc<VirtioPciDevice<VirtioRng>> {
    // One queue, plus a config change vector.
    let msix = Arc::new(MsixTable::new(2, ctx.msi_sink()));
    VirtioPciDevice::new(
        VirtioRng::new(),
        bits::VIRTIO_DEV_TYPE_RNG,
        1,
        64,
        0,
        lintr,
        Arc::clone(ctx.physmap),
        Arc::clone(ctx.bus_pio),
        Arc::clone(ctx.bus_mmio),
        Some(msix),
    )
}
