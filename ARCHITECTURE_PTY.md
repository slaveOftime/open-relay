# PTY Management Architecture

> Standalone architecture reference for PTY lifecycle, streaming protocols,
> cross-platform edge cases, and escape-sequence handling in Open Relay.
>
> See also: [`ARCHITECTURE.md`](ARCHITECTURE.md) for system-wide architecture.

---

## Table of Contents

1. [Design Philosophy](#1-design-philosophy)
2. [PTY Lifecycle](#2-pty-lifecycle)
3. [Module Layout](#3-module-layout)
4. [PtyHandle — Ownership Boundary](#4-ptyhandle--ownership-boundary)
5. [Reader & Writer Threads](#5-reader--writer-threads)
6. [Ring Buffer & Replay](#6-ring-buffer--replay)
7. [Mode Tracking (ModeTracker)](#7-mode-tracking-modetracker)
8. [Escape Sequence Pipeline](#8-escape-sequence-pipeline)
9. [Streaming Attach Protocol](#9-streaming-attach-protocol)
10. [Multi-Client Attach](#10-multi-client-attach)
11. [Resize Protocol](#11-resize-protocol)
12. [Cross-Platform Edge Cases](#12-cross-platform-edge-cases)
13. [Signal Handling & Process Lifecycle](#13-signal-handling--process-lifecycle)
14. [Error Recovery](#14-error-recovery)
15. [Node-Proxied Streaming](#15-node-proxied-streaming)
16. [Design Constraints & Future Work](#16-design-constraints--future-work)

---

## 1) Design Philosophy

Open Relay manages PTY sessions as a **daemon-side resource** (inspired by
tmux's server model) but differs in a key way: we are a **CLI proxy**, not a
terminal multiplexer.  The client is a regular terminal emulator — we never
render cells, track cursor position, or maintain a screen buffer.

Design principles:

| Principle | Rationale |
|---|---|
| **Daemon owns the PTY** | Sessions survive client disconnect/reconnect |
| **Raw byte streaming** | Preserve all escape sequences; let the client terminal render |
| **Broadcast fan-out** | Multiple clients attach to the same session simultaneously |
| **Byte-level state tracking** | Mode changes (DECCKM, bracketed paste) tracked without parsing full VT |
| **Cross-chunk correctness** | All parsers carry state across read boundaries |
| **Blocking I/O for PTY** | Dedicated OS threads avoid Tokio executor starvation |

### tmux Influence

We adopt several patterns from tmux's server-side session management:

- **Event-driven I/O**: tmux uses libevent for non-blocking PTY I/O; we use
  `std::thread` + `tokio::sync::broadcast` for the same decoupling.
- **Reference counting**: tmux uses `session_add_ref/remove_ref`; we track
  attach presence in `SessionStore` and defer cleanup.
- **Deferred destruction**: tmux marks panes as exited but delays cleanup
  (remain-on-exit); we keep the ring buffer available after child exit.
- **Smallest-client-size rule**: tmux resizes to the smallest attached client;
  we document this as the recommended multi-client resize strategy (§11).
- **Signal discipline**: tmux blocks signals during fork with `sigprocmask`;
  `portable_pty` handles this internally.

We deliberately **do not** adopt:
- Screen buffer / cell grid (we stream raw bytes)
- Window/pane layout management (single session per PTY)
- Client-side terminal rendering (the user's terminal handles it)

---

## 2) PTY Lifecycle

```text
                    spawn_session()
                         │
                    ┌────▼────┐
                    │ Spawned │  PtyHandle created
                    │         │  Reader/writer threads started
                    │         │  Ring buffer empty
                    └────┬────┘
                         │
              ┌──────────▼──────────┐
              │      Running        │  PTY output → ring + broadcast
              │                     │  Input accepted from clients
              │  ┌───────────────┐  │
              │  │  Attach(ed)   │◄─┼── IPC / WebSocket / node proxy
              │  │  Detach       │──┼── Client disconnect
              │  └───────────────┘  │
              └──────────┬──────────┘
                         │  Child exits (SIGCHLD / WaitForSingleObject)
                    ┌────▼────┐
                    │ Exited  │  Exit code captured
                    │         │  Ring buffer preserved (replayable)
                    │         │  No more input accepted
                    └────┬────┘
                         │  Grace period / eviction
                    ┌────▼────┐
                    │ Cleaned │  PTY master fd closed
                    │   Up    │  Threads terminate
                    │         │  Session metadata persisted to SQLite
                    └─────────┘
```

### State Transitions

| From | To | Trigger |
|---|---|---|
| — | Spawned | `SessionStore::start()` → `spawn_session()` |
| Spawned | Running | Immediately after spawn (reader thread starts) |
| Running | Exited | Child process exits (`try_wait()` returns `Some`) |
| Running | Exited | `kill()` called (stop command) |
| Exited | Cleaned Up | Periodic daemon maintenance tick after eviction TTL / daemon shutdown |

---

## 3) Module Layout

```text
src/session/
├── mod.rs              Session types (SessionMeta, SessionStatus, StartSpec)
├── pty.rs              PTY ownership + escape sequence handling (this doc)
│   ├── PtyHandle       Master fd, child, writer channel
│   ├── RuntimeChild    Child process wrapper
│   ├── EscapeFilter    CPR/DSR/OSC response stripper
│   ├── TerminalQuery   Query pattern matching & response generation
│   ├── has_visible_content()
│   └── extract_query_responses_no_client()
├── cursor_tracker.rs   Approximate cursor position tracker for CPR replies
│   └── CursorTracker   CSI/printable-text based cursor model
├── mode_tracker.rs     Byte-level DEC private mode state machine
│   ├── ModeTracker     Parser state machine
│   └── ModeSnapshot    Current mode values
├── runtime.rs          SessionRuntime (ring + broadcast + pty + meta)
│   ├── spawn_session() PTY spawn + thread creation
│   └── push_raw()      Ring write + mode tracking + persistence
├── store.rs            SessionStore (session registry, attach/detach/resize)
├── ring.rs             Fixed-capacity byte ring buffer
└── persist.rs          Disk persistence (append-only log)
```

---

## 4) PtyHandle — Ownership Boundary

`PtyHandle` (`src/session/pty.rs`) is the single ownership struct for PTY
resources.  All PTY interactions go through it.

```rust
pub struct PtyHandle {
    pub(crate) child: RuntimeChild,           // Child process (wait/kill/pid)
    pub(crate) writer_tx: mpsc::Sender<Vec<u8>>,  // To writer thread
    pub(crate) pty_master: Option<Box<dyn MasterPty>>,  // For resize
}
```

### Methods

| Method | Behaviour | Failure mode |
|---|---|---|
| `write_input(data)` | Non-blocking channel send to writer thread | Returns `false` if channel closed |
| `resize(rows, cols)` | `pty_master.resize()` → SIGWINCH / ConPTY resize | Returns `false` if master dropped |
| `kill()` | SIGKILL (POSIX) / TerminateProcess (Windows) | `io::Error` |
| `try_wait()` | Non-blocking `waitpid(WNOHANG)` / `WaitForSingleObject(0)` | `io::Error` |
| `process_id()` | Child PID (if available) | `None` on some platforms |

### Design Decision: Why Not Tokio AsyncFd?

PTY master fds on Linux are pollable with `epoll`, but:
1. ConPTY on Windows is **not** pollable — it requires blocking `ReadFile`.
2. `portable_pty` provides blocking `Read`/`Write` traits, not async.
3. Spawning 2 OS threads per session is acceptable for our scale (dozens, not thousands).

---

## 5) Reader & Writer Threads

### Reader Thread

```text
std::thread::spawn("pty-reader-{id}") {
    loop {
        let n = master_reader.read(&mut buf[..4096]);
        if n == 0 || n == Err(_) → break;

        // 1. Answer terminal queries when no client is attached
        for resp in extract_query_responses_no_client(&buf, &mut tail) {
            writer_tx.send(resp);  // Write response back to PTY stdin
        }

        // 2. Push to ring + mode track + persist + broadcast
        let bytes = Bytes::copy_from_slice(&buf[..n]);
        push_raw(bytes);  // → ring.push() + mode_tracker.process() + broadcast_tx.send()
    }
}
```

**Why a blocking thread?**  High-bandwidth PTY output (e.g., `cat /dev/urandom`)
would starve the Tokio executor if run as an async task.  A dedicated thread
ensures PTY reads never block other tasks.

### Writer Thread

```text
std::thread::spawn("pty-writer-{id}") {
    loop {
        let data = writer_rx.recv();  // blocks until input arrives
        master_writer.write_all(&data);
    }
}
```

Input sources: IPC `AttachInput`, HTTP `POST /sessions/:id/input`, WebSocket
`input` message.  All go through `SessionStore::attach_input()` which applies
DECCKM arrow-key transformation before sending to the writer channel.

---

## 6) Ring Buffer & Replay

The ring buffer (`src/session/ring.rs`) is a fixed-capacity circular byte
buffer (default 1 MiB) that stores the most recent PTY output.

### Replay on Attach

When a client attaches, `attach_subscribe_init()`:
1. Locks the session store
2. Reads the ring buffer contents as `Vec<(offset, Bytes)>` chunks
3. Creates a new `broadcast::Receiver` for live output
4. Returns the replay chunks + receiver + current mode snapshot

The replay is sent as the `AttachStreamInit` response frame, filtered through
`EscapeFilter` to strip device responses.

### Offset Tracking

Each byte in the ring has a monotonically increasing logical offset.  Clients
track their current offset to detect gaps (e.g., after broadcast lag).

---

## 7) Mode Tracking (ModeTracker)

`ModeTracker` (`src/session/mode_tracker.rs`) is a byte-level state machine
that processes raw PTY output and detects DEC private mode changes.

### State Machine

```text
Normal ──[0x1b]──► Esc
Esc    ──['[']──► Csi       ──[other]──► Normal
Csi    ──['?']──► CsiPrivate ──[other]──► Normal
CsiPrivate ──[digit]──► CsiParam
CsiParam   ──[digit/';']──► CsiParam (accumulate)
CsiParam   ──['h']──► process_set() → Normal
CsiParam   ──['l']──► process_reset() → Normal
CsiParam   ──[other]──► Normal
```

### Tracked Modes

| Mode | DEC ID | Set | Reset |
|---|---|---|---|
| Application cursor keys (DECCKM) | 1 | `\x1b[?1h` | `\x1b[?1l` |
| Bracketed paste mode | 2004 | `\x1b[?2004h` | `\x1b[?2004l` |

### Cross-Chunk Correctness

The parser state persists between `process()` calls, so sequences split across
PTY read boundaries are handled correctly:

```text
Chunk 1: "output\x1b"     → state = Esc
Chunk 2: "[?1h"           → detects DECCKM set
```

### ModeSnapshot

```rust
pub struct ModeSnapshot {
    pub app_cursor_keys: bool,
    pub bracketed_paste_mode: bool,
}
```

Used by:
- `AttachStreamInit` to send initial mode state to clients
- `AttachModeChanged` to notify clients of mode transitions
- `attach_input()` to transform arrow keys when DECCKM is active

---

## 8) Escape Sequence Pipeline

PTY output passes through several processing stages before reaching clients:

```text
PTY master fd
    │
    ▼
┌─────────────────────────────┐
│ extract_query_responses()   │  Answers CPR/DSR/OSC queries
│                             │  Writes responses back to PTY stdin
│ (only when no client)       │  Prevents child apps from blocking
└─────────────┬───────────────┘
              │
              ▼
┌─────────────────────────────┐
│ push_raw()                  │  Ring buffer append
│   └─ mode_tracker.process() │  DEC private mode detection
│   └─ persist()              │  Disk write
│   └─ broadcast_tx.send()    │  Fan-out to all subscribers
└─────────────┬───────────────┘
              │
              ▼ (per subscriber)
┌─────────────────────────────┐
│ EscapeFilter                │  Strips CPR/DSR echoes from output
│                             │  Strips OSC 10/11 color responses
│                             │  Strips generic OSC sequences
│                             │  Handles cross-chunk partial sequences
└─────────────┬───────────────┘
              │
              ▼
        Client terminal
```

### EscapeFilter Details

ConPTY on Windows echoes terminal device responses (CPR, DSR, OSC color
queries) back into the master output stream.  `EscapeFilter` strips these
before forwarding to clients.

Patterns stripped:
- **Full CPR**: `\x1b[<row>;<col>R` (with or without `?`)
- **Bare CPR**: `[<row>;<col>R` (ESC dropped by ConPTY)
- **DSR/CPR queries**: `\x1b[6n`, `\x1b[5n` (stripped to prevent client
  terminals from generating their own CPR responses, which would corrupt
  the child process's stdin via the attach input path)
- **Bare DSR queries**: `[5n`, `[6n` (ESC dropped by ConPTY)
- **OSC 10/11 color responses**: `\x1b]10;rgb:xxxx/xxxx/xxxx\x07`
- **Generic OSC**: `\x1b]<num>;<payload>\x07` (e.g., shell CWD updates)

Cross-chunk handling: `pending` field carries incomplete sequences across
`filter()` calls.  This handles ConPTY splitting ESC sequences at arbitrary
byte boundaries.

### Persist Filter

The `SessionRuntime` also runs its own `EscapeFilter` (the "persist filter")
on all PTY output before writing to `output.log`.  This ensures that
`oly logs` reads clean data from disk, free of CPR/DSR artifacts that would
otherwise appear as garbage characters in the log output.

### Query Response Generation

When no client is attached, terminal query sequences in the PTY output must
still be answered.  `extract_query_responses_no_client()` scans output for:

| Query | Sequence | Response |
|---|---|---|
| Cursor Position Report | `\x1b[6n` | `\x1b[1;1R` |
| Device Status Report | `\x1b[5n` | `\x1b[0n` |
| Foreground color | `\x1b]10;?\x07` | `\x1b]10;rgb:xxxx/xxxx/xxxx\x1b\\` |
| Background color | `\x1b]11;?\x07` | `\x1b]11;rgb:xxxx/xxxx/xxxx\x1b\\` |

Color responses use `COLORFGBG` env var if set, otherwise default to
white-on-black.

---

## 9) Streaming Attach Protocol

All attach paths (IPC, WebSocket, node-proxied) use the same streaming
protocol.  This was unified from separate polling/streaming implementations.

### Frame Types

| Frame | Direction | Purpose |
|---|---|---|
| `AttachStreamInit` | Server → Client | Ring buffer replay + initial mode state |
| `AttachStreamChunk` | Server → Client | Incremental PTY output (filtered) |
| `AttachModeChanged` | Server → Client | Terminal mode transition notification |
| `AttachStreamDone` | Server → Client | Session ended (with exit code) |
| `AttachInput` | Client → Server | Keyboard/paste input |
| `AttachResize` | Client → Server | Terminal size change |
| `AttachDetach` | Client → Server | Voluntary disconnect |

### IPC Streaming Flow

```text
CLI                              Daemon
 │                                  │
 │──AttachSubscribe────────────────►│
 │                                  │  subscribe to broadcast + ring replay
 │◄──AttachStreamInit──────────────│  (replay bytes + mode snapshot)
 │                                  │
 │◄──AttachStreamChunk─────────────│  (live PTY output, filtered)
 │◄──AttachStreamChunk─────────────│
 │──AttachInput────────────────────►│  (keyboard input)
 │──AttachResize───────────────────►│  (terminal resize)
 │◄──AttachModeChanged─────────────│  (DECCKM toggled)
 │◄──AttachStreamChunk─────────────│
 │                                  │  child exits
 │◄──AttachStreamDone──────────────│  (exit code)
```

### WebSocket Streaming Flow

WebSocket uses JSON messages with a `type` field:

```json
// Server → Client
{"type": "init", "data": "<base64>", "appCursorKeys": false, "bracketedPasteMode": false}
{"type": "data", "data": "<base64>"}
{"type": "mode_changed", "appCursorKeys": true, "bracketedPasteMode": false}
{"type": "session_ended", "exit_code": 0}

// Client → Server
{"type": "input", "data": "ls\r"}
{"type": "resize", "rows": 24, "cols": 80}
{"type": "detach"}
```

---

## 10) Multi-Client Attach

Multiple clients can attach to the same session simultaneously.  This is
achieved through the `broadcast::channel`:

```text
broadcast_tx ──► broadcast_rx_1 (IPC client 1)
              ├► broadcast_rx_2 (WebSocket client)
              └► broadcast_rx_3 (node-proxied client)
```

### Shared Input

All attached clients write to the same PTY stdin.  Input is **not** isolated —
keystrokes from any client are interleaved.  This is intentional (same as tmux
shared sessions).

### Broadcast Lag Recovery

If a subscriber falls behind (e.g., slow WebSocket), `RecvError::Lagged` is
returned.  The handler re-syncs by:
1. Re-subscribing to the broadcast channel
2. Reading the full ring buffer as a fresh replay
3. Continuing from the new end offset

### Attach Presence Tracking

`SessionStore` tracks whether any client is currently attached.  This is used
for:
- Query response routing (daemon answers queries when no client is attached)
- Notification suppression (no "session ended" push if client is watching)
- Idle detection

---

## 11) Resize Protocol

### Ordering Constraint

Resize must happen **after** the initial ring-buffer replay has been sent to
the client.  Otherwise:

1. Client attaches, receives ring replay (old terminal size)
2. Resize sent immediately → child redraws at new size
3. Client processes replay, then sees the full redraw → flicker/blank

Correct order:
1. `attach_subscribe_init()` → send `AttachStreamInit` with replay
2. Client renders replay
3. Send `AttachResize` → child redraws at correct size

### Multi-Client Resize Strategy

When multiple clients are attached with different terminal sizes, the
recommended strategy is **smallest-client-size wins** (following tmux):

- Track each client's reported size
- Resize PTY to `min(rows)` × `min(cols)` across all attached clients
- Larger clients see blank padding; smaller clients render correctly

Currently, the last resize wins (no size aggregation).  This is documented
as a known limitation.

### Race Condition: Rapid Resize

If a client sends multiple resize events in quick succession (e.g., during
window drag), each triggers a `SIGWINCH`.  The child may emit partial redraws
for intermediate sizes.  Mitigation: clients should debounce resize events
(the web UI does this with a 100ms delay).

---

## 12) Cross-Platform Edge Cases

### ConPTY (Windows)

| Issue | Impact | Mitigation |
|---|---|---|
| Echo of device responses | CPR/DSR/OSC responses appear in master output | `EscapeFilter` strips them (both per-client and persist filter) |
| DSR query forwarding | Queries like `\x1b[6n` forwarded to attach clients cause CPR response feedback loop | `EscapeFilter` strips DSR queries before sending to clients |
| ESC split across reads | Sequences split at arbitrary byte boundaries | `pending` field in `EscapeFilter` carries fragments |
| No SIGWINCH | ConPTY uses `ResizePseudoConsole()` internally | `portable_pty` abstracts this |
| No SIGCHLD | Child exit detected via `WaitForSingleObject` | `try_wait()` polls periodically |
| Process group semantics | No `setsid()` / process groups | ConPTY manages child lifetime |
| Color response format | May differ from POSIX terminal | Static fallback in `terminal_report_colors()` |

### POSIX PTY (Linux / macOS)

| Issue | Impact | Mitigation |
|---|---|---|
| File descriptor leaks | Child inherits daemon's fds | `portable_pty` handles `CLOEXEC`; tmux uses `closefrom(STDERR+1)` |
| Zombie processes | Parent must `waitpid()` after SIGCHLD | `try_wait()` polled in completion check loop |
| Signal during fork | SIGCHLD between fork/exec can confuse child | `portable_pty` blocks signals during fork |
| PTY master close race | Close master while child is writing → SIGPIPE | Reader thread detects `read() == 0` and breaks cleanly |
| macOS PTY buffer size | Smaller than Linux (4KB vs 16KB) | No special handling needed; affects throughput only |
| EMFILE/ENFILE in accept | FD exhaustion prevents new connections | Backoff with exponential delay (tmux pattern) |

### Encoding Assumptions

- PTY output is treated as raw bytes, not decoded as UTF-8
- `String::from_utf8_lossy` used only in filter functions that need regex
- JSON framing uses base64 for binary data, preserving all bytes
- Cross-chunk `pending` buffers in `EscapeFilter` operate on `String` but
  handle replacement characters gracefully

---

## 13) Signal Handling & Process Lifecycle

### Child Exit Detection

```text
┌──────────────────────────────────────────┐
│ Completion check interval (100-200ms)    │
│                                          │
│  pty.try_wait() ──► Some(exit_code)      │
│       │                  │               │
│       │            Store exit code       │
│       │            Set status = Exited   │
│       │            Send AttachStreamDone │
│       ▼                                  │
│   None → continue polling                │
└──────────────────────────────────────────┘
```

Both IPC and WebSocket handlers run a periodic completion check, and the daemon
also runs a periodic maintenance tick to persist/evict completed sessions even
while idle. This is necessary because:
- `broadcast::Receiver::Closed` only fires when the sender is dropped
- The sender is dropped when the reader thread exits
- The reader thread exits when `read()` returns 0 (master fd closed)
- On some platforms, the master fd may not close immediately on child exit

### Graceful Stop

`SessionStore::stop()`:
1. Send SIGTERM (POSIX) or `TerminateProcess` (Windows) via `kill()`
2. Wait up to `stop_grace_seconds` (configurable, default 5s)
3. If still running, send SIGKILL
4. Record exit code
5. Finalize the session as `stopped` for user-requested stops even when the OS
   reports a non-zero signal/termination code; reserve `failed` for unrequested
   non-zero exits or runtime errors

`SessionStore::kill_session()`:
1. Mark the runtime as stopping with a requested final state of `killed`
2. Skip the Ctrl-C grace path and terminate the child immediately
3. Finalize the session as `killed` once the process exit is observed

### Daemon Shutdown

`stop_all_sessions()`:
1. Iterate all running sessions
2. Call `stop()` on each
3. Wait for all to exit
4. Flush persistence

---

## 14) Error Recovery

| Failure | Detection | Recovery |
|---|---|---|
| Child crashes | `try_wait()` returns exit code | Send `AttachStreamDone`, preserve ring |
| PTY read failure | Reader thread `read()` returns error | Thread exits, broadcast closed |
| Client disconnect | IPC/WebSocket `recv()` returns None | `attach_detach()` cleanup |
| Broadcast lag | `RecvError::Lagged(n)` | Re-subscribe + full ring replay |
| Writer channel full | `send()` returns error | Input dropped (logged as warning) |
| IPC connection reset | `read_request` returns error | Client reader task exits |
| Node proxy disconnect | WebSocket closed | Stream receiver gets None |
| Master fd close race | `write()` to closed fd | Writer thread gets `BrokenPipe`, exits |

### Invariants

1. **Ring buffer is always consistent**: Push is atomic (single writer thread)
2. **Mode state is always consistent**: ModeTracker is only called from push_raw
3. **Exit code is captured at most once**: `try_wait` → store → done
4. **Cleanup always runs**: IPC handler has `attach_detach()` in all exit paths

---

## 15) Node-Proxied Streaming

When a session runs on a secondary node, the attach path crosses three
processes:

```text
CLI ──IPC──► Primary Daemon ──WebSocket──► Secondary Daemon
              (proxy)                       (owns the PTY)
```

### Protocol Stack

```text
Layer          Primary Side              Secondary Side
─────────────────────────────────────────────────────────
IPC            handle_node_proxy_streaming()  handle_attach_subscribe()
                    │                              │
Node Registry  proxy_rpc_stream()              (local store)
                    │                              │
Inter-node WS  RpcStreamFrame { id, resp, done }
                    │                              │
Join connector     rpc_nodes.rs relay loop    rpc_nodes.rs relay_streaming_rpc()
```

### Frame Relay

1. **CLI** sends `NodeProxy { node, inner: AttachSubscribe }` via IPC
2. **Primary daemon** detects streaming RPC, calls `proxy_rpc_stream()`
3. **NodeRegistry** sends `NodeWsMessage::RpcRequest` to the secondary
4. **Secondary's join connector** detects `AttachSubscribe`, spawns streaming task
5. Streaming task opens local IPC to secondary daemon, reads frames in loop
6. Each frame is sent as `NodeWsMessage::RpcStreamFrame { id, response, done }`
7. **Primary's relay loop** receives `RpcStreamFrame`, delivers to `mpsc` channel
8. **Primary daemon** relays each frame back to CLI via IPC
9. Stream ends when `done: true` is received

### Channel Types

| Connection | Channel Type | Why |
|---|---|---|
| CLI ↔ Primary (streaming) | IPC stream (read/write halves) | Bidirectional: frames out, input/resize in |
| Primary → NodeRegistry | `mpsc::UnboundedReceiver<Result<RpcResponse>>` | Multiple frames per request |
| Primary ↔ Secondary WS | Existing WebSocket (shared) | `RpcStreamFrame` variant added to protocol |
| Secondary → Local Daemon | IPC stream (same as CLI↔Daemon) | Reuses existing streaming protocol |

### Client Input During Proxy Streaming

While a proxied streaming session is active, the CLI continues to send
`AttachInput`, `AttachResize`, and `AttachDetach` on the same IPC connection.
The primary daemon reads these from the IPC reader and proxies them as one-shot
RPCs to the secondary node:

```text
CLI ──AttachInput──► Primary ──proxy_rpc()──► Secondary
```

---

## 16) Design Constraints & Future Work

### Current Constraints

1. **Single PTY per session**: No window/pane splitting (not a multiplexer)
2. **Last-resize-wins**: No multi-client size aggregation yet
3. **No scrollback beyond ring**: Ring is fixed-size; disk persistence is
   append-only but not queryable for replay
4. **Blocking PTY I/O**: 2 threads per session limits to ~hundreds of sessions
5. **No flow control**: Fast PTY output may overwhelm slow clients (broadcast
   channel provides buffering but not backpressure)

### Future Work

- **Smallest-client resize**: Track attached client sizes, resize PTY to minimum
- **Async PTY on Linux**: Use `AsyncFd` for PTY reads on platforms that support
  it, falling back to threads on Windows
- **Scrollback API**: Allow clients to request historical output beyond the
  ring buffer via the persisted append-only log
- **Structured output parsing**: Detect common patterns (exit codes, prompts)
  in PTY output for enhanced notifications
- **PTY health monitoring**: Detect hung processes (no output + no child exit)
  and offer automatic cleanup
