// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::{Arc, Mutex};

use vmm_core::mem::{MemCtx, PhysMap};
use vmm_migrate::codec::PAGE_BATCH_FLAG_ZSTD;
use vmm_migrate::limits::{
    MAX_BATCH_COMPRESSED_BYTES, MAX_BATCH_RAW_BYTES, MAX_PAGES_PER_BATCH,
};
use vmm_migrate::pages::{apply_page_batch, read_pages};
use vmm_migrate::protocol::PAGE_SIZE;
use vmm_migrate::MigrationStatus;

const RAM_SIZE: usize = 4 * 1024 * 1024;

fn test_memctx() -> MemCtx {
    let map = PhysMap::new_anon(0, RAM_SIZE)
        .expect("anonymous guest memory should be available");
    MemCtx::new(Arc::new(map))
}

fn test_status() -> Arc<Mutex<MigrationStatus>> {
    Arc::new(Mutex::new(MigrationStatus::default()))
}

#[test]
fn truncated_uncompressed_batch_errors_not_panics() {
    let result =
        apply_page_batch(&test_memctx(), 0, 8, 0, &[], None, &test_status());
    assert!(result.is_err());
}

#[test]
fn huge_page_count_rejected_before_allocation() {
    let page_count = MAX_PAGES_PER_BATCH + 1;
    let payload = vec![0u8; page_count as usize * PAGE_SIZE];
    let result = apply_page_batch(
        &test_memctx(),
        0,
        page_count,
        0,
        &payload,
        None,
        &test_status(),
    );
    let error = result.expect_err("oversized page count must be rejected");
    assert!(error.to_string().contains("invalid PageBatch page count"));
}

#[test]
fn decompression_bomb_rejected() {
    let expanded = vec![0u8; MAX_BATCH_RAW_BYTES + 4096];
    let compressed = zstd::bulk::compress(&expanded, 1)
        .expect("test payload should compress");
    assert!(compressed.len() <= MAX_BATCH_COMPRESSED_BYTES);

    let result = apply_page_batch(
        &test_memctx(),
        0,
        MAX_PAGES_PER_BATCH,
        PAGE_BATCH_FLAG_ZSTD,
        &compressed,
        None,
        &test_status(),
    );
    assert!(result.is_err());
}

#[test]
fn oversized_compressed_batch_rejected() {
    let mut oversized = zstd::bulk::compress(&[0u8; PAGE_SIZE], 1)
        .expect("test payload should compress");
    oversized.resize(MAX_BATCH_COMPRESSED_BYTES + 1, 0);
    let result = apply_page_batch(
        &test_memctx(),
        0,
        1,
        PAGE_BATCH_FLAG_ZSTD,
        &oversized,
        None,
        &test_status(),
    );
    let error =
        result.expect_err("oversized compressed batch must be rejected");
    assert!(error.to_string().contains("compressed PageBatch too large"));
}

#[test]
fn unknown_flag_bits_rejected() {
    let result = apply_page_batch(
        &test_memctx(),
        0,
        1,
        1 << 1,
        &[0u8; 4096],
        None,
        &test_status(),
    );
    assert!(result.is_err());
}

#[test]
fn sparse_len_must_equal_page_count() {
    let memctx = test_memctx();
    let raw = vec![0u8; 2 * 4096];

    let shorter =
        apply_page_batch(&memctx, 0, 2, 0, &raw, Some(&[0]), &test_status());
    assert!(shorter.is_err());

    let longer = apply_page_batch(
        &memctx,
        0,
        2,
        0,
        &raw,
        Some(&[0, 4096, 8192]),
        &test_status(),
    );
    assert!(longer.is_err());
}

#[test]
fn gpa_outside_ram_is_an_error_not_a_silent_skip() {
    let status = test_status();
    let result = apply_page_batch(
        &test_memctx(),
        RAM_SIZE as u64,
        1,
        0,
        &[0u8; 4096],
        None,
        &status,
    );

    assert!(result.is_err());
    let status = status.lock().expect("status lock should not be poisoned");
    assert_eq!(status.pages_transferred, 0);
    assert_eq!(status.bytes_transferred, 0);
}

#[test]
fn later_unmapped_gpa_does_not_write_earlier_page() {
    let memctx = test_memctx();
    let status = test_status();
    let mut raw = vec![0xAAu8; 4096];
    raw.extend_from_slice(&[0xBBu8; 4096]);

    let result = apply_page_batch(
        &memctx,
        0,
        2,
        0,
        &raw,
        Some(&[0, RAM_SIZE as u64]),
        &status,
    );
    assert!(result.is_err());

    let mut first = [0xFFu8; 4096];
    memctx
        .read(0, &mut first)
        .expect("first page should be mapped");
    assert_eq!(first, [0u8; 4096]);
    assert_eq!(
        status
            .lock()
            .expect("status lock should not be poisoned")
            .pages_transferred,
        0,
    );
}

#[test]
fn read_outside_ram_is_an_error() {
    let mut buf = [0u8; 4096];
    let result = read_pages(&test_memctx(), &[RAM_SIZE as u64], &mut buf);
    assert!(result.is_err());
}

#[test]
fn base_gpa_overflow_rejected() {
    let result = apply_page_batch(
        &test_memctx(),
        u64::MAX - 4095,
        2,
        0,
        &[0u8; 2 * 4096],
        None,
        &test_status(),
    );
    assert!(result.is_err());
}

#[test]
fn well_formed_batch_writes_all_pages() {
    let memctx = test_memctx();
    let status = test_status();
    let mut raw = vec![0x11u8; 4096];
    raw.extend_from_slice(&[0x22u8; 4096]);

    apply_page_batch(&memctx, 0x2000, 2, 0, &raw, None, &status)
        .expect("well-formed batch should be written");

    let mut first = [0u8; 4096];
    let mut second = [0u8; 4096];
    memctx
        .read(0x2000, &mut first)
        .expect("first page should be mapped");
    memctx
        .read(0x3000, &mut second)
        .expect("second page should be mapped");
    assert_eq!(first, [0x11u8; 4096]);
    assert_eq!(second, [0x22u8; 4096]);

    let status = status.lock().expect("status lock should not be poisoned");
    assert_eq!(status.pages_transferred, 2);
    assert_eq!(status.bytes_transferred, 2 * 4096);
}
