# oly architecture

`oly` runs interactive terminal programs under a daemon so they survive
closed terminals, can be observed and driven from several places at once,
and leave a truthful, durable recording.

This file is the 10-minute orientation. **The code is the source of truth**;
this document only explains the shape so you know where to look. Design
decisions and their reasons are captured in this document — read it
before changing anything structural.

## The three rules

Everything in the current design follows from keeping exactly one implementation of each
core concern:

1. **One journal.** Every session persists a single ordered stream — raw PTY
   output, resizes, lifecycle transitions, checkpoints — in a per-session
   journal (`src/session/journal.rs`). Nothing else is persisted per
   session. Shared SQLite session metadata may cache *derived hints* (for
   example, a `resume_command` discovered from the rendered journal tail
   after both child exit and PTY EOF using configurable `resume_patterns`);
   it is not another recording.
   Everything else (the filtered display stream, resize history,
   logs, attach snapshots) is *derived* from the journal at read time
   (`src/session/replay.rs`), so derived state can never silently disagree
   with the recording. Replay is checkpoint-anchored: anchored v2
   checkpoints carry their filtered-stream offset, so deriving a window
   costs O(checkpoint cadence), never O(total recording) (the checkpoint-
   anchored replay design).
   The journal is always on; if it fails, the session
   fails loudly rather than recording nothing (ADR-0002, ADR-0006).
   Sessions left over from pre-0.5 builds (only `output.log`) are rejected
   with an explicit error (see MIGRATION.md).
2. **One terminal engine.** All terminal-state questions — the live screen,
   `oly logs` rendering, web views, Windows repaint — are answered by one
   embedded alacritty engine (`src/terminal/`). vt100 survives only as a
   test-only conformance oracle (ADR-0001).
3. **One framing.** Attach streams are binary-framed end to end
   (`src/ipc.rs`, `web/src/api/ws-frames.ts`): length-delimited frames with
   raw output bytes, JSON only for low-volume control (ADR-0004).

## Component map

| Component | Where | What it does |
|---|---|---|
| CLI | `src/main.rs`, `src/cli.rs`, `src/client/` | Parses commands, talks to the daemon over the local IPC socket. |
| Daemon | `src/daemon/` | Owns all sessions. Serves IPC (`rpc*.rs`), HTTP + WebSocket (`src/http/`), and node federation (`src/node/`). |
| Session runtime | `src/session/runtime.rs` | One per session: owns the PTY, feeds the engine, sequences every output chunk through a single point, journals it, broadcasts it. |
| Journal | `src/session/journal.rs` | Segmented, incarnation-fenced append-only log with durability cursors and checkpoint-gated retention. Integrity is verifiable through manifest validation. |
| Replay | `src/session/replay.rs`, `src/session/logs/` | Derives the filtered display stream, resize history, and rendered logs from the journal. |
| Attach pump | `src/session/store/pump.rs` | The single state machine that serves every attach: snapshot from the journal, then gapless live chunks with enforced credits and bounded per-client queues. |
| Attachments | `src/session/store/attach.rs` | Registry with roles fixed at registration (controller, or view-only observer); geometry authority gated by role. |
| Web UI | `web/` | React + xterm.js; consumes the same binary frame protocol over WebSocket. |
| Federation | `src/node/`, `src/daemon/rpc_nodes.rs` | A primary supervises sessions on secondary nodes; remote attach relays the same stream with fencing, deadlines, and keepalives. See “Node federation”. |

## Following a byte: the key data flows

Output (one producer, fan-out at the sequencing point):

```
        child process (PTY)
              │ PTY read (reader thread)
        session/runtime.rs — single sequencing point:
        every chunk becomes SequencedChunk(incarnation, offset)
              │
      ┌───────┼────────────────────┐
      ▼       ▼                    ▼
  journal   broadcast ring   terminal engine
  (durable, (live attach,    (live screen, ModeSnapshot:
  fenced)   bounded lag)     DECCKM/paste/mouse/focus)
      │       ▲
      └───────┘ resync & completion tail read from the journal,
                never from memory (I2: gaps are errors, not skips)
              │
        AttachPump (store/pump.rs) — THE one streaming state machine:
        snapshot → gapless chunks → coalesce → bounded queue → credits
        emits AttachEvent { Chunk | Modes | Resync | End }
              │
   ┌──────────┼──────────────────┐
   ▼          ▼                  ▼
 IPC binary  WS binary frames   node relay (RpcStreamFrame)
 frames     (browser xterm.js)  (federated attach via primary)
 (CLI)      ws.rs + ws-frames.ts
```

Input (one gate):

```
CLI keys / xterm onData ──► AttachClientMessage
  { input | resize | applied-cursor | detach }
              │
  attachment registry (store/attach.rs): role gate —
  observe-mode attachments are rejected; controllers drive directly
              │
  PTY writer (runtime.rs) ──► child stdin
```

Two design points worth knowing before touching this code:

- **Flow control is cursor-based, not queue-based.** Clients ack an
  `applied-cursor`; the pump enforces stream credits and bounded queues,
  so a stuck client is resynced (from the journal, in bounded windows)
  or dropped — never buffered forever (I7).
- **Modes are engine state, mirrored per transport.** The engine tracks
  DECCKM, bracketed paste, mouse reporting (1000/1002/1003 collapsed to
  one flag, plus SGR 1006), and focus reporting (1004) in a
  `ModeSnapshot`. Snapshots never replay raw DECSET bytes; instead each
  transport mirrors the authoritative snapshot — the CLI drives its own
  local terminal (`sync_local_terminal_modes` in `client/attach.rs`),
  the browser gets the flags in the INIT/MODE_CHANGED frames
  (`web/src/api/ws-frames.ts: terminalModeSequences`) and writes a
  full set-or-clear DECSET stream into xterm. That is why mouse and
  focus work on a page load into an already-enabled program.
- **Scripted mouse input is application input, not a history view.**
  `oly send <id> mouse:wheel+3` or `mouse:left@2,3` uses a one-shot
  `AttachMouse { id, spec }` RPC. The owning daemon parses once, resolves live
  mouse modes and viewport geometry, and enqueues bytes under the input lock.
  `src/mouse.rs` shares encoding with interactive attach. Counts are bounded
  events, not rows; clicks require explicit positions and emit press/release
  pairs. Only wheel defaults to the viewport center. Unsupported modes and
  out-of-viewport positions fail explicitly. The application's redraw stays
  ordinary journaled PTY output visible through `oly logs --screen`; no
  viewing offset or second recording is introduced. Node proxies allow this
  operator RPC, not mid-stream attach messages.
- **CLI paste boundaries belong to the input reader.** The outer terminal
  keeps bracketed paste enabled even when the child does not; the CLI wraps
  an `Event::Paste` only when the child's mode snapshot enables it. Unix
  uses crossterm's event decoder. Windows uses a VT-aware console reader
  (`src/client/attach_input.rs`), since crossterm 0.29's Windows backend
  does not decode paste markers and loses raw VT control characters.
  Explicit paste boundaries are decoded before Enter, clipboard, or detach
  handling, preserving pasted CR/LF and Unicode. Windows mouse-mode changes
  preserve raw/VT input bits; teardown restores the console-mode baseline.
  Complete VT input sequences (mouse reports, focus events, and modified
  keys) also stay in one input frame: ConPTY can turn a CSI split across
  writes into native keyboard text instead of the intended mouse/key event.
  VT-encoded Shift+Insert is decoded back into the local clipboard action;
  it must not bypass file/image saving. VT input suppresses ConPTY's automatic
  native-key reporting, so attach explicitly requests Windows mode 9001 and
  unwraps its encoded key records before decoding VT/paste payloads. Windows
  Terminal can consume Ctrl+V/Shift+Insert key-down for its paste binding;
  bitmap-only paste emits no text, so an unmatched shortcut key-up must
  recover the clipboard action once. Key-up after an observed press must
  not duplicate it. Shortcut bytes inside bracketed paste remain literal.
  Teardown disables mode 9001 before returning control to the parent shell.
- **A screen repaint is not scrollback.** Windows attach still renders the
  canonical engine screen instead of forwarding ConPTY's wrap-dependent
  bytes, but the same parser delegates through `terminal/render_effects.rs`
  to capture newly scrolled main-buffer rows, alternate-buffer transitions,
  and explicit history clears. The client forwards those effects to the
  outer terminal in order, then repaints with row erases (not ED 2, which
  can manufacture duplicate history). Retention is bounded to a viewport
  for resize reflow; this is derived display state, not another recording.
  Fullscreen TUIs keep application-owned scrolling and mouse/key input;
  main-screen CLIs keep terminal-owned scrollback. Reattach seeds exclude
  the visible grid and flush every seeded row off-screen before repainting.

## Node federation

Any daemon can be a **primary** (it serves `GET /api/nodes/join`); any
daemon can attach to a primary as a **secondary** (`oly join start`,
config persisted in `joins.json`, connector task reconnects with
exponential backoff 1→60 s — `client/join.rs`, `daemon/rpc_nodes.rs`).

Federation-v2 handshake (`src/federation_crypto.rs`, `http/nodes.rs`,
`daemon/rpc_nodes.rs`; transport is WebSocket, not SSH):

```text
secondary                                     primary
   | -- plain Hello(version,name,key,nonce) --> |
   | <-- plain Challenge(primary_nonce) ------ |
   | -- plain Join(signature of transcript) -->| verify exact registered name/key
   | <-- sealed KeyConfirm(hash), s2c seq 0 --- | no registration yet
   | -- sealed Ready(hash), c2s seq 0 --------> | atomic registry insertion
   | <-- sealed Joined, s2c seq 1 ------------ | connector may now report success
   | <==== sealed RPCs/events/keepalives =====> |
```

Each daemon persists `identity_key` (32-byte Ed25519 seed) and
`identity_key.pub` (`ed25519 <standard-base64(raw32)>`). Operators independently
exchange public identities: accept binds the secondary name/key in
`node_identities`, and `--identity-pub-key` pins the primary on the secondary.
There is no in-band replacement key, OpenSSH import, or plaintext fallback.
The transcript length-prefixes its domain, version, role labels, name, both
raw identities, and both fresh 32-byte nonces. The primary verifies the
secondary's strict Ed25519 signature; encrypted confirmation proves matching
key possession in both directions. Wrong pins fail before connector success.

Static-static X25519 DH (converted from Ed25519) feeds HKDF-SHA-256 with the
transcript hash as salt and identity/direction-bound info. This provides fresh
session keys and **not forward secrecy**. Frames are `[0x21, seq:u64 BE,
ciphertext, GCM tag:16]`; nonce is `[0;4] || seq_be`, and AAD binds the domain,
transcript hash, direction, tag and sequence. Non-cloneable mutable channel
state requires the next receive sequence, commits it only after authenticated
parsing, and terminates on failure or counter exhaustion.

A 5-second overall handshake deadline and 4096-byte handshake payload cap
apply on each side; WS message/frame allocation is additionally capped at
64 MiB for application traffic. Cancellation interrupts connector handshake
IO and retry backoff. Registration is atomic after encrypted readiness;
generation-scoped disconnect cannot evict a newer connection. Federation
failure throttling is independent of browser authentication. Primary sealed
pings run every 15 s; only authenticated frames refresh the 45 s liveness clock.

Once joined, exactly three flows run over the one WS connection
(`NodeWsMessage`, `src/protocol.rs`):

1. **Event relay (push).** The secondary forwards its `SessionEvent`
   broadcast; the primary tags each event with the node name
   (`for_delivery(node)`) and re-broadcasts it, so web, SSE, and
   `oly list` show remote sessions with a `node` field. Notifications
   are relayed with a `[node]` title prefix.
2. **Proxied RPCs (one-shot).** Commands carry an optional node
   (`oly logs --node`, web `?node=`); `NodeRegistry::proxy_rpc` sends
   `Rpc{id,request}` and awaits `RpcResponse{id}`. The method set is
   allowlisted, and every proxy carries a deadline (30 s default;
   `LogsWait` gets its own timeout + margin) so a hung secondary can
   never stall a gateway caller.
3. **Relayed attach (streaming, M5-2).** This is the interesting one:

```
browser / CLI ──WS──► primary ws.rs ── NodeRegistry::proxy_rpc_stream
                        │  Rpc{id, AttachSubscribe}
                        ▼
                  secondary connector ── opens a NESTED LOCAL IPC
                                         connection to its OWN daemon
                                           │  full local attach runs:
                                           │  journal → AttachPump →
                                           │  role gate → credits
                                           ▼
   RpcStreamFrame{id, chunk, done} ◄───────┤  binary attach frames ride
   RpcStreamMessage{id, input|resize|      │  back as stream frames;
   applied-cursor|detach} ─►┤  client messages are
                                            │  forwarded verbatim
```

   Because the secondary serves the relay through its own local IPC,
   the *authoritative* implementation — pump cursors, the role gate,
   credit enforcement, gapless resume — runs unchanged on the
   owning node. A federated attach is a local attach plus one transport
   hop, which is why fencing and flow control are identical for remote
   clients. Mid-stream messages are allowlisted to attach-scoped ones;
   the relay is not a general RPC tunnel.

Federation rules to preserve:

- **Identity auth happens once, at join.** The exact registered name/key,
  fresh signed transcript, and mutual encrypted confirmation are all required
  even with HTTP `--no-auth`. No API-key or alternate credential branch remains.
- **The owning node is the only authority on its sessions.** The
  primary routes and displays; it never bypasses the secondary's lease
  or credit checks — everything goes through that node's own daemon.
- **Relay failure is loud.** Stream frames use bounded queues; a dead
  node ends proxied streams with an explicit error, and connectors
  reconnect with backoff rather than silently buffering.

Where to look: `src/node/registry.rs` (NodeRegistry, proxy_*),
`src/daemon/rpc_nodes.rs` (secondary connector, nested-IPC relay,
allowlists), `src/http/nodes.rs` (join endpoint, keepalive, event
tagging), `src/federation_crypto.rs` (channel-key derivation + signature
helpers), `src/client/join.rs` (join config + CLI).

## The invariants that matter

When you change session, streaming, or storage code, these are the rules the
test suite enforces (the full list with rationale is captured in the
invariant descriptions throughout this document):

- **Ordered, durable recording.** Every byte the PTY emits reaches the
  journal exactly once, in order; durability is asserted via sync
  acknowledgments, never assumed from timing.
- **Gapless streams.** Attach cursors are `(incarnation, offset)` pairs;
  resume continues exactly, cross-incarnation cursors are fenced, and
  gaps/duplicates are protocol errors — for local, browser, and relayed
  clients alike.
- **Roles fixed at attach.** Input is gated by attachment role: an
  observe-mode attach cannot inject; every other attach is a
  controller, and controllers drive concurrently (last successful
  resize wins geometry).
- **Bounded memory.** Slow clients backpressure via stream credits and
  bounded queues; a stuck client is resynced or dropped, never buffered
  forever.
- **Explicit degradation.** Missing, corrupt, or pre-0.5 state produces
  clear errors (see `MIGRATION.md`), never silently empty output.

## Reading the code

Start from the session runtime and follow a byte: PTY read →
`runtime.rs` sequencing → journal append + broadcast → attach pump →
`ipc.rs`/`ws.rs` frames → client. The reverse direction (input) goes through
the attachment registry's lease check into the PTY writer. For a federated
session, the same walk continues one hop further: the secondary's connector
(`daemon/rpc_nodes.rs`) serves the relay through its own local IPC, so there
is no second streaming implementation to learn.

Tests are the executable specification: unit tests live next to the code,
protocol conformance in `src/daemon/rpc.rs` (`ipc_conformance`) and
`tests/fixtures/`, end-to-end coverage in `tests/e2e_*.rs`.

## Frontend update pipeline (`/api/sessions/events`)

See PERFORMANCE.md §Update pipeline for the sampling and recovery budget.
The SSE endpoint keeps the view current rather than mirroring the database:

- **`stream_ready` opens each connection** with a version and connected node names,
  not a session inventory. Views reconcile their inventory through REST.
- **Structural changes use lifecycle events** (`session_created`, `session_updated`,
  `session_deleted`, notifications and `node_state`). Secondaries forward their
  local events; the primary applies the secondary name on delivery.
- **Activity uses compact `session_activity` batches**. Each owning daemon samples
  in-memory runtimes every 500 ms and emits changed counters or output timestamps.
  Sparklines consume counters directly without reloading the list.
- **Gaps require reconciliation**. `resync_required` identifies an affected node;
  a null scope means the shared stream may have lost events for any node. Clients
  preserve history, invalidate counter baselines and reconcile through REST.
  The first post-gap total establishes a baseline without plotting missing bytes.
- **Lagged SSE receivers get a resync marker and EOF**. Persistent clients reconnect
  with backoff and receive a fresh `stream_ready`. Missing events never imply a
  session deletion.

## Session listing and search

`GET /api/sessions` (and the `oly ls` / TUI baseline query) share one filter
implementation — `Database::push_list_filters` in `src/db.rs` — so the web
list, the CLI's server-side query, and the TUI's follow-mode baseline agree on
what "matches". The web search box and `oly ls --search` send the same
`ListQuery.search` string.

- The query is whitespace-split into ANDed terms.
- A **bare** term matches the union of `id`, `title`, `tags`, `command`,
  `args`, and `cwd`, so a session is findable by the command it runs *and* the
  directory it runs in. (The web placeholder surfaces `cmd`/`cwd`.)
- A `field:value` term scopes to one column: `id`, `title`, `tag`/`tags`,
  `cmd`/`command` (command **or** args, matching the rendered command cell),
  `args`, and `cwd`. Prefixes are case-insensitive; an unknown prefix — and a
  Windows drive path such as `C:\src` — stays a bare term so a colon never
  surprises the user. A scoped term with an empty value (`cmd:`) is ignored so
  typing a prefix mid-edit never blanks the list.
- Remote lists proxy the same filter to the owning node; the primary only adds
  the `node` tag on the way back (`tag_sessions_with_node`).

The CLI list view keeps a parallel client-side index
(`session_search_text`, `src/client/list_tui/app.rs`) over the same field set,
so follow-mode filtering stays in step with the server-side query.

## Documents

- `README.md` — what oly is and how to use it.
- `MIGRATION.md` — 0.3.x → 0.5.x breaking changes and upgrade steps.
- `ARCHITECTURE.md` — the architecture decision records (engine, journal,
  input, streaming, history UX, crash boundary, authz), consolidated here.
- `PERFORMANCE.md` — critical data-path tracking with before/after numbers.
- `CONTRIBUTING.md` — development workflow.
