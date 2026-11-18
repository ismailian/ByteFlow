<#
.SYNOPSIS
  Package a finished release build of ByteFlow Desktop as a Windows installer.

.DESCRIPTION
  Compiles installer\byteflow.iss with Inno Setup (7, or 6.4+) against
  dist\v<version>\ and writes ByteFlow-Setup-<version>-windows-x64.exe into
  that same folder, adding it to SHA256SUMS.txt and BUILDINFO.txt.

  Like the builds, installers are versioned and never overwritten: an
  existing setup for that version is an error.

  ISCC.exe is found through $env:ISCC, the Inno Setup uninstall entries in the
  registry (per-user and per-machine), or the usual install folders.

.EXAMPLE
  scripts\installer.ps1                  # the current workspace version
  scripts\installer.ps1 -Version 0.3.0
  scripts\build.ps1 -Bump minor -Installer   # build and package in one go
#>
param(
    [string]$Version
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

if (-not $Version) {
    $text = [IO.File]::ReadAllText((Join-Path $root 'Cargo.toml'))
    $m = [regex]::Match($text, '(?ms)^\[workspace\.package\].*?^version\s*=\s*"(\d+\.\d+\.\d+)"')
    if (-not $m.Success) { throw 'No [workspace.package] version in Cargo.toml' }
    $Version = $m.Groups[1].Value
}

$dist = Join-Path $root "dist\v$Version"
foreach ($f in "byteflow-$Version-windows-x64.exe", "byteflow-crypto-$Version-windows-x64.exe") {
    if (-not (Test-Path (Join-Path $dist $f))) {
        throw "$dist\$f is missing. Build v$Version first (scripts\build.ps1)."
    }
}
$setupName = "ByteFlow-Setup-$Version-windows-x64.exe"
$setup = Join-Path $dist $setupName
if (Test-Path $setup) {
    throw "$setup already exists. Installers are versioned: bump and rebuild for a new one."
}

function Find-Iscc {
    if ($env:ISCC -and (Test-Path $env:ISCC)) { return $env:ISCC }
    $keys = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*',
            'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*',
            'HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*'
    $found = Get-ItemProperty $keys -ErrorAction SilentlyContinue |
        Where-Object { $_.DisplayName -match '^Inno Setup (version )?(\d+)' -and $_.InstallLocation } |
        Sort-Object { [version]($_.DisplayVersion -replace '[^\d.].*$', '') } -Descending |
        ForEach-Object { Join-Path $_.InstallLocation 'ISCC.exe' } |
        Where-Object { Test-Path $_ }
    if ($found) { return @($found)[0] }
    foreach ($base in "$env:LOCALAPPDATA\Programs", ${env:ProgramFiles(x86)}, $env:ProgramFiles) {
        foreach ($v in '7', '6') {
            $p = Join-Path $base "Inno Setup $v\ISCC.exe"
            if ($base -and (Test-Path $p)) { return $p }
        }
    }
    throw 'Inno Setup not found. Install it, or point $env:ISCC at ISCC.exe.'
}

# ISCC.exe carries no version resource: take it from the matching uninstall entry.
function Get-InnoVersion([string]$iscc) {
    $dir = (Split-Path -Parent $iscc).TrimEnd('\')
    $keys = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*',
            'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*',
            'HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*'
    $hit = Get-ItemProperty $keys -ErrorAction SilentlyContinue |
        Where-Object { $_.DisplayName -match '^Inno Setup' -and $_.InstallLocation -and $_.InstallLocation.TrimEnd('\') -eq $dir } |
        Select-Object -First 1
    if ($hit) { return $hit.DisplayVersion }
    return 'unknown version'
}

$iscc = Find-Iscc
$innoVersion = Get-InnoVersion $iscc
Write-Host "Inno Setup $innoVersion ($iscc)"

$old = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
try {
    & $iscc /Q "/DAppVersion=$Version" "/DDistDir=$dist" "/O$dist" (Join-Path $root 'installer\byteflow.iss')
} finally { $ErrorActionPreference = $old }
if ($LASTEXITCODE -ne 0 -or -not (Test-Path $setup)) { throw "ISCC failed (exit $LASTEXITCODE)." }

$hash = (Get-FileHash -Algorithm SHA256 $setup).Hash.ToLower()
$sums = Join-Path $dist 'SHA256SUMS.txt'
$lines = @(if (Test-Path $sums) { Get-Content $sums | Where-Object { $_ -notmatch [regex]::Escape($setupName) } })
[IO.File]::WriteAllLines($sums, @($lines + "$hash  $setupName"))
$info = Join-Path $dist 'BUILDINFO.txt'
if (Test-Path $info) {
    $infoLines = @(Get-Content $info | Where-Object { $_ -notmatch '^Installer:' })
    [IO.File]::WriteAllLines($info, @($infoLines + "Installer: $setupName (Inno Setup $innoVersion)"))
}
Write-Host ""
Write-Host ("Installer -> {0} ({1:N0} bytes)" -f $setup, (Get-Item $setup).Length)
