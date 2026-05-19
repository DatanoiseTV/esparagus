//! MCP (Model Context Protocol) server over stdio.
//!
//! Spawned via `esparagus mcp`. Speaks newline-delimited JSON-RPC 2.0 on
//! stdin/stdout and exposes the esparagus tool surface to MCP-aware
//! clients (Claude Code / Desktop, Cursor, Goose, Junie, etc.).
//!
//! Design: **subprocess-per-tool-call, on-demand port acquisition.**
//!
//!   - For each `tools/call`, the server `Command::spawn()`s a child
//!     `esparagus --json <args>` and streams its stdout line by line.
//!     Each child-side NDJSON event is forwarded to the MCP client as
//!     a `notifications/esparagus/event` (custom notification method
//!     — clients that don't subscribe just ignore it).
//!   - When the child exits, we return the captured event list +
//!     stderr + exit code as the MCP `tools/call` result.
//!   - The serial port + lockfile are released the moment the child
//!     exits. Other processes can use the port between MCP calls,
//!     which the user explicitly asked for.
//!
//! Tradeoff vs. an in-process server with a persistent `Connection`:
//!   * pro: ~50ms extra fork/exec per call (dominated by the ~300ms
//!     chip sync anyway); zero state to manage between calls; the
//!     lockfile + TIOCEXCL naturally serialise concurrent MCP
//!     clients; any new CLI subcommand becomes available without
//!     re-implementing it in-process.
//!   * con: no streaming progress in the *strictest* MCP-`notifications/progress`
//!     sense — instead we stream the NDJSON event firehose, which
//!     is more information for the agent anyway.
//!
//! Protocol notes:
//!   * MCP version: `2024-11-05` (the spec version Claude Code targets
//!     as of this writing). Clients that send a newer protocolVersion
//!     get our protocolVersion back; that's the negotiation per spec.
//!   * Custom notification method `notifications/esparagus/event` —
//!     namespaced under the official `notifications/` prefix; clients
//!     with no handler discard it silently per JSON-RPC notification
//!     semantics.
//!   * Cancellation (`notifications/cancelled`): the v1 server runs
//!     each tools/call to completion. Stubbed (we'll honour `cancelled`
//!     in a follow-up commit by sending SIGINT to the child).

use std::io::{self, BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

/// MCP protocol version we advertise.
const PROTOCOL_VERSION: &str = "2024-11-05";
/// Custom notification method we use to stream NDJSON events from the
/// child to the client. Clients that didn't subscribe just discard.
const EVENT_NOTIFICATION_METHOD: &str = "notifications/esparagus/event";

/// Entrypoint for the `esparagus mcp` subcommand. Returns the process
/// exit code (0 on clean stdin EOF, 1 on internal error).
pub fn run() -> i32 {
    let stdin = io::stdin();
    let stdout = Arc::new(Mutex::new(io::stdout()));

    // BufRead for line-by-line iteration; JSON-RPC over stdio uses
    // newline-delimited messages.
    let reader = BufReader::new(stdin.lock());
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => return 1, // stdin closed unexpectedly
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                send_error(&stdout, Value::Null, -32700, &format!("Parse error: {e}"));
                continue;
            }
        };

        handle_message(&stdout, &req);
    }
    0
}

/// Dispatch one incoming JSON-RPC message. Notifications (no `id`)
/// produce no response.
fn handle_message(stdout: &Arc<Mutex<io::Stdout>>, req: &Value) {
    let id = req.get("id").cloned();
    let is_notification = id.is_none();
    let method = req
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(json!({}));

    let result: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(handle_initialize(&params)),
        // `notifications/initialized` is a one-shot fire-and-forget
        // signal that the client finished initialising. No response.
        "notifications/initialized" | "initialized" => return,
        // Some clients send `notifications/cancelled` for in-flight
        // requests; the v1 server doesn't support mid-flight cancel, so
        // we accept and ignore.
        "notifications/cancelled" => return,
        "ping" => Ok(json!({})),
        "tools/list" => Ok(handle_tools_list()),
        "tools/call" => handle_tools_call(stdout, &params),
        // resources/* and prompts/* are MCP optional features we don't
        // expose yet; respond with method-not-found per JSON-RPC.
        _ if !is_notification => Err((-32601, format!("Method not found: {method}"))),
        _ => return, // unknown notification: ignore
    };

    if is_notification {
        return;
    }
    match result {
        Ok(value) => send_result(stdout, id.unwrap_or(Value::Null), value),
        Err((code, msg)) => send_error(stdout, id.unwrap_or(Value::Null), code, &msg),
    }
}

fn handle_initialize(_params: &Value) -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "tools": { "listChanged": false },
            "logging": {}
        },
        "serverInfo": {
            "name": "esparagus",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

fn handle_tools_list() -> Value {
    json!({ "tools": tool_catalog() })
}

/// Build the tool catalog. Each tool gets a JSON Schema documenting
/// its arguments, and the description is the same prose an agent
/// would see when activating the Skill.
fn tool_catalog() -> Vec<Value> {
    let port_prop = json!({
        "type": "string",
        "description": "Serial port path (e.g. /dev/cu.usbserial-XYZ, COM5). If omitted, esparagus auto-selects when exactly one ESP-likely device is connected; otherwise errors with the candidate list."
    });
    let baud_prop = json!({
        "type": "integer",
        "minimum": 9600,
        "default": 460800,
        "description": "Baud rate after sync. The sync itself always happens at 115200; esparagus upgrades after the stub starts."
    });
    let chip_prop = json!({
        "type": "string",
        "description": "Override chip detection (esp32, esp32-s3, esp32-c5, etc). Usually unnecessary — detection works."
    });

    let mut tools: Vec<Value> = Vec::new();

    // ---- Offline (no port needed) ----
    tools.push(json!({
        "name": "list_ports",
        "description": "Walk the OS serial port list and print ESP-likely devices (Espressif native USB at VID 0x303a, plus CP210x / CH34x / FTDI bridges). De-duplicates macOS cu./tty. variants. Includes manufacturer / product / serial number from USB descriptors. Does not open any port.",
        "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
    }));

    tools.push(json!({
        "name": "elf2image",
        "description": "Offline: parse a 32-bit ELF (Xtensa or RISC-V), extract PT_LOAD segments, merge adjacent same-flag entries, build a v2 ESP firmware image (header + segments + checksum + SHA256).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "input":        {"type": "string", "description": "Input ELF path."},
                "output":       {"type": "string", "description": "Output .bin path."},
                "target_chip":  {"type": "string", "description": "Chip name (esp32, esp32-s3, esp32-c5, ...)"},
                "flash_mode":   {"type": "string", "enum": ["qio","qout","dio","dout"], "default": "dio"},
                "flash_freq":   {"type": "string", "default": "40m"},
                "flash_size":   {"type": "string", "default": "4MB"}
            },
            "required": ["input", "output", "target_chip"],
            "additionalProperties": false
        }
    }));

    tools.push(json!({
        "name": "merge_bin",
        "description": "Offline: combine multiple (address, file) pairs into one padded binary. Useful for building a complete flash image for distribution.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "output":        {"type": "string", "description": "Output .bin path."},
                "target_size":   {"type": ["integer", "null"], "description": "Pad to this size (bytes)."},
                "target_offset": {"type": "integer", "default": 0, "description": "Subtract this from each piece's address."},
                "pairs": {
                    "type": "array",
                    "description": "Pairs of (address, file_path).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "address":   {"type": "integer"},
                            "file_path": {"type": "string"}
                        },
                        "required": ["address", "file_path"]
                    },
                    "minItems": 1
                }
            },
            "required": ["output", "pairs"],
            "additionalProperties": false
        }
    }));

    // ---- Chip-touching: read-mostly ----
    tools.push(json!({
        "name": "detect",
        "description": "Identify the chip (name, chip_id), MAC, and flash JEDEC ID / size. Use this first when you don't know what's connected.",
        "inputSchema": {
            "type": "object",
            "properties": { "port": port_prop, "baud": baud_prop, "chip": chip_prop },
            "additionalProperties": false
        }
    }));
    tools.push(json!({
        "name": "read_mac",
        "description": "Read the base MAC from EFUSE.",
        "inputSchema": {
            "type": "object",
            "properties": { "port": port_prop, "baud": baud_prop, "chip": chip_prop },
            "additionalProperties": false
        }
    }));
    tools.push(json!({
        "name": "flash_id",
        "description": "SPI flash JEDEC ID (manufacturer + device + decoded size in MB).",
        "inputSchema": {
            "type": "object",
            "properties": { "port": port_prop, "baud": baud_prop, "chip": chip_prop },
            "additionalProperties": false
        }
    }));
    tools.push(json!({
        "name": "partitions",
        "description": "Read and list the partition table — from a local CSV (--table) or from the chip's flash at 0x8000.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "port":  port_prop,
                "baud":  baud_prop,
                "table": {"type": "string", "description": "Path to partitions.csv. If omitted, reads from chip."}
            },
            "additionalProperties": false
        }
    }));
    tools.push(json!({
        "name": "read_partition",
        "description": "Resolve a partition by name and read its contents to a file.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "port":   port_prop,
                "baud":   baud_prop,
                "name":   {"type": "string", "description": "Partition name (e.g. \"nvs\", \"ota_0\", \"factory\")."},
                "output": {"type": "string", "description": "Output path."},
                "table":  {"type": "string", "description": "Optional partitions.csv. If omitted, reads table from chip."}
            },
            "required": ["name", "output"],
            "additionalProperties": false
        }
    }));
    tools.push(json!({
        "name": "read_flash",
        "description": "Read a region of flash to a file. Stub-required.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "port":    port_prop,
                "baud":    baud_prop,
                "address": {"type": "integer"},
                "size":    {"type": "integer"},
                "output":  {"type": "string"}
            },
            "required": ["address", "size", "output"],
            "additionalProperties": false
        }
    }));
    tools.push(json!({
        "name": "backup",
        "description": "Dump the entire flash to a file. Size auto-detected from the SPI JEDEC capacity byte. `.gz` extension transparently gzips.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "port":   port_prop,
                "baud":   baud_prop,
                "output": {"type": "string"},
                "size":   {"type": ["integer", "null"], "description": "Override size (bytes); default: auto."}
            },
            "required": ["output"],
            "additionalProperties": false
        }
    }));
    tools.push(json!({
        "name": "nvs_export",
        "description": "Read the NVS partition and write all items as JSON (blobs base64-encoded). Read-only.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "port":      port_prop,
                "baud":      baud_prop,
                "output":    {"type": "string"},
                "name":      {"type": "string", "default": "nvs"},
                "from_file": {"type": "string", "description": "Inspect a partition-dump file instead of reading from chip."}
            },
            "required": ["output"],
            "additionalProperties": false
        }
    }));

    // ---- Chip-touching: writes ----
    tools.push(json!({
        "name": "write_flash",
        "description": "Write one or more files at given flash addresses. Uses compressed transport with per-block MD5 verify.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "port":  port_prop,
                "baud":  baud_prop,
                "no_compress": {"type": "boolean", "default": false, "description": "Use uncompressed FLASH_DATA path (~3x slower)."},
                "pairs": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "address":   {"type": "integer"},
                            "file_path": {"type": "string"}
                        },
                        "required": ["address", "file_path"]
                    },
                    "minItems": 1
                }
            },
            "required": ["pairs"],
            "additionalProperties": false
        }
    }));
    tools.push(json!({
        "name": "write_partition",
        "description": "Write a file to a partition addressed by name (no offset math). Reads the partition table from the chip (or --table CSV) to resolve.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "port":     port_prop,
                "baud":     baud_prop,
                "name":     {"type": "string"},
                "file_path":{"type": "string"},
                "table":    {"type": "string"}
            },
            "required": ["name", "file_path"],
            "additionalProperties": false
        }
    }));
    tools.push(json!({
        "name": "erase_partition",
        "description": "Erase a partition by name. Destructive — confirm before calling.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "port":  port_prop,
                "baud":  baud_prop,
                "name":  {"type": "string"},
                "table": {"type": "string"}
            },
            "required": ["name"],
            "additionalProperties": false
        }
    }));
    tools.push(json!({
        "name": "erase_flash",
        "description": "Erase the entire flash chip. **DESTRUCTIVE** — wipes everything including the bootloader and NVS. Stub required.",
        "inputSchema": {
            "type": "object",
            "properties": { "port": port_prop, "baud": baud_prop },
            "additionalProperties": false
        }
    }));
    tools.push(json!({
        "name": "restore",
        "description": "Write a previously-backed-up image back to flash from offset 0. Auto-decompresses .gz inputs.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "port":  port_prop,
                "baud":  baud_prop,
                "input": {"type": "string", "description": "Input image (.bin or .bin.gz)."}
            },
            "required": ["input"],
            "additionalProperties": false
        }
    }));

    // ---- Monitor / feedback-loop ----
    tools.push(json!({
        "name": "monitor",
        "description": "Open a serial monitor with GNU-expect-style pattern matching and built-in crash detection (panic, wdt, abort, assert, stack_smash, exception, cache, brownout, download_loop, reboot_loop). Exits 0 on --expect match, 30 on --expect-not, 31 on timeout, 32 on detected crash.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "port":             port_prop,
                "baud":             baud_prop,
                "timeout":          {"type": "integer", "default": 60, "description": "Hard ceiling in seconds. 0 = forever."},
                "expect":           {"type": "array", "items": {"type": "string"}, "default": [], "description": "Success regexes."},
                "expect_not":       {"type": "array", "items": {"type": "string"}, "default": [], "description": "Failure regexes."},
                "no_reset":         {"type": "boolean", "default": false, "description": "Don't pulse EN before listening."},
                "no_crash_detect":  {"type": "boolean", "default": false, "description": "Disable built-in crash patterns."}
            },
            "additionalProperties": false
        }
    }));
    tools.push(json!({
        "name": "flash_monitor",
        "description": "Single-shot flash + monitor: writes the files, then drops directly into the serial monitor with --expect/--expect-not/--timeout. The feedback-loop default for AI agents.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "port":             port_prop,
                "baud":             baud_prop,
                "monitor_baud":     {"type": ["integer", "null"], "description": "Baud for the monitor phase (defaults to --baud)."},
                "no_compress":      {"type": "boolean", "default": false},
                "timeout":          {"type": "integer", "default": 60},
                "expect":           {"type": "array", "items": {"type": "string"}, "default": []},
                "expect_not":       {"type": "array", "items": {"type": "string"}, "default": []},
                "no_crash_detect":  {"type": "boolean", "default": false},
                "pairs": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "address":   {"type": "integer"},
                            "file_path": {"type": "string"}
                        },
                        "required": ["address", "file_path"]
                    },
                    "minItems": 1
                }
            },
            "required": ["pairs"],
            "additionalProperties": false
        }
    }));
    tools.push(json!({
        "name": "reset",
        "description": "Hard-reset the chip via the EN line.",
        "inputSchema": {
            "type": "object",
            "properties": { "port": port_prop, "baud": baud_prop },
            "additionalProperties": false
        }
    }));

    tools
}

/// Convert an MCP tool call into argv for a child `esparagus` process.
/// Returns the args (excluding argv[0], which is set by Command::new).
fn tool_to_cli(name: &str, args: &Value) -> Result<Vec<String>, String> {
    let mut argv: Vec<String> = Vec::new();
    if let Some(s) = args.get("port").and_then(|v| v.as_str()) {
        argv.push("--port".into());
        argv.push(s.into());
    }
    if let Some(n) = args.get("baud").and_then(|v| v.as_u64()) {
        argv.push("--baud".into());
        argv.push(n.to_string());
    }
    if let Some(s) = args.get("chip").and_then(|v| v.as_str()) {
        argv.push("--chip".into());
        argv.push(s.into());
    }
    argv.push("--json".into());

    match name {
        "list_ports" => argv.push("list-ports".into()),
        "detect" => argv.push("detect".into()),
        "read_mac" => argv.push("read-mac".into()),
        "flash_id" => argv.push("flash-id".into()),
        "reset" => argv.push("reset".into()),
        "erase_flash" => argv.push("erase-flash".into()),

        "partitions" => {
            argv.push("partitions".into());
            if let Some(s) = args.get("table").and_then(|v| v.as_str()) {
                argv.push("--table".into());
                argv.push(s.into());
            }
        }
        "read_partition" => {
            argv.push("read-partition".into());
            let name = required_str(args, "name")?;
            let output = required_str(args, "output")?;
            argv.push("--name".into());
            argv.push(name);
            argv.push("--output".into());
            argv.push(output);
            if let Some(s) = args.get("table").and_then(|v| v.as_str()) {
                argv.push("--table".into());
                argv.push(s.into());
            }
        }
        "erase_partition" => {
            argv.push("erase-partition".into());
            argv.push("--name".into());
            argv.push(required_str(args, "name")?);
            if let Some(s) = args.get("table").and_then(|v| v.as_str()) {
                argv.push("--table".into());
                argv.push(s.into());
            }
        }
        "write_partition" => {
            argv.push("write-partition".into());
            argv.push("--name".into());
            argv.push(required_str(args, "name")?);
            if let Some(s) = args.get("table").and_then(|v| v.as_str()) {
                argv.push("--table".into());
                argv.push(s.into());
            }
            argv.push(required_str(args, "file_path")?);
        }
        "read_flash" => {
            argv.push("read-flash".into());
            argv.push("--address".into());
            argv.push(required_u32(args, "address")?.to_string());
            argv.push("--size".into());
            argv.push(required_u32(args, "size")?.to_string());
            argv.push("--output".into());
            argv.push(required_str(args, "output")?);
        }
        "backup" => {
            argv.push("backup".into());
            argv.push("--output".into());
            argv.push(required_str(args, "output")?);
            if let Some(s) = args.get("size").and_then(|v| v.as_u64()) {
                argv.push("--size".into());
                argv.push(s.to_string());
            }
        }
        "restore" => {
            argv.push("restore".into());
            argv.push(required_str(args, "input")?);
        }
        "nvs_export" => {
            argv.push("nvs".into());
            argv.push("export".into());
            argv.push("--output".into());
            argv.push(required_str(args, "output")?);
            if let Some(s) = args.get("name").and_then(|v| v.as_str()) {
                argv.push("--name".into());
                argv.push(s.into());
            }
            if let Some(s) = args.get("from_file").and_then(|v| v.as_str()) {
                argv.push("--from-file".into());
                argv.push(s.into());
            }
        }
        "elf2image" => {
            argv.push("elf2image".into());
            argv.push("--target-chip".into());
            argv.push(required_str(args, "target_chip")?);
            if let Some(s) = args.get("flash_mode").and_then(|v| v.as_str()) {
                argv.push("--flash-mode".into());
                argv.push(s.into());
            }
            if let Some(s) = args.get("flash_freq").and_then(|v| v.as_str()) {
                argv.push("--flash-freq".into());
                argv.push(s.into());
            }
            if let Some(s) = args.get("flash_size").and_then(|v| v.as_str()) {
                argv.push("--flash-size".into());
                argv.push(s.into());
            }
            argv.push("--output".into());
            argv.push(required_str(args, "output")?);
            argv.push(required_str(args, "input")?);
        }
        "merge_bin" => {
            argv.push("merge-bin".into());
            argv.push("--output".into());
            argv.push(required_str(args, "output")?);
            if let Some(n) = args.get("target_size").and_then(|v| v.as_u64()) {
                argv.push("--target-size".into());
                argv.push(n.to_string());
            }
            if let Some(n) = args.get("target_offset").and_then(|v| v.as_u64()) {
                argv.push("--target-offset".into());
                argv.push(n.to_string());
            }
            append_pairs(&mut argv, args)?;
        }
        "write_flash" => {
            argv.push("write-flash".into());
            if args
                .get("no_compress")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                argv.push("--no-compress".into());
            }
            append_pairs(&mut argv, args)?;
        }
        "monitor" => {
            argv.push("monitor".into());
            append_monitor_flags(&mut argv, args);
        }
        "flash_monitor" => {
            argv.push("flash-monitor".into());
            if let Some(n) = args.get("monitor_baud").and_then(|v| v.as_u64()) {
                argv.push("--monitor-baud".into());
                argv.push(n.to_string());
            }
            if args
                .get("no_compress")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                argv.push("--no-compress".into());
            }
            append_monitor_flags(&mut argv, args);
            append_pairs(&mut argv, args)?;
        }
        other => return Err(format!("unknown tool: {other}")),
    }

    Ok(argv)
}

fn required_str(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| format!("missing required string '{key}'"))
}

fn required_u32(args: &Value, key: &str) -> Result<u32, String> {
    args.get(key)
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| format!("missing required u32 '{key}'"))
}

fn append_pairs(argv: &mut Vec<String>, args: &Value) -> Result<(), String> {
    let pairs = args
        .get("pairs")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "missing 'pairs' array".to_string())?;
    if pairs.is_empty() {
        return Err("'pairs' must have at least one entry".into());
    }
    for p in pairs {
        let addr = p
            .get("address")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "pair missing address".to_string())?;
        let path = p
            .get("file_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "pair missing file_path".to_string())?;
        argv.push(format!("{:#x}", addr));
        argv.push(path.into());
    }
    Ok(())
}

fn append_monitor_flags(argv: &mut Vec<String>, args: &Value) {
    if let Some(n) = args.get("timeout").and_then(|v| v.as_u64()) {
        argv.push("--timeout".into());
        argv.push(n.to_string());
    }
    if let Some(arr) = args.get("expect").and_then(|v| v.as_array()) {
        for p in arr {
            if let Some(s) = p.as_str() {
                argv.push("--expect".into());
                argv.push(s.into());
            }
        }
    }
    if let Some(arr) = args.get("expect_not").and_then(|v| v.as_array()) {
        for p in arr {
            if let Some(s) = p.as_str() {
                argv.push("--expect-not".into());
                argv.push(s.into());
            }
        }
    }
    if args
        .get("no_reset")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        argv.push("--no-reset".into());
    }
    if args
        .get("no_crash_detect")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        argv.push("--no-crash-detect".into());
    }
}

/// Run a tool: spawn child esparagus, stream stdout (NDJSON) as
/// per-event MCP notifications, return final summary as content.
fn handle_tools_call(
    stdout: &Arc<Mutex<io::Stdout>>,
    params: &Value,
) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or((-32602, "missing tool name".to_string()))?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let cli_args = tool_to_cli(name, &args).map_err(|e| (-32602, e))?;

    let me = std::env::current_exe()
        .map_err(|e| (-32603, format!("current_exe: {e}")))?;

    let mut child = Command::new(&me)
        .args(&cli_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| (-32603, format!("spawn esparagus child: {e}")))?;

    // Stream stdout as MCP notifications.
    let child_stdout = child
        .stdout
        .take()
        .ok_or((-32603, "child stdout piped but unavailable".to_string()))?;
    let mut events: Vec<Value> = Vec::new();
    let reader = BufReader::new(child_stdout);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<Value>(trimmed) {
            events.push(event.clone());
            let notif = json!({
                "jsonrpc": "2.0",
                "method": EVENT_NOTIFICATION_METHOD,
                "params": { "tool": name, "event": event }
            });
            send_message(stdout, &notif);
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|e| (-32603, format!("wait_with_output: {e}")))?;
    let stderr_str = String::from_utf8_lossy(&output.stderr).into_owned();
    let exit_code = output.status.code().unwrap_or(-1);

    // Build the human-readable summary from the last terminal event
    // (`run_complete` for chip-flow tools, `monitor_complete` for
    // monitor / flash-monitor, or `discovered_port` count for
    // list_ports). Falls back to "exit_code = N" if no events.
    let summary = summarise(name, exit_code, &events);
    let structured = json!({
        "exit_code": exit_code,
        "tool": name,
        "events": events,
        "stderr": stderr_str
    });

    Ok(json!({
        "content": [
            {"type": "text", "text": summary},
            {"type": "text", "text": serde_json::to_string_pretty(&structured).unwrap_or_default()}
        ],
        "isError": exit_code != 0
    }))
}

fn summarise(tool: &str, exit_code: i32, events: &[Value]) -> String {
    let ok = exit_code == 0;
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "{} `{}` (exit {})",
        if ok { "✓" } else { "✗" },
        tool,
        exit_code
    ));

    // Pull a few user-facing fields out of well-known events when present.
    for ev in events {
        let Some(name) = ev.get("event").and_then(|v| v.as_str()) else {
            continue;
        };
        match name {
            "chip_detected" => {
                if let (Some(c), Some(id)) = (
                    ev.get("chip").and_then(|v| v.as_str()),
                    ev.get("chip_id").and_then(|v| v.as_u64()),
                ) {
                    lines.push(format!("  chip: {c} (chip_id={id})"));
                }
            }
            "flash_id_read" => {
                if let (Some(m), Some(d), size) = (
                    ev.get("manufacturer").and_then(|v| v.as_str()),
                    ev.get("device").and_then(|v| v.as_str()),
                    ev.get("size_mb").and_then(|v| v.as_u64()),
                ) {
                    let s = size.map(|n| format!(" {n}MB")).unwrap_or_default();
                    lines.push(format!("  flash: mfr={m} dev={d}{s}"));
                }
            }
            "mac_read" => {
                if let Some(m) = ev.get("mac").and_then(|v| v.as_str()) {
                    lines.push(format!("  mac: {m}"));
                }
            }
            "md5_verified" => {
                if let (Some(a), Some(sz), Some(md5)) = (
                    ev.get("addr").and_then(|v| v.as_str()),
                    ev.get("size").and_then(|v| v.as_u64()),
                    ev.get("md5").and_then(|v| v.as_str()),
                ) {
                    lines.push(format!("  wrote+verified {a} ({sz} bytes) md5={md5}"));
                }
            }
            "crash_detected" => {
                if let Some(k) = ev.get("kind").and_then(|v| v.as_str()) {
                    lines.push(format!("  ⚠ crash kind={k}"));
                }
            }
            "expect_match" => {
                if let (Some(k), Some(p)) = (
                    ev.get("kind").and_then(|v| v.as_str()),
                    ev.get("pattern").and_then(|v| v.as_str()),
                ) {
                    lines.push(format!("  expect_match ({k}): {p}"));
                }
            }
            "monitor_complete" => {
                if let Some(r) = ev.get("reason").and_then(|v| v.as_str()) {
                    lines.push(format!("  monitor_complete reason={r}"));
                }
            }
            "error" => {
                if let (Some(class), Some(detail)) = (
                    ev.get("class").and_then(|v| v.as_str()),
                    ev.get("detail").and_then(|v| v.as_str()),
                ) {
                    lines.push(format!("  error class={class}: {detail}"));
                }
            }
            "discovered_port" => {
                if let (Some(p), Some(bk)) = (
                    ev.get("path").and_then(|v| v.as_str()),
                    ev.get("bridge_human").and_then(|v| v.as_str()),
                ) {
                    lines.push(format!("  {p} ({bk})"));
                }
            }
            _ => {}
        }
    }
    lines.join("\n")
}

// ---- JSON-RPC writers ----

fn send_result(stdout: &Arc<Mutex<io::Stdout>>, id: Value, result: Value) {
    let msg = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    send_message(stdout, &msg);
}

fn send_error(stdout: &Arc<Mutex<io::Stdout>>, id: Value, code: i64, message: &str) {
    let msg = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    });
    send_message(stdout, &msg);
}

fn send_message(stdout: &Arc<Mutex<io::Stdout>>, msg: &Value) {
    let line = match serde_json::to_string(msg) {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut g = stdout.lock().unwrap();
    let _ = writeln!(g, "{}", line);
    let _ = g.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_catalog_has_expected_tools() {
        let catalog = tool_catalog();
        let names: Vec<&str> = catalog
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();
        for expected in [
            "list_ports",
            "detect",
            "read_mac",
            "flash_id",
            "partitions",
            "read_partition",
            "write_partition",
            "erase_partition",
            "write_flash",
            "read_flash",
            "erase_flash",
            "backup",
            "restore",
            "monitor",
            "flash_monitor",
            "nvs_export",
            "reset",
            "elf2image",
            "merge_bin",
        ] {
            assert!(
                names.contains(&expected),
                "tool catalog missing: {expected}"
            );
        }
    }

    #[test]
    fn tool_to_cli_detect_basic() {
        let argv =
            tool_to_cli("detect", &json!({"port": "/dev/cu.usbmodem1", "baud": 460800})).unwrap();
        assert!(argv.contains(&"--port".into()));
        assert!(argv.contains(&"/dev/cu.usbmodem1".into()));
        assert!(argv.contains(&"--json".into()));
        assert!(argv.contains(&"detect".into()));
    }

    #[test]
    fn tool_to_cli_write_flash_pairs() {
        let argv = tool_to_cli(
            "write_flash",
            &json!({
                "pairs": [
                    {"address": 0x10000, "file_path": "/tmp/app.bin"},
                    {"address": 0x20000, "file_path": "/tmp/data.bin"}
                ]
            }),
        )
        .unwrap();
        assert!(argv.contains(&"write-flash".into()));
        assert!(argv.contains(&"0x10000".into()));
        assert!(argv.contains(&"/tmp/app.bin".into()));
        assert!(argv.contains(&"0x20000".into()));
        assert!(argv.contains(&"/tmp/data.bin".into()));
    }

    #[test]
    fn tool_to_cli_monitor_expects() {
        let argv = tool_to_cli(
            "monitor",
            &json!({
                "timeout": 30,
                "expect": ["boot ok", "scheduler"],
                "expect_not": ["FATAL"]
            }),
        )
        .unwrap();
        assert!(argv.contains(&"--timeout".into()));
        assert!(argv.contains(&"30".into()));
        let expect_count = argv.iter().filter(|s| s.as_str() == "--expect").count();
        assert_eq!(expect_count, 2);
        let expect_not_count = argv.iter().filter(|s| s.as_str() == "--expect-not").count();
        assert_eq!(expect_not_count, 1);
    }

    #[test]
    fn tool_to_cli_rejects_unknown_tool() {
        let err = tool_to_cli("unknown_xyz", &json!({})).unwrap_err();
        assert!(err.contains("unknown tool"));
    }

    #[test]
    fn initialize_returns_protocol_version() {
        let r = handle_initialize(&json!({}));
        assert_eq!(r["protocolVersion"], json!(PROTOCOL_VERSION));
        assert_eq!(r["serverInfo"]["name"], json!("esparagus"));
    }

    #[test]
    fn summarise_picks_useful_fields() {
        let events = vec![
            json!({"event": "chip_detected", "chip": "ESP32-C5", "chip_id": 23}),
            json!({"event": "flash_id_read", "manufacturer": "0xc8", "device": "0x4018", "size_mb": 16}),
            json!({"event": "mac_read", "mac": "11:22:33:44:55:66"}),
            json!({"event": "run_complete", "ok": true}),
        ];
        let s = summarise("detect", 0, &events);
        assert!(s.contains("ESP32-C5"));
        assert!(s.contains("chip_id=23"));
        assert!(s.contains("16MB"));
        assert!(s.contains("11:22:33:44:55:66"));
    }
}
