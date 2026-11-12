//! Locations for configuration, logs and learned data.
//!
//! - **Elevated state** (control snapshot, AI models, logs, diagnostics, UI state): written by the
//!   elevated process, so it lives in `%ProgramData%\FanRGB` with an explicit admin-only DACL
//!   ([`super::sec::SDDL_STATE_DIR`]). A folder found there that a normal user could modify (or a
//!   junction) is moved aside and replaced on startup.
//! - **User preferences** (`config.json`): `%APPDATA%\FanRGB`. Treated as untrusted input: read with a
//!   size limit and written atomically with reparse points refused ([`super::fsx`]).
//! - Non-elevated runs (demo, `--no-elevate`, UAC cancelled) keep their logs in `%LOCALAPPDATA%\FanRGB`;
//!   nothing crosses a privilege boundary there.
//! - Folders come from `SHGetKnownFolderPath`, never from environment variables. `FANRGB_HOME` is only
//!   honoured by tests and by non-elevated debug builds (an elevated process inherits `HKCU\Environment`,
//!   which the user controls).

#![cfg_attr(test, allow(dead_code))]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Shell::{
    FOLDERID_LocalAppData, FOLDERID_ProgramData, FOLDERID_ProgramFiles, FOLDERID_RoamingAppData, SHGetKnownFolderPath,
};
use windows_sys::core::GUID;

use super::fsx;

pub const APP_DIR: &str = "FanRGB";
/// File name of the installed executable (`%ProgramFiles%\FanRGB\fanrgb.exe`).
pub const EXE_NAME: &str = "fanrgb.exe";
const MIGRATION_MARKER: &str = "migrated-v1";
const MAX_MIGRATE_BYTES: u64 = 16 << 20;

/// Where elevated state is kept for this process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateLocation {
    /// `%ProgramData%\FanRGB`, admin-only.
    Secure,
    /// `%ProgramData%\FanRGB.<pid>-<n>`, admin-only, used when the main folder could not be secured.
    Fallback,
    /// `%LOCALAPPDATA%\FanRGB` (process not elevated).
    User,
    /// `FANRGB_HOME` (tests / non-elevated debug builds).
    Override,
    /// No admin-only folder could be created: state files cannot be written at all.
    Unavailable,
}

struct State {
    dir: PathBuf,
    location: StateLocation,
}

static STATE: OnceLock<State> = OnceLock::new();
static NOTES: Mutex<Vec<(log::Level, String)>> = Mutex::new(Vec::new());

fn note(level: log::Level, msg: String) {
    if let Ok(mut n) = NOTES.lock() {
        n.push((level, msg));
    }
}

/// Messages produced while setting up the state folder (before the logger existed).
pub fn take_init_notes() -> Vec<(log::Level, String)> {
    NOTES.lock().map(|mut n| std::mem::take(&mut *n)).unwrap_or_default()
}

/// `SHGetKnownFolderPath` (reads the folder from the registry, not from the environment).
pub fn known_folder(id: &GUID) -> Option<PathBuf> {
    known_folder_for(id, std::ptr::null_mut())
}

/// [`known_folder`] of the user `token` belongs to (null: this process's user).
pub fn known_folder_for(id: &GUID, token: windows_sys::Win32::Foundation::HANDLE) -> Option<PathBuf> {
    // SAFETY: `id` is a valid GUID reference and `token` a valid token or null; on success `p` is a
    // NUL-terminated string allocated by the shell, read up to its terminator and freed exactly once
    // with CoTaskMemFree (also on failure, when the API may still allocate).
    unsafe {
        let mut p: *mut u16 = std::ptr::null_mut();
        let hr = SHGetKnownFolderPath(id, 0, token, &mut p);
        if hr < 0 || p.is_null() {
            if !p.is_null() {
                CoTaskMemFree(p as *const _);
            }
            return None;
        }
        let mut len = 0;
        while *p.add(len) != 0 {
            len += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
        CoTaskMemFree(p as *const _);
        (!s.is_empty()).then(|| PathBuf::from(s))
    }
}

pub fn program_data() -> PathBuf {
    known_folder(&FOLDERID_ProgramData).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
}

pub fn program_files() -> PathBuf {
    known_folder(&FOLDERID_ProgramFiles).unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
}

fn roaming_app_data() -> PathBuf {
    known_folder(&FOLDERID_RoamingAppData).unwrap_or_else(exe_dir)
}

fn local_app_data() -> PathBuf {
    known_folder(&FOLDERID_LocalAppData).unwrap_or_else(exe_dir)
}

fn exe_dir() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_else(|| ".".into())
}

/// Test builds: every file goes to `FANRGB_HOME` or a per-process folder next to the test binary
/// (under target/), so two runs of the same test binary never share a home.
#[cfg(test)]
pub fn test_home() -> PathBuf {
    std::env::var_os("FANRGB_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| exe_dir().join(format!("fanrgb-test-home-{}", std::process::id())))
}

/// `FANRGB_HOME` override: tests always, debug builds only when not elevated, release builds never.
fn home_override() -> Option<PathBuf> {
    #[cfg(test)]
    {
        Some(test_home())
    }
    #[cfg(all(debug_assertions, not(test)))]
    {
        if super::win::is_elevated() { None } else { std::env::var_os("FANRGB_HOME").map(PathBuf::from) }
    }
    #[cfg(all(not(debug_assertions), not(test)))]
    {
        None
    }
}

fn state() -> &'static State {
    STATE.get_or_init(init_state)
}

fn init_state() -> State {
    if let Some(home) = home_override() {
        let dir = home.join(APP_DIR);
        let _ = std::fs::create_dir_all(&dir);
        return State { dir, location: StateLocation::Override };
    }
    if !super::win::is_elevated() {
        let dir = local_app_data().join(APP_DIR);
        let _ = std::fs::create_dir_all(&dir);
        return State { dir, location: StateLocation::User };
    }
    let dir = program_data().join(APP_DIR);
    match fsx::ensure_secure_dir(&dir, super::sec::SDDL_STATE_DIR) {
        Ok(outcome) => {
            if let fsx::SecureDir::Replaced(old) = &outcome {
                note(
                    log::Level::Warn,
                    format!(
                        "{} was not admin-only (pre-created by another user or a link); moved it to {} and created a fresh one",
                        dir.display(),
                        old.display()
                    ),
                );
            }
            migrate_legacy(&dir);
            State { dir, location: StateLocation::Secure }
        }
        Err(e) => {
            note(log::Level::Error, format!("cannot secure {}: {e}", dir.display()));
            fallback_state()
        }
    }
}

/// Unpredictable 64-bit value (std seeds `RandomState` from the OS CSPRNG), so another user cannot
/// pre-create (squat) every fallback folder name.
fn random_suffix() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(super::win::uptime_ms());
    h.finish()
}

/// `FanRGB.<pid>-<16 lower-case hex digits>`.
fn fallback_name(pid: u32, suffix: u64) -> String {
    format!("{APP_DIR}.{pid}-{suffix:016x}")
}

/// True for the folder names a FanRGB process with `pid` may use for its state: `FanRGB`, or its own
/// fallback `FanRGB.<pid>-<16 hex>`. Quarantined `FanRGB.untrusted-*` folders never match.
pub(crate) fn is_state_dir_name(name: &str, pid: u32) -> bool {
    if name.eq_ignore_ascii_case(APP_DIR) {
        return true;
    }
    let Some(rest) = name.strip_prefix(&format!("{APP_DIR}.{pid}-")) else { return false };
    rest.len() == 16 && rest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn fallback_state() -> State {
    let base = program_data();
    for _ in 0..5 {
        let dir = base.join(fallback_name(std::process::id(), random_suffix()));
        if fsx::ensure_secure_dir(&dir, super::sec::SDDL_STATE_DIR).is_ok() {
            note(log::Level::Warn, format!("using temporary state folder {}", dir.display()));
            return State { dir, location: StateLocation::Fallback };
        }
    }
    note(
        log::Level::Error,
        format!(
            "[{}] no admin-only state folder available: logs, AI models and crash snapshots are disabled",
            crate::error::ErrorCode::StateFolder
        ),
    );
    // A device path that does not exist and that only the kernel could create: every write fails.
    State { dir: PathBuf::from(r"\\?\GLOBALROOT\Device\FanRGB-NoStateFolder"), location: StateLocation::Unavailable }
}

/// One-time copy of the learned AI models from the pre-ProgramData location. The old crash snapshot is
/// deliberately not migrated (it came from a user-writable folder) and old files are not deleted (an
/// elevated delete in a user-writable folder is itself an attack surface).
fn migrate_legacy(state: &Path) {
    let marker = state.join(MIGRATION_MARKER);
    if marker.exists() {
        return;
    }
    let legacy = local_app_data().join(APP_DIR);
    for name in ["ai_models.json"] {
        let (src, dst) = (legacy.join(name), state.join(name));
        if dst.exists() || !src.exists() {
            continue;
        }
        match fsx::read_limited(&src, MAX_MIGRATE_BYTES) {
            Ok(bytes) if serde_json::from_slice::<serde_json::Value>(&bytes).is_ok() => {
                match fsx::write_atomic(&dst, &bytes) {
                    Ok(()) => note(log::Level::Info, format!("migrated {} to {}", src.display(), dst.display())),
                    Err(e) => note(log::Level::Warn, format!("migrating {name} failed: {e}")),
                }
            }
            Ok(_) => note(log::Level::Warn, format!("not migrating {}: not valid JSON", src.display())),
            Err(e) => note(log::Level::Warn, format!("not migrating {}: {e}", src.display())),
        }
    }
    let _ = fsx::write_atomic(&marker, b"FanRGB state migrated from %LOCALAPPDATA%\\FanRGB\r\n");
}

/// Elevated, at run time: tries again to make `%ProgramData%\FanRGB` admin-only (moving a squatted
/// folder or link aside). The running process keeps its current state folder; a restart picks up the
/// repaired one. Returns the repaired folder.
pub fn repair_state_dir() -> std::io::Result<PathBuf> {
    let dir = program_data().join(APP_DIR);
    match fsx::ensure_secure_dir(&dir, super::sec::SDDL_STATE_DIR)? {
        fsx::SecureDir::Replaced(old) => {
            log::warn!("moved the blocked {} aside to {}", dir.display(), old.display())
        }
        other => log::info!("{}: {other:?}", dir.display()),
    }
    Ok(dir)
}

/// Watchdog child (`--state-dir`): adopt the parent's fallback state folder. Accepted only if it is an
/// admin-only `%ProgramData%\FanRGB.<parent pid>-<hex>` (or `FanRGB`) folder; quarantined
/// `FanRGB.untrusted-*` folders are refused. Must be called before any other path function.
pub fn use_state_dir(dir: &Path, parent_pid: u32) -> Result<(), String> {
    let parent_ok = dir.parent().is_some_and(|p| fsx::same_path(p, &program_data()));
    let name_ok = dir.file_name().is_some_and(|n| is_state_dir_name(&n.to_string_lossy(), parent_pid));
    if !parent_ok || !name_ok {
        return Err(format!("{} is not a FanRGB state folder", dir.display()));
    }
    match fsx::check_dir(dir) {
        Ok(fsx::DirCheck::Trusted) => {}
        Ok(other) => return Err(format!("{} is {other:?}", dir.display())),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    }
    STATE
        .set(State { dir: dir.to_path_buf(), location: StateLocation::Fallback })
        .map_err(|_| "state folder already chosen".to_string())
}

pub fn state_location() -> StateLocation {
    #[cfg(test)]
    {
        StateLocation::Override
    }
    #[cfg(not(test))]
    {
        state().location.clone()
    }
}

/// True if state files live in an admin-only folder (crash snapshots may only be trusted then).
pub fn state_dir_secure() -> bool {
    matches!(state_location(), StateLocation::Secure | StateLocation::Fallback)
}

/// Roaming user settings: `%APPDATA%\FanRGB`.
pub fn config_dir() -> PathBuf {
    let dir = match home_override() {
        Some(h) => h.join(APP_DIR),
        None => roaming_app_data().join(APP_DIR),
    };
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Elevated state (logs, AI models, control snapshot, diagnostics): see the module docs.
pub fn data_dir() -> PathBuf {
    #[cfg(test)]
    {
        let dir = test_home().join(APP_DIR);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }
    #[cfg(not(test))]
    {
        state().dir.clone()
    }
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.json")
}

pub fn ai_models_file() -> PathBuf {
    data_dir().join("ai_models.json")
}

/// Learned cooling-efficacy baseline (`control::efficacy`).
pub fn cooling_baseline_file() -> PathBuf {
    data_dir().join("cooling_baseline.json")
}

pub fn control_snapshot_file() -> PathBuf {
    data_dir().join("control_snapshot.json")
}

pub fn log_file() -> PathBuf {
    data_dir().join("fanrgb.log")
}

pub fn diag_file() -> PathBuf {
    data_dir().join("diagnostics.txt")
}

/// For eframe's `NativeOptions::persistence_path` (window/egui memory), so the elevated UI does not
/// read and write `%APPDATA%\FanRGB\data\app.ron`.
pub fn ui_persistence_file() -> PathBuf {
    data_dir().join("app.ron")
}

/// `%ProgramFiles%\FanRGB` (inherits the admin-only Program Files ACL).
pub fn install_dir() -> PathBuf {
    program_files().join(APP_DIR)
}

pub fn installed_exe() -> PathBuf {
    install_dir().join(EXE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tests_use_an_isolated_home() {
        // Engine tests point FANRGB_HOME at %TEMP%; otherwise the folder next to the test binary is used.
        // (Engine tests may set FANRGB_HOME concurrently; accept the value before or after.)
        let before = test_home();
        let data = data_dir();
        let cfg = config_file();
        let after = test_home();
        assert!(data.starts_with(&before) || data.starts_with(&after), "{} not isolated", data.display());
        assert!(cfg.starts_with(&before) || cfg.starts_with(&after), "{} not isolated", cfg.display());
        assert!(!data.starts_with(program_data()));
        assert_eq!(state_location(), StateLocation::Override);
        assert!(!state_dir_secure());
    }

    #[test]
    fn known_folders_resolve() {
        assert!(program_data().is_absolute());
        assert!(program_files().is_absolute());
        assert!(installed_exe().ends_with(r"FanRGB\fanrgb.exe"));
        assert!(installed_exe().starts_with(program_files()));
    }

    #[test]
    fn watchdog_state_dir_must_be_admin_only_programdata() {
        let d = super::super::fsx::tests::test_dir("statedir");
        assert!(use_state_dir(&d, 9).is_err());
        assert!(use_state_dir(&program_data().join("Other"), 9).is_err());
        assert!(use_state_dir(Path::new(r"C:\Users\Public\FanRGB"), 9).is_err());
        // Quarantined or foreign-pid folders are refused by name alone.
        assert!(use_state_dir(&program_data().join("FanRGB.untrusted-1700000000-9"), 9).is_err());
        assert!(use_state_dir(&program_data().join("FanRGB.8-0123456789abcdef"), 9).is_err());
    }

    #[test]
    fn state_dir_names() {
        let name = fallback_name(77, 0xdead_beef);
        assert!(is_state_dir_name(&name, 77));
        assert!(is_state_dir_name("FanRGB", 77));
        assert!(!is_state_dir_name(&name, 78));
        for bad in [
            "FanRGB.untrusted-1700000000-77",
            "FanRGB.77-0",
            "FanRGB.77-00000000DEADBEEF",
            "FanRGB.77-00000000deadbeef.x",
            "FanRGBx",
            "FanRGB.77-",
        ] {
            assert!(!is_state_dir_name(bad, 77), "{bad}");
        }
        // Fallback names are unpredictable.
        assert_ne!(random_suffix(), random_suffix());
    }
}
