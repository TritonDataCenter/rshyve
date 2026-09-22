// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Backing store for a boot image.

use std::fs::File;
use std::ops::Deref;
use std::path::Path;

use anyhow::{Context, Result};

/// The bytes of a boot image, either owned or mapped from a file.
///
/// Boot images run to megabytes, and `fs::read` charges a fresh
/// allocation, the kernel zeroing it, and a copy out of the page cache,
/// before the loader copies the image again into guest memory. Mapping
/// the file drops the allocation and one of the two copies, and reads
/// only the pages a loader touches. On a 12 MiB PVH kernel, a full read
/// costs 17 ms of a 22 ms load phase.
pub enum ImageBytes {
    /// Bytes already in hand, from a test or another loader.
    Owned(Vec<u8>),
    /// A read-only mapping of the image file, and the file itself.
    ///
    /// The mapping serves header parsing, which touches only a handful
    /// of pages. Bulk segment loads go through `file` instead, read
    /// straight into guest memory.
    Mapped { mapping: memmap2::Mmap, file: File },
}

impl ImageBytes {
    /// Map `path` read-only.
    ///
    /// The mapping tracks the file, so truncating it mid-boot turns a
    /// later read into SIGBUS. The kernel and initrd come from the
    /// operator and are read once at startup. Anyone who can truncate
    /// one can also replace its contents, so this gives an attacker
    /// nothing new.
    pub fn map(path: &Path) -> Result<Self> {
        let file = File::open(path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        // Safety: mapping cannot prevent concurrent modification of the
        // file. The doc comment on `map` explains why that is acceptable
        // for this input.
        let mapping = unsafe { memmap2::Mmap::map(&file) }
            .with_context(|| format!("failed to map {}", path.display()))?;
        Ok(Self::Mapped { mapping, file })
    }

    /// The open image file, when there is one.
    ///
    /// A loader uses this to read a segment straight into guest memory
    /// instead of copying it through the mapping. `Owned` bytes have no
    /// file, so a caller must keep a path that copies from the slice.
    pub fn file(&self) -> Option<&File> {
        match self {
            Self::Mapped { file, .. } => Some(file),
            Self::Owned(_) => None,
        }
    }
}

impl Deref for ImageBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Owned(data) => data,
            Self::Mapped { mapping, .. } => mapping,
        }
    }
}

// Prints the source and the size, never the contents. A derived impl
// would put a whole multi-megabyte kernel in every panic message.
impl std::fmt::Debug for ImageBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let source = match self {
            Self::Owned(_) => "owned",
            Self::Mapped { .. } => "mapped",
        };
        f.debug_struct("ImageBytes")
            .field("source", &source)
            .field("bytes", &self.len())
            .finish()
    }
}

impl From<Vec<u8>> for ImageBytes {
    fn from(data: Vec<u8>) -> Self {
        Self::Owned(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn owned_and_mapped_read_the_same() {
        let payload: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();

        let dir = std::env::temp_dir().join("vmm-boot-imagebytes");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("image.bin");
        let mut f = File::create(&path).unwrap();
        f.write_all(&payload).unwrap();
        f.sync_all().unwrap();
        drop(f);

        let mapped = ImageBytes::map(&path).unwrap();
        let owned = ImageBytes::from(payload.clone());
        assert_eq!(&*mapped, &payload[..]);
        assert_eq!(&*owned, &*mapped);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_panic() {
        let err = ImageBytes::map(Path::new("/nonexistent/vmm-boot/image"))
            .unwrap_err();
        assert!(err.to_string().contains("failed to open"), "got: {err}");
    }
}
