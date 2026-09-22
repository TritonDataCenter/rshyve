// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Hotplug register files, one submodule per resource kind.
//!
//! Each register file is the device half of a hotplug feature. It
//! latches the events the VMM raises, reports them to the guest through
//! a small I/O block, and takes the guest's answer. The AML half lives
//! in [`crate::acpi::hotplug`] and is written against the same block.
//!
//! A register file raises its general purpose event through
//! [`crate::acpi_gpe::HotplugEventSink`], so it never needs an
//! interrupt pin and stays testable on its own.
//!
//! Every entry point here runs on a vCPU thread inside an I/O exit.
//! None of them creates or destroys a resource: they record what the
//! guest asked for, and a dedicated thread does the work.

pub mod cpu;
pub mod mem;
pub mod pci;

#[cfg(test)]
mod tests {
    use crate::acpi_gpe::{GPE0_BLK_ADDR, GPE0_BLK_LEN};

    /// Each lane picks its own I/O base, so nothing but this test
    /// stops two of them landing on the same ports. An overlap makes
    /// one register file answer the other's reads.
    #[test]
    fn no_two_hotplug_blocks_share_a_port() {
        let blocks: [(&str, u32, u32); 4] = [
            (
                "pci",
                u32::from(super::pci::PCI_HOTPLUG_ADDR),
                u32::from(super::pci::PCI_HOTPLUG_LEN),
            ),
            (
                "cpu",
                u32::from(super::cpu::CPU_HOTPLUG_PORT),
                u32::from(super::cpu::CPU_HOTPLUG_LEN),
            ),
            (
                "mem",
                u32::from(super::mem::MEM_HOTPLUG_IO_BASE),
                u32::from(super::mem::MEM_HOTPLUG_IO_LEN),
            ),
            ("gpe0", u32::from(GPE0_BLK_ADDR), u32::from(GPE0_BLK_LEN)),
        ];

        for (i, (an, ab, al)) in blocks.iter().enumerate() {
            assert!(*al > 0, "{an} claims no ports");
            for (bn, bb, bl) in blocks.iter().skip(i + 1) {
                assert!(
                    ab + al <= *bb || bb + bl <= *ab,
                    "{an} {ab:#06x}+{al} overlaps {bn} {bb:#06x}+{bl}",
                );
            }
        }
    }
}
