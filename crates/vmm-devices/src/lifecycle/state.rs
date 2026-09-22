// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The state a migration carries for one device.
//!
//! These types are the wire format. A device exports one of them, the
//! migration layer ships it under the device's PCI address, and the
//! destination hands it to the device at that same address. A field
//! added here is a wire-format change and needs `PROTOCOL_RON` in
//! `vmm-migrate` bumped with it.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::pci::msix::MsixMigrateState;
use crate::pci::Bdf;

/// A device's PCI address on the migration wire.
///
/// The destination matches a payload to a device by this alone. Two
/// devices of one kind and one queue count are otherwise identical, so
/// anything weaker restores one device's rings onto another.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
)]
pub struct WireBdf {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
}

impl From<Bdf> for WireBdf {
    fn from(bdf: Bdf) -> Self {
        Self {
            bus: bdf.bus(),
            dev: bdf.dev(),
            func: bdf.func(),
        }
    }
}

impl fmt::Display for WireBdf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.bus, self.dev, self.func)
    }
}

/// Why a device could not export or restore its migration state.
///
/// Every variant fails the migration. The source keeps running on an
/// export failure; the destination is discarded on a restore failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeviceStateError {
    #[error("export failed: {0}")]
    Export(String),
    #[error("payload is {got} state, this device is {want}")]
    WrongKind {
        want: &'static str,
        got: &'static str,
    },
    #[error("invalid state: {0}")]
    Invalid(String),
}

impl DeviceStateError {
    pub fn invalid(what: impl fmt::Display) -> Self {
        Self::Invalid(what.to_string())
    }
}

/// The state a migration carries for one device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceMigrateState {
    Virtio(VirtioMigrateState),
    Nvme(NvmeMigrateState),
}

impl DeviceMigrateState {
    /// The kind name the topology check and the error messages use.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Virtio(_) => "virtio",
            Self::Nvme(_) => "nvme",
        }
    }
}

/// One device's state under the address it had on the source.
///
/// Every device with an address is on the wire, `None` state included,
/// so the destination can tell "the source had nothing to carry" from
/// "the source has a device this VM does not".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceStatePayload {
    pub bdf: WireBdf,
    pub state: Option<DeviceMigrateState>,
}

/// The Hyper-V enlightenment's guest-visible state.
///
/// Not a PCI device: the enlightenment belongs to the VM, so it travels
/// beside the per-address payloads rather than under one of them.
///
/// The two overlay pages are not carried. They are derived from the
/// MSR values, so replaying the MSR writes on the destination
/// reinstalls them, which is what Propolis does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HypervMigrateState {
    pub guest_os_id: u64,
    pub hypercall: u64,
    pub reference_tsc: u64,
    pub crash_p: [u64; 5],
    /// `HV_X64_MSR_TIME_REF_COUNT` at export, in 100 ns units.
    ///
    /// The destination continues the counter from here. A fresh
    /// process-local clock makes a counter the guest is told is
    /// monotonic run backwards.
    pub time_ref_count: u64,
}

/// PCI header state a transport replays before its queues.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigratePciState {
    /// PCI command register bits that gate BAR decoding and bus mastering.
    pub command: u16,
    /// BAR base addresses indexed by BAR number (BAR0..BAR5).
    pub bar_addrs: [u64; 6],
}

/// One virtio PCI transport: the register file the driver programmed
/// and every queue it configured.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtioMigrateState {
    /// The device status byte as the driver left it. Restored as it
    /// was: a destination that synthesises DRIVER_OK tells the guest
    /// driver a device is ready that the driver never finished.
    pub status: u8,
    /// Features the driver accepted.
    pub features: u64,
    /// MSI-X vector for configuration changes, `0xFFFF` for none.
    pub config_msix_vector: u16,
    pub pci: MigratePciState,
    /// The MSI-X table, when the transport has one. `None` keeps the
    /// destination on INTx, which is where an INTx guest has to stay.
    pub msix: Option<MsixMigrateState>,
    /// Every configured queue, in queue order.
    pub queues: Vec<VirtioMigrateQueue>,
}

/// One configured virtqueue.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtioMigrateQueue {
    pub queue_idx: u16,
    pub queue_size: u16,
    pub desc_addr: u64,
    pub avail_addr: u64,
    pub used_addr: u64,
    /// The queue is live: enabled through the modern register, or
    /// programmed through the legacy PFN register, which has no
    /// enable bit.
    pub live: bool,
    /// The device's consumption cursor into the available ring.
    pub avail_idx: u16,
    /// The device's completion cursor into the used ring.
    pub used_idx: u16,
    /// MSI-X vector for this queue, `0xFFFF` for none.
    pub msix_vector: u16,
}

/// One NVMe controller.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NvmeMigrateState {
    pub cc: u32,
    pub csts: u32,
    pub aqa: u32,
    pub asq_base: u64,
    pub acq_base: u64,
    pub admin_sq: Option<NvmeMigrateSq>,
    pub admin_cq: Option<NvmeMigrateCq>,
    pub io_sqs: Vec<Option<NvmeMigrateSq>>,
    pub io_cqs: Vec<Option<NvmeMigrateCq>>,
    pub num_io_queues: u16,
    pub pci: MigratePciState,
    pub msix: MsixMigrateState,
}

/// NVMe submission queue state for migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NvmeMigrateSq {
    pub base: u64,
    pub size: u16,
    pub head: u16,
    pub tail: u16,
    pub cq_id: u16,
}

/// NVMe completion queue state for migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NvmeMigrateCq {
    pub base: u64,
    pub size: u16,
    pub head: u16,
    pub tail: u16,
    pub phase: bool,
    pub iv: u16,
    /// Whether the driver asked for an interrupt on this queue. A Linux
    /// poll queue has none, and restoring one as interrupting sends the
    /// destination guest a message it never armed a handler for.
    pub ien: bool,
}
