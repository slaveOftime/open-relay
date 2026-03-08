---
name: Oly
description: >
  Manage long-running agent CLI sessions with oly (Open Relay). USE WHEN starting an agent session,
  running claude-code, gemini-cli, opencode or any CLI in the background, detaching from a
  terminal, monitoring agent output, sending input to a waiting prompt, supervising another agent,
  checking what agents are running, attaching to a session, reading logs, or stopping an agent.
---

# Oly — Open Relay

`oly` is a session-persistent PTY daemon. It keeps agent CLI processes alive after the terminal
closes, notifies you when input is needed, and lets you intervene without attaching.

## Quick Reference

```
oly daemon start                              # start daemon (once per boot)
oly start --title "task" --detach <cmd>       # launch agent, detach immediately
oly list                                      # show all sessions (id, title, status, age)
oly logs <id> --tail 40                       # peek at recent output
oly input <id> "yes" --key enter              # answer a prompt without attaching
oly attach <id>                               # live attach (replays buffer first)
oly stop <id>                                 # graceful stop (5s grace default)
```

**Detach from attached session:** `Ctrl-]` then `d`  
**Session IDs** are 7-character truncated UUIDs shown by `oly list`.

## Full Command Reference

### Start a session

```bash
oly start [--title <title>] [--detach] <cmd> [args...]
```

- Without `--detach`: starts and immediately attaches (interactive by default).
- With `--detach`: prints session id and exits — agent runs in background.
- `<cmd>` is any CLI agent: `claude`, `gemini`, `opencode`, `bash`, etc.

**Examples:**
```bash
oly start --title "refactor auth" --detach claude
oly start --title "review PR 42" --detach gemini --model gemini-2.0-flash
oly start --detach bash -c "npm run build && npm run test"
```

### List sessions

```bash
oly list
```

Shows id, title, status (`running` / `stopped` / `failed`), and age for all sessions.
Completed sessions remain visible for 15 minutes (900s) after exit.

### Read logs

```bash
oly logs <id> [--tail <n>] [--strip-color] [--replay]
```

- `--tail <n>`: last N lines (default 80).
- `--strip-color`: remove ANSI color codes.
- `--replay`: interactive viewer — `↑`/`↓` scroll, `q` quit.

Use `oly logs` to sample output before deciding whether to intervene.

### Send input without attaching

```bash
oly input <id> [text...] [--key <key>]...
```

**Named keys:** `enter`, `tab`, `esc`, `backspace`, `up`, `down`, `left`, `right`,
`home`, `end`, `pgup`, `pgdn`, `del`, `ins`  
**Modifier forms:** `ctrl+c`, `alt+enter`, `shift+tab`  
**Aliases:** `ctrl-`, `alt-`, `meta-`

**Examples:**
```bash
oly input <id> "yes" --key enter        # confirm a y/n prompt
oly input <id> --key ctrl-c             # send interrupt
oly input <id> "my answer here"         # send text (no newline appended)
oly input <id> --key enter              # send bare newline
echo "multiline\nresponse" | oly input <id>   # pipe stdin
```

All injected input is audit-logged in the session's `events.log`.

### Attach

```bash
oly attach <id>
```

Replays buffered output (up to 10,000 lines), then streams live. Detach with
`Ctrl-]` then `d` — does **not** stop the child process.

### Stop

```bash
oly stop <id> [--grace <seconds>]
```

Graceful termination first; force-kills after grace timeout (default 5s).

## Agent-Supervises-Agent Pattern

One agent process can monitor and drive another using `oly logs` + `oly input`
without human involvement.

**Typical supervisor loop:**

```bash
# Step 1: launch the worker agent detached
WORKER=$(oly start --title "worker" --detach claude --task "refactor auth module")

# Step 2: supervisor polls logs and sends decisions
while true; do
  OUTPUT=$(oly logs $WORKER --tail 20 --strip-color)

  # Agent analyzes OUTPUT and decides action:
  # - If prompt detected: oly input $WORKER "yes" --key enter
  # - If needs escalation: notify human
  # - If done: oly stop $WORKER && break

  sleep 5
done
```

**When the supervisor is unsure or detects a risky action**, it escalates to the
human operator rather than auto-approving. The human can then:

```bash
oly attach $WORKER      # intervene directly
oly input $WORKER "n"   # reject the action
oly stop $WORKER        # abort entirely
```

## Session Lifecycle

```
created → running → stopping → stopped
                             → failed
```

After a session reaches `stopped` or `failed`:
- Remains in daemon memory for 15 minutes.
- `attach` and `input` fail with a clear eviction error after memory eviction.
- Logs and metadata remain on disk indefinitely at the state directory.

## Examples

**Launch Claude and monitor for prompts:**
```bash
oly start --title "big refactor" --detach claude
oly list
oly logs <id> --tail 30
# see a y/n prompt in output →
oly input <id> "y" --key enter
```

**Run a build pipeline unattended:**
```bash
oly start --title "ci" --detach bash -c "cargo test && cargo build --release"
# go do something else
oly logs <id> --tail 50   # check back later
```

**Interactive session when you want to drive it yourself:**
```bash
oly start --title "debug session" claude   # attaches immediately, no --detach
# work interactively, then Ctrl-] d to detach
oly attach <id>   # reattach later with full replay
```
