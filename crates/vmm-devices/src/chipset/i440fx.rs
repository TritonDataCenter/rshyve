// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! i440FX chipset emulation.
//!
//! A minimal PCI topology: the host bridge (Intel 82441FX / PIIX4) at
//! slot 0, the ISA/LPC bridge (Intel PIIX3) at slot 1, the POST code
//! port (0x80), and the 0xCF8/0xCFC config access ports.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use slog;
use vmm_core::common::RWOp;
use vmm_core::hdl::VmmHdl;
use vmm_core::intr_pins::PciIntrRoutes;
use vmm_core::pio::{PioBus, PioFn};

use crate::pci::bits;
use crate::pci::bus::{AttachError, PciBus};
use crate::pci::device::{DeviceIdent, DeviceState, PciDevice};
use crate::pci::{BarN, Bdf, PioCfgDecoder};

const VENDOR_INTEL: u16 = 0x8086;

const PIIX4_HB_DEV_ID: u16 = 0x1237;

const PIIX3_ISA_DEV_ID: u16 = 0x7000;

const PORT_POST_CODE: u16 = 0x80;
const LEN_POST_CODE: u16 = 1;

/// The i440FX chipset. It owns the PCI bus and the PIO config decoder.
/// Other devices attach through [`attach_device`](Self::attach_device).
pub struct I440FxChipset {
    pci_bus: Arc<PciBus>,
    cfg_decoder: PioCfgDecoder,
    post_code: AtomicU8,
    /// Shared PCI INTx routing state. `None` without a VM handle, which
    /// leaves [`route_lintr`](Self::route_lintr) with nothing to route to.
    intr_routes: Option<Arc<PciIntrRoutes>>,
}

impl I440FxChipset {
    /// Create the chipset, attach both bridges and register the PIO
    /// handlers.
    pub fn create(
        bus_pio: &PioBus,
        hdl: Option<Arc<VmmHdl>>,
        log: slog::Logger,
    ) -> Arc<Self> {
        let pci_bus = PciBus::new();

        let hostbridge: Arc<dyn PciDevice> = Arc::new(Hostbridge::new());
        let bdf_hb = Bdf::new_unchecked(0, 0, 0);
        pci_bus.attach(bdf_hb, hostbridge);

        // Firmware writes the PIR registers through LPC bridge config
        // space (0x60-0x63). PciIntrRoutes reads them at delivery time.
        let pir_regs = Arc::new(Mutex::new([0x80u8; PIR_COUNT]));

        let lpc_pci: Arc<dyn PciDevice> =
            Arc::new(LpcBridge::new(pir_regs.clone()));
        let bdf_lpc = Bdf::new_unchecked(0, 1, 0);
        pci_bus.attach(bdf_lpc, lpc_pci);

        let intr_routes = hdl.map(|hdl| {
            PciIntrRoutes::new(
                pir_regs.clone(),
                hdl,
                log.new(slog::o!("component" => "pci-intr")),
            )
        });

        let chipset = Arc::new(Self {
            pci_bus,
            cfg_decoder: PioCfgDecoder::new(),
            post_code: AtomicU8::new(0),
            intr_routes,
        });

        // Only port 0xCF8 is claimed. Dword accesses still reach this
        // handler.
        let cs = Arc::clone(&chipset);
        let addr_handler: Arc<PioFn> =
            Arc::new(move |offset: u16, rwo: RWOp<'_>| {
                let port = bits::PORT_PCI_CONFIG_ADDR + offset;
                cs.cfg_decoder.service_addr(port, rwo);
            });
        bus_pio
            .register(
                bits::PORT_PCI_CONFIG_ADDR,
                bits::LEN_PCI_CONFIG_ADDR,
                addr_handler,
            )
            .expect("failed to register PCI config address port");

        let cs = Arc::clone(&chipset);
        let data_handler: Arc<PioFn> =
            Arc::new(move |offset: u16, rwo: RWOp<'_>| {
                let port = bits::PORT_PCI_CONFIG_DATA + offset;
                cs.pio_cfg_data_handler(port, rwo);
            });
        bus_pio
            .register(
                bits::PORT_PCI_CONFIG_DATA,
                bits::LEN_PCI_CONFIG_DATA,
                data_handler,
            )
            .expect("failed to register PCI config data port");

        let cs = Arc::clone(&chipset);
        let post_handler: Arc<PioFn> =
            Arc::new(move |_port: u16, rwo: RWOp<'_>| {
                cs.pio_post_handler(rwo);
            });
        bus_pio
            .register(PORT_POST_CODE, LEN_POST_CODE, post_handler)
            .expect("failed to register POST code port");

        chipset
    }

    pub fn pci_bus(&self) -> &Arc<PciBus> {
        &self.pci_bus
    }

    /// # Panics
    ///
    /// Panics if the BDF is occupied. Use
    /// [`try_attach_device`](Self::try_attach_device) when the BDF comes
    /// from a request at run time.
    pub fn attach_device(&self, bdf: Bdf, dev: Arc<dyn PciDevice>) {
        self.pci_bus.attach(bdf, dev);
    }

    /// Attach a device, or return an error if the BDF is occupied.
    pub fn try_attach_device(
        &self,
        bdf: Bdf,
        dev: Arc<dyn PciDevice>,
    ) -> Result<(), AttachError> {
        self.pci_bus.try_attach(bdf, dev)
    }

    /// Remove the device at `bdf` from the PCI bus and return it.
    pub fn detach_device(&self, bdf: &Bdf) -> Option<Arc<dyn PciDevice>> {
        self.pci_bus.detach(bdf)
    }

    /// A PCI interrupt pin for the device at `bdf`. It calls
    /// `vm_isa_assert_irq` with:
    /// - ATPIC IRQ: read from PIR[`(slot+pin)%4`] at delivery time
    /// - IOAPIC IRQ: fixed at `16 + (4 + slot + pin) % 8`
    ///
    /// This matches C bhyve's `pci_irq_assert()` / `ioapic_pci_alloc_irq()`.
    pub fn route_lintr(
        &self,
        bdf: &Bdf,
    ) -> Option<(crate::pci::INTxPinID, Arc<dyn vmm_core::intr_pins::IntrPin>)>
    {
        let routes = self.intr_routes.as_ref()?;
        let pin = routes.pin_handle(bdf.dev(), 0); // INT#A
        Some((crate::pci::INTxPinID::IntA, pin))
    }

    /// The last POST code the guest wrote.
    pub fn post_code(&self) -> u8 {
        self.post_code.load(Ordering::Relaxed)
    }

    /// PIO handler for 0xCFC-0xCFF (config data).
    fn pio_cfg_data_handler(&self, port: u16, rwo: RWOp<'_>) {
        let bus = &self.pci_bus;
        self.cfg_decoder.service_data(
            port,
            rwo,
            |bdf, offset, rwo| match rwo {
                RWOp::Read(ro) => {
                    let val = bus.config_read(bdf, offset, ro.len() as u8);
                    ro.write_dword(val)
                }
                RWOp::Write(wo) => {
                    bus.config_write(
                        bdf,
                        offset,
                        wo.len() as u8,
                        wo.read_u32(),
                    );
                }
            },
        );
    }

    /// PIO handler for POST code port (0x80).
    fn pio_post_handler(&self, rwo: RWOp<'_>) {
        match rwo {
            RWOp::Read(ro) => {
                ro.write_u8(self.post_code.load(Ordering::Relaxed));
            }
            RWOp::Write(wo) => {
                self.post_code.store(wo.read_u8(), Ordering::Relaxed);
            }
        }
    }
}

impl std::fmt::Debug for I440FxChipset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("I440FxChipset").finish_non_exhaustive()
    }
}

/// Host bridge at slot 0. It has the Intel i440FX identity and no BARs.
struct Hostbridge {
    state: Mutex<DeviceState>,
}

impl Hostbridge {
    fn new() -> Self {
        Self {
            state: Mutex::new(DeviceState::new(DeviceIdent {
                vendor_id: VENDOR_INTEL,
                device_id: PIIX4_HB_DEV_ID,
                class: bits::CLASS_BRIDGE,
                subclass: bits::SUBCLASS_BRIDGE_HOST,
                prog_if: 0,
                revision: 0,
                sub_vendor_id: 0,
                sub_device_id: 0,
            })),
        }
    }
}

impl PciDevice for Hostbridge {
    fn cfg_read(&self, offset: u8, len: u8) -> u32 {
        self.state
            .lock()
            .expect("Hostbridge lock poisoned")
            .cfg_read(offset, len)
    }

    fn cfg_write(&self, offset: u8, len: u8, val: u32) {
        self.state
            .lock()
            .expect("Hostbridge lock poisoned")
            .cfg_write(offset, len, val);
    }

    fn bar_rw(&self, _bar: BarN, _offset: usize, _rwo: RWOp<'_>) {}
}

/// PCI Interrupt Routing registers in the LPC config space. In each
/// register, bit 7 disables the link and bits 3:0 hold the ISA IRQ.
const PIR_OFFSET: u8 = 0x60;
const PIR_COUNT: usize = 4;

/// ISA/LPC bridge at slot 1 (Intel PIIX3).
///
/// Firmware writes the PIR registers (0x60-0x63) to map PIRQ links A-D
/// to ISA IRQs. `PciIntrRoutes` reads them at delivery time, so a PIR
/// update needs no callback.
struct LpcBridge {
    state: Mutex<DeviceState>,
    /// Shared with `PciIntrRoutes`.
    pir_regs: Arc<Mutex<[u8; PIR_COUNT]>>,
}

impl LpcBridge {
    fn new(pir_regs: Arc<Mutex<[u8; PIR_COUNT]>>) -> Self {
        Self {
            state: Mutex::new(DeviceState::new(DeviceIdent {
                vendor_id: VENDOR_INTEL,
                device_id: PIIX3_ISA_DEV_ID,
                class: bits::CLASS_BRIDGE,
                subclass: bits::SUBCLASS_BRIDGE_ISA,
                prog_if: 0,
                revision: 0,
                sub_vendor_id: 0,
                sub_device_id: 0,
            })),
            pir_regs,
        }
    }
}

impl PciDevice for LpcBridge {
    fn cfg_read(&self, offset: u8, len: u8) -> u32 {
        if offset >= PIR_OFFSET && offset < PIR_OFFSET + PIR_COUNT as u8 {
            let pir = self.pir_regs.lock().expect("pir lock");
            let idx = (offset - PIR_OFFSET) as usize;
            return u32::from(pir[idx]);
        }
        self.state
            .lock()
            .expect("LpcBridge lock poisoned")
            .cfg_read(offset, len)
    }

    fn cfg_write(&self, offset: u8, len: u8, val: u32) {
        // PciIntrPin reads the PIR value at delivery time.
        if offset >= PIR_OFFSET
            && offset < PIR_OFFSET + PIR_COUNT as u8
            && len == 1
        {
            let idx = (offset - PIR_OFFSET) as usize;
            let byte = val as u8;
            let mut pir = self.pir_regs.lock().expect("pir lock");
            pir[idx] = byte;
            return;
        }
        self.state
            .lock()
            .expect("LpcBridge lock poisoned")
            .cfg_write(offset, len, val);
    }

    fn bar_rw(&self, _bar: BarN, _offset: usize, _rwo: RWOp<'_>) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Create a chipset on a fresh PIO bus for testing.
    fn setup() -> (Arc<PioBus>, Arc<I440FxChipset>) {
        let pio = Arc::new(PioBus::new());
        let chipset = I440FxChipset::create(
            &pio,
            None,
            slog::Logger::root(slog::Discard, slog::o!()),
        );
        (pio, chipset)
    }

    /// Helper: write a config address, then read 4 bytes of config data.
    fn cfg_read_dword(
        pio: &PioBus,
        bus: u8,
        dev: u8,
        func: u8,
        reg: u8,
    ) -> u32 {
        let addr: u32 = 0x8000_0000
            | (u32::from(bus) << 16)
            | (u32::from(dev) << 11)
            | (u32::from(func) << 8)
            | (u32::from(reg) & 0xFC);
        pio.handle_out(0xCF8, 4, addr);
        pio.handle_in(0xCFC, 4)
    }

    /// Helper: write a config address, then write 4 bytes of config data.
    fn cfg_write_dword(
        pio: &PioBus,
        bus: u8,
        dev: u8,
        func: u8,
        reg: u8,
        val: u32,
    ) {
        let addr: u32 = 0x8000_0000
            | (u32::from(bus) << 16)
            | (u32::from(dev) << 11)
            | (u32::from(func) << 8)
            | (u32::from(reg) & 0xFC);
        pio.handle_out(0xCF8, 4, addr);
        pio.handle_out(0xCFC, 4, val);
    }

    /// A device that answers config reads from a plain [`DeviceState`].
    struct DummyDev(Mutex<DeviceState>);

    impl PciDevice for DummyDev {
        fn cfg_read(&self, offset: u8, len: u8) -> u32 {
            self.0
                .lock()
                .expect("dummy lock poisoned")
                .cfg_read(offset, len)
        }
        fn cfg_write(&self, offset: u8, len: u8, val: u32) {
            self.0
                .lock()
                .expect("dummy lock poisoned")
                .cfg_write(offset, len, val);
        }
        fn bar_rw(&self, _: BarN, _: usize, _: RWOp<'_>) {}
    }

    fn dummy_dev(vendor_id: u16, device_id: u16) -> Arc<dyn PciDevice> {
        Arc::new(DummyDev(Mutex::new(DeviceState::new(DeviceIdent {
            vendor_id,
            device_id,
            class: bits::CLASS_NETWORK,
            subclass: 0,
            ..Default::default()
        }))))
    }

    #[test]
    fn hostbridge_vendor_device_id() {
        let (pio, _cs) = setup();
        let val = cfg_read_dword(&pio, 0, 0, 0, 0x00);
        assert_eq!(val & 0xFFFF, VENDOR_INTEL as u32);
        assert_eq!((val >> 16) & 0xFFFF, PIIX4_HB_DEV_ID as u32);
    }

    #[test]
    fn hostbridge_class_code() {
        let (pio, _cs) = setup();
        let val = cfg_read_dword(&pio, 0, 0, 0, 0x08);
        let class = (val >> 24) & 0xFF;
        let subclass = (val >> 16) & 0xFF;
        assert_eq!(class, bits::CLASS_BRIDGE as u32);
        assert_eq!(subclass, bits::SUBCLASS_BRIDGE_HOST as u32);
    }

    #[test]
    fn lpc_bridge_vendor_device_id() {
        let (pio, _cs) = setup();
        let val = cfg_read_dword(&pio, 0, 1, 0, 0x00);
        assert_eq!(val & 0xFFFF, VENDOR_INTEL as u32);
        assert_eq!((val >> 16) & 0xFFFF, PIIX3_ISA_DEV_ID as u32);
    }

    #[test]
    fn lpc_bridge_class_code() {
        let (pio, _cs) = setup();
        let val = cfg_read_dword(&pio, 0, 1, 0, 0x08);
        let class = (val >> 24) & 0xFF;
        let subclass = (val >> 16) & 0xFF;
        assert_eq!(class, bits::CLASS_BRIDGE as u32);
        assert_eq!(subclass, bits::SUBCLASS_BRIDGE_ISA as u32);
    }

    #[test]
    fn empty_slot_returns_all_ff() {
        let (pio, _cs) = setup();
        let val = cfg_read_dword(&pio, 0, 2, 0, 0x00);
        assert_eq!(val, 0xFFFF_FFFF);
    }

    #[test]
    fn all_empty_slots_return_ff() {
        let (pio, _cs) = setup();
        for dev in 2..32u8 {
            let val = cfg_read_dword(&pio, 0, dev, 0, 0x00);
            assert_eq!(val, 0xFFFF_FFFF, "expected bus float at slot {dev}");
        }
    }

    /// The guest picks the bus in a config address, and an illumos
    /// guest walks all 256. Only bus 0 exists, so every other bus must
    /// float. If not, the guest finds a copy of bus 0 behind each one,
    /// builds a root bus node per bus, and the 27th node decodes to an
    /// inw of port 0x20.
    ///
    /// The test drives 0xCF8/0xCFC rather than PciBus, so it still
    /// holds if the check moves.
    #[test]
    fn no_bus_but_zero_answers_a_config_read() {
        let (pio, _cs) = setup();
        // Device 0 is the hostbridge, so bus 0 answers here.
        assert_ne!(cfg_read_dword(&pio, 0, 0, 0, 0x00), 0xFFFF_FFFF);

        for bus in 1..=u8::MAX {
            assert_eq!(
                cfg_read_dword(&pio, bus, 0, 0, 0x00),
                0xFFFF_FFFF,
                "bus {bus} answered with the devices of bus 0",
            );
        }
    }

    #[test]
    fn config_addr_disabled_returns_ff() {
        let (pio, _cs) = setup();
        pio.handle_out(0xCF8, 4, 0x0000_0000);
        let val = pio.handle_in(0xCFC, 4);
        assert_eq!(val, 0xFFFF_FFFF);
    }

    #[test]
    fn command_register_writable() {
        let (pio, _cs) = setup();
        // IO_EN | MMIO_EN
        cfg_write_dword(&pio, 0, 0, 0, 0x04, 0x0003);
        let val = cfg_read_dword(&pio, 0, 0, 0, 0x04);
        assert_eq!(val & 0x03, 0x03);
    }

    #[test]
    fn post_code_write_read() {
        let (pio, cs) = setup();
        pio.handle_out(0x80, 1, 0xAB);
        assert_eq!(cs.post_code(), 0xAB);
        assert_eq!(pio.handle_in(0x80, 1), 0xAB);
    }

    #[test]
    fn attach_additional_device() {
        let (pio, cs) = setup();

        let bdf = Bdf::new_unchecked(0, 3, 0);
        cs.attach_device(bdf, dummy_dev(0x1AF4, 0x1000));

        let val = cfg_read_dword(&pio, 0, 3, 0, 0x00);
        assert_eq!(val & 0xFFFF, 0x1AF4);
        assert_eq!((val >> 16) & 0xFFFF, 0x1000);
    }

    #[test]
    fn try_attach_rejects_an_occupied_slot() {
        let (pio, cs) = setup();
        let bdf = Bdf::new_unchecked(0, 5, 0);
        cs.attach_device(bdf, dummy_dev(0x1AF4, 0x1041));

        let err = cs
            .try_attach_device(bdf, dummy_dev(0x1AF4, 0x1042))
            .expect_err("slot is occupied");
        assert!(matches!(err, AttachError::SlotOccupied(b) if b == bdf));
        assert_eq!(cfg_read_dword(&pio, 0, 5, 0, 0x00) >> 16, 0x1041);
    }

    #[test]
    fn detach_frees_the_slot() {
        let (pio, cs) = setup();
        let bdf = Bdf::new_unchecked(0, 6, 0);
        cs.attach_device(bdf, dummy_dev(0x1AF4, 0x1041));

        assert!(cs.detach_device(&bdf).is_some());
        assert_eq!(cfg_read_dword(&pio, 0, 6, 0, 0x00), 0xFFFF_FFFF);
        assert!(cs.detach_device(&bdf).is_none());

        // The freed slot takes a new device.
        cs.try_attach_device(bdf, dummy_dev(0x1AF4, 0x1000))
            .expect("slot is free");
        assert_eq!(cfg_read_dword(&pio, 0, 6, 0, 0x00) >> 16, 0x1000);
    }

    #[test]
    fn bar_sizing_through_chipset() {
        let (pio, cs) = setup();

        struct BarDev(Mutex<DeviceState>);
        impl PciDevice for BarDev {
            fn cfg_read(&self, offset: u8, len: u8) -> u32 {
                self.0.lock().unwrap().cfg_read(offset, len)
            }
            fn cfg_write(&self, offset: u8, len: u8, val: u32) {
                self.0.lock().unwrap().cfg_write(offset, len, val);
            }
            fn bar_rw(&self, _: BarN, _: usize, _: RWOp<'_>) {}
        }

        let mut ds = DeviceState::new(DeviceIdent {
            vendor_id: 0x1234,
            device_id: 0x5678,
            ..Default::default()
        });
        ds.define_bar(BarN::BAR0, crate::pci::BarDefine::Mmio(0x1000));
        let dev: Arc<dyn PciDevice> = Arc::new(BarDev(Mutex::new(ds)));
        cs.attach_device(Bdf::new_unchecked(0, 4, 0), dev);

        cfg_write_dword(&pio, 0, 4, 0, 0x10, 0xFFFF_FFFF);
        let readback = cfg_read_dword(&pio, 0, 4, 0, 0x10);
        // Size 0x1000, type bits 0b000.
        assert_eq!(readback, 0xFFFF_F000);

        cfg_write_dword(&pio, 0, 4, 0, 0x10, 0xF000_0000);
        let readback = cfg_read_dword(&pio, 0, 4, 0, 0x10);
        assert_eq!(readback, 0xF000_0000);
    }
}
