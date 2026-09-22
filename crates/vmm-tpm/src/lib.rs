// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Virtual TPM 2.0 device.
//!
//! Backend: vendored libtpms (TCG-compliant TPM 2.0 emulator) running
//! in-process via the `vmm_tpm_sys` FFI.
//! Frontend: TCG PTP 2.0 Command Response Buffer (CRB) over MMIO at
//! 0xFED40000, advertised via a TPM2 ACPI table.
//!
//! End-to-end: a Windows 11 / Server 2022 (or modern Linux) guest
//! parses the TPM2 ACPI table, finds the Control Area, drives the
//! CRB, and gets responses backed by libtpms with NV state persisted
//! to a per-VM directory on the host filesystem.
//!
//! # libtpms is not thread-safe
//!
//! libtpms holds the whole TPM in C globals (the TCG reference code's
//! `g_*`/`s_*` state and the NV image) and takes no locks. Two threads
//! in `TPMLIB_Process` would corrupt that state and race the NV files.
//! It is also a single instance per process, so there is one [`Tpm`] per
//! process and one VM per process.
//!
//! [`Crb`] is the only caller. It serialises commands on its own mutex,
//! because a guest drives it from any vCPU. Any other caller into
//! libtpms must hold that same lock.

pub mod acpi;
mod callbacks;
pub mod crb;

use std::path::PathBuf;
use std::sync::Arc;

use slog::{info, Logger};

pub use crate::acpi::build_tpm2_table;
pub use crate::crb::{Crb, CRB_BASE, CRB_REGION_LEN};

use vmm_tpm_sys::{
    TPMLIB_ChooseTPMVersion, TPMLIB_MainInit, TPMLIB_SetBufferSize,
    TPMLIB_TPMVersion, TPM_SUCCESS,
};

use crate::crb::BUF_LEN;

/// Top-level TPM device. It owns the CRB frontend. The libtpms backend
/// is process-global state, initialized once at construction.
pub struct Tpm {
    pub crb: Arc<Crb>,
}

impl Tpm {
    /// Set up libtpms (TPM 2.0, file-backed NV in `state_dir`) and
    /// instantiate the CRB frontend. Call exactly once per process,
    /// because libtpms is single-instance.
    ///
    /// `state_dir` is created if it does not exist.
    pub fn new(state_dir: PathBuf, log: Logger) -> Result<Self, TpmError> {
        callbacks::register(state_dir.clone(), log.clone())
            .map_err(TpmError::CallbackInit)?;

        // SAFETY: libtpms takes no arguments here and this is the only
        // caller in the process. Order is fixed by the library: choose
        // the version, size the buffers, then initialise.
        let rc = unsafe {
            TPMLIB_ChooseTPMVersion(TPMLIB_TPMVersion::TPMLIB_TPM_VERSION_2)
        };
        if rc != TPM_SUCCESS {
            return Err(TpmError::Libtpms("ChooseTPMVersion", rc));
        }

        // Without this the library keeps its 4096-byte default, larger
        // than the CRB buffer, and would build responses that do not
        // fit. Telling it the real size makes it enforce the limit.
        let wanted = u32::try_from(BUF_LEN).expect("CRB buffer fits in u32");
        let mut min = 0u32;
        let mut max = 0u32;
        let negotiated =
            // SAFETY: both out-parameters point at live locals.
            unsafe { TPMLIB_SetBufferSize(wanted, &mut min, &mut max) };
        if negotiated != wanted {
            return Err(TpmError::BufferSize {
                wanted,
                negotiated,
                min,
                max,
            });
        }

        // SAFETY: the callbacks are registered and the version chosen.
        let rc = unsafe { TPMLIB_MainInit() };
        if rc != TPM_SUCCESS {
            return Err(TpmError::Libtpms("MainInit", rc));
        }
        info!(log, "vTPM 2.0 ready";
            "state_dir" => %state_dir.display(), "buffer" => negotiated);

        Ok(Self {
            crb: Arc::new(Crb::new(log)),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TpmError {
    #[error("vTPM callback registration failed: {0}")]
    CallbackInit(#[source] std::io::Error),
    #[error("libtpms {0} returned rc={1:#x}")]
    Libtpms(&'static str, u32),
    #[error(
        "libtpms would not take a {wanted}-byte buffer: got {negotiated}, \
         limits {min}..={max}"
    )]
    BufferSize {
        wanted: u32,
        negotiated: u32,
        min: u32,
        max: u32,
    },
}
