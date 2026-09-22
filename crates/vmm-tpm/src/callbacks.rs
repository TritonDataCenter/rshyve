// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! libtpms callback shim: file-backed NV storage + locality reporting.
//!
//! libtpms calls into the host whenever it needs to read or write the
//! TPM's non-volatile state (PERMANENT, VOLATILE, SAVESTATE blobs)
//! and to ask the active locality for an incoming command. Each NV
//! blob maps to a file in a per-VM state directory.
//!
//! The callbacks are C function pointers, registered once at TPM init
//! via `TPMLIB_RegisterCallbacks`. libtpms invokes them synchronously
//! from inside `TPMLIB_Process`, on the vCPU thread that wrote the CRB
//! START register. The CRB serialises those calls, so a callback sees
//! one command at a time. Per-VM state lives in a `OnceLock`, because
//! libtpms serves one VM per process.

use std::ffi::CStr;
use std::fs::File;
use std::io::{self, Write};
use std::mem::size_of;
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;

use slog::{debug, warn, Logger};
use vmm_tpm_sys::{TPMLIB_RegisterCallbacks, TPM_RESULT};

/// libtpms callback table layout. Mirrors `struct libtpms_callbacks`
/// in `tpm_library.h.in`. Order and field count must match exactly.
/// `size_of_struct` tells the library how large the table is.
#[repr(C)]
pub struct LibtpmsCallbacks {
    pub size_of_struct: i32,
    pub tpm_nvram_init: Option<unsafe extern "C" fn() -> TPM_RESULT>,
    pub tpm_nvram_loaddata: Option<
        unsafe extern "C" fn(
            data: *mut *mut u8,
            length: *mut u32,
            tpm_number: u32,
            name: *const c_char,
        ) -> TPM_RESULT,
    >,
    pub tpm_nvram_storedata: Option<
        unsafe extern "C" fn(
            data: *const u8,
            length: u32,
            tpm_number: u32,
            name: *const c_char,
        ) -> TPM_RESULT,
    >,
    pub tpm_nvram_deletename: Option<
        unsafe extern "C" fn(
            tpm_number: u32,
            name: *const c_char,
            must_exist: u8, // TPM_BOOL
        ) -> TPM_RESULT,
    >,
    pub tpm_io_init: Option<unsafe extern "C" fn() -> TPM_RESULT>,
    pub tpm_io_getlocality: Option<
        unsafe extern "C" fn(locality: *mut u32, tpm_number: u32) -> TPM_RESULT,
    >,
    pub tpm_io_getphysicalpresence: Option<
        unsafe extern "C" fn(present: *mut u8, tpm_number: u32) -> TPM_RESULT,
    >,
}

/// libtpms response codes used here.
const TPM_FAIL: TPM_RESULT = 9;
/// libtpms uses `TPM_RETRY` from `tpm_nvram_loaddata` to signal
/// "no stored blob; first-time startup". The library responds by
/// manufacturing fresh state on the next `MainInit`. Any other
/// non-zero code is a hard failure and aborts startup.
const TPM_RETRY: TPM_RESULT = 0x800;

/// Per-process callback state. Set once during TPM device construction.
struct CallbackState {
    state_dir: PathBuf,
    /// Currently-active locality (0-4). The CRB frontend updates this
    /// when the guest acquires/releases a locality. libtpms reads it
    /// via `tpm_io_getlocality` for every command.
    locality: AtomicU32,
    log: Logger,
}

static STATE: OnceLock<CallbackState> = OnceLock::new();

/// Install the callback table in libtpms. Call once before
/// `TPMLIB_MainInit`. Returns an error if libtpms rejects the table,
/// which usually means `LibtpmsCallbacks` does not match the C
/// declaration.
pub fn register(state_dir: PathBuf, log: Logger) -> Result<(), io::Error> {
    std::fs::create_dir_all(&state_dir).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("creating TPM state dir {}: {}", state_dir.display(), e),
        )
    })?;

    STATE
        .set(CallbackState {
            state_dir,
            locality: AtomicU32::new(0),
            log,
        })
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "TPM callbacks already registered",
            )
        })?;

    let mut cbs = LibtpmsCallbacks {
        size_of_struct: size_of::<LibtpmsCallbacks>() as i32,
        tpm_nvram_init: Some(cb_nvram_init),
        tpm_nvram_loaddata: Some(cb_nvram_loaddata),
        tpm_nvram_storedata: Some(cb_nvram_storedata),
        tpm_nvram_deletename: Some(cb_nvram_deletename),
        tpm_io_init: Some(cb_io_init),
        tpm_io_getlocality: Some(cb_io_getlocality),
        tpm_io_getphysicalpresence: Some(cb_io_getphysicalpresence),
    };

    // SAFETY: libtpms copies at most `size_of_struct` bytes out of the
    // table and keeps no pointer to it.
    let rc = unsafe {
        TPMLIB_RegisterCallbacks(std::ptr::from_mut(&mut cbs).cast())
    };
    if rc != 0 {
        return Err(io::Error::other(format!(
            "TPMLIB_RegisterCallbacks failed: {:#x}",
            rc
        )));
    }
    Ok(())
}

/// Update the active locality. Called by the CRB frontend when the
/// guest writes the LOC_CTRL register. libtpms reads this back via
/// `tpm_io_getlocality` on every command.
pub fn set_locality(locality: u8) {
    if let Some(s) = STATE.get() {
        s.locality.store(locality as u32, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------
// Callback implementations
// ---------------------------------------------------------------

unsafe extern "C" fn cb_nvram_init() -> TPM_RESULT {
    // `register` already created the state dir.
    0
}

unsafe extern "C" fn cb_io_init() -> TPM_RESULT {
    0
}

unsafe extern "C" fn cb_io_getphysicalpresence(
    present: *mut u8,
    _tpm_number: u32,
) -> TPM_RESULT {
    // A VM has no physical presence button.
    if !present.is_null() {
        unsafe { *present = 0 };
    }
    0
}

unsafe extern "C" fn cb_io_getlocality(
    locality: *mut u32,
    _tpm_number: u32,
) -> TPM_RESULT {
    let s = match STATE.get() {
        Some(s) => s,
        None => return TPM_FAIL,
    };
    if !locality.is_null() {
        unsafe { *locality = s.locality.load(Ordering::SeqCst) };
    }
    0
}

unsafe extern "C" fn cb_nvram_loaddata(
    data: *mut *mut u8,
    length: *mut u32,
    _tpm_number: u32,
    name: *const c_char,
) -> TPM_RESULT {
    let s = match STATE.get() {
        Some(s) => s,
        None => return TPM_FAIL,
    };
    let path = match nv_path(s, name) {
        Some(p) => p,
        None => return TPM_FAIL,
    };

    match std::fs::read(&path) {
        Ok(buf) => {
            // libtpms takes ownership and frees the buffer with libc
            // free, so it must come from libc malloc, not from Vec.
            let len = buf.len();
            if len == 0 {
                debug!(s.log, "tpm nv loaddata: empty file"; "path" => %path.display());
                unsafe {
                    *data = std::ptr::null_mut();
                    *length = 0;
                }
                return TPM_RETRY;
            }
            let p = unsafe { libc::malloc(len) } as *mut u8;
            if p.is_null() {
                return TPM_FAIL;
            }
            unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), p, len) };
            unsafe {
                *data = p;
                *length = len as u32;
            }
            debug!(s.log, "tpm nv loaddata"; "name" => display_name(name), "bytes" => len);
            0
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            debug!(s.log, "tpm nv loaddata: no file (fresh state)"; "name" => display_name(name));
            // Nothing stored. libtpms manufactures fresh TPM state on
            // the next MainInit.
            unsafe {
                *data = std::ptr::null_mut();
                *length = 0;
            }
            TPM_RETRY
        }
        Err(e) => {
            warn!(s.log, "tpm nv loaddata failed";
                "path" => %path.display(),
                "error" => %e
            );
            TPM_FAIL
        }
    }
}

unsafe extern "C" fn cb_nvram_storedata(
    data: *const u8,
    length: u32,
    _tpm_number: u32,
    name: *const c_char,
) -> TPM_RESULT {
    let s = match STATE.get() {
        Some(s) => s,
        None => return TPM_FAIL,
    };
    let path = match nv_path(s, name) {
        Some(p) => p,
        None => return TPM_FAIL,
    };

    let bytes = if data.is_null() || length == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(data, length as usize) }
    };

    // Rename prevents partial blobs, but file and directory syncs are needed
    // to survive host failure. Lost TPM NV state can make a BitLocker VMK
    // unrecoverable.
    if let Err(e) = durable_replace(&path, bytes) {
        warn!(s.log, "tpm nv storedata failed";
            "path" => %path.display(),
            "error" => %e
        );
        return TPM_FAIL;
    }
    debug!(s.log, "tpm nv storedata"; "name" => display_name(name), "bytes" => length);
    0
}

unsafe extern "C" fn cb_nvram_deletename(
    _tpm_number: u32,
    name: *const c_char,
    must_exist: u8,
) -> TPM_RESULT {
    let s = match STATE.get() {
        Some(s) => s,
        None => return TPM_FAIL,
    };
    let path = match nv_path(s, name) {
        Some(p) => p,
        None => return TPM_FAIL,
    };
    match std::fs::remove_file(&path) {
        Ok(()) => 0,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // libtpms sets mustExist when it expects the file. Only
            // then is a missing file an error.
            if must_exist != 0 {
                TPM_FAIL
            } else {
                0
            }
        }
        Err(e) => {
            warn!(s.log, "tpm nv deletename failed";
                "path" => %path.display(),
                "error" => %e
            );
            TPM_FAIL
        }
    }
}

// ---------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------

fn durable_replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = temp_path(path)?;
    let mut file = File::create(&tmp).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("creating TPM NV temp file {}: {e}", tmp.display()),
        )
    })?;
    file.write_all(bytes).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("writing TPM NV temp file {}: {e}", tmp.display()),
        )
    })?;
    file.sync_all().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("syncing TPM NV temp file {}: {e}", tmp.display()),
        )
    })?;
    drop(file);

    std::fs::rename(&tmp, path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "renaming TPM NV temp file {} to {}: {e}",
                tmp.display(),
                path.display(),
            ),
        )
    })?;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let directory = File::open(parent).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("opening TPM NV state directory {}: {e}", parent.display()),
        )
    })?;
    directory.sync_all().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("syncing TPM NV state directory {}: {e}", parent.display()),
        )
    })
}

/// The staging path for `path`.
///
/// It keeps the whole file name and appends the suffix. Replacing the
/// extension instead would stage `tpm2-00.permall`,
/// `tpm2-00.volatilestate` and `tpm2-00.savestate` through one
/// `tpm2-00.tmp`, and a rename would then publish one blob's bytes
/// under another blob's name.
fn temp_path(path: &Path) -> io::Result<PathBuf> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("TPM NV path has no file name: {}", path.display()),
        )
    })?;
    let mut tmp = name.to_os_string();
    tmp.push(".tmp");
    Ok(path.with_file_name(tmp))
}

fn nv_path(s: &CallbackState, name: *const c_char) -> Option<PathBuf> {
    if name.is_null() {
        return None;
    }
    let cstr = unsafe { CStr::from_ptr(name) };
    let n = cstr.to_str().ok()?;
    // libtpms uses names like "tpm2-00.permall", "tpm2-00.volatilestate",
    // "tpm2-00.savestate". Use the name verbatim as the file basename.
    Some(s.state_dir.join(n))
}

fn display_name(name: *const c_char) -> String {
    if name.is_null() {
        return "<null>".into();
    }
    unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned()
}

// A field count or order that drifts from `struct libtpms_callbacks`
// makes libtpms call the wrong pointer, with no runtime complaint.
const _: () = assert!(
    size_of::<LibtpmsCallbacks>() == 64,
    "LibtpmsCallbacks must be exactly 64 bytes on 64-bit targets: \
     check field count and order against libtpms tpm_library.h"
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir {
        path: PathBuf,
    }

    impl ScratchDir {
        fn new() -> Self {
            let sequence = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "vmm-tpm-callbacks-test-{}-{sequence}",
                std::process::id(),
            ));
            std::fs::create_dir(&path).expect("create scratch directory");
            Self { path }
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.path));
        }
    }

    #[test]
    fn durable_replace_writes_and_replaces_contents() {
        let scratch = ScratchDir::new();
        let path = scratch.path.join("tpm2-00.permall");

        // This exercises both sync calls and verifies replacement completes
        // without a leftover temp file. It cannot prove observation atomicity
        // or persistence across a sudden host power loss.
        durable_replace(&path, b"first state").expect("write initial state");
        assert_eq!(
            std::fs::read(&path).expect("read initial state"),
            b"first state"
        );

        durable_replace(&path, b"replacement state").expect("replace state");
        assert_eq!(
            std::fs::read(&path).expect("read replacement state"),
            b"replacement state",
        );
        assert!(!temp_path(&path).expect("temp path").exists());
    }

    /// Two blobs staged through one temp file let a rename publish one
    /// blob's bytes under the other blob's name, which destroys the
    /// vTPM state a BitLocker key depends on.
    #[test]
    fn each_nv_blob_stages_through_its_own_temp_file() {
        let dir = Path::new("/vm/tpm");
        let names = [
            "tpm2-00.permall",
            "tpm2-00.volatilestate",
            "tpm2-00.savestate",
        ];
        let temps: Vec<PathBuf> = names
            .iter()
            .map(|name| temp_path(&dir.join(name)).expect("temp path"))
            .collect();

        for (name, tmp) in names.iter().zip(&temps) {
            assert_eq!(tmp, &dir.join(format!("{name}.tmp")));
        }
        for (i, tmp) in temps.iter().enumerate() {
            assert!(
                !temps[i + 1..].contains(tmp),
                "{} is staged by more than one blob",
                tmp.display(),
            );
        }
    }

    #[test]
    fn a_path_without_a_file_name_has_no_temp_file() {
        assert!(temp_path(Path::new("/")).is_err());
    }
}
