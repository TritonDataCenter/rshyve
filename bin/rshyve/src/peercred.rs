// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Portable peer credentials for Unix domain sockets.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCred {
    pub uid: u32,
    pub gid: u32,
    pub zoneid: Option<i32>,
}

#[cfg(any(target_os = "illumos", target_os = "solaris"))]
pub fn peer_cred(stream: &UnixStream) -> io::Result<PeerCred> {
    struct Ucred(*mut libc::ucred_t);

    impl Drop for Ucred {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    libc::ucred_free(self.0);
                }
            }
        }
    }

    let mut raw = std::ptr::null_mut();
    let result = unsafe { libc::getpeerucred(stream.as_raw_fd(), &mut raw) };
    let cred = Ucred(raw);
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if cred.0.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "getpeerucred returned a null credential",
        ));
    }

    Ok(PeerCred {
        uid: unsafe { libc::ucred_geteuid(cred.0) },
        gid: unsafe { libc::ucred_getegid(cred.0) },
        zoneid: Some(unsafe { libc::ucred_getzoneid(cred.0) }),
    })
}

#[cfg(target_os = "linux")]
pub fn peer_cred(stream: &UnixStream) -> io::Result<PeerCred> {
    let mut cred = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            cred.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if len as usize != size_of::<libc::ucred>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SO_PEERCRED returned an unexpected credential size",
        ));
    }
    let cred = unsafe { cred.assume_init() };

    Ok(PeerCred {
        uid: cred.uid,
        gid: cred.gid,
        zoneid: None,
    })
}

#[cfg(target_os = "macos")]
pub fn peer_cred(stream: &UnixStream) -> io::Result<PeerCred> {
    let mut uid = 0;
    let mut gid = 0;
    let result =
        unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(PeerCred {
        uid,
        gid,
        zoneid: None,
    })
}

#[cfg(not(any(
    target_os = "illumos",
    target_os = "solaris",
    target_os = "linux",
    target_os = "macos",
)))]
pub fn peer_cred(_stream: &UnixStream) -> io::Result<PeerCred> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Unix peer credentials are unsupported on this platform",
    ))
}

#[cfg(any(target_os = "illumos", target_os = "solaris"))]
pub fn process_zoneid() -> io::Result<Option<i32>> {
    unsafe extern "C" {
        fn getzoneid() -> libc::zoneid_t;
    }

    Ok(Some(unsafe { getzoneid() }))
}

#[cfg(not(any(target_os = "illumos", target_os = "solaris")))]
pub fn process_zoneid() -> io::Result<Option<i32>> {
    Ok(None)
}

// Only the Linux and macOS arms run in a unit test. The illumos arm needs
// a real zone. The tests in control/listener.rs cover the authorization
// policy.
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn peer_credentials_match_current_process() {
        let (left, _right) =
            UnixStream::pair().expect("create Unix stream pair");

        let cred = peer_cred(&left).expect("read peer credentials");

        assert_eq!(cred.uid, unsafe { libc::geteuid() });
        assert_eq!(cred.gid, unsafe { libc::getegid() });
        assert_eq!(cred.zoneid, None);
    }
}
