// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use vmm_config::Cli;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostLocalState {
    has_vtpm: bool,
    has_varstore: bool,
}

impl HostLocalState {
    pub const fn new(has_vtpm: bool, has_varstore: bool) -> Self {
        Self {
            has_vtpm,
            has_varstore,
        }
    }

    pub const fn migration_blocker(self) -> Option<&'static str> {
        match (self.has_vtpm, self.has_varstore) {
            (false, false) => None,
            (true, false) => Some(
                "migrate-source refused: the VM has a vTPM whose host-local state is not carried by migration; the guest would arrive with a freshly manufactured TPM and TPM-sealed secrets such as BitLocker keys could be lost",
            ),
            (false, true) => Some(
                "migrate-source refused: the VM has a bootrom variable store whose host-local state is not carried by migration",
            ),
            (true, true) => Some(
                "migrate-source refused: the VM has a vTPM and bootrom variable store whose host-local state is not carried by migration; the guest would arrive with freshly manufactured firmware and TPM state, and TPM-sealed secrets such as BitLocker keys could be lost",
            ),
        }
    }
}

pub fn validate_startup(cli: &Cli) -> anyhow::Result<()> {
    anyhow::ensure!(
        !(cli.vtpm && cli.migrate_listen.is_some()),
        "--vtpm cannot be combined with --migrate-listen: vTPM state is host-local and is not carried by migration; the guest would arrive with a freshly manufactured TPM and any TPM-sealed secret, including BitLocker, would be lost",
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(args.iter().copied()).expect("parse test CLI")
    }

    #[test]
    fn vtpm_and_migrate_listen_are_mutually_exclusive() {
        let both = cli(&[
            "rshyve",
            "--vtpm",
            "--migrate-listen",
            "192.0.2.10:4567",
            "test-vm",
        ]);
        let vtpm = cli(&["rshyve", "--vtpm", "test-vm"]);
        let migrate =
            cli(&["rshyve", "--migrate-listen", "192.0.2.10:4567", "test-vm"]);

        let error = validate_startup(&both).expect_err("combination must fail");
        assert!(error.to_string().contains("--vtpm"));
        assert!(error.to_string().contains("--migrate-listen"));
        assert!(error.to_string().contains("BitLocker"));
        assert!(validate_startup(&vtpm).is_ok());
        assert!(validate_startup(&migrate).is_ok());
    }
}
