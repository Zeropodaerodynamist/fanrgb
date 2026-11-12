//! Intelligence: "What is the learning controller doing and can I trust it?"

use std::f32::consts::{PI, TAU};

use egui::{Align2, Id, Pos2, Rect, Sense, Shape, Stroke, Ui, pos2, vec2};

use super::{PageCtx, area_banners, emergency_banner, fan_block, fan_block_state};
use crate::config::{AiConfig, AiStyle, FanMode, Preset};
use crate::engine::AiZoneView;
use crate::ui::actions::UiAction;
use crate::ui::fonts::{Ts, font, tab};
use crate::ui::icons::{self, Icon};
use crate::ui::issues::Area;
use crate::ui::layout::{self, Breakpoint};
use crate::ui::overlays::{Modal, ToastKind};
use crate::ui::theme::{R_SM, pal, series_for, thermal, with_alpha};
use crate::ui::widgets::card::{card, card_eq, empty_state};
use crate::ui::widgets::chart::{at, nice_step, window_start};
use crate::ui::widgets::controls::{self, Button, Kind, Size, Tone, segmented};
use crate::ui::widgets::{self, text};

pub fn show(ui: &mut Ui, pc: &mut PageCtx) {
    let gap = pc.bp.gap();
    ui.add_space(4.0);
    if let Some(code) = fan_block(pc.st, &pc.ui.issues) {
        fan_block_state(ui, pc, code, "The learning controller");
        return;
    }
    area_banners(ui, pc, &[Area::Intelligence], &[]);
    emergency_banner(ui, pc);
    let zones = pc.st.ai.clone();
    if zones.is_empty() {
        empty(ui, pc);
        return;
    }
    if pc.ui.ai_zone >= zones.len() {
        pc.ui.ai_zone = 0;
    }
    // ---- zone tabs + style ----
    let z = zones[pc.ui.ai_zone].clone();
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
        for (i, zz) in zones.iter().enumerate() {
            let label = if zz.group.is_empty() {
                zz.fans.join(" + ")
            } else {
                format!("{} · {}", zz.group, zz.fans.join(" + "))
            };
            if controls::filter_chip(ui, &label, i == pc.ui.ai_zone).clicked() {
                pc.ui.ai_zone = i;
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let mut style = z.style.unwrap_or(AiStyle::Balanced);
            let old = style;
            segmented(
                ui,
                Id::new("ai_zone_style"),
                &mut style,
                &[(AiStyle::Silent, "Silent"), (AiStyle::Balanced, "Balanced"), (AiStyle::Performance, "Performance")],
                Some(300.0),
            );
            if style != old {
                set_zone_style(pc, &z, style);
            }
            text::label(ui, "Style", Ts::Label, pal(ui).text2);
        });
    });
    ui.add_space(gap * 0.75);
    hero(ui, pc, &z);
    ui.add_space(gap);
    learned(ui, pc, &z);
    ui.add_space(gap);
    controls_card(ui, pc, &z);
}

fn zone_fans<'a>(pc: &'a PageCtx, z: &AiZoneView) -> Vec<&'a crate::model::FanState> {
    pc.st.fans.iter().filter(|f| z.fans.contains(&f.name)).collect()
}

fn set_zone_style(pc: &mut PageCtx, z: &AiZoneView, style: AiStyle) {
    let fans: Vec<(String, crate::model::FanRole)> = zone_fans(pc, z).iter().map(|f| (f.id.clone(), f.role)).collect();
    for (id, role) in fans {
        let fc = pc.cfg.profile_mut().fan_mut(&id, role);
        let d = AiConfig::for_role(style, role);
        fc.ai.style = style;
        fc.ai.target = d.target;
        fc.ai.limit = d.limit;
    }
}

fn empty(ui: &mut Ui, pc: &mut PageCtx) {
    let p = pal(ui);
    empty_state(
        ui,
        Icon::Intelligence,
        Some(p.accent),
        "Let FanRGB learn your cooling",
        "It learns how your temperatures react to load and airflow, then runs the fans as slowly as possible while staying under your target.",
        |ui| {
            // 3-step explainer
            let w = ui.available_width().min(420.0);
            let (r, _) = ui.allocate_exact_size(vec2(w, 74.0), Sense::hover());
            let steps = [(Icon::Eye, "Observe"), (Icon::Intelligence, "Model"), (Icon::Cooling, "Quietest safe speed")];
            for (k, (icon, label)) in steps.iter().enumerate() {
                let cx = r.left() + w * (k as f32 + 0.5) / 3.0;
                let c = pos2(cx, r.top() + 22.0);
                ui.painter().circle_filled(c, 20.0, p.accent_subtle);
                icons::paint(ui.painter(), Rect::from_center_size(c, vec2(20.0, 20.0)), *icon, p.accent);
                ui.painter().text(pos2(cx, r.top() + 58.0), Align2::CENTER_CENTER, *label, font(Ts::Caption), p.text2);
                if k < 2 {
                    let x0 = c.x + 28.0;
                    let x1 = r.left() + w * (k as f32 + 1.5) / 3.0 - 28.0;
                    ui.painter().line_segment([pos2(x0, c.y), pos2(x1, c.y)], Stroke::new(1.5, p.border_strong));
                    icons::paint(
                        ui.painter(),
                        Rect::from_center_size(pos2(x1, c.y), vec2(12.0, 12.0)),
                        Icon::ChevronRight,
                        p.border_strong,
                    );
                }
            }
            ui.add_space(10.0);
            let sid = Id::new("ai_empty_style");
            let mut style: AiStyle = ui.data(|d| d.get_temp(sid)).unwrap_or(AiStyle::Balanced);
            segmented(
                ui,
                sid.with("seg"),
                &mut style,
                &[(AiStyle::Silent, "Silent"), (AiStyle::Balanced, "Balanced"), (AiStyle::Performance, "Performance")],
                Some(320.0),
            );
            ui.data_mut(|d| d.insert_temp(sid, style));
            ui.add_space(10.0);
            if Button::new(Kind::Primary, "Use AI for all fans").size(Size::Lg).icon(Icon::Play).show(ui).clicked() {
                let idx =
                    pc.cfg.profiles.iter().position(|pr| pr.preset == Preset::AiAuto).unwrap_or(pc.cfg.active_profile);
                pc.cfg.active_profile = idx;
                let fans: Vec<_> =
                    pc.st.fans.iter().filter(|f| f.controllable).map(|f| (f.id.clone(), f.role)).collect();
                let prof = pc.cfg.profile_mut();
                for (id, role) in fans {
                    let fc = prof.fan_mut(&id, role);
                    fc.mode = FanMode::Ai;
                    let d = AiConfig::for_role(style, role);
                    fc.ai.style = style;
                    fc.ai.target = d.target;
                    fc.ai.limit = d.limit;
                }
                pc.cfg.general.welcome_done = true;
                if pc.st.emergency_released {
                    pc.ui.pending.push(UiAction::Resume);
                }
                pc.ui.toast(ToastKind::Success, format!("AI control on ({})", style.label()));
            }
        },
    );
}

fn confidence_ring(ui: &mut Ui, value: f32, size: f32) {
    let p = pal(ui);
    let (r, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    let c = r.center();
    let rad = size * 0.4;
    let w = size * 0.1;
    ui.painter().circle_stroke(c, rad, Stroke::new(w, p.surface3));
    let f = value.clamp(0.0, 1.0);
    if f > 0.005 {
        let n = (f * 90.0).ceil() as usize + 2;
        let pts: Vec<Pos2> = (0..=n)
            .map(|i| {
                let a = -PI / 2.0 + TAU * f * i as f32 / n as f32;
                c + vec2(a.cos(), a.sin()) * rad
            })
            .collect();
        let color = if value > 0.6 {
            p.success
        } else if value > 0.3 {
            p.warning
        } else {
            p.danger
        };
        ui.painter().add(Shape::line(pts, Stroke::new(w, color)));
    }
    ui.painter().text(c, Align2::CENTER_CENTER, format!("{:.0}", value * 100.0), tab(Ts::Label), p.text1);
}

fn hero(ui: &mut Ui, pc: &mut PageCtx, z: &AiZoneView) {
    let p = pal(ui);
    let f = pc.cfg.general.fahrenheit;
    card(ui, |ui| {
        let narrow = pc.bp == Breakpoint::Narrow;
        let stat =
            |ui: &mut Ui, label: &str, value: String, color| controls::stat(ui, label, &value, color, Ts::Display);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(if narrow { 24.0 } else { 40.0 }, 10.0);
            stat(ui, "Now", widgets::fmt_deg(Some(z.temp), f), thermal(p, z.temp, pc.cfg.general.cpu_critical));
            stat(
                ui,
                "Target / limit",
                format!("{} / {}", widgets::fmt_deg(Some(z.target), f), widgets::fmt_deg(Some(z.limit), f)),
                p.text1,
            );
            stat(ui, "Fans", format!("{:.0} %", z.duty), p.accent);
            ui.horizontal(|ui| {
                confidence_ring(ui, z.confidence, 52.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    text::label(ui, "Confidence", Ts::Caption, p.text2);
                    text::label(ui, format!("{} of 6 load changes", z.n_inf.min(6)), Ts::Caption, p.text3);
                });
            })
            .response
            .on_hover_text(if z.confidence_why.is_empty() {
                "How much the model has proven itself".to_string()
            } else {
                z.confidence_why.clone()
            });
        });
        ui.add_space(8.0);
        text::para(ui, &z.reason, Ts::BodyStrong, p.text1);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if !z.workload.is_empty() {
                controls::chip(ui, &z.workload, Tone::Neutral).on_hover_text("Detected workload");
            }
            if z.ceiling_active
                && let Some(c) = z.quiet_ceiling
            {
                controls::chip(ui, &format!("Quiet ceiling {:.0} %", c * 100.0), Tone::Accent)
                    .on_hover_text("Learned quiet limit for this workload and time of day");
            }
            if z.latched {
                controls::chip(ui, "Holding", Tone::Neutral);
            }
            if z.probing {
                controls::chip(ui, "Probing", Tone::Info)
                    .on_hover_text("Briefly testing a different speed to learn the fan's effect");
            }
            if z.stale {
                controls::chip(ui, "Stale model", Tone::Warning);
            }
            if z.distrusted {
                controls::chip(ui, "Following safe curve", Tone::Warning)
                    .on_hover_text("The model's predictions are not trusted right now");
            }
            if (z.model_airflow - z.airflow).abs() > 0.02 {
                controls::chip(ui, &format!("Model alone: {:.0} %", z.model_airflow * 100.0), Tone::Neutral);
            }
        });
        ui.add_space(8.0);
        prediction_chart(ui, pc, z, if narrow { 180.0 } else { 220.0 });
    });
}

/// History of the zone's (combined) source temperature, from the in-place history.
fn combined_history(pc: &PageCtx, z: &AiZoneView, secs: f64) -> Vec<(f64, f32)> {
    let mut parts = z.key.splitn(2, ':');
    let combine = parts.next().unwrap_or("Max");
    let ids: Vec<&str> =
        parts.next().and_then(|s| s.split('|').next()).map(|s| s.split(',').collect()).unwrap_or_default();
    let h = pc.hist;
    let n = h.times.len();
    let start = window_start(&h.times, secs);
    let series: Vec<_> = ids.iter().filter_map(|id| h.series.get(*id)).collect();
    let end = h.times.back().copied().unwrap_or(0.0);
    (start..n)
        .filter_map(|i| {
            let vals: Vec<f32> = series.iter().map(|s| at(n, s, i)).filter(|v| v.is_finite()).collect();
            if vals.is_empty() {
                return None;
            }
            let v = if combine == "Average" {
                vals.iter().sum::<f32>() / vals.len() as f32
            } else {
                vals.iter().copied().fold(f32::MIN, f32::max)
            };
            Some((h.times[i] - end, v))
        })
        .collect()
}

fn prediction_chart(ui: &mut Ui, pc: &PageCtx, z: &AiZoneView, height: f32) {
    let p = pal(ui);
    let f = pc.cfg.general.fahrenheit;
    let past = 120.0f32;
    let step = if z.forecast_step_s > 0.0 { z.forecast_step_s } else { 5.0 };
    let future = (z.predicted.len() as f32 * step).clamp(30.0, 180.0);
    let hist = combined_history(pc, z, past as f64);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let plot = Rect::from_min_max(rect.min + vec2(36.0, 18.0), rect.max - vec2(8.0, 20.0));
    let painter = ui.painter_at(rect);
    painter.rect_filled(plot.expand(3.0), R_SM, p.inset);
    let conv = |v: f32| if f { crate::util::c_to_f(v) } else { v };
    let fut: Vec<f32> = z.predicted.iter().take((future / step) as usize).copied().collect();
    let all = hist.iter().map(|h| h.1).chain(fut.iter().copied()).chain([z.target, z.limit]);
    let (mut lo, mut hi) = all.fold((f32::MAX, f32::MIN), |(a, b), v| (a.min(v), b.max(v)));
    lo = (lo - z.err_h.max(1.0) - 2.0).floor();
    hi = (hi + 2.0).ceil();
    let span = (hi - lo).max(5.0);
    let to_y = |v: f32| plot.bottom() - (v - lo) / span * plot.height();
    let total = past + future;
    let to_x = |t: f32| plot.left() + (t + past) / total * plot.width();
    let now_x = to_x(0.0);
    // grid
    let gs = nice_step(span, 4.0);
    let mut g = (lo / gs).ceil() * gs;
    while g <= hi {
        let y = to_y(g);
        painter.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], Stroke::new(1.0, p.border_subtle));
        painter.text(
            pos2(plot.left() - 6.0, y),
            Align2::RIGHT_CENTER,
            format!("{:.0}", conv(g)),
            tab(Ts::Caption),
            p.text3,
        );
        g += gs;
    }
    // target / limit
    for (v, c, name) in [(z.target, p.accent, "target"), (z.limit, p.danger, "limit")] {
        let y = to_y(v);
        painter.extend(Shape::dashed_line(
            &[pos2(plot.left(), y), pos2(plot.right(), y)],
            Stroke::new(1.0, with_alpha(c, 0.7)),
            4.0,
            4.0,
        ));
        painter.text(
            pos2(plot.right() - 4.0, y - 3.0),
            Align2::RIGHT_BOTTOM,
            format!("{name} {}", widgets::fmt_deg(Some(v), f)),
            font(Ts::Caption),
            c,
        );
    }
    // now divider + labels
    painter.line_segment([pos2(now_x, plot.top()), pos2(now_x, plot.bottom())], Stroke::new(1.0, p.border_strong));
    painter.text(
        pos2(now_x - 6.0, rect.top() + 2.0),
        Align2::RIGHT_TOP,
        format!("past {past:.0} s"),
        font(Ts::Caption),
        p.text3,
    );
    painter.text(
        pos2(now_x + 6.0, rect.top() + 2.0),
        Align2::LEFT_TOP,
        format!("forecast {future:.0} s"),
        font(Ts::Caption),
        p.accent,
    );
    painter.text(pos2(now_x, plot.bottom() + 4.0), Align2::CENTER_TOP, "now", font(Ts::Caption), p.text3);
    // uncertainty band
    if !fut.is_empty() {
        let mut mesh = egui::epaint::Mesh::default();
        let c = with_alpha(p.accent, 0.14);
        let mut prev: Option<(f32, f32)> = Some((0.0, z.temp));
        for (k, v) in fut.iter().enumerate() {
            let t = (k + 1) as f32 * step;
            if let Some((t0, v0)) = prev {
                let e0 = z.err_h * (t0 / 60.0).clamp(0.2, 2.0);
                let e1 = z.err_h * (t / 60.0).clamp(0.2, 2.0);
                let i = mesh.vertices.len() as u32;
                mesh.colored_vertex(pos2(to_x(t0), to_y(v0 + e0)), c);
                mesh.colored_vertex(pos2(to_x(t), to_y(v + e1)), c);
                mesh.colored_vertex(pos2(to_x(t), to_y(v - e1)), c);
                mesh.colored_vertex(pos2(to_x(t0), to_y(v0 - e0)), c);
                mesh.add_triangle(i, i + 1, i + 2);
                mesh.add_triangle(i, i + 2, i + 3);
            }
            prev = Some((t, *v));
        }
        painter.add(Shape::mesh(mesh));
    }
    // history (solid)
    let color = series_for(p, "cpu.temp");
    let pts: Vec<Pos2> = hist.iter().map(|(t, v)| pos2(to_x(*t as f32), to_y(*v))).collect();
    if pts.len() > 1 {
        painter.add(Shape::line(pts.clone(), Stroke::new(2.0, color)));
    }
    // forecast (dashed)
    if !fut.is_empty() {
        let mut fp = vec![pos2(now_x, to_y(z.temp))];
        for (k, v) in fut.iter().enumerate() {
            fp.push(pos2(to_x((k + 1) as f32 * step), to_y(*v)));
        }
        painter.extend(Shape::dashed_line(&fp, Stroke::new(2.0, p.accent), 6.0, 4.0));
    }
    painter.circle_filled(pos2(now_x, to_y(z.temp)), 4.0, color);
    if resp.hovered() {
        let _ = resp.on_hover_text(format!(
            "Forecast: peak {} · {} at the end · ±{:.1}° typical error",
            widgets::fmt_deg(Some(z.predicted_peak), f),
            widgets::fmt_deg(Some(z.predicted_end), f),
            z.err_h
        ));
    }
}

fn learned(ui: &mut Ui, pc: &mut PageCtx, z: &AiZoneView) {
    let p = pal(ui);
    let gap = pc.bp.gap();
    let trained = z.samples > 24;
    let narrow = pc.bp == Breakpoint::Narrow;
    text::overline(ui, "What it learned");
    ui.add_space(4.0);
    let card1 = |ui: &mut Ui, min_h: f32| {
        card_eq(ui, min_h, 16, |ui| {
            text::label(ui, "Heat per 100 W", Ts::Headline, p.text1);
            text::label(ui, "Steady temperature rise at full load", Ts::Caption, p.text2);
            ui.add_space(6.0);
            let max = z.rise.iter().copied().fold(1.0, f32::max);
            for (k, a) in ["30 % air", "60 % air", "100 % air"].iter().enumerate() {
                let w = ui.available_width();
                let (r, _) = ui.allocate_exact_size(vec2(w, 24.0), Sense::hover());
                ui.painter().text(pos2(r.left(), r.center().y), Align2::LEFT_CENTER, *a, font(Ts::Caption), p.text2);
                let bar = Rect::from_min_max(
                    pos2(r.left() + 70.0, r.center().y - 4.0),
                    pos2(r.right() - 44.0, r.center().y + 4.0),
                );
                controls::paint_bar(
                    ui.painter(),
                    bar,
                    if trained { z.rise[k] / max } else { 0.0 },
                    p.accent,
                    p.surface3,
                );
                ui.painter().text(
                    pos2(r.right(), r.center().y),
                    Align2::RIGHT_CENTER,
                    if trained { format!("{:.0}°", z.rise[k]) } else { "—".into() },
                    tab(Ts::Label),
                    p.text1,
                );
            }
        })
    };
    let card2 = |ui: &mut Ui, min_h: f32| {
        card_eq(ui, min_h, 16, |ui| {
            text::label(ui, "Thermal inertia", Ts::Headline, p.text1);
            text::label(
                ui,
                &if trained {
                    format!("τ ≈ {:.0} s · short spikes are absorbed", z.time_constant)
                } else {
                    "Collecting data…".into()
                },
                Ts::Caption,
                p.text2,
            );
            ui.add_space(6.0);
            let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 64.0), Sense::hover());
            ui.painter().rect_filled(r, R_SM, p.inset);
            let tau = z.time_constant.max(5.0);
            let horizon = tau * 4.0;
            let pts: Vec<Pos2> = (0..=40)
                .map(|i| {
                    let t = horizon * i as f32 / 40.0;
                    let v = 1.0 - (-t / tau).exp();
                    pos2(r.left() + 8.0 + (r.width() - 16.0) * t / horizon, r.bottom() - 8.0 - v * (r.height() - 16.0))
                })
                .collect();
            ui.painter().add(Shape::line(pts, Stroke::new(1.75, if trained { p.accent } else { p.text3 })));
            let xt = r.left() + 8.0 + (r.width() - 16.0) / 4.0;
            ui.painter().extend(Shape::dashed_line(
                &[pos2(xt, r.top() + 6.0), pos2(xt, r.bottom() - 6.0)],
                Stroke::new(1.0, p.text3),
                3.0,
                3.0,
            ));
            ui.painter().text(pos2(xt + 4.0, r.top() + 6.0), Align2::LEFT_TOP, "τ", font(Ts::Caption), p.text3);
        })
    };
    let card3 = |ui: &mut Ui, min_h: f32| {
        card_eq(ui, min_h, 16, |ui| {
            text::label(ui, "Diminishing returns", Ts::Headline, p.text1);
            let gain = (z.rise[1] - z.rise[2]).max(0.0);
            let msg = if !trained {
                "Not enough data yet.".to_string()
            } else if gain < 3.0 {
                format!(
                    "Above 60 % airflow cools less than {:.0}° more, so it stays under 60 % unless needed.",
                    gain.max(1.0)
                )
            } else {
                format!("More airflow still helps here: 60 → 100 % is worth about {gain:.0}°.")
            };
            ui.add_space(4.0);
            text::para(ui, &msg, Ts::Body, p.text2);
        })
    };
    if narrow {
        layout::stack(ui, 3, gap * 0.75, |i, ui| {
            match i {
                0 => card1(ui, 0.0),
                1 => card2(ui, 0.0),
                _ => card3(ui, 0.0),
            };
        });
    } else {
        layout::row(ui, Id::new("ai_learned"), &[1.0, 1.0, 1.0], gap, |i, ui, min_h| match i {
            0 => card1(ui, min_h),
            1 => card2(ui, min_h),
            _ => card3(ui, min_h),
        });
    }
    if !z.params.is_empty() {
        ui.add_space(gap * 0.75);
        card(ui, |ui| {
            text::label(ui, "Model parameters", Ts::Headline, p.text1);
            ui.add_space(4.0);
            egui::Grid::new("ai_params").num_columns(2).spacing([24.0, 6.0]).striped(false).show(ui, |ui| {
                for prm in &z.params {
                    let r = text::label(ui, &prm.name, Ts::Body, p.text2);
                    if !prm.help.is_empty() {
                        r.on_hover_text(&prm.help);
                    }
                    let v = match prm.sd {
                        Some(sd) => format!("{:.2} ± {:.2} {}", prm.value, sd, prm.unit),
                        None => format!("{:.2} {}", prm.value, prm.unit),
                    };
                    ui.add(
                        egui::Label::new(egui::RichText::new(v).font(tab(Ts::Body)).color(p.text1)).selectable(false),
                    );
                    ui.end_row();
                }
            });
        });
    }
}

fn controls_card(ui: &mut Ui, pc: &mut PageCtx, z: &AiZoneView) {
    let p = pal(ui);
    let fans: Vec<(String, crate::model::FanRole)> = zone_fans(pc, z).iter().map(|f| (f.id.clone(), f.role)).collect();
    card(ui, |ui| {
        text::label(ui, "Targets", Ts::Headline, p.text1);
        text::label(ui, "Shared by every fan in this zone (same settings as Cooling › AI)", Ts::Caption, p.text2);
        ui.add_space(6.0);
        if let Some((id0, role0)) = fans.first().cloned() {
            let mut fc = pc.cfg.profile().fan(&id0, role0);
            let before = fc.clone();
            let cap = fans.iter().map(|(_, r)| super::ai_limit_cap(*r)).fold(95.0, f32::min);
            super::ai_target_sliders(ui, Id::new(("zone_ai", &z.key)), &mut fc.ai, cap);
            if fc != before {
                for (id, role) in &fans {
                    let f = pc.cfg.profile_mut().fan_mut(id, *role);
                    f.ai.target = fc.ai.target;
                    f.ai.limit = fc.ai.limit;
                }
            }
        }
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            let s = format!("Learning: {} samples · validated ±{:.1}°", widgets::thousands(z.samples as f32), z.err_h);
            text::label(ui, &s, Ts::Caption, p.text2);
            if !z.learning.is_empty() {
                text::label(ui, format!("· {}", z.learning), Ts::Caption, p.text3);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if Button::new(Kind::Danger, "Reset this model…").size(Size::Sm).show(ui).clicked() {
                    pc.ui.modal = Some(Modal::ConfirmResetModel {
                        key: Some(z.key.clone()),
                        what: format!(
                            "{}'s model ({} samples)",
                            z.fans.join(" + "),
                            widgets::thousands(z.samples as f32)
                        ),
                    });
                }
            });
        });
    });
}
