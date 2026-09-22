// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a control plane reports about the machine's CPU and memory
//! slots, and why it refuses what it refuses.
//!
//! Two control planes serve this, one over JSON and one over vsock
//! text lines. The facts are computed here once. Each serialises
//! them its own way.

use std::collections::BTreeSet;
use std::fmt;

use crate::hotplug::mem::{MemHotplugEngine, MemWindow};
use crate::hotplug::CpuSlots;

/// Why a CPU request was refused on a VM with no CPU slots.
///
/// Two starts give a VM no engine: no `--hotplug`, and a `maxcpus` that
/// is not above the boot count. The answer names both, because the
/// operator has to fix whichever one applies.
pub const CPU_HOTPLUG_OFF: &str =
    "CPU hot-add needs --hotplug and -c cpus=N,maxcpus=M";

/// The same for memory.
pub const MEM_HOTPLUG_OFF: &str =
    "memory hot-add needs --hotplug and -o hotplug.maxmem=SIZE";

/// Why `cpu-remove` cannot exist on this platform.
pub const NO_CPU_REMOVE: &str = "cpu-remove is not supported: illumos has no \
     vm_deactivate_cpu, so an active vCPU cannot be taken back";

/// Why `mem-remove` cannot exist on this platform.
pub const NO_MEM_REMOVE: &str = "mem-remove is not supported: illumos has no \
     VM_FREE_MEMSEG, so a memory segment lives until the VM does";

/// What one CPU slot holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuSlotState {
    /// A CPU is online in it.
    Present,
    /// Empty, and a hot-add may fill it.
    Absent,
    /// A failed add spent the id. illumos cannot deactivate a vCPU, so
    /// the slot can never be filled.
    Consumed,
}

impl fmt::Display for CpuSlotState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Present => "present",
            Self::Absent => "absent",
            Self::Consumed => "consumed",
        })
    }
}

/// Whether a slot was filled at boot or is one a hot-add may fill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuKind {
    Boot,
    Hotplug,
}

impl fmt::Display for CpuKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Boot => "boot",
            Self::Hotplug => "hotplug",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuSlot {
    pub id: u32,
    pub state: CpuSlotState,
    pub kind: CpuKind,
}

/// Every CPU slot the machine has, with the state of each.
///
/// Every slot the MADT describes is listed, whether or not this VM can
/// fill it: hiding the spare slots would contradict what the guest
/// reads. `hotplug` is what says whether `cpu-add` can work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpuInventory {
    pub boot_cpus: u32,
    pub max_cpus: u32,
    pub hotplug: bool,
    pub slots: Vec<CpuSlot>,
}

/// The CPU slots, from the engine when the VM has one and from the
/// boot counts otherwise.
pub fn cpu_inventory(
    boot_cpus: u32,
    max_cpus: u32,
    cpus: Option<&CpuSlots>,
) -> CpuInventory {
    // With an engine, its ceiling is the one an add is checked against.
    // The two agree: both come from `VmOpts::max_cpus`, which is
    // already capped at the kernel's 64.
    let max_cpus = cpus.map_or(max_cpus, CpuSlots::max_cpus);
    let boot_cpus = cpus.map_or(boot_cpus, CpuSlots::boot_cpus);
    // A set, never a slice indexed by an id. The ids come from the
    // engine and are bounded by the kernel's 64 CPUs, but nothing here
    // has to trust that.
    let consumed: BTreeSet<u32> = cpus
        .map(CpuSlots::consumed)
        .unwrap_or_default()
        .into_iter()
        .collect();

    let slots = (0..max_cpus)
        .map(|id| {
            let state = if consumed.contains(&id) {
                CpuSlotState::Consumed
            } else if cpus.map_or(id < boot_cpus, |cpus| cpus.is_online(id)) {
                CpuSlotState::Present
            } else {
                CpuSlotState::Absent
            };
            let kind = if id < boot_cpus {
                CpuKind::Boot
            } else {
                CpuKind::Hotplug
            };
            CpuSlot { id, state, kind }
        })
        .collect();
    CpuInventory {
        boot_cpus,
        max_cpus,
        hotplug: cpus.is_some(),
        slots,
    }
}

/// What `mem-list` says about the hot-add window.
///
/// Split from the engine so the wire shape can be tested: every part of
/// a real engine is an ioctl on a live VM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemWindowReport {
    pub base: u64,
    pub bytes: u64,
    pub slot_bytes: u64,
    pub slots: usize,
    pub slots_used: usize,
    pub added_bytes: u64,
}

impl MemWindowReport {
    pub fn new(
        window: &MemWindow,
        slots_used: usize,
        added_bytes: u64,
    ) -> Self {
        Self {
            base: window.base(),
            bytes: window.size(),
            slot_bytes: window.slot_size(),
            slots: window.slots(),
            slots_used,
            added_bytes,
        }
    }

    pub fn of(engine: &MemHotplugEngine) -> Self {
        Self::new(&engine.window(), engine.slots_used(), engine.bytes_added())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_refusal_carries_a_run_of_spaces() {
        // A `\` line continuation in a string literal eats the newline
        // AND the indent that follows it. A message rebuilt without one
        // keeps that indent and reaches the operator as a gap.
        for message in [
            CPU_HOTPLUG_OFF,
            MEM_HOTPLUG_OFF,
            NO_CPU_REMOVE,
            NO_MEM_REMOVE,
        ] {
            assert!(!message.contains("  "), "{message}");
        }
    }

    #[test]
    fn the_madt_slots_are_listed_even_with_no_engine() {
        // -c cpus=2,maxcpus=4 without --hotplug still gives the guest
        // four LAPIC entries. Hiding two of them would contradict what
        // the guest reads.
        let inv = cpu_inventory(2, 4, None);
        assert!(!inv.hotplug);
        let shape: Vec<(u32, String, String)> = inv
            .slots
            .iter()
            .map(|s| (s.id, s.state.to_string(), s.kind.to_string()))
            .collect();
        assert_eq!(
            shape,
            [
                (0, "present".into(), "boot".into()),
                (1, "present".into(), "boot".into()),
                (2, "absent".into(), "hotplug".into()),
                (3, "absent".into(), "hotplug".into()),
            ]
        );
    }
}
