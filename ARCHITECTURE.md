# Open Relay Architecture (Source of Truth)

This document is the authoritative, repository-wide architecture reference for `open-relay` (`oly`).  It covers system design, runtime internals, IPC/WebSocket protocols, terminal-mode tracking, escape-sequence edge cases, cross-platform considerations, and the full feature specification.  Future agents and contributors should start here before opening any source file.

---

## Table of Contents

1. [System Overview](#1-system-overview)
2. [High-Level Component Diagram](#2-high-level-component-diagram-ascii)
3. [Repository Structure and Responsibilities](#3-repository-structure-and-responsibilities)
4. [Core Runtime Flows](#4-core-runtime-flows)
5. [Data and State Model](#5-data-and-state-model)
6. [PTY Layer Internals](#6-pty-layer-internals)
7. [IPC Protocol Reference](#7-ipc-protocol-reference)
8. [WebSocket Attach Protocol](#8-websocket-attach-protocol)
9. [Terminal Mode Tracking](#9-terminal-mode-tracking)
10. [Escape Sequence Edge Cases](#10-escape-sequence-edge-cases)
11. [Feature Specification](#11-feature-specification)
12. [Build, Run, and Test Surfaces](#12-build-run-and-test-surfaces)
13. [Configuration Reference](#13-configuration-reference)
14. [Known Limitations and Cross-Platform Notes](#14-known-limitations-and-cross-platform-notes)
15. [Agent Usage Notes](#15-agent-usage-notes)

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

## 6) PTY Layer Internals

> **Detailed PTY architecture is in [`ARCHITECTURE_PTY.md`](ARCHITECTURE_PTY.md).**
> This section provides a summary; see the standalone doc for cross-platform
> edge cases, signal handling, escape-sequence pipeline, and streaming protocol details.

### Allocation

| Platform | Mechanism | Library |
|---|---|---|
| Linux / macOS | `openpty(3)` — POSIX pseudoterminal pair | `portable_pty` |
| Windows | ConPTY (`CreatePseudoConsole`) | `portable_pty` |

`portable_pty` abstracts both backends behind a common `PtyPair` / `CommandBuilder` API.  The master side is held by the daemon; the slave side is handed to the child process.

### PtyHandle (`src/session/pty.rs`)

`PtyHandle` is the ownership boundary for PTY resources.  It encapsulates:

| Field | Type | Purpose |
|---|---|---|
| `child` | `Box<dyn Child>` | Child process handle (wait, kill, pid) |
| `writer_tx` | `mpsc::Sender<Vec<u8>>` | Channel to the writer thread |
| `pty_master` | `Box<dyn MasterPty>` | Master-side fd for resize |

Methods:
- `write_input(data)` — sends bytes to the writer thread (non-blocking channel send)
- `resize(rows, cols)` — calls `pty_master.resize()`, delivers `SIGWINCH` / ConPTY resize
- `kill()` — terminates the child process
- `try_wait()` — non-blocking check for child exit status
- `process_id()` — returns the child PID if available

The reader and writer threads are spawned in `spawn_session()` but are **not** owned by `PtyHandle` — they run independently and terminate when the master fd closes or the writer channel drops.

### ModeTracker (`src/session/mode_tracker.rs`)

A byte-level state machine that tracks DEC private mode sequences:

```
State transitions:
  Normal → Esc (on 0x1b)
  Esc    → Csi (on '[')     | Normal (anything else)
  Csi    → CsiPrivate ('?') | Normal (anything else)
  CsiPrivate → CsiParam (on digit)  | Normal (non-digit, non-';')
  CsiParam   → process 'h'/'l' → Normal | collect digits/';'
```

Tracked modes:

| Mode | DEC ID | Set sequence | Reset sequence |
|---|---|---|---|
| Application cursor keys (DECCKM) | 1 | `\x1b[?1h` | `\x1b[?1l` |
| Bracketed paste mode | 2004 | `\x1b[?2004h` | `\x1b[?2004l` |

`ModeTracker::process(bytes)` scans a byte slice and returns `Option<ModeSnapshot>` when any tracked mode changes.  It correctly handles sequences split across chunk boundaries — the parser state persists between calls.

`ModeSnapshot` is a plain struct (`app_cursor_keys: bool`, `bracketed_paste_mode: bool`) used by attach handlers to:
1. Include initial mode state in the attach init frame
2. Transform arrow keys when DECCKM is active (see §7 Streaming Attach)
3. Notify attached clients of mode changes mid-stream

### PTY Reader Thread (`src/session/runtime.rs`)

A dedicated `std::thread` (not a Tokio task) owns blocking reads from the master side:

```
loop {
    read(master_fd, buf[4096])
    → extract_query_responses_no_client()   // strip/answer OSC/CPR queries
    → EscapeFilter::filter(bytes)           // derive canonical filtered stream
    → push_output(raw, filtered)            // mode tracking + ring + persist
    → broadcast_tx.send(filtered)           // fan-out to all attached clients
}
```

- Blocking I/O on purpose — avoids Tokio thread starvation for high-bandwidth PTY output.
- `extract_query_responses_no_client` answers ANSI color-query escape sequences so the child application does not block waiting for a response that only a real terminal would provide.  It also strips ConPTY/terminal-echo artifacts.
- `push_output` delegates to `ModeTracker::process()` using the raw PTY bytes, but only the filtered bytes are appended to the ring buffer, persisted to disk, and broadcast to attached clients.

### PTY Writer Thread (`src/session/runtime.rs`)

A second `std::thread` drains an `mpsc` channel and writes to the master fd:

```
loop {
    recv(input_rx) → write(master_fd, bytes)
}
```

Input bytes come from IPC `AttachInput` or HTTP `POST /sessions/:id/input`.  The `SessionStore::attach_input()` method transparently transforms arrow key escape sequences when DECCKM is active (`\x1b[A` → `\x1bOA`).

### Ring Buffer (`src/session/ring.rs`)

- Fixed-capacity byte ring (default 512 KB).
- New clients receive a replay of the ring contents as the first chunk of their attach stream.
- Written by `push_output`; read by `SessionStore::attach_subscribe_init()` for both IPC and WebSocket attach.

### Resize

`SessionStore::resize(id, rows, cols)` calls `PtyHandle::resize()` on the master side.  The child receives `SIGWINCH` (POSIX) or a ConPTY resize event (Windows).  Resize must be sent **after** the initial ring-buffer replay has been delivered to a new client, or the child may emit a full-screen redraw that blanks the replayed output.

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

## 9) Terminal Mode Tracking

The daemon tracks child-side terminal modes via `ModeTracker` (`src/session/mode_tracker.rs`), a byte-level state machine that scans raw PTY output.  The client must mirror these modes to translate user input correctly.

| Mode | Enable sequence | Disable sequence | Child default | Effect on client input |
|---|---|---|---|---|
| **DECCKM** (Application Cursor Keys) | `\x1b[?1h` | `\x1b[?1l` | disabled | Arrow keys: disabled=`\x1b[A/B/C/D`, enabled=`\x1bOA/B/C/D` |
| **Bracketed Paste Mode** | `\x1b[?2004h` | `\x1b[?2004l` | disabled | Paste: disabled=raw bytes, enabled=`\x1b[200~<data>\x1b[201~` |

### ModeTracker Design

`ModeTracker` uses a 5-state parser (Normal → Esc → Csi → CsiPrivate → CsiParam) that processes bytes one at a time.  It handles:

- **Cross-chunk boundaries**: The parser state persists between `process()` calls, so an escape sequence split across two PTY read chunks is still correctly parsed.
- **Unknown modes**: Only modes 1 (DECCKM) and 2004 (bracketed paste) are tracked; other DEC private modes are silently ignored.
- **Multiple modes per chunk**: A single byte slice may contain multiple mode-changing sequences; all are processed.

`ModeSnapshot` captures the current state of all tracked modes.  It is updated by the PTY reader's `push_output()` path and is also available via `SessionRuntime::mode_snapshot()` for on-demand queries (e.g., at attach init time).

### DECCKM Transformation (Server-Side)

`SessionStore::attach_input()` performs DECCKM arrow key transformation transparently on behalf of the client:

- When `app_cursor_keys == true`: `\x1b[A/B/C/D` → `\x1bOA/B/C/D`
- Spurious focus-loss sequences (`\x1b[O`) are filtered out

This means IPC and WebSocket clients can always send normal-mode arrow keys — the daemon translates them if needed.

### Client-Side Responsibilities

The attach client (`src/client/attach.rs`) tracks mirrored copies:
- `child_app_cursor_keys` — updated from `AttachStreamInit` and `AttachModeChanged`
- `child_bracketed_paste` — updated from `AttachStreamInit` and `AttachModeChanged`

On receipt of `crossterm::event::Event::Key`:
- Translate arrow keys using the current `child_app_cursor_keys` value.

On receipt of `crossterm::event::Event::Paste`:
- `crossterm` strips the `\x1b[200~`/`\x1b[201~` markers.
- If `child_bracketed_paste == true`, re-wrap the paste data in those markers before sending.

The web client (`web/src/utils/keyInput.ts`) mirrors the same logic in TypeScript.

---

## 10) Escape Sequence Edge Cases

This section documents every non-obvious terminal escape sequence problem discovered during development.

---

### EC-1: OSC Color Query Blocking (opencode / AI agents)

**Problem**: Some AI coding tools (e.g., opencode) probe the terminal's foreground/background colors at startup using OSC 10/11 queries:
```
ESC ] 10 ; ? BEL      # query foreground color
ESC ] 11 ; ? BEL      # query background color
```
A real terminal would respond with a color spec.  The daemon is not a terminal, so there is no response.  The child application blocks indefinitely waiting for the reply.

**Root cause**: The PTY output path was transparent — it forwarded bytes to the ring buffer but never synthesized terminal responses.

**Fix**: `extract_query_responses_no_client()` in `src/session/pty.rs` intercepts these queries and writes a synthetic response to the master fd before the bytes reach the filtered ring buffer:
- Reads `$COLORFGBG` environment variable if set (common in color-aware terminals).
- Falls back to white foreground (`rgb:ffff/ffff/ffff`) and black background (`rgb:0000/0000/0000`).

**Source**: `src/session/pty.rs` — `TerminalQuery::ForegroundColor`, `TerminalQuery::BackgroundColor`.

---

### EC-2: Duplicate `\x1b[?1049h` Blanks Alternate Screen

**Problem**: Both the attach client (via `crossterm::execute!(stdout, EnterAlternateScreen)`) AND the child process may emit `\x1b[?1049h`.  The second invocation resets the alternate screen buffer, wiping any output already drawn.

**Root cause**: The client unconditionally entered the alternate screen on attach, assuming it must set up terminal state.  But the child already manages its own screen.

**Fix**: The attach client must **not** call `EnterAlternateScreen`.  The child's own `\x1b[?1049h` establishes the alternate screen.  The client's `RawModeGuard` teardown sends `\x1b[?1049l` on detach to cleanly exit regardless.

**Affected file**: `src/client/attach.rs` — `run_attach_inner`, `run_attach_polled`.

---

### EC-3: Resize Before Initial Data Race (Blank Screen on Reattach)

**Problem**: A newly attaching client sends `AttachResize` to inform the daemon of its terminal size.  If this resize arrives before the ring-buffer replay is delivered, the child emits `\x1b[2J\x1b[H` (clear screen, cursor home) followed by a full repaint.  Those bytes arrive as `AttachData` frames **ahead of** or **mixed with** the ring replay, resulting in a blank or corrupted initial display.

**Root cause**: Resize and initial data were not sequenced — the client optimistically sent resize as soon as the IPC connection opened.

**Fix**: The `AttachStreamInit` frame carries `initial_data` (the full ring replay).  The client writes this replay to the terminal **before** sending `AttachResize`.  The daemon applies the resize only when it processes the `AttachResize` message, guaranteeing the replay is already consumed client-side.

**Affected files**: `src/client/attach.rs` (send resize after parsing init), `src/daemon/rpc.rs` (emit `AttachStreamInit` with ring data).

---

### EC-4: DECCKM Not Tracked Live (Wrong Arrow Keys in AI Tools)

**Problem**: An attached AI tool enables Application Cursor Keys (`\x1b[?1h`) after the initial attach handshake.  The daemon captures the initial mode at attach time via `AttachStreamInit`, but never emits `AttachModeChanged` while the child is running.  The client's `child_app_cursor_keys` stays `false`.  When the user presses an arrow key, the client sends `\x1b[A` (normal mode) but the child expects `\x1bOA` (application mode) — movement is silently ignored or misinterpreted.

**Root cause**: The mode-change detection in the PTY reader updates `RuntimeState` correctly, but no IPC signal was wired up to propagate the change to attached clients.

**Fix**: After `push_output()` updates the mode snapshot, the streaming paths compare the current `ModeSnapshot` after each forwarded chunk and emit an `AttachModeChanged` frame when needed.

**Affected file**: `src/daemon/rpc.rs` — live broadcast loop.

---

### EC-5: Bracketed Paste Re-Wrapping

**Problem**: `crossterm` parses `Event::Paste(text)` by stripping the `\x1b[200~` / `\x1b[201~` markers.  If the child has enabled Bracketed Paste Mode, it expects those markers — without them, the pasted text may be executed as commands rather than inserted as literal text.

**Root cause**: `crossterm` strips markers at the event layer.  The attach client re-emits the text without re-adding them.

**Fix**: When `child_bracketed_paste == true`, the client prepends `\x1b[200~` and appends `\x1b[201~` before writing the paste bytes to the IPC `AttachInput` frame.

**Affected file**: `src/client/attach.rs` — input handler.

---

### EC-6: ConPTY Bare CPR Echo

**Problem**: On Windows, ConPTY echoes cursor-position-report responses (`[35;1R`) without the leading ESC byte back into the master-side output.  These bare sequences pollute the output stream visible to clients.

**Root cause**: ConPTY implementation quirk — it echoes the response to the master before the child consumes it.

**Fix**: `EscapeFilter` in `src/session/pty.rs` strips these bare CPR sequences in the PTY reader before they reach the canonical retained stream.

**Source**: `src/session/pty.rs` — `EscapeFilter`.

---

### EC-7: OSC 7 / Generic OSC Echo

**Problem**: Shells and terminal apps send OSC 7 (`\x1b]7;file://hostname/path\x07`) to notify the terminal of the current working directory.  Some terminals echo this back through the PTY master, creating feedback loops.  Other OSC sequences may similarly be echoed.

**Root cause**: Echo pass-through from terminal emulator to PTY master.

**Fix**: `EscapeFilter` strips generic OSC sequences (BEL-terminated and ST-terminated) in the PTY reader before they are retained or broadcast.

**Source**: `src/session/pty.rs`.

---

### EC-8: Partial Escape Sequences Across `read()` Boundaries

**Problem**: The PTY reader thread reads 4 KB chunks.  An escape sequence (e.g., an OSC string or a multi-byte CPR response) can be split across two consecutive `read()` calls.  Naive per-chunk processing would fail to recognize the split sequence.

**Root cause**: POSIX `read()` on PTY master provides no framing — sequences can span arbitrary chunk boundaries.

**Fix**: Two carry-forward buffers:
- `query_tail: Vec<u8>` in the reader loop — carries the tail of a chunk that might be the start of a query sequence.
- `EscapeFilter.pending: Vec<u8>` in `src/session/pty.rs` — carries incomplete escape sequences across calls to `filter()`.

Both are reset when the sequence is completed or proven not to be an escape sequence.

**Source**: `src/session/pty.rs` — `EscapeFilter`; `src/session/runtime.rs` — `query_tail`.

---

### EC-9: Terminal Left in Bad State After Unexpected Client Exit

**Problem**: If `oly attach` is killed with SIGKILL, or panics, Rust destructors do not run.  The terminal is left in raw mode (echo disabled, canonical mode off).  The user sees no input feedback and may not be able to recover without external intervention.

**Root cause**: Raw mode is a process-level stty setting that survives process exit on POSIX.  `RawModeGuard::drop` sends a normalization string, but `Drop` is not called on SIGKILL.

**Mitigation**:
- `RawModeGuard::drop` sends a comprehensive normalize sequence:  
  `\x1b[?1049l\x1b[!p\x1b[0m\x1b[?25h\x1b[?1000l...\x1b[?2004l`
- This covers all known mode toggles for a clean detach under normal signals (SIGTERM, Ctrl-C, panic unwind).
- For SIGKILL recovery: users must run `stty sane` or `reset` from another terminal or SSH session.

**Cross-platform note**: On Windows, `crossterm` restores console mode via its own `Drop` implementation; ConPTY mode restoration is automatic.

**Source**: `src/client/attach.rs` — `RawModeGuard::teardown_terminal`.

---

## 11) Feature Specification

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
- See §13 for full option table.

---

## 12) Build, Run, and Test Surfaces

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

## 13) Configuration Reference

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

## 14) Known Limitations and Cross-Platform Notes

### Terminal Restoration on SIGKILL (All Platforms)

Rust `Drop` implementations do not run on `SIGKILL`.  Raw mode is not restored.  Users must run `stty sane` (Linux/macOS) or close and reopen the terminal (Windows).

### Windows ConPTY Quirks

- ConPTY echoes bare CPR responses (`[35;1R`) into the master output stream.  These are stripped by `BARE_CPR_RE`.
- ConPTY does not support all xterm extensions (e.g., mouse reporting beyond basic).
- `SIGWINCH` does not exist on Windows; `portable_pty` translates resize events to ConPTY API calls.
- Named pipes (IPC) have different path conventions: `\\.\pipe\oly-<user>`.

### macOS Terminal.app

- Does not set `$COLORFGBG` by default.  The color-query fallback (white-on-black) is used.
- `EnterAlternateScreen` from the client side causes double invocation of `\x1b[?1049h` — the attach client must not call it (see EC-2).

### Escape Sequence Fragmentation

Large bursts of PTY output can send escape sequences across `read()` chunk boundaries.  The carry-forward buffers (`query_tail`, `EscapeFilter.pending`) handle known sequences, but novel sequences from future terminal capabilities may not be handled.

### Multi-Client Input Multiplexing

All attached clients write to the same PTY master.  Concurrent input from multiple clients interleaves at the byte level.  This is correct for automation (only one client sends input) but may be confusing for human multi-attach scenarios.  There is no UI indication of which client is typing.

### Ring Buffer Overflow

If a session emits more than `ring_capacity_bytes` of output before any client attaches, early output is lost.  `output.log` always has the full history.

### Federation Latency

Each input/output hop through a secondary node adds one WebSocket round-trip.  High-throughput sessions (e.g., video in terminal) may lag noticeably over high-latency links.

### Auth Bypass Risk on Loopback-Only Deployments

If `bind = "127.0.0.1"` and `auth_enabled = false`, any local process can control sessions.  This is intentional for trusted developer environments but must not be used on shared machines.

---

## 15) Agent Usage Notes

When an agent needs context, start from this file, then only open targeted files listed below for the exact flow being changed.

### Practical Lookup Map

| Problem area | Files to open |
|---|---|
| Session lifecycle bug | `src/session/runtime.rs`, `src/session/store.rs`, `src/daemon/lifecycle.rs` |
| IPC protocol bug | `src/daemon/rpc.rs`, `src/ipc.rs` |
| CLI attach behavior | `src/client/attach.rs`, `src/client/input.rs` |
| Terminal mode tracking | `src/session/runtime.rs` (`push_output`), `src/client/attach.rs` (mode tracking) |
| Escape sequence handling | `src/session/pty.rs` (`EscapeFilter`, `extract_query_responses_no_client`) |
| API behavior bug | `src/http/*.rs`, `src/db.rs` |
| Web terminal/UI bug | `web/src/pages/SessionDetailPage.tsx`, `web/src/components/XTerm.tsx` |
| Key input translation | `web/src/utils/keyInput.ts`, `src/client/attach.rs` |
| Federation issues | `src/node/*`, `src/client/join.rs`, `src/http/nodes.rs` |
| Push notifications | `src/notification/*` |
| Cross-platform / PTY | `src/session/runtime.rs`, `portable_pty` crate docs |

