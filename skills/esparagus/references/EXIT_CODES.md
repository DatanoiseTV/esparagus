# Exit codes & error classes

esparagus's exit codes are **stable**. An agent can branch on the
numeric code alone, then read the report's `errors[0].class` and
`next_actions[]` for actionable detail.

## Exit code table

| Code | Meaning | Typical cause |
|---|---|---|
| 0 | Success | The operation completed; if it was a `monitor` / `flash-monitor` with patterns, a `--expect` matched. |
| 1 | Generic failure | Something failed that doesn't fit a more specific code. Always check `report.errors`. |
| 2 | CLI / usage error | Argument parsing failed, or required flag missing. Fix the invocation. |
| 10 | Could not open the port | Device path wrong, port busy (another process has it open), or permission denied. |
| 11 | Failed to sync with the chip | Tried `--connect-attempts` reset cycles, none succeeded. Manual BOOT/EN dance or lower baud usually fixes it. |
| 12 | Chip mismatch | `--chip` was set but the chip on the wire is a different family. |
| 13 | Flash operation failed | Includes MD5 mismatch on a write, erase failure, status byte non-zero from the bootloader/stub. |
| 14 | Stub loader upload or handshake failed | Stub didn't emit OHAI after MEM_END. Often retries are pointless; try `--no-stub`. |
| 20 | Image header invalid | A file passed to a flash op or `elf2image` doesn't look like an ESP image. |
| 30 | Monitor `--expect-not` pattern matched | Firmware emitted a forbidden line. |
| 31 | Monitor timed out | The hard `--timeout` ceiling was reached and no `--expect` had matched. |
| 32 | Monitor detected an ESP crash | Built-in pattern (panic / WDT / abort / assert / stack-smash / exception / cache / brownout) matched. |

## Error `class` → `next_actions` map

When an error happens, `report.errors[i].class` is the stable string
your agent should branch on. The same `class` drives the
`report.next_actions` array, where each entry has a stable `kind` and
a human-readable `desc`.

### `sync_timeout`

The chip didn't respond to sync packets.

```json
"next_actions": [
  {"kind":"manual_bootloader","desc":"Hold BOOT, press and release EN, release BOOT, then retry."},
  {"kind":"check_cable","desc":"Try a known data-capable USB cable; some are power-only."},
  {"kind":"lower_baud","desc":"Retry with --baud 115200 to rule out signal-integrity issues."},
  {"kind":"different_reset_mode","desc":"Pass --before usb-reset if the board has a native USB-Serial/JTAG."}
]
```

### `port`

Could not open the serial device.

Sub-cases by `detail` substring:
- "Permission denied" / "Access is denied" →
  `udev_group` (Linux: add yourself to `dialout`),
  `close_other_users` (close other IDE/serial monitor)
- "No such file" / "FileNotFoundError" →
  `check_port`, `check_cable`
- Otherwise → `check_port`

### `unsupported_command`

The ROM bootloader rejected a stub-only command.

```json
"next_actions": [{"kind":"use_stub","desc":"This command requires the flasher stub. Remove --no-stub."}]
```

### `chip_mismatch`

`--chip` value doesn't match the connected silicon.

```json
"next_actions": [{"kind":"fix_chip_flag","desc":"Remove --chip or set it to the value reported in the chip_detected event."}]
```

### `md5_mismatch`

Host-side MD5 vs device-side MD5 disagree after a write or read.

```json
"next_actions": [
  {"kind":"retry_lower_baud","desc":"MD5 mismatch often indicates UART corruption. Retry with --baud 115200."},
  {"kind":"check_psu","desc":"Brownouts during write also cause MD5 failures. Use a quality 5V supply."}
]
```

### `stub_handshake`

Stub never said OHAI.

```json
"next_actions": [{"kind":"use_no_stub","desc":"Stub failed to start. Retry with --no-stub for slower but more compatible operation."}]
```

### `stub_upload`

Failed before the stub even ran (couldn't write to RAM).

```json
"next_actions": [{"kind":"use_no_stub","desc":"Stub upload failed. Retry with --no-stub."}]
```

### `invalid_image`

```json
"next_actions": [{"kind":"check_image","desc":"Verify the image was built for this chip and starts with magic 0xE9."}]
```

### `unknown_chip`

Auto-detect couldn't match the chip's magic value or `image_chip_id`.

```json
"next_actions": [{"kind":"update_tool","desc":"Detected chip is not in the registry; check for a newer esparagus version."}]
```

### Other classes

`io`, `slip`, `response_mismatch`, `command_failed`, `no_stub_for_chip`,
`other` — generally indicates a bug or a corner case. Capture the
full report and surface to the user.
