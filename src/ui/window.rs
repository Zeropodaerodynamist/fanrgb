//! Main-window state that eframe does not manage: "closed to tray", show requests from other
//! threads, on-screen visibility for repaint decisions, the saved window placement and the size
//! guard for the OpenGL driver.
//!
//! **Tray mode is "minimized + `WS_EX_TOOLWINDOW`", not `SW_HIDE`.** A hidden window never receives
//! `WM_PAINT`, so a repaint that is due while it is hidden leaves eframe's event loop in
//! `ControlFlow::Poll` forever (measured 80-90 % of a core). A minimized window is
//! still visible to Windows: a pending repaint produces one cheap frame and eframe goes back to
//! `ControlFlow::Wait`. The tool-window style keeps it off the taskbar and out of Alt+Tab, and
//! Explorer parks minimized windows off-screen.
//!
//! All transitions run on the UI thread, in a window subclass, so they never race eframe/winit.
//! While in tray mode the subclass also keeps winit from re-applying the taskbar styles.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};

use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONULL, MONITORINFO, MonitorFromRect, RDW_INTERNALPAINT, RedrawWindow,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, GWL_EXSTYLE, GWL_STYLE, GWLP_WNDPROC, GetForegroundWindow, GetWindowLongPtrW, GetWindowPlacement,
    IsIconic, IsWindow, IsWindowVisible, IsZoomed, PostMessageW, SC_RESTORE, STYLESTRUCT, SW_HIDE, SW_RESTORE,
    SW_SHOWMINNOACTIVE, SWP_NOACTIVATE, SWP_NOSIZE, SWP_SHOWWINDOW, SendMessageW, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPlacement, ShowWindow, WINDOWPLACEMENT, WINDOWPOS, WM_APP, WM_STYLECHANGING,
    WM_SYSCOMMAND, WM_WINDOWPOSCHANGING, WNDPROC, WPF_RESTORETOMAXIMIZED, WPF_SETMINPOSITION, WS_EX_APPWINDOW,
    WS_EX_TOOLWINDOW, WS_MINIMIZE, WS_VISIBLE,
};

/// Largest window side in physical pixels. The Intel Arc OpenGL driver crashes the process when a
/// GL surface side reaches 16384 px; 8192 also matches wgpu's default limit.
pub const MAX_SIDE_PX: f32 = 8192.0;

const WM_APP_SHOW: u32 = WM_APP + 0x31;
const WM_APP_TRAY: u32 = WM_APP + 0x32;

static HWND_MAIN: AtomicIsize = AtomicIsize::new(0);
static ORIG_PROC: AtomicIsize = AtomicIsize::new(0);
/// `--minimized`: swallow eframe's first `ShowWindow` and go straight to tray mode.
static START_IN_TRAY: AtomicBool = AtomicBool::new(false);
static IN_TRAY: AtomicBool = AtomicBool::new(false);
/// `WS_EX_APPWINDOW` was set before entering tray mode (restored on the way out).
static HAD_APPWINDOW: AtomicBool = AtomicBool::new(false);
/// Foreground window when we started in tray mode: gets the focus back if Windows activates our
/// (invisible) window while it starts.
static PREV_FOREGROUND: AtomicIsize = AtomicIsize::new(0);
/// Counts transitions (tests / diagnostics).
static TRANSITIONS: AtomicU32 = AtomicU32::new(0);

fn hwnd() -> HWND {
    HWND_MAIN.load(Ordering::SeqCst) as HWND
}

/// Installs the subclass. Call once from `App::new`, on the UI thread, before the first frame.
pub fn init(hwnd: isize, start_in_tray: bool) {
    if hwnd == 0 || HWND_MAIN.swap(hwnd, Ordering::SeqCst) != 0 {
        return;
    }
    START_IN_TRAY.store(start_in_tray, Ordering::SeqCst);
    let h = hwnd as HWND;
    // SAFETY: `h` is our live top-level window, owned by this (UI) thread; the previous procedure is
    // stored before any message can reach `subclass_proc`.
    unsafe {
        let orig = GetWindowLongPtrW(h, GWLP_WNDPROC);
        ORIG_PROC.store(orig, Ordering::SeqCst);
        SetWindowLongPtrW(h, GWLP_WNDPROC, subclass_proc as *const () as isize);
        if start_in_tray {
            set_tray_styles(h, true);
            let fg = GetForegroundWindow();
            if fg != h {
                PREV_FOREGROUND.store(fg as isize, Ordering::SeqCst);
            }
        }
    }
}

/// Brings the window back from the tray / minimized state and focuses it. Any thread.
pub fn request_show() {
    let h = hwnd();
    if !h.is_null() {
        // SAFETY: posting to a window handle is always safe (a stale handle just fails).
        unsafe { PostMessageW(h, WM_APP_SHOW, 0, 0) };
    }
}

/// Closes the window to the tray. Any thread.
pub fn request_tray() {
    let h = hwnd();
    if !h.is_null() {
        // SAFETY: as above.
        unsafe { PostMessageW(h, WM_APP_TRAY, 0, 0) };
    }
}

pub fn in_tray() -> bool {
    IN_TRAY.load(Ordering::SeqCst) || START_IN_TRAY.load(Ordering::SeqCst)
}

/// The window is on screen: visible, not minimized, not in the tray.
pub fn shown() -> bool {
    let h = hwnd();
    if h.is_null() || in_tray() {
        return false;
    }
    // SAFETY: plain queries on a window handle.
    unsafe { IsWindowVisible(h) != 0 && IsIconic(h) == 0 }
}

/// Someone restored the window behind our back (e.g. an old `ShowWindow(SW_RESTORE)` caller) while
/// it was in tray mode: give it its taskbar button back.
pub fn repair_if_restored_externally() {
    let h = hwnd();
    // SAFETY: plain queries.
    if !h.is_null() && IN_TRAY.load(Ordering::SeqCst) && unsafe { IsWindowVisible(h) != 0 && IsIconic(h) == 0 } {
        leave_tray(h);
    }
}

/// # Safety
/// `h` must be our window, called on its thread.
unsafe fn set_tray_styles(h: HWND, tray: bool) {
    // SAFETY: caller guarantees a valid window on this thread.
    unsafe {
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
        let new = if tray {
            HAD_APPWINDOW.store(ex & WS_EX_APPWINDOW != 0, Ordering::SeqCst);
            (ex | WS_EX_TOOLWINDOW) & !WS_EX_APPWINDOW
        } else {
            let app = if HAD_APPWINDOW.load(Ordering::SeqCst) { WS_EX_APPWINDOW } else { 0 };
            (ex & !WS_EX_TOOLWINDOW) | app
        };
        if new != ex {
            SetWindowLongPtrW(h, GWL_EXSTYLE, new as isize);
        }
    }
}

fn enter_tray(h: HWND) {
    START_IN_TRAY.store(false, Ordering::SeqCst);
    IN_TRAY.store(true, Ordering::SeqCst);
    TRANSITIONS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: called from `subclass_proc`, i.e. on the window's thread with a valid handle.
    unsafe {
        let zoomed = IsZoomed(h) != 0;
        // Hide first: the taskbar only re-reads the style when a window is shown.
        ShowWindow(h, SW_HIDE);
        set_tray_styles(h, true);
        // Minimized tool windows have no taskbar button to collapse into, so Windows parks them as a
        // small caption bar above the taskbar. Put the iconic window far off-screen instead.
        let mut wp: WINDOWPLACEMENT = std::mem::zeroed();
        wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
        let placed = GetWindowPlacement(h, &mut wp) != 0 && {
            wp.flags = WPF_SETMINPOSITION | if zoomed { WPF_RESTORETOMAXIMIZED } else { 0 };
            wp.ptMinPosition = POINT { x: -32000, y: -32000 };
            wp.showCmd = SW_SHOWMINNOACTIVE as u32;
            SetWindowPlacement(h, &wp) != 0
        };
        if !placed {
            ShowWindow(h, SW_SHOWMINNOACTIVE);
        }
        // An invisible window must not keep the keyboard focus (start-up activation).
        let prev = PREV_FOREGROUND.swap(0, Ordering::SeqCst) as HWND;
        if GetForegroundWindow() == h && !prev.is_null() && IsWindow(prev) != 0 {
            SetForegroundWindow(prev);
        }
        // Make sure eframe gets one WM_PAINT (and with it ControlFlow::Wait) in the new state.
        RedrawWindow(h, std::ptr::null(), std::ptr::null_mut(), RDW_INTERNALPAINT);
    }
}

fn leave_tray(h: HWND) {
    let was_tray = IN_TRAY.swap(false, Ordering::SeqCst) | START_IN_TRAY.swap(false, Ordering::SeqCst);
    TRANSITIONS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: as in `enter_tray`.
    unsafe {
        if was_tray {
            ShowWindow(h, SW_HIDE);
            set_tray_styles(h, false);
            ShowWindow(h, SW_RESTORE);
        } else if IsIconic(h) != 0 {
            // SC_RESTORE (not ShowWindow) so winit's own "minimized" flag is cleared too.
            SendMessageW(h, WM_SYSCOMMAND, SC_RESTORE as WPARAM, 0);
        } else if IsWindowVisible(h) == 0 {
            ShowWindow(h, SW_RESTORE);
        }
        SetForegroundWindow(h);
        RedrawWindow(h, std::ptr::null(), std::ptr::null_mut(), RDW_INTERNALPAINT);
    }
}

unsafe extern "system" fn subclass_proc(h: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_APP_SHOW => {
            leave_tray(h);
            return 0;
        }
        WM_APP_TRAY => {
            if !IN_TRAY.load(Ordering::SeqCst) {
                enter_tray(h);
            }
            return 0;
        }
        WM_WINDOWPOSCHANGING if lparam != 0 => {
            // SAFETY: for WM_WINDOWPOSCHANGING, lparam points to a WINDOWPOS owned by the caller.
            let wp = unsafe { &mut *(lparam as *mut WINDOWPOS) };
            if START_IN_TRAY.load(Ordering::SeqCst) && wp.flags & SWP_SHOWWINDOW != 0 {
                // eframe shows the window after its first frame: stay hidden and enter the tray instead.
                wp.flags = (wp.flags & !SWP_SHOWWINDOW) | SWP_NOACTIVATE;
                // SAFETY: posting to our own window.
                unsafe { PostMessageW(h, WM_APP_TRAY, 0, 0) };
            }
            let r = call_orig(h, msg, wparam, lparam);
            if wp.flags & SWP_NOSIZE == 0 {
                let max = MAX_SIDE_PX as i32;
                wp.cx = wp.cx.min(max);
                wp.cy = wp.cy.min(max);
            }
            return r;
        }
        WM_STYLECHANGING if lparam != 0 => {
            // SAFETY: for WM_STYLECHANGING, lparam points to a STYLESTRUCT owned by the caller.
            let ss = unsafe { &mut *(lparam as *mut STYLESTRUCT) };
            let starting = START_IN_TRAY.load(Ordering::SeqCst);
            let tray = IN_TRAY.load(Ordering::SeqCst);
            if wparam as i32 == GWL_EXSTYLE && (starting || tray) {
                ss.styleNew = (ss.styleNew | WS_EX_TOOLWINDOW) & !WS_EX_APPWINDOW;
            } else if wparam as i32 == GWL_STYLE {
                if starting {
                    ss.styleNew &= !WS_VISIBLE;
                }
                if tray {
                    ss.styleNew = (ss.styleNew & !WS_MINIMIZE) | (ss.styleOld & WS_MINIMIZE);
                }
            }
        }
        _ => {}
    }
    call_orig(h, msg, wparam, lparam)
}

fn call_orig(h: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let orig = ORIG_PROC.load(Ordering::SeqCst);
    // SAFETY: `orig` is the window procedure that was installed before ours (winit's); WNDPROC is an
    // Option of a function pointer, so the transmute of a non-zero value is the documented pattern.
    unsafe {
        let proc: WNDPROC = std::mem::transmute::<isize, WNDPROC>(orig);
        CallWindowProcW(proc, h, msg, wparam, lparam)
    }
}

// ---------------------------------------------------------------------------------------
// Placement persistence (eframe's own saves garbage while the window is minimized / in the tray)

/// Restored (normal) window rectangle in physical pixels, plus the maximized flag.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Placement {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub maximized: bool,
}

impl Placement {
    /// A size the window may be restored to (not degenerate, not over the GL limit).
    pub fn sane(&self) -> bool {
        let (w, h) = (self.right - self.left, self.bottom - self.top);
        (600..=MAX_SIDE_PX as i32).contains(&w) && (400..=MAX_SIDE_PX as i32).contains(&h)
    }
}

pub fn placement() -> Option<Placement> {
    let h = hwnd();
    if h.is_null() {
        return None;
    }
    let mut wp: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
    wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
    // SAFETY: `wp` is a valid, sized out structure.
    if unsafe { GetWindowPlacement(h, &mut wp) } == 0 {
        return None;
    }
    let r = wp.rcNormalPosition;
    let maximized = wp.showCmd == 3 /* SW_SHOWMAXIMIZED */
        || (wp.showCmd == 2 /* SW_SHOWMINIMIZED */ && wp.flags & WPF_RESTORETOMAXIMIZED != 0);
    let p = Placement { left: r.left, top: r.top, right: r.right, bottom: r.bottom, maximized };
    p.sane().then_some(p)
}

/// Applies a saved placement to the still-hidden window (before eframe shows it). Skipped when the
/// rectangle is not on any monitor any more. Returns true if applied.
pub fn restore_placement(p: &Placement) -> bool {
    let h = hwnd();
    if h.is_null() || !p.sane() {
        return false;
    }
    let mut rect = RECT { left: p.left, top: p.top, right: p.right, bottom: p.bottom };
    // SAFETY: plain query with a valid RECT.
    let mon = unsafe { MonitorFromRect(&rect, MONITOR_DEFAULTTONULL) };
    if mon.is_null() {
        return false;
    }
    // Never restore a window larger than the monitor's work area it is on.
    let mut mi: MONITORINFO = unsafe { std::mem::zeroed() };
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    // SAFETY: valid monitor handle and sized out structure.
    if unsafe { GetMonitorInfoW(mon, &mut mi) } != 0 {
        rect = fit_into(rect, mi.rcWork);
    }
    let mut wp: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
    wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
    // SAFETY: valid out structure.
    if unsafe { GetWindowPlacement(h, &mut wp) } == 0 {
        return false;
    }
    wp.rcNormalPosition = rect;
    wp.showCmd = SW_HIDE as u32;
    wp.flags = if p.maximized { WPF_RESTORETOMAXIMIZED } else { 0 };
    // SAFETY: valid structure; SW_HIDE keeps the window hidden.
    unsafe { SetWindowPlacement(h, &wp) != 0 }
}

/// Shrinks `r` to fit `work` and moves it inside (keeps the size when it fits).
fn fit_into(r: RECT, work: RECT) -> RECT {
    let w = (r.right - r.left).min(work.right - work.left);
    let h = (r.bottom - r.top).min(work.bottom - work.top);
    let left = r.left.clamp(work.left, work.right - w);
    let top = r.top.clamp(work.top, work.bottom - h);
    RECT { left, top, right: left + w, bottom: top + h }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_sanity() {
        let p = Placement { left: 100, top: 100, right: 1380, bottom: 920, maximized: false };
        assert!(p.sane());
        assert!(!Placement { right: 164, bottom: 164, ..p }.sane(), "minimized-size garbage");
        assert!(!Placement { right: 100 + 16384, ..p }.sane(), "GL driver limit");
        assert!(!Placement { left: -32000, top: -32000, right: -31840, bottom: -31973, ..p }.sane());
    }

    #[test]
    fn placement_is_fitted_into_the_work_area() {
        let work = RECT { left: 0, top: 0, right: 2560, bottom: 1392 };
        let r = fit_into(RECT { left: 40, top: 20, right: 1640, bottom: 8212 }, work);
        assert_eq!((r.left, r.top, r.right, r.bottom), (40, 0, 1640, 1392));
        let r = fit_into(RECT { left: 2000, top: 100, right: 3600, bottom: 1100 }, work);
        assert_eq!((r.left, r.right), (960, 2560));
        let ok = RECT { left: 100, top: 100, right: 1380, bottom: 920 };
        let r = fit_into(ok, work);
        assert_eq!((r.left, r.top, r.right, r.bottom), (100, 100, 1380, 920));
    }

    #[test]
    fn no_window_means_not_shown() {
        // No window registered in unit tests: everything is a safe no-op.
        assert!(!shown());
        request_show();
        request_tray();
        assert!(placement().is_none());
        let _ = TRANSITIONS.load(Ordering::SeqCst);
    }
}
