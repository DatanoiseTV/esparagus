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
use crate::cli::{Cli, Command};
use crate::error::{Error, Result};
use crate::observe::{Emitter, Event, Report, ReportBuilder, ReportTransport};
use crate::protocol::Connection;
use crate::reset::{strategy_sequence, AfterMode, ResetMode};
use crate::transport::serial::SerialTransport;
use crate::transport::Transport;
use crate::{chip, image, ops, reset, stub};

pub struct Runtime {
    pub emitter: Emitter,
    pub report: ReportBuilder,
}

/// Top-level entrypoint used by main.rs. Returns the appropriate process
/// exit code; the report has already been written.
pub fn run(cli: Cli) -> i32 {
    // Required port for everything but `--help` and `--version`, which clap
    // handles itself.
    let port = match &cli.port {
        Some(p) => p.clone(),
        None => {
            eprintln!("error: --port is required");
            return 2;
        }
    };
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
        ReportTransport { port: port.clone(), baud },
    );

    emitter.info(Event::RunStart {
        tool: tool.clone(),
        chip_arg: cli.chip.clone(),
        port: port.clone(),
        baud,
    });

    let ok = match run_inner(&cli, &port, baud, &emitter, &mut report) {
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
    emitter.info(Event::RunComplete { ok, duration_ms: duration });

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
    }
    .into()
}

fn exit_code_for(report: &Report) -> i32 {
    let class = report.errors.first().map(|e| e.class.as_str()).unwrap_or("");
    match class {
        "port" => 10,
        "sync_timeout" => 11,
        "chip_mismatch" => 12,
        "md5_mismatch" | "command_failed" => 13,
        "stub_handshake" | "stub_upload" => 14,
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
    // --- Open port ---
    let transport = SerialTransport::open(port, baud)?;
    let vid_pid = transport
        .usb_vid()
        .zip(transport.usb_pid());
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
                continue;
            }
        }
        connected_strategy = Some(strategy.name().into());
        last_err = None;
        break;
    }

    if connected_strategy.is_none() {
        let err = last_err.unwrap_or(Error::SyncTimeout { attempts: attempts_max });
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
    if !cli.no_stub && should_use_stub(&cli.command) {
        let stub_guard = report.start_stage("stub_upload");
        emitter.info(Event::StubUploadStart {
            chip: chip.name.into(),
        });
        match stub::run(&mut conn, chip) {
            Ok(blob) => {
                emitter.info(Event::StubRunning {
                    chip: chip.name.into(),
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

    // --- Run the operation ---
    match &cli.command {
        Command::Detect => {
            let mac = ops::read_mac(&mut conn, chip)?;
            let id = ops::flash_id(&mut conn, chip)?;
            let mfr = (id & 0xFF) as u8;
            let dev = (id >> 8) & 0xFFFF;
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
            let dev = (id >> 8) & 0xFFFF;
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
                write_one(*addr, path, &mut conn, chip, emitter, report, cli.json)?;
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
    }

    // --- After-mode: hard reset if requested ---
    let after: AfterMode = cli.after.clone().into();
    match after {
        AfterMode::HardReset => {
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
    matches!(
        cmd,
        Command::EraseFlash
            | Command::EraseRegion { .. }
            | Command::WriteFlash { .. }
            | Command::ReadFlash { .. }
            | Command::Detect
            | Command::ReadMac
            | Command::FlashId
    )
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

fn write_one(
    addr: u32,
    path: &Path,
    conn: &mut Connection,
    chip: &Chip,
    emitter: &Emitter,
    report: &mut ReportBuilder,
    json_mode: bool,
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
        compressed: true,
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
            let pct = if total == 0 { 100 } else { (written * 100 / total) as u32 };
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
        ops::write_flash(conn, chip, addr, &bytes, Some(&mut progress))?
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
