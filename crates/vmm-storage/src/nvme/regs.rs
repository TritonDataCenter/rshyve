// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! BAR0 register and doorbell decoding, BAR4 MSI-X window.

use std::sync::Arc;

use vmm_core::common::RWOp;
use vmm_core::mmio::MmioFn;

use vmm_devices::pci::device::PciDevice;
use vmm_devices::pci::BarN;

use vmm_devices::{FlushIntent, Lifecycle};

use super::bits::*;
use super::queues::{io_queue_index, valid_queue_base, CompQueue, SubQueue};
use super::NvmeController;

impl NvmeController {
    /// Unregister a recorded MMIO base and clear the record.
    pub(super) fn release_mmio(&self, bar: BarN, reg: &mut Option<u64>) {
        if let Some(addr) = reg.take() {
            if let Err(e) = self.bus_mmio.unregister(addr) {
                slog::warn!(self.log, "nvme: MMIO unregister failed";
                    "bar" => ?bar,
                    "addr" => format!("{addr:#x}"),
                    "error" => %e);
            }
        }
    }

    /// Update MMIO bus registration for BAR0 and BAR4.
    pub(super) fn update_mmio_registration(self: &Arc<Self>) {
        let pci = self.pci_state.lock().expect("nvme: pci lock");
        let mmio_enabled = pci
            .command()
            .contains(vmm_devices::pci::bits::RegCmd::MMIO_EN);

        for (bar, reg_mutex) in [
            (BarN::BAR0, &self.registered_bar0),
            (BarN::BAR4, &self.registered_bar4),
        ] {
            let mut reg = reg_mutex.lock().expect("bar reg lock");
            self.release_mmio(bar, &mut reg);
            if !mmio_enabled {
                continue;
            }
            let Some((def, addr)) = pci.bars().get(bar) else {
                continue;
            };
            if addr == 0 || !def.is_mmio() {
                continue;
            }
            let size = def.size();
            let dev = Arc::clone(self);
            let handler: Arc<MmioFn> =
                Arc::new(move |offset: usize, rwo: RWOp<'_>| {
                    dev.bar_rw(bar, offset, rwo);
                });
            match self.bus_mmio.register(addr, size, handler) {
                Ok(()) => *reg = Some(addr),
                // On failure the controller decodes nothing on this BAR.
                Err(e) => {
                    slog::error!(self.log,
                        "nvme: MMIO register failed, BAR dark";
                        "bar" => ?bar,
                        "addr" => format!("{addr:#x}"),
                        "size" => size,
                        "error" => %e);
                }
            }
        }
    }

    /// Handle BAR0 MMIO access (controller registers and doorbells).
    pub(super) fn bar0_rw(&self, offset: usize, rwo: RWOp<'_>) {
        if offset >= REG_DOORBELL_BASE {
            self.doorbell_rw(offset, rwo);
            return;
        }
        // A CC transition fences the worker pool, which cannot happen
        // under the state lock: a worker needs it to finish.
        if let (REG_CC, RWOp::Write(wo)) = (offset, &rwo) {
            self.write_cc(wo.read_u32());
            return;
        }
        self.reg_rw(offset, rwo);
    }

    /// Handle controller register reads/writes (offsets 0x00..0x38).
    fn reg_rw(&self, offset: usize, rwo: RWOp<'_>) {
        let mut st = self.state.lock().expect("nvme: state lock");
        match rwo {
            RWOp::Read(ro) => {
                let val = match offset {
                    REG_CAP => {
                        // CAP is 8 bytes. A 4-byte read gets the low dword.
                        if ro.len() == 8 {
                            ro.write_u64(st.cap);
                            return;
                        }
                        st.cap as u32
                    }
                    0x04 => (st.cap >> 32) as u32, // CAP high dword
                    REG_VS => NVME_VS_1_0,
                    REG_INTMS => 0, // unused with MSI-X
                    REG_INTMC => 0,
                    REG_CC => st.cc,
                    REG_CSTS => st.csts,
                    REG_AQA => st.aqa,
                    REG_ASQ => {
                        if ro.len() == 8 {
                            ro.write_u64(st.asq_base);
                            return;
                        }
                        st.asq_base as u32
                    }
                    0x2C => (st.asq_base >> 32) as u32,
                    REG_ACQ => {
                        if ro.len() == 8 {
                            ro.write_u64(st.acq_base);
                            return;
                        }
                        st.acq_base as u32
                    }
                    0x34 => (st.acq_base >> 32) as u32,
                    _ => 0,
                };
                if ro.len() == 8 {
                    ro.write_u64(u64::from(val));
                } else {
                    ro.write_dword_at(val, offset);
                }
            }
            RWOp::Write(wo) => {
                let val32 = wo.read_u32();
                match offset {
                    REG_AQA if (st.cc & CC_EN) == 0 => st.aqa = val32,
                    REG_ASQ if (st.cc & CC_EN) == 0 => {
                        if wo.len() == 8 {
                            st.asq_base = wo.read_u64();
                        } else {
                            st.asq_base = (st.asq_base & 0xFFFF_FFFF_0000_0000)
                                | u64::from(val32);
                        }
                    }
                    0x2C if (st.cc & CC_EN) == 0 => {
                        st.asq_base = (st.asq_base & 0x0000_0000_FFFF_FFFF)
                            | (u64::from(val32) << 32);
                    }
                    REG_ACQ if (st.cc & CC_EN) == 0 => {
                        if wo.len() == 8 {
                            st.acq_base = wo.read_u64();
                        } else {
                            st.acq_base = (st.acq_base & 0xFFFF_FFFF_0000_0000)
                                | u64::from(val32);
                        }
                    }
                    0x34 if (st.cc & CC_EN) == 0 => {
                        st.acq_base = (st.acq_base & 0x0000_0000_FFFF_FFFF)
                            | (u64::from(val32) << 32);
                    }
                    // INTMS and INTMC do nothing in MSI-X mode.
                    _ => {}
                }
            }
        }
    }

    /// Apply a CC write and whichever controller transition it asks for.
    ///
    /// Runs without the state lock held so a disable or a shutdown can
    /// wait for the workers.
    fn write_cc(&self, new_cc: u32) {
        let old_cc = self.state.lock().expect("nvme: state lock").cc;
        let was_enabled = (old_cc & CC_EN) != 0;
        let now_enabled = (new_cc & CC_EN) != 0;
        let shn = (new_cc >> CC_SHN_SHIFT) & CC_SHN_MASK;
        let old_shn = (old_cc >> CC_SHN_SHIFT) & CC_SHN_MASK;

        if shn != SHST_NORMAL && old_shn == SHST_NORMAL {
            self.shutdown(new_cc);
        }
        if was_enabled && !now_enabled {
            self.disable(new_cc);
        } else if !was_enabled && now_enabled {
            self.enable(new_cc);
        } else {
            self.state.lock().expect("nvme: state lock").cc = new_cc;
        }
    }

    /// Bring the controller up, or refuse the configuration.
    ///
    /// A controller that cannot honour CC reports a fatal status
    /// instead of RDY. A 64-byte SQE fetch from a queue the driver sized
    /// for other entries reads whatever follows the queue.
    fn enable(&self, new_cc: u32) {
        let mut st = self.state.lock().expect("nvme: state lock");
        st.cc = new_cc;

        // A fence deadline has already passed on this controller, so a
        // worker can still be inside a transfer on pages the driver has
        // since given to something else. New queues would name those
        // same pages.
        if st.fence_failed {
            st.csts |= CSTS_CFS;
            return;
        }

        let asqs = (st.aqa & AQA_ASQS_MASK) + 1;
        let acqs = ((st.aqa >> AQA_ACQS_SHIFT) & AQA_ACQS_MASK) + 1;
        // A zero entry size means the driver has not set one yet, which
        // it may do before it creates an I/O queue. Anything else must
        // match the required value the Identify data reports.
        let iosqes = (new_cc >> CC_IOSQES_SHIFT) & CC_IOSQES_MASK;
        let iocqes = (new_cc >> CC_IOCQES_SHIFT) & CC_IOCQES_MASK;
        let supported = (iosqes == 0 || iosqes == SQE_SIZE_LOG2)
            && (iocqes == 0 || iocqes == CQE_SIZE_LOG2)
            // MPSMIN and MPSMAX are both zero: 4 KiB pages only.
            && (new_cc >> CC_MPS_SHIFT) & CC_MPS_MASK == 0
            && (new_cc >> CC_CSS_SHIFT) & CC_CSS_MASK == 0
            // Round robin is the only arbitration this controller has.
            && (new_cc >> CC_AMS_SHIFT) & CC_AMS_MASK == 0;
        if !supported
            || asqs < 2
            || acqs < 2
            || !self.queue_base_ok(st.asq_base, asqs, SQE_SIZE)
            || !self.queue_base_ok(st.acq_base, acqs, CQE_SIZE)
        {
            slog::warn!(self.log, "nvme: refused CC.EN";
                "cc" => format!("{new_cc:#x}"),
                "aqa" => format!("{:#x}", st.aqa));
            st.csts |= CSTS_CFS;
            return;
        }

        let instance = st.next_instance();
        st.admin_sq = Some(SubQueue {
            base: st.asq_base,
            size: asqs as u16,
            head: 0,
            tail: 0,
            cq_id: ADMIN_QUEUE_ID,
            instance,
        });
        let instance = st.next_instance();
        st.admin_cq = Some(CompQueue {
            base: st.acq_base,
            size: acqs as u16,
            head: 0,
            tail: 0,
            phase: true,
            iv: 0,
            ien: true,
            instance,
        });
        st.csts &= !CSTS_CFS;
        st.csts |= CSTS_RDY;
    }

    /// Take the controller down.
    ///
    /// The queues go first so nothing new is admitted, then the fence
    /// waits for the transfers already moving guest memory: the driver
    /// is free to reuse those pages once RDY clears. The drain is held
    /// until RDY is clear, so nothing is admitted in between.
    fn disable(&self, new_cc: u32) {
        self.state.lock().expect("nvme: state lock").retire_queues();
        let fence = self.fence_workers(self.fences.reset);
        // The messages in the PBA name a driver that is gone. Left in
        // place, they are sent on the next unmask.
        self.msix.clear_pending();

        let mut st = self.state.lock().expect("nvme: state lock");
        st.cc = new_cc;
        if fence.is_none() {
            // RDY stays set. A clear RDY tells the driver it can reuse
            // the pages a worker still holds, and it reports a reset that
            // did not happen.
            st.fail_fence();
            return;
        }
        st.csts &= !CSTS_RDY;
        // A controller reset returns the shutdown status to normal.
        st.set_shst(SHST_NORMAL);
    }

    /// Carry out a shutdown notification.
    ///
    /// The driver takes SHST complete as its licence to cut power, so
    /// nothing may still be in flight and the write cache the Identify
    /// data advertises has to be on stable storage first.
    fn shutdown(&self, new_cc: u32) {
        {
            let mut st = self.state.lock().expect("nvme: state lock");
            st.cc = new_cc;
            st.set_shst(SHST_OCCURRING);
            st.retire_queues();
        }

        // Held across the flush and the status write, so the shutdown
        // the driver reads as complete covers every transfer. Without
        // it the status is a guess.
        let Some(_fence) = self.fence_workers(self.fences.shutdown) else {
            self.state.lock().expect("nvme: state lock").fail_fence();
            return;
        };

        let flushed = self.flush_backing(FlushIntent::Durable);
        if let Err(error) = &flushed {
            slog::error!(self.log, "nvme: shutdown flush failed";
                "error" => %error);
        }

        let mut st = self.state.lock().expect("nvme: state lock");
        if flushed.is_err() {
            st.csts |= CSTS_CFS;
            return;
        }
        st.set_shst(SHST_COMPLETE);
    }

    /// Whether a guest queue base may hold `entries` entries.
    ///
    /// Only the first entry must be mapped. The rest are checked when
    /// they are read, so a queue that legally straddles two memory
    /// regions passes.
    pub(super) fn queue_base_ok(
        &self,
        base: u64,
        entries: u32,
        entry: usize,
    ) -> bool {
        valid_queue_base(base)
            && u64::from(entries)
                .checked_mul(entry as u64)
                .and_then(|span| base.checked_add(span))
                .is_some()
            && self.physmap.lookup(base, entry).is_some()
    }

    /// Handle doorbell writes (offsets 0x1000+).
    ///
    /// Doorbell layout (DSTRD=0, 4-byte stride):
    /// - 0x1000 + (2*qid + 0) * 4 = SQ tail doorbell for queue qid
    /// - 0x1000 + (2*qid + 1) * 4 = CQ head doorbell for queue qid
    #[inline]
    fn doorbell_rw(&self, offset: usize, rwo: RWOp<'_>) {
        // Doorbells are write-only. Reads return 0.
        let wo = match rwo {
            RWOp::Write(wo) => wo,
            RWOp::Read(ro) => {
                ro.write_u32(0);
                return;
            }
        };

        let db_offset = offset - REG_DOORBELL_BASE;
        let db_index = db_offset / 4;
        let qid = (db_index / 2) as u16;
        let is_sq = db_index.is_multiple_of(2);
        let new_val = wo.read_u32() as u16;

        if is_sq {
            self.sq_doorbell(qid, new_val);
        } else {
            self.cq_doorbell(qid, new_val);
        }
    }

    /// Handle SQ tail doorbell write: process new submissions.
    #[inline]
    fn sq_doorbell(&self, qid: u16, new_tail: u16) {
        if qid == ADMIN_QUEUE_ID {
            self.admin_sq_doorbell(new_tail);
        } else {
            self.io_sq_doorbell(qid, new_tail);
        }
    }

    /// Handle CQ head doorbell write: update head pointer.
    #[inline]
    fn cq_doorbell(&self, qid: u16, new_head: u16) {
        let mut st = self.state.lock().expect("nvme: state lock");
        if qid == ADMIN_QUEUE_ID {
            if let Some(cq) = &mut st.admin_cq {
                if new_head < cq.size {
                    cq.head = new_head;
                }
            }
        } else {
            if let Some(idx) = io_queue_index(qid) {
                if let Some(cq) = &mut st.io_cqs[idx] {
                    if new_head < cq.size {
                        cq.head = new_head;
                    }
                }
            }
        }
    }

    /// Handle BAR4 MMIO access (MSI-X table + PBA).
    pub(super) fn bar4_rw(&self, offset: usize, rwo: RWOp<'_>) {
        let pba_offset = self.msix.pba_offset();
        match rwo {
            RWOp::Read(ro) => {
                let val = if offset >= pba_offset {
                    self.msix.pba_read(offset - pba_offset)
                } else {
                    self.msix.table_read(offset)
                };
                ro.write_dword(val)
            }
            RWOp::Write(wo) => {
                let val = wo.read_dword();
                // The PBA is read-only to the guest.
                if offset < pba_offset {
                    self.msix.table_write(offset, val);
                }
            }
        }
    }
}
