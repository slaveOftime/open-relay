# Upgrade from 0.3.x to 0.5.x

0.5 is currently a **breaking beta**. Back up state before upgrading.
For features, see [What's new in 0.5](./whats-new-0.5.md).
This guide includes the **current node redesign**, not attributed to a shipped
beta. Exact-version release notes remain historical; installing the published
beta does not by itself establish that it contains this redesign.

## Breaking changes and required actions

| Change | What you need to do |
|---|---|
| Old recordings are unreadable | 0.5 rejects `output.log` / `events.log`; there is no import. Keep a backup and the old binary if you need them. Metadata migrates, but recordings do not. |
| CLI/daemon protocol changed | Upgrade the CLI and daemon together (Protocol **15**). Live sessions cannot transfer to the new daemon. |
| Federation credentials changed | Upgrade every node together. Old API-key/SSH-key credential tables are dropped without conversion. Verify public identities independently and configure peer trust with `oly node add --role ... --key`. See [node setup](../guides/remote-access.md#connect-machines). |
| Log flags changed | Replace removed `--raw`, `--from`, `--from-file`, `--cols`, `--keep-color`, `--exit`, and `--wait-for-prompt` flags; examples below. |
| Live recording caps removed | Remove `max_output_log_bytes`, `max_journal_bytes_per_session`, and `max_journal_bytes_per_session_legacy`, even if zero. They now fail explicitly. Use OS quotas for a hard disk bound. |
| Linux default IPC is scoped | Upgrade CLI and daemon together. Finish work and stop the old daemon with the old binary before replacement; the new default will not discover the old global socket. Explicit `OLY_SOCKET_NAME` still overrides routing. See [IPC upgrade and isolation](../guides/troubleshooting.md#linuxwsl-ipc-upgrade-and-isolation). |
| Node management redesigned (current checkout) | Replace removed `oly join` and `oly node accept`; review names, roles, JSON consumers, and saved trust. See [current node redesign](#current-node-redesign). |
| Invalid config now fails startup | Fix malformed JSON, invalid regexes, or out-of-range values instead of relying on a silent reset. Keys remain flat, not nested under `limits`. Unknown keys warn; invalid reloads keep the last good config. |
| Custom apps/proxies are disabled by default | Enable flat `"trusted_apps_enabled": true` only for reviewed code. This also gates `wwwroot` overrides, not the normal UI. Proxies allow loopback IP literals only, not DNS names or remote hosts. |

Custom apps share the daemon's control-plane origin; they are **not sandboxed**.
For configuration, use [the daemon skill](../../skills/oly-daemon/SKILL.md).
`journal_retention_days` remains opt-in permanent deletion of stopped sessions,
not a live byte quota; keep backups outside the active state directory.

### Current node redesign

These changes apply when moving to the redesigned checkout, **not retroactively
to 0.5.0-beta.1 or beta.2**. There are no legacy command aliases.

| Previous interface | Replacement/action |
|---|---|
| `oly node accept --name worker-1 --id-pub-key KEY` | On the primary: `oly node add worker-1 --role secondary --key KEY`; use `update` for an existing entry |
| `oly join start --name worker-1 --id-pub-key KEY URL` | On the secondary: `oly node add gateway --role primary --url URL --key KEY --as worker-1` |
| `oly join ls` | `oly node ls --role primary`; inspect `node info NAME` for errors/retry state |
| `oly join ls --primary` / connected-only `oly node ls` | `oly node ls --role secondary --status connected` for connected peers; unfiltered `ls` includes all configured peers |
| `oly join stop --name worker-1` | `oly node rm gateway` removes the outbound peer; `disable gateway` retains it without reconnecting |
| Re-add replaces an existing connector/key | Duplicate add fails; use `oly node update NAME`. Role changes require remove/add |
| Mutation success implies first connection | Success now means persisted/applied only; verify `connected` with `ls`/`info` |
| Separate `joins.json` / `node_identities` trust stores | SQLite `nodes` is the only active source of truth; old files/rows are not live configuration |

`gateway` names the **remote primary locally**; `worker-1` is the secondary's
claimed local name and must match the authorization on that primary. Primary
entries require URL/`--as`; secondary entries reject them. URL validation now
allows only HTTP/HTTPS without userinfo, query, or fragment. `--key` is the
verified public Ed25519 identity, not a legacy API key or private seed.

Before replacement, record verified public pins, remote URLs, and claimed names
using the old tooling and take a stopped-state backup. The schema migration
drops legacy `node_identities` authorizations without copying them; old
`joins.json` is not imported or used. After starting the new daemon, provision
both ends explicitly with verified keys and inspect `oly node ls`. Do not edit
SQLite or `joins.json` to repair trust. Mutations fail offline. Confirm every intended
peer's role/key/name and connected state before resuming remote supervision.

Update scripts: `node ls --json` is `{items:[summaries],total:N}`, not an array;
`node info --json` summaries are flat, with `name`, `role`, `key`, `url`, `as`,
`enabled`, `status`, `last_error`, and `next_retry_at`. Status is runtime-only.
HTTP `GET /api/nodes` intentionally stays the live secondary `{name,connected}`
array, and session `--node` still targets only secondaries. The cryptographic
handshake is unchanged; this does not permit mixed-version CLI/daemon rollout.

Disable retains peer trust and persists across restart; rm deletes local trust
and ends federation only. Neither stops sessions on the owning daemon. For
rollback, restore the whole pre-redesign backup and old binary, not just the
legacy trust files; never point an old binary at a migrated database.

### Update log scripts

```sh
oly logs <id> --tail 40 --color always      # rendered output
oly logs <id> --screen                    # current/final screen
oly logs <id> --wait prompt --timeout 5m   # replaces --wait-for-prompt
oly logs <id> --wait exit --timeout 5m     # replaces --exit
oly logs <id> --since "<cursor>" --json    # incremental display bytes
oly export <id> > capture.bin             # recorded display-byte export
```

Exported bytes are filtered display output, **not raw PTY output**. Rendered
views are lossy. Wait-for-exit succeeds even if the child failed; inspect
`oly info <id> --json` for its exit code. See [log semantics](../guides/cli.md#logs-views-waits-and-byte-continuation).

## Safe upgrade

1. Use the old binary to finish live work and stop its daemon.
2. Back up the **entire state directory while stopped**, to a fresh protected
   location outside active state. Keep the old binary. Never share one state
   directory between daemon versions.
3. Install the published beta and confirm `oly --version` before opening state:
   ```sh
   npm install -g @slaveoftime/oly@beta
   oly --version
   ```
4. Start the new daemon, check `oly daemon status`, and test a fresh benign
   session. Old session metadata remains visible; old recordings are rejected.
5. After all nodes are upgraded, review/recreate peer configuration for the
   selected version and verify connectivity. For the redesign, use the steps
   above. Browser users may need to log in again.

Find your actual path with `oly daemon status` or `OLY_STATE_DIR`. Defaults:

| Platform | State directory |
|---|---|
| Linux | `$XDG_STATE_HOME/oly` or `~/.local/state/oly` |
| macOS | `~/Library/Application Support/oly` |
| Windows | `%LOCALAPPDATA%\oly` |

Example backup on Linux with the default path (choose a new destination):

```sh
oly daemon stop
cp -a ~/.local/state/oly ~/.local/state/oly-0.3-backup
```

Backups contain private config, keys, and session data. Do not blindly print old
recording files into a sensitive terminal: they can contain terminal controls.

## Rollback

Stop the 0.5 daemon, move its state aside, restore the **pre-upgrade backup**,
and reinstall the old binary before restarting. Do not run 0.3 against a
migrated database or expect it to read new journals. New sessions cannot be
merged into the old backup, and live processes cannot be recovered by rollback.

For beta compatibility guarantees and private protocol consumers, see
[Compatibility for 0.5](./compatibility-0.5.md).
