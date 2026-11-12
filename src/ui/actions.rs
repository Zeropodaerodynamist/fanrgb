//! UI actions shared by keyboard shortcuts, the command palette, toasts and issue fixes.

use crate::config::{EffectKind, Preset};
use crate::error::FixAction;

use super::Page;
use super::icons::Icon;
use super::theme::ThemePref;

#[derive(Clone, Debug, PartialEq)]
pub enum UiAction {
    Navigate(Page),
    SelectFan(String),
    SelectZone(String),
    SetProfile(usize),
    /// Undo for a profile switch from the top bar.
    RestoreProfile(usize),
    /// Fan mode tile / Alt+1..6 / palette: apply a built-in preset in one step.
    ApplyPreset(Preset),
    /// Hotkey (Ctrl+Shift+M) / palette: the next fan mode.
    CyclePreset,
    /// Header identification wizard.
    IdentifyHeaders,
    SetEffect(EffectKind),
    EmergencyRelease,
    Resume,
    WriteDiagnostics,
    CopyReport,
    ReconnectRgb,
    ToggleLighting,
    OpenPalette,
    OpenProfileMenu,
    ShowShortcuts,
    Fix(FixAction, Option<String>),
    OpenLogFolder,
    SetTheme(ThemePref),
    ResetSettings,
    RestartApp,
    InstallPawnIo,
    FocusSearch,
    Calibrate(usize),
    /// Updates: check the app (and drivers when enabled) now.
    CheckForUpdates,
    /// Updates: install the verified release and restart (only offered when one is ready).
    InstallUpdate,
    /// Release notes of the available update.
    ShowWhatsNew,
    /// Lighting › Colour calibration for a zone.
    CalibrateColours(String),
    /// Windows Settings › Personalization › Dynamic Lighting.
    OpenDynamicLightingSettings,
}

pub struct PaletteEntry {
    pub icon: Icon,
    pub group: &'static str,
    pub title: String,
    pub context: String,
    pub hint: &'static str,
    pub action: UiAction,
}

/// Fuzzy subsequence match: every query char must appear in order. Higher is better; word-start
/// and consecutive matches score a bonus, earlier matches beat later ones.
pub fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    let q: Vec<char> = query.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    if q.is_empty() {
        return Some(0);
    }
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let mut score = 0i32;
    let mut qi = 0;
    let mut prev_match: Option<usize> = None;
    for (i, ch) in t.iter().enumerate() {
        if qi < q.len() && *ch == q[qi] {
            let word_start = i == 0 || !t[i - 1].is_alphanumeric();
            score += 10;
            if word_start {
                score += 15;
            }
            if prev_match == Some(i.wrapping_sub(1)) {
                score += 8;
            }
            score -= (i as i32).min(20) / 4;
            prev_match = Some(i);
            qi += 1;
        }
    }
    if qi == q.len() { Some(score) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_matches_subsequences() {
        assert!(fuzzy_score("sf1", "SYS_FAN1").is_some());
        assert!(fuzzy_score("xyz", "SYS_FAN1").is_none());
        assert_eq!(fuzzy_score("", "anything"), Some(0));
        // word starts beat mid-word matches
        let a = fuzzy_score("rb", "Return to BIOS").unwrap();
        let b = fuzzy_score("rb", "Turbo").unwrap();
        assert!(a > b, "{a} vs {b}");
        // prefix beats later occurrence
        let c = fuzzy_score("cool", "Cooling").unwrap();
        let d = fuzzy_score("cool", "Show unused cooling headers").unwrap();
        assert!(c > d);
    }
}
