// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Crate-boundary surface for vmm_hid.

use std::sync::Arc;

use vmm_devices::pci::device::PciDevice;
use vmm_devices::{KeyboardSink, Lifecycle, PointerSink};
use vmm_hid::fbuf::Framebuffer;
use vmm_hid::ps2::PS2Ctrl;
use vmm_hid::xhci::XhciController;

fn _coerce_fbuf(
    dev: Arc<Framebuffer>,
) -> (Arc<dyn PciDevice>, Arc<dyn Lifecycle>) {
    (dev.clone(), dev)
}

fn _coerce_xhci(
    dev: Arc<XhciController>,
) -> (Arc<dyn PciDevice>, Arc<dyn Lifecycle>, Arc<dyn PointerSink>) {
    (dev.clone(), dev.clone(), dev)
}

/// PS/2 is an ISA device: it is a `Lifecycle` and a `KeyboardSink`, and
/// deliberately not a `PciDevice`.
fn _coerce_ps2(
    dev: Arc<PS2Ctrl>,
) -> (Arc<dyn Lifecycle>, Arc<dyn KeyboardSink>) {
    (dev.clone(), dev)
}

#[test]
fn ps2_port_and_irq_constants_kept_their_module_paths() {
    assert_eq!(vmm_hid::ps2::PORT_PS2_DATA, 0x60);
    assert_eq!(vmm_hid::ps2::PORT_PS2_CMD_STATUS, 0x64);
    assert_eq!(vmm_hid::ps2::IRQ_PS2_PRI, 1);
    assert_eq!(vmm_hid::ps2::IRQ_PS2_AUX, 12);
}
