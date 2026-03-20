# Open Relay Architecture (Source of Truth)

This document is the authoritative, repository-wide architecture reference for `open-relay` (`oly`).  It covers system design, runtime internals, IPC/WebSocket protocols, and the full feature specification.  Future agents and contributors should start here before opening any source file.

PTY and terminal behavior live in [`ARCHITECTURE_PTY.md`](./ARCHITECTURE_PTY.md).
Detailed edge cases, limitations, and operational notes live in
[`ARCHITECTURE_NOTES.md`](./ARCHITECTURE_NOTES.md).

---

## Table of Contents

1. [System Overview](#1-system-overview)
2. [High-Level Component Diagram](#2-high-level-component-diagram-ascii)
3. [Repository Structure and Responsibilities](#3-repository-structure-and-responsibilities)
4. [Core Runtime Flows](#4-core-runtime-flows)
5. [Data and State Model](#5-data-and-state-model)
6. [PTY Integration Overview](#6-pty-integration-overview)
7. [IPC Protocol Reference](#7-ipc-protocol-reference)
8. [WebSocket Attach Protocol](#8-websocket-attach-protocol)
9. [Feature Specification](#9-feature-specification)
10. [Build, Run, and Test Surfaces](#10-build-run-and-test-surfaces)
11. [Configuration Reference](#11-configuration-reference)

---

## 1) System Overview

Open Relay is a Rust-based daemon + CLI for running and managing long-lived interactive PTY sessions, with optional web UI and node federation.

- **CLI (`oly`)** starts, lists, attaches to, controls, and stops sessions.
- **Daemon** owns session runtimes, persistence, IPC/HTTP APIs, auth, and notifications.
- **Web app** consumes daemon APIs (REST + SSE + WebSocket).
- **SQLite** stores session metadata, auth artifacts, and push subscriptions.
- **Federation** allows secondary nodes on other machines to relay and proxy sessions.

Primary references:

- `src/main.rs`
- `src/daemon/*`
- `src/session/*`
- `src/http/*`
- `src/db.rs`
- `web/src/*`

## 2) High-Level Component Diagram (ASCII)

```text
                                          Human use it
                                  +-------------------------+
                                  |      Web Browser UI     |
                                  |   (React + xterm.js)    |
                                  +-----------+-------------+
                                              ^
                                              |
                             REST / SSE / WS  |
 Human/Agent use it                           v
+------------------+      IPC RPC       +------+---------------------+       WS          +--------------------+
|   oly CLI        +------------------->|       Oly Daemon           |<----------------->|  Secondary node    |
| (start/list/... )|  (Unix/Named Sock) |  - request router          |                   |   on other pc      |
+---------+--------+                    |  - auth + config           |                   |                    |
                                        |  - notifications           |                   |                    |
                                        +------+---------------------+                   +--------------------+
                                               |
                                               | manages
                                               v
                                      +--------+---------------------+
                                      | Session Store + Runtime      |
                                      | - PTY child process          |
                                      | - ring buffer / polling      |
                                      | - resize/input/stop          |
                                      +--------+---------------------+
                                               |
                                               | persists metadata/logs
                                               v
                                      +--------+---------------------+
                                      | SQLite + log files           |
                                      | - sessions/api_keys/push     |
                                      | - output.log/events.log      |
                                      +------------------------------+
```

## 3) Repository Structure and Responsibilities

### Rust backend (core)

- **CLI + command dispatch**
  - `src/main.rs` - command routing, daemon spawn/check, CLI behavior.
  - `src/cli.rs` - clap command model and options.
  - `src/client/attach.rs`, `input.rs`, `list.rs`, `logs.rs` - human/agent-facing CLI command implementations.
  - `src/client/join.rs` - persisted join config management and CLI join/start-stop flows.
- **Daemon lifecycle + IPC handling**
  - `src/daemon/lifecycle.rs` - daemon startup/shutdown, lock/pid handling, foreground runtime orchestration.
  - `src/daemon/rpc.rs` - IPC request entrypoint and top-level dispatch.
  - `src/daemon/rpc_attach.rs` - streaming attach/input/resize/detach handlers.
  - `src/daemon/rpc_nodes.rs` - node proxy handlers plus secondary-node connector runtime.
  - `src/daemon/auth.rs` - password prompt and no-auth safety confirmation helpers.
  - `src/ipc.rs` - envelope protocol and transport framing.
- **Session subsystem**
  - `src/session/pty.rs` - PTY ownership (PtyHandle), escape filtering, terminal query handling.
  - `src/session/cursor_tracker.rs` - cursor position approximation for CPR responses.
  - `src/session/mode_tracker.rs` - byte-level DEC private mode state machine.
  - `src/session/runtime.rs` - SessionRuntime: ring + broadcast + pty + meta + spawn.
  - `src/session/store.rs` - in-memory registry, attach/detach, input, resize, stop.
  - `src/session/ring.rs` - fixed-capacity byte ring buffer.
  - `src/session/persist.rs` - append logs/events on disk.
  - `src/session/mod.rs` - shared types.
- **Persistence + storage**
  - `src/db.rs` - SQL access layer (sessions, push subscriptions, API keys).
  - `src/storage.rs` - state dir paths, sockets, pid/lock paths.
  - `migrations/*.sql` - DB schema evolution.
- **HTTP API**
  - `src/http/mod.rs` - Axum router composition and startup.
  - `src/http/sessions.rs` - session REST endpoints + logs endpoint.
  - `src/http/ws.rs` - interactive session websocket attach.
  - `src/http/sse.rs` - event stream endpoint.
  - `src/http/auth.rs` - auth/login/status/API key middleware endpoints.
  - `src/http/nodes.rs` - node/federation HTTP surfaces.
- **Federation (multi-node)**
  - `src/node/registry.rs` - connected node tracking, request proxying, and streaming fan-out.
  - `src/daemon/rpc_nodes.rs` - node proxy RPC handlers and outbound join connector runtime.
  - `src/client/join.rs` - join config persistence + CLI join commands.
- **Notifications**
  - `src/notification/*` - dispatcher and delivery channels.

### Web frontend (`web/`)

- React + Vite app.
- Uses:
  - REST for CRUD/control,
  - SSE for state/event updates,
  - WebSocket for interactive terminal attach.
- Important files:
  - `web/src/App.tsx`
  - `web/src/api/client.ts`
  - `web/src/pages/SessionDetailPage.tsx`
  - `web/src/components/XTerm.tsx`
  - `web/vite.config.ts` (dev proxy to backend)

### Tests

- Rust integration/e2e:
  - `tests/cli_errors.rs`
  - `tests/e2e_daemon.rs`
  - `tests/e2e_pty.rs`
- Web unit/e2e:
  - `web/src/utils/keyInput.test.ts`
  - `web/e2e/sessions.spec.ts`

## 4) Core Runtime Flows

### A. Start session
1. `oly start ...` sends start RPC to daemon.
2. Daemon calls `SessionStore::start_session`.
3. Runtime spawns PTY child and begins output capture.
4. Metadata/logs are persisted (DB row + output/event logs).

### B. Attach + interact
1. Client requests snapshot + polling stream.
2. CLI/web receives buffered + incremental output.
3. Input/resize events are translated and written to PTY.
4. Session status transitions to completed/stopped when child exits.

### C. List + logs
1. Session summaries come from DB.
2. Logs are read from persisted files (or proxied for remote nodes).
3. Completed sessions can be evicted from memory while history remains queryable.

### D. Federation
1. Secondary node joins primary with API key auth.
2. Primary tracks connected nodes and can proxy operations to remote sessions.

## 5) Data and State Model

- **In-memory:** active session runtime handles, ring buffers, polling cursors.
- **Durable:**
  - SQLite tables:
    - `sessions`
    - `push_subscriptions`
    - `api_keys`
  - Per-session files under state directory:
    - `output.log`
    - `events.log`
    - other artifacts as needed

## 6) PTY Integration Overview

[`ARCHITECTURE_PTY.md`](./ARCHITECTURE_PTY.md) is the source of truth for PTY
lifecycle, terminal mode tracking, escape-sequence handling, resize ordering,
and cross-platform terminal behavior.

At the system level, the important boundaries are:

- The daemon owns PTY allocation and the child process lifecycle through the
  session runtime.
- `SessionRuntime` retains filtered PTY output in the ring buffer, persists it
  to disk, and broadcasts live chunks to attach clients.
- `SessionStore` mediates input, resize, attach/detach, and stop operations for
  the rest of the system.
- IPC, WebSocket, and node-proxied clients all consume the same PTY-backed
  session model even though their transport details differ.

---

## 7) IPC Protocol Reference

Transport: Unix domain sockets (Linux/macOS) or Windows named pipes.  Each message is a newline-delimited JSON envelope:

```json
{ "id": "<request-id>", "payload": { ... } }
```

Responses on the same socket use the same `id`.  Long-running operations (attach, log-tail) respond with a stream of frames terminated by a final `Done` or `Error` frame.

### Request Payload Types (client → daemon)

| Type | Key fields | Description |
|---|---|---|
| `StartSession` | `command`, `name`, `env`, `cols`, `rows` | Spawn new PTY session |
| `StopSession` | `session_id` | Send SIGTERM / ConPTY close |
| `ListSessions` | — | Return all session summaries |
| `AttachStream` | `session_id`, `cols`, `rows` | Begin streaming attach |
| `AttachPoll` | `session_id`, `cursor`, `cols`, `rows` | Single-shot polling attach |
| `AttachInput` | `session_id`, `data` (base64) | Write bytes to PTY |
| `AttachResize` | `session_id`, `cols`, `rows` | Resize PTY window |
| `GetLogs` | `session_id`, `limit` | Fetch buffered output |
| `ListApiKeys` | — | List all API keys |
| `CreateApiKey` | `label` | Create new API key |
| `DeleteApiKey` | `id` | Delete API key |
| `ListNodes` | — | List connected federation nodes |
| `Subscribe` | `events` | Subscribe to event notifications |

### Response / Stream Frame Types (daemon → client)

| Type | Key fields | Description |
|---|---|---|
| `SessionStarted` | `session_id` | Confirmation of start |
| `SessionList` | `sessions[]` | Array of session summaries |
| `AttachStreamInit` | `initial_data` (base64), `app_cursor_keys`, `bracketed_paste_mode` | First frame of streaming attach |
| `AttachData` | `data` (base64) | Incremental PTY output chunk |
| `AttachModeChanged` | `app_cursor_keys`, `bracketed_paste_mode` | Live terminal mode update |
| `AttachPollResult` | `data` (base64), `cursor` | Polling attach result |
| `Logs` | `data` (base64) | Log snapshot |
| `ApiKeyList` / `ApiKeyCreated` / `ApiKeyDeleted` | — | Key management confirmations |
| `NodeList` | `nodes[]` | Federation node list |
| `Event` | `event_type`, `payload` | Async notification event |
| `Done` | — | Stream end |
| `Error` | `message` | Error response |

### Streaming Attach Sequence

```
Client                            Daemon
  |                                 |
  |-- AttachStream(id, cols, rows) ->|
  |                                 |  lock session
  |                                 |  read ring buffer
  |<- AttachStreamInit(data, modes) -|  (initial_data = ring replay)
  |                                 |
  |  [child emits output]           |
  |<-- AttachData(chunk) -----------|
  |<-- AttachData(chunk) -----------|
  |                                 |
  |  [child toggles DECCKM or BP]   |
  |<-- AttachModeChanged(modes) ----|
  |                                 |
  |-- AttachInput(bytes) ---------->|
  |-- AttachResize(cols, rows) ---->|
  |                                 |
  |  [child exits]                  |
  |<-- Done ------------------------|
```

### Polling Attach Sequence

```
Client                            Daemon
  |                                 |
  |-- AttachPoll(id, cursor=0) ---->|
  |<- AttachPollResult(data,cursor)-|
  |                                 |
  |-- AttachPoll(id, cursor=N) ---->|
  |<- AttachPollResult(data,cursor)-|
  |  (repeat at interval)           |
```

---

## 8) WebSocket Attach Protocol

Endpoint: `GET /api/sessions/:id/ws`

Authentication: Bearer token in `Authorization` header or `?token=` query param.

### Unified Streaming Architecture

The WebSocket attach handler uses the **same broadcast-based streaming** as IPC attach (§7).  Both protocols subscribe to `SessionRuntime::broadcast_tx` and receive PTY output chunks in real-time — there is no polling.

For **node-proxied sessions** (sessions on a remote daemon reached via federation), the WS handler falls back to polling via `proxy_rpc()` since the local daemon cannot subscribe to a remote broadcast channel.

### Message Format

All WebSocket messages are JSON text frames.  Binary PTY data is base64-encoded.

#### Client → Server

| `type` field | Additional fields | Description |
|---|---|---|
| `input` | `data` (base64) | Write bytes to PTY |
| `resize` | `cols`, `rows` | Resize PTY |
| `detach` | — | Gracefully detach |
| `ping` | — | Keep-alive |

#### Server → Client

| `type` field | Additional fields | Description |
|---|---|---|
| `init` | `data` (base64), `appCursorKeys`, `bracketedPasteMode` | Ring replay + initial mode state |
| `data` | `data` (base64) | Incremental PTY output (CPR/OSC filtered) |
| `mode_changed` | `appCursorKeys`, `bracketedPasteMode` | Live terminal mode update |
| `session_ended` | `exit_code` | Child process exited |
| `error` | `message` | Error condition |
| `pong` | — | Keep-alive reply |

### Streaming Attach Sequence (Local Session)

```
Browser                           Daemon
  |                                 |
  |--- WS Upgrade (/sessions/:id/ws?cols=N&rows=M) -->
  |                                 |
  |                                 |  attach_subscribe_init():
  |                                 |    lock session
  |                                 |    read ring buffer
  |                                 |    subscribe to broadcast_tx
  |                                 |    read ModeSnapshot
  |<-- { type: "init", data, modes } |  ← base64 ring replay + modes
  |  xterm.js writes replay          |
  |                                 |
  |  [broadcast_rx receives chunks] |
  |<-- { type: "data", data } -------|  ← live output (already source-filtered)
  |<-- { type: "data", data } -------|
  |                                 |
  |  [ModeTracker detects change]    |
  |<-- { type: "mode_changed" } -----|  ← when child toggles DECCKM/BP
  |                                 |
  |--- { type: "input", data } ---->|  ← DECCKM transform applied
  |--- { type: "resize", cols, rows } ->  ← PtyHandle::resize()
  |                                 |
  |  [child exits]                   |
  |<-- { type: "session_ended" } ----|
  |  WS closes                       |
```

### Edge Cases

**Broadcast lag recovery**: If the broadcast channel overflows (subscriber too slow), the WS handler drops the lagged receiver, re-reads a fresh ring snapshot, sends a new `init` frame (resync), and resubscribes.  The client sees a seamless replay.

**Resize ordering**: The client MUST NOT send `resize` before it receives `init`.  Sending resize first causes the child to emit a full-screen repaint (`\x1b[2J` + cursor home) that races with and blanks the ring-buffer replay.

**CPR/OSC filtering**: `EscapeFilter` now runs once in the PTY reader before bytes enter the ring buffer, broadcast stream, or persisted log. Attach handlers forward the already-filtered canonical stream directly.

---

## 9) Feature Specification

### F1 — Detached Session Start

**Spec**: `oly start <command>` spawns the command in a new PTY session managed by the daemon.  The CLI exits immediately; the session continues running in the background.

**Requirements**:
- Session must survive CLI process exit and terminal disconnect.
- Session must be addressable by name or UUID.
- `oly ls` must show the session as running.

### F2 — Reattach to Running Session

**Spec**: `oly attach <name-or-id>` connects the current terminal to a running session.

**Requirements**:
- The user's terminal is placed in raw mode.
- The ring-buffer snapshot is replayed so the user sees recent output.
- After replay, live output streams in real time.
- Terminal size (rows × cols) is communicated to the child.
- On detach (Ctrl-D or configured escape sequence), the terminal is fully restored.

### F3 — Simultaneous Multi-Client Attach

**Spec**: Multiple `oly attach` processes (or web/WS clients) can be attached to the same session concurrently.

**Requirements**:
- All clients receive every PTY output chunk via broadcast (`tokio::sync::broadcast`).
- Input from any client is multiplexed to the same PTY master.
- Mode-change notifications are delivered to all attached clients.
- A client detaching must not affect other clients or the child session.

### F4 — Multi-Machine Reattach (Federation)

**Spec**: A user on Machine B can attach to a session running on Machine A via a secondary node.

**Requirements**:
- Machine B runs `oly` configured as a secondary node, joined to Machine A's primary.
- Authentication uses an API key.
- `oly ls` on Machine B shows sessions from Machine A.
- `oly attach` on Machine B proxies input/output through the node WebSocket connection.
- Latency is added by the WS hop but correctness (mode tracking, resize, bracketed paste) is preserved.

### F5 — Session Persistence

**Spec**: Session metadata and output survive daemon restart.

**Requirements**:
- SQLite `sessions` table stores: `id`, `name`, `command`, `status`, `started_at`, `ended_at`, `exit_code`.
- Output is appended to `output.log` per session.  Log is replayed on `oly logs <id>`.
- On daemon restart, completed sessions are queryable; running sessions are detected as orphaned (exit code recorded as unknown or re-adopted if PID is still alive).

### F6 — Web Terminal

**Spec**: The web UI (`http://localhost:PORT`) provides a browser-based terminal attached to any session.

**Requirements**:
- xterm.js renders PTY output with full VT100/xterm support.
- DECCKM and Bracketed Paste are tracked client-side via `mode_changed` WS frames.
- Resize events are sent on browser window resize.
- Auth is enforced (Bearer token or cookie-based login).

### F7 — Push Notifications

**Spec**: The daemon sends push notifications when sessions exit or emit configured patterns.

**Requirements**:
- Web Push (VAPID) and desktop notifications are supported.
- Subscriptions are stored in `push_subscriptions` table.
- Dispatcher reads `events.log` and matches against notification rules.

### F8 — Session Stop and Cleanup

**Spec**: `oly stop <id>` gracefully terminates a session.

**Requirements**:
- SIGTERM sent to child process group (POSIX) or ConPTY closed (Windows).
- If child does not exit within timeout (default 5 s), SIGKILL is sent.
- Session status updated to `stopped` in SQLite.
- Attached clients receive `Done` / `session_ended` and detach cleanly.

### F9 — Terminal Query Interception

**Spec**: The daemon answers terminal capability queries emitted by the child before they can block.

**Requirements**:
- OSC 10 (foreground color query) answered.
- OSC 11 (background color query) answered.
- CPR / DA1 / DA2 / XTVERSION queries answered with sensible defaults.
- ConPTY bare CPR echoes stripped.
- Generic OSC sequences stripped from the broadcast stream.

### F10 — Cross-Platform Support

**Spec**: `oly` runs on Linux, macOS, and Windows.

**Requirements**:
- PTY: `openpty` on Linux/macOS, ConPTY on Windows (via `portable_pty`).
- Raw mode: `crossterm` on all platforms.
- IPC: Unix domain sockets on Linux/macOS, named pipes on Windows.
- Config/state directory:
  - Linux: `~/.local/share/oly/`
  - macOS: `~/Library/Application Support/oly/`
  - Windows: `%APPDATA%\oly\`

### F11 — Auth and API Keys

**Spec**: All HTTP API and WebSocket endpoints require authentication.

**Requirements**:
- Initial setup creates a random admin API key.
- Additional keys can be created/revoked via `oly key create/delete`.
- Keys are stored hashed in SQLite.
- Federation secondary nodes authenticate with API keys from the primary.

### F12 — Configuration

**Spec**: Daemon behavior is configurable via a TOML file.

**Requirements**:
- Config file location: `<state_dir>/config.toml`.
- CLI flags override config file values.
- See §11 for full option table.

---

## 10) Build, Run, and Test Surfaces

- Rust:
  - `cargo build`
  - `cargo test`
- Web (`web/`):
  - `npm run dev`
  - `npm run build`
  - `npm run test`
  - `npm run e2e`

`build.rs` triggers frontend production build for release profile.

---

## 11) Configuration Reference

Config file: `<state_dir>/config.toml` (created on first run with defaults).

| Key | Type | Default | Description |
|---|---|---|---|
| `port` | `u16` | `7703` | HTTP API listen port |
| `bind` | `string` | `"127.0.0.1"` | HTTP bind address |
| `ring_capacity_bytes` | `usize` | `524288` | Per-session ring buffer size (bytes) |
| `log_max_bytes` | `usize` | `10485760` | Max `output.log` size before rotation |
| `auth_enabled` | `bool` | `true` | Require API key for all HTTP requests |
| `tls_cert` | `path` | `""` | Path to TLS certificate (PEM) |
| `tls_key` | `path` | `""` | Path to TLS private key (PEM) |
| `federation.primary_url` | `string` | `""` | Primary node URL (secondary nodes only) |
| `federation.api_key` | `string` | `""` | API key for primary node auth |
| `notification.vapid_public` | `string` | `""` | VAPID public key for Web Push |
| `notification.vapid_private` | `string` | `""` | VAPID private key for Web Push |

CLI flags (`oly --help`) override all config file values.

---


