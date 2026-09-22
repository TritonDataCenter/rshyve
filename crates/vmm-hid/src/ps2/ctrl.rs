// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Portions derived from Propolis, Copyright Oxide Computer Company,
// used under MPL-2.0. Upstream: lib/propolis/src/hw/ps2/ctrl.rs
// https://github.com/oxidecomputer/propolis

use std::convert::TryFrom;
use std::sync::{Arc, Mutex};

use bitflags::bitflags;
use vmm_core::common::RWOp;
use vmm_core::hdl::SuspendHow;
use vmm_core::intr_pins::IntrPin;
use vmm_core::pio::{PioBus, PioFn};

use vmm_devices::acpi_pm::SuspendSink;
use vmm_devices::lifecycle::Lifecycle;

use super::kbd::PS2Kbd;
use super::keyboard::{KeyEvent, KeyEventRep};
use super::mouse::PS2Mouse;
use super::{PORT_PS2_CMD_STATUS, PORT_PS2_DATA};

/// PS/2 controller (Intel 8042) with an attached keyboard and mouse.
///
/// I/O PORTS
///
/// - Control port (0x64). Reads return the status register ([CtrlStatus]).
///   Writes are controller commands (`PS2C_CMD_*`). Some commands take one
///   more byte on the data port.
/// - Data port (0x60). Reads return a command response or device data.
///   Writes are the argument byte of a pending controller command, or a
///   keyboard command. To send a byte to the mouse, the guest first writes
///   `PS2C_CMD_WRITE_AUX_IN` to the control port.
///
/// REGISTERS
///
/// The CPU sees three 8-bit registers: the input buffer (written through
/// either port), the output buffer (read through the data port) and the
/// status register. The input buffer has no state here because writes are
/// handled at once. See [CtrlOutPort] for the output port.
///
/// The Controller Configuration Byte is byte 0 of the controller RAM. See
/// [CtrlCfg].
///
/// INTERRUPTS
///
/// The interrupts are edge-triggered. The controller pulses the pin when it
/// has output for the guest.
///
/// KEYBOARD INPUT
///
/// Key events come from the VNC server as X11 keysyms, not through the input
/// buffer. The controller converts each keysym to scan codes, puts them in
/// the keyboard output buffer and pulses the keyboard interrupt.

#[usdt::provider(provider = "vmm")]
mod probes {
    // Controller Configuration updates from OS
    fn ps2ctrl_ctrlcfg_update(ctrl_cfg: u8) {}

    // reads/writes on control/data ports
    fn ps2ctrl_data_read(val: u8) {}
    fn ps2ctrl_data_read_empty() {}
    fn ps2ctrl_data_write(v: u8) {}
    fn ps2ctrl_cmd_write(v: u8) {}
    fn ps2ctrl_unknown_cmd(v: u8) {}

    // reads of Controller Status register
    fn ps2ctrl_status_read(status: u8) {}

    // interrupts: fire when the controller issues pri/aux interrupts
    fn ps2ctrl_pulse_pri() {}
    fn ps2ctrl_pulse_aux() {}

    // keyboard event probes
    fn ps2ctrl_keyevent(
        keysym_raw: u32,
        scan_code_set: u8,
        s0: u8,
        s1: u8,
        s2: u8,
        s3: u8,
    ) {
    }
    fn ps2ctrl_keyevent_dropped(
        keysym_raw: u32,
        is_pressed: u8,
        scan_code_set: u8,
    ) {
    }

    // internal device buffer writes
    fn ps2ctrl_keyboard_data(v: u8) {}
    fn ps2ctrl_mouse_data(v: u8) {}
    fn ps2ctrl_keyboard_overflow(v: u8) {}
    fn ps2ctrl_mouse_overflow(v: u8) {}

    // internal device buffer reads
    fn ps2ctrl_keyboard_data_read(v: u8) {}
    fn ps2ctrl_mouse_data_read(v: u8) {}

    // device commands
    fn ps2ctrl_keyboard_cmd(v: u8) {}
    fn ps2ctrl_mouse_cmd(v: u8) {}
    fn ps2ctrl_unknown_keyboard_cmd(v: u8) {}
    fn ps2ctrl_unknown_mouse_cmd(v: u8) {}
}

#[inline]
pub(super) fn probe_keyboard_cmd(v: u8) {
    probes::ps2ctrl_keyboard_cmd!(|| v);
}

#[inline]
pub(super) fn probe_unknown_keyboard_cmd(v: u8) {
    probes::ps2ctrl_unknown_keyboard_cmd!(|| v);
}

#[inline]
pub(super) fn probe_keyboard_overflow(v: u8) {
    probes::ps2ctrl_keyboard_overflow!(|| v);
}

#[inline]
pub(super) fn probe_keyboard_data(v: u8) {
    probes::ps2ctrl_keyboard_data!(|| v);
}

#[inline]
pub(super) fn probe_mouse_cmd(v: u8) {
    probes::ps2ctrl_mouse_cmd!(|| v);
}

#[inline]
pub(super) fn probe_unknown_mouse_cmd(v: u8) {
    probes::ps2ctrl_unknown_mouse_cmd!(|| v);
}

#[inline]
pub(super) fn probe_mouse_overflow(v: u8) {
    probes::ps2ctrl_mouse_overflow!(|| v);
}

#[inline]
pub(super) fn probe_mouse_data(v: u8) {
    probes::ps2ctrl_mouse_data!(|| v);
}

bitflags! {
    /// Controller Status Register
    ///
    /// An 8-bit register indicating the status of the controller, accessed by
    /// reading from the Control Port.
    #[derive(Default)]
    pub struct CtrlStatus: u8 {
        /// Output Buffer Status
        /// This bit must be set to 1 (indicating the buffer is full) before the
        /// OS attempts to read data from the data port.
        const OUT_FULL = 1 << 0;

        /// Input Buffer Status
        /// 0 if the input buffer is empty; 1 if the input buffer is full and
        /// shouldn't be written to by the OS.
        const IN_FULL = 1 << 1;

        /// System Flag
        /// This bit should be cleared to 0 by the controller on reset, and set
        /// to 1 if the system passes self tests.
        const SYS_FLAG = 1 << 2;

        /// Command/Data
        /// 1 if the last write to the input buffer (data port) was a command; 0
        ///   if the last write to the input buffer was data.
        const CMD_DATA = 1 << 3;

        /// Keyboard not locked (1 = unlocked)
        const UNLOCKED = 1 << 4;

        /// Auxiliary Output Buffer contains data
        const AUX_FULL = 1 << 5;

        /// Timeout Error (0 = no error, 1 = timeout error)
        const TMO = 1 << 6;

        /// Parity Error with last byte (0 = no error, 1 = parity error)
        const PARITY = 1 << 7;
    }
}

bitflags! {
    /// Controller Configuration Byte
    /// The OS can read and write this byte to configure the controller.
    #[derive(Default)]
    pub struct CtrlCfg: u8 {
        /// Primary Port Interrupt (1 = enabled, 0 = disabled)
        const PRI_INTR_EN = 1 << 0;

        /// Auxiliary Port Interrupt (1 = enabled, 0 = disabled)
        const AUX_INTR_EN = 1 << 1;

        /// System Flag (1 = system tests passed)
        const SYS_FLAG = 1 << 2;

        // bit 3: must be 0

        /// Primary Port Clock (1 = disabled, 0 = enabled)
        const PRI_CLOCK_DIS = 1 << 4;

        /// Auxiliary Port Clock (1 = disabled, 0 = enabled)
        const AUX_CLOCK_DIS = 1 << 5;

        /// Primary Port Translation (1 = enabled, 0 = disabled)
        /// If enabled, the controller should translate keyboard data to scan
        /// code set 1.
        const PRI_XLATE_EN = 1 << 6;

        // bit 7: must be 0
    }
}

bitflags! {
    /// Controller Output Port
    #[derive(Default, Copy, Clone)]
    pub struct CtrlOutPort: u8 {
        // Bit 0 is system reset (active low). It is not modeled here. The
        // pulse commands (`PS2C_CMD_PULSE_*`) request the reset.

        /// A20 Gate
        const A20 = 1 << 1;

        /// Auxiliary Port Clock
        const AUX_CLOCK = 1 << 2;

        /// Auxiliary Port Data
        const AUX_DATA = 1 << 3;

        /// Primary Port Output Buffer Full
        const PRI_FULL = 1 << 4;

        /// Auxiliary Port Output Buffer Full
        const AUX_FULL = 1 << 5;

        /// Primary Port Clock
        const PRI_CLOCK = 1 << 6;

        /// Primary Port Data
        const PRI_DATA = 1 << 7;

        // PRI_FULL and AUX_FULL are dynamic
        const DYN_FLAGS = (1 << 4) | (1 << 5);
    }
}

// Controller Commands

// Read/write Controller Configuration Byte (byte 0 of controller internal RAM)
const PS2C_CMD_READ_CTRL_CFG: u8 = 0x20;
const PS2C_CMD_WRITE_CTRL_CFG: u8 = 0x60;

// Read byte N from controller internal RAM, where N is in the range: 0x21-0x3f
const PS2C_CMD_READ_RAM_START: u8 = 0x21;
const PS2C_CMD_READ_RAM_END: u8 = 0x3f;

// Write byte N to controller internal RAM, where N is in the range: 0x21-0x3f
const PS2C_CMD_WRITE_RAM_START: u8 = 0x61;
const PS2C_CMD_WRITE_RAM_END: u8 = 0x7f;

// Disable/enable auxiliary port
const PS2C_CMD_AUX_PORT_DIS: u8 = 0xa7;
const PS2C_CMD_AUX_PORT_ENA: u8 = 0xa8;

// Test auxiliary port
const PS2C_CMD_AUX_PORT_TEST: u8 = 0xa9;

// Test controller (and response, if test passes)
const PS2C_CMD_CTRL_TEST: u8 = 0xaa;
const PS2C_R_CTRL_TEST_PASS: u8 = 0x55;

// Test primary port (and response, if test passes)
const PS2C_CMD_PRI_PORT_TEST: u8 = 0xab;
const PS2C_R_PORT_TEST_PASS: u8 = 0x00;

// Disable/enable primary port
const PS2C_CMD_PRI_PORT_DIS: u8 = 0xad;
const PS2C_CMD_PRI_PORT_ENA: u8 = 0xae;

// Read/write next byte to the Controller Output Port
const PS2C_CMD_READ_CTLR_OUT: u8 = 0xd0;
const PS2C_CMD_WRITE_CTLR_OUT: u8 = 0xd1;

// Write next byte to the primary port output
const PS2C_CMD_WRITE_PRI_OUT: u8 = 0xd2;

// Write next byte to the auxiliary port output
const PS2C_CMD_WRITE_AUX_OUT: u8 = 0xd3;

// Write next byte to the auxiliary port input
const PS2C_CMD_WRITE_AUX_IN: u8 = 0xd4;

// Pulse output line low
const PS2C_CMD_PULSE_START: u8 = 0xf0;
const PS2C_CMD_PULSE_END: u8 = 0xff;

const PS2C_RAM_LEN: usize =
    (PS2C_CMD_WRITE_RAM_END - PS2C_CMD_WRITE_RAM_START) as usize + 1;
const _: () = assert!(
    PS2C_RAM_LEN
        == (PS2C_CMD_READ_RAM_END - PS2C_CMD_READ_RAM_START) as usize + 1
);

#[derive(Default)]
struct PS2State {
    resp: Option<u8>,
    cmd_prefix: Option<u8>,
    ctrl_cfg: CtrlCfg,
    ctrl_out_port: CtrlOutPort,
    ram: [u8; PS2C_RAM_LEN],

    pri_port: PS2Kbd,
    aux_port: PS2Mouse,

    pri_pin: Option<Arc<dyn IntrPin>>,
    aux_pin: Option<Arc<dyn IntrPin>>,
    reset_sink: Option<Arc<dyn SuspendSink>>,
}

pub struct PS2Ctrl {
    state: Mutex<PS2State>,
}

impl PS2Ctrl {
    pub fn create() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PS2State::default()),
        })
    }
    pub fn attach(
        self: &Arc<Self>,
        bus: &PioBus,
        pri_pin: Arc<dyn IntrPin>,
        aux_pin: Arc<dyn IntrPin>,
        reset_sink: Option<Arc<dyn SuspendSink>>,
    ) {
        let data_ctrl = Arc::clone(self);
        let data_handler: Arc<PioFn> =
            Arc::new(move |_offset: u16, rwo: RWOp<'_>| {
                data_ctrl.pio_rw(PORT_PS2_DATA, rwo);
            });
        bus.register(PORT_PS2_DATA, 1, data_handler)
            .expect("PS2Ctrl: data port conflict during attach");

        let status_ctrl = Arc::clone(self);
        let status_handler: Arc<PioFn> =
            Arc::new(move |_offset: u16, rwo: RWOp<'_>| {
                status_ctrl.pio_rw(PORT_PS2_CMD_STATUS, rwo);
            });
        bus.register(PORT_PS2_CMD_STATUS, 1, status_handler)
            .expect("PS2Ctrl: command/status port conflict during attach");

        let mut state = self.state.lock().unwrap();
        state.pri_pin = Some(pri_pin);
        state.aux_pin = Some(aux_pin);
        state.reset_sink = reset_sink;
    }

    pub fn key_event(&self, ke: KeyEvent) {
        let mut state = self.state.lock().unwrap();
        let translate = state.ctrl_cfg.contains(CtrlCfg::PRI_XLATE_EN);
        let key_rep = match KeyEventRep::try_from(ke) {
            Ok(kr) => kr,
            Err(_) => {
                // ignore any unrecognized keys
                probes::ps2ctrl_keyevent_dropped!(|| {
                    let set = if translate { 1 } else { 2 };
                    let is_pressed = if ke.is_pressed { 1 } else { 0 };
                    (ke.keysym_raw, is_pressed, set)
                });
                return;
            }
        };

        // With translation enabled the guest expects scan code set 1.
        // Otherwise it expects set 2.
        let scan_code = if translate {
            key_rep.to_scan_code(PS2ScanCodeSet::Set1)
        } else {
            key_rep.to_scan_code(PS2ScanCodeSet::Set2)
        };

        // Record the keysym and the scan code it produced, to debug key
        // mapping.
        probes::ps2ctrl_keyevent!(|| {
            let set = if translate { 1 } else { 2 };
            let sc_len = scan_code.len();

            let (mut s0, mut s1, mut s2, mut s3) = (0, 0, 0, 0);

            if sc_len > 0 {
                s0 = scan_code[0];
            }

            if sc_len > 1 {
                s1 = scan_code[1];
            }

            if sc_len > 2 {
                s2 = scan_code[2];
            }

            if sc_len > 3 {
                s3 = scan_code[3];
            }

            (key_rep.keysym_raw, set, s0, s1, s2, s3)
        });

        state.pri_port.recv_scancode(scan_code);
        self.update_intr(&mut state);
    }

    fn pio_rw(&self, port: u16, rwo: RWOp<'_>) {
        match port {
            PORT_PS2_DATA => match rwo {
                RWOp::Read(ro) => ro.write_u8(self.data_read()),
                RWOp::Write(wo) => self.data_write(wo.read_u8()),
            },

            PORT_PS2_CMD_STATUS => match rwo {
                RWOp::Read(ro) => ro.write_u8(self.status_read()),
                RWOp::Write(wo) => self.cmd_write(wo.read_u8()),
            },
            _ => {
                debug_assert!(false, "unexpected pio in {:x}", port);
            }
        }
    }

    fn data_write(&self, v: u8) {
        let mut state = self.state.lock().unwrap();
        let cmd_prefix = state.cmd_prefix.take();

        probes::ps2ctrl_data_write!(|| v);

        // A pending controller command takes this byte as its argument.
        // Otherwise the byte is a keyboard command.
        if let Some(prefix) = cmd_prefix {
            match prefix {
                PS2C_CMD_WRITE_CTRL_CFG => {
                    let cfg = CtrlCfg::from_bits_truncate(v);
                    probes::ps2ctrl_ctrlcfg_update!(|| cfg.bits());
                    state.ctrl_cfg = cfg;
                }
                PS2C_CMD_WRITE_RAM_START..=PS2C_CMD_WRITE_RAM_END => {
                    let off = (prefix - PS2C_CMD_WRITE_RAM_START) as usize;
                    if let Some(byte) = state.ram.get_mut(off) {
                        *byte = v;
                    }
                }
                PS2C_CMD_WRITE_CTLR_OUT => {
                    state.ctrl_out_port = CtrlOutPort::from_bits_truncate(v);
                    state.ctrl_out_port.remove(CtrlOutPort::DYN_FLAGS);
                }
                PS2C_CMD_WRITE_PRI_OUT => {
                    state.pri_port.loopback(v);
                }
                PS2C_CMD_WRITE_AUX_OUT => {
                    state.aux_port.loopback(v);
                }
                PS2C_CMD_WRITE_AUX_IN => {
                    state.aux_port.cmd_input(v);
                }
                _ => {
                    debug_assert!(false, "unexpected chain cmd {:x}", prefix);
                }
            }
        } else {
            state.pri_port.cmd_input(v);
        }
        self.update_intr(&mut state);
    }
    fn data_read(&self) -> u8 {
        let mut state = self.state.lock().unwrap();
        if let Some(rval) = state.resp {
            state.resp = None;
            probes::ps2ctrl_data_read!(|| rval);
            rval
        } else if state.pri_port.has_output() {
            let rval = state.pri_port.read_output().unwrap();
            probes::ps2ctrl_keyboard_data_read!(|| rval);
            self.update_intr(&mut state);
            rval
        } else if state.aux_port.has_output() {
            let rval = state.aux_port.read_output().unwrap();
            probes::ps2ctrl_mouse_data_read!(|| rval);
            self.update_intr(&mut state);
            rval
        } else {
            probes::ps2ctrl_data_read_empty!(|| {});
            0
        }
    }
    fn cmd_write(&self, v: u8) {
        let mut state = self.state.lock().unwrap();
        probes::ps2ctrl_cmd_write!(|| v);
        match v {
            PS2C_CMD_READ_CTRL_CFG => {
                state.resp = Some(state.ctrl_cfg.bits());
            }
            PS2C_CMD_READ_RAM_START..=PS2C_CMD_READ_RAM_END => {
                let off = (v - PS2C_CMD_READ_RAM_START) as usize;
                state.resp = Some(state.ram.get(off).copied().unwrap_or(0))
            }
            PS2C_CMD_CTRL_TEST => {
                state.resp = Some(PS2C_R_CTRL_TEST_PASS);
            }

            PS2C_CMD_PRI_PORT_TEST => {
                state.resp = Some(PS2C_R_PORT_TEST_PASS);
            }
            PS2C_CMD_AUX_PORT_TEST => {
                state.resp = Some(PS2C_R_PORT_TEST_PASS);
            }
            PS2C_CMD_PRI_PORT_ENA | PS2C_CMD_PRI_PORT_DIS => {
                state
                    .ctrl_cfg
                    .set(CtrlCfg::PRI_CLOCK_DIS, v == PS2C_CMD_PRI_PORT_DIS);
            }
            PS2C_CMD_AUX_PORT_ENA | PS2C_CMD_AUX_PORT_DIS => {
                state
                    .ctrl_cfg
                    .set(CtrlCfg::AUX_CLOCK_DIS, v == PS2C_CMD_AUX_PORT_DIS);
            }

            PS2C_CMD_READ_CTLR_OUT => {
                let mut val = state.ctrl_out_port;
                val.set(CtrlOutPort::PRI_FULL, state.pri_port.has_output());
                val.set(CtrlOutPort::AUX_FULL, state.aux_port.has_output());
                state.resp = Some(val.bits());
            }

            // commands with a following byte to complete
            PS2C_CMD_WRITE_CTRL_CFG
            | PS2C_CMD_WRITE_CTLR_OUT
            | PS2C_CMD_WRITE_PRI_OUT
            | PS2C_CMD_WRITE_AUX_OUT
            | PS2C_CMD_WRITE_AUX_IN
            | PS2C_CMD_WRITE_RAM_START..=PS2C_CMD_WRITE_RAM_END => {
                state.cmd_prefix = Some(v)
            }

            PS2C_CMD_PULSE_START..=PS2C_CMD_PULSE_END => {
                let to_pulse = v - PS2C_CMD_PULSE_START;
                if to_pulse == 0xe {
                    let reset_sink = state.reset_sink.clone();
                    drop(state);
                    if let Some(sink) = reset_sink {
                        sink.suspend(SuspendHow::Reset);
                    }
                }
            }

            _ => {
                // ignore all other unrecognized commands
                probes::ps2ctrl_unknown_cmd!(|| v);
            }
        }
    }
    fn status_read(&self) -> u8 {
        let state = self.state.lock().unwrap();
        // Always report unlocked
        let mut val = CtrlStatus::UNLOCKED;

        if state.resp.is_some()
            || state.pri_port.has_output()
            || state.aux_port.has_output()
        {
            val.insert(CtrlStatus::OUT_FULL);
        }
        val.set(CtrlStatus::AUX_FULL, state.aux_port.has_output());
        val.set(CtrlStatus::CMD_DATA, state.cmd_prefix.is_some());
        val.set(
            CtrlStatus::SYS_FLAG,
            state.ctrl_cfg.contains(CtrlCfg::SYS_FLAG),
        );

        probes::ps2ctrl_status_read!(|| val.bits());

        val.bits()
    }
    fn update_intr(&self, state: &mut PS2State) {
        // Like QEMU, gate the keyboard interrupt on the clock-disable bit as
        // well as on the interrupt-enable bit.
        let pri_pin = state.pri_pin.as_ref().unwrap();
        if state.ctrl_cfg.contains(CtrlCfg::PRI_INTR_EN)
            && !state.ctrl_cfg.contains(CtrlCfg::PRI_CLOCK_DIS)
            && state.pri_port.has_output()
        {
            probes::ps2ctrl_pulse_pri!(|| {});
            pri_pin.pulse();
        }

        let aux_pin = state.aux_pin.as_ref().unwrap();
        if state.ctrl_cfg.contains(CtrlCfg::AUX_INTR_EN)
            && state.aux_port.has_output()
        {
            probes::ps2ctrl_pulse_aux!(|| {});
            aux_pin.pulse();
        }
    }
    fn reset(&self) {
        let mut state = self.state.lock().unwrap();
        state.pri_port.reset();
        state.aux_port.reset();
        state.resp = None;
        state.cmd_prefix = None;
        state.ctrl_cfg = CtrlCfg::default();
        state.ctrl_out_port = CtrlOutPort::default();
        for b in state.ram.iter_mut() {
            *b = 0;
        }
        self.update_intr(&mut state);
    }
}

impl vmm_devices::KeyboardSink for PS2Ctrl {
    fn key_event(&self, down: bool, keysym: u32) {
        PS2Ctrl::key_event(
            self,
            KeyEvent {
                keysym_raw: keysym,
                is_pressed: down,
            },
        );
    }
}

impl Lifecycle for PS2Ctrl {
    fn type_name(&self) -> &'static str {
        "ps2ctrl"
    }

    fn reset(&self) {
        PS2Ctrl::reset(self);
    }
}

#[derive(Clone, Copy)]
pub(crate) enum PS2ScanCodeSet {
    Set1,
    Set2,
    // Scan code set 3 is not supported.
}
impl PS2ScanCodeSet {
    pub(super) fn as_byte(&self) -> u8 {
        match self {
            PS2ScanCodeSet::Set1 => 0x1,
            PS2ScanCodeSet::Set2 => 0x2,
        }
    }
}

// TODO: wire up remote console to enabled/led_status/typematic

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct RecordingPin {
        pulses: AtomicUsize,
    }

    impl RecordingPin {
        fn pulse_count(&self) -> usize {
            self.pulses.load(Ordering::SeqCst)
        }
    }

    impl IntrPin for RecordingPin {
        fn assert(&self) {}

        fn deassert(&self) {}

        fn is_asserted(&self) -> bool {
            false
        }

        fn pulse(&self) {
            self.pulses.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<SuspendHow>>);

    impl RecordingSink {
        fn recorded(&self) -> Vec<SuspendHow> {
            self.0.lock().expect("recording lock poisoned").clone()
        }
    }

    impl SuspendSink for RecordingSink {
        fn suspend(&self, how: SuspendHow) {
            self.0.lock().expect("recording lock poisoned").push(how);
        }
    }

    fn attached(
        reset_sink: Option<Arc<dyn SuspendSink>>,
    ) -> (PioBus, Arc<PS2Ctrl>, Arc<RecordingPin>) {
        let bus = PioBus::new();
        let ctrl = PS2Ctrl::create();
        let primary = Arc::new(RecordingPin::default());
        let auxiliary = Arc::new(RecordingPin::default());
        let pri_arg: Arc<dyn IntrPin> = primary.clone();
        let aux_arg: Arc<dyn IntrPin> = auxiliary;
        ctrl.attach(&bus, pri_arg, aux_arg, reset_sink);
        (bus, ctrl, primary)
    }

    fn set_ctrl_cfg(bus: &PioBus, cfg: &CtrlCfg) {
        bus.handle_out(PORT_PS2_CMD_STATUS, 1, PS2C_CMD_WRITE_CTRL_CFG.into());
        bus.handle_out(PORT_PS2_DATA, 1, cfg.bits().into());
    }

    fn key(keysym_raw: u32, is_pressed: bool) -> KeyEvent {
        KeyEvent {
            keysym_raw,
            is_pressed,
        }
    }

    #[test]
    fn fresh_controller_reports_idle_status() {
        let (bus, _ctrl, _primary) = attached(None);

        assert_eq!(
            bus.handle_in(PORT_PS2_CMD_STATUS, 1),
            u32::from(CtrlStatus::UNLOCKED.bits())
        );
    }

    #[test]
    fn controller_self_test_passes() {
        let (bus, _ctrl, _primary) = attached(None);

        bus.handle_out(PORT_PS2_CMD_STATUS, 1, PS2C_CMD_CTRL_TEST.into());

        assert_eq!(
            bus.handle_in(PORT_PS2_DATA, 1),
            u32::from(PS2C_R_CTRL_TEST_PASS)
        );
    }

    #[test]
    fn wide_port_accesses_use_the_low_byte() {
        for bytes in [2, 4] {
            let (bus, _ctrl, _primary) = attached(None);

            bus.handle_out(
                PORT_PS2_CMD_STATUS,
                bytes,
                PS2C_CMD_CTRL_TEST.into(),
            );

            assert_eq!(
                bus.handle_in(PORT_PS2_DATA, bytes),
                u32::from(PS2C_R_CTRL_TEST_PASS)
            );
        }
    }

    #[test]
    fn primary_port_test_passes() {
        let (bus, _ctrl, _primary) = attached(None);

        bus.handle_out(PORT_PS2_CMD_STATUS, 1, PS2C_CMD_PRI_PORT_TEST.into());

        assert_eq!(
            bus.handle_in(PORT_PS2_DATA, 1),
            u32::from(PS2C_R_PORT_TEST_PASS)
        );
    }

    #[test]
    fn controller_config_round_trips() {
        let (bus, _ctrl, _primary) = attached(None);
        let cfg = CtrlCfg::PRI_INTR_EN | CtrlCfg::PRI_XLATE_EN;

        bus.handle_out(PORT_PS2_CMD_STATUS, 1, PS2C_CMD_READ_CTRL_CFG.into());
        assert_eq!(bus.handle_in(PORT_PS2_DATA, 1), 0);

        set_ctrl_cfg(&bus, &cfg);
        bus.handle_out(PORT_PS2_CMD_STATUS, 1, PS2C_CMD_READ_CTRL_CFG.into());

        assert_eq!(bus.handle_in(PORT_PS2_DATA, 1), u32::from(cfg.bits()));
    }

    #[test]
    fn read_ram_top_of_range_does_not_panic() {
        let (bus, _ctrl, _primary) = attached(None);

        bus.handle_out(PORT_PS2_CMD_STATUS, 1, PS2C_CMD_READ_RAM_END.into());

        assert_eq!(bus.handle_in(PORT_PS2_DATA, 1), 0);
    }

    #[test]
    fn ram_round_trips_over_full_command_range() {
        let (bus, _ctrl, _primary) = attached(None);

        for n in 0..PS2C_RAM_LEN as u8 {
            bus.handle_out(
                PORT_PS2_CMD_STATUS,
                1,
                (PS2C_CMD_WRITE_RAM_START + n).into(),
            );
            bus.handle_out(PORT_PS2_DATA, 1, n.into());
        }

        for n in 0..PS2C_RAM_LEN as u8 {
            bus.handle_out(
                PORT_PS2_CMD_STATUS,
                1,
                (PS2C_CMD_READ_RAM_START + n).into(),
            );
            assert_eq!(bus.handle_in(PORT_PS2_DATA, 1), u32::from(n));
        }
    }

    #[test]
    fn write_ram_uses_command_prefix_not_data_byte() {
        let (bus, _ctrl, _primary) = attached(None);

        for n in 0..PS2C_RAM_LEN as u8 {
            bus.handle_out(
                PORT_PS2_CMD_STATUS,
                1,
                (PS2C_CMD_WRITE_RAM_START + n).into(),
            );
            bus.handle_out(PORT_PS2_DATA, 1, 0xff);
        }

        bus.handle_out(PORT_PS2_CMD_STATUS, 1, PS2C_CMD_WRITE_RAM_START.into());
        bus.handle_out(PORT_PS2_DATA, 1, 0x00);

        for n in 0..PS2C_RAM_LEN as u8 {
            bus.handle_out(
                PORT_PS2_CMD_STATUS,
                1,
                (PS2C_CMD_READ_RAM_START + n).into(),
            );
            let expected = if n == 0 { 0x00 } else { 0xff };
            assert_eq!(bus.handle_in(PORT_PS2_DATA, 1), expected);
        }
    }

    #[test]
    fn every_data_byte_after_write_ram_prefix_is_safe() {
        let (bus, _ctrl, _primary) = attached(None);

        for v in u8::MIN..=u8::MAX {
            bus.handle_out(
                PORT_PS2_CMD_STATUS,
                1,
                PS2C_CMD_WRITE_RAM_START.into(),
            );
            bus.handle_out(PORT_PS2_DATA, 1, v.into());
        }
    }

    #[test]
    fn keyboard_identifies_as_mf2() {
        let (bus, _ctrl, _primary) = attached(None);

        bus.handle_out(PORT_PS2_DATA, 1, 0xf2);

        assert_eq!(bus.handle_in(PORT_PS2_DATA, 1), 0xfa);
        assert_eq!(bus.handle_in(PORT_PS2_DATA, 1), 0xab);
        assert_eq!(bus.handle_in(PORT_PS2_DATA, 1), 0x83);
    }

    #[test]
    fn translation_selects_guest_visible_scan_code_set() {
        let (set1_bus, set1_ctrl, _primary) = attached(None);
        set_ctrl_cfg(&set1_bus, &CtrlCfg::PRI_XLATE_EN);

        set1_ctrl.key_event(key(0x61, true));
        assert_eq!(set1_bus.handle_in(PORT_PS2_DATA, 1), 0x1e);
        set1_ctrl.key_event(key(0x61, false));
        assert_eq!(set1_bus.handle_in(PORT_PS2_DATA, 1), 0x9e);

        let (set2_bus, set2_ctrl, _primary) = attached(None);
        set_ctrl_cfg(&set2_bus, &CtrlCfg::empty());

        set2_ctrl.key_event(key(0x61, true));
        assert_eq!(set2_bus.handle_in(PORT_PS2_DATA, 1), 0x1c);
        set2_ctrl.key_event(key(0x61, false));
        assert_eq!(set2_bus.handle_in(PORT_PS2_DATA, 1), 0xf0);
        assert_eq!(set2_bus.handle_in(PORT_PS2_DATA, 1), 0x1c);
    }

    #[test]
    fn extended_key_uses_selected_scan_code_set() {
        let (set1_bus, set1_ctrl, _primary) = attached(None);
        set_ctrl_cfg(&set1_bus, &CtrlCfg::PRI_XLATE_EN);

        set1_ctrl.key_event(key(0xff53, true));
        assert_eq!(set1_bus.handle_in(PORT_PS2_DATA, 1), 0xe0);
        assert_eq!(set1_bus.handle_in(PORT_PS2_DATA, 1), 0x4d);

        let (set2_bus, set2_ctrl, _primary) = attached(None);
        set_ctrl_cfg(&set2_bus, &CtrlCfg::empty());

        set2_ctrl.key_event(key(0xff53, true));
        assert_eq!(set2_bus.handle_in(PORT_PS2_DATA, 1), 0xe0);
        assert_eq!(set2_bus.handle_in(PORT_PS2_DATA, 1), 0x74);
    }

    #[test]
    fn keyboard_data_pulses_primary_irq_when_enabled() {
        let (bus, ctrl, primary) = attached(None);
        set_ctrl_cfg(&bus, &CtrlCfg::PRI_INTR_EN);

        ctrl.key_event(key(0x61, true));

        assert_eq!(primary.pulse_count(), 1);
    }

    #[test]
    fn pulse_reset_command_requests_vm_reset() {
        let sink = Arc::new(RecordingSink::default());
        let sink_arg: Arc<dyn SuspendSink> = sink.clone();
        let (bus, _ctrl, _primary) = attached(Some(sink_arg));

        bus.handle_out(PORT_PS2_CMD_STATUS, 1, 0xfe);

        assert_eq!(sink.recorded(), vec![SuspendHow::Reset]);
    }
}
