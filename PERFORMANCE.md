# Performance

Tracking document for the critical data paths (see ARCHITECTURE.md for the
shapes). One rule: **measure, then change, then measure again.** Every
optimization merged here must carry a before/after row.

## How to measure

Two always-available instruments, no rebuild or restart needed:

- **`GET /api/metrics`** — Prometheus text exposition of the daemon's
  in-process registry (`src/metrics.rs`). Behind the normal auth layer.
  `curl -s http://127.0.0.1:15443/api/metrics | grep -v '^#'`
- **`OLY_TIMING=1`** — prints wall-clock marks (relative to process start)
  on stderr for any `oly` command:

  ```text
  $ OLY_TIMING=1 oly ls
  oly-timing: startup 0.0ms
  oly-timing: ipc: connecting 0.3ms
  oly-timing: ipc: connected (list) 0.3ms
  oly-timing: ipc: response (list) 1.9ms      <- daemon-side handling
  oly-timing: list: data ready 1.9ms
  ```

  The gap between `connected` and `response` is pure daemon time; the gap
  before it is CLI startup + socket connect.

Quick benchmark recipe (isolated daemon, never touch your real state dir):

```bash
export OLY_STATE_DIR=/tmp/oly-bench OLY_SOCKET_NAME=oly-bench-sock
rm -rf $OLY_STATE_DIR && mkdir -p $OLY_STATE_DIR
printf '{"http_port": 15999}' > $OLY_STATE_DIR/config.json
oly daemon start --detach --no-auth-without-ask
oly start --detach --title flood -- sh -c \
  'i=0; while [ $i -lt 400000 ]; do i=$((i+1)); echo "line $i with padding"; done; sleep 3600'
# ...measure (oly ls, curl :15999/api/sessions, /api/metrics)...
oly daemon stop; rm -rf $OLY_STATE_DIR
```

## Instrumented paths

All histograms expose `oly_<name>_seconds_{bucket,sum,count,max}`; counters
expose `oly_<name>_total`. `label=` values come from code, listed below.

| Metric | Where | What it tells you |
|---|---|---|
| `ipc_request_seconds{label=<rpc>}` | daemon IPC dispatch | handling time per `RpcRequest` (non-streaming); this is what `OLY_TIMING` brackets |
| `http_request_seconds{label=<route>}` | axum `/api/*` middleware | full handled time per route template; SSE/NDJSON skipped on purpose |
| `attach_snapshot_seconds` | `AttachPump::subscribe` | journal snapshot + engine state build; shared by IPC, WebSocket and relayed attaches |
| `attach_init_seconds{label=local\|proxied}` | WS handler, init frame sent | browser-side time-to-first-byte; `proxied − attach_snapshot` ≈ federation relay cost |
| `attach_clients_total{label=local\|proxied}` | WS handler | attach mix across transports |
| `attach_resyncs_total` | pump | one per client-lag/gap episode (broadcast ring overflow) |
| `attach_resync_bytes_total` | pump | journal bytes replayed to close gaps; grows ⇒ clients can't keep up |
| `attach_credit_closes_total` | pump credit gate | clients that stalled and were dropped |
| `journal_append_seconds` | appender thread | per-record write into the journal |
| `journal_sync_seconds` | appender thread | fsync latency — the first number to check when *everything* is slow |
| `journal_bytes_total` | appender thread | durable output volume |
| `journal_failures_total{label=append\|sync}` | appender thread | any I/O failure that killed a session's journal |

## Baseline: 2026-09-21

Linux 6.18 WSL2, x86_64, Intel Core Ultra 7 265H, release build,
`OLY_STATE_DIR` on `/tmp` (tmpfs — `journal_sync` numbers will look
absurdly good; on real disks fsync dominates `journal_sync_seconds`).

Scenario: 5 sessions, of which one live session with a 21–28 MB journal
(`flood`), one 50 ms-periodic-output session (`chatty`), rest idle.

| Path | Before | After | Notes |
|---|---|---|---|
| `oly ls` List RPC (daemon-side) | **110–147 ms** | **p50 1.9 ms** | see Finding #1; scales with total journal bytes, not session count |
| `oly ls` wall clock (incl. process start) | ~150 ms | **p50 4.0 / p95 4.7 ms** | 20 runs |
| `GET /api/sessions` | same handler, same cost | **p50 2.8 / p95 12.2 ms** | web SessionsPage initial load |
| SSE connect snapshot (`/api/sessions/events`) | full-journal decode per connection | shares the fixed list path | every browser tab used to pay it |
| Repeated `oly ls` with a 21 MB *offline* session | 16 ms each | decode once, then cached | `persisted_filtered_len` incarnation cache |
| journal append | 13.3 µs avg (9.4k records during 28 MB flood) | — | writer path is not the bottleneck |
| journal fsync | 5.6 µs avg (tmpfs) | — | treat as lower bound only |
| attach snapshot (idle session) | 36–44 µs | — | `attach_snapshot_seconds` |

## Findings & fixes

### 1. `oly ls` / SessionsPage "mysterious slowness" (2026-09-21) — fixed

**Symptom.** `oly ls`, `oly ls -f` and the web SessionsPage felt slow in
proportion to how much *output* existed, not how many sessions there
were. Quiet periods felt fine — which is why it seemed to fix itself.

**Root cause.** `Database::list_summaries` computed the OUTPUT column via
`session_output_offset` → `replay::filtered_stream_len`, a **full VT
decode of the session's whole journal, per row, per list request**
(~1.3 GB/s ⇒ ~5 ms per 10 MB — linear in journal size). Two aggravators:

- For sessions with a live handle the computed value was **discarded**
  (immediately overwritten by `to_summary()`, whose byte counters are
  O(1) runtime state).
- The SSE handler ran the same list once per browser connection, and
  `oly ls -f` re-ran it every refresh.

**Fix.** `SessionStore::list_summaries` now asks SQL for rows *without*
offsets (`list_summaries_without_offsets`), fills live rows from the
runtime handle, and fills only offline rows via
`persisted_filtered_len` — O(1) live, **decoded once per incarnation and
cached** offline. `GET /api/sessions/{id}` switched to the same
`attach_filtered_len` helper. Direct `db.list_summaries` (with offsets)
remains only for the daemon-down CLI fallback, where there is no live
state to consult.

**Numbers.** 110–147 ms → p50 1.9 ms daemon-side (table above).

## Backlog (measure before optimizing)

- **Daemon-down fallback list** (`client/list.rs` → `db.list_summaries`)
  still decodes every journal inline; the rare path is O(total output).
  Same lazy-offset pattern would fix it.
- **Huge-scrollback attach**: `attach_snapshot_seconds` was measured on an
  idle session; a session with hundreds of MB of journal replays the
  window in 8 MiB slices — watch `attach_resyncs_total` and
  `attach_init_seconds{local}` on a `vim` + big-log session.
- **Federation relay overhead**: `attach_init_seconds{proxied}` minus
  owning node's `attach_snapshot_seconds` is the per-attach relay cost;
  no direct RPC-level relay histogram exists yet (would go in
  `relay_streaming_rpc` / `proxy_rpc_stream`).
- **journal_sync on real disks**: baseline above is tmpfs; rerun the
  recipe with `OLY_STATE_DIR` on the actual data volume before trusting
  append-latency claims.
- **Process start**: `oly ls` wall p50 is 4 ms, of which ~2 ms is binary
  startup + config + socket connect; only worth pursuing if attach or
  list ever regresses back toward it.
