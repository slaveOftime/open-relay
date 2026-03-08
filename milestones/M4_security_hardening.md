# Milestone 4: Security Hardening MVP

> **Status:** ⏳ Not started  
> **SPEC ref:** §3.2, §11, §13 M4

## Acceptance criteria

**Done when:**
- An unauthorized local user/process cannot control sessions via IPC.
- Stop / exit does not leave hanging PTY read loops on Windows.
- Input injection events are audit-logged.

---

## Tasks

### IPC peer identity validation

- [ ] **Linux:** verify caller via `SO_PEERCRED`; reject connections from other UIDs.
- [ ] **macOS/BSD:** verify caller via `getpeereid()`; reject connections from other UIDs.
- [ ] **Windows:** enforce named-pipe ACL (current-user SID only) + validate client token on `ConnectNamedPipe`.
- [ ] Attribute each request to caller identity in `events.log`.

### Input injection audit trail

- [ ] Log every `oly input` invocation (caller identity, session id, byte count, key specs) to `events.log`.
- [ ] Implement guarded-mode policy for `oly input`:
  - Default: allow literal bytes + known key specs.
  - Optional `--strict` flag: reject high-risk shell metacharacter payloads unless explicitly overridden.

### Windows ConPTY reliability

- [ ] Child-exit watcher runs independently from PTY read loop.
- [ ] PTY reader is cancellable via shutdown token; confirmed not to hang on session stop.
- [ ] Job object / process-group ownership guarantees stop semantics even when terminal handles remain open.

### Session directory permissions

- [ ] Session directory and IPC endpoint are user-private by default (mode 0700 on Unix; user-only ACL on Windows).

### Cross-platform validation

- [ ] Validate full command parity on **Windows**.
- [ ] Validate full command parity on **Linux**.
- [ ] Validate full command parity on **macOS**.
- [ ] Document any OS-specific caveats discovered during testing.
