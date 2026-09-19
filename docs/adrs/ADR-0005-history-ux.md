# ADR-0005: Complete-history UX versus finite host buffers

- Status: Proposed
- Plan reference: PLAN.md §6.3, §6.4 (invariant I9)

## Context

Scrollback differs per path: the CLI seeds a bounded recent window into the
host terminal's scrollback, xterm.js holds its own 1000-line buffer, and the
browser flush scrolls to bottom unconditionally. Host terminal scrollback is
finite with no portable random access; treating it as the history store
caps retention and corrupts on reconnect/reseed.

## Decision (draft)

1. Three distinct observation surfaces with separate APIs: **screen**
   (current state at a revision), **history/transcript** (finalized
   main-screen logical lines with stable IDs and source ranges),
   **recording/replay** (original events with geometry/timing in an
   isolated engine).
2. Browser: live-follow is separate from history-browse. Follow only when
   already at bottom; preserve selection and anchor `(line_id,
   cell_offset)` across output, paging, reconnect, retention. Older pages
   load with row/byte caps into the live buffer or a styled virtualized
   surface — never private xterm buffer surgery.
3. Native CLI: fresh attach seeds a bounded configurable recent window once
   without clearing host scrollback; full retained history is an explicit
   `oly logs --from` view. Intact-renderer resume never reseeds; a restarted
   CLI takes a fresh snapshot.
4. Historical work (tail/seek/search/replay) runs off the sequencer,
   checkpoint-bounded, and never mutates live state, geometry, notification
   state, or user scroll position.

## Rejected alternatives

- Growing host scrollback seeds toward "complete" history (finite, lossy,
  unportable).
- Replaying the recording from byte zero for every tail/search (cost grows
  with total history).
- Treating alternate-screen frames as lossless text history (they are not).

## Acceptance

- Scroll-up is never stolen by output; selection survives paging/reconnect.
- Indexed last-page access on a 1 GiB recording meets the PLAN §11 target
  without full replay.
- Attach after retention shows an explicit gap, not silently missing lines.

## Migration

Legacy `output.log` imports carry provenance labels; heuristic 2,048-byte
index records are replaced by logical-line indexes rebuilt from the journal.
