// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The mmap primitives every guest memory access goes through.
//!
//! The safety model is in the parent module comment.

use std::io::{Error, ErrorKind, Result};
use std::mem::{align_of, size_of};
use std::os::fd::AsRawFd;
use std::ptr::NonNull;
use std::sync::Arc;

use bitflags::bitflags;

use crate::hdl::VmmHdl;

bitflags! {
    /// Memory protection flags.
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub struct Prot: i32 {
        const READ  = libc::PROT_READ;
        const WRITE = libc::PROT_WRITE;
        const EXEC  = libc::PROT_EXEC;
        const RW    = libc::PROT_READ | libc::PROT_WRITE;
        const RWX   = libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC;
    }
}

/// An owned region of mapped memory.
///
/// Wraps an mmap'd region with protection metadata. The mapping is
/// unmapped on drop. No Rust reference to the underlying memory is
/// ever created. All access goes through raw pointer operations.
///
/// # Safety
///
/// The `Send` and `Sync` impls are safe because:
/// - The pointer is obtained from `mmap()` and remains valid until `drop()`.
/// - The API never exposes raw pointers or references to callers.
/// - All reads/writes go through `SubMapping` which enforces bounds
///   and protection checks.
pub(super) struct Mapping {
    pub(super) ptr: NonNull<u8>,
    pub(super) len: usize,
    pub(super) prot: Prot,
}

// Safety: See doc comment on Mapping. The pointer is stable (from mmap),
// and no code creates references to the underlying data.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Create anonymous read/write backing without a VMM file descriptor.
    pub(super) fn anon(size: usize) -> Result<Self> {
        // Safety: NULL addr lets the OS choose a non-conflicting location.
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                Prot::RW.bits(),
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };

        if raw == libc::MAP_FAILED {
            return Err(Error::last_os_error());
        }

        let ptr = NonNull::new(raw as *mut u8).expect("mmap returned non-NULL");
        Ok(Self {
            ptr,
            len: size,
            prot: Prot::RW,
        })
    }

    /// Create a new memory mapping from a VMM file descriptor.
    ///
    /// # Arguments
    /// - `size`: Region size in bytes (must be page-aligned).
    /// - `prot`: Requested protection flags.
    /// - `hdl`: VMM handle providing the file descriptor.
    /// - `offset`: Offset within the VMM fd to map.
    pub(super) fn new(
        size: usize,
        prot: Prot,
        hdl: &VmmHdl,
        offset: i64,
    ) -> Result<Self> {
        if size == 0 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "mapping size must be non-zero",
            ));
        }

        // Only the guest gets EXEC. The VMM's own mapping never has it.
        let mmap_prot = prot.intersection(Prot::RW);

        // Safety: NULL addr lets the OS choose a non-conflicting location.
        // The caller must ensure the VMM fd and offset are valid and that
        // the underlying segment outlives this Mapping.
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                mmap_prot.bits(),
                libc::MAP_SHARED,
                hdl.as_raw_fd(),
                offset,
            )
        };

        if raw == libc::MAP_FAILED {
            return Err(Error::last_os_error());
        }

        let ptr = NonNull::new(raw as *mut u8).expect("mmap returned non-NULL");

        Ok(Self {
            ptr,
            len: size,
            prot,
        })
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // Safety: ptr and len were set by a successful mmap() call.
        unsafe {
            libc::munmap(self.ptr.as_ptr() as *mut libc::c_void, self.len);
        }
    }
}

/// A borrowed, bounds-checked view into a [`Mapping`].
///
/// Provides safe read/write operations on guest memory without ever
/// creating Rust references to that memory.
///
/// The `SubMapping` holds an `Arc<Mapping>`, which keeps the mmap
/// alive for as long as any SubMapping exists. This is safe because
/// the Mapping's Drop (munmap) only runs when all Arc references
/// are dropped.
///
/// All reads use `read_volatile` and all writes use `write_volatile`
/// because guest vCPUs may concurrently access the same memory.
pub struct SubMapping {
    /// Shared ownership of the backing Mapping, preventing munmap
    /// while this SubMapping exists.
    _backing: Arc<Mapping>,
    ptr: NonNull<u8>,
    len: usize,
    pub(super) prot: Prot,
}

impl SubMapping {
    pub(super) fn new(mapping: &Arc<Mapping>) -> Self {
        Self {
            _backing: Arc::clone(mapping),
            ptr: mapping.ptr,
            len: mapping.len,
            prot: mapping.prot,
        }
    }

    /// Take away every permission not in `prot`.
    pub(super) fn restrict(mut self, prot: Prot) -> Self {
        self.prot = self.prot.intersection(prot);
        self
    }

    /// The raw host pointer to the start of this mapping.
    ///
    /// # Safety
    ///
    /// The pointer aliases guest memory that vCPUs access at the same
    /// time. It is only ever an address for the kernel to copy to or
    /// from, and this `SubMapping` must outlive that copy.
    #[inline]
    pub(super) unsafe fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// Create a sub-region within this mapping.
    ///
    /// Returns `None` if `offset + len` exceeds the mapping bounds.
    pub fn subregion(&self, offset: usize, len: usize) -> Option<SubMapping> {
        let end = offset.checked_add(len)?;
        if end > self.len {
            return None;
        }

        // Safety: offset is within bounds (checked above), and the
        // Arc<Mapping> keeps the mmap alive.
        let ptr = unsafe { self.ptr.as_ptr().add(offset) };
        Some(SubMapping {
            _backing: Arc::clone(&self._backing),
            ptr: NonNull::new(ptr).expect("offset from NonNull is non-null"),
            len,
            prot: self.prot,
        })
    }

    /// Read a value of type T from the start of this mapping.
    ///
    /// Uses `read_volatile` because guest memory is concurrently mutable.
    pub fn read<T: Copy>(&self) -> Result<T> {
        if !self.prot.contains(Prot::READ) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "mapping is not readable",
            ));
        }
        if self.len < size_of::<T>() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "mapping too small for requested type",
            ));
        }
        if !(self.ptr.as_ptr() as usize).is_multiple_of(align_of::<T>()) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "mapping not aligned for requested type",
            ));
        }

        // Safety: bounds and alignment checked above. read_volatile,
        // because a vCPU can write the same memory at the same time.
        let val =
            unsafe { std::ptr::read_volatile(self.ptr.as_ptr() as *const T) };
        Ok(val)
    }

    /// Write a value of type T to the start of this mapping.
    ///
    /// Uses `write_volatile` because guest memory is concurrently readable.
    pub fn write<T: Copy>(&self, val: &T) -> Result<()> {
        if !self.prot.contains(Prot::WRITE) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "mapping is not writable",
            ));
        }
        if self.len < size_of::<T>() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "mapping too small for requested type",
            ));
        }
        if !(self.ptr.as_ptr() as usize).is_multiple_of(align_of::<T>()) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "mapping not aligned for requested type",
            ));
        }

        // Safety: bounds and alignment checked above. write_volatile,
        // because a vCPU can read the same memory at the same time.
        unsafe {
            std::ptr::write_volatile(self.ptr.as_ptr() as *mut T, *val);
        }
        Ok(())
    }

    /// Read bytes from this mapping into a buffer.
    pub fn read_bytes(&self, buf: &mut [u8]) -> Result<()> {
        if !self.prot.contains(Prot::READ) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "mapping is not readable",
            ));
        }
        if buf.len() > self.len {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "read exceeds mapping bounds",
            ));
        }

        // Safety: bounds checked above. Volatile reads, because vCPUs
        // can write guest memory at the same time, and the optimizer can
        // merge or drop non-volatile loads.
        let src = self.ptr.as_ptr();
        for (i, byte) in buf.iter_mut().enumerate() {
            unsafe {
                *byte = std::ptr::read_volatile(src.add(i));
            }
        }
        Ok(())
    }

    /// Bulk copy out of guest-visible memory into a host buffer.
    ///
    /// Unlike the byte-at-a-time volatile loop in [`Self::read_bytes`],
    /// this is a plain memcpy, so the optimizer can split, merge or
    /// move the loads. Use it only when no vCPU can write the source
    /// while the copy runs, or when a torn result is acceptable. Two
    /// callers qualify. Display capture accepts a torn frame, and
    /// volatile byte reads cost too much for a multi-megabyte capture.
    /// A virtio device reads a data buffer that the driver gave to the
    /// device, and the driver must not write that buffer again until
    /// the used entry publishes it. Do not use this where a torn read
    /// is a correctness bug.
    pub fn copy_out(&self, dst: &mut [u8]) -> Result<()> {
        if !self.prot.contains(Prot::READ) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "mapping is not readable",
            ));
        }
        if dst.len() > self.len {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "copy exceeds mapping bounds",
            ));
        }

        // Safety: bounds are checked above, and callers cannot obtain an
        // overlapping destination through the safe SubMapping API.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.ptr.as_ptr(),
                dst.as_mut_ptr(),
                dst.len(),
            );
        }
        Ok(())
    }

    /// Write bytes from a buffer into this mapping.
    pub fn write_bytes(&self, data: &[u8]) -> Result<()> {
        if !self.prot.contains(Prot::WRITE) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "mapping is not writable",
            ));
        }
        if data.len() > self.len {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "write exceeds mapping bounds",
            ));
        }

        // Safety: bounds checked above. Volatile writes, because vCPUs
        // can read guest memory at the same time, and the optimizer can
        // reorder or drop non-volatile stores.
        let dst = self.ptr.as_ptr();
        for (i, byte) in data.iter().enumerate() {
            unsafe {
                std::ptr::write_volatile(dst.add(i), *byte);
            }
        }
        Ok(())
    }

    /// Bulk copy a host buffer into guest-visible memory.
    ///
    /// The write-side counterpart to [`Self::copy_out`]. The
    /// byte-at-a-time volatile loop in [`Self::write_bytes`] cannot be
    /// coalesced by the optimizer, which is the point of volatile, so it
    /// costs about one store instruction per byte. That is too slow for
    /// the multi-megabyte writes on the boot path: a 9.3 MiB PVH kernel
    /// loads about 14 ms slower that way than with a memcpy.
    ///
    /// Use it only for writes that no vCPU can observe part way
    /// through. Two things can make that true. The boot path writes
    /// before the vCPU threads start. A virtio device writes a data
    /// buffer that the driver gave to the device, and the driver must
    /// not read that buffer until the used index publishes it;
    /// `VirtQueue::push_used` writes that index after a release fence,
    /// which puts this copy before it on all architectures. Do not use
    /// this where a torn write is a correctness bug.
    pub fn copy_in(&self, src: &[u8]) -> Result<()> {
        if !self.prot.contains(Prot::WRITE) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "mapping is not writable",
            ));
        }
        if src.len() > self.len {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "copy exceeds mapping bounds",
            ));
        }

        // Safety: bounds are checked above. `copy` rather than
        // `copy_nonoverlapping`, because a caller may stage bytes that
        // alias guest memory, and memmove costs nothing extra when the
        // ranges do not overlap.
        unsafe {
            std::ptr::copy(src.as_ptr(), self.ptr.as_ptr(), src.len());
        }
        // Keep the copy from sinking past the stores that publish it,
        // such as the vCPU start that follows on the boot path.
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::Release);
        Ok(())
    }

    /// Fill the first `len` bytes of this mapping from `file`, starting
    /// at `file_offset`.
    ///
    /// Reads straight from the file descriptor into guest memory, so a
    /// multi-megabyte boot image is never staged in the VMM's address
    /// space. Uses `pread` on the raw pointer rather than a slice: the
    /// safety model at the top of this module forbids forming a Rust
    /// reference to guest memory, and that holds here too.
    ///
    /// Carries the same constraint as [`Self::copy_in`]: only for
    /// writes no vCPU can observe part way through. On the boot path,
    /// the vCPU threads have not started.
    ///
    /// Short reads are resumed and `EINTR` is retried. End of file
    /// before `len` bytes is an error rather than a short fill: a
    /// segment claiming more bytes than its file holds must not leave
    /// the tail holding whatever the guest page had before.
    pub fn read_exact_from(
        &self,
        file: &std::fs::File,
        file_offset: u64,
        len: usize,
    ) -> Result<()> {
        if !self.prot.contains(Prot::WRITE) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "mapping is not writable",
            ));
        }
        if len > self.len {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "read exceeds mapping bounds",
            ));
        }

        let fd = file.as_raw_fd();
        let mut done: usize = 0;
        while done < len {
            let offset = file_offset
                .checked_add(done as u64)
                .and_then(|o| i64::try_from(o).ok())
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidInput,
                        "file offset is out of range",
                    )
                })?;
            // Safety: `done < len <= self.len`, so the destination stays
            // inside the mapping, and the remaining count cannot run
            // past its end. No Rust reference to guest memory is formed.
            let got = unsafe {
                libc::pread(
                    fd,
                    self.ptr.as_ptr().add(done) as *mut libc::c_void,
                    len - done,
                    offset,
                )
            };
            if got < 0 {
                let e = Error::last_os_error();
                if e.kind() == ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if got == 0 {
                return Err(Error::new(
                    ErrorKind::UnexpectedEof,
                    format!(
                        "image ended {} bytes before the segment did",
                        len - done
                    ),
                ));
            }
            done += got as usize;
        }
        // Keep the fill from sinking past the stores that publish it,
        // such as the vCPU start that follows on the boot path.
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::Release);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}
