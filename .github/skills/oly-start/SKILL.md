---
name: oly-start
description: Start a any CLI for a task which needs interaction
---

`oly start --title "task 1" --detach claude`

It will return with session `<id>`, use it to wait interaction and send input:

`oly logs <id> --tail 40 --no-truncate --wait-for-prompt --timeout 600`

When you see a prompt, you can send input:

`oly input <id> --text "yes" --key enter`

> You can send multiple keys like `--key shift --key tab` or `--key "ctrl+c"`.
> `-k` is the short version of `--key`.

Then you can wait for actions again:

`oly logs <id> --tail 40 --no-truncate --wait-for-prompt --timeout 600`

Or you can stop it according to the output and history context:

`oly stop <id>`

You can check status for many sessions:

`oly ls`, it will return:

```text
ID STATUS CMD AGE PID CREATE_AT↓ TITLE ARGS
```

Below is some options for `oly ls` to filter sessions:
```text
Options:
      --search <SEARCH>  Filter by title or ID substring (case-insensitive)
  -s, --status <STATUS>  Only show sessions with these statuses (repeatable) [possible values: created, running, stopping, stopped, failed, unknown]
      --since <RFC3339>  Created at or after (RFC3339, e.g. 2026-03-04T15:04:05Z)
      --until <RFC3339>  Created at or before (RFC3339, e.g. 2026-03-04T15:04:05Z)
      --limit <LIMIT>    Maximum number of sessions to return [default: 10]
  -n, --node <NODE>      Target a secondary node by name
```