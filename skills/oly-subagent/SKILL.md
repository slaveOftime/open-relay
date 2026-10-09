---
name: oly-subagent
description: Use when a coding agent is authorized to launch or supervise another agent CLI through oly, including sending scoped tasks, handling prompts, monitoring progress, and verifying its result. Not for ordinary terminal commands; use the oly skill instead.
---

# Supervise an agent CLI with oly

Use `oly` as a durable PTY for the **user-authorized** worker CLI. This skill does not authorize delegation, extra agents, remote execution, approval bypass, or changes outside the user's requested scope. For CLI flags, cursors, nodes, and general session mechanics, consult `oly skill` (or `../oly/SKILL.md`). Do not assume a particular provider, model, or prompt flag: use the agent CLI the user chose or one already available in the environment.

## Start a scoped worker

Before starting, decide what the worker may change, which workspace/cwd it should use, how it will report results, and what verifies completion. In a shared workspace, assign disjoint write scopes; do not let workers reset, overwrite, or commit someone else's work. Do immediate blocking work yourself rather than launching a worker you must wait on before doing anything else.

```sh
oly daemon status
oly start --title "scoped task" --cwd /path/to/repo --detach <agent-cli> [agent-args...]
```

Save the returned **session ID** and specify it on every subsequent `oly` call. Use `--node <name>` consistently only if the worker was explicitly started on that node. If a sandbox reports `Access is denied`, the daemon may still be running; retry the failed `oly` command with the available approval mechanism instead of stopping or restarting the daemon. If daemon status genuinely confirms it is stopped, `oly daemon start --detach` is appropriate.

Every session knows its own id at runtime: oly injects `OLY_SESSION_ID` into the child's environment, so a worker reads its id with `$OLY_SESSION_ID`, and you can read yours the same way. To give the worker a way to hand results back without you polling its logs, pass your own session id in the task prompt and have it report with `oly send <your-session-id> -- <result>`:

```sh
# your id (if you run inside an oly session): echo "$OLY_SESSION_ID"
# hand that literal id to the worker in the task prompt; it then reports back with e.g.:
#   oly send abc1234 -- changed foo.rs and bar.rs, cargo test passes
```

Optional — watching the worker's logs below is normally enough — and it recurses: a worker that delegates its own sub worker reads its `$OLY_SESSION_ID` and passes it along the same way, so results can always reach the originating session.

If the CLI supports a documented one-shot prompt option, use it when suitable. Otherwise wait until its input UI is ready (`oly logs <ID> --screen`), then send a bounded task prompt containing the objective, allowed files, constraints, relevant checks, and what to report. Avoid asking the worker to run `oly skill` unless it itself needs oly.

```sh
oly send <ID> -- 'Implement the scoped change in <files>; run <checks>; report changed files, checks, and any blockers.'
oly send <ID> key:enter
```

`--` sends literal text but does **not** interpret `key:enter` after it: send Enter separately. Plain-text prompts that do not need free-text mode can be sent as `oly send <ID> "task" key:enter`.

## Observe, decide, act

```sh
oly logs <ID> --tail 40 --wait prompt --timeout 5m
oly logs <ID> --screen                  # current TUI, not a completion signal; pass --color always to keep ANSI in pipes
oly ls --json                           # status, node, attach count
oly logs <ID> --wait exit --timeout 30s # exit 2 means timeout, not success
```

Read fresh output and the current screen before replying. Startup/builds can leave the screen blank temporarily; idle or `input_needed` is a hint, not proof of completion. `oly send` only confirms bytes reached the PTY: check the next screen/output for the worker's actual response. Avoid rapid key sequences on menus; send a step, observe the redraw, then decide the next step. For long-running work, use bounded waits/cursors from `oly skill` instead of polling a full log repeatedly.

Default logs are always tail 40; TUI inspection needs explicit `--screen`.
Views are lossy observations, not transcripts. For exact future-byte polling,
start with `cursor=$(oly logs <ID> --cursor)`, then use `--since "$cursor"
--wait output --json`, decode base64 `.bytes`, and chain `.cursor`; drain while
`.has_more`. Never combine `--since` with a view flag. Timeout preserves the
cursor; a restart fails explicitly. See the general oly skill for page limits,
line-scoped match waits, and sampled-frame semantics.

Answer routine, authorized questions using the task context. **Never auto-approve** destructive actions, credential requests, unexpected network access, broad permission changes, or ambiguous choices; pause and ask the user when authorization is needed. An agent CLI's own approval prompts remain in force even when controlled through oly. If another person has attached, coordinate before sending input (`oly ls --json` exposes `attach_count`). Do not interpret echoed prompts or a message sent to another session as an acknowledgement.

## Finish and hand back

Check the worker's final output **and** observable artifacts (diffs, tests, files) before reporting completion. A stopped session alone is not success, and a still-running interactive agent may have finished its task but be waiting at a prompt. Report the worker ID, what actually changed, checks and failures, and any unfinished work. Gracefully exit or stop only sessions you started and no longer need; verify their status before assuming a Ctrl+C closed them. Do not stop the shared daemon or `oly rm` sessions just to clean up.

If you run inside an oly session, the worker can reach you with `oly send <your-session-id> -- <result>`, where your id is your `$OLY_SESSION_ID` handed to the worker in the task prompt. Prefer watching the worker's durable logs to depending on that message back: delivery/echo is never proof the parent processed it, so require the result in the worker's own log and repeat it in your final answer.