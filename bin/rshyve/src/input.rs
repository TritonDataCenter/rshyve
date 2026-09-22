// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Legacy input device setup.

use std::sync::Arc;

use slog::{info, Logger};

use vmm_core::intr_pins::LegacyPIC;
use vmm_core::machine::Machine;
use vmm_devices::acpi_pm::SuspendSink;
use vmm_devices::InputBroker;
use vmm_hid::ps2::{
    PS2Ctrl, IRQ_PS2_AUX, IRQ_PS2_PRI, PORT_PS2_CMD_STATUS, PORT_PS2_DATA,
};

pub fn setup_ps2(
    machine: &Machine,
    pic: &Arc<LegacyPIC>,
    suspend_sink: Arc<dyn SuspendSink>,
    input: &Arc<InputBroker>,
    log: &Logger,
) -> Arc<PS2Ctrl> {
    let pri = pic.pin_or_noop(IRQ_PS2_PRI);
    let aux = pic.pin_or_noop(IRQ_PS2_AUX);
    let ps2 = PS2Ctrl::create();
    ps2.attach(machine.bus_pio(), pri, aux, Some(suspend_sink));
    input.set_keyboard(ps2.clone());
    info!(log, "PS/2 controller attached";
        "data_port" => format!("{:#x}", PORT_PS2_DATA),
        "status_port" => format!("{:#x}", PORT_PS2_CMD_STATUS),
    );
    ps2
}
