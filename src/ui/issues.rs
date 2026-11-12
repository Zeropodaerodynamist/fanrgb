//! UI-side issue tracker: diffs engine alerts (and RGB / UI-side problems) by `(code, subject)`,
//! keeps first/last seen + counts, an event history, and maps `FixAction`s to real actions.
//! The UI never string-matches alert titles to choose a fix.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::SystemTime;

use crate::config::Config;
use crate::engine::{Command, EngineState, Shared};
use crate::error::{ErrorCode, FixAction, Severity};
use crate::model::Alert;
use crate::rgb::{AudioState, RgbStatus};

const EVENT_CAP: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Engine,
    Rgb,
}

#[derive(Clone, Debug)]
pub struct Issue {
    pub code: ErrorCode,
    pub subject: Option<String>,
    pub title: String,
    pub detail: String,
    pub first_seen: SystemTime,
    pub last_seen: SystemTime,
    pub count: u32,
    pub source: Source,
}

impl Issue {
    pub fn severity(&self) -> Severity {
        self.code.severity()
    }
    pub fn key(&self) -> (ErrorCode, Option<String>) {
        (self.code, self.subject.clone())
    }
    /// The page this issue belongs to (for page banners).
    pub fn area(&self) -> Area {
        match self.code.number() {
            100..=299 => Area::Cooling,
            300..=399 => Area::Sensors,
            400..=499 => Area::Lighting,
            500..=599 => Area::Intelligence,
            _ => Area::System,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Area {
    Cooling,
    Sensors,
    Lighting,
    Intelligence,
    System,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventKind {
    Appeared(Severity),
    Resolved,
    Profile,
    Emergency,
    Info,
    Failure,
}

#[derive(Clone, Debug)]
pub struct Event {
    pub at: SystemTime,
    pub kind: EventKind,
    pub code: Option<ErrorCode>,
    pub text: String,
}

/// A change the shell turns into a toast.
#[derive(Clone, Debug)]
pub enum Transition {
    Appeared(Issue),
    Resolved(Issue),
}

#[derive(Default)]
pub struct IssueTracker {
    pub active: Vec<Issue>,
    pub events: VecDeque<Event>,
    snoozed: HashSet<(ErrorCode, Option<String>)>,
    counts: std::collections::HashMap<(ErrorCode, Option<String>), u32>,
    started: bool,
}

/// Subject parsed from a customised title ("SYS_FAN2 is not spinning" → "SYS_FAN2").
pub fn subject_of(a: &Alert) -> Option<String> {
    let code = a.code?;
    if a.title == code.title() {
        return None;
    }
    a.title.split(" is ").next().filter(|s| !s.is_empty() && s.len() < a.title.len()).map(|s| s.to_string())
}

/// RGB-side problems from the RGB thread's connection state machine (coded by src/rgb).
pub fn rgb_issues(rgb: &RgbStatus, uses_music: bool, engine_reports_hung: bool) -> Vec<(ErrorCode, String)> {
    use crate::rgb::RgbConnState;
    let mut out = Vec::new();
    let detail = |fallback: ErrorCode| -> (ErrorCode, String) {
        match &rgb.last_error {
            Some(e) => (e.code, e.detail.clone()),
            None => (fallback, rgb.error.clone().unwrap_or_default()),
        }
    };
    match &rgb.state {
        RgbConnState::Connected | RgbConnState::Searching => {}
        RgbConnState::Hung => {
            if !engine_reports_hung {
                let (_, d) = detail(ErrorCode::RgbHung);
                out.push((ErrorCode::RgbHung, d));
            }
        }
        RgbConnState::InUseByOtherApp => {
            let (_, d) = detail(ErrorCode::RgbOpenFailed);
            out.push((ErrorCode::RgbOpenFailed, d));
        }
        RgbConnState::NotPresent => {
            if !engine_reports_hung {
                let (_, d) = detail(ErrorCode::RgbNotFound);
                out.push((ErrorCode::RgbNotFound, d));
            }
        }
        RgbConnState::Reconnecting { attempt, .. } => {
            let (code, d) = detail(ErrorCode::RgbDisconnected);
            let code = if code.number() / 100 == 4 { code } else { ErrorCode::RgbDisconnected };
            out.push((code, format!("{d} (reconnect attempt {attempt})")));
        }
    }
    if uses_music && let AudioState::Unavailable(why) = &rgb.audio {
        out.push((ErrorCode::AudioCapture, why.clone()));
    }
    // Windows Dynamic Lighting from the registry (never from the firmware byte alone).
    let dl = &rgb.dynamic_lighting;
    if dl.conflict() {
        out.push((ErrorCode::DynamicLightingActive, dl.summary()));
    }
    if let Some(e) = &dl.claim_error {
        out.push((ErrorCode::DynamicLightingClaim, e.clone()));
    }
    out
}

impl IssueTracker {
    pub fn log(&mut self, kind: EventKind, code: Option<ErrorCode>, text: impl Into<String>) {
        if self.events.len() >= EVENT_CAP {
            self.events.pop_front();
        }
        self.events.push_back(Event { at: SystemTime::now(), kind, code, text: text.into() });
    }

    /// Diffs the current alerts; returns appear/resolve transitions (none on the very first update,
    /// so startup state isn't toasted).
    pub fn update(&mut self, alerts: &[Alert], rgb: &[(ErrorCode, String)]) -> Vec<Transition> {
        let now = SystemTime::now();
        let mut seen: Vec<(ErrorCode, Option<String>)> = Vec::new();
        let mut out = Vec::new();
        let incoming = alerts
            .iter()
            .filter_map(|a| a.code.map(|c| (c, subject_of(a), a.title.clone(), a.detail.clone(), Source::Engine)))
            .chain(rgb.iter().map(|(c, d)| (*c, None, c.title().to_string(), d.clone(), Source::Rgb)));
        for (code, subject, title, detail, source) in incoming {
            let key = (code, subject.clone());
            if seen.contains(&key) {
                continue;
            }
            seen.push(key.clone());
            match self.active.iter_mut().find(|i| i.key() == key) {
                Some(i) => {
                    i.last_seen = now;
                    i.detail = detail;
                    i.title = title;
                }
                None => {
                    let count = self.counts.entry(key.clone()).or_insert(0);
                    *count += 1;
                    let issue =
                        Issue { code, subject, title, detail, first_seen: now, last_seen: now, count: *count, source };
                    self.log(EventKind::Appeared(code.severity()), Some(code), issue.title.clone());
                    if self.started {
                        out.push(Transition::Appeared(issue.clone()));
                    }
                    self.active.push(issue);
                }
            }
        }
        let mut resolved = Vec::new();
        self.active.retain(|i| {
            let keep = seen.contains(&i.key());
            if !keep {
                resolved.push(i.clone());
            }
            keep
        });
        for i in resolved {
            self.log(EventKind::Resolved, Some(i.code), format!("{} resolved", i.title));
            if self.started {
                out.push(Transition::Resolved(i));
            }
        }
        self.started = true;
        self.active.sort_by(|a, b| b.severity().cmp(&a.severity()).then(a.code.number().cmp(&b.code.number())));
        out
    }

    pub fn snooze(&mut self, i: &Issue) {
        self.snoozed.insert(i.key());
    }

    /// Visible (not snoozed) issues, most severe first.
    pub fn visible(&self) -> impl Iterator<Item = &Issue> {
        self.active.iter().filter(|i| !self.snoozed.contains(&i.key()))
    }

    /// Count for the top-bar badge and the sidebar (Info is not counted).
    pub fn badge_count(&self) -> usize {
        self.visible().filter(|i| i.severity() > Severity::Info).count()
    }

    pub fn max_severity(&self) -> Option<Severity> {
        self.visible().map(|i| i.severity()).max()
    }

    pub fn has(&self, code: ErrorCode) -> bool {
        self.active.iter().any(|i| i.code == code)
    }

    pub fn get(&self, code: ErrorCode) -> Option<&Issue> {
        self.active.iter().find(|i| i.code == code)
    }
}

// ---------------------------------------------------------------------------------------

pub fn fmt_time(t: SystemTime) -> String {
    let ago = SystemTime::now().duration_since(t).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    super::widgets::local_clock(ago)
}

fn sev_name(s: Severity) -> &'static str {
    match s {
        Severity::Info => "Info",
        Severity::Warning => "Warning",
        Severity::Error => "Error",
        Severity::Critical => "Critical",
    }
}

/// Plain-text report for support threads (stable format).
pub fn copy_details(i: &Issue, st: &EngineState, rgb: &RgbStatus) -> String {
    let info = &st.info;
    let mut s = String::new();
    s.push_str(&format!("FanRGB issue {}: {}  [{}]\n", i.code, i.title, sev_name(i.severity())));
    s.push_str(&format!(
        "Reported:  by the {}\n",
        match i.source {
            Source::Engine => "fan/sensor engine",
            Source::Rgb => "lighting controller link",
        }
    ));
    s.push_str(&format!(
        "Seen:      {} (first {}, {}×)  · active\n",
        fmt_time(i.last_seen),
        fmt_time(i.first_seen),
        i.count
    ));
    if !i.detail.is_empty() {
        s.push_str(&format!("Detail:    {}\n", i.detail));
    }
    s.push_str(&format!("Help:      {}\n", i.code.help()));
    s.push_str(&format!(
        "App:       FanRGB {} · elevated {} · demo {}\n",
        env!("CARGO_PKG_VERSION"),
        if info.elevated { "yes" } else { "no" },
        if info.simulated { "yes" } else { "no" }
    ));
    s.push_str(&format!(
        "Board:     {} · Super I/O {}\n",
        info.board,
        if info.chips.is_empty() { "none".into() } else { info.chips.join(", ") }
    ));
    s.push_str(&format!("CPU/GPU:   {} (temp: {}) · {}\n", info.cpu, info.cpu_temp_source, info.gpus.join(", ")));
    s.push_str(&format!(
        "Driver:    {} ({})\n",
        info.pawnio.describe(),
        info.pawnio_version.clone().unwrap_or_else(|| "not installed".into())
    ));
    s.push_str(&format!("RGB:       {}\n", rgb.summary()));
    s.push_str(&format!(
        "Conflicts: {}\n",
        if st.conflicts.is_empty() {
            "none".to_string()
        } else {
            st.conflicts.iter().map(|c| c.0.clone()).collect::<Vec<_>>().join(", ")
        }
    ));
    s.push_str("Log (last 20 lines):\n");
    let lines = crate::util::logger::recent_lines();
    for l in lines.iter().skip(lines.len().saturating_sub(20)) {
        s.push_str("  ");
        s.push_str(l);
        s.push('\n');
    }
    s
}

/// Full diagnostics summary for "Copy report" (the engine writes the long version to disk).
pub fn report_text(st: &EngineState, rgb: &RgbStatus, tracker: &IssueTracker) -> String {
    let info = &st.info;
    let mut s = format!("FanRGB {} report\n", env!("CARGO_PKG_VERSION"));
    s.push_str(&format!(
        "Board: {}\nCPU: {} ({})\nGPU: {}\nSuper I/O: {}\n",
        info.board,
        info.cpu,
        info.cpu_temp_source,
        info.gpus.join(", "),
        info.chips.join(", ")
    ));
    s.push_str(&format!(
        "Driver: {} ({})\nAdmin: {}\nDemo: {}\nRGB: {}\n",
        info.pawnio.describe(),
        info.pawnio_version.clone().unwrap_or_default(),
        info.elevated,
        info.simulated,
        rgb.summary()
    ));
    s.push_str("\nActive issues:\n");
    for i in &tracker.active {
        s.push_str(&format!("  {} {} [{}] {}\n", i.code, i.title, sev_name(i.severity()), i.detail));
    }
    s.push_str("\nSensors:\n");
    for e in &st.sensors {
        s.push_str(&format!(
            "  {:<14} {:<24} {:>10} {}\n",
            e.desc.id,
            e.desc.name,
            e.value.map(|v| format!("{v:.2}")).unwrap_or("-".into()),
            e.desc.kind.unit()
        ));
    }
    s.push_str("\nFans:\n");
    for f in &st.fans {
        s.push_str(&format!("  {:<10} {:<12} rpm={:?} duty={:?} mode={}\n", f.id, f.name, f.rpm, f.duty, f.mode));
    }
    s
}

// ---------------------------------------------------------------------------------------
// Fix actions

/// Runs a fix. `subject` is the affected fan/app when known.
pub fn run_fix(
    fix: FixAction,
    subject: Option<&str>,
    ui: &mut super::UiState,
    shared: &Arc<Shared>,
    st: &EngineState,
    cfg: &Config,
) {
    use super::overlays::{Modal, ToastKind};
    match fix {
        FixAction::None => {}
        FixAction::InstallPawnIo => ui.modal = Some(Modal::InstallPawnIo),
        FixAction::RestartAsAdmin => restart_elevated(ui, shared),
        FixAction::PowerCycle => ui.modal = Some(Modal::PowerCycle),
        FixAction::ReconnectRgb => {
            shared.send_rgb(crate::rgb::RgbCommand::Reconnect);
            ui.rgb_reconnect = Some(std::time::Instant::now());
            ui.toast(ToastKind::Info, "Reconnecting to the RGB controller…");
        }
        FixAction::CloseConflictingSoftware => ui.modal = Some(Modal::Conflicts),
        FixAction::RecalibrateFan => {
            let fan = subject
                .and_then(|s| st.fans.iter().find(|f| f.name == s || f.id == s))
                .or_else(|| st.fans.iter().find(|f| f.controllable));
            ui.pending.push(super::actions::UiAction::Navigate(super::Page::Cooling));
            if let Some(f) = fan {
                ui.selected_fan = Some(f.id.clone());
                ui.modal = Some(Modal::Calibrate { fan: f.index, name: f.name.clone(), started: false });
            }
        }
        FixAction::OpenLogFolder => {
            let _ = crate::util::win::open_folder(&crate::util::paths::data_dir());
        }
        FixAction::ResetAiModel => {
            ui.modal = Some(Modal::ConfirmResetModel { key: None, what: "All learned thermal models".into() })
        }
        FixAction::ResetSettings => ui.modal = Some(Modal::ConfirmResetSettings),
        FixAction::RepairStateFolder => repair_state_folder(ui),
    }
    let _ = cfg;
}

pub fn restart_elevated(ui: &mut super::UiState, shared: &Arc<Shared>) {
    use super::overlays::ToastKind;
    match crate::util::win::relaunch_elevated("") {
        crate::util::win::ElevateResult::Launched => crate::system::request_exit(shared),
        crate::util::win::ElevateResult::Cancelled => {
            ui.toast(ToastKind::Warning, "Still running without administrator rights")
        }
        crate::util::win::ElevateResult::Failed(e) => {
            ui.toast(ToastKind::Error, format!("Could not restart as administrator (error {e})"))
        }
    }
}

/// Starts winget (unelevated, absolute path, own console; no cmd.exe) to install PawnIO.
pub fn start_winget_install(ui: &mut super::UiState) {
    use super::overlays::ToastKind;
    match crate::system::spawn_pawnio_install() {
        Ok(child) => {
            ui.issues.log(EventKind::Info, None, "PawnIO installer (winget) started");
            ui.winget = Some(child);
        }
        Err(e) => {
            let code = crate::error::code_of(&e);
            ui.issues.log(EventKind::Failure, Some(code), format!("Could not start winget: {e}"));
            ui.toast_code(ToastKind::Error, format!("Could not start winget: {e}"), code);
        }
    }
}

/// "Repair data folder" (FRG-E610).
fn repair_state_folder(ui: &mut super::UiState) {
    use super::overlays::ToastKind;
    match crate::system::repair_state_folder() {
        Ok(()) => ui.toast_action(
            ToastKind::Success,
            "Data folder repaired. Restart FanRGB to use it.",
            "Restart",
            super::actions::UiAction::RestartApp,
        ),
        Err(e) => {
            let code = crate::error::code_of(&e);
            ui.issues.log(EventKind::Failure, Some(code), format!("Data folder repair failed: {e}"));
            ui.toast_code(ToastKind::Error, format!("Could not repair the data folder: {e}"), code);
        }
    }
}

/// Sends the engine command for a confirmed model reset.
pub fn reset_model(shared: &Arc<Shared>, key: Option<String>) {
    shared.send(Command::ResetAi(key));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AlertLevel;

    fn alert(code: ErrorCode, title: Option<&str>) -> Alert {
        let a = Alert::coded(code, "detail");
        match title {
            Some(t) => a.titled(t),
            None => a,
        }
    }

    #[test]
    fn subjects_from_custom_titles() {
        assert_eq!(
            subject_of(&alert(ErrorCode::FanStalled, Some("SYS_FAN2 is not spinning"))).as_deref(),
            Some("SYS_FAN2")
        );
        assert_eq!(
            subject_of(&alert(ErrorCode::ConflictingSoftware, Some("GIGABYTE Control Center is running"))).as_deref(),
            Some("GIGABYTE Control Center")
        );
        assert_eq!(subject_of(&alert(ErrorCode::PawnIoMissing, None)), None);
        let uncoded = Alert { level: AlertLevel::Info, title: "x".into(), detail: String::new(), code: None };
        assert_eq!(subject_of(&uncoded), None);
    }

    #[test]
    fn diff_appear_resolve_and_keys() {
        let mut t = IssueTracker::default();
        // First update: no toasts for startup state.
        let tr = t.update(&[alert(ErrorCode::PawnIoMissing, None)], &[]);
        assert!(tr.is_empty());
        assert_eq!(t.active.len(), 1);
        // Same alert again: no transition; a second subject of the same code is a new issue.
        let a = [
            alert(ErrorCode::PawnIoMissing, None),
            alert(ErrorCode::FanStalled, Some("SYS_FAN1 is not spinning")),
            alert(ErrorCode::FanStalled, Some("SYS_FAN2 is not spinning")),
        ];
        let tr = t.update(&a, &[]);
        assert_eq!(tr.len(), 2);
        assert_eq!(t.active.len(), 3);
        // Critical sorts first.
        assert_eq!(t.active[0].code, ErrorCode::FanStalled);
        // Resolve one.
        let tr = t.update(&a[..2], &[]);
        assert!(matches!(&tr[0], Transition::Resolved(i) if i.subject.as_deref() == Some("SYS_FAN2")));
        assert!(t.events.iter().any(|e| e.kind == EventKind::Resolved));
    }

    #[test]
    fn uncoded_alerts_are_not_issues_and_info_not_counted() {
        let mut t = IssueTracker::default();
        let state = Alert {
            level: AlertLevel::Info,
            title: "Fans handed back to BIOS".into(),
            detail: String::new(),
            code: None,
        };
        t.update(&[state], &[(ErrorCode::RgbNotFound, "No RGB Fusion 2 controller detected".into())]);
        assert_eq!(t.active.len(), 1);
        assert_eq!(t.badge_count(), 0, "Info is not counted");
        t.update(&[alert(ErrorCode::BusBusy, None)], &[]);
        assert_eq!(t.badge_count(), 1);
        let i = t.active[0].clone();
        t.snooze(&i);
        assert_eq!(t.badge_count(), 0);
    }

    #[test]
    fn rgb_states_map_to_codes() {
        use crate::rgb::RgbConnState;
        let mut r = RgbStatus { state: RgbConnState::NotPresent, ..Default::default() };
        assert_eq!(rgb_issues(&r, false, false)[0].0, ErrorCode::RgbNotFound);
        r.state = RgbConnState::InUseByOtherApp;
        assert_eq!(rgb_issues(&r, false, false)[0].0, ErrorCode::RgbOpenFailed);
        r.state = RgbConnState::Hung;
        assert_eq!(rgb_issues(&r, false, false)[0].0, ErrorCode::RgbHung);
        assert!(rgb_issues(&r, false, true).is_empty(), "E401 from the engine wins");
        r.state = RgbConnState::Connected;
        assert!(rgb_issues(&r, false, false).is_empty());
        r.audio = AudioState::Unavailable("no device".into());
        assert!(rgb_issues(&r, true, false).iter().any(|i| i.0 == ErrorCode::AudioCapture));
        // Dynamic Lighting: the firmware byte alone raises nothing; the registry state does.
        use crate::rgb::dynlight::DlState;
        r.dynamic_lighting.firmware_raw = Some(0);
        r.dynamic_lighting.state = DlState::Off;
        assert!(!rgb_issues(&r, false, false).iter().any(|i| i.0 == ErrorCode::DynamicLightingActive));
        r.dynamic_lighting.state = DlState::Controlling;
        assert!(rgb_issues(&r, false, false).iter().any(|i| i.0 == ErrorCode::DynamicLightingActive));
    }

    #[test]
    fn every_fix_action_has_a_label() {
        for c in ErrorCode::ALL {
            if c.fix() != FixAction::None {
                assert!(!c.fix().label().is_empty(), "{}", c.id());
            }
        }
    }
}
