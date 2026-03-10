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

Send normal text:

```bash
oly input <id> --text "yes" --key enter
```

Send multiple keys:

```bash
oly input <id> --key shift --key tab
oly input <id> --key ctrl+c
```

Key syntax notes:

- `-k` is the short form of `--key`.
- You can split modifier sequences across repeated flags, for example `--key ctrl --key c`.
- Supported named keys are: `enter`, `return`, `cr`, `lf`, `linefeed`, `tab`, `backspace`, `bs`, `esc`, `escape`, `up`, `down`, `left`, `right`, `home`, `end`, `pageup`, `pgup`, `pagedown`, `pgdn`, `delete`, `del`, `insert`, `ins`.
- Supported modifier forms include: `ctrl+<char>`, `alt+<char|named-key>`, `meta+<char|named-key>`, `shift+<char|tab>`, `capslock+<letter>`.
- Hex byte input is also supported, for example `0x1b` or `\x1b`.

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
4. Send input with `oly input <id> ...` if the context is clear.
5. Repeat until the task finishes, then stop or leave the session running as appropriate.
