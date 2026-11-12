//! OS integration: single instance, install + autostart, crash watchdog, panic hook, session events.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use windows_sys::Win32::Foundation::{
    DuplicateHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT,
    WAIT_TIMEOUT, WPARAM,
};
use windows_sys::Win32::Security::{
    CreateWellKnownSid, IsWellKnownSid, SECURITY_MAX_SID_SIZE, WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    DeleteFileW, MOVEFILE_DELAY_UNTIL_REBOOT, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    RemoveDirectoryW,
};
use windows_sys::Win32::System::Threading::{
    AddSIDToBoundaryDescriptor, CREATE_NO_WINDOW, ClosePrivateNamespace, CreateBoundaryDescriptorW, CreateEventW,
    CreateMutexW, CreatePrivateNamespaceW, DeleteBoundaryDescriptor, EVENT_MODIFY_STATE, GetCurrentProcess,
    GetProcessId, INFINITE, OpenEventW, OpenMutexW, OpenPrivateNamespaceW, OpenProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, SYNCHRONIZATION_SYNCHRONIZE, SetEvent,
    WaitForMultipleObjects, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::engine::Shared;
use crate::error::{ErrorCode, WithCode, err};
use crate::util::fsx::{self, DirCheck};
use crate::util::paths;
use crate::util::sec::{self, OwnedSd};
use crate::util::win::{OwnedHandle, wide};

pub const TASK_NAME: &str = "FanRGB";

/// Main window handle, set once the UI exists (used to show it from other threads).
pub static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);

// ---------------------------------------------------------------------------------------
// Named objects and single instance
//
// The elevated instance keeps its authoritative objects in a private namespace whose boundary
// requires the Administrators SID: a medium-integrity process can neither create (squat) nor open
// them, so it can't stop the app, fake "already running" or make it show its window. Two public
// marker objects (owned by Administrators, checked by owner) let a non-elevated launcher find a
// running elevated instance and bring it to the front without a UAC prompt. Non-elevated instances
// (demo / --no-elevate) use separate per-user names; nothing privileged depends on those.

const NS_BOUNDARY: &str = "FanRGB.Boundary";
const NS_ALIAS: &str = "FanRGB";
const PRIV_MUTEX: &str = r"FanRGB\SingleInstance";
const PRIV_SHOW: &str = r"FanRGB\ShowWindow";
const PRIV_EXIT: &str = r"FanRGB\Exit";
const PUB_MUTEX: &str = r"Local\FanRGB.SingleInstance";
const PUB_SHOW: &str = r"Local\FanRGB.ShowWindow";
const USER_MUTEX: &str = r"Local\FanRGB.User.SingleInstance";
const USER_SHOW: &str = r"Local\FanRGB.User.ShowWindow";
const USER_EXIT: &str = r"Local\FanRGB.User.Exit";
const DEMO_MUTEX: &str = r"Local\FanRGB.Demo.SingleInstance";
const DEMO_SHOW: &str = r"Local\FanRGB.Demo.ShowWindow";
const DEMO_EXIT: &str = r"Local\FanRGB.Demo.Exit";
const READ_CONTROL: u32 = 0x0002_0000;

/// Private namespace handle (closed on drop; the namespace lives while any handle is open).
struct Namespace(HANDLE);

// SAFETY: a private-namespace handle is a process-wide kernel handle, usable and closable from any thread.
unsafe impl Send for Namespace {}
// SAFETY: as above; `&Namespace` exposes nothing but drop.
unsafe impl Sync for Namespace {}

impl Drop for Namespace {
    fn drop(&mut self) {
        // SAFETY: `self.0` is an open namespace handle owned by this wrapper and closed exactly once.
        unsafe {
            ClosePrivateNamespace(self.0, 0);
        }
    }
}

fn admin_sid() -> Vec<u8> {
    let mut sid = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut len = SECURITY_MAX_SID_SIZE;
    // SAFETY: `sid` has SECURITY_MAX_SID_SIZE writable bytes and `len` says so; no domain SID is needed.
    unsafe { CreateWellKnownSid(WinBuiltinAdministratorsSid, null_mut(), sid.as_mut_ptr() as _, &mut len) };
    sid
}

/// Creates or opens FanRGB's private namespace. Fails unless the token has the Administrators SID enabled.
fn private_namespace(create: bool) -> io::Result<Namespace> {
    // SAFETY: `b`/`alias` are NUL-terminated; `bd` is the boundary descriptor created here, deleted exactly once below; `sid` is a valid SID buffer; `sa` points at the descriptor owned by `sd`, alive for the call.
    unsafe {
        let b = wide(NS_BOUNDARY);
        let mut bd = CreateBoundaryDescriptorW(b.as_ptr(), 0);
        if bd.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut sid = admin_sid();
        let res = (|| {
            if AddSIDToBoundaryDescriptor(&mut bd, sid.as_mut_ptr() as _) == 0 {
                return Err(io::Error::last_os_error());
            }
            let alias = wide(NS_ALIAS);
            let mut h = null_mut();
            if create {
                let sd = OwnedSd::from_sddl(sec::SDDL_PRIVATE_OBJECT)?;
                let sa = sd.attributes();
                h = CreatePrivateNamespaceW(&sa, bd as _, alias.as_ptr());
                if h.is_null() && GetLastError() != ERROR_ALREADY_EXISTS {
                    return Err(io::Error::last_os_error());
                }
            }
            if h.is_null() {
                h = OpenPrivateNamespaceW(bd as _, alias.as_ptr());
            }
            if h.is_null() { Err(io::Error::last_os_error()) } else { Ok(Namespace(h)) }
        })();
        DeleteBoundaryDescriptor(bd);
        res
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Mutex,
    Event,
}

/// Creates a named object with `sddl`. `Ok((handle, existed))`; an existing object that we may not
/// open (someone else's DACL) is an error.
fn create_object(kind: Kind, name: &str, sddl: Option<&str>) -> io::Result<(OwnedHandle, bool)> {
    let sd = sddl.map(OwnedSd::from_sddl).transpose()?;
    let sa = sd.as_ref().map(|s| s.attributes());
    let sa_ptr = sa.as_ref().map(|s| s as *const _).unwrap_or(null());
    let n = wide(name);
    // SAFETY: `n` is NUL-terminated; `sa_ptr` is null or points at `sa`, whose descriptor `sd` lives until the end of this function.
    let h = unsafe {
        match kind {
            Kind::Mutex => CreateMutexW(sa_ptr, 0, n.as_ptr()),
            Kind::Event => CreateEventW(sa_ptr, 0, 0, n.as_ptr()),
        }
    };
    // SAFETY: thread-local read, no preconditions.
    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    match OwnedHandle::new(h) {
        Some(h) => Ok((h, existed)),
        None => Err(io::Error::last_os_error()),
    }
}

fn open_object(kind: Kind, name: &str, access: u32) -> Option<OwnedHandle> {
    let n = wide(name);
    // SAFETY: `n` is NUL-terminated; the returned handle (or null) is owned by OwnedHandle.
    let h = unsafe {
        match kind {
            Kind::Mutex => OpenMutexW(access, 0, n.as_ptr()),
            Kind::Event => OpenEventW(access, 0, n.as_ptr()),
        }
    };
    OwnedHandle::new(h)
}

/// True if the kernel object is owned by Administrators or SYSTEM (i.e. created by an elevated process).
fn owned_by_admin(h: &OwnedHandle) -> bool {
    OwnedSd::of_kernel_object(h.raw()).is_ok_and(|sd| {
        let o = sd.owner();
        !o.is_null()
            // SAFETY: `o` is a non-null SID inside the descriptor `sd`, alive for the closure.
            && unsafe {
                IsWellKnownSid(o, WinBuiltinAdministratorsSid) != 0 || IsWellKnownSid(o, WinLocalSystemSid) != 0
            }
    })
}

pub struct InstanceGuard {
    _ns: Option<Namespace>,
    _markers: Vec<OwnedHandle>,
    /// Signalled when another launch asks this instance to show its window.
    pub show_events: Vec<OwnedHandle>,
    /// Signalled by `fanrgb --exit`.
    pub exit_event: Option<OwnedHandle>,
}

/// What a launcher finds before deciding whether to start (or elevate).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstanceProbe {
    NotRunning,
    /// A genuine elevated instance (marker owned by Administrators).
    Elevated,
    /// A non-elevated instance of this user (demo / --no-elevate).
    User,
    /// The public marker exists but was not created by an elevated process (squatted).
    Foreign,
}

/// Looks for a running instance without creating anything.
pub fn probe_instance() -> InstanceProbe {
    match open_object(Kind::Mutex, PUB_MUTEX, SYNCHRONIZATION_SYNCHRONIZE | READ_CONTROL) {
        Some(h) if owned_by_admin(&h) => return InstanceProbe::Elevated,
        Some(_) => return InstanceProbe::Foreign,
        // SAFETY: thread-local read, no preconditions.
        None if unsafe { GetLastError() } == ERROR_ACCESS_DENIED => return InstanceProbe::Foreign,
        None => {}
    }
    if open_object(Kind::Mutex, USER_MUTEX, SYNCHRONIZATION_SYNCHRONIZE).is_some() {
        return InstanceProbe::User;
    }
    InstanceProbe::NotRunning
}

/// Asks the running instance found by [`probe_instance`] to show its window.
pub fn signal_show(probe: InstanceProbe) -> bool {
    let (name, need_admin) = match probe {
        InstanceProbe::Elevated => (PUB_SHOW, true),
        InstanceProbe::User => (USER_SHOW, false),
        _ => return false,
    };
    match open_object(Kind::Event, name, EVENT_MODIFY_STATE | READ_CONTROL) {
        Some(ev) if !need_admin || owned_by_admin(&ev) => {
            allow_foreground();
            // SAFETY: `ev` is an open event handle with EVENT_MODIFY_STATE.
            unsafe { SetEvent(ev.raw()) != 0 }
        }
        _ => false,
    }
}

/// Claims the single-instance objects. Errors with `AlreadyRunning` (after asking the running instance
/// to show itself) or `InstanceConflict` (an object exists that isn't ours). Never blocks.
pub fn acquire_instance(elevated: bool) -> Result<InstanceGuard> {
    if elevated {
        match private_namespace(true) {
            Ok(ns) => return acquire_elevated(ns),
            Err(e) => log::warn!("private namespace unavailable ({e}); using per-user instance objects"),
        }
    }
    acquire_user()
}

fn acquire_elevated(ns: Namespace) -> Result<InstanceGuard> {
    let (mutex, existed) =
        create_object(Kind::Mutex, PRIV_MUTEX, Some(sec::SDDL_PRIVATE_OBJECT)).code(ErrorCode::InstanceConflict)?;
    if existed {
        if let Some(ev) = open_object(Kind::Event, PRIV_SHOW, EVENT_MODIFY_STATE) {
            allow_foreground();
            // SAFETY: `ev` is an open event handle with EVENT_MODIFY_STATE.
            unsafe { SetEvent(ev.raw()) };
        }
        return Err(err(ErrorCode::AlreadyRunning, "an elevated instance is already running"));
    }
    // We just created the mutex, so nobody legitimate holds these events: one that already exists was
    // made by someone else inside the namespace (defence in depth).
    let (show, show_existed) =
        create_object(Kind::Event, PRIV_SHOW, Some(sec::SDDL_PRIVATE_OBJECT)).code(ErrorCode::InstanceConflict)?;
    let (exit, exit_existed) =
        create_object(Kind::Event, PRIV_EXIT, Some(sec::SDDL_PRIVATE_OBJECT)).code(ErrorCode::InstanceConflict)?;
    if show_existed || exit_existed {
        return Err(err(ErrorCode::InstanceConflict, "FanRGB's private events already existed"));
    }
    let mut markers = vec![mutex];
    let mut show_events = vec![show];

    // Public markers for unelevated launchers. If someone squatted them, carry on without (the
    // private objects above are authoritative); launchers will then elevate and land here.
    match create_object(Kind::Mutex, PUB_MUTEX, Some(sec::SDDL_PUBLIC_MUTEX)) {
        Ok((h, false)) => markers.push(h),
        Ok((h, true)) if owned_by_admin(&h) => markers.push(h),
        Ok(_) | Err(_) => log::warn!(
            "[{}] {PUB_MUTEX} was created by another program; second launches will ask for elevation",
            ErrorCode::InstanceConflict
        ),
    }
    match create_object(Kind::Event, PUB_SHOW, Some(sec::SDDL_PUBLIC_EVENT)) {
        Ok((h, false)) => show_events.push(h),
        Ok((h, true)) if owned_by_admin(&h) => show_events.push(h),
        Ok(_) | Err(_) => log::warn!("[{}] {PUB_SHOW} was created by another program", ErrorCode::InstanceConflict),
    }
    Ok(InstanceGuard { _ns: Some(ns), _markers: markers, show_events, exit_event: Some(exit) })
}

fn acquire_user() -> Result<InstanceGuard> {
    acquire_named(USER_MUTEX, USER_SHOW, USER_EXIT)
}

/// `--demo`: separate per-user objects, so a demo neither blocks nor is blocked by the real app.
pub fn acquire_demo_instance() -> Result<InstanceGuard> {
    acquire_named(DEMO_MUTEX, DEMO_SHOW, DEMO_EXIT)
}

fn acquire_named(mutex_name: &str, show_name: &str, exit_name: &str) -> Result<InstanceGuard> {
    let (mutex, existed) = create_object(Kind::Mutex, mutex_name, None).code(ErrorCode::InstanceConflict)?;
    if existed {
        if let Some(ev) = open_object(Kind::Event, show_name, EVENT_MODIFY_STATE) {
            allow_foreground();
            // SAFETY: `ev` is an open event handle with EVENT_MODIFY_STATE.
            unsafe { SetEvent(ev.raw()) };
        }
        return Err(err(ErrorCode::AlreadyRunning, "an instance is already running"));
    }
    let (show, _) = create_object(Kind::Event, show_name, None).code(ErrorCode::InstanceConflict)?;
    let (exit, _) = create_object(Kind::Event, exit_name, None).code(ErrorCode::InstanceConflict)?;
    Ok(InstanceGuard { _ns: None, _markers: vec![mutex], show_events: vec![show], exit_event: Some(exit) })
}

/// Outcome of `fanrgb --exit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitOutcome {
    NotRunning,
    Stopped,
    TimedOut,
    /// The running instance is elevated; only an elevated `--exit` can reach it.
    NeedsElevation,
}

/// A running FanRGB started from a different executable than this one (an older build, a copy in
/// another folder). Bringing its window to the front would hide that this copy never started, so the
/// launcher offers to replace it instead. Helper processes of this copy share our path and don't count.
pub fn other_copy_path() -> Option<PathBuf> {
    let me = std::env::current_exe().ok()?;
    let own_name = me.file_name()?.to_str()?.to_lowercase();
    crate::util::win::other_process_paths(&["fanrgb.exe", &own_name]).into_iter().find(|p| !fsx::same_path(p, &me))
}

/// Asks a running instance to exit cleanly (`--exit`) and waits until it is gone.
pub fn request_instance_exit(timeout: Duration) -> ExitOutcome {
    // Elevated instance: reachable only through the private namespace.
    if let Ok(ns) = private_namespace(false)
        && let Some(ev) = open_object(Kind::Event, PRIV_EXIT, EVENT_MODIFY_STATE)
    {
        // SAFETY: `ev` is an open event handle with EVENT_MODIFY_STATE.
        unsafe { SetEvent(ev.raw()) };
        let gone = wait_gone(PRIV_MUTEX, timeout);
        drop(ns);
        return if gone { ExitOutcome::Stopped } else { ExitOutcome::TimedOut };
    }
    match probe_instance() {
        InstanceProbe::Elevated => ExitOutcome::NeedsElevation,
        InstanceProbe::User => {
            if let Some(ev) = open_object(Kind::Event, USER_EXIT, EVENT_MODIFY_STATE) {
                // SAFETY: `ev` is an open event handle with EVENT_MODIFY_STATE.
                unsafe { SetEvent(ev.raw()) };
            }
            if wait_gone(USER_MUTEX, timeout) { ExitOutcome::Stopped } else { ExitOutcome::TimedOut }
        }
        _ => ExitOutcome::NotRunning,
    }
}

/// Asks a running demo instance (`--demo`, its own instance objects) to exit and waits until it is
/// gone. A demo never controls hardware, so this is only a convenience for `--exit`.
pub fn request_demo_exit(timeout: Duration) -> ExitOutcome {
    match open_object(Kind::Event, DEMO_EXIT, EVENT_MODIFY_STATE) {
        Some(ev) => {
            // SAFETY: `ev` is an open event handle with EVENT_MODIFY_STATE.
            unsafe { SetEvent(ev.raw()) };
            if wait_gone(DEMO_MUTEX, timeout) { ExitOutcome::Stopped } else { ExitOutcome::TimedOut }
        }
        None => ExitOutcome::NotRunning,
    }
}

fn wait_gone(mutex: &str, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if open_object(Kind::Mutex, mutex, SYNCHRONIZATION_SYNCHRONIZE).is_none() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// Waits for `--exit` requests from another process.
pub fn spawn_exit_listener(event: OwnedHandle, shared: Arc<Shared>) {
    std::thread::Builder::new()
        .name("fanrgb-exit".into())
        // SAFETY: | unsafe {|`event` is an open event handle owned by this closure for the whole wait.
        .spawn(move || unsafe {
            if WaitForSingleObject(event.raw(), INFINITE) == 0 {
                log::info!("exit requested by another process");
                request_exit(&shared);
            }
        })
        .ok();
}

/// Waits for "show yourself" requests from a second launch.
pub fn spawn_show_listener(events: Vec<OwnedHandle>, shared: Arc<Shared>) {
    if events.is_empty() {
        return;
    }
    std::thread::Builder::new()
        .name("fanrgb-show".into())
        .spawn(move || {
            let handles: Vec<HANDLE> = events.iter().map(|e| e.raw()).collect();
            loop {
                // SAFETY: `handles` holds 1-2 open event handles kept alive by `events` for the loop.
                let r = unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, INFINITE) };
                if r as usize >= handles.len() {
                    return;
                }
                show_main_window();
                shared.request_repaint();
            }
        })
        .ok();
}

/// Brings the main window back from the tray / minimized state (the UI thread does the work).
pub fn show_main_window() {
    crate::ui::request_show();
}

/// Asks the whole app to exit (tray "Exit"): stops the engine, closes the window and,
/// as a last resort, exits the process once the engine had time to restore the BIOS.
/// Tells an update supervisor that this exit is not a failure, unless the UI failed.
fn clean_exit_for_updater() {
    if !UI_PANICKED.load(Ordering::SeqCst) && !crate::updater::install::EXIT_FAILED.load(Ordering::SeqCst) {
        crate::updater::install::mark_clean_exit();
    }
}

pub fn request_exit(shared: &Arc<Shared>) {
    shared.quit.store(true, Ordering::SeqCst);
    shared.send(crate::engine::Command::Wake);
    let hwnd = MAIN_HWND.load(Ordering::SeqCst) as HWND;
    if !hwnd.is_null() {
        // SAFETY: `hwnd` is our main window or a stale handle (rejected by the call); no memory is passed.
        unsafe {
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        }
    }
    shared.force_repaint();
    // A hidden egui window never processes the close request, so exit as soon as the
    // engine (fans back to BIOS) and the lighting thread have finished their shutdown.
    let shared = shared.clone();
    std::thread::spawn(move || {
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if shared.engine_done.load(Ordering::SeqCst) && shared.rgb_done.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(300));
        // A user exit is not a failed update: record it before the process
        // ends here without returning to `main` (no-op unless this version is on probation).
        clean_exit_for_updater();
        if shared.engine_done.load(Ordering::SeqCst) {
            log::info!("bye");
        } else {
            log::warn!("engine did not stop in time; exiting anyway");
        }
        log::logger().flush();
        std::process::exit(0);
    });
}

pub fn main_window_visible() -> bool {
    let hwnd = MAIN_HWND.load(Ordering::SeqCst) as HWND;
    // Minimized (also "closed to tray", which minimizes the window) counts as not visible.
    // SAFETY: `hwnd` is our main window or a stale handle (both calls then return 0).
    !hwnd.is_null() && unsafe { IsWindowVisible(hwnd) != 0 && IsIconic(hwnd) == 0 }
}

/// Lets the running instance take the foreground when a second launch asks it to show itself (Windows
/// otherwise only flashes its taskbar button).
fn allow_foreground() {
    // SAFETY: plain call; ASFW_ANY (all bits set) allows any process, failure is harmless.
    unsafe { AllowSetForegroundWindow(u32::MAX) };
}

// ---------------------------------------------------------------------------------------
// Install location
//
// Autostart runs FanRGB with administrator rights at every logon without a prompt, so it may only
// start an executable that normal programs cannot replace: `%ProgramFiles%\FanRGB\fanrgb.exe`
// (Program Files' inherited ACL is admin/TrustedInstaller-only; checked, not assumed).

static PINNED_EXE: OnceLock<std::fs::File> = OnceLock::new();

/// Opens our own executable denying write/delete/rename sharing for the process lifetime, so the file
/// that `--install` copies (and that a restart starts) is the one that is running, not one swapped in at
/// the same path later. Called as the very first statement of `main`; a swap *before* that
/// is caught by [`read_own_exe`], which compares the pinned file with the loaded image.
/// Skipped for executables in `%ProgramFiles%\FanRGB` (protected by its ACL, and an update must be able
/// to rename them while they run).
pub fn pin_own_exe() {
    use std::os::windows::fs::OpenOptionsExt;
    let Ok(exe) = std::env::current_exe() else { return };
    // Also `fanrgb.exe.old`, which runs the update supervisor and must be renamable for a rollback.
    if fsx::same_path(&exe, &paths::installed_exe())
        || exe.parent().is_some_and(|d| fsx::same_path(d, &paths::install_dir()))
    {
        return;
    }
    if let Ok(f) = std::fs::OpenOptions::new().read(true).share_mode(0x1 /* FILE_SHARE_READ */).open(exe) {
        let _ = PINNED_EXE.set(f);
    }
}

/// True if this process runs `%ProgramFiles%\FanRGB\fanrgb.exe` and that location is admin-only.
pub fn is_installed_copy() -> bool {
    std::env::current_exe().is_ok_and(|e| fsx::same_path(&e, &paths::installed_exe())) && install_location_trusted()
}

fn install_location_trusted() -> bool {
    matches!(fsx::check_dir(&paths::install_dir()), Ok(DirCheck::Trusted))
        && matches!(fsx::check_file(&paths::installed_exe()), Ok(DirCheck::Trusted))
}

/// Reads the running executable from the pinned handle and checks that it is byte-for-byte the program
/// that is running (every read-only section, see [`image_matches_file`]).
fn read_own_exe() -> Result<Vec<u8>> {
    use std::io::{Read, Seek};
    let mut f = match PINNED_EXE.get() {
        Some(f) => f,
        None => return Err(err(ErrorCode::InstallFailed, "the running executable could not be locked")),
    };
    f.seek(io::SeekFrom::Start(0)).code(ErrorCode::InstallFailed)?;
    let mut bytes = Vec::new();
    f.take(512 << 20).read_to_end(&mut bytes).code(ErrorCode::InstallFailed)?;
    // SAFETY: GetModuleHandleW(null) is the base of our own mapped image, valid for the process
    // lifetime; `image_matches_file` reads only inside the image's read-only sections.
    let base = unsafe { windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(null()) } as *const u8;
    // SAFETY: as above.
    match unsafe { image_matches_file(base, &bytes) } {
        Ok(()) => Ok(bytes),
        Err(why) => Err(err(
            ErrorCode::InstallFailed,
            format!("the executable on disk does not match the running program ({why})"),
        )),
    }
}

/// Minimal PE32+ layout needed to compare a file with its mapped image.
struct PeLayout {
    image_base: u64,
    size_of_image: u32,
    /// File offset of the optional header's `ImageBase` field (the one header field the loader rewrites).
    image_base_at: usize,
    /// `SizeOfHeaders`: DOS header, PE headers and section table, as mapped at the image base.
    size_of_headers: u32,
    /// End of the section table.
    headers_end: usize,
    /// (virtual address, virtual size, raw offset, raw size, characteristics)
    sections: Vec<(u32, u32, u32, u32, u32)>,
    /// Data directories (rva, size).
    dirs: Vec<(u32, u32)>,
}

fn rd16(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
fn rd32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
fn rd64(b: &[u8], o: usize) -> Option<u64> {
    b.get(o..o + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap_or([0; 8])))
}

fn pe_layout(b: &[u8]) -> Option<PeLayout> {
    if b.get(0..2)? != b"MZ" {
        return None;
    }
    let pe = rd32(b, 0x3c)? as usize;
    if pe > 4096 || b.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let nsec = rd16(b, pe + 6)? as usize;
    let opt_size = rd16(b, pe + 20)? as usize;
    let oh = pe + 24;
    if rd16(b, oh)? != 0x20b || nsec > 96 {
        return None; // PE32+ only (x64 build)
    }
    let ndirs = (rd32(b, oh + 108)? as usize).min(16);
    let dirs =
        (0..ndirs).map(|i| Some((rd32(b, oh + 112 + i * 8)?, rd32(b, oh + 116 + i * 8)?))).collect::<Option<_>>()?;
    let st = oh + opt_size;
    let sections = (0..nsec)
        .map(|i| {
            let s = st + i * 40;
            Some((rd32(b, s + 12)?, rd32(b, s + 8)?, rd32(b, s + 20)?, rd32(b, s + 16)?, rd32(b, s + 36)?))
        })
        .collect::<Option<_>>()?;
    Some(PeLayout {
        image_base: rd64(b, oh + 24)?,
        size_of_image: rd32(b, oh + 56)?,
        image_base_at: oh + 24,
        size_of_headers: rd32(b, oh + 60)?,
        headers_end: st + nsec * 40,
        sections,
        dirs,
    })
}

impl PeLayout {
    fn rva_to_offset(&self, rva: u32) -> Option<usize> {
        self.sections.iter().find_map(|&(va, vs, ptr, raw, _)| {
            let (rva, va, len) = (rva as u64, va as u64, vs.min(raw).max(1) as u64);
            (rva >= va && rva < va + len).then(|| (ptr as u64 + (rva - va)) as usize)
        })
    }
}

const SCN_MEM_WRITE: u32 = 0x8000_0000;
const SCN_MEM_DISCARDABLE: u32 = 0x0200_0000;
const SCN_CNT_UNINITIALIZED: u32 = 0x0000_0080;

/// Compares every read-only section of the PE file `file` with the image mapped at `base`, after
/// applying the file's base relocations for the actual load address, and the file's headers with the
/// image's (everything but `ImageBase`). The import address table (written by the loader) is masked.
/// Writable and discardable sections are skipped: their initial contents cannot be verified this way.
///
/// # Safety
/// `base` must be the base address of a PE image mapped by the loader whose headers describe the same
/// layout as `file` (checked: section table and SizeOfImage must match); every read-only section range
/// of that image must be readable.
pub(crate) unsafe fn image_matches_file(base: *const u8, file: &[u8]) -> std::result::Result<(), String> {
    let f = pe_layout(file).ok_or("not a PE32+ file")?;
    // SAFETY: the mapped image's headers (first page) are always readable.
    let head = unsafe { std::slice::from_raw_parts(base, 4096) };
    let m = pe_layout(head).ok_or("loaded image has no PE header")?;
    if m.size_of_image != f.size_of_image || m.sections.len() != f.sections.len() {
        return Err("different image layout".into());
    }
    if m.sections.iter().zip(&f.sections).any(|(a, b)| a.0 != b.0 || a.1 != b.1 || a.4 != b.4) {
        return Err("different section table".into());
    }
    // The headers decide what the loader does with the image (entry point; import, TLS and load-config
    // directories; raw section sizes) and what this function skips (the IAT range, the bytes past a
    // section's raw size). In a swapped file they are attacker-chosen, so they must be the loaded
    // image's own headers byte for byte. Only `ImageBase` may differ: the loader stores the actual
    // load address there.
    let hdr = f.size_of_headers as usize;
    if hdr < f.headers_end || hdr > head.len() || hdr > file.len() {
        return Err("unsupported header size".into());
    }
    let base_field = f.image_base_at..f.image_base_at + 8;
    if file[..hdr].iter().zip(&head[..hdr]).enumerate().any(|(i, (a, b))| a != b && !base_field.contains(&i)) {
        return Err("different headers".into());
    }
    let delta = (base as u64).wrapping_sub(f.image_base);
    // Relocated copy of the file's section bytes, indexed by RVA.
    let mut img = vec![0u8; f.size_of_image as usize];
    for &(va, vs, ptr, raw, _) in &f.sections {
        let n = vs.min(raw) as usize;
        let src = file.get(ptr as usize..ptr as usize + n).ok_or("truncated section")?;
        img.get_mut(va as usize..va as usize + n).ok_or("section outside image")?.copy_from_slice(src);
    }
    let (rel_rva, rel_size) = f.dirs.get(5).copied().unwrap_or((0, 0));
    if rel_size > 0 && delta != 0 {
        let start = f.rva_to_offset(rel_rva).ok_or("relocations outside sections")?;
        let rel = file.get(start..start + rel_size as usize).ok_or("truncated relocations")?;
        let mut o = 0usize;
        while o + 8 <= rel.len() {
            let page = rd32(rel, o).unwrap_or(0) as usize;
            let size = rd32(rel, o + 4).unwrap_or(0) as usize;
            if size < 8 || o + size > rel.len() {
                return Err("malformed relocation block".into());
            }
            for e in (o + 8..o + size).step_by(2) {
                let entry = rd16(rel, e).unwrap_or(0);
                let at = page + (entry & 0xfff) as usize;
                match entry >> 12 {
                    0 => {}
                    10 => {
                        let v = rd64(&img, at).ok_or("relocation outside image")?.wrapping_add(delta);
                        img[at..at + 8].copy_from_slice(&v.to_le_bytes());
                    }
                    3 => {
                        let v = rd32(&img, at).ok_or("relocation outside image")?.wrapping_add(delta as u32);
                        img[at..at + 4].copy_from_slice(&v.to_le_bytes());
                    }
                    t => return Err(format!("relocation type {t}")),
                }
            }
            o += size;
        }
    }
    // The import address table is written by the loader. Its range comes from the headers verified
    // above and must lie inside one read-only section (never "the whole image").
    let (iat, iat_size) = f.dirs.get(12).copied().unwrap_or((0, 0));
    let iat = iat as u64..iat as u64 + iat_size as u64;
    let iat_ok = iat.is_empty()
        || f.sections.iter().any(|&(va, vs, _, raw, ch)| {
            ch & (SCN_MEM_WRITE | SCN_MEM_DISCARDABLE | SCN_CNT_UNINITIALIZED) == 0
                && iat.start >= va as u64
                && iat.end <= va as u64 + vs.min(raw) as u64
        });
    if !iat_ok {
        return Err("import address table outside the read-only sections".into());
    }
    let mut compared = 0usize;
    for &(va, vs, _, raw, ch) in &f.sections {
        if ch & (SCN_MEM_WRITE | SCN_MEM_DISCARDABLE | SCN_CNT_UNINITIALIZED) != 0 {
            continue;
        }
        let n = vs.min(raw) as usize;
        // SAFETY: [va, va+n) lies inside SizeOfImage (checked when building `img`) and inside a mapped,
        // non-writable (hence readable: code or read-only data) section of the loaded image.
        let mem = unsafe { std::slice::from_raw_parts(base.add(va as usize), n) };
        let want = &img[va as usize..va as usize + n];
        for (i, (a, b)) in mem.iter().zip(want).enumerate() {
            let rva = va as usize + i;
            if a != b && !iat.contains(&(rva as u64)) {
                return Err(format!("section at {va:#x} differs at {rva:#x}"));
            }
        }
        compared += n;
    }
    if compared == 0 { Err("no read-only sections".into()) } else { Ok(()) }
}

/// SHA-256 of `bytes`.
fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
}

fn require_elevated(code: ErrorCode) -> Result<()> {
    if crate::util::win::is_elevated() { Ok(()) } else { Err(err(code, "administrator rights required")) }
}

/// Copies the running executable to `%ProgramFiles%\FanRGB\fanrgb.exe` (replacing an older copy,
/// even a running one) and returns that path. Elevated only. The bytes come from the pinned handle of the
/// running program and are verified against the loaded image; the installed file is read back and must
/// have the same SHA-256.
pub fn install_self() -> Result<PathBuf> {
    require_elevated(ErrorCode::InstallFailed)?;
    let dir = paths::install_dir();
    let target = paths::installed_exe();
    if is_installed_copy() {
        return Ok(target);
    }
    match fsx::check_dir(&dir).code(ErrorCode::InstallFailed)? {
        DirCheck::Missing => {
            // Default security: inherits the admin-only Program Files ACL.
            std::fs::create_dir(&dir).code(ErrorCode::InstallFailed)?;
            match fsx::check_dir(&dir).code(ErrorCode::InstallFailed)? {
                DirCheck::Trusted => {}
                other => crate::bail_code!(ErrorCode::InstallFailed, "{} is not admin-only ({other:?})", dir.display()),
            }
        }
        DirCheck::Trusted => {}
        other => crate::bail_code!(ErrorCode::InstallFailed, "{} is not admin-only ({other:?})", dir.display()),
    }
    let bytes = read_own_exe()?;
    let digest = sha256(&bytes);
    // Holds the folder (no rename/replace) until the copy is in place.
    let _dir = fsx::open_dir_checked(&dir).code(ErrorCode::InstallFailed)?;
    let tmp = dir.join(format!("{}.{}.new", paths::EXE_NAME, std::process::id()));
    let res = (|| -> Result<()> {
        use std::io::Write;
        use std::os::windows::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(0x0020_0000) // FILE_FLAG_OPEN_REPARSE_POINT
            .open(&tmp)
            .code(ErrorCode::InstallFailed)?;
        f.write_all(&bytes).code(ErrorCode::InstallFailed)?;
        f.sync_all().code(ErrorCode::InstallFailed)?;
        drop(f);
        if move_file(&tmp, Some(&target), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH).is_err() {
            // The installed copy is running: a running exe can be renamed but not overwritten.
            let old = dir.join(format!("{}.old-{}", paths::EXE_NAME, std::process::id()));
            move_file(&target, Some(&old), 0).code(ErrorCode::InstallFailed)?;
            move_file(&tmp, Some(&target), MOVEFILE_WRITE_THROUGH).code(ErrorCode::InstallFailed)?;
            let _ = move_file(&old, None, MOVEFILE_DELAY_UNTIL_REBOOT);
        }
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res?;
    remove_stale_copies(&dir);
    let copied = fsx::read_limited(&target, 512 << 20).code(ErrorCode::InstallFailed)?;
    if sha256(&copied) != digest {
        let _ = std::fs::remove_file(&target);
        crate::bail_code!(
            ErrorCode::InstallFailed,
            "{} does not match the running program (SHA-256)",
            target.display()
        );
    }
    match fsx::check_file(&target).code(ErrorCode::InstallFailed)? {
        DirCheck::Trusted => {
            log::info!("installed FanRGB to {}", target.display());
            Ok(target)
        }
        other => Err(err(ErrorCode::InstallFailed, format!("{} is not admin-only ({other:?})", target.display()))),
    }
}

fn move_file(from: &Path, to: Option<&Path>, flags: u32) -> io::Result<()> {
    let f = fsx::wide_path(from);
    let t = to.map(fsx::wide_path);
    // SAFETY: `f` and `t` (if any) are NUL-terminated wide paths alive for the call.
    let ok = unsafe { MoveFileExW(f.as_ptr(), t.as_ref().map(|t| t.as_ptr()).unwrap_or(null()), flags) };
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

fn remove_stale_copies(dir: &Path) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_lowercase();
            if name.starts_with(&format!("{}.old-", paths::EXE_NAME)) || name.ends_with(".new") {
                let w = fsx::wide_path(&e.path());
                // SAFETY: `w` is a NUL-terminated wide path.
                unsafe { DeleteFileW(w.as_ptr()) };
            }
        }
    }
}

/// What `--uninstall` did.
#[derive(Debug, Default)]
pub struct UninstallReport {
    pub task_removed: bool,
    pub exe_removed: bool,
    /// The installed copy is running this uninstall; it is deleted at the next restart.
    pub removal_pending_reboot: bool,
}

/// Removes the autostart task and the Program Files copy. Settings (`%APPDATA%\FanRGB`) and logs
/// (`%ProgramData%\FanRGB`) are kept. Stop running instances first (the caller does).
pub fn uninstall() -> Result<UninstallReport> {
    require_elevated(ErrorCode::InstallFailed)?;
    let mut report = UninstallReport::default();
    if task::query_xml().code(ErrorCode::Autostart)?.is_some() {
        task::delete().code(ErrorCode::Autostart)?;
        report.task_removed = true;
    }
    let dir = paths::install_dir();
    let exe = paths::installed_exe();
    match fsx::check_dir(&dir).code(ErrorCode::Io)? {
        DirCheck::Missing => return Ok(report),
        DirCheck::Trusted => {}
        other => crate::bail_code!(ErrorCode::Io, "{} is not admin-only ({other:?}); not touching it", dir.display()),
    }
    remove_stale_copies(&dir);
    if exe.exists() {
        if std::fs::remove_file(&exe).is_ok() {
            report.exe_removed = true;
        } else {
            move_file(&exe, None, MOVEFILE_DELAY_UNTIL_REBOOT).code(ErrorCode::Io)?;
            report.removal_pending_reboot = true;
        }
    }
    let w = fsx::wide_path(&dir);
    // SAFETY: `w` is a NUL-terminated wide path.
    if unsafe { RemoveDirectoryW(w.as_ptr()) } == 0 {
        let _ = move_file(&dir, None, MOVEFILE_DELAY_UNTIL_REBOOT);
    }
    Ok(report)
}

// ---------------------------------------------------------------------------------------
// Autostart via Task Scheduler (runs elevated at logon without a UAC prompt), registered through the
// Task Scheduler COM API: the XML never touches the disk.

/// Start-with-Windows state for the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutostartStatus {
    /// Task registered; it starts the admin-only installed copy.
    Enabled,
    /// No task; this is the installed copy, [`set_autostart`]`(true)` works directly.
    Disabled,
    /// No task; running from a folder normal programs can modify. Offer
    /// [`install_and_enable_autostart`] ("Install to Program Files and start with Windows").
    NeedsInstall,
    /// A task exists but starts something outside the admin-only install folder (e.g. registered by
    /// an older version). Elevated startup repairs or removes it; the UI should offer "Disable" or
    /// [`install_and_enable_autostart`].
    Insecure(String),
    /// Task Scheduler could not be queried (detail).
    Unavailable(String),
}

pub fn autostart_status() -> AutostartStatus {
    match task::query_xml() {
        Err(e) => AutostartStatus::Unavailable(e.to_string()),
        Ok(Some(xml)) => {
            let cmd = task_command(&xml).unwrap_or_default();
            if fsx::same_path(Path::new(&cmd), &paths::installed_exe()) && install_location_trusted() {
                AutostartStatus::Enabled
            } else {
                AutostartStatus::Insecure(cmd)
            }
        }
        Ok(None) if is_installed_copy() => AutostartStatus::Disabled,
        Ok(None) => AutostartStatus::NeedsInstall,
    }
}

/// Kept for existing callers: true only for a correctly installed, enabled autostart.
#[allow(dead_code)] // UI API
pub fn autostart_enabled() -> bool {
    autostart_status() == AutostartStatus::Enabled
}

/// Enables autostart for the installed copy, or disables it. Enabling from a non-installed copy fails
/// with `InstallRequired`; use [`install_and_enable_autostart`].
#[allow(dead_code)] // UI API
pub fn set_autostart(enable: bool) -> Result<()> {
    if !enable {
        task::delete().code(ErrorCode::Autostart)?;
        return Ok(());
    }
    if !is_installed_copy() {
        return Err(err(ErrorCode::InstallRequired, "FanRGB runs from a folder other programs can modify"));
    }
    register_task(&paths::installed_exe())
}

/// Copies this executable to Program Files and registers autostart for that copy. Returns the
/// installed path. The current process keeps running from where it was started.
pub fn install_and_enable_autostart() -> Result<PathBuf> {
    let exe = install_self()?;
    register_task(&exe)?;
    Ok(exe)
}

fn register_task(exe: &Path) -> Result<()> {
    require_elevated(ErrorCode::Autostart)?;
    if !fsx::same_path(exe, &paths::installed_exe()) || !install_location_trusted() {
        return Err(err(ErrorCode::InstallRequired, format!("{} is not the admin-only installed copy", exe.display())));
    }
    let sid = sec::current_user_sid().code(ErrorCode::Autostart)?;
    let xml = task_xml(&exe.to_string_lossy(), &sid);
    task::register(&xml).code(ErrorCode::Autostart)?;
    log::info!("autostart registered for {}", exe.display());
    Ok(())
}

/// Elevated startup: a task left by an older version that starts an exe from a user-writable folder is
/// a silent UAC bypass. Re-point it at the installed copy if there is one, otherwise remove it.
pub fn harden_autostart() {
    if !crate::util::win::is_elevated() {
        return;
    }
    if let AutostartStatus::Insecure(cmd) = autostart_status() {
        if install_location_trusted() && register_task(&paths::installed_exe()).is_ok() {
            log::warn!("[{}] autostart pointed at {cmd:?}; re-pointed to the installed copy", ErrorCode::Autostart);
        } else if task::delete().is_ok() {
            log::warn!(
                "[{}] removed the autostart task: it started {cmd:?}, which normal programs can replace. \
                 Re-enable it after installing to Program Files.",
                ErrorCode::Autostart
            );
        }
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// The `<Command>` of a task definition, unescaped and unquoted.
pub(crate) fn task_command(xml: &str) -> Option<String> {
    let start = xml.find("<Command>")? + "<Command>".len();
    let end = start + xml[start..].find("</Command>")?;
    let cmd = xml_unescape(xml[start..end].trim());
    Some(cmd.trim_matches('"').to_string())
}

pub(crate) fn task_xml(exe: &str, user_sid: &str) -> String {
    let exe = xml_escape(exe);
    let sid = xml_escape(user_sid);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>FanRGB fan control and RGB lighting</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled><Delay>PT5S</Delay><UserId>{sid}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings>
    <AllowStartOnDemand>false</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
  </Settings>
  <Actions Context="Author"><Exec><Command>"{exe}"</Command><Arguments>--minimized</Arguments></Exec></Actions>
</Task>"#
    )
}

/// Task Scheduler COM calls (on a short-lived MTA thread: the UI thread is an OLE STA).
mod task {
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    };
    use windows::Win32::System::TaskScheduler::{
        ITaskFolder, ITaskService, TASK_CREATE_OR_UPDATE, TASK_LOGON_INTERACTIVE_TOKEN, TaskScheduler,
    };
    use windows::Win32::System::Variant::VARIANT;
    use windows::core::BSTR;

    const NOT_FOUND: [u32; 2] = [0x8007_0002, 0x8007_0003]; // HRESULT_FROM_WIN32(FILE/PATH_NOT_FOUND)

    fn with_root<T: Send>(f: impl FnOnce(&ITaskFolder) -> windows::core::Result<T> + Send) -> windows::core::Result<T> {
        std::thread::scope(|s| {
            // SAFETY: | unsafe {|COM is initialised on this thread before any COM call and uninitialised only if that succeeded; the interfaces are released (dropped) before CoUninitialize.
            s.spawn(move || unsafe {
                let init = CoInitializeEx(None, COINIT_MULTITHREADED);
                let r = (|| {
                    let svc: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
                    let v = VARIANT::default();
                    svc.Connect(&v, &v, &v, &v)?;
                    let root = svc.GetFolder(&BSTR::from("\\"))?;
                    f(&root)
                })();
                if init.is_ok() {
                    CoUninitialize();
                }
                r
            })
            .join()
            .unwrap_or_else(|_| Err(windows::core::Error::from(windows::Win32::Foundation::E_FAIL)))
        })
    }

    fn not_found(e: &windows::core::Error) -> bool {
        NOT_FOUND.contains(&(e.code().0 as u32))
    }

    /// The registered task's XML, or `None` if there is no task.
    pub fn query_xml() -> windows::core::Result<Option<String>> {
        // SAFETY: root| unsafe {|`root` is a live ITaskFolder; the BSTR argument lives for the call.
        with_root(|root| unsafe {
            match root.GetTask(&BSTR::from(super::TASK_NAME)) {
                Ok(t) => Ok(Some(t.Xml()?.to_string())),
                Err(e) if not_found(&e) => Ok(None),
                Err(e) => Err(e),
            }
        })
    }

    pub fn register(xml: &str) -> windows::core::Result<()> {
        let xml = xml.to_string();
        // SAFETY: root| unsafe {|`root` is a live ITaskFolder; BSTR/VARIANT arguments live for the call.
        with_root(move |root| unsafe {
            let v = VARIANT::default();
            root.RegisterTask(
                &BSTR::from(super::TASK_NAME),
                &BSTR::from(xml.as_str()),
                TASK_CREATE_OR_UPDATE.0,
                &v,
                &v,
                TASK_LOGON_INTERACTIVE_TOKEN,
                &v,
            )?;
            Ok(())
        })
    }

    pub fn delete() -> windows::core::Result<()> {
        // SAFETY: root| unsafe {|`root` is a live ITaskFolder; the BSTR argument lives for the call.
        with_root(|root| unsafe {
            match root.DeleteTask(&BSTR::from(super::TASK_NAME), 0) {
                Err(e) if !not_found(&e) => Err(e),
                _ => Ok(()),
            }
        })
    }
}

// ---------------------------------------------------------------------------------------
// Restarting FanRGB and starting helper programs. Nothing here goes through cmd.exe or a PATH search.

/// Makes a new instance wait until the instance with this pid has exited (used by [`restart_self`]).
pub const RESTART_AFTER_ARG: &str = "--restart-after";

static RESTART_ARGS: OnceLock<Vec<String>> = OnceLock::new();

/// Flags a restarted instance keeps (e.g. `--demo`, `--no-elevate`, `--verbose`). Set once by `main`.
pub fn set_restart_args(args: Vec<String>) {
    let _ = RESTART_ARGS.set(args);
}

/// How [`restart_self`] starts the new instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RestartPlan {
    /// Not elevated: start the same executable with the same (normal) rights.
    SameToken,
    /// Elevated and running the admin-only installed copy: start that copy directly (elevated). Normal
    /// programs cannot replace it, so no prompt is needed.
    Installed,
    /// Elevated from a folder normal programs can modify: start the executable with the user's normal
    /// rights; it then asks for administrator rights again through UAC like any normal start. Never
    /// silently re-run a replaceable file elevated.
    ViaDesktopUser,
}

pub(crate) fn restart_plan(elevated: bool, installed: bool) -> RestartPlan {
    match (elevated, installed) {
        (false, _) => RestartPlan::SameToken,
        (true, true) => RestartPlan::Installed,
        (true, false) => RestartPlan::ViaDesktopUser,
    }
}

/// Starts a new FanRGB instance that waits for this one to exit. On `Ok` the caller must exit this
/// instance (e.g. [`request_exit`]); on `Err` (code `RestartFailed` or `UnelevatedLaunch`) nothing was
/// started and this instance should keep running.
pub fn restart_self() -> Result<()> {
    let pid = std::process::id().to_string();
    let extra = RESTART_ARGS.get().cloned().unwrap_or_default();
    let mut args: Vec<&str> = vec![RESTART_AFTER_ARG, &pid];
    args.extend(extra.iter().map(String::as_str));
    let plan = restart_plan(crate::util::win::is_elevated(), is_installed_copy());
    let res = match plan {
        RestartPlan::Installed => crate::util::win::spawn_same_token(&paths::installed_exe(), &args),
        RestartPlan::SameToken => {
            let exe = std::env::current_exe().code(ErrorCode::RestartFailed)?;
            crate::util::win::spawn_same_token(&exe, &args)
        }
        RestartPlan::ViaDesktopUser => {
            let exe = std::env::current_exe().code(ErrorCode::RestartFailed)?;
            crate::util::win::spawn_unelevated(&exe, &args)
        }
    };
    match &res {
        Ok(()) => log::info!("restarting ({plan:?})"),
        Err(e) => log::warn!("[{}] restart failed: {e:#}", crate::error::code_of(e)),
    }
    res
}

/// `--restart-after <pid>`: waits (at most `timeout`) until the previous instance has exited, so this one
/// can take over the single-instance objects.
pub fn wait_for_previous_instance(pid: u32, timeout: Duration) {
    // SAFETY: plain call; the handle (or null) is owned by the wrapper.
    if let Some(h) = OwnedHandle::new(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) }) {
        // SAFETY: `h` is a valid process handle with SYNCHRONIZE access.
        unsafe { WaitForSingleObject(h.raw(), timeout.as_millis().min(u32::MAX as u128 - 1) as u32) };
        return;
    }
    // Not openable (already gone, or an elevated process seen from a normal one): watch the instance marker.
    let start = std::time::Instant::now();
    while start.elapsed() < timeout && matches!(probe_instance(), InstanceProbe::Elevated | InstanceProbe::User) {
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// winget's package family (Microsoft's "App Installer").
const WINGET_FAMILY: &str = "Microsoft.DesktopAppInstaller_8wekyb3d8bbwe";
/// The winget package id of the PawnIO driver.
pub const PAWNIO_WINGET_ID: &str = "namazso.PawnIO";

/// Checks that an App Execution Alias really points at Microsoft's winget in `%ProgramFiles%\WindowsApps`.
pub(crate) fn check_winget_link(link: &crate::util::win::AppExecLink, program_files: &Path) -> Result<(), String> {
    if !link.package_family.eq_ignore_ascii_case(WINGET_FAMILY) {
        return Err(format!("alias belongs to {:?}", link.package_family));
    }
    if !link.app_user_model_id.eq_ignore_ascii_case(&format!("{WINGET_FAMILY}!winget")) {
        return Err(format!("alias starts {:?}", link.app_user_model_id));
    }
    let t = &link.target;
    let name_ok = t.file_name().is_some_and(|n| n.eq_ignore_ascii_case("winget.exe"));
    let pkg =
        t.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let pkg_ok = pkg.starts_with("microsoft.desktopappinstaller_") && pkg.ends_with("__8wekyb3d8bbwe");
    let root_ok =
        t.parent().and_then(Path::parent).is_some_and(|r| fsx::same_path(r, &program_files.join("WindowsApps")));
    let clean = !t.components().any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::CurDir));
    if name_ok && pkg_ok && root_ok && clean {
        Ok(())
    } else {
        Err(format!("alias target {} is not Microsoft's winget", t.display()))
    }
}

/// `%LOCALAPPDATA%\Microsoft\WindowsApps\winget.exe` of the desktop user, after checking that it is the
/// App Execution Alias of Microsoft's App Installer (resolved from the known folder, never from PATH).
pub fn winget_path() -> Result<PathBuf> {
    let p = crate::util::win::desktop_user_local_app_data()?.join(r"Microsoft\WindowsApps\winget.exe");
    let link = crate::util::win::read_app_exec_link(&p).code(ErrorCode::WingetUnavailable)?;
    check_winget_link(&link, &paths::program_files()).map_err(|e| err(ErrorCode::WingetUnavailable, e))?;
    Ok(p)
}

/// Starts `winget install --id namazso.PawnIO` in its own console window with the user's normal rights
/// (winget asks for administrator rights itself, through UAC, when the installer needs them). Poll the
/// returned child for completion.
pub fn spawn_pawnio_install() -> Result<crate::util::win::UnelevatedChild> {
    let winget = winget_path()?;
    let args = ["install", "--id", PAWNIO_WINGET_ID, "-e", "--accept-source-agreements"];
    let child = crate::util::win::spawn_unelevated_child(&winget, &args, true)?;
    log::info!("started winget for PawnIO (pid {})", child.id());
    Ok(child)
}

// ---------------------------------------------------------------------------------------
// Crash watchdog: a tiny child process that restores BIOS fan control if we die (killed, crashed,
// panicked). It waits on an inherited handle to this process, so PID reuse can't fool it.

static WATCHDOG_ACTIVE: AtomicBool = AtomicBool::new(false);

/// True while the crash watchdog process is alive. The engine should not take manual control of a
/// fan without a hardware failsafe while this is false.
#[allow(dead_code)] // engine API
pub fn watchdog_active() -> bool {
    WATCHDOG_ACTIVE.load(Ordering::SeqCst)
}

/// Arguments of the `--watchdog` child.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WatchdogArgs {
    pub pid: u32,
    /// Inherited handle (value) to the parent process.
    pub parent_handle: Option<usize>,
    /// Parent's fallback state folder (only when `%ProgramData%\FanRGB` could not be secured).
    pub state_dir: Option<PathBuf>,
}

/// True if the executable file at our path is the running program: the admin-only installed copy, or
/// a pinned file whose read-only sections match the loaded image (checked once). Elevated children
/// (the watchdog) are only started from such a file.
fn own_exe_trusted() -> bool {
    static TRUSTED: OnceLock<bool> = OnceLock::new();
    *TRUSTED.get_or_init(|| {
        is_installed_copy()
            || match read_own_exe() {
                Ok(_) => true,
                Err(e) => {
                    log::error!("{e:#}");
                    false
                }
            }
    })
}

pub fn spawn_watchdog() -> Result<()> {
    use std::os::windows::process::CommandExt;
    let exe = std::env::current_exe().code(ErrorCode::Watchdog)?;
    if !own_exe_trusted() {
        crate::bail_code!(ErrorCode::Watchdog, "{} is not the running program; not starting it", exe.display());
    }
    // An inheritable handle to ourselves (SYNCHRONIZE | query) for the child to wait on.
    let mut dup: HANDLE = null_mut();
    // SAFETY: pseudo handles of the current process; `dup` is a valid out pointer and owned by OwnedHandle afterwards.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            GetCurrentProcess(),
            GetCurrentProcess(),
            &mut dup,
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            1,
            0,
        )
    };
    let dup = if ok != 0 { OwnedHandle::new(dup) } else { None };
    // Heartbeat pipe: the watchdog's stdin (inherited read end); the engine writes a byte every few
    // seconds (`watchdog_heartbeat`). Without one the watchdog only covers crashes.
    let heartbeat = match std::io::pipe() {
        Ok(p) => Some(p),
        Err(e) => {
            log::warn!("[{}] heartbeat pipe unavailable ({e}); the watchdog covers crashes only", ErrorCode::Watchdog);
            None
        }
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--watchdog").arg(std::process::id().to_string());
    let writer = heartbeat.map(|(reader, writer)| {
        cmd.stdin(reader);
        writer
    });
    if let Some(h) = &dup {
        cmd.arg("--parent-handle").arg((h.raw() as usize).to_string());
    }
    if paths::state_location() == paths::StateLocation::Fallback {
        cmd.arg("--state-dir").arg(paths::data_dir());
    }
    // std spawns with bInheritHandles = TRUE (needed for stdio), which passes `dup` on.
    let mut child = cmd.creation_flags(CREATE_NO_WINDOW).spawn().code(ErrorCode::Watchdog)?;
    drop(dup);
    // `cmd` still holds the read end: close ours so a dead watchdog breaks the pipe.
    drop(cmd);
    if let Some(w) = writer {
        let pump = std::thread::Builder::new()
            .name("fanrgb-heartbeat".into())
            .spawn(move || pump_heartbeats(w, &ENGINE_TICKS, HEARTBEAT_EVERY));
        if let Err(e) = pump {
            log::warn!(
                "[{}] heartbeat thread not started ({e}); the watchdog covers crashes only",
                ErrorCode::Watchdog
            );
        }
    }
    WATCHDOG_ACTIVE.store(true, Ordering::SeqCst);
    log::info!("crash watchdog started (pid {})", child.id());
    std::thread::Builder::new()
        .name("fanrgb-watchdog-mon".into())
        .spawn(move || {
            let status = child.wait();
            WATCHDOG_ACTIVE.store(false, Ordering::SeqCst);
            log::warn!("[{}] crash watchdog exited ({status:?})", ErrorCode::Watchdog);
        })
        .ok();
    Ok(())
}

/// Interprets a `--parent-handle` value: 0 and the pseudo handles (-1 current process, -2 current
/// thread, ... down to -8) are never inherited handles.
pub(crate) fn plausible_inherited_handle(v: usize) -> bool {
    v != 0 && v < usize::MAX - 8 && v.is_multiple_of(4)
}

/// Without an engine heartbeat for this long, the watchdog restores the BIOS fan settings although
/// the process is still alive (a hung engine). The engine ticks at least
/// every 5 s.
pub const HANG_TIMEOUT: Duration = Duration::from_secs(30);
/// The engine writes a heartbeat at most this often.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(5);

/// Engine ticks so far ([`watchdog_heartbeat`]); the heartbeat thread forwards progress.
static ENGINE_TICKS: AtomicU64 = AtomicU64::new(0);

/// Tells the watchdog the engine is alive (called every engine tick). Only counts: the pipe write
/// runs on the heartbeat thread, so a full or stuck pipe (a watchdog whose reader never started)
/// can never block the engine tick. No-op without a watchdog.
pub fn watchdog_heartbeat() {
    ENGINE_TICKS.fetch_add(1, Ordering::SeqCst);
}

/// Body of the heartbeat thread: every `every`, one byte if the engine ticked since the last
/// check. Ends when a write fails (the watchdog is gone). If the pipe blocks, only this thread
/// waits; the watchdog then misses heartbeats and treats the engine as hung, which fails safe.
fn pump_heartbeats(mut pipe: impl Write, ticks: &AtomicU64, every: Duration) {
    let mut seen = 0;
    loop {
        let n = ticks.load(Ordering::SeqCst);
        if n != seen {
            seen = n;
            if let Err(e) = pipe.write_all(&[1]).and_then(|()| pipe.flush()) {
                log::warn!("[{}] watchdog heartbeat failed ({e}); hang detection is off", ErrorCode::Watchdog);
                return;
            }
        }
        std::thread::sleep(every);
    }
}

/// The watchdog's hang detection: armed by the first heartbeat (a parent without one, e.g. an older
/// build, is only watched for crashes), fires once per silence of [`HANG_TIMEOUT`], re-arms when
/// heartbeats return.
#[derive(Debug, Default)]
pub(crate) struct HangMonitor {
    last_beat: Option<Instant>,
    fired: bool,
}

impl HangMonitor {
    pub fn beat(&mut self, now: Instant) {
        self.last_beat = Some(now);
        self.fired = false;
    }

    /// The watchdog itself did not run for `gap` (system sleep): that time is not silence.
    pub fn skip(&mut self, gap: Duration) {
        if let Some(t) = self.last_beat.as_mut() {
            *t += gap;
        }
    }

    /// True once when the heartbeat has been missing for [`HANG_TIMEOUT`].
    pub fn hung(&mut self, now: Instant) -> bool {
        let silent = self.last_beat.is_some_and(|t| now.saturating_duration_since(t) >= HANG_TIMEOUT);
        if silent && !self.fired {
            self.fired = true;
            return true;
        }
        false
    }
}

/// Counts heartbeat bytes arriving on `input` (the watchdog's stdin) on a background thread.
fn count_heartbeats(mut input: impl std::io::Read + Send + 'static) -> Arc<AtomicU64> {
    let n = Arc::new(AtomicU64::new(0));
    let c = n.clone();
    let reader = std::thread::Builder::new().name("fanrgb-watchdog-heartbeat".into()).spawn(move || {
        let mut buf = [0u8; 64];
        // EOF or an error: the parent closed the pipe (exit) or never had one.
        while let Ok(k) = input.read(&mut buf) {
            if k == 0 {
                break;
            }
            c.fetch_add(k as u64, Ordering::SeqCst);
        }
    });
    // No reader: hang detection is never armed (no heartbeat is ever counted); crash recovery
    // still works, and the parent's writes only block its heartbeat thread.
    if let Err(e) = reader {
        log::warn!("[{}] watchdog: heartbeat reader not started ({e}); hang detection is off", ErrorCode::Watchdog);
    }
    n
}

/// Body of the `--watchdog` process.
pub fn run_watchdog(args: &WatchdogArgs) {
    let parent = args
        .parent_handle
        .filter(|&v| plausible_inherited_handle(v) && args.pid != 0)
        .map(|v| v as HANDLE)
        // SAFETY: GetProcessId only queries the handle; a value that is not a process handle fails (0).
        .filter(|&h| unsafe { GetProcessId(h) } == args.pid)
        .map(OwnedHandle)
        // SAFETY: plain call; the returned handle (or null) is owned by the wrapper.
        .or_else(|| OwnedHandle::new(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, args.pid) }));
    match parent {
        Some(h) => {
            let beats = count_heartbeats(std::io::stdin());
            let mut seen = 0u64;
            let mut mon = HangMonitor::default();
            loop {
                let t0 = Instant::now();
                // SAFETY: `h` is a valid process handle with SYNCHRONIZE access.
                if unsafe { WaitForSingleObject(h.raw(), 1000) } != WAIT_TIMEOUT {
                    break; // exited (or the handle failed): crash recovery below
                }
                let now = Instant::now();
                let waited = now.duration_since(t0);
                if waited > Duration::from_secs(5) {
                    mon.skip(waited - Duration::from_secs(1));
                }
                let n = beats.load(Ordering::SeqCst);
                if n != seen {
                    seen = n;
                    mon.beat(now);
                }
                if mon.hung(now) {
                    log::error!(
                        "[{}] watchdog: no heartbeat from FanRGB (pid {}) for {} s although it is running; \
                         handing the fans back to the BIOS",
                        ErrorCode::Watchdog,
                        args.pid,
                        HANG_TIMEOUT.as_secs()
                    );
                    let r = recover_from_snapshot(Some(args.pid));
                    log::warn!("watchdog: hang recovery: {r:?}; still watching");
                }
            }
        }
        // The parent is already gone (died before we could open it): recover right away.
        None => log::warn!("watchdog: parent {} is not running", args.pid),
    }
    // The parent deletes the snapshot when it exits cleanly.
    std::thread::sleep(Duration::from_millis(500));
    recover_from_snapshot(Some(args.pid));
}

/// Why a crash snapshot was not used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotReject {
    /// Written by another process than the one we watch: leave it alone.
    OtherProcess,
    /// From before the last boot (the BIOS already re-initialized the controller).
    Stale,
    /// Implausible content (forged or corrupt).
    Invalid(String),
}

/// Identifies the current boot independently of the wall clock (review safety #7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BootStamp {
    /// Windows' boot counter (`...\Memory Management\PrefetchParameters\BootId`), +1 per boot.
    #[serde(default)]
    pub boot_id: Option<u32>,
    /// Milliseconds since boot when the snapshot was written (never decreases within a boot).
    #[serde(default)]
    pub uptime_ms: Option<u64>,
}

/// The current boot's [`BootStamp`]. The crash snapshot should carry these two fields (`boot_id`,
/// `uptime_ms`) next to `boot_time`.
pub fn boot_stamp_now() -> BootStamp {
    BootStamp { boot_id: boot_id(), uptime_ms: Some(crate::util::win::uptime_ms()) }
}

/// Windows' boot counter, readable by every user.
pub fn boot_id() -> Option<u32> {
    use windows_sys::Win32::System::Registry::{RRF_RT_REG_DWORD, RegGetValueW};
    let sub = wide(r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management\PrefetchParameters");
    let val = wide("BootId");
    let mut v = 0u32;
    let mut len = 4u32;
    // SAFETY: NUL-terminated key/value names; `v` is a writable 4-byte buffer and `len` says so.
    let rc = unsafe {
        RegGetValueW(
            crate::util::win::HKEY_LOCAL_MACHINE,
            sub.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            &mut v as *mut u32 as *mut std::ffi::c_void,
            &mut len,
        )
    };
    (rc == 0).then_some(v)
}

/// True if a snapshot written with (`snap_boot_time`, `written`) belongs to the current boot.
/// The boot counter decides when both sides have it (a wall-clock change can't fool it); uptime that
/// went backwards proves a reboot; only without either does the wall-clock boot time (±120 s) decide.
pub(crate) fn same_boot(snap_boot_time: u64, written: BootStamp, boot_time_now: u64, now: BootStamp) -> bool {
    if let (Some(w), Some(n)) = (written.uptime_ms, now.uptime_ms)
        && n < w
    {
        return false;
    }
    match (written.boot_id, now.boot_id) {
        (Some(a), Some(b)) => a == b,
        _ => snap_boot_time.abs_diff(boot_time_now) <= 120,
    }
}

/// Validates a crash snapshot before it is written to hardware. Returns the snapshot reduced to the
/// channels whose saved state is plausible firmware state.
pub(crate) fn validate_snapshot(
    snap: &crate::hw::CrashSnapshot,
    written: BootStamp,
    boot_time_now: u64,
    now: BootStamp,
    expected_pid: Option<u32>,
    own_pid: u32,
) -> std::result::Result<crate::hw::CrashSnapshot, SnapshotReject> {
    if expected_pid.is_some_and(|p| p != snap.pid) {
        return Err(SnapshotReject::OtherProcess);
    }
    if !same_boot(snap.boot_time, written, boot_time_now, now) {
        return Err(SnapshotReject::Stale);
    }
    if snap.pid == 0 || snap.pid == own_pid {
        return Err(SnapshotReject::Invalid(format!("pid {}", snap.pid)));
    }
    if snap.chips.is_empty() || snap.chips.len() > 2 {
        return Err(SnapshotReject::Invalid(format!("{} chips", snap.chips.len())));
    }
    let mut out = snap.clone();
    let mut seen = Vec::new();
    for chip in &mut out.chips {
        // Same rule as Super I/O detection: an ISA I/O base in 0x100..0x1000, 8-byte aligned.
        if chip.base < 0x100 || chip.base & 0xF007 != 0 {
            return Err(SnapshotReject::Invalid(format!("chip base {:#06x}", chip.base)));
        }
        if seen.contains(&chip.base) {
            return Err(SnapshotReject::Invalid(format!("duplicate chip base {:#06x}", chip.base)));
        }
        seen.push(chip.base);
        if chip.channels.len() > 6 {
            return Err(SnapshotReject::Invalid(format!("{} channels", chip.channels.len())));
        }
        let mut idx_seen = [false; 6];
        for (idx, _) in &chip.channels {
            if *idx >= 6 || std::mem::replace(&mut idx_seen[*idx], true) {
                return Err(SnapshotReject::Invalid(format!("channel index {idx}")));
            }
        }
        // Firmware state is automatic mode (ctrl bit 7) or a fixed duty of at least 50 %. Anything
        // else (e.g. manual 0 %) would leave the fan stopped, so it is not restored.
        chip.channels.retain(|(idx, s)| {
            let ok = s.ctrl & 0x80 != 0 || s.duty >= 0x80;
            if !ok {
                log::warn!("snapshot: not restoring PWM{} (ctrl {:#04x}, duty {:#04x})", idx + 1, s.ctrl, s.duty);
            }
            ok
        });
    }
    out.chips.retain(|c| !c.channels.is_empty());
    if out.chips.is_empty() {
        return Err(SnapshotReject::Invalid("no plausible channels".into()));
    }
    Ok(out)
}

const MAX_SNAPSHOT_BYTES: u64 = 64 * 1024;

/// What [`recover_from_snapshot`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recovery {
    /// No snapshot (normal), or one that was stale/invalid/not ours and was discarded or left alone.
    Nothing,
    /// Restored this many channels and deleted the snapshot.
    Restored(usize),
    /// A valid snapshot of this boot could not be written back. It is kept: it still holds the true
    /// firmware state, which the next attempt (or the watchdog) needs.
    Failed(String),
}

/// Restores BIOS fan settings left behind by a crashed instance. The snapshot is only trusted from the
/// admin-only state folder, only if the file itself is owned by Administrators/SYSTEM, and only after
/// validation.
pub fn recover_from_snapshot(expected_pid: Option<u32>) -> Recovery {
    if !paths::state_dir_secure() {
        log::warn!("[{}] state folder is not admin-only; crash snapshot ignored", ErrorCode::StateFolder);
        return Recovery::Nothing;
    }
    let path = paths::control_snapshot_file();
    let text = match fsx::read_limited_admin_owned(&path, MAX_SNAPSHOT_BYTES)
        .and_then(|b| String::from_utf8(b).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)))
    {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Recovery::Nothing,
        Err(e) => {
            log::error!("[{}] unusable crash snapshot ({e}); discarding", ErrorCode::Io);
            let _ = std::fs::remove_file(&path);
            return Recovery::Nothing;
        }
    };
    let Ok(snap) = serde_json::from_str::<crate::hw::CrashSnapshot>(&text) else {
        log::error!("[{}] malformed crash snapshot; discarding", ErrorCode::Internal);
        let _ = std::fs::remove_file(&path);
        return Recovery::Nothing;
    };
    // The boot stamp is read from the same JSON, so it works whether or not CrashSnapshot has the fields.
    let written = serde_json::from_str::<BootStamp>(&text).unwrap_or_default();
    let snap = match validate_snapshot(
        &snap,
        written,
        crate::util::win::boot_time_unix(),
        boot_stamp_now(),
        expected_pid,
        std::process::id(),
    ) {
        Ok(s) => s,
        Err(SnapshotReject::OtherProcess) => return Recovery::Nothing,
        Err(SnapshotReject::Stale) => {
            let _ = std::fs::remove_file(&path);
            return Recovery::Nothing;
        }
        Err(SnapshotReject::Invalid(why)) => {
            log::error!("[{}] crash snapshot rejected ({why}); discarding", ErrorCode::Internal);
            let _ = std::fs::remove_file(&path);
            return Recovery::Nothing;
        }
    };
    match crate::hw::restore_crash_snapshot(&snap) {
        Ok(n) => {
            log::warn!("recovered {n} fan channel(s) left in manual mode by a previous crash");
            let _ = std::fs::remove_file(&path);
            Recovery::Restored(n)
        }
        Err(e) => {
            log::error!("[{}] crash recovery failed: {e:#}; keeping the snapshot", crate::error::code_of(&e));
            Recovery::Failed(format!("{e:#}"))
        }
    }
}

/// Start-up recovery (elevated, before the engine starts). A failed restore is retried once; if it still
/// fails, fan take-over is blocked for this run (review safety #6): taking control now would capture
/// FanRGB's leftover settings as "firmware state" and overwrite the snapshot that holds the real one.
pub fn recover_at_startup() -> Recovery {
    let mut r = recover_from_snapshot(None);
    if let Recovery::Failed(_) = r {
        std::thread::sleep(Duration::from_millis(1000));
        r = recover_from_snapshot(None);
    }
    if let Recovery::Failed(why) = &r {
        block_takeover(format!(
            "restoring the BIOS fan settings left by a crashed FanRGB failed ({why}). FanRGB does not take \
             control of any fan until Windows restarts (the BIOS then resets the fan controller)."
        ));
    }
    r
}

static TAKEOVER_BLOCKED: OnceLock<String> = OnceLock::new();

/// Blocks software fan control for the rest of this run (see [`takeover_blocked`]).
pub fn block_takeover(why: String) {
    log::error!("[{}] fan take-over blocked: {why}", ErrorCode::RestoreFailed);
    let _ = TAKEOVER_BLOCKED.set(why);
}

/// `Some(reason)` when the engine must not take control of *any* fan and must not write or delete
/// `control_snapshot.json` (a crash snapshot of this boot could not be restored; it holds the true BIOS
/// state). Report it as [`ErrorCode::RestoreFailed`].
pub fn takeover_blocked() -> Option<String> {
    TAKEOVER_BLOCKED.get().cloned()
}

/// Problems found while starting that the UI should show (engine: report each once into the error
/// ring / alerts): state folder unusable (`StateFolder`, fix: [`repair_state_folder`]), process
/// hardening incomplete (`Hardening`), crash recovery failed (`RestoreFailed`).
pub fn startup_issues() -> Vec<crate::error::AppError> {
    use crate::error::AppError;
    let mut out = Vec::new();
    if crate::util::win::is_elevated() && paths::state_location() == paths::StateLocation::Unavailable {
        out.push(AppError::new(
            ErrorCode::StateFolder,
            format!("{} could not be secured", paths::program_data().join(paths::APP_DIR).display()),
        ));
    }
    let hardening = crate::util::sec::hardening_problems();
    if !hardening.is_empty() {
        out.push(AppError::new(ErrorCode::Hardening, hardening.join("; ")));
    }
    if let Some(why) = takeover_blocked() {
        out.push(AppError::new(ErrorCode::RestoreFailed, why));
    }
    out
}

/// Fix action for [`ErrorCode::StateFolder`]: moves a squatted `%ProgramData%\FanRGB` aside and creates
/// a protected one. On `Ok`, ask the user to restart FanRGB ([`restart_self`]) to use it.
pub fn repair_state_folder() -> Result<()> {
    require_elevated(ErrorCode::StateFolder)?;
    paths::repair_state_dir().code(ErrorCode::StateFolder)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Panic hook: a panic on the main (UI) thread ends the process (main.rs catches it, joins the engine and
// exits; a panic that cannot unwind aborts right after this hook), so hand the fans back to the BIOS
// first.

static PANIC_SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

/// Set by the panic hook when the UI thread panicked. The engine may poll it (in addition to `quit`) to
/// release fans without waiting for UI locks.
pub static UI_PANICKED: AtomicBool = AtomicBool::new(false);

pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("?");
        let msg = crate::util::logger::escape_controls(&info.to_string()).into_owned();
        log::error!("[{}] panic in thread '{name}': {msg}", ErrorCode::Internal);
        log::logger().flush();
        // Other threads' panics don't end the process (the engine catches its own, the session window
        // procedure too); only the main thread's (or a double panic, which aborts after this hook) does.
        if name != "main" {
            return;
        }
        UI_PANICKED.store(true, Ordering::SeqCst);
        if let Some(shared) = PANIC_SHARED.get()
            && !shared.engine_done.load(Ordering::SeqCst)
        {
            log::error!("returning fans to BIOS control before exiting");
            shared.quit.store(true, Ordering::SeqCst);
            // EmergencyRelease is handled at the top of the engine's next tick, before it publishes
            // state (which may need a lock the panicking UI frame still holds). main.rs additionally
            // joins the engine after the unwind released those locks.
            shared.send(crate::engine::Command::EmergencyRelease);
            shared.send(crate::engine::Command::Wake);
            let start = std::time::Instant::now();
            while !shared.engine_done.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(3) {
                std::thread::sleep(Duration::from_millis(20));
            }
            log::logger().flush();
        }
    }));
}

/// Gives the panic hook access to the engine (call once `Shared` exists).
pub fn set_panic_shared(shared: Arc<Shared>) {
    let _ = PANIC_SHARED.set(shared);
}

// ---------------------------------------------------------------------------------------
// Session watcher: logoff/shutdown and resume-from-sleep notifications.

static SESSION_SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

/// Window procedure of the hidden session window. A panic must not unwind into the OS (that aborts the
/// process with the fans possibly in manual mode), so the body runs under `catch_unwind`; on a panic the
/// fans are handed back to the BIOS.
unsafe extern "system" fn session_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let handled = std::panic::catch_unwind(|| session_message(msg, wparam));
    match handled {
        Ok(Some(r)) => r,
        // SAFETY: forwarding the unmodified message of our own window to the default procedure.
        Ok(None) => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        Err(_) => {
            // The panic hook already logged it.
            if let Some(shared) = SESSION_SHARED.get() {
                let _ = std::panic::catch_unwind(|| shared.send(crate::engine::Command::EmergencyRelease));
            }
            0
        }
    }
}

/// Handles a session message; `None` = let `DefWindowProcW` handle it.
fn session_message(msg: u32, wparam: WPARAM) -> Option<LRESULT> {
    match msg {
        WM_QUERYENDSESSION => Some(1),
        WM_ENDSESSION => {
            if wparam != 0
                && let Some(shared) = SESSION_SHARED.get()
            {
                log::info!("session ending: handing fans back to the BIOS");
                shared.quit.store(true, Ordering::SeqCst);
                shared.send(crate::engine::Command::EmergencyRelease);
                // RGB: leave the exit lighting on the controller with [CC 47 01] (persists through shutdown).
                crate::rgb::suspend_blocking(shared, Duration::from_millis(1500));
                clean_exit_for_updater();
                std::thread::sleep(Duration::from_millis(600));
            }
            Some(0)
        }
        WM_POWERBROADCAST => {
            // PBT_APMRESUMEAUTOMATIC (0x12): the engine re-applies manual mode automatically
            // because it verifies the PWM registers every tick.
            if wparam == 0x12 {
                log::info!("resumed from sleep");
                // RGB: [CC 47 00] + full controller re-init (its state was lost in sleep).
                if let Some(shared) = SESSION_SHARED.get() {
                    shared.send_rgb(crate::rgb::RgbCommand::Resume);
                    // Engine: reset control filters, re-apply hw curves/failsafe (CONTROL).
                    shared.send(crate::engine::Command::Resumed);
                }
            }
            // PBT_APMSUSPEND (0x4): hand the lighting over to the controller before sleeping.
            if wparam == 0x4
                && let Some(shared) = SESSION_SHARED.get()
            {
                crate::rgb::suspend_blocking(shared, Duration::from_millis(1500));
            }
            Some(1)
        }
        _ => None,
    }
}

pub fn spawn_session_watcher(shared: Arc<Shared>) {
    let _ = SESSION_SHARED.set(shared);
    std::thread::Builder::new()
        .name("fanrgb-session".into())
        // SAFETY: standard window-class/window/message-loop setup on this thread. `class` outlives the
        // class registration and window creation; WNDCLASSW is plain data (zeroed = valid defaults);
        // `msg` is a valid MSG buffer for the loop.
        .spawn(|| unsafe {
            let class = wide("FanRGBSessionWatcher");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(session_wndproc),
                lpszClassName: class.as_ptr(),
                hInstance: windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(null()),
                ..std::mem::zeroed()
            };
            RegisterClassW(&wc);
            // A hidden top-level window (message-only windows don't get session messages).
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                wc.hInstance,
                null(),
            );
            if hwnd.is_null() {
                return;
            }
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw::superio::ite::SavedPwm;
    use crate::hw::{ChipSnapshot, CrashSnapshot};

    /// Built through serde so fields added later (with serde defaults) don't break the tests.
    fn snap(base: u16, channels: Vec<(usize, SavedPwm)>) -> CrashSnapshot {
        let chans: Vec<serde_json::Value> =
            channels.iter().map(|(i, p)| serde_json::json!([i, serde_json::to_value(p).unwrap()])).collect();
        serde_json::from_value(serde_json::json!({
            "boot_time": 1_000_000u64,
            "pid": 4242u32,
            "chips": [{ "model": "IT8689E", "base": base, "channels": chans }],
        }))
        .unwrap()
    }

    fn chip(base: u16, channels: Vec<(usize, SavedPwm)>) -> ChipSnapshot {
        snap(base, channels).chips.remove(0)
    }

    fn pwm(ctrl: u8, duty: u8) -> SavedPwm {
        serde_json::from_str(&format!(r#"{{"ctrl":{ctrl},"duty":{duty}}}"#)).unwrap()
    }

    fn auto() -> SavedPwm {
        pwm(0x80, 0x40)
    }

    const NO_STAMP: BootStamp = BootStamp { boot_id: None, uptime_ms: None };

    /// Validation with wall-clock boot time `now` only (no boot counter on either side).
    fn check(
        s: &CrashSnapshot,
        now: u64,
        pid: Option<u32>,
        own: u32,
    ) -> std::result::Result<CrashSnapshot, SnapshotReject> {
        validate_snapshot(s, NO_STAMP, now, NO_STAMP, pid, own)
    }

    #[test]
    fn genuine_snapshot_passes() {
        let s = snap(0xa40, vec![(0, auto()), (2, auto())]);
        let v = check(&s, 1_000_030, Some(4242), 1).unwrap();
        assert_eq!(v.chips[0].channels.len(), 2);
        assert!(check(&s, 1_000_000, None, 1).is_ok());
    }

    #[test]
    fn update_handover_old_watchdog_ignores_the_new_instance() {
        // Self-update: the old instance (pid 1111) exits cleanly, its watchdog wakes up and finds the
        // snapshot the new instance (pid 4242) already wrote. It must leave it alone; only the new
        // instance's own watchdog may restore from it.
        let s = snap(0xa40, vec![(0, auto())]);
        assert_eq!(check(&s, 1_000_000, Some(1111), 1111).unwrap_err(), SnapshotReject::OtherProcess);
        assert!(check(&s, 1_000_000, Some(4242), 5555).is_ok());
    }

    #[test]
    fn snapshot_identity_checks() {
        let s = snap(0xa40, vec![(0, auto())]);
        assert!(matches!(check(&s, 1_000_000, Some(7), 1), Err(SnapshotReject::OtherProcess)));
        assert!(matches!(check(&s, 1_000_500, Some(4242), 1), Err(SnapshotReject::Stale)));
        assert!(matches!(check(&s, 1_000_000, None, 4242), Err(SnapshotReject::Invalid(_))));
        let mut z = s.clone();
        z.pid = 0;
        assert!(matches!(check(&z, 1_000_000, None, 1), Err(SnapshotReject::Invalid(_))));
    }

    #[test]
    fn staleness_uses_the_boot_counter_not_the_clock() {
        let written = BootStamp { boot_id: Some(123), uptime_ms: Some(60_000) };
        // Same boot, wall clock corrected by an hour after the snapshot was written: still this boot.
        let now = BootStamp { boot_id: Some(123), uptime_ms: Some(4_000_000) };
        assert!(same_boot(1_000_000, written, 1_003_600, now));
        // Next boot, even with an identical wall-clock boot time: stale.
        let next = BootStamp { boot_id: Some(124), uptime_ms: Some(90_000) };
        assert!(!same_boot(1_000_000, written, 1_000_000, next));
        // Uptime went backwards: a reboot happened, whatever the clocks say.
        assert!(!same_boot(
            1_000_000,
            BootStamp { boot_id: None, uptime_ms: Some(60_000) },
            1_000_000,
            BootStamp { boot_id: None, uptime_ms: Some(5_000) }
        ));
        // Old snapshots without a stamp fall back to the ±120 s wall-clock rule.
        assert!(same_boot(1_000_000, NO_STAMP, 1_000_100, now));
        assert!(!same_boot(1_000_000, NO_STAMP, 1_000_500, now));
        // Through validate_snapshot.
        let s = snap(0xa40, vec![(0, auto())]);
        assert!(validate_snapshot(&s, written, 1_003_600, now, Some(4242), 1).is_ok());
        assert_eq!(validate_snapshot(&s, written, 1_000_000, next, Some(4242), 1).unwrap_err(), SnapshotReject::Stale);
        // The stamp is read from the snapshot JSON next to the other fields.
        let json = r#"{"boot_time":1,"pid":2,"chips":[],"boot_id":123,"uptime_ms":60000}"#;
        assert_eq!(serde_json::from_str::<BootStamp>(json).unwrap(), written);
        assert_eq!(serde_json::from_str::<BootStamp>(r#"{"boot_time":1}"#).unwrap(), NO_STAMP);
    }

    #[test]
    fn this_machine_has_a_boot_counter() {
        let a = boot_stamp_now();
        assert!(a.boot_id.is_some(), "BootId not readable");
        assert!(same_boot(crate::util::win::boot_time_unix(), a, crate::util::win::boot_time_unix(), boot_stamp_now()));
    }

    #[test]
    fn forged_snapshots_are_rejected() {
        // Base outside the Super I/O HWM range or misaligned.
        // (An aligned in-range base like 0xcf8 passes this shape check; restore_crash_snapshot then only
        // touches chips that detection actually found at that base.)
        for base in [0x0000u16, 0x002e, 0x00ff, 0x0a41, 0x1000, 0xffff] {
            assert!(
                matches!(check(&snap(base, vec![(0, auto())]), 1_000_000, None, 1), Err(SnapshotReject::Invalid(_))),
                "{base:#x}"
            );
        }
        // Channel indices.
        assert!(matches!(check(&snap(0xa40, vec![(6, auto())]), 1_000_000, None, 1), Err(SnapshotReject::Invalid(_))));
        assert!(matches!(
            check(&snap(0xa40, vec![(1, auto()), (1, auto())]), 1_000_000, None, 1),
            Err(SnapshotReject::Invalid(_))
        ));
        // Too many chips.
        let mut many = snap(0xa40, vec![(0, auto())]);
        many.chips.push(chip(0xa48, vec![(0, auto())]));
        many.chips.push(chip(0xa50, vec![(0, auto())]));
        assert!(matches!(check(&many, 1_000_000, None, 1), Err(SnapshotReject::Invalid(_))));
        // Duplicate chip.
        let mut dup = snap(0xa40, vec![(0, auto())]);
        dup.chips.push(chip(0xa40, vec![(1, auto())]));
        assert!(matches!(check(&dup, 1_000_000, None, 1), Err(SnapshotReject::Invalid(_))));
    }

    #[test]
    fn stopped_fan_state_is_not_restored() {
        let stop = pwm(0x00, 0x00);
        let full = pwm(0x00, 0xff);
        let v = check(&snap(0xa40, vec![(0, stop), (1, full), (2, auto())]), 1_000_000, None, 1).unwrap();
        let idx: Vec<usize> = v.chips[0].channels.iter().map(|(i, _)| *i).collect();
        assert_eq!(idx, vec![1, 2]);
        assert!(matches!(check(&snap(0xa40, vec![(0, stop)]), 1_000_000, None, 1), Err(SnapshotReject::Invalid(_))));
    }

    #[test]
    fn snapshot_must_be_owned_by_admins() {
        // A file written by this (test) user, as a squatter would: refused before it is parsed. When the
        // test runs elevated under UAC the owner is Administrators and the file is accepted (without UAC
        // the owner is the user; FanRGB's own writes set Administrators, see fsx::write_atomic).
        let d = crate::util::fsx::tests::test_dir("snapowner");
        let p = d.join("control_snapshot.json");
        std::fs::write(&p, br#"{"boot_time":1,"pid":2,"chips":[]}"#).unwrap();
        let r = fsx::read_limited_admin_owned(&p, MAX_SNAPSHOT_BYTES);
        assert_eq!(r.is_ok(), crate::util::win::default_owner_is_admin(), "{r:?}");
        if let Err(e) = r {
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        }
    }

    #[test]
    fn failed_recovery_blocks_takeover_and_is_reported() {
        // (Process-global, so only this test sets it.)
        assert!(takeover_blocked().is_none());
        block_takeover("verify mismatch".into());
        assert!(takeover_blocked().is_some_and(|w| w.contains("verify mismatch")));
        assert!(startup_issues().iter().any(|e| e.code == ErrorCode::RestoreFailed));
    }

    #[test]
    fn pseudo_and_zero_parent_handles_are_rejected() {
        for v in [0usize, usize::MAX, usize::MAX - 1, usize::MAX - 5, usize::MAX - 7, 3] {
            assert!(!plausible_inherited_handle(v), "{v:#x}");
        }
        assert!(plausible_inherited_handle(0x2c4));
    }

    #[test]
    fn task_xml_escapes_and_round_trips() {
        let exe = r"C:\Program Files\R&D 'tools'\FanRGB\fanrgb.exe";
        let xml = task_xml(exe, "S-1-5-21-1-2-3-1001");
        assert!(xml.contains("R&amp;D &apos;tools&apos;"));
        assert!(xml.contains("<UserId>S-1-5-21-1-2-3-1001</UserId>"));
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(xml.contains("<AllowStartOnDemand>false</AllowStartOnDemand>"));
        assert_eq!(task_command(&xml).as_deref(), Some(exe));
        // Task Scheduler's own XML (escaped quotes) parses too.
        let ts = "<Exec><Command>&quot;C:\\Program Files\\FanRGB\\fanrgb.exe&quot;</Command></Exec>";
        assert_eq!(task_command(ts).as_deref(), Some(r"C:\Program Files\FanRGB\fanrgb.exe"));
        assert_eq!(task_command("<Task/>"), None);
    }

    fn own_base() -> *const u8 {
        // SAFETY: base of this test executable's mapped image.
        unsafe { windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(null()) as *const u8 }
    }

    #[test]
    fn running_image_matches_its_file_and_detects_changes() {
        let exe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        // SAFETY: own image base, layout checked inside.
        unsafe { image_matches_file(own_base(), &exe) }.unwrap();
        // One flipped byte in the code section is detected.
        let l = pe_layout(&exe).unwrap();
        let text = l.sections.iter().find(|s| s.4 & 0x2000_0000 != 0).unwrap(); // IMAGE_SCN_MEM_EXECUTE
        let mut bad = exe.clone();
        bad[text.2 as usize + 0x100] ^= 0xff;
        // SAFETY: as above.
        assert!(unsafe { image_matches_file(own_base(), &bad) }.is_err());
        // Read-only data too (embedded assets live there).
        let rdata = l
            .sections
            .iter()
            .find(|s| s.4 & (SCN_MEM_WRITE | 0x2000_0000 | SCN_MEM_DISCARDABLE) == 0 && s.3 > 0x2000)
            .unwrap();
        let mut bad = exe.clone();
        bad[rdata.2 as usize + rdata.3 as usize - 0x800] ^= 0x55;
        // SAFETY: as above.
        assert!(unsafe { image_matches_file(own_base(), &bad) }.is_err());
        // SAFETY: as above.
        assert!(unsafe { image_matches_file(own_base(), b"MZ not a pe") }.is_err());
    }

    /// audit FR-PRIV-001 (independent tester, on the real loader-mapped image of this test exe): a
    /// swapped file whose own headers are doctored to hide its changes is refused. Each variant
    /// changes what the old check took from the file (entry point, import / TLS / load-config
    /// directories, the IAT range it skipped, a section's raw size or flags, SizeOfHeaders,
    /// ImageBase), most of them together with a changed code byte the doctored header would hide.
    #[test]
    fn swapped_file_with_doctored_headers_is_refused() {
        let exe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        // SAFETY (every call below): own image base; the function checks the layout first.
        let check = |file: &[u8]| unsafe { image_matches_file(own_base(), file) };
        check(&exe).expect("the genuine file matches its loader-mapped image on Windows");
        let l = pe_layout(&exe).unwrap();
        let pe = rd32(&exe, 0x3c).unwrap() as usize;
        let oh = pe + 24;
        let st = oh + rd16(&exe, pe + 20).unwrap() as usize;
        let dir = |i: usize| oh + 112 + 8 * i;
        let put32 = |b: &mut Vec<u8>, at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        let ti = l.sections.iter().position(|s| s.4 & 0x2000_0000 != 0).unwrap(); // IMAGE_SCN_MEM_EXECUTE
        let text = l.sections[ti];
        let code_at = text.2 as usize + (text.3.min(text.1) as usize) / 2;
        let flip_code = |b: &mut Vec<u8>| b[code_at] ^= 0xa5;

        let mut variants: Vec<(&str, Vec<u8>)> = Vec::new();
        let mut b = exe.clone();
        put32(&mut b, oh + 16, rd32(&exe, oh + 16).unwrap().wrapping_add(0x40));
        variants.push(("entry point moved", b));
        let mut b = exe.clone();
        put32(&mut b, dir(1), text.0 + 0x80);
        variants.push(("import directory re-pointed", b));
        let mut b = exe.clone();
        put32(&mut b, dir(9), text.0);
        put32(&mut b, dir(9) + 4, 0x28);
        variants.push(("TLS directory added", b));
        let mut b = exe.clone();
        put32(&mut b, dir(10) + 4, rd32(&exe, dir(10) + 4).unwrap() ^ 0x8);
        variants.push(("load-config directory resized (DLL search pin)", b));
        let mut b = exe.clone();
        put32(&mut b, dir(12), 0);
        put32(&mut b, dir(12) + 4, l.size_of_image);
        flip_code(&mut b);
        variants.push(("IAT = the whole image, code changed", b));
        let mut b = exe.clone();
        put32(&mut b, st + ti * 40 + 16, 1);
        flip_code(&mut b);
        variants.push(("code section SizeOfRawData = 1, code changed", b));
        let mut b = exe.clone();
        put32(&mut b, st + ti * 40 + 36, text.4 | SCN_MEM_WRITE);
        flip_code(&mut b);
        variants.push(("code section marked writable (skipped), code changed", b));
        let mut b = exe.clone();
        put32(&mut b, oh + 60, (st + 40) as u32);
        variants.push(("SizeOfHeaders cut into the section table", b));
        let mut b = exe.clone();
        put32(&mut b, oh + 60, 0x10_0000);
        variants.push(("SizeOfHeaders beyond the mapped header page", b));
        // ImageBase is the one header field allowed to differ (the loader rewrites it), so a fake one
        // must not shift the relocated comparison into a match either.
        let mut b = exe.clone();
        let fake = l.image_base.wrapping_add(0x1_0000);
        b[oh + 24..oh + 32].copy_from_slice(&fake.to_le_bytes());
        variants.push(("ImageBase changed", b));
        let mut b = exe.clone();
        b.truncate(text.2 as usize + 16);
        variants.push(("file cut inside the code section", b));
        for (what, file) in &variants {
            assert!(check(file).is_err(), "{what}: accepted");
        }
        // The header / section-table comparison is what refuses the header edits (not a later accident).
        for (what, file) in &variants[..7] {
            let why = check(file).unwrap_err();
            assert!(why.contains("headers") || why.contains("section table"), "{what}: {why}");
        }
        // RVA arithmetic on hostile section tables is done without u32 overflow (no panic in debug).
        let hostile = PeLayout {
            image_base: 0,
            size_of_image: u32::MAX,
            image_base_at: 0,
            size_of_headers: 0,
            headers_end: 0,
            sections: vec![(u32::MAX - 0x10, u32::MAX, u32::MAX - 4, u32::MAX, 0)],
            dirs: Vec::new(),
        };
        assert_eq!(hostile.rva_to_offset(u32::MAX), Some((u32::MAX - 4) as usize + 0x10));
        assert_eq!(hostile.rva_to_offset(0), None);
    }

    #[test]
    fn pinned_exe_reads_back_the_running_image() {
        pin_own_exe();
        let bytes = read_own_exe().unwrap();
        assert_eq!(sha256(&bytes), sha256(&std::fs::read(std::env::current_exe().unwrap()).unwrap()));
        // While pinned, the file can't be renamed (swapped) under us.
        let exe = std::env::current_exe().unwrap();
        assert!(std::fs::rename(&exe, exe.with_extension("swapped")).is_err());
    }

    #[test]
    fn restart_never_reruns_a_replaceable_exe_elevated() {
        assert_eq!(restart_plan(true, true), RestartPlan::Installed);
        assert_eq!(restart_plan(true, false), RestartPlan::ViaDesktopUser);
        assert_eq!(restart_plan(false, false), RestartPlan::SameToken);
        assert_eq!(restart_plan(false, true), RestartPlan::SameToken);
    }

    #[test]
    fn winget_alias_must_be_microsofts() {
        use crate::util::win::AppExecLink;
        let pf = paths::program_files();
        let good = AppExecLink {
            package_family: "Microsoft.DesktopAppInstaller_8wekyb3d8bbwe".into(),
            app_user_model_id: "Microsoft.DesktopAppInstaller_8wekyb3d8bbwe!winget".into(),
            target: pf.join(r"WindowsApps\Microsoft.DesktopAppInstaller_1.29.379.0_x64__8wekyb3d8bbwe\winget.exe"),
        };
        assert!(check_winget_link(&good, &pf).is_ok());
        let mut bad = good.clone();
        bad.package_family = "Evil.Package_1234567890abc".into();
        assert!(check_winget_link(&bad, &pf).is_err());
        let mut bad = good.clone();
        bad.target = PathBuf::from(r"C:\Users\x\AppData\Local\Temp\winget.exe");
        assert!(check_winget_link(&bad, &pf).is_err());
        let mut bad = good.clone();
        bad.target = pf.join(r"WindowsApps\Microsoft.DesktopAppInstaller_1.0_x64__8wekyb3d8bbwe\..\..\evil\winget.exe");
        assert!(check_winget_link(&bad, &pf).is_err());
        let mut bad = good.clone();
        bad.target = pf.join(r"WindowsApps\Evil_1.0_x64__8wekyb3d8bbwe\winget.exe");
        assert!(check_winget_link(&bad, &pf).is_err());
        // On this machine (read-only): the real alias, if winget is installed, passes.
        if crate::util::win::skip_without_unelevated_shell() {
            return;
        }
        let alias = crate::util::win::desktop_user_local_app_data().unwrap().join(r"Microsoft\WindowsApps\winget.exe");
        if std::fs::symlink_metadata(&alias).is_ok() {
            let p = winget_path().unwrap();
            assert!(p.ends_with(r"Microsoft\WindowsApps\winget.exe"));
        }
    }

    #[test]
    fn waiting_for_an_exited_instance_returns_at_once() {
        let mut c = std::process::Command::new(crate::util::win::system32().join("hostname.exe"))
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = c.id();
        c.wait().unwrap();
        drop(c);
        let t = std::time::Instant::now();
        // (A real FanRGB running on this machine would make the fallback poll wait; keep the timeout short.)
        wait_for_previous_instance(pid, Duration::from_secs(2));
        assert!(t.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn user_instance_objects_detect_a_second_instance() {
        // Uses unique names so a running FanRGB (or another test) is not disturbed.
        let name = format!(r"Local\FanRGB.Test.{}", std::process::id());
        let (_a, existed) = create_object(Kind::Mutex, &name, None).unwrap();
        assert!(!existed);
        let (_b, existed) = create_object(Kind::Mutex, &name, None).unwrap();
        assert!(existed);
        // Created by this (test) process with no SDDL: the token's default owner (the user, or
        // Administrators when elevated under UAC). FanRGB's real objects name O:BA in their SDDL.
        let h = open_object(Kind::Mutex, &name, SYNCHRONIZATION_SYNCHRONIZE | READ_CONTROL).unwrap();
        assert_eq!(owned_by_admin(&h), crate::util::win::default_owner_is_admin());
        // A squatter's object with a DACL that denies us is reported, not waited on.
        let squat = format!(r"Local\FanRGB.Test.Squat.{}", std::process::id());
        let _s = create_object(Kind::Mutex, &squat, Some("D:P(D;;GA;;;WD)")).unwrap();
        assert!(create_object(Kind::Mutex, &squat, None).is_err());
    }

    #[test]
    fn task_scheduler_query_is_read_only_and_works() {
        // Only queries (GetTask); never registers or deletes anything.
        let status = autostart_status();
        assert!(!matches!(status, AutostartStatus::Unavailable(_)), "{status:?}");
        assert!(!is_installed_copy());
    }

    #[test]
    fn private_namespace_needs_admin() {
        // Non-elevated: the Administrators SID is deny-only, so the namespace can't be created/opened.
        let r = private_namespace(false);
        if !crate::util::win::is_elevated() {
            assert!(r.is_err());
        }
    }

    /// The engine only counts ticks; the heartbeat thread writes at most one
    /// byte per interval while ticks advance, stops writing when they stop, and a pipe that never
    /// drains blocks only that thread.
    #[test]
    fn heartbeat_pump_never_blocks_the_engine() {
        use std::sync::Mutex;
        #[derive(Clone, Default)]
        struct Sink(Arc<Mutex<usize>>);
        impl Write for Sink {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                *self.0.lock().unwrap() += b.len();
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let ticks: &'static AtomicU64 = Box::leak(Box::new(AtomicU64::new(0)));
        let sink = Sink::default();
        let out = sink.0.clone();
        std::thread::spawn(move || pump_heartbeats(sink, ticks, Duration::from_millis(20)));
        let bytes = || *out.lock().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(bytes(), 0, "no tick, no heartbeat");
        for _ in 0..1000 {
            ticks.fetch_add(1, Ordering::SeqCst);
        }
        std::thread::sleep(Duration::from_millis(100));
        let n = bytes();
        assert!((1..=2).contains(&n), "one byte per interval, not per tick: {n}");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(bytes(), n, "engine stalled: no more heartbeats");
        // A pipe whose reader never runs: the writer thread blocks, the engine never does.
        let (_r, w) = std::io::pipe().unwrap();
        let blocked: &'static AtomicU64 = Box::leak(Box::new(AtomicU64::new(0)));
        std::thread::spawn(move || {
            // 64 KiB per "tick" fills any pipe buffer at once.
            struct Big(std::io::PipeWriter);
            impl Write for Big {
                fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                    self.0.write_all(&[1; 65536]).map(|()| 1)
                }
                fn flush(&mut self) -> io::Result<()> {
                    Ok(())
                }
            }
            pump_heartbeats(Big(w), blocked, Duration::from_millis(1))
        });
        let t0 = Instant::now();
        for _ in 0..10_000 {
            blocked.fetch_add(1, Ordering::SeqCst);
            watchdog_heartbeat();
        }
        std::thread::sleep(Duration::from_millis(50));
        for _ in 0..10_000 {
            blocked.fetch_add(1, Ordering::SeqCst);
            watchdog_heartbeat();
        }
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
        // A broken pipe ends the thread.
        let (r, w) = std::io::pipe().unwrap();
        drop(r);
        let done = std::thread::spawn(move || pump_heartbeats(w, ticks, Duration::from_millis(1)));
        ticks.fetch_add(1, Ordering::SeqCst);
        let t0 = Instant::now();
        while !done.is_finished() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(done.is_finished(), "ends when the watchdog is gone");
    }

    /// A simulated engine stall. Heartbeats travel over a real pipe; once
    /// they stop for 30 s the monitor fires exactly once, a sleep gap is not silence, and it re-arms
    /// when heartbeats return.
    #[test]
    fn hang_watchdog_fires_once_per_stall() {
        let (r, mut w) = std::io::pipe().unwrap();
        let beats = count_heartbeats(r);
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut mon = HangMonitor::default();
        assert!(!mon.hung(at(100)), "not armed before the first heartbeat (older parent)");
        let mut seen = 0;
        let mut poll = |mon: &mut HangMonitor, now: Instant| {
            for _ in 0..200 {
                let n = beats.load(Ordering::SeqCst);
                if n != seen {
                    seen = n;
                    mon.beat(now);
                    return true;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            false
        };
        // Engine alive: a beat every 5 s.
        for s in (0..60).step_by(5) {
            w.write_all(&[1]).unwrap();
            assert!(poll(&mut mon, at(s)), "heartbeat {s} arrived");
            assert!(!mon.hung(at(s + 4)));
        }
        // Stall: no beat after t = 55 s.
        assert!(!mon.hung(at(84)), "29 s of silence is tolerated");
        assert!(mon.hung(at(85)), "30 s: restore the BIOS settings");
        assert!(!mon.hung(at(86)) && !mon.hung(at(300)), "only once per stall");
        // The engine comes back: re-armed.
        w.write_all(&[1]).unwrap();
        assert!(poll(&mut mon, at(301)));
        assert!(!mon.hung(at(320)));
        // A 2 h system sleep is not silence.
        mon.skip(Duration::from_secs(7200));
        assert!(!mon.hung(at(7520)));
        assert!(mon.hung(at(7531)));
        // The parent exits: the pipe breaks and the reader thread ends without a beat.
        drop(w);
    }
}
