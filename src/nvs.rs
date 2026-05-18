//! ESP-IDF NVS (Non-Volatile Storage) partition reader.
//!
//! NVS is Espressif's key-value store backed by a section of flash.  Each
//! partition is a sequence of 4 KB pages; each page contains a header, an
//! entry-state bitmap, and 126 fixed-size 32-byte entries.  Variable-length
//! values (strings, blobs) span multiple entries.
//!
//! This module decodes the on-flash bytes into a `NvsPartition` of
//! `NvsItem`s.  Writing back is intentionally not implemented here — see
//! `docs/STATUS.md` for the rationale.
//!
//! Format reference: ESP-IDF v5.x `components/nvs_flash/src/`.

use std::collections::HashMap;

use byteorder::{ByteOrder, LittleEndian};
use serde::Serialize;

use crate::error::{Error, Result};

pub const PAGE_SIZE: usize = 4096;
pub const ENTRY_SIZE: usize = 32;
pub const ENTRIES_PER_PAGE: usize = 126;
pub const PAGE_HEADER_SIZE: usize = 32;
pub const PAGE_BITMAP_SIZE: usize = 32;

/// Page-version byte at offset 8 of the page header. v2 is the modern
/// format used by all ESP-IDF >= 4.0 builds.
pub const PAGE_VERSION_V2: u8 = 0xFE;
pub const PAGE_VERSION_V1: u8 = 0xFF;

/// Entry state, encoded as 2 bits in the page bitmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryState {
    Empty,   // 0b11 — never used
    Written, // 0b10 — current
    Erased,  // 0b00 — superseded
    Invalid, // 0b01 — partial-write / corruption
}

impl EntryState {
    fn decode(bits: u8) -> Self {
        match bits & 0b11 {
            0b11 => EntryState::Empty,
            0b10 => EntryState::Written,
            0b00 => EntryState::Erased,
            _ => EntryState::Invalid,
        }
    }
}

/// NVS type byte. Values match `nvs::ItemType` in ESP-IDF.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NvsType {
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    U64,
    I64,
    Str,
    Blob,     // legacy single-entry blob
    BlobData, // chunk of a multi-entry blob
    BlobIdx,  // index entry for multi-chunk blob
    Unknown(u8),
}

impl NvsType {
    fn from_byte(b: u8) -> Self {
        match b {
            0x01 => NvsType::U8,
            0x11 => NvsType::I8,
            0x02 => NvsType::U16,
            0x12 => NvsType::I16,
            0x04 => NvsType::U32,
            0x14 => NvsType::I32,
            0x08 => NvsType::U64,
            0x18 => NvsType::I64,
            0x21 => NvsType::Str,
            0x41 => NvsType::Blob,
            0x42 => NvsType::BlobData,
            0x48 => NvsType::BlobIdx,
            other => NvsType::Unknown(other),
        }
    }

    pub fn name(&self) -> String {
        match self {
            NvsType::U8 => "u8".into(),
            NvsType::I8 => "i8".into(),
            NvsType::U16 => "u16".into(),
            NvsType::I16 => "i16".into(),
            NvsType::U32 => "u32".into(),
            NvsType::I32 => "i32".into(),
            NvsType::U64 => "u64".into(),
            NvsType::I64 => "i64".into(),
            NvsType::Str => "string".into(),
            NvsType::Blob => "blob".into(),
            NvsType::BlobData => "blob_data".into(),
            NvsType::BlobIdx => "blob_idx".into(),
            NvsType::Unknown(b) => format!("0x{:02x}", b),
        }
    }

    fn is_variable_length(&self) -> bool {
        matches!(
            self,
            NvsType::Str | NvsType::Blob | NvsType::BlobData | NvsType::BlobIdx
        )
    }
}

/// A decoded NVS value.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum NvsValue {
    U8(u8),
    I8(i8),
    U16(u16),
    I16(i16),
    U32(u32),
    I32(i32),
    U64(u64),
    I64(i64),
    String(String),
    /// Variable-length binary value. Stored as base64 in JSON for safety.
    Blob {
        #[serde(serialize_with = "serialize_bytes_b64")]
        bytes: Vec<u8>,
    },
    /// A type the parser doesn't yet understand. Raw 8-byte payload.
    Raw {
        #[serde(serialize_with = "serialize_bytes_b64")]
        bytes: Vec<u8>,
    },
}

fn serialize_bytes_b64<S: serde::Serializer>(
    b: &[u8],
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    use base64::Engine;
    s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(b))
}

impl NvsValue {
    /// Render the value for table / CLI display.  Long blobs are truncated.
    pub fn display(&self) -> String {
        match self {
            NvsValue::U8(v) => v.to_string(),
            NvsValue::I8(v) => v.to_string(),
            NvsValue::U16(v) => v.to_string(),
            NvsValue::I16(v) => v.to_string(),
            NvsValue::U32(v) => v.to_string(),
            NvsValue::I32(v) => v.to_string(),
            NvsValue::U64(v) => v.to_string(),
            NvsValue::I64(v) => v.to_string(),
            NvsValue::String(s) => format!("{:?}", s),
            NvsValue::Blob { bytes } => {
                let preview = bytes
                    .iter()
                    .take(16)
                    .map(|b| format!("{:02x}", b))
                    .collect::<Vec<_>>()
                    .join(" ");
                if bytes.len() > 16 {
                    format!("[{}B] {}...", bytes.len(), preview)
                } else {
                    format!("[{}B] {}", bytes.len(), preview)
                }
            }
            NvsValue::Raw { bytes } => bytes
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

/// One decoded entry in the partition.
#[derive(Debug, Clone, Serialize)]
pub struct NvsItem {
    pub namespace: String,
    pub key: String,
    pub ty: NvsType,
    pub value: NvsValue,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct NvsPartition {
    pub version: u8,
    pub items: Vec<NvsItem>,
}

impl NvsPartition {
    /// Group items by namespace for table-friendly rendering.
    pub fn by_namespace(&self) -> Vec<(String, Vec<&NvsItem>)> {
        let mut groups: HashMap<String, Vec<&NvsItem>> = HashMap::new();
        for it in &self.items {
            groups.entry(it.namespace.clone()).or_default().push(it);
        }
        let mut out: Vec<_> = groups.into_iter().collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, items) in out.iter_mut() {
            items.sort_by(|a, b| a.key.cmp(&b.key));
        }
        out
    }
}

/// Parse a full NVS partition.  `bytes.len()` must be a multiple of `PAGE_SIZE`.
pub fn parse(bytes: &[u8]) -> Result<NvsPartition> {
    if !bytes.len().is_multiple_of(PAGE_SIZE) {
        return Err(Error::Other(format!(
            "NVS partition not a multiple of {}B (got {}B)",
            PAGE_SIZE,
            bytes.len()
        )));
    }

    // First pass: walk every page and collect raw entries (still tagged
    // with namespace_index, not yet resolved to a namespace name).
    // Variable-length values are stitched together from `span` consecutive
    // entries within the page.
    #[derive(Debug)]
    struct RawEntry {
        ns_index: u8,
        ty: NvsType,
        /// Chunk index for blob_data entries (so the coalescer can sort
        /// chunks before concatenating). 0xFF for non-chunk types.
        chunk_index: u8,
        key: String,
        value: NvsValue,
    }
    let mut raws: Vec<RawEntry> = Vec::new();
    let mut version_seen: u8 = 0;

    for page_off in (0..bytes.len()).step_by(PAGE_SIZE) {
        let page = &bytes[page_off..page_off + PAGE_SIZE];
        let header = &page[..PAGE_HEADER_SIZE];
        // 0xFF page bytes mean "uninitialised" — skip without complaint.
        if header.iter().all(|&b| b == 0xFF) {
            continue;
        }
        let version = header[8];
        if version == PAGE_VERSION_V1 || version == PAGE_VERSION_V2 {
            version_seen = version;
        }

        let bitmap = &page[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + PAGE_BITMAP_SIZE];
        let entries_off = PAGE_HEADER_SIZE + PAGE_BITMAP_SIZE;
        let entries = &page[entries_off..];

        let mut i = 0;
        while i < ENTRIES_PER_PAGE {
            let state = entry_state(bitmap, i);
            if state != EntryState::Written {
                i += 1;
                continue;
            }
            let entry_off = i * ENTRY_SIZE;
            if entry_off + ENTRY_SIZE > entries.len() {
                break;
            }
            let entry = &entries[entry_off..entry_off + ENTRY_SIZE];
            let ns_index = entry[0];
            if ns_index == 0xFF {
                i += 1;
                continue;
            }
            let ty = NvsType::from_byte(entry[1]);
            let span = entry[2] as usize;
            let chunk_index = entry[3];
            let key = read_key(&entry[8..24]);

            if ty.is_variable_length() {
                let size = LittleEndian::read_u16(&entry[24..26]) as usize;
                // Data bytes occupy `span - 1` follow-on entries.
                let data_entries = span.saturating_sub(1);
                let data_start = entry_off + ENTRY_SIZE;
                let data_end = data_start + data_entries * ENTRY_SIZE;
                if data_end > entries.len() {
                    // Truncated; skip this item and move on rather than
                    // bailing the whole parse.
                    i += 1;
                    continue;
                }
                let mut data = entries[data_start..data_end].to_vec();
                data.truncate(size);
                let value = match ty {
                    NvsType::Str => {
                        // ESP-IDF stores strings with a trailing null byte
                        // included in `size`; strip it for the decoded value.
                        let s = String::from_utf8_lossy(&data)
                            .trim_end_matches('\0')
                            .to_string();
                        NvsValue::String(s)
                    }
                    NvsType::Blob | NvsType::BlobData => NvsValue::Blob { bytes: data },
                    NvsType::BlobIdx => {
                        // Index entry; the data here is metadata about chunks.
                        // We surface it as a Blob for now — the BlobData chunks
                        // are read as their own entries.
                        NvsValue::Blob { bytes: data }
                    }
                    _ => NvsValue::Raw { bytes: data },
                };
                raws.push(RawEntry {
                    ns_index,
                    ty,
                    chunk_index,
                    key,
                    value,
                });
                i += span.max(1);
            } else {
                let payload = &entry[24..32];
                let value = match ty {
                    NvsType::U8 => NvsValue::U8(payload[0]),
                    NvsType::I8 => NvsValue::I8(payload[0] as i8),
                    NvsType::U16 => NvsValue::U16(LittleEndian::read_u16(&payload[..2])),
                    NvsType::I16 => NvsValue::I16(LittleEndian::read_i16(&payload[..2])),
                    NvsType::U32 => NvsValue::U32(LittleEndian::read_u32(&payload[..4])),
                    NvsType::I32 => NvsValue::I32(LittleEndian::read_i32(&payload[..4])),
                    NvsType::U64 => NvsValue::U64(LittleEndian::read_u64(payload)),
                    NvsType::I64 => NvsValue::I64(LittleEndian::read_i64(payload)),
                    _ => NvsValue::Raw {
                        bytes: payload.to_vec(),
                    },
                };
                raws.push(RawEntry {
                    ns_index,
                    ty,
                    chunk_index,
                    key,
                    value,
                });
                i += 1;
            }
        }
    }

    // Second pass: coalesce multi-chunk blobs. An NVS v2 blob is stored
    // as (a) one `blob_idx` entry whose value is a header describing the
    // total size and number of chunks, plus (b) one or more `blob_data`
    // entries with matching (ns_index, key) and distinct `chunk_index`.
    //
    // The user wants to see one logical blob per (ns_index, key), not
    // the index entry alongside its data chunks. Merge them here: drop
    // the blob_idx entries, gather the blob_data chunks sorted by
    // chunk_index, concatenate, and emit a single legacy-style Blob.
    use std::collections::BTreeMap;
    type BlobKey = (u8, String);
    type BlobChunks = Vec<(u8, Vec<u8>)>;
    let mut blob_indexed: std::collections::HashSet<BlobKey> = std::collections::HashSet::new();
    let mut blob_chunks: BTreeMap<BlobKey, BlobChunks> = BTreeMap::new();
    for r in &raws {
        match r.ty {
            NvsType::BlobIdx => {
                blob_indexed.insert((r.ns_index, r.key.clone()));
            }
            NvsType::BlobData => {
                if let NvsValue::Blob { bytes } = &r.value {
                    blob_chunks
                        .entry((r.ns_index, r.key.clone()))
                        .or_default()
                        .push((r.chunk_index, bytes.clone()));
                }
            }
            _ => {}
        }
    }
    let mut coalesced: Vec<RawEntry> = Vec::new();
    for ((ns, key), mut parts) in blob_chunks {
        if !blob_indexed.contains(&(ns, key.clone())) {
            // Orphaned chunks (no matching index). Probably mid-rotation
            // garbage — drop them rather than show stale data.
            continue;
        }
        parts.sort_by_key(|(c, _)| *c);
        let mut data = Vec::with_capacity(parts.iter().map(|(_, b)| b.len()).sum());
        for (_, b) in parts {
            data.extend(b);
        }
        coalesced.push(RawEntry {
            ns_index: ns,
            ty: NvsType::Blob,
            chunk_index: 0xFF,
            key,
            value: NvsValue::Blob { bytes: data },
        });
    }
    // Drop the entries we just folded into coalesced blobs.
    let raws: Vec<RawEntry> = raws
        .into_iter()
        .filter(|r| {
            let owns = blob_indexed.contains(&(r.ns_index, r.key.clone()));
            !owns || !matches!(r.ty, NvsType::BlobIdx | NvsType::BlobData)
        })
        .chain(coalesced)
        .collect();

    // Third pass: resolve namespace indices to names. Items in the special
    // namespace 0 are the registry itself — key=name, value=index.
    let mut ns_name: HashMap<u8, String> = HashMap::new();
    ns_name.insert(0, "<registry>".into());
    for r in &raws {
        if r.ns_index == 0 {
            if let NvsValue::U8(idx) = &r.value {
                ns_name.insert(*idx, r.key.clone());
            }
        }
    }

    let mut items: Vec<NvsItem> = raws
        .into_iter()
        .filter(|r| r.ns_index != 0) // drop registry entries from the user view
        .map(|r| NvsItem {
            namespace: ns_name
                .get(&r.ns_index)
                .cloned()
                .unwrap_or_else(|| format!("<ns{}>", r.ns_index)),
            key: r.key,
            ty: r.ty,
            value: r.value,
        })
        .collect();
    items.sort_by(|a, b| {
        a.namespace
            .cmp(&b.namespace)
            .then_with(|| a.key.cmp(&b.key))
    });

    Ok(NvsPartition {
        version: version_seen,
        items,
    })
}

fn entry_state(bitmap: &[u8], index: usize) -> EntryState {
    let byte = bitmap[index / 4];
    let shift = (index % 4) * 2;
    EntryState::decode(byte >> shift)
}

fn read_key(bytes: &[u8]) -> String {
    // Keys are ASCII, max 15 chars + null. Trim at the first null byte.
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthesize a one-page NVS partition with a u32 entry in a namespace
    /// called "storage", round-trip it through the parser.
    #[test]
    fn parses_simple_u32_entry() {
        let mut page = vec![0xFFu8; PAGE_SIZE];
        // Header: state UNINITIALIZED is fine for the parser, but set
        // version=v2 so the parsed `version` reflects it.
        page[8] = PAGE_VERSION_V2;
        // Bitmap: mark entries 0 and 1 as WRITTEN (0b10), rest empty (0b11).
        // Entry 0: namespace registry (ns_index=0, type=u8, key="storage", value=1)
        // Entry 1: ns_index=1, type=u32, key="counter", value=42
        // 0b10 = 0x2 in the low two bits of the first state byte.
        // State for entries 0 and 1: shift 0 and shift 2 → byte = 0b11_11_10_10 = 0xFA.
        let bitmap_off = PAGE_HEADER_SIZE;
        page[bitmap_off] = 0b11_11_10_10;
        // Rest of bitmap stays 0xFF.

        let entry0_off = PAGE_HEADER_SIZE + PAGE_BITMAP_SIZE;
        // Registry entry: ns=0, type=U8, span=1, chunk=0xFF, crc=ignored,
        // key="storage", payload[0]=1.
        page[entry0_off] = 0;
        page[entry0_off + 1] = 0x01;
        page[entry0_off + 2] = 1;
        page[entry0_off + 3] = 0xFF;
        // crc32 (4 bytes) ignored by parser
        let key0 = b"storage";
        page[entry0_off + 8..entry0_off + 8 + key0.len()].copy_from_slice(key0);
        // null-pad rest of key
        for b in &mut page[entry0_off + 8 + key0.len()..entry0_off + 24] {
            *b = 0;
        }
        page[entry0_off + 24] = 1; // namespace index assignment

        // App entry: ns=1, type=U32, span=1, key="counter", value=42.
        let entry1_off = entry0_off + ENTRY_SIZE;
        page[entry1_off] = 1;
        page[entry1_off + 1] = 0x04;
        page[entry1_off + 2] = 1;
        page[entry1_off + 3] = 0xFF;
        let key1 = b"counter";
        page[entry1_off + 8..entry1_off + 8 + key1.len()].copy_from_slice(key1);
        for b in &mut page[entry1_off + 8 + key1.len()..entry1_off + 24] {
            *b = 0;
        }
        LittleEndian::write_u32(&mut page[entry1_off + 24..entry1_off + 28], 42);

        let parsed = parse(&page).unwrap();
        assert_eq!(parsed.version, PAGE_VERSION_V2);
        assert_eq!(parsed.items.len(), 1);
        let it = &parsed.items[0];
        assert_eq!(it.namespace, "storage");
        assert_eq!(it.key, "counter");
        assert!(matches!(it.value, NvsValue::U32(42)));
    }

    #[test]
    fn parses_string_entry_spanning_two_entries() {
        let mut page = vec![0xFFu8; PAGE_SIZE];
        page[8] = PAGE_VERSION_V2;
        // Three entries WRITTEN: registry + string header + string data.
        let bitmap_off = PAGE_HEADER_SIZE;
        // bits for entries 0, 1, 2 = WRITTEN → 0b11_10_10_10 = 0xEA.
        page[bitmap_off] = 0b11_10_10_10;

        let e0 = PAGE_HEADER_SIZE + PAGE_BITMAP_SIZE;
        // Registry: ns=0, type=U8, key="ns", value=1.
        // Real-world NVS null-pads the 16-byte key field; we mirror that
        // in the synthetic page so the parser sees a proper terminator.
        page[e0] = 0;
        page[e0 + 1] = 0x01;
        page[e0 + 2] = 1;
        page[e0 + 3] = 0xFF;
        for b in &mut page[e0 + 8..e0 + 24] {
            *b = 0;
        }
        page[e0 + 8..e0 + 10].copy_from_slice(b"ns");
        page[e0 + 24] = 1;

        let e1 = e0 + ENTRY_SIZE;
        // String entry: ns=1, type=STR, span=2 (header + 1 data entry)
        page[e1] = 1;
        page[e1 + 1] = 0x21;
        page[e1 + 2] = 2;
        page[e1 + 3] = 0xFF;
        for b in &mut page[e1 + 8..e1 + 24] {
            *b = 0;
        }
        let key = b"hello";
        page[e1 + 8..e1 + 8 + key.len()].copy_from_slice(key);
        // size = 6 (includes trailing null)
        LittleEndian::write_u16(&mut page[e1 + 24..e1 + 26], 6);

        let e2 = e1 + ENTRY_SIZE;
        // 6 bytes of "world\0" in the next entry's first 6 bytes.
        page[e2..e2 + 6].copy_from_slice(b"world\0");

        let parsed = parse(&page).unwrap();
        assert_eq!(parsed.items.len(), 1);
        let it = &parsed.items[0];
        assert_eq!(it.namespace, "ns");
        assert_eq!(it.key, "hello");
        assert!(matches!(&it.value, NvsValue::String(s) if s == "world"));
    }

    #[test]
    fn rejects_misaligned_partition() {
        let buf = vec![0xFFu8; PAGE_SIZE + 100];
        assert!(parse(&buf).is_err());
    }

    /// A two-chunk blob in NVS v2: one blob_idx + two blob_data entries
    /// with chunk_index 0 and 1. Parser should coalesce them into one
    /// row whose value is the concatenated bytes of both chunks.
    #[test]
    fn coalesces_blob_idx_and_data_chunks() {
        let mut page = vec![0xFFu8; PAGE_SIZE];
        page[8] = PAGE_VERSION_V2;
        // 5 entries WRITTEN: registry, blob_idx, blob_data#0, blob_data#1
        // (each blob_data uses span=2 so it consumes itself + 1 follow-on
        // entry for its bytes). We'll write entries 0..6:
        //   0  registry (storage → ns 1)
        //   1  blob_idx (ns=1, key="cfg", chunk_count=2, size=8)
        //   2  blob_data (ns=1, key="cfg", chunk_index=0, span=2, size=4)
        //   3      data for chunk 0 (4 bytes used, padded)
        //   4  blob_data (ns=1, key="cfg", chunk_index=1, span=2, size=4)
        //   5      data for chunk 1
        // Entry-state bitmap: 6 WRITTEN entries; rest empty.
        // Bits for slots 0..3 = WRITTEN (0b10) packed as 0b10_10_10_10 = 0xAA
        // Bits for slots 4..5 = WRITTEN (0b10), 6..7 = EMPTY (0b11) → 0xFA
        let bm = PAGE_HEADER_SIZE;
        page[bm] = 0b10_10_10_10;
        page[bm + 1] = 0b11_11_10_10;

        let entries_off = PAGE_HEADER_SIZE + PAGE_BITMAP_SIZE;
        let entry = |i: usize| entries_off + i * ENTRY_SIZE;

        // Entry 0 — namespace registry mapping "storage" → 1.
        page[entry(0)] = 0;
        page[entry(0) + 1] = 0x01;
        page[entry(0) + 2] = 1;
        page[entry(0) + 3] = 0xFF;
        for b in &mut page[entry(0) + 8..entry(0) + 24] {
            *b = 0;
        }
        page[entry(0) + 8..entry(0) + 15].copy_from_slice(b"storage");
        page[entry(0) + 24] = 1;

        // Entry 1 — blob_idx. ns=1, type=0x48, span=1, chunk_index=0xFF.
        page[entry(1)] = 1;
        page[entry(1) + 1] = 0x48;
        page[entry(1) + 2] = 1;
        page[entry(1) + 3] = 0xFF;
        for b in &mut page[entry(1) + 8..entry(1) + 24] {
            *b = 0;
        }
        page[entry(1) + 8..entry(1) + 11].copy_from_slice(b"cfg");
        // The 8 metadata bytes (dataSize u32, chunk_count u8, chunk_start u8, reserved u16).
        // We don't read them yet — parser uses `size` from bytes 24..26
        // for the (currently unused) NvsValue::Blob it produces.

        // Entry 2 — blob_data chunk 0. ns=1, type=0x42, span=2,
        // chunk_index=0, size=4 (4 data bytes in entry 3).
        page[entry(2)] = 1;
        page[entry(2) + 1] = 0x42;
        page[entry(2) + 2] = 2;
        page[entry(2) + 3] = 0; // chunk_index 0
        for b in &mut page[entry(2) + 8..entry(2) + 24] {
            *b = 0;
        }
        page[entry(2) + 8..entry(2) + 11].copy_from_slice(b"cfg");
        LittleEndian::write_u16(&mut page[entry(2) + 24..entry(2) + 26], 4);
        // Entry 3 — chunk 0 data: 0xDE 0xAD 0xBE 0xEF
        page[entry(3)..entry(3) + 4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);

        // Entry 4 — blob_data chunk 1, same key, chunk_index=1.
        page[entry(4)] = 1;
        page[entry(4) + 1] = 0x42;
        page[entry(4) + 2] = 2;
        page[entry(4) + 3] = 1; // chunk_index 1
        for b in &mut page[entry(4) + 8..entry(4) + 24] {
            *b = 0;
        }
        page[entry(4) + 8..entry(4) + 11].copy_from_slice(b"cfg");
        LittleEndian::write_u16(&mut page[entry(4) + 24..entry(4) + 26], 4);
        page[entry(5)..entry(5) + 4].copy_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);

        let parsed = parse(&page).unwrap();
        // After coalescing: exactly one user-visible entry for `cfg`.
        assert_eq!(parsed.items.len(), 1, "expected one coalesced blob entry");
        let it = &parsed.items[0];
        assert_eq!(it.namespace, "storage");
        assert_eq!(it.key, "cfg");
        match &it.value {
            NvsValue::Blob { bytes } => {
                assert_eq!(
                    bytes,
                    &vec![0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE],
                    "chunks should be concatenated in chunk_index order"
                );
            }
            other => panic!("expected Blob, got {:?}", other),
        }
    }
}
