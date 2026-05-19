//! Serial monitor with GNU-expect-style pattern matching.
//!
//! Used as a top-level subcommand to chain after `write-flash` in CI / LLM
//! feedback loops:
//!
//!   esparagus write-flash ... && esparagus monitor --expect "boot ok"
//!
//! Behavior:
//!   * Open the port at the user-requested baud (no sync, no stub).
//!   * Optionally hard-reset the chip via the EN line so the boot log
//!     starts from byte 0.
//!   * Decode incoming bytes into lines (split on `\n`, strip a trailing
//!     `\r`).  Tolerates non-UTF-8 via `String::from_utf8_lossy`.
//!   * For every line: check it against `expect` and `expect_not` regexes.
//!     First positive match → exit 0, success.  First negative match →
//!     exit 30, "expect_not_match".  Whole-run timeout → exit 31.
//!   * Each line is emitted as a `serial_line` NDJSON event in --json
//!     mode, and printed directly to stdout in human mode (so it feels
//!     like `screen` / `minicom`).

use std::time::{Duration, Instant};

use regex::Regex;

use crate::error::{Error, Result};
use crate::observe::{Emitter, Event};
use crate::reset;
use crate::transport::serial::SerialTransport;
use crate::transport::Transport;

/// Outcome of a monitor session — drives the CLI exit code.
#[derive(Debug, Clone, Copy)]
pub enum Outcome {
    /// A `--expect` pattern matched → CI/LLM should treat this as success.
    ExpectMatch,
    /// A `--expect-not` pattern matched → fail fast.
    ExpectNotMatch,
    /// No pattern matched within the deadline.
    Timeout,
    /// Built-in crash detector triggered (panic / watchdog / abort / ...).
    Crash,
}

/// Built-in regex patterns for the ESP-IDF / Arduino panic handler output.
/// Each pattern carries a stable `kind` string the CLI / NDJSON consumers
/// can branch on without parsing English.
///
/// Order matters only as a tiebreaker — the first one to match wins.  We
/// list the most specific signatures first so e.g. an `assert failed` line
/// isn't mis-classified as a generic panic via a more permissive pattern.
///
/// `pub(crate)` so the expect-script runner (`src/expect.rs`) can share
/// the exact same detection set without duplication.
pub(crate) const CRASH_PATTERNS: &[(&str, &str)] = &[
    // Watchdog (often appears before the actual panic so check it first)
    (r"Task watchdog got triggered", "wdt"),
    (r"\bWDT\b.*timeout", "wdt"),
    (r"Interrupt watchdog", "wdt"),
    // Asserts
    (r"assert failed:", "assert"),
    (r"ASSERTION FAILED", "assert"),
    // abort() — both libc abort and IDF's wrapper
    (r"abort\(\) was called", "abort"),
    // Stack smash protector
    (r"Stack smashing protect failure", "stack_smash"),
    // CPU exceptions — Xtensa
    (r"Guru Meditation Error", "panic"),
    (r"LoadProhibited", "exception"),
    (r"StoreProhibited", "exception"),
    (r"IllegalInstruction", "exception"),
    (r"InstructionFetchError", "exception"),
    (r"LoadStoreError", "exception"),
    // RISC-V (C3/C6/H2/P4 use a different exception decode)
    (r"Guru Meditation", "panic"),
    (r"Exception was unhandled", "exception"),
    // Cache misconfiguration
    (r"Cache disabled but cached memory region accessed", "cache"),
    // ROM brownout
    (r"Brownout detector was triggered", "brownout"),
    // Firmware didn't boot — the chip dropped into ROM DOWNLOAD mode.
    // The ROM's own boot banner announces this; matching it in monitor
    // (i.e. *after* the flash phase) is the signal the agent needs that
    // the freshly-written firmware isn't actually running.
    (r"boot:0x[0-9a-fA-F]+ \(DOWNLOAD", "download_loop"),
];

/// Sentinel lines that mark "the crash output is done" — we stop the
/// context capture when we see one of these.
///
/// Two categories:
///   * Reboot-bound: chip is about to reset, so context past this point
///     is the next boot. `Rebooting...`, `CPU halted.`, `ELF file SHA256:`.
///   * Non-fatal dumps (RISC-V WDT warnings that don't reset): the IDF
///     panic handler emits a register dump then resumes the app. If we
///     don't stop here, the context fills up with normal post-warning
///     app logs that are unrelated. Sentinels: the RISC-V end-of-dump
///     line ("Please enable CONFIG_ESP_SYSTEM_USE_FRAME_POINTER") and
///     the Xtensa backtrace announcement.
pub(crate) const CRASH_END_SENTINELS: &[&str] = &[
    "Rebooting...",
    "CPU halted.",
    "ELF file SHA256:",
    "Please enable CONFIG_ESP_SYSTEM_USE_FRAME_POINTER",
    "Backtrace:",
];

/// Maximum lines / time to capture after a crash signature before
/// emitting the `crash_context` event.
pub(crate) const CRASH_CONTEXT_MAX_LINES: usize = 200;
pub(crate) const CRASH_CONTEXT_MAX_DURATION: Duration = Duration::from_secs(5);

#[allow(clippy::too_many_arguments)]
pub fn run(
    port: &str,
    baud: u32,
    timeout: Duration,
    expect: &[String],
    expect_not: &[String],
    no_reset: bool,
    no_crash_detect: bool,
    emitter: &Emitter,
    print_raw: bool,
) -> Result<Outcome> {
    // Compile patterns up front so a bad regex fails before we ever touch
    // the chip.  Same precompiled `Regex` used for every line — cheap.
    let pos: Vec<Regex> = expect
        .iter()
        .map(|p| {
            Regex::new(p).map_err(|e| Error::Other(format!("bad --expect regex {:?}: {}", p, e)))
        })
        .collect::<Result<Vec<_>>>()?;
    let neg: Vec<Regex> = expect_not
        .iter()
        .map(|p| {
            Regex::new(p)
                .map_err(|e| Error::Other(format!("bad --expect-not regex {:?}: {}", p, e)))
        })
        .collect::<Result<Vec<_>>>()?;
    let crash_patterns: Vec<(Regex, &'static str)> = if no_crash_detect {
        Vec::new()
    } else {
        CRASH_PATTERNS
            .iter()
            .map(|(p, kind)| (Regex::new(p).expect("built-in crash regex compiles"), *kind))
            .collect()
    };

    let mut transport = SerialTransport::open(port, baud)?;

    emitter.info(Event::MonitorStart {
        port: port.into(),
        baud,
        timeout_secs: timeout.as_secs(),
        expect: expect.to_vec(),
        expect_not: expect_not.to_vec(),
    });

    if !no_reset {
        // Deliberate "boot the app, not the bootloader" sequence: drive
        // DTR=false (so GPIO0 stays HIGH, normal-boot strap), then pulse
        // RTS to bounce EN.  Plain hard_reset() doesn't touch DTR, which
        // lets the OS-default DTR state pull GPIO0 LOW on some bridges
        // (notably CH343 on ESP32-P4 dev boards) — putting the chip into
        // DOWNLOAD mode instead of running the firmware.
        let _ = reset::reset_to_app(&mut transport);
    }

    let started = Instant::now();
    let deadline = if timeout.is_zero() {
        // 0 = run forever; use a far-future deadline.
        Instant::now() + Duration::from_secs(60 * 60 * 24 * 365)
    } else {
        started + timeout
    };

    let mut line_buf: Vec<u8> = Vec::with_capacity(256);
    let mut buf = [0u8; 1024];
    let mut lines_seen: u64 = 0;
    let mut bytes_seen: u64 = 0;
    // None = no crash in progress; Some((kind, started_at, ctx_lines)) when
    // capturing a crash context post-detection.
    let mut crash_ctx: Option<(&'static str, Instant, Vec<String>)> = None;
    // Reboot-loop detector state: count how many times we've seen the
    // ROM banner (e.g. "ESP-ROM:esp32c5-..."). The first banner is
    // expected from our own reset_to_app pulse at start. The *second*
    // means the bootloader / app rebooted on its own — which is the
    // "second-stage bootloader runs, jumps to entry, immediate reset
    // before any app log" failure pattern. Distinct from `download_loop`:
    // here the chip is in SPI_FAST_FLASH_BOOT mode, just rebooting.
    let rom_banner_re = if !no_crash_detect {
        Some(Regex::new(r"^ESP-ROM:").expect("static regex compiles"))
    } else {
        None
    };
    let mut rom_banner_count: u32 = 0;

    loop {
        let now = Instant::now();

        // Bug fix from a real bench session: the crash-context flush
        // used to be gated on a new line arriving. When the chip went
        // silent right after a crash signature (typical for a
        // `reboot_loop`: chip rebooted, ROM banner printed, then
        // chip stuck in download mode waiting), the time budget never
        // got checked and the run terminated as `timeout` instead of
        // `crash`. Check the budget every poll iteration here, before
        // the overall-deadline check, so a silent chip still produces
        // the crash_context event the agent needs.
        if let Some((kind, started_at, _)) = crash_ctx.as_ref() {
            if started_at.elapsed() >= CRASH_CONTEXT_MAX_DURATION {
                let _ = kind;
                let (k, _, lines) = crash_ctx.take().unwrap();
                emitter.error(Event::CrashContext { kind: k, lines });
                emitter.info(Event::MonitorComplete {
                    reason: "crash",
                    duration_ms: started.elapsed().as_millis(),
                    lines_seen,
                    bytes_seen,
                });
                return Ok(Outcome::Crash);
            }
        }

        if now >= deadline {
            // Flush any partial trailing line so the user/agent sees it.
            if !line_buf.is_empty() {
                emit_line(emitter, &line_buf, &mut lines_seen, print_raw);
                line_buf.clear();
            }
            // If we're mid-crash-context when the overall deadline hits,
            // promote the outcome to `crash` rather than dropping the
            // context on the floor — the agent's branching cares about
            // *why* we stopped, and "crash with truncated context" is
            // still better signal than "timeout".
            if let Some((kind, _, lines)) = crash_ctx.take() {
                emitter.error(Event::CrashContext { kind, lines });
                emitter.info(Event::MonitorComplete {
                    reason: "crash",
                    duration_ms: started.elapsed().as_millis(),
                    lines_seen,
                    bytes_seen,
                });
                return Ok(Outcome::Crash);
            }
            emitter.warn(Event::MonitorTimeout {
                lines_seen,
                bytes_seen,
            });
            emitter.info(Event::MonitorComplete {
                reason: "timeout",
                duration_ms: started.elapsed().as_millis(),
                lines_seen,
                bytes_seen,
            });
            return Ok(Outcome::Timeout);
        }
        let remaining = deadline - now;
        // 100ms read slices keep the timeout/expect-check loop responsive.
        transport.set_timeout(remaining.min(Duration::from_millis(100)))?;

        let n = match transport.read(&mut buf) {
            Ok(n) => n,
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => return Err(e),
        };
        if n == 0 {
            continue;
        }
        bytes_seen += n as u64;

        for &b in &buf[..n] {
            if b == b'\n' {
                // Strip a trailing CR if present.
                if line_buf.last() == Some(&b'\r') {
                    line_buf.pop();
                }
                emit_line(emitter, &line_buf, &mut lines_seen, print_raw);
                let line = String::from_utf8_lossy(&line_buf).into_owned();
                line_buf.clear();

                // If we're already inside a crash context, append the line
                // and check whether we hit a sentinel or the max budget.
                if let Some((kind, started_at, ctx)) = crash_ctx.as_mut() {
                    ctx.push(line.clone());
                    let ended_by_sentinel = CRASH_END_SENTINELS.iter().any(|s| line.contains(s));
                    let budget_exhausted = ctx.len() >= CRASH_CONTEXT_MAX_LINES
                        || started_at.elapsed() >= CRASH_CONTEXT_MAX_DURATION;
                    if ended_by_sentinel || budget_exhausted {
                        let (k, _, lines) = crash_ctx.take().unwrap();
                        emitter.error(Event::CrashContext { kind: k, lines });
                        emitter.info(Event::MonitorComplete {
                            reason: "crash",
                            duration_ms: started.elapsed().as_millis(),
                            lines_seen,
                            bytes_seen,
                        });
                        return Ok(Outcome::Crash);
                    }
                    let _ = kind;
                    // Still capturing — skip the normal pattern checks.
                    continue;
                }

                // Negative match first — we want fail-fast on a forbidden line.
                for pat in &neg {
                    if pat.is_match(&line) {
                        emitter.error(Event::ExpectMatch {
                            kind: "negative",
                            pattern: pat.as_str().into(),
                            line: line.clone(),
                        });
                        emitter.info(Event::MonitorComplete {
                            reason: "expect_not_match",
                            duration_ms: started.elapsed().as_millis(),
                            lines_seen,
                            bytes_seen,
                        });
                        return Ok(Outcome::ExpectNotMatch);
                    }
                }
                // Reboot-loop detector. The first ROM banner is expected
                // (our own reset_to_app produced it); the second is the
                // chip rebooting on its own. Distinct from `download_loop`:
                // boot mode is normal (e.g. SPI_FAST_FLASH_BOOT) but the
                // app never makes it far enough to print.
                if let Some(re) = &rom_banner_re {
                    if re.is_match(&line) {
                        rom_banner_count += 1;
                        if rom_banner_count >= 2 && crash_ctx.is_none() {
                            emitter.error(Event::CrashDetected {
                                kind: "reboot_loop",
                                pattern: re.as_str().into(),
                                line: line.clone(),
                            });
                            crash_ctx = Some(("reboot_loop", Instant::now(), vec![line.clone()]));
                            continue;
                        }
                    }
                }

                // Built-in crash detector. On match, switch into context
                // capture mode — we keep reading until a sentinel or the
                // budget is reached, then emit and exit.
                for (pat, kind) in &crash_patterns {
                    if pat.is_match(&line) {
                        emitter.error(Event::CrashDetected {
                            kind,
                            pattern: pat.as_str().into(),
                            line: line.clone(),
                        });
                        crash_ctx = Some((*kind, Instant::now(), vec![line.clone()]));
                        break;
                    }
                }
                if crash_ctx.is_some() {
                    continue;
                }
                for pat in &pos {
                    if pat.is_match(&line) {
                        emitter.info(Event::ExpectMatch {
                            kind: "positive",
                            pattern: pat.as_str().into(),
                            line: line.clone(),
                        });
                        emitter.info(Event::MonitorComplete {
                            reason: "expect_match",
                            duration_ms: started.elapsed().as_millis(),
                            lines_seen,
                            bytes_seen,
                        });
                        return Ok(Outcome::ExpectMatch);
                    }
                }
            } else {
                line_buf.push(b);
                // Defensive cap: if a process emits a huge unterminated
                // blob (e.g. a core-dump hex dump), flush it after 64K so
                // we still match patterns and don't OOM.
                if line_buf.len() >= 65_536 {
                    emit_line(emitter, &line_buf, &mut lines_seen, print_raw);
                    line_buf.clear();
                }
            }
        }
    }
}

fn emit_line(emitter: &Emitter, raw: &[u8], lines_seen: &mut u64, _print_raw: bool) {
    let line = String::from_utf8_lossy(raw).into_owned();
    *lines_seen += 1;
    // Output routing is centralised in the Emitter: in human mode it sends
    // SerialLine straight to stdout without a timestamp prefix; in JSON
    // mode it emits the NDJSON event.  Either way, the log_file mirrors
    // every event regardless.  This used to double-print to stdout AND
    // stderr in human mode — fixed by funnelling through one path.
    emitter.info(Event::SerialLine { line });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: compile the built-in crash patterns and look up which `kind`
    /// (if any) matches the given line. Reproduces the runtime check.
    fn classify(line: &str) -> Option<&'static str> {
        for (pat, kind) in CRASH_PATTERNS {
            if Regex::new(pat).unwrap().is_match(line) {
                return Some(*kind);
            }
        }
        None
    }

    #[test]
    fn detects_guru_meditation_xtensa() {
        // Verbatim panic header from an ESP32 LoadProhibited fault.
        assert_eq!(
            classify(
                "Guru Meditation Error: Core  0 panic'd (LoadProhibited). Exception was unhandled."
            ),
            Some("panic")
        );
    }

    #[test]
    fn detects_task_watchdog() {
        assert_eq!(
            classify("E (10000) task_wdt: Task watchdog got triggered. The following tasks/users did not reset the watchdog in time:"),
            Some("wdt")
        );
    }

    #[test]
    fn detects_abort() {
        assert_eq!(
            classify("abort() was called at PC 0x40087812 on core 0"),
            Some("abort")
        );
    }

    #[test]
    fn detects_assert() {
        assert_eq!(
            classify(
                "assert failed: do_global_ctors components/cxx/cxx_guards.cpp:42 (some condition)"
            ),
            Some("assert")
        );
    }

    #[test]
    fn detects_stack_smash() {
        assert_eq!(
            classify("Stack smashing protect failure!"),
            Some("stack_smash")
        );
    }

    #[test]
    fn detects_brownout() {
        assert_eq!(
            classify("Brownout detector was triggered"),
            Some("brownout")
        );
    }

    #[test]
    fn detects_cache_misuse() {
        assert_eq!(
            classify("Cache disabled but cached memory region accessed"),
            Some("cache")
        );
    }

    #[test]
    fn detects_illegal_instruction_exception() {
        assert_eq!(
            classify("Guru Meditation Error: Core  0 panic'd (IllegalInstruction). Exception was unhandled."),
            // First matcher wins; "Guru Meditation Error" comes earlier in
            // the table than the bare "IllegalInstruction" string, so the
            // classification rolls up to "panic" — which is the right call
            // for a panic header that happens to mention the exception.
            Some("panic")
        );
    }

    #[test]
    fn ignores_normal_log_lines() {
        assert_eq!(classify("I (1234) main: hello world"), None);
        assert_eq!(classify("[boot] 0x1000 partition table"), None);
        assert_eq!(classify(""), None);
    }

    #[test]
    fn end_sentinels_match_rebooting() {
        assert!(CRASH_END_SENTINELS
            .iter()
            .any(|s| "Rebooting...".contains(s)));
    }

    /// `reboot_loop` is stateful — fires after the second ROM-banner
    /// occurrence in a monitor session — so the per-line classifier
    /// alone doesn't classify it. Test the stateful behavior via the
    /// `Regex::is_match` on the canonical regex string.
    #[test]
    fn rom_banner_pattern_matches_real_lines() {
        let re = Regex::new(r"^ESP-ROM:").unwrap();
        assert!(re.is_match("ESP-ROM:esp32c5-eco2-20250121"));
        assert!(re.is_match("ESP-ROM:esp32p4-eco2-20240710"));
        assert!(re.is_match("ESP-ROM:esp32s3-20210327"));
        // Doesn't false-match a normal log line that happens to mention
        // the string mid-line.
        assert!(!re.is_match("I (1234) main: ESP-ROM info goes here"));
    }

    /// Verbatim ROM boot announcement when the chip drops into download
    /// mode (e.g. because firmware didn't boot or the BOOT strap is held
    /// low). Should classify as `download_loop` so an agent watching a
    /// post-flash monitor knows the chip isn't actually running the
    /// image we just wrote.
    #[test]
    fn detects_unexpected_download_mode() {
        assert_eq!(
            classify("rst:0x15 (USB_UART_HPSYS),boot:0x49 (DOWNLOAD(UART0/USB))"),
            Some("download_loop")
        );
        assert_eq!(
            classify("rst:0x1 (POWERON),boot:0x07 (DOWNLOAD(USB/UART0/SPI))"),
            Some("download_loop")
        );
        // A normal boot line (the chip booting from flash) doesn't carry
        // the (DOWNLOAD ...) tail, so we don't false-flag.
        assert_eq!(
            classify("rst:0x1 (POWERON),boot:0x8 (SPI_FAST_FLASH_BOOT)"),
            None
        );
    }
}
