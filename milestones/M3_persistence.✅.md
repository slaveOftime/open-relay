# Milestone 3: Persistence + Notifications MVP

> **Status:** ✅ Done — persistence + notification debounce verified  
> **SPEC ref:** §13 M3

## Acceptance criteria

**Done when:**
- After daemon restart, completed sessions remain listable and logs remain readable.
- Notification triggers exactly once per debounce window for a matching prompt + silence case.

---

## Tasks

### Persistence

- [x] Resolve platform-specific state directory:
  - Windows: `%LOCALAPPDATA%/oly`
  - Linux: `$XDG_STATE_HOME/oly` or `~/.local/state/oly`
  - macOS: `~/Library/Application Support/oly`
- [x] Write daemon runtime logs to `<state>/logs/daemon.log` (daily rolling).
- [x] Write `meta.json` on session lifecycle changes.
- [x] Append all PTY output to `output.log` (append-only).
- [x] Track lifecycle events in `events.log`.
- [x] Daemon restart must restore session metadata; `list` + `logs` work for completed sessions.
- [x] Support optional `<state>/config.json` override (at minimum `session_eviction_seconds`).

### Session directory naming

- [x] Use `YYYY-MM-dd_HH-mm-ss_<session-id>_<title-or-cmd-hint>` (title/hint truncated to 20 chars).

### SQLite index (post-M3 addition, already implemented)

- [x] Introduce SQLite database (`src/db.rs`) as secondary index for fast list/search queries across sessions.
- [x] Migrations in `migrations/` (0001 sessions table).

### In-memory eviction

- [x] Completed sessions retained in daemon memory for configurable retention window (default 900 s).
- [x] After eviction, `attach` / `input` fail with clear eviction error; persisted data remains.

### Notification engine (local only)

- [x] Strip / neutralize ANSI control sequences before regex matching (see `notification/prompt.rs`).
- [x] Prompt-like regex matching with configurable default patterns (e.g. `(?i)(y/n)`, `(?i)password:`, `>\s*$`).
- [x] Silence timer: no new output for X seconds (default 8 s) as second trigger condition.
- [x] Anti-noise debounce: suppress duplicate notifications for same session within 30 s window.
- [x] Disable notification once session exits.
- [x] Emit local OS notification with session id + short output excerpt.
- [x] Notification payload structured enough for machine consumers (see `notification/event.rs`).

### Verification

- [x] After daemon restart, `oly ls` shows completed sessions from persisted metadata.
- [x] `oly logs <id>` reads existing `output.log` after restart.
- [x] Prompt + silence case emits exactly one local notification per 30 s debounce window.
