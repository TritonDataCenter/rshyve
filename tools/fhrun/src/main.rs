// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `fhrun`: make a Linux binary feel like a process by wrapping it in a
//! microVM.

mod cpio;
mod initramfs;
mod launcher;
mod manifest;
mod status;

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};

use crate::manifest::Manifest;

/// fhrun's own failures: a bad manifest, a VMM that could not start,
/// or a VMM that ended before init reported the payload.
const EXIT_FAILURE: u8 = 1;
/// A command line fhrun does not understand.
const EXIT_USAGE: u8 = 2;

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("fhrun: {e:#}");
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

/// The code fhrun exits with once the VMM has ended.
///
/// The payload's status is the answer when init reported one. Without
/// a marker the VMM's own outcome is all there is, and none of its
/// codes means "the payload succeeded": 0 is a reset, which under
/// `panic=-1` is a kernel panic, and 1 is a poweroff that init reached
/// without a payload, so both are fhrun failures.
fn exit_code_for(outcome: &launcher::LaunchOutcome) -> u8 {
    if let Some(payload) = outcome.payload {
        return payload.exit_code();
    }
    match outcome.vmm_exit_code {
        Some(code) => eprintln!(
            "fhrun: the payload reported no status; the vmm exited {code}"
        ),
        None => eprintln!(
            "fhrun: the payload reported no status; the vmm died from a signal"
        ),
    }
    EXIT_FAILURE
}

fn run() -> Result<u8> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        usage();
        return Ok(EXIT_USAGE);
    }

    match args[1].as_str() {
        "-h" | "--help" => {
            usage();
            Ok(0)
        }
        "--check" => {
            let path = args.get(2).context("--check needs a manifest path")?;
            let m = Manifest::load(Path::new(path))?;
            println!("ok: {} ({} vCPU, {})", m.name, m.vcpus, m.memory);
            Ok(0)
        }
        "--print-argv" => {
            let path =
                args.get(2).context("--print-argv needs a manifest path")?;
            let m = Manifest::load(Path::new(path))?;
            // Placeholder paths: this prints the shape of the command
            // line without building an initramfs or a temp directory.
            let inv = launcher::build_vmm_invocation(
                &m,
                Path::new("<initramfs>"),
                Path::new("<runtime-dir>"),
            )?;
            println!("{}", inv.program.display());
            for a in &inv.args {
                println!("{}", a.to_string_lossy());
            }
            Ok(0)
        }
        "--emit-initramfs" => {
            let manifest_path = args.get(2).context("missing manifest path")?;
            let out_path = args.get(3).context("missing output path")?;
            let m = Manifest::load(Path::new(manifest_path))?;
            initramfs::build_to_path(&m, Path::new(out_path))?;
            println!("wrote {out_path}");
            Ok(0)
        }
        path if !path.starts_with('-') => {
            let m = Manifest::load(Path::new(path))?;
            let outcome = launcher::launch(&m)?;
            Ok(exit_code_for(&outcome))
        }
        other => {
            eprintln!("fhrun: unknown flag '{other}'");
            usage();
            Ok(EXIT_USAGE)
        }
    }
}

fn usage() {
    eprintln!(
        "Usage:\n  \
         fhrun <manifest.json>\n  \
         fhrun --check <manifest.json>\n  \
         fhrun --print-argv <manifest.json>\n  \
         fhrun --emit-initramfs <manifest.json> <out.cpio>\n"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launcher::LaunchOutcome;
    use crate::status::PayloadStatus;

    #[test]
    fn the_payload_status_is_the_exit_code() {
        let outcome = LaunchOutcome {
            vmm_exit_code: Some(1),
            payload: Some(PayloadStatus::Exited(3)),
        };
        assert_eq!(exit_code_for(&outcome), 3);

        let outcome = LaunchOutcome {
            vmm_exit_code: Some(1),
            payload: Some(PayloadStatus::Signaled(11)),
        };
        assert_eq!(exit_code_for(&outcome), 139);
    }

    /// A reset is the VMM's exit 0, and under `panic=-1` it is what a
    /// kernel panic looks like. It must not become a success.
    #[test]
    fn a_vmm_outcome_without_a_marker_is_a_failure() {
        for vmm in [Some(0), Some(1), Some(3), None] {
            let outcome = LaunchOutcome {
                vmm_exit_code: vmm,
                payload: None,
            };
            assert_eq!(exit_code_for(&outcome), EXIT_FAILURE, "{vmm:?}");
        }
    }
}
