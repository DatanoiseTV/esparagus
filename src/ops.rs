//! High-level operations: SPI flash command pass-through, MAC read, flash
//! attach, set params, write_flash (uncompressed + compressed), read_flash,
//! erase, MD5 verify.
//!
//! These functions operate on a `protocol::Connection` and a `chip::Chip`,
//! so any combination of chip + ROM-vs-stub works uniformly.

use std::time::{Duration, Instant};

use byteorder::{ByteOrder, LittleEndian};
use md5::{Digest, Md5};
use miniz_oxide::deflate::{compress_to_vec_zlib, CompressionLevel};
use tracing::{debug, info};

use crate::chip::{Chip, SpiLayout};
use crate::error::{Error, Result};
use crate::protocol::commands::{checksum, Cmd};
use crate::protocol::{Connection, CHIP_ERASE_TIMEOUT, DEFAULT_TIMEOUT};

/// Flash sector size (4 KiB) — minimum unit of erase.
pub const FLASH_SECTOR_SIZE: u32 = 0x1000;
/// Flash write block size in bytes (matches upstream `FLASH_WRITE_SIZE = 0x400`).
pub const FLASH_WRITE_SIZE: usize = 0x400;

/// Number of attempts per data block in write_flash, matching upstream's
/// `WRITE_BLOCK_ATTEMPTS = 3`.
const WRITE_BLOCK_ATTEMPTS: u32 = 3;

/// Per-MB scaling for erase and MD5 timeouts.
const ERASE_REGION_TIMEOUT_PER_MB: Duration = Duration::from_secs(30);
const MD5_TIMEOUT_PER_MB: Duration = Duration::from_secs(8);

fn timeout_per_mb(per_mb: Duration, size_bytes: usize) -> Duration {
    let mb = (size_bytes as f64) / (1024.0 * 1024.0);
    let scaled = per_mb.mul_f64(mb.max(1.0));
    scaled.max(DEFAULT_TIMEOUT)
}

// ---------------------------------------------------------------------------
// SPI flash command pass-through (used for RDID / SFDP / RDSR).
// Matches upstream `ESPLoader.run_spiflash_command()` exactly: drives the
// chip's SPI peripheral via WRITE_REG / READ_REG to send arbitrary SPI cmds.
// ---------------------------------------------------------------------------

const SPI_USR_COMMAND: u32 = 1 << 31;
const SPI_USR_ADDR: u32 = 1 << 30;
const SPI_USR_DUMMY: u32 = 1 << 29;
const SPI_USR_MISO: u32 = 1 << 28;
const SPI_USR_MOSI: u32 = 1 << 27;
const SPI_CMD_USR: u32 = 1 << 18;
const SPI_USR2_COMMAND_LEN_SHIFT: u32 = 28;
const SPI_USR_ADDR_LEN_SHIFT: u32 = 26;

#[allow(clippy::too_many_arguments)]
pub fn run_spiflash_command(
    conn: &mut Connection,
    chip: &Chip,
    spi_cmd: u8,
    data: &[u8],
    read_bits: u32,
    addr: Option<u32>,
    addr_len: u32,
    dummy_len: u32,
) -> Result<u32> {
    if read_bits > 32 {
        return Err(Error::Other(
            "reading more than 32 bits with one SPI command is unsupported".into(),
        ));
    }
    if data.len() > 64 {
        return Err(Error::Other(
            "writing more than 64 bytes with one SPI command is unsupported".into(),
        ));
    }

    let spi = &chip.spi;
    let base = spi.reg_base;
    let cmd_reg = base;
    let addr_reg = base + 0x04;
    let usr_reg = base + spi.usr_offs;
    let usr2_reg = base + spi.usr2_offs;
    let w0_reg = base + spi.w0_offs;

    let data_bits = (data.len() * 8) as u32;
    let old_spi_usr = conn.read_reg(usr_reg)?;
    let old_spi_usr2 = conn.read_reg(usr2_reg)?;

    let mut flags = SPI_USR_COMMAND;
    if read_bits > 0 {
        flags |= SPI_USR_MISO;
    }
    if data_bits > 0 {
        flags |= SPI_USR_MOSI;
    }
    if addr_len > 0 {
        flags |= SPI_USR_ADDR;
    }
    if dummy_len > 0 {
        flags |= SPI_USR_DUMMY;
    }

    set_data_lengths(conn, spi, data_bits, read_bits, addr_len, dummy_len)?;

    conn.write_reg(usr_reg, flags, 0xFFFF_FFFF, 0)?;
    let cmd_code = (7u32 << SPI_USR2_COMMAND_LEN_SHIFT) | (spi_cmd as u32);
    conn.write_reg(usr2_reg, cmd_code, 0xFFFF_FFFF, 0)?;

    if addr_len > 0 {
        let mut a = addr.unwrap_or(0);
        if spi.addr_reg_msb {
            a <<= 32u32 - addr_len;
        }
        conn.write_reg(addr_reg, a, 0xFFFF_FFFF, 0)?;
    }
    if data_bits == 0 {
        conn.write_reg(w0_reg, 0, 0xFFFF_FFFF, 0)?;
    } else {
        // Pack `data` into little-endian 32-bit words.
        let mut padded = data.to_vec();
        while padded.len() % 4 != 0 {
            padded.push(0);
        }
        let mut next = w0_reg;
        for word in padded.chunks(4) {
            let v = LittleEndian::read_u32(word);
            conn.write_reg(next, v, 0xFFFF_FFFF, 0)?;
            next += 4;
        }
    }
    conn.write_reg(cmd_reg, SPI_CMD_USR, 0xFFFF_FFFF, 0)?;

    // Wait for SPI_CMD_USR to go back to zero — up to 10 polls per upstream.
    for _ in 0..10 {
        if (conn.read_reg(cmd_reg)? & SPI_CMD_USR) == 0 {
            break;
        }
    }
    if (conn.read_reg(cmd_reg)? & SPI_CMD_USR) != 0 {
        return Err(Error::Other("SPI command did not complete in time".into()));
    }

    let status = conn.read_reg(w0_reg)?;

    // Restore SPI controller registers.
    conn.write_reg(usr_reg, old_spi_usr, 0xFFFF_FFFF, 0)?;
    conn.write_reg(usr2_reg, old_spi_usr2, 0xFFFF_FFFF, 0)?;

    // If `read_bits < 32`, mask off the unused high bits.
    let mask = if read_bits == 32 {
        0xFFFF_FFFF
    } else if read_bits == 0 {
        0
    } else {
        (1u32 << read_bits) - 1
    };
    Ok(status & if read_bits == 0 { 0xFFFF_FFFF } else { mask })
}

fn set_data_lengths(
    conn: &mut Connection,
    spi: &SpiLayout,
    mosi_bits: u32,
    miso_bits: u32,
    addr_len: u32,
    dummy_len: u32,
) -> Result<()> {
    let base = spi.reg_base;
    let usr1_reg = base + spi.usr1_offs;
    if let (Some(mosi_off), Some(miso_off)) = (spi.mosi_dlen_offs, spi.miso_dlen_offs) {
        // ESP32+ has dedicated registers for data lengths.
        if mosi_bits > 0 {
            conn.write_reg(base + mosi_off, mosi_bits - 1, 0xFFFF_FFFF, 0)?;
        }
        if miso_bits > 0 {
            conn.write_reg(base + miso_off, miso_bits - 1, 0xFFFF_FFFF, 0)?;
        }
        let mut flags = 0u32;
        if dummy_len > 0 {
            flags |= dummy_len - 1;
        }
        if addr_len > 0 {
            flags |= (addr_len - 1) << SPI_USR_ADDR_LEN_SHIFT;
        }
        if flags != 0 {
            conn.write_reg(usr1_reg, flags, 0xFFFF_FFFF, 0)?;
        }
    } else {
        // ESP8266 / older: lengths packed into USR1.
        const MOSI_SHIFT: u32 = 17;
        const MISO_SHIFT: u32 = 8;
        let mosi_mask = if mosi_bits == 0 { 0 } else { mosi_bits - 1 };
        let miso_mask = if miso_bits == 0 { 0 } else { miso_bits - 1 };
        let mut flags = (miso_mask << MISO_SHIFT) | (mosi_mask << MOSI_SHIFT);
        if dummy_len > 0 {
            flags |= dummy_len - 1;
        }
        if addr_len > 0 {
            flags |= (addr_len - 1) << SPI_USR_ADDR_LEN_SHIFT;
        }
        conn.write_reg(usr1_reg, flags, 0xFFFF_FFFF, 0)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public ops
// ---------------------------------------------------------------------------

/// JEDEC RDID (0x9F) — returns 24 bits: [manufacturer_id, dev_id_hi, dev_id_lo].
pub fn flash_id(conn: &mut Connection, chip: &Chip) -> Result<u32> {
    run_spiflash_command(conn, chip, 0x9F, &[], 24, None, 0, 0)
}

/// Decode flash size in megabytes from the JEDEC capacity byte of RDID.
///
/// Convention used by Winbond / GigaDevice / Macronix / ISSI / XMC: the
/// capacity byte (the third byte returned by 0x9F, i.e. bits 16..24 of the
/// little-endian u32 read out of W0) encodes `log2(size_in_bytes)`:
///   0x14 → 1 MB, 0x15 → 2 MB, 0x16 → 4 MB, 0x17 → 8 MB,
///   0x18 → 16 MB, 0x19 → 32 MB, 0x1A → 64 MB, 0x1B → 128 MB.
pub fn flash_size_mb_from_id(flash_id: u32) -> Option<u32> {
    let cap = ((flash_id >> 16) & 0xFF) as u8;
    if (0x14..=0x1B).contains(&cap) {
        Some(1u32 << (cap - 0x14))
    } else {
        None
    }
}

/// Extract the 16-bit JEDEC device ID from the raw u32 read out of W0.
/// SPI peripheral packs bytes little-endian: byte0=mfr, byte1=type, byte2=cap.
/// JEDEC convention prints dev_id as (type << 8) | capacity.
pub fn flash_dev_id(flash_id: u32) -> u16 {
    let dev_type = ((flash_id >> 8) & 0xFF) as u16;
    let cap = ((flash_id >> 16) & 0xFF) as u16;
    (dev_type << 8) | cap
}

/// Attach to the SPI flash (configures pins). Matches `ESPLoader.flash_spi_attach`.
/// `hspi_arg` is 0 for default pins (the usual case).
pub fn flash_spi_attach(conn: &mut Connection, hspi_arg: u32) -> Result<()> {
    let mut payload = vec![0u8; 4];
    LittleEndian::write_u32(&mut payload, hspi_arg);
    if !conn.stub_uploaded {
        // ROM bootloader takes 4 extra reserved/zero bytes.
        payload.extend_from_slice(&[0, 0, 0, 0]);
    }
    conn.check_command(
        "configure SPI flash pins",
        Cmd::SpiAttach,
        &payload,
        0,
        0,
        DEFAULT_TIMEOUT,
    )?;
    Ok(())
}

/// Tell the ROM how big the flash chip is and a few other parameters. Matches
/// `ESPLoader.flash_set_parameters`.
pub fn spi_set_params(conn: &mut Connection, size_bytes: u32) -> Result<()> {
    let mut payload = [0u8; 24];
    LittleEndian::write_u32(&mut payload[0..4], 0); // fl_id
    LittleEndian::write_u32(&mut payload[4..8], size_bytes);
    LittleEndian::write_u32(&mut payload[8..12], 64 * 1024); // block_size
    LittleEndian::write_u32(&mut payload[12..16], 4 * 1024); // sector_size
    LittleEndian::write_u32(&mut payload[16..20], 256); // page_size
    LittleEndian::write_u32(&mut payload[20..24], 0xFFFF); // status_mask
    conn.check_command(
        "set SPI params",
        Cmd::SpiSetParams,
        &payload,
        0,
        0,
        DEFAULT_TIMEOUT,
    )?;
    Ok(())
}

/// Read the 6-byte BASE_MAC from EFUSE. For most chips, MAC_EFUSE_REG points
/// at a 4-byte word; the high 2 bytes of (reg+4) are the upper part.
///
/// ESP32 uses a different EFUSE layout (BLOCK0 words 1+2 with a 2-byte CRC at
/// the front), which we handle as a special case.
pub fn read_mac(conn: &mut Connection, chip: &Chip) -> Result<[u8; 6]> {
    if chip.name == "ESP32" {
        // ESP32 original layout: BLOCK0 word 1 (0x3FF5A004) holds MAC[2..6]
        // (low->high), BLOCK0 word 2 (0x3FF5A008) holds the rest in big-endian
        // with a 2-byte CRC prepended.
        let w2 = conn.read_reg(0x3FF5A008)?;
        let w1 = conn.read_reg(0x3FF5A004)?;
        // Pack as big-endian word2 ++ word1, then trim the 2-byte CRC.
        let mut buf = [0u8; 8];
        buf[0..4].copy_from_slice(&w2.to_be_bytes());
        buf[4..8].copy_from_slice(&w1.to_be_bytes());
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&buf[2..8]);
        return Ok(mac);
    }
    let mac0 = conn.read_reg(chip.mac_efuse_reg)?;
    let mac1 = conn.read_reg(chip.mac_efuse_reg + 4)?;
    // Low 4 bytes from mac0, top 2 bytes from low 16 bits of mac1, in network
    // order — matches upstream `read_mac()` in S2/S3/C3 paths.
    let mut buf = [0u8; 8];
    buf[0..2].copy_from_slice(&(mac1 as u16).to_be_bytes());
    buf[2..6].copy_from_slice(&mac0.to_be_bytes());
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&buf[0..6]);
    // The natural order is high→low; we returned the bytes in the layout
    // that matches `XX:XX:XX:XX:XX:XX` when each byte is printed.
    // Reverse so that mac[0] is the OUI MSB.
    // (Validated against esptool's `read_mac` which returns tuple(reversed)).
    let mut out = [0u8; 6];
    for i in 0..6 {
        out[i] = mac[5 - i];
    }
    Ok(out)
}

/// Format a MAC as XX:XX:XX:XX:XX:XX.
pub fn format_mac(mac: &[u8; 6]) -> String {
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

// ---------------------------------------------------------------------------
// Erase
// ---------------------------------------------------------------------------

/// Erase the whole flash chip. Stub-only command.
pub fn erase_flash(conn: &mut Connection) -> Result<()> {
    if !conn.stub_running {
        return Err(Error::Other(
            "erase-flash requires the stub loader (don't pass --no-stub)".into(),
        ));
    }
    conn.check_command(
        "erase flash",
        Cmd::EraseFlash,
        &[],
        0,
        0,
        CHIP_ERASE_TIMEOUT,
    )?;
    Ok(())
}

/// Erase a region of flash. Offset and size must be sector-aligned. Stub-only.
pub fn erase_region(conn: &mut Connection, offset: u32, size: u32) -> Result<()> {
    if !conn.stub_running {
        return Err(Error::Other(
            "erase-region requires the stub loader (don't pass --no-stub)".into(),
        ));
    }
    if offset % FLASH_SECTOR_SIZE != 0 || size % FLASH_SECTOR_SIZE != 0 {
        return Err(Error::Other(format!(
            "erase region offset and size must be {}B aligned",
            FLASH_SECTOR_SIZE
        )));
    }
    let mut payload = [0u8; 8];
    LittleEndian::write_u32(&mut payload[0..4], offset);
    LittleEndian::write_u32(&mut payload[4..8], size);
    let timeout = timeout_per_mb(ERASE_REGION_TIMEOUT_PER_MB, size as usize);
    conn.check_command("erase region", Cmd::EraseRegion, &payload, 0, 0, timeout)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Write flash (compressed)
// ---------------------------------------------------------------------------

/// Worker erase-size hint for FLASH_BEGIN — number of bytes the ROM will
/// erase up front, rounded up to a sector boundary.
fn rounded_erase_size(offset: u32, size: u32) -> u32 {
    let head = offset % FLASH_SECTOR_SIZE;
    let tail = (head + size).div_ceil(FLASH_SECTOR_SIZE) * FLASH_SECTOR_SIZE;
    tail - head
}

/// Progress callback signature: (bytes_written, total).
pub type ProgressFn<'a> = &'a mut dyn FnMut(u64, u64);

/// Write `data` to flash at `addr`. Dispatches to the compressed
/// (FLASH_DEFL_*) or raw (FLASH_DATA) wire path. The chip-side behavior is
/// identical: ROM erases up front (slow); stub erases as it writes.
pub fn write_flash(
    conn: &mut Connection,
    chip: &Chip,
    addr: u32,
    data: &[u8],
    progress: Option<ProgressFn>,
    compress: bool,
) -> Result<String> {
    if compress {
        write_flash_compressed(conn, chip, addr, data, progress)
    } else {
        write_flash_uncompressed(conn, chip, addr, data, progress)
    }
}

/// Compressed write path. Sends zlib-deflate blocks via FLASH_DEFL_BEGIN /
/// FLASH_DEFL_DATA / FLASH_DEFL_END.  esptool's default.
pub fn write_flash_compressed(
    conn: &mut Connection,
    chip: &Chip,
    addr: u32,
    data: &[u8],
    mut progress: Option<ProgressFn>,
) -> Result<String> {
    let size = data.len() as u32;
    if size == 0 {
        return Ok(String::new());
    }

    // Compress with zlib level=9 (matches upstream `compress(level=9)`).
    let compressed = compress_to_vec_zlib(data, CompressionLevel::BestCompression as u8);
    let comp_size = compressed.len();
    info!(
        addr = format_args!("{:#010x}", addr),
        original = size,
        compressed = comp_size,
        "flash deflate begin"
    );

    let num_blocks = comp_size.div_ceil(FLASH_WRITE_SIZE);
    let erase_size = rounded_erase_size(addr, size);

    // FLASH_DEFL_BEGIN params.
    let write_size = if conn.stub_running {
        size // stub: pass uncompressed length and let it manage erase
    } else {
        let erase_blocks = (size as usize).div_ceil(FLASH_WRITE_SIZE);
        (erase_blocks * FLASH_WRITE_SIZE) as u32
    };

    let mut params = Vec::with_capacity(20);
    let mut buf = [0u8; 16];
    LittleEndian::write_u32(&mut buf[0..4], write_size);
    LittleEndian::write_u32(&mut buf[4..8], num_blocks as u32);
    LittleEndian::write_u32(&mut buf[8..12], FLASH_WRITE_SIZE as u32);
    LittleEndian::write_u32(&mut buf[12..16], addr);
    params.extend_from_slice(&buf);
    // ESP32 ROM doesn't support the extra `encrypted_write` arg; everything
    // else does (and we never pass encrypted_write=1 here).
    if conn.stub_running || chip.name != "ESP32" {
        params.extend_from_slice(&[0, 0, 0, 0]);
    }

    let begin_timeout = if conn.stub_running {
        DEFAULT_TIMEOUT
    } else {
        timeout_per_mb(ERASE_REGION_TIMEOUT_PER_MB, erase_size as usize)
    };
    let t = Instant::now();
    conn.check_command(
        "enter compressed flash mode",
        Cmd::FlashDeflBegin,
        &params,
        0,
        0,
        begin_timeout,
    )?;
    if !conn.stub_running {
        debug!("ROM erase took {:.2}s", t.elapsed().as_secs_f64());
    }

    let _ = erase_size; // (used only for the timeout above; keep silent)

    // Send each compressed block.
    let mut written: usize = 0;
    for (seq, chunk) in compressed.chunks(FLASH_WRITE_SIZE).enumerate() {
        let mut hdr = [0u8; 16];
        LittleEndian::write_u32(&mut hdr[0..4], chunk.len() as u32);
        LittleEndian::write_u32(&mut hdr[4..8], seq as u32);
        let mut payload = Vec::with_capacity(16 + chunk.len());
        payload.extend_from_slice(&hdr);
        payload.extend_from_slice(chunk);
        let chk = checksum(chunk) as u32;

        let mut attempts_left = WRITE_BLOCK_ATTEMPTS;
        loop {
            attempts_left -= 1;
            match conn.check_command(
                "write compressed flash block",
                Cmd::FlashDeflData,
                &payload,
                chk,
                0,
                DEFAULT_TIMEOUT,
            ) {
                Ok(_) => break,
                Err(e) if attempts_left > 0 => {
                    debug!(error = %e, "compressed write retry");
                }
                Err(e) => return Err(e),
            }
        }
        written += chunk.len();
        if let Some(pf) = progress.as_deref_mut() {
            pf(written as u64, comp_size as u64);
        }
    }

    // FLASH_DEFL_END: only meaningful on stub (ROM exits the bootloader on FE).
    if conn.stub_running {
        let mut end = [0u8; 4];
        LittleEndian::write_u32(&mut end, 1); // stay in bootloader/stub
        conn.check_command(
            "leave compressed flash mode",
            Cmd::FlashDeflEnd,
            &end,
            0,
            0,
            DEFAULT_TIMEOUT,
        )?;
    }

    // Compute host MD5 over the *uncompressed* data, then have the device
    // hash the same region; compare.
    let mut hasher = Md5::new();
    hasher.update(data);
    let host_md5 = format!("{:x}", hasher.finalize());

    let device_md5 = md5_region(conn, addr, size)?;
    if !host_md5.eq_ignore_ascii_case(&device_md5) {
        return Err(Error::Md5Mismatch {
            addr,
            computed: host_md5,
            device: device_md5,
        });
    }
    Ok(host_md5)
}

/// Uncompressed write path. Sends raw blocks via FLASH_BEGIN / FLASH_DATA /
/// FLASH_END.  ~3x slower over the wire but matches esptool's `--no-compress`.
pub fn write_flash_uncompressed(
    conn: &mut Connection,
    chip: &Chip,
    addr: u32,
    data: &[u8],
    mut progress: Option<ProgressFn>,
) -> Result<String> {
    let size = data.len() as u32;
    if size == 0 {
        return Ok(String::new());
    }

    let num_blocks = (data.len()).div_ceil(FLASH_WRITE_SIZE);
    let erase_size = rounded_erase_size(addr, size);

    let mut params = Vec::with_capacity(20);
    let mut buf = [0u8; 16];
    LittleEndian::write_u32(&mut buf[0..4], erase_size);
    LittleEndian::write_u32(&mut buf[4..8], num_blocks as u32);
    LittleEndian::write_u32(&mut buf[8..12], FLASH_WRITE_SIZE as u32);
    LittleEndian::write_u32(&mut buf[12..16], addr);
    params.extend_from_slice(&buf);
    if conn.stub_running || chip.name != "ESP32" {
        params.extend_from_slice(&[0, 0, 0, 0]);
    }

    let begin_timeout = if conn.stub_running {
        DEFAULT_TIMEOUT
    } else {
        timeout_per_mb(ERASE_REGION_TIMEOUT_PER_MB, erase_size as usize)
    };
    conn.check_command(
        "enter flash mode",
        Cmd::FlashBegin,
        &params,
        0,
        0,
        begin_timeout,
    )?;

    let mut written: usize = 0;
    for (seq, chunk) in data.chunks(FLASH_WRITE_SIZE).enumerate() {
        // The last block may be shorter than FLASH_WRITE_SIZE; pad with 0xFF
        // so the chip's writer sees a full block.
        let padded: Vec<u8> = if chunk.len() < FLASH_WRITE_SIZE {
            let mut v = chunk.to_vec();
            v.resize(FLASH_WRITE_SIZE, 0xFF);
            v
        } else {
            chunk.to_vec()
        };
        let mut hdr = [0u8; 16];
        LittleEndian::write_u32(&mut hdr[0..4], padded.len() as u32);
        LittleEndian::write_u32(&mut hdr[4..8], seq as u32);
        let mut payload = Vec::with_capacity(16 + padded.len());
        payload.extend_from_slice(&hdr);
        payload.extend_from_slice(&padded);
        let chk = checksum(&padded) as u32;

        let mut attempts_left = WRITE_BLOCK_ATTEMPTS;
        loop {
            attempts_left -= 1;
            match conn.check_command(
                "write flash block",
                Cmd::FlashData,
                &payload,
                chk,
                0,
                DEFAULT_TIMEOUT,
            ) {
                Ok(_) => break,
                Err(e) if attempts_left > 0 => {
                    debug!(error = %e, "block write retry");
                }
                Err(e) => return Err(e),
            }
        }
        written += chunk.len();
        if let Some(pf) = progress.as_deref_mut() {
            pf(written as u64, data.len() as u64);
        }
    }

    if conn.stub_running {
        let mut end = [0u8; 4];
        LittleEndian::write_u32(&mut end, 1); // stay in bootloader/stub
        conn.check_command(
            "leave flash mode",
            Cmd::FlashEnd,
            &end,
            0,
            0,
            DEFAULT_TIMEOUT,
        )?;
    }

    let mut hasher = Md5::new();
    hasher.update(data);
    let host_md5 = format!("{:x}", hasher.finalize());

    let device_md5 = md5_region(conn, addr, size)?;
    if !host_md5.eq_ignore_ascii_case(&device_md5) {
        return Err(Error::Md5Mismatch {
            addr,
            computed: host_md5,
            device: device_md5,
        });
    }
    Ok(host_md5)
}

/// Ask the chip to MD5 a flash region. Returns lowercase hex.
pub fn md5_region(conn: &mut Connection, addr: u32, size: u32) -> Result<String> {
    let mut payload = [0u8; 16];
    LittleEndian::write_u32(&mut payload[0..4], addr);
    LittleEndian::write_u32(&mut payload[4..8], size);
    let resp_data_len = if conn.stub_running { 16 } else { 32 };
    let timeout = timeout_per_mb(MD5_TIMEOUT_PER_MB, size as usize);
    let res = conn.check_command(
        "compute MD5",
        Cmd::SpiFlashMd5,
        &payload,
        0,
        resp_data_len,
        timeout,
    )?;
    if conn.stub_running {
        // Stub returns 16 raw bytes; hexify here.
        Ok(hex::encode(&res.data))
    } else {
        // ROM returns 32 ASCII hex bytes.
        Ok(String::from_utf8_lossy(&res.data).to_string())
    }
}

// ---------------------------------------------------------------------------
// Read flash
// ---------------------------------------------------------------------------

/// Read `length` bytes from flash at `addr`, using the stub's streaming
/// READ_FLASH command. ROM read-flash (slow) is intentionally omitted from
/// the v0.1 surface — use the stub.
pub fn read_flash(
    conn: &mut Connection,
    addr: u32,
    length: u32,
    mut progress: Option<ProgressFn>,
) -> Result<Vec<u8>> {
    if !conn.stub_running {
        return Err(Error::Other(
            "read-flash requires the stub loader (don't pass --no-stub)".into(),
        ));
    }

    // Issue the start command. The stub will then push N data frames followed
    // by one 16-byte MD5 frame; we send back an ACK (the running byte total)
    // after each data frame.
    let mut payload = [0u8; 16];
    LittleEndian::write_u32(&mut payload[0..4], addr);
    LittleEndian::write_u32(&mut payload[4..8], length);
    LittleEndian::write_u32(&mut payload[8..12], FLASH_SECTOR_SIZE);
    LittleEndian::write_u32(&mut payload[12..16], 64);
    conn.check_command(
        "read flash",
        Cmd::ReadFlash,
        &payload,
        0,
        0,
        DEFAULT_TIMEOUT,
    )?;

    let mut data = Vec::with_capacity(length as usize);
    let mut decoder = crate::protocol::slip::Decoder::new();
    let mut buf = [0u8; 4096];

    conn.transport.set_timeout(Duration::from_secs(3))?;

    while (data.len() as u32) < length {
        let n = match conn.transport.read(&mut buf) {
            Ok(n) => n,
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => return Err(e),
        };
        for &b in &buf[..n] {
            if let Some(frame) = decoder.push(b)? {
                if (data.len() as u32 + frame.len() as u32) > length {
                    return Err(Error::Other("read more than expected".into()));
                }
                data.extend_from_slice(&frame);

                // ACK back the running byte count (LE u32).
                let mut ack = [0u8; 4];
                LittleEndian::write_u32(&mut ack, data.len() as u32);
                let ack_frame = crate::protocol::slip::encode(&ack);
                conn.transport.write(&ack_frame)?;

                if let Some(pf) = progress.as_deref_mut() {
                    pf(data.len() as u64, length as u64);
                }
            }
        }
    }

    // Final MD5 frame.
    loop {
        let n = match conn.transport.read(&mut buf) {
            Ok(n) => n,
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => return Err(e),
        };
        for &b in &buf[..n] {
            if let Some(frame) = decoder.push(b)? {
                if frame.len() != 16 {
                    return Err(Error::Other(format!(
                        "expected MD5 digest, got {} bytes",
                        frame.len()
                    )));
                }
                let expected = hex::encode(&frame);
                let mut hasher = Md5::new();
                hasher.update(&data);
                let computed = format!("{:x}", hasher.finalize());
                if computed != expected {
                    return Err(Error::Md5Mismatch {
                        addr,
                        computed,
                        device: expected,
                    });
                }
                return Ok(data);
            }
        }
    }
}
