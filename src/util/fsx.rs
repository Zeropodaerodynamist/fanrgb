//! File-system helpers for an elevated process that must not be redirected by junctions, symbolic
//! links or attacker-chosen file contents.
//!
//! - [`read_limited`] refuses reparse points, directories, oversized files and paths that resolve
//!   somewhere else (a junction in any parent folder).
//! - [`write_atomic`] writes a uniquely named temp file in the target folder (`CREATE_NEW`, never follows
//!   an existing link) and renames it over the target, while holding the folder open so it cannot be
//!   swapped for a junction in between.
//! - [`ensure_secure_dir`] creates (or repairs) an admin-only directory such as `%ProgramData%\FanRGB`.
//! - [`take_limited`] / [`delete_checked`] read and delete a file in a folder a *less privileged* user
//!   controls (the desktop user's `%LOCALAPPDATA%`): the deletion goes through the handle whose real
//!   location was verified, never through a path an elevated `DeleteFileW` would resolve again.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetSecurityInfo};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED,
    FILE_RENAME_INFO, FILE_SHARE_READ, FILE_SHARE_WRITE, FileDispositionInfo, FileRenameInfo,
    GetFileInformationByHandle, GetFinalPathNameByHandleW, GetFullPathNameW, GetLongPathNameW,
    SetFileInformationByHandle, VOLUME_NAME_DOS,
};

use super::sec::{OwnedSd, Trust};

const READ_CONTROL: u32 = 0x0002_0000;
const WRITE_DAC: u32 = 0x0004_0000;
const WRITE_OWNER: u32 = 0x0008_0000;
const DELETE: u32 = 0x0001_0000;
const SYNCHRONIZE: u32 = 0x0010_0000;
const FILE_READ_ATTRIBUTES: u32 = 0x0080;
const FILE_LIST_DIRECTORY: u32 = 0x0001;
const FILE_SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | 0x4; // | FILE_SHARE_DELETE
/// `SECURITY_IDENTIFICATION` (with `SECURITY_SQOS_PRESENT`, which `security_qos_flags` adds): if a path in
/// a folder a less privileged user controls is redirected to a named pipe (a junction to an object
/// directory plus an object-manager link), the pipe's server may identify this elevated client but
/// never impersonate it. The redirect itself is still refused afterwards by the final-path check.
const SQOS_IDENTIFICATION: u32 = 0x0001_0000;
/// Share mode of a pinned folder handle (see [`open_dir_checked`]). Write sharing must stay allowed:
/// renaming a file *into* the folder opens the folder for writing (a handle denying it makes every
/// rename fail with a sharing violation).
const DIR_PIN_SHARE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;
const GENERIC_WRITE: u32 = 0x4000_0000;
const GENERIC_READ: u32 = 0x8000_0000;

/// Null-terminated UTF-16 path.
pub fn wide_path(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

fn refused(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, msg.into())
}

fn info(f: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    // SAFETY: plain data; all-zero is a valid value.
    let mut i: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `f` is an open file handle and `i` a writable BY_HANDLE_FILE_INFORMATION.
    if unsafe { GetFileInformationByHandle(f.as_raw_handle() as HANDLE, &mut i) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(i)
}

fn is_reparse(i: &BY_HANDLE_FILE_INFORMATION) -> bool {
    i.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn is_dir(i: &BY_HANDLE_FILE_INFORMATION) -> bool {
    i.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0
}

/// The path the handle actually refers to (`\\?\` prefix removed).
pub fn final_path(f: &File) -> io::Result<PathBuf> {
    let mut buf = vec![0u16; 1024];
    loop {
        // SAFETY: `f` is an open handle; `buf` is writable for the length passed; the return value is checked against it.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                f.as_raw_handle() as HANDLE,
                buf.as_mut_ptr(),
                buf.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        } as usize;
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n < buf.len() {
            return Ok(PathBuf::from(strip_verbatim(&String::from_utf16_lossy(&buf[..n]))));
        }
        buf.resize(n + 1, 0);
    }
}

fn strip_verbatim(s: &str) -> String {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s.to_string()
    }
}

/// Absolute, long-name, lower-case form of `p` for comparisons.
fn normalized(p: &Path) -> String {
    let w = wide_path(p);
    let mut full = vec![0u16; 1024];
    // SAFETY: `w` is NUL-terminated; `full` is writable for the length passed; the file-part pointer is optional.
    let n =
        unsafe { GetFullPathNameW(w.as_ptr(), full.len() as u32, full.as_mut_ptr(), std::ptr::null_mut()) } as usize;
    let mut s: Vec<u16> = if n > 0 && n < full.len() { full[..n].to_vec() } else { w[..w.len() - 1].to_vec() };
    s.push(0);
    let mut long = vec![0u16; 1024];
    // SAFETY: `s` is NUL-terminated; `long` is writable for the length passed.
    let n = unsafe { GetLongPathNameW(s.as_ptr(), long.as_mut_ptr(), long.len() as u32) } as usize;
    let text = if n > 0 && n < long.len() {
        String::from_utf16_lossy(&long[..n])
    } else {
        String::from_utf16_lossy(&s[..s.len() - 1])
    };
    strip_verbatim(&text).trim_end_matches('\\').to_lowercase()
}

/// True if both paths name the same location (case-insensitive, after normalization).
pub fn same_path(a: &Path, b: &Path) -> bool {
    normalized(a) == normalized(b)
}

/// Opens a directory without following a reparse point and verifies that no component of `dir`
/// is redirected. The handle denies delete/rename sharing, so while it is open neither the folder nor
/// any of its parents can be renamed or replaced by a junction. (It cannot deny write sharing, see
/// [`DIR_PIN_SHARE`]; [`write_atomic`] covers the remaining "empty folder becomes a mount point" case
/// with handle-based checks.)
pub fn open_dir_checked(dir: &Path) -> io::Result<File> {
    // FILE_LIST_DIRECTORY is needed for the share mode to be enforced at all (share access is only
    // recorded for opens that request data or delete access).
    let f = OpenOptions::new()
        .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE)
        .share_mode(DIR_PIN_SHARE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(dir)?;
    let i = info(&f)?;
    if is_reparse(&i) {
        return Err(refused(format!("{} is a junction or link", dir.display())));
    }
    if !is_dir(&i) {
        return Err(refused(format!("{} is not a directory", dir.display())));
    }
    let real = final_path(&f)?;
    if !same_path(&real, dir) {
        return Err(refused(format!("{} is redirected to {}", dir.display(), real.display())));
    }
    Ok(f)
}

/// Reads a regular file of at most `max` bytes. Refuses reparse points, directories and redirected paths.
pub fn read_limited(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let f = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_ALL)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .security_qos_flags(SQOS_IDENTIFICATION)
        .open(path)?;
    read_open_file(&f, path, max)
}

/// [`read_limited`] for files only an elevated process may have written: also refuses a file whose
/// owner is not Administrators/SYSTEM/TrustedInstaller (e.g. planted by a normal user before the folder
/// was secured). Owner and contents come from the same handle.
pub fn read_limited_admin_owned(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let f = OpenOptions::new()
        .access_mode(GENERIC_READ | READ_CONTROL)
        .share_mode(FILE_SHARE_ALL)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if !OwnedSd::of_file(f.as_raw_handle() as HANDLE)?.owner_is_admin() {
        return Err(refused(format!("{} is not owned by Administrators or SYSTEM", path.display())));
    }
    read_open_file(&f, path, max)
}

/// Type, size and location checks by handle, then reads from the same handle.
fn read_open_file(f: &File, path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let i = info(f)?;
    if is_reparse(&i) {
        return Err(refused(format!("{} is a link", path.display())));
    }
    if is_dir(&i) {
        return Err(refused(format!("{} is a directory", path.display())));
    }
    let size = ((i.nFileSizeHigh as u64) << 32) | i.nFileSizeLow as u64;
    if size > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is too large ({size} bytes)", path.display()),
        ));
    }
    let real = final_path(f)?;
    if !same_path(&real, path) {
        return Err(refused(format!("{} is redirected to {}", path.display(), real.display())));
    }
    let mut out = Vec::with_capacity(size as usize);
    f.take(max + 1).read_to_end(&mut out)?;
    if out.len() as u64 > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} grew beyond {max} bytes", path.display())));
    }
    Ok(out)
}

/// Opens `path` for deletion (plus `extra` access) and verifies, on the handle, that it is a regular
/// file (not a reparse point, not a directory) whose real location is `path`, so a junction or an
/// object-manager link (`\RPC Control\…`) in any parent folder is refused before anything is deleted.
/// Other openers may only read while the handle is open (no rename, write or delete).
fn open_for_delete(path: &Path, extra: u32) -> io::Result<File> {
    let f = OpenOptions::new()
        .access_mode(DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE | extra)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .security_qos_flags(SQOS_IDENTIFICATION)
        .open(path)?;
    let i = info(&f)?;
    if is_reparse(&i) {
        return Err(refused(format!("{} is a link", path.display())));
    }
    if is_dir(&i) {
        return Err(refused(format!("{} is a directory", path.display())));
    }
    let real = final_path(&f)?;
    if !same_path(&real, path) {
        return Err(refused(format!("{} is redirected to {}", path.display(), real.display())));
    }
    Ok(f)
}

/// Reads a regular file of at most `max` bytes in a folder a less privileged user controls, then
/// deletes it through the same handle (an elevated process never deletes there by name).
/// Refuses (and deletes nothing) for links, directories and redirected paths. A file that is too
/// large is still deleted (its location was verified), and the size error returned.
pub fn take_limited(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let f = open_for_delete(path, GENERIC_READ)?;
    let data = read_open_file(&f, path, max);
    delete_by_handle(&f)?;
    data
}

/// Deletes a regular file in a folder a less privileged user controls, through a handle whose real
/// location was verified (see [`take_limited`]). A missing file is not an error.
pub fn delete_checked(path: &Path) -> io::Result<()> {
    match open_for_delete(path, 0) {
        Ok(f) => delete_by_handle(&f),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// [`read_limited`] + UTF-8 decoding.
pub fn read_to_string_limited(path: &Path, max: u64) -> io::Result<String> {
    String::from_utf8(read_limited(path, max)?).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

static TMP_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Atomically replaces `path` with `bytes`: unique temp file in the same folder (`CREATE_NEW`, so an
/// existing file or link with that name is never opened), flushed, then renamed over the target.
/// Refuses if the folder (or a parent) is redirected, or if the target is a link or a directory.
///
/// Junction races: the folder stays pinned (it and its parents cannot be renamed). The only
/// remaining trick, turning an *empty* folder into a mount point between the check and the temp file's
/// creation, is caught by checking the temp file's real location from its handle. Once the temp file
/// exists the folder is not empty, so it can no longer become a mount point. The rename is done on the
/// temp file's handle (`SetFileInformationByHandle`, never by re-opening the source by name) and the
/// final location is verified from the same handle. A temp file that ended up anywhere else is deleted
/// through its handle, never by name.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).ok_or_else(|| refused("no parent folder"))?;
    let name = path.file_name().ok_or_else(|| refused("no file name"))?;
    // Held until the rename is done: the folder cannot be renamed/replaced meanwhile.
    let dir_handle = open_dir_checked(dir)?;

    match OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_ALL)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
    {
        Ok(existing) => {
            let i = info(&existing)?;
            if is_reparse(&i) {
                return Err(refused(format!("{} is a link", path.display())));
            }
            if is_dir(&i) {
                return Err(refused(format!("{} is a directory", path.display())));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    let tmp = dir.join(format!(
        ".{}.{}-{}.tmp",
        name.to_string_lossy(),
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut f = OpenOptions::new()
        .write(true)
        .access_mode(GENERIC_WRITE | DELETE | WRITE_OWNER | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .create_new(true)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&tmp)?;
    let result = (|| {
        let real = final_path(&f)?;
        if !same_path(&real, &tmp) {
            return Err(refused(format!("{} was redirected to {}", tmp.display(), real.display())));
        }
        if super::win::is_elevated() {
            own_as_admins(f.as_raw_handle() as HANDLE)?;
        }
        f.write_all(bytes)?;
        f.sync_all()?;
        rename_handle(f.as_raw_handle() as HANDLE, None, path, true)?;
        let real = final_path(&f)?;
        if !same_path(&real, path) {
            return Err(refused(format!("{} landed at {}", path.display(), real.display())));
        }
        Ok(())
    })();
    if result.is_err() {
        // Our own file, wherever it is now: delete it through the handle, never by name.
        let _ = delete_by_handle(&f);
    }
    drop(f);
    drop(dir_handle);
    result
}

/// Makes Administrators the owner of the new file `h` (opened with `WRITE_OWNER`), as UAC's elevated
/// token already does by default. Without UAC (built-in Administrator, UAC switched off) the default
/// owner is the user, and [`read_limited_admin_owned`] would refuse FanRGB's own state files.
fn own_as_admins(h: HANDLE) -> io::Result<()> {
    let sd = OwnedSd::from_sddl("O:BA")?;
    // SAFETY: `h` is open with WRITE_OWNER; the owner SID points into `sd`, alive for the call.
    let rc = unsafe {
        SetSecurityInfo(
            h,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            sd.owner(),
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    Ok(())
}

/// Marks the file behind `f` (opened with `DELETE`) for deletion when its last handle closes.
fn delete_by_handle(f: &File) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: `f` is open with DELETE access; `info` is a valid FILE_DISPOSITION_INFO of the size passed.
    let ok = unsafe {
        SetFileInformationByHandle(
            f.as_raw_handle() as HANDLE,
            FileDispositionInfo,
            &info as *const FILE_DISPOSITION_INFO as *const c_void,
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

// ---------------------------------------------------------------------------------------
// Admin-only directories

/// State of an existing directory.
#[derive(Debug, PartialEq, Eq)]
pub enum DirCheck {
    Missing,
    /// A junction / symbolic link (or other reparse point) sits at the path.
    ReparsePoint,
    NotDirectory,
    Untrusted(Trust),
    Trusted,
}

/// Inspects `dir` without following reparse points.
pub fn check_dir(dir: &Path) -> io::Result<DirCheck> {
    let f = match OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES | READ_CONTROL)
        .share_mode(FILE_SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(dir)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(DirCheck::Missing),
        Err(e) => return Err(e),
    };
    let i = info(&f)?;
    if is_reparse(&i) {
        return Ok(DirCheck::ReparsePoint);
    }
    if !is_dir(&i) {
        return Ok(DirCheck::NotDirectory);
    }
    let real = final_path(&f)?;
    if !same_path(&real, dir) {
        return Ok(DirCheck::ReparsePoint);
    }
    match OwnedSd::of_file(f.as_raw_handle() as HANDLE)?.trust() {
        Trust::Trusted => Ok(DirCheck::Trusted),
        t => Ok(DirCheck::Untrusted(t)),
    }
}

/// Like [`check_dir`] for a regular file (e.g. the installed executable).
pub fn check_file(file: &Path) -> io::Result<DirCheck> {
    let f = match OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES | READ_CONTROL)
        .share_mode(FILE_SHARE_ALL)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(file)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(DirCheck::Missing),
        Err(e) => return Err(e),
    };
    let i = info(&f)?;
    if is_reparse(&i) {
        return Ok(DirCheck::ReparsePoint);
    }
    if is_dir(&i) {
        return Ok(DirCheck::NotDirectory);
    }
    if !same_path(&final_path(&f)?, file) {
        return Ok(DirCheck::ReparsePoint);
    }
    match OwnedSd::of_file(f.as_raw_handle() as HANDLE)?.trust() {
        Trust::Trusted => Ok(DirCheck::Trusted),
        t => Ok(DirCheck::Untrusted(t)),
    }
}

/// What [`ensure_secure_dir`] had to do.
#[derive(Debug, PartialEq, Eq)]
pub enum SecureDir {
    Created,
    Existing,
    /// A weak/foreign object was found at the path, moved aside to this name, and a fresh dir created.
    Replaced(PathBuf),
}

/// Makes sure `dir` is a real directory that only SYSTEM/Administrators can modify, creating it with
/// `sddl` if missing. Anything else found at the path (a junction, a file, a folder pre-created by a
/// normal user to squat the name) is locked down and moved aside to `<name>.untrusted-<time>`, then a
/// fresh directory is created. Needs elevation for everything but the "already trusted" case.
pub fn ensure_secure_dir(dir: &Path, sddl: &str) -> io::Result<SecureDir> {
    let sd = OwnedSd::from_sddl(sddl)?;
    let mut replaced = None;
    for _ in 0..3 {
        let sa = sd.attributes();
        let w = wide_path(dir);
        // SAFETY: `w` is NUL-terminated; `sa` points at the descriptor owned by `sd`, alive for the call.
        if unsafe { CreateDirectoryW(w.as_ptr(), &sa) } != 0 {
            return match check_dir(dir)? {
                DirCheck::Trusted => Ok(replaced.map(SecureDir::Replaced).unwrap_or(SecureDir::Created)),
                other => Err(refused(format!("{} was created but is {other:?}", dir.display()))),
            };
        }
        // SAFETY: thread-local read, no preconditions.
        let err = unsafe { GetLastError() };
        if err != ERROR_ALREADY_EXISTS {
            return Err(io::Error::from_raw_os_error(err as i32));
        }
        match check_dir(dir)? {
            DirCheck::Trusted => {
                return Ok(replaced.map(SecureDir::Replaced).unwrap_or(SecureDir::Existing));
            }
            DirCheck::Missing => continue, // removed in between; try creating again
            _ => replaced = Some(quarantine(dir, &sd)?),
        }
    }
    Err(refused(format!("{} keeps being re-created by someone else", dir.display())))
}

/// Renames the object at `dir` out of the way. A real directory is first taken over (owner and DACL
/// replaced, so handles opened later fail); a junction or a file is renamed as it is (the link itself,
/// never its target; see the hard-link note below).
fn quarantine(dir: &Path, sd: &OwnedSd) -> io::Result<PathBuf> {
    super::sec::enable_privilege("SeTakeOwnershipPrivilege");
    super::sec::enable_privilege("SeRestorePrivilege");
    super::sec::enable_privilege("SeBackupPrivilege");
    let f = OpenOptions::new()
        .access_mode(DELETE | WRITE_DAC | WRITE_OWNER | READ_CONTROL | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(FILE_SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(dir)?;
    let h = f.as_raw_handle() as HANDLE;
    let i = info(&f)?;
    // Only a real directory is locked down. A *file* at this name may be a hard link another user made
    // to a file they cannot otherwise touch: a security descriptor belongs to the file, not to the
    // name, so rewriting it here (with backup/restore/take-ownership privileges) would hand every user
    // read access to the link's target and drop its real owner and ACL. A file (like a junction) is
    // only renamed out of the way, which affects this one name.
    if !is_reparse(&i) && is_dir(&i) {
        // SAFETY: `h` is open with WRITE_DAC/WRITE_OWNER; owner and DACL point into the descriptor owned by `sd`, alive for the call.
        let rc = unsafe {
            SetSecurityInfo(
                h,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                sd.owner(),
                std::ptr::null_mut(),
                sd.dacl(),
                std::ptr::null(),
            )
        };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
    }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let target = dir.with_file_name(format!("{name}.untrusted-{stamp}-{}", std::process::id()));
    rename_handle(h, None, &target, false)?;
    Ok(target)
}

/// Renames the object behind `h` (opened with `DELETE`) to `to`: an absolute path, or a plain name
/// relative to the directory handle `root`.
fn rename_handle(h: HANDLE, root: Option<HANDLE>, to: &Path, replace: bool) -> io::Result<()> {
    let name: Vec<u16> = to.as_os_str().encode_wide().collect();
    let size = std::mem::size_of::<FILE_RENAME_INFO>() + name.len() * 2;
    let mut buf = vec![0u64; size.div_ceil(8)];
    // SAFETY: `buf` is 8-byte aligned and large enough for FILE_RENAME_INFO plus `name.len()` UTF-16
    // units (FileName is the trailing array); every write stays inside it. `h` is a handle opened with
    // DELETE access and `root` (if any) an open directory handle, both valid for the call.
    unsafe {
        let ri = buf.as_mut_ptr() as *mut FILE_RENAME_INFO;
        (*ri).Anonymous.ReplaceIfExists = replace;
        (*ri).RootDirectory = root.unwrap_or(std::ptr::null_mut());
        (*ri).FileNameLength = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(name.as_ptr(), (*ri).FileName.as_mut_ptr(), name.len());
        if SetFileInformationByHandle(h, FileRenameInfo, ri as *const c_void, size as u32) == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fresh, empty folder under the cargo target dir (never in a real user/system location).
    pub(crate) fn test_dir(name: &str) -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let d = exe.parent().unwrap().join("fanrgb-sec-tests").join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn junction(link: &Path, target: &Path) -> bool {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn atomic_write_creates_and_replaces() {
        let d = test_dir("atomic");
        let p = d.join("config.json");
        write_atomic(&p, b"one").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"one");
        write_atomic(&p, b"two, longer").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two, longer");
        // Elevated, the file is Administrators' even where UAC is off (read_limited_admin_owned accepts it).
        assert_eq!(read_limited_admin_owned(&p, 64).is_ok(), crate::util::win::is_elevated());
        // No temp files left behind.
        let names: Vec<_> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[test]
    fn failed_atomic_write_deletes_its_temp_file_by_handle() {
        let d = test_dir("atomic-fail");
        let p = d.join("config.json");
        std::fs::write(&p, b"old").unwrap();
        // Another handle without delete sharing: the rename over the target fails after the temp file
        // was created and written.
        let lock = OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(&p).unwrap();
        assert!(write_atomic(&p, b"new").is_err());
        drop(lock);
        let names: Vec<_> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("config.json")], "temp file left behind");
        assert_eq!(std::fs::read(&p).unwrap(), b"old");
        write_atomic(&p, b"new").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
    }

    #[test]
    fn atomic_write_refuses_directory_target() {
        let d = test_dir("atomic-dir");
        std::fs::create_dir(d.join("x")).unwrap();
        assert!(write_atomic(&d.join("x"), b"data").is_err());
    }

    #[test]
    fn junctions_are_refused() {
        let d = test_dir("junction");
        let real = d.join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("f.json"), b"{}").unwrap();
        let link = d.join("link");
        if !junction(&link, &real) {
            eprintln!("mklink /J unavailable; skipping");
            return;
        }
        // Writing through a junctioned folder is refused, and nothing lands in the real folder.
        assert!(write_atomic(&link.join("new.json"), b"x").is_err());
        assert!(!real.join("new.json").exists());
        // Reading through it is refused as well.
        assert!(read_limited(&link.join("f.json"), 100).is_err());
        assert!(open_dir_checked(&link).is_err());
        assert_eq!(check_dir(&link).unwrap(), DirCheck::ReparsePoint);
        // A junction in a parent folder is caught too.
        let nested = real.join("sub");
        std::fs::create_dir(&nested).unwrap();
        assert!(write_atomic(&link.join("sub").join("n.json"), b"x").is_err());
        assert!(!nested.join("n.json").exists());
        // The real paths still work.
        assert_eq!(read_limited(&real.join("f.json"), 100).unwrap(), b"{}");
    }

    /// Review M3: read-and-delete through a verified handle. Through a junctioned folder nothing is
    /// read or deleted; the real file stays.
    #[test]
    fn take_and_delete_only_where_the_path_really_is() {
        let d = test_dir("take");
        let p = d.join("report.json");
        std::fs::write(&p, b"{}").unwrap();
        assert_eq!(take_limited(&p, 100).unwrap(), b"{}");
        assert!(!p.exists(), "deleted after reading");
        assert_eq!(take_limited(&p, 100).unwrap_err().kind(), io::ErrorKind::NotFound);
        // Too large: the error is returned and the file is still removed.
        std::fs::write(&p, vec![b'a'; 300]).unwrap();
        assert!(take_limited(&p, 100).is_err());
        assert!(!p.exists());
        // delete_checked: removes a file, tolerates a missing one, refuses a directory.
        std::fs::write(&p, b"x").unwrap();
        delete_checked(&p).unwrap();
        assert!(!p.exists());
        delete_checked(&p).unwrap();
        assert!(delete_checked(&d).is_err() && d.exists());
        // A junction in a parent folder: refused, the target stays.
        let real = d.join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("victim.json"), b"keep").unwrap();
        let link = d.join("link");
        if !junction(&link, &real) {
            eprintln!("mklink /J unavailable; skipping the junction part");
            return;
        }
        assert!(take_limited(&link.join("victim.json"), 100).is_err());
        assert!(delete_checked(&link.join("victim.json")).is_err());
        assert_eq!(std::fs::read(real.join("victim.json")).unwrap(), b"keep");
        // The junction itself (a reparse point) is never deleted as a file either.
        assert!(delete_checked(&link).is_err() && link.exists());
    }

    #[test]
    fn read_limited_enforces_size_and_type() {
        let d = test_dir("limited");
        let p = d.join("big.json");
        std::fs::write(&p, vec![b'a'; 2048]).unwrap();
        assert!(read_limited(&p, 1024).is_err());
        assert_eq!(read_limited(&p, 4096).unwrap().len(), 2048);
        assert!(read_limited(&d, 4096).is_err());
        assert!(read_limited(&d.join("missing"), 10).is_err());
    }

    #[test]
    fn user_created_dir_is_not_trusted() {
        let d = test_dir("trust");
        // Created by the (non-elevated or elevated) test user with inherited ACLs from target/:
        // the user owns it, so it must never be accepted as an admin-only state folder.
        match check_dir(&d).unwrap() {
            DirCheck::Untrusted(_) => {}
            DirCheck::Trusted => {
                // Only possible if the test runs elevated *and* target/ inherits admin-only ACLs.
                assert!(crate::util::win::is_elevated());
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(check_dir(&d.join("missing")).unwrap(), DirCheck::Missing);
        std::fs::write(d.join("file"), b"").unwrap();
        assert_eq!(check_dir(&d.join("file")).unwrap(), DirCheck::NotDirectory);
    }

    #[test]
    fn ensure_secure_dir_never_accepts_a_weak_dir() {
        let d = test_dir("ensure");
        let target = d.join("FanRGB");
        std::fs::create_dir(&target).unwrap();
        // Non-elevated: cannot take ownership, so it must fail rather than silently use the weak folder.
        // Elevated: it must move the weak folder aside and create a trusted one.
        match ensure_secure_dir(&target, super::super::sec::SDDL_STATE_DIR) {
            Ok(SecureDir::Replaced(old)) => {
                assert!(old.exists());
                assert_eq!(check_dir(&target).unwrap(), DirCheck::Trusted);
            }
            Ok(other) => panic!("weak dir accepted: {other:?}"),
            Err(_) => assert!(!crate::util::win::is_elevated() || check_dir(&target).unwrap() != DirCheck::Trusted),
        }
    }

    #[test]
    fn real_system_folders_are_classified() {
        // Read-only checks of this machine's ACLs.
        assert_eq!(check_dir(&crate::util::paths::program_files()).unwrap(), DirCheck::Trusted);
        assert_eq!(check_file(&crate::util::win::explorer_exe()).unwrap(), DirCheck::Trusted);
        // ProgramData lets every user create sub-folders: never trusted as is.
        assert!(matches!(check_dir(&crate::util::paths::program_data()).unwrap(), DirCheck::Untrusted(_)));
    }

    #[test]
    fn checked_dir_handle_blocks_rename() {
        let d = test_dir("pin");
        let inner = d.join("state");
        std::fs::create_dir(&inner).unwrap();
        let h = open_dir_checked(&inner).unwrap();
        assert!(std::fs::rename(&inner, d.join("moved")).is_err(), "folder renamed while pinned");
        // Files inside can still be replaced atomically.
        write_atomic(&inner.join("a.json"), b"1").unwrap();
        write_atomic(&inner.join("a.json"), b"2").unwrap();
        drop(h);
        std::fs::rename(&inner, d.join("moved")).unwrap();
    }

    #[test]
    fn path_comparison_normalizes() {
        assert!(same_path(Path::new(r"C:\Windows\System32"), Path::new(r"c:\windows\system32\")));
        assert!(!same_path(Path::new(r"C:\Windows"), Path::new(r"C:\Windows\System32")));
    }
}
