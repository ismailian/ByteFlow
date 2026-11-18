# Website screenshots from a dev-env build with demo data (crates/byteflow/src/demo.rs).
#   cargo build --release -p byteflow --features dev-env
#   scripts\screenshot.ps1 -Out site\assets\files-light.png
#   scripts\screenshot.ps1 -Out site\assets\photos-grid-dark.png -Theme dark -Prefix "Photos/2026 Lisbon/" -Grid 1
# Each run uses a fresh throwaway vault in %TEMP% and deletes it afterwards.
param(
    [string]$Exe = (Join-Path $PSScriptRoot "..\target\release\byteflow.exe"),
    [Parameter(Mandatory)] [string]$Out,
    [string]$Page = "files",
    [string]$Theme = "light",
    [string]$Prefix = "",
    [string]$Select = "",
    [string]$Grid = "0",
    [string]$View = "",
    [int]$Wait = 6,
    [int]$ClientW = 1600,
    [int]$ClientH = 960
)
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class W {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int c);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr a, int x, int y, int cx, int cy, uint f);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
}
"@
[void][W]::SetProcessDPIAware()

$data = Join-Path $env:TEMP ("bf-demo-" + [guid]::NewGuid().ToString("N"))
$env:BYTEFLOW_DEV_DATA_DIR = $data
$env:BYTEFLOW_DEV_DEMO = "1"
$env:BYTEFLOW_DEV_ALLOW_CAPTURE = "1"
$env:BYTEFLOW_DEV_PAGE = $Page
$env:BYTEFLOW_DEV_THEME = $Theme
$env:BYTEFLOW_DEV_PREFIX = $Prefix
$env:BYTEFLOW_DEV_SELECT = $Select
$env:BYTEFLOW_DEV_GRID = $Grid
$env:BYTEFLOW_DEV_VIEW = $View

$p = Start-Process -FilePath $Exe -PassThru
try {
    $deadline = (Get-Date).AddSeconds(30)
    while ($p.MainWindowHandle -eq 0 -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 300; $p.Refresh() }
    if ($p.MainWindowHandle -eq 0) { throw "no window" }
    $h = $p.MainWindowHandle
    [void][W]::ShowWindow($h, 9)
    [void][W]::SetForegroundWindow($h)
    Start-Sleep -Seconds $Wait
    $p.Refresh(); $h = $p.MainWindowHandle
    [void][W]::ShowWindow($h, 9)
    [void][W]::SetForegroundWindow($h)
    # Fit above the taskbar: client area $ClientW x $ClientH physical pixels.
    $wr = New-Object W+RECT; [void][W]::GetWindowRect($h, [ref]$wr)
    $cr = New-Object W+RECT; [void][W]::GetClientRect($h, [ref]$cr)
    $bw = ($wr.R - $wr.L) - $cr.R; $bh = ($wr.B - $wr.T) - $cr.B
    [void][W]::SetWindowPos($h, [IntPtr]::Zero, 40, 0, $ClientW + $bw, $ClientH + $bh, 0x0040)
    [void][W]::SetCursorPos(1900, 1060)
    Start-Sleep -Milliseconds 1500
    $r = New-Object W+RECT; [void][W]::GetClientRect($h, [ref]$r)
    $pt = New-Object W+POINT; [void][W]::ClientToScreen($h, [ref]$pt)
    $w = $r.R - $r.L; $hh = $r.B - $r.T
    $bmp = New-Object System.Drawing.Bitmap $w, $hh
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($pt.X, $pt.Y, 0, 0, (New-Object System.Drawing.Size $w, $hh))
    $bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose(); $bmp.Dispose()
    "saved $Out ($w x $hh)"
} finally {
    Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 800
    Remove-Item -Recurse -Force $data -ErrorAction SilentlyContinue
}
