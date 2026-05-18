//! Offline image generation: `elf2image` and `merge_bin`.
//!
//! These commands don't touch the chip — they produce flash-ready binary
//! files from an ELF (via `elf2image`) or from a bag of already-built
//! `(address, file)` pairs (via `merge_bin`).
//!
//! ELF parsing is done inline (ELF32-LE is small and stable) to avoid pulling
//! in the `object` crate for one feature.  The ESP firmware image format is
//! documented in ESP-IDF's `components/bootloader_support/include/esp_app_format.h`
//! and in upstream esptool's `bin_image.py`.

use byteorder::{ByteOrder, LittleEndian};
use sha2::{Digest, Sha256};

use crate::chip::Chip;
use crate::error::{Error, Result};

/// First byte of every ESP application image header.
pub const ESP_IMAGE_MAGIC: u8 = 0xE9;
/// Initial XOR state for the trailing 1-byte segment checksum.
pub const CHECKSUM_MAGIC: u8 = 0xEF;
/// All-image final alignment (esptool & idf use 16); the checksum byte goes
/// at position `(length % 16) == 15`.
pub const IMAGE_ALIGN: usize = 16;

/// Parsed loadable segment of an ELF file.
#[derive(Debug, Clone)]
pub struct ElfSegment {
    pub load_addr: u32,
    pub data: Vec<u8>,
    pub flags: u32,
}

/// Image-header parameters chosen at build time.
#[derive(Debug, Clone, Copy)]
pub struct ImageParams {
    /// Encoded flash mode: 0=QIO, 1=QOUT, 2=DIO, 3=DOUT.
    pub flash_mode: u8,
    /// Encoded flash frequency (low nibble of byte 3).
    pub flash_freq: u8,
    /// Encoded flash size (high nibble of byte 3): 0=1MB, 1=2MB, 2=4MB,
    /// 3=8MB, 4=16MB, 5=32MB, 6=64MB, 7=128MB.
    pub flash_size: u8,
    /// Minimum required chip revision (legacy, in tenths).
    pub min_rev: u8,
    /// Minimum required chip revision, full precision (major*100 + minor).
    pub min_rev_full: u16,
    /// Maximum supported chip revision, full precision (default 0xFFFF).
    pub max_rev_full: u16,
    /// If true, append a SHA256 digest of the image after the checksum.
    pub hash_append: bool,
}

impl Default for ImageParams {
    fn default() -> Self {
        Self {
            flash_mode: 2, // DIO — safe default
            flash_freq: 0, // 40 MHz on most chips
            flash_size: 2, // 4 MB — sane default
            min_rev: 0,
            min_rev_full: 0,
            max_rev_full: 0xFFFF,
            hash_append: true,
        }
    }
}

// ---------------------------------------------------------------------------
// merge_bin
// ---------------------------------------------------------------------------

/// Merge a set of `(address, bytes)` pairs into a single binary, gaps padded
/// with 0xFF.  Output starts at `target_offset` (so a piece at address
/// `target_offset` lands at byte 0 of the output).  If `target_size` is
/// given, the result is padded up to that size and any piece extending
/// beyond it is rejected.
///
/// Used by the `merge-bin` CLI subcommand.
pub fn merge_bin(
    parts: &[(u32, Vec<u8>)],
    target_offset: u32,
    target_size: Option<u32>,
) -> Result<Vec<u8>> {
    if parts.is_empty() {
        return Err(Error::Other("merge_bin requires at least one part".into()));
    }
    for (addr, _) in parts {
        if *addr < target_offset {
            return Err(Error::Other(format!(
                "piece at {:#x} is below target_offset {:#x}",
                addr, target_offset
            )));
        }
    }
    // Highest end address among the pieces, expressed as offset into output.
    let max_end: u32 = parts
        .iter()
        .map(|(a, d)| a + d.len() as u32 - target_offset)
        .max()
        .unwrap_or(0);
    let total_len = match target_size {
        Some(ts) => {
            if max_end > ts {
                return Err(Error::Other(format!(
                    "pieces extend to offset {:#x} but target-size is only {:#x}",
                    max_end, ts
                )));
            }
            ts
        }
        None => max_end,
    };
    let mut out = vec![0xFFu8; total_len as usize];
    for (addr, data) in parts {
        let offset = (*addr - target_offset) as usize;
        // Detect overlap with any previously-placed piece. Easy heuristic:
        // none of the bytes we're about to write should be set already (i.e.
        // they should all still be 0xFF). False positives only happen if a
        // piece contains all-0xFF tails — unusual; we accept that.
        if out[offset..offset + data.len()].iter().any(|&b| b != 0xFF) {
            return Err(Error::Other(format!(
                "piece at {:#x} overlaps a previously placed piece",
                addr
            )));
        }
        out[offset..offset + data.len()].copy_from_slice(data);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// elf2image
// ---------------------------------------------------------------------------

/// Parse a 32-bit little-endian ELF file (Xtensa or RISC-V; same layout) and
/// return its PT_LOAD segments and entry address.
pub fn parse_elf32_le(bytes: &[u8]) -> Result<(u32, Vec<ElfSegment>)> {
    if bytes.len() < 52 {
        return Err(Error::Other("ELF too small for header".into()));
    }
    if &bytes[0..4] != b"\x7FELF" {
        return Err(Error::Other("not an ELF file (bad magic)".into()));
    }
    if bytes[4] != 1 {
        // 1 = ELFCLASS32, 2 = ELFCLASS64
        return Err(Error::Other(
            "only 32-bit ELF (Xtensa/RISC-V) supported".into(),
        ));
    }
    if bytes[5] != 1 {
        return Err(Error::Other("only little-endian ELF supported".into()));
    }
    let e_entry = LittleEndian::read_u32(&bytes[24..28]);
    let e_phoff = LittleEndian::read_u32(&bytes[28..32]) as usize;
    let e_phentsize = LittleEndian::read_u16(&bytes[42..44]) as usize;
    let e_phnum = LittleEndian::read_u16(&bytes[44..46]) as usize;
    if e_phentsize < 32 {
        return Err(Error::Other(format!(
            "ELF e_phentsize {} smaller than expected 32",
            e_phentsize
        )));
    }
    let mut segments = Vec::new();
    for i in 0..e_phnum {
        let off = e_phoff + i * e_phentsize;
        if off + 32 > bytes.len() {
            return Err(Error::Other("ELF program header out of bounds".into()));
        }
        let ph = &bytes[off..off + 32];
        let p_type = LittleEndian::read_u32(&ph[0..4]);
        let p_offset = LittleEndian::read_u32(&ph[4..8]) as usize;
        let p_paddr = LittleEndian::read_u32(&ph[12..16]);
        let p_filesz = LittleEndian::read_u32(&ph[16..20]) as usize;
        let p_flags = LittleEndian::read_u32(&ph[24..28]);
        if p_type != 1 {
            continue;
        } // not PT_LOAD
        if p_filesz == 0 {
            continue;
        } // BSS-only — no payload to write
        if p_offset + p_filesz > bytes.len() {
            return Err(Error::Other(format!(
                "PT_LOAD segment {} extends past EOF",
                i
            )));
        }
        let data = bytes[p_offset..p_offset + p_filesz].to_vec();
        segments.push(ElfSegment {
            load_addr: p_paddr,
            data,
            flags: p_flags,
        });
    }
    // Merge contiguous segments with identical flags. ESP-IDF linker scripts
    // frequently produce many small adjacent PT_LOAD entries that the
    // bootloader is happy to load as one.
    segments.sort_by_key(|s| s.load_addr);
    let mut merged: Vec<ElfSegment> = Vec::with_capacity(segments.len());
    for seg in segments {
        match merged.last_mut() {
            Some(prev)
                if prev.flags == seg.flags
                    && prev.load_addr + prev.data.len() as u32 == seg.load_addr =>
            {
                prev.data.extend_from_slice(&seg.data);
            }
            _ => merged.push(seg),
        }
    }
    Ok((e_entry, merged))
}

/// Build an ESP firmware image for `chip` from `(entry, segments)`.
///
/// Output layout (matches upstream `esptool`'s v2 image):
///   - 24-byte header (magic 0xE9, seg_count, mode, size|freq, entry,
///     wp_pin, drvs, chip_id, min_rev, min_rev_full, max_rev_full,
///     reserved×4, hash_appended)
///   - For each segment: 8-byte segment header + segment data
///   - Padding so total length is a multiple of 16 minus 1
///   - 1-byte XOR checksum (state init = 0xEF)
///   - If `params.hash_append`: 32-byte SHA256 of all preceding bytes
pub fn build_image(
    entry_addr: u32,
    segments: &[ElfSegment],
    params: &ImageParams,
    chip: &Chip,
) -> Result<Vec<u8>> {
    if segments.is_empty() {
        return Err(Error::Other("no loadable segments in ELF".into()));
    }
    let mut out = Vec::with_capacity(0x10000);
    out.push(ESP_IMAGE_MAGIC);
    out.push(segments.len() as u8);
    out.push(params.flash_mode);
    out.push(((params.flash_size & 0x0F) << 4) | (params.flash_freq & 0x0F));
    let mut buf = [0u8; 4];
    LittleEndian::write_u32(&mut buf, entry_addr);
    out.extend_from_slice(&buf);
    // Extended ESP32+ header (16 more bytes).
    out.push(0xEE); // wp_pin (no SPI WP override)
    out.extend_from_slice(&[0, 0, 0]); // clk_q_drv, d_cs_drv, gd_wp_drv
    let mut chip_id = [0u8; 2];
    LittleEndian::write_u16(&mut chip_id, chip.image_chip_id as u16);
    out.extend_from_slice(&chip_id);
    out.push(params.min_rev);
    let mut rev_buf = [0u8; 2];
    LittleEndian::write_u16(&mut rev_buf, params.min_rev_full);
    out.extend_from_slice(&rev_buf);
    LittleEndian::write_u16(&mut rev_buf, params.max_rev_full);
    out.extend_from_slice(&rev_buf);
    out.extend_from_slice(&[0u8; 4]); // reserved
    out.push(if params.hash_append { 1 } else { 0 });
    debug_assert_eq!(out.len(), 24);

    // Per-segment XOR checksum, init = CHECKSUM_MAGIC.
    let mut chk = CHECKSUM_MAGIC;
    for seg in segments {
        let mut hdr = [0u8; 8];
        LittleEndian::write_u32(&mut hdr[0..4], seg.load_addr);
        LittleEndian::write_u32(&mut hdr[4..8], seg.data.len() as u32);
        out.extend_from_slice(&hdr);
        out.extend_from_slice(&seg.data);
        for &b in &seg.data {
            chk ^= b;
        }
    }
    // Pad with zero bytes so the checksum byte lands at the last byte of a
    // 16-byte block (i.e. `out.len() + 1` is a multiple of 16).
    let pad = (IMAGE_ALIGN - (out.len() + 1) % IMAGE_ALIGN) % IMAGE_ALIGN;
    out.extend(std::iter::repeat_n(0u8, pad));
    out.push(chk);

    if params.hash_append {
        let mut hasher = Sha256::new();
        hasher.update(&out);
        let digest = hasher.finalize();
        out.extend_from_slice(&digest);
    }
    Ok(out)
}

/// One-shot helper: parse ELF, build image.
pub fn elf_to_image(elf_bytes: &[u8], params: &ImageParams, chip: &Chip) -> Result<Vec<u8>> {
    let (entry, segs) = parse_elf32_le(elf_bytes)?;
    build_image(entry, &segs, params, chip)
}

// ---------------------------------------------------------------------------
// Param encoding helpers (string → byte)
// ---------------------------------------------------------------------------

pub fn encode_flash_mode(s: &str) -> Result<u8> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "qio" => 0,
        "qout" => 1,
        "dio" => 2,
        "dout" => 3,
        other => {
            return Err(Error::Other(format!(
                "unknown flash mode {:?} (expected qio/qout/dio/dout)",
                other
            )))
        }
    })
}

pub fn encode_flash_size(s: &str) -> Result<u8> {
    Ok(match s.to_ascii_uppercase().as_str() {
        "1MB" => 0,
        "2MB" => 1,
        "4MB" => 2,
        "8MB" => 3,
        "16MB" => 4,
        "32MB" => 5,
        "64MB" => 6,
        "128MB" => 7,
        other => {
            return Err(Error::Other(format!(
                "unknown flash size {:?} (expected 1MB..128MB)",
                other
            )))
        }
    })
}

/// Flash frequency encoding differs per chip family.  ESP32 uses
/// {40m=0, 26m=1, 20m=2, 80m=0xF}; newer chips (S3/C3/H2/...) use a
/// different (overlapping) set of low-nibble codes.  We accept the
/// canonical string spellings and emit the value used by the chip's
/// bootloader header byte.
pub fn encode_flash_freq(chip: &Chip, s: &str) -> Result<u8> {
    let key = s.to_ascii_lowercase();
    Ok(match (chip.name, key.as_str()) {
        ("ESP32", "40m") | ("ESP32", "40mhz") => 0x0,
        ("ESP32", "26m") | ("ESP32", "26mhz") => 0x1,
        ("ESP32", "20m") | ("ESP32", "20mhz") => 0x2,
        ("ESP32", "80m") | ("ESP32", "80mhz") => 0xF,

        // ESP32-S3 / ESP32-C3 / ESP32-C6 / ESP32-P4 family share the same
        // set of canonical strings; the bootloader treats unknown values as
        // "use defaults", but these are the documented codes.
        (_, "80m") | (_, "80mhz") => 0xF,
        (_, "40m") | (_, "40mhz") => 0x0,
        (_, "26m") | (_, "26mhz") => 0x1,
        (_, "20m") | (_, "20mhz") => 0x2,

        // ESP32-H2 has different codes (its XTAL is 32MHz, not 40MHz).
        ("ESP32-H2", "48m") | ("ESP32-H2", "48mhz") => 0xF,
        ("ESP32-H2", "24m") | ("ESP32-H2", "24mhz") => 0x0,
        ("ESP32-H2", "16m") | ("ESP32-H2", "16mhz") => 0x1,
        ("ESP32-H2", "12m") | ("ESP32-H2", "12mhz") => 0x2,

        (chip_name, other) => {
            return Err(Error::Other(format!(
                "unknown flash freq {:?} for {}",
                other, chip_name
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chip;

    #[test]
    fn merge_bin_padding() {
        let parts = vec![(0x0, vec![0x11, 0x22]), (0x10, vec![0x33])];
        let out = merge_bin(&parts, 0, None).unwrap();
        assert_eq!(out.len(), 0x11);
        assert_eq!(&out[..2], &[0x11, 0x22]);
        assert!(out[2..0x10].iter().all(|&b| b == 0xFF));
        assert_eq!(out[0x10], 0x33);
    }

    #[test]
    fn merge_bin_target_size_pads() {
        let parts = vec![(0x0, vec![0xAA, 0xBB])];
        let out = merge_bin(&parts, 0, Some(8)).unwrap();
        assert_eq!(out.len(), 8);
        assert_eq!(&out[..2], &[0xAA, 0xBB]);
        assert!(out[2..].iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn merge_bin_detects_overlap() {
        let parts = vec![(0x0, vec![0x11, 0x22]), (0x1, vec![0x33])];
        assert!(merge_bin(&parts, 0, None).is_err());
    }

    #[test]
    fn merge_bin_rejects_below_offset() {
        let parts = vec![(0x5, vec![0x11])];
        assert!(merge_bin(&parts, 0x10, None).is_err());
    }

    #[test]
    fn build_image_header_and_checksum() {
        let chip = chip::by_name("esp32-s3").unwrap();
        let segs = vec![ElfSegment {
            load_addr: 0x40380000,
            data: vec![0x01, 0x02, 0x03, 0x04],
            flags: 5,
        }];
        let img = build_image(0x40380000, &segs, &ImageParams::default(), chip).unwrap();
        assert_eq!(img[0], ESP_IMAGE_MAGIC);
        assert_eq!(img[1], 1); // segment count
                               // Layout: 24-byte header + 8-byte seg header + 4-byte data + pad +
                               // 1-byte checksum + 32-byte SHA256.  Header+seghdr+data = 36; needs
                               // 11 pad bytes to reach 47, +1 checksum = 48 (multiple of 16). Plus
                               // SHA256 = 80 bytes total.
        assert_eq!(img.len(), 80);
        // Checksum byte = init ^ data[0] ^ ... = 0xEF^0x01^0x02^0x03^0x04
        let expected_chk = CHECKSUM_MAGIC ^ 0x01 ^ 0x02 ^ 0x03 ^ 0x04;
        assert_eq!(img[47], expected_chk);
    }

    #[test]
    fn parse_minimal_elf32_le() {
        // Build a tiny ELF32-LE with one PT_LOAD segment.
        let mut bytes = vec![0u8; 200];
        bytes[0..4].copy_from_slice(b"\x7FELF");
        bytes[4] = 1; // ELFCLASS32
        bytes[5] = 1; // little-endian
        bytes[6] = 1; // ELF version
                      // e_entry @ 24..28
        LittleEndian::write_u32(&mut bytes[24..28], 0x40380000);
        // e_phoff = 52 (right after ELF header)
        LittleEndian::write_u32(&mut bytes[28..32], 52);
        // e_phentsize = 32, e_phnum = 1
        LittleEndian::write_u16(&mut bytes[42..44], 32);
        LittleEndian::write_u16(&mut bytes[44..46], 1);
        // One program header @ offset 52
        let ph_off = 52;
        // p_type = PT_LOAD = 1
        LittleEndian::write_u32(&mut bytes[ph_off..ph_off + 4], 1);
        // p_offset = 100 (where segment data lives)
        LittleEndian::write_u32(&mut bytes[ph_off + 4..ph_off + 8], 100);
        // p_vaddr/p_paddr = load addr
        LittleEndian::write_u32(&mut bytes[ph_off + 8..ph_off + 12], 0x40380000);
        LittleEndian::write_u32(&mut bytes[ph_off + 12..ph_off + 16], 0x40380000);
        // p_filesz = 4
        LittleEndian::write_u32(&mut bytes[ph_off + 16..ph_off + 20], 4);
        // Segment data at offset 100: 0xAA 0xBB 0xCC 0xDD
        bytes[100..104].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);

        let (entry, segs) = parse_elf32_le(&bytes).unwrap();
        assert_eq!(entry, 0x40380000);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].load_addr, 0x40380000);
        assert_eq!(segs[0].data, vec![0xAA, 0xBB, 0xCC, 0xDD]);
    }
}
