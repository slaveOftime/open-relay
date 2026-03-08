# Milestone 12: Enterprise Policy Adapters

> **Status:** ⏳ Not started  
> **SPEC ref:** §14.9

## Goal

Allow organizations to enforce approval gates, risk scoring, and cost / risk checkpoints before or during agent sessions — integrating with existing identity and approval infrastructure.

---

## Tasks

### Policy engine core

- [ ] Define policy rule schema: `{ trigger: event_matcher, action: gate|notify|block, target: webhook_url|builtin }`.
- [ ] Load policy rules from `<state>/policy.json` (or environment-specified path).
- [ ] Evaluate rules on session lifecycle events and `OSC 888` signals.
- [ ] Default policy: pass-through (no gates) — opt-in model.

### Approval gate

- [ ] When a matching event hits an `action: gate` rule, pause the session output relay until the gate resolves.
- [ ] Expose `GET /api/gates` and `POST /api/gates/:gate_id/approve|reject` endpoints.
- [ ] Timeout semantics: auto-reject or auto-approve after configurable deadline.
- [ ] Record gate outcome in `events.log`.

### Risk scoring

- [ ] Pluggable risk scorer interface: command string, working directory, recent output excerpt → numeric score.
- [ ] Builtin heuristic scorer (regex-based: destructive commands, broad file paths, credentials patterns).
- [ ] Optional LLM-based scorer: call a local Ollama endpoint with a small model for semantic risk assessment.
- [ ] Score threshold config: sessions above threshold auto-trigger human approval gate.

### Cost / resource checkpoint

- [ ] Track estimated token / API cost signals from `OSC 888 checkpoint` payloads (cost field in metadata).
- [ ] Configurable cumulative cost threshold; pause session when exceeded.
- [ ] Resume or terminate from web UI or CLI.

### Webhook adapter

- [ ] `action: webhook` policy rule posts structured event payload to a configured URL.
- [ ] Support custom headers (auth tokens) in webhook config.
- [ ] Webhook response body can carry `{ decision: "approve"|"reject", instructions: string }` to resolve a gate.

### Audit export

- [ ] `oly audit export [--session <id>] [--since <iso8601>] [--format json|csv]` — export events + gate decisions.

### Verification

- [ ] A policy rule blocks a session from running a destructive command pattern until a human approves via web UI.
- [ ] LLM scorer correctly flags a high-risk command in a test script.
- [ ] Webhook adapter delivers event payload; gate resolved by webhook response.
