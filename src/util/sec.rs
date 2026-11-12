//! Process hardening and Windows security-descriptor helpers.
//!
//! FanRGB runs elevated and holds a kernel I/O driver, so everything a medium-integrity process of the
//! same user can influence (DLL search path, junctions, files and named objects) is treated as hostile.

use std::ffi::c_void;
use std::io;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, GetLastError, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_FILE_OBJECT, SE_KERNEL_OBJECT, SE_OBJECT_TYPE,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, AdjustTokenPrivileges, DACL_SECURITY_INFORMATION, GetAce,
    GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, GetTokenInformation, IsWellKnownSid, LUID_AND_ATTRIBUTES,
    LookupPrivilegeValueW, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_PRIVILEGE_ENABLED,
    SECURITY_ATTRIBUTES, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    WinBuiltinAdministratorsSid, WinCreatorOwnerSid, WinLocalSystemSid,
};
use windows_sys::Win32::System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessMitigationPolicy, OpenProcessToken, ProcessImageLoadPolicy,
    ProcessRedirectionTrustPolicy, SetProcessMitigationPolicy,
};

use super::win::{OwnedHandle, from_wide, wide};

/// Directory DACL for elevated state (`%ProgramData%\FanRGB`): owner Administrators, full control for
/// SYSTEM and Administrators only, read-only for Users (so the unelevated Explorer/editor launched by
/// "Data & logs folder" can show logs and diagnostics). Protected: nothing is inherited from ProgramData,
/// whose default ACL lets every user create files and folders.
pub const SDDL_STATE_DIR: &str = "O:BAG:SYD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)";

/// Named kernel objects only FanRGB (elevated) may use.
pub const SDDL_PRIVATE_OBJECT: &str = "O:BAD:P(A;;GA;;;SY)(A;;GA;;;BA)";
/// Public single-instance marker: elevated FanRGB owns it, anyone may test for its existence.
pub const SDDL_PUBLIC_MUTEX: &str = "O:BAD:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x120000;;;AU)"; // SYNCHRONIZE|READ_CONTROL
/// Public "show your window" event: anyone may signal it (EVENT_MODIFY_STATE|READ_CONTROL), but not wait
/// on it (no SYNCHRONIZE), so another program cannot swallow the auto-reset signal.
pub const SDDL_PUBLIC_EVENT: &str = "O:BAD:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x20002;;;AU)";

const TRUSTED_INSTALLER_SID: &str = "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";

/// Access bits that let a principal change a file or directory, its contents, its ACL or its owner.
const WRITE_LIKE: u32 = 0x1000_0000 // GENERIC_ALL
    | 0x4000_0000 // GENERIC_WRITE
    | 0x0001_0000 // DELETE
    | 0x0004_0000 // WRITE_DAC
    | 0x0008_0000 // WRITE_OWNER
    | 0x0000_0002 // FILE_WRITE_DATA / FILE_ADD_FILE
    | 0x0000_0004 // FILE_APPEND_DATA / FILE_ADD_SUBDIRECTORY
    | 0x0000_0010 // FILE_WRITE_EA
    | 0x0000_0040 // FILE_DELETE_CHILD
    | 0x0000_0100; // FILE_WRITE_ATTRIBUTES

const INHERIT_ONLY_ACE: u8 = 0x08;

// ---------------------------------------------------------------------------------------
// Process mitigations

/// Applied as the very first thing in `main`:
/// - DLLs loaded at run time by name come from System32 only (never the exe folder, the current
///   directory or PATH). hidapi (`hid.dll`), glutin (`opengl32.dll`), winit (`dwmapi`/`uxtheme`) and cpal
///   (WASAPI via COM, registered by full path) only need System32; OpenGL ICDs are loaded by full path.
/// - Image loads prefer System32 and refuse remote / low-integrity images.
/// - Redirection Guard (Windows 11 22H2+): junctions created by non-admin users are not followed, which
///   defuses the junction/object-manager-symlink tricks against the files the elevated process writes.
///
/// Failures are recorded: [`hardening_problems`] lists them for the log and the UI.
pub fn harden_process() {
    let mut problems = Vec::new();
    // SAFETY: plain flag argument.
    if unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) } == 0 {
        problems.push(format!("SetDefaultDllDirectories failed: {}", io::Error::last_os_error()));
    }
    // PROCESS_MITIGATION_IMAGE_LOAD_POLICY: NoRemoteImages | NoLowMandatoryLabelImages | PreferSystem32Images
    let image: u32 = 0b111;
    // SAFETY: `image` is a 4-byte policy structure (a bit field DWORD) and its size is passed.
    if unsafe { SetProcessMitigationPolicy(ProcessImageLoadPolicy, &image as *const u32 as *const c_void, 4) } == 0 {
        problems.push(format!("image load policy not applied: {}", io::Error::last_os_error()));
    }
    // PROCESS_MITIGATION_REDIRECTION_TRUST_POLICY: EnforceRedirectionTrust (Windows 11 22H2+).
    let redirection: u32 = 0b1;
    // SAFETY: as above.
    let set = unsafe {
        SetProcessMitigationPolicy(ProcessRedirectionTrustPolicy, &redirection as *const u32 as *const c_void, 4)
    };
    let set_err = io::Error::last_os_error();
    if !redirection_guard_active() {
        problems.push(if set == 0 {
            format!("Redirection Guard unavailable ({set_err}); junction protection relies on FanRGB's own checks")
        } else {
            "Redirection Guard did not stay enabled".to_string()
        });
    }
    let _ = HARDENING.set(problems);
}

static HARDENING: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// What [`harden_process`] could not enable (empty when everything is active).
pub fn hardening_problems() -> Vec<String> {
    HARDENING.get().cloned().unwrap_or_default()
}

/// Reads the Redirection Guard policy back from the process.
pub fn redirection_guard_active() -> bool {
    let mut v: u32 = 0;
    // SAFETY: `v` is a writable 4-byte buffer and its size is passed; the pseudo handle needs no closing.
    let ok = unsafe {
        GetProcessMitigationPolicy(
            GetCurrentProcess(),
            ProcessRedirectionTrustPolicy,
            &mut v as *mut u32 as *mut c_void,
            4,
        )
    };
    ok != 0 && v & 0b1 != 0
}

/// Enables a privilege (e.g. `SeRestorePrivilege`) in the process token. Returns false if the token
/// does not hold it (not elevated).
pub fn enable_privilege(name: &str) -> bool {
    // SAFETY: `token` is a valid out pointer and owned by OwnedHandle afterwards; `n` is NUL-terminated; TOKEN_PRIVILEGES holds exactly the one entry declared in PrivilegeCount.
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let token = OwnedHandle(token);
        let n = wide(name);
        let mut tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES { Luid: std::mem::zeroed(), Attributes: SE_PRIVILEGE_ENABLED }],
        };
        if LookupPrivilegeValueW(null(), n.as_ptr(), &mut tp.Privileges[0].Luid) == 0 {
            return false;
        }
        if AdjustTokenPrivileges(token.raw(), 0, &tp, 0, null_mut(), null_mut()) == 0 {
            return false;
        }
        GetLastError() == ERROR_SUCCESS
    }
}

/// SID string (`S-1-5-21-...`) of the user the process runs as.
pub fn current_user_sid() -> Option<String> {
    // SAFETY: `token` is owned by OwnedHandle; `buf` is 512 writable, 8-byte aligned bytes (size passed), and on success starts with a TOKEN_USER whose SID points into `buf`, which outlives its use.
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let token = OwnedHandle(token);
        let mut buf = vec![0u64; 64]; // 512 bytes, 8-byte aligned
        let mut len = 0u32;
        if GetTokenInformation(token.raw(), TokenUser, buf.as_mut_ptr() as *mut c_void, 512, &mut len) == 0 {
            return None;
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        sid_to_string(user.User.Sid)
    }
}

fn sid_to_string(sid: PSID) -> Option<String> {
    // SAFETY: `sid` is a valid SID supplied by the caller; the returned string is NUL-terminated, read up to the terminator and freed once with LocalFree.
    unsafe {
        let mut s: *mut u16 = null_mut();
        if ConvertSidToStringSidW(sid, &mut s) == 0 || s.is_null() {
            return None;
        }
        let mut len = 0;
        while *s.add(len) != 0 {
            len += 1;
        }
        let out = from_wide(std::slice::from_raw_parts(s, len));
        LocalFree(s as *mut c_void);
        Some(out)
    }
}

// ---------------------------------------------------------------------------------------
// Security descriptors

/// A security descriptor allocated by the system (freed with `LocalFree`).
pub struct OwnedSd(PSECURITY_DESCRIPTOR);

impl OwnedSd {
    pub fn from_sddl(sddl: &str) -> io::Result<Self> {
        let s = wide(sddl);
        let mut psd: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: `s` is NUL-terminated; `psd` is a valid out pointer whose allocation is owned by OwnedSd.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(s.as_ptr(), SDDL_REVISION_1, &mut psd, null_mut())
        };
        if ok == 0 || psd.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedSd(psd))
    }

    /// Owner and DACL of an open handle (needs `READ_CONTROL`).
    pub fn of_handle(h: HANDLE, kind: SE_OBJECT_TYPE) -> io::Result<Self> {
        let mut psd: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: `h` is an open handle with READ_CONTROL (caller contract); only `psd` is requested, and it is owned by OwnedSd; the owner/DACL pointers point into it.
        let rc = unsafe {
            GetSecurityInfo(
                h,
                kind,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                &mut psd,
            )
        };
        if rc != 0 || psd.is_null() {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        Ok(OwnedSd(psd))
    }

    pub fn of_file(h: HANDLE) -> io::Result<Self> {
        Self::of_handle(h, SE_FILE_OBJECT)
    }

    pub fn of_kernel_object(h: HANDLE) -> io::Result<Self> {
        Self::of_handle(h, SE_KERNEL_OBJECT)
    }

    #[allow(dead_code)]
    pub fn raw(&self) -> PSECURITY_DESCRIPTOR {
        self.0
    }

    /// `SECURITY_ATTRIBUTES` pointing at this descriptor (valid while `self` lives).
    pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }

    pub fn owner(&self) -> PSID {
        let mut owner: PSID = null_mut();
        let mut defaulted = 0;
        // SAFETY: `self.0` is a valid descriptor for the lifetime of self; both out pointers are valid.
        unsafe { GetSecurityDescriptorOwner(self.0, &mut owner, &mut defaulted) };
        owner
    }

    /// The DACL, or null if the descriptor has none (= everyone has full access).
    pub fn dacl(&self) -> *mut ACL {
        let mut present = 0;
        let mut dacl: *mut ACL = null_mut();
        let mut defaulted = 0;
        // SAFETY: as for `owner`: valid descriptor, valid out pointers.
        let ok = unsafe { GetSecurityDescriptorDacl(self.0, &mut present, &mut dacl, &mut defaulted) };
        if ok == 0 || present == 0 { null_mut() } else { dacl }
    }

    /// True if the owner is SYSTEM, Administrators or TrustedInstaller.
    pub fn owner_is_admin(&self) -> bool {
        let o = self.owner();
        !o.is_null() && is_admin_sid(o)
    }

    /// Whether only SYSTEM / Administrators / TrustedInstaller can modify the object.
    pub fn trust(&self) -> Trust {
        evaluate(self)
    }
}

impl Drop for OwnedSd {
    fn drop(&mut self) {
        // SAFETY: `self.0` was allocated by the system with LocalAlloc semantics and is freed exactly once here.
        unsafe {
            LocalFree(self.0);
        }
    }
}

/// Result of checking who can modify an object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trust {
    /// Only SYSTEM, Administrators and TrustedInstaller can modify it.
    Trusted,
    /// Owned by someone else (an owner can always rewrite the DACL).
    UntrustedOwner(String),
    /// Missing (null) DACL: everyone has full access.
    NoDacl,
    /// An ACE grants write-like access to another principal.
    WeakAce(String),
}

impl Trust {
    #[allow(dead_code)]
    pub fn is_trusted(&self) -> bool {
        *self == Trust::Trusted
    }
}

fn is_admin_sid(sid: PSID) -> bool {
    // SAFETY: `sid` points to a valid SID (callers pass owner/ACE SIDs inside a live descriptor).
    unsafe {
        IsWellKnownSid(sid, WinLocalSystemSid) != 0
            || IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0
            || sid_to_string(sid).as_deref() == Some(TRUSTED_INSTALLER_SID)
    }
}

fn evaluate(sd: &OwnedSd) -> Trust {
    let owner = sd.owner();
    if owner.is_null() || !is_admin_sid(owner) {
        let name = if owner.is_null() { "none".into() } else { sid_to_string(owner).unwrap_or_default() };
        return Trust::UntrustedOwner(name);
    }
    let dacl = sd.dacl();
    if dacl.is_null() {
        return Trust::NoDacl;
    }
    // SAFETY: `dacl` is non-null and points into the descriptor owned by `sd`, which outlives this loop.
    let count = unsafe { (*dacl).AceCount };
    for i in 0..count as u32 {
        let mut pace: *mut c_void = null_mut();
        // SAFETY: `dacl` is a valid ACL and `i` < AceCount; `pace` is a valid out pointer.
        if unsafe { GetAce(dacl, i, &mut pace) } == 0 || pace.is_null() {
            return Trust::WeakAce(format!("unreadable ACE #{i}"));
        }
        // SAFETY: GetAce returned a pointer to an ACE inside the live ACL; every ACE starts with ACE_HEADER.
        let header = unsafe { &*(pace as *const ACE_HEADER) };
        // Every ACE type has the access mask right after the header.
        // SAFETY: every ACE layout used here has the 32-bit mask right after the header, inside the ACE.
        let mask = unsafe { (*(pace as *const ACCESS_ALLOWED_ACE)).Mask };
        match header.AceType {
            // Deny ACEs only take rights away.
            0x01 | 0x06 | 0x0A | 0x0C => continue,
            _ => {}
        }
        if mask & WRITE_LIKE == 0 {
            continue;
        }
        // ACCESS_ALLOWED (0x00) and ACCESS_ALLOWED_CALLBACK (0x09) have the SID right after the mask;
        // anything else (object ACEs, unknown types) granting write access is not expected here.
        if header.AceType != 0x00 && header.AceType != 0x09 {
            return Trust::WeakAce(format!("ACE type {:#04x}", header.AceType));
        }
        // SAFETY: for ACCESS_ALLOWED(_CALLBACK) ACEs (checked above) the SID starts at SidStart, inside the ACE.
        let sid = unsafe { &(*(pace as *const ACCESS_ALLOWED_ACE)).SidStart as *const u32 as PSID };
        if is_admin_sid(sid) {
            continue;
        }
        // CREATOR OWNER on an inherit-only ACE only applies to children, and only principals that can
        // already write here can create children (Program Files uses this).
        let inherit_only = header.AceFlags & INHERIT_ONLY_ACE != 0;
        // SAFETY: `sid` points to the ACE's SID inside the live ACL.
        if inherit_only && unsafe { IsWellKnownSid(sid, WinCreatorOwnerSid) } != 0 {
            continue;
        }
        return Trust::WeakAce(sid_to_string(sid).unwrap_or_else(|| "?".into()));
    }
    Trust::Trusted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trust(sddl: &str) -> Trust {
        OwnedSd::from_sddl(sddl).unwrap().trust()
    }

    #[test]
    fn state_dir_descriptor_is_trusted() {
        assert_eq!(trust(SDDL_STATE_DIR), Trust::Trusted);
        assert_eq!(trust(SDDL_PRIVATE_OBJECT), Trust::Trusted);
        // Read/synchronize-only grants for users do not weaken the object.
        assert_eq!(trust(SDDL_PUBLIC_MUTEX), Trust::Trusted);
        // (The public event grants EVENT_MODIFY_STATE = 0x2, which the file-oriented check would read as
        // FILE_ADD_FILE; it is only parsed here.)
        assert!(OwnedSd::from_sddl(SDDL_PUBLIC_EVENT).is_ok());
    }

    #[test]
    fn program_files_style_acl_is_trusted() {
        let sddl = format!(
            "O:{TRUSTED_INSTALLER_SID}G:{TRUSTED_INSTALLER_SID}D:PAI(A;;FA;;;{TRUSTED_INSTALLER_SID})(A;CIIO;GA;;;{TRUSTED_INSTALLER_SID})\
             (A;;FA;;;SY)(A;OICIIO;GA;;;SY)(A;;FA;;;BA)(A;OICIIO;GA;;;BA)(A;;0x1200a9;;;BU)(A;OICIIO;GXGR;;;BU)\
             (A;OICIIO;GA;;;CO)(A;;0x1200a9;;;AC)(A;OICIIO;GXGR;;;AC)"
        );
        assert_eq!(trust(&sddl), Trust::Trusted);
    }

    #[test]
    fn weak_descriptors_are_rejected() {
        // Users may write.
        assert!(matches!(trust("O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;BU)"), Trust::WeakAce(_)));
        // Users may only add files (ProgramData's default).
        assert!(matches!(trust("O:BAD:P(A;;FA;;;BA)(A;CI;0x2;;;BU)"), Trust::WeakAce(_)));
        // Everyone may change the DACL.
        assert!(matches!(trust("O:BAD:P(A;;FA;;;BA)(A;;WD;;;WD)"), Trust::WeakAce(_)));
        // Owned by a normal user (could rewrite the DACL).
        assert!(matches!(trust("O:S-1-5-21-1-2-3-1001D:P(A;;FA;;;BA)"), Trust::UntrustedOwner(_)));
        // Null DACL.
        assert_eq!(trust("O:BAD:NO_ACCESS_CONTROL"), Trust::NoDacl);
        // CREATOR OWNER is only acceptable on inherit-only ACEs.
        assert!(matches!(trust("O:BAD:P(A;;FA;;;BA)(A;;GA;;;CO)"), Trust::WeakAce(_)));
    }

    #[test]
    fn deny_and_read_aces_are_fine() {
        assert_eq!(trust("O:SYD:P(D;;FA;;;WD)(A;;FA;;;SY)(A;;FR;;;WD)"), Trust::Trusted);
    }

    #[test]
    fn current_user_has_a_sid() {
        assert!(current_user_sid().is_some_and(|s| s.starts_with("S-1-")));
    }
}
