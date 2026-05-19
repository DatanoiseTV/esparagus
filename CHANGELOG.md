# Changelog

All notable changes to this project follow
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `esparagus expect <script.toml>` — scriptable serial automation
  (better-than-GNU-`expect`). TOML script of `send` / `expect` /
  `expect_any` / `expect_not` steps with regex captures, mustache
  `{{var}}` templates (`{{env.NAME}}` / `{{capture_name}}` /
  `{{1}}`..`{{9}}` group refs), named branches with `goto`, and the
  same built-in crash detectors as `monitor`. Stable exit codes
  (0/12/13/20/31), one NDJSON event per step. Example scripts at
  `examples/expect/`.
- MCP `expect` tool wired to the new subcommand.

### Changed

- `monitor` and `flash-monitor` now default to **115200** baud for
  the monitor session, not the global `--baud` (which is the
  *flashing* rate and almost never matches what the running firmware
  uses). New `--monitor-baud` flag overrides. Behaviour-breaking for
  anyone who relied on the previous "monitor at --baud" defaulting.

### Fixed

- `read-mac` byte order on non-ESP32 chips. The pre-fix
  implementation reversed the MAC end-for-end and printed a chip
  with Espressif OUI `3C:DC:75:9A:EC:9C` as `9C:EC:9A:75:DC:3C`.
  Matches upstream esptool / `esp_efuse_mac_get_default()` /
  `idf.py monitor` now. Pinned by regression tests using real
  bench-unit register values.

### Added

- `completions <shell>` and `man` subcommands emit shell completions
  and a roff-formatted man page on stdout (clap-complete +
  clap-mangen).
- `merge-bin --format uf2` produces a Microsoft UF2 container with
  the correct per-chip family ID (looked up from upstream esptool's
  `targets/*.py:UF2_FAMILY_ID`). `--no-md5` clears the per-block MD5
  trailer.
- `read-efuse` reads BLOCK0+BLOCK1 of the EFUSE peripheral as 32-bit
  words plus a decoded BASE_MAC (all chips) and silicon revision
  (ESP32-P4 today). EFUSE burn intentionally remains out of scope
  for v0.x — use `espefuse.py` for that.
- MCP server now honours `notifications/cancelled` mid-call (SIGINT
  to the spawned child) and emits typed `notifications/progress`
  when the client supplied a `_meta.progressToken`.
- `.github/workflows/release.yml` builds prebuilt binaries for
  Linux x86_64/aarch64, macOS x86_64/aarch64, Windows x86_64 on
  every `v*` tag, with per-asset SHA256 sums.
- `dist/homebrew/esparagus.rb` template formula for publishing to a
  Homebrew tap (`brew tap DatanoiseTV/esparagus && brew install
  esparagus`).
- Integration test suite (`tests/cli_integration.rs`) exercising
  the binary end-to-end: --version / --help / list-ports NDJSON
  shape / merge-bin UF2 byte structure / silent-PTY chip-flow error
  path.

### Changed

- Cargo.toml gains `authors`, `homepage`, `documentation`,
  `include` allowlist, and `metadata.docs.rs` — package is now
  publication-ready (`cargo publish --dry-run` clean).

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
