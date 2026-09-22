// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The public surface `bin/rshyve` and `bin/firehyve` both call.
//!
//! Exercised from outside the crate so a private-item move cannot
//! quietly change what a binary can reach.

use std::fs;
use std::path::PathBuf;

use vmm_boot::direct::{InitrdImage, KernelImage};
use vmm_boot::{BootImage, BootProtocol};

/// The smallest bzImage `KernelImage::open` accepts: "HdrS" at 0x202,
/// boot protocol 0x020F, XLF_KERNEL_64 set, one setup sector, and a
/// preferred load address of 16 MiB.
fn synthetic_bzimage() -> Vec<u8> {
    let mut img = vec![0u8; 0x1000];
    img[0x1F1] = 1;
    img[0x202..0x206].copy_from_slice(b"HdrS");
    img[0x206..0x208].copy_from_slice(&0x020Fu16.to_le_bytes());
    img[0x236..0x238].copy_from_slice(&1u16.to_le_bytes());
    img[0x258..0x260].copy_from_slice(&0x0100_0000u64.to_le_bytes());
    img
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "vmm-boot-{}-{}",
        std::process::id(),
        name
    ))
}

#[test]
fn kernel_entry_point_is_pref_address_plus_0x200() {
    let path = scratch("kernel.bin");
    fs::write(&path, synthetic_bzimage()).expect("write kernel");
    let ki = KernelImage::open(&path).expect("open kernel");
    assert_eq!(ki.entry_point_64(), 0x0100_0200);
    fs::remove_file(&path).ok();
}

#[test]
fn kernel_without_hdrs_magic_is_rejected() {
    let mut img = synthetic_bzimage();
    img[0x202..0x206].copy_from_slice(b"xxxx");
    let path = scratch("nomagic.bin");
    fs::write(&path, img).expect("write kernel");
    let Err(err) = KernelImage::open(&path) else {
        panic!("HdrS magic must be checked");
    };
    assert!(format!("{err:#}").contains("HdrS"));
    fs::remove_file(&path).ok();
}

#[test]
fn empty_initrd_is_rejected() {
    let path = scratch("initrd.img");
    fs::write(&path, b"").expect("write initrd");
    let Err(err) = InitrdImage::open(&path) else {
        panic!("an empty initrd must be rejected");
    };
    assert!(format!("{err:#}").contains("initrd is empty"));
    fs::remove_file(&path).ok();
}

/// `vmm_boot::KernelImage` and `vmm_boot::direct::KernelImage` must be
/// the same type, and `vmm_boot::load_kernel` /
/// `vmm_boot::setup_direct_boot_bsp` must exist at the crate root.
/// This test compiles only while lib.rs has the re-exports.
#[test]
fn crate_root_reexports_resolve() {
    let _load: fn(
        &vmm_core::mem::MemCtx,
        &KernelImage,
        &str,
        Option<&InitrdImage>,
        usize,
        u64,
    ) -> anyhow::Result<u64> = vmm_boot::load_kernel;

    let _bsp: fn(&vmm_core::vcpu::Vcpu, u64) -> anyhow::Result<()> =
        vmm_boot::setup_direct_boot_bsp;

    let _pvh: fn(Vec<u8>) -> anyhow::Result<vmm_boot::PvhKernel> =
        vmm_boot::PvhKernel::from_bytes;

    let _detect: fn(&[u8]) -> anyhow::Result<BootProtocol> =
        vmm_boot::detect_boot_protocol;
}

/// `BootImage::open` is the only kernel entry point a binary calls, so
/// the protocol comes from the file and never from a flag.
#[test]
fn boot_image_open_classifies_a_bzimage_on_disk() {
    let path = scratch("autodetect.bin");
    fs::write(&path, synthetic_bzimage()).expect("write kernel");
    let img = BootImage::open(&path).expect("open kernel");
    assert_eq!(img.protocol(), BootProtocol::Bzimage);
    assert_eq!(img.entry_point(), 0x0100_0200);
    fs::remove_file(&path).ok();
}

/// An unrecognized file is an error, never a guessed protocol. A guess
/// enters the guest at an address that does not match its register
/// state, which surfaces as an unexplained triple fault.
#[test]
fn boot_image_open_rejects_an_unrecognized_file() {
    let path = scratch("garbage.bin");
    fs::write(&path, vec![0u8; 0x1000]).expect("write file");
    let Err(err) = BootImage::open(&path) else {
        panic!("an unrecognized image must be rejected");
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("unrecognized kernel image"), "got: {msg}");
    fs::remove_file(&path).ok();
}
