# Migrating from 0.3.x to 0.5.x (beta)

oly 0.5.0 is a **beta / preview release** on the `v0.5.x` branch. The
stable line remains 0.3.x on `main`.

0.5.0 is a **clean break** from 0.3.x. It will not read your old session
recordings, and old binaries will not read the new ones. Your data is
never deleted by upgrading — but you must back it up yourself before
switching.

Read this file fully before upgrading a machine that has sessions you care
about.

## Why 0.5.0 breaks compatibility

0.3.x accumulated three parallel ways of doing the same thing: two persisted
output streams (`output.log` + `events.log` vs. the journal), two terminal
parsers (vt100 vs. the alacritty engine), and two attach wire formats (base64
JSON vs. binary frames). Each duplicate was a place where behavior could
silently disagree — and did. 0.5.0 keeps exactly one implementation of each
concern, chosen by the ADRs in `docs/adrs/`:

- **Storage:** the per-session journal is the only persisted stream
  (ADR-0002, ADR-0006). It records raw output, resizes, lifecycle, and
  checkpoints with durability cursors; everything else (the filtered display
  stream, resize history, logs, attach snapshots) is *derived* from it at read
  time, so derived state can never disagree with the recording.
- **Terminal parsing:** one engine (alacritty_terminal) renders live attach,
  logs, and web views (ADR-0001). vt100 remains only as a test-only
  conformance oracle.
- **Streaming:** attach output travels as binary frames, not base64-in-JSON
  (ADR-0004). Control messages stay newline-delimited JSON.

Keeping 0.3.x readers alive would mean shipping the old bugs as permanent
compatibility branches. We chose a single supported stack plus an explicit,
honest migration instead.

## What breaks

| Area | 0.3.x | 0.5.0 | What you must do |
|---|---|---|---|
| Session recordings | `output.log` + `events.log` per session | `journal/` directory per session (segmented, incarnation-fenced) | Nothing readable automatically; see "Preserving old recordings" below. 0.5.0 prints an explicit error for pre-0.5 sessions instead of guessing. |
| CLI ↔ daemon protocol | IPC protocol v4 | IPC protocol **v13** | CLI and daemon must both be 0.5.0. A mismatch exits with a precise version error — no silent misbehavior. |
| Attach stream framing | base64 chunks inside JSON lines | binary frames after the JSON init line | Only relevant to tooling that spoke IPC directly; update it or pipe through the 0.5.0 CLI. |
| Federation | 0.3.x node handshake | fenced, deadline/keepalive-driven node links (M5-2…M5-4) | All nodes must run 0.5.0; re-`join` secondaries after upgrading. No rolling mixed-version federation. |
| Config | `ring_buffer_bytes`, `max_output_log_bytes` | keys removed | Delete them from `config.json`. Unknown keys are **silently ignored** (the config parser is permissive by design), so a stale key will not error — it just does nothing. |
| API keys | one all-powerful daemon key | scoped keys (M5-4) | Old keys stop working; create new scoped keys with `oly api-key`. |
| Web UI auth | shared token | per-principal sessions, origin checks (M5-4) | Everyone logs in again after upgrade. |
| Input handling | timing heuristics for paste/keys | event-driven; paste boundaries come from bracketed-paste markers only (ADR-0003) | No action; apps that enable bracketed paste get exact paste boundaries. |
| Attach control | first attach holds control; later attaches observe | attaching (CLI or browser) **takes control by default** and resizes the session to the new controller's viewport; the previous controller is demoted to observer | Use `oly attach --observer` for view-only attaches. Scripts that relied on attach-as-observer should add `--observer`. |
| Detach keys | `Ctrl-]` then `d` (broken in some terminals) | `Ctrl-]` then `d` works in all supported terminals (plain `d` or Ctrl-held `d`) | No action. |
| Daemon status | reported the client's config values | reports the running daemon's effective flags and HTTP endpoint (from the daemon's own record) | No action; `oly daemon start` against a running daemon now prints the running config and the remedy. |

Everything else about the CLI is **additive**: new subcommands
(`doctor`, `observe`, `notify`, `restart`, `status`, `skill`, `update`;
the `history`/`screen`/`wait` read surfaces are modes of `oly logs` —
`--from`, `--screen`, and `--after`/`--exit`/`--idle-ms`/`--pattern`
respectively) and new flags. All 0.x commands
(`start`, `stop`, `list`, `logs`, `attach`, `send`, `node`, `join`,
`api-key`, `daemon`) still exist with the same names.

## Upgrade procedure

### 1. Preflight

```sh
oly list            # note running sessions — live PTYs cannot be migrated
oly daemon status   # confirm which daemon version is running
df -h ~/.local/state/oly   # ensure free space for the backup
```

Finish or stop every session you still need interactively. A running 0.3.x
session belongs to the 0.3.x daemon; 0.5.0 cannot adopt it.

### 2. Back up

```sh
# State directory: $XDG_STATE_HOME/oly or ~/.local/state/oly on Linux,
# ~/Library/Application Support/oly on macOS, %LOCALAPPDATA%\oly on
# Windows. OLY_STATE_DIR overrides it (tests use this).
cp -a ~/.local/state/oly ~/.local/state/oly-0.x-backup
```

The backup contains your metadata and every session's `output.log` /
`events.log`. Keep it somewhere the 0.5.0 daemon does not write.

### 3. Stop the old daemon

```sh
oly daemon stop
```

Do not start the 0.5.0 daemon while a 0.3.x daemon is running on the same
state directory.

### 4. Install 0.5.0 and start

```sh
cargo install --path .        # or: npm install -g oly@beta
oly daemon start
oly daemon status             # should report the 0.5.0 daemon
```

The SQLite metadata database migrates itself forward automatically (schema
migrations are additive and applied on open). Session *recordings* are not
migrated — see below.

### 5. Verify

```sh
oly list                    # 0.3.x sessions appear; logs/attach report the
                            # pre-0.5 format explicitly, never garbage
oly start -- echo hello     # create a fresh 0.5.0 session
oly logs <id>               # renders from the new journal
oly doctor <id>             # journal health check
```

## Preserving old recordings

There is **no automatic import**, on purpose. A faithful 0.5.0 journal needs
ordered raw bytes, resize offsets, and checkpoints; reconstructing those from
`output.log`/`events.log` would fabricate provenance we cannot guarantee
(ambiguous resize offsets, unknown timing, truncated tails). The project rule
is: never present a guess as a faithful recording (PLAN §14.4).

Your old bytes are still useful:

- `output.log` contains the filtered display stream (what the terminal
  showed; device query/response and other control exchanges were filtered
  out before persisting). Replaying it with `cat output.log` in a wide
  terminal, or `tmux new-session 'cat output.log; read'`, is a reasonable
  approximation of the session — but beware the raw byte stream only ever
  existed in the live PTY and is not recoverable from these files.
- `events.log` lines are newline-delimited JSON — greppable for lifecycle
  and resize history.
- Old sessions remain listed in 0.5.0 with size 0; asking for their logs or
  attaching produces an explicit `uses the pre-0.5 log format (output.log) …
  see MIGRATION.md` error rather than empty output.

## Rollback

1. `oly daemon stop` (the 0.5.0 daemon).
2. Move the 0.5.0 state aside: `mv ~/.local/state/oly ~/.local/state/oly-0.5`.
3. Restore: `cp -a ~/.local/state/oly-0.x-backup ~/.local/state/oly`.
4. Reinstall the 0.3.x binary and start it.

A 0.3.x binary will never be pointed at 0.5.0 state, and 0.5.0 journals are
not promised to be readable by any 0.3.x build.

## Notes for tool authors

- IPC: requests and one-shot responses are newline-delimited JSON envelopes
  (`{"version":13,"payload":…}`). Attach streams send one JSON init line,
  then binary frames: `[u32 LE payload_len][u8 tag][payload]` where tag 1 =
  output (`[u64 LE offset][raw bytes]`) and tag 2 = control (bare JSON
  `RpcResponse`). Frame lengths are capped and rejected from the header
  before allocation.
- Browser WebSocket attach was already binary in late 0.x and keeps the same
  frame layout (`web/src/api/ws-frames.ts`, pinned by
  `tests/fixtures/ws_frames.json`).
- If your tool only needs terminal output, prefer piping `oly attach <id>`
  or `oly logs --raw <id>` over speaking IPC yourself.
