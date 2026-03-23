---
name: oly
description: "Use when starting a long-running or interactive CLI command with oly, especially when it may need later input, should be detachable, or should keep durable logs for supervision and resume."
---

Use `oly` when a command should keep running outside the current terminal, may need later input, or should produce durable logs that another human or agent can inspect.

## When to use this skill

Prefer `oly start` instead of launching the command directly when:

- the command may ask for input later
- the session should survive terminal closes, disconnects, or agent handoff
- logs, replay, or auditability matter
- you want to supervise the task without attaching immediately
- another human or agent may need to resume or inspect the work later

Do **not** reach for `oly` by default for short, non-interactive commands where a normal terminal invocation is simpler.

## Agent operating principles

- Think like a supervisor, not a shell typist. Start the job, monitor it, and intervene only when needed.
- Prefer short log tails over full transcripts. Start with `--tail 40` and only ask for more when necessary.
- Prefer longer waits with fewer polls. Choose a timeout that matches the expected next checkpoint to reduce churn and token use.
- Use `oly ls --json` when you need machine-readable status for scripting or structured decisions.

## Recommended workflow

### 1) Start the session

Start the command detached so you get the session ID immediately:

```bash
oly start --title "task 1" --cwd /path/to/working/directory --detach the_cmd --cmd-arg1 --cmd-arg2
```

Useful options:

- `--detach` returns immediately with the session ID.
- `--disable-notifications` is useful when you plan to supervise the session yourself.
- `--node <name>` runs the command on a connected secondary node.

### 2) Monitor progress

Use `oly logs` to inspect the latest output or wait for an interactive checkpoint:

```bash
oly logs <ID> --tail 40 --no-truncate --wait-for-prompt --timeout 10s
```

Guidance:

- `--wait-for-prompt` blocks until the session likely needs input or the timeout expires.
- `--timeout` accepts plain milliseconds or units such as `250ms`, `10s`, `5m`, and `1h`. The default is `30s`.
- `oly logs` can omit the ID and target the most recently created session.
- If the task should finish soon, reduce `--timeout` so you do not wait longer than necessary.
- If the task is expected to run for a while, increase `--timeout` to avoid unnecessary polling.
- Start with a small tail and only expand when the recent context is insufficient.

### 3) Send input without attaching

Send text or key events directly into the session:

```bash
oly send <ID> "hello world!" key:enter
```

Rules:

- Arguments are sent left to right.
- Plain arguments send literal text.
- Special keys use `key:...`, for example `key:enter`, `key:ctrl+c`, `key:alt+x`, `key:shift+tab`.
- Raw hex bytes use `key:hex:...`.
- `oly send` can omit the ID and target the most recently created session.
- Piped stdin is supported when you do not pass positional chunks.

Input strategy:

- If the program is showing menu choices or a TUI selection, prefer navigation keys such as `key:up`, `key:down`, and `key:enter` instead of sending plain text.
- If the program is prompting for freeform text, send the instruction as text and include `key:enter` when appropriate.
- If a command is stuck or needs to be interrupted, send the relevant control key sequence such as `key:ctrl+c`.

### 4) Attach, list, or stop

Use these commands when you need direct control or lifecycle management:

```bash
oly attach <ID>
oly stop <ID>
oly ls
```

Notes:

- Detach from an attached session with `Ctrl-]`, then `d`.
- `oly ls --json` is preferred for scripts and agents.

## Help

Use the built-in help when you need command details:

```bash
oly --help
oly notify --help
oly <command> --help
```

## Presenting rich output with HTML

When you want to show rich output to the user, publish it as either:

- a single HTML page
- a single-page application bundle

Place each app in its own child folder under `<OLY-ROOT>/wwwroot/apps`, and make sure that folder contains an `index.html` entry point.

For SPAs, use relative asset paths and set the base href to `./` so the app works correctly when served from its discovered route.

Users can then open the app at:

```text
http://127.0.0.1:<OLY-PORT>/apps/<your-app-name>/
```

- `<OLY-PORT>` is the `oly` HTTP port, `15443` by default.
- `<OLY-ROOT>` is the state directory root:
  - `%LOCALAPPDATA%\oly` on Windows
  - `$XDG_STATE_HOME/oly` or `~/.local/state/oly` on Linux
  - `~/Library/Application Support/oly` on macOS
