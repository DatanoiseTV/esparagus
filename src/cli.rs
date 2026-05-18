//! CLI surface (clap derive).

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "esparagus",
    version,
    about = "ESP32 flasher with structured observability for CI/CD and LLM feedback loops",
    long_about = None,
)]
pub struct Cli {
    /// Serial port (e.g. /dev/cu.usbserial-XYZ, COM5). Required for all
    /// device-touching subcommands.
    #[arg(long, short = 'p', global = true, env = "ESPARAGUS_PORT")]
    pub port: Option<String>,

    /// Baud rate after sync. The initial sync always happens at 115200 (the
    /// ROM bootloader's safe rate); once we've synced (and uploaded the stub
    /// if applicable), we upgrade to this rate for the rest of the run.
    #[arg(long, short = 'b', global = true, default_value_t = 460_800)]
    pub baud: u32,

    /// Override chip detection. Accepts "esp32", "esp32-s3", "esp32s3", etc.
    #[arg(long, short = 'c', global = true)]
    pub chip: Option<String>,

    /// How to enter bootloader mode.
    #[arg(long, global = true, default_value = "default-reset")]
    pub before: BeforeMode,

    /// What to do after the operation completes.
    #[arg(long, global = true, default_value = "hard-reset")]
    pub after: AfterMode,

    /// Disable the flasher stub (use the ROM bootloader only). Slower but
    /// useful for debugging stub issues.
    #[arg(long, global = true)]
    pub no_stub: bool,

    /// Number of connect-retry rounds. 0 = retry forever.
    #[arg(long, global = true, default_value_t = 7)]
    pub connect_attempts: u32,

    /// Write NDJSON event stream to stdout (otherwise: human prose on stderr).
    #[arg(long, global = true)]
    pub json: bool,

    /// Mirror every event into this file as NDJSON.
    #[arg(long, global = true)]
    pub log_file: Option<PathBuf>,

    /// Emit a structured final report (JSON) to this path.
    #[arg(long, global = true)]
    pub report: Option<PathBuf>,

    /// Print SLIP-level transmit/receive traces (very verbose).
    #[arg(long, global = true)]
    pub trace: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Detect the chip and report its identity + flash JEDEC ID + MAC.
    Detect,

    /// Read the base MAC address from EFUSE.
    ReadMac,

    /// Read SPI flash JEDEC ID (manufacturer + device).
    FlashId,

    /// Erase the entire flash chip (stub required).
    EraseFlash,

    /// Erase a sector-aligned region of flash (stub required).
    EraseRegion {
        /// Start address (hex prefix accepted, e.g. 0x10000).
        #[arg(value_parser = parse_u32)]
        address: u32,
        /// Size in bytes (hex prefix accepted).
        #[arg(value_parser = parse_u32)]
        size: u32,
    },

    /// Write one or more binaries to flash at the given addresses.
    /// Arguments come in (address, file) pairs.
    WriteFlash {
        /// Pairs of (address, file). Example: 0x0 boot.bin 0x10000 app.bin
        #[arg(required = true, num_args = 2..)]
        args: Vec<String>,
    },

    /// Read a region of flash to a file (stub required).
    ReadFlash {
        #[arg(long, value_parser = parse_u32)]
        address: u32,
        #[arg(long, value_parser = parse_u32)]
        size: u32,
        #[arg(long, short = 'o')]
        output: PathBuf,
    },

    /// Hard-reset the chip (release EN line).
    Reset,

    /// List the partition table from a CSV file or read from the chip's
    /// flash at offset 0x8000.
    Partitions {
        /// Path to a partitions.csv file. If omitted, read the table from
        /// the chip at 0x8000.
        #[arg(long)]
        table: Option<PathBuf>,
    },

    /// Write a file to a partition addressed by name. The partition table is
    /// either supplied via --table or read from the chip's flash.
    WritePartition {
        /// Partition name (e.g. "factory", "ota_0", "nvs").
        #[arg(long)]
        name: String,
        /// Path to a partitions.csv. Omit to read table from flash.
        #[arg(long)]
        table: Option<PathBuf>,
        /// File to write.
        file: PathBuf,
    },

    /// Read an entire partition to a file.
    ReadPartition {
        #[arg(long)]
        name: String,
        #[arg(long)]
        table: Option<PathBuf>,
        #[arg(long, short = 'o')]
        output: PathBuf,
    },

    /// Erase an entire partition.
    ErasePartition {
        #[arg(long)]
        name: String,
        #[arg(long)]
        table: Option<PathBuf>,
    },

    /// Dump the entire flash to a file. Size auto-detected from the SPI
    /// flash JEDEC capacity byte unless --size is provided.
    Backup {
        #[arg(long, short = 'o')]
        output: PathBuf,
        /// Override size (bytes; hex prefix accepted). Default: auto-detect.
        #[arg(long, value_parser = parse_u32)]
        size: Option<u32>,
    },

    /// Restore a previously-dumped flash image, starting at 0x0.
    Restore {
        /// File to restore. Must be ≤ flash size.
        input: PathBuf,
    },
}

#[derive(clap::ValueEnum, Clone, Debug)]
pub enum BeforeMode {
    DefaultReset,
    UsbReset,
    NoReset,
    NoResetNoSync,
}

#[derive(clap::ValueEnum, Clone, Debug)]
pub enum AfterMode {
    HardReset,
    NoReset,
    NoResetStub,
}

/// Parse `0x10000` or `65536`.
pub fn parse_u32(s: &str) -> Result<u32, String> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).map_err(|e| e.to_string())
    } else {
        s.parse::<u32>().map_err(|e| e.to_string())
    }
}

impl From<BeforeMode> for crate::reset::ResetMode {
    fn from(b: BeforeMode) -> Self {
        match b {
            BeforeMode::DefaultReset => crate::reset::ResetMode::Default,
            BeforeMode::UsbReset => crate::reset::ResetMode::UsbReset,
            BeforeMode::NoReset => crate::reset::ResetMode::NoReset,
            BeforeMode::NoResetNoSync => crate::reset::ResetMode::NoResetNoSync,
        }
    }
}

impl From<AfterMode> for crate::reset::AfterMode {
    fn from(a: AfterMode) -> Self {
        match a {
            AfterMode::HardReset => crate::reset::AfterMode::HardReset,
            AfterMode::NoReset => crate::reset::AfterMode::NoReset,
            AfterMode::NoResetStub => crate::reset::AfterMode::NoResetStub,
        }
    }
}

/// Parse the (address, file) repeated pairs from `write-flash`.
pub fn parse_write_pairs(args: &[String]) -> Result<Vec<(u32, PathBuf)>, String> {
    if args.len() % 2 != 0 {
        return Err("write-flash expects pairs of <address> <file>".into());
    }
    let mut out = Vec::with_capacity(args.len() / 2);
    for chunk in args.chunks(2) {
        let addr = parse_u32(&chunk[0])?;
        let path = PathBuf::from(&chunk[1]);
        if !path.is_file() {
            return Err(format!("not a regular file: {}", path.display()));
        }
        out.push((addr, path));
    }
    Ok(out)
}
