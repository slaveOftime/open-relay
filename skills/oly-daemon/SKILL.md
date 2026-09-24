---
name: oly-daemon
description: Use when an agent or operator needs to read or update the oly daemon's `config.json`-style settings, decide whether each field is hot-reloadable, or configure prompt and session-resume detection patterns. For running, querying, and supervising sessions, consult the `oly` skill instead.
---

# Configure and hot-reload the oly daemon

This skill covers the **daemon's configuration**: the JSON file at `<STATE_DIR>/config.json`, the file's resolution rules, the override precedence, and how hot reload actually behaves. It does not cover running individual sessions, the interactive TUI, or `oly send` recipes — see the bundled `oly` skill for those.

The daemon's configuration is hot-reload aware: the reloader polls `config.json` every `CONFIG_RELOAD_POLL_INTERVAL` (≈2 s) and, after a successful parse, swaps in a new snapshot and applies side effects (rebuild notification channels, update eviction and journal limits and resume patterns, reinstall the log filter). Most edits land within a few seconds.

## State directory and CLI/start vs config-file precedence

`config.json` lives under the platform-specific state directory, in priority order:

```text
1. $OLY_STATE_DIR
2. %LOCALAPPDATA%\oly                        (Windows)
3. ~/Library/Application Support/oly         (macOS)
4. $XDG_STATE_HOME/oly  →  ~/.local/state/oly (Linux)
```

- `oly daemon status` prints the effective `STATE`, `ROOT`, `LOGS`, `SESSIONS`, `HTTP`, `SSH-PUB-KEY`, and `AUTH` for the daemons in this state dir — that is also where `config.json` lives.
- The first run creates `config.json` and writes a fresh VAPID key pair (Unix: `0600`).
- Some paths are fixed at startup — `state_dir`, `sessions_dir`, `db_file`, `lock_file`, `info_file`, socket name and file — and cannot be re-pointed by editing `config.json`.

### Precedence at startup

```text
CLI flag  >  config.json field  >  built-in default
```

The CLI exposes four runtime overrides on `oly daemon start`:

| Flag | Alters |
| --- | --- |
| `--bind <ADDR>` | `http_bind` |
| `--port <PORT>` | `http_port` |
| `--notification-hook <PATH>` | `notification_hook` |
| `--web-push-proxy <URL>` | `web_push_proxy` |

The daemon records every CLI override on the in-memory config; **after every hot reload those overrides keep winning over `config.json` for the lifetime of the process**. So `http_bind`/`http_port` edits will appear loaded but inert until the matching CLI flag is dropped, the daemon is restarted without it, or the user understands the override is masking the file value.

The `OLY_WEB_PUSH_PROXY` environment variable is honoured on first start (it also resolves second after a CLI override). Reload does not re-read the env var.

## Hot reload vs restart

The reload task watches mtime. When `config.json` changes:

1. `try_reload` re-parses the file. **Parse errors keep the previous config in memory** — the daemon does not silently fall back to defaults, which is why the diff is reported as `config reload failed; keeping current configuration` instead of `config reloaded`.
2. `old.hot_reload_changes(&new)` lists the fields picked up live; `old.restart_required_changes(&new)` lists the ones that still need a daemon restart.
3. The reloader applies the live side effects it owns, then warns about the restart-only ones.

Always look at both diffs when you change something — `restart_required_changes` means the change is **inert** until you stop and start the daemon.

| Field (in `config.json`) | Effect |
| --- | --- |
| `http_bind` | **Restart required.** Rebinds the TCP socket. The CLI flag keeps winning until you restart without it. |
| `http_port` | **Restart required.** Same as `http_bind`. |
| `notification_min_interval_seconds` | Hot reload. The notify monitor reseeds its debounce window. Default: `10` (`1` after clamp). |
| `notification_hook` | Hot reload. Triggered a notification-pipeline rebuild the next time `notify` matches a session prompt. `null`/empty disables. |
| `prompt_patterns` | Hot reload. Live-read by the prompt-detection sweep on every tick. |
| `resume_patterns` | Hot reload. Replaces built-in Codex/Pi resume rules; `[]` disables them unless additional rules are supplied. Applies to future scans, not previously scanned sessions. |
| `additional_resume_patterns` | Hot reload. Appends rules to the built-ins (or to `resume_patterns` when specified). Applies to future scans. |
| `web_push_subject` | Hot reload. Triggers a notification-pipeline rebuild. |
| `web_push_vapid_public_key` | Hot reload. Triggers a notification-pipeline rebuild. |
| `web_push_vapid_private_key` | Hot reload. Triggers a notification-pipeline rebuild. |
| `web_push_proxy` | Hot reload. Triggers a notification-pipeline rebuild. |
| `log_level` | Hot reload. Reinstalls the tracing env filter. |
| `max_running_sessions` | Hot reload. Re-sampled by session-start paths. Default: `50`. |
| `session_eviction_seconds` | Hot reload (Pushed to `SessionStoreHandle::set_eviction_seconds`). Default: `15`. |
| `screen_scrollback_rows` | Hot reload. Sampled on next attach. Default: `5000`. Floor of `1000` rows is also kept for any fresh attach. |
| `silence_seconds` | Hot reload. Default: `10`. |
| `stop_grace_seconds` | Hot reload. Sampled per-stop. Default: `5`. |
| `max_journal_bytes_per_session` | Hot reload. Applied via `SessionStoreHandle::set_journal_byte_cap`. `0` = unlimited. Legacy alias: `max_output_log_bytes`. |
| `journal_retention_days` | Hot reload. Wall-clock retention for *stopped* sessions. `0` = keep everything. Running sessions are never deleted by this knob. |

**Anything outside this table is fixed at startup and needs a daemon restart.** If a change is not appearing, it almost certainly is in this table's "restart required" rows (or the field doesn't exist in `AppConfigOverrides` and was silently dropped on parse).

## Editing the file

Read the current effective value first (`oly daemon status` shows config-derived fields; for everything else read the file):

```sh
cat "$OLY_STATE_DIR/config.json"
```

Edit it with whichever tool you trust (`jq`, your editor, printf) — **the file is JSON, not JSON-with-comments**, so check it parses before saving:

```sh
jq . "$OLY_STATE_DIR/config.json" >/dev/null || echo "broken: fix before continuing"
```

After every save:

- The daemon logs `config.json reloaded changed=...` and lists the rad fields.
- If something needs a restart it logs `config changes require a daemon restart to take effect changed=...`.
- On JSON parse error it logs `config reload failed; keeping current configuration` and keeps running with the last good config. Re-save a corrected file and the next reload attempt will succeed.

To restart safely:

```sh
oly daemon stop --grace 15   # gives in-flight sessions a chance to exit cleanly
oly daemon start --detach
```

The exit code is `0` even when warning that sessions did not exit cleanly; re-run `oly ls` to confirm everything is in the state you want.

## `prompt_patterns` (hot-reloadable, lives in `NotifyConfig`)

`prompt_patterns` is the regex list oly matches against each session's filtered output to decide whether it might need input. Each pattern is an unanchored Rust `regex` matched against one output line; when **any line produced after the last match** matches, the notify monitor fires an event (subject to `notification_min_interval_seconds`). Empty or absent fall back to the built-in defaults below.

Patterns are evaluated as a flat list — order is irrelevant, but each line is independent. A pattern that ends with `\s*$` matches only the printf prompt character at end of line so a noisy log line cannot trip the prompt detector. Case-insensitive flags go inside the regex (the `(?i)` prefix), not in the JSON.

### CRU vs extend

The default list is intentionally broad. To **extend**:

1. Copy the default list from this skill (it is the live literal in `src/config.rs`).
2. Append/insert your own patterns. Keep them anchored with `^` or `$` where it matters — naive phrases like `password` will catch user logs in unrelated sessions.
3. Save and verify `config.json reloaded changed=["prompt_patterns"]` appears in the daemon log (the daemon is at `WORK_DIR`/corresponding `info_file`, started with `RUST_LOG=oly=info`); a parse failure shows `config reload failed; keeping current configuration` and keeps the live patterns.

To **replace** with a small curated set, clear the array and use only what you want; an empty array disables prompt detection entirely (only `input_needed` replies then trigger notifications).

To **revert to defaults**, remove the `prompt_patterns` key (or set it to `[]` for an explicit, narrow hook). After saving the file, the next reload will fall back to the list below.

### Built-in `DEFAULT_PROMPT_PATTERNS`

This is the live literal as of this skill release, copy-pasted from `src/config.rs`. Keep the JSON-array shape and the escaping:

```json
"prompt_patterns": [
    "[>❯›\\$#%]\\s*$",
    "❯\\s+",
    "^\\s*>\\s+\\S",
    ">>>\\s*$",
    "(?i)[\\(\\[](y/n|yes/no)[\\)\\]]",
    "(?i)(?:password|api[_ ]?key|token|secret)\\s*:",
    "^\\?\\s",
    "(?i)(?:do you|are you sure|allow\\b).{0,80}\\?",
    "(?i)continue\\?\\s*$",
    "(?i)press (?:enter|return|any key)"
]
```

In `config.json` the strings must be valid JSON string literals: regex backslashes are doubled (`\\s`, `\\$`, `\\(`), and the quotes are removed by JSON. The Rust `regex` crate compiles each entry with case-sensitive defaults; add `(?i)` inside the pattern to make it case-insensitive. Patterns are matched per line, so end-of-line anchors (`$`) and the literal question mark (`\\?`) are how you scope a match to the actual prompt shape.

Patterns are **not bounded to a single shell or REPL**. The defaults cover:

- Shell/REPL continuation prompts (`$`, `#`, `>`, `▶`, `❯`).
- Python REPL (`>>>`).
- `(y/n)` / `[yes/no]` confirmations.
- `password:` / `api key:` / `token:` / `secret:` credential prompts (case-insensitive).
- Inquirer-style `? ` prefix.
- Natural-language questions (`do you …?`, `are you sure …?`, `allow …?`).
- `Continue?` at end of line.
- `press <key>` continuation prompts.

### Examples

Add an `npm install` confirmation prompt:

```json
"prompt_patterns": [
    "<existing>",
    "(?i)ok to proceed\\?\\s*$"
]
```

Add a custom in-app `>` prompt (note the JSON-doubled backslashes):

```json
"prompt_patterns": [
    "^\\s*>\\s+\\S",
    "Do you want to continue? \\[y/N\\]\\s*$"
]
```

Detect an in-house prompt that wraps to the next line:

```json
"prompt_patterns": [
    "deploy\\?\\s*\\(\\s*Y\\s*/\\s*N\\s*\\)"
]
```

To add a fourth example of your own without keeping the whole list, just write the JSON with only the keys you want to toggle:

```jsonc
{
    "notification_hook": "C:/bin/desktop-notify.exe",
    "notification_min_interval_seconds": 30,
    "prompt_patterns": [
        "(?i)approve\\?",
        "(?i)deny\\?"
    ]
}
```

After saving, run `oly logs <ID>` on a session you know is waiting at a prompt and confirm the daemon log line for that PID switches from waiting/idle to the prompt-detected pathway. If it does not, your pattern is too narrow or your shell is overwriting the prompt before the journal sees it.

## Session resume hints (`resume_patterns` and `additional_resume_patterns`)

Resume patterns are **not** `prompt_patterns`: they inspect the *rendered journal tail* after a child process has completed, PTY output has closed, and the journal is durable. The daemon stores the most recent matching suggestion as `resume_command` in SQLite session metadata; it is also returned in session summaries and `oly ls --json`. It is a hint for a future resume workflow, **not a command the daemon executes**. Existing sessions whose tails have already been scanned are not rescanned when configuration changes.

Each rule has three required strings:

- `program`: the actual session child executable's basename, case-insensitive; `.exe`, `.cmd` and `.bat` suffixes are ignored. A session launched as `bash` or a wrapper such as `npx` does not match a rule for the tool inside it.
- `pattern`: a Rust `regex` against the rendered tail, with **at least one capture group** for the session identifier or path. Escape regex backslashes twice in JSON (`\\s`, `\\.`).
- `command`: the saved suggestion template. `$1`, `$2`, etc. expand from capture groups; for example, `agent --restore $1`.

Absent keys retain the built-in Codex (`codex resume <UUID>`) and Pi (`pi --session <path>`) rules from `src/config.rs`. To **add** a tool without copying defaults, set `additional_resume_patterns`:

```json
{
  "additional_resume_patterns": [{
    "program": "agent",
    "pattern": "agent --resume ([a-z0-9-]+)",
    "command": "agent --restore $1"
  }]
}
```

To **replace** the built-ins, use `resume_patterns` with your own array; `"resume_patterns": []` disables detection unless `additional_resume_patterns` supplies rules. Both keys may appear together: the additional rules append to the replacement array. If several rules match, the last matching occurrence in the tail wins.

Edits hot-reload into the session store. The reload diff reports `patterns` (the `ResumeConfig` field). An invalid regex, missing capture group or empty `program`/`command` rejects a hot reload and preserves the previous configuration; check the daemon log for `config reload failed`. No restart is required after a valid edit. To verify a rule, finish a session whose **child executable** matches `program`, then inspect its `resume_command` via `oly ls --json` after the completed-output scan.

## Common pitfalls

- **CLI flags out-rank the file.** `oly daemon start --port 17000` keeps `http_port=17000` even after you set `http_port: 15443` in the file. Stop the daemon and start it without the flag to let the file win.
- **HTTP bind/port is hot-loaded into memory but not rebound.** The reload log line lists the diff; the actual TCP socket is still on the old address until you restart.
- **Parse errors silently keep the old config.** Validate with `jq . config.json` before saving; the daemon logs `config reload failed; keeping current configuration` if the new file is bad.
- **Env vars are read only at startup.** `OLY_WEB_PUSH_PROXY` does not re-trigger on hot reload — drop it in `config.json` if you want it to follow file edits.
- **`prompt_patterns: []` is not the same as omitting the key.** An explicit empty array disables prompt detection; omitting the key restores the defaults.
- **`resume_patterns: []` also disables its defaults.** To keep Codex/Pi and add a rule, use `additional_resume_patterns`; a rule for the wrong child executable never matches.
- **Wrong key names silently no-op.** Typos such as `notification_hookk` make it look like the daemon ignored you, when actually nothing was set. The reload log won't mention them because no diff exists. Cross-check the JSON key in the table above.

## Verify and self-check

After every config edit, in order:

1. `jq . "$STATE_DIR/config.json"` exits 0.
2. In the daemon log: `config.json reloaded changed=["..."]`. If you expected a restart-required field, also expect the `config changes require a daemon restart` warning.
3. Restart the daemon if any field in the restart-only set changed.
4. Trigger the behaviour you changed (start a session that prompts, push an HTTP request to the new port, switch the log level and watch verbosity change) and confirm the runtime reflects the new value.
