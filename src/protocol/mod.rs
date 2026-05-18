//! ESP serial-protocol layer.
//!
//! Wraps a `Transport` with SLIP framing, command/response correlation,
//! retries, and timeouts.  Mirrors the behavior of upstream esptool's
//! `ESPLoader.command()` / `check_command()` / `sync()`.

pub mod commands;
pub mod slip;

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use byteorder::{ByteOrder, LittleEndian};
use tracing::trace;

use crate::error::{Error, Result};
use crate::transport::Transport;

pub use commands::{checksum, Cmd, Response};

/// Default per-command timeout (matches esptool's `DEFAULT_TIMEOUT = 3`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);
/// Sync command timeout (matches esptool's `SYNC_TIMEOUT = 0.1`).
pub const SYNC_TIMEOUT: Duration = Duration::from_millis(100);
/// Max single-command timeout (clamps any caller request).
pub const MAX_TIMEOUT: Duration = Duration::from_secs(240);
/// Chip-erase timeout (matches esptool's `CHIP_ERASE_TIMEOUT = 120`).
pub const CHIP_ERASE_TIMEOUT: Duration = Duration::from_secs(120);
/// Default connect attempts.
pub const DEFAULT_CONNECT_ATTEMPTS: u32 = 7;

/// High-level connection to a chip in download mode (ROM or stub).
pub struct Connection {
    pub transport: Box<dyn Transport>,
    decoder: slip::Decoder,
    /// Frames decoded but not yet handed to the caller.  When a single OS
    /// read pulls multiple SLIP frames off the wire (typical with the stub
    /// after MEM_END, where the MEM_END response and OHAI arrive back-to-
    /// back), we queue the extras here instead of dropping bytes.
    pending_frames: VecDeque<Vec<u8>>,
    /// True once `sync()` has been answered by a stub (val==0).
    pub stub_running: bool,
    /// True once a stub has actually been uploaded by us.
    pub stub_uploaded: bool,
    trace_enabled: bool,
}

impl Connection {
    pub fn new(transport: Box<dyn Transport>) -> Self {
        Self {
            transport,
            decoder: slip::Decoder::new(),
            pending_frames: VecDeque::new(),
            stub_running: false,
            stub_uploaded: false,
            trace_enabled: false,
        }
    }

    pub fn set_trace(&mut self, on: bool) {
        self.trace_enabled = on;
    }

    /// Drop any pending bytes and reset the SLIP decoder.
    pub fn flush_input(&mut self) -> Result<()> {
        self.transport.flush_input()?;
        self.decoder.reset();
        self.pending_frames.clear();
        Ok(())
    }

    /// Send a SLIP-framed command packet.
    fn write_packet(&mut self, op: u8, payload: &[u8], chk: u32) -> Result<()> {
        let body = commands::encode_packet(op, payload, chk);
        let frame = slip::encode(&body);
        if self.trace_enabled {
            trace!(
                target: "esparagus::protocol",
                op_name = Cmd::name(op),
                op = format_args!("{:#04x}", op),
                payload_len = payload.len(),
                "TX"
            );
        }
        self.transport.write(&frame)?;
        Ok(())
    }

    /// Read one SLIP frame from the transport, respecting `deadline`.
    ///
    /// Critically, if a single OS read pulls more bytes than one frame's
    /// worth (e.g. MEM_END reply immediately followed by the stub's OHAI),
    /// we feed ALL the bytes through the decoder and queue any extra frames
    /// for the next call.  Dropping bytes mid-stream broke the stub
    /// handshake on real hardware.
    pub fn read_frame(&mut self, deadline: Instant) -> Result<Vec<u8>> {
        if let Some(f) = self.pending_frames.pop_front() {
            if self.trace_enabled {
                trace!(
                    target: "esparagus::protocol",
                    frame_len = f.len(),
                    "RX (queued)"
                );
            }
            return Ok(f);
        }
        let mut buf = [0u8; 256];
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(Error::Other("read timed out".into()));
            }
            let remaining = deadline - now;
            self.transport
                .set_timeout(remaining.min(Duration::from_millis(100)))?;
            let n = match self.transport.read(&mut buf) {
                Ok(n) => n,
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(e) => return Err(e),
            };
            if n == 0 {
                continue;
            }
            let mut first_frame: Option<Vec<u8>> = None;
            for &b in &buf[..n] {
                if let Some(frame) = self.decoder.push(b)? {
                    if first_frame.is_none() {
                        first_frame = Some(frame);
                    } else {
                        self.pending_frames.push_back(frame);
                    }
                }
            }
            if let Some(frame) = first_frame {
                if self.trace_enabled {
                    trace!(
                        target: "esparagus::protocol",
                        frame_len = frame.len(),
                        queued = self.pending_frames.len(),
                        "RX"
                    );
                }
                return Ok(frame);
            }
        }
    }

    /// Read one raw SLIP frame using the connection's own decoder, with a
    /// fresh deadline.  Used by the stub loader for the OHAI handshake.
    pub fn read_raw_frame(&mut self, timeout: Duration) -> Result<Vec<u8>> {
        self.read_frame(Instant::now() + timeout)
    }

    /// Send a command and wait for a matching response.
    /// Mirrors esptool's `ESPLoader.command()` including the up-to-100 retry
    /// loop for stale responses (some ESP8266 ROMs send extra sync replies).
    pub fn command(
        &mut self,
        op: Cmd,
        payload: &[u8],
        chk: u32,
        timeout: Duration,
    ) -> Result<Response> {
        let timeout = timeout.min(MAX_TIMEOUT);
        let deadline = Instant::now() + timeout;
        self.write_packet(op.as_u8(), payload, chk)?;

        for _ in 0..100 {
            let frame = match self.read_frame(deadline) {
                Ok(f) => f,
                Err(Error::Other(_)) => continue, // timeout-on-this-iter, keep trying within deadline
                Err(e) => return Err(e),
            };
            if frame.len() < 8 {
                continue;
            }
            let resp = commands::decode_packet(&frame)?;
            if resp.op == op.as_u8() {
                return Ok(resp);
            }
            // Possible ROM_INVALID_RECV_MSG payload meaning "I don't know that command"
            if resp.data.len() >= 2
                && resp.data[0] != 0
                && resp.data[1] == commands::ROM_INVALID_RECV_MSG
            {
                // Drain the input buffer the way esptool does (best-effort).
                let _ = self.drain_after_unsupported();
                return Err(Error::UnsupportedCommand { op: op.as_u8() });
            }
            // Mismatched op_ret — keep looping like upstream.
        }
        Err(Error::Other("response doesn't match request".into()))
    }

    /// Send a command without waiting for a response (e.g. RUN_USER_CODE).
    pub fn command_no_response(&mut self, op: Cmd, payload: &[u8], chk: u32) -> Result<()> {
        self.write_packet(op.as_u8(), payload, chk)
    }

    fn drain_after_unsupported(&mut self) -> Result<()> {
        // Upstream reads up to 14*8 bytes with a very short timeout. We do
        // the same in spirit: 200ms grace period, then flush.
        let deadline = Instant::now() + Duration::from_millis(200);
        let mut sink = [0u8; 256];
        while Instant::now() < deadline {
            self.transport.set_timeout(Duration::from_millis(10))?;
            match self.transport.read(&mut sink) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        self.flush_input()?;
        Ok(())
    }

    /// Wrap `command()` with the standard status-byte check used by
    /// `check_command()` in upstream esptool.
    pub fn check_command(
        &mut self,
        stage: &str,
        op: Cmd,
        payload: &[u8],
        chk: u32,
        resp_data_len: usize,
        timeout: Duration,
    ) -> Result<CheckResult> {
        let resp = self.command(op, payload, chk, timeout)?;
        // ESP8266 ROM returns 2 status bytes, ESP32+ returns 4 (2 reserved).
        // The convention: status_bytes start at resp_data_len; the first
        // byte is the failure flag, the second is the reason code.
        const STATUS_LEN: usize = 2;
        if resp.data.len() < resp_data_len + STATUS_LEN {
            let status = resp.data.first().copied().unwrap_or(0);
            let reason = resp.data.get(1).copied().unwrap_or(0);
            if status != 0 {
                return Err(Error::CommandFailed {
                    stage: stage.into(),
                    status,
                    reason,
                });
            }
            return Err(Error::Other(format!(
                "{stage}: only got {} bytes of status response",
                resp.data.len()
            )));
        }
        let status = resp.data[resp_data_len];
        let reason = resp.data[resp_data_len + 1];
        if status != 0 {
            return Err(Error::CommandFailed {
                stage: stage.into(),
                status,
                reason,
            });
        }
        Ok(CheckResult {
            value: resp.value,
            data: resp.data[..resp_data_len].to_vec(),
        })
    }

    /// Sync sequence; idempotent. Detects if a stub is already running.
    pub fn sync(&mut self) -> Result<()> {
        let payload = commands::sync_payload();
        let resp = self.command(Cmd::Sync, &payload, 0, SYNC_TIMEOUT)?;
        // ROM bootloaders send a non-zero val response. The stub sends 0.
        self.stub_running = resp.value == 0;
        // Drain the seven extra responses upstream expects.
        for _ in 0..7 {
            let deadline = Instant::now() + SYNC_TIMEOUT;
            if let Ok(frame) = self.read_frame(deadline) {
                if let Ok(r) = commands::decode_packet(&frame) {
                    self.stub_running &= r.value == 0;
                }
            }
        }
        Ok(())
    }

    /// Read a 32-bit register on the target.
    pub fn read_reg(&mut self, addr: u32) -> Result<u32> {
        let mut payload = [0u8; 4];
        LittleEndian::write_u32(&mut payload, addr);
        let resp = self.command(Cmd::ReadReg, &payload, 0, DEFAULT_TIMEOUT)?;
        Ok(resp.value)
    }

    /// Write a 32-bit register on the target.
    pub fn write_reg(&mut self, addr: u32, value: u32, mask: u32, delay_us: u32) -> Result<()> {
        let mut payload = [0u8; 16];
        LittleEndian::write_u32(&mut payload[0..4], addr);
        LittleEndian::write_u32(&mut payload[4..8], value);
        LittleEndian::write_u32(&mut payload[8..12], mask);
        LittleEndian::write_u32(&mut payload[12..16], delay_us);
        self.check_command(
            "write register",
            Cmd::WriteReg,
            &payload,
            0,
            0,
            DEFAULT_TIMEOUT,
        )?;
        Ok(())
    }

    /// Change UART baud rate.  `second_arg` is the old baud (stub uses it to
    /// reset the UART divider correctly); pass 0 for ROM bootloader.
    pub fn change_baud(&mut self, new_baud: u32, second_arg: u32) -> Result<()> {
        let mut payload = [0u8; 8];
        LittleEndian::write_u32(&mut payload[0..4], new_baud);
        LittleEndian::write_u32(&mut payload[4..8], second_arg);
        self.check_command(
            "change baud",
            Cmd::ChangeBaudrate,
            &payload,
            0,
            0,
            DEFAULT_TIMEOUT,
        )?;
        Ok(())
    }
}

/// Result of `check_command`. `value` is the ROM-supplied response value
/// (often a return value or address); `data` is the prefix of the payload
/// (resp_data_len bytes) before the status bytes.
#[derive(Debug, Clone)]
pub struct CheckResult {
    pub value: u32,
    pub data: Vec<u8>,
}
