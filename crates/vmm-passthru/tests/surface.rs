// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Crate-boundary surface for vmm_passthru.
//!
//! `PciPassthru::new` binds a real `/dev/pptN`, so it cannot run in a
//! test. What crosses the boundary is the path and the trait coercion,
//! and both of these assertions bind at compile time.

use std::sync::Arc;

use vmm_devices::pci::device::PciDevice;
use vmm_passthru::PciPassthru;

/// The catalog hands passthru out as `Arc<dyn PciDevice>` and nothing
/// else. It carries no `Lifecycle`, so it is never quiesced, paused or
/// migrated. A wider coercion would be a behaviour change.
fn _coerce_pci(dev: Arc<PciPassthru>) -> Arc<dyn PciDevice> {
    dev
}

#[test]
fn passthru_lives_at_the_crate_root() {
    // A compile-time path assertion: the type resolves as
    // `vmm_passthru::PciPassthru`, not `vmm_passthru::passthru::…`.
    fn _resolves(_: Option<&PciPassthru>) {}
    _resolves(None);
}
