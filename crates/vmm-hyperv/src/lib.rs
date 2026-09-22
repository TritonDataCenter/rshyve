// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Microsoft Hyper-V enlightenment ("Tier 1").
//!
//! Implements the subset of the Hyper-V Top-Level Functional
//! Specification (TLFS v6.0b) that Windows guests need:
//!
//!   * Hypervisor identification CPUID leaves 0x4000_0000 to 0x4000_0006
//!   * `relaxed timing` recommendation (suppresses CLOCK_WATCHDOG bugcheck)
//!   * `spinlock retries = 0xFFFFFFFF` (suppresses spinlock notifications)
//!   * `HV_X64_MSR_GUEST_OS_ID` / `HV_X64_MSR_HYPERCALL` (with overlay page)
//!   * `HV_X64_MSR_VP_INDEX` (per-vCPU readback)
//!   * `HV_X64_MSR_TIME_REF_COUNT` (monotonic 100 ns counter since VM boot)
//!   * `HV_X64_MSR_REFERENCE_TSC` (with overlay page, the largest perf gain)
//!   * `HV_X64_MSR_RESET` (writing the reset bit triggers a guest reset)
//!   * Crash dump MSRs `CRASH_P0..P4` + `CRASH_CTL` (the BSOD payload goes
//!     to the log)
//!
//! Synthetic interrupts (synic), synthetic timers (stimer), virtual
//! APIC, TLB-flush hypercalls and IPI hypercalls are not implemented.
//! They need kernel support in vmm.ko.

pub mod bits;
pub mod hypercall;
pub mod overlay;
pub mod tsc;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bhyve_api::vcpu_cpuid_entry;
use slog::{info, warn, Logger};
use vmm_core::mem::PhysMap;
use vmm_core::msr::{MsrHandler, RdmsrOutcome, WrmsrOutcome};
use vmm_core::ratelimit::TokenBucket;
use vmm_devices::lifecycle::HypervMigrateState;

use crate::bits::*;
use crate::hypercall::MsrHypercallValue;
use crate::overlay::{Overlay, PAGE_SIZE};
use crate::tsc::{MsrReferenceTscValue, ReferenceTscPage};

/// Feature gates for the enlightenment.
#[derive(Debug, Clone, Copy)]
pub struct Features {
    /// Advertise and handle `HV_X64_MSR_REFERENCE_TSC` and
    /// `TIME_REF_COUNT`.
    pub reference_tsc: bool,
    pub reset: bool,
    /// Advertise the `frequencies` CPUID leaf (0x4000_0006). Some
    /// Windows guests need it to skip TSC calibration.
    pub frequencies: bool,
    /// Guest TSC frequency in Hz, for the reference-TSC page scale
    /// factor. It must match the frequency the kernel VMM gives the
    /// guest. Ignored if `reference_tsc` is false.
    pub tsc_freq_hz: u64,
}

impl Default for Features {
    fn default() -> Self {
        Self {
            reference_tsc: true,
            reset: true,
            frequencies: true,
            tsc_freq_hz: 0,
        }
    }
}

/// Per-VM Hyper-V state. Shared MSR storage is behind one
/// `Mutex<Inner>`. Windows caches the values, so RDMSRs are rare and
/// one mutex is enough.
pub struct HyperV {
    log: Logger,
    physmap: Arc<PhysMap>,
    features: Features,
    boot: Instant,
    /// Records a guest can earn by writing CRASH_CTL.
    crash_log_budget: TokenBucket,
    /// What `HV_X64_MSR_TIME_REF_COUNT` read when this VM started, in
    /// 100 ns units.
    ///
    /// Zero for a VM that booted here. A migration destination takes
    /// the source's last value, because the guest is told the counter
    /// is monotonic and a fresh process clock runs it backwards.
    ref_count_base: AtomicU64,
    inner: Mutex<Inner>,
}

const CRASH_LOG_BURST: u32 = 4;
const CRASH_LOG_PERIOD: Duration = Duration::from_secs(15);

struct Inner {
    msr_guest_os_id: u64,
    msr_hypercall: MsrHypercallValue,
    msr_reference_tsc: MsrReferenceTscValue,

    hypercall_overlay: Option<Overlay>,
    reference_tsc_overlay: Option<Overlay>,

    crash_p: [u64; 5],
}

impl Inner {
    fn new() -> Self {
        Self {
            msr_guest_os_id: 0,
            msr_hypercall: MsrHypercallValue::default(),
            msr_reference_tsc: MsrReferenceTscValue::default(),
            hypercall_overlay: None,
            reference_tsc_overlay: None,
            crash_p: [0; 5],
        }
    }
}

impl HyperV {
    pub fn new(
        log: Logger,
        physmap: Arc<PhysMap>,
        features: Features,
    ) -> Arc<Self> {
        Arc::new(Self {
            log,
            physmap,
            features,
            boot: Instant::now(),
            crash_log_budget: TokenBucket::new(
                CRASH_LOG_BURST,
                CRASH_LOG_PERIOD,
            ),
            ref_count_base: AtomicU64::new(0),
            inner: Mutex::new(Inner::new()),
        })
    }

    /// Append the Hyper-V CPUID leaves to `entries`.
    ///
    /// The caller must sort again with `vcpu_cpuid_entry::eval_sort`
    /// before it programs the kernel VMM: `set_cpuid` requires sorted
    /// input.
    pub fn add_cpuid(&self, entries: &mut Vec<vcpu_cpuid_entry>) {
        let max_leaf = if self.features.frequencies {
            HYPERV_MAX_CPUID_LEAF
        } else {
            0x4000_0005
        };

        // 0x4000_0000: vendor signature.
        entries.push(cpuid_entry(
            0x4000_0000,
            0,
            max_leaf,
            HV_CPUID_VENDOR_EBX,
            HV_CPUID_VENDOR_ECX,
            HV_CPUID_VENDOR_EDX,
        ));

        // 0x4000_0001: interface signature ("Hv#1").
        entries.push(cpuid_entry(
            0x4000_0001,
            0,
            HV_CPUID_INTERFACE_EAX,
            0,
            0,
            0,
        ));

        // 0x4000_0002: hypervisor version. Zero, so no version
        // semantics must hold across migration.
        entries.push(cpuid_entry(0x4000_0002, 0, 0, 0, 0, 0));

        // 0x4000_0003: per-MSR access privileges.
        let mut leaf3 = HvLeaf3Eax::HYPERCALL | HvLeaf3Eax::VP_INDEX;
        if self.features.reset {
            leaf3 |= HvLeaf3Eax::SYSTEM_RESET;
        }
        if self.features.frequencies {
            leaf3 |= HvLeaf3Eax::FREQUENCIES;
        }
        if self.features.reference_tsc {
            leaf3 |= HvLeaf3Eax::PARTITION_REFERENCE_COUNTER;
            leaf3 |= HvLeaf3Eax::PARTITION_REFERENCE_TSC;
        }
        // EDX: the crash MSRs are always handled, and both Windows and
        // Linux test this bit before they write them.
        let leaf3_edx = HvLeaf3Edx::GUEST_CRASH_MSRS.bits();
        entries.push(cpuid_entry(
            0x4000_0003,
            0,
            leaf3.bits(),
            0,
            0,
            leaf3_edx,
        ));

        // 0x4000_0004: recommended guest behaviors.
        // EAX = RELAXED_TIMING (the watchdog suppression bit).
        // EBX = 0xFFFFFFFF: the guest should never call out to the
        //       hypervisor on spinlock contention.
        let leaf4_eax = HvLeaf4Eax::RELAXED_TIMING.bits();
        entries.push(cpuid_entry(0x4000_0004, 0, leaf4_eax, 0xFFFF_FFFF, 0, 0));

        // 0x4000_0005: implementation limits. All zero (TLFS-allowed).
        entries.push(cpuid_entry(0x4000_0005, 0, 0, 0, 0, 0));

        // 0x4000_0006: hardware features advertised by the host.
        // Without `frequencies`, max_leaf stops at 0x4000_0005 and the
        // leaf is not visible.
        if self.features.frequencies {
            entries.push(cpuid_entry(0x4000_0006, 0, 0, 0, 0, 0));
        }
    }

    fn handle_wr_guest_os_id(&self, inner: &mut Inner, value: u64) {
        inner.msr_guest_os_id = value;
        // TLFS 3.13: writing 0 to GUEST_OS_ID clears the hypercall
        // MSR's Enabled bit, so the overlay goes too.
        if value == 0 {
            inner.msr_hypercall = MsrHypercallValue(
                inner.msr_hypercall.raw() & !MsrHypercallValue::ENABLED,
            );
            if let Some(ov) = inner.hypercall_overlay.take() {
                overlay::remove(&self.physmap, &ov);
            }
        }
    }

    fn handle_wr_hypercall(&self, inner: &mut Inner, value: u64) {
        let old = inner.msr_hypercall;
        let new = old.apply_write(value);
        inner.msr_hypercall = new;

        // The hypercall page cannot become live while GUEST_OS_ID is
        // 0 (TLFS 3.13).
        if inner.msr_guest_os_id == 0 {
            return;
        }

        let want_enabled = new.enabled();
        let new_gpa = new.gpa();
        let payload = hypercall::build_page();

        match (inner.hypercall_overlay.take(), want_enabled) {
            (Some(ov), true) if ov.gpa == new_gpa => {
                inner.hypercall_overlay = Some(ov);
            }
            (Some(ov), true) => {
                match overlay::relocate(&self.physmap, ov, new_gpa, &payload) {
                    Ok(new_ov) => inner.hypercall_overlay = Some(new_ov),
                    Err(e) => warn!(
                        self.log, "could not relocate hypercall overlay";
                        "gpa" => format!("{:#x}", new_gpa),
                        "error" => %e
                    ),
                }
            }
            (Some(ov), false) => {
                overlay::remove(&self.physmap, &ov);
            }
            (None, true) => {
                match overlay::install(&self.physmap, new_gpa, &payload) {
                    Ok(ov) => inner.hypercall_overlay = Some(ov),
                    Err(e) => warn!(
                        self.log, "could not install hypercall overlay";
                        "gpa" => format!("{:#x}", new_gpa),
                        "error" => %e
                    ),
                }
            }
            (None, false) => {}
        }
    }

    fn handle_wr_reference_tsc(&self, inner: &mut Inner, value: u64) {
        let new = MsrReferenceTscValue(value);
        inner.msr_reference_tsc = new;

        let scale = match tsc::compute_scale(self.features.tsc_freq_hz) {
            Some(s) => s,
            None => {
                // Frequency unknown or out of range: publish the page
                // with sequence=0 so the guest falls back. The GPA is
                // still honored, so later writes can succeed.
                warn!(self.log, "reference-TSC freq unusable, publishing invalid page";
                      "freq_hz" => self.features.tsc_freq_hz);
                self.publish_tsc_page(inner, new, 0)
                    .unwrap_or_else(|e| warn!(self.log, "tsc page publish failed"; "error" => %e));
                return;
            }
        };
        if let Err(e) = self.publish_tsc_page(inner, new, scale) {
            warn!(self.log, "tsc page publish failed"; "error" => %e);
        }
    }

    fn publish_tsc_page(
        &self,
        inner: &mut Inner,
        msr: MsrReferenceTscValue,
        scale: u64,
    ) -> Result<(), overlay::OverlayError> {
        let gpa = msr.gpa();
        let page = if scale == 0 {
            // Sequence=0 means "page invalid". The guest falls back.
            ReferenceTscPage {
                sequence: 0,
                reserved: 0,
                scale: 0,
                offset: 0,
            }
        } else {
            ReferenceTscPage {
                sequence: 1,
                reserved: 0,
                scale,
                offset: 0,
            }
        }
        .into_page();

        match (inner.reference_tsc_overlay.take(), msr.enabled()) {
            (Some(ov), true) if ov.gpa == gpa => {
                inner.reference_tsc_overlay = Some(ov);
                if let Some(map) = self.physmap.lookup(gpa, PAGE_SIZE) {
                    let _ = map.write_bytes(page.as_slice());
                }
            }
            (Some(ov), true) => {
                let new_ov = overlay::relocate(&self.physmap, ov, gpa, &page)?;
                inner.reference_tsc_overlay = Some(new_ov);
            }
            (Some(ov), false) => overlay::remove(&self.physmap, &ov),
            (None, true) => {
                let ov = overlay::install(&self.physmap, gpa, &page)?;
                inner.reference_tsc_overlay = Some(ov);
            }
            (None, false) => {}
        }
        Ok(())
    }

    fn log_crash(&self, inner: &Inner) {
        // CRASH_CTL is a guest WRMSR, so the record needs a cap.
        if !self.crash_log_budget.take() {
            return;
        }
        info!(
            self.log,
            "Windows guest BSOD";
            "bugcheck" => format!("{:#x}", inner.crash_p[0]),
            "p1" => format!("{:#x}", inner.crash_p[1]),
            "p2" => format!("{:#x}", inner.crash_p[2]),
            "p3" => format!("{:#x}", inner.crash_p[3]),
            "p4" => format!("{:#x}", inner.crash_p[4]),
        );
    }

    /// The partition reference counter, in 100 ns units.
    ///
    /// Continues from the base a migration import set, so it never
    /// runs backwards for a guest that was told it is monotonic.
    fn time_ref_count(&self) -> u64 {
        let elapsed = u64::try_from(self.boot.elapsed().as_nanos() / 100)
            .unwrap_or(u64::MAX);
        self.ref_count_base
            .load(Ordering::Relaxed)
            .saturating_add(elapsed)
    }

    /// The guest-visible state a migration carries.
    pub fn export_state(&self) -> HypervMigrateState {
        let inner = self.inner.lock().unwrap();
        HypervMigrateState {
            guest_os_id: inner.msr_guest_os_id,
            hypercall: inner.msr_hypercall.raw(),
            reference_tsc: inner.msr_reference_tsc.raw(),
            crash_p: inner.crash_p,
            time_ref_count: self.time_ref_count(),
        }
    }

    /// Put a source's enlightenment back.
    ///
    /// The MSR writes are replayed through the same handlers a guest
    /// goes through, so both overlay pages are reinstalled from the
    /// values rather than copied. GUEST_OS_ID goes first: the
    /// hypercall page cannot become live before it (TLFS 3.13).
    pub fn import_state(&self, state: &HypervMigrateState) {
        self.ref_count_base
            .store(state.time_ref_count, Ordering::Relaxed);
        let mut inner = self.inner.lock().unwrap();
        inner.crash_p = state.crash_p;
        self.handle_wr_guest_os_id(&mut inner, state.guest_os_id);
        self.handle_wr_hypercall(&mut inner, state.hypercall);
        if self.features.reference_tsc {
            self.handle_wr_reference_tsc(&mut inner, state.reference_tsc);
        }
    }

    /// Drop overlay pages and restore the original guest contents.
    ///
    /// The migration source calls this before the final RAM pass, so
    /// the destination receives the guest's own bytes, not the overlay
    /// payloads. The destination reinstalls both overlays from the MSR
    /// values in [`Self::import_state`].
    pub fn pause_overlays(&self) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(ov) = inner.hypercall_overlay.take() {
            overlay::remove(&self.physmap, &ov);
        }
        if let Some(ov) = inner.reference_tsc_overlay.take() {
            overlay::remove(&self.physmap, &ov);
        }
    }
}

impl MsrHandler for HyperV {
    fn rdmsr(&self, vcpu_id: i32, msr: u32) -> RdmsrOutcome {
        if !is_hyperv_msr(msr) {
            return RdmsrOutcome::NotHandled;
        }
        let inner = self.inner.lock().unwrap();
        match msr {
            HV_X64_MSR_GUEST_OS_ID => {
                RdmsrOutcome::Handled(inner.msr_guest_os_id)
            }
            HV_X64_MSR_HYPERCALL => {
                RdmsrOutcome::Handled(inner.msr_hypercall.raw())
            }
            HV_X64_MSR_VP_INDEX => RdmsrOutcome::Handled(vcpu_id as u64),

            // Read-only. Gated on the reference-TSC feature because
            // TLFS leaf 0x4000_0003 EAX bit 1 governs both.
            HV_X64_MSR_TIME_REF_COUNT if self.features.reference_tsc => {
                RdmsrOutcome::Handled(self.time_ref_count())
            }
            HV_X64_MSR_REFERENCE_TSC if self.features.reference_tsc => {
                RdmsrOutcome::Handled(inner.msr_reference_tsc.raw())
            }
            HV_X64_MSR_TIME_REF_COUNT | HV_X64_MSR_REFERENCE_TSC => {
                RdmsrOutcome::GpException
            }

            // The reset MSR acts on a write. A read returns 0 (no reset
            // in progress).
            HV_X64_MSR_RESET if self.features.reset => RdmsrOutcome::Handled(0),
            HV_X64_MSR_RESET => RdmsrOutcome::GpException,

            // Crash MSRs: the guest reads back what it last wrote.
            HV_X64_MSR_CRASH_P0 => RdmsrOutcome::Handled(inner.crash_p[0]),
            HV_X64_MSR_CRASH_P1 => RdmsrOutcome::Handled(inner.crash_p[1]),
            HV_X64_MSR_CRASH_P2 => RdmsrOutcome::Handled(inner.crash_p[2]),
            HV_X64_MSR_CRASH_P3 => RdmsrOutcome::Handled(inner.crash_p[3]),
            HV_X64_MSR_CRASH_P4 => RdmsrOutcome::Handled(inner.crash_p[4]),
            // CRASH_CTL: bit 63 (NOTIFY) reads back as set, which tells
            // the guest its writes are accepted. Lower bits are zero.
            HV_X64_MSR_CRASH_CTL => RdmsrOutcome::Handled(HV_CRASH_CTL_NOTIFY),

            _ => RdmsrOutcome::NotHandled,
        }
    }

    fn wrmsr(&self, _vcpu_id: i32, msr: u32, value: u64) -> WrmsrOutcome {
        if !is_hyperv_msr(msr) {
            return WrmsrOutcome::NotHandled;
        }
        let mut inner = self.inner.lock().unwrap();
        match msr {
            HV_X64_MSR_GUEST_OS_ID => {
                self.handle_wr_guest_os_id(&mut inner, value);
                WrmsrOutcome::Handled
            }
            HV_X64_MSR_HYPERCALL => {
                self.handle_wr_hypercall(&mut inner, value);
                WrmsrOutcome::Handled
            }
            HV_X64_MSR_VP_INDEX => WrmsrOutcome::GpException,

            HV_X64_MSR_RESET if self.features.reset => {
                if value & 1 != 0 {
                    info!(self.log, "guest requested reset via Hyper-V MSR");
                    WrmsrOutcome::Reset
                } else {
                    WrmsrOutcome::Handled
                }
            }
            HV_X64_MSR_RESET => WrmsrOutcome::GpException,

            HV_X64_MSR_TIME_REF_COUNT => WrmsrOutcome::GpException,
            HV_X64_MSR_REFERENCE_TSC if self.features.reference_tsc => {
                self.handle_wr_reference_tsc(&mut inner, value);
                WrmsrOutcome::Handled
            }
            HV_X64_MSR_REFERENCE_TSC => WrmsrOutcome::GpException,

            HV_X64_MSR_CRASH_P0 => {
                inner.crash_p[0] = value;
                WrmsrOutcome::Handled
            }
            HV_X64_MSR_CRASH_P1 => {
                inner.crash_p[1] = value;
                WrmsrOutcome::Handled
            }
            HV_X64_MSR_CRASH_P2 => {
                inner.crash_p[2] = value;
                WrmsrOutcome::Handled
            }
            HV_X64_MSR_CRASH_P3 => {
                inner.crash_p[3] = value;
                WrmsrOutcome::Handled
            }
            HV_X64_MSR_CRASH_P4 => {
                inner.crash_p[4] = value;
                WrmsrOutcome::Handled
            }
            HV_X64_MSR_CRASH_CTL => {
                if value & HV_CRASH_CTL_NOTIFY != 0 {
                    self.log_crash(&inner);
                }
                WrmsrOutcome::Handled
            }

            _ => WrmsrOutcome::NotHandled,
        }
    }
}

fn cpuid_entry(
    func: u32,
    index: u32,
    eax: u32,
    ebx: u32,
    ecx: u32,
    edx: u32,
) -> vcpu_cpuid_entry {
    // Hyper-V leaves have no sub-leaves, so VCE_FLAG_MATCH_INDEX stays
    // clear (any ECX input matches), as for the other non-indexed
    // leaves in vmm-core's cpuid module.
    vcpu_cpuid_entry {
        vce_function: func,
        vce_index: index,
        vce_flags: 0,
        vce_eax: eax,
        vce_ebx: ebx,
        vce_ecx: ecx,
        vce_edx: edx,
        _pad: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts the records a guest earns, so a test can show a WRMSR
    /// loop does not log in that loop.
    struct CountingDrain(Arc<AtomicUsize>);

    impl slog::Drain for CountingDrain {
        type Ok = ();
        type Err = slog::Never;

        fn log(
            &self,
            record: &slog::Record<'_>,
            _values: &slog::OwnedKVList,
        ) -> Result<(), Self::Err> {
            if record.level().is_at_least(slog::Level::Info) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }
    }

    fn hyperv(log: Logger) -> Arc<HyperV> {
        HyperV::new(log, Arc::new(PhysMap::new()), Features::default())
    }

    fn counting() -> (Arc<HyperV>, Arc<AtomicUsize>) {
        use slog::Drain;
        let count = Arc::new(AtomicUsize::new(0));
        let log = Logger::root(CountingDrain(count.clone()).fuse(), slog::o!());
        (hyperv(log), count)
    }

    #[test]
    fn the_crash_msrs_are_advertised_in_leaf_3_edx() {
        // Windows and Linux both test this bit before they write the
        // crash MSRs, so without it the BSOD payload never arrives.
        let hv = hyperv(Logger::root(slog::Discard, slog::o!()));
        let mut entries = Vec::new();
        hv.add_cpuid(&mut entries);
        let leaf3 = entries
            .iter()
            .find(|e| e.vce_function == 0x4000_0003)
            .expect("leaf 0x4000_0003");
        assert_ne!(leaf3.vce_edx & HvLeaf3Edx::GUEST_CRASH_MSRS.bits(), 0);
    }

    #[test]
    fn a_crash_ctl_loop_cannot_flood_the_log() {
        let (hv, count) = counting();
        for _ in 0..1000 {
            assert!(matches!(
                hv.wrmsr(0, HV_X64_MSR_CRASH_CTL, HV_CRASH_CTL_NOTIFY),
                WrmsrOutcome::Handled
            ));
        }
        let logged = count.load(Ordering::Relaxed);
        assert!(logged > 0, "the first crash must be reported");
        assert!(logged <= CRASH_LOG_BURST as usize, "{logged} records");
    }

    #[test]
    fn crash_ctl_without_notify_reports_nothing() {
        let (hv, count) = counting();
        assert!(matches!(
            hv.wrmsr(0, HV_X64_MSR_CRASH_CTL, 0),
            WrmsrOutcome::Handled
        ));
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }

    /// Enough guest memory for both overlay pages.
    const GPA: u64 = 0x1000;
    const LEN: usize = 0x10000;

    fn enlightenment() -> Arc<HyperV> {
        let physmap =
            Arc::new(PhysMap::new_anon(GPA, LEN).expect("guest memory"));
        HyperV::new(
            Logger::root(slog::Discard, slog::o!()),
            physmap,
            Features {
                tsc_freq_hz: 3_000_000_000,
                ..Features::default()
            },
        )
    }

    /// The hypercall MSR with the Enabled bit and a page at `gpa`.
    fn hypercall_msr(gpa: u64) -> u64 {
        gpa | MsrHypercallValue::ENABLED
    }

    /// What a guest RDMSR reads back. `RdmsrOutcome` carries no
    /// equality, so the value is taken out here.
    fn read(hv: &HyperV, msr: u32) -> u64 {
        match hv.rdmsr(0, msr) {
            RdmsrOutcome::Handled(value) => value,
            other => panic!("msr {msr:#x} was not handled: {other:?}"),
        }
    }

    #[test]
    fn the_msr_state_survives_an_export_and_import() {
        let source = enlightenment();
        source.wrmsr(0, HV_X64_MSR_GUEST_OS_ID, 0x8100_0000_0000_0001);
        source.wrmsr(0, HV_X64_MSR_HYPERCALL, hypercall_msr(0x2000));
        source.wrmsr(0, HV_X64_MSR_REFERENCE_TSC, 0x3000 | 1);
        source.wrmsr(0, HV_X64_MSR_CRASH_P0, 0xDEAD);

        let state = source.export_state();
        let dest = enlightenment();
        dest.import_state(&state);

        assert_eq!(read(&dest, HV_X64_MSR_GUEST_OS_ID), 0x8100_0000_0000_0001);
        assert_eq!(read(&dest, HV_X64_MSR_HYPERCALL), hypercall_msr(0x2000));
        assert_eq!(read(&dest, HV_X64_MSR_CRASH_P0), 0xDEAD);
    }

    #[test]
    fn the_hypercall_overlay_is_reinstalled_from_the_msr() {
        // The source drops its overlays before the final RAM pass, so
        // the destination must rebuild the page.
        let source = enlightenment();
        source.wrmsr(0, HV_X64_MSR_GUEST_OS_ID, 1);
        source.wrmsr(0, HV_X64_MSR_HYPERCALL, hypercall_msr(0x2000));
        let state = source.export_state();

        let dest = enlightenment();
        dest.import_state(&state);
        assert!(
            dest.inner.lock().unwrap().hypercall_overlay.is_some(),
            "the destination must hold the hypercall page",
        );
    }

    #[test]
    fn the_reference_counter_never_runs_backwards() {
        // A monotonic partition counter that restarts at zero is a
        // counter the guest sees go backwards.
        let source = enlightenment();
        let state = HypervMigrateState {
            time_ref_count: 500_000_000,
            ..source.export_state()
        };

        let dest = enlightenment();
        assert!(dest.time_ref_count() < 500_000_000, "a fresh VM starts low");
        dest.import_state(&state);
        assert!(
            dest.time_ref_count() >= 500_000_000,
            "the counter must continue from the source's value",
        );
    }

    #[test]
    fn a_guest_that_never_used_the_enlightenment_carries_nothing() {
        let state = enlightenment().export_state();
        assert_eq!(state.guest_os_id, 0);
        assert_eq!(state.hypercall, 0);
        assert_eq!(state.crash_p, [0; 5]);
    }
}
