---
name: oly-subagent
description: Use when a coding agent is authorized to launch, coordinate, or supervise one or more sub-agent CLIs through oly, including choosing workflow topologies, reducing token cost, sending scoped tasks, monitoring progress, and verifying results.
---

# Orchestrate and supervise sub-agents with oly

`oly skill --subagent` prints this self-contained guide directly from the `oly` binary (no local file read, `oly skill`, or `oly start --help` is needed for standard workflows).

Use `oly` as a durable PTY and supervision runtime for **user-authorized** agent CLIs (`pi`, `codex`, `claude`, etc.). This skill does not authorize delegation, extra agents, approval bypass, or edits outside the user's scope. Use the agent CLI and model already chosen by the user or environment.

**Core `oly` CLI reference:**
- `oly daemon status` — check daemon health (`oly daemon start --detach` only if confirmed stopped; never stop/restart a shared daemon).
- `oly start --detach --disable-notifications --title "<name>" --cwd <dir> [--tag <t>] [--node <n>] <cmd> [args...]` — launch worker and print `<ID>` (`oly` flags go **before** `<cmd>`).
- `oly logs <ID> --tail <N> --wait <exit|prompt|output|idle|match> --timeout <dur>` — wait for gate (`0` met, `2` timeout, `1` error), then print output.
- `oly info <ID> --json` — inspect `status`, `exit_code`, `input_needed`, and `program_status`.

## 1) Right-size the topology (lower defaults first)

Multi-agent flows consume more tokens than single-agent work. Choose the **smallest topology** that fits the task:

| Topology | When to use | How to run |
|---|---|---|
| **Direct (0 workers)** | Small, immediate, or tightly coupled edits where you would just sit waiting on a single worker. | Do the work directly in your own session. |
| **Single worker (1)** | Offload a noisy investigation, test/build diagnosis, or isolated module change to keep raw logs out of your context. | Spawn 1 worker, wait for completion, verify its output artifact. |
| **Fan-out / Map-Reduce** | $N$ independent modules, files, or hypotheses with **disjoint write scopes**. | **Map:** launch $N$ workers in one shell call; each writes a compact result/file. **Reduce:** wait for all in one shell loop, then synthesize/verify. |
| **Staged pipeline** | Sequential phases with hard gates (`Scan -> Implement -> Review/Test`). | Run Phase 1 workers to produce a compact plan/diff; gate Phase 2 on verified Phase 1 output. |
| **Hierarchical (Lieutenants)** | Large multi-subsystem work where flat fan-out would bottleneck the lead agent. | Delegate each subsystem to a lieutenant worker that coordinates its own sub-workers and returns one verified summary. |

## 2) Choose worker mode: one-shot vs interactive

- **One-shot / non-interactive mode (default for discrete tasks):**
  Pass the task prompt directly on the command line (e.g., `pi -p --no-session "..."`, `codex exec "..."`). The worker runs to completion and **exits**, avoiding TUI startup races, modal prompts, and multi-turn polling.
- **Interactive / TUI mode (for iterative multi-turn steering or human handoff):**
  Start the interactive CLI (`pi`, `codex`), wait for the UI to settle, and send prompts via `oly send`. Keep the session alive only while follow-up turns are needed.

## 3) Low-token orchestration patterns

Every extra LLM turn in the lead agent re-sends the full conversation history. Minimize lead-agent turns and context growth:

1. **Batch launch + wait in ONE shell call (the "orchestration program"):**
   Never spend separate tool calls to start Worker A, start Worker B, wait on A, wait on B, and inspect exit codes. Run an entire phase in a single shell block:
   ```sh
   # Fan-out + wait + check exit codes in a single tool call:
   id_a=$(oly start --detach --disable-notifications --title "fix-mod-a" --cwd /path/to/repo \
     pi -p --no-session "Edit ONLY mod_a.py to <goal>. Run <check>. Print a 3-line summary.")
   id_b=$(oly start --detach --disable-notifications --title "fix-mod-b" --cwd /path/to/repo \
     pi -p --no-session "Edit ONLY mod_b.py to <goal>. Run <check>. Print a 3-line summary.")

   oly logs "$id_a" --tail 25 --wait exit --timeout 5m
   oly logs "$id_b" --tail 25 --wait exit --timeout 5m
   oly info "$id_a" --json | grep -E '"status"|"exit_code"'
   oly info "$id_b" --json | grep -E '"status"|"exit_code"'
   ```
2. **Exchange compact artifacts via the filesystem, not raw scrollback:**
   For research, scanning, or multi-phase flows, instruct each worker to write findings to a dedicated file (e.g., `/tmp/flow/worker-a.md`) and print only a short summary. The lead agent reads the compact file instead of ingesting hundreds of lines of terminal output.
3. **Crystallize state between phases:**
   Across multi-phase workflows, distil verified findings, file ownership, and interfaces into a concise shared brief (e.g., `/tmp/flow/context.md`) and pass that file path to next-phase workers instead of forwarding raw logs.
4. **Asynchronous agent-to-agent (A2A) inbox & push reporting (`oly skill --a2a`):**
   - **N-to-1 fan-in inbox (recommended for multiple or interactive workers):** Start a lightweight inbox session (`inbox=$(oly start --detach --disable-notifications --title "inbox:lead" --tag inbox sh -c 'stty -onlcr 2>/dev/null; cat >/dev/null')` and `cur=$(oly logs "$inbox" --cursor)`). Pass `$inbox` to workers so they push status/questions via `printf '%s\n' "MSG|from:worker-a|type:done|body:..." | oly send "$inbox"`. Wait on `oly logs "$inbox" --since "$cur" --wait output --timeout 5m` to wake as soon as *any* worker reports.
   - **Direct TUI push:** Because `oly` injects `OLY_SESSION_ID` into every session's environment, an idle interactive parent can also receive `oly send <parent-id> -- "DONE: ..."` (plus `oly send <parent-id> key:enter`). For peer discovery (`--tag inbox`), shared pub/sub channels, and background auto-wake, run `oly skill --a2a`.

## 4) Supervising interactive TUI workers

When running an interactive agent CLI instead of one-shot mode:

```sh
# 1. Start detached (or pass the initial prompt directly if the CLI supports it)
id=$(oly start --detach --disable-notifications --title "worker-tui" --cwd /path/to/repo <agent-cli>)

# 2. Wait for initial UI redraw to settle, then inspect the screen
oly logs "$id" --wait idle --idle-for 1s --screen --timeout 30s

# 3. Send a bounded task prompt in free-text mode (--), then press Enter separately
oly send "$id" -- 'Objective: <goal>. Allowed files: <files>. Checks: <cmd>. Report: changed files and test output.'
oly send "$id" key:enter

# 4. Wait for the worker to need input or finish its turn, then read the screen
oly logs "$id" --wait prompt --screen --timeout 5m
```

- **`--` free-text rule:** `oly send <ID> -- '...'` disables `key:`/`mouse:` parsing for all remaining arguments and does **not** press Enter. Always send `oly send <ID> key:enter` as a separate command.
- **Wait exit codes:** `0` condition met, `2` timeout, `1` error. `--wait exit` returns `0` even if the child failed; always check `exit_code` in `oly info <ID> --json`.
- **Observe after input:** `oly send` only confirms PTY delivery. On TUI menus/modals, send one step, inspect `oly logs <ID> --screen`, then decide the next step.

## 5) Safety, write scopes, and verification

- **Disjoint write scopes:** Every worker prompt must state: (a) exact objective, (b) **explicit allowed files** (and forbidden files), (c) instruction to stay strictly within `<cwd>` (no parent/`/` filesystem scans), (d) validation command to run, and (e) concise report format. Never let parallel workers edit the same file or run `git reset`/`git checkout`/`git commit` over shared state.
- **Approval boundaries:** Answer routine, in-scope questions from task context. **Never auto-approve** destructive operations, credential/secret access, unexpected network calls, broad permission escalation, or ambiguous product decisions — pause and ask the user.
- **Human co-op:** Check `attach_count` in `oly ls --json` before typing into a session a human might be controlling.
- **Verify observable outcomes:** Neither `input_needed`, `idle`, `program_status: "done"`, echoed input, nor a stopped session proves success. Verify actual diffs (`git diff` / file contents), test results, and child `exit_code` before reporting completion.
- **Clean up only your own sessions:** Stop interactive workers you started and no longer need (`oly stop <ID> --grace 5`). Retain logs until verification is complete. Never stop the shared daemon (`oly daemon stop` kills all live sessions).
