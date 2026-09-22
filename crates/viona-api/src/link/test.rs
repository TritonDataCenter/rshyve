// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What each link call sends the kernel.
//!
//! The recording channel checks, for every method, the command and the
//! argument in each position. The errno shows neither: a wrong entry
//! point or swapped payload fields give the same errno as the correct
//! call.
//!
//! The `/dev/null` handle reaches the kernel and shows only the errno,
//! which proves that a call still makes a real ioctl.

use std::fs::File;
use std::io::{ErrorKind, PipeReader, Write};
use std::os::fd::OwnedFd;
use std::sync::mpsc;
use std::time::Duration;

use super::*;
use crate::test::{
    answers_with, calls, cmds, not_a_link, recorder, recorder_on,
};
use crate::{Call, VIONA_PROMISC_ALL};

/// A wait this long means a thread is stuck, not slow.
const WEDGED: Duration = Duration::from_secs(15);

/// How long the test watches a wait that must not end.
///
/// A correct wait blocks in `poll(2)` with no deadline.
const PARKED: Duration = Duration::from_millis(200);

/// A recorder whose descriptor the test can make ready.
///
/// A pipe read end never reports `POLLRDBAND`, the viona interrupt band,
/// so a wait on it ends only as the test arranges.
fn link_on(reader: PipeReader) -> VionaFd {
    recorder_on(File::from(OwnedFd::from(reader)))
}

/// One wait, running on its own thread.
struct Waiting {
    /// The wait result, when it ends.
    end: mpsc::Receiver<Result<IntrWait>>,
    /// The wait thread, for a test that signals it.
    thread: Tid,
}

/// A thread id a test can send across a channel.
///
/// `pthread_t` is a pointer on macOS, so it is not `Send`.
#[derive(Clone, Copy)]
struct Tid(libc::pthread_t);

// Safety: the value only names a thread, and every test that holds one
// waits for that thread to end before it drops the id.
unsafe impl Send for Tid {}

/// Run one wait on its own thread.
///
/// A wait that never ends must fail the test, not hang it, so no test
/// calls it on the test thread.
fn spawn_wait(fd: VionaFd, stop: PipeReader) -> Waiting {
    let (tx, end) = mpsc::channel();
    let (named, id) = mpsc::channel();
    std::thread::Builder::new()
        .name("wait-intr".into())
        .spawn(move || {
            // Safety: pthread_self names the calling thread.
            let me = Tid(unsafe { libc::pthread_self() });
            named.send(me).expect("the test reads this");
            tx.send(fd.wait_intr(&stop))
                .expect("the test waits for this");
        })
        .expect("the test can spawn a thread");
    let thread = id.recv_timeout(WEDGED).expect("the thread named itself");
    Waiting { end, thread }
}

/// Whether this platform separates the viona interrupt band from
/// ordinary readability.
///
/// Viona signals a pending interrupt in a priority band. macOS maps
/// every read-side poll bit to one select filter, so a readable pipe
/// reports `POLLRDBAND` there. illumos and Linux keep them separate,
/// and the tests run there for real, so a false result on those
/// platforms is a failure, not a skip.
fn tells_a_band_from_a_byte() -> bool {
    let (rx, mut tx) = std::io::pipe().expect("a pipe");
    tx.write_all(b"x").expect("the pipe takes a byte");
    let mut pfd = libc::pollfd {
        fd: rx.as_raw_fd(),
        events: libc::POLLRDBAND,
        revents: 0,
    };
    // Safety: one descriptor, open for this call, and the count
    // matches the array.
    let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
    assert!(ret >= 0, "poll: {}", std::io::Error::last_os_error());

    let apart = pfd.revents & libc::POLLRDBAND == 0;
    assert!(
        apart || !cfg!(any(target_os = "illumos", target_os = "linux")),
        "this platform no longer keeps the poll bands apart, so the \
         wait cannot tell an interrupt from a byte",
    );
    apart
}

/// The result of one spawned wait.
fn waited(waiting: &Waiting) -> IntrWait {
    waiting
        .end
        .recv_timeout(WEDGED)
        .expect("the wait ended")
        .expect("the poll answered")
}

/// Make `SIGUSR1` interrupt a system call on this process.
///
/// No `SA_RESTART`: with it the C library restarts the call, and the
/// retry under test never runs. The handler does nothing, so the only
/// effect is the `EINTR`.
fn sigusr1_interrupts_a_call() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    extern "C" fn note(_sig: libc::c_int) {}

    INSTALLED.call_once(|| {
        // Safety: the struct is initialized before the call, and the
        // handler is a plain function that touches nothing.
        let rc = unsafe {
            let mut act: libc::sigaction = std::mem::zeroed();
            act.sa_sigaction =
                note as extern "C" fn(libc::c_int) as libc::sighandler_t;
            act.sa_flags = 0;
            libc::sigemptyset(&mut act.sa_mask);
            libc::sigaction(libc::SIGUSR1, &act, std::ptr::null_mut())
        };
        assert_eq!(rc, 0, "sigaction: {}", std::io::Error::last_os_error());
    });
}

#[test]
fn every_link_call_sends_the_command_its_name_says() {
    // A wrong command reaches another viona entry point and reports
    // success. `delete` matters most: any other command leaves the
    // viona vmm_drv hold in place, and VM_DESTROY_SELF then waits for it
    // in an untimed cv_wait.
    let fd = recorder();

    fd.ring_reset(1).expect("a recorder answers");
    fd.ring_kick(1).expect("a recorder answers");
    fd.ring_pause(1).expect("a recorder answers");
    fd.ring_intr_clr(1).expect("a recorder answers");
    fd.set_notify_iop(0x2000).expect("a recorder answers");
    fd.set_promisc(PromiscMode::All)
        .expect("a recorder answers");
    fd.delete().expect("a recorder answers");

    assert_eq!(
        calls(&fd),
        [
            Call::Usize {
                cmd: VNA_IOC_RING_RESET,
                arg: 1
            },
            Call::Usize {
                cmd: VNA_IOC_RING_KICK,
                arg: 1
            },
            Call::Usize {
                cmd: VNA_IOC_RING_PAUSE,
                arg: 1
            },
            Call::Usize {
                cmd: VNA_IOC_RING_INTR_CLR,
                arg: 1
            },
            Call::Usize {
                cmd: VNA_IOC_SET_NOTIFY_IOP,
                arg: 0x2000
            },
            Call::Usize {
                cmd: VNA_IOC_SET_PROMISC,
                arg: VIONA_PROMISC_ALL as usize
            },
            Call::Usize {
                cmd: VNA_IOC_DELETE,
                arg: 0
            },
        ],
    );
}

#[test]
fn every_link_call_that_takes_a_struct_sends_the_command_its_name_says() {
    // The commands that copy a payload, in order. The tests below check
    // each payload.
    let fd = recorder();

    fd.ring_init(1, 256, 0x1000, 0x2000, 0x3000)
        .expect("a recorder answers");
    fd.ring_set_state(&vioc_ring_state::default())
        .expect("a recorder answers");
    fd.ring_get_state(1).expect("a recorder answers");
    fd.ring_set_msi(1, 0xFEE0_0000, 0x21)
        .expect("a recorder answers");
    fd.intr_status().expect("a recorder answers");
    fd.set_features(1).expect("a recorder answers");
    fd.set_notify_mmio(0xC000_0000, 0x1000)
        .expect("a recorder answers");

    assert_eq!(
        cmds(&fd),
        [
            VNA_IOC_RING_INIT_MODERN,
            VNA_IOC_RING_SET_STATE,
            VNA_IOC_RING_GET_STATE,
            VNA_IOC_RING_SET_MSI,
            VNA_IOC_INTR_POLL,
            VNA_IOC_SET_FEATURES,
            VNA_IOC_SET_NOTIFY_MMIO,
        ],
    );
}

#[test]
fn every_ring_call_sends_the_ring_it_was_given() {
    // A call that drops its argument always works on ring 0. The halt
    // would then leave every other ring running, and the delete would
    // wait for each of them.
    for ring in [0u16, 1, 7, u16::MAX] {
        let fd = recorder();

        fd.ring_reset(ring).expect("a recorder answers");
        fd.ring_kick(ring).expect("a recorder answers");
        fd.ring_pause(ring).expect("a recorder answers");
        fd.ring_intr_clr(ring).expect("a recorder answers");

        let args: Vec<usize> = calls(&fd).iter().map(Call::value).collect();
        assert_eq!(args, [ring as usize; 4], "a call dropped ring {ring}");
    }
}

#[test]
fn a_ring_init_sends_the_addresses_the_guest_programmed() {
    // The in-kernel worker reads these three addresses. A swapped pair
    // makes it treat the used ring as the available ring and write
    // completions over entries the driver has not read.
    let fd = recorder();

    fd.ring_init(1, 256, 0x1_0000, 0x2_0000, 0x3_0000)
        .expect("a recorder answers");

    assert_eq!(
        calls(&fd)[0].payload::<vioc_ring_init_modern>(),
        vioc_ring_init_modern {
            rim_index: 1,
            rim_qsize: 256,
            rim_qaddr_desc: 0x1_0000,
            rim_qaddr_avail: 0x2_0000,
            rim_qaddr_used: 0x3_0000,
            _pad: [0; 2],
        },
    );
}

#[test]
fn a_ring_state_write_sends_the_state_it_was_given() {
    // A migration destination resumes from this. Every field is
    // distinct, so a reordered payload fails here, not in a guest that
    // replays descriptors.
    let fd = recorder();
    let state = vioc_ring_state {
        vrs_index: 1,
        vrs_avail_idx: 7,
        vrs_used_idx: 5,
        vrs_qsize: 256,
        vrs_qaddr_desc: 0x1_0000,
        vrs_qaddr_avail: 0x2_0000,
        vrs_qaddr_used: 0x3_0000,
    };

    fd.ring_set_state(&state).expect("a recorder answers");

    assert_eq!(calls(&fd)[0].payload::<vioc_ring_state>(), state);
}

#[test]
fn a_ring_state_read_asks_for_the_ring_it_was_given() {
    // The index is the only input field. A call that drops it always
    // reads ring 0, and a migration source then sends the ring 0
    // indices for every ring.
    let fd = recorder();
    let filled = vioc_ring_state {
        vrs_index: 1,
        vrs_avail_idx: 7,
        vrs_used_idx: 5,
        vrs_qsize: 256,
        vrs_qaddr_desc: 0x1_0000,
        vrs_qaddr_avail: 0x2_0000,
        vrs_qaddr_used: 0x3_0000,
    };
    answers_with(&fd, VNA_IOC_RING_GET_STATE, filled);

    let state = fd.ring_get_state(1).expect("a recorder answers");

    assert_eq!(
        calls(&fd)[0].payload::<vioc_ring_state>(),
        vioc_ring_state {
            vrs_index: 1,
            ..Default::default()
        },
    );
    // The kernel fills the indices, and the migration payload carries
    // them. A read that returned its input struct would report every
    // ring at 0, and the destination would replay descriptors the guest
    // reclaimed.
    assert_eq!(state, filled);
}

#[test]
fn a_ring_msi_sends_the_message_where_the_driver_asked() {
    // The kernel sends this message itself. A swapped address and data
    // word send the interrupt to the data word as an address, which
    // reaches no armed vector.
    let fd = recorder();

    fd.ring_set_msi(1, 0xFEE0_0000, 0x21)
        .expect("a recorder answers");

    assert_eq!(
        calls(&fd)[0].payload::<vioc_ring_msi>(),
        vioc_ring_msi {
            rm_index: 1,
            rm_addr: 0xFEE0_0000,
            rm_msg: 0x21,
            _pad: [0; 3],
        },
    );
}

#[test]
fn a_feature_write_sends_the_features_the_guest_accepted() {
    // VERSION_1 selects the modern ring layout. A call that sends 0
    // makes the kernel read a legacy ring at modern ring addresses.
    let fd = recorder();
    let features = (1u64 << 32) | 0x30;

    fd.set_features(features).expect("a recorder answers");

    assert_eq!(calls(&fd)[0].payload::<u64>(), features);
}

#[test]
fn a_notify_window_sends_its_address_and_its_size() {
    // The kernel receives the guest kick only for writes inside this
    // window, so a size of 0 sends every kick to userspace.
    let fd = recorder();

    fd.set_notify_mmio(0xC000_0000, 0x1000)
        .expect("a recorder answers");

    assert_eq!(
        calls(&fd)[0].payload::<vioc_notify_mmio>(),
        vioc_notify_mmio {
            vim_address: 0xC000_0000,
            vim_size: 0x1000,
            _pad: 0,
        },
    );
}

#[test]
fn an_interrupt_poll_reads_the_pending_rings() {
    // The kernel copies out the payload, so it goes in empty. A stale
    // input status would report rings the kernel never signalled.
    //
    // A read that returned an empty status would find no ring pending
    // on every wakeup. Nothing clears the kernel signal, the band stays
    // asserted, the wait returns at once forever, and one core spins
    // while the guest waits for an interrupt.
    let fd = recorder();
    // Only the tx ring, so the position of the pending entry is checked.
    let pending = vioc_intr_poll { vip_status: [0, 1] };
    answers_with(&fd, VNA_IOC_INTR_POLL, pending);

    let status = fd.intr_status().expect("a recorder answers");

    assert_eq!(
        calls(&fd)[0].payload::<vioc_intr_poll>(),
        vioc_intr_poll::default(),
    );
    assert_eq!(status, pending.vip_status);
}

#[test]
fn the_promiscuous_mode_goes_by_value() {
    // viona_ioc_set_promisc casts the data argument to its enum. A
    // pointer would set the mode to a userspace address, and the kernel
    // would refuse the call. A guest allowed to spoof its MAC would then
    // get no traffic.
    let fd = recorder();

    fd.set_promisc(PromiscMode::None)
        .expect("a recorder answers");
    fd.set_promisc(PromiscMode::Multi)
        .expect("a recorder answers");
    fd.set_promisc(PromiscMode::All)
        .expect("a recorder answers");

    assert_eq!(
        calls(&fd),
        [
            Call::Usize {
                cmd: VNA_IOC_SET_PROMISC,
                arg: 0
            },
            Call::Usize {
                cmd: VNA_IOC_SET_PROMISC,
                arg: 1
            },
            Call::Usize {
                cmd: VNA_IOC_SET_PROMISC,
                arg: 2
            },
        ],
    );
}

#[test]
fn each_link_call_sends_exactly_one_ioctl() {
    // A call that also reset a ring, or called the kernel twice, adds a
    // second entry. The halt order and the device tests depend on one
    // call per viona entry point.
    let fd = recorder();

    fd.ring_init(0, 8, 1, 2, 3).expect("a recorder answers");
    fd.ring_kick(0).expect("a recorder answers");
    fd.ring_pause(0).expect("a recorder answers");
    fd.ring_reset(0).expect("a recorder answers");
    fd.ring_intr_clr(0).expect("a recorder answers");
    fd.ring_set_state(&vioc_ring_state::default())
        .expect("a recorder answers");
    fd.ring_get_state(0).expect("a recorder answers");
    fd.ring_set_msi(0, 1, 2).expect("a recorder answers");
    fd.set_features(1).expect("a recorder answers");
    fd.intr_status().expect("a recorder answers");
    fd.set_notify_iop(1).expect("a recorder answers");
    fd.set_notify_mmio(1, 2).expect("a recorder answers");
    fd.set_promisc(PromiscMode::None)
        .expect("a recorder answers");
    fd.delete().expect("a recorder answers");

    assert_eq!(calls(&fd).len(), 14, "a link call sent more than one ioctl");
}

#[test]
fn every_link_call_reports_what_the_kernel_refused() {
    // /dev/null is not a link, so the ioctl refuses each call. A call
    // that dropped the result would report success, and the device
    // would treat the ring as programmed.
    let link = not_a_link();
    let outcomes = [
        ("ring_init", link.ring_init(0, 8, 1, 2, 3).is_err()),
        ("ring_kick", link.ring_kick(0).is_err()),
        ("ring_reset", link.ring_reset(0).is_err()),
        ("ring_pause", link.ring_pause(0).is_err()),
        (
            "ring_set_state",
            link.ring_set_state(&vioc_ring_state::default()).is_err(),
        ),
        ("ring_get_state", link.ring_get_state(0).is_err()),
        ("ring_set_msi", link.ring_set_msi(0, 1, 2).is_err()),
        ("set_features", link.set_features(1).is_err()),
        ("intr_status", link.intr_status().is_err()),
        ("ring_intr_clr", link.ring_intr_clr(0).is_err()),
        ("set_notify_iop", link.set_notify_iop(1).is_err()),
        ("set_notify_mmio", link.set_notify_mmio(1, 2).is_err()),
        ("set_promisc", link.set_promisc(PromiscMode::All).is_err()),
        ("delete", link.delete().is_err()),
    ];

    for (name, failed) in outcomes {
        assert!(failed, "{name} swallowed what the kernel refused");
    }
}

#[test]
fn a_failed_delete_reports_it() {
    // The halt calls this and continues to VM_DESTROY_SELF. A hidden
    // failure would hide the stuck destroy that follows.
    let err = not_a_link().delete().expect_err("/dev/null holds no link");
    assert_ne!(
        err.kind(),
        ErrorKind::InvalidInput,
        "the delete never reached the ioctl",
    );
}

#[test]
fn a_failed_ring_reset_reports_it() {
    // The halt resets every ring before the delete and reports each
    // failure. The kernel, not this call, refuses an out-of-range ring.
    let fd = not_a_link();
    for ring in [0, 1, u16::MAX] {
        let err = fd.ring_reset(ring).expect_err("/dev/null holds no link");
        assert_ne!(
            err.kind(),
            ErrorKind::InvalidInput,
            "ring {ring} never reached the ioctl",
        );
    }
}

#[test]
fn one_wakeup_is_read_the_way_the_poll_thread_needs_it() {
    // Every decision the wait makes, one row each.
    //
    // The stop descriptor is checked first. The halt closes the write
    // end, so it stays ready. A wakeup read as pending there would send
    // the poll thread back to a link the delete destroys next, and the
    // halt would join a thread that never exits.
    //
    // Viona signals on POLLRDBAND, not on ordinary readability. A hang
    // up or a closed descriptor stops the thread instead of spinning
    // it: none of those states clears by itself.
    //
    // The two `None` rows are events the wait never requests, so only a
    // mistake produces them. The caller reports them instead of waiting
    // again on a descriptor that stays ready.
    let stopped = Some(IntrWait::Stopped);
    let pending = Some(IntrWait::Pending);
    let table = [
        // link revents, stop revents, verdict
        (0, libc::POLLIN, stopped),
        (libc::POLLRDBAND, libc::POLLIN, stopped),
        (libc::POLLRDBAND, 0, pending),
        (libc::POLLRDBAND | libc::POLLHUP, 0, pending),
        (libc::POLLHUP, 0, stopped),
        (libc::POLLERR, 0, stopped),
        (libc::POLLNVAL, 0, stopped),
        (libc::POLLIN, 0, None),
        (0, 0, None),
    ];

    for (link, stop, want) in table {
        assert_eq!(
            poll_verdict(link, stop),
            want,
            "link {link:#x} and stop {stop:#x} were read wrong",
        );
    }
}

#[test]
fn the_wait_ends_when_the_halt_closes_the_stop_pipe() {
    // The halt closes the write end and joins the poll thread. A wait
    // that read that as pending would return to the link, and the halt
    // would wait for a thread that never exits.
    //
    // Where the platform allows, the link carries ordinary data, so the
    // wait must end for the stop descriptor and not for the link. macOS
    // reports that byte in the interrupt band, so there the link stays
    // idle.
    let (link_rx, mut link_tx) = std::io::pipe().expect("a pipe");
    let (stop_rx, stop_tx) = std::io::pipe().expect("a pipe");
    if tells_a_band_from_a_byte() {
        link_tx.write_all(b"x").expect("the pipe takes a byte");
    }

    let waiting = spawn_wait(link_on(link_rx), stop_rx);
    drop(stop_tx);

    assert_eq!(waited(&waiting), IntrWait::Stopped);
}

#[test]
fn the_wait_ends_when_the_stop_pipe_carries_a_byte() {
    // The halt ends this wait by closing the write end, and a closed end
    // reports POLLHUP whatever the wait requested. The wait must also
    // request readability on the stop descriptor. Otherwise a stop
    // signalled by a write goes unseen, and the halt joins the poll
    // thread forever.
    //
    // The link write end stays open, so only the byte can end this wait.
    let (link_rx, link_tx) = std::io::pipe().expect("a pipe");
    let (stop_rx, mut stop_tx) = std::io::pipe().expect("a pipe");

    let waiting = spawn_wait(link_on(link_rx), stop_rx);
    stop_tx.write_all(b"x").expect("the pipe takes a byte");

    assert_eq!(waited(&waiting), IntrWait::Stopped);
    drop(link_tx);
}

#[test]
fn a_signal_does_not_end_the_wait() {
    // poll(2) returns EINTR when a signal reaches this thread, and the
    // kernel interrupt is still pending. A wait that reported it would
    // end the poll thread on the first signal: the link stays up, and
    // the guest loses every later interrupt.
    //
    // Both write ends stay open, so only the signal reaches the wait
    // until the test closes the stop pipe.
    let (link_rx, link_tx) = std::io::pipe().expect("a pipe");
    let (stop_rx, stop_tx) = std::io::pipe().expect("a pipe");
    sigusr1_interrupts_a_call();

    let waiting = spawn_wait(link_on(link_rx), stop_rx);
    // The thread stays in poll(2) for the whole loop, so the signals
    // arrive there. One is enough to end a faulty wait.
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(10));
        // Safety: the thread sent its id above, and the assertion below
        // checks whether the thread still exists.
        let rc = unsafe { libc::pthread_kill(waiting.thread.0, libc::SIGUSR1) };
        // The thread sends its result before it exits, so a missing
        // thread has already sent the reason.
        assert_eq!(
            rc,
            0,
            "the wait thread left the run of signals, ending with {:?}",
            waiting.end.try_recv(),
        );
    }

    match waiting.end.recv_timeout(PARKED) {
        Err(mpsc::RecvTimeoutError::Timeout) => {}
        other => panic!("a signal ended the wait with {other:?}"),
    }

    // Released in all cases, so the thread never stays blocked.
    drop(stop_tx);
    assert_eq!(waited(&waiting), IntrWait::Stopped);
    drop(link_tx);
}

#[test]
fn the_wait_ends_when_the_link_hangs_up() {
    // A link whose write end is closed signals only POLLHUP, forever.
    // Anything but an end spins one core until the halt runs.
    if !tells_a_band_from_a_byte() {
        return;
    }
    let (link_rx, link_tx) = std::io::pipe().expect("a pipe");
    let (stop_rx, _stop_tx) = std::io::pipe().expect("a pipe");
    drop(link_tx);

    let waiting = spawn_wait(link_on(link_rx), stop_rx);

    assert_eq!(waited(&waiting), IntrWait::Stopped);
}

#[test]
fn a_readable_link_with_no_interrupt_does_not_end_the_wait() {
    // Viona signals interrupts in a priority band, not with ordinary
    // readability. A wait on POLLIN would return with nothing pending,
    // and the poll thread would query the status on every byte instead
    // of every interrupt.
    if !tells_a_band_from_a_byte() {
        return;
    }
    let (link_rx, mut link_tx) = std::io::pipe().expect("a pipe");
    let (stop_rx, stop_tx) = std::io::pipe().expect("a pipe");
    link_tx.write_all(b"x").expect("the pipe takes a byte");

    let waiting = spawn_wait(link_on(link_rx), stop_rx);

    match waiting.end.recv_timeout(PARKED) {
        Err(mpsc::RecvTimeoutError::Timeout) => {}
        other => panic!("a readable link ended the wait with {other:?}"),
    }

    // Released in all cases, so the thread never stays blocked.
    drop(stop_tx);
    assert_eq!(waited(&waiting), IntrWait::Stopped);
}
