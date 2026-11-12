//! Sensors: "What is reading X right now / what was the peak?"

use std::collections::BTreeMap;

use egui::{Align2, Id, Rect, Sense, Stroke, Ui, pos2, vec2};

use super::{PageCtx, short_group, value_text};
use crate::model::{SensorEntry, SensorKind};
use crate::ui::actions::UiAction;
use crate::ui::fonts::{Ts, font, tab};
use crate::ui::icons::{self, Icon};
use crate::ui::layout::{self, Breakpoint};
use crate::ui::overlays::ToastKind;
use crate::ui::theme::{R_LG, R_MD, pal, series_for, thermal};
use crate::ui::widgets::card::{card, header};
use crate::ui::widgets::chart::{self, ChartSeries, TimeChart};
use crate::ui::widgets::controls::{self, Button, Kind, Size};
use crate::ui::widgets::text;

const KINDS: [(Option<SensorKind>, &str); 6] = [
    (None, "All"),
    (Some(SensorKind::Temperature), "Temperatures"),
    (Some(SensorKind::Fan), "Fans"),
    (Some(SensorKind::Voltage), "Voltages"),
    (Some(SensorKind::Power), "Power"),
    (Some(SensorKind::Load), "Load"),
];

/// Case-insensitive filter over name, group and id.
pub fn matches(s: &SensorEntry, filter: &str, kind: Option<SensorKind>) -> bool {
    let kind_ok = match kind {
        None => true,
        Some(SensorKind::Load) => matches!(s.desc.kind, SensorKind::Load | SensorKind::Duty),
        Some(k) => s.desc.kind == k,
    };
    if !kind_ok {
        return false;
    }
    let f = filter.trim().to_lowercase();
    f.is_empty() || format!("{} {} {}", s.desc.group, s.desc.name, s.desc.id).to_lowercase().contains(&f)
}

pub fn show(ui: &mut Ui, pc: &mut PageCtx) {
    let p = pal(ui);
    let gap = pc.bp.gap();
    ui.add_space(4.0);
    // ---- toolbar ----
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
        let sid = Id::new("sensor_search");
        let r = controls::search_field(ui, sid, &mut pc.ui.sensor_filter, "Filter sensors…", 240.0);
        if pc.ui.focus_search {
            r.request_focus();
            pc.ui.focus_search = false;
        }
        for (k, label) in KINDS {
            if controls::filter_chip(ui, label, pc.ui.sensor_kind == k).clicked() {
                pc.ui.sensor_kind = k;
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            text::label(
                ui,
                format!("{} sensors · poll {:.1} ms", pc.st.sensors.len(), pc.st.poll_ms),
                Ts::Caption,
                p.text3,
            );
        });
    });
    ui.add_space(gap * 0.75);
    if pc.st.sensors.is_empty() {
        text::label(ui, if pc.st.tick == 0 { "Reading sensors…" } else { "No sensors reported." }, Ts::Body, p.text2);
        return;
    }
    if super::fan_block(pc.st, &pc.ui.issues).is_some() {
        crate::ui::widgets::card::banner(
            ui,
            crate::ui::widgets::card::Sev::Info,
            "Motherboard and CPU sensors need PawnIO",
            "Rows below show what is available without the driver.",
            |_| {},
        );
        ui.add_space(gap * 0.75);
    }
    let selected = pc.ui.sensor_selected.clone().filter(|id| pc.st.sensor(id).is_some());
    if selected.is_some() && pc.bp == Breakpoint::Wide {
        let avail = ui.available_width();
        layout::row(ui, Id::new("sens_split"), &[avail - 340.0 - gap, 340.0], gap, |i, ui, _| {
            if i == 0 {
                table(ui, pc, true);
            } else {
                drawer(ui, pc);
            }
            0.0
        });
    } else {
        if selected.is_some() {
            drawer(ui, pc);
            ui.add_space(gap);
        }
        table(ui, pc, false);
    }
}

struct Cols {
    value: f32,
    min: f32,
    max: f32,
    spark: f32,
    id: f32,
}

fn table(ui: &mut Ui, pc: &mut PageCtx, compact: bool) {
    let p = pal(ui);
    let f = pc.cfg.general.fahrenheit;
    let narrow = pc.bp == Breakpoint::Narrow;
    let cols = Cols {
        value: 104.0,
        min: 96.0,
        max: 96.0,
        spark: if compact {
            0.0
        } else if narrow {
            96.0
        } else {
            150.0
        },
        id: if narrow || compact { 0.0 } else { 120.0 },
    };
    let mut groups: BTreeMap<String, Vec<&SensorEntry>> = BTreeMap::new();
    for s in &pc.st.sensors {
        if matches(s, &pc.ui.sensor_filter, pc.ui.sensor_kind) {
            groups.entry(s.desc.group.clone()).or_default().push(s);
        }
    }
    let frame = crate::ui::widgets::card::card_frame(ui, 8);
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 2.0;
        let width = ui.available_width();
        let header_rect = {
            let (r, _) = ui.allocate_exact_size(vec2(width, 32.0), Sense::hover());
            r
        };
        let paint_header = |painter: &egui::Painter, r: Rect| {
            painter.rect_filled(r, R_MD, p.surface1);
            let mut x = r.right() - 8.0;
            let y = r.center().y;
            let col = |painter: &egui::Painter, x: f32, label: &str| {
                painter.text(pos2(x, y), Align2::RIGHT_CENTER, label, crate::ui::fonts::font(Ts::Overline), p.text3);
            };
            if cols.id > 0.0 {
                painter.text(pos2(x - cols.id + 8.0, y), Align2::LEFT_CENTER, "ID", font(Ts::Overline), p.text3);
                x -= cols.id;
            }
            if cols.spark > 0.0 {
                painter.text(
                    pos2(x - cols.spark + 8.0, y),
                    Align2::LEFT_CENTER,
                    "LAST 5 MIN",
                    font(Ts::Overline),
                    p.text3,
                );
                x -= cols.spark;
            }
            col(painter, x - 8.0, "MAX");
            x -= cols.max;
            col(painter, x - 8.0, "MIN");
            x -= cols.min;
            col(painter, x - 8.0, "VALUE");
            painter.text(pos2(r.left() + 10.0, y), Align2::LEFT_CENTER, "NAME", font(Ts::Overline), p.text3);
            painter.line_segment([r.left_bottom(), r.right_bottom()], Stroke::new(1.0, p.border_subtle));
        };
        if groups.is_empty() {
            ui.add_space(12.0);
            text::label(ui, "No sensors match the filter.", Ts::Body, p.text3);
            ui.add_space(12.0);
        }
        for (group, list) in &groups {
            let collapsed = pc.ui.sensor_collapsed.contains(group);
            let (gr, gresp) = ui.allocate_exact_size(vec2(width, 34.0), Sense::click());
            if gresp.hovered() {
                ui.painter().rect_filled(gr, R_MD, p.surface2);
            }
            icons::paint(
                ui.painter(),
                Rect::from_center_size(pos2(gr.left() + 14.0, gr.center().y), vec2(14.0, 14.0)),
                if collapsed { Icon::ChevronRight } else { Icon::ChevronDown },
                p.text2,
            );
            let title = format!("{} · {}", short_group(group), group);
            let title = if short_group(group) == group.as_str() { group.clone() } else { title };
            ui.painter().text(
                pos2(gr.left() + 28.0, gr.center().y),
                Align2::LEFT_CENTER,
                format!("{title}  ({})", list.len()),
                font(Ts::BodyStrong),
                p.text1,
            );
            if gresp.clicked() {
                if collapsed {
                    pc.ui.sensor_collapsed.remove(group);
                } else {
                    pc.ui.sensor_collapsed.insert(group.clone());
                }
            }
            if collapsed {
                continue;
            }
            for s in list {
                row(ui, pc, s, &cols, width, f);
            }
        }
        // Sticky header: repaint it at the top of the visible area once scrolled past.
        let clip_top = ui.clip_rect().top();
        let table_bottom = ui.min_rect().bottom();
        let y = header_rect.top().max(clip_top).min(table_bottom - header_rect.height());
        let r = Rect::from_min_size(pos2(header_rect.left(), y), header_rect.size());
        paint_header(ui.painter(), r);
    });
    let _ = R_LG;
}

fn row(ui: &mut Ui, pc: &mut PageCtx, s: &SensorEntry, cols: &Cols, width: f32, f: bool) {
    let p = pal(ui);
    let (r, resp) = ui.allocate_exact_size(vec2(width, 36.0), Sense::click());
    if !ui.is_rect_visible(r) {
        return;
    }
    let selected = pc.ui.sensor_selected.as_deref() == Some(s.desc.id.as_str());
    if selected {
        ui.painter().rect_filled(r, R_MD, p.accent_subtle);
    } else if resp.hovered() {
        ui.painter().rect_filled(r, R_MD, p.surface2);
    }
    let k = s.desc.kind;
    let y = r.center().y;
    let mut x = r.right() - 8.0;
    if cols.id > 0.0 {
        ui.painter().text(pos2(x - cols.id + 8.0, y), Align2::LEFT_CENTER, &s.desc.id, font(Ts::Mono), p.text3);
        x -= cols.id;
    }
    if cols.spark > 0.0 {
        let sr = Rect::from_min_size(pos2(x - cols.spark + 8.0, y - 10.0), vec2(cols.spark - 16.0, 20.0));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(sr));
        let min_span = match k {
            SensorKind::Temperature => 4.0,
            SensorKind::Fan => 200.0,
            SensorKind::Load | SensorKind::Duty => 20.0,
            SensorKind::Voltage => 0.05,
            SensorKind::Power => 5.0,
        };
        chart::sparkline(&mut child, pc.hist, &s.desc.id, 300.0, sr.size(), series_for(p, &s.desc.id), min_span, |v| v);
        x -= cols.spark;
    }
    // A header without a tach never reads "0 rpm": min/max are "—", the value is its
    // speed in % (e.g. the AIO pump on CPU_FAN).
    let no_tach_fan =
        pc.st.fans.iter().find(|x| k == SensorKind::Fan && x.no_speed_signal() && super::rpm_id(x) == s.desc.id);
    let no_tach = no_tach_fan.is_some();
    let vt = |v: Option<f32>| if no_tach { "—".to_string() } else { value_text(v, k, f) };
    let now = match no_tach_fan.and_then(super::no_tach_speed) {
        Some((v, _)) => format!("{v} %"),
        None => vt(s.value),
    };
    ui.painter().text(pos2(x - 8.0, y), Align2::RIGHT_CENTER, vt(s.max), tab(Ts::Body), p.text2);
    x -= cols.max;
    ui.painter().text(pos2(x - 8.0, y), Align2::RIGHT_CENTER, vt(s.min), tab(Ts::Body), p.text2);
    x -= cols.min;
    let vc = if k == SensorKind::Temperature {
        s.value.map(|t| thermal(p, t, pc.cfg.general.cpu_critical)).unwrap_or(p.text3)
    } else {
        p.text1
    };
    let vr = ui.painter().text(pos2(x - 8.0, y), Align2::RIGHT_CENTER, now, tab(Ts::BodyStrong), p.text1);
    if k == SensorKind::Temperature && s.value.is_some() {
        ui.painter().circle_filled(pos2(vr.left() - 9.0, y), 3.5, vc);
    }
    let name = if k == SensorKind::Duty { format!("{} (speed)", s.desc.name) } else { s.desc.name.clone() };
    let name_w = (x - cols.value - r.left() - 24.0).max(40.0);
    let g = ui.painter().layout(name, font(Ts::Body), p.text1, name_w);
    ui.painter().galley(pos2(r.left() + 28.0, y - g.size().y / 2.0), g, p.text1);
    if resp.clicked() {
        pc.ui.sensor_selected = if selected { None } else { Some(s.desc.id.clone()) };
    }
}

fn drawer(ui: &mut Ui, pc: &mut PageCtx) {
    let p = pal(ui);
    let f = pc.cfg.general.fahrenheit;
    let Some(id) = pc.ui.sensor_selected.clone() else { return };
    let Some(s) = pc.st.sensor(&id).cloned() else { return };
    card(ui, |ui| {
        header(ui, &s.desc.name, Some(short_group(&s.desc.group)), |ui| {
            if controls::icon_button_sized(ui, Icon::Close, "Close", 28.0, None).clicked() {
                pc.ui.sensor_selected = None;
            }
        });
        let no_tach = s.desc.kind == SensorKind::Fan
            && pc.st.fans.iter().any(|x| x.no_speed_signal() && super::rpm_id(x) == s.desc.id);
        if no_tach {
            // Its speed in %, not rpm (the header has no tach).
            let fan = pc.st.fans.iter().find(|x| super::rpm_id(x) == s.desc.id);
            let (v, how) = fan.and_then(super::no_tach_speed).unwrap_or(("—".into(), "unknown"));
            text::value_unit(ui, &v, "%", Ts::Display, Ts::Caption, p.text1, p.text2);
            text::label(ui, format!("Running · no speed signal · {how}"), Ts::Caption, p.text2)
                .on_hover_text(super::NO_SPEED_SIGNAL_HELP);
        } else {
            ui.horizontal(|ui| {
                text::value_unit(
                    ui,
                    &value_text(s.value, s.desc.kind, f),
                    "",
                    Ts::Display,
                    Ts::Caption,
                    p.text1,
                    p.text2,
                );
            });
            text::label(
                ui,
                format!("min {} · max {}", value_text(s.min, s.desc.kind, f), value_text(s.max, s.desc.kind, f)),
                Ts::Caption,
                p.text2,
            );
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            super::overview::range_selector(ui, pc);
        });
        ui.add_space(4.0);
        let series =
            [ChartSeries { id: s.desc.id.clone(), name: s.desc.name.clone(), color: series_for(p, &s.desc.id) }];
        let temp = s.desc.kind == SensorKind::Temperature;
        let map: fn(f32) -> f32 = if temp && f { crate::util::c_to_f } else { |v| v };
        let unit = match s.desc.kind {
            SensorKind::Temperature => "°",
            SensorKind::Fan => " rpm",
            SensorKind::Voltage => " V",
            SensorKind::Power => " W",
            _ => " %",
        };
        TimeChart {
            id: Id::new(("drawer_chart", &id)),
            group: None,
            height: 200.0,
            range_s: pc.prefs.chart_range_s as f64,
            series: &series,
            thresholds: &[],
            unit,
            decimals: if s.desc.kind == SensorKind::Voltage { 3 } else { 1 },
            min_span: match s.desc.kind {
                SensorKind::Temperature => 6.0,
                SensorKind::Fan => 300.0,
                SensorKind::Voltage => 0.05,
                _ => 10.0,
            },
            floor: if matches!(s.desc.kind, SensorKind::Fan | SensorKind::Load | SensorKind::Duty) {
                Some(0.0)
            } else {
                None
            },
            map,
        }
        .show(ui, pc.hist);
        ui.add_space(8.0);
        text::label(ui, &s.desc.id, Ts::Mono, p.text3);
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            if Button::new(Kind::Secondary, "Copy id").size(Size::Sm).icon(Icon::Copy).show(ui).clicked() {
                ui.ctx().copy_text(id.clone());
                pc.ui.toast(ToastKind::Success, "Sensor id copied");
            }
            if temp {
                let r = Button::new(Kind::Secondary, "Use as fan source…").size(Size::Sm).icon(Icon::Cooling).show(ui);
                let fans: Vec<(String, String, crate::model::FanRole)> = pc
                    .st
                    .fans
                    .iter()
                    .filter(|f| f.controllable && f.visible)
                    .map(|f| (f.id.clone(), f.name.clone(), f.role))
                    .collect();
                egui::Popup::menu(&r).show(|ui| {
                    for (fid, name, role) in &fans {
                        if ui.button(name).clicked() {
                            let fc = pc.cfg.profile_mut().fan_mut(fid, *role);
                            fc.curve.source.sensors = vec![id.clone()];
                            fc.ai.source.sensors = vec![id.clone()];
                            pc.ui.toast_action(
                                ToastKind::Success,
                                format!("{name} now follows {}", s.desc.name),
                                "Open",
                                UiAction::SelectFan(fid.clone()),
                            );
                        }
                    }
                });
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SensorDesc;

    fn e(id: &str, name: &str, group: &str, kind: SensorKind) -> SensorEntry {
        SensorEntry { desc: SensorDesc::new(id, name, group, kind), value: Some(1.0), min: None, max: None }
    }

    #[test]
    fn filter_by_text_and_kind() {
        let cpu = e("cpu.temp", "CPU (Tctl/Tdie)", "CPU", SensorKind::Temperature);
        let fan = e("mb.fan0", "CPU_FAN", "Motherboard", SensorKind::Fan);
        let duty = e("mb.duty0", "CPU_FAN", "Motherboard", SensorKind::Duty);
        assert!(matches(&cpu, "tctl", None));
        assert!(matches(&cpu, "", Some(SensorKind::Temperature)));
        assert!(!matches(&fan, "", Some(SensorKind::Temperature)));
        assert!(matches(&fan, "cpu_fan", Some(SensorKind::Fan)));
        assert!(matches(&duty, "", Some(SensorKind::Load)), "duty shown under Load");
        assert!(!matches(&cpu, "gpu", None));
    }
}
