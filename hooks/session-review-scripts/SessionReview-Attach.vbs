Option Explicit

Dim shell
Dim scriptPath
Dim command
Dim index

Set shell = CreateObject("WScript.Shell")
scriptPath = "C:\Users\cnBinweW\DEV\Slaveoftime\open-relay\hooks\session-review-scripts\SessionReview-Attach.ps1"
command = "powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File " & Quote(scriptPath)

For index = 0 To WScript.Arguments.Count - 1
    command = command & " " & Quote(WScript.Arguments(index))
Next

shell.Run command, 0, False

Function Quote(value)
    Quote = Chr(34) & Replace(value, Chr(34), Chr(34) & Chr(34)) & Chr(34)
End Function