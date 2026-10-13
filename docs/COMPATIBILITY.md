# Compatibility policy for the 0.5 beta

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

## State and recordings

0.3 recordings are not automatically imported. SQLite migration is forward-only;
rollback requires restoring a pre-upgrade backup. Never use an old binary on a
migrated database. Private state includes identity keys, push keys, and session data.

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
- **Federation:** deploy compatible CLI/daemon versions on every node and upgrade
  together. Pinned Ed25519 identities and encrypted v2 readiness are required.
  Static-static DH has **no forward secrecy**. Handshake changes and operational
  identity revocation remain pre-stable work; no mixed-version rollout is promised.
- **Custom web apps/proxies:** opt-in reviewed operator code on the control-plane
  origin (`trusted_apps_enabled`), not untrusted agent output. Only loopback literal
  proxies are supported. Disabling blocks new requests but does not retroactively
  close open documents or connections. Review services before exposing them.
- **Attention hints and resume extraction:** prompt/silence heuristics and configured
  child-output suggestions, not authorization, semantic task state, or automatic execution.

## Before stable 0.5

Freeze the CLI/JSON/config contract deliberately; validate backup/upgrade/rollback
on actual platform artifacts. Decide federation key exchange and identity
revocation before declaring stable interoperability. If bounded live history is
added, define an explicit retained-start cursor and history-expiration behavior
before deleting recorded prefixes. Do not add parallel recording/parser stacks
just to retain old accidental behavior.
