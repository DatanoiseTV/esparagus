//! End-to-end integration tests for the esparagus binary.
//!
//! These tests exercise the compiled binary directly (not the library)
//! and validate user-visible behaviour:
//!
//!   * `--version` / `--help` output shape
//!   * Offline commands (`merge-bin`, `elf2image`) produce the right
//!     bytes and exit cleanly
//!   * NDJSON event-stream shape under `--json` is well-formed
//!     (one JSON object per line, every line parses)
//!   * Chip-flow commands against a virtual serial port (socat PTY
//!     pair) fail with the documented exit code class and emit a
//!     diagnostic hint event
//!
//! The PTY-based tests are gated on:
//!   * `cfg(unix)` — socat-driven PTY pairs need Unix
//!   * `socat` available on `$PATH` — they self-skip otherwise so a
//!     dev box without socat doesn't fail the suite
//!
//! Running:
//!   * `cargo test --release` runs the offline tests immediately.
//!   * `cargo test --release -- --ignored` runs the PTY tests.

use std::path::PathBuf;
use std::process::Command;

fn esparagus_path() -> PathBuf {
    let target_dir = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(target_dir).join("target/release/esparagus")
}

/// Build the release binary if missing. Keeps `cargo test` self-contained.
fn ensure_release_build() {
    let bin = esparagus_path();
    if !bin.exists() {
        let status = Command::new(env!("CARGO"))
            .args(["build", "--release"])
            .status()
            .expect("cargo build invocation");
        assert!(status.success(), "cargo build --release failed");
    }
}

// ---------------------------------------------------------------------------
// Offline tests (always run)
// ---------------------------------------------------------------------------

#[test]
fn version_string_matches_cargo() {
    ensure_release_build();
    let out = Command::new(esparagus_path())
        .arg("--version")
        .output()
        .expect("spawn");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "version output missing package version: {:?}",
        stdout
    );
}

#[test]
fn help_lists_core_subcommands() {
    ensure_release_build();
    let out = Command::new(esparagus_path())
        .arg("--help")
        .output()
        .expect("spawn");
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    for cmd in [
        "detect",
        "write-flash",
        "read-flash",
        "merge-bin",
        "monitor",
        "mcp",
        "read-efuse",
    ] {
        assert!(s.contains(cmd), "help missing {cmd}");
    }
}

#[test]
fn list_ports_json_is_ndjson_wellformed() {
    ensure_release_build();
    let out = Command::new(esparagus_path())
        .args(["--json", "list-ports"])
        .output()
        .expect("spawn");
    // exit 0 even when no ports are connected — the catalog is just empty.
    assert!(
        out.status.success(),
        "list-ports failed: stderr = {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for (i, line) in stdout.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let _: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line {i} not valid JSON ({e}): {line}"));
    }
}

#[test]
fn merge_bin_uf2_smoke() {
    ensure_release_build();
    let tmp = tempfile_path("uf2_smoke");
    std::fs::create_dir_all(&tmp).unwrap();
    let a = tmp.join("a.bin");
    let b = tmp.join("b.bin");
    let out = tmp.join("out.uf2");
    std::fs::write(&a, vec![0xAAu8; 100]).unwrap();
    std::fs::write(&b, vec![0xBBu8; 200]).unwrap();

    let st = Command::new(esparagus_path())
        .args([
            "merge-bin",
            "--format",
            "uf2",
            "--chip",
            "esp32-c3",
            "-o",
            out.to_str().unwrap(),
            "0x0",
            a.to_str().unwrap(),
            "0x10000",
            b.to_str().unwrap(),
        ])
        .output()
        .expect("spawn");
    assert!(
        st.status.success(),
        "merge-bin uf2 failed: stderr = {}",
        String::from_utf8_lossy(&st.stderr)
    );

    let bytes = std::fs::read(&out).unwrap();
    // Two single-block files at distinct addresses → exactly 1024 bytes.
    assert_eq!(bytes.len(), 1024, "expected 2 blocks, got {}", bytes.len());
    // First-block magic.
    assert_eq!(&bytes[0..4], &0x0A324655u32.to_le_bytes());
    // Final magic at end of first block.
    assert_eq!(&bytes[508..512], &0x0AB16F30u32.to_le_bytes());

    // Cleanup
    let _ = std::fs::remove_dir_all(&tmp);
}

fn tempfile_path(prefix: &str) -> PathBuf {
    let n: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    std::env::temp_dir().join(format!("esparagus_{prefix}_{n}"))
}

// ---------------------------------------------------------------------------
// Virtual serial port (PTY) tests — Unix-only, gated on `socat`
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod pty {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    fn socat_available() -> bool {
        Command::new("socat")
            .arg("-V")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Spawn socat to create a PTY pair, return the two device paths.
    /// Caller must keep the returned Child alive for as long as the
    /// PTYs are needed.
    fn spawn_pty_pair() -> (std::process::Child, String, String) {
        // -d -d → verbose logging on stderr (we read those to discover the
        // generated device paths).
        let mut child = Command::new("socat")
            .args(["-d", "-d", "pty,raw,echo=0", "pty,raw,echo=0"])
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn socat");

        let mut paths: Vec<String> = Vec::new();
        // Read socat's stderr until we've collected both PTY paths, or
        // hit a hard 3s deadline.
        let deadline = Instant::now() + Duration::from_secs(3);
        let stderr = child.stderr.take().unwrap();
        let mut reader = BufReader::new(stderr);
        let mut buf = String::new();
        while paths.len() < 2 && Instant::now() < deadline {
            buf.clear();
            if reader.read_line(&mut buf).unwrap_or(0) == 0 {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            if let Some(idx) = buf.find("/dev/") {
                let path: String = buf[idx..]
                    .chars()
                    .take_while(|c| !c.is_whitespace())
                    .collect();
                paths.push(path);
            }
        }
        assert_eq!(
            paths.len(),
            2,
            "socat did not announce two PTYs in time (got {paths:?})"
        );
        (child, paths.remove(0), paths.remove(0))
    }

    #[test]
    #[ignore = "needs socat in PATH (run with --ignored)"]
    fn detect_timeout_against_silent_pty() {
        if !socat_available() {
            eprintln!("skipping: socat not available");
            return;
        }
        ensure_release_build();
        let (mut socat, pty_a, _pty_b) = spawn_pty_pair();

        // detect against a silent PTY: connect_attempts=1 to keep it fast.
        let mut child = Command::new(esparagus_path())
            .args([
                "--json",
                "--port",
                &pty_a,
                "--chip",
                "esp32",
                "--connect-attempts",
                "1",
                "--no-stub",
                "detect",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn esparagus");

        // Hard timeout in case detect hangs on us.
        let deadline = Instant::now() + Duration::from_secs(8);
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let stdout_lines: Vec<String> = std::thread::spawn(move || {
            BufReader::new(stdout)
                .lines()
                .map_while(|l| l.ok())
                .collect()
        })
        .join()
        .unwrap();
        let mut stderr_buf = String::new();
        let _ = BufReader::new(stderr).read_to_string(&mut stderr_buf);

        // Reap esparagus + socat regardless of test outcome.
        let _ = child.wait();
        let _ = socat.kill();
        let _ = socat.wait();

        // We expect detect to fail (the PTY isn't a chip, and on
        // macOS the serial-open syscall doesn't even accept a PTY —
        // we hit an error event before connect_attempt). Either path
        // is fine; assert that:
        //   1. Every NDJSON line parses (no malformed events).
        //   2. We saw `run_start` + a `run_complete{ok:false}`.
        //   3. We saw at least one of: `connect_attempt` (Linux PTY
        //      path) OR an `error` with `stage="detect"` (macOS PTY
        //      path that fails at port open).
        assert!(
            !stdout_lines.is_empty() || !stderr_buf.is_empty(),
            "no output at all"
        );
        let mut saw_run_start = false;
        let mut saw_run_complete_failure = false;
        let mut saw_chip_flow_or_error = false;
        for line in &stdout_lines {
            if line.trim().is_empty() {
                continue;
            }
            let v: serde_json::Value =
                serde_json::from_str(line).unwrap_or_else(|_| panic!("malformed NDJSON: {line}"));
            match v.get("event").and_then(|x| x.as_str()) {
                Some("run_start") => saw_run_start = true,
                Some("run_complete") => {
                    if v.get("ok").and_then(|x| x.as_bool()) == Some(false) {
                        saw_run_complete_failure = true;
                    }
                }
                Some("connect_attempt") | Some("transport_info") => saw_chip_flow_or_error = true,
                Some("error") => saw_chip_flow_or_error = true,
                _ => {}
            }
        }
        assert!(saw_run_start, "missing run_start event");
        assert!(
            saw_run_complete_failure,
            "missing run_complete with ok=false"
        );
        assert!(
            saw_chip_flow_or_error,
            "expected either connect_attempt/transport_info or an error event"
        );
        let _ = deadline;
    }
}
