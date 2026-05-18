# Changelog

All notable changes to this project follow
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] — 2026-05-18

First public release.

### Added

#### Protocol port (behavioral parity with upstream esptool)

- SLIP framing (RFC 1055) with ESP escapes, streaming decoder that
  preserves unparsed bytes across frame boundaries.
- ESP serial protocol: command packets, sync, command/response
  correlation with the upstream retry loop, register read/write,
  change_baud, MEM_BEGIN/MEM_DATA/MEM_END, FLASH_BEGIN/DATA/END,
  FLASH_DEFL_*, ERASE_FLASH, ERASE_REGION, READ_FLASH, SPI_FLASH_MD5,
  SPI_ATTACH, SPI_SET_PARAMS, GET_SECURITY_INFO.
- Reset strategies: ClassicReset, UnixTightReset (ioctl TIOCMSET for
  atomic DTR/RTS on Unix), USBJTAGSerialReset for native USB-Serial/
  JTAG parts, HardReset.
- Stub loader: RAM upload via MEM_*, OHAI handshake; bundled
  esp-flasher-stub v0.7.0 blobs (Apache-2.0 OR MIT) in `stubs/`.
- Per-chip stub variant selection (ESP32-P4 picks `esp32p4-rev1` when
  silicon revision < 3.00 via EFUSE_BLOCK1).
- Chip registry covering ESP32, ESP32-S2, ESP32-S3, ESP32-C2,
  ESP32-C3, ESP32-C5, ESP32-C6, ESP32-H2, ESP32-P4 with magic numbers,
  SPI register layouts, EFUSE bases, watchdog configs, and USB
  capability flags.

#### Operations

- `detect`, `read-mac`, `flash-id` (with JEDEC capacity decoded into
  flash size in MB).
- `erase-flash`, `erase-region` (stub-only, sector-aligned).
- `write-flash` with compressed (`FLASH_DEFL_*`, default) and
  uncompressed (`FLASH_DATA`, `--no-compress`) wire paths; per-block
  retry and per-region MD5 verify.
- `read-flash` over the stub's streamed READ_FLASH protocol with ACK
  framing and trailing MD5 digest verification.
- `reset` (hard EN pulse).
- Partition-aware operations: CSV parser + binary partition-table
  parser; `partitions`, `write-partition`, `read-partition`,
  `erase-partition`. Auto-resolves table from chip flash at offset
  0x8000 when no `--table` is supplied.
- `backup` / `restore`: dump and restore entire flash. Backup
  auto-detects flash size from the SPI JEDEC capacity byte. File-level
  gzip is auto-detected by `.gz` extension or magic bytes; explicit
  `--compress gz` / `--compress none` available.
- Offline image generation:
  - `elf2image`: parses 32-bit LE ELF (Xtensa or RISC-V), extracts
    PT_LOAD segments, merges adjacent same-flag entries, builds the v2
    ESP image (header + segments + XOR checksum + optional SHA256).
  - `merge-bin`: combine multiple `(address, file)` pairs into one
    padded binary; overlap detection, `--target-size` padding,
    `--target-offset` shifting.

#### Observability

- NDJSON event stream on stdout via `--json` (one JSON object per
  event with stable `event` discriminator and `ts` / `level` fields).
- File mirroring via `--log-file` (always NDJSON, independent of
  stdout mode).
- Structured final report via `--report path.json`: per-stage
  timings, success flags, byte counts, MD5 digests, every error keyed
  by stable `class` strings, machine-readable `next_actions`
  remediation hints.
- Diagnostic hint engine for the common failure classes
  (`sync_timeout`, `port`, `unsupported_command`, `chip_mismatch`,
  `md5_mismatch`, `stub_handshake`, `stub_upload`, `invalid_image`,
  `unknown_chip`).
- Transport info event emits the detected USB VID/PID so the consumer
  knows whether the chip is on a UART bridge, USB-Serial/JTAG, or
  USB-OTG.
- Stable exit codes documented per failure class (10 port, 11 sync,
  12 chip mismatch, 13 flash op, 14 stub, 20 image, ...).

#### Behavior

- Initial sync at 115200 (the ROM bootloader's safe rate), then
  `change_baud` upgrade to the user-requested rate (default 460800).
- macOS port matching is tolerant of `/dev/tty.*` vs `/dev/cu.*`
  variants and basename-only matches.

### License

GPL-2.0-or-later (derivative of esptool, which is GPL-2.0+).
Bundled stub blobs in `stubs/` are dual Apache-2.0 OR MIT.

[0.1.0]: https://github.com/DatanoiseTV/esparagus/releases/tag/v0.1.0
