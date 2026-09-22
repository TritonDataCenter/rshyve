// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! COM1 and COM2: the UARTs, their host-side backends, and the mdata
//! agent that serves COM2 when argv names no backend for it.

use std::io::Read;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TrySendError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use slog::{debug, info, warn, Logger};

use vmm_config::Cli;
use vmm_core::intr_pins::LegacyPIC;
use vmm_core::machine::Machine;
use vmm_core::ratelimit::TokenBucket;
use vmm_core::unixsock::{accept, write_all_bounded};
use vmm_devices::uart::backend::SerialBackend;
use vmm_devices::uart::lpc::{self, LpcUart};
use vmm_machine::find_lpc_device;

use crate::mdata;

/// Pause between two tries to push a byte into a full RX FIFO.
const RX_RETRY: Duration = Duration::from_millis(1);

/// How long host bytes wait for the guest to drain the RX FIFO.
const RX_BUDGET: Duration = Duration::from_millis(100);

/// How long one write to a socket backend may wait for its peer.
const SOCKET_WRITE_BUDGET: Duration = Duration::from_millis(200);

/// Bytes the guest may have in flight to a socket backend.
const SOCKET_TX_BYTES: usize = 8192;

/// Guest output taken off the UART on the vCPU thread and handed to a
/// consumer thread.
///
/// The TX sink runs inside the PIO handler with the UART lock held, so
/// it must not wait. It pushes into a bounded channel and drops the byte
/// when the channel is full. The consumer blocks on the other end, so an
/// idle port costs no wakeups.
pub struct TxQueue {
    pub bytes: Receiver<u8>,
    dropped: Arc<AtomicU64>,
}

impl TxQueue {
    /// Install the sink on `uart`, buffering up to `capacity` bytes.
    pub fn install(uart: &LpcUart, capacity: usize) -> Self {
        let (tx, bytes) = mpsc::sync_channel(capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&dropped);
        uart.set_tx_sink(Box::new(move |b: u8| match tx.try_send(b) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                counter.fetch_add(1, Ordering::Relaxed);
            }
            // The consumer is gone, so there is nothing to count.
            Err(TrySendError::Disconnected(_)) => {}
        }));
        Self { bytes, dropped }
    }

    /// Bytes dropped since the last call.
    pub fn take_dropped(&self) -> u64 {
        self.dropped.swap(0, Ordering::Relaxed)
    }
}

/// Push `bytes` into the UART RX FIFO, giving up after `budget`.
///
/// The FIFO holds 256 bytes, so more bytes wait for the guest to read.
/// The guest may never read, so the wait has a limit. The error is the
/// count of bytes taken.
pub fn input_bytes(
    uart: &LpcUart,
    bytes: &[u8],
    budget: Duration,
) -> Result<(), usize> {
    let deadline = Instant::now() + budget;
    for (sent, &b) in bytes.iter().enumerate() {
        while !uart.input_byte(b) {
            if Instant::now() >= deadline {
                return Err(sent);
            }
            thread::sleep(RX_RETRY);
        }
    }
    Ok(())
}

/// Create and attach COM1/COM2 UARTs, set up serial I/O paths, and
/// optionally start the mdata agent on COM2.
pub fn setup_serial(
    machine: &Machine,
    pic: &Arc<LegacyPIC>,
    cli: &Cli,
    log: &Logger,
) -> anyhow::Result<(Arc<LpcUart>, Arc<LpcUart>)> {
    let com1 =
        vmm_machine::attach_uart(machine, pic, lpc::COM1_BASE, lpc::COM1_IRQ);
    info!(log, "COM1 attached"; "port" => format!("{:#x}", lpc::COM1_BASE));
    let com2 =
        vmm_machine::attach_uart(machine, pic, lpc::COM2_BASE, lpc::COM2_IRQ);
    info!(log, "COM2 attached"; "port" => format!("{:#x}", lpc::COM2_BASE));

    let com1_path = find_lpc_device(&cli.lpc, "com1");
    let com2_path = find_lpc_device(&cli.lpc, "com2");

    setup_serial_io(&com1, com1_path.as_deref(), "com1", log)?;

    // COM2 serves the backend the argv named, or the built-in mdata
    // agent when it named none.
    if let Some(ref path) = com2_path {
        setup_serial_io(&com2, Some(path.as_str()), "com2", log)?;
    } else {
        let root_pw = root_password(cli, log)?;
        let mdata_store = Arc::new(mdata::build_store_from_zone_config(
            &cli.vm_name,
            cli.uuid.as_deref(),
            cli.mdata_nics.as_deref(),
            cli.mdata_resolvers.as_deref(),
            cli.mdata_ssh_keys.as_deref(),
            root_pw.as_deref(),
        ));
        mdata::start_mdata_agent(com2.clone(), mdata_store, log.clone())
            .context("failed to start the mdata agent")?;
        info!(log, "mdata agent started on COM2");
    }

    Ok((com1, com2))
}

/// The root password the mdata agent serves, from a file when argv
/// named one.
fn root_password(cli: &Cli, log: &Logger) -> anyhow::Result<Option<String>> {
    if let Some(path) = cli.mdata_root_pw_file.as_deref() {
        return crate::secret::read_file(std::path::Path::new(path), log)
            .map(Some);
    }
    if cli.mdata_root_pw.is_some() {
        crate::secret::warn_argv_exposure(
            log,
            "--mdata-root-pw",
            "--mdata-root-pw-file",
        );
    }
    Ok(cli.mdata_root_pw.clone())
}

/// Set up serial I/O for a COM port.
///
/// Supports:
/// - `stdio` or absent → stdout/stdin
/// - `socket,/path` → Unix domain socket (host metadata agent connects)
/// - `/dev/zconsole` or any path → open as read/write device
///
/// The last two forms of backend live in `vmm_devices::uart::backend`,
/// so this binary and firehyve serve a zone console the same way.
fn setup_serial_io(
    uart: &Arc<LpcUart>,
    config: Option<&str>,
    name: &'static str,
    log: &Logger,
) -> anyhow::Result<()> {
    if let Some(path) = config.and_then(|c| c.strip_prefix("socket,")) {
        setup_serial_socket(uart, path, name, log)?;
        return Ok(());
    }

    let backend = match config {
        None | Some("stdio") => SerialBackend::Stdio,
        Some(path) => SerialBackend::Device(path),
    };
    // A console that cannot be opened is not worth failing a boot for:
    // the guest still runs, and the warning names the path.
    if let Err(e) = vmm_devices::uart::backend::attach(uart, backend, name, log)
    {
        warn!(log, "serial backend unusable"; "port" => name,
            "error" => %e);
    }
    Ok(())
}

/// Serve a COM port over a Unix domain socket at `path`.
///
/// One client at a time. Two threads do the work, so that neither
/// direction can stop a vCPU:
///
/// - TX: the sink is [`TxQueue`], which never waits. A writer thread
///   drains it into the socket with a bounded write. The sink runs inside
///   the PIO handler with the UART lock held, and `SO_SNDTIMEO` has no
///   effect on illumos. A host agent that stops reading costs dropped
///   bytes, not a stuck vCPU.
/// - RX: the accept thread reads from the client and pushes into the RX
///   FIFO with a bounded wait, so a guest that never opens the port
///   cannot block that thread.
fn setup_serial_socket(
    uart: &Arc<LpcUart>,
    path: &str,
    name: &'static str,
    log: &Logger,
) -> std::io::Result<()> {
    let listener = vmm_core::unixsock::bind_restricted(
        std::path::Path::new(path),
        vmm_core::unixsock::SocketPolicy::default(),
    )?;
    info!(log, "serial socket listening"; "port" => name, "path" => path);

    let queue = TxQueue::install(uart, SOCKET_TX_BYTES);
    let (conn_tx, conn_rx) = mpsc::channel();
    // Set when the writer's queue closes, which happens only when the
    // UART is gone. Neither thread then outlives the device it serves.
    let shutdown = Arc::new(AtomicBool::new(false));

    let writer_shutdown = Arc::clone(&shutdown);
    let writer_log = log.clone();
    thread::Builder::new()
        .name(format!("{name}-socket-tx"))
        .spawn(move || {
            socket_writer_loop(queue, conn_rx, name, &writer_log);
            writer_shutdown.store(true, Ordering::Release);
        })?;

    let uart_rx = Arc::clone(uart);
    let accept_log = log.clone();
    thread::Builder::new()
        .name(format!("{name}-socket"))
        .spawn(move || {
            let stalled = TokenBucket::new(2, Duration::from_secs(60));
            accept::accept_loop(
                listener,
                &shutdown,
                &accept_log,
                name,
                |stream| {
                    serve_socket_client(
                        &uart_rx,
                        stream,
                        &conn_tx,
                        name,
                        &accept_log,
                        &stalled,
                    );
                },
            );
        })?;

    Ok(())
}

/// Bridge one connected client until it disconnects.
fn serve_socket_client(
    uart: &LpcUart,
    stream: UnixStream,
    conns: &Sender<Option<UnixStream>>,
    name: &'static str,
    log: &Logger,
    stalled: &TokenBucket,
) {
    // The writer needs its own descriptor: it writes outside any lock
    // this thread holds, and this thread keeps reading meanwhile.
    let writer = match stream.try_clone() {
        Ok(writer) => writer,
        Err(e) => {
            warn!(log, "serial client refused"; "port" => name,
                "error" => %e);
            return;
        }
    };
    if conns.send(Some(writer)).is_err() {
        return;
    }
    info!(log, "serial client connected"; "port" => name);

    let mut reader = &stream;
    let mut buf = [0u8; 64];
    loop {
        let read = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if let Err(sent) = input_bytes(uart, &buf[..read], RX_BUDGET) {
            // Waiting without end would pin this thread and keep every
            // later client behind it.
            if stalled.take() {
                warn!(log, "guest is not reading the serial port; \
                    host bytes dropped";
                    "port" => name, "taken" => sent, "offered" => read);
            }
        }
    }

    info!(log, "serial client disconnected"; "port" => name);
    if conns.send(None).is_err() {
        // The writer thread is gone, so there is no stream left to
        // clear and nothing to report.
        debug!(log, "serial writer is gone"; "port" => name);
    }
}

/// Drain guest output into whichever client is connected.
///
/// Runs for the life of the port, so the connected socket arrives over
/// `conns` and is not captured.
fn socket_writer_loop(
    queue: TxQueue,
    conns: Receiver<Option<UnixStream>>,
    name: &'static str,
    log: &Logger,
) {
    let stalled = TokenBucket::new(2, Duration::from_secs(60));
    let dropped = TokenBucket::new(2, Duration::from_secs(60));
    let mut client: Option<UnixStream> = None;

    while let Ok(first) = queue.bytes.recv() {
        // Act on a connection change only between writes. That is the
        // only point where this thread owns no socket.
        while let Ok(next) = conns.try_recv() {
            client = next;
        }

        let mut out = vec![first];
        while let Ok(b) = queue.bytes.try_recv() {
            out.push(b);
        }
        let lost = queue.take_dropped();
        if lost > 0 && dropped.take() {
            debug!(log, "serial output dropped"; "port" => name,
                "bytes" => lost);
        }

        let Some(stream) = client.as_ref() else {
            continue;
        };
        if let Err(e) = write_all_bounded(stream, &out, SOCKET_WRITE_BUDGET) {
            // Part of the write may be on the socket, so the stream
            // cannot be used again. Dropping the connection lets the
            // agent reconnect and resynchronise.
            if stalled.take() {
                warn!(log, "serial client did not take its output";
                    "port" => name, "error" => %e);
            }
            if let Err(e) = stream.shutdown(Shutdown::Both) {
                debug!(log, "serial client shutdown failed";
                    "port" => name, "error" => %e);
            }
            client = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use vmm_core::intr_pins::NoOpPin;
    use vmm_core::pio::PioBus;
    use vmm_devices::uart::lpc::COM2_BASE;

    use super::*;

    fn uart() -> Arc<LpcUart> {
        LpcUart::new(Arc::new(NoOpPin))
    }

    /// The sink runs on the vCPU thread with the UART lock held.
    #[test]
    fn a_guest_write_with_no_consumer_drops_instead_of_waiting() {
        let uart = uart();
        let bus = PioBus::new();
        uart.attach(&bus, COM2_BASE);
        let queue = TxQueue::install(&uart, 4);

        let started = Instant::now();
        for _ in 0..1000 {
            bus.handle_out(COM2_BASE, 1, u32::from(b'x'));
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the PIO writes took {:?}",
            started.elapsed(),
        );

        let mut taken = 0;
        while queue.bytes.try_recv().is_ok() {
            taken += 1;
        }
        assert_eq!(taken, 4);
        assert_eq!(queue.take_dropped(), 996);
        // Counted once: a second read is for the next interval.
        assert_eq!(queue.take_dropped(), 0);
    }

    #[test]
    fn host_bytes_stop_waiting_for_a_guest_that_does_not_read() {
        let uart = uart();
        // Longer than the 256 byte RX FIFO, with nothing draining it.
        let offered = [b'x'; 300];

        let started = Instant::now();
        let taken = input_bytes(&uart, &offered, Duration::from_millis(50))
            .expect_err("a full FIFO with no reader must not be waited on");

        assert_eq!(taken, 256);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the push took {:?}",
            started.elapsed(),
        );
    }

    /// A stalled peer must cost dropped bytes, not a stopped vCPU.
    #[test]
    fn a_client_that_reads_nothing_does_not_stop_the_guest() {
        let (_peer, server) = UnixStream::pair().expect("a socket pair");
        let uart = uart();
        let bus = PioBus::new();
        uart.attach(&bus, COM2_BASE);
        let queue = TxQueue::install(&uart, SOCKET_TX_BYTES);
        let (conn_tx, conn_rx) = mpsc::channel();
        conn_tx.send(Some(server)).expect("hand over the client");
        drop(conn_tx);

        let log = Logger::root(slog::Discard, slog::o!());
        let writer = thread::spawn(move || {
            socket_writer_loop(queue, conn_rx, "com2", &log);
        });

        // Far more than any socket buffer, from a peer that never reads.
        // Check the longest single write: a sink that waits shows the
        // writer's whole budget in one write, whatever the machine load.
        let mut slowest = Duration::ZERO;
        for _ in 0..(64 << 10) {
            let started = Instant::now();
            bus.handle_out(COM2_BASE, 1, u32::from(b'x'));
            slowest = slowest.max(started.elapsed());
        }

        assert!(
            slowest < SOCKET_WRITE_BUDGET,
            "one guest write took {slowest:?}",
        );

        // Dropping the UART drops the sink, and with it the only sender.
        drop(bus);
        drop(uart);
        writer.join().expect("the writer thread must end");
    }
}
