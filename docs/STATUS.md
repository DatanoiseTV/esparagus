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
| ESP32-P4   | -      | -        | -        | -     | -                 | -                  | -          | -     |

Legend: `-` not bench-tested yet · `OK` confirmed working · `FAIL` known issue (see below)

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
