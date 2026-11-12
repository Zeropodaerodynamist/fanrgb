<#
.SYNOPSIS
    Keeps this PC's folder names out of release builds. Dot-sourced by tools\release.ps1 and tools\dist.ps1.

.DESCRIPTION
    Panic messages and #[track_caller] locations embed the path of the source file they come from. For the
    crates.io dependencies that is an absolute path in the cargo home, so an exe built here carries strings
    such as C:\Users\<name>\.cargo\registry\src\... . Enable-PathRemap sets CARGO_ENCODED_RUSTFLAGS so that
    rustc writes neutral names instead (--remap-path-prefix):

        cargo home ($env:CARGO_HOME, else %USERPROFILE%\.cargo)   ->  cargo
        the active toolchain (rustc --print sysroot)               ->  rust
        the repository root                                        ->  fanrgb
        the target folder, when it is outside the repository       ->  target

    The flags come after any the user already set (CARGO_ENCODED_RUSTFLAGS, else RUSTFLAGS, which cargo
    ignores once CARGO_ENCODED_RUSTFLAGS is set). Cargo takes rustflags from the first of
    CARGO_ENCODED_RUSTFLAGS, RUSTFLAGS and the config files, so setting it also makes cargo ignore rustflags
    from every config file it would read: this repository's (it has none), parent folders' and
    %CARGO_HOME%\config.toml.

    Only the release builds of these two scripts use it, and Restore-Rustflags puts the environment back
    afterwards (the scripts run in the caller's PowerShell session): a plain `cargo build` or `cargo test`
    keeps its own flags and cache. CI and the `cargo auditable` build on GitHub's runners build in throwaway
    runner folders, so they do not need it.
#>

# The separator cargo uses in CARGO_ENCODED_RUSTFLAGS.
$script:RustflagsSeparator = [string][char]0x1f

# The flags the user already set, in the order cargo would pass them.
function Get-UserRustflags {
    if ($null -ne $env:CARGO_ENCODED_RUSTFLAGS) {
        if ($env:CARGO_ENCODED_RUSTFLAGS -eq '') { return @() }
        return @($env:CARGO_ENCODED_RUSTFLAGS.Split([char]0x1f))
    }
    if ($env:RUSTFLAGS) {
        # As cargo does: split on spaces, trim, drop empty entries.
        return @($env:RUSTFLAGS.Split(' ') | ForEach-Object { $_.Trim() } | Where-Object { $_ -ne '' })
    }
    return @()
}

# Absolute path without a trailing separator; relative paths resolve against the current PowerShell location.
function ConvertTo-FullPath([string] $Path) {
    $full = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Path)
    return [System.IO.Path]::GetFullPath($full).TrimEnd('\', '/')
}

# The --remap-path-prefix flags for a build of the repository at $Root into $TargetDir.
function Get-RemapFlags([string] $Root, [string] $TargetDir) {
    $rootFull = ConvertTo-FullPath $Root
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
    # rustup picks the toolchain from rust-toolchain.toml in the repository.
    Push-Location -LiteralPath $rootFull
    try {
        $sysroot = "$(& rustc --print sysroot)".Trim()
        if ($LASTEXITCODE -ne 0 -or -not $sysroot) { throw 'rustc --print sysroot failed' }
    } finally { Pop-Location }
    $map = @(
        [pscustomobject]@{ From = (ConvertTo-FullPath $cargoHome); To = 'cargo' },
        [pscustomobject]@{ From = (ConvertTo-FullPath $sysroot); To = 'rust' },
        [pscustomobject]@{ From = $rootFull; To = 'fanrgb' }
    )
    if ($TargetDir) {
        $targetFull = ConvertTo-FullPath $TargetDir
        if (-not $targetFull.StartsWith($rootFull + '\', [System.StringComparison]::OrdinalIgnoreCase)) {
            $map += [pscustomobject]@{ From = $targetFull; To = 'target' }
        }
    }
    # rustc applies the last matching prefix: the longer (more specific) paths go last.
    return @($map | Sort-Object { $_.From.Length } | ForEach-Object { "--remap-path-prefix=$($_.From)=$($_.To)" })
}

# Sets CARGO_ENCODED_RUSTFLAGS for one build. Returns what to hand to Restore-Rustflags.
function Enable-PathRemap([string] $Root, [string] $TargetDir) {
    $saved = [pscustomobject]@{ Set = ($null -ne $env:CARGO_ENCODED_RUSTFLAGS); Value = $env:CARGO_ENCODED_RUSTFLAGS }
    $remap = Get-RemapFlags $Root $TargetDir
    $all = @(Get-UserRustflags) + $remap
    $env:CARGO_ENCODED_RUSTFLAGS = $all -join $script:RustflagsSeparator
    Write-Host "release build paths: $(($remap | ForEach-Object { $_ -replace '^--remap-path-prefix=', '' }) -join ', ')"
    return $saved
}

function Restore-Rustflags($Saved) {
    if ($Saved -and $Saved.Set) {
        $env:CARGO_ENCODED_RUSTFLAGS = $Saved.Value
    } else {
        Remove-Item -LiteralPath Env:CARGO_ENCODED_RUSTFLAGS -ErrorAction SilentlyContinue
    }
}
