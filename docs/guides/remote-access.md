# Remote access, nodes, and notifications

[Documentation](../README.md) · [CLI guide](./cli.md) · [Architecture](../../ARCHITECTURE.md)

## Browser access

`oly daemon start --detach` serves the web UI and HTTP API on
**http://127.0.0.1:15443** by default, protected by a startup password.

```sh
oly daemon status                  # effective endpoint, auth mode, and paths
oly daemon start --detach --port 17000
oly daemon start --detach --no-http # CLI-only daemon
```

Listener bind/port flags apply at startup only; changing them requires a daemon
restart (which ends live sessions). See the [daemon skill](../../skills/oly-daemon/SKILL.md)
for `config.json` precedence and hot reload.

![The session list, grouped by working directory, with live status and activity charts](../../assets/oly-web-sessions-page.png)

![Session detail: live terminal, text input, quick keys, and file upload](../../assets/oly-web-detail-page.png)

The web UI installs as a PWA on mobile devices (requires a secure context: `https://` or `localhost`) and includes a touch key bar.

| Sessions on a phone | Detail on a phone |
|---|---|
| [![Mobile session list](../../assets/oly-web-sessions-page-mobile.png)](../../assets/oly-web-sessions-page-mobile.png) | [![Mobile session detail with the key bar](../../assets/oly-web-detail-page-mobile.png)](../../assets/oly-web-detail-page-mobile.png) |

Keep HTTP bound to loopback. For remote or phone access, place `oly` behind an authenticated tunnel or reverse proxy with TLS:

```text
browser → authenticated gateway / trusted tunnel (SSH, Tailscale, Cloudflare Access) → 127.0.0.1:15443
```

`--bind 0.0.0.0` exposes the listener without TLS or sandboxing. `--no-auth` disables HTTP password checks and should only be used behind an authenticating proxy. Authenticated operators can execute commands as the daemon OS user.

## Connect machines

Node federation lets a **primary** daemon route commands and streams to connected **secondary** daemons. Each owning daemon retains authority over its processes, journal, and attach roles. Run matching `oly` versions across all nodes; read the [compatibility policy](../releases/compatibility-0.5.md#node-federation) before deploying.

Each daemon generates a private Ed25519 seed (`<state>/identity_key`) and public identity (`<state>/identity_key.pub`, shown in `oly daemon status` as `ID-PUB-KEY`). Verify public identities out of band and never share `identity_key`.

A node's **role describes the remote peer**, not the local daemon. `NAME` always
names that peer locally. A primary peer is an outbound connection target;
a secondary peer is authorized to connect inbound. A daemon may have both roles.

```sh
# 1. On each machine: inspect ID-PUB-KEY
oly daemon status

# 2. On the primary: authorize the secondary's claimed name and public key
oly node add worker-1 --role secondary --key 'ed25519 <secondary-base64>'

# 3. On the secondary: name the primary locally and pin its public key
oly node add gateway --role primary --url http://primary-host:15443 \
  --key 'ed25519 <primary-base64>' --as worker-1

# 4. Check connection state on both ends; add success is not connectivity
oly node ls
oly node info gateway                 # on the secondary

# 5. On the primary: target a connected secondary with --node
oly start --node worker-1 --detach --title "nightly task" claude
oly ls --node worker-1
oly info <id> --node worker-1
oly logs <id> --node worker-1 --screen
oly send <id> --node worker-1 "continue" key:enter
oly attach <id> --node worker-1
oly stop <id> --node worker-1
```

`--as worker-1` is the local name claimed to the remote primary; it must exactly
match the secondary name authorized there. Primary peers require `--url` and
`--as`; secondary peers reject both. URLs must use `http` or `https`, without
userinfo, query, or fragment. `--node` targets secondary peers only; it does not
route sessions to a configured primary such as `gateway`.

### Manage configured peers

| Command | Effect |
|---|---|
| `oly node ls [--role primary\|secondary] [--status connected\|connecting\|retrying\|waiting\|disabled] [--json]` | List configured peers, including disconnected and disabled entries |
| `oly node info NAME [--json]` | Inspect configuration, current state, last error, and next retry |
| `oly node update NAME [--url URL] [--key KEY] [--as LOCAL_NAME]` | Change an existing peer; role cannot change |
| `oly node disable NAME` | Persist disabled state, end the link, and block reconnection; retain configuration |
| `oly node enable NAME` | Persist enabled state and resume connection/authorization |
| `oly node rm NAME` | Delete configuration and authorization, and end federation only; do not stop remote sessions |

Add enables a peer by default. A duplicate name fails: use `update`, not another
`add`. To change role, remove and add the peer deliberately. Every mutation
requires a running local daemon; there is no offline direct-write fallback.
Mutations return the current node state without waiting for the first connection.
Success means **persisted and applied**, not connected; confirm via `ls`/`info`.
Changes invalidate old links and in-progress handshakes; existing relayed streams
are interrupted, not reported as completed sessions.

| Status | Meaning |
|---|---|
| `connected` | Authenticated link established |
| `connecting` | Outbound connection attempt in progress |
| `retrying` | Outbound peer waiting for another attempt; inspect `last_error` and `next_retry_at` |
| `waiting` | Enabled secondary authorization waiting for an inbound link |
| `disabled` | Persistently disabled; no connection allowed |

`node ls --json` returns an object `{items: [...], total: N}`, not an array.
`node info --json` returns a flat summary with `name`, `role`, `key`,
`url`, `as`, `enabled`, `status`, `last_error`, and `next_retry_at`.
`enabled` is boolean; secondary `url`/`as` are null. `last_error` is nullable
text and `next_retry_at` is a nullable UTC timestamp. Runtime status is never persisted. Both outbound pins and inbound authorization
live in SQLite's `nodes` table, not `joins.json` or `node_identities`.

Enabled primary peers reconnect at daemon startup and after link failure with
1–60 s backoff, reset after a successful handshake. Multiple primaries have
independent connection, retry, liveness, and relay-task state. Both endpoints
abandon a connection after 45 s without authenticated inbound traffic; stalled
sends also have deadlines. Disabled state survives daemon restarts.

For existing UI clients, HTTP `GET /api/nodes` deliberately remains the **live
secondary route inventory**: an array of `{name, connected}` entries. It is not
the unified configured-peer inventory exposed by `oly node ls`.

### Trust, encryption, and rotation

- **Transport:** WebSocket (not SSH) using canonical `ed25519 <base64>` identities. No TOFU, SSH-agent, or API-key fallback. Each configured peer has one key, unique across local peer names; verify replacements independently.
- **Encryption:** Signed Federation v2 handshake with mutual `KeyConfirm`/`Ready` messages, followed by AES-256-GCM frame encryption derived via static-static X25519 + HKDF-SHA-256. **No forward secrecy** — run federation over a trusted network or tunnel. See [Architecture](../../ARCHITECTURE.md#node-federation).
- **Rotation:** Disable the peer on both ends, update the affected pin with `oly node update NAME --key 'ed25519 <new-base64>'`, then enable and inspect `node info` to confirm a fresh handshake. Do not restart a shared daemon just to rotate a key.
- **Decommission:** `oly node rm NAME` revokes that local authorization/pin and closes federation; remove the counterpart configuration too to stop its retries. `disable` is reversible containment. Neither operation kills sessions on the owning daemon. A compromised seed still requires identity replacement and transport containment; deletion cannot protect recorded traffic.

## Notifications

Desktop notifications, Web Push, and `notification_hook` alert the human when a session likely needs attention (after a 5s activity-suppression window and `notification_min_interval_seconds` debounce).

For programs emitting OSC 7501 (`idle`, `working`, `blocked`, `done`, `error`), `working` suppresses heuristic guesses while `idle`/`blocked`/`done`/`error` set `input_needed` immediately and trigger `program_status` / `input_needed` notifications without re-notifying on ordinary redraws. See [OSC 7501 status and lifetime](./cli.md#agent-reported-status-osc-7501).

```sh
oly notify enable <id>
oly notify disable <id>
oly notify send --title "Review ready" --description "Check the result."
oly notify send <running-id> --title "Needs review" --body "Inspect the session."
```

Notifications target humans, not other agent sessions (for agent-to-agent coordination, see the [subagent skill](../../skills/oly-subagent/SKILL.md)).

To run a custom hook, set `notification_hook` in flat `<state>/config.json`:

```json
{
  "notification_hook": "python /path/to/oly_notify.py {kind} {session_ids}"
}
```

Available placeholders (also exported as `OLY_EVENT_*` env vars): `{kind}`, `{title}` / `{summary}`, `{description}`, `{body}`, `{navigation_url}`, `{node}`, `{session_ids}`, `{trigger_rule}`, `{trigger_detail}`. Treat session-derived text as untrusted input.

## State and configuration

| Platform | Default state directory |
|---|---|
| Windows | `%LOCALAPPDATA%\oly` |
| Linux | `$XDG_STATE_HOME/oly` or `~/.local/state/oly` |
| macOS | `~/Library/Application Support/oly` |

Override with `OLY_STATE_DIR` (use `OLY_STATE_DIR="$PWD/.dev-state"` for development clones).
On Linux, default IPC names are scoped to the canonical state path and WSL
distribution. An explicit `OLY_SOCKET_NAME` overrides this: use the same value
for daemon and CLI, and distinct values for independent environments. Other
platforms keep their previous defaults. See [IPC upgrade and isolation](./troubleshooting.md#linuxwsl-ipc-upgrade-and-isolation).
State contains `oly.db` (including configured peers in `nodes`), `logs/`, `sessions/<id>/journal/`, `config.json`, `identity_key`, `identity_key.pub`, and optional `wwwroot/apps/`. Node configuration is managed through the daemon, not by editing state files.

Configuration in `<state>/config.json` is flat JSON:
- For config keys, defaults, and hot reload, see the [daemon skill](../../skills/oly-daemon/SKILL.md) (`oly skill --daemon`).
- For trusted web apps and loopback proxies (`trusted_apps_enabled`), see the [apps skill](../../skills/oly-apps/SKILL.md) (`oly skill --apps`).
