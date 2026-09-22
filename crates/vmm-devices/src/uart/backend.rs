// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Host-side backends for an emulated UART.
//!
//! An emulated serial port needs a destination for the bytes the guest
//! transmits and a source for the bytes it receives. Two backends are
//! shared by every binary here:
//!
//! - [`SerialBackend::Stdio`], which a hypervisor started from a
//!   terminal uses, and
//! - [`SerialBackend::Device`], which is what the bhyve zone brand
//!   names when it emits `-l com1,/dev/zconsole`. That path is the
//!   `zcons` slave, and it is the only channel `zlogin -C` reads. A
//!   process's inherited stdout goes to `/dev/zfd/1`, which zoneadmd
//!   copies into the zone log instead, so a binary that serves COM1 on
//!   stdio leaves the operator with a dead console.
//!
//! Both live here so the zone console has one implementation.
//!
//! Every stdout-bound port shares one writer thread, because the
//! transmit sink runs on a vCPU thread with the UART lock held and may
//! not wait for a consumer. See [`flush_stdout`].

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use slog::{info, warn, Logger};
use vmm_core::poll::{wait_readable, Readiness};

use super::lpc::LpcUart;

/// How many transmit errors are logged before the backend goes quiet.
/// A console whose reader has gone away fails on every byte, and an
/// unbounded log would bury everything else in the zone log.
const TX_ERROR_LOG_LIMIT: u32 = 3;

/// Bytes the stdout pump holds for a consumer that has fallen behind.
/// Past this the sink drops bytes rather than park the vCPU that wrote
/// them: the guest keeps running with a gap in its console, which the
/// log records, instead of stalling behind a pipe nobody reads.
const STDOUT_QUEUE_CAP: usize = 64 * 1024;

/// Receive poll timeout. Bounded so the thread notices a closed
/// backend instead of parking for ever.
const POLL_TIMEOUT: Duration = Duration::from_secs(1);

/// Where one emulated serial port's bytes come from and go to.
pub enum SerialBackend<'a> {
    /// This process's own stdio: transmit to stdout, receive from
    /// stdin.
    Stdio,
    /// This process's stdout, with no receive path. Used for a one-way
    /// marker channel, and for the second port when the first one
    /// already owns stdin.
    StdoutOnly,
    /// A character device or pty, opened read/write. `/dev/zconsole`
    /// is the case that matters.
    Device(&'a str),
}

/// Connect `uart` to `backend`.
///
/// `name` labels the port in log records and in the receive thread's
/// name.
pub fn attach(
    uart: &Arc<LpcUart>,
    backend: SerialBackend<'_>,
    name: &str,
    log: &Logger,
) -> anyhow::Result<()> {
    match backend {
        SerialBackend::Stdio => {
            attach_stdout(uart, name, log)?;
            spawn_stdin_reader(uart, name)?;
            info!(log, "serial port connected"; "port" => name,
                "backend" => "stdio");
            Ok(())
        }
        SerialBackend::StdoutOnly => {
            attach_stdout(uart, name, log)?;
            info!(log, "serial port connected"; "port" => name,
                "backend" => "stdout");
            Ok(())
        }
        SerialBackend::Device(path) => attach_device(uart, path, name, log),
    }
}

/// Send this port's transmit bytes to stdout through the process pump.
fn attach_stdout(
    uart: &Arc<LpcUart>,
    name: &str,
    log: &Logger,
) -> anyhow::Result<()> {
    let pump = stdout_pump(log)
        .map_err(|e| anyhow::anyhow!("cannot start the stdout pump: {e}"))?;
    attach_pump(uart, name, &pump);
    Ok(())
}

/// Install a sink that queues each byte on `pump`.
///
/// The sink runs on the vCPU thread with the UART lock held, so it
/// must not wait: it queues and returns.
fn attach_pump(uart: &Arc<LpcUart>, name: &str, pump: &Arc<OutputPump>) {
    let pump = Arc::clone(pump);
    let port = name.to_string();
    uart.set_tx_sink(Box::new(move |b: u8| pump.push(b, &port)));
}

/// Wait until every byte queued for stdout has been written, or
/// `timeout` passes. Returns whether the queue drained.
///
/// A VMM that exits before the pump thread has written the guest's
/// last bytes loses them, and those bytes carry the fhrun status
/// marker. Call this before the process exits.
pub fn flush_stdout(timeout: Duration) -> bool {
    let pump = STDOUT_PUMP
        .lock()
        .ok()
        .and_then(|slot| slot.as_ref().map(Arc::clone));
    match pump {
        Some(pump) => pump.flush(timeout),
        None => true,
    }
}

/// The one pump every stdout port shares, so their bytes stay in the
/// order the guest wrote them.
static STDOUT_PUMP: Mutex<Option<Arc<OutputPump>>> = Mutex::new(None);

fn stdout_pump(log: &Logger) -> std::io::Result<Arc<OutputPump>> {
    let mut slot = STDOUT_PUMP
        .lock()
        .map_err(|_| std::io::Error::other("stdout pump lock poisoned"))?;
    if let Some(pump) = slot.as_ref() {
        return Ok(Arc::clone(pump));
    }
    let pump = OutputPump::start("stdout", std::io::stdout(), log.clone())?;
    *slot = Some(Arc::clone(&pump));
    Ok(pump)
}

struct OutputQueue {
    buf: VecDeque<u8>,
    /// Bytes the writer thread has taken but not yet written.
    in_flight: usize,
    /// Set once a write failed. The consumer is gone, so every later
    /// byte is discarded rather than queued.
    closed: bool,
    dropped: u64,
}

/// A bounded queue drained to a writer by its own thread.
struct OutputPump {
    queue: Mutex<OutputQueue>,
    changed: Condvar,
    log: Logger,
}

impl OutputPump {
    fn start<W: Write + Send + 'static>(
        name: &str,
        mut writer: W,
        log: Logger,
    ) -> std::io::Result<Arc<Self>> {
        let pump = Arc::new(Self {
            queue: Mutex::new(OutputQueue {
                buf: VecDeque::new(),
                in_flight: 0,
                closed: false,
                dropped: 0,
            }),
            changed: Condvar::new(),
            log,
        });
        let worker = Arc::clone(&pump);
        std::thread::Builder::new()
            .name(format!("{name}-output"))
            .spawn(move || worker.run(&mut writer))?;
        Ok(pump)
    }

    /// Drain the queue until a write fails. Returning ends the thread,
    /// because `push` queues nothing more once the queue is closed and
    /// the condvar would never be signalled again.
    fn run<W: Write>(&self, writer: &mut W) {
        let mut chunk: Vec<u8> = Vec::new();
        loop {
            {
                let mut queue = self.lock();
                while queue.buf.is_empty() {
                    queue = self
                        .changed
                        .wait(queue)
                        .unwrap_or_else(|e| e.into_inner());
                }
                chunk.extend(queue.buf.drain(..));
                queue.in_flight = chunk.len();
            }
            let result = writer.write_all(&chunk).and_then(|()| writer.flush());
            chunk.clear();
            let mut queue = self.lock();
            queue.in_flight = 0;
            if let Err(e) = result {
                warn!(self.log, "serial output consumer gone"; "error" => %e);
                queue.closed = true;
                queue.buf.clear();
                self.changed.notify_all();
                return;
            }
            self.changed.notify_all();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, OutputQueue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queue one byte. Never waits: a full queue or a closed consumer
    /// drops the byte, and the first drop is logged.
    fn push(&self, b: u8, port: &str) {
        let mut queue = self.lock();
        if queue.closed {
            return;
        }
        if queue.buf.len() >= STDOUT_QUEUE_CAP {
            queue.dropped += 1;
            if queue.dropped == 1 {
                warn!(self.log, "serial output dropped";
                    "port" => port,
                    "reason" => "the stdout consumer is not keeping up",
                    "note" => "later drops are counted, not logged");
            }
            return;
        }
        let was_idle = queue.buf.is_empty();
        queue.buf.push_back(b);
        if was_idle {
            self.changed.notify_one();
        }
    }

    fn flush(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut queue = self.lock();
        loop {
            if queue.closed || (queue.buf.is_empty() && queue.in_flight == 0) {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            queue = self
                .changed
                .wait_timeout(queue, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    #[cfg(test)]
    fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    #[cfg(test)]
    fn is_closed(&self) -> bool {
        self.lock().closed
    }
}

/// Feed stdin into this port's receive FIFO.
///
/// Only one port may do this: a second reader would take bytes away
/// from the first.
fn spawn_stdin_reader(uart: &Arc<LpcUart>, name: &str) -> anyhow::Result<()> {
    let rx = Arc::clone(uart);
    std::thread::Builder::new()
        .name(format!("{name}-input"))
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut buf = [0u8; 1];
            loop {
                match stdin.lock().read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        rx.input_byte(buf[0]);
                    }
                }
            }
        })
        .map_err(|e| {
            anyhow::anyhow!("cannot spawn {name} input thread: {e}")
        })?;
    Ok(())
}

/// Open `path` read/write and serve both directions of this port on it.
fn attach_device(
    uart: &Arc<LpcUart>,
    path: &str,
    name: &str,
    log: &Logger,
) -> anyhow::Result<()> {
    // O_NONBLOCK on the open itself, so a device with no carrier does
    // not park the whole process here. It stays on the description, so
    // a transmit never stalls a vCPU because nobody drains the console.
    let device = File::options()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| {
            anyhow::anyhow!("cannot open {name} device {path}: {e}")
        })?;

    make_raw(&device);

    // Separate descriptors for the two directions. `try_clone` makes
    // them close-on-exec, so a guest-reset re-exec does not inherit
    // them.
    let write_half = device
        .try_clone()
        .map_err(|e| anyhow::anyhow!("cannot dup {name} device: {e}"))?;
    let read_half = device
        .try_clone()
        .map_err(|e| anyhow::anyhow!("cannot dup {name} device: {e}"))?;
    drop(device);

    let tx_errors = AtomicU32::new(0);
    let tx_log = log.clone();
    let tx_name = name.to_string();
    uart.set_tx_sink(Box::new(move |b: u8| {
        if let Err(e) = (&write_half).write(&[b]) {
            if tx_errors.fetch_add(1, Ordering::Relaxed) < TX_ERROR_LOG_LIMIT {
                warn!(tx_log, "serial transmit failed"; "port" => &tx_name,
                    "error" => %e);
            }
        }
    }));

    spawn_device_reader(uart, read_half, name)?;
    info!(log, "serial port connected"; "port" => name, "backend" => path);
    Ok(())
}

/// Stop the line discipline translating anything: the guest owns it.
///
/// A backend that is not a terminal has no attributes to set, so a
/// failed `tcgetattr` is not an error.
fn make_raw(device: &File) {
    let fd = device.as_raw_fd();
    // SAFETY: `device` keeps `fd` open for the whole block. `termios`
    // is a repr(C) struct of integers and a byte array, so all zeroes is
    // a valid instance, and `tcgetattr` fills it before `cfmakeraw` and
    // `tcsetattr` read it. Each call touches only `tio`.
    unsafe {
        let mut tio: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut tio) == 0 {
            libc::cfmakeraw(&mut tio);
            // CLOCAL: no modem, so never wait on carrier.
            tio.c_cflag |= libc::CLOCAL;
            libc::tcsetattr(fd, libc::TCSANOW, &tio);
        }
    }
}

/// Read from `device` into the port's receive FIFO until it closes.
fn spawn_device_reader(
    uart: &Arc<LpcUart>,
    device: File,
    name: &str,
) -> anyhow::Result<()> {
    let rx = Arc::clone(uart);
    std::thread::Builder::new()
        .name(format!("{name}-input"))
        .spawn(move || {
            let mut buf = [0u8; 64];
            loop {
                match wait_readable(device.as_fd(), POLL_TIMEOUT) {
                    Readiness::Readable => {}
                    Readiness::Idle => continue,
                    Readiness::Gone => break,
                }
                match (&device).read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        for byte in &buf[..n] {
                            rx.input_byte(*byte);
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => break,
                }
            }
        })
        .map_err(|e| {
            anyhow::anyhow!("cannot spawn {name} input thread: {e}")
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    use vmm_core::intr_pins::IntrPin;
    use vmm_core::pio::PioBus;

    use super::super::lpc::{COM1_BASE, REGISTER_LEN};

    /// How long a test waits for the receive thread to move a byte.
    const RX_DEADLINE: Duration = Duration::from_secs(5);

    struct TestPin(AtomicBool);

    impl IntrPin for TestPin {
        fn assert(&self) {
            self.0.store(true, Ordering::SeqCst);
        }
        fn deassert(&self) {
            self.0.store(false, Ordering::SeqCst);
        }
        fn is_asserted(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn quiet_logger() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    /// Serializes the `ptsname` static buffer between test threads.
    static PTSNAME: Mutex<()> = Mutex::new(());

    /// A pty pair, which is the shape `/dev/zconsole` has: zoneadmd
    /// holds the master, and the backend opens the slave by path.
    struct Pty {
        master: File,
        slave_path: String,
    }

    impl Pty {
        fn open() -> Self {
            let master = File::options()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOCTTY)
                .open("/dev/ptmx")
                .expect("open /dev/ptmx");
            let fd = master.as_raw_fd();
            // `ptsname` answers from libc's own static buffer, and the
            // test harness runs tests on parallel threads, so the name
            // is read under a lock and copied before it is released.
            let _named = PTSNAME
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // SAFETY: `master` keeps `fd` open for the whole block.
            // `ptsname` returns a pointer into that static buffer, which
            // is checked non-null before it is read and copied while the
            // lock is still held, so nothing can free or replace it.
            let slave_path = unsafe {
                assert_eq!(libc::grantpt(fd), 0, "grantpt");
                assert_eq!(libc::unlockpt(fd), 0, "unlockpt");
                let name = libc::ptsname(fd);
                assert!(!name.is_null(), "ptsname");
                std::ffi::CStr::from_ptr(name)
                    .to_str()
                    .expect("slave path is utf8")
                    .to_string()
            };
            Self { master, slave_path }
        }

        fn write(&self, bytes: &[u8]) {
            (&self.master)
                .write_all(bytes)
                .expect("write to the master");
        }

        /// Read until `want` bytes arrive or the deadline passes.
        fn read_exact(&self, want: usize) -> Vec<u8> {
            let deadline = Instant::now() + RX_DEADLINE;
            let mut out = Vec::new();
            let mut buf = [0u8; 64];
            while out.len() < want && Instant::now() < deadline {
                if wait_readable(
                    self.master.as_fd(),
                    Duration::from_millis(200),
                ) != Readiness::Readable
                {
                    continue;
                }
                let Ok(n) = (&self.master).read(&mut buf) else {
                    continue;
                };
                out.extend_from_slice(&buf[..n]);
            }
            out
        }
    }

    fn uart_on_bus() -> (Arc<LpcUart>, PioBus) {
        let bus = PioBus::new();
        let uart = LpcUart::new(Arc::new(TestPin(AtomicBool::new(false))));
        uart.attach(&bus, COM1_BASE);
        (uart, bus)
    }

    /// Drain the guest-visible receive register until `want` bytes are
    /// read or the deadline passes.
    fn guest_read(bus: &PioBus, want: usize) -> Vec<u8> {
        const LSR_DATA_READY: u32 = 0x01;
        let deadline = Instant::now() + RX_DEADLINE;
        let mut out = Vec::new();
        while out.len() < want && Instant::now() < deadline {
            let lsr = bus.handle_in(COM1_BASE + 5, 1);
            if lsr & LSR_DATA_READY == 0 {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            out.push(bus.handle_in(COM1_BASE, 1) as u8);
        }
        out
    }

    /// Guest output must leave through the device the brand named, not
    /// through this process's stdout.
    ///
    /// Mutation this kills: transmitting to stdout, or installing no
    /// transmit sink at all, for a `Device` backend.
    #[test]
    fn a_device_backend_transmits_to_the_device() {
        let pty = Pty::open();
        let (uart, bus) = uart_on_bus();
        attach(
            &uart,
            SerialBackend::Device(&pty.slave_path),
            "com1",
            &quiet_logger(),
        )
        .expect("attach to the pty slave");

        for b in b"OK" {
            bus.handle_out(COM1_BASE, 1, u32::from(*b));
        }

        assert_eq!(pty.read_exact(2), b"OK", "device saw no guest output");
        assert_eq!(REGISTER_LEN, 8, "port window unchanged");
    }

    /// Input is the half that makes `zlogin -C` a console rather than a
    /// log. Without it an operator can watch a guest and never type at
    /// it.
    ///
    /// Mutation this kills: dropping the receive thread, or reading the
    /// device and discarding the bytes instead of calling input_byte.
    #[test]
    fn a_device_backend_feeds_the_guest_what_arrives() {
        let pty = Pty::open();
        let (uart, bus) = uart_on_bus();
        attach(
            &uart,
            SerialBackend::Device(&pty.slave_path),
            "com1",
            &quiet_logger(),
        )
        .expect("attach to the pty slave");

        pty.write(b"hi");

        assert_eq!(guest_read(&bus, 2), b"hi", "guest saw no input");
    }

    /// A path that cannot be opened must be reported, not swallowed.
    /// A silent failure leaves a dead console with no stated reason.
    ///
    /// Mutation this kills: returning Ok(()) when open fails.
    #[test]
    fn a_device_that_cannot_be_opened_is_an_error() {
        let (uart, _bus) = uart_on_bus();
        let err = attach(
            &uart,
            SerialBackend::Device("/nonexistent/nwboot/zconsole"),
            "com1",
            &quiet_logger(),
        )
        .expect_err("a missing device must not report success");
        let text = err.to_string();
        assert!(
            text.contains("/nonexistent/nwboot/zconsole"),
            "error names no path: {text}"
        );
    }

    /// A pipe pair as raw files, the shape of a stdout that a parent
    /// process reads.
    fn pipe() -> (File, File) {
        let mut fds = [0i32; 2];
        // SAFETY: `fds` is a live two-element array, which is exactly
        // what pipe writes, and it writes nothing else.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
        use std::os::fd::FromRawFd;
        // SAFETY: the call above succeeded, so both entries are open
        // descriptors this frame alone owns. Each is handed to one
        // `File`, so neither is closed twice.
        unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
    }

    fn uart_on_pump(pump: &Arc<OutputPump>) -> (Arc<LpcUart>, PioBus) {
        let (uart, bus) = uart_on_bus();
        attach_pump(&uart, "com1", pump);
        (uart, bus)
    }

    /// The vCPU writes the port with the UART lock held. A consumer
    /// that stops reading must cost the guest bytes, not its vCPU.
    ///
    /// Mutation this kills: writing to the consumer from the sink.
    #[test]
    fn a_stalled_consumer_does_not_park_the_port_write() {
        let (_reader, writer) = pipe();
        let pump = OutputPump::start("test", writer, quiet_logger())
            .expect("start the pump");
        let (_uart, bus) = uart_on_pump(&pump);

        // Well past any pipe buffer plus the queue.
        let start = Instant::now();
        for _ in 0..(4 * STDOUT_QUEUE_CAP) {
            bus.handle_out(COM1_BASE, 1, u32::from(b'x'));
        }

        assert!(
            start.elapsed() < Duration::from_secs(5),
            "port writes waited on the consumer"
        );
        assert!(pump.dropped() > 0, "nothing was dropped, so what waited?");
    }

    /// A parent that closes its end of the pipe must not panic the
    /// sink. Under panic=abort that ends the VMM on one guest byte.
    ///
    /// Mutation this kills: writing the byte from the sink, or keeping
    /// the writer thread parked on a queue nothing will ever fill.
    #[test]
    fn a_closed_consumer_does_not_panic_the_sink() {
        let (reader, writer) = pipe();
        drop(reader);
        let pump = OutputPump::start("test", writer, quiet_logger())
            .expect("start the pump");
        let (_uart, bus) = uart_on_pump(&pump);

        for _ in 0..64 {
            bus.handle_out(COM1_BASE, 1, u32::from(b'x'));
        }
        assert!(pump.flush(RX_DEADLINE), "flush did not return");

        assert!(pump.is_closed(), "the broken pipe was not noticed");
        bus.handle_out(COM1_BASE, 1, u32::from(b'x'));
        // The worker holds the second reference, so it has ended.
        let deadline = Instant::now() + RX_DEADLINE;
        while Arc::strong_count(&pump) > 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            Arc::strong_count(&pump),
            2,
            "the writer thread is still parked"
        );
    }

    /// Everything the guest wrote reaches the consumer, in order, and
    /// `flush` does not return before it has.
    #[test]
    fn flush_waits_for_the_consumer_to_have_the_bytes() {
        let (mut reader, writer) = pipe();
        let pump = OutputPump::start("test", writer, quiet_logger())
            .expect("start the pump");
        let (_uart, bus) = uart_on_pump(&pump);
        let want: Vec<u8> =
            (0..200u32).map(|i| b'a' + (i % 26) as u8).collect();
        let collector = std::thread::spawn(move || {
            let mut got = vec![0u8; 200];
            reader.read_exact(&mut got).expect("read the bytes");
            got
        });

        for b in &want {
            bus.handle_out(COM1_BASE, 1, u32::from(*b));
        }
        assert!(pump.flush(RX_DEADLINE), "the queue did not drain");

        assert_eq!(collector.join().expect("collector"), want);
    }

    /// A NUL in the path would truncate at the C boundary and open a
    /// different file from the one named. `File::options().open`
    /// refuses it, and this test holds that contract.
    #[test]
    fn a_device_path_with_an_embedded_nul_is_refused() {
        let (uart, _bus) = uart_on_bus();
        attach(
            &uart,
            SerialBackend::Device("/dev/zconsole\0/x"),
            "com1",
            &quiet_logger(),
        )
        .expect_err("an embedded NUL must be refused");
    }
}
