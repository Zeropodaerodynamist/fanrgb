//! Interface to the PawnIO kernel driver (https://pawnio.eu).
//!
//! PawnIO is a signed, scriptable driver: user mode loads a signed module blob and
//! then calls the module's `ioctl_*` functions. Every handle hosts one module instance.
//! It speaks the same driver protocol as LibreHardwareMonitor's `PawnIo.cs`.

use std::ffi::c_void;
use std::ptr::{null, null_mut};

use anyhow::Result;
use sha2::{Digest, Sha256};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, GENERIC_READ, GENERIC_WRITE,
    GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::error::{ErrorCode, err};
use crate::util::win::{HKEY_LOCAL_MACHINE, reg_get_string, wide};

/// Signed modules from the official PawnIO.Modules release (LGPL-2.1, see assets/pawnio/COPYING).
pub const MODULE_LPCIO: &[u8] = include_bytes!("../../assets/pawnio/LpcIO.bin");
pub const MODULE_AMD17: &[u8] = include_bytes!("../../assets/pawnio/AMDFamily17.bin");

/// Release of PawnIO.Modules the bundled blobs come from.
pub const MODULES_RELEASE: &str = "0.2.11";

/// SHA-256 of the PawnIO.Modules 0.2.11 release blobs above. A module is only handed to the
/// driver if its hash matches, so a swapped asset can't slip into a build unnoticed.
pub const MODULE_SHA256: &[(&str, &str)] = &[
    ("LpcIO", "b3896a1cab0d808fca31fe2ebcae045d59dac690da87b17c858bb8da357eb45e"),
    ("AMDFamily17", "dae74615761b78bdf064dfb3e136252ddcc6fc727d88f14738d0e5800d427a91"),
];

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// Checks `blob` against the pinned hash of `module`.
pub fn verify_module(module: &str, blob: &[u8]) -> std::result::Result<(), String> {
    let Some((_, want)) = MODULE_SHA256.iter().find(|(m, _)| *m == module) else {
        return Err(format!("PawnIO module {module} has no pinned hash; refusing to load it"));
    };
    let got = sha256_hex(blob);
    if got != *want {
        return Err(format!("PawnIO module {module} failed its integrity check (SHA-256 {got}); refusing to load it"));
    }
    Ok(())
}

const DEVICE_TYPE: u32 = 41394;
const fn ctl_code(device: u32, function: u32) -> u32 {
    // METHOD_BUFFERED (0), FILE_ANY_ACCESS (0)
    (device << 16) | (function << 2)
}
const IOCTL_PIO_LOAD_BINARY: u32 = ctl_code(DEVICE_TYPE, 0x821);
const IOCTL_PIO_EXECUTE_FN: u32 = ctl_code(DEVICE_TYPE, 0x841);
const FN_NAME_LENGTH: usize = 32;
const MAX_ARGS: usize = 8;

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PawnIoStatus {
    /// Driver opened and module accepted.
    Ready,
    /// Driver is not installed.
    NotInstalled,
    /// Installed, but the device could not be opened (usually: not elevated).
    AccessDenied,
    /// Other failure (error text).
    Error(String),
    /// Not probed yet.
    #[default]
    Unknown,
}

impl PawnIoStatus {
    pub fn describe(&self) -> &str {
        match self {
            PawnIoStatus::Ready => "PawnIO driver ready",
            PawnIoStatus::NotInstalled => "PawnIO driver is not installed",
            PawnIoStatus::AccessDenied => "PawnIO needs administrator rights",
            PawnIoStatus::Error(e) => e.as_str(),
            PawnIoStatus::Unknown => "PawnIO not checked",
        }
    }

    /// The error code for a status that prevents hardware access (`None` when ready).
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            PawnIoStatus::Ready => None,
            PawnIoStatus::NotInstalled => Some(ErrorCode::PawnIoMissing),
            PawnIoStatus::AccessDenied => Some(ErrorCode::NotElevated),
            PawnIoStatus::Error(e) if e.contains("module") => Some(ErrorCode::PawnIoModule),
            PawnIoStatus::Error(_) | PawnIoStatus::Unknown => Some(ErrorCode::PawnIoCall),
        }
    }
}

pub struct PawnIo {
    handle: HANDLE,
    module: &'static str,
}

// SAFETY: the handle is a plain kernel file handle; calls are serialized by the owner.
unsafe impl Send for PawnIo {}

impl PawnIo {
    /// Installed PawnIO version from the uninstall registry key, if any.
    pub fn installed_version() -> Option<String> {
        reg_get_string(
            HKEY_LOCAL_MACHINE,
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO",
            "DisplayVersion",
        )
    }

    /// Opens the driver and loads `blob` into a fresh executor.
    pub fn open(module: &'static str, blob: &[u8]) -> std::result::Result<Self, PawnIoStatus> {
        if let Err(e) = verify_module(module, blob) {
            log::error!("[{}] {e}", ErrorCode::PawnIoModule);
            return Err(PawnIoStatus::Error(e));
        }
        let path = wide(r"\\?\GLOBALROOT\Device\PawnIO");
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            let err = unsafe { GetLastError() };
            return Err(match err {
                ERROR_ACCESS_DENIED => PawnIoStatus::AccessDenied,
                ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => {
                    if Self::installed_version().is_some() {
                        PawnIoStatus::Error(
                            "PawnIO is installed but its driver is not running (reboot may be required)".into(),
                        )
                    } else {
                        PawnIoStatus::NotInstalled
                    }
                }
                e => PawnIoStatus::Error(format!("Cannot open PawnIO device (Win32 error {e})")),
            });
        }
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_PIO_LOAD_BINARY,
                blob.as_ptr() as *const c_void,
                blob.len() as u32,
                null_mut(),
                0,
                &mut returned,
                null_mut(),
            )
        };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            unsafe { CloseHandle(handle) };
            return Err(PawnIoStatus::Error(format!(
                "PawnIO refused module {module} (Win32 error {err}); try updating PawnIO"
            )));
        }
        Ok(Self { handle, module })
    }

    /// Executes `func` with up to 8 input values; writes into `out` and returns the
    /// number of values the module produced.
    pub fn execute_into(&self, func: &str, input: &[u64], out: &mut [u64]) -> Result<usize> {
        if input.len() > MAX_ARGS || out.len() > MAX_ARGS {
            return Err(err(
                ErrorCode::PawnIoCall,
                format!("PawnIO {}::{}: more than {MAX_ARGS} values", self.module, func),
            ));
        }
        let mut inbuf = [0u8; FN_NAME_LENGTH + MAX_ARGS * 8];
        let name = func.as_bytes();
        let n = name.len().min(FN_NAME_LENGTH - 1);
        inbuf[..n].copy_from_slice(&name[..n]);
        for (i, v) in input.iter().enumerate() {
            let off = FN_NAME_LENGTH + i * 8;
            inbuf[off..off + 8].copy_from_slice(&v.to_le_bytes());
        }
        let in_len = FN_NAME_LENGTH + input.len() * 8;
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_PIO_EXECUTE_FN,
                inbuf.as_ptr() as *const c_void,
                in_len as u32,
                out.as_mut_ptr() as *mut c_void,
                (out.len() * 8) as u32,
                &mut returned,
                null_mut(),
            )
        };
        if ok == 0 {
            let code = unsafe { GetLastError() };
            return Err(err(
                ErrorCode::PawnIoCall,
                format!("PawnIO {}::{} failed (Win32 error {code})", self.module, func),
            ));
        }
        // Never report more values than the caller's buffer holds, whatever the driver says.
        Ok((returned as usize / 8).min(out.len()))
    }

    /// Convenience wrapper returning the first output value.
    pub fn call1(&self, func: &str, input: &[u64]) -> Result<u64> {
        let mut out = [0u64; 1];
        let n = self.execute_into(func, input, &mut out)?;
        if n < 1 {
            return Err(err(ErrorCode::PawnIoCall, format!("PawnIO {}::{} returned no data", self.module, func)));
        }
        Ok(out[0])
    }

    pub fn call0(&self, func: &str, input: &[u64]) -> Result<()> {
        let mut out = [0u64; 0];
        self.execute_into(func, input, &mut out).map(|_| ())
    }
}

impl Drop for PawnIo {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_modules_match_pinned_hashes() {
        assert_eq!(verify_module("LpcIO", MODULE_LPCIO), Ok(()));
        assert_eq!(verify_module("AMDFamily17", MODULE_AMD17), Ok(()));
        assert!(verify_module("LpcIO", MODULE_AMD17).is_err());
        assert!(verify_module("Unknown", MODULE_LPCIO).is_err());
        let mut tampered = MODULE_LPCIO.to_vec();
        tampered[100] ^= 1;
        assert!(verify_module("LpcIO", &tampered).is_err());
    }

    #[test]
    fn status_codes() {
        assert_eq!(PawnIoStatus::NotInstalled.code(), Some(ErrorCode::PawnIoMissing));
        assert_eq!(PawnIoStatus::AccessDenied.code(), Some(ErrorCode::NotElevated));
        assert_eq!(PawnIoStatus::Error("PawnIO refused module LpcIO".into()).code(), Some(ErrorCode::PawnIoModule));
        assert_eq!(PawnIoStatus::Ready.code(), None);
    }
}
