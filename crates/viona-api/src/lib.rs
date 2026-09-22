// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copied from Propolis, Copyright Oxide Computer Company,
// used under MPL-2.0. Upstream: crates/viona-api/src/lib.rs
// https://github.com/oxidecomputer/propolis

use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Result};

use std::os::fd::*;
use std::os::unix::fs::MetadataExt;
use vmm_api_common::ioctl;

mod ffi;
mod link;

pub use ffi::*;
pub use link::{IntrStatus, IntrWait, LinkOps};

pub const VIONA_DEV_PATH: &str = "/dev/viona";

/// Where a [`VionaFd`] sends its ioctls.
///
/// [`Chan::Dev`] is the link a VM runs on. A test builds the other arm,
/// which records the command and argument and reaches no kernel. Both
/// arms share one set of methods, so a test watches the production
/// `delete`.
///
/// The seam is at this layer because upstream Propolis sends every
/// ioctl directly to `libc::ioctl` in `VionaFd::ioctl`, so a test can
/// read only the errno. A wrong command or swapped arguments give the
/// same errno as the correct call.
///
/// A recorder is `#[cfg(test)]`, so no production build can make one.
enum Chan {
    /// An open viona device.
    Dev(File),

    /// A recorder. It holds a file only for [`AsRawFd`] and to give
    /// `wait_intr` a descriptor to poll.
    #[cfg(test)]
    Record(File, std::sync::Mutex<Recording>),
}

/// The calls a recorder keeps, and the replies it gives.
#[cfg(test)]
#[derive(Default)]
struct Recording {
    /// The ioctls a caller sent, in order.
    calls: Vec<Call>,

    /// The bytes each command copies out, by command.
    ///
    /// Without them a caller reads back the struct it passed in, so a
    /// method that returns the copy-out (`intr_status`, `get_features`)
    /// looks the same as one that returns a constant.
    replies: std::collections::HashMap<i32, Vec<u8>>,
}

/// One ioctl that a caller sent.
#[cfg(test)]
#[derive(Clone, PartialEq, Eq)]
enum Call {
    /// The data argument is a value the kernel reads directly.
    Usize { cmd: i32, arg: usize },

    /// The data argument is a pointer to a struct, kept as the bytes
    /// the kernel would copy in, so swapped payload fields show.
    Typed { cmd: i32, data: Vec<u8> },
}

/// The command in hex, as `uts/intel/sys/viona_io.h` writes it, so a
/// failed assertion is easy to compare with that header.
#[cfg(test)]
impl std::fmt::Debug for Call {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Call::Usize { cmd, arg } => {
                write!(f, "Usize {{ cmd: {cmd:#x}, arg: {arg:#x} }}")
            }
            Call::Typed { cmd, data } => {
                write!(f, "Typed {{ cmd: {cmd:#x}, data: {data:02x?} }}")
            }
        }
    }
}

#[cfg(test)]
impl Call {
    fn cmd(&self) -> i32 {
        match self {
            Call::Usize { cmd, .. } | Call::Typed { cmd, .. } => *cmd,
        }
    }

    /// The value this call sent the kernel.
    fn value(&self) -> usize {
        match self {
            Call::Usize { arg, .. } => *arg,
            Call::Typed { cmd, .. } => panic!("cmd {cmd:#x} sent no value"),
        }
    }

    /// The payload this call sent the kernel, read back.
    fn payload<T: Copy>(&self) -> T {
        let Call::Typed { cmd, data } = self else {
            panic!("cmd {:#x} sent no payload", self.cmd())
        };
        assert_eq!(
            data.len(),
            size_of::<T>(),
            "cmd {cmd:#x} sent {} bytes",
            data.len(),
        );
        // Safety: the bytes came from one T. A Vec buffer has no
        // alignment guarantee, so the read is unaligned.
        unsafe { std::ptr::read_unaligned(data.as_ptr().cast::<T>()) }
    }
}

pub struct VionaFd(Chan);
impl VionaFd {
    /// Open the viona device and bind it to link `link_id` and VMM
    /// instance `vm`.
    pub fn new(link_id: u32, vm: BorrowedFd<'_>) -> Result<Self> {
        let this = Self::open()?;
        this.create(link_id, vm)?;
        Ok(this)
    }

    /// Bind an open handle to a link and a VM.
    ///
    /// Separate from [`Self::new`], which opens the real device first,
    /// so a recorder can check the field order.
    fn create(&self, link_id: u32, vm: BorrowedFd<'_>) -> Result<()> {
        let mut vna_create = vioc_create {
            c_linkid: link_id,
            c_vmfd: vm.as_raw_fd(),
        };
        // Safety: the kernel reads one vioc_create for this command.
        let _ = unsafe { self.ioctl(VNA_IOC_CREATE, &mut vna_create) }?;
        Ok(())
    }

    /// Open a viona device instance with no other initialization.
    pub fn open() -> Result<Self> {
        let fp = OpenOptions::new()
            .read(true)
            .write(true)
            .open(VIONA_DEV_PATH)?;

        Ok(Self(Chan::Dev(fp)))
    }

    fn file(&self) -> &File {
        match &self.0 {
            Chan::Dev(fp) => fp,
            #[cfg(test)]
            Chan::Record(fp, _) => fp,
        }
    }

    /// Issue an ioctl on the open viona instance.
    ///
    /// # Safety
    ///
    /// `data` must be valid for every copyin and copyout the ioctl does.
    pub unsafe fn ioctl<T>(&self, cmd: i32, data: *mut T) -> Result<i32> {
        match &self.0 {
            Chan::Dev(fp) => {
                ioctl(fp.as_raw_fd(), cmd, data as *mut libc::c_void)
            }
            // A recorder keeps the input bytes, which show the payload
            // field order. It then writes back the reply the test set for
            // this command, so a copy-out differs from the input.
            //
            // Safety: by the caller contract, `data` points at one
            // initialized T, writable for the same length. Every ioctl
            // payload names its padding, so no byte read here is
            // uninitialized, and the reply has the size of T.
            #[cfg(test)]
            Chan::Record(_, rec) => {
                let sent = std::slice::from_raw_parts(
                    data.cast::<u8>(),
                    size_of::<T>(),
                )
                .to_vec();
                let mut rec = rec.lock().expect("the recorded calls");
                rec.calls.push(Call::Typed { cmd, data: sent });
                if let Some(reply) = rec.replies.get(&cmd) {
                    assert_eq!(
                        reply.len(),
                        size_of::<T>(),
                        "cmd {cmd:#x} copies out {} bytes",
                        size_of::<T>(),
                    );
                    std::ptr::copy_nonoverlapping(
                        reply.as_ptr(),
                        data.cast::<u8>(),
                        reply.len(),
                    );
                }
                Ok(0)
            }
        }
    }

    pub fn ioctl_usize(&self, cmd: i32, data: usize) -> Result<i32> {
        if !Self::ioctl_usize_safe(cmd) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "unsafe cmd provided",
            ));
        }
        match &self.0 {
            // Safety: `ioctl_usize_safe` admits only commands that do
            // not treat the data argument as a copyin/copyout pointer.
            // The caller owns any other side effects.
            Chan::Dev(fp) => unsafe {
                ioctl(fp.as_raw_fd(), cmd, data as *mut libc::c_void)
            },
            // A recorder returns 1, a valid version, so the
            // `api_version` assertion passes.
            #[cfg(test)]
            Chan::Record(_, rec) => {
                rec.lock()
                    .expect("the recorded calls")
                    .calls
                    .push(Call::Usize { cmd, arg: data });
                Ok(1)
            }
        }
    }

    /// Read the features the kernel driver offers.
    pub fn get_features(&self) -> Result<u64> {
        let mut feat: u64 = 0;
        // Safety: the command copies out one u64, which `feat` is.
        unsafe { self.ioctl(VNA_IOC_GET_FEATURES, &mut feat) }?;
        Ok(feat)
    }

    /// Query the API version exposed by the kernel VMM.
    pub fn api_version(&self) -> Result<u32> {
        let vers = self.ioctl_usize(VNA_IOC_VERSION, 0)?;

        // VNA_IOC_VERSION must return a positive version.
        assert!(vers > 0);
        Ok(vers as u32)
    }

    /// The minor number of the viona device instance, which matches
    /// kernel statistic entries to the device.
    pub fn instance_id(&self) -> Result<u32> {
        let meta = self.file().metadata()?;
        Ok(minor(&meta))
    }

    /// Whether `cmd` is a viona ioctl that does no copyin or copyout.
    const fn ioctl_usize_safe(cmd: i32) -> bool {
        matches!(
            cmd,
            VNA_IOC_DELETE
                | VNA_IOC_RING_RESET
                | VNA_IOC_RING_KICK
                | VNA_IOC_RING_PAUSE
                | VNA_IOC_RING_INTR_CLR
                | VNA_IOC_VERSION
                | VNA_IOC_SET_NOTIFY_IOP
                | VNA_IOC_SET_PROMISC
                | VNA_IOC_GET_MTU
                | VNA_IOC_SET_MTU
                | VNA_IOC_GET_PAIRS
                | VNA_IOC_SET_PAIRS
                | VNA_IOC_GET_USEPAIRS
                | VNA_IOC_SET_USEPAIRS,
        )
    }
}
impl AsRawFd for VionaFd {
    fn as_raw_fd(&self) -> RawFd {
        self.file().as_raw_fd()
    }
}

#[cfg(target_os = "illumos")]
fn minor(meta: &std::fs::Metadata) -> u32 {
    // libc makes minor() a const fn on most Unix platforms, but not on
    // illumos, so this wrapper calls it in an unsafe block. Viona runs
    // only on illumos.
    unsafe { libc::minor(meta.rdev()) }
}
#[cfg(not(target_os = "illumos"))]
fn minor(meta: &std::fs::Metadata) -> u32 {
    let _rdev = meta.rdev();
    panic!("illumos required");
}

/// Viona API versions and the change each one added.
#[repr(u32)]
#[derive(Copy, Clone)]
pub enum ApiVersion {
    /// Adds multi-queue support and changes per-queue interrupt polling
    /// to a compact bitmap.
    V6 = 6,

    /// Adds support for virtio 1.0 (modern) virtqueues.
    V5 = 5,

    /// Adds support for getting and setting the MTU.
    V4 = 4,

    /// Adds support for interface parameters.
    V3 = 3,

    /// Adds support for non-VNIC datalink devices.
    V2 = 2,

    /// The first version available for query.
    V1 = 1,
}
impl ApiVersion {
    pub const fn current() -> Self {
        Self::V6
    }
}
impl PartialEq<ApiVersion> for u32 {
    fn eq(&self, other: &ApiVersion) -> bool {
        *self == *other as u32
    }
}
impl PartialOrd<ApiVersion> for u32 {
    fn partial_cmp(&self, other: &ApiVersion) -> Option<std::cmp::Ordering> {
        Some(self.cmp(&(*other as u32)))
    }
}

use vmm_api_common::CachedVersion;

static VERSION_CACHE: CachedVersion = CachedVersion::new();

/// Query the API version from the viona device on the system.
///
/// Caches the version, or the error, for later calls. The VM can need
/// the version at runtime, where a new query would delay the guest.
pub fn api_version() -> Result<u32> {
    VERSION_CACHE.get_or_init(|| -> Result<u32> {
        let ctl = VionaFd::open()?;
        let vers = ctl.api_version()?;
        Ok(vers)
    })
}

#[cfg(test)]
mod test;
