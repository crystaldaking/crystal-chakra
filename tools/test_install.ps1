# Native Windows fixture tests; no downloads or user-PATH modifications.
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$installer = Join-Path $PSScriptRoot 'install.ps1'
$compiler = Join-Path $env:WINDIR 'Microsoft.NET/Framework64/v4.0.30319/csc.exe'
if (-not (Test-Path $compiler)) { throw 'Windows .NET Framework compiler is required' }
$root = Join-Path $env:TEMP ('chakra-install-test-' + [Guid]::NewGuid().ToString('N'))
$installDir = Join-Path $root 'installed'
$target = 'x86_64-pc-windows-msvc'

function Make-Fixture([string]$Name, [string]$Tag, [string]$ReportedVersion, [int]$ExitCode = 0) {
    $base = Join-Path $root $Name
    $bundleName = "chakra-$Tag-$target"
    $bundle = Join-Path $base $bundleName
    New-Item -ItemType Directory -Force $bundle | Out-Null
    $source = Join-Path $base 'Program.cs'
    Set-Content $source ('using System; class Program { static int Main() { Console.WriteLine("chakra ' + $ReportedVersion + '"); return ' + $ExitCode + '; } }')
    & $compiler /nologo /target:exe "/out:$bundle/chakra.exe" $source
    if ($LASTEXITCODE -ne 0) { throw 'fixture compilation failed' }
    $download = Join-Path $base "download/$Tag"
    New-Item -ItemType Directory -Force $download | Out-Null
    $archive = "$bundleName.zip"
    Compress-Archive -Path $bundle -DestinationPath (Join-Path $download $archive)
    $digest = (Get-FileHash (Join-Path $download $archive) -Algorithm SHA256).Hash.ToLower()
    Set-Content (Join-Path $download 'SHA256SUMS') "$digest  $archive"
    return $base
}

function Run-Installer([string]$Fixture, [string]$Tag, [bool]$Success) {
    & pwsh -NoProfile -File $installer -Version $Tag -BaseUrl $Fixture -Dir $installDir -NoPathModify
    if (($LASTEXITCODE -eq 0) -ne $Success) { throw "unexpected installer result for $Fixture" }
    if (Get-ChildItem $installDir -Filter '.chakra.new.*' -Force) { throw 'staging files left behind' }
}

try {
    $good = Make-Fixture 'good' 'v0.4.0' '0.4.0'
    Run-Installer $good 'v0.4.0' $true
    Run-Installer $good 'v0.4.0' $true
    $binary = Join-Path $installDir 'chakra.exe'
    $before = (Get-FileHash $binary).Hash
    foreach ($case in @(@('bad-exit', '0.5.0', 126), @('bad-version', '0.3.0', 0))) {
        $bad = Make-Fixture $case[0] 'v0.5.0' $case[1] $case[2]
        Run-Installer $bad 'v0.5.0' $false
        if ((Get-FileHash $binary).Hash -ne $before) { throw 'working binary was replaced on failure' }
        if ((& $binary --version) -ne 'chakra 0.4.0') { throw 'previous install no longer works' }
    }
    $badChecksum = Make-Fixture 'bad-checksum' 'v0.5.0' '0.5.0'
    Set-Content (Join-Path $badChecksum 'download/v0.5.0/SHA256SUMS') ((('0' * 64) + "  chakra-v0.5.0-$target.zip"))
    Run-Installer $badChecksum 'v0.5.0' $false
    if ((Get-FileHash $binary).Hash -ne $before) { throw 'checksum failure replaced the previous binary' }
    $upgrade = Make-Fixture 'upgrade' 'v0.5.0' '0.5.0'
    Run-Installer $upgrade 'v0.5.0' $true
    if ((& $binary --version) -ne 'chakra 0.5.0') { throw 'upgrade did not take effect' }
    Write-Host 'Windows installer fixture tests passed'
} finally {
    Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
}
