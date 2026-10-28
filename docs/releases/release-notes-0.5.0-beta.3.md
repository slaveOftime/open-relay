# oly 0.5.0-beta.3

**Published beta. Stable remains 0.3.3. Back up state before upgrading.**

[GitHub release](https://github.com/slaveOftime/open-relay/releases/tag/v0.5.0-beta.3)
· [What's new in 0.5](./whats-new-0.5.md)
· [Upgrade from 0.3](./migration-0.5.md)
· [Compatibility](./compatibility-0.5.md)

## Why upgrade

Changes for anyone already on the 0.5 beta line:

- **Peer management is unified under `oly node`.** `oly join` and `oly node
  accept` are gone, with no aliases, and SQLite `nodes` is the single trust
  store. This is the redesign described in
  [Migration to 0.5](./migration-0.5.md#current-node-redesign).
- **The CLI/daemon protocol is now 15** (was 14). Upgrade the CLI, the daemon,
  and every federation node together; a live session cannot move to the new
  daemon.
- Failure is reported honestly: a relayed attach stream that is interrupted
  returns an explicit error instead of a fabricated clean end, and a broken
  attention scan is logged and skipped rather than looking like "nothing needs
  attention".
- Daemon-lifetime loops are supervised — bounded restarts, then `failed` with a
  reason. `/api/ready` and the DaemonStatus RPC report readiness; `/api/health`
  stays liveness-only. `oly daemon status [--node NAME] [--json]` summarises
  version, readiness and its reasons, component state, disk capacity, and
  affected sessions from metadata only.
- Capacity is observable before it is fatal: bounded background sampling of the
  state volume reports free bytes and a journal/uploads/SQLite/logs breakdown,
  with an always-on warning threshold. The admission guard is opt-in, refuses new
  sessions below the floor or on unknown/stale samples, and never kills existing
  ones. Five flat `disk_*` config keys carry defaults, validation, and hot reload.
- Static browser assets are cached correctly: content-hashed bundle files are
  immutable for a year, everything else revalidates (`no-cache`), and every
  static response carries an ETag answered with `304`, so an in-place upgrade is
  not shadowed by a stale shell or chunk.
- Web UI: session groups are sticky and collapsible and the collapse is
  remembered per grouping; the session list remembers its scroll position and
  collapsed groups across reloads; the attach panel collapses to reclaim terminal
  width; the terminal theme follows an explicit `data-theme` override and live OS
  flips; and the attach size is reconciled when the socket opens so the PTY is not
  left at a stale handshake viewport.
- Stalled secondary joins recover, and Linux default IPC is scoped — finish work
  and stop the old daemon with the old binary before replacement, or set
  `OLY_SOCKET_NAME` explicitly.

Upgrading from 0.3 is unchanged. Read [Migration to 0.5](./migration-0.5.md)
before switching existing state.

## Breaking changes

Beta.3 carries two breaking changes relative to beta.2:

- **Protocol 15.** The CLI/daemon envelope moved from 14 to 15 with the node
  redesign. Upgrade the CLI, the daemon, and every federation node together; a
  live session cannot transfer to the new daemon.
- **Unified federation management.** `oly join` and `oly node accept` are removed
  with no aliases, and SQLite migration `0012_unify_nodes` drops the
  `node_identities` table without copying it. Re-provision both peers with
  verified keys after upgrading. See
  [current node redesign](./migration-0.5.md#current-node-redesign).

Other axes are unchanged from beta.2:

- Journal records, segments, and manifests did not change, so `0.5.0-beta.2`
  recordings stay readable (as beta state permits; no frozen on-disk format).
- `SessionSummary` gained only the optional `program_status` field; consumers
  must ignore unknown fields and handle optional values.

The 0.3 → 0.5 breaking changes and required actions are unchanged; see
[Migration to 0.5](./migration-0.5.md) for the actions, not just highlights.

## Install

```sh
npm install -g @slaveoftime/oly@beta
oly --version
```

Or download the exact platform archive and verify its checksum.
Supported bundled binaries: Linux x64, Linux arm64, macOS Apple Silicon,
Windows x64.

This beta does not update crates.io or the stable Homebrew formula. The formula
deliberately covers macOS Apple Silicon and Linux x64 only.

## Known limits

- Daemon stop/crash or reboot ends live processes; recordings are not PTY recovery.
- Prompt/idle signals and successful input delivery are not proof of completion.
- Program status is app-reported state, not process status and not approval, and
  a program that never emits OSC 7501 reports none.
- Controllers share input and geometry; coordinate with other users.
- No live recording quota is implemented; use OS quotas and independent backups.
  The admission guard is not a quota and does not cap an existing session.
- The disk sample is bounded and cached; unknown and stale stay explicit states,
  not a zero gauge.
- Federation has no forward secrecy and no supported mixed-version rollout, and
  the beta.3 migration drops legacy authorizations until peers are re-provisioned.
- Custom apps share the control-plane origin; they are not sandboxed.

See [Compatibility for 0.5](./compatibility-0.5.md) for the beta contract.

## Verification

- Release commit: tag `v0.5.0-beta.3` on release-prep commit `099a4d0`.
- Release workflow
  ([run 39105294538](https://github.com/slaveOftime/open-relay/actions/runs/39105294538)):
  all four `build-release` jobs passed and `publish-npm` published.
  `publish-crates` and `update-homebrew-formula` were skipped, as a beta tag
  requires.
- npm: `beta` moved to `0.5.0-beta.3` after registry propagation; `latest` stayed
  `0.3.3`.
- Archives: `SHA256SUMS.txt` listed all four platform assets. Full archive
  checksum re-verification was not completed for this beta.

### Manual checks — maintainer-reported

None recorded for this beta. Physical-device checks (real Windows sleep/resume,
mobile and browser access) are the maintainer's report, not a CI result, and are
not claimed here.
