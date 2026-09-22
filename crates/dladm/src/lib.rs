// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Datalink lookups through libdladm.
//!
//! A viona device needs the kernel link id of a VNIC and its MAC. The
//! MAC comes from `dladm_vnic_info`, which works inside a bhyve zone
//! where `dladm show-vnic` lacks the privilege to answer.

use std::io::{Error, ErrorKind, Result};

mod sys;

/// Bytes in an Ethernet address.
pub const ETHERADDRL: usize = 6;

/// What the kernel knows about one VNIC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VnicInfo {
    pub link_id: u32,
    pub mac_addr: [u8; ETHERADDRL],
}

/// An open libdladm handle.
pub struct Handle {
    #[cfg(target_os = "illumos")]
    raw: sys::dladm_handle_t,
}

impl Handle {
    #[cfg(target_os = "illumos")]
    pub fn open() -> Result<Self> {
        let mut hdl: sys::dladm_handle_t = std::ptr::null_mut();
        // Safety: dladm_open writes one handle pointer.
        let rc = unsafe { sys::dladm_open(&mut hdl) };
        status(rc, "dladm_open")?;
        Ok(Self { raw: hdl })
    }

    #[cfg(not(target_os = "illumos"))]
    pub fn open() -> Result<Self> {
        Err(unsupported())
    }

    /// Resolve `name` to its link id and MAC address.
    #[cfg(target_os = "illumos")]
    pub fn vnic(&self, name: &str) -> Result<VnicInfo> {
        let name_c = std::ffi::CString::new(name).map_err(|_| {
            Error::new(ErrorKind::InvalidInput, "VNIC name contains a NUL")
        })?;
        let mut link_id: sys::datalink_id_t = 0;
        // Safety: the handle is open, the name is NUL terminated, and
        // the out-pointers that are not null point at live locals.
        let rc = unsafe {
            sys::dladm_name2info(
                self.raw,
                name_c.as_ptr(),
                &mut link_id,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        status(rc, "dladm_name2info")
            .map_err(|e| Error::new(e.kind(), format!("{name}: {e}")))?;

        let mut attr = sys::dladm_vnic_attr_t::zeroed();
        // Safety: `attr` is the head of the struct the call fills, and
        // the padding covers the rest of it.
        let rc = unsafe {
            sys::dladm_vnic_info(
                self.raw,
                link_id,
                &mut attr,
                sys::DLADM_OPT_ACTIVE,
            )
        };
        status(rc, "dladm_vnic_info")
            .map_err(|e| Error::new(e.kind(), format!("{name}: {e}")))?;
        if (attr.va_mac_len as usize) < ETHERADDRL {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("{name}: MAC address is {} bytes", attr.va_mac_len),
            ));
        }
        let mut mac_addr = [0u8; ETHERADDRL];
        mac_addr.copy_from_slice(&attr.va_mac_addr[..ETHERADDRL]);
        Ok(VnicInfo { link_id, mac_addr })
    }

    #[cfg(not(target_os = "illumos"))]
    pub fn vnic(&self, _name: &str) -> Result<VnicInfo> {
        Err(unsupported())
    }
}

#[cfg(target_os = "illumos")]
impl Drop for Handle {
    fn drop(&mut self) {
        // Safety: the handle came from dladm_open and is closed once.
        unsafe { sys::dladm_close(self.raw) }
    }
}

// Safety: libdladm handles carry no thread affinity.
unsafe impl Send for Handle {}

#[cfg(not(target_os = "illumos"))]
fn unsupported() -> Error {
    Error::new(
        ErrorKind::Unsupported,
        "libdladm is only available on illumos",
    )
}

/// Turn a `dladm_status_t` into an error naming the call.
#[cfg(target_os = "illumos")]
fn status(rc: libc::c_int, call: &str) -> Result<()> {
    match rc {
        0 => Ok(()),
        sys::DLADM_STATUS_NOTFOUND => Err(Error::new(
            ErrorKind::NotFound,
            format!("{call}: no such datalink"),
        )),
        rc => Err(Error::other(format!("{call}: dladm status {rc}"))),
    }
}
