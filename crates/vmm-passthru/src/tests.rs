// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The device driven through its guest entry points over a recorded
//! backend.

use std::io::{Error, ErrorKind, Result};
use std::sync::Mutex;

use super::*;

use vmm_core::common::{ReadOp, WriteOp};
use vmm_core::hdl::PptdevLimits;
use vmm_devices::pci::bits;

/// A device with a 16 KiB MMIO BAR0, a 256-byte I/O BAR1, an empty
/// BAR2 and BAR3, a 64-bit BAR4 whose high half is BAR5, and an MSI
/// capability that the host left enabled and pointed at itself.
struct FakePpt {
    cfg: Mutex<[u8; 256]>,
    bars: [Option<BarInfo>; BAR_COUNT],
    cfg_writes: Mutex<Vec<(u8, u8, u32)>>,
    maps: Mutex<Vec<(u64, u64, u64)>>,
    unmaps: Mutex<Vec<(u64, u64)>>,
    msi: Mutex<Vec<MsiProgram>>,
    bar_reads: Mutex<Vec<(usize, u32, u8)>>,
    bar_writes: Mutex<Vec<(usize, u32, u8, u32)>>,
    fail_map: bool,
}

const MSI_OFF: usize = 0x70;
const HOST_MSI_ADDR: u32 = 0xFEE0_3000;
const HOST_MSI_DATA: u16 = 0x00A5;

fn fake_cfg() -> [u8; 256] {
    let mut cfg = [0u8; 256];
    cfg[0..4].copy_from_slice(&[0x86, 0x80, 0x33, 0x15]);
    // Status: capability list.
    cfg[6] = 0x10;
    cfg[8..12].copy_from_slice(&[0x03, 0x00, 0x00, 0x02]);
    cfg[0x0C] = 0x10;
    cfg[0x0E] = bits::HEADER_TYPE_DEVICE;
    cfg[0x2C..0x30].copy_from_slice(&[0x86, 0x80, 0x01, 0x00]);
    // The host's ROM decode window.
    cfg[0x30..0x34].copy_from_slice(&0xFE00_0001u32.to_le_bytes());
    cfg[0x34] = MSI_OFF as u8;
    // Host IRQ line and pin A.
    cfg[0x3C] = 0x0B;
    cfg[0x3D] = 0x01;
    // MSI, 32-bit, enabled by the host with two vectors.
    cfg[MSI_OFF] = bits::CAP_ID_MSI;
    cfg[MSI_OFF + 1] = 0;
    let ctrl: u16 = MSI_MSG_CTRL_ENABLE | (1 << 4) | (3 << 1);
    cfg[MSI_OFF + 2..MSI_OFF + 4].copy_from_slice(&ctrl.to_le_bytes());
    cfg[MSI_OFF + 4..MSI_OFF + 8].copy_from_slice(&HOST_MSI_ADDR.to_le_bytes());
    cfg[MSI_OFF + 8..MSI_OFF + 10]
        .copy_from_slice(&HOST_MSI_DATA.to_le_bytes());
    cfg
}

fn fake_bars() -> [Option<BarInfo>; BAR_COUNT] {
    [
        Some(BarInfo {
            bar_type: PCI_ADDR_MEM32,
            hpa: 0xF000_0000,
            size: 0x4000,
        }),
        Some(BarInfo {
            bar_type: PCI_ADDR_IO,
            hpa: 0x3000,
            size: 0x100,
        }),
        None,
        None,
        Some(BarInfo {
            bar_type: PCI_ADDR_MEM64,
            hpa: 0x38_0000_0000,
            size: 0x10_0000,
        }),
        None,
    ]
}

impl FakePpt {
    fn new() -> Self {
        Self {
            cfg: Mutex::new(fake_cfg()),
            bars: fake_bars(),
            cfg_writes: Mutex::new(Vec::new()),
            maps: Mutex::new(Vec::new()),
            unmaps: Mutex::new(Vec::new()),
            msi: Mutex::new(Vec::new()),
            bar_reads: Mutex::new(Vec::new()),
            bar_writes: Mutex::new(Vec::new()),
            fail_map: false,
        }
    }
}

/// The backend outlives the device in a test, so the device holds a
/// reference and the test keeps the recorder.
struct Shared(Arc<FakePpt>);

impl PptOps for Shared {
    fn cfg_read(&self, offset: u8, width: u8) -> Result<u32> {
        let cfg = self.0.cfg.lock().unwrap();
        let mut val = 0u32;
        for i in 0..usize::from(width) {
            let byte = cfg.get(usize::from(offset) + i).copied().unwrap_or(0);
            val |= u32::from(byte) << (i * 8);
        }
        Ok(val)
    }

    fn cfg_write(&self, offset: u8, width: u8, data: u32) -> Result<()> {
        self.0
            .cfg_writes
            .lock()
            .unwrap()
            .push((offset, width, data));
        let mut cfg = self.0.cfg.lock().unwrap();
        for (i, byte) in
            data.to_le_bytes().iter().enumerate().take(width.into())
        {
            if let Some(slot) = cfg.get_mut(usize::from(offset) + i) {
                *slot = *byte;
            }
        }
        Ok(())
    }

    fn bar_query(&self, idx: usize) -> Result<Option<BarInfo>> {
        Ok(self.0.bars[idx])
    }

    fn bar_read(&self, bar: usize, offset: u32, width: u8) -> Result<u32> {
        self.0.bar_reads.lock().unwrap().push((bar, offset, width));
        Ok(0x1234_5678)
    }

    fn bar_write(
        &self,
        bar: usize,
        offset: u32,
        width: u8,
        data: u32,
    ) -> Result<()> {
        self.0
            .bar_writes
            .lock()
            .unwrap()
            .push((bar, offset, width, data));
        Ok(())
    }

    fn limits(&self) -> Result<PptdevLimits> {
        Ok(PptdevLimits { msi: 4, msix: 0 })
    }

    fn map_mmio(&self, gpa: u64, hpa: u64, len: u64) -> Result<()> {
        if self.0.fail_map {
            return Err(Error::from(ErrorKind::OutOfMemory));
        }
        self.0.maps.lock().unwrap().push((gpa, hpa, len));
        Ok(())
    }

    fn unmap_mmio(&self, gpa: u64, len: u64) -> Result<()> {
        self.0.unmaps.lock().unwrap().push((gpa, len));
        Ok(())
    }

    fn setup_msi(&self, addr: u64, data: u64, numvec: i32) -> Result<()> {
        self.0
            .msi
            .lock()
            .unwrap()
            .push(MsiProgram { addr, data, numvec });
        Ok(())
    }
}

fn null_log() -> Logger {
    Logger::root(slog::Discard, slog::o!())
}

fn build(fake: FakePpt) -> (Arc<PciPassthru>, Arc<FakePpt>, Arc<PioBus>) {
    let fake = Arc::new(fake);
    let bus = Arc::new(PioBus::new());
    let dev = PciPassthru::attach(
        Box::new(Shared(Arc::clone(&fake))),
        Arc::clone(&bus),
        null_log(),
    )
    .expect("attach");
    (dev, fake, bus)
}

fn write(dev: &PciPassthru, off: u8, len: u8, val: u32) {
    dev.cfg_write(off, len, val);
}

fn read(dev: &PciPassthru, off: u8, len: u8) -> u32 {
    dev.cfg_read(off, len)
}

#[test]
fn empty_and_high_half_slots_do_not_abort_construction() {
    let (dev, _, _) = build(FakePpt::new());
    assert_eq!(read(&dev, 0x00, 4), 0x1533_8086);
    // BAR2, BAR3 and the high half read as an empty slot or the
    // high dword, never as an error.
    assert_eq!(read(&dev, 0x18, 4), 0);
    assert_eq!(read(&dev, 0x1C, 4), 0);
    assert_eq!(read(&dev, 0x20, 4) & 0x7, bits::BAR_TYPE_MEM64);
    assert_eq!(read(&dev, 0x24, 4), 0);
}

#[test]
fn a_bar_query_error_other_than_enoent_fails_construction() {
    struct Broken(Shared);
    impl PptOps for Broken {
        fn cfg_read(&self, o: u8, w: u8) -> Result<u32> {
            self.0.cfg_read(o, w)
        }
        fn cfg_write(&self, o: u8, w: u8, d: u32) -> Result<()> {
            self.0.cfg_write(o, w, d)
        }
        fn bar_query(&self, idx: usize) -> Result<Option<BarInfo>> {
            if idx == 2 {
                Err(Error::from(ErrorKind::PermissionDenied))
            } else {
                self.0.bar_query(idx)
            }
        }
        fn bar_read(&self, b: usize, o: u32, w: u8) -> Result<u32> {
            self.0.bar_read(b, o, w)
        }
        fn bar_write(&self, b: usize, o: u32, w: u8, d: u32) -> Result<()> {
            self.0.bar_write(b, o, w, d)
        }
        fn limits(&self) -> Result<PptdevLimits> {
            self.0.limits()
        }
        fn map_mmio(&self, g: u64, h: u64, l: u64) -> Result<()> {
            self.0.map_mmio(g, h, l)
        }
        fn unmap_mmio(&self, g: u64, l: u64) -> Result<()> {
            self.0.unmap_mmio(g, l)
        }
        fn setup_msi(&self, a: u64, d: u64, n: i32) -> Result<()> {
            self.0.setup_msi(a, d, n)
        }
    }
    let err = PciPassthru::attach(
        Box::new(Broken(Shared(Arc::new(FakePpt::new())))),
        Arc::new(PioBus::new()),
        null_log(),
    )
    .err()
    .expect("construction must fail");
    assert_eq!(err.kind(), ErrorKind::PermissionDenied);
}

#[test]
fn sub_page_mmio_bar_is_refused_at_bind() {
    let mut fake = FakePpt::new();
    fake.bars[0] = Some(BarInfo {
        bar_type: PCI_ADDR_MEM32,
        hpa: 0xF000_0000,
        size: 0x100,
    });
    let err = PciPassthru::attach(
        Box::new(Shared(Arc::new(fake))),
        Arc::new(PioBus::new()),
        null_log(),
    )
    .err()
    .expect("a sub-page BAR must be refused");
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert!(err.to_string().contains("BAR0"), "{err}");
}

#[test]
fn msi_capability_writes_never_reach_the_hardware() {
    let (dev, fake, _) = build(FakePpt::new());
    write(&dev, 0x74, 4, 0xFEE0_1000);
    write(&dev, 0x78, 2, 0x0031);
    write(&dev, 0x72, 2, u32::from(MSI_MSG_CTRL_ENABLE | (1 << 4)));
    write(&dev, 0x70, 4, 0xFFFF_FFFF);

    let writes = fake.cfg_writes.lock().unwrap();
    assert!(
        writes.iter().all(|(off, _, _)| !(0x70..0x7C).contains(off)),
        "MSI block written to hardware: {writes:x?}",
    );
    let cfg = fake.cfg.lock().unwrap();
    assert_eq!(
        u32::from_le_bytes(cfg[0x74..0x78].try_into().unwrap()),
        HOST_MSI_ADDR,
        "host address untouched",
    );
}

#[test]
fn msi_setup_uses_the_shadow_values() {
    let (dev, fake, _) = build(FakePpt::new());
    write(&dev, 0x74, 4, 0xFEE0_1000);
    write(&dev, 0x78, 2, 0x0031);
    assert!(fake.msi.lock().unwrap().is_empty(), "disabled: no call");

    write(&dev, 0x72, 2, u32::from(MSI_MSG_CTRL_ENABLE | (1 << 4)));
    assert_eq!(
        fake.msi.lock().unwrap().as_slice(),
        &[MsiProgram {
            addr: 0xFEE0_1000,
            data: 0x31,
            numvec: 2
        }]
    );

    // The same value again is not reprogrammed.
    write(&dev, 0x72, 2, u32::from(MSI_MSG_CTRL_ENABLE | (1 << 4)));
    assert_eq!(fake.msi.lock().unwrap().len(), 1);

    // Multiple Message Enable past the kernel limit is clamped.
    write(&dev, 0x72, 2, u32::from(MSI_MSG_CTRL_ENABLE | (5 << 4)));
    assert_eq!(fake.msi.lock().unwrap().last().unwrap().numvec, 4);

    write(&dev, 0x72, 2, 0);
    assert_eq!(fake.msi.lock().unwrap().last().unwrap().numvec, 0);
}

#[test]
fn msi_capability_reads_come_from_the_shadow() {
    let (dev, _, _) = build(FakePpt::new());
    assert_eq!(read(&dev, 0x70, 1), u32::from(bits::CAP_ID_MSI));
    let ctrl = read(&dev, 0x72, 2) as u16;
    assert_eq!(ctrl & MSI_MSG_CTRL_ENABLE, 0, "host enable hidden");
    assert_eq!((ctrl >> 1) & 0x7, 2, "capable count clamped to limit 4");
    assert_ne!(read(&dev, 0x74, 4), HOST_MSI_ADDR, "host address hidden");
    assert_ne!(
        read(&dev, 0x78, 2),
        u32::from(HOST_MSI_DATA),
        "host vector hidden"
    );

    write(&dev, 0x74, 4, 0xFEE0_1000);
    assert_eq!(read(&dev, 0x74, 4), 0xFEE0_1000);
    assert_eq!(read(&dev, 0x76, 2), 0xFEE0);
}

#[test]
fn rom_bar_and_interrupt_line_are_emulated() {
    let (dev, _, _) = build(FakePpt::new());
    assert_eq!(read(&dev, 0x30, 4), 0, "host ROM address hidden");
    assert_eq!(read(&dev, 0x3C, 1), 0x0B, "line starts at the host value");
    assert_eq!(read(&dev, 0x3D, 1), 0x01, "pin from hardware");
    write(&dev, 0x3C, 1, 0x05);
    assert_eq!(
        read(&dev, 0x3C, 1),
        0x05,
        "line reads back what was written"
    );
    assert_eq!(read(&dev, 0x3C, 4) & 0xFFFF, 0x0105);
}

/// PCI makes the header type read-only, and the multi-function bit
/// belongs to the slot: `PciBus` ORs it into the read because only the
/// bus knows which functions a slot holds. A guest write here must not
/// reach the device or the hardware.
#[test]
fn the_header_type_is_read_only() {
    let (dev, fake, _) = build(FakePpt::new());
    assert_eq!(read(&dev, 0x0E, 1), u32::from(bits::HEADER_TYPE_DEVICE));
    write(&dev, 0x0E, 1, u32::from(bits::HEADER_TYPE_MULTIFUNC));
    assert_eq!(
        read(&dev, 0x0E, 1),
        u32::from(bits::HEADER_TYPE_DEVICE),
        "a guest write changed the header type",
    );
    assert_eq!(read(&dev, 0x0C, 1), 0x10, "cache line from hardware");
    assert!(fake.cfg_writes.lock().unwrap().is_empty());
}

#[test]
fn mmio_bar_is_mapped_only_while_decode_is_enabled() {
    let (dev, fake, _) = build(FakePpt::new());
    write(&dev, 0x10, 4, 0xC000_0000);
    assert!(fake.maps.lock().unwrap().is_empty(), "MMIO_EN clear");

    write(&dev, 0x04, 2, u32::from(PCI_CMD_MMIO_EN));
    assert_eq!(
        fake.maps.lock().unwrap().as_slice(),
        &[(0xC000_0000, 0xF000_0000, 0x4000)]
    );
    assert_eq!(fake.cfg_writes.lock().unwrap().as_slice(), &[(0x04, 2, 2)]);

    // A move unmaps the old window and maps the new one.
    write(&dev, 0x10, 4, 0xC100_0000);
    assert_eq!(
        fake.unmaps.lock().unwrap().as_slice(),
        &[(0xC000_0000, 0x4000)]
    );
    assert_eq!(
        fake.maps.lock().unwrap().last(),
        Some(&(0xC100_0000, 0xF000_0000, 0x4000))
    );

    write(&dev, 0x04, 2, 0);
    assert_eq!(
        fake.unmaps.lock().unwrap().last(),
        Some(&(0xC100_0000, 0x4000))
    );
}

#[test]
fn sixty_four_bit_bar_maps_with_its_high_half() {
    let (dev, fake, _) = build(FakePpt::new());
    write(&dev, 0x04, 2, u32::from(PCI_CMD_MMIO_EN));
    write(&dev, 0x20, 4, 0x0000_0000);
    write(&dev, 0x24, 4, 0x0000_0010);
    assert_eq!(
        fake.maps.lock().unwrap().as_slice(),
        &[(0x10_0000_0000, 0x38_0000_0000, 0x10_0000)]
    );
}

#[test]
fn sub_dword_bar_write_merges_into_the_register() {
    let (dev, _, _) = build(FakePpt::new());
    write(&dev, 0x10, 4, 0xC000_0000);
    // A byte write to the third byte must keep the rest of the BAR.
    write(&dev, 0x12, 1, 0x40);
    assert_eq!(read(&dev, 0x10, 4), 0xC040_0000);
    write(&dev, 0x12, 2, 0xC1FF);
    assert_eq!(read(&dev, 0x10, 4), 0xC1FF_0000 & !0x3FFF);
}

#[test]
fn a_failed_map_is_not_recorded_as_a_window() {
    let mut fake = FakePpt::new();
    fake.fail_map = true;
    let (dev, fake, _) = build(fake);
    write(&dev, 0x10, 4, 0xC000_0000);
    write(&dev, 0x04, 2, u32::from(PCI_CMD_MMIO_EN));
    write(&dev, 0x04, 2, 0);
    dev.detach_regions();
    assert!(fake.unmaps.lock().unwrap().is_empty(), "nothing to unmap");
}

#[test]
fn io_bar_is_registered_on_the_pio_bus_and_relayed() {
    let (dev, fake, bus) = build(FakePpt::new());
    write(&dev, 0x14, 4, 0xC000);
    assert_eq!(bus.handle_in(0xC010, 4), 0xFFFF_FFFF, "IO_EN clear");

    write(&dev, 0x04, 2, u32::from(PCI_CMD_IO_EN));
    assert_eq!(bus.handle_in(0xC010, 4), 0x1234_5678);
    assert_eq!(fake.bar_reads.lock().unwrap().as_slice(), &[(1, 0x10, 4)]);
    bus.handle_out(0xC0FC, 2, 0xBEEF);
    assert_eq!(
        fake.bar_writes.lock().unwrap().as_slice(),
        &[(1, 0xFC, 2, 0xBEEF)]
    );

    // An access that runs past the BAR is not relayed.
    assert_eq!(bus.handle_in(0xC0FE, 4), 0xFFFF_FFFF);
    bus.handle_out(0xC0FE, 4, 1);
    assert_eq!(fake.bar_reads.lock().unwrap().len(), 1);
    assert_eq!(fake.bar_writes.lock().unwrap().len(), 1);

    // Moving the BAR moves the registration.
    write(&dev, 0x14, 4, 0xD000);
    assert_eq!(bus.handle_in(0xC010, 4), 0xFFFF_FFFF);
    assert_eq!(bus.handle_in(0xD010, 4), 0x1234_5678);

    dev.detach_regions();
    assert_eq!(bus.handle_in(0xD010, 4), 0xFFFF_FFFF);
}

#[test]
fn pio_handler_does_not_keep_the_device_alive() {
    let (dev, _, bus) = build(FakePpt::new());
    write(&dev, 0x14, 4, 0xC000);
    write(&dev, 0x04, 2, u32::from(PCI_CMD_IO_EN));
    let weak = Arc::downgrade(&dev);
    drop(dev);
    assert!(weak.upgrade().is_none());
    assert_eq!(bus.handle_in(0xC010, 4), 0xFFFF_FFFF);
}

#[test]
fn a_dword_write_at_the_command_register_drops_the_status_half() {
    let (dev, fake, _) = build(FakePpt::new());
    write(&dev, 0x04, 4, 0xFFFF_0006);
    assert_eq!(fake.cfg_writes.lock().unwrap().as_slice(), &[(0x04, 2, 6)]);
    assert_eq!(read(&dev, 0x04, 2), 6);
    assert_eq!(read(&dev, 0x06, 2), 0x10, "status from hardware");
}

#[test]
fn bar_rw_on_an_io_bar_relays_each_width() {
    let (dev, fake, _) = build(FakePpt::new());
    let mut ro = ReadOp::new(1);
    dev.bar_rw(BarN::BAR1, 3, RWOp::Read(&mut ro));
    assert_eq!(ro.buf(), &[0x78]);
    let wo = WriteOp::from_buf(&[0xAA, 0xBB]);
    dev.bar_rw(BarN::BAR1, 4, RWOp::Write(&wo));
    assert_eq!(fake.bar_reads.lock().unwrap().as_slice(), &[(1, 3, 1)]);
    assert_eq!(
        fake.bar_writes.lock().unwrap().as_slice(),
        &[(1, 4, 2, 0xBBAA)]
    );

    // An MMIO BAR never reaches here. If one did, it reads as undecoded.
    let mut ro = ReadOp::new(2);
    dev.bar_rw(BarN::BAR0, 0, RWOp::Read(&mut ro));
    assert_eq!(ro.buf(), &[0xFF, 0xFF]);
}

#[test]
fn merge_and_extract_are_inverse_at_every_offset() {
    let dword = 0x4433_2211u32;
    assert_eq!(extract_bytes(dword, 0x01, 1), 0x22);
    assert_eq!(extract_bytes(dword, 0x02, 2), 0x4433);
    assert_eq!(merge_bytes(dword, 0x01, 1, 0xEE), 0x4433_EE11);
    assert_eq!(merge_bytes(dword, 0x02, 2, 0xBEEF), 0xBEEF_2211);
    assert_eq!(merge_bytes(dword, 0x00, 4, 0), 0);
}
