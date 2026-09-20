# oly 0.5.0 plan

> **Status: executed.** This plan produced the 0.5.0 beta (`v0.5.x`
> branch). It is kept as the normative reference for the invariants,
> architecture rules, and release checklist that the code and tests cite
> (e.g. "PLAN §4"); milestone history is condensed to outcomes.

## 1. Outcome, priorities, and scope

**Make oly a reliable home for interactive programs, not a terminal approximation that users must learn to work around.** Humans should move between a local terminal, browser, and remote node without losing context or corrupting input. Agents should observe, wait, send input, and hand control to humans through explicit, resumable APIs.

This is a proposed implementation plan, not a description of capabilities already delivered. Compatibility with 0.x CLI behavior, protocols, configuration, and storage is not a constraint. Preserve valuable user data through explicit export/import tooling, not permanent compatibility branches.

Priority order:

1. **Correctness and fidelity:** terminal state, input bytes, history, and stream boundaries.
2. **Interactive latency:** no artificial typing waits; history operations and slow observers must not delay the controller.
3. **Multi-client operation:** explicit control ownership, stable geometry, predictable reconnect.
4. **Maintainability:** one terminal contract, one session stream, one attach implementation, bounded resources.
5. **Human and agent ergonomics:** trustworthy observations and understandable failure states.

Security, crash behavior, and cross-platform validation are release requirements throughout, not cleanup afterward.

### What “indistinguishable from the original CLI” means

For a supported terminal profile, application, platform, geometry, font/width configuration, and input script, compare a direct PTY run with a run through oly. Require equivalent application-visible input and query responses, visible cells/styles/cursor, main/alternate-screen behavior, and terminal history, within the latency budgets below.

Do not promise pixel identity between different fonts/emulators, unlimited native scrollback, zero network latency, or independent application layouts for clients sharing one PTY. Unsupported extensions must be declared and negotiated honestly, not silently discarded while advertising support. Security restrictions on clipboard/window manipulation are explicit exceptions to transparent relay.

### Scope boundaries

- Keep one PTY per session, local-first deployment, Linux/macOS/Windows, CLI, browser, notifications, and existing federation workflows.
- Do not add panes, a desktop terminal application, a distributed scheduler, collaboration CRDTs, or mandatory external infrastructure.
- Do not require a from-scratch terminal emulator. Select and harden an existing engine behind a narrow interface.
- Do not make thousands of sessions or native async PTY a prerequisite. Preserve portable blocking I/O until measurements justify another backend.
- Do not promise live-process survival across daemon death in 0.5.0. Sessions survive **client** death; history survives daemon restart according to the durability policy. A separately supervised session-host process is a possible later feature, not PID-based “reattachment” to a lost PTY.
- Advanced graphics can follow 0.5.0 if not advertised by its terminal profile. Common text/TUI fidelity, Unicode, mouse, paste, and correct keyboard negotiation cannot be deferred.

## 2. Repository review and baseline

Reviewed baseline: commit `931d7b9`, package version `0.3.3` in `Cargo.toml`.

The review covered `README.md`, `ARCHITECTURE.md`, `ARCHITECTURE_PTY.md`, `ARCHITECTURE_NOTES.md`, `SPEC.md`, frontend guidance, the prior security audit, and relevant session, client, protocol, storage, HTTP, federation, frontend, and test paths. Source is evidence of present behavior; several architecture sections describe older implementations.

### 2.1 Keep the useful foundation

- Daemon-owned PTYs and independent attach lifetimes are the right product boundary.
- `portable-pty`, dedicated reader/writer isolation, `Bytes` fan-out, SQLite metadata, and persistent log handles are useful building blocks.
- Preserve `PtyScanner` fragmentation tests, its fast plain-text path, and platform-specific fixtures even if policy and ownership change.
- Preserve cheap immutable mode publication from `SharedModes`, but associate input modes with a precise stream revision rather than an unrelated latest value.
- Existing log indexes, PTY integration fixtures, terminal guards, node routing, notification integration, and xterm.js are assets to evolve.
- Binary server-to-browser output and node-proxied streaming **already exist**. The work is consistent framing, semantics, and flow control, not introducing streaming for the first time.

### 2.2 Findings driving the plan

| Finding | Source evidence | Consequence / required change |
|---|---|---|
| Input deliberately waits | `src/client/attach.rs`: 30 ms key-burst wait, 150 ms paste-followup suppression, up to 60 ms waiting for server frames before checking terminal events again | Independently wakeable input/output paths; remove timing-based typing/paste detection. Constants are not measured latency results. |
| Input is interpreted at several layers | CLI attach/send, `src/session/store/attach.rs`, web key utilities | Ctrl-D is consumed as detach, clipboard shortcuts intercepted, key mapping incomplete, focus-loss reports discarded, normal arrows globally replaced under DECCKM. Raw/pasted bytes must not undergo key substitution. |
| Snapshot, file offset, and broadcast lack one atomic boundary | `runtime.rs` updates screen under lock, then appends/broadcasts outside it; `store/attach.rs` combines screen state with file length; broadcasts carry unsequenced `Bytes` | Snapshot can include not-yet-appended/broadcast bytes; subscribe-then-file-read can overlap broadcasts. A session read lock alone does not solve this. |
| Replay is not bounded to captured EOF | `src/session/persist.rs::read_output_from` captures EOF, then uses `read_to_end` | Concurrent append can make returned bytes disagree with reported end. Read fixed committed ranges. |
| Resize reconstructs state destructively | `src/session/screen.rs::safe_resize_parser` replays formatted VT rows trimmed to new width, resets on panic | Narrowing can discard columns; inactive main-buffer history and parser state require a real preservation model. Formatted rows are not lossless state serialization. |
| Scrollback differs by path | CLI seed has a 1,000-row floor bounded by session retention; xterm uses `scrollback: 1000`; local WS starts from a screen snapshot | Bigger seeds alone cannot provide durable pageable history. Browser output flush also calls `scrollToBottom()` unconditionally. |
| Retention reuses offsets | `src/session/store/lifecycle.rs::truncate_oversized_logs` truncates the whole file, resets counters, clears resize history | Cursors can refer to different bytes. Replace with immutable segments and stable logical identities. |
| Persistence failure does not stop publication | `src/session/runtime.rs` warns on append failure and still broadcasts | File can cease to represent everything clients saw. Define persistence health and explicit gap/failure policy. Successful `write` is not power-loss durability. |
| Terminal policy removes useful semantics | `src/session/scan.rs` strips most OSC and DCS/APC/PM/SOS traffic; only selected queries are answered | Hyperlinks, integration signals, and query behavior need a capability/security contract, not blind forwarding to every terminal. |
| Queries can use the wrong position | Reader collects queries, processes full filtered chunk, then answers every CPR from the final cursor | Answer at the query's position in stream order, independent of OS read fragmentation. |
| Modes/resize travel separately from output | `SharedModes`, `resize_tx`, attach relay loops | Modes can come from a later chunk; resize lacks exact replay boundary. Include ordered revisions/events. |
| Presence is counting/activity, not ownership | Runtime/store attach counters, shared writer, latest successful resize wins | No identified controller, lease, resize authority, or safe agent/human handoff. A phone must not reshape a desktop TUI. |
| Bounded federation queues still allow head-of-line blocking | `src/http/nodes.rs` awaits per-stream channel sends in the shared receive loop; registry lacks general RPC deadlines | A stalled stream can delay unrelated replies/heartbeats. Add credits, fairness, cancellation, connection-generation cleanup. |
| Client buffering can grow without a byte limit | CLI attach has an unbounded frame channel; browser queues output arrays until rendering catches up | Bound bytes, not just message counts; connect rendering progress to credit. |
| Runtime/history responsibilities are coupled | Parsing, append, query replies, status polling, log rendering, persistence across runtime/store/relay paths | One session ordering authority; history/index/notification work off the hot path. |
| Browser orchestration is concentrated in one large page | `web/src/pages/SessionDetailPage.tsx` owns sockets, timers, resize, replay, queues, reconnect, UI | Extract testable stream/terminal/history controllers and cancellation states. |
| Documentation drifts from code | Architecture claims JSON-only WS, polling remote attach, no daemon screen buffer, obsolete commands/config/resize/seed behavior | Generate protocol/config references; update architecture invariants. README detach instructions also disagree with Ctrl-D implementation. |
| Security debt needs current validation | `src/http/auth.rs` has a non-expiring password-hash-derived token; prior audit predates IPC permissions/limits and decompression fixes | Re-audit current paths; revocable browser sessions, WS origin policy, safe replay. Do not count already-fixed findings as new work. |
| CI misses release coverage | Rust CI runs selected integrations rather than unit suite; path filters miss build inputs; web CI omits Playwright | Complete supported test surfaces, deterministic stream/fault tests, broader triggers. |

### 2.3 Validation performed for this plan

- `cargo test --locked --offline --bin oly`: **491 passed**, 0 failed.
- `cd web && npm test`: **112 passed across 12 files**, 0 failed.

These are baselines, not proof of terminal fidelity or concurrency correctness. Full integrations, Playwright, release builds, benchmarks, and macOS/Windows validation were **not run as part of this planning change**. Performance numbers below are proposed targets.

## 3. Non-negotiable invariants

Reference these IDs in tests, reviews, and release evidence.

- **I1 — Ordered session:** each incarnation has one event order covering output, successful resize, terminal revisions, and completion.
- **I2 — Correct attach boundary:** snapshot C represents exactly state through C; subsequent application starts after C without omission/duplication.
- **I3 — Stable history:** retention never reuses sequences/offsets. Missing history returns an explicit gap/`HistoryExpired`, not empty success.
- **I4 — Input integrity:** accepted raw bytes are not normalized, key-rewritten, or silently dropped. Semantic input is encoded exactly once under defined modes.
- **I5 — Single query authority:** only the session engine answers application queries. Another renderer never adds a respondent.
- **I6 — Geometry authority:** passive observers cannot resize the PTY. Resize results and ordering are visible to all clients.
- **I7 — Bounded isolation:** every queue/cache has byte limits and overflow policy. Slow clients cannot stall unrelated control/session work.
- **I8 — Truthful durability:** accepted input, PTY-written input, journal availability, and disk durability are distinct. PTY write success does not prove application consumption/effect.
- **I9 — Non-destructive observation:** screen/history/search/replay do not mutate the live PTY, size, input-needed state, or user scroll position.
- **I10 — Ordered ending:** completion follows final retained output and persistence barrier, or reports incomplete capture. Process exit and PTY EOF are separate facts.
- **I11 — Honest capabilities:** advertised features have tested input, live rendering, snapshot, and replay semantics.
- **I12 — Safe replay:** replay cannot write child stdin, set clipboard, launch URLs, upload files, or execute hooks.

## 4. Target architecture and concurrency

```text
 Human CLI / Browser / Agent API
        | control requests + one versioned session stream
 IPC / HTTP / WebSocket / Federation adapters
        | authenticate, authorize, frame, route; no terminal policy
 Session service / attachment registry
        | commands, controller leases, subscriptions
 Per-session ordering authority
   terminal engine + query/input policy
   sequences, resize barriers, lifecycle
   immutable state + bounded recent replay cache
        ^ PTY reads       | input/resize       | ordered records
 Platform PTY backend <---+                   v
   reader / writer / waiter           Journal writer / segments
                                              |
                              Checkpoints / history projection / index
                                              |
                                  Logs / replay / search / agents

 SQLite: launch metadata, identities, auth/config, indexed summaries
 Notifications: state/activity consumers, never in the PTY critical path
```

### 4.1 Ownership

Use one logical sequencer per session, implemented as an owned event loop/actor. This does not mandate a new process, an actor framework, or a Tokio task per byte. Dedicated PTY I/O threads remain acceptable. Prototype parsing placement and measure thread crossings before fixing topology.

The sequencer:

1. Serializes observed output, successful resize barriers, input authorization/encoding, and lifecycle facts.
2. Owns mutable terminal state; publishes immutable snapshots/revisions. Transports do not access parser internals under a session lock.
3. Allocates sequences before exposing events; enqueues journal records and retains recent events until persistence makes them replayable.
4. Generates query replies at exact parser positions using reserved bounded writer capacity. Never block the PTY reader on a full user-input queue and create a read/output → query → blocked writer deadlock.
5. Delegates checkpoint serialization, historical replay, indexing, metadata updates, and notifications to bounded workers. Snapshot capture is a bounded copy or immutable/copy-on-write view, not a long global lock.

Use the registry as a handle registry, not the owner of every detail. Begin with library modules (`src/lib.rs` if needed for testing); extract crates only when dependency boundaries/reuse justify them.

| Boundary | Owns | Consolidate from |
|---|---|---|
| `terminal/` | Engine adapter, profile, query broker, input codec, snapshot, safe display | `session/scan.rs`, `screen.rs`, terminal parts of `pty.rs`, client transforms |
| `session/` | Sequencer, attachments, leases, lifecycle | Runtime/store and `session/resize.rs` |
| `journal/` | Segments, cursors, checkpoints, recovery, retention, projections | `session/persist.rs`, `session/logs/*` |
| `protocol/` | Schemas, binary framing, typed errors, conformance vectors | `protocol.rs`, IPC/WS framing |
| `attach/` | Shared subscribe/resume/credit state machine | `daemon/rpc_attach.rs`, local/proxied WS loops |
| Transport adapters | Connections, authorization, routing, cancellation | IPC/HTTP/node modules |
| Web controllers | Connection state, rendering queue, history, ownership UI | `SessionDetailPage.tsx`, `XTerm.tsx` orchestration |

### 4.2 Live availability versus durability

Move synchronous recording off the interactive publication path through a **bounded** pipeline, not fire-and-forget logging:

- `head_seq`: accepted ordered events available in memory/live delivery.
- `journal_seq`: contiguous events appended and readable from storage.
- `durable_seq`: contiguous events covered by synchronization policy.
- `earliest_seq`: earliest retained reconstructable history boundary.

Resume combines immutable disk ranges with recent cache for `journal_seq < seq <= head_seq`. File length is never live state. Events cannot leave cache before journal availability or an explicit failure/loss transition.

Default: low-latency live publication with periodic group synchronization; begin with a 100 ms sync interval and expose actual durable progress. This is not a hard power-loss bound under storage stalls. A strict recording option waits for durable publication at a documented latency cost. Graceful shutdown/completion synchronizes.

On disk full/permission loss/write failure, expose `persistence_degraded` immediately. Default to bounded buffering, then backpressure this session's PTY ingestion instead of silently dropping output. After a configurable recovery deadline, fail capture/session explicitly. Opt-in availability-first recording may continue with an explicit missing range, never a complete-history claim. Do not block all sessions, spin, or allocate indefinitely.

## 5. Terminal fidelity and capability design

### 5.1 Stable virtual terminal identity

A detached multi-client application cannot use every attached terminal as its authority. Choose a session profile at creation covering:

- `TERM`/terminfo and supported extensions;
- geometry, default colors/palette, Unicode width/version;
- cursor/keypad/keyboard protocols, paste, mouse/focus;
- query responses and permission-gated effects.

Do not advertise the strongest current client and change capabilities when it leaves. Clients negotiate render/control compatibility. A weaker client can observe through an explicitly compatible viewport or receive an actionable incompatibility error. Profile changes require a new session, not a browser theme switch.

`TERM=xterm-256color` is an interoperability starting point only if advertised behavior is supported. Package an oly terminfo entry if needed, with installation/fallback guidance. Queries, snapshots, history, and browser rendering share color/width policy; do not infer session colors from the daemon's unrelated shell on each query.

### 5.2 Release-blocking engine/restore prototype

Compare current `vt100` with maintained embeddable alternatives. Alacritty/WezTerm-family cores or libvterm bindings are evaluation candidates, subject to license, platform/build, API, and maintenance review—not assumed drop-in dependencies.

Required capabilities:

- main **and** alternate buffers, saved state, history with hard/soft-wrap metadata;
- lossless supported resize, not trimmed row reconstruction;
- parse callbacks at exact query positions; complete input-affecting modes;
- maintainable state export/import;
- CJK, combining marks, emoji/ZWJ, wide right-edge cells, malformed UTF-8;
- bounded adversarial strings/parameters; sustained TUI throughput and cheap state capture.

Build the comparison corpus before choosing. Test live output, resize, attach on alternate screen, return to main screen, and continuation after restore. Keep `vt100` only if it passes without accumulating lossy workarounds. If none passes, fund a narrowly scoped upstream change/adapter; do not silently reduce fidelity to meet a date.

### 5.3 Checkpoint state versus renderer restoration

An internal checkpoint includes, as applicable:

- both grids, active buffer, history references, hard/soft boundaries;
- current/saved cursor, wrap-pending, tabs, margins, origin/insert/autowrap;
- rendition, palette/default colors, charsets, hyperlinks, cursor shape/visibility;
- keyboard/mouse/focus/paste modes and stacks;
- title/progress/integration and synchronized-output state;
- incremental UTF-8/escape parser state, or guaranteed safe parser boundary;
- geometry, profile, engine/schema version, event cursor, checksum.

Engine serialization is **not** automatically a valid restore stream for xterm.js/native terminals. Implement a separate renderer restore adapter. Do not casually mutate private xterm buffers; a serialize addon is not a universal cross-engine importer.

Native VT restoration uses representable parser boundaries and a side-effect-free restore program plus bounded subsequent replay. Partial strings/UTF-8, saved cursor attributes, wrap state, both buffers, and mode stacks require continuation tests. If a state cannot be restored, reconstruct from an earlier valid checkpoint. Matching visible cells alone is insufficient; cadence/profile limits must bound fallback work.

Normal native output remains an approved byte stream, not repeated full-screen repaint. On Windows validate VT input/output modes and ConPTY with the same corpus. Retire current `AttachRenderer` when direct output passes. Any necessary Windows adapter is isolated and held to the same fidelity contract, not a hidden lower-quality fallback.

### 5.4 Ordered terminal policy instead of broad stripping

Keep original PTY bytes in the journal. Derive terminal state and a renderer-safe display stream; these are versioned reproducible projections, not competing authoritative logs.

| Category | Treatment |
|---|---|
| Text, motion, erase, SGR, scroll regions, buffer changes | Interpret state; preserve compatible live display semantics. |
| DSR/CPR, DA, DECRQM, supported window/color/keyboard queries | Answer centrally at query position; consume query so renderers cannot reply again. Return standard negative/unsupported replies where defined; otherwise document unsupported behavior. Never inject unsolicited guesses. |
| OSC 8, title, cursor shape, progress, useful shell integration | Model/preserve supported semantics; integration is metadata, not executable instructions. |
| OSC 52 / window manipulation | Explicit live-controller permission, off by default; never execute in replay/observers. |
| DCS/APC and extensions | Classify by capability. Unsupported graphics are declared unsupported, not blindly forwarded. |
| ConPTY echoes | Narrow backend handling with fixtures; never strip ordinary Unix text resembling reports. |

Use one ordered parser/policy stream, not “collect probes, render whole chunk, then answer.” Regression: `A`, CPR, `B`, CPR in one read produces two correct distinct positions, identical when split at every byte.

Oversized/unfinished control strings must not leak a dangerous tail as terminal commands when a buffer cap is hit. Use bounded discard/streaming until termination, retain original recording subject to retention, emit diagnostics, and finalize incomplete input at EOF deterministically.

## 6. Journal, scrollback, logs, and replay

### 6.1 Storage model

Replace `output.log` plus independently timed text resize records with a typed segmented journal:

```text
sessions/<id>/
  manifest.json                    # format/profile/incarnation, segment ranges
  journal/00000001.seg              # bounded records, checksums
  checkpoints/<cursor>.checkpoint   # atomic, versioned state
  indexes/                         # disposable seek/history/search indexes
  files/                           # artifacts/uploads
```

Records have format version, incarnation, monotonic `seq`, monotonic elapsed time, type, bounded length, checksum. Launch wall time is separate. Record original PTY output, successful resize/geometry version, lifecycle, and policy/profile facts needed for reconstruction. Input audit metadata can contain client/request identity/length; **input content is not recorded by default** because passwords may not be echoed.

Durable API cursor: `{session_id, incarnation, seq}`. A sequence denotes a complete event; wire fragments are transport-local, not another history identity. Optional cumulative original-output byte offsets help export tools but never replace event ordering. TypeScript uses `bigint`/lossless encoding, not JSON numbers beyond 2^53.

Queries may exist in original output but not display projection. Display batches carry source event ranges; events with no display bytes still advance cursor. Resume requires compatible recorded profile/projection version, never counts rendered ANSI bytes as source offsets.

### 6.2 Recovery and retention

- Seal segments with range metadata/checksums. Recover a torn active tail to the last valid record; quarantine/report non-tail corruption.
- Daemon restart seals an interrupted live incarnation at its recovered boundary; do not append new live events into possibly previously published sequence numbers. A new process run gets a new incarnation. Cursors beyond recovered history return explicit incomplete-capture errors, not empty success or reused identities.
- Install manifests/checkpoints atomically with platform-correct synchronization. Reconcile lagging SQLite summaries; no per-output SQLite transaction.
- Configure age/byte retention, per-session/global quotas. Ratify defaults after soak tests; expose retained/expired ranges.
- Delete whole old segments only after a retained checkpoint reconstructs the first exposed boundary. Use bounded expiring read pins; disconnected readers cannot pin forever.
- Never reset sequences/truncate active stream to zero. Return `HistoryExpired { earliest_cursor, available_checkpoint }` for obsolete cursors.
- Version checkpoints/projections. Indexes are rebuildable; recordings are not disposable. Engine upgrades need readable checkpoint versions or explicit conversion with sufficient source state retained.
- Completed history remains readable without a live runtime, including through an offline read-only journal reader.

### 6.3 Three explicit observation views

1. **Screen:** current terminal state at revision; “what is showing now?”, not a transcript.
2. **History/transcript:** finalized main-screen logical lines, styles, wrap metadata, stable IDs, source ranges. Overwritten progress and alternate-screen frames are not magically lossless text history.
3. **Recording/replay:** original events plus geometry/timing in an isolated engine, for historical TUI reconstruction.

Expose these choices in API/CLI. Plain text is the safe default for agents/pipes; sanitized ANSI is optional; original bytes need explicit raw export and a TTY warning. An arbitrary byte slice beginning inside an escape is not standalone replay.

Seek finds a checkpoint, replays a bounded range with recorded resizes, then renders/pages. Do not replay gigabytes from byte zero for every tail/search. Keep the sidecar-index concept but index logical history/checkpoints instead of calling heuristic 2,048-byte records stable lines. Search is cancellable and off the sequencer.

### 6.4 Scrollback UX and resize

**Browser:** separate live-follow from history-browse. Follow only when already at bottom or explicitly requested. Preserve selection and anchor `(line_id, cell_offset)` across output, paging, reconnect, retention. Fetch older pages with row/byte caps. A history view never resizes/replays into the live PTY. Show new-output count and “Jump to live.” If xterm cannot safely prepend history, use a styled virtualized history surface, not private buffer surgery.

**Native:** seed bounded configurable recent history once on fresh attach, then restore live state without clearing preexisting host scrollback. Native buffers are finite and have no portable random-access/prepend API. Provide `oly history`/explicit history view for all retained history rather than claiming scrollbar seeding is complete. Intact-renderer resume never reseeds; a restarted CLI cannot assume hidden host terminal state survived.

**Resize:** preserve logical lines/soft wraps, including inactive main-buffer history while a TUI is active. Reflow history view separately from live grid. Replay live resize at recorded boundaries. Narrow-then-wide must not destroy retained historical text/styles. Actual live grid shrink follows the selected terminal profile; do not invent a guarantee that application-visible cells can never be lost on shrink.

## 7. Attach protocol, resume, and flow control

### 7.1 One contract, thin adapters

Introduce an independently versioned major/minor stream protocol with feature negotiation. Keep JSON for low-volume control where useful; binary length-delimited PTY data over IPC, browser WS, and federation. No repeated JSON/base64 hot-path encoding at node hops.

Specify header length/type/stream ID, source cursor/range, endianness, unknown-frame behavior, fragmentation, limits checked before allocation, and decompression bounds. Generate TS control types and shared golden vectors from Rust/schema definitions. Chunk snapshots/history instead of giant init JSON lines.

Messages:

- `Hello/Welcome`: versions/features, identity, limits, profile compatibility.
- `Attach`: fresh/resume cursor, role, viewport, history budget, renderer profile/state identity.
- `SnapshotBegin/Part/End`: state through C, geometry/modes/profile, restore representation, checksum.
- `Events`: display/output and ordered geometry/state events with source ranges.
- `Applied/Credit`: last completely applied cursor and byte budget.
- `Input/InputAck`, `AcquireControl/ReleaseControl`, `ViewportChanged/SetGeometry`.
- `Gap/HistoryExpired/ResnapshotRequired`, `Cancel`, `Ping/Pong`, typed errors.
- `SessionEnded`: final cursor, exit reason/code/signal, capture/durability status.

### 7.2 Correct fresh attach

1. Authenticate/register an identified attachment, select role/profile. Apply **authorized** initial geometry through the sequencer, never out-of-band.
2. Obtain immutable snapshot boundary C and subscription at C+1 in one ordered operation. State, dimensions, history endpoint, and modes all describe C.
3. Capture a bounded replay pin; serialize/send outside the hot loop while events continue into cache/journal.
4. Client restores at declared geometry with input/query effects suppressed, then acknowledges snapshot application.
5. Stream exact `(C, head]` ranges. Discard duplicates by cursor; detect holes before applying later events. Continue live without resubscribing at inferred file length.
6. If catch-up exceeds pin/retention, report explicit resnapshot/history expiry; never allocate unbounded initial replay.

Socket send completion is not renderer completion. xterm write callbacks acknowledge application; measure actual paint separately. Native stdout completion means handed to terminal, not pixel-painted. Uncertain/partial external-renderer restoration requires a fresh safe restore, not an optimistic resume cursor.

### 7.3 Reconnect and completion

- Resume requires matching incarnation, profile/projection, renderer state, applied cursor. New incarnation/schema rejects stale cursors.
- Suspended browser with intact renderer resumes; reload/new terminal snapshots. Old connection-generation callbacks cannot advance new state.
- Snapshot replacement and incremental replay are distinct. Never send a delta labeled init and reset the terminal before applying it.
- Capped exponential reconnect backoff with jitter; keep local history visible and status explicit. Do not buffer unlimited disconnected typing or replay uncertain keystrokes.
- Control authority expires independently of readable history.
- Deliver final output before session-ended. Child exit without EOF has bounded drain deadline and `capture_incomplete` when descendants keep handles open.

### 7.4 Byte budgets and scheduling

Initial budgets to tune with benchmarks:

| Resource | Starting policy |
|---|---|
| Data frame | 64 KiB payload; fragment larger transfers; no wait to fill |
| Per-client server queue | 256 KiB plus at most 1 MiB negotiated in flight |
| Shared recent event cache | 8 MiB/session, shared across clients, global cap |
| Browser pending writes | 1 MiB high-water mark; credit after bounded application |
| Input queue | 1 MiB/session, per-client quotas, reserved query/control capacity |
| Paste/upload | Chunked, bounded/staged; no multi-megabyte JSON control messages |

Account for paused tabs, blocked stdout, proxies, compression, history readers, snapshots—not only application channels. Coalesce only adjacent already-ready data; stop at geometry/state/ownership barriers.

Slow observers catch up from journal or explicitly disconnect/resnapshot, not throttle the controller. Journal pressure is a separate session recording policy. Prioritize input/control/cancel/heartbeats over bulk history/output with fairness. Already-sent TCP bytes cannot be overtaken: bound frames and isolate bulk connections if needed to meet measured latency.

## 8. Input, control ownership, and geometry

### 8.1 Attachment registry and controller lease

Replace anonymous counters with attachment records: principal/client kind, connection generation, role, capabilities, viewport, applied cursor, liveness.

Default policy: **many observers, one controller**.

- Human attach acquires free control; otherwise joins visibly as observer with request/takeover action. Opening a browser does not steal control.
- Agents observe by default. `send` requires an authorized short-lived lease or existing automation lease, not a bypass of a human controller.
- Leases have monotonic generation/fencing token, server expiry, connection-liveness renewal. Every input/geometry command carries the token; stale delayed packets are rejected.
- Handoff is sequenced: reject new old-generation input, resolve the bounded already-accepted prefix, acknowledge boundary, then enable new controller. Force takeover reports discarded/unwritten input, not invisible late old-controller bytes.
- Show controller identity/type, observers, ownership changes, expiry in CLI/browser/API. Presence alone does not imply active watching or suppress all notifications.
- Shared simultaneous typing is not the default. An opt-in shared mode can follow after serialized transactions and clear UX; it is unnecessary for smooth multi-client attachment.

### 8.2 Raw versus semantic input

Distinct wire forms:

- `RawBytes`: opaque bytes from compatible native/xterm frontend or explicit binary agent request. Never arrow-rewrite or normalize newlines.
- `Key`: key/modifiers/press-repeat-release as supported, encoded once by session codec under current modes.
- `Paste`: explicit text/bytes, newline policy, bracketed-paste policy, bounded transaction ID.
- `Mouse/Focus`: semantic events where needed, encoded once using session modes and viewport mapping; observers cannot send them to PTY.

Prefer raw native input when outer terminal/profile produces compatible bytes. Use a backend adapter when Windows events or negotiated keyboard differences require it. Test modifiers, function keys, keypad, Alt/Meta, Ctrl combinations, Kitty/CSI-u negotiation, repeat/release, IME, and mouse modes. Avoid independent incomplete key tables in three languages.

Detach is `Ctrl-D` (EOT), matching the historic key contract. Ctrl-C/Ctrl-Z/Escape retain application meaning; Ctrl-V and Shift+Insert are intercepted as clipboard paste operations. Clipboard/file transfer reads files, images, and text from the clipboard.

Paste boundaries are explicit, not guessed from typing speed. Remove 30/150 ms heuristics after platform tests pass. Serialize paste relative to user transactions; impose quotas/cancellation; never silently normalize/retry. Stage/chunk large paste without session locks. Define cancellation of an open bracketed paste and reserved query/control capacity so neither is starved. PTY writes are not atomic application transactions and cannot be rolled back.

### 8.3 Acknowledgments and retries

`InputAck`: request ID, lease generation, accepted/written byte counts, status (`rejected`, `queued`, `written`, `partial`, `unknown`). Acceptance reserves capacity. Reject before acceptance when full or use bounded backpressure; never acknowledge then drop keys.

Deduplicate request IDs within a documented live-incarnation window; same ID/different payload is an error. Lost connection after PTY write can be ambiguous, especially after daemon crash. Do not claim exactly-once application effects. Return `unknown` where necessary; agents inspect before retrying.

Separate input write acknowledgment from waiting for output. Remove transport `wait_for_change`; use cursor-based wait/subscription with independent deadline/cancellation. Echo is not task completion.

### 8.4 Geometry

Default: **controller-owned geometry**. No controller means retain last size or fixed start size. Agents can start deterministic fixed-size sessions. Observers report viewports but cannot resize.

Do not default to smallest-attached-client size: a phone/background tab would constrain the working TUI. Include fixed geometry in 0.5.0; other aggregation policies are optional later.

- Validate dimensions/allocation limits; deduplicate equal requests.
- Coalesce resize drags at short bounded cadence, preserving final size. No arbitrary half-second startup suppression.
- Sequence successful backend/model resize as one geometry event. Failed backend resize leaves published geometry unchanged.
- Specify read/resize barrier semantics. Already-buffered bytes may predate OS resize; userspace cannot infer an application's unobserved redraw boundary. Record one consistent observed order and test real applications, not claim kernel atomicity.
- Browser observer can letterbox/pan canonical geometry. Map mouse coordinates only after it becomes controller.
- Native observer with incompatible dimensions must not pour canonical-width VT into a narrow terminal and claim fidelity. Use bounded viewport adapter or explicit compatible/fixed-size view. Never create resize fights between clients.

## 9. Human and agent interfaces

### 9.1 Native CLI

- Split terminal input, framed network reading, and bounded terminal writing into independently wakeable paths. Unix readiness/dedicated reader and appropriate Windows console/VT backend; no server-frame timeout as input scheduler.
- Blocked stdout must not prevent detach/cancel/input; keep bounded output queues and ordering.
- Live output remains streamed. Restore rendering is for init/recovery/explicit viewport adaptation.
- Restore termios/console, cursor/title, mouse/focus/paste/keyboard modes, synchronized output on normal detach, errors, unwind, supported signals. Preserve surrounding shell/list-TUI behavior. SIGKILL/power loss cannot run cleanup and must be documented.
- Keep status/error chrome out of terminal data. Non-TTY raw output has no trailing “Session ended” prose; diagnostics use stderr.
- Reconnect shows connection/control state; no reseeding intact renderer or retrying uncertain input without consent.

### 9.2 Browser

Extract `AttachConnection`, `TerminalController`, `HistoryController`, and input/control UI. One state machine owns `connecting → restoring → catching_up → live → reconnecting/ended/error`; one generation owns callbacks/cancellation.

- Feed small output promptly, let xterm batch rendering, avoid an extra animation-frame gate before each first write. Bound burst work so input/UI stays responsive.
- Advance applied cursor/credits in write callbacks; measure paint independently.
- Handle backgrounding, phone suspension, network switch, rapid navigation, and unmount with writes in flight.
- Separate container measurement from PTY resize authority; no FitAddon/server feedback loop.
- Preserve scroll position/selection, never unconditional `scrollToBottom()`. Separate live terminal and history replay.
- Test composition/IME, mobile keys, selection versus mouse reporting, touch scroll, explicit paste, upload cancellation, controller loss mid-paste.
- Add accessible ownership/connection controls without per-chunk screen-reader noise; evaluate xterm accessibility deliberately.

### 9.3 Agent API and CLI

Specify machine workflows before adding aliases. These are 0.5.0 surfaces, not existing commands:

```text
oly start --json --size 120x40 -- <program> <args...>
oly screen <id> --json
oly history <id> --before <cursor> --limit N --json
oly wait <id> --after <cursor> --until <condition> --timeout 30s --json
oly control acquire <id> --json
oly send <id> --lease <token> --request-id <id> --key enter --json
```

Contracts:

- Explicit session IDs for machine mutations; “most recent” only as human convenience.
- Machine responses include schema version, session/incarnation, cursor/revision, typed status/error, truncation/durability indicators. JSON stdout is data only; diagnostics stderr. Document exit codes/timeouts.
- Separate raw recording, plain transcript, styled history, screen cells. Application output may be adversarial; agent integrations treat it as data, not oly instructions.
- `wait` conditions: exit, output after cursor, bounded pattern, explicit app signal, heuristic idle/input-needed. Return matched evidence/condition or real timeout/cancel error.
- Retain silence notifications but label `likely_input_needed` with reason/time/cursor, not proof it is safe to approve. Define title/progress-only activity semantics.
- Optional app/shell integration emits bounded structured ready/busy/prompt signals over an authenticated session-scoped channel. Screen regexes are not the only interface; child signals never confer authorization.
- Document handoff: agent runs/observes → requests attention → human acquires → agent read-only → human releases → agent resumes from cursor.
- Generate API/schema docs and repository examples/skill material. MCP is optional after the stable API, not a duplicate runtime implementation.

## 10. Federation, lifecycle, and security

### 10.1 Federation is transport, not another runtime

- Preserve origin identity/incarnation/cursors end-to-end. Primary routes; no terminal reinterpretation or new history offsets.
- Reuse attach service directly on secondary instead of secondary-to-itself JSON IPC relay for each stream.
- Per-stream credit, fair outbound scheduling, deadlines, explicit cancel, cleanup. Never await a full stream channel in shared receive loop.
- Tag pending state with node connection generation; old disconnect cleanup cannot remove replacement connection state.
- Forward input without waiting for application output; acknowledgments asynchronous. Relay overhead is transport/queueing/serialization, not inherently an extra RTT per byte.
- Disable small interactive compression. Benchmark existing gzip JSON against uncompressed binary and optional bulk-history compression; avoid synchronous/repeated encoding. Bound decompression and reject overflow explicitly.
- Separate bulk connections if one TCP stream cannot meet latency under loss. No mandatory QUIC/new network stack before evidence.
- Test restart/reconnect, connection replacement/duplicate node names, stale responses, cancellation, unavailable origins, slow streams, version mismatch.

### 10.2 Lifecycle and process ownership

- Centralize child waiting/finalization; attachments subscribe to lifecycle rather than polling status individually.
- Distinguish stop request, natural exit, signal, kill, spawn failure, PTY failure, capture failure, daemon interruption. Stop/kill/finalize are idempotent.
- Define graceful stop: optional app interrupt, then platform process-tree termination/escalation with deadline. Injected Ctrl-C is not POSIX SIGTERM.
- Validate Unix process groups, Windows process-tree/Job Object behavior where appropriate, reaping, inherited PTY handles, blocked readers/writers, no leaked managed children after shutdown/delete.
- Keep final output through EOF or bounded drain expiry; persist final state once. Stop must not lose last diagnostics.
- Restart reconciliation reports interrupted sessions truthfully. A PID cannot recover a lost master PTY. Never silently restart agent commands that may have produced side effects.
- Session-host separation for process-surviving daemon upgrades is a separately costed future ADR; preserve reusable protocol/journal boundaries now.

### 10.3 Security/privacy release requirements

- Replace password-derived non-expiring browser tokens with random revocable expiring sessions, securely stored/validated. Revocation/logout invalidates server-side authority including open control streams.
- Scoped machine credentials (`observe`, `input/control`, `manage`, `node`) and session/node authorization. A frontend read-only flag is not access control.
- WS Origin validation, cookie-mutation/CSRF policy, trusted proxies, secure cookies, request/rate limits, TLS deployment requirements: documented/tested. No reusable query-string credentials; secure cookies or short-lived upgrade tickets where headers unavailable.
- Keep loopback defaults, explicit unsafe no-auth mode. Validate IPC peer/pipe ownership and Unix/Windows state-file permissions; preserve existing fixes.
- Isolate reverse-proxied apps from daemon credentials. Do not forward oly cookies/Authorization to arbitrary upstreams as SSO; require explicit trusted exchange/origin isolation.
- Recordings private by default. Input not recorded by default; output can still contain secrets. Provide per-session recording/retention controls and redacted diagnostics. Do not promise perfect automatic secret redaction or guaranteed physical secure erase.
- Untrusted escapes/recordings: safe plain export and side-effect-free replay mandatory; hyperlink scheme restrictions; upload authorization/path safety independent of terminal input.
- Test traversal/symlink races, quotas, decompression bombs, frame/checkpoint limits, parser resource exhaustion, node compromise boundaries, hook argument handling.
- Revalidate `docs/SECURITY_AUDIT_REPORT.md` against current code; publish verified fixes, remaining risks, and regressions instead of copying the old severity list.

## 11. Performance budgets and observability

### 11.1 Measurement methodology

Build deterministic immediate-echo, query-checker, line-producer, resize-aware TUI, and input-recorder fixtures. Real applications form a second layer, not the only timing oracle.

Compare direct PTY/native-terminal and oly with identical input/dimensions/terminal/platform/release build. Report p50/p95/p99, throughput, CPU, allocations, memory/backlog. Record hardware, OS, terminal/browser, compiler flags, storage, profile, network. Benchmark current `opt-level = "z"` against `2`/`3`; binary size need not dictate interactive performance.

Trace:

```text
input observed → client queued → transport sent → owner accepted → PTY written
output read → terminal parsed/ordered → subscriber sent → client applied → painted
                        └→ journal appended → durable
```

Use monotonic local clocks. Calibrate cross-node clocks or use round-trip/delta measurements; never subtract unsynchronized timestamps as one-way latency. Browser paint needs visual/frame instrumentation beyond write callbacks.

### 11.2 Proposed release targets

Ratify these budgets against M0 measurements. They are not current results; changes require evidence and explicit decision.

| Scenario | Target |
|---|---|
| Local CLI key-to-echo overhead versus direct run, not overloaded | p95 ≤ 5 ms, p99 ≤ 15 ms added; no intentional per-key sleep |
| Backend input acceptance → writable PTY write | p95 ≤ 2 ms, p99 ≤ 5 ms |
| PTY output read → local client application, interactive load | p95 ≤ 5 ms, p99 ≤ 15 ms |
| Browser key-to-paint overhead versus direct browser PTY harness, 60 Hz | p95 ≤ 25 ms, p99 ≤ 50 ms added; report frame cadence |
| One-hop federation processing overhead, excluding link RTT/app work | p95 ≤ 5 ms, p99 ≤ 15 ms; no per-key output wait |
| Warm local attach, 120×40, bounded recent history | usable screen ≤ 150 ms p95; interactive ≤ 300 ms |
| Browser LAN attach, excluding auth/assets | usable screen ≤ 300 ms p95; history backfill never gates input |
| Intact-renderer reconnect, ≤ 1 MiB local/LAN gap | catch-up ≤ 250 ms p95 after transport reconnection; no reset/duplicates |
| Indexed last-page read of 1 GiB recording on reference SSD | ≤ 200 ms p95; no full replay |
| Attach versus total history | cost bounded by checkpoint/replay/seed budget, not total file size |
| Sustained reference workload, recording + one client | ≥ 20 MiB/s plain and ≥ 5 MiB/s representative VT-heavy output; characterize slower renderers separately |
| 1 controller + 9 observers, one stalled | controller p95 within 20% of no-stall 10-client baseline |
| Stalled readers/background tabs | configured memory ceiling, no growth with stall duration/total recording |
| Soak | 24 hours mixed start/attach/resize/reconnect/retention without unexplained heap/thread/FD growth or cursor gaps |

Initial scale: 50 mostly idle sessions, consistent with current product scale. Start with ≤ 64 MiB incremental default budget per active session including bounded terminal/history/cache; validate aggregate/global admission limits. Client pending queues obey byte budgets regardless of renderer memory. Measure idle CPU/wakeups and remove per-attachment polling growth.

Test 0/20/100 ms RTT, jitter/loss, slow disk, blocked PTY input, CPU contention, and floods. Under overload specify degradation; infinite output cannot be consumed without bounded backpressure, buffering, or explicit gaps.

### 11.3 Operational visibility

Expose roles/controller generation, geometry version, head/journal/durable/earliest cursors, retained/queue bytes, lag, resync count, parser faults, input rejection/partial writes, persistence health.

Low-overhead counters/histograms and sampled tracing; no per-byte debug logs or secrets. diagnostic export includes protocol/profile/config/build and redacted health. Traces must distinguish scheduling, parsing, journaling, network, and rendering time.

## 12. Test and verification plan

### 12.1 Terminal conformance corpus

Extend existing `tests/e2e_pty.rs`, `tests/e2e_csvlens.rs`, Copilot/OpenCode recordings, and terminal guards; do not discard useful fixtures.

- Shell/readline, REPL, less, vim/neovim, top/htop, fzf, csvlens, representative agent CLIs.
- Nested SSH/tmux where available, without network-service dependencies in normal CI.
- Main/alternate transitions, attach on alternate, inactive main history, saved cursor/margins/tabs/wrap-pending, synchronized output.
- Truecolor/256-color, hyperlinks/default colors/cursor/title/progress/integration.
- Wide/combining/emoji/ZWJ/box drawing, invalid UTF-8/NUL, unbroken lines, oversized/unfinished controls.
- Supported modifiers/function keys/keypad, literal prefix, Ctrl-V paste/Shift+Insert paste/Ctrl-D detach/Escape, paste/CRLF/binary, mouse/focus/IME.
- Multiple queries per record, queries between cursor moves, detached queries, full input queue, exactly one response with 10 observers.
- Shrink/grow/drag, alternate-buffer resize, reconnect/continuation at every relevant state boundary.

Primary oracles: cells/state and application-observed input, not only textual goldens/screenshots. Same-terminal visual checks cover flicker/cursor/scrollbar/selection/paint. Do not use the implementation engine as its sole oracle; independent reference fixtures and manual platform checks are necessary.

### 12.2 Properties and fault injection

Use deterministic scheduling barriers/property tests, not sleep-only race tests:

- Every chunk split, including UTF-8/ESC/control strings.
- Snapshot at each output/append/publication boundary; exact C/C+1 handoff.
- Duplicate/lost/delayed/coalesced/fragmented frames and missing state events.
- Stale modes, resize interleaving, cancelled snapshot, output versus exit.
- Slow clients/tabs, disk full, short/partial writes, missing segments, corrupt/torn tails, interrupted checkpoint/manifest install.
- Retention concurrent with readers/attach/search/restart.
- Takeover with queued input, lease expiry, stale fencing token, uncertain acknowledgments/dedup-window expiry.
- Stalled federation stream while another sends input/heartbeats; old disconnect cleanup racing replacement.
- Repeated attach/detach without duplicated history, reset midway through restore.

Fuzz frame decoders, terminal policy, checkpoint/journal loaders. Use concurrency-model checking where appropriate for sequencing/leases, backed by end-to-end tests.

### 12.3 CI/release validation

- Rust unit and integration suites on Linux/macOS/Windows; preserve needed Windows PTY serialization.
- Format/clippy, frontend typecheck/lint/unit/build, protocol vectors, dependency/security/license checks.
- Playwright against actual daemon; Chromium/Firefox/WebKit coverage per support policy, including attach/reconnect/history/control, not only CRUD.
- Trigger on manifests/locks/migrations/`build.rs`/schemas/tests/workflows; locked Cargo and `npm ci`.
- Reproducible benchmarks and latency/soak on stable machines, not precise thresholds on noisy shared CI.
- Archive failure recordings/traces/screenshots/benchmark metadata without secrets.
- Verify Cargo/npm/Homebrew/Windows artifacts/installers and embedded frontend/protocol consistency.

## 13. Implementation milestones and exit gates

Develop vertical slices behind development-only switches, then delete replaced code. Do not ship two supported stacks. This is dependency ordering, not a calendar promise; engine and restoration feasibility are the biggest uncertainty.

### M0 — Evidence and contract (P0)

**Deliver**
- Direct-versus-oly timing harness and deterministic input/terminal fixtures.
- Reproductions for snapshot overlap, retention cursor reuse, resize history loss, query/mode ordering, multi-client geometry.
- Inventory of capabilities, CLI/API/config/auth, 0.x data variants.
- Engine/checkpoint/native+xterm restoration and raw/event-driven input prototypes on supported platforms.
- ADRs for terminal/profile, sequencing/journal, controller geometry, durability, fidelity matrix.

**Exit:** measured baseline; failing reproductions of highest-risk problems; demonstrated continuation after restore on both buffers; selected engine or funded dependency fix. No wholesale rewrite before this gate.

### M1 — Stable journal and ordering (P0; after M0) ✅ **(shadow-journal form)**

**Deliver** sequence/incarnation types, sequencer, immutable publication, bounded cache; segments/checksums/fixed-range reads; availability/durability cursors; checkpoint/recovery/non-resetting retention; ordered resize/end; off-loop history work; persistence failure state.

**Exit:** I1/I3/I8/I10 and crash/retention faults pass; bounded tail/seek; disk stall cannot grow memory indefinitely. One internal read API serves live/completed history.

**Status (complete).** The journal is canonical and always on: contiguous ordered stream of raw PTY bytes, resize, Policy, and lifecycle records; bounded parts with torn-tail recovery; cursor-validated reads; retention gated on anchored checkpoints; fault-injection coverage for torn tails, disk-full, and retention races.


### M2 — Terminal/input fidelity core (P0; after M0, integrates with M1)

**Deliver** engine/profile/checkpoints/query broker; capability/security policy; real resize preservation; raw/semantic codecs/paste/acknowledgments; event-driven CLI input; Windows VT validation and isolated remaining adaptations.

**Exit:** declared core direct-run corpus passes, including query positions, raw bytes, alternate return, narrow/wide history. No artificial ordinary-key timer. Unsupported extensions negotiated/documented.

**Status (complete).** One embedded alacritty engine answers every terminal query in stream order; versioned anchored checkpoints (OJCK v2) gate retention; attach input is event-driven and byte-exact (protocol v13); `map_key_to_input` implements the xterm-compatible profile; detach is Ctrl-D; Ctrl-V and Shift+Insert are clipboard paste shortcuts; mouse/focus/SGR modes propagate to attach clients.


### M3 — Unified attach/resume/control (P0; after M1/M2)

**Deliver** shared attach state machine, binary framing/generated TS/vectors; C/C+1 snapshot barrier, applied cursors/credits/chunked init/gaps/cancel/end; attachment registry/fenced leases/handoff; controller/fixed geometry and observer viewports; thin IPC/WS adapters.

**Exit:** I2/I4/I5/I6/I7 hold under adversarial scheduling; 10 mixed clients stay consistent through resize/takeover/lag/reconnect. No input guessing/retry or inferred log offsets.

**Status (complete).** One `AttachPump` state machine serves every attach behind thin IPC/WS adapters; cursor-checked streaming with incarnation fencing and sealed-part manifests; control lease with observer gating and fenced takeover; enforced credits with loud stall disconnects; manifests are verifiable.


### M4 — History UX and agent workflows (P0; after M3)

**Deliver** fresh/resume CLI/browser flows; extracted browser controllers; anchored pageable history/isolated replay/selection/mobile/IME; screen/history/recording APIs; cursor wait/events/JSON exit codes; notification evidence and human-agent handoff.

**Exit:** scroll-up is not stolen, attach does not duplicate/truncate retained history, input remains responsive during history load, agents observe/handoff/resume without stealing control or equating idle with success.

**Status (complete).** Agent surfaces (`history`/`screen`/`wait`/`control`, lease-gated `send`) over cursor-checked RPCs; browser `HistoryController` with loud anchor errors; scroll-theft fix.


**Status amendment (complete).** The post-M4 audit corrective increment landed: journal coherence (manifest tombstones, sealing on open), persisted agent surfaces across restarts, control-lease TTL/caps, and the shared WS-frame fixture pinning the protocol from both sides.


### M5 — Federation/security/lifecycle parity (required for 0.5.0; after M3, parallel M4)

**Deliver** direct remote stream adapters/credits/fairness/deadlines/cancel/connection fencing; revocable auth/scopes/origin/proxy isolation; process-tree/drain/shutdown/reconciliation; safe export/upload/privacy; current security audit.

**Status (complete).** Byte-bounded queues with enforced credits; direct remote attachment streams with fencing, deadlines, and keepalives; principal-bound authz with scoped API keys and optional SSH key authentication, Origin checks, and proxy credential isolation; process-tree kill with staged escalation and signal-driven drain; privacy-by-construction log/export defaults (security audit addendum in docs/SECURITY_AUDIT_REPORT.md).


**Exit:** local/remote share conformance suite; stalled history does not stall control; revoked controllers cannot write; supported cleanup leaves no leaked managed processes/resources.

Status (M6, complete): vt100 retired as a runtime parser (test-only conformance oracle); legacy persistence (`output.log`/`events.log`, sidecar index) deleted — pre-0.5 sessions fail loud; binary attach framing on IPC; MIGRATION.md; human-first ARCHITECTURE.md. A post-release review hardening pass (protocol v13) added byte-exact input, mode propagation, checkpoint-anchored bounded replay (`SegmentStream`), fail-loud web frame parsing, a zero-warning clippy gate, and full CI gates (fmt/clippy/test on 3 OSes, web lint/format/test/build, Playwright, cargo audit).


**Status (post-release review hardening; complete).** Folded into the M6 status above: protocol v13, zero-warning clippy gate, CI hardening.


### M6 — Debt removal and release (P0; after M1–M5)

**Deliver** removal of obsolete framing/base64 hot paths/unsequenced broadcasts/last-resizer/string rewriting/paste timers/destructive retention/lossy restore fallbacks; remove parser duplication and per-attach status polling; full platform/fault/security/performance/soak evidence; import/export/upgrade guide; current docs and distribution validation.

**Exit:** §16 checklist passes with evidence, one supported 0.5.0 stack remains, no hidden release-blocking fidelity exceptions.

Dependency spine: **M0 → M1 + M2 → M3 → M4 + M5 → M6**. Test infrastructure and security review start in M0 even where their final gates are later.

### After 0.5.0 unless evidence promotes the work

- Session hosts for process-surviving daemon restart/upgrades.
- Async Unix PTY if scheduling/thread budgets demand it.
- Advanced graphics with complete restore/history/security/resource semantics.
- Shared simultaneous input, more geometry policies, richer team administration.
- Dedicated bulk transports/QUIC, distributed archive, advanced recording search, optional MCP.

## 14. Breaking changes and data transition

Clean 0.5.0 is preferable to permanent 0.3.x compatibility, but upgrade must not destroy recordings.

1. **Preflight:** identify state format, active sessions, free disk, credentials, node versions. Require finishing/stopping 0.x sessions; live PTYs cannot transfer by migrating metadata.
2. **Backup/export:** metadata, launch parameters, artifacts, available `output.log/events.log`, checksums. Untouched backup outside new writable state.
3. **New namespace:** initialize explicit new format in a separate location that 0.x does not automatically open; 0.5.0 readers reject unsupported format versions. Never point an old binary at new state. Convert to temporary directory and atomically install after validation.
4. **Offline import:** optionally wrap legacy filtered bytes/known resize records as a provenance-labeled legacy recording. Not original raw PTY. Stripped sequences, unknown timing, truncations, lost history, ambiguous resize offsets cannot be recovered. Do not fabricate faithful checkpoints.
5. **Protocol break:** CLI/daemon and federation major versions must match or give precise upgrade errors. Coordinated federation upgrade or isolated deployments, no permanent bridge.
6. **Config/API break:** version config, reject unknown/deleted keys with replacements; define hot/restart-required fields; document command/exit-code changes; generate references.
7. **Credentials:** require re-login and rotation/reissue of scoped credentials where needed. Do not retain insecure sessions for convenience.
8. **Rollback:** stop 0.5.0, preserve its data separately, restore original 0.x binary/backup. No promise old binary can read/merge new journal.

Old/new development paths may coexist only for comparison/migration tests; M6 deletes them.

## 15. Decisions, risks, and scope control

| Risk / decision | Handling |
|---|---|
| Daemon/native/browser disagree on subtle VT | M0 differential prototype/profile; continuation tests, not just matching cells; highest technical risk. |
| Checkpoint/restore unavailable in candidate APIs | Require maintainable adapter or bounded reconstruction before engine choice; no assumed universal ANSI serialization. |
| Transparency conflicts with queries/security | Original journal plus centralized queries and explicit safe display policy; never blind multi-responder passthrough. |
| Double input encoding | Distinct wire types/codec ownership; raw bypasses transforms; pasted escape-like data tests. |
| Off-loop journal hides loss | Availability/durability cursors, bounded cache, explicit degraded/failure state and fault tests. |
| Retention removes reconstruction dependencies | Require validated retained checkpoint/versioned projection before segment deletion; expiring pins. |
| Resize cannot infer OS/application timing | Consistent observed order and real-app tests, no impossible atomicity claim. |
| Sequencer becomes bottleneck | Batch ready data, immutable publication, off-loop serialization/index, profile crossings; ordering remains mandatory. |
| Scope becomes a terminal/multiplexer rewrite | Existing engines, one PTY, current scale, no services; defer host/graphics/transport expansion absent evidence. |
| Old tests encode old bugs | Replace obsolete behavior assertions under new invariants; do not weaken fidelity to retain green tests. |
| Targets turn anecdotal | Store traces/conditions, ratify in M0, explicit evidence for revisions. |

ADRs to finish in M0–M1:

1. Engine/profile/Unicode/restoration representation.
2. Event order, journal/cursors, durability, retention dependencies.
3. Raw/semantic codec, acknowledgment, leases, geometry.
4. Framing/negotiation/credits/federation scheduling.
5. Browser/native complete-history UX versus finite host buffers.
6. Crash boundary and deferred process-surviving upgrades.
7. Auth scopes/sessions/proxy isolation/terminal side effects.

Each ADR records rejected alternatives, acceptance tests, migration implications, and an assigned owner. Each ticket names affected paths, invariant IDs, tests, and the old code it deletes.

## 16. 0.5.0 release checklist

### Fidelity and interaction

- [ ] Declared profile passes direct-versus-oly tests on supported Linux/macOS/Windows environments.
- [ ] Keys/modifiers/function keys/Ctrl-D/Ctrl-V/Escape/paste/mouse/focus/keyboard negotiation work without timing-based suppression.
- [ ] Queries use exact ordered state and answer once regardless of observers.
- [ ] Both buffers, saved state, colors/styles/cursor, Unicode, and history survive attach/reconnect/resize under the profile.
- [ ] No silent destructive parser reset or repaint workaround substitutes for supported behavior.

### Streams, history, ownership

- [ ] Snapshot/resume survive adversarial scheduling without missing/duplicate events.
- [ ] Retention never resets identity; missing/corrupt/incomplete history is explicit.
- [ ] Tail/seek/page are checkpoint/index-bounded, not total-recording-bounded.
- [ ] History scroll/selection persist; output does not force follow mode.
- [ ] Mixed clients have clear controller, stable geometry, fenced handoff, no observer input side effects.
- [ ] Slow clients/tabs/federation remain memory-bounded and cannot stall unrelated control.
- [ ] Rejection/partial/unknown write and retry semantics are tested/documented.

### Reliability, security, performance

- [ ] Crash/disk-full/partial-write/retention recovery preserves truthful durability boundaries.
- [ ] Exit drains output or declares incomplete capture; stop/kill/shutdown are idempotent/platform-tested.
- [ ] Auth/revocation/scopes/Origin/CSRF/IPC permissions/replay/proxy/privacy pass current security review.
- [ ] Ratified latency/throughput/resource and 24-hour soak budgets pass with benchmark conditions published.
- [ ] CI covers unit/integration/frontend/E2E/protocol/fault tests and release inputs.

### Maintainability and delivery

- [ ] One supported terminal policy, journal model, attach state machine, stream protocol; obsolete 0.x paths deleted.
- [ ] Architecture/protocol/config/commands/edge-case and human-agent docs agree with release code.
- [ ] Backup/import/upgrade/rollback tested with real legacy recordings and interrupted conversion.
- [ ] CLI/daemon/browser/node mismatch errors actionable; all package channels install matching artifacts.

**Release rule:** no known silent input loss, attach-boundary corruption, destructive retained-history resize, ambiguous cursor reuse, or unbounded client queue is accepted as a documented limitation. Those defects undermine the reason to use oly and block 0.5.0.
