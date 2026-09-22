// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Command Response Buffer (CRB) MMIO interface, TCG PTP 2.0.
//!
//! Serves the locality-0 register layout at the standard base
//! 0xFED40000. Only locality 0 exists here, which is what every TPM 2.0
//! boot, attestation and BitLocker flow uses, and what QEMU's CRB
//! offers.
//!
//! The guest writes a command into the buffer at offset 0x80, then sets
//! INVOKE in `CTRL_START`. The MMIO handler runs the command through
//! libtpms on the writing vCPU, replaces the buffer contents with the
//! response and clears INVOKE, which is what the guest polls.
//!
//! libtpms has no internal locking, so [`Crb`] must be the only caller
//! and must have one command in flight. See [`crate`].

use std::ptr;
use std::sync::{Mutex, MutexGuard};

use slog::{debug, warn, Logger};
use vmm_core::common::{RWOp, ReadOp, WriteOp};
use vmm_tpm_sys::{tpmlib_free, TPMLIB_Process, TPM_RESULT, TPM_SUCCESS};

use crate::callbacks;

/// Standard MMIO base for locality 0 per TCG PTP.
pub const CRB_BASE: u64 = 0xFED4_0000;
/// Locality 0 only, one 0x1000 page.
pub const CRB_REGION_LEN: u64 = 0x1000;

const BUF_OFFSET: usize = 0x80;
/// The CRB command/response buffer fills the rest of the page. It is
/// also the size negotiated with libtpms (see [`crate::Tpm::new`]), so
/// the library refuses a longer command and never builds a longer
/// response.
pub const BUF_LEN: usize = 0x1000 - BUF_OFFSET;

// Register offsets (within the MMIO region).
const REG_LOC_STATE: usize = 0x00;
const REG_LOC_CTRL: usize = 0x08;
const REG_LOC_STS: usize = 0x0C;
const REG_INTF_ID: usize = 0x30;
const REG_INTF_ID2: usize = 0x34;
const REG_CTRL_EXT: usize = 0x38;
const REG_CTRL_REQ: usize = 0x40;
const REG_CTRL_STS: usize = 0x44;
const REG_CTRL_CANCEL: usize = 0x48;
const REG_CTRL_START: usize = 0x4C;
const REG_INT_ENABLED: usize = 0x50;
const REG_INT_STS: usize = 0x54;
const REG_CTRL_CMD_SIZE: usize = 0x58;
const REG_CTRL_CMD_LADDR: usize = 0x5C;
const REG_CTRL_CMD_HADDR: usize = 0x60;
const REG_CTRL_RSP_SIZE: usize = 0x64;
const REG_CTRL_RSP_ADDR: usize = 0x68;
const REG_CTRL_RSP_HADDR: usize = 0x6C;

// Bit definitions, mirroring TCG PTP 2.0.
const LOC_STATE_TPM_ESTABLISHED: u32 = 1 << 0;
const LOC_STATE_LOC_ASSIGNED: u32 = 1 << 1;
const LOC_STATE_TPM_REG_VALID: u32 = 1 << 7;

const LOC_CTRL_REQ_ACCESS: u32 = 1 << 0;
const LOC_CTRL_RELINQUISH: u32 = 1 << 1;
const LOC_CTRL_RESET_EST: u32 = 1 << 3;

const LOC_STS_GRANTED: u32 = 1 << 0;

const CTRL_REQ_CMD_READY: u32 = 1 << 0;
const CTRL_REQ_GO_IDLE: u32 = 1 << 1;

const CTRL_STS_TPM_IDLE: u32 = 1 << 1;

const CTRL_START_INVOKE: u32 = 1 << 0;

/// Static interface ID register value advertised to the guest.
///
/// Mirrors QEMU's CRB device:
///   InterfaceType = 1 (CRB active)
///   InterfaceVersion = 1
///   CapLocality = 0  (locality 0 only)
///   CapCRBIdleBypass = 0
///   CapDataXferSizeSupport = 3 (64-byte burst max)
///   CapFIFO = 0
///   CapCRB = 1
///   InterfaceSelector = 1 (CRB selected)
///   RID = 0
fn intf_id_value() -> u32 {
    let interface_type = 1u32; // bits 0..3
    let interface_version = 1u32; // bits 4..7
    let cap_locality = 0u32; // bit 8
    let cap_crb_idle_bypass = 0u32; // bit 9
    let cap_data_xfer = 3u32; // bits 11..12
    let cap_fifo = 0u32; // bit 13
    let cap_crb = 1u32; // bit 14
    let interface_selector = 1u32; // bits 17..18
    let intf_sel_lock = 0u32; // bit 19
    let rid = 0u32; // bits 24..31

    interface_type
        | (interface_version << 4)
        | (cap_locality << 8)
        | (cap_crb_idle_bypass << 9)
        | (cap_data_xfer << 11)
        | (cap_fifo << 13)
        | (cap_crb << 14)
        | (interface_selector << 17)
        | (intf_sel_lock << 19)
        | (rid << 24)
}

/// Register file and command buffer, as the guest sees them.
struct State {
    loc_state: u32,
    loc_ctrl: u32,
    loc_sts: u32,
    ctrl_req: u32,
    ctrl_sts: u32,
    ctrl_cancel: u32,
    ctrl_start: u32,

    /// Shared command/response buffer.
    buffer: Box<[u8; BUF_LEN]>,
}

impl State {
    fn new() -> Self {
        Self {
            loc_state: LOC_STATE_TPM_REG_VALID,
            loc_ctrl: 0,
            loc_sts: 0,
            // tpmIdle = 1 at power-on per TCG PTP. The guest must
            // write CMD_READY to clear idle before sending commands.
            ctrl_req: 0,
            ctrl_sts: CTRL_STS_TPM_IDLE,
            ctrl_cancel: 0,
            ctrl_start: 0,
            buffer: Box::new([0u8; BUF_LEN]),
        }
    }
}

/// CRB device. Owns the MMIO state and serves read/write requests
/// dispatched by `MmioBus`. Construct one per VM, register at
/// `CRB_BASE` with length `CRB_REGION_LEN`.
pub struct Crb {
    state: Mutex<State>,
    /// Held for one whole libtpms command. libtpms keeps its TPM state
    /// in C globals with no locking of its own, so two vCPUs inside
    /// `TPMLIB_Process` would race on them and on the NV files.
    ///
    /// It is separate from `state` so a register read answers while a
    /// command runs, and it is always taken before `state`, never with
    /// `state` already held.
    command: Mutex<()>,
    log: Logger,
}

impl Crb {
    pub fn new(log: Logger) -> Self {
        Self {
            state: Mutex::new(State::new()),
            command: Mutex::new(()),
            log,
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("CRB register lock")
    }

    /// Top-level MMIO dispatch. `offset` is relative to `CRB_BASE`.
    pub fn handle(&self, offset: usize, op: RWOp<'_>) {
        if offset >= BUF_OFFSET {
            self.handle_buffer(offset - BUF_OFFSET, op);
            return;
        }
        match op {
            RWOp::Read(ro) => self.handle_reg_read(offset, ro),
            RWOp::Write(wo) => self.handle_reg_write(offset, wo),
        }
    }

    fn handle_buffer(&self, buf_off: usize, op: RWOp<'_>) {
        let mut state = self.state();
        match op {
            RWOp::Read(ro) => {
                let mut tmp = [0u8; 8];
                let n = ro.len();
                if buf_off < BUF_LEN {
                    let take = n.min(BUF_LEN - buf_off);
                    tmp[..take].copy_from_slice(
                        &state.buffer[buf_off..buf_off + take],
                    );
                }
                match n {
                    1 => ro.write_u8(tmp[0]),
                    2 => ro.write_u16(u16::from_le_bytes([tmp[0], tmp[1]])),
                    4 => ro.write_u32(u32::from_le_bytes([
                        tmp[0], tmp[1], tmp[2], tmp[3],
                    ])),
                    8 => ro.write_u64(u64::from_le_bytes(tmp)),
                    _ => {}
                }
            }
            RWOp::Write(wo) => {
                let data = wo.buf();
                let len = data.len().min(BUF_LEN.saturating_sub(buf_off));
                if buf_off < BUF_LEN {
                    state.buffer[buf_off..buf_off + len]
                        .copy_from_slice(&data[..len]);
                }
            }
        }
    }

    fn handle_reg_read(&self, offset: usize, ro: &mut ReadOp) {
        let state = self.state();
        let value: u32 = match offset & !0x3 {
            REG_LOC_STATE => state.loc_state,
            REG_LOC_CTRL => state.loc_ctrl,
            REG_LOC_STS => state.loc_sts,
            REG_INTF_ID => intf_id_value(),
            REG_INTF_ID2 => 0x0000_1014, // VID=IBM, DID=0
            REG_CTRL_EXT => 0,
            REG_CTRL_REQ => state.ctrl_req,
            REG_CTRL_STS => state.ctrl_sts,
            REG_CTRL_CANCEL => state.ctrl_cancel,
            REG_CTRL_START => state.ctrl_start,
            REG_INT_ENABLED => 0,
            REG_INT_STS => 0,
            REG_CTRL_CMD_SIZE => BUF_LEN as u32,
            REG_CTRL_CMD_LADDR => (CRB_BASE + BUF_OFFSET as u64) as u32,
            REG_CTRL_CMD_HADDR => ((CRB_BASE + BUF_OFFSET as u64) >> 32) as u32,
            REG_CTRL_RSP_SIZE => BUF_LEN as u32,
            REG_CTRL_RSP_ADDR => (CRB_BASE + BUF_OFFSET as u64) as u32,
            REG_CTRL_RSP_HADDR => ((CRB_BASE + BUF_OFFSET as u64) >> 32) as u32,
            _ => 0,
        };
        // Sub-word read: shift the 4-byte word so the requested byte
        // aligns at offset 0, then deliver in the right width.
        let in_word = offset & 0x3;
        let shifted = value >> (in_word * 8);
        match ro.len() {
            1 => ro.write_u8(shifted as u8),
            2 => ro.write_u16(shifted as u16),
            4 => ro.write_u32(shifted),
            8 => ro.write_u64(shifted as u64),
            _ => {}
        }
    }

    fn handle_reg_write(&self, offset: usize, wo: &WriteOp) {
        let val = match wo.len() {
            1 => wo.buf()[0] as u32,
            2 => u16::from_le_bytes([wo.buf()[0], wo.buf()[1]]) as u32,
            4 => u32::from_le_bytes([
                wo.buf()[0],
                wo.buf()[1],
                wo.buf()[2],
                wo.buf()[3],
            ]),
            _ => return, // the spec disallows other sizes
        };

        match offset & !0x3 {
            REG_LOC_CTRL => self.handle_loc_ctrl(val),
            REG_CTRL_REQ => self.handle_ctrl_req(val),
            REG_CTRL_CANCEL => self.handle_ctrl_cancel(val),
            REG_CTRL_START => self.handle_ctrl_start(val),
            // Read-only or reserved: silently ignored.
            _ => {}
        }
    }

    fn handle_loc_ctrl(&self, val: u32) {
        let mut state = self.state();
        if val & LOC_CTRL_REQ_ACCESS != 0 {
            // Grant access at once. Locality 0 is always available on a
            // single-locality CRB.
            state.loc_state |= LOC_STATE_LOC_ASSIGNED;
            state.loc_sts |= LOC_STS_GRANTED;
            callbacks::set_locality(0);
            debug!(self.log, "CRB locality 0 granted");
        }
        if val & LOC_CTRL_RELINQUISH != 0 {
            state.loc_state &= !LOC_STATE_LOC_ASSIGNED;
            state.loc_sts &= !LOC_STS_GRANTED;
            debug!(self.log, "CRB locality 0 relinquished");
        }
        if val & LOC_CTRL_RESET_EST != 0 {
            state.loc_state &= !LOC_STATE_TPM_ESTABLISHED;
        }
    }

    fn handle_ctrl_req(&self, val: u32) {
        let mut state = self.state();
        if val & CTRL_REQ_CMD_READY != 0 {
            state.ctrl_sts &= !CTRL_STS_TPM_IDLE;
        }
        if val & CTRL_REQ_GO_IDLE != 0 {
            state.ctrl_sts |= CTRL_STS_TPM_IDLE;
        }
    }

    /// Record a cancel request. A command in flight is not preempted.
    /// libtpms returns in well under the guest's CRB timeout for every
    /// command except RSA key generation.
    fn handle_ctrl_cancel(&self, val: u32) {
        self.state().ctrl_cancel = val;
    }

    fn handle_ctrl_start(&self, val: u32) {
        if val & CTRL_START_INVOKE == 0 {
            return;
        }

        let cmd_bytes = {
            let mut state = self.state();
            if state.ctrl_start & CTRL_START_INVOKE != 0 {
                // A command is already running, on this vCPU or
                // another. libtpms cannot take a second one. Leave
                // INVOKE set: the guest is already waiting on it.
                return;
            }
            state.ctrl_start |= CTRL_START_INVOKE;

            // TPM command header: tag(2) commandSize(4, big endian).
            // A size past the buffer stays past it, so libtpms sees a
            // short command and answers TPM_RC_COMMAND_SIZE itself.
            let size = u32::from_be_bytes([
                state.buffer[2],
                state.buffer[3],
                state.buffer[4],
                state.buffer[5],
            ]) as usize;
            state.buffer[..size.min(BUF_LEN)].to_vec()
        };

        let response = {
            let _running = self.command.lock().expect("CRB command lock");
            self.run_command(&cmd_bytes)
        };

        let mut state = self.state();
        match response {
            Some(bytes) => state.buffer[..bytes.len()].copy_from_slice(&bytes),
            // A well-formed failure reply, so the guest fails the
            // command instead of timing out. Tag 8001, size 0x0A,
            // TPM_RC_FAILURE.
            None => state.buffer[..10].copy_from_slice(&[
                0x80, 0x01, 0x00, 0x00, 0x00, 0x0A, 0x00, 0x00, 0x01, 0x01,
            ]),
        }
        state.ctrl_start &= !CTRL_START_INVOKE;
    }

    /// Run one command through libtpms and copy out its response.
    ///
    /// The caller must hold `self.command` and must not hold `self.state`:
    /// libtpms calls back into the NV callbacks from this thread.
    fn run_command(&self, cmd: &[u8]) -> Option<Vec<u8>> {
        let mut resp_ptr: *mut u8 = ptr::null_mut();
        let mut resp_size: u32 = 0;
        // A null buffer makes libtpms allocate one of its own size.
        let mut resp_bufsize: u32 = 0;
        // SAFETY: the three out-parameters point at live locals, `cmd`
        // is a live slice of `cmd.len()` bytes, and the command lock
        // makes this the only call in flight.
        let rc: TPM_RESULT = unsafe {
            TPMLIB_Process(
                &mut resp_ptr,
                &mut resp_size,
                &mut resp_bufsize,
                cmd.as_ptr(),
                cmd.len() as u32,
            )
        };

        let response = self.take_response(rc, resp_ptr, resp_size, cmd.len());
        if !resp_ptr.is_null() {
            // SAFETY: libtpms allocated this buffer with libc malloc and
            // gives ownership to the caller. `response` holds a copy.
            unsafe { tpmlib_free(resp_ptr.cast()) };
        }
        response
    }

    fn take_response(
        &self,
        rc: TPM_RESULT,
        resp_ptr: *const u8,
        resp_size: u32,
        cmd_len: usize,
    ) -> Option<Vec<u8>> {
        if rc != TPM_SUCCESS || resp_ptr.is_null() {
            warn!(self.log, "TPMLIB_Process error";
                "rc" => format!("{rc:#x}"), "cmd_size" => cmd_len);
            return None;
        }
        let n = resp_size as usize;
        if n > BUF_LEN {
            // Cannot happen while the negotiated buffer size holds, and
            // a truncated frame would leave the guest reading a length
            // the buffer does not contain.
            warn!(self.log, "libtpms response exceeds the CRB buffer";
                "resp_size" => n, "buffer" => BUF_LEN);
            return None;
        }
        // SAFETY: libtpms reports `resp_size` valid bytes at `resp_ptr`,
        // and the copy ends before this function returns.
        Some(unsafe { std::slice::from_raw_parts(resp_ptr, n) }.to_vec())
    }
}

#[cfg(test)]
mod tests {
    //! CRB driver simulation, driven through the MMIO entry points with
    //! the register sequence the Linux `tpm_crb` driver uses.
    use super::*;
    use slog::Drain;

    /// Build a logger that discards everything.
    fn null_logger() -> Logger {
        let drain = slog::Discard;
        Logger::root(drain.fuse(), slog::o!())
    }

    /// Read a 32-bit register through the public MMIO dispatch.
    fn read_reg(crb: &Crb, off: usize) -> u32 {
        use vmm_core::common::ReadOp;
        let mut ro = ReadOp::new(4);
        crb.handle(off, RWOp::Read(&mut ro));
        u32::from_le_bytes(ro.buf().try_into().unwrap())
    }

    /// Write a 32-bit register through the public MMIO dispatch.
    fn write_reg(crb: &Crb, off: usize, val: u32) {
        use vmm_core::common::WriteOp;
        let bytes = val.to_le_bytes();
        let wo = WriteOp::from_buf(&bytes);
        crb.handle(off, RWOp::Write(&wo));
    }

    /// Write a slice into the command/response buffer at offset
    /// `BUF_OFFSET + buf_off` using 4-byte writes, as the Linux driver
    /// does with `__raw_writel`.
    fn write_buffer(crb: &Crb, bytes: &[u8]) {
        use vmm_core::common::WriteOp;
        let mut i = 0;
        while i + 4 <= bytes.len() {
            let word = u32::from_le_bytes([
                bytes[i],
                bytes[i + 1],
                bytes[i + 2],
                bytes[i + 3],
            ]);
            let wo = WriteOp::from_buf(&word.to_le_bytes());
            crb.handle(BUF_OFFSET + i, RWOp::Write(&wo));
            i += 4;
        }
        // Trailing bytes. TPM commands are usually multiples of 4.
        while i < bytes.len() {
            let wo = WriteOp::from_buf(&[bytes[i]]);
            crb.handle(BUF_OFFSET + i, RWOp::Write(&wo));
            i += 1;
        }
    }

    /// Read N bytes back from the buffer 4 at a time.
    fn read_buffer(crb: &Crb, n: usize) -> Vec<u8> {
        use vmm_core::common::ReadOp;
        let mut out = Vec::with_capacity(n);
        let mut i = 0;
        while i + 4 <= n {
            let mut ro = ReadOp::new(4);
            crb.handle(BUF_OFFSET + i, RWOp::Read(&mut ro));
            out.extend_from_slice(ro.buf());
            i += 4;
        }
        while i < n {
            let mut ro = ReadOp::new(1);
            crb.handle(BUF_OFFSET + i, RWOp::Read(&mut ro));
            out.push(ro.buf()[0]);
            i += 1;
        }
        out
    }

    #[test]
    fn crb_register_layout_matches_tcg_ptp() {
        // Static register reads: locality 0, no command in flight.
        let crb = Crb::new(null_logger());

        // LOC_STATE: tpmRegValidSts (bit 7) is set at power-on so
        // the guest knows the register file is meaningful.
        let loc_state = read_reg(&crb, REG_LOC_STATE);
        assert!(
            loc_state & LOC_STATE_TPM_REG_VALID != 0,
            "LOC_STATE missing tpmRegValidSts: {loc_state:#x}",
        );

        // INTF_ID: type=1 (CRB active), CapCRB=1, InterfaceSelector=1.
        let intf = read_reg(&crb, REG_INTF_ID);
        assert_eq!(intf & 0xF, 1, "InterfaceType must be CRB");

        // CTRL_CMD_LADDR / CTRL_CMD_SIZE: must point at the buffer.
        assert_eq!(
            read_reg(&crb, REG_CTRL_CMD_LADDR),
            (CRB_BASE + BUF_OFFSET as u64) as u32,
        );
        assert_eq!(read_reg(&crb, REG_CTRL_CMD_SIZE), BUF_LEN as u32);
        assert_eq!(read_reg(&crb, REG_CTRL_RSP_SIZE), BUF_LEN as u32);

        // CTRL_STS: tpmIdle is set at power-on. The guest must clear it
        // with CMD_READY before it sends commands.
        assert_ne!(read_reg(&crb, REG_CTRL_STS) & CTRL_STS_TPM_IDLE, 0);
    }

    #[test]
    fn crb_locality_acquire() {
        let crb = Crb::new(null_logger());

        // Initial state: locAssigned == 0, granted == 0.
        assert_eq!(read_reg(&crb, REG_LOC_STATE) & LOC_STATE_LOC_ASSIGNED, 0);
        assert_eq!(read_reg(&crb, REG_LOC_STS) & LOC_STS_GRANTED, 0);

        // Request locality 0.
        write_reg(&crb, REG_LOC_CTRL, LOC_CTRL_REQ_ACCESS);

        // Grant should be immediate: locAssigned and granted both set.
        assert_ne!(read_reg(&crb, REG_LOC_STATE) & LOC_STATE_LOC_ASSIGNED, 0);
        assert_ne!(read_reg(&crb, REG_LOC_STS) & LOC_STS_GRANTED, 0);

        // Relinquish.
        write_reg(&crb, REG_LOC_CTRL, LOC_CTRL_RELINQUISH);
        assert_eq!(read_reg(&crb, REG_LOC_STATE) & LOC_STATE_LOC_ASSIGNED, 0);
        assert_eq!(read_reg(&crb, REG_LOC_STS) & LOC_STS_GRANTED, 0);
    }

    /// TPM2_Startup(TPM_SU_CLEAR), which a TPM needs once after
    /// power-on. Header: tag=8001 size=0000000C cc=00000144.
    const STARTUP: [u8; 12] = [
        0x80, 0x01, 0x00, 0x00, 0x00, 0x0C, 0x00, 0x00, 0x01, 0x44, 0x00, 0x00,
    ];
    /// TPM2_GetRandom(8). Header: tag=8001 size=0000000C cc=0000017B.
    const GET_RANDOM_8: [u8; 12] = [
        0x80, 0x01, 0x00, 0x00, 0x00, 0x0C, 0x00, 0x00, 0x01, 0x7B, 0x00, 0x08,
    ];
    /// `RC_VER1 + 0x042` from the TPM 2 reference code.
    const TPM_RC_COMMAND_SIZE: u32 = 0x142;

    /// Response code from a reply sitting in the CRB buffer.
    fn response_code(crb: &Crb) -> u32 {
        let resp = read_buffer(crb, 10);
        u32::from_be_bytes([resp[6], resp[7], resp[8], resp[9]])
    }

    /// Submit the bytes now in the buffer and wait for the reply.
    ///
    /// The command runs inside the START write, so INVOKE is already
    /// clear when the write returns.
    fn submit(crb: &Crb) {
        write_reg(crb, REG_CTRL_START, CTRL_START_INVOKE);
        assert_eq!(
            read_reg(crb, REG_CTRL_START) & CTRL_START_INVOKE,
            0,
            "START still latched after the write returned",
        );
    }

    /// Every libtpms-backed case shares one test: `Tpm::new` installs a
    /// process-global callback table and libtpms keeps one TPM per
    /// process, so it can run only once per test binary.
    #[test]
    fn crb_command_path_under_a_kernel_style_driver() {
        // A fresh state directory per run, so this always takes the
        // first-boot manufacturing path.
        let dir = std::env::temp_dir()
            .join(format!("vmm-tpm-crbtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tpm = match crate::Tpm::new(dir.clone(), null_logger()) {
            Ok(t) => t,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                panic!("libtpms would not start, so nothing below ran: {e}");
            }
        };

        libtpms_took_the_crb_buffer_size();
        power_on(&tpm.crb);
        get_random_returns_eight_random_bytes(&tpm.crb);
        a_start_while_a_command_runs_is_ignored(&tpm.crb);
        an_oversize_command_header_is_refused(&tpm.crb);
        concurrent_starts_leave_the_tpm_working(&tpm.crb);

        drop(tpm);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `MAX_COMMAND_SIZE` and `MAX_RESPONSE_SIZE` inside libtpms follow
    /// this value. Left at the library default of 4096 it would build
    /// responses the CRB buffer cannot hold.
    fn libtpms_took_the_crb_buffer_size() {
        // SAFETY: a zero request only reports the current value.
        let negotiated = unsafe {
            vmm_tpm_sys::TPMLIB_SetBufferSize(
                0,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        assert_eq!(negotiated as usize, BUF_LEN, "negotiated buffer size");
    }

    /// Acquire locality 0, clear tpmIdle and start the TPM, the way the
    /// Linux `tpm_crb` driver does.
    fn power_on(crb: &Crb) {
        write_reg(crb, REG_LOC_CTRL, LOC_CTRL_REQ_ACCESS);
        assert_ne!(read_reg(crb, REG_LOC_STS) & LOC_STS_GRANTED, 0);
        write_reg(crb, REG_CTRL_REQ, CTRL_REQ_CMD_READY);

        write_buffer(crb, &STARTUP);
        submit(crb);
        let rc = response_code(crb);
        assert_eq!(rc, 0, "TPM2_Startup rc: {rc:#x}");
    }

    fn get_random_returns_eight_random_bytes(crb: &Crb) {
        write_buffer(crb, &GET_RANDOM_8);
        submit(crb);

        // tag(2) size(4) rc(4) randomSize(2) random(8) = 20 bytes.
        let resp = read_buffer(crb, 20);
        assert_eq!(&resp[0..2], &[0x80, 0x01], "tag");
        let size = u32::from_be_bytes([resp[2], resp[3], resp[4], resp[5]]);
        assert_eq!(size, 20, "frame size");
        let rc = u32::from_be_bytes([resp[6], resp[7], resp[8], resp[9]]);
        assert_eq!(rc, 0, "GetRandom rc: {rc:#x}");
        assert_eq!(u16::from_be_bytes([resp[10], resp[11]]), 8, "randomSize");
        let random = &resp[12..20];
        assert!(random.iter().any(|&b| b != 0), "all zero: {random:?}");
    }

    /// libtpms has no locking, so a second command must not start while
    /// one is in flight. A latched INVOKE is what marks that.
    fn a_start_while_a_command_runs_is_ignored(crb: &Crb) {
        write_buffer(crb, &GET_RANDOM_8);
        // Stand in for another vCPU inside TPMLIB_Process.
        crb.state().ctrl_start |= CTRL_START_INVOKE;

        write_reg(crb, REG_CTRL_START, CTRL_START_INVOKE);

        assert_eq!(
            read_buffer(crb, GET_RANDOM_8.len()),
            GET_RANDOM_8,
            "the command was replaced by a response, so it ran twice",
        );
        assert_ne!(
            read_reg(crb, REG_CTRL_START) & CTRL_START_INVOKE,
            0,
            "INVOKE cleared under the command that still holds it",
        );

        crb.state().ctrl_start &= !CTRL_START_INVOKE;
        submit(crb);
        let rc = response_code(crb);
        assert_eq!(rc, 0, "the same command after the latch cleared: {rc:#x}");
    }

    /// A commandSize header past the buffer reaches libtpms as a short
    /// command, which the library itself answers.
    fn an_oversize_command_header_is_refused(crb: &Crb) {
        let mut cmd = GET_RANDOM_8;
        cmd[2..6].copy_from_slice(&0x4000u32.to_be_bytes());
        write_buffer(crb, &cmd);
        submit(crb);

        let rc = response_code(crb);
        assert_eq!(rc, TPM_RC_COMMAND_SIZE, "oversize command rc: {rc:#x}");
    }

    /// Four threads writing START do what two vCPUs do. Without the
    /// command lock they race libtpms' global TPM state. A TPM left in
    /// failure mode answers every later command with a non-zero code.
    fn concurrent_starts_leave_the_tpm_working(crb: &Crb) {
        write_buffer(crb, &GET_RANDOM_8);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..25 {
                        write_reg(crb, REG_CTRL_START, CTRL_START_INVOKE);
                    }
                });
            }
        });

        assert_eq!(
            read_reg(crb, REG_CTRL_START) & CTRL_START_INVOKE,
            0,
            "INVOKE left latched, so the guest would wait forever",
        );
        write_buffer(crb, &GET_RANDOM_8);
        submit(crb);
        let rc = response_code(crb);
        assert_eq!(rc, 0, "TPM stopped answering after the races: {rc:#x}");
    }
}
