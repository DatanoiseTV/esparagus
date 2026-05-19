use std::io;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("serial port error: {0}")]
    Serial(#[from] serialport::Error),

    #[error("could not open port {port:?}: {source}")]
    OpenPort {
        port: String,
        #[source]
        source: serialport::Error,
    },

    #[error("port {port:?} is in use by another process ({detail})")]
    PortBusy { port: String, detail: String },

    #[error("invalid SLIP framing: {0}")]
    Slip(&'static str),

    #[error("invalid response (op {expected:#04x} != got {got:#04x})")]
    ResponseMismatch { expected: u8, got: u8 },

    #[error("ROM responded with invalid-message status (op {op:#04x}); command not supported")]
    UnsupportedCommand { op: u8 },

    #[error("{stage}: failed (status={status:#04x}, reason={reason:#04x})")]
    CommandFailed {
        stage: String,
        status: u8,
        reason: u8,
    },

    #[error("sync timed out after {attempts} attempts")]
    SyncTimeout { attempts: u32 },

    #[error("unknown / unsupported chip (magic={magic:#010x}, chip_id={chip_id:?})")]
    UnknownChip { magic: u32, chip_id: Option<u32> },

    #[error("chip mismatch: requested {requested}, found {found}")]
    ChipMismatch { requested: String, found: String },

    #[error("invalid image header at {addr:#x}: {detail}")]
    InvalidImage { addr: u32, detail: String },

    #[error("MD5 verify failed at {addr:#x}: computed {computed}, device {device}")]
    Md5Mismatch {
        addr: u32,
        computed: String,
        device: String,
    },

    #[error("stub upload failed: {0}")]
    StubUpload(String),

    #[error("stub handshake failed (expected OHAI 0x4F48414900000000, got {got})")]
    StubHandshake { got: String },

    #[error("no stub blob bundled for {0}")]
    NoStubForChip(&'static str),

    #[error("{0}")]
    Other(String),
}

impl Error {
    /// A short stable identifier used in NDJSON events and reports so an LLM
    /// can match on `class` without parsing English.
    pub fn class(&self) -> &'static str {
        match self {
            Error::Io(_) => "io",
            Error::Serial(_) | Error::OpenPort { .. } => "port",
            Error::PortBusy { .. } => "port_busy",
            Error::Slip(_) => "slip",
            Error::ResponseMismatch { .. } => "response_mismatch",
            Error::UnsupportedCommand { .. } => "unsupported_command",
            Error::CommandFailed { .. } => "command_failed",
            Error::SyncTimeout { .. } => "sync_timeout",
            Error::UnknownChip { .. } => "unknown_chip",
            Error::ChipMismatch { .. } => "chip_mismatch",
            Error::InvalidImage { .. } => "invalid_image",
            Error::Md5Mismatch { .. } => "md5_mismatch",
            Error::StubUpload(_) => "stub_upload",
            Error::StubHandshake { .. } => "stub_handshake",
            Error::NoStubForChip(_) => "no_stub_for_chip",
            Error::Other(_) => "other",
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
