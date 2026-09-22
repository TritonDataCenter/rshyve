// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Portions derived from Propolis, Copyright Oxide Computer Company,
// used under MPL-2.0. Upstream: lib/propolis/src/intr_pins.rs
// https://github.com/oxidecomputer/propolis

//! Interrupt pin abstractions for virtual device IRQ delivery.
//!
//! Provides:
//! - [`IntrPin`] trait for abstract interrupt pin behavior
//! - [`LegacyPIC`] for ISA IRQ delivery through the kernel PIC/IOAPIC
//! - [`LegacyPin`] for individual IRQ lines on the legacy PIC
//! - [`NoOpPin`] for optional/unused interrupt signals

#![allow(clippy::mutex_atomic)]

use std::sync::{Arc, Mutex, Weak};

use slog;

use crate::hdl::VmmHdl;

/// Number of ISA IRQ lines (IRQ 0-15).
const PIN_COUNT: u8 = 16;

/// An abstract interrupt pin that can be asserted or deasserted.
///
/// Implementations deliver interrupts to the guest through
/// platform-specific mechanisms (e.g., kernel ioctls for PIC/IOAPIC).
pub trait IntrPin: Send + Sync + 'static {
    /// Assert (raise) the interrupt line.
    fn assert(&self);

    /// Deassert (lower) the interrupt line.
    fn deassert(&self);

    /// Returns whether the pin is currently asserted.
    fn is_asserted(&self) -> bool;

    /// Pulse the interrupt: assert then immediately deassert.
    ///
    /// Only pulses if the pin is not already asserted, to avoid
    /// spurious edges on a held-high line.
    fn pulse(&self) {
        if !self.is_asserted() {
            self.assert();
            self.deassert();
        }
    }

    /// Set the pin to a specific state.
    fn set_state(&self, is_asserted: bool) {
        if is_asserted {
            self.assert();
        } else {
            self.deassert();
        }
    }
}

/// Describes the operation to perform on an interrupt pin.
enum PinOp {
    Assert,
    Deassert,
    /// Assert then immediately deassert.
    Pulse,
}

/// Tracks the shared level count for a single IRQ line.
///
/// Multiple devices can share an IRQ. The level count tracks how many
/// sources are currently asserting so the kernel ioctl is only called
/// on actual transitions (0->1 for assert, 1->0 for deassert).
#[derive(Default, Copy, Clone)]
struct Entry {
    level: usize,
}

impl Entry {
    /// Process an interrupt operation and return whether the kernel
    /// should be notified (i.e., whether a level transition occurred).
    fn process_op(&mut self, op: &PinOp) -> bool {
        match op {
            PinOp::Assert => {
                self.level += 1;
                // Notify on 0->1 transition
                self.level == 1
            }
            PinOp::Deassert => {
                // Saturate at zero to avoid underflow from mismatched
                // assert/deassert pairs.
                if self.level == 0 {
                    return false;
                }
                self.level -= 1;
                // Notify on 1->0 transition
                self.level == 0
            }
            PinOp::Pulse => {
                // Only pulse if line is currently low
                self.level == 0
            }
        }
    }

    /// Put the level back where [`Self::process_op`] found it.
    ///
    /// A committed level the kernel never took is worse than no
    /// bookkeeping at all: a deassert whose ioctl failed leaves the
    /// kernel line high and this count at zero, so the next deassert
    /// sees no transition and never retries, and the line stays up for
    /// the life of the VM.
    fn undo_op(&mut self, op: &PinOp) {
        match op {
            PinOp::Assert => self.level -= 1,
            PinOp::Deassert => self.level += 1,
            // Pulse leaves the level alone.
            PinOp::Pulse => {}
        }
    }
}

/// Virtual legacy PIC/IOAPIC that delivers ISA IRQs to the kernel.
///
/// Wraps a [`VmmHdl`] and manages 16 IRQ lines (IRQ 0-15). Each line
/// tracks a shared level count so that multiple devices can share an
/// IRQ without spurious transitions.
pub struct LegacyPIC {
    inner: Mutex<[Entry; PIN_COUNT as usize]>,
    hdl: Arc<VmmHdl>,
    log: slog::Logger,
}

impl LegacyPIC {
    /// Create a new `LegacyPIC` backed by the given VM handle.
    pub fn new(hdl: Arc<VmmHdl>, log: slog::Logger) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new([Entry::default(); PIN_COUNT as usize]),
            hdl,
            log,
        })
    }

    /// Create a pin handle for the given ISA IRQ number.
    ///
    /// Returns `None` if the IRQ is out of range (>= 16) or is IRQ 2
    /// (which is reserved for the PIC cascade and cannot be used by
    /// devices).
    pub fn pin_handle(self: &Arc<Self>, irq: u8) -> Option<Arc<LegacyPin>> {
        if irq >= PIN_COUNT || irq == 2 {
            return None;
        }
        Some(Arc::new(LegacyPin::new(irq, Arc::downgrade(self))))
    }

    /// The pin for `irq`, or a [`NoOpPin`] for an IRQ the PIC cannot
    /// route, so a device with an optional line always has one.
    pub fn pin_or_noop(self: &Arc<Self>, irq: u8) -> Arc<dyn IntrPin> {
        match self.pin_handle(irq) {
            Some(pin) => pin,
            None => Arc::new(NoOpPin),
        }
    }

    /// Perform an IRQ operation, calling the kernel ioctl only on
    /// level transitions.
    ///
    /// Returns whether the caller's pin state change stands. A false
    /// return means the kernel refused the operation, so the caller
    /// must leave its own state where it was and let a later call try
    /// again.
    fn do_irq(&self, op: PinOp, irq: u8) -> bool {
        let mut pins = self.inner.lock().expect("LegacyPIC lock poisoned");

        if !pins[irq as usize].process_op(&op) {
            return true;
        }
        let irq_i32 = i32::from(irq);
        let result = match op {
            PinOp::Assert => self.hdl.isa_assert_irq(irq_i32, irq_i32),
            PinOp::Deassert => self.hdl.isa_deassert_irq(irq_i32, irq_i32),
            PinOp::Pulse => self.hdl.isa_pulse_irq(irq_i32, irq_i32),
        };
        if let Err(e) = result {
            // Log, do not panic: a failed delivery must not crash the
            // VMM. The undo below lets a later call try again.
            slog::error!(self.log, "failed to deliver IRQ"; "irq" => irq, "error" => %e);
            pins[irq as usize].undo_op(&op);
            return false;
        }
        true
    }
}

/// A single ISA IRQ line on a [`LegacyPIC`].
///
/// Tracks per-pin asserted state so that redundant assert/deassert
/// calls from a device don't generate unnecessary kernel ioctls.
/// Holds a [`Weak`] reference to the parent PIC so the PIC can be
/// dropped independently.
pub struct LegacyPin {
    irq: u8,
    asserted: Mutex<bool>,
    pic: Weak<LegacyPIC>,
}

impl LegacyPin {
    fn new(irq: u8, pic: Weak<LegacyPIC>) -> Self {
        Self {
            irq,
            asserted: Mutex::new(false),
            pic,
        }
    }
}

impl IntrPin for LegacyPin {
    fn assert(&self) {
        let mut asserted =
            self.asserted.lock().expect("LegacyPin lock poisoned");
        if !*asserted {
            *asserted = true;
            if let Some(pic) = self.pic.upgrade() {
                *asserted = pic.do_irq(PinOp::Assert, self.irq);
            }
        }
    }

    fn deassert(&self) {
        let mut asserted =
            self.asserted.lock().expect("LegacyPin lock poisoned");
        if *asserted {
            *asserted = false;
            if let Some(pic) = self.pic.upgrade() {
                *asserted = !pic.do_irq(PinOp::Deassert, self.irq);
            }
        }
    }

    fn pulse(&self) {
        let asserted = self.asserted.lock().expect("LegacyPin lock poisoned");
        if !*asserted {
            if let Some(pic) = self.pic.upgrade() {
                pic.do_irq(PinOp::Pulse, self.irq);
            }
        }
    }

    fn is_asserted(&self) -> bool {
        *self.asserted.lock().expect("LegacyPin lock poisoned")
    }
}

/// A no-op interrupt pin that silently discards all operations.
///
/// Useful for optional signals (e.g., power button, reset line) where
/// the interrupt is not wired to anything.
pub struct NoOpPin;

impl IntrPin for NoOpPin {
    fn assert(&self) {}
    fn deassert(&self) {}
    fn pulse(&self) {}
    fn is_asserted(&self) -> bool {
        false
    }
}

/// An interrupt pin backed by the IOAPIC for PCI GSI delivery.
///
/// Used for PCI devices whose interrupts are routed through the
/// IOAPIC (GSIs >= 16) rather than the legacy ISA PIC.
pub struct IoApicPin {
    irq: i32,
    asserted: Mutex<bool>,
    hdl: Arc<VmmHdl>,
    log: slog::Logger,
}

impl IoApicPin {
    /// Create a new IOAPIC pin for the given GSI.
    pub fn new(gsi: u8, hdl: Arc<VmmHdl>, log: slog::Logger) -> Arc<Self> {
        Arc::new(Self {
            irq: gsi as i32,
            asserted: Mutex::new(false),
            hdl,
            log,
        })
    }
}

impl IntrPin for IoApicPin {
    fn assert(&self) {
        let mut asserted =
            self.asserted.lock().expect("ioapic pin lock poisoned");
        if !*asserted {
            *asserted = true;
            if let Err(e) = self.hdl.ioapic_assert_irq(self.irq) {
                slog::warn!(self.log, "assert GSI failed"; "gsi" => self.irq, "error" => %e);
            }
        }
    }

    fn deassert(&self) {
        let mut asserted =
            self.asserted.lock().expect("ioapic pin lock poisoned");
        if *asserted {
            // A refused deassert leaves the kernel line high. Keeping
            // the pin asserted lets the next deassert retry it.
            match self.hdl.ioapic_deassert_irq(self.irq) {
                Ok(()) => *asserted = false,
                Err(e) => {
                    slog::warn!(self.log, "deassert GSI failed"; "gsi" => self.irq, "error" => %e);
                }
            }
        }
    }

    fn is_asserted(&self) -> bool {
        *self.asserted.lock().expect("ioapic pin lock poisoned")
    }
}

/// PIR register bit 7: disable routing.
const PIR_DISABLE: u8 = 0x80;
/// PIR register bits 3:0: ISA IRQ number.
const PIR_IRQ_MASK: u8 = 0x0F;
/// Number of PIR routing registers (PIRQ A-D).
const PIR_COUNT: usize = 4;

/// Number of GSIs that PCI INTx routing can reach.
const PCI_GSI_COUNT: usize = 8;
/// First GSI in the PCI INTx range.
const PCI_GSI_BASE: i32 = 16;

/// Shared PCI INTx routing state for one chipset.
///
/// INTx routing aliases 8 ways (`16 + (4 + slot + pin) % 8`), so devices
/// in different slots share a GSI. Like [`LegacyPIC`], each GSI keeps a
/// shared level count, so a deassert from one device does not lower a
/// line that another device still holds.
pub struct PciIntrRoutes {
    inner: Mutex<[Entry; PCI_GSI_COUNT]>,
    /// Shared PIR register state (from LPC bridge config space).
    pir_regs: Arc<Mutex<[u8; PIR_COUNT]>>,
    /// VM handle for kernel IRQ ioctls. `None` in unit tests, which have
    /// no kernel to deliver to.
    hdl: Option<Arc<VmmHdl>>,
    /// Logger for interrupt delivery errors.
    log: slog::Logger,
}

impl PciIntrRoutes {
    /// Create the routing state for a chipset.
    pub fn new(
        pir_regs: Arc<Mutex<[u8; PIR_COUNT]>>,
        hdl: Arc<VmmHdl>,
        log: slog::Logger,
    ) -> Arc<Self> {
        Self::build(pir_regs, Some(hdl), log)
    }

    fn build(
        pir_regs: Arc<Mutex<[u8; PIR_COUNT]>>,
        hdl: Option<Arc<VmmHdl>>,
        log: slog::Logger,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new([Entry::default(); PCI_GSI_COUNT]),
            pir_regs,
            hdl,
            log,
        })
    }

    /// Create a pin handle for a device INTx pin.
    ///
    /// # Arguments
    /// - `slot` - PCI device number (0-31)
    /// - `pin` - INTx pin (0=INTA, 1=INTB, 2=INTC, 3=INTD)
    pub fn pin_handle(self: &Arc<Self>, slot: u8, pin: u8) -> Arc<PciIntrPin> {
        Arc::new(PciIntrPin::new(slot, pin, Arc::downgrade(self)))
    }

    /// Read the current ATPIC IRQ from a PIR register.
    ///
    /// Returns `Some(irq)` if the route is enabled, `None` if disabled.
    fn atpic_irq(&self, pirq_idx: usize) -> Option<i32> {
        let pir = self.pir_regs.lock().expect("pir lock");
        let val = pir[pirq_idx];
        if val & PIR_DISABLE != 0 {
            return None;
        }
        let irq = (val & PIR_IRQ_MASK) as i32;
        if irq > 0 {
            Some(irq)
        } else {
            None
        }
    }

    /// Perform a PCI INTx operation, calling the kernel ioctl only on
    /// level transitions of the shared GSI.
    ///
    /// Returns whether the caller's pin state change stands. A false
    /// return means the kernel refused the operation, so the caller
    /// must leave its own state where it was and let a later call try
    /// again.
    fn do_irq(&self, op: PinOp, pirq_idx: usize, line: usize) -> bool {
        let mut lines = self.inner.lock().expect("PciIntrRoutes lock poisoned");
        if !lines[line].process_op(&op) {
            return true;
        }
        let Some(hdl) = self.hdl.as_ref() else {
            return true;
        };

        let ioapic_irq = PCI_GSI_BASE + line as i32;
        // The PIR register is read at delivery time so that firmware can
        // reprogram routing without invalidating existing pin handles.
        // A disabled route leaves the IOAPIC pin, which the guest kernel
        // programs from the ACPI _PRT regardless of PIR.
        let atpic_irq = self.atpic_irq(pirq_idx);
        let result = match (&op, atpic_irq) {
            (PinOp::Assert, Some(atpic)) => {
                hdl.isa_assert_irq(atpic, ioapic_irq)
            }
            (PinOp::Deassert, Some(atpic)) => {
                hdl.isa_deassert_irq(atpic, ioapic_irq)
            }
            (PinOp::Pulse, Some(atpic)) => hdl.isa_pulse_irq(atpic, ioapic_irq),
            (PinOp::Assert, None) => hdl.ioapic_assert_irq(ioapic_irq),
            (PinOp::Deassert, None) => hdl.ioapic_deassert_irq(ioapic_irq),
            // The IOAPIC has no pulse ioctl, so raise and lower the pin.
            (PinOp::Pulse, None) => hdl
                .ioapic_assert_irq(ioapic_irq)
                .and_then(|()| hdl.ioapic_deassert_irq(ioapic_irq)),
        };
        if let Err(e) = result {
            slog::error!(self.log, "failed to deliver PCI INTx";
                "pirq_idx" => pirq_idx,
                "atpic_irq" => atpic_irq,
                "ioapic_irq" => ioapic_irq,
                "error" => %e,
            );
            lines[line].undo_op(&op);
            return false;
        }
        true
    }

    /// Shared level count of one GSI, for unit tests.
    #[cfg(test)]
    fn line_level(&self, line: usize) -> usize {
        self.inner.lock().expect("PciIntrRoutes lock poisoned")[line].level
    }
}

/// PCI interrupt pin routed through the i440fx/PIIX3 PIRQ mechanism.
///
/// Matches C bhyve's `pci_irq_assert()` behavior: calls
/// `vm_isa_assert_irq(atpic_irq, ioapic_irq)` where:
/// - `atpic_irq` is read dynamically from the PIR register (programmed
///   by firmware at config offsets 0x60-0x63 of the LPC bridge)
/// - `ioapic_irq` is fixed per device slot: `16 + (4 + slot + pin) % 8`
///
/// Tracks per-pin asserted state. The shared level count of the GSI
/// lives in the parent [`PciIntrRoutes`], because slots 8 apart alias
/// onto the same GSI. Holds a [`Weak`] reference to the parent so the
/// chipset can be dropped independently.
pub struct PciIntrPin {
    /// Index into PIR registers (0-3), from `(slot + pin) % 4`.
    pirq_idx: usize,
    /// Index into the shared GSI table, from `(4 + slot + pin) % 8`.
    line: usize,
    /// Per-pin asserted state.
    asserted: Mutex<bool>,
    routes: Weak<PciIntrRoutes>,
}

impl PciIntrPin {
    fn new(slot: u8, pin: u8, routes: Weak<PciIntrRoutes>) -> Self {
        let pirq_idx = ((slot as usize) + (pin as usize)) % PIR_COUNT;
        let line = (4 + slot as usize + pin as usize) % PCI_GSI_COUNT;
        Self {
            pirq_idx,
            line,
            asserted: Mutex::new(false),
            routes,
        }
    }
}

impl IntrPin for PciIntrPin {
    fn assert(&self) {
        let mut asserted = self.asserted.lock().expect("pci pin lock");
        if !*asserted {
            *asserted = true;
            if let Some(routes) = self.routes.upgrade() {
                *asserted =
                    routes.do_irq(PinOp::Assert, self.pirq_idx, self.line);
            }
        }
    }

    fn deassert(&self) {
        let mut asserted = self.asserted.lock().expect("pci pin lock");
        if *asserted {
            *asserted = false;
            if let Some(routes) = self.routes.upgrade() {
                *asserted =
                    !routes.do_irq(PinOp::Deassert, self.pirq_idx, self.line);
            }
        }
    }

    fn pulse(&self) {
        let asserted = self.asserted.lock().expect("pci pin lock");
        if !*asserted {
            if let Some(routes) = self.routes.upgrade() {
                routes.do_irq(PinOp::Pulse, self.pirq_idx, self.line);
            }
        }
    }

    fn is_asserted(&self) -> bool {
        *self.asserted.lock().expect("pci pin lock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Routes with no VM handle: only the shared level count runs.
    fn test_routes() -> Arc<PciIntrRoutes> {
        PciIntrRoutes::build(
            Arc::new(Mutex::new([PIR_DISABLE; PIR_COUNT])),
            None,
            slog::Logger::root(slog::Discard, slog::o!()),
        )
    }

    // A level the kernel refused must not stay committed. Committed,
    // a failed deassert leaves the line high in the kernel and the
    // count at zero, so no later deassert sees a transition and the
    // line never comes down again.
    #[test]
    fn a_refused_operation_leaves_the_level_where_it_was() {
        let mut entry = Entry::default();

        assert!(entry.process_op(&PinOp::Assert));
        entry.undo_op(&PinOp::Assert);
        assert_eq!(entry.level, 0, "a refused assert stayed on the count");
        // The retry has a transition to report, so the kernel is asked
        // again.
        assert!(entry.process_op(&PinOp::Assert));

        assert!(entry.process_op(&PinOp::Deassert));
        entry.undo_op(&PinOp::Deassert);
        assert_eq!(entry.level, 1, "a refused deassert left the line low");
        assert!(
            entry.process_op(&PinOp::Deassert),
            "a refused deassert is never retried"
        );
    }

    #[test]
    fn a_single_pin_tracks_one_level() {
        let routes = test_routes();
        let pin = routes.pin_handle(3, 0);
        assert!(!pin.is_asserted());

        pin.assert();
        pin.assert();
        assert!(pin.is_asserted());
        assert_eq!(routes.line_level(pin.line), 1);

        pin.deassert();
        pin.deassert();
        assert!(!pin.is_asserted());
        assert_eq!(routes.line_level(pin.line), 0);
    }

    #[test]
    fn deassert_without_assert_leaves_the_line_low() {
        let routes = test_routes();
        let pin = routes.pin_handle(5, 0);
        pin.deassert();
        assert_eq!(routes.line_level(pin.line), 0);
    }

    #[test]
    fn pins_on_the_same_gsi_share_the_line() {
        let routes = test_routes();
        // Slots 8 apart alias onto one GSI.
        let a = routes.pin_handle(2, 0);
        let b = routes.pin_handle(10, 0);
        assert_eq!(a.line, b.line);

        a.assert();
        b.assert();
        assert_eq!(routes.line_level(a.line), 2);

        // One device deasserts, the other still holds the line.
        a.deassert();
        assert!(!a.is_asserted());
        assert!(b.is_asserted());
        assert_eq!(routes.line_level(a.line), 1);

        b.deassert();
        assert_eq!(routes.line_level(a.line), 0);
    }

    #[test]
    fn pins_on_different_gsis_are_independent() {
        let routes = test_routes();
        let a = routes.pin_handle(2, 0);
        let b = routes.pin_handle(3, 0);
        assert_ne!(a.line, b.line);

        a.assert();
        assert_eq!(routes.line_level(a.line), 1);
        assert_eq!(routes.line_level(b.line), 0);
    }
}
