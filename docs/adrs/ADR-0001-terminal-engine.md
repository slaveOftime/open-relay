# ADR-0001: Terminal engine, profile, and restoration

- Status: Proposed
- Plan reference: PLAN.md §5 (invariants I5, I11)

## Context

The daemon owns one PTY per session and renders it with `vt100`. Known
defects the 0.5.0 design must fix:

- Resize rebuilds the parser from formatted, width-trimmed rows
  (`safe_resize_parser`), permanently destroying retained history beyond the
  narrower width (`repro_shrink_then_widen_preserves_history`).
- Queries collected from a chunk are all answered with the final chunk
  cursor (`repro_queries_are_answered_at_their_own_stream_position`).
- The scanner strips most OSC/DCS traffic; snapshots come from
  `state_formatted()`, which is not a continuation-safe restoration stream.

## Decision (draft)

1. Keep one embeddable terminal engine behind a narrow `terminal/` adapter.
   Evaluate `vt100` against maintained alternatives (Alacritty/WezTerm-family
   cores, libvterm bindings) with the M0 conformance corpus before choosing;
   do not write an emulator from scratch.
2. Sessions get a stable profile at creation: TERM/terminfo, Unicode width
   version, colors, keyboard/paste/mouse/focus capabilities, query behavior.
   Capabilities never change when clients attach or detach.
3. The engine owns query responses, answered at each query's exact position
   in the ordered stream. Renderers never respond.
4. Internal checkpoints (both buffers, scrollback, modes, palette, cursor
   stacks, parser-boundary state, geometry/profile/schema, cursor, checksum)
   are separate from renderer restoration adapters for xterm.js and native
   terminals.

## Rejected alternatives

- Pass-through relay with per-client query answering (multi-responder
  corruption; capabilities swing with attachments).
- From-scratch emulator (scope explosion for a solved problem).
- Assuming xterm.js serialize addon output is a valid cross-engine restore
  stream (private buffer formats are not interchangeable).

## Acceptance

- `repro_queries_are_answered_at_their_own_stream_position` and
  `repro_shrink_then_widen_preserves_history` pass un-ignored.
- Continuation after restore on main and alternate buffers matches the
  direct-run oracle for the declared profile corpus.
- Every advertised capability has live, snapshot, and replay tests.

## Evidence gathered in M0

Incumbent `vt100` + `state_formatted` snapshot, restore-continuation
oracle (M0 measurements, recorded here):

- Passes: committed text/SGR/cursor; the wrap-pending flag survives too
  (a `state_formatted` replay encodes the last-column cursor position such
  that the next write wraps identically).
- Fails (pinned repros): escape sequence split at the snapshot boundary
  (tail renders as literal text); multi-byte UTF-8 split at the boundary
  (character corrupted); **alternate-screen residency lost** (after
  restore the parser is on the main buffer, so `\x1b[?1049l` reveals the
  TUI frame instead of the preserved main screen).

Conclusion so far: the incumbent screen replay is not a checkpoint; a
0.5.0 checkpoint must serialize parser state (pending escape/UTF-8
fragments, active modes incl. DECSET 1049, wrap-pending) alongside the
grid. Whether the incumbent engine can grow that checkpoint or a
replacement core is needed is the open M2 decision.

Dependency note (offline builds): the local cargo registry cache contains
`vte 0.15` (parser state machine only — no grid), `termwiz 0.23`, and
`wezterm-bidi`-family crates; the full alternative cores (`alacritty_terminal`,
`wezterm-term`) are **not** cached.

## Owner decision (M0 gate)

A **time-boxed evaluation of `alacritty_terminal` (lead) and
`wezterm-term` (comparison)** is authorized, via one online build or
vendored exact-version sources, in an isolated prototype crate — no
production dependency is added until the checkpoint/resize corpus passes.
`vt100` is not selected as the 0.5.0 engine unless both candidates fail and
a scoped fork is demonstrably cheaper; building a screen model on `vte`
is rejected as a de-facto from-scratch emulator.

## Evaluation outcome

Measured with a throwaway evaluation crate (removed after the decision; the
corpus now lives in `src/terminal/mod.rs` conformance tests):

- Clean-stream agreement with the incumbent oracle: exact on 6/7 corpus
  streams; the 7th is an API difference (logical vs physical wrapped
  lines), not a semantic divergence.
- Incumbent restore strategy, quantified: **73 of 803 byte-boundary
  splits (9.1%) corrupt the continuation**, including every split during
  alternate-screen residency — formatted-screen replay is not a
  checkpoint.
- Non-destructive resize: alacritty's native reflow lost **0/40** history
  lines on shrink→widen where oly's vt100 rebuild lost **29/40**.
- Checkpoint feasibility: alacritty's `Grid<Cell>` (both buffers) is
  serde-serializable today; `Term` needs a bounded serde patch (~8–10
  derives) and partial-input state in `vte::Processor` needs either the
  same treatment or a ground-boundary + retained-raw-bytes strategy
  (M2 decision). `wezterm-term` is not published to crates.io and is
  deferred unless the Alacritty patch path proves prohibitive.

**Direction: Alacritty's core is the selected engine.** Ratified by the M2
checkpoint prototype (0 failures); the corpus runs as
`clean_stream_corpus_agrees_with_vt100_oracle` in `src/terminal/mod.rs`.

## Migration

Session profile is recorded in the journal manifest; 0.x sessions are
imported with the compatibility profile and labeled provenance, never
silently upgraded.
