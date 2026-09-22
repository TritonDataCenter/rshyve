// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ELF64 and Xen PVH note parsing.
//!
//! Hand-written rather than delegated to `linux-loader`, because that
//! crate reaches guest memory through a raw host pointer and this repo
//! keeps `PhysMap::lookup` as the only path into guest RAM.

use std::fmt;
use std::path::Path;

use anyhow::{ensure, Context, Result};

use crate::ImageBytes;

pub(crate) const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;
const NHDR_SIZE: usize = 12;

const EI_CLASS: usize = 4;
const EI_DATA: usize = 5;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const E_MACHINE_OFF: usize = 0x12;
const E_PHOFF_OFF: usize = 0x20;
const E_PHENTSIZE_OFF: usize = 0x36;
const E_PHNUM_OFF: usize = 0x38;
const EM_X86_64: u16 = 0x3E;

const PT_LOAD: u32 = 1;
const PT_NOTE: u32 = 4;

/// `XEN_ELFNOTE_PHYS32_ENTRY`.
const XEN_ELFNOTE_PHYS32_ENTRY: u32 = 18;
/// Note name, NUL included, as the Xen ABI writes it.
const XEN_NOTE_NAME: &[u8] = b"Xen\0";

/// One `PT_LOAD` segment to copy into guest RAM.
#[derive(Debug, Clone, Copy)]
pub struct PvhSegment {
    /// Guest physical address to load the segment at.
    pub paddr: u64,
    /// Offset of the segment contents in the image.
    pub file_offset: usize,
    /// Byte count to copy. The rest of `p_memsz` is BSS, and guest RAM
    /// is already zero.
    pub filesz: usize,
}

/// A PVH-capable ELF64 kernel.
pub struct PvhKernel {
    data: ImageBytes,
    entry32: u32,
    segments: Vec<PvhSegment>,
    load_end: u64,
}

// Debug leaves out the image bytes. A derived one prints a whole
// multi-megabyte kernel into every panic message.
impl fmt::Debug for PvhKernel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PvhKernel")
            .field("bytes", &self.data.len())
            .field("entry32", &format_args!("{:#x}", self.entry32))
            .field("segments", &self.segments)
            .field("load_end", &format_args!("{:#x}", self.load_end))
            .finish()
    }
}

impl PvhKernel {
    pub fn open(path: &Path) -> Result<Self> {
        Self::from_bytes(ImageBytes::map(path)?)
    }

    pub fn from_bytes(data: impl Into<ImageBytes>) -> Result<Self> {
        let data = data.into();
        ensure!(
            data.len() >= EHDR_SIZE,
            "file too small to be an ELF64 image"
        );
        ensure!(&data[0..4] == ELF_MAGIC, "not an ELF image (bad magic)");
        ensure!(data[EI_CLASS] == ELFCLASS64, "PVH kernel must be ELF64");
        ensure!(
            data[EI_DATA] == ELFDATA2LSB,
            "PVH kernel must be little-endian"
        );
        let machine = read_u16(&data, E_MACHINE_OFF)?;
        ensure!(
            machine == EM_X86_64,
            "PVH kernel must target x86-64 (e_machine={machine:#06x})"
        );

        let phoff = usize_of(read_u64(&data, E_PHOFF_OFF)?)?;
        let phentsize = read_u16(&data, E_PHENTSIZE_OFF)? as usize;
        let phnum = read_u16(&data, E_PHNUM_OFF)? as usize;
        ensure!(
            phentsize == PHDR_SIZE,
            "unexpected e_phentsize {phentsize} (want {PHDR_SIZE})"
        );
        ensure!(
            phoff >= EHDR_SIZE,
            "program header table overlaps the ELF header"
        );

        let mut segments: Vec<PvhSegment> = Vec::new();
        let mut load_end: u64 = 0;
        let mut entry32: Option<u32> = None;

        for i in 0..phnum {
            let at = i
                .checked_mul(PHDR_SIZE)
                .and_then(|off| phoff.checked_add(off))
                .context("program header table offset overflows")?;
            let hdr_end = at
                .checked_add(PHDR_SIZE)
                .context("program header table overflows")?;
            ensure!(
                hdr_end <= data.len(),
                "program header {i} lies outside the file"
            );

            let p_type = read_u32(&data, at)?;
            let p_offset = usize_of(read_u64(&data, at + 8)?)?;
            let p_paddr = read_u64(&data, at + 24)?;
            let p_filesz = usize_of(read_u64(&data, at + 32)?)?;
            let p_memsz = read_u64(&data, at + 40)?;

            let file_end = p_offset
                .checked_add(p_filesz)
                .context("segment range overflows the file")?;

            match p_type {
                PT_LOAD if p_filesz > 0 => {
                    ensure!(
                        file_end <= data.len(),
                        "PT_LOAD segment {i} lies outside the file"
                    );
                    let seg_end = p_paddr
                        .checked_add(p_memsz)
                        .context("PT_LOAD segment overflows guest memory")?;
                    load_end = load_end.max(seg_end);
                    segments.push(PvhSegment {
                        paddr: p_paddr,
                        file_offset: p_offset,
                        filesz: p_filesz,
                    });
                }
                PT_NOTE => {
                    ensure!(
                        file_end <= data.len(),
                        "PT_NOTE segment {i} lies outside the file"
                    );
                    if entry32.is_none() {
                        entry32 =
                            find_xen_phys32_entry(&data[p_offset..file_end])?;
                    }
                }
                _ => {}
            }
        }

        let entry32 = entry32.context(
            "ELF image carries no XEN_ELFNOTE_PHYS32_ENTRY note; \
             build the kernel with CONFIG_PVH=y",
        )?;
        ensure!(
            !segments.is_empty(),
            "ELF image has no loadable PT_LOAD segments"
        );

        Ok(Self {
            data,
            entry32,
            segments,
            load_end,
        })
    }

    /// 32-bit protected-mode entry point from the Xen note.
    pub fn entry_point_32(&self) -> u32 {
        self.entry32
    }

    /// Highest `p_paddr + p_memsz` across every `PT_LOAD` segment.
    pub fn load_end(&self) -> u64 {
        self.load_end
    }

    /// The open image file, when this kernel was opened from a path.
    ///
    /// Lets a loader read each segment straight into guest memory.
    pub fn source(&self) -> Option<&std::fs::File> {
        self.data.file()
    }

    /// Segments to copy into guest RAM, in program-header order.
    pub fn segments(&self) -> &[PvhSegment] {
        &self.segments
    }

    /// Raw image bytes. Every `PvhSegment` file range is inside them.
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }
}

/// Walk one `PT_NOTE` segment for the Xen 32-bit entry point.
///
/// Each note is a 12-byte header followed by the name and the
/// descriptor, each padded up to a 4-byte boundary. Every cursor step is
/// checked, so a malformed length yields an error or a clean `None`
/// rather than a panic or an endless loop.
fn find_xen_phys32_entry(note_bytes: &[u8]) -> Result<Option<u32>> {
    let mut cursor = 0usize;
    loop {
        let hdr_end = match cursor.checked_add(NHDR_SIZE) {
            Some(end) => end,
            None => return Ok(None),
        };
        if hdr_end > note_bytes.len() {
            return Ok(None);
        }

        let namesz = read_u32(note_bytes, cursor)? as usize;
        let descsz = read_u32(note_bytes, cursor + 4)? as usize;
        let ntype = read_u32(note_bytes, cursor + 8)?;

        let name_end = match hdr_end.checked_add(namesz) {
            Some(end) if end <= note_bytes.len() => end,
            _ => return Ok(None),
        };
        let desc_at = align4(name_end)?;
        let desc_end = match desc_at.checked_add(descsz) {
            Some(end) if end <= note_bytes.len() => end,
            _ => return Ok(None),
        };

        if ntype == XEN_ELFNOTE_PHYS32_ENTRY
            && namesz == XEN_NOTE_NAME.len()
            && &note_bytes[hdr_end..name_end] == XEN_NOTE_NAME
        {
            ensure!(
                descsz >= 4,
                "Xen PVH note descriptor is shorter than 4 bytes"
            );
            return Ok(Some(read_u32(note_bytes, desc_at)?));
        }

        let next = align4(desc_end)?;
        ensure!(next > cursor, "ELF note walk made no progress");
        cursor = next;
    }
}

fn align4(value: usize) -> Result<usize> {
    Ok(value.checked_add(3).context("ELF note offset overflows")? & !3)
}

fn usize_of(value: u64) -> Result<usize> {
    usize::try_from(value).context("ELF offset exceeds this host's usize")
}

fn read_u16(buf: &[u8], off: usize) -> Result<u16> {
    let end = off.checked_add(2).context("ELF field offset overflows")?;
    let b = buf
        .get(off..end)
        .context("ELF field lies outside the file")?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn read_u32(buf: &[u8], off: usize) -> Result<u32> {
    let end = off.checked_add(4).context("ELF field offset overflows")?;
    let b = buf
        .get(off..end)
        .context("ELF field lies outside the file")?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn read_u64(buf: &[u8], off: usize) -> Result<u64> {
    let end = off.checked_add(8).context("ELF field offset overflows")?;
    let b = buf
        .get(off..end)
        .context("ELF field lies outside the file")?;
    Ok(u64::from_le_bytes([
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHOFF: usize = 64;
    const NOTE_OFF: usize = 0x100;
    const LOAD_OFF: usize = 0x200;
    const LOAD_PADDR: u64 = 0x10_0000;
    const LOAD_BYTES: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

    fn pad_to_4(buf: &mut Vec<u8>) {
        while !buf.len().is_multiple_of(4) {
            buf.push(0);
        }
    }

    fn push_note(buf: &mut Vec<u8>, ntype: u32, name: &[u8], desc: &[u8]) {
        buf.extend_from_slice(&(name.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(desc.len() as u32).to_le_bytes());
        buf.extend_from_slice(&ntype.to_le_bytes());
        buf.extend_from_slice(name);
        pad_to_4(buf);
        buf.extend_from_slice(desc);
        pad_to_4(buf);
    }

    fn write_phdr(
        img: &mut [u8],
        idx: usize,
        p_type: u32,
        p_offset: u64,
        p_paddr: u64,
        p_filesz: u64,
        p_memsz: u64,
    ) {
        let at = PHOFF + idx * PHDR_SIZE;
        img[at..at + 4].copy_from_slice(&p_type.to_le_bytes());
        img[at + 8..at + 16].copy_from_slice(&p_offset.to_le_bytes());
        img[at + 24..at + 32].copy_from_slice(&p_paddr.to_le_bytes());
        img[at + 32..at + 40].copy_from_slice(&p_filesz.to_le_bytes());
        img[at + 40..at + 48].copy_from_slice(&p_memsz.to_le_bytes());
    }

    /// Synthetic ELF64: one PT_NOTE segment holding `notes`, one PT_LOAD.
    fn build_elf(notes: &[u8]) -> Vec<u8> {
        let mut img = vec![0u8; 0x400];
        img[0..4].copy_from_slice(b"\x7fELF");
        img[EI_CLASS] = ELFCLASS64;
        img[EI_DATA] = ELFDATA2LSB;
        img[E_MACHINE_OFF..E_MACHINE_OFF + 2]
            .copy_from_slice(&EM_X86_64.to_le_bytes());
        img[E_PHOFF_OFF..E_PHOFF_OFF + 8]
            .copy_from_slice(&(PHOFF as u64).to_le_bytes());
        img[E_PHENTSIZE_OFF..E_PHENTSIZE_OFF + 2]
            .copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
        img[E_PHNUM_OFF..E_PHNUM_OFF + 2].copy_from_slice(&2u16.to_le_bytes());

        write_phdr(
            &mut img,
            0,
            PT_NOTE,
            NOTE_OFF as u64,
            0,
            notes.len() as u64,
            notes.len() as u64,
        );
        write_phdr(
            &mut img,
            1,
            PT_LOAD,
            LOAD_OFF as u64,
            LOAD_PADDR,
            LOAD_BYTES.len() as u64,
            LOAD_BYTES.len() as u64,
        );
        img[NOTE_OFF..NOTE_OFF + notes.len()].copy_from_slice(notes);
        img[LOAD_OFF..LOAD_OFF + LOAD_BYTES.len()].copy_from_slice(&LOAD_BYTES);
        img
    }

    fn xen_notes(entry: u32) -> Vec<u8> {
        let mut notes = Vec::new();
        push_note(&mut notes, 18, b"Foo\0", &0xDEAD_BEEFu32.to_le_bytes());
        push_note(&mut notes, 3, b"Xen\0", b"4.17");
        push_note(&mut notes, 18, b"Xen\0", &entry.to_le_bytes());
        notes
    }

    #[test]
    fn finds_the_xen_phys32_entry() {
        let k = PvhKernel::from_bytes(build_elf(&xen_notes(0x0100_0000)))
            .expect("PVH ELF parses");
        assert_eq!(k.entry_point_32(), 0x0100_0000);
        assert_eq!(k.load_end(), LOAD_PADDR + LOAD_BYTES.len() as u64);
        assert_eq!(k.segments().len(), 1);
        assert_eq!(k.segments()[0].paddr, LOAD_PADDR);
        assert_eq!(k.bytes().len(), 0x400);
    }

    #[test]
    fn rejects_an_elf_without_the_xen_note() {
        let mut notes = Vec::new();
        push_note(&mut notes, 18, b"Foo\0", &0u32.to_le_bytes());
        let err = PvhKernel::from_bytes(build_elf(&notes))
            .expect_err("a kernel without the note must be rejected");
        assert!(err.to_string().contains("CONFIG_PVH"), "got: {err}");
    }

    #[test]
    fn rejects_a_short_note_descriptor() {
        let mut notes = Vec::new();
        push_note(&mut notes, 18, b"Xen\0", &[0u8, 1]);
        let err = PvhKernel::from_bytes(build_elf(&notes))
            .expect_err("a 2-byte descriptor cannot hold an entry point");
        assert!(
            err.to_string().contains("shorter than 4 bytes"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_a_non_elf_file() {
        let err = PvhKernel::from_bytes(vec![0u8; 512])
            .expect_err("a zero-filled file is not an ELF image");
        assert!(err.to_string().contains("not an ELF image"), "got: {err}");
    }

    #[test]
    fn note_walk_terminates_on_a_truncated_segment() {
        // Header claims a 4 GiB descriptor that the segment cannot hold.
        let mut notes = Vec::new();
        notes.extend_from_slice(&4u32.to_le_bytes());
        notes.extend_from_slice(&u32::MAX.to_le_bytes());
        notes.extend_from_slice(&18u32.to_le_bytes());
        notes.extend_from_slice(b"Xen\0");
        let err = PvhKernel::from_bytes(build_elf(&notes))
            .expect_err("a truncated descriptor must not be accepted");
        assert!(err.to_string().contains("CONFIG_PVH"), "got: {err}");
    }
}
