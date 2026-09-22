// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Instance lifetime tests against a real `/dev/vmm`.
//!
//! The kernel reclaims an instance when the last handle closes only if
//! `VM_SET_AUTODESTRUCT` is armed. These tests need illumos and the
//! privilege to create a VM.
#![cfg(target_os = "illumos")]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bhyve_api::VmmCtlFd;
use vmm_core::hdl::{CreateOpts, VmmHdl};

/// Kernel destruction can stall behind other clean-up, so poll for it.
const REAP_BUDGET: Duration = Duration::from_secs(10);

/// The kernel allows only one instance per non-global zone, so these
/// tests must not overlap.
static VMM_SLOT: Mutex<()> = Mutex::new(());

fn exclusive() -> MutexGuard<'static, ()> {
    VMM_SLOT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Destroys the instance when the test ends, also after a panic.
struct Reaper(String);

impl Drop for Reaper {
    fn drop(&mut self) {
        if let Ok(ctl) = VmmCtlFd::open() {
            let _ = ctl.vm_destroy(self.0.as_bytes());
        }
        // The next test cannot create until this instance is gone.
        let _ = gone_within(&instance_path(&self.0), REAP_BUDGET);
    }
}

fn unique_name(tag: &str) -> String {
    format!("vmmcore-test-{tag}-{}", std::process::id())
}

fn instance_path(name: &str) -> PathBuf {
    Path::new("/dev/vmm").join(name)
}

fn gone_within(path: &Path, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if !path.exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn create(name: &str) -> VmmHdl {
    let opts = CreateOpts {
        force: true,
        ..Default::default()
    };
    VmmHdl::create(name, &opts).expect("test host must allow VM creation")
}

#[test]
fn armed_autodestruct_reclaims_the_instance_on_close() {
    let _slot = exclusive();
    let name = unique_name("armed");
    let _reaper = Reaper(name.clone());

    let hdl = create(&name);
    hdl.set_autodestruct(true).expect("arm autodestruct");
    assert!(
        instance_path(&name).exists(),
        "create must publish /dev/vmm/{name}",
    );

    drop(hdl);

    assert!(
        gone_within(&instance_path(&name), REAP_BUDGET),
        "armed autodestruct must reclaim /dev/vmm/{name} on close",
    );
}

#[test]
fn disarmed_autodestruct_keeps_the_instance() {
    // Guards the argument polarity. A zero argument clears the flag, and
    // that disarms the only crash reclaimer.
    let _slot = exclusive();
    let name = unique_name("disarmed");
    let reaper = Reaper(name.clone());

    let hdl = create(&name);
    hdl.set_autodestruct(false).expect("disarm autodestruct");
    drop(hdl);

    let survived = instance_path(&name).exists();
    drop(reaper);

    assert!(survived, "a disarmed instance must survive the close");
    assert!(
        gone_within(&instance_path(&name), REAP_BUDGET),
        "clean-up must destroy /dev/vmm/{name}",
    );
}
