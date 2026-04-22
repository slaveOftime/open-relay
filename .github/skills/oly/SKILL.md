---
name: oly
description: "Use when starting a long-running or interactive CLI command with oly, especially when it may need later input, should be detachable, or should keep durable logs for supervision and resume."
---

## When to use

Use `oly start` instead of a direct terminal invocation when ANY of these apply:

- The command may prompt for input later.
- The session must survive terminal closes, disconnects, or agent handoff.
- Logs, replay, or auditability matter.
- Another human or agent may need to resume or inspect the work.

Do **not** use `oly` for short, non-interactive commands — a normal terminal is simpler.

## Principles

- **Supervisor mindset.** Start the job, monitor it, intervene only when needed.
- **Small tails first.** Use `--tail 40`; expand only when context is insufficient.
- **Fewer polls, longer waits.** Set `--timeout` to match the expected next checkpoint — reduces churn and token cost.
- **Machine-readable listing.** Use `oly ls --json --status running` for scripting or structured decisions.

## Workflow

### 1) Start

```bash
oly start --title "task 1" --cwd /path/to/dir --detach the_cmd --arg1 --arg2
```

| Flag | Purpose |
|---|---|
| `--detach` | Return immediately with the session ID. |
| `--disable-notifications` | Suppress notifications when you will supervise yourself. |
| `--node <name>` | Run on a connected secondary node. |

### 2) Monitor

```bash
oly logs <ID> --tail 40 --no-truncate --wait-for-prompt --timeout 10s
```

- `--wait-for-prompt` — blocks until the session likely needs input or timeout expires.
- `--timeout` — accepts `250ms`, `10s`, `5m`, `1h` (default `5m`). Shorten for fast tasks; lengthen for slow ones.
- Start with `--tail 40`; increase only when recent context is insufficient.

### 3) Send input

```bash
oly send <ID> "hello world!" key:enter
oly send <ID> oly-clipboard
oly send <ID> "cat " oly-file:/path/to/file key:enter
```

- Arguments are sent left-to-right. Plain text is literal; special keys use `key:` prefix.
- Keys: `key:enter`, `key:ctrl+c`, `key:alt+x`, `key:shift+tab`, `key:up`, `key:down`.
- Raw hex: `key:hex:...`. Piped stdin is supported when no positional chunks are given.
- Clipboard content uses `oly-clipboard`. Local file upload uses `oly-file:<path>`. This is normally used for remote node sessions.

**Input strategy:**

| Scenario | Action |
|---|---|
| Menu / TUI selection | Navigate with `key:up` / `key:down` / `key:enter`. |
| Freeform text prompt | Send text + `key:enter`. |
| Stuck / needs interrupt | `key:ctrl+c` (or relevant control sequence like `key:esc`). |

### 4) Lifecycle

```bash
oly attach <ID>      # Detach: Ctrl-], then d
oly update <ID> --title "better name"
oly update <ID> --title ""
oly update <ID> --tag prod --tag release
oly update <ID> --tag ""
oly stop <ID>
oly ls                # oly ls --json for agents
```

- `oly update` changes session metadata without restarting the session.
- `--title ""` clears the title. If `--title` is omitted, the existing title is kept.
- `--tag ""` clears all tags. If `--tag` is omitted, existing tags are kept.
- Repeating `--tag` replaces the full tag list with the provided tags.

### 5) Notify

```bash
oly notify <ID> --title "Done" --description "Summary." --body "Details."
```

- `<ID>` is optional — include it to link the notification to a specific session.
- Toggle per-session notifications: `oly notify enable <ID>` / `oly notify disable <ID>`.

### Help

```bash
oly --help            # or: oly <command> --help
```
