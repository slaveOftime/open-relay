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

```sh
# 1. On each machine: inspect ID-PUB-KEY
oly daemon status

# 2. On the primary: authorize the secondary's name and public key
oly node accept --name worker-1 --id-pub-key "<secondary public identity>"

# 3. On the secondary: connect and pin the primary's public key
oly join start --name worker-1 --id-pub-key "<primary public identity>" \
  http://primary-host:15443

# 4. On the primary: target the connected node with --node
oly node ls
oly start --node worker-1 --detach --title "nightly task" claude
oly ls --node worker-1
oly info <id> --node worker-1
oly logs <id> --node worker-1 --screen
oly send <id> --node worker-1 "continue" key:enter
oly attach <id> --node worker-1
oly stop <id> --node worker-1
```

`oly join ls` lists saved outbound joins on a secondary (`oly join ls --primary` inspects active connector inventory). `oly join stop --name worker-1` disconnects and removes the saved join.

### Trust, encryption, and rotation

- **Transport:** WebSocket (not SSH) using canonical `ed25519 <base64>` identities. No TOFU, SSH-agent, or API-key fallback.
- **Encryption:** Signed Federation v2 handshake with mutual `KeyConfirm`/`Ready` messages, followed by AES-256-GCM frame encryption derived via static-static X25519 + HKDF-SHA-256. **No forward secrecy** — run federation over a trusted network or tunnel. See [Architecture](../../ARCHITECTURE.md#node-federation).
- **Key rotation & decommission:** Re-run `oly node accept --name worker-1 --id-pub-key "<new-key>"` on the primary, then restart the secondary connector (`oly join stop` + `oly join start`) so the live connection re-verifies. 0.5.x has no primary-side `node remove` command, and each public key is unique to one node name. See [Node federation](../releases/compatibility-0.5.md#node-federation).

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

Override with `OLY_STATE_DIR` (use `OLY_STATE_DIR="$PWD/.dev-state"` for development clones). State contains `oly.db`, `logs/`, `sessions/<id>/journal/`, `config.json`, `identity_key`, `joins.json`, and optional `wwwroot/apps/`.

Configuration in `<state>/config.json` is flat JSON:
- For config keys, defaults, and hot reload, see the [daemon skill](../../skills/oly-daemon/SKILL.md) (`oly skill --daemon`).
- For trusted web apps and loopback proxies (`trusted_apps_enabled`), see the [apps skill](../../skills/oly-apps/SKILL.md) (`oly skill --apps`).
