// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! FUSE request dispatcher.
//!
//! A request arrives as the `ChainBuf` list of one descriptor chain. A
//! `ChainReader` over the readable segments yields the `fuse_in_header`
//! and body. The reply goes out through a `ChainWriter` over the writable
//! segments.
//!
//! `read_request` and `write_reply` are bounded copies of guest memory.
//! `run` sits between them and can take as long as the host filesystem
//! does. The device holds its guest-access guard over the copies only,
//! so a device reset never waits on the backing store.
//!
//! All three run on the FUSE worker thread. The readers and writers
//! build `SubMapping`s, which are `!Send`, so they must be built where
//! they are used.

use std::io::Read;
use std::io::Write;
use std::sync::Arc;

use vmm_core::mem::PhysMap;

use super::fuse::{self, bytes_of, push_val, read_at};
use super::passthrough::Passthrough;
use crate::chain_io::{ChainReader, ChainWriter};
use crate::queue::ChainBuf;

/// Largest write payload the server promises the guest in FUSE_INIT_OUT.
pub const INIT_MAX_WRITE: u32 = 512 * 1024;

/// Upper bound on one FUSE message body, in either direction.
///
/// Twice [`INIT_MAX_WRITE`], so a write of the promised size fits with
/// its header. READ and READDIR replies are capped here whatever size
/// the guest asks for.
pub const MAX_MSG_SIZE: usize = 2 * INIT_MAX_WRITE as usize;

/// Per-server limits exposed to the guest in FUSE_INIT_OUT.
pub const INIT_MAX_BACKGROUND: u16 = 64;
pub const INIT_CONGESTION: u16 = 48;
pub const INIT_TIME_GRAN: u32 = 1;

pub struct FuseServer {
    passthrough: Arc<Passthrough>,
}

enum Reply {
    /// Success, header only.
    Empty,
    Body(Vec<u8>),
    /// `-errno`, written into `fuse_out_header.error`.
    Error(i32),
    /// No reply expected (FORGET / BATCH_FORGET).
    None,
}

/// One FUSE request, copied out of the guest's chain.
///
/// It is host memory, so [`FuseServer::run`] holds no guest access while
/// the host filesystem works.
pub struct FuseCall {
    unique: u64,
    kind: CallKind,
}

enum CallKind {
    /// A request for the backing store.
    Dispatch {
        header: fuse::FuseInHeader,
        body: Vec<u8>,
    },
    /// A header the device refused. The backing store never sees it.
    Reject(i32),
    /// The chain held no request to answer.
    Silent,
}

impl FuseCall {
    fn silent() -> Self {
        Self {
            unique: 0,
            kind: CallKind::Silent,
        }
    }

    fn reject(unique: u64, errno: i32) -> Self {
        Self {
            unique,
            kind: CallKind::Reject(errno),
        }
    }
}

/// A reply held in host memory, waiting for the guest's chain.
pub struct FuseReply {
    unique: u64,
    reply: Reply,
}

impl FuseServer {
    pub fn new(passthrough: Arc<Passthrough>) -> Self {
        Self { passthrough }
    }

    /// Borrow the passthrough backend, for reset and teardown.
    pub fn passthrough(&self) -> &Arc<Passthrough> {
        &self.passthrough
    }

    /// Copy one request out of the guest's chain.
    ///
    /// The reader keeps to the readable segments, so a guest that
    /// interleaves readable and writable segments cannot make it read a
    /// segment the device must write.
    ///
    /// The caller holds guest access across this call. [`MAX_MSG_SIZE`]
    /// caps the length, so the copy is bounded and the caller can hold
    /// that access on a vCPU-facing lock.
    pub fn read_request(
        &self,
        bufs: &[ChainBuf],
        physmap: &PhysMap,
    ) -> FuseCall {
        let mut reader = ChainReader::new(bufs, physmap);

        let mut hdr_buf = [0u8; fuse::FUSE_IN_HEADER_SIZE];
        if reader.read_exact(&mut hdr_buf).is_err() {
            return FuseCall::silent();
        }
        let header: fuse::FuseInHeader = match read_at(&hdr_buf) {
            Some(h) => h,
            None => return FuseCall::silent(),
        };

        let total_len = header.len as usize;
        let limits = fuse::FUSE_IN_HEADER_SIZE..=MAX_MSG_SIZE;
        if !limits.contains(&total_len) {
            return FuseCall::reject(header.unique, -libc::EINVAL);
        }

        let body_len = total_len - fuse::FUSE_IN_HEADER_SIZE;
        let mut body = vec![0u8; body_len];
        if body_len > 0 && reader.read_exact(&mut body).is_err() {
            return FuseCall::reject(header.unique, -libc::EINVAL);
        }

        FuseCall {
            unique: header.unique,
            kind: CallKind::Dispatch { header, body },
        }
    }

    /// Run a request against the backing store.
    ///
    /// Host memory in, host memory out. The caller must NOT hold guest
    /// access here: the host filesystem can block indefinitely, and a
    /// device reset must not wait behind it.
    pub fn run(&self, call: &FuseCall) -> FuseReply {
        let reply = match &call.kind {
            CallKind::Dispatch { header, body } => self.dispatch(header, body),
            CallKind::Reject(errno) => Reply::Error(*errno),
            CallKind::Silent => Reply::None,
        };
        FuseReply {
            unique: call.unique,
            reply,
        }
    }

    /// Write a reply into the chain's writable window.
    ///
    /// Returns the byte count for the used-ring `len` field. The writer
    /// keeps to the writable segments.
    ///
    /// The caller holds guest access across this call and the used entry
    /// it publishes. The writable window bounds the copy.
    pub fn write_reply(
        &self,
        bufs: &[ChainBuf],
        physmap: &PhysMap,
        reply: FuseReply,
    ) -> u32 {
        let mut writer = ChainWriter::new(bufs, physmap);
        emit_reply(&mut writer, reply.unique, reply.reply)
    }

    fn dispatch(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        match header.opcode {
            fuse::FUSE_INIT => self.op_init(body),
            fuse::FUSE_DESTROY => Reply::Empty,
            fuse::FUSE_LOOKUP => self.op_lookup(header, body),
            fuse::FUSE_FORGET => {
                if let Some(f) = read_at::<fuse::FuseForgetIn>(body) {
                    self.passthrough.forget(header.nodeid, f.nlookup);
                } else {
                    self.passthrough.forget(header.nodeid, 1);
                }
                Reply::None
            }
            fuse::FUSE_BATCH_FORGET => {
                self.op_batch_forget(body);
                Reply::None
            }
            fuse::FUSE_GETATTR => self.op_getattr(header),
            fuse::FUSE_STATFS => self.op_statfs(header),
            fuse::FUSE_ACCESS => self.op_access(header, body),
            fuse::FUSE_READLINK => self.op_readlink(header),
            fuse::FUSE_OPEN => self.op_open(header, body),
            fuse::FUSE_RELEASE => self.op_release(body),
            fuse::FUSE_READ => self.op_read(body),
            fuse::FUSE_OPENDIR => self.op_opendir(header),
            fuse::FUSE_RELEASEDIR => self.op_releasedir(body),
            fuse::FUSE_READDIR => self.op_readdir(header, body, false),
            fuse::FUSE_READDIRPLUS => self.op_readdir(header, body, true),
            fuse::FUSE_FLUSH => Reply::Empty,
            fuse::FUSE_FSYNC | fuse::FUSE_FSYNCDIR => self.op_fsync(body),
            fuse::FUSE_GETXATTR | fuse::FUSE_LISTXATTR => {
                Reply::Error(-libc::ENOTSUP)
            }
            fuse::FUSE_WRITE => self.op_write(body),
            fuse::FUSE_CREATE => self.op_create(header, body),
            fuse::FUSE_MKDIR => self.op_mkdir(header, body),
            fuse::FUSE_MKNOD => self.op_mknod(header, body),
            fuse::FUSE_UNLINK => self.op_unlink(header, body),
            fuse::FUSE_RMDIR => self.op_rmdir(header, body),
            fuse::FUSE_RENAME => self.op_rename(header, body, false),
            fuse::FUSE_RENAME2 => self.op_rename(header, body, true),
            fuse::FUSE_SETATTR => self.op_setattr(header, body),
            fuse::FUSE_SYMLINK => self.op_symlink(header, body),
            fuse::FUSE_LINK => self.op_link(header, body),
            _ => Reply::Error(-libc::ENOSYS),
        }
    }

    // -------------------------------------------------------------------
    // Op handlers
    // -------------------------------------------------------------------

    fn op_init(&self, body: &[u8]) -> Reply {
        let init_in: fuse::FuseInitIn = match read_at(body) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        if init_in.major < 7 {
            return Reply::Error(-libc::EPROTO);
        }
        let minor = init_in.minor.min(fuse::FUSE_KERNEL_MINOR_VERSION);
        let flags = fuse::FUSE_BIG_WRITES
            | fuse::FUSE_ASYNC_READ
            | fuse::FUSE_DO_READDIRPLUS
            | fuse::FUSE_READDIRPLUS_AUTO
            | fuse::FUSE_ATOMIC_O_TRUNC
            | fuse::FUSE_EXPORT_SUPPORT
            | fuse::FUSE_MAX_PAGES
            | fuse::FUSE_CACHE_SYMLINKS
            | fuse::FUSE_SUBMOUNTS;
        let out = fuse::FuseInitOut {
            major: fuse::FUSE_KERNEL_VERSION,
            minor,
            max_readahead: init_in.max_readahead,
            flags: flags & init_in.flags,
            max_background: INIT_MAX_BACKGROUND,
            congestion_threshold: INIT_CONGESTION,
            max_write: INIT_MAX_WRITE,
            time_gran: INIT_TIME_GRAN,
            max_pages: (INIT_MAX_WRITE / 4096) as u16,
            map_alignment: 0,
            unused: [0; 8],
        };
        Reply::Body(bytes_of(&out))
    }

    fn op_lookup(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        let name = match extract_name(body) {
            Some(n) => n,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self.passthrough.lookup(header.nodeid, &name) {
            Ok(entry) => Reply::Body(bytes_of(&entry)),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_batch_forget(&self, body: &[u8]) {
        if body.len() < 8 {
            return;
        }
        let count = u32::from_le_bytes(body[0..4].try_into().unwrap()) as usize;
        let mut off = 8;
        for _ in 0..count {
            if off + 16 > body.len() {
                break;
            }
            let nodeid =
                u64::from_le_bytes(body[off..off + 8].try_into().unwrap());
            let nlookup =
                u64::from_le_bytes(body[off + 8..off + 16].try_into().unwrap());
            self.passthrough.forget(nodeid, nlookup);
            off += 16;
        }
    }

    fn op_getattr(&self, header: &fuse::FuseInHeader) -> Reply {
        match self.passthrough.getattr(header.nodeid) {
            Ok(attr) => Reply::Body(bytes_of(&fuse::FuseAttrOut {
                attr_valid: 1,
                attr_valid_nsec: 0,
                dummy: 0,
                attr,
            })),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_statfs(&self, header: &fuse::FuseInHeader) -> Reply {
        match self.passthrough.statfs(header.nodeid) {
            Ok(s) => Reply::Body(bytes_of(&s)),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_access(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        let acc: fuse::FuseAccessIn = match read_at(body) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self.passthrough.access(header.nodeid, acc.mask) {
            Ok(()) => Reply::Empty,
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_readlink(&self, header: &fuse::FuseInHeader) -> Reply {
        match self.passthrough.readlink(header.nodeid) {
            Ok(target) => Reply::Body(target),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_open(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        let oi: fuse::FuseOpenIn = match read_at(body) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self.passthrough.open(header.nodeid, oi.flags) {
            Ok(fh) => Reply::Body(bytes_of(&fuse::FuseOpenOut {
                fh,
                open_flags: fuse::FOPEN_KEEP_CACHE,
                padding: 0,
            })),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_release(&self, body: &[u8]) -> Reply {
        let r: fuse::FuseReleaseIn = match read_at(body) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self.passthrough.release(r.fh) {
            Ok(()) => Reply::Empty,
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_read(&self, body: &[u8]) -> Reply {
        let rd: fuse::FuseReadIn = match read_at(body) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        if rd.size as usize > MAX_MSG_SIZE {
            return Reply::Error(-libc::EINVAL);
        }
        match self.passthrough.read(rd.fh, rd.offset, rd.size) {
            Ok(data) => Reply::Body(data),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_opendir(&self, header: &fuse::FuseInHeader) -> Reply {
        match self.passthrough.opendir(header.nodeid) {
            Ok(fh) => Reply::Body(bytes_of(&fuse::FuseOpenOut {
                fh,
                open_flags: fuse::FOPEN_CACHE_DIR,
                padding: 0,
            })),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_releasedir(&self, body: &[u8]) -> Reply {
        let r: fuse::FuseReleaseIn = match read_at(body) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self.passthrough.releasedir(r.fh) {
            Ok(()) => Reply::Empty,
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_readdir(
        &self,
        header: &fuse::FuseInHeader,
        body: &[u8],
        with_plus: bool,
    ) -> Reply {
        let rd: fuse::FuseReadIn = match read_at(body) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        let max = (rd.size as usize).min(MAX_MSG_SIZE);
        let mut out: Vec<u8> = Vec::with_capacity(max.min(64 * 1024));
        let pt = &self.passthrough;
        let res = pt.readdir_each(rd.fh, rd.offset, |ino, name, dt, next| {
            let name_bytes = name.to_bytes();
            let namelen = name_bytes.len();
            let plus_prefix = if with_plus {
                size_of::<fuse::FuseEntryOut>()
            } else {
                0
            };
            let needed = plus_prefix + fuse::dirent_size(namelen);
            if out.len() + needed > max {
                return false;
            }
            if with_plus {
                // An entry the lookup refuses (a socket or FIFO, an
                // entry unlinked since the snapshot, or one past the
                // inode cap) goes out with nodeid 0. Linux takes that as
                // "not cached" and looks up the name itself, so one bad
                // entry does not fail the listing. A failed lookup takes
                // no lookup reference.
                let entry = pt
                    .lookup(header.nodeid, name)
                    .unwrap_or_else(|_| fuse::FuseEntryOut::default());
                push_val(&mut out, &entry);
            }
            let d = fuse::FuseDirent {
                ino,
                off: next,
                namelen: namelen as u32,
                typ: dt,
            };
            push_val(&mut out, &d);
            out.extend_from_slice(name_bytes);
            let pad =
                fuse::dirent_size(namelen) - fuse::FUSE_DIRENT_SIZE - namelen;
            out.extend(std::iter::repeat_n(0u8, pad));
            true
        });
        if let Err(e) = res {
            return Reply::Error(-e.to_errno());
        }
        Reply::Body(out)
    }

    // -------------------------------------------------------------------
    // Write-path handlers
    // -------------------------------------------------------------------

    fn op_write(&self, body: &[u8]) -> Reply {
        let write_hdr_size = size_of::<fuse::FuseWriteIn>();
        if body.len() < write_hdr_size {
            return Reply::Error(-libc::EINVAL);
        }
        let w: fuse::FuseWriteIn = match read_at(&body[..write_hdr_size]) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        let data_len = w.size as usize;
        if data_len > MAX_MSG_SIZE || body.len() < write_hdr_size + data_len {
            return Reply::Error(-libc::EINVAL);
        }
        let data = &body[write_hdr_size..write_hdr_size + data_len];
        match self.passthrough.write(w.fh, w.offset, data) {
            Ok(n) => Reply::Body(bytes_of(&fuse::FuseWriteOut {
                size: n,
                padding: 0,
            })),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_create(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        let ci: fuse::FuseCreateIn = match read_at(body) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        let name_off = size_of::<fuse::FuseCreateIn>();
        let name = match extract_name(&body[name_off..]) {
            Some(n) => n,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self.passthrough.create(
            header.nodeid,
            &name,
            ci.flags,
            ci.mode,
            ci.umask,
        ) {
            Ok((entry, fh, _)) => Reply::Body(bytes_of(&fuse::FuseCreateOut {
                entry,
                open: fuse::FuseOpenOut {
                    fh,
                    open_flags: fuse::FOPEN_KEEP_CACHE,
                    padding: 0,
                },
            })),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_mkdir(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        let mi: fuse::FuseMkdirIn = match read_at(body) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        let name_off = size_of::<fuse::FuseMkdirIn>();
        let name = match extract_name(&body[name_off..]) {
            Some(n) => n,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self
            .passthrough
            .mkdir(header.nodeid, &name, mi.mode, mi.umask)
        {
            Ok(entry) => Reply::Body(bytes_of(&entry)),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_mknod(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        if body.len() < 16 {
            return Reply::Error(-libc::EINVAL);
        }
        let mode = u32::from_le_bytes(body[0..4].try_into().unwrap());
        let rdev = u32::from_le_bytes(body[4..8].try_into().unwrap());
        let umask = u32::from_le_bytes(body[8..12].try_into().unwrap());
        let name = match extract_name(&body[16..]) {
            Some(n) => n,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self
            .passthrough
            .mknod(header.nodeid, &name, mode, rdev, umask)
        {
            Ok(entry) => Reply::Body(bytes_of(&entry)),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_unlink(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        let name = match extract_name(body) {
            Some(n) => n,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self.passthrough.unlink(header.nodeid, &name) {
            Ok(()) => Reply::Empty,
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_rmdir(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        let name = match extract_name(body) {
            Some(n) => n,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self.passthrough.rmdir(header.nodeid, &name) {
            Ok(()) => Reply::Empty,
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_rename(
        &self,
        header: &fuse::FuseInHeader,
        body: &[u8],
        is_rename2: bool,
    ) -> Reply {
        let (newdir, flags, names_off) = if is_rename2 {
            if body.len() < 16 {
                return Reply::Error(-libc::EINVAL);
            }
            let newdir = u64::from_le_bytes(body[0..8].try_into().unwrap());
            let flags = u32::from_le_bytes(body[8..12].try_into().unwrap());
            (newdir, flags, 16)
        } else {
            if body.len() < 8 {
                return Reply::Error(-libc::EINVAL);
            }
            let newdir = u64::from_le_bytes(body[0..8].try_into().unwrap());
            (newdir, 0u32, 8)
        };
        if flags != 0 {
            return Reply::Error(-libc::ENOSYS);
        }
        let old_name = match extract_name(&body[names_off..]) {
            Some(n) => n,
            None => return Reply::Error(-libc::EINVAL),
        };
        let old_len = old_name.as_bytes_with_nul().len();
        let new_name = match extract_name(&body[names_off + old_len..]) {
            Some(n) => n,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self.passthrough.rename(
            header.nodeid,
            &old_name,
            newdir,
            &new_name,
            flags,
        ) {
            Ok(()) => Reply::Empty,
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_setattr(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        let sa: fuse::FuseSetattrIn = match read_at(body) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        let fh = if sa.valid & fuse::FATTR_FH != 0 {
            Some(sa.fh)
        } else {
            None
        };
        let res = self.passthrough.setattr(
            header.nodeid,
            sa.valid,
            fh,
            sa.size,
            sa.mode,
            sa.uid,
            sa.gid,
            (
                sa.atime,
                sa.atimensec,
                sa.valid & fuse::FATTR_ATIME_NOW != 0,
            ),
            (
                sa.mtime,
                sa.mtimensec,
                sa.valid & fuse::FATTR_MTIME_NOW != 0,
            ),
        );
        match res {
            Ok(attr) => Reply::Body(bytes_of(&fuse::FuseAttrOut {
                attr_valid: 1,
                attr_valid_nsec: 0,
                dummy: 0,
                attr,
            })),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_symlink(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        let name = match extract_name(body) {
            Some(n) => n,
            None => return Reply::Error(-libc::EINVAL),
        };
        let name_len = name.as_bytes_with_nul().len();
        let target = match extract_name(&body[name_len..]) {
            Some(n) => n,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self.passthrough.symlink(header.nodeid, &name, &target) {
            Ok(entry) => Reply::Body(bytes_of(&entry)),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_link(&self, header: &fuse::FuseInHeader, body: &[u8]) -> Reply {
        if body.len() < 8 {
            return Reply::Error(-libc::EINVAL);
        }
        let oldnodeid = u64::from_le_bytes(body[0..8].try_into().unwrap());
        let new_name = match extract_name(&body[8..]) {
            Some(n) => n,
            None => return Reply::Error(-libc::EINVAL),
        };
        match self.passthrough.link(oldnodeid, header.nodeid, &new_name) {
            Ok(entry) => Reply::Body(bytes_of(&entry)),
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }

    fn op_fsync(&self, body: &[u8]) -> Reply {
        let f: fuse::FuseFsyncIn = match read_at(body) {
            Some(v) => v,
            None => return Reply::Error(-libc::EINVAL),
        };
        let datasync = f.fsync_flags & 1 != 0;
        match self.passthrough.fsync(f.fh, datasync) {
            Ok(()) => Reply::Empty,
            Err(e) => Reply::Error(-e.to_errno()),
        }
    }
}

// ---------------------------------------------------------------------------
// Wire helpers
// ---------------------------------------------------------------------------

fn emit_reply<W: Write>(writer: &mut W, unique: u64, reply: Reply) -> u32 {
    match reply {
        Reply::None => 0,
        Reply::Empty => write_header(writer, unique, 0, &[]),
        Reply::Body(body) => write_header(writer, unique, 0, &body),
        // The only point where an errno reaches the guest, so the only
        // place that translates host errno to Linux.
        Reply::Error(err) => {
            let linux = if err < 0 {
                -fuse::to_linux_errno(-err)
            } else {
                err
            };
            write_header(writer, unique, linux, &[])
        }
    }
}

fn write_header<W: Write>(
    writer: &mut W,
    unique: u64,
    err: i32,
    body: &[u8],
) -> u32 {
    let total = fuse::FUSE_OUT_HEADER_SIZE + body.len();
    let hdr = fuse::FuseOutHeader {
        len: total as u32,
        error: err,
        unique,
    };
    let hdr_bytes = bytes_of(&hdr);
    let mut written = 0usize;
    match writer.write_all(&hdr_bytes) {
        Ok(()) => written += hdr_bytes.len(),
        Err(_) => return 0,
    }
    // If the body does not fit the writable window, still count the
    // header already written, so the guest sees a reply.
    if !body.is_empty() && writer.write_all(body).is_ok() {
        written += body.len();
    }
    written as u32
}

/// Interpret a zero-terminated name at the start of `buf`.
fn extract_name(buf: &[u8]) -> Option<std::ffi::CString> {
    let end = buf.iter().position(|b| *b == 0)?;
    if end == 0 {
        return None;
    }
    std::ffi::CString::new(&buf[..end]).ok()
}

#[cfg(test)]
mod tests;
