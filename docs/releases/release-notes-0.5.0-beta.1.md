# oly 0.5.0-beta.1

**Published beta. Stable remains 0.3.3. Back up state before upgrading.**

[GitHub release](https://github.com/slaveOftime/open-relay/releases/tag/v0.5.0-beta.1)
· [What's new in 0.5](./whats-new-0.5.md)
· [Upgrade from 0.3](./migration-0.5.md)
· [Compatibility](./compatibility-0.5.md)

## Why try it

Journal-backed recording, inspectable live/final screens, cursor-based output,
full session metadata, and shared terminal/browser control make interactive
CLIs easier to supervise. The dashboard, paste/mouse/scrollback handling, and
Windows output draining are improved. Connected-machine federation uses pinned
identities and encrypted traffic; its trust model and limits are in the
[compatibility policy](./compatibility-0.5.md#node-federation).

## Breaking changes

- Old recordings are not imported; database migration requires a backup for rollback.
- Upgrade CLI/daemon and federation nodes together; recreate identity-pinned joins.
- Update removed log flags and recording-cap config keys.
- Invalid config fails startup. Custom apps are opt-in; proxies are loopback-only.

See [Migration to 0.5](./migration-0.5.md) for the required actions, not just highlights.

## Install

```sh
npm install -g @slaveoftime/oly@beta
oly --version
```

Or download the exact platform archive and verify its checksum.
Supported bundled binaries: Linux x64, macOS Apple Silicon, Windows x64.
This beta does not update crates.io or the stable Homebrew formula.

## Known limits

- Daemon stop/crash or reboot ends live processes; recordings are not PTY recovery.
- Prompt/idle signals and successful input delivery are not proof of completion.
- Controllers share input and geometry; coordinate with other users.
- No live recording quota is implemented; use OS quotas and independent backups.
- Federation has no forward secrecy and no supported mixed-version rollout.
- Custom apps share the control-plane origin; they are not sandboxed.

See [Compatibility for 0.5](./compatibility-0.5.md) for the beta contract.

## Verification

Published from merge commit `92969c2ea6a4610ae0e46d722e4033bb45ad4ee7`.

### Automated and artifact checks — passed

- [PR-head CI](https://github.com/slaveOftime/open-relay/actions/runs/38401827899)
  and [frontend CI](https://github.com/slaveOftime/open-relay/actions/runs/38401828240):
  Windows/macOS/Linux tests and release builds, Clippy, frontend lint/unit/build/
  Chromium passed. Dependency audit was skipped on the PR, then passed in
  [post-merge CI](https://github.com/slaveOftime/open-relay/actions/runs/38404202511).
- [Release workflow](https://github.com/slaveOftime/open-relay/actions/runs/38404258644):
  all platform tests/builds, uploads, checksums, and npm publication passed.
- Downloaded archives match `SHA256SUMS.txt`; binary entries and executable
  metadata were verified. Linux/Windows archive binaries report `0.5.0-beta.1`.
- An isolated published npm installation reports `0.5.0-beta.1`; all three
  bundled binaries match the GitHub archives byte-for-byte.
- npm `beta` points to `0.5.0-beta.1`; npm/GitHub latest remain `0.3.3`.
  crates.io publication and Homebrew updates were intentionally skipped.

### Manual checks — maintainer-reported passes

The maintainer confirmed these steps were tested manually and work:

- Finish live work, back up existing 0.3 state, upgrade, and verify the version.
- Windows/macOS npm installation and embedded browser UI.
- Session launch, input, detach/reattach, stop, and restart.
- Real Windows sleep/resume and browser/mobile access on actual devices.

This is the maintainer's report, not a CI or agent-observed result. Device/OS
inventory and soak duration were not supplied. It does not establish exhaustive
rollback, federation reconnect, multi-controller race, slow-client cursor, or
disk-pressure coverage. See [maintainer release gates](../maintainers/release-process.md#2-prepare-and-merge-a-pr)
for preparing the next release.
