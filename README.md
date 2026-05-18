<img width="600" height="600" alt="image" src="https://github.com/user-attachments/assets/a12e35af-3f7f-4f61-91c1-a86a253dd0b6" />


[![License: GPL-2.0+](https://img.shields.io/badge/License-GPL%202.0%2B-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75+-orange.svg)](https://www.rust-lang.org)
[![CI](https://github.com/DatanoiseTV/esparagus/actions/workflows/ci.yml/badge.svg)](https://github.com/DatanoiseTV/esparagus/actions/workflows/ci.yml)

**A Rust port of [esptool](https://github.com/espressif/esptool) with
structured observability for CI/CD and LLM-driven feedback loops.**

If you've ever tried to wire an automated system — Jenkins, GitHub Actions,
or an AI agent — around `esptool flash`, you know the friction: prose mixed
with progress bars on stdout, errors hide useful state behind English
strings, no machine-readable way to extract *"what happened, what broke,
what to try next."* esparagus emits everything as an NDJSON event stream,
writes a structured final report you can `jq` or feed to a model, and
pairs every failure with a remediation hint your outer loop can act on.

Underneath the observability layer, esparagus is a behavioral port of
esptool's protocol, sync, reset, chip detection, and stub loader paths —
so hardware behavior is intended to be identical to upstream.

---

## Why esparagus?

| | esptool (Python) | esparagus (Rust) |
|---|---|---|
| Single static binary | ❌ (Python install required) | ✅ |
| Structured (NDJSON) stdout | ❌ | ✅ |
| Machine-readable final report | ❌ | ✅ |
| Remediation hints on failure | ❌ | ✅ (stable `next_actions` keys) |
| Stable exit codes | partial | ✅ (documented per failure class) |
| File-level compression for backups | ❌ | ✅ (gzip auto-detect) |
| Faithful protocol port | — | ✅ (same SLIP/sync/reset/stub flow) |

esparagus isn't trying to replace `esptool` for human-interactive use. It's
trying to be the right tool when *another program* drives the flasher.

## Install

From source (requires Rust 1.75+):

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

## Supported chips

ESP32, ESP32-S2, ESP32-S3, ESP32-C2, ESP32-C3, ESP32-C5, ESP32-C6,
ESP32-H2, ESP32-P4. See `docs/STATUS.md` for which combinations of
(chip, operation) have been bench-validated on real hardware versus
unit-tested only.

## Quick examples

Detect the chip, read its MAC + flash JEDEC ID:

```sh
esparagus detect --port /dev/cu.usbserial-XYZ
```

Flash one or more binaries at given addresses (with on-wire compression,
per-block MD5 verification, retry on transient failure):

```sh
esparagus write-flash \
  --port /dev/cu.usbserial-XYZ \
  0x0 bootloader.bin \
  0x8000 partition-table.bin \
  0x10000 app.bin
```

Flash by partition name — esparagus reads the partition table from the
chip (or from a `--table partitions.csv`) and resolves the offset for
you:

```sh
esparagus write-partition --port /dev/cu.usbserial-XYZ \
  --name ota_0 firmware.bin
```

Back up the entire flash (auto-detects size from the SPI flash JEDEC
capacity byte; `.gz` suffix transparently gzips the dump):

```sh
esparagus backup --port /dev/cu.usbserial-XYZ -o device-backup.bin.gz
esparagus restore --port /dev/cu.usbserial-XYZ device-backup.bin.gz
```

Build an image offline from an ELF (no chip needed):

```sh
esparagus elf2image --target-chip esp32-s3 \
  --flash-mode dio --flash-freq 80m --flash-size 16MB \
  -o app.bin firmware.elf
```

Merge bootloader + partition table + app into one flash image:

```sh
esparagus merge-bin -o firmware-flash.bin \
  0x0 bootloader.bin \
  0x8000 partition-table.bin \
  0x10000 app.bin
```

Monitor the chip after flashing — GNU-expect-style. Exits 0 on first
positive match, 30 on negative match, 31 on timeout, 32 on detected
ESP panic / watchdog / abort (with the surrounding backtrace captured
as a `crash_context` NDJSON event):

```sh
esparagus write-flash --after no-reset --port /dev/cu.usbserial-XYZ \
  0x10000 app.bin

esparagus monitor --port /dev/cu.usbserial-XYZ \
  --expect "boot complete" \
  --expect-not "FATAL" \
  --timeout 30
```

Just stream the serial output (no patterns → exit 0 at the timeout):

```sh
esparagus monitor --port /dev/cu.usbserial-XYZ
```

One-shot flash + monitor (the LLM/CI loop's bread and butter):

```sh
esparagus flash-monitor --port /dev/cu.usbserial-XYZ \
  --monitor-baud 115200 \
  --expect "boot complete" --timeout 30 \
  0x10000 app.bin
```

`--monitor-baud` is for the common case where the bootloader runs at
460800 but firmware prints at 115200.  Defaults to the global `--baud`
if omitted.

The crash detector recognises Guru Meditation, task watchdog, abort,
assert, stack-smashing, the Xtensa / RISC-V CPU exceptions
(LoadProhibited, StoreProhibited, IllegalInstruction, ...), the
"cache disabled" trap, and brownout. On a hit it captures the next
~200 lines or up to a `Rebooting...` sentinel as full crash context.

## The CI / LLM feedback loop

The `--json` flag turns stdout into a stream of one-JSON-per-line events
that any agent can `tail | jq`:

```json
{"ts":"2026-05-18T12:34:55.998Z","level":"info","event":"run_start","tool":"esparagus 0.1.0","port":"/dev/cu.usbserial-XYZ","baud":460800}
{"ts":"2026-05-18T12:34:56.013Z","level":"info","event":"transport_info","port":"/dev/cu.usbserial-XYZ","usb_vid":"0x1a86","usb_pid":"0x55d3"}
{"ts":"2026-05-18T12:34:56.301Z","level":"info","event":"chip_detected","chip":"ESP32-P4","chip_id":18}
{"ts":"2026-05-18T12:34:56.523Z","level":"info","event":"stub_running","chip":"ESP32-P4","blob":"esp32p4-rev1","entry":"0x4ff10000"}
{"ts":"2026-05-18T12:34:56.610Z","level":"info","event":"baud_upgrade","from":115200,"to":460800}
{"ts":"2026-05-18T12:34:57.115Z","level":"info","event":"write_progress","addr":"0x00010000","written":65536,"total":1048576,"pct":6.25}
{"ts":"2026-05-18T12:35:01.000Z","level":"info","event":"md5_verified","addr":"0x00010000","size":1048576,"md5":"f4af..."}
{"ts":"2026-05-18T12:35:01.420Z","level":"info","event":"run_complete","ok":true,"duration_ms":5422}
```

And the `--report path.json` flag writes a structured summary with
per-stage timings, every error, and machine-readable remediation hints:

```json
{
  "ok": false,
  "tool": "esparagus 0.1.0",
  "started_at": "2026-05-18T12:34:55.998Z",
  "duration_ms": 5012,
  "chip": "ESP32-P4",
  "transport": { "port": "/dev/cu.usbserial-XYZ", "baud": 460800 },
  "stages": [
    { "name": "connect", "ok": true, "ms": 320, "attempts": 1 },
    { "name": "stub_upload", "ok": false, "ms": 4200,
      "detail": "stub handshake failed (timeout)" }
  ],
  "errors": [
    { "stage": "stub_upload", "class": "stub_handshake",
      "detail": "expected OHAI ..." }
  ],
  "next_actions": [
    { "kind": "use_no_stub",
      "desc": "Stub failed to start. Retry with --no-stub for slower but more compatible operation." }
  ]
}
```

The `class` strings are stable, so a model or a CI script can branch on
them without reading the English `detail`. See `src/observe.rs::hints`
for the full mapping.

## Exit codes

| Code | Meaning |
|------|---------|
| 0    | Success |
| 1    | Generic failure |
| 2    | CLI/usage error |
| 10   | Could not open serial port |
| 11   | Failed to sync with chip |
| 12   | Chip mismatch (`--chip` flag) |
| 13   | Flash operation failed (write/erase/read/MD5 mismatch) |
| 14   | Stub loader upload or handshake failed |
| 20   | Image header invalid |
| 30   | Monitor `--expect-not` pattern matched |
| 31   | Monitor `--timeout` reached without an `--expect` match |
| 32   | Monitor detected an ESP crash (panic / WDT / abort / ...) |

## Subcommand summary

| Command | Needs port | Purpose |
|---|---|---|
| `detect` | yes | Identify chip, read MAC + flash ID |
| `read-mac` | yes | Read BASE_MAC from EFUSE |
| `flash-id` | yes | SPI flash JEDEC ID (mfr, dev, size) |
| `erase-flash` | yes | Erase entire chip (stub-only) |
| `erase-region` | yes | Erase a sector-aligned region (stub-only) |
| `write-flash` | yes | Write `(addr, file)` pairs to flash |
| `read-flash` | yes | Dump a flash region to file (stub-only) |
| `reset` | yes | Hard-reset (EN pulse) |
| `partitions` | yes | List partition table (CSV or read from chip) |
| `write-partition` | yes | Flash a file to a named partition |
| `read-partition` | yes | Dump a named partition to file |
| `erase-partition` | yes | Erase a named partition |
| `backup` | yes | Dump entire flash (auto-sized; `.gz` aware) |
| `restore` | yes | Restore a flash backup (`.gz` aware) |
| `monitor` | yes | Serial monitor with `--expect`/`--expect-not` regex matching and built-in crash detection |
| `elf2image` | no | Build a `.bin` image from an ELF |
| `merge-bin` | no | Combine multiple bins into one padded image |

## Architecture

- `src/protocol/` — SLIP framing, command packets, sync sequence
- `src/transport/` — Transport trait + serialport-rs backend
- `src/reset.rs` — Classic, UnixTight (ioctl TIOCMSET), USB-JTAG-Serial,
  HardReset strategies
- `src/chip.rs` — Per-chip registry (magic numbers, SPI regs, EFUSE
  layout, stub variant hooks, watchdog config)
- `src/stub.rs` — RAM upload + OHAI handshake; bundled `esp-flasher-stub`
  v0.7.0 blobs in `stubs/`
- `src/ops.rs` — High-level ops (flash, erase, read, MD5 verify, SPI
  flash command pass-through)
- `src/partition.rs` — CSV parser + binary partition-table parser
- `src/imagegen.rs` — ELF parsing + ESP image building + merge-bin
- `src/observe.rs` — Event types, NDJSON emitter, report aggregator,
  diagnostic hint engine
- `src/cli.rs` + `src/runner.rs` — CLI parsing and orchestration

## For AI agents: Agent Skill

The repo ships an [Agent Skill](https://agentskills.io) at
`skills/esparagus/`. Drop the directory into your agent's skills
folder (Claude Code, Cursor, OpenCode, Goose, etc.) and the agent
gets a structured guide to esparagus's CLI, the NDJSON event stream,
the GNU-expect-style monitor, exit-code semantics, and the
crash-detection patterns — everything an agent needs to flash a build
and decide what to do based on the result, without parsing prose.

```
skills/esparagus/
├── SKILL.md                      # name + description + main guide
├── references/
│   ├── EVENTS.md                 # full NDJSON event schema
│   ├── EXIT_CODES.md             # exit codes + diagnostic classes
│   ├── EXPECT_PATTERNS.md        # --expect / crash detection
│   └── ESPTOOL_COMPAT.md         # busybox-style esptool symlink mode
└── assets/
    └── partitions-example.csv    # IDF-format partition table
```

## Drop-in for `esptool.py` (busybox-style multi-call)

esparagus is also a **multi-call binary** in the BusyBox sense: when invoked
through an `esptool` name (typically a symlink), it translates the CLI on
the fly so tools and scripts written against upstream `esptool.py` keep
working unmodified. The most useful case is `idf.py flash`, which shells
out to `esptool.py` internally.

```sh
# Install the alias (drop a symlink earlier on $PATH than the real esptool.py)
ln -s "$(which esparagus)" ~/.local/bin/esptool.py

# Now this runs esparagus under the hood:
esptool.py --chip esp32-s3 --port /dev/cu.usbserial-XYZ \
  --before default_reset --after hard_reset \
  write_flash 0x0 boot.bin 0x10000 app.bin

# And so does idf.py:
idf.py -p /dev/cu.usbserial-XYZ flash
```

What the compat layer does to the argv:

- Underscore subcommand names → hyphenated (`write_flash` → `write-flash`).
- `chip_id` → `detect` (semantically equivalent — both read the chip ID via
  GET_SECURITY_INFO).
- Underscored `--before`/`--after` values → hyphenated
  (`default_reset` → `default-reset`).
- `read_flash <addr> <size> <out>` positional → `read-flash --address X
  --size Y --output Z`.
- Flags esparagus doesn't honour yet (`--flash_mode`, `--flash_freq`,
  `--flash_size`, `--erase-all`, `--encrypt`) are stripped with a stderr
  warning so the run still succeeds. Flash params come from the image
  header in normal IDF builds anyway.
- Subcommands esparagus genuinely doesn't implement (`image_info`,
  `verify_flash`, `dump_mem`, `load_ram`, `read_mem`, `write_mem`,
  `make_image`, `summary`, `get_security_info`) print a clear error
  pointing at upstream esptool and exit 2.

## Provenance and licensing

esparagus is a Rust port of esptool's protocol implementation. Because
it contains a derived translation of GPL-2.0+ code, the whole work is
distributed under **GPL-2.0-or-later** to match upstream.

The bundled flasher stub binaries in `stubs/` come from
[esp-flasher-stub v0.7.0](https://github.com/espressif/esp-flasher-stub)
and are dual licensed Apache-2.0 OR MIT. See `stubs/LICENSE-APACHE`,
`stubs/LICENSE-MIT`, and the top-level `NOTICE` for full attribution.

## Status and what's intentionally not (yet) implemented

See `docs/STATUS.md` for the per-chip bench-validation matrix.

The current release covers the **flash workflow**: detect, erase,
write, read, verify, partition-aware operations, backup/restore, offline
image generation. The following esptool features are *not* in v0.1 and
will land in their own focused releases:

- **eFuse read/burn** (`espefuse` equivalent) — burn is irreversible
  per-chip silicon; needs careful guard rails and per-chip block
  schemas. Planned next.
- **Secure boot signing** (`espsecure` equivalent) — RSA-3072 / ECDSA
  with Espressif's key format compatibility.
- **Flash encryption** — depends on the secure-boot key infrastructure.
- **NAND flash** — niche; pending demand.
- **RFC2217 / TCP transports** — niche; pending demand.

If one of these is blocking you, open an issue.

## Contributing

See `CONTRIBUTING.md`. Bug reports should include the full NDJSON event
stream (`--json --log-file out.ndjson`) and the final report
(`--report out.json`) — that's exactly the shape of feedback esparagus
was designed to produce.
