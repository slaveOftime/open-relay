# Federation identity cleanup — implementation plan

Status: **P1/P2 and P3 implemented locally; final release gates remain blocked**.
P3 construction and regression evidence are recorded in
[FEDERATION-IDENTITY-P3-REVIEW.md](FEDERATION-IDENTITY-P3-REVIEW.md).
Final acceptance is defined in
[FEDERATION-IDENTITY-FINISH-GATE.md](FEDERATION-IDENTITY-FINISH-GATE.md).

## Scope and decisions

Replace misleading SSH terminology with **node identity authentication**, remove
the API-key feature, and make the sole remaining federation path fail closed.
This is a clean break for federation: no old flag aliases, old key/config
loaders, old wire variants, or key/config conversion. Database migration history
is preserved, with one appended cleanup migration; this is not a legacy
federation authentication path. Acceptance uses isolated fresh state and a
previous-schema database fixture to verify cleanup without losing session data.

Browser password authentication, expiring/revocable login sessions, cookies,
Bearer **session tokens**, WebSocket origin checks, and explicit `--no-auth`
behavior remain. Removing API keys does not mean removing these mechanisms.
In particular, `--no-auth` disables browser/HTTP login, not federation identity
verification or channel encryption.

## Review of the earlier recommendation

The naming direction was correct, but the proposed mechanical rename was not
sufficient:

1. **The current canonical public-key string is not OpenSSH format.**
   `src/sshauth.rs::public_key_line` emits `ssh-ed25519 <base64(raw32)>`.
   Real OpenSSH public-key lines encode an SSH binary blob containing an
   algorithm string and the key bytes, not just the raw key bytes. The parser
   separately accepts those real lines. The previous claim that oly's emitted
   line was RFC-8709/OpenSSH-compatible was incorrect.
2. **Persistence is part of the change.** `JoinConfig` in `src/client/join.rs`
   has serialized `api_key` and `ssh_primary_pubkey` fields. `src/db.rs` uses
   `ssh_keys` and `api_keys` tables. Renaming only flags/files misses these.
3. **API keys are not confined to joins.** `src/http/auth.rs::authorize_request`
   also accepts the same keys as scoped HTTP Bearer credentials. Removing only
   `NodeJoinAuth::ApiKey` leaves a working alternate access path.
4. **This is not currently a fresh challenge-response handshake.** The join
   signature has no per-connection challenge. Use precise terminology, rather
   than inheriting SSH security properties through names.
5. **Existing acceptance checks do not prove pin verification.** The primary
   sends plaintext `Joined`, registers the node before encrypted confirmation,
   and the connector calls `attempt_report.joined()` on that plaintext reply.
   `tests/e2e_daemon.rs` explicitly avoids a wrong-primary-pin experiment.

### Source-observed security gaps to close

These are source observations, not exploit experiments or a complete crypto audit.

- `src/http/nodes.rs` looks up the signing key using `get_ssh_key_entry`, then
  ignores the registered name. A key registered for one name can sign a fresh
  payload claiming another name. The existing name-substitution test changes
  the name without re-signing, so it does not cover that authorization gap.
- `Hello.public_key` is separately retained and used for ECDH, without checking
  that it is the same identity verified in `Join.auth.public_key`.
- `derive_channel_keys` uses static-static DH, identities, and direction labels
  only. Reconnecting the same two identities produces the same channel keys;
  contrary comments about different keys for each connection are inaccurate.
- Sealed frames have random nonces but no receive-side replay tracking. AEAD
  integrity alone is not replay protection.
- Primary handshake reads are not bounded by a handshake deadline. Repeated
  `Hello` messages are accepted by the current loop.
- `is_connected()` followed by `connect()` is not an atomic duplicate-name
  check. Disconnect is by name rather than connection ownership.

Renaming and API-key removal must not be used as evidence that these gaps are
fixed. The implementation below includes bounded handshake hardening; a naming
cleanup alone cannot pass the security finish gate.

## Target naming and formats

| Surface | Target |
|---|---|
| Join flag | `--identity-pub-key <PRIMARY_PUB_KEY>`; required, no `-k` short alias |
| Accept flag | `--identity-pub-key <SECONDARY_PUB_KEY>`; required; retain `-k` here |
| CLI/RPC accept field | `identity_pub_key: String` |
| Saved join / JoinStart RPC | `primary_identity_pub_key: String`, not `Option` |
| Private seed file | `<state>/identity_key` — raw 32-byte Ed25519 seed |
| Published public file | `<state>/identity_key.pub` |
| Status label | `IDENTITY-PUB-KEY:` |
| Canonical public line | `ed25519 <standard-base64(raw32)>` |
| Module | `src/federation_crypto.rs`, replacing `src/sshauth.rs` |
| Existing identity type | Keep `NodeIdentity`; avoid unrelated relocation |
| Accept table | `node_identities`, storing registered name and canonical public key |
| Documentation | “Ed25519 node identity authentication”; transport is WebSocket |

Accept surrounding whitespace, then require exactly an `ed25519` tag and one
base64 body. Validate length, Ed25519 point validity, and suitability for DH;
reject invalid/weak identities. Re-encode to one canonical form. Reject old
`ssh-ed25519` lines, real OpenSSH lines, other algorithms, extra tokens, and
private-key input. Do not retain `ssh-key` just for import compatibility.
Use typed key bytes after parsing; do not repeatedly split strings in crypto code.

Operator-facing workflow:

```sh
# On each daemon: obtain its public identity; never copy the private seed.
oly daemon status

# On the primary: bind this secondary name to its public identity.
oly node accept --name worker-1 --identity-pub-key "ed25519 <secondary-base64>"

# On the secondary: pin the primary identity through an independent trusted channel.
oly join start --name worker-1 --identity-pub-key "ed25519 <primary-base64>" \
  http://primary-host:15443
```

## Resolved implementation decisions

These resolve the five follow-up questions; implementation should proceed with
these choices rather than introducing additional compatibility alternatives.

1. **Handshake observability:** use test-only raw-WebSocket peers/proxies against
   real daemon processes. Test the real connector with a controlled fake primary
   and observe CLI status/stderr, node-state events, and forwarded side effects.
   Do not implement a parallel `join_for_test` handshake: a mirrored protocol can
   pass while production is broken. Keep reusable production handshake helpers
   small and testable, but do not add a public state-event API solely for tests.
2. **Authenticated HTTP fixture:** compute an Argon2 PHC hash for an isolated
   known test password, launch the actual binary with `--foreground-internal`
   and existing `OLY_AUTH_HASH_INTERNAL`, then exercise real HTTP login and
   issued sessions. This is the existing detached-child initialization path,
   not a `cfg(test)` auth bypass. `OLY_DAEMON_PASSWORD` does not exist and
   `rpassword::prompt_password` does not simply read piped stdin. Do not add a
   plaintext-password environment variable or new startup flag for this fixture.
   Interactive password-prompt UI can be checked separately through a PTY.
3. **Database cleanup:** leave migrations 0001–0009 byte-for-byte unchanged and
   append `0010_add_node_identities.sql` to create `node_identities`, then
   `0011_drop_legacy_node_credentials.sql` to drop `api_keys` and `ssh_keys`
   once all readers are removed. Do not copy any legacy credentials. Preserve
   session, push-subscription, and other unrelated tables/records. Test both
   empty-database initialization and an isolated database with migrations
   0001–0009 already applied. This avoids SQLx missing/checksum failures without
   retaining credential compatibility. Leaving dead tables would be functional
   once all readers are gone, but dropping them removes obsolete credential
   records and makes the active schema unambiguous.
4. **Channel sequences:** use mutable per-connection channel state, owned by the
   existing single relay task, with separate send/receive counters. Do not use
   `AtomicU64` or clone live channel state. Freeze framing as
   `[sealed_tag:u8, sequence:u64 big-endian, ciphertext, GCM_tag:16]`; derive
   the 12-byte nonce as `[0u8; 4] || sequence.to_be_bytes()`. The minimum frame
   length is 25 bytes, not 37; a redundant explicit random nonce is unnecessary
   with fresh per-connection/per-direction keys. Bind tag, sequence, direction,
   and transcript hash in AAD. Advance receive state only after successful AEAD
   authentication and parsing; any failed frame terminates the connection.
   Reserve the exhausted counter value and close before increment/reuse.
5. **`--no-auth`:** preserve its HTTP-login semantics. It already does not bypass
   federation signature checks; the new flow must also require the registered
   identity and encrypted confirmation regardless of HTTP auth being enabled.
   Federation failure throttling must have its own state instead of depending
   on optional browser `AuthState`.

Implementation order remains P0–P5. These are reviewable steps of one complete
change, not a decision to omit P3 hardening. Final cross-platform CI and crypto
review evidence remain mandatory gate items; unavailable checks are BLOCKED,
not presumed to pass.

## Implementation sequence

### P0 — Baseline and write ownership

- Record `git status` and baseline test results before changing implementation.
- Preserve existing documentation edits and unrelated `web/package-lock.json`
  changes; do not reset or reformat unrelated files.
- One owner handles `src/cli.rs`, `src/main.rs`, `src/protocol.rs`, migrations,
  Cargo files, formatting, and builds. Tests/docs can be delegated after that
  owner freezes the new CLI, persistence, and handshake contracts.
- Use existing isolated e2e helpers and benign shell probes; never use real
  daemon state, credentials, or business sessions.

Completion: baseline failures/blockers are recorded separately from new failures.

### P1 — Identity representation and persistence

Files: `src/sshauth.rs` → `src/federation_crypto.rs`, `src/http/mod.rs`,
`src/main.rs`, `src/db.rs`, `migrations/`, `Cargo.toml`, `Cargo.lock`.

- Implement the new canonical line/parser and update module references.
- Rename identity file paths and status-related accessors/comments.
- Create new seed files with user-private permissions from the start on Unix,
  rather than writing permissively and ignoring a later chmod failure. Use
  platform-appropriate protection on Windows and verify the actual behavior.
- Derive the public file from the persisted seed. A corrupt/inaccessible seed
  disables federation with a clear error; never replace it silently or fall
  back to unauthenticated federation. Local-only operation may remain available.
- Append `0010_add_node_identities.sql`; preserve all existing migration
  files unchanged. Create `node_identities` with `name TEXT PRIMARY KEY`,
  `public_key TEXT NOT NULL UNIQUE`, and a creation timestamp. A public key
  identifies one registered name per primary; do not retain wildcard names.
- Implement accept-table lookup by claimed name and exact canonical key match.
  Use an explicit upsert on the name, not broad `INSERT OR REPLACE` semantics;
  attempts to bind a key already owned by another name must fail clearly.
- In migration 0011, drop `api_keys` and `ssh_keys` without copying old records.
  Keep historical migrations 0003, 0007, and 0008 unchanged. Verify SQLx applies
  the complete sequence to an empty database and to a previous-schema fixture,
  preserving unrelated session/push records and applied migration checksums.
- Remove the `ssh-key` dependency when its final production/test use is gone.
  Keep Argon2 for browser passwords and crypto dependencies used by web push.

Completion: focused parser, identity persistence, fresh-database, and
previous-schema cleanup tests pass. There are no old-key-path probes or key
conversion routines; schema cleanup is only the appended SQL migration.

### P2 — CLI, config, RPC, and API-key removal

Files: `src/cli.rs`, `src/main.rs`, `src/protocol.rs`, `src/client/join.rs`,
`src/daemon/rpc.rs`, `src/daemon/rpc_nodes.rs`, `src/db.rs`,
`src/http/auth.rs`, `src/http/mod.rs`, `src/http/nodes.rs`.

- Add the target flags/help and make the primary identity mandatory in the CLI,
  daemon-side validation, saved config, and `JoinStart` RPC.
- Persist only `{name, primary_url, primary_identity_pub_key}` in `joins.json`.
  Normalize before saving. Use strict join-config parsing to reject obsolete
  auth fields, and report malformed config rather than silently making it an
  empty list. No loader for old field names or secret-based configs.
- Remove `Commands::ApiKey`, argument types, command dispatch, all
  `ApiKey{Add,List,Remove}` request/response variants, `ApiKeySummary`,
  default-scope functions, daemon handlers, and database CRUD/record types.
- Remove `--key` and its `-k` alias from **join start**. `-k` on **node accept**
  remains a public-identity shortcut, not a shared secret.
- Remove `NodeJoinAuth::ApiKey` and `SshKey`; replace the auth-method union with
  a single typed identity proof for the new handshake. Do not keep an `auth`
  method selector that only exists for deleted credential alternatives.
- Remove HTTP API-key scope constants/classification/validation, verification,
  cache/TTL/cleanup, and the API-key Bearer fallback. Simplify
  `authorize_request` and every caller without changing session-token handling.
- Keep password hashing/login, session extraction and validation, logout,
  revocation watches, expiry, origin checks, and IP lockout.
- Bump CLI/daemon `PROTOCOL_VERSION` from 13 to 14. Introduce an explicit
  federation version for the new wire contract and reject other versions;
  do not imply that the IPC version alone fences WebSocket node handshakes.

Completion: CLI/RPC/config rejection tests pass, fresh DB has no API-key table,
HTTP still accepts valid login sessions and rejects arbitrary Bearer strings,
and there is no credential-based alternate federation branch.

### P3 — Sole identity handshake and encrypted readiness

Files: `src/protocol.rs`, `src/federation_crypto.rs`, `src/http/nodes.rs`,
`src/daemon/rpc_nodes.rs`, `src/node/registry.rs`.

Freeze a single bounded state machine before converting tests:

1. Secondary sends plaintext `Hello` with federation version, name, canonical
   secondary identity, and a fresh 32-byte secondary nonce.
2. Primary returns plaintext `Challenge` with a fresh 32-byte primary nonce.
   It does **not** provide a replacement trust-root key. The primary identity
   remains the operator-supplied pin.
3. Secondary sends `Join` carrying an Ed25519 signature of the full transcript.
   The transcript uses explicit length-delimited encoding and domain separation:
   version, role labels, name, both raw identity keys, and both nonces.
   There is only one secondary identity in the transcript/state; `Join` cannot
   introduce a second unbound public key.
4. Primary checks the exact registered name/key pair and the signature, rejects
   weak keys/non-contributory (all-zero) DH, derives the channel keys, and sends
   a **sealed `KeyConfirm`**. No node registration or success event yet.
5. Secondary decrypts and validates that confirmation, then sends a
   **sealed `Ready`** bound to the same transcript.
6. Primary verifies `Ready`, atomically registers the connection if the name is
   free, and sends **sealed `Joined`**. Only that final reply permits the
   connector to call `attempt_report.joined()` and relay application traffic.

Additional invariants:

- HKDF derives fresh per-connection, per-direction keys from static-static
  X25519 DH and the transcript hash containing both fresh nonces. Reconnecting
  the same identities must not produce the same keys.
- Sealed framing is `[tag:u8, sequence:u64 BE, ciphertext, GCM_tag:16]`, with
  nonce `[0u8; 4] || sequence.to_be_bytes()`. Mutable channel state owns separate
  send/receive counters and cannot be copied or cloned. Bind the header,
  direction, and transcript hash to AAD; require the next expected receive
  sequence. Reject repeats/out-of-order frames and terminate before counter
  overflow. Do not silently fall back to plaintext encoding or decoding.
- An overall handshake deadline and bounded message sizes apply on both sides;
  repeated/out-of-order handshake messages fail. Cancellation must interrupt
  handshake reads and connector retries.
- Failed pin confirmation, malformed handshakes, timeout, or disabled identity
  never publish a connected node or allow any application RPC.
- Registry insertion rejects duplicate names atomically. Cleanup uses a
  connection ownership/generation token so a stale task cannot remove a newer
  connection. Failed confirmations must not disconnect an existing node.
- Rate-limit failed identity attempts without coupling federation's trust
  decision to whether browser authentication is enabled.

This design retains static identity-derived DH: **it does not provide forward
secrecy**. Fresh nonces and sequence numbers provide session separation and
replay protection, not forward secrecy. Ephemeral-DH/Noise/TLS redesign is a
separate project; do not claim those properties in this change. The transcript,
nonce construction, and state transitions require explicit review before merge.

Completion: adversarial handshake/frame tests pass, including wrong-pin failure
before success reporting and replay rejection. A raw-frame test proves the
transition, not merely a node appearing in a list.

### P4 — Convert and extend tests without losing behavior coverage

Files: `tests/e2e_daemon.rs`, `tests/e2e/mod.rs`, `tests/cli_errors.rs`, and
focused unit tests beside changed modules.

- Add shared helpers to obtain generated identities, register a secondary,
  and join using a primary pin. Remove test-only OpenSSH key generation.
- Convert API-key-backed full lifecycle and streaming attach tests; keep their
  remote start/list/send/logs/stop and observable output/input assertions.
- Replace the “same API key, different names” test with two distinct secondary
  identities registered under two names. Preserve the multi-connector coverage
  with joins to distinct primaries where needed.
- Replace API-key rejection/revocation tests with obsolete-method rejection,
  unregistered identity, registered-name mismatch with a **fresh valid
  signature**, invalid signature, and wrong-primary-pin tests.
- Cover persistence and restart of identity files, accept records, and joins,
  plus stop/rejoin, duplicate names, relay disconnect, and outstanding-RPC
  failure. Test failed handshakes do not evict legitimate connections.
- Add an authenticated HTTP daemon fixture using a known isolated password's
  PHC hash and existing `OLY_AUTH_HASH_INTERNAL` on the actual foreground child.
  Exercise real HTTP login and session validation. Existing `start_daemon_http`
  passes `--no-auth`, so it cannot prove browser-auth preservation or removal
  of the HTTP API-key fallback by itself. Do not add an auth bypass or alternate
  handshake implementation just for tests.
- Retain generic `api key:` credential-prompt recognition; it concerns output
  from supervised programs, not oly's deleted API-key feature.

Completion: all converted lifecycle tests run against the sole encrypted path;
negative tests assert no success notification, no registered node, no forwarded
side effect, and bounded actionable failure where applicable.

### P5 — Documentation and final verification

Files: `README.md`, `ARCHITECTURE.md`, `MIGRATION.md`, `skills/oly-daemon/SKILL.md`,
other affected skills/help, and the two plan/gate documents.

- Update flags, key filenames, status labels, encoding, and public-key exchange
  examples together. Explain there is no SSH transport, SSH agent, system SSH
  identity, OpenSSH import, or host-key-discovery endpoint.
- Replace stale handshake diagrams and security claims with the implemented
  transcript, encrypted confirmation, readiness, and replay invariants.
- Record API-key removal for security reasons before merge to `master` in
  migration notes; include the HTTP machine-key path, not only node joins.
  Describe the clean-break setup using new daemon identities and registrations.
  Explain exactly that migration 0011 drops old credential tables without
  converting their records; unrelated session/push data is preserved. Old key
  files/configs are not automatically moved, converted, or deleted.
- Replace the current migration prose claiming broader automatic state deletion
  and completed removal before implementation. Remove rollback history from
  architecture prose where it obscures the actual current design.
- Preserve `Ctrl-D` detach documentation and generic credential-prompt examples.
- Run the linked finish gate and attach actual evidence. A green unit suite or
  successful `oly join start` input delivery alone is not completion.

Completion: every mandatory finish-gate item passes; no outstanding security or
cross-platform blocker is hidden by a skipped check.
