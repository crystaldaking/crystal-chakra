# Chakra installer for Windows (issue #203): verified download, user-owned
# install directory, idempotent user-PATH setup, failure preservation.
#
# Usage:
#   pwsh tools/install.ps1 [-Version vX.Y.Z] [-Dir PATH] [-NoPathModify]
#                          [-BaseUrl URL]
#
# Defaults: latest stable GitHub release, %LOCALAPPDATA%\Programs\chakra.
[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$Dir = (Join-Path $env:LOCALAPPDATA "Programs\chakra"),
    [switch]$NoPathModify,
    [string]$BaseUrl = "https://github.com/crystaldaking/crystal-chakra/releases",
    [string]$ApiBase = "https://api.github.com"
)

$ErrorActionPreference = "Stop"
$Repo = "crystaldaking/crystal-chakra"

function Fail([string]$Message) {
    Write-Error "chakra installer: $Message"
    exit 1
}

if ($env:PROCESSOR_ARCHITECTURE -ne "AMD64") {
    Fail "unsupported platform windows/$($env:PROCESSOR_ARCHITECTURE); see README for source build"
}
$Target = "x86_64-pc-windows-msvc"

function Fetch([string]$Url, [string]$Dest) {
    try {
        if ($Url.StartsWith("file://")) {
            Copy-Item ($Url.Substring(7)) $Dest
        } elseif (Test-Path $Url -PathType Leaf) {
            # Test fixture: a local filesystem path instead of HTTPS.
            Copy-Item $Url $Dest
        } else {
            Invoke-WebRequest -Uri $Url -OutFile $Dest -MaximumRedirection 3 -TimeoutSec 60
        }
    } catch {
        Fail "download failed: $Url ($($_.Exception.Message)); previous installation untouched"
    }
}

function VersionKey([string]$Tag) {
    $parts = $Tag.TrimStart('v').Split('.')
    return [int]$parts[0] * 1000000 + [int]$parts[1] * 1000 + [int]$parts[2]
}

$explicitVersion = [bool]$Version
if (-not $Version) {
    $latestJson = Join-Path $env:TEMP "chakra-latest-$PID.json"
    Fetch "$ApiBase/repos/$Repo/releases/latest" $latestJson
    $metadata = Get-Content $latestJson -Raw | ConvertFrom-Json
    Remove-Item $latestJson -ErrorAction SilentlyContinue
    $Version = $metadata.tag_name
    if (-not $Version) { Fail "could not resolve the latest stable release tag" }
}
if ($Version -notmatch '^v\d+\.\d+\.\d+$') { Fail "malformed release tag: $Version" }

$existing = Join-Path $Dir "chakra.exe"
if (Test-Path $existing) {
    $installed = (& $existing --version).Split(' ')[1]
    if ($installed -and ((VersionKey $installed) -gt (VersionKey $Version)) -and (-not $explicitVersion)) {
        Fail "installed chakra $installed is newer than $Version; pass -Version explicitly to downgrade"
    }
}

$archive = "chakra-$Version-$Target.zip"
$work = New-Item -ItemType Directory -Path (Join-Path $env:TEMP "chakra-install-$PID")
$tmpBinary = $null
try {
    Fetch "$BaseUrl/download/$Version/$archive" (Join-Path $work $archive)
    Fetch "$BaseUrl/download/$Version/SHA256SUMS" (Join-Path $work "SHA256SUMS")

    $expected = $null
    foreach ($line in Get-Content (Join-Path $work "SHA256SUMS")) {
        if ($line -match ("^([0-9a-f]{64})\s+" + [Regex]::Escape($archive) + '$')) { $expected = $Matches[1]; break }
    }
    if (-not $expected) { Fail "no checksum entry for $archive; nothing was installed" }
    $actual = (Get-FileHash (Join-Path $work $archive) -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $expected) {
        Fail "checksum verification failed for $archive; nothing was installed"
    }

    Expand-Archive (Join-Path $work $archive) -DestinationPath $work
    $binary = Join-Path $work "chakra-$Version-$Target\chakra.exe"
    if (-not (Test-Path $binary)) { Fail "archive does not contain chakra.exe; nothing was installed" }

    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    $tmpBinary = Join-Path $Dir (".chakra.new." + [Guid]::NewGuid().ToString('N') + ".exe")
    Copy-Item $binary $tmpBinary
    # Verify runtime compatibility before touching the previous installation.
    $versionOutput = (& $tmpBinary --version) -join "`n"
    if ($LASTEXITCODE -ne 0) {
        Fail "downloaded binary cannot run on this machine; previous installation untouched"
    }
    $installedVersion = $Version.TrimStart('v')
    if ($versionOutput -ne "chakra $installedVersion") {
        Fail "downloaded binary reports $versionOutput, expected chakra $installedVersion; previous installation untouched"
    }
    Move-Item -Force $tmpBinary $existing

    $pathNote = ""
    if (-not $NoPathModify) {
        $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
        $entries = @()
        if ($userPath) { $entries = $userPath.Split(';') }
        if ($entries -contains $Dir) {
            $pathNote = "$Dir is already on the user PATH"
        } else {
            $newPath = if ($userPath) { "$userPath;$Dir" } else { $Dir }
            [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
            $pathNote = "added $Dir to the user PATH; open a new terminal for it to take effect"
        }
    } else {
        $pathNote = "PATH setup skipped (-NoPathModify)"
    }

    $resolved = (Get-Command chakra -ErrorAction SilentlyContinue).Source
    if ($resolved -and ($resolved -ne $existing)) {
        Write-Host "warning: 'chakra' currently resolves to $resolved; earlier PATH entries shadow $Dir"
    }
    Write-Host "installed chakra $installedVersion to $existing"
    Write-Host $pathNote
    Write-Host "next: open a new terminal, then run 'chakra init --agent <client>' to set up your agent (see README)"
} finally {
    if ($tmpBinary -and (Test-Path $tmpBinary)) {
        Remove-Item -Force $tmpBinary -ErrorAction SilentlyContinue
    }
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
