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

## Real-disk capacity and reconnect baseline: 2026-10-16

The 2026-09-21 baseline ran on tmpfs, so its `journal_sync` numbers were a
lower bound. This is the real-disk counterpart, with the client fleet
reconnecting under sustained output instead of idling.

- **Platform:** Windows 11 Pro build 10.0.26300, x86_64, Intel Core i7-8700K
  @ 3.70 GHz, 31.7 GiB RAM, release build (fat LTO, `opt-level = "z"`).
- **Disk:** `C:` NTFS system volume (the host exposes Samsung 980 PRO 1TB /
  970 PRO 512GB NVMe drives; the state dir is on the boot drive, not a
  RAM-backed filesystem). Free before 31,735,304,192 B (29.56 GiB), after
  31,707,910,144 B (29.53 GiB); 900.7 GiB used.
- **Sessions:** 10 — `flood` (bulk writer, ~10k lines/s while live),
  `chatty` (1 line/s), 8 idle `cmd` shells.
- **Clients:** 2 concurrent `oly attach --observe` observers per phase.
- **History size:** 16,067,715 B journal on disk when `flood` was stopped
  (single segment; appender payload 6,739,319 B, ~2.4× framing overhead),
  produced by ~259k lines in ~25–30 s of live flooding.

Maintainer-reported measurements; raw run artifacts are not checked in.
Workload sketch (PowerShell; isolated daemon, never the real state dir).
This is not a complete automation script: the eight idle shells, observer
launch/reconnect phases, metric collection, and cleanup must be supplied
separately. The results below describe one run, not a capacity guarantee:

```powershell
$benchId = [guid]::NewGuid().ToString('N')
$env:OLY_STATE_DIR = "$env:TEMP\oly-bench-$benchId"; $env:OLY_SOCKET_NAME = "oly-bench-$benchId"
New-Item -ItemType Directory -Path $env:OLY_STATE_DIR | Out-Null
Set-Content "$env:OLY_STATE_DIR\config.json" '{"http_port": 15999}'
oly daemon start --detach --no-auth-without-ask
oly start --detach --title flood -- cmd /c '(for /L %i in (1,1,600000) do @echo line %i with padding) & ping -n 601 127.0.0.1 >nul'
oly start --detach --title chatty -- cmd /c '(for /L %k in (1,1,1000000) do @(echo chatty %k & ping -n 2 127.0.0.1 >nul))'
# then per phase: 2× `oly attach <id> --observe` in the background, a
# /api/metrics snapshot, and `OLY_TIMING=1 oly ls --limit 500`; finally
# `oly stop <flood>` and `oly logs <flood> --tail 40` / `--cursor`.
```

`oly ls` daemon-side is `ipc: response (list)` minus `ipc: connected
(list)` from `OLY_TIMING=1`; attach numbers come from
`attach_snapshot_seconds` snapshots and the observers' `attach:` marks.

| Observation | Value |
|---|---|
| list RPC, `flood` live (~10k lines/s) | 38–74 ms (5 runs) |
| list RPC, `flood` stopped | 23.6–25.3 ms (5 runs) |
| `oly ls` wall clock, live / stopped | 148–269 ms / 100–103 ms |
| `logs --tail 40` on the stopped 16 MB journal, wall / daemon `logs_read` | 457 ms / 257 ms |
| `logs --cursor`, wall / daemon | 76 ms / 23 ms |
| observer init on live flood (`attach: ipc connected` → `init frame received`) | 35 ms (73.2−38.1) |
| `attach_snapshot_seconds` | ~60–90 µs (5 attaches, incl. flood) |
| `attach_resyncs_total` / `attach_resync_bytes_total` | 322 / 4.15 MB (~13 KB per resync) |
| `attach_credit_closes_total` | 0 (counter never registered) |
| journal appends | 259,360 records, avg 15.0 µs, max 3.73 ms (99.8% ≤100 µs) |
| journal fsyncs | 615 calls, avg 7.1 ms, max 397 ms (1 ≤0.5 ms; 479 ≤5 ms; 606 ≤10 ms) |

Notes:

- **fsync is the real-disk story:** avg 7.1 ms vs 5.6 µs on tmpfs, with a
  397 ms tail spike. `journal_append` stays ~15 µs, so writer-side CPU is
  still not the bottleneck.
- **Live observers lag under a 10k-lines/s writer and self-heal by
  resync:** ~4.15 MB of journal replayed across 322 resyncs in ~30 s
  (~13 KB each — small ring gaps, not full re-reads). No client was
  credit-closed; `attach_credit_closes_total` never registered.
- **`oly ls` in this measured workload:** ~40–70 ms daemon-side while the
  flood runs vs ~24 ms after it stops, on 10 live sessions. The remaining
  gap is live-row work while a session writes ~0.6 MB/s of durable
  journal.
- The run consumed ~26 MiB of disk in total (16 MB journal + observer
  output + db/WAL). The script's final `BENCH-DONE`/`disk-after` lines were
  lost to a cleanup error after `flood` stopped (`Stop-Process` on an
  observer that had already exited); disk-after was measured immediately
  after with the state tree still present.

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

## Session list at scale (added 2026-10-16)

Windows 11, x86_64 dev host, release build (`opt-level = "z"`, fat LTO),
isolated `OLY_STATE_DIR` seeded with 5000 synthetic offline sessions and no
journals (worst case for the list path: every row is persisted-only).
Daemon numbers come from `OLY_TIMING=1 oly ls --limit 5000` and
`GET /api/metrics`; client numbers from the ignored probes in
`src/client/list_tui/perf_tests.rs`:

```text
cargo test --release --bin oly perf_ -- --ignored --nocapture
```

| Path | Before | After | Notes |
|---|---|---|---|
| `oly ls --limit 5000` list RPC (daemon-side) | **~790 ms** | **~205 ms** (4 runs: 204–240 ms) | `ipc: response (list)`; the redundant per-row `session_exists` query left the path |
| TUI refresh (`replace_sessions`, warm) | 75.6 ms | **29.7 ms** | quiet dashboard, list mode |
| TUI refresh (`apply_refresh`, warm) | 73.9 ms | **27.6 ms** | |
| TUI list frame (140×40) | 38.5 ms | **1.9 ms** | middle-of-list selection 36.8 → 2.0 ms |
| TUI filter keystroke | 24 ms | **9.0 ms** | each keystroke re-filters and rebuilds the tree |
| TUI tree frame (140×40) | 1.8 ms | **0.8 ms** | |
| Ctrl+G list↔tree toggle | 51.5 ms | **18.5 ms** | dominated by the tree rebuild |
| tree `build_tree_nodes` | 49.2 ms | **19.4 ms** | hottest remaining client phase; path compares no longer allocate per component |
| list-path `session_exists` × 5000 | 438 ms (88 µs/op) | not called | release probe; kept as evidence for the root cause |

### 2. `oly ls -f` with thousands of sessions (2026-10-16) — fixed

**Symptom.** `oly ls -f --node-all --limit 5000` felt sluggish with a few
thousand sessions: keystrokes lagged and the dashboard's refresh tick
overran, with the daemon's `list` RPC accounting for most of the time.

**Root causes.**

- **Daemon.** `SessionStore::list_summaries` filled each offline row's
  OUTPUT via `persisted_filtered_len`, which starts with a `session_exists`
  SQL round trip. The rows came *from* the sessions table, so the check was
  redundant at ~88 µs per row (~440 ms at 5000 rows).
- **Daemon.** `persisted_stream_len_cache` was capped at 1024 entries and
  cleared wholesale when full, so any list touching more than 1024 offline
  sessions re-decoded journals (or re-probed them) on every refresh.
- **Client.** Each 250 ms refresh rebuilt a lowercased search string for
  every session even with no filter and built attach bookkeeping even with
  nothing open; the tree sort re-lowercased labels inside the comparator,
  and every tree path comparison allocated normalized strings.
- **Client.** Every frame built a `Row` (with truncated cells and spans) for
  all 5000 sessions, looked up each row's rate through a freshly formatted
  key string, and rescanned every session for column maxima and aggregate
  throughput.

**Fix.**

- `list_summaries` uses `persisted_filtered_len_known` — same per-incarnation
  cache, no existence query — and the cache bound moved to
  `PERSISTED_STREAM_LEN_CACHE_ENTRIES = 20_000` so a large list stays cached
  between refreshes.
- The TUI keeps one parallel `keys: Vec<String>` per list and one lazily
  built `search_text` index (only while filtering), caches the layout's
  column maxima (invalidated when the session set changes), builds table
  rows only for the viewport slice with the selection expressed relative to
  it, and sums rate values directly for the throughput header.
- Tree path comparisons (`path_eq_ci`, `path_starts_with_ci`, …) compare OS
  strings case-insensitively without allocating on the ASCII fast path
  (falling back to the original normalize + Unicode lowercase semantics),
  each session's tree path is computed once per rebuild, and tree sort
  labels are lowercased once instead of once per comparison.

**Numbers.** See the table above: the list RPC is ~4× faster and a
list-mode frame ~20× faster on the same 5000-session set. The probes and
the seeding recipe stay available for reruns.

## Update pipeline (added 2026-10-02)

Activity is produced by **one** `ActivitySampler` per daemon, ticked at
500 ms. The sampler owns the only path that emits `SessionEvent::SessionActivity`;
the per-output-reader fast publish path was retired at the same time, so a busy
session no longer races the sampler through `SessionUpdated`. Concrete shape:

- **In-memory only**: the sampler walks `SessionStore::sessions` under
  `try_read`, skipping any runtime a PTY reader is currently writing to. No
  SQL, no journal decode, no terminal engine snapshot.
- **Batched**: only sessions whose `last_total_bytes` advanced are
  included, so an idle daemon publishes zero frames between bursts of
  output. The frontend renders sparse sparkline buckets exactly once.
- **Federated**: secondary nodes forward the batch through the existing
  node channel; the primary rebroadcasts it through its SSE encoder.
- **Lag safety**: relay-side `Lagged(n)` surfaces a
  `ResyncRequired { node, reason: "relay_lagged" }` event into the same
  broadcast so any active SSE listener knows to refetch REST. SSE clients
  that lag the bounded ring close the stream; the broker auto-reconnects
  and the next `stream_ready` carries a fresh node list.

Measurement target (not yet benchmarked): one primary + up to five
secondaries, 50 sessions/node and 10 SSE clients. Aim for sampler CPU/frame
below 50 µs at idle and no DB or journal reads per tick. Idle sessions should
produce no activity frames. Lag and activity batches currently have tracing
logs; dedicated metric counters and latency histograms are not implemented.

## Two-primary federation resilience probe (2026-10-17)

Linux WSL2 x86_64, debug builds, loopback, isolated state/socket names:
two primaries and one secondary with a distinct outbound name for each primary.
Before is the preserved pre-fix Rust binary; the fetched main revision changed
only web code relative to that Rust baseline. After includes bounded concurrent
local RPC relays and authenticated-inbound watchdogs. For each build, start fresh
daemons with empty session inventories, establish both joins, then alternate
35 `oly ls --node <name> --json` calls between primaries. Discard five warmups;
verify every result's `items` is empty and `total` is zero. IPC durations use
`OLY_TIMING=1` response-minus-connected marks; wall time includes CLI startup.

| Path (30 samples/build) | Before | After |
|---|---|---|
| Empty remote list RPC, p50 / p95 | 2.8 / 3.5 ms | 3.1 / 3.7 ms |
| CLI wall time, p50 / p95 | 8.24 / 8.83 ms | 8.49 / 10.24 ms |

This short debug probe shows slight fast-path overhead, **not a speedup or a
release-throughput guarantee**. The change removes head-of-line blocking by
slow local RPCs and bounds stalled socket/channel operations, rather than
optimizing empty list requests. Release throughput and laptop sleep/resume
soak tests remain unmeasured.

`tests/e2e_join_reconnect.rs` separately verifies two simultaneous joins: one
primary remains registered and serves proxied lists throughout the other's
45-second authenticated-inbound timeout, and the stalled connection re-handshakes
and answers a sealed ping without restarting the secondary. Unsealed transport
pings continue during the outage and must not refresh authenticated liveness.
Retry backoff resets after successful joins, so a previous long outage does not
leave the next outage at a 60-second initial retry delay.

## Backlog (measure before optimizing)

- **TUI tree rebuild on every refresh** (2026-10-16): even a list-mode
  refresh rebuilds the folder tree (~19 ms at 5000 sessions). Skipping
  rebuilds whose tree inputs (filters plus each session's tree-visible
  fields) are unchanged would cut quiet-tick cost; the daemon list RPC
  (~205 ms for 5000 offline rows) is still the larger per-tick cost.
- **Daemon list at scale** (2026-10-16): `list_summaries` is still ~40 µs
  per offline row (SQL row build + per-row journal-dir probe). Batching or
  parallelizing the offline-row fill is the next candidate.
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
- **journal_sync on real disks**: done — see the 2026-10-16 real-disk
  capacity baseline (avg 7.1 ms / 397 ms max fsync on the boot NVMe), so
  append-latency claims no longer rest on tmpfs numbers.
- **Process start**: `oly ls` wall p50 is 4 ms, of which ~2 ms is binary
  startup + config + socket connect; only worth pursuing if attach or
  list ever regresses back toward it.
