// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PCI bus with 32 device slots and 8 functions per slot.
//!
//! Empty slots read as all-ones, the x86 bus float that UEFI relies on
//! when it scans for devices.

use std::sync::{Arc, Mutex};

use thiserror::Error;

use super::bits;
use super::device::PciDevice;
use super::Bdf;

const SLOTS_PER_BUS: usize = 32;
const FUNCS_PER_SLOT: usize = 8;

/// The only bus number. There is no PCI-to-PCI bridge, so bus 0 is the
/// whole topology.
const ROOT_BUS: u8 = 0;

/// The value an unclaimed config address returns: the bus floats high.
const fn bus_float(len: u8) -> u32 {
    match len {
        1 => 0xFF,
        2 => 0xFFFF,
        _ => 0xFFFF_FFFF,
    }
}

type PciSlot = [Option<Arc<dyn PciDevice>>; FUNCS_PER_SLOT];
type PciSlots = [PciSlot; SLOTS_PER_BUS];

/// Reason an attach request was refused.
#[derive(Debug, Error)]
pub enum AttachError {
    /// Another device holds this BDF.
    #[error("device already attached at BDF {0}")]
    SlotOccupied(Bdf),
    /// The BDF names a bus this machine does not have.
    #[error("no such PCI bus for BDF {0}")]
    NoSuchBus(Bdf),
}

pub struct PciBus {
    inner: Mutex<PciBusInner>,
}

struct PciBusInner {
    /// 32 slots x 8 functions.
    slots: Box<PciSlots>,
}

/// Where the header-type byte sits in the value a read of `offset` for
/// `len` bytes answers with, or `None` when the access misses it.
fn header_type_shift(offset: u8, len: u8) -> Option<u32> {
    let last = offset.checked_add(len)?;
    (offset <= bits::REG_HEADER_TYPE && bits::REG_HEADER_TYPE < last)
        .then(|| u32::from(bits::REG_HEADER_TYPE - offset) * 8)
}

impl PciBus {
    pub fn new() -> Arc<Self> {
        // Boxed to keep 32 * 8 slots off the stack.
        let slots =
            Box::new(std::array::from_fn(|_| std::array::from_fn(|_| None)));
        Arc::new(Self {
            inner: Mutex::new(PciBusInner { slots }),
        })
    }

    /// Attach a PCI device at the given bus/device/function address.
    ///
    /// # Panics
    ///
    /// Panics if a device is already attached at this BDF. Use
    /// [`try_attach`](Self::try_attach) when the BDF comes from a
    /// request at run time.
    pub fn attach(&self, bdf: Bdf, dev: Arc<dyn PciDevice>) {
        if let Err(e) = self.try_attach(bdf, dev) {
            panic!("{e}");
        }
    }

    /// Attach a PCI device, or return an error if the BDF is occupied.
    pub fn try_attach(
        &self,
        bdf: Bdf,
        dev: Arc<dyn PciDevice>,
    ) -> Result<(), AttachError> {
        if bdf.bus() != ROOT_BUS {
            return Err(AttachError::NoSuchBus(bdf));
        }
        let mut inner = self.inner.lock().expect("PciBus lock poisoned");
        let slot = &mut inner.slots[bdf.dev() as usize][bdf.func() as usize];
        if slot.is_some() {
            return Err(AttachError::SlotOccupied(bdf));
        }
        *slot = Some(dev);
        Ok(())
    }

    /// Remove and return the device at `bdf`. `None` if the slot is
    /// empty.
    pub fn detach(&self, bdf: &Bdf) -> Option<Arc<dyn PciDevice>> {
        if bdf.bus() != ROOT_BUS {
            return None;
        }
        let mut inner = self.inner.lock().expect("PciBus lock poisoned");
        inner.slots[bdf.dev() as usize][bdf.func() as usize].take()
    }

    /// Read from PCI configuration space.
    ///
    /// Empty slots float to all-ones, which UEFI expects when it scans.
    /// A bus other than [`ROOT_BUS`] floats too. Otherwise each of the
    /// 256 bus numbers answers with a copy of bus 0.
    pub fn config_read(&self, bdf: &Bdf, offset: u8, len: u8) -> u32 {
        if bdf.bus() != ROOT_BUS {
            return bus_float(len);
        }
        let inner = self.inner.lock().expect("PciBus lock poisoned");
        let slot = &inner.slots[bdf.dev() as usize];
        let Some(dev) = slot[bdf.func() as usize].clone() else {
            return bus_float(len);
        };
        // The slot owns the multi-function bit, not the device: only the
        // bus knows which functions are populated, and PCI makes the
        // header type read-only to the guest.
        let multifunc =
            bdf.func() == 0 && slot[1..].iter().any(Option::is_some);
        drop(inner);

        let val = dev.cfg_read(offset, len);
        match header_type_shift(offset, len).filter(|_| multifunc) {
            Some(shift) => {
                val | (u32::from(bits::HEADER_TYPE_MULTIFUNC) << shift)
            }
            None => val,
        }
    }

    /// Write to PCI configuration space. Writes to empty slots are
    /// dropped.
    pub fn config_write(&self, bdf: &Bdf, offset: u8, len: u8, val: u32) {
        if bdf.bus() != ROOT_BUS {
            return;
        }
        let inner = self.inner.lock().expect("PciBus lock poisoned");
        if let Some(dev) = &inner.slots[bdf.dev() as usize][bdf.func() as usize]
        {
            let dev = Arc::clone(dev);
            drop(inner);
            dev.cfg_write(offset, len, val);
        }
    }

    pub fn device_at(&self, bdf: &Bdf) -> Option<Arc<dyn PciDevice>> {
        if bdf.bus() != ROOT_BUS {
            return None;
        }
        let inner = self.inner.lock().expect("PciBus lock poisoned");
        inner.slots[bdf.dev() as usize][bdf.func() as usize].clone()
    }
}

impl Default for PciBus {
    fn default() -> Self {
        let slots =
            Box::new(std::array::from_fn(|_| std::array::from_fn(|_| None)));
        Self {
            inner: Mutex::new(PciBusInner { slots }),
        }
    }
}

impl std::fmt::Debug for PciBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PciBus").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmm_core::common::RWOp;

    /// Minimal test device that responds with its vendor/device ID.
    struct TestDevice {
        vendor_id: u16,
        device_id: u16,
    }

    impl PciDevice for TestDevice {
        fn cfg_read(&self, offset: u8, len: u8) -> u32 {
            match (offset, len) {
                (0x00, 4) => {
                    u32::from(self.vendor_id)
                        | (u32::from(self.device_id) << 16)
                }
                (0x00, 2) => u32::from(self.vendor_id),
                (0x02, 2) => u32::from(self.device_id),
                _ => 0,
            }
        }

        fn cfg_write(&self, _offset: u8, _len: u8, _val: u32) {}

        fn bar_rw(&self, _bar: BarN, _offset: usize, _rwo: RWOp<'_>) {}
    }

    use super::super::BarN;

    /// Test device whose header type answers like a `DeviceState`: byte
    /// 0x0E of the dword at 0x0C, and read-only.
    struct HeaderDevice {
        writes: Mutex<Vec<(u8, u32)>>,
    }

    impl HeaderDevice {
        fn new() -> Self {
            Self {
                writes: Mutex::new(Vec::new()),
            }
        }

        fn writes(&self) -> Vec<(u8, u32)> {
            self.writes.lock().expect("write log poisoned").clone()
        }
    }

    impl PciDevice for HeaderDevice {
        fn cfg_read(&self, offset: u8, len: u8) -> u32 {
            match header_type_shift(offset, len) {
                Some(shift) => u32::from(bits::HEADER_TYPE_DEVICE) << shift,
                None => 0,
            }
        }

        fn cfg_write(&self, offset: u8, _len: u8, val: u32) {
            self.writes
                .lock()
                .expect("write log poisoned")
                .push((offset, val));
        }

        fn bar_rw(&self, _bar: BarN, _offset: usize, _rwo: RWOp<'_>) {}
    }

    fn header_type_of(bus: &PciBus, bdf: &Bdf) -> u8 {
        bus.config_read(bdf, bits::REG_HEADER_TYPE, 1) as u8
    }

    #[test]
    fn empty_slot_returns_all_ones() {
        let bus = PciBus::new();
        let bdf = Bdf::new_unchecked(0, 5, 0);
        assert_eq!(bus.config_read(&bdf, 0x00, 4), 0xFFFF_FFFF);
        assert_eq!(bus.config_read(&bdf, 0x00, 2), 0xFFFF);
        assert_eq!(bus.config_read(&bdf, 0x00, 1), 0xFF);
    }

    #[test]
    fn all_slots_empty_on_creation() {
        let bus = PciBus::new();
        for dev in 0..32u8 {
            for func in 0..8u8 {
                let bdf = Bdf::new_unchecked(0, dev, func);
                assert_eq!(
                    bus.config_read(&bdf, 0x00, 4),
                    0xFFFF_FFFF,
                    "expected bus float at {}.{}.{}",
                    0,
                    dev,
                    func,
                );
            }
        }
    }

    #[test]
    fn attach_and_read() {
        let bus = PciBus::new();
        let dev: Arc<dyn PciDevice> = Arc::new(TestDevice {
            vendor_id: 0x8086,
            device_id: 0x1237,
        });
        let bdf = Bdf::new_unchecked(0, 0, 0);
        bus.attach(bdf, dev);

        let val = bus.config_read(&bdf, 0x00, 4);
        assert_eq!(val & 0xFFFF, 0x8086);
        assert_eq!((val >> 16) & 0xFFFF, 0x1237);
    }

    #[test]
    fn write_to_empty_slot_is_silent() {
        let bus = PciBus::new();
        let bdf = Bdf::new_unchecked(0, 10, 0);
        bus.config_write(&bdf, 0x04, 2, 0x0003);
    }

    #[test]
    fn device_at_returns_some_for_attached() {
        let bus = PciBus::new();
        let bdf = Bdf::new_unchecked(0, 2, 0);
        let dev: Arc<dyn PciDevice> = Arc::new(TestDevice {
            vendor_id: 0x1234,
            device_id: 0x5678,
        });
        bus.attach(bdf, dev);
        assert!(bus.device_at(&bdf).is_some());
    }

    #[test]
    fn device_at_returns_none_for_empty() {
        let bus = PciBus::new();
        let bdf = Bdf::new_unchecked(0, 3, 0);
        assert!(bus.device_at(&bdf).is_none());
    }

    #[test]
    #[should_panic(expected = "already attached")]
    fn double_attach_panics() {
        let bus = PciBus::new();
        let bdf = Bdf::new_unchecked(0, 0, 0);
        let dev1: Arc<dyn PciDevice> = Arc::new(TestDevice {
            vendor_id: 0x1111,
            device_id: 0x2222,
        });
        let dev2: Arc<dyn PciDevice> = Arc::new(TestDevice {
            vendor_id: 0x3333,
            device_id: 0x4444,
        });
        bus.attach(bdf, dev1);
        bus.attach(bdf, dev2);
    }

    #[test]
    fn try_attach_occupied_keeps_first_device() {
        let bus = PciBus::new();
        let bdf = Bdf::new_unchecked(0, 0, 0);
        let dev1: Arc<dyn PciDevice> = Arc::new(TestDevice {
            vendor_id: 0x1111,
            device_id: 0x2222,
        });
        let dev2: Arc<dyn PciDevice> = Arc::new(TestDevice {
            vendor_id: 0x3333,
            device_id: 0x4444,
        });
        bus.try_attach(bdf, dev1).expect("first attach");

        let err = bus.try_attach(bdf, dev2).expect_err("slot is occupied");
        assert!(matches!(err, AttachError::SlotOccupied(b) if b == bdf));
        assert_eq!(bus.config_read(&bdf, 0x00, 2), 0x1111);
    }

    #[test]
    fn detach_returns_the_device() {
        let bus = PciBus::new();
        let bdf = Bdf::new_unchecked(0, 6, 0);
        let dev: Arc<dyn PciDevice> = Arc::new(TestDevice {
            vendor_id: 0x1AF4,
            device_id: 0x1000,
        });
        bus.attach(bdf, Arc::clone(&dev));

        let removed = bus.detach(&bdf).expect("device was attached");
        assert!(Arc::ptr_eq(&removed, &dev));
        assert!(bus.device_at(&bdf).is_none());
        assert_eq!(bus.config_read(&bdf, 0x00, 4), 0xFFFF_FFFF);
    }

    #[test]
    fn detach_empty_slot_returns_none() {
        let bus = PciBus::new();
        let bdf = Bdf::new_unchecked(0, 7, 0);
        assert!(bus.detach(&bdf).is_none());
    }

    #[test]
    fn attach_at_func1_sets_multifunc() {
        let bus = PciBus::new();
        let bdf0 = Bdf::new_unchecked(0, 8, 0);
        let func0 = Arc::new(HeaderDevice::new());
        bus.attach(bdf0, func0.clone());
        assert_eq!(header_type_of(&bus, &bdf0), bits::HEADER_TYPE_DEVICE);

        bus.attach(Bdf::new_unchecked(0, 8, 1), Arc::new(HeaderDevice::new()));
        assert_eq!(
            header_type_of(&bus, &bdf0) & bits::HEADER_TYPE_MULTIFUNC,
            bits::HEADER_TYPE_MULTIFUNC
        );
        // The bit is the bus's, so no config write reached the device.
        assert!(func0.writes().is_empty());
    }

    #[test]
    fn multifunc_shows_at_every_access_width_that_covers_the_byte() {
        let bus = PciBus::new();
        let bdf0 = Bdf::new_unchecked(0, 11, 0);
        bus.attach(bdf0, Arc::new(HeaderDevice::new()));
        bus.attach(Bdf::new_unchecked(0, 11, 1), Arc::new(HeaderDevice::new()));

        let expect =
            u32::from(bits::HEADER_TYPE_DEVICE | bits::HEADER_TYPE_MULTIFUNC);
        assert_eq!(bus.config_read(&bdf0, 0x0C, 4), expect << 16);
        assert_eq!(bus.config_read(&bdf0, 0x0E, 2), expect);
        assert_eq!(bus.config_read(&bdf0, 0x0E, 1), expect);
        // An access that stops short of byte 0x0E is untouched.
        assert_eq!(bus.config_read(&bdf0, 0x0C, 2), 0);
    }

    #[test]
    fn detach_of_last_upper_func_clears_multifunc() {
        let bus = PciBus::new();
        let bdf0 = Bdf::new_unchecked(0, 9, 0);
        let bdf1 = Bdf::new_unchecked(0, 9, 1);
        let bdf2 = Bdf::new_unchecked(0, 9, 2);
        bus.attach(bdf0, Arc::new(HeaderDevice::new()));
        bus.attach(bdf1, Arc::new(HeaderDevice::new()));
        bus.attach(bdf2, Arc::new(HeaderDevice::new()));

        // One function > 0 remains, so the slot is still multi-function.
        bus.detach(&bdf2).expect("func 2 was attached");
        assert_eq!(
            header_type_of(&bus, &bdf0) & bits::HEADER_TYPE_MULTIFUNC,
            bits::HEADER_TYPE_MULTIFUNC
        );

        bus.detach(&bdf1).expect("func 1 was attached");
        assert_eq!(header_type_of(&bus, &bdf0), bits::HEADER_TYPE_DEVICE);
    }

    #[test]
    fn multiple_devices_independent() {
        let bus = PciBus::new();
        let bdf0 = Bdf::new_unchecked(0, 0, 0);
        let bdf1 = Bdf::new_unchecked(0, 1, 0);

        let dev0: Arc<dyn PciDevice> = Arc::new(TestDevice {
            vendor_id: 0xAAAA,
            device_id: 0xBBBB,
        });
        let dev1: Arc<dyn PciDevice> = Arc::new(TestDevice {
            vendor_id: 0xCCCC,
            device_id: 0xDDDD,
        });
        bus.attach(bdf0, dev0);
        bus.attach(bdf1, dev1);

        assert_eq!(bus.config_read(&bdf0, 0x00, 2), 0xAAAA);
        assert_eq!(bus.config_read(&bdf1, 0x00, 2), 0xCCCC);

        let bdf_empty = Bdf::new_unchecked(0, 2, 0);
        assert_eq!(bus.config_read(&bdf_empty, 0x00, 4), 0xFFFF_FFFF);
    }

    /// A guest walks all 256 bus numbers. Only bus 0 may answer, or the
    /// guest finds a copy of bus 0 behind every one of them.
    #[test]
    fn config_space_does_not_alias_onto_another_bus() {
        let bus = PciBus::new();
        let dev: Arc<dyn PciDevice> = Arc::new(TestDevice {
            vendor_id: 0x8086,
            device_id: 0x2821,
        });
        bus.attach(Bdf::new_unchecked(0, 3, 0), dev);
        assert_eq!(
            bus.config_read(&Bdf::new_unchecked(0, 3, 0), 0x00, 2),
            0x8086
        );

        for b in 1..=u8::MAX {
            let bdf = Bdf::new_unchecked(b, 3, 0);
            assert_eq!(bus.config_read(&bdf, 0x00, 4), 0xFFFF_FFFF);
            assert_eq!(bus.config_read(&bdf, 0x00, 2), 0xFFFF);
            assert_eq!(bus.config_read(&bdf, 0x00, 1), 0xFF);
            assert!(bus.device_at(&bdf).is_none());
        }
    }

    #[test]
    fn a_config_write_to_another_bus_is_dropped() {
        let bus = PciBus::new();
        let dev = Arc::new(HeaderDevice::new());
        bus.attach(Bdf::new_unchecked(0, 4, 0), dev.clone());

        bus.config_write(
            &Bdf::new_unchecked(7, 4, 0),
            bits::REG_HEADER_TYPE,
            1,
            0x5A,
        );

        assert!(dev.writes().is_empty());
    }

    #[test]
    fn attach_to_another_bus_is_refused() {
        let bus = PciBus::new();
        let dev: Arc<dyn PciDevice> = Arc::new(TestDevice {
            vendor_id: 0x1234,
            device_id: 0x5678,
        });
        let bdf = Bdf::new_unchecked(1, 4, 0);
        let err = bus.try_attach(bdf, dev).expect_err("bus 1 does not exist");
        assert!(matches!(err, AttachError::NoSuchBus(b) if b == bdf));
        assert!(bus.device_at(&Bdf::new_unchecked(0, 4, 0)).is_none());
    }
}
