// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SMBIOS 2.8 table generation for bhyve-based VMs.
//!
//! Generates the SMBIOS entry point (anchor) and structure table for
//! delivery to the guest via fw_cfg entries:
//!   - `etc/smbios/smbios-anchor`: the 31-byte entry point
//!   - `etc/smbios/smbios-tables`: the concatenated structure tables

// ── SMBIOS structure type constants ──────────────────────────────────

const SMBIOS_TYPE_BIOS: u8 = 0;
const SMBIOS_TYPE_SYSTEM: u8 = 1;
const SMBIOS_TYPE_BOARD: u8 = 2;
const SMBIOS_TYPE_CHASSIS: u8 = 3;
const SMBIOS_TYPE_PROCESSOR: u8 = 4;
const SMBIOS_TYPE_MEMARRAY: u8 = 16;
const SMBIOS_TYPE_MEMDEVICE: u8 = 17;
const SMBIOS_TYPE_MEMARRAYMAP: u8 = 19;
const SMBIOS_TYPE_BOOT: u8 = 32;
const SMBIOS_TYPE_EOT: u8 = 127;

// ── BIOS characteristics flags (Type 0) ──────────────────────────────

const SMBIOS_FL_ISA: u64 = 0x0000_0010;
const SMBIOS_FL_PCI: u64 = 0x0000_0080;
const SMBIOS_FL_SHADOW: u64 = 0x0000_1000;
const SMBIOS_FL_CDBOOT: u64 = 0x0000_8000;
const SMBIOS_FL_EDD: u64 = 0x0008_0000;

const SMBIOS_XB1_FL_ACPI: u8 = 0x01;
const SMBIOS_XB2_FL_BBS: u8 = 0x01;
const SMBIOS_XB2_FL_VM: u8 = 0x10;

// ── Processor flags (Type 4) ─────────────────────────────────────────

const SMBIOS_PRT_CENTRAL: u8 = 0x03;
const SMBIOS_PRF_OTHER: u8 = 0x01;
const SMBIOS_PRS_PRESENT: u8 = 0x40;
const SMBIOS_PRS_ENABLED: u8 = 0x01;
const SMBIOS_PRU_NONE: u8 = 0x06;
const SMBIOS_PFL_64B: u16 = 0x04;

// ── Memory flags ─────────────────────────────────────────────────────

const SMBIOS_MAL_SYSMB: u8 = 0x03;
const SMBIOS_MAU_SYSTEM: u8 = 0x03;
const SMBIOS_MAE_NONE: u8 = 0x03;
const SMBIOS_MDFF_UNKNOWN: u8 = 0x02;
const SMBIOS_MDT_UNKNOWN: u8 = 0x02;
const SMBIOS_MDF_UNKNOWN: u16 = 0x0004;

// ── Entry point size ─────────────────────────────────────────────────

/// SMBIOS 2.x entry point is 31 bytes.
const ENTRY_POINT_LEN: u8 = 0x1F;

/// Configuration for SMBIOS table generation.
#[derive(Debug, Default)]
pub struct SmbiosConfig {
    /// VM name, used as the product name in Type 1.
    pub vm_name: String,
    /// Optional UUID string (e.g. from `-U` flag). If None, the UUID
    /// field in Type 1 is zeroed.
    pub uuid: Option<String>,
    /// Number of virtual CPUs.
    pub num_cpus: u32,
    /// Total guest memory in megabytes.
    pub memory_mb: u64,
    /// Type 1 manufacturer (from -B flag).
    pub manufacturer: Option<String>,
    /// Type 1 product name (from -B flag, overrides vm_name).
    pub product: Option<String>,
    /// Type 1 version string (from -B flag).
    pub version: Option<String>,
    /// Type 1 serial number (from -B flag).
    pub serial: Option<String>,
    /// Type 1 SKU number (from -B flag).
    pub sku: Option<String>,
    /// Type 1 family (from -B flag).
    pub family: Option<String>,
}

/// A `-B` value or a VM size this generator will not encode.
#[derive(Debug, thiserror::Error)]
pub enum SmbiosError {
    /// An empty value would emit a bare NUL, which is the double-NUL
    /// that ends a structure: the guest would read the strings that
    /// follow as the next structure header.
    #[error("-B {key}= is empty; SMBIOS has no empty string")]
    EmptyValue { key: &'static str },
    /// Type 17 and Type 19 describe the range `0..memory_mb`, which is
    /// not a range when the VM has no memory.
    #[error("SMBIOS needs a non-zero memory size")]
    NoMemory,
    /// The structure table has a fixed window below 1 MiB, and the
    /// legacy ACPI tables sit right above it.
    #[error("SMBIOS table is {len} bytes, and only {max} fit below the legacy ACPI tables")]
    TableTooLarge { len: usize, max: usize },
}

/// Parse the bhyve `-B` SMBIOS string.
///
/// Format: `type,key=value,key=value,...`
/// Example: `1,manufacturer=Joyent,product=SmartDC HVM,serial=myzone`
///
/// Only type 1 (System Information) is supported. Other types are
/// silently ignored.
pub fn parse_smbios_flag(s: &str) -> Result<SmbiosConfig, SmbiosError> {
    let mut config = SmbiosConfig {
        vm_name: String::new(),
        uuid: None,
        num_cpus: 1,
        memory_mb: 256,
        manufacturer: None,
        product: None,
        version: None,
        serial: None,
        sku: None,
        family: None,
    };

    // First token is the type number
    let parts: Vec<&str> = s.splitn(2, ',').collect();

    // Only handle type 1 (System Information)
    if parts[0].trim() != "1" {
        return Ok(config);
    }

    if let Some(kvs) = parts.get(1) {
        for kv in kvs.split(',') {
            let Some(eq) = kv.find('=') else { continue };
            let (key, val) = (&kv[..eq], &kv[eq + 1..]);
            let (field, name) = match key {
                "manufacturer" => (&mut config.manufacturer, "manufacturer"),
                "product" => (&mut config.product, "product"),
                "version" => (&mut config.version, "version"),
                "serial" => (&mut config.serial, "serial"),
                "sku" => (&mut config.sku, "sku"),
                "family" => (&mut config.family, "family"),
                _ => continue,
            };
            if val.is_empty() {
                return Err(SmbiosError::EmptyValue { key: name });
            }
            *field = Some(val.to_string());
        }
    }

    Ok(config)
}

/// Generate SMBIOS tables suitable for fw_cfg delivery.
///
/// Returns `(anchor, tables)`:
/// - `anchor`: the 31-byte SMBIOS entry point (goes into
///   `etc/smbios/smbios-anchor`)
/// - `tables`: the concatenated structure tables (goes into
///   `etc/smbios/smbios-tables`)
pub fn generate_smbios(
    config: &SmbiosConfig,
    max_table_len: usize,
) -> Result<(Vec<u8>, Vec<u8>), SmbiosError> {
    if config.memory_mb == 0 {
        return Err(SmbiosError::NoMemory);
    }
    let mut tables = Vec::with_capacity(1024);
    let mut handle: u16 = 0;
    let mut max_struct_size: u16 = 0;
    let mut num_structures: u16 = 0;

    // Helper: emit a structure, track max size and count.
    let mut emit = |data: Vec<u8>| {
        let len = data.len() as u16;
        if len > max_struct_size {
            max_struct_size = len;
        }
        num_structures += 1;
        tables.extend_from_slice(&data);
    };

    emit(build_type0(&mut handle));

    emit(build_type1(config, &mut handle));

    let chassis_handle = handle + 1; // Type 3 handle
    emit(build_type2(chassis_handle, &mut handle));

    emit(build_type3(&mut handle));

    for cpu_idx in 0..config.num_cpus {
        emit(build_type4(cpu_idx, &mut handle));
    }

    let memarray_handle = handle;
    emit(build_type16(config, &mut handle));

    // One DIMM covers all guest memory.
    emit(build_type17(config, memarray_handle, &mut handle));

    emit(build_type19(config, memarray_handle, &mut handle));

    emit(build_type32(&mut handle));

    emit(build_type127(&mut handle));

    // The anchor records the length in 16 bits, and the writer puts the
    // table in a fixed window under 1 MiB. The caller states both
    // bounds. Truncation must never be used to meet them.
    let max_table_len = max_table_len.min(u16::MAX as usize);
    if tables.len() > max_table_len {
        return Err(SmbiosError::TableTooLarge {
            len: tables.len(),
            max: max_table_len,
        });
    }

    let anchor =
        build_entry_point(max_struct_size, tables.len() as u16, num_structures);

    Ok((anchor, tables))
}

// ── Structure builder helpers ────────────────────────────────────────
//
// Each SMBIOS structure consists of:
//   - Header: type (1B) + length (1B) + handle (2B)
//   - Formatted data (variable, type-specific)
//   - Unformatted string section: NUL-terminated strings, followed by
//     an extra NUL (double-NUL terminator). If there are no strings,
//     two NUL bytes are still required.
//
// String references in formatted data use 1-based indices.

/// Start a structure with header bytes. Returns the buffer.
fn start_struct(stype: u8, formatted_len: usize, handle: u16) -> Vec<u8> {
    let length = (4 + formatted_len) as u8; // header(4) + formatted data
    let mut buf = Vec::with_capacity(formatted_len + 64);
    buf.push(stype);
    buf.push(length);
    buf.extend_from_slice(&handle.to_le_bytes());
    buf
}

/// Append a string to the string section and return its 1-based index.
fn push_string(strings: &mut Vec<String>, s: &str) -> u8 {
    strings.push(s.to_string());
    strings.len() as u8
}

/// Finalize a structure by appending the string section with double-NUL
/// termination.
fn finalize_struct(buf: &mut Vec<u8>, strings: &[String]) {
    if strings.is_empty() {
        // No strings: two NUL bytes required
        buf.push(0);
        buf.push(0);
    } else {
        for s in strings {
            buf.extend_from_slice(s.as_bytes());
            buf.push(0); // NUL-terminate each string
        }
        buf.push(0); // Extra NUL for double-NUL termination
    }
}

/// Allocate the next handle and return it.
fn next_handle(handle: &mut u16) -> u16 {
    let h = *handle;
    *handle += 1;
    h
}

// ── Type 0: BIOS Information ─────────────────────────────────────────

fn build_type0(handle: &mut u16) -> Vec<u8> {
    let h = next_handle(handle);
    let mut strings = Vec::new();

    let vendor_idx = push_string(&mut strings, "BHYVE");
    let version_idx = push_string(&mut strings, "14.0");
    let date_idx = push_string(&mut strings, "03/31/2026");

    // Formatted area: 20 bytes after header
    let formatted_len = 20;
    let mut buf = start_struct(SMBIOS_TYPE_BIOS, formatted_len, h);

    buf.push(vendor_idx); // vendor string
    buf.push(version_idx); // version string
    buf.extend_from_slice(&0xF000u16.to_le_bytes()); // address segment
    buf.push(date_idx); // release date string
    buf.push(0x00); // BIOS size (64k * (n+1))

    // BIOS characteristics (8 bytes)
    let cflags: u64 = SMBIOS_FL_ISA
        | SMBIOS_FL_PCI
        | SMBIOS_FL_SHADOW
        | SMBIOS_FL_CDBOOT
        | SMBIOS_FL_EDD;
    buf.extend_from_slice(&cflags.to_le_bytes());

    // Extension bytes
    buf.push(SMBIOS_XB1_FL_ACPI);
    buf.push(SMBIOS_XB2_FL_BBS | SMBIOS_XB2_FL_VM);

    // System BIOS major/minor release
    buf.push(0);
    buf.push(0);
    // Embedded controller firmware major/minor release
    buf.push(0xFF);
    buf.push(0xFF);

    finalize_struct(&mut buf, &strings);
    buf
}

// ── Type 1: System Information ───────────────────────────────────────

fn build_type1(config: &SmbiosConfig, handle: &mut u16) -> Vec<u8> {
    let h = next_handle(handle);
    let mut strings = Vec::new();

    let mfr = config.manufacturer.as_deref().unwrap_or("Joyent");
    let mfr_idx = push_string(&mut strings, mfr);

    let prod = config.product.as_deref().unwrap_or("SmartDC HVM");
    let prod_idx = push_string(&mut strings, prod);

    let ver = config.version.as_deref().unwrap_or("1.0");
    let ver_idx = push_string(&mut strings, ver);

    let serial = config
        .serial
        .as_deref()
        .or(config.uuid.as_deref())
        .unwrap_or("None");
    let serial_idx = push_string(&mut strings, serial);

    let sku = config.sku.as_deref().unwrap_or("None");
    let sku_idx = push_string(&mut strings, sku);

    let family = config.family.as_deref().unwrap_or("Virtual Machine");
    let family_idx = push_string(&mut strings, family);

    // Formatted area: 21 bytes after header
    let formatted_len = 21;
    let mut buf = start_struct(SMBIOS_TYPE_SYSTEM, formatted_len, h);

    buf.push(mfr_idx); // manufacturer
    buf.push(prod_idx); // product name
    buf.push(ver_idx); // version
    buf.push(serial_idx); // serial number

    // UUID (16 bytes), zero when the config has none.
    let uuid_bytes = parse_uuid(config.uuid.as_deref());
    buf.extend_from_slice(&uuid_bytes);

    buf.push(0x06); // wake-up type: power switch
    buf.push(sku_idx); // SKU
    buf.push(family_idx); // family

    finalize_struct(&mut buf, &strings);
    buf
}

/// Parse a UUID string into the 16-byte mixed-endian form SMBIOS
/// uses for the Type 1 UUID. Returns zeroes for `None` or a bad string.
fn parse_uuid(uuid_str: Option<&str>) -> [u8; 16] {
    let mut out = [0u8; 16];
    let s = match uuid_str {
        Some(s) => s,
        None => return out,
    };

    let hex: String = s.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return out;
    }

    let mut bytes = [0u8; 16];
    for i in 0..16 {
        bytes[i] = match u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16) {
            Ok(b) => b,
            Err(_) => return out,
        };
    }

    // SMBIOS mixed-endian: first 3 fields are little-endian
    // time_low (4 bytes LE)
    out[0] = bytes[3];
    out[1] = bytes[2];
    out[2] = bytes[1];
    out[3] = bytes[0];
    // time_mid (2 bytes LE)
    out[4] = bytes[5];
    out[5] = bytes[4];
    // time_hi_and_version (2 bytes LE)
    out[6] = bytes[7];
    out[7] = bytes[6];
    // Remaining 8 bytes are big-endian
    out[8..16].copy_from_slice(&bytes[8..16]);

    out
}

// ── Type 2: Baseboard Information ────────────────────────────────────

fn build_type2(chassis_handle: u16, handle: &mut u16) -> Vec<u8> {
    let h = next_handle(handle);
    let mut strings = Vec::new();

    let mfr_idx = push_string(&mut strings, "SmartOS");
    let prod_idx = push_string(&mut strings, "Virtual Machine");
    let ver_idx = push_string(&mut strings, "1.0");
    let serial_idx = push_string(&mut strings, "None");
    let asset_idx = push_string(&mut strings, "None");
    let loc_idx = push_string(&mut strings, "None");

    // Formatted area: 11 bytes after header
    let formatted_len = 11;
    let mut buf = start_struct(SMBIOS_TYPE_BOARD, formatted_len, h);

    buf.push(mfr_idx); // manufacturer
    buf.push(prod_idx); // product name
    buf.push(ver_idx); // version
    buf.push(serial_idx); // serial number
    buf.push(asset_idx); // asset tag
    buf.push(0x01); // feature flags: hosting board
    buf.push(loc_idx); // location in chassis
    buf.extend_from_slice(&chassis_handle.to_le_bytes()); // chassis handle
    buf.push(0x0A); // board type: motherboard
    buf.push(0); // number of contained object handles

    finalize_struct(&mut buf, &strings);
    buf
}

// ── Type 3: System Enclosure ─────────────────────────────────────────

fn build_type3(handle: &mut u16) -> Vec<u8> {
    let h = next_handle(handle);
    let mut strings = Vec::new();

    let mfr_idx = push_string(&mut strings, "SmartOS");
    let ver_idx = push_string(&mut strings, "1.0");
    let serial_idx = push_string(&mut strings, "None");
    let asset_idx = push_string(&mut strings, "None");
    let sku_idx = push_string(&mut strings, "None");

    // Formatted area: 17 bytes after header
    let formatted_len = 17;
    let mut buf = start_struct(SMBIOS_TYPE_CHASSIS, formatted_len, h);

    buf.push(mfr_idx); // manufacturer
    buf.push(0x02); // type: unknown
    buf.push(ver_idx); // version
    buf.push(serial_idx); // serial number
    buf.push(asset_idx); // asset tag
    buf.push(0x03); // boot-up state: safe
    buf.push(0x03); // power supply state: safe
    buf.push(0x03); // thermal state: safe
    buf.push(0x03); // security status: none
    buf.extend_from_slice(&0u32.to_le_bytes()); // OEM-specific data
    buf.push(0); // height in U's
    buf.push(0); // number of power cords
    buf.push(0); // number of contained elements
    buf.push(0); // contained element record length
    buf.push(sku_idx); // SKU number

    finalize_struct(&mut buf, &strings);
    buf
}

// ── Type 4: Processor Information ────────────────────────────────────

fn build_type4(cpu_idx: u32, handle: &mut u16) -> Vec<u8> {
    let h = next_handle(handle);
    let mut strings = Vec::new();

    let socket_str = format!("CPU #{}", cpu_idx);
    let socket_idx = push_string(&mut strings, &socket_str);
    let mfr_idx = push_string(&mut strings, " ");
    let ver_idx = push_string(&mut strings, " ");
    let serial_idx = push_string(&mut strings, "None");
    let asset_idx = push_string(&mut strings, "None");
    let part_idx = push_string(&mut strings, "None");

    // Formatted area: 38 bytes after the 4-byte header
    let formatted_len = 38;
    let mut buf = start_struct(SMBIOS_TYPE_PROCESSOR, formatted_len, h);

    buf.push(socket_idx); // socket designation
    buf.push(SMBIOS_PRT_CENTRAL); // processor type
    buf.push(SMBIOS_PRF_OTHER); // processor family
    buf.push(mfr_idx); // manufacturer
    buf.extend_from_slice(&0u64.to_le_bytes()); // processor ID (cpuid)
    buf.push(ver_idx); // version
    buf.push(0); // voltage
    buf.extend_from_slice(&0u16.to_le_bytes()); // external clock (MHz)
    buf.extend_from_slice(&0u16.to_le_bytes()); // max speed (MHz)
    buf.extend_from_slice(&0u16.to_le_bytes()); // current speed (MHz)
    buf.push(SMBIOS_PRS_PRESENT | SMBIOS_PRS_ENABLED); // status
    buf.push(SMBIOS_PRU_NONE); // upgrade
    buf.extend_from_slice(&0xFFFFu16.to_le_bytes()); // L1 cache handle
    buf.extend_from_slice(&0xFFFFu16.to_le_bytes()); // L2 cache handle
    buf.extend_from_slice(&0xFFFFu16.to_le_bytes()); // L3 cache handle
    buf.push(serial_idx); // serial number
    buf.push(asset_idx); // asset tag
    buf.push(part_idx); // part number
    buf.push(0); // cores per socket
    buf.push(0); // enabled cores
    buf.push(0); // threads per socket
    buf.extend_from_slice(&SMBIOS_PFL_64B.to_le_bytes()); // proc characteristics
    buf.extend_from_slice(&(SMBIOS_PRF_OTHER as u16).to_le_bytes()); // family 2

    finalize_struct(&mut buf, &strings);
    buf
}

// ── Type 16: Physical Memory Array ───────────────────────────────────

fn build_type16(config: &SmbiosConfig, handle: &mut u16) -> Vec<u8> {
    let h = next_handle(handle);

    // Formatted area: 11 bytes after header
    let formatted_len = 11;
    let mut buf = start_struct(SMBIOS_TYPE_MEMARRAY, formatted_len, h);

    buf.push(SMBIOS_MAL_SYSMB); // location: system board
    buf.push(SMBIOS_MAU_SYSTEM); // use: system memory
    buf.push(SMBIOS_MAE_NONE); // error correction: none

    // Maximum capacity in KB. 0x80000000 means "see Extended Maximum
    // Capacity", which this 11-byte form does not carry.
    buf.extend_from_slice(&0x8000_0000u32.to_le_bytes());
    // Error info handle: not provided
    buf.extend_from_slice(&0xFFFEu16.to_le_bytes());
    // Number of memory devices
    buf.extend_from_slice(&1u16.to_le_bytes());

    finalize_struct(&mut buf, &[]);

    // Replace the placeholder with the real size when it fits the
    // 32-bit KB field.
    let max_kb = config.memory_mb.saturating_mul(1024);
    if max_kb <= 0x7FFF_FFFF {
        let offset = 4 + 3; // header + location/use/ecc
        buf[offset..offset + 4].copy_from_slice(&(max_kb as u32).to_le_bytes());
    }

    buf
}

// ── Type 17: Memory Device ───────────────────────────────────────────

fn build_type17(
    config: &SmbiosConfig,
    memarray_handle: u16,
    handle: &mut u16,
) -> Vec<u8> {
    let h = next_handle(handle);
    let mut strings = Vec::new();

    let dloc_idx = push_string(&mut strings, "DIMM 0");
    let bloc_idx = push_string(&mut strings, "Bank 0");
    let mfr_idx = push_string(&mut strings, " ");
    let serial_idx = push_string(&mut strings, "None");
    let asset_idx = push_string(&mut strings, "None");
    let part_idx = push_string(&mut strings, "None");

    // Formatted area: 24 bytes after header
    let formatted_len = 24;
    let mut buf = start_struct(SMBIOS_TYPE_MEMDEVICE, formatted_len, h);

    buf.extend_from_slice(&memarray_handle.to_le_bytes()); // phys mem array handle
    buf.extend_from_slice(&0xFFFEu16.to_le_bytes()); // error info handle
    buf.extend_from_slice(&64u16.to_le_bytes()); // total width (bits)
    buf.extend_from_slice(&64u16.to_le_bytes()); // data width (bits)

    // Size in MB (0x7FFF = use extended size field)
    let size_mb = config.memory_mb;
    if size_mb <= 0x7FFE {
        // Bit 15 = 0 means MB
        buf.extend_from_slice(&(size_mb as u16).to_le_bytes());
    } else {
        buf.extend_from_slice(&0x7FFFu16.to_le_bytes());
    }

    buf.push(SMBIOS_MDFF_UNKNOWN); // form factor
    buf.push(0); // set (0 = none)
    buf.push(dloc_idx); // device locator
    buf.push(bloc_idx); // bank locator
    buf.push(SMBIOS_MDT_UNKNOWN); // memory type
    buf.extend_from_slice(&SMBIOS_MDF_UNKNOWN.to_le_bytes()); // type detail
    buf.extend_from_slice(&0u16.to_le_bytes()); // max speed (MHz)
    buf.push(mfr_idx); // manufacturer
    buf.push(serial_idx); // serial number
    buf.push(asset_idx); // asset tag
    buf.push(part_idx); // part number
    buf.push(0); // attributes (rank unknown)

    finalize_struct(&mut buf, &strings);
    buf
}

// ── Type 19: Memory Array Mapped Address ─────────────────────────────

fn build_type19(
    config: &SmbiosConfig,
    memarray_handle: u16,
    handle: &mut u16,
) -> Vec<u8> {
    let h = next_handle(handle);

    // Formatted area: 27 bytes after header
    let formatted_len = 27;
    let mut buf = start_struct(SMBIOS_TYPE_MEMARRAYMAP, formatted_len, h);

    // generate_smbios refuses a zero size, so both ends are >= 1.
    let mem_bytes = config.memory_mb.saturating_mul(1024 * 1024);
    let mem_kb = config.memory_mb.saturating_mul(1024);

    if mem_kb <= 0xFFFF_FFFE {
        // Use KB-based addresses
        buf.extend_from_slice(&0u32.to_le_bytes()); // starting addr (KB)
        buf.extend_from_slice(&((mem_kb - 1) as u32).to_le_bytes()); // ending addr (KB)
    } else {
        // Use extended addresses
        buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    }

    buf.extend_from_slice(&memarray_handle.to_le_bytes()); // array handle
    buf.push(1); // partition width

    // Extended addresses (bytes)
    buf.extend_from_slice(&0u64.to_le_bytes()); // extended start
    buf.extend_from_slice(&(mem_bytes - 1).to_le_bytes()); // extended end

    finalize_struct(&mut buf, &[]);
    buf
}

// ── Type 32: System Boot Information ─────────────────────────────────

fn build_type32(handle: &mut u16) -> Vec<u8> {
    let h = next_handle(handle);

    // Formatted area: 7 bytes (6 reserved + 1 status)
    let formatted_len = 7;
    let mut buf = start_struct(SMBIOS_TYPE_BOOT, formatted_len, h);

    // 6 reserved bytes
    buf.extend_from_slice(&[0u8; 6]);
    // Boot status: no errors
    buf.push(0);

    finalize_struct(&mut buf, &[]);
    buf
}

// ── Type 127: End of Table ───────────────────────────────────────────

fn build_type127(handle: &mut u16) -> Vec<u8> {
    let h = next_handle(handle);

    // No formatted data beyond header
    let mut buf = start_struct(SMBIOS_TYPE_EOT, 0, h);
    finalize_struct(&mut buf, &[]);
    buf
}

// ── Entry Point ──────────────────────────────────────────────────────

/// Build the 31-byte SMBIOS 2.x entry point structure.
///
/// Layout (from the SMBIOS 2.8 specification):
/// ```text
/// Offset  Size  Field
/// 0x00    4     Anchor string: "_SM_"
/// 0x04    1     Entry point checksum
/// 0x05    1     Entry point length (0x1F)
/// 0x06    1     Major version (2)
/// 0x07    1     Minor version (8)
/// 0x08    2     Maximum structure size
/// 0x0A    1     Entry point revision (0)
/// 0x0B    5     Formatted area (zeroes)
/// 0x10    5     Intermediate anchor: "_DMI_"
/// 0x15    1     Intermediate checksum
/// 0x16    2     Structure table length
/// 0x18    4     Structure table address (0: fw_cfg places the table)
/// 0x1C    2     Number of SMBIOS structures
/// 0x1E    1     BCD revision (0x28 for 2.8)
/// ```
fn build_entry_point(
    max_struct_size: u16,
    table_length: u16,
    num_structures: u16,
) -> Vec<u8> {
    let mut buf = vec![0u8; ENTRY_POINT_LEN as usize];

    buf[0x00..0x04].copy_from_slice(b"_SM_");
    // 0x04: checksum, set last.
    buf[0x05] = ENTRY_POINT_LEN;
    buf[0x06] = 2;
    buf[0x07] = 8;
    buf[0x08..0x0A].copy_from_slice(&max_struct_size.to_le_bytes());
    buf[0x0A] = 0;
    // 0x0B-0x0F: formatted area, left zero.

    buf[0x10..0x15].copy_from_slice(b"_DMI_");
    // 0x15: intermediate checksum, set last.
    buf[0x16..0x18].copy_from_slice(&table_length.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&0u32.to_le_bytes());
    buf[0x1C..0x1E].copy_from_slice(&num_structures.to_le_bytes());
    buf[0x1E] = 0x28;

    // The intermediate checksum covers 0x10..0x1F and the other covers
    // 0x00..0x10. Each half sums to zero, so the full 31-byte entry
    // point also sums to zero, as its checksum rule requires.
    fix_checksum_at(&mut buf, 0x15, 0x10, 0x0F);
    fix_checksum_at(&mut buf, 0x04, 0x00, 0x10);

    buf
}

/// Set the byte at `cksum_offset` so that the sum of all bytes in
/// `buf[start..start+len]` is 0 mod 256.
fn fix_checksum_at(
    buf: &mut [u8],
    cksum_offset: usize,
    start: usize,
    len: usize,
) {
    buf[cksum_offset] = 0;
    let sum: u8 = buf[start..start + len]
        .iter()
        .fold(0u8, |acc, &b| acc.wrapping_add(b));
    buf[cksum_offset] = 0u8.wrapping_sub(sum);
}

#[cfg(test)]
mod tests;
