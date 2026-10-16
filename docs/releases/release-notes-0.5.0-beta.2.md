# oly 0.5.0-beta.2

**Published beta. Stable remains 0.3.3. Back up state before upgrading.**

[GitHub release](https://github.com/slaveOftime/open-relay/releases/tag/v0.5.0-beta.2)
· [What's new in 0.5](./whats-new-0.5.md)
· [Upgrade from 0.3](./migration-0.5.md)
· [Compatibility](./compatibility-0.5.md)

## Why upgrade

Changes for anyone already on the 0.5 beta line:

- The session list stays responsive with thousands of sessions instead of
  degrading as history grows. Measurement recipes are in
  [Performance](../../PERFORMANCE.md).
- Agent-reported program status (OSC 7501) is readable: `oly info` and its JSON
  form, `oly ls --json`, `[state]` labels in `oly ls --follow`, and status
  badges in the web UI. Windows `pi` sessions work even though ConPTY can answer
  the device-attributes query before oly's own reply.
- **Linux arm64 is now a shipped target.** A native `ubuntu-24.04-arm` runner
  builds the archive and the npm bundle, so the shipped binary is the one the
  test suite exercised; there is no cross toolchain to trust.
- Resume hints are captured more reliably when an agent takes longer to exit
  during a graceful stop.
- A session no longer fails when journal writes fall behind: the ingest path
  applies bounded backpressure instead of ending the session.
- `rustls` moved to 0.23.46 for four RUSTSEC advisories published 2026-10-14.
- Dashboard and TUI polish: attach on start by default, a search menu that stays
  on screen, relative session ages, a scrollable session header on mobile, and
  program-status labels.

Upgrading from 0.3 is unchanged. Read [Migration to 0.5](./migration-0.5.md)
before switching existing state.

## Breaking changes

None new in beta.2.

- The CLI/daemon envelope is still Protocol **14**.
- The SQLite migrations are unchanged, and no journal record, segment, or
  manifest format changed — so `0.5.0-beta.1` state stays readable in place.
- `SessionSummary` gained one **optional** `program_status` field. Documented
  consumers must ignore unknown fields and handle optional values, so this is
  additive.

The 0.3 → 0.5 breaking changes and required actions are unchanged; see
[Migration to 0.5](./migration-0.5.md) for the actions, not just highlights.

## Install

```sh
npm install -g @slaveoftime/oly@beta
oly --version
```

Or download the exact platform archive and verify its checksum.
Supported bundled binaries: Linux x64, **Linux arm64**, macOS Apple Silicon,
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
- Federation has no forward secrecy and no supported mixed-version rollout.
- Custom apps share the control-plane origin; they are not sandboxed.
- The session-list work is measured, not a published performance contract.

See [Compatibility for 0.5](./compatibility-0.5.md) for the beta contract.

## Verification

- Release commit: tag `v0.5.0-beta.2` on merge commit
  `96ae02d083fba69923129de2bdb8830a0db2e9fe`.
- PR-head CI for the release-prep PR: `docs contracts` passed in 9s
  ([run 38890315468](https://github.com/slaveOftime/open-relay/actions/runs/38890315468)).
  The change is documentation-only, so that is the only status the diff can
  affect; the Rust matrix and the web bundle were not triggered, by design
  rather than by omission.
- Post-merge `cargo audit` on `main`: the release commit changes no source, so
  push CI did not run for it. The last source-changing push (`9742a90`, the
  session-list performance merge) had `cargo audit` green.
- Release workflow
  ([run 38891124722](https://github.com/slaveOftime/open-relay/actions/runs/38891124722)):
  all four `build-release` jobs passed — locked tests on Linux x64, Linux arm64,
  macOS Apple Silicon, and Windows x64 (serial) — and `publish-npm` published.
  `publish-crates` and `update-homebrew-formula` were skipped, as a beta tag
  requires.
- npm: `beta` is `0.5.0-beta.2` and `latest` stays `0.3.3`, read from the
  registry document directly rather than from a client cache.
- Artifact checks: in progress.

### Manual checks — maintainer-reported

None recorded for this beta. Physical-device checks (real Windows sleep/resume,
mobile and browser access) are the maintainer's report, not a CI result, and are
not claimed here.
