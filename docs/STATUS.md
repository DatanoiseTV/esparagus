# Implementation status

## Bench-tested matrix

Operations confirmed on real hardware:

| Chip       | detect | read-mac | flash-id | erase | write-flash (ROM) | write-flash (stub) | read-flash | reset |
|------------|--------|----------|----------|-------|-------------------|--------------------|------------|-------|
| ESP32      | -      | -        | -        | -     | -                 | -                  | -          | -     |
| ESP32-S2   | -      | -        | -        | -     | -                 | -                  | -          | -     |
| ESP32-S3   | -      | -        | -        | -     | -                 | -                  | -          | -     |
| ESP32-C2   | -      | -        | -        | -     | -                 | -                  | -          | -     |
| ESP32-C3   | -      | -        | -        | -     | -                 | -                  | -          | -     |
| ESP32-C5   | -      | -        | -        | -     | -                 | -                  | -          | -     |
| ESP32-C6   | -      | -        | -        | -     | -                 | -                  | -          | -     |
| ESP32-H2   | -      | -        | -        | -     | -                 | -                  | -          | -     |
| ESP32-P4   | OK     | OK       | OK       | -     | -                 | -                  | -          | OK    |

Legend: `-` not bench-tested yet · `OK` confirmed working · `FAIL` known issue (see below)

ESP32-P4 was bench-verified on an early-rev (silicon revision < 3.00) board
connected via a CH343 USB-UART bridge.  Detect read chip_id=18 via
GET_SECURITY_INFO, then chose the `esp32p4-rev1` stub variant via
EFUSE_BLOCK1 revision reading, completed the OHAI handshake, and reported
flash JEDEC id 0xC8 0x4018 (GigaDevice GD25Q128, 16 MB) and the base MAC.

## Known unimplemented features

- Secure Download Mode (SDM) interaction
- Flash encryption (`--encrypt`) and per-region encrypt mapping
- Secure boot key/signing operations (`espsecure` equivalents)
- eFuse read/burn (`espefuse` equivalents)
- UF2 image generation
- NAND flash commands
- Image generation (`elf2image`, `merge_bin`, etc.)
- Native USB CDC transport (USB-Serial/JTAG appears as a serial port to the OS,
  but a dedicated nusb-based fast path could bypass the userspace UART)
- RFC2217 / TCP-bridged ports

## Architecture

- `src/protocol/` — SLIP framing, command packet encoding/decoding, sync
- `src/transport/` — Transport trait (open/close/read/write/baud/DTR/RTS)
- `src/reset.rs` — ClassicReset, UnixTightReset, USBJTAGSerialReset, HardReset
- `src/chip/` — Chip trait + registry, one module per silicon family
- `src/stub.rs` — Stub uploader (MEM_BEGIN/MEM_DATA/MEM_END + OHAI handshake)
- `src/ops/` — High-level operations (flash, erase, read, etc.)
- `src/observe/` — Event types, NDJSON emitter, report aggregator, hint engine
- `src/cli.rs` — clap definitions; `src/main.rs` runs the chosen subcommand

The stub loader path is wired but unverified on hardware; it follows the same
sequence as upstream esptool (MEM_END jumps to stub entry, expect OHAI byte,
then talk stub command set). Failures during stub handshake fall back to
`--no-stub` and report the failure via `next_actions`.
