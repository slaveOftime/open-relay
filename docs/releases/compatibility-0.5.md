# Compatibility for 0.5.x

The beta tests a local-first interactive session supervisor, not a general
agent framework, application sandbox, or multi-user authorization service.
It is suitable for early adopters who can back up state and coordinate upgrades.

## Supported user-facing contracts

- **CLI:** the documented commands, argument boundaries, and wait exit codes.
  `restart` creates a new session/process; it does not recover a lost PTY or
  resume an agent's internal conversation. Input delivery is not task completion.
- **JSON:** documented fields, types, units, and null/missing semantics. New fields
  may be added; consumers must ignore unknown fields and handle optional values.
  Logs carry `v: 1`; list/info currently use their documented schemas without a
  common version envelope. Do not scrape the human table or colored output.
- **Cursors:** opaque, session/incarnation-bound tokens. Wrong-session, stale,
  past-end, or unavailable history fails explicitly; never interpret a token as
  a timestamp or synthesize one. Rendered-view cursors are observation positions,
  not proof that every earlier byte was delivered.
- **Configuration:** flat JSON. Existing malformed files fail startup; reload
  keeps the last good config on error. Unknown keys warn. Removed recording-cap
  keys fail explicitly; `journal_retention_days` is permanent stopped-session
  cleanup, not a live byte quota.
- **Shared control:** multiple controllers may input concurrently; last successful
  resize wins. Observe mode is coordination, not a separate restricted principal.

## Node federation

Pinned-identity federation is a supported interface in 0.5.x. The trust model
is stated here rather than implied; setup is in
[Remote access](../guides/remote-access.md#connect-machines).

- **Identity:** one Ed25519 identity per daemon, encoded as
  `ed25519 <standard-base64(raw32)>`. A primary binds one identity to one
  registered name, and `public_key` is `UNIQUE`, so one identity cannot be bound
  to two names. The accept table and the secondary's configured pin are the
  trust anchors and are re-checked on every join. No SSH integration, OpenSSH key
  import, TOFU, key-discovery endpoint, or shared API-key fallback.
- **Rotation:** `oly node accept --name <name> --id-pub-key <key>` replaces that
  name's key atomically. Verify the replacement public identity out of band
  first. The previous key then fails the next join; an already-connected node is
  **not** torn down, so restart the connector on the secondary
  (`oly join stop`, then `oly join start`) to force re-verification.
- **Transport:** WebSocket, not SSH. AES-256-GCM seals application frames and
  keepalives under per-direction monotonic sequences; replay and reordering are
  rejected terminally. HTTP `--no-auth` does not relax federation identity checks
  or encryption.
- **Known limit — no revocation command:** there is no primary-side `node
  remove` in 0.5.x. An accepted name's row cannot be deleted through the CLI, and
  because `public_key` is `UNIQUE` a retired machine's identity cannot be rebound
  to a different name. Re-accept a name to replace its identity; otherwise
  generate a new identity on the replacement machine. A leaked seed cannot be
  contained by configuration alone — also block the transport path.
- **Known property — no forward secrecy:** the channel key is static-static
  X25519 over the two long-lived identities, mixed with HKDF-SHA-256 over the
  signed transcript. Later private-seed compromise can decrypt recorded
  federation traffic, so run nodes on trusted transport (SSH tunnel,
  Tailscale/Headscale, private network).
- **Versions:** no mixed-version rollout is supported. The signed handshake binds
  the version, so run compatible CLI/daemon versions on every node and upgrade
  them together.

## State and recordings

0.3 recordings are not automatically imported. SQLite migration is forward-only;
rollback requires restoring a pre-upgrade backup. Never use an old binary on a
migrated database. Follow [Migration to 0.5](./migration-0.5.md).
Private state includes identity keys, push keys, and session data.

During beta we aim to retain readability of earlier **0.5 beta** recordings,
but do not yet promise a frozen on-disk format. Any incompatible beta update must
be called out in its release notes with an export/backup procedure before upgrade.
Do not use beta state as the sole copy of irreplaceable work. Export the canonical
filtered bytes and retain protected state backups; exports are not raw journals.

Live PTYs survive client disconnect, not daemon crash/stop or host reboot. Group
sync has a pending-unsynced window; displayed output is not automatically proof
of fsync completion. No supported live journal byte cap is provided in this beta.

## Experimental/internal interfaces

- **Private IPC and browser WS envelopes:** use the packaged CLI/web UI. Direct
  consumers follow source and fixtures at their own risk; wire versions can change.
- **Custom web apps/proxies:** opt-in reviewed operator code on the control-plane
  origin (`trusted_apps_enabled`), not untrusted agent output. Only loopback literal
  proxies are supported. Disabling blocks new requests but does not retroactively
  close open documents or connections. Review services before exposing them.
- **Attention hints and resume extraction:** prompt/silence heuristics and configured
  child-output suggestions, not authorization, semantic task state, or automatic execution.

### Private protocol consumers

CLI/daemon IPC uses Protocol **14**. One-shot messages are newline-delimited
`{"version":14,"payload":...}` envelopes. Attach sends a JSON init line, then
`[u32 LE payload_len][u8 tag][payload]`: output tag 1 carries an offset and bytes;
control tag 2 carries bare JSON `RpcResponse`. Browser WebSocket uses a separate
binary envelope over the attach contract.

Source and fixtures are authoritative: `src/protocol.rs`, `src/ipc.rs`,
`web/src/api/ws-frames.ts`, and `tests/fixtures/ws_frames.json`. Prefer the
packaged CLI's JSON/cursor/export commands over implementing private IPC.

## Version and storage decisions

These decisions constrain how upcoming capabilities may ship. Nothing
here extends what 0.5.x already supports, and the per-axis versions stay
what the sections above state.

- **One version per axis.** External CLI JSON, the private IPC envelope
  (`Protocol **14**`), the browser WS frame layout, SSE view notifications,
  the federation handshake, and the journal/SQLite formats are separate
  contracts. A change on one axis never implies compatibility on another.
- **New semantic records use a new journal format version.** When the
  event work lands it extends the record format explicitly and writes it
  through the existing sequencer. New readers keep reading old recordings;
  old readers reject an unknown future version instead of presenting empty
  history; existing journals are never rewritten in place. One journal
  stays the only recording.
- **Exclusive control is opt-in.** Shared control and observe mode remain
  the default. Per-session exclusive mode must be requested explicitly;
  a client that does not understand it is refused or restricted to a true
  read-only path, never silently downgraded to an unrestricted controller.
- **Machine interfaces negotiate before they act.** The planned public
  supervisor interface answers a version/capability query first, and
  unsupported operations fail with an explicit error instead of emulating
  success. Planned capabilities: version/mode query, list/info, output
  cursors, semantic-event cursors and pending-item snapshots, control
  request/release, input receipts, and correlated result messages. Only
  the query itself is a stable promise once shipped.
- **Federation stays experimental** until identity life cycle (including
  revocation), upgrade drills, and the storage decisions above are
  exercised.

Response rules for the unsupported and malformed cases are part of the
contract:

| Situation | Required response |
|---|---|
| Unknown future journal version | explicit unsupported-version error; never empty history |
| Peer lacks a requested capability (planned) | explicit refusal naming the capability |
| Unknown optional field in CLI JSON or private IPC JSON | ignored (additions stay forward-compatible) |
| Unknown field in a federation envelope | rejected on field-bearing variants; unit `joined`, `ping`, and `pong` tolerate extras (serde limit); nested RPC JSON follows the IPC rules — the handshake version is the gate |
| Unknown IPC type tag or missing required field | protocol error |
| Wrong-session, stale-incarnation, or past-end cursor | explicit cursor error naming the condition |
| Delivered request rejected by the daemon | request-failure error; never reported as daemon-unavailable |

## Before stable 0.5

Freeze the CLI/JSON/config contract deliberately; validate backup/upgrade/rollback
on actual platform artifacts. Add a primary-side node identity removal command so
revocation no longer depends on rotating an identity or blocking transport. If
bounded live history is added, define an explicit retained-start cursor and
history-expiration behavior before deleting recorded prefixes. Do not add parallel
recording/parser stacks just to retain old accidental behavior.
