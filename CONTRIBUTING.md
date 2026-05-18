# Contributing to esparagus

Thanks for considering a contribution. esparagus is a Rust port of
esptool's flash workflow, layered with structured observability for
CI/CD and LLM feedback loops. The bar is "should ship cleaner than
what Google ships" — please read this file before opening a PR.

## What to file an issue about

* **Bug reports**: include the full NDJSON event stream
  (`--json --log-file out.ndjson`) and the final report
  (`--report out.json`). Those two artifacts contain everything we
  need — port, baud, VID/PID, chip, stub variant, every command, every
  retry, every error class.
* **Hardware compatibility**: please include the chip family, silicon
  revision (`detect` reports it via stub variant selection on P4), USB
  bridge VID/PID, and operating system.
* **Feature requests for upstream-esptool features** (eFuse burn,
  secure boot signing, flash encryption, etc.): yes, we're aware; see
  README's "intentionally not yet implemented" section for the
  rationale. Please add a thumbs-up on existing tickets rather than
  filing duplicates.

## Local development

Requires Rust 1.88 or newer (see `rust-version` in `Cargo.toml`).
The floor is set by transitive deps (darling 0.23, instability
0.3.12) that demand rustc 1.88; their `edition2024` use requires
1.85 minimum and their own MSRV pushes another bump on top. CI only
tests against stable — pinning a separate MSRV job costs more than
it earns now that every dep bump tightens the floor.

```sh
git clone https://github.com/DatanoiseTV/esparagus
cd esparagus
cargo build --release
cargo test
cargo clippy -- -D warnings
```

The repo CI runs `cargo build`, `cargo test`, and
`cargo clippy -- -D warnings` on every push. PRs need all three green.

If you have access to ESP32-family hardware, please bench-test changes
against real silicon and include the resulting NDJSON / report in the
PR description. See `docs/STATUS.md` for the current bench-validation
matrix; please update it in the same PR when you confirm a new
combination.

## Coding style

* `cargo fmt` (rustfmt defaults). CI will fail on unformatted code.
* `cargo clippy -- -D warnings` clean. Suppress with `#[allow(...)]`
  only when the lint is genuinely wrong for this case, and add a brief
  comment saying why.
* Don't add features, refactors, or abstractions beyond what the PR
  fixes. A bug fix doesn't need surrounding cleanup; a one-shot
  operation doesn't need a helper.
* Don't ship placeholder / scaffolded code. Either it's implemented
  end-to-end and tested, or it's absent. "Coming soon" comments rot.
* Hardware behavior must match upstream esptool unless the PR
  explicitly documents and justifies the divergence.

## Commit messages

* Imperative subject line, ~50 chars.
* Body explains *why* the change is needed when it isn't obvious from
  the diff. Don't restate the diff in English.
* One concern per commit. If a commit message needs "and", it should
  probably be two commits.
* Don't reference Claude, AI, or AI assistance anywhere. No
  `Co-Authored-By: Claude` trailers.

## Adding a chip

The chip registry lives at `src/chip.rs`. Each entry needs:

* `name`, `image_chip_id` from upstream esptool's target file
* `magic_value` (if the chip uses the legacy CHIP_DETECT_MAGIC_REG path)
* SPI register layout (`SpiLayout`) — base address and per-chip
  offsets, lifted from upstream `targets/<chip>.py`
* `efuse_base`, `mac_efuse_reg`, `efuse_block1_addr`
* `watchdog` config if the chip needs WDT-disable before flashing
* `has_usb_jtag_serial`, `has_usb_otg`
* `stub_blob_name` — must correspond to a file in `stubs/`
* `stub_blob_selector` if the chip has silicon-revision-specific stub
  variants (see `esp32p4_stub_selector` for the pattern)

Bench-test detect / read-mac / flash-id on the new chip before merging.

## Adding a stub blob

Stub binaries live in `stubs/`. They come from
[esp-flasher-stub](https://github.com/espressif/esp-flasher-stub),
dual licensed Apache-2.0 OR MIT. When updating to a new
esp-flasher-stub release, copy the new `*.json` files in place and
update `stubs/README.md` with the source version.

## Releasing

(Maintainer notes.)

1. Bump version in `Cargo.toml` per semver.
2. Update `CHANGELOG.md` (Keep-a-Changelog format).
3. `cargo test && cargo clippy -- -D warnings`.
4. Create an annotated tag: `git tag -a vX.Y.Z -m "release vX.Y.Z"`.
5. `git push origin master --tags`.
