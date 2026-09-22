// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Resource limits for the migration transport.

use crate::protocol::PAGE_SIZE;

pub const MAX_PAGES_PER_BATCH: u32 = 256;
pub const MAX_BATCH_RAW_BYTES: usize = MAX_PAGES_PER_BATCH as usize * PAGE_SIZE;
pub const MAX_BATCH_COMPRESSED_BYTES: usize =
    MAX_BATCH_RAW_BYTES + MAX_BATCH_RAW_BYTES / 8 + 4096;
pub const MAX_SPARSE_GPAS: usize = MAX_PAGES_PER_BATCH as usize;
/// Pages per PageBatch message on the source.
pub const BATCH_SIZE: usize = 64;
const _: () = assert!(BATCH_SIZE <= MAX_PAGES_PER_BATCH as usize);
/// zstd compression level (1 = fastest, good ratio for RAM).
pub const ZSTD_LEVEL: i32 = 1;
pub const MAX_SERIALIZED_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_FRAME_BYTES: usize = MAX_SERIALIZED_BYTES + 4096;

pub fn ws_config() -> tungstenite::protocol::WebSocketConfig {
    tungstenite::protocol::WebSocketConfig {
        max_message_size: Some(MAX_FRAME_BYTES),
        max_frame_size: Some(MAX_FRAME_BYTES),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_config_uses_migration_frame_limit() {
        let config = ws_config();
        assert_eq!(config.max_message_size, Some(MAX_FRAME_BYTES));
        assert_eq!(config.max_frame_size, Some(MAX_FRAME_BYTES));
    }
}
