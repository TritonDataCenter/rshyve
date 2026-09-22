// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Port I/O bus dispatch.
//!
//! The x86 port I/O address space is 16-bit (0x0000..=0xFFFF). Device
//! handlers register a port range and receive IN/OUT instructions that
//! target ports within that range.
//!
//! # Thread safety
//!
//! Multiple vCPU threads call [`PioBus::handle_in`] and
//! [`PioBus::handle_out`] concurrently. The bus holds a `Mutex` only
//! for the lookup. The lookup clones the handler `Arc` and drops the
//! lock before the call, so a handler that registers or unregisters
//! ports does not deadlock.

use std::sync::{Arc, Mutex};

use crate::aspace::{ASpace, Error as ASpaceError};
use crate::common::{RWOp, ReadOp, WriteOp};

#[usdt::provider(provider = "vmm")]
mod probes {
    fn pio_in(port: u16, bytes: u8, value: u32, handled: u8) {}
    fn pio_out(port: u16, bytes: u8, value: u32, handled: u8) {}
}

/// Handler function type for port I/O.
///
/// The first argument is the offset within the registered port range.
/// The second argument is the I/O operation to fulfill.
pub type PioFn = dyn Fn(u16, RWOp<'_>) + Send + Sync + 'static;

/// Port I/O bus dispatcher.
///
/// Dispatches x86 IN/OUT instructions to registered device handlers
/// based on port address.
pub struct PioBus {
    map: Mutex<ASpace<Arc<PioFn>>>,
}

impl std::fmt::Debug for PioBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PioBus").finish_non_exhaustive()
    }
}

impl PioBus {
    /// Create a new, empty PIO bus covering the full x86 port space.
    pub fn new() -> Self {
        Self {
            map: Mutex::new(ASpace::new(0, u16::MAX as usize)),
        }
    }

    /// Register a handler for a contiguous port range.
    ///
    /// `port` is the base port number and `len` is the number of ports.
    /// Returns an error if the range conflicts with an existing
    /// registration.
    pub fn register(
        &self,
        port: u16,
        len: u16,
        handler: Arc<PioFn>,
    ) -> Result<(), ASpaceError> {
        self.map
            .lock()
            .unwrap()
            .register(port as usize, len as usize, handler)
    }

    /// Unregister the handler whose registration starts at `port`.
    pub fn unregister(&self, port: u16) -> Result<(), ASpaceError> {
        self.map
            .lock()
            .unwrap()
            .unregister(port as usize)
            .map(|_| ())
    }

    /// Handle an IN instruction (guest reads from port).
    ///
    /// Returns the value read. If no handler is registered for the port,
    /// returns `0xFFFF_FFFF` (all 1s), matching x86 floating-bus behavior.
    pub fn handle_in(&self, port: u16, bytes: u8) -> u32 {
        match bytes {
            1 | 2 | 4 => {}
            _ => return 0xFFFF_FFFF,
        }

        let mut ro = ReadOp::new(bytes as usize);

        let handled = if let Some((offset, handler)) = self.lookup(port) {
            handler(offset, RWOp::Read(&mut ro));
            1u8
        } else {
            // Unregistered port: fill with 1s (x86 bus float)
            ro.write_u32(0xFFFF_FFFF);
            0u8
        };

        let buf = ro.buf();
        let value = match bytes {
            1 => buf[0] as u32,
            2 => u16::from_le_bytes([buf[0], buf[1]]) as u32,
            4 => u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
            _ => 0xFFFF_FFFF,
        };

        probes::pio_in!(|| (port, bytes, value, handled));
        value
    }

    /// Handle an OUT instruction (guest writes to port).
    ///
    /// If no handler is registered for the port, the write is silently
    /// ignored, matching x86 bus behavior.
    pub fn handle_out(&self, port: u16, bytes: u8, val: u32) {
        match bytes {
            1 | 2 | 4 => {}
            _ => return,
        }

        let data = &val.to_le_bytes()[..bytes as usize];
        let wo = WriteOp::from_buf(data);

        let handled = if let Some((offset, handler)) = self.lookup(port) {
            handler(offset, RWOp::Write(&wo));
            1u8
        } else {
            0u8
        };

        probes::pio_out!(|| (port, bytes, val, handled));
    }

    /// Look up the handler for a port, clone the Arc, and drop the lock.
    ///
    /// Returns `Some((offset_within_region, handler))` or `None` if no
    /// handler is registered.
    fn lookup(&self, port: u16) -> Option<(u16, Arc<PioFn>)> {
        let map = self.map.lock().unwrap();
        match map.region_at(port as usize) {
            Ok((start, _len, handler)) => {
                let handler = Arc::clone(handler);
                let offset = port - start as u16;
                drop(map);
                Some((offset, handler))
            }
            Err(_) => None,
        }
    }
}

impl Default for PioBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn unregistered_port_reads_all_ones() {
        let bus = PioBus::new();
        assert_eq!(bus.handle_in(0x80, 1), 0xFF);
        assert_eq!(bus.handle_in(0x80, 2), 0xFFFF);
        assert_eq!(bus.handle_in(0x80, 4), 0xFFFF_FFFF);
    }

    #[test]
    fn unregistered_port_write_is_silent() {
        let bus = PioBus::new();
        bus.handle_out(0x80, 1, 0x42);
    }

    #[test]
    fn register_and_read() {
        let bus = PioBus::new();
        let handler: Arc<PioFn> = Arc::new(|offset, rwop| {
            if let RWOp::Read(ro) = rwop {
                ro.write_u8(0x42 + offset as u8);
            }
        });
        bus.register(0x3F8, 8, handler).unwrap();

        // Read from base port
        assert_eq!(bus.handle_in(0x3F8, 1), 0x42);
        // Read from offset 2 within the region
        assert_eq!(bus.handle_in(0x3FA, 1), 0x44);
    }

    #[test]
    fn register_and_write() {
        let bus = PioBus::new();
        let captured = Arc::new(AtomicU32::new(0));
        let captured_clone = captured.clone();

        let handler: Arc<PioFn> = Arc::new(move |_offset, rwop| {
            if let RWOp::Write(wo) = rwop {
                captured_clone.store(wo.read_u32(), Ordering::SeqCst);
            }
        });
        bus.register(0x3F8, 8, handler).unwrap();

        bus.handle_out(0x3F8, 4, 0xDEAD_BEEF);
        assert_eq!(captured.load(Ordering::SeqCst), 0xDEAD_BEEF);
    }

    #[test]
    fn handle_in_rejects_oversized_width() {
        let bus = PioBus::new();
        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = Arc::clone(&calls);
        let handler: Arc<PioFn> = Arc::new(move |_, _| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
        });
        bus.register(0x100, 1, handler).unwrap();

        assert_eq!(bus.handle_in(0x100, 5), 0xFFFF_FFFF);
        assert_eq!(bus.handle_in(0x100, 255), 0xFFFF_FFFF);

        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn handle_in_rejects_zero_width() {
        let bus = PioBus::new();
        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = Arc::clone(&calls);
        let handler: Arc<PioFn> = Arc::new(move |_, _| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
        });
        bus.register(0x100, 1, handler).unwrap();

        assert_eq!(bus.handle_in(0x100, 0), 0xFFFF_FFFF);

        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn handle_out_rejects_oversized_width() {
        let bus = PioBus::new();
        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = Arc::clone(&calls);
        let handler: Arc<PioFn> = Arc::new(move |_, _| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
        });
        bus.register(0x100, 1, handler).unwrap();

        bus.handle_out(0x100, 5, 0);
        bus.handle_out(0x100, 255, 0);

        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn handle_out_rejects_zero_width() {
        let bus = PioBus::new();
        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = Arc::clone(&calls);
        let handler: Arc<PioFn> = Arc::new(move |_, _| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
        });
        bus.register(0x100, 1, handler).unwrap();

        bus.handle_out(0x100, 0, 0);

        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn register_conflict() {
        let bus = PioBus::new();
        let h: Arc<PioFn> = Arc::new(|_, _| {});
        bus.register(0x100, 0x10, h.clone()).unwrap();
        assert!(bus.register(0x108, 0x10, h).is_err());
    }

    #[test]
    fn pci_cfg_and_reset_ports_coexist() {
        let bus = PioBus::new();
        let h: Arc<PioFn> = Arc::new(|_, _| {});

        assert!(bus.register(0xCF8, 1, h.clone()).is_ok());
        assert!(bus.register(0xCF9, 1, h.clone()).is_ok());
        assert!(bus.register(0xCFC, 4, h).is_ok());
    }

    #[test]
    fn four_byte_access_at_cf8_reaches_one_byte_registration() {
        let bus = PioBus::new();
        let seen_offset = Arc::new(AtomicU32::new(u32::MAX));
        let seen_len = Arc::new(AtomicU32::new(0));
        let offset_clone = seen_offset.clone();
        let len_clone = seen_len.clone();
        let handler: Arc<PioFn> = Arc::new(move |offset, rwo| {
            offset_clone.store(offset as u32, Ordering::SeqCst);
            len_clone.store(rwo.len() as u32, Ordering::SeqCst);
        });
        bus.register(0xCF8, 1, handler).unwrap();

        bus.handle_out(0xCF8, 4, 0x8000_0000);

        assert_eq!(seen_offset.load(Ordering::SeqCst), 0);
        assert_eq!(seen_len.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn unregister() {
        let bus = PioBus::new();
        let h: Arc<PioFn> = Arc::new(|_, rwop| {
            if let RWOp::Read(ro) = rwop {
                ro.write_u8(0x42);
            }
        });
        bus.register(0x100, 1, h).unwrap();
        assert_eq!(bus.handle_in(0x100, 1), 0x42);

        bus.unregister(0x100).unwrap();
        assert_eq!(bus.handle_in(0x100, 1), 0xFF);
    }

    #[test]
    fn unregister_not_found() {
        let bus = PioBus::new();
        assert!(bus.unregister(0x100).is_err());
    }

    #[test]
    fn handler_receives_correct_offset() {
        let bus = PioBus::new();
        let seen_offset = Arc::new(AtomicU32::new(u32::MAX));
        let seen_clone = seen_offset.clone();

        let handler: Arc<PioFn> = Arc::new(move |offset, _| {
            seen_clone.store(offset as u32, Ordering::SeqCst);
        });
        bus.register(0x200, 0x10, handler).unwrap();

        bus.handle_out(0x205, 1, 0);
        assert_eq!(seen_offset.load(Ordering::SeqCst), 5);

        bus.handle_out(0x20F, 1, 0);
        assert_eq!(seen_offset.load(Ordering::SeqCst), 0xF);
    }

    #[test]
    fn multiple_registrations() {
        let bus = PioBus::new();

        let h1: Arc<PioFn> = Arc::new(|_, rwop| {
            if let RWOp::Read(ro) = rwop {
                ro.write_u8(0xAA);
            }
        });
        let h2: Arc<PioFn> = Arc::new(|_, rwop| {
            if let RWOp::Read(ro) = rwop {
                ro.write_u8(0xBB);
            }
        });

        bus.register(0x100, 0x10, h1).unwrap();
        bus.register(0x200, 0x10, h2).unwrap();

        assert_eq!(bus.handle_in(0x100, 1), 0xAA);
        assert_eq!(bus.handle_in(0x200, 1), 0xBB);
        // Gap between registrations
        assert_eq!(bus.handle_in(0x150, 1), 0xFF);
    }
}
