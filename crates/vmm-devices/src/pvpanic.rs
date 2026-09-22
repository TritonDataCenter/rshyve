// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! QEMU-compatible pvpanic device.
//!
//! Linux guests write I/O port 0x505 on a kernel panic, and the VMM
//! logs the event. Guest support: `CONFIG_PVPANIC=y` (Linux 4.9+). The
//! guest finds the device through ACPI (HID "QEMU0001") or auto-probe.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use slog::{crit, Logger};
use vmm_core::ratelimit::TokenBucket;

use crate::Lifecycle;

pub const PVPANIC_IOPORT: u16 = 0x505;

const PVPANIC_PANICKED: u8 = 1 << 0;
/// The guest has a crash kernel loaded (kdump/kexec).
const PVPANIC_CRASH_LOADED: u8 = 1 << 1;

/// Supported events, returned on read.
const PVPANIC_SUPPORTED: u8 = PVPANIC_PANICKED | PVPANIC_CRASH_LOADED;

/// Records a guest can provoke per minute once the latches are cleared.
const LOG_BURST: u32 = 4;
const LOG_PERIOD: Duration = Duration::from_secs(15);

pub struct PvPanic {
    log: Logger,
    /// Each event logs only on its false-to-true edge. Otherwise a
    /// guest that writes the port in a loop drives the host log, one
    /// record and one allocation per PIO exit.
    panicked: AtomicBool,
    crash_loaded: AtomicBool,
    budget: TokenBucket,
}

impl PvPanic {
    pub fn new(log: Logger) -> Arc<Self> {
        Arc::new(Self {
            log,
            panicked: AtomicBool::new(false),
            crash_loaded: AtomicBool::new(false),
            budget: TokenBucket::new(LOG_BURST, LOG_PERIOD),
        })
    }

    /// Returns the supported events bitmask.
    pub fn pio_read(&self) -> u8 {
        PVPANIC_SUPPORTED
    }

    /// The guest writes event bits to signal a panic.
    pub fn pio_write(&self, val: u8) {
        if self.latch(&self.panicked, val & PVPANIC_PANICKED != 0) {
            crit!(self.log, "guest panic reported through pvpanic";
                "event" => val,
            );
        }
        if self.latch(&self.crash_loaded, val & PVPANIC_CRASH_LOADED != 0) {
            slog::info!(self.log, "guest has a crash kernel loaded (kdump)");
        }
    }

    /// Whether this write is the edge that earns a record.
    fn latch(&self, flag: &AtomicBool, set: bool) -> bool {
        set && !flag.swap(true, Ordering::Relaxed) && self.budget.take()
    }
}

impl Lifecycle for PvPanic {
    fn type_name(&self) -> &'static str {
        "pvpanic"
    }

    /// A rebooted guest gets to report its own panic.
    fn reset(&self) {
        self.panicked.store(false, Ordering::Relaxed);
        self.crash_loaded.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn test_logger() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    /// Counts the records the device emits, so a test can show that a
    /// port a guest drives in a loop does not log in that loop.
    struct CountingDrain(Arc<AtomicUsize>);

    impl slog::Drain for CountingDrain {
        type Ok = ();
        type Err = slog::Never;

        fn log(
            &self,
            record: &slog::Record<'_>,
            _values: &slog::OwnedKVList,
        ) -> Result<(), Self::Err> {
            if record.level().is_at_least(slog::Level::Info) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }
    }

    fn counting_device() -> (Arc<PvPanic>, Arc<AtomicUsize>) {
        use slog::Drain;
        let count = Arc::new(AtomicUsize::new(0));
        let log = Logger::root(CountingDrain(count.clone()).fuse(), slog::o!());
        (PvPanic::new(log), count)
    }

    #[test]
    fn pio_read_returns_supported_mask() {
        let dev = PvPanic::new(test_logger());
        assert_eq!(dev.pio_read(), PVPANIC_SUPPORTED);
        assert_eq!(dev.pio_read(), dev.pio_read());
    }

    #[test]
    fn a_guest_loop_on_the_port_cannot_flood_the_log() {
        let (dev, count) = counting_device();
        for _ in 0..10_000 {
            dev.pio_write(PVPANIC_PANICKED | PVPANIC_CRASH_LOADED);
        }
        // One record per event, on its first write, and no more.
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn each_event_is_reported_once_on_its_own_edge() {
        let (dev, count) = counting_device();
        dev.pio_write(PVPANIC_CRASH_LOADED);
        assert_eq!(count.load(Ordering::Relaxed), 1);
        // The panic that follows the crash kernel is a new event.
        dev.pio_write(PVPANIC_PANICKED);
        assert_eq!(count.load(Ordering::Relaxed), 2);
        dev.pio_write(PVPANIC_PANICKED);
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_reset_lets_the_next_boot_report_its_panic() {
        let (dev, count) = counting_device();
        dev.pio_write(PVPANIC_PANICKED);
        dev.reset();
        dev.pio_write(PVPANIC_PANICKED);
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_reset_loop_spends_a_bounded_budget() {
        // Reset clears the latches, so the token bucket is what stops a
        // guest that reboots itself in a loop.
        let (dev, count) = counting_device();
        for _ in 0..1000 {
            dev.pio_write(PVPANIC_PANICKED);
            dev.reset();
        }
        assert!(count.load(Ordering::Relaxed) <= LOG_BURST as usize);
    }

    #[test]
    fn writes_with_no_event_bits_report_nothing() {
        let (dev, count) = counting_device();
        dev.pio_write(0);
        dev.pio_write(0xFC);
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }
}
