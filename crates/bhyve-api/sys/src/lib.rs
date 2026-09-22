// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod enums;
pub mod ioctls;
mod structs;
mod vmm_data;

pub use enums::*;
pub use ioctls::*;
pub use structs::*;
pub use vmm_data::*;

/// Maximum vCPUs per VM (matches kernel's VM_MAXCPU).
pub const VM_MAXCPU: usize = 64;

/// The VMM interface version this crate targets. All constants and structs
/// in the crate match this version.
pub const VMM_CURRENT_INTERFACE_VERSION: u32 = 18;
