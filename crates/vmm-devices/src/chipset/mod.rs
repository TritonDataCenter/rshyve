// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Chipset emulation: an i440FX with a host bridge, an ISA/LPC bridge
//! and one PCI bus.

pub mod i440fx;

pub use i440fx::I440FxChipset;
