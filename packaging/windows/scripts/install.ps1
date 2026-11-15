# Installs ShareCraft for the signed-in Windows user, without administrator rights.
# Run it through "Install ShareCraft.cmd" in the package folder.
#
#   -Reinstall          install this copy over an earlier one, keeping its data and database
#   -AdminEmail/-AdminName  the first administrator (asked for when not given)
#   -StartAtSignIn yes|no   add a Startup shortcut (asked for when not given)
#   -NoBrowser          do not open the browser at the end
#
# The program goes to %LOCALAPPDATA%\Programs\ShareCraft; the database, data, logs, .env and
# settings to %LOCALAPPDATA%\ShareCraft. No password, database address or .env content is
# ever printed; the administrator's password is asked for by sharecraft-server itself.
[CmdletBinding()]
param(
    [switch]$Reinstall,
    [string]$AdminEmail,
    [string]$AdminName,
    [ValidateSet('ask', 'yes', 'no')][string]$StartAtSignIn = 'ask',
    [switch]$NoBrowser
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'common.ps1')
# ShareCraft's programs use only ShareCraft's own database settings, never the user's.
Clear-DatabaseEnvironment

function Read-Required {
    param([Parameter(Mandatory)][string]$Prompt, [Parameter(Mandatory)][string]$Pattern, [Parameter(Mandatory)][string]$Hint)
    while ($true) {
        $value = Read-Host $Prompt
        if ($null -ne $value) { $value = $value.Trim() }
        if ($value -and $value -match $Pattern) { return $value }
        Write-Step $Hint
    }
}

function New-Cluster {
    # initdb with a random superuser password passed through --pwfile, deleted at once; then
    # the database listens on 127.0.0.1 only, on its own port.
    param([Parameter(Mandatory)]$Paths, [Parameter(Mandatory)][string]$PgBin, [Parameter(Mandatory)][string]$SuperPassword)
    Write-Step 'Making the database (this takes a minute)...'
    New-Item -ItemType Directory -Force -Path $Paths.Db | Out-Null
    $pwfile = Join-Path $Paths.Run ('initdb-' + [guid]::NewGuid().ToString('N') + '.txt')
    try {
        Write-SecretFile -Path $pwfile -Content "$SuperPassword`n"
        Invoke-Native -FilePath (Get-PgTool $PgBin 'initdb') -Arguments @(
            '-D', (ConvertTo-PgSafePath $Paths.Db), '-U', 'postgres', '--pwfile', (ConvertTo-PgSafePath $pwfile),
            '-L', (ConvertTo-PgSafePath (Join-Path (Split-Path -Parent $PgBin) 'share')),
            '--auth=scram-sha-256', '--encoding=UTF8', '--locale=C') `
            -What 'Making the database (initdb)' -OutputBase (Join-Path $Paths.Logs 'initdb') | Out-Null
    } finally {
        if (Test-Path -LiteralPath $pwfile) { Remove-Item -LiteralPath $pwfile -Force }
    }
    $conf = Join-Path $Paths.Db 'postgresql.conf'
    [System.IO.File]::AppendAllText($conf, "`n# ShareCraft: the port and address are in sharecraft.conf, written by the installer.`ninclude 'sharecraft.conf'`n", [System.Text.Encoding]::ASCII)
}

function Write-ClusterSettings {
    param([Parameter(Mandatory)]$Paths, [Parameter(Mandatory)][int]$Port)
    $text = "# Written by the ShareCraft installer.`nlisten_addresses = '127.0.0.1'`nport = $Port`n"
    [System.IO.File]::WriteAllText((Join-Path $Paths.Db 'sharecraft.conf'), $text, [System.Text.Encoding]::ASCII)
}

function New-ShareCraftDatabase {
    # The role sharecraft with its own random password, and the database sharecraft. The
    # statement is not written to the database log even if it fails.
    param([Parameter(Mandatory)]$Paths, [Parameter(Mandatory)][string]$PgBin, [Parameter(Mandatory)][int]$Port,
        [Parameter(Mandatory)][string]$SuperPassword, [Parameter(Mandatory)][string]$RolePassword)
    Write-Step 'Making the ShareCraft database and its account...'
    $sql = @"
set log_min_error_statement = panic;
set log_statement = 'none';
create role sharecraft login password '$RolePassword';
create database sharecraft owner sharecraft encoding 'UTF8' template template0;
"@
    Invoke-WithPgPass -Folder $Paths.Run -Port $Port -User 'postgres' -Password $SuperPassword -ScriptBlock {
        Invoke-Psql -Paths $Paths -PgBin $PgBin -Port $Port -User 'postgres' -Database 'postgres' -Sql $sql `
            -What 'Making the ShareCraft database' -Secret
    }
}

function Add-Administrator {
    # sharecraft-server add-user --admin; the server asks for the password itself. Up to three
    # tries, for a password it refuses.
    param([Parameter(Mandatory)]$Paths, [Parameter(Mandatory)][string]$Email, [Parameter(Mandatory)][string]$Name)
    Clear-DatabaseEnvironment   # the server must read only ShareCraft's .env
    Write-Step ''
    Write-Step "Now choose the password for $Email (at least 10 characters). It is not shown as you type."
    for ($try = 1; $try -le 3; $try++) {
        Push-Location -LiteralPath $Paths.Root
        try {
            & $Paths.ServerExe add-user --email $Email --name $Name --admin
            $code = $LASTEXITCODE
        } finally {
            Pop-Location
        }
        if ($code -eq 0) { return }
        if ($try -lt 3) { Write-Step 'The administrator was not created (see the message above). Try again.' }
    }
    throw 'The first administrator could not be created.'
}

function Set-StandInScanner {
    param([Parameter(Mandatory)]$Paths, [Parameter(Mandatory)][string]$PgBin, [Parameter(Mandatory)][int]$PgPort,
        [Parameter(Mandatory)][string]$RolePassword, [Parameter(Mandatory)][string]$Sql)
    Invoke-WithPgPass -Folder $Paths.Run -Port $PgPort -User 'sharecraft' -Password $RolePassword -ScriptBlock {
        Invoke-Psql -Paths $Paths -PgBin $PgBin -Port $PgPort -User 'sharecraft' -Database 'sharecraft' -Sql $Sql `
            -What 'Setting the scanner address'
    }
}

function Install-ShareCraft {
    $package = Split-Path -Parent $PSScriptRoot
    Assert-PackageFolder -Folder $package
    $paths = Get-ShareCraftPaths
    $existing = Test-ShareCraftInstalled -Paths $paths
    Assert-InstallAllowed -Paths $paths -Reinstall:$Reinstall
    $reusing = $existing -and (Test-Path -LiteralPath (Join-Path $paths.Db 'PG_VERSION'))
    $packagePg = Join-Path $package 'pgsql\bin'
    # The database programs need a plain-ASCII path to everything below LOCALAPPDATA: find out
    # now, before copying anything, whether one exists.
    ConvertTo-PgSafePath -Path $env:LOCALAPPDATA | Out-Null
    # The helper that starts programs is compiled in TEMP, which needs a plain-ASCII path too.
    Initialize-SafeTemp

    Write-Step "Installing ShareCraft for $env:USERNAME"
    Write-Step "  program: $($paths.Program)"
    Write-Step "  data:    $($paths.Root)"
    Write-Step ''

    # Every question first, so the rest runs without waiting.
    if (-not $reusing) {
        if (-not $AdminEmail) {
            $AdminEmail = Read-Required -Prompt "The first administrator's email address" -Pattern '^[^@\s]+@[^@\s]+$' -Hint 'Please type an email address, such as you@example.org.'
        }
        if (-not $AdminName) {
            $AdminName = Read-Required -Prompt "The administrator's name, as people will see it" -Pattern '^[^"]*\S[^"]*$' -Hint 'Please type a name (without double quotes).'
        }
    }
    $atSignIn = switch ($StartAtSignIn) {
        'yes' { $true }
        'no' { $false }
        default { Read-YesNo -Question 'Start ShareCraft when you sign in to Windows?' -Default $false }
    }

    $createdRoot = -not (Test-Path -LiteralPath $paths.Root)
    $done = $false
    try {
        if ($existing) {
            Write-Step 'Stopping the ShareCraft that is installed now...'
            $stopPg = if (Test-Path -LiteralPath (Join-Path $paths.PgBin 'pg_ctl.exe')) { $paths.PgBin } else { $packagePg }
            Initialize-StateFolders -Paths $paths
            Stop-ShareCraft -Paths $paths -PgBin $stopPg
        }

        Write-Step 'Copying the program (about 800 MB)...'
        Copy-ShareCraftProgram -Source $package -Destination $paths.Program
        Initialize-StateFolders -Paths $paths

        $oldSettings = $null
        if (Test-Path -LiteralPath $paths.SettingsFile) { $oldSettings = Read-SettingsFile -Path $paths.SettingsFile }
        $ports = Get-InstallPorts -Paths $paths
        Write-Step "Ports: ShareCraft $($ports.App), database $($ports.Pg), stand-in scanner $($ports.Icap)."

        $rolePassword = $null
        if ($reusing) {
            Write-ClusterSettings -Paths $paths -Port $ports.Pg
            $rolePassword = Get-UrlPassword -Url (Read-EnvDatabaseUrl -Path $paths.EnvFile)
            Write-EnvFile -Path $paths.EnvFile -DatabaseUrl (New-DatabaseUrl -Password $rolePassword -Port $ports.Pg)
            Write-SettingsFile -Path $paths.SettingsFile -AppPort $ports.App -PgPort $ports.Pg -IcapPort $ports.Icap
            Start-Postgres -Paths $paths -PgBin $paths.PgBin
            if ($oldSettings -and [int]$oldSettings.SHARECRAFT_ICAP_PORT -ne $ports.Icap) {
                # Follow the stand-in scanner to its new port, unless an administrator chose another scanner.
                $sql = Get-ScannerSettingSql -Url (Get-ScannerUrl $ports.Icap) -OnlyIf (Get-ScannerUrl ([int]$oldSettings.SHARECRAFT_ICAP_PORT))
                Set-StandInScanner -Paths $paths -PgBin $paths.PgBin -PgPort $ports.Pg -RolePassword $rolePassword -Sql $sql
            }
            Write-Step 'Kept the existing database, data and administrator accounts.'
        } else {
            $superPassword = New-RandomSecret
            $rolePassword = New-RandomSecret
            New-Cluster -Paths $paths -PgBin $paths.PgBin -SuperPassword $superPassword
            Write-ClusterSettings -Paths $paths -Port $ports.Pg
            Write-SettingsFile -Path $paths.SettingsFile -AppPort $ports.App -PgPort $ports.Pg -IcapPort $ports.Icap
            Start-Postgres -Paths $paths -PgBin $paths.PgBin
            New-ShareCraftDatabase -Paths $paths -PgBin $paths.PgBin -Port $ports.Pg -SuperPassword $superPassword -RolePassword $rolePassword
            $superPassword = $null
            Write-EnvFile -Path $paths.EnvFile -DatabaseUrl (New-DatabaseUrl -Password $rolePassword -Port $ports.Pg)
            Set-ServerEnvironment -Settings (Read-SettingsFile -Path $paths.SettingsFile)
            # add-user brings the database schema up to date before it creates the account.
            Add-Administrator -Paths $paths -Email $AdminEmail -Name $AdminName
            Write-Step 'Switching on the stand-in malware scanner...'
            $sql = Get-ScannerSettingSql -Url (Get-ScannerUrl $ports.Icap) -AdminEmail $AdminEmail
            Set-StandInScanner -Paths $paths -PgBin $paths.PgBin -PgPort $ports.Pg -RolePassword $rolePassword -Sql $sql
        }
        $rolePassword = $null

        Write-Step 'Adding Start-menu shortcuts...'
        $scripts = Join-Path $paths.Program 'scripts'
        New-ShareCraftShortcut -Path (Join-Path $paths.StartMenu 'ShareCraft.lnk') -Script (Join-Path $scripts 'start.ps1') -ScriptArguments @('-Gui') -Hidden -Description 'Start ShareCraft and open it in the browser'
        New-ShareCraftShortcut -Path (Join-Path $paths.StartMenu 'Stop ShareCraft.lnk') -Script (Join-Path $scripts 'stop.ps1') -ScriptArguments @('-Gui') -Hidden -Description 'Stop ShareCraft and its database'
        New-ShareCraftShortcut -Path (Join-Path $paths.StartMenu 'Uninstall ShareCraft.lnk') -Script (Join-Path $scripts 'uninstall.ps1') -Description 'Remove ShareCraft from this computer'
        $guide = Join-Path $paths.Program 'README.html'
        if (Test-Path -LiteralPath $guide) {
            $shell = New-Object -ComObject WScript.Shell
            $link = $shell.CreateShortcut((Join-Path $paths.StartMenu 'ShareCraft guide.lnk'))
            $link.TargetPath = $guide
            $link.Save()
        }
        if ($atSignIn) {
            New-ShareCraftShortcut -Path $paths.StartupShortcut -Script (Join-Path $scripts 'start.ps1') -ScriptArguments @('-Gui', '-NoBrowser') -Hidden -Description 'Start ShareCraft'
        } elseif (Test-Path -LiteralPath $paths.StartupShortcut) {
            Remove-Item -LiteralPath $paths.StartupShortcut -Force
        }

        $address = Start-ShareCraft -Paths $paths -NoBrowser:$NoBrowser
        $done = $true
    } finally {
        if (-not $done -and -not $existing -and $createdRoot) {
            # A first install that failed half way: take back what it made, so it can simply be run again.
            Write-Step 'Taking back the half-finished install...'
            try { Stop-ShareCraft -Paths $paths -PgBin $packagePg } catch { Write-Step "Could not stop everything: $($_.Exception.Message)" }
            try { Remove-ShareCraft -Paths $paths -DeleteData } catch { Write-Step "Could not remove everything: $($_.Exception.Message)" }
        }
    }

    Write-Step ''
    Write-Step "ShareCraft is installed and running at $address"
    if (-not $reusing) { Write-Step "Sign in there as $AdminEmail with the password you just chose." }
    Write-Step 'Start it again later from the Start menu: ShareCraft. Stop it with Stop ShareCraft.'
    Write-Step "The guide is $(Join-Path $paths.Program 'README.html')"
}

try {
    Install-ShareCraft
    exit 0
} catch {
    Write-Host ''
    Write-Host "ShareCraft was not installed: $($_.Exception.Message)" -ForegroundColor Red
    exit 1
}
