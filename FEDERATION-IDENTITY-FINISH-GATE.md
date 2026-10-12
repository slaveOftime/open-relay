# Federation identity cleanup — finish gate

Implementation contract: [FEDERATION-IDENTITY-PLAN.md](FEDERATION-IDENTITY-PLAN.md).

Current verdict: **BLOCKED for final release; P3 implemented and locally tested**.
The source/construction review is [FEDERATION-IDENTITY-P3-REVIEW.md](FEDERATION-IDENTITY-P3-REVIEW.md).
Independent crypto review and macOS/Windows CI are not claimed.

## Gate rules

- All mandatory checks must have a test name or an explicit reproducible probe,
  its command, actual result, and evidence. Do not check a box based on source
  edits, successful compilation, input delivery, or an idle daemon.
- Run against fresh isolated state directories and benign helpers. Never use
  the operator's real state directory or actual secrets.
- Removed flags/formats have no aliases/loaders. Negative rejection tests are
  allowed and required; they do not constitute compatibility support.
- Record failures, skipped checks, and untested platforms separately. Missing
  evidence or a mandatory skipped check means **BLOCKED**, not PASS.
- Build / format / test processes have one owner. Preserve unrelated working-tree
  changes. Capture command exit codes; do not hide failures behind `| tail`.

## G1 — Public surface and complete credential removal

- [ ] `oly --help` has no `api-key` command; `api-key add/ls/remove` fail parsing.
- [ ] Join requires `--id-pub-key <PRIMARY_PUB_KEY>` and accepts no
  `--key`, join-side `-k`, or `--ssh-pub-key`.
- [ ] Accept requires the secondary identity via `--id-pub-key`; its
  `-k` shortcut is explicitly a public key, not a secret.
- [ ] Missing, blank, malformed, wrong-length, invalid/weak-point, private-key,
  OpenSSH, and old `ssh-ed25519` inputs fail in CLI and daemon validation.
- [ ] RPC/API-key commands and old federation auth variants cannot deserialize
  or reach a handler. IPC protocol is v14 and the federation version is checked.
- [ ] Saved joins contain only `name`, `primary_url`, and required canonical
  `primary_id_pub_key`; obsolete auth fields fail strict parsing.
  Malformed saved config produces an actionable diagnostic, not a silent reset.
- [ ] Original migrations 0001–0009 are byte-for-byte unchanged; appended
  migrations 0010/0011 succeed both on an empty DB and an isolated previously
  migrated DB, drops old credential tables without key conversion, and
  preserves session/push records.
- [ ] API-key CRUD, scopes, verifier/cache, HTTP authorization fallback, and
  `ssh-key` dependency are absent. Password Argon2 and unrelated crypto remain.

## G2 — Identity storage and trust anchors

- [ ] New daemon generates a 32-byte seed in `identity_key` and publishes the
  matching canonical `ed25519 <base64(raw32)>` in `identity_key.pub`.
- [ ] `daemon status` prints `ID-PUB-KEY:` matching that file and the
  actual signing identity. Help/output/examples agree character-for-character.
- [ ] Restart preserves the seed/identity; corrupt or inaccessible seed fails
  federation closed without silent regeneration or secret fallback.
- [ ] Unix creation permissions are verified as private from creation; failures
  are not ignored. Windows file protection behavior is tested/documented.
- [ ] No legacy key-path probes, automatic moves, aliases, or import code remain.
- [ ] Accept binds one exact name/key pair. A fresh valid signature by a key
  registered as `worker-a` cannot authenticate as `worker-b`.
- [ ] Local-only operation and explicit `--no-auth` retain their documented
  behavior; `--no-auth` never disables federation identity verification.

## G3 — Handshake, readiness, and negative behavior

Use a benign raw-WebSocket helper or test proxy to inspect/control frames.
Tests must exercise the real daemon path, not only crypto helper functions.

- [ ] Two fresh daemons join with the new flags; actual remote RPC output proves
  usable encrypted communication, beyond a successful command exit/node listing.
- [ ] Exactly one secondary identity binds Hello, registered name, signature,
  channel derivation, and readiness. No second unverified key can be substituted.
- [ ] Wrong primary pin fails encrypted confirmation with bounded actionable
  status/stderr. Neither side reports success or a connected-node event; no
  application RPC is forwarded and no accepted node remains in the registry.
- [ ] Unregistered key, correctly re-signed wrong name, invalid signature, changed
  version/role/name/key/nonce, missing proof, and obsolete `api_key`/`ssh_key`
  methods fail before registration/application forwarding.
- [ ] Primary sends sealed `KeyConfirm`; secondary sends sealed `Ready`; final
  `Joined` is sealed. Plain `Joined`/`Ready` injection cannot report success.
- [ ] Duplicate/out-of-order handshake messages, missing Hello/Challenge/Ready,
  oversized input, and idle handshake all terminate within defined bounds.
- [ ] Cancellation interrupts a stuck handshake and reconnect backoff.
- [ ] Concurrent same-name joins cannot replace one another; failed handshakes
  cannot evict an existing legitimate node; stale disconnect cleanup cannot
  remove a newer connection. Connected-state events match actual ownership.
- [ ] Invalid/weak identity and all-zero/non-contributory DH are rejected without
  panic, fallback, node registration, or partial usable channel.

## G4 — Channel security and scope of claims

- [ ] Both sides derive matching keys for the same transcript, with distinct
  c2s/s2c keys. Repeated connections between the same identities derive different
  keys due to fresh transcript nonces.
- [ ] Signatures/HKDF bind version, roles, name, both keys, and both nonces using
  unambiguous transcript encoding. Test vectors and transcript review are saved.
- [ ] Data/RPC replies/events/keepalives after readiness are sealed in both
  directions; plaintext fallback is rejected, including encode-error paths.
- [ ] Tampered header/ciphertext/tag, wrong key, wrong direction, wrong sequence,
  replay within one connection, and replay from an older connection are rejected.
- [ ] Framing is `[tag:u8, sequence:u64 BE, ciphertext, GCM tag:16]` (25-byte
  minimum), with nonce `[0u8; 4] || sequence.to_be_bytes()`. Mutable, non-clonable
  per-connection state owns send/receive counters; header/direction/transcript
  are bound in AAD. Failed authentication does not advance state incorrectly;
  counter exhaustion closes before reuse. Rejected replay causes no duplicate
  observable side effect.
- [ ] Handshake/frame review reports no unresolved trust-binding or crypto
  construction blocker. Passing tests is not described as a formal crypto audit.
- [ ] Docs explicitly state static-DH **does not provide forward secrecy**.
  Freshness/replay fixes are not labeled as forward secrecy or SSH security.

## G5 — Existing behavior and browser authentication

- [ ] Converted full lifecycle covers remote start/list/send/logs/stop and
  asserts observable session output on the secondary.
- [ ] Streaming attach covers observable input/output, credits, ownership/fencing,
  and disconnect handling through the new encrypted path.
- [ ] Two distinct secondary identities connect under their registered names;
  outbound multi-connector coverage is preserved where applicable.
- [ ] Saved join and accept configuration survives restart; stop/rejoin, event
  relay, keepalive loss, and pending-RPC failure retain their behavior.
- [ ] Auth-enabled real daemon fixture uses an isolated known password's PHC
  hash through existing `OLY_AUTH_HASH_INTERNAL` foreground-child startup, not
  a `cfg(test)` bypass/new password environment variable. Real HTTP password
  login works; issued header/cookie sessions work; arbitrary Bearer values do not.
- [ ] Session expiry/revocation/logout, WS/SSE stream closure, origin checks,
  static-app/proxy protection, and browser auth failure/lockout remain covered.
  A `--no-auth` fixture alone cannot satisfy this check.
- [ ] API-key-looking Bearer credentials do not authorize HTTP routes, static
  apps/proxies, or federation. Login-session tokens remain supported as Bearer
  credentials; the removal is not a blanket Authorization-header ban.
- [ ] Credential prompt detection for literal `api key:` still works. `Ctrl-D`
  detach and unrelated web/push/session behavior are unchanged.

## G6 — Build, suite, documentation, and residue audit

Run focused tests first, then the full existing CI-equivalent checks:

```sh
# Fast feedback; test names/targets must exist and run nonzero test counts.
cargo test --locked --bin oly
cargo test --locked --test cli_errors
cargo test --locked --test e2e_daemon e2e_federation -- --test-threads=1
cargo test --locked --test e2e_federation_security -- --test-threads=1

# Full acceptance, with real exit codes and retained logs.
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo build --locked --bin oly
cargo test --locked
# Windows full suite: cargo test --locked -- --test-threads=1

git diff --check
```

- [ ] Focused checks run the new tests (record counts; zero matching tests is not
  a pass). No old API-key positive-path test remains.
- [ ] Full suite includes integration/e2e targets, not only `--bin oly`; all
  failures resolved, baseline ignored tests explained, no newly hidden ignores.
- [ ] Existing CI build/test matrix passes on Linux, macOS, and Windows. A local
  unsupported optional-feature build is recorded as blocked until CI evidence
  satisfies the required all-features lint check.
- [ ] README, architecture diagram, migration notes, CLI help/status/hints, and
  daemon skill consistently describe the implemented flags/format/state machine.
  Database cleanup is accurately limited to migration 0011 dropping credential
  tables; no key/config conversion, file deletion, or unrelated data loss is
  claimed. API-key removal is recorded as the security-driven pre-`master`
  removal actually implemented.
- [ ] Residue searches are reviewed across tracked source, migrations, tests,
  docs, and Cargo files; no active old implementation remains. Historical
  removal notes, unchanged historical SQL migrations, migration 0011's DROP
  statements, schema-cleanup fixtures, explicit negative rejection tests, this
  plan/gate, and generic credential-prompt examples are justified old-name
  mentions. None provides a runtime old-credential authorization path.
- [ ] Unrelated edits (including pre-existing `web/package-lock.json`) are
  preserved and not attributed to this implementation. Test daemons/probes are
  stopped and temporary resources cleaned up.

Suggested residue audit (nonzero grep status may mean “no matches”; assess it,
not as a build failure):

```sh
git grep -n -E 'ApiKey|api_key|api-key|api_keys|SCOPE_NODE|ssh_pub_key|ssh_primary_pubkey|ssh-pub-key|ssh_host_key|SSH-PUB-KEY|sshauth|ssh_keys|ssh_key::|ssh-ed25519' -- \
  src tests migrations Cargo.toml Cargo.lock README.md ARCHITECTURE.md MIGRATION.md skills
```

## Final evidence report (P3 checkpoint)

| Gate | Result | Evidence / remaining work |
|---|---|---|
| G1 Public surface/removal | PARTIAL | IPC v14/federation v2; legacy auth union removed and rejected; `ssh-key` dependency and `sshauth.rs` deleted. Migration 0010 adds `node_identities`, 0011 drops both old credential tables; upgrade test preserves session/push records/checksums. Saved config rejects obsolete fields, but malformed-config diagnostic behavior is still a P2 follow-up. |
| G2 Identity persistence/trust | PARTIAL | Production and tests share identity creation; Unix mode 0600 checked; exact name/key authorization and conflict-safe upsert tested. Windows file protection not verified. |
| G3 Handshake/readiness | LOCAL PASS | Actual binary connector and raw peers verify sealed KeyConfirm/Ready/Joined, wrong pin, fresh Join replay, re-signed wrong name, invalid proof/weak identity, legacy/repeated/oversized/out-of-order messages, idle/missing Ready timeout, connector timeout/stop, duplicates and stale-owner cleanup. Failed handshakes cannot forward an RPC or create a session. |
| G4 Channel security | PARTIAL | Production channel has directional, transcript/header-bound AEAD and terminal replay rejection; mutation/exhaustion/reflection/prior-session tests plus fixed crypto vectors. Real SSE replay yields exactly one event, then disconnect; pending RPC fails promptly. Source review recorded; independent cryptographic review remains BLOCKED. Static-static DH explicitly does not provide forward secrecy. |
| G5 Behavior/browser auth | LOCAL PASS for exercised paths | Existing remote lifecycle and credited attach pass. Actual auth-enabled daemon fixture uses existing `OLY_AUTH_HASH_INTERNAL` PHC path; HTTP password login, issued Bearer/cookie sessions and logout pass; arbitrary machine Bearer fails. Full local suite retains origin/session/revocation coverage. |
| G6 Suite/docs/platforms | BLOCKED | Full Linux suite passes. Strict fmt/clippy hit pre-existing unrelated residue listed below; focused P3 checks pass. macOS/Windows CI and independent crypto review unrun. |

### Commands and actual evidence

Logs retained under `/tmp/p3-*.log` on this implementation host (not permanent
CI artifacts). All commands capture the real exit status, not pipeline status.

- `cargo build --locked --bin oly`: PASS, `/tmp/p3-build-final.log`.
- `cargo test --locked`: PASS; final full suite log `/tmp/p3-full-suite-final.log`:
  **858 unit passed + 1 existing ignored**, 21 CLI-error, 25 daemon e2e,
  **28 federation-security target** (18 production crypto unit tests reused in
  this target + 10 real-daemon adversarial tests), 22 PTY, and 4 other active
  integration tests. The existing ignored test is a manually invoked Unix
  isolated PTY helper. One platform-gated target ran zero tests; not evidence
  for that platform. No new ignored tests.
- `cargo test --locked --test e2e_federation_security -- --test-threads=1`:
  PASS, **27 tests** (18 crypto + 9 real-daemon security),
  `/tmp/p3-security-final.log`. This includes pending-RPC failure and stronger
  zero-session side-effect assertions. A subsequent additional daemon restart
  test passed individually (`/tmp/p3-restart.log`) and in the final full suite
  (**28 security-target tests**).
- `cargo test --locked --test cli_errors`: PASS, **21 tests**,
  `/tmp/p3-cli.log`.
- `cargo test --locked --test e2e_daemon e2e_federation_attach -- --test-threads=1`:
  PASS, **1 test**, `/tmp/p3-attach.log`. Initial full-daemon run found a raw
  IPC fixture hardcoded to version 13; updated to 14 and the full suite passed.
- `cargo clippy --locked --all-targets --all-features -- -D warnings`:
  FAIL from existing `src/client/attach.rs:533` infallible-destructuring lint
  and `tests/e2e_attach_scrollback.rs:44` unused-mut. Both code regions exist
  unchanged in the pre-P3 commit. New federation lint findings were fixed.
- Same clippy command with explicit `-A clippy::infallible_destructuring_match
  -A unused_mut`: PASS (`/tmp/p3-clippy-focused.log`). This is a diagnostic
  run, **not a substitute for the mandatory strict lint gate**.
- `cargo fmt --all -- --check`: pre-existing formatting residue in
  `src/client/list_tui/{app,tests}.rs`, `src/http/ws.rs`, `tests/e2e_pty.rs`.
  Full formatter was inspected; unrelated formatting reverted. Changed
  P3 Rust files are formatted separately, without child-file traversal.

### Implemented in P3

- Length-prefixed transcript; strict secondary signature; fresh per-session
  HKDF keys; exact registered name/key trust; sole ordered bounded handshake.
- Mutable non-cloneable channel with tag/sequence/direction/transcript AAD;
  counters continue through confirmation into RPC/events/sealed keepalives.
- Atomic duplicate rejection and generation-owned disconnect; independent
  bounded federation failure throttling with HTTP `--no-auth` unaffected.
- New identity table used in production; no `INSERT OR REPLACE` key theft.
  Migration history unchanged; 0011 deletes obsolete credential records.
- Legacy module/dependency removed; README/architecture/migration/status skill
  updated to the actual wire contract; construction review and vectors saved.

### Explicit remaining gates

- Independent cryptographic review; macOS/Windows build/test and Windows key ACL
  verification; strict project-wide fmt/clippy baseline cleanup.
- Strict malformed saved-config diagnostics (the loader still treats parse
  failure as an empty list; does not enable a legacy auth path).
- Long-duration sealed-keepalive loss and Windows-specific PTY behavior are not
  newly verified here beyond existing local suite coverage.
- WS allocation cap is 64 MiB for supported application messages; the 4096-byte
  handshake payload bound is enforced after assembly, before parsing. Do not
  claim a 4096-byte pre-auth allocation bound.

**Finish rule:** this P3 checkpoint is not a claim that the whole change is
FINISHED. Final merge readiness requires all mandatory G1–G6 evidence and no
outstanding security/platform blocker.
