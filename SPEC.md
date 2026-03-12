# Specification: Open Relay (`oly`)

**Version:** 0.2.0  
**Language:** Rust  
**Status:** Implementation-Ready MVP

## 1. Purpose

`oly` is a session-persistent PTY relay for long-running interactive CLI workloads (AI agents, SSH, REPLs). A background daemon owns sessions. A CLI client can create, attach, detach, inspect logs, and stop sessions without killing work when a terminal closes.

`oly` is designed for unattended agent operation: users should not need to babysit long-running agent sessions while waiting for HITL checkpoints.

### 1.1 Product Positioning

`oly` is not a terminal multiplexer replacement. It is an **agent supervision relay** focused on Human-in-the-Loop (HITL) control for autonomous CLI workloads.

- Traditional multiplexers (`tmux`, `zellij`) provide persistence/windowing but are terminal-state blind for agent runtime intent.
- Agent IDE hooks can react to events, but typically lack a cross-platform, durable daemon process that standardizes long-running supervision.
- `oly` targets the gap: persistent PTY execution + explicit intervention channels (`attach`, `input`, notification) with auditable lifecycle state.

### 1.2 Target User

- individual developers (solo-first) using agent CLI tools (e.g., Claude Code, GitHub Copilot CLI, Gemini CLI, OpenCode) who need reliable detach/reattach, async monitoring, and fast intervention for long-running coding tasks without constant terminal babysitting.

- platform engineers / agentic architects running many unattended sessions and requiring approval checkpoints for risky or costly actions.

### 1.3 Primary Operator Model (Solo-first)

- The primary operator model is: one developer running autonomous agent sessions and intervening only at meaningful checkpoints.
- Remote supervision is a first-class operational pattern: operator may expose access through a user-managed tunnel and authentication gateway, then monitor/intervene from anywhere.
- Multi-agent supervision is in scope at product level: one agent CLI can supervise another, escalate uncertainty, and request higher-permission approval from the human operator.

## 2. Scope

### 2.1 MVP (in scope for v0.1)

1. Local daemon process managed by the same `oly` binary.
2. CLI commands:
	 - `oly daemon start`
   - `oly start [--title <title>] [--detach] [--cwd <dir>] [--node <name>] <cmd> [args...]`
   - `oly stop <id> [--grace <seconds>] [--node <name>]`
   - `oly ls [--search <text>] [--status <status>]... [--since <RFC3339>] [--until <RFC3339>] [--limit <n>] [--node <name>]`
   - `oly attach <id> [--node <name>]`
   - `oly send <id> [CHUNK]... [--node <name>]`
   - `oly logs <id> [--tail <n>] [--keep-color] [--no-truncate] [--wait-for-prompt] [--timeout <ms>] [--node <name>]`
3. PTY-backed child process execution with detach/reattach.
4. Rolling in-memory output buffer (default 10,000 lines) plus on-disk log persistence.
5. Local notification when input is likely required.
6. Cross-platform support from day 1: Windows, Linux, macOS.
7. Documented operator pattern for remote supervision via external tunnel + auth gateway in front of local daemon IPC (no embedded network listener in MVP).

### 2.2 Explicitly out of scope for MVP

1. HTTP/REST/WebSocket server and OpenAPI docs.
2. Web UI / PWA push notifications.
3. Crash-time orphan process adoption.
4. Built-in remote listener, built-in auth gateway, and hosted control plane.
5. Policy engine for organization-wide approval workflows.

These are post-MVP milestones, not blockers for v0.1 release.

## 3. High-Level Architecture

Single binary, two roles:

- **Daemon role**
	- Owns all live sessions.
	- Starts PTY child processes.
	- Maintains in-memory ring buffer and appends logs to disk.
	- Runs input-needed detection and emits local notifications.
- **CLI role**
	- Sends control requests to daemon through local IPC.
	- Streams PTY bytes during `attach`.

### 3.1 Local IPC requirement

- Must support bidirectional byte streaming and request/response RPC.
- Must be local-machine only for MVP.
- Transport can be Unix domain socket / named pipe abstraction, as long as behavior is equivalent across target OSes.
- Remote access, when needed, is achieved outside the daemon process using operator-managed tunneling/gateway layers that terminate auth before forwarding to local IPC.

### 3.2 IPC Authentication and Authorization (MVP Mandatory)

- IPC server must reject non-local callers.
- Unix: verify peer identity via `SO_PEERCRED` (Linux) or `getpeereid()` (macOS/BSD), and require same user by default.
- Windows: use named-pipe ACLs + client token validation (same-user default).
- Requests must be attributed to caller identity for auditing in `events.log`.

### 3.3 Module layout

### 3.2 Module layout

### 3.3 Module layout

```
src/
  main.rs              – Entry point and CLI dispatch.
  cli.rs               – CLI argument structs (clap).
  config.rs            – AppConfig with runtime defaults.
  daemon.rs            – Daemon lifecycle (start / stop / lock).
  ipc.rs               – Local-socket transport (connect / bind / read / write).
  protocol.rs          – RPC envelope, request/response types, NodeWsMessage, ListQuery.
  storage.rs           – Filesystem utilities (state dir, session dir lookup, disk list).
  notification/        – Notification channel, prompt detection, dispatch pipeline.
  error.rs             – AppError and Result alias.
  session/
    mod.rs             – SessionMeta, SessionStatus, StartSpec, SessionLookupError.
    store.rs           – SessionStore (in-memory runtime registry, eviction).
    runtime.rs         – SessionRuntime, RuntimeChild, PTY spawning, output buffering.
    persist.rs         – Disk helpers (meta.json, output.log, events.log, format_age).
  node/
    mod.rs             – re-exports
    registry.rs        – NodeRegistry, NodeHandle (channels for WS relay).
  client/
    mod.rs             – Re-exports for all client-side commands.
    attach.rs          – run_attach, RawModeGuard, terminal query responses.
    input.rs           – run_send, key-spec parsing.
    list.rs            – run_list, query building, display formatting.
    logs.rs            – run_logs, VT100 frame building, replay viewer.
    join.rs            – JoinConfig, run_join, run_join_stop, connector task.
  http/
    mod.rs             – Axum router, AppState, SSE event broadcast.
    auth.rs            – Password auth, token issuance, rate limiting.
    sessions.rs        – Session CRUD and log endpoints.
    sse.rs             – Server-sent events stream.
    ws.rs              – WebSocket PTY attach handler.
    nodes.rs           – /api/nodes/* endpoints (join WS + node list).
```

## 4. Session Model

Each session has:

- `id` (7 chars from stable UUID)
- `title` (optional short text)
- `command` + `args`
- `cwd` (optional, if not provided then, it means it should be under the folder of the current session)
- `created_at`, `started_at`, `ended_at`
- `status`: `created | running | stopping | stopped | failed`
- `pid` (if running)
- `exit_code` (if stopped)

## 5. CLI Contract

### 5.1 Create session

`oly start [--title <title>] [--detach] [--cwd <dir>] [--node <name>] <cmd> [args...]`

- Spawns `<cmd>` in PTY under daemon ownership.
- `--cwd` overrides the working directory; when omitted, the CLI sends its own current working directory.
- Returns session id on success.
- By default, immediately attaches to the newly created session after printing id.
- `--detach` skips auto-attach and exits after printing id.
- Exit code:
	- `0` success
	- non-zero for daemon unavailable, invalid args, or spawn failure.

### 5.2 List sessions

`oly ls [--search <text>] [--status <status>]... [--since <RFC3339>] [--until <RFC3339>] [--limit <n>] [--node <name>]`

- Displays active and completed sessions with id, title, status, and age.
- `--search` filters by title or id substring (case-insensitive).
- `--status` is repeatable and supports: `created`, `running`, `stopping`, `stopped`, `failed`, `unknown`.
- `--since` and `--until` filter by created timestamp in RFC3339 format.
- `--limit` defaults to `10`.

### 5.3 Attach session

`oly attach <id> [--node <name>]`

- Replays buffered output first, then switches to live stream.
- Forwards local keyboard input to PTY.
- Supports detach escape sequence: `Ctrl-]` then `d`.
- Detach does **not** stop the child process.

### 5.4 Stop session

`oly stop <id> [--grace <seconds>] [--node <name>]`

- Attempts graceful termination first; force-kills after grace timeout.
- Grace default: `5` seconds.

### 5.5 Read logs

`oly logs <id> [--tail <n>] [--keep-color] [--no-truncate] [--wait-for-prompt] [--timeout <ms>] [--node <name>]`

- Reads persisted output without attaching.
- `--tail` defaults to `40` lines.
- By default, output strips ANSI color sequences unless `--keep-color` is set.
- `--no-truncate` disables column truncation.
- `--wait-for-prompt` blocks until the session needs input (or exits), then prints logs.
- `--timeout` is in milliseconds and defaults to `30000`; set `0` for no timeout.

### 5.6 Send input without attach

`oly send <id> [CHUNK]... [--node <name>]`

- Sends input bytes to the target session PTY without opening `attach`.
- Supports automation flow driven by `oly logs <id>` output with agent analysis.
- Accepts piped stdin (example: `cmd xxx | oly send <id>`); cannot be combined with positional chunks.
- Positional chunks are processed left to right. Plain text is sent literally. Prefix with `key:` for special keys.
- `key:<spec>` sends terminal key/control sequences, including named keys (`enter`, `tab`, `esc`, arrows, `home/end`, `pgup/pgdn`, `del/ins`, `backspace`), modifier forms (`ctrl+<char>`, `alt+<char|named-key>`, `meta+<char|named-key>`, `shift+tab`), and raw bytes (`hex:<bytes>`).

## 6. PTY + Buffer Requirements

1. PTY must support interactive programs (line editing, prompts, ANSI output).
2. Output bytes are appended to:
	 - in-memory ring buffer for fast replay,
	 - on-disk log segment for persistence.
3. On reattach, daemon sends ring buffer snapshot before live stream.
4. Ring buffer size configurable; default `10,000` lines.
5. Completed sessions (`stopped` / `failed`) remain in daemon memory for a retention window (default `900s`), then are evicted from memory.
6. After memory eviction, interactive daemon operations (`attach` / `input`) must fail with a clear eviction error, while persisted logs/metadata remain on disk.

## 7. Input-Needed Detection Strategy

Detection must be robust against ANSI-heavy TUI output and platform PTY quirks.

### 7.1 Multi-signal model

MVP trigger when both are true:

1. Prompt-like pattern match is observed in canonicalized recent output.
2. No new output for `X` seconds (default `8s`).

Post-MVP quality upgrades:

- OS-level wait-state hints (best-effort) may increase confidence score.
- Session can emit explicit machine-readable signal (see OSC extension in section 14).
- Integrate LLM directly to check the result (ollama with tiny models are enough)

### 7.2 Canonicalization requirements (MVP Mandatory)

Before regex matching, daemon must normalize stream:

- Strip or neutralize ANSI control sequences (CSI/OSC/DCS and common cursor controls).
- Preserve printable text ordering.
- Keep short trailing window (configurable bytes/lines) for prompt matching.

### 7.3 Platform notes

- Linux/macOS: process-state hints are advisory only; never sole trigger.
- Windows/ConPTY: pipe liveness is not equal to child liveness; detector must not block forever on PTY read assumptions.

## 8. Notification Engine (MVP)

MVP supports local notifications only.

Trigger when both are true:

1. Recent output matches a prompt-like regex set (default examples: `(?i)(y/n)`, `(?i)password:`, `>\s*$`).
2. No new output for `X` seconds (default `8s`).

Action:

- Emit a local OS notification with session id and short excerpt.

Anti-noise requirements:

- Debounce duplicate notifications for same session within `30s` window.
- Disable notification if session exits.

Escalation semantics:

- Notification payload must be structured enough to support machine consumers (e.g., supervisor-agent adapters) in addition to human-readable local notifications.
- A supervising agent workflow may classify an event as `unsure` or `higher_permission_required` and escalate to human approval.

## 9. Persistence Contract

Base directory:

- `%LOCALAPPDATA%/oly` on Windows.
- `$XDG_STATE_HOME/oly` or `~/.local/state/oly` on Linux.
- `~/Library/Application Support/oly` on macOS.

Layout:

```text
<state>/
	logs/
		daemon.log
	sessions/
		YYYY-MM-dd_HH-mm-ss_<session-id>_<title or hint of cmd and args (truncate to 20 chars)>/
			meta.json
			output.log
			events.log
```

`meta.json` must include session model fields from section 4.

Durability rules:

1. `meta.json` written on lifecycle changes.
2. `output.log` append-only for process output.
3. `logs/daemon.log` stores daemon runtime logs with daily rolling.
4. Daemon restart must restore session metadata and allow `list` + `logs` for completed sessions.

Optional local config file:

- `<state>/config.json` may override runtime defaults.
- MVP supports `session_eviction_seconds` override for completed-session in-memory retention.

## 10. Platform Requirements

Target platforms: Windows, Linux, macOS.

Minimum parity for MVP:

1. Create/list/attach/detach/stop/logs/input behavior is functionally equivalent.
2. PTY resize and control sequences work for common terminals.
3. Graceful stop uses best native mechanism per OS, then hard kill fallback.

Windows-specific reliability requirements:

1. ConPTY session supervision must include explicit child-exit watcher independent from PTY read loop.
2. PTY reader must be cancellable (shutdown token or equivalent) to prevent indefinite hangs.
3. Job object/process-group ownership must guarantee stop semantics even if terminal handles remain open.

Known platform caveats are allowed if documented in release notes.

## 11. Security Requirements (MVP)

1. IPC caller identity verification is mandatory (section 3.2).
2. `oly send` must support a guarded mode for automation/supervisor channels:
	- default allows literal bytes + known key specs,
	- optional strict policy rejects high-risk shell metacharacter payloads unless explicitly overridden,
	- all injected input events are audit-logged.
3. Session directory and IPC endpoint permissions must be user-private by default.
4. When operators expose access remotely via tunnel/gateway, authN/authZ enforcement must happen at the gateway boundary; daemon trust boundary remains local IPC identity + audit trail.

### 11.1 HTTP Authentication

The HTTP/WebSocket API is protected by an interactive password set at daemon start.

**Default mode (password required):**

- `oly daemon start` prompts for a password (hidden, no echo) and a confirmation before starting.
- For `--detach` mode, the parent process prompts interactively, hashes the password with Argon2id + `OsRng` salt, and forwards the hash to the background child via a hidden CLI argument. The plaintext password is never stored or logged.
- On daemon restart, all active tokens are invalidated; users must re-authenticate.

**Login flow:**

1. `GET /api/auth/status` → `{ "auth_required": true }` — always public, no token required.
2. `POST /api/auth/login` with `{ "password": "..." }` → `200 { "token": "<uuid>" }` on success.
3. All subsequent API calls include `Authorization: Bearer <token>`.
4. `POST /api/auth/logout` revokes the token.

**Brute-force protection:**

- 3 failed login attempts → 15-minute global lockout.
- `429 Too Many Requests` response with `Retry-After` header during lockout.
- On success, the failure counter resets.

**Bypassed endpoints (no token required):**

- `GET /api/health` — liveness probe.
- `GET /api/auth/status`, `POST /api/auth/login` — login flow itself.
- Static web assets (non-`/api/` paths).

**No-auth mode (`--no-auth`):**

- `oly daemon start --no-auth` disables HTTP authentication entirely.
- User must type `yes` to acknowledge the security risk before the daemon starts.
- Use only behind a secure gateway or with the port not publicly accessible.
- IPC (CLI commands) remain independently protected by OS peer-credential validation (§3.2).

**Web UI behaviour:**

- When `auth_required: true`, a full-screen non-dismissible login dialog is shown before any session data is accessed.
- Incorrect password shows remaining attempt count; after lockout a live countdown is displayed.
- A logout button is shown in the top-right header when authenticated.

## 12. Error Handling

1. Clear user-facing error messages for daemon unavailable, session not found, and spawn failure.
2. Non-zero exit code for all failed CLI operations.
3. Daemon must not crash if one session crashes; isolate session failures.

## 13. Milestones and Acceptance Criteria

### M1: Foundation

- CLI skeleton and daemon bootstrap exist.
- Local IPC request/response is functional.

**Done when:** `oly daemon start` and `oly ls` succeed on all target OSes.

### M2: Session lifecycle MVP

- `session create`, `attach`, `input`, `stop`, `logs` implemented.
- PTY stream and detach/reattach behavior implemented.

**Done when:** manual flow works end-to-end:
1. start session,
2. attach and interact,
3. detach,
4. reattach with replay,
5. inspect logs,
6. send automation input without attach,
7. stop session,
8. view logs.

### M3: Persistence + notifications MVP

- Metadata/log persistence implemented.
- Local input-needed notification implemented with debounce.

**Done when:** after daemon restart, completed sessions remain listable and logs remain readable; notification triggers exactly once per debounce window for a matching prompt + silence case.

### M4: Security hardening MVP

- IPC peer identity validation enforced on all supported OSes.
- Input injection audit trail persisted.
- ConPTY cancellable reader and exit watcher validated on Windows.

**Done when:** unauthorized local user/process cannot control sessions; stop/exit does not leave hanging read loops on Windows test matrix.

### M5c: HTTP password authentication

- Interactive password prompt at `oly daemon start`; Argon2id hash held in memory only.
- `POST /api/auth/login` issues Bearer tokens; 3-attempt lockout with 15-minute cooldown.
- Auth middleware enforces tokens on all `/api/*` routes except health and auth itself.
- `--no-auth` mode with explicit risk acknowledgment.
- Web UI login dialog with lockout feedback; logout button in header.

**Done when:** unauthenticated `curl /api/sessions` returns 401; web UI shows login dialog; 3 wrong passwords trigger lockout; `--no-auth` requires confirmation.

### M13: Primary/secondary node federation

- `oly api-key add <name>` generates an Argon2id-hashed API key, stores it in DB, prints the plaintext key once.
- `oly join start --name <name> --key <key> <primary_url>` on the secondary connects the secondary's daemon to the primary via a persistent WebSocket.
- The primary's `NodeRegistry` registers the live connection and proxies any `NodeProxy` RPC to the named secondary.
- `--node <name>` flag on all session commands wraps the IPC payload in `NodeProxy { node, inner }`.
- `oly api-key ls` / `oly api-key remove` manage API keys.
- `oly node ls` shows currently connected secondary nodes.
- Secondary connector auto-reconnects with exponential backoff; join config persists across daemon restarts.

**Done when:** `oly api-key add worker-key` on primary prints a key; `oly join start` on secondary shows connected in `oly node ls`; `oly start --node worker1 -- sleep 30` creates a session on the secondary; `oly ls --node worker1` returns it; `oly attach --node worker1 <id>` provides interactive PTY; restarting the secondary daemon auto-reconnects.

## 14. Strategic Backlog (Post-MVP)

1. HTTP/REST + WebSocket API.
2. OpenAPI docs.
3. Web client integration.
4. PWA push notifications.
5. Orphan process adoption and recovery strategy.
6. Built-in remote auth + TLS boundary (for deployments that do not use external gateway/tunnel). Basic password auth is implemented (M5c); full TLS + role-based tokens are in M9.
7. Standardized agent PTY signaling sequence (proposed `OSC 888`) for explicit `needs-input / unsure / checkpoint / higher-permission-required` events.
8. Supervisor-of-supervisor workflows (agent supervising agent) with explicit escalation contracts to human operator.
9. Enterprise policy adapters (approval gates, risk scoring, cost/risk checkpoint).
10. **Primary/secondary node federation** (M13): multiple daemons connect to a single primary; all CLI/web commands are transparently proxied to the target node via `--node <name>`.

## 15. Remote Supervision Deployment Pattern (Non-normative)

Recommended pattern for solo operators who need anywhere access before built-in remote API exists:

1. Keep `oly` daemon local-only (no direct network listener).
2. Place a user-managed tunnel in front of host machine access.
3. Place an auth gateway in front of the relay/control endpoint with identity-aware access controls.
4. Require explicit approval path for supervisor-agent escalations (`unsure`, `higher_permission_required`).
5. Ensure all remotely initiated control actions remain attributable in `events.log`.

## 16. Primary/Secondary Node Federation

### 16.1 Overview

One daemon is designated the **primary** — the single publicly-exposed entry point. Daemons on other machines join as **secondaries** by opening a persistent authenticated WebSocket to the primary. All existing CLI and web-UI commands work transparently against any node via `--node <name>`.

### 16.2 Node lifecycle

```
Primary side                       Secondary side
─────────────────────────────────  ────────────────────────────────────
oly api-key add worker-key         oly join start --name worker1 \
  → prints API key                         --key <key> http://primary:15443
                                     → saves join config, daemon opens WS
oly api-key ls                     oly join stop --name worker1
  → lists registered keys            → removes config, closes WS
oly node ls
  → name, connected, last_seen
oly api-key remove worker-key
  → key deleted; future joins with key fail
```

### 16.3 Wire protocol

All messages are JSON. The secondary initiates with a `join` handshake; after the primary sends `joined`, the channel enters RPC relay mode.

**Message types** (`NodeWsMessage`):

| Direction            | `type`          | Fields                             |
|----------------------|-----------------|------------------------------------|
| Secondary → Primary  | `join`          | `name`, `key`                      |
| Primary → Secondary  | `joined`        |                                    |
| Primary → Secondary  | `error`         | `message`                          |
| Primary → Secondary  | `rpc`           | `id`, `request` (RpcRequest JSON)  |
| Secondary → Primary  | `rpc_response`  | `id`, `response` (RpcResponse JSON)|
| Either               | `ping` / `pong` |                                    |

### 16.4 IPC protocol extension

`RpcRequest` gets a new variant:

```
NodeProxy { node: String, inner: Box<RpcRequest> }
```

When the primary's `handle_client` receives a `NodeProxy` request, it:
1. Looks up `node` in `NodeRegistry`.
2. Serialises `inner` to JSON and sends an `rpc` message over the WS.
3. Awaits the `rpc_response` on a one-shot channel.
4. Returns the inner `RpcResponse` to the CLI caller as if it were local.

### 16.5 Key management

- API keys are 32 random bytes, hex-encoded (64 chars), generated by the primary daemon.
- The primary stores only the Argon2id PHC hash in the `api_keys` DB table.
- The plaintext key is printed once on `oly api-key add` and is never stored or logged.
- The secondary stores its own key in `<state_dir>/joins.json` (user-private permissions).

### 16.6 `oly ls` default scope

`oly ls` (no `--node`) always shows local sessions only. Use `--node <name>` or `--node all` (future) for remote sessions. This avoids accidental cross-node confusion and keeps the default fast.

### 16.7 Security boundary

This feature assumes operation over a trusted LAN or VPN. TLS hardening (M9) can be layered on top for public exposure. The Argon2id key check prevents unauthorized secondaries from registering, but the wire traffic is not encrypted by the feature itself.

### 16.8 Module layout additions

```
src/
  node/
    mod.rs          – re-exports
    registry.rs     – NodeRegistry, NodeHandle (HashMap + mpsc + oneshot channels)
  client/
    join.rs         – JoinConfig, connector task, load/save joins.json
  http/
    nodes.rs        – /api/nodes/join WS handler, /api/nodes GET handler
migrations/
  0003_migrate_to_api_keys.sql
```

## 17. Licensing

MIT
