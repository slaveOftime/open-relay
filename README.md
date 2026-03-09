# oly — Open Relay

<p align="center"><img src="./assets/icon-demo.svg" width="160" /></p>

> Run agents. Walk away. Intervene when it matters.

`oly` is a session-persistent PTY daemon for long-running CLI agents (Claude Code, Gemini CLI, OpenCode, normal CLI etc.). It keeps your agent alive after you close the terminal, notifies you when something needs a human, and lets you intervene from anywhere.

For a full repository architecture map, see [ARCHITECTURE.md](./ARCHITECTURE.md).

---

## The problem 😤

You start an agent task. It runs for 20 minutes. Halfway through it hits a `y/n` prompt and just... sits there. You had to keep a terminal open, stay at your desk, babysit it.

**That's over.**

---

## What oly does ⚡

- Owns agent sessions in a background daemon — closing your terminal changes nothing
- Replays buffered output when you reattach (no lost context)
- Detects when input is likely needed and notifies you
- Lets you inject input without attaching (`oly input <id> --key enter`)
- Keeps auditable logs of everything

### Agent-supervises-agent 🤖 👀 🤖

You can run one agent to supervise another. When the supervisor hits something it's unsure about — or something that needs elevated permission — it escalates to you. You decide whether to approve, modify, or abort. You're still in the loop, just not watching.

---

## Install 📦

### Pre-built Binaries
Download the latest release for your platform from the [Releases page](https://github.com/Slaveoftime/open-relay/releases).

- **macOS**: Download `oly-macos-arm64`, unzip, `chmod +x oly`, and move to `/usr/local/bin`.
- **Linux**: Download `oly-linux-amd64`, unzip, `chmod +x oly`, and move to `/usr/local/bin`.
- **Windows**: Download `oly-windows-amd64`, unzip, and add to your PATH.

### From Source

```sh
cargo install --path .
```

---

## Quick start 🚀

The cli is both for human and AI agent.

```sh
# Start the daemon (once per boot, or add to your init)
oly daemon start

# Launch an agent session and detach immediately
oly start --title "code is cheap" --detach copilot

# Check what's running
oly ls

# Peek at output without attaching
oly logs <id>

# Something needs your input — send it without a terminal
oly input <id> --text "yes" --key enter

# Actually attach and drive it yourself
# Ctrl+D to detach anytime
oly attach <id>

# Stop it when done
oly stop <id>
```

---

## Remote supervision 🌐

Access and manage sessions from browser, with push notification support. Intervene from anywhere.

`oly` has no built-in network listener — deliberately. Expose it the way you control:

```
[anywhere] → [your auth gateway] → [tunnel] → [oly daemon, local IPC]
```

Put Cloudflare Access, Tailscale, or any auth proxy in front. Every action logged. Your rules. 🔒

---

## Commands 📋

| Command | What it does |
|---|---|
| `oly daemon start` | Start background daemon |
| `oly start [--detach] [--disable-notifications] <cmd>` | Launch session in PTY |
| `oly ls` | Show sessions (supports search/status/time filters) |
| `oly attach <id>` | Reattach (replays buffer first) |
| `oly logs <id> [--tail N] [--wait-for-prompt]` | Read logs without attaching |
| `oly input <id> [--text TEXT] [--key k]` | Send input without attaching |
| `oly stop <id>` | Graceful stop |

### Federation commands

| Command | What it does |
|---|---|
| `oly api-key add <name>` | Create API key on primary (printed once) |
| `oly api-key ls` | List API key labels on primary |
| `oly api-key remove <name>` | Revoke API key on primary |
| `oly join start --name <name> --key <key> <url>` | Connect this daemon as a secondary |
| `oly join ls` | List saved join configs on this daemon |
| `oly join stop --name <name>` | Disconnect and remove a saved join config |
| `oly node ls` | List currently connected secondary nodes on primary |

All session commands support `--node <name>` to target a connected secondary.

Detach from an attached session: `Ctrl-]` then `d`.
