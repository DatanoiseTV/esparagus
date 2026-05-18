//! ESP32 partition table parser and resolver.
//!
//! Supports both formats:
//!   * CSV — the text format users write at `partitions.csv`
//!   * Binary — the 0xAA50-magic-prefixed 32-byte records stored in flash,
//!     typically at offset 0x8000. We can read the table from the chip and
//!     use it to resolve named partitions, so callers don't need to track
//!     offsets.
//!
//! Binary format is taken from ESP-IDF's `components/partition_table/`
//! (Apache-2.0); see that source for the canonical spec. Type / subtype
//! enumerations mirror `esp_partition.h`.

use std::path::Path;

use byteorder::{ByteOrder, LittleEndian};
use serde::Serialize;

use crate::error::{Error, Result};

/// Default offset where the binary partition table lives in flash.
pub const PARTITION_TABLE_OFFSET: u32 = 0x8000;
/// Total sector size reserved for the partition table (4 KB).
pub const PARTITION_TABLE_SECTOR: u32 = 0x1000;
/// Maximum bytes of the partition table actually parsed (3 KB → 96 entries).
pub const MAX_PARTITION_LENGTH: usize = 0xC00;
/// Length of one partition entry record.
pub const ENTRY_SIZE: usize = 32;
/// Magic bytes at the start of every entry record.
pub const ENTRY_MAGIC: [u8; 2] = [0xAA, 0x50];
/// Magic bytes at the start of the MD5-checksum record.
pub const MD5_MAGIC: [u8; 2] = [0xEB, 0xEB];

// Type IDs (match esp_partition_type_t).
pub const TYPE_APP: u8 = 0x00;
pub const TYPE_DATA: u8 = 0x01;
pub const TYPE_BOOTLOADER: u8 = 0x02;
pub const TYPE_PARTITION_TABLE: u8 = 0x03;

// App subtypes
pub const APP_FACTORY: u8 = 0x00;
pub const APP_OTA_MIN: u8 = 0x10;
pub const APP_OTA_MAX: u8 = 0x1F; // inclusive: ota_0..ota_15
pub const APP_TEST: u8 = 0x20;
pub const APP_TEE_MIN: u8 = 0x30;
pub const APP_TEE_MAX: u8 = 0x31;

// Data subtypes
pub const DATA_OTA: u8 = 0x00;
pub const DATA_PHY: u8 = 0x01;
pub const DATA_NVS: u8 = 0x02;
pub const DATA_COREDUMP: u8 = 0x03;
pub const DATA_NVS_KEYS: u8 = 0x04;
pub const DATA_EFUSE: u8 = 0x05;
pub const DATA_UNDEFINED: u8 = 0x06;
pub const DATA_ESPHTTPD: u8 = 0x80;
pub const DATA_FAT: u8 = 0x81;
pub const DATA_SPIFFS: u8 = 0x82;
pub const DATA_LITTLEFS: u8 = 0x83;
pub const DATA_TEE_OTA: u8 = 0x90;

// Flags (bit positions)
pub const FLAG_ENCRYPTED: u32 = 1 << 0;
pub const FLAG_READONLY: u32 = 1 << 1;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PartitionEntry {
    pub name: String,
    pub ptype: u8,
    pub subtype: u8,
    pub offset: u32,
    pub size: u32,
    pub flags: u32,
}

impl PartitionEntry {
    pub fn encrypted(&self) -> bool {
        self.flags & FLAG_ENCRYPTED != 0
    }
    pub fn readonly(&self) -> bool {
        self.flags & FLAG_READONLY != 0
    }

    /// Human-readable type name.
    pub fn type_name(&self) -> &'static str {
        match self.ptype {
            TYPE_APP => "app",
            TYPE_DATA => "data",
            TYPE_BOOTLOADER => "bootloader",
            TYPE_PARTITION_TABLE => "partition_table",
            _ => "unknown",
        }
    }

    /// Human-readable subtype name.
    pub fn subtype_name(&self) -> String {
        match (self.ptype, self.subtype) {
            (TYPE_APP, APP_FACTORY) => "factory".into(),
            (TYPE_APP, APP_TEST) => "test".into(),
            (TYPE_APP, s) if (APP_OTA_MIN..=APP_OTA_MAX).contains(&s) => {
                format!("ota_{}", s - APP_OTA_MIN)
            }
            (TYPE_APP, s) if (APP_TEE_MIN..=APP_TEE_MAX).contains(&s) => {
                format!("tee_{}", s - APP_TEE_MIN)
            }
            (TYPE_DATA, DATA_OTA) => "ota".into(),
            (TYPE_DATA, DATA_PHY) => "phy".into(),
            (TYPE_DATA, DATA_NVS) => "nvs".into(),
            (TYPE_DATA, DATA_COREDUMP) => "coredump".into(),
            (TYPE_DATA, DATA_NVS_KEYS) => "nvs_keys".into(),
            (TYPE_DATA, DATA_EFUSE) => "efuse".into(),
            (TYPE_DATA, DATA_UNDEFINED) => "undefined".into(),
            (TYPE_DATA, DATA_ESPHTTPD) => "esphttpd".into(),
            (TYPE_DATA, DATA_FAT) => "fat".into(),
            (TYPE_DATA, DATA_SPIFFS) => "spiffs".into(),
            (TYPE_DATA, DATA_LITTLEFS) => "littlefs".into(),
            (TYPE_DATA, DATA_TEE_OTA) => "tee_ota".into(),
            (_, s) => format!("0x{:02x}", s),
        }
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct PartitionTable {
    pub entries: Vec<PartitionEntry>,
}

impl PartitionTable {
    pub fn find(&self, name: &str) -> Option<&PartitionEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Parse the binary partition table sector read from flash.
    /// Stops at the 32-byte 0xFF end marker; verifies MD5 record if present.
    pub fn from_binary(bytes: &[u8]) -> Result<Self> {
        let mut entries = Vec::new();
        let mut md5_hasher = md5::Md5::new();
        let mut md5_running = true;
        use md5::Digest;
        for (idx, chunk) in bytes.chunks(ENTRY_SIZE).enumerate() {
            if chunk.len() != ENTRY_SIZE {
                return Err(Error::Other(format!(
                    "partition table not a multiple of {}B (entry {})",
                    ENTRY_SIZE, idx
                )));
            }
            if chunk == [0xFF; ENTRY_SIZE] {
                return Ok(Self { entries });
            }
            if chunk[0..2] == MD5_MAGIC {
                let expected = &chunk[16..32];
                let computed = md5_hasher.clone().finalize();
                if computed.as_slice() != expected {
                    return Err(Error::Other(format!(
                        "partition table MD5 mismatch: computed {:x?}, in-table {:x?}",
                        computed, expected
                    )));
                }
                md5_running = false;
                continue;
            }
            if md5_running {
                md5_hasher.update(chunk);
            }
            if chunk[0..2] != ENTRY_MAGIC {
                return Err(Error::Other(format!(
                    "invalid partition magic {:#04x}{:02x} at entry {}",
                    chunk[0], chunk[1], idx
                )));
            }
            let ptype = chunk[2];
            let subtype = chunk[3];
            let offset = LittleEndian::read_u32(&chunk[4..8]);
            let size = LittleEndian::read_u32(&chunk[8..12]);
            let name_bytes = &chunk[12..28];
            let name_end = name_bytes.iter().position(|&b| b == 0).unwrap_or(16);
            let name = String::from_utf8_lossy(&name_bytes[..name_end]).to_string();
            let flags = LittleEndian::read_u32(&chunk[28..32]);
            entries.push(PartitionEntry {
                name,
                ptype,
                subtype,
                offset,
                size,
                flags,
            });
        }
        Err(Error::Other(
            "partition table missing end-of-table marker (32 × 0xFF)".into(),
        ))
    }

    /// Parse a CSV-format partition table.  Supports the same dialect as
    /// IDF's `gen_esp32part.py` for the columns we care about, including
    /// auto-allocated offsets (blank Offset column → starts right after the
    /// previous partition, aligned per type rules) and K/M size suffixes.
    pub fn from_csv(text: &str) -> Result<Self> {
        let mut entries: Vec<PartitionEntry> = Vec::new();
        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
            if fields.len() < 5 {
                return Err(Error::Other(format!(
                    "partition CSV line {} has {} fields, expected ≥ 5",
                    lineno + 1,
                    fields.len()
                )));
            }
            let name = fields[0].to_string();
            if name.len() > 16 {
                return Err(Error::Other(format!(
                    "partition CSV line {}: name {:?} exceeds 16 bytes",
                    lineno + 1,
                    name
                )));
            }
            let ptype = parse_type(fields[1])?;
            let subtype = parse_subtype(ptype, fields[2])?;
            let size = parse_size(fields[4])?;
            // Auto-offset: blank → after previous entry, aligned.
            let offset = if fields[3].is_empty() {
                let next = entries
                    .last()
                    .map(|p| p.offset + p.size)
                    .unwrap_or(PARTITION_TABLE_OFFSET + PARTITION_TABLE_SECTOR);
                align_offset(next, ptype)
            } else {
                parse_offset(fields[3])?
            };
            let flags_str = fields.get(5).copied().unwrap_or("");
            let mut flags = 0u32;
            for f in flags_str.split(':') {
                match f.trim() {
                    "" => {}
                    "encrypted" => flags |= FLAG_ENCRYPTED,
                    "readonly" => flags |= FLAG_READONLY,
                    other => {
                        return Err(Error::Other(format!(
                            "partition CSV line {}: unknown flag '{}'",
                            lineno + 1,
                            other
                        )))
                    }
                }
            }
            entries.push(PartitionEntry {
                name,
                ptype,
                subtype,
                offset,
                size,
                flags,
            });
        }
        Ok(Self { entries })
    }

    /// Load a CSV partition table from disk.
    pub fn load_csv(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Self::from_csv(&text)
    }

    /// Validate the table: no duplicate names, no overlapping regions.
    pub fn validate(&self) -> Result<()> {
        for (i, a) in self.entries.iter().enumerate() {
            for b in &self.entries[i + 1..] {
                if a.name == b.name {
                    return Err(Error::Other(format!(
                        "duplicate partition name {:?}",
                        a.name
                    )));
                }
                let a_end = a.offset.saturating_add(a.size);
                let b_end = b.offset.saturating_add(b.size);
                if a.offset < b_end && b.offset < a_end {
                    return Err(Error::Other(format!(
                        "partition overlap: {:?} [{:#x}..{:#x}) and {:?} [{:#x}..{:#x})",
                        a.name, a.offset, a_end, b.name, b.offset, b_end
                    )));
                }
            }
        }
        Ok(())
    }
}

fn parse_type(s: &str) -> Result<u8> {
    Ok(match s.trim() {
        "app" => TYPE_APP,
        "data" => TYPE_DATA,
        "bootloader" => TYPE_BOOTLOADER,
        "partition_table" => TYPE_PARTITION_TABLE,
        other => parse_int_u8(other)
            .map_err(|e| Error::Other(format!("unknown partition type {:?}: {}", other, e)))?,
    })
}

fn parse_subtype(ptype: u8, s: &str) -> Result<u8> {
    let s = s.trim();
    Ok(match (ptype, s) {
        (TYPE_APP, "factory") => APP_FACTORY,
        (TYPE_APP, "test") => APP_TEST,
        (TYPE_APP, name) if name.starts_with("ota_") => {
            let n: u8 = name[4..]
                .parse()
                .map_err(|_| Error::Other(format!("bad ota slot {:?}", name)))?;
            if n > 15 {
                return Err(Error::Other(format!("ota slot {} > 15", n)));
            }
            APP_OTA_MIN + n
        }
        (TYPE_APP, name) if name.starts_with("tee_") => {
            let n: u8 = name[4..]
                .parse()
                .map_err(|_| Error::Other(format!("bad tee slot {:?}", name)))?;
            if n > 1 {
                return Err(Error::Other(format!("tee slot {} > 1", n)));
            }
            APP_TEE_MIN + n
        }
        (TYPE_DATA, "ota") => DATA_OTA,
        (TYPE_DATA, "phy") => DATA_PHY,
        (TYPE_DATA, "nvs") => DATA_NVS,
        (TYPE_DATA, "coredump") => DATA_COREDUMP,
        (TYPE_DATA, "nvs_keys") => DATA_NVS_KEYS,
        (TYPE_DATA, "efuse") => DATA_EFUSE,
        (TYPE_DATA, "undefined") => DATA_UNDEFINED,
        (TYPE_DATA, "esphttpd") => DATA_ESPHTTPD,
        (TYPE_DATA, "fat") => DATA_FAT,
        (TYPE_DATA, "spiffs") => DATA_SPIFFS,
        (TYPE_DATA, "littlefs") => DATA_LITTLEFS,
        (TYPE_DATA, "tee_ota") => DATA_TEE_OTA,
        (_, other) => parse_int_u8(other).map_err(|e| {
            Error::Other(format!(
                "unknown subtype {:?} for type {}: {}",
                other, ptype, e
            ))
        })?,
    })
}

fn parse_offset(s: &str) -> Result<u32> {
    parse_int_u32(s.trim()).map_err(|e| Error::Other(format!("bad offset {:?}: {}", s, e)))
}

fn parse_size(s: &str) -> Result<u32> {
    let s = s.trim();
    if s.is_empty() {
        return Err(Error::Other("size field can't be empty".into()));
    }
    let (num_str, mult): (&str, u32) = if let Some(stripped) = s.strip_suffix(['M', 'm']) {
        (stripped, 1024 * 1024)
    } else if let Some(stripped) = s.strip_suffix(['K', 'k']) {
        (stripped, 1024)
    } else {
        (s, 1)
    };
    let base =
        parse_int_u32(num_str).map_err(|e| Error::Other(format!("bad size {:?}: {}", s, e)))?;
    base.checked_mul(mult)
        .ok_or_else(|| Error::Other(format!("size overflow: {:?}", s)))
}

fn parse_int_u8(s: &str) -> std::result::Result<u8, String> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u8::from_str_radix(hex, 16).map_err(|e| e.to_string())
    } else {
        s.parse::<u8>().map_err(|e| e.to_string())
    }
}

fn parse_int_u32(s: &str) -> std::result::Result<u32, String> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).map_err(|e| e.to_string())
    } else {
        s.parse::<u32>().map_err(|e| e.to_string())
    }
}

/// App partitions need 64KB alignment, all others sector (4K) alignment.
fn align_offset(offset: u32, ptype: u8) -> u32 {
    let align = if ptype == TYPE_APP { 0x10000 } else { 0x1000 };
    offset.div_ceil(align) * align
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_suffixes() {
        assert_eq!(parse_size("0x1000").unwrap(), 0x1000);
        assert_eq!(parse_size("1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("256K").unwrap(), 256 * 1024);
        assert_eq!(parse_size("4096").unwrap(), 4096);
    }

    #[test]
    fn parse_full_csv() {
        let csv = "
# Name,   Type, SubType, Offset,  Size, Flags
nvs,      data, nvs,     0x9000,  0x6000,
phy_init, data, phy,     0xf000,  0x1000,
factory,  app,  factory, 0x10000, 1M,
ota_0,    app,  ota_0,   ,        1M,
ota_1,    app,  ota_1,   ,        1M,
";
        let table = PartitionTable::from_csv(csv).unwrap();
        assert_eq!(table.entries.len(), 5);
        assert_eq!(table.find("factory").unwrap().offset, 0x10000);
        // ota_0 auto-allocated right after factory @ 0x10000 + 1MB = 0x110000
        assert_eq!(table.find("ota_0").unwrap().offset, 0x110000);
        assert_eq!(table.find("ota_1").unwrap().offset, 0x210000);
        assert_eq!(table.find("nvs").unwrap().subtype, DATA_NVS);
        table.validate().unwrap();
    }

    #[test]
    fn round_trip_binary() {
        let entry = PartitionEntry {
            name: "app0".into(),
            ptype: TYPE_APP,
            subtype: APP_OTA_MIN,
            offset: 0x10000,
            size: 0x100000,
            flags: 0,
        };
        // Build a synthetic binary table with one entry + end marker.
        let mut buf = vec![0u8; ENTRY_SIZE];
        buf[0..2].copy_from_slice(&ENTRY_MAGIC);
        buf[2] = entry.ptype;
        buf[3] = entry.subtype;
        LittleEndian::write_u32(&mut buf[4..8], entry.offset);
        LittleEndian::write_u32(&mut buf[8..12], entry.size);
        let name = entry.name.as_bytes();
        buf[12..12 + name.len()].copy_from_slice(name);
        LittleEndian::write_u32(&mut buf[28..32], entry.flags);
        // end marker
        buf.extend_from_slice(&[0xFF; ENTRY_SIZE]);
        let table = PartitionTable::from_binary(&buf).unwrap();
        assert_eq!(table.entries[0], entry);
    }

    #[test]
    fn detects_overlap() {
        let csv = "
a, data, nvs, 0x9000, 0x4000,
b, data, nvs, 0xa000, 0x4000,
";
        let table = PartitionTable::from_csv(csv).unwrap();
        assert!(table.validate().is_err());
    }
}
