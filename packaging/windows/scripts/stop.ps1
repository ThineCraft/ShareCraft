# Stops ShareCraft for the signed-in Windows user: the server and the stand-in scanner by the
# process ids Start recorded, then the database with pg_ctl stop -m fast.
#
#   -Gui  show messages in a window (used by the Start-menu shortcut, which is hidden)
[CmdletBinding()]
param([switch]$Gui)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'common.ps1')

try {
    $paths = Get-ShareCraftPaths
    Stop-ShareCraft -Paths $paths
    if ($Gui) { Show-GuiMessage -Message 'ShareCraft has stopped.' } else { Write-Step 'ShareCraft has stopped.' }
    exit 0
} catch {
    $message = "ShareCraft could not be stopped: $($_.Exception.Message)"
    if ($Gui) { Show-GuiMessage -Message $message -IsError } else { Write-Host $message -ForegroundColor Red }
    exit 1
}
