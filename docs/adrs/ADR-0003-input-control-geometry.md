# ADR-0003: Input codec, acknowledgments, controller leases, geometry

- Status: Accepted (semantic/raw split, fenced controller, controller-owned geometry)
- Plan reference: PLAN.md §8 (invariants I4, I6)

## Context

Input is interpreted at several layers: the CLI maps keys incompletely,
consumes Ctrl-D as detach, intercepts clipboard shortcuts, detects paste by
timing (30/150 ms heuristics), and the daemon globally rewrites arrows under
DECCKM — including inside raw/pasted bytes. Any attached client can write or
resize; the latest resize wins, so a phone reshapes the desktop TUI. Focus
reports are discarded. There is no ownership, no fencing, and no truthful
acknowledgment.

## Decision (draft)

1. Distinct wire forms: `RawBytes` (never transformed), `Key` (encoded once
   by the session codec under current modes), `Paste` (explicit, bounded,
   transactional), `Mouse`/`Focus` (semantic, controller-only).
2. Detach is a documented prefix (`Ctrl-]` then `d`, with literal escape and
   disable option). Ctrl-D/Ctrl-C/Ctrl-Z/Ctrl-V/Escape keep their
   application meaning.
3. `InputAck` reports request ID, lease generation, accepted/written counts
   and status (`rejected`/`queued`/`written`/`partial`/`unknown`).
   Acknowledgment is not application effect; ambiguous writes are never
   silently retried. Dedup is bounded to the live incarnation.
4. Many observers, one controller. Attachment records carry identity, role,
   generation, viewport, applied cursor, liveness. Fenced expiring leases
   gate all input and geometry. Agents observe by default; human handoff is
   a sequenced, reported operation.
5. Geometry is controller-owned (or fixed at start). Observers report
   viewports but cannot resize. Successful resizes are sequenced journal
   events; failed resizes publish nothing.

## Rejected alternatives

- Smallest-client/last-resizer geometry (a background tab constrains work).
- Timing-based paste detection (misclassifies fast typing and slow networks).
- Exactly-once input claims across crashes (impossible; be honest instead).

## Acceptance

- Raw bytes with embedded arrows/escape-like content pass unmodified.
- DECCKM arrows encode correctly; paste is never key-rewritten.
- Takeover with queued old-generation input resolves a bounded prefix and
  rejects the rest; stale tokens are fenced.
- Observer viewport changes never emit a PTY resize.

## Evidence gathered in M0

Incumbent `map_key_to_input` coverage gaps, pinned as repros in
`client::attach::tests` (see [M0_EVIDENCE.md](../M0_EVIDENCE.md)):

Expected sequences below are the legacy xterm-compatible profile
(ADR-0001 stable profiles); enhanced keyboard profiles (kitty protocol)
are a separate negotiated capability.

- **Alt+<char> is sent unprefixed** (`x` instead of `\x1bx`) — every
  Alt-chord in the incumbent codec corrupts into a bare character.
- **Modifiers on arrows/Home/End/PageUp/PageDown/Delete are ignored** —
  Ctrl+Up emits plain `\x1b[A`, so editor chord bindings (e.g. word-jump)
  never fire.
- **The Ctrl+@/digit family sends raw `c & 0x1f`** — Ctrl+2 emits `\x12`
  instead of the NUL every mainstream terminal sends (and 3–8 duplicate
  ESC/FS/GS/RS/US/DEL).
- **Function keys fall through to `None` and are silently dropped** —
  F5 is input loss by omission.
- Typing floor: every paste-candidate key (ordinary text, Enter,
  unshifted Tab) is buffered and each follow-up **resets** the 30 ms
  `PASTE_BURST_WAIT` deadline, so continuous typing accumulates until the
  user pauses for a full burst window — while the daemon backend
  round-trip measures ~100 µs p50 ([M0_EVIDENCE.md](../M0_EVIDENCE.md)).
- Silent loss: the 150 ms `PASTE_KEY_SUPPRESS_WINDOW` drops incoming key
  events after a clipboard paste (input-side, not output filtering) —
  legitimate fast follow-up typing after a paste is silently discarded.
- The 1.0 codec must use explicit paste events (bracketed paste where the
  terminal supports it, explicit paste command otherwise) with no
  timing-based classification of ordinary typing.

## Migration

CLI detach keybinding changes are breaking and documented; agent `send`
requires explicit session IDs and leases in 1.0 machine workflows.
