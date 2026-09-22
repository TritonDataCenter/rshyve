// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The command engine resolves guest mappings on each use, so no cached host
//! pointer can bypass their bounds or lifetime.

use super::atapi::{dispatch, AtapiOutcome, SenseState};
use super::bits::*;
pub use super::decode::{
    cmd_table_len, parse_cmd_hdr, parse_prdt, CmdHdr, HdrError, Prd, PrdtError,
};
use super::{reset_port_state, AhciCtrl, AhciState};
use std::mem::size_of;
use vmm_core::mem::GuestIoVec;

pub fn build_d2h_fis(
    cfis: &[u8; FIS_D2H_LEN],
    tfd: u32,
    intr: bool,
) -> [u8; FIS_D2H_LEN] {
    let mut fis = [0u8; FIS_D2H_LEN];
    fis[0] = FIS_TYPE_REGD2H;
    fis[1] = if intr { 0x40 } else { 0 };
    fis[2] = (tfd & 0xff) as u8;
    fis[3] = ((tfd >> 8) & 0xff) as u8;
    fis[4..14].copy_from_slice(&cfis[4..14]);
    fis
}

pub fn build_piosetup_fis() -> [u8; FIS_PIOSETUP_LEN] {
    let mut fis = [0u8; FIS_PIOSETUP_LEN];
    fis[0] = FIS_TYPE_PIOSETUP;
    fis
}

pub fn build_reset_d2h_fis() -> [u8; FIS_D2H_LEN] {
    let mut fis = [0u8; FIS_D2H_LEN];
    fis[0] = FIS_TYPE_REGD2H;
    fis[3] = 1;
    fis[4] = 1;
    fis[5] = 0x14;
    fis[6] = 0xeb;
    fis[12] = 1;
    fis
}

pub fn atapi_set_reason(cfis: &mut [u8; FIS_D2H_LEN]) {
    // Set the reason in a local copy. A write into the guest command table
    // lets the guest race the completion FIS that is being built.
    cfis[4] = (cfis[4] & !0x07) | ATA_I_CMD | ATA_I_IN;
}

pub fn identify_packet_device(serial: &[u8; 20]) -> [u8; 512] {
    let mut identify = [0u8; 512];

    put_word(&mut identify, 0, 0x85c0);
    ata_bytes(&mut identify[10 * 2..20 * 2], serial);
    ata_string(&mut identify[23 * 2..27 * 2], "001");
    ata_string(&mut identify[27 * 2..47 * 2], "BHYVE SATA DVD ROM");
    put_word(&mut identify, 49, 0x0300);
    put_word(&mut identify, 50, 0x4001);
    put_word(&mut identify, 53, 0x0006);
    put_word(&mut identify, 62, 0x003f);
    put_word(&mut identify, 63, 0x0007);
    put_word(&mut identify, 64, 0x0003);
    for word in 65..=68 {
        put_word(&mut identify, word, 0x0078);
    }
    put_word(&mut identify, 76, 0x000e);
    put_word(&mut identify, 77, 0x0006);
    put_word(&mut identify, 78, 0x0010);
    put_word(&mut identify, 80, 0x03f0);
    put_word(&mut identify, 82, 0x4218);
    put_word(&mut identify, 83, 0x4000);
    put_word(&mut identify, 84, 0x4000);
    put_word(&mut identify, 85, 0x4218);
    put_word(&mut identify, 87, 0x4000);
    put_word(&mut identify, 88, 0x407f);
    put_word(&mut identify, 222, 0x1020);
    put_word(&mut identify, 255, 0x00a5);

    let sum = identify[..511]
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    identify[511] = 0u8.wrapping_sub(sum);
    identify
}

fn put_word(dest: &mut [u8; 512], word: usize, value: u16) {
    let offset = word * 2;
    dest[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn ata_bytes(dest: &mut [u8], src: &[u8]) {
    dest.fill(b' ');
    for (index, byte) in src.iter().copied().take(dest.len()).enumerate() {
        dest[index ^ 1] = byte;
    }
}

fn ata_string(dest: &mut [u8], src: &str) {
    ata_bytes(dest, src.as_bytes());
}

impl AhciCtrl {
    pub(super) fn hba_reset(&self, st: &mut AhciState) -> bool {
        st.hba.reset();
        st.port.ie = 0;
        st.port.is = 0;
        st.port.cmd = PXCMD_RESET;
        st.port.sctl = 0;
        self.port_reset(st)
    }

    pub(super) fn port_reset(&self, st: &mut AhciState) -> bool {
        reset_port_state(st);
        self.write_fis(st, FIS_OFF_RFIS, &build_reset_d2h_fis())
    }

    fn read_cmd_hdr(&self, st: &AhciState, slot: usize) -> Option<CmdHdr> {
        if slot >= NUM_SLOTS {
            return None;
        }
        let base = (u64::from(st.port.clbu) << 32) | u64::from(st.port.clb);
        let offset = slot.checked_mul(CL_ENTRY_SIZE)?;
        let address = base.checked_add(offset as u64)?;
        let sub = self.physmap.lookup(address, CL_ENTRY_SIZE)?;
        let mut raw = [0u8; CL_ENTRY_SIZE];
        sub.read_bytes(&mut raw).ok()?;
        parse_cmd_hdr(&raw).ok()
    }

    fn write_prdbc(&self, st: &AhciState, slot: usize, bytes: u32) -> bool {
        if slot >= NUM_SLOTS {
            return false;
        }
        #[cfg(test)]
        self.prdbc_writes
            .lock()
            .expect("ahci: PRDBC test log lock")
            .push((slot, bytes));
        let base = (u64::from(st.port.clbu) << 32) | u64::from(st.port.clb);
        let Some(offset) = slot
            .checked_mul(CL_ENTRY_SIZE)
            .and_then(|offset| offset.checked_add(4))
        else {
            return false;
        };
        let Some(address) = base.checked_add(offset as u64) else {
            return false;
        };
        let Some(sub) = self.physmap.lookup(address, size_of::<u32>()) else {
            return false;
        };
        sub.write_bytes(&bytes.to_le_bytes()).is_ok()
    }

    fn read_cmd_table(&self, hdr: &CmdHdr) -> Option<Vec<u8>> {
        let len = cmd_table_len(hdr.prdtl)?;
        let sub = self.physmap.lookup(hdr.ctba, len)?;
        let mut table = vec![0u8; len];
        sub.read_bytes(&mut table).ok()?;
        Some(table)
    }

    fn write_fis(&self, st: &mut AhciState, offset: usize, fis: &[u8]) -> bool {
        if st.port.cmd & PXCMD_FRE == 0 {
            return false;
        }
        let base = (u64::from(st.port.fbu) << 32) | u64::from(st.port.fb);
        let Some(address) = base.checked_add(offset as u64) else {
            return false;
        };
        let Some(sub) = self.physmap.lookup(address, fis.len()) else {
            return false;
        };
        if sub.write_bytes(fis).is_err() {
            return false;
        }

        let mut irq = if fis.get(1).is_some_and(|flags| flags & 0x40 != 0) {
            match offset {
                FIS_OFF_RFIS => PXIS_DHRS,
                FIS_OFF_PSFIS => PXIS_PSS,
                _ => 0,
            }
        } else {
            0
        };
        if fis
            .get(2)
            .is_some_and(|status| u32::from(*status) & ATA_S_ERROR != 0)
        {
            irq |= PXIS_TFES;
        }
        if irq != 0 && !st.port.is & irq != 0 {
            st.port.is |= irq;
            return true;
        }
        false
    }

    fn scatter_to_prdt(&self, prds: &[Prd], data: &[u8]) -> Option<usize> {
        let mut transferred = 0usize;
        for prd in prds {
            let remaining = data.get(transferred..)?;
            if remaining.is_empty() {
                break;
            }
            let chunk = remaining.len().min(prd.len as usize);
            let sub = self.physmap.lookup(prd.dba, chunk)?;
            if sub.write_bytes(&remaining[..chunk]).is_err() {
                return None;
            }
            let next = transferred.checked_add(chunk)?;
            transferred = next;
        }
        Some(transferred)
    }

    fn record_prdbc(&self, st: &AhciState, slot: usize, bytes: u32) {
        if !self.write_prdbc(st, slot, bytes) {
            slog::debug!(self.log, "ahci: failed to write PRDBC"; "slot" => slot);
        }
    }

    fn complete_packet_check(
        &self,
        st: &mut AhciState,
        slot: usize,
        cfis: &mut [u8; FIS_D2H_LEN],
        sense_key: u8,
        asc: u8,
    ) -> bool {
        st.port.sense_key = sense_key;
        st.port.asc = asc;
        self.record_prdbc(st, slot, 0);
        self.complete_atapi(st, slot, cfis, tfd_check_condition(sense_key))
    }

    fn read_packet_blocks(
        &self,
        st: &mut AhciState,
        slot: usize,
        cfis: &mut [u8; FIS_D2H_LEN],
        table: &[u8],
        prdtl: u16,
        lba: u64,
        count: u32,
    ) -> bool {
        if count == 0 {
            self.record_prdbc(st, slot, 0);
            return self.complete_atapi(st, slot, cfis, TFD_OK);
        }

        let Some((offset, bytes)) = self.media.read_range(lba, count) else {
            return self.complete_packet_check(
                st,
                slot,
                cfis,
                SENSE_ILLEGAL_REQUEST,
                ASC_LBA_OUT_OF_RANGE,
            );
        };
        let Ok(prds) = parse_prdt(table, prdtl) else {
            return self.complete_packet_check(
                st,
                slot,
                cfis,
                SENSE_ILLEGAL_REQUEST,
                ASC_INVALID_FIELD_IN_CDB,
            );
        };

        let mut iov = GuestIoVec::with_capacity(prds.len());
        let mut remaining = bytes;
        for prd in &prds {
            if remaining == 0 {
                break;
            }
            let chunk = (prd.len as usize).min(remaining);
            let Some(sub) = self.physmap.lookup(prd.dba, chunk) else {
                return self.complete_packet_check(
                    st,
                    slot,
                    cfis,
                    SENSE_ILLEGAL_REQUEST,
                    ASC_INVALID_FIELD_IN_CDB,
                );
            };
            iov.push(sub);
            remaining -= chunk;
        }

        let want = bytes - remaining;
        // MMIO dispatch releases its address-space lock before this call. A
        // synchronous read stalls only the issuing vCPU, and it leaves no I/O
        // for a lifecycle quiesce or a reboot to drain.
        let nread = iov.read_from(self.media.file(), offset);

        if !matches!(nread, Ok(n) if n == want) {
            return self.complete_packet_check(
                st,
                slot,
                cfis,
                SENSE_ILLEGAL_REQUEST,
                ASC_LBA_OUT_OF_RANGE,
            );
        }

        self.record_prdbc(st, slot, want as u32);
        self.complete_atapi(st, slot, cfis, TFD_OK)
    }

    fn handle_packet_command(
        &self,
        st: &mut AhciState,
        slot: usize,
        cfis: &mut [u8; FIS_D2H_LEN],
        table: &[u8],
        prdtl: u16,
    ) -> bool {
        let mut acmd = [0u8; ACMD_LEN];
        acmd.copy_from_slice(
            &table[CMD_TBL_ACMD_OFF..CMD_TBL_ACMD_OFF + ACMD_LEN],
        );
        let mut sense = SenseState {
            key: st.port.sense_key,
            asc: st.port.asc,
        };
        let outcome = dispatch(&acmd, self.media.blocks(), &mut sense);
        st.port.sense_key = sense.key;
        st.port.asc = sense.asc;

        match outcome {
            AtapiOutcome::Ok => {
                self.record_prdbc(st, slot, 0);
                self.complete_atapi(st, slot, cfis, TFD_OK)
            }
            AtapiOutcome::Data(buf) => {
                let Ok(prds) = parse_prdt(table, prdtl) else {
                    return self.complete_packet_check(
                        st,
                        slot,
                        cfis,
                        SENSE_ILLEGAL_REQUEST,
                        ASC_INVALID_FIELD_IN_CDB,
                    );
                };
                let Some(transferred) = self.scatter_to_prdt(&prds, &buf)
                else {
                    return self.complete_packet_check(
                        st,
                        slot,
                        cfis,
                        SENSE_ILLEGAL_REQUEST,
                        ASC_INVALID_FIELD_IN_CDB,
                    );
                };
                self.record_prdbc(st, slot, transferred as u32);
                self.complete_atapi(st, slot, cfis, TFD_OK)
            }
            AtapiOutcome::ReadBlocks { lba, count } => self
                .read_packet_blocks(st, slot, cfis, table, prdtl, lba, count),
            AtapiOutcome::Check { sense_key, asc } => {
                self.complete_packet_check(st, slot, cfis, sense_key, asc)
            }
        }
    }

    fn complete(
        &self,
        st: &mut AhciState,
        slot: usize,
        cfis: &[u8; FIS_D2H_LEN],
        tfd: u32,
    ) -> bool {
        #[cfg(test)]
        {
            let mut count = self
                .completion_count
                .lock()
                .expect("ahci: completion test count lock");
            *count += 1;
        }
        let fis = build_d2h_fis(cfis, tfd, true);
        if tfd & ATA_S_ERROR == 0 {
            st.port.ci &= !(1u32 << slot);
        } else {
            // CI stays set for an error, so the slot belongs to the
            // driver's CLO whether or not the FIS could be posted.
            // Otherwise the dispatch loop finds the slot idle and runs
            // the same command again, once per slot, per doorbell.
            st.port.wait_for_clear = true;
        }
        st.port.tfd = tfd;
        self.write_fis(st, FIS_OFF_RFIS, &fis)
    }

    fn complete_atapi(
        &self,
        st: &mut AhciState,
        slot: usize,
        cfis: &mut [u8; FIS_D2H_LEN],
        tfd: u32,
    ) -> bool {
        atapi_set_reason(cfis);
        self.complete(st, slot, cfis, tfd)
    }

    fn abort_malformed_slot(
        &self,
        st: &mut AhciState,
        slot: usize,
        cfis: &[u8; FIS_D2H_LEN],
    ) -> bool {
        let needs_intr = self.complete(st, slot, cfis, TFD_ABORT);
        // CLO cannot repair an unreadable command: no command state exists
        // to resume. A CI bit left set only wedges the HBA.
        st.port.ci &= !(1u32 << slot);
        st.port.wait_for_clear = false;
        needs_intr
    }

    pub(super) fn handle_command_fis(
        &self,
        st: &mut AhciState,
        slot: usize,
        mut cfis: [u8; FIS_D2H_LEN],
        table: &[u8],
        prdtl: u16,
    ) -> bool {
        if cfis[0] != FIS_TYPE_REGH2D {
            st.port.ci &= !(1u32 << slot);
            return false;
        }
        if cfis[1] & 0x80 == 0 {
            let needs_intr = if cfis[15] & (1 << 2) != 0 {
                st.port.reset_pending = true;
                false
            } else if st.port.reset_pending {
                st.port.reset_pending = false;
                self.port_reset(st)
            } else {
                false
            };
            st.port.ci &= !(1u32 << slot);
            return needs_intr;
        }

        st.port.tfd |= ATA_S_BUSY;
        match cfis[2] {
            ATA_ATAPI_IDENTIFY => {
                let Ok(prds) = parse_prdt(table, prdtl) else {
                    return self.complete_atapi(st, slot, &mut cfis, TFD_ABORT);
                };
                let mut needs_intr =
                    self.write_fis(st, FIS_OFF_PSFIS, &build_piosetup_fis());
                let identify = identify_packet_device(self.media.serial());
                let Some(transferred) = self.scatter_to_prdt(&prds, &identify)
                else {
                    return self.complete_packet_check(
                        st,
                        slot,
                        &mut cfis,
                        SENSE_ILLEGAL_REQUEST,
                        ASC_INVALID_FIELD_IN_CDB,
                    );
                };
                self.record_prdbc(st, slot, transferred as u32);
                needs_intr |= self.complete_atapi(st, slot, &mut cfis, TFD_OK);
                needs_intr
            }
            ATA_PACKET_CMD => {
                self.handle_packet_command(st, slot, &mut cfis, table, prdtl)
            }
            ATA_ATA_IDENTIFY => {
                self.complete_atapi(st, slot, &mut cfis, TFD_ABORT)
            }
            ATA_SETFEATURES => {
                let tfd = if cfis[3] == 0x03
                    || (cfis[3] == 0x10 && cfis[12] == 0x05)
                {
                    TFD_OK
                } else {
                    TFD_ABORT
                };
                self.complete_atapi(st, slot, &mut cfis, tfd)
            }
            ATA_CHECK_POWER_MODE => {
                cfis[12] = 0xff;
                self.complete_atapi(st, slot, &mut cfis, TFD_OK)
            }
            ATA_STANDBY_IMMEDIATE
            | ATA_IDLE_IMMEDIATE
            | ATA_STANDBY_CMD
            | ATA_IDLE_CMD
            | ATA_SLEEP
            | ATA_READ_VERIFY
            | ATA_READ_VERIFY48 => {
                self.complete_atapi(st, slot, &mut cfis, TFD_OK)
            }
            ATA_NOP | ATA_SMART_CMD | ATA_SECURITY_FREEZE_LOCK => {
                self.complete_atapi(st, slot, &mut cfis, TFD_ABORT)
            }
            _ => self.complete_atapi(st, slot, &mut cfis, TFD_ABORT),
        }
    }

    fn handle_slot(&self, st: &mut AhciState, slot: usize) -> bool {
        let Some(hdr) = self.read_cmd_hdr(st, slot) else {
            return self.abort_malformed_slot(st, slot, &[0; FIS_D2H_LEN]);
        };
        let Some(table) = self.read_cmd_table(&hdr) else {
            return self.abort_malformed_slot(st, slot, &[0; FIS_D2H_LEN]);
        };
        let mut cfis = [0u8; FIS_D2H_LEN];
        cfis.copy_from_slice(&table[..FIS_D2H_LEN]);

        self.handle_command_fis(st, slot, cfis, &table, hdr.prdtl)
    }

    pub(super) fn handle_port(&self, st: &mut AhciState) -> bool {
        if st.port.cmd & PXCMD_ST == 0 {
            return false;
        }

        let mut needs_intr = false;
        // The bound stops a malformed guest command from spinning a vCPU,
        // even if a command-completion invariant breaks.
        for _ in 0..NUM_SLOTS {
            if st.port.ci == 0 {
                break;
            }
            if st.port.tfd & (ATA_S_BUSY | ATA_S_DRQ) != 0 {
                break;
            }
            if st.port.wait_for_clear {
                break;
            }
            let ccs = st.port.ci.trailing_zeros() as usize;
            st.port.cmd = (st.port.cmd & !PXCMD_CCS_MASK)
                | ((ccs as u32) << PXCMD_CCS_SHIFT);
            needs_intr |= self.handle_slot(st, ccs);
        }
        needs_intr
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::ahci::media::IsoMedia;
    use vmm_core::mem::PhysMap;
    use vmm_core::mmio::MmioBus;

    use super::*;

    const TEST_CLB: u64 = 0x1000;
    const TEST_FB: u64 = 0x2000;
    const TEST_CTBA: u64 = 0x3000;

    fn test_controller(physmap: Arc<PhysMap>) -> Arc<AhciCtrl> {
        let file =
            tempfile::NamedTempFile::new().expect("create temporary ISO");
        file.as_file().set_len(4096).expect("size temporary ISO");
        AhciCtrl::new(
            IsoMedia::open(file.path()).expect("open temporary ISO"),
            physmap,
            Arc::new(MmioBus::new()),
            None,
            slog::Logger::root(slog::Discard, slog::o!()),
        )
    }

    fn anonymous_controller(gpa: u64, len: usize) -> Arc<AhciCtrl> {
        test_controller(Arc::new(
            PhysMap::new_anon(gpa, len).expect("create anonymous guest memory"),
        ))
    }

    fn command_header(prdtl: u16, ctba: u64) -> [u8; CL_ENTRY_SIZE] {
        let mut raw = [0u8; CL_ENTRY_SIZE];
        let flags = 5u16 | (1u16 << 5);
        raw[0..2].copy_from_slice(&flags.to_le_bytes());
        raw[2..4].copy_from_slice(&prdtl.to_le_bytes());
        raw[8..16].copy_from_slice(&ctba.to_le_bytes());
        raw
    }

    fn command_fis(command: u8) -> [u8; FIS_D2H_LEN] {
        let mut cfis = [0u8; FIS_D2H_LEN];
        cfis[0] = FIS_TYPE_REGH2D;
        cfis[1] = 0x80;
        cfis[2] = command;
        cfis
    }

    fn packet_table(prdtl: u16, cdb: &[u8]) -> Vec<u8> {
        let mut table = vec![0u8; cmd_table_len(prdtl).expect("bounded PRDTL")];
        table[..FIS_D2H_LEN].copy_from_slice(&command_fis(ATA_PACKET_CMD));
        table[CMD_TBL_ACMD_OFF..CMD_TBL_ACMD_OFF + cdb.len()]
            .copy_from_slice(cdb);
        table
    }

    fn write_guest(ctrl: &AhciCtrl, gpa: u64, data: &[u8]) {
        ctrl.physmap
            .lookup(gpa, data.len())
            .expect("mapped guest range")
            .write_bytes(data)
            .expect("write guest range");
    }

    fn start_slot_zero(ctrl: &AhciCtrl, clb: u64) -> bool {
        let mut st = ctrl.state.lock().expect("ahci: state lock");
        st.port.clb = clb as u32;
        st.port.clbu = (clb >> 32) as u32;
        st.port.cmd = PXCMD_ST | PXCMD_CR;
        st.port.ci = 1;
        ctrl.handle_port(&mut st)
    }

    fn assert_packet_table_error(table: &[u8], prdtl: u16) {
        let ctrl = test_controller(Arc::new(PhysMap::new()));
        let mut st = ctrl.state.lock().expect("ahci: state lock");
        st.port.cmd = PXCMD_ST | PXCMD_CR | PXCMD_FRE | PXCMD_FR;
        st.port.ci = 1;

        let needs_intr = ctrl.handle_command_fis(
            &mut st,
            0,
            command_fis(ATA_PACKET_CMD),
            table,
            prdtl,
        );

        assert!(!needs_intr);
        assert_ne!(st.port.tfd & ATA_S_ERROR, 0);
        assert_eq!(st.port.ci, 1);
        assert_eq!(st.port.is, 0);
        assert_eq!(st.port.sense_key, SENSE_ILLEGAL_REQUEST);
        assert_eq!(st.port.asc, ASC_INVALID_FIELD_IN_CDB);
        assert_eq!(
            *ctrl
                .completion_count
                .lock()
                .expect("ahci: completion test count lock"),
            1,
        );
    }

    #[test]
    fn build_d2h_fis_layout() {
        let mut cfis = [0u8; FIS_D2H_LEN];
        cfis[4..14].copy_from_slice(&[4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);

        let fis = build_d2h_fis(&cfis, 0xabcd, true);

        assert_eq!(fis[0], 0x34);
        assert_eq!(fis[1], 0x40);
        assert_eq!(fis[2], 0xcd);
        assert_eq!(fis[3], 0xab);
        assert_eq!(&fis[4..14], &cfis[4..14]);
        assert_eq!(&fis[14..20], &[0; 6]);
    }

    #[test]
    fn build_d2h_fis_no_interrupt_bit() {
        assert_eq!(build_d2h_fis(&[0; FIS_D2H_LEN], TFD_OK, false)[1], 0);
    }

    #[test]
    fn reset_fis_carries_atapi_signature() {
        let fis = build_reset_d2h_fis();
        assert_eq!(fis[5], 0x14);
        assert_eq!(fis[6], 0xeb);
    }

    #[test]
    fn atapi_set_reason_sets_cmd_and_in() {
        let mut cfis = [0u8; FIS_D2H_LEN];
        cfis[4] = 0xf8;
        atapi_set_reason(&mut cfis);
        assert_eq!(cfis[4], 0xfb);
    }

    #[test]
    fn identify_is_512_bytes_and_checksums() {
        let identify = identify_packet_device(b"BHYVE-0000-0000-0000");
        assert_eq!(identify.len(), 512);
        assert_eq!(u16::from_le_bytes([identify[0], identify[1]]), 0x85c0);
        assert_eq!(identify[510], 0xa5);
        assert_eq!(
            identify
                .iter()
                .fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
            0
        );
    }

    #[test]
    fn identify_model_string_is_byte_swapped() {
        let identify = identify_packet_device(b"BHYVE-0000-0000-0000");
        assert_eq!(&identify[27 * 2..27 * 2 + 2], b"HB");
    }

    #[test]
    fn unmapped_clb_aborts_and_clears_ci() {
        let ctrl = test_controller(Arc::new(PhysMap::new()));

        let needs_intr = start_slot_zero(&ctrl, TEST_CLB);
        let st = ctrl.state.lock().expect("ahci: state lock");

        assert!(!needs_intr);
        assert_eq!(st.port.tfd, TFD_ABORT);
        assert_eq!(st.port.ci, 0);
        assert_eq!(st.port.cmd & PXCMD_CR, PXCMD_CR);
    }

    #[test]
    fn command_header_straddling_region_aborts_and_clears_ci() {
        let region_end = TEST_CLB + 16;
        let ctrl = anonymous_controller(0, region_end as usize);

        let needs_intr = start_slot_zero(&ctrl, TEST_CLB);
        let st = ctrl.state.lock().expect("ahci: state lock");

        assert!(!needs_intr);
        assert_eq!(st.port.tfd, TFD_ABORT);
        assert_eq!(st.port.ci, 0);
    }

    #[test]
    fn an_unpostable_error_holds_the_slot_instead_of_rerunning_it() {
        // Either an unmapped FIS area or FIS receive turned off stops
        // the error FIS reaching the driver.
        for cmd in [
            PXCMD_ST | PXCMD_CR | PXCMD_FRE | PXCMD_FR,
            PXCMD_ST | PXCMD_CR,
        ] {
            let ctrl = test_controller(Arc::new(PhysMap::new()));
            let mut st = ctrl.state.lock().expect("ahci: state lock");
            st.port.fb = TEST_FB as u32;
            st.port.cmd = cmd;
            st.port.ci = 1;

            let needs_intr = ctrl.handle_command_fis(
                &mut st,
                0,
                command_fis(ATA_ATA_IDENTIFY),
                &[0; CMD_TBL_PRDT_OFF],
                0,
            );

            assert!(!needs_intr);
            assert_eq!(st.port.tfd, TFD_ABORT);
            assert_eq!(st.port.ci, 1);
            assert_eq!(st.port.is, 0);
            assert!(st.port.wait_for_clear, "the slot was left runnable");

            // A further doorbell must not run the same command again.
            ctrl.handle_port(&mut st);
            assert_eq!(
                *ctrl
                    .completion_count
                    .lock()
                    .expect("ahci: completion test count lock"),
                1,
            );
        }
    }

    #[test]
    fn unmapped_ctba_aborts_and_clears_ci() {
        let ctrl = anonymous_controller(TEST_CLB, CL_SIZE);
        write_guest(&ctrl, TEST_CLB, &command_header(0, TEST_CTBA));

        let needs_intr = start_slot_zero(&ctrl, TEST_CLB);
        let st = ctrl.state.lock().expect("ahci: state lock");

        assert!(!needs_intr);
        assert_eq!(st.port.tfd, TFD_ABORT);
        assert_eq!(st.port.ci, 0);
    }

    #[test]
    fn command_table_straddling_region_aborts_and_clears_ci() {
        let region_end = TEST_CTBA + CMD_TBL_PRDT_OFF as u64 - 1;
        let ctrl =
            anonymous_controller(TEST_CLB, (region_end - TEST_CLB) as usize);
        write_guest(&ctrl, TEST_CLB, &command_header(0, TEST_CTBA));

        let needs_intr = start_slot_zero(&ctrl, TEST_CLB);
        let st = ctrl.state.lock().expect("ahci: state lock");

        assert!(!needs_intr);
        assert_eq!(st.port.tfd, TFD_ABORT);
        assert_eq!(st.port.ci, 0);
    }

    #[test]
    fn prdtl_boundaries_abort_unmapped_tables_without_losing_ci_state() {
        for prdtl in [0, 1, MAX_PRDTL, MAX_PRDTL + 1, u16::MAX] {
            let ctrl = anonymous_controller(TEST_CLB, CL_SIZE);
            write_guest(&ctrl, TEST_CLB, &command_header(prdtl, TEST_CTBA));

            let needs_intr = start_slot_zero(&ctrl, TEST_CLB);
            let st = ctrl.state.lock().expect("ahci: state lock");

            assert!(!needs_intr, "PRDTL {prdtl}");
            assert_eq!(st.port.tfd, TFD_ABORT, "PRDTL {prdtl}");
            assert_eq!(st.port.ci, 0, "PRDTL {prdtl}");
        }
    }

    #[test]
    fn max_dba_with_nonzero_dbc_completes_with_error() {
        let mut table = packet_table(1, &[SCSI_INQUIRY, 0, 0, 0, 36]);
        table[CMD_TBL_PRDT_OFF..CMD_TBL_PRDT_OFF + 8]
            .copy_from_slice(&u64::MAX.to_le_bytes());
        table[CMD_TBL_PRDT_OFF + 12..CMD_TBL_PRDT_OFF + 16]
            .copy_from_slice(&1u32.to_le_bytes());

        assert_packet_table_error(&table, 1);
    }

    #[test]
    fn aligned_dba_end_overflow_completes_with_error() {
        let mut table = packet_table(1, &[SCSI_INQUIRY, 0, 0, 0, 36]);
        table[CMD_TBL_PRDT_OFF..CMD_TBL_PRDT_OFF + 8]
            .copy_from_slice(&(u64::MAX - 1).to_le_bytes());
        table[CMD_TBL_PRDT_OFF + 12..CMD_TBL_PRDT_OFF + 16]
            .copy_from_slice(&1u32.to_le_bytes());

        assert_packet_table_error(&table, 1);
    }

    #[test]
    fn prdt_total_over_limit_completes_with_error() {
        let mut table = packet_table(2, &[SCSI_INQUIRY, 0, 0, 0, 36]);
        table[CMD_TBL_PRDT_OFF + 12..CMD_TBL_PRDT_OFF + 16]
            .copy_from_slice(&((MAX_XFER_BYTES as u32) - 1).to_le_bytes());
        let second = CMD_TBL_PRDT_OFF + PRD_ENTRY_SIZE;
        table[second..second + 8].copy_from_slice(&0x20_0000u64.to_le_bytes());
        table[second + 12..second + 16].copy_from_slice(&1u32.to_le_bytes());

        assert_packet_table_error(&table, 2);
    }

    #[test]
    fn maximum_lba_and_read12_count_complete_with_error() {
        let ctrl = test_controller(Arc::new(PhysMap::new()));
        let mut st = ctrl.state.lock().expect("ahci: state lock");
        st.port.cmd = PXCMD_ST | PXCMD_CR | PXCMD_FRE | PXCMD_FR;
        st.port.ci = 1;
        let mut cfis = command_fis(ATA_PACKET_CMD);

        let needs_intr = ctrl.read_packet_blocks(
            &mut st,
            0,
            &mut cfis,
            &[0; CMD_TBL_PRDT_OFF],
            0,
            u64::MAX,
            u32::MAX,
        );

        assert!(!needs_intr);
        assert_ne!(st.port.tfd & ATA_S_ERROR, 0);
        assert_eq!(st.port.ci, 1);
        assert_eq!(st.port.sense_key, SENSE_ILLEGAL_REQUEST);
        assert_eq!(st.port.asc, ASC_LBA_OUT_OF_RANGE);
        assert_eq!(
            *ctrl
                .completion_count
                .lock()
                .expect("ahci: completion test count lock"),
            1,
        );
    }

    #[test]
    fn abort_in_slot_zero_stalls_all_ci_dispatch() {
        let ctrl = anonymous_controller(0, 0x4000);
        write_guest(&ctrl, TEST_CLB, &command_header(0, TEST_CTBA));
        write_guest(&ctrl, TEST_CTBA, &command_fis(ATA_ATA_IDENTIFY));
        let mut st = ctrl.state.lock().expect("ahci: state lock");
        st.port.clb = TEST_CLB as u32;
        st.port.fb = TEST_FB as u32;
        st.port.cmd = PXCMD_ST | PXCMD_CR | PXCMD_FRE | PXCMD_FR;
        st.port.ci = u32::MAX;

        let needs_intr = ctrl.handle_port(&mut st);

        assert!(needs_intr);
        assert_eq!(st.port.tfd, TFD_ABORT);
        assert_eq!(st.port.ci, u32::MAX);
        assert!(st.port.wait_for_clear);
        assert_ne!(st.port.is & PXIS_TFES, 0);
        assert_eq!(
            *ctrl
                .completion_count
                .lock()
                .expect("ahci: completion test count lock"),
            1,
        );
    }

    #[test]
    fn zero_length_read_completes_once() {
        let ctrl = test_controller(Arc::new(PhysMap::new()));
        let mut table = vec![0u8; CMD_TBL_PRDT_OFF];
        table[CMD_TBL_ACMD_OFF] = SCSI_READ_10;
        let mut cfis = [0u8; FIS_D2H_LEN];
        cfis[0] = FIS_TYPE_REGH2D;
        cfis[1] = 0x80;
        cfis[2] = ATA_PACKET_CMD;
        let mut st = ctrl.state.lock().expect("ahci: state lock");
        st.port.ci = 1;

        let needs_intr = ctrl.handle_command_fis(&mut st, 0, cfis, &table, 1);

        assert!(!needs_intr);
        assert_eq!(st.port.tfd, TFD_OK);
        assert_eq!(st.port.ci, 0);
        assert_eq!(
            *ctrl.prdbc_writes.lock().expect("ahci: PRDBC test log lock"),
            vec![(0, 0)],
        );
        assert_eq!(
            *ctrl
                .completion_count
                .lock()
                .expect("ahci: completion test count lock"),
            1,
        );
    }
}
