# oly 0.5.0-beta.2

**Prepared for publication, not yet published. Stable remains 0.3.3. Back up state before upgrading.**

[What's new in 0.5](./whats-new-0.5.md)
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

**Not yet published.** The fields below are completed at publication, and are
the same fields [0.5.0-beta.1](./release-notes-0.5.0-beta.1.md) used to record
its release.

- Release commit, tagged on `main`: pending.
- PR-head CI — Rust tests on Linux x64, Linux arm64, macOS Apple Silicon, and
  Windows x64, `fmt`/Clippy, frontend lint/unit/build/Chromium, and the offline
  docs contracts: pending.
- Post-merge `cargo audit`: pending. The audit job runs on `main` pushes, so PR
  runs skip it.
- Release workflow — platform tests and builds, uploads, checksums, and npm
  `beta` publication: pending.
- Artifact checks — downloaded archives match `SHA256SUMS.txt`, the published
  npm installation reports `0.5.0-beta.2`, and `beta` moved while `latest`
  stayed `0.3.3`: pending.

### Manual checks — maintainer-reported

To be recorded after publication. Physical-device checks (real Windows
sleep/resume, mobile and browser access) are the maintainer's report, not a CI
result; this beta has no such report yet.
