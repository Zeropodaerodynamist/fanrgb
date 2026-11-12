# Builds: which exe to run, and where old ones go

FanRGB is **one program, `fanrgb.exe`**. There is no installer and no second exe to ship. What can look
like more than one:

| You see | What it is |
| --- | --- |
| Two `fanrgb.exe` in Task Manager | The app and its **crash watchdog** (`fanrgb.exe --watchdog <pid>`, same file). The watchdog hands the fans back to the BIOS if the app crashes. It exits with the app. Short-lived helpers (`--driver-check`, `--update-fetch`, `--catalog-fetch`) are the same exe as well. |
| `fanrgb-release.exe` | Maintainer tool for signing releases (`tools/fanrgb-release`). Never shipped and never run by the app. |
| Many `fanrgb.exe` under `target\` | cargo build output: build caches only, never something to run. |

## The layout

```
dist\                      git-ignored; the executables people run
  current\fanrgb.exe       THE build to use / test now
  current\BUILD.txt        version, commit, build time, SHA-256
  archive\<id>\            every earlier build, same two files
target\                    cargo's build cache only; never run from here
  dist\                    the folder tools\dist.ps1 builds into
  debug\, release\         plain `cargo build` / `cargo test`
```

`<id>` = `<date>_v<version>_<commit>`, so the archive sorts by date. A commit ending in `-dirty` was built
with uncommitted changes.

**Why not run from `target\`?** A running exe is locked. While FanRGB ran from `target\release`, every
`cargo build --release` failed ("being used by another process"). That's why the extra `target\rc`,
`target\rc2`, … folders appeared. Running from `dist\current` keeps cargo's folder free.

## Making a new build

```powershell
.\tools\dist.ps1
```

This runs `cargo build --release --locked` into `target\dist`, moves the old `dist\current` into
`dist\archive\<its id>\`, and copies the new exe and a `BUILD.txt` into `dist\current`. If the exe in
`dist\current` is running, the script stops and tells you to exit FanRGB first (tray › Exit).
The build passes `--remap-path-prefix` for the cargo home, the toolchain and the repository
(`tools\remap-paths.ps1`, for that build only), so the exe carries no folder names of this PC.

To archive an exe that was built some other way (the script copies it, so the original can still be running):

```powershell
.\tools\dist.ps1 -ArchiveExe <path\to\fanrgb.exe> -Id 2026-10-20_v0.2.0_<commit or label> -Note "why it matters"
```

## Starting a new build while an old one runs

Only one FanRGB can control the fans. Starting a second copy used to bring the running window to the front
and quit. If that running copy was an *older* build, it looked as if the old app had "popped up" instead
of the new one.

Since 0.2.0 (this change), a second launch checks where the running copy was started from:

- **Same exe:** its window comes to the front, as before.
- **A different exe** (older build, another folder, the Program Files copy): a dialog shows both paths
  and asks *"Close it and start this copy instead?"*
  - **Yes:** the running copy is stopped cleanly (its fans go back to the BIOS), then this copy starts
    (one UAC prompt).
  - **No:** the running copy comes to the front.

This works with a running 0.1 build too, because 0.1 already listens for the clean-exit request
(`fanrgb.exe --exit` uses the same signal).

You can also stop any running copy by hand: tray › **Exit**, or `dist\current\fanrgb.exe --exit`.
