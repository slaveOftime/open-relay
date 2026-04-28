[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [string]$SessionIds,

    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$ExtraSessionIds
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$MinTerminalCols = 20
$MinTerminalRows = 5
$EstimatedCellWidthPx = 9
$EstimatedCellHeightPx = 18
$EstimatedWindowChromeWidthPx = 24
$EstimatedWindowChromeHeightPx = 72

function Write-HookLog {
    param(
        [string]$Message
    )

    try {
        $logDir = [System.IO.Path]::Combine((Get-OlyStateDir), 'logs')
        [System.IO.Directory]::CreateDirectory($logDir) | Out-Null
        $timestamp = [DateTimeOffset]::Now.ToString('o')
        Add-Content -LiteralPath ([System.IO.Path]::Combine($logDir, 'session-review-hook.log')) -Value "[$timestamp] $Message"
    }
    catch {
        # Notification hooks must never fail just because diagnostic logging failed.
    }
}

function Get-OlyStateDir {
    if ($env:OLY_STATE_DIR) {
        return $env:OLY_STATE_DIR
    }

    if ($env:LOCALAPPDATA) {
        return [System.IO.Path]::Combine($env:LOCALAPPDATA, 'oly')
    }

    $userProfile = [Environment]::GetFolderPath('UserProfile')
    if (-not $userProfile) {
        throw 'Unable to resolve the oly state directory.'
    }

    return [System.IO.Path]::Combine($userProfile, 'AppData', 'Local', 'oly')
}

function Get-RequestedSessionIds {
    param(
        [string]$Primary,
        [string[]]$Additional
    )

    $rawValues = @()
    if ($Primary) {
        $rawValues += $Primary
    }
    if ($Additional) {
        $rawValues += $Additional
    }
    if ($env:OLY_EVENT_SESSION_IDS) {
        $rawValues += $env:OLY_EVENT_SESSION_IDS
    }

    $ids = foreach ($value in $rawValues) {
        foreach ($part in ($value -split ',')) {
            $trimmed = $part.Trim()
            if ($trimmed) {
                $trimmed
            }
        }
    }

    return @($ids | Select-Object -Unique)
}

function Get-SingleSessionIdOrNull {
    param(
        [string[]]$Ids
    )

    if ($Ids.Length -ne 1) {
        Write-Verbose "Expected exactly one session id, got $($Ids.Length)."
        return $null
    }

    $sessionId = $Ids[0]
    if ($sessionId -notmatch '^[A-Za-z0-9]+$') {
        throw "Invalid session id '$sessionId'."
    }

    return $sessionId
}

function Get-SessionEventsLogPath {
    param(
        [string]$SessionId
    )

    $stateDir = Get-OlyStateDir
    return [System.IO.Path]::Combine($stateDir, 'sessions', $SessionId, 'events.log')
}

function Get-LatestResizeEvent {
    param(
        [string]$EventsLogPath
    )

    if (-not (Test-Path -LiteralPath $EventsLogPath -PathType Leaf)) {
        throw "events.log not found at '$EventsLogPath'."
    }

    $latest = $null
    foreach ($line in [System.IO.File]::ReadLines($EventsLogPath)) {
        if ($line -match '^resize\s+offset=\d+\s+rows=(\d+)\s+cols=(\d+)\s*$') {
            $latest = [pscustomobject]@{
                Rows = [int]$matches[1]
                Cols = [int]$matches[2]
            }
        }
    }

    if (-not $latest) {
        throw "No resize event found in '$EventsLogPath'."
    }

    return $latest
}

function Get-SessionWorkingDirectoryOrNull {
    param(
        [string]$SessionId
    )

    try {
        $listing = oly ls --search $SessionId --json | ConvertFrom-Json
        $matches = @($listing.items | Where-Object { $_.id -eq $SessionId })
        if ($matches.Length -ne 1) {
            Write-Verbose "Expected one matching session from oly ls, got $($matches.Length)."
            return $null
        }

        $workingDirectory = [string]$matches[0].current_working_directory
        if (-not $workingDirectory) {
            Write-Verbose "Session '$SessionId' has no current_working_directory."
            return $null
        }
        if (-not (Test-Path -LiteralPath $workingDirectory -PathType Container)) {
            Write-Verbose "Session '$SessionId' working directory does not exist: $workingDirectory"
            return $null
        }

        return $workingDirectory
    }
    catch {
        Write-Verbose "Unable to read session working directory: $($_.Exception.Message)"
        return $null
    }
}

function Get-TerminalSize {
    param(
        [int]$Rows,
        [int]$Cols
    )

    return [pscustomobject]@{
        Rows = [Math]::Max($MinTerminalRows, $Rows)
        Cols = [Math]::Max($MinTerminalCols, $Cols)
    }
}

function Get-CenteredTerminalPosition {
    param(
        [int]$Rows,
        [int]$Cols
    )

    try {
        Add-Type -AssemblyName System.Windows.Forms -ErrorAction Stop
        $workArea = [System.Windows.Forms.Screen]::PrimaryScreen.WorkingArea

        $windowWidth = ($Cols * $EstimatedCellWidthPx) + $EstimatedWindowChromeWidthPx
        $windowHeight = ($Rows * $EstimatedCellHeightPx) + $EstimatedWindowChromeHeightPx
        $x = [Math]::Max($workArea.Left, [int]($workArea.Left + (($workArea.Width - $windowWidth) / 2)))
        $y = [Math]::Max($workArea.Top, [int]($workArea.Top + (($workArea.Height - $windowHeight) / 2)))

        return "$x,$y"
    }
    catch {
        Write-Verbose "Unable to compute centered terminal position: $($_.Exception.Message)"
        return $null
    }
}

function Quote-CmdArgument {
    param(
        [string]$Value
    )

    return '"' + $Value.Replace('"', '""') + '"'
}

function Get-AttachCommandLine {
    param(
        [string]$SessionId,
        [string]$WorkingDirectory
    )

    $attachCommand = "oly attach $SessionId"
    if (-not $WorkingDirectory) {
        return $attachCommand
    }

    return "cd /d $(Quote-CmdArgument $WorkingDirectory) && $attachCommand"
}

function New-AttachCommandScript {
    param(
        [string]$SessionId,
        [string]$WorkingDirectory
    )

    $scriptPath = [System.IO.Path]::Combine([System.IO.Path]::GetTempPath(), "oly-attach-run-$SessionId.cmd")
    $lines = @('@echo off')

    if ($WorkingDirectory) {
        $lines += "cd /d $(Quote-CmdArgument $WorkingDirectory)"
        $lines += 'if errorlevel 1 ('
        $lines += '  echo Failed to change to the session working directory.'
        $lines += '  pause'
        $lines += '  exit /b 1'
        $lines += ')'
    }

    $lines += "oly attach $SessionId"
    Set-Content -LiteralPath $scriptPath -Value ($lines -join "`r`n") -Encoding ASCII
    return $scriptPath
}

function Invoke-DetachedCmdLauncher {
    param(
        [string]$LauncherBody,
        [string]$SessionId,
        [string]$Label
    )

    $cmdCommand = Get-Command cmd.exe -ErrorAction Stop
    $launcherPath = [System.IO.Path]::Combine([System.IO.Path]::GetTempPath(), "oly-attach-launch-$SessionId.cmd")
    $contents = "@echo off`r`n$LauncherBody`r`n"
    Set-Content -LiteralPath $launcherPath -Value $contents -Encoding ASCII

    Write-HookLog "launcher path='$launcherPath' body='$LauncherBody'"
    Start-Process -FilePath $cmdCommand.Source -ArgumentList @('/c', (Quote-CmdArgument $launcherPath)) -WindowStyle Hidden | Out-Null
    Write-HookLog "launched detached $Label launcher for session=$SessionId"
}

function Start-AttachTerminal {
    param(
        [string]$SessionId,
        [int]$Rows,
        [int]$Cols,
        [string]$WorkingDirectory
    )

    $cmdCommand = Get-Command cmd.exe -ErrorAction Stop
    $terminalSize = Get-TerminalSize -Rows $Rows -Cols $Cols
    $attachCommand = Get-AttachCommandLine -SessionId $SessionId -WorkingDirectory $WorkingDirectory
    $attachScriptPath = New-AttachCommandScript -SessionId $SessionId -WorkingDirectory $WorkingDirectory
    Write-HookLog "launch session=$SessionId rows=$($terminalSize.Rows) cols=$($terminalSize.Cols) cwd='$WorkingDirectory' cmd='$attachCommand'"
    Write-HookLog "attach script path='$attachScriptPath'"

    $wtCommand = Get-Command wt.exe -ErrorAction SilentlyContinue
    if ($wtCommand) {
        $wtArgs = @(
            '--window',
            'new',
            '--size',
            "$($terminalSize.Cols),$($terminalSize.Rows)"
        )

        $position = Get-CenteredTerminalPosition -Rows $terminalSize.Rows -Cols $terminalSize.Cols
        if ($position) {
            $wtArgs += @('--pos', $position)
        }

        $launcherParts = @(
            'start',
            '""',
            (Quote-CmdArgument $wtCommand.Source),
            '--window',
            'new',
            '--size',
            (Quote-CmdArgument "$($terminalSize.Cols),$($terminalSize.Rows)")
        )
        if ($position) {
            $launcherParts += @('--pos', (Quote-CmdArgument $position))
        }
        $launcherParts += @((Quote-CmdArgument $cmdCommand.Source), '/k', 'call', (Quote-CmdArgument $attachScriptPath))

        Invoke-DetachedCmdLauncher -LauncherBody ($launcherParts -join ' ') -SessionId $SessionId -Label 'wt.exe'
        return
    }

    $cmdLine = "mode con: cols=$($terminalSize.Cols) lines=$($terminalSize.Rows) && call $(Quote-CmdArgument $attachScriptPath)"
    $launcher = "start `"`" $(Quote-CmdArgument $cmdCommand.Source) /k $(Quote-CmdArgument $cmdLine)"
    Invoke-DetachedCmdLauncher -LauncherBody $launcher -SessionId $SessionId -Label 'cmd.exe fallback'
}

try {
[string[]]$requestedSessionIds = @(Get-RequestedSessionIds -Primary $SessionIds -Additional $ExtraSessionIds)
Write-HookLog "received ids='$($requestedSessionIds -join ',')' env='$env:OLY_EVENT_SESSION_IDS' args='$SessionIds $($ExtraSessionIds -join ' ')'"
$sessionId = Get-SingleSessionIdOrNull -Ids $requestedSessionIds
if (-not $sessionId) {
    Write-HookLog 'ignored notification because it did not contain exactly one session id'
    return
}

$eventsLogPath = Get-SessionEventsLogPath -SessionId $sessionId
$latestResize = Get-LatestResizeEvent -EventsLogPath $eventsLogPath
$workingDirectory = Get-SessionWorkingDirectoryOrNull -SessionId $sessionId
Start-AttachTerminal -SessionId $sessionId -Rows $latestResize.Rows -Cols $latestResize.Cols -WorkingDirectory $workingDirectory
}
catch {
    Write-HookLog "failed: $($_.Exception.Message)"
    throw
}