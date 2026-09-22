// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! DSDT: the AML description of the platform device topology.

use super::{
    fix_checksum, write_header, AcpiConfig, TpmDevice, ACPI_HDR_SIZE, HPET_ADDR,
};
use crate::acpi_gpe::{GPE0_BLK_ADDR, GPE0_BLK_LEN};

/// Build the DSDT containing AML bytecode that describes the
/// platform's device topology to the guest OS.
///
/// This matches bhyve's DSDT structure:
/// - Sleep state S5 for shutdown support
/// - PIC/APIC mode switch (PICM / _PIC method)
/// - PCI host bridge (PCI0) with bus resources and interrupt routing
/// - HPET device
/// - ISA/LPC bridge with COM1, COM2, keyboard, mouse, PIC, timer, and RTC
///
/// A hotplug VM also gets the `\\_GPE` handlers and a reservation for
/// the GPE0 I/O ports.
pub(super) fn build_dsdt(cfg: &AcpiConfig) -> Vec<u8> {
    use crate::aml::{Aml, AmlValue};

    let mut aml = Aml::new();

    // ── Sleep state S5 (soft-off) ──────────────────────────────
    aml.name_package("_S5_", &[AmlValue::Byte(0x05), AmlValue::Zero]);

    // ── PIC/APIC mode indicator ────────────────────────────────
    aml.name_val("PICM", AmlValue::Zero);

    aml.method("_PIC", 1, false, |m| {
        m.store_arg0("PICM");
    });

    // ── Scope(\_SB) ────────────────────────────────────────────
    aml.scope("\\_SB", |sb| {
        // ── PCI Host Bridge: Device(PC00) ──────────────────
        sb.device("PC00", |pci| {
            pci.name_val("_HID", AmlValue::DWord(Aml::eisa_id("PNP0A03")));
            pci.name_val("_ADR", AmlValue::Zero);

            pci.method("_BBN", 0, false, |m| {
                m.return_val(AmlValue::Zero);
            });

            // ── PCI bus resources ──────────────────────────
            pci.name_resource_template("_CRS", |r| {
                // Bus numbers 0-0xFF
                r.word_bus_number(0x0000, 0x00FF, 0x0100);

                // PCI config space I/O ports (0xCF8-0xCFF)
                r.io_resource(0x0CF8, 0x0CF8, 1, 8);

                // I/O range below PCI config
                r.word_io(0x0000, 0x0CF7, 0x0CF8);

                // I/O range above PCI config
                r.word_io(0x0D00, 0xFFFF, 0xF300);

                // 32-bit MMIO window
                r.dword_memory(0xC000_0000, 0xFEBF_FFFF, 0x3EC0_0000);
            });

            // ── PCI interrupt routing ─────────────────────
            // PCI INTx uses IOAPIC pins 16-23, with the same spread as
            // ioapic_pci_alloc_irq() in C bhyve.
            let mut storage: Vec<[AmlValue; 4]> = Vec::new();

            for slot in 1u32..32 {
                for pin in 0u32..4 {
                    let gsi = 16 + (4 + slot + pin) % 8;
                    storage.push([
                        AmlValue::DWord((slot << 16) | 0xFFFF),
                        AmlValue::Byte(pin as u8),
                        AmlValue::Zero,
                        AmlValue::DWord(gsi),
                    ]);
                }
            }

            let refs: Vec<&[AmlValue]> =
                storage.iter().map(|s| s.as_slice()).collect();
            pci.name_nested_packages("APRT", &refs);

            pci.method("_PRT", 0, false, |m| {
                m.return_name("APRT");
            });

            // ── HPET device ───────────────────────────────
            pci.device("HPET", |hpet| {
                hpet.name_val("_HID", AmlValue::DWord(Aml::eisa_id("PNP0103")));
                hpet.name_val("_UID", AmlValue::Zero);
                hpet.name_resource_template("_CRS", |r| {
                    r.memory32_fixed(HPET_ADDR, 0x400);
                });
            });

            // ── ISA/LPC bridge ────────────────────────────
            // LPC bridge at slot 1, function 0
            pci.device("ISA_", |isa| {
                isa.name_val("_ADR", AmlValue::DWord(0x0001_0000));

                // COM1 UART (0x3F8, IRQ 4)
                isa.device("COM1", |com| {
                    com.name_val(
                        "_HID",
                        AmlValue::DWord(Aml::eisa_id("PNP0501")),
                    );
                    com.name_val("_UID", AmlValue::One);
                    com.name_resource_template("_CRS", |r| {
                        r.io_resource(0x3F8, 0x3F8, 1, 8);
                        r.irq_no_flags(4);
                    });
                });

                // COM2 UART (0x2F8, IRQ 3)
                isa.device("COM2", |com| {
                    com.name_val(
                        "_HID",
                        AmlValue::DWord(Aml::eisa_id("PNP0501")),
                    );
                    com.name_val("_UID", AmlValue::Byte(2));
                    com.name_resource_template("_CRS", |r| {
                        r.io_resource(0x2F8, 0x2F8, 1, 8);
                        r.irq_no_flags(3);
                    });
                });

                isa.device("KBD", |kbd| {
                    kbd.name_val(
                        "_HID",
                        AmlValue::DWord(Aml::eisa_id("PNP0303")),
                    );
                    kbd.name_resource_template("_CRS", |r| {
                        r.io_resource(0x60, 0x60, 1, 1);
                        r.io_resource(0x64, 0x64, 1, 1);
                        r.irq_no_flags(1);
                    });
                });

                isa.device("MOU", |mou| {
                    mou.name_val(
                        "_HID",
                        AmlValue::DWord(Aml::eisa_id("PNP0F13")),
                    );
                    mou.name_resource_template("_CRS", |r| {
                        r.io_resource(0x60, 0x60, 1, 1);
                        r.io_resource(0x64, 0x64, 1, 1);
                        r.irq_no_flags(12);
                    });
                });

                // 8259 PIC (0x20-0x21, 0xA0-0xA1, IRQ 2)
                isa.device("PIC_", |pic| {
                    pic.name_val(
                        "_HID",
                        AmlValue::DWord(Aml::eisa_id("PNP0000")),
                    );
                    pic.name_resource_template("_CRS", |r| {
                        r.io_resource(0x20, 0x20, 1, 2);
                        r.io_resource(0xA0, 0xA0, 1, 2);
                        r.irq_no_flags(2);
                    });
                });

                // System timer (0x40-0x43, IRQ 0)
                isa.device("TIMR", |tmr| {
                    tmr.name_val(
                        "_HID",
                        AmlValue::DWord(Aml::eisa_id("PNP0100")),
                    );
                    tmr.name_resource_template("_CRS", |r| {
                        r.io_resource(0x40, 0x40, 1, 4);
                        r.irq_no_flags(0);
                    });
                });

                // RTC (0x70-0x71, IRQ 8)
                isa.device("RTC0", |rtc| {
                    rtc.name_val(
                        "_HID",
                        AmlValue::DWord(Aml::eisa_id("PNP0B00")),
                    );
                    rtc.name_resource_template("_CRS", |r| {
                        r.io_resource(0x70, 0x70, 1, 2);
                        r.irq_no_flags(8);
                    });
                });
            });
        });

        if let Some(t) = cfg.tpm {
            emit_tpm_device(sb, t);
        }

        if cfg.hotplug {
            emit_gpe_reservation(sb);
            super::hotplug::emit_devices(sb, cfg);
        }
    });

    if cfg.hotplug {
        super::hotplug::emit_gpe_handlers(&mut aml, cfg);
    }

    // ── Wrap in DSDT header ────────────────────────────────────
    let aml_body = aml.into_bytes();
    let total_length = ACPI_HDR_SIZE + aml_body.len();
    let mut buf = Vec::with_capacity(total_length);
    write_header(&mut buf, b"DSDT", total_length as u32, 2);
    buf.extend_from_slice(&aml_body);
    fix_checksum(&mut buf, 0, total_length);
    buf
}

/// Reserve the GPE0 I/O ports.
///
/// Without the reservation the guest can give the range to another
/// driver, which then writes over the hotplug status register.
fn emit_gpe_reservation(parent: &mut crate::aml::Aml) {
    use crate::aml::{Aml, AmlValue};

    parent.device("GPEB", |device| {
        device.name_val("_HID", AmlValue::DWord(Aml::eisa_id("PNP0A06")));
        device.name_val("_UID", AmlValue::Zero);
        // Present, enabled, and functional, but not shown to the user.
        device.name_val("_STA", AmlValue::Byte(0x0B));
        device.name_resource_template("_CRS", |resources| {
            resources.io_resource(
                GPE0_BLK_ADDR,
                GPE0_BLK_ADDR,
                1,
                GPE0_BLK_LEN,
            );
        });
    });
}

fn emit_tpm_device(parent: &mut crate::aml::Aml, tpm: TpmDevice) {
    use crate::aml::AmlValue;

    parent.device("TPM", |device| {
        device.name_val("_HID", AmlValue::String("MSFT0101"));
        device.name_val("_STA", AmlValue::Byte(0x0F));
        device.name_resource_template("_CRS", |resources| {
            resources.memory32_fixed(tpm.crb_base, tpm.crb_len);
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acpi::test_support::{
        contains_tpm_hid, tpm_crs_window, verify_checksum, TEST_TPM_DEVICE,
        TPM_DEVICE_AML,
    };
    use crate::aml::{walk_aml, Aml};

    /// The DSDT does not vary with the CPU counts, so one boot CPU is
    /// enough for every case here.
    fn dsdt_cfg(tpm: Option<TpmDevice>) -> AcpiConfig {
        AcpiConfig::boot_only(1).with_tpm(tpm)
    }

    fn position(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        position(haystack, needle).is_some()
    }

    #[test]
    fn tpm_device_node_exact_bytes() {
        let mut aml = Aml::new();
        emit_tpm_device(&mut aml, TEST_TPM_DEVICE);
        assert_eq!(aml.into_bytes().as_slice(), TPM_DEVICE_AML.as_slice());
    }

    #[test]
    fn dsdt_structure_valid() {
        for tpm in [None, Some(TEST_TPM_DEVICE)] {
            for hotplug in [false, true] {
                let cfg = dsdt_cfg(tpm).with_hotplug(hotplug);
                let dsdt = build_dsdt(&cfg);
                let header_len =
                    u32::from_le_bytes(dsdt[4..8].try_into().unwrap()) as usize;
                assert_eq!(header_len, dsdt.len());
                assert!(verify_checksum(&dsdt));
                walk_aml(&dsdt[ACPI_HDR_SIZE..]).unwrap();
            }
        }
    }

    /// Byte-for-byte proof that asking for no hotplug changes nothing.
    #[test]
    fn dsdt_without_hotplug_is_byte_for_byte_unchanged() {
        for tpm in [None, Some(TEST_TPM_DEVICE)] {
            assert_eq!(
                build_dsdt(&dsdt_cfg(tpm)),
                build_dsdt(&dsdt_cfg(tpm).with_hotplug(false)),
            );
        }
    }

    #[test]
    fn dsdt_gpe_handlers_are_gated_on_hotplug() {
        let plain = build_dsdt(&dsdt_cfg(None));
        let hotplug = build_dsdt(&dsdt_cfg(None).with_hotplug(true));

        for method in [b"_E01", b"_E02", b"_E03"] {
            assert!(
                !contains(&plain, method),
                "{} leaked into a non-hotplug DSDT",
                std::str::from_utf8(method).unwrap(),
            );
            assert!(contains(&hotplug, method));
        }
        // Scope(\_GPE): ScopeOp, PkgLength, RootChar, then the NameSeg.
        assert!(contains(&hotplug, b"\\_GPE"));
        assert!(hotplug.len() > plain.len());
    }

    #[test]
    fn dsdt_hotplug_reserves_the_gpe0_ports() {
        let plain = build_dsdt(&dsdt_cfg(None));
        let hotplug = build_dsdt(&dsdt_cfg(None).with_hotplug(true));

        // PNP0A06: a generic container that only holds resources.
        let container = Aml::eisa_id("PNP0A06").to_le_bytes();
        assert!(!contains(&plain, &container));
        assert!(contains(&hotplug, &container));

        // IO descriptor: tag 0x47, Decode16, min, max, align, length.
        let base = GPE0_BLK_ADDR.to_le_bytes();
        let descriptor = [
            0x47,
            0x01,
            base[0],
            base[1],
            base[0],
            base[1],
            1,
            GPE0_BLK_LEN,
        ];
        assert!(!contains(&plain, &descriptor));
        assert!(contains(&hotplug, &descriptor));
    }

    #[test]
    fn dsdt_hotplug_gpe_methods_take_no_arguments() {
        let dsdt = build_dsdt(&dsdt_cfg(None).with_hotplug(true));
        for method in [b"_E01", b"_E02", b"_E03"] {
            let at = position(&dsdt, method).expect("method NameSeg");
            // MethodOp, one PkgLength byte, then the NameSeg, then
            // the flags byte. The low three bits of the flags byte are
            // the ArgCount.
            assert_eq!(dsdt[at - 2], 0x14, "MethodOp");
            let pkg_len = usize::from(dsdt[at - 1]);
            // A body under 63 bytes keeps the PkgLength to one byte,
            // so its two leading bits stay clear.
            assert_eq!(pkg_len >> 6, 0, "PkgLength is not one byte");
            assert_eq!(dsdt[at + 4] & 0x07, 0, "ArgCount");
            // The body length varies by handler. The package must still
            // hold its own header and stay inside the table.
            // The PkgLength byte, the NameSeg and the flags byte.
            const HEADER: usize = 1 + 4 + 1;
            assert!(pkg_len >= HEADER, "package is short");
            assert!(at - 1 + pkg_len <= dsdt.len(), "package overruns");
        }
    }

    #[test]
    fn dsdt_tpm_node_gated() {
        let with_tpm = build_dsdt(&dsdt_cfg(Some(TEST_TPM_DEVICE)));
        assert!(!contains_tpm_hid(&build_dsdt(&dsdt_cfg(None))));
        assert!(contains_tpm_hid(&with_tpm));
    }

    #[test]
    fn dsdt_tpm_node_costs_55_bytes() {
        let without_tpm = build_dsdt(&dsdt_cfg(None));
        let with_tpm = build_dsdt(&dsdt_cfg(Some(TEST_TPM_DEVICE)));
        assert_eq!(with_tpm.len() - without_tpm.len(), TPM_DEVICE_AML.len());
    }

    #[test]
    fn dsdt_tpm_crs_covers_control_area() {
        let dsdt = build_dsdt(&dsdt_cfg(Some(TEST_TPM_DEVICE)));
        let (base, len) = tpm_crs_window(&dsdt, TEST_TPM_DEVICE.crb_base);
        let control_area = base + 0x40;
        assert!(base <= control_area);
        assert!(control_area + 0x38 <= base + len);
        assert_eq!(len, TEST_TPM_DEVICE.crb_len);
    }

    #[test]
    fn dsdt_checksum() {
        let dsdt = build_dsdt(&dsdt_cfg(None));
        assert_eq!(&dsdt[0..4], b"DSDT");
        assert!(verify_checksum(&dsdt));
    }

    #[test]
    fn dsdt_has_aml_body() {
        let dsdt = build_dsdt(&dsdt_cfg(None));
        assert!(
            dsdt.len() > ACPI_HDR_SIZE,
            "DSDT should contain AML bytecode, but is only {} bytes",
            dsdt.len(),
        );
        let hdr_len =
            u32::from_le_bytes([dsdt[4], dsdt[5], dsdt[6], dsdt[7]]) as usize;
        assert_eq!(hdr_len, dsdt.len());
    }

    #[test]
    fn dsdt_contains_scope_op() {
        let dsdt = build_dsdt(&dsdt_cfg(None));
        let body = &dsdt[ACPI_HDR_SIZE..];
        assert!(body.contains(&0x10), "DSDT body should contain ScopeOp",);
    }

    #[test]
    fn dsdt_contains_device_ops() {
        let dsdt = build_dsdt(&dsdt_cfg(None));
        let body = &dsdt[ACPI_HDR_SIZE..];
        // DeviceOp is ExtPrefix(0x5B) + 0x82
        let has_device = body.windows(2).any(|w| w == [0x5B, 0x82]);
        assert!(has_device, "DSDT body should contain at least one DeviceOp",);
    }

    #[test]
    fn dsdt_contains_pci_eisa_id() {
        let dsdt = build_dsdt(&dsdt_cfg(None));
        // PNP0A03 EISA ID = 0x030AD041 in LE bytes: [0x41, 0xD0, 0x0A, 0x03]
        let eisa_bytes = 0x030AD041u32.to_le_bytes();
        let has_pci_id = dsdt.windows(4).any(|w| w == eisa_bytes);
        assert!(
            has_pci_id,
            "DSDT should contain PNP0A03 (PCI host bridge) EISA ID",
        );
    }

    #[test]
    fn dsdt_contains_uart_eisa_id() {
        let dsdt = build_dsdt(&dsdt_cfg(None));
        // PNP0501 EISA ID = 0x0105D041
        let eisa_bytes = 0x0105D041u32.to_le_bytes();
        let count = dsdt.windows(4).filter(|w| *w == eisa_bytes).count();
        // COM1 and COM2 both use PNP0501
        assert!(
            count >= 2,
            "DSDT should contain at least 2 PNP0501 (UART) EISA IDs, found {}",
            count,
        );
    }

    #[test]
    fn dsdt_contains_ps2_eisa_ids() {
        let dsdt = build_dsdt(&dsdt_cfg(None));
        let keyboard_id = 0x0303D041u32.to_le_bytes();
        let mouse_id = 0x130FD041u32.to_le_bytes();

        assert!(
            dsdt.windows(4).any(|w| w == keyboard_id),
            "DSDT should contain PNP0303 (PS/2 keyboard) EISA ID",
        );
        assert!(
            dsdt.windows(4).any(|w| w == mouse_id),
            "DSDT should contain PNP0F13 (PS/2 mouse) EISA ID",
        );
    }

    #[test]
    fn dsdt_contains_hpet_address() {
        let dsdt = build_dsdt(&dsdt_cfg(None));
        let hpet_bytes = HPET_ADDR.to_le_bytes();
        let has_hpet = dsdt.windows(4).any(|w| w == hpet_bytes);
        assert!(
            has_hpet,
            "DSDT should contain HPET address 0x{:08X}",
            HPET_ADDR,
        );
    }

    #[test]
    fn dsdt_contains_com1_port() {
        let dsdt = build_dsdt(&dsdt_cfg(None));
        let port_bytes = 0x03F8u16.to_le_bytes();
        let has_com1 = dsdt.windows(2).any(|w| w == port_bytes);
        assert!(has_com1, "DSDT should contain COM1 I/O port 0x3F8",);
    }
}
