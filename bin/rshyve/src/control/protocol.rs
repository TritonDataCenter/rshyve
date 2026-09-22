// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Wire types for the control socket: the request and response bodies.

use serde::{Deserialize, Serialize};

use super::VmRunState;

#[derive(Debug, Deserialize)]
#[serde(tag = "command")]
pub(super) enum Command {
    #[serde(rename = "status")]
    Status,
    #[serde(rename = "pause")]
    Pause,
    #[serde(rename = "resume")]
    Resume,
    #[serde(rename = "shutdown")]
    Shutdown,
    #[serde(rename = "reset")]
    Reset,
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "migrate-source")]
    MigrateSource {
        target_addr: String,
        /// Unix socket path for the post-pause ZFS sync barrier. The VMM
        /// connects after it pauses the vCPUs. The agent writes back a
        /// status when the final ZFS sync is done.
        #[serde(default)]
        zfs_barrier: Option<String>,
    },
    #[serde(rename = "migrate-dest")]
    MigrateDest {
        listen_addr: String,
        /// Take a source whose guest saw CPU features this host lacks.
        /// The guest takes #UD on the next such instruction, so the
        /// orchestrator has to ask for it per migration.
        #[serde(default)]
        allow_cpu_feature_mismatch: bool,
    },
    #[serde(rename = "migrate-status")]
    MigrateStatus,
    #[serde(rename = "migrate-config")]
    MigrateConfig,
    #[serde(rename = "metrics")]
    Metrics,
    #[serde(rename = "metrics-prometheus")]
    MetricsPrometheus,
    #[serde(rename = "device-list")]
    DeviceList,
    /// Add a `slot,driver[,config]` device, the same grammar `-s` takes.
    #[serde(rename = "device-add")]
    DeviceAdd { spec: String },
    /// Ask the guest to give a device up. Advisory: the device leaves
    /// only when the guest runs `_EJ0`.
    #[serde(rename = "device-remove")]
    DeviceRemove { id: String },
    #[serde(rename = "cpu-list")]
    CpuList,
    /// Bring one more vCPU online. `id` names a CPU slot the tables
    /// describe and the boot path did not fill.
    #[serde(rename = "cpu-add")]
    CpuAdd { id: u32 },
    #[serde(rename = "mem-list")]
    MemList,
    /// Give the guest more memory. `bytes` is a byte count, the same
    /// unit firehyve's `mem-add` takes, and it is rounded up to a whole
    /// hot-add slot.
    #[serde(rename = "mem-add")]
    MemAdd { bytes: u64 },
    /// Recognised only so the answer says why this platform cannot do
    /// it. illumos has no `vm_deactivate_cpu`.
    #[serde(rename = "cpu-remove")]
    CpuRemove,
    /// The same for memory: illumos has no `VM_FREE_MEMSEG`.
    #[serde(rename = "mem-remove")]
    MemRemove,
}

impl Command {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Shutdown => "shutdown",
            Self::Reset => "reset",
            Self::Stop => "stop",
            Self::MigrateSource { .. } => "migrate-source",
            Self::MigrateDest { .. } => "migrate-dest",
            Self::MigrateStatus => "migrate-status",
            Self::MigrateConfig => "migrate-config",
            Self::Metrics => "metrics",
            Self::MetricsPrometheus => "metrics-prometheus",
            Self::DeviceList => "device-list",
            Self::DeviceAdd { .. } => "device-add",
            Self::DeviceRemove { .. } => "device-remove",
            Self::CpuList => "cpu-list",
            Self::CpuAdd { .. } => "cpu-add",
            Self::MemList => "mem-list",
            Self::MemAdd { .. } => "mem-add",
            Self::CpuRemove => "cpu-remove",
            Self::MemRemove => "mem-remove",
        }
    }
}

/// One control response line.
///
/// The named fields are a fixed wire contract. They keep their names,
/// their order and their omit-when-absent behaviour. Other commands
/// supply `data`, which flattens into the same object, so a new command
/// needs one dispatch arm and no change here.
#[derive(Debug, Default, Serialize)]
pub(super) struct Response {
    pub(super) success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) state: Option<VmRunState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) vm_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) num_cpus: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) memory_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) uptime_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) migrate_phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) migrate_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) migrate_pages: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) config: Option<serde_json::Value>,
    /// Command-specific fields, merged into this object. Always a JSON
    /// object: `serde(flatten)` cannot flatten anything else.
    #[serde(flatten)]
    pub(super) data: Option<serde_json::Value>,
}

impl Response {
    pub(super) fn ok() -> Self {
        Self {
            success: true,
            ..Self::default()
        }
    }

    pub(super) fn err(msg: &str) -> Self {
        Self {
            error: Some(msg.to_string()),
            ..Self::default()
        }
    }

    /// Success, plus the fields of `data` merged into the response.
    ///
    /// `data` must be a JSON object. A scalar cannot be flattened, so the
    /// line fails to serialize and the client sees only a closed
    /// connection. A non-object becomes an error response instead.
    pub(super) fn ok_data(data: serde_json::Value) -> Self {
        if !data.is_object() {
            return Self::err(
                "internal error: response payload is not an object",
            );
        }
        Self {
            success: true,
            data: Some(data),
            ..Self::default()
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_deserialize_status() {
        let cmd: Command =
            serde_json::from_str(r#"{"command":"status"}"#).unwrap();
        assert!(matches!(cmd, Command::Status));
    }

    #[test]
    fn command_deserialize_pause() {
        let cmd: Command =
            serde_json::from_str(r#"{"command":"pause"}"#).unwrap();
        assert!(matches!(cmd, Command::Pause));
    }

    #[test]
    fn command_deserialize_shutdown() {
        let cmd: Command =
            serde_json::from_str(r#"{"command":"shutdown"}"#).unwrap();
        assert!(matches!(cmd, Command::Shutdown));
    }

    #[test]
    fn parse_reset_command() {
        let cmd: Command =
            serde_json::from_str(r#"{"command":"reset"}"#).unwrap();
        assert!(matches!(cmd, Command::Reset));
    }

    #[test]
    fn unknown_command_is_rejected() {
        let result = serde_json::from_str::<Command>(r#"{"command":"reboot"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn response_ok_serializes() {
        let r = Response::ok();
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains(r#""success":true"#));
        assert!(!json.contains("error"));
        assert!(!json.contains("state"));
    }

    #[test]
    fn response_err_serializes() {
        let r = Response::err("something broke");
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains(r#""success":false"#));
        assert!(json.contains(r#""error":"something broke""#));
    }

    #[test]
    fn command_deserialize_migrate_source() {
        let cmd: Command = serde_json::from_str(
            r#"{"command":"migrate-source","target_addr":"10.0.0.2:1234"}"#,
        )
        .unwrap();
        assert!(matches!(cmd, Command::MigrateSource { .. }));
    }

    #[test]
    fn command_deserialize_migrate_dest() {
        let cmd: Command = serde_json::from_str(
            r#"{"command":"migrate-dest","listen_addr":"0.0.0.0:1234"}"#,
        )
        .unwrap();
        assert!(matches!(cmd, Command::MigrateDest { .. }));
    }

    #[test]
    fn command_deserialize_migrate_status() {
        let cmd: Command =
            serde_json::from_str(r#"{"command":"migrate-status"}"#).unwrap();
        assert!(matches!(cmd, Command::MigrateStatus));
    }

    #[test]
    fn the_hotplug_commands_parse() {
        let cmd: Command = serde_json::from_str(
            r#"{"command":"device-add","spec":"5,virtio-blk,/d.img"}"#,
        )
        .expect("device-add parses");
        let Command::DeviceAdd { spec } = cmd else {
            panic!("wrong command");
        };
        assert_eq!(spec, "5,virtio-blk,/d.img");

        let cmd: Command = serde_json::from_str(
            r#"{"command":"device-remove","id":"virtio-blk@5"}"#,
        )
        .expect("device-remove parses");
        let Command::DeviceRemove { id } = cmd else {
            panic!("wrong command");
        };
        assert_eq!(id, "virtio-blk@5");
    }

    #[test]
    fn a_hotplug_command_without_its_argument_is_refused() {
        // A device-add with no spec must not read as an empty spec.
        assert!(
            serde_json::from_str::<Command>(r#"{"command":"device-add"}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<Command>(
            r#"{"command":"device-remove"}"#
        )
        .is_err());
    }

    #[test]
    fn the_cpu_and_memory_add_commands_parse() {
        let cmd: Command =
            serde_json::from_str(r#"{"command":"cpu-add","id":3}"#)
                .expect("cpu-add parses");
        assert!(matches!(cmd, Command::CpuAdd { id: 3 }));

        let cmd: Command =
            serde_json::from_str(r#"{"command":"mem-add","bytes":134217728}"#)
                .expect("mem-add parses");
        assert!(matches!(cmd, Command::MemAdd { bytes: 134_217_728 }));
    }

    #[test]
    fn an_add_argument_that_is_not_a_count_is_refused_by_the_decoder() {
        // A negative or fractional id never reaches a handler, so no
        // handler has to guess what one would mean.
        for text in [
            r#"{"command":"cpu-add","id":-1}"#,
            r#"{"command":"cpu-add","id":"3"}"#,
            r#"{"command":"cpu-add"}"#,
            r#"{"command":"mem-add","bytes":-1}"#,
            r#"{"command":"mem-add","bytes":1.5}"#,
            r#"{"command":"mem-add"}"#,
        ] {
            assert!(
                serde_json::from_str::<Command>(text).is_err(),
                "{text} was accepted",
            );
        }
    }

    #[test]
    fn the_removal_commands_parse_so_the_answer_can_say_why() {
        // Left out of the enum they would read as an unknown command,
        // which is a typo an operator would try to fix.
        for (text, name) in [
            (r#"{"command":"cpu-remove"}"#, "cpu-remove"),
            (r#"{"command":"mem-remove"}"#, "mem-remove"),
        ] {
            let cmd: Command =
                serde_json::from_str(text).expect("command parses");
            assert_eq!(cmd.name(), name);
        }
    }

    #[test]
    fn the_read_only_inventory_commands_parse() {
        for (text, name) in [
            (r#"{"command":"device-list"}"#, "device-list"),
            (r#"{"command":"cpu-list"}"#, "cpu-list"),
            (r#"{"command":"mem-list"}"#, "mem-list"),
        ] {
            let cmd: Command =
                serde_json::from_str(text).expect("command parses");
            assert_eq!(cmd.name(), name);
        }
    }

    // The next five tests pin the bytes that existing clients read. The
    // flattened `data` field must not add, remove or reorder a key in
    // any of these lines.

    #[test]
    fn the_status_line_is_unchanged() {
        let mut r = Response::ok();
        r.state = Some(VmRunState::Running);
        r.vm_name = Some("testvm".to_string());
        r.num_cpus = Some(2);
        r.memory_bytes = Some(1_073_741_824);
        r.uptime_secs = Some(42);

        assert_eq!(
            serde_json::to_string(&r).expect("serializes"),
            r#"{"success":true,"state":"running","vm_name":"testvm","num_cpus":2,"memory_bytes":1073741824,"uptime_secs":42}"#
        );
    }

    /// pause, resume, shutdown, reset and stop all answer with this.
    #[test]
    fn the_state_only_line_is_unchanged() {
        let mut r = Response::ok();
        r.state = Some(VmRunState::Paused);

        assert_eq!(
            serde_json::to_string(&r).expect("serializes"),
            r#"{"success":true,"state":"paused"}"#
        );
    }

    #[test]
    fn the_error_line_is_unchanged() {
        assert_eq!(
            serde_json::to_string(&Response::err("VM is not running"))
                .expect("serializes"),
            r#"{"success":false,"error":"VM is not running"}"#
        );
    }

    #[test]
    fn the_migrate_status_line_is_unchanged() {
        let mut r = Response::ok();
        r.state = Some(VmRunState::Migrating);
        r.migrate_phase = Some("dirty-sync".to_string());
        r.migrate_bytes = Some(4096);
        r.migrate_pages = Some(1);

        assert_eq!(
            serde_json::to_string(&r).expect("serializes"),
            r#"{"success":true,"state":"migrating","migrate_phase":"dirty-sync","migrate_bytes":4096,"migrate_pages":1}"#
        );
    }

    #[test]
    fn the_migrate_config_line_is_unchanged() {
        let mut r = Response::ok();
        r.config = Some(serde_json::json!({"num_cpus": 2}));

        assert_eq!(
            serde_json::to_string(&r).expect("serializes"),
            r#"{"success":true,"config":{"num_cpus":2}}"#
        );
    }

    #[test]
    fn a_new_command_flattens_its_own_fields() {
        let r = Response::ok_data(serde_json::json!({"num_cpus": 4}));

        assert_eq!(
            serde_json::to_string(&r).expect("serializes"),
            r#"{"success":true,"num_cpus":4}"#
        );
    }

    #[test]
    fn a_payload_that_is_not_an_object_answers_with_an_error() {
        // serde cannot flatten a scalar. Left alone it fails to
        // serialize, and the client sees a closed socket with no reply.
        let r = Response::ok_data(serde_json::json!(7));

        assert_eq!(
            serde_json::to_string(&r).expect("serializes"),
            r#"{"success":false,"error":"internal error: response payload is not an object"}"#
        );
    }
}
