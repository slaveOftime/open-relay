# Troubleshooting

[Documentation](../README.md) · [CLI guide](./cli.md) · [Remote access](./remote-access.md) · [Architecture](../../ARCHITECTURE.md)

Most answers start with two commands:

```sh
oly --version
oly daemon status
```

`oly daemon status` prints either `Daemon is not running.` or the running
daemon's `Started at:`, `HTTP:`, `Auth:`, `ROOT:`, `LOGS:`, `SESSIONS:`, and
`ID-PUB-KEY:` values. The `LOGS:` and `SESSIONS:` paths are where the evidence in
this guide lives. See [State and configuration](./remote-access.md#state-and-configuration)
for per-platform defaults and `OLY_STATE_DIR`.

Read the error text before changing anything: `oly` names the layer that failed,
and accepted input is not task completion.

## The daemon

### Start flags seem to do nothing

`--bind`, `--port`, `--no-http`, `--no-auth`, and `--notification-hook` apply at
startup only. Starting again against a live daemon is refused:

```text
a daemon is already running with this state directory:
stop it first (`oly daemon stop`) to start with different flags.
```

`oly daemon stop --grace <seconds>` waits before forcing termination (default 15).
**Stopping a daemon ends its live sessions**: a client disconnect is what preserves
a child, not a daemon restart.

### `daemon is already running` / `daemon is unavailable`

Two daemons cannot share one state directory — `oly.db`, `daemon.lock`, and
`daemon.sock` are single-writer. For a second, isolated daemon, give it its own
`OLY_STATE_DIR`.

`daemon is unavailable: ...` means nothing is listening on the state socket
(`OLY_SOCKET_NAME` overrides its name). A socket or lock left behind by a killed
daemon is reclaimed before binding, so a persistent message means the daemon is
genuinely not running — or that the CLI is pointed at a different state directory
than the daemon. Check `oly daemon status` on both sides.

### A detached start reports the daemon never became ready

Either `daemon process exited before becoming ready. Check logs under <path>` or
`daemon failed to become ready in time`. Read `<state>/logs/daemon.log`; a Rust
panic is recorded in `logs/daemon-crash.log` and detached stderr in
`logs/daemon-stderr.log`. Preserve the **first** panic's location: later
`PoisonError` panics can be secondary failures, and release backtraces may lack
symbols.

### `protocol mismatch: client=<cli>, daemon=<daemon>`

The CLI and the running daemon are different builds; the numbers are each side's
`PROTOCOL_VERSION`. `oly` refuses rather than guessing a wire format, and the
daemon-side wording is `protocol version <n> is not supported`. Stop the daemon
and start it with the new binary. There is no mixed-version support and no
in-place upgrade of a running daemon.

### `oly ls` shows stale or missing sessions

`oly ls` falls back to reading the database directly when the daemon is
unreachable, and exits **0**:

```text
⚠️ Daemon unavailable; falling back to direct DB access (data may be stale)
```

The JSON form emits `warning: daemon unavailable; falling back to direct DB access
(data may be stale)`. The fallback cannot see live state, so treat a successful
`oly ls` as proof of the database, not of the daemon. Prefer `oly daemon status`
before trusting it.

## Sessions

### A session shows `failed` after a restart

A daemon restart cannot recover live PTYs. Sessions left in a running state are
reconciled to `failed` with no exit code and reported on startup. This is expected,
not corruption: the journal is intact, and `oly logs` still reads it.

Use `oly info <id>` (`Resume command`) and `oly restart <id>` to relaunch from
persisted launch metadata. `restart` starts a **new** process; it does not resume
the previous process or the agent's own conversation.

### A session looks stuck

Distinguish three cases before intervening:

- `oly info <id> --json` for `status`, `input_needed`, `program_status`,
  `exit_code`, and `last_output_epoch`.
- `oly logs <id> --tail 200` for what the child last painted.
- `oly ls --follow` and `Ctrl+K` to stop, `Ctrl+R` to resume from the list.

A quiet session may be thinking; attention hints are heuristics, not proof of a
prompt. TUI applications that never emit a prompt legitimately produce no hint.
`oly stop <id>` escalates: Ctrl-C, repeated Ctrl-C, then Ctrl-D, SIGTERM on Unix
at 3/4 of `--grace`, then a tree kill at the deadline. `--grace 0` kills immediately.

### Output is lost, or disk keeps growing

Live journal deletion is unsupported — there is no byte cap, and
`max_journal_bytes_per_session` and friends are **removed keys that fail startup**
rather than being ignored. Bound disk with OS quotas.

`journal_retention_days` (default `0`, disabled) is the only reaper: it deletes
whole `sessions/<id>` directories **and** their database rows for `stopped`,
`killed`, and `failed` sessions, measured from `ended_at`, on an hourly sweep. A
session can therefore outlive the configured age by up to an hour.

### Hand-edited config is ignored or rejected

`config.json` is **flat** JSON. A nested or misspelled key is reported on stderr and
ignored:

```text
warning: <config path>: unknown config key '<key>' ignored; config keys must be flat (see oly skill --daemon)
```

A file that cannot be parsed fails startup instead. Hot reload keeps the last good
configuration on error. Removed byte-cap keys are a hard error, not a warning, and
name the replacement: `... was removed: live journal byte caps cannot bound a
current incarnation; remove this key and use journal_retention_days for stopped
sessions or an OS disk quota (see the migration guide)`. Use `oly skill --daemon`
for the authoritative key list and defaults.

## Attach and the dashboard

### Keys do nothing, or the wrong thing

`Ctrl+D` is overloaded: in `oly attach` it **detaches**, while in the
`oly ls --follow` dashboard it opens the Duplicate dialog (`Ctrl+C` exits the
dashboard). After attaching inline from the dashboard, the exit hint is
`Press Ctrl+D to return to the session list`.

Input is also gated by role: `oly attach <id> --observe` is view-only and prints
`Attached in observe mode (view-only). Ctrl-D detaches.` Observers cannot send
input or resize. Reattach without `--observe` to drive the session. There is no
`--read-only` flag; the role is chosen at attach time.

### Mouse wheel or the scrollbar does nothing

Mouse events reach the child only when the attach client is allowed to drive **and**
the child has enabled mouse reporting. The rule that resolves most confusion is in
[Attach and scroll](./cli.md#attach-and-scroll): main-screen programs use your
terminal's scrollbar or scrollback, fullscreen/alternate-screen programs own their
viewport and need the application's keys or mouse wheel.

### The display is garbled, or the wrong size

Resize follows the last successful controller resize; observers never resize. If
geometry looks wrong, detach and reattach, or resize so a controller reports the
new size. On Windows, console mouse and VT-input modes are preserved per attach —
if a child's input behaves oddly after reattaching, detach fully and reattach
rather than leaving a second client attached.

## Logs and cursors

`oly logs <id>` renders the last 40 lines **through the terminal model**, so a TUI
looks like the screen, not like its byte stream. Rendered views are lossy by
design: they are observation positions, not transcripts, and a cursor is not a
timestamp. For exact bytes use `export`, or `--since` with a bounded page.

Wait exit codes are `0` met, `2` timeout, `1` error. A timeout is not a failure of
the child.

If you are carrying flags forward from an older release, these were removed and are
rejected rather than ignored: `--raw`, `--from`, `--from-file`, `--cols`,
`--keep-color`, `--exit`, `--wait-for-prompt`. Use the flags in the
[CLI guide](./cli.md#logs-views-waits-and-byte-continuation).

Journal recovery is automatic: a torn tail is truncated to the last valid record,
and a CRC-corrupt segment is quarantined and reported instead of being truncated.
There is currently **no user-facing integrity command** — verification exists only
in tests. If you suspect journal corruption, preserve the whole state directory
before doing anything else.

## Browser, phone, and notifications

### The browser UI does not load

Check the endpoint and mode from `oly daemon status`: a daemon started with
`--no-http` serves no UI at all. The default is `http://127.0.0.1:15443` with
password auth enabled. `GET /api/health` is unauthenticated (as are
`/api/auth/*`), so it distinguishes "listener down" from "not logged in". Reach it
from your phone via a gateway or tunnel — see
[Browser access](./remote-access.md#browser-access). `--bind 0.0.0.0` widens the
listener but adds no TLS and no sandbox.

### The phone will not install the PWA or enable push

Browsers allow service-worker installation and Web Push only in a secure context
(`https://`, or `localhost`). Reaching the daemon over plain `http://` on a LAN
address gives you the UI without install or push. Terminate TLS at a reverse proxy
or tunnel in front of loopback. Note that the repository's browser tests cover
Chromium only; real-phone-install behavior is not verified by CI.

### No notification arrives

Notifications are best-effort. In order:

1. The session's own notifications are enabled — `oly update <id> --notifications enabled`,
   or check `Notifications` in `oly info <id>`.
2. The trigger fired. A plain `OSC 9` is stripped, `OSC 9;4` progress passes
   through, and agent-reported state uses `OSC 7501`. Other private sequences,
   including `OSC 777`, are **not** notification triggers.
3. The timing gates passed: a 5-second activity-suppression window, then
   `notification_min_interval_seconds` (default `10`).
4. Delivery worked. A configured `notification_hook` receives the
   `{kind} {title} {description} {body} {navigation_url} {node} {session_ids} {trigger_rule} {trigger_detail}`
   placeholders and the matching `OLY_EVENT_*` environment variables. Hook failures
   are logged and never block the session, so a broken hook looks like silence.

Test the delivery path independently with `oly notify send --title "Review ready" --description "Check the result."`.

## Connected machines

### A node will not connect

`oly node ls` on the primary lists **connected** nodes only; `oly join ls` on the
secondary shows each join as `connected` or `saved`. A `saved` join is retrying
with exponential backoff, so a correct configuration can simply be waiting.

Then check, in order:

- `oly join start` without `--id-pub-key` fails with
  `missing --id-pub-key <PRIMARY_PUB_KEY>` — the pin is the trust root and is never
  discovered automatically.
- The primary must `oly node accept` the **exact** name and public identity. One
  identity cannot be bound to two names.
- Versions must match across nodes; the handshake binds the version and
  mixed-version rollout is unsupported.
- `--no-auth` does not relax federation: identity checks and encryption stay on.

Rotating an identity with `oly node accept` does not disconnect an
already-connected node. Restart the secondary's connector to force
re-verification. There is no primary-side removal command in 0.5.x; see
[Node federation](../releases/compatibility-0.5.md#node-federation) for the
supported rotation and decommission procedure.

## Before reporting a problem

There is no self-service diagnostic bundle yet, so include:

```sh
oly --version
oly daemon status
oly info <id> --json
oly logs <id> --tail 200 --json
```

plus the operating system, whether the daemon was started detached, and the
platform-specific lines from `<state>/logs/daemon.log` around the failure. Redact
`ID-PUB-KEY:` and anything from a session's output that you would not share —
agent output is untrusted data, and this project's issue tracker is public.

## Limits that are not bugs

- Live PTYs do not survive a daemon stop/crash or a host reboot.
- `restart` creates a new process; it does not resume an agent's conversation.
- Input delivery is not task completion, and attention hints are heuristics.
- No live journal byte cap; use OS quotas.
- No forward secrecy in federation, and no primary-side node removal in 0.5.x.
- No mixed-version protocol support.

See [Compatibility for 0.5](../releases/compatibility-0.5.md) for the full list.
