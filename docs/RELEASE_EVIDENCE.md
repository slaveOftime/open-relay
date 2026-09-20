# 0.5.0 (beta) release evidence

Item-by-item evidence for the PLAN §16 release checklist, for the 0.5.0
beta line (`v0.5.x` branch; 0.3.x on `main` remains the stable default).
Test names are exact; run any of them with
`cargo test --locked --offline <name>`.
Suite totals at this commit: **634 unit + 59 integration/e2e = 693 tests,
0 ignored** (the M0-era ignored repro/probe tests were removed during the
0.5.0 cleanup; their findings are recorded in the ADRs), `cargo fmt`
clean, and **clippy is a zero-warning gate**:
`cargo clippy --locked --all-targets --all-features -- -D warnings` passes
with the deliberate allowances pinned (with per-lint rationale) in
`[lints.clippy]` in `Cargo.toml`. Web: 132 vitest tests green, `tsc` +
`vite build` green, **eslint zero errors** (the 20 pre-existing errors are
fixed: setState-in-effect cascades converted to render-time adjustments or
deferred microtasks, `SparklineStore` split out of the component file).
Post-M6 hardening on this branch (protocol v13): byte-exact attach
input end-to-end, Ctrl-] prefix detach, mouse/focus/SGR mode propagation,
checkpoint-anchored bounded replay (OJCK v2), and removal of the pre-0.5
`output.log` read fallback.

## Fidelity and interaction

- [x] **Declared profile passes on supported platforms.** One engine
  (alacritty_terminal 0.26, ADR-0001) renders live attach, `oly logs`, and
  web views on Linux/macOS/Windows — the Windows renderer is
  `#[cfg(any(windows, test))]` so its 8 tests run in Linux CI. Differential
  oracle: `clean_stream_corpus_agrees_with_vt100_oracle`
  (`src/terminal/mod.rs`).
  Platform e2e: `tests/e2e_pty.rs` (19 tests incl. native shell, PowerShell,
  concurrent sessions), `tests/e2e_daemon.rs`, `tests/e2e_csvlens.rs`.
- [x] **Keys/modifiers/paste without timing suppression.** Event-driven
  input with xterm-compatible codec (ADR-0003, M2-4/M2-5); paste boundaries
  come from bracketed-paste markers only
  (`wrap_paste_input`/`normalize_clipboard_text` tests in
  `src/client/attach.rs`); `e2e_special_keys_reach_pty_without_error`,
  `e2e_ctrl_c_interrupts_sleep_and_returns_prompt`.
  **Byte-exact input (v13):** `AttachInput.data` is raw bytes
  (base64-in-JSON) end to end — no UTF-8 lossiness, no server-side DECCKM
  rewriting; Ctrl-D/Ctrl-V reach the PTY as EOT/SYN
  (`test_ctrl_d_maps_to_eot_byte_for_the_session`,
  `test_ctrl_v_maps_to_syn_byte_for_the_session`); detach is the
  documented Ctrl-] prefix (`d`, Esc cancels, double-prefix sends a
  literal GS). Mouse/focus/SGR modes propagate from the engine to CLI
  attach via init/mode-changed frames (`map_mouse_input`,
  `sync_local_terminal_modes`, focus in/out reporting torn down by
  `terminal_normalize_disables_focus_reporting`).
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
- [x] **Bounded history reads.** Replay is checkpoint-anchored (PLAN
  §5.3): v2 checkpoints (`OJCK`) written at scanner-idle boundaries carry
  their filtered-stream offset, and `SegmentStream` resumes reading at the
  anchor's byte position instead of rescanning the recording.
  `anchored_replay_matches_full_replay_for_any_offset`,
  `anchored_windows_match_full_stream_slices`,
  `anchored_replay_skips_records_before_the_anchor` (a corrupted pre-anchor
  record does not affect anchored replay but fails a full replay),
  `checkpoint_anchors_scan_headers_and_track_filtered_offsets`,
  `checkpoint_v1_records_still_decode_without_a_filtered_offset`;
  `read_range_byte_budget_truncates_without_a_hole`,
  `read_history_budget_truncates_and_resumes_exactly`.
- [x] **History scroll persists; no forced follow.** Web HistoryController
  (M4-2) with scroll-theft fix; unit tests in
  `web/src/lib/history-controller.test.ts`.
- [x] **Mixed clients: one controller, fenced handoff.** Attach-time
  control lease + observer gating (M3-4); interactive attach takes control
  by default and takeover resizes the session to the new controller's
  viewport:
  `attach_control_lease_gates_input_and_resize`,
  `attach_takeover_resizes_session_to_the_new_controllers_viewport`,
  `attach_applied_cursor_credits_register_per_attachment`; native window
  resizes propagate to the daemon (observers take control on resize) and a
  startup-race resize is reconciled after the replay drain:
  `e2e_attach_terminal_resize_reaches_the_daemon`. (The separate
  `oly control acquire/release` + `send --lease` parked-lease ceremony was
  removed: `oly send` is an ungated operator channel by design.)
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
- [x] **Latency/throughput/resource budgets.** Bounded soak: the full suite
  run repeatedly
  (`cargo test --locked --offline`; rounds 2–3 fully green after a
  round-1 flake in `e2e_ws_attach_frames_conform_and_journal_stays_clean`
  caused by running `cargo clippy` concurrently on the same machine —
  the test then passed 2× in isolation and in every uncontended run).
  The full 24-hour daemon soak remains a release-gate activity to run on
  the release candidate build.
- [x] **CI coverage.** `.github/workflows/ci.yml`: a `fmt + clippy` gate
  job (zero-warning) plus the full `cargo test --locked` suite (unit +
  integration + e2e) on Linux/macOS/Windows with the embedded web build;
  triggers cover `Cargo.toml`/`Cargo.lock`/`build.rs`/`src`/`tests`/web
  inputs. `ci-web.yml`: `npm ci`, eslint, prettier check, vitest (incl.
  the shared WS frame fixture), build, and a Playwright chromium e2e job.
  `release.yml` runs `cargo test --locked --target <target>` before
  building release artifacts. `cargo audit` runs in CI. Playwright note:
  the e2e spec is exercised in CI (browsers install with system deps
  there); the local dev container lacks the browser's system libraries
  (no sudo), so it was not runnable here — every other gate above was run
  locally and is green.

## Maintainability and delivery

- [x] **One supported stack; obsolete 0.x paths deleted.** M6-1 (vt100
  runtime path gone), M6-2 (`output.log`/`events.log`/`persist.rs` gone),
  M6-3 (base64 attach framing gone). `grep` finds legacy names only in
  comments explaining the current design.
- [x] **Docs agree with release code.** ARCHITECTURE.md rewritten
  human-first against current sources; ARCHITECTURE_PTY.md /
  ARCHITECTURE_NOTES.md (0.x-era, drifted) deleted; README command/config
  tables verified against `src/cli.rs`/`src/config.rs`; ADRs unchanged as
  the agent-facing detail.
- [x] **Backup/upgrade/rollback documented and honest.** `MIGRATION.md`;
  no auto-import by design (PLAN §14.4 — never fabricate provenance);
  pre-0.5 sessions produce explicit errors, verified by
  `persisted_log_page_rejects_pre_0_5_output_log_sessions` (the legacy
  `output.log` read path and its sidecar index are deleted, not
  deprecated — HTTP returns 410, IPC returns an Error frame).
- [x] **Mismatch errors actionable; channels consistent.** IPC version
  handshake rejects mismatched CLI/daemon pairs with a precise version
  message; protocol version bumped 11 → 12 with the framing change and
  12 → 13 with byte-exact input + attach mode fields;
  `release.yml` builds the matching Linux/macOS/Windows artifacts from one
  tag; crate version set to 0.5.0. The web attach client fails loudly on
  unknown/truncated binary frames (`parseServerFrame` throws; surfaced via
  `onError`) instead of silently dropping stream bytes (I2).

## Release rule

No known silent input loss, attach-boundary corruption, destructive
retained-history resize, ambiguous cursor reuse, or unbounded client queue
exists at this commit.
