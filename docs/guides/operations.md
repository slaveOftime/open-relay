# Daemon operations: capacity, diagnostics, and backups

[Documentation](../README.md) · [Troubleshooting](./troubleshooting.md) · [Remote access](./remote-access.md) · [CLI guide](./cli.md)

This guide covers the operational contracts of the 0.5 daemon: what it measures about disk capacity, when it refuses new sessions, how to read its health, and how to take and verify an offline backup.

## Capacity: measurement and guard boundaries

The daemon samples the **state volume** (the filesystem that holds `OLY_STATE_DIR`) in a bounded background task — never on the list, health, or output hot paths. Every sample carries its time, and a sample that cannot read free space carries the failure reason instead of a number: "unknown" is never rendered as `0`.

The size breakdown separates the four things people confuse:

| Category | What it covers |
|---|---|
| `journal` | Recorded session journals under `sessions/<id>/journal` |
| `uploads` | Files attached to sessions under `sessions/<id>/files` |
| `sqlite` | The session database and its `-wal`/`-shm` companions |
| `logs` | Daemon logs under `<STATE_DIR>/logs` |

A size scan that hits its entry budget or encounters a read/metadata error reports `partial` and its numbers are a lower bound. Symlinks are not followed. The `journal` column is not "all disk usage" — declared uploads, SQLite, and logs are separate.

### Thresholds

| Config key | Default | Effect |
|---|---|---|
| `disk_warn_free_bytes` | `2147483648` (2 GiB) | Always-on warning line; must be `>= disk_min_free_bytes` |
| `disk_min_free_bytes` | `1073741824` (1 GiB) | Floor consulted only while the guard is enabled |
| `disk_sample_interval_seconds` | `30` | Background sampling cadence (`5..=86400`) |
| `disk_sample_max_age_seconds` | `120` | Samples older than this refuse starts while the guard is enabled (`5..=86400`, `>= disk_sample_interval_seconds`) |
| `disk_guard_enabled` | `false` | Opt-in admission guard (see below) |

Invalid combinations (warn below the floor, max age below the interval, values above 86400 s) fail startup and reject hot reload, like every other config key.

### The admission guard is advisory, not a quota

With `disk_guard_enabled: true`, a **new** session start is refused when the newest sample is:

- below `disk_min_free_bytes`, or
- **unknown or stale** — the guard never assumes free space it cannot observe.

Refusals name the reason and the threshold to change, and are counted as `disk_guard_decisions_total{label=disabled|allowed|unknown|stale|below_minimum}`. The boundaries are deliberate:

- Existing sessions are never killed, paused, or evicted by a disk sample, low or not.
- It does not reserve space: writes after the check (including by other processes) can still fill the volume. Use filesystem quotas or OS-level monitoring for hard limits.
- Concurrent starts reserve a session slot under one lock and recheck the sample when their blocking spawn worker begins. This does not reserve disk bytes: multiple starts can pass the same reading, and filesystem/PTY preparation can race later writes.
- Journal deletion stays a separate, explicit policy: `journal_retention_days` removes stopped/killed/failed sessions on an hourly sweep. Live journals are never auto-deleted. Each sweep validates the session id, deletes files before the database row, and counts failures per stage (`retention_sweep_failures_total{label=unsafe_id|dir|db}`), so a failed sweep is retried rather than silently skipped.

Related metrics: `state_disk_free_bytes` (the series is removed when free space becomes unknown; disk gauges expire at the sampling-time freshness deadline even if the sampler stops), `state_disk_bytes{label=journal|uploads|sqlite|logs}`, `disk_guard_decisions_total`, `retention_sweep_failures_total`, `retention_sessions_evicted_total`.

## Diagnostics: liveness is not readiness

`oly daemon status` answers liveness and then prints the read-only diagnostic summary: version, `Started at`, readiness, supervised-component states, disk capacity, failure-record paths, and affected sessions.

- **Liveness** — `GET /api/health` (public) is "the daemon answered". A degraded background component never makes the process look dead.
- **Readiness** — `GET /api/ready` (public) returns `200` only while every ready-critical task runs and an enabled disk guard permits new starts, otherwise `503` with component/admission states. Public probes never include private failure text or filesystem paths; use `oly daemon status` for detailed reasons. Task liveness does not prove individual tick progress or a successful free-space probe when the guard is disabled. Detached startup waits for IPC liveness, not readiness, so degraded daemons remain inspectable.

Supervised components are `http`, `sse_encoder`, `activity_sampler`, `notification_monitor`, and `disk_sampler` (ready-critical), plus `config_reloader` and `journal_retention` (supervised and reported, but their failure does not gate readiness). A component that exits or panics is restarted with a bounded budget (three restarts, 1 s/2 s/4 s backoff) and then reported as `failed` with its reason; `disabled` marks intentional absence such as HTTP under `--no-http`. The daemon itself keeps answering IPC throughout.

- **Failure records** — the report lists the exact paths on the owning machine: the log directory, `logs/daemon-crash.log` (Rust panics), and `logs/daemon-stderr.log`.
- **Affected sessions** — current in-flight start reservations (id order), then sessions that ended in `failed` (newest first), capped at 20 with the total reported. Reservations are volatile and do not survive a daemon crash. Failed metadata queries produce `diagnostic_errors` and an unknown (`null`) total, never a fabricated empty success.
- **Remote owners** — `oly daemon status --node <NAME>` relays the summary from a secondary's daemon; the primary never substitutes its own disk numbers or liveness. If the local relay is unavailable, remote status fails explicitly rather than claiming the secondary is stopped. `--json` prints the same report for tooling.

The summary is assembled from metadata only: no terminal content, input, environment, identity seeds, or credentials. `oly export <id>` is a separate, explicit terminal-bytes export and is never part of a diagnostic summary. Even so, treat any diagnostic text as potentially sensitive and review it before sharing.

## Listener changes and stopping a daemon

`http_bind`, `http_port`, and `--no-http` are startup-only settings: change them in `config.json` and restart the daemon deliberately. A restart (or any `oly daemon stop`, host reboot, or crash) ends all live PTY sessions — that is the documented contract, and there is no way to migrate a live child to a new daemon process.

Never stop a shared daemon without operator approval, and never start a second daemon against the same state directory to dodge a restart. After a stop, sessions that were `running` are reconciled to `failed` at the next startup while their journals stay readable; the `Resume command` shown by `oly info <id>` is a suggestion for the operator and is never executed automatically.

## Offline backup

A supported backup is taken **while no daemon runs**:

1. Stop the daemon and confirm it: `oly daemon stop`, then `oly daemon status` must report that the daemon is not running.
2. Copy the **entire** state directory — `oly.db` together with any `-wal`/`-shm` companions, the `sessions/` journals, `config.json`, and the identity keys. Copying the directory while a daemon runs is not a supported backup: SQLite and journals are mid-write and files change underneath the copy.
3. Store the copy outside the active state directory and keep it together with the version that produced it.

## Restore drill

Restore into an **isolated** state directory so the original stays untouched, and verify the copy before trusting it:

```sh
OLY_STATE_DIR=/path/to/restored oly daemon start --detach --no-http
OLY_STATE_DIR=/path/to/restored oly daemon status   # readiness + component states
OLY_STATE_DIR=/path/to/restored oly ls              # backed-up sessions appear
OLY_STATE_DIR=/path/to/restored oly logs <id>       # journals replay
OLY_STATE_DIR=/path/to/restored oly daemon stop
```

On Windows, set `OLY_STATE_DIR` with `$env:OLY_STATE_DIR = "C:\path\to\restored"` (PowerShell) or `set OLY_STATE_DIR=...` (cmd) before each command. Use `--no-http` (or a free port; HTTP mode then prompts for a password) and a distinct `OLY_SOCKET_NAME` when the original daemon may run concurrently.

What the drill proves and what it does not:

- It proves the copied database opens, sessions are listed, and journals replay through `oly logs`.
- Sessions that were `running` at backup time come back as `failed`: PTYs and child processes do not exist in a backup. Use `oly restart <id>` (or the suggested resume command) to start a fresh run.
- It does not prove the backup was taken at a consistent moment if the daemon was running during the copy — that is why the offline rule above is the first step.

The end-to-end regression `e2e_offline_backup_restore_drill_recovers_journal_and_metadata` automates exactly this drill: stop, copy, restore into an isolated directory, list, replay logs.
