# oly — Open Relay

<p align="center">
  <img src="./assets/icon-demo.svg" width="160" alt="oly logo" />
</p>

[![npm version](https://img.shields.io/npm/v/@slaveoftime/oly.svg)](https://www.npmjs.com/package/@slaveoftime/oly)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)

https://github.com/user-attachments/assets/bd52a474-d9c4-48a7-824b-8df328a9d5a7

> Run interactive CLIs and AI agents like managed services.

`oly` gives long-running terminal jobs a durable home.

> **Preview branch:** this branch (`v0.5.x`) carries the 0.5.x beta line —
> a ground-up rework of storage, terminal fidelity, and streaming with
> breaking changes. The stable release line is 0.3.x on `main`. Beta
> artifacts ship as GitHub *prereleases* and under the npm `beta`
> dist-tag. See [MIGRATION.md](./MIGRATION.md) before switching.

Start a command once, detach, close your terminal, come back later, inspect logs, send input only when needed, or reattach and take over. It is built for AI agent workflows, interactive CLIs, and any session you do not want tied to one fragile terminal window.

If `oly` saves you time, please star the repo. That helps more people discover it.

Upgrading from 0.3.x? Read [MIGRATION.md](./MIGRATION.md) first — 0.5.0 is a clean break. For internals, see [ARCHITECTURE.md](./ARCHITECTURE.md).

---

## Why people use `oly`

- **Detach without losing the process.** The daemon owns the session, not your terminal.
- **Stop babysitting prompts.** Watch logs or wait for likely input-needed checkpoints.
- **Intervene surgically.** Send text or keys without attaching.
- **Resume with context.** Reattach and replay buffered output first.
- **Keep an audit trail.** Session output and lifecycle events persist on disk.
- **Control it from more than one place.** Use the CLI, the web UI, or route work to connected nodes.

`oly` is not trying to replace your favorite terminal. It is a supervision layer for long-lived, interactive workloads. A simple CLI proxy.

---

## Install

### npm

```sh
npm i -g @slaveoftime/oly
```

The published npm package bundles the supported platform binaries directly, so users do not need to download a GitHub release asset during `npm install`.

### Cargo

```sh
cargo install oly
```

### Homebrew

```sh
brew tap slaveOftime/open-relay https://github.com/slaveOftime/open-relay
brew install slaveOftime/open-relay/oly
```

### Prebuilt binaries

Download the latest release from the [Releases page](https://github.com/slaveOftime/open-relay/releases).

Current release artifacts are published for:

- **macOS**: Apple Silicon (`arm64`)
- **Linux**: `x86_64` / AMD64
- **Windows**: `x86_64` / AMD64

---

## Quick start

If you only try one workflow, make it this one:

```sh
# Start the daemon.
# By default, the local web UI/API is enabled and protected by a password
# you set at startup.
oly daemon start --detach

# Launch a detached session
oly start copilot

# See what's running
oly ls

# Check recent output and optionally wait for an input-needed checkpoint
oly logs <id> --wait-for-prompt --timeout 1m

# Send input without attaching
oly send <id> "yes" key:enter

# Reattach when you want full control
oly attach <id>

# Stop the session
oly stop <id>
```

Useful behavior to know:

- `oly attach`, `oly logs`, `oly send`, `oly stop`, and `oly notify` accept an optional session ID. If you omit it, `oly` targets the most recently created session.
- To detach from an attached session, press `Ctrl-D`.
- Attaching (CLI or browser) takes control of the session by default and resizes it to the attaching client's viewport — the most recently active client always drives. Use `oly attach --observer` for a view-only attach that never drives input or geometry.
- Resizing an attached terminal window reports the new size to the daemon and resizes the session; if that client was an observer, the resize takes control first (same last-active-wins rule), so the session always follows the window you are actively shaping.
- The control lease is coordination, not a security boundary: it records who is driving and demotes the previous controller, but operator commands like `oly send` reach the session regardless by design — no lease ceremony is needed (or available) for scripted input.

---

## What `oly` does well

### 1. Supervise agent sessions

Run coding agents, REPLs, installers, or approval-heavy workflows in the background without keeping one terminal open forever.

### 2. Detect likely human checkpoints

`oly logs --wait-for-prompt` lets you block until a session likely needs attention, then inspect the output before deciding what to do next.

### 3. Let humans stay in the loop

When a process needs confirmation, credentials, or a decision, you or agent can send input directly:

```sh
oly send <id> "continue" key:enter
oly send <id> key:ctrl+c
oly send <id> key:up key:enter
```

### 4. Keep a browser-accessible control plane

By default, `oly daemon start -d` also serves a local web UI and HTTP API on `http://127.0.0.1:15443`.

- Use `--bind` to change the bind address (for example `--bind 0.0.0.0`).
- Use `--port` to change the port.
- Use `--no-http` for CLI-only operation.
- Use `--no-auth` only if you understand the risk and are protecting access elsewhere.

### 5. Route work to other machines

`oly` can connect multiple daemons together so one primary can supervise sessions on secondary nodes.

---

## Core workflow patterns

### Detached agent run

```sh
oly start --title "fix failing tests" --detach copilot
oly logs --wait-for-prompt
oly send <id> "approve" key:enter
```

### Watch logs without attaching

```sh
oly logs <id> --tail 80
oly logs <id> --tail 80 --keep-color
oly logs <id> --tail 120 --no-truncate
```

### Start on a connected node

```sh
oly start --node worker-1 --title "nightly task" --detach claude
oly logs --node worker-1 --wait-for-prompt <id>
```

---

## Command reference

### Session and daemon commands

| Command | Purpose |
| --- | --- |
| `oly daemon start [--detach] [--bind <addr>] [--port <port>] [--no-auth] [--no-http]` | Start the daemon, optional local web API/UI |
| `oly daemon stop [--grace <seconds>]` | Stop the daemon and let sessions exit cleanly first |
| `oly start [--title <title>] [--detach] [--disable-notifications] [--cwd <dir>] [--node <name>] <cmd> [args...]` | Start a session |
| `oly ls [--search <text>] [--json] [--status <status>]... [--since <rfc3339>] [--until <rfc3339>] [--limit <n>] [--node <name>]... [--node-local]` | List sessions |
| `oly attach [id] [--observer] [--node <name>]` | Reattach to a session (takes control by default) |
| `oly logs [id] [--tail <n>] [--keep-color] [--no-truncate] [--wait-for-prompt] [--timeout <duration>] [--node <name>]` | Read logs without attaching (also `--screen`, `--from`, and wait modes — see below) |
| `oly send [id] [chunk]... [--node <name>]` | Send text or special keys to a session |
| `oly stop [id] [--grace <seconds>] [--node <name>]` | Stop a session |
| `oly restart <id> [--force] [--node <name>]` | Start a new session from persisted launch metadata (`--force` first kills a running source) |
| `oly rm [id] [--force] [--node <name>]` | Delete a stopped session and its logs (`--force` also kills a running session first) |
| `oly notify enable [id] [--node <name>]` | Enable notifications for a session |
| `oly notify disable [id] [--node <name>]` | Disable notifications for a session |
| `oly update <id> ...` | Override a session's title, tags, and notification setting |
| `oly skill` | Print the bundled `oly` skill markdown |
| `oly daemon status` | Show the running daemon's effective flags and HTTP endpoint |

The interactive view can monitor several nodes at once, for example `oly ls --follow --node worker-a --node worker-b`. Add `--node-local` to include sessions from the current daemon (or the primary itself); the table shows a node column when multiple sources are selected. `Ctrl+D` opens a clone editor prefilled from the selected session, while `Ctrl+U` opens an update editor for the selected session's title, tags, and notification setting. `Tab`/`Ctrl+Tab` move between dialog fields, `Space` toggles notifications, and `Enter` submits the current dialog. Use `Ctrl+K` to stop the selected running session, `Enter` to open it inline, `Ctrl+Enter` to open it in another terminal window, and `Ctrl+C` to exit the list view.

Supported `oly send` key forms include named keys like `key:enter`, `key:tab`, `key:esc`, arrows, `home/end`, `pgup/pgdn`, `del/ins`, modifier forms like `key:ctrl+c`, `key:alt+x`, `key:meta+enter`, `key:shift+tab`, and raw bytes via `key:hex:...`.

### Reading session output: `oly logs`

`oly logs` is the single read surface for session output, organized as three
independent axes that combine freely:

1. **What to read** (pick at most one): the rendered log tail (default),
   `--screen` (visible screen as text), `--from <offset>` (raw byte window of
   the canonical stream), or `--raw` (the whole original byte stream).
2. **When to read it** (optional gate): `-w/--wait-for-prompt`, or
   `--after <offset>` with any of `--exit`, `--idle-ms <ms>`,
   `--pattern <regex>`. With nothing to read selected, the gate alone prints
   the condition result and the new cursor; combined with a read, it blocks
   first and reads after the condition is met.
3. **How to format it**: `--tail`, `--keep-color`, `--cols`, `--no-truncate`,
   `--from-file` (rendered modes), `--limit` and `--json` (window reads and
   wait-only results), `--timeout` (gates).

```bash
# Human: rendered log tail (default)
oly logs <ID> --tail 40 --keep-color
oly logs <ID> -w                        # block until the session needs input, then print
oly logs <ID> --raw > dump.bin          # exact child bytes (pipes/files)

# Screen snapshots
oly logs <ID> --screen [--cols 120] [--keep-color]
oly logs <ID> --pattern 'ERROR' --screen   # block until output matches, then show the screen

# Machine: cursor-based window reads
oly logs <ID> --from <offset> --limit 65536 --json   # one window, base64

# Machine: the cursor loop in one call — block until there is output after
# the cursor, then read exactly that window
oly logs <ID> --from <offset> --after <offset> --json

# Machine: gates on their own report the condition and the new cursor
oly logs <ID> --after <offset>                     # any new output
oly logs <ID> --after <offset> --idle-ms 800       # quiet for 800ms (heuristic)
oly logs <ID> --exit --timeout 30s                 # session exited
```

- Wait exit codes: `0` condition met, `2` timeout, `1` error. `--timeout`
  accepts plain milliseconds or `s`/`m`/`h` suffixes; `0` waits forever
  (defaults: 30s for gates, 5m with `--wait-for-prompt`).
- `--idle-ms` means "quiet", never "done" — a silent session may be thinking,
  blocked, or crashed. Confirm with `--screen` or a pattern.
- Window reads are bounded (`--limit`, hard-capped server-side); page with the
  returned `next` offset instead of asking for everything.
- Combinations that would be silently meaningless (e.g. `--screen --tail`,
  `--raw --exit`, `--json` without `--from` or a gate) are usage errors.

### Federation commands

| Command | Purpose |
| --- | --- |
| `oly api-key add <name>` | Create an API key on the primary and print it once |
| `oly api-key ls` | List API key labels on the primary |
| `oly api-key remove <name>` | Revoke an API key on the primary |
| `oly join start --name <name> --key <key> <url>` | Connect this daemon to a primary |
| `oly join stop --name <name>` | Disconnect and remove a saved join config |
| `oly join ls` | List saved outbound join configs on this daemon |
| `oly join ls --primary` | Ask the daemon for currently active primary-side joins |
| `oly node ls` | List secondary nodes currently connected to the primary |

---

## Browser access and remote supervision

`oly` serves its HTTP API and web UI on loopback by default:

```text
http://127.0.0.1:15443
```

That default is deliberate. The safe pattern is:

```text
browser or phone -> your auth gateway -> your tunnel -> local oly HTTP service
```

Examples:

- Cloudflare Access
- Tailscale / Headscale
- SSH tunnel
- your own reverse proxy with strong auth

This keeps `oly` small and local-first while still supporting remote intervention when you need it.

---

## Notification hooks

If desktop notifications are not enough, you can configure a custom notification hook in `config.json` under `OLY_STATE_DIR` (or the default state directory for your OS).

```json
{
  "notification_hook": "python C:\\scripts\\oly_notify.py {kind} {session_ids}"
}
```

Placeholders available in the command:

- `{kind}`
- `{title}` / `{summary}`
- `{description}`
- `{body}`
- `{navigation_url}`
- `{node}`
- `{session_ids}`
- `{trigger_rule}`
- `{trigger_detail}`

The same values are also exported as `OLY_EVENT_*` environment variables.

Hooks are best-effort: failures are logged, but they do not block the session or notification pipeline.

---

## State, config, and files

Default state directory:

- **Windows**: `%LOCALAPPDATA%\oly`
- **Linux**: `$XDG_STATE_HOME/oly` or `~/.local/state/oly`
- **macOS**: `~/Library/Application Support/oly`

You can override it with `OLY_STATE_DIR`.

Inside that directory, `oly` stores:

- the SQLite database
- daemon logs
- per-session journals and metadata
- generated default `config.json`
- saved join configs on secondary nodes
- optional `wwwroot` static content

### Tunable `config.json` keys

These keys can be set in `config.json` (runtime overrides win over the file). Several are hot-reloaded when the file changes, so no daemon restart is needed:

| Key | Default | Purpose |
| --- | --- | --- |
| `silence_seconds` | `10` | Idle time before a session is considered silent for `input_needed` detection |
| `notification_min_interval_seconds` | `10` | Minimum seconds between repeat `input_needed` notifications for the same session |
| `screen_scrollback_rows` | engine default | Scrollback rows kept in memory per live session screen |

Session recordings live in a per-session journal (raw bytes, resizes,
lifecycle, checkpoints); there is no size cap key — retention is
checkpoint-gated. Inspect a session's journal; export
raw bytes with `oly logs --raw <id>`.

---

## Good fits for `oly`

- GitHub Copilot CLI, Claude Code, Gemini CLI, OpenCode, and similar agent workflows
- Long-running installs or migrations that may need approval later
- Interactive REPLs or TUIs you want to resume safely
- Background automation that still needs occasional human intervention
- Single-operator or small-team setups that want a lightweight supervision layer

---

## Learn more

- [MIGRATION.md](./MIGRATION.md) for the 0.3.x → 0.5.0 transition
- [SPEC.md](./SPEC.md) for the implementation-aligned product spec
- [ARCHITECTURE.md](./ARCHITECTURE.md) for the system overview
- [ARCHITECTURE.md](./ARCHITECTURE.md) for the architecture decision records

If you are building agent workflows and want durable, inspectable terminal sessions, `oly` is for you.
