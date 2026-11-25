<#
.SYNOPSIS
  Package a finished release build of ByteFlow Desktop as a Windows installer.

.DESCRIPTION
  Runs cargo-packager (NSIS) for the byteflow crate, using the config in
  crates\byteflow\Cargo.toml ([package.metadata.packager]) and the template
  installer\nsis\installer.nsi, against the release binaries in
  target\release\ (the ones scripts\build.ps1 just built and copied into
  dist\v<version>\). The setup is written as
  dist\v<version>\ByteFlow-Setup-<version>-windows-x64.exe and added to
  SHA256SUMS.txt and BUILDINFO.txt.

  Like the builds, installers are versioned and never overwritten: an
  existing setup for that version is an error.

  Needs cargo-packager:  cargo install cargo-packager --locked
  (it downloads NSIS itself on first use).

  Also writes dist\v<version>\latest.json, the update manifest. Attach it to
  the GitHub release with the setup: the app reads
  https://github.com/ismailian/ByteFlow/releases/latest/download/latest.json.

.EXAMPLE
  scripts\installer.ps1                  # the current workspace version
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
# target\release must hold the same binaries as dist (cargo-packager reads them there).
$release = Join-Path $root 'target\release'
foreach ($pair in @(@('byteflow.exe', "byteflow-$Version-windows-x64.exe"), @('byteflow-crypto.exe', "byteflow-crypto-$Version-windows-x64.exe"))) {
    $a = Join-Path $release $pair[0]
    $b = Join-Path $dist $pair[1]
    if (-not (Test-Path $a) -or (Get-FileHash $a).Hash -ne (Get-FileHash $b).Hash) {
        throw "target\release\$($pair[0]) is not the v$Version build. Rebuild with scripts\build.ps1."
    }
}

$setupName = "ByteFlow-Setup-$Version-windows-x64.exe"
$setup = Join-Path $dist $setupName
if (Test-Path $setup) {
    throw "$setup already exists. Installers are versioned: bump and rebuild for a new one."
}

if (-not (Get-Command cargo-packager -ErrorAction SilentlyContinue)) {
    throw 'cargo-packager not found. Install it: cargo install cargo-packager --locked'
}
$packagerVersion = (& cargo packager --version) -replace '^cargo-packager\s*', ''
Write-Host "cargo-packager $packagerVersion"

$work = Join-Path $root 'target\packager'
if (Test-Path $work) { Remove-Item -Recurse -Force $work }
New-Item -ItemType Directory -Force $work | Out-Null

$old = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
Push-Location $root
try {
    & cargo packager --release -p byteflow -f nsis --binaries-dir $release --out-dir $work
} finally {
    Pop-Location
    $ErrorActionPreference = $old
}
if ($LASTEXITCODE -ne 0) { throw "cargo packager failed (exit $LASTEXITCODE)." }

$built = Get-ChildItem $work -Filter '*.exe' -Recurse | Where-Object { $_.Name -like "*$Version*setup*.exe" } | Select-Object -First 1
if (-not $built) { throw "cargo packager produced no setup in $work." }
Copy-Item $built.FullName $setup
Remove-Item -Recurse -Force $work

$hash = (Get-FileHash -Algorithm SHA256 $setup).Hash.ToLower()
$sums = Join-Path $dist 'SHA256SUMS.txt'
$lines = @(if (Test-Path $sums) { Get-Content $sums | Where-Object { $_ -notmatch [regex]::Escape($setupName) } })
# Same setup under a fixed name: attached to every release, so
# releases/latest/download/ByteFlow-Setup-windows-x64.exe is always the
# newest installer (the website's Download button goes there).
$stableName = 'ByteFlow-Setup-windows-x64.exe'
Copy-Item $setup (Join-Path $dist $stableName) -Force
$lines = @($lines | Where-Object { $_ -notmatch [regex]::Escape($stableName) })
[IO.File]::WriteAllLines($sums, @($lines + "$hash  $setupName" + "$hash  $stableName"))
$info = Join-Path $dist 'BUILDINFO.txt'
if (Test-Path $info) {
    $infoLines = @(Get-Content $info | Where-Object { $_ -notmatch '^Installer:' })
    [IO.File]::WriteAllLines($info, @($infoLines + "Installer: $setupName (cargo-packager $packagerVersion, NSIS)"))
}

# Update manifest (published next to the setup on the site).
$manifest = [ordered]@{
    version = $Version
    url     = "https://github.com/ismailian/ByteFlow/releases/download/v$Version/$setupName"
    sha256  = $hash
    size    = (Get-Item $setup).Length
}
[IO.File]::WriteAllText((Join-Path $dist 'latest.json'), ($manifest | ConvertTo-Json), (New-Object Text.UTF8Encoding($false)))

Write-Host ""
Write-Host ("Installer -> {0} ({1:N0} bytes)" -f $setup, (Get-Item $setup).Length)
