---
name: esparagus
description: Flash, read, erase, verify, and serial-monitor ESP32-family chips (ESP32, S2, S3, C2, C3, C5, C6, H2, P4) with structured NDJSON output, a GNU-expect-style monitor, and stable machine-readable exit codes. Use for any ESP32 firmware task an agent runs from the command line — flashing builds, reading partitions or NVS, backing up / restoring flash, running expect-based boot or regression checks, or implementing a flash-test-fix feedback loop. Always invoke with `--json --report path.json` so the agent can parse one JSON event per line on stdout and a structured final report on disk. Exit codes are stable (0 success, 30 expect-not match, 31 timeout without expect match, 32 detected ESP panic / watchdog / abort), so the agent can branch on the run outcome without reading English. Works as a drop-in busybox-style replacement for `esptool.py` via symlink, so `idf.py flash` keeps working too.
license: GPL-2.0-or-later
compatibility: Requires a USB-attached ESP32-family chip and a serial port. The `nvs view` subcommand needs an interactive terminal; everything else is fine in CI / headless / piped contexts.
metadata:
  homepage: https://github.com/DatanoiseTV/esparagus
  language: rust
---

# esparagus — agent usage guide

esparagus is an ESP32-family flasher written in Rust. It does the same
chip-side work as upstream `esptool.py` (faithful protocol port + stub
loader) but is built around being driven by another program — a CI
pipeline, an LLM agent, a test harness. Two things make it good at
that job: **structured output** (NDJSON event stream + final JSON
report) and **stable exit codes** for the common branching cases.

Read this whole file once when you pick up an esparagus task. The
files under `references/` exist for when you need detail you don't
have here — load them on demand, not preemptively.

## Golden rule: run with `--json --report`

In every non-interactive invocation, pass these two flags **first**:

```bash
esparagus --port <PORT> --json --report /tmp/esparagus-report.json <subcommand> ...
```

- `--json` switches stdout from human prose to **one JSON event per
  line** (NDJSON). You can `tail`, `jq`, or feed it to a model.
- `--report PATH` writes a single structured JSON document at the end
  with per-stage timings, errors keyed by stable `class` strings, and
  machine-readable `next_actions` remediation hints.

If you forget `--json`, you'll get human-formatted lines on stderr
which are not stable to parse. If you forget `--report`, you can
still decide the next step from the exit code and the event stream,
but the hint engine's `next_actions` won't be available.

## Required global flags

| Flag | Purpose |
|------|---------|
| `--port /dev/...` | If omitted **and** exactly one ESP-likely USB serial device is present, esparagus auto-selects it and emits `auto-selected port ...` on stderr. If multiple are present, the run aborts with exit 2 and a list of candidates. Use `list-ports` to see what's visible. Mandatory if you want to skip the auto-select, and ignored entirely for the offline `elf2image` / `merge-bin` / `list-ports` subcommands. |
| `--baud 460800` | Default; lower (115200) if you see CRC / MD5 mismatches. The bootloader sync always happens at 115200 internally and is upgraded after the stub starts. |
| `--chip esp32-s3` | Optional. If omitted, esparagus auto-detects via `GET_SECURITY_INFO` or chip-magic register. Set it only when you specifically want to abort on the wrong chip. |
| `--json` | Emit NDJSON on stdout. **Always set this.** |
| `--report PATH` | Write final structured report. **Always set this.** |
| `--log-file PATH` | Mirror every event to a file in NDJSON regardless of stdout mode. |

## The two flows you care about

### Flow 1: flash and verify it booted (the feedback loop)

The single most useful pattern. Replaces a `esptool write_flash && reset && monitor` shell chain with one command:

```bash
esparagus --port /dev/cu.usbserial-XYZ --json --report /tmp/r.json \
  flash-monitor \
    --monitor-baud 115200 \
    --expect 'I \(\d+\) cpu_start' \
    --timeout 30 \
    0x10000 build/app.bin
```

- `flash-monitor` writes the files (same syntax as `write-flash`),
  then drops directly into a serial monitor.
- `--monitor-baud` is for the (common) case where firmware logs at a
  different baud than the bootloader (e.g. flash at 460800, app
  prints at 115200). Defaults to the global `--baud`.
- `--expect REGEX` is a Rust regex. Match → success → exit **0**.
  Multiple `--expect` flags = OR.
- `--expect-not REGEX` (also repeatable) = fail-fast on a match →
  exit **30**.
- `--timeout SECS` = hard ceiling. Without an `--expect` match → exit
  **31**.
- The built-in crash detector watches for Guru Meditation, task
  watchdog, abort(), assert failed, stack-smashing, CPU exceptions,
  cache misuse, and brownout. Hit → exit **32**, with the full
  panic + backtrace captured into a `crash_context` NDJSON event.

For details on regex syntax and crash patterns, see
`references/EXPECT_PATTERNS.md`.

### Flow 2: partition-name-addressed read/write (no offset math)

Instead of looking up the partition offset in the user's partition
table CSV, esparagus reads the binary partition table from the chip's
flash at 0x8000 and resolves names directly:

```bash
# Inspect what's there
esparagus --port <PORT> --json partitions

# Flash a file into the partition named ota_0
esparagus --port <PORT> --json --report /tmp/r.json \
  write-partition --name ota_0 build/app.bin

# Read the nvs partition out
esparagus --port <PORT> --json read-partition --name nvs -o /tmp/nvs.bin

# Erase nvs
esparagus --port <PORT> --json erase-partition --name nvs
```

If the chip's table is missing or you want to pin a known layout,
pass `--table partitions.csv` (IDF-format CSV).

## Subcommand cheat sheet

| Subcommand | Needs port | What it does |
|---|---|---|
| `detect` | yes | Identify chip + MAC + flash ID. Use this first when you don't know what's connected. **Note: the subcommand is `detect`, not `chip-id` or `chip_id`** — though those are accepted as aliases. |
| `list-ports` | **no** | Walk the OS serial port list (IORegistry on macOS, udev/sysfs on Linux, WMI on Windows) and print ESP-likely devices: Espressif-native USB (VID 0x303A) plus the common UART bridges (CP210x, CH34x, FTDI). Includes manufacturer / product / serial number from the USB descriptors when the OS exposes them. De-duplicates the macOS cu./tty. variants. |
| `read-mac` | yes | Read base MAC from EFUSE. |
| `flash-id` | yes | SPI flash JEDEC ID + decoded size (MB). |
| `erase-flash` | yes | Erase the entire chip. **Destructive.** |
| `erase-region` | yes | Erase a sector-aligned region. |
| `write-flash` | yes | Write `<addr> <file>` pairs. Same flag syntax as esptool. |
| `read-flash` | yes | Dump a region to a file. `--address` `--size` `--output`. |
| `reset` | yes | Hard reset via EN line. |
| `partitions` | yes | Show partition table (from chip flash or `--table CSV`). |
| `write-partition --name X` | yes | Resolve partition name, then write. |
| `read-partition --name X -o F` | yes | Resolve partition name, then dump. |
| `erase-partition --name X` | yes | Resolve partition name, then erase region. |
| `backup -o file[.gz]` | yes | Dump entire flash. Auto-size from JEDEC. `.gz` extension = gzip. |
| `restore file[.gz]` | yes | Write a backup back to flash. Auto-decompresses `.gz`. |
| `monitor` | yes | Serial monitor with `--expect`/`--expect-not`/`--timeout` and built-in crash detection. **Defaults to 115200 baud** (the ESP-IDF app-console default); override with `--monitor-baud`. |
| `flash-monitor` | yes | `write-flash` + `monitor` in one command. The feedback-loop default. Monitor phase also defaults to 115200. |
| `expect <script.toml>` | yes | Run a scripted send/expect/branches/captures flow (better-than-GNU-`expect`). One NDJSON event per step; same crash detectors as `monitor`; templates pull from `{{env.X}}` / captures / `{{1}}`–`{{9}}`. See `references/EXPECT_SCRIPTS.md`. `--check` validates without a port. |
| `read-efuse` | yes | Dump EFUSE BLOCK0+BLOCK1 as words + decoded MAC + (P4 only today) silicon revision. Read-only — burn is intentionally out of scope. |
| `completions <shell>` / `man` | **no** | Emit shell-completion or roff man-page source on stdout. |
| `elf2image` | **no** | Offline: ELF → ESP firmware image. |
| `merge-bin` | **no** | Offline: combine `<addr> <file>` pairs into a single padded image. |
| `nvs view` | yes (or `--from-file`) | Interactive TUI for the NVS partition. Not suitable for piped/CI use. |
| `nvs export --output F` | yes (or `--from-file`) | Read NVS, write all items as JSON. Use this in scripts. |

## Exit codes (branch on these)

| Code | Meaning | Typical next action |
|---|---|---|
| 0 | Success (or monitor `--expect` matched) | Continue. |
| 1 | Generic failure | Read `report.errors[0]` for the `class` string. |
| 2 | CLI / usage error | Fix the invocation. |
| 10 | Could not open the port | Check the device path / permissions. |
| 11 | Failed to sync with chip | Reset manually, lower baud, or retry. |
| 12 | Chip mismatch with `--chip` flag | Remove `--chip` or set the correct one. |
| 13 | Flash op failed (write / MD5 mismatch / read error) | Lower baud and retry; check power. |
| 14 | Stub loader upload or handshake failed | Try `--no-stub` for slower-but-safer ROM-only. |
| 15 | Port held by another process | Another `esparagus` is racing (flock held), or `screen` / `minicom` / debugger has the OS fd via TIOCEXCL. Wait, kill the other consumer, or close the other terminal. |
| 20 | Image header invalid | Image is for a different chip, or corrupted. |
| 30 | Monitor `--expect-not` pattern matched | Firmware emitted a forbidden line. Inspect log. |
| 31 | Monitor timed out without an `--expect` match | Firmware didn't reach the expected state in time. |
| 32 | Monitor detected an ESP panic / WDT / abort | Read the `crash_context` event for the full backtrace. |
| 40 | `expect` script step timed out (or hit `ok = false`) | Read the failing step name from `expect_step_timeout` / `expect_script_complete`. |
| 41 | `expect` script `expect_not` pattern matched | Forbidden line appeared. Inspect the `expect_step_negative_match` event. |
| 42 | `expect` script crash detector fired | Same `crash_context` shape as `monitor` exit 32. |
| 43 | `expect` script failed validation | Bad regex, unknown `goto`, duplicate step name, etc. Run with `--check` on the local machine to iterate fast. |

Full mapping at `references/EXIT_CODES.md`.

## Reading the NDJSON event stream

Every event has these three top-level fields:

```json
{"ts":"2026-05-18T13:36:25.787Z","level":"info","event":"<event_name>", ...}
```

The `event` discriminator is stable. Branch on it. Common events you
will see in order during a typical `flash-monitor` run:

```
run_start                     # start of run
transport_info                # VID/PID detected
connect_attempt               # reset strategy + attempt N
connected                     # synced with chip
chip_detected                 # chip name + image_chip_id
stub_upload_start             # uploading stub blob
stub_running                  # OHAI received
baud_upgrade                  # rate-bumped to user --baud
flash_id_read                 # JEDEC + size
write_begin / write_progress  # per-block progress
md5_verified                  # device-side MD5 matched host MD5
reset_issued                  # hard reset before monitor handoff
monitor_start                 # opening for the monitor phase
serial_line                   # one decoded line from the chip
expect_match                  # --expect or --expect-not hit
crash_detected                # built-in pattern matched
crash_context                 # gathered backtrace lines
monitor_complete              # reason="expect_match"|"expect_not_match"|"timeout"|"crash"
run_complete                  # final ok/duration
error                         # any failure; carries `class` + `detail`
```

Full event schema with field names at `references/EVENTS.md`.

When something fails, the final report's `next_actions` array tells
you what to try. Each entry has a stable `kind` (e.g.
`manual_bootloader`, `lower_baud`, `use_no_stub`, `use_stub`,
`check_cable`, `udev_group`, `check_port`, `fix_chip_flag`,
`retry_lower_baud`, `check_psu`, `check_image`) and a human `desc`.

## Pitfalls and gotchas

- **`monitor` resets the chip by default** so you see the boot log
  from byte 0. Add `--no-reset` if you intentionally want to watch
  an already-running firmware.
- **Default baud is 460800.** Most ESP32-family chips are happy at
  this rate via UART bridges. If you hit `md5_mismatch` errors,
  retry with `--baud 115200`.
- **macOS port paths**: prefer `/dev/cu.usbserial-XYZ` over
  `/dev/tty.usbserial-XYZ`. esparagus handles both, but `cu.*` is
  the right one for outgoing serial.
- **`nvs view` is interactive** — do not call it from a script or
  piped context; you'll just print escape sequences. For programmatic
  NVS access, use `nvs export -o file.json` and parse the JSON.
- **NVS is currently read-only.** Writing back is deferred; see
  `docs/STATUS.md` in the source tree for the rationale.
- **`merge-bin` and `elf2image` don't open a port.** They don't need
  `--port`. They're file-only operations.
- **`flash-monitor` skips the post-flash reset.** Its monitor phase
  does its own `reset_to_app` sequence (DTR=false first, then RTS
  pulse) which avoids the GPIO0-stuck-low download-mode trap that
  caught the bench session.

- **`reboot_loop` ≠ chip is broken — sometimes the console is going
  somewhere else.** If the `transport_info` event reports a USB
  VID/PID *other* than Espressif's `0x303a` (i.e. you're talking to
  the chip through a CH340/CH343 `0x1a86`, CP210x `0x10c4`, or FTDI
  `0x0403` bridge), and the build's `sdkconfig` has
  `CONFIG_ESP_CONSOLE_USB_SERIAL_JTAG=y`, every `printf` / `ESP_LOG`
  is being routed to the chip's *unused native USB peripheral*, not
  out UART0 to the bridge. The chip runs fine, but esparagus's
  monitor sees zero app output and the detector reports
  `reboot_loop`. Fix is in IDF: switch to
  `CONFIG_ESP_CONSOLE_UART=y` (or `=DEFAULT`). Cross-validate by
  comparing `transport_info.usb_vid` against the chip's native VID
  (always `0x303a`) before recommending firmware changes.

## When NOT to use esparagus

- **eFuse burn**: not implemented (read planned). Use `espefuse.py`.
- **Secure boot signing**: not implemented. Use `espsecure.py`.
- **Flash encryption**: not implemented.
- **NAND flash**: not implemented.
- **Image generation features beyond elf2image / merge_bin**
  (image_info, verify_flash, dump_mem, load_ram, read_mem, write_mem,
  make_image, summary): not implemented. Use upstream `esptool.py`.

The busybox-style esptool-compat layer (see
`references/ESPTOOL_COMPAT.md`) hard-errors on these so a misrouted
`idf.py` invocation fails fast and clear instead of doing the wrong
thing silently.

## MCP server mode

esparagus can also be driven over the **Model Context Protocol** —
useful for clients that natively speak MCP (Claude Desktop, some
Cursor configurations, IDE plugins). Start with:

```sh
esparagus mcp
```

It reads JSON-RPC 2.0 from stdin, writes responses + notifications to
stdout, supports `initialize`, `tools/list`, `tools/call`, and `ping`.
Each `tools/call` spawns a fresh `esparagus` child process so the
serial port is opened on demand and released immediately after —
other processes can keep using the port between calls.

The MCP tool set mirrors the CLI: `list_ports`, `detect`, `read_mac`,
`flash_id`, `partitions`, `read_partition`, `write_partition`,
`erase_partition`, `write_flash`, `read_flash`, `erase_flash`,
`backup`, `restore`, `monitor`, `flash_monitor`, `nvs_export`,
`reset`, `elf2image`, `merge_bin`. Per-tool input schemas are
delivered via `tools/list` — there's no separate schema file to keep
in sync.

Mid-call, the server streams every NDJSON event from the child as a
`notifications/esparagus/event` notification, so an MCP client that
subscribes gets the same live event firehose a `--json` CLI consumer
sees. The final `tools/call` result contains:
- a human-readable summary (chip, MAC, flash size, exit reason)
- a structured JSON dump (`exit_code`, `tool`, `events[]`, `stderr`)
- `isError: true` when the child exited non-zero

For agents that already drive the CLI via Bash, the MCP path adds
little — the CLI + NDJSON + Skill already covers it. MCP earns its
keep with clients that don't have shell access or want typed tool
calls without re-implementing argv construction.

## References

- `references/EVENTS.md` — every NDJSON event and its fields.
- `references/EXIT_CODES.md` — full exit code table + diagnostic
  `class` → `next_actions` mapping.
- `references/EXPECT_PATTERNS.md` — regex syntax for `--expect` /
  `--expect-not` and the built-in crash detector patterns.
- `references/ESPTOOL_COMPAT.md` — how esparagus impersonates
  `esptool.py` via symlink for `idf.py flash` compatibility.
- `assets/partitions-example.csv` — IDF-format partition table you
  can pass to `--table`.
