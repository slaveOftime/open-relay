---
name: oly
description: "Use when starting a long-running or interactive CLI command with oly, especially when the task may need later input, should run detached, or should keep durable logs for supervision and resume."
---

Use `oly start` instead of a normal terminal launch when:

- the command may ask for input later
- the session should be detachable and resumable
- logs or auditability matter
- you do not want to block the main terminal

## Core workflow

Start detached:

```bash
oly start --title "task 1" --cwd /path/to/working/directory --detach --disable-notifications the_cmd --cmd-arg1 --cmd-arg2
```

- `--detach` returns the session ID immediately.
- `--disable-notifications` is useful when you want to supervise it yourself first.

Wait for a prompt or check progress:

```bash
oly logs <ID> --tail 40 --no-truncate --wait-for-prompt --timeout 600
```

- `--wait-for-prompt` blocks until input is needed or the timeout expires.
- `--timeout` in milliseconds.

> If the target command is supposed to be very fast for starting or reacting to send command, then there is no need to use those flags. 

Send input:

```bash
oly send <ID> hello world! key:enter
```

- Arguments are sent left to right.
- Plain arguments send literal text.
- Special keys use `key:...`, for example `key:enter`, `key:ctrl+c`, `key:alt+x`.
- Raw hex bytes use `key:hex:...`.

Stop or list sessions:

```bash
oly stop <ID>
oly ls
```

Help:

```bash
oly --help
oly ls --help
```

Use `oly <command> --help` for command-specific details.
