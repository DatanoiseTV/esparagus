//! ESP32 firmware image header parsing.
//!
//! Just enough to validate that a file the user is about to flash actually
//! looks like a bootable image — we surface a structured warning if the magic
//! byte is wrong or the chip_id mismatches, but we don't refuse to flash
//! (some workflows legitimately write raw data, partition tables, NVS dumps
//! etc.).

use std::path::Path;

/// First byte of every ESP32 application image header (matches upstream
/// `ESPLoader.ESP_IMAGE_MAGIC = 0xE9`).
pub const ESP_IMAGE_MAGIC: u8 = 0xE9;

#[derive(Debug, Clone)]
pub struct ImageHeader {
    pub magic: u8,
    pub segment_count: u8,
    pub flash_mode: u8,
    pub flash_size_freq: u8,
    pub entry_addr: u32,
    pub wp_pin: u8,
    pub spi_pin_drv: [u8; 3],
    /// IMAGE_CHIP_ID — used to verify image was built for the connected chip.
    pub chip_id: u16,
    pub min_chip_rev: u8,
    pub min_chip_rev_full: u16,
    pub max_chip_rev_full: u16,
    pub hash_appended: bool,
}

impl ImageHeader {
    /// Parse the first 24 bytes of an ESP image file. Returns `None` if the
    /// data is too short — caller can then treat the file as raw data.
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 24 {
            return None;
        }
        let entry_addr = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let chip_id = u16::from_le_bytes([data[12], data[13]]);
        let min_chip_rev_full = u16::from_le_bytes([data[15], data[16]]);
        let max_chip_rev_full = u16::from_le_bytes([data[17], data[18]]);
        Some(Self {
            magic: data[0],
            segment_count: data[1],
            flash_mode: data[2],
            flash_size_freq: data[3],
            entry_addr,
            wp_pin: data[8],
            spi_pin_drv: [data[9], data[10], data[11]],
            chip_id,
            min_chip_rev: data[14],
            min_chip_rev_full,
            max_chip_rev_full,
            hash_appended: data[23] != 0,
        })
    }
}

/// Load a flash payload from disk. Returns the bytes plus an optional parsed
/// header (only present when the file looks like a real ESP image).
pub fn load_payload(path: &Path) -> std::io::Result<(Vec<u8>, Option<ImageHeader>)> {
    let bytes = std::fs::read(path)?;
    let header = ImageHeader::parse(&bytes).filter(|h| h.magic == ESP_IMAGE_MAGIC);
    Ok((bytes, header))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        let mut data = [0u8; 32];
        data[0] = ESP_IMAGE_MAGIC;
        data[1] = 6; // segment_count
        data[12] = 9; // chip_id ESP32-S3
        let h = ImageHeader::parse(&data).unwrap();
        assert_eq!(h.magic, ESP_IMAGE_MAGIC);
        assert_eq!(h.segment_count, 6);
        assert_eq!(h.chip_id, 9);
    }

    #[test]
    fn header_short_returns_none() {
        let data = [0u8; 8];
        assert!(ImageHeader::parse(&data).is_none());
    }
}
