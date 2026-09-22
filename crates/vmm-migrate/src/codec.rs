// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Migration message codec over WebSocket binary frames.
//!
//! Structured data is RON text. Memory pages are raw binary.
//!
//! # Wire format
//!
//! Each WebSocket binary frame is `[payload...][tag: u8]`. The tag is
//! the last byte and identifies the [`MessageType`]. The frame length
//! gives the payload length, so there is no length prefix. All integers
//! are little-endian.

use serde::{Deserialize, Serialize};

use crate::limits;

/// Message types in the migration protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    /// Acknowledgement. Empty payload.
    Okay = 0,
    /// Failure. Payload: a RON-serialized [`MigrateError`].
    Error = 1,
    /// Payload: RON-serialized structured data.
    Serialized = 2,
    /// Payload: a RON list of the GPAs of the next PageBatch. Sent
    /// only when those pages are not contiguous.
    MemFetch = 7,
    /// End of a memory transfer phase. Empty payload.
    MemEnd = 9,
    /// Acknowledges the end of memory transfer. Empty payload.
    MemDone = 10,
    /// Batch of memory pages.
    ///
    /// Payload: `[base_gpa: u64][page_count: u32][flags: u32][data...]`.
    /// Flags bit 0 ([`PAGE_BATCH_FLAG_ZSTD`]): `data` is zstd-compressed.
    PageBatch = 11,
    /// The source paused the VM. The next RAM pass is the final one.
    /// Empty payload.
    PauseSignal = 12,
}

/// PageBatch flag: data payload is zstd-compressed.
pub const PAGE_BATCH_FLAG_ZSTD: u32 = 1 << 0;

impl MessageType {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Okay),
            1 => Some(Self::Error),
            2 => Some(Self::Serialized),
            7 => Some(Self::MemFetch),
            9 => Some(Self::MemEnd),
            10 => Some(Self::MemDone),
            11 => Some(Self::PageBatch),
            12 => Some(Self::PauseSignal),
            _ => None,
        }
    }
}

/// Errors that can occur during migration.
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
pub enum MigrateError {
    #[error("protocol negotiation failed: {0}")]
    ProtocolMismatch(String),
    #[error("preamble mismatch: {0}")]
    PreambleMismatch(String),
    #[error("unexpected message: expected {expected}, got {got}")]
    UnexpectedMessage { expected: String, got: String },
    #[error("codec error: {0}")]
    Codec(String),
    #[error("websocket error: {0}")]
    WebSocket(String),
    #[error("I/O error: {0}")]
    Io(String),
    #[error("vmm-data error: {0}")]
    VmmData(String),
    #[error("state import/export error: {0}")]
    State(String),
    #[error("remote error: {0}")]
    RemoteError(String),
    #[error("phase error: {0}")]
    Phase(String),
    /// The source handed the guest over and then lost the peer.
    ///
    /// The source cannot take the guest back, because the destination
    /// may run it. The VM stays paused until an operator decides.
    #[error("handed over but unconfirmed: {0}")]
    Committed(String),
}

impl From<std::io::Error> for MigrateError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<tungstenite::Error> for MigrateError {
    fn from(e: tungstenite::Error) -> Self {
        Self::WebSocket(e.to_string())
    }
}

impl From<ron::Error> for MigrateError {
    fn from(e: ron::Error) -> Self {
        Self::Codec(format!("RON deserialize: {e}"))
    }
}

impl From<ron::error::SpannedError> for MigrateError {
    fn from(e: ron::error::SpannedError) -> Self {
        Self::Codec(format!("RON deserialize: {e}"))
    }
}

/// Time synchronization data for TSC and boot hrtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeData {
    /// Raw bytes of vdi_time_info_v1.
    pub vmm_time: Vec<u8>,
}

/// Aggregated device state for a single Serialized message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceState {
    /// Per-vCPU state payloads, indexed by vCPU ID.
    pub vcpus: Vec<VcpuStatePayload>,
    /// System device state payloads (IOAPIC, ATPIT, etc.).
    pub devices: Vec<DevicePayload>,
    /// Emulated-device state, keyed by the device's PCI address on the
    /// source. The destination restores by that address alone.
    pub emulated: Vec<DeviceStatePayload>,
    /// Hyper-V enlightenment state, if the VM has it. It is not a PCI
    /// device, so it is not in `emulated`.
    pub hyperv: Option<HypervMigrateState>,
}

/// The wire uses the device crate's own state types, so no copy occurs
/// between export and encode.
pub use vmm_devices::lifecycle::{
    DeviceMigrateState, DeviceStatePayload, HypervMigrateState,
    MigratePciState, WireBdf,
};

/// State data for a single vCPU.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VcpuStatePayload {
    pub vcpuid: i32,
    /// VDC_REGISTER data.
    pub registers: Vec<u8>,
    /// VDC_MSR data.
    pub msrs: Vec<u8>,
    /// VDC_FPU data.
    pub fpu: Vec<u8>,
    /// VDC_LAPIC data.
    pub lapic: Vec<u8>,
    /// VDC_VMM_ARCH data.
    pub vmm_arch: Vec<u8>,
    /// Run state (VRS_HALT, VRS_INIT, VRS_RUN, etc.).
    #[serde(default = "default_run_state")]
    pub run_state: u32,
    /// SIPI vector, for APs in INIT/SIPI wait.
    #[serde(default)]
    pub sipi_vector: u8,
}

fn default_run_state() -> u32 {
    bhyve_api::VRS_RUN
}

/// A named system device state blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevicePayload {
    /// Device identifier (e.g., "ioapic", "atpit").
    pub name: String,
    /// VDC class constant.
    pub class: u16,
    /// VDC version used for the data.
    pub version: u16,
    /// Raw device state bytes.
    pub data: Vec<u8>,
}

/// Migration protocol message.
///
/// Each variant has one [`MessageType`] tag. [`Message::encode`] and
/// [`Message::decode`] convert to and from WebSocket binary frames.
#[derive(Debug, Clone)]
pub enum Message {
    Okay,
    Error(MigrateError),
    /// RON text: protocol offer, preamble, time data or device state.
    Serialized(Vec<u8>),
    /// The GPAs of the next PageBatch. Sent only when the batch's pages
    /// are not contiguous.
    MemFetch(Vec<u64>),
    MemEnd,
    MemDone,
    PageBatch {
        /// GPA of the first page. Ignored when a MemFetch precedes the
        /// batch.
        base_gpa: u64,
        /// Number of 4 KiB pages, 1 to
        /// [`limits::MAX_PAGES_PER_BATCH`].
        page_count: u32,
        /// Bit 0 ([`PAGE_BATCH_FLAG_ZSTD`]): `data` is zstd-compressed.
        /// All other bits must be zero.
        flags: u32,
        data: Vec<u8>,
    },
    PauseSignal,
}

impl Message {
    /// The variant name, for an error about an unexpected message.
    ///
    /// It never includes the payload: a peer must not control log text.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Okay => "Okay",
            Self::Error(_) => "Error",
            Self::Serialized(_) => "Serialized",
            Self::MemFetch(_) => "MemFetch",
            Self::MemEnd => "MemEnd",
            Self::MemDone => "MemDone",
            Self::PageBatch { .. } => "PageBatch",
            Self::PauseSignal => "PauseSignal",
        }
    }

    /// Encode this message as `[payload...][tag: u8]`.
    pub fn encode(&self) -> tungstenite::Message {
        let (tag, payload) = match self {
            Message::Okay => (MessageType::Okay, Vec::new()),
            Message::Error(e) => {
                let data = ron::ser::to_string(e)
                    .unwrap_or_else(|_| "serialization_failed".to_string());
                (MessageType::Error, data.into_bytes())
            }
            Message::Serialized(data) => {
                (MessageType::Serialized, data.clone())
            }
            Message::MemFetch(gpas) => {
                let data = ron::ser::to_string(gpas)
                    .unwrap_or_else(|_| "[]".to_string());
                (MessageType::MemFetch, data.into_bytes())
            }
            Message::MemEnd => (MessageType::MemEnd, Vec::new()),
            Message::MemDone => (MessageType::MemDone, Vec::new()),
            Message::PageBatch {
                base_gpa,
                page_count,
                flags,
                ref data,
            } => {
                let mut payload = Vec::with_capacity(16 + data.len());
                payload.extend_from_slice(&base_gpa.to_le_bytes());
                payload.extend_from_slice(&page_count.to_le_bytes());
                payload.extend_from_slice(&flags.to_le_bytes());
                payload.extend_from_slice(data);
                (MessageType::PageBatch, payload)
            }
            Message::PauseSignal => (MessageType::PauseSignal, Vec::new()),
        };

        let mut frame = payload;
        frame.push(tag as u8);
        tungstenite::Message::Binary(frame)
    }

    /// Decode a WebSocket binary frame into a Message.
    pub fn decode(ws_msg: tungstenite::Message) -> Result<Self, MigrateError> {
        let data = match ws_msg {
            tungstenite::Message::Binary(d) => d,
            tungstenite::Message::Close(_) => {
                return Err(MigrateError::WebSocket(
                    "connection closed".to_string(),
                ));
            }
            other => {
                return Err(MigrateError::Codec(format!(
                    "expected binary frame, got {:?}",
                    other,
                )));
            }
        };

        if data.is_empty() {
            return Err(MigrateError::Codec(
                "empty frame (no tag byte)".to_string(),
            ));
        }

        let tag_byte = data[data.len() - 1];
        let payload = &data[..data.len() - 1];

        let tag = MessageType::from_u8(tag_byte).ok_or_else(|| {
            MigrateError::Codec(format!("unknown message tag: {tag_byte}"))
        })?;

        match tag {
            MessageType::Okay => Ok(Message::Okay),
            MessageType::Error => {
                let err: MigrateError =
                    ron::from_str(Self::text(payload, "Error")?)?;
                Ok(Message::Error(err))
            }
            MessageType::Serialized => {
                Ok(Message::Serialized(payload.to_vec()))
            }
            MessageType::MemFetch => {
                let gpas: Vec<u64> =
                    ron::from_str(Self::text(payload, "MemFetch")?)?;
                if gpas.len() > limits::MAX_SPARSE_GPAS {
                    return Err(MigrateError::Codec(format!(
                        "MemFetch has too many GPAs: {}",
                        gpas.len(),
                    )));
                }
                Ok(Message::MemFetch(gpas))
            }
            MessageType::MemEnd => Ok(Message::MemEnd),
            MessageType::MemDone => Ok(Message::MemDone),
            MessageType::PageBatch => {
                if payload.len() < 16 {
                    return Err(MigrateError::Codec(
                        "PageBatch too short for header".to_string(),
                    ));
                }
                let word = |at: usize| -> [u8; 4] {
                    payload[at..at + 4].try_into().expect("4 bytes")
                };
                let base_gpa = u64::from_le_bytes(
                    payload[..8].try_into().expect("8 bytes"),
                );
                let page_count = u32::from_le_bytes(word(8));
                let flags = u32::from_le_bytes(word(12));
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
                Ok(Message::PageBatch {
                    base_gpa,
                    page_count,
                    flags,
                    data: payload[16..].to_vec(),
                })
            }
            MessageType::PauseSignal => Ok(Message::PauseSignal),
        }
    }

    /// Encode a serializable value as a Serialized message.
    pub fn serialized<T: Serialize>(val: &T) -> Result<Self, MigrateError> {
        let data = ron::ser::to_string(val)
            .map_err(|e| MigrateError::Codec(format!("RON serialize: {e}")))?;
        Ok(Message::Serialized(data.into_bytes()))
    }

    /// The payload as text, for the RON decoders.
    fn text<'a>(
        payload: &'a [u8],
        what: &str,
    ) -> Result<&'a str, MigrateError> {
        std::str::from_utf8(payload).map_err(|e| {
            MigrateError::Codec(format!("invalid UTF-8 in {what}: {e}"))
        })
    }

    /// Decode the payload of a Serialized message.
    pub fn deserialize<T: for<'de> Deserialize<'de>>(
        data: &[u8],
    ) -> Result<T, MigrateError> {
        let val: T = ron::from_str(Self::text(data, "Serialized")?)?;
        Ok(val)
    }
}

/// Protocol negotiation offer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolOffer {
    /// Supported protocol strings, such as
    /// [`crate::protocol::PROTOCOL_RON`].
    pub protocols: Vec<String>,
}

/// Protocol negotiation selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolSelect {
    pub protocol: String,
}

/// One device of the source, as listed in the preamble.
///
/// The destination compares this set with its own before it accepts a
/// page. A destination with a missing disk fails the migration instead
/// of resuming a guest with a dead disk.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize,
)]
pub struct DeviceIdentity {
    pub bdf: WireBdf,
    /// The device's `type_name()`. Two devices of different kinds at
    /// one address do not match.
    pub kind: String,
}

impl std::fmt::Display for DeviceIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.kind, self.bdf)
    }
}

/// Preamble exchanged during the Sync phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPreamble {
    pub num_cpus: u32,
    pub mem_size: u64,
    /// Every device that carries migration state, sorted by address.
    pub devices: Vec<DeviceIdentity>,
    /// Features the source exposes to the guest. The destination
    /// rejects the migration if it lacks any of them.
    pub cpu_features: Option<CpuFeatures>,
}

/// The CPUID leaf values that decide which instructions a guest can
/// use. The destination must support every feature the source exposes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuFeatures {
    /// CPUID leaf 1 ECX (SSE3, SSE4.1, SSE4.2, AVX, FMA, etc.).
    pub leaf1_ecx: u32,
    /// CPUID leaf 1 EDX (SSE, SSE2, etc.).
    pub leaf1_edx: u32,
    /// CPUID leaf 7 subleaf 0 EBX (AVX2, AVX-512F, etc.).
    pub leaf7_ebx: u32,
    /// CPUID leaf 7 subleaf 0 ECX (AVX-512VBMI, etc.).
    pub leaf7_ecx: u32,
    /// CPUID leaf 7 subleaf 0 EDX (AVX-512 4VNNIW, etc.).
    pub leaf7_edx: u32,
    /// XCR0 supported bits (XSAVE components).
    pub xcr0: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: &Message) -> Message {
        let ws_msg = msg.encode();
        Message::decode(ws_msg).expect("decode should succeed")
    }

    #[test]
    fn roundtrip_okay() {
        let out = roundtrip(&Message::Okay);
        assert!(matches!(out, Message::Okay));
    }

    #[test]
    fn roundtrip_mem_end() {
        let out = roundtrip(&Message::MemEnd);
        assert!(matches!(out, Message::MemEnd));
    }

    #[test]
    fn roundtrip_mem_done() {
        let out = roundtrip(&Message::MemDone);
        assert!(matches!(out, Message::MemDone));
    }

    #[test]
    fn roundtrip_error() {
        let err = MigrateError::ProtocolMismatch("test error".to_string());
        let out = roundtrip(&Message::Error(err));
        match out {
            Message::Error(MigrateError::ProtocolMismatch(s)) => {
                assert_eq!(s, "test error");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_serialized() {
        let offer = ProtocolOffer {
            protocols: vec![crate::protocol::PROTOCOL_RON.to_string()],
        };
        let msg = Message::serialized(&offer).unwrap();
        let out = roundtrip(&msg);
        match out {
            Message::Serialized(data) => {
                let decoded: ProtocolOffer =
                    Message::deserialize(&data).unwrap();
                assert_eq!(decoded.protocols, offer.protocols);
            }
            other => panic!("expected Serialized, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_mem_fetch() {
        let gpas = vec![0x1000u64, 0x2000, 0x5000];
        let out = roundtrip(&Message::MemFetch(gpas.clone()));
        match out {
            Message::MemFetch(g) => assert_eq!(g, gpas),
            other => panic!("expected MemFetch, got {other:?}"),
        }
    }

    #[test]
    fn decode_mem_fetch_rejects_too_many_gpas() {
        let msg = Message::MemFetch(vec![0; limits::MAX_SPARSE_GPAS + 1]);
        assert!(Message::decode(msg.encode()).is_err());
    }

    #[test]
    fn decode_empty_frame_fails() {
        let ws = tungstenite::Message::Binary(Vec::new());
        let result = Message::decode(ws);
        assert!(result.is_err());
    }

    #[test]
    fn roundtrip_page_batch() {
        let data = vec![0xABu8; 4096 * 4]; // 4 pages
        let msg = Message::PageBatch {
            base_gpa: 0x10_0000,
            page_count: 4,
            flags: PAGE_BATCH_FLAG_ZSTD,
            data: data.clone(),
        };
        let out = roundtrip(&msg);
        match out {
            Message::PageBatch {
                base_gpa,
                page_count,
                flags,
                data: d,
            } => {
                assert_eq!(base_gpa, 0x10_0000);
                assert_eq!(page_count, 4);
                assert_eq!(flags, PAGE_BATCH_FLAG_ZSTD);
                assert_eq!(d, data);
            }
            other => panic!("expected PageBatch, got {other:?}"),
        }
    }

    #[test]
    fn decode_page_batch_rejects_out_of_range_page_count() {
        let msg = Message::PageBatch {
            base_gpa: 0,
            page_count: limits::MAX_PAGES_PER_BATCH + 1,
            flags: 0,
            data: Vec::new(),
        };
        assert!(Message::decode(msg.encode()).is_err());
    }

    #[test]
    fn decode_page_batch_rejects_unknown_flags() {
        let msg = Message::PageBatch {
            base_gpa: 0,
            page_count: 1,
            flags: 1 << 1,
            data: vec![0; 4096],
        };
        assert!(Message::decode(msg.encode()).is_err());
    }

    #[test]
    fn roundtrip_pause_signal() {
        let out = roundtrip(&Message::PauseSignal);
        assert!(matches!(out, Message::PauseSignal));
    }

    #[test]
    fn decode_unknown_tag_fails() {
        let ws = tungstenite::Message::Binary(vec![0xFF]);
        let result = Message::decode(ws);
        assert!(result.is_err());
    }

    #[test]
    fn roundtrip_preamble_keeps_device_identity() {
        let devices = vec![DeviceIdentity {
            bdf: WireBdf {
                bus: 0,
                dev: 5,
                func: 0,
            },
            kind: "virtio-blk".to_string(),
        }];
        let preamble = MigrationPreamble {
            num_cpus: 4,
            mem_size: 1024 * 1024 * 1024,
            devices: devices.clone(),
            cpu_features: None,
        };
        let msg = Message::serialized(&preamble).unwrap();
        match roundtrip(&msg) {
            Message::Serialized(data) => {
                let decoded: MigrationPreamble =
                    Message::deserialize(&data).unwrap();
                assert_eq!(decoded.num_cpus, 4);
                assert_eq!(decoded.mem_size, 1024 * 1024 * 1024);
                assert_eq!(decoded.devices, devices);
            }
            other => panic!("expected Serialized, got {other:?}"),
        }
    }

    #[test]
    fn a_retired_message_tag_is_refused() {
        // Tags 3, 4, 5, 6 and 8 are retired. No decoder exists for
        // them, and a peer must not reach one.
        for tag in [3u8, 4, 5, 6, 8] {
            let frame = tungstenite::Message::Binary(vec![0, tag]);
            assert!(
                Message::decode(frame).is_err(),
                "tag {tag} must not decode"
            );
        }
    }

    #[test]
    fn an_unexpected_message_is_named_by_variant_not_by_payload() {
        // A peer must not choose what goes in a log line.
        let msg = Message::Serialized(b"\n\x1b[2J".to_vec());
        assert_eq!(msg.name(), "Serialized");
    }

    #[test]
    fn device_state_carries_one_payload_per_address() {
        use vmm_devices::lifecycle::{DeviceStatePayload, VirtioMigrateState};
        let state = DeviceState {
            vcpus: Vec::new(),
            devices: Vec::new(),
            hyperv: None,
            emulated: vec![
                DeviceStatePayload {
                    bdf: WireBdf {
                        bus: 0,
                        dev: 4,
                        func: 0,
                    },
                    state: Some(DeviceMigrateState::Virtio(
                        VirtioMigrateState::default(),
                    )),
                },
                DeviceStatePayload {
                    bdf: WireBdf {
                        bus: 0,
                        dev: 5,
                        func: 0,
                    },
                    state: Some(DeviceMigrateState::Virtio(
                        VirtioMigrateState::default(),
                    )),
                },
            ],
        };
        let msg = Message::serialized(&state).unwrap();
        match roundtrip(&msg) {
            Message::Serialized(data) => {
                let decoded: DeviceState = Message::deserialize(&data).unwrap();
                let addrs: Vec<String> = decoded
                    .emulated
                    .iter()
                    .map(|d| d.bdf.to_string())
                    .collect();
                assert_eq!(addrs, ["0.4.0", "0.5.0"]);
            }
            other => panic!("expected Serialized, got {other:?}"),
        }
    }
}
