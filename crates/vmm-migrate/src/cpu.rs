// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Checks that the destination can run the guest from the source.
//!
//! One comparison covers the whole effective CPUID contract. Both ends
//! report the features they expose after their own `--cpu-baseline`
//! mask, because that is what the guest sees. The comparison rejects by
//! default: a guest that uses an instruction the destination lacks
//! takes #UD in kernel context, and no error path can recover.

use vmm_core::cpuid::{query_masked_features, CpuBaseline};

use crate::codec::CpuFeatures;

/// What this host exposes to a guest under `baseline`.
pub fn local_features(baseline: CpuBaseline) -> CpuFeatures {
    let (leaf1_ecx, leaf1_edx, leaf7_ebx, leaf7_ecx, leaf7_edx, xcr0) =
        query_masked_features(baseline);
    CpuFeatures {
        leaf1_ecx,
        leaf1_edx,
        leaf7_ebx,
        leaf7_ecx,
        leaf7_edx,
        xcr0,
    }
}

/// The features the source exposes and the destination does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Missing {
    pub leaf1_ecx: u32,
    pub leaf1_edx: u32,
    pub leaf7_ebx: u32,
    pub leaf7_ecx: u32,
    pub leaf7_edx: u32,
    pub xcr0: u32,
}

impl Missing {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl std::fmt::Display for Missing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "leaf1.ecx={:#x} leaf1.edx={:#x} leaf7.ebx={:#x} \
             leaf7.ecx={:#x} leaf7.edx={:#x} xcr0={:#x}",
            self.leaf1_ecx,
            self.leaf1_edx,
            self.leaf7_ebx,
            self.leaf7_ecx,
            self.leaf7_edx,
            self.xcr0,
        )
    }
}

/// Every bit the source has and the destination does not have.
///
/// Extra bits on the destination are safe: the guest never saw them.
pub fn missing(src: &CpuFeatures, dst: &CpuFeatures) -> Missing {
    Missing {
        leaf1_ecx: src.leaf1_ecx & !dst.leaf1_ecx,
        leaf1_edx: src.leaf1_edx & !dst.leaf1_edx,
        leaf7_ebx: src.leaf7_ebx & !dst.leaf7_ebx,
        leaf7_ecx: src.leaf7_ecx & !dst.leaf7_ecx,
        leaf7_edx: src.leaf7_edx & !dst.leaf7_edx,
        xcr0: src.xcr0 & !dst.xcr0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features(leaf1_ecx: u32, leaf7_ebx: u32, xcr0: u32) -> CpuFeatures {
        CpuFeatures {
            leaf1_ecx,
            leaf1_edx: 0,
            leaf7_ebx,
            leaf7_ecx: 0,
            leaf7_edx: 0,
            xcr0,
        }
    }

    #[test]
    fn a_matching_destination_is_missing_nothing() {
        let f = features(0xFFFF, 0x20, 0x7);
        assert!(missing(&f, &f).is_empty());
    }

    #[test]
    fn a_richer_destination_is_missing_nothing() {
        // The guest never saw the extra bits, so it cannot use them.
        let src = features(0x1, 0x0, 0x3);
        let dst = features(0x3, 0x20, 0x7);
        assert!(missing(&src, &dst).is_empty());
    }

    #[test]
    fn every_leaf_is_compared_not_only_leaf_seven() {
        // A leaf 7 check alone accepts a destination without AVX or
        // without the XSAVE components the guest uses.
        let src = CpuFeatures {
            leaf1_ecx: 1 << 28,
            leaf1_edx: 1 << 25,
            leaf7_ebx: 0,
            leaf7_ecx: 0,
            leaf7_edx: 0,
            xcr0: 1 << 2,
        };
        let dst = features(0, 0, 0x3);
        let gap = missing(&src, &dst);
        assert!(!gap.is_empty());
        assert_eq!(gap.leaf1_ecx, 1 << 28);
        assert_eq!(gap.leaf1_edx, 1 << 25);
        assert_eq!(gap.xcr0, 1 << 2);
    }

    #[test]
    fn a_masked_source_hides_what_the_guest_never_saw() {
        // Both ends report post-mask tables, so a source with more
        // features than its baseline still matches.
        let src = local_features(CpuBaseline::Sse42);
        let dst = local_features(CpuBaseline::Sse42);
        assert!(missing(&src, &dst).is_empty());
    }
}
