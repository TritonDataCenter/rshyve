// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ACPI Power Management register emulation.
//!
//! Implements the PM1 event, enable and control registers, plus the
//! reset register at 0xCF9. PM1_CNT SLP_TYP 5 with SLP_EN is the ACPI
//! S5 poweroff the guest asks for, and 0xCF9 is its reset.
//!
//! No event source raises a PM1 status bit: there is no fixed power or
//! sleep button, and the PM timer is the kernel's. The status register
//! therefore reads zero and the FADT says so. A poweroff from the
//! control plane goes through `VM_SUSPEND_POWEROFF`, not through an
//! SCI the guest would have to answer.

use std::sync::{Arc, Mutex};

use slog;
use vmm_core::common::RWOp;
use vmm_core::hdl::{SuspendHow, SuspendOutcome};
use vmm_core::pio::{PioBus, PioFn};

use crate::bhyve::pmtimer::PMBASE_DEFAULT as PMBASE;

/// SMI Command port for ACPI enable/disable.
const SMI_CMD_PORT: u16 = 0xB2;
/// Value written to SMI_CMD to enable ACPI (set SCI_EN).
const ACPI_ENABLE: u8 = 0xA0;
/// Value written to SMI_CMD to disable ACPI (clear SCI_EN).
const ACPI_DISABLE: u8 = 0xA1;

/// PM1 Status register bit definitions.
const PM1_BM_STS: u16 = 0x0010;
const PM1_PWRBTN_STS: u16 = 0x0100;
const PM1_SLPBTN_STS: u16 = 0x0200;
const PM1_RTC_STS: u16 = 0x0400;
const PM1_WAK_STS: u16 = 0x8000;

/// Clearable status bits (write-1-to-clear).
const PM1_STS_CLEARABLE: u16 =
    PM1_WAK_STS | PM1_RTC_STS | PM1_SLPBTN_STS | PM1_PWRBTN_STS | PM1_BM_STS;

/// PM1 Enable register bit definitions.
const PM1_TMR_EN: u16 = 0x0001;
const PM1_GBL_EN: u16 = 0x0020;
const PM1_PWRBTN_EN: u16 = 0x0100;
const PM1_RTC_EN: u16 = 0x0400;

/// Writable enable bits.
const PM1_EN_WRITABLE: u16 =
    PM1_RTC_EN | PM1_PWRBTN_EN | PM1_GBL_EN | PM1_TMR_EN;

/// PM1 Control register bit definitions.
const PM1_SCI_EN: u16 = 0x0001;
const PM1_SLP_TYP_MASK: u16 = 0x1C00;
const PM1_SLP_EN: u16 = 0x2000;
const PM1_ALWAYS_ZERO: u16 = 0xC003;

/// Sink for guest-initiated power transitions.
///
/// Indirected through a trait so the register emulation is unit-testable
/// without a live /dev/vmm handle.
pub trait SuspendSink: Send + Sync + 'static {
    fn suspend(&self, how: SuspendHow);
}

/// Suspend sink backed by a live vmm handle.
pub struct HdlSuspendSink {
    hdl: Arc<vmm_core::hdl::VmmHdl>,
    log: slog::Logger,
}

impl HdlSuspendSink {
    /// Create a suspend sink for the VM handle.
    pub fn new(
        hdl: Arc<vmm_core::hdl::VmmHdl>,
        log: slog::Logger,
    ) -> Arc<Self> {
        Arc::new(Self { hdl, log })
    }
}

impl SuspendSink for HdlSuspendSink {
    fn suspend(&self, how: SuspendHow) {
        // A device request is not attributable to a specific vCPU.
        match self.hdl.suspend(how, -1) {
            Ok(SuspendOutcome::Requested) => {}
            Ok(SuspendOutcome::AlreadyLatched) => {
                // Another vCPU may have already latched the transition.
                slog::debug!(self.log, "VM suspend already latched"; "how" => %how);
            }
            Err(e) => {
                slog::error!(self.log, "failed to suspend VM";
                    "how" => %how, "error" => %e);
            }
        }
    }
}

/// ACPI PM1 register state.
pub struct AcpiPm {
    log: slog::Logger,
    sink: Arc<dyn SuspendSink>,
    inner: Mutex<AcpiPmInner>,
}

struct AcpiPmInner {
    pm1_status: u16,
    pm1_enable: u16,
    pm1_control: u16,
    reset_control: u8,
}

impl AcpiPm {
    /// Create and attach the ACPI PM registers to the PIO bus.
    ///
    /// Claims the PM1 event (0x400, 4 bytes), control (0x404, 2 bytes),
    /// reset (0xCF9), and SMI command (0xB2) ports.
    pub fn create_and_attach(
        bus_pio: &PioBus,
        sink: Arc<dyn SuspendSink>,
        log: slog::Logger,
    ) -> anyhow::Result<Arc<Self>> {
        let pm = Arc::new(Self {
            log,
            sink,
            inner: Mutex::new(AcpiPmInner {
                pm1_status: 0,
                pm1_enable: 0,
                // SCI_EN starts clear. The OS sets it with an SMI_CMD
                // write.
                pm1_control: 0,
                reset_control: 0,
            }),
        });

        let pm_evt = Arc::clone(&pm);
        let evt_handler: Arc<PioFn> =
            Arc::new(move |offset: u16, rwo: RWOp<'_>| {
                pm_evt.handle_pm1_evt(offset, rwo);
            });
        bus_pio.register(PMBASE, 4, evt_handler).map_err(|e| {
            anyhow::anyhow!("acpi-pm: cannot claim port {:#x}: {e}", PMBASE)
        })?;

        let pm_cnt = Arc::clone(&pm);
        let cnt_handler: Arc<PioFn> =
            Arc::new(move |offset: u16, rwo: RWOp<'_>| {
                pm_cnt.handle_pm1_cnt(offset, rwo);
            });
        bus_pio.register(PMBASE + 4, 2, cnt_handler).map_err(|e| {
            anyhow::anyhow!("acpi-pm: cannot claim port {:#x}: {e}", PMBASE + 4)
        })?;

        let pm_rst = Arc::clone(&pm);
        let rst_handler: Arc<PioFn> =
            Arc::new(move |_offset: u16, rwo: RWOp<'_>| {
                pm_rst.handle_reset(rwo);
            });
        bus_pio.register(0xCF9, 1, rst_handler).map_err(|e| {
            anyhow::anyhow!("acpi-pm: cannot claim port {:#x}: {e}", 0xCF9)
        })?;

        let pm_smi = Arc::clone(&pm);
        let smi_handler: Arc<PioFn> =
            Arc::new(move |_offset: u16, rwo: RWOp<'_>| {
                pm_smi.handle_smi_cmd(rwo);
            });
        bus_pio
            .register(SMI_CMD_PORT, 1, smi_handler)
            .map_err(|e| {
                anyhow::anyhow!(
                    "acpi-pm: cannot claim port {:#x}: {e}",
                    SMI_CMD_PORT
                )
            })?;

        Ok(pm)
    }

    /// Handle PM1 Event block I/O (status at offset 0, enable at offset 2).
    fn handle_pm1_evt(&self, offset: u16, rwo: RWOp<'_>) {
        let mut inner = self.inner.lock().expect("pm lock poisoned");

        match (offset, rwo) {
            (0, RWOp::Read(ro)) => {
                ro.write_u16(inner.pm1_status);
            }
            // Write 1 to clear.
            (0, RWOp::Write(wo)) => {
                let val = wo.read_u16();
                inner.pm1_status &= !(val & PM1_STS_CLEARABLE);
            }
            (2, RWOp::Read(ro)) => {
                ro.write_u16(inner.pm1_enable);
            }
            (2, RWOp::Write(wo)) => {
                let val = wo.read_u16();
                inner.pm1_enable = val & PM1_EN_WRITABLE;
            }
            (_, RWOp::Read(ro)) => {
                // A byte access or an odd offset reads as zero.
                ro.write_u8(0);
            }
            _ => {
                // Other writes are dropped.
            }
        }
    }

    /// Handle PM1 Control block I/O.
    fn handle_pm1_cnt(&self, _offset: u16, rwo: RWOp<'_>) {
        let mut inner = self.inner.lock().expect("pm lock poisoned");

        match rwo {
            RWOp::Read(ro) => {
                ro.write_u16(inner.pm1_control);
            }
            RWOp::Write(wo) => {
                let val = wo.read_u16();
                let slp_typ = (val & PM1_SLP_TYP_MASK) >> 10;
                // OSPM cannot change SCI_EN. Reserved bits stay clear and
                // SLP_EN is not stored.
                inner.pm1_control = (inner.pm1_control & PM1_SCI_EN)
                    | (val & !(PM1_SLP_EN | PM1_ALWAYS_ZERO));

                if val & PM1_SLP_EN != 0 && slp_typ == 5 {
                    slog::info!(self.log, "guest requested ACPI S5 poweroff");
                    // Release the lock so the suspend ioctl does not block
                    // another vCPU on device state.
                    drop(inner);
                    self.sink.suspend(SuspendHow::PowerOff);
                }
            }
        }
    }

    /// Handle SMI Command register (0xB2) for ACPI enable/disable.
    fn handle_smi_cmd(&self, rwo: RWOp<'_>) {
        if let RWOp::Write(wo) = rwo {
            let val = wo.read_u8();
            let mut inner = self.inner.lock().expect("pm lock poisoned");
            match val {
                ACPI_ENABLE => {
                    inner.pm1_control |= PM1_SCI_EN;
                }
                ACPI_DISABLE => {
                    inner.pm1_control &= !PM1_SCI_EN;
                }
                _ => {
                    // Other SMI commands are dropped.
                }
            }
        }
    }

    /// Handle reset register I/O at 0xCF9.
    fn handle_reset(&self, rwo: RWOp<'_>) {
        let mut inner = self.inner.lock().expect("pm lock poisoned");

        match rwo {
            RWOp::Read(ro) => {
                ro.write_u8(inner.reset_control);
            }
            RWOp::Write(wo) => {
                let val = wo.read_u8();
                inner.reset_control = val;
                if val & 0x04 != 0 {
                    slog::info!(self.log, "guest requested reset via 0xCF9");
                    // Release the lock so the suspend ioctl does not block
                    // another vCPU on device state.
                    drop(inner);
                    self.sink.suspend(SuspendHow::Reset);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingSink(Mutex<Vec<SuspendHow>>);

    impl RecordingSink {
        fn new() -> Arc<Self> {
            Arc::new(Self(Mutex::new(Vec::new())))
        }

        fn recorded(&self) -> Vec<SuspendHow> {
            self.0.lock().expect("recording lock poisoned").clone()
        }
    }

    impl SuspendSink for RecordingSink {
        fn suspend(&self, how: SuspendHow) {
            self.0.lock().expect("recording lock poisoned").push(how);
        }
    }

    fn test_log() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    fn attached_bus() -> (PioBus, Arc<RecordingSink>) {
        let bus = PioBus::new();
        let sink = RecordingSink::new();
        let sink_arg: Arc<dyn SuspendSink> = sink.clone();
        AcpiPm::create_and_attach(&bus, sink_arg, test_log())
            .expect("ACPI PM registers should attach");
        (bus, sink)
    }

    #[test]
    fn s5_write_triggers_poweroff() {
        let (bus, sink) = attached_bus();

        bus.handle_out(PMBASE + 4, 2, (PM1_SLP_EN | (5 << 10)) as u32);

        assert_eq!(sink.recorded(), vec![SuspendHow::PowerOff]);
    }

    #[test]
    fn s1_write_is_inert() {
        let (bus, sink) = attached_bus();

        bus.handle_out(PMBASE + 4, 2, (PM1_SLP_EN | (1 << 10)) as u32);

        assert!(sink.recorded().is_empty());
    }

    #[test]
    fn slp_typ5_without_slp_en_is_inert() {
        let (bus, sink) = attached_bus();

        bus.handle_out(PMBASE + 4, 2, (5 << 10) as u32);

        assert!(sink.recorded().is_empty());
    }

    #[test]
    fn cf9_bit2_triggers_reset() {
        let (bus, sink) = attached_bus();

        bus.handle_out(0xCF9, 1, 0x06);

        assert_eq!(sink.recorded(), vec![SuspendHow::Reset]);
    }

    #[test]
    fn cf9_without_bit2_is_inert() {
        let (bus, sink) = attached_bus();

        bus.handle_out(0xCF9, 1, 0x02);

        assert!(sink.recorded().is_empty());
    }

    #[test]
    fn cf9_reads_back_last_write() {
        let (bus, _sink) = attached_bus();

        bus.handle_out(0xCF9, 1, 0x06);

        assert_eq!(bus.handle_in(0xCF9, 1), 6);
    }

    #[test]
    fn attach_fails_when_cf9_is_claimed() {
        let bus = PioBus::new();
        let handler: Arc<PioFn> = Arc::new(|_, _| {});
        bus.register(0xCF9, 1, handler)
            .expect("dummy reset handler should attach");
        let sink: Arc<dyn SuspendSink> = RecordingSink::new();

        let result = AcpiPm::create_and_attach(&bus, sink, test_log());

        assert!(result.is_err());
    }
}
