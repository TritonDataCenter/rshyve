// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Spec format read by `/init` from `/firehyve-spec.json`.
//!
//! Mirrors the runtime-relevant subset of
//! `tools/fhrun/src/manifest.rs::GuestSpec`. It is duplicated rather
//! than shared through a crate, so this binary stays a self-contained
//! static musl binary that the launcher drops into the initramfs.
//!
//! Only the fields init acts on are declared here. serde ignores
//! anything else the host sends, `metadata` above all, and it stays in
//! the spec file for the payload to read.

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct GuestSpec {
    pub bin: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub workdir: String,
    /// All NICs in eth0..ethN order. Init brings each one up in order
    /// using ioctl-based interface configuration.
    #[serde(default)]
    pub nics: Vec<NetConfig>,
    /// Virtio-console channels. Init only logs these: the device nodes
    /// come from devtmpfs.
    #[serde(default)]
    pub consoles: Vec<GuestConsole>,
}

#[derive(Debug, Deserialize)]
pub struct NetConfig {
    pub vnic: String,
    pub mac: String,
    pub ip: String,
    #[serde(default)]
    pub gateway: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
}

/// Guest-visible half of one virtio-console channel. The host socket
/// path is stripped before the spec leaves fhrun.
#[derive(Debug, Deserialize)]
pub struct GuestConsole {
    pub guest_device: String,
    #[serde(default)]
    pub role: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_parses_generic_consoles_and_ignores_metadata() {
        let raw = r#"{
            "bin": "/app",
            "args": [],
            "env": {},
            "workdir": "/",
            "nics": [],
            "consoles": [{"guest_device": "/dev/hvc0", "role": "control"}],
            "metadata": {"tenant": "acme"}
        }"#;

        let spec: GuestSpec = serde_json::from_str(raw).expect("parse spec");

        assert_eq!(spec.bin, "/app");
        assert_eq!(spec.consoles.len(), 1);
        assert_eq!(spec.consoles[0].guest_device, "/dev/hvc0");
        assert_eq!(spec.consoles[0].role.as_deref(), Some("control"));
    }

    #[test]
    fn spec_without_consoles_parses() {
        let raw = r#"{"bin":"/app","args":[],"env":{},"workdir":"/"}"#;

        let spec: GuestSpec = serde_json::from_str(raw).expect("parse spec");

        assert!(spec.consoles.is_empty());
        assert!(spec.nics.is_empty());
    }
}
