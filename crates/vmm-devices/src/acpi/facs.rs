// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! FACS (Firmware ACPI Control Structure).

const FACS_SIZE: usize = 64;

pub(super) fn build_facs() -> Vec<u8> {
    let mut buf = vec![0u8; FACS_SIZE];
    // Signature: "FACS" at offset 0
    buf[0..4].copy_from_slice(b"FACS");
    // Length at offset 4 (u32 LE)
    buf[4..8].copy_from_slice(&(FACS_SIZE as u32).to_le_bytes());
    // Version at offset 32 (u32 LE)
    buf[32..36].copy_from_slice(&2u32.to_le_bytes());
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facs_valid() {
        let facs = build_facs();
        assert_eq!(facs.len(), FACS_SIZE);
        assert_eq!(&facs[0..4], b"FACS");
        let len = u32::from_le_bytes([facs[4], facs[5], facs[6], facs[7]]);
        assert_eq!(len, FACS_SIZE as u32);
    }
}
