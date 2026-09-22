// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The migration source.
//!
//! This module owns the order of the phases:
//!
//! 1. Pause the devices and wait for them to quiesce. Every thread that
//!    writes guest memory must stop before the last dirty pass. If not,
//!    that pass ships a used ring that the destination never sees
//!    filled.
//! 2. Pause the vCPUs.
//! 3. Flush every backing store, then wait for the ZFS barrier. The
//!    destination disk must hold every write that the guest saw
//!    complete before the guest resumes there.
//! 4. Send the last dirty pass, the time data and the device state.
//! 5. Hand over.
//!
//! A failure from step 1 until the commit rolls the source back: the
//! devices and the paused rings resume, and the vCPUs run again.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vmm_core::cpuid::CpuBaseline;
use vmm_core::hdl::VmmHdl;
use vmm_core::mem::{MemCtx, RegionKind};
use vmm_devices::lifecycle::{
    DeviceStateError, DeviceStatePayload, HypervMigrateState,
};
use vmm_devices::FlushError;

use crate::codec::{
    DeviceIdentity, Message, MigrateError, MigrationPreamble, ProtocolOffer,
    ProtocolSelect, PAGE_BATCH_FLAG_ZSTD,
};
use crate::cpu;
use crate::limits::{BATCH_SIZE, ZSTD_LEVEL};
use crate::pages::{
    are_contiguous, build_contiguous_gpas, page_buffer_len, read_pages,
};
use crate::protocol::{PageIter, PAGE_SIZE, PROTOCOL_RON};
use crate::state;
use crate::wire::{set_phase, Chan, Deadlines, Transport};
use crate::MigrationPhase;
use crate::MigrationStatus;

/// The byte the ZFS barrier sends when the final incremental is on the
/// destination.
pub const ZFS_BARRIER_OK: u8 = 0;

/// Stop every device worker and every kernel ring, then wait for them
/// to quiesce. Runs before the vCPUs pause.
///
/// A running device writes guest memory after the final dirty pass. A
/// running kernel ring moves the indices that the export reads.
pub type DevicePrePauseFn =
    Box<dyn FnOnce() -> Result<(), DeviceStateError> + Send>;

/// Read every device's migration state. The devices are already
/// paused, so this only reads.
pub type DeviceExportFn = Box<
    dyn FnOnce() -> Result<Vec<DeviceStatePayload>, DeviceStateError> + Send,
>;

/// Flush device writeback caches. Runs after `hdl.pause()` and before
/// the ZFS barrier, so `zfs send` sees every write that the guest saw
/// complete.
pub type DeviceFlushFn =
    Box<dyn FnOnce() -> Result<(), DeviceFlushError> + Send>;

/// A device's backing store flush failed.
#[derive(Debug, thiserror::Error)]
#[error("{device}: {source}")]
pub struct DeviceFlushError {
    pub device: &'static str,
    pub source: FlushError,
}

/// Resume the devices after a migration fails with the guest paused.
pub type DeviceResumeFn = Box<dyn Fn() + Send>;

/// Export the Hyper-V enlightenment. Remove its overlay pages first, so
/// the RAM image holds the guest's own bytes.
///
/// `None` for a VM started without `--hyperv`.
pub type HypervExportFn =
    Box<dyn FnOnce() -> Option<HypervMigrateState> + Send>;

/// Source settings, other than the stream.
pub struct SourceConfig {
    pub num_cpus: u32,
    pub mem_size: u64,
    pub cpu_baseline: CpuBaseline,
    /// Every device that carries migration state. The destination
    /// compares topology before it accepts a page.
    pub devices: Vec<DeviceIdentity>,
    pub deadlines: Deadlines,
    /// Socket where the GZ agent reports the final ZFS incremental.
    pub zfs_barrier: Option<PathBuf>,
}

/// Device lifecycle hooks that the phases call.
pub struct SourceHooks {
    pub pre_pause: DevicePrePauseFn,
    pub flush: DeviceFlushFn,
    pub export: DeviceExportFn,
    pub resume: DeviceResumeFn,
    pub hyperv: HypervExportFn,
}

pub fn run_source<S: Transport>(
    stream: S,
    hdl: Arc<VmmHdl>,
    memctx: &MemCtx,
    config: SourceConfig,
    hooks: SourceHooks,
    status: Arc<Mutex<MigrationStatus>>,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    let mut chan = Chan::new(
        stream,
        tungstenite::protocol::Role::Client,
        config.deadlines,
    )?;

    let resume = hooks.resume;
    let result = run_inner(
        &mut chan,
        &hdl,
        memctx,
        &config,
        hooks.pre_pause,
        hooks.flush,
        hooks.export,
        hooks.hyperv,
        &status,
        log,
    );

    let phase = status
        .lock()
        .map(|s| s.phase)
        .unwrap_or(MigrationPhase::Error);
    finish_source(result, phase, log, resume, || hdl.resume())
}

/// Roll the source back when a migration failed after the pause.
fn finish_source<E, F>(
    result: Result<(), MigrateError>,
    phase: MigrationPhase,
    log: &slog::Logger,
    device_resume_fn: DeviceResumeFn,
    resume_vm: F,
) -> Result<(), MigrateError>
where
    E: std::fmt::Display,
    F: FnOnce() -> Result<(), E>,
{
    // A failure before the pause changes nothing. A failure at or after
    // the commit cannot be undone: the destination may run the guest.
    // The source stays paused and an operator decides.
    let rollback = (MigrationPhase::Pause as u8
        ..MigrationPhase::Committed as u8)
        .contains(&(phase as u8));
    if result.is_err() && rollback {
        // Device workers must be available before vCPUs can issue more I/O.
        device_resume_fn();
        if let Err(error) = resume_vm() {
            slog::warn!(log, "failed to resume source VM after migration abort";
                "error" => %error);
        }
    }

    result
}

#[allow(clippy::too_many_arguments)]
fn run_inner<S: Transport>(
    chan: &mut Chan<S>,
    hdl: &Arc<VmmHdl>,
    memctx: &MemCtx,
    config: &SourceConfig,
    device_pre_pause_fn: DevicePrePauseFn,
    device_flush_fn: DeviceFlushFn,
    device_export_fn: DeviceExportFn,
    hyperv_export_fn: HypervExportFn,
    status: &Arc<Mutex<MigrationStatus>>,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    // ── Sync ──
    set_phase(status, MigrationPhase::Sync);

    chan.send(&Message::serialized(&ProtocolOffer {
        protocols: vec![PROTOCOL_RON.to_string()],
    })?)?;

    let sel: ProtocolSelect = chan.expect_serialized()?;
    if sel.protocol != PROTOCOL_RON {
        return Err(MigrateError::ProtocolMismatch(crate::wire::peer_text(
            &sel.protocol,
        )));
    }

    let mut devices = config.devices.clone();
    devices.sort();
    chan.send(&Message::serialized(&MigrationPreamble {
        num_cpus: config.num_cpus,
        mem_size: config.mem_size,
        devices,
        cpu_features: Some(cpu::local_features(config.cpu_baseline)),
    })?)?;
    chan.expect_okay()?;

    // ── RamPushPrePause: send ALL pages ──
    set_phase(status, MigrationPhase::RamPushPrePause);
    push_ram_all(chan, hdl, memctx, status)?;

    // ── Iterative convergence ──
    const MAX_ITERS: u32 = 5;
    const CONVERGE_THRESHOLD: u64 = 1024;

    for iter in 1..=MAX_ITERS {
        std::thread::sleep(Duration::from_millis(500));
        let dirty = push_ram(chan, hdl, memctx, status)?;
        slog::info!(log, "convergence"; "iter" => iter, "dirty" => dirty);
        if dirty <= CONVERGE_THRESHOLD {
            break;
        }
    }

    chan.send(&Message::PauseSignal)?;

    // ── Pause ──
    // From here every failure must roll back, so record the phase
    // before the first step that can fail.
    set_phase(status, MigrationPhase::Pause);
    chan.enter_post_pause()?;
    device_pre_pause_fn()
        .map_err(|e| MigrateError::State(format!("device pause: {e}")))?;
    hdl.pause()
        .map_err(|e| MigrateError::Io(format!("pause: {e}")))?;

    // With zvol WCE=1, writes that the guest saw complete can still be
    // in cache. `zfs send` ships only committed pool state.
    slog::info!(log, "flushing device backing caches");
    device_flush_fn().map_err(|error| {
        MigrateError::Io(format!("device flush failed: {error}"))
    })?;

    if let Some(barrier) = config.zfs_barrier.as_deref() {
        zfs_barrier_wait(barrier, config.deadlines.zfs_barrier, log)?;
    }

    // Remove the overlay pages before the final pass. The destination
    // reinstalls them from the MSR values, and the RAM image must hold
    // the guest's own bytes at those addresses.
    let hyperv = hyperv_export_fn();

    // ── Post-pause dirty ──
    // Every writer stopped in the pre-pause step, so this pass is final.
    set_phase(status, MigrationPhase::RamPushPostPause);
    let final_dirty = push_ram(chan, hdl, memctx, status)?;
    slog::info!(log, "post-pause"; "dirty" => final_dirty);

    // ── TimeData ──
    set_phase(status, MigrationPhase::TimeData);
    let time_data = state::export_time_data(hdl, log)?;
    chan.send(&Message::serialized(&time_data)?)?;

    // ── DeviceState ──
    set_phase(status, MigrationPhase::DeviceState);
    let mut dev_state = state::export_device_state(hdl, config.num_cpus, log)?;
    dev_state.emulated = device_export_fn()
        .map_err(|e| MigrateError::State(format!("device export: {e}")))?;
    dev_state.hyperv = hyperv;
    slog::info!(log, "exporting device state";
        "devices" => dev_state.emulated.len());
    chan.send(&Message::serialized(&dev_state)?)?;

    // ── RamPull (none needed) ──
    set_phase(status, MigrationPhase::RamPull);
    chan.send(&Message::MemEnd)?;

    // ── Finish ──
    // The order of these three steps is the invariant:
    // 1. The destination reports that it holds all state.
    // 2. The source commits. It never runs the guest again.
    // 3. The destination starts the guest.
    // If the destination resumes before the commit and the source rolls
    // back, two copies of the guest run against the same disk.
    set_phase(status, MigrationPhase::Finish);
    chan.expect_okay()?;
    chan.send(&Message::Okay)?;

    set_phase(status, MigrationPhase::Committed);
    chan.expect_okay().map_err(|e| {
        MigrateError::Committed(format!(
            "the destination never confirmed it started the guest: {e}"
        ))
    })?;

    chan.close();
    Ok(())
}

/// Wait for the GZ agent to finish the final ZFS incremental.
///
/// The connect tells the agent that the vCPUs are paused. The byte that
/// comes back is the agent's result. Every failure fails the migration:
/// a destination disk older than guest memory lacks writes that the
/// guest saw complete.
fn zfs_barrier_wait(
    barrier_path: &Path,
    budget: Duration,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    slog::info!(log, "waiting for post-pause ZFS sync";
        "barrier" => %barrier_path.display());
    let mut stream = std::os::unix::net::UnixStream::connect(barrier_path)
        .map_err(|e| {
            MigrateError::Io(format!("ZFS barrier connect failed: {e}"))
        })?;
    stream.set_read_timeout(Some(budget)).map_err(|e| {
        MigrateError::Io(format!("ZFS barrier read timeout: {e}"))
    })?;
    let mut status = [0u8; 1];
    stream.read_exact(&mut status).map_err(|e| {
        MigrateError::Io(format!("ZFS barrier reported nothing: {e}"))
    })?;
    if status[0] != ZFS_BARRIER_OK {
        return Err(MigrateError::Io(format!(
            "ZFS barrier reported failure (status {})",
            status[0],
        )));
    }
    slog::info!(log, "post-pause ZFS sync complete");
    Ok(())
}

fn push_ram_all<S: Transport>(
    chan: &mut Chan<S>,
    hdl: &Arc<VmmHdl>,
    memctx: &MemCtx,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    for &(gpa, len, kind) in &memctx.regions() {
        if kind != RegionKind::Ram {
            continue;
        }
        let bm_len = crate::protocol::bitmap_size(len);
        let mut bm = vec![0u8; bm_len];
        // Fail here, not on the next pass: broken tracking otherwise
        // costs a full RAM copy before it shows.
        hdl.track_dirty_pages(gpa, len, &mut bm).map_err(|e| {
            MigrateError::Io(format!("dirty tracking unavailable: {e}"))
        })?;

        let num_pages = len / PAGE_SIZE;
        let mut off = 0;
        while off < num_pages {
            let count = BATCH_SIZE.min(num_pages - off);
            let base = gpa + (off * PAGE_SIZE) as u64;
            send_batch(chan, memctx, base, count, status)?;
            off += count;
        }
    }
    chan.send(&Message::MemEnd)?;
    chan.expect_mem_done()
}

fn push_ram<S: Transport>(
    chan: &mut Chan<S>,
    hdl: &VmmHdl,
    memctx: &MemCtx,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<u64, MigrateError> {
    let mut total: u64 = 0;
    for &(gpa, len, kind) in &memctx.regions() {
        if kind != RegionKind::Ram {
            continue;
        }
        let bm_len = crate::protocol::bitmap_size(len);
        let mut bm = vec![0u8; bm_len];
        hdl.track_dirty_pages(gpa, len, &mut bm)
            .map_err(|e| MigrateError::Io(format!("track_dirty: {e}")))?;

        let mut batch: Vec<u64> = Vec::with_capacity(BATCH_SIZE);
        for dirty_gpa in PageIter::new(&bm, gpa, len) {
            batch.push(dirty_gpa);
            total += 1;
            if batch.len() >= BATCH_SIZE {
                send_batch_sparse(chan, memctx, &batch, status)?;
                batch.clear();
            }
        }
        if !batch.is_empty() {
            send_batch_sparse(chan, memctx, &batch, status)?;
        }
    }
    if let Ok(mut s) = status.lock() {
        s.dirty_pages_remaining = total;
    }
    chan.send(&Message::MemEnd)?;
    chan.expect_mem_done()?;
    Ok(total)
}

fn send_batch<S: Transport>(
    chan: &mut Chan<S>,
    memctx: &MemCtx,
    base_gpa: u64,
    count: usize,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    let raw_size = page_buffer_len(count)?;
    let gpas = build_contiguous_gpas(base_gpa, count)?;
    let mut buf = vec![0u8; raw_size];
    read_pages(memctx, &gpas, &mut buf)?;
    let (data, flags) = compress(&buf);
    chan.send(&Message::PageBatch {
        base_gpa,
        page_count: page_count(count)?,
        flags,
        data,
    })?;
    if let Ok(mut s) = status.lock() {
        s.pages_transferred += count as u64;
        s.bytes_transferred += raw_size as u64;
    }
    Ok(())
}

fn send_batch_sparse<S: Transport>(
    chan: &mut Chan<S>,
    memctx: &MemCtx,
    gpas: &[u64],
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    if gpas.is_empty() {
        return Ok(());
    }
    let base = gpas[0];
    if are_contiguous(gpas) {
        return send_batch(chan, memctx, base, gpas.len(), status);
    }
    let raw_size = page_buffer_len(gpas.len())?;
    let count = gpas.len();
    let mut buf = vec![0u8; raw_size];
    read_pages(memctx, gpas, &mut buf)?;
    let (data, flags) = compress(&buf);
    chan.send(&Message::MemFetch(gpas.to_vec()))?;
    chan.send(&Message::PageBatch {
        base_gpa: base,
        page_count: page_count(count)?,
        flags,
        data,
    })?;
    if let Ok(mut s) = status.lock() {
        s.pages_transferred += count as u64;
        s.bytes_transferred += raw_size as u64;
    }
    Ok(())
}

fn page_count(count: usize) -> Result<u32, MigrateError> {
    u32::try_from(count).map_err(|_| {
        MigrateError::Codec(format!("invalid page count: {count}"))
    })
}

fn compress(raw: &[u8]) -> (Vec<u8>, u32) {
    match zstd::bulk::compress(raw, ZSTD_LEVEL) {
        Ok(c) if c.len() < raw.len() => (c, PAGE_BATCH_FLAG_ZSTD),
        _ => (raw.to_vec(), 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn discard() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    /// A barrier socket that answers with `reply`, or with nothing when
    /// `reply` is `None`.
    fn barrier(reply: Option<u8>) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("barrier.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                if let Some(byte) = reply {
                    let _ = stream.write_all(&[byte]);
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        (dir, path)
    }

    #[test]
    fn a_reported_zfs_failure_fails_the_migration() {
        // Otherwise the destination disk lacks writes that the guest
        // saw complete.
        let (_dir, path) = barrier(Some(1));
        let error = zfs_barrier_wait(&path, Duration::from_secs(5), &discard())
            .expect_err("a non-zero status must fail");
        assert!(error.to_string().contains("reported failure"), "{error}");
    }

    #[test]
    fn a_silent_zfs_barrier_fails_the_migration() {
        let (_dir, path) = barrier(None);
        let error =
            zfs_barrier_wait(&path, Duration::from_millis(200), &discard())
                .expect_err("a timeout must fail");
        assert!(error.to_string().contains("reported nothing"), "{error}");
    }

    #[test]
    fn a_missing_zfs_barrier_fails_the_migration() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = zfs_barrier_wait(
            &dir.path().join("absent.sock"),
            Duration::from_secs(1),
            &discard(),
        )
        .expect_err("a missing socket must fail");
        assert!(error.to_string().contains("connect failed"), "{error}");
    }

    #[test]
    fn an_agreeing_zfs_barrier_lets_the_migration_go_on() {
        let (_dir, path) = barrier(Some(ZFS_BARRIER_OK));
        zfs_barrier_wait(&path, Duration::from_secs(5), &discard())
            .expect("status 0 is the go-ahead");
    }

    #[test]
    fn flush_failure_resumes_devices_and_vcpus() {
        let result: Result<(), MigrateError> =
            Err(MigrateError::Io("device flush failed".into()));
        let devices_resumed = Arc::new(AtomicBool::new(false));
        let vcpus_resumed = Arc::new(AtomicBool::new(false));
        let devices_resumed_by_callback = Arc::clone(&devices_resumed);
        let vcpus_resumed_by_callback = Arc::clone(&vcpus_resumed);
        let devices_resumed_before_vcpus = Arc::clone(&devices_resumed);
        let resume_fn: DeviceResumeFn = Box::new(move || {
            devices_resumed_by_callback.store(true, Ordering::Release);
        });

        let result = finish_source(
            result,
            MigrationPhase::Pause,
            &discard(),
            resume_fn,
            move || {
                assert!(devices_resumed_before_vcpus.load(Ordering::Acquire));
                vcpus_resumed_by_callback.store(true, Ordering::Release);
                Ok::<(), &'static str>(())
            },
        );

        assert!(result.is_err());
        assert!(devices_resumed.load(Ordering::Acquire));
        assert!(vcpus_resumed.load(Ordering::Acquire));
    }

    #[test]
    fn a_committed_source_is_never_resumed() {
        // The destination may run the guest. Two copies against one
        // disk are worse than a guest that needs an operator.
        let resumed = Arc::new(AtomicBool::new(false));
        let by_callback = Arc::clone(&resumed);
        let resume_fn: DeviceResumeFn = Box::new(move || {
            by_callback.store(true, Ordering::Release);
        });
        let result = finish_source(
            Err(MigrateError::Committed("no confirmation".into())),
            MigrationPhase::Committed,
            &discard(),
            resume_fn,
            || -> Result<(), &'static str> {
                panic!("a committed source must not resume its vCPUs")
            },
        );
        assert!(result.is_err());
        assert!(!resumed.load(Ordering::Acquire));
    }

    #[test]
    fn a_failure_in_finish_still_rolls_back() {
        // Before the commit, the destination does not start the guest.
        let resumed = Arc::new(AtomicBool::new(false));
        let by_callback = Arc::clone(&resumed);
        let resume_fn: DeviceResumeFn = Box::new(move || {
            by_callback.store(true, Ordering::Release);
        });
        let result = finish_source(
            Err(MigrateError::WebSocket("closed".into())),
            MigrationPhase::Finish,
            &discard(),
            resume_fn,
            || Ok::<(), &'static str>(()),
        );
        assert!(result.is_err());
        assert!(resumed.load(Ordering::Acquire));
    }

    #[test]
    fn a_pre_pause_failure_does_not_resume_a_running_guest() {
        // Nothing is paused yet, so a resume changes the state of a VM
        // that never stopped.
        let resumed = Arc::new(AtomicBool::new(false));
        let by_callback = Arc::clone(&resumed);
        let resume_fn: DeviceResumeFn = Box::new(move || {
            by_callback.store(true, Ordering::Release);
        });
        let result = finish_source(
            Err(MigrateError::Io("connect failed".into())),
            MigrationPhase::Sync,
            &discard(),
            resume_fn,
            || Ok::<(), &'static str>(()),
        );
        assert!(result.is_err());
        assert!(!resumed.load(Ordering::Acquire));
    }
}
