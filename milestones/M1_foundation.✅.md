# Milestone 1: Foundation

> **Status:** ✅ Complete  
> **SPEC ref:** §13 M1

## Acceptance criteria

**Done when:** `oly daemon start` and `oly ls` succeed on all target OSes.

---

## Tasks

### Build scaffolding

- [x] Add crate dependencies in `Cargo.toml` for MVP (`clap`, `tokio`, `serde`, `serde_json`, PTY crate, local notification crate).
- [x] Define module layout (`cli`, `daemon`, `ipc`, `session/`, `session/store`, `session/runtime`, `session/persist`, `storage`, `notification/`, `protocol`, `config`, `client/`, `client/attach`, `client/input`, `client/list`, `client/logs`).
- [x] Add `AppConfig` with runtime defaults (ring buffer size, silence seconds, debounce seconds, stop grace seconds).

### Daemon bootstrap

- [x] Implement `oly daemon start` entrypoint.
- [x] Implement `oly daemon stop` to terminate the daemon.
- [x] Ensure single daemon instance per user profile (lock file).
- [x] Add clean startup/shutdown logging.

### IPC baseline

- [x] Implement local-only IPC transport abstraction (Unix socket / named pipe).
- [x] Implement request/response for health and list commands.
- [x] Add protocol version field for forward compatibility.

### Verification

- [x] `oly daemon start` launches and stays running.
- [x] `oly ls` returns empty list (not an error) on fresh state.
