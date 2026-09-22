// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Read-only AHCI ATAPI CD-ROM controller.
//!
//! It differs from C bhyve's `pci_ahci.c` on purpose:
//!
//! - Level INTx, because this VMM has no MSI capability.
//! - One port, not 32.
//! - Command-list and received-FIS memory are resolved on each use, not
//!   cached as host pointers.
//! - Every guest-controlled address and length is bounds-checked.
//!
//! The checks close the guest-triggerable host SIGSEGV paths in bhyve's
//! `ahci_port_stop`, `ahci_handle_dsm_trim`, `ahci_handle_read_write` and
//! `pci_ahci_ioreq_init`. Checked READ(12) sizing avoids the `u32` wrap in
//! bhyve's `atapi_read`. A zero-length read completes once, not twice.

pub mod atapi;
pub mod bits;
pub mod decode;
pub mod media;
pub mod port;

use std::sync::{Arc, Mutex, Weak};

use vmm_core::common::{RWOp, ReadOp, WriteOp};
use vmm_core::mem::PhysMap;
use vmm_core::mmio::{MmioBus, MmioFn};

use vmm_devices::pci::bar::BarDefine;
use vmm_devices::pci::device::{DeviceIdent, DeviceState, PciDevice};
use vmm_devices::pci::{BarN, IntrPin};
use vmm_devices::{Lifecycle, Migrator};

use bits::*;
use media::IsoMedia;

/// Single-port AHCI controller for a read-only ATAPI CD-ROM.
pub struct AhciCtrl {
    pci_state: Mutex<DeviceState>,
    state: Mutex<AhciState>,
    media: Arc<IsoMedia>,
    physmap: Arc<PhysMap>,
    bus_mmio: Arc<MmioBus>,
    registered_abar: Mutex<Option<u64>>,
    intr_pin: Option<Arc<dyn IntrPin>>,
    self_ref: Mutex<Option<Weak<Self>>>,
    log: slog::Logger,
    #[cfg(test)]
    prdbc_writes: Mutex<Vec<(usize, u32)>>,
    #[cfg(test)]
    completion_count: Mutex<usize>,
}

pub(crate) struct AhciState {
    pub hba: HbaRegs,
    pub port: PortRegs,
}

pub(crate) struct HbaRegs {
    pub ghc: u32,
    pub is: u32,
    pub ccc_ctl: u32,
    pub ccc_ports: u32,
}

pub(crate) struct PortRegs {
    pub clb: u32,
    pub clbu: u32,
    pub fb: u32,
    pub fbu: u32,
    pub is: u32,
    pub ie: u32,
    pub cmd: u32,
    pub tfd: u32,
    pub sig: u32,
    pub ssts: u32,
    pub sctl: u32,
    pub serr: u32,
    pub sact: u32,
    pub ci: u32,
    pub sntf: u32,
    pub reset_pending: bool,
    pub wait_for_clear: bool,
    pub sense_key: u8,
    pub asc: u8,
}

impl HbaRegs {
    fn new() -> Self {
        let mut regs = Self {
            ghc: 0,
            is: 0,
            ccc_ctl: 0,
            ccc_ports: 0,
        };
        regs.reset();
        regs
    }

    fn reset(&mut self) {
        self.ghc = GHC_RESET;
        self.is = 0;
        self.ccc_ctl = 0;
        self.ccc_ports = 0;
    }
}

impl PortRegs {
    fn new() -> Self {
        Self {
            clb: 0,
            clbu: 0,
            fb: 0,
            fbu: 0,
            is: 0,
            ie: 0,
            cmd: PXCMD_RESET,
            tfd: PXTFD_RESET_ATAPI,
            sig: PXSIG_ATAPI,
            ssts: PXSSTS_RESET,
            sctl: 0,
            serr: 0,
            sact: 0,
            ci: 0,
            sntf: 0,
            reset_pending: false,
            wait_for_clear: false,
            sense_key: 0,
            asc: 0,
        }
    }
}

impl AhciState {
    fn new() -> Self {
        let mut state = Self {
            hba: HbaRegs::new(),
            port: PortRegs::new(),
        };
        reset_port_state(&mut state);
        state
    }
}

fn port_stop(st: &mut AhciState) {
    st.port.cmd &= !(PXCMD_CR | PXCMD_CCS_MASK);
    st.port.ci = 0;
    st.port.sact = 0;
    st.port.wait_for_clear = false;
}

fn reset_port_state(st: &mut AhciState) {
    st.port.serr = 0;
    st.port.sact = 0;
    // A reset must not leave phantom commands for the dispatch loop.
    // storahci does not expect CI to survive a COMRESET.
    st.port.ci = 0;
    st.port.wait_for_clear = false;
    st.port.reset_pending = false;
    st.port.ssts = if st.port.sctl & PXSCTL_SPD_MASK != 0 {
        0x0000_0103 | (st.port.sctl & PXSCTL_SPD_MASK)
    } else {
        PXSSTS_RESET
    };
    st.port.tfd = PXTFD_RESET_ATAPI;
    st.port.sig = PXSIG_ATAPI;
}

impl AhciCtrl {
    /// Create an AHCI controller backed by read-only ISO media.
    pub fn new(
        media: Arc<IsoMedia>,
        physmap: Arc<PhysMap>,
        bus_mmio: Arc<MmioBus>,
        intr_pin: Option<Arc<dyn IntrPin>>,
        log: slog::Logger,
    ) -> Arc<Self> {
        let ident = DeviceIdent {
            vendor_id: PCI_VENDOR_INTEL,
            device_id: PCI_DEVICE_ICH8_AHCI,
            class: vmm_devices::pci::bits::CLASS_STORAGE,
            subclass: vmm_devices::pci::bits::SUBCLASS_STORAGE_SATA,
            prog_if: vmm_devices::pci::bits::PROGIF_SATA_AHCI_1_0,
            revision: 0x01,
            sub_vendor_id: PCI_VENDOR_INTEL,
            sub_device_id: PCI_DEVICE_ICH8_AHCI,
        };

        let mut pci_state = DeviceState::new(ident);
        pci_state.define_bar(BarN::BAR5, BarDefine::Mmio(ABAR_SIZE));
        pci_state.set_intr_pin(1);

        let ctrl = Arc::new(Self {
            pci_state: Mutex::new(pci_state),
            state: Mutex::new(AhciState::new()),
            media,
            physmap,
            bus_mmio,
            registered_abar: Mutex::new(None),
            intr_pin,
            self_ref: Mutex::new(None),
            log,
            #[cfg(test)]
            prdbc_writes: Mutex::new(Vec::new()),
            #[cfg(test)]
            completion_count: Mutex::new(0),
        });

        *ctrl.self_ref.lock().expect("ahci: self_ref lock") =
            Some(Arc::downgrade(&ctrl));

        ctrl
    }

    /// Unregister the recorded ABAR base and clear the record.
    fn release_abar(&self, reg: &mut Option<u64>) {
        if let Some(addr) = reg.take() {
            if let Err(e) = self.bus_mmio.unregister(addr) {
                slog::warn!(self.log, "ahci: MMIO unregister failed";
                    "bar" => ?BarN::BAR5,
                    "addr" => format!("{addr:#x}"),
                    "error" => %e);
            }
        }
    }

    fn update_mmio_registration(self: &Arc<Self>) {
        let pci = self.pci_state.lock().expect("ahci: pci lock");
        let mmio_enabled = pci
            .command()
            .contains(vmm_devices::pci::bits::RegCmd::MMIO_EN);

        let mut reg = self.registered_abar.lock().expect("ahci: abar reg lock");
        self.release_abar(&mut reg);

        if mmio_enabled {
            if let Some((def, addr)) = pci.bars().get(BarN::BAR5) {
                if addr != 0 && def.is_mmio() {
                    let size = def.size();
                    let dev = Arc::clone(self);
                    let handler: Arc<MmioFn> =
                        Arc::new(move |offset: usize, rwo: RWOp<'_>| {
                            dev.bar_rw(BarN::BAR5, offset, rwo);
                        });
                    match self.bus_mmio.register(addr, size, handler) {
                        Ok(()) => *reg = Some(addr),
                        // On failure the controller decodes nothing on ABAR.
                        Err(e) => {
                            slog::error!(self.log,
                                "ahci: MMIO register failed, ABAR dark";
                                "bar" => ?BarN::BAR5,
                                "addr" => format!("{addr:#x}"),
                                "size" => size,
                                "error" => %e);
                        }
                    }
                }
            }
        }
    }

    fn abar_read(&self, offset: usize, ro: &mut ReadOp) {
        let dword_off = offset & !0x3;
        let shift = (offset & 0x3) * 8;
        match ro.len() {
            1 => ro.write_u8((self.read_dword(dword_off) >> shift) as u8),
            2 => ro.write_u16((self.read_dword(dword_off) >> shift) as u16),
            8 => {
                let lo = self.read_dword(dword_off) as u64;
                let hi = self.read_dword(dword_off + 4) as u64;
                ro.write_u64((lo | (hi << 32)) >> shift);
            }
            _ => ro.write_u32(self.read_dword(dword_off) >> shift),
        }
    }

    fn read_dword(&self, offset: usize) -> u32 {
        let st = self.state.lock().expect("ahci: state lock");
        if offset < AHCI_OFFSET {
            return match offset {
                HBA_CAP => CAP_VALUE,
                HBA_GHC => st.hba.ghc,
                HBA_IS => st.hba.is,
                HBA_PI => PI_VALUE,
                HBA_VS => VS_VALUE,
                HBA_CCC_CTL => st.hba.ccc_ctl,
                HBA_CCC_PORTS => st.hba.ccc_ports,
                HBA_CAP2 => CAP2_VALUE,
                _ => 0,
            };
        }

        if offset < AHCI_OFFSET + AHCI_STEP {
            return match offset - AHCI_OFFSET {
                PX_CLB => st.port.clb,
                PX_CLBU => st.port.clbu,
                PX_FB => st.port.fb,
                PX_FBU => st.port.fbu,
                PX_IS => st.port.is,
                PX_IE => st.port.ie,
                PX_CMD => st.port.cmd,
                PX_TFD => st.port.tfd,
                PX_SIG => st.port.sig,
                PX_SSTS => st.port.ssts,
                PX_SCTL => st.port.sctl,
                PX_SERR => st.port.serr,
                PX_SACT => st.port.sact,
                PX_CI => st.port.ci,
                PX_SNTF => st.port.sntf,
                PX_FBS | PX_DEVSLP | 0x1C => 0,
                _ => 0,
            };
        }

        0
    }

    fn abar_write(&self, offset: usize, wo: &WriteOp) {
        if !offset.is_multiple_of(4) || wo.len() != 4 {
            slog::debug!(self.log, "ahci: ignored invalid ABAR write";
                "offset" => offset,
                "len" => wo.len(),
            );
            return;
        }

        let val = wo.read_u32();
        // refresh_intr takes both controller locks, so the state guard must
        // drop before the call.
        let needs_intr = {
            let mut st = self.state.lock().expect("ahci: state lock");
            if offset < AHCI_OFFSET {
                match offset {
                    HBA_GHC => {
                        if val & GHC_HR != 0 {
                            self.hba_reset(&mut st);
                        } else {
                            st.hba.ghc = GHC_AE | (val & GHC_IE);
                        }
                        true
                    }
                    HBA_IS => {
                        st.hba.is &= !val;
                        true
                    }
                    HBA_CCC_CTL => {
                        st.hba.ccc_ctl = val;
                        false
                    }
                    HBA_CCC_PORTS => {
                        st.hba.ccc_ports = val;
                        false
                    }
                    _ => false,
                }
            } else if offset < AHCI_OFFSET + AHCI_STEP {
                match offset - AHCI_OFFSET {
                    PX_CLB => {
                        st.port.clb = val & !0x3FF;
                        false
                    }
                    PX_CLBU => {
                        st.port.clbu = val;
                        false
                    }
                    PX_FB => {
                        st.port.fb = val & !0xFF;
                        false
                    }
                    PX_FBU => {
                        st.port.fbu = val;
                        false
                    }
                    PX_IS => {
                        st.port.is &= !val;
                        true
                    }
                    PX_IE => {
                        st.port.ie = val & PXIE_WMASK;
                        true
                    }
                    PX_CMD => {
                        st.port.cmd =
                            (st.port.cmd & !PXCMD_WMASK) | (val & PXCMD_WMASK);
                        if val & PXCMD_ST != 0 {
                            st.port.cmd |= PXCMD_CR;
                        } else {
                            port_stop(&mut st);
                        }
                        if val & PXCMD_FRE != 0 {
                            st.port.cmd |= PXCMD_FR;
                        } else {
                            st.port.cmd &= !PXCMD_FR;
                        }
                        if val & PXCMD_CLO != 0 {
                            st.port.tfd &= !(ATA_S_BUSY | ATA_S_DRQ);
                            st.port.cmd &= !PXCMD_CLO;
                        }
                        if val & PXCMD_ICC_MASK != 0 {
                            st.port.cmd &= !PXCMD_ICC_MASK;
                        }
                        self.handle_port(&mut st)
                    }
                    PX_TFD | PX_SIG | PX_SSTS => false,
                    PX_SCTL => {
                        st.port.sctl = val;
                        if st.port.cmd & PXCMD_ST == 0
                            && val & PXSCTL_DET_MASK == PXSCTL_DET_COMRESET
                        {
                            self.port_reset(&mut st)
                        } else {
                            false
                        }
                    }
                    PX_SERR => {
                        st.port.serr &= !val;
                        false
                    }
                    PX_SACT => {
                        st.port.sact |= val;
                        false
                    }
                    PX_CI => {
                        st.port.ci |= val;
                        self.handle_port(&mut st)
                    }
                    _ => false,
                }
            } else {
                false
            }
        };

        if needs_intr {
            self.refresh_intr();
        }
    }

    fn refresh_intr(&self) {
        // Callers release controller state before entering this lock order.
        let intx_disabled = {
            let pci = self.pci_state.lock().expect("ahci: pci lock");
            pci.command()
                .contains(vmm_devices::pci::bits::RegCmd::INTX_DIS)
        };
        let level = {
            let mut st = self.state.lock().expect("ahci: state lock");
            // Only this path sets HBA IS and only its W1C clears it. Thus the
            // guest's clear-PxIS-then-clear-IS order converges.
            if st.port.is & st.port.ie != 0 {
                st.hba.is |= 1;
            }
            st.hba.is != 0 && st.hba.ghc & GHC_IE != 0 && !intx_disabled
        };
        if let Some(pin) = &self.intr_pin {
            // The shared INTx line is level-triggered. GHC.IE and PCI
            // INTX_DIS both gate it, so guest masking cannot leave it raised.
            if pin.is_asserted() != level {
                pin.set_state(level);
            }
        }
    }
}

impl PciDevice for AhciCtrl {
    fn cfg_read(&self, offset: u8, len: u8) -> u32 {
        let pci = self.pci_state.lock().expect("ahci: pci_state lock");
        pci.cfg_read(offset, len)
    }

    fn cfg_write(&self, offset: u8, len: u8, val: u32) {
        let mut pci = self.pci_state.lock().expect("ahci: pci_state lock");
        let old_command = pci.command();
        pci.cfg_write(offset, len, val);
        let command_changed = pci.command() != old_command;

        let dword_off = offset & 0xFC;
        if dword_off == 0x04 || (0x10..=0x24).contains(&dword_off) {
            let weak = self.self_ref.lock().expect("ahci: self_ref lock");
            let arc_self = weak.as_ref().and_then(Weak::upgrade);
            drop(weak);
            drop(pci);
            if let Some(arc_self) = arc_self {
                arc_self.update_mmio_registration();
                if command_changed {
                    arc_self.refresh_intr();
                }
            }
        }
    }

    fn bar_rw(&self, bar: BarN, offset: usize, rwo: RWOp<'_>) {
        match bar {
            BarN::BAR5 => match rwo {
                RWOp::Read(ro) => self.abar_read(offset, ro),
                RWOp::Write(wo) => self.abar_write(offset, wo),
            },
            _ => {
                if let RWOp::Read(ro) = rwo {
                    ro.write_u32(0);
                }
            }
        }
    }

    fn detach_regions(&self) {
        let mut reg = self.registered_abar.lock().expect("ahci: abar reg lock");
        self.release_abar(&mut reg);
    }
}

impl Lifecycle for AhciCtrl {
    fn type_name(&self) -> &'static str {
        "ahci-cd"
    }

    fn migrate(&'_ self) -> Migrator<'_> {
        // Guest-visible command and sense state have no migration schema.
        // A stateless migration could resume partial I/O.
        Migrator::NonMigratable
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use vmm_core::common::{RWOp, ReadOp, WriteOp};

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

    fn test_controller(intr_pin: Option<Arc<dyn IntrPin>>) -> Arc<AhciCtrl> {
        let file =
            tempfile::NamedTempFile::new().expect("create temporary ISO");
        file.as_file().set_len(4096).expect("size temporary ISO");
        let media = IsoMedia::open(file.path()).expect("open temporary ISO");

        AhciCtrl::new(
            media,
            Arc::new(PhysMap::new()),
            Arc::new(MmioBus::new()),
            intr_pin,
            slog::Logger::root(slog::Discard, slog::o!()),
        )
    }

    fn abar_read(ctrl: &AhciCtrl, offset: usize, len: usize) -> u64 {
        let mut ro = ReadOp::new(len);
        ctrl.bar_rw(BarN::BAR5, offset, RWOp::Read(&mut ro));
        match len {
            1 => u64::from(ro.buf()[0]),
            2 => u64::from(u16::from_le_bytes([ro.buf()[0], ro.buf()[1]])),
            4 => u64::from(u32::from_le_bytes(
                ro.buf().try_into().expect("four-byte ABAR read"),
            )),
            8 => u64::from_le_bytes(
                ro.buf().try_into().expect("eight-byte ABAR read"),
            ),
            _ => unreachable!("unsupported test ABAR read width"),
        }
    }

    fn abar_write(ctrl: &AhciCtrl, offset: usize, data: &[u8]) {
        let wo = WriteOp::from_buf(data);
        ctrl.bar_rw(BarN::BAR5, offset, RWOp::Write(&wo));
    }

    fn abar_write_u32(ctrl: &AhciCtrl, offset: usize, val: u32) {
        abar_write(ctrl, offset, &val.to_le_bytes());
    }

    #[test]
    fn pci_identity_is_ich8_ahci() {
        let ctrl = test_controller(None);

        let identity = ctrl.cfg_read(0x00, 4);
        let class = ctrl.cfg_read(0x08, 4);

        assert_eq!(identity & 0xFFFF, 0x8086);
        assert_eq!((identity >> 16) & 0xFFFF, 0x2821);
        assert_eq!((class >> 24) & 0xFF, 1);
        assert_eq!((class >> 16) & 0xFF, 6);
        assert_eq!((class >> 8) & 0xFF, 1);
    }

    #[test]
    fn no_capability_list() {
        let ctrl = test_controller(None);

        assert_eq!(ctrl.cfg_read(0x34, 1), 0);
        assert_eq!((ctrl.cfg_read(0x04, 4) >> 16) & (1 << 4), 0);
    }

    #[test]
    fn intr_pin_is_inta() {
        let ctrl = test_controller(None);

        assert_eq!((ctrl.cfg_read(0x3C, 4) >> 8) & 0xFF, 1);
    }

    #[test]
    fn bar5_is_32bit_mmio() {
        let ctrl = test_controller(None);

        ctrl.cfg_write(0x24, 4, u32::MAX);
        let bar5 = ctrl.cfg_read(0x24, 4);

        assert_eq!(bar5 & 0x7, vmm_devices::pci::bits::BAR_TYPE_MEM);
        assert_eq!(bar5 & vmm_devices::pci::bits::BAR_TYPE_IO, 0);
        assert_eq!(bar5 & vmm_devices::pci::bits::BAR_TYPE_MEM64, 0);
        assert_eq!(bar5, !(ABAR_SIZE - 1));
        for offset in (0x10..=0x20).step_by(4) {
            assert_eq!(ctrl.cfg_read(offset, 4), 0);
        }
    }

    #[test]
    fn hba_reset_values() {
        let ctrl = test_controller(None);

        assert_eq!(abar_read(&ctrl, 0x00, 4), u64::from(CAP_VALUE));
        assert_eq!(abar_read(&ctrl, 0x04, 4), u64::from(GHC_RESET));
        assert_eq!(abar_read(&ctrl, 0x0C, 4), u64::from(PI_VALUE));
        assert_eq!(abar_read(&ctrl, 0x10, 4), u64::from(VS_VALUE));
        assert_eq!(abar_read(&ctrl, 0x24, 4), u64::from(CAP2_VALUE));
    }

    #[test]
    fn port_reset_values() {
        let ctrl = test_controller(None);

        assert_eq!(abar_read(&ctrl, 0x118, 4), u64::from(PXCMD_RESET));
        assert_eq!(abar_read(&ctrl, 0x120, 4), u64::from(PXTFD_RESET_ATAPI),);
        assert_eq!(abar_read(&ctrl, 0x124, 4), u64::from(PXSIG_ATAPI));
        assert_eq!(abar_read(&ctrl, 0x128, 4), u64::from(PXSSTS_RESET));
    }

    #[test]
    fn ghc_hr_self_clears() {
        let ctrl = test_controller(None);

        abar_write_u32(&ctrl, HBA_GHC, GHC_HR);
        let ghc = abar_read(&ctrl, HBA_GHC, 4) as u32;

        assert_eq!(ghc & GHC_HR, 0);
        assert_ne!(ghc & GHC_AE, 0);
    }

    #[test]
    fn ghc_hr_resets_port() {
        let ctrl = test_controller(None);
        abar_write_u32(&ctrl, AHCI_OFFSET + PX_IE, u32::MAX);
        abar_write_u32(&ctrl, AHCI_OFFSET + PX_CMD, PXCMD_ST);

        abar_write_u32(&ctrl, HBA_GHC, GHC_HR);

        assert_eq!(abar_read(&ctrl, AHCI_OFFSET + PX_IE, 4), 0);
        assert_eq!(
            abar_read(&ctrl, AHCI_OFFSET + PX_CMD, 4),
            u64::from(PXCMD_RESET),
        );
    }

    #[test]
    fn ghc_hr_cancels_midflight_command_state() {
        let ctrl = test_controller(None);
        {
            let mut st = ctrl.state.lock().expect("ahci: state lock");
            st.port.cmd = PXCMD_ST
                | PXCMD_CR
                | PXCMD_FRE
                | PXCMD_FR
                | (7 << PXCMD_CCS_SHIFT);
            st.port.ci = 0x8080_0001;
            st.port.sact = u32::MAX;
            st.port.tfd = ATA_S_BUSY;
            st.port.wait_for_clear = true;
        }

        abar_write_u32(&ctrl, HBA_GHC, GHC_HR);

        let st = ctrl.state.lock().expect("ahci: state lock");
        assert_eq!(st.port.ci, 0);
        assert_eq!(st.port.sact, 0);
        assert_eq!(st.port.cmd & (PXCMD_ST | PXCMD_CR | PXCMD_CCS_MASK), 0);
        assert_eq!(st.port.tfd, PXTFD_RESET_ATAPI);
        assert!(!st.port.wait_for_clear);
    }

    #[test]
    fn is_is_write_one_to_clear() {
        let ctrl = test_controller(None);
        {
            let mut st = ctrl.state.lock().expect("ahci: state lock");
            st.port.is = PXIS_DHRS;
            st.port.ie = PXIS_DHRS;
        }
        ctrl.refresh_intr();
        assert_eq!(abar_read(&ctrl, HBA_IS, 4), 1);

        abar_write_u32(&ctrl, HBA_IS, 1);

        assert_eq!(abar_read(&ctrl, HBA_IS, 4), 1);
    }

    #[test]
    fn pxie_masks_reserved_bits() {
        let ctrl = test_controller(None);

        abar_write_u32(&ctrl, AHCI_OFFSET + PX_IE, u32::MAX);

        assert_eq!(
            abar_read(&ctrl, AHCI_OFFSET + PX_IE, 4),
            u64::from(PXIE_WMASK),
        );
    }

    #[test]
    fn pxcmd_masks_readonly_bits() {
        let ctrl = test_controller(None);

        abar_write_u32(&ctrl, AHCI_OFFSET + PX_CMD, u32::MAX);
        let cmd = abar_read(&ctrl, AHCI_OFFSET + PX_CMD, 4) as u32;

        assert_ne!(cmd & PXCMD_CPS, 0);
        assert_eq!(cmd & !(PXCMD_CPS | PXCMD_WMASK | PXCMD_CR | PXCMD_FR), 0,);
    }

    #[test]
    fn pxcmd_clo_clears_bsy_drq() {
        let ctrl = test_controller(None);
        ctrl.state.lock().expect("ahci: state lock").port.tfd |=
            ATA_S_BUSY | ATA_S_DRQ;

        abar_write_u32(&ctrl, AHCI_OFFSET + PX_CMD, PXCMD_CLO);
        let tfd = abar_read(&ctrl, AHCI_OFFSET + PX_TFD, 4) as u32;
        let cmd = abar_read(&ctrl, AHCI_OFFSET + PX_CMD, 4) as u32;

        assert_eq!(tfd & (ATA_S_BUSY | ATA_S_DRQ), 0);
        assert_eq!(cmd & PXCMD_CLO, 0);
    }

    #[test]
    fn clearing_pxcmd_st_cancels_ci_and_command_running() {
        let ctrl = test_controller(None);
        {
            let mut st = ctrl.state.lock().expect("ahci: state lock");
            st.port.cmd = PXCMD_ST | PXCMD_CR | (12 << PXCMD_CCS_SHIFT);
            st.port.ci = 0x8000_0001;
            st.port.wait_for_clear = true;
        }

        abar_write_u32(&ctrl, AHCI_OFFSET + PX_CMD, 0);

        let st = ctrl.state.lock().expect("ahci: state lock");
        assert_eq!(st.port.ci, 0);
        assert_eq!(st.port.cmd & (PXCMD_ST | PXCMD_CR | PXCMD_CCS_MASK), 0);
        assert!(!st.port.wait_for_clear);
    }

    #[test]
    fn migration_is_refused_without_controller_state_schema() {
        let ctrl = test_controller(None);

        assert!(matches!(ctrl.migrate(), Migrator::NonMigratable));
    }

    #[test]
    fn identify_packet_completes_and_clears_ci() {
        let ctrl = test_controller(None);
        abar_write_u32(&ctrl, AHCI_OFFSET + PX_CMD, PXCMD_ST);

        abar_write_u32(&ctrl, AHCI_OFFSET + PX_CI, 1);

        assert_eq!(
            abar_read(&ctrl, AHCI_OFFSET + PX_TFD, 4),
            u64::from(TFD_ABORT)
        );
        assert_eq!(abar_read(&ctrl, AHCI_OFFSET + PX_CI, 4), 0);
    }

    #[test]
    fn non_h2d_cfis_clears_ci_without_fis() {
        let ctrl = test_controller(None);
        let mut st = ctrl.state.lock().expect("ahci: state lock");
        st.port.ci = 1;
        let initial_is = st.port.is;
        let initial_tfd = st.port.tfd;

        let needs_intr = ctrl.handle_command_fis(
            &mut st,
            0,
            [0; FIS_D2H_LEN],
            &[0; CMD_TBL_PRDT_OFF],
            0,
        );

        assert!(!needs_intr);
        assert_eq!(st.port.ci, 0);
        assert_eq!(st.port.is, initial_is);
        assert_eq!(st.port.tfd, initial_tfd);
    }

    #[test]
    fn dispatch_terminates_with_all_ci_bits_set() {
        let ctrl = test_controller(None);
        abar_write_u32(&ctrl, AHCI_OFFSET + PX_CMD, PXCMD_ST);

        abar_write_u32(&ctrl, AHCI_OFFSET + PX_CI, u32::MAX);

        assert_eq!(
            abar_read(&ctrl, AHCI_OFFSET + PX_TFD, 4),
            u64::from(TFD_ABORT)
        );
        assert_eq!(abar_read(&ctrl, AHCI_OFFSET + PX_CI, 4), 0);
    }

    #[test]
    fn readonly_port_regs_ignore_writes() {
        let ctrl = test_controller(None);
        let offsets = [PX_TFD, PX_SIG, PX_SSTS];
        let before: Vec<u64> = offsets
            .iter()
            .map(|offset| abar_read(&ctrl, AHCI_OFFSET + offset, 4))
            .collect();

        for offset in offsets {
            abar_write_u32(&ctrl, AHCI_OFFSET + offset, 0xA5A5_5A5A);
        }

        let after: Vec<u64> = offsets
            .iter()
            .map(|offset| abar_read(&ctrl, AHCI_OFFSET + offset, 4))
            .collect();
        assert_eq!(after, before);
    }

    #[test]
    fn unaligned_and_subdword_reads() {
        let ctrl = test_controller(None);

        assert_eq!(abar_read(&ctrl, 0x125, 1), 0x01);
        assert_eq!(abar_read(&ctrl, 0x126, 2), 0xEB14);
    }

    #[test]
    fn unaligned_write_is_ignored_not_panicking() {
        let ctrl = test_controller(None);
        let ghc = abar_read(&ctrl, HBA_GHC, 4);
        let clb = abar_read(&ctrl, AHCI_OFFSET + PX_CLB, 4);

        abar_write(&ctrl, 0x005, &[0xFF]);
        abar_write_u32(&ctrl, 0x102, u32::MAX);

        assert_eq!(abar_read(&ctrl, HBA_GHC, 4), ghc);
        assert_eq!(abar_read(&ctrl, AHCI_OFFSET + PX_CLB, 4), clb);
    }

    #[test]
    fn out_of_range_abar_reads_zero() {
        let ctrl = test_controller(None);

        assert_eq!(abar_read(&ctrl, 0x800, 4), 0);
    }

    #[test]
    fn intx_suppressed_when_disabled() {
        let pin = TestPin::new();
        let intr_pin: Arc<dyn IntrPin> = pin.clone();
        let ctrl = test_controller(Some(intr_pin));
        ctrl.cfg_write(0x04, 2, 0);
        {
            let mut st = ctrl.state.lock().expect("ahci: state lock");
            st.hba.ghc |= GHC_IE;
            st.port.is = PXIS_DHRS;
            st.port.ie = PXIS_DHRS;
        }

        ctrl.refresh_intr();
        assert!(pin.is_asserted());

        ctrl.cfg_write(
            0x04,
            2,
            u32::from(vmm_devices::pci::bits::RegCmd::INTX_DIS.bits()),
        );

        assert!(!pin.is_asserted());
    }
}
