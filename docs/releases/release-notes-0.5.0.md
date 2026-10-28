# oly 0.5.0

**The first stable 0.5 release.** Close your laptop; the daemon keeps your
sessions running. Come back from a terminal, a browser, or your phone.

[GitHub release](https://github.com/slaveOftime/open-relay/releases/tag/v0.5.0)
· [What's new in 0.5](./whats-new-0.5.md)
· [Upgrade from 0.3](./migration-0.5.md)
· [Compatibility](./compatibility-0.5.md)

## Highlights

- **Your work outlives your terminal.** The daemon owns the PTY, records output,
  and keeps the child running when a client disconnects.
- **One place to see what needs you.** The terminal dashboard and the
  password-protected web UI show lifecycle state and agent-reported program
  status (OSC 7501) across local and connected machines.
- **Check in without taking over.** Read rendered output, inspect a live screen,
  wait for a likely prompt, then send text, keys, or mouse input — or attach.
- **Scriptable supervision.** `oly info --json`, bounded byte pages, and
  resumable cursors let agents and scripts follow sessions safely.
- **Built to be operated.** Readiness and health endpoints, disk-capacity
  visibility, honest failure reporting, and four shipped platforms.
- **Works on the devices you carry.** The PWA fits above the Android soft
  keyboard, keeps the cursor visible, and avoids duplicated IME input.

## What's new since 0.3

0.5 is a substantial redesign: journal-backed recording, one authoritative
terminal model, the unified `oly node` federation CLI, and a session list that
stays responsive with thousands of sessions. See
[What's new in 0.5](./whats-new-0.5.md) for the tour and
[Compatibility for 0.5](./compatibility-0.5.md) for the supported contracts.

## Upgrade from 0.3 — read this first

0.5 is a breaking upgrade. Finish live work, stop the daemon, back up the whole
state directory, and keep the old binary.

- **Recordings are not imported.** Keep the old binary if you still need 0.3 logs.
- **Protocol 15.** Upgrade the CLI, the daemon, and every federation node together.
- **Node management is unified.** `oly join` and `oly node accept` are removed,
  and the SQLite migration drops legacy authorizations. Re-provision both peers
  with verified keys.
- **Log flags and recording-cap config keys changed.** Update scripts and config;
  removed keys now fail startup explicitly.
- **Linux default IPC is scoped.** Stop the old daemon with the old binary before
  replacing it, or set `OLY_SOCKET_NAME` explicitly.
- **Custom apps and proxies stay opt-in.** Review code before enabling them.

Follow [Migration to 0.5](./migration-0.5.md) for the exact actions, not just
the highlights.

## Install

```sh
npm install -g @slaveoftime/oly
oly --version
```

**Homebrew** (macOS Apple Silicon, Linux x64):

```sh
brew tap slaveOftime/open-relay https://github.com/slaveOftime/open-relay
brew install slaveOftime/open-relay/oly
```

**Cargo:**

```sh
cargo install oly
```

**Prebuilt archives:** Linux x64, **Linux arm64**, macOS Apple Silicon, and
Windows x64, with SHA-256 checksums on the GitHub release.

## Known limits

- Daemon stop/crash or reboot ends live processes; recordings are not PTY recovery.
- Prompt/idle signals and successful input delivery are not proof of completion.
- Program status is app-reported state, not process status and not approval.
- Controllers share input and geometry; coordinate with other users.
- No live recording quota is implemented; use OS quotas and independent backups.
  The admission guard is not a quota and never kills an existing session.
- Federation has no forward secrecy and no supported mixed-version rollout.
- Custom apps share the control-plane origin; they are not sandboxed.

See [Compatibility for 0.5](./compatibility-0.5.md) for the 0.5 contract.

## Verification

This section records what was observed for this release and is updated once the
tag's Release workflow finishes.

- Release commit: tag `v0.5.0` on the release-prep commit on `main`.
- Pre-tag documents contract: `python3 tests/docs_sync.py` passed (9/9) on the
  release-prep tree. `tests/release_workflow.py` requires a POSIX host and runs
  in `CI-DOCS`.
- Release workflow: pending the tag push. A stable tag builds and tests all four
  platforms, publishes npm `latest`, publishes the crate, and commits the
  Homebrew version and checksums.
- npm, crates.io, Homebrew, artifact checksum, and installed-package checks:
  pending publication.

### Manual checks — maintainer-reported

None recorded. Physical-device checks (real Windows sleep/resume, mobile and
browser access) are the maintainer's report, not a CI result, and are not
claimed here.
