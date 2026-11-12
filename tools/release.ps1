<#
.SYNOPSIS
    Builds, signs and (optionally) publishes a FanRGB release; promotes a soaked pre-release to latest.

.DESCRIPTION
    Release (default):
    1. Checks (all must pass; -DryRun reports them as warnings and continues):
       - the version is the Cargo.toml (and Cargo.lock) version, and v<version> is not released yet;
       - the git tree is clean (no uncommitted or untracked files);
       - HEAD (full SHA) is pushed: it is on origin/<Branch> (default main) after a fetch;
       - CI (.github/workflows/ci.yml) succeeded on exactly that commit (gh run list --commit);
       - if the tag v<version> already exists on origin (for example an SSH-signed tag pushed by
         hand), it points at that same commit.
    2. cargo build --release --locked (CARGO_TARGET_DIR is honoured) and the signing tool. The app is
       built with --remap-path-prefix for the cargo home, the toolchain and the repository
       (tools\remap-paths.ps1), so the exe carries no folder names of this PC. The built executable
       must report the version, carry the commit stamp of HEAD (`git describe`), not "-dirty", and
       contain neither the repository, the user profile nor the cargo home path.
    3. Release notes: -NotesFile, or the "## [<version>]" section of CHANGELOG.md (must exist, not
       empty, at most 8 KiB).
    4. Writes release.json (version, published_at, notes, fanrgb.exe size + SHA-256) and signs it with
       the minisign key from -KeyDir (default %USERPROFILE%\.fanrgb-release, never in the repo). The key
       must be password-protected (sign --require-encrypted); the signing tool asks for the password
       on the console. Protect existing keys once with: fanrgb-release protect-keys --dir <KeyDir>
    5. Verifies the signature with the public key compiled into src\updater\keys.rs.
    6. Only with -Publish: re-checks that HEAD and the tree did not change, then
         gh release create v<version> ... --target <full SHA of HEAD>
       so the tag is created on (or must already be on) the exact commit that was built and tested.
       Next to fanrgb.exe, release.json and its signature, the release carries the licence files
       (LICENSE, LICENSE-MPL-2.0, NOTICE, THIRD-PARTY-LICENSES.html and .txt, licenses\*), copied into
       the release folder first. They are not in the signed manifest, which covers fanrgb.exe only.
       -Prerelease (default ON) publishes a pre-release, invisible to the app's updater
       (/releases/latest ignores pre-releases); -Prerelease:$false publishes it as latest at once.
       Without -Publish the exact command is printed.

    Promote (-PromoteLatest <tag>): after the 24-48 h canary soak (docs/RELEASING.md), marks the
    pre-release <tag> as a full release and "latest", which is what the app's updater offers. Refuses a
    release that is not a pre-release, or one published less than 24 h ago (unless -SkipSoak). Prints
    the gh command; runs it only with -Publish.

    -DryRun tests the pipeline with a throwaway, unencrypted key pair created in a temporary folder and
    deleted afterwards: the real keys are never read, and FanRGB cannot install the result (it is not
    signed by a compiled-in key), so a left-over dry-run manifest is harmless even if published.

.EXAMPLE
    tools\release.ps1                                   # build + sign v<Cargo.toml version>, print the gh command
    tools\release.ps1 -Publish                          # ... and publish it as a pre-release (canary)
    tools\release.ps1 -Version 0.0.1 -DryRun            # pipeline test with a throwaway key
    tools\release.ps1 -PromoteLatest v0.2.1             # print the promotion command after the soak
    tools\release.ps1 -PromoteLatest v0.2.1 -Publish    # ... and promote
#>
[CmdletBinding(DefaultParameterSetName = 'Release')]
param(
    [Parameter(ParameterSetName = 'Release')] [string] $NotesFile,
    [Parameter(ParameterSetName = 'Release')] [string] $Version,
    [Parameter(ParameterSetName = 'Release')] [string] $MinVersion,
    [Parameter(ParameterSetName = 'Release')] [string] $KeyDir = (Join-Path $env:USERPROFILE '.fanrgb-release'),
    [Parameter(ParameterSetName = 'Release')] [ValidateSet('primary', 'backup')] [string] $Key = 'primary',
    [Parameter(ParameterSetName = 'Release')] [string] $OutDir,
    [Parameter(ParameterSetName = 'Release')] [string] $Branch = 'main',
    [Parameter(ParameterSetName = 'Release')] [switch] $SkipBuild,
    [Parameter(ParameterSetName = 'Release')] [switch] $Prerelease = $true,
    [Parameter(ParameterSetName = 'Promote', Mandatory = $true)] [string] $PromoteLatest,
    [Parameter(ParameterSetName = 'Promote')] [switch] $SkipSoak,
    [switch] $DryRun,
    [switch] $Publish
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Repo = 'Zeropodaerodynamist/fanrgb'
$Workflow = 'ci.yml'
$MaxNotes = 8 * 1024
$Root = Split-Path $PSScriptRoot -Parent
$script:warnings = 0
function Fail([string] $msg) { Write-Host "release: $msg" -ForegroundColor Red; exit 1 }
function Step([string] $msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Run([string] $exe, [string[]] $argv) {
    & $exe @argv
    if ($LASTEXITCODE -ne 0) { Fail "$exe $($argv -join ' ') failed ($LASTEXITCODE)" }
}
# A failed precondition: fatal for a real release, a warning in a dry run (the pipeline still runs).
function Check([bool] $ok, [string] $msg) {
    if ($ok) { return }
    if ($DryRun) {
        Write-Host "dry run: would refuse: $msg" -ForegroundColor Yellow
        $script:warnings++
    } else {
        Fail $msg
    }
}
# A native command whose failure is handled by the caller (stderr discarded). Windows PowerShell turns
# redirected stderr into errors, which 'Stop' would make fatal: 'Continue' in this scope only.
function Native([string] $exe, [string[]] $argv) {
    $ErrorActionPreference = 'Continue'
    $out = & $exe @argv 2>$null
    return [pscustomobject]@{ Ok = ($LASTEXITCODE -eq 0); Out = $out }
}
function Invoke-RepoGit([string[]] $argv) { return Native 'git' (@('-C', $Root) + $argv) }
# Enable-PathRemap / Restore-Rustflags: no local paths in the release build.
. (Join-Path $PSScriptRoot 'remap-paths.ps1')
function Show-Command([string] $exe, [string[]] $argv) {
    $shown = $argv | ForEach-Object { if ($_ -match '\s') { "`"$_`"" } else { $_ } }
    Write-Host "  $exe $($shown -join ' ')"
}

if ($Publish -and $DryRun) { Fail '-DryRun cannot be combined with -Publish' }

# =====================================================================================
# Promote a soaked pre-release to latest
if ($PSCmdlet.ParameterSetName -eq 'Promote') {
    if ($PromoteLatest -notmatch '^v\d+\.\d+\.\d+$') { Fail "tag $PromoteLatest is not vMAJOR.MINOR.PATCH" }
    if (-not (Get-Command gh -ErrorAction SilentlyContinue)) { Fail 'the GitHub CLI (gh) is not installed' }
    $view = Native 'gh' @('release', 'view', $PromoteLatest, '--repo', $Repo, '--json', 'tagName,isPrerelease,isDraft,publishedAt,targetCommitish,assets')
    if (-not $view.Ok -or -not $view.Out) { Fail "release $PromoteLatest not found in $Repo" }
    $rel = ($view.Out -join "`n") | ConvertFrom-Json
    if ($rel.isDraft) { Fail "$PromoteLatest is a draft; publish it as a pre-release first" }
    Check $rel.isPrerelease "$PromoteLatest is not a pre-release (already promoted?)"
    $names = @($rel.assets | ForEach-Object { $_.name })
    foreach ($a in @('fanrgb.exe', 'release.json', 'release.json.minisig')) {
        Check ($names -contains $a) "$PromoteLatest has no $a asset"
    }
    $published = ([DateTime] $rel.publishedAt).ToUniversalTime()
    $age = [DateTime]::UtcNow - $published
    Write-Host ("{0}: published {1:u} ({2:N1} h ago), target {3}" -f $PromoteLatest, $published, $age.TotalHours, $rel.targetCommitish)
    if (-not $SkipSoak) {
        Check ($age.TotalHours -ge 24) "$PromoteLatest was published $([math]::Round($age.TotalHours, 1)) h ago; the canary soak is 24-48 h (-SkipSoak overrides)"
    }
    $editArgs = @('release', 'edit', $PromoteLatest, '--repo', $Repo, '--prerelease=false', '--latest')
    if ($Publish) {
        Step "gh $($editArgs -join ' ')"
        Run 'gh' $editArgs
        Write-Host "$PromoteLatest is now the latest release; the app's updater offers it." -ForegroundColor Green
    } else {
        Write-Host ''
        Write-Host 'To promote (or run again with -Publish):' -ForegroundColor Green
        Show-Command 'gh' $editArgs
    }
    exit 0
}

# =====================================================================================
# Release
# ---- 1. Preconditions: version, clean tree, pushed and CI-green commit, tag
$cargoToml = Get-Content -LiteralPath (Join-Path $Root 'Cargo.toml') -Raw
if ($cargoToml -notmatch '(?m)^version\s*=\s*"(\d+\.\d+\.\d+)"') { Fail 'no version in Cargo.toml' }
$cargoVersion = $Matches[1]
if (-not $Version) { $Version = $cargoVersion }
if ($Version -notmatch '^\d+\.\d+\.\d+$') { Fail "version $Version is not MAJOR.MINOR.PATCH" }
$tag = "v$Version"
Check ($Version -eq $cargoVersion) "version $Version differs from Cargo.toml ($cargoVersion); bump Cargo.toml first"
$cargoLock = Get-Content -LiteralPath (Join-Path $Root 'Cargo.lock') -Raw
$lockOk = $cargoLock -match "(?m)^name = `"fanrgb`"\r?\nversion = `"$([regex]::Escape($cargoVersion))`""
Check $lockOk "Cargo.lock does not list fanrgb $cargoVersion; run cargo build once and commit Cargo.lock"

$head = Invoke-RepoGit @('rev-parse', '--verify', 'HEAD^{commit}')
if (-not $head.Ok) { Fail 'not a git checkout' }
$sha = "$($head.Out)".Trim()
if ($sha -notmatch '^[0-9a-f]{40}$') { Fail "unexpected HEAD $sha" }
Step "commit $sha"
$status = Invoke-RepoGit @('status', '--porcelain', '--untracked-files=normal')
Check (-not $status.Out) 'the git tree has uncommitted or untracked files; commit (or remove) them so the release matches a commit'

$fetch = Invoke-RepoGit @('fetch', '--quiet', 'origin', $Branch)
Check $fetch.Ok "could not fetch origin/$Branch"
$pushed = $fetch.Ok -and (Invoke-RepoGit @('merge-base', '--is-ancestor', $sha, "refs/remotes/origin/$Branch")).Ok
$containing = (Invoke-RepoGit @('branch', '-r', '--contains', $sha)).Out
if ($containing) { Write-Host "on: $((@($containing) | ForEach-Object { $_.Trim() }) -join ', ')" }
Check $pushed "HEAD $sha is not on origin/${Branch}: push it (and let CI pass) first"

$haveGh = [bool](Get-Command gh -ErrorAction SilentlyContinue)
Check $haveGh 'the GitHub CLI (gh) is not installed'
if ($haveGh) {
    $runs = Native 'gh' @('run', 'list', '--repo', $Repo, '--workflow', $Workflow, '--commit', $sha, '--json', 'databaseId,status,conclusion,event', '--limit', '20')
    $green = $false
    if ($runs.Ok -and $runs.Out) {
        $list = ($runs.Out -join "`n") | ConvertFrom-Json
        foreach ($r in $list) {
            Write-Host "CI run $($r.databaseId) ($($r.event)): $($r.status) $($r.conclusion)"
            if ($r.status -eq 'completed' -and $r.conclusion -eq 'success') { $green = $true }
        }
    }
    Check $green "no successful '$Workflow' run for $sha (gh run list --workflow $Workflow --commit $sha)"
    $existing = Native 'gh' @('release', 'view', $tag, '--repo', $Repo)
    Check (-not $existing.Ok) "release $tag already exists (a bad release gets a newer version, never a re-publish)"
}
$remoteTag = (Invoke-RepoGit @('ls-remote', '--tags', 'origin', "refs/tags/$tag", "refs/tags/$tag^{}")).Out
if ($remoteTag) {
    # Annotated tags list the tag object and the peeled commit (^{}); lightweight tags only the commit.
    $lines = @($remoteTag)
    $peeled = $lines | Where-Object { $_ -match '\^\{\}$' }
    $tagCommit = if ($peeled) { ($peeled -split '\s+')[0] } else { ($lines[0] -split '\s+')[0] }
    Check ($tagCommit -eq $sha) "tag $tag already exists on origin at $tagCommit, not at $sha"
    if ($tagCommit -eq $sha) { Write-Host "tag $tag already on origin at $sha (gh reuses it)" }
}

# ---- 2. Release notes
if ($NotesFile) {
    if (-not (Test-Path -LiteralPath $NotesFile -PathType Leaf)) { Fail "notes file $NotesFile not found" }
    $notes = Get-Content -LiteralPath $NotesFile -Raw
} else {
    $changelog = Join-Path $Root 'CHANGELOG.md'
    if (-not (Test-Path -LiteralPath $changelog)) { Fail 'CHANGELOG.md not found (or pass -NotesFile)' }
    $text = (Get-Content -LiteralPath $changelog -Raw -Encoding UTF8) -replace "`r`n", "`n"
    $m = [regex]::Match($text, "(?ms)^## \[$([regex]::Escape($Version))\]([^\n]*)\n(.*?)(?=^## \[|\z)")
    if (-not $m.Success) {
        Check $false "CHANGELOG.md has no '## [$Version]' section"
        $notes = "Dry run of FanRGB $Version (no CHANGELOG section)."
    } else {
        # The release commit dates its section: "## [0.2.0] - 2026-10-21".
        Check ($m.Groups[1].Value -match '\d{4}-\d{2}-\d{2}') "the CHANGELOG.md heading '## [$Version]$($m.Groups[1].Value)' has no release date"
        $notes = $m.Groups[2].Value
    }
}
$notes = ($notes -replace "`r`n", "`n").Trim()
if (-not $notes) { Fail "the release notes for $Version are empty" }
$noteBytes = [System.Text.Encoding]::UTF8.GetByteCount($notes)
if ($noteBytes -gt $MaxNotes) { Fail "the release notes are $noteBytes bytes; the manifest allows $MaxNotes" }

if ($DryRun) {
    # Never the real key: a throwaway pair, created after the build.
    $throwaway = Join-Path ([System.IO.Path]::GetTempPath()) ('fanrgb-dryrun-' + [guid]::NewGuid().ToString('N'))
    $secret = Join-Path $throwaway 'primary.key'
    $public = Join-Path $throwaway 'primary.pub'
} else {
    $secret = Join-Path $KeyDir "$Key.key"
    $public = Join-Path $KeyDir "$Key.pub"
    if (-not (Test-Path -LiteralPath $secret)) { Fail "signing key $secret not found (run: fanrgb-release keygen --dir `"$KeyDir`")" }
}

$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $Root 'target' }
if (-not $OutDir) {
    $OutDir = if ($DryRun) { Join-Path $target "release-dist\dryrun-$Version" } else { Join-Path $target "release-dist\$Version" }
}

# ---- 3. Build
Push-Location $Root
try {
    if (-not $SkipBuild) {
        Step "cargo build --release --locked (FanRGB $cargoVersion)"
        # Only the shipped app gets the path remapping; the environment is restored right after.
        $savedRustflags = Enable-PathRemap $Root $target
        try { Run 'cargo' @('build', '--release', '--locked') } finally { Restore-Rustflags $savedRustflags }
    }
    Step 'building the signing tool'
    # Same target folder as the app (the tool is its own workspace and would otherwise build into
    # tools\fanrgb-release\target, where $tool below does not look).
    Run 'cargo' @('build', '--release', '--quiet', '--locked', '--target-dir', $target, '--manifest-path', (Join-Path $Root 'tools\fanrgb-release\Cargo.toml'))
} finally { Pop-Location }
$exe = Join-Path $target 'release\fanrgb.exe'
$tool = Join-Path $target 'release\fanrgb-release.exe'
if (-not (Test-Path -LiteralPath $exe)) { Fail "$exe not found" }

# The executable must report the version the manifest announces and be built from this commit.
$bytes = [System.IO.File]::ReadAllBytes($exe)
$marker = [System.Text.Encoding]::ASCII.GetString($bytes)
if (-not $marker.Contains("FANRGB_VERSION=$cargoVersion;")) { Fail "$exe does not carry version $cargoVersion" }
if ($Version -ne $cargoVersion) { Write-Host "dry run: manifest version $Version, executable $cargoVersion" -ForegroundColor Yellow }
$describe = "$((Invoke-RepoGit @('describe', '--always', '--dirty', '--abbrev=10')).Out)".Trim()
Check ($describe -and -not $describe.EndsWith('-dirty') -and $marker.Contains($describe)) "$exe does not carry the commit stamp '$describe' of a clean HEAD (stale build or -SkipBuild?)"
# No folder names of this PC in the exe (tools\remap-paths.ps1); an exe from elsewhere (-SkipBuild) may have them.
$wide = [System.Text.Encoding]::Unicode.GetString($bytes)
$localPaths = @(@($Root, $env:USERPROFILE, $env:CARGO_HOME) | Where-Object { $_ } | ForEach-Object { $_.TrimEnd('\') } |
        Where-Object { $marker.IndexOf($_, [System.StringComparison]::OrdinalIgnoreCase) -ge 0 -or
            $wide.IndexOf($_, [System.StringComparison]::OrdinalIgnoreCase) -ge 0 })
Check ($localPaths.Count -eq 0) "$exe contains local paths ($($localPaths -join ', ')); build it with this script, without -SkipBuild"
$exeSha = (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant()
Write-Host "fanrgb.exe $($bytes.Length) bytes, sha256 $exeSha, stamp $describe"

# ---- 4. Manifest + signature
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$distExe = Join-Path $OutDir 'fanrgb.exe'
$manifest = Join-Path $OutDir 'release.json'
$signature = "$manifest.minisig"
$notesOut = Join-Path $OutDir 'notes.md'
[System.IO.File]::WriteAllText($notesOut, $notes, (New-Object System.Text.UTF8Encoding($false)))
Copy-Item -LiteralPath $exe -Destination $distExe -Force
Remove-Item -LiteralPath $signature -ErrorAction SilentlyContinue
Step 'writing release.json'
$margs = @('manifest', '--exe', $distExe, '--version', $Version, '--notes-file', $notesOut, '--out', $manifest)
if ($MinVersion) { $margs += @('--min-version', $MinVersion) }
Run $tool $margs

# The licence files every release ships with (docs/RELEASING.md, "Licences"), copied next to the exe.
# Not part of release.json: the signature covers fanrgb.exe only, and the updater ignores other assets.
$licenceFiles = @('LICENSE', 'LICENSE-MPL-2.0', 'NOTICE', 'THIRD-PARTY-LICENSES.html', 'THIRD-PARTY-LICENSES.txt' |
        ForEach-Object { Join-Path $Root $_ })
$licenceFiles += @(Get-ChildItem -LiteralPath (Join-Path $Root 'licenses') -File | Sort-Object Name | ForEach-Object { $_.FullName })
# LGPL-2.1: the source of the embedded PawnIO modules travels with every binary (assets/pawnio/README).
$licenceFiles += @(Get-ChildItem -LiteralPath (Join-Path $Root 'assets\pawnio\source') -File | Sort-Object Name | ForEach-Object { $_.FullName })
$licenceAssets = @()
$assetNames = @('fanrgb.exe', 'release.json', 'release.json.minisig')
foreach ($f in $licenceFiles) {
    if (-not (Test-Path -LiteralPath $f -PathType Leaf) -or (Get-Item -LiteralPath $f).Length -eq 0) { Fail "licence file $f is missing or empty" }
    $name = Split-Path $f -Leaf
    if ($assetNames -contains $name) { Fail "two release assets would be named $name" }
    $assetNames += $name
    $copy = Join-Path $OutDir $name
    Copy-Item -LiteralPath $f -Destination $copy -Force
    $licenceAssets += $copy
}
Write-Host "licence files: $(($licenceAssets | ForEach-Object { Split-Path $_ -Leaf }) -join ', ')"

$ghArgs = @('release', 'create', $tag, $distExe, $manifest, $signature) + $licenceAssets + @('--repo', $Repo,
    '--target', $sha, '--title', "FanRGB $Version", '--notes-file', $notesOut)
if ($Prerelease) { $ghArgs += @('--prerelease', '--latest=false') } else { $ghArgs += @('--latest') }

if ($DryRun) {
    Step 'dry run: signing with a throwaway key (the real keys are not used)'
    try {
        Run $tool @('keygen', '--dir', $throwaway, '--no-password')
        Run $tool @('sign', '--key', $secret, '--pub', $public, '--file', $manifest)
        $throwawayPub = (Get-Content -LiteralPath $public)[1].Trim()
        Step 'verifying with the throwaway public key'
        Run $tool @('verify', '--pubkey', $throwawayPub, '--file', $manifest)
    } finally {
        Remove-Item -LiteralPath $throwaway -Recurse -Force -ErrorAction SilentlyContinue
    }
    Write-Host ''
    Write-Host "Dry run OK ($($script:warnings) precondition(s) would refuse a real release). $OutDir is signed by a throwaway key; FanRGB will not install it." -ForegroundColor Green
    Write-Host 'A real release would publish with:'
    Show-Command 'gh' $ghArgs
    exit 0
}
Step "signing with the $Key key (asks for its password)"
Run $tool @('sign', '--key', $secret, '--pub', $public, '--file', $manifest, '--require-encrypted')

# ---- 5. Verify with the key the app trusts
$keysRs = Get-Content -LiteralPath (Join-Path $Root 'src\updater\keys.rs') -Raw
$constName = if ($Key -eq 'primary') { 'PRIMARY' } else { 'BACKUP' }
if ($keysRs -notmatch "pub const $constName`: &str = `"([A-Za-z0-9+/=]+)`"") { Fail "no $constName key in src\updater\keys.rs" }
$compiled = $Matches[1]
$pubLine = (Get-Content -LiteralPath $public)[1].Trim()
if ($pubLine -ne $compiled) { Fail "$public is not the $constName key compiled into the app" }
Step 'verifying with the compiled-in public key'
Run $tool @('verify', '--pubkey', $compiled, '--file', $manifest)

# ---- 6. Publish
if ($Publish) {
    # Nothing may have moved while the password prompt was open.
    $now = "$((Invoke-RepoGit @('rev-parse', '--verify', 'HEAD^{commit}')).Out)".Trim()
    if ($now -ne $sha) { Fail "HEAD moved from $sha to $now during the release; start again" }
    if ((Invoke-RepoGit @('status', '--porcelain', '--untracked-files=normal')).Out) { Fail 'the git tree changed during the release; start again' }
    Step "gh $($ghArgs -join ' ')"
    Run 'gh' $ghArgs
    if ($Prerelease) {
        Write-Host "Published $tag as a pre-release at $sha. Install it by hand, soak 24-48 h, then: tools\release.ps1 -PromoteLatest $tag -Publish" -ForegroundColor Green
    } else {
        Write-Host "Published $tag as the latest release at $sha." -ForegroundColor Green
    }
} else {
    Write-Host ''
    Write-Host "Release files in $OutDir. To publish (or run again with -Publish):" -ForegroundColor Green
    Show-Command 'gh' $ghArgs
}
