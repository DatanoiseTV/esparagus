# Changelog

All notable changes to this project follow [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Initial Rust port of esptool's protocol, sync, reset, chip detection, and
  stub loader paths.
- NDJSON event stream on stdout (`--json`).
- File logging (`--log-file`).
- Final structured report (`--report path.json`) with per-stage timings,
  errors, and machine-readable `next_actions` remediation hints.
- Diagnostic hint engine mapping common failure classes to next-step
  suggestions for CI/LLM feedback loops.
- `detect`, `read-mac`, `flash-id`, `erase-flash`, `erase-region`,
  `write-flash`, `read-flash`, `reset` subcommands.
- Bundled esp-flasher-stub v0.7.0 blobs (Apache-2.0 OR MIT) for ESP32,
  ESP32-S2, ESP32-S3, ESP32-C2, ESP32-C3, ESP32-C5, ESP32-C6, ESP32-H2,
  ESP32-P4.
- Stable exit codes for CI integration.
