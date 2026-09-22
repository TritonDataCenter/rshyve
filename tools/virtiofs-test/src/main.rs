// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `virtiofs-test`: PID 1 that mounts a virtio-fs share and reports on
//! it, then powers the VM off.
//!
//! The `init` module documents the roles, the `exec-probe` copy and the
//! `virtiofs.*` kernel command line parameters. The `hotplug` role reads
//! its own `hotplug.*` keys, which [`hotplug::HotplugSpec`] documents.
//!
//! Cross-compile target: `x86_64-unknown-linux-musl`. The binary must be
//! statically linked: the initramfs has no dynamic loader, and the copy
//! on the share must also run without one.

mod cmdline;
mod hotplug;
mod report;
mod spec;

#[cfg(target_os = "linux")]
mod checks;
#[cfg(target_os = "linux")]
mod container;
#[cfg(target_os = "linux")]
mod hotplug_checks;
#[cfg(target_os = "linux")]
mod init;
#[cfg(target_os = "linux")]
mod mount;

#[cfg(target_os = "linux")]
fn main() -> ! {
    init::main()
}

/// The guest runs only on Linux, but the spec parsing is host
/// independent. A stub entry point lets `cargo test` run on any host.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("virtiofs-test runs as PID 1 inside a Linux guest");
    std::process::exit(2);
}
