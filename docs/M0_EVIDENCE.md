# M0 evidence pack (in progress)

Baseline for the 1.0.0 plan ([PLAN.md](../PLAN.md) §13, milestone M0). This
file accumulates the measurements and reproductions the M0 exit gate
requires. Nothing here is a latency/fidelity guarantee; it is recorded
evidence with machine context.

Machine context for all numbers below:

- Linux 6.18 (WSL2), x86_64, Intel Core Ultra 7 265H, 16 cores
- Baseline commit `931d7b9`, crate 0.3.3, `cargo test --locked --offline`
- Release-profile probes use the shipped profile (`opt-level = "z"`, fat LTO)

## Test baselines

- Rust unit suite: **504 passed, 0 failed, 15 ignored** (was 491 before M0
  work; new: journal record layer, bounded replay-read regression, engine
  continuation probes, input-codec probes, latency probes,
  reproductions).
- Web suite: **112 passed** (run during planning; not re-run for this
  Rust-only change).

## Defect reproductions (all currently FAIL under `--ignored`, by design)

| Test | Defect | Plan reference |
|---|---|---|
| `session::runtime::tests::repro_queries_are_answered_at_their_own_stream_position` | One read carrying `A`, CPR, `B`, CPR gets two identical replies computed from the final chunk cursor | I5, §5.4 |
| `session::runtime::tests::repro_snapshot_boundary_can_overlap_live_stream` | `push_output` → append → broadcast are separate steps; a snapshot taken mid-sequence contains bytes the returned resume offset does not cover, so replay duplicates them | I2, §7.2 |
| `session::persist::tests::repro_truncated_log_reuses_offsets` | `truncate_output_log` resets the file; stale cursors silently alias the new stream or read as empty EOF | I3, §6.2 |
| `session::screen::tests::repro_shrink_then_widen_preserves_history` | Resize re-feeds width-trimmed scrollback; shrink then widen permanently loses history text | §5.3, §6.4 |
| `session::screen::tests::repro_restore_continuation_after_partial_escape` | An escape sequence split at the snapshot boundary is lost: its tail renders as literal text after restore | ADR-0001, §5.3 |
| `session::screen::tests::repro_restore_continuation_after_partial_utf8` | A multi-byte UTF-8 character split at the snapshot boundary is corrupted after restore | ADR-0001, §5.3 |
| `session::screen::tests::repro_restore_continuation_preserves_alt_screen_residency` | Alternate-screen residency does not survive restore: the TUI frame is painted onto the main buffer, and a later `\x1b[?1049l` reveals it instead of the preserved main screen | ADR-0001, §5.3 |
| `client::attach::tests::repro_alt_char_is_esc_prefixed` | Alt+<char> is sent as the bare character — every Alt chord corrupts | ADR-0003, §8.2 |
| `client::attach::tests::repro_ctrl_arrow_is_parameterized` | Modifiers on arrow keys are dropped, so chord bindings never fire | ADR-0003, §8.2 |
| `client::attach::tests::repro_ctrl_digit_family_sends_legacy_control_bytes` | Ctrl+2..8 send `c & 0x1f` instead of the legacy NUL/ESC/FS/GS/RS/US/DEL bytes | ADR-0003, §8.2 |
| `client::attach::tests::repro_function_keys_are_mapped` | Function keys return `None` and are silently dropped | ADR-0003, §8.2 |

Engine-eval findings that PASS (incumbent capability baseline for
ADR-0001): committed text/styles/cursor survive `state_formatted`
snapshot/restore, and the wrap-pending flag survives too
(`restore_continuation_matches_reference_for_committed_text_and_styles`,
`restore_continuation_preserves_wrap_pending`).

Input-codec findings that PASS (incumbent baseline for ADR-0003):
`map_key_baseline_documents_current_coverage` pins Enter, Backspace
variants, Ctrl+<char>, DECCKM arrows; `typing_is_held_for_the_paste_burst_window`
pins the fixed typing floor (below).

Not yet reproduced as failing tests:

- **Snapshot/broadcast subscription overlap** — partially covered by the
  snapshot-boundary repro above; the full C/C+1 race across subscribe needs
  the M3 attach state machine to express the desired behavior.
- **Multi-client geometry conflict** — the current resize API has no client
  identity or lease parameter, so the desired "observer resize rejected"
  assertion cannot compile yet. Captured as a design defect in
  [ADR-0003](adrs/ADR-0003-input-control-geometry.md).

## Fixed during M0

- `read_output_from` no longer reads past the captured EOF: a concurrent
  append between the seek and the read could return bytes beyond the
  reported end offset (PLAN §2.2). Regression test:
  `session::persist::tests::read_output_from_never_returns_bytes_beyond_the_captured_eof`.

## Hot-path baseline (M0 measurement probes, `--release --ignored`)

| Probe | Result | Plan target (§11.2) |
|---|---|---|
| `probe_scan_throughput_plain_text` | ~12 000 MiB/s | ≥ 20 MiB/s end-to-end |
| `probe_scan_throughput_vt_heavy` | ~500 MiB/s | ≥ 5 MiB/s end-to-end |
| `m0_probes::probe_direct_pty_echo_roundtrip` | p50 58 µs, p95 140 µs, p99 284 µs (n=300, `cat` echo) | reference floor |
| `m0_probes::probe_daemon_backend_echo_roundtrip` | p50 100 µs, p95 183 µs, p99 313 µs (n=300) | backend input→PTY p95 ≤ 2 ms; output→client p95 ≤ 5 ms |

Conclusions so far:

- The scanner fast path is orders of magnitude above the end-to-end
  targets — not a bottleneck.
- The daemon backend (writer queue → PTY → echo → scan → screen → log
  append → broadcast wakeup) adds only ~40 µs at p50 over a direct PTY on
  an idle machine — comfortably inside the §11.2 backend budgets. This
  probe excludes IPC framing and client rendering by design.
- The interactive-latency debt therefore concentrates where the plan
  predicted: client-side waits (`PASTE_BURST_WAIT` 30 ms, the 150 ms
  paste-suppression window, the 60 ms server-frame wait in the CLI attach
  loop) and browser render scheduling. Client-side probes are the next
  measurement target.

Client-attach latency analysis (code-reading evidence, `src/client/attach.rs`):

- **Every paste-candidate key — ordinary characters, Enter, unshifted
  Tab — enters the burst buffer, and each follow-up candidate resets the
  30 ms `PASTE_BURST_WAIT` deadline** (event loop at
  `attach.rs:390`–`400`; `push_pending_key_burst`). A single key waits
  ~30 ms; continuous typing faster than one key per 30 ms accumulates and
  reaches the wire only when the user pauses for a full burst window —
  normal typing, not just pastes, carries a fixed client-side floor
  ~300× the daemon backend round trip. Pinned by
  `typing_is_held_until_a_full_pause_in_the_paste_burst_window`.
- **The 150 ms `PASTE_KEY_SUPPRESS_WINDOW` silently drops input, not
  output**: after a clipboard paste, `should_suppress_paste_followup_key`
  `continue`s past incoming `Event::Key` values for Char/Enter/Tab/
  clipboard-paste keys before they are ever sent — legitimate fast
  follow-up typing after a paste is input loss by omission (violates the
  no-silent-input-loss invariant).
- The 60 ms `tokio::time::timeout` on `frame_rx.recv()` (`:457`) is a
  ceiling that keeps the input loop live, not a floor. Neither the frame
  timeout nor the suppression window explains the typing floor; the burst
  buffer does.

## M1 groundwork landed

- `src/session/journal.rs`: typed record format (`OJRN`, version, kind,
  seq, elapsed ms, CRC-32), segment writer with explicit `sync()`, scanner
  with torn-tail recovery, sequence-continuity enforcement, and
  allocation-bounded payload lengths. Event timing is assigned by the
  caller (the future sequencer), never by the writer, so reopening a
  segment cannot move event time backwards. Scan results distinguish a
  rewound torn tail (`ScanStop::PartialTail`) from quarantinable
  corruption (CRC/sequence/header/version/kind/length stops).
  Unit-tested (round-trip, torn tail, CRC corruption, sequence gap,
  garbage length, oversize rejection, caller-assigned timing).
  Not wired into the runtime yet — the sequencer (M1) will own it.

## Remaining M0 work

- [x] Daemon-backend latency probe versus direct PTY floor (results above).
- [ ] Full key-to-echo through the IPC attach client, instrumented end
      to end (the daemon probe measures in-process; the client-side
      **analysis** above identifies the 30 ms burst-buffer floor as the
      dominant added latency, but an instrumented run has not been
      captured).
- [x] Engine continuation evaluation against incumbent vt100 (partial
      escape/UTF-8 boundary and alternate-screen failures pinned;
      committed-state and wrap-pending passes recorded).
- [x] Input-codec coverage evaluation against incumbent
      `map_key_to_input` (Alt-chord, modifier-arrows, Ctrl+digit family,
      function keys pinned; typing-floor analysis recorded).
- [x] Terminal engine alternatives comparison — see
      [ENGINE_EVAL.md](ENGINE_EVAL.md): `alacritty_terminal 0.26` agrees
      with the incumbent oracle on clean streams, loses 0/40 history lines
      on shrink→widen (incumbent loses 29/40), and has a bounded
      checkpoint patch path; the incumbent's `state_formatted` strategy
      fails 73/803 byte-boundary splits. WezTerm deferred (not published
      to crates.io). Alacritty core is the selected engine candidate
      (ADR-0001); final ratification awaits the M2 checkpoint prototype.
- [ ] Raw/event-driven input prototype per platform (ADR-0003).

## M1 progress (post-gate)

- Increment 1 (superseded by increment 2): a fused `Sequencer` owned
  both sequence allocation and the segment writer. Replaced in
  increment 2 — sequence allocation is now in-memory (`SequencerCore`)
  and disk writes belong to a bounded `JournalAppender` thread, with
  incarnation recovery in `journal::open()`.
- Wired as a **shadow journal** behind dev-only `OLY_JOURNAL=1`: the PTY
  reader thread journals every canonical filtered output chunk it also
  appends to `output.log`. Failures log and do not affect the session.
  Ordered resize/lifecycle records and cursor consumers land next.
- Verified: unit tests (monotonic seq/elapsed, incarnation reopen,
  torn-tail rewind, corruption reporting) + `probe_shadow_journal_records_output_in_order`
  (ignored dev probe; real PTY session, clean scan, contiguous seqs).
  Suite: 508 passed, 16 ignored.

### Increment 2: ordering split from persistence

- The fused `Sequencer` was split per PLAN.md §4.2 before any further
  record types were wired, so the wrong abstraction never hardens:
  - `SegmentWriter` is now a pure serializer (`append_record` takes a
    caller-assigned `seq`); it no longer allocates sequences.
  - `journal::open()` performs incarnation recovery (torn-tail rewind,
    corruption reporting) and returns the new incarnation's writer.
  - `SequencerCore`: in-memory sequencing authority — allocates
    monotonic `seq` + `elapsed_ms` **before publication**, keeps a
    byte-bounded recent replay cache that may only evict journaled
    records (never silently drops; `over_budget()` signals
    backpressure), tracks `head_seq`/`journal_seq`/`durable_seq`
    separately, and has an explicit sticky `degraded` state.
  - `JournalAppender`: dedicated thread, sole owner of the segment
    writer. Queue bounded in messages (256) **and** bytes (32 MiB) — a
    stalled disk cannot grow session memory. Validates contiguity
    (`expected_seq`), acks `Journaled`/`Durable` cursors, group-syncs on
    request, and after any failure (I/O or contiguity) is dead and
    fails fast instead of writing past a hole.
  - `OrderedEvent { cursor: JournalCursor { incarnation, seq },
    elapsed_ms, kind, payload: Bytes }` — one immutable allocation
    shared by live delivery, replay cache and journal queue.
- The reader-thread shadow wiring (`OLY_JOURNAL=1`) now goes through
  `ShadowJournal` (core + appender): the PTY reader no longer performs
  synchronous disk writes.
- Verified: 19 journal tests including the appender boundary tests
  (in-order acks, contiguity violation kills the appender without a
  hole, queue byte/message budgets reject without dropping, eviction
  never precedes journal availability, sticky degradation) + the real-PTY
  shadow probe re-run. Suite: 515 passed, 16 ignored; clippy baseline
  unchanged (81 with `--all-targets`).

### Increment 3: ordered resize/lifecycle through one sequencing point

- Typed payload codecs in `journal.rs`: resize (`rows|cols u16 LE`,
  zero geometry rejected) and lifecycle (`code u8 | exit_code i32 LE,
  `i32::MIN` = absent | detail UTF-8`; `LifecycleCode` =
  Started/Stopped/Killed/Failed). Round-trip and malformed-input tests
  included.
- The `ShadowJournal` moved from the reader thread into
  `SessionRuntime.journal: Option<Mutex<ShadowJournal>>` — one
  sequencing point per session. The mutex only covers in-memory
  sequencing + bounded non-blocking submit, so it can be taken while
  the runtime write lock is held: that lock now orders mutation,
  sequencing and publication against each other (PLAN.md §4.1 item 4).
  Output is journaled inside the same write-lock section as
  `push_output`; `resize_pty` journals the geometry change after the
  mutation succeeds; `mark_completed` journals the end fact exactly
  once (`newly_ended` guard); `spawn_session` journals initial
  geometry + `Started`. Failure handling: degrade-and-log-once.
- Verified: codec round-trips/malformed rejection, mixed-kind ordering
  test (resize/lifecycle/output in one stream), runtime test that
  `mark_completed` journals the end fact exactly once, and the extended
  real-PTY probe asserting the full order: `Resize(24,80) → Started →
  JRN-AAAA → JRN-BBBB → Resize(40,120) → JRN-CCCC → … → Killed` with
  contiguous seqs and a clean scan. Suite: 519 passed, 16 ignored;
  clippy baseline unchanged (80 with `--all-targets`).

### Increment 4: fixed-range reads + group-sync cadence

- `journal::read_range(session_dir, incarnation, from_seq, to_seq,
  max_bytes)` (I3): the internal fixed-range read API. Returns the
  in-window records contiguous and in order; validates integrity and
  continuity of the whole consumed **prefix** (corruption before the
  window surfaces instead of presenting a hole); stops early once the
  window completes; the byte budget always includes the first in-window
  record so callers can always make progress, and `truncated` tells the
  caller to resume at `last_seq + 1`. On a live segment
  `ScanStop::PartialTail` simply means "append in progress".
- The appender now group-syncs on a cadence (default
  `DEFAULT_SYNC_INTERVAL = 50ms`, ADR-0002's deferred 20/50/100 decision
  — configurable via `spawn_with_sync_interval` /
  `ShadowJournal::open_with_sync_interval` for the eventual probe), so
  `durable_seq` advances in production without per-record `fsync` on the
  ingest path. Explicit `request_sync` remains for immediate flushes.
- Verified: window exactness, past-end clamping, invalid-arg/NotFound
  rejection, budget truncation + resume, prefix-corruption surfacing on
  out-of-window reads, and cadence-driven `durable_seq` progress with no
  explicit sync. Suite: 524 passed, 16 ignored; clippy baseline
  unchanged (80); real-PTY shadow probe re-run.
- [x] Capability/CLI/API/config/auth inventory for the compatibility break
      — see [M0_INVENTORY.md](M0_INVENTORY.md).
- [x] ADR ratification at the M0 exit gate: ADR-0002/0003/0006/0007
      Accepted on the gathered evidence (direction; their acceptance
      repros still gate their milestones). ADR-0001 direction accepted
      (Alacritty candidate), final selection awaits the M2 checkpoint
      prototype. ADR-0004/0005 remain Proposed pending M3/M4 prototypes.
