// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure decoders for the command header and the PRD table. They are separate
//! so that their checked arithmetic is reviewable on its own.

use super::bits::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CmdHdr {
    pub cfl_dwords: u8,
    pub atapi: bool,
    pub write: bool,
    pub prdtl: u16,
    pub ctba: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum HdrError {
    CflOutOfRange(u8),
    PrdtlTooLarge(u16),
    CtbaMisaligned(u64),
    SizeOverflow,
}

pub fn parse_cmd_hdr(raw: &[u8; CL_ENTRY_SIZE]) -> Result<CmdHdr, HdrError> {
    let flags = u16::from_le_bytes([raw[0], raw[1]]);
    let cfl_dwords = (flags & 0x1f) as u8;
    if !(2..=16).contains(&cfl_dwords) {
        return Err(HdrError::CflOutOfRange(cfl_dwords));
    }

    let prdtl = u16::from_le_bytes([raw[2], raw[3]]);
    if prdtl > MAX_PRDTL {
        return Err(HdrError::PrdtlTooLarge(prdtl));
    }

    let ctba = u64::from_le_bytes(
        raw[8..16]
            .try_into()
            .expect("command header CTBA has fixed width"),
    );
    if ctba & 0x7f != 0 {
        return Err(HdrError::CtbaMisaligned(ctba));
    }
    if cmd_table_len(prdtl).is_none() {
        return Err(HdrError::SizeOverflow);
    }

    Ok(CmdHdr {
        cfl_dwords,
        atapi: flags & (1 << 5) != 0,
        write: flags & (1 << 6) != 0,
        prdtl,
        ctba,
    })
}

pub fn cmd_table_len(prdtl: u16) -> Option<usize> {
    CMD_TBL_PRDT_OFF
        .checked_add(usize::from(prdtl).checked_mul(PRD_ENTRY_SIZE)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prd {
    pub dba: u64,
    pub len: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PrdtError {
    Truncated,
    ZeroOrOddLength(usize),
    DbaMisaligned(u64),
    TotalTooLarge,
}

pub fn parse_prdt(raw: &[u8], prdtl: u16) -> Result<Vec<Prd>, PrdtError> {
    let mut prds = Vec::with_capacity(usize::from(prdtl));
    let mut total = 0u64;
    let entries = match raw.get(CMD_TBL_PRDT_OFF..) {
        Some(entries) => entries,
        None if prdtl == 0 => return Ok(prds),
        None => return Err(PrdtError::Truncated),
    };

    // as_chunks drops a trailing partial entry, so a short final PRD is
    // ignored.
    for (index, entry) in entries
        .as_chunks::<PRD_ENTRY_SIZE>()
        .0
        .iter()
        .take(usize::from(prdtl))
        .enumerate()
    {
        let dba = u64::from_le_bytes(
            entry[0..8].try_into().expect("PRDT DBA has fixed width"),
        );
        if dba & 1 != 0 {
            return Err(PrdtError::DbaMisaligned(dba));
        }

        let dbc = u32::from_le_bytes(
            entry[12..16].try_into().expect("PRDT DBC has fixed width"),
        );
        let len = (dbc & DBCMASK) + 1;
        if len == 0 || len & 1 != 0 {
            return Err(PrdtError::ZeroOrOddLength(index));
        }

        total = total
            .checked_add(u64::from(len))
            .ok_or(PrdtError::TotalTooLarge)?;
        if total > MAX_XFER_BYTES {
            return Err(PrdtError::TotalTooLarge);
        }
        prds.push(Prd { dba, len });
    }

    if prds.len() != usize::from(prdtl) {
        return Err(PrdtError::Truncated);
    }

    Ok(prds)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_cmd_hdr() -> [u8; CL_ENTRY_SIZE] {
        let mut raw = [0u8; CL_ENTRY_SIZE];
        let flags = 5u16 | (1u16 << 5) | (1u16 << 6);
        raw[0..2].copy_from_slice(&flags.to_le_bytes());
        raw[2..4].copy_from_slice(&2u16.to_le_bytes());
        raw[8..16].copy_from_slice(&0x1234_5600u64.to_le_bytes());
        raw
    }

    fn prdt_table(entries: usize) -> Vec<u8> {
        let prdtl = u16::try_from(entries).expect("test PRDTL fits u16");
        vec![0u8; cmd_table_len(prdtl).expect("test command table length")]
    }

    #[test]
    fn parse_cmd_hdr_roundtrip() {
        assert_eq!(
            parse_cmd_hdr(&valid_cmd_hdr()),
            Ok(CmdHdr {
                cfl_dwords: 5,
                atapi: true,
                write: true,
                prdtl: 2,
                ctba: 0x1234_5600,
            })
        );
    }

    #[test]
    fn parse_cmd_hdr_rejects_cfl_out_of_range() {
        for cfl in [0, 17] {
            let mut raw = valid_cmd_hdr();
            raw[0] = (raw[0] & !0x1f) | cfl;
            assert_eq!(parse_cmd_hdr(&raw), Err(HdrError::CflOutOfRange(cfl)));
        }
    }

    #[test]
    fn parse_cmd_hdr_checks_prdtl_boundaries() {
        for prdtl in [0, 1, MAX_PRDTL] {
            let mut raw = valid_cmd_hdr();
            raw[2..4].copy_from_slice(&prdtl.to_le_bytes());
            assert_eq!(parse_cmd_hdr(&raw).expect("valid PRDTL").prdtl, prdtl);
        }
        for prdtl in [MAX_PRDTL + 1, u16::MAX] {
            let mut raw = valid_cmd_hdr();
            raw[2..4].copy_from_slice(&prdtl.to_le_bytes());
            assert_eq!(
                parse_cmd_hdr(&raw),
                Err(HdrError::PrdtlTooLarge(prdtl))
            );
        }
    }

    #[test]
    fn parse_cmd_hdr_rejects_unaligned_ctba() {
        let mut raw = valid_cmd_hdr();
        raw[8..16].copy_from_slice(&0x81u64.to_le_bytes());
        assert_eq!(parse_cmd_hdr(&raw), Err(HdrError::CtbaMisaligned(0x81)));
    }

    #[test]
    fn cmd_table_len_is_checked_for_every_u16_prdtl() {
        assert_eq!(cmd_table_len(MAX_PRDTL), Some(0x2080));
        assert_eq!(cmd_table_len(u16::MAX), Some(0x10_0070));
    }

    #[test]
    fn parse_prdt_rejects_truncated_buffer() {
        let mut raw = prdt_table(1);
        raw[CMD_TBL_PRDT_OFF + 12..CMD_TBL_PRDT_OFF + 16]
            .copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(parse_prdt(&raw, 2), Err(PrdtError::Truncated));
    }

    #[test]
    fn parse_prdt_rejects_odd_length() {
        let raw = prdt_table(1);
        assert_eq!(parse_prdt(&raw, 1), Err(PrdtError::ZeroOrOddLength(0)));
    }

    #[test]
    fn parse_prdt_rejects_unaligned_dba() {
        let mut raw = prdt_table(1);
        raw[CMD_TBL_PRDT_OFF..CMD_TBL_PRDT_OFF + 8]
            .copy_from_slice(&0x1001u64.to_le_bytes());
        raw[CMD_TBL_PRDT_OFF + 12..CMD_TBL_PRDT_OFF + 16]
            .copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(parse_prdt(&raw, 1), Err(PrdtError::DbaMisaligned(0x1001)));
    }

    #[test]
    fn parse_prdt_rejects_total_over_max() {
        let mut raw = prdt_table(usize::from(MAX_PRDTL));
        for entry in raw[CMD_TBL_PRDT_OFF..].as_chunks_mut::<PRD_ENTRY_SIZE>().0
        {
            entry[12..16].copy_from_slice(&DBCMASK.to_le_bytes());
        }
        assert_eq!(parse_prdt(&raw, MAX_PRDTL), Err(PrdtError::TotalTooLarge));
    }

    #[test]
    fn parse_prdt_rejects_single_entry_over_max() {
        let mut raw = prdt_table(1);
        raw[CMD_TBL_PRDT_OFF + 12..CMD_TBL_PRDT_OFF + 16]
            .copy_from_slice(&DBCMASK.to_le_bytes());
        assert_eq!(DBCMASK + 1, 0x0040_0000);
        assert_eq!(parse_prdt(&raw, 1), Err(PrdtError::TotalTooLarge));
    }
}
