// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Memory-mapped I/O bus dispatch.
//!
//! MMIO handlers register a physical address range and receive read/write
//! operations when the guest accesses addresses within that range.
//!
//! # Thread safety
//!
//! Same approach as [`PioBus`](crate::pio::PioBus): the handler `Arc` is
//! cloned and the lock is dropped before invoking the handler.

use std::sync::{Arc, Mutex};

use crate::aspace::{ASpace, Error as ASpaceError};
use crate::common::{RWOp, ReadOp, WriteOp};

/// Maximum physical address space (16 TiB).
const MAX_PHYSMEM: usize = 0x1000_0000_0000;

/// Handler function type for memory-mapped I/O.
///
/// The first argument is the offset within the registered MMIO region.
/// The second argument is the I/O operation to fulfill.
pub type MmioFn = dyn Fn(usize, RWOp<'_>) + Send + Sync + 'static;

/// Memory-mapped I/O bus dispatcher.
///
/// Dispatches guest physical memory accesses to registered device
/// handlers based on address.
pub struct MmioBus {
    map: Mutex<ASpace<Arc<MmioFn>>>,
}

impl std::fmt::Debug for MmioBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MmioBus").finish_non_exhaustive()
    }
}

impl MmioBus {
    /// Create a new, empty MMIO bus covering the full physical address
    /// space (0 to 16 TiB).
    pub fn new() -> Self {
        Self {
            map: Mutex::new(ASpace::new(0, MAX_PHYSMEM - 1)),
        }
    }

    /// Register a handler for a contiguous MMIO address range.
    ///
    /// `addr` is the base physical address and `len` is the region
    /// length in bytes.
    pub fn register(
        &self,
        addr: u64,
        len: u64,
        handler: Arc<MmioFn>,
    ) -> Result<(), ASpaceError> {
        self.map
            .lock()
            .unwrap()
            .register(addr as usize, len as usize, handler)
    }

    /// Unregister the handler whose registration starts at `addr`.
    pub fn unregister(&self, addr: u64) -> Result<(), ASpaceError> {
        self.map
            .lock()
            .unwrap()
            .unregister(addr as usize)
            .map(|_| ())
    }

    /// Handle an MMIO read (guest reads from a physical address).
    ///
    /// Returns the value read. If no handler is registered, returns
    /// all 1s, matching bus-float behavior.
    pub fn handle_read(&self, addr: u64, bytes: u8) -> u64 {
        match bytes {
            1 | 2 | 4 | 8 => {}
            _ => return 0xFFFF_FFFF_FFFF_FFFF,
        }

        let mut ro = ReadOp::new(bytes as usize);

        if let Some((offset, handler)) = self.lookup(addr) {
            handler(offset, RWOp::Read(&mut ro));
        } else {
            // Unregistered address: fill with 1s
            ro.write_u64(0xFFFF_FFFF_FFFF_FFFF);
        }

        let buf = ro.buf();
        match bytes {
            1 => buf[0] as u64,
            2 => u16::from_le_bytes([buf[0], buf[1]]) as u64,
            4 => u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as u64,
            8 => u64::from_le_bytes([
                buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
            ]),
            _ => 0xFFFF_FFFF_FFFF_FFFF,
        }
    }

    /// Handle an MMIO write (guest writes to a physical address).
    ///
    /// If no handler is registered, the write is silently ignored.
    pub fn handle_write(&self, addr: u64, bytes: u8, val: u64) {
        match bytes {
            1 | 2 | 4 | 8 => {}
            _ => return,
        }

        let data = &val.to_le_bytes()[..bytes as usize];
        let wo = WriteOp::from_buf(data);

        if let Some((offset, handler)) = self.lookup(addr) {
            handler(offset, RWOp::Write(&wo));
        }
    }

    /// Look up the handler for an address, clone the Arc, and drop the lock.
    fn lookup(&self, addr: u64) -> Option<(usize, Arc<MmioFn>)> {
        let map = self.map.lock().unwrap();
        match map.region_at(addr as usize) {
            Ok((start, _len, handler)) => {
                let handler = Arc::clone(handler);
                let offset = addr as usize - start;
                drop(map);
                Some((offset, handler))
            }
            Err(_) => None,
        }
    }
}

impl Default for MmioBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn unregistered_addr_reads_all_ones() {
        let bus = MmioBus::new();
        assert_eq!(bus.handle_read(0x1000, 1), 0xFF);
        assert_eq!(bus.handle_read(0x1000, 2), 0xFFFF);
        assert_eq!(bus.handle_read(0x1000, 4), 0xFFFF_FFFF);
        assert_eq!(bus.handle_read(0x1000, 8), 0xFFFF_FFFF_FFFF_FFFF);
    }

    #[test]
    fn unregistered_addr_write_is_silent() {
        let bus = MmioBus::new();
        bus.handle_write(0x1000, 4, 0xDEADBEEF);
    }

    #[test]
    fn register_and_read() {
        let bus = MmioBus::new();
        let handler: Arc<MmioFn> = Arc::new(|offset, rwop| {
            if let RWOp::Read(ro) = rwop {
                ro.write_u32(0x1000 + offset as u32);
            }
        });
        bus.register(0xFEC0_0000, 0x1000, handler).unwrap();

        assert_eq!(bus.handle_read(0xFEC0_0000, 4), 0x1000);
        assert_eq!(bus.handle_read(0xFEC0_0010, 4), 0x1010);
    }

    #[test]
    fn register_and_write() {
        let bus = MmioBus::new();
        let captured = Arc::new(AtomicU64::new(0));
        let captured_clone = captured.clone();

        let handler: Arc<MmioFn> = Arc::new(move |_offset, rwop| {
            if let RWOp::Write(wo) = rwop {
                captured_clone.store(wo.read_u64(), Ordering::SeqCst);
            }
        });
        bus.register(0xFEE0_0000, 0x1000, handler).unwrap();

        bus.handle_write(0xFEE0_0000, 8, 0x0102_0304_0506_0708);
        assert_eq!(captured.load(Ordering::SeqCst), 0x0102_0304_0506_0708);
    }

    #[test]
    fn handle_write_rejects_oversized_width() {
        let bus = MmioBus::new();
        let calls = Arc::new(AtomicU64::new(0));
        let calls_clone = Arc::clone(&calls);
        let handler: Arc<MmioFn> = Arc::new(move |_, _| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
        });
        bus.register(0x1000, 0x100, handler).unwrap();

        bus.handle_write(0x1000, 9, 0);
        bus.handle_write(0x1000, 255, 0);

        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn handle_read_rejects_oversized_width() {
        let bus = MmioBus::new();
        let calls = Arc::new(AtomicU64::new(0));
        let calls_clone = Arc::clone(&calls);
        let handler: Arc<MmioFn> = Arc::new(move |_, _| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
        });
        bus.register(0x1000, 0x100, handler).unwrap();

        assert_eq!(bus.handle_read(0x1000, 9), 0xFFFF_FFFF_FFFF_FFFF);
        assert_eq!(bus.handle_read(0x1000, 255), 0xFFFF_FFFF_FFFF_FFFF);

        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn handle_write_rejects_zero_width() {
        let bus = MmioBus::new();
        let calls = Arc::new(AtomicU64::new(0));
        let calls_clone = Arc::clone(&calls);
        let handler: Arc<MmioFn> = Arc::new(move |_, _| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
        });
        bus.register(0x1000, 0x100, handler).unwrap();

        bus.handle_write(0x1000, 0, 0);

        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn handle_read_rejects_zero_width() {
        let bus = MmioBus::new();
        let calls = Arc::new(AtomicU64::new(0));
        let calls_clone = Arc::clone(&calls);
        let handler: Arc<MmioFn> = Arc::new(move |_, _| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
        });
        bus.register(0x1000, 0x100, handler).unwrap();

        assert_eq!(bus.handle_read(0x1000, 0), 0xFFFF_FFFF_FFFF_FFFF);

        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn handle_write_accepts_legal_widths() {
        let bus = MmioBus::new();
        let widths = Arc::new(Mutex::new(Vec::new()));
        let widths_clone = Arc::clone(&widths);
        let handler: Arc<MmioFn> = Arc::new(move |_, rwop| {
            if let RWOp::Write(wo) = rwop {
                widths_clone.lock().unwrap().push(wo.len());
            }
        });
        bus.register(0x1000, 0x100, handler).unwrap();

        for bytes in [1, 2, 4, 8] {
            bus.handle_write(0x1000, bytes, u64::MAX);
        }

        assert_eq!(*widths.lock().unwrap(), [1, 2, 4, 8]);
    }

    #[test]
    fn register_conflict() {
        let bus = MmioBus::new();
        let h: Arc<MmioFn> = Arc::new(|_, _| {});
        bus.register(0x1000, 0x1000, h.clone()).unwrap();
        assert!(bus.register(0x1800, 0x1000, h).is_err());
    }

    #[test]
    fn unregister() {
        let bus = MmioBus::new();
        let h: Arc<MmioFn> = Arc::new(|_, rwop| {
            if let RWOp::Read(ro) = rwop {
                ro.write_u8(0x42);
            }
        });
        bus.register(0x1000, 0x10, h).unwrap();
        assert_eq!(bus.handle_read(0x1000, 1), 0x42);

        bus.unregister(0x1000).unwrap();
        assert_eq!(bus.handle_read(0x1000, 1), 0xFF);
    }

    #[test]
    fn handler_receives_correct_offset() {
        let bus = MmioBus::new();
        let seen_offset = Arc::new(AtomicU64::new(u64::MAX));
        let seen_clone = seen_offset.clone();

        let handler: Arc<MmioFn> = Arc::new(move |offset, _| {
            seen_clone.store(offset as u64, Ordering::SeqCst);
        });
        bus.register(0xFEC0_0000, 0x1000, handler).unwrap();

        bus.handle_write(0xFEC0_0010, 1, 0);
        assert_eq!(seen_offset.load(Ordering::SeqCst), 0x10);

        bus.handle_write(0xFEC0_0FFF, 1, 0);
        assert_eq!(seen_offset.load(Ordering::SeqCst), 0xFFF);
    }

    #[test]
    fn byte_width_reads() {
        let bus = MmioBus::new();
        let handler: Arc<MmioFn> = Arc::new(|_, rwop| {
            if let RWOp::Read(ro) = rwop {
                match ro.len() {
                    1 => ro.write_u8(0xAB),
                    2 => ro.write_u16(0xABCD),
                    4 => ro.write_u32(0xABCD_EF01),
                    8 => ro.write_u64(0xABCD_EF01_2345_6789),
                    _ => {}
                }
            }
        });
        bus.register(0x1000, 0x100, handler).unwrap();

        assert_eq!(bus.handle_read(0x1000, 1), 0xAB);
        assert_eq!(bus.handle_read(0x1000, 2), 0xABCD);
        assert_eq!(bus.handle_read(0x1000, 4), 0xABCD_EF01);
        assert_eq!(bus.handle_read(0x1000, 8), 0xABCD_EF01_2345_6789);
    }
}
