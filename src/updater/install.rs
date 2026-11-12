//! The elevated half of an update: re-verify what the unelevated helper
//! staged, swap it into `%ProgramFiles%\FanRGB`, hand over to the new version and roll back if it does
//! not come up healthy.
//!
//! ```text
//! old instance (elevated)          --update-finish (old exe, elevated)     new instance (elevated)
//!  verify staged files               waits for the old pid to exit
//!  fanrgb.exe.new (CREATE_NEW,       starts fanrgb.exe --restart-after ───►  engine ticks for 60 s
//!   read back, SHA-256)              polls update_state.json  ◄──────────── pending.healthy = true
//!  fanrgb.exe → fanrgb.exe.old       healthy: clear pending, exit
//!  .new → fanrgb.exe                 exited / 180 s: kill it, fanrgb.exe → .failed,
//!  record pending, start finisher      .old → fanrgb.exe, start it
//!  request_exit (fans → BIOS)
//! ```
//!
//! The old instance's watchdog ignores the new instance's crash snapshot (different pid,
//! `system::validate_snapshot`), and the new instance starts only after the old one has handed the fans
//! back to the BIOS (`--restart-after`). The scheduled task keeps starting `fanrgb.exe`, so autostart
//! needs no change. `update_state.json` lives in the admin-only state folder; an update is refused while
//! FanRGB runs on a temporary fallback state folder (FRG-E712), because the finisher and the new version
//! would each get a different one and never see `pending`.
//!
//! The version only counts as installed (`last_installed`, downgrade protection) once it has proved
//! healthy, so a version that never started can be installed again. A user who closes the new version
//! during the health check (tray Exit, sign-out) is not a failure: the new instance records
//! `pending.clean_exit_pid` and the supervisor keeps the update without rolling back.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::fetch::{MANIFEST_NAME, SIGNATURE_NAME, STAGED_EXE};
use super::manifest::{self, Release, Seen, Version, sha256_hex};
use crate::engine::Shared;
use crate::error::{ErrorCode, WithCode, err};
use crate::util::fsx::{self, DirCheck};
use crate::util::paths;

pub const STATE_FILE: &str = "update_state.json";
pub const FINISH_ARG: &str = "--update-finish";
const NEW_SUFFIX: &str = ".new";
const OLD_SUFFIX: &str = ".old";
const FAILED_SUFFIX: &str = ".failed";
/// The new version must report healthy within this time or it is rolled back.
pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(180);
/// How long the engine must tick without a gap before the new version counts as healthy.
pub const HEALTHY_AFTER: Duration = Duration::from_secs(60);
const MAX_STATE: u64 = 64 * 1024;

/// Persistent update state (`%ProgramData%\FanRGB\update_state.json`, admin-only).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateRecord {
    /// Highest version installed by an update (downgrade protection).
    #[serde(default)]
    pub last_installed: Option<String>,
    /// Newest `published_at` of any manifest that passed verification (replay protection).
    #[serde(default)]
    pub last_published: u64,
    /// An update in progress (written before the hand-over, cleared by `--update-finish`).
    #[serde(default)]
    pub pending: Option<Pending>,
    /// Version that was rolled back last (shown once by the UI).
    #[serde(default)]
    pub rolled_back: Option<String>,
    /// One line about the last update, for the UI and the log.
    #[serde(default)]
    pub last_result: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub to: String,
    pub from: String,
    pub at: u64,
    pub new_sha256: String,
    pub old_sha256: String,
    #[serde(default)]
    pub healthy: bool,
    #[serde(default)]
    pub healthy_pid: u32,
    /// Pid of a new instance that was closed cleanly (by the user, sign-out, or because another
    /// instance runs) before it reported healthy: not a failed update.
    #[serde(default)]
    pub clean_exit_pid: u32,
}

impl UpdateRecord {
    pub fn seen(&self) -> Seen {
        Seen {
            last_installed: self.last_installed.as_deref().and_then(Version::parse),
            last_published: self.last_published,
        }
    }

    /// Records `to` as installed (downgrade protection) once it proved healthy; never lowers it.
    pub fn record_installed(&mut self, to: &str) {
        let Some(v) = Version::parse(to) else { return };
        if self.seen().last_installed.is_none_or(|last| v > last) {
            self.last_installed = Some(to.to_string());
        }
    }
}

pub fn state_file() -> PathBuf {
    paths::data_dir().join(STATE_FILE)
}

/// Reads the record; a missing or unreadable one is the default (nothing seen yet). In the admin-only
/// state folder the file must also be owned by Administrators/SYSTEM.
pub fn load_record(path: &Path) -> UpdateRecord {
    let bytes = if paths::state_dir_secure() {
        fsx::read_limited_admin_owned(path, MAX_STATE)
    } else {
        fsx::read_limited(path, MAX_STATE)
    };
    match bytes {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
            log::warn!("[{}] {} unreadable ({e}); starting fresh", ErrorCode::Io, path.display());
            UpdateRecord::default()
        }),
        Err(_) => UpdateRecord::default(),
    }
}

pub fn save_record(path: &Path, rec: &UpdateRecord) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(rec).code(ErrorCode::Io)?;
    fsx::write_atomic(path, &bytes).code(ErrorCode::Io)
}

// ---------------------------------------------------------------------------------------
// Verification of the staged files

/// A staged update that passed every check.
pub struct Verified {
    pub release: Release,
    pub exe: Vec<u8>,
    /// 0 = primary key, 1 = backup key.
    pub key: usize,
}

/// Re-verifies the helper's download (a folder the user can write, so: hostile input).
pub fn verify_staged(staged: &Path, keys: &[&str], current: Version, seen: Seen, now: u64) -> Result<Verified> {
    let read = |name: &str, max: u64| {
        fsx::read_limited(&staged.join(name), max)
            .map_err(|e| err(ErrorCode::UpdatePackage, format!("staged {name}: {e}")))
    };
    let m = read(MANIFEST_NAME, manifest::MAX_MANIFEST)?;
    let s = read(SIGNATURE_NAME, manifest::MAX_SIGNATURE)?;
    let key = manifest::verify_signature(&m, &s, keys)?;
    let release = manifest::parse(&m)?;
    manifest::check_offer(&release, current, seen, now)?;
    let exe = read(STAGED_EXE, release.manifest.asset.size)?;
    manifest::verify_package(&release, &exe)?;
    Ok(Verified { release, exe, key })
}

// ---------------------------------------------------------------------------------------
// File swap

fn with_suffix(dir: &Path, suffix: &str) -> PathBuf {
    dir.join(format!("{}{suffix}", paths::EXE_NAME))
}

fn move_file(from: &Path, to: &Path, flags: u32) -> std::io::Result<()> {
    let f = fsx::wide_path(from);
    let t = fsx::wide_path(to);
    // SAFETY: both are NUL-terminated wide paths alive for the call.
    let ok = unsafe { windows_sys::Win32::Storage::FileSystem::MoveFileExW(f.as_ptr(), t.as_ptr(), flags) };
    if ok == 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

fn remove_if_present(p: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(p) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Puts `exe` in place of `<dir>\fanrgb.exe` and keeps the previous file as `fanrgb.exe.old` (exactly one
/// `.old`). The new bytes go to `fanrgb.exe.new` (`CREATE_NEW`, links refused), are flushed, read back
/// and compared by SHA-256 before anything is renamed. A running `fanrgb.exe` can be renamed (not
/// overwritten), so: running → `.old`, `.new` → `fanrgb.exe` (write-through); if the second rename fails
/// the first is undone. Returns the SHA-256 of the previous executable.
pub fn swap_in(dir: &Path, exe: &[u8], sha256: &str) -> Result<String> {
    use std::io::Write;
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
    let code = ErrorCode::UpdateInstall;
    // The folder (and its parents) cannot be renamed or replaced by a junction while this is open.
    let _pin = fsx::open_dir_checked(dir).code(code)?;
    let (target, new, old) = (dir.join(paths::EXE_NAME), with_suffix(dir, NEW_SUFFIX), with_suffix(dir, OLD_SUFFIX));
    let old_sha = sha256_hex(&fsx::read_limited(&target, manifest::MAX_EXE).code(code)?);
    remove_if_present(&new).code(code)?;
    remove_if_present(&with_suffix(dir, FAILED_SUFFIX)).code(code)?;
    let written = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&new)?;
        f.write_all(exe)?;
        f.sync_all()
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&new);
        return Err(err(code, format!("{}: {e}", new.display())));
    }
    match fsx::read_limited(&new, manifest::MAX_EXE) {
        Ok(back) if sha256_hex(&back) == sha256 => {}
        other => {
            let _ = std::fs::remove_file(&new);
            return Err(err(
                ErrorCode::UpdateHash,
                format!("{} does not read back as written ({:?})", new.display(), other.err()),
            ));
        }
    }
    if let Err(e) = remove_if_present(&old) {
        let _ = std::fs::remove_file(&new);
        return Err(err(code, format!("cannot replace {}: {e}", old.display())));
    }
    if let Err(e) = move_file(&target, &old, MOVEFILE_WRITE_THROUGH) {
        let _ = std::fs::remove_file(&new);
        return Err(err(code, format!("renaming {}: {e}", target.display())));
    }
    if let Err(e) = move_file(&new, &target, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) {
        let undo = move_file(&old, &target, MOVEFILE_WRITE_THROUGH);
        let _ = std::fs::remove_file(&new);
        return Err(err(
            code,
            format!("installing {}: {e} (previous version restored: {})", target.display(), undo.is_ok()),
        ));
    }
    Ok(old_sha)
}

/// Puts `fanrgb.exe.old` back as `fanrgb.exe`; the current one (if any) becomes `fanrgb.exe.failed`.
pub fn swap_back(dir: &Path) -> Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
    let code = ErrorCode::UpdateInstall;
    let _pin = fsx::open_dir_checked(dir).code(code)?;
    let (target, old) = (dir.join(paths::EXE_NAME), with_suffix(dir, OLD_SUFFIX));
    if !old.is_file() {
        return Err(err(code, format!("{} is missing; nothing to roll back to", old.display())));
    }
    if target.exists() {
        move_file(&target, &with_suffix(dir, FAILED_SUFFIX), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)
            .map_err(|e| err(code, format!("moving the failed version aside: {e}")))?;
    }
    move_file(&old, &target, MOVEFILE_WRITE_THROUGH).map_err(|e| err(code, format!("restoring {}: {e}", old.display())))
}

// ---------------------------------------------------------------------------------------
// `--revert` and `--reset-update-state` (by hand, elevated)

/// Version an executable reports through its `FANRGB_VERSION=x.y.z;` marker (`updater::VERSION_MARKER`).
/// Every occurrence is tried: the needle itself may also be in the file.
pub fn exe_version(bytes: &[u8]) -> Option<Version> {
    const NEEDLE: &[u8] = b"FANRGB_VERSION=";
    let mut from = 0;
    while let Some(i) = bytes[from..].windows(NEEDLE.len()).position(|w| w == NEEDLE) {
        let rest = &bytes[from + i + NEEDLE.len()..];
        if let Some(end) = rest.iter().take(16).position(|&b| b == b';')
            && let Some(v) = std::str::from_utf8(&rest[..end]).ok().and_then(Version::parse)
        {
            return Some(v);
        }
        from += i + 1;
    }
    None
}

/// What `--revert` would do: put `fanrgb.exe.old` (version `previous`) back in place of the current
/// `fanrgb.exe` (version `current`, if it can be read).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevertPlan {
    pub current: Option<Version>,
    pub previous: Version,
    /// SHA-256 of the `.old` file that was checked; the file put in place must read back as this.
    pub previous_sha256: String,
}

/// Read-only check of `<dir>\fanrgb.exe.old` (also run unelevated, to show what `--revert` will do
/// before asking). It must be a Windows program that carries a FanRGB version marker.
pub fn revert_plan(dir: &Path) -> Result<RevertPlan> {
    let code = ErrorCode::UpdateInstall;
    let old = with_suffix(dir, OLD_SUFFIX);
    if !old.is_file() {
        crate::bail_code!(code, "{} is missing: there is no previous version to go back to", old.display());
    }
    let bytes = fsx::read_limited(&old, manifest::MAX_EXE).code(code)?;
    if !bytes.starts_with(b"MZ") {
        crate::bail_code!(code, "{} is not a Windows program", old.display());
    }
    let previous =
        exe_version(&bytes).ok_or_else(|| err(code, format!("{} does not report a FanRGB version", old.display())))?;
    let current = fsx::read_limited(&dir.join(paths::EXE_NAME), manifest::MAX_EXE).ok().and_then(|b| exe_version(&b));
    Ok(RevertPlan { current, previous, previous_sha256: sha256_hex(&bytes) })
}

/// Puts the checked `.old` back ([`swap_back`]: the current file becomes `fanrgb.exe.failed`) and reads
/// the result back: if `fanrgb.exe` is not the file `plan` checked, the swap is undone.
pub fn revert_files(dir: &Path, plan: &RevertPlan) -> Result<()> {
    use windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH;
    swap_back(dir)?;
    let target = dir.join(paths::EXE_NAME);
    match fsx::read_limited(&target, manifest::MAX_EXE) {
        Ok(b) if sha256_hex(&b) == plan.previous_sha256 => Ok(()),
        other => {
            let failed = with_suffix(dir, FAILED_SUFFIX);
            let undone = move_file(&target, &with_suffix(dir, OLD_SUFFIX), MOVEFILE_WRITE_THROUGH)
                .and_then(|()| move_file(&failed, &target, MOVEFILE_WRITE_THROUGH));
            Err(err(
                ErrorCode::UpdateHash,
                format!(
                    "{} does not read back as the checked previous version ({:?}); swap undone: {}",
                    target.display(),
                    other.err(),
                    undone.is_ok()
                ),
            ))
        }
    }
}

/// An update hand-over the supervisor may still be deciding (`--update-finish` waits up to
/// [`HEALTH_TIMEOUT`] plus the old instance's exit): `--revert` and the reset wait for it.
pub(crate) fn pending_in_progress(rec: &UpdateRecord, now: u64) -> Option<&Pending> {
    rec.pending.as_ref().filter(|p| now.saturating_sub(p.at) < HEALTH_TIMEOUT.as_secs() + 60)
}

/// The update record after going back by hand from `from` to `to`. Anti-downgrade stays consistent:
/// `last_installed` only moves forward, and the version left counts as installed, so the updater never
/// offers it again (like an automatic rollback); a newer release is offered as usual. A stale `pending`
/// is dropped; the UI shows the rollback once.
pub(crate) fn record_revert(rec: &mut UpdateRecord, from: Option<Version>, to: Version) {
    rec.pending = None;
    let from_s = from.map(|v| v.to_string());
    if let Some(f) = &from_s {
        rec.record_installed(f);
        rec.rolled_back = Some(f.clone());
    }
    rec.last_result = Some(format!("went back from {} to {to} by hand (--revert)", from_s.as_deref().unwrap_or("?")));
}

/// `--revert` (elevated, the installed copy): stops a running FanRGB first (it hands every fan back
/// to the BIOS as it exits; nothing drives the fans during the swap), checks `fanrgb.exe.old`, swaps it
/// in, records it in `update_state.json` ([`record_revert`]) and starts the previous version again if
/// FanRGB was running. Returns a line for the console.
pub fn run_revert() -> Result<String> {
    let code = ErrorCode::UpdateInstall;
    let dir = paths::install_dir();
    match fsx::check_dir(&dir).code(code)? {
        DirCheck::Trusted => {}
        DirCheck::Missing => crate::bail_code!(code, "FanRGB is not installed ({} does not exist)", dir.display()),
        other => crate::bail_code!(code, "{} is not admin-only ({other:?}); not touching it", dir.display()),
    }
    check_state_location(paths::state_location())?;
    let path = state_file();
    let mut rec = load_record(&path);
    if let Some(p) = pending_in_progress(&rec, super::now()) {
        crate::bail_code!(code, "the update {} → {} is still being checked; try again in a few minutes", p.from, p.to);
    }
    let plan = revert_plan(&dir)?;
    match fsx::check_file(&with_suffix(&dir, OLD_SUFFIX)) {
        Ok(DirCheck::Trusted) => {}
        other => crate::bail_code!(code, "fanrgb.exe.old is not admin-only ({other:?}); not using it"),
    }
    let was_running = match crate::system::request_instance_exit(Duration::from_secs(15)) {
        crate::system::ExitOutcome::Stopped => true,
        crate::system::ExitOutcome::NotRunning => false,
        other => crate::bail_code!(code, "FanRGB did not stop ({other:?}); nothing was changed"),
    };
    let from = plan.current.map_or_else(|| "?".to_string(), |v| v.to_string());
    log::warn!("--revert: going back from {from} to {} (fans are on the BIOS during the swap)", plan.previous);
    revert_files(&dir, &plan)?;
    record_revert(&mut rec, plan.current, plan.previous);
    if let Err(e) = save_record(&path, &rec) {
        log::warn!("--revert: {e:#}");
    }
    let mut msg = format!(
        "FanRGB {} is back in place of {from} (kept as fanrgb.exe.failed until the next update); {from} will not be offered again.",
        plan.previous
    );
    if was_running {
        match crate::util::win::spawn_same_token(&paths::installed_exe(), &["--minimized"]) {
            Ok(()) => msg.push_str(" FanRGB was started again."),
            Err(e) => msg.push_str(&format!(" Starting it again failed ({e:#}); start FanRGB yourself.")),
        }
    }
    log::warn!("--revert: {msg}");
    Ok(msg)
}

/// `--reset-update-state` (elevated): moves `update_state.json` aside (`update_state.json.reset-<unix
/// time>.bak`), which clears the downgrade (`last_installed`) and replay (`last_published`) records.
/// For a reinstall after a bad or poisoned release. `Ok(None)`:
/// there was nothing to reset.
pub fn reset_update_state() -> Result<Option<PathBuf>> {
    use windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH;
    check_state_location(paths::state_location())?;
    let path = state_file();
    if !path.exists() {
        return Ok(None);
    }
    let rec = load_record(&path);
    if let Some(p) = pending_in_progress(&rec, super::now()) {
        crate::bail_code!(
            ErrorCode::UpdateInstall,
            "the update {} → {} is still being checked; try again in a few minutes",
            p.from,
            p.to
        );
    }
    let backup = path.with_file_name(format!("{STATE_FILE}.reset-{}.bak", super::now()));
    move_file(&path, &backup, MOVEFILE_WRITE_THROUGH)
        .map_err(|e| err(ErrorCode::Io, format!("moving {} aside: {e}", path.display())))?;
    log::warn!(
        "--reset-update-state: update record reset (was: last installed {:?}, last published {}); kept as {}",
        rec.last_installed,
        rec.last_published,
        backup.display()
    );
    Ok(Some(backup))
}

// ---------------------------------------------------------------------------------------
// Applying an update (elevated UI)

/// The hand-over needs the one persistent state folder: the finisher (`--update-finish`) and the new
/// version are separate processes and must find `pending` there. A fallback folder is per process
/// (`FanRGB.<pid>-<random>`), and its downgrade/replay record is lost at exit.
pub(crate) fn check_state_location(loc: paths::StateLocation) -> Result<()> {
    match loc {
        paths::StateLocation::Secure => Ok(()),
        other => crate::bail_code!(
            ErrorCode::UpdateStateFolder,
            "FanRGB runs on a temporary state folder ({other:?}), so it cannot hand over to the new version"
        ),
    }
}

/// What [`apply`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applied {
    /// Installed; FanRGB is restarting into the new version (the caller's process is exiting).
    Restarting { version: String },
    /// Verified, but this is not the installed copy (a development build): nothing was replaced.
    VerifiedOnly { version: String },
}

/// Verifies the staged update, records it as seen and, when this process is the elevated installed
/// copy, installs it and hands over: starts `fanrgb.exe.old --update-finish <pid>` and asks this
/// instance to exit ([`crate::system::request_exit`]; the engine returns the fans to the BIOS).
/// `expected` is the release the user was shown (version, SHA-256 of the executable): if the staged
/// files changed since (a later check staged another release), nothing is installed (FRG-E713).
/// Blocking (reads and hashes ~10 MB): call it off the UI thread.
pub fn apply(shared: &Arc<Shared>, staged: &Path, expected: Option<(&str, &str)>) -> Result<Applied> {
    let rec_path = state_file();
    let mut rec = load_record(&rec_path);
    let v = verify_staged(staged, &super::keys::TRUSTED, Version::current(), rec.seen(), super::now())?;
    check_expected(&v.release, expected)?;
    let to = v.release.manifest.version.clone();
    rec.last_published = rec.last_published.max(v.release.manifest.published_at);
    save_record(&rec_path, &rec)?;
    log::info!(
        "update {to} verified (signed by the {} key, sha256 {})",
        if v.key == 0 { "primary" } else { "backup" },
        v.release.manifest.asset.sha256
    );
    if !crate::util::win::is_elevated() || !crate::system::is_installed_copy() {
        log::info!("[{}] not replacing a development build with {to}", ErrorCode::UpdateDevBuild);
        return Ok(Applied::VerifiedOnly { version: to });
    }
    check_state_location(paths::state_location())?;
    let dir = paths::install_dir();
    match fsx::check_dir(&dir).code(ErrorCode::UpdateInstall)? {
        DirCheck::Trusted => {}
        other => crate::bail_code!(ErrorCode::UpdateInstall, "{} is not admin-only ({other:?})", dir.display()),
    }
    let old_sha = swap_in(&dir, &v.exe, &v.release.manifest.asset.sha256)?;
    let rollback = |why: String| -> anyhow::Error {
        let undone = swap_back(&dir);
        err(ErrorCode::UpdateInstall, format!("{why}; previous version restored: {}", undone.is_ok()))
    };
    if !matches!(fsx::check_file(&paths::installed_exe()), Ok(DirCheck::Trusted)) {
        return Err(rollback("the installed file is not admin-only".into()));
    }
    let from = Version::current().to_string();
    rec.pending = Some(Pending {
        to: to.clone(),
        from: from.clone(),
        at: super::now(),
        new_sha256: v.release.manifest.asset.sha256.clone(),
        old_sha256: old_sha,
        healthy: false,
        healthy_pid: 0,
        clean_exit_pid: 0,
    });
    // `last_installed` is recorded only once the new version proved healthy: a
    // version that never ran must stay installable.
    rec.rolled_back = None;
    if let Err(e) = save_record(&rec_path, &rec) {
        return Err(rollback(format!("{e:#}")));
    }
    let pid = std::process::id().to_string();
    if let Err(e) = crate::util::win::spawn_same_token(&with_suffix(&dir, OLD_SUFFIX), &[FINISH_ARG, &pid]) {
        rec.pending = None;
        let _ = save_record(&rec_path, &rec);
        return Err(rollback(format!("starting the update supervisor: {e:#}")));
    }
    log::info!("updating {from} → {to}: handing over (fans return to the BIOS during the switch)");
    crate::system::request_exit(shared);
    Ok(Applied::Restarting { version: to })
}

/// The staged release must still be the one the user chose to install.
pub(crate) fn check_expected(r: &Release, expected: Option<(&str, &str)>) -> Result<()> {
    let Some((version, sha256)) = expected else { return Ok(()) };
    let m = &r.manifest;
    if m.version != version || !m.asset.sha256.eq_ignore_ascii_case(sha256) {
        crate::bail_code!(
            ErrorCode::UpdateChanged,
            "the staged update is {} (sha256 {}), not the {version} that was shown",
            m.version,
            m.asset.sha256
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Health of the new version

static ENGINE_TICKS: AtomicU64 = AtomicU64::new(0);

/// Called on every engine tick (next to the watchdog heartbeat).
pub fn engine_tick() {
    ENGINE_TICKS.fetch_add(1, Ordering::Relaxed);
}

/// Tracks "the engine keeps ticking": healthy once it has ticked without a gap of more than `max_gap`
/// for [`HEALTHY_AFTER`].
#[derive(Debug)]
pub(crate) struct Probation {
    last_ticks: u64,
    last_change: Option<Instant>,
    steady_since: Option<Instant>,
}

impl Probation {
    const MAX_GAP: Duration = Duration::from_secs(15);

    pub fn new() -> Self {
        Probation { last_ticks: 0, last_change: None, steady_since: None }
    }

    pub fn sample(&mut self, ticks: u64, now: Instant) -> bool {
        if ticks != self.last_ticks {
            self.last_ticks = ticks;
            self.last_change = Some(now);
            self.steady_since.get_or_insert(now);
        } else if self.last_change.is_none_or(|t| now.duration_since(t) > Self::MAX_GAP) {
            self.steady_since = None;
        }
        self.steady_since.is_some_and(|t| now.duration_since(t) >= HEALTHY_AFTER)
    }
}

/// New instance, at start: if an update to this version is pending, report health to the supervisor
/// once the engine has been ticking steadily for [`HEALTHY_AFTER`].
pub fn start_probation(shared: Arc<Shared>) {
    let path = state_file();
    let rec = load_record(&path);
    let current = Version::current().to_string();
    let Some(p) = rec.pending else { return };
    if p.to != current || p.healthy || !crate::system::is_installed_copy() {
        return;
    }
    log::info!(
        "update {} → {current}: on probation until the engine has run for {} s",
        p.from,
        HEALTHY_AFTER.as_secs()
    );
    std::thread::Builder::new()
        .name("fanrgb-update-probation".into())
        .spawn(move || {
            let start = Instant::now();
            let mut probation = Probation::new();
            let mut sampled = Instant::now();
            while start.elapsed() < HEALTH_TIMEOUT {
                std::thread::sleep(Duration::from_millis(100));
                if shared.quit.load(Ordering::SeqCst) {
                    // Quitting before the verdict. Every exit path sets `quit` (tray Exit with a hidden
                    // window ends the process from `request_exit` without returning to `main`), and the
                    // failure paths set `UI_PANICKED` / `EXIT_FAILED` first.
                    if !crate::system::UI_PANICKED.load(Ordering::SeqCst) && !EXIT_FAILED.load(Ordering::SeqCst) {
                        mark_clean_exit();
                    }
                    return;
                }
                if sampled.elapsed() < Duration::from_secs(2) {
                    continue;
                }
                sampled = Instant::now();
                if !probation.sample(ENGINE_TICKS.load(Ordering::Relaxed), sampled) {
                    continue;
                }
                let mut rec = load_record(&path);
                if let Some(p) = rec.pending.as_mut().filter(|p| p.to == current) {
                    p.healthy = true;
                    p.healthy_pid = std::process::id();
                    // Proved healthy: from now on it counts as installed (downgrade protection).
                    rec.record_installed(&current);
                    match save_record(&path, &rec) {
                        Ok(()) => log::info!("update to {current} is healthy"),
                        Err(e) => log::error!("could not record update health: {e:#}"),
                    }
                }
                return;
            }
        })
        .ok();
}

/// New instance, on a clean exit (window closed, tray Exit, sign-out, another instance already running):
/// if this version is still on probation, tells the supervisor that its exit is not a failure.
/// Never called after a panic or a UI error. Cheap when no update is pending.
pub fn mark_clean_exit() {
    let path = state_file();
    let mut rec = load_record(&path);
    let current = Version::current().to_string();
    let Some(p) = rec.pending.as_mut() else { return };
    if p.to != current || p.healthy || p.clean_exit_pid != 0 || !crate::system::is_installed_copy() {
        return;
    }
    p.clean_exit_pid = std::process::id();
    match save_record(&path, &rec) {
        Ok(()) => log::info!("update to {current}: closed before the health check finished (not a failure)"),
        Err(e) => log::warn!("could not record the clean exit for the update supervisor: {e:#}"),
    }
}

/// Set (by `main`) before the process quits because of a UI error or panic: the probation watcher must
/// then not report a clean exit.
pub static EXIT_FAILED: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------------------
// `--update-finish <old pid>` (runs from fanrgb.exe.old, elevated)

/// What the supervisor concludes from one look at the new instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Running,
    Healthy,
    /// Closed cleanly before it reported healthy: keep the update, no rollback.
    ClosedCleanly,
    Failed(String),
}

/// `exited`: `None` while the child runs, `Some(Ok(code))` once it has exited, `Some(Err(_))` if it can
/// no longer be watched. Health recorded by that child wins even if it exited right after (the record
/// is read after the exit was seen). A clean exit needs exit code 0 and the child's own marker.
pub(crate) fn judge(
    rec: &UpdateRecord,
    p: &Pending,
    child_pid: u32,
    exited: Option<Result<u32, String>>,
    timed_out: bool,
) -> Verdict {
    let mine = rec.pending.as_ref().filter(|q| q.to == p.to);
    if mine.is_some_and(|q| q.healthy && q.healthy_pid == child_pid) {
        return Verdict::Healthy;
    }
    match exited {
        Some(Ok(0)) if mine.is_some_and(|q| q.clean_exit_pid == child_pid) => Verdict::ClosedCleanly,
        Some(Ok(code)) => Verdict::Failed(format!("{} exited with code {code:#x}", p.to)),
        Some(Err(e)) => Verdict::Failed(format!("cannot watch {}: {e}", p.to)),
        None if timed_out => {
            Verdict::Failed(format!("{} did not report healthy within {} s", p.to, HEALTH_TIMEOUT.as_secs()))
        }
        None => Verdict::Running,
    }
}

/// Supervisor, the update is kept: clear `pending`; a healthy version also counts as installed.
pub(crate) fn finish_kept(rec: &mut UpdateRecord, p: &Pending, healthy: bool) {
    rec.pending = None;
    if healthy {
        rec.record_installed(&p.to);
        rec.last_result = Some(format!("updated {} → {}", p.from, p.to));
    } else {
        rec.last_result = Some(format!("updated {} → {} (closed before the 60 s health check finished)", p.from, p.to));
    }
}

/// Waits for the old instance to exit, starts the new version and watches it. Returns the exit code.
pub fn run_finisher(old_pid: u32) -> i32 {
    let dir = paths::install_dir();
    let path = state_file();
    crate::system::wait_for_previous_instance(old_pid, Duration::from_secs(45));
    let Some(p) = load_record(&path).pending else {
        log::warn!("update supervisor: no update pending");
        return 0;
    };
    let new_exe = dir.join(paths::EXE_NAME);
    let old_pid_s = old_pid.to_string();
    let mut child =
        match crate::util::win::spawn_same_token_child(&new_exe, &[crate::system::RESTART_AFTER_ARG, &old_pid_s]) {
            Ok(c) => c,
            Err(e) => return roll_back(&dir, &path, &p, None, format!("could not start {}: {e:#}", p.to)),
        };
    log::info!(
        "update supervisor: started {} (pid {}), waiting up to {} s for it to report healthy",
        p.to,
        child.id(),
        HEALTH_TIMEOUT.as_secs()
    );
    let start = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let exited = match child.try_wait() {
            Ok(Some(code)) => Some(Ok(code)),
            Ok(None) => None,
            Err(e) => Some(Err(e.to_string())),
        };
        // Read after the exit check, so health or a clean exit recorded just before exiting counts.
        let mut rec = load_record(&path);
        let healthy = match judge(&rec, &p, child.id(), exited, start.elapsed() >= HEALTH_TIMEOUT) {
            Verdict::Running => continue,
            Verdict::Failed(why) => return roll_back(&dir, &path, &p, Some(&mut child), why),
            Verdict::Healthy => true,
            Verdict::ClosedCleanly => false,
        };
        finish_kept(&mut rec, &p, healthy);
        if let Err(e) = save_record(&path, &rec) {
            log::warn!("update supervisor: {e:#}");
        }
        if healthy {
            log::info!("update supervisor: {} is healthy; keeping {} as fanrgb.exe.old", p.to, p.from);
        } else {
            log::info!("update supervisor: {} was closed cleanly before its health check; keeping it", p.to);
        }
        return 0;
    }
}

fn roll_back(
    dir: &Path,
    path: &Path,
    p: &Pending,
    child: Option<&mut crate::util::win::UnelevatedChild>,
    why: String,
) -> i32 {
    log::error!("[{}] update to {} failed: {why}; going back to {}", ErrorCode::UpdateRolledBack, p.to, p.from);
    let child_pid = child.as_ref().map(|c| c.id());
    if let Some(c) = child {
        // Its watchdog hands the fans back to the BIOS when it dies.
        c.kill();
        std::thread::sleep(Duration::from_millis(1500));
    }
    let mut rec = load_record(path);
    rec.pending = None;
    rec.rolled_back = Some(p.to.clone());
    rec.last_result = Some(format!("update to {} rolled back: {why}", p.to));
    let _ = save_record(path, &rec);
    if let Err(e) = swap_back(dir) {
        log::error!("[{}] rollback failed: {e:#}", ErrorCode::UpdateInstall);
        return 2;
    }
    let after = child_pid.unwrap_or(std::process::id()).to_string();
    let exe = dir.join(paths::EXE_NAME);
    match crate::util::win::spawn_same_token(&exe, &[crate::system::RESTART_AFTER_ARG, &after]) {
        Ok(()) => log::warn!("rolled back to {}", p.from),
        Err(e) => log::error!("rolled back to {}, but restarting it failed: {e:#}", p.from),
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updater::manifest::tests::fake_exe;

    #[test]
    fn swap_keeps_one_old_copy_and_rolls_back() {
        let dir = fsx::tests::test_dir("update-swap");
        let v1 = fake_exe(1, 2048);
        let v2 = fake_exe(2, 3000);
        let v3 = fake_exe(3, 2500);
        std::fs::write(dir.join(paths::EXE_NAME), &v1).unwrap();
        let old_sha = swap_in(&dir, &v2, &sha256_hex(&v2)).unwrap();
        assert_eq!(old_sha, sha256_hex(&v1));
        assert_eq!(std::fs::read(dir.join(paths::EXE_NAME)).unwrap(), v2);
        assert_eq!(std::fs::read(with_suffix(&dir, OLD_SUFFIX)).unwrap(), v1);
        // A second update replaces the single .old.
        swap_in(&dir, &v3, &sha256_hex(&v3)).unwrap();
        assert_eq!(std::fs::read(with_suffix(&dir, OLD_SUFFIX)).unwrap(), v2);
        // Rollback: v2 is back, v3 kept aside as .failed.
        swap_back(&dir).unwrap();
        assert_eq!(std::fs::read(dir.join(paths::EXE_NAME)).unwrap(), v2);
        assert_eq!(std::fs::read(with_suffix(&dir, FAILED_SUFFIX)).unwrap(), v3);
        assert!(!with_suffix(&dir, OLD_SUFFIX).exists());
        assert!(swap_back(&dir).is_err(), "nothing left to roll back to");
        // Power loss between the two renames (no fanrgb.exe at all): rollback still restores.
        swap_in(&dir, &v3, &sha256_hex(&v3)).unwrap();
        std::fs::remove_file(dir.join(paths::EXE_NAME)).unwrap();
        swap_back(&dir).unwrap();
        assert_eq!(std::fs::read(dir.join(paths::EXE_NAME)).unwrap(), v2);
    }

    #[test]
    fn swap_refuses_bad_bytes_and_running_target_is_renamable() {
        let dir = fsx::tests::test_dir("update-swap-bad");
        let v1 = fake_exe(1, 2048);
        std::fs::write(dir.join(paths::EXE_NAME), &v1).unwrap();
        // Hash of other bytes: nothing renamed, no .new left.
        let e = swap_in(&dir, &fake_exe(2, 2048), &sha256_hex(&v1)).unwrap_err();
        assert_eq!(crate::error::code_of(&e), ErrorCode::UpdateHash);
        assert_eq!(std::fs::read(dir.join(paths::EXE_NAME)).unwrap(), v1);
        assert!(!with_suffix(&dir, NEW_SUFFIX).exists() && !with_suffix(&dir, OLD_SUFFIX).exists());
        // An open handle that allows rename/delete (like a mapped, running exe) does not block the swap.
        let hold = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().read(true).share_mode(0x7).open(dir.join(paths::EXE_NAME)).unwrap()
        };
        let v2 = fake_exe(2, 2048);
        swap_in(&dir, &v2, &sha256_hex(&v2)).unwrap();
        drop(hold);
        // No current executable: refuse (there is nothing to keep for a rollback).
        let empty = fsx::tests::test_dir("update-swap-empty");
        assert!(swap_in(&empty, &v2, &sha256_hex(&v2)).is_err());
    }

    #[test]
    fn probation_needs_a_steady_engine() {
        let t0 = Instant::now();
        let mut p = Probation::new();
        let mut ticks = 0;
        let mut t = t0;
        // Ticking every 2 s: healthy after 60 s.
        let mut healthy_at = None;
        for _ in 0..40 {
            ticks += 1;
            t += Duration::from_secs(2);
            if p.sample(ticks, t) && healthy_at.is_none() {
                healthy_at = Some(t.duration_since(t0));
            }
        }
        assert!(healthy_at.is_some_and(|d| d >= HEALTHY_AFTER && d <= HEALTHY_AFTER + Duration::from_secs(4)));
        // A 20 s stall resets the clock.
        let mut p = Probation::new();
        let mut t = t0;
        for i in 1..=10 {
            t += Duration::from_secs(2);
            assert!(!p.sample(i, t));
        }
        t += Duration::from_secs(20);
        assert!(!p.sample(10, t));
        t += Duration::from_secs(2);
        assert!(!p.sample(11, t), "a hung engine must not count as healthy");
    }

    /// A fake FanRGB executable that reports `version` (like the real `VERSION_MARKER`).
    fn fake_versioned_exe(fill: u8, version: &str) -> Vec<u8> {
        let mut b = fake_exe(fill, 4096);
        let m = format!("FANRGB_VERSION={version};");
        // A bare needle first (the real exe has the search string itself too).
        b[0x200..0x20f].copy_from_slice(b"FANRGB_VERSION=");
        b[0x400..0x400 + m.len()].copy_from_slice(m.as_bytes());
        b
    }

    #[test]
    fn exe_version_reads_the_marker() {
        assert_eq!(exe_version(&fake_versioned_exe(1, "0.2.1")), Some(Version(0, 2, 1)));
        assert_eq!(exe_version(&fake_exe(1, 4096)), None);
        assert_eq!(exe_version(b"xxFANRGB_VERSION=1.2;FANRGB_VERSION=1.2.3;"), Some(Version(1, 2, 3)));
        assert_eq!(exe_version(b"FANRGB_VERSION=0.2.0"), None, "no terminator");
        // The marker the app compiles in.
        assert_eq!(exe_version(super::super::VERSION_MARKER.as_bytes()), Some(Version::current()));
    }

    /// `--revert`: the checked `.old` goes back in place, the current
    /// version is kept as `.failed`; nothing to go back to, or a `.old` that is no FanRGB program, is
    /// refused without touching anything.
    #[test]
    fn revert_swaps_back_a_checked_previous_version() {
        let dir = fsx::tests::test_dir("update-revert");
        let (v1, v2) = (fake_versioned_exe(1, "0.2.0"), fake_versioned_exe(2, "0.2.1"));
        std::fs::write(dir.join(paths::EXE_NAME), &v1).unwrap();
        assert!(revert_plan(&dir).is_err(), "no .old yet");
        swap_in(&dir, &v2, &sha256_hex(&v2)).unwrap();
        let plan = revert_plan(&dir).unwrap();
        assert_eq!(
            plan,
            RevertPlan {
                current: Some(Version(0, 2, 1)),
                previous: Version(0, 2, 0),
                previous_sha256: sha256_hex(&v1)
            }
        );
        revert_files(&dir, &plan).unwrap();
        assert_eq!(std::fs::read(dir.join(paths::EXE_NAME)).unwrap(), v1);
        assert_eq!(std::fs::read(with_suffix(&dir, FAILED_SUFFIX)).unwrap(), v2);
        assert!(!with_suffix(&dir, OLD_SUFFIX).exists());
        assert!(revert_plan(&dir).is_err(), "only one step back");
        // A .old that is not a FanRGB build: refused, nothing renamed.
        std::fs::write(with_suffix(&dir, OLD_SUFFIX), fake_exe(9, 4096)).unwrap();
        assert!(revert_plan(&dir).is_err());
        std::fs::write(with_suffix(&dir, OLD_SUFFIX), b"not a program").unwrap();
        assert!(revert_plan(&dir).is_err());
        assert_eq!(std::fs::read(dir.join(paths::EXE_NAME)).unwrap(), v1);
        // The .old changed after it was checked: the swap is undone.
        let v3 = fake_versioned_exe(3, "0.1.9");
        std::fs::write(with_suffix(&dir, OLD_SUFFIX), &v3).unwrap();
        let mut stale = revert_plan(&dir).unwrap();
        stale.previous_sha256 = sha256_hex(&v2);
        let e = revert_files(&dir, &stale).unwrap_err();
        assert_eq!(crate::error::code_of(&e), ErrorCode::UpdateHash);
        assert_eq!(std::fs::read(dir.join(paths::EXE_NAME)).unwrap(), v1, "undone");
        assert_eq!(std::fs::read(with_suffix(&dir, OLD_SUFFIX)).unwrap(), v3);
    }

    /// Anti-downgrade after `--revert`: `last_installed` never goes down, the version left is never
    /// offered again (also when it was installed by hand, not by an update), a newer one is.
    #[test]
    fn revert_keeps_the_update_record_consistent() {
        let mut rec = UpdateRecord { last_installed: Some("0.2.1".into()), last_published: 50, ..Default::default() };
        rec.pending = Some(Pending { to: "0.2.1".into(), from: "0.2.0".into(), at: 1000, ..Default::default() });
        assert!(pending_in_progress(&rec, 1000 + HEALTH_TIMEOUT.as_secs()).is_some(), "supervisor may still decide");
        assert!(pending_in_progress(&rec, 1000 + HEALTH_TIMEOUT.as_secs() + 61).is_none(), "stale");
        record_revert(&mut rec, Some(Version(0, 2, 1)), Version(0, 2, 0));
        assert_eq!(rec.pending, None);
        assert_eq!(rec.last_installed.as_deref(), Some("0.2.1"), "never lowered");
        assert_eq!(rec.rolled_back.as_deref(), Some("0.2.1"));
        assert_eq!(rec.last_published, 50);
        let seen = rec.seen();
        let offer = |v: Version| seen.last_installed.is_none_or(|last| v > last);
        assert!(!offer(Version(0, 2, 1)) && offer(Version(0, 2, 2)));
        // 0.2.2 installed by hand (--install), then reverted: 0.2.2 is not offered again either.
        let mut rec = UpdateRecord { last_installed: Some("0.2.1".into()), ..Default::default() };
        record_revert(&mut rec, Some(Version(0, 2, 2)), Version(0, 2, 1));
        assert_eq!(rec.last_installed.as_deref(), Some("0.2.2"));
        // The current version unreadable: nothing recorded as installed.
        let mut rec = UpdateRecord::default();
        record_revert(&mut rec, None, Version(0, 2, 0));
        assert_eq!((rec.last_installed, rec.rolled_back), (None, None));
        assert!(rec.last_result.is_some_and(|r| r.contains("--revert")));
    }

    #[test]
    fn record_round_trip_and_seen() {
        let dir = fsx::tests::test_dir("update-record");
        let path = dir.join(STATE_FILE);
        assert_eq!(load_record(&path), UpdateRecord::default());
        let rec = UpdateRecord { last_installed: Some("0.2.0".into()), last_published: 77, ..Default::default() };
        save_record(&path, &rec).unwrap();
        assert_eq!(load_record(&path), rec);
        assert_eq!(rec.seen(), Seen { last_installed: Some(Version(0, 2, 0)), last_published: 77 });
        std::fs::write(&path, b"{ broken").unwrap();
        assert_eq!(load_record(&path), UpdateRecord::default());
    }
}
