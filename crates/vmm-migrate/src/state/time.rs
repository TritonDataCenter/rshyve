// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Moves the source's guest clock onto the destination's time base.
//!
//! The peer controls every source field. The kernel's only check on the
//! result is that `boot_hrtime` is not in the future (illumos `vmm.c`).
//! A wrapped value passes that check and corrupts every LAPIC, HPET and
//! PIT target, so all arithmetic here is checked.

use std::mem::size_of;
use std::slice;
use std::sync::Arc;

use bhyve_api::VDC_VMM_TIME;
use vmm_core::hdl::VmmHdl;

use crate::codec::{MigrateError, TimeData};

use super::checks::{check_class_blob, read_class_layouts};
use super::{read_class_all, write_class_all};

/// Export time synchronization data from the kernel.
pub fn export_time_data(
    hdl: &Arc<VmmHdl>,
    log: &slog::Logger,
) -> Result<TimeData, MigrateError> {
    let vmm_time = read_class_all(hdl, VDC_VMM_TIME, 1)?;
    slog::info!(log, "exported VMM_TIME"; "bytes" => vmm_time.len());
    Ok(TimeData { vmm_time })
}

/// The largest wall-clock gap that a switchover can show.
///
/// A larger gap is clock disagreement, not migration time.
const MAX_MIGRATE_DELTA_NS: i128 = 3_600 * 1_000_000_000;

/// Put the source's guest clock on the destination's time base.
///
/// Pure, so tests can check the arithmetic without a live instance. The
/// peer controls every `src` field, so every step is checked. See the
/// module documentation.
fn adjust_time(
    src: &bhyve_api::vdi_time_info_v1,
    dst: &bhyve_api::vdi_time_info_v1,
    log: &slog::Logger,
) -> Result<bhyve_api::vdi_time_info_v1, MigrateError> {
    use bhyve_api::vdi_time_info_v1;

    let bad = |what: &str| MigrateError::State(format!("VMM_TIME: {what}"));

    // hrtime is monotonic from boot, so uptime cannot be negative and
    // cannot exceed hrtime.
    let guest_uptime = src
        .vt_hrtime
        .checked_sub(src.vt_boot_hrtime)
        .ok_or_else(|| bad("hrtime - boot_hrtime overflows"))?;
    if guest_uptime < 0 {
        return Err(bad("boot_hrtime is after hrtime"));
    }

    let wall_ns = |sec: u64, ns: u64| -> Option<i128> {
        i128::from(sec)
            .checked_mul(1_000_000_000)?
            .checked_add(i128::from(ns))
    };
    let src_wc_ns = wall_ns(src.vt_hres_sec, src.vt_hres_ns)
        .ok_or_else(|| bad("source wall clock overflows"))?;
    let dst_wc_ns = wall_ns(dst.vt_hres_sec, dst.vt_hres_ns)
        .ok_or_else(|| bad("destination wall clock overflows"))?;
    // A source clock ahead of the destination clock gives a negative
    // delta. That is clock skew, so clamp it to 0.
    let migrate_delta_ns = (dst_wc_ns - src_wc_ns).max(0);
    // A switchover takes seconds. A delta past the bound means the
    // clocks disagree or the peer sent a false value. In both cases
    // every guest timer derived from it is wrong.
    if migrate_delta_ns > MAX_MIGRATE_DELTA_NS {
        return Err(bad(&format!(
            "the two hosts' clocks differ by {} s",
            migrate_delta_ns / 1_000_000_000,
        )));
    }

    let new_boot_hrtime = i128::from(dst.vt_hrtime)
        - (i128::from(guest_uptime) + migrate_delta_ns);
    let new_boot_hrtime = i64::try_from(new_boot_hrtime)
        .map_err(|_| bad("adjusted boot_hrtime does not fit"))?;

    // From here the guest TSC ticks at the destination's rate, so the
    // delta uses that rate.
    let tsc_freq_for_delta = if dst.vt_guest_freq != src.vt_guest_freq {
        dst.vt_guest_freq
    } else {
        src.vt_guest_freq
    };
    let tsc_delta = if tsc_freq_for_delta > 0 {
        u64::try_from(
            migrate_delta_ns * i128::from(tsc_freq_for_delta) / 1_000_000_000,
        )
        .map_err(|_| bad("TSC delta does not fit"))?
    } else {
        0
    };
    let scaled_guest_tsc =
        if dst.vt_guest_freq != src.vt_guest_freq && src.vt_guest_freq > 0 {
            u64::try_from(
                u128::from(src.vt_guest_tsc) * u128::from(dst.vt_guest_freq)
                    / u128::from(src.vt_guest_freq),
            )
            .map_err(|_| bad("scaled guest TSC does not fit"))?
        } else {
            src.vt_guest_tsc
        };
    // The guest reads the TSC as a wrapping counter. The wrap matches
    // hardware behaviour.
    let new_guest_tsc = scaled_guest_tsc.wrapping_add(tsc_delta);

    slog::info!(log, "VMM_TIME adjust";
        "guest_uptime_ms" => guest_uptime / 1_000_000,
        "migrate_delta_ms" => migrate_delta_ns / 1_000_000,
        "boot_hrtime" => new_boot_hrtime,
        "tsc_delta" => tsc_delta,
    );

    // The kernel can reject a write with a different frequency (EPERM).
    // The kernel scales the TSC itself.
    let guest_freq = if dst.vt_guest_freq != src.vt_guest_freq {
        slog::warn!(log, "TSC freq mismatch, using destination";
            "src_freq_hz" => src.vt_guest_freq,
            "dst_freq_hz" => dst.vt_guest_freq,
        );
        dst.vt_guest_freq
    } else {
        src.vt_guest_freq
    };

    Ok(vdi_time_info_v1 {
        vt_guest_freq: guest_freq,
        vt_guest_tsc: new_guest_tsc,
        vt_boot_hrtime: new_boot_hrtime,
        vt_hrtime: dst.vt_hrtime,
        vt_hres_sec: dst.vt_hres_sec,
        vt_hres_ns: dst.vt_hres_ns,
    })
}

/// Import time synchronization data into the kernel.
///
/// Call this before the device state import, because timers need the
/// correct time base.
pub fn import_time_data(
    hdl: &Arc<VmmHdl>,
    time: &TimeData,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    use bhyve_api::vdi_time_info_v1;

    // The payload is a raw vdi_time_info_v1 from the source kernel. A
    // different length is a different struct, and reading it as the
    // local struct gives wrong timer targets.
    let layouts = read_class_layouts(hdl)?;
    check_class_blob(
        &layouts,
        "VMM_TIME",
        VDC_VMM_TIME,
        1,
        time.vmm_time.len(),
    )?;
    if time.vmm_time.len() != size_of::<vdi_time_info_v1>() {
        return Err(MigrateError::State(format!(
            "VMM_TIME: payload has {} bytes, this build reads {}",
            time.vmm_time.len(),
            size_of::<vdi_time_info_v1>()
        )));
    }
    // SAFETY: the length check above makes the buffer exactly one
    // vdi_time_info_v1, and read_unaligned has no alignment need. The
    // struct is plain integers, so every bit pattern is valid.
    let src: vdi_time_info_v1 = unsafe {
        std::ptr::read_unaligned(
            time.vmm_time.as_ptr() as *const vdi_time_info_v1
        )
    };

    let dst = hdl
        .data_op(VDC_VMM_TIME, 1)
        .read::<vdi_time_info_v1>()
        .map_err(|e| {
            MigrateError::VmmData(format!("read dst VMM_TIME: {e:?}"))
        })?;

    let adjusted = adjust_time(&src, &dst, log)?;

    // SAFETY: `adjusted` is a live vdi_time_info_v1 of plain integers
    // with no padding. The slice covers exactly its size and does not
    // outlive it.
    let adjusted_bytes: &[u8] = unsafe {
        slice::from_raw_parts(
            &adjusted as *const _ as *const u8,
            size_of::<vdi_time_info_v1>(),
        )
    };
    write_class_all(hdl, VDC_VMM_TIME, 1, adjusted_bytes)?;
    slog::info!(log, "VMM_TIME import complete (adjusted)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use bhyve_api::vdi_time_info_v1;

    fn test_log() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    fn time(
        hrtime: i64,
        boot_hrtime: i64,
        sec: u64,
        ns: u64,
    ) -> vdi_time_info_v1 {
        vdi_time_info_v1 {
            vt_guest_freq: 3_000_000_000,
            vt_guest_tsc: 1_000,
            vt_boot_hrtime: boot_hrtime,
            vt_hrtime: hrtime,
            vt_hres_sec: sec,
            vt_hres_ns: ns,
        }
    }

    #[test]
    fn time_adjust_carries_uptime_and_the_migration_gap() {
        let src = time(10_000_000_000, 1_000_000_000, 100, 0);
        let dst = time(50_000_000_000, 0, 102, 0);
        let out = adjust_time(&src, &dst, &test_log()).expect("sane input");
        // uptime 9 s plus a 2 s gap, taken off the destination's hrtime.
        assert_eq!(out.vt_boot_hrtime, 50_000_000_000 - 11_000_000_000);
        assert_eq!(out.vt_hrtime, dst.vt_hrtime);
        // 2 s at 3 GHz.
        assert_eq!(out.vt_guest_tsc, 1_000 + 6_000_000_000);
    }

    #[test]
    fn a_source_clock_ahead_of_ours_is_not_a_negative_migration() {
        let src = time(10_000_000_000, 0, 200, 0);
        let dst = time(50_000_000_000, 0, 100, 0);
        let out = adjust_time(&src, &dst, &test_log()).expect("clock skew");
        assert_eq!(out.vt_boot_hrtime, 40_000_000_000);
        assert_eq!(out.vt_guest_tsc, 1_000);
    }

    #[test]
    fn a_peer_wall_clock_at_u64_max_does_not_wrap_the_multiply() {
        // vt_hres_sec * 1e9 overflows an i64. With a wide multiply it
        // is a source clock far ahead, so the delta clamps to 0.
        let src = time(10_000_000_000, 0, u64::MAX, 0);
        let dst = time(50_000_000_000, 0, 100, 0);
        let out = adjust_time(&src, &dst, &test_log()).expect("clamped");
        assert_eq!(out.vt_boot_hrtime, 40_000_000_000);
        assert_eq!(out.vt_guest_tsc, 1_000);
    }

    #[test]
    fn a_zero_peer_wall_clock_is_refused_as_clock_disagreement() {
        // vt_hres_sec = 0 gives a ~1.7e18 ns delta, which overflows
        // u64 at 3 GHz. A 54-year gap is not a switchover.
        let src = time(10_000_000_000, 0, 0, 0);
        let dst = time(50_000_000_000, 0, 1_700_000_000, 0);
        let error = adjust_time(&src, &dst, &test_log())
            .map(|_| ())
            .expect_err("a 54-year gap must be refused");
        assert!(error.to_string().contains("clocks differ"), "{error}");
    }

    #[test]
    fn a_boot_hrtime_after_hrtime_is_refused() {
        let src = time(1_000, 2_000, 100, 0);
        let dst = time(50_000_000_000, 0, 100, 0);
        let error = adjust_time(&src, &dst, &test_log())
            .map(|_| ())
            .expect_err("uptime cannot be negative");
        assert!(error.to_string().contains("after hrtime"), "{error}");
    }

    #[test]
    fn a_boot_hrtime_at_i64_min_does_not_wrap_the_subtraction() {
        let src = time(10_000_000_000, i64::MIN, 100, 0);
        let dst = time(50_000_000_000, 0, 100, 0);
        adjust_time(&src, &dst, &test_log())
            .map(|_| ())
            .expect_err("i64::MIN boot_hrtime must be refused");
    }
}
