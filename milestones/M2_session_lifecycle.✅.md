# Milestone 2: Session Lifecycle MVP

> **Status:** ✅ Done — verified via integration tests on current target OS  
> **SPEC ref:** §13 M2

## Acceptance criteria

**Done when:** manual flow works end-to-end:
1. start session
2. attach and interact
3. detach
4. reattach with replay
5. inspect logs
6. send automation input without attach
7. stop session
8. view logs

---

## Tasks

### Session create / list

- [x] Implement `oly start [--title <title>] [--detach] <cmd> [args...]`.
- [x] Generate stable UUID session ids (7-char prefix).
- [x] Persist initial session metadata at creation.

### PTY stream + attach / detach

- [x] Spawn child process in PTY and capture output stream.
- [x] Implement attach bridge (stdin → PTY, PTY → stdout).
- [x] Implement detach sequence (`Ctrl-]` then `d`) without terminating child.
- [x] Replay ring buffer before live stream on reattach.

### Stop + logs + input

- [x] Implement `oly stop <id> [--grace <seconds>]` (graceful SIGTERM/TerminateProcess then force-kill fallback).
- [x] Implement `oly logs <id> [--tail <n>] [--keep-color] [--no-truncate] [--wait-for-prompt] [--timeout <ms>]` from persisted logs.
- [x] Default `--tail` to 40 lines.
- [x] Implement `oly input <id> [--text <text>]...` for non-attach PTY input.
- [x] Support piped stdin for input automation (`cmd xxx | oly input <id>`).
- [x] Add repeatable `--key <key>` with named keys, modifier forms (`ctrl/alt/meta/shift/capslock`), and dash-style aliases.

### Verification

- [x] End-to-end flow validated by integration tests (start → input/interaction → logs → stop/exit-path coverage) on current target OS.
- [x] Error paths return non-zero exit codes with clear messages:
  - daemon unavailable
  - session not found
  - spawn failure
  - session evicted from memory (interactive ops must fail gracefully)
