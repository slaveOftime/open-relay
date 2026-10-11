# Federation identity cleanup — finish gate

Implementation contract: [FEDERATION-IDENTITY-PLAN.md](FEDERATION-IDENTITY-PLAN.md).

Current verdict: **NOT RUN — implementation is still planned**.
No check below has passed merely because the checklist exists. The previously
reported 844 unit-test passes cover the old implementation, not this contract.

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
- Build/format/test processes have one owner. Preserve unrelated working-tree
  changes. Capture command exit codes; do not hide failures behind `| tail`.

## G1 — Public surface and complete credential removal

- [ ] `oly --help` has no `api-key` command; `api-key add/ls/remove` fail parsing.
- [ ] Join requires `--identity-pub-key <PRIMARY_PUB_KEY>` and accepts no
  `--key`, join-side `-k`, or `--ssh-pub-key`.
- [ ] Accept requires the secondary identity via `--identity-pub-key`; its
  `-k` shortcut is explicitly a public key, not a secret.
- [ ] Missing, blank, malformed, wrong-length, invalid/weak-point, private-key,
  OpenSSH, and old `ssh-ed25519` inputs fail in CLI and daemon validation.
- [ ] RPC/API-key commands and old federation auth variants cannot deserialize
  or reach a handler. IPC protocol is v14 and the federation version is checked.
- [ ] Saved joins contain only `name`, `primary_url`, and required canonical
  `primary_identity_pub_key`; obsolete auth fields fail strict parsing.
  Malformed saved config produces an actionable diagnostic, not a silent reset.
- [ ] Fresh DB has `node_identities` and no `api_keys`/`ssh_keys` table. Name and
  public-key uniqueness/upsert behavior match the plan. Original migrations
  0001–0009 are byte-for-byte unchanged; appended migration 0010 succeeds both
  on an empty DB and an isolated previously migrated DB, drops old credential
  tables without key conversion, and preserves session/push records.
- [ ] API-key CRUD, scopes, verifier/cache, HTTP authorization fallback, and
  `ssh-key` dependency are absent. Password Argon2 and unrelated crypto remain.

## G2 — Identity storage and trust anchors

- [ ] New daemon generates a 32-byte seed in `identity_key` and publishes the
  matching canonical `ed25519 <base64(raw32)>` in `identity_key.pub`.
- [ ] `daemon status` prints `IDENTITY-PUB-KEY:` matching that file and the
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
- [ ] Framing is `[tag:u8, sequence:u64 BE, ciphertext, GCM_tag:16]` (25-byte
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
  Database cleanup is accurately limited to migration 0010 dropping credential
  tables; no key/config conversion, file deletion, or unrelated data loss is
  claimed. API-key removal is recorded as the security-driven pre-`master`
  removal actually implemented.
- [ ] Residue searches are reviewed across tracked source, migrations, tests,
  docs, and Cargo files; no active old implementation remains. Historical
  removal notes, unchanged historical SQL migrations, migration 0010's DROP
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

## Final evidence report

Fill this in only after implementation and checks:

| Gate | Result (PASS / FAIL / BLOCKED) | Tests/probes and evidence |
|---|---|---|
| G1 Public surface/removal | NOT RUN | |
| G2 Identity persistence/trust | NOT RUN | |
| G3 Handshake/readiness | NOT RUN | |
| G4 Channel security | NOT RUN | |
| G5 Behavior/browser auth | NOT RUN | |
| G6 Suite/docs/platforms | NOT RUN | |

## Implementation status — interrupted after first attempt

Initial implementation was attempted and rolled back. The complete state is
reproduced below so the next cycle can resume without re-reading history.

### What landed
- **`FEDERATION-IDENTITY-PLAN.md`** and **`FEDERATION-IDENTITY-FINISH-GATE.md`**
  created, recording the design and acceptance contract. Decisions Q1–Q5 are
  resolved and incorporated.
- Branch `feat/federation-identity` prepared from the previous baseline.
  Existing documentation edits (`ARCHITECTURE.md`, `MIGRATION.md`,
  `README.md`, `skills/oly/SKILL.md`) and an unrelated `web/package-lock.json`
  churn are co-resident on this branch and pre-existing — not introduced by
  this implementation.

### What was attempted and reverted
- A draft `src/federation_crypto.rs` was written with the new encoding,
  transcript signature, channel-key derivation, and `ChannelSide` per-direction
  AEAD state, plus a `migrations/0010_federation_node_identities.sql` cleanup
  migration.
- Drafting the migration immediately forced a clash with existing call sites:
  `src/protocol.rs`, `src/cli.rs`, `src/main.rs`, `src/db.rs`, `src/http/auth.rs`,
  `src/http/nodes.rs`, `src/http/mod.rs`, `src/daemon/rpc_nodes.rs`,
  `src/daemon/rpc.rs`, `src/client/join.rs`, `tests/e2e_daemon.rs`,
  `tests/e2e/mod.rs`, and `tests/cli_errors.rs` all reach into
  `sshauth::{ChannelPhase, phase_encode_message, phase_decode_payload,
  normalize_public_key, public_key_line, sign_b64, verify_signature,
  b64_decode, derive_channel_keys, ed25519_priv_to_x25519,
  ed25519_pub_to_x25519, load_raw_seed, AEAD_KEY_LEN, node_join_payload}`.
- Converting that surface in one pass with a strict clean break produces a
  non-compiling tree mid-flight, so a one-shot conversion risks shipping the
  partial state. A safer approach is incremental commits with the test suite
  green after each phase, but the surface size still requires several focused
  PRs and a careful review of the wire-protocol changes (Hello /
  Join / Challenge / Ready / KeyConfirm / Joined sequence, sequence-numbered
  AEAD, and registry ownership) before they go near production.
- The draft `sshauth.rs` and migration 0010 were reverted; the source tree
  currently matches the documented baseline:
  `cargo test --locked --bin oly` → 844 passed; 0 failed; 1 ignored.

### Reality check

A full, evidence-grade pass against the finish gate is multi-PR work:

1. Build the new crypto + persistence module and wire it in aliased form,
   updating one call-site at a time and rerunning the suite between steps.
2. Replace the join handshake with Challenge / KeyConfirm / Ready / encrypted
   `Joined`, plus a per-direction counter seam. This is the highest-risk step
   and needs both producer-side and consumer-side conversions kept in lock-step,
   plus crypto review of transcript and nonce construction.
3. Migrate CLI flags (`--ssh-pub-key` → `--identity-pub-key`), accept-table
   file/state, and remove every `api_keys` reader/writer/RPC/CLI path,
   including the HTTP Bearer-token fallback. The `cargo test -p oly` suite
   covers the runtime paths; converting it should be re-run after each chunk.
4. Update existing federation e2e tests, replace `--key` paths with the new
   identity flag, and add wrong-pin / wrong-name / replay tests plus an
   authenticated HTTP daemon fixture.
5. Cross-platform CI (Linux / macOS / Windows PTY single-threaded), the full
   gated suite (lint + clippy + build + integration), and a docs sweep covering
   the architecture diagram, status labels, examples, and the migration notes.

Each of those steps needs its own pull request on `origin/HEAD` with a green
CI, not a single session-batched rewrite.

### What the next attempt should do

- Use a worktree separate from the current one (e.g. `git worktree add .worktrees/identity 41632e2`)
  so the existing documentation edits stay on a clean branch.
- Convert call sites incrementally: protocol types → cli → main → auth → nodes
  → daemon/relay → db / migrations → tests, with `cargo test --locked --bin oly`
  green after each step.
- Land the migration that drops `api_keys` / `ssh_keys` only after every
  in-tree reader is gone; SQLx will refuse the cleanup otherwise.
- Use the new `ChannelSide` struct (single owner, no `Clone`) as the unit of
  channel state in the relay task from the start. The current `ChannelPhase`
  enum approach leaks counters and is what justified the rework.
- Promote `_PIN` testing into a dedicated test type that exercises a fake
  secondary against the real primary, asserting no `Joined` is emitted before
  encrypted KeyConfirm verification.

### Final verdict during this attempt

**NOT RUN — implementation halted at the surface-conversion barrier; baseline
remained green throughout.**

Unresolved blockers at the time of halt:

- End-to-end handshake redesign (Challenge / Ready / KeyConfirm / encrypted
  `Joined`) and per-direction sequence-numbered AEAD.
- API-key removal from every code path (CLI, RPC, HTTP auth, daemon-side
  database handlers, save/load of prior `joins.json` contents).
- Migration that drops `api_keys` and `ssh_keys` without breaking existing
  on-disk databases.
- Cross-platform CI run + crypto review sign-off + Windows PTY gated tests.
- Documentation sweep for the new wiring, including the architecture diagram,
  `daemon status` labels, and the migration notes.

Skipped / untested behavior: every gate item in G1–G6 remains untested
because the implementation has not landed.

**Finish rule:** declare FINISHED only when G1–G6 are PASS, required evidence
exists, and no security, behavior, or platform blocker is outstanding.

