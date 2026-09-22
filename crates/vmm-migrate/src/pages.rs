// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Validated guest-memory page transfer helpers.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use vmm_core::mem::MemCtx;

use crate::codec::{MigrateError, PAGE_BATCH_FLAG_ZSTD};
use crate::limits;
use crate::protocol::PAGE_SIZE;
use crate::MigrationStatus;

pub fn apply_page_batch(
    memctx: &MemCtx,
    base_gpa: u64,
    page_count: u32,
    flags: u32,
    data: &[u8],
    sparse: Option<&[u64]>,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    if page_count == 0 || page_count > limits::MAX_PAGES_PER_BATCH {
        return Err(MigrateError::Codec(format!(
            "invalid PageBatch page count: {page_count}",
        )));
    }
    if flags & !PAGE_BATCH_FLAG_ZSTD != 0 {
        return Err(MigrateError::Codec(format!(
            "unknown PageBatch flags: {flags:#x}",
        )));
    }

    let expected =
        (page_count as usize)
            .checked_mul(PAGE_SIZE)
            .ok_or_else(|| {
                MigrateError::Codec("PageBatch byte count overflow".to_string())
            })?;

    let raw = if flags & PAGE_BATCH_FLAG_ZSTD != 0 {
        if data.len() > limits::MAX_BATCH_COMPRESSED_BYTES {
            return Err(MigrateError::Codec(format!(
                "compressed PageBatch too large: {} bytes",
                data.len(),
            )));
        }
        Cow::Owned(zstd::bulk::decompress(data, expected).map_err(|e| {
            MigrateError::Codec(format!("zstd decompress: {e}"))
        })?)
    } else {
        if data.len() != expected {
            return Err(MigrateError::Codec(format!(
                "PageBatch size mismatch: expected {expected}, got {}",
                data.len(),
            )));
        }
        Cow::Borrowed(data)
    };

    if raw.len() != expected {
        return Err(MigrateError::Codec(format!(
            "PageBatch size mismatch: expected {expected}, got {}",
            raw.len(),
        )));
    }

    let contiguous_gpas;
    let gpas = match sparse {
        Some(gpas) => {
            if gpas.len() != page_count as usize {
                return Err(MigrateError::Codec(format!(
                    "sparse GPA count mismatch: expected {}, got {}",
                    page_count,
                    gpas.len(),
                )));
            }
            gpas
        }
        None => {
            contiguous_gpas =
                build_contiguous_gpas(base_gpa, page_count as usize)?;
            &contiguous_gpas
        }
    };

    // Preflight every destination before the first guest-memory mutation.
    // A zero-length SubMapping write checks protection without changing RAM.
    for &gpa in gpas {
        let mapping = memctx.lookup(gpa, PAGE_SIZE).ok_or_else(|| {
            MigrateError::Io(format!(
                "guest memory write gpa={gpa:#x}: page is not mapped",
            ))
        })?;
        mapping.write_bytes(&[]).map_err(|e| {
            MigrateError::Io(format!("guest memory write gpa={gpa:#x}: {e}",))
        })?;
    }

    for (&gpa, page) in gpas.iter().zip(raw.as_chunks::<PAGE_SIZE>().0) {
        memctx.write(gpa, page).map_err(|e| {
            MigrateError::Io(format!("guest memory write gpa={gpa:#x}: {e}",))
        })?;
    }

    if let Ok(mut status) = status.lock() {
        status.pages_transferred += page_count as u64;
        status.bytes_transferred += expected as u64;
    }

    Ok(())
}

pub fn read_pages(
    memctx: &MemCtx,
    gpas: &[u64],
    buf: &mut [u8],
) -> Result<(), MigrateError> {
    let expected = page_buffer_len(gpas.len())?;
    if buf.len() != expected {
        return Err(MigrateError::Codec(format!(
            "page read buffer size mismatch: expected {expected}, got {}",
            buf.len(),
        )));
    }

    for (&gpa, page) in gpas.iter().zip(buf.as_chunks_mut::<PAGE_SIZE>().0) {
        memctx.read(gpa, page).map_err(|e| {
            MigrateError::Io(format!("guest memory read gpa={gpa:#x}: {e}",))
        })?;
    }

    Ok(())
}

pub(crate) fn page_buffer_len(
    page_count: usize,
) -> Result<usize, MigrateError> {
    if page_count == 0 || page_count > limits::MAX_PAGES_PER_BATCH as usize {
        return Err(MigrateError::Codec(format!(
            "invalid page count: {page_count}",
        )));
    }
    page_count.checked_mul(PAGE_SIZE).ok_or_else(|| {
        MigrateError::Codec("page byte count overflow".to_string())
    })
}

pub(crate) fn build_contiguous_gpas(
    base_gpa: u64,
    page_count: usize,
) -> Result<Vec<u64>, MigrateError> {
    page_buffer_len(page_count)?;
    let mut gpas = Vec::with_capacity(page_count);
    for i in 0..page_count {
        let offset =
            (i as u64).checked_mul(PAGE_SIZE as u64).ok_or_else(|| {
                MigrateError::Codec("page GPA offset overflow".to_string())
            })?;
        let gpa = base_gpa.checked_add(offset).ok_or_else(|| {
            MigrateError::Codec("page GPA overflow".to_string())
        })?;
        gpas.push(gpa);
    }
    Ok(gpas)
}

pub(crate) fn are_contiguous(gpas: &[u64]) -> bool {
    let Some(&base_gpa) = gpas.first() else {
        return false;
    };
    gpas.iter().enumerate().all(|(i, &gpa)| {
        (i as u64)
            .checked_mul(PAGE_SIZE as u64)
            .and_then(|offset| base_gpa.checked_add(offset))
            == Some(gpa)
    })
}
