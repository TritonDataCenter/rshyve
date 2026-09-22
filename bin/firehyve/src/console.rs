// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! COM1 console and the COM2 marker channel.
//!
//! The default kernel command line names `ttyS0`, so without COM1 a
//! guest produces no output at all. COM2 carries out-of-band markers
//! from the guest. PS/2 and the metadata agent stay rshyve-only.
//!
//! COM1 goes to the backend the zone brand names when that backend is a
//! device path, which is what makes `zlogin -C` a working console. COM2
//! stays on this process's stdout, where the boot harness reads its
//! markers. See [`com1_backend`] and [`report_serial_backends`].

use std::sync::Arc;

use slog::warn;

use vmm_core::intr_pins::LegacyPIC;
use vmm_core::machine::Machine;
use vmm_devices::uart::backend::{attach, SerialBackend};
use vmm_devices::uart::lpc::{self, LpcUart};

const SERVED_PORTS: [&str; 2] = ["com1", "com2"];

/// Where COM1's bytes go.
///
/// Decided from the argv alone, so a test needs no live VM and the
/// choice can be reported before the VM exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Com1Backend {
    /// The stdio this process inherited. zhyve points that at
    /// `/dev/zfd/1`, which zoneadmd copies into the zone log.
    Stdio,
    /// A character device named by `-l com1,<path>`. In a bhyve zone
    /// that is `/dev/zconsole`, the one channel `zlogin -C` reads.
    Device(String),
}

/// Choose the COM1 backend from `-l com1,<backend>`.
///
/// An absolute path is served. Anything else falls back to the
/// inherited stdio, because a refusal stops an unmodified zone brand
/// from ever starting this binary. A relative path is not served: it
/// does not resolve against the operator's working directory, so the
/// file it opens is a guess.
pub(crate) fn com1_backend(lpc_args: &[String]) -> Com1Backend {
    match vmm_machine::find_lpc_device(lpc_args, "com1") {
        Some(backend)
            if backend.starts_with('/') && !backend.contains('\0') =>
        {
            Com1Backend::Device(backend)
        }
        _ => Com1Backend::Stdio,
    }
}

/// Report each `-l com1,<backend>` / `-l com2,<backend>` this binary
/// accepts but does not use.
///
/// The bhyve zone brand puts a backend on every zone. tritond-vmadm
/// always sets the `com1` zonecfg attr to `/dev/zconsole` and `com2`
/// to `socket,/tmp/vm.ttyb`. COM1 serves a device path, so only a
/// backend firehyve cannot serve is reported. A refusal means an
/// unmodified brand can never start this binary.
///
/// The log line tells the operator where the bytes go. A port that
/// falls back to stdio writes to `/dev/zfd/1`, which zoneadmd copies
/// into the zone log, NOT to the `/dev/zconsole` the brand asked for.
/// So `zlogin -C` shows nothing for that port.
///
/// Kept apart from the attach functions so a test needs no live VM,
/// and so `validate_cli` can call it before the VM exists.
pub(crate) fn report_serial_backends(lpc_args: &[String], log: &slog::Logger) {
    let com1 = com1_backend(lpc_args);
    for port in SERVED_PORTS {
        let Some(backend) = vmm_machine::find_lpc_device(lpc_args, port) else {
            continue;
        };
        if backend == "stdio" {
            continue;
        }
        if port == "com1" && com1 == Com1Backend::Device(backend.clone()) {
            continue;
        }
        warn!(log, "serial backend ignored";
            "port" => port,
            "requested" => &backend,
            "served_on" => "inherited stdio",
            "note" => "zoneadmd copies stdout to the zone log, \
                       not to the zone console",
        );
    }
}

/// Attach COM1 and connect it to the backend the argv named.
///
/// A device that cannot be opened falls back to stdio, and the boot
/// continues: a guest with its output in the zone log is better than
/// no guest. The warning names the fallback.
pub(crate) fn setup_console(
    machine: &Machine,
    pic: &Arc<LegacyPIC>,
    lpc_args: &[String],
    log: &slog::Logger,
) -> anyhow::Result<Arc<LpcUart>> {
    let uart =
        vmm_machine::attach_uart(machine, pic, lpc::COM1_BASE, lpc::COM1_IRQ);
    if let Com1Backend::Device(path) = com1_backend(lpc_args) {
        match attach(&uart, SerialBackend::Device(&path), "com1", log) {
            Ok(()) => return Ok(uart),
            Err(e) => warn!(log, "serial device unusable, falling back";
                "port" => "com1",
                "requested" => &path,
                "error" => %e,
            ),
        }
    }
    attach(&uart, SerialBackend::Stdio, "com1", log)?;
    Ok(uart)
}

/// Attach COM2 and mirror it to stdout as a one-way marker channel.
///
/// Separate from COM1 on purpose. Bytes a guest writes to `/dev/ttyS1`
/// never enter the kernel console or printk path. So the host still
/// reads boot markers when the guest removes `console=` from its
/// command line for speed. `fhrun-init` sends `init-start` and `ready`
/// this way, and the fhrun boot harness times them.
///
/// Transmit only. COM1 owns the single stdin reader, and a second
/// reader would take bytes away from it.
pub(crate) fn setup_com2(
    machine: &Machine,
    pic: &Arc<LegacyPIC>,
    log: &slog::Logger,
) -> anyhow::Result<Arc<LpcUart>> {
    let uart =
        vmm_machine::attach_uart(machine, pic, lpc::COM2_BASE, lpc::COM2_IRQ);
    attach(&uart, SerialBackend::StdoutOnly, "com2", log)?;
    Ok(uart)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn lpc(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_string()).collect()
    }

    /// Keeps every record so a test can prove firehyve logged an
    /// ignored option.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl Recorder {
        fn lines(&self) -> Vec<String> {
            self.0.lock().expect("recorder lock").clone()
        }

        fn logger(&self) -> slog::Logger {
            slog::Logger::root(self.clone(), slog::o!())
        }
    }

    struct Flatten(String);

    impl slog::Serializer for Flatten {
        fn emit_arguments(
            &mut self,
            key: slog::Key,
            val: &std::fmt::Arguments<'_>,
        ) -> slog::Result {
            use std::fmt::Write as _;
            let _ = write!(self.0, " {key}={val}");
            Ok(())
        }
    }

    impl slog::Drain for Recorder {
        type Ok = ();
        type Err = slog::Never;

        fn log(
            &self,
            record: &slog::Record<'_>,
            values: &slog::OwnedKVList,
        ) -> Result<Self::Ok, Self::Err> {
            use slog::KV as _;
            let mut out =
                Flatten(format!("{} {}", record.level(), record.msg()));
            let _ = record.kv().serialize(record, &mut out);
            let _ = values.serialize(record, &mut out);
            self.0.lock().expect("recorder lock").push(out.0);
            Ok(())
        }
    }

    #[test]
    fn a_stdio_backend_or_no_entry_at_all_reports_nothing() {
        // Nothing is ignored in these cases, so a log line is noise.
        for args in [lpc(&[]), lpc(&["com1,stdio"]), lpc(&["com2,stdio"])] {
            let rec = Recorder::default();
            report_serial_backends(&args, &rec.logger());
            assert!(rec.lines().is_empty(), "reported: {:?}", rec.lines());
        }
    }

    /// tritond-vmadm sets the `com1` zonecfg attr to `/dev/zconsole` on
    /// every bhyve zone, and that path is the only channel `zlogin -C`
    /// reads. It is served, so it must not be reported as ignored.
    ///
    /// Mutation this kills: accepting and dropping every com1 backend
    /// that is not stdio.
    #[test]
    fn the_com1_device_the_brand_emits_is_served_not_ignored() {
        assert_eq!(
            com1_backend(&lpc(&["com1,/dev/zconsole"])),
            Com1Backend::Device("/dev/zconsole".to_string()),
        );

        let rec = Recorder::default();
        report_serial_backends(&lpc(&["com1,/dev/zconsole"]), &rec.logger());
        assert!(rec.lines().is_empty(), "reported: {:?}", rec.lines());
    }

    /// firehyve has no Unix-socket serial backend, so a com1 socket
    /// falls back to stdio and logs a warning.
    ///
    /// Mutation this kills: treating any backend string as a device
    /// path, which would make firehyve open a file called
    /// `socket,/tmp/vm.ttya`.
    #[test]
    fn a_com1_socket_backend_falls_back_to_stdio_and_is_logged() {
        assert_eq!(
            com1_backend(&lpc(&["com1,socket,/tmp/vm.ttya"])),
            Com1Backend::Stdio,
        );

        let rec = Recorder::default();
        report_serial_backends(
            &lpc(&["com1,socket,/tmp/vm.ttya"]),
            &rec.logger(),
        );
        let lines = rec.lines();
        assert_eq!(lines.len(), 1, "expected one line, got {lines:?}");
        assert!(lines[0].starts_with("WARN"), "must warn: {}", lines[0]);
        assert!(
            lines[0].contains("/tmp/vm.ttya"),
            "names no backend: {}",
            lines[0]
        );
    }

    /// A relative path does not resolve against the operator's working
    /// directory, so the file it opens is a guess.
    ///
    /// Mutation this kills: dropping the leading-slash test.
    #[test]
    fn a_relative_com1_backend_is_not_opened() {
        assert_eq!(com1_backend(&lpc(&["com1,zconsole"])), Com1Backend::Stdio);
    }

    /// A NUL truncates the path at the C boundary, so the file opened
    /// is not the file named.
    ///
    /// Mutation this kills: dropping the NUL test and leaving the
    /// refusal to the open call.
    #[test]
    fn a_com1_backend_with_an_embedded_nul_is_not_opened() {
        assert_eq!(
            com1_backend(&lpc(&["com1,/dev/zconsole\0/x"])),
            Com1Backend::Stdio,
        );
    }

    /// `-l com2,socket,/tmp/vm.ttyb` is on every bhyve zone, and
    /// firehyve has no COM2 agent to serve it.
    ///
    /// Mutation this kills: reporting com1 only.
    #[test]
    fn the_com2_backend_the_brand_emits_is_accepted_and_logged() {
        let rec = Recorder::default();
        report_serial_backends(
            &lpc(&["com2,socket,/tmp/vm.ttyb"]),
            &rec.logger(),
        );

        let lines = rec.lines();
        assert_eq!(lines.len(), 1, "expected one line, got {lines:?}");
        assert!(lines[0].contains("com2"), "names no port: {}", lines[0]);
        assert!(
            lines[0].contains("/tmp/vm.ttyb"),
            "names no backend: {}",
            lines[0]
        );
    }

    /// Both ports at once, as the brand builds them. COM1 is served on
    /// the device, so only COM2 is reported.
    #[test]
    fn only_the_unserved_brand_backend_is_reported() {
        let rec = Recorder::default();
        report_serial_backends(
            &lpc(&["com1,/dev/zconsole", "com2,socket,/tmp/vm.ttyb"]),
            &rec.logger(),
        );
        let lines = rec.lines();
        assert_eq!(lines.len(), 1, "got {lines:?}");
        assert!(lines[0].contains("com2"), "wrong port: {}", lines[0]);
    }

    /// Mutation this kills: matching any `-l` entry instead of the two
    /// serial ports. The bootrom is then reported here and again in
    /// validate_cli.
    #[test]
    fn an_lpc_entry_for_another_device_is_not_reported() {
        let rec = Recorder::default();
        report_serial_backends(
            &lpc(&["bootrom,/usr/share/bhyve/uefi-rom.bin"]),
            &rec.logger(),
        );
        assert!(rec.lines().is_empty(), "reported: {:?}", rec.lines());
    }
}
