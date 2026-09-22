// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! FADT (Fixed ACPI Description Table).

use super::{
    fix_checksum, write_gas, write_gas_zero, write_header, AcpiConfig,
    ACPI_AS_SYSTEM_IO, ACPI_GAS_BYTE, ACPI_GAS_DWORD, ACPI_GAS_WORD, IO_PMTMR,
    PM1A_CNT_ADDR, PM1A_EVT_ADDR, RESET_REG_PORT, SCI_INT,
};
use crate::acpi_gpe::{GPE0_BLK_ADDR, GPE0_BLK_LEN};

/// FADT size in the ACPI 6.0 layout, which ends with the Hypervisor
/// Vendor Identity field.
const FADT_SIZE: u32 = 276;

// FADT flag bits
const FADT_WBINVD: u32 = 1 << 0;
const FADT_PROC_C1: u32 = 1 << 2;
/// No fixed power button: set means the OS looks for a control-method
/// device instead of polling PM1_STS, which nothing here raises.
const FADT_PWR_BUTTON: u32 = 1 << 4;
/// No fixed sleep button, for the same reason.
const FADT_SLP_BUTTON: u32 = 1 << 5;
const FADT_TMR_VAL_EXT: u32 = 1 << 8;
const FADT_RESET_REG_SUP: u32 = 1 << 10;
const FADT_HEADLESS: u32 = 1 << 12;

pub(super) fn build_fadt(
    facs_gpa: u32,
    dsdt_gpa: u32,
    cfg: &AcpiConfig,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(FADT_SIZE as usize);

    // A VM with no hotplug has no GPE source, so its GPE0 fields stay
    // zero.
    let gpe0_blk = if cfg.hotplug { GPE0_BLK_ADDR } else { 0 };
    let gpe0_blk_len = if cfg.hotplug { GPE0_BLK_LEN } else { 0 };

    // SDT header (offsets 0..36)
    write_header(&mut buf, b"FACP", FADT_SIZE, 5);

    // offset 36: FIRMWARE_CTRL (FACS pointer, 4 bytes)
    buf.extend_from_slice(&facs_gpa.to_le_bytes());
    // offset 40: DSDT pointer (4 bytes)
    buf.extend_from_slice(&dsdt_gpa.to_le_bytes());

    // offset 44: reserved (was INT_MODEL, now must be 0)
    buf.push(0);
    // offset 45: Preferred_PM_Profile (unspecified = 0)
    buf.push(0);

    // offset 46: SCI_INT (u16 LE)
    buf.extend_from_slice(&SCI_INT.to_le_bytes());
    // offset 48: SMI_CMD (u32 LE) - port 0xB2 for ACPI enable/disable
    buf.extend_from_slice(&0xB2u32.to_le_bytes());
    // offset 52: ACPI_ENABLE - value to write to SMI_CMD
    buf.push(0xA0);
    // offset 53: ACPI_DISABLE
    buf.push(0xA1);
    // offset 54: S4BIOS_REQ
    buf.push(0);
    // offset 55: PSTATE_CNT
    buf.push(0);

    // offset 56: PM1a_EVT_BLK (u32 LE)
    buf.extend_from_slice(&PM1A_EVT_ADDR.to_le_bytes());
    // offset 60: PM1b_EVT_BLK (u32 LE) - not used
    buf.extend_from_slice(&0u32.to_le_bytes());
    // offset 64: PM1a_CNT_BLK (u32 LE)
    buf.extend_from_slice(&PM1A_CNT_ADDR.to_le_bytes());
    // offset 68: PM1b_CNT_BLK (u32 LE) - not used
    buf.extend_from_slice(&0u32.to_le_bytes());
    // offset 72: PM2_CNT_BLK (u32 LE) - not used
    buf.extend_from_slice(&0u32.to_le_bytes());
    // offset 76: PM_TMR_BLK (u32 LE)
    buf.extend_from_slice(&IO_PMTMR.to_le_bytes());
    // offset 80: GPE0_BLK (u32 LE)
    buf.extend_from_slice(&u32::from(gpe0_blk).to_le_bytes());
    // offset 84: GPE1_BLK (u32 LE)
    buf.extend_from_slice(&0u32.to_le_bytes());

    // offset 88: PM1_EVT_LEN
    buf.push(4);
    // offset 89: PM1_CNT_LEN
    buf.push(2);
    // offset 90: PM2_CNT_LEN
    buf.push(0);
    // offset 91: PM_TMR_LEN
    buf.push(4);
    // offset 92: GPE0_BLK_LEN
    buf.push(gpe0_blk_len);
    // offset 93: GPE1_BLK_LEN
    buf.push(0);
    // offset 94: GPE1_BASE
    buf.push(0);
    // offset 95: CST_CNT
    buf.push(0);
    // offset 96: P_LVL2_LAT (u16 LE)
    buf.extend_from_slice(&0u16.to_le_bytes());
    // offset 98: P_LVL3_LAT (u16 LE)
    buf.extend_from_slice(&0u16.to_le_bytes());
    // offset 100: FLUSH_SIZE (u16 LE)
    buf.extend_from_slice(&0u16.to_le_bytes());
    // offset 102: FLUSH_STRIDE (u16 LE)
    buf.extend_from_slice(&0u16.to_le_bytes());
    // offset 104: DUTY_OFFSET
    buf.push(0);
    // offset 105: DUTY_WIDTH
    buf.push(0);
    // offset 106: DAY_ALRM
    buf.push(0);
    // offset 107: MON_ALRM
    buf.push(0);
    // offset 108: CENTURY
    buf.push(0x32);

    // offset 109: IAPC_BOOT_ARCH (u16 LE) - NO_VGA | NO_ASPM
    let boot_flags: u16 = (1 << 2) | (1 << 4); // NO_VGA=bit2, NO_ASPM=bit4
    buf.extend_from_slice(&boot_flags.to_le_bytes());

    // offset 111: reserved
    buf.push(0);

    // offset 112: Flags (u32 LE)
    let flags = FADT_WBINVD
        | FADT_PROC_C1
        | FADT_PWR_BUTTON
        | FADT_SLP_BUTTON
        | FADT_TMR_VAL_EXT
        | FADT_RESET_REG_SUP
        | FADT_HEADLESS;
    buf.extend_from_slice(&flags.to_le_bytes());

    // offset 116: RESET_REG (12-byte GAS)
    write_gas(
        &mut buf,
        ACPI_AS_SYSTEM_IO,
        8,
        0,
        ACPI_GAS_BYTE,
        RESET_REG_PORT,
    );

    // offset 128: RESET_VALUE
    buf.push(6); // Writing 6 to 0xCF9 triggers system reset

    // offset 129: ARM_BOOT_ARCH (u16 LE) - x86, not used
    buf.extend_from_slice(&0u16.to_le_bytes());

    // offset 131: FADT Minor Version
    buf.push(1);

    // offset 132: X_FIRMWARE_CTRL (u64 LE) - extended FACS pointer
    buf.extend_from_slice(&(facs_gpa as u64).to_le_bytes());
    // offset 140: X_DSDT (u64 LE)
    buf.extend_from_slice(&(dsdt_gpa as u64).to_le_bytes());

    // offset 148: X_PM1a_EVT_BLK (GAS, 12 bytes)
    write_gas(
        &mut buf,
        ACPI_AS_SYSTEM_IO,
        0x20,
        0,
        ACPI_GAS_WORD,
        PM1A_EVT_ADDR as u64,
    );
    // offset 160: X_PM1b_EVT_BLK
    write_gas_zero(&mut buf);
    // offset 172: X_PM1a_CNT_BLK
    write_gas(
        &mut buf,
        ACPI_AS_SYSTEM_IO,
        0x10,
        0,
        ACPI_GAS_WORD,
        PM1A_CNT_ADDR as u64,
    );
    // offset 184: X_PM1b_CNT_BLK
    write_gas_zero(&mut buf);
    // offset 196: X_PM2_CNT_BLK
    write_gas_zero(&mut buf);
    // offset 208: X_PM_TMR_BLK
    write_gas(
        &mut buf,
        ACPI_AS_SYSTEM_IO,
        0x20,
        0,
        ACPI_GAS_DWORD,
        IO_PMTMR as u64,
    );
    // offset 220: X_GPE0_BLK. ACPI 6.5, section 4.8.4.1 requires
    // byte-at-a-time access to a GPE block.
    if cfg.hotplug {
        write_gas(
            &mut buf,
            ACPI_AS_SYSTEM_IO,
            gpe0_blk_len * 8,
            0,
            ACPI_GAS_BYTE,
            u64::from(gpe0_blk),
        );
    } else {
        write_gas_zero(&mut buf);
    }
    // offset 232: X_GPE1_BLK
    write_gas_zero(&mut buf);
    // offset 244: SLEEP_CONTROL_REG
    write_gas_zero(&mut buf);
    // offset 256: SLEEP_STATUS_REG
    write_gas_zero(&mut buf);
    // offset 268: Hypervisor Vendor Identity (u64 LE)
    buf.extend_from_slice(&0u64.to_le_bytes());

    assert_eq!(buf.len(), FADT_SIZE as usize);

    fix_checksum(&mut buf, 0, FADT_SIZE as usize);

    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acpi::test_support::verify_checksum;

    /// Offsets of the three GPE0 fields inside the FADT.
    const GPE0_BLK_OFF: usize = 80;
    const GPE0_BLK_LEN_OFF: usize = 92;
    const X_GPE0_BLK_OFF: usize = 220;

    fn no_hotplug() -> AcpiConfig {
        AcpiConfig::boot_only(1)
    }

    fn hotplug() -> AcpiConfig {
        AcpiConfig::boot_only(1).with_hotplug(true)
    }

    #[test]
    fn fadt_checksum() {
        for cfg in [no_hotplug(), hotplug()] {
            let fadt = build_fadt(0x1000, 0x2000, &cfg);
            assert_eq!(fadt.len(), FADT_SIZE as usize);
            assert_eq!(&fadt[0..4], b"FACP");
            assert!(verify_checksum(&fadt));
        }
    }

    #[test]
    fn fadt_advertises_no_fixed_power_or_sleep_button() {
        // Nothing raises PM1_PWRBTN_STS, so a guest told there is a
        // fixed power button would wait on an SCI that never comes.
        let fadt = build_fadt(0x1000, 0x2000, &no_hotplug());
        let flags =
            u32::from_le_bytes([fadt[112], fadt[113], fadt[114], fadt[115]]);
        assert_ne!(flags & FADT_PWR_BUTTON, 0);
        assert_ne!(flags & FADT_SLP_BUTTON, 0);
    }

    #[test]
    fn fadt_pm_timer_port() {
        let fadt = build_fadt(0x1000, 0x2000, &no_hotplug());
        // PM_TMR_BLK is at offset 76
        let pmtmr =
            u32::from_le_bytes([fadt[76], fadt[77], fadt[78], fadt[79]]);
        assert_eq!(pmtmr, IO_PMTMR);
        assert_eq!(pmtmr, 0x408);
    }

    #[test]
    fn fadt_facs_dsdt_pointers() {
        let fadt = build_fadt(0xAAAA, 0xBBBB, &no_hotplug());
        // FACS at offset 36
        let facs_ptr =
            u32::from_le_bytes([fadt[36], fadt[37], fadt[38], fadt[39]]);
        assert_eq!(facs_ptr, 0xAAAA);
        // DSDT at offset 40
        let dsdt_ptr =
            u32::from_le_bytes([fadt[40], fadt[41], fadt[42], fadt[43]]);
        assert_eq!(dsdt_ptr, 0xBBBB);
    }

    #[test]
    fn fadt_without_hotplug_has_no_gpe0_block() {
        let fadt = build_fadt(0x1000, 0x2000, &no_hotplug());
        assert_eq!(
            u32::from_le_bytes(
                fadt[GPE0_BLK_OFF..GPE0_BLK_OFF + 4].try_into().unwrap()
            ),
            0,
        );
        assert_eq!(fadt[GPE0_BLK_LEN_OFF], 0);
        assert_eq!(&fadt[X_GPE0_BLK_OFF..X_GPE0_BLK_OFF + 12], &[0u8; 12]);
    }

    #[test]
    fn fadt_hotplug_publishes_the_gpe0_block() {
        let fadt = build_fadt(0x1000, 0x2000, &hotplug());
        assert_eq!(
            u32::from_le_bytes(
                fadt[GPE0_BLK_OFF..GPE0_BLK_OFF + 4].try_into().unwrap()
            ),
            u32::from(GPE0_BLK_ADDR),
        );
        assert_eq!(fadt[GPE0_BLK_LEN_OFF], GPE0_BLK_LEN);
        assert_eq!(GPE0_BLK_LEN % 2, 0, "ACPI needs an even GPE block");

        let gas = &fadt[X_GPE0_BLK_OFF..X_GPE0_BLK_OFF + 12];
        assert_eq!(gas[0], ACPI_AS_SYSTEM_IO);
        assert_eq!(gas[1], GPE0_BLK_LEN * 8);
        assert_eq!(gas[2], 0);
        assert_eq!(gas[3], ACPI_GAS_BYTE);
        assert_eq!(
            u64::from_le_bytes(gas[4..12].try_into().unwrap()),
            u64::from(GPE0_BLK_ADDR),
        );
    }

    /// Hotplug changes the GPE0 fields and no other FADT byte.
    #[test]
    fn fadt_hotplug_touches_only_the_gpe0_fields() {
        let plain = build_fadt(0x1000, 0x2000, &no_hotplug());
        let with_hotplug = build_fadt(0x1000, 0x2000, &hotplug());
        assert_eq!(plain.len(), with_hotplug.len());

        let gpe0_fields = |index: usize| {
            (GPE0_BLK_OFF..GPE0_BLK_OFF + 4).contains(&index)
                || index == GPE0_BLK_LEN_OFF
                || (X_GPE0_BLK_OFF..X_GPE0_BLK_OFF + 12).contains(&index)
                // The checksum covers the whole table.
                || index == 9
        };
        for index in 0..plain.len() {
            if gpe0_fields(index) {
                continue;
            }
            assert_eq!(plain[index], with_hotplug[index], "byte {index} moved");
        }
    }
}
