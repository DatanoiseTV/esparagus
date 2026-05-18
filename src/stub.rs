//! Flasher stub loader.
//!
//! Uploads the compiled stub firmware (bundled `stubs/<chip>.json`, dual
//! Apache/MIT from esp-flasher-stub) into chip RAM via MEM_BEGIN / MEM_DATA /
//! MEM_END, then jumps to its entry point and waits for the "OHAI" handshake
//! from the running stub.
//!
//! After this completes, the same `Connection` can issue stub-only commands
//! (ERASE_FLASH, ERASE_REGION, READ_FLASH) and stub-faster variants of
//! existing commands.

use std::time::Duration;

use base64::Engine;
use byteorder::{ByteOrder, LittleEndian};
use serde::Deserialize;
use tracing::{debug, info};

use crate::chip::Chip;
use crate::error::{Error, Result};
use crate::protocol::{commands::Cmd, Connection, DEFAULT_TIMEOUT};

/// Max RAM block size, matching upstream `ESP_RAM_BLOCK = 0x1800` (6144B).
pub const ESP_RAM_BLOCK: usize = 0x1800;

/// MEM_END on the ROM bootloader can sometimes time out because the running
/// stub re-initialises the UART before its MEM_END response leaves the FIFO.
/// Upstream esptool uses a short timeout and treats timeout as success.
pub const MEM_END_ROM_TIMEOUT: Duration = Duration::from_millis(50);

/// Decoded stub firmware blob, ready to upload.
#[derive(Debug)]
pub struct StubBlob {
    pub entry: u32,
    pub text: Vec<u8>,
    pub text_start: u32,
    pub data: Vec<u8>,
    pub data_start: u32,
    pub bss_start: u32,
}

#[derive(Deserialize)]
struct StubJson {
    entry: u32,
    text: String,
    text_start: u32,
    data: String,
    data_start: u32,
    bss_start: u32,
}

/// All bundled stubs, embedded at compile time. The keys match either
/// `Chip::stub_blob_name` or, for chips with revision-specific variants,
/// the values returned by `Chip::stub_blob_selector`.
mod blobs {
    pub const ESP32: &str = include_str!("../stubs/esp32.json");
    pub const ESP32S2: &str = include_str!("../stubs/esp32s2.json");
    pub const ESP32S3: &str = include_str!("../stubs/esp32s3.json");
    pub const ESP32C2: &str = include_str!("../stubs/esp32c2.json");
    pub const ESP32C3: &str = include_str!("../stubs/esp32c3.json");
    pub const ESP32C5: &str = include_str!("../stubs/esp32c5.json");
    pub const ESP32C6: &str = include_str!("../stubs/esp32c6.json");
    pub const ESP32H2: &str = include_str!("../stubs/esp32h2.json");
    pub const ESP32P4: &str = include_str!("../stubs/esp32p4.json");
    pub const ESP32P4_REV1: &str = include_str!("../stubs/esp32p4-rev1.json");
}

fn raw_blob(name: &str) -> Result<&'static str> {
    Ok(match name {
        "esp32" => blobs::ESP32,
        "esp32s2" => blobs::ESP32S2,
        "esp32s3" => blobs::ESP32S3,
        "esp32c2" => blobs::ESP32C2,
        "esp32c3" => blobs::ESP32C3,
        "esp32c5" => blobs::ESP32C5,
        "esp32c6" => blobs::ESP32C6,
        "esp32h2" => blobs::ESP32H2,
        "esp32p4" => blobs::ESP32P4,
        "esp32p4-rev1" => blobs::ESP32P4_REV1,
        other => {
            // Leak isn't ideal, but blob names are static strings from the
            // chip registry; nothing dynamic lands here at runtime.
            return Err(Error::NoStubForChip(Box::leak(other.to_string().into_boxed_str())));
        }
    })
}

/// Decode the JSON blob for a given chip into raw text/data segments.
/// `blob_name` is the resolved variant (e.g. "esp32p4-rev1" for early-rev P4
/// silicon); use `Chip::stub_blob_name` if you don't need revision-aware
/// selection.
pub fn load_blob(chip: &Chip, blob_name: &str) -> Result<StubBlob> {
    let raw = raw_blob(blob_name)?;
    let j: StubJson = serde_json::from_str(raw)
        .map_err(|e| Error::StubUpload(format!("bad stub JSON for {}: {e}", chip.name)))?;
    let engine = base64::engine::general_purpose::STANDARD;
    let text = engine
        .decode(&j.text)
        .map_err(|e| Error::StubUpload(format!("base64 text decode: {e}")))?;
    let data = engine
        .decode(&j.data)
        .map_err(|e| Error::StubUpload(format!("base64 data decode: {e}")))?;
    Ok(StubBlob {
        entry: j.entry,
        text,
        text_start: j.text_start,
        data,
        data_start: j.data_start,
        bss_start: j.bss_start,
    })
}

/// Upload one (segment_bytes, load_addr) pair into chip RAM.
fn upload_segment(conn: &mut Connection, segment: &[u8], load_addr: u32) -> Result<()> {
    let length = segment.len();
    let blocks = length.div_ceil(ESP_RAM_BLOCK);
    let mut payload = [0u8; 16];
    LittleEndian::write_u32(&mut payload[0..4], length as u32);
    LittleEndian::write_u32(&mut payload[4..8], blocks as u32);
    LittleEndian::write_u32(&mut payload[8..12], ESP_RAM_BLOCK as u32);
    LittleEndian::write_u32(&mut payload[12..16], load_addr);
    conn.check_command(
        "enter RAM download mode",
        Cmd::MemBegin,
        &payload,
        0,
        0,
        DEFAULT_TIMEOUT,
    )?;
    for seq in 0..blocks {
        let from = seq * ESP_RAM_BLOCK;
        let to = (from + ESP_RAM_BLOCK).min(length);
        let block = &segment[from..to];
        let mut hdr = [0u8; 16];
        LittleEndian::write_u32(&mut hdr[0..4], block.len() as u32);
        LittleEndian::write_u32(&mut hdr[4..8], seq as u32);
        // bytes 8..16 are reserved/zero
        let mut buf = Vec::with_capacity(16 + block.len());
        buf.extend_from_slice(&hdr);
        buf.extend_from_slice(block);
        let chk = crate::protocol::commands::checksum(block) as u32;
        conn.check_command(
            "write to target RAM",
            Cmd::MemData,
            &buf,
            chk,
            0,
            DEFAULT_TIMEOUT,
        )?;
    }
    Ok(())
}

/// Tell the chip we're done with RAM downloads and jump to `entry`.
fn mem_finish(conn: &mut Connection, entry: u32) -> Result<()> {
    let mut payload = [0u8; 8];
    LittleEndian::write_u32(&mut payload[0..4], if entry == 0 { 1 } else { 0 });
    LittleEndian::write_u32(&mut payload[4..8], entry);
    // The running stub may reset the UART before its MEM_END response makes
    // it out of the FIFO. Treat a short timeout as success on ROM.
    match conn.check_command(
        "leave RAM download mode",
        Cmd::MemEnd,
        &payload,
        0,
        0,
        MEM_END_ROM_TIMEOUT,
    ) {
        Ok(_) => Ok(()),
        Err(Error::Other(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Verify the user-supplied flash payload doesn't overlap the stub's
/// resident text/data sections — that would brick the in-RAM stub.
pub fn check_no_overlap(blob: &StubBlob, load_addr: u32, size: u32) -> Result<()> {
    let load_end = load_addr.saturating_add(size);
    let ranges = [
        (
            blob.bss_start.min(blob.data_start),
            blob.data_start + blob.data.len() as u32,
        ),
        (blob.text_start, blob.text_start + blob.text.len() as u32),
    ];
    for (start, end) in ranges {
        if load_addr < end && load_end > start {
            return Err(Error::StubUpload(format!(
                "would overlap stub at {:#010x}-{:#010x}",
                start, end
            )));
        }
    }
    Ok(())
}

/// Upload and run the stub for `chip`. After this returns Ok, the connection
/// is talking to the stub — `conn.stub_running` and `conn.stub_uploaded` are
/// both `true`.
///
/// Idempotent: if the sync detected that a stub is already running, we skip
/// the upload entirely.
pub fn run(conn: &mut Connection, chip: &Chip) -> Result<StubBlob> {
    // Pick the right blob: revision-specific via the chip's selector hook
    // (P4) or just the default name for everyone else.
    let blob_name = match chip.stub_blob_selector {
        Some(selector) => selector(chip, conn)?,
        None => chip.stub_blob_name,
    };
    let blob = load_blob(chip, blob_name)?;

    if conn.stub_running {
        info!(chip = chip.name, blob = blob_name, "stub already running, skipping upload");
        conn.stub_uploaded = true;
        return Ok(blob);
    }

    info!(chip = chip.name, blob = blob_name, "uploading stub flasher");
    upload_segment(conn, &blob.text, blob.text_start)?;
    if !blob.data.is_empty() {
        upload_segment(conn, &blob.data, blob.data_start)?;
    }

    info!(entry = format_args!("{:#010x}", blob.entry), "running stub flasher");
    mem_finish(conn, blob.entry)?;

    // Expect the stub to send "OHAI" (as a raw SLIP frame) once it boots.
    // Use the connection's own decoder so any OHAI bytes that arrived
    // alongside the MEM_END response are honored instead of dropped.
    match conn.read_raw_frame(Duration::from_secs(3)) {
        Ok(frame) => {
            debug!(
                frame_hex = hex::encode(&frame).as_str(),
                "stub handshake frame"
            );
            if frame == b"OHAI" {
                conn.stub_running = true;
                conn.stub_uploaded = true;
                info!("stub running");
                Ok(blob)
            } else {
                Err(Error::StubHandshake {
                    got: hex::encode(&frame),
                })
            }
        }
        Err(_) => Err(Error::StubHandshake {
            got: "(timeout)".into(),
        }),
    }
}
