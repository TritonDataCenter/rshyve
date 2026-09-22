// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SCSI/MMC command dispatch. It is pure, so tests can feed hostile CDBs
//! without guest-memory mappings.

use super::bits::{
    ASC_INVALID_FIELD_IN_CDB, ASC_INVALID_OPCODE, ASC_MEDIA_REMOVAL_PREVENTED,
    ASC_SAVING_PARAMS_NOT_SUPPORTED, MODEPAGE_CD_CAPABILITIES,
    MODEPAGE_RW_ERROR_RECOVERY, SCSI_GET_EVENT_STATUS, SCSI_INQUIRY,
    SCSI_MODE_SENSE_10, SCSI_PREVENT_ALLOW, SCSI_READ_10, SCSI_READ_12,
    SCSI_READ_CAPACITY, SCSI_READ_TOC, SCSI_REPORT_LUNS, SCSI_REQUEST_SENSE,
    SCSI_START_STOP_UNIT, SCSI_TEST_UNIT_READY, SENSE_ILLEGAL_REQUEST,
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SenseState {
    pub key: u8,
    pub asc: u8,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AtapiOutcome {
    Ok,
    Data(Vec<u8>),
    ReadBlocks { lba: u64, count: u32 },
    Check { sense_key: u8, asc: u8 },
}

pub fn dispatch(
    acmd: &[u8; 16],
    blocks: u64,
    sense: &mut SenseState,
) -> AtapiOutcome {
    let outcome = match acmd[0] {
        SCSI_TEST_UNIT_READY | SCSI_PREVENT_ALLOW => AtapiOutcome::Ok,
        SCSI_REQUEST_SENSE => request_sense(acmd, sense),
        SCSI_INQUIRY => inquiry(acmd),
        SCSI_START_STOP_UNIT => match acmd[4] & 3 {
            0 | 1 | 3 => AtapiOutcome::Ok,
            2 => AtapiOutcome::Check {
                sense_key: SENSE_ILLEGAL_REQUEST,
                asc: ASC_MEDIA_REMOVAL_PREVENTED,
            },
            _ => unreachable!("two-bit start/stop field"),
        },
        SCSI_READ_CAPACITY => read_capacity(blocks),
        SCSI_READ_10 => AtapiOutcome::ReadBlocks {
            lba: u64::from(u32::from_be_bytes([
                acmd[2], acmd[3], acmd[4], acmd[5],
            ])),
            count: u32::from(u16::from_be_bytes([acmd[7], acmd[8]])),
        },
        SCSI_READ_12 => AtapiOutcome::ReadBlocks {
            lba: u64::from(u32::from_be_bytes([
                acmd[2], acmd[3], acmd[4], acmd[5],
            ])),
            count: u32::from_be_bytes([acmd[6], acmd[7], acmd[8], acmd[9]]),
        },
        SCSI_READ_TOC => read_toc(acmd, blocks),
        SCSI_GET_EVENT_STATUS => get_event_status(acmd),
        SCSI_MODE_SENSE_10 => mode_sense_10(acmd),
        SCSI_REPORT_LUNS => {
            let mut buf = vec![0u8; 16];
            buf[3] = 8;
            AtapiOutcome::Data(buf)
        }
        // Windows Setup needs no other MMC opcode, GET CONFIGURATION
        // included, so all other commands fail.
        _ => AtapiOutcome::Check {
            sense_key: SENSE_ILLEGAL_REQUEST,
            asc: ASC_INVALID_OPCODE,
        },
    };

    match outcome {
        AtapiOutcome::Check { sense_key, asc } => {
            sense.key = sense_key;
            sense.asc = asc;
            AtapiOutcome::Check { sense_key, asc }
        }
        other => other,
    }
}

fn request_sense(acmd: &[u8; 16], sense: &SenseState) -> AtapiOutcome {
    let len = usize::from(acmd[4]).min(64);
    let mut buf = vec![0u8; len];
    if let Some(byte) = buf.get_mut(0) {
        *byte = 0xf0;
    }
    if let Some(byte) = buf.get_mut(2) {
        *byte = sense.key;
    }
    if let Some(byte) = buf.get_mut(7) {
        *byte = 10;
    }
    if let Some(byte) = buf.get_mut(12) {
        *byte = sense.asc;
    }
    AtapiOutcome::Data(buf)
}

fn inquiry(acmd: &[u8; 16]) -> AtapiOutcome {
    if acmd[1] & 1 != 0 {
        return if acmd[2] == 0 {
            AtapiOutcome::Data(vec![0x05, 0, 0, 1, 0])
        } else {
            AtapiOutcome::Check {
                sense_key: SENSE_ILLEGAL_REQUEST,
                asc: ASC_INVALID_FIELD_IN_CDB,
            }
        };
    }

    let mut buf = vec![0u8; 36];
    buf[0] = 0x05;
    buf[1] = 0x80;
    buf[2] = 0x00;
    buf[3] = 0x21;
    buf[4] = 31;
    atapi_string(&mut buf[8..16], "BHYVE");
    atapi_string(&mut buf[16..32], "BHYVE DVD-ROM");
    atapi_string(&mut buf[32..36], "001");
    truncate(buf, usize::from(acmd[4]))
}

fn read_capacity(blocks: u64) -> AtapiOutcome {
    let mut buf = vec![0u8; 8];
    put_be32(&mut buf[0..4], blocks.saturating_sub(1) as u32);
    put_be32(&mut buf[4..8], 2048);
    AtapiOutcome::Data(buf)
}

fn read_toc(acmd: &[u8; 16], blocks: u64) -> AtapiOutcome {
    let alloc_len = usize::from(u16::from_be_bytes([acmd[7], acmd[8]]));
    match acmd[9] >> 6 {
        0 => read_toc_format0(acmd, blocks, alloc_len),
        1 => {
            let mut buf = vec![0u8; 12];
            buf[1] = 0x0a;
            buf[2] = 0x01;
            buf[3] = 0x01;
            truncate(buf, alloc_len)
        }
        2 | 3 => {
            // Setup does not use TOC format 2. Refusing it removes one more
            // guest-controlled variable-length layout.
            AtapiOutcome::Check {
                sense_key: SENSE_ILLEGAL_REQUEST,
                asc: ASC_INVALID_FIELD_IN_CDB,
            }
        }
        _ => unreachable!("two-bit TOC format"),
    }
}

fn read_toc_format0(
    acmd: &[u8; 16],
    blocks: u64,
    alloc_len: usize,
) -> AtapiOutcome {
    let start_track = acmd[6];
    if start_track > 1 && start_track != 0xaa {
        return AtapiOutcome::Check {
            sense_key: SENSE_ILLEGAL_REQUEST,
            asc: ASC_INVALID_FIELD_IN_CDB,
        };
    }

    let msf = (acmd[1] >> 1) & 1 != 0;
    let mut buf = [0u8; 20];
    buf[2] = 1;
    buf[3] = 1;
    let mut len = 4;

    if start_track <= 1 {
        buf[len..len + 4].copy_from_slice(&[0x00, 0x14, 0x01, 0x00]);
        put_toc_address(&mut buf[len + 4..len + 8], 0, msf);
        len += 8;
    }

    buf[len..len + 4].copy_from_slice(&[0x00, 0x14, 0xaa, 0x00]);
    put_toc_address(&mut buf[len + 4..len + 8], blocks, msf);
    len += 8;
    put_be16(&mut buf[0..2], (len - 2) as u16);

    truncate(buf[..len].to_vec(), alloc_len)
}

fn put_toc_address(dest: &mut [u8], lba: u64, msf: bool) {
    if msf {
        dest[0] = 0;
        dest[1..4].copy_from_slice(&lba_to_msf(lba));
    } else {
        put_be32(dest, lba as u32);
    }
}

fn get_event_status(acmd: &[u8; 16]) -> AtapiOutcome {
    if acmd[1] & 1 == 0 {
        return AtapiOutcome::Check {
            sense_key: SENSE_ILLEGAL_REQUEST,
            asc: ASC_INVALID_FIELD_IN_CDB,
        };
    }

    let alloc_len = usize::from(u16::from_be_bytes([acmd[7], acmd[8]]));
    let mut buf = vec![0u8; 8];
    put_be16(&mut buf[0..2], 6);
    buf[2] = 0x04;
    buf[3] = 0x10;
    buf[4] = 0;
    buf[5] = 0x02;
    truncate(buf, alloc_len)
}

fn mode_sense_10(acmd: &[u8; 16]) -> AtapiOutcome {
    let alloc_len = usize::from(u16::from_be_bytes([acmd[7], acmd[8]]));
    let pc = acmd[2] >> 6;
    let code = acmd[2] & 0x3f;

    let buf = match (pc, code) {
        (0, MODEPAGE_RW_ERROR_RECOVERY) => {
            let mut buf = vec![0u8; 16];
            put_be16(&mut buf[0..2], 14);
            buf[2] = 0x70;
            buf[8] = MODEPAGE_RW_ERROR_RECOVERY;
            buf[9] = 6;
            buf[11] = 0x05;
            buf
        }
        (0, MODEPAGE_CD_CAPABILITIES) => {
            let mut buf = vec![0u8; 30];
            put_be16(&mut buf[0..2], 28);
            buf[2] = 0x70;
            buf[8] = MODEPAGE_CD_CAPABILITIES;
            buf[9] = 20;
            buf[10] = 0x08;
            buf[12] = 0x71;
            put_be16(&mut buf[18..20], 2);
            put_be16(&mut buf[20..22], 512);
            buf
        }
        (3, _) => {
            return AtapiOutcome::Check {
                sense_key: SENSE_ILLEGAL_REQUEST,
                asc: ASC_SAVING_PARAMS_NOT_SUPPORTED,
            };
        }
        _ => {
            return AtapiOutcome::Check {
                sense_key: SENSE_ILLEGAL_REQUEST,
                asc: ASC_INVALID_FIELD_IN_CDB,
            };
        }
    };

    truncate(buf, alloc_len)
}

fn truncate(mut buf: Vec<u8>, len: usize) -> AtapiOutcome {
    buf.truncate(len);
    AtapiOutcome::Data(buf)
}

fn atapi_string(dest: &mut [u8], src: &str) {
    dest.fill(b' ');
    let len = dest.len().min(src.len());
    dest[..len].copy_from_slice(&src.as_bytes()[..len]);
}

fn put_be16(dest: &mut [u8], value: u16) {
    dest.copy_from_slice(&value.to_be_bytes());
}

fn put_be32(dest: &mut [u8], value: u32) {
    dest.copy_from_slice(&value.to_be_bytes());
}

fn lba_to_msf(lba: u64) -> [u8; 3] {
    let lba = lba.saturating_add(150);
    [
        (lba / 75 / 60) as u8,
        ((lba / 75) % 60) as u8,
        (lba % 75) as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ahci::bits::{
        ASC_INVALID_FIELD_IN_CDB, ASC_INVALID_OPCODE,
        ASC_MEDIA_REMOVAL_PREVENTED, ASC_SAVING_PARAMS_NOT_SUPPORTED,
        SENSE_ILLEGAL_REQUEST,
    };
    use crate::ahci::media::checked_read_range;

    fn cdb(bytes: &[u8]) -> [u8; 16] {
        let mut acmd = [0u8; 16];
        acmd[..bytes.len()].copy_from_slice(bytes);
        acmd
    }

    fn data(outcome: AtapiOutcome) -> Vec<u8> {
        match outcome {
            AtapiOutcome::Data(buf) => buf,
            other => panic!("expected data, got {other:?}"),
        }
    }

    #[test]
    fn inquiry_reports_removable_cdrom() {
        let mut sense = SenseState::default();
        let buf = data(dispatch(&cdb(&[0x12, 0, 0, 0, 36]), 1, &mut sense));

        assert_eq!(buf[0], 0x05);
        assert_eq!(buf[1], 0x80);
        assert_eq!(buf[4], 31);
        assert_eq!(&buf[8..16], b"BHYVE   ");
        assert_eq!(&buf[16..32], b"BHYVE DVD-ROM   ");
    }

    #[test]
    fn inquiry_truncates_to_alloc_len() {
        let mut sense = SenseState::default();
        let buf = data(dispatch(&cdb(&[0x12, 0, 0, 0, 5]), 1, &mut sense));

        assert_eq!(buf, vec![0x05, 0x80, 0x00, 0x21, 31]);
    }

    #[test]
    fn inquiry_vpd_page_zero() {
        let mut sense = SenseState::default();
        let buf = data(dispatch(&cdb(&[0x12, 1, 0, 0, 5]), 1, &mut sense));

        assert_eq!(buf, vec![0x05, 0, 0, 1, 0]);
    }

    #[test]
    fn inquiry_vpd_other_page_is_illegal_request() {
        let mut sense = SenseState::default();
        let outcome = dispatch(&cdb(&[0x12, 1, 0x80, 0, 5]), 1, &mut sense);

        assert_eq!(
            outcome,
            AtapiOutcome::Check {
                sense_key: SENSE_ILLEGAL_REQUEST,
                asc: ASC_INVALID_FIELD_IN_CDB,
            }
        );
    }

    #[test]
    fn read_capacity_reports_last_lba_and_2048() {
        let mut sense = SenseState::default();
        let buf = data(dispatch(&cdb(&[0x25]), 32_256, &mut sense));

        assert_eq!(u32::from_be_bytes(buf[0..4].try_into().unwrap()), 32_255);
        assert_eq!(u32::from_be_bytes(buf[4..8].try_into().unwrap()), 2048);
    }

    #[test]
    fn read_capacity_on_single_block_media_does_not_underflow() {
        for (blocks, last_lba) in [(1, 0), (0, 0)] {
            let mut sense = SenseState::default();
            let buf = data(dispatch(&cdb(&[0x25]), blocks, &mut sense));

            assert_eq!(
                u32::from_be_bytes(buf[0..4].try_into().unwrap()),
                last_lba
            );
        }
    }

    #[test]
    fn read10_decodes_be_lba_and_count() {
        let mut sense = SenseState::default();
        let outcome = dispatch(
            &cdb(&[0x28, 0, 0x12, 0x34, 0x56, 0x78, 0, 0x9a, 0xbc]),
            u64::MAX,
            &mut sense,
        );

        assert_eq!(
            outcome,
            AtapiOutcome::ReadBlocks {
                lba: 0x1234_5678,
                count: 0x9abc,
            }
        );
    }

    #[test]
    fn read12_decodes_32bit_count() {
        let mut sense = SenseState::default();
        let outcome = dispatch(
            &cdb(&[0xa8, 0, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0]),
            u64::MAX,
            &mut sense,
        );

        assert_eq!(
            outcome,
            AtapiOutcome::ReadBlocks {
                lba: 0x1234_5678,
                count: 0x9abc_def0,
            }
        );
    }

    #[test]
    fn read12_huge_count_is_returned_verbatim_and_rejected_downstream() {
        let mut sense = SenseState::default();
        let outcome = dispatch(
            &cdb(&[0xa8, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]),
            u64::MAX,
            &mut sense,
        );

        assert_eq!(
            outcome,
            AtapiOutcome::ReadBlocks {
                lba: u64::from(u32::MAX),
                count: u32::MAX,
            }
        );
        assert_eq!(
            checked_read_range(u64::from(u32::MAX), u32::MAX, u64::MAX),
            None,
        );
    }

    #[test]
    fn read_toc_format0_lengths() {
        let mut sense = SenseState::default();
        let full = data(dispatch(
            &cdb(&[0x43, 0, 0, 0, 0, 0, 0, 0, 20]),
            32_256,
            &mut sense,
        ));
        let leadout_only = data(dispatch(
            &cdb(&[0x43, 0, 0, 0, 0, 0, 0xaa, 0, 20]),
            32_256,
            &mut sense,
        ));
        let invalid = dispatch(
            &cdb(&[0x43, 0, 0, 0, 0, 0, 2, 0, 20]),
            32_256,
            &mut sense,
        );

        assert_eq!(full.len(), 20);
        assert_eq!(u16::from_be_bytes([full[0], full[1]]), 18);
        assert_eq!(leadout_only.len(), 12);
        assert_eq!(u16::from_be_bytes([leadout_only[0], leadout_only[1]]), 10);
        assert_eq!(
            invalid,
            AtapiOutcome::Check {
                sense_key: SENSE_ILLEGAL_REQUEST,
                asc: ASC_INVALID_FIELD_IN_CDB,
            }
        );
    }

    #[test]
    fn read_toc_format0_leadout_is_media_size() {
        let mut sense = SenseState::default();
        let blocks = 32_256;
        let buf = data(dispatch(
            &cdb(&[0x43, 0, 0, 0, 0, 0, 0, 0, 20]),
            blocks,
            &mut sense,
        ));

        assert_eq!(
            u32::from_be_bytes(buf[buf.len() - 4..].try_into().unwrap()),
            blocks as u32,
        );
    }

    #[test]
    fn read_toc_msf_encoding() {
        assert_eq!(lba_to_msf(0), [0, 2, 0]);
    }

    #[test]
    fn read_toc_format1_layout() {
        let mut sense = SenseState::default();
        let buf = data(dispatch(
            &cdb(&[0x43, 0, 0, 0, 0, 0, 0, 0, 12, 1 << 6]),
            32_256,
            &mut sense,
        ));

        assert_eq!(buf, vec![0, 0x0a, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn read_toc_format2_is_rejected() {
        let mut sense = SenseState::default();
        let outcome = dispatch(
            &cdb(&[0x43, 0, 0, 0, 0, 0, 0, 0, 20, 2 << 6]),
            32_256,
            &mut sense,
        );

        assert_eq!(
            outcome,
            AtapiOutcome::Check {
                sense_key: SENSE_ILLEGAL_REQUEST,
                asc: ASC_INVALID_FIELD_IN_CDB,
            }
        );
    }

    #[test]
    fn gesn_requires_polled_bit() {
        let mut sense = SenseState::default();
        let asynchronous =
            dispatch(&cdb(&[0x4a, 0, 0, 0, 0, 0, 0, 0, 8]), 1, &mut sense);
        let polled = data(dispatch(
            &cdb(&[0x4a, 1, 0, 0, 0, 0, 0, 0, 8]),
            1,
            &mut sense,
        ));

        assert_eq!(
            asynchronous,
            AtapiOutcome::Check {
                sense_key: SENSE_ILLEGAL_REQUEST,
                asc: ASC_INVALID_FIELD_IN_CDB,
            }
        );
        assert_eq!(polled.len(), 8);
        assert_eq!(polled[2], 0x04);
        assert_eq!(polled[3], 0x10);
        assert_eq!(polled[5], 0x02);
    }

    #[test]
    fn mode_sense_page01_layout() {
        let mut sense = SenseState::default();
        let buf = data(dispatch(
            &cdb(&[0x5a, 0, 0x01, 0, 0, 0, 0, 0, 16]),
            1,
            &mut sense,
        ));

        assert_eq!(buf.len(), 16);
        assert_eq!(u16::from_be_bytes([buf[0], buf[1]]), 14);
        assert_eq!(buf[2], 0x70);
        assert_eq!(&buf[8..12], &[0x01, 6, 0, 0x05]);
    }

    #[test]
    fn mode_sense_page2a_layout() {
        let mut sense = SenseState::default();
        let buf = data(dispatch(
            &cdb(&[0x5a, 0, 0x2a, 0, 0, 0, 0, 0, 30]),
            1,
            &mut sense,
        ));

        assert_eq!(buf.len(), 30);
        assert_eq!(u16::from_be_bytes([buf[0], buf[1]]), 28);
        assert_eq!(buf[2], 0x70);
        assert_eq!(buf[12], 0x71);
        assert_eq!(u16::from_be_bytes([buf[18], buf[19]]), 2);
        assert_eq!(u16::from_be_bytes([buf[20], buf[21]]), 512);
    }

    #[test]
    fn mode_sense_page3f_is_illegal_request() {
        let mut sense = SenseState::default();
        let outcome =
            dispatch(&cdb(&[0x5a, 0, 0x3f, 0, 0, 0, 0, 0, 30]), 1, &mut sense);

        assert_eq!(
            outcome,
            AtapiOutcome::Check {
                sense_key: SENSE_ILLEGAL_REQUEST,
                asc: ASC_INVALID_FIELD_IN_CDB,
            }
        );
    }

    #[test]
    fn mode_sense_pc3_returns_asc_39() {
        let mut sense = SenseState::default();
        let outcome =
            dispatch(&cdb(&[0x5a, 0, 0xc1, 0, 0, 0, 0, 0, 16]), 1, &mut sense);

        assert_eq!(
            outcome,
            AtapiOutcome::Check {
                sense_key: SENSE_ILLEGAL_REQUEST,
                asc: ASC_SAVING_PARAMS_NOT_SUPPORTED,
            }
        );
    }

    #[test]
    fn request_sense_reports_last_error() {
        let mut sense = SenseState::default();
        assert_eq!(
            dispatch(&cdb(&[0xff]), 1, &mut sense),
            AtapiOutcome::Check {
                sense_key: SENSE_ILLEGAL_REQUEST,
                asc: ASC_INVALID_OPCODE,
            }
        );

        let buf = data(dispatch(&cdb(&[0x03, 0, 0, 0, 64]), 1, &mut sense));
        assert_eq!(buf.len(), 64);
        assert_eq!(buf[0], 0xf0);
        assert_eq!(buf[2], SENSE_ILLEGAL_REQUEST);
        assert_eq!(buf[7], 10);
        assert_eq!(buf[12], ASC_INVALID_OPCODE);
    }

    #[test]
    fn request_sense_honours_short_alloc_len() {
        let mut sense = SenseState {
            key: SENSE_ILLEGAL_REQUEST,
            asc: ASC_INVALID_OPCODE,
        };
        let buf = data(dispatch(&cdb(&[0x03, 0, 0, 0, 4]), 1, &mut sense));

        assert_eq!(buf, vec![0xf0, 0, SENSE_ILLEGAL_REQUEST, 0]);
    }

    #[test]
    fn unknown_opcode_is_asc_20() {
        for opcode in [0x46, 0x51, 0xbd, 0xbe] {
            let mut sense = SenseState::default();
            assert_eq!(
                dispatch(&cdb(&[opcode]), 1, &mut sense),
                AtapiOutcome::Check {
                    sense_key: SENSE_ILLEGAL_REQUEST,
                    asc: ASC_INVALID_OPCODE,
                }
            );
        }
    }

    #[test]
    fn start_stop_unit_eject_is_rejected() {
        let mut sense = SenseState::default();
        let outcome = dispatch(&cdb(&[0x1b, 0, 0, 0, 2]), 1, &mut sense);

        assert_eq!(
            outcome,
            AtapiOutcome::Check {
                sense_key: SENSE_ILLEGAL_REQUEST,
                asc: ASC_MEDIA_REMOVAL_PREVENTED,
            }
        );
    }

    #[test]
    fn test_unit_ready_and_prevent_allow_are_ok() {
        let mut sense = SenseState::default();

        assert_eq!(dispatch(&cdb(&[0x00]), 1, &mut sense), AtapiOutcome::Ok);
        assert_eq!(dispatch(&cdb(&[0x1e]), 1, &mut sense), AtapiOutcome::Ok);
    }

    #[test]
    fn report_luns_reports_one_lun() {
        let mut sense = SenseState::default();
        let buf = data(dispatch(&cdb(&[0xa0]), 1, &mut sense));

        assert_eq!(buf.len(), 16);
        assert_eq!(buf[3], 8);
        assert_eq!(buf.iter().filter(|byte| **byte != 0).count(), 1);
    }
}
