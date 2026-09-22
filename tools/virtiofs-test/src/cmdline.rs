// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Reading the settings the host put on the kernel command line.
//!
//! Host independent on purpose, so `cargo test` on a developer's own
//! machine covers it. Everything else the guest does needs a Linux
//! kernel under it.

/// Value of `key=` on the kernel command line, if it is there.
///
/// The kernel hands init the arguments it did not consume itself, and
/// the guest reads them back from `/proc/cmdline`, which is one line of
/// space separated tokens.
pub fn param(cmdline: &str, key: &str) -> Option<String> {
    let want = format!("{key}=");
    cmdline
        .split_ascii_whitespace()
        .find_map(|tok| tok.strip_prefix(&want))
        .map(|v| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_reads_back_its_value() {
        let line = "console=ttyS0 virtiofs.tag=testfs panic=30";
        assert_eq!(param(line, "virtiofs.tag").as_deref(), Some("testfs"));
        assert_eq!(param(line, "panic").as_deref(), Some("30"));
    }

    #[test]
    fn a_missing_key_is_none() {
        assert_eq!(param("console=ttyS0", "virtiofs.tag"), None);
    }

    /// A key must match a whole token. Without that, `hotplug.mem` would
    /// read the value of `hotplug.mem_bytes` and the guest would check
    /// the wrong number.
    #[test]
    fn a_key_is_not_a_prefix_of_another_key() {
        let line = "hotplug.mem_bytes=134217728 xvirtiofs.tag=no";
        assert_eq!(param(line, "hotplug.mem"), None);
        assert_eq!(param(line, "virtiofs.tag"), None);
        assert_eq!(
            param(line, "hotplug.mem_bytes").as_deref(),
            Some("134217728")
        );
    }

    /// `key=` with nothing after it reads as an empty value, not as a
    /// missing key. The caller decides whether that is usable.
    #[test]
    fn an_empty_value_is_not_a_missing_key() {
        assert_eq!(
            param("virtiofs.entry= x=1", "virtiofs.entry").as_deref(),
            Some(""),
        );
    }
}
