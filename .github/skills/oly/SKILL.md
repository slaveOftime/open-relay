---
name: oly
description: "Use when starting a long-running or interactive CLI command with oly, especially when it may need later input, should be detachable, or should keep durable logs for supervision and resume."
---

Use `oly start` instead of a normal terminal launch when:

- the command may ask for input later
- the session should survive terminal closes or disconnects
- logs, auditability, or replay matter
- you want to inspect or intervene without attaching
- the task may be supervised by a human or another agent

## Recommended workflow

Start a session detached:

```bash
oly start --title "task 1" --cwd /path/to/working/directory --detach the_cmd --cmd-arg1 --cmd-arg2
```

- `--detach` returns the session ID immediately.
- `--disable-notifications` is useful when you want to supervise it by yourself.
- `--node <name>` runs the command on a connected secondary node.

Check progress or wait for a likely interactive checkpoint:

```bash
oly logs <ID> --tail 40 --no-truncate --wait-for-prompt --timeout 10s
```

- `--wait-for-prompt` blocks until the session likely needs input or the timeout expires.
- `--timeout` accepts plain milliseconds or units like `250ms`, `10s`, `5m`, and `1h`.
- `0` waits forever. Default value is `30s`.
- `oly logs` can omit the ID and will target the most recently created session.

Send input without attaching:

```bash
oly send <ID> "hello world!" key:enter
```

- Arguments are sent left to right.
- Plain arguments send literal text.
- Special keys use `key:...`, for example `key:enter`, `key:ctrl+c`, `key:alt+x`, `key:shift+tab`.
- Raw hex bytes use `key:hex:...`.
- `oly send` can omit the ID and will target the most recently created session.
- Piped stdin is supported when you do not pass positional chunks.

Reattach or stop when needed:

```bash
oly attach <ID>
oly stop <ID>
oly ls
```

- `oly attach`, `oly stop`, `oly notify enable`, and `oly notify disable` also accept an omitted ID and then use the most recently created session.
- Detach from an attached session with `Ctrl-]`, then `d`.
- `oly ls --json` prints machine-readable output for scripts and agents.

Help:

```bash
oly --help
oly notify --help
```

Use `oly <command> --help` for command-specific details.

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
