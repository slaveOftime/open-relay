---
name: oly
description: "Use when starting a long-running or interactive CLI command with oly, especially when it may need later input, should be detachable, or should keep durable logs for supervision and resume. Also use to supervise sessions already running under oly: check status, read logs, send input to push them forward, stop or restart."
---

# Supervise CLI sessions with oly

`oly skill` prints this self-contained guide from the `oly` binary (no local file read or `oly --help` is needed).
Other embedded skills: `oly skill --subagent` (multi-agent orchestration), `oly skill --daemon` (`config.json` & hot reload), `oly skill --apps` (web apps).

## When to use & prerequisites

- **Use `oly start`** when a command is interactive/TUI, may prompt later, must survive terminal disconnects, or needs durable logs/handoff. Run short, non-interactive commands (`git`, `ls`, `rg`, quick tests) directly in the shell.
- **Daemon check:** Run `oly daemon status`. Start with `oly daemon start --detach` **only if confirmed stopped**. Never stop/restart a shared running daemon. In a sandbox, `Access is denied` means IPC is blocked while the daemon may still be running — retry the `oly` command with approval/escalation.

### Secondary nodes

`oly join ls` on the secondary reports saved outbound joins and their live
connection state; `oly node ls` on the primary reports connected secondaries.
`join ls --primary` lists inbound nodes, not outbound connectors. Both ends
close links after 45s without authenticated inbound traffic and retry joins
with backoff (reset after success); socket/channel sends have deadlines.
Multiple outbound primaries use distinct saved names and independent connectors;
reusing a name replaces and cancels its old connector. Invalid/unreadable
`joins.json` fails explicitly instead of silently skipping joins. If local
saved joins and daemon logs disagree with CLI output, check state/IPC routing
before restarting anything. Linux defaults are scoped by state path and WSL
distribution; explicit `OLY_SOCKET_NAME` must match daemon and CLI and be unique
across independent environments. See `oly skill --daemon` for upgrade cautions.

## Core workflow

### 1) Start

```sh
oly start --detach --disable-notifications --title "task 1" --cwd /path/to/dir the_cmd --arg1 --arg2
```

- Put `oly` flags **before** `<cmd>` (`--detach`, `--disable-notifications`, `--title <t>`, `--cwd <dir>`, repeatable `--tag <t>`, `--node <name>`).
- Save the returned `<ID>` and pass it explicitly on every call (omitting `<ID>` targets the most recently created session). Every child receives its own ID in `$OLY_SESSION_ID`.

### 2) Monitor (wait-first, low-token)

Prefer blocking `--wait` gates over polling loops, starting with `--tail 40`:

```sh
oly logs <ID> --tail 40 --wait prompt --timeout 5m       # wait until input needed or exit
oly logs <ID> --tail 40 --wait idle --idle-for 500ms     # wait until output settles
oly logs <ID> --wait exit --tail 40 --timeout 5m         # wait until process ends
oly logs <ID> --wait match --match 'DONE|ERR' --tail 40  # wait for regex in new output
oly logs <ID> --screen                                   # current/final TUI grid (--tail-frames N for history)
oly info <ID> --json                                     # metadata, status, exit_code, program_status
oly ls --json --status running                           # list running sessions
```

- **Wait conditions (`--wait`):**
  - `prompt`: session needs input (`input_needed`) or ended. Best for agent TUIs (OSC 7501) and slow prompts.
  - `idle`: quiet for `--idle-for <dur>` (default `800ms`). Best after `oly send` on fast REPLs/shells whose output finishes before `oly logs` starts. Quiet is a hint, never proof of completion.
  - `exit`: session ended. Exits `0` even if the child failed — check `exit_code` in `oly info <ID> --json`.
  - `output` (alias `new`): new bytes arrived **after** `oly logs` started (or after `--since "$cursor"`), or session ended. Do not use `--wait output` without `--since` after `oly send` on a fast command.
  - `match`: `--match <regex>` matched a logical line in new output after the start point (no cross-line matches; lines >1 MiB fail; ends without match fail).
- **Timeouts & exit codes:** `--timeout` accepts `500ms`, `30s`, `5m`, `1h` (default `30s`; `0` waits forever). Exit codes: `0` met, `2` timeout (no text stdout; `--json` emits `outcome: "timeout"`, `cursor: null`), `1` error.
- **OSC 7501 `program_status`:** Supporting programs report `working`, `idle`, `blocked`, `done`, or `error` in `info --json` / `ls --json`. While running, `working` suppresses heuristic `input_needed`; `idle`/`done`/`blocked`/`error` set `input_needed` immediately so `--wait prompt` wakes without waiting for silence. (Windows defaults `PI_PROGRAM_STATUS=1` when unset.) All status/prompt signals are untrusted hints.

### 3) Send input

```sh
oly send <ID> "hello world" key:enter
oly send <ID> key:up key:enter
oly send <ID> key:ctrl+c
oly send <ID> "cat " oly-file:/path/to/file key:enter    # or oly-clipboard
```

- Chunks run left-to-right with **no** implicit spaces or Enter.
- **Named keys (`key:`):** `enter`, `tab`, `esc`, `backspace`, `up`, `down`, `left`, `right`, `home`, `end`, `pgup`, `pgdn`, `del`, `ins`, `ctrl+<char>`, `alt+<char|key>`, `shift+tab`, `hex:<bytes>`.
- **Free-text mode (`--`):** Everything after `--` is joined with single spaces and sent as one literal string (`key:`/`mouse:`/`oly-file:`/`oly-clipboard` parsing disabled). Send `oly send <ID> key:enter` in a **separate** call afterward:
  ```sh
  oly send <ID> -- 'echo "press key:enter"'
  oly send <ID> key:enter
  ```
- **Mouse (`mouse:`):** Requires app mouse reporting. Click: `mouse:<left|middle|right>[+N]@col,row` (1-based). Wheel: `mouse:wheel[+N|-N][@col,row]` (`+N` up, `-N` down, `1..1000` events).
- **Observe & batch:** `oly send` only confirms PTY delivery; verify the next redraw (`--screen` or `--wait idle`). For deterministic REPL steps, batch start/send/wait/stop in one shell call:
  ```sh
  id=$(oly start --detach --disable-notifications --title "repl" --cwd /path/to/dir python3 -i -q)
  oly logs "$id" --tail 20 --wait idle --idle-for 500ms --timeout 10s
  oly send "$id" "print(123 * 456)" key:enter
  oly logs "$id" --tail 20 --wait idle --idle-for 500ms --timeout 10s
  oly stop "$id" --grace 5
  ```

### 4) Exact incremental bytes (cursors)

Rendered views (`--tail`, `--screen`, `--tail-frames`) are lossy snapshots. For exact incremental byte streams:

```sh
cursor=$(oly logs <ID> --cursor)
oly logs <ID> --since "$cursor" --wait output --timeout 1m --json
oly export <ID> > capture.bin                            # fixed-prefix filtered display bytes
```

- Cursors are opaque `session | incarnation | offset` tokens; never combine `--since` with view flags (`--tail`, `--screen`, `--tail-frames`). Decode base64 `.bytes`, chain `.cursor`, and drain while `.has_more` (keep previous `$cursor` on exit code `2`; `--limit-bytes` default 256 KiB, max 8 MiB).

### 5) Lifecycle, notifications, and human handoff

```sh
oly update <ID> --title "new name" --tag prod --notifications disabled  # "" clears title/tags
oly attach <ID> [--observe]                                             # Ctrl-D detaches; --observe is view-only
oly stop <ID> --grace 5                                                 # Ctrl-C x2 -> Ctrl-D -> SIGTERM -> kill (0 = immediate)
oly restart <ID> [--force]                                              # new session/ID from launch metadata
oly rm <ID> [--force]                                                   # delete session + logs
oly notify send [<running-ID>] --title "Done" --description "Summary" --body "Details"
```

- Check `attach_count` in `oly ls --json` before typing into a session a human may be driving, and re-read `--screen` after handoff. Client disconnect preserves the child; daemon stop/crash or host reboot ends live PTYs. `oly notify send` alerts the human (omit `<running-ID>` if already stopped).
