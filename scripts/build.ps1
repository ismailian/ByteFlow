<#
.SYNOPSIS
  Release build of ByteFlow Desktop with versioned output.

.DESCRIPTION
  Builds are release-only (bf-core/build.rs refuses the debug profile) and
  always versioned: every build lands in dist\v<version>\ with the version in
  each file name, and an existing version is never overwritten. Bump the
  version to build again.

.EXAMPLE
  scripts\build.ps1 -Bump patch     # 0.2.0 -> 0.2.1, then build
  scripts\build.ps1 -Test           # run the release test suite first
  scripts\build.ps1 -Installer      # also package dist\v<version>\ (cargo-packager, NSIS)
#>
param(
    [ValidateSet('patch', 'minor', 'major')]
    [string]$Bump,
    [switch]$Test,
    [switch]$Installer
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$manifest = Join-Path $root 'Cargo.toml'

function Get-WorkspaceVersion {
    $text = [IO.File]::ReadAllText($manifest)
    $m = [regex]::Match($text, '(?ms)^\[workspace\.package\].*?^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"')
    if (-not $m.Success) { throw "No [workspace.package] version in $manifest" }
    return [int[]]@($m.Groups[1].Value, $m.Groups[2].Value, $m.Groups[3].Value)
}

# Cargo prints progress on stderr. Under 'Stop', Windows PowerShell 5.1 turns
# redirected stderr into a terminating error, so run it under 'Continue' and
# judge success by the exit code.
function Invoke-Cargo {
    $old = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { & cargo @args } finally { $ErrorActionPreference = $old }
    if ($LASTEXITCODE -ne 0) { throw "cargo $($args -join ' ') failed (exit $LASTEXITCODE)." }
}

$v = Get-WorkspaceVersion
if ($Bump) {
    switch ($Bump) {
        'major' { $v = @(($v[0] + 1), 0, 0) }
        'minor' { $v = @($v[0], ($v[1] + 1), 0) }
        'patch' { $v = @($v[0], $v[1], ($v[2] + 1)) }
    }
    $text = [IO.File]::ReadAllText($manifest)
    $re = [regex]'(?ms)(^\[workspace\.package\].*?^version\s*=\s*")\d+\.\d+\.\d+(")'
    $text = $re.Replace($text, ('${1}' + ($v -join '.') + '${2}'), 1)
    [IO.File]::WriteAllText($manifest, $text, (New-Object Text.UTF8Encoding($false)))
}
$version = $v -join '.'
$out = Join-Path $root "dist\v$version"
if (Test-Path $out) {
    throw "v$version is already built ($out). Builds are versioned: run with -Bump patch|minor|major."
}

Push-Location $root
try {
    if ($Test) {
        Invoke-Cargo test --release --workspace
    }
    # No features: the dev-env feature never ships.
    Invoke-Cargo build --release -p byteflow -p bf-cli

    New-Item -ItemType Directory -Force $out | Out-Null
    $files = @(
        @{ From = 'target\release\byteflow.exe'; To = "byteflow-$version-windows-x64.exe" },
        @{ From = 'target\release\byteflow-crypto.exe'; To = "byteflow-crypto-$version-windows-x64.exe" }
    )
    $sums = @()
    foreach ($f in $files) {
        $dest = Join-Path $out $f.To
        Copy-Item (Join-Path $root $f.From) $dest
        $hash = (Get-FileHash -Algorithm SHA256 $dest).Hash.ToLower()
        $sums += "$hash  $($f.To)"
    }
    [IO.File]::WriteAllLines((Join-Path $out 'SHA256SUMS.txt'), $sums)
    $rustc = (rustc --version)
    $info = @(
        "ByteFlow Desktop $version",
        "Built: $((Get-Date).ToString('yyyy-MM-dd HH:mm:ss zzz'))",
        "Profile: release",
        "Toolchain: $rustc"
    )
    [IO.File]::WriteAllLines((Join-Path $out 'BUILDINFO.txt'), $info)
    if ($Installer) {
        & (Join-Path $PSScriptRoot 'installer.ps1') -Version $version
    }
    Write-Host ""
    Write-Host "Built ByteFlow v$version -> $out"
    Get-ChildItem $out | ForEach-Object { Write-Host ("  {0,-48} {1,12:N0} bytes" -f $_.Name, $_.Length) }
}
finally {
    Pop-Location
}
