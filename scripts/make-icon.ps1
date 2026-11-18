<#
.SYNOPSIS
  Draws the ByteFlow app icon and writes crates\byteflow\assets\icon\.

.DESCRIPTION
  The mark matches the in-app brand tile (app.rs privacy overlay): a blue
  rounded square with a white padlock. Every size is drawn directly from
  vector geometry (not downscaled), so small sizes stay crisp; the keyhole
  is dropped below 24 px where it would only blur.

  Outputs:
    byteflow.ico   16/20/24/32/40/48/64/96/128/256, PNG-compressed entries
                   (embedded in byteflow.exe by build.rs, used by the installer)
    byteflow.png   256x256 (window/taskbar icon at runtime)
#>
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$root = Split-Path -Parent $PSScriptRoot
$outDir = Join-Path $root 'crates\byteflow\assets\icon'
New-Item -ItemType Directory -Force $outDir | Out-Null

function New-RoundRect([single]$x, [single]$y, [single]$w, [single]$h, [single]$r) {
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = 2 * $r
    $p.AddArc($x, $y, $d, $d, 180, 90)
    $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $p.CloseFigure()
    return $p
}

function New-IconPng([int]$size) {
    $bmp = New-Object System.Drawing.Bitmap($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.PixelOffsetMode = 'HighQuality'
    $g.CompositingQuality = 'HighQuality'
    $g.Clear([System.Drawing.Color]::Transparent)
    # Geometry is authored on a 256-unit grid.
    $s = [single]($size / 256.0)
    $g.ScaleTransform($s, $s)

    $c1 = [System.Drawing.ColorTranslator]::FromHtml('#3B82F6')
    $c2 = [System.Drawing.ColorTranslator]::FromHtml('#1D4ED8')
    $ink = [System.Drawing.ColorTranslator]::FromHtml('#1E40AF')

    # Tile. Small sizes fill more of the canvas so the mark stays readable.
    $inset = if ($size -le 24) { 4 } else { 10 }
    $tile = New-RoundRect $inset $inset (256 - 2 * $inset) (256 - 2 * $inset) 58
    $grad = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
        (New-Object System.Drawing.PointF(0, 0)), (New-Object System.Drawing.PointF(256, 256)), $c1, $c2)
    $g.FillPath($grad, $tile)

    $white = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::White)
    # Shackle.
    $sw = if ($size -le 24) { 26 } else { 22 }
    $pen = New-Object System.Drawing.Pen([System.Drawing.Color]::White, $sw)
    $shackle = New-Object System.Drawing.Drawing2D.GraphicsPath
    $shackle.AddLine(90, 124, 90, 100)
    $shackle.AddArc(90, 62, 76, 76, 180, 180)
    $shackle.AddLine(166, 100, 166, 124)
    $g.DrawPath($pen, $shackle)
    # Body.
    $body = New-RoundRect 66 114 124 94 20
    $g.FillPath($white, $body)
    # Keyhole.
    if ($size -ge 24) {
        $kb = New-Object System.Drawing.SolidBrush($ink)
        $g.FillEllipse($kb, 114, 138, 28, 28)
        $slot = New-Object System.Drawing.Drawing2D.GraphicsPath
        $slot.AddPolygon(@(
            (New-Object System.Drawing.PointF(123, 156)),
            (New-Object System.Drawing.PointF(133, 156)),
            (New-Object System.Drawing.PointF(137, 186)),
            (New-Object System.Drawing.PointF(119, 186))))
        $g.FillPath($kb, $slot)
    }
    $g.Dispose()

    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    return , $ms.ToArray()
}

$sizes = 16, 20, 24, 32, 40, 48, 64, 96, 128, 256
$pngs = @{}
foreach ($sz in $sizes) { $pngs[$sz] = New-IconPng $sz }

# ICO: ICONDIR, one ICONDIRENTRY per image, then the PNG payloads.
$ico = New-Object System.IO.MemoryStream
$w = New-Object System.IO.BinaryWriter($ico)
$w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
foreach ($sz in $sizes) {
    $dim = if ($sz -ge 256) { 0 } else { $sz }
    $w.Write([byte]$dim); $w.Write([byte]$dim); $w.Write([byte]0); $w.Write([byte]0)
    $w.Write([uint16]1); $w.Write([uint16]32)
    $w.Write([uint32]$pngs[$sz].Length); $w.Write([uint32]$offset)
    $offset += $pngs[$sz].Length
}
foreach ($sz in $sizes) { $w.Write($pngs[$sz]) }
$w.Flush()
[IO.File]::WriteAllBytes((Join-Path $outDir 'byteflow.ico'), $ico.ToArray())
[IO.File]::WriteAllBytes((Join-Path $outDir 'byteflow.png'), $pngs[256])
Write-Host "Wrote $outDir\byteflow.ico and byteflow.png"
