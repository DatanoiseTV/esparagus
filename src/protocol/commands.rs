//! ESP serial-protocol command IDs and packet structures.
//!
//! See esptool's `loader.py::ESPLoader.ESP_CMDS` for the source-of-truth list.
//! Values are stable across chip families; some commands are stub-only.

#![allow(dead_code)]

use crate::error::{Error, Result};
use byteorder::{ByteOrder, LittleEndian};

/// Command opcode byte.  Values match upstream esptool exactly.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Cmd {
    FlashBegin = 0x02,
    FlashData = 0x03,
    FlashEnd = 0x04,
    MemBegin = 0x05,
    MemEnd = 0x06,
    MemData = 0x07,
    Sync = 0x08,
    WriteReg = 0x09,
    ReadReg = 0x0A,
    SpiSetParams = 0x0B,
    SpiAttach = 0x0D,
    ReadFlashSlow = 0x0E,
    ChangeBaudrate = 0x0F,
    FlashDeflBegin = 0x10,
    FlashDeflData = 0x11,
    FlashDeflEnd = 0x12,
    SpiFlashMd5 = 0x13,
    GetSecurityInfo = 0x14,
    // Stub-only commands
    EraseFlash = 0xD0,
    EraseRegion = 0xD1,
    ReadFlash = 0xD2,
    RunUserCode = 0xD3,
}

impl Cmd {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn name(b: u8) -> &'static str {
        match b {
            0x02 => "FLASH_BEGIN",
            0x03 => "FLASH_DATA",
            0x04 => "FLASH_END",
            0x05 => "MEM_BEGIN",
            0x06 => "MEM_END",
            0x07 => "MEM_DATA",
            0x08 => "SYNC",
            0x09 => "WRITE_REG",
            0x0A => "READ_REG",
            0x0B => "SPI_SET_PARAMS",
            0x0D => "SPI_ATTACH",
            0x0E => "READ_FLASH_SLOW",
            0x0F => "CHANGE_BAUDRATE",
            0x10 => "FLASH_DEFL_BEGIN",
            0x11 => "FLASH_DEFL_DATA",
            0x12 => "FLASH_DEFL_END",
            0x13 => "SPI_FLASH_MD5",
            0x14 => "GET_SECURITY_INFO",
            0xD0 => "ERASE_FLASH",
            0xD1 => "ERASE_REGION",
            0xD2 => "READ_FLASH",
            0xD3 => "RUN_USER_CODE",
            _ => "UNKNOWN",
        }
    }
}

/// Initial XOR state for the per-block checksum used in MEM_DATA / FLASH_DATA.
pub const CHECKSUM_INIT: u8 = 0xEF;

/// XOR-fold checksum used by the ROM bootloader for data-block commands.
pub fn checksum(data: &[u8]) -> u8 {
    data.iter().fold(CHECKSUM_INIT, |acc, &b| acc ^ b)
}

/// Status response byte indicating the ROM rejected the command as unknown.
pub const ROM_INVALID_RECV_MSG: u8 = 0x05;

/// Direction byte for outgoing packets ("request").
pub const DIR_REQ: u8 = 0x00;
/// Direction byte for incoming packets ("response").
pub const DIR_RESP: u8 = 0x01;

/// Build a command packet body (pre-SLIP-framing).
///
///   byte  0: direction (0x00 for requests)
///   byte  1: opcode
///   bytes 2..4: little-endian payload length
///   bytes 4..8: little-endian checksum (only meaningful for *_DATA cmds)
///   bytes 8..: payload
pub fn encode_packet(op: u8, payload: &[u8], chk: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.push(DIR_REQ);
    out.push(op);
    let mut len_buf = [0u8; 2];
    LittleEndian::write_u16(&mut len_buf, payload.len() as u16);
    out.extend_from_slice(&len_buf);
    let mut chk_buf = [0u8; 4];
    LittleEndian::write_u32(&mut chk_buf, chk);
    out.extend_from_slice(&chk_buf);
    out.extend_from_slice(payload);
    out
}

#[derive(Debug, Clone)]
pub struct Response {
    pub op: u8,
    pub value: u32,
    pub data: Vec<u8>,
}

/// Parse a response packet body (after SLIP de-framing).
pub fn decode_packet(frame: &[u8]) -> Result<Response> {
    if frame.len() < 8 {
        return Err(Error::Other(format!(
            "response too short ({} bytes)",
            frame.len()
        )));
    }
    if frame[0] != DIR_RESP {
        return Err(Error::Other(format!(
            "expected response direction byte 0x01, got {:#04x}",
            frame[0]
        )));
    }
    let op = frame[1];
    let len = LittleEndian::read_u16(&frame[2..4]) as usize;
    let value = LittleEndian::read_u32(&frame[4..8]);
    let data = frame[8..]
        .get(..len)
        .ok_or_else(|| {
            Error::Other(format!(
                "declared length {} exceeds payload {}",
                len,
                frame.len() - 8
            ))
        })?
        .to_vec();
    Ok(Response { op, value, data })
}

/// The sync packet body: a magic 4-byte sequence followed by 32 0x55s.
/// Matches upstream esptool `sync()`.
pub fn sync_payload() -> Vec<u8> {
    let mut p = vec![0x07, 0x07, 0x12, 0x20];
    p.extend(std::iter::repeat_n(0x55, 32));
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_init_with_empty_data() {
        assert_eq!(checksum(&[]), CHECKSUM_INIT);
    }

    #[test]
    fn checksum_xor_fold() {
        assert_eq!(
            checksum(&[0x01, 0x02, 0x03]),
            CHECKSUM_INIT ^ 0x01 ^ 0x02 ^ 0x03
        );
    }

    #[test]
    fn packet_round_trip() {
        let p = encode_packet(Cmd::ReadReg.as_u8(), &[0x78, 0x00, 0x00, 0x60], 0);
        assert_eq!(p[0], DIR_REQ);
        assert_eq!(p[1], 0x0A);
        assert_eq!(LittleEndian::read_u16(&p[2..4]), 4);
    }

    #[test]
    fn sync_payload_shape() {
        let p = sync_payload();
        assert_eq!(p.len(), 36);
        assert_eq!(&p[..4], &[0x07, 0x07, 0x12, 0x20]);
        assert!(p[4..].iter().all(|&b| b == 0x55));
    }

    #[test]
    fn decode_response_short_errors() {
        let r = decode_packet(&[0x01, 0x08, 0x00, 0x00, 0x00, 0x00]);
        assert!(r.is_err());
    }

    #[test]
    fn decode_response_wrong_dir_errors() {
        let r = decode_packet(&[0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert!(r.is_err());
    }
}
