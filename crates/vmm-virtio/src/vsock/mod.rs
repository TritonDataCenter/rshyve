// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! virtio-vsock: multiplexed streams between host and guest.

pub mod conn;
pub mod control;
pub mod device;
pub mod mux;
pub mod packet;
