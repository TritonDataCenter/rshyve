// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! xHCI USB 3.0 host controller with built-in HID tablet device.
//!
//! A minimal xHCI controller that presents one USB HID absolute-pointer
//! tablet to the guest. It is the device behind `-s 30:1,xhci,tablet` and
//! supplies VNC pointer input for the framebuffer.
//!
//! # Architecture
//!
//! - BAR0: 32-bit MMIO (8 KiB) containing capability, operational,
//!   port, doorbell, runtime, and extended capability registers.
//! - INTx interrupt delivery via PCI PIRQ routing.
//! - Single device slot (slot 1) hosting the tablet.
//! - 2 ports (1 USB2 + 1 USB3) for extended capability compliance.
//! - 1 interrupter (interrupter 0).
//! - Command ring, event ring, and per-endpoint transfer rings.

pub mod bits;
pub mod tablet;

use std::sync::{Arc, Mutex, Weak};

use vmm_core::common::RWOp;
use vmm_core::mem::PhysMap;
use vmm_core::mmio::{MmioBus, MmioFn};

use vmm_devices::pci::bar::BarDefine;
use vmm_devices::pci::device::{DeviceIdent, DeviceState, PciDevice};
use vmm_devices::pci::{BarN, IntrPin};
use vmm_devices::Lifecycle;

use bits::*;
use tablet::TabletDevice;

/// Replace the low dword of a 64-bit register.
fn merge_low_dword(reg: u64, val: u32) -> u64 {
    (reg & 0xFFFF_FFFF_0000_0000) | u64::from(val)
}

/// Replace the high dword of a 64-bit register.
fn merge_high_dword(reg: u64, val: u32) -> u64 {
    (reg & 0x0000_0000_FFFF_FFFF) | (u64::from(val) << 32)
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Disabled,
    Enabled,
    Addressed,
    Configured,
}

/// Per-endpoint transfer ring tracking.
#[derive(Debug)]
struct TransferRing {
    /// Guest physical address of the ring base.
    base: u64,
    dequeue: u64,
    /// Ring cycle state (expected cycle bit).
    ccs: bool,
}

impl TransferRing {
    fn new() -> Self {
        Self {
            base: 0,
            dequeue: 0,
            ccs: true,
        }
    }
}

/// xHCI controller mutable state (under lock).
struct XhciState {
    // ---- Operational registers ----
    usbcmd: u32,
    usbsts: u32,
    dnctrl: u32,
    crcr: u64,   // Command Ring Control Register
    dcbaap: u64, // Device Context Base Address Array Pointer
    config: u32,

    // ---- Port state ----
    /// Port 1 (USB2) status/control.
    port1_sc: u32,
    /// Port 2 (USB3) status/control. The tablet is connected here.
    port2_sc: u32,

    // ---- Runtime / interrupter 0 ----
    iman: u32,
    imod: u32,
    erstsz: u32,
    erstba: u64,
    erdp: u64,

    // ---- Event ring state ----
    /// Guest physical base of event ring segment 0.
    event_ring_base: u64,
    /// Size of event ring segment 0 (in TRBs).
    event_ring_size: u32,
    /// Next write index into the event ring.
    event_ring_enqueue: u32,
    /// Event ring producer cycle state.
    event_ring_pcs: bool,

    // ---- Command ring state ----
    cmd_ring_base: u64,
    cmd_ring_dequeue: u64,
    cmd_ring_ccs: bool,

    // ---- Device slot 1 ----
    slot_state: SlotState,
    /// EP0 (control) transfer ring.
    ep0_ring: TransferRing,
    /// EP1 IN (interrupt) transfer ring for tablet reports.
    ep1_in_ring: TransferRing,
    /// Whether EP1 IN has pending TRBs queued by the guest.
    ep1_in_has_trbs: bool,
}

impl XhciState {
    fn new() -> Self {
        Self {
            usbcmd: 0,
            usbsts: USBSTS_HCH, // Halted on reset
            dnctrl: 0,
            crcr: 0,
            dcbaap: 0,
            config: 0,

            // Port 1 (USB2): not connected, powered
            port1_sc: PORTSC_PP | (PLS_RXDETECT << PORTSC_PLS_SHIFT),
            // Port 2 (USB3): connected, powered, SuperSpeed
            port2_sc: PORTSC_CCS
                | PORTSC_PP
                | (PLS_U0 << PORTSC_PLS_SHIFT)
                | (SPEED_SUPER << PORTSC_SPEED_SHIFT),

            iman: 0,
            imod: 0,
            erstsz: 0,
            erstba: 0,
            erdp: 0,

            event_ring_base: 0,
            event_ring_size: 0,
            event_ring_enqueue: 0,
            event_ring_pcs: true,

            cmd_ring_base: 0,
            cmd_ring_dequeue: 0,
            cmd_ring_ccs: true,

            slot_state: SlotState::Disabled,
            ep0_ring: TransferRing::new(),
            ep1_in_ring: TransferRing::new(),
            ep1_in_has_trbs: false,
        }
    }

    fn enable_slot(&mut self) -> (u32, u8) {
        if self.slot_state != SlotState::Disabled {
            return (TRB_CC_NO_SLOTS_AVAILABLE, 0);
        }

        self.slot_state = SlotState::Enabled;
        (TRB_CC_SUCCESS, 1)
    }
}

// ---------------------------------------------------------------------------
// xHCI Controller
// ---------------------------------------------------------------------------

/// xHCI USB 3.0 host controller.
///
/// Presents a PCI device with a single BAR0 (MMIO) and INTx interrupt.
/// Hosts a built-in USB HID tablet device for absolute pointer input.
pub struct XhciController {
    pci_state: Mutex<DeviceState>,
    state: Mutex<XhciState>,
    tablet: TabletDevice,
    physmap: Arc<PhysMap>,
    /// MMIO bus for BAR registration.
    bus_mmio: Arc<MmioBus>,
    /// Currently registered BAR0 address.
    registered_bar0: Mutex<Option<u64>>,
    /// Interrupt pin for INTx delivery.
    intr_pin: Option<Arc<dyn IntrPin>>,
    /// Weak self-reference for MMIO handler closures.
    self_ref: Mutex<Option<Weak<Self>>>,
    /// Structured logger for bus registration diagnostics.
    log: slog::Logger,
}

impl XhciController {
    /// Create a new xHCI controller with a built-in tablet device.
    ///
    /// Bus registration failures go to a discarding logger. Use
    /// [`new_with_logger`](Self::new_with_logger) to see them.
    pub fn new(
        physmap: Arc<PhysMap>,
        bus_mmio: Arc<MmioBus>,
        intr_pin: Option<Arc<dyn IntrPin>>,
    ) -> Arc<Self> {
        Self::new_with_logger(
            physmap,
            bus_mmio,
            intr_pin,
            slog::Logger::root(slog::Discard, slog::o!()),
        )
    }

    /// Create a new xHCI controller with a logger for bus registration
    /// diagnostics.
    pub fn new_with_logger(
        physmap: Arc<PhysMap>,
        bus_mmio: Arc<MmioBus>,
        intr_pin: Option<Arc<dyn IntrPin>>,
        log: slog::Logger,
    ) -> Arc<Self> {
        let ident = DeviceIdent {
            vendor_id: PCI_VENDOR_INTEL,
            device_id: PCI_DEVICE_XHCI_PPT,
            class: PCI_CLASS_SERIAL,
            subclass: PCI_SUBCLASS_USB,
            prog_if: PCI_PROGIF_XHCI,
            revision: 0x04,
            sub_vendor_id: PCI_VENDOR_INTEL,
            sub_device_id: PCI_DEVICE_XHCI_PPT,
        };

        let mut pci_state = DeviceState::new(ident);
        pci_state.define_bar(BarN::BAR0, BarDefine::Mmio(BAR0_SIZE));
        pci_state.set_intr_pin(1); // INTA

        let ctrl = Arc::new(Self {
            pci_state: Mutex::new(pci_state),
            state: Mutex::new(XhciState::new()),
            tablet: TabletDevice::new(),
            physmap,
            bus_mmio,
            registered_bar0: Mutex::new(None),
            intr_pin,
            self_ref: Mutex::new(None),
            log,
        });

        *ctrl.self_ref.lock().expect("xhci: self_ref lock") =
            Some(Arc::downgrade(&ctrl));

        ctrl
    }

    /// Get a reference to the built-in tablet device.
    ///
    /// The VNC server uses this to call `pointer_event()`.
    pub fn tablet(&self) -> &TabletDevice {
        &self.tablet
    }

    // ---- MMIO BAR registration -------------------------------------------

    /// Unregister the recorded BAR0 base and clear the record.
    fn release_mmio(&self, reg: &mut Option<u64>) {
        if let Some(addr) = reg.take() {
            if let Err(e) = self.bus_mmio.unregister(addr) {
                slog::warn!(self.log, "xhci: MMIO unregister failed";
                    "bar" => ?BarN::BAR0,
                    "addr" => format!("{addr:#x}"),
                    "error" => %e);
            }
        }
    }

    /// Update MMIO bus registration after BAR or command register changes.
    fn update_mmio_registration(self: &Arc<Self>) {
        let pci = self.pci_state.lock().expect("xhci: pci lock");
        let mmio_enabled = pci
            .command()
            .contains(vmm_devices::pci::bits::RegCmd::MMIO_EN);

        let mut reg = self.registered_bar0.lock().expect("xhci: bar0 reg lock");
        self.release_mmio(&mut reg);

        if mmio_enabled {
            if let Some((def, addr)) = pci.bars().get(BarN::BAR0) {
                if addr != 0 && def.is_mmio() {
                    let size = def.size();
                    let dev = Arc::clone(self);
                    let handler: Arc<MmioFn> =
                        Arc::new(move |offset: usize, rwo: RWOp<'_>| {
                            dev.bar_rw(BarN::BAR0, offset, rwo);
                        });
                    match self.bus_mmio.register(addr, size, handler) {
                        Ok(()) => *reg = Some(addr),
                        // The controller now decodes nothing on BAR0.
                        Err(e) => {
                            slog::error!(self.log,
                                "xhci: MMIO register failed, BAR0 dark";
                                "bar" => ?BarN::BAR0,
                                "addr" => format!("{addr:#x}"),
                                "size" => size,
                                "error" => %e);
                        }
                    }
                }
            }
        }
    }

    // ---- Interrupt delivery ----------------------------------------------

    /// Assert the INTx interrupt to the guest.
    fn assert_interrupt(&self, st: &mut XhciState) {
        st.usbsts |= USBSTS_EINT;
        st.iman |= IMAN_IP;
        if let Some(pin) = &self.intr_pin {
            pin.pulse();
        }
    }

    // ---- Event ring management -------------------------------------------

    /// Write an event TRB to the event ring and optionally assert interrupt.
    fn post_event(&self, st: &mut XhciState, event: RawTrb) {
        if st.event_ring_base == 0 || st.event_ring_size == 0 {
            return;
        }

        let idx = st.event_ring_enqueue;
        if idx >= st.event_ring_size {
            // An out-of-range index would write outside the ring. Reset
            // it and drop the event.
            st.event_ring_enqueue = 0;
            return;
        }
        let offset = u64::from(idx) * TRB_SIZE as u64;
        let gpa = match st.event_ring_base.checked_add(offset) {
            Some(a) => a,
            None => return,
        };

        let mut trb = event;
        if st.event_ring_pcs {
            trb.dword3 |= TRB_CYCLE;
        } else {
            trb.dword3 &= !TRB_CYCLE;
        }

        // SAFETY: RawTrb is repr(C) with four u32 fields, so it has no
        // padding and every bit pattern of its 16 bytes is a valid [u8; 16].
        let trb_bytes: [u8; TRB_SIZE] = unsafe { std::mem::transmute(trb) };
        if let Some(sub) = self.physmap.lookup(gpa, TRB_SIZE) {
            let _ = sub.write_bytes(&trb_bytes);
        }

        st.event_ring_enqueue = idx + 1;
        if st.event_ring_enqueue >= st.event_ring_size {
            st.event_ring_enqueue = 0;
            st.event_ring_pcs = !st.event_ring_pcs;
        }

        if (st.iman & IMAN_IE) != 0 && (st.usbcmd & USBCMD_INTE) != 0 {
            self.assert_interrupt(st);
        }
    }

    /// Post a Command Completion Event.
    fn post_cmd_completion(
        &self,
        st: &mut XhciState,
        cmd_trb_addr: u64,
        cc: u32,
        slot_id: u8,
    ) {
        let event = RawTrb {
            dword0: cmd_trb_addr as u32,
            dword1: (cmd_trb_addr >> 32) as u32,
            dword2: (cc << 24),
            dword3: (TRB_TYPE_CMD_COMPLETION << TRB_TYPE_SHIFT)
                | (u32::from(slot_id) << 24),
        };
        self.post_event(st, event);
    }

    /// Post a Port Status Change Event.
    fn post_port_status_change(&self, st: &mut XhciState, port_id: u8) {
        let event = RawTrb {
            dword0: 0,
            dword1: 0,
            dword2: TRB_CC_SUCCESS << 24,
            dword3: (TRB_TYPE_PORT_STATUS_CHANGE << TRB_TYPE_SHIFT)
                | (u32::from(port_id) << 24),
        };
        self.post_event(st, event);
    }

    /// Post a Transfer Event.
    fn post_transfer_event(
        &self,
        st: &mut XhciState,
        trb_addr: u64,
        cc: u32,
        transfer_len: u32,
        slot_id: u8,
        ep_id: u8,
    ) {
        let event = RawTrb {
            dword0: trb_addr as u32,
            dword1: (trb_addr >> 32) as u32,
            dword2: (cc << 24) | (transfer_len & 0xFFFFFF),
            dword3: (TRB_TYPE_TRANSFER_EVENT << TRB_TYPE_SHIFT)
                | (u32::from(slot_id) << 24)
                | (u32::from(ep_id) << 16),
        };
        self.post_event(st, event);
    }

    /// Apply a complete CRCR value.
    ///
    /// The command ring never runs between doorbells in this model, so
    /// CRR is never set and every write lands. A driver that writes the
    /// two halves separately reaches here twice. The last write wins.
    fn write_crcr(&self, st: &mut XhciState, crcr: u64) {
        let ptr = crcr & CRCR_PTR_MASK;
        st.crcr = crcr & !CRCR_CRR;
        st.cmd_ring_base = ptr;
        st.cmd_ring_dequeue = ptr;
        st.cmd_ring_ccs = (crcr & CRCR_RCS) != 0;
    }

    // ---- Event Ring Segment Table parsing ---------------------------------

    /// Read the ERST and configure the event ring.
    ///
    /// ERST Max advertises one segment, and ERSTSZ is clamped to match,
    /// so entry 0 describes the whole ring.
    fn setup_event_ring(&self, st: &mut XhciState) {
        if st.erstba == 0 || st.erstsz == 0 {
            return;
        }

        let mut buf = [0u8; 16];
        if let Some(sub) = self.physmap.lookup(st.erstba, 16) {
            if sub.read_bytes(&mut buf).is_err() {
                return;
            }
        } else {
            return;
        }

        let base_lo = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let base_hi = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let size = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);

        // Cap the ring size so a guest cannot make the enqueue pointer sweep
        // a huge range of guest memory before it wraps. The spec permits
        // larger segments, but 4096 TRBs (64 KiB) is ample for one tablet.
        const MAX_EVENT_RING_SIZE: u32 = 4096;
        if size == 0 || size > MAX_EVENT_RING_SIZE {
            return;
        }

        st.event_ring_base = u64::from(base_lo) | (u64::from(base_hi) << 32);
        st.event_ring_size = size;
        st.event_ring_enqueue = 0;
        st.event_ring_pcs = true;
    }

    // ---- Command ring processing -----------------------------------------

    /// Read a TRB from guest memory at the given GPA.
    fn read_trb(&self, gpa: u64) -> Option<RawTrb> {
        let mut buf = [0u8; TRB_SIZE];
        let sub = self.physmap.lookup(gpa, TRB_SIZE)?;
        sub.read_bytes(&mut buf).ok()?;

        Some(RawTrb {
            dword0: u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
            dword1: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            dword2: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            dword3: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
        })
    }

    /// Process all pending TRBs on the command ring.
    fn process_command_ring(&self, st: &mut XhciState) {
        if st.cmd_ring_base == 0 {
            return;
        }

        // Bound the work per doorbell so a looping ring cannot hang the vCPU.
        for _ in 0..16 {
            let trb_addr = st.cmd_ring_dequeue;
            let trb = match self.read_trb(trb_addr) {
                Some(t) => t,
                None => break,
            };

            // A cycle bit mismatch marks the end of the queued commands.
            if trb.cycle_bit() != st.cmd_ring_ccs {
                break;
            }

            let trb_type = trb.trb_type();

            match trb_type {
                TRB_TYPE_LINK => {
                    // Follow the link to the new segment
                    st.cmd_ring_dequeue = trb.parameter() & CRCR_PTR_MASK;
                    // Toggle cycle if the Toggle Cycle bit is set
                    if (trb.dword3 & (1 << 1)) != 0 {
                        st.cmd_ring_ccs = !st.cmd_ring_ccs;
                    }
                    continue;
                }

                TRB_TYPE_ENABLE_SLOT => {
                    let (cc, slot_id) = st.enable_slot();
                    self.post_cmd_completion(st, trb_addr, cc, slot_id);
                }

                TRB_TYPE_ADDRESS_DEVICE => {
                    if st.slot_state == SlotState::Disabled {
                        self.post_cmd_completion(
                            st,
                            trb_addr,
                            TRB_CC_SLOT_NOT_ENABLED,
                            trb.slot_id(),
                        );
                    } else {
                        // Read the input context to get EP0 transfer ring
                        let input_ctx_addr = trb.parameter() & !0xF;
                        self.parse_input_context_for_ep0(st, input_ctx_addr);
                        st.slot_state = SlotState::Addressed;
                        self.post_cmd_completion(
                            st,
                            trb_addr,
                            TRB_CC_SUCCESS,
                            trb.slot_id(),
                        );
                    }
                }

                TRB_TYPE_CONFIGURE_EP => {
                    if st.slot_state == SlotState::Disabled {
                        self.post_cmd_completion(
                            st,
                            trb_addr,
                            TRB_CC_SLOT_NOT_ENABLED,
                            trb.slot_id(),
                        );
                    } else {
                        // Parse input context for EP1 IN configuration
                        let input_ctx_addr = trb.parameter() & !0xF;
                        self.parse_input_context_for_ep1(st, input_ctx_addr);
                        st.slot_state = SlotState::Configured;
                        self.post_cmd_completion(
                            st,
                            trb_addr,
                            TRB_CC_SUCCESS,
                            trb.slot_id(),
                        );
                    }
                }

                TRB_TYPE_EVALUATE_CTX | TRB_TYPE_NOOP_CMD => {
                    self.post_cmd_completion(
                        st,
                        trb_addr,
                        TRB_CC_SUCCESS,
                        trb.slot_id(),
                    );
                }

                TRB_TYPE_RESET_EP => {
                    self.post_cmd_completion(
                        st,
                        trb_addr,
                        TRB_CC_SUCCESS,
                        trb.slot_id(),
                    );
                }

                TRB_TYPE_SET_TR_DEQUEUE => {
                    let ep_id = ((trb.dword3 >> 16) & 0x1F) as u8;
                    let new_deq = trb.parameter() & !0xF;
                    let new_ccs = (trb.parameter() & 1) != 0;
                    match ep_id {
                        1 => {
                            st.ep0_ring.dequeue = new_deq;
                            st.ep0_ring.ccs = new_ccs;
                        }
                        3 => {
                            st.ep1_in_ring.dequeue = new_deq;
                            st.ep1_in_ring.ccs = new_ccs;
                        }
                        _ => {}
                    }
                    self.post_cmd_completion(
                        st,
                        trb_addr,
                        TRB_CC_SUCCESS,
                        trb.slot_id(),
                    );
                }

                _ => {
                    // Unknown command: complete with error
                    self.post_cmd_completion(
                        st,
                        trb_addr,
                        TRB_CC_TRB_ERROR,
                        trb.slot_id(),
                    );
                }
            }

            st.cmd_ring_dequeue += TRB_SIZE as u64;
        }
    }

    /// Read the EP0 transfer ring dequeue pointer from the Input Context.
    fn parse_input_context_for_ep0(
        &self,
        st: &mut XhciState,
        input_ctx_addr: u64,
    ) {
        // Input Context layout (32-byte contexts):
        //   [0x00..0x1F] Input Control Context
        //   [0x20..0x3F] Slot Context
        //   [0x40..0x5F] EP0 Context (DCI 1)
        let ep0_ctx_addr =
            input_ctx_addr + INPUT_CTRL_CTX_SIZE as u64 + SLOT_CTX_SIZE as u64;

        // EP Context dword 2 (offset 0x08): TR Dequeue Pointer low
        // EP Context dword 3 (offset 0x0C): TR Dequeue Pointer high
        let mut buf = [0u8; EP_CTX_SIZE];
        if let Some(sub) = self.physmap.lookup(ep0_ctx_addr, EP_CTX_SIZE) {
            if sub.read_bytes(&mut buf).is_ok() {
                let deq_lo =
                    u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
                let deq_hi =
                    u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
                let deq = u64::from(deq_lo) | (u64::from(deq_hi) << 32);
                st.ep0_ring.dequeue = deq & !0xF;
                st.ep0_ring.ccs = (deq & 1) != 0;
                st.ep0_ring.base = st.ep0_ring.dequeue;
            }
        }
    }

    /// Read the EP1 IN transfer ring dequeue pointer from the Input Context.
    fn parse_input_context_for_ep1(
        &self,
        st: &mut XhciState,
        input_ctx_addr: u64,
    ) {
        // After the Input Control Context, contexts are indexed by DCI
        // (slot = 0, DCI = 2 * endpoint + direction). EP1 IN is DCI 3, at
        // offset 0x80 even though the tablet has no EP1 OUT.
        let ep1_in_ctx_addr = input_ctx_addr
            + INPUT_CTRL_CTX_SIZE as u64
            + 3 * EP_CTX_SIZE as u64; // DCI 3

        let mut buf = [0u8; EP_CTX_SIZE];
        if let Some(sub) = self.physmap.lookup(ep1_in_ctx_addr, EP_CTX_SIZE) {
            if sub.read_bytes(&mut buf).is_ok() {
                let deq_lo =
                    u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
                let deq_hi =
                    u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
                let deq = u64::from(deq_lo) | (u64::from(deq_hi) << 32);
                st.ep1_in_ring.dequeue = deq & !0xF;
                st.ep1_in_ring.ccs = (deq & 1) != 0;
                st.ep1_in_ring.base = st.ep1_in_ring.dequeue;
            }
        }
    }

    // ---- Transfer ring processing ----------------------------------------

    /// Process the EP0 (control) transfer ring.
    ///
    /// Handles SETUP/DATA/STATUS stage TRBs for USB control transfers.
    fn process_ep0_transfer_ring(&self, st: &mut XhciState) {
        for _ in 0..32 {
            let trb_addr = st.ep0_ring.dequeue;
            let trb = match self.read_trb(trb_addr) {
                Some(t) => t,
                None => break,
            };

            if trb.cycle_bit() != st.ep0_ring.ccs {
                break;
            }

            let trb_type = trb.trb_type();

            match trb_type {
                TRB_TYPE_LINK => {
                    st.ep0_ring.dequeue = trb.parameter() & CRCR_PTR_MASK;
                    if (trb.dword3 & (1 << 1)) != 0 {
                        st.ep0_ring.ccs = !st.ep0_ring.ccs;
                    }
                    continue;
                }

                TRB_TYPE_SETUP_STAGE => {
                    // Parse the 8-byte SETUP packet from the TRB
                    let bm_request_type = trb.dword0 as u8;
                    let b_request = (trb.dword0 >> 8) as u8;
                    let w_value = (trb.dword0 >> 16) as u16;
                    let w_index = trb.dword1 as u16;
                    let w_length = (trb.dword1 >> 16) as u16;

                    let response = self.tablet.handle_control(
                        bm_request_type,
                        b_request,
                        w_value,
                        w_index,
                        w_length,
                    );

                    // Complete the SETUP stage
                    self.post_transfer_event(
                        st,
                        trb_addr,
                        TRB_CC_SUCCESS,
                        8, // SETUP is always 8 bytes
                        1, // slot 1
                        1, // EP0 = DCI 1
                    );

                    st.ep0_ring.dequeue += TRB_SIZE as u64;

                    self.process_ep0_data_status(st, response);
                    continue;
                }

                _ => {
                    // Skip unexpected TRB types
                    st.ep0_ring.dequeue += TRB_SIZE as u64;
                    continue;
                }
            }
        }
    }

    /// Process DATA and STATUS stage TRBs following a SETUP stage.
    fn process_ep0_data_status(
        &self,
        st: &mut XhciState,
        response: Option<Vec<u8>>,
    ) {
        for _ in 0..8 {
            let trb_addr = st.ep0_ring.dequeue;
            let trb = match self.read_trb(trb_addr) {
                Some(t) => t,
                None => break,
            };

            if trb.cycle_bit() != st.ep0_ring.ccs {
                break;
            }

            let trb_type = trb.trb_type();
            match trb_type {
                TRB_TYPE_LINK => {
                    st.ep0_ring.dequeue = trb.parameter() & CRCR_PTR_MASK;
                    if (trb.dword3 & (1 << 1)) != 0 {
                        st.ep0_ring.ccs = !st.ep0_ring.ccs;
                    }
                    continue;
                }

                TRB_TYPE_DATA_STAGE => {
                    let data_buf_addr = trb.parameter();
                    let trb_transfer_len = trb.transfer_length();
                    let dir_in = (trb.dword3 & TRB_DATA_DIR_IN) != 0;

                    let cc;
                    let residual;

                    if dir_in {
                        // IN data stage: copy response data to guest buffer
                        if let Some(ref data) = response {
                            let copy_len =
                                data.len().min(trb_transfer_len as usize);
                            if let Some(sub) =
                                self.physmap.lookup(data_buf_addr, copy_len)
                            {
                                let _ = sub.write_bytes(&data[..copy_len]);
                            }
                            residual = trb_transfer_len
                                .saturating_sub(copy_len as u32);
                            cc = if copy_len < trb_transfer_len as usize {
                                TRB_CC_SHORT_PACKET
                            } else {
                                TRB_CC_SUCCESS
                            };
                        } else {
                            // STALL
                            cc = TRB_CC_STALL;
                            residual = trb_transfer_len;
                        }
                    } else {
                        // OUT data stage. The device accepts the data and
                        // ignores it.
                        cc = TRB_CC_SUCCESS;
                        residual = 0;
                    }

                    self.post_transfer_event(st, trb_addr, cc, residual, 1, 1);
                    st.ep0_ring.dequeue += TRB_SIZE as u64;
                }

                TRB_TYPE_STATUS_STAGE => {
                    let cc = if response.is_some() {
                        TRB_CC_SUCCESS
                    } else {
                        TRB_CC_STALL
                    };
                    self.post_transfer_event(st, trb_addr, cc, 0, 1, 1);
                    st.ep0_ring.dequeue += TRB_SIZE as u64;
                    return;
                }

                _ => {
                    st.ep0_ring.dequeue += TRB_SIZE as u64;
                }
            }
        }
    }

    /// Process the EP1 IN (interrupt) transfer ring.
    ///
    /// If the tablet has a pending report and the guest has queued TRBs,
    /// copy the report into the guest buffer and post a transfer event.
    fn process_ep1_in_transfer_ring(&self, st: &mut XhciState) {
        if st.slot_state != SlotState::Configured {
            return;
        }

        if !self.tablet.has_pending_report() {
            // No data to send. Mark that TRBs are available for later.
            st.ep1_in_has_trbs = true;
            return;
        }

        for _ in 0..4 {
            let trb_addr = st.ep1_in_ring.dequeue;
            let trb = match self.read_trb(trb_addr) {
                Some(t) => t,
                None => {
                    st.ep1_in_has_trbs = false;
                    break;
                }
            };

            if trb.cycle_bit() != st.ep1_in_ring.ccs {
                st.ep1_in_has_trbs = false;
                break;
            }

            let trb_type = trb.trb_type();

            match trb_type {
                TRB_TYPE_LINK => {
                    st.ep1_in_ring.dequeue = trb.parameter() & CRCR_PTR_MASK;
                    if (trb.dword3 & (1 << 1)) != 0 {
                        st.ep1_in_ring.ccs = !st.ep1_in_ring.ccs;
                    }
                    continue;
                }

                TRB_TYPE_NORMAL => {
                    let data_buf_addr = trb.parameter();
                    let trb_transfer_len = trb.transfer_length();
                    let report = self.tablet.get_report();
                    let copy_len = report.len().min(trb_transfer_len as usize);

                    if let Some(sub) =
                        self.physmap.lookup(data_buf_addr, copy_len)
                    {
                        let _ = sub.write_bytes(&report[..copy_len]);
                    }

                    let residual =
                        trb_transfer_len.saturating_sub(copy_len as u32);
                    let cc = if copy_len < trb_transfer_len as usize {
                        TRB_CC_SHORT_PACKET
                    } else {
                        TRB_CC_SUCCESS
                    };

                    // EP1 IN = DCI 3
                    self.post_transfer_event(st, trb_addr, cc, residual, 1, 3);
                    st.ep1_in_ring.dequeue += TRB_SIZE as u64;
                    st.ep1_in_has_trbs = false;
                    return;
                }

                _ => {
                    st.ep1_in_ring.dequeue += TRB_SIZE as u64;
                }
            }
        }
    }

    /// Deliver a pending tablet report if the guest has queued EP1 IN TRBs.
    ///
    /// Call this after `tablet().pointer_event()`.
    pub fn notify_pointer_event(&self) {
        let mut st = self.state.lock().expect("xhci: state lock");
        if st.ep1_in_has_trbs {
            self.process_ep1_in_transfer_ring(&mut st);
        }
    }

    // ---- BAR0 register access -------------------------------------------

    /// Handle a BAR0 MMIO read or write.
    fn bar0_rw(&self, offset: usize, rwo: RWOp<'_>) {
        if offset < OP_BASE {
            self.cap_reg_rw(offset, rwo);
        } else if offset < (DBOFF as usize) {
            // Operational + port registers
            let op_off = offset - OP_BASE;
            self.op_reg_rw(op_off, rwo);
        } else if offset < (RTSOFF as usize) {
            // Doorbell registers
            let db_off = offset - DBOFF as usize;
            self.doorbell_rw(db_off, rwo);
        } else if offset < (XCAP_BASE) {
            // Runtime registers
            let rt_off = offset - RTSOFF as usize;
            self.runtime_reg_rw(rt_off, rwo);
        } else if offset < (BAR0_SIZE as usize) {
            // Extended capabilities
            let xcap_off = offset - XCAP_BASE;
            self.xcap_rw(xcap_off, rwo);
        } else {
            // Out of range
            if let RWOp::Read(ro) = rwo {
                ro.write_u32(0);
            }
        }
    }

    // ---- Capability registers (read-only) --------------------------------

    fn cap_reg_rw(&self, offset: usize, rwo: RWOp<'_>) {
        match rwo {
            RWOp::Read(ro) => {
                let val = match offset & !0x3 {
                    // CAPLENGTH (8) | reserved (8) | HCIVERSION (16)
                    0x00 => {
                        u32::from(CAPLENGTH) | (u32::from(HCIVERSION) << 16)
                    }
                    0x04 => hcsparams1(),
                    0x08 => hcsparams2(),
                    0x0C => HCSPARAMS3,
                    0x10 => hccparams1(),
                    0x14 => DBOFF,
                    0x18 => RTSOFF,
                    0x1C => HCCPARAMS2,
                    _ => 0,
                };
                ro.write_dword_at(val, offset)
            }
            RWOp::Write(_) => {
                // Capability registers are read-only
            }
        }
    }

    // ---- Operational registers -------------------------------------------

    fn op_reg_rw(&self, op_off: usize, rwo: RWOp<'_>) {
        if op_off >= PORTSC_BASE {
            self.port_reg_rw(op_off, rwo);
            return;
        }

        let mut st = self.state.lock().expect("xhci: state lock");
        match rwo {
            RWOp::Read(ro) => {
                // AC64 is advertised, so a guest may read a 64-bit
                // register in one access.
                if ro.len() == 8 {
                    match op_off {
                        OP_CRCR => return ro.write_u64(st.crcr),
                        OP_DCBAAP => return ro.write_u64(st.dcbaap),
                        _ => {}
                    }
                }
                let val = match op_off & !0x3 {
                    OP_USBCMD => st.usbcmd,
                    OP_USBSTS => st.usbsts,
                    OP_PAGESIZE => 1, // 4K pages
                    OP_DNCTRL => st.dnctrl,
                    OP_CRCR => st.crcr as u32,
                    0x1C => (st.crcr >> 32) as u32, // CRCR high
                    OP_DCBAAP => st.dcbaap as u32,
                    0x34 => (st.dcbaap >> 32) as u32, // DCBAAP high
                    OP_CONFIG => st.config,
                    _ => 0,
                };
                ro.write_dword_at(val, op_off)
            }
            RWOp::Write(wo) => {
                if wo.len() == 8 {
                    match op_off {
                        OP_CRCR => self.write_crcr(&mut st, wo.read_u64()),
                        OP_DCBAAP => st.dcbaap = wo.read_u64(),
                        _ => {}
                    }
                    return;
                }
                let val = wo.read_u32();
                match op_off & !0x3 {
                    OP_USBCMD => {
                        let old = st.usbcmd;
                        st.usbcmd = val;

                        // Handle Run/Stop transition
                        if (val & USBCMD_RS) != 0 && (old & USBCMD_RS) == 0 {
                            // Starting: clear HCHalted
                            st.usbsts &= !USBSTS_HCH;

                            // Report the connected USB3 port (port 2).
                            st.port2_sc |= PORTSC_CSC;
                            self.post_port_status_change(&mut st, 2);
                        } else if (val & USBCMD_RS) == 0
                            && (old & USBCMD_RS) != 0
                        {
                            // Stopping: set HCHalted
                            st.usbsts |= USBSTS_HCH;
                        }

                        // Handle Host Controller Reset
                        if (val & USBCMD_HCRST) != 0 {
                            st.usbcmd = 0;
                            st.usbsts = USBSTS_HCH;
                            st.crcr = 0;
                            st.dcbaap = 0;
                            st.config = 0;
                            st.slot_state = SlotState::Disabled;
                            st.event_ring_base = 0;
                            st.event_ring_size = 0;
                            st.event_ring_enqueue = 0;
                            st.event_ring_pcs = true;
                            st.cmd_ring_base = 0;
                            st.cmd_ring_dequeue = 0;
                            st.cmd_ring_ccs = true;
                            st.ep0_ring = TransferRing::new();
                            st.ep1_in_ring = TransferRing::new();
                            st.ep1_in_has_trbs = false;
                            st.iman = 0;
                            st.imod = 0;
                            st.erstsz = 0;
                            st.erstba = 0;
                            st.erdp = 0;
                        }
                    }
                    OP_USBSTS => {
                        // Write-1-to-clear for status bits
                        st.usbsts &= !val;
                        // Deassert interrupt if EINT was cleared
                        if (val & USBSTS_EINT) != 0 {
                            if let Some(pin) = &self.intr_pin {
                                pin.deassert();
                            }
                        }
                    }
                    OP_DNCTRL => {
                        st.dnctrl = val;
                    }
                    OP_CRCR => {
                        let crcr = merge_low_dword(st.crcr, val);
                        self.write_crcr(&mut st, crcr);
                    }
                    0x1C => {
                        let crcr = merge_high_dword(st.crcr, val);
                        self.write_crcr(&mut st, crcr);
                    }
                    OP_DCBAAP => {
                        st.dcbaap = merge_low_dword(st.dcbaap, val);
                    }
                    0x34 => {
                        st.dcbaap = merge_high_dword(st.dcbaap, val);
                    }
                    OP_CONFIG => {
                        st.config = (val & 0xFF).min(u32::from(MAX_SLOTS));
                    }
                    _ => {}
                }
            }
        }
    }

    // ---- Port registers --------------------------------------------------

    fn port_reg_rw(&self, op_off: usize, rwo: RWOp<'_>) {
        let port_off = op_off - PORTSC_BASE;
        let port_idx = port_off / PORT_REG_SIZE;
        let reg_off = port_off % PORT_REG_SIZE;

        let mut st = self.state.lock().expect("xhci: state lock");

        match rwo {
            RWOp::Read(ro) => {
                let val = match (port_idx, reg_off) {
                    (0, PORTSC_PORTSC) => st.port1_sc,
                    (1, PORTSC_PORTSC) => st.port2_sc,
                    _ => 0,
                };
                ro.write_dword_at(val, op_off)
            }
            RWOp::Write(wo) => {
                let val = wo.read_u32();
                if reg_off == PORTSC_PORTSC {
                    let portsc = match port_idx {
                        0 => &mut st.port1_sc,
                        1 => &mut st.port2_sc,
                        _ => return,
                    };

                    // Write-1-to-clear for change bits
                    let w1c_bits = PORTSC_CSC | PORTSC_PRC | PORTSC_WRC;
                    *portsc &= !(val & w1c_bits);

                    // Handle port reset
                    if (val & PORTSC_PR) != 0 && (*portsc & PORTSC_CCS) != 0 {
                        // Complete reset immediately
                        *portsc |= PORTSC_PED | PORTSC_PRC;
                        *portsc &= !PORTSC_PR;
                        // Set link state to U0
                        *portsc &= !(PORTSC_PLS_MASK << PORTSC_PLS_SHIFT);
                        *portsc |= PLS_U0 << PORTSC_PLS_SHIFT;

                        let port_id = (port_idx + 1) as u8;
                        self.post_port_status_change(&mut st, port_id);
                    }

                    // Handle port link state write
                    if (val & PORTSC_LWS) != 0 {
                        let new_pls =
                            (val >> PORTSC_PLS_SHIFT) & PORTSC_PLS_MASK;
                        let portsc = match port_idx {
                            0 => &mut st.port1_sc,
                            1 => &mut st.port2_sc,
                            _ => return,
                        };
                        *portsc &= !(PORTSC_PLS_MASK << PORTSC_PLS_SHIFT);
                        *portsc |= new_pls << PORTSC_PLS_SHIFT;
                    }
                }
            }
        }
    }

    // ---- Doorbell registers ----------------------------------------------

    fn doorbell_rw(&self, db_off: usize, rwo: RWOp<'_>) {
        match rwo {
            RWOp::Read(ro) => {
                // Doorbell registers read as 0
                ro.write_dword(0)
            }
            RWOp::Write(wo) => {
                let db_idx = db_off / DB_REG_SIZE;
                if db_idx > usize::from(MAX_SLOTS) {
                    return;
                }
                let val = wo.read_u32();
                let mut st = self.state.lock().expect("xhci: state lock");

                match db_idx {
                    // Doorbell 0: Command ring
                    0 => {
                        self.process_command_ring(&mut st);
                    }
                    // Doorbell 1: Device slot 1
                    1 => {
                        let ep_target = val & 0xFF;
                        match ep_target {
                            // DCI 1 = EP0 (control, bidirectional)
                            1 => {
                                self.process_ep0_transfer_ring(&mut st);
                            }
                            // DCI 3 = EP1 IN (interrupt)
                            3 => {
                                self.process_ep1_in_transfer_ring(&mut st);
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // ---- Runtime registers -----------------------------------------------

    fn runtime_reg_rw(&self, rt_off: usize, rwo: RWOp<'_>) {
        let mut st = self.state.lock().expect("xhci: state lock");

        match rwo {
            RWOp::Read(ro) => {
                // AC64 is advertised, so a guest may read a 64-bit
                // register in one access.
                if ro.len() == 8 && rt_off >= IR0_BASE {
                    match rt_off - IR0_BASE {
                        IR_ERSTBA => return ro.write_u64(st.erstba),
                        IR_ERDP => return ro.write_u64(st.erdp),
                        _ => {}
                    }
                }
                let val = if rt_off < IR0_BASE {
                    // Microframes are not tracked, so MFINDEX stays 0.
                    0
                } else {
                    let ir_off = rt_off - IR0_BASE;
                    match ir_off & !0x3 {
                        IR_IMAN => st.iman,
                        IR_IMOD => st.imod,
                        IR_ERSTSZ => st.erstsz,
                        IR_ERSTBA => st.erstba as u32,
                        0x14 => (st.erstba >> 32) as u32,
                        IR_ERDP => st.erdp as u32,
                        0x1C => (st.erdp >> 32) as u32,
                        _ => 0,
                    }
                };
                ro.write_dword_at(val, rt_off)
            }
            RWOp::Write(wo) => {
                if rt_off < IR0_BASE {
                    return; // MFINDEX is read-only
                }
                let ir_off = rt_off - IR0_BASE;
                if wo.len() == 8 {
                    match ir_off {
                        IR_ERSTBA => {
                            st.erstba = wo.read_u64() & ERSTBA_PTR_MASK;
                            self.setup_event_ring(&mut st);
                        }
                        IR_ERDP => st.erdp = wo.read_u64() & ERDP_PTR_MASK,
                        _ => {}
                    }
                    return;
                }
                let val = wo.read_u32();
                match ir_off & !0x3 {
                    IR_IMAN => {
                        // IP is write-1-to-clear. IE is read/write.
                        if (val & IMAN_IP) != 0 {
                            st.iman &= !IMAN_IP;
                        }
                        if (val & IMAN_IE) != 0 {
                            st.iman |= IMAN_IE;
                        } else {
                            st.iman &= !IMAN_IE;
                        }
                    }
                    IR_IMOD => {
                        st.imod = val;
                    }
                    IR_ERSTSZ => {
                        // ERST Max advertises one segment, so only the
                        // first entry can ever be valid.
                        st.erstsz = (val & 0xFFFF).min(1);
                    }
                    IR_ERSTBA => {
                        st.erstba =
                            merge_low_dword(st.erstba, val) & ERSTBA_PTR_MASK;
                        self.setup_event_ring(&mut st);
                    }
                    0x14 => {
                        st.erstba =
                            merge_high_dword(st.erstba, val) & ERSTBA_PTR_MASK;
                        self.setup_event_ring(&mut st);
                    }
                    IR_ERDP => {
                        st.erdp = merge_low_dword(st.erdp, val) & ERDP_PTR_MASK;
                    }
                    0x1C => {
                        st.erdp =
                            merge_high_dword(st.erdp, val) & ERDP_PTR_MASK;
                    }
                    _ => {}
                }
            }
        }
    }

    // ---- Extended capabilities -------------------------------------------

    fn xcap_rw(&self, xcap_off: usize, rwo: RWOp<'_>) {
        match rwo {
            RWOp::Read(ro) => {
                let val = if xcap_off < XCAP_PROTO_SIZE {
                    // USB2 Supported Protocol
                    let dw_idx = xcap_off / 4;
                    let usb2 = xcap_usb2();
                    if dw_idx < usb2.len() {
                        usb2[dw_idx]
                    } else {
                        0
                    }
                } else if xcap_off < 2 * XCAP_PROTO_SIZE {
                    // USB3 Supported Protocol
                    let dw_idx = (xcap_off - XCAP_PROTO_SIZE) / 4;
                    let usb3 = xcap_usb3();
                    if dw_idx < usb3.len() {
                        usb3[dw_idx]
                    } else {
                        0
                    }
                } else {
                    0
                };
                ro.write_dword_at(val, xcap_off)
            }
            RWOp::Write(_) => {
                // Extended capabilities are read-only
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PciDevice trait implementation
// ---------------------------------------------------------------------------

impl PciDevice for XhciController {
    fn cfg_read(&self, offset: u8, len: u8) -> u32 {
        let pci = self.pci_state.lock().expect("xhci: pci_state lock");
        pci.cfg_read(offset, len)
    }

    fn cfg_write(&self, offset: u8, len: u8, val: u32) {
        let mut pci = self.pci_state.lock().expect("xhci: pci_state lock");
        pci.cfg_write(offset, len, val);

        // A command register or BAR write can move or disable BAR0.
        let dword_off = offset & 0xFC;
        if dword_off == 0x04 || (0x10..=0x24).contains(&dword_off) {
            let weak = self.self_ref.lock().expect("xhci: self_ref lock");
            if let Some(arc_self) = weak.as_ref().and_then(|w| w.upgrade()) {
                drop(pci);
                arc_self.update_mmio_registration();
            }
        }
    }

    fn bar_rw(&self, bar: BarN, offset: usize, rwo: RWOp<'_>) {
        match bar {
            BarN::BAR0 => self.bar0_rw(offset, rwo),
            _ => {
                if let RWOp::Read(ro) = rwo {
                    ro.write_u32(0);
                }
            }
        }
    }

    fn detach_regions(&self) {
        let mut reg = self.registered_bar0.lock().expect("xhci: bar0 reg lock");
        self.release_mmio(&mut reg);
    }
}

impl vmm_devices::PointerSink for XhciController {
    fn pointer_event(&self, buttons: u8, x: u16, y: u16) {
        self.tablet().pointer_event(buttons, x, y);
        self.notify_pointer_event();
    }
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

impl Lifecycle for XhciController {
    fn type_name(&self) -> &'static str {
        "xhci"
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use vmm_core::common::{ReadOp, WriteOp};

    fn test_state() -> XhciState {
        XhciState::new()
    }

    fn test_controller() -> Arc<XhciController> {
        XhciController::new(
            Arc::new(PhysMap::new()),
            Arc::new(MmioBus::new()),
            None,
        )
    }

    const RAM_LEN: usize = 0x10_000;

    /// A controller over real guest memory, so `PhysMap::lookup`
    /// answers and the ring walks reach something.
    fn test_controller_with_ram() -> Arc<XhciController> {
        let physmap =
            PhysMap::new_anon(0, RAM_LEN).expect("anonymous guest memory");
        XhciController::new(Arc::new(physmap), Arc::new(MmioBus::new()), None)
    }

    fn write_ram(ctrl: &XhciController, gpa: u64, bytes: &[u8]) {
        let sub = ctrl
            .physmap
            .lookup(gpa, bytes.len())
            .expect("guest memory at gpa");
        sub.write_bytes(bytes).expect("write guest memory");
    }

    /// One ERST entry: `base` and `size` TRBs.
    fn write_erst_entry(ctrl: &XhciController, gpa: u64, base: u64, size: u32) {
        let mut entry = [0u8; 16];
        entry[..8].copy_from_slice(&base.to_le_bytes());
        entry[8..12].copy_from_slice(&size.to_le_bytes());
        write_ram(ctrl, gpa, &entry);
    }

    fn write_reg(ctrl: &XhciController, off: usize, bytes: &[u8]) {
        let wo = WriteOp::from_buf(bytes);
        ctrl.bar0_rw(off, RWOp::Write(&wo));
    }

    fn read_reg(ctrl: &XhciController, off: usize, len: usize) -> u64 {
        let mut ro = ReadOp::new(len);
        ctrl.bar0_rw(off, RWOp::Read(&mut ro));
        let mut buf = [0u8; 8];
        buf[..len].copy_from_slice(ro.buf());
        u64::from_le_bytes(buf)
    }

    const ERSTBA_OFF: usize = RTSOFF as usize + IR0_BASE + IR_ERSTBA;
    const ERSTSZ_OFF: usize = RTSOFF as usize + IR0_BASE + IR_ERSTSZ;
    const ERDP_OFF: usize = RTSOFF as usize + IR0_BASE + IR_ERDP;

    #[test]
    fn a_qword_erstba_write_sets_up_the_event_ring() {
        // The illumos xhci driver writes ERSTBA with one ddi_put64, so
        // a device that only latches the high dword never gets a ring.
        let ctrl = test_controller_with_ram();
        write_erst_entry(&ctrl, 0x2000, 0x3000, 16);
        write_reg(&ctrl, ERSTSZ_OFF, &1u32.to_le_bytes());
        write_reg(&ctrl, ERSTBA_OFF, &0x2000u64.to_le_bytes());

        let st = ctrl.state.lock().expect("xhci: state lock");
        assert_eq!(st.erstba, 0x2000);
        assert_eq!(st.event_ring_base, 0x3000);
        assert_eq!(st.event_ring_size, 16);
    }

    #[test]
    fn split_dword_erstba_writes_still_set_up_the_event_ring() {
        // Linux writes the low half first. Both orders must end with a
        // configured ring.
        for high_first in [false, true] {
            let ctrl = test_controller_with_ram();
            write_erst_entry(&ctrl, 0x2000, 0x3000, 16);
            write_reg(&ctrl, ERSTSZ_OFF, &1u32.to_le_bytes());
            let halves: [(usize, u32); 2] =
                [(ERSTBA_OFF, 0x2000), (ERSTBA_OFF + 4, 0)];
            let order = if high_first { [1, 0] } else { [0, 1] };
            for i in order {
                write_reg(&ctrl, halves[i].0, &halves[i].1.to_le_bytes());
            }

            let st = ctrl.state.lock().expect("xhci: state lock");
            assert_eq!(st.event_ring_base, 0x3000, "high_first={high_first}");
            assert_eq!(st.event_ring_size, 16, "high_first={high_first}");
        }
    }

    #[test]
    fn qword_writes_keep_the_upper_dword_of_every_64_bit_register() {
        let ctrl = test_controller_with_ram();
        let crcr = 0x0000_0001_2345_6740u64;
        let dcbaap = 0x0000_0002_3456_7800u64;
        let erdp = 0x0000_0003_4567_8900u64;
        write_reg(&ctrl, OP_BASE + OP_CRCR, &(crcr | CRCR_RCS).to_le_bytes());
        write_reg(&ctrl, OP_BASE + OP_DCBAAP, &dcbaap.to_le_bytes());
        write_reg(&ctrl, ERDP_OFF, &erdp.to_le_bytes());

        let st = ctrl.state.lock().expect("xhci: state lock");
        assert_eq!(st.cmd_ring_base, crcr);
        assert_eq!(st.cmd_ring_dequeue, crcr);
        assert!(st.cmd_ring_ccs);
        assert_eq!(st.dcbaap, dcbaap);
        assert_eq!(st.erdp, erdp);
    }

    #[test]
    fn qword_reads_answer_with_all_eight_bytes() {
        let ctrl = test_controller_with_ram();
        let dcbaap = 0x0000_0002_3456_7800u64;
        write_reg(&ctrl, OP_BASE + OP_DCBAAP, &dcbaap.to_le_bytes());
        assert_eq!(read_reg(&ctrl, OP_BASE + OP_DCBAAP, 8), dcbaap);

        write_erst_entry(&ctrl, 0x2000, 0x3000, 16);
        write_reg(&ctrl, ERSTSZ_OFF, &1u32.to_le_bytes());
        write_reg(&ctrl, ERSTBA_OFF, &0x2000u64.to_le_bytes());
        assert_eq!(read_reg(&ctrl, ERSTBA_OFF, 8), 0x2000);
    }

    #[test]
    fn erstsz_is_clamped_to_the_one_segment_we_advertise() {
        // Only ERST entry 0 is read, so a larger table would lose every
        // event posted into the segments the controller never walks.
        assert_eq!((hcsparams2() >> 4) & 0xF, 0, "ERST Max = 2^0 = 1");

        let ctrl = test_controller_with_ram();
        write_reg(&ctrl, ERSTSZ_OFF, &0xFFFFu32.to_le_bytes());
        assert_eq!(ctrl.state.lock().expect("xhci: state lock").erstsz, 1);
    }

    #[test]
    fn initial_state_is_halted() {
        let st = test_state();
        assert_ne!(st.usbsts & USBSTS_HCH, 0);
        assert_eq!(st.usbcmd & USBCMD_RS, 0);
    }

    #[test]
    fn slot_starts_disabled() {
        let st = test_state();
        assert_eq!(st.slot_state, SlotState::Disabled);
    }

    #[test]
    fn enable_slot_twice_reports_no_slots() {
        let mut st = test_state();
        assert_eq!(st.enable_slot(), (TRB_CC_SUCCESS, 1));
        assert_eq!(st.enable_slot(), (TRB_CC_NO_SLOTS_AVAILABLE, 0));
    }

    #[test]
    fn port2_connected_at_superspeed() {
        let st = test_state();
        assert_ne!(st.port2_sc & PORTSC_CCS, 0);
        let speed = (st.port2_sc >> PORTSC_SPEED_SHIFT) & PORTSC_SPEED_MASK;
        assert_eq!(speed, SPEED_SUPER);
    }

    #[test]
    fn port1_not_connected() {
        let st = test_state();
        assert_eq!(st.port1_sc & PORTSC_CCS, 0);
    }

    #[test]
    fn cap_caplength_hciversion() {
        let val = u32::from(CAPLENGTH) | (u32::from(HCIVERSION) << 16);
        assert_eq!(val & 0xFF, 0x20);
        assert_eq!((val >> 16) & 0xFFFF, 0x0100);
    }

    #[test]
    fn cap_hcsparams1() {
        let val = hcsparams1();
        assert_eq!(val & 0xFF, 64);
        assert_eq!((val >> 8) & 0x3FF, 1);
        assert_eq!((val >> 24) & 0xFF, 2);
    }

    #[test]
    fn cap_hccparams1_ac64() {
        let val = hccparams1();
        assert_ne!(val & 1, 0);
    }

    #[test]
    fn cap_dboff_rtsoff() {
        assert_eq!(DBOFF, 0x440);
        assert_eq!(RTSOFF, 0x560);
    }

    #[test]
    fn config_max_slots_is_clamped() {
        let ctrl = test_controller();
        let wo = WriteOp::from_buf(&u32::MAX.to_le_bytes());
        ctrl.bar0_rw(OP_BASE + OP_CONFIG, RWOp::Write(&wo));

        let st = ctrl.state.lock().expect("xhci: state lock");
        assert_eq!(st.config, u32::from(MAX_SLOTS));
    }

    #[test]
    fn doorbell_beyond_max_slots_is_ignored() {
        let ctrl = test_controller();
        let original_erstba = 0x1122_3344_5566_7780;
        ctrl.state.lock().expect("xhci: state lock").erstba = original_erstba;

        let wo = WriteOp::from_buf(&20u32.to_le_bytes());
        ctrl.bar0_rw(DBOFF as usize + 20 * DB_REG_SIZE, RWOp::Write(&wo));
        assert_eq!(
            ctrl.state.lock().expect("xhci: state lock").erstba,
            original_erstba,
        );

        let wo = WriteOp::from_buf(&28u32.to_le_bytes());
        ctrl.bar0_rw(DBOFF as usize + 28 * DB_REG_SIZE, RWOp::Write(&wo));
        assert_eq!(
            ctrl.state.lock().expect("xhci: state lock").erstba,
            original_erstba,
        );

        let beyond_max = usize::from(MAX_SLOTS) + 1;
        let wo = WriteOp::from_buf(&(beyond_max as u32).to_le_bytes());
        ctrl.bar0_rw(
            DBOFF as usize + beyond_max * DB_REG_SIZE,
            RWOp::Write(&wo),
        );
        assert_eq!(
            ctrl.state.lock().expect("xhci: state lock").erstba,
            original_erstba,
        );
    }

    #[test]
    fn trb_type_values() {
        assert_eq!(TRB_TYPE_ENABLE_SLOT, 9);
        assert_eq!(TRB_TYPE_ADDRESS_DEVICE, 11);
        assert_eq!(TRB_TYPE_CONFIGURE_EP, 12);
        assert_eq!(TRB_TYPE_NOOP_CMD, 23);
        assert_eq!(TRB_TYPE_CMD_COMPLETION, 33);
        assert_eq!(TRB_TYPE_PORT_STATUS_CHANGE, 34);
    }

    #[test]
    fn raw_trb_parsing() {
        let trb = RawTrb {
            dword0: 0x1000,
            dword1: 0,
            dword2: (TRB_CC_SUCCESS << 24) | 64,
            dword3: (TRB_TYPE_TRANSFER_EVENT << TRB_TYPE_SHIFT)
                | (1u32 << 24)
                | TRB_CYCLE,
        };
        assert_eq!(trb.trb_type(), TRB_TYPE_TRANSFER_EVENT);
        assert!(trb.cycle_bit());
        assert_eq!(trb.slot_id(), 1);
        assert_eq!(trb.parameter(), 0x1000);
        assert_eq!(trb.completion_code(), TRB_CC_SUCCESS);
        assert_eq!(trb.transfer_length(), 64);
    }

    #[test]
    fn event_ring_pcs_toggles_on_wrap() {
        let mut st = test_state();
        st.event_ring_base = 0x1000;
        st.event_ring_size = 2;
        st.event_ring_pcs = true;

        st.event_ring_enqueue = 1;
        st.event_ring_enqueue += 1;
        if st.event_ring_enqueue >= st.event_ring_size {
            st.event_ring_enqueue = 0;
            st.event_ring_pcs = !st.event_ring_pcs;
        }
        assert_eq!(st.event_ring_enqueue, 0);
        assert!(!st.event_ring_pcs);
    }

    #[test]
    fn transfer_ring_initial_state() {
        let ring = TransferRing::new();
        assert_eq!(ring.base, 0);
        assert_eq!(ring.dequeue, 0);
        assert!(ring.ccs);
    }

    #[test]
    fn xcap_usb2_cap_id() {
        let cap = xcap_usb2();
        assert_eq!(cap[0] & 0xFF, 2);
    }

    #[test]
    fn xcap_usb3_is_last() {
        let cap = xcap_usb3();
        assert_eq!((cap[0] >> 8) & 0xFF, 0);
    }

    #[test]
    fn crcr_ptr_mask() {
        let addr: u64 = 0x0000_1234_5678_9ABC;
        let masked = addr & CRCR_PTR_MASK;
        assert_eq!(masked & 0x3F, 0);
        assert_eq!(masked, 0x0000_1234_5678_9A80);
    }

    #[test]
    fn bar0_size_is_8k() {
        assert_eq!(BAR0_SIZE, 0x2000);
        assert!(BAR0_SIZE.is_power_of_two());
    }

    #[test]
    fn portsc_change_bits_are_w1c() {
        let mut portsc: u32 = PORTSC_CCS | PORTSC_PP | PORTSC_CSC;
        portsc &= !(PORTSC_CSC);
        assert_eq!(portsc & PORTSC_CSC, 0);
        assert_ne!(portsc & PORTSC_CCS, 0);
        assert_ne!(portsc & PORTSC_PP, 0);
    }
}
