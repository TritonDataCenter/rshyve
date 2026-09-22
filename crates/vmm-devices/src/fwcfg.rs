// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! QEMU fw_cfg device emulation.
//!
//! The fw_cfg device provides a simple interface for the guest firmware
//! to discover configuration data (ACPI tables, E820 memory map, etc.)
//! via PIO ports.
//!
//! # Ports
//!
//! - **0x510** (selector): 2-byte write selects the item to read.
//! - **0x511** (data): 1-byte read returns sequential bytes from the
//!   selected item.
//!
//! # Item layout
//!
//! - Selectors 0x0000..0x001F are reserved for legacy/built-in items.
//! - Selectors 0x0020..0x3FFF are used for named file entries.
//! - Selector 0x0019 is the file directory listing all named entries.
//!
//! # File directory format
//!
//! ```text
//! count: u32 (big-endian)
//! entries[count]:
//!     size:     u32 (big-endian)
//!     selector: u16 (big-endian)
//!     reserved: u16
//!     name:     [u8; 56] (NUL-padded)
//! ```

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use vmm_core::common::RWOp;
use vmm_core::pio::{PioBus, PioFn};

/// PIO port for the selector register.
const FWCFG_PORT_SEL: u16 = 0x510;
/// PIO port for the data register. The handler sees it as offset 1 of
/// the selector port registration.
#[allow(dead_code)]
const FWCFG_PORT_DATA: u16 = 0x511;

/// Selector for the "QEMU" signature.
const SEL_SIGNATURE: u16 = 0x0000;
/// Selector for the interface version/ID.
const SEL_ID: u16 = 0x0001;
/// Selector for the file directory.
const SEL_FILE_DIR: u16 = 0x0019;

/// First selector available for named file entries.
const FIRST_FILE_SELECTOR: u16 = 0x0020;

/// Maximum length of a file entry name (including NUL terminator).
const MAX_NAME_LEN: usize = 56;

/// Size of a single file directory entry (on the wire).
const FILE_ENTRY_SIZE: usize = 4 + 2 + 2 + MAX_NAME_LEN; // 64 bytes

/// Metadata for a named file entry in the directory.
struct FwCfgFile {
    name: String,
    selector: u16,
    size: u32,
}

struct FwCfgInner {
    /// All items indexed by selector.
    items: BTreeMap<u16, Vec<u8>>,
    /// Currently selected item.
    cur_selector: u16,
    /// Read offset within the currently selected item.
    cur_offset: usize,
    /// Next available selector for named file entries.
    next_file_selector: u16,
    /// Named file directory entries.
    file_dir: Vec<FwCfgFile>,
}

/// QEMU fw_cfg device.
pub struct FwCfg {
    inner: Mutex<FwCfgInner>,
}

impl FwCfg {
    /// Create a new fw_cfg device with built-in items (signature, ID).
    pub fn new() -> Arc<Self> {
        let mut items = BTreeMap::new();

        items.insert(SEL_SIGNATURE, b"QEMU".to_vec());

        // Interface ID 1: traditional interface, no DMA.
        items.insert(SEL_ID, 1u32.to_le_bytes().to_vec());

        let this = Arc::new(Self {
            inner: Mutex::new(FwCfgInner {
                items,
                cur_selector: 0,
                cur_offset: 0,
                next_file_selector: FIRST_FILE_SELECTOR,
                file_dir: Vec::new(),
            }),
        });

        this.rebuild_file_dir();
        this
    }

    /// Insert a named file entry.
    ///
    /// The entry appears in the file directory. Selectors are assigned
    /// in order from 0x0020.
    pub fn insert_named(&self, name: &str, data: Vec<u8>) {
        let mut inner = self.inner.lock().unwrap();

        let selector = inner.next_file_selector;
        inner.next_file_selector += 1;

        let size = data.len() as u32;
        inner.items.insert(selector, data);
        inner.file_dir.push(FwCfgFile {
            name: name.to_owned(),
            selector,
            size,
        });

        // rebuild_file_dir takes the lock itself.
        drop(inner);
        self.rebuild_file_dir();
    }

    /// Insert data at a specific (legacy) selector.
    ///
    /// This does NOT add the item to the file directory.
    pub fn insert_legacy(&self, selector: u16, data: Vec<u8>) {
        let mut inner = self.inner.lock().unwrap();
        inner.items.insert(selector, data);
    }

    /// Register PIO handlers on the given bus.
    ///
    /// Registers a 2-port region at 0x510-0x511:
    /// - Port 0x510: selector (2-byte write, 2-byte read)
    /// - Port 0x511: data (1-byte read)
    pub fn attach(self: &Arc<Self>, pio: &PioBus) {
        let this = Arc::clone(self);
        let handler: Arc<PioFn> =
            Arc::new(move |offset: u16, rwop: RWOp<'_>| {
                match (offset, rwop) {
                    // Selector port write (0x510)
                    (0, RWOp::Write(wo)) => {
                        let sel = wo.read_u16();
                        let mut inner = this.inner.lock().unwrap();
                        inner.cur_selector = sel;
                        inner.cur_offset = 0;
                    }
                    // Selector port read (0x510)
                    (0, RWOp::Read(ro)) => {
                        let inner = this.inner.lock().unwrap();
                        ro.write_u16(inner.cur_selector);
                    }
                    // Data port read (0x511)
                    (1, RWOp::Read(ro)) => {
                        let mut inner = this.inner.lock().unwrap();
                        let byte = Self::read_data_byte(&mut inner);
                        ro.write_u8(byte);
                    }
                    // Data port write (0x511) - ignored per QEMU spec
                    (1, RWOp::Write(_)) => {}
                    _ => {}
                }
            });

        pio.register(FWCFG_PORT_SEL, 2, handler).unwrap();
    }

    /// Read one byte from the currently selected item and advance offset.
    ///
    /// Returns 0x00 if no item is selected, the selector is invalid, or
    /// the read offset is past the end of the data.
    #[inline]
    fn read_data_byte(inner: &mut FwCfgInner) -> u8 {
        let sel = inner.cur_selector;
        let offset = inner.cur_offset;

        let byte = inner
            .items
            .get(&sel)
            .and_then(|data| data.get(offset).copied())
            .unwrap_or(0x00);

        inner.cur_offset = offset.saturating_add(1);
        byte
    }

    /// Rebuild the file directory item (selector 0x19), in the format
    /// the module comment gives, with entries sorted by name.
    fn rebuild_file_dir(&self) {
        let inner = self.inner.lock().unwrap();

        let count = inner.file_dir.len() as u32;
        let dir_size = 4 + inner.file_dir.len() * FILE_ENTRY_SIZE;
        let mut buf = Vec::with_capacity(dir_size);

        buf.extend_from_slice(&count.to_be_bytes());

        let mut sorted: Vec<&FwCfgFile> = inner.file_dir.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));

        for entry in &sorted {
            // size (u32 BE)
            buf.extend_from_slice(&entry.size.to_be_bytes());
            // selector (u16 BE)
            buf.extend_from_slice(&entry.selector.to_be_bytes());
            // reserved (u16)
            buf.extend_from_slice(&0u16.to_be_bytes());
            // name (56 bytes, NUL-padded)
            let name_bytes = entry.name.as_bytes();
            let copy_len = name_bytes.len().min(MAX_NAME_LEN - 1);
            buf.extend_from_slice(&name_bytes[..copy_len]);
            buf.resize(buf.len() + MAX_NAME_LEN - copy_len, 0);
        }

        drop(inner);

        let mut inner = self.inner.lock().unwrap();
        inner.items.insert(SEL_FILE_DIR, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_item() {
        let fwcfg = FwCfg::new();
        let inner = fwcfg.inner.lock().unwrap();
        let sig = inner.items.get(&SEL_SIGNATURE).unwrap();
        assert_eq!(sig, b"QEMU");
    }

    #[test]
    fn id_item() {
        let fwcfg = FwCfg::new();
        let inner = fwcfg.inner.lock().unwrap();
        let id = inner.items.get(&SEL_ID).unwrap();
        assert_eq!(id, &1u32.to_le_bytes());
    }

    #[test]
    fn insert_named_adds_file_entry() {
        let fwcfg = FwCfg::new();
        fwcfg.insert_named("etc/e820", vec![1, 2, 3, 4]);

        let inner = fwcfg.inner.lock().unwrap();

        // Check file entry was created
        assert_eq!(inner.file_dir.len(), 1);
        assert_eq!(inner.file_dir[0].name, "etc/e820");
        assert_eq!(inner.file_dir[0].size, 4);
        assert_eq!(inner.file_dir[0].selector, FIRST_FILE_SELECTOR);

        // Check data is stored
        let data = inner.items.get(&FIRST_FILE_SELECTOR).unwrap();
        assert_eq!(data, &[1, 2, 3, 4]);

        // Check file directory was rebuilt
        let dir = inner.items.get(&SEL_FILE_DIR).unwrap();
        // Count should be 1
        let count = u32::from_be_bytes([dir[0], dir[1], dir[2], dir[3]]);
        assert_eq!(count, 1);
    }

    #[test]
    fn read_data_sequential() {
        let fwcfg = FwCfg::new();

        // Select the signature item
        {
            let mut inner = fwcfg.inner.lock().unwrap();
            inner.cur_selector = SEL_SIGNATURE;
            inner.cur_offset = 0;
        }

        // Read bytes sequentially
        let mut inner = fwcfg.inner.lock().unwrap();
        assert_eq!(FwCfg::read_data_byte(&mut inner), b'Q');
        assert_eq!(FwCfg::read_data_byte(&mut inner), b'E');
        assert_eq!(FwCfg::read_data_byte(&mut inner), b'M');
        assert_eq!(FwCfg::read_data_byte(&mut inner), b'U');
        // Past end returns 0
        assert_eq!(FwCfg::read_data_byte(&mut inner), 0);
    }

    #[test]
    fn selector_resets_offset() {
        let fwcfg = FwCfg::new();
        let mut inner = fwcfg.inner.lock().unwrap();

        // Read a couple bytes from signature
        inner.cur_selector = SEL_SIGNATURE;
        inner.cur_offset = 0;
        assert_eq!(FwCfg::read_data_byte(&mut inner), b'Q');
        assert_eq!(FwCfg::read_data_byte(&mut inner), b'E');
        assert_eq!(inner.cur_offset, 2);

        // Setting a new selector resets offset
        inner.cur_selector = SEL_SIGNATURE;
        inner.cur_offset = 0;
        assert_eq!(FwCfg::read_data_byte(&mut inner), b'Q');
    }

    #[test]
    fn invalid_selector_returns_zero() {
        let fwcfg = FwCfg::new();
        let mut inner = fwcfg.inner.lock().unwrap();
        inner.cur_selector = 0xFFFF;
        inner.cur_offset = 0;
        assert_eq!(FwCfg::read_data_byte(&mut inner), 0);
    }

    #[test]
    fn file_directory_format() {
        let fwcfg = FwCfg::new();
        fwcfg.insert_named("etc/test", vec![0xAA, 0xBB]);

        let inner = fwcfg.inner.lock().unwrap();
        let dir = inner.items.get(&SEL_FILE_DIR).unwrap();

        // Count
        let count = u32::from_be_bytes([dir[0], dir[1], dir[2], dir[3]]);
        assert_eq!(count, 1);

        // First entry starts at offset 4
        let entry = &dir[4..];
        // size (u32 BE) = 2
        let size = u32::from_be_bytes([entry[0], entry[1], entry[2], entry[3]]);
        assert_eq!(size, 2);
        // selector (u16 BE)
        let sel = u16::from_be_bytes([entry[4], entry[5]]);
        assert_eq!(sel, FIRST_FILE_SELECTOR);
        // reserved
        assert_eq!(entry[6], 0);
        assert_eq!(entry[7], 0);
        // name starts at offset 8
        assert_eq!(&entry[8..16], b"etc/test");
    }

    #[test]
    fn insert_legacy_does_not_add_to_directory() {
        let fwcfg = FwCfg::new();
        fwcfg.insert_legacy(0x0005, vec![2, 0]); // nb_cpus

        let inner = fwcfg.inner.lock().unwrap();
        // Should have the data
        assert!(inner.items.contains_key(&0x0005));
        // But no file directory entry
        assert!(inner.file_dir.is_empty());
    }

    #[test]
    fn multiple_named_entries_sorted() {
        let fwcfg = FwCfg::new();
        fwcfg.insert_named("etc/zzz", vec![3]);
        fwcfg.insert_named("etc/aaa", vec![1]);
        fwcfg.insert_named("etc/mmm", vec![2]);

        let inner = fwcfg.inner.lock().unwrap();
        let dir = inner.items.get(&SEL_FILE_DIR).unwrap();

        let count = u32::from_be_bytes([dir[0], dir[1], dir[2], dir[3]]);
        assert_eq!(count, 3);

        // Entries should be sorted: aaa, mmm, zzz
        let entry0 = &dir[4..4 + FILE_ENTRY_SIZE];
        let entry1 = &dir[4 + FILE_ENTRY_SIZE..4 + 2 * FILE_ENTRY_SIZE];
        let entry2 = &dir[4 + 2 * FILE_ENTRY_SIZE..4 + 3 * FILE_ENTRY_SIZE];

        assert_eq!(&entry0[8..15], b"etc/aaa");
        assert_eq!(&entry1[8..15], b"etc/mmm");
        assert_eq!(&entry2[8..15], b"etc/zzz");
    }
}
