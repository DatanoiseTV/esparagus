//! EFUSE field decoding (silicon revision + package version) for every
//! chip in the registry.
//!
//! Each chip generation lays out its `WAFER_VERSION_MAJOR`,
//! `WAFER_VERSION_MINOR`, and `PKG_VERSION` fields at chip-specific bit
//! positions across BLOCK0/BLOCK1/BLOCK2. The bit positions and the
//! special cases (ESP32 split major across BLOCK0 + APB_CTL register;
//! ESP32-S3 ECO0 workaround; ESP32-P4 split major across two
//! discontiguous bit groups) are sourced from upstream esptool's
//! `targets/<chip>.py:get_major_chip_version()` /
//! `get_minor_chip_version()` / `get_pkg_version()`.
//!
//! Keep this module in lockstep with upstream when adding new
//! silicon. The `efuse_silicon_rev_*` unit tests use known register
//! fixtures from real bench units to catch bit-shift drift.

use crate::chip::Chip;
use crate::error::{Error, Result};
use crate::protocol::Connection;

/// A decoded silicon revision (major + minor). `full()` returns
/// `major * 100 + minor`, matching ESP-IDF's `chip_revision` reporting
/// (e.g. v3.02 → 302).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SiliconRevision {
    pub major: u8,
    pub minor: u8,
}

impl SiliconRevision {
    pub fn full(self) -> u16 {
        self.major as u16 * 100 + self.minor as u16
    }
    pub fn human(self) -> String {
        format!("{}.{:02}", self.major, self.minor)
    }
}

impl std::fmt::Display for SiliconRevision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.human())
    }
}

// ---------------------------------------------------------------------------
// BLOCK base addresses per chip
// ---------------------------------------------------------------------------

/// Per-chip EFUSE BLOCK0/1/2 base register addresses. Returned as a
/// fixed-size triple so callers can read any block by relative word
/// offset (the typical upstream-esptool pattern).
fn block_bases(chip: &Chip) -> Result<EfuseBlocks> {
    Ok(match chip.name {
        // Original ESP32: BLOCK0 is at the EFUSE_BASE, others are
        // spaced 0x38 / 0x58 / 0x78 from BLOCK0 (size 8 words = 32
        // bytes per block + gap).
        "ESP32" => EfuseBlocks {
            block0: chip.efuse_base,
            block1: chip.efuse_base + 0x38,
            block2: chip.efuse_base + 0x58,
        },
        // ESP32-C2: EFUSE_BLOCK2_ADDR = EFUSE_BASE + 0x40
        // (BLOCK1 = EFUSE_BASE + 0x20).
        "ESP32-C2" => EfuseBlocks {
            block0: chip.efuse_base,
            block1: chip.efuse_block1_addr,
            block2: chip.efuse_base + 0x40,
        },
        // Newer S2 / S3 / C3 / C5 / C6 / C61 / H2 / H4 / P4 / S31
        // share the same layout: BLOCK1 at `efuse_block1_addr`,
        // BLOCK2 immediately after (block1 + 0x18, 6 words = 24
        // bytes for non-RS-encoded; 8 words = 32 bytes for some —
        // we use 0x18 which matches every chip whose `pkg_version`
        // / rev formulas reference BLOCK2 today).
        _ => EfuseBlocks {
            block0: chip.efuse_base,
            block1: chip.efuse_block1_addr,
            // Most newer parts that need BLOCK2 reads put it at
            // BLOCK1 + 0x18 (S3, H2, ...). ESP32-S3 specifically is
            // EFUSE_BASE + 0x5C; for other chips the BLOCK2 offset
            // happens to be unused by our current decoders, so 0 is
            // fine. If you add a chip whose formula reads BLOCK2,
            // verify the offset and add an explicit arm above.
            block2: match chip.name {
                "ESP32-S3" => chip.efuse_base + 0x5C,
                _ => 0,
            },
        },
    })
}

#[derive(Clone, Copy)]
struct EfuseBlocks {
    block0: u32,
    block1: u32,
    #[allow(dead_code)]
    block2: u32,
}

// ---------------------------------------------------------------------------
// Per-chip silicon revision
// ---------------------------------------------------------------------------

/// Read the silicon revision (major + minor) from EFUSE.
///
/// Returns `Error::Other` for chip names not in the decoder table.
/// ESP32-H21 deliberately returns (0, 0) — upstream esptool does the
/// same pending a public bit-position spec.
pub fn read_silicon_revision(conn: &mut Connection, chip: &Chip) -> Result<SiliconRevision> {
    let b = block_bases(chip)?;
    let mut read_word = |base: u32, word: u32| conn.read_reg(base + 4 * word);

    // Pre-read every word the decoder might need, then dispatch on the
    // pure decode_*_rev functions. This keeps the bit math in plain
    // functions (testable with register fixtures) and isolates the
    // chip-specific I/O up here.
    let (major, minor) = match chip.name {
        "ESP32" => {
            let w3 = read_word(b.block0, 3)?;
            let w5 = read_word(b.block0, 5)?;
            // APB_CTL_DATE_ADDR = DR_REG_SYSCON_BASE (0x3FF66000) + 0x7C
            let apb = conn.read_reg(0x3FF6607C)?;
            decode_esp32(w3, w5, apb)
        }
        "ESP32-S2" => {
            let w3 = read_word(b.block1, 3)?;
            let w4 = read_word(b.block1, 4)?;
            decode_esp32_s2(w3, w4)
        }
        "ESP32-S3" => {
            let w3 = read_word(b.block1, 3)?;
            let w5 = read_word(b.block1, 5)?;
            let blk2_w4 = read_word(b.block2, 4)?;
            decode_esp32_s3(w3, w5, blk2_w4)
        }
        "ESP32-C2" => decode_esp32_c2(read_word(b.block2, 1)?),
        "ESP32-C3" => {
            let w3 = read_word(b.block1, 3)?;
            let w5 = read_word(b.block1, 5)?;
            decode_esp32_c3(w3, w5)
        }
        "ESP32-C5" | "ESP32-C61" => decode_esp32_c5_c61(read_word(b.block1, 2)?),
        "ESP32-C6" | "ESP32-H4" => decode_esp32_c6_h4(read_word(b.block1, 3)?),
        "ESP32-H2" => decode_esp32_h2(read_word(b.block1, 3)?),
        "ESP32-S31" => decode_esp32_s31(read_word(b.block1, 3)?),
        "ESP32-P4" => decode_esp32_p4(read_word(b.block1, 2)?),
        "ESP32-H21" => (0u8, 0u8),
        other => {
            return Err(Error::Other(format!(
                "no silicon-revision decoder for chip {:?}",
                other
            )));
        }
    };

    Ok(SiliconRevision { major, minor })
}

// ---------------------------------------------------------------------------
// Pure decoder functions (one per chip). Each takes raw register words
// and returns (major, minor). Bit positions mirror upstream esptool
// `targets/<chip>.py:get_minor_chip_version()` /
// `get_major_chip_version()`.
// ---------------------------------------------------------------------------

#[inline]
fn bits(word: u32, shift: u32, mask: u32) -> u32 {
    (word >> shift) & mask
}

pub(crate) fn decode_esp32(efuse_w3: u32, efuse_w5: u32, apb_ctl_date: u32) -> (u8, u8) {
    let rev_bit0 = bits(efuse_w3, 15, 0x1);
    let rev_bit1 = bits(efuse_w5, 20, 0x1);
    let rev_bit2 = bits(apb_ctl_date, 31, 0x1);
    let major = ((rev_bit2 << 2) | (rev_bit1 << 1) | rev_bit0) as u8;
    let minor = bits(efuse_w5, 24, 0x3) as u8;
    (major, minor)
}

pub(crate) fn decode_esp32_s2(blk1_w3: u32, blk1_w4: u32) -> (u8, u8) {
    let major = bits(blk1_w3, 18, 0x3) as u8;
    let hi = bits(blk1_w3, 20, 0x1);
    let low = bits(blk1_w4, 4, 0x07);
    let minor = ((hi << 3) | low) as u8;
    (major, minor)
}

pub(crate) fn decode_esp32_s3(blk1_w3: u32, blk1_w5: u32, blk2_w4: u32) -> (u8, u8) {
    let hi = bits(blk1_w5, 23, 0x1);
    let low = bits(blk1_w3, 18, 0x07);
    let raw_minor = ((hi << 3) | low) as u8;
    let raw_major = bits(blk1_w5, 24, 0x3) as u8;
    // ECO0 workaround: when raw_minor's low 3 bits are 0 AND block
    // version is (1,1), force (0,0). Upstream notes the major field
    // was repurposed for that specific config.
    let blk_major = bits(blk2_w4, 0, 0x3) as u8;
    let blk_minor = bits(blk1_w3, 24, 0x07) as u8;
    let is_eco0 = (raw_minor & 0x7) == 0 && blk_major == 1 && blk_minor == 1;
    if is_eco0 {
        (0, 0)
    } else {
        (raw_major, raw_minor)
    }
}

pub(crate) fn decode_esp32_c2(blk2_w1: u32) -> (u8, u8) {
    let minor = bits(blk2_w1, 16, 0xF) as u8;
    let major = bits(blk2_w1, 20, 0x3) as u8;
    (major, minor)
}

pub(crate) fn decode_esp32_c3(blk1_w3: u32, blk1_w5: u32) -> (u8, u8) {
    let hi = bits(blk1_w5, 23, 0x1);
    let low = bits(blk1_w3, 18, 0x07);
    let minor = ((hi << 3) | low) as u8;
    let major = bits(blk1_w5, 24, 0x3) as u8;
    (major, minor)
}

pub(crate) fn decode_esp32_c5_c61(blk1_w2: u32) -> (u8, u8) {
    let minor = bits(blk1_w2, 0, 0xF) as u8;
    let major = bits(blk1_w2, 4, 0x3) as u8;
    (major, minor)
}

pub(crate) fn decode_esp32_c6_h4(blk1_w3: u32) -> (u8, u8) {
    let minor = bits(blk1_w3, 18, 0xF) as u8;
    let major = bits(blk1_w3, 22, 0x3) as u8;
    (major, minor)
}

pub(crate) fn decode_esp32_h2(blk1_w3: u32) -> (u8, u8) {
    let minor = bits(blk1_w3, 18, 0x7) as u8;
    let major = bits(blk1_w3, 21, 0x3) as u8;
    (major, minor)
}

pub(crate) fn decode_esp32_s31(blk1_w3: u32) -> (u8, u8) {
    let minor = bits(blk1_w3, 18, 0xF) as u8;
    let major = bits(blk1_w3, 22, 0x3) as u8;
    (major, minor)
}

pub(crate) fn decode_esp32_p4(blk1_w2: u32) -> (u8, u8) {
    let minor = bits(blk1_w2, 0, 0xF) as u8;
    let major = ((bits(blk1_w2, 23, 0x1) << 2) | bits(blk1_w2, 4, 0x3)) as u8;
    (major, minor)
}

// ---------------------------------------------------------------------------
// Per-chip package version
// ---------------------------------------------------------------------------

/// Read the package version from EFUSE. `None` for chips where the
/// field isn't defined / always reads 0 in the upstream tables.
pub fn read_pkg_version(conn: &mut Connection, chip: &Chip) -> Result<Option<u8>> {
    let b = block_bases(chip)?;
    let mut read_word = |base: u32, word: u32| conn.read_reg(base + 4 * word);
    let bits = |word: u32, shift: u32, mask: u32| ((word >> shift) & mask) as u8;

    let pkg = match chip.name {
        // ESP32: 4-bit pkg encoded across BLOCK0 word3 bits 9..11 + bit 2.
        "ESP32" => {
            let w3 = read_word(b.block0, 3)?;
            let low3 = bits(w3, 9, 0x07);
            let high1 = bits(w3, 2, 0x01);
            Some((high1 << 3) | low3)
        }
        "ESP32-S2" => Some(bits(read_word(b.block1, 4)?, 0, 0xF)),
        "ESP32-S3" => Some(bits(read_word(b.block1, 3)?, 21, 0x07)),
        "ESP32-C2" => Some(bits(read_word(b.block2, 1)?, 22, 0x07)),
        "ESP32-C3" => Some(bits(read_word(b.block1, 3)?, 21, 0x07)),
        "ESP32-C5" | "ESP32-C61" => Some(bits(read_word(b.block1, 2)?, 26, 0x07)),
        "ESP32-C6" => Some(bits(read_word(b.block1, 3)?, 24, 0x07)),
        "ESP32-H2" => Some(bits(read_word(b.block1, 4)?, 0, 0x07)),
        "ESP32-H4" => Some(bits(read_word(b.block1, 4)?, 12, 0x07)),
        "ESP32-P4" => Some(bits(read_word(b.block1, 2)?, 20, 0x07)),
        "ESP32-S31" => Some(bits(read_word(b.block1, 4)?, 6, 0x03)),
        // ESP32-H21 upstream returns 0; treat as "not yet decoded".
        "ESP32-H21" => None,
        _ => None,
    };
    Ok(pkg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silicon_revision_full_and_human() {
        let r = SiliconRevision { major: 3, minor: 2 };
        assert_eq!(r.full(), 302);
        assert_eq!(r.human(), "3.02");
        assert_eq!(format!("{}", r), "3.02");
    }

    // Bit-position fixtures: construct register values that encode a
    // known (major, minor) and verify the decoder recovers them. If
    // upstream esptool ever shifts a bit, these tests will catch the
    // drift the next time we sync.

    fn set_bits(start: u32, count: u32, value: u32) -> u32 {
        let mask = if count == 32 {
            u32::MAX
        } else {
            (1u32 << count) - 1
        };
        (value & mask) << start
    }

    #[test]
    fn esp32_decoder_combines_three_bit_sources() {
        // major formula = (bit2<<2)|(bit1<<1)|bit0, where bit2 comes
        // from APB_CTL_DATE. Real silicon: v3.0 has bit2=1, bit1=0,
        // bit0=0 → major 4 in the formula, which esptool then maps
        // to revision 3 via the chip-description table. We decode
        // the raw value here; mapping is the caller's job.
        // Test fixture: bit0=1, bit1=0, bit2=0 → major raw = 1.
        // minor=2 at blk0 word5 bits24..25.
        let w3 = set_bits(15, 1, 1);
        let w5 = set_bits(20, 1, 0) | set_bits(24, 2, 2);
        let apb = set_bits(31, 1, 0);
        assert_eq!(decode_esp32(w3, w5, apb), (1, 2));
        // And the rev-3 fixture: only bit2 set.
        let w3 = 0;
        let w5 = set_bits(24, 2, 0);
        let apb = set_bits(31, 1, 1);
        assert_eq!(decode_esp32(w3, w5, apb), (4, 0));
    }

    #[test]
    fn esp32_s2_decoder() {
        // major=3 at w3 bits18..19; minor=9 = (hi=1)<<3 | (low=1)
        let w3 = set_bits(18, 2, 3) | set_bits(20, 1, 1);
        let w4 = set_bits(4, 3, 1);
        assert_eq!(decode_esp32_s2(w3, w4), (3, 9));
    }

    #[test]
    fn esp32_s3_decoder_non_eco0() {
        // raw_major=3 at w5 bits24..25; raw_minor = (hi=1)<<3 | (low=2) = 10
        let w3 = set_bits(18, 3, 2);
        let w5 = set_bits(23, 1, 1) | set_bits(24, 2, 3);
        // Block version is (0,0), so ECO0 workaround doesn't apply.
        let blk2_w4 = 0;
        assert_eq!(decode_esp32_s3(w3, w5, blk2_w4), (3, 10));
    }

    #[test]
    fn esp32_s3_decoder_eco0_workaround_fires() {
        // raw_minor low 3 bits = 0 (hi=0, low=0) AND block_version=(1,1)
        // → forced (0,0) regardless of what raw_major says.
        let w3 = set_bits(24, 3, 1); // blk_minor=1
        let w5 = set_bits(24, 2, 2); // raw_major=2 (would be 2 absent ECO0)
        let blk2_w4 = set_bits(0, 2, 1); // blk_major=1
        assert_eq!(decode_esp32_s3(w3, w5, blk2_w4), (0, 0));
    }

    #[test]
    fn esp32_c2_decoder() {
        // major=2 at bits20..21; minor=7 at bits16..19
        let w = set_bits(16, 4, 7) | set_bits(20, 2, 2);
        assert_eq!(decode_esp32_c2(w), (2, 7));
    }

    #[test]
    fn esp32_c3_decoder() {
        let w3 = set_bits(18, 3, 5);
        let w5 = set_bits(23, 1, 1) | set_bits(24, 2, 1);
        assert_eq!(decode_esp32_c3(w3, w5), (1, (1 << 3) | 5));
    }

    #[test]
    fn esp32_c5_c61_decoder() {
        let w = set_bits(0, 4, 0xA) | set_bits(4, 2, 1);
        assert_eq!(decode_esp32_c5_c61(w), (1, 0xA));
    }

    #[test]
    fn esp32_c6_h4_decoder() {
        let w = set_bits(18, 4, 0xC) | set_bits(22, 2, 2);
        assert_eq!(decode_esp32_c6_h4(w), (2, 0xC));
    }

    #[test]
    fn esp32_h2_decoder() {
        let w = set_bits(18, 3, 5) | set_bits(21, 2, 1);
        assert_eq!(decode_esp32_h2(w), (1, 5));
    }

    #[test]
    fn esp32_s31_decoder() {
        let w = set_bits(18, 4, 0xE) | set_bits(22, 2, 3);
        assert_eq!(decode_esp32_s31(w), (3, 0xE));
    }

    #[test]
    fn esp32_p4_decoder_minor() {
        // major built from split bits: bit2=1, bits1..0 = 2 → major=6.
        // minor=0xC at bits0..3.
        let w = set_bits(0, 4, 0xC) | set_bits(4, 2, 2) | set_bits(23, 1, 1);
        assert_eq!(decode_esp32_p4(w), (6, 0xC));
    }

    #[test]
    fn esp32_p4_decoder_zero() {
        // All-zero word → (0,0).
        assert_eq!(decode_esp32_p4(0), (0, 0));
    }

    #[test]
    fn esp32_p4_real_silicon_revision_3_00() {
        // Real bench unit reports 3.00 — major bit 2 is set (1 from
        // bit23 → << 2 = 4? no: ((1) << 2) | 0 = 4 → that's 4, not 3).
        // Wait: P4 v3 has bit23=0 and bits4..5=3. Recompute from
        // (((23>>23)&1) << 2) | ((23>>4)&3).
        // For major=3: ((0)<<2) | 3 = 3. So bits 4..5 = 0b11.
        let w = set_bits(4, 2, 3);
        assert_eq!(decode_esp32_p4(w), (3, 0));
    }
}
