// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CPU hotplug register file, at PIO 0xAF00.
//!
//! The layout is QEMU's modern CPU hotplug interface, specified in its
//! `docs/specs/acpi_cpu_hotplug.rst`. The AML in `acpi::hotplug::cpu`
//! is written against the same ABI, so a guest driver that already
//! knows the interface works with no change.
//!
//! The guest selects a CPU slot, then reads that slot's status or
//! writes its control bits. A selector past the last slot makes every
//! later read answer zero and every later non-selector write a no-op,
//! as the ABI document requires. Thus no guest value reaches a `Vec`
//! index here.
//!
//! An eject is recorded, never performed. Tearing a vCPU down on the
//! vCPU thread that is running the guest's `_EJ0` would deadlock.

use std::sync::{Arc, Mutex};

use slog::Logger;
use vmm_core::common::RWOp;

use crate::acpi_gpe::{GpeBit, HotplugEventSink};

/// Base I/O port of the CPU hotplug register block.
pub const CPU_HOTPLUG_PORT: u16 = 0xAF00;

/// Length of the register block in bytes.
pub const CPU_HOTPLUG_LEN: u16 = 12;

/// Most CPU slots this VMM describes.
///
/// A slot id is an APIC id in the MADT and in the `_MAT` buffer of the
/// hotplug AML, and three hex digits in the AML device name, so it has
/// to fit a byte. bhyve's `VM_MAXCPU` is far below the limit.
pub const MAX_CPU_SLOTS: usize = 256;

// Register offsets inside the block. Some read and write different
// registers at the same offset, so the names carry the direction.
const OFF_CMD_DATA2_R: usize = 0x0;
const OFF_SELECTOR_W: usize = 0x0;
const OFF_FLAGS_RW: usize = 0x4;
const OFF_COMMAND_W: usize = 0x5;
const OFF_CMD_DATA_RW: usize = 0x8;

/// Width of the two DWORD registers.
const DWORD: usize = 4;
/// Width of the two byte registers.
const BYTE: usize = 1;

// Status bits, read at `OFF_FLAGS_RW`.
const STS_ENABLED: u8 = 1 << 0;
const STS_INSERT_EVENT: u8 = 1 << 1;
const STS_REMOVE_EVENT: u8 = 1 << 2;

// Control bits, written at `OFF_FLAGS_RW`.
const CTL_CLEAR_INSERT: u8 = 1 << 1;
const CTL_CLEAR_REMOVE: u8 = 1 << 2;
const CTL_EJECT: u8 = 1 << 3;
const CTL_FW_EJECT: u8 = 1 << 4;

// Commands, written at `OFF_COMMAND_W`.
const CMD_GET_NEXT_EVENT: u8 = 0;
const CMD_OST_EVENT: u8 = 1;
const CMD_OST_STATUS: u8 = 2;
const CMD_GET_CPU_ID: u8 = 3;
/// One above the last command this ABI defines.
const CMD_MAX: u8 = 4;

/// Slot 0 holds the boot CPU.
const BOOT_CPU: usize = 0;

const LOCK_POISONED: &str = "cpu hotplug lock poisoned";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CpuSlot {
    /// The guest may use this CPU. Reported as status bit 0, which the
    /// AML `_STA` method returns.
    enabled: bool,
    /// An add the guest has not acknowledged yet.
    inserting: bool,
    /// A removal the guest has not acknowledged yet.
    removing: bool,
    /// The host asked for this CPU to go.
    ///
    /// Separate from `removing`: the guest acknowledges the remove
    /// event when it sends the eject request notification, and calls
    /// `_EJ0` only after it takes the CPU offline. An eject gated on
    /// `removing` would refuse every real eject (cloud-hypervisor has
    /// this defect).
    pending_removal: bool,
    /// `_OST` source, written under command 1.
    ost_event: u32,
    /// `_OST` status, written under command 2.
    ost_status: u32,
}

struct Inner {
    cpus: Vec<CpuSlot>,
    /// The raw value the guest last wrote. Command 0 reads it back.
    selector: u32,
    /// The slot `selector` names, or `None` when it names no slot.
    index: Option<usize>,
    command: u8,
    /// Ejects the guest asked for, waiting for the control plane.
    ///
    /// Bounded: an eject needs a `pending_removal` that only the host
    /// sets, and taking the eject clears it, so a guest cannot make
    /// this grow on its own.
    ejects: Vec<u32>,
}

impl Inner {
    fn selected(&self) -> Option<(usize, &CpuSlot)> {
        let index = self.index?;
        Some((index, self.cpus.get(index)?))
    }

    fn selected_mut(&mut self) -> Option<(usize, &mut CpuSlot)> {
        let index = self.index?;
        Some((index, self.cpus.get_mut(index)?))
    }

    /// Store a guest-written selector, and resolve it to a slot.
    ///
    /// This is the only bounds check in the file. Everything after it
    /// works from `index`, so no guest value reaches a `Vec` index.
    fn select(&mut self, value: u32) {
        self.selector = value;
        self.index = usize::try_from(value)
            .ok()
            .filter(|index| *index < self.cpus.len());
    }

    /// Move the selector to the next slot with an unacknowledged
    /// event, starting at the current slot and wrapping once.
    fn select_next_event(&mut self) {
        let Some(start) = self.index else {
            return;
        };
        let count = self.cpus.len();
        if count == 0 {
            return;
        }
        for step in 0..count {
            let Some(index) = start.checked_add(step).map(|sum| sum % count)
            else {
                return;
            };
            let Some(slot) = self.cpus.get(index) else {
                return;
            };
            if slot.inserting || slot.removing {
                if let Ok(selector) = u32::try_from(index) {
                    self.selector = selector;
                    self.index = Some(index);
                }
                return;
            }
        }
    }
}

/// The CPU hotplug register block.
pub struct CpuHotplug {
    log: Logger,
    sink: Arc<dyn HotplugEventSink>,
    inner: Mutex<Inner>,
}

impl CpuHotplug {
    /// Describe `max_cpus` CPU slots, all of them absent.
    ///
    /// The boot CPUs are marked with [`set_boot_cpus`](Self::set_boot_cpus).
    /// They are not an argument here because an add through
    /// [`notify_added`](Self::notify_added) raises the SCI, and a boot
    /// CPU must not.
    pub fn new(
        max_cpus: u32,
        sink: Arc<dyn HotplugEventSink>,
        log: Logger,
    ) -> Arc<Self> {
        let asked = usize::try_from(max_cpus).unwrap_or(MAX_CPU_SLOTS);
        let count = asked.min(MAX_CPU_SLOTS);
        if count != asked {
            slog::warn!(log, "CPU hotplug clamped the slot count";
                "asked" => asked, "slots" => count);
        }
        Arc::new(Self {
            log,
            sink,
            inner: Mutex::new(Inner {
                cpus: vec![CpuSlot::default(); count],
                selector: 0,
                // The ABI says the selector starts valid, so a guest
                // that reads before it writes gets slot 0, not zeroes.
                index: (count > 0).then_some(BOOT_CPU),
                command: CMD_GET_NEXT_EVENT,
                ejects: Vec::new(),
            }),
        })
    }

    /// Mark the first `num_cpus` slots present, with no insert event.
    ///
    /// The boot CPUs are already running when the guest reads the
    /// DSDT, so they must report `_STA` as enabled without ever having
    /// raised a hotplug event.
    pub fn set_boot_cpus(&self, num_cpus: u32) {
        let mut inner = self.inner.lock().expect(LOCK_POISONED);
        let count = usize::try_from(num_cpus).unwrap_or(usize::MAX);
        let available = inner.cpus.len();
        if count > available {
            slog::warn!(self.log, "more boot CPUs than slots";
                "boot" => count, "slots" => available);
        }
        for slot in inner.cpus.iter_mut().take(count) {
            slot.enabled = true;
        }
    }

    /// Report that `cpu_id` is now usable, and raise the SCI.
    pub fn notify_added(&self, cpu_id: u32) {
        let mut inner = self.inner.lock().expect(LOCK_POISONED);
        let Some(slot) = index_of(&mut inner, cpu_id, &self.log) else {
            return;
        };
        slot.enabled = true;
        slot.inserting = true;
        // An add cancels a removal the guest has not finished, or the
        // next `_EJ0` would tear down the CPU that was just added.
        slot.removing = false;
        slot.pending_removal = false;
        drop(inner);

        slog::debug!(self.log, "CPU hotplug add"; "cpu" => cpu_id);
        self.sink.raise(GpeBit::Cpu);
    }

    /// Ask the guest to give `cpu_id` up, and raise the SCI.
    ///
    /// The guest answers with an eject request, which
    /// [`take_eject_requests`](Self::take_eject_requests) reports.
    pub fn notify_removed(&self, cpu_id: u32) {
        if cpu_id == 0 {
            slog::warn!(self.log, "refused to remove the boot CPU");
            return;
        }
        let mut inner = self.inner.lock().expect(LOCK_POISONED);
        let Some(slot) = index_of(&mut inner, cpu_id, &self.log) else {
            return;
        };
        if !slot.enabled {
            slog::warn!(self.log, "refused to remove an absent CPU";
                "cpu" => cpu_id);
            return;
        }
        slot.removing = true;
        slot.pending_removal = true;
        drop(inner);

        slog::debug!(self.log, "CPU hotplug remove"; "cpu" => cpu_id);
        self.sink.raise(GpeBit::Cpu);
    }

    /// Take the ejects the guest has agreed to since the last call.
    pub fn take_eject_requests(&self) -> Vec<u32> {
        let mut inner = self.inner.lock().expect(LOCK_POISONED);
        std::mem::take(&mut inner.ejects)
    }

    /// Handle one access to the block. `offset` is from
    /// [`CPU_HOTPLUG_PORT`].
    pub fn pio_rw(&self, offset: usize, rwo: RWOp<'_>) {
        match rwo {
            RWOp::Read(ro) => {
                // Every reserved register, and every width this ABI
                // does not define, reads as zero. Clearing first means
                // a refused access cannot leak a stale buffer.
                ro.write_u64(0);
                let inner = self.inner.lock().expect(LOCK_POISONED);
                let Some((index, slot)) = inner.selected() else {
                    return;
                };
                let Ok(arch_id) = u64::try_from(index) else {
                    return;
                };
                match (offset, ro.len()) {
                    (OFF_CMD_DATA2_R, DWORD) => {
                        ro.write_u32(cmd_data2(inner.command, arch_id));
                    }
                    (OFF_FLAGS_RW, BYTE) => ro.write_u8(status_bits(slot)),
                    (OFF_CMD_DATA_RW, DWORD) => {
                        let selector = inner.selector;
                        ro.write_u32(cmd_data(
                            inner.command,
                            arch_id,
                            selector,
                        ));
                    }
                    _ => {}
                }
            }
            RWOp::Write(wo) => {
                let mut inner = self.inner.lock().expect(LOCK_POISONED);
                match (offset, wo.len()) {
                    // The selector write is the one access a bad
                    // selector does not block, or the guest could
                    // never get back to a valid one.
                    (OFF_SELECTOR_W, DWORD) => inner.select(wo.read_u32()),
                    (OFF_FLAGS_RW, BYTE) => {
                        self.write_control(&mut inner, wo.read_u8())
                    }
                    (OFF_COMMAND_W, BYTE) => {
                        write_command(&mut inner, wo.read_u8())
                    }
                    (OFF_CMD_DATA_RW, DWORD) => {
                        write_cmd_data(&mut inner, wo.read_u32())
                    }
                    _ => {}
                }
            }
        }
    }

    /// Apply a write to the control byte.
    ///
    /// One bit acts per write, in the order QEMU uses, so a guest that
    /// sets the eject bit together with an acknowledge bit gets the
    /// acknowledge and not the eject.
    fn write_control(&self, inner: &mut Inner, data: u8) {
        let Some((index, slot)) = inner.selected_mut() else {
            return;
        };
        if data & CTL_CLEAR_INSERT != 0 {
            slot.inserting = false;
        } else if data & CTL_CLEAR_REMOVE != 0 {
            // `pending_removal` deliberately survives. The guest calls
            // `_EJ0` after this acknowledge, not before it.
            slot.removing = false;
        } else if data & CTL_EJECT != 0 {
            self.request_eject(inner, index);
        } else if data & CTL_FW_EJECT != 0 {
            // No SMI firmware exists to finish the handover, so taking
            // the bit would park an event nothing can ever clear. The
            // AML leaves the bit reserved, so no guest driver asks.
            slog::debug!(self.log, "refused a firmware eject handover";
                "cpu" => index);
        }
    }

    /// Record an eject the guest asked for.
    ///
    /// Every refusal here logs at debug: a guest can drive this path
    /// in a loop, so a louder level would let it flood the log.
    fn request_eject(&self, inner: &mut Inner, index: usize) {
        if index == BOOT_CPU {
            // Guest and host cannot resynchronise after the boot CPU
            // goes, so this is refused however it arrives.
            slog::debug!(self.log, "refused an eject of the boot CPU");
            return;
        }
        let Ok(cpu_id) = u32::try_from(index) else {
            return;
        };
        let Some(slot) = inner.cpus.get_mut(index) else {
            return;
        };
        if !slot.enabled {
            slog::debug!(self.log, "refused an eject of an absent CPU";
                "cpu" => cpu_id);
            return;
        }
        if !slot.pending_removal {
            slog::debug!(self.log, "refused an eject the host did not ask for";
                "cpu" => cpu_id);
            return;
        }
        slot.pending_removal = false;
        slot.removing = false;
        // The guest has already taken the CPU offline by the time it
        // runs `_EJ0`. Leaving the enable bit set would make its next
        // `_STA` read the CPU as present and treat the eject as failed.
        slot.enabled = false;
        inner.ejects.push(cpu_id);
        slog::info!(self.log, "guest ejected a CPU"; "cpu" => cpu_id);
    }

    /// The slot state, for unit tests.
    #[cfg(test)]
    fn slot(&self, index: usize) -> Option<CpuSlot> {
        self.inner
            .lock()
            .expect(LOCK_POISONED)
            .cpus
            .get(index)
            .cloned()
    }

    /// The resolved selector, for unit tests.
    #[cfg(test)]
    fn selected_index(&self) -> Option<usize> {
        self.inner.lock().expect(LOCK_POISONED).index
    }
}

/// Resolve a host-supplied CPU id to its slot.
///
/// A bad id is a VMM bug, not a guest action, so it logs at warn.
fn index_of<'a>(
    inner: &'a mut Inner,
    cpu_id: u32,
    log: &Logger,
) -> Option<&'a mut CpuSlot> {
    let index = usize::try_from(cpu_id).ok()?;
    let count = inner.cpus.len();
    let slot = inner.cpus.get_mut(index);
    if slot.is_none() {
        slog::warn!(log, "CPU hotplug event names no slot";
            "cpu" => cpu_id, "slots" => count);
    }
    slot
}

/// Pack the status byte the guest reads at offset 4.
fn status_bits(slot: &CpuSlot) -> u8 {
    let mut bits = 0u8;
    if slot.enabled {
        bits |= STS_ENABLED;
    }
    if slot.inserting {
        bits |= STS_INSERT_EVENT;
    }
    if slot.removing {
        bits |= STS_REMOVE_EVENT;
    }
    // Status bit 4 reports a firmware eject handover, which this VMM
    // refuses, so it always reads back clear.
    bits
}

/// The value read from Command data 2, at offset 0.
fn cmd_data2(command: u8, arch_id: u64) -> u32 {
    match command {
        CMD_GET_CPU_ID => (arch_id >> 32) as u32,
        _ => 0,
    }
}

/// The value read from Command data, at offset 8.
fn cmd_data(command: u8, arch_id: u64, selector: u32) -> u32 {
    match command {
        CMD_GET_NEXT_EVENT => selector,
        CMD_GET_CPU_ID => arch_id as u32,
        _ => 0,
    }
}

/// Apply a write to the command byte.
fn write_command(inner: &mut Inner, command: u8) {
    if command >= CMD_MAX {
        return;
    }
    inner.command = command;
    if command == CMD_GET_NEXT_EVENT {
        inner.select_next_event();
    }
}

/// Apply a write to Command data, at offset 8.
fn write_cmd_data(inner: &mut Inner, value: u32) {
    let command = inner.command;
    let Some((_, slot)) = inner.selected_mut() else {
        return;
    };
    match command {
        CMD_OST_EVENT => slot.ost_event = value,
        CMD_OST_STATUS => slot.ost_status = value,
        _ => {}
    }
}

#[cfg(test)]
mod tests;
