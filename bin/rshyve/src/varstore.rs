// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CFI-compatible persistent UEFI variable storage.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use anyhow::{ensure, Context};
use memmap2::{MmapMut, MmapOptions};
use slog::{error, info, o, warn, Drain, Logger};

use vmm_core::common::RWOp;
use vmm_devices::{FlushError, FlushIntent, Lifecycle, Migrator};

const CFI_BCS_WRITE_BYTE: u8 = 0x10;
const CFI_BCS_CLEAR_STATUS: u8 = 0x50;
const CFI_BCS_READ_STATUS: u8 = 0x70;
const CFI_BCS_READ_ARRAY: u8 = 0xFF;

use vmm_core::common::PAGE_SIZE;
const MAX_VARSTORE_SIZE: u64 = 16 * 1024 * 1024;
const EFI_SYSTEM_NV_DATA_FV_GUID: [u8; 16] = [
    0x8d, 0x2b, 0xf1, 0xff, 0x96, 0x76, 0x8b, 0x4c, 0xa9, 0x85, 0x27, 0x47,
    0x07, 0x5b, 0x4f, 0x50,
];

struct Cfi {
    cmd: u8,
    dirty: bool,
}

impl Cfi {
    fn new() -> Self {
        Self {
            cmd: CFI_BCS_READ_ARRAY,
            dirty: false,
        }
    }

    /// Returns false if the access was rejected as out of range.
    fn access(
        &mut self,
        store: &mut [u8],
        offset: usize,
        op: RWOp<'_>,
    ) -> bool {
        let len = op.len();
        let Some(end) = offset.checked_add(len) else {
            return false;
        };
        if end > store.len() {
            return false;
        }

        match op {
            RWOp::Write(wo) => match self.cmd {
                CFI_BCS_WRITE_BYTE => {
                    store[offset..end].copy_from_slice(wo.buf());
                    self.dirty = true;
                    self.cmd = CFI_BCS_READ_ARRAY;
                }
                _ => self.cmd = wo.buf()[0],
            },
            RWOp::Read(ro) => match self.cmd {
                CFI_BCS_CLEAR_STATUS | CFI_BCS_READ_STATUS => {
                    ro.write_u64(0);
                    self.cmd = CFI_BCS_READ_ARRAY;
                }
                _ => {
                    let mut bytes = [0u8; 8];
                    bytes[..len].copy_from_slice(&store[offset..end]);
                    ro.write_u64(u64::from_le_bytes(bytes));
                }
            },
        }
        true
    }

    fn flush_with(
        &mut self,
        flush: impl FnOnce() -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        if self.dirty {
            flush()?;
            self.dirty = false;
        }
        Ok(())
    }
}

pub struct VarFile {
    file: File,
    map: MmapMut,
    path: PathBuf,
    len: usize,
}

impl VarFile {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        // A caller with no application logger must still see the security
        // and format warnings before it accepts a writable host file.
        let decorator = slog_term::PlainSyncDecorator::new(std::io::stderr());
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        let log = Logger::root(
            drain,
            o!("component" => "rshyve", "module" => "varstore"),
        );
        Self::open_with_logger(path, &log)
    }

    fn open_with_logger(path: &Path, log: &Logger) -> anyhow::Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| {
                format!("cannot open variable store: {}", path.display())
            })?;

        let metadata = file.metadata().with_context(|| {
            format!("cannot stat variable store: {}", path.display())
        })?;
        ensure!(
            metadata.file_type().is_file(),
            "variable store {} is not a regular file",
            path.display(),
        );

        let file_len = metadata.len();
        ensure!(
            file_len >= PAGE_SIZE as u64,
            "variable store {} is too small ({} bytes, minimum {})",
            path.display(),
            file_len,
            PAGE_SIZE,
        );
        ensure!(
            file_len <= MAX_VARSTORE_SIZE,
            "variable store {} is too large ({} bytes, maximum {})",
            path.display(),
            file_len,
            MAX_VARSTORE_SIZE,
        );
        ensure!(
            file_len.is_multiple_of(PAGE_SIZE as u64),
            "variable store {} size ({} bytes) is not page-aligned (must be multiple of {})",
            path.display(),
            file_len,
            PAGE_SIZE,
        );
        let len = usize::try_from(file_len)
            .context("variable store size does not fit in usize")?;

        let mode = metadata.permissions().mode();
        if mode & 0o022 != 0 {
            warn!(log, "variable store is group or other writable";
                "path" => path.display().to_string(),
                "mode" => format!("{mode:#o}"));
        }

        // The lock lives on the open file description, so it lasts as
        // long as `file` does and no longer.
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => anyhow::bail!(
                "variable store {} is locked by another vmm process",
                path.display(),
            ),
            Err(TryLockError::Error(error)) => {
                return Err(error).with_context(|| {
                    format!("cannot lock variable store: {}", path.display())
                });
            }
        }

        validate_header(&mut file, path, len, log)?;

        // MAP_SHARED aliasing is sound: the lock excludes other VMM
        // instances, and this process never changes the validated length.
        // A host actor that can truncate the file can still cause SIGBUS,
        // so file permissions are the trust boundary.
        let map = unsafe { MmapOptions::new().len(len).map_mut(&file) }
            .with_context(|| {
                format!("cannot map variable store: {}", path.display())
            })?;

        Ok(Self {
            file,
            map,
            path: path.to_path_buf(),
            len,
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }
}

fn validate_header(
    file: &mut File,
    path: &Path,
    len: usize,
    log: &Logger,
) -> anyhow::Result<()> {
    let mut header = [0u8; PAGE_SIZE];
    file.read_exact(&mut header).with_context(|| {
        format!("cannot read variable store header: {}", path.display())
    })?;

    if header.iter().all(|byte| *byte == 0xFF) {
        warn!(log, "blank variable store; firmware will format it";
            "path" => path.display().to_string());
        return Ok(());
    }

    ensure!(
        header[16..32] == EFI_SYSTEM_NV_DATA_FV_GUID,
        "variable store {} has an invalid firmware volume GUID",
        path.display(),
    );
    let fv_length = u64::from_le_bytes([
        header[32], header[33], header[34], header[35], header[36], header[37],
        header[38], header[39],
    ]);
    ensure!(
        fv_length == len as u64,
        "variable store {} firmware volume length ({fv_length}) does not match file length ({len})",
        path.display(),
    );
    ensure!(
        &header[40..44] == b"_FVH",
        "variable store {} is missing the _FVH signature",
        path.display(),
    );

    let header_len = u16::from_le_bytes([header[48], header[49]]) as usize;
    ensure!(
        (56..=len).contains(&header_len) && header_len.is_multiple_of(8),
        "variable store {} has invalid firmware volume header length ({header_len})",
        path.display(),
    );
    ensure!(
        matches!(header_len.checked_add(22), Some(end) if end <= len),
        "variable store {} has a truncated variable store header",
        path.display(),
    );
    ensure!(
        header[55] == 2,
        "variable store {} has unsupported firmware volume revision ({})",
        path.display(),
        header[55],
    );

    file.seek(SeekFrom::Start(header_len as u64))
        .with_context(|| {
            format!("cannot seek variable store header: {}", path.display())
        })?;
    let mut store_header = [0u8; 22];
    file.read_exact(&mut store_header).with_context(|| {
        format!("cannot read variable store metadata: {}", path.display())
    })?;

    info!(log, "UEFI variable store header";
        "path" => path.display().to_string(),
        "store_guid" => ?&store_header[..16]);
    if store_header[20] != 0x5A || store_header[21] != 0xFE {
        warn!(log, "UEFI variable store has unexpected format or state";
            "path" => path.display().to_string(),
            "format" => format!("{:#04x}", store_header[20]),
            "state" => format!("{:#04x}", store_header[21]));
    }

    Ok(())
}

pub struct VarStore {
    inner: Mutex<(Cfi, MmapMut)>,
    _file: File,
    path: PathBuf,
    size: usize,
    gpa: u64,
    oob_warned: AtomicBool,
    log: Logger,
}

impl VarStore {
    pub fn new(vf: VarFile, gpa: u64, log: Logger) -> Self {
        Self {
            inner: Mutex::new((Cfi::new(), vf.map)),
            _file: vf.file,
            path: vf.path,
            size: vf.len,
            gpa,
            oob_warned: AtomicBool::new(false),
            log,
        }
    }

    pub fn handle(&self, offset: usize, op: RWOp<'_>) {
        let access_len = op.len();
        let accepted = {
            let mut inner = self.inner.lock().unwrap_or_else(|poisoned| {
                error!(self.log, "variable store lock was poisoned");
                poisoned.into_inner()
            });
            let (cfi, map) = &mut *inner;
            cfi.access(&mut map[..], offset, op)
        };

        if !accepted && !self.oob_warned.swap(true, Ordering::Relaxed) {
            warn!(self.log, "rejected out-of-range variable store access";
                "path" => ?self.path,
                "offset" => offset,
                "access_len" => access_len,
                "store_len" => self.size);
        }
    }

    pub fn gpa(&self) -> u64 {
        self.gpa
    }

    pub fn len(&self) -> usize {
        self.size
    }
}

impl Lifecycle for VarStore {
    fn type_name(&self) -> &'static str {
        "uefi-varstore"
    }

    fn is_quiesced(&self) -> bool {
        true
    }

    fn flush_backing(&self, _intent: FlushIntent) -> Result<(), FlushError> {
        let mut inner = self.inner.lock().map_err(|_| {
            FlushError::Sync(std::io::Error::other(
                "variable store lock was poisoned",
            ))
        })?;
        let (cfi, map) = &mut *inner;
        cfi.flush_with(|| map.flush()).map_err(FlushError::Sync)
    }

    fn migrate(&'_ self) -> Migrator<'_> {
        Migrator::Empty
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::Command;
    use std::sync::atomic::AtomicU64;

    const PRISTINE_LEN: usize = 0x84000;
    #[rustfmt::skip]
    const PRISTINE_PROLOGUE: [u8; 96] = [
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x8d, 0x2b, 0xf1, 0xff, 0x96, 0x76, 0x8b, 0x4c,
        0xa9, 0x85, 0x27, 0x47, 0x07, 0x5b, 0x4f, 0x50,
        0x00, 0x40, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x5f, 0x46, 0x56, 0x48, 0xff, 0xfe, 0x04, 0x00,
        0x48, 0x00, 0xaf, 0xb8, 0x00, 0x00, 0x00, 0x02,
        0x84, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x78, 0x2c, 0xf3, 0xaa, 0x7b, 0x94, 0x9a, 0x43,
        0xa1, 0x80, 0x2e, 0x14, 0x4e, 0xc3, 0x77, 0x92,
        0xb8, 0x3f, 0x08, 0x00, 0x5a, 0xfe, 0x00, 0x00,
    ];

    static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);
    const LOCK_PROBE_PATH: &str = "VMM_VARSTORE_LOCK_PROBE_PATH";

    struct ScratchFile {
        path: PathBuf,
    }

    impl ScratchFile {
        fn with_contents(contents: &[u8]) -> Self {
            let path = scratch_path();
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
                .expect("create scratch file");
            file.write_all(contents).expect("write scratch file");
            drop(file);
            Self { path }
        }
    }

    impl Drop for ScratchFile {
        fn drop(&mut self) {
            drop(std::fs::remove_file(&self.path));
        }
    }

    fn scratch_path() -> PathBuf {
        let sequence = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "vmm-varstore-test-{}-{sequence}",
            std::process::id(),
        ))
    }

    fn valid_store(len: usize) -> Vec<u8> {
        let mut store = vec![0xFF; len];
        store[..PRISTINE_PROLOGUE.len()].copy_from_slice(&PRISTINE_PROLOGUE);
        store[32..40].copy_from_slice(&(len as u64).to_le_bytes());
        store[88..92].copy_from_slice(&((len - 0x48) as u32).to_le_bytes());
        store
    }

    fn cfi_read(
        cfi: &mut Cfi,
        store: &mut [u8],
        offset: usize,
        len: usize,
    ) -> (bool, Vec<u8>) {
        let mut read = vmm_core::common::ReadOp::new(len);
        let accepted = cfi.access(store, offset, RWOp::Read(&mut read));
        (accepted, read.buf().to_vec())
    }

    fn cfi_write(
        cfi: &mut Cfi,
        store: &mut [u8],
        offset: usize,
        bytes: &[u8],
    ) -> bool {
        let write = vmm_core::common::WriteOp::from_buf(bytes);
        cfi.access(store, offset, RWOp::Write(&write))
    }

    #[test]
    fn read_array_returns_backing_bytes() {
        let mut cfi = Cfi::new();
        let mut store = vec![0x10, 0x20, 0x30, 0x40];
        let (accepted, bytes) = cfi_read(&mut cfi, &mut store, 1, 2);
        assert!(accepted);
        assert_eq!(bytes, [0x20, 0x30]);
    }

    #[test]
    fn read_array_honours_1_2_4_8_widths() {
        let mut cfi = Cfi::new();
        let mut store: Vec<u8> = (0..16).collect();
        for width in [1, 2, 4, 8] {
            let (accepted, bytes) = cfi_read(&mut cfi, &mut store, 3, width);
            assert!(accepted);
            assert_eq!(bytes, store[3..3 + width]);
        }
    }

    #[test]
    fn write_byte_then_data_commits_and_reverts_to_read_array() {
        let mut cfi = Cfi::new();
        let mut store = vec![0xFF; 8];
        assert!(cfi_write(&mut cfi, &mut store, 0, &[CFI_BCS_WRITE_BYTE]));
        assert!(cfi_write(&mut cfi, &mut store, 3, &[0x42]));
        assert_eq!(store[3], 0x42);
        assert_eq!(cfi.cmd, CFI_BCS_READ_ARRAY);
    }

    #[test]
    fn write_byte_commits_operand_sized_chunk_not_one_byte() {
        let mut cfi = Cfi::new();
        let mut store = vec![0xFF; 8];
        cfi_write(&mut cfi, &mut store, 0, &[CFI_BCS_WRITE_BYTE]);
        cfi_write(&mut cfi, &mut store, 2, &[1, 2, 3, 4]);
        assert_eq!(&store[2..6], &[1, 2, 3, 4]);
    }

    #[test]
    fn read_status_returns_zero_and_reverts() {
        let mut cfi = Cfi::new();
        let mut store = vec![0xA5; 8];
        cfi_write(&mut cfi, &mut store, 0, &[CFI_BCS_READ_STATUS]);
        let (_, bytes) = cfi_read(&mut cfi, &mut store, 0, 4);
        assert_eq!(bytes, [0, 0, 0, 0]);
        assert_eq!(cfi.cmd, CFI_BCS_READ_ARRAY);
    }

    #[test]
    fn clear_status_returns_zero_and_reverts() {
        let mut cfi = Cfi::new();
        let mut store = vec![0xA5; 8];
        cfi_write(&mut cfi, &mut store, 0, &[CFI_BCS_CLEAR_STATUS]);
        let (_, bytes) = cfi_read(&mut cfi, &mut store, 0, 8);
        assert_eq!(bytes, [0; 8]);
        assert_eq!(cfi.cmd, CFI_BCS_READ_ARRAY);
    }

    #[test]
    fn unknown_written_byte_becomes_command() {
        let mut cfi = Cfi::new();
        let mut store = vec![0x5A; 8];
        cfi_write(&mut cfi, &mut store, 0, &[0x33]);
        assert_eq!(cfi.cmd, 0x33);
        let (_, bytes) = cfi_read(&mut cfi, &mut store, 0, 1);
        assert_eq!(bytes, [0x5A]);
        assert_eq!(cfi.cmd, 0x33);
    }

    #[test]
    fn data_write_without_write_byte_command_leaves_backing_unmodified() {
        let mut cfi = Cfi::new();
        let mut store = vec![0xFF; 8];
        cfi_write(&mut cfi, &mut store, 2, &[0x7A]);
        assert_eq!(store, vec![0xFF; 8]);
    }

    #[test]
    fn command_taken_from_low_byte_of_wide_write() {
        let mut cfi = Cfi::new();
        let mut store = vec![0xFF; 8];
        cfi_write(
            &mut cfi,
            &mut store,
            0,
            &[CFI_BCS_WRITE_BYTE, 0xAA, 0xBB, 0xCC],
        );
        assert_eq!(cfi.cmd, CFI_BCS_WRITE_BYTE);
    }

    #[test]
    fn tail_overrun_read_is_rejected_and_leaves_cmd_unchanged() {
        let mut cfi = Cfi::new();
        let mut store = vec![0xFF; 16];
        cfi_write(&mut cfi, &mut store, 0, &[CFI_BCS_READ_STATUS]);
        let (accepted, bytes) = cfi_read(&mut cfi, &mut store, 12, 8);
        assert!(!accepted);
        assert_eq!(bytes, [0; 8]);
        assert_eq!(cfi.cmd, CFI_BCS_READ_STATUS);
    }

    #[test]
    fn tail_overrun_write_is_rejected_and_does_not_modify_backing() {
        let mut cfi = Cfi::new();
        let mut store = vec![0xFF; 16];
        cfi_write(&mut cfi, &mut store, 0, &[CFI_BCS_WRITE_BYTE]);
        assert!(!cfi_write(
            &mut cfi,
            &mut store,
            12,
            &[1, 2, 3, 4, 5, 6, 7, 8],
        ));
        assert_eq!(store, vec![0xFF; 16]);
        assert_eq!(cfi.cmd, CFI_BCS_WRITE_BYTE);
    }

    #[test]
    fn offset_usize_overflow_is_rejected() {
        let mut cfi = Cfi::new();
        let mut store = vec![0xFF; 16];
        let (accepted, bytes) =
            cfi_read(&mut cfi, &mut store, usize::MAX - 3, 8);
        assert!(!accepted);
        assert_eq!(bytes, [0; 8]);
    }

    #[test]
    fn eight_byte_access_at_last_eight_bytes_succeeds() {
        let mut cfi = Cfi::new();
        let mut store: Vec<u8> = (0..16).collect();
        let (accepted, bytes) = cfi_read(&mut cfi, &mut store, 8, 8);
        assert!(accepted);
        assert_eq!(bytes, store[8..]);
    }

    #[test]
    fn dirty_set_only_by_committed_data_write() {
        let mut cfi = Cfi::new();
        let mut store = vec![0xFF; 8];
        cfi_write(&mut cfi, &mut store, 0, &[CFI_BCS_WRITE_BYTE]);
        assert!(!cfi.dirty);
        let (accepted, _) = cfi_read(&mut cfi, &mut store, 7, 2);
        assert!(!accepted);
        assert!(!cfi.dirty);
        cfi_write(&mut cfi, &mut store, 3, &[0x12]);
        assert!(cfi.dirty);
    }

    #[test]
    fn dirty_cleared_by_flush() {
        let mut cfi = Cfi::new();
        let mut store = vec![0xFF; 8];
        cfi_write(&mut cfi, &mut store, 0, &[CFI_BCS_WRITE_BYTE]);
        cfi_write(&mut cfi, &mut store, 0, &[0x12]);
        let mut flushed = false;
        cfi.flush_with(|| {
            flushed = true;
            Ok(())
        })
        .unwrap();
        assert!(flushed);
        assert!(!cfi.dirty);
    }

    #[test]
    fn rejects_missing_file() {
        let path = scratch_path();
        assert!(VarFile::open(&path).is_err());
    }

    #[test]
    fn rejects_non_page_multiple() {
        let scratch = ScratchFile::with_contents(&vec![0xFF; PAGE_SIZE + 1]);
        assert!(VarFile::open(&scratch.path).is_err());
    }

    #[test]
    fn rejects_too_small() {
        let scratch = ScratchFile::with_contents(&vec![0xFF; PAGE_SIZE - 1]);
        assert!(VarFile::open(&scratch.path).is_err());
    }

    #[test]
    fn accepts_all_ff_blank_store() {
        let scratch = ScratchFile::with_contents(&vec![0xFF; PAGE_SIZE]);
        let file = VarFile::open(&scratch.path).expect("open blank store");
        assert_eq!(file.len(), PAGE_SIZE);
    }

    #[test]
    fn rejects_bad_fv_guid() {
        let mut store = valid_store(PAGE_SIZE);
        store[16] ^= 0xFF;
        let scratch = ScratchFile::with_contents(&store);
        assert!(VarFile::open(&scratch.path).is_err());
    }

    #[test]
    fn rejects_fvlength_mismatch() {
        let mut store = valid_store(PAGE_SIZE);
        store[32..40].copy_from_slice(&0x2000u64.to_le_bytes());
        let scratch = ScratchFile::with_contents(&store);
        assert!(VarFile::open(&scratch.path).is_err());
    }

    #[test]
    fn rejects_missing_fvh_signature() {
        let mut store = valid_store(PAGE_SIZE);
        store[40..44].copy_from_slice(b"BAD!");
        let scratch = ScratchFile::with_contents(&store);
        assert!(VarFile::open(&scratch.path).is_err());
    }

    #[test]
    fn rejects_bad_revision() {
        let mut store = valid_store(PAGE_SIZE);
        store[55] = 1;
        let scratch = ScratchFile::with_contents(&store);
        assert!(VarFile::open(&scratch.path).is_err());
    }

    #[test]
    fn accepts_pristine_header() {
        let mut store = vec![0xFF; PRISTINE_LEN];
        store[..PRISTINE_PROLOGUE.len()].copy_from_slice(&PRISTINE_PROLOGUE);
        let scratch = ScratchFile::with_contents(&store);
        let file = VarFile::open(&scratch.path).expect("open pristine store");
        assert_eq!(file.len(), PRISTINE_LEN);
    }

    #[test]
    fn second_open_of_same_path_fails_with_lock_error() {
        if let Ok(path) = std::env::var(LOCK_PROBE_PATH) {
            let error = match VarFile::open(Path::new(&path)) {
                Ok(_) => panic!("second process acquired variable store lock"),
                Err(error) => error,
            };
            assert!(error
                .to_string()
                .contains("locked by another vmm process"));
            return;
        }

        let scratch = ScratchFile::with_contents(&vec![0xFF; PAGE_SIZE]);
        let held = VarFile::open(&scratch.path).expect("open first store");
        let output = Command::new(
            std::env::current_exe().expect("test executable"),
        )
        .args([
            "--exact",
            "varstore::tests::second_open_of_same_path_fails_with_lock_error",
        ])
        .env(LOCK_PROBE_PATH, &scratch.path)
        .output()
        .expect("run lock probe");
        assert!(
            output.status.success(),
            "lock probe failed: {}",
            String::from_utf8_lossy(&output.stderr),
        );
        drop(held);
    }
}
