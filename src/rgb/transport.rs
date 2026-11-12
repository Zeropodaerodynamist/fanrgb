//! HID transport for the RGB Fusion 2 controller.
//!
//! The protocol code in [`super::fusion2`] only talks to a [`HidTransport`], so it can be tested
//! byte-for-byte against a recording mock. The real implementation wraps hidapi. Every transport
//! failure is turned into a coded error ([`ErrorCode::RgbDisconnected`], ...) based on the Win32
//! error hidapi reports.

use std::ffi::CString;
use std::time::Duration;

use anyhow::Result;

use super::fusion2::{PIDS, USAGE, USAGE_PAGE, VID_ITE};
use crate::error::{ErrorCode, err};

/// Feature-report I/O with one device collection. Implementations must be `Send` because device
/// I/O runs on a dedicated worker thread.
pub trait HidTransport: Send {
    /// `HidD_SetFeature`: `data[0]` is the report id.
    fn send_feature(&self, data: &[u8]) -> Result<()>;
    /// `HidD_GetFeature`: `buf[0]` holds the report id on entry. Returns the number of bytes read.
    fn get_feature(&self, buf: &mut [u8]) -> Result<usize>;
    /// Settle delays required by the firmware. Routed through the transport so tests can record
    /// them instead of sleeping.
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

impl<T: HidTransport + ?Sized> HidTransport for Box<T> {
    fn send_feature(&self, data: &[u8]) -> Result<()> {
        (**self).send_feature(data)
    }
    fn get_feature(&self, buf: &mut [u8]) -> Result<usize> {
        (**self).get_feature(buf)
    }
    fn sleep(&self, d: Duration) {
        (**self).sleep(d)
    }
}

// ---------------------------------------------------------------------------------------
// Win32 error classification

/// Extracts the Win32 error from a hidapi message like
/// `HidD_SetFeature: (0x0000048F) The device is not connected.`
pub fn parse_win32(msg: &str) -> Option<u32> {
    let i = msg.find("(0x")?;
    let hex = msg.get(i + 3..i + 11)?;
    let code = u32::from_str_radix(hex, 16).ok()?;
    // HRESULT_FROM_WIN32 (0x8007xxxx) -> xxxx
    Some(if code & 0xFFFF_0000 == 0x8007_0000 { code & 0xFFFF } else { code })
}

/// Maps a Win32 error to an RGB error code. `opening` = the error came from opening the device.
pub fn classify_win32(code: Option<u32>, opening: bool) -> ErrorCode {
    match code {
        // ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_INVALID_HANDLE, ERROR_NOT_READY,
        // ERROR_OPERATION_ABORTED (I/O cancelled by device removal), ERROR_DEVICE_NOT_CONNECTED,
        // ERROR_NO_SUCH_DEVICE (433), ERROR_DEVICE_REMOVED (1617)
        Some(2 | 3 | 6 | 21 | 433 | 995 | 1167 | 1617) => ErrorCode::RgbDisconnected,
        // ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION: someone holds the collection exclusively.
        Some(5 | 32) => ErrorCode::RgbOpenFailed,
        _ if opening => ErrorCode::RgbOpenFailed,
        // ERROR_GEN_FAILURE (31), ERROR_SEM_TIMEOUT (121), ERROR_IO_DEVICE (1117), ...
        _ => ErrorCode::RgbWriteFailed,
    }
}

fn win32_name(code: u32) -> &'static str {
    match code {
        2 => "ERROR_FILE_NOT_FOUND",
        3 => "ERROR_PATH_NOT_FOUND",
        5 => "ERROR_ACCESS_DENIED",
        6 => "ERROR_INVALID_HANDLE",
        21 => "ERROR_NOT_READY",
        31 => "ERROR_GEN_FAILURE",
        32 => "ERROR_SHARING_VIOLATION",
        87 => "ERROR_INVALID_PARAMETER",
        121 => "ERROR_SEM_TIMEOUT",
        433 => "ERROR_NO_SUCH_DEVICE",
        995 => "ERROR_OPERATION_ABORTED",
        1117 => "ERROR_IO_DEVICE",
        1167 => "ERROR_DEVICE_NOT_CONNECTED",
        1617 => "ERROR_DEVICE_REMOVED",
        _ => "",
    }
}

/// Builds the coded error for a failed HID operation.
pub fn io_error(op: &str, win32: Option<u32>, msg: &str, opening: bool) -> anyhow::Error {
    let code = classify_win32(win32, opening);
    let detail = match win32 {
        Some(w) if !win32_name(w).is_empty() => format!("{op} failed: {msg} (Win32 {w} {})", win32_name(w)),
        Some(w) => format!("{op} failed: {msg} (Win32 {w})"),
        None => format!("{op} failed: {msg}"),
    };
    err(code, detail)
}

/// The Win32 error behind a hidapi error, if any.
pub fn hid_win32(e: &hidapi::HidError) -> Option<u32> {
    match e {
        hidapi::HidError::HidApiError { message } => parse_win32(message),
        hidapi::HidError::IoError { error } => error.raw_os_error().map(|c| c as u32),
        _ => None,
    }
}

fn hid_error(op: &str, e: &hidapi::HidError, opening: bool) -> anyhow::Error {
    io_error(op, hid_win32(e), &e.to_string(), opening)
}

// ---------------------------------------------------------------------------------------
// hidapi implementation

pub struct HidApiTransport {
    dev: hidapi::HidDevice,
}

impl HidApiTransport {
    pub fn new(dev: hidapi::HidDevice) -> Self {
        HidApiTransport { dev }
    }
}

impl HidTransport for HidApiTransport {
    fn send_feature(&self, data: &[u8]) -> Result<()> {
        self.dev.send_feature_report(data).map_err(|e| hid_error("HID feature write", &e, false))
    }

    fn get_feature(&self, buf: &mut [u8]) -> Result<usize> {
        self.dev.get_feature_report(buf).map_err(|e| hid_error("HID feature read", &e, false))
    }
}

// ---------------------------------------------------------------------------------------
// Device discovery

/// One HID collection that may be the controller.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub path: CString,
    pub pid: u16,
}

/// Finds and opens controllers. Owned by the I/O worker thread.
pub trait Connector: Send {
    /// Present collections with VID 048D, a known PID, usage page 0xFF89 and usage 0xCC.
    fn candidates(&mut self) -> Result<Vec<Candidate>>;
    fn open(&mut self, c: &Candidate) -> Result<Box<dyn HidTransport>>;
    /// Windows' problem code (43 = "device descriptor request failed", ...) for the controller's
    /// USB port, if the device failed to enumerate. Does not touch the device itself.
    fn usb_problem(&mut self) -> Option<u32>;
}

/// The real connector over hidapi + the CfgMgr32 USB health check.
#[derive(Default)]
pub struct HidApiConnector {
    api: Option<hidapi::HidApi>,
}

/// Only the vendor collection (UP 0xFF89, U 0xCC) takes 0xCC reports; the IT5702's second MI_01
/// collection and the LampArray interface must never be used.
pub fn is_controller_collection(vid: u16, pid: u16, usage_page: u16, usage: u16) -> bool {
    vid == VID_ITE && PIDS.contains(&pid) && usage_page == USAGE_PAGE && usage == USAGE
}

/// Opens hidapi without enumerating every HID device on the system.
pub fn new_hidapi() -> Result<hidapi::HidApi> {
    hidapi::HidApi::disable_device_discovery();
    hidapi::HidApi::new().map_err(|e| err(ErrorCode::RgbOpenFailed, format!("HID subsystem unavailable: {e}")))
}

impl Connector for HidApiConnector {
    fn candidates(&mut self) -> Result<Vec<Candidate>> {
        let api = match self.api.as_mut() {
            Some(a) => a,
            None => self.api.insert(new_hidapi()?),
        };
        let _ = api.reset_devices();
        for pid in PIDS {
            let _ = api.add_devices(VID_ITE, pid);
        }
        let mut out: Vec<Candidate> = api
            .device_list()
            .filter(|d| is_controller_collection(d.vendor_id(), d.product_id(), d.usage_page(), d.usage()))
            .map(|d| Candidate { path: d.path().to_owned(), pid: d.product_id() })
            .collect();
        out.dedup();
        Ok(out)
    }

    fn open(&mut self, c: &Candidate) -> Result<Box<dyn HidTransport>> {
        let api = match self.api.as_mut() {
            Some(a) => a,
            None => self.api.insert(new_hidapi()?),
        };
        let dev = api.open_path(&c.path).map_err(|e| hid_error("opening the RGB controller", &e, true))?;
        Ok(Box::new(HidApiTransport::new(dev)))
    }

    fn usb_problem(&mut self) -> Option<u32> {
        crate::diag::rgb_usb_health().problem_code()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::code_of;

    #[test]
    fn win32_parsing() {
        assert_eq!(parse_win32("HidD_SetFeature: (0x0000048F) The device is not connected."), Some(1167));
        assert_eq!(parse_win32("CreateFile: (0x00000005) Access is denied."), Some(5));
        assert_eq!(parse_win32("x (0x8007001F) y"), Some(31));
        assert_eq!(parse_win32("no code"), None);
        assert_eq!(parse_win32("(0x12"), None);
    }

    #[test]
    fn win32_classification() {
        for c in [2, 3, 6, 995, 1167] {
            assert_eq!(classify_win32(Some(c), false), ErrorCode::RgbDisconnected, "{c}");
        }
        assert_eq!(classify_win32(Some(5), true), ErrorCode::RgbOpenFailed);
        assert_eq!(classify_win32(Some(32), false), ErrorCode::RgbOpenFailed);
        assert_eq!(classify_win32(Some(31), false), ErrorCode::RgbWriteFailed);
        assert_eq!(classify_win32(None, false), ErrorCode::RgbWriteFailed);
        assert_eq!(classify_win32(Some(1), true), ErrorCode::RgbOpenFailed);
        let e = io_error("HID feature write", Some(1167), "gone", false);
        assert_eq!(code_of(&e), ErrorCode::RgbDisconnected);
        assert!(e.to_string().contains("ERROR_DEVICE_NOT_CONNECTED"));
        let hid = hidapi::HidError::HidApiError {
            message: "HidD_SetFeature: (0x0000001F) A device attached to the system is not functioning.".into(),
        };
        assert_eq!(code_of(&hid_error("w", &hid, false)), ErrorCode::RgbWriteFailed);
        let io = hidapi::HidError::IoError { error: std::io::Error::from_raw_os_error(5) };
        assert_eq!(code_of(&hid_error("open", &io, true)), ErrorCode::RgbOpenFailed);
    }

    #[test]
    fn only_vendor_collection_is_accepted() {
        assert!(is_controller_collection(0x048D, 0x5702, 0xFF89, 0xCC));
        assert!(!is_controller_collection(0x048D, 0x5702, 0xFF89, 0x10)); // F1: no fallback
        assert!(!is_controller_collection(0x048D, 0x5702, 0x59, 0x01)); // LampArray
        assert!(!is_controller_collection(0x1234, 0x5702, 0xFF89, 0xCC));
    }
}
