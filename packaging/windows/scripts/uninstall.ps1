# Removes ShareCraft for the signed-in Windows user. It stops everything, removes the program
# and the shortcuts, and deletes the data and the database only if you say so (the default
# is to keep them).
#
#   -Yes         do not ask "Remove ShareCraft?" first
#   -DeleteData  also delete all data and the database, without asking
#   -KeepData    keep the data and the database, without asking
[CmdletBinding()]
param([switch]$Yes, [switch]$DeleteData, [switch]$KeepData)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'common.ps1')

function Wait-ForEnter {
    if (-not $Yes) { [void](Read-Host 'Press Enter to close this window') }
}

try {
    if ($DeleteData -and $KeepData) { throw 'Give -DeleteData or -KeepData, not both.' }
    $paths = Get-ShareCraftPaths
    if (-not (Test-ShareCraftInstalled -Paths $paths) -and -not (Test-Path -LiteralPath $paths.StartMenu)) {
        Write-Step 'ShareCraft is not installed for this Windows user. Nothing was changed.'
        Wait-ForEnter
        exit 0
    }
    if (-not $Yes -and -not (Read-YesNo -Question "Remove ShareCraft for $env:USERNAME from this computer?" -Default $false)) {
        Write-Step 'Nothing was changed.'
        Wait-ForEnter
        exit 0
    }
    # The program folder is about to go; work from somewhere else.
    Set-Location -LiteralPath $env:TEMP
    $pgBin = $paths.PgBin
    Stop-ShareCraft -Paths $paths -PgBin $pgBin

    $delete = $false
    if ($DeleteData) {
        $delete = $true
    } elseif (-not $KeepData -and (Test-Path -LiteralPath $paths.Root)) {
        Write-Step ''
        Write-Step "Your documents, accounts and the database are in $($paths.Root)."
        $delete = Read-YesNo -Question 'Also delete all ShareCraft data and the database?' -Default $false
    }
    Remove-ShareCraft -Paths $paths -DeleteData:$delete
    Write-Step 'ShareCraft has been uninstalled.'
    Wait-ForEnter
    exit 0
} catch {
    Write-Host "ShareCraft was not fully uninstalled: $($_.Exception.Message)" -ForegroundColor Red
    Wait-ForEnter
    exit 1
}
