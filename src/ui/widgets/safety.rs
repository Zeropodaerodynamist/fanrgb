//! "CPU safety" card: a calm status, not an alarm. Gauge, a state chip (Safe / Watch /
//! Hot / No sensor), a headline, the thermal bar with target and limit, the CPU fan line and, only
//! when the engine says so, "Temperature follows fan speed — cooling is working."

use egui::{Align2, Color32, Id, Rect, Sense, Stroke, Ui, pos2, vec2};

use crate::error::ErrorCode;
use crate::model::{FanState, ids};
use crate::ui::PageCtx;
use crate::ui::actions::UiAction;
use crate::ui::fonts::{Ts, font, tab};
use crate::ui::icons::{self, Icon};
use crate::ui::theme::{Palette, R_PILL, pal, thermal, thermal_stops, thermal_with};
use crate::ui::widgets::chart::{at, window_start};
use crate::ui::widgets::controls::{self, Tone};
use crate::ui::widgets::gauge::{Gauge, fan_ring};
use crate::ui::widgets::{self, text};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Safety {
    Safe,
    Watch,
    Hot,
    NoSensor,
}

impl Safety {
    pub fn label(self) -> &'static str {
        match self {
            Safety::Safe => "Safe",
            Safety::Watch => "Watch",
            Safety::Hot => "Hot",
            Safety::NoSensor => "No sensor",
        }
    }

    pub fn tone(self) -> Tone {
        match self {
            Safety::Safe => Tone::Success,
            Safety::Watch => Tone::Warning,
            Safety::Hot => Tone::Danger,
            Safety::NoSensor => Tone::Neutral,
        }
    }
}

/// The state rule (the engine has no `cpu_safety` yet, so the UI derives it):
/// Hot = the critical latch (E301) or at the limit; Watch = within 10° of the limit, or rising
/// ≥ 1.5 °C/s for 3 s; Safe again only below 1.0 °C/s (hysteresis via `prev`).
pub fn classify(temp: Option<f32>, limit: f32, rate_3s: Option<f32>, critical: bool, prev: Safety) -> Safety {
    let Some(t) = temp else { return Safety::NoSensor };
    if critical || t >= limit {
        return Safety::Hot;
    }
    if t >= limit - 10.0 {
        return Safety::Watch;
    }
    match rate_3s {
        Some(r) if r >= 1.5 => Safety::Watch,
        Some(r) if r >= 1.0 && prev == Safety::Watch => Safety::Watch,
        _ => Safety::Safe,
    }
}

/// Slowest per-second rise over the last 3 s of history (so a single noisy step never counts as
/// "rising for 3 s"); `None` without enough samples.
pub fn sustained_rate(times: &std::collections::VecDeque<f64>, s: &std::collections::VecDeque<f32>) -> Option<f32> {
    let n = times.len();
    if n < 4 {
        return None;
    }
    let start = window_start(times, 3.5).min(n - 4);
    let mut min_rate = f32::INFINITY;
    for i in start + 1..n {
        let (a, b) = (at(n, s, i - 1), at(n, s, i));
        let dt = (times[i] - times[i - 1]) as f32;
        if !a.is_finite() || !b.is_finite() || dt <= 0.05 {
            return None;
        }
        min_rate = min_rate.min((b - a) / dt);
    }
    min_rate.is_finite().then_some(min_rate)
}

/// The header that cools the CPU (CPU_FAN first, else the first CPU-chain role).
pub fn cpu_fan(fans: &[FanState]) -> Option<&FanState> {
    fans.iter()
        .find(|f| f.header.eq_ignore_ascii_case("CPU_FAN") && !f.unconnected)
        .or_else(|| fans.iter().find(|f| f.role == crate::model::FanRole::Cpu && !f.unconnected))
}

/// "CPU_FAN · Radiator fans" (the friendly name only when the user / wizard gave one).
pub fn fan_title(f: &FanState) -> String {
    if f.name.is_empty() || f.name == f.header { f.header.clone() } else { format!("{} · {}", f.header, f.name) }
}

/// Speed words: "1 240 rpm", or "Running · no speed signal" (neutral, never a warning).
pub fn speed_words(f: &FanState) -> String {
    if f.no_speed_signal() {
        if f.duty.or(f.duty_est).is_some_and(|d| d < 1.0) {
            "Stopped · no speed signal".into()
        } else {
            "Running · no speed signal".into()
        }
    } else {
        widgets::fmt_rpm(f.rpm)
    }
}

/// Glyph spin for a fan: the RPM, or for a header without a tach an estimate from the duty.
pub fn spin_rpm(f: &FanState) -> Option<f32> {
    f.rpm.or_else(|| f.no_speed_signal().then(|| f.duty.or(f.duty_est).map(|d| d * 18.0)).flatten())
}

pub fn cpu_safety_card(ui: &mut Ui, pc: &mut PageCtx, min_h: f32) -> f32 {
    let p = pal(ui);
    let f = pc.cfg.general.fahrenheit;
    let limit = pc.cfg.general.cpu_critical;
    let temp = pc.st.value(ids::CPU_TEMP);
    let rate = pc.hist.series.get(ids::CPU_TEMP).and_then(|s| sustained_rate(&pc.hist.times, s));
    let critical = pc.ui.issues.has(ErrorCode::CriticalTemperature);
    let key = Id::new("cpu_safety_state");
    let prev = ui
        .data(|d| d.get_temp::<u8>(key))
        .map(|v| if v == 1 { Safety::Watch } else { Safety::Safe })
        .unwrap_or(Safety::Safe);
    let state = classify(temp, limit, rate, critical, prev);
    ui.data_mut(|d| d.insert_temp(key, if state == Safety::Watch { 1u8 } else { 0u8 }));
    let target = pc.st.ai.iter().find(|z| z.source.contains("CPU") || z.source.contains("Tctl")).map(|z| z.target);
    let blocked = crate::ui::pages::fan_block(pc.st, &pc.ui.issues);
    let fan = cpu_fan(&pc.st.fans).cloned();
    let narrow = ui.available_width() < 440.0;

    crate::ui::widgets::card::card_eq(ui, min_h, 18, |ui| {
        // ---- header ----
        ui.horizontal(|ui| {
            ui.set_min_height(28.0);
            text::label(ui, "CPU safety", Ts::Title, p.text1);
            ui.add_space(4.0);
            controls::dot_chip(ui, state.label(), state.tone());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                text::label(ui, format!("Limit {}", widgets::fmt_temp(Some(limit), f)), Ts::Caption, p.text2)
                    .on_hover_text("Every controlled fan runs at 100 % from this CPU temperature (Settings › Safety).");
            });
        });
        ui.add_space(6.0);
        ui.horizontal_top(|ui| {
            // ---- gauge ----
            let gsize = if narrow { 112.0 } else { 140.0 };
            let mut sub = Vec::new();
            if let Some(w) = pc.st.value(ids::CPU_POWER) {
                sub.push(format!("{w:.0} W"));
            }
            if let Some(l) = pc.st.value(ids::CPU_LOAD) {
                sub.push(format!("{l:.0} %"));
            }
            let sub = sub.join(" · ");
            let src = pc.st.info.cpu_temp_source.clone();
            let label = if src.contains("Tctl") { "CPU · Tctl" } else { "CPU" };
            let locked = (temp.is_none() && blocked.is_some()).then_some("");
            let (resp, _) = Gauge {
                id: Id::new("g_cpu_safety"),
                size: gsize,
                value: temp,
                lo: 20.0,
                hi: 100.0,
                critical: limit,
                target,
                label,
                sub: &sub,
                fahrenheit: f,
                locked,
            }
            .show(ui);
            if resp.on_hover_text("Open CPU sensors").clicked() {
                pc.ui.sensor_filter = "cpu".into();
                pc.ui.pending.push(UiAction::Navigate(crate::ui::Page::Sensors));
            }
            ui.add_space(10.0);
            // ---- right side ----
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                ui.add_space(8.0);
                let (head, tail): (String, String) = match (state, temp) {
                    (Safety::NoSensor, _) => (
                        "CPU temperature unavailable".into(),
                        match blocked {
                            Some(ErrorCode::NotElevated) => "Needs administrator".into(),
                            Some(_) => "Needs PawnIO driver".into(),
                            None => "No CPU sensor was found".into(),
                        },
                    ),
                    (Safety::Hot, Some(t)) => (
                        widgets::fmt_temp(Some(t), f),
                        format!("All fans at 100 % until < {}", widgets::fmt_temp(Some(limit - 8.0), f)),
                    ),
                    (Safety::Watch, Some(_)) if rate.is_some_and(|r| r >= 1.0) => {
                        ("Rising fast".into(), format!("+{:.1} °C/s · fans ramping", rate.unwrap_or(0.0)))
                    }
                    (Safety::Watch, Some(t)) => (
                        format!("Warm · {}", widgets::fmt_temp(Some(t), f)),
                        format!("{:.0}° below the limit", (limit - t).max(0.0)),
                    ),
                    (_, Some(t)) => (
                        format!("Holding {}", widgets::fmt_temp(Some(t), f)),
                        format!("{:.0}° below the limit", (limit - t).max(0.0)),
                    ),
                    (_, None) => ("—".into(), String::new()),
                };
                ui.horizontal_wrapped(|ui| {
                    text::label(ui, &head, Ts::DisplaySm, p.text1);
                    ui.add_space(2.0);
                    if state == Safety::NoSensor && blocked.is_some() {
                        let code = blocked.unwrap_or(ErrorCode::PawnIoMissing);
                        if controls::link(ui, code.fix().label()).clicked() {
                            pc.ui.pending.push(UiAction::Fix(code.fix(), None));
                        }
                    } else {
                        text::label(ui, &tail, Ts::Body, p.text2);
                    }
                });
                if let Some(t) = temp {
                    thermal_bar(ui, p, t, target, limit, f);
                }
                if let Some(fan) = &fan {
                    fan_line(ui, pc, fan);
                }
                if pc.st.cpu_cooling_verified == Some(true) {
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                        icons::paint(ui.painter(), r, Icon::Check, p.success);
                        text::label(ui, "Temperature follows fan speed — cooling is working.", Ts::Body, p.text2);
                    });
                }
            });
        });
    })
}

/// Oklab thermal ramp with the live marker, the target tick (accent) and the limit tick (danger).
fn thermal_bar(ui: &mut Ui, p: &Palette, t: f32, target: Option<f32>, limit: f32, f: bool) {
    let w = ui.available_width().min(460.0);
    let (rect, _) = ui.allocate_exact_size(vec2(w, 30.0), Sense::hover());
    let bar = Rect::from_min_size(pos2(rect.left(), rect.top() + 6.0), vec2(w, 4.0));
    let painter = ui.painter();
    let (lo, hi) = (20.0, 100.0);
    let x_of = |v: f32| bar.left() + ((v - lo) / (hi - lo)).clamp(0.0, 1.0) * bar.width();
    let stops = thermal_stops(limit);
    let n = 48;
    for i in 0..n {
        let a = i as f32 / n as f32;
        let b = (i + 1) as f32 / n as f32;
        let c = thermal_with(&p.thermal, &stops, lo + (a + b) / 2.0 * (hi - lo));
        let r = Rect::from_min_max(
            pos2(bar.left() + a * bar.width(), bar.top()),
            pos2(bar.left() + b * bar.width() + 0.5, bar.bottom()),
        );
        painter.rect_filled(r, 0.0, c);
    }
    let x = x_of(t);
    painter.circle_filled(pos2(x, bar.center().y), 6.0, Color32::WHITE);
    painter.circle_stroke(pos2(x, bar.center().y), 6.0, Stroke::new(1.5, thermal(p, t, limit)));
    let tick = |v: f32, c: Color32, label: String| {
        let x = x_of(v);
        painter.line_segment([pos2(x, bar.top() - 5.0), pos2(x, bar.bottom() + 5.0)], Stroke::new(2.0, c));
        painter.text(pos2(x, bar.bottom() + 9.0), Align2::CENTER_TOP, label, font(Ts::Caption), c);
    };
    if let Some(tg) = target {
        tick(tg, p.accent, format!("target {}", widgets::fmt_deg(Some(tg), f)));
    }
    tick(limit, p.danger, "limit".into());
}

/// "◎ CPU_FAN · Radiator fans  Running · no speed signal   [62 %]".
fn fan_line(ui: &mut Ui, pc: &mut PageCtx, fan: &FanState) {
    let p = pal(ui);
    let w = ui.available_width().min(460.0);
    let (r, resp) = ui.allocate_exact_size(vec2(w, 40.0), Sense::click());
    let color = if fan.failsafe {
        p.danger
    } else if fan.controlled {
        p.accent
    } else {
        p.text3
    };
    let angle = crate::ui::anim::spin_angle(ui.ctx(), spin_rpm(fan));
    fan_ring(ui.painter(), p, pos2(r.left() + 14.0, r.center().y), 13.0, fan.duty.or(fan.duty_est), color, angle);
    let painter = ui.painter();
    painter.text(
        pos2(r.left() + 36.0, r.top() + 11.0),
        Align2::LEFT_CENTER,
        fan_title(fan),
        font(Ts::BodyStrong),
        p.text1,
    );
    painter.text(
        pos2(r.left() + 36.0, r.top() + 29.0),
        Align2::LEFT_CENTER,
        speed_words(fan),
        font(Ts::Caption),
        p.text2,
    );
    let duty = match (fan.duty, fan.duty_est) {
        (Some(d), _) => format!("{d:.0} %"),
        (None, Some(e)) => format!("≈{e:.0} %"),
        (None, None) => "BIOS".into(),
    };
    let g = painter.layout_no_wrap(duty, tab(Ts::Label), p.text1);
    let chip =
        Rect::from_min_size(pos2(r.right() - g.size().x - 20.0, r.center().y - 12.0), vec2(g.size().x + 20.0, 24.0));
    painter.rect_filled(chip, R_PILL, p.surface2);
    painter.galley(pos2(chip.left() + 10.0, chip.center().y - g.size().y / 2.0), g, p.text1);
    let resp = if fan.no_speed_signal() {
        resp.on_hover_text(format!(
            "{}\n\nThe glyph's speed is estimated from the duty.",
            crate::ui::pages::NO_SPEED_SIGNAL_HELP
        ))
    } else {
        resp
    };
    if resp.clicked() {
        pc.ui.pending.push(UiAction::SelectFan(fan.id.clone()));
    }
}

#[cfg(test)]
pub mod tests_support {
    use crate::config::HeaderRole;
    use crate::model::{FanRole, FanState};

    /// A present, controllable header without a tach (like CPU_FAN on the reference PC).
    pub fn fan(id: &str, header: &str, assigned: Option<HeaderRole>, role: FanRole) -> FanState {
        FanState {
            id: id.into(),
            index: 0,
            name: assigned.and_then(HeaderRole::default_name).unwrap_or(header).into(),
            header: header.into(),
            role,
            assigned,
            rpm: None,
            duty: Some(62.0),
            mode: "Curve".into(),
            controlled: true,
            controllable: true,
            visible: true,
            failsafe: false,
            hw_curve: true,
            hw_failsafe: false,
            unconnected: false,
            speed_signal: false,
            duty_est: None,
            floor: Some(60.0),
            ai_refused: None,
            gpu_lift: None,
            identifying: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn states_follow_the_design_rules() {
        assert_eq!(classify(None, 90.0, None, false, Safety::Safe), Safety::NoSensor);
        assert_eq!(classify(Some(63.0), 90.0, Some(0.2), false, Safety::Safe), Safety::Safe);
        assert_eq!(classify(Some(81.0), 90.0, Some(0.0), false, Safety::Safe), Safety::Watch);
        assert_eq!(classify(Some(70.0), 90.0, Some(2.1), false, Safety::Safe), Safety::Watch);
        // Hysteresis: 1.2 °C/s keeps Watch but does not enter it.
        assert_eq!(classify(Some(70.0), 90.0, Some(1.2), false, Safety::Watch), Safety::Watch);
        assert_eq!(classify(Some(70.0), 90.0, Some(1.2), false, Safety::Safe), Safety::Safe);
        assert_eq!(classify(Some(70.0), 90.0, Some(0.5), false, Safety::Watch), Safety::Safe);
        assert_eq!(classify(Some(89.0), 90.0, None, true, Safety::Safe), Safety::Hot);
        assert_eq!(classify(Some(91.0), 90.0, None, false, Safety::Safe), Safety::Hot);
    }

    #[test]
    fn sustained_rate_needs_every_second_rising() {
        let times: VecDeque<f64> = (0..6).map(|i| i as f64).collect();
        let steady: VecDeque<f32> = [50.0, 52.0, 54.0, 56.0, 58.0, 60.0].into();
        assert_eq!(sustained_rate(&times, &steady), Some(2.0));
        // One spike is not "rising for 3 s".
        let spike: VecDeque<f32> = [50.0, 50.0, 50.0, 50.0, 55.0, 55.0].into();
        assert_eq!(sustained_rate(&times, &spike), Some(0.0));
        assert_eq!(sustained_rate(&times.iter().take(2).copied().collect(), &steady), None);
    }

    #[test]
    fn no_tach_reads_as_running_not_missing() {
        let f = tests_support::fan("mb.pwm0", "CPU_FAN", None, crate::model::FanRole::Unknown);
        assert_eq!(speed_words(&f), "Running · no speed signal");
        assert_eq!(fan_title(&f), "CPU_FAN");
        assert!(spin_rpm(&f).is_some_and(|r| r > 0.0));
    }
}
