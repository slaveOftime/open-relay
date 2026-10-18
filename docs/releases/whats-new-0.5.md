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
- `oly daemon status [--node NAME] [--json]` summarises version, readiness and
  its reasons, component state, disk capacity, and affected sessions from
  metadata only. `/api/ready` reports readiness; `/api/health` stays
  liveness-only, and supervised loops report `failed` with a reason instead of
  silently stopping.
- Disk capacity is sampled in the background and reported explicitly: free
  bytes plus a journal/uploads/SQLite/logs breakdown, an always-on warning
  threshold, and an opt-in admission guard that refuses new sessions below the
  floor. It is not a quota and never kills an existing session.
- Bundled skills cover session supervision, workers, daemon configuration,
  and trusted apps.

See [CLI guide](../guides/cli.md) and [Agent skills](../../skills/oly/SKILL.md).

## Shared terminal and browser control

- Concurrent controllers share a session; observers never send input or resize.
- Improved paste boundaries, mouse input, scrollback, and mobile controls.
- Windows named-pipe and console teardown fixes improve disconnect and final
  output handling.
- Session groups are sticky and collapsible, and the list remembers its scroll
  position and collapsed groups across reloads.
- The attach panel collapses to reclaim terminal width; the terminal theme
  follows a live OS flip or an explicit `data-theme` override; and the attach
  size is reconciled when the socket opens instead of holding a stale handshake
  viewport.
- Content-hashed bundle files are cached immutably while the shell and other
  files revalidate with `no-cache` and an ETag/`304`, so an in-place upgrade is
  not shadowed by a stale bundle.
- Password login, origin checks, token revocation, and opt-in trusted apps
  constrain browser access.

Shared control is not exclusive ownership or multi-user authorization.
See [Attach](../guides/cli.md#attach-and-scroll) and [Browser access](../guides/remote-access.md#browser-access).

## Connected machines

Pinned Ed25519 identities, encrypted readiness, and replay-checked application
traffic replace the old federation credential paths. Verify public identities
independently and upgrade nodes together. Federation has **no forward secrecy**;
later seed compromise can expose recorded federation traffic.

### Current node redesign (first shipped in 0.5.0-beta.3)

- Unified `oly node add/ls/info/update/enable/disable/rm` manages both roles.
  Names identify the remote peer; outbound primary entries use `--as` for the
  local claimed name authorized on the remote.
- SQLite `nodes` stores inbound authorization and outbound pins together.
  Disable survives restart; remove revokes local trust without killing sessions.
- List/info expose connected, connecting, retrying, waiting, and disabled state,
  plus last errors and next retry. Mutations report persisted/applied state
  immediately; success is not a completed connection.
- The cryptographic handshake and HTTP live-secondary route inventory stay
  unchanged. Session `--node` still targets secondaries only.

This replaces `oly join` and `oly node accept` without aliases. See
[Migration](./migration-0.5.md#current-node-redesign) for script/state changes.
It first shipped in **0.5.0-beta.3**; earlier betas keep the old commands, and
exact-version release notes remain the record of each shipped scope.

## Platforms

Bundled binaries cover Linux x64, Linux arm64, macOS Apple Silicon, and
Windows x64; the npm launcher selects one. Linux arm64 joined the matrix in
0.5.0-beta.2 and is built on a native arm64 runner.

See [Node setup](../guides/remote-access.md#connect-machines),
[Compatibility](./compatibility-0.5.md), and
[0.5.0-beta.3 release notes](./release-notes-0.5.0-beta.3.md) for shipped scope
and verification.
