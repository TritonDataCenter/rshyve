// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Thin wrappers over `mount(2)`, shared by the check and container
//! roles.

use std::ffi::CString;
use std::io;

/// Mount `src` on `target`, returning the raw error so a caller can act
/// on the errno rather than on a message.
fn raw(
    src: &str,
    target: &str,
    fstype: &str,
    flags: u64,
    data: Option<&str>,
) -> Result<(), io::Error> {
    let nul = |what: &str| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("{what} has NUL"))
    };
    let src_c = CString::new(src).map_err(|_| nul("source"))?;
    let target_c = CString::new(target).map_err(|_| nul("target"))?;
    let fstype_c = CString::new(fstype).map_err(|_| nul("fstype"))?;
    let data_c = match data {
        Some(d) => Some(CString::new(d).map_err(|_| nul("data"))?),
        None => None,
    };
    let data_ptr = match &data_c {
        Some(c) => c.as_ptr() as *const libc::c_void,
        None => std::ptr::null(),
    };
    let rc = unsafe {
        libc::mount(
            src_c.as_ptr(),
            target_c.as_ptr(),
            fstype_c.as_ptr(),
            flags,
            data_ptr,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Mount `src` on `target`, with optional flags and filesystem data.
pub fn mount(
    src: &str,
    target: &str,
    fstype: &str,
    flags: u64,
    data: Option<&str>,
) -> Result<(), String> {
    raw(src, target, fstype, flags, data)
        .map_err(|e| format!("mount {fstype} on {target}: {e}"))
}

/// Mount with no flags and no data, treating EBUSY as success.
///
/// EBUSY on /proc or /sys means the kernel mounted it already, which is
/// the outcome the caller wanted.
pub fn simple(src: &str, target: &str, fstype: &str) -> Result<(), String> {
    match raw(src, target, fstype, 0, None) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EBUSY) => Ok(()),
        Err(e) => Err(format!("mount {fstype} on {target}: {e}")),
    }
}

/// Move an existing mount to a new location, keeping it mounted.
pub fn move_to(src: &str, target: &str) -> Result<(), String> {
    raw(src, target, "", libc::MS_MOVE, None)
        .map_err(|e| format!("move mount {src} to {target}: {e}"))
}
