// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Portions derived from Propolis, Copyright Oxide Computer Company,
// used under MPL-2.0. Upstream: lib/propolis/src/hw/uart/uart16550.rs
// https://github.com/oxidecomputer/propolis

//! 16550 UART register emulation.
//!
//! Implements the full register set of a National Semiconductor 16550
//! UART: RBR/THR, IER, IIR/FCR, LCR, MCR, LSR, MSR, SCR, and the
//! divisor latch registers (DLL/DLH) accessed via the DLAB bit in LCR.
//!
//! ## Data paths
//!
//! **Guest to host (TX):** The guest writes a byte to the Transmitter
//! Holding Register (THR at offset 0, DLAB=0), which queues it in
//! `tx_fifo`. An external consumer drains it with [`Uart::data_read`].
//!
//! **Host to guest (RX):** An external producer puts a byte in
//! `rx_fifo` with [`Uart::data_write`]. The guest reads it from the
//! Receiver Buffer Register (RBR at offset 0, DLAB=0). The Data Ready
//! interrupt clears when the FIFO is empty.
//!
//! ## Interrupt priority (highest to lowest)
//!
//! 1. Receiver Line Status (RLS) -- overrun error
//! 2. Data Ready (DR)
//! 3. Transmitter Holding Register Empty (THRE)
//! 4. Modem Status (MDM)
//!
//! Reading the IIR while THRE is the active interrupt clears the THRE
//! interrupt (but leaves the THRE/TEMT bits in LSR asserted).

use std::collections::VecDeque;

use bitflags::bitflags;

// ---------------------------------------------------------------------------
// Public UART core
// ---------------------------------------------------------------------------

/// 16550 UART register state machine.
pub struct Uart {
    reg_intr_enable: IntrEnaReg,
    reg_intr_ident: IntrIdentReg,
    reg_line_ctrl: LineCtrlReg,
    reg_line_status: LineStatusReg,
    reg_modem_ctrl: ModemCtrlReg,
    reg_modem_status: u8,
    reg_scratch: u8,
    reg_div_low: u8,
    reg_div_high: u8,

    /// Separate tracking for the THRE interrupt because reading IIR
    /// clears it while LSR THRE/TEMT bits remain asserted.
    thre_intr: bool,

    /// Synthesized interrupt pin state, read by the LPC wrapper to
    /// drive the actual `IntrPin`.
    intr_pin: bool,

    rx_fifo: Fifo,
    tx_fifo: Fifo,
}

impl Uart {
    /// Create a new UART in its power-on reset state.
    pub fn new() -> Self {
        Self {
            reg_intr_enable: IntrEnaReg::empty(),
            reg_intr_ident: IntrIdentReg::NOPEND,
            reg_line_ctrl: LineCtrlReg::empty(),
            reg_line_status: LineStatusReg::THRE | LineStatusReg::TEMT,
            reg_modem_ctrl: ModemCtrlReg::empty(),
            reg_modem_status: 0,
            reg_scratch: 0,
            reg_div_low: 0,
            reg_div_high: 0,
            thre_intr: false,
            intr_pin: false,
            // A real 16550A has 16-byte FIFOs. These hold a full mdata
            // V2 frame (more than 100 bytes) in each direction, so no
            // byte drops between the guest serial driver and the mdata
            // agent poll loop.
            rx_fifo: Fifo::new(256),
            tx_fifo: Fifo::new(256),
        }
    }

    /// Read a UART register.
    ///
    /// An `offset` outside 0..=7 reads 0.
    pub fn reg_read(&mut self, offset: u8) -> u8 {
        let dlab = self.is_dlab();

        match reg_for_read(offset, dlab) {
            Some(Reg::DivisorLow) => self.reg_div_low,
            Some(Reg::DivisorHigh) => self.reg_div_high,

            Some(Reg::RecvHold) => {
                if let Some(d) = self.rx_fifo.read() {
                    self.update_dr();
                    self.update_isr();
                    d
                } else {
                    0
                }
            }
            Some(Reg::IntrEnable) => self.reg_intr_enable.bits(),
            Some(Reg::IntrIdent) => {
                let val = self.reg_intr_ident;
                if val.get_intr() == Some(IntrIdent::Thre) {
                    // Reading the IIR clears the THRE interrupt source,
                    // but leaves the THRE/TEMT bits in LSR.
                    self.thre_intr = false;
                    self.update_isr();
                }
                val.bits()
            }
            Some(Reg::LineCtrl) => self.reg_line_ctrl.bits(),
            Some(Reg::ModemCtrl) => self.reg_modem_ctrl.bits(),
            Some(Reg::LineStatus) => {
                let val = self.reg_line_status;
                // OE is cleared on read of LSR.
                self.reg_line_status.remove(LineStatusReg::OE);
                self.update_isr();
                val.bits()
            }
            Some(Reg::ModemStatus) => self.reg_modem_status,
            Some(Reg::Scratch) => self.reg_scratch,
            // Write-only registers and unknown offsets read as 0.
            _ => 0,
        }
    }

    /// Write a UART register.
    ///
    /// A write to an `offset` outside 0..=7 has no effect. No guest
    /// input can cause a panic.
    pub fn reg_write(&mut self, offset: u8, data: u8) {
        let dlab = self.is_dlab();

        match reg_for_write(offset, dlab) {
            Some(Reg::DivisorLow) => {
                self.reg_div_low = data;
            }
            Some(Reg::DivisorHigh) => {
                self.reg_div_high = data;
            }
            Some(Reg::TransmitHold) => {
                if !self.is_loopback() {
                    let _ = self.tx_fifo.write(data);
                    // Model instant TX completion. The drain thread takes
                    // the byte later, but the guest sees TX ready at once
                    // and does not spin on LSR.
                    self.set_thre(true);
                } else {
                    // Loopback feeds TX into RX.
                    if !self.rx_fifo.write(data) {
                        self.reg_line_status.insert(LineStatusReg::OE);
                    }
                    self.update_dr();
                    self.set_thre(true);
                }
            }
            Some(Reg::IntrEnable) => {
                let old = self.reg_intr_enable;
                let new = IntrEnaReg::from_bits_truncate(data);
                self.reg_intr_enable = new;
                // Some guests expect a THRE interrupt when ETBEI is
                // toggled on and the TX FIFO is already empty.
                if !old.contains(IntrEnaReg::ETBEI)
                    && new.contains(IntrEnaReg::ETBEI)
                    && self.tx_fifo.is_empty()
                {
                    self.thre_intr = true;
                }
                self.update_isr();
            }
            Some(Reg::FifoCtrl) => {
                // FIFO mode is not emulated, so the write has no effect.
            }
            Some(Reg::LineCtrl) => {
                // Only the DLAB bit has an effect.
                self.reg_line_ctrl = LineCtrlReg::from_bits_retain(data);
            }
            Some(Reg::ModemCtrl) => {
                self.reg_modem_ctrl = ModemCtrlReg::from_bits_truncate(data);
            }
            Some(Reg::Scratch) => {
                self.reg_scratch = data;
            }
            // Read-only registers / unknown offsets: silently ignore.
            _ => {}
        }
    }

    /// Read data transmitted by the guest (drain `tx_fifo`).
    ///
    /// Returns `None` when the TX FIFO is empty.
    pub fn data_read(&mut self) -> Option<u8> {
        let d = self.tx_fifo.read()?;
        self.set_thre(self.tx_fifo.is_empty());
        Some(d)
    }

    /// Write data to be received by the guest (fill `rx_fifo`).
    ///
    /// Returns `true` if the byte was accepted, `false` if the FIFO
    /// was full (byte is dropped). In loopback mode the serial input
    /// is disconnected and all data is silently discarded.
    pub fn data_write(&mut self, data: u8) -> bool {
        if self.is_loopback() {
            // Per the datasheet, the serial input pin is disconnected.
            return true;
        }
        let accepted = self.rx_fifo.write(data);
        self.update_dr();
        self.update_isr();
        accepted
    }

    /// Returns `true` when the UART has an interrupt pending.
    ///
    /// The LPC wrapper uses this to drive the physical `IntrPin`.
    #[inline]
    pub fn intr_state(&self) -> bool {
        self.intr_pin
    }

    /// Reset all registers and FIFOs to power-on defaults.
    pub fn reset(&mut self) {
        self.reg_intr_enable = IntrEnaReg::empty();
        self.reg_intr_ident = IntrIdentReg::NOPEND;
        self.reg_line_ctrl = LineCtrlReg::empty();
        self.reg_line_status = LineStatusReg::THRE | LineStatusReg::TEMT;
        self.reg_modem_ctrl = ModemCtrlReg::empty();
        self.reg_modem_status = 0;
        self.reg_scratch = 0;
        self.reg_div_low = 0;
        self.reg_div_high = 0;
        self.thre_intr = false;
        self.intr_pin = false;
        self.rx_fifo.reset();
        self.tx_fifo.reset();
    }

    // -- private helpers ----------------------------------------------------

    #[inline]
    fn is_dlab(&self) -> bool {
        self.reg_line_ctrl.contains(LineCtrlReg::DLAB)
    }

    #[inline]
    fn is_loopback(&self) -> bool {
        self.reg_modem_ctrl.contains(ModemCtrlReg::LOOP)
    }

    /// Determine the highest-priority pending interrupt.
    fn next_intr(&self) -> Option<IntrIdent> {
        if self.reg_intr_enable.contains(IntrEnaReg::ELSI)
            && self.reg_line_status.contains(LineStatusReg::OE)
        {
            Some(IntrIdent::Rls)
        } else if self.reg_intr_enable.contains(IntrEnaReg::ERBFI)
            && self.reg_line_status.contains(LineStatusReg::DR)
        {
            Some(IntrIdent::DR)
        } else if self.reg_intr_enable.contains(IntrEnaReg::ETBEI)
            && self.thre_intr
        {
            Some(IntrIdent::Thre)
        } else if self.reg_intr_enable.contains(IntrEnaReg::EDSSI)
            && self.reg_modem_status != 0
        {
            Some(IntrIdent::Mdm)
        } else {
            None
        }
    }

    /// Recompute IIR and `intr_pin` from current state.
    fn update_isr(&mut self) {
        let new = self.next_intr();
        self.reg_intr_ident.set_intr(new);
        self.intr_pin = new.is_some();
    }

    /// Set or clear THRE/TEMT in LSR and the separate `thre_intr` flag.
    fn set_thre(&mut self, state: bool) {
        self.reg_line_status
            .set(LineStatusReg::THRE | LineStatusReg::TEMT, state);
        self.thre_intr = state;
        self.update_isr();
    }

    /// Update Data Ready in LSR based on RX FIFO occupancy.
    fn update_dr(&mut self) {
        self.reg_line_status
            .set(LineStatusReg::DR, !self.rx_fifo.is_empty());
    }
}

impl Default for Uart {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// FIFO
// ---------------------------------------------------------------------------

struct Fifo {
    capacity: usize,
    buf: VecDeque<u8>,
}

impl Fifo {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            buf: VecDeque::with_capacity(capacity),
        }
    }

    /// Push a byte. Returns `true` if accepted, `false` if full.
    fn write(&mut self, data: u8) -> bool {
        if self.buf.len() < self.capacity {
            self.buf.push_back(data);
            true
        } else {
            false
        }
    }

    /// Pop the oldest byte, or `None` if empty.
    fn read(&mut self) -> Option<u8> {
        self.buf.pop_front()
    }

    fn reset(&mut self) {
        self.buf.clear();
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Register decode
// ---------------------------------------------------------------------------

/// Internal register identity used for read/write dispatch.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reg {
    RecvHold,
    TransmitHold,
    IntrEnable,
    IntrIdent,
    FifoCtrl,
    LineCtrl,
    ModemCtrl,
    LineStatus,
    ModemStatus,
    Scratch,
    DivisorLow,
    DivisorHigh,
}

/// Map an offset + DLAB state to the register for a **read** operation.
const fn reg_for_read(offset: u8, dlab: bool) -> Option<Reg> {
    match (offset, dlab) {
        (0, true) => Some(Reg::DivisorLow),
        (0, false) => Some(Reg::RecvHold),
        (1, true) => Some(Reg::DivisorHigh),
        (1, false) => Some(Reg::IntrEnable),
        (2, _) => Some(Reg::IntrIdent),
        (3, _) => Some(Reg::LineCtrl),
        (4, _) => Some(Reg::ModemCtrl),
        (5, _) => Some(Reg::LineStatus),
        (6, _) => Some(Reg::ModemStatus),
        (7, _) => Some(Reg::Scratch),
        _ => None,
    }
}

/// Map an offset + DLAB state to the register for a **write** operation.
const fn reg_for_write(offset: u8, dlab: bool) -> Option<Reg> {
    match (offset, dlab) {
        (0, true) => Some(Reg::DivisorLow),
        (0, false) => Some(Reg::TransmitHold),
        (1, true) => Some(Reg::DivisorHigh),
        (1, false) => Some(Reg::IntrEnable),
        (2, _) => Some(Reg::FifoCtrl),
        (3, _) => Some(Reg::LineCtrl),
        (4, _) => Some(Reg::ModemCtrl),
        // 5 (LSR) and 6 (MSR) are read-only.
        (5, _) => None,
        (6, _) => None,
        (7, _) => Some(Reg::Scratch),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Bitflag register types
// ---------------------------------------------------------------------------

bitflags! {
    /// Interrupt Enable Register (IER).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct IntrEnaReg: u8 {
        /// Enable Received Data Available interrupt.
        const ERBFI = 1 << 0;
        /// Enable Transmitter Holding Register Empty interrupt.
        const ETBEI = 1 << 1;
        /// Enable Receiver Line Status interrupt.
        const ELSI  = 1 << 2;
        /// Enable Modem Status interrupt.
        const EDSSI = 1 << 3;
    }
}

bitflags! {
    /// Interrupt Identification Register (IIR).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct IntrIdentReg: u8 {
        /// No interrupt pending (set when idle).
        const NOPEND = 1;
        /// Mask covering the interrupt ID bits.
        const INTID  = 0b1110;
    }
}

bitflags! {
    /// Line Control Register (LCR).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct LineCtrlReg: u8 {
        /// Word Length Select (2 bits).
        const WLS  = 0b11;
        /// Number of Stop Bits.
        const STB  = 1 << 2;
        /// Parity Enable.
        const PEN  = 1 << 3;
        /// Even Parity Select.
        const EPS  = 1 << 4;
        /// Stick Parity.
        const SP   = 1 << 5;
        /// Break Control.
        const BC   = 1 << 6;
        /// Divisor Latch Access Bit.
        const DLAB = 1 << 7;
    }
}

bitflags! {
    /// Line Status Register (LSR).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct LineStatusReg: u8 {
        /// Data Ready.
        const DR   = 1 << 0;
        /// Overrun Error.
        const OE   = 1 << 1;
        /// Transmitter Holding Register Empty.
        const THRE = 1 << 5;
        /// Transmitter Empty.
        const TEMT = 1 << 6;
    }
}

bitflags! {
    /// Modem Control Register (MCR).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct ModemCtrlReg: u8 {
        /// Loopback mode.
        const LOOP = 1 << 4;
    }
}

// ---------------------------------------------------------------------------
// Interrupt identification values
// ---------------------------------------------------------------------------

/// Interrupt source identity, encoded as the IIR INTID field value.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntrIdent {
    /// Modem Status -- priority 4 (lowest).
    Mdm = 0b0000,
    /// Transmitter Holding Register Empty -- priority 3.
    Thre = 0b0010,
    /// Data Ready -- priority 2.
    DR = 0b0100,
    /// Receiver Line Status -- priority 1 (highest).
    Rls = 0b0110,
}

impl IntrIdent {
    /// Try to decode a raw IIR INTID field value.
    const fn from_bits(val: u8) -> Option<Self> {
        match val {
            0b0000 => Some(Self::Mdm),
            0b0010 => Some(Self::Thre),
            0b0100 => Some(Self::DR),
            0b0110 => Some(Self::Rls),
            _ => None,
        }
    }
}

impl IntrIdentReg {
    /// Update the IIR to reflect the given interrupt source (or none).
    fn set_intr(&mut self, id: Option<IntrIdent>) {
        self.remove(IntrIdentReg::INTID);
        if let Some(intr) = id {
            *self = Self::from_bits_retain(
                (self.bits() & !Self::INTID.bits()) | intr as u8,
            );
            self.remove(IntrIdentReg::NOPEND);
        } else {
            self.insert(IntrIdentReg::NOPEND);
        }
    }

    /// Decode the current interrupt source from the IIR.
    fn get_intr(&self) -> Option<IntrIdent> {
        if self.contains(IntrIdentReg::NOPEND) {
            None
        } else {
            IntrIdent::from_bits(self.intersection(IntrIdentReg::INTID).bits())
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Register offsets.
    const REG_RBR: u8 = 0;
    const REG_THR: u8 = 0;
    const REG_IER: u8 = 1;
    const REG_IIR: u8 = 2;
    const REG_LCR: u8 = 3;
    const REG_MCR: u8 = 4;
    const REG_LSR: u8 = 5;
    const REG_SCR: u8 = 7;

    // IER bits.
    const IER_ERBFI: u8 = 1 << 0;
    const IER_ETBEI: u8 = 1 << 1;
    const IER_ELSI: u8 = 1 << 2;
    const IER_EDSSI: u8 = 1 << 3;

    // IIR sources.
    const IIR_NONE: u8 = 0b0001;
    const IIR_THRE: u8 = 0b0010;
    const IIR_DR: u8 = 0b0100;
    const IIR_RLS: u8 = 0b0110;

    // LSR bits.
    const LSR_DR: u8 = 1 << 0;
    const LSR_OE: u8 = 1 << 1;
    const LSR_THRE: u8 = 1 << 5;
    const LSR_TEMT: u8 = 1 << 6;

    // MCR bits.
    const MCR_LOOP: u8 = 1 << 4;

    // LCR bits.
    const LCR_DLAB: u8 = 1 << 7;

    // IIR mask.
    const MASK_IIR: u8 = 0b0000_1111;

    #[test]
    fn reset_state() {
        let mut uart = Uart::new();
        assert_eq!(uart.reg_read(REG_IER), 0);
        assert_eq!(uart.reg_read(REG_IIR), IIR_NONE);
        assert_eq!(uart.reg_read(REG_LCR), 0);
        assert_eq!(uart.reg_read(REG_MCR), 0);
        assert_eq!(
            uart.reg_read(REG_LSR),
            LSR_THRE | LSR_TEMT,
            "LSR should show THRE|TEMT at reset"
        );
    }

    #[test]
    fn scratch_register_roundtrip() {
        let mut uart = Uart::new();
        uart.reg_write(REG_SCR, 0xA5);
        assert_eq!(uart.reg_read(REG_SCR), 0xA5);
        uart.reg_write(REG_SCR, 0x00);
        assert_eq!(uart.reg_read(REG_SCR), 0x00);
    }

    #[test]
    fn dlab_divisor_latch() {
        let mut uart = Uart::new();
        // Enable DLAB.
        uart.reg_write(REG_LCR, LCR_DLAB);
        // Write divisor latch registers (offset 0 and 1).
        uart.reg_write(0, 0x0C); // DLL
        uart.reg_write(1, 0x00); // DLH
        assert_eq!(uart.reg_read(0), 0x0C);
        assert_eq!(uart.reg_read(1), 0x00);
        // Clear DLAB -- offset 0 should now be RBR/THR again.
        uart.reg_write(REG_LCR, 0);
        assert_eq!(uart.reg_read(0), 0); // RBR (empty)
    }

    #[test]
    fn intr_thre_on_etbei_toggle() {
        let mut uart = Uart::new();
        // No interrupts enabled -- none should be asserted.
        uart.reg_write(REG_IER, 0);
        assert_eq!(uart.reg_read(REG_LSR) & LSR_THRE, LSR_THRE);
        assert_eq!(uart.reg_read(REG_IIR) & MASK_IIR, IIR_NONE);
        assert!(!uart.intr_state());

        // Enable THRE interrupt.
        uart.reg_write(REG_IER, IER_ETBEI);
        assert_eq!(uart.reg_read(REG_LSR) & LSR_THRE, LSR_THRE);
        assert!(uart.intr_state());
        assert_eq!(uart.reg_read(REG_IIR) & MASK_IIR, IIR_THRE);

        // After reading IIR, THRE interrupt should deassert.
        assert!(!uart.intr_state());
        assert_eq!(uart.reg_read(REG_IIR) & MASK_IIR, IIR_NONE);

        // LSR should still show THRE.
        assert_eq!(uart.reg_read(REG_LSR) & LSR_THRE, LSR_THRE);
    }

    #[test]
    fn intr_dr_on_incoming() {
        let mut uart = Uart::new();
        let tval = 0x20;

        uart.reg_write(REG_IER, IER_ERBFI);
        assert!(!uart.intr_state());
        assert_eq!(uart.reg_read(REG_IIR) & MASK_IIR, IIR_NONE);

        uart.data_write(tval);
        assert!(uart.intr_state());
        assert_eq!(uart.reg_read(REG_IIR) & MASK_IIR, IIR_DR);
        assert_eq!(uart.reg_read(REG_RBR), tval);
        assert!(!uart.intr_state());
        assert_eq!(uart.reg_read(REG_IIR) & MASK_IIR, IIR_NONE);
    }

    #[test]
    fn intr_thre_on_outgoing() {
        let mut uart = Uart::new();
        let tval = 0x20;

        uart.reg_write(REG_IER, 0);
        assert!(!uart.intr_state());

        // The instant TX model sets THRE on the THR write.
        uart.reg_write(REG_THR, tval);
        // THRE is set, so enabling the interrupt fires it at once.
        uart.reg_write(REG_IER, IER_ETBEI);
        assert!(uart.intr_state());
        assert_eq!(uart.reg_read(REG_IIR) & MASK_IIR, IIR_THRE);

        // Cleared after IIR read.
        assert!(!uart.intr_state());

        // External consumer can still drain TX FIFO.
        assert_eq!(uart.data_read(), Some(tval));
    }

    #[test]
    fn interrupt_priority_order() {
        let mut uart = Uart::new();

        // Enable all interrupt sources.
        uart.reg_write(REG_IER, IER_ERBFI | IER_ETBEI | IER_ELSI | IER_EDSSI);

        // TX empty so THRE fires.
        assert_eq!(uart.reg_read(REG_IIR), IIR_THRE);

        // Loopback lets the test cause an overrun.
        uart.reg_write(REG_MCR, MCR_LOOP);

        // Loopback data to fill the RX FIFO and assert DR.
        let rval = 0x20;
        for _ in 0..256 {
            uart.reg_write(REG_THR, rval);
        }
        assert_eq!(uart.reg_read(REG_IIR), IIR_DR);

        // Overrun: write again with RX FIFO full.
        uart.reg_write(REG_THR, rval);
        assert_eq!(uart.reg_read(REG_IIR), IIR_RLS);

        // Clear OE by reading LSR.
        assert_ne!(uart.reg_read(REG_LSR) & LSR_OE, 0);
        assert_eq!(uart.reg_read(REG_IIR), IIR_DR);

        // Clear DR by reading all RBR bytes.
        for _ in 0..256 {
            assert_eq!(uart.reg_read(REG_RBR), rval);
        }
        assert_eq!(uart.reg_read(REG_IIR), IIR_THRE);

        // Leave loopback, queue outgoing data.
        // With instant TX, THRE stays set after THR write.
        uart.reg_write(REG_MCR, 0);
        let tval = 0x40;
        uart.reg_write(REG_THR, tval);
        // THRE interrupt fires since ETBEI is enabled and THRE is set
        assert_eq!(uart.reg_read(REG_IIR) & MASK_IIR, IIR_THRE);
        assert_eq!(uart.data_read(), Some(tval));
    }

    #[test]
    fn safe_read_write_all_offsets() {
        let mut uart = Uart::new();

        // All offsets 0-7 must be safe to read and write.
        for i in 0..=7 {
            let _ = uart.reg_read(i);
        }
        for i in 0..=7 {
            uart.reg_write(i, 0xFF);
        }
        // DLAB is now set (LCR was written with 0xFF).
        // Verify divisor registers are accessible.
        let _ = uart.reg_read(0);
        let _ = uart.reg_read(1);
        uart.reg_write(0, 0xFF);
        uart.reg_write(1, 0xFF);
    }

    #[test]
    fn out_of_range_offset_is_safe() {
        let mut uart = Uart::new();
        // Offsets >= 8 must not panic.
        assert_eq!(uart.reg_read(8), 0);
        assert_eq!(uart.reg_read(255), 0);
        uart.reg_write(8, 0xFF);
        uart.reg_write(255, 0xFF);
    }

    #[test]
    fn loopback_discards_external_input() {
        let mut uart = Uart::new();
        uart.reg_write(REG_MCR, MCR_LOOP);
        // External data_write should be silently discarded.
        assert!(uart.data_write(0x42));
        // RX FIFO should remain empty (no DR).
        assert_eq!(uart.reg_read(REG_LSR) & LSR_DR, 0);
    }

    #[test]
    fn data_write_returns_false_when_full() {
        let mut uart = Uart::new();
        for i in 0..256 {
            assert!(uart.data_write(i as u8), "write {i} should succeed");
        }
        // The 256-byte FIFO is full.
        assert!(!uart.data_write(0xFF));
    }

    #[test]
    fn data_read_returns_none_when_empty() {
        let mut uart = Uart::new();
        assert_eq!(uart.data_read(), None);
    }

    #[test]
    fn reset_clears_state() {
        let mut uart = Uart::new();
        uart.reg_write(REG_IER, IER_ERBFI | IER_ETBEI);
        uart.reg_write(REG_SCR, 0xAB);
        uart.data_write(0x42);
        uart.reset();

        assert_eq!(uart.reg_read(REG_IER), 0);
        assert_eq!(uart.reg_read(REG_SCR), 0);
        assert_eq!(uart.reg_read(REG_IIR), IIR_NONE);
        assert!(!uart.intr_state());
        assert_eq!(uart.reg_read(REG_LSR), LSR_THRE | LSR_TEMT,);
    }
}
