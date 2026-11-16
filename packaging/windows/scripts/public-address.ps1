# Turns ShareCraft production mode on or off for this Windows user, or shows its status.
# Run it through "Set public address.cmd" in the installed program folder.
[CmdletBinding()]
param(
    [Parameter(Position = 0)][string]$Url,
    [string]$TrustedProxies,
    [string]$HstsSeconds,
    [switch]$Off,
    [switch]$Status
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'common.ps1')

function ConvertTo-ShareCraftPublicUrl {
    param([Parameter(Mandatory)][string]$Value)
    $message = 'Type one valid HTTPS public address, such as https://share.example.com. It may have a port, but no path, spaces, quotes, percent signs, query, fragment, user name or password.'
    if ([string]::IsNullOrWhiteSpace($Value) -or $Value -match '[\s"%\\?#@]' -or
        $Value -cne $Value.ToLowerInvariant()) {
        throw $message
    }
    $match = [regex]::Match($Value, '^https://(?<host>\[[0-9a-f:]+\]|[a-z0-9](?:[a-z0-9.-]*[a-z0-9])?)(?::(?<port>[0-9]+))?/?$')
    if (-not $match.Success) { throw $message }
    $uri = $null
    if (-not [uri]::TryCreate($Value, [UriKind]::Absolute, [ref]$uri) -or
        $uri.Scheme -cne 'https' -or $uri.UserInfo -or $uri.Query -or $uri.Fragment -or
        $uri.AbsolutePath -cne '/' -or [uri]::CheckHostName($uri.DnsSafeHost) -eq [UriHostNameType]::Unknown) {
        throw $message
    }
    if ($match.Groups['port'].Success) {
        $port = 0
        if (-not [int]::TryParse($match.Groups['port'].Value, [ref]$port) -or $port -lt 1 -or $port -gt 65535) {
            throw $message
        }
    }
    $Value
}

function ConvertTo-ShareCraftTrustedProxies {
    param([Parameter(Mandatory)][string]$Value)
    $message = 'Trusted proxies must be a comma-separated list of IP addresses or CIDR ranges, such as 127.0.0.1, ::1, 10.0.0.0/24.'
    if ([string]::IsNullOrWhiteSpace($Value) -or $Value -notmatch '^[0-9A-Fa-f:.,/ ]+$') { throw $message }
    foreach ($entry in $Value.Split(',')) {
        $part = $entry.Trim()
        if (-not $part) { throw $message }
        $pieces = @($part.Split('/'))
        if ($pieces.Count -gt 2) { throw $message }
        if ($pieces[0] -notmatch ':') {
            if ($pieces[0] -notmatch '^\d{1,3}(?:\.\d{1,3}){3}$') { throw $message }
            foreach ($octet in $pieces[0].Split('.')) {
                $number = 0
                if (-not [int]::TryParse($octet, [ref]$number) -or $number -gt 255 -or
                    $octet -cne [string]$number) { throw $message }
            }
        }
        $ip = $null
        if (-not [System.Net.IPAddress]::TryParse($pieces[0], [ref]$ip)) { throw $message }
        if ($pieces.Count -eq 2) {
            $prefix = 0
            $maximum = if ($ip.AddressFamily -eq [System.Net.Sockets.AddressFamily]::InterNetwork) { 32 } else { 128 }
            $minimum = if ($maximum -eq 32) { 8 } else { 32 }
            if (-not [int]::TryParse($pieces[1], [ref]$prefix) -or $prefix -lt $minimum -or $prefix -gt $maximum) {
                throw $message
            }
        }
    }
    $Value
}

function ConvertTo-ShareCraftHstsSeconds {
    param([Parameter(Mandatory)][string]$Value)
    $seconds = [uint64]0
    if ($Value -notmatch '^\d+$' -or -not [uint64]::TryParse($Value, [ref]$seconds)) {
        throw 'HSTS seconds must be a whole number, such as 300 or 31536000.'
    }
    $Value
}

function Restart-ShareCraftForPublicAddress {
    param([Parameter(Mandatory)]$Paths)
    Stop-ShareCraft -Paths $Paths
    Start-ShareCraft -Paths $Paths -NoBrowser | Out-Null
}

function Write-PublicAddressInstructions {
    param([Parameter(Mandatory)][hashtable]$Settings)
    $port = Get-AppPort -Settings $Settings
    Write-Step "ShareCraft production mode is on at $($Settings.SHARECRAFT_PUBLIC_URL)."
    Write-Step "Point the proxy or tunnel at http://127.0.0.1:$port."
    Write-Step 'It must pass Host, or set X-Forwarded-Host, and set X-Forwarded-For and X-Forwarded-Proto: https.'
    Write-Step 'It must pass WebSocket upgrades through.'
    Write-Step 'Allow request bodies of at least 9 MB; uploads arrive in 8 MiB parts.'
}

function Invoke-PublicAddress {
    param(
        [Parameter(Mandatory)]$Paths,
        [string]$Url,
        [string]$TrustedProxies,
        [string]$HstsSeconds,
        [switch]$Off,
        [switch]$Status
    )
    $modes = @($PSBoundParameters.ContainsKey('Url'), [bool]$Off, [bool]$Status | Where-Object { $_ }).Count
    if ($modes -ne 1) { throw 'Use -Url https://... to turn production mode on, -Off to turn it off, or -Status to show the current mode.' }

    # Validate every supplied value before reading or writing settings and before stopping anything.
    if ($PSBoundParameters.ContainsKey('Url')) { $Url = ConvertTo-ShareCraftPublicUrl -Value $Url }
    if ($PSBoundParameters.ContainsKey('TrustedProxies')) { $TrustedProxies = ConvertTo-ShareCraftTrustedProxies -Value $TrustedProxies }
    if ($PSBoundParameters.ContainsKey('HstsSeconds')) { $HstsSeconds = ConvertTo-ShareCraftHstsSeconds -Value $HstsSeconds }

    if (-not (Test-Path -LiteralPath $Paths.SettingsFile)) {
        throw 'ShareCraft is not installed for this Windows user. Run Install ShareCraft.cmd first.'
    }
    $settings = Read-SettingsFile -Path $Paths.SettingsFile
    if ($Status) {
        if ($settings.ContainsKey('SHARECRAFT_PUBLIC_URL')) {
            Write-PublicAddressInstructions -Settings $settings
        } else {
            Write-Step "ShareCraft is in this-computer-only mode at http://$($settings.SHARECRAFT_ADDR)/."
        }
        return
    }

    if ($Off) {
        Write-SettingsFile -Path $Paths.SettingsFile -AppPort (Get-AppPort -Settings $settings) `
            -PgPort ([int]$settings.SHARECRAFT_PG_PORT) -IcapPort ([int]$settings.SHARECRAFT_ICAP_PORT)
        Restart-ShareCraftForPublicAddress -Paths $Paths
        Write-Step "Production mode is off. Use ShareCraft at http://$($settings.SHARECRAFT_ADDR)/ on this computer."
        return
    }

    $optional = @{ PublicUrl = $Url }
    if ($PSBoundParameters.ContainsKey('TrustedProxies')) { $optional.TrustedProxies = $TrustedProxies }
    elseif ($settings.ContainsKey('SHARECRAFT_TRUSTED_PROXIES')) { $optional.TrustedProxies = $settings.SHARECRAFT_TRUSTED_PROXIES }
    if ($PSBoundParameters.ContainsKey('HstsSeconds')) { $optional.HstsSeconds = $HstsSeconds }
    elseif ($settings.ContainsKey('SHARECRAFT_HSTS_SECONDS')) { $optional.HstsSeconds = $settings.SHARECRAFT_HSTS_SECONDS }
    Write-SettingsFile -Path $Paths.SettingsFile -AppPort (Get-AppPort -Settings $settings) `
        -PgPort ([int]$settings.SHARECRAFT_PG_PORT) -IcapPort ([int]$settings.SHARECRAFT_ICAP_PORT) @optional
    Restart-ShareCraftForPublicAddress -Paths $Paths
    Write-PublicAddressInstructions -Settings (Read-SettingsFile -Path $Paths.SettingsFile)
}

if ($MyInvocation.InvocationName -ne '.') {
    try {
        $arguments = @{ Paths = Get-ShareCraftPaths; Off = $Off; Status = $Status }
        if ($PSBoundParameters.ContainsKey('Url')) { $arguments.Url = $Url }
        if ($PSBoundParameters.ContainsKey('TrustedProxies')) { $arguments.TrustedProxies = $TrustedProxies }
        if ($PSBoundParameters.ContainsKey('HstsSeconds')) { $arguments.HstsSeconds = $HstsSeconds }
        Invoke-PublicAddress @arguments
        exit 0
    } catch {
        Write-Host ''
        Write-Host "ShareCraft's public address was not changed: $($_.Exception.Message)" -ForegroundColor Red
        exit 1
    }
}
