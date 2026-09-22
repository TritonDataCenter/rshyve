// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Migration command handlers and their address guards.

use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use slog::{error, info};

use vmm_core::mem::MemCtx;
use vmm_machine::HotplugEngines;

use super::protocol::{Command, Response};
use super::{VmController, VmRunState};
use crate::host_state::HostLocalState;

/// How long a finished source waits to be reaped before it ends itself.
///
/// Long enough for a slow orchestrator to finish its switchover. Short
/// enough that a wedged one does not leave the node holding the whole
/// guest memory of a VM that runs nowhere.
const SOURCE_RELEASE_GRACE: Duration = Duration::from_secs(300);

pub(super) fn migration_source_guard(
    state: HostLocalState,
    has_ahci_cd: bool,
    command: &Command,
) -> Option<Response> {
    if !matches!(command, Command::MigrateSource { .. }) {
        return None;
    }
    if has_ahci_cd {
        return Some(Response::err(
            "ahci-cd is not supported on a migration source",
        ));
    }
    state.migration_blocker().map(Response::err)
}

/// What the VM gained after boot: added CPUs, and added bytes.
fn growth(hotplug: &HotplugEngines) -> (usize, u64) {
    let cpus = hotplug.cpu.as_ref().map_or(0, |cpus| {
        // Ids at or above the boot count that are running. Counted, not
        // indexed: an id is only ever compared here.
        (cpus.boot_cpus()..cpus.max_cpus())
            .filter(|id| cpus.is_online(*id))
            .count()
    });
    let bytes = hotplug.mem.as_ref().map_or(0, |mem| mem.bytes_added());
    (cpus, bytes)
}

/// Why a VM whose device set no longer matches its boot `-s` list
/// cannot be a migration source.
///
/// The destination is started from the boot `-s` list, so a device the
/// guest gained since boot has nowhere to land. The wire names devices
/// by PCI address, so the destination would refuse the payload. The
/// refusal here comes before the guest pauses, and it names the slots
/// to remove.
fn device_set_changed(live_hotplug: &[String]) -> Option<String> {
    if live_hotplug.is_empty() {
        return None;
    }
    Some(format!(
        "this VM hot-added {} device(s) since boot ({}); a migration \
         destination is built from the boot device list and would have \
         none of them",
        live_hotplug.len(),
        live_hotplug.join(" "),
    ))
}

/// Why a VM that grew since boot cannot be a migration source.
///
/// The destination is started from the same command line, so it gets
/// the boot CPU count and the boot memory size. Nothing carries a
/// hot-add across: a vCPU that is running here would have no thread
/// there, and memory the guest is using would have nowhere to land.
/// Refusing is the only safe answer, because the loss would show up as
/// guest corruption on the far side and not as a failed migration.
fn grown_since_boot(added_cpus: usize, added_bytes: u64) -> Option<String> {
    if added_cpus > 0 {
        return Some(format!(
            "this VM hot-added {added_cpus} vCPUs; a migration \
             destination is built from the same command line and would \
             have none of them",
        ));
    }
    if added_bytes > 0 {
        return Some(format!(
            "this VM hot-added {added_bytes} bytes of memory; a \
             migration destination is built from the same command line \
             and would have nowhere to put it",
        ));
    }
    None
}

// ── Migration address validation ────────────────────────────────────

/// Check a TCP migration address (`host:port`). It rejects:
/// - a missing, invalid or zero port
/// - an empty host
/// - the host spellings `localhost`, `127.0.0.1`, `::1` and `0.0.0.0`,
///   because a migration to self has no use
fn validate_migration_addr(addr: &str) -> Result<(), String> {
    let colon = addr.rfind(':').ok_or("missing port (expected host:port)")?;
    let host = &addr[..colon];
    let port_str = &addr[colon + 1..];

    let port: u16 = port_str
        .parse()
        .map_err(|_| format!("invalid port: {port_str}"))?;

    if port == 0 {
        return Err("port 0 is not allowed".into());
    }

    let host_trimmed = host.trim_matches(|c| c == '[' || c == ']');
    if host_trimmed == "localhost"
        || host_trimmed == "127.0.0.1"
        || host_trimmed == "::1"
        || host_trimmed == "0.0.0.0"
    {
        return Err(format!("loopback/wildcard address not allowed: {host}"));
    }

    if host_trimmed.is_empty() {
        return Err("empty host".into());
    }

    Ok(())
}

/// Devices that carry guest-visible state no migration puts back.
///
/// A guest whose driver configured one of these would find it at reset
/// on the far side, with no error anywhere. The list is documented in
/// docs/migration.md; grow it there and here together.
const UNMIGRATABLE: &[&str] = &[
    "ahci-cd",
    "virtio-console",
    "virtio-fs",
    "virtio-vsock",
    "xhci",
];

/// Why a VM carrying a device with unmigratable state cannot be a
/// source.
fn unmigratable_devices(
    devices: &[vmm_migrate::codec::DeviceIdentity],
) -> Option<String> {
    let blocking: Vec<String> = devices
        .iter()
        .filter(|d| UNMIGRATABLE.contains(&d.kind.as_str()))
        .map(|d| d.to_string())
        .collect();
    if blocking.is_empty() {
        return None;
    }
    Some(format!(
        "no migration state is carried for {}; the guest would find \
         the device at reset on the destination",
        blocking.join(" "),
    ))
}

/// Where a migration source connects to.
enum MigrateTarget {
    /// A Unix socket the GZ agent bridges to the peer. The only kind
    /// that works inside a bhyve zone.
    Unix(String),
    Tcp(String),
}

impl MigrateTarget {
    fn parse(target: &str) -> Result<Self, String> {
        if target.starts_with('/') || target.ends_with(".sock") {
            return Ok(Self::Unix(target.to_string()));
        }
        validate_migration_addr(target)?;
        Ok(Self::Tcp(target.to_string()))
    }
}

// ── Handlers ────────────────────────────────────────────────────────

pub(super) fn migrate_source(
    ctrl: &Arc<VmController>,
    target_addr: String,
    zfs_barrier: Option<String>,
) -> Response {
    let target = match MigrateTarget::parse(&target_addr) {
        Ok(target) => target,
        Err(e) => return Response::err(&format!("invalid target_addr: {e}")),
    };

    if !ctrl.transition(VmRunState::Running, VmRunState::Migrating) {
        return Response::err("VM must be running to start migration");
    }

    // The state comes first, then the topology under the lock a hot-add
    // takes. An add that was in flight is therefore counted here, and
    // one that arrives later finds the VM migrating and is refused.
    // Reading the topology before the state change would miss both.
    let refused = {
        let _topology = ctrl.lock_topology();
        let (cpus, bytes) = growth(&ctrl.hotplug);
        grown_since_boot(cpus, bytes)
            .or_else(|| {
                device_set_changed(&vmm_machine::hotplug_specs(&ctrl.registry))
            })
            .or_else(|| unmigratable_devices(&ctrl.migrate_identities()))
    };
    if let Some(reason) = refused {
        ctrl.state
            .store(VmRunState::Running as u8, Ordering::Release);
        return Response::err(&reason);
    }

    let status = Arc::new(Mutex::new(vmm_migrate::MigrationStatus::default()));
    if let Ok(mut ms) = ctrl.migrate_status.lock() {
        *ms = Some(Arc::clone(&status));
    }

    let ctrl_arc = Arc::clone(ctrl);
    let hdl = Arc::clone(&ctrl.hdl);
    let memctx = MemCtx::new(Arc::clone(&ctrl.physmap));
    let mlog = ctrl.log.clone();
    let config = vmm_migrate::source::SourceConfig {
        num_cpus: ctrl.num_cpus,
        mem_size: ctrl.mem_size as u64,
        cpu_baseline: ctrl.cpu_baseline,
        devices: ctrl.migrate_identities(),
        deadlines: vmm_migrate::wire::Deadlines::default(),
        zfs_barrier: zfs_barrier.map(PathBuf::from),
    };

    if let Err(e) =
        thread::Builder::new()
            .name("migrate-source".into())
            .spawn(move || {
                let hooks = source_hooks(&ctrl_arc);
                let outcome = match target {
                    MigrateTarget::Unix(path) => {
                        info!(mlog, "connecting to migration socket";
                        "path" => &path);
                        UnixStream::connect(&path)
                            .map_err(|e| SourceOutcome::Failed(e.to_string()))
                            .and_then(|stream| {
                                run_source(
                                    stream, hdl, &memctx, config, hooks,
                                    status, &mlog,
                                )
                            })
                    }
                    MigrateTarget::Tcp(addr) => {
                        info!(mlog, "connecting to migration peer";
                        "target" => &addr);
                        TcpStream::connect(&addr)
                            .map_err(|e| SourceOutcome::Failed(e.to_string()))
                            .and_then(|stream| {
                                run_source(
                                    stream, hdl, &memctx, config, hooks,
                                    status, &mlog,
                                )
                            })
                    }
                };
                match outcome {
                    Ok(()) => {
                        info!(mlog, "source migration complete");
                        ctrl_arc.finish_migrated_away(SOURCE_RELEASE_GRACE);
                    }
                    Err(SourceOutcome::Committed(why)) => {
                        // The destination may already be running the
                        // guest. Running it here too would put two
                        // copies on one disk, so the VM stays paused
                        // and migrating until an operator decides.
                        error!(mlog, "the guest was handed over and the \
                            destination never confirmed; this VM stays \
                            paused and is NOT released";
                            "error" => why);
                    }
                    Err(e @ SourceOutcome::Failed(_)) => {
                        error!(mlog, "source migration failed"; "error" => %e);
                        ctrl_arc.state.store(
                            VmRunState::Running as u8,
                            Ordering::Release,
                        );
                    }
                }
            })
    {
        ctrl.state
            .store(VmRunState::Running as u8, Ordering::Release);
        return Response::err(&format!(
            "failed to spawn migration thread: {e}"
        ));
    }

    let mut r = Response::ok();
    r.state = Some(VmRunState::Migrating);
    r
}

/// The device hooks the source phases drive.
fn source_hooks(ctrl: &Arc<VmController>) -> vmm_migrate::source::SourceHooks {
    let for_pre_pause = Arc::clone(ctrl);
    let for_flush = Arc::clone(ctrl);
    let for_resume = Arc::clone(ctrl);
    let for_export = Arc::clone(ctrl);
    let for_hyperv = Arc::clone(ctrl);
    vmm_migrate::source::SourceHooks {
        pre_pause: Box::new(move || {
            for_pre_pause.pause_devices_for_migration()
        }),
        flush: Box::new(move || for_flush.flush_devices_for_migration()),
        resume: Box::new(move || for_resume.resume_devices_after_migration()),
        export: Box::new(move || for_export.export_device_state()),
        hyperv: Box::new(move || for_hyperv.export_hyperv()),
    }
}

/// How a source migration ended, when it did not succeed.
enum SourceOutcome {
    /// The guest never left. The VM can run again here.
    Failed(String),
    /// The guest was handed over and the far side never confirmed. The
    /// VM must not run again here.
    Committed(String),
}

impl From<vmm_migrate::codec::MigrateError> for SourceOutcome {
    fn from(error: vmm_migrate::codec::MigrateError) -> Self {
        match error {
            vmm_migrate::codec::MigrateError::Committed(why) => {
                Self::Committed(why)
            }
            other => Self::Failed(other.to_string()),
        }
    }
}

impl std::fmt::Display for SourceOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(why) | Self::Committed(why) => f.write_str(why),
        }
    }
}

fn run_source<S: vmm_migrate::wire::Transport>(
    stream: S,
    hdl: Arc<vmm_core::hdl::VmmHdl>,
    memctx: &MemCtx,
    config: vmm_migrate::source::SourceConfig,
    hooks: vmm_migrate::source::SourceHooks,
    status: Arc<Mutex<vmm_migrate::MigrationStatus>>,
    log: &slog::Logger,
) -> Result<(), SourceOutcome> {
    vmm_migrate::source::run_source(
        stream, hdl, memctx, config, hooks, status, log,
    )
    .map_err(SourceOutcome::from)
}

pub(super) fn migrate_dest(
    ctrl: &Arc<VmController>,
    listen_addr: String,
    allow_cpu_feature_mismatch: bool,
) -> Response {
    if let Err(e) = validate_migration_addr(&listen_addr) {
        return Response::err(&format!("invalid listen_addr: {e}"));
    }

    // Only a paused VM may take an import: the import writes every
    // vCPU's registers and every device's rings, and a running guest
    // would execute against half of them.
    let prev_state = ctrl.state();
    if prev_state != VmRunState::Paused {
        return Response::err("VM must be paused to receive migration");
    }
    ctrl.state
        .store(VmRunState::Migrating as u8, Ordering::Release);

    let status = Arc::new(Mutex::new(vmm_migrate::MigrationStatus::default()));
    if let Ok(mut ms) = ctrl.migrate_status.lock() {
        *ms = Some(Arc::clone(&status));
    }

    let ctrl_arc = Arc::clone(ctrl);
    let hdl = Arc::clone(&ctrl.hdl);
    let memctx = MemCtx::new(Arc::clone(&ctrl.physmap));
    let mlog = ctrl.log.clone();
    let config = vmm_migrate::destination::DestConfig {
        num_cpus: ctrl.num_cpus,
        mem_size: ctrl.mem_size as u64,
        cpu_baseline: ctrl.cpu_baseline,
        devices: ctrl.migrate_identities(),
        deadlines: vmm_migrate::wire::Deadlines::default(),
        allow_cpu_feature_mismatch,
    };

    if let Err(e) =
        thread::Builder::new()
            .name("migrate-dest".into())
            .spawn(move || {
                let hooks = dest_hooks(&ctrl_arc);
                let outcome =
                    accept_one(&listen_addr, &mlog).and_then(|stream| {
                        vmm_migrate::destination::run_destination(
                            stream, hdl, &memctx, config, hooks, status, &mlog,
                        )
                        .map_err(|e| e.to_string())
                    });
                match outcome {
                    Ok(()) => {
                        info!(
                            mlog,
                            "destination migration complete, VM running"
                        );
                        ctrl_arc.state.store(
                            VmRunState::Running as u8,
                            Ordering::Release,
                        );
                    }
                    Err(e) => {
                        // The guest never left this VM's previous state, so
                        // marking it Stopped would strand a live VM no
                        // control command can reach.
                        error!(mlog, "destination migration failed";
                        "error" => %e);
                        ctrl_arc
                            .state
                            .store(prev_state as u8, Ordering::Release);
                    }
                }
            })
    {
        ctrl.state.store(prev_state as u8, Ordering::Release);
        return Response::err(&format!(
            "failed to spawn migration thread: {e}"
        ));
    }

    let mut r = Response::ok();
    r.state = Some(VmRunState::Migrating);
    r
}

/// The device hooks the destination import drives.
fn dest_hooks(ctrl: &Arc<VmController>) -> vmm_migrate::destination::DestHooks {
    let for_restore = Arc::clone(ctrl);
    let for_resume = Arc::clone(ctrl);
    let for_hyperv = Arc::clone(ctrl);
    vmm_migrate::destination::DestHooks {
        restore: Box::new(move |bdf, state| {
            for_restore.restore_device_state(bdf, state)
        }),
        resume: Box::new(move || for_resume.resume_devices()),
        hyperv: Box::new(move |state| for_hyperv.restore_hyperv(state)),
    }
}

/// Bind, take one connection, and give the listener back.
fn accept_one(
    listen_addr: &str,
    log: &slog::Logger,
) -> Result<TcpStream, String> {
    let listener = TcpListener::bind(listen_addr)
        .map_err(|e| format!("bind {listen_addr}: {e}"))?;
    info!(log, "waiting for migration"; "addr" => listen_addr);
    let (stream, peer) =
        listener.accept().map_err(|e| format!("accept: {e}"))?;
    info!(log, "accepted migration connection"; "peer" => %peer);
    Ok(stream)
}

pub(super) fn migrate_status(ctrl: &Arc<VmController>) -> Response {
    let phase_info = ctrl.migrate_status.lock().ok().and_then(|ms| {
        ms.as_ref().and_then(|s| s.lock().ok().map(|s| s.clone()))
    });

    match phase_info {
        Some(s) => {
            let mut r = Response::ok();
            r.state = Some(ctrl.state());
            r.migrate_phase = Some(s.phase.to_string());
            r.migrate_bytes = Some(s.bytes_transferred);
            r.migrate_pages = Some(s.pages_transferred);
            r
        }
        None => {
            let mut r = Response::ok();
            r.state = Some(ctrl.state());
            r
        }
    }
}

pub(super) fn migrate_config(ctrl: &Arc<VmController>) -> Response {
    let config = serde_json::json!({
        "vm_name": ctrl.vm_name,
        "num_cpus": ctrl.num_cpus,
        "memory_mb": ctrl.mem_size / (1024 * 1024),
        "pci_slots": migrate_pci_slots(&ctrl.cli_pci_slots, &ctrl.registry),
        "lpc_devices": ctrl.cli_lpc,
    });
    let mut r = Response::ok();
    r.config = Some(config);
    r
}

/// The `-s` list the destination has to be started with.
///
/// argv plus the devices an operator hot-added. The destination imports
/// the state of every device in the registry, so one it was not told to
/// create would leave the guest a disk short. This is a report only:
/// `migrate-source` refuses a VM whose device set grew. See
/// [`device_set_changed`].
fn migrate_pci_slots(
    argv_slots: &[String],
    registry: &vmm_machine::DeviceRegistry,
) -> Vec<String> {
    let mut slots = argv_slots.to_vec();
    slots.extend(vmm_machine::hotplug_specs(registry));
    slots
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_destination_is_told_about_a_hotplugged_device() {
        // The destination imports the state of every registered device,
        // so a device it was not told to create loses the guest a disk.
        use vmm_machine::{DeviceRegistry, RegisteredDevice};

        let registry = DeviceRegistry::new();
        let mut added = RegisteredDevice::new(
            "virtio-blk@5",
            vmm_machine::parse_bdf("5"),
            None,
            None,
            Some("5,virtio-blk,/added.img".to_string()),
        );
        added.hotpluggable = true;
        registry.insert(added).expect("insert");

        let slots = migrate_pci_slots(
            &["4,nvme,/boot.img,bootindex=1".to_string()],
            &registry,
        );

        assert_eq!(
            slots,
            ["4,nvme,/boot.img,bootindex=1", "5,virtio-blk,/added.img"],
        );
    }

    #[test]
    fn a_machine_with_no_hotplug_reports_its_own_argv() {
        let argv = ["4,nvme,/boot.img".to_string()];
        assert_eq!(
            migrate_pci_slots(&argv, &vmm_machine::DeviceRegistry::new()),
            argv,
        );
    }

    #[test]
    fn migrate_source_rejects_host_local_state() {
        let command = Command::MigrateSource {
            target_addr: "192.0.2.10:4567".to_string(),
            zfs_barrier: None,
        };

        for state in [
            HostLocalState::new(true, false),
            HostLocalState::new(false, true),
            HostLocalState::new(true, true),
        ] {
            let response = migration_source_guard(state, false, &command)
                .expect("host-local state must block migration");
            assert!(!response.success);
            assert!(response
                .error
                .as_deref()
                .expect("error response must explain rejection")
                .contains("host-local state is not carried by migration"));
        }

        assert!(migration_source_guard(
            HostLocalState::default(),
            false,
            &command
        )
        .is_none());
    }

    #[test]
    fn migrate_source_rejects_ahci_cd() {
        let command = Command::MigrateSource {
            target_addr: "192.0.2.10:4567".to_string(),
            zfs_barrier: None,
        };

        let response =
            migration_source_guard(HostLocalState::default(), true, &command)
                .expect("AHCI CD must block migration");
        assert!(!response.success);
        assert_eq!(
            response.error.as_deref(),
            Some("ahci-cd is not supported on a migration source")
        );
    }

    #[test]
    fn a_vm_that_did_not_grow_can_still_migrate() {
        // Starting with --hotplug is not the same as using it. A VM
        // that added nothing has to keep the migration it had.
        assert_eq!(growth(&HotplugEngines::default()), (0, 0));
        assert_eq!(grown_since_boot(0, 0), None);
    }

    #[test]
    fn a_boot_device_set_can_still_migrate() {
        // Only a hot-added device changes the set. A VM that added
        // nothing has to keep the migration it had.
        assert_eq!(device_set_changed(&[]), None);
    }

    #[test]
    fn a_hot_added_device_cannot_be_a_migration_source() {
        // The destination is built from the boot device list, so it has
        // nowhere to put a device the guest gained since.
        let reason =
            device_set_changed(&["5,virtio-blk,/added.img".to_string()])
                .expect("an added device must block migration");
        assert!(reason.contains("hot-added 1 device(s)"), "{reason}");
        assert!(reason.contains("5,virtio-blk,/added.img"), "{reason}");
        assert!(reason.contains("boot device list"), "{reason}");
    }

    fn ident(dev: u8, kind: &str) -> vmm_migrate::codec::DeviceIdentity {
        vmm_migrate::codec::DeviceIdentity {
            bdf: vmm_devices::lifecycle::WireBdf {
                bus: 0,
                dev,
                func: 0,
            },
            kind: kind.to_string(),
        }
    }

    #[test]
    fn a_vm_with_only_migratable_devices_can_be_a_source() {
        assert_eq!(unmigratable_devices(&[]), None);
        assert_eq!(
            unmigratable_devices(&[ident(4, "nvme"), ident(5, "virtio-blk")]),
            None,
        );
    }

    #[test]
    fn a_device_whose_state_is_not_carried_blocks_migration() {
        // The guest driver would find the device at reset on the far
        // side, with no error anywhere.
        for kind in ["xhci", "ahci-cd", "virtio-fs", "virtio-vsock"] {
            let reason = unmigratable_devices(&[ident(6, kind)])
                .unwrap_or_else(|| panic!("{kind} must block migration"));
            assert!(reason.contains(kind), "{reason}");
            assert!(reason.contains("6.0"), "{reason}");
        }
    }

    #[test]
    fn every_blocking_device_is_named_in_the_refusal() {
        // An operator has to see all of them before they can retry.
        let reason =
            unmigratable_devices(&[ident(6, "xhci"), ident(7, "virtio-fs")])
                .expect("two blocking devices");
        assert!(reason.contains("xhci@0.6.0"), "{reason}");
        assert!(reason.contains("virtio-fs@0.7.0"), "{reason}");
    }

    #[test]
    fn a_unix_target_skips_the_tcp_address_check() {
        // The GZ agent's socket path is not a host:port.
        assert!(matches!(
            MigrateTarget::parse("/tmp/vmm-migrate-data.sock"),
            Ok(MigrateTarget::Unix(_)),
        ));
        assert!(matches!(
            MigrateTarget::parse("192.0.2.10:4567"),
            Ok(MigrateTarget::Tcp(_)),
        ));
        MigrateTarget::parse("127.0.0.1:4567")
            .err()
            .expect("loopback is refused");
    }

    #[test]
    fn every_changed_slot_is_named_in_the_refusal() {
        // An operator has to see which slots to remove before they can
        // retry, so the message names all of them.
        let reason = device_set_changed(&[
            "5,virtio-blk,/a.img".to_string(),
            "6,virtio-blk,/b.img".to_string(),
        ])
        .expect("two added devices must block migration");
        assert!(reason.contains("5,virtio-blk,/a.img"), "{reason}");
        assert!(reason.contains("6,virtio-blk,/b.img"), "{reason}");
    }

    #[test]
    fn a_device_still_pending_removal_blocks_migration() {
        // `hotplug_specs` keeps a slot the guest has not ejected, so it
        // is still part of the live set and still unsafe to migrate.
        use vmm_machine::{DeviceRegistry, RegisteredDevice, SlotState};

        let registry = DeviceRegistry::new();
        let mut added = RegisteredDevice::new(
            "virtio-blk@5",
            vmm_machine::parse_bdf("5"),
            None,
            None,
            Some("5,virtio-blk,/added.img".to_string()),
        );
        added.hotpluggable = true;
        let id = added.id.clone();
        registry.insert(added).expect("insert");
        registry
            .set_state(&id, SlotState::Present, SlotState::RemovePending)
            .expect("remove requested");

        assert!(device_set_changed(&vmm_machine::hotplug_specs(&registry))
            .is_some());
    }

    #[test]
    fn a_vm_that_hot_added_cannot_be_a_migration_source() {
        // The destination is built from the same command line, so the
        // loss would show up as guest corruption on the far side and
        // not as a failed migration.
        let cpus = grown_since_boot(2, 0).expect("two added vCPUs");
        assert!(cpus.contains("hot-added 2 vCPUs"), "{cpus}");

        let memory = grown_since_boot(0, 134_217_728).expect("added memory");
        assert!(memory.contains("134217728 bytes"), "{memory}");
    }
}
