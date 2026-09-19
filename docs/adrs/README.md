# Architecture decision records

Decisions for the 0.5.0 redesign described in [PLAN.md](../../PLAN.md).
Each ADR records context, the decision, rejected alternatives, acceptance
tests, and migration impact. Statuses:

- **Proposed** — drafted in M0, not yet ratified by the M0 exit gate.
- **Accepted** — ratified with prototype/measurement evidence.
- **Superseded** — replaced by a later ADR (link both ways).

| ADR | Title | Status | Plan reference |
|---|---|---|---|
| [0001](ADR-0001-terminal-engine.md) | Terminal engine, profile, and restoration | Proposed — Alacritty/WezTerm evaluation authorized | PLAN §5 |
| [0002](ADR-0002-event-journal.md) | Event order, journal, cursors, durability, retention | Accepted — format v1 provisional until M1 | PLAN §4.2, §6 |
| [0003](ADR-0003-input-control-geometry.md) | Input codec, acknowledgments, controller leases, geometry | Accepted | PLAN §8 |
| [0004](ADR-0004-stream-protocol.md) | Stream framing, negotiation, credits, federation scheduling | Proposed | PLAN §7, §10.1 |
| [0005](ADR-0005-history-ux.md) | Complete-history UX versus finite host buffers | Proposed | PLAN §6.4 |
| [0006](ADR-0006-crash-boundary.md) | Crash boundary; defer process-surviving upgrades | Accepted | PLAN §1, §10.2 |
| [0007](ADR-0007-authz-sessions.md) | Auth scopes, browser sessions, proxy isolation, side effects | Accepted — details in M5 | PLAN §10.3 |

Ratification rule (PLAN §13, M0 exit): an ADR becomes Accepted when the
owner ratifies its direction on the strength of the gathered evidence;
its "Acceptance" tests still gate the corresponding milestone exit (e.g.
ADR-0002's ignored repros must pass un-ignored for M1). Until an ADR's
code lands, development work stays behind development-only switches and
both paths are deleted or promoted by M6 — never shipped as two
supported stacks.
