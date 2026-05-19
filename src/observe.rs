//! Observability: NDJSON event stream, file logging, structured final report,
//! and diagnostic hint engine — the layer that makes esparagus useful inside
//! CI pipelines and LLM-driven feedback loops.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::error::Error;

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// A single timestamped event in the run.  Serialized as one JSON object per
/// line on stdout when `--json` is set; also fed into the final report.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    RunStart {
        tool: String,
        chip_arg: Option<String>,
        port: String,
        baud: u32,
    },
    TransportInfo {
        port: String,
        usb_vid: Option<String>,
        usb_pid: Option<String>,
    },
    ConnectAttempt {
        strategy: String,
        attempt: u32,
    },
    Connected {
        strategy: String,
        attempts: u32,
    },
    ChipDetected {
        chip: String,
        chip_id: u8,
    },
    StubUploadStart {
        chip: String,
        blob: String,
    },
    StubRunning {
        chip: String,
        blob: String,
        entry: String,
    },
    FlashIdRead {
        manufacturer: String,
        device: String,
        size_mb: Option<u32>,
    },
    MacRead {
        mac: String,
    },
    EfuseRead {
        /// Decoded MAC (same string format as `MacRead`).
        mac: String,
        /// Chip silicon revision as `major.minor` (e.g. "1.02"), or
        /// "?" if the chip's revision bits aren't in the table.
        chip_rev: String,
        /// Absolute EFUSE base address of the dumped region (hex).
        base: String,
        /// One 32-bit word per entry, little-endian as read from the
        /// memory-mapped EFUSE registers. Length matches the requested
        /// `--words` count.
        words: Vec<u32>,
    },
    WriteBegin {
        addr: String,
        size: u64,
        compressed: bool,
    },
    WriteProgress {
        addr: String,
        written: u64,
        total: u64,
        pct: f64,
    },
    Md5Verified {
        addr: String,
        size: u64,
        md5: String,
    },
    EraseBegin {
        addr: String,
        size: u64,
    },
    EraseDone {
        addr: String,
        size: u64,
        ms: u128,
    },
    ReadBegin {
        addr: String,
        size: u64,
    },
    ReadDone {
        addr: String,
        size: u64,
        md5: String,
    },
    ResetIssued {
        kind: String,
    },
    BaudUpgrade {
        from: u32,
        to: u32,
    },
    PartitionTableLoaded {
        source: String,
        count: usize,
    },
    PartitionResolved {
        name: String,
        ptype: String,
        subtype: String,
        offset: String,
        size: u64,
    },
    BackupBegin {
        size: u64,
    },
    BackupDone {
        size: u64,
        md5: String,
    },
    RestoreBegin {
        size: u64,
    },
    RestoreDone {
        size: u64,
        md5: String,
    },
    MonitorStart {
        port: String,
        baud: u32,
        timeout_secs: u64,
        expect: Vec<String>,
        expect_not: Vec<String>,
    },
    SerialLine {
        line: String,
    },
    ExpectMatch {
        kind: &'static str, // "positive" or "negative"
        pattern: String,
        line: String,
    },
    MonitorTimeout {
        lines_seen: u64,
        bytes_seen: u64,
    },
    MonitorComplete {
        reason: &'static str, // "expect_match" | "expect_not_match" | "timeout" | "crash" | "interrupted"
        duration_ms: u128,
        lines_seen: u64,
        bytes_seen: u64,
    },
    CrashDetected {
        kind: &'static str, // "panic" | "wdt" | "abort" | "assert" | "stack_smash" | "exception" | "cache"
        pattern: String,
        line: String,
    },
    CrashContext {
        kind: &'static str,
        lines: Vec<String>,
    },
    Warning {
        message: String,
    },
    Error {
        stage: String,
        class: String,
        detail: String,
    },
    RunComplete {
        ok: bool,
        duration_ms: u128,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct LoggedEvent {
    pub ts: String,
    pub level: &'static str,
    #[serde(flatten)]
    pub event: Event,
}

// ---------------------------------------------------------------------------
// Emitter
// ---------------------------------------------------------------------------

/// Where structured events go. `json` mode writes NDJSON to stdout; `plain`
/// renders a human-readable line. Optional `log_file` receives every event in
/// NDJSON regardless of mode.
pub struct Emitter {
    inner: Arc<Mutex<EmitterInner>>,
}

struct EmitterInner {
    json_stdout: bool,
    plain: bool,
    file: Option<std::fs::File>,
    /// True when stderr is a TTY (human runs at an interactive prompt) AND
    /// we're not in JSON mode. Drives ANSI colour emission so piped runs
    /// stay clean.
    color: bool,
}

impl Emitter {
    pub fn new(json_stdout: bool, log_file: Option<&Path>) -> std::io::Result<Self> {
        use std::io::IsTerminal;
        let file = match log_file {
            Some(p) => Some(OpenOptions::new().create(true).append(true).open(p)?),
            None => None,
        };
        // Honour NO_COLOR (https://no-color.org) and CLICOLOR=0.
        let no_color = std::env::var_os("NO_COLOR").is_some()
            || matches!(std::env::var("CLICOLOR").as_deref(), Ok("0"));
        let color = !json_stdout && std::io::stderr().is_terminal() && !no_color;
        Ok(Self {
            inner: Arc::new(Mutex::new(EmitterInner {
                json_stdout,
                plain: !json_stdout,
                file,
                color,
            })),
        })
    }

    /// Emit one event. Failures (broken pipe, full disk) are swallowed; we
    /// don't let observability break a real flash operation.
    pub fn emit(&self, level: &'static str, event: Event) {
        let logged = LoggedEvent {
            ts: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            level,
            event,
        };
        let line = serde_json::to_string(&logged)
            .unwrap_or_else(|_| "{\"event\":\"serialize_failed\"}".into());
        let mut g = self.inner.lock().unwrap();
        if g.json_stdout {
            let _ = writeln!(std::io::stdout().lock(), "{}", line);
        } else if g.plain {
            // SerialLine *is* the user-visible firmware output — it belongs
            // on stdout without a `[ts] ...` decoration, the way a normal
            // `screen` / `minicom` session looks.  Other events are
            // metadata and go to stderr so they don't get mixed into the
            // captured serial stream.
            if let Event::SerialLine { line: ref serial } = logged.event {
                let mut out = std::io::stdout().lock();
                let _ = writeln!(out, "{}", serial);
                let _ = out.flush();
            } else {
                let _ = writeln!(std::io::stderr().lock(), "{}", human(&logged, g.color));
            }
        }
        if let Some(f) = g.file.as_mut() {
            let _ = writeln!(f, "{}", line);
        }
    }

    pub fn info(&self, event: Event) {
        self.emit("info", event);
    }
    pub fn warn(&self, event: Event) {
        self.emit("warn", event);
    }
    pub fn error(&self, event: Event) {
        self.emit("error", event);
    }
}

impl Clone for Emitter {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

// ANSI escape helpers. Inactive when `c` is false so the same formatter
// works for both colorised TTY output and plain piped logs.
const ANSI_RESET: &str = "\x1b[0m";
fn dim(s: impl AsRef<str>, c: bool) -> String {
    if c {
        format!("\x1b[2m{}{}", s.as_ref(), ANSI_RESET)
    } else {
        s.as_ref().to_string()
    }
}
fn bold(s: impl AsRef<str>, c: bool) -> String {
    if c {
        format!("\x1b[1m{}{}", s.as_ref(), ANSI_RESET)
    } else {
        s.as_ref().to_string()
    }
}
fn cyan(s: impl AsRef<str>, c: bool) -> String {
    if c {
        format!("\x1b[36m{}{}", s.as_ref(), ANSI_RESET)
    } else {
        s.as_ref().to_string()
    }
}
fn green(s: impl AsRef<str>, c: bool) -> String {
    if c {
        format!("\x1b[32m{}{}", s.as_ref(), ANSI_RESET)
    } else {
        s.as_ref().to_string()
    }
}
fn yellow(s: impl AsRef<str>, c: bool) -> String {
    if c {
        format!("\x1b[33m{}{}", s.as_ref(), ANSI_RESET)
    } else {
        s.as_ref().to_string()
    }
}
fn red(s: impl AsRef<str>, c: bool) -> String {
    if c {
        format!("\x1b[31m{}{}", s.as_ref(), ANSI_RESET)
    } else {
        s.as_ref().to_string()
    }
}
fn magenta(s: impl AsRef<str>, c: bool) -> String {
    if c {
        format!("\x1b[35m{}{}", s.as_ref(), ANSI_RESET)
    } else {
        s.as_ref().to_string()
    }
}

fn human(e: &LoggedEvent, c: bool) -> String {
    // Every line has the same shape: dim timestamp, then the per-event
    // colorised body. Keeps the visual rhythm consistent so the eye can
    // scan a long run quickly.
    let ts = dim(format!("[{}]", e.ts), c);
    match &e.event {
        Event::RunStart {
            tool, port, baud, ..
        } => format!(
            "{} {} starting on {} @ {}",
            ts,
            bold(tool, c),
            cyan(port, c),
            baud
        ),
        Event::TransportInfo {
            port,
            usb_vid,
            usb_pid,
        } => format!(
            "{} transport {} vid={} pid={}",
            ts,
            cyan(port, c),
            usb_vid.clone().unwrap_or_else(|| "?".into()),
            usb_pid.clone().unwrap_or_else(|| "?".into())
        ),
        Event::ConnectAttempt { strategy, attempt } => format!(
            "{} connect {} (attempt {})",
            ts,
            magenta(strategy, c),
            attempt
        ),
        Event::Connected { strategy, attempts } => format!(
            "{} connected via {} after {} attempt(s)",
            ts,
            green(strategy, c),
            attempts
        ),
        Event::ChipDetected { chip, chip_id } => format!(
            "{} detected {} (chip_id={})",
            ts,
            bold(green(chip, c), c),
            chip_id
        ),
        Event::StubUploadStart { chip, blob } => format!(
            "{} uploading stub {} for {}",
            ts,
            cyan(blob, c),
            bold(chip, c)
        ),
        Event::StubRunning { chip, blob, entry } => format!(
            "{} stub {} running on {} (entry {})",
            ts,
            green(blob, c),
            bold(chip, c),
            magenta(entry, c)
        ),
        Event::FlashIdRead {
            manufacturer,
            device,
            size_mb,
        } => format!(
            "{} flash id: mfr={} dev={} size={}",
            ts,
            magenta(manufacturer, c),
            magenta(device, c),
            bold(
                size_mb
                    .map(|v| format!("{}MB", v))
                    .unwrap_or_else(|| "?".into()),
                c
            )
        ),
        Event::MacRead { mac } => format!("{} MAC {}", ts, bold(mac, c)),
        Event::EfuseRead {
            mac,
            chip_rev,
            base,
            words,
        } => {
            let mut lines = vec![format!(
                "{} EFUSE base={} MAC={} rev={}",
                ts,
                cyan(base, c),
                bold(mac, c),
                bold(chip_rev, c)
            )];
            for (i, w) in words.iter().enumerate() {
                if i % 4 == 0 {
                    lines.push(format!("{} {}", ts, cyan(format!("+{:#06x}", i * 4), c)));
                }
                let last = lines.len() - 1;
                lines[last].push_str(&format!(" {:08x}", w));
            }
            lines.join("\n")
        }
        Event::WriteBegin {
            addr,
            size,
            compressed,
        } => format!(
            "{} writing {} bytes at {} (compressed={})",
            ts,
            bold(size.to_string(), c),
            cyan(addr, c),
            compressed
        ),
        Event::WriteProgress {
            addr,
            written,
            total,
            pct,
        } => format!(
            "{}   {}: {} / {} ({:.1}%)",
            ts,
            cyan(addr, c),
            written,
            total,
            pct
        ),
        Event::Md5Verified { addr, size, md5 } => format!(
            "{} {} ({} bytes) md5={}",
            ts,
            cyan(addr, c),
            size,
            green(md5, c)
        ),
        Event::EraseBegin { addr, size } => {
            format!("{} erase {} +{} bytes", ts, cyan(addr, c), size)
        }
        Event::EraseDone { addr, size, ms } => format!(
            "{} erased {} +{} bytes in {}ms",
            ts,
            cyan(addr, c),
            size,
            ms
        ),
        Event::ReadBegin { addr, size } => {
            format!("{} read {} +{} bytes", ts, cyan(addr, c), size)
        }
        Event::ReadDone { addr, size, md5 } => format!(
            "{} read {} +{} bytes md5={}",
            ts,
            cyan(addr, c),
            size,
            green(md5, c)
        ),
        Event::ResetIssued { kind } => format!("{} reset issued ({})", ts, magenta(kind, c)),
        Event::BaudUpgrade { from, to } => {
            format!(
                "{} baud upgrade {} -> {}",
                ts,
                from,
                bold(to.to_string(), c)
            )
        }
        Event::PartitionTableLoaded { source, count } => format!(
            "{} partition table {} ({} entries)",
            ts,
            cyan(source, c),
            count
        ),
        Event::PartitionResolved {
            name,
            ptype,
            subtype,
            offset,
            size,
        } => format!(
            "{} partition {} type={}/{} @ {} ({} bytes)",
            ts,
            bold(name, c),
            ptype,
            magenta(subtype, c),
            cyan(offset, c),
            size
        ),
        Event::BackupBegin { size } => format!("{} backup begin ({} bytes)", ts, size),
        Event::BackupDone { size, md5 } => {
            format!("{} backup done {} bytes md5={}", ts, size, green(md5, c))
        }
        Event::RestoreBegin { size } => format!("{} restore begin ({} bytes)", ts, size),
        Event::RestoreDone { size, md5 } => {
            format!("{} restore done {} bytes md5={}", ts, size, green(md5, c))
        }
        Event::MonitorStart {
            port,
            baud,
            timeout_secs,
            expect,
            expect_not,
        } => format!(
            "{} monitor {} @ {} (timeout {}s, expect {:?}, expect_not {:?})",
            ts,
            cyan(port, c),
            baud,
            timeout_secs,
            expect,
            expect_not
        ),
        Event::SerialLine { line } => line.clone(),
        Event::ExpectMatch {
            kind,
            pattern,
            line,
        } => format!(
            "{} {} match {:?} on line: {}",
            ts,
            if *kind == "negative" {
                red(*kind, c)
            } else {
                green(*kind, c)
            },
            pattern,
            line
        ),
        Event::MonitorTimeout {
            lines_seen,
            bytes_seen,
        } => format!(
            "{} {} ({} lines, {} bytes)",
            ts,
            yellow("monitor timeout", c),
            lines_seen,
            bytes_seen
        ),
        Event::MonitorComplete {
            reason,
            duration_ms,
            lines_seen,
            bytes_seen,
        } => {
            let coloured = match *reason {
                "expect_match" => green(*reason, c),
                "expect_not_match" | "crash" => red(*reason, c),
                "timeout" => yellow(*reason, c),
                _ => (*reason).to_string(),
            };
            format!(
                "{} monitor complete ({}) {}ms / {} lines / {} bytes",
                ts, coloured, duration_ms, lines_seen, bytes_seen
            )
        }
        Event::CrashDetected {
            kind,
            pattern,
            line,
        } => format!(
            "{} {} ({}) matched {:?}: {}",
            ts,
            red(bold("!! CRASH", c), c),
            red(*kind, c),
            pattern,
            line
        ),
        Event::CrashContext { kind, lines } => format!(
            "{} {} ({}, {} lines):\n{}",
            ts,
            red(bold("crash context", c), c),
            red(*kind, c),
            lines.len(),
            lines.join("\n")
        ),
        Event::Warning { message } => format!("{} {}: {}", ts, yellow("WARN", c), message),
        Event::Error {
            stage,
            class,
            detail,
        } => format!(
            "{} {} {}/{}: {}",
            ts,
            red(bold("ERROR", c), c),
            stage,
            magenta(class, c),
            detail
        ),
        Event::RunComplete { ok, duration_ms } => format!(
            "{} run complete ({}) in {}ms",
            ts,
            if *ok {
                green(bold("ok", c), c)
            } else {
                red(bold("FAILED", c), c)
            },
            duration_ms
        ),
    }
}

// ---------------------------------------------------------------------------
// Stage + Report
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Stage {
    pub name: String,
    pub ok: bool,
    pub ms: u128,
    pub attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub md5: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportError {
    pub stage: String,
    pub class: String,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct NextAction {
    pub kind: &'static str,
    pub desc: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub ok: bool,
    pub tool: String,
    pub started_at: String,
    pub duration_ms: u128,
    pub chip: Option<String>,
    pub transport: ReportTransport,
    pub stages: Vec<Stage>,
    pub warnings: Vec<String>,
    pub errors: Vec<ReportError>,
    pub next_actions: Vec<NextAction>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportTransport {
    pub port: String,
    pub baud: u32,
}

/// Builder for an end-of-run report. Lives in the runner across all stages.
pub struct ReportBuilder {
    started: Instant,
    started_ts: DateTime<Utc>,
    pub tool: String,
    pub chip: Option<String>,
    pub transport: ReportTransport,
    pub stages: Vec<Stage>,
    pub warnings: Vec<String>,
    pub errors: Vec<ReportError>,
}

impl ReportBuilder {
    pub fn new(tool: String, transport: ReportTransport) -> Self {
        Self {
            started: Instant::now(),
            started_ts: Utc::now(),
            tool,
            chip: None,
            transport,
            stages: vec![],
            warnings: vec![],
            errors: vec![],
        }
    }

    pub fn start_stage(&self, name: impl Into<String>) -> StageGuard {
        StageGuard {
            name: name.into(),
            start: Instant::now(),
            attempts: 1,
        }
    }

    pub fn finish_stage(&mut self, g: StageGuard, ok: bool, detail: Option<String>) -> Stage {
        let stage = Stage {
            name: g.name,
            ok,
            ms: g.start.elapsed().as_millis(),
            attempts: g.attempts,
            bytes: None,
            md5: None,
            detail,
        };
        self.stages.push(stage.clone());
        stage
    }

    pub fn record_stage(&mut self, stage: Stage) {
        self.stages.push(stage);
    }

    pub fn record_warning(&mut self, msg: impl Into<String>) {
        self.warnings.push(msg.into());
    }

    pub fn record_error(&mut self, stage: impl Into<String>, err: &Error) {
        self.errors.push(ReportError {
            stage: stage.into(),
            class: err.class().into(),
            detail: err.to_string(),
        });
    }

    pub fn build(self, ok: bool) -> Report {
        let hints = crate::observe::hints::for_errors(&self.errors);
        Report {
            ok,
            tool: self.tool,
            started_at: self
                .started_ts
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            duration_ms: self.started.elapsed().as_millis(),
            chip: self.chip,
            transport: self.transport,
            stages: self.stages,
            warnings: self.warnings,
            errors: self.errors,
            next_actions: hints,
        }
    }

    pub fn write_to(report: &Report, path: &Path) -> std::io::Result<()> {
        let s = serde_json::to_string_pretty(report).map_err(std::io::Error::other)?;
        std::fs::write(path, s)
    }
}

pub struct StageGuard {
    pub name: String,
    pub start: Instant,
    pub attempts: u32,
}

impl StageGuard {
    pub fn bump_attempt(&mut self) {
        self.attempts += 1;
    }
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
}

/// Resolve a CLI-supplied path (`--log-file flash.log`), expanding `~`.
pub fn expand_path(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return Path::new(&home).join(rest);
        }
    }
    PathBuf::from(p)
}

// ---------------------------------------------------------------------------
// Diagnostic hint engine
// ---------------------------------------------------------------------------

pub mod hints {
    use super::{NextAction, ReportError};

    /// Map a list of errors to a deduplicated, ordered list of next-action
    /// suggestions. Stable strings so LLMs / CI scripts can match on `kind`.
    pub fn for_errors(errors: &[ReportError]) -> Vec<NextAction> {
        let mut out: Vec<NextAction> = vec![];
        let mut push_unique = |action: NextAction| {
            if !out.iter().any(|a| a.kind == action.kind) {
                out.push(action);
            }
        };
        for e in errors {
            for h in hints_for(&e.class, &e.detail) {
                push_unique(*h);
            }
        }
        out
    }

    pub fn hints_for(class: &str, detail: &str) -> &'static [NextAction] {
        match class {
            "sync_timeout" => &[
                NextAction {
                    kind: "manual_bootloader",
                    desc: "Hold BOOT, press and release EN, release BOOT, then retry.",
                },
                NextAction {
                    kind: "check_cable",
                    desc: "Try a known data-capable USB cable; some are power-only.",
                },
                NextAction {
                    kind: "lower_baud",
                    desc: "Retry with --baud 115200 to rule out signal-integrity issues.",
                },
                NextAction {
                    kind: "different_reset_mode",
                    desc: "Pass --before usb-reset if the board has a native USB-Serial/JTAG.",
                },
            ],
            "port_busy" => &[
                NextAction {
                    kind: "wait_other_instance",
                    desc: "Another esparagus instance is using this port. Wait for it to finish or kill it.",
                },
                NextAction {
                    kind: "close_other_users",
                    desc: "If you have screen / minicom / a serial monitor on this port, close it.",
                },
            ],
            "port" => {
                if detail.contains("Permission denied")
                    || detail.contains("Access is denied")
                {
                    return &[
                        NextAction {
                            kind: "udev_group",
                            desc: "On Linux: add yourself to the 'dialout' (or 'uucp') group, log out, log back in.",
                        },
                        NextAction {
                            kind: "close_other_users",
                            desc: "Close any IDE / serial monitor that already has the port open.",
                        },
                    ];
                }
                if detail.contains("No such file") || detail.contains("FileNotFoundError") {
                    return &[
                        NextAction {
                            kind: "check_port",
                            desc: "Confirm the device path; run `ls /dev/cu.* /dev/ttyUSB* /dev/ttyACM*`.",
                        },
                        NextAction {
                            kind: "check_cable",
                            desc: "Replug the USB cable; check it isn't a power-only cable.",
                        },
                    ];
                }
                &[NextAction {
                    kind: "check_port",
                    desc: "Verify the port path is correct and the device is connected.",
                }]
            }
            "unsupported_command" => &[
                NextAction {
                    kind: "use_stub",
                    desc: "This command requires the flasher stub. Remove --no-stub.",
                },
            ],
            "chip_mismatch" => &[NextAction {
                kind: "fix_chip_flag",
                desc: "Remove --chip or set it to the value reported in the chip_detected event.",
            }],
            "md5_mismatch" => &[
                NextAction {
                    kind: "retry_lower_baud",
                    desc: "MD5 mismatch often indicates UART corruption. Retry with --baud 115200.",
                },
                NextAction {
                    kind: "check_psu",
                    desc: "Brownouts during write also cause MD5 failures. Use a quality 5V supply.",
                },
            ],
            "stub_handshake" => &[NextAction {
                kind: "use_no_stub",
                desc: "Stub failed to start. Retry with --no-stub for slower but more compatible operation.",
            }],
            "stub_upload" => &[NextAction {
                kind: "use_no_stub",
                desc: "Stub upload failed. Retry with --no-stub.",
            }],
            "invalid_image" => &[NextAction {
                kind: "check_image",
                desc: "Verify the image was built for this chip and starts with magic 0xE9.",
            }],
            "unknown_chip" => &[NextAction {
                kind: "update_tool",
                desc: "Detected chip is not in the registry; check for a newer esparagus version.",
            }],
            _ => &[],
        }
    }
}
