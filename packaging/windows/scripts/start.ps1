# Starts ShareCraft for the signed-in Windows user: the database (pg_ctl start -w), the
# stand-in malware scanner and the server, hidden, with their output in
# %LOCALAPPDATA%\ShareCraft\logs. Waits up to 60 seconds for the server to answer, then opens
# the browser. If ShareCraft already runs, it only opens the browser.
#
#   -NoBrowser  do not open the browser
#   -Gui        show messages in a window (used by the Start-menu shortcut, which is hidden)
[CmdletBinding()]
param([switch]$NoBrowser, [switch]$Gui)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'common.ps1')
# ShareCraft's programs use only ShareCraft's own database settings, never the user's.
Clear-DatabaseEnvironment

try {
    $paths = Get-ShareCraftPaths
    Start-ShareCraft -Paths $paths -NoBrowser:$NoBrowser | Out-Null
    exit 0
} catch {
    $message = "ShareCraft could not be started: $($_.Exception.Message)"
    if ($Gui) { Show-GuiMessage -Message $message -IsError } else { Write-Host $message -ForegroundColor Red }
    exit 1
}
