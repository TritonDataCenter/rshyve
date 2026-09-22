// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

pub mod ctrl;
mod kbd;
mod keyboard;
mod keysym;
mod mouse;

pub use ctrl::PS2Ctrl;
pub use keyboard::KeyEvent;

pub const PORT_PS2_DATA: u16 = 0x60;
pub const PORT_PS2_CMD_STATUS: u16 = 0x64;

pub const IRQ_PS2_PRI: u8 = 1;
pub const IRQ_PS2_AUX: u8 = 12;
