# Session Review Attach Hook

This folder contains a Windows notification hook that opens a centered terminal and attaches to an `oly` session when a notification contains exactly one session id.

## Files

- `SessionReview-Attach.vbs` is the hook entrypoint. It starts PowerShell with a hidden window so the hook does not flash a PowerShell console.
- `SessionReview-Attach.ps1` contains the real hook logic: read the session resize event, find the session working directory, create a detached launcher, and run `oly attach <session_id>` in a popup terminal.

## Behavior

For a single session id, the hook:

1. Reads `%LOCALAPPDATA%\oly\sessions\<session_id>\events.log` unless `OLY_STATE_DIR` is set.
2. Finds the latest line like `resize offset=398 rows=35 cols=110`.
3. Runs `oly ls --search <session_id> --json` and uses the exact match's `current_working_directory` when available.
4. Opens Windows Terminal with the matching `cols,rows`, centered on the primary screen.
5. Runs `oly attach <session_id>` from the session working directory.

If more than one session id is received, the hook intentionally does nothing.

## Setup

Set the `notification_hook` value in `%LOCALAPPDATA%\oly\config.json`:

```json
{
  "notification_hook": "wscript.exe 'C:\\Users\\cnBinweW\\DEV\\Slaveoftime\\open-relay\\hooks\\session-review-scripts\\SessionReview-Attach.vbs' {session_ids}"
}
```

Keep the script path quoted. The `oly` hook parser treats unquoted backslashes as escape characters, so an unquoted Windows path can be mangled before it reaches `wscript.exe`.

Restart the daemon after changing `config.json`:

```powershell
oly daemon stop
oly daemon start --detach --no-auth
```

Adjust the daemon start flags to match your normal local setup if you use auth or a custom config.

## Direct Demo Test

Pick a running session id:

```powershell
oly ls --json
```

Run the visible PowerShell implementation directly:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\hooks\session-review-scripts\SessionReview-Attach.ps1 <session_id>
```

Run through the hidden hook entrypoint, which matches the configured hook path:

```powershell
wscript.exe .\hooks\session-review-scripts\SessionReview-Attach.vbs <session_id>
```

For console-visible VBScript debugging, use `cscript.exe` instead of `wscript.exe`:

```powershell
cscript.exe //NoLogo .\hooks\session-review-scripts\SessionReview-Attach.vbs <session_id>
```

## Diagnostics

The PowerShell script writes diagnostics to:

```text
%LOCALAPPDATA%\oly\logs\session-review-hook.log
```

Tail the log while testing:

```powershell
Get-Content "$env:LOCALAPPDATA\oly\logs\session-review-hook.log" -Tail 40 -Wait
```

The script also writes temporary command files that are useful for debugging quoting and launch behavior:

```text
%TEMP%\oly-attach-run-<session_id>.cmd
%TEMP%\oly-attach-launch-<session_id>.cmd
```

Inspect them with:

```powershell
Get-Content "$env:TEMP\oly-attach-run-<session_id>.cmd"
Get-Content "$env:TEMP\oly-attach-launch-<session_id>.cmd"
```

## Troubleshooting

- If no popup appears, confirm the daemon was restarted after editing `%LOCALAPPDATA%\oly\config.json`.
- If the hook log has no new entries, check that `notification_hook` points to `SessionReview-Attach.vbs` and that the path is quoted.
- If the popup opens in the wrong folder, run `oly ls --search <session_id> --json` and check `current_working_directory` for the exact session id.
- If the popup says `events.log not found` or `No resize event found`, the session may not have a stored resize event yet.
- If Windows Terminal is unavailable, the script falls back to `cmd.exe`; sizing is less precise in that mode.