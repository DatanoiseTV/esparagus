<img width="600" height="600" alt="esparagus" src="https://github.com/user-attachments/assets/a12e35af-3f7f-4f61-91c1-a86a253dd0b6" />


[![License: GPL-2.0+](https://img.shields.io/badge/License-GPL%202.0%2B-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.88+-orange.svg)](https://www.rust-lang.org)
[![CI](https://github.com/DatanoiseTV/esparagus/actions/workflows/ci.yml/badge.svg)](https://github.com/DatanoiseTV/esparagus/actions/workflows/ci.yml)

**esparagus** is an ESP32-family flasher written in Rust — a faithful behavioral
port of [esptool](https://github.com/espressif/esptool)'s protocol / sync /
reset / stub-loader paths, wrapped in observability that's designed for
**programs, not humans**: structured NDJSON events, machine-readable reports,
stable exit codes, an expect-style serial monitor with built-in panic
detection, and three integration surfaces — direct CLI, an
[agentskills.io](https://agentskills.io) **Agent Skill** for Claude
Code/Cursor/Goose/etc., and an **MCP** (Model Context Protocol) server for
Claude Desktop and other MCP-aware clients.

Hardware behavior is intended to be identical to upstream esptool. The
observability layer is what makes it suitable for an outer loop — Jenkins,
GitHub Actions, an LLM coding agent in a flash-test-fix iteration — to drive
the flasher without parsing English.

---

## When and how to use it

esparagus exposes the same underlying operations through three surfaces. Pick
based on **what's driving it**.

| Surface | Driven by | Best for | Latency per call | What you wire up |
|---|---|---|---|---|
| **Direct CLI** | shells, scripts, Makefiles, GitHub Actions | CI/CD, dev workflows, manual bench work, `idf.py` (via esptool symlink) | none | `esparagus <subcommand>` |
| **Agent Skill** | shell-capable agents (Claude Code, Cursor, Goose, Junie, ...) | LLM agents already running in a terminal context | none (same as CLI) | drop `skills/esparagus/` in the client's skills directory |
| **MCP server** | MCP-aware clients (Claude Desktop, Cursor with MCP, IDE plugins, programmatic agents using the MCP SDK) | clients that don't have shell access; typed tool calls; live notification streaming | ~50ms fork/exec (dominated by chip sync anyway) | `esparagus mcp` over stdio, configured in the client |

**They wrap the same underlying ops** — the protocol layer, chip registry,
reset strategies, stub loader, monitor, NVS reader, partition logic — so a
flash done over MCP is byte-identical to one done from a shell. The choice
is *interface*, not *capability*.

### Direct CLI vs Skill vs MCP — when to pick which

- **Direct CLI** is the universal substrate. Everything that can shell out
  uses it: GitHub Actions, Jenkins, Makefiles, bench tinkering, agents that
  have a Bash tool. If your driver can run a process and parse stdout,
  prefer this.

- **Agent Skill** is documentation, not runtime. It's a folder of
  Markdown — `SKILL.md` plus references — that agentskills.io-compatible
  clients load on demand to teach the LLM how to use the CLI well: exit-code
  semantics, NDJSON event shapes, `--json --report` patterns, pitfalls. The
  agent still runs the CLI via Bash. **Use this when your client has a
  shell tool** — it adds context with zero extra infrastructure.

- **MCP server** is a separate long-running process the client talks to over
  JSON-RPC. Tool schemas are typed (delivered via `tools/list`), live
  events stream as MCP notifications during a call. **Use this when your
  client doesn't have shell access**, or when you want the client to do
  typed-tool-call argument validation, or when the orchestration framework
  is MCP-native (Claude Desktop today, increasingly more tomorrow).

You can — and probably will — use more than one. CI runs the CLI directly;
your bench agent uses the Skill via Claude Code; Claude Desktop uses the MCP
server. Same chip-side behavior across all three.

## Use cases

1. **AI agent firmware feedback loop.** Flash → boot → check → diagnose →
   iterate. `flash-monitor --expect '<app marker>' --timeout 30` writes the
   image, watches the boot log, exits 0 on success or 32 with a structured
   `crash_context` event (panic / WDT / abort / brownout / reboot-loop /
   chip-stuck-in-download) the agent can branch on without parsing English.

2. **GitHub Actions / Jenkins / GitLab CI**: flash a build on a USB-attached
   board in the runner, verify it boots, archive `--report report.json` as a
   pipeline artifact. Stable exit codes mean
   `if status != 0: fail with errors[0].class` Just Works.

3. **`idf.py flash` drop-in.** Symlink `esparagus` as `esptool.py` on
   `$PATH` and existing IDF builds shell out to it transparently
   (`ln -s "$(which esparagus)" ~/.local/bin/esptool.py`). The busybox-style
   compat layer translates the argv (`write_flash` → `write-flash`,
   `default_reset` → `default-reset`, `read_flash` positionals → flags,
   strips unsupported flash-mode/freq/size overrides with a warning).

4. **Field-service backup/restore.** `esparagus backup -o device.bin.gz`
   captures a working device's full flash (auto-sized from JEDEC). `restore`
   replays it onto a replacement board. gzip auto-detect on both sides.

5. **Partition-name-addressed workflows.** No offset math: `write-partition
   --name ota_0 app.bin`, `read-partition --name nvs -o nvs.bin`,
   `erase-partition --name nvs`. Partition table read directly from the
   chip's flash at 0x8000 (or from a `--table partitions.csv`).

6. **NVS inspection.** TUI viewer (`nvs view`) for an interactive look at
   what's in the chip's key/value store, with hex/ASCII detail on each entry.
   `nvs export -o nvs.json` for programmatic access; blob values are
   base64-encoded, multi-chunk blobs are coalesced into single rows.

7. **Bench bring-up.** `list-ports` walks every USB-attached ESP-likely
   device (Espressif native USB at VID `0x303a`, plus the common UART
   bridges — CP210x, CH34x, FTDI), de-dupes the macOS cu./tty. pair,
   surfaces manufacturer/product/serial from USB descriptors. `detect`
   auto-selects the port when exactly one candidate is present.

8. **Offline image generation.** `elf2image` parses 32-bit ELFs (Xtensa or
   RISC-V), extracts and merges PT_LOAD segments, emits a valid v2 ESP
   firmware image with proper header + checksum + SHA256. `merge-bin`
   combines (address, file) pairs into one padded blob — bootloader +
   partition table + app → one distributable image.

9. **Expect-style boot regression tests.** `monitor --expect '<pattern>'
   --expect-not 'FATAL' --timeout 30`. Built-in panic / WDT / abort /
   assert / stack_smash / exception / brownout / cache / download_loop /
   reboot_loop detectors fire `crash_detected` + `crash_context` events
   before your timeout hits, so a failing test gets diagnostic context, not
   just a "no match" timeout.

---

## Install

From source (requires Rust 1.88+):

```sh
cargo install --git https://github.com/DatanoiseTV/esparagus
```

Or clone and build:

```sh
git clone https://github.com/DatanoiseTV/esparagus
cd esparagus
cargo build --release
./target/release/esparagus --help
```

## Direct CLI

Detect the chip and report its identity:

```sh
esparagus detect --port /dev/cu.usbserial-XYZ
# or omit --port to auto-select when only one ESP-likely board is connected
```

Flash binaries at given addresses (compressed transport, per-block MD5
verify, retry on transient failure):

```sh
esparagus write-flash \
  --port /dev/cu.usbserial-XYZ \
  0x0 bootloader.bin \
  0x8000 partition-table.bin \
  0x10000 app.bin
```

Single-shot flash + monitor (the LLM/CI feedback-loop default):

```sh
esparagus flash-monitor \
  --port /dev/cu.usbserial-XYZ \
  --monitor-baud 115200 \
  --expect "boot complete" --timeout 30 \
  0x10000 app.bin
```

`--monitor-baud` handles the common case where the bootloader runs at 460800
but firmware prints at 115200.

Flash by partition name (no offset math — table read from the chip):

```sh
esparagus write-partition --port /dev/cu.usbserial-XYZ --name ota_0 firmware.bin
esparagus read-partition  --port /dev/cu.usbserial-XYZ --name nvs --output nvs.bin
esparagus erase-partition --port /dev/cu.usbserial-XYZ --name nvs
```

Backup / restore the full flash:

```sh
esparagus backup  --port /dev/cu.usbserial-XYZ -o device.bin.gz
esparagus restore --port /dev/cu.usbserial-XYZ device.bin.gz
```

Offline image generation:

```sh
esparagus elf2image --target-chip esp32-s3 \
  --flash-mode dio --flash-freq 80m --flash-size 16MB \
  -o app.bin firmware.elf

esparagus merge-bin -o firmware-flash.bin \
  0x0 bootloader.bin \
  0x8000 partition-table.bin \
  0x10000 app.bin
```

Drop-in `esptool.py` replacement (busybox-style multi-call):

```sh
ln -s "$(which esparagus)" ~/.local/bin/esptool.py
# Now this — and `idf.py flash` — runs esparagus under the hood:
esptool.py --chip esp32-s3 --port /dev/cu.usbserial-XYZ write_flash 0x0 boot.bin 0x10000 app.bin
```

## Agent Skill (Claude Code, Cursor, Goose, ...)

The `skills/esparagus/` directory follows the [agentskills.io](https://agentskills.io)
spec. Drop it into your client's skills folder (typically
`~/.claude/skills/esparagus/` for Claude Code; check your client's docs)
and the agent gets:

- `SKILL.md` — golden rules (always run with `--json --report`), the
  feedback-loop pattern, subcommand cheat sheet, pitfalls (e.g. the
  CH343 + `CONFIG_ESP_CONSOLE_USB_SERIAL_JTAG` gotcha)
- `references/EVENTS.md` — full NDJSON event schema with field names
- `references/EXIT_CODES.md` — codes 0/10/11/12/13/14/15/20/30/31/32 mapped
  to error classes and `next_actions` remediation hints
- `references/EXPECT_PATTERNS.md` — regex flavor + every built-in crash
  pattern, with the per-kind diagnostic walkthrough
- `references/ESPTOOL_COMPAT.md` — busybox-symlink mode for `idf.py flash`

Progressive disclosure: the client loads `SKILL.md` (~250 lines) on
activation; reference files are pulled in only when the agent needs the
detail. The agent then runs `esparagus` via its Bash tool, which is the
universal path that also works in CI.

## MCP server (Claude Desktop, MCP-aware clients)

```sh
esparagus mcp
```

Speaks JSON-RPC 2.0 over stdin/stdout. 18 tools mirror the CLI:
`list_ports`, `detect`, `read_mac`, `flash_id`, `partitions`,
`read_partition`, `write_partition`, `erase_partition`, `write_flash`,
`read_flash`, `erase_flash`, `backup`, `restore`, `monitor`,
`flash_monitor`, `nvs_export`, `reset`, `elf2image`, `merge_bin`.

**Configuration:**

Claude Desktop (`~/Library/Application Support/Claude/claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "esparagus": {
      "command": "/path/to/esparagus",
      "args": ["mcp"]
    }
  }
}
```

Claude Code (CLI):

```sh
claude mcp add --scope user esparagus "$(which esparagus)" mcp
```

**Each `tools/call` spawns a fresh `esparagus` child process** — the serial
port is opened on-demand and released the moment the call returns. Other
processes (`idf.py monitor`, `screen`, a second client) can use the port
between MCP calls; the lockfile + TIOCEXCL make this safe and
deterministic.

**Live event streaming:** during a long-running call like `flash_monitor`,
every NDJSON event the child produces is forwarded as a
`notifications/esparagus/event` MCP notification. Clients that subscribe get
the same firehose a `--json` CLI consumer sees; clients that don't, silently
discard per JSON-RPC notification semantics.

**Tool result content:** two text blocks per call — a one-glance human
summary built from the canonical events (chip, MAC, flash size, MD5,
crash kind, monitor reason), plus a structured JSON dump
(`{exit_code, tool, events[], stderr}`) for typed parsing. The `isError`
flag is set when the child exited non-zero.

## The CI / LLM feedback loop in events

The `--json` flag (and, equivalently, every MCP notification from the
server) emits one JSON object per event:

```json
{"ts":"2026-05-19T12:34:55.998Z","level":"info","event":"run_start","tool":"esparagus 0.1.0","port":"/dev/cu.usbserial-XYZ","baud":460800}
{"ts":"2026-05-19T12:34:56.013Z","level":"info","event":"transport_info","port":"/dev/cu.usbserial-XYZ","usb_vid":"0x1a86","usb_pid":"0x55d3"}
{"ts":"2026-05-19T12:34:56.301Z","level":"info","event":"chip_detected","chip":"ESP32-C5","chip_id":23}
{"ts":"2026-05-19T12:34:56.523Z","level":"info","event":"stub_running","chip":"ESP32-C5","blob":"esp32c5","entry":"0x40800000"}
{"ts":"2026-05-19T12:34:56.610Z","level":"info","event":"baud_upgrade","from":115200,"to":460800}
{"ts":"2026-05-19T12:34:57.115Z","level":"info","event":"write_progress","addr":"0x00010000","written":65536,"total":1048576,"pct":6.25}
{"ts":"2026-05-19T12:35:01.000Z","level":"info","event":"md5_verified","addr":"0x00010000","size":1048576,"md5":"f4af..."}
{"ts":"2026-05-19T12:35:01.420Z","level":"info","event":"run_complete","ok":true,"duration_ms":5422}
```

The `--report path.json` flag writes a structured summary:

```json
{
  "ok": false,
  "tool": "esparagus 0.1.0",
  "duration_ms": 5012,
  "chip": "ESP32-C5",
  "transport": { "port": "/dev/cu.usbserial-XYZ", "baud": 460800 },
  "stages": [
    { "name": "connect", "ok": true, "ms": 320, "attempts": 1 },
    { "name": "stub_upload", "ok": false, "ms": 4200,
      "detail": "stub handshake failed (timeout)" }
  ],
  "errors": [{ "stage": "stub_upload", "class": "stub_handshake",
               "detail": "expected OHAI ..." }],
  "next_actions": [{
    "kind": "use_no_stub",
    "desc": "Stub failed to start. Retry with --no-stub for slower but more compatible operation."
  }]
}
```

The `class` strings and `next_actions[].kind` strings are stable. An agent
or CI script can branch on them without reading the English `detail`.

## Exit codes

| Code | Meaning |
|------|---------|
| 0    | Success (monitor: `--expect` matched, or `--timeout` reached with no patterns set) |
| 1    | Generic failure |
| 2    | CLI / usage error |
| 10   | Could not open serial port |
| 11   | Failed to sync with chip |
| 12   | Chip mismatch with `--chip` |
| 13   | Flash op failed (write/erase/read/MD5 mismatch) |
| 14   | Stub loader upload or handshake failed |
| 15   | Port held by another process (lockfile or TIOCEXCL) |
| 20   | Image header invalid |
| 30   | Monitor `--expect-not` pattern matched |
| 31   | Monitor `--timeout` reached without an `--expect` match |
| 32   | Monitor detected an ESP crash (panic / WDT / abort / reboot_loop / download_loop / ...) |

## Subcommand reference

| Command | Needs port | Purpose |
|---|---|---|
| `list-ports` | no | List ESP-likely USB devices (with USB descriptors and bridge classification) |
| `detect` | yes (auto-selects) | Identify chip, read MAC + flash ID |
| `read-mac` / `flash-id` | yes | Individual reads |
| `erase-flash` / `erase-region` | yes | Erase (stub-only) |
| `write-flash` | yes | Write `(addr, file)` pairs (compressed + MD5-verified) |
| `read-flash` | yes | Dump a flash region |
| `reset` | yes | Hard-reset via EN |
| `partitions` | yes | Read partition table (CSV or from chip) |
| `write-partition` / `read-partition` / `erase-partition` | yes | Name-addressed partition ops |
| `backup` / `restore` | yes | Full-flash dump / replay (gzip-aware) |
| `monitor` | yes | Serial monitor with expect / built-in crash detection |
| `flash-monitor` | yes | `write-flash` + `monitor` in one command |
| `nvs view` / `nvs export` | yes (or `--from-file`) | NVS partition inspection (TUI or JSON) |
| `elf2image` | no | ELF → ESP firmware image |
| `merge-bin` | no | Combine bins into one padded image |
| `mcp` | no | Run as an MCP server over stdio |

## Architecture (brief)

- `src/protocol/` — SLIP framing, ESP serial-protocol commands, sync
- `src/transport/` — Transport trait, serialport-rs backend, flock + TIOCEXCL port locking
- `src/reset.rs` — Classic, UnixTight, USBJTAGSerial, HardReset, reset_to_app
- `src/chip.rs` — Per-chip registry (magic numbers, SPI regs, EFUSE, watchdog, FORCE_DOWNLOAD_BOOT)
- `src/stub.rs` — Stub uploader + bundled [esp-flasher-stub](https://github.com/espressif/esp-flasher-stub) blobs
- `src/ops.rs` — Flash / erase / read / MD5 ops
- `src/partition.rs` — CSV + binary partition-table parser
- `src/imagegen.rs` — Offline ELF parsing + image build
- `src/nvs.rs` — NVS v2 partition reader, blob coalescing
- `src/tui.rs` — Ratatui-based NVS viewer (table + hex/ASCII detail)
- `src/monitor.rs` — Expect-style monitor + built-in crash detectors
- `src/discover.rs` — Cross-platform USB serial device discovery
- `src/observe.rs` — NDJSON emitter, report builder, diagnostic hint engine
- `src/esptool_compat.rs` — Busybox-style argv translation
- `src/mcp.rs` — MCP server (JSON-RPC 2.0 over stdio)
- `src/cli.rs` + `src/runner.rs` — CLI parsing + orchestration

See `docs/STATUS.md` for the per-chip bench-validation matrix and the
list of features intentionally not yet implemented (eFuse burn, secure
boot signing, flash encryption, NAND).

## Provenance and licensing

esparagus is a Rust port of esptool's protocol implementation. Because it
contains a derived translation of GPL-2.0+ code, the whole work is
distributed under **GPL-2.0-or-later** to match upstream.

The bundled flasher stub binaries in `stubs/` come from
[esp-flasher-stub v0.7.0](https://github.com/espressif/esp-flasher-stub) and
are dual licensed Apache-2.0 OR MIT (see `stubs/LICENSE-APACHE`,
`stubs/LICENSE-MIT`, and the top-level `NOTICE`).

## Contributing

`CONTRIBUTING.md` has the bug-report shape and chip-adding playbook. Bug
reports should include the full NDJSON stream
(`--json --log-file out.ndjson`) and the report file
(`--report out.json`) — that's the shape esparagus was designed to produce.
