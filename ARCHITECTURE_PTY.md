# PTY Management Architecture

> Standalone architecture reference for PTY lifecycle, terminal mode tracking,
> streaming protocols, escape-sequence handling, and PTY/platform edge cases in
> Open Relay.
>
> See also: [`ARCHITECTURE.md`](ARCHITECTURE.md) for system-wide architecture.
> Detailed edge cases and notes now live in
> [`ARCHITECTURE_NOTES.md`](ARCHITECTURE_NOTES.md).

---

## Table of Contents

1. [Design Philosophy](#1-design-philosophy)
2. [PTY Lifecycle](#2-pty-lifecycle)
3. [Module Layout](#3-module-layout)
4. [PtyHandle — Ownership Boundary](#4-ptyhandle--ownership-boundary)
5. [Reader & Writer Threads](#5-reader--writer-threads)
6. [Snapshot & Replay](#6-snapshot--replay)
7. [Mode Tracking](#7-mode-tracking)
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

Open Relay manages PTY sessions as a **daemon-side resource**.  The client is
a regular terminal emulator — we never render cells or maintain a screen
buffer.  We only keep lightweight byte-level mode state and enough
cursor-position state to answer shared terminal queries.

Design principles:

| Principle | Rationale |
|---|---|
| **Daemon owns the PTY** | Sessions survive client disconnect/reconnect |
| **Raw byte streaming** | Preserve all escape sequences; let the client terminal render |
| **Broadcast fan-out** | Multiple clients attach to the same session simultaneously |
| **Byte-level state tracking** | Mode changes (DECCKM, bracketed paste) tracked without parsing full VT |
| **Cross-chunk correctness** | All parsers carry state across read boundaries |
| **Blocking I/O for PTY** | Dedicated OS threads avoid Tokio executor starvation |

### Architectural Choices

- **Event-driven I/O**: PTY reads and writes are decoupled from attach clients
  using dedicated OS threads plus Tokio broadcast channels.
- **Attach tracking**: `SessionStore` tracks live attach presence separately
  from PTY lifetime so clients can disconnect and reconnect without killing the
  session.
- **Deferred cleanup**: completed sessions keep their rendered screen and persisted
  output available until eviction.
- **Single shared PTY size**: the PTY adopts the most recent successful resize
  request; per-client size aggregation remains future work.
- **Platform-safe child startup**: `portable_pty` handles the platform-specific
  PTY setup and child-spawn details for us.

---

## 2) PTY Lifecycle

```text
                    spawn_session()
                         │
                    ┌────▼────┐
                    │ Spawned │  PtyHandle created
                    │         │  Reader/writer threads started
                    │         │  Screen + log empty
                    └────┬────┘
                         │
              ┌──────────▼──────────┐
              │      Running        │  PTY output → screen + log + broadcast
              │                     │  Input accepted from clients
              │  ┌───────────────┐  │
              │  │  Attach(ed)   │◄─┼── IPC / WebSocket / node proxy
              │  │  Detach       │──┼── Client disconnect
              │  └───────────────┘  │
              └──────────┬──────────┘
                         │  Child exits (SIGCHLD / WaitForSingleObject)
                    ┌────▼────┐
                    │ Exited  │  Exit code captured
                    │         │  Screen + log preserved (replayable)
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
├── pty.rs              PTY ownership + terminal semantics (this doc)
│   ├── PtyHandle       Master fd, child, writer channel
│   ├── RuntimeChild    Child process wrapper
│   ├── TerminalQuery   Probes the daemon answers, plus reply generation
│   └── TerminalSignals Retained title / progress / cursor-shape notifications
├── scan.rs             Single-pass byte-level PTY output scanner
│   ├── PtyScanner      VT state machine: filter + probes + signals + activity
│   └── ScanOut         Reused output buffers for one scan call
├── resize.rs           Resize broadcast helper for attach handlers
│   └── ResizeSubscriber  Self-echo suppression for resize notifications
├── runtime.rs          SessionRuntime (screen + broadcast + pty + meta)
│   ├── spawn_session() PTY spawn + reader/writer thread creation
│   ├── push_output()   Screen parse + stream counters + mode publication
│   ├── SharedModes     Lock-free DECCKM / bracketed-paste mirror
│   ├── resize_tx       Broadcast channel for resize notifications
│   └── pty_size        Current PTY dimensions for dedupe
├── screen.rs           Screen parser helpers (safe resize, rehydration)
├── logs/               Reading back the persisted stream
│   ├── index.rs        Record boundaries, `output.log.idx` sidecar, pagination
│   └── render.rs       vt100 replay of log bytes into rendered rows
├── store/              SessionStore, one file per concern
│   ├── mod.rs          Shared state, constants, `lookup_runtime()`
│   ├── lifecycle.rs    start / stop / kill / terminate / evict
│   ├── query.rs        Read-only observation, metadata edits, live-log reads
│   ├── attach.rs       attach / detach / input / resize (latency-sensitive)
│   └── notify.rs       Silence detection for push notifications
├── file.rs             Uploaded-file storage under `sessions/<id>/files/`
└── persist.rs          Disk persistence (append-only log + OutputLog handle)
```

`store.rs` and `logs.rs` are directories rather than single files purely for
comprehension: each submodule contributes its own `impl SessionStore` block, so
`SessionStore` remains one type with one public API.

There is no separate `mode_tracker.rs`, `cursor_tracker.rs` or `ring.rs`.
Terminal modes and the cursor position are read directly from the `vt100`
screen parser that already models the session, and the retained stream is the
append-only `output.log` rather than an in-memory ring.

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
| `try_write_input(data)` | Non-blocking channel send to writer thread | `TrySendError::Full` / `TrySendError::Closed` |
| `resize(rows, cols)` | `pty_master.resize()` → SIGWINCH / ConPTY resize | Returns `false` if master is unavailable or resize fails |
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
    let mut buf     = vec![0u8; 64 * 1024];   // one read swallows a whole burst
    let mut scanner = PtyScanner::new();      // carries partial sequences + signals
    let mut out     = ScanOut::default();     // reused buffers, no steady-state alloc
    let mut log     = OutputLog::open(dir);   // persistent append handle

    loop {
        let n = master_reader.read(&mut buf);
        if n == 0 || n == Err(_) → break;

        // 1. ONE pass over the chunk produces everything downstream needs.
        scanner.scan(&buf[..n], &mut out);
        //    out.filtered       → the canonical stream
        //    out.queries        → probes that need a session-global reply
        //    out.signal_bytes → bytes that are not screen activity
        //    scanner signals    → title / progress / cursor shape, if changed

        // 2. One write lock: screen parse, stream counters, mode publication.
        let cursor = runtime.write().push_output(&filtered, out.meaningful_bytes());

        // 3. Persist and fan out the same filtered bytes.
        log.append(&filtered);
        broadcast_tx.send(filtered);          // Bytes, refcounted, no copy per client

        // 4. Answer the probes that have a session-global reply.
        for query in out.queries.drain(..) {
            writer_tx.send(query.response(cursor));
        }
    }
}
```

**Why a blocking thread?**  High-bandwidth PTY output (e.g., `cat /dev/urandom`)
would starve the Tokio executor if run as an async task.  A dedicated thread
ensures PTY reads never block other tasks.

**Why one pass?**  The previous pipeline filtered each chunk with ~20 sequential
`regex::bytes::Regex::replace_all` passes, each allocating a fresh buffer, then
scanned the raw chunk a second time to extract probes, and scanned the filtered
chunk a third time to discount progress notifications.  Measured end to end that
capped the reader at roughly 35 MB/s.  Because a paste is echoed back by the
child, *every* pasted byte paid that cost, which is exactly why large pastes
crawled.  The single-pass scanner measures at ~1.7 GB/s on TUI-style output and
~10 GB/s on plain echoed text.

**Why a 64 KiB buffer?**  Once scanning is cheap, the per-chunk overheads —
read syscall, write lock, log write, broadcast send, IPC frame — dominate.  A
larger read collapses a burst into far fewer chunks, and therefore far fewer of
each of those.

**Why a persistent log handle?**  `OutputLog` keeps `output.log` open in append
mode for the life of the session instead of reopening it per chunk.  Writes are
unbuffered, so replay readers (`read_output_from`) still observe bytes as soon
as the append returns.

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

The writer queue holds 4096 messages.  A bracketed paste arrives as a burst of
input frames, and a full queue makes the daemon reject input with
`SessionError::Busy`, which the user experiences as dropped keystrokes; the
writer drains at PTY speed, so a deep queue costs only memory.

The reader answers terminal capability probes centrally before bytes are fanned
out to attach clients.  This keeps detached sessions progressing, and it avoids
leaking probes such as CPR/DSR/DA/DECRPM to the real terminal attached to an
IPC or WebSocket client.

---
## 6) Snapshot & Replay

The retained stream is the append-only `output.log` written by the reader
thread.  It holds the *canonical filtered stream*: the exact bytes that also
feed the screen parser and the live broadcast, so every stream offset in the
system refers to filtered bytes.

### Replay on Attach

When a client attaches with an explicit offset, `attach_subscribe_init()`:
1. Takes a short read lock to clone the session directory and subscribe to the
   broadcast channel
2. Reads `output.log` from the requested offset
3. Returns the replay bytes + end offset + receiver + current mode snapshot

Because the log already stores the canonical filtered stream, attach handlers
run no filtering of their own.

### Terminal Signal Restore

A fresh attach normally starts from `attach_snapshot_init()`, which renders the
canonical screen state (`vt100::Screen::state_formatted`) instead of replaying
history.  That state models only the character grid, cursor and input modes, so
three one-way signals would be lost on every attach:

| Signal | Sequence | Tracked in |
|---|---|---|
| Icon / window title | `OSC 0`, `OSC 1`, `OSC 2` | `TerminalSignals::icon_title` / `window_title` |
| Progress / busy indicator | `OSC 9;4;<state>;<pct>` | `TerminalSignals::progress` |
| Cursor shape (DECSCUSR) | `CSI <n> SP q` | `TerminalSignals::cursor_style` |

`PtyScanner` records these as it walks each chunk — the same pass that produces
the filtered stream — and reports through `take_changed_signals()` whether
anything actually changed.  The reader copies the new snapshot into
`SessionRuntime::terminal_signals` only on a change, so the common case costs no
extra work inside the write lock.  `attach_snapshot_bytes()` appends
`TerminalSignals::restore_bytes()` after the screen state.  A cleared progress
indicator (`OSC 9;4;0`) and a default cursor shape (`CSI 0 SP q`) drop their slot
instead of being replayed as a no-op, and payloads over 1 KiB are ignored so a
misbehaving child cannot pin memory.

On Windows the attach client repaints from its own canonical parser, so
`passthrough_signals()` re-extracts the same signals from each live chunk
(`extract_passthrough_osc_sequences` plus `last_cursor_style_params`) and writes
them ahead of the repaint.

### Scrollback Seeding

The snapshot restores only the visible screen, so a freshly attached
terminal's scrollbar would start empty — and on Windows the canonical repaint
never writes to the real scrollback at all.  For sessions **not** in the
alternate screen, the daemon therefore appends a `scrollback` field to
`AttachStreamInit`, gated on the subscribe carrying terminal dimensions so
piped attaches stay byte-exact.  Alternate-screen sessions skip seeding
because rows that scrolled off there are not linear history.

The seed holds only scrolled-off rows, at most the attaching client's own
screen height — the visible screen is excluded because the snapshot already
covers it, and seeding it would duplicate content in the client's scrollback.
The attached view thus reads like a natively run CLI: up to one screenful of
history above the live screen.

The seed is rendered from the session's live `vt100` parser, which retains
scrolled-off rows in memory — bounded by the `screen_scrollback_rows` config
key (default 5000) — while the persisted `output.log` stays the authoritative
full history.  vt100 keeps
scrollback only for the main grid, so alternate-screen TUIs cost nothing, and
it exposes retained rows only through the viewing offset (one screenful per
view), which is why `scrollback_rows()` pages through the offset.  Resizes
rebuild the parser from its state snapshot, so `safe_resize_parser()` first
re-feeds the retained rows as plain lines to carry them into the new parser's
scrollback; because the alternate grid's scrollback is inaccessible while a
TUI is active, a resize during a full-screen TUI drops the pre-TUI history.

The CLI prints the seed before the snapshot: each row is CRLF-terminated
(raw mode disables ONLCR), then a screenful of padding newlines scrolls every
seeded row into the real scrollback.  Plain scrolling is used instead of ED 2
because `\x1b[2J`'s effect on scrollback differs between terminals; the
seeded rows read continuously into the repainted live screen, matching how
the child would have looked had it run directly in the user's terminal.

### Offset Tracking

Each byte in the log has a monotonically increasing logical offset.  Clients
track their current offset to detect gaps (e.g., after broadcast lag) and resume
by re-reading the log from that offset.

---

## 7) Mode Tracking

Terminal modes are **not** tracked by a separate state machine.  The session
already maintains a full `vt100::Parser` for snapshot rendering, and that parser
models DEC private modes correctly, including sequences split across PTY read
boundaries.  `SessionRuntime::mode_snapshot()` simply reads them back:

```rust
pub struct ModeSnapshot {
    pub app_cursor_keys: bool,      // screen.application_cursor()  — DECCKM
    pub bracketed_paste_mode: bool, // screen.bracketed_paste()     — DEC 2004
}
```

### Lock-Free Publication (`SharedModes`)

Every attach relay has to notice the moment either mode flips, and the natural
place to check is after each output chunk.  Deriving the snapshot needs the
session's `RwLock`, and taking it once per chunk per client turned the reader
thread's lock into a contention point under load.

Instead, `push_output()` packs the two bits into an `AtomicU8`
(`SessionRuntime::shared_modes`) while it already holds the write lock.  Relays
clone the `Arc<SharedModes>` once at subscribe time and afterwards poll it with
a relaxed atomic load — no lock, no allocation.

Used by:
- `AttachStreamInit` to send initial mode state to clients
- `AttachModeChanged` to notify clients of mode transitions
- `attach_input()` to transform arrow keys when DECCKM is active

---
## 8) Escape Sequence Pipeline

Every byte the child writes passes through **one** scanner pass
(`src/session/scan.rs`) before it reaches anything else:

```text
PTY master fd
    │
    ▼
┌───────────────────────────────────────────────────────────────┐
│ PtyScanner::scan()  — a single byte-level VT state machine    │
│                                                               │
│  • bulk-copies runs of plain text (memchr to the next ESC)    │
│  • strips terminal↔application protocol traffic               │
│  • collects the probes that need a session-global reply       │
│  • records title / progress / cursor-shape notifications      │
│  • counts progress bytes so they are not "screen activity"    │
│  • carries an incomplete trailing sequence into the next chunk│
└───────────────────────────┬───────────────────────────────────┘
                            │  ScanOut { filtered, queries, signal_bytes }
              ┌─────────────┼─────────────┬──────────────────┐
              ▼             ▼             ▼                  ▼
      push_output()   OutputLog     broadcast_tx     query.response()
      screen parse    append        fan-out          → PTY stdin
      + counters      output.log    (Bytes)
      + SharedModes
```

This section is the source of truth for terminal query handling and filtered PTY
output.  The detailed incident catalog for discovered escape-sequence quirks
lives in
[`ARCHITECTURE_NOTES.md`](./ARCHITECTURE_NOTES.md#1-architecture-wide-escape-sequence-edge-cases).

### Why a Hand-Written Scanner

The scanner replaced a cascade of roughly twenty
`regex::bytes::Regex::replace_all` calls, each allocating a fresh buffer, plus a
separate probe-extraction scan and a separate progress-accounting scan.  Beyond
the ~50× throughput difference, the single machine also removes a class of
correctness hazards the cascade had: each regex saw the output of the previous
one, so a sequence could be created or destroyed by an earlier substitution, and
ordering between the passes was load-bearing but implicit.

`ScanOut` buffers are owned by the reader thread and reused across chunks, so
steady-state scanning performs no allocation at all.

### What Is Stripped

| Category | Sequences |
|---|---|
| Cursor/status reports | `CSI <row>;<col> R`, `CSI ?<row>;<col> R`, `CSI <n> n`, `CSI ?<n> n` |
| Device attributes | `CSI c`, `CSI <n> c`, `CSI > c`, `CSI = c` and their replies |
| DEC private mode reports | `CSI ? <mode> $p`, `CSI ? <mode>;<v> $y` |
| Version probe | `CSI > <n> q` (XTVERSION) |
| Kitty keyboard | `CSI ? u`, `CSI = <n> u`, `CSI > <n> u`, `CSI < <n> u` |
| Window-size probes | `CSI 14 t` … `CSI 19 t` (other `CSI <n> t` forms pass through) |
| String sequences | DCS / APC / PM / SOS payloads (`ESC P`, `ESC _`, `ESC ^`, `ESC X`) |
| Non-allowlisted OSC | every `OSC <ps>` other than the allowlist below |

`CSI <n> u` without a private marker is **not** stripped: that is the kitty
keyboard *stack pop*, a legitimate output sequence.  Only the marked variants
are probes.

### Passthrough Allowlist

`OSC 0/1/2` (icon and window title) and `OSC 9;4;…` (progress / busy) are
one-way notifications and survive the filter.  Only BEL and `ESC \` terminate an
OSC: a lone backslash is payload, since Windows shells report titles such as
`C:\Users\me`.

All passthrough bytes — titles *and* `OSC 9;4` progress — are forwarded to
clients but excluded from the meaningful-byte count
(`ScanOut::meaningful_bytes()`), so a child that only retitles itself (e.g.
`]0;[ ! ] Action Required | build`) or animates a progress indicator is still
correctly treated as silent by notification logic.  This is a deliberate
trade-off: a bare title flip usually announces an input-required prompt rather
than real progress, and genuine progress almost always comes with regular
screen output that still counts as activity.

### ConPTY Bare Forms

Windows ConPTY sometimes drops the introducing `ESC`, so a cursor report can
arrive as `[35;1R` and a title as `]0;title\x07`.  These escape-less variants are
recognised **only on Windows** (`BARE_CONPTY_FORMS = cfg!(windows)`).  On Unix
the same bytes are ordinary program output, and treating them as sequences
corrupts the rendering of anything that legitimately prints `[12;3R`.  A bare
form that reaches the allowlist is rewritten into the escaped form before being
forwarded, since emitting it verbatim is not a valid escape sequence.

### Cross-Chunk Handling

The scanner's `pending` buffer carries an incomplete trailing sequence into the
next chunk, so a sequence split at any byte boundary is still handled correctly.
The carried prefix is capped at `MAX_PENDING_ESCAPE_BYTES` (4 KiB): plain text
that merely looks like the start of a sequence (a line ending in `]12;`, say)
is flushed verbatim rather than stalling the session's output while `pending`
grows without bound.

A dedicated test (`chunking_does_not_change_the_result`) re-scans the same input
at every chunk size from 1 byte upward and asserts the filtered output is
identical, which is the property the whole design depends on.

### Query Response Generation

The daemon answers only the probes whose correct answer is a property of the
*session*, not of whatever terminal happens to be attached:

| Query | Sequence | Response |
|---|---|---|
| Cursor Position Report | `CSI 6 n` | `CSI <row>;<col> R` from the screen parser |
| Device Status Report | `CSI 5 n` | `CSI 0 n` |
| Foreground color | `OSC 10 ; ?` | `OSC 10 ; rgb:…` (ST-terminated) |
| Background color | `OSC 11 ; ?` | `OSC 11 ; rgb:…` (ST-terminated) |

Color responses use the `COLORFGBG` environment variable if set, otherwise
default to white-on-black.

Device attributes, XTVERSION, DEC private mode reports, kitty keyboard flags and
window-size-in-pixels probes are **stripped but deliberately not answered**.
Their answers describe the user's terminal, which the daemon does not know and
may not have; injecting a guess into the child's stdin corrupts the input stream
of anything that was not waiting for that reply.

---
## 9) Streaming Attach Protocol

All attach paths (IPC, WebSocket, node-proxied) use the same streaming
protocol.  This was unified from separate polling/streaming implementations.

### Frame Types

| Frame | Direction | Purpose |
|---|---|---|
| `AttachStreamInit` | Server → Client | Screen snapshot or log replay + initial mode state |
| `AttachStreamChunk` | Server → Client | Incremental PTY output (filtered) |
| `AttachModeChanged` | Server → Client | Terminal mode transition notification |
| `AttachResized` | Server → Client | PTY resized by another attached client |
| `AttachStreamDone` | Server → Client | Session ended (with exit code) |
| `AttachInput` | Client → Server | Keyboard/paste input |
| `AttachResize` | Client → Server | Terminal size change |
| `AttachDetach` | Client → Server | Voluntary disconnect |

### IPC Streaming Flow

```text
CLI                              Daemon
 │                                  │
 │──AttachSubscribe────────────────►│
 │                                  │  subscribe to broadcast + snapshot/log replay
 │◄──AttachStreamInit──────────────│  (replay bytes + mode snapshot)
 │                                  │
 │◄──AttachStreamChunk─────────────│  (live PTY output, filtered)
 │◄──AttachStreamChunk─────────────│
 │──AttachInput────────────────────►│  (keyboard input)
 │──AttachResize───────────────────►│  (terminal resize)
 │◄──AttachModeChanged─────────────│  (DECCKM toggled)
 │◄──AttachResized─────────────────│  (another client resized the PTY)
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
{"type": "resized", "rows": 24, "cols": 80}
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

resize_tx   ──► resize_rx_1 (IPC client 1)
              ├► resize_rx_2 (WebSocket client)
              └► resize_rx_3 (node-proxied client)
```

### Shared Input

All attached clients write to the same PTY stdin.  Input is **not** isolated —
keystrokes from any client are interleaved.  This is intentional: attached
clients share a single PTY input stream.

### Resize Broadcast

When any attached client resizes the PTY, the new dimensions are broadcast to
all *other* attached clients via a dedicated `resize_tx` broadcast channel.
Each attach handler wraps that receiver in `ResizeSubscriber`, which tracks the
last resize it sent (`last_self_resize`) and suppresses the matching echo so
the originating client never receives its own resize back.  Redundant resizes
(same rows × cols as the current PTY size) are skipped at the
`SessionRuntime` level.

### Broadcast Lag Recovery

If a subscriber falls behind (e.g., slow WebSocket), `RecvError::Lagged` is
returned.  The handler re-syncs by:
1. Re-subscribing to the broadcast channel
2. Re-reading `output.log` from the last acknowledged offset
3. Continuing from the new end offset

### Attach Presence Tracking

`SessionStore` tracks whether any client is currently attached.  This is used
for:
- Attach accounting / idle tracking
- Distinguishing detached automation from live interactive viewing
- Notification suppression (no "session ended" push if client is watching)

---

## 11) Resize Protocol

### Ordering Constraint

Attach-driven resize is sequenced **after** the init frame is produced:

1. `attach_subscribe_init()` snapshots replay bytes + live receivers
2. Server sends `AttachStreamInit`
3. Server registers attach presence and subscribes to resize broadcasts
4. The attach path applies its initial size, if it has one

Current attach paths differ slightly:

- **WebSocket**: browser dimensions are supplied up front, so the daemon sends
  `init`, registers the client, creates `ResizeSubscriber`, and then applies
  the initial resize on the server side.
- **IPC / CLI**: the daemon sends `AttachStreamInit` first, then the interactive
  CLI immediately follows with `AttachResize` using the local terminal size.
  The daemon does not auto-resize IPC attaches by itself.

### Multi-Client Resize Strategy

The effective strategy is **last successful resize wins**.  Newly attached
interactive clients usually become the current winner because they send an
initial `AttachResize` immediately after `AttachStreamInit`, and later resize
events from any client overwrite the PTY size again.  Every successful resize
emits an `AttachResized` notification to the other attached clients.

The PTY tracks its current dimensions in `SessionRuntime::pty_size`.  Resize
requests that match the current size are no-ops — neither the PTY nor other
clients are notified.

### Cursor Position Reporting

CPR replies are generated from the session's `vt100` screen parser, not from a
separate approximate cursor model.  `push_output()` returns
`screen().cursor_position()` after processing the chunk, and the reader uses
that value for every probe found in the same chunk.  Because the parser is also
what `safe_resize_parser()` resizes on every successful PTY resize, the reported
position is automatically consistent with the current PTY dimensions — including
resizes that originated from another attached client.

### Race Condition: Rapid Resize

If a client sends multiple resize events in quick succession (e.g., during
window drag), each triggers a `SIGWINCH`.  The child may emit partial redraws
for intermediate sizes.  Mitigation: the web UI debounces resize sends by
120ms, and the CLI attach path ignores stale resize events for the first 500ms
after entering the alternate screen before re-reading the actual terminal size.

---

## 12) Cross-Platform Edge Cases

The detailed PTY cross-platform edge-case catalog now lives in
[`ARCHITECTURE_NOTES.md`](./ARCHITECTURE_NOTES.md#4-pty-cross-platform-edge-cases),
including the ConPTY- and POSIX-specific caveats plus the encoding assumptions
for raw PTY byte handling.

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
| Child crashes | `try_wait()` returns exit code | Send `AttachStreamDone`, preserve output.log |
| PTY read failure | Reader thread `read()` returns error | Thread exits, broadcast closed |
| Client disconnect | IPC/WebSocket `recv()` returns None | `attach_detach()` cleanup |
| Broadcast lag | `RecvError::Lagged(n)` | Re-subscribe + replay `output.log` from offset |
| Writer channel full | `send()` returns error | Input dropped (logged as warning) |
| IPC connection reset | `read_request` returns error | Client reader task exits |
| Node proxy disconnect | WebSocket closed | Stream receiver gets None |
| Master fd close race | `write()` to closed fd | Writer thread gets `BrokenPipe`, exits |

### Invariants

1. **Canonical stream is always consistent**: the PTY reader is the single writer — scan, screen update, log append and broadcast happen in that order for every chunk
2. **Mode state is always consistent**: `TerminalSignals` and `SharedModes` are only updated from the PTY reader's `push_output` path
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

1. **Single PTY per session**: No window/pane splitting or layout management
2. **Last successful resize wins**: Newly attached interactive clients usually
   send an immediate resize, so the most recently attached or resized client's
   dimensions are applied; other clients receive a resize notification but
   their local terminal size is not forcefully changed
3. **Unbounded `output.log`**: the retained stream grows for the life of the
   session; there is no compaction or age-based truncation yet
4. **Blocking PTY I/O**: 2 threads per session limits to ~hundreds of sessions
5. **No flow control**: fast PTY output may overwhelm slow clients.  Output is
   batched rather than throttled: relays coalesce every already-queued chunk
   into one frame (capped at 1 MiB) and slow subscribers that fall off the
   broadcast channel resync from `output.log`, so nothing is lost — but a client
   that cannot keep up still cannot slow the child down.

### Future Work

- **Smallest-client resize**: Track attached client sizes, resize PTY to minimum
- **Async PTY on Linux**: Use `AsyncFd` for PTY reads on platforms that support
  it, falling back to threads on Windows.  The
  [rmux `PtyIo`](https://github.com/Helvesec/rmux/blob/main/crates/rmux-pty/src/pty.rs)
  design — a nonblocking master fd exposed via `as_fd()` with `try_read` /
  `try_write_immediate` — is a good reference for this
- **Log compaction**: bound `output.log` growth while keeping offset-addressed
  replay working
- **Binary attach framing**: attach payloads are base64 inside JSON today
  (~1.37× wire overhead); a length-prefixed binary frame would remove both the
  expansion and the encode/decode pass
- **Structured output parsing**: Detect common patterns (exit codes, prompts)
  in PTY output for enhanced notifications
- **PTY health monitoring**: Detect hung processes (no output + no child exit)
  and offer automatic cleanup
