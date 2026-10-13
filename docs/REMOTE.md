# Remote access, nodes, and notifications

[README](../README.md) · [CLI guide](./CLI.md) · [Architecture](../ARCHITECTURE.md)

## Browser access

`oly daemon start --detach` serves the web UI and HTTP API on
**http://127.0.0.1:15443** by default, protected by a password set at startup.

```sh
oly daemon status                  # effective endpoint, auth mode, and paths
oly daemon start --detach --port 17000
oly daemon start --detach --no-http # CLI-only daemon
```

These start variants are alternatives, not ways to reconfigure an already
running daemon. Listener bind/port changes require a restart, which stops its
live sessions. See the [daemon skill](../skills/oly-daemon/SKILL.md) for config
precedence and hot reload.

Keep HTTP on loopback. For access from a phone or another machine, prefer:

```text
browser → authenticated gateway / trusted tunnel → local oly HTTP service
```

Examples include SSH tunnels, Tailscale/Headscale, Cloudflare Access, or a
reverse proxy with strong authentication and TLS. `--bind 0.0.0.0` exposes the
listener beyond loopback; it does not add TLS or a sandbox. `--no-auth` disables
browser/HTTP password authentication and should only be used behind protection
you control. An authenticated operator can run commands as the daemon user;
view-only attach is coordination, not a restricted account.

## Connect machines

A **primary** routes commands to connected **secondary** daemons. Each owning
daemon keeps authority over its processes, journal, attach roles, and credits.
Run compatible versions on all nodes; mixed-version rollout is unsupported.

Each daemon generates a private 32-byte Ed25519 seed at `<state>/identity_key`
and publishes `<state>/identity_key.pub`. Exchange and verify **public**
identities through an independent trusted channel. Never share the private seed.

```sh
# On each machine: read the ID-PUB-KEY line.
oly daemon status

# On the primary: bind the exact secondary name to its public identity.
oly node accept --name worker-1 --id-pub-key "<secondary public identity>"

# On the secondary: pin the independently verified primary identity.
oly join start --name worker-1 --id-pub-key "<primary public identity>" \
  http://primary-host:15443

# On the primary: inspect and use the connected worker.
oly node ls
oly start --node worker-1 --detach --title "nightly task" claude
oly ls --node worker-1
oly info <id> --node worker-1
oly logs <id> --node worker-1 --screen
oly send <id> --node worker-1 "continue" key:enter
oly attach <id> --node worker-1
oly stop <id> --node worker-1
```

`oly join ls` lists saved outbound joins; `oly join ls --primary` queries the
local daemon's active primary-side connector inventory. `oly join stop --name
worker-1` disconnects and removes the saved join. Connectors retry with backoff.

### Trust and encryption

Transport is **WebSocket, not SSH**. Only canonical
`ed25519 <standard-base64(raw32)>` public identities are accepted. There is no
SSH-agent integration, OpenSSH import, TOFU, key-discovery endpoint, or shared
API-key fallback. One identity belongs to one registered name per primary.

Federation v2 binds version, roles, name, both identities, and fresh nonces
in a signed transcript. Encrypted KeyConfirm/Ready/Joined messages complete
mutual confirmation before registration or application RPCs. The configured
pin and accept table are trust anchors; challenges do not replace them.

Static-static X25519 DH derived from the identities feeds HKDF-SHA-256;
AES-256-GCM seals application frames and keepalives with ordered sequences.
There is **no forward secrecy**: later private-seed compromise can expose
recorded federation traffic. Use trusted transport infrastructure too.
Federation identity checks and encryption remain required with HTTP `--no-auth`.
See [Architecture](../ARCHITECTURE.md#node-federation) for protocol details.

## Notifications

Input-needed notifications are best-effort attention hints, not proof of a
prompt or task completion. A quiet session may still be thinking or blocked.
Desktop delivery, browser push, and a custom hook can alert the human; always
inspect the current output before intervening.

```sh
oly notify enable <id>
oly notify disable <id>
oly notify send --title "Review ready" --description "Check the result."
oly notify send <running-id> --title "Needs review" --body "Inspect the session."
```

A source session must be running; omit the source when it has ended.
`notify send` requires a title. Notifications go to humans, **not** another
agent session. See the [subagent skill](../skills/oly-subagent/SKILL.md) for
worker supervision and optional session-to-session reporting.

Configure a hook in the **flat** `<state>/config.json`:

```json
{
  "notification_hook": "python /path/to/oly_notify.py {kind} {session_ids}"
}
```

On Windows, JSON paths need escaped backslashes, such as
`C:\\scripts\\oly_notify.py`. Hook command placeholders:

- `{kind}`, `{title}` / `{summary}`, `{description}`, `{body}`
- `{navigation_url}`, `{node}`, `{session_ids}`, `{trigger_rule}`, `{trigger_detail}`

The values are also exported as `OLY_EVENT_*` environment variables. Use a
trusted helper and treat session-derived text as untrusted input. Hook failures
are logged without blocking the session or notification pipeline.

## State and configuration

| Platform | Default state directory |
|---|---|
| Windows | `%LOCALAPPDATA%\oly` |
| Linux | `$XDG_STATE_HOME/oly` or `~/.local/state/oly` |
| macOS | `~/Library/Application Support/oly` |

`OLY_STATE_DIR` overrides it. State includes `oly.db`, daemon logs,
`sessions/<id>/journal/`, generated `config.json`, daemon identity files,
saved `joins.json`, and optional `wwwroot/apps/` content. Keep private state
out of shared repositories and backups protected.

For a development clone, use `OLY_STATE_DIR="$PWD/.dev-state"` rather than the
normal operator state. Tests use separate temporary directories.

Config is flat JSON, **not** nested under `limits` or `notify`. For available
keys, actual defaults, and reload behavior, use `oly skill --daemon` or the
[daemon skill](../skills/oly-daemon/SKILL.md). Recording retention is disabled
by default. Its byte budget is checkpoint-gated and not a hard filesystem
quota; age retention permanently removes stopped sessions and their files.

To publish an HTML result or proxy a local web app, read `oly skill --apps`
or the [apps skill](../skills/oly-apps/SKILL.md).
