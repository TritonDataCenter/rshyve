// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What every VMM binary does before `run` and after it.

use std::ffi::{OsStr, OsString};
use std::process::ExitCode;

use slog::{error, o, Drain, Logger};

use crate::teardown::RunOutcome;

/// The process logger, tagged with the binary's name.
pub fn setup_logger(component: &'static str) -> Logger {
    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain).build().fuse();
    Logger::root(drain, o!("component" => component))
}

/// Whether the argv asks for the version. Answered before clap so it
/// works with the required arguments absent.
pub fn version_requested(args: &[OsString]) -> bool {
    args.iter().any(|arg| {
        arg.as_os_str() == OsStr::new("--version")
            || arg.as_os_str() == OsStr::new("-V")
    })
}

/// Register the USDT probes. Not fatal: `/dev/dtrace/helper` may not
/// exist inside a zone.
pub fn register_probes(binary: &str) {
    if let Err(e) = usdt::register_probes() {
        eprintln!("[{binary}] USDT probe registration skipped: {e}");
    }
}

/// The exit code the zone brand reads for a run that ended.
///
/// C bhyve's codes, from usr/src/cmd/bhyve/amd64/vmexit.c. They are
/// not cosmetic: `boot.c` sets ZONE_ATTR_INITRESTART0 on every bhyve
/// zone and the brand declares no <initreboot>, so the kernel restarts
/// init only on exit 0 and halts the zone on anything else. A guest
/// that powers itself off must therefore NOT exit 0, or the zone
/// restarts it for ever.
///
/// vmadm is unaffected. Its stop sends SIGTERM, which C bhyve turns
/// into an ACPI power button (`sci_init` in amd64/pm.c), so an orderly
/// guest shutdown exits 1.
pub fn exit_code(outcome: RunOutcome, log: &Logger) -> ExitCode {
    match outcome {
        RunOutcome::Reboot => ExitCode::SUCCESS,
        RunOutcome::PowerOff => ExitCode::from(1),
        RunOutcome::Halt => ExitCode::from(2),
        RunOutcome::TripleFault(src) => {
            error!(log, "VM triple fault"; "source_vcpu" => src);
            ExitCode::from(3)
        }
        // Distinct from HALT, which C bhyve gives 2.
        RunOutcome::GuestFault => ExitCode::from(4),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet() -> Logger {
        Logger::root(slog::Discard, o!())
    }

    /// C bhyve's codes, from usr/src/cmd/bhyve/amd64/vmexit.c: RESET 0,
    /// POWEROFF 1, HALT 2, TRIPLEFAULT 3. The kernel restarts a zone's
    /// init only on exit 0, so only a reboot may map there.
    #[test]
    fn the_exit_codes_match_the_ones_the_zone_brand_reads() {
        let log = quiet();
        let cases = [
            (RunOutcome::Reboot, 0u8),
            (RunOutcome::PowerOff, 1),
            (RunOutcome::Halt, 2),
            (RunOutcome::TripleFault(0), 3),
            // Distinct from HALT, or a guest fault reads as a clean halt.
            (RunOutcome::GuestFault, 4),
        ];
        for (outcome, code) in cases {
            assert_eq!(
                exit_code(outcome, &log),
                ExitCode::from(code),
                "{outcome:?}"
            );
        }
    }
}
