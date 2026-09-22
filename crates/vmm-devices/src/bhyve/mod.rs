// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Thin wrappers around bhyve kernel-emulated devices.
//!
//! The bhyve kernel VMM emulates these devices and handles all their
//! PIO. The userspace wrappers take part in lifecycle coordination
//! (pause and resume during migration) and set the PM timer port with
//! VM_PMTMR_LOCATE.

pub mod pmtimer;

use std::sync::Arc;
use vmm_core::hdl::VmmHdl;

/// Bhyve kernel-emulated AT PIC (Intel 8259A).
///
/// The kernel handles PIO at ports 0x20-0x21 (master) and 0xA0-0xA1
/// (slave).
pub struct BhyveAtPic;

impl BhyveAtPic {
    pub fn create() -> Arc<Self> {
        Arc::new(Self)
    }
}

/// Bhyve kernel-emulated AT PIT (Intel 8254).
///
/// The kernel handles PIO at ports 0x40-0x43.
pub struct BhyveAtPit;

impl BhyveAtPit {
    pub fn create() -> Arc<Self> {
        Arc::new(Self)
    }
}

/// Bhyve kernel-emulated HPET (High Precision Event Timer).
///
/// The kernel handles MMIO at 0xFED00000.
pub struct BhyveHpet;

impl BhyveHpet {
    pub fn create() -> Arc<Self> {
        Arc::new(Self)
    }
}

/// Bhyve kernel-emulated I/O APIC.
///
/// The kernel handles MMIO at 0xFEC00000.
pub struct BhyveIoApic;

impl BhyveIoApic {
    pub fn create() -> Arc<Self> {
        Arc::new(Self)
    }
}

/// Bhyve kernel-emulated RTC (MC146818).
///
/// The kernel handles PIO at ports 0x70-0x71.
pub struct BhyveRtc {
    hdl: Arc<VmmHdl>,
}

impl BhyveRtc {
    pub fn create(hdl: Arc<VmmHdl>) -> Arc<Self> {
        Arc::new(Self { hdl })
    }

    /// Write the memory size to NVRAM for BIOS/UEFI discovery.
    ///
    /// RTC NVRAM 0x34-0x35 holds memory above 16 MiB in 64 KiB units,
    /// capped at 0xFFFF (4 GiB - 16 MiB). 0x5B-0x5D holds memory above
    /// 4 GiB in 64 KiB units.
    pub fn memsize_to_nvram(
        &self,
        lowmem: u32,
        highmem: u64,
    ) -> std::io::Result<()> {
        let ext_mem = if lowmem > 16 * 1024 * 1024 {
            ((lowmem - 16 * 1024 * 1024) / (64 * 1024)).min(0xFFFF) as u16
        } else {
            0
        };
        self.hdl.rtc_write(0x34, ext_mem as u8)?;
        self.hdl.rtc_write(0x35, (ext_mem >> 8) as u8)?;

        let high_units = (highmem / (64 * 1024)) as u32;
        self.hdl.rtc_write(0x5B, high_units as u8)?;
        self.hdl.rtc_write(0x5C, (high_units >> 8) as u8)?;
        self.hdl.rtc_write(0x5D, (high_units >> 16) as u8)?;

        Ok(())
    }
}
