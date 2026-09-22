// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Initrd placement, shared by the direct-boot and PVH paths.

use anyhow::{ensure, Context, Result};

/// Ceiling of the low guest-RAM window, below the 32-bit MMIO hole.
pub use vmm_core::mem::MMIO_HOLE_BASE as LOWMEM_LIMIT;

/// Highest page-aligned address that holds `rd_size` bytes below the
/// low-memory ceiling, without touching the kernel image.
///
/// `checked_sub` is load-bearing. An unchecked subtraction wraps in a
/// release build, and the wrapped address then satisfies the overlap
/// guard below, so the guard is defeated by the underflow before it.
pub fn place_initrd(
    mem_size: usize,
    rd_size: usize,
    load_end: u64,
) -> Result<u64> {
    let lowmem_limit = (mem_size as u64).min(LOWMEM_LIMIT);
    let rd_addr = lowmem_limit
        .checked_sub(rd_size as u64)
        .context("initrd larger than low memory")?
        & !0xFFF;
    ensure!(
        rd_addr >= load_end,
        "initrd overlaps kernel (need more RAM)"
    );
    Ok(rd_addr)
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1024 * 1024;

    #[test]
    fn oversized_initrd_is_rejected_not_wrapped() {
        // An unchecked `lowmem_limit - rd_size` wraps to a huge address
        // in a release build, and the wrapped value then satisfies the
        // overlap guard in `place_initrd`.
        let err = place_initrd(64 * MIB, 128 * MIB, 0x10_0000)
            .expect_err("an initrd larger than low memory must be an error");
        assert!(
            err.to_string().contains("initrd larger than low memory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn initrd_exactly_filling_lowmem_hits_the_overlap_guard() {
        let err = place_initrd(64 * MIB, 64 * MIB, 0x10_0000)
            .expect_err("rd_addr 0 must fail the overlap guard");
        assert!(
            err.to_string().contains("initrd overlaps kernel"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn overlapping_initrd_is_rejected() {
        let err = place_initrd(64 * MIB, 60 * MIB, 32 * MIB as u64)
            .expect_err("an initrd below the kernel end must be an error");
        assert!(
            err.to_string().contains("initrd overlaps kernel"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn placement_is_page_aligned_below_the_lowmem_ceiling() {
        let addr = place_initrd(64 * MIB, 1000, 0x10_0000).expect("placed");
        assert_eq!(addr & 0xFFF, 0, "placement must be page-aligned");
        assert!(addr + 1000 <= 64 * MIB as u64);
        assert!(addr >= 0x10_0000);
    }

    #[test]
    fn lowmem_ceiling_caps_placement_on_a_large_vm() {
        // 8 GiB of RAM still places the initrd below the 32-bit MMIO hole.
        let addr =
            place_initrd(8 * 1024 * MIB, 4096, 0x10_0000).expect("placed");
        assert!(addr < LOWMEM_LIMIT);
    }
}
