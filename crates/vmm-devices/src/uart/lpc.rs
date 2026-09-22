// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! LPC-attached UART.
//!
//! Bridges a [`Uart`] to the PIO bus and an interrupt pin, providing the
//! standard PC serial port interface. The standard configurations are:
//!
//! | Port  | Base   | IRQ |
//! |-------|--------|-----|
//! | COM1  | 0x3F8  |  4  |
//! | COM2  | 0x2F8  |  3  |
//!
//! # Thread safety
//!
//! One `Mutex` holds all UART state. Every register access syncs the
//! interrupt pin, so the IRQ line always matches the logical state.
//!
//! # External data path
//!
//! Use [`LpcUart::input_byte`] to feed data from an external source
//! (e.g., a terminal emulator) into the guest's RX FIFO, and
//! [`LpcUart::output_byte`] to drain data the guest has transmitted.

use std::sync::{Arc, Mutex};

use vmm_core::common::RWOp;
use vmm_core::intr_pins::IntrPin;
use vmm_core::pio::{PioBus, PioFn};

use crate::Lifecycle;

use super::uart16550::Uart;

/// Number of I/O ports occupied by a 16550 UART (offsets 0-7).
pub const REGISTER_LEN: u16 = 8;

/// Standard I/O base address for COM1.
pub const COM1_BASE: u16 = 0x3F8;

/// Standard IRQ for COM1.
pub const COM1_IRQ: u8 = 4;

/// Standard I/O base address for COM2.
pub const COM2_BASE: u16 = 0x2F8;

/// Standard IRQ for COM2.
pub const COM2_IRQ: u8 = 3;

// ---------------------------------------------------------------------------
// Inner state behind the mutex
// ---------------------------------------------------------------------------

struct UartState {
    uart: Uart,
    irq_pin: Arc<dyn IntrPin>,
}

impl UartState {
    /// Drive the physical interrupt pin to match the UART's logical
    /// interrupt state.
    #[inline]
    fn sync_intr_pin(&self) {
        self.irq_pin.set_state(self.uart.intr_state());
    }
}

// ---------------------------------------------------------------------------
// LpcUart -- public API
// ---------------------------------------------------------------------------

/// Callback for synchronous TX output. It runs in PIO handler context.
pub type TxSink = Box<dyn Fn(u8) + Send + Sync>;

/// An LPC-attached 16550 UART.
///
/// Wraps [`Uart`] with PIO bus registration and interrupt pin
/// synchronisation.
pub struct LpcUart {
    state: Mutex<UartState>,
    /// When set, every PIO access drains the TX FIFO into this sink,
    /// on the vCPU thread with the UART lock held.
    tx_sink: Mutex<Option<TxSink>>,
}

impl LpcUart {
    /// Create a new LPC UART wired to the given interrupt pin.
    pub fn new(irq_pin: Arc<dyn IntrPin>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(UartState {
                uart: Uart::new(),
                irq_pin,
            }),
            tx_sink: Mutex::new(None),
        })
    }

    /// Set a synchronous TX sink. It receives each byte in the same PIO
    /// exit as the guest's THR write, so output has no buffer delay.
    pub fn set_tx_sink(&self, sink: TxSink) {
        *self.tx_sink.lock().expect("tx_sink lock") = Some(sink);
    }

    /// Register PIO handlers on `bus` at the given `base` port address.
    ///
    /// This covers ports `base` through `base + 7`.
    ///
    /// # Panics
    ///
    /// Panics if the port range conflicts with an existing registration
    /// on `bus`.
    pub fn attach(self: &Arc<Self>, bus: &PioBus, base: u16) {
        let this = Arc::clone(self);
        let handler: Arc<PioFn> =
            Arc::new(move |offset: u16, rwo: RWOp<'_>| {
                this.pio_rw(offset, rwo);
            });
        bus.register(base, REGISTER_LEN, handler)
            .expect("LpcUart: port range conflict during attach");
    }

    /// Feed a byte from an external source into the guest's RX path.
    ///
    /// Returns `true` if the byte was accepted, `false` if the RX FIFO
    /// was full (byte dropped).
    pub fn input_byte(&self, data: u8) -> bool {
        let mut state = self.state.lock().unwrap();
        let accepted = state.uart.data_write(data);
        state.sync_intr_pin();
        accepted
    }

    /// Drain a byte from the guest's TX path.
    ///
    /// Returns `None` when there is no pending output.
    pub fn output_byte(&self) -> Option<u8> {
        let mut state = self.state.lock().unwrap();
        let byte = state.uart.data_read();
        state.sync_intr_pin();
        byte
    }

    /// Reset the UART to its power-on state.
    pub fn reset(&self) {
        let mut state = self.state.lock().unwrap();
        state.uart.reset();
        state.sync_intr_pin();
    }

    // -- private -----------------------------------------------------------

    /// Handle a PIO read or write from a vCPU thread.
    fn pio_rw(&self, offset: u16, rwo: RWOp<'_>) {
        let reg_offset = (offset & 0x07) as u8;

        let mut state = self.state.lock().unwrap();

        match rwo {
            RWOp::Read(ro) => {
                ro.write_u8(state.uart.reg_read(reg_offset));
            }
            RWOp::Write(wo) => {
                state.uart.reg_write(reg_offset, wo.read_u8());
            }
        }

        // Drain TX into the sink in this exit, as C bhyve does, so serial
        // output has no buffer delay.
        if let Ok(sink) = self.tx_sink.lock() {
            if let Some(ref sink_fn) = *sink {
                while let Some(b) = state.uart.data_read() {
                    sink_fn(b);
                }
            }
        }

        state.sync_intr_pin();
    }
}

impl Lifecycle for LpcUart {
    fn type_name(&self) -> &'static str {
        "lpc-uart"
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A test interrupt pin that tracks assertion state.
    struct TestPin {
        asserted: AtomicBool,
    }

    impl TestPin {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                asserted: AtomicBool::new(false),
            })
        }
    }

    impl IntrPin for TestPin {
        fn assert(&self) {
            self.asserted.store(true, Ordering::SeqCst);
        }
        fn deassert(&self) {
            self.asserted.store(false, Ordering::SeqCst);
        }
        fn is_asserted(&self) -> bool {
            self.asserted.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn attach_and_read_lsr() {
        let pin = TestPin::new();
        let bus = PioBus::new();
        let uart = LpcUart::new(pin.clone());
        uart.attach(&bus, COM1_BASE);

        // Read LSR (offset 5 from base) -- should show THRE|TEMT.
        let val = bus.handle_in(COM1_BASE + 5, 1);
        assert_eq!(val, 0x60, "LSR should be THRE|TEMT at reset");
    }

    #[test]
    fn input_output_byte() {
        let pin = TestPin::new();
        let uart = LpcUart::new(pin);

        // Feed bytes in until FIFO is full (256 bytes).
        for i in 0..256u16 {
            assert!(uart.input_byte(i as u8), "input_byte {i} should succeed");
        }
        // The FIFO is full, so the next byte is rejected.
        assert!(!uart.input_byte(0xFF));

        // Nothing in TX yet.
        assert_eq!(uart.output_byte(), None);
    }

    #[test]
    fn guest_tx_and_output_byte() {
        let pin = TestPin::new();
        let bus = PioBus::new();
        let uart = LpcUart::new(pin.clone());
        uart.attach(&bus, COM1_BASE);

        // Guest writes to THR (offset 0).
        bus.handle_out(COM1_BASE, 1, 0x61);

        // External consumer drains it.
        assert_eq!(uart.output_byte(), Some(0x61));
        assert_eq!(uart.output_byte(), None);
    }

    #[test]
    fn guest_rx_and_input_byte() {
        let pin = TestPin::new();
        let bus = PioBus::new();
        let uart = LpcUart::new(pin);
        uart.attach(&bus, COM1_BASE);

        // External source feeds data.
        assert!(uart.input_byte(0x62));

        // Guest reads RBR (offset 0).
        let val = bus.handle_in(COM1_BASE, 1);
        assert_eq!(val, 0x62);
    }

    #[test]
    fn interrupt_pin_synced() {
        let pin = TestPin::new();
        let bus = PioBus::new();
        let uart = LpcUart::new(pin.clone());
        uart.attach(&bus, COM1_BASE);

        // Enable ERBFI (data available interrupt).
        bus.handle_out(COM1_BASE + 1, 1, 0x01);
        assert!(!pin.is_asserted());

        // Feed data -- pin should assert.
        assert!(uart.input_byte(0x42));
        assert!(pin.is_asserted());

        // Guest reads the byte -- pin should deassert.
        let _ = bus.handle_in(COM1_BASE, 1);
        assert!(!pin.is_asserted());
    }

    #[test]
    fn reset_deasserts_pin() {
        let pin = TestPin::new();
        let uart = LpcUart::new(pin.clone());

        // Enable ETBEI via internal state to get THRE interrupt.
        {
            let mut state = uart.state.lock().unwrap();
            state.uart.reg_write(1, 0x02); // IER = ETBEI
            state.sync_intr_pin();
        }
        assert!(pin.is_asserted());

        uart.reset();
        assert!(!pin.is_asserted());
    }
}
