# oly — Open Relay

<p align="center">
  <img src="https://raw.githubusercontent.com/slaveOftime/open-relay/main/assets/icon-demo.svg" width="120" alt="oly logo" />
</p>

<p align="center"><strong>Your agents keep running. You stay in control.</strong></p>

[![npm version](https://img.shields.io/npm/v/@slaveoftime/oly.svg)](https://www.npmjs.com/package/@slaveoftime/oly)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/slaveOftime/open-relay/blob/main/LICENSE)

Run an interactive CLI once. Close your terminal. Come back from another
terminal or a browser — with the process still running and its output recorded.

`oly` is a local-first session supervisor for coding agents, REPLs, and
long-running commands that still need a human in the loop. It works with the
CLI you already use; no model-provider integration required.

https://github.com/user-attachments/assets/bd52a474-d9c4-48a7-824b-8df328a9d5a7

## Why oly?

- **Walk away without losing the job.** The daemon owns the PTY, not your terminal.
- **Check in, don't babysit.** Read recent output, inspect a live TUI, or wait for a likely input-needed checkpoint.
- **Intervene without attaching.** Send text, keys, or application mouse input; take over interactively when needed.
- **Pick up from anywhere.** Use the terminal or browser, and route work to connected machines.
- **Give agents a reliable control surface.** JSON metadata, bounded byte pages, and resumable cursors support scripted supervision.

Think **persistent terminal sessions + a supervision API**, not another agent
framework. Use your shell for quick commands; use oly when a job needs a durable home.

## Install

```sh
npm install -g @slaveoftime/oly
```

The npm package bundles binaries for **Linux x64, macOS Apple Silicon, and
Windows x64**. No GitHub asset download is needed during installation.

> **0.5.0-beta.1 is available:** a breaking preview, not the stable channel.
> Stable installs still use the 0.3 line. Try `npm install -g @slaveoftime/oly@beta`
> or the [GitHub prerelease](https://github.com/slaveOftime/open-relay/releases/tag/v0.5.0-beta.1).
> Back up state and read the [migration guide](./docs/releases/migration-0.5.md) before upgrading.
> See the [release notes and verification](./docs/releases/release-notes-0.5.0-beta.1.md).

<details>
<summary>Other installation options</summary>

**Homebrew** (stable):

```sh
brew tap slaveOftime/open-relay https://github.com/slaveOftime/open-relay
brew install slaveOftime/open-relay/oly
```

**Prebuilt binaries:** download a platform archive from
[GitHub Releases](https://github.com/slaveOftime/open-relay/releases).

**Cargo** (stable):

```sh
cargo install oly
```

Building from source requires Rust and Node.js/npm: release builds compile and
embed the web UI. See [Contributing](./CONTRIBUTING.md) for development setup.

</details>

## From launch to handoff

```sh
# Start the daemon (skip this if it is already running).
# Default: password-protected web UI at http://127.0.0.1:15443.
oly daemon start --detach

# Run your preferred CLI in the background; save the returned session ID.
oly start --detach --title "fix failing tests" copilot

# See sessions and inspect recent output.
oly ls
oly logs <id> --tail 40

# Inspect a TUI and, after reviewing its prompt, reply without attaching.
oly logs <id> --screen
oly send <id> "continue" key:enter

# Take over interactively. Ctrl-D detaches; the child keeps running.
oly attach <id>
```

Replace `copilot` with `claude`, `codex`, another agent, or any interactive
command. Pass oly options **before** the child command.

Open **http://127.0.0.1:15443** for the browser UI, or use
`oly ls --follow` for the live terminal dashboard.

### A few commands worth remembering

| Want to… | Command |
|---|---|
| Wait for a likely checkpoint | `oly logs <id> --wait prompt --timeout 5m` |
| Inspect metadata and exit status | `oly info <id>` / `oly info <id> --json` |
| Watch without sending input or resizing | `oly attach <id> --observe` |
| Interrupt the child | `oly send <id> key:ctrl+c` |
| Export recorded display bytes | `oly export <id> > capture.bin` |
| Stop a session | `oly stop <id>` |
| Start a fresh run from saved launch metadata | `oly restart <id>` |
| Teach an agent how to supervise sessions | `oly skill` / `oly skill --subagent` |

## Know the boundaries

- **Persistent sessions, not immortal processes.** Closing a client does not stop
  the child. Stopping/crashing the daemon or rebooting the host does; retained
  recordings remain, but live PTYs cannot be recovered. `restart` launches a new run.
- **Hints, not task understanding.** Prompt and idle detection can be wrong.
  Check fresh output and the actual result; successful input delivery is not completion.
- **Shared control, not exclusive ownership.** Multiple attached controllers can
  type concurrently; the last successful resize wins. Coordinate with other users.
- **Local-first, not a sandbox.** Child commands run with the daemon user's
  privileges. Keep HTTP on loopback or put remote access behind a trusted tunnel
  and authentication gateway. View-only mode is not a security boundary.

## Go deeper

- [CLI guide](./docs/guides/cli.md) — logs, cursors, input, scrolling, and session lifecycle
- [Remote access](./docs/guides/remote-access.md) — browser security, connected nodes, and notifications
- [Agent skills](./skills/oly/SKILL.md) — general usage; [worker supervision](./skills/oly-subagent/SKILL.md), [daemon configuration](./skills/oly-daemon/SKILL.md), [web apps](./skills/oly-apps/SKILL.md)
- [Upgrade to 0.5](./docs/releases/migration-0.5.md) — breaking changes and safe upgrades
- [Architecture](./ARCHITECTURE.md) · [Performance](./PERFORMANCE.md) · [Contributing](./CONTRIBUTING.md)
- [What's new in 0.5](./docs/releases/whats-new-0.5.md) · [Compatibility](./docs/releases/compatibility-0.5.md)
- [All documentation](./docs/README.md) · [How to publish a release](./docs/maintainers/release-process.md)

MIT licensed. If oly makes your workflow easier, a
[star on GitHub](https://github.com/slaveOftime/open-relay) helps others find it.
