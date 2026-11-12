//! Overview: "Am I cool, quiet and lit the way I want?"
//!
//! Fan-mode tiles, then the lit rig (hero) next to CPU safety, GPU + board temperatures and the fan
//! list; the history charts continue below the fold.

use egui::{Align2, Id, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use super::{PageCtx, board_for, emergency_banner, fan_block, fan_nodes, handle_card, led_nodes, rpm_id};
use crate::model::{FanState, SensorKind, ids};
use crate::ui::actions::UiAction;
use crate::ui::fonts::{Ts, font, tab};
use crate::ui::icons::{self, Icon};
use crate::ui::layout::{self, Breakpoint};
use crate::ui::theme::{R_LG, R_MD, fan_color, pal, series_for, thermal};
use crate::ui::widgets::board::{BoardMode, BoardView};
use crate::ui::widgets::card::{card, card_eq, header};
use crate::ui::widgets::chart::{self, ChartSeries, Threshold, TimeChart};
use crate::ui::widgets::controls::{self, Tone};
use crate::ui::widgets::gauge::Gauge;
use crate::ui::widgets::issue_card::issue_card;
use crate::ui::widgets::{self, fan_modes, hero, safety, text, thumb};
use crate::ui::{Page, anim};

pub fn show(ui: &mut Ui, pc: &mut PageCtx) {
    let gap = pc.bp.gap();
    ui.add_space(4.0);
    // ---- issue slot: at most one card + "N more" ----
    let top: Vec<_> = pc
        .ui
        .issues
        .visible()
        .filter(|i| {
            i.severity() > crate::error::Severity::Info && i.code != crate::error::ErrorCode::CriticalTemperature
        })
        .cloned()
        .collect();
    if let Some(first) = top.first() {
        let r = issue_card(ui, first, true, pc.ui.reconnecting());
        handle_card(ui, pc, first, r);
        if top.len() > 1 {
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                if controls::link(
                    ui,
                    &format!("{} more issue{} →", top.len() - 1, if top.len() > 2 { "s" } else { "" }),
                )
                .clicked()
                {
                    pc.ui.pending.push(UiAction::Navigate(Page::Diagnostics));
                }
            });
        }
        ui.add_space(gap);
    }
    emergency_banner(ui, pc);
    super::updates::banner(ui, pc);

    // ---- fan modes ----
    fan_modes::section_header(ui, "One click · applies to every fan · Ctrl+P");
    ui.add_space(4.0);
    fan_modes::mode_bar(ui, pc, false);
    ui.add_space(gap);

    if pc.st.tick == 0 {
        skeleton(ui, pc);
        return;
    }

    // ---- stage + status column ----
    let w = ui.available_width();
    let stage = if pc.prefs.show_hero { w >= 700.0 } else { pc.prefs.show_board_map && w >= 760.0 };
    if stage {
        let left = if pc.prefs.show_hero { (w * 0.43).clamp(300.0, 440.0) } else { (w * 0.42).clamp(320.0, 440.0) };
        layout::row(ui, Id::new("ov_main"), &[left, w - left - gap], gap, |i, ui, min_h| {
            if i == 0 {
                if pc.prefs.show_hero { hero::hero_stage(ui, pc, min_h) } else { board_card(ui, pc, min_h) }
            } else {
                status_column(ui, pc, min_h)
            }
        });
    } else {
        status_column(ui, pc, 0.0);
    }
    ui.add_space(gap);
    history_card(ui, pc);
}

/// CPU safety, GPU + board, fans. Returns its natural height; the fan list stretches so the column
/// ends level with the stage.
fn status_column(ui: &mut Ui, pc: &mut PageCtx, min_h: f32) -> f32 {
    let gap = pc.bp.gap();
    let top = ui.cursor().top();
    let h1 = safety::cpu_safety_card(ui, pc, 0.0);
    ui.add_space(gap);
    let narrow = ui.available_width() < 470.0;
    let h2 = if narrow {
        let a = gpu_card(ui, pc, 0.0);
        ui.add_space(gap);
        a + gap + board_temps_card(ui, pc, 0.0)
    } else {
        // Two equal-height cards; the second takes exactly what is left, so the pair never pushes the
        // column wider than the CPU safety card above it.
        let key = Id::new("ov_gpu_board_h");
        let prev: f32 = ui.data(|d| d.get_temp(key)).unwrap_or(0.0);
        let total = ui.available_width();
        let w1 = ((total - gap) * 0.42).floor();
        let mut hh = 0.0f32;
        ui.allocate_ui_with_layout(vec2(total, 0.0), egui::Layout::left_to_right(egui::Align::Min), |ui| {
            ui.spacing_mut().item_spacing.x = gap;
            ui.allocate_ui_with_layout(vec2(w1, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(w1);
                hh = hh.max(gpu_card(ui, pc, prev));
            });
            let w2 = (total - w1 - gap).max(0.0);
            ui.allocate_ui_with_layout(vec2(w2, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(w2);
                hh = hh.max(board_temps_card(ui, pc, prev));
            });
        });
        if (hh - prev).abs() > 0.5 {
            ui.data_mut(|d| d.insert_temp(key, hh));
            ui.ctx().request_repaint();
        }
        hh
    };
    ui.add_space(gap);
    let used = ui.cursor().top() - top;
    let h3 = fans_card(ui, pc, (min_h - used).max(0.0));
    h1 + h2 + h3 + 2.0 * gap
}

fn skeleton(ui: &mut Ui, pc: &mut PageCtx) {
    let p = pal(ui);
    let t = ui.input(|i| i.time) as f32;
    let shimmer = if anim::reduce_motion(ui.ctx()) { 0.0 } else { 0.5 + 0.5 * (t * 3.0).sin() };
    let fill = crate::ui::theme::mix(p.surface1, p.surface2, shimmer);
    layout::row(ui, Id::new("sk"), &[0.43, 0.57], pc.bp.gap(), |_, ui, _| {
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 420.0), Sense::hover());
        ui.painter().rect_filled(r, R_LG, fill);
        420.0
    });
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
}

fn gpu_card(ui: &mut Ui, pc: &mut PageCtx, min_h: f32) -> f32 {
    let p = pal(ui);
    let f = pc.cfg.general.fahrenheit;
    let gpu = pc.st.value(ids::GPU_TEMP);
    let name = pc
        .st
        .info
        .gpus
        .first()
        .map(|g| {
            g.replace("Intel(R) ", "")
                .replace("(TM)", "")
                .replace("NVIDIA ", "")
                .replace("AMD ", "")
                .replace("Graphics", "")
                .trim()
                .to_string()
        })
        .unwrap_or_default();
    card_eq(ui, min_h, 14, |ui| {
        ui.horizontal(|ui| {
            let (resp, _) = Gauge {
                id: Id::new("g_gpu"),
                size: 96.0,
                value: gpu,
                lo: 20.0,
                hi: 100.0,
                critical: pc.cfg.general.gpu_critical,
                target: None,
                label: "",
                sub: "",
                fahrenheit: f,
                locked: None,
            }
            .show(ui);
            let resp = resp.on_hover_ui(|ui| minmax_tip(ui, pc, ids::GPU_TEMP));
            if resp.clicked() {
                pc.ui.sensor_filter = "gpu".into();
                pc.ui.pending.push(UiAction::Navigate(Page::Sensors));
            }
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.add_space(14.0);
                ui.spacing_mut().item_spacing.y = 2.0;
                text::label(ui, "GPU", Ts::Headline, p.text1);
                if !name.is_empty() {
                    text::para(ui, &name, Ts::Body, p.text2);
                }
                let mut parts = Vec::new();
                if let Some(w) = pc.st.value("gpu0.power") {
                    parts.push(format!("{w:.0} W"));
                }
                if let Some(l) = pc.st.value(ids::GPU_LOAD) {
                    parts.push(format!("{l:.0} %"));
                }
                if let Some(r) = pc.st.value("gpu0.fan") {
                    parts.push(if r < 1.0 { "fans stopped".into() } else { format!("{} rpm", widgets::thousands(r)) });
                }
                if !parts.is_empty() {
                    text::label(ui, parts.join(" · "), Ts::Caption, p.text3);
                }
            });
        });
    })
}

fn minmax_tip(ui: &mut Ui, pc: &PageCtx, id: &str) {
    let p = pal(ui);
    let f = pc.cfg.general.fahrenheit;
    if let Some(s) = pc.st.sensor(id) {
        text::label(ui, format!("{} · {}", super::short_group(&s.desc.group), s.desc.name), Ts::BodyStrong, p.text1);
        text::label(
            ui,
            format!("Session min {} · max {}", widgets::fmt_temp(s.min, f), widgets::fmt_temp(s.max, f)),
            Ts::Caption,
            p.text2,
        );
        chart::sparkline(ui, pc.hist, id, 60.0, vec2(220.0, 32.0), series_for(p, id), 4.0, |v| v);
        text::label(ui, "Click to open Sensors", Ts::Caption, p.text3);
    }
}

fn board_temps_card(ui: &mut Ui, pc: &mut PageCtx, min_h: f32) -> f32 {
    let p = pal(ui);
    let f = pc.cfg.general.fahrenheit;
    let crit = pc.cfg.general.cpu_critical;
    card_eq(ui, min_h, 14, |ui| {
        text::label(ui, "Board", Ts::Headline, p.text1);
        ui.add_space(2.0);
        let rows: Vec<(String, f32)> = pc
            .st
            .sensors
            .iter()
            .filter(|s| s.desc.kind == SensorKind::Temperature && s.desc.id.starts_with("mb.") && s.value.is_some())
            .take(3)
            .map(|s| (s.desc.name.clone(), s.value.unwrap_or(0.0)))
            .collect();
        if rows.is_empty() {
            text::para(
                ui,
                "Motherboard sensors need the PawnIO driver and administrator rights.",
                Ts::Caption,
                p.text2,
            );
            if let Some(code) = fan_block(pc.st, &pc.ui.issues) {
                ui.add_space(4.0);
                if controls::link(ui, code.fix().label()).clicked() {
                    pc.ui.pending.push(UiAction::Fix(code.fix(), None));
                }
            }
        }
        for (name, v) in rows {
            let w = ui.available_width();
            let (r, _) = ui.allocate_exact_size(vec2(w, 24.0), Sense::hover());
            let painter = ui.painter();
            let name_w = (w * 0.34).min(110.0);
            painter.text(pos2(r.left(), r.center().y), Align2::LEFT_CENTER, &name, font(Ts::Body), p.text2);
            painter.text(
                pos2(r.right(), r.center().y),
                Align2::RIGHT_CENTER,
                widgets::fmt_deg(Some(v), f),
                tab(Ts::BodyStrong),
                p.text1,
            );
            let bar = Rect::from_min_max(
                pos2(r.left() + name_w, r.center().y - 2.0),
                pos2(r.right() - 46.0, r.center().y + 2.0),
            );
            controls::paint_bar(painter, bar, ((v - 20.0) / 80.0).clamp(0.02, 1.0), thermal(p, v, crit), p.surface3);
        }
    })
}

fn board_card(ui: &mut Ui, pc: &mut PageCtx, min_h: f32) -> f32 {
    let fans = fan_nodes(pc.st, false);
    let leds = led_nodes(pc.cfg, pc.rgb);
    let map = board_for(pc.st);
    let preview_only = pc.rgb.device.is_none();
    let texture = map.and_then(|_| pc.assets.board(ui.ctx()));
    card_eq(ui, min_h, 16, |ui| {
        header(ui, "Board", map.map(|m| m.name), |ui| {
            if preview_only {
                controls::chip(ui, "Preview only", Tone::Neutral)
                    .on_hover_text("No RGB controller connected; LED colors are the preview");
            }
        });
        ui.add_space(6.0);
        let selected = pc.ui.selected_fan.clone();
        let clicked = BoardView {
            id: Id::new("ov_board"),
            map,
            mode: BoardMode::Overview,
            fans: &fans,
            leds: &leds,
            selected: selected.as_deref(),
            show_ghosts: false,
            preview_only,
            texture,
        }
        .show(ui);
        if let Some(id) = clicked {
            if id.starts_with("mb.") {
                pc.ui.pending.push(UiAction::SelectFan(id));
            } else {
                pc.ui.pending.push(UiAction::SelectZone(id));
            }
        }
    })
}

/// Headers whose device nobody confirmed yet (the engine's list, [`EngineState::mode_gate`]): until
/// they are identified, every fan mode except BIOS leaves all headers on the BIOS.
///
/// [`EngineState::mode_gate`]: crate::engine::EngineState::mode_gate
pub fn unidentified(pc: &PageCtx) -> Vec<String> {
    pc.st.mode_gate.clone()
}

/// A fan mode other than BIOS is selected but waits for the identification.
pub fn mode_held(pc: &PageCtx) -> bool {
    !pc.st.mode_gate.is_empty() && pc.cfg.profile().preset != crate::config::Preset::Bios
}

/// "Identify your fans" prompt (no wiring is assumed until the user / wizard confirms it). It can be
/// put off with "Later" only while the BIOS mode is selected: a selected mode that waits for it keeps
/// the prompt up.
pub fn identify_prompt(ui: &mut Ui, pc: &mut PageCtx) {
    let unknown = unidentified(pc);
    let held = mode_held(pc);
    if unknown.is_empty() || (pc.prefs.identify_prompt_dismissed && !held) || pc.st.identify.is_some() {
        return;
    }
    let p = pal(ui);
    let w = ui.available_width();
    let compact = w < 420.0;
    egui::Frame::new()
        .fill(p.info_subtle)
        .corner_radius(R_MD)
        .inner_margin(egui::Margin { left: 12, right: 8, top: 8, bottom: 8 })
        .show(ui, |ui| {
            ui.set_width(w - 20.0);
            ui.horizontal_top(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                icons::paint(ui.painter(), r, Icon::Info, p.info);
                ui.add_space(2.0);
                let text_w = if compact { ui.available_width() - 44.0 } else { ui.available_width() - 160.0 };
                ui.vertical(|ui| {
                    ui.set_width(text_w.max(80.0));
                    ui.spacing_mut().item_spacing.y = 2.0;
                    text::label(ui, "Identify your fans", Ts::BodyStrong, p.text1);
                    let what = match (compact, held) {
                        (true, true) => "Fan modes wait for this: every fan stays on the BIOS until then.".to_string(),
                        (true, false) => "Needed before any fan mode other than BIOS.".to_string(),
                        (false, true) => format!(
                            "{} not confirmed yet. The {} mode waits for this: every fan stays on the BIOS until then.",
                            unknown.join(", "),
                            pc.cfg.fan_mode().label
                        ),
                        (false, false) => format!(
                            "{} not confirmed yet. Fan modes other than BIOS start once every header is identified.",
                            unknown.join(", ")
                        ),
                    };
                    text::para(ui, what, Ts::Caption, p.text2);
                    if compact {
                        ui.add_space(4.0);
                        if controls::Button::new(controls::Kind::Secondary, "Identify…")
                            .size(controls::Size::Sm)
                            .show(ui)
                            .clicked()
                        {
                            pc.ui.pending.push(UiAction::IdentifyHeaders);
                        }
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    if !held
                        && controls::icon_button_sized(
                            ui,
                            Icon::Close,
                            "Later (the Fans menu on Cooling keeps it)",
                            26.0,
                            None,
                        )
                        .clicked()
                    {
                        pc.prefs.identify_prompt_dismissed = true;
                    }
                    if !compact
                        && controls::Button::new(controls::Kind::Secondary, "Identify…")
                            .size(controls::Size::Sm)
                            .show(ui)
                            .clicked()
                    {
                        pc.ui.pending.push(UiAction::IdentifyHeaders);
                    }
                });
            });
        });
    ui.add_space(6.0);
}

fn fans_card(ui: &mut Ui, pc: &mut PageCtx, min_h: f32) -> f32 {
    let p = pal(ui);
    let fans: Vec<_> = pc.st.fans.iter().filter(|f| f.visible).cloned().collect();
    card_eq(ui, min_h, 14, |ui| {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            header(ui, "Fans", None, |ui| {
                if controls::link(ui, "Cooling ›").clicked() {
                    pc.ui.pending.push(UiAction::Navigate(Page::Cooling));
                }
            });
        });
        ui.add_space(2.0);
        identify_prompt(ui, pc);
        if fans.is_empty() {
            ui.add_space(4.0);
            match fan_block(pc.st, &pc.ui.issues) {
                Some(code) => {
                    text::para(ui, format!("{} ({}).", code.title(), code.id()), Ts::Body, p.text2);
                    if controls::Button::new(controls::Kind::Primary, code.fix().label())
                        .size(controls::Size::Sm)
                        .show(ui)
                        .clicked()
                    {
                        pc.ui.pending.push(UiAction::Fix(code.fix(), None));
                    }
                }
                None => {
                    text::para(ui, "No fan headers reported yet.", Ts::Body, p.text2);
                }
            }
        }
        for fan in &fans {
            fan_row(ui, pc, fan);
        }
    })
}

/// Subtitle under a fan's name: the silkscreen header, or what is known about it.
pub fn fan_subtitle(fan: &FanState) -> String {
    if fan.name != fan.header {
        return fan.header.clone();
    }
    match fan.assigned {
        Some(r) => r.label().to_string(),
        None if fan.controllable && !fan.unconnected => "Not identified yet".into(),
        None => "Read-only".into(),
    }
}

/// Fan row (Overview and the Cooling list): thumbnail, name + header, speed (or "— rpm · no speed
/// signal", neutral), duty bar.
pub fn fan_row(ui: &mut Ui, pc: &mut PageCtx, fan: &FanState) {
    let p = pal(ui);
    let w = ui.available_width();
    let (r, resp) = ui.allocate_exact_size(vec2(w, 52.0), Sense::click());
    let selected = pc.ui.selected_fan.as_deref() == Some(fan.id.as_str()) && *pc.page == Page::Cooling;
    if selected {
        ui.painter().rect_filled(r, R_MD, p.accent_subtle);
    } else if resp.hovered() {
        ui.painter().rect_filled(r, R_MD, p.surface2);
    }
    if resp.has_focus() {
        ui.painter().rect_stroke(r, R_MD, Stroke::new(2.0, p.focus), StrokeKind::Inside);
    }
    let ctx = ui.ctx().clone();
    let tr = Rect::from_center_size(pos2(r.left() + 24.0, r.center().y), vec2(36.0, 36.0));
    thumb::paint(&ctx, ui.painter(), pc.assets, p, tr, thumb::thumb_for(fan), None, fan.assigned.is_none());
    if fan.failsafe {
        ui.painter().circle_filled(tr.right_top(), 5.0, p.danger);
    }
    let painter = ui.painter();
    let nx = r.left() + 52.0;
    painter.text(pos2(nx, r.top() + 17.0), Align2::LEFT_CENTER, &fan.name, font(Ts::BodyStrong), p.text1);
    painter.text(pos2(nx, r.top() + 36.0), Align2::LEFT_CENTER, fan_subtitle(fan), font(Ts::Caption), p.text3);
    // right side: value + caption, then the duty bar (wide rows only)
    let bar_w = if w > 420.0 { (w * 0.22).min(130.0) } else { 0.0 };
    let right = r.right() - 10.0;
    if bar_w > 0.0 {
        let bar = Rect::from_min_size(pos2(right - bar_w, r.center().y - 2.0), vec2(bar_w, 4.0));
        let color = if fan.failsafe {
            p.danger
        } else if fan.controlled {
            p.accent
        } else {
            p.text3
        };
        controls::paint_bar(painter, bar, fan.duty.or(fan.duty_est).unwrap_or(0.0) / 100.0, color, p.surface3);
    }
    let tx = right - bar_w - if bar_w > 0.0 { 18.0 } else { 0.0 };
    // No tach (e.g. the AIO pump on CPU_FAN): show its speed in %, never a meaningless rpm.
    let (speed, speed_c, sub) = if fan.no_speed_signal() {
        match super::no_tach_speed(fan) {
            Some((v, _)) => (format!("{v} %"), p.text1, "no speed signal".to_string()),
            None => ("—".to_string(), p.text3, "no speed signal".to_string()),
        }
    } else {
        (widgets::fmt_rpm(fan.rpm), p.text1, super::duty_caption(fan))
    };
    let sub = if fan.failsafe { "Safety 100 %".to_string() } else { sub };
    painter.text(pos2(tx, r.top() + 17.0), Align2::RIGHT_CENTER, speed, tab(Ts::BodyStrong), speed_c);
    painter.text(pos2(tx, r.top() + 36.0), Align2::RIGHT_CENTER, sub, tab(Ts::Caption), p.text2);
    let label = format!("{}, {}, open in Cooling", fan.name, safety::speed_words(fan));
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
    let resp = if fan.no_speed_signal() { resp.on_hover_text(super::NO_SPEED_SIGNAL_HELP) } else { resp };
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    if resp.clicked() {
        pc.ui.pending.push(UiAction::SelectFan(fan.id.clone()));
    }
}

fn history_card(ui: &mut Ui, pc: &mut PageCtx) {
    let p = pal(ui);
    let f = pc.cfg.general.fahrenheit;
    let group = Id::new("ov_hist_group");
    card(ui, |ui| {
        let temps_id = Id::new("ov_temps");
        let mut series: Vec<ChartSeries> = Vec::new();
        for (id, name) in [
            (ids::CPU_TEMP, "CPU"),
            (ids::GPU_TEMP, "GPU"),
            ("mb.t4", "VRM"),
            ("mb.t1", "Chipset"),
            ("mb.t0", "System"),
        ] {
            if pc.st.sensor(id).is_some() {
                series.push(ChartSeries { id: id.into(), name: name.into(), color: series_for(p, id) });
            }
        }
        ui.horizontal(|ui| {
            text::label(ui, "History", Ts::Headline, p.text1);
            ui.add_space(8.0);
            if pc.bp != Breakpoint::Narrow {
                chart::legend(ui, temps_id, &series);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                range_selector(ui, pc);
            });
        });
        if pc.bp == Breakpoint::Narrow {
            chart::legend(ui, temps_id, &series);
        }
        ui.add_space(6.0);
        let range = pc.prefs.chart_range_s as f64;
        let crit = pc.cfg.general.cpu_critical;
        let mut thresholds = vec![Threshold {
            value: crit,
            color: p.danger,
            label: format!("critical {}", widgets::fmt_deg(Some(crit), f)),
        }];
        if let Some(z) = pc.st.ai.first() {
            thresholds.push(Threshold {
                value: z.target,
                color: p.accent,
                label: format!("target {}", widgets::fmt_deg(Some(z.target), f)),
            });
        }
        let map: fn(f32) -> f32 = if f { crate::util::c_to_f } else { |v| v };
        TimeChart {
            id: temps_id,
            group: Some(group),
            height: 180.0,
            range_s: range,
            series: &series,
            thresholds: &thresholds,
            unit: "°",
            decimals: 1,
            min_span: 10.0,
            floor: None,
            map,
        }
        .show(ui, pc.hist);
        ui.add_space(10.0);
        let fan_id = Id::new("ov_fans");
        let fans: Vec<ChartSeries> = pc
            .st
            .fans
            .iter()
            // A header without a tach would draw a flat 0 rpm line.
            .filter(|x| x.visible && !x.no_speed_signal())
            .map(|x| ChartSeries { id: rpm_id(x), name: x.name.clone(), color: fan_color(p, x.index) })
            .collect();
        ui.horizontal(|ui| {
            text::label(ui, "Fan speeds", Ts::Label, p.text2);
            ui.add_space(8.0);
            chart::legend(ui, fan_id, &fans);
        });
        TimeChart {
            id: fan_id,
            group: Some(group),
            height: 120.0,
            range_s: range,
            series: &fans,
            thresholds: &[],
            unit: " rpm",
            decimals: 0,
            min_span: 400.0,
            floor: Some(0.0),
            map: |v| v,
        }
        .show(ui, pc.hist);
    });
}

pub fn range_selector(ui: &mut Ui, pc: &mut PageCtx) {
    let mut r = pc.prefs.chart_range_s;
    if controls::segmented_sized(
        ui,
        Id::new("chart_range"),
        &mut r,
        &[(60u32, "1m"), (300, "5m"), (900, "15m")],
        None,
        28.0,
    )
    .changed()
    {
        pc.prefs.chart_range_s = r;
    }
}
