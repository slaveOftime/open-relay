# oly architecture

`oly` runs interactive terminal programs under a daemon so they survive
closed terminals, can be observed and driven from several places at once,
and leave a truthful, durable recording.

This file is the 10-minute orientation. **The code is the source of truth**;
this document only explains the shape so you know where to look. Design
decisions and their reasons live in [`docs/adrs/`](docs/adrs/) — read those
before changing anything structural.

## The three rules

Everything in 1.0 follows from keeping exactly one implementation of each
core concern:

1. **One journal.** Every session persists a single ordered stream — raw PTY
   output, resizes, lifecycle transitions, checkpoints — in a per-session
   shadow journal (`src/session/journal.rs`). Nothing else is persisted per
   session. Everything else (the filtered display stream, resize history,
   logs, attach snapshots) is *derived* from the journal at read time
   (`src/session/replay.rs`), so derived state can never silently disagree
   with the recording. The journal is always on; if it fails, the session
   fails loudly rather than recording nothing (ADR-0002, ADR-0006).
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
| Journal | `src/session/journal.rs` | Segmented, incarnation-fenced append-only log with durability cursors and checkpoint-gated retention. `oly doctor` inspects it. |
| Replay | `src/session/replay.rs`, `src/session/logs/` | Derives the filtered display stream, resize history, and rendered logs from the journal. |
| Attach pump | `src/session/store/pump.rs` | The single state machine that serves every attach: snapshot from the journal, then gapless live chunks with enforced credits and bounded per-client queues. |
| Attachments | `src/session/store/attach.rs` | Registry, control lease (one controller, many observers), fenced handoff, geometry authority. |
| Web UI | `web/` | React + xterm.js; consumes the same binary frame protocol over WebSocket. |
| Federation | `src/node/`, `src/daemon/rpc_nodes.rs` | A primary supervises sessions on secondary nodes; remote attach relays the same stream with fencing, deadlines, and keepalives. |

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
- **Explicit degradation.** Missing, corrupt, or pre-1.0 state produces
  clear errors (see `MIGRATION.md`), never silently empty output.

## Reading the code

Start from the session runtime and follow a byte: PTY read →
`runtime.rs` sequencing → journal append + broadcast → attach pump →
`ipc.rs`/`ws.rs` frames → client. The reverse direction (input) goes through
the attachment registry's lease check into the PTY writer.

Tests are the executable specification: unit tests live next to the code,
protocol conformance in `src/daemon/rpc.rs` (`ipc_conformance`) and
`tests/fixtures/`, end-to-end coverage in `tests/e2e_*.rs`.

## Documents

- `README.md` — what oly is and how to use it.
- `MIGRATION.md` — 0.x → 1.0 breaking changes and upgrade steps.
- `SPEC.md` — the product surface (commands, behaviors) as implemented.
- `docs/adrs/` — the seven architecture decision records (engine, journal,
  input, streaming, history UX, crash boundary, authz).
- `PLAN.md` — the 1.0 milestone plan and per-slice status log.
- `docs/` — evidence reports (engine evaluation, security audit, M0
  baselines) and release evidence.
