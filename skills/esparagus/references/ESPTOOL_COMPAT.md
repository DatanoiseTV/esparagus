# Esptool compatibility (busybox-style multi-call)

When esparagus is invoked through a basename starting with `esptool`
(typically a symlink: `esptool.py` → `esparagus`), the CLI shifts
into a compatibility mode that translates upstream esptool argv into
esparagus argv before clap parses it. This is the path tools like
`idf.py flash` use.

## Install

```bash
ln -s "$(which esparagus)" ~/.local/bin/esptool.py
# Make sure ~/.local/bin is earlier on PATH than the real esptool.py.
```

Then this command runs inside esparagus:

```bash
esptool.py --chip esp32-s3 --port /dev/cu.usbserial-XYZ \
  --before default_reset --after hard_reset \
  write_flash 0x0 boot.bin 0x10000 app.bin
```

And `idf.py flash` works unmodified.

## What gets translated

| esptool argv form | esparagus argv form |
|---|---|
| `write_flash` | `write-flash` |
| `read_mac` | `read-mac` |
| `flash_id` | `flash-id` |
| `erase_flash` | `erase-flash` |
| `erase_region` | `erase-region` |
| `read_flash <addr> <size> <out>` | `read-flash --address <addr> --size <size> --output <out>` |
| `chip_id` | `detect` |
| `merge_bin` | `merge-bin` |
| `elf2image` | `elf2image` (no change) |
| `version` | global `--version` |
| `--before default_reset` | `--before default-reset` |
| `--after hard_reset` | `--after hard-reset` |
| `--before=usb_reset` | `--before=usb-reset` |

The substitution is positional: only the first non-flag token after
`argv[0]` (skipping over known value-bearing flags like `--port` and
`--chip`) is treated as the subcommand and rewritten.

## What gets stripped (with a stderr warning)

esparagus doesn't currently support modifying the image header at
flash time (the flash mode/freq/size override that esptool does
on-the-fly in `write_flash`). The compat layer strips these flags
and prints a warning to stderr so the run still completes; if your
build pipeline relies on the override, regenerate the image with
the right header instead.

Stripped flags:

```
--flash_mode, --flash-mode, --fm
--flash_freq, --flash-freq, --ff
--flash_size, --flash-size, --fs
--encrypt-files
--erase-all, --erase_all, -e
--encrypt
--ignore-flash-encryption-efuse-setting
--no-progress
```

## What hard-errors (exit 2)

The compat layer refuses to silently approximate operations
esparagus doesn't implement. Calling these via `esptool.py`
prints a clear message pointing at upstream:

```
image_info     verify_flash     dump_mem
load_ram       read_mem         write_mem
make_image     summary          get_security_info
```

Example:

```text
$ esptool.py image_info app.bin
error: esptool's `image_info` subcommand is not implemented in esparagus.
       Use upstream esptool.py for this operation, or open an issue.
```

## Output differences to be aware of

- esparagus's `detect` output is shaped differently from esptool's
  `chip_id`. Tools that grep for esptool's prose (`Chip is ESP32-S3`)
  may not match. Pass `--json` and read the `chip_detected` event
  instead.
- esparagus's progress output uses indicatif's bar rather than
  esptool's dotted line. Tools that parse stderr for percentages
  should use `--json` and read `write_progress` events.

## When to bypass compat mode

Always invoke esparagus directly when:

- You're writing the orchestration yourself (use the native
  hyphenated subcommand names + `--json`).
- You need the partition-name-addressed operations
  (`write-partition`, `read-partition`, `erase-partition`),
  `backup`/`restore`, `monitor`, or `flash-monitor` — these don't
  exist in upstream esptool.
- You need the structured NDJSON output. The compat layer doesn't
  add or remove `--json`; it's still your call. But the whole point
  of esparagus over esptool is the structured output, so don't go
  through the compat layer when you can choose.
