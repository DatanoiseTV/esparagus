//! SLIP framing (RFC 1055) as used by the ESP serial protocol.
//!
//! Framing bytes:
//!   0xC0 — END (also frame delimiter on both sides)
//!   0xDB — ESC
//!   0xDB 0xDC — escaped 0xC0
//!   0xDB 0xDD — escaped 0xDB

use crate::error::{Error, Result};

pub const END: u8 = 0xC0;
pub const ESC: u8 = 0xDB;
pub const ESC_END: u8 = 0xDC;
pub const ESC_ESC: u8 = 0xDD;

/// Encode a packet body into a complete SLIP frame (with delimiters).
pub fn encode(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 8);
    out.push(END);
    for &b in body {
        match b {
            END => {
                out.push(ESC);
                out.push(ESC_END);
            }
            ESC => {
                out.push(ESC);
                out.push(ESC_ESC);
            }
            _ => out.push(b),
        }
    }
    out.push(END);
    out
}

/// Streaming SLIP decoder.
///
/// Feed bytes via `push`; whenever a complete frame is decoded, it is returned
/// and pulled off the internal buffer.  Garbage before the first 0xC0 is
/// rejected (matches upstream esptool behavior — flags it as serial noise).
pub struct Decoder {
    state: State,
    buf: Vec<u8>,
}

#[derive(PartialEq, Eq, Debug)]
enum State {
    AwaitingStart,
    InFrame,
    InEscape,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            state: State::AwaitingStart,
            buf: Vec::with_capacity(256),
        }
    }

    /// Reset the decoder. Call after `flush_input()` on the transport.
    pub fn reset(&mut self) {
        self.state = State::AwaitingStart;
        self.buf.clear();
    }

    /// Push bytes; returns `Some(frame)` for each complete frame.
    ///
    /// Caller should keep feeding bytes until it gets a frame, or transport
    /// times out, or `Err` is returned.
    pub fn push(&mut self, b: u8) -> Result<Option<Vec<u8>>> {
        match self.state {
            State::AwaitingStart => {
                if b == END {
                    self.state = State::InFrame;
                    self.buf.clear();
                    Ok(None)
                } else {
                    // Tolerate garbage before the first 0xC0 (boot logs, etc.)
                    // — upstream esptool only complains *after* a valid frame
                    // has started. We do the same.
                    Ok(None)
                }
            }
            State::InFrame => match b {
                END => {
                    // Two consecutive 0xC0 are common: end of one frame is the
                    // start of nothing; an empty buffer means "frame opener",
                    // a non-empty buffer means "frame closer".
                    if self.buf.is_empty() {
                        Ok(None)
                    } else {
                        let frame = std::mem::take(&mut self.buf);
                        self.state = State::AwaitingStart;
                        Ok(Some(frame))
                    }
                }
                ESC => {
                    self.state = State::InEscape;
                    Ok(None)
                }
                _ => {
                    self.buf.push(b);
                    Ok(None)
                }
            },
            State::InEscape => {
                self.state = State::InFrame;
                match b {
                    ESC_END => {
                        self.buf.push(END);
                        Ok(None)
                    }
                    ESC_ESC => {
                        self.buf.push(ESC);
                        Ok(None)
                    }
                    _ => Err(Error::Slip("invalid escape sequence")),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_no_escapes() {
        let body = b"\x01\x02\x03\x04";
        let encoded = encode(body);
        assert_eq!(encoded[0], END);
        assert_eq!(*encoded.last().unwrap(), END);

        let mut dec = Decoder::new();
        let mut frame = None;
        for &b in &encoded {
            if let Some(f) = dec.push(b).unwrap() {
                frame = Some(f);
            }
        }
        assert_eq!(frame.unwrap(), body);
    }

    #[test]
    fn escape_end_byte() {
        let body = &[0x01, END, 0x02];
        let encoded = encode(body);
        // Expect ESC ESC_END in the middle
        assert!(encoded.windows(2).any(|w| w == [ESC, ESC_END]));

        let mut dec = Decoder::new();
        let mut frame = None;
        for &b in &encoded {
            if let Some(f) = dec.push(b).unwrap() {
                frame = Some(f);
            }
        }
        assert_eq!(frame.unwrap(), body);
    }

    #[test]
    fn escape_esc_byte() {
        let body = &[ESC, 0x42, ESC];
        let encoded = encode(body);
        let mut dec = Decoder::new();
        let mut frame = None;
        for &b in &encoded {
            if let Some(f) = dec.push(b).unwrap() {
                frame = Some(f);
            }
        }
        assert_eq!(frame.unwrap(), body);
    }

    #[test]
    fn ignores_leading_garbage() {
        // Boot-log style ASCII before frame
        let mut bytes = b"ets Jun  8 2016 ".to_vec();
        bytes.extend_from_slice(&encode(b"hello"));

        let mut dec = Decoder::new();
        let mut frame = None;
        for &b in &bytes {
            if let Some(f) = dec.push(b).unwrap() {
                frame = Some(f);
            }
        }
        assert_eq!(frame.as_deref(), Some(&b"hello"[..]));
    }

    #[test]
    fn invalid_escape_errors() {
        let mut dec = Decoder::new();
        for &b in &[END, ESC, 0xFF] {
            let res = dec.push(b);
            if res.is_err() {
                return;
            }
        }
        panic!("expected error on invalid escape");
    }
}
