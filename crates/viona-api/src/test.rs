// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The version constants and the guard on `ioctl_usize`.
//!
//! [`crate::link`] tests what the link calls send. The test handles
//! for both files are built here.

use std::sync::Mutex;

use super::*;

/// An open fd that is not a viona link.
///
/// The ioctls reach the kernel and fail there, so the calls are
/// testable off illumos. On illumos, `mmioctl` returns ENXIO for every
/// ioctl on /dev/null.
pub(crate) fn not_a_link() -> VionaFd {
    VionaFd(Chan::Dev(dev_null()))
}

/// A handle that records the kernel calls a caller makes.
pub(crate) fn recorder() -> VionaFd {
    recorder_on(dev_null())
}

/// A recorder over one open file.
///
/// `wait_intr` polls the file, so a test of the wait supplies a
/// descriptor it can make ready.
pub(crate) fn recorder_on(fp: File) -> VionaFd {
    VionaFd(Chan::Record(fp, Mutex::new(Recording::default())))
}

/// The calls a recorder received, in order.
pub(crate) fn calls(fd: &VionaFd) -> Vec<Call> {
    recording(fd).calls.clone()
}

/// Set the bytes the kernel copies out for `cmd`.
///
/// Without this a caller reads back the struct it sent, and a method
/// that returns a constant looks correct.
pub(crate) fn answers_with<T: Copy>(fd: &VionaFd, cmd: i32, value: T) {
    // Safety: `value` is one initialized T, and every ioctl payload
    // names its padding, so no byte read here is uninitialized.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            std::ptr::from_ref(&value).cast::<u8>(),
            size_of::<T>(),
        )
    };
    recording(fd).replies.insert(cmd, bytes.to_vec());
}

fn recording(fd: &VionaFd) -> std::sync::MutexGuard<'_, Recording> {
    match &fd.0 {
        Chan::Record(_, rec) => rec.lock().expect("the recording"),
        Chan::Dev(_) => panic!("only a recorder keeps its calls"),
    }
}

/// The commands a recorder received, in order.
pub(crate) fn cmds(fd: &VionaFd) -> Vec<i32> {
    calls(fd).iter().map(Call::cmd).collect()
}

pub(crate) fn dev_null() -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .expect("every unix has /dev/null")
}

#[test]
fn latest_api_version() {
    let cur = ApiVersion::current();
    assert_eq!(VIONA_CURRENT_INTERFACE_VERSION, cur as u32);
}

#[test]
fn u32_comparisons() {
    assert!(1u32 < ApiVersion::V2);
    assert!(2u32 == ApiVersion::V2);
    assert!(3u32 > ApiVersion::V2);
}

/// Every command, against the value the kernel header computes.
///
/// uts/intel/sys/viona_io.h builds these from `('V' << 16) | ('C' << 8)`
/// plus a selector. The literals here are that arithmetic done by hand,
/// not through the helper under test.
///
/// A wrong DELETE matters most: it leaves the vmm_drv hold, and
/// VM_DESTROY_SELF then waits for it.
#[test]
fn the_commands_match_the_kernel() {
    assert_eq!(VNA_IOC_CREATE, 0x0056_4301);
    assert_eq!(VNA_IOC_DELETE, 0x0056_4302);
    assert_eq!(VNA_IOC_VERSION, 0x0056_4303);
    assert_eq!(VNA_IOC_DEFAULT_PARAMS, 0x0056_4304);
    assert_eq!(VNA_IOC_RING_INIT, 0x0056_4310);
    assert_eq!(VNA_IOC_RING_RESET, 0x0056_4311);
    assert_eq!(VNA_IOC_RING_KICK, 0x0056_4312);
    assert_eq!(VNA_IOC_RING_SET_MSI, 0x0056_4313);
    assert_eq!(VNA_IOC_RING_INTR_CLR, 0x0056_4314);
    assert_eq!(VNA_IOC_RING_SET_STATE, 0x0056_4315);
    assert_eq!(VNA_IOC_RING_GET_STATE, 0x0056_4316);
    assert_eq!(VNA_IOC_RING_PAUSE, 0x0056_4317);
    assert_eq!(VNA_IOC_RING_INIT_MODERN, 0x0056_4318);
    assert_eq!(VNA_IOC_INTR_POLL, 0x0056_4320);
    assert_eq!(VNA_IOC_SET_FEATURES, 0x0056_4321);
    assert_eq!(VNA_IOC_GET_FEATURES, 0x0056_4322);
    assert_eq!(VNA_IOC_SET_NOTIFY_IOP, 0x0056_4323);
    assert_eq!(VNA_IOC_SET_PROMISC, 0x0056_4324);
    assert_eq!(VNA_IOC_GET_PARAMS, 0x0056_4325);
    assert_eq!(VNA_IOC_SET_PARAMS, 0x0056_4326);
    assert_eq!(VNA_IOC_GET_MTU, 0x0056_4327);
    assert_eq!(VNA_IOC_SET_MTU, 0x0056_4328);
    assert_eq!(VNA_IOC_SET_NOTIFY_MMIO, 0x0056_4329);
    assert_eq!(VNA_IOC_INTR_POLL_MQ, 0x0056_432A);
    assert_eq!(VNA_IOC_GET_PAIRS, 0x0056_4330);
    assert_eq!(VNA_IOC_SET_PAIRS, 0x0056_4331);
    assert_eq!(VNA_IOC_GET_USEPAIRS, 0x0056_4332);
    assert_eq!(VNA_IOC_SET_USEPAIRS, 0x0056_4333);
}

/// A selector must not be reused. The kernel dispatches on the whole
/// number, so two names with one value are one entry point.
#[test]
fn no_two_commands_share_a_number() {
    let all = [
        ("VNA_IOC_CREATE", VNA_IOC_CREATE),
        ("VNA_IOC_DELETE", VNA_IOC_DELETE),
        ("VNA_IOC_VERSION", VNA_IOC_VERSION),
        ("VNA_IOC_DEFAULT_PARAMS", VNA_IOC_DEFAULT_PARAMS),
        ("VNA_IOC_RING_INIT", VNA_IOC_RING_INIT),
        ("VNA_IOC_RING_RESET", VNA_IOC_RING_RESET),
        ("VNA_IOC_RING_KICK", VNA_IOC_RING_KICK),
        ("VNA_IOC_RING_SET_MSI", VNA_IOC_RING_SET_MSI),
        ("VNA_IOC_RING_INTR_CLR", VNA_IOC_RING_INTR_CLR),
        ("VNA_IOC_RING_SET_STATE", VNA_IOC_RING_SET_STATE),
        ("VNA_IOC_RING_GET_STATE", VNA_IOC_RING_GET_STATE),
        ("VNA_IOC_RING_PAUSE", VNA_IOC_RING_PAUSE),
        ("VNA_IOC_RING_INIT_MODERN", VNA_IOC_RING_INIT_MODERN),
        ("VNA_IOC_INTR_POLL", VNA_IOC_INTR_POLL),
        ("VNA_IOC_SET_FEATURES", VNA_IOC_SET_FEATURES),
        ("VNA_IOC_GET_FEATURES", VNA_IOC_GET_FEATURES),
        ("VNA_IOC_SET_NOTIFY_IOP", VNA_IOC_SET_NOTIFY_IOP),
        ("VNA_IOC_SET_PROMISC", VNA_IOC_SET_PROMISC),
        ("VNA_IOC_GET_PARAMS", VNA_IOC_GET_PARAMS),
        ("VNA_IOC_SET_PARAMS", VNA_IOC_SET_PARAMS),
        ("VNA_IOC_GET_MTU", VNA_IOC_GET_MTU),
        ("VNA_IOC_SET_MTU", VNA_IOC_SET_MTU),
        ("VNA_IOC_SET_NOTIFY_MMIO", VNA_IOC_SET_NOTIFY_MMIO),
        ("VNA_IOC_INTR_POLL_MQ", VNA_IOC_INTR_POLL_MQ),
        ("VNA_IOC_GET_PAIRS", VNA_IOC_GET_PAIRS),
        ("VNA_IOC_SET_PAIRS", VNA_IOC_SET_PAIRS),
        ("VNA_IOC_GET_USEPAIRS", VNA_IOC_GET_USEPAIRS),
        ("VNA_IOC_SET_USEPAIRS", VNA_IOC_SET_USEPAIRS),
    ];
    for (i, (name, val)) in all.iter().enumerate() {
        for (other, other_val) in &all[i + 1..] {
            assert_ne!(
                val, other_val,
                "{name} and {other} name one viona entry point",
            );
        }
    }
}

/// The call that binds a handle to its link and its VM.
///
/// `c_linkid` and `c_vmfd` are adjacent 32-bit fields, so a swap still
/// compiles and sends a well-formed struct. The kernel would then look
/// up a link by a descriptor number and a VM by a link id.
/// `VionaFd::new` opens the real device first, so the test calls
/// `create` on a recorder.
#[test]
fn the_create_call_sends_the_link_and_the_vm_where_the_kernel_reads_them() {
    let fd = recorder();
    // Distinct values, neither plausible for the other field.
    let link_id: u32 = 0xAABB_CCDD;
    let vm_fd: RawFd = 0x0102_0304;

    // Safety: a recorder never gives the descriptor to the kernel, so it
    // does not need to be open.
    let vm = unsafe { BorrowedFd::borrow_raw(vm_fd) };
    fd.create(link_id, vm).expect("a recorder answers");

    let call = &calls(&fd)[0];
    assert_eq!(call.cmd(), VNA_IOC_CREATE);
    let sent = call.payload::<vioc_create>();
    assert_eq!(sent.c_linkid, link_id, "the link id moved");
    assert_eq!(sent.c_vmfd, vm_fd, "the vm descriptor moved");
}

#[test]
fn the_version_query_sends_the_command_its_name_says() {
    // The one call that is not a link operation: the binary queries the
    // version before it opens a link, and refuses a kernel that is too
    // old.
    let fd = recorder();

    assert_eq!(fd.api_version().expect("a recorder answers"), 1);

    assert_eq!(
        calls(&fd),
        [Call::Usize {
            cmd: VNA_IOC_VERSION,
            arg: 0
        }],
    );
}

#[test]
fn the_halt_commands_may_take_a_usize() {
    // delete() and ring_reset() use ioctl_usize. A cmd missing from the
    // allowlist is refused before the kernel, and the halt would then
    // leave the viona hold on the VM.
    assert!(VionaFd::ioctl_usize_safe(VNA_IOC_DELETE));
    assert!(VionaFd::ioctl_usize_safe(VNA_IOC_RING_RESET));
}

#[test]
fn no_command_that_copies_in_may_take_a_usize() {
    // ioctl_usize gives its argument to the kernel as the data pointer.
    // A cmd that copies in would read the usize as a user address that
    // the caller chooses.
    for cmd in [
        VNA_IOC_CREATE,
        VNA_IOC_DEFAULT_PARAMS,
        VNA_IOC_RING_INIT,
        VNA_IOC_RING_INIT_MODERN,
        VNA_IOC_RING_SET_MSI,
        VNA_IOC_RING_SET_STATE,
        VNA_IOC_RING_GET_STATE,
        VNA_IOC_INTR_POLL,
        VNA_IOC_INTR_POLL_MQ,
        VNA_IOC_SET_FEATURES,
        VNA_IOC_GET_FEATURES,
        VNA_IOC_SET_NOTIFY_MMIO,
        VNA_IOC_GET_PARAMS,
        VNA_IOC_SET_PARAMS,
    ] {
        assert!(
            !VionaFd::ioctl_usize_safe(cmd),
            "cmd {cmd:#x} copies in through its data argument",
        );
    }
}

#[test]
fn a_command_that_copies_in_is_refused_before_it_is_recorded() {
    // The allowlist applies at the call, not only in the const fn, and
    // before anything else sees the command.
    let fd = recorder();

    let err = fd
        .ioctl_usize(VNA_IOC_RING_SET_STATE, 0x4141_4141)
        .expect_err("a copyin cmd may not take a usize");

    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert!(calls(&fd).is_empty(), "a refused cmd reached the channel");
}

#[test]
fn a_command_that_copies_in_is_refused_at_the_kernel_call() {
    let err = not_a_link()
        .ioctl_usize(VNA_IOC_RING_SET_STATE, 0x4141_4141)
        .expect_err("a copyin cmd may not take a usize");
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[test]
fn the_features_the_kernel_offers_are_read_from_the_kernel() {
    // The device makes this call before it holds the link, and every
    // feature offered to the guest starts here. A read that returned a
    // constant would offer only MAC and STATUS and drop every kernel
    // feature: the guest would run a legacy ring layout on a modern
    // viona.
    let fd = recorder();
    // VERSION_1 and two more bits.
    let offered = (1u64 << 32) | 0x30;
    answers_with(&fd, VNA_IOC_GET_FEATURES, offered);

    assert_eq!(fd.get_features().expect("a recorder answers"), offered);

    assert_eq!(cmds(&fd), [VNA_IOC_GET_FEATURES]);
    // The kernel fills the payload, so it goes in empty.
    assert_eq!(calls(&fd)[0].payload::<u64>(), 0);
}

#[test]
fn a_failed_feature_read_reports_it() {
    // The device abandons the link instead of offering the guest
    // features the kernel never offered.
    let err = not_a_link()
        .get_features()
        .expect_err("/dev/null holds no link");
    assert_ne!(
        err.kind(),
        ErrorKind::InvalidInput,
        "the read never reached the ioctl",
    );
}
