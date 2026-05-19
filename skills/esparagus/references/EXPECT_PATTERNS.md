# Expect-style patterns and crash detection

`monitor` and `flash-monitor` both take `--expect` and `--expect-not`
patterns. This file documents the regex syntax and the built-in
crash detector that runs alongside them.

## Regex flavour

Patterns are compiled with the Rust [`regex` crate](https://docs.rs/regex/),
which is the same one ripgrep uses. Key points:

- **Unanchored by default.** A pattern matches if it's found anywhere
  in a serial line. Add `^` / `$` if you need full-line anchoring.
- **Default flags**: case-sensitive, single-line. `(?i)` for
  case-insensitive, `(?m)` for multiline, `(?s)` for `.` to match
  newlines.
- **Lookaround is unsupported** (Rust's regex engine is RE2-flavoured
  for guaranteed linear runtime). If you need `(?<=foo)bar`, restructure.
- **Bad regex = exit 2** at startup, before the chip is touched.

## Examples

Match an ESP-IDF boot log line:

```bash
--expect 'I \(\d+\) cpu_start: Starting scheduler'
```

Match either of two acceptance lines (use multiple `--expect`):

```bash
--expect 'app_main: started OK' \
--expect 'main: ready'
```

Fail fast on an error level log line:

```bash
--expect-not 'E \(\d+\)'
```

Match an Arduino sketch printing a calibration result:

```bash
--expect '^Calibration: PASS$'
```

Combine: succeed on "OK", fail on "FAIL":

```bash
--expect 'OK' --expect-not 'FAIL' --timeout 20
```

## What esparagus considers a "line"

Bytes from the serial port are accumulated until a `\n`. The optional
`\r` immediately before the `\n` is stripped. Each line then goes
through:

1. Negative match check (any `--expect-not`).
2. Built-in crash detector (unless `--no-crash-detect`).
3. Positive match check (any `--expect`).

So if a single line contains both a crash signature and a
`--expect` match, the crash wins (exit 32). If it contains both a
crash signature and a `--expect-not` match, the `--expect-not`
wins (exit 30, earlier in the priority order).

## Built-in crash detector

Always on unless `--no-crash-detect`. When any of these match a line,
esparagus:

1. Emits a `crash_detected` event with a stable `kind`.
2. Switches into context-capture mode, grabbing up to 200 follow-up
   lines (or 5 seconds, or until a "Rebooting..." sentinel) into
   a `crash_context` event.
3. Exits with **code 32**.

### Pattern → kind mapping

| Regex (case-sensitive) | `kind` | Source |
|---|---|---|
| `Task watchdog got triggered` | `wdt` | ESP-IDF task_wdt |
| `\bWDT\b.*timeout` | `wdt` | Generic WDT |
| `Interrupt watchdog` | `wdt` | ESP-IDF int_wdt |
| `assert failed:` | `assert` | ESP-IDF `assert()` macro |
| `ASSERTION FAILED` | `assert` | Various RTOSes |
| `abort\(\) was called` | `abort` | libc abort / IDF wrapper |
| `Stack smashing protect failure` | `stack_smash` | GCC SSP |
| `Guru Meditation Error` | `panic` | Xtensa panic handler header |
| `LoadProhibited` | `exception` | Xtensa CPU exception |
| `StoreProhibited` | `exception` | Xtensa CPU exception |
| `IllegalInstruction` | `exception` | Xtensa / RISC-V CPU exception |
| `InstructionFetchError` | `exception` | Xtensa CPU exception |
| `LoadStoreError` | `exception` | Xtensa CPU exception |
| `Guru Meditation` | `panic` | RISC-V panic handler header |
| `Exception was unhandled` | `exception` | RISC-V panic handler |
| `Cache disabled but cached memory region accessed` | `cache` | ESP cache fault |
| `Brownout detector was triggered` | `brownout` | ROM brownout |
| `boot:0x.. \(DOWNLOAD` | `download_loop` | Chip dropped into ROM DOWNLOAD mode after reset — i.e. the freshly-written firmware isn't actually booting. Often means the BOOT strap is held low (auto-reset circuit, faulty boot button) or the image at the app offset is invalid. |
| `^ESP-ROM:` (stateful, ≥2 hits per monitor session) | `reboot_loop` | Chip is rebooting in a tight loop. First ROM banner is expected (our own reset_to_app produced it); the second means the second-stage bootloader / app reset on its own. Boot mode is normal (e.g. `SPI_FAST_FLASH_BOOT`) — the chip just doesn't make it far enough to print any app log. Distinct from `download_loop`. |

### `reboot_loop` causes, in order of likelihood

When `reboot_loop` fires, walk this list before blaming the firmware:

1. **Console-on-wrong-interface.** If the `transport_info` event
   reports a non-Espressif USB VID (CH340/CH343 `0x1a86`, CP210x
   `0x10c4`, FTDI `0x0403`), but the IDF build has
   `CONFIG_ESP_CONSOLE_USB_SERIAL_JTAG=y`, every log line is going
   to the chip's unused native USB peripheral instead of out UART0
   to your bridge. The chip is fine and running, you just can't see
   it. **The signature in `crash_context` is the dead giveaway: you
   see the IDF second-stage bootloader's lines** (`SPI mode`, `load`,
   `entry`) — those *are* routed through UART0 because the ROM
   bootloader hard-codes them there. But the ESP-IDF app uses
   whatever the menuconfig'd console says, which is the native USB.
   Switch `sdkconfig` to `CONFIG_ESP_CONSOLE_UART=y` (or
   `=DEFAULT`) and reflash. Bench-validated 2026-05-19 on an
   ESP32-C5 through a CH343.

2. **Brownout during early init.** Wi-Fi / BLE radio initialisation
   draws inrush spikes (>300 mA on C5). On a thin USB cable, a
   powered hub without enough current per port, or a marginal PSU,
   the chip browns out before `esp_log_init` finishes. The
   `Brownout detector was triggered` line *usually* prints first,
   but if the brownout fires before the brownout detector itself is
   armed (during the very first hundred milliseconds), no print
   makes it out and it manifests as a silent reboot loop. Move to a
   direct USB port + thick cable to test.

3. **Panic in `app_main` precursors before `esp_log_init`.** C++
   static constructors, custom early hardware init, mis-allocated
   stack — anything that faults before the logging subsystem is up
   eats the panic message. Add a single `ets_printf("PRE\n");` at
   the very top of `app_main` (that uses the ROM printf, works
   before IDF logging is set up). If `PRE` appears, the crash is
   deeper in your app; if it doesn't, the crash is in IDF startup.

4. **IDF version skew between bootloader and app.** Rebuilding only
   one of them leaves an incompatible boot-args layout. `idf.py
   fullclean && idf.py build` to rebuild both atomically.

The patterns are checked in this order; the first match wins.

### What "context" the agent gets

After a crash hit, the `crash_context` event's `lines` array contains
the matched line plus all subsequent serial lines until one of:

- A sentinel match: `Rebooting...`, `CPU halted.`, `ELF file SHA256:`.
- 200 lines accumulated.
- 5 seconds of wall time elapsed.

For an ESP-IDF panic this is typically enough to capture:

- The panic header
- The faulting core's register dump
- The backtrace addresses
- The chip's reset / reboot announcement

If your firmware reroutes its panic output (e.g. via UART2 or a
custom logger), the built-in patterns may not match. In that case
pass `--no-crash-detect` and use `--expect-not` for the substring
your panic prints (e.g. `--expect-not 'PANIC:'`).

## Putting it together: a feedback-loop run

A typical agent invocation that exercises everything:

```bash
esparagus --port /dev/cu.usbserial-XYZ --json \
  --log-file /tmp/flash.ndjson \
  --report /tmp/flash.report.json \
  flash-monitor \
    --monitor-baud 115200 \
    --expect 'I \(\d+\) cpu_start: Starting scheduler' \
    --expect-not 'E \(\d+\)' \
    --timeout 30 \
    0x10000 build/app.bin
```

Outcome dispatch:

```
exit 0   → app booted, scheduler running. Continue.
exit 30  → 'E (...)' line seen. Read report.errors / serial_line events.
exit 31  → no scheduler log within 30s. Firmware likely hung.
exit 32  → panic/WDT/abort. Read the crash_context event for backtrace.
exit 13  → flash write failed. Likely UART corruption; lower baud.
other    → see report.next_actions for what to try.
```
