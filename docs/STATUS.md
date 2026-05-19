# Implementation status

## Hardware support matrix

| Chip       | detect | read-mac | flash-id | erase | write-flash | read-flash / backup | reset |
|------------|--------|----------|----------|-------|-------------|---------------------|-------|
| ESP32      | ~      | ~        | ~        | ~     | ~           | ~                   | ~     |
| ESP32-S2   | ~      | ~        | ~        | ~     | ~           | ~                   | ~     |
| ESP32-S3   | ~      | ~        | ~        | ~     | ~           | ~                   | ~     |
| ESP32-C2   | ~      | ~        | ~        | ~     | ~           | ~                   | ~     |
| ESP32-C3   | ~      | ~        | ~        | ~     | ~           | ~                   | ~     |
| ESP32-C5   | OK     | ~        | ~        | ~     | ~           | OK                  | ~     |
| ESP32-C6   | ~      | ~        | ~        | ~     | ~           | ~                   | ~     |
| ESP32-H2   | ~      | ~        | ~        | ~     | ~           | ~                   | ~     |
| ESP32-P4   | OK     | OK       | OK       | OK    | OK          | OK                  | OK    |

**Legend:**

- `OK` — bench-validated on real silicon by the maintainer.
- `~`  — assumed working via protocol parity with bench-validated
  families. The protocol layer, stub handshake, reset strategy, and
  per-chip register layouts are shared across families; we have not
  bench-run every (chip, operation) pair yet, but a regression here
  would also break the validated rows.
- `FAIL` — known issue, see notes below.

## Bench notes

**ESP32-P4** (silicon rev < 3.00, CH343 USB-UART bridge): full flash
workflow validated end-to-end. `detect` reads chip_id 18 via
GET_SECURITY_INFO; EFUSE_BLOCK1 revision read picks the
`esp32p4-rev1` stub; OHAI handshake at 460800 (after sync at 115200);
write_flash with on-wire compression and MD5 verify works against a
GigaDevice GD25Q128 (16 MB) part; full-flash backup round-trips; reset
into firmware via the monitor's `reset_to_app` sequence works.

**ESP32-C5**: `detect` and full-flash `backup` validated. Other
operations not specifically bench-run.

## Known unimplemented features

- Secure Download Mode (SDM) interaction
- Flash encryption (`--encrypt`) and per-region encrypt mapping
- Secure boot key/signing operations (`espsecure` equivalents)
- eFuse read/burn (`espefuse` equivalents)
- UF2 image generation
- NAND flash commands
- Native USB CDC transport via a dedicated nusb-based fast path
  (USB-Serial/JTAG already works through the OS-level serial port)
- RFC2217 / TCP-bridged ports
- NVS write/edit (read + view + export are implemented)

## Architecture

- `src/protocol/` — SLIP framing, command packet encoding/decoding, sync
- `src/transport/` — Transport trait (open/close/read/write/baud/DTR/RTS)
- `src/reset.rs` — ClassicReset, UnixTightReset, USBJTAGSerialReset,
  HardReset, reset_to_app (monitor-specific)
- `src/chip.rs` — Chip registry, per-chip stub-blob selector
  (P4 revision check), all magic numbers in one table
- `src/stub.rs` — Stub uploader (MEM_BEGIN/MEM_DATA/MEM_END + OHAI handshake)
- `src/ops.rs` — High-level operations (flash, erase, read, MD5)
- `src/partition.rs` — Partition CSV + binary table parser
- `src/imagegen.rs` — Offline `elf2image` + `merge-bin`
- `src/nvs.rs` — NVS partition parser (read-only)
- `src/tui.rs` — Ratatui-based NVS view (table + per-entry hex/ASCII detail)
- `src/monitor.rs` — Expect-style serial monitor + built-in crash detector
- `src/observe.rs` — Event types, NDJSON emitter, report aggregator, hint engine
- `src/esptool_compat.rs` — Busybox-style argv translation for the
  `esptool.py`-symlink invocation path
- `src/cli.rs` — clap definitions
- `src/runner.rs` — Orchestrates a single CLI run

The stub loader path bench-runs against ESP32-P4 today; the same
sequence is used for every other chip (only the bundled blob differs).
