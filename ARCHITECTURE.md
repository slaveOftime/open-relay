# Open Relay Architecture (Source of Truth)

This document is the repository-wide architecture reference for `open-relay` (`oly`) so future agents can orient quickly without re-exploring the full codebase.

## 1) System Overview

Open Relay is a Rust-based daemon + CLI for running and managing long-lived interactive PTY sessions, with optional web UI and node federation.

- **CLI (`oly`)** starts, lists, attaches to, controls, and stops sessions.
- **Daemon** owns session runtimes, persistence, IPC/HTTP APIs, auth, and notifications.
- **Web app** consumes daemon APIs (REST + SSE + WebSocket).
- **SQLite** stores session metadata, auth artifacts, and push subscriptions.

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
          |                             |  - notifications           |                   |                    |
          |                             +------+---------------------+                   +--------------------+
          |                                    |
          |                                    | manages
          |                                    v
          |                           +--------+---------------------+
          |                           | Session Store + Runtime      |
          |                           | - PTY child process          |
          |                           | - ring buffer / polling      |
          |                           | - resize/input/stop          |
          |                           +--------+---------------------+
          |                                    |
          |                                    | persists metadata/logs
          |                                    v
          |                           +--------+---------------------+
          |                           | SQLite + log files           |
          |                           | - sessions/api_keys/push     |
          |                           | - output.log/events.log      |
          |                           +------------------------------+
          |
          | optional federation proxy/join
          v
+--------------------+
| Other Oly Node     |
| (primary/secondary)|
+--------------------+
```

## 3) Repository Structure and Responsibilities

### Rust backend (core)

- **CLI + command dispatch**
  - `src/main.rs` - command routing, daemon spawn/check, CLI behavior.
  - `src/cli.rs` - clap command model and options.
- **Daemon lifecycle + IPC handling**
  - `src/daemon/lifecycle.rs` - daemon startup/shutdown, lock/pid handling, foreground runtime orchestration.
  - `src/daemon/rpc.rs` - IPC request entrypoint and top-level dispatch.
  - `src/daemon/rpc_handlers.rs` - request handlers grouped by domain (sessions/logs/federation/api keys/joins).
  - `src/daemon/notifications.rs` - notification monitor and notifier channel wiring.
  - `src/daemon/auth.rs` - password prompt and no-auth safety confirmation helpers.
  - `src/ipc.rs` - envelope protocol and transport framing.
- **Session subsystem**
  - `src/session/runtime.rs` - PTY process lifecycle and output ingestion.
  - `src/session/store.rs` - in-memory registry, attach polling, input, resize, stop.
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
  - `src/node/registry.rs` - node tracking and state.
  - `src/client/join.rs` - join/connect loop for secondary nodes.
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

## 6) Build, Run, and Test Surfaces

- Rust:
  - `cargo build`
  - `cargo test`
- Web (`web/`):
  - `npm run dev`
  - `npm run build`
  - `npm run test`
  - `npm run e2e`

`build.rs` triggers frontend production build for release profile.

## 7) Agent Usage Notes

When an agent needs context, start from this file, then only open targeted files listed above for the exact flow being changed.

Practical lookup map:
- Session lifecycle bug -> `src/session/runtime.rs`, `src/session/store.rs`, `src/daemon/lifecycle.rs`, `src/daemon/rpc_handlers.rs`
- API behavior bug -> `src/http/*`, `src/db.rs`
- CLI behavior bug -> `src/main.rs`, `src/client/*`
- Web terminal/UI bug -> `web/src/pages/SessionDetailPage.tsx`, `web/src/components/XTerm.tsx`, `web/src/api/client.ts`
- Federation issues -> `src/node/*`, `src/client/join.rs`, `src/http/nodes.rs`

