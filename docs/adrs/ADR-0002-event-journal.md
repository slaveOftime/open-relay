# ADR-0002: Event order, journal, cursors, durability, retention

- Status: Accepted. M1 ships the journal as a shadow writer (gated by
  `OLY_JOURNAL`); `output.log` remains canonical until M3. Record format v1
  is provisional until then.
- Plan reference: PLAN.md §4, §6 (invariants I1, I2, I3, I8, I10)

## Context

`output.log` is a flat byte file. Offsets double as cursors, but
`truncate_oversized_logs` resets the file to zero and reuses them
(`repro_truncated_log_reuses_offsets`). The reader updates screen state,
appends, and broadcasts as three separate steps, so snapshot/file/broadcast
have no single atomic boundary. Persistence failure logs a warning while
clients keep receiving bytes the log will never contain.

## Decision (draft)

1. One per-session sequencer assigns a monotonic `seq` to every event
   (output, successful resize, lifecycle, policy) before publication. The
   durable cursor is `{session_id, incarnation, seq}`; `seq` is never
   reused **within an incarnation**, and daemon restart opens a new
   incarnation rather than appending past possibly-published sequences.
   The sequencer also owns event timing (`elapsed_ms` since incarnation
   start) — the segment writer serializes what it is given and never
   invents timestamps.
2. Records live in bounded segments with per-record CRC and continuity
   checks (`src/session/journal.rs` implements the record layer). One
   incarnation spans bounded **parts** (`seg-NNNNNNNN-PPPP.ojrn`,
   default 64 MiB) with a continuous sequence, so cursors never name
   parts; only the newest part can be torn by a crash (older parts are
   sealed with fsync + dir-fsync at rollover), so recovery scans only the
   newest part — O(part), never O(history). Recovery distinguishes a torn
   active tail (`ScanStop::PartialTail` — rewind to `valid_len`) from
   corruption (`ScanStop::{CrcMismatch, SequenceDiscontinuity,
   InvalidHeader, UnsupportedVersion, UnknownKind, OversizeLength}` —
   quarantine and report, never silently truncate). Tail reads stay
   bounded by a sparse (1 MiB stride) per-part index and offset seeks;
   sealed-part corruption fails loudly instead of truncating.
3. Retention deletes whole sealed incarnations only, after a retained
   checkpoint reconstructs the first exposed boundary. Sequences are
   never reused; obsolete cursors get `HistoryExpired { earliest_cursor }`.
   The checkpoint gate landed in **M2-3** (checkpoint records land with
   the engine integration): `journal::retain_before` refuses to delete
   below the newest checkpoint's incarnation and deletes nothing when no
   checkpoint exists; the test-only unchecked primitive remains
   `retain_before_unchecked`.
   Deletion is newest-part-first so a concurrent reader can observe a
   truthful prefix of a being-deleted incarnation, never a hole; part
   numbering is validated contiguous from 1 on every read. Exact
   reader/retention coordination is M3 manifest work.
4. Cursor validity is checked against the **recovered valid tail**: a
   cursor past it (e.g. into a torn-away region) fails loudly with
   `InvalidData` ("incomplete capture") rather than silently sliding into
   the next incarnation's bytes; a cursor into an expired incarnation
   fails with `NotFound`.
5. Track `head_seq` / `journal_seq` / `durable_seq` / `earliest_seq`
   separately. Live publication is low-latency from bounded memory; group
   sync advances durability (measure 20/50/100 ms cadences in M1 before
   choosing the default). Persistence failure degrades explicitly: bounded
   buffer, then this session's backpressure, then explicit capture
   failure — never a silent gap by default.

## Rejected alternatives

- Keeping file offsets as cursors (ambiguous under retention).
- Synchronous `fsync` per chunk on the reader hot path (latency).
- Unbounded fire-and-forget journal queue (memory growth hides loss).

## Acceptance

- `repro_truncated_log_reuses_offsets` passes un-ignored against the
  journal-backed read API. **Met (M1)** by
  `journal::tests::history_cursors_never_alias_across_restart_and_retention`:
  `(incarnation, seq)` cursors never alias after restart, and retention
  expiry fails loudly with `NotFound`. The 0.x reproduction stays ignored
  until `output.log` retires in M3.
- Crash-fault tests: torn tail recovery, retention concurrent with readers,
  disk-full degradation — all without cursor reuse or silent holes.
  **Met (M1, corrective increment)**: torn-tail rewind
  (`reopen_rewinds_a_torn_tail_before_starting_the_new_incarnation`),
  rollover sequence continuity across parts
  (`rollover_keeps_one_sequence_across_parts`), cursor-beyond-tail loudness
  incl. after a rewind (`cursor_beyond_recovered_tail_fails_loudly`), a
  200-iteration reader/retention race
  (`retention_concurrent_with_readers_never_shows_a_hole`),
  disk-full degradation (`disk_full_degrades_the_appender_without_a_hole`,
  `/dev/full`), the sync-deadline starvation test
  (`sync_deadline_fires_under_a_continuous_producer`), the shutdown final
  barrier (`shutdown_syncs_unsynced_records`), bounded memory after
  degradation (`degraded_journal_stops_growing`), and the O(1)-memory
  stats recovery scan (`stats_scan_counts_records_without_buffering`).
  The checkpoint-gated retention half is **deferred to M2**; M1 exposes
  only the test-only unchecked primitive.
- Group-sync cadence: measured by `probe_journal_sync_cadence_durable_lag`
  (M1): append→durable lag tracks the cadence (p95 ≈ cadence + ~1 ms at
  20/50/100 ms). Default is 50 ms (`DEFAULT_SYNC_INTERVAL`); revisit with
  production workloads in M3.

## Migration

0.x `output.log`/`events.log` import as provenance-labeled legacy recordings;
stripped sequences and ambiguous offsets are documented, not reconstructed.
