# Milestone 10: Standardized Agent PTY Signaling (OSC 888)

> **Status:** ⏳ Not started  
> **SPEC ref:** §14.7, §7.1 (post-MVP quality upgrades), §8 (escalation semantics)

## Goal

Define and implement a machine-readable OSC escape sequence (`OSC 888`) that agent processes can emit to explicitly signal their intent to the daemon — replacing the heuristic prompt-detection model with reliable first-class events.

---

## Signal types

| Signal | Meaning |
|---|---|
| `needs_input` | Agent is waiting for human input at a well-defined prompt |
| `unsure` | Agent encountered uncertainty; requesting guidance |
| `checkpoint` | Agent completed a phase; status update for operator |
| `higher_permission_required` | Agent needs elevated authorization to proceed |

---

## Tasks

### Protocol definition

- [ ] Draft `OSC 888` payload spec: `ESC ] 888 ; <kind> ; <json-metadata> BEL`.
- [ ] Define `json-metadata` schema: `{ "message": string, "context": any, "session_id": string }`.
- [ ] Publish spec in `SPEC.md` §14.7 and a standalone `docs/osc888.md`.

### Daemon-side parser

- [ ] Extend PTY output canonicalizer to detect and extract `OSC 888` sequences.
- [ ] Emit structured `NotificationEvent` from `notification/event.rs` with the decoded kind and metadata.
- [ ] Strip `OSC 888` sequences from `output.log` (or store raw + stripped variants — TBD).

### Integration with notification dispatcher

- [ ] `OSC 888` events bypass heuristic silence timer; trigger notification immediately.
- [ ] Include `kind` field in local OS notification payload and SSE `session_notification` event.
- [ ] `higher_permission_required` escalations receive elevated urgency in notification.

### Supervisor agent adapter

- [ ] Document and provide an example of a supervisor agent polling `GET /api/events` (SSE) and reacting to `session_notification` events with kind `unsure` or `higher_permission_required`.

### Verification

- [ ] A shell script emitting `printf '\033]888;needs_input;{"message":"approve change?"}\007'` triggers an immediate notification.
- [ ] Signal is correctly suppressed from rendered terminal output.
- [ ] Debounce still applies to `needs_input`; `higher_permission_required` is not debounced.
