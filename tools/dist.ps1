<#
.SYNOPSIS
    Builds FanRGB and puts the one exe to run in dist\current; the previous one moves to dist\archive.

.DESCRIPTION
    dist\ (git-ignored) holds the executables people actually run; target\ is only cargo's build cache
    and is never run from (a running exe there blocks the next build). See docs/BUILDS.md.

        dist\current\fanrgb.exe   + BUILD.txt   the build to use / test now
        dist\archive\<id>\fanrgb.exe + BUILD.txt   earlier builds, newest id last when sorted

    <id> is <yyyy-MM-dd>_v<version>_<commit>, e.g. 2026-10-20_v0.2.0_f2bd702219.

    Default: cargo build --release --locked into target\dist, archive dist\current, copy the new exe in.
    The build remaps the cargo home, toolchain and repository paths (tools\remap-paths.ps1), so the exe
    carries no folder names of this PC.
    -ArchiveExe <exe> -Id <id> [-Note <text>]: archive an existing exe without building.

    Close FanRGB first if it runs from dist\current (tray > Exit, or `fanrgb.exe --exit`): a running
    exe cannot be moved. Starting the new dist\current exe while an older copy runs elsewhere asks
    whether to replace it.

.EXAMPLE
    .\tools\dist.ps1
.EXAMPLE
    .\tools\dist.ps1 -ArchiveExe target\release\fanrgb.exe -Id 2026-10-15_v0.1.0_first-build -Note "first hardware run"
#>
[CmdletBinding()]
param(
    [string]$ArchiveExe,
    [string]$Id,
    [string]$Note = ''
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$dist = Join-Path $root 'dist'
$current = Join-Path $dist 'current'
$archive = Join-Path $dist 'archive'

function Test-InUse([string]$path) {
    if (-not (Test-Path $path)) { return $false }
    try { $s = [IO.File]::Open($path, 'Open', 'ReadWrite', 'None'); $s.Close(); return $false } catch { return $true }
}

function Write-BuildInfo([string]$dir, [string]$id, [string]$version, [string]$commit, [string]$built, [string]$note) {
    $exe = Join-Path $dir 'fanrgb.exe'
    $hash = (Get-FileHash $exe -Algorithm SHA256).Hash.ToLower()
    $lines = @(
        "id       $id",
        "version  $version",
        "commit   $commit",
        "built    $built",
        "sha256   $hash"
    )
    if ($note) { $lines += "note     $note" }
    Set-Content -Path (Join-Path $dir 'BUILD.txt') -Value $lines -Encoding utf8
}

function Read-BuildId([string]$dir) {
    $info = Join-Path $dir 'BUILD.txt'
    if (Test-Path $info) {
        $line = Get-Content $info | Where-Object { $_ -like 'id *' } | Select-Object -First 1
        if ($line) { return ($line -replace '^id\s+', '').Trim() }
    }
    # No BUILD.txt: name it after the exe's date.
    return (Get-Item (Join-Path $dir 'fanrgb.exe')).LastWriteTime.ToString('yyyy-MM-dd_HHmm') + '_unknown'
}

function Move-ToArchive([string]$dir) {
    $exe = Join-Path $dir 'fanrgb.exe'
    if (-not (Test-Path $exe)) { return }
    if (Test-InUse $exe) {
        throw "$exe is running. Exit FanRGB first (tray > Exit, or `"$exe`" --exit), then run this again."
    }
    $id = Read-BuildId $dir
    $dest = Join-Path $archive $id
    if (Test-Path $dest) { $dest = "$dest-" + (Get-Date -Format 'HHmmss') }
    New-Item -ItemType Directory -Force $dest | Out-Null
    Get-ChildItem $dir | Move-Item -Destination $dest
    Write-Host "archived  $dest"
}

New-Item -ItemType Directory -Force $current, $archive | Out-Null

if ($ArchiveExe) {
    if (-not $Id) { throw '-ArchiveExe needs -Id <yyyy-MM-dd>_v<version>_<commit or label>' }
    $src = Resolve-Path $ArchiveExe
    $dest = Join-Path $archive $Id
    if (Test-Path $dest) { throw "$dest already exists" }
    New-Item -ItemType Directory -Force $dest | Out-Null
    # Copy, not move: the exe may be running (it locks against moves) or still be cargo's output.
    Copy-Item $src (Join-Path $dest 'fanrgb.exe')
    $parts = $Id -split '_'
    $version = if ($parts.Count -ge 2) { $parts[1] -replace '^v', '' } else { 'unknown' }
    $commit = if ($parts.Count -ge 3) { ($parts[2..($parts.Count - 1)] -join '_') } else { 'unknown' }
    $built = (Get-Item $src).LastWriteTime.ToString('yyyy-MM-dd HH:mm')
    Write-BuildInfo $dest $Id $version $commit $built ("copied from $src" + $(if ($Note) { "; $Note" } else { '' }))
    Write-Host "archived  $dest"
    return
}

Push-Location $root
try {
    $version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
    $commit = (git describe --always --dirty --abbrev=10).Trim()
    # target\dist: never the folder anything runs from, so a running FanRGB cannot block the build.
    # cargo reports progress on stderr; with 'Stop', Windows PowerShell 5.1 turns that into a terminating
    # error whenever the output is redirected, so judge the build by its exit code only.
    # Path remapping for this build only; the session's environment is restored afterwards.
    . (Join-Path $PSScriptRoot 'remap-paths.ps1')
    $savedRustflags = Enable-PathRemap $root (Join-Path $root 'target\dist')
    $ErrorActionPreference = 'Continue'
    try {
        cargo build --release --locked --target-dir target\dist
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = 'Stop'
        Restore-Rustflags $savedRustflags
    }
    if ($code -ne 0) { throw "cargo build failed ($code)" }
    $built = Get-Date
    $id = '{0}_v{1}_{2}' -f $built.ToString('yyyy-MM-dd'), $version, $commit

    Move-ToArchive $current
    Copy-Item 'target\dist\release\fanrgb.exe' (Join-Path $current 'fanrgb.exe')
    Write-BuildInfo $current $id $version $commit $built.ToString('yyyy-MM-dd HH:mm') $Note
    Write-Host "current   $(Join-Path $current 'fanrgb.exe')  ($id)"
} finally {
    Pop-Location
}
