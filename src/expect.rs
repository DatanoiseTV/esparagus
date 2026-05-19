//! Scriptable expect mode.
//!
//! Drives a serial port through a TOML-defined sequence of `send` /
//! `expect` steps with regex captures, named branches, per-step
//! timeouts, and the same crash detectors as the monitor.  Designed
//! for CI / LLM agents that need deterministic firmware verification
//! without learning TCL.
//!
//! Surface (single subcommand):
//!
//! ```text
//! esparagus expect script.toml --port /dev/cu.usbserial-XYZ
//! ```
//!
//! Script shape:
//!
//! ```toml
//! name = "boot-and-login"   # optional metadata
//! timeout_secs = 30         # default per-step timeout
//!
//! [[step]]
//! name = "wait-prompt"
//! expect = "login: "
//!
//! [[step]]
//! name = "send-user"
//! send = "root\n"
//! expect = "Password: "
//!
//! [[step]]
//! name = "send-pw"
//! send = "{{env.DEVICE_PW}}\n"
//! expect = "# "
//! capture = { prompt = "(.+) #" }
//!
//! [[step]]
//! name = "check"
//! send = "uname -a\n"
//! expect_any = [
//!     { pattern = "Linux", goto = "done" },
//!     { pattern = "ERROR",  goto = "fail" },
//! ]
//!
//! [[step]]
//! name = "done"
//! ok = true
//!
//! [[step]]
//! name = "fail"
//! ok = false
//! ```
//!
//! Template substitution is mustache-style:
//!   * `{{env.NAME}}`  → `std::env::var("NAME")`, empty string if unset.
//!   * `{{X}}`         → captures named `X` from earlier expects.
//!   * `{{1}}`..`{{9}}`→ positional groups from the immediately prior
//!     successful match.
//!
//! Exit codes (kept distinct from chip-flow and monitor codes so
//! callers can branch on the number alone without checking which
//! subcommand produced it):
//!   * 0  — script ended on an `ok = true` step or fell off the end
//!     with no expects pending.
//!   * 40 — expect timed out, or an `ok = false` terminal step.
//!   * 41 — `expect_not` matched (negative pattern hit).
//!   * 42 — crash detector fired during an expect wait.
//!   * 43 — script parse / validation error.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::monitor::{
    CRASH_CONTEXT_MAX_DURATION, CRASH_CONTEXT_MAX_LINES, CRASH_END_SENTINELS, CRASH_PATTERNS,
};
use crate::observe::{Emitter, Event};
use crate::reset;
use crate::transport::serial::SerialTransport;
use crate::transport::Transport;

// ---------------------------------------------------------------------------
// Script schema (serde)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug, Clone)]
pub struct Script {
    /// Optional human-readable name; surfaced in NDJSON events.
    #[serde(default)]
    pub name: Option<String>,
    /// Default per-step timeout (seconds). Steps may override.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Steps in declared order. Use `goto` to jump out of order.
    #[serde(default, rename = "step")]
    pub steps: Vec<Step>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Step {
    /// Optional label; targets for `goto` reference this name. Must be
    /// unique across the script.
    #[serde(default)]
    pub name: Option<String>,
    /// String to send before the expect. Templates with `{{var}}` are
    /// expanded against env, captures, and positional groups.
    #[serde(default)]
    pub send: Option<String>,
    /// Single regex to match against incoming lines.
    #[serde(default)]
    pub expect: Option<String>,
    /// Optional negative pattern: if matched, abort with exit 13.
    #[serde(default)]
    pub expect_not: Option<String>,
    /// Alternative branches; first matching pattern wins, control
    /// transfers to the step named in its `goto`.
    #[serde(default)]
    pub expect_any: Vec<Branch>,
    /// Named regex captures from the matched line. Each value is its
    /// own regex with named groups, e.g. `{ ip = "addr (\\d+\\.\\d+\\.\\d+\\.\\d+)" }`.
    /// Stored in the substitution table for later steps.
    #[serde(default)]
    pub capture: HashMap<String, String>,
    /// Per-step timeout override (seconds).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Terminal step. `ok = true` exits 0; `ok = false` exits 12.
    /// Stops script execution; later steps don't run.
    #[serde(default)]
    pub ok: Option<bool>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Branch {
    pub pattern: String,
    pub goto: String,
}

fn default_timeout_secs() -> u64 {
    30
}

// ---------------------------------------------------------------------------
// Parsing & validation
// ---------------------------------------------------------------------------

/// Outcome class — maps to one of the documented esparagus exit codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    ExpectFail,
    ExpectNotFail,
    Crash,
    ParseError,
}

impl Outcome {
    pub fn exit_code(self) -> i32 {
        match self {
            Outcome::Ok => 0,
            Outcome::ExpectFail => 40,
            Outcome::ExpectNotFail => 41,
            Outcome::Crash => 42,
            Outcome::ParseError => 43,
        }
    }
}

/// Parse a script from a TOML file at `path`.
pub fn load(path: &Path) -> Result<Script> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Other(format!("read script {}: {}", path.display(), e)))?;
    parse(&text)
}

/// Parse a script from in-memory TOML.
pub fn parse(text: &str) -> Result<Script> {
    let script: Script =
        toml::from_str(text).map_err(|e| Error::Other(format!("parse script: {}", e)))?;
    validate(&script)?;
    Ok(script)
}

/// Validate cross-references and regex compilability at load time, so
/// we surface mistakes (typoed `goto`, malformed regex) before opening
/// the serial port.
///
/// Patterns containing `{{...}}` are NOT regex-compiled here — they
/// reference runtime captures/env that aren't known yet. They get a
/// "looks like a template" check (matched braces) and are compiled at
/// run time after substitution, with a clear error class.
fn validate(s: &Script) -> Result<()> {
    if s.steps.is_empty() {
        return Err(Error::Other("script has no [[step]] entries".into()));
    }
    // Build name → index map; check uniqueness.
    let mut by_name: HashMap<&str, usize> = HashMap::new();
    for (i, st) in s.steps.iter().enumerate() {
        if let Some(n) = &st.name {
            if by_name.insert(n.as_str(), i).is_some() {
                return Err(Error::Other(format!("duplicate step name {:?}", n)));
            }
        }
    }
    // Compile each regex; resolve every `goto`.
    for (i, st) in s.steps.iter().enumerate() {
        let label = step_label(st, i);
        if let Some(e) = &st.expect {
            check_pattern(&label, "expect", e)?;
        }
        if let Some(e) = &st.expect_not {
            check_pattern(&label, "expect_not", e)?;
        }
        for b in &st.expect_any {
            check_pattern(&label, "expect_any", &b.pattern)?;
            if !by_name.contains_key(b.goto.as_str()) {
                return Err(Error::Other(format!(
                    "step {label}: goto target {:?} does not exist",
                    b.goto
                )));
            }
        }
        for (k, v) in &st.capture {
            // Capture-table patterns can't reference earlier captures
            // (chicken-and-egg), so compile them eagerly.
            Regex::new(v).map_err(|err| {
                Error::Other(format!(
                    "step {label}: bad capture regex for {:?}: {}",
                    k, err
                ))
            })?;
        }
        // At most one of {expect, expect_any} per step; a step with
        // only `send` (no wait) is allowed and is essentially "fire
        // and continue".
        if st.expect.is_some() && !st.expect_any.is_empty() {
            return Err(Error::Other(format!(
                "step {label}: cannot use both expect and expect_any",
            )));
        }
    }
    Ok(())
}

/// Validate-time pattern check. If the pattern contains a `{{...}}`
/// template, we can't regex-compile it (the substitution target is a
/// runtime value) so we only sanity-check that braces are balanced.
/// Otherwise we compile as usual.
fn check_pattern(step: &str, field: &str, pat: &str) -> Result<()> {
    if pat.contains("{{") {
        if pat.matches("{{").count() != pat.matches("}}").count() {
            return Err(Error::Other(format!(
                "step {step}: {field} pattern {:?} has unbalanced {{{{...}}}} braces",
                pat
            )));
        }
        return Ok(());
    }
    Regex::new(pat)
        .map_err(|err| Error::Other(format!("step {step}: bad {field} regex {:?}: {}", pat, err)))
        .map(|_| ())
}

/// Runtime pattern compile: substitute templates, then compile. Used
/// for any pattern field that may contain `{{var}}` references.
fn compile_pattern(pat: &str, vars: &HashMap<String, String>) -> Result<Regex> {
    let expanded = substitute(pat, vars);
    Regex::new(&expanded).map_err(|e| {
        Error::Other(format!(
            "runtime regex compile failed for {:?} (expanded to {:?}): {}",
            pat, expanded, e
        ))
    })
}

fn step_label(st: &Step, idx: usize) -> String {
    st.name.clone().unwrap_or_else(|| format!("#{idx}"))
}

// ---------------------------------------------------------------------------
// Template substitution: {{name}} / {{env.NAME}} / {{1}}..{{9}}
// ---------------------------------------------------------------------------

/// Replace `{{...}}` placeholders in `s` using:
///   * `vars` — named captures from prior steps.
///   * environment — for `{{env.NAME}}`.
///
/// Unknown references render to the empty string.  This intentionally
/// matches Handlebars / Mustache "silent on miss" semantics: a typo
/// in the script never crashes the run mid-step, only produces an
/// expect mismatch which the agent can see.
pub fn substitute(s: &str, vars: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // Find closing `}}`.
            if let Some(end) = find_subseq(&bytes[i + 2..], b"}}") {
                let key = std::str::from_utf8(&bytes[i + 2..i + 2 + end])
                    .unwrap_or("")
                    .trim();
                let value = if let Some(env_key) = key.strip_prefix("env.") {
                    std::env::var(env_key).unwrap_or_default()
                } else {
                    vars.get(key).cloned().unwrap_or_default()
                };
                out.push_str(&value);
                i += 2 + end + 2;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn find_subseq(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------
// Run loop
// ---------------------------------------------------------------------------

/// Execute the script against the given port at `baud`. Emits NDJSON
/// `expect_*` events and the existing `crash_detected` /
/// `crash_context` events. Returns the outcome class.
pub fn run(
    script: Script,
    port: &str,
    baud: u32,
    emitter: &Emitter,
    no_reset: bool,
    no_crash_detect: bool,
) -> Result<Outcome> {
    let mut transport = SerialTransport::open(port, baud)?;
    if !no_reset {
        // Same reasoning as monitor.rs: reset cleanly to app boot so we
        // start the expect run from a known state (CH343 + USB-JTAG
        // gotcha — see docs).
        let _ = reset::reset_to_app(&mut transport);
    }
    run_with_transport(script, &mut transport, emitter, no_crash_detect)
}

/// Pure runtime: execute the script against any `Transport`. Doesn't
/// open ports or reset the chip — caller owns lifecycle.  This is the
/// integration-test entry point (mock transport in unit tests).
pub fn run_with_transport(
    script: Script,
    transport: &mut dyn Transport,
    emitter: &Emitter,
    no_crash_detect: bool,
) -> Result<Outcome> {
    let crash_patterns: Vec<(Regex, &'static str)> = if no_crash_detect {
        Vec::new()
    } else {
        CRASH_PATTERNS
            .iter()
            .map(|(p, kind)| (Regex::new(p).expect("built-in crash regex compiles"), *kind))
            .collect()
    };

    emitter.info(Event::ExpectScriptStart {
        name: script.name.clone(),
        step_count: script.steps.len(),
    });

    let by_name: HashMap<String, usize> = script
        .steps
        .iter()
        .enumerate()
        .filter_map(|(i, st)| st.name.as_ref().map(|n| (n.clone(), i)))
        .collect();

    let mut vars: HashMap<String, String> = HashMap::new();
    let mut line_buf: Vec<u8> = Vec::with_capacity(256);
    // Bytes read from the transport but not yet handed to the current
    // step's matcher — typically the trailing bytes of a multi-line
    // read where the previous step's expect matched on the first
    // newline and we'd otherwise lose the rest. Drained ahead of each
    // new transport read.
    let mut pending: Vec<u8> = Vec::new();
    let mut idx: usize = 0;

    while idx < script.steps.len() {
        let step = script.steps[idx].clone();
        let label = step_label(&step, idx);
        let step_timeout = Duration::from_secs(step.timeout_secs.unwrap_or(script.timeout_secs));

        // 1. send (with template expansion) if any
        let send_preview = if let Some(s) = &step.send {
            let expanded = substitute(s, &vars);
            let preview = preview_for_event(&expanded);
            transport.write(expanded.as_bytes())?;
            Some(preview)
        } else {
            None
        };

        // 2. Determine expected patterns. Pattern strings may carry
        // `{{var}}` templates that reference captures recorded in
        // earlier steps; substitute against `vars` before compiling.
        enum Mode {
            None,
            Single(Regex, String),
            Any(Vec<(Regex, String)>),
        }
        let mode = if !step.expect_any.is_empty() {
            let mut v: Vec<(Regex, String)> = Vec::with_capacity(step.expect_any.len());
            for b in &step.expect_any {
                v.push((compile_pattern(&b.pattern, &vars)?, b.goto.clone()));
            }
            Mode::Any(v)
        } else if let Some(p) = &step.expect {
            Mode::Single(compile_pattern(p, &vars)?, p.clone())
        } else {
            Mode::None
        };
        let expect_not_re = match &step.expect_not {
            Some(p) => Some(compile_pattern(p, &vars)?),
            None => None,
        };

        emitter.info(Event::ExpectStepBegin {
            name: label.clone(),
            send_preview,
            expect_summary: match &mode {
                Mode::None => None,
                Mode::Single(_, p) => Some(p.clone()),
                Mode::Any(v) => Some(
                    v.iter()
                        .map(|(_, g)| format!("→{g}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            },
            timeout_ms: step_timeout.as_millis() as u64,
        });

        // 3. If terminal `ok` set, stop immediately.
        if let Some(ok) = step.ok {
            let outcome = if ok { Outcome::Ok } else { Outcome::ExpectFail };
            emitter.info(Event::ExpectScriptComplete {
                ok,
                steps_run: idx + 1,
                final_step: label,
            });
            return Ok(outcome);
        }

        // 4. If this is a send-only step with nothing to wait for
        // (no expect, no expect_any, no expect_not), there's nothing
        // to read for — advance to the next step immediately rather
        // than burning the step's timeout doing nothing useful. This
        // is the "fire and continue" idiom (e.g. send a setup line
        // whose output is consumed by the next step's expect).
        if matches!(mode, Mode::None) && expect_not_re.is_none() {
            idx += 1;
            continue;
        }

        // 5. Read lines until match / timeout / crash. Re-use the
        //    monitor's line-buffering + crash detection.
        let deadline = Instant::now() + step_timeout;
        let mut buf = [0u8; 1024];
        let mut crash_ctx: Option<(&'static str, Instant, Vec<String>)> = None;
        let outcome: Option<Outcome> = 'wait: loop {
            // Crash-context budget check (matches monitor.rs behaviour).
            if let Some((kind, started_at, _)) = crash_ctx.as_ref() {
                if started_at.elapsed() >= CRASH_CONTEXT_MAX_DURATION {
                    let _ = kind;
                    let (k, _, lines) = crash_ctx.take().unwrap();
                    emitter.error(Event::CrashContext { kind: k, lines });
                    emitter.info(Event::ExpectScriptComplete {
                        ok: false,
                        steps_run: idx + 1,
                        final_step: label.clone(),
                    });
                    return Ok(Outcome::Crash);
                }
            }
            if Instant::now() >= deadline {
                // Flush any partial crash context we'd captured.
                if let Some((k, _, lines)) = crash_ctx.take() {
                    emitter.error(Event::CrashContext { kind: k, lines });
                    emitter.info(Event::ExpectScriptComplete {
                        ok: false,
                        steps_run: idx + 1,
                        final_step: label.clone(),
                    });
                    return Ok(Outcome::Crash);
                }
                emitter.warn(Event::ExpectStepTimeout {
                    name: label.clone(),
                    pattern: match &mode {
                        Mode::None => "<none>".into(),
                        Mode::Single(_, p) => p.clone(),
                        Mode::Any(v) => v
                            .iter()
                            .map(|(_, g)| g.clone())
                            .collect::<Vec<_>>()
                            .join(", "),
                    },
                    timeout_ms: step_timeout.as_millis() as u64,
                });
                break 'wait Some(Outcome::ExpectFail);
            }
            // Refill `pending` from the transport if we drained it.
            // Anything still in `pending` survived from the previous
            // step (or previous read iteration) and gets first crack.
            if pending.is_empty() {
                let remaining = deadline.saturating_duration_since(Instant::now());
                transport.set_timeout(remaining.min(Duration::from_millis(250)))?;
                let n: usize = transport.read(&mut buf).unwrap_or_default();
                if n == 0 {
                    continue;
                }
                pending.extend_from_slice(&buf[..n]);
            }
            // Process pending byte by byte. Index `j` tracks how many
            // we consumed; on a match-break we keep `pending[j..]` for
            // the next step. Without this preservation, a step that
            // matches on the first newline of a multi-line read would
            // silently drop the rest, breaking any "match X then match
            // Y on the next line" script.
            let mut j = 0;
            // Set by the inner match arms; processed after the while
            // exits so we can drain pending first.
            let mut step_outcome: Option<Outcome> = None;
            while j < pending.len() {
                let b = pending[j];
                j += 1;
                if b == b'\r' {
                    continue;
                }
                if b == b'\n' {
                    let line = String::from_utf8_lossy(&line_buf).into_owned();
                    line_buf.clear();
                    emitter.info(Event::SerialLine { line: line.clone() });

                    // Crash detection (skip if disabled or already in a
                    // crash-context window).
                    if crash_ctx.is_none() {
                        for (re, kind) in &crash_patterns {
                            if re.is_match(&line) {
                                emitter.error(Event::CrashDetected {
                                    kind,
                                    pattern: re.as_str().to_string(),
                                    line: line.clone(),
                                });
                                crash_ctx = Some((kind, Instant::now(), vec![line.clone()]));
                                break;
                            }
                        }
                    } else if let Some((_, started_at, ctx)) = crash_ctx.as_mut() {
                        ctx.push(line.clone());
                        let budget_done = ctx.len() >= CRASH_CONTEXT_MAX_LINES
                            || started_at.elapsed() >= CRASH_CONTEXT_MAX_DURATION
                            || CRASH_END_SENTINELS.iter().any(|s| line.contains(s));
                        if budget_done {
                            let (k, _, lines) = crash_ctx.take().unwrap();
                            emitter.error(Event::CrashContext { kind: k, lines });
                            emitter.info(Event::ExpectScriptComplete {
                                ok: false,
                                steps_run: idx + 1,
                                final_step: label.clone(),
                            });
                            return Ok(Outcome::Crash);
                        }
                    }

                    // Negative match (expect_not).
                    if let Some(re) = &expect_not_re {
                        if re.is_match(&line) {
                            emitter.error(Event::ExpectStepNegativeMatch {
                                name: label.clone(),
                                pattern: re.as_str().to_string(),
                                line: line.clone(),
                            });
                            emitter.info(Event::ExpectScriptComplete {
                                ok: false,
                                steps_run: idx + 1,
                                final_step: label.clone(),
                            });
                            return Ok(Outcome::ExpectNotFail);
                        }
                    }

                    // Positive match.
                    match &mode {
                        Mode::None => {
                            // Send-only step — break immediately on
                            // first byte of output (or even no output;
                            // we'll never enter this branch because
                            // step_timeout=0 isn't allowed, but the
                            // step still completes via the deadline).
                            // Practically: send-only steps skip the
                            // wait by virtue of the deadline expiring.
                            // No-op here.
                        }
                        Mode::Single(re, pattern) => {
                            if let Some(caps) = re.captures(&line) {
                                record_captures(re, &caps, &mut vars);
                                run_step_capture_table(&step, &line, &mut vars);
                                emitter.info(Event::ExpectStepMatch {
                                    name: label.clone(),
                                    pattern: pattern.clone(),
                                    line: line.clone(),
                                    captures: snapshot_vars(&vars),
                                });
                                step_outcome = Some(Outcome::Ok);
                                break;
                            }
                        }
                        Mode::Any(branches) => {
                            let mut branched = false;
                            for (re, target) in branches {
                                if let Some(caps) = re.captures(&line) {
                                    record_captures(re, &caps, &mut vars);
                                    run_step_capture_table(&step, &line, &mut vars);
                                    emitter.info(Event::ExpectStepMatch {
                                        name: label.clone(),
                                        pattern: re.as_str().to_string(),
                                        line: line.clone(),
                                        captures: snapshot_vars(&vars),
                                    });
                                    let next = *by_name.get(target).expect("validated");
                                    emitter.info(Event::ExpectStepBranch {
                                        from: label.clone(),
                                        to: target.clone(),
                                    });
                                    idx = next;
                                    branched = true;
                                    break;
                                }
                            }
                            if branched {
                                // idx was reassigned to the goto target;
                                // the dispatch loop picks up from there.
                                step_outcome = Some(Outcome::Ok);
                                break;
                            }
                        }
                    }
                } else {
                    line_buf.push(b);
                }
            }
            // Drop consumed bytes; keep the rest for the next step.
            pending.drain(..j);
            if let Some(o) = step_outcome {
                break 'wait Some(o);
            }
        };

        // Apply outcome for terminal cases. If `Some(Outcome::Ok)` and
        // we matched a branch, `idx` was already updated to the goto
        // target — skip the default `idx += 1`. We detect this by
        // comparing the step we're holding vs the current idx.
        match outcome {
            Some(Outcome::ExpectFail) => {
                emitter.info(Event::ExpectScriptComplete {
                    ok: false,
                    steps_run: idx + 1,
                    final_step: label,
                });
                return Ok(Outcome::ExpectFail);
            }
            Some(Outcome::Crash) => {
                emitter.info(Event::ExpectScriptComplete {
                    ok: false,
                    steps_run: idx + 1,
                    final_step: label,
                });
                return Ok(Outcome::Crash);
            }
            Some(Outcome::ExpectNotFail) => {
                emitter.info(Event::ExpectScriptComplete {
                    ok: false,
                    steps_run: idx + 1,
                    final_step: label,
                });
                return Ok(Outcome::ExpectNotFail);
            }
            Some(Outcome::Ok) => {
                // For branch matches the goto already moved idx; for
                // single-expect matches, we still need to advance to
                // the next step.
                if matches!(mode, Mode::Single(_, _)) || matches!(mode, Mode::None) {
                    idx += 1;
                }
                // For Mode::Any, idx was reassigned by goto resolution.
            }
            _ => {
                idx += 1;
            }
        }
    }

    // Ran off the end without a terminal `ok` step → success.
    emitter.info(Event::ExpectScriptComplete {
        ok: true,
        steps_run: script.steps.len(),
        final_step: "<end>".into(),
    });
    Ok(Outcome::Ok)
}

/// Record capture groups from a regex match into `vars` for later
/// template substitution.
///
///   * Numbered groups → `{{1}}`..`{{N}}`.
///   * Named groups in the regex itself (e.g. `expect = "ip
///     (?P<ip>\\d+\\.\\d+\\.\\d+\\.\\d+)"`) → `{{ip}}`.
///
/// The script's `capture = { name = "pattern" }` table is a separate,
/// more explicit form layered on top of this (see
/// `run_step_capture_table`).
fn record_captures(re: &Regex, caps: &regex::Captures, vars: &mut HashMap<String, String>) {
    for i in 1..caps.len() {
        if let Some(m) = caps.get(i) {
            vars.insert(i.to_string(), m.as_str().to_string());
        }
    }
    for name in re.capture_names().flatten() {
        if let Some(m) = caps.name(name) {
            vars.insert(name.to_string(), m.as_str().to_string());
        }
    }
}

/// Apply the per-step `capture = { name = "pattern" }` table against
/// the matched line and record each into `vars`.
fn run_step_capture_table(step: &Step, line: &str, vars: &mut HashMap<String, String>) {
    for (name, pattern) in &step.capture {
        if let Ok(re) = Regex::new(pattern) {
            if let Some(caps) = re.captures(line) {
                // Pick group 1 by convention; if no group, pick the
                // whole match.
                let val = caps
                    .get(1)
                    .or_else(|| caps.get(0))
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default();
                vars.insert(name.clone(), val);
            }
        }
    }
}

fn snapshot_vars(vars: &HashMap<String, String>) -> HashMap<String, String> {
    vars.clone()
}

/// Render send-payload bytes for the NDJSON event. Trims to the first
/// 80 chars and replaces control bytes with `\xNN` escapes so the
/// event stays one line.
fn preview_for_event(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars().take(80) {
        match ch {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    if s.chars().count() > 80 {
        out.push('…');
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_script() {
        let s = parse(
            r#"
            timeout_secs = 5

            [[step]]
            expect = "ready"
            "#,
        )
        .unwrap();
        assert_eq!(s.steps.len(), 1);
        assert_eq!(s.timeout_secs, 5);
        assert_eq!(s.steps[0].expect.as_deref(), Some("ready"));
    }

    #[test]
    fn rejects_unknown_goto() {
        let err = parse(
            r#"
            [[step]]
            name = "a"
            expect_any = [{ pattern = "X", goto = "nowhere" }]
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("does not exist"), "got: {err}");
    }

    #[test]
    fn rejects_duplicate_step_names() {
        let err = parse(
            r#"
            [[step]]
            name = "a"
            expect = "x"
            [[step]]
            name = "a"
            expect = "y"
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("duplicate step name"), "got: {err}");
    }

    #[test]
    fn rejects_bad_regex() {
        let err = parse(
            r#"
            [[step]]
            expect = "[unclosed"
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("bad expect regex"), "got: {err}");
    }

    #[test]
    fn substitute_env_and_named_and_positional() {
        std::env::set_var("EXPECT_TEST_X", "from_env");
        let mut vars = HashMap::new();
        vars.insert("ip".into(), "10.0.0.1".into());
        vars.insert("1".into(), "first".into());
        let got = substitute("{{env.EXPECT_TEST_X}}|{{ip}}|{{1}}|{{missing}}", &vars);
        assert_eq!(got, "from_env|10.0.0.1|first|");
        std::env::remove_var("EXPECT_TEST_X");
    }

    #[test]
    fn substitute_passes_through_non_template_text() {
        let vars = HashMap::new();
        assert_eq!(substitute("hello {world", &vars), "hello {world");
        assert_eq!(substitute("a {{", &vars), "a {{");
        assert_eq!(substitute("plain", &vars), "plain");
    }

    #[test]
    fn forbids_both_expect_and_expect_any() {
        let err = parse(
            r#"
            [[step]]
            name = "a"
            expect = "X"
            expect_any = [{ pattern = "Y", goto = "a" }]
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("cannot use both expect and expect_any"),
            "got: {err}"
        );
    }

    #[test]
    fn outcome_exit_codes_are_documented() {
        assert_eq!(Outcome::Ok.exit_code(), 0);
        assert_eq!(Outcome::ExpectFail.exit_code(), 40);
        assert_eq!(Outcome::ExpectNotFail.exit_code(), 41);
        assert_eq!(Outcome::Crash.exit_code(), 42);
        assert_eq!(Outcome::ParseError.exit_code(), 43);
    }

    // ----------------------------------------------------------------
    // Runtime tests against a mock transport.
    //
    // The mock has a fixed RX script (bytes esparagus reads from "the
    // chip") and captures TX (bytes esparagus writes "to the chip").
    // Real timing is faked: every read of a non-empty RX returns
    // immediately; reads after RX is drained return 0 (matching the
    // `unwrap_or_default()` no-bytes-available path).
    // ----------------------------------------------------------------

    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crate::transport::Transport;

    struct MockTransport {
        rx: Arc<Mutex<Vec<u8>>>,
        tx: Arc<Mutex<Vec<u8>>>,
    }

    impl Transport for MockTransport {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
            let mut rx = self.rx.lock().unwrap();
            if rx.is_empty() {
                // Match how SerialTransport behaves when no bytes are
                // available within the timeout: error / 0 bytes. The
                // expect loop treats both as "keep polling".
                return Ok(0);
            }
            let n = buf.len().min(rx.len());
            buf[..n].copy_from_slice(&rx[..n]);
            rx.drain(..n);
            Ok(n)
        }
        fn write(&mut self, data: &[u8]) -> Result<()> {
            self.tx.lock().unwrap().extend_from_slice(data);
            Ok(())
        }
        fn set_timeout(&mut self, _t: Duration) -> Result<()> {
            Ok(())
        }
        fn set_baud(&mut self, _baud: u32) -> Result<()> {
            Ok(())
        }
        fn flush_input(&mut self) -> Result<()> {
            self.rx.lock().unwrap().clear();
            Ok(())
        }
        fn flush_output(&mut self) -> Result<()> {
            Ok(())
        }
        fn set_dtr(&mut self, _on: bool) -> Result<()> {
            Ok(())
        }
        fn set_rts(&mut self, _on: bool) -> Result<()> {
            Ok(())
        }
        fn port_name(&self) -> &str {
            "<mock>"
        }
    }

    fn mock_with_rx(bytes: &[u8]) -> (MockTransport, Arc<Mutex<Vec<u8>>>) {
        let rx = Arc::new(Mutex::new(bytes.to_vec()));
        let tx: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        (
            MockTransport {
                rx: rx.clone(),
                tx: tx.clone(),
            },
            tx,
        )
    }

    fn null_emitter() -> Emitter {
        Emitter::new(false, None).expect("create stderr-only emitter")
    }

    #[test]
    fn runtime_matches_single_expect() {
        let script = parse(
            r#"
            timeout_secs = 2
            [[step]]
            expect = "MAIN LOOP READY"
            "#,
        )
        .unwrap();
        let (mut t, _tx) = mock_with_rx(b"booting...\nMAIN LOOP READY\nfoo\n");
        let em = null_emitter();
        let outcome = run_with_transport(script, &mut t, &em, true).unwrap();
        assert_eq!(outcome, Outcome::Ok);
    }

    #[test]
    fn runtime_times_out_on_no_match() {
        let script = parse(
            r#"
            timeout_secs = 1
            [[step]]
            expect = "WILL NEVER ARRIVE"
            timeout_secs = 1
            "#,
        )
        .unwrap();
        let (mut t, _tx) = mock_with_rx(b"hello\nworld\n");
        let em = null_emitter();
        let outcome = run_with_transport(script, &mut t, &em, true).unwrap();
        assert_eq!(outcome, Outcome::ExpectFail);
    }

    #[test]
    fn runtime_sends_with_template_expansion() {
        std::env::set_var("EXPECT_TEST_PW", "s3cret");
        let script = parse(
            r#"
            timeout_secs = 2
            [[step]]
            send = "auth {{env.EXPECT_TEST_PW}}\n"
            expect = "ok"
            "#,
        )
        .unwrap();
        let (mut t, tx) = mock_with_rx(b"ok\n");
        let em = null_emitter();
        let outcome = run_with_transport(script, &mut t, &em, true).unwrap();
        assert_eq!(outcome, Outcome::Ok);
        let written = String::from_utf8(tx.lock().unwrap().clone()).unwrap();
        assert_eq!(written, "auth s3cret\n");
        std::env::remove_var("EXPECT_TEST_PW");
    }

    #[test]
    fn runtime_branches_via_expect_any() {
        let script = parse(
            r#"
            timeout_secs = 2

            [[step]]
            name = "probe"
            send = "status\n"
            expect_any = [
                { pattern = "ERROR", goto = "fail" },
                { pattern = "ready", goto = "win" },
            ]

            [[step]]
            name = "win"
            ok = true

            [[step]]
            name = "fail"
            ok = false
            "#,
        )
        .unwrap();
        let (mut t, _tx) = mock_with_rx(b"booting\nready\n");
        let em = null_emitter();
        let outcome = run_with_transport(script, &mut t, &em, true).unwrap();
        assert_eq!(outcome, Outcome::Ok);
    }

    #[test]
    fn runtime_captures_substitute_into_later_send() {
        // The expect regex names a group `ip`; the next step
        // references {{ip}}. The mock will see the substituted value.
        let script = parse(
            r#"
            timeout_secs = 2

            [[step]]
            expect = "addr (?P<ip>\\d+\\.\\d+\\.\\d+\\.\\d+)"

            [[step]]
            send = "ping {{ip}}\n"
            "#,
        )
        .unwrap();
        let (mut t, tx) = mock_with_rx(b"addr 10.0.0.1\n");
        let em = null_emitter();
        let outcome = run_with_transport(script, &mut t, &em, true).unwrap();
        assert_eq!(outcome, Outcome::Ok);
        let written = String::from_utf8(tx.lock().unwrap().clone()).unwrap();
        // Note: send-only step with no expect advances immediately —
        // the second step doesn't need a reply.
        assert_eq!(written, "ping 10.0.0.1\n");
    }

    #[test]
    fn runtime_expect_not_negative_match() {
        let script = parse(
            r#"
            timeout_secs = 2

            [[step]]
            expect = "FINAL"
            expect_not = "FATAL"
            "#,
        )
        .unwrap();
        let (mut t, _tx) = mock_with_rx(b"warming up\nFATAL panic\nFINAL\n");
        let em = null_emitter();
        let outcome = run_with_transport(script, &mut t, &em, true).unwrap();
        assert_eq!(outcome, Outcome::ExpectNotFail);
    }

    #[test]
    fn runtime_crash_detector_fires() {
        let script = parse(
            r#"
            timeout_secs = 3
            [[step]]
            expect = "WILL NEVER ARRIVE"
            "#,
        )
        .unwrap();
        // "Guru Meditation Error" matches the panic detector; followed
        // by "Rebooting..." which is a crash-end sentinel that flushes
        // the context immediately rather than waiting for the full
        // 5-second context window.
        let (mut t, _tx) = mock_with_rx(b"normal\nGuru Meditation Error\nstack\nRebooting...\n");
        let em = null_emitter();
        // Crash detection ENABLED (third arg = no_crash_detect = false).
        let outcome = run_with_transport(script, &mut t, &em, false).unwrap();
        assert_eq!(outcome, Outcome::Crash);
    }

    #[test]
    fn runtime_expect_pattern_substitutes_captures() {
        // Capture an IP in step 1, then reference it in step 2's
        // EXPECT pattern (not just send). The test asserts the
        // substitution happens at runtime — validate accepts the
        // templated pattern, runtime compiles it after subs.
        let script = parse(
            r#"
            timeout_secs = 2

            [[step]]
            expect = "addr (?P<ip>\\d+\\.\\d+\\.\\d+\\.\\d+)"

            [[step]]
            expect = "ip={{ip}}\\b"
            "#,
        )
        .unwrap();
        let (mut t, _tx) = mock_with_rx(b"addr 10.0.0.1\necho ip=10.0.0.1\n");
        let em = null_emitter();
        let outcome = run_with_transport(script, &mut t, &em, true).unwrap();
        assert_eq!(outcome, Outcome::Ok);
    }

    #[test]
    fn validation_accepts_templated_patterns() {
        // Must validate without a runtime — the template can't be
        // expanded yet so we only check brace balance.
        let s = parse(
            r#"
            [[step]]
            expect = "ip={{ip}}"
            "#,
        );
        assert!(s.is_ok(), "templated expect should validate: {s:?}");
    }

    #[test]
    fn validation_rejects_unbalanced_template_braces() {
        let err = parse(
            r#"
            [[step]]
            expect = "ip={{ip"
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unbalanced"), "got: {err}");
    }

    #[test]
    fn runtime_send_only_step_advances_immediately() {
        // Both steps are send-only with no expect — the script should
        // complete in well under the 30s default timeout. We just
        // assert it returns Ok within a reasonable wall-clock budget.
        let script = parse(
            r#"
            timeout_secs = 30

            [[step]]
            send = "one\n"

            [[step]]
            send = "two\n"
            "#,
        )
        .unwrap();
        let (mut t, tx) = mock_with_rx(b"");
        let em = null_emitter();
        let started = std::time::Instant::now();
        let outcome = run_with_transport(script, &mut t, &em, true).unwrap();
        let elapsed = started.elapsed();
        assert_eq!(outcome, Outcome::Ok);
        assert!(
            elapsed < Duration::from_secs(2),
            "send-only steps should not wait; elapsed = {:?}",
            elapsed
        );
        assert_eq!(
            String::from_utf8(tx.lock().unwrap().clone()).unwrap(),
            "one\ntwo\n"
        );
    }
}
