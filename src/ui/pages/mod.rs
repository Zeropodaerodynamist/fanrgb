//! Pages and helpers shared between them.

pub mod cooling;
pub mod diagnostics;
pub mod intelligence;
pub mod lighting;
pub mod overview;
pub mod sensors;
pub mod settings;
pub mod updates;

use egui::Ui;

use super::PageCtx;
use super::actions::UiAction;
use super::board_map;
use super::issues::{Area, Issue};
use super::overlays::ToastKind;
use super::widgets::board::{FanNode, FanNodeState, LedNode};
use super::widgets::card::{Sev, banner, empty_state};
use super::widgets::controls::{Button, Kind, Size};
use super::widgets::issue_card::issue_card;
use super::widgets::{self};
use crate::config::{Config, Preset, TempSource};
use crate::engine::EngineState;
use crate::error::{ErrorCode, FixAction};
use crate::hw::pawnio::PawnIoStatus;
use crate::model::{FanState, SensorKind};
use crate::rgb::RgbStatus;
use crate::ui::icons::Icon;

pub fn profile_description(p: Preset) -> &'static str {
    match p {
        Preset::Bios => "Your BIOS Smart Fan curves stay in charge",
        Preset::Silent => "Gentle curves, slow ramps, warmer temps",
        Preset::Balanced => "Everyday use and gaming",
        Preset::Performance => "Lowest temperatures, more noise",
        Preset::AiAuto => "Learns your cooling, quietest safe speed",
        Preset::FullSpeed => "Every controlled fan at 100 %",
        Preset::Custom => "Your own settings",
    }
}

/// Temperature sensors as (id, "Group · Name").
pub fn temperature_sensors(st: &EngineState) -> Vec<(String, String)> {
    st.sensors
        .iter()
        .filter(|s| s.desc.kind == SensorKind::Temperature)
        .map(|s| (s.desc.id.clone(), format!("{} · {}", short_group(&s.desc.group), s.desc.name)))
        .collect()
}

pub fn short_group(g: &str) -> &str {
    if g.starts_with("Motherboard") {
        "Board"
    } else if g.contains("Arc") || g.contains("GeForce") || g.contains("Radeon") || g.contains("GPU") {
        "GPU"
    } else {
        g
    }
}

/// Short label for a temperature source ("CPU", "CPU + GPU (hottest)").
pub fn source_short(st: &EngineState, src: &TempSource) -> String {
    let names: Vec<String> = src
        .sensors
        .iter()
        .map(|s| {
            if s.starts_with("cpu") {
                "CPU".to_string()
            } else if s.starts_with("gpu") {
                "GPU".to_string()
            } else {
                st.sensor(s).map(|x| x.desc.name.clone()).unwrap_or_else(|| s.clone())
            }
        })
        .collect();
    match names.len() {
        0 => "No sensor".into(),
        1 => names[0].clone(),
        _ => format!(
            "{} ({})",
            names.join(" + "),
            if src.combine == crate::config::Combine::Max { "hottest" } else { "average" }
        ),
    }
}

pub fn combined(st: &EngineState, src: &TempSource) -> Option<f32> {
    let temps: Vec<f32> = src.sensors.iter().filter_map(|s| st.value(s)).collect();
    crate::control::combine(&temps, src.combine)
}

pub fn value_text(v: Option<f32>, kind: SensorKind, fahrenheit: bool) -> String {
    match (v, kind) {
        (None, _) => "—".into(),
        (Some(_), SensorKind::Temperature) => widgets::fmt_temp1(v, fahrenheit),
        (Some(x), SensorKind::Fan) => format!("{} rpm", widgets::thousands(x)),
        (Some(x), SensorKind::Voltage) => format!("{x:.3} V"),
        (Some(x), SensorKind::Power) => format!("{x:.1} W"),
        (Some(x), SensorKind::Load | SensorKind::Duty) => format!("{x:.0} %"),
    }
}

/// Speed line under a fan's RPM: the duty we drive, the BIOS curve's estimate, or "BIOS"; a
/// header without a tach says so.
pub fn duty_caption(f: &FanState) -> String {
    let duty = match (f.duty, f.duty_est) {
        (Some(d), _) => format!("{d:.0} %"),
        (None, Some(e)) => format!("BIOS ≈{e:.0} %"),
        (None, None) => "BIOS".into(),
    };
    if f.no_speed_signal() { format!("no speed signal · {duty}") } else { duty }
}

/// Speed of a header without a tach, in % (its RPM is unknown): FanRGB's duty, or "≈" the BIOS
/// curve's estimate. `None` when neither is known.
pub fn no_tach_speed(f: &FanState) -> Option<(String, &'static str)> {
    match (f.duty, f.duty_est) {
        (Some(d), _) => Some((format!("{d:.0}"), "set by FanRGB")),
        (None, Some(e)) => Some((format!("≈{e:.0}"), "BIOS curve, estimated")),
        (None, None) => None,
    }
}

/// Tooltip for a header that drives something but reports no RPM.
pub const NO_SPEED_SIGNAL_HELP: &str = "This header drives a fan or pump but reports no speed (tach) signal: a 3-pin \
     splitter or hub without tach, or the pump's tach on another header. FanRGB can't see if it stops, so it watches the \
     CPU temperature instead.";

/// RPM sensor id of a fan channel ("mb.pwm1" → "mb.fan1").
pub fn rpm_id(f: &FanState) -> String {
    f.id.replace("pwm", "fan")
}

pub fn duty_id(f: &FanState) -> String {
    f.id.replace("pwm", "duty")
}

pub fn fan_state(f: &FanState) -> FanNodeState {
    if !f.controllable {
        FanNodeState::ReadOnly
    } else if f.failsafe {
        FanNodeState::Failsafe
    } else if f.controlled {
        FanNodeState::Controlled
    } else {
        FanNodeState::Bios
    }
}

pub fn fan_nodes(st: &EngineState, show_all: bool) -> Vec<FanNode> {
    st.fans
        .iter()
        .filter(|f| f.visible || show_all)
        .map(|f| FanNode {
            id: f.id.clone(),
            name: f.name.clone(),
            rpm: f.rpm,
            duty: f.duty,
            state: fan_state(f),
            mode: f.mode.clone(),
            ghost: f.unconnected,
            no_tach: f.no_speed_signal(),
        })
        .collect()
}

pub fn led_nodes(cfg: &Config, rgb: &RgbStatus) -> Vec<LedNode> {
    let zones = if rgb.zones.is_empty() { crate::rgb::standard_zones() } else { rgb.zones.clone() };
    zones
        .iter()
        .filter(|z| !z.advanced || cfg.rgb.show_all_zones)
        .map(|z| {
            let zc = cfg.rgb.zones.get(&z.id).cloned().unwrap_or_default();
            let eff = if cfg.rgb.sync { &cfg.rgb.global } else { &zc.effect };
            LedNode {
                id: z.id.clone(),
                name: z.name.clone(),
                preview: rgb.preview.get(&z.id).cloned().unwrap_or_default(),
                enabled: cfg.rgb.enabled && zc.enabled,
                effect: widgets::gallery::short_label(eff.kind).to_string(),
                leds: if z.addressable { zc.led_count } else { 1 },
            }
        })
        .collect()
}

pub fn board_for(st: &EngineState) -> Option<&'static board_map::BoardMap> {
    board_map::for_board(&st.info.board, st.info.simulated)
}

/// Handles a card's result (fix buttons, copy, dismiss).
pub fn handle_card(ui: &mut Ui, pc: &mut PageCtx, issue: &Issue, r: widgets::issue_card::CardResult) {
    if let Some(f) = r.fix {
        pc.ui.pending.push(UiAction::Fix(f, issue.subject.clone()));
    }
    if r.reconnect {
        pc.ui.pending.push(UiAction::Fix(FixAction::ReconnectRgb, None));
    }
    if r.open_logs {
        pc.ui.pending.push(UiAction::OpenLogFolder);
    }
    if r.copy_code {
        ui.ctx().copy_text(issue.code.id());
        pc.ui.toast(ToastKind::Success, format!("Copied {}", issue.code.id()));
    }
    if r.copy_details {
        ui.ctx().copy_text(super::issues::copy_details(issue, pc.st, pc.rgb));
        pc.ui.toast(ToastKind::Success, "Details copied");
    }
    if r.dismiss {
        pc.ui.issues.snooze(issue);
    }
}

/// Non-blocking issue banners for a page area (compact issue cards).
pub fn area_banners(ui: &mut Ui, pc: &mut PageCtx, areas: &[Area], skip: &[ErrorCode]) {
    let list: Vec<Issue> = pc
        .ui
        .issues
        .visible()
        .filter(|i| areas.contains(&i.area()) && !skip.contains(&i.code) && i.code != ErrorCode::CriticalTemperature)
        .cloned()
        .collect();
    for i in list {
        let r = issue_card(ui, &i, true, pc.ui.reconnecting());
        handle_card(ui, pc, &i, r);
        ui.add_space(pc.bp.gap() * 0.75);
    }
}

/// "Fans handed back to BIOS" state banner.
pub fn emergency_banner(ui: &mut Ui, pc: &mut PageCtx) {
    if !pc.st.emergency_released {
        return;
    }
    let name = pc.cfg.profile().name.clone();
    banner(
        ui,
        Sev::Info,
        "Fans are under BIOS control",
        "You returned the fans to the BIOS. Resume to let FanRGB control them again.",
        |ui| {
            if Button::new(Kind::Primary, &format!("Resume {name}")).size(Size::Sm).icon(Icon::Play).show(ui).clicked()
            {
                pc.ui.pending.push(UiAction::Resume);
            }
        },
    );
    ui.add_space(pc.bp.gap());
}

/// Which blocking state (if any) prevents fan control. Ordered by root cause.
pub fn fan_block(st: &EngineState, issues: &super::issues::IssueTracker) -> Option<ErrorCode> {
    if st.info.simulated {
        return None;
    }
    match st.info.pawnio {
        PawnIoStatus::NotInstalled => return Some(ErrorCode::PawnIoMissing),
        PawnIoStatus::AccessDenied => return Some(ErrorCode::NotElevated),
        _ => {}
    }
    if !st.info.elevated && !st.info.fan_control {
        return Some(ErrorCode::NotElevated);
    }
    if issues.has(ErrorCode::PawnIoModule) && !st.info.fan_control {
        return Some(ErrorCode::PawnIoModule);
    }
    if st.tick > 0 && (st.info.chips.is_empty() || st.fans.is_empty()) && !st.info.fan_control {
        return Some(ErrorCode::NoSuperIo);
    }
    None
}

/// Blocking empty state for fan control (replaces the page body).
pub fn fan_block_state(ui: &mut Ui, pc: &mut PageCtx, code: ErrorCode, what: &str) {
    let (icon, title, body) = match code {
        ErrorCode::PawnIoMissing => (Icon::Shield, format!("{what} needs the PawnIO driver"), code.help().to_string()),
        ErrorCode::NotElevated => (Icon::Lock, format!("{what} needs administrator rights"), "FanRGB talks to the fan controller through PawnIO, which requires administrator rights. Lighting and GPU/CPU load keep working without them.".to_string()),
        ErrorCode::NoSuperIo => (Icon::Chip, "No supported fan controller found".to_string(), "No ITE Super I/O chip was detected, so fans stay under BIOS control. Copy the details and attach them to a support request.".to_string()),
        _ => (Icon::Error, code.title().to_string(), code.help().to_string()),
    };
    let issue = pc.ui.issues.get(code).cloned();
    empty_state(ui, icon, None, &title, &body, |ui| {
        let fix = code.fix();
        if fix != FixAction::None && fix != FixAction::OpenLogFolder {
            if Button::new(Kind::Primary, fix.label()).size(Size::Lg).show(ui).clicked() {
                pc.ui.pending.push(UiAction::Fix(fix, None));
            }
            ui.add_space(6.0);
        }
        if code == ErrorCode::PawnIoMissing && widgets::controls::link(ui, "What is PawnIO? (pawnio.eu)").clicked() {
            let _ = crate::util::win::open_url("https://pawnio.eu/");
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let w = 220.0;
            ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
            widgets::controls::code_chip(ui, &code.id());
            if Button::new(Kind::Ghost, "Copy details").size(Size::Sm).icon(Icon::Copy).show(ui).clicked() {
                let i = issue.clone().unwrap_or_else(|| Issue {
                    code,
                    subject: None,
                    title: code.title().into(),
                    detail: String::new(),
                    first_seen: std::time::SystemTime::now(),
                    last_seen: std::time::SystemTime::now(),
                    count: 1,
                    source: super::issues::Source::Engine,
                });
                ui.ctx().copy_text(super::issues::copy_details(&i, pc.st, pc.rgb));
                pc.ui.toast(ToastKind::Success, "Details copied");
            }
        });
    });
}

/// Highest AI hard limit the engine accepts: 95 °C, 90 °C for CPU/pump fans (engine clamps too).
pub fn ai_limit_cap(role: crate::model::FanRole) -> f32 {
    match role {
        crate::model::FanRole::Cpu | crate::model::FanRole::Pump | crate::model::FanRole::Unknown => 90.0,
        crate::model::FanRole::System => 95.0,
    }
}

/// Target + hard-limit sliders with the engine's clamps (target 40..92, limit target+3..cap).
pub fn ai_target_sliders(ui: &mut Ui, id: egui::Id, ai: &mut crate::config::AiConfig, cap: f32) {
    let (t_lo, t_hi) = ai_target_range(cap);
    ai.target = ai.target.clamp(t_lo, t_hi);
    super::widgets::controls::slider_row(
        ui,
        id.with("t"),
        "Target (keep below)",
        &mut ai.target,
        t_lo..=t_hi,
        1.0,
        " °C",
        0,
    );
    let lo = ai.target + 3.0;
    ai.limit = ai.limit.clamp(lo, cap.max(lo));
    super::widgets::controls::slider_row(
        ui,
        id.with("l"),
        "Hard limit (100 %)",
        &mut ai.limit,
        lo..=cap.max(lo),
        1.0,
        " °C",
        0,
    );
}

pub fn ai_target_range(cap: f32) -> (f32, f32) {
    (40.0, (cap - 3.0).min(92.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FanRole;

    #[test]
    fn ai_caps_match_engine_clamps() {
        assert_eq!(ai_limit_cap(FanRole::Cpu), 90.0);
        assert_eq!(ai_limit_cap(FanRole::Pump), 90.0);
        assert_eq!(ai_limit_cap(FanRole::System), 95.0);
        assert_eq!(ai_target_range(95.0), (40.0, 92.0));
        assert_eq!(ai_target_range(90.0), (40.0, 87.0));
    }
}
