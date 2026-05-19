//! CLI surface (clap derive).

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug, Clone)]
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

    /// Disable on-the-wire deflate compression for flash writes (FLASH_DATA
    /// instead of FLASH_DEFL_DATA). ~3x slower; useful when debugging stub
    /// or compressed-mode issues. Matches esptool's --no-compress.
    #[arg(long, global = true)]
    pub no_compress: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// List USB serial devices that look like ESP boards (Espressif native
    /// USB or known UART bridges: CP210x, CH34x, FTDI). De-duplicates the
    /// macOS cu./tty. variants and enriches each entry with USB descriptor
    /// strings (manufacturer / product / serial number) via nusb. Does
    /// not open any port, so safe to run without --port.
    #[command(name = "list-ports")]
    ListPorts,

    /// Detect the chip and report its identity + flash JEDEC ID + MAC.
    /// `chip-id` and `chip_id` are accepted as aliases so esptool-style
    /// invocations work without going through the busybox compat layer.
    #[command(alias = "chip-id", alias = "chip_id")]
    Detect,

    /// Read the base MAC address from EFUSE.
    ReadMac,

    /// Read SPI flash JEDEC ID (manufacturer + device).
    FlashId,

    /// Read EFUSE: dump the BLOCK0-1 region as 32-bit words, plus
    /// decoded common fields (BASE_MAC, chip revision). EFUSE *burn*
    /// is intentionally not implemented (one-way fuses; out of scope
    /// for the v1 release — use `espefuse.py` for burning).
    #[command(name = "read-efuse", alias = "efuse-read", alias = "efuse_read")]
    ReadEfuse {
        /// How many 32-bit words to dump starting from EFUSE_BASE.
        /// Default 64 (256 bytes) covers BLOCK0 + BLOCK1 on every
        /// supported chip. The EFUSE peripheral is memory-mapped and
        /// the unused window beyond the implemented blocks reads as 0.
        #[arg(long, default_value_t = 64)]
        words: u32,
    },

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

    /// Read and view / export NVS (Non-Volatile Storage) partition contents.
    /// Read-only in this release; the writer is a deliberate next-session
    /// task because corrupt NVS bytes can make the firmware refuse to
    /// mount the partition.
    Nvs {
        #[command(subcommand)]
        action: NvsAction,
    },

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
    ///
    /// File-level compression: pass an output path ending in .gz for gzip,
    /// or pass --compress gz explicitly. Auto-detection from extension is
    /// the default.
    Backup {
        #[arg(long, short = 'o')]
        output: PathBuf,
        /// Override size (bytes; hex prefix accepted). Default: auto-detect.
        #[arg(long, value_parser = parse_u32)]
        size: Option<u32>,
        /// File-level compression mode.
        #[arg(long, default_value = "auto")]
        compress: FileCompression,
    },

    /// Restore a previously-dumped flash image, starting at 0x0. The input
    /// is auto-decompressed if it ends in .gz or starts with the gzip
    /// magic bytes 0x1F 0x8B.
    Restore {
        /// File to restore. Must be ≤ flash size after decompression.
        input: PathBuf,
    },

    /// Build an ESP firmware image from an ELF file (offline; no chip
    /// needed). Produces a binary that can be flashed at the offset
    /// matching the partition table's app slot.
    #[command(name = "elf2image")]
    Elf2Image {
        /// Input ELF file.
        input: PathBuf,
        /// Output .bin path.
        #[arg(long, short = 'o')]
        output: PathBuf,
        /// Target chip family (required: --chip esp32-s3, etc.).
        #[arg(long, short = 'C')]
        target_chip: String,
        /// Flash mode encoded in the header.
        #[arg(long, default_value = "dio")]
        flash_mode: String,
        /// Flash frequency string ("40m", "80m", etc.; chip-dependent).
        #[arg(long, default_value = "40m")]
        flash_freq: String,
        /// Flash size string ("4MB", "16MB", etc.).
        #[arg(long, default_value = "4MB")]
        flash_size: String,
        /// Minimum required chip revision (major*100+minor).
        #[arg(long, default_value_t = 0)]
        min_rev_full: u16,
        /// Maximum supported chip revision (major*100+minor).
        #[arg(long, default_value_t = 0xFFFF)]
        max_rev_full: u16,
        /// Skip appending the SHA256 digest (legacy bootloaders).
        #[arg(long)]
        no_hash: bool,
    },

    /// Flash one or more files and then drop straight into the serial
    /// monitor — the single-command "flash + check the firmware boots"
    /// flow that LLM/CI loops want. Skips the post-flash reset since the
    /// monitor's own reset_to_app sequence handles it.
    #[command(name = "flash-monitor")]
    FlashMonitor {
        /// Pairs of (address, file). Same syntax as `write-flash`.
        #[arg(required = true, num_args = 2..)]
        args: Vec<String>,
        /// Baud rate to use for the monitor phase. Defaults to
        /// **115200** (ESP-IDF default app-console baud) since the
        /// global `--baud` is the *flashing* rate, which the running
        /// firmware almost never uses.
        #[arg(long, default_value_t = 115_200)]
        monitor_baud: u32,
        /// Monitor: hard ceiling on listen time (0 = forever).
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        /// Monitor: success pattern (regex). Repeatable.
        #[arg(long = "expect")]
        expect: Vec<String>,
        /// Monitor: failure pattern (regex). Repeatable.
        #[arg(long = "expect-not")]
        expect_not: Vec<String>,
        /// Monitor: disable the built-in ESP crash detector.
        #[arg(long)]
        no_crash_detect: bool,
    },

    /// Run as an MCP (Model Context Protocol) server over stdio.
    /// Speaks JSON-RPC 2.0 on stdin/stdout and exposes the esparagus
    /// tool surface to MCP-aware clients (Claude Code/Desktop, Cursor,
    /// Goose, Junie, etc.). Each `tools/call` spawns a fresh child
    /// `esparagus` process so the serial port is opened on-demand and
    /// released after each call — other processes can still use the
    /// port between MCP calls.
    Mcp,

    /// Print a shell-completion script to stdout. Pipe into the
    /// shell's completion-loading directory, e.g.
    ///   esparagus completions zsh > ~/.zfunc/_esparagus
    ///   esparagus completions bash > /usr/local/etc/bash_completion.d/esparagus
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },

    /// Print a roff(7)-formatted man page to stdout. Save as
    /// `esparagus.1` under your manpath, e.g.
    ///   esparagus man > /usr/local/share/man/man1/esparagus.1
    Man,

    /// Open a serial monitor on the chip's UART/USB-CDC and watch the
    /// output for expected/forbidden patterns.  GNU-expect-style: the
    /// command exits 0 on first --expect match, 30 on first --expect-not
    /// match, or 31 on timeout.  Designed to chain after `write-flash`
    /// in a CI / LLM feedback loop.
    Monitor {
        /// Baud rate for the monitor session. Defaults to **115200**
        /// (the ESP-IDF default app-console baud) rather than the
        /// global `--baud`, because the latter is the post-sync
        /// flashing baud, which the running firmware almost never
        /// uses. Override here when your firmware printk()s at a
        /// non-default rate.
        #[arg(long = "monitor-baud", default_value_t = 115_200)]
        monitor_baud: u32,
        /// Hard ceiling on total monitor time. 0 = run forever.
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        /// Success pattern (regex). Repeatable; any match exits 0.
        #[arg(long = "expect")]
        expect: Vec<String>,
        /// Failure pattern (regex). Repeatable; any match exits 30.
        #[arg(long = "expect-not")]
        expect_not: Vec<String>,
        /// Don't hard-reset before listening. By default we pulse EN so
        /// the chip starts its boot output from byte 0.
        #[arg(long)]
        no_reset: bool,
        /// Disable automatic detection of ESP panic / watchdog / assert
        /// output. By default the monitor recognises Guru Meditation,
        /// task watchdog, abort, assert, stack-smashing, and the common
        /// CPU exceptions, captures the surrounding backtrace into a
        /// crash_context event, and exits 32.
        #[arg(long)]
        no_crash_detect: bool,
    },

    /// Merge multiple binaries into one padded image (offline; no chip
    /// needed). Useful for building a complete flash image (bootloader +
    /// partition table + app + ...) for distribution.
    #[command(name = "merge-bin")]
    MergeBin {
        /// Output path (`.bin` for raw, `.uf2` for uf2).
        #[arg(long, short = 'o')]
        output: PathBuf,
        /// Pad output to this size (bytes; hex prefix accepted). Only
        /// applies to `--format raw`; ignored for UF2 (which is a
        /// per-block stream and has no overall size).
        #[arg(long, value_parser = parse_u32)]
        target_size: Option<u32>,
        /// Subtract this offset from each piece's address (so the result
        /// starts at offset 0 of the file). Default 0. Only meaningful
        /// for `--format raw`; ignored for UF2 (each file carries its
        /// own absolute load address).
        #[arg(long, default_value_t = 0, value_parser = parse_u32)]
        target_offset: u32,
        /// Output format: `raw` (concatenated padded .bin — the default,
        /// matches upstream `esptool merge-bin`), or `uf2` (Microsoft
        /// UF2 container, suitable for drag-and-drop bootloaders).
        #[arg(long, value_enum, default_value_t = MergeFormat::Raw)]
        format: MergeFormat,
        /// Target chip name (required for `--format uf2` to select the
        /// UF2 family ID). Ignored for raw.
        #[arg(long)]
        chip: Option<String>,
        /// Omit the per-block MD5 footer (UF2 spec extension). On by
        /// default; clear if your bootloader rejects unknown UF2 flags.
        #[arg(long)]
        no_md5: bool,
        /// Pairs of (address, file). Example:
        ///   0x0 bootloader.bin 0x8000 partitions.bin 0x10000 app.bin
        #[arg(required = true, num_args = 2..)]
        args: Vec<String>,
    },

    /// Run a TOML expect script against the chip's serial port.
    /// Send/expect/branching/captures with crash detection — like GNU
    /// `expect` but with structured NDJSON output and stable exit
    /// codes for CI / LLM agents.  See README and
    /// references/EXPECT_PATTERNS.md for the script grammar.
    Expect {
        /// Path to a TOML script (see module docs for shape).
        script: PathBuf,
        /// Don't pulse EN/DTR before the first step. Useful when the
        /// firmware is already running and you don't want to interrupt
        /// it.
        #[arg(long)]
        no_reset: bool,
        /// Disable the built-in crash detector (panic / wdt / abort /
        /// ...). Off by default — crashes during expect waits abort
        /// the run with exit 42.
        #[arg(long)]
        no_crash_detect: bool,
        /// Validate the script (parse, regex compile, `goto`
        /// resolution, unique step names) and exit without opening a
        /// serial port. Exit 0 if the script is well-formed, 43
        /// otherwise. Useful in pre-commit / CI lint passes.
        #[arg(long)]
        check: bool,
    },
}

#[derive(Copy, Clone, Debug, clap::ValueEnum)]
pub enum MergeFormat {
    /// Concatenated padded .bin matching upstream `esptool merge-bin`.
    Raw,
    /// Microsoft UF2 container (https://github.com/microsoft/uf2).
    Uf2,
}

#[derive(Subcommand, Debug, Clone)]
pub enum NvsAction {
    /// Open an interactive table view of the NVS partition. Press `/` to
    /// filter, `q` or Esc to quit. Defaults to reading the partition
    /// named "nvs" from the chip; pass `--from-file PATH` to inspect a
    /// previously dumped partition bin.
    View {
        /// Partition name on the chip (default: "nvs").
        #[arg(long, default_value = "nvs")]
        name: String,
        /// Inspect a local file (raw partition bytes) instead of reading
        /// from the chip.
        #[arg(long)]
        from_file: Option<PathBuf>,
    },

    /// Read the NVS partition and write its contents to a JSON file.
    /// Useful as the read half of an "export → edit JSON → import"
    /// workflow once the writer lands.
    Export {
        #[arg(long, default_value = "nvs")]
        name: String,
        #[arg(long)]
        from_file: Option<PathBuf>,
        #[arg(long, short = 'o')]
        output: PathBuf,
    },
}

/// File-level compression mode for the `backup` subcommand.
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum FileCompression {
    /// Infer from output extension (.gz → gzip, anything else → none).
    Auto,
    /// Always write gzip.
    Gz,
    /// Always write raw.
    None,
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
    if !args.len().is_multiple_of(2) {
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
