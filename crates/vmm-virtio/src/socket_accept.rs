// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The accept loop these devices share with the rest of the workspace.

pub(crate) use vmm_core::unixsock::accept::accept_loop;
#[cfg(test)]
pub(crate) use vmm_core::unixsock::accept::accepted_blocking;
