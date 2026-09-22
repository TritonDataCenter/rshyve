// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Crate-boundary surface for vmm_virtio.

use std::sync::Arc;

use vmm_devices::pci::device::PciDevice;
use vmm_devices::Lifecycle;
use vmm_virtio::{
    VirtQueue, VirtioBlock, VirtioConsole, VirtioFs, VirtioPciDevice,
    VirtioRng, VirtioViona,
};

fn _coerce_rng(
    dev: Arc<VirtioPciDevice<VirtioRng>>,
) -> (Arc<dyn PciDevice>, Arc<dyn Lifecycle>) {
    (dev.clone(), dev)
}

fn _coerce_blk(
    dev: Arc<VirtioPciDevice<VirtioBlock>>,
) -> (Arc<dyn PciDevice>, Arc<dyn Lifecycle>) {
    (dev.clone(), dev)
}

fn _coerce_viona(
    dev: Arc<VirtioPciDevice<VirtioViona>>,
) -> (Arc<dyn PciDevice>, Arc<dyn Lifecycle>) {
    (dev.clone(), dev)
}

fn _coerce_console(
    dev: Arc<VirtioPciDevice<VirtioConsole>>,
) -> (Arc<dyn PciDevice>, Arc<dyn Lifecycle>) {
    (dev.clone(), dev)
}

fn _coerce_fs(
    dev: Arc<VirtioPciDevice<VirtioFs>>,
) -> (Arc<dyn PciDevice>, Arc<dyn Lifecycle>) {
    (dev.clone(), dev)
}

#[test]
fn queue_is_reachable_at_the_crate_root() {
    let queue = VirtQueue::new(128);
    assert_eq!(queue.size(), 128);
    assert!(!queue.is_configured());
}

#[test]
fn device_type_and_net_constants_kept_their_module_paths() {
    assert_eq!(vmm_virtio::bits::VIRTIO_DEV_TYPE_NET, 1);
    assert_eq!(vmm_virtio::bits::VIRTIO_DEV_TYPE_BLOCK, 2);
    assert_eq!(vmm_virtio::bits::VIRTIO_DEV_TYPE_RNG, 4);
    let _queues: usize = vmm_virtio::viona::NET_NUM_QUEUES;
    let _size: u16 = vmm_virtio::viona::NET_QUEUE_SIZE;
    let _config: u16 = vmm_virtio::viona::NET_CONFIG_SIZE;
}
