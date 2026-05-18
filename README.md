# esparagus

A Rust port of [esptool](https://github.com/espressif/esptool) with structured
observability designed for **CI/CD pipelines** and **LLM-driven feedback
loops**.

If you ever tried to teach an AI agent to "flash this firmware, see if it
works, and try again if not", you know the pain: esptool's output is human-
prose mixed with progress bars, errors hide useful state behind generic
strings, and there's no machine-readable way to extract "what happened, what
broke, what to try next." esparagus emits everything as an NDJSON event stream
on stdout, writes a structured final report to a file you can `jq` or feed
back to a model, and pairs every error with a remediation hint.

It is otherwise a faithful behavioral port — the protocol, sync sequences,
reset strategies, chip detection, and stub loader handshake match upstream
esptool. Hardware behavior is intended to be identical.

## Status

This is a v0.1.0 first cut. The architecture, protocol, transport, observability,
and CLI are complete. The faithful port of chip-specific logic and the stub
loader path are wired up but **not bench-tested on real silicon by the author
yet** — please report issues against `master` with the full NDJSON output so
they can be reproduced. See `docs/STATUS.md` for the bench-tested matrix.

## Install

    cargo install --path .

Or build:

    cargo build --release
    ./target/release/esparagus --help

## Quick examples

Detect a chip:

    esparagus detect --port /dev/cu.usbserial-XYZ

Flash with full structured logging to a file your CI/LLM can read:

    esparagus write-flash \
      --port /dev/cu.usbserial-XYZ \
      --log-file flash.log \
      --report flash.report.json \
      --json \
      0x0 bootloader.bin \
      0x10000 app.bin

Read back the same region (e.g. for a verify step):

    esparagus read-flash \
      --port /dev/cu.usbserial-XYZ \
      --address 0x0 --size 0x100000 \
      --output dump.bin

Hard-reset the chip after flashing:

    esparagus reset --port /dev/cu.usbserial-XYZ

Work with the partition table — either from a CSV or by reading it back
from the chip's flash at 0x8000:

    # List the table the chip is actually running
    esparagus partitions --port /dev/cu.usbserial-XYZ

    # Flash a file to a partition addressed by name (no offset math)
    esparagus write-partition --port /dev/cu.usbserial-XYZ \
      --name ota_0 firmware.bin

    # Read back the nvs partition
    esparagus read-partition --port /dev/cu.usbserial-XYZ \
      --name nvs --output nvs.bin

    # Erase the nvs partition
    esparagus erase-partition --port /dev/cu.usbserial-XYZ --name nvs

Backup and restore the full flash, with size auto-detected from the SPI
flash JEDEC capacity byte:

    esparagus backup --port /dev/cu.usbserial-XYZ -o flash-dump.bin
    esparagus restore --port /dev/cu.usbserial-XYZ flash-dump.bin

## Why CI/LLM-friendly

Every CLI run emits a stream of structured events:

```json
{"ts":"2026-05-18T12:34:56.012Z","level":"info","event":"connect_start","port":"/dev/cu.usbserial-XYZ","baud":115200}
{"ts":"2026-05-18T12:34:56.300Z","level":"info","event":"chip_detected","chip":"ESP32-S3","rev":"v0.1","crystal_mhz":40}
{"ts":"2026-05-18T12:34:56.520Z","level":"info","event":"flash_id","manufacturer":"0xEF","device":"0x4017","size_mb":8}
{"ts":"2026-05-18T12:34:56.600Z","level":"info","event":"stub_uploaded","entry":"0x40000000"}
{"ts":"2026-05-18T12:34:56.800Z","level":"info","event":"write_begin","addr":"0x0","size":12288,"compressed":true}
{"ts":"2026-05-18T12:34:57.001Z","level":"info","event":"write_progress","addr":"0x0","written":4096,"total":12288,"pct":33.3}
{"ts":"2026-05-18T12:34:57.450Z","level":"info","event":"md5_verified","addr":"0x0","md5":"f4..."}
{"ts":"2026-05-18T12:35:01.000Z","level":"info","event":"run_complete","ok":true,"duration_ms":4988}
```

And a final report (`--report flash.report.json`):

```json
{
  "ok": true,
  "tool": "esparagus 0.1.0",
  "started_at": "2026-05-18T12:34:55.998Z",
  "duration_ms": 5012,
  "chip": "ESP32-S3",
  "transport": {"port":"/dev/cu.usbserial-XYZ","baud":115200},
  "stages": [
    {"name":"connect","ok":true,"ms":302,"attempts":1},
    {"name":"detect","ok":true,"ms":51},
    {"name":"stub_upload","ok":true,"ms":201},
    {"name":"flash 0x0","ok":true,"ms":4500,"bytes":12288,"md5":"f4..."},
    {"name":"reset","ok":true,"ms":210}
  ],
  "warnings": [],
  "errors": [],
  "next_actions": []
}
```

When something fails, `next_actions` carries machine-readable remediation:

```json
{
  "ok": false,
  "errors": [{"stage":"connect","class":"sync_timeout","detail":"Failed after 7 attempts"}],
  "next_actions": [
    {"kind":"manual_bootloader","desc":"Hold BOOT, press and release EN, release BOOT, retry"},
    {"kind":"check_cable","desc":"Try a known data-capable USB cable; some are power-only"},
    {"kind":"lower_baud","desc":"Retry with --baud 115200 to rule out signal-integrity issues"}
  ]
}
```

This is the shape that lets an outer loop ("LLM agent" or "Jenkins script")
make progress without parsing English.

## Exit codes

| Code | Meaning |
|------|---------|
| 0    | Success |
| 1    | Generic failure |
| 2    | CLI/usage error |
| 10   | Could not open port |
| 11   | Failed to sync with chip |
| 12   | Chip mismatch (wrong --chip flag) |
| 13   | Flash operation failed (bad size, bad address, MD5 mismatch) |
| 14   | Stub loader upload/handshake failed |
| 20   | Image header invalid |

## Provenance and licensing

esparagus is a derivative work of esptool (GPL-2.0-or-later) and is therefore
distributed under **GPL-2.0-or-later**. The bundled flasher stub binaries in
`stubs/` come from esp-flasher-stub and are dual licensed Apache-2.0 OR MIT
(see `stubs/LICENSE-APACHE`, `stubs/LICENSE-MIT`).

See `NOTICE` for full attribution.
