---
name: oly-start
description: Start an interactive CLI task with oly so the session can be supervised, audited, and resumed without tying up the main terminal.
---

Use this skill when you want to run a CLI that may block, prompt for input, or require later supervision.

Prefer `oly start` over launching the command directly in a terminal when:

- the process may need interactive input later
- you want a durable session with logs and auditability
- you want to detach immediately and come back only if the task needs attention

## Start a supervised session

```bash
oly start --title "task 1" --cwd /path/to/working/directory --detach --disable-notifications the_command --arg1 --arg2
```

Notes:

- `--detach` returns immediately after the session starts.
- `--disable-notifications` is useful when you want to supervise the task yourself first and only escalate to the user when necessary.
- `oly start` returns a session ID. Use that ID for later `logs`, `input`, `attach`, or `stop` commands.

## Wait for a prompt or inspect progress

```bash
oly logs <id> --tail 40 --no-truncate --wait-for-prompt --timeout 600
```

Notes:

- `--wait-for-prompt` blocks until the session reports that it needs input, or until the timeout expires.
- `--timeout 600` means 600 seconds. Increase it when the command may take longer before prompting.
- Remove `--wait-for-prompt` when you only want to inspect recent output without waiting.

## Send input

Send text followed by enter:

```bash
oly send <id> "yes" key:enter
```

Send special keys:

```bash
oly send <id> key:shift+tab
oly send <id> key:ctrl+c
```

Key syntax notes:

- Prefix special keys with `key:`, e.g. `key:enter`, `key:ctrl+c`, `key:alt+x`.
- Plain arguments are sent as literal text.
- Chunks are processed left to right, so `oly send <id> "hello" key:enter` types "hello" then presses enter.
- Supported named keys: `enter`, `return`, `cr`, `lf`, `linefeed`, `tab`, `backspace`, `bs`, `esc`, `escape`, `up`, `down`, `left`, `right`, `home`, `end`, `pageup`, `pgup`, `pagedown`, `pgdn`, `delete`, `del`, `insert`, `ins`.
- Supported modifier forms: `ctrl+<char>`, `alt+<char|named-key>`, `meta+<char|named-key>`, `shift+tab`.
- Raw hex bytes: `key:hex:1b` or `key:hex:1b5b41`.

After sending input, you can wait again:

```bash
oly logs <id> --tail 40 --no-truncate --wait-for-prompt --timeout 600
```

## Stop or inspect sessions

Stop a session when the task is complete or should be terminated:

```bash
oly stop <id>
```

List recent sessions:

```bash
oly ls
```

Typical output columns:

```text
ID STATUS CMD AGE PID CREATE_AT↓ TITLE ARGS
```

Useful `oly ls` filters:

```text
--search <SEARCH>   Filter by title or ID substring (case-insensitive)
--status            Only show specific statuses; repeatable
--since <RFC3339>   Created at or after the given timestamp
--until <RFC3339>   Created at or before the given timestamp
--limit <LIMIT>     Maximum number of sessions to return
```

## Recommended workflow

1. Start the command with `oly start --detach`.
2. Watch with `oly logs <id> --no-truncate  --wait-for-prompt`.
3. Decide whether to answer the prompt yourself or escalate to the user.
4. Send input with `oly send <id> ...` if the context is clear.
5. Repeat until the task finishes, then stop or leave the session running as appropriate.
