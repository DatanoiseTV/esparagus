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
    },
    StubRunning {
        chip: String,
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
}

impl Emitter {
    pub fn new(json_stdout: bool, log_file: Option<&Path>) -> std::io::Result<Self> {
        let file = match log_file {
            Some(p) => Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)?,
            ),
            None => None,
        };
        Ok(Self {
            inner: Arc::new(Mutex::new(EmitterInner {
                json_stdout,
                plain: !json_stdout,
                file,
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
            let _ = writeln!(std::io::stderr().lock(), "{}", human(&logged));
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

fn human(e: &LoggedEvent) -> String {
    match &e.event {
        Event::RunStart { tool, port, baud, .. } => {
            format!("[{}] {} starting on {} @ {}", e.ts, tool, port, baud)
        }
        Event::ConnectAttempt { strategy, attempt } => {
            format!("[{}] connect {} (attempt {})", e.ts, strategy, attempt)
        }
        Event::Connected { strategy, attempts } => {
            format!("[{}] connected via {} after {} attempt(s)", e.ts, strategy, attempts)
        }
        Event::ChipDetected { chip, chip_id } => {
            format!("[{}] detected {} (chip_id={})", e.ts, chip, chip_id)
        }
        Event::StubUploadStart { chip } => format!("[{}] uploading stub for {}", e.ts, chip),
        Event::StubRunning { chip, entry } => {
            format!("[{}] stub running on {} (entry {})", e.ts, chip, entry)
        }
        Event::FlashIdRead { manufacturer, device, size_mb } => format!(
            "[{}] flash id: mfr={} dev={} size={}MB",
            e.ts,
            manufacturer,
            device,
            size_mb.map(|v| v.to_string()).unwrap_or_else(|| "?".into())
        ),
        Event::MacRead { mac } => format!("[{}] MAC {}", e.ts, mac),
        Event::WriteBegin { addr, size, compressed } => format!(
            "[{}] writing {} bytes at {} (compressed={})",
            e.ts, size, addr, compressed
        ),
        Event::WriteProgress { addr, written, total, pct } => format!(
            "[{}]   {}: {} / {} ({:.1}%)",
            e.ts, addr, written, total, pct
        ),
        Event::Md5Verified { addr, size, md5 } => {
            format!("[{}] {} ({} bytes) md5={}", e.ts, addr, size, md5)
        }
        Event::EraseBegin { addr, size } => format!("[{}] erase {} +{} bytes", e.ts, addr, size),
        Event::EraseDone { addr, size, ms } => {
            format!("[{}] erased {} +{} bytes in {}ms", e.ts, addr, size, ms)
        }
        Event::ReadBegin { addr, size } => format!("[{}] read {} +{} bytes", e.ts, addr, size),
        Event::ReadDone { addr, size, md5 } => {
            format!("[{}] read {} +{} bytes md5={}", e.ts, addr, size, md5)
        }
        Event::ResetIssued { kind } => format!("[{}] reset issued ({})", e.ts, kind),
        Event::Warning { message } => format!("[{}] WARN: {}", e.ts, message),
        Event::Error { stage, class, detail } => {
            format!("[{}] ERROR {}/{}: {}", e.ts, stage, class, detail)
        }
        Event::RunComplete { ok, duration_ms } => format!(
            "[{}] run complete ({}) in {}ms",
            e.ts,
            if *ok { "ok" } else { "FAILED" },
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
