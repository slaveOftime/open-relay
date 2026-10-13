# Migrating from 0.3.x to 0.5

The 0.5 development line is a **breaking preview**; stable installs currently
use 0.3.x. Release channels depend on the **tag**, not the branch: tags with a
prerelease suffix publish to GitHub prereleases and npm `beta`; stable tags
publish to the stable channels. See [Releasing](./docs/RELEASING.md).

0.5 will not read 0.3 session recordings, and old binaries will not read new
ones. The recording change does not convert or delete old files, but SQLite
migrations remove obsolete federation credential tables. **Back up state before
switching.** Live processes cannot move between daemons or versions.

## What changes

| Area | 0.5 contract | Upgrade action |
|---|---|---|
| Recordings | One segmented, incarnation-fenced `journal/` per session instead of `output.log`/`events.log` | Preserve old files separately; old recordings are rejected explicitly, not imported |
| Terminal model | Alacritty is the authoritative backend engine; vt100 is test-only | No user action; browser xterm.js remains a rendering client |
| CLI/daemon IPC | Protocol **14** with binary attach frames after JSON init | Upgrade CLI and daemon together; protocol mismatch fails explicitly |
| Federation | Federation v2, pinned Ed25519 identities, encrypted readiness and application frames | Upgrade every node, re-authorize identities, recreate joins; no mixed-version rollout |
| Machine credentials | Preview API-key and SSH-key credential paths removed | No `api-key` command or join `--key`; use `--id-pub-key` |
| Browser auth | Password login and issued session tokens; origin checks and revocation | Log in again; HTTP `--no-auth` never disables federation identity auth |
| Attach control | Multiple concurrent controllers; `--observe` is view-only | Coordinate input; last successful controller resize wins; roles are not authorization |
| Input/detach | Explicit paste boundaries; `Ctrl-D` detaches | Clipboard shortcuts include Ctrl-V/Shift+Insert; detach does not stop child |
| Log reads | Default tail 40, explicit screen/frame views, bounded cursor continuation | Update scripts to the commands below; removed flags are errors |
| Configuration | **Flat JSON**; stopped-session age retention only | Remove `max_output_log_bytes` / `max_journal_bytes_per_session`; they now fail explicitly. Live byte caps are unsupported. Do not wrap keys in `limits`. |
| Daemon status | Effective running flags and endpoint, not just client config | Check the actual endpoint after restart |

### Configuration validation

An existing unreadable/malformed config or invalid prompt/resume regex now fails
startup; hot reload preserves the last good snapshot. Unknown keys emit named
warnings (including mistakenly nested objects). HTTP bind must be an IP address,
port must be 1..65535; resource/duration bounds are documented in the daemon skill.
Fix the file rather than relying on a silent default reset. Missing configs are
created without overwriting an existing file and with owner-only permissions on Unix.

### Log-script changes

Do not use obsolete `logs --raw`, `--from`, `--from-file`, `--cols`,
`--keep-color`, `--exit`, or `--wait-for-prompt` flags. Current forms:

```sh
oly logs <id> --tail 40 --color always
oly logs <id> --screen
oly logs <id> --tail-frames 3
oly logs <id> --wait prompt --timeout 5m
oly logs <id> --wait exit --timeout 5m
oly logs <id> --wait idle --idle-for 2s --timeout 1m
oly logs <id> --wait match --match 'DONE|FAILED' --timeout 5m
oly logs <id> --cursor
oly logs <id> --since "<cursor>" --json
oly export <id> > capture.bin
```

Rendered views are lossy. Continuation returns exact **filtered display bytes**,
base64 in JSON, with a next cursor and `has_more`. Cursors bind session and
incarnation; preserve them on timeout (exit 2). Exit waits return 0 when the
child ends, even on child failure: inspect `oly info <id> --json` for exit status.
See the [CLI guide](./docs/CLI.md#logs-views-waits-and-byte-continuation).

### Federation credential cleanup

Migrations 0001–0009 retain their historical checksums. Migration 0010 creates
`node_identities`; 0011 drops obsolete `api_keys` and `ssh_keys` **without copying
credentials**. Session/push data is preserved. Old identity files are not
converted or deleted automatically.

Each daemon generates `<state>/identity_key` (private seed) and
`identity_key.pub` (canonical `ed25519 <standard-base64(raw32)>`). Independently
verify public identities, register each exact secondary name/key on its primary,
and pin the primary with `join start --id-pub-key`. Accept keeps `-k` as a
public-key shortcut; join does not. No SSH transport/import, TOFU, in-band key
replacement, or shared API-key fallback exists. Static-static DH provides no
forward secrecy. Follow the [remote setup guide](./docs/REMOTE.md#connect-machines).

## Safe upgrade

The following shell examples are for Linux with the default state path. Use
the actual `OLY_STATE_DIR` or platform path reported by `oly daemon status`.
On Windows use equivalent PowerShell copy/move commands; on macOS use
`~/Library/Application Support/oly`.

### 1. Inspect and finish live work

Use the **old binary's supported list command** (the current CLI uses `oly ls`)
and inspect daemon status. Finish or explicitly stop sessions that matter.
A 0.3 daemon owns its live PTYs; 0.5 cannot adopt them.

### 2. Stop the old daemon, then back up

```sh
oly daemon stop
cp -a ~/.local/state/oly ~/.local/state/oly-0.3-backup
```

Stop before copying to get a quiescent SQLite/journal backup. Choose a fresh
backup destination, confirm the copy, and keep it outside the new daemon's
writable state. Backups contain private identity/config/session data; protect
access. Do not start two versions against the same state directory.

### 3. Install the intended version

For a **published preview**:

```sh
npm install -g @slaveoftime/oly@beta
```

Or download its exact prerelease archive. To build this checked-out source
(Rust and Node.js/npm required):

```sh
cargo install --path . --locked
```

Do not assume unqualified npm/Cargo/Homebrew stable installs contain the 0.5
preview. Confirm `oly --version` before opening existing state.

### 4. Start, inspect, and test a fresh session

```sh
oly daemon start                  # follow the password setup; Ctrl+C stops this foreground daemon
# From another terminal:
oly daemon status
oly ls
oly start --detach --title "upgrade check" echo hello
oly logs <returned-id> --wait exit --timeout 30s
oly info <returned-id> --json      # confirm exit code and metadata
```

Use `--detach` for normal background daemon operation if preferred. SQLite
migrates forward on open. Old session rows remain visible, but old recordings
are not readable through the new log/attach paths. Recreate federation joins
only after all nodes have compatible binaries.

## Preserve old recordings without inventing provenance

There is **no automatic import**. Old filtered bytes and event lines cannot
reconstruct guaranteed raw-byte ordering, resize offsets, checkpoints, or
truncated history. 0.5 rejects pre-0.5 logs explicitly instead of guessing.

- `output.log` is the old **filtered display stream**, not recoverable raw PTY
  bytes. Inspect it as untrusted data in a file viewer or isolated terminal
  parser. Direct replay can execute terminal controls; do not blindly print it
  into a sensitive terminal.
- `events.log` contains newline-delimited lifecycle/resize JSON.
- Retain the old binary and backup if you need the old reader. Never point it
  at a migrated database.

The new journal is the sole terminal recording. It captures raw PTY output,
resizes, lifecycle, and checkpoints; replay derives the filtered stream.
See [Architecture](./ARCHITECTURE.md) for the design rationale.

## Retention: removed live caps and permanent stopped-session cleanup

`max_journal_bytes_per_session`, `max_output_log_bytes`, and the internal legacy
spelling are removed and fail explicitly, even when set to zero. The previous
scheme only deleted older incarnations; ordinary long-running sessions stay in
one incarnation, so it could not provide the advertised disk bound. Remove these
keys before starting 0.5. The journal recovery/retention primitives remain internal,
but no supported setting enables live recording deletion.

Use an OS filesystem quota if you need a hard disk bound; disk-write failures stop
the affected session rather than silently continuing without a recording.

Stopped-session age cleanup remains opt-in and hot-reloadable (default `0`, keep
all sessions):

```json
{
  "journal_retention_days": 30
}
```

An hourly sweep permanently deletes stopped/killed/failed session directories
and DB rows whose `ended_at` exceeds the configured age. Running sessions are
excluded. Enabling it can delete old recordings left on disk after upgrade,
so keep the independent backup first.

See the [daemon skill](./skills/oly-daemon/SKILL.md) for configuration/reload
verification. Config keys are flat, not nested under `limits`.

## Rollback

1. Stop the 0.5 daemon and confirm no live sessions remain.
2. Move new state aside, preserving it if needed.
3. Restore the **pre-upgrade backup** to the original path.
4. Install the old binary and restart using the restored state.

```sh
oly daemon stop
mv ~/.local/state/oly ~/.local/state/oly-0.5-saved
cp -a ~/.local/state/oly-0.3-backup ~/.local/state/oly
# Reinstall the intended 0.3 binary, then start it.
```

Do not run old binaries against the migrated DB. New journals are not promised
to be readable by old binaries. Recordings survive client disconnect, not live
process loss; `oly restart` starts a new process rather than recovering a PTY.

## Tool authors

IPC one-shot requests/responses use newline-delimited envelopes with
`{"version":14,"payload":...}`. Attach sends a JSON init line, then
`[u32 LE payload_len][u8 tag][payload]`: output tag 1 carries an offset and raw
output bytes; control tag 2 carries bare JSON `RpcResponse`. Browser WebSocket
uses its own binary transport envelope over the same attach contract.

Protocol source and fixtures are authoritative: `src/protocol.rs`, `src/ipc.rs`,
`web/src/api/ws-frames.ts`, and `tests/fixtures/ws_frames.json`. Prefer `info
--json`, cursor-based `logs`, or `export` over implementing private IPC yourself.
