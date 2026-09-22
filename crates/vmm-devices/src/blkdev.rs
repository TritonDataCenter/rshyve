// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Zvol write-cache control (DKIOCSETWCE).
//!
//! Zvols default to write-through on illumos, so every sync 4K write
//! commits the ZIL. Writeback, with guest flushes for durability, takes
//! 4K random writes from about 1k to over 50k IOPS.
//!
//! As in propolis `block/file.rs`: probe at open, enable writeback if
//! supported, restore the original state on Drop. C bhyve gates this on
//! a feature flag. That gate is not necessary here: the virtio-blk device
//! always advertises `VIRTIO_BLK_F_FLUSH`, every real guest negotiates
//! it, and NVMe has implicit FLUSH semantics.
//!
//! `DiskCache` shares the `File`, so the restore ioctl always runs on
//! an open descriptor, whatever the field order of the owner.

use std::fs::File;
#[cfg(target_os = "illumos")]
use std::io;
#[cfg(target_os = "illumos")]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::Arc;

#[cfg(target_os = "illumos")]
const DKIOC: libc::c_int = 0x04 << 8; // 0x0400
#[cfg(target_os = "illumos")]
const DKIOCGETWCE: libc::c_int = DKIOC | 36; // 0x0424
#[cfg(target_os = "illumos")]
const DKIOCSETWCE: libc::c_int = DKIOC | 37; // 0x0425

/// Holds zvol write-cache state for the lifetime of a backing fd.
///
/// `initial == Some(false)` means this handle enabled writeback and
/// restores write-through on Drop. `None` means the handle changed
/// nothing: read-only, non-illumos, already in writeback, or no
/// DKIOCSETWCE support.
pub struct DiskCache {
    #[cfg_attr(not(target_os = "illumos"), allow(dead_code))]
    file: Arc<File>,
    #[cfg_attr(not(target_os = "illumos"), allow(dead_code))]
    initial: Option<bool>,
}

impl DiskCache {
    /// Probe and (where supported) enable writeback on the backing file.
    ///
    /// Best effort: an ioctl failure leaves the device state unchanged.
    pub fn new(file: Arc<File>, read_only: bool) -> Self {
        if read_only {
            return Self {
                file,
                initial: None,
            };
        }
        Self::new_inner(file)
    }

    #[cfg(target_os = "illumos")]
    fn new_inner(file: Arc<File>) -> Self {
        let mut v: libc::c_int = 0;
        if dkioc_wce(file.as_fd(), DKIOCGETWCE, &mut v).is_err() {
            // Not a disk device, or the backing store does not expose
            // WCE (regular files, iSCSI/COMSTAR targets and others).
            return Self {
                file,
                initial: None,
            };
        }
        let initial = v != 0;
        if initial {
            // Already in writeback. Nothing to restore.
            return Self {
                file,
                initial: None,
            };
        }
        let mut enable: libc::c_int = 1;
        if dkioc_wce(file.as_fd(), DKIOCSETWCE, &mut enable).is_err() {
            // GET worked but SET was refused.
            return Self {
                file,
                initial: None,
            };
        }
        Self {
            file,
            initial: Some(false),
        }
    }

    #[cfg(not(target_os = "illumos"))]
    fn new_inner(file: Arc<File>) -> Self {
        Self {
            file,
            initial: None,
        }
    }
}

impl Drop for DiskCache {
    fn drop(&mut self) {
        #[cfg(target_os = "illumos")]
        {
            if self.initial == Some(false) {
                let mut v: libc::c_int = 0;
                let _ = dkioc_wce(self.file.as_fd(), DKIOCSETWCE, &mut v);
            }
        }
    }
}

/// Takes `&mut c_int` so the kernel always reads and writes the 4 bytes
/// that DKIOC{GET,SET}WCE expects.
#[cfg(target_os = "illumos")]
fn dkioc_wce(
    fd: BorrowedFd<'_>,
    cmd: libc::c_int,
    v: &mut libc::c_int,
) -> io::Result<()> {
    // SAFETY: `v` lives through the call and is the size
    // DKIOC{GET,SET}WCE reads and writes.
    unsafe {
        vmm_api_common::ioctl(fd.as_raw_fd(), cmd, std::ptr::from_mut(v).cast())
    }?;
    Ok(())
}
