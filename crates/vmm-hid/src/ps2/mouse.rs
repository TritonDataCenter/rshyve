// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Portions derived from Propolis, Copyright Oxide Computer Company,
// used under MPL-2.0. Upstream keeps `PS2Mouse` in
// lib/propolis/src/hw/ps2/ctrl.rs.

use std::collections::VecDeque;

use bitflags::bitflags;

use super::ctrl::{
    probe_mouse_cmd, probe_mouse_data, probe_mouse_overflow,
    probe_unknown_mouse_cmd,
};
use super::kbd::PS2_KBD_BUFSZ;

const PS2M_CMD_RESET: u8 = 0xff;
const PS2M_CMD_RESEND: u8 = 0xfe;
const PS2M_CMD_SET_DEFAULTS: u8 = 0xf6;
const PS2M_CMD_DATA_REP_DIS: u8 = 0xf5;
const PS2M_CMD_DATA_REP_ENA: u8 = 0xf4;
const PS2M_CMD_SET_SAMP_RATE: u8 = 0xf3;
const PS2M_CMD_GET_DEVID: u8 = 0xf2;
const PS2M_CMD_REMOTE_MODE_SET: u8 = 0xf0;
const PS2M_CMD_WRAP_MODE_SET: u8 = 0xee;
const PS2M_CMD_WRAP_MODE_RESET: u8 = 0xec;
const PS2M_CMD_READ_DATA: u8 = 0xeb;
const PS2M_CMD_STREAM_MODE_SET: u8 = 0xea;
const PS2M_CMD_STATUS_REQ: u8 = 0xe9;
const PS2M_CMD_RESOLUTION_SET: u8 = 0xe8;
const PS2M_CMD_SCALING1_SET: u8 = 0xe7;
const PS2M_CMD_SCALING2_SET: u8 = 0xe6;

const PS2M_R_ACK: u8 = 0xfa;
const PS2M_R_SELF_TEST_PASS: u8 = 0xaa;
// basic mouse device ID
const PS2M_R_DEVID: u8 = 0x00;

bitflags! {
    #[derive(Default)]
    pub struct PS2MStatus: u8 {
        const B_LEFT = 1 << 0;
        const B_RIGHT = 1 << 1;
        const B_MID = 1 << 2;

        const SCALE2 = 1 << 4;
        const ENABLE = 1 << 5;
        const REMOTE = 1 << 6;
    }
}

pub(super) struct PS2Mouse {
    buf: VecDeque<u8>,
    cur_cmd: Option<u8>,
    status: PS2MStatus,
    resolution: u8,
    sample_rate: u8,
}
impl PS2Mouse {
    pub(super) fn new() -> Self {
        Self {
            buf: VecDeque::with_capacity(PS2_KBD_BUFSZ),
            cur_cmd: None,
            status: PS2MStatus::empty(),
            resolution: 0,
            sample_rate: 10,
        }
    }
    pub(super) fn cmd_input(&mut self, v: u8) {
        probe_mouse_cmd(v);
        if let Some(cmd) = self.cur_cmd {
            self.cur_cmd = None;
            match cmd {
                PS2M_CMD_SET_SAMP_RATE => {
                    // XXX: check for valid values?
                    self.sample_rate = v;
                }
                PS2M_CMD_RESOLUTION_SET => {
                    // XXX: check for valid values?
                    self.resolution = v;
                }
                _ => {
                    debug_assert!(false, "bad multi-part ps2 cmd {}", cmd);
                }
            }
        } else {
            match v {
                PS2M_CMD_RESET => {
                    self.reset();
                    self.resp(PS2M_R_ACK);
                    self.resp(PS2M_R_SELF_TEST_PASS);
                    self.resp(PS2M_R_DEVID);
                }
                PS2M_CMD_RESEND => {
                    // XXX: last-byte tracking is not implemented.
                    self.resp(PS2M_R_ACK);
                }
                PS2M_CMD_SET_DEFAULTS => {
                    // XXX: set which defaults?
                    self.resp(PS2M_R_ACK);
                }
                PS2M_CMD_DATA_REP_DIS => {
                    self.resp(PS2M_R_ACK);
                    self.status.remove(PS2MStatus::ENABLE);
                }
                PS2M_CMD_DATA_REP_ENA => {
                    self.resp(PS2M_R_ACK);
                    self.status.insert(PS2MStatus::ENABLE);
                }
                PS2M_CMD_GET_DEVID => {
                    self.resp(PS2M_R_ACK);
                    // standard ps2 mouse dev id
                    self.resp(PS2M_R_DEVID);
                }
                PS2M_CMD_REMOTE_MODE_SET => {
                    self.resp(PS2M_R_ACK);
                    self.status.insert(PS2MStatus::REMOTE);
                }
                PS2M_CMD_WRAP_MODE_SET | PS2M_CMD_WRAP_MODE_RESET => {
                    self.resp(PS2M_R_ACK);
                }

                PS2M_CMD_READ_DATA => {
                    self.resp(PS2M_R_ACK);
                    self.movement();
                }
                PS2M_CMD_STREAM_MODE_SET => {
                    // XXX wire to what?
                }
                PS2M_CMD_STATUS_REQ => {
                    // status, resolution, sample rate
                    self.resp(PS2M_R_ACK);
                    self.resp(self.status.bits());
                    self.resp(self.resolution);
                    self.resp(self.sample_rate);
                }

                PS2M_CMD_SET_SAMP_RATE | PS2M_CMD_RESOLUTION_SET => {
                    self.cur_cmd = Some(v);
                    self.resp(PS2M_R_ACK);
                }
                PS2M_CMD_SCALING1_SET | PS2M_CMD_SCALING2_SET => {
                    self.status
                        .set(PS2MStatus::SCALE2, v == PS2M_CMD_SCALING2_SET);
                    self.resp(PS2M_R_ACK);
                }

                _ => {
                    // ignore unrecognized cmds
                    probe_unknown_mouse_cmd(v);
                }
            }
        }
    }
    pub(super) fn resp(&mut self, v: u8) {
        let remain = PS2_KBD_BUFSZ - self.buf.len();
        match remain {
            0 => {
                probe_mouse_overflow(v);
                // overrun already in progress, do nothing
            }
            1 => {
                // indicate overflow instead
                probe_mouse_overflow(v);
                self.buf.push_back(0xff)
            }
            _ => {
                probe_mouse_data(v);
                self.buf.push_back(v);
            }
        }
    }
    pub(super) fn reset(&mut self) {
        // XXX  what should the defaults be?
        self.buf.clear();
        self.cur_cmd = None;
        self.status = PS2MStatus::empty();
        self.resolution = 0;
        self.sample_rate = 10;
    }
    pub(super) fn has_output(&self) -> bool {
        !self.buf.is_empty()
    }
    pub(super) fn read_output(&mut self) -> Option<u8> {
        self.buf.pop_front()
    }
    pub(super) fn loopback(&mut self, v: u8) {
        self.resp(v);
    }
    pub(super) fn movement(&mut self) {
        // no buttons, just the always-one bit
        self.resp(0b00001000);
        // no X movement
        self.resp(0x00);
        // no Y movement
        self.resp(0x00);
    }
}
impl Default for PS2Mouse {
    fn default() -> Self {
        Self::new()
    }
}
