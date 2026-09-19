# ADR-0006: Crash boundary; defer process-surviving upgrades

- Status: Accepted (history survives daemon restart; live processes do not)
- Plan reference: PLAN.md §1, §10.2 (invariants I8, I10)

## Context

The daemon owns the PTY master. Sessions survive client death but not
daemon death: a surviving child PID cannot reattach to a lost master fd.
0.x restart reconciliation is ambiguous about what survived, and completion
races output drain — stop can lose the last diagnostics.

## Decision (draft)

1. 0.5.0 documents the boundary honestly: sessions survive **client** loss;
   journal history survives daemon restart according to the durability
   policy (`journal_seq`/`durable_seq`); live processes do not survive
   daemon death.
2. Daemon restart seals the interrupted incarnation at its recovered
   journal boundary. Cursors past it return explicit incomplete-capture
   errors — never empty success or reused sequences.
3. Child exit and PTY EOF are separate facts. Completion follows the final
   retained output plus a persistence barrier, or reports
   `capture_incomplete` when descendants hold handles past the drain
   deadline. Stop/kill/finalize are idempotent and platform-tested
   (Unix process groups, Windows Job Objects).
4. A separately supervised session-host process (for daemon upgrades that
   keep sessions alive) is deferred past 0.5.0 as its own ADR; the journal
   and protocol boundaries chosen here must not preclude it.

## Rejected alternatives

- PID-based "reattachment" to a lost PTY (impossible; do not fake it).
- Silent restart of agent commands after daemon death (side effects are
  not safely replayable).
- Shipping session-host separation in 0.5.0 (doubles the lifecycle matrix).

## Acceptance

- Fault tests: SIGKILL the daemon mid-output, restart, verify sealed
  incarnation, truthful interrupted status, no reused cursors.
- Stop during heavy output delivers final bytes or a declared incomplete
  capture, on all supported platforms.

## Migration

Restart reconciliation messages change from ambiguous "resumed" claims to
explicit interrupted/sealed states; documented in the upgrade guide.
