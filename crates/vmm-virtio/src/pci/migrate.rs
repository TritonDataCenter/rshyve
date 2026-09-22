// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What this transport puts on the migration wire, and the order in
//! which a restore applies it.
//!
//! This module owns the restore order. Each step depends on the one
//! before it:
//!
//! 1. Config space first. Its BAR writes put the device back on the
//!    PIO and MMIO buses. A queue programmed before them belongs to a
//!    device that decodes nothing.
//! 2. Features next, applied to every queue. A guest does this in the
//!    feature register handler, which a restore does not call. A queue
//!    without INDIRECT_DESC silently drops every indirect chain, and
//!    all Linux virtio-blk I/O uses them.
//! 3. Ring addresses and indices, then the MSI-X table. The backend
//!    restores a ring only after both, because it programs hardware
//!    from them.
//! 4. An interrupt on each live queue, last. A driver whose rings are
//!    caught up has nothing to poll for. Without the interrupt it
//!    posts no new receive buffers, and the kernel viona driver drops
//!    arriving packets with "no_space".
//!
//! Every queue field is checked before any field is applied. The peer
//! is not authenticated, so a payload the destination cannot hold must
//! fail the migration. The MSI-X table is checked only when step 3
//! imports it, so a bad table fails after steps 1 and 2 are applied.

use std::sync::atomic::Ordering;

use vmm_devices::lifecycle::{
    DeviceStateError, MigratePciState, VirtioMigrateQueue, VirtioMigrateState,
};
use vmm_devices::pci::device::replay_migrate_pci_state;
use vmm_devices::pci::BarN;

use super::bits;
use super::{VirtioDevice, VirtioPciDevice};

impl<D: VirtioDevice> VirtioPciDevice<D> {
    /// Replay BAR and command-register writes to restore the bus
    /// registrations. The writes go through `cfg_write`, the same path
    /// as a guest BAR write, so the bus applies its own rules.
    fn restore_migrate_pci_state(&self, state: &MigratePciState) {
        let writes = self
            .pci_state
            .lock()
            .expect("pci lock")
            .bar_replay_writes(&state.bar_addrs);
        replay_migrate_pci_state(self, &writes, state.command);
    }

    /// Snapshot the register file the driver programmed and every queue
    /// it configured.
    pub(super) fn export_migrate_queues(
        &self,
    ) -> Result<VirtioMigrateState, DeviceStateError> {
        let vs = self.virtio_state.lock().expect("virtio lock");
        let modern = vs.guest_features & bits::VIRTIO_F_VERSION_1 != 0;
        let mut queues = Vec::new();
        for idx in 0..self.num_queues as usize {
            let q = &vs.queues[idx];
            if !q.is_configured() {
                continue;
            }
            // A backend that owns its cursors supplies both, and the
            // pair is used as one value. Zero is a valid used cursor,
            // so a test against zero discards a valid ring each time
            // the 16-bit counter wraps past it.
            let (avail_idx, used_idx) =
                match self.device.kernel_ring_indices(idx as u16)? {
                    Some(pair) => pair,
                    // A software device keeps its completion cursor in
                    // VirtioCompletion. That writes the used ring
                    // directly and never moves the VirtQueue shadow, so
                    // only guest memory holds the cursor.
                    None => {
                        let guest_used = q.read_used_ring_idx(&self.physmap);
                        let used_idx = if guest_used != 0 {
                            guest_used
                        } else {
                            q.shadow_used_idx()
                        };
                        (q.last_avail_idx(), used_idx)
                    }
                };

            let vector = self
                .msix_queue_vectors
                .get(idx)
                .map(|a| a.load(Ordering::Acquire))
                .unwrap_or(bits::VIRTIO_MSI_NO_VECTOR);

            queues.push(VirtioMigrateQueue {
                queue_idx: idx as u16,
                queue_size: q.size(),
                desc_addr: q.desc_addr(),
                avail_addr: q.avail_addr(),
                used_addr: q.used_addr(),
                // The legacy transport has no enable register. A
                // QUEUE_PFN write makes the ring live, so every
                // configured legacy queue is live. A dead queue skips
                // the backend restore and the wake on the destination.
                live: if modern {
                    vs.queue_enabled.get(idx).copied().unwrap_or(false)
                } else {
                    true
                },
                avail_idx,
                used_idx,
                msix_vector: vector,
            });
        }

        Ok(VirtioMigrateState {
            status: vs.status,
            features: vs.guest_features,
            config_msix_vector: self.msix_config_vector.load(Ordering::Acquire),
            pci: self.pci_state.lock().expect("pci lock").migrate_state(),
            msix: self.msix.as_ref().map(|msix| msix.export_state()),
            queues,
        })
    }

    /// Everything the payload must satisfy before any of it is applied.
    fn check_migrate_state(
        &self,
        state: &VirtioMigrateState,
    ) -> Result<(), DeviceStateError> {
        let nq = self.num_queues;
        let table_len = self.msix.as_ref().map_or(0, |m| m.count());
        if state.msix.is_some() != self.msix.is_some() {
            return Err(DeviceStateError::invalid(format!(
                "payload {} an MSI-X table, this device {} one",
                if state.msix.is_some() {
                    "has"
                } else {
                    "has no"
                },
                if self.msix.is_some() {
                    "has"
                } else {
                    "has none"
                },
            )));
        }
        check_vector("config", state.config_msix_vector, table_len)?;

        let vs = self.virtio_state.lock().expect("virtio lock");
        let mut seen = vec![false; nq as usize];
        for qs in &state.queues {
            let idx = usize::from(qs.queue_idx);
            if qs.queue_idx >= nq {
                return Err(DeviceStateError::invalid(format!(
                    "queue {} is past this device's {nq} queues",
                    qs.queue_idx,
                )));
            }
            if std::mem::replace(&mut seen[idx], true) {
                return Err(DeviceStateError::invalid(format!(
                    "queue {} appears twice",
                    qs.queue_idx,
                )));
            }
            // A modern driver may shrink a ring below the maximum the
            // device offers (sec 4.1.4.3.2), and the source exports the
            // ring it ran. So the destination accepts any size it could
            // offer, not only its maximum.
            let max = vs.queues[idx].max_size();
            let size = qs.queue_size;
            if size == 0 || !size.is_power_of_two() || size > max {
                return Err(DeviceStateError::invalid(format!(
                    "queue {} has size {size}, not a power of two in \
                     1..={max}",
                    qs.queue_idx,
                )));
            }
            // The cursor difference is the number of chains in flight,
            // and a ring cannot hold more chains than descriptors. Both
            // cursors wrap in a u16.
            let in_flight = qs.avail_idx.wrapping_sub(qs.used_idx);
            if in_flight > qs.queue_size {
                return Err(DeviceStateError::invalid(format!(
                    "queue {} reports {in_flight} chains in flight on a \
                     {}-descriptor ring",
                    qs.queue_idx, qs.queue_size,
                )));
            }
            check_vector("queue", qs.msix_vector, table_len)?;
            if qs.live {
                // Use the payload's size. The ring bounds that the
                // device reads and writes follow from it, not from this
                // device's maximum.
                let mut probe = crate::queue::VirtQueue::new(size);
                probe.set_addr_modern(
                    qs.desc_addr,
                    qs.avail_addr,
                    qs.used_addr,
                );
                probe.set_event_idx(
                    state.features & bits::VIRTIO_F_RING_EVENT_IDX != 0,
                );
                probe.validate_addrs(&self.physmap).map_err(|error| {
                    DeviceStateError::invalid(format!(
                        "queue {} rings: {error}",
                        qs.queue_idx,
                    ))
                })?;
            }
        }
        Ok(())
    }

    /// Rebuild the transport from an export, in the order the module
    /// doc gives.
    pub(super) fn restore_migrate_queues(
        &self,
        state: &VirtioMigrateState,
    ) -> Result<(), DeviceStateError> {
        self.check_migrate_state(state)?;
        self.restore_migrate_pci_state(&state.pci);

        let mut vs = self.virtio_state.lock().expect("virtio lock");
        self.device.reset_all_rings()?;

        vs.guest_features = state.features;
        self.device.set_features(state.features);
        // The guest does this in the feature register handler, which a
        // restore does not call. See step 2 of the module doc.
        let event_idx = state.features & bits::VIRTIO_F_RING_EVENT_IDX != 0;
        let indirect = state.features & bits::VIRTIO_F_RING_INDIRECT_DESC != 0;
        for q in &mut vs.queues {
            q.set_event_idx(event_idx);
            q.set_indirect_supported(indirect);
        }

        for qs in &state.queues {
            let idx = usize::from(qs.queue_idx);
            // Set the size before the addresses. The device reads and
            // writes `size` entries, so a ring restored at this
            // device's maximum overruns the ring the source ran. The
            // reset makes the queue unconfigured, which is the only
            // state `set_size` accepts.
            vs.queues[idx].reset();
            vs.queues[idx].set_size(qs.queue_size).map_err(|error| {
                DeviceStateError::invalid(format!(
                    "queue {}: {error}",
                    qs.queue_idx,
                ))
            })?;
            vs.queues[idx].set_addr_modern(
                qs.desc_addr,
                qs.avail_addr,
                qs.used_addr,
            );
            // set_addr_modern zeroes the cursors. The guest continues
            // from the source's cursors.
            vs.queues[idx].set_last_avail_idx(qs.avail_idx);
            vs.queues[idx].set_shadow_used_idx(qs.used_idx);
            if idx < vs.queue_desc.len() {
                vs.queue_desc[idx] = qs.desc_addr;
                vs.queue_avail[idx] = qs.avail_addr;
                vs.queue_used[idx] = qs.used_addr;
            }
            if idx < vs.queue_enabled.len() {
                vs.queue_enabled[idx] = qs.live;
            }
            if idx < vs.queue_msix_vectors.len() {
                vs.queue_msix_vectors[idx] = qs.msix_vector;
            }
            if let Some(a) = self.msix_queue_vectors.get(idx) {
                a.store(qs.msix_vector, Ordering::Release);
            }
        }

        // Restore the driver's status byte as it was. A synthesised
        // DRIVER_OK tells a guest whose driver never finished the
        // handshake that the device is ready.
        vs.status = state.status;
        vs.config_msix_vector = state.config_msix_vector;
        self.msix_config_vector
            .store(state.config_msix_vector, Ordering::Release);

        // Masks and pending bits come back with the table. An INTx
        // guest arrives with MSI-X disabled, and it must stay disabled.
        // Enabled MSI-X moves device config from 0x14 to 0x18 and
        // routes every queue interrupt to NO_VECTOR.
        if let (Some(msix), Some(table)) = (self.msix.as_ref(), &state.msix) {
            msix.import_state(table).map_err(|error| {
                DeviceStateError::invalid(format!("MSI-X table: {error}"))
            })?;
        }

        for qs in &state.queues {
            if !qs.live {
                continue;
            }
            let (msix_addr, msix_data) = self.msix_entry(qs.msix_vector);
            self.device.restore_ring_state(
                qs.queue_idx,
                qs.queue_size,
                qs.desc_addr,
                qs.avail_addr,
                qs.used_addr,
                qs.avail_idx,
                qs.used_idx,
                msix_addr,
                msix_data,
            )?;
        }

        let (bar0_port, bar2_addr) = self.notify_bases();
        if bar0_port != 0 || bar2_addr != 0 {
            let notify_pio = if bar0_port != 0 {
                bar0_port + bits::LEGACY_REG_QUEUE_NOTIFY
            } else {
                0
            };
            let notify_mmio = if bar2_addr != 0 {
                bar2_addr + bits::MODERN_BAR_NOTIFY_OFFSET as u64
            } else {
                0
            };
            self.device.set_notify_addrs(notify_pio, notify_mmio);
        }

        // Rings, vectors and notification addresses are all programmed.
        self.device.start_poll_deferred();

        drop(vs);
        // Step 4 of the module doc: wake each live queue.
        for qs in &state.queues {
            if qs.live {
                self.raise_queue_interrupt_current(qs.queue_idx);
            }
        }
        Ok(())
    }

    /// The MSI-X address and data for a vector, from the restored
    /// table. `(0, 0)` when the queue has no vector.
    fn msix_entry(&self, vector: u16) -> (u64, u32) {
        if vector == bits::VIRTIO_MSI_NO_VECTOR {
            return (0, 0);
        }
        match self.msix.as_ref() {
            Some(msix) => {
                let (addr, data) = msix.read_entry(vector);
                (addr, data as u32)
            }
            None => (0, 0),
        }
    }

    /// The PIO port and MMIO base of the notification registers.
    fn notify_bases(&self) -> (u16, u64) {
        let pci = self.pci_state.lock().expect("pci lock");
        let bar0_port = pci
            .bars()
            .get(BarN::BAR0)
            .and_then(|(def, addr)| def.is_pio().then_some(addr as u16))
            .unwrap_or(0);
        let bar2_addr = pci
            .bars()
            .get(BarN::BAR2)
            .and_then(|(def, addr)| def.is_mmio().then_some(addr))
            .unwrap_or(0);
        (bar0_port, bar2_addr)
    }
}

/// An MSI-X vector the destination's table cannot hold.
fn check_vector(
    what: &str,
    vector: u16,
    table_len: u16,
) -> Result<(), DeviceStateError> {
    if vector == bits::VIRTIO_MSI_NO_VECTOR || vector < table_len {
        return Ok(());
    }
    Err(DeviceStateError::invalid(format!(
        "{what} MSI-X vector {vector} is past the {table_len}-entry table",
    )))
}
