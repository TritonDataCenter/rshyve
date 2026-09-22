// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Portions derived from Propolis, Copyright Oxide Computer Company,
// used under MPL-2.0. Upstream keeps `PS2Kbd` in
// lib/propolis/src/hw/ps2/ctrl.rs.

use std::collections::VecDeque;

use super::ctrl::{
    probe_keyboard_cmd, probe_keyboard_data, probe_keyboard_overflow,
    probe_unknown_keyboard_cmd, PS2ScanCodeSet,
};

const PS2K_CMD_SET_LEDS: u8 = 0xed;

const PS2K_CMD_SCAN_CODE: u8 = 0xf0;
const PS2K_CMD_TYPEMATIC: u8 = 0xf3;

const PS2K_CMD_ECHO: u8 = 0xee;
const PS2K_CMD_IDENT: u8 = 0xf2;
const PS2K_CMD_SCAN_EN: u8 = 0xf4;
const PS2K_CMD_SCAN_DIS: u8 = 0xf5;
const PS2K_CMD_SET_DEFAULT: u8 = 0xf6;
const PS2K_CMD_RESEND: u8 = 0xfe;
const PS2K_CMD_RESET: u8 = 0xff;

const PS2K_R_ACK: u8 = 0xfa;
const PS2K_R_ECHO: u8 = 0xee;
const PS2K_R_SELF_TEST_PASS: u8 = 0xaa;

const PS2K_TYPEMATIC_MASK: u8 = 0x7f;

pub(super) const PS2_KBD_BUFSZ: usize = 16;

// TODO: wire up remote console to enabled/led_status/typematic
#[allow(unused)]
pub(super) struct PS2Kbd {
    buf: VecDeque<u8>,
    cur_cmd: Option<u8>,
    enabled: bool,
    led_status: u8,
    typematic: u8,
    scan_code_set: PS2ScanCodeSet,
}
impl PS2Kbd {
    pub(super) fn new() -> Self {
        Self {
            buf: VecDeque::with_capacity(PS2_KBD_BUFSZ),
            cur_cmd: None,
            enabled: true,
            led_status: 0,
            typematic: 0,
            scan_code_set: PS2ScanCodeSet::Set1,
        }
    }
    pub(super) fn cmd_input(&mut self, v: u8) {
        probe_keyboard_cmd(v);
        if let Some(cmd) = self.cur_cmd {
            self.cur_cmd = None;
            match cmd {
                PS2K_CMD_SET_LEDS => {
                    // low three bits set scroll/num/caps lock
                    self.led_status = v & 0b111;
                    self.resp(PS2K_R_ACK);
                }
                PS2K_CMD_SCAN_CODE => {
                    match v {
                        0 => {
                            // get scan code set
                            self.resp(PS2K_R_ACK);
                            self.resp(self.scan_code_set.as_byte());
                        }
                        1 => {
                            self.resp(PS2K_R_ACK);
                            self.scan_code_set = PS2ScanCodeSet::Set1;
                        }
                        2 => {
                            self.resp(PS2K_R_ACK);
                            self.scan_code_set = PS2ScanCodeSet::Set2;
                        }
                        _ => {}
                    }
                }
                PS2K_CMD_TYPEMATIC => {
                    self.typematic = v & PS2K_TYPEMATIC_MASK;
                    self.resp(PS2K_R_ACK);
                }
                _ => {
                    debug_assert!(false, "bad multi-part ps2 cmd {}", cmd);
                }
            }
        } else {
            match v {
                PS2K_CMD_SET_LEDS | PS2K_CMD_SCAN_CODE | PS2K_CMD_TYPEMATIC => {
                    // multi-part command, wait for next byte
                    self.cur_cmd = Some(v);
                    self.resp(PS2K_R_ACK);
                }
                PS2K_CMD_ECHO => {
                    self.resp(PS2K_R_ECHO);
                }
                PS2K_CMD_IDENT => {
                    self.resp(PS2K_R_ACK);
                    // MF2 keyboard
                    self.resp(0xab);
                    self.resp(0x83);
                }
                PS2K_CMD_SCAN_EN => {
                    self.enabled = true;
                    self.resp(PS2K_R_ACK);
                }
                PS2K_CMD_SCAN_DIS => {
                    self.enabled = false;
                    self.resp(PS2K_R_ACK);
                }
                PS2K_CMD_SET_DEFAULT => {
                    // XXX which things to reset?
                    self.resp(PS2K_R_ACK);
                }
                PS2K_CMD_RESEND => {
                    // XXX: last-byte tracking is not implemented.
                    self.resp(PS2K_R_ACK);
                }
                PS2K_CMD_RESET => {
                    self.reset();
                    // Even for reset, ack is expected
                    self.resp(PS2K_R_ACK);
                    self.resp(PS2K_R_SELF_TEST_PASS);
                }
                _ => {
                    // ignore unrecognized cmds
                    probe_unknown_keyboard_cmd(v);
                }
            }
        }
    }
    pub(super) fn resp(&mut self, v: u8) {
        let remain = PS2_KBD_BUFSZ - self.buf.len();
        match remain {
            0 => {
                // overrun already in progress, do nothing
                probe_keyboard_overflow(v);
            }
            1 => {
                // indicate overflow instead
                probe_keyboard_overflow(v);
                self.buf.push_back(0xff)
            }
            _ => {
                probe_keyboard_data(v);
                self.buf.push_back(v);
            }
        }
    }
    pub(super) fn reset(&mut self) {
        // XXX  what should the defaults be?
        self.cur_cmd = None;
        self.enabled = true;
        self.led_status = 0;
        self.typematic = 0;
        self.scan_code_set = PS2ScanCodeSet::Set1;
        self.buf.clear();
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

    pub(super) fn recv_scancode(&mut self, scan_code: Vec<u8>) {
        for s in scan_code.into_iter() {
            self.resp(s);
        }
    }
}
impl Default for PS2Kbd {
    fn default() -> Self {
        Self::new()
    }
}
