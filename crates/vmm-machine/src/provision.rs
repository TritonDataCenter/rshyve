// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tell the zone brand the VM finished starting.
//!
//! `vmadm create` does not watch the VMM process. It watches a file:
//! the bhyve brand sets `var_svc_provisioning` (in `proptable.js`), so
//! `VM.waitForProvisioning` waits for `/var/svc/provisioning` to be
//! renamed and calls `markVMFailure` after PROVISION_TIMEOUT, 300
//! seconds. C bhyve does the rename in `mark_provisioned()`
//! (usr/src/cmd/bhyve/common/bhyverun.c) right after it starts the
//! vCPUs. A VMM that does not rename it provisions in milliseconds and
//! still reports a five minute failure.
//!
//! Outside a zone the file is absent and this does nothing, which is
//! also what C bhyve does.

use std::path::Path;

/// The file the brand creates before it starts the VMM.
const PROVISIONING: &str = "/var/svc/provisioning";

/// What it is renamed to once the guest is running.
const PROVISION_SUCCESS: &str = "/var/svc/provision_success";

/// Rename the provisioning marker, if the brand left one.
///
/// Call this once the guest can run, and BEFORE dropping privileges: the
/// rename needs file access, which is why C bhyve widens its own
/// privilege window around the same call in `main` (`bhyverun.c`).
///
/// A failure is logged and not returned. The VM is running by this
/// point, so failing the boot over the marker would turn a reportable
/// timeout into a worse outcome.
pub fn mark_provisioned(log: &slog::Logger) {
    mark_provisioned_at(
        Path::new(PROVISIONING),
        Path::new(PROVISION_SUCCESS),
        log,
    )
}

fn mark_provisioned_at(from: &Path, to: &Path, log: &slog::Logger) {
    // Absent means this is not a brand-managed zone. Not an error.
    if from.symlink_metadata().is_err() {
        return;
    }
    match std::fs::rename(from, to) {
        Ok(()) => slog::debug!(log, "marked the zone provisioned";
            "from" => %from.display(), "to" => %to.display()),
        Err(e) => slog::error!(log,
            "could not mark the zone provisioned, vmadm will time out";
            "from" => %from.display(), "error" => %e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logger() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    #[test]
    fn the_marker_is_renamed_the_way_the_brand_waits_for() {
        let dir = tempfile::tempdir().expect("tempdir");
        let from = dir.path().join("provisioning");
        let to = dir.path().join("provision_success");
        std::fs::write(&from, b"").expect("write");

        mark_provisioned_at(&from, &to, &logger());

        assert!(!from.exists(), "the brand still sees a provisioning file");
        assert!(to.exists(), "vmadm never sees the rename it waits for");
    }

    /// Outside a zone the brand leaves no marker, and C bhyve returns
    /// early on the same condition.
    #[test]
    fn no_marker_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let from = dir.path().join("absent");
        let to = dir.path().join("provision_success");

        mark_provisioned_at(&from, &to, &logger());

        assert!(!to.exists(), "invented a marker the brand never made");
    }
}
