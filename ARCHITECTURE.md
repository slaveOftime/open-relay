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
   session. Everything else (the filtered display stream, resize history,
   logs, attach snapshots) is *derived* from the journal at read time
   (`src/session/replay.rs`), so derived state can never silently disagree
   with the recording. Replay is checkpoint-anchored: anchored v2
   checkpoints carry their filtered-stream offset, so deriving a window
   costs O(checkpoint cadence), never O(total recording) (PLAN §5.3).
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
| Attachments | `src/session/store/attach.rs` | Registry, control lease (one controller, many observers), fenced handoff, geometry authority. |
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
  { input | resize | acquire-control | applied-cursor | detach }
              │
  attachment registry (store/attach.rs): control-lease check —
  observers are rejected, handoff is explicit and fenced
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

## Node federation

Any daemon can be a **primary** (it serves `GET /api/nodes/join`); any
daemon can attach to a primary as a **secondary** (`oly join start`,
config persisted in `joins.json`, connector task reconnects with
exponential backoff 1→60 s — `client/join.rs`, `daemon/rpc_nodes.rs`).

Join handshake (WS, binary JSON; `src/sshauth.rs`, `http/nodes.rs`):

```
secondary                                      primary
   │ WS /api/nodes/join                          │
   ├─ get_host_key ────────────────────────────► │
   │ ◄── host_key {pub, nonce, sig} ─────────────┤ sig = Ed25519("oly-host-challenge-v1"‖nonce)
   │  verify sig (live proof), TOFU-pin          │ — no credentials sent unless this verifies
   │  against known_hosts (mismatch = abort)     │
   ├─ join {name, auth} ───────────────────────► │ auth = api_key, or Ed25519 sig over
   │ ◄── joined | error ─────────────────────────┤ "oly-node-join-v1"‖name‖nonce‖pubkey
   │        (nonce is fresh per connection: no replay, no key substitution)
   │  keepalive: primary pings every 15 s; node dropped after 45 s silent
```

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
                                           │  control lease → credits
                                           ▼
   RpcStreamFrame{id, chunk, done} ◄───────┤  binary attach frames ride
   RpcStreamMessage{id, input|resize|      │  back as stream frames;
   acquire-control|applied-cursor|detach} ─►┤  client messages are
                                            │  forwarded verbatim
```

   Because the secondary serves the relay through its own local IPC,
   the *authoritative* implementation — pump cursors, one-controller
   lease, credit enforcement, gapless resume — runs unchanged on the
   owning node. A federated attach is a local attach plus one transport
   hop, which is why fencing and flow control are identical for remote
   clients. Mid-stream messages are allowlisted to attach-scoped ones;
   the relay is not a general RPC tunnel.

Federation rules to preserve:

- **Auth happens once, at join.** API keys are stored `0600` on the
  secondary; SSH auth proves the primary live (host-key signature per
  connection) and binds the secondary's signature to name + fresh nonce
  + key. Join attempts are rate-limited and IP-locked on the primary.
- **The owning node is the only authority on its sessions.** The
  primary routes and displays; it never bypasses the secondary's lease
  or credit checks — everything goes through that node's own daemon.
- **Relay failure is loud.** Stream frames use bounded queues; a dead
  node ends proxied streams with an explicit error, and connectors
  reconnect with backoff rather than silently buffering.

Where to look: `src/node/registry.rs` (NodeRegistry, proxy_*),
`src/daemon/rpc_nodes.rs` (secondary connector, nested-IPC relay,
allowlists), `src/http/nodes.rs` (join endpoint, keepalive, event
tagging), `src/sshauth.rs` (challenge-response),
`src/client/join.rs` (join config + CLI).

## The invariants that matter

When you change session, streaming, or storage code, these are the rules the
test suite enforces (the full list with rationale is in `PLAN.md` §4):

- **Ordered, durable recording.** Every byte the PTY emits reaches the
  journal exactly once, in order; durability is asserted via sync
  acknowledgments, never assumed from timing.
- **Gapless streams.** Attach cursors are `(incarnation, offset)` pairs;
  resume continues exactly, cross-incarnation cursors are fenced, and
  gaps/duplicates are protocol errors — for local, browser, and relayed
  clients alike.
- **One controller.** Input is gated by a control lease; observers cannot
  inject; handoff is explicit and fenced.
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

## Documents

- `README.md` — what oly is and how to use it.
- `MIGRATION.md` — 0.3.x → 0.5.0 breaking changes and upgrade steps.
- `SPEC.md` — the product surface (commands, behaviors) as implemented.
- `ARCHITECTURE.md` — the architecture decision records (engine, journal,
  input, streaming, history UX, crash boundary, authz), consolidated here.
- `PLAN.md` — the design plan behind 0.5.0: invariants, architecture rules,
  and the release checklist the code and tests cite.
- `docs/` — the security audit report and the 0.5.0 release evidence.
