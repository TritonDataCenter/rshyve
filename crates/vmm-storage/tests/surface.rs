// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Crate-boundary surface for vmm_storage.

use std::path::Path;
use std::sync::Arc;

use vmm_devices::pci::device::PciDevice;
use vmm_devices::Lifecycle;
use vmm_storage::ahci::media::IsoMedia;
use vmm_storage::ahci::AhciCtrl;
use vmm_storage::nvme::NvmeController;

fn _coerce_ahci(
    dev: Arc<AhciCtrl>,
) -> (Arc<dyn PciDevice>, Arc<dyn Lifecycle>) {
    (dev.clone(), dev)
}

fn _coerce_nvme(
    dev: Arc<NvmeController>,
) -> (Arc<dyn PciDevice>, Arc<dyn Lifecycle>) {
    (dev.clone(), dev)
}

#[test]
fn iso_media_open_reports_a_missing_path() {
    // IsoMedia has no Debug, so expect_err does not compile here.
    let err = match IsoMedia::open(Path::new("/nonexistent/split-probe.iso")) {
        Ok(_) => panic!("a missing ISO must be an error"),
        Err(err) => err,
    };
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}
