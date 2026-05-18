# esparagus NDJSON event schema

Every event is a single JSON object on its own line with at least:

```json
{ "ts": "<RFC3339 UTC ms>", "level": "info"|"warn"|"error", "event": "<name>", ... }
```

The `event` discriminator is stable and meant to be the primary thing
you branch on. Fields are documented per event below.

## Run lifecycle

### `run_start`

Emitted first on every chip-touching run.

```json
{"event":"run_start","tool":"esparagus 0.1.0","chip_arg":null,"port":"/dev/cu.usbserial-XYZ","baud":460800}
```

| Field | Type | Notes |
|---|---|---|
| `tool` | string | "esparagus &lt;version&gt;" |
| `chip_arg` | string\|null | The value passed via `--chip`, or null |
| `port` | string | OS-level device path |
| `baud` | u32 | The user-requested baud (the actual sync happens at 115200 internally) |

### `transport_info`

Emitted right after the port is opened. Tells you which USB device the
OS sees so you can distinguish a UART bridge from native USB-CDC.

```json
{"event":"transport_info","port":"/dev/cu.usbserial-XYZ","usb_vid":"0x1a86","usb_pid":"0x55d3"}
```

VID/PID values are hex-formatted strings to survive JSON's loose
number handling.

### `run_complete`

Always the last `info` event in a chip-touching run.

```json
{"event":"run_complete","ok":true,"duration_ms":5012}
```

For `monitor` / `flash-monitor` runs the corresponding terminator is
`monitor_complete` (see below).

## Connect phase

### `connect_attempt`

One per try. The `strategy` is the reset method used.

```json
{"event":"connect_attempt","strategy":"unix_tight","attempt":1}
```

Strategy values: `classic`, `unix_tight` (Unix only — atomic
DTR/RTS via ioctl), `usb_jtag_serial` (native USB-CDC), `no_reset`.

### `connected`

```json
{"event":"connected","strategy":"unix_tight","attempts":1}
```

## Identify phase

### `chip_detected`

```json
{"event":"chip_detected","chip":"ESP32-P4","chip_id":18}
```

`chip` is the human-readable name from the registry. `chip_id` is the
`IMAGE_CHIP_ID` byte from `GET_SECURITY_INFO`.

## Stub phase

### `stub_upload_start`

```json
{"event":"stub_upload_start","chip":"ESP32-P4","blob":"esp32p4-rev1"}
```

`blob` is the resolved stub variant. P4 has rev1 (silicon < 3.00) vs
non-rev1; all other chips have a single variant.

### `stub_running`

```json
{"event":"stub_running","chip":"ESP32-P4","blob":"esp32p4-rev1","entry":"0x4ff10000"}
```

`entry` is the address the chip jumped to. Hex-formatted string.

### `baud_upgrade`

Emitted after the stub starts and `change_baud` succeeds.

```json
{"event":"baud_upgrade","from":115200,"to":460800}
```

## Read phase

### `flash_id_read`

```json
{"event":"flash_id_read","manufacturer":"0xc8","device":"0x4018","size_mb":16}
```

| Field | Notes |
|---|---|
| `manufacturer` | JEDEC manufacturer byte (hex) |
| `device` | JEDEC device id, displayed in JEDEC order (`type<<8 \| capacity`) |
| `size_mb` | u32 or null; decoded from the capacity byte via the standard `1 << (cap - 0x14)` formula |

### `mac_read`

```json
{"event":"mac_read","mac":"7F:AF:D0:B2:F1:80"}
```

### `read_begin` / `read_done`

```json
{"event":"read_begin","addr":"0x00000000","size":1048576}
{"event":"read_done","addr":"0x00000000","size":1048576,"md5":"f4af..."}
```

## Write phase

### `write_begin`

```json
{"event":"write_begin","addr":"0x00010000","size":1048576,"compressed":true}
```

`compressed` reflects whether the wire transport used `FLASH_DEFL_*`
(default) or the uncompressed `FLASH_DATA` path.

### `write_progress`

Emitted roughly every 5% during a write. Don't rely on exact cadence.

```json
{"event":"write_progress","addr":"0x00010000","written":65536,"total":1048576,"pct":6.25}
```

### `md5_verified`

Emitted after each region finishes writing and the device-side MD5
matches the host-side MD5.

```json
{"event":"md5_verified","addr":"0x00010000","size":1048576,"md5":"f4af..."}
```

## Erase phase

```json
{"event":"erase_begin","addr":"0x00009000","size":24576}
{"event":"erase_done","addr":"0x00009000","size":24576,"ms":120}
```

## Partition table

### `partition_table_loaded`

```json
{"event":"partition_table_loaded","source":"flash:0x8000+0x1000","count":8}
```

`source` is either `csv:<path>` or `flash:<offset>+<len>`.

### `partition_resolved`

Emitted whenever esparagus resolves a partition name to a region.
Also emitted once per entry in response to a `partitions` subcommand.

```json
{"event":"partition_resolved","name":"ota_0","ptype":"app","subtype":"ota_0","offset":"0x00110000","size":1048576}
```

## Backup / restore

```json
{"event":"backup_begin","size":16777216}
{"event":"backup_done","size":16777216,"md5":"..."}
{"event":"restore_begin","size":16777216}
{"event":"restore_done","size":16777216,"md5":"..."}
```

## Reset

```json
{"event":"reset_issued","kind":"uart"}
```

Values for `kind`: `uart` (DTR/RTS via the bridge), `usb` (native
USB-Serial/JTAG path).

## Monitor phase

### `monitor_start`

```json
{"event":"monitor_start","port":"...","baud":115200,"timeout_secs":30,
 "expect":["boot complete"],"expect_not":["FATAL"]}
```

### `serial_line`

One event per decoded line. The line content has the trailing
`\r\n` already stripped.

```json
{"event":"serial_line","line":"I (1234) main: hello world"}
```

### `expect_match`

```json
{"event":"expect_match","kind":"positive","pattern":"boot complete","line":"I (1234) main: boot complete"}
```

`kind` is `"positive"` (matched a `--expect`) or `"negative"`
(matched a `--expect-not`).

### `monitor_timeout`

Emitted on timeout, immediately before `monitor_complete`.

```json
{"event":"monitor_timeout","lines_seen":42,"bytes_seen":3014}
```

### `monitor_complete`

Always the last monitor event. Branch on `reason`.

```json
{"event":"monitor_complete","reason":"expect_match","duration_ms":4821,"lines_seen":42,"bytes_seen":3014}
```

`reason` values: `expect_match`, `expect_not_match`, `timeout`,
`crash`, `interrupted`.

### `crash_detected`

Emitted as soon as a built-in crash pattern matches.

```json
{"event":"crash_detected","kind":"panic","pattern":"Guru Meditation Error","line":"Guru Meditation Error: Core  0 panic'd (LoadProhibited)"}
```

`kind` values: `panic`, `wdt`, `abort`, `assert`, `stack_smash`,
`exception`, `cache`, `brownout`.

### `crash_context`

Emitted after `crash_detected`, once we've gathered the surrounding
backtrace lines (or hit a "Rebooting..." sentinel / line budget).

```json
{"event":"crash_context","kind":"panic","lines":[
  "Guru Meditation Error: Core 0 panic'd (LoadProhibited)",
  "Core  0 register dump:",
  "PC      : 0x40080123  PS      : 0x00060030  ...",
  "Backtrace: 0x40080123:0x3ffb0c40 ...",
  "Rebooting..."
]}
```

Up to 200 follow-up lines or 5 seconds, whichever comes first.

## Warnings and errors

### `warning`

Non-fatal. Examples: image chip_id mismatches the connected chip;
the partition table doesn't have an MD5 record; an unsupported
write_flash flag was stripped by the esptool-compat layer.

```json
{"event":"warning","message":"image at app.bin has chip_id 9 but chip is ESP32-P4"}
```

### `error`

Fatal. Carries the stable `class` string used by the
`next_actions` hint engine.

```json
{"event":"error","stage":"stub_upload","class":"stub_handshake","detail":"stub handshake failed (timeout)"}
```

Full list of `class` values and their associated `next_actions` in
`EXIT_CODES.md`.
