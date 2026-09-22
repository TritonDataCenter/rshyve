// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Read back the MSI-X table size a guest driver will find.

use vmm_devices::pci::bits::CAP_ID_MSIX;
use vmm_devices::pci::PciDevice;

/// Configuration space offset of the Status register.
const STATUS_REG: u8 = 0x06;
/// Status bit 4: the device presents a capability list.
const STATUS_CAP_LIST: u32 = 1 << 4;
/// Configuration space offset of the Capabilities Pointer.
const CAP_PTR_REG: u8 = 0x34;
/// First offset a capability may start at. The 64 bytes below it are
/// the standard header.
const FIRST_CAP: u8 = 0x40;
/// Message Control bits 10:0 hold the table size, less one.
const TABLE_SIZE_MASK: u16 = 0x07FF;

/// Most capabilities a chain may hold before the walk gives up.
///
/// Configuration space has 0x40..=0xFF for capabilities and the
/// smallest one is 4 bytes, so a valid chain is shorter than this.
/// Passthrough reads the config space of real hardware, and a device
/// that reports a loop must not block the thread that attaches it.
const MAX_CAPS: usize = 48;

/// How many MSI-X vectors `dev` gives the guest, or `None` when it
/// presents no MSI-X capability.
///
/// The count is read through configuration space, as a guest driver
/// reads it, so a caller cannot report a number that the device does
/// not have.
pub fn vector_count(dev: &dyn PciDevice) -> Option<u16> {
    if dev.cfg_read(STATUS_REG, 2) & STATUS_CAP_LIST == 0 {
        return None;
    }

    // Bits 1:0 of a capability pointer are reserved and read zero.
    let mut offset = (dev.cfg_read(CAP_PTR_REG, 1) as u8) & 0xFC;
    for _ in 0..MAX_CAPS {
        if offset < FIRST_CAP {
            return None;
        }
        let header = dev.cfg_read(offset, 4);
        if header as u8 == CAP_ID_MSIX {
            let message_control = (header >> 16) as u16;
            return Some((message_control & TABLE_SIZE_MASK) + 1);
        }
        offset = ((header >> 8) as u8) & 0xFC;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmm_core::common::RWOp;
    use vmm_devices::pci::BarN;

    /// A device whose configuration space the test writes by hand.
    ///
    /// Every offset and identifier below is the PCI value, not the
    /// constant above. A test double that reused those constants would
    /// change with them and pass whatever offset the walk read.
    struct FakeDevice {
        space: Vec<u8>,
    }

    /// PCI Status, Capabilities Pointer, the CAP_LIST bit and the MSI-X
    /// capability ID (PCI 3.0, 6.2.3 and 6.8.2).
    const PCI_STATUS: usize = 0x06;
    const PCI_CAP_PTR: usize = 0x34;
    const PCI_STATUS_CAP_LIST: u16 = 0x0010;
    const PCI_CAP_ID_MSIX: u8 = CAP_ID_MSIX;
    /// Lowest offset a capability may sit at.
    const PCI_FIRST_CAP: u8 = 0x40;

    impl FakeDevice {
        fn new() -> Self {
            Self {
                space: vec![0u8; 256],
            }
        }

        fn set_status(&mut self, status: u16) {
            self.space[PCI_STATUS] = status as u8;
            self.space[PCI_STATUS + 1] = (status >> 8) as u8;
        }

        fn set_cap_ptr(&mut self, offset: u8) {
            self.space[PCI_CAP_PTR] = offset;
        }

        /// Place one capability header: id, next pointer and the
        /// 16 bits that follow them.
        fn set_cap(&mut self, at: u8, id: u8, next: u8, control: u16) {
            let at = at as usize;
            self.space[at] = id;
            self.space[at + 1] = next;
            self.space[at + 2] = control as u8;
            self.space[at + 3] = (control >> 8) as u8;
        }

        /// A device with `vectors` MSI-X vectors and no other
        /// capability, the shape every virtio device here has.
        fn with_msix(vectors: u16) -> Self {
            let mut dev = Self::new();
            dev.set_status(PCI_STATUS_CAP_LIST);
            dev.set_cap_ptr(PCI_FIRST_CAP);
            dev.set_cap(PCI_FIRST_CAP, PCI_CAP_ID_MSIX, 0, vectors - 1);
            dev
        }
    }

    impl PciDevice for FakeDevice {
        fn cfg_read(&self, offset: u8, len: u8) -> u32 {
            let mut value = 0u32;
            for byte in 0..len.min(4) {
                let at = offset as usize + byte as usize;
                let read = self.space.get(at).copied().unwrap_or(0);
                value |= u32::from(read) << (byte * 8);
            }
            value
        }

        fn cfg_write(&self, _offset: u8, _len: u8, _val: u32) {}

        fn bar_rw(&self, _bar: BarN, _offset: usize, _rwo: RWOp<'_>) {}
    }

    #[test]
    fn reports_the_table_size_the_capability_encodes() {
        // Message Control holds the size less one. A walk that reads
        // the field unchanged reports one too few.
        assert_eq!(vector_count(&FakeDevice::with_msix(1)), Some(1));
        assert_eq!(vector_count(&FakeDevice::with_msix(3)), Some(3));
        assert_eq!(vector_count(&FakeDevice::with_msix(2048)), Some(2048));
    }

    #[test]
    fn enable_and_mask_bits_do_not_change_the_count() {
        // A guest driver that owns the device sets bits 15 and 14 of
        // the register that holds the size.
        let mut dev = FakeDevice::with_msix(3);
        dev.set_cap(PCI_FIRST_CAP, PCI_CAP_ID_MSIX, 0, 0xC000 | 2);

        assert_eq!(vector_count(&dev), Some(3));
    }

    #[test]
    fn finds_msix_behind_another_capability() {
        let mut dev = FakeDevice::new();
        dev.set_status(PCI_STATUS_CAP_LIST);
        dev.set_cap_ptr(0x50);
        // 0x10 is PCI Express, whose second word is not a table size.
        dev.set_cap(0x50, 0x10, 0x60, 0xFFFF);
        dev.set_cap(0x60, PCI_CAP_ID_MSIX, 0, 6);

        assert_eq!(vector_count(&dev), Some(7));
    }

    #[test]
    fn a_device_without_msix_reports_none() {
        let mut dev = FakeDevice::new();
        dev.set_status(PCI_STATUS_CAP_LIST);
        dev.set_cap_ptr(PCI_FIRST_CAP);
        dev.set_cap(PCI_FIRST_CAP, 0x05, 0, 0x0180); // MSI, not MSI-X

        assert_eq!(vector_count(&dev), None);
    }

    #[test]
    fn a_device_with_no_capability_list_reports_none() {
        let mut dev = FakeDevice::new();
        // A stale pointer with the status bit clear must not be walked.
        dev.set_cap_ptr(PCI_FIRST_CAP);
        dev.set_cap(PCI_FIRST_CAP, PCI_CAP_ID_MSIX, 0, 2);

        assert_eq!(vector_count(&dev), None);
    }

    #[test]
    fn a_pointer_below_the_first_capability_ends_the_walk() {
        let mut dev = FakeDevice::new();
        dev.set_status(PCI_STATUS_CAP_LIST);
        dev.set_cap_ptr(PCI_FIRST_CAP);
        // 0x00 is the end of a chain, not a capability.
        dev.set_cap(PCI_FIRST_CAP, 0x05, 0x00, 0x0180);
        // A vendor ID whose low byte is the MSI-X id. A walk that reads
        // offset 0 as a capability reports a table that is not there.
        dev.set_cap(0x00, PCI_CAP_ID_MSIX, 0, 9);

        assert_eq!(vector_count(&dev), None);
    }

    /// A cycle must end the walk, not block the caller.
    #[test]
    fn a_looping_chain_ends_the_walk() {
        let mut dev = FakeDevice::new();
        dev.set_status(PCI_STATUS_CAP_LIST);
        dev.set_cap_ptr(PCI_FIRST_CAP);
        dev.set_cap(PCI_FIRST_CAP, 0x05, 0x50, 0);
        dev.set_cap(0x50, 0x05, PCI_FIRST_CAP, 0);

        assert_eq!(vector_count(&dev), None);
    }

    /// The walk must find the table on a device that the binary builds.
    /// A hand-written space cannot check the real offsets of the
    /// capability pointer and header, or where `VirtioPciDevice` puts
    /// its MSI-X capability in the chain.
    #[test]
    fn reads_the_table_size_off_a_real_virtio_device() {
        use std::sync::Arc;
        use vmm_core::mem::PhysMap;
        use vmm_core::mmio::MmioBus;
        use vmm_core::pio::PioBus;
        use vmm_devices::pci::msix::{MsiSink, MsixTable};
        use vmm_virtio::{bits, VirtioPciDevice, VirtioRng};

        struct NullSink;
        impl MsiSink for NullSink {
            fn send(&self, _addr: u64, _data: u64) {}
        }

        // Not 2, so a helper that returns the virtio-rng count as a
        // constant fails here.
        const VECTORS: u16 = 5;

        let msix = Arc::new(MsixTable::new(VECTORS, Arc::new(NullSink)));
        let dev = VirtioPciDevice::new(
            VirtioRng::new(),
            bits::VIRTIO_DEV_TYPE_RNG,
            1,
            64,
            0,
            None,
            Arc::new(PhysMap::new()),
            Arc::new(PioBus::new()),
            Arc::new(MmioBus::new()),
            Some(msix),
        );

        assert_eq!(vector_count(dev.as_ref()), Some(VECTORS));
    }

    /// A device built with no MSI-X table reports none. The walk must
    /// not decode the first virtio capability as MSI-X.
    #[test]
    fn a_real_virtio_device_without_msix_reports_none() {
        use std::sync::Arc;
        use vmm_core::mem::PhysMap;
        use vmm_core::mmio::MmioBus;
        use vmm_core::pio::PioBus;
        use vmm_virtio::{bits, VirtioPciDevice, VirtioRng};

        let dev = VirtioPciDevice::new(
            VirtioRng::new(),
            bits::VIRTIO_DEV_TYPE_RNG,
            1,
            64,
            0,
            None,
            Arc::new(PhysMap::new()),
            Arc::new(PioBus::new()),
            Arc::new(MmioBus::new()),
            None,
        );

        assert_eq!(vector_count(dev.as_ref()), None);
    }
}
