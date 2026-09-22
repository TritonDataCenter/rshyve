// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Hypercall MSR + page.
//!
//! TLFS section 3.13. The guest writes a GPA into HV_X64_MSR_HYPERCALL,
//! and the hypervisor fills that page with a code sequence the guest
//! can call. No hypercall is implemented: the page returns "not
//! supported" without a trap. This is enough for the Windows boot-time
//! probe. Performance hypercalls (TLB flush, IPI) are not enabled.

use crate::overlay::PAGE_SIZE;

/// HYPERCALL MSR layout (TLFS 3.13):
///   bit 0     Enabled
///   bit 1     Locked (once set, MSR becomes write-protected)
///   bits 2-11 Reserved (must be 0)
///   bits 12-63 Hypercall page PFN
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MsrHypercallValue(pub u64);

impl MsrHypercallValue {
    pub const ENABLED: u64 = 1 << 0;
    pub const LOCKED: u64 = 1 << 1;
    pub const PFN_MASK: u64 = !0xFFF;

    pub fn enabled(self) -> bool {
        self.0 & Self::ENABLED != 0
    }
    pub fn locked(self) -> bool {
        self.0 & Self::LOCKED != 0
    }
    pub fn gpa(self) -> u64 {
        self.0 & Self::PFN_MASK
    }
    pub fn raw(self) -> u64 {
        self.0
    }

    /// Apply a guest write. Once `Locked` is set, the guest cannot
    /// clear it or change the PFN or Enabled.
    pub fn apply_write(self, new: u64) -> Self {
        if self.locked() {
            return self;
        }
        Self(new)
    }
}

/// Build the 4 KiB hypercall page contents.
///
/// The first 8 bytes are `mov rax, 2; ret`: return status
/// HV_STATUS_INVALID_HYPERCALL_CODE without a trap. The rest is zero.
/// Windows accepts this, and does not use the page for
/// performance-critical paths while their CPUID feature bits are clear.
pub fn build_page() -> Box<[u8; PAGE_SIZE]> {
    let mut page = Box::new([0u8; PAGE_SIZE]);
    // 48 c7 c0 02 00 00 00   mov rax, 2
    // c3                     ret
    page[0..8]
        .copy_from_slice(&[0x48, 0xc7, 0xc0, 0x02, 0x00, 0x00, 0x00, 0xc3]);
    page
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locked_bit_is_sticky() {
        let v = MsrHypercallValue(
            MsrHypercallValue::LOCKED | MsrHypercallValue::ENABLED,
        );
        assert!(v.locked());
        // A write that clears Locked and Enabled is ignored.
        let v2 = v.apply_write(0);
        assert!(v2.locked());
        assert!(v2.enabled());
        assert_eq!(v2.0, v.0);
    }

    #[test]
    fn unlocked_writes_pass_through() {
        let v = MsrHypercallValue(0);
        let new = MsrHypercallValue::ENABLED
            | (0xdead_0000 & MsrHypercallValue::PFN_MASK);
        let v2 = v.apply_write(new);
        assert!(v2.enabled());
        assert_eq!(v2.gpa(), 0xdead_0000);
    }

    #[test]
    fn page_starts_with_mov_ret() {
        let page = build_page();
        assert_eq!(
            &page[0..8],
            &[0x48, 0xc7, 0xc0, 0x02, 0x00, 0x00, 0x00, 0xc3]
        );
        assert!(page[8..].iter().all(|&b| b == 0));
    }
}
