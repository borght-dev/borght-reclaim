<#
.SYNOPSIS
Builds every logo file the app and the README use from one transparent source picture.

.EXAMPLE
.\scripts\New-AppIcon.ps1 -Source design\logo-source.png
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $Source,

    [string] $AssetsFolder = (Join-Path $PSScriptRoot '..\assets')
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$marginShare = 0.04
$readmeLogoSize = 512
$windowLogoSize = 256
$iconSizes = 16, 20, 24, 32, 40, 48, 64, 128, 256
$faintestVisibleAlpha = 8

function Find-VisibleBounds([System.Drawing.Bitmap] $picture) {
    $everything = [System.Drawing.Rectangle]::new(0, 0, $picture.Width, $picture.Height)
    $pixels = $picture.LockBits($everything, 'ReadOnly', 'Format32bppArgb')
    try {
        $bytes = [byte[]]::new($pixels.Stride * $picture.Height)
        [System.Runtime.InteropServices.Marshal]::Copy($pixels.Scan0, $bytes, 0, $bytes.Length)
        $left = $picture.Width; $top = $picture.Height; $right = -1; $bottom = -1
        for ($y = 0; $y -lt $picture.Height; $y++) {
            $rowStart = $y * $pixels.Stride
            for ($x = 0; $x -lt $picture.Width; $x++) {
                if ($bytes[$rowStart + ($x * 4) + 3] -lt $faintestVisibleAlpha) { continue }
                if ($x -lt $left) { $left = $x }
                if ($x -gt $right) { $right = $x }
                if ($y -lt $top) { $top = $y }
                if ($y -gt $bottom) { $bottom = $y }
            }
        }
    } finally {
        $picture.UnlockBits($pixels)
    }
    if ($right -lt 0) { throw "$Source has no visible pixels" }
    [System.Drawing.Rectangle]::FromLTRB($left, $top, $right + 1, $bottom + 1)
}

function New-CenteredSquare([System.Drawing.Bitmap] $picture, [System.Drawing.Rectangle] $visible, [int] $size) {
    $square = [System.Drawing.Bitmap]::new($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $graphics = [System.Drawing.Graphics]::FromImage($square)
    try {
        $graphics.CompositingQuality = 'HighQuality'
        $graphics.InterpolationMode = 'HighQualityBicubic'
        $graphics.PixelOffsetMode = 'HighQuality'
        $graphics.SmoothingMode = 'HighQuality'
        $drawable = $size * (1 - (2 * $marginShare))
        $scale = $drawable / [Math]::Max($visible.Width, $visible.Height)
        $width = $visible.Width * $scale
        $height = $visible.Height * $scale
        $target = [System.Drawing.RectangleF]::new(($size - $width) / 2, ($size - $height) / 2, $width, $height)
        $graphics.DrawImage($picture, $target, [System.Drawing.RectangleF]$visible, 'Pixel')
    } finally {
        $graphics.Dispose()
    }
    $square
}

function Get-PngBytes([System.Drawing.Bitmap] $picture) {
    $stream = [System.IO.MemoryStream]::new()
    try {
        $picture.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
        , $stream.ToArray()
    } finally {
        $stream.Dispose()
    }
}

function Write-Icon([string] $path, [int[]] $sizes, [byte[][]] $frames) {
    $largestSizeAnIconByteHolds = 255
    $writer = [System.IO.BinaryWriter]::new([System.IO.File]::Create($path))
    try {
        $writer.Write([uint16]0)
        $writer.Write([uint16]1)
        $writer.Write([uint16]$frames.Count)
        $offset = 6 + (16 * $frames.Count)
        for ($index = 0; $index -lt $frames.Count; $index++) {
            $sizeByte = if ($sizes[$index] -gt $largestSizeAnIconByteHolds) { 0 } else { $sizes[$index] }
            $writer.Write([byte]$sizeByte)
            $writer.Write([byte]$sizeByte)
            $writer.Write([byte]0)
            $writer.Write([byte]0)
            $writer.Write([uint16]1)
            $writer.Write([uint16]32)
            $writer.Write([uint32]$frames[$index].Length)
            $writer.Write([uint32]$offset)
            $offset += $frames[$index].Length
        }
        foreach ($frame in $frames) { $writer.Write($frame) }
    } finally {
        $writer.Dispose()
    }
}

$assets = [System.IO.Path]::GetFullPath($AssetsFolder)
$null = New-Item -ItemType Directory -Force -Path $assets
$original = [System.Drawing.Bitmap]::new((Resolve-Path -LiteralPath $Source).Path)
try {
    $visible = Find-VisibleBounds $original
    $rendered = @{}
    foreach ($size in @($readmeLogoSize, $windowLogoSize) + $iconSizes | Sort-Object -Unique) {
        $square = New-CenteredSquare $original $visible $size
        try { $rendered[$size] = Get-PngBytes $square } finally { $square.Dispose() }
    }
} finally {
    $original.Dispose()
}

[System.IO.File]::WriteAllBytes((Join-Path $assets 'logo.png'), $rendered[$readmeLogoSize])
[System.IO.File]::WriteAllBytes((Join-Path $assets 'logo-256.png'), $rendered[$windowLogoSize])
Write-Icon (Join-Path $assets 'reclaim.ico') $iconSizes ([byte[][]]($iconSizes | ForEach-Object { , $rendered[$_] }))

Write-Host "Wrote logo.png, logo-256.png and reclaim.ico to $assets"
