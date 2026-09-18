# ADR-0004: Stream framing, negotiation, credits, federation scheduling

- Status: Proposed
- Plan reference: PLAN.md §7, §10.1 (invariants I2, I7)

## Context

IPC is newline-delimited JSON with base64 bytes; browser WS output is
binary; federation re-encodes at hops. Snapshot, file offset, and broadcast
subscription are separate steps, so fresh attach can overlap or gap events.
Queues are bounded in messages or not at all (CLI frame channel, browser
arrays), and the shared node receive loop awaits per-stream sends, letting
one stalled stream block unrelated heartbeats.

## Decision (draft)

1. One versioned major/minor stream protocol with feature negotiation,
   shared by IPC, browser WS, and federation. JSON stays for low-volume
   control; PTY data is binary length-delimited with limits checked before
   allocation.
2. Snapshot boundary C and subscription at C+1 are one ordered operation.
   Snapshots chunk (`SnapshotBegin/Part/End`); events carry source cursor
   ranges; clients report `Applied` cursors that drive byte credits.
3. All queues are byte-bounded (starting budgets in PLAN §7.4). Slow
   observers resync from the journal or disconnect; they never throttle the
   controller. Control/cancel/heartbeat frames get priority with fairness.
4. Federation is a thin transport adapter over the same attach state
   machine: origin cursors preserved end-to-end, per-stream credits, fair
   scheduling, deadlines, explicit cancel, connection-generation fencing.
   No secondary-to-self IPC relay per stream.

## Rejected alternatives

- JSON/base64 for hot-path data (CPU and size overhead at every hop).
- Per-stream synchronous send loops (head-of-line blocking).
- A new network transport (QUIC) before measurements demand it.

## Acceptance

- Property tests: arbitrary fragmentation/coalescing/duplication never
  violates the C/C+1 boundary or applies events twice.
- A stalled stream does not delay an unrelated stream's heartbeats.
- Golden vectors generated from the Rust schema pass in TypeScript.

## Migration

Protocol major bump; CLI/daemon/node mismatches fail with precise upgrade
errors. No permanent 0.x bridge.

## Addendum (M5-1): credits are enforced, queues are byte-bounded

Applied-cursor credits stopped being advisory signals and became gates:

1. **Credit gate in the pump.** Each credited attachment carries a shared
   applied-cursor cell (registry) that its `AttachPump` reads. The pump may
   run at most 4 MiB (4x the 1 MiB ack stride, so one ack round-trip never
   gates a healthy client) plus one frame ahead of the applied cursor;
   beyond that it waits, and a client that never catches up is ended loudly
   (`Closed`) after 30 s — never buffered without bound. The gate sits at
   the top of `next()`, before any chunk is consumed or offset advanced,
   keeping `next()` cancel-safe. The cell initializes at the INIT boundary
   (the client is deemed to have applied everything up to `end_offset`),
   so only post-attach bytes count as in-flight.
2. **Uncredited exception, fail-open.** Node-relayed subscriptions cannot
   receive mid-stream credits (one request envelope per stream), so the
   gateway rewrites `AttachSubscribe.credited = false` before relaying and
   those pumps run ungated — the pre-M5-1 behavior — until direct remote
   attachment streams (M5-2). The serde default is `false` for the same
   reason: an absent flag never silently enables gating.
3. **Client queues bounded.** The CLI attach loop's daemon-frame channel
   (16 frames ≈ 11 MiB worst case) and terminal-event channel (4096
   events) are bounded; the frame reader backpressures the socket and the
   input thread blocks — input is never dropped to relieve pressure.

Known remaining gap (M5-2): relayed streams and their ack forwarding; the
broadcast ring is message-bounded (256 chunks), not byte-bounded.
