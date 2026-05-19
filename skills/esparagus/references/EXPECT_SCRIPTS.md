# Expect script reference (TOML)

`esparagus expect <script.toml>` runs a declarative script of
send/expect/branches/captures with the same crash detectors as
`monitor`. This file is the agent-facing reference: shape, semantics,
gotchas.

For the inline-flag form (`monitor --expect "X" --expect-not "Y"`),
see `EXPECT_PATTERNS.md`.

## Quick reference

```toml
# Optional metadata.
name = "boot-and-login"
# Default per-step timeout (seconds).
timeout_secs = 30

# Steps execute in declared order. Use `goto` to jump.
[[step]]
name = "wait-prompt"               # Optional. Required if anything goto's here.
send = "\n"                         # Optional. {{templates}} expanded.
expect = "esparagus> "              # Optional regex (unanchored).
expect_not = "PANIC|ABORT"          # Optional negative. If matched: exit 41.
timeout_secs = 5                    # Per-step override.

# Branching: first matching pattern transfers control to its `goto`.
[[step]]
name = "auth"
send = "auth {{env.PW}}\n"
expect_any = [
    { pattern = "ok",   goto = "post-login" },
    { pattern = "FAIL", goto = "fail"       },
]

# Capture table: per-step regex with named or numbered groups.
# Captured values become available as {{name}} in later steps.
[[step]]
name = "post-login"
capture = { user = "Welcome (\\S+)" }
send = "uptime\n"
expect = "load average"

# Terminal step. ok = true → exit 0; ok = false → exit 40.
[[step]]
name = "fail"
ok = false
```

## Template substitution

Templates use `{{ ... }}`. Resolution order:

1. `{{env.NAME}}` → `$NAME` from the process env (empty if unset).
2. `{{name}}`     → captures from prior `capture = { ... }` tables
                     OR from named groups `(?P<name>...)` in prior `expect` patterns.
3. `{{1}}`–`{{9}}` → positional groups from the most recent successful match.

Templates expand inside `send` AND inside any pattern field
(`expect`, `expect_any[].pattern`, `expect_not`). Patterns containing
`{{` are NOT regex-compiled at validation time (their substituted
form isn't known yet) — only balanced-brace shape is checked; the
regex is compiled at runtime after substitution.

## Step semantics

- **`expect` only**: wait for the regex. Match → next step. Timeout → exit 40.
- **`expect_any` only**: wait for any branch. First match → `goto` target. Timeout → exit 40.
- **`expect_not` only**: wait for the step timeout; if seen during that window, exit 41. Otherwise advance.
- **Both `expect` and `expect_any`** in the same step: validation error (43).
- **`send` only, no expect/expect_not**: fire and advance immediately. The step timeout is NOT consumed.
- **`ok = true`**: terminal success step. Stops the script with exit 0.
- **`ok = false`**: terminal failure step. Stops with exit 40.
- **Falling off the end** without a terminal step: exit 0 (every step ran successfully).

Crash detection runs in parallel with every wait. A detected
panic / WDT / abort / assert / stack_smash / exception / cache /
brownout / download_loop / reboot_loop aborts the script with exit
42 and emits a `crash_context` event (same shape as `monitor`).
Disable with `--no-crash-detect`.

## NDJSON events emitted

- `expect_script_start { name?, step_count }`
- `expect_step_begin { name, send_preview?, expect_summary?, timeout_ms }`
- `serial_line { line }` (every line received from the chip)
- `expect_step_match { name, pattern, line, captures }`
- `expect_step_branch { from, to }`
- `expect_step_timeout { name, pattern, timeout_ms }`
- `expect_step_negative_match { name, pattern, line }`
- `crash_detected { kind, pattern, line }` + `crash_context { kind, lines }` on crash
- `expect_script_complete { ok, steps_run, final_step }`

## Validation (`--check`)

`esparagus expect script.toml --check` runs the parser + validator
without opening a serial port. Catches:

- malformed TOML
- duplicate `name` across steps
- unknown `goto` targets
- bad regex in `expect` / `expect_any[].pattern` / `expect_not` / `capture`
  (only for patterns NOT containing `{{` templates)
- unbalanced `{{ ... }}` braces in templated patterns
- both `expect` and `expect_any` on one step (mutually exclusive)

Exit 0 if clean, 43 otherwise. Useful in pre-commit / CI lint passes.

## Exit codes

| Code | Meaning |
|---|---|
| 0  | Script ended on `ok = true` or fell off the end |
| 40 | Step timed out, or hit `ok = false` |
| 41 | `expect_not` pattern matched |
| 42 | Crash detector fired during a wait |
| 43 | Script failed validation (parse / regex / goto / etc.) |
| 10 | Could not open the serial port |
| 1  | Generic internal failure |

## When to pick `expect` over `monitor --expect ...`

- More than one step (most flows). Inline flags only express one wait.
- Need to *send* anything (auth, query, command). `monitor` is read-only.
- Branching on which line appears first.
- Reusing captured values across steps.

Stick with `monitor --expect` when:

- Single one-shot wait ("did the firmware reach `MAIN LOOP READY`?").
- You want the raw NDJSON event stream without any per-step
  bookkeeping in your script.
