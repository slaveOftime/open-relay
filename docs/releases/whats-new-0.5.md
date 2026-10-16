# What's new in 0.5.x

0.5 is currently a beta for early adopters. This page covers features and
improvements, **not upgrade instructions**. Read [Migration to 0.5](./migration-0.5.md)
before switching existing state.

## Recorded sessions and inspectable output

- One journal captures PTY output, resizes, lifecycle, and checkpoints.
  One authoritative backend terminal model drives live and replayed views.
- Read rendered tails, inspect the live/final screen, or request sampled frames.
- Cursor-based byte pages support incremental observation across calls;
  `export` captures a fixed prefix of filtered display bytes.
- Attach completion and final cursors wait for PTY EOF, including Windows
  output buffered after child exit.

See [Logs and continuation](../guides/cli.md#logs-views-waits-and-byte-continuation).

## Easier supervision

- `oly info` exposes session, process, activity, terminal, and storage metadata,
  with a JSON form for scripts.
- The live dashboard adds working-directory trees, scoped search, metadata
  editing, and cloning.
- Prompt, idle, output, exit, and pattern waits help decide when to inspect work.
  These are signals, not proof of task completion or approval.
- Agent-reported program status (OSC 7501) is exposed as `program_status` in
  `oly info`/`oly ls`, as `[state]` labels in `oly ls --follow`, and as badges
  in the web UI. It is app-reported state, not process status or approval.
- The session list stays responsive with thousands of sessions. Measurement
  recipes are in [Performance](../../PERFORMANCE.md).
- Bundled skills cover session supervision, workers, daemon configuration,
  and trusted apps.

See [CLI guide](../guides/cli.md) and [Agent skills](../../skills/oly/SKILL.md).

## Shared terminal and browser control

- Concurrent controllers share a session; observers never send input or resize.
- Improved paste boundaries, mouse input, scrollback, and mobile controls.
- Windows named-pipe and console teardown fixes improve disconnect and final
  output handling.
- Password login, origin checks, token revocation, and opt-in trusted apps
  constrain browser access.

Shared control is not exclusive ownership or multi-user authorization.
See [Attach](../guides/cli.md#attach-and-scroll) and [Browser access](../guides/remote-access.md#browser-access).

## Connected machines

Pinned Ed25519 identities, encrypted readiness, and replay-checked application
traffic replace the old federation credential paths. Verify public identities
independently and upgrade nodes together. Federation has **no forward secrecy**
and, in 0.5.x, no primary-side identity removal.

## Platforms

Bundled binaries cover Linux x64, Linux arm64, macOS Apple Silicon, and
Windows x64; the npm launcher selects one. Linux arm64 joined the matrix in
0.5.0-beta.2 and is built on a native arm64 runner.

See [Node setup](../guides/remote-access.md#connect-machines),
[Compatibility](./compatibility-0.5.md), and
[0.5.0-beta.2 release notes](./release-notes-0.5.0-beta.2.md) for shipped scope
and verification.
