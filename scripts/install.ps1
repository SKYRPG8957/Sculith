# Windows PowerShell 5.1+; installs a verified preview ZIP into a new folder.
[CmdletBinding()]
param(
    [ValidatePattern('^v[0-9]+\.[0-9]+\.[0-9]+-preview\.[0-9]+$')]
    [string]$Version = 'v0.1.0-preview.9',
    [string]$Destination = (Join-Path (Get-Location) 'Sculith')
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
if ([Environment]::OSVersion.Platform -ne 'Win32NT' -or -not [Environment]::Is64BitOperatingSystem) {
    throw 'This installer requires 64-bit Windows.'
}
$Destination = [IO.Path]::GetFullPath($Destination)
if (Test-Path -LiteralPath $Destination) {
    throw "Destination already exists: $Destination. Choose a new folder with -Destination; existing settings and worlds will not be overwritten."
}
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
$Asset = "sculith-$Version-windows-x86_64.zip"
$BaseUrl = "https://github.com/SKYRPG8957/Sculith/releases/download/$Version"
$Temporary = Join-Path ([IO.Path]::GetTempPath()) ('sculith-install-' + [Guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path $Temporary
try {
    Write-Host "Downloading Sculith $Version..."
    $ZipPath = Join-Path $Temporary $Asset
    $ChecksumsPath = Join-Path $Temporary 'SHA256SUMS.txt'
    Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/$Asset" -OutFile $ZipPath
    Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/SHA256SUMS.txt" -OutFile $ChecksumsPath
    $Pattern = '^([0-9a-fA-F]{64})  ' + [Regex]::Escape($Asset) + '$'
    $MatchingLines = @(Get-Content -LiteralPath $ChecksumsPath | Where-Object { $_ -match $Pattern })
    if ($MatchingLines.Count -ne 1) { throw 'The release does not contain exactly one matching SHA-256 checksum.' }
    $null = $MatchingLines[0] -match $Pattern
    $ExpectedHash = $Matches[1]
    if ((Get-FileHash -LiteralPath $ZipPath -Algorithm SHA256).Hash -ne $ExpectedHash) {
        throw 'SHA-256 verification failed. Nothing was installed.'
    }
    $Extracted = Join-Path $Temporary 'extracted'
    Expand-Archive -LiteralPath $ZipPath -DestinationPath $Extracted
    foreach ($Required in @('sculith.exe', 'start.cmd', 'sculith.local.example.toml', 'release.json', 'THIRD_PARTY_LICENSES.txt')) {
        if (-not (Test-Path -LiteralPath (Join-Path $Extracted $Required) -PathType Leaf)) {
            throw "The archive is missing $Required. Nothing was installed."
        }
    }
    $Manifest = Get-Content -LiteralPath (Join-Path $Extracted 'release.json') -Raw | ConvertFrom-Json
    if ($Manifest.version -ne $Version -or $Manifest.target -ne 'x86_64-pc-windows-msvc') {
        throw 'The archive does not match the requested release and platform.'
    }
    # Directory.Move fails if another process creates Destination; never merge.
    $Parent = Split-Path -Parent $Destination
    $null = New-Item -ItemType Directory -Path $Parent -Force
    $Staged = Join-Path $Parent ('.sculith-install-' + [Guid]::NewGuid().ToString('N'))
    try {
        Copy-Item -LiteralPath $Extracted -Destination $Staged -Recurse
        [IO.Directory]::Move($Staged, $Destination)
    }
    finally {
        if (Test-Path -LiteralPath $Staged) { Remove-Item -LiteralPath $Staged -Recurse -Force }
    }
    Write-Host "Installed: $Destination"
    Write-Host 'Double-click start.cmd to start. Bedrock address: 127.0.0.1, port: 19132.'
    Write-Host 'This is a development preview. Official game-client entry remains unverified; Java world entry is not implemented.'
}
finally {
    if (Test-Path -LiteralPath $Temporary) { Remove-Item -LiteralPath $Temporary -Recurse -Force }
}
