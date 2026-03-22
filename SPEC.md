# Specification: Open Relay (`oly`)

**Version:** 0.1.0  
**Language:** Rust  
**Status:** Implementation-aligned living spec

This document describes the implemented product surface for `oly` as reflected by the current CLI and runtime behavior. For deeper internal design details, see [ARCHITECTURE.md](./ARCHITECTURE.md).

---

## 1. Purpose

`oly` is a persistent session relay for long-running interactive CLI workloads.

A background daemon owns PTY-backed sessions so commands can outlive the terminal that launched them. Users and agents can:

- start commands under daemon ownership
- detach and reattach later
- inspect persisted logs
- send input without attaching
- receive notifications when a session likely needs attention
- access a local web UI / HTTP API
- proxy commands to connected secondary nodes

`oly` is aimed at AI agent supervision, interactive tooling, and other terminal workloads that benefit from durable control and auditability.

---

## 2. Product positioning

`oly` is not primarily a terminal multiplexer. It is a **session supervision layer**.

Compared with a traditional multiplexer:

- the daemon, not a single terminal, owns the workload
- logs persist independently of attach state
- input can be injected without opening an interactive terminal
- likely input-needed checkpoints can trigger notifications
- browser-based supervision and node federation are part of the product surface

---

## 3. Implemented scope

### 3.1 In scope

1. Local daemon process managed by the same `oly` binary.
2. PTY-backed session lifecycle: start, attach, detach, input, logs, stop.
3. Persisted session metadata and output logs.
4. Prompt / input-needed detection with notifications.
5. Local HTTP API and browser UI on loopback by default.
6. Password-protected web access by default, with explicit `--no-auth` opt-out.
7. Primary / secondary node federation via authenticated daemon-to-daemon connection.
8. Notification hooks for custom local automation.

---

## 4. High-level architecture

Single binary, three primary surfaces:

- **CLI**
  - parses commands from `src/cli.rs`
  - routes requests in `src/main.rs`
  - talks to the local daemon over IPC
- **Daemon**
  - owns live sessions
  - persists metadata and logs
  - performs notification and prompt-detection work
  - serves the local HTTP/UI surface unless disabled
- **HTTP/UI surface**
  - serves a browser UI and HTTP endpoints on `127.0.0.1:<port>`
  - exposes REST, SSE, and WebSocket session control primitives

Federation extends this model by letting a secondary daemon connect outbound to a primary daemon over WebSocket. CLI requests can then be wrapped with `--node <name>` and proxied transparently.

---

## 5. Session model

Each session has durable metadata plus live runtime state.

Notable fields include:

- `id`
- optional `title`
- `command` and `args`
- optional `cwd`
- `created_at`
- optional `pid`
- session `status`
- whether input is currently likely needed

Implemented status values exposed through `oly ls --status` are:

- `created`
- `running`
- `stopping`
- `stopped`
- `killed`
- `failed`
- `unknown`

---

## 6. CLI contract

### 6.1 Top-level commands

The implemented top-level commands are:

- `daemon`
- `start`
- `notify`
- `skill`
- `ls`
- `stop`
- `attach`
- `logs`
- `send`
- `api-key`
- `join`
- `node`

### 6.2 Daemon commands

### `oly daemon start [--detach] [--port <port>] [--no-auth] [--no-http]`

- Starts the daemon.
- Runs in the foreground by default.
- `--detach` starts it in the background.
- `--port` overrides the default HTTP port.
- `--no-auth` disables HTTP authentication and requires explicit risk acknowledgement.
- `--no-http` disables the HTTP API and browser UI entirely.

### `oly daemon stop [--grace <seconds>]`

- Requests daemon shutdown.
- Waits up to `--grace` seconds for sessions to exit cleanly before forcing termination.
- Default grace period is `15` seconds.

### 6.3 Session lifecycle commands

### `oly start [--title <title>] [--detach] [--disable-notifications] [--cwd <dir>] [--node <name>] <cmd> [args...]`

- Starts a PTY-backed session under daemon ownership.
- `--cwd` is resolved relative to the caller's current working directory when a relative path is provided.
- Without `--detach`, `oly` immediately tries to attach after creation.
- With `--detach`, `oly` prints the session ID and exits.
- `--disable-notifications` disables notification delivery for that session.
- `--node` runs the command on a connected secondary node.

### `oly ls [--search <text>] [--json] [--status <status>]... [--since <rfc3339>] [--until <rfc3339>] [--limit <n>] [--node <name>]`

- Lists sessions.
- `--search` filters by title or ID substring, case-insensitively.
- `--json` returns machine-readable output.
- `--status` is repeatable and accepts the implemented values from section 5.
- `--since` and `--until` accept RFC3339 timestamps.
- `--limit` defaults to `10`.
- `--node` targets a connected secondary node.

### `oly attach [id] [--node <name>]`

- Attaches to a running session.
- Replays recent buffered output first, then switches to live IO.
- If `id` is omitted, `oly` resolves the most recently created session.
- Detach escape is `Ctrl-]`, then `d`.

### `oly logs [id] [--tail <n>] [--keep-color] [--no-truncate] [--wait-for-prompt] [--timeout <duration>] [--node <name>]`

- Prints recent logs without attaching.
- If `id` is omitted, `oly` resolves the most recently created session.
- `--tail` defaults to terminal height minus one line when available, otherwise `40`.
- `--keep-color` preserves ANSI color codes.
- `--no-truncate` disables column truncation when rendering.
- `--wait-for-prompt` waits until the session likely needs input or exits, then prints logs.
- `--timeout` accepts plain milliseconds or `ms`, `s`, `m`, `h` suffixes.
- Default timeout is `30s`; `0` means wait forever.

### `oly send [id] [CHUNK]... [--node <name>]`

- Sends input to a session without attaching.
- If `id` is omitted, `oly` resolves the most recently created session.
- Positional chunks are processed left to right.
- Plain chunks send literal text.
- `key:<spec>` sends terminal key sequences.
- When no chunks are provided and stdin is piped, `oly` reads all stdin and sends it as input.

Supported key forms include:

- named keys: `enter`, `tab`, `esc`, `backspace`, `up`, `down`, `left`, `right`, `home`, `end`, `pageup`, `pagedown`, `delete`, `insert`
- modifier forms: `ctrl+<char>`, `alt+<char|key>`, `meta+<char|key>`, `shift+tab`
- raw bytes: `hex:<hex-bytes>`

### `oly stop [id] [--grace <seconds>] [--node <name>]`

- Stops a session.
- If `id` is omitted, `oly` resolves the most recently created session.
- Default grace period is `5` seconds before hard termination.

### 6.4 Notification commands

### `oly notify enable [id] [--node <name>]`

- Enables notifications for a running session.
- If `id` is omitted, `oly` resolves the most recently created session.

### `oly notify disable [id] [--node <name>]`

- Disables notifications for a running session.
- If `id` is omitted, `oly` resolves the most recently created session.

### 6.5 Skill command

### `oly skill`

- Prints the bundled `oly` skill markdown used for agent/tooling guidance.

### 6.6 Federation commands

### `oly api-key add <name>`

- Creates an API key on the primary daemon.
- Prints the plaintext key once.

### `oly api-key ls`

- Lists API key labels and creation timestamps on the primary daemon.

### `oly api-key remove <name>`

- Revokes a named API key.

### `oly join start --name <name> --key <key> <url>`

- Saves an outbound join configuration on the secondary.
- Requests the local daemon to connect to the primary.
- If the daemon is not running, the saved join config remains and will be used on the next daemon start.

### `oly join stop --name <name>`

- Removes the saved join config and requests disconnection.

### `oly join ls`

- Lists saved outbound join configs on the current daemon.

### `oly join ls --primary`

- Lists active joins from the daemon's primary-side view.

### `oly node ls`

- Lists currently connected secondary nodes on the primary daemon.

---

## 7. HTTP and browser surface

When HTTP is enabled, the daemon serves on loopback:

```text
http://127.0.0.1:15443
```

The port is configurable via `oly daemon start --port <port>` or config.

Implemented HTTP surface includes:

- auth status, login, and logout endpoints
- health endpoint
- session CRUD and log endpoints
- session event streaming via SSE
- interactive attach via WebSocket
- push subscription endpoints
- node listing and join-related endpoints
- static asset serving for the bundled web UI and optional local `wwwroot`

`--no-http` disables this entire surface.

---

## 8. Prompt detection and notifications

`oly` maintains prompt-like pattern matching plus idle detection to decide when a session likely needs input.

The default pattern set includes common confirmation, password, token, shell, and agent-style prompts such as:

- `(y/n)`
- `[y/n]`
- `[yes/no]`
- `password:`
- token / secret prompts
- `? ` prompt lines
- `continue?`
- `are you sure`
- `press enter`

Important implemented behaviors:

- session notifications can be enabled or disabled per session
- `oly logs --wait-for-prompt` blocks until the daemon reports likely input-needed state or the timeout expires
- notifications can be routed through a custom `notification_hook`

---

## 9. Persistence and configuration

Default state directory:

- Windows: `%LOCALAPPDATA%\oly`
- Linux: `$XDG_STATE_HOME/oly` or `~/.local/state/oly`
- macOS: `~/Library/Application Support/oly`

`OLY_STATE_DIR` overrides the state root.

Persisted state includes:

- daemon logs
- SQLite database
- session directories with logs and metadata
- `config.json`
- saved join configs
- optional `wwwroot` assets

Important implemented config fields include:

- `http_port`
- `log_level`
- `prompt_patterns`
- `web_push_subject`
- `web_push_vapid_public_key`
- `web_push_vapid_private_key`
- `max_running_sessions`
- `session_eviction_seconds`
- `notification_hook`

---

## 10. Security model

### 10.1 Local CLI / IPC

- CLI commands talk to a local daemon over local IPC.
- The daemon enforces local caller checks according to the host platform.

### 10.2 HTTP authentication

By default, the HTTP/UI surface is password protected.

Implemented behavior:

1. `oly daemon start` prompts for a password unless `--no-auth` is used.
2. Passwords are stored as Argon2 PHC hashes, not plaintext.
3. `GET /api/auth/status` is public and reports whether auth is required.
4. `POST /api/auth/login` issues an auth token on success.
5. `POST /api/auth/logout` revokes the token.
6. Static assets and selected public endpoints remain reachable for the login flow.

Lockout behavior:

- failed login attempts are tracked per client IP
- `3` failed attempts from the same client trigger a `15` minute lockout for that client
- successful login resets the failure counter for that client

### 10.3 Remote access pattern

`oly` is intended to stay local-first.

The recommended remote deployment pattern is:

```text
remote browser/client -> strong auth gateway -> secure tunnel -> local oly HTTP service
```

Examples include Cloudflare Access, Tailscale, or SSH tunneling.

---

## 11. Federation model

One daemon can act as the **primary** and accept outbound secondary connections.

The current model is:

1. Generate a key on the primary with `oly api-key add`.
2. Start a join from the secondary with `oly join start --name <name> --key <key> <url>`.
3. The secondary daemon maintains a connection to the primary.
4. Session-oriented commands can target the secondary with `--node <name>`.

This allows the CLI and browser surface to supervise sessions across multiple machines without changing the session command model.

---

## 12. Known boundaries

Current non-goals or intentionally externalized concerns:

- public internet exposure and TLS termination
- hosted auth / identity integration
- organization-wide approval policy engine
- daemon adoption of arbitrary pre-existing processes

---

## 13. References

- [`src/cli.rs`](./src/cli.rs)
- [`src/main.rs`](./src/main.rs)
- [`src/config.rs`](./src/config.rs)
- [`ARCHITECTURE.md`](./ARCHITECTURE.md)
- [`ARCHITECTURE_PTY.md`](./ARCHITECTURE_PTY.md)
