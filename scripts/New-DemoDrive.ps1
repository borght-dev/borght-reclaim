<#
.SYNOPSIS
Builds a throwaway tree of fake projects, so Reclaim can be tried without pointing it at real work.

.DESCRIPTION
The files hold no data, but they do take their size on disk: about 1.8 GB in total.
Timestamps are set in the past so that each project lands in a different rating.
Delete the folder when you are done.

.EXAMPLE
.\scripts\New-DemoDrive.ps1 -Path $env:TEMP\reclaim-demo
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $Path
)

$ErrorActionPreference = 'Stop'
$megabyte = 1MB
$root = [System.IO.Path]::GetFullPath($Path)
if ((Test-Path -LiteralPath $root) -and (Get-ChildItem -LiteralPath $root -Force | Select-Object -First 1)) {
    throw "$root is not empty. Pick a new folder."
}
$null = New-Item -ItemType Directory -Force -Path $root

function Add-TextFile([string] $relativePath, [string] $content) {
    $file = Join-Path $root $relativePath
    $null = New-Item -ItemType Directory -Force -Path (Split-Path $file)
    Set-Content -LiteralPath $file -Value $content -Encoding utf8NoBOM
}

function Add-Bulk([string] $relativeFolder, [int] $megabytes, [int] $fileCount = 8, [string] $extension = 'bin', [string] $baseName = 'chunk') {
    $folder = Join-Path $root $relativeFolder
    $null = New-Item -ItemType Directory -Force -Path $folder
    $bytesPerFile = [long]($megabytes * $megabyte / $fileCount)
    foreach ($number in 1..$fileCount) {
        $stream = [System.IO.File]::Create((Join-Path $folder "$baseName-$number.$extension"))
        try { $stream.SetLength($bytesPerFile) } finally { $stream.Dispose() }
    }
}

function Invoke-Git([string] $relativeFolder, [datetime] $when, [string[]] $arguments) {
    $env:GIT_AUTHOR_DATE = $env:GIT_COMMITTER_DATE = $when.ToString('o')
    try {
        $identity = '-c', 'user.name=Demo', '-c', 'user.email=demo@example.invalid', '-c', 'core.autocrlf=false'
        & git -C (Join-Path $root $relativeFolder) @identity @arguments 2>&1 | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "git $($arguments -join ' ') failed in $relativeFolder" }
    } finally {
        Remove-Item Env:GIT_AUTHOR_DATE, Env:GIT_COMMITTER_DATE
    }
}

function New-Repository([string] $relativeFolder, [datetime] $when) {
    Invoke-Git $relativeFolder $when 'init', '--quiet', '--initial-branch=main'
    Invoke-Git $relativeFolder $when 'add', '--all'
    Invoke-Git $relativeFolder $when 'commit', '--quiet', '--message', 'Initial commit'
}

function Set-LastTouched([string] $relativeFolder, [datetime] $when) {
    $folder = Get-Item -LiteralPath (Join-Path $root $relativeFolder) -Force
    foreach ($entry in @(Get-ChildItem -LiteralPath $folder.FullName -Recurse -Force) + $folder) {
        $entry.LastWriteTime = $when
    }
}

$now = Get-Date
$threeHoursAgo = $now.AddHours(-3)
$twoWeeksAgo = $now.AddDays(-14)
$threeWeeksAgo = $now.AddDays(-20)
$lastMonth = $now.AddDays(-30)
$longAgo = $now.AddDays(-200)

# A web shop nobody opened for three weeks: build output and dependencies are safe to remove.
Add-TextFile 'shop-web\package.json' '{ "name": "shop-web", "private": true }'
Add-TextFile 'shop-web\.gitignore' "node_modules/`n.next/`ndist/`n.worktrees/"
Add-TextFile 'shop-web\src\app.ts' 'export const shop = "open";'
New-Repository 'shop-web' $threeWeeksAgo
Add-Bulk 'shop-web\node_modules\react' 130
Add-Bulk 'shop-web\node_modules\typescript' 130
Add-Bulk 'shop-web\.next\cache' 90
Add-Bulk 'shop-web\dist' 30 -extension 'js'

# A finished feature branch in a worktree of that shop, idle for two weeks.
Invoke-Git 'shop-web' $twoWeeksAgo 'worktree', 'add', '--quiet', '-b', 'feature/checkout', '.worktrees\feature-checkout'
Add-Bulk 'shop-web\.worktrees\feature-checkout\node_modules\react' 180
Set-LastTouched 'shop-web' $threeWeeksAgo
Set-LastTouched 'shop-web\.worktrees\feature-checkout' $twoWeeksAgo

# An API somebody is working on right now: everything in it is blocked.
Add-TextFile 'billing-api\Billing.sln' ''
Add-TextFile 'billing-api\.gitignore' "bin/`nobj/"
Add-TextFile 'billing-api\Billing.Api\Billing.Api.csproj' '<Project Sdk="Microsoft.NET.Sdk.Web" />'
Add-TextFile 'billing-api\Billing.Api\Program.cs' 'System.Console.WriteLine("billing");'
New-Repository 'billing-api' $now
Add-Bulk 'billing-api\Billing.Api\bin\Debug\net10.0' 140 -extension 'dll'
Add-Bulk 'billing-api\Billing.Api\obj\Debug\net10.0' 60 -extension 'dll'

# A Rust project built three hours ago: removable, but a rebuild is likely soon.
Add-TextFile 'render-engine\Cargo.toml' "[package]`nname = `"render-engine`"`nversion = `"0.1.0`""
Add-TextFile 'render-engine\.gitignore' 'target/'
Add-TextFile 'render-engine\src\main.rs' 'fn main() {}'
New-Repository 'render-engine' $threeHoursAgo
Add-Bulk 'render-engine\target\debug\deps' 320 -extension 'rlib'
Set-LastTouched 'render-engine' $threeHoursAgo

# Python notebooks untouched for a month.
Add-TextFile 'notebooks\pyproject.toml' "[project]`nname = `"notebooks`""
Add-TextFile 'notebooks\analysis.py' 'print("analysis")'
Add-TextFile 'notebooks\.venv\pyvenv.cfg' 'home = C:\Python313'
Add-Bulk 'notebooks\.venv\Lib\site-packages\numpy' 120 -extension 'pyd'
Set-LastTouched 'notebooks' $lastMonth

# Scratch, a large download and an old prototype.
Add-Bulk 'tmp\export-2024' 80 -extension 'csv'
Add-Bulk 'tmp\crash-dumps' 30 -extension 'dmp'
Set-LastTouched 'tmp\export-2024' $lastMonth
Set-LastTouched 'tmp\crash-dumps' $lastMonth
Add-Bulk 'downloads' 300 -fileCount 1 -extension 'iso' -baseName 'linux-desktop-amd64'
Set-LastTouched 'downloads' $lastMonth
Add-TextFile 'archive\2023-prototype\notes.txt' 'first try'
Add-Bulk 'archive\2023-prototype\assets' 140 -extension 'psd'
Set-LastTouched 'archive' $longAgo

Write-Host "Demo drive ready: $root"
Write-Host "Try: reclaim plan `"$root`""
