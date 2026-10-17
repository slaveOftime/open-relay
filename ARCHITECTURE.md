# oly architecture

`oly` runs interactive terminal programs under a daemon so they survive
closed terminals, can be observed and driven from several places at once,
and leave an ordered recording for inspection and replay. A client disconnect
preserves the child; a daemon stop/crash or host reboot does not recover live PTYs.

This is the maintainer orientation for the 0.5 design. **The code is the source
of truth**; this document maps the implementation and explains its boundaries.
Historical ADR/invariant IDs in source refer to the consolidated decisions here,
not separate ADR files. Read this before changing anything structural.

## The three rules

Everything in the current design follows from keeping exactly one implementation of each
core concern:

1. **One journal.** Every session persists a single ordered stream — raw PTY
   output, resizes, lifecycle transitions, checkpoints — in a per-session
   journal (`src/session/journal/`). It is the only persisted terminal
   recording; uploaded files can also live in the session directory.
   Shared SQLite stores launch/lifecycle metadata and may cache *derived hints* (for
   example, a `resume_command` discovered from the rendered journal tail
   after both child exit and PTY EOF using configurable `resume_patterns`);
   it is not another recording.
   The filtered display stream and historical reads are derived from the
   journal (`src/session/replay.rs`, `src/session/logs/`); live screens and
   attach snapshots use the engine maintained by the same sequencing path.
   Anchored v2 checkpoints carry filtered offsets, reducing prefix replay
   when a suitable anchor exists. Missing/unusable anchors fall back to a
   bounded-batch full-prefix scan; anchor discovery itself scans record
   headers. Cold rendered tail/frame reconstruction can still be linear
   in recording size. Do not promise constant-cost historical reads.
   The journal is always on; if it fails, the session
   fails loudly rather than recording nothing (ADR-0002, ADR-0006).
   Sessions left over from pre-0.5 builds (only `output.log`) are rejected
   with an explicit error (see docs/releases/migration-0.5.md).
2. **One authoritative terminal model.** The daemon's live screen, `oly logs`
   rendering, snapshots, and Windows repaint use the embedded alacritty
   engine (`src/terminal/`). Browsers render the canonical stream with
   xterm.js; they are clients, not another backend source of recording truth.
   vt100 survives only as a test-only conformance oracle (ADR-0001).
3. **One attach contract.** Output is binary end to end, with cursor/mode
   semantics shared by every transport (`src/ipc.rs`,
   `web/src/api/ws-frames.ts`). IPC and browser WS have transport-specific
   envelopes; high-volume output avoids base64-in-JSON. IPC control remains
   JSON, while browser control uses its WS frame layout (ADR-0004).

## Component map

| Component | Where | What it does |
|---|---|---|
| CLI | `src/main.rs`, `src/cli.rs`, `src/client/` | Parses commands, talks to the daemon over the local IPC socket. |
| Daemon | `src/daemon/` | Owns all sessions. Serves IPC (`rpc*.rs`), HTTP + WebSocket (`src/http/`), and node federation (`src/node/`). |
| Session runtime | `src/session/runtime.rs` | One per session: owns the PTY, feeds the engine, sequences every output chunk through a single point, journals it, broadcasts it. |
| Journal | `src/session/journal/` | Segmented, incarnation-fenced append-only log with durability cursors and checkpoint-gated retention. Integrity is verifiable through manifest validation. |
| Replay | `src/session/replay.rs`, `src/session/logs/` | Derives the filtered display stream, resize history, and rendered logs from the journal. |
| Attach pump | `src/session/store/pump.rs` | Shared streaming state machine: fresh live-engine snapshot or journal-backed offset resume, then cursor-checked chunks and bounded recovery/credits. |
| Attachments | `src/session/store/attach.rs` | Registry with roles fixed at registration (controller, or view-only observer); geometry authority gated by role. |
| Web UI | `web/` | React + xterm.js; consumes the shared attach contract in browser-specific binary WS frames. |
| Federation | `src/node/`, `src/daemon/rpc_nodes.rs` | A primary supervises sessions on secondary nodes; remote attach relays the same stream with fencing, deadlines, and keepalives. See “Node federation”. |

## Following a byte: the key data flows

Output (one producer, fan-out at the sequencing point):

```
        child process (PTY)
              │ PTY read (reader thread)
        session/runtime.rs — single sequencing point:
        raw bytes → journal; filtered bytes → SequencedChunk(offset, bytes)
        attach subscription/cursors carry the incarnation fence
              │
      ┌───────┼────────────────────┐
      ▼       ▼                    ▼
  journal   broadcast ring   terminal engine
  (append + (live attach,    (live screen, ModeSnapshot:
  sync ack) bounded lag)     DECCKM/paste/mouse/focus)
      │       ▲
      └───────┘ resync & completion tail read from the journal,
                never from memory (I2: gaps are errors, not skips)
              │
        AttachPump (store/pump.rs) — THE one streaming state machine:
        live snapshot / journal resume → cursor checks → recovery → credits
        emits AttachEvent { Chunk | Modes | Done | Closed }
              │
   ┌──────────┼──────────────────┐
   ▼          ▼                  ▼
 IPC binary  WS binary frames   node relay (RpcStreamFrame)
 frames     (browser xterm.js)  (federated attach via primary)
 (CLI)      ws.rs + ws-frames.ts
```

Interactive attach input (one role gate):

```
CLI keys / xterm onData ──► AttachClientMessage
  { input | resize | applied-cursor | detach }
              │
  attachment registry (store/attach.rs): role gate —
  observe-mode attachments are rejected; controllers drive directly
              │
  PTY writer (runtime.rs) ──► child stdin
```

Operator `oly send` uses the same PTY input path without an attachment role;
observe mode is not an authorization boundary or an exclusive controller lease.

Design points worth knowing before touching this code:

- **Flow control is cursor-based, not queue-based.** Clients ack an
  `applied-cursor`; the pump enforces stream credits and bounded queues,
  so a stuck client is resynced (from the journal, in bounded windows)
  or dropped — never buffered forever (I7).
- **Modes are engine state, mirrored per transport.** The engine tracks
  DECCKM, bracketed paste, mouse reporting (1000/1002/1003 collapsed to
  one flag, plus SGR 1006), and focus reporting (1004) in a
  `ModeSnapshot`. Fresh screen snapshots are renderer restore streams,
  not historical DECSET replay (they can explicitly enter the alternate screen).
  Each transport mirrors authoritative input-mode flags — the CLI drives its own
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
- **Stop keys precede signal escalation.** The shared stop path (CLI/TUI,
  HTTP/RPC/federation, daemon shutdown) queues Ctrl-C, repeated Ctrl-C at
  `min(500ms, grace/4)`, then Ctrl-D at `min(1s, grace/2)`. These are application
  input, not a PTY close or portable signals. Unix process-group SIGTERM waits
  until `3*grace/4`; tree kill remains at the deadline. Full input queues retry
  without blocking runtime locks, shift later keys to preserve enqueue spacing,
  and stop retries at the signal boundary without extending grace. Zero grace
  and kill bypass keys. Child completion still does not imply output EOF;
  resume derivation keeps its EOF and journal-durability gates.
- **ConPTY close is not an output cutoff.** Child exit and a quiet reader do
  not prove the output pipe is empty. Windows defers closing the master with a
  bounded drain wait between processed chunks, fences its activity counter only
  when no read is in flight, and continues recording every successful read through EOF. Buffered child output can arrive alongside console
  mode-reset bytes after close starts; timing is not a safe reason to discard it.
  Attach `Done` and byte-observation stream liveness wait for output EOF, not
  just the child's terminal lifecycle status. Only then is the final cursor fixed.
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
`node_identities`, and `--id-pub-key` pins the primary on the secondary.
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
   `oly ls` show remote sessions with a `node` field. Notifications
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
  primary routes and displays; it never bypasses the secondary's role
  or credit checks — everything goes through that node's own daemon.
- **Relay failure is loud.** Stream frames use bounded queues; a dead
  node ends proxied streams with an explicit error, and connectors
  reconnect with backoff rather than silently buffering. A lagged secondary
  connector sends its `resync_required` marker directly to its primary,
  never back into the source event ring: otherwise multiple lagged joins
  can keep each other lagging and flood logs after a stalled send or sleep.

Where to look: `src/node/registry.rs` (NodeRegistry, proxy_*),
`src/daemon/rpc_nodes.rs` (secondary connector, nested-IPC relay,
allowlists), `src/http/nodes.rs` (join endpoint, keepalive, event
tagging), `src/federation_crypto.rs` (channel-key derivation + signature
helpers), `src/client/join.rs` (join config + CLI).

## Windows IPC and sleep/resume failures

Local IPC uses `interprocess` → Tokio → Mio Windows named pipes; federation
uses a separate TCP/WebSocket connection. Network loss during sleep/resume is
an expected federation disconnect and is retried with backoff. It must not
terminate the daemon or local sessions. Relayed attaches also use nested local
IPC, so a remote disconnect can coincide with local pipe teardown.

Windows builds require Mio **1.2.4 or newer** (enforced in `Cargo.toml`). Earlier
versions can mishandle an overlapped operation that fails just after submission:
a completion packet still arrives, but its pending state/reference was not
retained. This can cause a panic in `write_done` (`State::None => unreachable!()`),
mutex poisoning in subsequent deregister/drop calls, or use-after-free
(upstream `tokio-rs/mio#2014`). Catching a client-handler panic is not a repair
for corrupted IOCP state; use the upstream fix.

`src/daemon/crash.rs` records Rust panics in `logs/daemon-crash.log` and detached
stderr in `logs/daemon-stderr.log`. Preserve the first panic's location: later
`PoisonError` panics can be secondary failures. Release backtraces may lack
symbols; a debug build gives better attribution. Actual laptop suspend/resume
still needs a manual soak test; the Windows IPC regression in
`tests/e2e_daemon.rs` exercises peer disconnects during queued writes on real
named pipes without suspending the host.

## The invariants that matter

When you change session, streaming, or storage code, these are the rules the
test suite enforces (the full list with rationale is captured in the
invariant descriptions throughout this document):

- **Ordered, durable recording.** Every byte the PTY emits reaches the
  journal exactly once, in order; durability is asserted via sync
  acknowledgments, never assumed from timing.
- **Backpressure before degradation.** The ingest path waits for journal
  queue room while the appender is momentarily behind — an `fsync`, a
  competing session — and degrades the session only when no slot frees
  within the stall deadline. The queue's byte budget stays the hard
  memory bound, so a slow disk slows a session instead of failing it
  while a genuinely wedged journal still fails loudly.
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
  clear errors (see `docs/releases/migration-0.5.md`), never silently empty output.

## Reading the code

Start from the session runtime and follow a byte: PTY read →
`runtime.rs` sequencing → journal append + broadcast → attach pump →
`ipc.rs`/`ws.rs` frames → client. The reverse direction (input) goes through
the attachment registry's role check into the PTY writer. For a federated
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

## Configuration and attention hints

`src/config.rs` resolves **flat** JSON into internal substructures. CLI overrides
remain authoritative across reload; the daemon's inherited `OLY_WEB_PUSH_PROXY`
also outranks the file. `src/daemon/reload.rs` polls mtime every two seconds,
swaps `LiveConfig`, and applies channel/eviction/retention/resume/log side effects.
HTTP listener changes require restart. Live config availability does not mean
existing session engines are rebuilt: scrollback retention is captured at spawn.

`src/notification/mod.rs` samples silent candidates once a second. Candidates
use activity epochs and a rendered live-screen excerpt (`store/notify.rs`), not
a historical byte-pattern scan. Regex prompt hints can trigger after activity
suppression; silence remains a fallback even with `prompt_patterns: []`.
The LLM trigger function is currently a stub. None of this establishes task
completion or approval authority. Resume rules instead scan a completed,
durable journal tail and store a suggestion; they never execute it.

OSC 7501 reports are parsed by the same `session/scan.rs` sequencing path and
retained in `TerminalSignals` using `session/program_status.rs`. The scanner
answers support queries itself (same terminator, constant reply, before fallback
DA replies), strips the protocol from the canonical display stream, and does
not charge it as meaningful screen output. Raw reports remain in the one journal.
Up to 256 hierarchical replacement records obey scoped clear, app inheritance,
RIS reset, and OSC 133 A / PTY EOF transient expiry. The selected report is exposed
as optional `SessionSummary.program_status` over existing IPC/REST/SSE/federation;
process lifecycle remains separate. For running processes, idle/done/blocked/error
set input-needed immediately, independently of notification delivery; working
suppresses attention. Non-running processes never need input. The browser presents
live idle/done as readiness, with process lifecycle in the primary badge tooltip.
Retained reports override attention heuristics. Notification epochs advance only
when the selected report changes, not on background progress or screen redraws;
idle/blocked/done/error use the existing suppression/debounce. Idle notifies
readiness for another instruction, not completion. Text has bounded base64
and decoded UTF-8 sizes, rejects controls, and disarms direction/invisible
formatting. Reports are never replayed to client terminals as host state or used
as approval authority. They are live-derived hints, unavailable after runtime
eviction or daemon restart; cold reads do not reconstruct program status.

Windows ConPTY can consume a primary DA query and answer the child before the
owning daemon receives/answers the preceding OSC 7501 query. Pi uses that DA reply
as its negotiation sentinel, so it can disable status output on Windows even
though unsolicited reports pass through ConPTY and the scanner. Windows child
environments default pi's documented `PI_PROGRAM_STATUS=1` opt-in only when
absent, after environment refresh/merge; explicit values and case-insensitive
keys are preserved. No parser fallback, guessed status, or generic ConPTY
passthrough change is introduced. A native Windows Node fixture covers the
opt-in and observable state/list/prompt/export behavior; Unix keeps query/reply
coverage. The pi terminal library was separately reproduced with automatic
failure and successful opt-in on Windows build 26200.

## Version axes

Each contract below evolves on its own axis; a change on one is never
assumed to be compatible on another. Version mismatch is a loud failure,
not a best-effort fallback.

| Axis | Where the version lives | Policy |
|---|---|---|
| CLI JSON | documented schemas; `v` where the guide says so | additive fields; consumers ignore unknown ones |
| Private IPC | `{"version":N,"payload":...}` envelope (`PROTOCOL_VERSION`) | exact match; mismatch is an explicit protocol error |
| Browser WS | binary layout pinned by `tests/fixtures/ws_frames.json` | encoder and decoder conformance vectors must both pass |
| SSE | bounded view notifications; gaps trigger a REST resync | not a durable cursor or event log |
| Federation | signed handshake binds `FEDERATION_VERSION` | no mixed-version rollout; unsupported fails closed |
| Journal | `RECORD_VERSION` in each record header | new readers read old; unknown future versions are rejected, never treated as empty |
| SQLite | forward-only migrations | rollback by restoring a backup, never by an old binary |

Recorded decisions that constrain upcoming work (not shipped): semantic records
arrive as a new journal format version through the existing sequencer
(no in-place rewrite of old journals, no second recording); exclusive
control is per-session opt-in while shared control stays the default; a
public machine interface starts with a version/capability query and
refuses unsupported operations explicitly. See the compatibility policy
for the response rules.

## Deliberate trade-offs

- **Journal (ADR-0002/0006):** one ordered recording avoids divergent persisted
  streams; old ambiguous recordings are rejected rather than fabricated.
  Append/enqueue is not fsync: durable acknowledgements are the persistence boundary.
- **Terminal model (ADR-0001):** one backend engine keeps replay/rendering aligned;
  transport clients still render and mirror modes appropriate to their terminals.
- **Input (ADR-0003):** explicit paste boundaries and live modes replace timing
  guesses; platform-specific decoders preserve the same input contract.
- **Framing (ADR-0004):** binary output avoids base64 expansion on high-volume
  attach paths; transport-specific control carries the same mode/cursor contract.
  Byte cursors enforce continuity.
- **History UX:** recorded bytes, rendered views, and application-owned scrolling
  are different surfaces. Views are lossy observations, not complete transcripts.
- **Retention:** older-incarnation deletion primitives remain internal, but the
  live byte-cap configuration was removed: ordinary sessions stay within one
  incarnation, making that policy ineffective for bounded live storage. No
  production setting enables live journal deletion. The hourly age sweeper can
  permanently delete stopped sessions; it defaults to disabled. Use OS quotas
  for hard disk limits.
- **Trust:** the daemon executes as its OS user, without a sandbox. HTTP login
  protects operator access; attach roles only coordinate input/resize. Custom apps,
  proxies, and wwwroot overrides require `trusted_apps_enabled` (default false).
  They are reviewed operator code sharing the control-plane origin, not isolated
  agent reports. App routes permit inline scripts; normal UI routes do not.
  Proxy targets are loopback IP literals only, and joined paths cannot change
  origin. Security middleware covers fallback responses as well as named routes.
  Federation uses pinned identities and sealed traffic but has no forward secrecy
  and no primary-side identity removal in 0.5.x; the
  [compatibility policy](./docs/releases/compatibility-0.5.md#node-federation)
  states the trust model and its limits.

## Documents

- [README](./README.md) — positioning, install, and quick start.
- [CLI guide](./docs/guides/cli.md) — commands and log/input contracts.
- [Remote access](./docs/guides/remote-access.md) — browser, federation, notifications, and state.
- [Skills](./skills/oly/SKILL.md) — bundled agent instructions; configuration lives
  in the [daemon skill](./skills/oly-daemon/SKILL.md).
- [Migration to 0.5](./docs/releases/migration-0.5.md) — breaking changes and safe upgrade/rollback.
- [Performance](./PERFORMANCE.md) — data-path measurements and recovery budgets.
- [Contributing](./CONTRIBUTING.md) — development workflow.
- [Compatibility](./docs/releases/compatibility-0.5.md) — supported contracts, node
  federation limits, and private interfaces.
- [How to publish a release](./docs/maintainers/release-process.md) — maintainer steps and verification.
