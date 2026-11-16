# Shared helpers for the ShareCraft Windows scripts (install, start, stop, uninstall).
# Dot-source it: . (Join-Path $PSScriptRoot 'common.ps1')
#
# Rules kept everywhere in this file:
# - Windows PowerShell 5.1, StrictMode, every native exit code checked.
# - No password, database address or .env content is ever printed or logged. Secrets reach the
#   PostgreSQL tools only through files that this Windows user alone can read (initdb --pwfile,
#   PGPASSFILE), deleted straight after use; never on a command line.
# - Every path is passed whole, so folders with spaces or letters such as e-diaeresis work.
# This file is kept ASCII so that Windows PowerShell reads it the same on every code page.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:PgPorts = 5440..5460
$script:AppPorts = 8787..8799
$script:IcapPorts = 1344..1354
$script:Utf8NoBom = New-Object System.Text.UTF8Encoding $false

# --- Messages ----------------------------------------------------------------------------------

function Write-Step {
    # One line of progress for the person watching. Never pass a secret.
    param([Parameter(Mandatory)][AllowEmptyString()][string]$Message)
    Write-Host $Message
}

function Read-YesNo {
    # Asks a yes/no question; anything but y or yes counts as the default.
    param([Parameter(Mandatory)][string]$Question, [bool]$Default = $false)
    $hint = if ($Default) { '[Y/n]' } else { '[y/N]' }
    $answer = Read-Host "$Question $hint"
    if ([string]::IsNullOrWhiteSpace($answer)) { return $Default }
    return ($answer.Trim() -match '^(y|yes)$')
}

# --- Where things live -------------------------------------------------------------------------

function Get-ShareCraftPaths {
    # Every folder and file the scripts use, for the signed-in Windows user. Read from
    # LOCALAPPDATA and APPDATA each time, so tests can point them elsewhere.
    $local = $env:LOCALAPPDATA
    if ([string]::IsNullOrWhiteSpace($local) -or -not [System.IO.Path]::IsPathRooted($local) -or
        -not ($local -match '^[A-Za-z]:\\|^\\\\')) {
        throw 'LOCALAPPDATA is not set to a full folder path, so ShareCraft cannot tell where it belongs. Nothing was changed.'
    }
    $roaming = $env:APPDATA
    if ([string]::IsNullOrWhiteSpace($roaming) -or -not [System.IO.Path]::IsPathRooted($roaming)) {
        throw 'APPDATA is not set to a full folder path, so the Start menu cannot be found. Nothing was changed.'
    }
    $local = $local.TrimEnd('\')
    $root = Join-Path $local 'ShareCraft'
    $program = Join-Path $local 'Programs\ShareCraft'
    $menu = Join-Path $roaming.TrimEnd('\') 'Microsoft\Windows\Start Menu\Programs'
    [pscustomobject]@{
        Root            = $root
        Program         = $program
        Db              = Join-Path $root 'db'
        Data            = Join-Path $root 'data'
        Logs            = Join-Path $root 'logs'
        Run             = Join-Path $root 'run'
        EnvFile         = Join-Path $root '.env'
        SettingsFile    = Join-Path $root 'settings.cmd'
        StartMenu       = Join-Path $menu 'ShareCraft'
        StartupShortcut = Join-Path $menu 'Startup\ShareCraft.lnk'
        ServerExe       = Join-Path $program 'bin\sharecraft-server.exe'
        XtaskExe        = Join-Path $program 'bin\xtask.exe'
        PgBin           = Join-Path $program 'pgsql\bin'
        ServerPid       = Join-Path $root 'run\server.pid'
        ScannerPid      = Join-Path $root 'run\scanner.pid'
    }
}

function Initialize-StateFolders {
    # Makes the folders under %LOCALAPPDATA%\ShareCraft that do not exist yet; never empties
    # one that does. The run folder, which briefly holds password files, is narrowed to this
    # user.
    param([Parameter(Mandatory)]$Paths)
    foreach ($folder in $Paths.Root, $Paths.Data, $Paths.Logs, $Paths.Run) {
        if (-not (Test-Path -LiteralPath $folder)) {
            New-Item -ItemType Directory -Path $folder | Out-Null
        }
    }
    Set-OwnerOnlyAccess -Path $Paths.Run -Folder
}

function Test-ShareCraftInstalled {
    # Whether anything of an earlier install is here: the program, the database, the data,
    # .env or the settings.
    param([Parameter(Mandatory)]$Paths)
    foreach ($item in $Paths.Program, $Paths.Db, $Paths.Data, $Paths.EnvFile, $Paths.SettingsFile) {
        if (Test-Path -LiteralPath $item) { return $true }
    }
    return $false
}

function Assert-InstallAllowed {
    # Refuses a second install unless -Reinstall is given, and refuses to reuse a half-made
    # earlier install (data without a database, or a database without its .env).
    param([Parameter(Mandatory)]$Paths, [switch]$Reinstall)
    if (-not (Test-ShareCraftInstalled -Paths $Paths)) { return }
    if (-not $Reinstall) {
        throw ("ShareCraft is already installed for this Windows user (in $($Paths.Root)). Nothing was changed. " +
            'To install this copy over it and keep all data and the database, run Install ShareCraft.cmd -Reinstall from a Command Prompt in this folder.')
    }
    $hasDb = Test-Path -LiteralPath (Join-Path $Paths.Db 'PG_VERSION')
    $hasEnv = Test-Path -LiteralPath $Paths.EnvFile
    $hasData = Test-Path -LiteralPath $Paths.Data
    if (-not $hasDb -and ($hasEnv -or $hasData)) {
        throw ("An earlier ShareCraft left data in $($Paths.Root) but no database, so it cannot be reused safely. Nothing was changed. " +
            'Move that folder somewhere else (or delete it if you do not need it), then install again.')
    }
    if ($hasDb -and -not $hasEnv) {
        throw ("The database in $($Paths.Db) is there but its .env file is missing, so ShareCraft cannot sign in to it. Nothing was changed. " +
            'Put back .env from a backup, or move the ShareCraft folder away and install again.')
    }
}

# --- Secrets -----------------------------------------------------------------------------------

function New-RandomSecret {
    # A random password of letters and digits from the system's cryptographic generator.
    param([int]$Length = 32)
    $alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789'
    $rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
    try {
        $chars = New-Object System.Text.StringBuilder
        $buffer = New-Object byte[] 64
        while ($chars.Length -lt $Length) {
            $rng.GetBytes($buffer)
            foreach ($b in $buffer) {
                # 248 is 4 x 62: higher bytes are skipped so every letter is equally likely.
                if ($b -lt 248 -and $chars.Length -lt $Length) { [void]$chars.Append($alphabet[$b % 62]) }
            }
        }
        $chars.ToString()
    } finally {
        $rng.Dispose()
    }
}

function Set-OwnerOnlyAccess {
    # Narrows a file's or folder's access list to the signed-in Windows user alone (no
    # inherited entries), with icacls.
    param([Parameter(Mandatory)][string]$Path, [switch]$Folder)
    $sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $grant = if ($Folder) { "*${sid}:(OI)(CI)(F)" } else { "*${sid}:(F)" }
    $icacls = Join-Path $env:SystemRoot 'System32\icacls.exe'
    Invoke-Native -FilePath $icacls -Arguments @($Path.TrimEnd('\'), '/inheritance:r', '/grant:r', $grant, '/Q') -What "Narrowing the access list of $Path" | Out-Null
    # icacls /grant:r replaces this user's entry but keeps other explicit ones: remove those.
    $acl = Get-Acl -LiteralPath $Path
    $changed = $false
    foreach ($rule in @($acl.GetAccessRules($true, $false, [System.Security.Principal.SecurityIdentifier]))) {
        if ($rule.IdentityReference.Value -ne $sid) {
            [void]$acl.RemoveAccessRuleSpecific($rule)
            $changed = $true
        }
    }
    if ($changed) { Set-Acl -LiteralPath $Path -AclObject $acl }
}

function Write-SecretFile {
    # Writes text holding a secret, UTF-8 without a BOM. A new file beside it is made empty,
    # narrowed to this user, and only then filled, so the secret never sits in a file others
    # can read; then it replaces the old file in one step. If anything fails, the old file
    # (for .env, the only copy of the database password) is left exactly as it was, never
    # empty or missing.
    param([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][string]$Content)
    $folder = Split-Path -Parent $Path
    $new = Join-Path $folder ('.' + [System.IO.Path]::GetFileName($Path) + '.new-' + [guid]::NewGuid().ToString('N'))
    try {
        [System.IO.File]::WriteAllText($new, '', $script:Utf8NoBom)
        Set-OwnerOnlyAccess -Path $new
        [System.IO.File]::WriteAllText($new, $Content, $script:Utf8NoBom)
        if (Test-Path -LiteralPath $Path) {
            # Narrow the old file too (its content untouched), so the swap never widens access.
            Set-OwnerOnlyAccess -Path $Path
            [System.IO.File]::Replace($new, $Path, [NullString]::Value)
        } else {
            [System.IO.File]::Move($new, $Path)
        }
    } finally {
        if (Test-Path -LiteralPath $new) { Remove-Item -LiteralPath $new -Force }
    }
}

function Clear-DatabaseEnvironment {
    # Removes DATABASE_URL and every PG* variable from this process's environment, so that
    # neither the server (whose own variables beat .env) nor psql, pg_ctl or initdb (which read
    # PGHOST, PGPORT, PGPASSWORD, PGSERVICE and the rest) can be sent to another database by
    # the user's own settings. Only this process and the programs it starts are affected.
    foreach ($item in @(Get-ChildItem Env:)) {
        if ($item.Name -match '^(PG|DATABASE_URL$)') { Remove-Item -LiteralPath "Env:$($item.Name)" }
    }
}

function Initialize-SafeTemp {
    # The helper that starts programs is compiled into TEMP, and the compiler cannot write to a
    # folder whose name has letters outside the Windows code page. Points TEMP and TMP (for
    # this process and what it starts) at the folder's plain-ASCII short name when needed.
    $temp = $env:TEMP
    if (-not $temp) { $temp = $env:TMP }
    if (-not $temp -or -not (Test-Path -LiteralPath $temp -PathType Container)) {
        throw "Windows' temporary folder for this user (TEMP) is not set or does not exist. Nothing was changed."
    }
    try {
        $safe = ConvertTo-PgSafePath -Path $temp
    } catch {
        throw ("Windows' temporary folder for this user, $temp, has letters other than plain English ones and this drive keeps no short (8.3) name for it, " +
            'so ShareCraft cannot prepare its helper there. Nothing was changed.')
    }
    $env:TEMP = $safe
    $env:TMP = $safe
}

function New-DatabaseUrl {
    # The address the server connects with. It holds the password: never print it.
    param([Parameter(Mandatory)][string]$Password, [Parameter(Mandatory)][int]$Port)
    "postgres://sharecraft:$Password@127.0.0.1:$Port/sharecraft"
}

function Write-EnvFile {
    # Writes %LOCALAPPDATA%\ShareCraft\.env, holding DATABASE_URL, readable by this user only.
    param([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][string]$DatabaseUrl)
    Write-SecretFile -Path $Path -Content "DATABASE_URL=$DatabaseUrl`n"
}

function Read-EnvDatabaseUrl {
    # The DATABASE_URL in .env, for the scripts' own use. Never print it.
    param([Parameter(Mandatory)][string]$Path)
    foreach ($line in [System.IO.File]::ReadAllLines($Path)) {
        if ($line -match '^\s*DATABASE_URL\s*=\s*(.+?)\s*$') { return $Matches[1] }
    }
    throw "$Path has no DATABASE_URL line."
}

function Get-UrlPassword {
    # The password inside a postgres:// address. Never print it.
    param([Parameter(Mandatory)][string]$Url)
    if ($Url -match '^postgres(?:ql)?://[^:/@]+:([^@]+)@') { return $Matches[1] }
    throw 'The database address in .env has no password in the form the installer writes.'
}

function Invoke-WithPgPass {
    # Runs a script block with PGPASSFILE naming a password file that only this user can read;
    # the file and the variable are removed afterwards, also when the block fails.
    param(
        [Parameter(Mandatory)][string]$Folder,
        [Parameter(Mandatory)][int]$Port,
        [Parameter(Mandatory)][string]$User,
        [Parameter(Mandatory)][string]$Password,
        [Parameter(Mandatory)][scriptblock]$ScriptBlock
    )
    $file = Join-Path $Folder ('pgpass-' + [guid]::NewGuid().ToString('N') + '.conf')
    try {
        Write-SecretFile -Path $file -Content "127.0.0.1:${Port}:*:${User}:$Password`n"
        $env:PGPASSFILE = ConvertTo-PgSafePath -Path $file
        & $ScriptBlock
    } finally {
        Remove-Item Env:PGPASSFILE -ErrorAction SilentlyContinue
        if (Test-Path -LiteralPath $file) { Remove-Item -LiteralPath $file -Force }
    }
}

# --- Command lines and paths -------------------------------------------------------------------

function ConvertTo-CommandLineArgument {
    # Quotes one argument the way Windows programs split their command line, so spaces,
    # quotes and trailing backslashes arrive unchanged.
    param([Parameter(Mandatory)][AllowEmptyString()][string]$Value)
    if ($Value.Length -gt 0 -and $Value -notmatch '[\s"]') { return $Value }
    $sb = New-Object System.Text.StringBuilder
    [void]$sb.Append('"')
    $backslashes = 0
    foreach ($c in $Value.ToCharArray()) {
        if ($c -eq [char]92) {
            $backslashes++
            continue
        }
        if ($c -eq [char]34) {
            [void]$sb.Append([char]92, 2 * $backslashes + 1)
        } elseif ($backslashes -gt 0) {
            [void]$sb.Append([char]92, $backslashes)
        }
        [void]$sb.Append($c)
        $backslashes = 0
    }
    if ($backslashes -gt 0) { [void]$sb.Append([char]92, 2 * $backslashes) }
    [void]$sb.Append('"')
    $sb.ToString()
}

function Join-CommandLine {
    # A whole command line from separate arguments, each quoted as needed.
    param([Parameter(Mandatory)][AllowEmptyCollection()][AllowEmptyString()][string[]]$Arguments)
    (@($Arguments) | ForEach-Object { ConvertTo-CommandLineArgument -Value $_ }) -join ' '
}

function ConvertTo-PgSafePath {
    # The PostgreSQL programs read their arguments in the Windows code page, and initdb writes
    # some paths into SQL in a UTF-8 database, so a folder name with any non-ASCII letter (even
    # e-diaeresis) breaks them. Such a path is given to them in its short 8.3 form, which is
    # plain ASCII, when Windows keeps one; otherwise it is refused plainly. The path must exist.
    param([Parameter(Mandatory)][string]$Path)
    if ($Path -match '^[ -~]+$') { return $Path }
    $fso = New-Object -ComObject Scripting.FileSystemObject
    $short = $null
    if (Test-Path -LiteralPath $Path -PathType Container) {
        $short = $fso.GetFolder($Path).ShortPath
    } elseif (Test-Path -LiteralPath $Path -PathType Leaf) {
        $short = $fso.GetFile($Path).ShortPath
    }
    if ($short -and $short -match '^[ -~]+$') { return $short }
    throw ("The database programs (PostgreSQL) cannot use the folder $Path, because its name has letters other than plain English ones " +
        'and this drive keeps no short (8.3) names for folders. Nothing more was changed.')
}

# --- Starting programs --------------------------------------------------------------------------

# Start-Process in Windows PowerShell 5.1 opens redirected output files so that nobody can read
# them while the program runs, and lets the program inherit every inheritable handle of this
# process; a program that leaves a server running (pg_ctl start) then holds them, and the
# console, for as long as the server runs, so the installer's window could not close (or
# closing it would end the database). This starts a program in a new, hidden console, its
# output going to files that can be read at any time, and gives it those handles and no others.
$script:LauncherSource = @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

public static class ShareCraftLauncher {
    [StructLayout(LayoutKind.Sequential)]
    struct SECURITY_ATTRIBUTES { public int nLength; public IntPtr lpSecurityDescriptor; public bool bInheritHandle; }
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct STARTUPINFO {
        public int cb; public string lpReserved; public string lpDesktop; public string lpTitle;
        public int dwX; public int dwY; public int dwXSize; public int dwYSize; public int dwXCountChars;
        public int dwYCountChars; public int dwFillAttribute; public int dwFlags; public short wShowWindow;
        public short cbReserved2; public IntPtr lpReserved2; public IntPtr hStdInput; public IntPtr hStdOutput;
        public IntPtr hStdError;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct STARTUPINFOEX { public STARTUPINFO StartupInfo; public IntPtr lpAttributeList; }
    [StructLayout(LayoutKind.Sequential)]
    struct PROCESS_INFORMATION { public IntPtr hProcess; public IntPtr hThread; public int dwProcessId; public int dwThreadId; }

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr CreateFileW(string name, uint access, uint share, ref SECURITY_ATTRIBUTES sa, uint disposition, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool InitializeProcThreadAttributeList(IntPtr list, int count, int flags, ref IntPtr size);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool UpdateProcThreadAttribute(IntPtr list, uint flags, IntPtr attribute, IntPtr value, IntPtr size, IntPtr previous, IntPtr returnSize);
    [DllImport("kernel32.dll")]
    static extern void DeleteProcThreadAttributeList(IntPtr list);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool CreateProcessW(string app, StringBuilder commandLine, IntPtr processAttributes, IntPtr threadAttributes,
        bool inheritHandles, uint flags, IntPtr environment, string directory, ref STARTUPINFOEX startup, out PROCESS_INFORMATION info);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetExitCodeProcess(IntPtr process, out int code);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool TerminateProcess(IntPtr process, uint code);
    [DllImport("kernel32.dll")]
    static extern bool CloseHandle(IntPtr handle);

    static IntPtr Open(string path, bool write) {
        var sa = new SECURITY_ATTRIBUTES { nLength = Marshal.SizeOf(typeof(SECURITY_ATTRIBUTES)), bInheritHandle = true };
        // Read, write and delete sharing, so a log can be read while the program runs.
        IntPtr h = CreateFileW(path, write ? 0x40000000u : 0x80000000u, 7u, ref sa, write ? 2u : 3u, 0x80u, IntPtr.Zero);
        if (h == new IntPtr(-1)) throw new Win32Exception(Marshal.GetLastWin32Error(), "cannot open " + path);
        return h;
    }

    // Starts the program and answers its process id. With wait, waits up to timeoutMs and
    // sets exitCode (-1 when it did not finish in time and was ended).
    public static int Start(string app, string commandLine, string directory, string outPath, string errPath,
                            bool wait, int timeoutMs, out int exitCode) {
        exitCode = 0;
        IntPtr input = Open("NUL", false), output = IntPtr.Zero, error = IntPtr.Zero, list = IntPtr.Zero, handles = IntPtr.Zero;
        bool listReady = false;
        try {
            output = Open(outPath, true);
            error = Open(errPath, true);
            IntPtr size = IntPtr.Zero;
            InitializeProcThreadAttributeList(IntPtr.Zero, 1, 0, ref size);
            list = Marshal.AllocHGlobal(size);
            if (!InitializeProcThreadAttributeList(list, 1, 0, ref size)) throw new Win32Exception(Marshal.GetLastWin32Error());
            listReady = true;
            handles = Marshal.AllocHGlobal(IntPtr.Size * 3);
            Marshal.WriteIntPtr(handles, 0, input);
            Marshal.WriteIntPtr(handles, IntPtr.Size, output);
            Marshal.WriteIntPtr(handles, IntPtr.Size * 2, error);
            // PROC_THREAD_ATTRIBUTE_HANDLE_LIST: the program inherits these three handles only.
            if (!UpdateProcThreadAttribute(list, 0, new IntPtr(0x20002), handles, new IntPtr(IntPtr.Size * 3), IntPtr.Zero, IntPtr.Zero))
                throw new Win32Exception(Marshal.GetLastWin32Error());
            var si = new STARTUPINFOEX();
            si.StartupInfo.cb = Marshal.SizeOf(typeof(STARTUPINFOEX));
            si.StartupInfo.dwFlags = 0x100 | 0x1;    // STARTF_USESTDHANDLES | STARTF_USESHOWWINDOW
            si.StartupInfo.wShowWindow = 0;          // SW_HIDE
            si.StartupInfo.hStdInput = input;
            si.StartupInfo.hStdOutput = output;
            si.StartupInfo.hStdError = error;
            si.lpAttributeList = list;
            PROCESS_INFORMATION pi;
            // EXTENDED_STARTUPINFO_PRESENT | CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT
            uint flags = 0x00080000u | 0x00000010u | 0x00000400u;
            if (!CreateProcessW(app, new StringBuilder(commandLine), IntPtr.Zero, IntPtr.Zero, true, flags, IntPtr.Zero, directory, ref si, out pi))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "cannot start " + app);
            CloseHandle(pi.hThread);
            try {
                if (wait) {
                    if (WaitForSingleObject(pi.hProcess, (uint)timeoutMs) != 0) {
                        TerminateProcess(pi.hProcess, 1);
                        exitCode = -1;
                    } else if (!GetExitCodeProcess(pi.hProcess, out exitCode)) {
                        throw new Win32Exception(Marshal.GetLastWin32Error());
                    }
                }
            } finally {
                CloseHandle(pi.hProcess);
            }
            return pi.dwProcessId;
        } finally {
            if (listReady) DeleteProcThreadAttributeList(list);
            if (list != IntPtr.Zero) Marshal.FreeHGlobal(list);
            if (handles != IntPtr.Zero) Marshal.FreeHGlobal(handles);
            CloseHandle(input);
            if (output != IntPtr.Zero) CloseHandle(output);
            if (error != IntPtr.Zero) CloseHandle(error);
        }
    }
}
'@

function Start-Detached {
    # Starts a program in a new hidden console, with output to two files readable at any time,
    # inheriting only its standard handles. With -Wait, answers its exit code (-1 after a
    # timeout, when it is ended); otherwise its process id.
    param(
        [Parameter(Mandatory)][string]$FilePath,
        [Parameter(Mandatory)][AllowEmptyCollection()][string[]]$Arguments,
        [Parameter(Mandatory)][string]$StdOut,
        [Parameter(Mandatory)][string]$StdErr,
        [string]$WorkingDirectory,
        [switch]$Wait,
        [int]$TimeoutSeconds = 600
    )
    if (-not ('ShareCraftLauncher' -as [type])) {
        Initialize-SafeTemp
        Add-Type -TypeDefinition $script:LauncherSource
    }
    # Never the current folder by default: a program left running (a database started by
    # pg_ctl) would hold it, for example the unzipped package the installer runs from.
    if (-not $WorkingDirectory) { $WorkingDirectory = $env:SystemRoot }
    $line = (@(ConvertTo-CommandLineArgument -Value $FilePath) + @($Arguments | ForEach-Object { ConvertTo-CommandLineArgument -Value $_ })) -join ' '
    $code = 0
    $id = [ShareCraftLauncher]::Start($FilePath, $line, $WorkingDirectory, $StdOut, $StdErr, [bool]$Wait, $TimeoutSeconds * 1000, [ref]$code)
    if ($Wait) { return $code }
    $id
}

function Read-SharedText {
    # Reads a whole text file that a running program may still be writing.
    param([Parameter(Mandatory)][string]$Path)
    $stream = New-Object System.IO.FileStream($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]'ReadWrite, Delete')
    try { (New-Object System.IO.StreamReader($stream)).ReadToEnd() } finally { $stream.Dispose() }
}

function Invoke-Native {
    # Runs a program to the end with Start-Detached (no window; its output to files), and
    # checks its exit code. With -Secret its output is never shown, even on failure, because it may
    # repeat a statement that held a password. Answers ExitCode and Output.
    param(
        [Parameter(Mandatory)][string]$FilePath,
        [Parameter(Mandatory)][AllowEmptyCollection()][string[]]$Arguments,
        [Parameter(Mandatory)][string]$What,
        [string]$WorkingDirectory,
        [string]$OutputFolder,
        [string]$OutputBase,
        [int[]]$AllowedExitCodes = @(0),
        [int]$TimeoutSeconds = 600,
        [switch]$Secret
    )
    if ($OutputBase) {
        $out = "$OutputBase.out.log"
        $err = "$OutputBase.err.log"
    } else {
        if (-not $OutputFolder) { $OutputFolder = $env:TEMP }
        $stem = Join-Path $OutputFolder ('sharecraft-' + [guid]::NewGuid().ToString('N'))
        $out = "$stem.out"
        $err = "$stem.err"
    }
    $output = ''
    try {
        $code = Start-Detached -FilePath $FilePath -Arguments $Arguments -StdOut $out -StdErr $err -WorkingDirectory $WorkingDirectory -Wait -TimeoutSeconds $TimeoutSeconds
        if ($code -eq -1) { throw "$What did not finish within $TimeoutSeconds seconds." }
        foreach ($file in $out, $err) {
            if (Test-Path -LiteralPath $file) {
                try { $output += Read-SharedText -Path $file } catch { }
            }
        }
        if ($AllowedExitCodes -notcontains $code) {
            if ($Secret) {
                throw "$What failed (exit code $code). Its messages are not shown because they may include a password."
            }
            $said = $output.Trim()
            if ($said.Length -gt 2000) { $said = $said.Substring($said.Length - 2000) }
            if ($said) { throw "$What failed (exit code $code):`n$said" }
            throw "$What failed (exit code $code)."
        }
        [pscustomobject]@{ ExitCode = $code; Output = $output }
    } finally {
        if (-not $OutputBase -or $Secret) {
            foreach ($file in $out, $err) {
                if (Test-Path -LiteralPath $file) { Remove-Item -LiteralPath $file -Force -ErrorAction SilentlyContinue }
            }
        }
    }
}

# --- Ports -------------------------------------------------------------------------------------

function Test-PortFree {
    # Whether nothing listens on a TCP port on this computer, on any address.
    param([Parameter(Mandatory)][int]$Port)
    $listeners = [System.Net.NetworkInformation.IPGlobalProperties]::GetIPGlobalProperties().GetActiveTcpListeners()
    foreach ($endpoint in $listeners) {
        if ($endpoint.Port -eq $Port) { return $false }
    }
    $probe = New-Object System.Net.Sockets.TcpListener ([System.Net.IPAddress]::Loopback), $Port
    $probe.ExclusiveAddressUse = $true
    try {
        $probe.Start()
        return $true
    } catch [System.Net.Sockets.SocketException] {
        return $false
    } finally {
        $probe.Stop()
    }
}

function Find-FreePort {
    # The first free port of a list, or a plain error naming the range.
    param([Parameter(Mandatory)][int[]]$Ports, [string]$What = 'this', [int[]]$Exclude = @())
    foreach ($port in $Ports) {
        if ($Exclude -contains $port) { continue }
        if (Test-PortFree -Port $port) { return $port }
    }
    $range = if ($Ports.Count -gt 1) { "$($Ports[0]) to $($Ports[-1])" } else { "$($Ports[0])" }
    throw "There is no free port for $What between $range; other programs use them all. Close one of them and try again."
}

function Get-InstallPorts {
    # The ports to use: on a reinstall, the ones in settings.cmd while they are still free;
    # otherwise the first free one of each range.
    param([Parameter(Mandatory)]$Paths)
    $old = $null
    if (Test-Path -LiteralPath $Paths.SettingsFile) { $old = Read-SettingsFile -Path $Paths.SettingsFile }
    $pick = {
        param([string]$Name, [int[]]$Range, [string]$What, [int[]]$Taken)
        if ($old -and $old.ContainsKey($Name)) {
            $value = 0
            if ([int]::TryParse(($old[$Name] -replace '^.*:', ''), [ref]$value) -and
                $Taken -notcontains $value -and (Test-PortFree -Port $value)) {
                return $value
            }
        }
        Find-FreePort -Ports $Range -What $What -Exclude $Taken
    }
    $pg = & $pick 'SHARECRAFT_PG_PORT' $script:PgPorts 'the database' @()
    $app = & $pick 'SHARECRAFT_ADDR' $script:AppPorts 'ShareCraft' @($pg)
    $icap = & $pick 'SHARECRAFT_ICAP_PORT' $script:IcapPorts 'the stand-in scanner' @($pg, $app)
    [pscustomobject]@{ Pg = [int]$pg; App = [int]$app; Icap = [int]$icap }
}

# --- settings.cmd ------------------------------------------------------------------------------

function ConvertTo-ProfileRelative {
    # Writes a folder under LOCALAPPDATA as %LOCALAPPDATA%\..., keeping settings.cmd ASCII
    # whatever the user's name is.
    param([Parameter(Mandatory)][string]$Path)
    $local = $env:LOCALAPPDATA.TrimEnd('\')
    if ($Path.StartsWith($local + '\', [System.StringComparison]::OrdinalIgnoreCase)) {
        return '%LOCALAPPDATA%' + $Path.Substring($local.Length)
    }
    $Path
}

function Write-SettingsFile {
    # Writes %LOCALAPPDATA%\ShareCraft\settings.cmd: the server's address and folders, and the
    # ports of the database and the stand-in scanner. It holds no secret.
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][int]$AppPort,
        [Parameter(Mandatory)][int]$PgPort,
        [Parameter(Mandatory)][int]$IcapPort,
        [string]$PublicUrl,
        [string]$TrustedProxies,
        [string]$HstsSeconds
    )
    $paths = Get-ShareCraftPaths
    $lines = @(
        '@rem ShareCraft settings, written by the installer. The Start shortcut reads them;',
        '@rem "call" this file from a Command Prompt to run sharecraft-server.exe by hand.',
        '@rem The database password is in .env next to this file: never share or print it.',
        "@set `"SHARECRAFT_ADDR=127.0.0.1:$AppPort`"",
        "@set `"SHARECRAFT_DATA=$(ConvertTo-ProfileRelative $paths.Data)`"",
        "@set `"SHARECRAFT_WEB=$(ConvertTo-ProfileRelative (Join-Path $paths.Program 'web'))`"",
        "@set `"SHARECRAFT_EDITORS_DIR=$(ConvertTo-ProfileRelative (Join-Path $paths.Program 'editors'))`"",
        "@set `"SHARECRAFT_PG_PORT=$PgPort`"",
        "@set `"SHARECRAFT_ICAP_PORT=$IcapPort`""
    )
    if ($PublicUrl) { $lines += "@set `"SHARECRAFT_PUBLIC_URL=$PublicUrl`"" }
    if ($TrustedProxies) { $lines += "@set `"SHARECRAFT_TRUSTED_PROXIES=$TrustedProxies`"" }
    if ($HstsSeconds) { $lines += "@set `"SHARECRAFT_HSTS_SECONDS=$HstsSeconds`"" }
    $text = ($lines -join "`r`n") + "`r`n"
    foreach ($ch in $text.ToCharArray()) {
        if ([int]$ch -gt 127) {
            throw "A ShareCraft folder lies outside LOCALAPPDATA and has a non-ASCII letter; settings.cmd cannot hold it."
        }
    }
    [System.IO.File]::WriteAllText($Path, $text, [System.Text.Encoding]::ASCII)
}

function Write-InstallSettingsFile {
    # Writes settings during installation. On a reinstall, keep the optional production-mode
    # settings while replacing the ports and program paths chosen by this installer run.
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][int]$AppPort,
        [Parameter(Mandatory)][int]$PgPort,
        [Parameter(Mandatory)][int]$IcapPort,
        [hashtable]$OldSettings
    )
    $optional = @{}
    if ($OldSettings) {
        foreach ($name in 'SHARECRAFT_PUBLIC_URL', 'SHARECRAFT_TRUSTED_PROXIES', 'SHARECRAFT_HSTS_SECONDS') {
            if ($OldSettings.ContainsKey($name)) {
                $parameter = switch ($name) {
                    'SHARECRAFT_PUBLIC_URL' { 'PublicUrl' }
                    'SHARECRAFT_TRUSTED_PROXIES' { 'TrustedProxies' }
                    'SHARECRAFT_HSTS_SECONDS' { 'HstsSeconds' }
                }
                $optional[$parameter] = $OldSettings[$name]
            }
        }
    }
    Write-SettingsFile -Path $Path -AppPort $AppPort -PgPort $PgPort -IcapPort $IcapPort @optional
}

function Read-SettingsFile {
    # The variables settings.cmd sets, with %VARIABLES% expanded, as a hashtable.
    param([Parameter(Mandatory)][string]$Path)
    $settings = @{}
    foreach ($line in [System.IO.File]::ReadAllLines($Path)) {
        if ($line -match '^@?set "([A-Z_]+)=(.*)"\s*$') {
            $settings[$Matches[1]] = [System.Environment]::ExpandEnvironmentVariables($Matches[2])
        }
    }
    foreach ($name in 'SHARECRAFT_ADDR', 'SHARECRAFT_DATA', 'SHARECRAFT_WEB', 'SHARECRAFT_EDITORS_DIR', 'SHARECRAFT_PG_PORT', 'SHARECRAFT_ICAP_PORT') {
        if (-not $settings.ContainsKey($name)) { throw "$Path does not set $name; install ShareCraft again with -Reinstall." }
    }
    if ($settings.SHARECRAFT_ADDR -notmatch '^127\.0\.0\.1:\d+$') {
        throw "$Path sets SHARECRAFT_ADDR to something other than 127.0.0.1:<port>; ShareCraft only answers on this computer."
    }
    $settings
}

function Get-AppPort {
    param([Parameter(Mandatory)][hashtable]$Settings)
    [int]($Settings.SHARECRAFT_ADDR -replace '^.*:', '')
}

# --- Shortcuts ---------------------------------------------------------------------------------

function Get-WindowsPowerShell {
    Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
}

function New-ShareCraftShortcut {
    # A Start-menu shortcut that runs one of the scripts with Windows PowerShell.
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Script,
        [string[]]$ScriptArguments = @(),
        [switch]$Hidden,
        [string]$Description = ''
    )
    $folder = Split-Path -Parent $Path
    if (-not (Test-Path -LiteralPath $folder)) { New-Item -ItemType Directory -Force -Path $folder | Out-Null }
    $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass')
    if ($Hidden) { $arguments += @('-WindowStyle', 'Hidden') }
    $arguments += @('-File', $Script) + $ScriptArguments
    $shell = New-Object -ComObject WScript.Shell
    $link = $shell.CreateShortcut($Path)
    $link.TargetPath = Get-WindowsPowerShell
    $link.Arguments = Join-CommandLine -Arguments $arguments
    # Never a folder inside the program: Windows will not delete a process's working folder,
    # so Uninstall could not remove the program.
    $link.WorkingDirectory = $env:LOCALAPPDATA
    $link.Description = $Description
    $link.Save()
}

# --- Processes ---------------------------------------------------------------------------------

function Get-RecordedProcess {
    # The process whose id is recorded in a pid file, if it still runs and is the expected
    # program; a process that merely reuses the id is not ours.
    param([Parameter(Mandatory)][string]$PidFile, [Parameter(Mandatory)][string]$ExpectedPath)
    if (-not (Test-Path -LiteralPath $PidFile)) { return $null }
    $id = 0
    if (-not [int]::TryParse(([System.IO.File]::ReadAllText($PidFile).Trim()), [ref]$id)) { return $null }
    $process = Get-Process -Id $id -ErrorAction SilentlyContinue
    if (-not $process) { return $null }
    $path = $null
    try { $path = $process.Path } catch { $path = $null }
    if (-not $path) { return $null }
    $expected = [System.IO.Path]::GetFullPath($ExpectedPath)
    if (-not [string]::Equals([System.IO.Path]::GetFullPath($path), $expected, [System.StringComparison]::OrdinalIgnoreCase)) {
        return $null
    }
    $process
}

function Stop-RecordedProcess {
    # Stops the recorded process if it is the expected program, then removes the record.
    # Answers whether something was stopped.
    param([Parameter(Mandatory)][string]$PidFile, [Parameter(Mandatory)][string]$ExpectedPath)
    $stopped = $false
    $process = Get-RecordedProcess -PidFile $PidFile -ExpectedPath $ExpectedPath
    if ($process) {
        Stop-Process -Id $process.Id -Force
        if (-not $process.WaitForExit(15000)) {
            throw "$ExpectedPath (process $($process.Id)) did not stop within 15 seconds."
        }
        $stopped = $true
    }
    if (Test-Path -LiteralPath $PidFile) { Remove-Item -LiteralPath $PidFile -Force }
    $stopped
}

function Start-HiddenProcess {
    # Starts a program in a hidden console, its output going to <LogBase>.out.log and
    # <LogBase>.err.log (the previous run's logs are kept as .previous.log), and records its
    # process id.
    param(
        [Parameter(Mandatory)][string]$FilePath,
        [Parameter(Mandatory)][AllowEmptyCollection()][string[]]$Arguments,
        [Parameter(Mandatory)][string]$WorkingDirectory,
        [Parameter(Mandatory)][string]$LogBase,
        [Parameter(Mandatory)][string]$PidFile
    )
    $out = "$LogBase.out.log"
    $err = "$LogBase.err.log"
    foreach ($file in $out, $err) {
        if (Test-Path -LiteralPath $file) {
            $previous = $file -replace '\.log$', '.previous.log'
            Copy-Item -LiteralPath $file -Destination $previous -Force
            Remove-Item -LiteralPath $file -Force
        }
    }
    $id = Start-Detached -FilePath $FilePath -Arguments $Arguments -StdOut $out -StdErr $err -WorkingDirectory $WorkingDirectory
    $process = Get-Process -Id $id
    $null = $process.Handle   # keeps HasExited readable
    [System.IO.File]::WriteAllText($PidFile, [string]$process.Id, [System.Text.Encoding]::ASCII)
    $process
}

function Stop-OwnPrograms {
    # Stops any copy of a program still running from this exact path (for example one
    # started by hand), which a pid file does not name.
    param([Parameter(Mandatory)][string]$ExpectedPath)
    $name = [System.IO.Path]::GetFileNameWithoutExtension($ExpectedPath)
    $expected = [System.IO.Path]::GetFullPath($ExpectedPath)
    foreach ($process in @(Get-Process -Name $name -ErrorAction SilentlyContinue)) {
        $path = $null
        try { $path = $process.Path } catch { $path = $null }
        if ($path -and [string]::Equals([System.IO.Path]::GetFullPath($path), $expected, [System.StringComparison]::OrdinalIgnoreCase)) {
            Stop-Process -Id $process.Id -Force
            [void]$process.WaitForExit(15000)
        }
    }
}

# --- The database ------------------------------------------------------------------------------

function Get-PgTool {
    param([Parameter(Mandatory)][string]$PgBin, [Parameter(Mandatory)][string]$Name)
    $path = Join-Path $PgBin "$Name.exe"
    if (-not (Test-Path -LiteralPath $path)) { throw "$path is missing; the ShareCraft program is incomplete. Install it again with -Reinstall." }
    # Run by its plain-ASCII path, so that it also finds its own lib and share folders by one.
    ConvertTo-PgSafePath -Path $path
}

function Test-PostgresRunning {
    param([Parameter(Mandatory)]$Paths, [Parameter(Mandatory)][string]$PgBin)
    if (-not (Test-Path -LiteralPath (Join-Path $Paths.Db 'PG_VERSION'))) { return $false }
    $result = Invoke-Native -FilePath (Get-PgTool $PgBin 'pg_ctl') -Arguments @('status', '-D', (ConvertTo-PgSafePath $Paths.Db)) `
        -What 'Asking whether the database runs' -OutputFolder $Paths.Run -AllowedExitCodes @(0, 3)
    $result.ExitCode -eq 0
}

function Start-Postgres {
    # Starts the database with pg_ctl start -w, unless it already runs.
    param([Parameter(Mandatory)]$Paths, [Parameter(Mandatory)][string]$PgBin)
    if (Test-PostgresRunning -Paths $Paths -PgBin $PgBin) { return }
    Write-Step 'Starting the database...'
    $log = Join-Path $Paths.Logs 'postgresql.log'
    if (-not (Test-Path -LiteralPath $log)) { [System.IO.File]::WriteAllText($log, '') }
    Invoke-Native -FilePath (Get-PgTool $PgBin 'pg_ctl') `
        -Arguments @('start', '-w', '-t', '60', '-s', '-D', (ConvertTo-PgSafePath $Paths.Db), '-l', (ConvertTo-PgSafePath $log)) `
        -What "Starting the database (its log is $log)" -OutputBase (Join-Path $Paths.Logs 'pg_ctl') -WorkingDirectory $Paths.Root | Out-Null
}

function Stop-Postgres {
    # Stops the database with pg_ctl stop -m fast, if it runs.
    param([Parameter(Mandatory)]$Paths, [Parameter(Mandatory)][string]$PgBin)
    if (-not (Test-PostgresRunning -Paths $Paths -PgBin $PgBin)) { return $false }
    Invoke-Native -FilePath (Get-PgTool $PgBin 'pg_ctl') -Arguments @('stop', '-m', 'fast', '-w', '-t', '60', '-s', '-D', (ConvertTo-PgSafePath $Paths.Db)) `
        -What 'Stopping the database' -OutputFolder $Paths.Run | Out-Null
    $true
}

function Invoke-Psql {
    # Runs a SQL file with psql against the local database. The password comes from the
    # PGPASSFILE that Invoke-WithPgPass set. Output is never shown when -Secret is given.
    param(
        [Parameter(Mandatory)]$Paths,
        [Parameter(Mandatory)][string]$PgBin,
        [Parameter(Mandatory)][int]$Port,
        [Parameter(Mandatory)][string]$User,
        [Parameter(Mandatory)][string]$Database,
        [Parameter(Mandatory)][string]$Sql,
        [Parameter(Mandatory)][string]$What,
        [switch]$Secret
    )
    $file = Join-Path $Paths.Run ('step-' + [guid]::NewGuid().ToString('N') + '.sql')
    $savedEncoding = $env:PGCLIENTENCODING
    try {
        if ($Secret) { Write-SecretFile -Path $file -Content $Sql } else { [System.IO.File]::WriteAllText($file, $Sql, $script:Utf8NoBom) }
        $env:PGCLIENTENCODING = 'UTF8'
        $arguments = @('-X', '-q', '-w', '-v', 'ON_ERROR_STOP=1', '-h', '127.0.0.1', '-p', [string]$Port,
            '-U', $User, '-d', $Database, '-f', (ConvertTo-PgSafePath $file))
        Invoke-Native -FilePath (Get-PgTool $PgBin 'psql') -Arguments $arguments -What $What -OutputFolder $Paths.Run -Secret:$Secret | Out-Null
    } finally {
        if ($null -eq $savedEncoding) { Remove-Item Env:PGCLIENTENCODING -ErrorAction SilentlyContinue } else { $env:PGCLIENTENCODING = $savedEncoding }
        if (Test-Path -LiteralPath $file) { Remove-Item -LiteralPath $file -Force }
    }
}

function ConvertTo-SqlLiteral {
    param([Parameter(Mandatory)][AllowEmptyString()][string]$Value)
    "'" + $Value.Replace("'", "''") + "'"
}

function Get-ScannerUrl {
    param([Parameter(Mandatory)][int]$Port)
    "icap://127.0.0.1:$Port/scan"
}

function Get-ScannerSettingSql {
    # SQL that sets the scanner address as PUT /api/settings/scanner would: the scanner_url
    # setting holds the address as a JSON string, the change is credited to the administrator
    # and recorded as a setting.changed event. With -OnlyIf, it changes nothing unless the
    # setting still holds that address (so an administrator's own choice is kept).
    param([Parameter(Mandatory)][string]$Url, [string]$AdminEmail, [string]$OnlyIf)
    $to = "to_jsonb($(ConvertTo-SqlLiteral $Url)::text)"
    $actor = if ($AdminEmail) { "(select id from users where lower(email) = lower($(ConvertTo-SqlLiteral $AdminEmail)))" } else { 'null::uuid' }
    $condition = if ($OnlyIf) { "and value = to_jsonb($(ConvertTo-SqlLiteral $OnlyIf)::text)" } else { '' }
    @"
begin;
create temporary table scanner_change on commit drop as
  select value as before from settings where key = 'scanner_url' $condition;
insert into events (kind, actor, detail)
  select 'setting.changed', $actor, jsonb_build_object('key', 'scanner_url', 'from', before, 'to', $to)
  from scanner_change where before is distinct from $to;
update settings set value = $to, changed_by = $actor, changed_at = now()
  where key = 'scanner_url' and exists (select 1 from scanner_change) and value is distinct from $to;
commit;
"@
}

# --- Program files -----------------------------------------------------------------------------

function Find-ForbiddenPackageEntries {
    # Anything a package must never carry: .env files and data folders, anywhere.
    param([Parameter(Mandatory)][string]$Folder)
    foreach ($item in @(Get-ChildItem -LiteralPath $Folder -Recurse -Force)) {
        if ($item.Name -ieq '.env' -or ($item.PSIsContainer -and $item.Name -ieq 'data')) {
            $item.FullName
        }
    }
}

function Assert-PackageFolder {
    # Checks that a folder is a complete ShareCraft package.
    param([Parameter(Mandatory)][string]$Folder)
    foreach ($item in 'bin\sharecraft-server.exe', 'bin\xtask.exe', 'pgsql\bin\initdb.exe', 'pgsql\bin\pg_ctl.exe',
        'pgsql\bin\psql.exe', 'pgsql\bin\postgres.exe', 'web\index.html', 'editors', 'scripts\common.ps1') {
        if (-not (Test-Path -LiteralPath (Join-Path $Folder $item))) {
            throw "This copy of ShareCraft is incomplete: $item is missing from $Folder. Copy or unzip the whole ShareCraft folder again."
        }
    }
    $forbidden = @(Find-ForbiddenPackageEntries -Folder $Folder)
    if ($forbidden.Count -gt 0) {
        throw "This copy of ShareCraft holds files it must never carry ($($forbidden -join ', ')). Make the package again with cargo xtask package."
    }
}

function Copy-ShareCraftProgram {
    # Puts the program into %LOCALAPPDATA%\Programs\ShareCraft, replacing an older copy
    # entirely. It never touches %LOCALAPPDATA%\ShareCraft, where the data lives.
    param([Parameter(Mandatory)][string]$Source, [Parameter(Mandatory)][string]$Destination)
    $from = [System.IO.Path]::GetFullPath($Source).TrimEnd('\')
    $to = [System.IO.Path]::GetFullPath($Destination).TrimEnd('\')
    if ([string]::Equals($from, $to, [System.StringComparison]::OrdinalIgnoreCase)) { return }
    if (Test-Path -LiteralPath $to) { Remove-Item -LiteralPath $to -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $to | Out-Null
    foreach ($item in @(Get-ChildItem -LiteralPath $from -Force)) {
        Copy-Item -LiteralPath $item.FullName -Destination $to -Recurse -Force
    }
    # Files unzipped from a download carry a mark that makes Windows ask before running them.
    Get-ChildItem -LiteralPath $to -Recurse -File -Force | Unblock-File
}

function Remove-ShareCraft {
    # Removes the program and the shortcuts; with -DeleteData also everything under
    # %LOCALAPPDATA%\ShareCraft (data, database, logs, .env, settings). Says what it did.
    param([Parameter(Mandatory)]$Paths, [switch]$DeleteData)
    foreach ($item in $Paths.Program, $Paths.StartMenu, $Paths.StartupShortcut) {
        if (Test-Path -LiteralPath $item) {
            Remove-Item -LiteralPath $item -Recurse -Force
        }
    }
    Write-Step "Removed the program ($($Paths.Program)) and the Start-menu shortcuts."
    if ($DeleteData) {
        if (Test-Path -LiteralPath $Paths.Root) { Remove-Item -LiteralPath $Paths.Root -Recurse -Force }
        Write-Step "Deleted all ShareCraft data and the database ($($Paths.Root))."
    } elseif (Test-Path -LiteralPath $Paths.Root) {
        Write-Step "Your data and the database were kept in $($Paths.Root). Install ShareCraft.cmd -Reinstall uses them again."
    }
}

# --- Starting and stopping ---------------------------------------------------------------------

function Test-ServerAnswers {
    # Whether ShareCraft answers /healthz on this computer.
    param([Parameter(Mandatory)][int]$Port)
    try {
        $request = [System.Net.HttpWebRequest]::Create("http://127.0.0.1:$Port/healthz")
        $request.Proxy = $null
        $request.Timeout = 3000
        $response = $request.GetResponse()
        try { return ([int]$response.StatusCode -eq 200) } finally { $response.Close() }
    } catch {
        return $false
    }
}

function Set-ServerEnvironment {
    # Puts the settings into this process's environment, for the programs it starts.
    param([Parameter(Mandatory)][hashtable]$Settings)
    foreach ($name in 'SHARECRAFT_ADDR', 'SHARECRAFT_DATA', 'SHARECRAFT_WEB', 'SHARECRAFT_EDITORS_DIR') {
        Set-Item -Path "Env:$name" -Value $Settings[$name]
    }
    foreach ($name in 'SHARECRAFT_PUBLIC_URL', 'SHARECRAFT_TRUSTED_PROXIES', 'SHARECRAFT_HSTS_SECONDS') {
        if ($Settings.ContainsKey($name)) {
            Set-Item -Path "Env:$name" -Value $Settings[$name]
        } else {
            Remove-Item -Path "Env:$name" -ErrorAction SilentlyContinue
        }
    }
}

function Get-ShareCraftBrowserAddress {
    # The public HTTPS address in production, or the loopback HTTP address in development.
    param([Parameter(Mandatory)][hashtable]$Settings)
    if ($Settings.ContainsKey('SHARECRAFT_PUBLIC_URL')) {
        return $Settings.SHARECRAFT_PUBLIC_URL.TrimEnd('/') + '/'
    }
    "http://127.0.0.1:$(Get-AppPort -Settings $Settings)/"
}

function Start-ShareCraft {
    # Starts the database, the stand-in scanner and the server (hidden, logging to
    # %LOCALAPPDATA%\ShareCraft\logs), waits up to 60 seconds for the server to answer, then
    # opens the browser. If it already runs, it only opens the browser.
    param([Parameter(Mandatory)]$Paths, [switch]$NoBrowser, [int]$WaitSeconds = 60)
    # The server must read only its own .env, and the database tools only what they are given.
    Clear-DatabaseEnvironment
    if (-not (Test-Path -LiteralPath $Paths.SettingsFile)) {
        throw "ShareCraft is not installed for this Windows user ($($Paths.SettingsFile) is missing). Run Install ShareCraft.cmd first."
    }
    $settings = Read-SettingsFile -Path $Paths.SettingsFile
    $port = Get-AppPort -Settings $settings
    $address = Get-ShareCraftBrowserAddress -Settings $settings
    Initialize-StateFolders -Paths $Paths
    Set-ServerEnvironment -Settings $settings

    $server = Get-RecordedProcess -PidFile $Paths.ServerPid -ExpectedPath $Paths.ServerExe
    if ($server -and (Test-ServerAnswers -Port $port)) {
        Write-Step "ShareCraft is already running at $address"
        if (-not $NoBrowser) { Start-Process $address }
        return $address
    }

    Start-Postgres -Paths $Paths -PgBin $Paths.PgBin

    if (-not (Get-RecordedProcess -PidFile $Paths.ScannerPid -ExpectedPath $Paths.XtaskExe)) {
        Write-Step 'Starting the stand-in malware scanner...'
        Start-HiddenProcess -FilePath $Paths.XtaskExe -Arguments @('fake-icap', '--port', $settings.SHARECRAFT_ICAP_PORT) `
            -WorkingDirectory $Paths.Root -LogBase (Join-Path $Paths.Logs 'scanner') -PidFile $Paths.ScannerPid | Out-Null
    }

    if (-not $server) {
        Write-Step 'Starting ShareCraft...'
        # The server reads DATABASE_URL from .env in its working folder.
        $server = Start-HiddenProcess -FilePath $Paths.ServerExe -Arguments @() -WorkingDirectory $Paths.Root `
            -LogBase (Join-Path $Paths.Logs 'server') -PidFile $Paths.ServerPid
    }

    $log = Join-Path $Paths.Logs 'server.err.log'
    $deadline = (Get-Date).AddSeconds($WaitSeconds)
    while (-not (Test-ServerAnswers -Port $port)) {
        if ($server.HasExited) {
            throw "ShareCraft stopped while starting. Its messages are in $log"
        }
        if ((Get-Date) -gt $deadline) {
            throw "ShareCraft did not answer within $WaitSeconds seconds, so the browser was not opened. It may still be starting: try $address in a minute. Its messages are in $log"
        }
        Start-Sleep -Milliseconds 500
    }
    Write-Step "ShareCraft is running at $address"
    if (-not $NoBrowser) { Start-Process $address }
    $address
}

function Stop-ShareCraft {
    # Stops the server and the scanner by their recorded process ids (and any other copy of
    # them running from the program folder), then the database with pg_ctl stop -m fast.
    param([Parameter(Mandatory)]$Paths, [string]$PgBin)
    if (-not $PgBin) { $PgBin = $Paths.PgBin }
    if (Stop-RecordedProcess -PidFile $Paths.ServerPid -ExpectedPath $Paths.ServerExe) { Write-Step 'Stopped ShareCraft.' }
    Stop-OwnPrograms -ExpectedPath $Paths.ServerExe
    if (Stop-RecordedProcess -PidFile $Paths.ScannerPid -ExpectedPath $Paths.XtaskExe) { Write-Step 'Stopped the stand-in scanner.' }
    Stop-OwnPrograms -ExpectedPath $Paths.XtaskExe
    if ((Test-Path -LiteralPath $Paths.Db) -and (Test-Path -LiteralPath (Join-Path $PgBin 'pg_ctl.exe'))) {
        if (Stop-Postgres -Paths $Paths -PgBin $PgBin) { Write-Step 'Stopped the database.' }
    }
}

function Show-GuiMessage {
    # A message box, for scripts run from a hidden shortcut where no console is seen.
    param([Parameter(Mandatory)][string]$Message, [string]$Title = 'ShareCraft', [switch]$IsError)
    try {
        Add-Type -AssemblyName System.Windows.Forms
        $icon = if ($IsError) { [System.Windows.Forms.MessageBoxIcon]::Error } else { [System.Windows.Forms.MessageBoxIcon]::Information }
        [void][System.Windows.Forms.MessageBox]::Show($Message, $Title, [System.Windows.Forms.MessageBoxButtons]::OK, $icon)
    } catch {
        Write-Host $Message
    }
}
