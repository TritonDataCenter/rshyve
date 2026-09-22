// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// The declarations follow crates/dladm/src/sys.rs in Propolis.

//! The libdladm entry points this crate calls.

#![allow(non_camel_case_types)]

use libc::{c_int, c_uint};

// The struct layout is checked on every target, so a developer host
// catches a header change. Only the calls need illumos.
#[cfg(target_os = "illumos")]
pub use illumos_only::*;

pub type datalink_id_t = u32;

pub const MAXMACADDRLEN: usize = 20;

#[cfg(target_os = "illumos")]
mod illumos_only {
    use libc::c_int;

    /// Opaque handle to libdladm.
    pub enum dladm_handle {}
    pub type dladm_handle_t = *mut dladm_handle;

    /// The `DLADM_STATUS_NOTFOUND` member of `dladm_status_t`.
    pub const DLADM_STATUS_NOTFOUND: c_int = 5;

    /// `DLADM_OPT_ACTIVE`: read the running state, not the persistent one.
    pub const DLADM_OPT_ACTIVE: u32 = 0x1;
}

/// `mac_resource_props_t` in units of its own alignment.
///
/// The caller reads only the MAC, so the tail is opaque. It must still
/// have the C type's size and 8-byte alignment: libdladm may `bcopy` a
/// whole `mac_resource_props_t` into it, and a smaller or less aligned
/// tail would put the end of that copy past the Rust local.
const RESOURCE_PROPS_WORDS: usize = 1929;

/// `dladm_vnic_attr_t` from `libdlvnic.h`.
#[repr(C)]
pub struct dladm_vnic_attr_t {
    pub va_vnic_id: datalink_id_t,
    pub va_link_id: datalink_id_t,
    pub va_mac_addr_type: c_int,
    pub va_mac_len: c_uint,
    pub va_mac_addr: [u8; MAXMACADDRLEN],
    pub va_mac_slot: c_int,
    pub va_mac_prefix_len: c_uint,
    pub va_vid: u16,
    pub va_force: c_int,
    pub va_vrid: u32,
    pub va_af: c_int,
    _resource_props: [u64; RESOURCE_PROPS_WORDS],
}

/// Sizes from `mac_flow.h`: `mac_cpus_t` is 5144, `mac_protect_t` is
/// 9236, and the rest of `mac_resource_props_t` is 4 + 4 padding + 8 +
/// 4 + 4 + 4 + `MAXPATHLEN`.
const _: () = assert!(
    size_of::<dladm_vnic_attr_t>() == 15496,
    "dladm_vnic_attr_t no longer matches libdlvnic.h; libdladm would \
     write past it"
);
const _: () = assert!(align_of::<dladm_vnic_attr_t>() == 8);

#[cfg(target_os = "illumos")]
impl dladm_vnic_attr_t {
    pub fn zeroed() -> Self {
        // Safety: every field is plain data for which zero is valid.
        unsafe { std::mem::zeroed() }
    }
}

#[cfg(target_os = "illumos")]
#[link(name = "dladm")]
extern "C" {
    pub fn dladm_open(handle: *mut dladm_handle_t) -> c_int;
    pub fn dladm_close(handle: dladm_handle_t);
    pub fn dladm_name2info(
        handle: dladm_handle_t,
        link: *const libc::c_char,
        linkidp: *mut datalink_id_t,
        flagp: *mut u32,
        classp: *mut c_int,
        mediap: *mut u32,
    ) -> c_int;
    pub fn dladm_vnic_info(
        handle: dladm_handle_t,
        linkid: datalink_id_t,
        attrp: *mut dladm_vnic_attr_t,
        flags: u32,
    ) -> c_int;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::offset_of;

    /// Field offsets from `libdlvnic.h`. `va_resource_props` follows
    /// `va_af` at 56 after four bytes of alignment padding. A `[u8; _]`
    /// tail needs no alignment and would start at 60.
    #[test]
    fn vnic_attr_fields_sit_where_libdladm_writes_them() {
        assert_eq!(offset_of!(dladm_vnic_attr_t, va_mac_addr), 16);
        assert_eq!(offset_of!(dladm_vnic_attr_t, va_mac_slot), 36);
        assert_eq!(offset_of!(dladm_vnic_attr_t, va_vid), 44);
        assert_eq!(offset_of!(dladm_vnic_attr_t, va_force), 48);
        assert_eq!(offset_of!(dladm_vnic_attr_t, va_af), 56);
        assert_eq!(offset_of!(dladm_vnic_attr_t, _resource_props), 64);
        assert_eq!(size_of::<dladm_vnic_attr_t>(), 15496);
    }
}
