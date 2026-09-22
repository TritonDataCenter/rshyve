// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The migration destination.
//!
//! Every check occurs before the guest runs: topology and CPU features
//! in the preamble, then the device payload's identity set against the
//! local device set. The guest resumes only when every device holds its
//! own state.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use vmm_core::cpuid::CpuBaseline;
use vmm_core::hdl::VmmHdl;
use vmm_core::mem::MemCtx;
use vmm_devices::lifecycle::{
    DeviceMigrateState, DeviceStateError, HypervMigrateState, WireBdf,
};

use crate::codec::{
    DeviceIdentity, DeviceState, Message, MigrateError, MigrationPreamble,
    ProtocolOffer, ProtocolSelect, TimeData,
};
use crate::cpu;
use crate::pages::apply_page_batch;
use crate::protocol::PROTOCOL_RON;
use crate::state;
use crate::wire::{set_phase, unexpected, Chan, Deadlines, Transport};
use crate::MigrationPhase;
use crate::MigrationStatus;

/// Give one device the state that its counterpart on the source
/// exported.
///
/// Called once per payload entry. The entry is already matched to a
/// local device by PCI address.
pub type DeviceRestoreFn = Box<
    dyn Fn(WireBdf, &DeviceMigrateState) -> Result<(), DeviceStateError> + Send,
>;

/// Start the device workers after a successful import.
pub type DeviceResumeFn = Box<dyn Fn() + Send>;

/// Restore the Hyper-V enlightenment and reinstall both overlay pages
/// from the source's MSR values.
///
/// Returns false if this VM has no enlightenment. The import then
/// refuses the payload instead of dropping the state.
pub type HypervRestoreFn = Box<dyn Fn(&HypervMigrateState) -> bool + Send>;

/// Destination settings, other than the stream.
pub struct DestConfig {
    pub num_cpus: u32,
    pub mem_size: u64,
    pub cpu_baseline: CpuBaseline,
    /// Every local device that carries migration state. The payload's
    /// identity set must equal this.
    pub devices: Vec<DeviceIdentity>,
    pub deadlines: Deadlines,
    /// Accept a source whose guest saw CPU features this host lacks.
    ///
    /// The guest may already use one of those instructions, and takes
    /// #UD on the next use. Each use of this override is logged.
    pub allow_cpu_feature_mismatch: bool,
}

/// Device hooks that the import calls.
pub struct DestHooks {
    pub restore: DeviceRestoreFn,
    pub resume: DeviceResumeFn,
    pub hyperv: HypervRestoreFn,
}

pub fn run_destination<S: Transport>(
    stream: S,
    hdl: Arc<VmmHdl>,
    memctx: &MemCtx,
    config: DestConfig,
    hooks: DestHooks,
    status: Arc<Mutex<MigrationStatus>>,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    let mut chan = Chan::new(
        stream,
        tungstenite::protocol::Role::Server,
        config.deadlines,
    )?;
    run_inner(&mut chan, &hdl, memctx, &config, &hooks, &status, log)
}

fn run_inner<S: Transport>(
    chan: &mut Chan<S>,
    hdl: &Arc<VmmHdl>,
    memctx: &MemCtx,
    config: &DestConfig,
    hooks: &DestHooks,
    status: &Arc<Mutex<MigrationStatus>>,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    // ── Sync ──
    set_phase(status, MigrationPhase::Sync);

    let offer: ProtocolOffer = chan.expect_serialized()?;
    if !offer.protocols.iter().any(|p| p == PROTOCOL_RON) {
        let err = MigrateError::ProtocolMismatch(crate::wire::peer_text(
            &offer.protocols.join(","),
        ));
        chan.send(&Message::Error(err.clone()))?;
        return Err(err);
    }
    chan.send(&Message::serialized(&ProtocolSelect {
        protocol: PROTOCOL_RON.to_string(),
    })?)?;

    let preamble: MigrationPreamble = chan.expect_serialized()?;
    if let Err(err) = check_preamble(&preamble, config, log) {
        chan.report_failure(&err, log);
        return Err(err);
    }
    chan.send(&Message::Okay)?;

    // ── Pre-pause RAM passes ──
    set_phase(status, MigrationPhase::RamPushPrePause);
    receive_pass(chan, memctx, status, PassEnd::PauseSignal)?;

    // ── Post-pause final pass ──
    set_phase(status, MigrationPhase::RamPushPostPause);
    chan.enter_post_pause()?;
    receive_pass(chan, memctx, status, PassEnd::MemEnd)?;

    slog::info!(log, "pausing VM before state import");
    hdl.pause()
        .map_err(|e| MigrateError::Io(format!("pause: {e}")))?;

    // ── TimeData ──
    set_phase(status, MigrationPhase::TimeData);
    let time_data: TimeData = chan.expect_serialized()?;
    if let Err(e) = state::import_time_data(hdl, &time_data, log) {
        chan.report_failure(&e, log);
        return Err(e);
    }

    // ── DeviceState ──
    set_phase(status, MigrationPhase::DeviceState);
    let dev_state: DeviceState = chan.expect_serialized()?;
    if let Err(e) = restore_devices(&dev_state, config, hooks, log) {
        chan.report_failure(&e, log);
        return Err(e);
    }
    if let Err(e) =
        state::import_device_state(hdl, &dev_state, config.num_cpus, log)
    {
        chan.report_failure(&e, log);
        return Err(e);
    }

    // ── RamPull ──
    set_phase(status, MigrationPhase::RamPull);
    match chan.recv()? {
        Message::MemEnd => {}
        other => return Err(unexpected("MemEnd", &other)),
    }

    // ── Finish ──
    // Nothing starts until the source commits never to run the guest
    // again. If the destination resumes before the commit and the source
    // rolls back, two copies of the guest run against the same disk.
    set_phase(status, MigrationPhase::Finish);
    chan.send(&Message::Okay)?;
    chan.expect_okay()?;

    set_phase(status, MigrationPhase::Committed);
    // The workers are not started on this VM yet, and the guest posts
    // I/O to all of them when it resumes.
    (hooks.resume)();
    slog::info!(log, "migration import complete, starting vCPUs");
    hdl.resume()
        .map_err(|e| MigrateError::Io(format!("resume: {e}")))?;

    // The source stays paused until this message arrives. It lets the
    // finished source exit.
    chan.send(&Message::Okay)?;
    chan.close();
    Ok(())
}

/// Refuse a source that this VM cannot replace, before it accepts a
/// page.
fn check_preamble(
    preamble: &MigrationPreamble,
    config: &DestConfig,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    // Both sides report their boot topology, so CPUs or memory that the
    // source hot-added do not show here. Only the source can see that
    // growth, and it refuses to start the migration.
    if preamble.num_cpus != config.num_cpus {
        return Err(MigrateError::PreambleMismatch(format!(
            "CPU count mismatch: source has {}, we have {}",
            preamble.num_cpus, config.num_cpus,
        )));
    }
    if preamble.mem_size != config.mem_size {
        return Err(MigrateError::PreambleMismatch(format!(
            "memory size mismatch: source has {} bytes, we have {}",
            preamble.mem_size, config.mem_size,
        )));
    }
    check_topology(&preamble.devices, &config.devices)?;

    let Some(ref src) = preamble.cpu_features else {
        return Err(MigrateError::PreambleMismatch(
            "source sent no CPU feature table".into(),
        ));
    };
    let gap = cpu::missing(src, &cpu::local_features(config.cpu_baseline));
    if gap.is_empty() {
        slog::info!(log, "CPU feature check passed");
        return Ok(());
    }
    if config.allow_cpu_feature_mismatch {
        slog::warn!(log, "accepting a CPU feature mismatch by request; \
            the guest takes #UD on an instruction this host lacks";
            "missing" => %gap);
        return Ok(());
    }
    Err(MigrateError::PreambleMismatch(format!(
        "this host lacks CPU features the guest already sees ({gap}); \
         send migrate-dest with allow_cpu_feature_mismatch to accept \
         the risk",
    )))
}

/// The source's device set must equal this VM's device set.
///
/// Otherwise a destination with a missing disk resumes the guest with
/// that disk at reset, and no error shows.
fn check_topology(
    source: &[DeviceIdentity],
    local: &[DeviceIdentity],
) -> Result<(), MigrateError> {
    let named = |set: &[DeviceIdentity]| -> Vec<String> {
        let mut v: Vec<String> = set.iter().map(|d| d.to_string()).collect();
        v.sort();
        v
    };
    let src = named(source);
    let dst = named(local);
    if src == dst {
        return Ok(());
    }
    let only_src: Vec<&String> =
        src.iter().filter(|d| !dst.contains(d)).collect();
    let only_dst: Vec<&String> =
        dst.iter().filter(|d| !src.contains(d)).collect();
    Err(MigrateError::PreambleMismatch(format!(
        "device set mismatch: only on the source {only_src:?}, \
         only here {only_dst:?}",
    )))
}

/// Give every device the state that its counterpart exported, matched
/// by PCI address alone.
fn restore_devices(
    dev_state: &DeviceState,
    config: &DestConfig,
    hooks: &DestHooks,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    let expected: BTreeMap<WireBdf, &str> = config
        .devices
        .iter()
        .map(|d| (d.bdf, d.kind.as_str()))
        .collect();
    let mut seen: BTreeMap<WireBdf, ()> = BTreeMap::new();
    for entry in &dev_state.emulated {
        if !expected.contains_key(&entry.bdf) {
            return Err(MigrateError::State(format!(
                "payload carries state for {}, which this VM has no device \
                 at",
                entry.bdf,
            )));
        }
        if seen.insert(entry.bdf, ()).is_some() {
            return Err(MigrateError::State(format!(
                "payload carries two states for {}",
                entry.bdf,
            )));
        }
    }
    // A local device with no payload entry resumes at reset, while the
    // guest driver thinks it is configured.
    if let Some(bdf) = expected.keys().find(|bdf| !seen.contains_key(bdf)) {
        return Err(MigrateError::State(format!(
            "payload carries no entry for {} ({})",
            bdf, expected[bdf],
        )));
    }

    if let Some(hyperv) = &dev_state.hyperv {
        if !(hooks.hyperv)(hyperv) {
            return Err(MigrateError::State(
                "payload carries Hyper-V enlightenment state, and this VM \
                 was not started with --hyperv"
                    .into(),
            ));
        }
    }

    slog::info!(log, "restoring device state";
        "devices" => dev_state.emulated.len());
    for entry in &dev_state.emulated {
        let Some(ref state) = entry.state else {
            continue;
        };
        (hooks.restore)(entry.bdf, state).map_err(|error| {
            MigrateError::State(format!("{}: {error}", entry.bdf))
        })?;
    }
    Ok(())
}

/// The message that ends a RAM pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PassEnd {
    /// Receive passes until the source reports that it paused.
    PauseSignal,
    /// Receive exactly one pass: the final one.
    MemEnd,
}

/// Receive RAM passes until `until` arrives.
///
/// After a `MemFetch` the peer must send exactly one `PageBatch`. Only
/// the `MemFetch` GPA list tells where those pages go, so any other
/// message after it is an error.
fn receive_pass<S: Transport>(
    chan: &mut Chan<S>,
    memctx: &MemCtx,
    status: &Arc<Mutex<MigrationStatus>>,
    until: PassEnd,
) -> Result<(), MigrateError> {
    let mut sparse: Option<Vec<u64>> = None;
    loop {
        let msg = chan.recv()?;
        match (msg, sparse.take()) {
            (
                Message::PageBatch {
                    base_gpa,
                    page_count,
                    flags,
                    data,
                },
                gpas,
            ) => {
                apply_page_batch(
                    memctx,
                    base_gpa,
                    page_count,
                    flags,
                    &data,
                    gpas.as_deref(),
                    status,
                )?;
            }
            (Message::MemFetch(gpas), None) => sparse = Some(gpas),
            (Message::MemEnd, None) => {
                chan.send(&Message::MemDone)?;
                if until == PassEnd::MemEnd {
                    return Ok(());
                }
            }
            (Message::PauseSignal, None) if until == PassEnd::PauseSignal => {
                return Ok(());
            }
            (other, Some(_)) => {
                return Err(unexpected("PageBatch following MemFetch", &other));
            }
            (other, None) => {
                return Err(unexpected(
                    "PageBatch/MemFetch/MemEnd/PauseSignal",
                    &other,
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(dev: u8, kind: &str) -> DeviceIdentity {
        DeviceIdentity {
            bdf: WireBdf {
                bus: 0,
                dev,
                func: 0,
            },
            kind: kind.to_string(),
        }
    }

    #[test]
    fn a_matching_device_set_is_accepted() {
        let set = vec![ident(4, "nvme"), ident(5, "virtio-blk")];
        check_topology(&set, &set).expect("same set");
    }

    #[test]
    fn set_order_does_not_matter() {
        let src = vec![ident(5, "virtio-blk"), ident(4, "nvme")];
        let dst = vec![ident(4, "nvme"), ident(5, "virtio-blk")];
        check_topology(&src, &dst).expect("same set, other order");
    }

    #[test]
    fn a_destination_started_with_one_disk_fewer_is_refused() {
        // The guest would otherwise arrive with that disk at reset and
        // no error anywhere.
        let src = vec![ident(4, "nvme"), ident(5, "virtio-blk")];
        let dst = vec![ident(4, "nvme")];
        let error = check_topology(&src, &dst).expect_err("must refuse");
        assert!(error.to_string().contains("virtio-blk@0.5.0"), "{error}");
    }

    #[test]
    fn a_device_of_another_kind_at_the_same_address_is_refused() {
        let src = vec![ident(4, "nvme")];
        let dst = vec![ident(4, "virtio-blk")];
        check_topology(&src, &dst).expect_err("kind is part of identity");
    }
}

#[cfg(test)]
mod pass_tests {
    use super::*;
    use crate::codec::PAGE_BATCH_FLAG_ZSTD;
    use crate::protocol::PAGE_SIZE;
    use crate::wire::Deadlines;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    use vmm_core::mem::PhysMap;

    const RAM: usize = 64 * 1024;

    fn memctx() -> MemCtx {
        MemCtx::new(Arc::new(
            PhysMap::new_anon(0, RAM).expect("anonymous guest memory"),
        ))
    }

    fn status() -> Arc<Mutex<MigrationStatus>> {
        Arc::new(Mutex::new(MigrationStatus::default()))
    }

    /// A connected pair of channels, source end first.
    fn pair(deadlines: Deadlines) -> (Chan<UnixStream>, Chan<UnixStream>) {
        let (a, b) = UnixStream::pair().expect("socket pair");
        (
            Chan::new(a, tungstenite::protocol::Role::Client, deadlines)
                .expect("source channel"),
            Chan::new(b, tungstenite::protocol::Role::Server, deadlines)
                .expect("destination channel"),
        )
    }

    fn page(fill: u8) -> Vec<u8> {
        vec![fill; PAGE_SIZE]
    }

    fn read_page(memctx: &MemCtx, gpa: u64) -> Vec<u8> {
        let mut out = vec![0u8; PAGE_SIZE];
        crate::pages::read_pages(memctx, &[gpa], &mut out).expect("read back");
        out
    }

    #[test]
    fn a_whole_pass_lands_every_page_it_carried() {
        // Covers the full pass, so a dropped sparse batch shows.
        let (mut source, mut dest) = pair(Deadlines::default());
        let sender = std::thread::spawn(move || {
            // Contiguous.
            source
                .send(&Message::PageBatch {
                    base_gpa: 0,
                    page_count: 2,
                    flags: 0,
                    data: [page(0xAA), page(0xBB)].concat(),
                })
                .expect("contiguous batch");
            // Sparse: only the GPA list tells where these pages go.
            let gpas = vec![0x8000u64, 0xC000];
            source.send(&Message::MemFetch(gpas)).expect("fetch");
            source
                .send(&Message::PageBatch {
                    base_gpa: 0x8000,
                    page_count: 2,
                    flags: 0,
                    data: [page(0xCC), page(0xDD)].concat(),
                })
                .expect("sparse batch");
            source.send(&Message::MemEnd).expect("end of pass");
            source.expect_mem_done().expect("the pass is acknowledged");
            source.send(&Message::PauseSignal).expect("pause");
            source
        });
        let memctx = memctx();
        receive_pass(&mut dest, &memctx, &status(), PassEnd::PauseSignal)
            .expect("a well-formed pass");
        sender.join().expect("the sender finished");

        assert_eq!(read_page(&memctx, 0), page(0xAA));
        assert_eq!(read_page(&memctx, PAGE_SIZE as u64), page(0xBB));
        assert_eq!(read_page(&memctx, 0x8000), page(0xCC));
        assert_eq!(read_page(&memctx, 0xC000), page(0xDD));
    }

    #[test]
    fn a_compressed_batch_lands_the_same_bytes() {
        let (mut source, mut dest) = pair(Deadlines::default());
        let raw = [page(0x11), page(0x22)].concat();
        let data = zstd::bulk::compress(&raw, 1).expect("compress");
        let sender = std::thread::spawn(move || {
            source
                .send(&Message::PageBatch {
                    base_gpa: 0,
                    page_count: 2,
                    flags: PAGE_BATCH_FLAG_ZSTD,
                    data,
                })
                .expect("batch");
            source.send(&Message::MemEnd).expect("end");
            source
        });
        let memctx = memctx();
        receive_pass(&mut dest, &memctx, &status(), PassEnd::MemEnd)
            .expect("a compressed pass");
        sender.join().expect("the sender finished");
        assert_eq!(read_page(&memctx, 0), page(0x11));
    }

    #[test]
    fn anything_but_a_batch_after_a_fetch_is_refused() {
        // After a MemFetch the peer must send exactly one PageBatch.
        // Any other message drops the pages for those GPAs.
        let (mut source, mut dest) = pair(Deadlines::default());
        let sender = std::thread::spawn(move || {
            source
                .send(&Message::MemFetch(vec![0x8000]))
                .expect("fetch");
            source.send(&Message::MemEnd).expect("not a batch");
            source
        });
        let error =
            receive_pass(&mut dest, &memctx(), &status(), PassEnd::MemEnd)
                .expect_err("a fetch without its batch");
        assert!(
            error.to_string().contains("PageBatch following MemFetch"),
            "{error}",
        );
        sender.join().expect("the sender finished");
    }

    #[test]
    fn a_pause_signal_does_not_end_the_final_pass() {
        // The final pass ends at MemEnd. A PauseSignal there is out of
        // step, and accepting it loses the last dirty pages.
        let (mut source, mut dest) = pair(Deadlines::default());
        let sender = std::thread::spawn(move || {
            source.send(&Message::PauseSignal).expect("pause");
            source
        });
        receive_pass(&mut dest, &memctx(), &status(), PassEnd::MemEnd)
            .expect_err("a pause in the final pass");
        sender.join().expect("the sender finished");
    }

    #[test]
    fn a_silent_peer_does_not_hold_the_destination_for_ever() {
        // After the pause every second of waiting is guest downtime.
        let deadlines = Deadlines {
            pre_pause: Duration::from_millis(200),
            post_pause: Duration::from_millis(200),
            ..Deadlines::default()
        };
        let (source, mut dest) = pair(deadlines);
        let started = std::time::Instant::now();
        receive_pass(&mut dest, &memctx(), &status(), PassEnd::MemEnd)
            .expect_err("a peer that says nothing");
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(source);
    }
}
