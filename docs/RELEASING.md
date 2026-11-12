# Releasing FanRGB

The runbook for a release, the canary soak, rollback, the signing keys and what to do if a key or token leaks.
Keys: [KEYS.md](KEYS.md).

**Who builds and signs.** The maintainer's PC builds and signs every release (`tools/release.ps1`); the signing
key never goes to CI. CI (`.github/workflows/ci.yml`) is the second machine: it must be green on the exact
commit, and at the tag it rebuilds the same commit with `cargo auditable`, writes a CycloneDX SBOM, a SHA-256 and
(where the plan allows) a build-provenance attestation. Until the build is reproducible the CI hash is
informational evidence, not the installed file's provenance.

## Rules

- **Versions:** `0.MINOR.PATCH`. MINOR = behaviour users notice, PATCH = fixes and threshold tuning.
  `Cargo.toml` is the single source; the release commit ("Release 0.2.1") bumps `Cargo.toml` and `Cargo.lock` and
  dates its `CHANGELOG.md` section (`## [0.2.1] - 2026-10-21`). That commit is what the tag points at.
- **Changelog:** every PR adds a line under `## [Unreleased]`; the release commit renames it to the version.
  `release.ps1` takes the release notes from that section (at most 8 KiB).
- **A version number is used once.** Never delete or edit a published release's assets, never re-publish a tag.
  A bad release gets a newer, fixed release, so a failed update burns a PATCH number. That is expected.
  Note what the updater does and does not remember: only a version left with `--revert` is recorded and never
  offered again. After an **automatic** rollback (the 180 s health check) the version is shown once as rolled
  back but is **offered again** at the next check for as long as it is the latest release (by design: a version
  that never ran must stay installable). Nothing in the app can withdraw a signed release: take a bad one out of
  "latest" on GitHub and ship the fix under a higher version quickly.
- **Tags** `vX.Y.Z` are created only by `release.ps1` (`gh release create --target <sha>`), only on a commit that
  is on `origin/main` and has a green `ci` run. Optionally sign it first: `git tag -s vX.Y.Z <sha>` (SSH signing
  key), `git push origin vX.Y.Z`; `release.ps1` checks that the tag points at the same commit and reuses it.
- `min_version` in the manifest only when `config.json`, `update_state.json` or `cooling_baseline.json` change
  incompatibly (record it in the changelog's "Data formats").
- Error codes (`FRG-Exxx`) are never renumbered.
- **Licences:** when `Cargo.lock` changes, regenerate the crate licence notices with
  `cargo about generate --locked about.hbs -o THIRD-PARTY-LICENSES.html` and
  `cargo about generate --locked about-text.hbs -o THIRD-PARTY-LICENSES.txt`, and commit both (`fanrgb.exe`
  embeds the `.txt` and shows it under Settings › About › Licences, so build after regenerating). Every release
  ships `fanrgb.exe` with `LICENSE`, `LICENSE-MPL-2.0`, `NOTICE`, `THIRD-PARTY-LICENSES.html`,
  `THIRD-PARTY-LICENSES.txt` and the files in `licenses/`: `release.ps1` copies them into the release folder and
  attaches them to the GitHub release next to the exe. They are not part of the signed `release.json`, which
  covers `fanrgb.exe` only.
- **No local paths in the exe:** `release.ps1` and `dist.ps1` build with `--remap-path-prefix` for the cargo home,
  the toolchain and the repository (`tools/remap-paths.ps1`), set for that build only. CI and the
  `cargo auditable` build run on throwaway GitHub runners and do not need it.

## Release steps

1. **On `main`, CI green.** Merge the release PR (squash). `git switch main; git pull --ff-only`.
   For any change under `src/hw`, `src/control` or `src/engine.rs` the PR says whether it ran on the IT8689E.
2. **Release commit.** Bump `version` in `Cargo.toml`, `cargo build --release --locked` once (updates
   `Cargo.lock`), date the `CHANGELOG.md` section, commit "Release X.Y.Z", push, wait for `ci` to pass on it.
3. **Dry run** (throwaway key, real keys never read; reports every precondition that would refuse):
   `tools\release.ps1 -DryRun`
4. **Build and sign** in a fresh console (the key password is asked on the console):
   `tools\release.ps1` — checks version, clean tree, pushed + CI-green HEAD, builds, signs, verifies with the
   compiled-in key and prints the `gh release create ... --target <sha> --prerelease` command.
5. **Publish as a pre-release** (the canary): `tools\release.ps1 -Publish`. A pre-release is invisible to the
   app's updater (`/releases/latest` ignores pre-releases).
6. **Check CI at the tag:** the `provenance` job's summary lists the CI build's SHA-256; compare it with
   `asset.sha256` in `release.json` (expected to differ until the build is reproducible; note it).
7. **Canary soak** (below), then **promote**: `tools\release.ps1 -PromoteLatest vX.Y.Z -Publish`.

## Canary: pre-release, soak, promote

1. Install the pre-release by hand on the maintainer PC:
   ```
   gh release download vX.Y.Z --repo Zeropodaerodynamist/fanrgb --dir %TEMP%\fanrgb-vX.Y.Z
   ```
   verify it (`fanrgb-release verify --pubkey <PRIMARY from keys.rs> --file release.json`, and the SHA-256 of
   `fanrgb.exe` against `release.json`), then run `fanrgb.exe --install` from that folder (elevated).
2. **Soak 24-48 h** on the installed copy in normal use (Balanced or the usual mode). Watch:
   the log has no FRG-E2xx/E30x storm, `--diag` registers on hand-back equal the reference image, the tray stays
   idle (≤ 0.3 % of a core), no WER crash for `fanrgb.exe` in Reliability Monitor.
3. **Promote:** `tools\release.ps1 -PromoteLatest vX.Y.Z` (prints the command; refuses < 24 h unless
   `-SkipSoak`), then again with `-Publish`. It runs
   `gh release edit vX.Y.Z --prerelease=false --latest`; nothing is rebuilt or re-signed. From then on the
   updater offers it.
4. **A canary that fails:** leave it a pre-release, fix, release X.Y.(Z+1).

First releases (review §1(b)): v0.2.0 is installed by hand with `--install` and stays a pre-release; v0.2.1
(same code, version bump) validates the updater including one deliberate rollback; v0.2.2 becomes the first
release marked latest.

## After an update: what to check

The log lines: the supervisor's "is healthy; keeping X as fanrgb.exe.old", the new version banner, "Task
unchanged"; a `--diag` register image identical to the one before (the old instance released the fans, the new
one took them again); `update_state.json` without `pending`.

## Rollback

In order of preference:

1. **Automatic:** the `--update-finish` supervisor rolls back when the new version is not healthy within 180 s
   (60 s of steady engine ticks; a clean user exit is not a failure).
2. **By hand:** `fanrgb.exe --revert` (asks first, then elevates; `--yes` skips the question). It stops FanRGB
   (every fan back to the BIOS during the swap), checks `%ProgramFiles%\FanRGB\fanrgb.exe.old` (admin-only, a
   FanRGB build with a version marker), swaps it in (the current one becomes `fanrgb.exe.failed`), reads it back,
   records it (the version left counts as installed, so it is never offered again; `last_installed` never goes
   down) and starts FanRGB again. There is one step back: the updater keeps exactly one `.old`. Refused while an
   update hand-over is still being decided (wait 4 minutes).
3. **Reinstall** a known-good build: `fanrgb.exe --install` from it.

**Update record reset** (a reinstall after a bad or wrongly signed release that left a high `last_installed`, or
a replayed `published_at`): `fanrgb.exe --reset-update-state` (asks first, elevated; `--yes` skips the question).
It moves `%ProgramData%\FanRGB\update_state.json` aside as `update_state.json.reset-<time>.bak`; the next check
starts from the running version. Do it only after reinstalling from a trusted build.

## Key ceremony and rotation

See [KEYS.md](KEYS.md): inventory, the one-time ceremony (protect the keys, two offline backups, delete the backup
keys from the PC) and planned rotation (new key next to the current one, ship, then switch). Routine signing: only
in a fresh console, the passphrase never stored on the PC. Signing on a separate offline machine is the next
step up.

## Incident: a key or token leaked

Assume the worst case for each column: an attacker who has the key may also have repository write access (a `gh`
token may live on the same PC).

| Step | Release key leaked | Catalog key leaked | GitHub token / account |
|---|---|---|---|
| T+0 | Revoke the `gh` token and every PAT (GitHub › Settings › Developer settings), change the account password, check 2FA, review the security log and the Releases list for foreign releases or tags; delete them. | Rotate the Actions secret: `fanrgb-release catalog-keygen` (overwrites `CATALOG_MINISIGN_KEY`). Check the `catalog` branch history for commits not made by the workflow. | Revoke and rotate; review the audit log, deploy keys, Actions secrets, webhooks, collaborators. |
| T+1 h | On the PC: `fanrgb.exe --exit`; check the hashes in `%ProgramFiles%\FanRGB` against the release manifests; look in `%LOCALAPPDATA%\FanRGB\update\staged`. If anything differs: `--uninstall`, rebuild from the tag, `--install`. | Nothing on the PC: a bad catalog can only mislead text, it has no install path. | As for the release key if a release was published meanwhile. |
| T+24 h | Ship a release signed with the **backup** key (`tools\release.ps1 -Key backup`) whose `keys.rs` holds two **new** keys; sign with the new primary from then on. Say so in the release notes. If the attacker published a high version, users on it reinstall by hand and run `fanrgb.exe --reset-update-state`. | Sign the next catalog by hand with the catalog backup key (higher serial), commit it to `catalog`, ship new catalog public keys in the next app release. | Create a new fine-grained read-only PAT; paste it in Settings › Updates. |
| Post-mortem | How did the key leave the PC? Move signing off the FanRGB machine. | Was the secret in a workflow log? Pinned actions, `::add-mask::`. | Passkeys; fine-grained tokens instead of classic `repo` scope. |

## CI and branch protection

`ci.yml` jobs: `fmt`, `clippy` (shipped code: no new warnings; the 7 known pre-v0.2 lints are allowed by name),
`test` (`cargo test --release --locked`, private TEMP), `audit` (`cargo audit`, `cargo deny` with `deny.toml`),
`build-release` (exe + PDB artifacts, 14 days), and on tags `provenance` (cargo-auditable build, CycloneDX SBOM,
`actions/attest-build-provenance`; attestations need a public repository or GitHub Enterprise Cloud, so on a
private repository that step may fail without failing the job). Make `fmt`, `clippy`, `test`, `audit`,
`build-release` required checks for `main` once the plan allows rulesets; otherwise `release.ps1` is the gate (it
refuses a commit without a green `ci` run).

The toolchain is pinned in `rust-toolchain.toml`; bump it in its own PR with CI green.
