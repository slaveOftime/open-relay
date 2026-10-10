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
oly logs <id> --wait prompt --timeout 1m

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
- Attaching (CLI or browser) controls the session by default and resizes it to the attaching client's viewport. Any number of attached clients can drive input at the same time; geometry follows the most recent successful resize. Use `oly attach --observe` for a view-only attach that never drives input or geometry.
- Resizing an attached terminal window reports the new size to the daemon and resizes the session (controllers only — an observer's resize is gated out). When several controllers resize, the last successful resize wins.
- Input gating is coordination, not a security boundary: operator commands like `oly send` reach the session regardless of attach mode — no ceremony is needed (or available) for scripted input.

---

## What `oly` does well

### 1. Supervise agent sessions

Run coding agents, REPLs, installers, or approval-heavy workflows in the background without keeping one terminal open forever.

### 2. Detect likely human checkpoints

`oly logs --wait prompt` lets you block until a session likely needs attention, then inspect the output before deciding what to do next. `--wait output`, `--wait idle`, and `--wait exit` round out the gate.

### 3. Let humans stay in the loop

When a process needs confirmation, credentials, or a decision, you or agent can send input directly:

```sh
oly send <id> "continue" key:enter
oly send <id> key:ctrl+c
oly send <id> key:up key:enter
```

For arbitrary text — multi-line scripts, embedded quotes, anything that would otherwise force you to base64-encode and decode inside the target shell — use the free-text form. Once you pass `--`, every argument after it is sent as one literal blob joined with single spaces, with **no** per-token dispatch into `key:` / `oly-clipboard` / `oly-file:`:

```sh
# Multi-line body. The '--' opt-out lets you skip bash's quoting layers.
oly send <id> -- 'echo "hello world"
ls -la
git status'

# A token that happens to start with `key:` stays literal.
oly send <id> -- echo "press key:enter to continue"

# Compose with regular chunks before `--` (head still dispatches normally).
oly send <id> key:enter -- echo "after the enter"
```

Free-text mode is the easiest way to drive other CLI tools or bash from `oly send` without round-tripping through base64 or fighting shell escaping.
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
oly logs --wait prompt
oly send <id> "approve" key:enter
```

### Watch logs without attaching

```sh
oly logs <id> --tail 80
oly logs <id> --tail 80 --color always
oly logs <id> --screen                # current/last visible grid, including a blank final screen
```

### Lossless incremental read (agent API)

```sh
cursor=$(oly logs <id> --cursor)       # observe new bytes from now
oly logs <id> --since "$cursor" --json # exact base64 byte page; chain .cursor
# See the complete paging loop below.
```

### Start on a connected node

```sh
oly start --node worker-1 --title "nightly task" --detach claude
oly logs --node worker-1 --wait prompt <id>
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
| `oly attach [id] [--observe] [--node <name>]` | Reattach to a session (controls it by default; `--observe` is view-only) |
| `oly logs [id] [--tail <n>] [--screen] [--tail-frames <n>] [--wait <condition>] [--match <regex>] [--idle-for <duration>] [--since <token>] [--limit-bytes <n>] [--cursor] [--color <auto\|always\|never>] [--json] [--timeout <duration>] [--node <name>]` | Read recorded output, optionally after a gate, and hand back a cursor token for the next read |
| `oly export [id] [--json] [--node <name>]` | Dump the canonical filtered byte stream, control sequences and all, for files and pipes |
| `oly send [id] [chunk]... [--node <name>]` | Send text or special keys to a session. Use `--` to send arbitrary text verbatim without per-token dispatch (see "Let humans stay in the loop"). |
| `oly stop [id] [--grace <seconds>] [--node <name>]` | Stop a session |
| `oly restart <id> [--force] [--node <name>]` | Start a new session from persisted launch metadata (`--force` first kills a running source) |
| `oly rm [id] [--force] [--node <name>]` | Delete a stopped session and its logs (`--force` also kills a running session first) |
| `oly notify enable [id] [--node <name>]` | Enable notifications for a session |
| `oly notify disable [id] [--node <name>]` | Disable notifications for a session |
| `oly update <id> ...` | Override a session's title, tags, and notification setting |
| `oly skill [--apps \| --subagent \| --daemon]` | Print the bundled general, app, agent-CLI supervision, or daemon configuration skill |
| `oly daemon status` | Show the running daemon's effective flags and HTTP endpoint |

The interactive view can monitor several nodes at once, for example `oly ls --follow --node worker-a --node worker-b`. Add `--node-local` to include sessions from the current daemon (or the primary itself); the table shows a node column when multiple sources are selected. `Ctrl+D` opens a clone editor prefilled from the selected session, while `Ctrl+U` opens an update editor for the selected session's title, tags, and notification setting. `Tab`/`Ctrl+Tab` move between dialog fields, `Space` toggles notifications, and `Enter` submits the current dialog. Use `Ctrl+K` to stop the selected running session, `Enter` to open it inline, `Ctrl+Enter` to open it in another terminal window, and `Ctrl+C` to exit the list view.

Supported `oly send` key forms include named keys like `key:enter`, `key:tab`, `key:esc`, arrows, `home/end`, `pgup/pgdn`, `del/ins`, modifier forms like `key:ctrl+c`, `key:alt+x`, `key:meta+enter`, `key:shift+tab`, and raw bytes via `key:hex:...`.

### Reading session output: `oly logs`

`oly logs` separates rendered **views** from exact byte **continuation**.
The default is always the last 40 rendered text lines, even for TUIs.
Use `--screen` explicitly for a TUI. A wait only gates the selected output:

| Axis | Options |
| --- | --- |
| **Print surface** (pick at most one) | `--tail <n>` (default), `--screen`, `--tail-frames <n>` |
| **Gate** (never changes what is printed) | `--wait output` (alias `--wait new`), `--wait prompt`, `--wait idle`, `--wait exit`, `--wait match`. The last one needs `--match <regex>`. |
| **Byte continuation** | `--since <cursor>` with no view flag; `--limit-bytes <n>` defaults to 256 KiB (max 8 MiB). `--cursor` prints the current stream-end token. |
| **Format / routing** | `--color auto\|always\|never`, `--json`, `--node <name>` |

Views are deliberately lossy: terminal cells can be overwritten and tails
omit older lines. Their JSON cursors are **observation positions**, not claims
that every preceding byte was delivered. `--since` cannot combine with
`--tail`, `--screen`, or `--tail-frames`; it returns exact canonical filtered
bytes, possibly splitting UTF-8/control sequences. Keep a decoder/parser
across pages. JSON uses `mode: "stream"`, base64 `bytes`, `encoding`,
`start_cursor`, next `cursor`, `has_more`, `running`, and `exit_code`.
Empty pages preserve the cursor. `has_more` describes this read's observed end.

Frame history samples after complete recorded output events, not arbitrary
I/O chunks or application-defined redraws. It includes blanks, preserves
main/alternate buffers, and deduplicates only consecutive identical styled
states (A → B → A remains three observations). Resizes affect the next output
sample; resize-only changes are visible immediately in `--screen`.
Counts: tail 0..65535, frames 0..1024. Zero prints nothing without replay.
Tails render retained scrollback plus the visible grid at recorded geometry,
with continuous parser/styles/main/alternate state; long lines can wrap at
that geometry. Cold tail/frame reconstruction is linear in recording size;
warm reads use a shared byte-budgeted LRU and replay only appended records.
Rebuilds coalesce per session/mode, not across unrelated sessions. Oversized
entries are served uncached. The conservative cache-admission charge for
Unicode/OSC input is not a read limit: large recordings remain readable.
Requested frame payload above 64 MiB, recorded grids above one million cells,
or tail grids/history above eight million cells fail explicitly rather than
silently dropping observations. For count-related limits, request
fewer frames/lines; use byte paging or export when rendering is unsuitable.

```bash
# Human: read rendered output
oly logs <ID> --tail 40 --color always
oly logs <ID> --screen                          # current/last visible grid

# TUI history: last N distinct screens (oldest first)
oly logs <ID> --tail-frames 3

# Gate, then read
oly logs <ID> --wait prompt --screen            # block until input needed, then print the live screen
oly logs <ID> --wait exit  --tail 40            # block until the session ends, then read its tail

# Lossless future-byte observation (requires jq and base64)
id=<ID>
cursor=$(oly logs "$id" --cursor)
while :; do
  page=$(oly logs "$id" --since "$cursor" --wait output --timeout 60s --json)
  rc=$?
  [ "$rc" -eq 2 ] && continue                  # timeout: cursor unchanged
  [ "$rc" -eq 0 ] || break                    # stale cursor/error: inspect
  printf '%s' "$page" | jq -r .bytes | base64 --decode
  cursor=$(printf '%s' "$page" | jq -r .cursor)
  [ "$(printf '%s' "$page" | jq -r .running)" = false ] &&
    [ "$(printf '%s' "$page" | jq -r .has_more)" = false ] && break
done

# Pattern trigger
oly logs <ID> --wait match --match 'DONE|FAILED' --tail 40

# Status hint: the only line a quiet stream needs (exit 2 on timeout, 0 on met)
oly logs <ID> --cursor --json                   # {"cursor": "Aa..", "running": true}
```

- **Cursors are opaque.** A token encodes `session | incarnation | offset`;
  handing it to the wrong session, or to a session that restarted since, is
  rejected loudly. Pair `--cursor` with `--since` to resume losslessly.
- **Wait exit codes:** `0` condition met, `2` timeout, `1` error. `--timeout`
  accepts plain milliseconds or `s`/`m`/`h` suffixes; `0` waits forever
  (default 30s).
- **Idle means "quiet", never "done".** A silent session may be thinking,
  blocked, or crashed. Confirm with `--screen` or a `--wait match` pattern.
- **Match is line-scoped.** Regex sees canonical bytes decoded with UTF-8
  replacement, including controls. Page boundaries are transparent; partial
  prompts can match. No cross-line matches; an unmatched line over 1 MiB is
  an error rather than a silently missed match. An ended session without a
  match fails promptly.
- **Restart fencing:** every wait/read checks incarnation; a restart fails
  instead of attaching an old cursor to a new stream.
- **Timeout:** prints no content and advances no cursor. JSON emits one
  `outcome: "timeout"` object with `cursor: null`, then exits 2.
- **Modifier validation:** timeout needs a wait; idle-for needs wait idle;
  match needs wait match. Cursor-only cannot take wait modifiers.
- **Combinations that would be silently meaningless** (e.g. `--tail --screen`,
  `--cursor --since`, `--wait exit --match foo`) are usage errors.

### Canonical byte export: `oly export`

`oly export` writes the canonical filtered byte stream straight to stdout for
pipes and files, using bounded pages locally and remotely. It exports a fixed
prefix captured at invocation, not an endless live feed. JSON incrementally
serializes base64 without buffering the recording. It warns on stderr when
stdout is a terminal. Restart/read failures are nonzero; streaming output
already written cannot be retracted, so discard partial output on error:

```bash
oly export <id> > capture.bin     # canonical bytes, control sequences included
oly export <id> --json            # {"bytes": "...", "size": 12345}
```

Subtle: `oly export` writes the **filtered** stream, so terminal queries that
the scanner has already answered are not present. Use the journal directly if
you need the unfiltered raw PTY bytes; for normal screen captures the
filtered stream is the one a user actually saw.

### Federation commands

| Command | Purpose |
| --- | --- |
| `oly api-key add <name>` | Create an API key on the primary and print it once |
| `oly api-key ls` | List API key labels on the primary |
| `oly api-key remove <name>` | Revoke an API key on the primary |
| `oly node accept --name <name> -k <secondary-pub>` | Register a secondary's Ed25519 pub key on the primary (SSH key auth) |
| `oly join start --name <name> --key <key> <url>` | Connect this daemon to a primary (API key auth) |
| `oly join start --name <name> --ssh-pub-key <primary-pub> <url>` | Connect this daemon to a primary (SSH key auth) |
| `oly join stop --name <name>` | Disconnect and remove a saved join config |
| `oly join ls` | List saved outbound join configs on this daemon |
| `oly join ls --primary` | Ask the daemon for currently active primary-side joins |
| `oly node ls` | List secondary nodes currently connected to the primary |

---

### Federation joining — two auth methods

**Method 1 — API key (shared secret):**

```bash
# On the primary, generate a scoped API key
oly api-key add myserver
# prints: a3f4c1b2... (copy this once)

# On the secondary, join with the key
oly join start --name myserver --key a3f4c1b2... http://192.168.1.100:15443
```

**Method 2 — SSH key (challenge-response, MITM protected):**

Every daemon (primary *and* secondary) auto-generates its own Ed25519
identity key on first start at `<state>/ssh_host_key{,pub}`. To join over
SSH, the operator copies the **secondary's** identity pub key to the
**primary** and pins the **primary's** identity pub key on the
**secondary**.

```bash
# 1. Look up the identities with `oly daemon status` on each side:
#      it prints "SSH PUB: ssh-ed25519 <base64...>" — the canonical line.

# 2. On the primary, register the secondary's identity pub key:
oly node accept --name myserver -k "<secondary's SSH PUB>"

# 3. On the secondary, start the join, pinning the primary's identity:
oly join start --name myserver --ssh-pub-key "<primary's SSH PUB>" http://192.168.1.100:15443
```

`oly join ls` on the secondary prints the canonical pub key it just signed
with, mirroring the line on the primary. Real `ssh-keygen` public key
lines are accepted too (in addition to the canonical internal form) and
normalized automatically; only Ed25519 keys are supported.

**`--ssh-pub-key` is the trust root.** When you start the join, the
secondary uses this pinned pub key to derive AES-256-GCM channel keys
(static-static X25519 ECDH against its own identity). There is **no
in-band host-key exchange** — no `GET /api/nodes/host-key` endpoint,
no `get_host_key` / `host_key` wire frames, no per-connection nonce.
If the primary's identity pub key is wrong (typo, restore from a
different backup, MITM attempt), the connector's handshake still
completes but the post-Join channel goes silent because both sides
derive different shared secrets.

**How SSH join works under the hood:**

1. The secondary opens the WebSocket and sends a `hello` frame with
   its own Ed25519 identity pub key (so the primary can derive the
   channel keys).
2. The secondary signs the join payload with its own Ed25519 identity
   key. The signed payload covers a domain tag, the claimed node
   **name**, and the signer's **public key** — no plaintext secret
   ever crosses the wire.
3. The primary verifies the signature against the public key registered
   via `oly node accept`. After accepting, both sides derive the same
   per-direction AES-256-GCM keys from `ED25519→X25519(their pub)` and
   switch to sealed frames for everything that follows.
4. The primary's WebSocket layer keeps `Joined` itself in plain, so
   the secondary can flip its own phase to Sealed only after it sees
   the success response — sealing begins from the first frame after
   `Joined` on both sides.

The per-daemon identity key pair lives in the state dir
(`ssh_host_key` / `ssh_host_key.pub`); when SSH join is used the *only*
signing key is the one the daemon generated for itself — there is no
user-managed key file and no TOFU file to hand-edit.

**Combined steps for a typical pair:**

```bash
# --- On the primary ---
oly daemon start

# On the primary, capture its own identity pub key:
oly daemon status   # note the "SSH PUB: ssh-ed25519 ..." line

# --- On the secondary ---
# A separate SSH key file is no longer required — the daemon's identity
# key lives in its own state dir. Run `oly daemon status` to read it:
oly daemon status   # note the "SSH PUB: ..." line

# On the primary (paste the secondary's pub key):
oly node accept --name myserver -k "<secondary's SSH PUB>"

# On the secondary (pin the primary's pub key you captured above):
oly join start --name myserver --ssh-pub-key "<primary's SSH PUB>" \
    http://192.168.1.100:15443
```

**Reverse direction (secondary→primary commands):**

After joining, run commands targeting the secondary from the primary's CLI:

```bash
oly node ls           # see connected secondaries
oly send --node myserver --screen --tail 20
oly attach --node myserver <session-id>
oly logs --node myserver <session-id>
oly stop --node myserver <session-id>
```

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

### Working on `oly` from a clone

When iterating on `oly` itself, keep the dev daemons and the per-session
journal directories **out** of the repo root. Set:

```sh
export OLY_STATE_DIR="$PWD/.dev-state"
```

before running `oly daemon start` (or `cargo run -- daemon start`). The
repo's `.gitignore` already excludes `.dev-state/` and the per-session
`<id>/` directories underneath it, so the working tree stays clean.
`CONTRIBUTING.md` documents the same convention with examples.

### Tunable `config.json` keys

These keys can be set in `config.json` (runtime overrides win over the file). Several are hot-reloaded when the file changes, so no daemon restart is needed:

| Key | Default | Purpose |
| --- | --- | --- |
| `silence_seconds` | `10` | Idle time before a session is considered silent for `input_needed` detection |
| `notification_min_interval_seconds` | `10` | Minimum seconds between repeat `input_needed` notifications for the same session |
| `screen_scrollback_rows` | engine default | Scrollback rows kept in memory per live session screen |

Session recordings live in a per-session journal (raw bytes, resizes,
lifecycle, checkpoints); there is no size cap key — retention is
checkpoint-gated. Inspect a session's journal; export the filtered stream
with `oly export <id>`.

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
- [ARCHITECTURE.md](./ARCHITECTURE.md) for the system overview and architecture decision records
- [CONTRIBUTING.md](./CONTRIBUTING.md) for development workflow

If you are building agent workflows and want durable, inspectable terminal sessions, `oly` is for you.
