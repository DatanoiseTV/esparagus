//! Orchestrates a single CLI run.
//!
//! 1. Parses CLI.
//! 2. Opens the transport and sets up the observability stack.
//! 3. Runs the reset/sync sequence to put the chip in download mode.
//! 4. Detects the chip (or honors --chip).
//! 5. Optionally uploads the stub flasher.
//! 6. Dispatches to the requested operation.
//! 7. Hard-resets the chip if requested.
//! 8. Writes the final report.

use std::path::Path;
use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressStyle};

use crate::chip::Chip;
use crate::cli::FileCompression;
use crate::cli::{Cli, Command};
use crate::error::{Error, Result};
use crate::monitor as monitor_mod;
use crate::observe::{Emitter, Event, Report, ReportBuilder, ReportTransport};
use crate::partition::{
    PartitionEntry, PartitionTable, PARTITION_TABLE_OFFSET, PARTITION_TABLE_SECTOR,
};
use crate::protocol::Connection;
use crate::reset::{strategy_sequence, AfterMode, ResetMode};
use crate::transport::serial::SerialTransport;
use crate::transport::Transport;
use crate::{chip, image, ops, reset, stub};

/// ROM bootloader's auto-baud target — every flow opens at this rate, syncs,
/// and only then upgrades to the user-requested baud.
const SYNC_BAUD: u32 = 115_200;

pub struct Runtime {
    pub emitter: Emitter,
    pub report: ReportBuilder,
}

/// Top-level entrypoint used by main.rs. Returns the appropriate process
/// exit code; the report has already been written.
pub fn run(cli: Cli) -> i32 {
    // Offline subcommands (file-only) don't need a serial port at all.
    if let Some(code) = run_offline_if_applicable(&cli) {
        return code;
    }
    // `list-ports` is also offline — it walks the OS / USB lists.
    if matches!(cli.command, Command::ListPorts) {
        return handle_list_ports(cli.json);
    }
    // `mcp` runs the MCP server on stdio; it never opens a port itself,
    // it spawns child esparagus processes on demand.
    if matches!(cli.command, Command::Mcp) {
        return crate::mcp::run();
    }
    // Required port for everything but `--help` and `--version`, which clap
    // handles itself. If --port is missing AND exactly one ESP-like
    // candidate is found on the system, auto-select it; otherwise list the
    // candidates and exit 2 so the user can pick.
    let port = match &cli.port {
        Some(p) => p.clone(),
        None => match crate::discover::auto_select_port() {
            Ok(d) => {
                eprintln!("auto-selected port {} ({})", d.path, d.bridge_human,);
                d.path
            }
            Err(msg) => {
                eprintln!("error: --port not given and {}", msg);
                return 2;
            }
        },
    };

    // Monitor needs the port but bypasses the entire sync/detect/stub flow
    // (we explicitly do NOT want to enter the ROM bootloader — we want the
    // chip running its firmware).
    if let Command::Monitor {
        timeout,
        expect,
        expect_not,
        no_reset,
        no_crash_detect,
    } = &cli.command
    {
        return run_monitor(
            &port,
            cli.baud,
            *timeout,
            expect,
            expect_not,
            *no_reset,
            *no_crash_detect,
            cli.json,
            cli.log_file.as_deref(),
        );
    }

    // flash-monitor: do the write-flash, then drop straight into the
    // monitor at --monitor-baud. We synthesize a regular WriteFlash CLI
    // for phase 1 so the existing chip-flow handles it; then if phase 1
    // succeeded, hand off to run_monitor for phase 2.
    if let Command::FlashMonitor {
        args,
        monitor_baud,
        timeout,
        expect,
        expect_not,
        no_crash_detect,
    } = &cli.command
    {
        let mut phase1 = cli.clone();
        phase1.command = Command::WriteFlash { args: args.clone() };
        // Skip the post-flash reset; reset_to_app inside the monitor will
        // bounce the chip into app firmware cleanly.
        phase1.after = crate::cli::AfterMode::NoReset;
        let flash_code = run_chip_flow(phase1, &port);
        if flash_code != 0 {
            return flash_code;
        }
        // Give the OS a moment to release the port (some macOS / Linux
        // bridges hold the file descriptor briefly after close).
        std::thread::sleep(std::time::Duration::from_millis(150));
        let mb = monitor_baud.unwrap_or(cli.baud);
        return run_monitor(
            &port,
            mb,
            *timeout,
            expect,
            expect_not,
            false,
            *no_crash_detect,
            cli.json,
            cli.log_file.as_deref(),
        );
    }

    run_chip_flow(cli, &port)
}

/// Body of the chip-touching path — extracted from `run()` so it can be
/// invoked twice (once for the write-flash phase of `flash-monitor`, once
/// for any of the regular subcommands).
fn run_chip_flow(cli: Cli, port: &str) -> i32 {
    let baud = cli.baud;

    let emitter = match Emitter::new(cli.json, cli.log_file.as_deref()) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: could not open --log-file: {}", e);
            return 2;
        }
    };

    let tool = format!("esparagus {}", env!("CARGO_PKG_VERSION"));
    let mut report = ReportBuilder::new(
        tool.clone(),
        ReportTransport {
            port: port.to_string(),
            baud,
        },
    );

    emitter.info(Event::RunStart {
        tool: tool.clone(),
        chip_arg: cli.chip.clone(),
        port: port.to_string(),
        baud,
    });

    let ok = match run_inner(&cli, port, baud, &emitter, &mut report) {
        Ok(()) => true,
        Err(e) => {
            emitter.error(Event::Error {
                stage: current_stage_name(&cli),
                class: e.class().into(),
                detail: e.to_string(),
            });
            report.record_error(current_stage_name(&cli), &e);
            false
        }
    };

    let duration = report.stages.iter().map(|s| s.ms).sum::<u128>();
    emitter.info(Event::RunComplete {
        ok,
        duration_ms: duration,
    });

    let final_report = report.build(ok);
    if let Some(p) = cli.report.as_deref() {
        if let Err(e) = ReportBuilder::write_to(&final_report, p) {
            eprintln!("warn: failed to write --report: {}", e);
        }
    }
    if ok {
        0
    } else {
        exit_code_for(&final_report)
    }
}

#[allow(clippy::too_many_arguments)]
fn run_monitor(
    port: &str,
    baud: u32,
    timeout_secs: u64,
    expect: &[String],
    expect_not: &[String],
    no_reset: bool,
    no_crash_detect: bool,
    json: bool,
    log_file: Option<&Path>,
) -> i32 {
    let emitter = match Emitter::new(json, log_file) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: could not open --log-file: {}", e);
            return 2;
        }
    };
    // Human mode = print decoded serial lines directly to stdout.
    let print_raw = !json;
    match monitor_mod::run(
        port,
        baud,
        std::time::Duration::from_secs(timeout_secs),
        expect,
        expect_not,
        no_reset,
        no_crash_detect,
        &emitter,
        print_raw,
    ) {
        Ok(monitor_mod::Outcome::ExpectMatch) => 0,
        Ok(monitor_mod::Outcome::ExpectNotMatch) => 30,
        Ok(monitor_mod::Outcome::Timeout) => {
            // With no patterns the timeout is the natural exit and not a
            // failure — match a `cat /dev/tty` mental model.
            if expect.is_empty() && expect_not.is_empty() {
                0
            } else {
                31
            }
        }
        Ok(monitor_mod::Outcome::Crash) => 32,
        Err(e) => {
            emitter.error(Event::Error {
                stage: "monitor".into(),
                class: e.class().into(),
                detail: e.to_string(),
            });
            match e.class() {
                "port" => 10,
                _ => 1,
            }
        }
    }
}

fn handle_list_ports(json: bool) -> i32 {
    let cands = crate::discover::list_esp_candidates();
    if json {
        // One NDJSON event per discovered port — same shape an agent would
        // see if it ran a chip-touching command, so the discovery output is
        // greppable / parseable with the same machinery.
        for c in &cands {
            let line = serde_json::json!({
                "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "level": "info",
                "event": "discovered_port",
                "path": c.path,
                "vid": c.vid,
                "pid": c.pid,
                "manufacturer": c.manufacturer,
                "product": c.product,
                "serial_number": c.serial_number,
                "bridge": c.bridge,
                "bridge_human": c.bridge_human,
            });
            println!("{}", line);
        }
    } else if cands.is_empty() {
        eprintln!("no ESP-like USB serial devices found.");
    } else {
        println!("{} ESP-like device(s) found:", cands.len());
        for c in &cands {
            println!();
            println!("  path           {}", c.path);
            println!("  vid:pid        {}:{}  ({})", c.vid, c.pid, c.bridge_human);
            if let Some(m) = &c.manufacturer {
                println!("  manufacturer   {}", m);
            }
            if let Some(p) = &c.product {
                println!("  product        {}", p);
            }
            if let Some(s) = &c.serial_number {
                println!("  serial         {}", s);
            }
        }
    }
    0
}

/// Dispatch the file-only subcommands. Returns `Some(exit_code)` if it
/// handled the command, `None` if the command needs a chip connection.
fn run_offline_if_applicable(cli: &Cli) -> Option<i32> {
    match &cli.command {
        Command::Elf2Image {
            input,
            output,
            target_chip,
            flash_mode,
            flash_freq,
            flash_size,
            min_rev_full,
            max_rev_full,
            no_hash,
        } => Some(handle_elf2image(
            input,
            output,
            target_chip,
            flash_mode,
            flash_freq,
            flash_size,
            *min_rev_full,
            *max_rev_full,
            *no_hash,
        )),
        Command::MergeBin {
            output,
            target_size,
            target_offset,
            args,
        } => Some(handle_merge_bin(output, *target_size, *target_offset, args)),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_elf2image(
    input: &Path,
    output: &Path,
    target_chip: &str,
    flash_mode: &str,
    flash_freq: &str,
    flash_size: &str,
    min_rev_full: u16,
    max_rev_full: u16,
    no_hash: bool,
) -> i32 {
    let chip = match chip::by_name(target_chip) {
        Some(c) => c,
        None => {
            eprintln!(
                "error: unknown chip {:?} (supported: {:?})",
                target_chip,
                chip::names()
            );
            return 2;
        }
    };
    let mode = match crate::imagegen::encode_flash_mode(flash_mode) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {}", e);
            return 2;
        }
    };
    let freq = match crate::imagegen::encode_flash_freq(chip, flash_freq) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {}", e);
            return 2;
        }
    };
    let size = match crate::imagegen::encode_flash_size(flash_size) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {}", e);
            return 2;
        }
    };
    let params = crate::imagegen::ImageParams {
        flash_mode: mode,
        flash_freq: freq,
        flash_size: size,
        min_rev: (min_rev_full / 100) as u8,
        min_rev_full,
        max_rev_full,
        hash_append: !no_hash,
    };
    let elf = match std::fs::read(input) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: read {}: {}", input.display(), e);
            return 1;
        }
    };
    let img = match crate::imagegen::elf_to_image(&elf, &params, chip) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("error: {}", e);
            return 20;
        }
    };
    if let Err(e) = std::fs::write(output, &img) {
        eprintln!("error: write {}: {}", output.display(), e);
        return 1;
    }
    eprintln!("wrote {} ({} bytes)", output.display(), img.len());
    0
}

fn handle_merge_bin(
    output: &Path,
    target_size: Option<u32>,
    target_offset: u32,
    args: &[String],
) -> i32 {
    let pairs = match crate::cli::parse_write_pairs(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {}", e);
            return 2;
        }
    };
    let mut loaded: Vec<(u32, Vec<u8>)> = Vec::with_capacity(pairs.len());
    for (addr, path) in &pairs {
        match std::fs::read(path) {
            Ok(b) => loaded.push((*addr, b)),
            Err(e) => {
                eprintln!("error: read {}: {}", path.display(), e);
                return 1;
            }
        }
    }
    let merged = match crate::imagegen::merge_bin(&loaded, target_offset, target_size) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };
    if let Err(e) = std::fs::write(output, &merged) {
        eprintln!("error: write {}: {}", output.display(), e);
        return 1;
    }
    eprintln!("wrote {} ({} bytes)", output.display(), merged.len());
    0
}

fn current_stage_name(cli: &Cli) -> String {
    match &cli.command {
        Command::Detect => "detect",
        Command::ReadMac => "read_mac",
        Command::FlashId => "flash_id",
        Command::EraseFlash => "erase_flash",
        Command::EraseRegion { .. } => "erase_region",
        Command::WriteFlash { .. } => "write_flash",
        Command::ReadFlash { .. } => "read_flash",
        Command::Reset => "reset",
        Command::Partitions { .. } => "partitions",
        Command::WritePartition { .. } => "write_partition",
        Command::ReadPartition { .. } => "read_partition",
        Command::ErasePartition { .. } => "erase_partition",
        Command::Backup { .. } => "backup",
        Command::Restore { .. } => "restore",
        Command::Nvs { .. } => "nvs",
        // Offline commands handled before we get here.
        Command::Elf2Image { .. } => "elf2image",
        Command::MergeBin { .. } => "merge_bin",
        Command::Monitor { .. } => "monitor",
        Command::FlashMonitor { .. } => "flash_monitor",
        Command::ListPorts => "list_ports",
        Command::Mcp => "mcp",
    }
    .into()
}

fn exit_code_for(report: &Report) -> i32 {
    let class = report
        .errors
        .first()
        .map(|e| e.class.as_str())
        .unwrap_or("");
    match class {
        "port" => 10,
        "sync_timeout" => 11,
        "chip_mismatch" => 12,
        "md5_mismatch" | "command_failed" => 13,
        "stub_handshake" | "stub_upload" => 14,
        "port_busy" => 15,
        "invalid_image" => 20,
        _ => 1,
    }
}

fn run_inner(
    cli: &Cli,
    port: &str,
    baud: u32,
    emitter: &Emitter,
    report: &mut ReportBuilder,
) -> Result<()> {
    // --- Open port at the ROM safe rate; we'll upgrade after sync ---
    let initial_baud = SYNC_BAUD.min(baud);
    let transport = SerialTransport::open(port, initial_baud)?;
    let vid_pid = transport.usb_vid().zip(transport.usb_pid());
    emitter.info(Event::TransportInfo {
        port: port.to_string(),
        usb_vid: transport.usb_vid().map(|v| format!("{:#06x}", v)),
        usb_pid: transport.usb_pid().map(|v| format!("{:#06x}", v)),
    });
    let mut conn = Connection::new(Box::new(transport));
    conn.set_trace(cli.trace);

    // --- Reset + sync ---
    let reset_mode: ResetMode = cli.before.clone().into();
    let attempts_max = if cli.connect_attempts == 0 {
        u32::MAX
    } else {
        cli.connect_attempts.max(1)
    };
    let strategies = strategy_sequence(reset_mode, vid_pid);

    let mut connect_guard = report.start_stage("connect");
    let mut last_err: Option<Error> = None;
    let mut connected_strategy: Option<String> = None;

    for attempt in 0..attempts_max {
        let strategy = &strategies[(attempt as usize) % strategies.len()];
        emitter.info(Event::ConnectAttempt {
            strategy: strategy.name().into(),
            attempt: attempt + 1,
        });

        // Reset (or no-op for no-reset modes).
        conn.transport.flush_input()?;
        strategy.apply(&mut *conn.transport)?;

        // Sync. For no-reset-no-sync mode we skip and assume.
        if !matches!(reset_mode, ResetMode::NoResetNoSync) {
            let mut sync_ok = false;
            for _ in 0..5 {
                conn.flush_input()?;
                conn.transport.flush_output()?;
                match conn.sync() {
                    Ok(_) => {
                        sync_ok = true;
                        break;
                    }
                    Err(e) => {
                        last_err = Some(e);
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
            if !sync_ok {
                connect_guard.bump_attempt();
                // After the first whole reset+sync attempt fails, surface
                // an actionable hint up-front instead of making the user
                // wait ~25s for the full retry budget to drain.  Tailor
                // the wording to the connection type — on native USB-
                // Serial/JTAG, firmware that has grabbed the USB
                // peripheral is the overwhelmingly common cause.
                if attempt == 0 {
                    let on_native_usb = matches!(
                        vid_pid,
                        Some((reset::ESPRESSIF_VID, reset::USB_JTAG_SERIAL_PID))
                    );
                    let msg = if on_native_usb {
                        "chip is not entering ROM bootloader. \
                         If firmware on the chip has grabbed the USB-Serial/JTAG \
                         peripheral, the host-side reset can't pull it back. \
                         To recover: hold BOOT, tap RESET, release BOOT, then retry."
                            .to_string()
                    } else {
                        "chip is not responding to sync. \
                         To force download mode: hold BOOT, tap RESET, release BOOT, \
                         then retry. Or try --baud 115200."
                            .to_string()
                    };
                    emitter.warn(Event::Warning { message: msg });
                }
                continue;
            }
        }
        connected_strategy = Some(strategy.name().into());
        last_err = None;
        break;
    }

    if connected_strategy.is_none() {
        let err = last_err.unwrap_or(Error::SyncTimeout {
            attempts: attempts_max,
        });
        let stage = report.finish_stage(connect_guard, false, Some(err.to_string()));
        let _ = stage;
        return Err(err);
    }
    let stage = report.finish_stage(connect_guard, true, connected_strategy.clone());
    emitter.info(Event::Connected {
        strategy: connected_strategy.unwrap(),
        attempts: stage.attempts,
    });

    // --- Identify chip ---
    let detect_guard = report.start_stage("detect");
    let chip: &'static Chip = chip::detect(&mut conn)?;
    report.chip = Some(chip.name.into());
    emitter.info(Event::ChipDetected {
        chip: chip.name.into(),
        chip_id: chip.image_chip_id,
    });
    report.finish_stage(detect_guard, true, None);

    // Verify against --chip if supplied.
    if let Some(requested) = cli.chip.as_deref() {
        match chip::by_name(requested) {
            Some(c) if c.name == chip.name => {}
            Some(c) => {
                return Err(Error::ChipMismatch {
                    requested: c.name.into(),
                    found: chip.name.into(),
                })
            }
            None => {
                return Err(Error::Other(format!(
                    "unknown --chip value '{}'; supported: {:?}",
                    requested,
                    chip::names()
                )))
            }
        }
    }

    // --- Optionally upload + run the stub ---
    let mut current_baud = initial_baud;
    if !cli.no_stub && should_use_stub(&cli.command) {
        let stub_guard = report.start_stage("stub_upload");
        let blob_name = match chip.stub_blob_selector {
            Some(selector) => selector(chip, &mut conn)?,
            None => chip.stub_blob_name,
        };
        emitter.info(Event::StubUploadStart {
            chip: chip.name.into(),
            blob: blob_name.into(),
        });
        match stub::run(&mut conn, chip) {
            Ok(blob) => {
                emitter.info(Event::StubRunning {
                    chip: chip.name.into(),
                    blob: blob_name.into(),
                    entry: format!("{:#010x}", blob.entry),
                });
                report.finish_stage(stub_guard, true, None);
            }
            Err(e) => {
                report.finish_stage(stub_guard, false, Some(e.to_string()));
                return Err(e);
            }
        }
    }

    // --- Upgrade baud rate now that we're past sync / stub upload ---
    if baud > current_baud {
        // change_baud's `second_arg` is the previous baud when talking to the
        // stub (so it can reset the UART divider correctly); the ROM expects 0.
        let second_arg = if conn.stub_running { current_baud } else { 0 };
        conn.change_baud(baud, second_arg)?;
        conn.transport.set_baud(baud)?;
        // Allow the chip's UART to finish reconfiguring.
        std::thread::sleep(Duration::from_millis(50));
        conn.flush_input()?;
        emitter.info(Event::BaudUpgrade {
            from: current_baud,
            to: baud,
        });
        current_baud = baud;
    }
    let _ = current_baud;

    // --- Run the operation ---
    match &cli.command {
        Command::Detect => {
            let mac = ops::read_mac(&mut conn, chip)?;
            let id = ops::flash_id(&mut conn, chip)?;
            let mfr = (id & 0xFF) as u8;
            let dev = ops::flash_dev_id(id);
            let size_mb = ops::flash_size_mb_from_id(id);
            emitter.info(Event::FlashIdRead {
                manufacturer: format!("{:#04x}", mfr),
                device: format!("{:#06x}", dev),
                size_mb,
            });
            emitter.info(Event::MacRead {
                mac: ops::format_mac(&mac),
            });
        }
        Command::ReadMac => {
            let mac = ops::read_mac(&mut conn, chip)?;
            emitter.info(Event::MacRead {
                mac: ops::format_mac(&mac),
            });
        }
        Command::FlashId => {
            let id = ops::flash_id(&mut conn, chip)?;
            let mfr = (id & 0xFF) as u8;
            let dev = ops::flash_dev_id(id);
            emitter.info(Event::FlashIdRead {
                manufacturer: format!("{:#04x}", mfr),
                device: format!("{:#06x}", dev),
                size_mb: ops::flash_size_mb_from_id(id),
            });
        }
        Command::EraseFlash => {
            let g = report.start_stage("erase_flash");
            let start = Instant::now();
            emitter.info(Event::EraseBegin {
                addr: "0x0".into(),
                size: 0,
            });
            ops::erase_flash(&mut conn)?;
            emitter.info(Event::EraseDone {
                addr: "0x0".into(),
                size: 0,
                ms: start.elapsed().as_millis(),
            });
            report.finish_stage(g, true, None);
        }
        Command::EraseRegion { address, size } => {
            let g = report.start_stage("erase_region");
            let start = Instant::now();
            emitter.info(Event::EraseBegin {
                addr: format!("{:#010x}", address),
                size: *size as u64,
            });
            ops::erase_region(&mut conn, *address, *size)?;
            emitter.info(Event::EraseDone {
                addr: format!("{:#010x}", address),
                size: *size as u64,
                ms: start.elapsed().as_millis(),
            });
            report.finish_stage(g, true, None);
        }
        Command::WriteFlash { args } => {
            let pairs = crate::cli::parse_write_pairs(args).map_err(Error::Other)?;
            // Configure SPI before any write.
            ops::flash_spi_attach(&mut conn, 0)?;
            for (addr, path) in &pairs {
                write_one(
                    *addr,
                    path,
                    &mut conn,
                    chip,
                    emitter,
                    report,
                    cli.json,
                    !cli.no_compress,
                )?;
            }
        }
        Command::ReadFlash {
            address,
            size,
            output,
        } => {
            let g = report.start_stage("read_flash");
            emitter.info(Event::ReadBegin {
                addr: format!("{:#010x}", address),
                size: *size as u64,
            });
            let bar = make_bar(*size as u64, cli.json);
            let data = {
                let mut progress = |w: u64, _t: u64| {
                    if let Some(b) = bar.as_ref() {
                        b.set_position(w);
                    }
                };
                ops::read_flash(&mut conn, *address, *size, Some(&mut progress))?
            };
            if let Some(b) = bar.as_ref() {
                b.finish_and_clear();
            }
            std::fs::write(output, &data)?;
            let md5 = {
                use md5::{Digest, Md5};
                let mut h = Md5::new();
                h.update(&data);
                format!("{:x}", h.finalize())
            };
            emitter.info(Event::ReadDone {
                addr: format!("{:#010x}", address),
                size: data.len() as u64,
                md5: md5.clone(),
            });
            let mut stage = report.finish_stage(g, true, None);
            stage.bytes = Some(data.len() as u64);
            stage.md5 = Some(md5);
            *report.stages.last_mut().unwrap() = stage;
        }
        Command::Reset => {
            // Handled by the after-mode below.
        }

        Command::Nvs { action } => match action {
            crate::cli::NvsAction::View { name, from_file } => {
                let (bytes, source) = nvs_load_bytes(&mut conn, name, from_file.as_deref())?;
                let partition = crate::nvs::parse(&bytes)?;
                emitter.info(Event::PartitionResolved {
                    name: format!("nvs:{}", name),
                    ptype: "data".into(),
                    subtype: "nvs".into(),
                    offset: source.clone(),
                    size: bytes.len() as u64,
                });
                // Run TUI synchronously. It restores the terminal in its
                // own RAII guard on return / panic.
                crate::tui::run_nvs_view(&partition, &source)?;
            }
            crate::cli::NvsAction::Export {
                name,
                from_file,
                output,
            } => {
                let (bytes, _source) = nvs_load_bytes(&mut conn, name, from_file.as_deref())?;
                let partition = crate::nvs::parse(&bytes)?;
                let json = serde_json::to_string_pretty(&partition)
                    .map_err(|e| Error::Other(format!("serialize nvs: {e}")))?;
                std::fs::write(output, json)?;
                emitter.info(Event::Warning {
                    message: format!(
                        "exported {} NVS items ({} bytes) to {}",
                        partition.items.len(),
                        bytes.len(),
                        output.display()
                    ),
                });
            }
        },

        Command::Partitions { table } => {
            let pt = load_partition_table(&mut conn, table.as_deref(), emitter)?;
            // Emit per-partition resolved events so an LLM or CI script can
            // round-trip the table without re-parsing.
            for p in &pt.entries {
                emitter.info(Event::PartitionResolved {
                    name: p.name.clone(),
                    ptype: p.type_name().into(),
                    subtype: p.subtype_name(),
                    offset: format!("{:#010x}", p.offset),
                    size: p.size as u64,
                });
            }
        }

        Command::WritePartition { name, table, file } => {
            let pt = load_partition_table(&mut conn, table.as_deref(), emitter)?;
            let entry = pt
                .find(name)
                .ok_or_else(|| Error::Other(format!("no partition named {:?} in table", name)))?;
            emit_resolved(emitter, entry);
            let (bytes, _hdr) = image::load_payload(file)?;
            if bytes.len() as u32 > entry.size {
                return Err(Error::Other(format!(
                    "image is {} bytes, partition {:?} is only {} bytes",
                    bytes.len(),
                    name,
                    entry.size
                )));
            }
            ops::flash_spi_attach(&mut conn, 0)?;
            write_payload(
                emitter,
                report,
                &mut conn,
                chip,
                entry.offset,
                &bytes,
                cli.json,
                !cli.no_compress,
            )?;
        }

        Command::ReadPartition {
            name,
            table,
            output,
        } => {
            let pt = load_partition_table(&mut conn, table.as_deref(), emitter)?;
            let entry = pt
                .find(name)
                .ok_or_else(|| Error::Other(format!("no partition named {:?} in table", name)))?;
            emit_resolved(emitter, entry);
            let g = report.start_stage(format!("read_partition {}", entry.name));
            emitter.info(Event::ReadBegin {
                addr: format!("{:#010x}", entry.offset),
                size: entry.size as u64,
            });
            let bar = make_bar(entry.size as u64, cli.json);
            let data = {
                let mut progress = |w: u64, _t: u64| {
                    if let Some(b) = bar.as_ref() {
                        b.set_position(w);
                    }
                };
                ops::read_flash(&mut conn, entry.offset, entry.size, Some(&mut progress))?
            };
            if let Some(b) = bar.as_ref() {
                b.finish_and_clear();
            }
            std::fs::write(output, &data)?;
            let md5 = md5_hex(&data);
            emitter.info(Event::ReadDone {
                addr: format!("{:#010x}", entry.offset),
                size: data.len() as u64,
                md5: md5.clone(),
            });
            let mut stage = report.finish_stage(g, true, None);
            stage.bytes = Some(data.len() as u64);
            stage.md5 = Some(md5);
            *report.stages.last_mut().unwrap() = stage;
        }

        Command::ErasePartition { name, table } => {
            let pt = load_partition_table(&mut conn, table.as_deref(), emitter)?;
            let entry = pt
                .find(name)
                .ok_or_else(|| Error::Other(format!("no partition named {:?} in table", name)))?;
            emit_resolved(emitter, entry);
            let g = report.start_stage(format!("erase_partition {}", entry.name));
            emitter.info(Event::EraseBegin {
                addr: format!("{:#010x}", entry.offset),
                size: entry.size as u64,
            });
            let start = Instant::now();
            ops::erase_region(&mut conn, entry.offset, entry.size)?;
            emitter.info(Event::EraseDone {
                addr: format!("{:#010x}", entry.offset),
                size: entry.size as u64,
                ms: start.elapsed().as_millis(),
            });
            report.finish_stage(g, true, None);
        }

        Command::Backup {
            output,
            size,
            compress,
        } => {
            let resolved_size = match size {
                Some(s) => *s,
                None => {
                    let id = ops::flash_id(&mut conn, chip)?;
                    let mb = ops::flash_size_mb_from_id(id).ok_or_else(|| {
                        Error::Other(
                            "could not auto-detect flash size; pass --size explicitly".into(),
                        )
                    })?;
                    mb * 1024 * 1024
                }
            };
            ops::flash_spi_attach(&mut conn, 0)?;
            let g = report.start_stage("backup");
            emitter.info(Event::BackupBegin {
                size: resolved_size as u64,
            });
            let bar = make_bar(resolved_size as u64, cli.json);
            let data = {
                let mut progress = |w: u64, _t: u64| {
                    if let Some(b) = bar.as_ref() {
                        b.set_position(w);
                    }
                };
                ops::read_flash(&mut conn, 0, resolved_size, Some(&mut progress))?
            };
            if let Some(b) = bar.as_ref() {
                b.finish_and_clear();
            }
            let use_gz = resolve_file_gz(*compress, output);
            write_backup_file(output, &data, use_gz)?;
            let md5 = md5_hex(&data);
            emitter.info(Event::BackupDone {
                size: data.len() as u64,
                md5: md5.clone(),
            });
            let mut stage = report.finish_stage(g, true, None);
            stage.bytes = Some(data.len() as u64);
            stage.md5 = Some(md5);
            *report.stages.last_mut().unwrap() = stage;
        }

        Command::Elf2Image { .. }
        | Command::MergeBin { .. }
        | Command::Monitor { .. }
        | Command::FlashMonitor { .. }
        | Command::ListPorts
        | Command::Mcp => {
            // These are dispatched before we ever open a port for the
            // protocol flow (offline ones via run_offline_if_applicable,
            // monitor + flash-monitor via their own branches in run());
            // reaching here would be a logic bug.
            unreachable!("offline / monitor command leaked into chip-connect path")
        }

        Command::Restore { input } => {
            let bytes = read_restore_file(input)?;
            ops::flash_spi_attach(&mut conn, 0)?;
            let g = report.start_stage("restore");
            emitter.info(Event::RestoreBegin {
                size: bytes.len() as u64,
            });
            let bar = make_bar(bytes.len() as u64, cli.json);
            let md5 = {
                let mut progress = |w: u64, _t: u64| {
                    if let Some(b) = bar.as_ref() {
                        b.set_position(w);
                    }
                };
                ops::write_flash(
                    &mut conn,
                    chip,
                    0,
                    &bytes,
                    Some(&mut progress),
                    !cli.no_compress,
                )?
            };
            if let Some(b) = bar.as_ref() {
                b.finish_and_clear();
            }
            emitter.info(Event::RestoreDone {
                size: bytes.len() as u64,
                md5: md5.clone(),
            });
            let mut stage = report.finish_stage(g, true, None);
            stage.bytes = Some(bytes.len() as u64);
            stage.md5 = Some(md5);
            *report.stages.last_mut().unwrap() = stage;
        }
    }

    // --- After-mode: hard reset if requested ---
    let after: AfterMode = cli.after.clone().into();
    match after {
        AfterMode::HardReset => {
            // Esptool parity: on chips with a sticky FORCE_DOWNLOAD_BOOT
            // bit (S2, S3, P4 via USB-OTG), clear it before the EN pulse
            // so the chip lands in flash on the next reset instead of
            // dropping back into the ROM download loop. The write may
            // fail during a teardown race (e.g. the stub already exited
            // its command loop); we treat that as best-effort, matching
            // upstream esptool's `try/except` pattern.
            if let Some(reg) = chip.rtc_cntl_option1_reg {
                let mask = chip.rtc_cntl_force_download_boot_mask;
                match conn.write_reg(reg, 0, mask, 0) {
                    Ok(()) => {
                        tracing::debug!(
                            target: "esparagus::reset",
                            reg = format_args!("{:#010x}", reg),
                            mask = format_args!("{:#x}", mask),
                            "cleared FORCE_DOWNLOAD_BOOT before hard reset"
                        );
                    }
                    Err(e) => {
                        // Surface as a warning so an agent watching the
                        // NDJSON sees we tried; not a fatal condition.
                        emitter.warn(Event::Warning {
                            message: format!(
                                "could not clear FORCE_DOWNLOAD_BOOT at {:#010x} (mask {:#x}): {}; \
                                 hard reset may land back in download mode",
                                reg, mask, e
                            ),
                        });
                    }
                }
            }
            let uses_usb = chip.has_usb_jtag_serial
                && vid_pid == Some((reset::ESPRESSIF_VID, reset::USB_JTAG_SERIAL_PID));
            reset::hard_reset(&mut *conn.transport, uses_usb)?;
            emitter.info(Event::ResetIssued {
                kind: if uses_usb { "usb" } else { "uart" }.into(),
            });
        }
        AfterMode::NoReset => {}
        AfterMode::NoResetStub => {
            // Leave the stub running.
        }
    }

    Ok(())
}

fn should_use_stub(cmd: &Command) -> bool {
    match cmd {
        Command::EraseFlash
        | Command::EraseRegion { .. }
        | Command::WriteFlash { .. }
        | Command::ReadFlash { .. }
        | Command::Detect
        | Command::ReadMac
        | Command::FlashId
        | Command::Partitions { .. }
        | Command::WritePartition { .. }
        | Command::ReadPartition { .. }
        | Command::ErasePartition { .. }
        | Command::Backup { .. }
        | Command::Restore { .. } => true,
        // NVS view/export from a file is offline (no chip); from the chip
        // it needs the stub for read_flash. We can't tell at this site
        // which arg the user picked, so always say "yes" for Nvs — when
        // the chip-flow runs and we go down the from_file path it just
        // skips the read_flash call.
        Command::Nvs { .. } => true,
        _ => false,
    }
}

/// Load the raw bytes of an NVS partition either from a local file (for
/// offline inspection) or by reading the named partition from the chip's
/// flash. Returns the bytes and a human-readable source label for the TUI
/// header.
fn nvs_load_bytes(
    conn: &mut Connection,
    name: &str,
    from_file: Option<&Path>,
) -> Result<(Vec<u8>, String)> {
    if let Some(p) = from_file {
        let bytes = std::fs::read(p)?;
        return Ok((bytes, format!("file:{}", p.display())));
    }
    // Resolve the partition by name via the on-chip partition table.
    let raw = ops::read_flash(conn, PARTITION_TABLE_OFFSET, PARTITION_TABLE_SECTOR, None)?;
    let table = PartitionTable::from_binary(&raw)?;
    let entry = table
        .find(name)
        .ok_or_else(|| Error::Other(format!("no partition named {:?} on chip", name)))?;
    let bytes = ops::read_flash(conn, entry.offset, entry.size, None)?;
    Ok((
        bytes,
        format!("flash:{:#x}+{:#x}", entry.offset, entry.size),
    ))
}

fn load_partition_table(
    conn: &mut Connection,
    csv_path: Option<&Path>,
    emitter: &Emitter,
) -> Result<PartitionTable> {
    match csv_path {
        Some(p) => {
            let table = PartitionTable::load_csv(p)?;
            table.validate()?;
            emitter.info(Event::PartitionTableLoaded {
                source: format!("csv:{}", p.display()),
                count: table.entries.len(),
            });
            Ok(table)
        }
        None => {
            // Read PARTITION_TABLE_SECTOR (4 KB) from flash at 0x8000.
            let raw = ops::read_flash(conn, PARTITION_TABLE_OFFSET, PARTITION_TABLE_SECTOR, None)?;
            let table = PartitionTable::from_binary(&raw)?;
            table.validate()?;
            emitter.info(Event::PartitionTableLoaded {
                source: format!(
                    "flash:{:#x}+{:#x}",
                    PARTITION_TABLE_OFFSET, PARTITION_TABLE_SECTOR
                ),
                count: table.entries.len(),
            });
            Ok(table)
        }
    }
}

fn emit_resolved(emitter: &Emitter, entry: &PartitionEntry) {
    emitter.info(Event::PartitionResolved {
        name: entry.name.clone(),
        ptype: entry.type_name().into(),
        subtype: entry.subtype_name(),
        offset: format!("{:#010x}", entry.offset),
        size: entry.size as u64,
    });
}

fn md5_hex(data: &[u8]) -> String {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

#[allow(clippy::too_many_arguments)]
fn write_payload(
    emitter: &Emitter,
    report: &mut ReportBuilder,
    conn: &mut Connection,
    chip: &chip::Chip,
    addr: u32,
    bytes: &[u8],
    json_mode: bool,
    compress: bool,
) -> Result<()> {
    let stage_name = format!("write_flash {:#010x}", addr);
    let g = report.start_stage(&stage_name);
    let addr_str = format!("{:#010x}", addr);
    emitter.info(Event::WriteBegin {
        addr: addr_str.clone(),
        size: bytes.len() as u64,
        compressed: compress,
    });
    let bar = make_bar(bytes.len() as u64, json_mode);
    let md5 = {
        let mut last_pct = 0u32;
        let emit_for_progress = emitter.clone();
        let addr_for_progress = addr_str.clone();
        let mut progress = |written: u64, total: u64| {
            if let Some(b) = bar.as_ref() {
                b.set_position(written);
            }
            let pct = written
                .checked_mul(100)
                .and_then(|n| n.checked_div(total))
                .map(|p| p as u32)
                .unwrap_or(100);
            if pct >= last_pct + 5 || pct == 100 {
                last_pct = pct;
                emit_for_progress.info(Event::WriteProgress {
                    addr: addr_for_progress.clone(),
                    written,
                    total,
                    pct: pct as f64,
                });
            }
        };
        ops::write_flash(conn, chip, addr, bytes, Some(&mut progress), compress)?
    };
    if let Some(b) = bar.as_ref() {
        b.finish_and_clear();
    }
    emitter.info(Event::Md5Verified {
        addr: addr_str,
        size: bytes.len() as u64,
        md5: md5.clone(),
    });
    let mut stage = report.finish_stage(g, true, None);
    stage.bytes = Some(bytes.len() as u64);
    stage.md5 = Some(md5);
    *report.stages.last_mut().unwrap() = stage;
    Ok(())
}

fn resolve_file_gz(mode: FileCompression, path: &Path) -> bool {
    match mode {
        FileCompression::Auto => path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("gz"))
            .unwrap_or(false),
        FileCompression::Gz => true,
        FileCompression::None => false,
    }
}

fn write_backup_file(path: &Path, data: &[u8], gz: bool) -> Result<()> {
    if gz {
        use std::io::Write;
        let f = std::fs::File::create(path)?;
        let mut enc = flate2::write::GzEncoder::new(f, flate2::Compression::best());
        enc.write_all(data)?;
        enc.finish()?;
    } else {
        std::fs::write(path, data)?;
    }
    Ok(())
}

fn read_restore_file(path: &Path) -> Result<Vec<u8>> {
    let raw = std::fs::read(path)?;
    let is_gz_by_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("gz"))
        .unwrap_or(false);
    let is_gz_by_magic = raw.len() >= 2 && raw[0] == 0x1F && raw[1] == 0x8B;
    if is_gz_by_ext || is_gz_by_magic {
        use std::io::Read;
        let mut dec = flate2::read::GzDecoder::new(&raw[..]);
        let mut out = Vec::new();
        dec.read_to_end(&mut out)?;
        Ok(out)
    } else {
        Ok(raw)
    }
}

fn make_bar(total: u64, suppress: bool) -> Option<ProgressBar> {
    if suppress {
        return None;
    }
    let bar = ProgressBar::new(total);
    bar.set_style(
        ProgressStyle::with_template("{spinner} {wide_bar} {bytes}/{total_bytes} {eta_precise}")
            .unwrap(),
    );
    Some(bar)
}

#[allow(clippy::too_many_arguments)]
fn write_one(
    addr: u32,
    path: &Path,
    conn: &mut Connection,
    chip: &Chip,
    emitter: &Emitter,
    report: &mut ReportBuilder,
    json_mode: bool,
    compress: bool,
) -> Result<()> {
    let stage_name = format!("write_flash {:#010x}", addr);
    let g = report.start_stage(&stage_name);

    let (bytes, header) = image::load_payload(path)?;
    if let Some(h) = header {
        if h.chip_id as u32 != chip.image_chip_id as u32 {
            report.record_warning(format!(
                "image at {} has chip_id {} but chip is {}",
                path.display(),
                h.chip_id,
                chip.name
            ));
            emitter.warn(Event::Warning {
                message: format!(
                    "image chip_id={} differs from connected chip {} (chip_id={})",
                    h.chip_id, chip.name, chip.image_chip_id
                ),
            });
        }
    }

    emitter.info(Event::WriteBegin {
        addr: format!("{:#010x}", addr),
        size: bytes.len() as u64,
        compressed: compress,
    });

    let bar = make_bar(bytes.len() as u64, json_mode);
    let addr_str = format!("{:#010x}", addr);
    let md5 = {
        let mut last_pct = 0u32;
        let emit_for_progress = emitter.clone();
        let addr_for_progress = addr_str.clone();
        let mut progress = |written: u64, total: u64| {
            if let Some(b) = bar.as_ref() {
                b.set_position(written);
            }
            let pct = written
                .checked_mul(100)
                .and_then(|n| n.checked_div(total))
                .map(|p| p as u32)
                .unwrap_or(100);
            if pct >= last_pct + 5 || pct == 100 {
                last_pct = pct;
                emit_for_progress.info(Event::WriteProgress {
                    addr: addr_for_progress.clone(),
                    written,
                    total,
                    pct: pct as f64,
                });
            }
        };
        ops::write_flash(conn, chip, addr, &bytes, Some(&mut progress), compress)?
    };
    if let Some(b) = bar.as_ref() {
        b.finish_and_clear();
    }

    emitter.info(Event::Md5Verified {
        addr: addr_str,
        size: bytes.len() as u64,
        md5: md5.clone(),
    });
    let mut stage = report.finish_stage(g, true, None);
    stage.bytes = Some(bytes.len() as u64);
    stage.md5 = Some(md5);
    *report.stages.last_mut().unwrap() = stage;
    Ok(())
}
