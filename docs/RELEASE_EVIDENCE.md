# 1.0.0 release evidence

Item-by-item evidence for the PLAN §16 release checklist. Test names are
exact; run any of them with `cargo test --locked --offline <name>`.
Suite totals at this commit: **620 unit + 52 integration/e2e = 672 tests**
(9 ignored: M0 repro/probe tests kept `#[ignore]`d by design), `cargo fmt`
clean, clippy at the pinned baselines (bin 56, bin-test 68, e2e targets
unchanged).

## Fidelity and interaction

- [x] **Declared profile passes on supported platforms.** One engine
  (alacritty_terminal 0.26, ADR-0001) renders live attach, `oly logs`, and
  web views on Linux/macOS/Windows — the Windows renderer is
  `#[cfg(any(windows, test))]` so its 8 tests run in Linux CI. Differential
  oracle: `clean_stream_corpus_agrees_with_vt100_oracle`
  (`src/terminal/mod.rs`); full evaluation in `docs/ENGINE_EVAL.md`.
  Platform e2e: `tests/e2e_pty.rs` (26 tests incl. native shell, PowerShell,
  concurrent sessions), `tests/e2e_daemon.rs`, `tests/e2e_csvlens.rs`.
- [x] **Keys/modifiers/paste without timing suppression.** Event-driven
  input with xterm-compatible codec (ADR-0003, M2-4/M2-5); paste boundaries
  come from bracketed-paste markers only
  (`wrap_paste_input`/`normalize_paste_text` tests in
  `src/client/attach.rs`); `e2e_special_keys_reach_pty_without_error`,
  `e2e_ctrl_c_interrupts_sleep_and_returns_prompt`.
- [x] **Queries answered once from ordered state.** Single sequencing point
  in the session runtime (`src/session/runtime.rs`); the engine holds
  authoritative mode/geometry state.
- [x] **Buffers/styles/cursor/Unicode survive attach/reconnect/resize.**
  Snapshot-from-journal + gapless live stream (`src/session/store/pump.rs`);
  resize = engine resize + refeed (no parser rebuild); scrollback seeded on
  reattach; transcript goldens in `src/session/logs/` fixture tests.
- [x] **No destructive parser resets.** The vt100 reset/repaint workarounds
  were deleted with the vt100 runtime path (M6-1).

## Streams, history, ownership

- [x] **Snapshot/resume under adversarial scheduling.** Cursor-checked
  streaming with incarnation fencing (M3-3):
  `attach_stream_conforms_end_to_end_over_a_real_socket`
  (`src/daemon/rpc.rs`, real socket, connections A–D incl. cross-incarnation
  fencing and takeover), `pump_resyncs_the_gap_before_a_far_ahead_chunk`,
  `pump_resyncs_from_persisted_stream_after_lag`.
- [x] **Retention never resets identity; gaps are explicit.** Journal
  incarnations + sealed-part manifests (M3-6, W1):
  `open_seals_validated_parts_orphaned_by_a_crash`,
  `corrupted_payload_stops_the_scan`,
  `reopen_reports_interior_corruption_without_rewinding`
  (`src/session/journal.rs`); `oly doctor` verifies manifests (e2e in
  `tests/e2e_daemon.rs`).
- [x] **Bounded history reads.** `read_range_byte_budget_truncates_without_a_hole`,
  `read_history_budget_truncates_and_resumes_exactly`; logs reads go through
  journal replay windows (`src/session/replay.rs`).
- [x] **History scroll persists; no forced follow.** Web HistoryController
  (M4-2) with scroll-theft fix; unit tests in
  `web/src/lib/history-controller.test.ts`.
- [x] **Mixed clients: one controller, fenced handoff.** Control lease +
  observer gating (M3-4):
  `attach_control_lease_gates_input_and_resize`,
  `parked_control_leases_drive_input_are_capped_and_releasable`,
  `attach_applied_cursor_credits_register_per_attachment`.
- [x] **Slow clients stay memory-bounded.** Enforced stream credits and
  bounded queues (M5-1): `credit_gate_holds_output_until_the_client_applies`,
  `credit_gate_disconnects_a_stalled_client_loudly`,
  `pump_stress_ten_mixed_clients_stay_consistent`; bounded client reader
  channel (16 frames) in `src/client/attach.rs`.
- [x] **Rejection/partial/retry semantics tested.** Frame codec rejects
  oversize/unknown tags from the header before allocation
  (`attach_frame_caps_are_enforced_before_allocation`, `src/ipc.rs`);
  mid-stream errors map to `RequestFailed`
  (`read_checked_attach_frame`).

## Reliability, security, performance

- [x] **Crash/disk-full/partial-write recovery.** ADR-0006: journal failures
  fail the session loudly; `disk_full_degrades_the_appender_without_a_hole`,
  partial-read handling (`read_exact_or_partial`), crash-sealing on open.
- [x] **Exit drains or declares incomplete; stop/kill idempotent.** Exit
  stress (M3-7); process-tree kill + graceful drain (M5-5):
  `e2e_kill_terminates_the_whole_process_tree`,
  `e2e_daemon_stop_kills_process_trees`,
  `e2e_kill_session_status_transitions_to_killed`.
- [x] **Security review passes.** `docs/SECURITY_AUDIT_REPORT.md` + M5-6
  addendum; principal-bound authz, scopes, Origin/CSRF checks, proxy
  isolation (M5-4, `src/http/auth.rs` scope tests); IPC socket is
  owner-only; privacy-by-construction log defaults (M5-6).
- [x] **Latency/throughput/resource budgets.** Scan throughput probes
  (`probe_scan_throughput_*`, `src/session/scan.rs`, ratified M0 numbers in
  `docs/M0_EVIDENCE.md`); bounded soak: full suite run repeatedly
  (`cargo test --locked --offline`; rounds 2–3 fully green after a
  round-1 flake in `e2e_ws_attach_frames_conform_and_journal_stays_clean`
  caused by running `cargo clippy` concurrently on the same machine —
  the test then passed 2× in isolation and in every uncontended run).
  The full 24-hour daemon soak remains a release-gate activity to run on
  the release candidate build.
- [x] **CI coverage.** `.github/workflows/ci.yml` (Rust unit/integration/
  e2e incl. protocol conformance + fault tests), `ci-web.yml` (eslint, tsc,
  vitest incl. the shared WS frame fixture), release inputs in
  `release.yml`.

## Maintainability and delivery

- [x] **One supported stack; obsolete 0.x paths deleted.** M6-1 (vt100
  runtime path gone), M6-2 (`output.log`/`events.log`/`persist.rs` gone),
  M6-3 (base64 attach framing gone). `grep` finds legacy names only in
  comments explaining the 1.0 design.
- [x] **Docs agree with release code.** ARCHITECTURE.md rewritten
  human-first against 1.0 sources; ARCHITECTURE_PTY.md /
  ARCHITECTURE_NOTES.md (0.x-era, drifted) deleted; README command/config
  tables verified against `src/cli.rs`/`src/config.rs`; ADRs unchanged as
  the agent-facing detail.
- [x] **Backup/upgrade/rollback documented and honest.** `MIGRATION.md`;
  no auto-import by design (PLAN §14.4 — never fabricate provenance);
  pre-1.0 sessions produce explicit errors, verified by tests
  (`render_log_session` legacy-branch errors, stats report size 0).
- [x] **Mismatch errors actionable; channels consistent.** IPC version
  handshake rejects mismatched CLI/daemon pairs with a precise version
  message; protocol version bumped 11 → 12 with the framing change;
  `release.yml` builds the matching Linux/macOS/Windows artifacts from one
  tag; crate version set to 1.0.0.

## Release rule

No known silent input loss, attach-boundary corruption, destructive
retained-history resize, ambiguous cursor reuse, or unbounded client queue
exists at this commit. The M0 repro tests that demonstrated these classes of
defects in 0.x remain in the tree (`#[ignore]`d) as documentation of what
was fixed.
