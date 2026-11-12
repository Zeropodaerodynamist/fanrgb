//! Critical alerts as a notification from the tray icon, so they reach the
//! user while the window is hidden. Uses the legacy `Shell_NotifyIcon` balloon (`NIF_INFO`), which
//! Windows 10/11 shows as a toast and which works for an unpackaged app without an AppUserModelID.
//!
//! [`Notifier`] decides what to show (pure, tested): one notification when a critical condition
//! starts, never repeated for the same code within [`PER_CODE`], never two within [`MIN_GAP`]
//! (what is suppressed by either limit is shown once it is over, if it is still active).
//! [`show`] hands it to the shell on a short-lived thread: `Shell_NotifyIcon` sends a message to
//! Explorer and must never be able to stall the engine thread.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use crate::error::{ErrorCode, Severity};
use crate::model::Alert;

/// The same code notifies at most this often.
pub const PER_CODE: Duration = Duration::from_secs(15 * 60);
/// Minimum time between two notifications.
pub const MIN_GAP: Duration = Duration::from_secs(30);

/// Window of the tray icon (set by the tray; 0 = no tray icon: nothing is shown).
static TARGET: AtomicIsize = AtomicIsize::new(0);

/// The tray icon's window (`TrayIcon::window_handle`).
pub fn set_target(hwnd: isize) {
    TARGET.store(hwnd, Ordering::SeqCst);
}

/// Alerts that notify: every critical one (FRG-E301 critical temperature, E305 / E309 cooling,
/// E205 restore failed, E207 fan stalled) and a lost sensor (E300).
pub fn notifies(code: ErrorCode) -> bool {
    code.severity() == Severity::Critical || code == ErrorCode::SensorLost
}

/// One notification.
#[derive(Clone, Debug, PartialEq)]
pub struct Balloon {
    pub code: ErrorCode,
    pub title: String,
    pub text: String,
    pub critical: bool,
}

#[derive(Default)]
pub struct Notifier {
    /// Codes active on the last update.
    active: HashSet<ErrorCode>,
    /// Active, not yet shown in this episode.
    pending: HashSet<ErrorCode>,
    last_by_code: HashMap<ErrorCode, Instant>,
    last_any: Option<Instant>,
}

impl Notifier {
    /// Feeds this tick's alerts; returns the notification to show now, if any.
    pub fn update(&mut self, now: Instant, alerts: &[Alert], enabled: bool) -> Option<Balloon> {
        let current: HashSet<ErrorCode> = alerts.iter().filter_map(|a| a.code).filter(|c| notifies(*c)).collect();
        for c in &current {
            if !self.active.contains(c) {
                self.pending.insert(*c);
            }
        }
        self.pending.retain(|c| current.contains(c));
        self.active = current;
        if !enabled {
            self.pending.clear();
            return None;
        }
        // Codes notified recently wait: a new episode within [`PER_CODE`] stays pending and is shown
        // once that time is over, if it is still active (it used to be dropped
        // for the whole episode).
        let recent = |c: &ErrorCode| self.last_by_code.get(c).is_some_and(|t| now.duration_since(*t) < PER_CODE);
        let due: Vec<ErrorCode> = self.pending.iter().copied().filter(|c| !recent(c)).collect();
        if due.is_empty() || self.last_any.is_some_and(|t| now.duration_since(t) < MIN_GAP) {
            return None;
        }
        let rank = |c: &ErrorCode| (c.severity(), std::cmp::Reverse(c.number()));
        let code = *due.iter().max_by_key(|c| rank(c))?;
        let alert = alerts.iter().find(|a| a.code == Some(code))?;
        for c in &due {
            self.pending.remove(c);
            self.last_by_code.insert(*c, now);
        }
        self.last_any = Some(now);
        let more = if due.len() > 1 { format!(" (+{} more alert(s) in FanRGB)", due.len() - 1) } else { String::new() };
        Some(Balloon {
            code,
            title: truncate(&alert.title, 63),
            text: truncate(&format!("{}{more} [{code}]", alert.detail), 255),
            critical: code.severity() == Severity::Critical,
        })
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { format!("{}…", s.chars().take(max - 1).collect::<String>()) }
}

/// Shows `b` from the tray icon (asynchronously). Returns false if there is no tray icon.
pub fn show(b: Balloon) -> bool {
    let hwnd = TARGET.load(Ordering::SeqCst);
    if hwnd == 0 {
        return false;
    }
    log::info!("[{}] notification: {}", b.code, b.title);
    let spawned = std::thread::Builder::new().name("fanrgb-notify".into()).spawn(move || {
        if !show_now(hwnd, &b) {
            log::warn!("tray notification for {} could not be shown", b.code);
        }
    });
    spawned.is_ok()
}

#[cfg(windows)]
fn show_now(hwnd: isize, b: &Balloon) -> bool {
    use windows_sys::Win32::UI::Shell::{
        NIF_INFO, NIIF_ERROR, NIIF_WARNING, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW,
    };
    fn copy(dst: &mut [u16], s: &str) {
        let w: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
        dst[..w.len()].copy_from_slice(&w);
        dst[w.len()] = 0;
    }
    // SAFETY: NOTIFYICONDATAW is plain data; all-zero is a valid initial value.
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd as _;
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = if b.critical { NIIF_ERROR } else { NIIF_WARNING };
    copy(&mut nid.szInfoTitle, &b.title);
    copy(&mut nid.szInfo, &b.text);
    // tray-icon numbers its icons from 1 and does not expose the id; the window is the icon's own,
    // so the first id the shell accepts for it is ours.
    (1..=8).any(|id| {
        nid.uID = id;
        // SAFETY: `nid` is fully initialised and lives for the call.
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) != 0 }
    })
}

#[cfg(not(windows))]
fn show_now(_hwnd: isize, _b: &Balloon) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(code: ErrorCode) -> Alert {
        Alert::coded(code, format!("detail of {code}"))
    }

    #[test]
    fn notifies_once_per_episode_with_rate_limits() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut n = Notifier::default();
        assert_eq!(n.update(at(0), &[alert(ErrorCode::FanReadOnly)], true), None, "not a notifying code");
        let b = n.update(at(1), &[alert(ErrorCode::CoolingIneffective)], true).expect("new critical alert");
        assert_eq!(b.code, ErrorCode::CoolingIneffective);
        assert!(b.critical && b.text.contains("FRG-E305") && b.title == ErrorCode::CoolingIneffective.title());
        assert_eq!(n.update(at(2), &[alert(ErrorCode::CoolingIneffective)], true), None, "same episode");
        // Gone and back within 15 min: quiet.
        assert_eq!(n.update(at(3), &[], true), None);
        assert_eq!(n.update(at(100), &[alert(ErrorCode::CoolingIneffective)], true), None);
        // A second code within 30 s of the last notification waits for the gap.
        let mut m = Notifier::default();
        assert!(m.update(at(0), &[alert(ErrorCode::CriticalTemperature)], true).is_some());
        let both = [alert(ErrorCode::CriticalTemperature), alert(ErrorCode::SensorLost)];
        assert_eq!(m.update(at(10), &both, true), None, "gap");
        let b = m.update(at(31), &both, true).expect("shown after the gap while still active");
        assert_eq!((b.code, b.critical), (ErrorCode::SensorLost, false));
        // Suppressed by the gap and resolved meanwhile: dropped.
        let mut k = Notifier::default();
        assert!(k.update(at(0), &[alert(ErrorCode::CoolingDegraded)], true).is_some());
        assert_eq!(k.update(at(5), &[alert(ErrorCode::FanStalled)], true), None);
        assert_eq!(k.update(at(40), &[], true), None);
        // After 15 min the same code notifies again.
        assert!(n.update(at(1 + 15 * 60), &[], true).is_none());
        assert!(n.update(at(2 + 15 * 60), &[alert(ErrorCode::CoolingIneffective)], true).is_some());
    }

    /// An episode that returns within 15 min of the last notification and
    /// stays is notified once the 15 min are over, not dropped.
    #[test]
    fn recurring_episode_notifies_after_the_per_code_limit() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let crit = [alert(ErrorCode::CriticalTemperature)];
        let mut n = Notifier::default();
        assert!(n.update(at(0), &crit, true).is_some());
        assert_eq!(n.update(at(60), &[], true), None, "cleared");
        assert_eq!(n.update(at(300), &crit, true), None, "back within 15 min: waits");
        assert_eq!(n.update(at(899), &crit, true), None);
        let b = n.update(at(900), &crit, true).expect("still active after 15 min: notified");
        assert_eq!(b.code, ErrorCode::CriticalTemperature);
        assert_eq!(n.update(at(2000), &crit, true), None, "once per episode");
    }

    #[test]
    fn most_severe_first_and_disabled_means_silent() {
        let t0 = Instant::now();
        let mut n = Notifier::default();
        let b = n.update(t0, &[alert(ErrorCode::SensorLost), alert(ErrorCode::CoolingDegraded)], true).unwrap();
        assert_eq!(b.code, ErrorCode::CoolingDegraded);
        assert!(b.text.contains("+1 more"), "{}", b.text);
        let mut off = Notifier::default();
        assert_eq!(off.update(t0, &[alert(ErrorCode::CriticalTemperature)], false), None);
        assert_eq!(
            off.update(t0 + Duration::from_secs(60), &[alert(ErrorCode::CriticalTemperature)], true),
            None,
            "episode began while disabled"
        );
        assert!(
            notifies(ErrorCode::RestoreFailed)
                && notifies(ErrorCode::FanStalled)
                && !notifies(ErrorCode::ProfileSwitched)
        );
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert!(!show(b), "no tray icon in tests");
    }
}
