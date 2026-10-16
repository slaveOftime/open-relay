# CLI guide

[Documentation](../README.md) · [Remote access](./remote-access.md) · [Daemon configuration](../../skills/oly-daemon/SKILL.md)

This guide covers the 0.5 CLI. Run `oly <command> --help` for flag details.
Most runtime operations require a running daemon; local `oly ls` falls back to
persisted SQLite metadata when the daemon is unreachable.

## Start, inspect, and finish

```sh
oly daemon status
oly daemon start --detach                 # only if confirmed stopped
oly start --detach --title "test run" --tag dev --cwd /repo copilot
oly ls
oly ls --json --status running
oly info <id>
oly logs <id> --tail 40
oly stop <id> --grace 5
oly restart <id>                           # fresh ID and journal; keeps source history
oly rm <id>                                # stopped session and files: permanent deletion
```

Place `oly` flags before the child executable; everything after it is passed to
the child. `--cwd` resolves relative to the caller locally (use an explicit path
valid on the target node for `--node`).

`info`, `attach`, `logs`, `export`, `send`, `stop`, `rm`, and `notify` toggles
default to the most recently created session when `<id>` is omitted. Always pass
explicit IDs in scripts, concurrent workflows, and destructive commands.
`restart` and `update` require an `<id>`. `restart --force` and `rm --force`
kill a running session before proceeding.

```sh
oly update <id> --title "new title" --tag dev --tag release
oly update <id> --title ""                  # clear title
oly update <id> --tag ""                    # clear all tags
oly update <id> --notifications disabled
```

Omitted `update` fields remain unchanged; repeated `--tag` flags replace the tag list.

### Graceful stop

`oly stop` defaults to `--grace 5` seconds:
1. Queues `Ctrl-C` immediately, a second `Ctrl-C` at `min(500ms, grace/4)`, and `Ctrl-D` at `min(1s, grace/2)`.
2. Sends process-group `SIGTERM` on Unix at `3*grace/4`.
3. Forces process-tree termination at the deadline (`--grace 0` kills immediately).

Because `Ctrl-C` and `Ctrl-D` are application input bytes, an unresponsive app or
full input queue may still require forced termination. Check final logs and
`exit_code` via `oly info <id> --json`. Configured `resume_command` suggestions
appear only after child exit, PTY EOF, journal durability, and the completed-output scan.

## Live dashboard and search

```sh
oly ls --follow
oly ls --search 'cmd:copilot cwd:open-relay'
oly ls --tag release --status running
oly ls --follow --node worker-a --node worker-b --node-local
oly ls --follow --node-all
```

| List view (`Ctrl+G` switches) | Tree view |
|---|---|
| [![The list view: sessions grouped by node, with live rate, output, and command columns](../../assets/oly-list-tui.png)](../../assets/oly-list-tui.png) | [![The tree view: the same sessions grouped by working directory](../../assets/oly-list-tui-tree.png)](../../assets/oly-list-tui-tree.png) |

Search terms are whitespace-split and ANDed:
- **Bare terms** match `id`, `title`, `tags`, `command`, `args`, or `cwd`.
- **Scoped terms** use `id:`, `title:`, `tag:`/`tags:`, `cmd:`/`command:` (matches command or args), `args:`, or `cwd:`. Unknown prefixes and Windows drive paths (`C:\...`) are treated as bare terms.
- `oly ls` defaults to `--limit 10` (`100` with `--follow`). `--node-all` requires `--follow` and cannot combine with `--json`.

| Dashboard key | Action |
|---|---|
| `Enter` | Attach inline (or expand/collapse a folder in tree view) |
| `Ctrl+Enter` | Attach in another terminal window |
| `Ctrl+G` | Switch compact table / working-directory tree |
| `Ctrl+N` / `Ctrl+D` | New Session / Duplicate dialog (**Attach on start** checked by default; `Space` toggles) |
| `Ctrl+U` | Edit title, tags, and notifications |
| `Ctrl+K` | Stop the selected running session |
| `Ctrl+C` | Exit dashboard |

## Full session inspection

```sh
oly info <id>
oly info <id> --json
oly info <id> --color always
oly info <id> --node worker-1
```

`oly info` displays session metadata, process status, PID, `exit_code`,
`resume_command`, timestamps, byte totals, terminal geometry/modes, `attach_count`,
`session_dir`, and `journal_incarnation`. Stopped or evicted sessions remain
inspectable from persisted metadata (`—` indicates unavailable live-only fields).

### Agent-reported status (OSC 7501)

Programs supporting the [Program Status Protocol](https://www.superlogical.com/rex/docs/build/program-status)
can emit `idle`, `working`, `blocked`, `done`, or `error`:

- **Visibility:** Shown in `oly info`, `program_status` in `oly info --json` / `oly ls --json`, compact `[state]` labels in `oly ls --follow`, and status badges in the web UI.
- **Attention & waits:** While a session is running, `idle`, `done`, `blocked`, and `error` set `input_needed` immediately (waking `oly logs --wait prompt` without waiting for silence/regex heuristics), while `working` clears `input_needed`. Stopped processes never need input.
- **Precedence & lifetime:** Up to 256 hierarchical records are retained in memory per live session, prioritized as `blocked > error > working > done > idle` (newest wins ties). Shell prompt markers (`OSC 133 A`) and PTY EOF clear `working`/`blocked`; full terminal reset (`RIS`) clears all records.
- **Trust boundary:** Status reports are untrusted child hints, not proof of task completion or approval authority.
- **Windows + `pi`:** ConPTY can answer device-attributes queries before `oly`'s OSC 7501 reply. `oly` defaults `PI_PROGRAM_STATUS=1` in Windows child environments when unset (preserving any explicit value).

## Attach and scroll

```sh
oly attach <id>              # controller by default; Ctrl-D detaches
oly attach <id> --observe    # view-only; never sends input or resizes
```

Multiple controllers can attach concurrently; the last controller resize sets
terminal geometry. `oly send` is an operator command and is not gated by attach role.

- **Main-screen programs:** Scroll with your outer terminal's scrollbar or keys. Reattach seeds up to 1000 available main-buffer history rows.
- **Fullscreen / alternate-screen programs:** Scroll with the application's keys (`PgUp`/`PgDn`) or mouse wheel.
- **Historical output:** Use `oly logs <id> --tail <n>`.

## Send text, keys, files, or mouse input

```sh
oly send <id> "continue" key:enter
oly send <id> key:ctrl+c
oly send <id> key:up key:enter
oly send <id> oly-clipboard
oly send <id> "cat " oly-file:/path/to/file key:enter
```

Chunks are sent left-to-right with no implicit spaces or Enter:
- **Named keys (`key:`):** `enter`, `tab`, `esc`, `backspace`, `up`, `down`, `left`, `right`, `home`, `end`, `pgup`, `pgdn`, `del`, `ins`, `ctrl+<char>`, `alt+<char|key>`, `meta+enter`, `shift+tab`, `hex:<bytes>`.
- **Clipboard / file upload:** `oly-clipboard` and `oly-file:<path>` upload content to the owning daemon and type the saved path. Piped stdin is accepted when no positional chunks are given.
- **Free-text mode (`--`):** Everything after `--` is joined with single spaces and sent as one literal string (disables `key:`, `mouse:`, `oly-file:`, and `oly-clipboard` parsing). It does **not** press Enter — send `oly send <id> key:enter` in a separate call:
  ```sh
  oly send <id> -- 'echo "press key:enter to continue"'
  oly send <id> key:enter
  ```
- **Application mouse input (`mouse:`):** Requires application mouse reporting.
  ```sh
  oly send <id> mouse:wheel+3       # 3 wheel-up events at viewport center
  oly send <id> mouse:wheel-3       # 3 wheel-down events
  oly send <id> mouse:left@2,3      # click col 2, row 3 (1-based)
  oly send <id> mouse:left+2@2,3    # double click
  ```
  Click: `mouse:<left|middle|right>[+N]@col,row` (position required). Wheel: `mouse:wheel[+N|-N][@col,row]` (defaults to viewport center). Counts are `1..1000` events. Verify redraws with `oly logs <id> --screen`.

## Logs: views, waits, and byte continuation

Default `oly logs <id>` prints the **last 40 rendered lines**. Use `--screen` to inspect a live or final TUI grid. `--wait` gates when the read happens without changing what it prints.

| Axis | Options |
|---|---|
| View (at most one) | `--tail <n>` (`0..65535`, default `40`), `--screen`, `--tail-frames <n>` (`0..1024`) |
| Gate | `--wait output` (alias `new`), `prompt`, `idle`, `exit`, or `match` |
| Byte continuation | `--since <cursor>` (no view flag); `--limit-bytes <n>` (default 256 KiB, max 8 MiB) |
| Cursor only | `--cursor` (prints current end cursor token and exits) |
| Format / routing | `--json`, `--color auto\|always\|never`, `--node <name>` |

```sh
oly logs <id> --tail 80 --color always
oly logs <id> --screen
oly logs <id> --tail-frames 3
oly logs <id> --wait prompt --screen --timeout 5m
oly logs <id> --wait exit --tail 40 --timeout 5m
oly logs <id> --wait idle --idle-for 2s --timeout 1m
oly logs <id> --wait match --match 'DONE|FAILED' --timeout 5m
```

- **Wait exit codes:** `0` condition met, `2` timeout, `1` error. `--wait exit` exits `0` when the child terminates even if its `exit_code` is nonzero.
- **Timeouts:** `--timeout` defaults to `30s` (`ms`, `s`, `m`, `h`, or `0` to wait forever). Timeout prints no text stdout (`--json` emits `outcome: "timeout"` and `cursor: null`).
- **Wait semantics:**
  - `output` and `prompt` also complete if the session exits.
  - `idle` and `prompt` are hints, not proof of task completion.
  - `match` scans logical lines in canonical bytes produced after the start cursor (or invocation time). No cross-line regexes; unmatched lines >1 MiB or session exit without a match fail explicitly.

### Exact incremental bytes

Rendered views are lossy snapshots. To stream exact canonical display bytes across calls, capture `--cursor` and chain `--since`:

```bash
id=<ID>
cursor=$(oly logs "$id" --cursor) || exit 1
while :; do
  page=$(oly logs "$id" --since "$cursor" --wait output --timeout 60s --json)
  rc=$?
  [ "$rc" -eq 2 ] && continue       # preserve cursor on timeout
  [ "$rc" -eq 0 ] || exit "$rc"
  printf '%s' "$page" | jq -r .bytes | base64 --decode
  cursor=$(printf '%s' "$page" | jq -r .cursor)
  [ "$(printf '%s' "$page" | jq -r .running)" = false ] &&
    [ "$(printf '%s' "$page" | jq -r .has_more)" = false ] && break
done
```

JSON stream pages return `mode: "stream"`, base64 `bytes`, `start_cursor`, next `cursor`, `has_more`, `running` (true until PTY EOF), and `exit_code`. Drain while `has_more`. Pages may split UTF-8 or escape sequences across reads. Cursors are opaque and fenced by session ID and journal incarnation.

### Rendering and limits

- `--tail-frames <n>` returns distinct sampled screen states after recorded output events (oldest first, including blank screens; `A -> B -> A` keeps all three).
- Rendering fails explicitly rather than truncating silently if a request exceeds 64 MiB of frame payload, 1M viewport cells, or 8M tail/history cells — request fewer lines/frames or use `--since` / `export`.

## Export

```sh
oly export <id> > capture.bin
oly export <id> --json            # streamed base64 bytes and size
```

`export` streams a fixed prefix of canonical **filtered display bytes** (scanner-answered terminal queries stripped) captured at invocation time. Use `--since` for live incremental continuation.
