# Troubleshooting

[Documentation](../README.md) · [CLI guide](./cli.md) · [Remote access](./remote-access.md) · [Architecture](../../ARCHITECTURE.md)

Start with:

```sh
oly --version
oly daemon status
```

`oly daemon status` reports whether the daemon is running and prints `Started at:`, `HTTP:`, `Auth:`, `ROOT:`, `LOGS:`, `SESSIONS:`, and `ID-PUB-KEY:`. See [State and configuration](./remote-access.md#state-and-configuration) for default paths and `OLY_STATE_DIR`.

## The daemon

### Startup flags seem to do nothing
`--bind`, `--port`, `--no-http`, `--no-auth`, and `--notification-hook` apply only when launching a daemon. Starting against an already running daemon is refused. To change listener flags, stop the daemon first (`oly daemon stop --grace 15`) — note that **stopping a daemon terminates all live sessions**.

### `daemon is already running` / `daemon is unavailable`
- Only one daemon can own a state directory (`oly.db`, `daemon.lock`, `daemon.sock`). Use a separate `OLY_STATE_DIR` for an isolated second daemon.
- `daemon is unavailable` means no daemon is listening on the state socket (or the CLI and daemon use different `OLY_STATE_DIR` / `OLY_SOCKET_NAME` values). In a restricted agent sandbox, `Access is denied` means IPC is blocked by the sandbox while the daemon may still be running — retry with approval/escalation rather than starting a second daemon.

### Linux/WSL IPC upgrade and isolation

The Linux default socket is now scoped to the canonical state directory and
`WSL_DISTRO_NAME`, rather than the shared `open-relay.oly.sock`. Upgrade the CLI
and daemon together. **Finish live work and stop the old daemon with the old
binary before replacing it**; a new CLI will not discover the old default
socket. If an old daemon is still running after the upgrade, an explicit
`OLY_SOCKET_NAME=open-relay.oly.sock` can address it, but verify ownership first.

WSL distributions can share a network namespace while having different PID
namespaces and filesystems. An old global or reused explicit socket name can
make the CLI reach another distribution's daemon. Symptoms include an empty
node inventory despite locally configured SQLite peers, missing local daemon lock/info files,
and no fresh local logs. Do not stop that daemon blindly. Use a unique explicit
socket name with older binaries, or the new scoped default, consistently in
both the daemon service environment and CLI shells. Custom containers that
share identical state paths/network namespaces still need distinct explicit
socket names if they do not provide a distinct `WSL_DISTRO_NAME`.

### Detached start never becomes ready
Check `<state>/logs/daemon.log`, `logs/daemon-crash.log` (Rust panics), and `logs/daemon-stderr.log`. When inspecting panics, preserve the **first** panic location (later `PoisonError` panics are usually secondary).

### `protocol mismatch: client=<cli>, daemon=<daemon>`
The CLI and running daemon were built from different protocol versions. Stop the daemon and start it with the matching binary; mixed-version IPC is unsupported.

### `oly ls` shows stale or missing sessions
When the daemon is unreachable, `oly ls` falls back to reading SQLite directly and prints a warning (`Daemon unavailable; falling back to direct DB access`). Use `oly daemon status` to confirm daemon liveness.

## Sessions

### Sessions show `failed` after a daemon restart
Live PTYs cannot survive a daemon stop/crash or host reboot. On startup, sessions left in `running` state are reconciled to `failed` while their journals remain readable via `oly logs`. Use `oly restart <id>` (or a detected `Resume command` from `oly info <id>`) to start a fresh run.

### A session looks stuck
Inspect before intervening:
1. `oly info <id> --json` — check `status`, `input_needed`, `program_status`, `exit_code`, and `last_output_epoch`.
2. `oly logs <id> --tail 200` or `oly logs <id> --screen` — inspect what the child last rendered.
3. If hung, send `oly send <id> key:ctrl+c` or run `oly stop <id> --grace 5` (`--grace 0` kills immediately).

### Disk usage keeps growing / byte-cap keys fail startup
Live journal byte caps (`max_journal_bytes_per_session`, `max_output_log_bytes`) were removed in 0.5 and fail startup if present in `config.json`. Remove those keys and use `journal_retention_days` (hourly sweep of stopped/killed/failed sessions) or OS disk quotas. `oly daemon status` reports the state volume's free space plus a journal/uploads/SQLite/logs breakdown; see [Operations](./operations.md#capacity-measurement-and-guard-boundaries) for the opt-in admission guard and its boundaries.

### Hand-edited `config.json` is ignored or rejected
`config.json` must be **flat** JSON:
- Unknown or nested keys emit a warning on stderr and are ignored.
- Malformed JSON, out-of-range values, or invalid regexes fail startup and reject hot reload (keeping the last valid config on reload). See `oly skill --daemon`.

## Attach and the dashboard

### Keys do nothing, or `Ctrl+D` behaves differently
- In `oly attach`, `Ctrl+D` **detaches** (leaving the child running). In `oly ls --follow`, `Ctrl+D` opens the Duplicate dialog (`Ctrl+C` exits the dashboard).
- `oly attach <id> --observe` is view-only: observers cannot send input or resize. Reattach without `--observe` to take control.

### Mouse wheel or scrollbar does nothing
Follow [Attach and scroll](./cli.md#attach-and-scroll): main-screen programs use your terminal's scrollbar/scrollback, whereas fullscreen/alternate-screen TUIs own their viewport and require application keys (`PgUp`/`PgDn`) or mouse reporting.

### Display is garbled or the wrong size
Geometry follows the last controller resize. Detach and reattach (or resize the controller terminal) to refresh geometry.

## Logs and cursors

- Default `oly logs <id>` renders the last 40 lines through the terminal model (use `--screen` for TUIs). Rendered views are lossy snapshots; use `oly logs <id> --since <cursor> --json` or `oly export <id>` for exact filtered display bytes.
- Wait exit codes: `0` condition met, `2` timeout, `1` error.
- Removed 0.3 flags (`--raw`, `--from`, `--from-file`, `--cols`, `--keep-color`, `--exit`, `--wait-for-prompt`) fail explicitly; see the [CLI guide](./cli.md#logs-views-waits-and-byte-continuation).
- If you suspect journal corruption, back up the entire state directory before taking action.

## Browser, phone, and notifications

- **Browser UI does not load:** Check `HTTP:` and `Auth:` in `oly daemon status` (`--no-http` disables the server; `GET /api/health` is unauthenticated). See [Browser access](./remote-access.md#browser-access).
- **Phone will not install PWA or enable Web Push:** Browsers require a secure context (`https://` or `localhost`). Use a TLS tunnel or reverse proxy in front of loopback.
- **No notification arrives:** Verify (1) session notifications are enabled (`oly info <id>`), (2) 5s activity suppression and `notification_min_interval_seconds` have elapsed, and (3) delivery works via `oly notify send --title "Test" --description "Check delivery"`.

## Connected machines

`oly node ls` lists configured primary and secondary peers, including disconnected
ones. Add/update/enable success is not a completed connection. If a peer fails:

1. Both ends must run matching versions; the node CLI redesign is not backward-compatible.
2. On the primary, inspect `oly node info worker-1`: it must be an enabled secondary with the secondary's verified key. `waiting` means authorized but not connected.
3. On the secondary, inspect `oly node info gateway`: it must be an enabled primary with the primary's verified key, reachable `http`/`https` URL, and `as` exactly `worker-1`.
4. Use `oly node ls --role primary --status retrying` and `oly node info gateway --json` for `last_error` and `next_retry_at`. `disabled` persists until `oly node enable NAME`; a daemon restart does not enable it.
5. Check state/IPC routing before restarting anything; see [Linux/WSL isolation](#linuxwsl-ipc-upgrade-and-isolation). Offline mutations fail rather than editing SQLite. HTTP `GET /api/nodes` lists live secondary routes, not all configured peers.
6. Inspect `<state>/logs/daemon.log.<UTC-date>` or `daemon-stderr.log` for handshake, send, and authenticated keepalive timeout reasons. No fresh local logs may mean the CLI is reaching the wrong daemon.
7. For rotation, independently verify the new key, disable both counterpart peers, update the affected pin, then enable and confirm `connected`. See [Trust and rotation](./remote-access.md#trust-encryption-and-rotation). Never fix a mismatch by removing identity checks.

## Reporting an issue

Collect and redact sensitive session text or keys before posting:

```sh
oly --version
oly daemon status
oly info <id> --json
oly logs <id> --tail 200 --json
```

Include your OS, whether the daemon runs detached, and relevant lines from `<state>/logs/daemon.log`.
