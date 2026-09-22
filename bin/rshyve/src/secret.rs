// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Secrets read from a file rather than taken from argv.
//!
//! Every argument of this process is in `/proc/<pid>/psinfo`, so `ps` and
//! `pargs` show it to every process in the zone and in the global zone.
//! `reexec_for_reboot` also copies argv into the next run. The options that
//! take a path keep the secret in a file that the brand can protect.

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::Context;
use slog::{warn, Logger};

/// Longest secret file this reads, so a wrong path cannot fill memory.
const MAX_SECRET_BYTES: u64 = 64 * 1024;

/// Read one secret from `path`, without its trailing newline.
///
/// A file that other users can read causes a warning but is still used.
/// The operator chose the path, and a failed boot is worse than a warning.
pub fn read_file(path: &Path, log: &Logger) -> anyhow::Result<String> {
    let file = File::open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to stat {}", path.display()))?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        warn!(log, "a secret file is readable beyond its owner";
            "path" => path.display().to_string(),
            "mode" => format!("{mode:#o}"));
    }

    let mut text = String::new();
    file.take(MAX_SECRET_BYTES)
        .read_to_string(&mut text)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let end = text.trim_end_matches(['\n', '\r']).len();
    text.truncate(end);
    Ok(text)
}

/// Report an option that carries a secret through argv.
pub fn warn_argv_exposure(log: &Logger, option: &str, file_option: &str) {
    warn!(log, "a secret was passed in argv, where every process in the \
        zone can read it";
        "option" => option, "use_instead" => file_option);
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn null_log() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    fn write_secret(tag: &str, body: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("vmm-secret-{tag}-{}", std::process::id(),));
        let mut file = File::create(&path).expect("create a secret file");
        file.write_all(body).expect("write a secret file");
        path
    }

    #[test]
    fn a_trailing_newline_is_not_part_of_the_secret() {
        let path = write_secret("newline", b"hunter2\n");

        let secret = read_file(&path, &null_log()).expect("read the secret");

        assert_eq!(secret, "hunter2");
        std::fs::remove_file(&path).expect("remove the secret file");
    }

    #[test]
    fn an_inner_newline_is_kept() {
        // An SSH key file is several lines and all of them count.
        let path = write_secret("inner", b"one\ntwo\n");

        let secret = read_file(&path, &null_log()).expect("read the secret");

        assert_eq!(secret, "one\ntwo");
        std::fs::remove_file(&path).expect("remove the secret file");
    }

    #[test]
    fn a_missing_file_names_the_path() {
        let path = std::env::temp_dir().join("vmm-secret-absent-file");
        let _ = std::fs::remove_file(&path);

        let error = read_file(&path, &null_log()).expect_err("no such file");

        assert!(
            format!("{error:#}").contains("vmm-secret-absent-file"),
            "{error:#}",
        );
    }
}
