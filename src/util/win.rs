//! Small, safe wrappers around the Win32 APIs used throughout the app.

use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use anyhow::Result;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows_sys::Win32::Security::{
    DuplicateTokenEx, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, SecurityImpersonation,
    TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_ELEVATION,
    TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenElevation, TokenIntegrityLevel, TokenPrimary,
    TokenSessionId,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Registry::{
    HKEY, KEY_NOTIFY, KEY_READ, KEY_SET_VALUE, REG_DWORD, REG_NOTIFY_CHANGE_LAST_SET, REG_NOTIFY_CHANGE_NAME, REG_SZ,
    RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegEnumValueW, RegGetValueW, RegNotifyChangeKeyValue,
    RegOpenKeyExW, RegSetValueExW,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemDirectoryW, GetSystemWindowsDirectoryW, GetTickCount64};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_CONSOLE, CREATE_SUSPENDED, CreateEventW, CreateProcessW, CreateProcessWithTokenW, GetCurrentProcess,
    GetExitCodeProcess, INFINITE, OpenProcess, OpenProcessToken, PROCESS_INFORMATION, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW, ResumeThread, STARTUPINFOW, SetEvent,
    TerminateProcess, WaitForMultipleObjects, WaitForSingleObject,
};
use windows_sys::Win32::UI::Shell::{SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId, SW_SHOWNORMAL};

use crate::error::{ErrorCode, err};

use windows_sys::Win32::System::Registry::HKEY_USERS;
pub use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

/// Null-terminated UTF-16 string.
pub fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// Converts a (possibly null-terminated) UTF-16 buffer into a `String`.
pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Owned kernel handle that is closed on drop.
pub struct OwnedHandle(pub HANDLE);

// SAFETY: a kernel handle is a process-wide table index; it may be used and closed from any thread.
unsafe impl Send for OwnedHandle {}
// SAFETY: as above; `&OwnedHandle` only exposes the raw value.
unsafe impl Sync for OwnedHandle {}

impl OwnedHandle {
    pub fn new(h: HANDLE) -> Option<Self> {
        if h.is_null() || h == INVALID_HANDLE_VALUE { None } else { Some(Self(h)) }
    }
    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a real handle owned exclusively by this wrapper (constructors reject
        // null/INVALID_HANDLE_VALUE); it is closed exactly once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

pub fn reg_get_string(root: HKEY, subkey: &str, value: &str) -> Option<String> {
    let sub = wide(subkey);
    let val = wide(value);
    let mut buf = vec![0u16; 512];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: `sub`/`val` are NUL-terminated; `buf` is writable for `len` bytes; RegGetValueW writes at
    // most `len` bytes and updates `len`.
    let rc = unsafe {
        RegGetValueW(
            root,
            sub.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            buf.as_mut_ptr() as *mut c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return None;
    }
    let s = from_wide(&buf).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// A REG_DWORD value.
pub fn reg_get_dword(root: HKEY, subkey: &str, value: &str) -> Option<u32> {
    let sub = wide(subkey);
    let val = wide(value);
    let mut v = 0u32;
    let mut len = 4u32;
    // SAFETY: NUL-terminated names; `v` is a writable 4-byte buffer and `len` says so.
    let rc = unsafe {
        RegGetValueW(
            root,
            sub.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            &mut v as *mut u32 as *mut c_void,
            &mut len,
        )
    };
    (rc == 0).then_some(v)
}

/// A registry key opened read-only, closed on drop.
struct RegKey(HKEY);

impl RegKey {
    fn open(root: HKEY, subkey: &str) -> Option<RegKey> {
        let sub = wide(subkey);
        let mut h: HKEY = null_mut();
        // SAFETY: NUL-terminated name; `h` is a valid out pointer, owned by the wrapper on success.
        (unsafe { RegOpenKeyExW(root, sub.as_ptr(), 0, KEY_READ, &mut h) } == 0).then_some(RegKey(h))
    }
}

impl Drop for RegKey {
    fn drop(&mut self) {
        // SAFETY: an open key owned by this wrapper, closed once.
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

/// Sets an existing REG_DWORD-capable value on an *existing* key (the key is never created).
/// Used only for the per-device Dynamic Lighting switch under `HKCU\Software\Microsoft\Lighting`.
pub fn reg_set_dword(root: HKEY, subkey: &str, value: &str, data: u32) -> Result<()> {
    let sub = wide(subkey);
    let val = wide(value);
    let mut h: HKEY = null_mut();
    // SAFETY: NUL-terminated name; `h` is a valid out pointer and is closed below on success.
    let rc = unsafe { RegOpenKeyExW(root, sub.as_ptr(), 0, KEY_SET_VALUE, &mut h) };
    if rc != 0 {
        return Err(err(ErrorCode::Io, format!("opening registry key {subkey} for writing failed (Win32 error {rc})")));
    }
    let key = RegKey(h);
    let bytes = data.to_le_bytes();
    // SAFETY: open key; NUL-terminated value name; `bytes` is 4 readable bytes and that size is passed.
    let rc = unsafe { RegSetValueExW(key.0, val.as_ptr(), 0, REG_DWORD, bytes.as_ptr(), 4) };
    if rc != 0 {
        return Err(err(ErrorCode::Io, format!("writing {subkey}\\{value} failed (Win32 error {rc})")));
    }
    Ok(())
}

/// An auto-reset event: one thread blocks in [`Signal::wait`] (or a [`RegWatch::wait`]) and another
/// ends the wait early with [`Signal::set`]. Lets an idle thread block for seconds instead of
/// polling a flag or a channel with short sleeps.
pub struct Signal(OwnedHandle);

impl Signal {
    pub fn new() -> Option<Signal> {
        // SAFETY: an unnamed auto-reset event, initially unset, default security; owned by the wrapper.
        OwnedHandle::new(unsafe { CreateEventW(null(), 0, 0, null()) }).map(Signal)
    }

    /// Wakes the waiter (or the next wait, if nobody waits right now).
    pub fn set(&self) {
        // SAFETY: a valid event handle owned by `self`.
        unsafe { SetEvent(self.0.raw()) };
    }

    /// Blocks up to `timeout`; true when [`Signal::set`] ended the wait.
    pub fn wait(&self, timeout: std::time::Duration) -> bool {
        // SAFETY: a valid event handle owned by `self`.
        unsafe { WaitForSingleObject(self.0.raw(), wait_ms(timeout)) == WAIT_OBJECT_0 }
    }
}

fn wait_ms(timeout: std::time::Duration) -> u32 {
    timeout.as_millis().min(u32::MAX as u128 - 1) as u32
}

/// Sleeps up to `timeout`, ended early by `wake`. Without a signal it sleeps at most `fallback`
/// (callers then poll at that rate, as they would without the signal).
pub fn pause(wake: Option<&Signal>, timeout: std::time::Duration, fallback: std::time::Duration) {
    match wake {
        Some(w) => {
            w.wait(timeout);
        }
        None => std::thread::sleep(timeout.min(fallback)),
    }
}

/// Waits for changes under a registry key (`RegNotifyChangeKeyValue`, subtree, names + values),
/// like GCC's Dynamic Lighting monitor. Re-arms itself after each signal.
pub struct RegWatch {
    key: RegKey,
    event: OwnedHandle,
    armed: bool,
}

impl RegWatch {
    pub fn new(root: HKEY, subkey: &str) -> Option<RegWatch> {
        let sub = wide(subkey);
        let mut h: HKEY = null_mut();
        // SAFETY: NUL-terminated name; `h` is a valid out pointer, owned by the wrapper on success.
        if unsafe { RegOpenKeyExW(root, sub.as_ptr(), 0, KEY_NOTIFY | KEY_READ, &mut h) } != 0 {
            return None;
        }
        let key = RegKey(h);
        // SAFETY: an unnamed auto-reset event with default security; the handle is owned by the wrapper.
        let ev = unsafe { CreateEventW(null(), 0, 0, null()) };
        let event = OwnedHandle::new(ev)?;
        Some(RegWatch { key, event, armed: false })
    }

    /// Blocks up to `timeout` for a change, or until `wake` is set. Returns true when the key (or a
    /// subkey) changed.
    pub fn wait(&mut self, timeout: std::time::Duration, wake: Option<&Signal>) -> bool {
        if !self.armed {
            // SAFETY: open key with KEY_NOTIFY; the event handle lives as long as `self` (the
            // registration ends when the key is closed, before the event is).
            let rc = unsafe {
                RegNotifyChangeKeyValue(
                    self.key.0,
                    1,
                    REG_NOTIFY_CHANGE_NAME | REG_NOTIFY_CHANGE_LAST_SET,
                    self.event.raw(),
                    1,
                )
            };
            if rc != 0 {
                pause(wake, timeout, std::time::Duration::from_millis(500));
                return false;
            }
            self.armed = true;
        }
        let ms = wait_ms(timeout);
        let signalled = match wake {
            Some(w) => {
                let handles = [self.event.raw(), w.0.raw()];
                // SAFETY: two valid event handles owned by `self` and `w`; wait for any one.
                unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, ms) == WAIT_OBJECT_0 }
            }
            // SAFETY: a valid event handle owned by `self`.
            None => unsafe { WaitForSingleObject(self.event.raw(), ms) == WAIT_OBJECT_0 },
        };
        if signalled {
            self.armed = false;
        }
        signalled
    }
}

/// Names of the subkeys of `subkey` (at most `limit`; empty if the key does not exist).
pub fn reg_subkeys(root: HKEY, subkey: &str, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let Some(k) = RegKey::open(root, subkey) else { return out };
    for i in 0..limit as u32 {
        let mut name = [0u16; 512];
        let mut len = name.len() as u32;
        // SAFETY: `name` is writable for `len` UTF-16 units; the optional out pointers are null.
        let rc =
            unsafe { RegEnumKeyExW(k.0, i, name.as_mut_ptr(), &mut len, null(), null_mut(), null_mut(), null_mut()) };
        if rc != 0 {
            break;
        }
        out.push(String::from_utf16_lossy(&name[..len as usize]));
    }
    out
}

/// The REG_SZ values of `subkey` as (name, data), at most `limit`.
pub fn reg_string_values(root: HKEY, subkey: &str, limit: usize) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(k) = RegKey::open(root, subkey) else { return out };
    for i in 0..limit as u32 {
        let mut name = [0u16; 256];
        let mut name_len = name.len() as u32;
        let mut data = [0u16; 1024];
        let mut data_len = (data.len() * 2) as u32;
        let mut ty = 0u32;
        // SAFETY: `name` / `data` are writable for the sizes passed (UTF-16 units / bytes).
        let rc = unsafe {
            RegEnumValueW(
                k.0,
                i,
                name.as_mut_ptr(),
                &mut name_len,
                null(),
                &mut ty,
                data.as_mut_ptr() as *mut u8,
                &mut data_len,
            )
        };
        if rc != 0 {
            break;
        }
        if ty == REG_SZ {
            let n = (data_len as usize / 2).min(data.len());
            out.push((String::from_utf16_lossy(&name[..name_len as usize]), from_wide(&data[..n])));
        }
    }
    out
}

/// Opens the token of `process` (a real or pseudo handle) with `access`.
fn process_token(process: HANDLE, access: u32) -> Option<OwnedHandle> {
    let mut token: HANDLE = null_mut();
    // SAFETY: `token` is a valid out pointer; the handle is owned by the returned wrapper.
    if unsafe { OpenProcessToken(process, access, &mut token) } == 0 {
        return None;
    }
    OwnedHandle::new(token)
}

/// A `u32`-sized token information class (TokenElevation, TokenSessionId).
fn token_u32(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Option<u32> {
    let mut v = 0u32;
    let mut ret = 0u32;
    // SAFETY: `v` is a writable 4-byte buffer and its size is passed; `ret` is a valid out pointer.
    let ok = unsafe { GetTokenInformation(token, class, &mut v as *mut u32 as *mut c_void, 4, &mut ret) };
    (ok != 0).then_some(v)
}

/// Mandatory integrity level RID of a token (0x2000 medium, 0x3000 high, 0x4000 system).
fn token_integrity(token: HANDLE) -> Option<u32> {
    let mut buf = vec![0u64; 16]; // 128 bytes, 8-byte aligned: TOKEN_MANDATORY_LABEL + SID
    let mut ret = 0u32;
    // SAFETY: `buf` is writable for the size passed and suitably aligned for TOKEN_MANDATORY_LABEL.
    let ok = unsafe { GetTokenInformation(token, TokenIntegrityLevel, buf.as_mut_ptr() as *mut c_void, 128, &mut ret) };
    if ok == 0 {
        return None;
    }
    // SAFETY: on success the buffer starts with a TOKEN_MANDATORY_LABEL whose SID points into the same
    // buffer; the SID has at least one sub-authority (the integrity RID), and the count is checked.
    unsafe {
        let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let sid = label.Label.Sid;
        if sid.is_null() {
            return None;
        }
        let count = *GetSidSubAuthorityCount(sid);
        if count == 0 {
            return None;
        }
        Some(*GetSidSubAuthority(sid, count as u32 - 1))
    }
}

/// SID string (`S-1-5-21-…`) of a token's user.
fn token_user_sid(token: HANDLE) -> Option<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{TOKEN_USER, TokenUser};
    let mut buf = vec![0u64; 64]; // 512 bytes, 8-byte aligned: TOKEN_USER + SID (at most 76 bytes)
    let mut ret = 0u32;
    // SAFETY: `buf` is writable for the size passed and suitably aligned for TOKEN_USER.
    let ok = unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr() as *mut c_void, 512, &mut ret) };
    if ok == 0 {
        return None;
    }
    let mut s: *mut u16 = null_mut();
    // SAFETY: on success the buffer starts with a TOKEN_USER whose SID points into the same buffer;
    // `s` receives a LocalAlloc'ed NUL-terminated string, freed below.
    unsafe {
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        if user.User.Sid.is_null() || ConvertSidToStringSidW(user.User.Sid, &mut s) == 0 || s.is_null() {
            return None;
        }
        let mut n = 0;
        while n < 256 && *s.add(n) != 0 {
            n += 1;
        }
        let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
        LocalFree(s as *mut c_void);
        is_user_sid(&out).then_some(out)
    }
}

/// `S-1-<digits>-<digits>…`: safe to use as a key name under `HKEY_USERS`.
fn is_user_sid(s: &str) -> bool {
    s.len() <= 184
        && s.strip_prefix("S-1-").is_some_and(|rest| {
            rest.split('-').all(|p| !p.is_empty() && p.len() <= 10 && p.bytes().all(|b| b.is_ascii_digit()))
        })
}

/// The desktop user's registry hive (`HKEY_CURRENT_USER` as *they* see it). See [`desktop_user_hive`].
pub struct UserHive {
    key: HKEY,
    /// `HKEY_USERS\<sid>` opened by us (closed on drop); otherwise the predefined `HKEY_CURRENT_USER`.
    owned: bool,
    sid: Option<String>,
}

// SAFETY: a registry key handle has no thread affinity; it is only closed once, on drop.
unsafe impl Send for UserHive {}

impl UserHive {
    /// Root key to pass to the `reg_*` helpers (subkeys relative to the user's hive).
    pub fn root(&self) -> HKEY {
        self.key
    }

    /// `HKEY_USERS\<sid>`, or `HKCU` when FanRGB runs as the desktop user without elevation.
    pub fn describe(&self) -> String {
        match &self.sid {
            Some(sid) => format!(r"HKEY_USERS\{sid}"),
            None => "HKCU".into(),
        }
    }
}

impl Drop for UserHive {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: a key opened by `open_user_hive`, closed once.
            unsafe {
                RegCloseKey(self.key);
            }
        }
    }
}

/// `HKEY_USERS\<sid>` (read access; subkeys are opened with their own rights).
fn open_user_hive(sid: &str) -> Result<UserHive> {
    if !is_user_sid(sid) {
        return Err(err(ErrorCode::UnelevatedLaunch, format!("not a user SID: {sid:?}")));
    }
    let w = wide(sid);
    let mut h: HKEY = null_mut();
    // SAFETY: NUL-terminated name; `h` is a valid out pointer, owned by the wrapper on success.
    let rc = unsafe { RegOpenKeyExW(HKEY_USERS, w.as_ptr(), 0, KEY_READ, &mut h) };
    if rc != 0 {
        return Err(err(
            ErrorCode::UnelevatedLaunch,
            format!(r"the desktop user's registry HKEY_USERS\{sid} is not loaded (Win32 error {rc})"),
        ));
    }
    Ok(UserHive { key: h, owned: true, sid: Some(sid.to_string()) })
}

/// The registry hive of the user at the desktop. Elevated, `HKEY_CURRENT_USER` is the
/// *administrator's* hive, which differs from the logged-on user's with over-the-shoulder UAC (and a
/// thread impersonating the user does not help: the predefined `HKCU` handle is cached per process),
/// so this resolves the shell's verified unelevated token to its SID and opens `HKEY_USERS\<SID>`,
/// as the updater does for files ([`as_desktop_user`]). Not elevated: `HKEY_CURRENT_USER`. Error:
/// the desktop user cannot be determined; callers must then report "unknown", never a value read
/// from another account.
pub fn desktop_user_hive() -> Result<UserHive> {
    if !is_elevated() {
        return Ok(UserHive { key: HKEY_CURRENT_USER, owned: false, sid: None });
    }
    let token = shell_token().map_err(|e| err(ErrorCode::UnelevatedLaunch, e))?;
    let sid = token_user_sid(token.raw())
        .ok_or_else(|| err(ErrorCode::UnelevatedLaunch, "cannot read the desktop user's SID"))?;
    open_user_hive(&sid)
}

/// True when the current process token is elevated (running as administrator).
pub fn is_elevated() -> bool {
    // SAFETY: GetCurrentProcess returns a pseudo handle that needs no closing.
    let Some(token) = process_token(unsafe { GetCurrentProcess() }, TOKEN_QUERY) else { return false };
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut ret = 0u32;
    // SAFETY: `elevation` is a writable TOKEN_ELEVATION and its exact size is passed.
    let ok = unsafe {
        GetTokenInformation(
            token.raw(),
            TokenElevation,
            &mut elevation as *mut _ as *mut c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret,
        )
    };
    ok != 0 && elevation.TokenIsElevated != 0
}

#[derive(Debug)]
pub enum ElevateResult {
    Launched,
    Cancelled,
    Failed(u32),
}

/// ShellExecuteEx "runas" of this executable. `wait`: keep the process handle and return its exit code.
fn shell_execute_runas(args: &str, wait: bool) -> Result<Option<u32>, ElevateResult> {
    let exe = std::env::current_exe().map_err(|_| ElevateResult::Failed(0))?;
    let file = wide(&exe.to_string_lossy());
    let verb = wide("runas");
    let params = wide(args);
    // SAFETY: SHELLEXECUTEINFOW is plain data (all-zero is valid); every string pointer stays alive for
    // the call and is NUL-terminated.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOASYNC | if wait { SEE_MASK_NOCLOSEPROCESS } else { 0 };
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_SHOWNORMAL;
    // SAFETY: `info` is fully initialised as described above.
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        // SAFETY: trivially safe thread-local read.
        let e = unsafe { GetLastError() };
        return Err(if e == windows_sys::Win32::Foundation::ERROR_CANCELLED {
            ElevateResult::Cancelled
        } else {
            ElevateResult::Failed(e)
        });
    }
    if !wait {
        return Ok(None);
    }
    let Some(process) = OwnedHandle::new(info.hProcess) else { return Err(ElevateResult::Failed(0)) };
    let mut code = 0u32;
    // SAFETY: `process` is the valid process handle returned because of SEE_MASK_NOCLOSEPROCESS.
    unsafe {
        WaitForSingleObject(process.raw(), INFINITE);
        GetExitCodeProcess(process.raw(), &mut code);
    }
    Ok(Some(code))
}

/// Re-launches this executable elevated ("runas") with the given arguments.
pub fn relaunch_elevated(args: &str) -> ElevateResult {
    match shell_execute_runas(args, false) {
        Ok(_) => ElevateResult::Launched,
        Err(e) => e,
    }
}

/// Runs this executable elevated ("runas") with fixed arguments and waits for it to finish.
/// Returns its exit code.
pub fn run_elevated_and_wait(args: &str) -> Result<u32, ElevateResult> {
    shell_execute_runas(args, true).map(|c| c.unwrap_or(0))
}

// ---------------------------------------------------------------------------------------
// Opening folders, files and web pages from an elevated process.
//
// ShellExecute from the elevated process would start the handler elevated, and an elevated
// ShellExecute still consults HKCU\Software\Classes (user-writable) to find the handler. Instead the
// target is validated and handed to explorer.exe started with the *unelevated* shell's token, so
// whatever ends up opening it runs at the user's normal integrity level.

/// Web pages the app may open. Nothing else is accepted as a URL.
pub const ALLOWED_URLS: &[&str] = &[
    "https://pawnio.eu/",
    "https://github.com/namazso/PawnIO.Setup/releases",
    // Windows Settings › Personalization › Dynamic Lighting (rgb::dynlight::SETTINGS_URI).
    "ms-settings:personalization-lighting",
    // Driver list "Open vendor page" (crate::drivers::VENDOR_URLS).
    crate::drivers::URL_GIGABYTE_REV13_DRIVERS,
    crate::drivers::URL_GIGABYTE_REV13_BIOS,
    crate::drivers::URL_GIGABYTE_REV10_DRIVERS,
    crate::drivers::URL_GIGABYTE_REV10_BIOS,
    crate::drivers::URL_REALTEK_LAN,
    crate::drivers::URL_INTEL_ARC,
    crate::drivers::URL_AMD_B650,
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenTarget {
    Url(&'static str),
    Folder(PathBuf),
    File(PathBuf),
}

/// Syntactic check of an absolute local path (`X:\...`): no quotes, control characters, `%`, wildcards,
/// alternate data streams, `..` components, UNC or device prefixes.
fn validate_local_path(target: &str) -> Result<PathBuf, String> {
    if target.contains(':') && target.find(':') != Some(1) {
        return Err(format!("URL or stream not allowed: {target}"));
    }
    if target.chars().any(|c| c.is_control() || matches!(c, '"' | '%' | '<' | '>' | '|' | '*' | '?')) {
        return Err(format!("invalid characters in path: {target}"));
    }
    let b = target.as_bytes();
    let drive_abs = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\';
    if !drive_abs {
        return Err(format!("not an absolute local path: {target}"));
    }
    if target[2..].contains(':') || target.contains('/') {
        return Err(format!("unexpected characters in path: {target}"));
    }
    let p = PathBuf::from(target);
    if p.components().any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::CurDir)) {
        return Err(format!("relative components are not allowed: {target}"));
    }
    Ok(p)
}

/// Syntactic check: an allow-listed URL, or an absolute local path (see [`validate_local_path`]).
fn validate_open_syntax(target: &str) -> Result<Result<&'static str, PathBuf>, String> {
    if let Some(u) = ALLOWED_URLS.iter().find(|u| **u == target) {
        return Ok(Ok(*u));
    }
    validate_local_path(target).map(Err)
}

/// Full validation of something the UI wants to open: syntax, then it must exist and must not be a
/// link or other reparse point.
pub fn validate_open_target(target: &str) -> Result<OpenTarget, String> {
    let p = match validate_open_syntax(target)? {
        Ok(url) => return Ok(OpenTarget::Url(url)),
        Err(p) => p,
    };
    let meta = std::fs::symlink_metadata(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_type().is_symlink() || meta.file_attributes() & 0x400 != 0 {
            return Err(format!("{} is a link or reparse point", p.display()));
        }
    }
    Ok(if meta.is_dir() { OpenTarget::Folder(p) } else { OpenTarget::File(p) })
}

fn system_folder(windows: bool) -> Option<PathBuf> {
    let mut buf = vec![0u16; 260];
    // SAFETY: `buf` is writable for the length passed; the call returns the number of characters written.
    let n = unsafe {
        if windows {
            GetSystemWindowsDirectoryW(buf.as_mut_ptr(), buf.len() as u32)
        } else {
            GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32)
        }
    } as usize;
    (n > 0 && n < buf.len()).then(|| PathBuf::from(from_wide(&buf[..n])))
}

/// `C:\Windows\explorer.exe` (from the system, not from PATH or the environment).
pub fn explorer_exe() -> PathBuf {
    system_folder(true).unwrap_or_else(|| PathBuf::from(r"C:\Windows")).join("explorer.exe")
}

/// `C:\Windows\System32` (from the system, not from the environment).
pub fn system32() -> PathBuf {
    system_folder(false).unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32"))
}

/// Opens a validated target (see [`validate_open_target`]) through the unelevated shell. Files are
/// opened by Explorer with the user's default handler at normal integrity. Returns false (and logs
/// why) if the target was rejected or could not be launched unelevated.
pub fn shell_open(target: &str) -> bool {
    match validate_open_target(target) {
        Ok(t) => open_target(&t),
        Err(e) => {
            log::warn!("refusing to open {target:?}: {e}");
            false
        }
    }
}

/// Opens a folder in Explorer.
pub fn open_folder(dir: &Path) -> bool {
    match validate_open_target(&dir.to_string_lossy()) {
        Ok(t @ OpenTarget::Folder(_)) => open_target(&t),
        Ok(_) => false,
        Err(e) => {
            log::warn!("refusing to open folder: {e}");
            false
        }
    }
}

/// Opens one of [`ALLOWED_URLS`] in the default browser.
pub fn open_url(url: &str) -> bool {
    ALLOWED_URLS.contains(&url) && shell_open(url)
}

/// Vendor hosts whose pages the signed driver catalog may link to (download and release-notes pages).
/// The hosts of the pinned vendor pages in [`ALLOWED_URLS`] plus Intel's download mirror (release
/// notes). CI enforces the same list (`catalog/allowed-hosts.json`, checked against this one by a test).
pub const VENDOR_HOSTS: &[&str] =
    &["www.intel.com", "downloadmirror.intel.com", "www.amd.com", "www.gigabyte.com", "www.realtek.com", "pawnio.eu"];

/// Opens a vendor page named by the verified driver catalog: `https://` on a [`VENDOR_HOSTS`] host with a
/// conservative character set (`drivers::catalog::url_allowed`), through the unelevated shell like
/// [`open_url`]. Anything else is refused and logged.
pub fn open_vendor_url(url: &str) -> bool {
    if ALLOWED_URLS.contains(&url) {
        return shell_open(url);
    }
    if !crate::drivers::catalog::url_allowed(url) {
        log::warn!("refusing to open {url:?}: not an allow-listed vendor page");
        return false;
    }
    launch_explorer(&format!("\"{url}\""))
}

/// Shows a file selected in its Explorer folder (no handler runs).
#[allow(dead_code)] // UI API
pub fn reveal_file(file: &Path) -> bool {
    match validate_open_target(&file.to_string_lossy()) {
        Ok(OpenTarget::File(p)) => launch_explorer(&format!("/select,\"{}\"", p.display())),
        _ => false,
    }
}

fn open_target(t: &OpenTarget) -> bool {
    match t {
        OpenTarget::Url(u) => launch_explorer(&format!("\"{u}\"")),
        OpenTarget::Folder(p) | OpenTarget::File(p) => launch_explorer(&format!("\"{}\"", p.display())),
    }
}

/// Starts `explorer.exe <args>` unelevated. `args` is passed verbatim (Explorer parses its command line
/// itself, e.g. `/select,"path"`); callers only pass validated paths and allow-listed URLs, which
/// contain no quotes.
fn launch_explorer(args: &str) -> bool {
    let exe = explorer_exe();
    let cmd = format!("\"{}\" {args}", exe.display());
    match spawn_command_line(&exe, cmd, false, Launch::Unelevated, false) {
        Ok(_) => true,
        Err(e) => {
            log::warn!("not opening {args:?}: {e:#}");
            false
        }
    }
}

// ---------------------------------------------------------------------------------------
// Starting programs with the user's normal (unelevated) rights.

/// Quotes one argument so `CommandLineToArgvW` / the MSVC CRT parse it back unchanged.
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']) {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// `"<exe>" arg1 "arg 2" ...` for an absolute executable path (see [`validate_local_path`]).
pub fn command_line(exe: &Path, args: &[&str]) -> Result<String, String> {
    let exe_s = exe.to_string_lossy();
    validate_local_path(&exe_s)?;
    if let Some(a) = args.iter().find(|a| a.contains('\0')) {
        return Err(format!("argument contains NUL: {a:?}"));
    }
    let mut cmd = format!("\"{exe_s}\"");
    for a in args {
        cmd.push(' ');
        cmd.push_str(&quote_arg(a));
    }
    Ok(cmd)
}

/// A process started by [`spawn_unelevated_child`].
pub struct UnelevatedChild {
    process: OwnedHandle,
    pid: u32,
    /// [`spawn_unelevated_child_tied`]: kill-on-close job holding the process. Dropped after
    /// `process`, which ends the process if it still runs.
    _job: Option<OwnedHandle>,
}

impl UnelevatedChild {
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Ends the process (exit code 1) and waits up to 10 s for it to go. Returns false if it had
    /// already ended or could not be stopped; harmless either way. Blocks: not on the UI thread
    /// (use [`Self::terminate`] there).
    pub fn kill(&self) -> bool {
        let ok = self.terminate();
        // SAFETY: `process` is a valid process handle owned by self.
        unsafe { WaitForSingleObject(self.process.raw(), 10_000) };
        ok
    }

    /// Asks Windows to end the process (exit code 1) without waiting for it to go.
    pub fn terminate(&self) -> bool {
        // SAFETY: `process` is a valid process handle owned by self (full access from CreateProcess*).
        unsafe { TerminateProcess(self.process.raw(), 1) != 0 }
    }

    /// `Ok(Some(exit code))` once the process has ended, `Ok(None)` while it runs.
    pub fn try_wait(&mut self) -> std::io::Result<Option<u32>> {
        // SAFETY: `process` is a valid process handle owned by self.
        match unsafe { WaitForSingleObject(self.process.raw(), 0) } {
            WAIT_OBJECT_0 => {
                let mut code = 0u32;
                // SAFETY: as above; `code` is a valid out pointer.
                if unsafe { GetExitCodeProcess(self.process.raw(), &mut code) } == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(Some(code))
            }
            0x102 /* WAIT_TIMEOUT */ => Ok(None),
            _ => Err(std::io::Error::last_os_error()),
        }
    }
}

const SHELL_ELEVATED: &str = "the desktop shell runs as administrator";

/// Tests: true (and says so) when the desktop shell itself runs elevated (UAC off, e.g. a CI runner).
/// There is then no unelevated token to start programs with, and no desktop user's hive to find
/// through it; the caller skips its test.
#[cfg(test)]
pub(crate) fn skip_without_unelevated_shell() -> bool {
    let skip = matches!(shell_token(), Err(e) if e == SHELL_ELEVATED);
    if skip {
        eprintln!("skipped: {SHELL_ELEVATED} (UAC is off)");
    }
    skip
}

/// Tests: true when objects this process creates are owned by Administrators by default (elevated
/// under UAC). An elevated process without UAC (a CI runner) creates them owned by the user.
#[cfg(test)]
pub(crate) fn default_owner_is_admin() -> bool {
    use windows_sys::Win32::Security::{IsWellKnownSid, TOKEN_OWNER, TokenOwner, WinBuiltinAdministratorsSid};
    // SAFETY: pseudo handle, no closing needed.
    let Some(t) = process_token(unsafe { GetCurrentProcess() }, TOKEN_QUERY) else { return false };
    let mut buf = vec![0u64; 64]; // 512 bytes, 8-byte aligned: TOKEN_OWNER + SID
    let mut ret = 0u32;
    // SAFETY: `buf` is writable for the size passed and suitably aligned for TOKEN_OWNER.
    let ok = unsafe { GetTokenInformation(t.raw(), TokenOwner, buf.as_mut_ptr() as *mut c_void, 512, &mut ret) };
    // SAFETY: on success the buffer starts with a TOKEN_OWNER whose SID points into the same buffer.
    ok != 0
        && unsafe {
            let o = (*(buf.as_ptr() as *const TOKEN_OWNER)).Owner;
            !o.is_null() && IsWellKnownSid(o, WinBuiltinAdministratorsSid) != 0
        }
}

/// Decides whether the shell's token may be used to start "unelevated" programs: it must not be
/// elevated, must be at most medium integrity and must belong to our session.
fn judge_shell_token(
    elevated: Option<u32>,
    integrity: Option<u32>,
    session: Option<u32>,
    ours: Option<u32>,
) -> Result<(), String> {
    match elevated {
        Some(0) => {}
        Some(_) => return Err(SHELL_ELEVATED.into()),
        None => return Err("cannot read the shell's elevation".into()),
    }
    match integrity {
        Some(rid) if rid <= 0x2000 => {}
        Some(rid) => return Err(format!("the desktop shell runs at integrity level {rid:#x}")),
        None => return Err("cannot read the shell's integrity level".into()),
    }
    match (session, ours) {
        (Some(a), Some(b)) if a == b => Ok(()),
        (a, b) => Err(format!("the desktop shell is in another session ({a:?} vs {b:?})")),
    }
}

/// Primary token of the desktop shell (Explorer), i.e. the user's unelevated token. Refuses a shell
/// that is itself elevated, above medium integrity or in another session.
fn shell_token() -> Result<OwnedHandle, String> {
    // SAFETY: no arguments; returns a window handle or null.
    let hwnd = unsafe { GetShellWindow() };
    if hwnd.is_null() {
        return Err("no desktop shell (Explorer) is running".into());
    }
    let mut pid = 0u32;
    // SAFETY: `hwnd` came from GetShellWindow (a stale handle just yields pid 0); `pid` is a valid out pointer.
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid == 0 {
        return Err("cannot identify the shell process".into());
    }
    // SAFETY: plain call; the returned handle (or null) is owned by the wrapper.
    let proc = OwnedHandle::new(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) })
        .ok_or_else(|| format!("cannot open the shell process: {}", std::io::Error::last_os_error()))?;
    let tok = process_token(proc.raw(), TOKEN_DUPLICATE | TOKEN_QUERY)
        .ok_or_else(|| format!("cannot open the shell's token: {}", std::io::Error::last_os_error()))?;
    // SAFETY: GetCurrentProcess returns a pseudo handle that needs no closing.
    let ours =
        process_token(unsafe { GetCurrentProcess() }, TOKEN_QUERY).and_then(|t| token_u32(t.raw(), TokenSessionId));
    judge_shell_token(
        token_u32(tok.raw(), TokenElevation),
        token_integrity(tok.raw()),
        token_u32(tok.raw(), TokenSessionId),
        ours,
    )?;
    let mut primary: HANDLE = null_mut();
    let access = TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT | TOKEN_ADJUST_SESSIONID;
    // SAFETY: `tok` is a valid token handle opened with TOKEN_DUPLICATE; `primary` is a valid out pointer.
    if unsafe { DuplicateTokenEx(tok.raw(), access, null(), SecurityImpersonation, TokenPrimary, &mut primary) } == 0 {
        return Err(format!("cannot duplicate the shell's token: {}", std::io::Error::last_os_error()));
    }
    OwnedHandle::new(primary).ok_or_else(|| "DuplicateTokenEx returned no token".into())
}

/// Starts `exe` (an absolute path; no `cmd.exe`, no PATH or current-directory search) with the user's
/// normal rights and returns a handle to poll. From an elevated process the program is started with the
/// desktop shell's token (verified unelevated); if that is not possible nothing is started and the
/// error carries [`ErrorCode::UnelevatedLaunch`]. The working directory is System32 and no handles are
/// inherited. `new_console`: give a console program its own visible console window.
pub fn spawn_unelevated_child(exe: &Path, args: &[&str], new_console: bool) -> Result<UnelevatedChild> {
    let cmd = command_line(exe, args).map_err(|e| err(ErrorCode::UnelevatedLaunch, e))?;
    spawn_command_line(exe, cmd, new_console, Launch::Unelevated, false)
}

/// [`spawn_unelevated_child`] (no console) whose process cannot outlive the returned child or this
/// process: it is created suspended, put in a job object with
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` whose only handle is kept in the child (never inherited), and
/// then resumed. If the job cannot be set up the process still runs, untied (logged).
pub fn spawn_unelevated_child_tied(exe: &Path, args: &[&str]) -> Result<UnelevatedChild> {
    let cmd = command_line(exe, args).map_err(|e| err(ErrorCode::UnelevatedLaunch, e))?;
    spawn_command_line(exe, cmd, false, Launch::Unelevated, true)
}

/// A job object that kills its processes when its last handle closes, with `process` in it.
fn kill_on_close_job(process: HANDLE) -> Result<OwnedHandle, String> {
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    // SAFETY: unnamed job with default security; the handle (or null) is owned by the wrapper.
    let job = OwnedHandle::new(unsafe { CreateJobObjectW(null(), null()) })
        .ok_or_else(|| format!("CreateJobObject failed: {}", std::io::Error::last_os_error()))?;
    // SAFETY: plain data; all-zero is a valid value.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: `job` is a valid job handle; `info` is a JOBOBJECT_EXTENDED_LIMIT_INFORMATION of the size passed.
    let ok = unsafe {
        SetInformationJobObject(
            job.raw(),
            JobObjectExtendedLimitInformation,
            &info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        return Err(format!("SetInformationJobObject failed: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: both are valid handles; `process` was opened with full access by CreateProcess*.
    if unsafe { AssignProcessToJobObject(job.raw(), process) } == 0 {
        return Err(format!("AssignProcessToJobObject failed: {}", std::io::Error::last_os_error()));
    }
    Ok(job)
}

/// Starts `exe` with *this process's* token (elevated if we are), with the same path and argument rules
/// as [`spawn_unelevated_child`]. Only for executables that normal programs cannot replace (the
/// admin-only installed copy) or when this process is not elevated.
pub fn spawn_same_token(exe: &Path, args: &[&str]) -> Result<()> {
    spawn_same_token_child(exe, args).map(drop)
}

/// [`spawn_same_token`] that keeps the process handle (the update supervisor watches the new version).
pub fn spawn_same_token_child(exe: &Path, args: &[&str]) -> Result<UnelevatedChild> {
    let cmd = command_line(exe, args).map_err(|e| err(ErrorCode::RestartFailed, e))?;
    spawn_command_line(exe, cmd, false, Launch::SameToken, false)
}

/// Runs `f` on a short-lived thread that impersonates the desktop user (the shell's verified unelevated
/// token), so files it creates belong to that user and DPAPI binds secrets to that user's keys, also
/// with over-the-shoulder UAC. Not elevated: runs `f` directly.
pub fn as_desktop_user<R: Send + 'static>(f: impl FnOnce() -> Result<R> + Send + 'static) -> Result<R> {
    if !is_elevated() {
        return f();
    }
    let token = shell_token().map_err(|e| err(ErrorCode::UnelevatedLaunch, e))?;
    let raw = token.raw() as usize;
    std::thread::scope(|s| {
        s.spawn(move || {
            use windows_sys::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};
            // SAFETY: `raw` is the shell's primary token (TOKEN_QUERY|TOKEN_DUPLICATE), alive until the
            // scope ends; impersonation only affects this thread, which reverts before it ends.
            if unsafe { ImpersonateLoggedOnUser(raw as HANDLE) } == 0 {
                return Err(err(
                    ErrorCode::UnelevatedLaunch,
                    format!("cannot act as the desktop user: {}", std::io::Error::last_os_error()),
                ));
            }
            let r = f();
            // SAFETY: plain call on this (impersonating) thread. Should it ever fail, the impersonation
            // still ends with this thread, which returns right away.
            if unsafe { RevertToSelf() } == 0 {
                log::error!("RevertToSelf failed: {}", std::io::Error::last_os_error());
            }
            r
        })
        .join()
        .unwrap_or_else(|_| Err(err(ErrorCode::Internal, "desktop-user helper thread panicked")))
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Launch {
    Unelevated,
    SameToken,
}

/// See [`spawn_unelevated_child`]; `cmd` is the complete command line starting with the quoted `exe`.
/// `tied`: see [`spawn_unelevated_child_tied`].
fn spawn_command_line(
    exe: &Path,
    cmd: String,
    new_console: bool,
    launch: Launch,
    tied: bool,
) -> Result<UnelevatedChild> {
    validate_local_path(&exe.to_string_lossy()).map_err(|e| err(ErrorCode::UnelevatedLaunch, e))?;
    let app = wide(&exe.to_string_lossy());
    let mut cmd = wide(&cmd);
    let cwd = wide(&system32().to_string_lossy());
    let flags = if new_console { CREATE_NEW_CONSOLE } else { 0 } | if tied { CREATE_SUSPENDED } else { 0 };
    // SAFETY: STARTUPINFOW/PROCESS_INFORMATION are plain data; all-zero is a valid initial state.
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    // SAFETY: as above.
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = if launch == Launch::Unelevated && is_elevated() {
        let token = shell_token().map_err(|e| err(ErrorCode::UnelevatedLaunch, e))?;
        // SAFETY: `token` is a primary token with TOKEN_QUERY|DUPLICATE|ASSIGN_PRIMARY; `app`/`cwd` are
        // NUL-terminated; `cmd` is a writable NUL-terminated buffer (the API may modify it); `si`/`pi` are
        // valid. A null environment makes the system build one from the token's user profile.
        unsafe {
            CreateProcessWithTokenW(
                token.raw(),
                0,
                app.as_ptr(),
                cmd.as_mut_ptr(),
                flags,
                null(),
                cwd.as_ptr(),
                &si,
                &mut pi,
            )
        }
    } else {
        // SAFETY: same buffers as above; bInheritHandles = FALSE.
        unsafe {
            CreateProcessW(app.as_ptr(), cmd.as_mut_ptr(), null(), null(), 0, flags, null(), cwd.as_ptr(), &si, &mut pi)
        }
    };
    if ok == 0 {
        return Err(err(
            ErrorCode::UnelevatedLaunch,
            format!("starting {} failed: {}", exe.display(), std::io::Error::last_os_error()),
        ));
    }
    let thread = OwnedHandle::new(pi.hThread);
    let process = OwnedHandle::new(pi.hProcess).ok_or_else(|| err(ErrorCode::UnelevatedLaunch, "no process handle"))?;
    let mut job = None;
    if tied {
        match kill_on_close_job(process.raw()) {
            Ok(j) => job = Some(j),
            Err(e) => log::warn!("{} runs without a kill-on-close job: {e}", exe.display()),
        }
        // SAFETY: the primary thread handle of the process created suspended above.
        if thread.as_ref().is_none_or(|t| unsafe { ResumeThread(t.raw()) } == u32::MAX) {
            // SAFETY: valid process handle; a process that cannot start must not stay suspended.
            unsafe { TerminateProcess(process.raw(), 1) };
            return Err(err(ErrorCode::UnelevatedLaunch, format!("could not start {}", exe.display())));
        }
    }
    drop(thread);
    Ok(UnelevatedChild { process, pid: pi.dwProcessId, _job: job })
}

/// `%LOCALAPPDATA%` of the desktop user (the shell's token when elevated: with over-the-shoulder UAC the
/// administrator account differs from the logged-on user).
pub fn desktop_user_local_app_data() -> Result<PathBuf> {
    use windows_sys::Win32::UI::Shell::FOLDERID_LocalAppData;
    if !is_elevated() {
        return crate::util::paths::known_folder(&FOLDERID_LocalAppData)
            .ok_or_else(|| err(ErrorCode::UnelevatedLaunch, "LocalAppData is not set"));
    }
    let token = shell_token().map_err(|e| err(ErrorCode::UnelevatedLaunch, e))?;
    crate::util::paths::known_folder_for(&FOLDERID_LocalAppData, token.raw())
        .ok_or_else(|| err(ErrorCode::UnelevatedLaunch, "the desktop user's LocalAppData is not set"))
}

/// [`spawn_unelevated_child`] without a console and without keeping the process handle.
pub fn spawn_unelevated(exe: &Path, args: &[&str]) -> Result<()> {
    spawn_unelevated_child(exe, args, false).map(drop)
}

// ---------------------------------------------------------------------------------------
// App Execution Aliases (`%LOCALAPPDATA%\Microsoft\WindowsApps\*.exe`)

/// Parsed `IO_REPARSE_TAG_APPEXECLINK` data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppExecLink {
    pub package_family: String,
    pub app_user_model_id: String,
    pub target: PathBuf,
}

/// Parses the reparse data (after the 8-byte REPARSE_DATA_BUFFER header): version 3, then
/// NUL-terminated UTF-16 strings: package family, AUMID, target exe, app type.
pub fn parse_app_exec_link(data: &[u8]) -> Option<AppExecLink> {
    if data.len() < 4 || u32::from_le_bytes([data[0], data[1], data[2], data[3]]) != 3 {
        return None;
    }
    let units: Vec<u16> = data[4..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let mut parts = units.split(|&u| u == 0).map(String::from_utf16_lossy);
    let package_family = parts.next()?;
    let app_user_model_id = parts.next()?;
    let target = PathBuf::from(parts.next()?);
    (!package_family.is_empty() && !app_user_model_id.is_empty() && target.is_absolute()).then_some(AppExecLink {
        package_family,
        app_user_model_id,
        target,
    })
}

/// Reads the App Execution Alias at `path` without following it. Fails if it is anything else.
pub fn read_app_exec_link(path: &Path) -> std::io::Result<AppExecLink> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    const IO_REPARSE_TAG_APPEXECLINK: u32 = 0x8000_001B;
    const FSCTL_GET_REPARSE_POINT: u32 = 0x0009_00A8;
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: {m}", path.display()));
    let f = std::fs::OpenOptions::new()
        .access_mode(0x0080) // FILE_READ_ATTRIBUTES
        .share_mode(0x7)
        .custom_flags(0x0020_0000) // FILE_FLAG_OPEN_REPARSE_POINT
        .open(path)?;
    let mut buf = vec![0u8; 16 * 1024];
    let mut n = 0u32;
    // SAFETY: `f` is an open file handle; `buf` is writable for the length passed; no input buffer.
    let ok = unsafe {
        DeviceIoControl(
            f.as_raw_handle() as HANDLE,
            FSCTL_GET_REPARSE_POINT,
            null(),
            0,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            &mut n,
            null_mut(),
        )
    };
    if ok == 0 {
        return Err(bad(&format!("not an app execution alias ({})", std::io::Error::last_os_error())));
    }
    let data = &buf[..(n as usize).min(buf.len())];
    if data.len() < 8 || u32::from_le_bytes([data[0], data[1], data[2], data[3]]) != IO_REPARSE_TAG_APPEXECLINK {
        return Err(bad("not an app execution alias"));
    }
    let len = u16::from_le_bytes([data[4], data[5]]) as usize;
    parse_app_exec_link(data.get(8..8 + len).ok_or_else(|| bad("truncated reparse data"))?)
        .ok_or_else(|| bad("malformed app execution alias"))
}

/// Lower-cased executable names of all running processes.
pub fn running_processes() -> Vec<String> {
    let mut out = Vec::new();
    // SAFETY: snapshot handle is owned by the wrapper; PROCESSENTRY32W is plain data with dwSize set as
    // the API requires; the iteration stops when Process32NextW fails.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        let Some(snap) = OwnedHandle::new(snap) else { return out };
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap.raw(), &mut entry) != 0 {
            loop {
                out.push(from_wide(&entry.szExeFile).to_lowercase());
                if Process32NextW(snap.raw(), &mut entry) == 0 {
                    break;
                }
            }
        }
    }
    out
}

/// Full paths of the other running processes whose executable name is one of `names` (lower case).
/// PROCESS_QUERY_LIMITED_INFORMATION is granted on an elevated process too, so an unelevated launcher
/// can see where a running elevated copy was started from.
pub fn other_process_paths(names: &[&str]) -> Vec<PathBuf> {
    let me = std::process::id();
    let mut out: Vec<PathBuf> = Vec::new();
    // SAFETY: as in `running_processes`; each process handle is owned by OwnedHandle, `buf` is writable
    // for the length passed in `len`, which the call updates to the characters written.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        let Some(snap) = OwnedHandle::new(snap) else { return out };
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snap.raw(), &mut entry) != 0;
        while more {
            let name = from_wide(&entry.szExeFile).to_lowercase();
            if entry.th32ProcessID != me
                && names.contains(&name.as_str())
                && let Some(h) =
                    OwnedHandle::new(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, entry.th32ProcessID))
            {
                let mut buf = [0u16; 1024];
                let mut len = buf.len() as u32;
                if QueryFullProcessImageNameW(h.raw(), PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0 {
                    let p = PathBuf::from(String::from_utf16_lossy(&buf[..len as usize]));
                    if !out.contains(&p) {
                        out.push(p);
                    }
                }
            }
            more = Process32NextW(snap.raw(), &mut entry) != 0;
        }
    }
    out
}

/// Milliseconds since boot.
pub fn uptime_ms() -> u64 {
    // SAFETY: no arguments, no side effects.
    unsafe { GetTickCount64() }
}

/// Approximate boot time as UNIX seconds (stable to a few seconds across calls).
pub fn boot_time_unix() -> u64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    now.saturating_sub(uptime_ms() / 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_target_validation() {
        assert_eq!(validate_open_target("https://pawnio.eu/"), Ok(OpenTarget::Url("https://pawnio.eu/")));
        assert_eq!(
            validate_open_target(crate::rgb::dynlight::SETTINGS_URI),
            Ok(OpenTarget::Url("ms-settings:personalization-lighting"))
        );
        for bad in [
            "https://pawnio.eu.evil.example/",
            "http://pawnio.eu/",
            "file:///C:/Windows/System32/calc.exe",
            "ms-settings:privacy",
            "calc.exe",
            r"..\..\Windows",
            r"\\server\share\x",
            r"\\?\C:\Windows",
            r"\\.\PhysicalDrive0",
            r"C:\Windows\..\Windows",
            r"C:\Windows\win.ini:stream",
            "C:\\Windows\" /c calc",
            r"%SystemRoot%\System32",
            r"C:\Windows*",
            "C:\\Windows\n",
            "",
        ] {
            assert!(validate_open_target(bad).is_err(), "accepted {bad:?}");
        }
        let win = explorer_exe().parent().unwrap().to_path_buf();
        assert_eq!(validate_open_target(&win.to_string_lossy()), Ok(OpenTarget::Folder(win.clone())));
        let exe = explorer_exe();
        assert_eq!(validate_open_target(&exe.to_string_lossy()), Ok(OpenTarget::File(exe.clone())));
        assert!(validate_open_target(r"C:\definitely\not\here").is_err());
    }

    #[test]
    fn explorer_is_in_windows_dir() {
        let e = explorer_exe();
        assert!(e.is_absolute() && e.ends_with("explorer.exe"));
        assert!(system32().join("kernel32.dll").exists());
    }

    /// Splits a command line the way the MSVC CRT / CommandLineToArgvW do (arguments after argv[0]).
    fn crt_split(s: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut chars = s.chars().peekable();
        loop {
            while chars.peek().is_some_and(|c| *c == ' ' || *c == '\t') {
                chars.next();
            }
            if chars.peek().is_none() {
                return out;
            }
            let (mut cur, mut quoted, mut bs) = (String::new(), false, 0usize);
            while let Some(&c) = chars.peek() {
                if c == '\\' {
                    bs += 1;
                    chars.next();
                    continue;
                }
                if c == '"' {
                    cur.extend(std::iter::repeat_n('\\', bs / 2));
                    chars.next();
                    if bs % 2 == 1 {
                        cur.push('"');
                    } else {
                        quoted = !quoted;
                    }
                    bs = 0;
                    continue;
                }
                cur.extend(std::iter::repeat_n('\\', bs));
                bs = 0;
                if !quoted && (c == ' ' || c == '\t') {
                    break;
                }
                cur.push(c);
                chars.next();
            }
            cur.extend(std::iter::repeat_n('\\', bs));
            out.push(cur);
        }
    }

    #[test]
    fn arguments_are_quoted_losslessly() {
        let args = [
            "install",
            "--id",
            "namazso.PawnIO",
            "",
            "two words",
            r#"quote"inside"#,
            r"C:\path with space\",
            r"trailing\\",
            r#"\"already\""#,
            "& calc.exe",
            "%PATH%",
        ];
        let cmd = command_line(&explorer_exe(), &args).unwrap();
        assert!(cmd.starts_with(&format!("\"{}\" ", explorer_exe().display())));
        let rest = &cmd[explorer_exe().to_string_lossy().len() + 3..];
        assert_eq!(crt_split(rest), args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg(""), "\"\"");
    }

    #[test]
    fn command_line_requires_an_absolute_exe() {
        for bad in ["winget", "winget.exe", r".\x.exe", r"..\x.exe", r"\\server\share\x.exe", "C:\\a\"b.exe"] {
            assert!(command_line(Path::new(bad), &[]).is_err(), "accepted {bad}");
        }
        assert!(command_line(&explorer_exe(), &["a\0b"]).is_err());
    }

    #[test]
    fn elevated_or_foreign_shell_tokens_are_refused() {
        assert!(judge_shell_token(Some(0), Some(0x2000), Some(1), Some(1)).is_ok());
        assert!(judge_shell_token(Some(1), Some(0x2000), Some(1), Some(1)).is_err());
        assert!(judge_shell_token(Some(0), Some(0x3000), Some(1), Some(1)).is_err());
        assert!(judge_shell_token(Some(0), Some(0x2000), Some(2), Some(1)).is_err());
        assert!(judge_shell_token(None, Some(0x2000), Some(1), Some(1)).is_err());
        assert!(judge_shell_token(Some(0), None, Some(1), Some(1)).is_err());
        assert!(judge_shell_token(Some(0), Some(0x2000), None, None).is_err());
    }

    #[test]
    fn current_token_queries_work() {
        // SAFETY: pseudo handle, no closing needed.
        let t = process_token(unsafe { GetCurrentProcess() }, TOKEN_QUERY).unwrap();
        let rid = token_integrity(t.raw()).unwrap();
        assert!((0x1000..=0x4000).contains(&rid), "{rid:#x}");
        assert_eq!(rid >= 0x3000, is_elevated());
        assert!(token_u32(t.raw(), TokenSessionId).is_some());
    }

    /// Review M4: the user's hive is found by SID. Opening `HKEY_USERS\<our SID>` shows the same keys
    /// as our own `HKCU` (the path an elevated FanRGB takes with the shell's SID).
    #[test]
    fn user_hive_by_sid_matches_hkcu() {
        // SAFETY: pseudo handle, no closing needed.
        let t = process_token(unsafe { GetCurrentProcess() }, TOKEN_QUERY).unwrap();
        let sid = token_user_sid(t.raw()).unwrap();
        assert!(sid.starts_with("S-1-5-"), "{sid}");
        let hive = open_user_hive(&sid).unwrap();
        assert!(hive.describe().ends_with(&sid));
        let mine = reg_subkeys(HKEY_CURRENT_USER, "Software", 512);
        assert!(!mine.is_empty());
        assert_eq!(reg_subkeys(hive.root(), "Software", 512), mine);
        // Not elevated (tests): the desktop user is us.
        if !is_elevated() {
            assert_eq!(desktop_user_hive().unwrap().describe(), "HKCU");
        }
        for bad in ["", "S-1-", "S-1-5-21-1-", r"S-1-5\..\x", "S-1-5-21-12345678901", "s-1-5-18", ".DEFAULT"] {
            assert!(!is_user_sid(bad), "{bad}");
            assert!(open_user_hive(bad).is_err(), "{bad}");
        }
        assert!(is_user_sid("S-1-5-21-1004336348-1177238915-682003330-1001"));
    }

    #[test]
    fn same_token_child_can_be_killed() {
        // ping.exe -n 60: harmless, runs about a minute unless killed (the update supervisor kills a
        // new version that never reports healthy).
        let mut c = spawn_same_token_child(&system32().join("PING.EXE"), &["-n", "60", "127.0.0.1"]).unwrap();
        assert_eq!(c.try_wait().unwrap(), None);
        c.kill();
        assert_eq!(c.try_wait().unwrap(), Some(1));
        c.kill(); // already gone: harmless
    }

    /// Review L25: a tied child dies with its handle (and so with FanRGB), wherever it is in its run.
    #[test]
    fn tied_child_ends_when_dropped() {
        if skip_without_unelevated_shell() {
            return;
        }
        use windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE;
        let mut c = spawn_unelevated_child_tied(&system32().join("PING.EXE"), &["-n", "60", "127.0.0.1"]).unwrap();
        assert_eq!(c.try_wait().unwrap(), None, "resumed and running");
        // SAFETY: plain call; the handle (or null) is owned by the wrapper.
        let watch = OwnedHandle::new(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, c.id()) }).unwrap();
        drop(c);
        // SAFETY: valid process handle owned by `watch`.
        assert_eq!(unsafe { WaitForSingleObject(watch.raw(), 10_000) }, WAIT_OBJECT_0, "still running");
        // Exits normally when left alone.
        let mut c = spawn_unelevated_child_tied(&system32().join("hostname.exe"), &[]).unwrap();
        let t = std::time::Instant::now();
        while c.try_wait().unwrap().is_none() {
            assert!(t.elapsed() < std::time::Duration::from_secs(10), "hostname.exe did not exit");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(c.try_wait().unwrap(), Some(0));
    }

    #[test]
    fn desktop_user_closure_runs() {
        if skip_without_unelevated_shell() {
            return;
        }
        // Not elevated: runs directly. Elevated: on a thread impersonating the shell user, which is not
        // elevated, so the closure sees a non-elevated thread token.
        assert_eq!(as_desktop_user(|| Ok(41 + 1)).unwrap(), 42);
        assert!(as_desktop_user::<()>(|| Err(err(ErrorCode::Io, "x"))).is_err());
    }

    #[test]
    fn unelevated_spawn_runs_and_reports_exit_code() {
        if skip_without_unelevated_shell() {
            return;
        }
        // hostname.exe: harmless, exits 0 at once. Not elevated: CreateProcessW with our token; elevated:
        // the desktop shell's token.
        let mut c = spawn_unelevated_child(&system32().join("hostname.exe"), &[], false).unwrap();
        assert!(c.id() != 0);
        let t = std::time::Instant::now();
        let code = loop {
            if let Some(code) = c.try_wait().unwrap() {
                break code;
            }
            assert!(t.elapsed() < std::time::Duration::from_secs(10), "hostname.exe did not exit");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert_eq!(code, 0);
        // Relative names are never searched for.
        assert!(spawn_unelevated(Path::new("hostname.exe"), &[]).is_err());
    }

    #[test]
    fn app_exec_link_parsing() {
        let mut data = 3u32.to_le_bytes().to_vec();
        for s in [
            "Microsoft.DesktopAppInstaller_8wekyb3d8bbwe",
            "Microsoft.DesktopAppInstaller_8wekyb3d8bbwe!winget",
            r"C:\Program Files\WindowsApps\Microsoft.DesktopAppInstaller_1.29.379.0_x64__8wekyb3d8bbwe\winget.exe",
            "0",
        ] {
            data.extend(s.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes));
        }
        let l = parse_app_exec_link(&data).unwrap();
        assert_eq!(l.package_family, "Microsoft.DesktopAppInstaller_8wekyb3d8bbwe");
        assert!(l.target.ends_with("winget.exe"));
        let mut v2 = data.clone();
        v2[0] = 2;
        assert!(parse_app_exec_link(&v2).is_none());
        assert!(parse_app_exec_link(&data[..10]).is_none());
        // A regular file is not an alias.
        assert!(read_app_exec_link(&explorer_exe()).is_err());
    }
}
