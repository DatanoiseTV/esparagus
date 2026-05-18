//! Busybox-style multi-call entry: when esparagus is invoked via an
//! `esptool` name (typically a symlink), we translate the argv before
//! handing it to clap so that scripts and tools written against
//! upstream esptool — most importantly `idf.py flash` — keep working.
//!
//! Conventions we accommodate:
//!
//!   * esptool uses **underscored** subcommand names (`write_flash`,
//!     `chip_id`); esparagus uses **hyphenated** ones (`write-flash`,
//!     `detect`). We rewrite the subcommand token in place.
//!
//!   * esptool's `read_flash` takes three **positional** args
//!     (`<addr> <size> <output>`) where esparagus uses the named flags
//!     `--address` / `--size` / `--output`. We pull the three positions
//!     out and re-emit them as flags.
//!
//!   * esptool spells reset modes with underscores (`default_reset`,
//!     `hard_reset`) but esparagus's clap value-enum uses hyphens.
//!     We rewrite the *value* of `--before` and `--after`.
//!
//!   * esptool accepts several `write_flash` flags (`--flash_mode`,
//!     `--flash_freq`, `--flash_size`, `--erase-all`, `--encrypt`)
//!     that have no equivalent in esparagus's current surface. We
//!     emit a warning on stderr, strip the flag (and its argument
//!     when present), and continue. Failing the run would block
//!     `idf.py flash` for no good reason — flash params come from
//!     the image header in normal IDF builds anyway.
//!
//!   * Subcommands esparagus genuinely doesn't implement
//!     (`image_info`, `verify_flash`, `dump_mem`, `load_ram`,
//!     `read_mem`, `write_mem`, `make_image`, `summary`,
//!     `get_security_info`) print a clear error pointing at upstream
//!     esptool and exit 2.
//!
//! The detection is path-based on argv[0]'s basename — same as how
//! BusyBox does it. To opt in, drop a symlink:
//!
//! ```text
//! ln -s /usr/local/bin/esparagus /usr/local/bin/esptool.py
//! ```
//!
//! Then `esptool.py write_flash 0x0 boot.bin` will run inside
//! esparagus.

use std::path::Path;

/// Return `true` when the basename of `argv0` indicates the user is
/// running the binary through an esptool-flavoured name (symlink,
/// rename, etc.).
pub fn is_esptool_invocation(argv0: &str) -> bool {
    let base = Path::new(argv0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(argv0)
        .to_ascii_lowercase();
    // Strip a trailing ".exe" on Windows and a ".py" suffix from the
    // upstream Python script name.
    let stem = base
        .strip_suffix(".exe")
        .unwrap_or(&base)
        .strip_suffix(".py")
        .unwrap_or_else(|| base.strip_suffix(".exe").unwrap_or(&base));
    stem == "esptool"
}

/// Translate an esptool-style argv into an esparagus-style argv. Returns
/// the rewritten argv (with `argv[0]` preserved) or an error message
/// suitable for printing on stderr if the requested subcommand has no
/// esparagus equivalent.
pub fn translate_argv(mut argv: Vec<String>) -> Result<Vec<String>, String> {
    if argv.len() <= 1 {
        return Ok(argv);
    }
    // Walk the argv looking for the first non-flag token after argv[0].
    // We treat any single-dashed or double-dashed token as a flag; if a
    // flag takes a value, that value just looks like another arg and
    // gets considered as the subcommand candidate. To avoid that, we
    // skip the immediate next arg whenever a known value-bearing flag
    // appears (--port, --baud, --chip, --before, --after).
    let value_flags = [
        "-p",
        "--port",
        "-b",
        "--baud",
        "-c",
        "--chip",
        "--before",
        "--after",
        "--connect-attempts",
    ];

    let mut subcmd_idx: Option<usize> = None;
    let mut i = 1usize;
    while i < argv.len() {
        let s = &argv[i];
        if let Some(eq) = s.find('=') {
            // Form `--flag=value`. Always a flag, never a subcommand.
            if s.starts_with('-') && eq > 0 {
                i += 1;
                continue;
            }
        }
        if s.starts_with('-') {
            // Skip the flag, then skip its value if applicable.
            let consumed = if value_flags.contains(&s.as_str()) {
                2
            } else {
                1
            };
            i += consumed;
            continue;
        }
        subcmd_idx = Some(i);
        break;
    }

    // Rewrite --before VALUE and --after VALUE values from underscored
    // to hyphenated form so they match esparagus's clap value-enum.
    rewrite_underscored_value(&mut argv, "--before");
    rewrite_underscored_value(&mut argv, "--after");

    let sub_pos = match subcmd_idx {
        Some(i) => i,
        // No subcommand → let clap handle `--help`, `--version`, etc.
        None => return Ok(argv),
    };

    // Special-case the `version` pseudo-subcommand → global --version.
    if argv[sub_pos] == "version" {
        argv.drain(sub_pos..);
        argv.push("--version".into());
        return Ok(argv);
    }

    let was_read_flash = argv[sub_pos] == "read_flash";
    let translated = match argv[sub_pos].as_str() {
        "chip_id" | "chip-id" => "detect",
        "read_mac" => "read-mac",
        "flash_id" => "flash-id",
        "erase_flash" => "erase-flash",
        "erase_region" => "erase-region",
        "write_flash" => "write-flash",
        "read_flash" => "read-flash",
        // `elf2image` and `merge_bin` are the same shape in both tools;
        // we just normalise underscores.
        "elf2image" => "elf2image",
        "merge_bin" | "merge-bin" => "merge-bin",
        s @ ("image_info" | "verify_flash" | "dump_mem" | "load_ram" | "read_mem" | "write_mem"
        | "make_image" | "summary" | "get_security_info") => {
            return Err(format!(
                "esptool's `{}` subcommand is not implemented in esparagus. \
                 Use upstream esptool.py for this operation, or open an issue.",
                s
            ));
        }
        other => other, // pass through (e.g. `monitor`, already same)
    };
    argv[sub_pos] = translated.to_string();

    if was_read_flash {
        rewrite_read_flash_positionals(&mut argv, sub_pos)?;
    }

    // Strip write-flash flags we don't yet honour, emitting a warning so
    // users notice the silent change in behaviour.
    let unsupported_with_value = [
        "--flash_mode",
        "--flash-mode",
        "--fm",
        "--flash_freq",
        "--flash-freq",
        "--ff",
        "--flash_size",
        "--flash-size",
        "--fs",
        "--encrypt-files",
    ];
    let unsupported_bool = [
        "--erase-all",
        "--erase_all",
        "-e",
        "--encrypt",
        "--ignore-flash-encryption-efuse-setting",
        "--no-progress",
    ];
    for flag in unsupported_with_value {
        strip_flag_and_value(&mut argv, flag, true);
    }
    for flag in unsupported_bool {
        strip_flag_and_value(&mut argv, flag, false);
    }

    Ok(argv)
}

/// Find `flag` in argv. If `has_value`, remove the following arg as well.
/// Repeats until the flag no longer appears. Prints a warning on each hit.
fn strip_flag_and_value(argv: &mut Vec<String>, flag: &str, has_value: bool) {
    loop {
        // Match either `--flag` or `--flag=value` (single occurrence).
        let pos = argv
            .iter()
            .position(|a| a == flag || (has_value && a.starts_with(&format!("{}=", flag))));
        let Some(i) = pos else { return };
        let removed = argv.remove(i);
        let took_value = removed.contains('=');
        if has_value && !took_value && i < argv.len() && !argv[i].starts_with('-') {
            let v = argv.remove(i);
            eprintln!("warn: esparagus does not yet support {flag} (got value {v:?}); ignored");
        } else {
            eprintln!("warn: esparagus does not yet support {flag}; ignored");
        }
    }
}

/// For an esptool-style value like `default_reset` after a flag, change
/// it to the hyphenated form `default-reset` that esparagus's clap
/// enum expects.
fn rewrite_underscored_value(argv: &mut [String], flag: &str) {
    let mut i = 1usize;
    while i < argv.len() {
        let take_value = if argv[i] == flag {
            true
        } else if let Some(rest) = argv[i].strip_prefix(&format!("{flag}=")) {
            let new_val = rest.replace('_', "-");
            argv[i] = format!("{flag}={new_val}");
            false
        } else {
            false
        };
        if take_value && i + 1 < argv.len() {
            let v = std::mem::take(&mut argv[i + 1]);
            argv[i + 1] = v.replace('_', "-");
        }
        i += 1;
    }
}

/// esptool's `read_flash` takes three positional args: `<addr> <size>
/// <output>`. esparagus's clap expects `--address`, `--size`, `-o`.
/// Rewrite the first three positional args after the subcommand into
/// those flags. Anything before the subcommand or other flags after
/// it is left in place.
fn rewrite_read_flash_positionals(argv: &mut Vec<String>, sub_pos: usize) -> Result<(), String> {
    let mut positions: Vec<usize> = Vec::new();
    let mut i = sub_pos + 1;
    while positions.len() < 3 && i < argv.len() {
        if argv[i].starts_with('-') {
            // Boolean flag — skip; flag-with-value would have its value
            // immediately following, but for the flags we care about on
            // read_flash (`--no-progress`, `--no-stub`, etc.) they're
            // booleans. Safer to just skip one arg at a time.
            i += 1;
            continue;
        }
        positions.push(i);
        i += 1;
    }
    if positions.len() < 3 {
        return Err(format!(
            "esptool-compat: `read_flash` needs 3 positional args (addr size output); got {}",
            positions.len()
        ));
    }
    // Snapshot the values, then remove them from argv (right-to-left
    // so the earlier indices stay valid), then push the named flags.
    let addr = argv[positions[0]].clone();
    let size = argv[positions[1]].clone();
    let output = argv[positions[2]].clone();
    for &idx in positions.iter().rev() {
        argv.remove(idx);
    }
    argv.push("--address".into());
    argv.push(addr);
    argv.push("--size".into());
    argv.push(size);
    argv.push("--output".into());
    argv.push(output);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn detects_esptool_basename() {
        assert!(is_esptool_invocation("esptool.py"));
        assert!(is_esptool_invocation("esptool"));
        assert!(is_esptool_invocation("/usr/local/bin/esptool.py"));
        assert!(is_esptool_invocation("esptool.exe"));
        assert!(is_esptool_invocation("/Users/foo/.local/bin/esptool"));
        assert!(!is_esptool_invocation("esparagus"));
        assert!(!is_esptool_invocation("esptool-other"));
    }

    #[test]
    fn translates_write_flash_subcommand() {
        let out = translate_argv(s(&["esptool.py", "write_flash", "0x10000", "app.bin"])).unwrap();
        assert_eq!(out, s(&["esptool.py", "write-flash", "0x10000", "app.bin"]));
    }

    #[test]
    fn translates_chip_id_to_detect() {
        let out = translate_argv(s(&["esptool.py", "--chip", "esp32-s3", "chip_id"])).unwrap();
        assert_eq!(out, s(&["esptool.py", "--chip", "esp32-s3", "detect"]));
    }

    #[test]
    fn translates_before_after_values() {
        let out = translate_argv(s(&[
            "esptool.py",
            "--before",
            "default_reset",
            "--after",
            "hard_reset",
            "flash_id",
        ]))
        .unwrap();
        assert_eq!(
            out,
            s(&[
                "esptool.py",
                "--before",
                "default-reset",
                "--after",
                "hard-reset",
                "flash-id",
            ])
        );
    }

    #[test]
    fn translates_before_after_equals_form() {
        let out = translate_argv(s(&[
            "esptool.py",
            "--before=default_reset",
            "--after=hard_reset",
            "flash_id",
        ]))
        .unwrap();
        assert_eq!(
            out,
            s(&[
                "esptool.py",
                "--before=default-reset",
                "--after=hard-reset",
                "flash-id",
            ])
        );
    }

    #[test]
    fn rewrites_read_flash_positionals_to_flags() {
        let out = translate_argv(s(&[
            "esptool.py",
            "--port",
            "/dev/cu.usbserial-XYZ",
            "read_flash",
            "0x0",
            "0x100000",
            "out.bin",
        ]))
        .unwrap();
        assert_eq!(
            out,
            s(&[
                "esptool.py",
                "--port",
                "/dev/cu.usbserial-XYZ",
                "read-flash",
                "--address",
                "0x0",
                "--size",
                "0x100000",
                "--output",
                "out.bin",
            ])
        );
    }

    #[test]
    fn strips_unsupported_write_flash_flags() {
        let out = translate_argv(s(&[
            "esptool.py",
            "write_flash",
            "--flash_mode",
            "dio",
            "--flash_freq",
            "40m",
            "--flash_size",
            "4MB",
            "--erase-all",
            "0x10000",
            "app.bin",
        ]))
        .unwrap();
        assert_eq!(out, s(&["esptool.py", "write-flash", "0x10000", "app.bin"]));
    }

    #[test]
    fn errors_on_unsupported_subcommand() {
        let res = translate_argv(s(&["esptool.py", "image_info", "app.bin"]));
        let err = res.unwrap_err();
        assert!(err.contains("image_info"));
        assert!(err.contains("esparagus"));
    }

    #[test]
    fn version_becomes_global_flag() {
        let out = translate_argv(s(&["esptool.py", "version"])).unwrap();
        assert_eq!(out, s(&["esptool.py", "--version"]));
    }

    #[test]
    fn passes_through_when_no_subcommand() {
        let out = translate_argv(s(&["esptool.py", "--help"])).unwrap();
        assert_eq!(out, s(&["esptool.py", "--help"]));
    }
}
