// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ACPI GPE0 register block and the level-triggered SCI.
//!
//! A hotplug source raises a GPE bit here. The guest takes the SCI,
//! runs the matching `_Exx` method from the DSDT, and clears the bit.
//!
//! The SCI is a level-triggered line, so the pin must follow the
//! register state at all times. A pin that stays high with no status
//! bit set wedges IRQ 9 for the guest. A pin that stays low with a
//! status bit set loses the event. The GPE0 registers are the whole
//! level: no PM1 event source raises the SCI (see [`crate::acpi_pm`]).
//!
//! Layout follows the QEMU guest ABI, which the hotplug AML in the
//! DSDT is written against: a 4 byte block at 0xAFE0 holding a 2 byte
//! status register and a 2 byte enable register.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use slog;
use vmm_core::common::RWOp;
use vmm_core::intr_pins::IntrPin;
use vmm_core::pio::{PioBus, PioFn};

use crate::lifecycle::Lifecycle;
use crate::migrate::{
    MigrateCtx, MigrateSingle, MigrateStateError, Migrator, PayloadOffer,
    PayloadOutput, Schema, SchemaId,
};

/// Base I/O port of the GPE0 register block.
pub const GPE0_BLK_ADDR: u16 = 0xAFE0;

/// Length of the GPE0 register block in bytes.
///
/// The first half is GPE0_STS, the second half is GPE0_EN. ACPI 6.5,
/// section 4.8.4.1 requires an even length.
pub const GPE0_BLK_LEN: u8 = 4;

/// Offset of the first GPE0_EN byte in the block.
const GPE0_EN_OFFSET: u16 = GPE0_BLK_LEN as u16 / 2;

/// General purpose event bits this VMM raises.
///
/// The numbers select the `_Exx` method the guest runs, and match the
/// QEMU guest ABI so an unmodified guest driver works.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpeBit {
    /// PCI hotplug: runs `_E01`.
    Pci = 1,
    /// CPU hotplug: runs `_E02`.
    Cpu = 2,
    /// Memory hotplug: runs `_E03`.
    Memory = 3,
}

impl GpeBit {
    /// The bit's mask in GPE0_STS and GPE0_EN.
    pub const fn mask(self) -> u16 {
        1u16 << (self as u16)
    }
}

/// Bits the guest can enable, and status bits it can clear.
///
/// A bit outside this mask has no source and no `_Exx` method, so a
/// guest that sets it would park a level the VMM can never lower.
const GPE0_VALID: u16 =
    GpeBit::Pci.mask() | GpeBit::Cpu.mask() | GpeBit::Memory.mask();

/// Raises a general purpose event without knowing about the SCI.
///
/// The hotplug register files depend on this instead of on
/// [`AcpiGpe`], so they stay testable with no interrupt pin.
pub trait HotplugEventSink: Send + Sync {
    /// Set `bit` in GPE0_STS and update the SCI.
    fn raise(&self, bit: GpeBit);
}

/// Migration payload: the two GPE0 registers.
///
/// The SCI level is not carried. It follows from these registers, and
/// the destination has its own PIC, so the import recomputes the line
/// instead of trusting a level that was measured on the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpiGpeV1 {
    pub status: u16,
    pub enable: u16,
}

impl<'de> Schema<'de> for AcpiGpeV1 {
    fn id() -> SchemaId {
        ("acpi-gpe", 1)
    }
}

/// The GPE0 register block.
pub struct AcpiGpe {
    log: slog::Logger,
    sci: Arc<dyn IntrPin>,
    inner: Mutex<GpeInner>,
}

struct GpeInner {
    status: u16,
    enable: u16,
}

impl AcpiGpe {
    /// Create the GPE0 block and claim its I/O ports.
    ///
    /// `sci` is the IRQ 9 pin. The caller must keep the pin's parent
    /// PIC alive: a [`vmm_core::intr_pins::LegacyPin`] holds only a
    /// weak reference and goes silent once its PIC is dropped.
    pub fn create_and_attach(
        bus_pio: &PioBus,
        sci: Arc<dyn IntrPin>,
        log: slog::Logger,
    ) -> anyhow::Result<Arc<Self>> {
        let gpe = Arc::new(Self {
            log,
            sci,
            inner: Mutex::new(GpeInner {
                status: 0,
                enable: 0,
            }),
        });

        let handler_gpe = Arc::clone(&gpe);
        let handler: Arc<PioFn> =
            Arc::new(move |offset: u16, rwo: RWOp<'_>| {
                handler_gpe.handle(offset, rwo);
            });
        bus_pio
            .register(GPE0_BLK_ADDR, u16::from(GPE0_BLK_LEN), handler)
            .map_err(|e| {
                anyhow::anyhow!(
                    "acpi-gpe: cannot claim port {:#x}: {e}",
                    GPE0_BLK_ADDR
                )
            })?;

        Ok(gpe)
    }

    /// Drive the SCI pin from the full register state.
    ///
    /// The caller holds the lock across the pin update on purpose. Two
    /// vCPUs that compute a level and then apply it unordered can leave
    /// the line high with nothing set, or low with an event pending.
    fn recompute_sci(&self, inner: &GpeInner) {
        let level = (inner.status & inner.enable) != 0;
        self.sci.set_state(level);
    }

    /// Handle one access to the GPE0 block.
    fn handle(&self, offset: u16, rwo: RWOp<'_>) {
        // ACPI 6.5, section 4.8.4.1: a GPE block is accessed one byte
        // at a time. bhyve and QEMU both refuse a wider access, so a
        // guest never depends on it. Answer a refused read with the
        // floating bus value and drop the write, rather than apply
        // half of it. The offset cannot leave a 4 byte block, but the
        // shifts below depend on that, so it is checked here.
        let shift = match byte_shift(offset) {
            Some(shift) if rwo.len() == 1 => shift,
            _ => {
                if let RWOp::Read(ro) = rwo {
                    ro.write_u64(u64::MAX);
                }
                return;
            }
        };

        let mut inner = self.inner.lock().expect("gpe lock poisoned");

        if offset < GPE0_EN_OFFSET {
            match rwo {
                RWOp::Read(ro) => ro.write_u8((inner.status >> shift) as u8),
                RWOp::Write(wo) => {
                    // Write 1 to clear.
                    let clear = u16::from(wo.read_u8()) << shift;
                    inner.status &= !(clear & GPE0_VALID);
                    self.recompute_sci(&inner);
                }
            }
        } else {
            match rwo {
                RWOp::Read(ro) => ro.write_u8((inner.enable >> shift) as u8),
                RWOp::Write(wo) => {
                    let keep = !(0x00FFu16 << shift);
                    let written = u16::from(wo.read_u8()) << shift;
                    inner.enable =
                        ((inner.enable & keep) | written) & GPE0_VALID;
                    self.recompute_sci(&inner);
                }
            }
        }
    }

    /// The registers, for migration export.
    fn export_registers(&self) -> AcpiGpeV1 {
        let inner = self.inner.lock().expect("gpe lock poisoned");
        AcpiGpeV1 {
            status: inner.status,
            enable: inner.enable,
        }
    }

    /// Restore the registers, then drive the SCI from them.
    ///
    /// The mask repeats the guest write path. A bit with no source has
    /// no `_Exx` method, so a status bit outside the mask would park a
    /// level that the guest can never lower.
    fn import_registers(&self, state: AcpiGpeV1) {
        let mut inner = self.inner.lock().expect("gpe lock poisoned");
        inner.status = state.status & GPE0_VALID;
        inner.enable = state.enable & GPE0_VALID;
        self.recompute_sci(&inner);
    }

    /// GPE0_STS, for unit tests.
    #[cfg(test)]
    fn status(&self) -> u16 {
        self.inner.lock().expect("gpe lock poisoned").status
    }

    /// GPE0_EN, for unit tests.
    #[cfg(test)]
    fn enable(&self) -> u16 {
        self.inner.lock().expect("gpe lock poisoned").enable
    }
}

impl Lifecycle for AcpiGpe {
    fn type_name(&self) -> &'static str {
        "acpi-gpe"
    }

    /// Cold start: nothing latched and no source enabled.
    fn reset(&self) {
        let mut inner = self.inner.lock().expect("gpe lock poisoned");
        inner.status = 0;
        inner.enable = 0;
        self.recompute_sci(&inner);
    }

    fn migrate(&'_ self) -> Migrator<'_> {
        Migrator::Single(self)
    }
}

impl MigrateSingle for AcpiGpe {
    fn export(
        &self,
        _ctx: &MigrateCtx<'_>,
    ) -> Result<PayloadOutput, MigrateStateError> {
        Ok(self.export_registers().into())
    }

    fn import(
        &self,
        mut offer: PayloadOffer<'_>,
        _ctx: &MigrateCtx<'_>,
    ) -> Result<(), MigrateStateError> {
        // The SCI is re-asserted here when the restored registers ask
        // for it. A guest that migrated with an event pending would
        // otherwise wait for a line the destination PIC never raised.
        self.import_registers(offer.parse()?);
        Ok(())
    }
}

impl HotplugEventSink for AcpiGpe {
    fn raise(&self, bit: GpeBit) {
        let mut inner = self.inner.lock().expect("gpe lock poisoned");
        inner.status |= bit.mask() & GPE0_VALID;
        slog::debug!(self.log, "GPE raised";
            "bit" => ?bit, "enable" => inner.enable);
        self.recompute_sci(&inner);
    }
}

/// Bit position of the byte at `offset` within its 16 bit register.
///
/// Returns `None` for an offset outside the block, so a caller cannot
/// build a shift wider than the register.
fn byte_shift(offset: u16) -> Option<u32> {
    if offset >= u16::from(GPE0_BLK_LEN) {
        return None;
    }
    Some(u32::from(offset % GPE0_EN_OFFSET) * 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STS_LO: u16 = GPE0_BLK_ADDR;
    const STS_HI: u16 = GPE0_BLK_ADDR + 1;
    const EN_LO: u16 = GPE0_BLK_ADDR + 2;
    const EN_HI: u16 = GPE0_BLK_ADDR + 3;

    /// Records every level transition of the SCI line.
    ///
    /// The edge filter mirrors [`vmm_core::intr_pins::LegacyPin`], so
    /// the recorded history is the line state a guest would see, not
    /// the call count.
    struct RecordingPin {
        asserted: Mutex<bool>,
        history: Mutex<Vec<bool>>,
    }

    impl RecordingPin {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                asserted: Mutex::new(false),
                history: Mutex::new(Vec::new()),
            })
        }

        fn history(&self) -> Vec<bool> {
            self.history.lock().expect("history lock poisoned").clone()
        }

        fn edge(&self, level: bool) {
            let mut asserted = self.asserted.lock().expect("pin lock poisoned");
            if *asserted != level {
                *asserted = level;
                self.history
                    .lock()
                    .expect("history lock poisoned")
                    .push(level);
            }
        }
    }

    impl IntrPin for RecordingPin {
        fn assert(&self) {
            self.edge(true);
        }

        fn deassert(&self) {
            self.edge(false);
        }

        fn is_asserted(&self) -> bool {
            *self.asserted.lock().expect("pin lock poisoned")
        }
    }

    fn test_log() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    fn attached() -> (PioBus, Arc<AcpiGpe>, Arc<RecordingPin>) {
        let bus = PioBus::new();
        let pin = RecordingPin::new();
        let sci: Arc<dyn IntrPin> = pin.clone();
        let gpe = AcpiGpe::create_and_attach(&bus, sci, test_log())
            .expect("GPE0 block should attach");
        (bus, gpe, pin)
    }

    #[test]
    fn attach_fails_when_the_block_is_claimed() {
        let bus = PioBus::new();
        let stub: Arc<PioFn> = Arc::new(|_, _| {});
        bus.register(GPE0_BLK_ADDR + 1, 1, stub)
            .expect("stub handler should attach");
        let sci: Arc<dyn IntrPin> = RecordingPin::new();

        assert!(AcpiGpe::create_and_attach(&bus, sci, test_log()).is_err());
    }

    #[test]
    fn raising_an_enabled_bit_asserts() {
        let (bus, gpe, pin) = attached();
        bus.handle_out(EN_LO, 1, u32::from(GpeBit::Cpu.mask()));

        gpe.raise(GpeBit::Cpu);

        assert!(pin.is_asserted());
        assert_eq!(pin.history(), vec![true]);
        assert_eq!(bus.handle_in(STS_LO, 1), u32::from(GpeBit::Cpu.mask()));
    }

    #[test]
    fn raising_a_disabled_bit_does_not_assert() {
        let (bus, gpe, pin) = attached();

        gpe.raise(GpeBit::Cpu);

        assert!(!pin.is_asserted());
        assert!(pin.history().is_empty());
        // The event is latched even though the guest masked it.
        assert_eq!(bus.handle_in(STS_LO, 1), u32::from(GpeBit::Cpu.mask()));
    }

    #[test]
    fn enabling_a_bit_that_is_already_set_asserts() {
        let (bus, gpe, pin) = attached();
        gpe.raise(GpeBit::Pci);
        assert!(!pin.is_asserted());

        bus.handle_out(EN_LO, 1, u32::from(GpeBit::Pci.mask()));

        assert_eq!(pin.history(), vec![true]);
    }

    #[test]
    fn clearing_the_last_set_and_enabled_bit_deasserts() {
        let (bus, gpe, pin) = attached();
        bus.handle_out(EN_LO, 1, u32::from(GPE0_VALID));
        gpe.raise(GpeBit::Memory);

        bus.handle_out(STS_LO, 1, u32::from(GpeBit::Memory.mask()));

        assert!(!pin.is_asserted());
        assert_eq!(pin.history(), vec![true, false]);
        assert_eq!(gpe.status(), 0);
    }

    #[test]
    fn clearing_one_of_two_set_bits_keeps_the_sci_asserted() {
        let (bus, gpe, pin) = attached();
        bus.handle_out(EN_LO, 1, u32::from(GPE0_VALID));
        gpe.raise(GpeBit::Pci);
        gpe.raise(GpeBit::Cpu);

        bus.handle_out(STS_LO, 1, u32::from(GpeBit::Pci.mask()));

        assert!(pin.is_asserted());
        assert_eq!(gpe.status(), GpeBit::Cpu.mask());

        bus.handle_out(STS_LO, 1, u32::from(GpeBit::Cpu.mask()));

        assert_eq!(pin.history(), vec![true, false]);
    }

    #[test]
    fn disabling_a_bit_deasserts_while_the_status_stays_set() {
        let (bus, gpe, pin) = attached();
        bus.handle_out(EN_LO, 1, u32::from(GpeBit::Pci.mask()));
        gpe.raise(GpeBit::Pci);

        bus.handle_out(EN_LO, 1, 0);

        assert!(!pin.is_asserted());
        assert_eq!(pin.history(), vec![true, false]);
        assert_eq!(gpe.status(), GpeBit::Pci.mask());
    }

    #[test]
    fn writing_one_to_a_clear_status_bit_is_a_noop() {
        let (bus, gpe, pin) = attached();
        bus.handle_out(EN_LO, 1, u32::from(GPE0_VALID));
        gpe.raise(GpeBit::Pci);

        bus.handle_out(STS_LO, 1, u32::from(GpeBit::Cpu.mask()));

        assert_eq!(gpe.status(), GpeBit::Pci.mask());
        assert_eq!(pin.history(), vec![true]);
    }

    #[test]
    fn writing_zero_to_the_status_register_clears_nothing() {
        let (bus, gpe, pin) = attached();
        bus.handle_out(EN_LO, 1, u32::from(GPE0_VALID));
        gpe.raise(GpeBit::Pci);

        bus.handle_out(STS_LO, 1, 0);

        assert_eq!(gpe.status(), GpeBit::Pci.mask());
        assert!(pin.is_asserted());
    }

    #[test]
    fn reserved_enable_bits_are_dropped() {
        let (bus, _gpe, pin) = attached();

        bus.handle_out(EN_LO, 1, 0xFF);
        bus.handle_out(EN_HI, 1, 0xFF);

        assert_eq!(bus.handle_in(EN_LO, 1), u32::from(GPE0_VALID));
        assert_eq!(bus.handle_in(EN_HI, 1), 0);
        assert!(!pin.is_asserted());
    }

    #[test]
    fn reserved_status_bits_are_never_set() {
        let (bus, gpe, _pin) = attached();
        gpe.raise(GpeBit::Pci);
        gpe.raise(GpeBit::Cpu);
        gpe.raise(GpeBit::Memory);

        assert_eq!(gpe.status() & !GPE0_VALID, 0);
        assert_eq!(bus.handle_in(STS_HI, 1), 0);
    }

    #[test]
    fn the_high_bytes_address_the_upper_half_of_each_register() {
        let (bus, _gpe, _pin) = attached();

        bus.handle_out(EN_HI, 1, 0xFF);

        // Every valid bit is in the low byte, so the high byte cannot
        // change the enable register.
        assert_eq!(bus.handle_in(EN_LO, 1), 0);
        assert_eq!(bus.handle_in(EN_HI, 1), 0);
    }

    #[test]
    fn a_wide_access_is_refused() {
        let (bus, gpe, pin) = attached();
        bus.handle_out(EN_LO, 1, u32::from(GPE0_VALID));
        gpe.raise(GpeBit::Pci);

        assert_eq!(bus.handle_in(STS_LO, 2), 0xFFFF);
        assert_eq!(bus.handle_in(STS_LO, 4), 0xFFFF_FFFF);
        assert_eq!(bus.handle_in(EN_LO, 2), 0xFFFF);

        // A wide write must not clear the status or the enable.
        bus.handle_out(STS_LO, 2, u32::from(GPE0_VALID));
        bus.handle_out(EN_LO, 2, 0);

        assert_eq!(gpe.status(), GpeBit::Pci.mask());
        assert_eq!(bus.handle_in(EN_LO, 1), u32::from(GPE0_VALID));
        assert!(pin.is_asserted());
    }

    #[test]
    fn raising_the_same_bit_twice_makes_one_edge() {
        let (bus, gpe, pin) = attached();
        bus.handle_out(EN_LO, 1, u32::from(GPE0_VALID));

        gpe.raise(GpeBit::Pci);
        gpe.raise(GpeBit::Pci);

        assert_eq!(pin.history(), vec![true]);
        assert_eq!(gpe.status(), GpeBit::Pci.mask());
    }

    #[test]
    fn the_sci_line_history_is_exact() {
        let (bus, gpe, pin) = attached();

        bus.handle_out(EN_LO, 1, u32::from(GPE0_VALID));
        gpe.raise(GpeBit::Pci);
        gpe.raise(GpeBit::Cpu);
        bus.handle_out(STS_LO, 1, u32::from(GpeBit::Pci.mask()));
        bus.handle_out(STS_LO, 1, u32::from(GpeBit::Cpu.mask()));
        gpe.raise(GpeBit::Memory);
        bus.handle_out(EN_LO, 1, 0);

        assert_eq!(pin.history(), vec![true, false, true, false]);
    }

    /// Every reachable (status, enable) pair, in both orders.
    #[test]
    fn the_sci_follows_status_and_enable_for_every_combination() {
        const BITS: [GpeBit; 3] = [GpeBit::Pci, GpeBit::Cpu, GpeBit::Memory];

        for raised in 0..(1u16 << BITS.len()) {
            // The enable byte sweeps reserved bit 0 as well.
            for enable in 0..16u16 {
                for enable_first in [false, true] {
                    let (bus, gpe, pin) = attached();
                    let mut status = 0u16;

                    let mut raise_all = |gpe: &AcpiGpe| {
                        for (index, bit) in BITS.iter().enumerate() {
                            if raised & (1 << index) != 0 {
                                gpe.raise(*bit);
                                status |= bit.mask();
                            }
                        }
                    };

                    if enable_first {
                        bus.handle_out(EN_LO, 1, u32::from(enable));
                        raise_all(&gpe);
                    } else {
                        raise_all(&gpe);
                        bus.handle_out(EN_LO, 1, u32::from(enable));
                    }

                    let expected = (status & enable & GPE0_VALID) != 0;
                    assert_eq!(
                        pin.is_asserted(),
                        expected,
                        "raised {raised:#06x} enable {enable:#06x} \
                         enable_first {enable_first}",
                    );
                    assert_eq!(gpe.status(), status);
                }
            }
        }
    }

    #[test]
    fn the_block_covers_exactly_four_ports() {
        let (bus, _gpe, _pin) = attached();

        assert_eq!(bus.handle_in(GPE0_BLK_ADDR - 1, 1), 0xFF);
        for offset in 0..u16::from(GPE0_BLK_LEN) {
            assert_eq!(bus.handle_in(GPE0_BLK_ADDR + offset, 1), 0);
        }
        assert_eq!(
            bus.handle_in(GPE0_BLK_ADDR + u16::from(GPE0_BLK_LEN), 1),
            0xFF,
        );
    }

    /// Empty guest memory. The GPE0 block reads none of it, but the
    /// migration entry points still take a context.
    fn migrate_mem() -> vmm_core::MemCtx {
        vmm_core::MemCtx::new(Arc::new(vmm_core::PhysMap::new()))
    }

    fn migrator(gpe: &AcpiGpe) -> &dyn MigrateSingle {
        match gpe.migrate() {
            Migrator::Single(single) => single,
            _ => panic!("the GPE0 block exports one payload"),
        }
    }

    /// Move the registers the way the migration protocol does: export a
    /// payload, serialise it, and offer the bytes to a second block.
    fn migrate_registers(source: &AcpiGpe, dest: &AcpiGpe) {
        let mem = migrate_mem();
        let ctx = MigrateCtx { mem: &mem };

        let output = migrator(source).export(&ctx).expect("export the block");
        let bytes =
            serde_json::to_string(&output.payload).expect("serialise state");

        let mut de = serde_json::Deserializer::from_str(&bytes);
        let offer = PayloadOffer {
            kind: output.kind,
            version: output.version,
            payload: Box::new(<dyn erased_serde::Deserializer<'_>>::erase(
                &mut de,
            )),
        };
        migrator(dest)
            .import(offer, &ctx)
            .expect("import the block");
    }

    #[test]
    fn reset_clears_both_registers_and_deasserts() {
        let (bus, gpe, pin) = attached();
        bus.handle_out(EN_LO, 1, u32::from(GPE0_VALID));
        gpe.raise(GpeBit::Cpu);
        assert!(pin.is_asserted());

        gpe.reset();

        assert_eq!(gpe.status(), 0);
        assert_eq!(gpe.enable(), 0);
        assert_eq!(pin.history(), vec![true, false]);
    }

    #[test]
    fn export_then_import_round_trips_the_registers() {
        let (bus, gpe, _pin) = attached();
        bus.handle_out(EN_LO, 1, u32::from(GPE0_VALID));
        gpe.raise(GpeBit::Memory);
        let (_dest_bus, dest, _dest_pin) = attached();

        migrate_registers(&gpe, &dest);

        assert_eq!(dest.status(), GpeBit::Memory.mask());
        assert_eq!(dest.enable(), GPE0_VALID);
    }

    #[test]
    fn an_import_of_a_pending_enabled_event_asserts_the_sci() {
        let (bus, gpe, _pin) = attached();
        bus.handle_out(EN_LO, 1, u32::from(GpeBit::Pci.mask()));
        gpe.raise(GpeBit::Pci);
        let (_dest_bus, dest, dest_pin) = attached();

        migrate_registers(&gpe, &dest);

        // status 2 and enable 2: the destination must raise IRQ 9, or
        // the guest waits forever for the hotplug SCI.
        assert_eq!(dest.status(), GpeBit::Pci.mask());
        assert!(dest_pin.is_asserted());
        assert_eq!(dest_pin.history(), vec![true]);
    }

    #[test]
    fn an_import_of_a_masked_event_leaves_the_sci_deasserted() {
        let (_bus, gpe, _pin) = attached();
        gpe.raise(GpeBit::Pci);
        let (_dest_bus, dest, dest_pin) = attached();

        migrate_registers(&gpe, &dest);

        // status 2 and enable 0: latched, but the guest masked it.
        assert_eq!(dest.status(), GpeBit::Pci.mask());
        assert_eq!(dest.enable(), 0);
        assert!(!dest_pin.is_asserted());
        assert!(dest_pin.history().is_empty());
    }

    #[test]
    fn an_import_drops_bits_with_no_source() {
        let (_bus, dest, dest_pin) = attached();

        dest.import_registers(AcpiGpeV1 {
            status: u16::MAX,
            enable: u16::MAX,
        });

        // A bit outside the mask has no `_Exx` method, so a guest could
        // never clear the level it would park.
        assert_eq!(dest.status(), GPE0_VALID);
        assert_eq!(dest.enable(), GPE0_VALID);
        assert!(dest_pin.is_asserted());
    }

    #[test]
    fn an_import_of_the_wrong_payload_is_refused() {
        let (_bus, dest, _pin) = attached();
        let mem = migrate_mem();
        let ctx = MigrateCtx { mem: &mem };
        let mut de = serde_json::Deserializer::from_str("{}");
        let offer = PayloadOffer {
            kind: "acpi-gpe",
            version: AcpiGpeV1::id().1 + 1,
            payload: Box::new(<dyn erased_serde::Deserializer<'_>>::erase(
                &mut de,
            )),
        };

        let error = migrator(&dest)
            .import(offer, &ctx)
            .expect_err("a version this build cannot read must be refused");

        assert!(matches!(
            error,
            MigrateStateError::UnexpectedPayload(kind, _) if kind == "acpi-gpe"
        ));
    }

    #[test]
    fn byte_shift_rejects_an_offset_past_the_block() {
        assert_eq!(byte_shift(0), Some(0));
        assert_eq!(byte_shift(1), Some(8));
        assert_eq!(byte_shift(2), Some(0));
        assert_eq!(byte_shift(3), Some(8));
        assert_eq!(byte_shift(u16::from(GPE0_BLK_LEN)), None);
        assert_eq!(byte_shift(u16::MAX), None);
    }
}
