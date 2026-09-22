// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bring up `lo` and one or more `ethN` interfaces with static IPv4
//! addresses, plus an optional default route, using the classic ioctl
//! path: SIOCSIFADDR + SIOCSIFNETMASK + SIOCSIFFLAGS, then SIOCADDRT
//! for the gateway.
//!
//! Netlink is not used: it costs either a dependency or about 300 lines
//! of hand-written message building. The ioctl path is six syscalls
//! with no parsing.

use std::mem::{size_of, zeroed};

use crate::spec::NetConfig;

const IFNAMSIZ: usize = 16;

// `ifreq` layout for x86_64 Linux. It is defined here because the libc
// crate sometimes hides its own definition behind cfg flags.
#[repr(C)]
struct Ifreq {
    name: [u8; IFNAMSIZ],
    // The kernel ifreq union is large enough to hold any of the
    // sockaddr_*/integer payloads. 24 bytes covers sockaddr_in (16),
    // ifr_flags (16-bit, padded), ifr_ifindex (32-bit, padded), and
    // route metadata.
    payload: [u8; 24],
}

// On musl/Linux, libc::ioctl takes the request as `c_int` (the Ioctl
// type alias). glibc uses `c_ulong`. `c_int` is correct because the
// static-musl init is the only build that runs.
const SIOCSIFFLAGS: libc::c_int = 0x8914;
const SIOCSIFADDR: libc::c_int = 0x8916;
const SIOCSIFNETMASK: libc::c_int = 0x891C;
const SIOCADDRT: libc::c_int = 0x890B;

const IFF_UP: i16 = 0x1;
const IFF_RUNNING: i16 = 0x40;

/// Bring `lo` up unconditionally. The kernel creates the loopback
/// netdev but leaves `IFF_UP` clear, which makes `bind 127.0.0.1` fail
/// with `EADDRNOTAVAIL`. Doing it from init means even a manifest with
/// no NIC still gets working loopback.
pub fn bring_up_lo() -> Result<(), String> {
    let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if sock < 0 {
        return Err(format!("socket(AF_INET): {}", last_err()));
    }
    let mut ifr: Ifreq = unsafe { zeroed() };
    write_name(&mut ifr.name, b"lo");
    let flags: i16 = IFF_UP | IFF_RUNNING;
    ifr.payload[..2].copy_from_slice(&flags.to_ne_bytes());
    let rc = unsafe { libc::ioctl(sock, SIOCSIFFLAGS, &ifr as *const _) };
    let res = if rc != 0 {
        Err(format!("SIOCSIFFLAGS lo: {}", last_err()))
    } else {
        Ok(())
    };
    unsafe { libc::close(sock) };
    res
}

/// Configure one interface (`ethN`) with the address from `cfg`.
///
/// The caller supplies the name: the first NIC in the spec is `eth0`,
/// the second is `eth1`, and so on, up to the four NIC slots fhrun
/// reserves.
///
/// `set_default_route` controls whether `cfg.gateway`, when present,
/// installs a system-wide default route through this NIC. Linux keeps
/// one default route per table, so the caller passes `true` only for
/// the NIC that should be the default. The convention is that the
/// first NIC with a gateway wins.
pub fn configure_iface(
    name: &str,
    cfg: &NetConfig,
    set_default_route: bool,
) -> Result<(), String> {
    let _ = &cfg.vnic; // host-side label, irrelevant in-guest
    let _ = &cfg.mac; // viona programs the MAC on the host side

    let (addr, prefix) = parse_cidr(&cfg.ip)?;
    let netmask = prefix_to_mask(prefix);

    let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if sock < 0 {
        return Err(format!("socket(AF_INET): {}", last_err()));
    }
    let res: Result<(), String> = (|| {
        set_addr(sock, name, SIOCSIFADDR, addr)?;
        set_addr(sock, name, SIOCSIFNETMASK, netmask)?;
        set_flags(sock, name, IFF_UP | IFF_RUNNING)?;
        if set_default_route {
            if let Some(ref gw) = cfg.gateway {
                let gw_addr = parse_ipv4(gw)?;
                add_default_route(sock, gw_addr)?;
            }
        }
        Ok(())
    })();
    unsafe {
        libc::close(sock);
    }
    res
}

fn set_addr(
    sock: libc::c_int,
    name: &str,
    op: libc::c_int,
    addr: u32,
) -> Result<(), String> {
    let mut ifr: Ifreq = unsafe { zeroed() };
    write_name(&mut ifr.name, name.as_bytes());

    // ifr.payload starts with a sockaddr (sa_family u16, then payload).
    // For AF_INET: sin_family u16, sin_port u16, sin_addr u32, 8B pad.
    let sa = build_sockaddr_in(addr);
    ifr.payload[..size_of::<libc::sockaddr_in>()].copy_from_slice(&sa);

    let rc = unsafe { libc::ioctl(sock, op, &ifr as *const _) };
    if rc != 0 {
        return Err(format!("ioctl 0x{op:x} on {name}: {}", last_err()));
    }
    Ok(())
}

fn set_flags(sock: libc::c_int, name: &str, flags: i16) -> Result<(), String> {
    let mut ifr: Ifreq = unsafe { zeroed() };
    write_name(&mut ifr.name, name.as_bytes());
    ifr.payload[..2].copy_from_slice(&flags.to_ne_bytes());

    let rc = unsafe { libc::ioctl(sock, SIOCSIFFLAGS, &ifr as *const _) };
    if rc != 0 {
        return Err(format!("SIOCSIFFLAGS on {name}: {}", last_err()));
    }
    Ok(())
}

fn add_default_route(sock: libc::c_int, gateway: u32) -> Result<(), String> {
    // `struct rtentry` from <net/route.h>, zeroed, with the three
    // sockaddr fields and the flags filled in. Routes are AF_INET only
    // here.
    //
    // Layout (x86_64):
    //   unsigned long rt_pad1;          8
    //   struct sockaddr rt_dst;         16
    //   struct sockaddr rt_gateway;     16
    //   struct sockaddr rt_genmask;     16
    //   unsigned short rt_flags;        2
    //   short rt_pad2;                  2 + 4 pad
    //   unsigned long rt_pad3;          8
    //   void *rt_pad4;                  8
    //   short rt_metric;                2 + 6 pad
    //   char *rt_dev;                   8 (NULL: the kernel picks)
    //   unsigned long rt_mtu;           8
    //   unsigned long rt_window;        8
    //   unsigned short rt_irtt;         2 + 6 pad
    // The total is 120 bytes. The kernel copies only that much out of
    // the larger buffer.
    const RT_FLAGS_OFFSET: usize = 8 + 16 * 3;
    const RTF_UP: u16 = 0x1;
    const RTF_GATEWAY: u16 = 0x2;

    let mut buf = [0u8; 256];
    let buf_ref = &mut buf;

    let dst = build_sockaddr_in(0); // 0.0.0.0
    let gw = build_sockaddr_in(gateway);
    let mask = build_sockaddr_in(0);

    buf_ref[8..8 + 16].copy_from_slice(&dst);
    buf_ref[24..24 + 16].copy_from_slice(&gw);
    buf_ref[40..40 + 16].copy_from_slice(&mask);
    buf_ref[RT_FLAGS_OFFSET..RT_FLAGS_OFFSET + 2]
        .copy_from_slice(&(RTF_UP | RTF_GATEWAY).to_ne_bytes());

    let rc = unsafe { libc::ioctl(sock, SIOCADDRT, buf_ref.as_ptr()) };
    if rc != 0 {
        return Err(format!("SIOCADDRT: {}", last_err()));
    }
    Ok(())
}

fn write_name(name: &mut [u8; IFNAMSIZ], src: &[u8]) {
    let n = src.len().min(IFNAMSIZ - 1);
    name[..n].copy_from_slice(&src[..n]);
    name[n] = 0;
}

/// Encode `addr` as a 16-byte sockaddr_in, address in network byte
/// order. It is hand-built because libc's `sockaddr_in` is a nominal
/// type and the ioctl payload needs a flat byte buffer.
fn build_sockaddr_in(addr: u32) -> [u8; 16] {
    let mut out = [0u8; 16];
    // sin_family = AF_INET (2), little-endian on x86_64.
    out[..2].copy_from_slice(&(libc::AF_INET as u16).to_ne_bytes());
    // sin_port = 0 (bytes 2..4 are already zero).
    // sin_addr.s_addr in network byte order.
    out[4..8].copy_from_slice(&addr.to_be_bytes());
    out
}

fn parse_cidr(s: &str) -> Result<(u32, u8), String> {
    let (ip, prefix) = s
        .split_once('/')
        .ok_or_else(|| format!("not CIDR form: {s}"))?;
    let prefix: u8 =
        prefix.parse().map_err(|_| format!("bad prefix in {s}"))?;
    if prefix > 32 {
        return Err(format!("prefix > 32 in {s}"));
    }
    Ok((parse_ipv4(ip)?, prefix))
}

fn parse_ipv4(s: &str) -> Result<u32, String> {
    let octets: Vec<&str> = s.split('.').collect();
    if octets.len() != 4 {
        return Err(format!("not IPv4: {s}"));
    }
    let mut out: u32 = 0;
    for o in octets {
        let v: u8 = o.parse().map_err(|_| format!("bad octet in {s}"))?;
        // Four octets shift 24 bits in total, so this cannot overflow.
        out = (out << 8) | u32::from(v);
    }
    Ok(out)
}

fn prefix_to_mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}

fn last_err() -> String {
    let e = std::io::Error::last_os_error();
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_parse() {
        assert_eq!(parse_cidr("10.0.0.1/24").unwrap(), (0x0A000001, 24));
        assert_eq!(parse_cidr("0.0.0.0/0").unwrap(), (0, 0));
    }

    #[test]
    fn cidr_bad() {
        assert!(parse_cidr("10.0.0.1").is_err());
        assert!(parse_cidr("10.0.0.1/33").is_err());
        assert!(parse_cidr("not.an.ip/8").is_err());
    }

    #[test]
    fn mask_from_prefix() {
        assert_eq!(prefix_to_mask(24), 0xFFFFFF00);
        assert_eq!(prefix_to_mask(32), 0xFFFFFFFF);
        assert_eq!(prefix_to_mask(0), 0);
    }
}
