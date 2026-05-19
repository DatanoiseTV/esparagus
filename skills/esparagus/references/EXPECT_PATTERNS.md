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
