// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Live migration protocol and implementation.
//!
//! One driver runs over any `Read + Write` stream: a Unix socket inside
//! a bhyve zone, where `/dev/poll` is unavailable, or a TCP socket. A
//! single driver keeps the two transports from drifting apart.

pub mod codec;
pub mod cpu;
pub mod destination;
pub mod limits;
pub mod pages;
pub mod protocol;
pub mod source;
pub mod state;
pub mod wire;

/// Migration phases, executed in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationPhase {
    /// Protocol negotiation and instance spec comparison.
    Sync,
    /// Pre-pause iterative dirty page transfer.
    RamPushPrePause,
    /// Pause all vCPUs and devices.
    Pause,
    /// Transfer pages dirtied since pre-pause.
    RamPushPostPause,
    /// TSC, boot hrtime synchronization.
    TimeData,
    /// Export/import all device state.
    DeviceState,
    /// Destination requests any missing pages.
    RamPull,
    /// The destination holds all state.
    Finish,
    /// The source handed the guest over and cannot take it back.
    ///
    /// A rollback past this point runs a second copy of the guest
    /// against the same disk. The source stays paused however the rest
    /// of the exchange ends.
    Committed,
    /// Migration failed.
    Error,
}

impl std::fmt::Display for MigrationPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sync => write!(f, "sync"),
            Self::RamPushPrePause => write!(f, "ram-push-pre-pause"),
            Self::Pause => write!(f, "pause"),
            Self::RamPushPostPause => write!(f, "ram-push-post-pause"),
            Self::TimeData => write!(f, "time-data"),
            Self::DeviceState => write!(f, "device-state"),
            Self::RamPull => write!(f, "ram-pull"),
            Self::Finish => write!(f, "finish"),
            Self::Committed => write!(f, "committed"),
            Self::Error => write!(f, "error"),
        }
    }
}

/// Migration progress information.
#[derive(Debug, Clone)]
pub struct MigrationStatus {
    pub phase: MigrationPhase,
    pub bytes_transferred: u64,
    pub pages_transferred: u64,
    pub dirty_pages_remaining: u64,
}

impl Default for MigrationStatus {
    fn default() -> Self {
        Self {
            phase: MigrationPhase::Sync,
            bytes_transferred: 0,
            pages_transferred: 0,
            dirty_pages_remaining: 0,
        }
    }
}
