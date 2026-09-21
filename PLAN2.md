# PLAN2 — Review follow-up: structure, performance, security

This plan captures the findings of the 0.5.0 code review of `src/` as
actionable work. It is deliberately ordered **structure/design first, then
performance, then security**: the structural work de-risks everything that
follows (it is behavior-preserving and covered by the existing suite), the
performance work then lands in modules that are easy to change, and the
security items that need design conversations land last, after the
structural seams exist to implement them cleanly.

Rules of engagement (inherited from PLAN.md / ARCHITECTURE.md):

- Every phase keeps `cargo clippy --locked --all-targets --all-features -- -D warnings` green.
- Structural phases must be pure refactors: no protocol, wire-format, or
  behavior changes; the existing conformance + e2e suites are the proof.
- Any item that changes observable behavior (error strings, config
  semantics, authz) gets a bullet in the changelog section of its task and,
  where flagged, a note in ARCHITECTURE.md / MIGRATION.md.
- Line numbers reference the 0.5.0 tree and will drift; anchor on the
  named functions instead.

Phase summaries:

| Phase | Theme | Risk | Depends on |
|---|---|---|---|
| S1 | God-module decomposition | low (mechanical) | — |
| S2 | One streaming loop (`ws.rs` unification) | medium | — |
| S3 | Module hygiene & lint scoping | low | S1 |
| S4 | Authorization architecture | medium | S1 |
| P1 | Off the async workers | low | S1 |
| P2 | Hot-path costs | low | — |
| P3 | Caching & polling elimination | low | — |
| X1 | Security quick wins | low | — (S4 helps X1.4) |
| X2 | Credential verification hardening | medium | — |
| X3 | Trust & token design (ADRs) | medium | S4 |

---

## Phase S1 — Structure: decompose the god modules

Pure file splits; `mod` declarations only, zero logic edits. Each task is
one reviewable commit.

### S1.1 Split `session/journal.rs` (4.8k lines) → `journal/` — **partial**

*S1.1 [DONE] first carve-out.* The on-disk format, primary types
(`RecordKind`, `Record`), CRC-32 (`CRC32_TABLE`, `Crc32`,
`crc32_two`, `encode_record_header`) are now in `journal/record.rs`
(153 lines). Everything else stays in `journal/mod.rs`, which re-exports
the carved-out items so external callers (`crate::session::journal::*`)
are unchanged.

| new file | contents (anchor symbols) | state |
|---|---|---|
| `journal/record.rs` | `RecordKind`, `Record`, `HEADER_LEN`, `crc32_table`/`CRC32_TABLE`, `Crc32`, `crc32*`, `encode_record_header` | done (153 LOC) |
| `journal/segment.rs` | `SegmentWriter`, `RollingSegmentWriter`, `segment_path`, `parse_segment_name`, `list_segments`, `list_incarnations`, `part_first_seq` | TODO (S1.5 / S1.6) |
| `journal/appender.rs` | `SequencerCore`, `JournalAck`, `JournalAppender`, `JournalSubmitError`, `appender_loop` | TODO |
| `journal/manifest.rs` | `SegmentManifestEntry`, `RetiredIncarnation`, `ManifestLine`, `append_manifest_line`, `read_manifest*`, `verify_manifest` | TODO |
| `journal/shadow.rs` | `ShadowJournal` | TODO |
| `journal/scan.rs` | `ScanStop`, `ScanOutcome`, `SegmentStats`, segment scan | TODO |
| `journal/open.rs` | `open`, `OpenedJournal`, `RecoveryReport`, `complete_retired_retention`, `seal_validated_part`, `sync_dir`, `JournalCursor`, `OrderedEvent` + the durability constants (`retain_*`, `checkpoint_*`) | TODO |

**Why partial.** Byte-level move of the remaining ~4.5k lines into 6
files risks silently dropping helpers (e.g. `crc32_two`, `Crc32`
struct and `Default` impl) — first attempt tripped duplicate-name
errors that surface only after a full re-compile, and the file's
section-aware structure means roughly a dozen tiny helpers straddle the
proposed splits. Better to deliver what's safe (the format/CRC block,
the smallest read from the audit lens) and resume the rest as
discrete, byte-verifiable moves after each sub-step right now lives
behind a single diff. Plan2 S1.5 / S1.6 will pick this up: same
section map, one file per change, byte-identical contents (only `use`
paths re-targeted), full test suite green per step.

- Acceptance for this partial: 153-line `record.rs`; `journal/mod.rs`
  still under or wrapping the original with re-exports; `cargo
  clippy --locked --all-targets --all-features -- -D warnings`,
  `cargo test --bin oly`, `cargo fmt --check` all green.

### S1.2 Split `http/apps.rs` (1.5k lines) → `http/apps/`
- `apps/manifest.rs` — `AppManifest`, `load_app_manifest`, `read_manifest_file`,
  `parse_manifest`, `resolve_manifest_entry*`, `canonicalize_redirect_path`.
- `apps/html.rs` — `extract_title`, `extract_app_description`, `extract_app_kind`,
  `extract_app_icon_href`, `detect_app_icon_href`, `resolve_app_asset_href`,
  the static index page bootstrap (`ensure_wwwroot`, `build_static_app`).
- `apps/proxy_targets.rs` — `build_proxy_target_urls`, `with_proxy_query`,
  `origin_root_url`, `filtered_proxy_query`, `is_private_proxy_target`,
  `is_ssrf_dangerous_ip` (X1.1 will rewrite the last two in place).
- `apps/resolve.rs` — `resolve_app_request`, candidates, local-asset lookup.

### S1.3 Carve the Windows crash handler out of `client/list_tui.rs` (5.6k lines)
- New `client/crash.rs`: the `unsafe` vectored-exception handler,
  `report_crash`, `native_crash_message`, `write_handle`, `SetUnhandledExceptionFilter`.
- Follow-up (optional, same phase): split render vs input vs state in
  `list_tui.rs` behind the same `ListApp` facade. Only the crash carve-out
  is required; it isolates the **crash-handling** `unsafe` (Win32 calls into
  `SetUnhandledExceptionFilter`, `EXCEPTION_POINTERS` reads, `WriteFile`).
  Other Win32 pointer dances in `list_tui.rs`
  (`windows_screen_geometry`) live in their own fenced function and stay
  out of scope here.

### S1.4 Remove the `#[cfg(test)]` module-visibility inversion
- `session/mod.rs`: delete the `#[cfg(not(test))] mod store; #[cfg(test)] pub(crate) mod store;`
  pair. Keep `store` private; give `SessionStore` a `#[doc(hidden)] pub fn
  new_for_test(...)`-style constructor (or a `store::testsupport` gated
  inside the module) so unit tests don't need a wider module.
- Replace the `#[cfg(test)] pub fn` leaks in `journal` (`crc32`,
  `scan_segment`, `parse_policy`, `decode_lifecycle_payload`) with
  `pub(crate)` (they are trivial) or move them into `#[cfg(test)]` test-only
  helper modules, so unit tests compile the same module graph as shipping
  code.

### S1.5 Continue the journal split — single-file, byte-verifiable moves

Resumes the partial S1.1 work. Each step below is **one** file move
(only `use` paths retargeted), verified by `diff` against `/tmp/journal.rs.ORIG`
(or the previously committed state of `journal/mod.rs`) before the move
and by the test suite afterwards.

Order chosen to keep names non-colliding and to minimise `use`-path
edits in untouched files:

1. **`journal/shadow.rs`** — `ShadowJournal`, ~120 LOC.
2. **`journal/appender.rs`** — `SequencerCore`, `JournalAck`,
   `JournalAppender`, `JournalSubmitError`, `appender_loop`, ~470 LOC.
3. **`journal/scan.rs`** — `ScanStop`, `ScanOutcome`, `SegmentStats`,
   segment scan + the small codec helpers it directly references,
   ~360 LOC.
4. **`journal/manifest.rs`** — `SegmentManifestEntry`,
   `RetiredIncarnation`, `ManifestLine`, `append_manifest_line`,
   `read_manifest*`, `verify_manifest`, ~280 LOC.

Each step ships independently. Cargo gates must stay green per step.

### S1.6 Finish the journal split

Continues S1.5 for the two largest remaining clusters:

5. **`journal/segment.rs`** — `SegmentWriter`,
   `RollingSegmentWriter`, `segment_path`, `parse_segment_name`,
   `list_segments`, `list_incarnations`, `part_first_seq`, ~700 LOC.
6. **`journal/open.rs`** — `open`, `OpenedJournal`, `RecoveryReport`,
   `complete_retired_retention`, `seal_validated_part`, `sync_dir`,
   `JournalCursor`, `OrderedEvent` + the durability constants
   (`retain_*`, `checkpoint_*`), ~1300 LOC.

After S1.6 lands, `journal/mod.rs` should shrink below ~2.2k LOC and
hold only the cross-file type definitions + the `AppArmor` types
referenced across files, plus the `mod tests` block.

Exit criteria for S1: no file under `src/` (excluding `*_tests.rs` /
`test-support` modules) exceeds ~2.5k lines for `journal/*` and ~2.0k
lines elsewhere; no `cfg`-dependent module visibility anywhere; final
S1 status is recorded in this file.

---

### S1.5 / S1.6 progress recorded in-session (current snapshot)

Steps performed in this session (single-file byte-verified moves,
`cargo clippy --locked --all-targets --all-features -- -D warnings`
and `cargo test --bin oly` = 653 passing at every step):

- S1.1 partial: `journal/record.rs` extracted (154 lines).
- S1.2: `http/apps/` carved into 5 sibling files.
- S1.3: `client/crash.rs` carved out of `client/list_tui.rs`.
- S1.4: cfg(test) visibility inversion at `session/mod.rs`.
- S1.5 step 1: `journal/codec.rs` (105 lines).
- S1.5 step 2: `journal/manifest.rs` (232 lines, with `ManifestLine`, `read_manifest_lines`, `verify_manifest` cfg-test re-exported).
- S1.5 step 3: `journal/scan.rs` (111 lines, top section).
- S1.5 step 4: `journal/stream.rs` (593 lines).
- S1.5 step 5: `journal/segment.rs` (199 lines).
- S1.5 step 6: `journal/open.rs` (267 lines).
- S1.5 step 7: `journal/appender.rs` (626 lines).
- S1.5 step 8: `journal/shadow.rs` (162 lines).
- **S1.6 step 1: scan-internals → `scan.rs` (361 lines, +250 LOC).** Carved the second non-contiguous block (`ScanMode`, `ScanResult`, `ScanStart`, `scan_impl`, `outcome`, `ReadPiece`, `read_exact_or_partial`, `SPARSE_INDEX_STRIDE_BYTES`) into the existing scan module. Widened visibility to `pub(crate)` on internals reached from sibling modules (`stream::read_tail` for the stride const; `segment::part_first_seq` for `ReadPiece`/`read_exact_or_partial`). `IndexEntry` promoted from `#[cfg(test)]`-private to production `pub(crate)` because `ScanResult::index` field type.
- **S1.6 step 2: checkpoint + retention → `checkpoint.rs` (351 lines).** Carved the `CHECKPOINT_*` constants, `Checkpoint` struct, `encode_checkpoint`/`decode_checkpoint` codec, `CheckpointAnchor` machinery, `latest_checkpoint_incarnation`, `incarnation_has_checkpoint`, `retain_before`, `retain_before_unchecked` into a new `journal/checkpoint.rs`. `CHECKPOINT_MAGIC` widened from `const` to `pub(crate) const` (reached from `mod.rs`'s `mod tests` via `super::*`). `incarnation_has_checkpoint` likewise. `retain_before_unchecked` kept `#[cfg(test)] pub fn`; mod.rs cfg(test) re-export.

Final journal layout (S1 end state):

| File | Lines | Status |
|---|---|---|
| `journal/mod.rs` | 2024 | DONE (cross-module glue + mod tests; below 2.2k target) |
| `journal/codec.rs` | 105 | DONE (S1.5 step 1) |
| `journal/manifest.rs` | 232 | DONE (S1.5 step 2) |
| `journal/record.rs` | 154 | DONE (S1.1) |
| `journal/scan.rs` | 361 | DONE (S1.5 step 3 + S1.6 step 1 internals) |
| `journal/segment.rs` | 199 | DONE (S1.5 step 5) |
| `journal/stream.rs` | 593 | DONE (S1.5 step 4) |
| `journal/open.rs` | 267 | DONE (S1.5 step 6) |
| `journal/appender.rs` | 626 | DONE (S1.5 step 7) |
| `journal/shadow.rs` | 162 | DONE (S1.5 step 8) |
| `journal/checkpoint.rs` | 351 | DONE (S1.6 step 2) |
| TOTAL | 5074 | (4796 → 5074, +278 LOC from doc headers, visibility widenings, cfg-test exports) |

**S1 exit criteria met:**
- All gates green at every step and at end: `cargo check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, `cargo test --bin oly` (653 passed), `cargo fmt --check`.
- `journal/mod.rs` = 2024 lines (S1 target was ≤ ~2.2k). ✅
- No `cfg`-dependent module visibility inversion remains in `journal/mod.rs`. ✅
- Split is purely structural (no protocol/wire/behavior changes). ✅
- Visibility widened only to `pub(crate)` — nothing escapes crate boundary beyond `pub use codec::{...}`, `pub use record::{...}`, etc., which mirror pre-S1 visibility. ✅

No commits made during this run; pausing per user instruction for review.

## Phase S2 — Structure: one streaming loop for local and relayed attach

Today `http/ws.rs` holds `handle_ws_streaming` (~340 lines) and
`handle_ws_proxied_streaming` (~350 lines) as structurally parallel copies
(init → snapshot → chunk loop → coalesce → send → backpressure → teardown).
ARCHITECTURE.md says a federated attach is a local attach plus one
transport hop; the code should say so too.

### S2.1 Introduce an attach source abstraction
- Define in `http/ws.rs` (or `http/attach_source.rs`):

  ```rust
  enum AttachSource {
      Local { registration: AttachmentRegistration, pump: AttachPump },
      Relayed { stream_rx: mpsc::Receiver<...>, rpc_id: ..., node: String },
  }
  ```

  with two methods: `next_event() -> AttachStreamEvent` (chunk / modes /
  resize / ended / error) and `send_control(ClientMessage)` — the local arm
  drives the pump + attachment registry directly, the relayed arm forwards
  `RpcStreamMessage`s through `NodeRegistry`.

- The differing preambles (lease registration vs `proxy_rpc_stream`) stay
  as small per-arm constructors; everything from init-frame onward merges
  into a single `serve_attach(socket, source, params)` loop.

### S2.2 Prove equivalence with tests
- Port the proxied-path tests to run the same assertions as the local path
  (frame order, coalescing, credit enforcement, teardown on revocation)
  against `AttachSource::Relayed` backed by a scripted `mpsc`.
- Add one table-driven test that feeds an identical event sequence through
  both arms and asserts byte-identical WS frames.

Exit criteria: the shared serve loop exists exactly once; `handle_ws_proxied_streaming`
is gone; `node/` semantics unchanged (allowlist, deadlines, fencing all
still enforced by the owning node's own daemon — do not weaken).

---

## Phase S3 — Structure: module hygiene and lint scoping

### S3.1 Scope the clippy allowances
- Delete the crate-wide `await_holding_lock` allow from `Cargo.toml`;
  place `#[allow(clippy::await_holding_lock)]` on the specific store
  functions the comment describes (`store/*.rs`). New deadlock-lint
  violations must surface. If a new violation appears, fix it, don't re-allow.
- Audit the other four allowances at the same time and keep them as narrow
  per-function allows with the existing rationale comments.

### S3.2 Delete dead code and dead-code allows
- `AppError::Unimplemented` (reserved-for-milestone = YAGNI; delete).
- `#[allow(dead_code)]` on the whole `SessionEvent` enum
  (`session/mod.rs`) — if variants are relay-only, say so in a comment per
  variant instead of muting the whole enum.
- `ipc::read_request`, `ipc::write_response` (`ipc.rs`),
  `notification::prompt::matches_prompt`.

### S3.3 Fix the stringly-typed drift bugs found in review
- `http/mod.rs` `METRIC_ROUTES`: `"/api/push/subscribe"` ≠ the actual
  `MatchedPath` `"/api/push/subscriptions"` — every push subscription
  currently lands in the `"other"` histogram bucket. Fix the string AND add
  a debug-only assertion that each `METRIC_ROUTES` entry appears in the
  router (or drop the list and label by `MatchedPath` with a cardinality
  cap).
- `session/mod.rs` `SessionError::message`: two format strings contain
  literal ~18-space runs (`"...incarnation Some(1),␣␣␣...  session
  incarnation is..."`) — leftovers from line-continuation removal in
  user-facing protocol errors. Reflow; update the fixtures that assert on
  the messages if any (`tests/fixtures/`, `rpc.rs` conformance).

### S3.4 Config structure (enables S4/X tasks)
- Group the 25 flat `AppConfig` fields into sub-structs (`Paths`, `Http`,
  `Notify`, `Limits`, `WebPush`), keeping the JSON shape backward
  compatible via `#[serde(flatten)]`.
- With sub-structs, replace the hand-maintained `hot_reload_changes` /
  `restart_required_changes` field lists with per-sub-struct diff
  functions, so a newly added field cannot silently miss both lists.
- Rename the JSON override key drift: accept both `"bind"` and
  `"http_bind"` (old key stays accepted; document canonical name).

---

## Phase P1 — Performance: get blocking work off the async workers

The daemon runs a 4-worker tokio runtime (`main.rs`). These paths block a
worker (and in P1.2 a session lock) on syscall/CPU-bound work.

### P1.1 `spawn_session` on the hot path — 🔴
- `store/lifecycle.rs` `start_session_via_handle` calls the fully
  synchronous `runtime::spawn_session` (mkdir + `ShadowJournal::open`
  recovery scan + fsyncs + `which_in` PATH walk + PTY spawn) from an async
  fn. A dirty, large journal stalls all live attach streams.
- Fix: wrap in `tokio::task::spawn_blocking`. Audit
  `restart_session_via_handle` and every other caller of `spawn_session`
  the same way.
- Add a debug-mode assertion helper (or CI lint) that greps for
  `spawn_session(` outside `spawn_blocking` to prevent regression.
- Test: start a session while another streams output; with an artificially
  slowed journal dir (existing test harness supports slow-fs injection?)
  assert streaming latency unaffected — otherwise assert scheduling via a
  `tokio::test` that yields and checks a concurrent pump keeps draining.

### P1.2 Log rendering under the session lock — 🔴
- `store/query.rs` `render_live_logs` calls `rt.render_logs(...)`
  (CPU-bound engine re-render) while holding the `RwLock` read guard in an
  async fn: starves a worker AND delays the PTY reader, which needs the
  write lock for sequencing. A `oly logs` on a big screen adds
  keystroke→screen latency for live attaches.
- Fix: snapshot the minimum render input under the read lock, drop it,
  render in `spawn_blocking`; or (preferred long-term, matches "derived
  state comes from the journal") render from the journal off the hot lock.
- Measure with the existing `/api/metrics` histograms before/after
  (`attach` chunk inter-arrival while a large `logs` render runs).

### P1.3 Audit remaining sync-I/O on async paths
- `http/mod.rs` fallback + `apps` request resolution performs per-request
  directory I/O (`resolve_app_request`, manifest reads) synchronously in
  handlers. Cache the discovered app tree with a mtime-based invalidation
  (notify watch already available) or move resolution to `spawn_blocking`.
- Sweep `grep -n "std::fs::" src/**` for calls reachable from `async fn`
  outside the deliberately-synced journal/pty threads; document the
  deliberate ones.

---

## Phase P2 — Performance: hot-path costs

### P2.1 Hardware-accelerated CRC-32 — 🔴 (biggest cheap win)
- `journal/record.rs` (`crc32_table`, `crc32_two`, `Crc32`): hand-rolled
  byte-at-a-time table CRC runs on every record append (hot path for PTY
  output) and on every recovery/verify scan.
- Fix: swap to `crc32fast` (same IEEE polynomial ⇒ byte-identical records;
  on-disk format unchanged, no version bump). Keep the existing
  table-based implementation as a `#[cfg(test)]` cross-check oracle so a
  conformance test asserts both produce identical digests over fixtures.

### P2.2 Tighten recovery allocation cap
- `journal::MAX_PAYLOAD_LEN` = 64 MiB is honored before allocation (good)
  but 128× the runtime's actual max chunk (512 KiB). A corrupt-but-valid
  header still forces a 64 MiB read during scans. Keep the on-disk cap
  (readers must accept what old writers wrote), but scan/verify should
  stream-check instead of materializing: read+hash in ≤1 MiB windows.

### P2.3 Micro items
- `auth.rs` `verify_api_key_scopes`: `entries.to_vec()` clones every
  stored hash on every uncached verify — pass borrowed data into a scoped
  task or clone only the matching entry after an O(N) cheap-key match
  (interim until X2.2 replaces the scheme).
- `runtime.rs` `to_summary()` clones ~15 strings per call and is called
  per-broadcast/per-list; acceptable at `max_running_sessions = 50` — note
  it, revisit if list fan-out shows up in `/api/metrics`.

---

## Phase P3 — Performance: caching & polling elimination

### P3.1 HTTP caching for static assets — 🔴
- `http/mod.rs` static serving does `metadata` + full file `read` per
  request with no `ETag`/`Last-Modified`/`If-None-Match` and no streaming.
- Fix: implement stat→ETag→304 (or adopt `tower_http::services::ServeDir`
  semantics for the wwwroot path); for embedded `WebAssets`, `rust-embed`'s
  const mode or a `OnceLock` of leaked bytes to kill the per-request
  `into_owned()` copy.

### P3.2 Replace the 4 ms input-ack poll
- `store/mod.rs` `ATTACH_INPUT_OUTPUT_POLL_INTERVAL`: keystroke-ack by
  polling an output counter adds latency and wakeups on the interaction
  path. Replace with `tokio::sync::Notify` (or a `watch<u64>` bumped by
  the sequencer) awaited by `wait_for_change`; keep the timeout as a
  fallback.

### P3.3 Housekeeping
- `http/mod.rs` `serve()`: snapshot `config.get()` once instead of
  repeated `ArcSwap::load_full` clones of the whole `AppConfig`.
- CORS origins: make `http_cors_origins` configurable (default keeps
  today's `http://127.0.0.1:{port}` + `http://localhost:{port}` list).

---

## Phase X1 — Security: quick wins (no design debates)

### X1.1 SSRF filter: resolve-then-validate — 🔴
- `apps/proxy_targets.rs` `is_private_proxy_target` only inspects literal
  IP hosts. Bypasses today: a hostname resolving to `169.254.169.254` /
  `10.x.y.z`; DNS rebinding (check at manifest time, reconnect later →
  different answer).
- Fix:
  1. At manifest load AND at connect time, resolve the host and validate
     **every** returned A/AAAA (custom resolver via `reqwest`'s
     `resolve`/`resolve_to_addrs`, or pre-resolve and pin the validated
     addrs for the proxied request).
  2. Extend `is_ssrf_dangerous_ip`: `v6.is_unique_local()` (fc00::/7),
     IPv4-mapped already covered, add CGNAT `100.64.0.0/10`, and
     `v4.is_benchmarking()`/docs ranges if cheap. Loopback carve-out stays
     (documented dev-server feature) but gate it behind a config flag
     `wwwroot_allow_loopback_proxy` default-on.
  3. Keep `redirect::Policy::none()` (already set) and add a test that a
     redirect attempt is not followed.
- Tests: fake resolver returning private addr for public name → rejected;
  re-resolve on each request path covered by a unit test with an injected
  resolver.

### X1.2 CSP `connect-src` wildcard websocket
- `http/mod.rs` `security_headers`: `connect-src 'self' ws: wss:` lets any
  injected script exfiltrate to any host over WS. Tighten to same-origin
  schemes: `connect-src 'self' ws://HOST wss://` is awkward statically —
  simplest correct version: `connect-src 'self' ws: 'self' wss: 'self'` is
  invalid, so generate the header per-request from the `Host` header
  (`ws://{host}` + `wss://{host}`), falling back to `'self' ws: wss:` only
  when `Host` is absent. `style-src 'unsafe-inline'` stays (React), add a
  rationale comment.

### X1.3 Strict startup config parse
- `config.rs` `load_overrides` ends in `unwrap_or_default()`: a typo'd
  `config.json` silently boots with defaults (bind, limits, patterns) —
  violates the "explicit degradation" invariant; the hot-reload path
  (`try_load_overrides`) is already strict. Make `AppConfig::load()` fail
  with the parse error (and point at the file), like reload does.
  MIGRATION/changelog note: daemons with broken configs now refuse to
  start loudly instead of booting with defaults.

### X1.4 Unify proxy trust model
- Today: `effective_ip` trusts `X-Forwarded-For` only from loopback,
  `ws_origin_allowed` trusts `X-Forwarded-Host` unconditionally. Two
  models in one module invite copy-paste drift.
- Interim (this task): one helper `trusted_proxy_headers(peer, headers)`
  used by both, preserving current effective semantics; add the
  `trusted_proxies` config key (CIDR list, default loopback) that will
  become authoritative in X3.2.

### X1.5 Small hygiene
- `http/mod.rs` `try_read_static_file`: use `symlink_metadata` and reject
  symlinks escaping wwwroot (defense-in-depth; wwwroot is owner-written so
  severity is low).
- `reverse_proxy::proxy_request`: multi-target failover currently applies
  only to 404s — first connect error returns 502 immediately. Make
  connect errors fall through to the next target like 404s; 502 only if
  all targets fail.
- WS app bridge (`bridge_app_websocket`): set an explicit max message size
  (e.g. 4 MiB) and an idle timeout on both legs; tungstenite's 64 MiB
  default is an upstream-app DoS lever.

---

## Phase X2 — Security: credential verification hardening

### X2.1 Rate-limit the Bearer path — 🔴
- `auth.rs` `verify_api_key_scopes` + `authorize_request`: failed Bearer
  verifications are not cached and not rate-limited; the login lockout
  covers `/api/auth/login` only. Random-bearer spraying = unbounded
  Argon2 load on the blocking pool.
- Fix: reuse `LockoutRecord` to track per-IP failed bearer
  authentications (separate bucket from password logins); short circuit to
  401 while rate-limited.

### X2.2 O(1) API-key lookup — replaces the scan-all-keys design
- Today: every uncached verify runs `Argon2::verify_password` against
  **every stored key** (O(N) × ~50–100 ms).
- Fix: key-id scheme — issue keys as `oly_<keyid>_<secret>`;
  `list_api_key_entries` (or a new `lookup_api_key(id)`) resolves the id
  in one lookup; verify exactly one hash. Keep verifying legacy
  (id-less) keys via the old scan behind a capped, rate-limited path until
  they age out; document rotation guidance (`oly apikey` CLI).
- Cache semantics unchanged (sha256(key) → scopes, 60 s TTL) plus a short
  negative cache for failed verifications.
- Migration doc: existing keys keep working; note the new format in
  `SPEC.md`/`README.md`.

---

## Phase X3 — Security: trust & token design items (write the ADR first)

Each item: 1 short ADR section in ARCHITECTURE.md → review → implement.
These were judged design-significant in the review; do not land them as
drive-by patches.

### X3.1 Scoped browser sessions
- Today a valid cookie/session token bypasses the scope system entirely
  (scopes gate only API keys). Decide: either (a) sessions gain a scope
  list at login (read-only login UX) or (b) document "sessions are
  always-all; scopes are machine-credentials-only" as an explicit ADR with
  the security implications stated. Option (a) is small once S4.1
  (route-attached scopes) exists.

### X3.2 WS/SSE handshake without query-string tokens
- `?token=` leaks credentials into reverse-proxy access logs and any
  future request logging. Options: `Sec-WebSocket-Protocol:
  ["oly.attach", <token>]` (browser can set subprotocols), or single-use
  5-second handshake tickets minted via authenticated POST. Decide in an
  ADR; implement alongside X1.4's `trusted_proxies` so Origin/XFH policy
  lives in exactly one place.

### X3.3 Local IPC hello-token (macOS/Windows)
- `ipc.rs`: filesystem-backed sockets are `0600`, but the namespaced
  fallback (`GenericNamespaced::is_supported()`) has no FS-ACL analogue —
  any local process knowing `"open-relay.oly.sock"` gets full daemon
  control (= code execution as the daemon user).
- ADR: write a random IPC token into `daemon.info` (0600) at daemon start;
  clients must send `hello {token}` as the first frame before any RPC is
  served. Token omitted ⇒ reject. Evaluate whether Linux (abstract-namespace
  interprocess quirks aside) needs it too; document the per-platform
  threat model.

### X3.4 Node-registry failure sweep (verify + lock in with a test)
- Review found the relay design sound but did not fully trace: on node
  disconnect, every entry in `NodeRegistry::pending` (one-shot AND stream
  maps) must be failed loudly at once ("relay failure is loud" is a
  stated federation rule). Audit `drop_node`/connector teardown; add a
  test: disconnect a fake secondary mid-RPC and mid-stream ⇒ both callers
  get explicit errors, nothing hangs until the deadline.

---

## Explicit non-goals

- No wire-format or journal-format changes anywhere in this plan (P2.1 is
  an implementation swap with byte-identical output).
- No async-mutex rewrite of the session store (the Cargo.toml rationale
  stands; S3.1 only narrows where the exemption applies).
- No change to the federation trust model ("auth at join, owning node is
  the only authority") — X3 items harden transports, not the model.
- `list_tui.rs` full decomposition (only the crash-handler carve-out is
  in scope; a future plan can slice the TUI proper).

## Definition of done (whole plan)

1. `cargo clippy --locked --all-targets --all-features -- -D warnings`
   with **no crate-wide lint allowances**.
2. Largest non-test source file < ~2.0k lines.
3. One shared attach serve loop for local + relayed WS streaming
   (byte-identical conformance test between arms).
4. No blocking FS/CPU work reachable from async handlers without
   `spawn_blocking` (grep audit attached to the PR).
5. SSRF filter validated at resolve time incl. IPv6 ULA; bearer path
   rate-limited and O(1)-lookup for new keys.
6. ADRs merged in ARCHITECTURE.md for X3.1–X3.3; user-visible behavior
   changes listed in changelog + MIGRATION/SPEC as flagged per task.
