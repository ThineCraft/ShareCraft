# Self-tests for the ShareCraft Windows scripts. They need neither Pester nor a built package:
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File packaging\windows\tests\scripts.Tests.ps1
#
# Every test works in its own temporary folder, whose path has a space and a non-ASCII letter,
# with LOCALAPPDATA and APPDATA pointed into it, so this computer's own ShareCraft (if any) and
# its Start menu are never touched. Give -Package <folder> to also check a built package folder
# for files that must never be shipped.
#
# This file is kept ASCII; non-ASCII letters are made with [char].
param([string]$Package)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:Passed = 0
$script:Failures = New-Object System.Collections.ArrayList
$script:Here = Split-Path -Parent $MyInvocation.MyCommand.Path
$script:TestRoot = Join-Path $env:TEMP ('sharecraft-script-tests-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
$script:Accent = [string][char]0x00EB   # e with diaeresis, as in Zoe Smith
$script:Loaded = $false

try {
    . (Join-Path $script:Here '..\scripts\common.ps1')
    . (Join-Path $script:Here '..\scripts\public-address.ps1')
    $script:Loaded = $true
} catch {
    Write-Host "cannot load the Windows scripts: $($_.Exception.Message)"
}

# --- A small test harness ----------------------------------------------------------------------

function New-TestFolder {
    # A fresh folder with a space and a non-ASCII letter in its name.
    $path = Join-Path $script:TestRoot ([guid]::NewGuid().ToString('N').Substring(0, 6) + "\Zo$($script:Accent) Smith")
    New-Item -ItemType Directory -Force -Path $path | Out-Null
    $path
}

function Use-TestProfile {
    # Points LOCALAPPDATA and APPDATA into a test folder, as a different Windows user would have.
    param([Parameter(Mandatory)][string]$Folder)
    $env:LOCALAPPDATA = Join-Path $Folder 'AppData\Local'
    $env:APPDATA = Join-Path $Folder 'AppData\Roaming'
    New-Item -ItemType Directory -Force -Path $env:LOCALAPPDATA, $env:APPDATA | Out-Null
}

function Test {
    param([Parameter(Mandatory)][string]$Name, [Parameter(Mandatory)][scriptblock]$Body)
    $savedLocal = $env:LOCALAPPDATA
    $savedRoaming = $env:APPDATA
    $savedTemp = $env:TEMP
    $savedTmp = $env:TMP
    try {
        if (-not $script:Loaded) { throw 'scripts\common.ps1 could not be loaded' }
        & $Body
        $script:Passed++
        Write-Host "ok   $Name"
    } catch {
        [void]$script:Failures.Add($Name)
        Write-Host "FAIL $Name"
        Write-Host "     $($_.Exception.Message) (line $($_.InvocationInfo.ScriptLineNumber))"
    } finally {
        $env:LOCALAPPDATA = $savedLocal
        $env:APPDATA = $savedRoaming
        $env:TEMP = $savedTemp
        $env:TMP = $savedTmp
        foreach ($name in 'PGPASSFILE', 'DATABASE_URL', 'PGHOST', 'PGHOSTADDR', 'PGPORT', 'PGUSER', 'PGPASSWORD', 'PGDATABASE', 'PGSERVICE',
            'SHARECRAFT_PUBLIC_URL', 'SHARECRAFT_TRUSTED_PROXIES', 'SHARECRAFT_HSTS_SECONDS') {
            Remove-Item "Env:$name" -ErrorAction SilentlyContinue
        }
    }
}

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw "expected: $Message" }
}

function Assert-Equal {
    param($Expected, $Actual, [string]$Message)
    if ($Expected -cne $Actual) { throw "$Message`: expected [$Expected], got [$Actual]" }
}

function Assert-Throws {
    param([Parameter(Mandatory)][scriptblock]$Body, [string]$Pattern, [string]$Message)
    $thrown = $null
    try { & $Body } catch { $thrown = $_.Exception.Message }
    if ($null -eq $thrown) { throw "expected an error: $Message" }
    if ($Pattern -and $thrown -notmatch $Pattern) { throw "$Message`: the error [$thrown] does not match [$Pattern]" }
    $thrown
}

function Assert-NoSecret {
    # Never shows the secret itself, only where it was found.
    param([AllowEmptyString()][string]$Text, [Parameter(Mandatory)][string]$Secret, [string]$Where)
    if ($Text.Contains($Secret)) { throw "a secret appeared in $Where" }
}

function Get-FileSha {
    param([string]$Path)
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
}

function New-FakePackage {
    # A folder shaped like the package, with small stand-in files.
    param([Parameter(Mandatory)][string]$Folder, [string]$Marker = 'v1')
    foreach ($file in 'bin\sharecraft-server.exe', 'bin\xtask.exe', 'pgsql\bin\initdb.exe',
        'pgsql\bin\pg_ctl.exe', 'pgsql\bin\psql.exe', 'pgsql\bin\postgres.exe',
        'web\index.html', 'editors\gridcraft\index.html', 'scripts\common.ps1', 'README.html') {
        $path = Join-Path $Folder $file
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $path) | Out-Null
        [System.IO.File]::WriteAllText($path, "$Marker $file")
    }
}

# --- Secrets ---------------------------------------------------------------------------------

Test 'a random secret is 32 letters and digits, different each time' {
    $a = New-RandomSecret
    $b = New-RandomSecret
    Assert-Equal 32 $a.Length 'length'
    Assert-True ($a -cmatch '^[A-Za-z0-9]{32}$') 'only letters and digits'
    Assert-True ($a -cne $b) 'two secrets differ'
}

# --- Ports -----------------------------------------------------------------------------------

Test 'port picking skips a port that is in use' {
    $busy = New-Object System.Net.Sockets.TcpListener ([System.Net.IPAddress]::Loopback), 0
    $busy.Start()
    try {
        $port = ([System.Net.IPEndPoint]$busy.LocalEndpoint).Port
        Assert-True (-not (Test-PortFree -Port $port)) 'a listening port is not free'
        $candidates = @($port) + (($port + 1)..($port + 20) | Where-Object { $_ -le 65535 })
        $picked = Find-FreePort -Ports $candidates
        Assert-True ($picked -ne $port) 'the busy port is skipped'
        Assert-True ($candidates -contains $picked) 'the port comes from the range'
        Assert-True (Test-PortFree -Port $picked) 'the picked port is free'
    } finally {
        $busy.Stop()
    }
}

Test 'a port held on every address is not free either' {
    $busy = New-Object System.Net.Sockets.TcpListener ([System.Net.IPAddress]::Any), 0
    $busy.Start()
    try {
        $port = ([System.Net.IPEndPoint]$busy.LocalEndpoint).Port
        Assert-True (-not (Test-PortFree -Port $port)) 'a port listened on 0.0.0.0 is not free'
    } finally {
        $busy.Stop()
    }
}

Test 'port picking says plainly when the whole range is in use' {
    $busy = New-Object System.Net.Sockets.TcpListener ([System.Net.IPAddress]::Loopback), 0
    $busy.Start()
    try {
        $port = ([System.Net.IPEndPoint]$busy.LocalEndpoint).Port
        Assert-Throws { Find-FreePort -Ports @($port) -What 'ShareCraft' } "no free port.*$port" 'all busy' | Out-Null
    } finally {
        $busy.Stop()
    }
}

# --- Quoting ---------------------------------------------------------------------------------

$script:EchoExe = $null
function Get-EchoExe {
    # A tiny program that writes each argument it was given, base64 of its UTF-8 bytes, one
    # per line: the arguments exactly as Windows split the command line.
    if ($script:EchoExe) { return $script:EchoExe }
    $folder = New-TestFolder
    $exe = Join-Path $folder 'echo args.exe'
    $source = @'
using System; using System.IO; using System.Text;
public static class EchoArgs {
    public static void Main(string[] args) {
        var lines = new StringBuilder();
        foreach (var a in args) { lines.AppendLine(Convert.ToBase64String(Encoding.UTF8.GetBytes(a))); }
        File.WriteAllText(Environment.GetEnvironmentVariable("ECHO_ARGS_OUT"), lines.ToString());
    }
}
'@
    Add-Type -TypeDefinition $source -OutputAssembly $exe -OutputType ConsoleApplication
    $script:EchoExe = $exe
    $exe
}

Test 'paths with spaces and non-ASCII letters are quoted so a program gets them whole' {
    $exe = Get-EchoExe
    $values = @(
        'plain',
        'with space',
        "C:\Users\Zo$($script:Accent) Smith\AppData\Local\ShareCraft\db",
        'C:\folder with space\',
        'C:\no-space\',
        'say "hello"',
        "tab`there",
        '',
        'trailing\\"quote',
        "--email=zo$($script:Accent)@example.test"
    )
    $out = Join-Path (Split-Path -Parent $exe) 'args.txt'
    $env:ECHO_ARGS_OUT = $out
    try {
        $line = Join-CommandLine -Arguments $values
        $p = Start-Process -FilePath $exe -ArgumentList $line -NoNewWindow -PassThru
        $null = $p.Handle
        $p.WaitForExit()
        Assert-Equal 0 $p.ExitCode 'echo exit code'
        $got = @([System.IO.File]::ReadAllLines($out) | Where-Object { $_ -ne $null } |
            ForEach-Object { [System.Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($_)) })
        Assert-Equal $values.Count $got.Count 'argument count'
        for ($i = 0; $i -lt $values.Count; $i++) {
            Assert-Equal $values[$i] $got[$i] "argument $i"
        }
    } finally {
        Remove-Item Env:ECHO_ARGS_OUT -ErrorAction SilentlyContinue
    }
}

Test 'native programs started by the scripts get quoted paths too' {
    $exe = Get-EchoExe
    $folder = New-TestFolder
    $out = Join-Path $folder 'args.txt'
    $env:ECHO_ARGS_OUT = $out
    try {
        $arg = Join-Path $folder 'data dir'
        $result = Invoke-Native -FilePath $exe -Arguments @('-D', $arg) -What 'echo'
        Assert-Equal 0 $result.ExitCode 'exit code'
        $got = @([System.IO.File]::ReadAllLines($out) |
            ForEach-Object { [System.Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($_)) })
        Assert-Equal '-D' $got[0] 'first argument'
        Assert-Equal $arg $got[1] 'path argument'
    } finally {
        Remove-Item Env:ECHO_ARGS_OUT -ErrorAction SilentlyContinue
    }
}

Test 'a shortcut keeps a script path with spaces and non-ASCII letters whole' {
    $folder = New-TestFolder
    $script = Join-Path $folder 'Programs\ShareCraft\scripts\start.ps1'
    $link = Join-Path $folder "Start Menu\ShareCraft\Share$($script:Accent).lnk"
    New-ShareCraftShortcut -Path $link -Script $script -ScriptArguments @('-Gui') -Hidden -Description 'Start ShareCraft'
    Assert-True (Test-Path -LiteralPath $link) 'the shortcut exists'
    $shell = New-Object -ComObject WScript.Shell
    $read = $shell.CreateShortcut($link)
    Assert-True ($read.TargetPath -like '*\powershell.exe') 'the target is Windows PowerShell'
    Assert-True ($read.Arguments.Contains("-File `"$script`"")) "the script path is quoted whole: $($read.Arguments)"
    Assert-True ($read.Arguments.Contains('-WindowStyle Hidden')) 'hidden'
    Assert-True ($read.Arguments.EndsWith('-Gui')) 'the script argument follows'
    # Never a folder inside the program: Windows will not delete a process's working folder,
    # so Uninstall run from its shortcut could not remove the program.
    Assert-Equal $env:LOCALAPPDATA $read.WorkingDirectory 'working folder'
}

Test 'a path with non-ASCII letters reaches the database tools in plain ASCII, or is refused plainly' {
    # initdb writes some paths into SQL in a UTF-8 database, so even a letter the Windows code
    # page holds (such as e-diaeresis) breaks it: every path given to the PostgreSQL tools must
    # be plain ASCII.
    $folder = New-TestFolder
    $plain = Join-Path $script:TestRoot 'plain'
    New-Item -ItemType Directory -Force -Path $plain | Out-Null
    Assert-Equal $plain (ConvertTo-PgSafePath -Path $plain) 'an ASCII path is kept'
    foreach ($name in "db", ([string][char]0x5F20 + [char]0x4F1F)) {
        $odd = Join-Path $folder $name
        New-Item -ItemType Directory -Force -Path $odd | Out-Null
        $converted = $null
        $message = $null
        try { $converted = ConvertTo-PgSafePath -Path $odd } catch { $message = $_.Exception.Message }
        if ($null -ne $converted) {
            Assert-True ($converted -match '^[ -~]+$') "the path given is plain ASCII: $converted"
            Assert-True (Test-Path -LiteralPath $converted) 'it names the same folder'
            [System.IO.File]::WriteAllText((Join-Path $odd 'marker.txt'), $name)
            Assert-Equal $name ([System.IO.File]::ReadAllText((Join-Path $converted 'marker.txt'))) 'it is the same folder'
        } else {
            Assert-True ($message -match 'cannot use the folder') "a plain refusal, got: $message"
        }
    }
}

# --- The .env file and its access list ---------------------------------------------------------

Test '.env is written with an access list narrowed to this user' {
    $folder = New-TestFolder
    $path = Join-Path $folder '.env'
    # A file that already exists, readable by everyone, is narrowed too.
    [System.IO.File]::WriteAllText($path, 'old')
    $secret = New-RandomSecret
    $url = New-DatabaseUrl -Password $secret -Port 5440
    Write-EnvFile -Path $path -DatabaseUrl $url
    $bytes = [System.IO.File]::ReadAllBytes($path)
    Assert-True (-not ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB)) 'no byte order mark'
    $text = [System.Text.Encoding]::UTF8.GetString($bytes)
    Assert-True ($text -ceq "DATABASE_URL=$url`n") 'the file holds DATABASE_URL only'
    Assert-True ($url -ceq "postgres://sharecraft:$secret@127.0.0.1:5440/sharecraft") 'the address form'
    Assert-True ($url -ceq (Read-EnvDatabaseUrl -Path $path)) 'read back (not shown: it holds a password)'

    $acl = Get-Acl -LiteralPath $path
    Assert-True $acl.AreAccessRulesProtected 'inheritance is removed'
    $me = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
    $rules = @($acl.GetAccessRules($true, $true, [System.Security.Principal.SecurityIdentifier]))
    Assert-Equal 1 $rules.Count 'one access rule'
    Assert-Equal $me.Value $rules[0].IdentityReference.Value 'the rule is for this user'
    Assert-Equal 'Allow' ([string]$rules[0].AccessControlType) 'allow'
    Assert-True (($rules[0].FileSystemRights -band [System.Security.AccessControl.FileSystemRights]::FullControl) -eq [System.Security.AccessControl.FileSystemRights]::FullControl) 'full control'
}

# --- Secrets never reach the console ----------------------------------------------------------

Test 'secrets never reach the console or the files around them' {
    $folder = New-TestFolder
    Use-TestProfile -Folder $folder
    $paths = Get-ShareCraftPaths
    Initialize-StateFolders -Paths $paths
    $secret = New-RandomSecret
    $seenInside = $false
    $output = & {
        Write-EnvFile -Path $paths.EnvFile -DatabaseUrl (New-DatabaseUrl -Password $secret -Port 5441)
        Write-SettingsFile -Path $paths.SettingsFile -AppPort 8790 -PgPort 5441 -IcapPort 1345
        Invoke-WithPgPass -Folder $paths.Run -Port 5441 -User 'postgres' -Password $secret -ScriptBlock {
            $file = $env:PGPASSFILE
            Write-Step "using the password file"
            $script:PgPassSeen = [System.IO.File]::ReadAllText($file).Contains($secret)
        }
        Write-Step 'done'
    } *>&1 | Out-String
    Assert-True $script:PgPassSeen 'the password file held the password while in use'
    Assert-NoSecret -Text $output -Secret $secret -Where 'the console output'
    Assert-True ($output -match 'done') 'output was captured'
    Assert-True ($null -eq $env:PGPASSFILE) 'PGPASSFILE is cleared afterwards'
    Assert-Equal 0 @(Get-ChildItem -LiteralPath $paths.Run -Force).Count 'the password file is deleted'
    Assert-NoSecret -Text ([System.IO.File]::ReadAllText($paths.SettingsFile)) -Secret $secret -Where 'settings.cmd'
}

Test 'the password file is deleted even when the step fails' {
    $folder = New-TestFolder
    $secret = New-RandomSecret
    Assert-Throws {
        Invoke-WithPgPass -Folder $folder -Port 5441 -User 'sharecraft' -Password $secret -ScriptBlock { throw 'step failed' }
    } 'step failed' 'the error comes through' | Out-Null
    Assert-Equal 0 @(Get-ChildItem -LiteralPath $folder -Force).Count 'no password file is left'
    Assert-True ($null -eq $env:PGPASSFILE) 'PGPASSFILE is cleared'
}

Test 'a failing step that handled a secret does not show its output' {
    $folder = New-TestFolder
    $secret = New-RandomSecret
    $ps = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    $command = "[Console]::Out.WriteLine('out $secret'); [Console]::Error.WriteLine('err $secret'); exit 3"
    $quiet = Assert-Throws {
        Invoke-Native -FilePath $ps -Arguments @('-NoProfile', '-Command', $command) -What 'the database step' -Secret -OutputFolder $folder
    } 'the database step failed \(exit code 3\)' 'a secret step fails'
    Assert-NoSecret -Text $quiet -Secret $secret -Where 'the error message'
    Assert-Equal 0 @(Get-ChildItem -LiteralPath $folder -Force).Count 'its output files are deleted'
    # Without -Secret the output is shown, so the test above is not empty.
    $loud = Assert-Throws {
        Invoke-Native -FilePath $ps -Arguments @('-NoProfile', '-Command', "[Console]::Error.WriteLine('visible'); exit 4") -What 'a plain step' -OutputFolder $folder
    } 'exit code 4' 'a plain step fails'
    Assert-True ($loud -match 'visible') 'a plain step shows what it said'
}

Test 'settings round-trip with and without a public address' {
    $folder = New-TestFolder
    Use-TestProfile -Folder $folder
    $paths = Get-ShareCraftPaths
    Initialize-StateFolders -Paths $paths
    Write-SettingsFile -Path $paths.SettingsFile -AppPort 8791 -PgPort 5442 -IcapPort 1346
    $bytes = [System.IO.File]::ReadAllBytes($paths.SettingsFile)
    Assert-True (@($bytes | Where-Object { $_ -gt 127 }).Count -eq 0) 'the file is plain ASCII, so cmd reads it on any code page'
    $s = Read-SettingsFile -Path $paths.SettingsFile
    Assert-Equal '127.0.0.1:8791' $s.SHARECRAFT_ADDR 'address'
    Assert-Equal $paths.Data $s.SHARECRAFT_DATA 'data folder'
    Assert-Equal (Join-Path $paths.Program 'web') $s.SHARECRAFT_WEB 'web folder'
    Assert-Equal (Join-Path $paths.Program 'editors') $s.SHARECRAFT_EDITORS_DIR 'editors folder'
    Assert-Equal '5442' $s.SHARECRAFT_PG_PORT 'database port'
    Assert-Equal '1346' $s.SHARECRAFT_ICAP_PORT 'scanner port'
    Assert-True ($s.SHARECRAFT_DATA.Contains("Zo$($script:Accent) Smith")) 'the non-ASCII folder comes back'
    Assert-True (-not $s.ContainsKey('SHARECRAFT_PUBLIC_URL')) 'development mode has no public address'
    Assert-True (-not $s.ContainsKey('SHARECRAFT_TRUSTED_PROXIES')) 'development mode has no trusted proxies setting'
    Assert-True (-not $s.ContainsKey('SHARECRAFT_HSTS_SECONDS')) 'development mode has no HSTS setting'

    Write-SettingsFile -Path $paths.SettingsFile -AppPort 8791 -PgPort 5442 -IcapPort 1346 `
        -PublicUrl 'https://share.example.com:8443/' -TrustedProxies '127.0.0.1, ::1, 10.0.0.0/24' -HstsSeconds '300'
    $s = Read-SettingsFile -Path $paths.SettingsFile
    Assert-Equal 'https://share.example.com:8443/' $s.SHARECRAFT_PUBLIC_URL 'public address'
    Assert-Equal '127.0.0.1, ::1, 10.0.0.0/24' $s.SHARECRAFT_TRUSTED_PROXIES 'trusted proxies'
    Assert-Equal '300' $s.SHARECRAFT_HSTS_SECONDS 'HSTS seconds'
}

Test 'public address is checked before anything is written' {
    $folder = New-TestFolder
    Use-TestProfile -Folder $folder
    $paths = Get-ShareCraftPaths
    Initialize-StateFolders -Paths $paths
    Write-SettingsFile -Path $paths.SettingsFile -AppPort 8791 -PgPort 5442 -IcapPort 1346
    $before = Get-FileSha $paths.SettingsFile
    $script:PublicAddressRestarts = 0
    function Restart-ShareCraftForPublicAddress { $script:PublicAddressRestarts++ }
    foreach ($bad in 'http://share.example.com', 'https://share.example.com/a', 'https://share example.com',
        'https://share.example.com"', 'https://share.example.com%', 'https://Share.example.com',
        'https://share.example.com?x=1', 'https://share.example.com#x', 'https://me@share.example.com',
        'https://share.example.com:0') {
        Assert-Throws { Invoke-PublicAddress -Paths $paths -Url $bad } 'valid HTTPS public address' "refuse $bad" | Out-Null
        Assert-Equal $before (Get-FileSha $paths.SettingsFile) "settings unchanged for $bad"
    }
    foreach ($bad in '127.0.0.1%', '1.2.3', '10.0.0.0/7', '::/0') {
        Assert-Throws { Invoke-PublicAddress -Paths $paths -Url 'https://share.example.com' -TrustedProxies $bad } `
            'comma-separated list' "bad trusted proxies $bad" | Out-Null
        Assert-Equal $before (Get-FileSha $paths.SettingsFile) "settings unchanged for bad trusted proxies $bad"
    }
    Assert-Throws { Invoke-PublicAddress -Paths $paths -Url 'https://share.example.com' -HstsSeconds '3.5' } `
        'whole number' 'bad HSTS seconds' | Out-Null
    Assert-Equal $before (Get-FileSha $paths.SettingsFile) 'settings unchanged for bad HSTS seconds'
    Assert-Equal 0 $script:PublicAddressRestarts 'invalid addresses never restart ShareCraft'
}

Test 'public address command restarts and explains the proxy settings' {
    $folder = New-TestFolder
    Use-TestProfile -Folder $folder
    $paths = Get-ShareCraftPaths
    Initialize-StateFolders -Paths $paths
    Write-SettingsFile -Path $paths.SettingsFile -AppPort 8791 -PgPort 5442 -IcapPort 1346
    $script:PublicAddressRestarts = 0
    function Restart-ShareCraftForPublicAddress { param($Paths) $script:PublicAddressRestarts++ }
    $output = & {
        Invoke-PublicAddress -Paths $paths -Url 'https://share.example.com:8443/' `
            -TrustedProxies '127.0.0.1, ::1' -HstsSeconds '300'
        Invoke-PublicAddress -Paths $paths -Status
    } *>&1 | Out-String
    $s = Read-SettingsFile -Path $paths.SettingsFile
    Assert-Equal 'https://share.example.com:8443/' $s.SHARECRAFT_PUBLIC_URL 'public address written'
    Assert-Equal '127.0.0.1, ::1' $s.SHARECRAFT_TRUSTED_PROXIES 'trusted proxies written'
    Assert-Equal '300' $s.SHARECRAFT_HSTS_SECONDS 'HSTS seconds written'
    Assert-Equal 1 $script:PublicAddressRestarts 'ShareCraft restarted once; status did not restart it'
    foreach ($expected in 'http://127.0.0.1:8791', 'Host', 'X-Forwarded-Host', 'X-Forwarded-For',
        'X-Forwarded-Proto: https', 'WebSocket', '9 MB', '8 MiB') {
        Assert-True ($output.Contains($expected)) "instructions mention $expected"
    }
}

Test 'off removes production settings' {
    $folder = New-TestFolder
    Use-TestProfile -Folder $folder
    $paths = Get-ShareCraftPaths
    Initialize-StateFolders -Paths $paths
    Write-SettingsFile -Path $paths.SettingsFile -AppPort 8791 -PgPort 5442 -IcapPort 1346 `
        -PublicUrl 'https://share.example.com' -TrustedProxies '127.0.0.1, ::1' -HstsSeconds '300'
    $script:PublicAddressRestarts = 0
    function Restart-ShareCraftForPublicAddress { param($Paths) $script:PublicAddressRestarts++ }
    Invoke-PublicAddress -Paths $paths -Off
    $s = Read-SettingsFile -Path $paths.SettingsFile
    Assert-True (-not $s.ContainsKey('SHARECRAFT_PUBLIC_URL')) 'public address removed'
    Assert-True (-not $s.ContainsKey('SHARECRAFT_TRUSTED_PROXIES')) 'trusted proxies removed'
    Assert-True (-not $s.ContainsKey('SHARECRAFT_HSTS_SECONDS')) 'HSTS seconds removed'
    Assert-Equal '127.0.0.1:8791' $s.SHARECRAFT_ADDR 'local address kept'
    Assert-Equal 1 $script:PublicAddressRestarts 'ShareCraft restarted once'
}

Test 'settings still refuse a non-loopback SHARECRAFT_ADDR' {
    $folder = New-TestFolder
    Use-TestProfile -Folder $folder
    $paths = Get-ShareCraftPaths
    Initialize-StateFolders -Paths $paths
    Write-SettingsFile -Path $paths.SettingsFile -AppPort 8791 -PgPort 5442 -IcapPort 1346 `
        -PublicUrl 'https://share.example.com'
    $text = [System.IO.File]::ReadAllText($paths.SettingsFile).Replace('127.0.0.1:8791', '0.0.0.0:8791')
    [System.IO.File]::WriteAllText($paths.SettingsFile, $text, [System.Text.Encoding]::ASCII)
    Assert-Throws { Read-SettingsFile -Path $paths.SettingsFile } '127\.0\.0\.1:<port>' 'non-loopback listen address' | Out-Null
}

Test 'server environment and browser address use production settings' {
    $settings = @{
        SHARECRAFT_ADDR = '127.0.0.1:8791'
        SHARECRAFT_DATA = 'C:\data'
        SHARECRAFT_WEB = 'C:\web'
        SHARECRAFT_EDITORS_DIR = 'C:\editors'
        SHARECRAFT_PUBLIC_URL = 'https://share.example.com:8443/'
        SHARECRAFT_TRUSTED_PROXIES = '127.0.0.1, ::1'
        SHARECRAFT_HSTS_SECONDS = '300'
    }
    Set-ServerEnvironment -Settings $settings
    Assert-Equal 'https://share.example.com:8443/' $env:SHARECRAFT_PUBLIC_URL 'server public address'
    Assert-Equal '127.0.0.1, ::1' $env:SHARECRAFT_TRUSTED_PROXIES 'server trusted proxies'
    Assert-Equal '300' $env:SHARECRAFT_HSTS_SECONDS 'server HSTS seconds'
    Assert-Equal 'https://share.example.com:8443/' (Get-ShareCraftBrowserAddress -Settings $settings) 'production browser address'

    $settings.Remove('SHARECRAFT_PUBLIC_URL')
    $settings.Remove('SHARECRAFT_TRUSTED_PROXIES')
    $settings.Remove('SHARECRAFT_HSTS_SECONDS')
    Set-ServerEnvironment -Settings $settings
    Assert-True ($null -eq $env:SHARECRAFT_PUBLIC_URL) 'stale public address removed from environment'
    Assert-True ($null -eq $env:SHARECRAFT_TRUSTED_PROXIES) 'stale trusted proxies removed from environment'
    Assert-True ($null -eq $env:SHARECRAFT_HSTS_SECONDS) 'stale HSTS seconds removed from environment'
    Assert-Equal 'http://127.0.0.1:8791/' (Get-ShareCraftBrowserAddress -Settings $settings) 'development browser address'
}

# --- Reinstalling and uninstalling -------------------------------------------------------------

function New-InstalledState {
    # What an earlier install leaves: a program, a database, data, .env and settings.
    param([Parameter(Mandatory)]$Paths)
    New-FakePackage -Folder $Paths.Program -Marker 'v1'
    [System.IO.File]::WriteAllText((Join-Path $Paths.Program 'bin\old-only.dll'), 'stale')
    Initialize-StateFolders -Paths $Paths
    New-Item -ItemType Directory -Force -Path $Paths.Db, (Join-Path $Paths.Data 'libraries\one.git') | Out-Null
    [System.IO.File]::WriteAllText((Join-Path $Paths.Db 'PG_VERSION'), "18`n")
    [System.IO.File]::WriteAllText((Join-Path $Paths.Db 'base.dat'), 'database bytes')
    [System.IO.File]::WriteAllText((Join-Path $Paths.Data 'libraries\one.git\HEAD'), 'ref: refs/heads/main')
    Write-EnvFile -Path $Paths.EnvFile -DatabaseUrl (New-DatabaseUrl -Password (New-RandomSecret) -Port 5443)
    Write-SettingsFile -Path $Paths.SettingsFile -AppPort 8792 -PgPort 5443 -IcapPort 1347
}

Test 'a second install is refused unless -Reinstall is given' {
    $folder = New-TestFolder
    Use-TestProfile -Folder $folder
    $paths = Get-ShareCraftPaths
    Assert-True (-not (Test-ShareCraftInstalled -Paths $paths)) 'nothing installed yet'
    Assert-InstallAllowed -Paths $paths
    New-InstalledState -Paths $paths
    Assert-True (Test-ShareCraftInstalled -Paths $paths) 'installed'
    Assert-Throws { Assert-InstallAllowed -Paths $paths } 'already installed.*-Reinstall' 'a second install' | Out-Null
    Assert-InstallAllowed -Paths $paths -Reinstall
}

Test 'a reinstall replaces the program and keeps the data, database and .env' {
    $folder = New-TestFolder
    Use-TestProfile -Folder $folder
    $paths = Get-ShareCraftPaths
    New-InstalledState -Paths $paths
    $kept = @{}
    foreach ($f in (Get-ChildItem -LiteralPath $paths.Root -Recurse -File -Force)) { $kept[$f.FullName] = Get-FileSha $f.FullName }
    $envAcl = (Get-Acl -LiteralPath $paths.EnvFile).Sddl

    $package = Join-Path $folder 'Downloads\ShareCraft-0.2.0-windows'
    New-FakePackage -Folder $package -Marker 'v2'
    Assert-InstallAllowed -Paths $paths -Reinstall
    Copy-ShareCraftProgram -Source $package -Destination $paths.Program
    Initialize-StateFolders -Paths $paths

    Assert-Equal 'v2 bin\sharecraft-server.exe' ([System.IO.File]::ReadAllText((Join-Path $paths.Program 'bin\sharecraft-server.exe'))) 'the program is the new one'
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $paths.Program 'bin\old-only.dll'))) 'files of the old program are gone'
    foreach ($name in $kept.Keys) {
        Assert-True (Test-Path -LiteralPath $name) "kept: $name"
        Assert-Equal $kept[$name] (Get-FileSha $name) "unchanged: $name"
    }
    Assert-Equal $envAcl (Get-Acl -LiteralPath $paths.EnvFile).Sddl '.env keeps its access list'
}

Test 'reinstall keeps the public address' {
    $folder = New-TestFolder
    Use-TestProfile -Folder $folder
    $paths = Get-ShareCraftPaths
    Initialize-StateFolders -Paths $paths
    Write-SettingsFile -Path $paths.SettingsFile -AppPort 8792 -PgPort 5443 -IcapPort 1347 `
        -PublicUrl 'https://share.example.com' -TrustedProxies '127.0.0.1, ::1' -HstsSeconds '300'
    $old = Read-SettingsFile -Path $paths.SettingsFile
    Write-InstallSettingsFile -Path $paths.SettingsFile -AppPort 8793 -PgPort 5444 -IcapPort 1348 -OldSettings $old
    $new = Read-SettingsFile -Path $paths.SettingsFile
    Assert-Equal 'https://share.example.com' $new.SHARECRAFT_PUBLIC_URL 'public address kept'
    Assert-Equal '127.0.0.1, ::1' $new.SHARECRAFT_TRUSTED_PROXIES 'trusted proxies kept'
    Assert-Equal '300' $new.SHARECRAFT_HSTS_SECONDS 'HSTS seconds kept'
    Assert-Equal '127.0.0.1:8793' $new.SHARECRAFT_ADDR 'new app port written'
}

Test 'a reinstall keeps the ports it had, and moves only a port now taken' {
    $folder = New-TestFolder
    Use-TestProfile -Folder $folder
    $paths = Get-ShareCraftPaths
    New-InstalledState -Paths $paths
    $busy = $null
    if (Test-PortFree -Port 8792) {
        $ports = Get-InstallPorts -Paths $paths
        Assert-Equal 8792 $ports.App 'app port kept'
        $busy = New-Object System.Net.Sockets.TcpListener ([System.Net.IPAddress]::Loopback), 8792
        $busy.Start()
    }
    try {
        $ports = Get-InstallPorts -Paths $paths
        Assert-True ($ports.App -ne 8792 -and $ports.App -ge 8787 -and $ports.App -le 8799) "a free app port from the range, got $($ports.App)"
        if (Test-PortFree -Port 5443) { Assert-Equal 5443 $ports.Pg 'database port kept' }
        Assert-True ($ports.Pg -ge 5440 -and $ports.Pg -le 5460) 'database port in range'
    } finally {
        if ($busy) { $busy.Stop() }
    }
}

Test 'a fresh install picks ports from the planned ranges' {
    $folder = New-TestFolder
    Use-TestProfile -Folder $folder
    $ports = Get-InstallPorts -Paths (Get-ShareCraftPaths)
    Assert-True ($ports.Pg -ge 5440 -and $ports.Pg -le 5460) 'database'
    Assert-True ($ports.App -ge 8787 -and $ports.App -le 8799) 'ShareCraft'
    Assert-True ($ports.Icap -ge 1344 -and $ports.Icap -le 1354) 'scanner'
}

Test 'a half-made earlier install is never reused silently' {
    $folder = New-TestFolder
    Use-TestProfile -Folder $folder
    $paths = Get-ShareCraftPaths
    Initialize-StateFolders -Paths $paths
    New-Item -ItemType Directory -Force -Path (Join-Path $paths.Data 'libraries') | Out-Null
    Assert-Throws { Assert-InstallAllowed -Paths $paths -Reinstall } 'no database' 'data without a database' | Out-Null
}

Test 'uninstall keeps the data unless deleting it was confirmed' {
    $folder = New-TestFolder
    Use-TestProfile -Folder $folder
    $paths = Get-ShareCraftPaths
    New-InstalledState -Paths $paths
    New-Item -ItemType Directory -Force -Path $paths.StartMenu | Out-Null
    [System.IO.File]::WriteAllText((Join-Path $paths.StartMenu 'ShareCraft.lnk'), 'x')
    $output = Remove-ShareCraft -Paths $paths *>&1 | Out-String
    Assert-True (-not (Test-Path -LiteralPath $paths.Program)) 'the program is removed'
    Assert-True (-not (Test-Path -LiteralPath $paths.StartMenu)) 'the shortcuts are removed'
    Assert-True (Test-Path -LiteralPath (Join-Path $paths.Db 'base.dat')) 'the database is kept'
    Assert-True (Test-Path -LiteralPath (Join-Path $paths.Data 'libraries\one.git\HEAD')) 'the data is kept'
    Assert-True (Test-Path -LiteralPath $paths.EnvFile) '.env is kept'
    Assert-True ($output -match 'kept') 'it says what was kept'
    Remove-ShareCraft -Paths $paths -DeleteData | Out-Null
    Assert-True (-not (Test-Path -LiteralPath $paths.Root)) 'confirmed: the data and database are deleted'
    Assert-True (Test-Path -LiteralPath $env:LOCALAPPDATA) 'nothing above ShareCraft is touched'
}

Test 'nothing is deleted when LOCALAPPDATA is missing or not a full path' {
    $env:LOCALAPPDATA = ''
    Assert-Throws { Get-ShareCraftPaths } 'LOCALAPPDATA' 'empty' | Out-Null
    $env:LOCALAPPDATA = 'relative\folder'
    Assert-Throws { Get-ShareCraftPaths } 'LOCALAPPDATA' 'relative' | Out-Null
}

# --- Stopping ----------------------------------------------------------------------------------

Test 'stop ends only the program it started, never another one with a reused process id' {
    $folder = New-TestFolder
    Use-TestProfile -Folder $folder
    $paths = Get-ShareCraftPaths
    Initialize-StateFolders -Paths $paths
    $ping = Join-Path $env:SystemRoot 'System32\PING.EXE'
    $pidFile = Join-Path $paths.Run 'server.pid'
    $expected = $paths.ServerExe

    # Another program holds the recorded id: it is left running and the stale record removed.
    $other = Start-Process -FilePath $ping -ArgumentList '-n 30 127.0.0.1' -WindowStyle Hidden -PassThru
    try {
        [System.IO.File]::WriteAllText($pidFile, [string]$other.Id)
        Stop-RecordedProcess -PidFile $pidFile -ExpectedPath $expected | Out-Null
        Assert-True (-not $other.HasExited) 'the unrelated program still runs'
        Assert-True (-not (Test-Path -LiteralPath $pidFile)) 'the stale record is removed'
    } finally {
        if (-not $other.HasExited) { $other.Kill() }
    }

    # The recorded program is ours: it is stopped.
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $expected) | Out-Null
    Copy-Item -LiteralPath $ping -Destination $expected
    $ours = Start-HiddenProcess -FilePath $expected -Arguments @('-n', '30', '127.0.0.1') -WorkingDirectory $paths.Root -LogBase (Join-Path $paths.Logs 'server') -PidFile $pidFile
    try {
        Assert-Equal ([string]$ours.Id) ([System.IO.File]::ReadAllText($pidFile).Trim()) 'the id is recorded'
        Assert-True ($null -ne (Get-RecordedProcess -PidFile $pidFile -ExpectedPath $expected)) 'it is found as ours'
        Stop-RecordedProcess -PidFile $pidFile -ExpectedPath $expected | Out-Null
        Assert-True $ours.HasExited 'our program is stopped'
        Assert-True (-not (Test-Path -LiteralPath $pidFile)) 'the record is removed'
    } finally {
        if (-not $ours.HasExited) { $ours.Kill() }
    }
}

Test 'a program started in the background can have its log read while it runs' {
    $folder = New-TestFolder
    $ps = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    $command = "[Console]::Out.WriteLine('ready'); [Console]::Out.Flush(); Start-Sleep -Seconds 30"
    $process = Start-HiddenProcess -FilePath $ps -Arguments @('-NoProfile', '-Command', $command) -WorkingDirectory $folder `
        -LogBase (Join-Path $folder 'program') -PidFile (Join-Path $folder 'program.pid')
    try {
        $log = Join-Path $folder 'program.out.log'
        $deadline = (Get-Date).AddSeconds(20)
        while (-not ([string](Get-Content -LiteralPath $log -Raw) -match 'ready') -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 200 }
        Assert-True ([string](Get-Content -LiteralPath $log -Raw) -match 'ready') 'the log can be read (as Notepad or Get-Content would)'
        Assert-True (-not $process.HasExited) 'while the program still runs'
    } finally {
        if (-not $process.HasExited) { $process.Kill() }
    }
}

# --- Fix round 1 -------------------------------------------------------------------------------

function Invoke-ChildPowerShell {
    # Runs a script file in a new Windows PowerShell, hidden, in the given working folder, with
    # this process's environment; answers the exit code.
    param([Parameter(Mandatory)][string]$Script, [string[]]$Arguments = @(), [Parameter(Mandatory)][string]$WorkingDirectory, [int]$TimeoutSeconds = 180)
    $ps = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    $line = Join-CommandLine -Arguments (@('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $Script) + $Arguments)
    $p = Start-Process -FilePath $ps -ArgumentList $line -WorkingDirectory $WorkingDirectory -WindowStyle Hidden -PassThru
    $null = $p.Handle
    if (-not $p.WaitForExit($TimeoutSeconds * 1000)) { $p.Kill(); throw "$Script did not finish within $TimeoutSeconds seconds" }
    $p.ExitCode
}

Test 'uninstall removes everything when its working folder is inside the program (as from its shortcut)' {
    $folder = New-TestFolder
    Use-TestProfile -Folder $folder
    $paths = Get-ShareCraftPaths
    New-InstalledState -Paths $paths
    # The fake package has no real pg_ctl; the database part of Stop is not what is tested here.
    Remove-Item -LiteralPath (Join-Path $paths.Program 'pgsql') -Recurse -Force
    foreach ($name in 'common.ps1', 'uninstall.ps1') {
        Copy-Item -LiteralPath (Join-Path $script:Here "..\scripts\$name") -Destination (Join-Path $paths.Program "scripts\$name") -Force
    }
    New-Item -ItemType Directory -Force -Path $paths.StartMenu | Out-Null
    [System.IO.File]::WriteAllText((Join-Path $paths.StartMenu 'ShareCraft.lnk'), 'x')
    $scripts = Join-Path $paths.Program 'scripts'
    $code = Invoke-ChildPowerShell -Script (Join-Path $scripts 'uninstall.ps1') -Arguments @('-Yes', '-DeleteData') -WorkingDirectory $scripts
    Assert-Equal 0 $code 'uninstall exit code'
    Assert-True (-not (Test-Path -LiteralPath $paths.Program)) 'the program folder is removed'
    Assert-True (-not (Test-Path -LiteralPath $paths.StartMenu)) 'the shortcuts are removed'
    Assert-True (-not (Test-Path -LiteralPath $paths.Root)) 'the confirmed data deletion is done'
}

Test 'native programs start when TEMP has letters outside the code page' {
    $folder = New-TestFolder
    $odd = Join-Path $folder ('tmp ' + [char]0x5F20 + [char]0x4F1F + " $($script:Accent)")
    New-Item -ItemType Directory -Force -Path $odd | Out-Null
    $out = Join-Path $folder 'launched.txt'
    $probe = Join-Path $folder 'probe.ps1'
    $common = (Resolve-Path (Join-Path $script:Here '..\scripts\common.ps1')).Path
    [System.IO.File]::WriteAllText($probe, @"
param([string]`$Common, [string]`$Out)
Set-StrictMode -Version Latest
`$ErrorActionPreference = 'Stop'
. `$Common
`$cmd = Join-Path `$env:SystemRoot 'System32\cmd.exe'
`$r = Invoke-Native -FilePath `$cmd -Arguments @('/c', 'echo', 'launched') -What 'echo'
[System.IO.File]::WriteAllText(`$Out, `$r.Output)
"@, (New-Object System.Text.UTF8Encoding $true))
    # A fresh process, so the launcher is compiled there with that TEMP.
    $env:TEMP = $odd
    $env:TMP = $odd
    $code = Invoke-ChildPowerShell -Script $probe -Arguments @('-Common', $common, '-Out', $out) -WorkingDirectory $folder
    Assert-Equal 0 $code 'exit code'
    Assert-True ((Test-Path -LiteralPath $out) -and ([System.IO.File]::ReadAllText($out) -match 'launched')) 'the program ran and its output came back'
}

Test 'a TEMP with letters outside the code page is changed to its plain-ASCII short name' {
    $folder = New-TestFolder
    $odd = Join-Path $folder ('tmp ' + [char]0x5F20 + [char]0x4F1F + " $($script:Accent)")
    New-Item -ItemType Directory -Force -Path $odd | Out-Null
    [System.IO.File]::WriteAllText((Join-Path $odd 'marker.txt'), 'same')
    $env:TEMP = $odd
    $env:TMP = $odd
    $message = $null
    try { Initialize-SafeTemp } catch { $message = $_.Exception.Message }
    if ($null -eq $message) {
        Assert-True ($env:TEMP -match '^[ -~]+$') "TEMP is plain ASCII: $env:TEMP"
        Assert-Equal $env:TEMP $env:TMP 'TMP follows TEMP'
        Assert-Equal 'same' ([System.IO.File]::ReadAllText((Join-Path $env:TEMP 'marker.txt'))) 'the same folder'
    } else {
        Assert-True ($message -match 'temporary folder') "a plain refusal, got: $message"
    }
}

Test '.env is never left empty or missing when writing it fails' {
    $folder = New-TestFolder
    $path = Join-Path $folder '.env'
    $old = New-DatabaseUrl -Password (New-RandomSecret) -Port 5440
    Write-EnvFile -Path $path -DatabaseUrl $old
    $before = [System.IO.File]::ReadAllText($path)
    # Narrowing the access list fails part way, as a killed icacls would.
    function Set-OwnerOnlyAccess { param([string]$Path, [switch]$Folder) throw 'narrowing failed' }
    Assert-Throws { Write-EnvFile -Path $path -DatabaseUrl (New-DatabaseUrl -Password (New-RandomSecret) -Port 5441) } 'narrowing failed' 'the failure comes through' | Out-Null
    Remove-Item Function:\Set-OwnerOnlyAccess
    Assert-True ($before -ceq [System.IO.File]::ReadAllText($path)) '.env still holds the old address (not shown: it holds a password)'
    Assert-Equal 1 @(Get-ChildItem -LiteralPath $folder -Force).Count 'no half-written file is left beside it'
    # And a write that succeeds replaces it whole, still narrowed, with nothing left over.
    $new = New-DatabaseUrl -Password (New-RandomSecret) -Port 5442
    Write-EnvFile -Path $path -DatabaseUrl $new
    Assert-True ($new -ceq (Read-EnvDatabaseUrl -Path $path)) 'the new address (not shown: it holds a password)'
    $rules = @((Get-Acl -LiteralPath $path).GetAccessRules($true, $true, [System.Security.Principal.SecurityIdentifier]))
    Assert-Equal 1 $rules.Count 'one access rule after replacing'
    Assert-Equal 1 @(Get-ChildItem -LiteralPath $folder -Force).Count 'only .env is there'
}

Test 'programs ShareCraft starts never see another DATABASE_URL or PG* setting' {
    $folder = New-TestFolder
    $env:DATABASE_URL = 'postgres://nobody:wrong@192.0.2.1:1/elsewhere'
    $env:PGHOST = '192.0.2.1'
    $env:PGPORT = '1'
    $env:PGPASSWORD = 'wrong'
    $env:PGSERVICE = 'elsewhere'
    Clear-DatabaseEnvironment
    $ps = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    $r = Invoke-Native -FilePath $ps -Arguments @('-NoProfile', '-Command', "(Get-ChildItem Env: | Where-Object { `$_.Name -match '^(PG|DATABASE_URL`$)' } | ForEach-Object Name) -join ','; 'end'") -What 'listing' -OutputFolder $folder
    Assert-Equal 'end' $r.Output.Trim() "the child saw: $($r.Output.Trim())"
    foreach ($name in 'install.ps1', 'start.ps1', 'stop.ps1', 'uninstall.ps1') {
        $text = [System.IO.File]::ReadAllText((Join-Path $script:Here "..\scripts\$name"))
        Assert-True ($text -match '(?m)^\s*Clear-DatabaseEnvironment') "$name clears them before it starts anything"
    }
    # Start clears them itself, also when called on its own.
    Use-TestProfile -Folder $folder
    $env:DATABASE_URL = 'postgres://nobody:wrong@192.0.2.1:1/elsewhere'
    try { Start-ShareCraft -Paths (Get-ShareCraftPaths) -NoBrowser | Out-Null } catch { }
    Assert-True ($null -eq $env:DATABASE_URL) 'Start-ShareCraft removed DATABASE_URL'
}

# --- The package -------------------------------------------------------------------------------

Test 'a package holding .env or data is noticed' {
    $folder = New-TestFolder
    $package = Join-Path $folder 'pkg'
    New-FakePackage -Folder $package
    Assert-Equal 0 @(Find-ForbiddenPackageEntries -Folder $package).Count 'a clean package'
    [System.IO.File]::WriteAllText((Join-Path $package 'web\.env'), 'x')
    New-Item -ItemType Directory -Force -Path (Join-Path $package 'data') | Out-Null
    $found = @(Find-ForbiddenPackageEntries -Folder $package)
    Assert-Equal 2 $found.Count 'both are found'
}

if ($Package) {
    Test "the package at $Package holds no .env, data or tests" {
        Assert-True (Test-Path -LiteralPath (Join-Path $Package 'bin\sharecraft-server.exe')) 'it is a package folder'
        $found = @(Find-ForbiddenPackageEntries -Folder $Package)
        Assert-Equal 0 $found.Count "forbidden entries: $($found -join ', ')"
    }
}

# --- Summary -----------------------------------------------------------------------------------

Remove-Item -LiteralPath $script:TestRoot -Recurse -Force -ErrorAction SilentlyContinue
Write-Host ''
Write-Host "$($script:Passed) passed, $($script:Failures.Count) failed"
if ($script:Failures.Count -gt 0) { exit 1 }
exit 0
