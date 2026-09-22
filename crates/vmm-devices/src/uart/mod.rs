// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! UART serial port emulation.
//!
//! This module provides a 16550-compatible UART:
//!
//! - [`uart16550::Uart`] -- register-level state machine.
//! - [`lpc::LpcUart`] -- LPC-attached wrapper that bridges the UART
//!   to a PIO bus and interrupt pin.
//!
//! Standard PC COM port assignments:
//!
//! | Port | Base   | IRQ |
//! |------|--------|-----|
//! | COM1 | 0x3F8  |  4  |
//! | COM2 | 0x2F8  |  3  |

pub mod backend;
pub mod lpc;
pub mod uart16550;

pub use backend::{attach, SerialBackend};
pub use lpc::LpcUart;
pub use uart16550::Uart;
