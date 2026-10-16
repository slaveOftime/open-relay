---
name: oly-daemon
description: Use when reading or updating oly daemon configuration, checking hot-reload behavior, or configuring prompt and session-resume hints. For running and supervising sessions, use the oly skill instead.
---

# Configure the oly daemon

Configuration lives at `<STATE_DIR>/config.json`. Read the current file before
editing; preserve unrelated fields and generated secrets. It is **flat JSON**,
not JSON-with-comments and not nested under `limits`, `notify`, or `web_push`.
Unknown keys are ignored **with a warning naming each key**, so successful
JSON parsing does not prove a setting was applied. Confirm the reload diff and
observable behavior.

## Locate state and understand precedence

`oly daemon status` reports effective paths, HTTP endpoint, authentication mode,
and `ID-PUB-KEY`. State resolution:

| Platform | State directory |
|---|---|
| Override | `$OLY_STATE_DIR` |
| Windows | `%LOCALAPPDATA%\oly` |
| macOS | `~/Library/Application Support/oly` |
| Linux | `$XDG_STATE_HOME/oly` or `~/.local/state/oly` |

The first load generates `config.json` and a VAPID key pair. State/db/session
paths are derived from this directory, not configurable JSON fields. Set
`OLY_STATE_DIR` before launch; socket naming is also startup configuration.

Startup precedence is **CLI override > file > built-in default**, except
`web_push_proxy`, which resolves **CLI > OLY_WEB_PUSH_PROXY > file > default**.

| Startup flag | Config field |
|---|---|
| `--bind <ADDR>` | `http_bind` (legacy alias: `bind`) |
| `--port <PORT>` | `http_port` |
| `--notification-hook <COMMAND>` | `notification_hook` |
| `--web-push-proxy <URL>` | `web_push_proxy` |

These CLI overrides keep winning after reload for the process lifetime. The
reloader resolves `OLY_WEB_PUSH_PROXY` from the daemon's process environment
again; editing the file does not remove that inherited override. Changing an
environment variable in another shell does not change the running daemon.
`--no-http` and HTTP auth flags are startup choices, not JSON fields.

## Hot reload and restart boundaries

The daemon polls file mtime every **2 seconds**. A valid changed file replaces
the live snapshot and applies reload side effects. Read/JSON errors, removed byte-cap keys, and invalid
resume rules preserve the last good configuration; another file edit retries.
The same invalid file now fails startup rather than silently loading defaults.
Invalid prompt and resume regexes now reject startup/reload rather than being
silently skipped. Startup cannot fall back to defaults on an invalid existing file.

| Flat JSON key | Default | When it takes effect |
|---|---|---|
| `http_bind` | `127.0.0.1` | Restart required; existing listener is not rebound |
| `http_port` | `15443` | Restart required; 1..65535 |
| `notification_min_interval_seconds` | `10` | Live; minimum 1 second |
| `notification_hook` | none | Live; channels rebuilt on reload; null/empty disables hook |
| `prompt_patterns` | `DEFAULT_PROMPT_PATTERNS` below | Live on monitor tick; `[]` disables regex matching only |
| `resume_patterns` | `[]` | Live for future completed-output scans |
| `web_push_subject` | none | Live; channels rebuilt |
| `web_push_vapid_public_key` | generated on first load | Live; channels rebuilt |
| `web_push_vapid_private_key` | generated on first load | Live; channels rebuilt; secret |
| `web_push_proxy` | none | Live subject to CLI/environment overrides |
| `log_level` | `info` | Live tracing-filter reload |
| `trusted_apps_enabled` | `false` | Live; opt in to trusted custom apps/proxies and wwwroot overrides |
| `max_running_sessions` | `50` | Sampled at session start; 0..1000; 0 blocks new sessions |
| `session_eviction_seconds` | `15` | Live runtime-eviction delay; minimum 1 second |
| `screen_scrollback_rows` | `5000` | New session engines; 0..100000; existing engines retain their setting |
| `silence_seconds` | `10` | Live silence fallback; minimum 1 second |
| `stop_grace_seconds` | `5` | Live HTTP stop default when grace is omitted; minimum 1 second |
| `journal_retention_days` | `0` | Hourly stopped-session sweep; 0..36500; `0` keeps everything |

`http_bind` must be an IP address. Duration settings in seconds cannot exceed
31536000 (365 days); existing minimum-one-second clamps remain. Invalid values
reject startup/reload; no partial config is applied.

Attach seeds at least 1000 main-buffer history rows **when available**, or the
client height if larger, capped by that session's retained rows. This is not a
minimum imposed on `screen_scrollback_rows`; setting 250 retains at most 250.
Eviction releases completed runtime state, not persisted session history.

Live journal byte caps are **removed**: `max_journal_bytes_per_session`,
`max_output_log_bytes`, and the internal legacy spelling now fail with an
explicit error. An ordinary session stays in one incarnation, so the previous
older-incarnation deletion could not bound its recording. Remove those keys;
use an OS quota for disk bounds. Age retention permanently removes files and
DB rows for stopped/killed/failed sessions older than the configured `ended_at`
cutoff. Running sessions are excluded.

### Edit and verify

Use a file-reading tool/editor to inspect the resolved path; do not assume
`OLY_STATE_DIR` is set. Save valid JSON, then validate, for example:

```sh
jq . /resolved/state/config.json >/dev/null
```

Check `<STATE_DIR>/logs/` for:

- `config.json reloaded changed=...`
- `config changes require a daemon restart to take effect changed=...`
- `config reload failed; keeping current configuration`

Diff names use **internal fields**, not always JSON keys: `notification_hook`
reports `hook`, interval reports `min_interval_seconds`, resume rules report
`patterns`, listener fields report `bind`/`port`, and push fields report
`subject`/`vapid_public_key`/`vapid_private_key`/`proxy`. Verify the expected
behavior too; an unchanged/masked setting may produce no diff.

Do not restart a shared daemon just to apply a file edit. A restart stops all
live sessions; inspect them and obtain authorization first. If a restart is
approved, finish needed work, then:

```sh
oly daemon stop --grace 15
oly daemon start --detach   # omit any overrides you intend the file to control
oly daemon status
oly ls
```

Shutdown stops sessions concurrently: Ctrl-C, repeated Ctrl-C, then Ctrl-D
precede Unix SIGTERM at three-quarters of each grace window and tree kill at
its deadline. Explicit zero grace skips keys. Use enough grace for application
cleanup; keys are best-effort, not proof of clean exit. Resume derivation still
waits for child exit, PTY EOF, and journal durability in a bounded shutdown flush.
Stop can exit 0 with warnings about forced termination. Check final session
states, logs, and saved resume hints, not just the daemon-stop exit code.
If IPC says `Access is denied`, retry with available approval/escalation;
do not launch a replacement daemon
unless it is confirmed stopped.

## Prompt hints: `prompt_patterns`

The notification monitor examines a **rendered live-screen excerpt** (last 15
lines, ANSI stripped), not every historical/new output line. It runs once a
second with activity suppression and notification debounce. Regex matches can
raise attention before the longer silence fallback. There is no active LLM
prompt detector in the current implementation.

Retained OSC 7501 program-status reports take precedence over regex/silence:
working suppresses attention; idle/done/blocked/error set input-needed immediately
while the process is running, independently of notification delivery. Non-running
processes never need input. Idle/done readiness is not a blocked approval request.
These states can notify through the same activity suppression and
minimum interval. Duplicate reports and ordinary redraws do not re-notify.
Notifications use trigger rule `program_status`; done/error use notification
kind `program_status`, as does idle ("Ready for instruction"); blocked uses
`input_needed`. Idle means readiness, not completed work. These are untrusted child
claims, not authorization or verified completion. No extra config is required.

**Omitting** `prompt_patterns` restores the defaults. **`[]` disables regex
matching**, but silence-based attention hints/notifications remain. A nonempty
array replaces the complete list; to extend it, copy the defaults and append.
Patterns are Rust regexes matched per nonempty line; array order can affect
which matching rule is reported. Use anchors where appropriate to reduce false
positives. Prompt/input-needed and idle are hints, never proof of task completion
or authorization to approve an action.

### DEFAULT_PROMPT_PATTERNS

These strings mirror `src/config.rs`. Wrap the fragment in `{ ... }` to make a
complete file; regex backslashes must be doubled in JSON.

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

A complete custom example (replaces the defaults):

```json
{
  "notification_min_interval_seconds": 30,
  "prompt_patterns": ["(?i)ok to proceed\\?\\s*$", "(?i)approve\\?"]
}
```

Test a benign session with a known prompt; wait beyond activity suppression and
confirm `trigger_rule=regex_pattern` and the matching `pattern` in daemon logs.
Confirm actual notification delivery if that is the intended change; reading
logs successfully or seeing an idle process is not sufficient.

## Resume hints: `resume_patterns`

Rules inspect the **rendered journal tail after child exit, PTY EOF, and journal
durability**. The latest matching occurrence produces `resume_command` in SQLite
and session summaries (`oly info <ID> --json`, `oly ls --json`). It is untrusted
child text and a suggestion only — never automatically executed.

There are **no built-in resume rules**. Absent/empty disables detection. Existing
already-scanned sessions are not rescanned after edits.

Each rule requires:

- `program`: child executable basename, case-insensitive, ignoring `.exe`, `.cmd`,
  `.bat`. A `bash`/`npx` wrapper does not match a rule for the inner tool.
- `pattern`: Rust regex with at least one capture group.
- `command`: template using `$1`, `$2`, etc. from captures.

```json
{
  "resume_patterns": [
    {
      "program": "codex",
      "pattern": "(?i)(?:^|[^a-z0-9_])codex(?:\\.exe)?[ \\t]+resume[ \\t]+([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})(?:$|[^a-z0-9-])",
      "command": "codex resume $1"
    },
    {
      "program": "pi",
      "pattern": "(?i)(?:^|[^a-z0-9_])pi(?:\\.exe)?[ \\t]+--session[ \\t]+(\"[a-z0-9_./:\\\\~ -]{1,1024}\"|'[a-z0-9_./:\\\\~ -]{1,1024}'|[a-z0-9_./:\\\\~-]{1,1024})",
      "command": "pi --session $1"
    }
  ]
}
```

These are example output shapes, not guarantees about a tool's current CLI.
Verify against its real output. Invalid regexes, missing capture groups, or
empty program/command reject reload and preserve the previous config. After a
valid edit, finish a benign session whose **actual executable** matches the
rule and confirm the saved hint via `info --json` after the completed-output scan.
