//! Global named mutexes shared by hardware-monitoring tools (HWiNFO, AIDA64, LHM, ...)
//! so that two programs never interleave Super I/O or PCI accesses.

use std::marker::PhantomData;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{HANDLE, LocalFree, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, MUTEX_MODIFY_STATE, OpenMutexW, ReleaseMutex, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
};

use crate::util::win::{OwnedHandle, wide};

pub const ISA_BUS: &str = r"Global\Access_ISABUS.HTP.Method";
pub const PCI_BUS: &str = r"Global\Access_PCI";

pub struct GlobalMutex {
    handle: Option<OwnedHandle>,
}

impl GlobalMutex {
    /// Creates (or opens) the named mutex with a DACL that lets every tool open it.
    pub fn new(name: &str) -> Self {
        let wname = wide(name);
        unsafe {
            let sddl = wide("D:(A;;GA;;;WD)");
            let mut sd: PSECURITY_DESCRIPTOR = null_mut();
            let have_sd = ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                null_mut(),
            ) != 0;
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: if have_sd { sd } else { null_mut() },
                bInheritHandle: 0,
            };
            let mut h: HANDLE = CreateMutexW(&sa, 0, wname.as_ptr());
            if have_sd {
                LocalFree(sd as _);
            }
            if h.is_null() {
                h = OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE | MUTEX_MODIFY_STATE, 0, wname.as_ptr());
            }
            if h.is_null() {
                log::warn!("could not create or open mutex {name}; continuing without it");
            }
            Self { handle: OwnedHandle::new(h) }
        }
    }

    /// Waits up to `timeout_ms`. Returns `None` if another program holds the bus.
    pub fn lock(&self, timeout_ms: u32) -> Option<GlobalMutexGuard<'_>> {
        let Some(h) = &self.handle else {
            return Some(GlobalMutexGuard { mutex: self, _not_send: PhantomData });
        };
        let r = unsafe { WaitForSingleObject(h.raw(), timeout_ms) };
        if r == WAIT_OBJECT_0 || r == WAIT_ABANDONED {
            Some(GlobalMutexGuard { mutex: self, _not_send: PhantomData })
        } else {
            None
        }
    }
}

/// Released on drop; Win32 mutexes are thread-affine, so the guard is `!Send`.
pub struct GlobalMutexGuard<'a> {
    mutex: &'a GlobalMutex,
    _not_send: PhantomData<*const ()>,
}

impl Drop for GlobalMutexGuard<'_> {
    fn drop(&mut self) {
        if let Some(h) = &self.mutex.handle {
            unsafe {
                ReleaseMutex(h.raw());
            }
        }
    }
}
