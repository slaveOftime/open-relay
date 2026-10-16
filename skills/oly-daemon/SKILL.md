---
name: oly-daemon
description: Use when reading or updating oly daemon configuration, checking hot-reload behavior, or configuring prompt and session-resume hints. For running and supervising sessions, use the oly skill instead.
---

# Configure the oly daemon

Configuration lives at `<STATE_DIR>/config.json`. Read the existing file before editing and preserve unrelated fields and generated secrets.

- **Flat JSON only:** Keys are top-level, not JSON-with-comments and not nested under `limits`, `notify`, or `web_push`.
- **Validation:** Malformed JSON, out-of-range values, invalid regexes, or removed byte-cap keys fail startup and reject hot reload (preserving the last good config on reload). Unknown keys are ignored with a warning naming each key.

## 1) Locate state and precedence

Run `oly daemon status` to inspect `ROOT` (`<STATE_DIR>`), `LOGS`, `SESSIONS`, `HTTP`, `Auth`, and `ID-PUB-KEY`.

| Platform | Default state directory |
|---|---|
| Override | `$OLY_STATE_DIR` (must be set before daemon launch) |
| Windows | `%LOCALAPPDATA%\oly` |
| macOS | `~/Library/Application Support/oly` |
| Linux | `$XDG_STATE_HOME/oly` or `~/.local/state/oly` |

Startup precedence is **CLI flag > `config.json` > built-in default** (for `web_push_proxy`: **CLI > `OLY_WEB_PUSH_PROXY` > file > default**). CLI flags keep winning across hot reloads for the lifetime of the daemon process.

| Startup flag | Config field |
|---|---|
| `--bind <ADDR>` | `http_bind` (legacy alias: `bind`) |
| `--port <PORT>` | `http_port` |
| `--notification-hook <COMMAND>` | `notification_hook` |
| `--web-push-proxy <URL>` | `web_push_proxy` |

`--no-http` and `--no-auth` are startup-only flags, not JSON keys.

## 2) Config keys and hot-reload boundaries

The daemon polls `config.json` mtime every **2 seconds**. Duration settings in seconds are clamped/validated within `1..=31536000` (365 days).

| Flat JSON key | Default | When it takes effect |
|---|---|---|
| `http_bind` | `127.0.0.1` | Restart required; must be an IP address |
| `http_port` | `15443` | Restart required; `1..=65535` |
| `notification_min_interval_seconds` | `10` | Live (`>= 1`) |
| `notification_hook` | none | Live; null/empty disables hook |
| `prompt_patterns` | `DEFAULT_PROMPT_PATTERNS` | Live on monitor tick; `[]` disables regex matching only |
| `resume_patterns` | `[]` | Live for future completed-output scans |
| `web_push_subject` | none | Live; rebuilds push channels |
| `web_push_vapid_public_key` | generated on first load | Live; rebuilds push channels |
| `web_push_vapid_private_key` | generated on first load | Live; secret |
| `web_push_proxy` | none | Live (unless overridden by CLI/env) |
| `log_level` | `info` | Live tracing-filter reload |
| `trusted_apps_enabled` | `false` | Live; enables trusted `/apps/` and `wwwroot` overrides |
| `max_running_sessions` | `50` | Live at session start (`0..=1000`; `0` blocks new starts) |
| `session_eviction_seconds` | `15` | Live runtime-eviction delay after exit (`>= 1`) |
| `screen_scrollback_rows` | `5000` | New sessions only (`0..=100000`) |
| `silence_seconds` | `10` | Live silence fallback (`>= 1`) |
| `stop_grace_seconds` | `5` | Live HTTP stop default when grace is omitted (`>= 1`) |
| `journal_retention_days` | `0` | Hourly sweep of stopped/killed/failed sessions (`0..=36500`; `0` keeps all) |

Removed keys (`max_journal_bytes_per_session`, `max_output_log_bytes`, `max_journal_bytes_per_session_legacy`) fail explicitly; remove them and use `journal_retention_days` or OS disk quotas.

### Edit and verify

1. Validate syntax before saving: `jq . <STATE_DIR>/config.json >/dev/null`.
2. Inspect `<STATE_DIR>/logs/daemon.log` after ~2s for:
   - `config.json reloaded changed=...` (internal diff names: `hook`, `min_interval_seconds`, `patterns`, `bind`, `port`, `subject`, `vapid_public_key`, `vapid_private_key`, `proxy`)
   - `config changes require a daemon restart to take effect changed=...`
   - `config reload failed; keeping current configuration`
3. **Never restart a shared daemon without operator approval** — `oly daemon stop` terminates all live sessions. If a sandbox reports `Access is denied`, retry with escalation rather than starting a second daemon.

## 3) Prompt hints: `prompt_patterns`

The monitor checks the last 15 rendered live-screen lines (ANSI stripped) once per second with 5s activity suppression and `notification_min_interval_seconds` debounce. OSC 7501 `program_status` reports take precedence over regex/silence heuristics (`working` suppresses attention; `idle`/`done`/`blocked`/`error` set `input_needed` immediately while running).

- **Omitted `prompt_patterns`:** uses `DEFAULT_PROMPT_PATTERNS`.
- **`"prompt_patterns": []`:** disables regex matching, while silence-based attention hints/notifications remain.
- **Nonempty array:** replaces the full list (copy defaults below and append to extend).

### DEFAULT_PROMPT_PATTERNS

Matches `src/config.rs` (wrap in `{ ... }` for a full JSON file):

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

Custom override example:

```json
{
  "notification_min_interval_seconds": 30,
  "prompt_patterns": ["(?i)ok to proceed\\?\\s*$", "(?i)approve\\?"]
}
```

## 4) Resume hints: `resume_patterns`

Rules scan the rendered journal tail after child exit, PTY EOF, and journal durability. The latest match populates `resume_command` (`oly info <ID> --json`, `oly ls --json`) as an untrusted suggestion (never auto-executed). No rules are built in.

Each rule requires:
- `program`: child executable basename (case-insensitive, without `.exe`/`.cmd`/`.bat`; shell wrappers like `bash` do not match the inner tool).
- `pattern`: Rust regex with at least one capture group.
- `command`: non-empty template using `$1`, `$2`, etc.

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
