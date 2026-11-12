//! Hero stage: the rendered rig with its LEDs lit live from the lighting preview,
//! callouts that follow the configured header roles, and a lighting strip.
//!
//! Cost per frame: one base quad, ≤ 20 additive LED quads (texture × colour, no CPU image work) and
//! the callouts. The page repaints only on data ticks unless an animated effect runs while the
//! window is focused (`ui/mod.rs` repaint policy).

use egui::epaint::{Mesh, Vertex, WHITE_UV};
use egui::{Align2, Color32, CornerRadius, Id, Rect, Sense, Shape, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::config::HeaderRole;
use crate::model::FanState;
use crate::ui::PageCtx;
use crate::ui::actions::UiAction;
use crate::ui::assets::hero_map;
use crate::ui::fonts::{Ts, font, tab};
use crate::ui::icons::{self, Icon};
use crate::ui::theme::{Palette, R_LG, R_MD, pal};
use crate::ui::widgets::{self, thumb};

const UV: Rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
const STRIP_H: f32 = 44.0;

/// Hero fans fed by each ARGB header (default wiring: the hub chain on D_LED1 runs
/// front1 → front3 → rear, the pump head is on D_LED2).
const LED_MAP: &[(&str, &[&str])] = &[("dled1", &["front1", "front2", "front3", "rear"]), ("dled2", &["pump"])];

/// Mean colour of quarter `q` of `fan` from the zone previews (`None`: unlit).
pub fn segment_colour(
    previews: &std::collections::HashMap<String, Vec<[u8; 3]>>,
    lit: &dyn Fn(&str) -> bool,
    fan: &str,
    q: usize,
) -> Option<Color32> {
    let (zone, fans) = LED_MAP.iter().find(|(_, f)| f.contains(&fan))?;
    if !lit(zone) {
        return None;
    }
    let px = previews.get(*zone)?;
    let k = fans.iter().position(|f| *f == fan)?;
    let per_fan = px.len() / fans.len();
    if per_fan == 0 {
        return None;
    }
    let start = k * per_fan;
    let (a, b) = (start + q * per_fan / 4, start + (q + 1) * per_fan / 4);
    let run = &px[a.min(px.len())..b.max(a + 1).min(px.len())];
    let c = thumb::avg(run);
    (c != Color32::BLACK).then_some(c)
}

/// Where a header's callout points: a component when the user / wizard confirmed what is
/// connected, otherwise the header itself on the motherboard (no wiring is assumed).
pub fn anchor_for(f: &FanState) -> Option<&'static str> {
    match f.assigned {
        Some(HeaderRole::Pump) => return Some("pump"),
        Some(HeaderRole::RadiatorFans) | Some(HeaderRole::CpuFans) => return Some("radiator"),
        Some(HeaderRole::CaseFans) => return Some("front2"),
        Some(HeaderRole::Unused) => return None,
        _ => {}
    }
    match f.header.to_ascii_uppercase().as_str() {
        "CPU_FAN" => Some("cpu_fan_header"),
        "SYS_FAN1" => Some("sys_fan1_header"),
        "SYS_FAN2" => Some("sys_fan2_header"),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    TopLeft,
    TopRight,
    RightMid,
    LeftMid,
    RightLow,
    LeftLow,
}

fn preferred_slots(anchor: &str) -> [Slot; 6] {
    use Slot::*;
    match anchor {
        "pump" | "rear" => [TopLeft, LeftMid, LeftLow, TopRight, RightMid, RightLow],
        "radiator" | "cpu_fan_header" => [TopRight, TopLeft, RightMid, LeftMid, RightLow, LeftLow],
        "sys_fan2_header" => [RightLow, RightMid, LeftLow, LeftMid, TopRight, TopLeft],
        _ => [RightMid, RightLow, TopRight, LeftMid, LeftLow, TopLeft],
    }
}

struct Callout {
    anchor: &'static str,
    title: String,
    value: String,
    fan_id: String,
}

fn callouts(fans: &[FanState]) -> Vec<Callout> {
    fans.iter()
        .filter(|f| f.visible && !f.unconnected)
        .filter_map(|f| {
            let anchor = anchor_for(f)?;
            let title = if f.name.is_empty() || f.name == f.header {
                f.header.clone()
            } else {
                format!("{} · {}", f.name, f.header)
            };
            let value = if f.no_speed_signal() {
                match f.duty.or(f.duty_est) {
                    Some(d) => format!("{d:.0} % · no rpm"),
                    None => "no speed signal".into(),
                }
            } else {
                match (f.rpm, f.duty) {
                    (Some(r), Some(d)) if d < 99.5 => format!("{} rpm · {d:.0} %", widgets::thousands(r)),
                    (Some(r), _) => format!("{} rpm", widgets::thousands(r)),
                    (None, _) => "—".into(),
                }
            };
            Some(Callout { anchor, title, value, fan_id: f.id.clone() })
        })
        .collect()
}

/// Two-colour vertical gradient with rounded top and bottom bands.
fn stage_fill(painter: &egui::Painter, r: Rect, top: Color32, bottom: Color32) {
    let rad = R_LG as f32;
    painter.rect_filled(
        Rect::from_min_max(r.min, pos2(r.right(), r.top() + rad + 1.0)),
        CornerRadius { nw: R_LG, ne: R_LG, sw: 0, se: 0 },
        top,
    );
    painter.rect_filled(
        Rect::from_min_max(pos2(r.left(), r.bottom() - rad - 1.0), r.max),
        CornerRadius { nw: 0, ne: 0, sw: R_LG, se: R_LG },
        bottom,
    );
    let mid = Rect::from_min_max(pos2(r.left(), r.top() + rad), pos2(r.right(), r.bottom() - rad));
    let mut mesh = Mesh::default();
    for (pos, c) in
        [(mid.left_top(), top), (mid.right_top(), top), (mid.right_bottom(), bottom), (mid.left_bottom(), bottom)]
    {
        mesh.vertices.push(Vertex { pos, uv: WHITE_UV, color: c });
    }
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.add(Shape::mesh(mesh));
}

/// Draws the stage at the available width; returns its natural height (for `layout::row`).
pub fn hero_stage(ui: &mut Ui, pc: &mut PageCtx, min_h: f32) -> f32 {
    let p = pal(ui);
    let ctx = ui.ctx().clone();
    let map = hero_map();
    let w = ui.available_width();
    let narrow = w < 380.0;
    let img_w = (w - 24.0).clamp(160.0, crate::ui::theme::HERO_W);
    let img_h = img_w * map.size_1x[1] / map.size_1x[0];
    let natural = img_h + STRIP_H + 24.0;
    let fans: Vec<FanState> = pc.st.fans.clone();
    let cs = callouts(&fans);
    let legend_h = if narrow { cs.len() as f32 * 20.0 + 6.0 } else { 0.0 };
    let h = (natural + legend_h).max(min_h);
    let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return natural + legend_h;
    }
    let painter = ui.painter_at(rect);
    stage_fill(&painter, rect, p.stage_top, p.stage_bottom);
    painter.rect_stroke(rect, R_LG, Stroke::new(1.0, p.border_subtle), StrokeKind::Inside);

    // ---- render + live LEDs ----
    let area = Rect::from_min_max(rect.min + vec2(0.0, 8.0), pos2(rect.right(), rect.bottom() - STRIP_H - legend_h));
    // Top-aligned (the stage stretches to the status column; extra height goes below the PC).
    let top = area.top() + ((area.height() - img_h) / 2.0).clamp(0.0, 16.0);
    let img = Rect::from_min_size(pos2(area.center().x - img_w / 2.0, top), vec2(img_w, img_h));
    let scale = img_w / map.size_1x[0];
    let rgb_on = pc.cfg.rgb.enabled;
    let zones_on: Vec<(String, bool)> = ["dled1", "dled2"]
        .iter()
        .map(|z| (z.to_string(), rgb_on && pc.cfg.rgb.zones.get(*z).is_none_or(|c| c.enabled)))
        .collect();
    let lit = |z: &str| zones_on.iter().any(|(id, on)| id == z && *on);
    match pc.assets.hero(&ctx) {
        Some((base, led)) => {
            painter.image(base, img, UV, Color32::WHITE);
            for s in &map.segments {
                let Some(c) = segment_colour(&pc.rgb.preview, &lit, &s.fan, s.segment) else { continue };
                let dest = Rect::from_min_max(
                    img.min + vec2(s.rect[0] as f32, s.rect[1] as f32) * scale,
                    img.min + vec2(s.rect[2] as f32, s.rect[3] as f32) * scale,
                );
                let [ax, ay, aw, ah] = s.atlas.map(|v| v as f32);
                let uv = Rect::from_min_max(
                    pos2(ax / map.atlas_size[0], ay / map.atlas_size[1]),
                    pos2((ax + aw) / map.atlas_size[0], (ay + ah) / map.atlas_size[1]),
                );
                painter.image(led, dest, uv, thumb::gain(c, p.led_gain).additive());
            }
        }
        None => {
            icons::paint(&painter, Rect::from_center_size(img.center(), vec2(64.0, 64.0)), Icon::Board, p.text3);
        }
    }
    // Clicking the PC (not a callout) opens Lighting.
    let img_resp =
        ui.interact(img, Id::new("hero_img"), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);

    // ---- callouts ----
    let anchor_pos =
        |k: &str| map.anchors.get(k).map(|a| pos2(img.left() + a[0] * img.width(), img.top() + a[1] * img.height()));
    let mut used: Vec<Slot> = Vec::new();
    let mut clicked_fan: Option<String> = None;
    for (n, c) in cs.iter().enumerate() {
        let Some(ap) = anchor_pos(c.anchor) else { continue };
        if narrow {
            // Numbered dots + legend below.
            painter.circle_filled(ap, 8.0, p.surface1);
            painter.circle_stroke(ap, 8.0, Stroke::new(1.0, p.border_strong));
            painter.text(ap, Align2::CENTER_CENTER, (n + 1).to_string(), tab(Ts::Caption), p.text1);
            continue;
        }
        let g1 = painter.layout_no_wrap(c.title.clone(), font(Ts::Caption), p.text2);
        let g2 = painter.layout_no_wrap(c.value.clone(), tab(Ts::BodyStrong), p.text1);
        let size = vec2(g1.size().x.max(g2.size().x) + 20.0, 44.0);
        let slot = preferred_slots(c.anchor).into_iter().find(|s| !used.contains(s)).unwrap_or(Slot::LeftLow);
        used.push(slot);
        let (x_left, x_right) = (rect.left() + 14.0, rect.right() - 14.0 - size.x);
        let y = |f: f32| rect.top() + 14.0 + f * (img.bottom() - rect.top() - 70.0);
        let min = match slot {
            Slot::TopLeft => pos2(x_left, y(0.0)),
            Slot::TopRight => pos2(x_right, y(0.0)),
            Slot::RightMid => pos2(x_right, y(0.24)),
            Slot::LeftMid => pos2(x_left, y(0.42)),
            Slot::RightLow => pos2(x_right, y(0.5)),
            Slot::LeftLow => pos2(x_left, y(0.66)),
        };
        let pill = Rect::from_min_size(min, size);
        // Leader from the anchor to the pill's nearest vertical edge.
        let edge = if ap.x < pill.center().x {
            pos2(pill.left(), pill.center().y)
        } else {
            pos2(pill.right(), pill.center().y)
        };
        let end = pos2(edge.x, edge.y.clamp(pill.top() + 6.0, pill.bottom() - 6.0));
        painter.line_segment([ap, end], Stroke::new(1.0, p.text2));
        painter.circle_stroke(ap, 6.0, Stroke::new(1.0, p.text2));
        painter.circle_filled(ap, 3.0, p.text1);
        let resp = ui.interact(pill, Id::new(("hero_callout", &c.fan_id)), Sense::click());
        let fill = if resp.hovered() { p.surface2 } else { p.surface1 };
        painter.rect_filled(pill, R_MD, fill);
        painter.rect_stroke(
            pill,
            R_MD,
            Stroke::new(1.0, if resp.has_focus() { p.focus } else { p.border }),
            StrokeKind::Inside,
        );
        painter.galley(pos2(pill.left() + 10.0, pill.top() + 5.0), g1, p.text2);
        painter.galley(pos2(pill.left() + 10.0, pill.top() + 21.0), g2, p.text1);
        let label = format!("{}, {}, open in Cooling", c.title, c.value);
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            clicked_fan = Some(c.fan_id.clone());
        }
    }
    if narrow {
        for (n, c) in cs.iter().enumerate() {
            let y = area.bottom() + 4.0 + n as f32 * 20.0;
            painter.text(
                pos2(rect.left() + 14.0, y + 9.0),
                Align2::LEFT_CENTER,
                format!("{}  {} · {}", n + 1, c.title, c.value),
                font(Ts::Caption),
                p.text2,
            );
        }
    }

    // ---- lighting strip ----
    let strip = Rect::from_min_max(pos2(rect.left(), rect.bottom() - STRIP_H), rect.max);
    painter.line_segment([strip.left_top(), strip.right_top()], Stroke::new(1.0, p.border_subtle));
    let strip_resp = ui.interact(strip, Id::new("hero_strip"), Sense::click());
    if strip_resp.hovered() {
        painter.rect_filled(strip.shrink(1.0), CornerRadius { nw: 0, ne: 0, sw: R_LG, se: R_LG }, p.surface2);
    }
    paint_strip(&painter, p, pc, strip);
    let strip_resp = strip_resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    strip_resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Open Lighting"));
    if strip_resp.clicked() || img_resp.clicked() {
        pc.ui.pending.push(UiAction::Navigate(crate::ui::Page::Lighting));
    }
    if let Some(id) = clicked_fan {
        pc.ui.pending.push(UiAction::SelectFan(id));
    }
    natural + legend_h
}

fn paint_strip(painter: &egui::Painter, p: &Palette, pc: &PageCtx, strip: Rect) {
    let cy = strip.center().y;
    // Five dots sampled from the first lit zone's preview.
    let preview = ["dled1", "dled2", "ledc"]
        .iter()
        .filter(|z| pc.cfg.rgb.enabled && pc.cfg.rgb.zones.get(**z).is_none_or(|c| c.enabled))
        .find_map(|z| pc.rgb.preview.get(*z).filter(|v| !v.is_empty()));
    let mut x = strip.left() + 20.0;
    for k in 0..5 {
        let c = match preview {
            Some(v) => {
                let px = v[(k * v.len() / 5).min(v.len() - 1)];
                Color32::from_rgb(px[0], px[1], px[2])
            }
            None => p.surface3,
        };
        painter.circle_filled(pos2(x, cy), 4.0, c);
        x += 7.0;
    }
    x += 10.0;
    let zones = crate::ui::pages::led_nodes(pc.cfg, pc.rgb).iter().filter(|l| l.enabled).count();
    let (name, rest) = if !pc.cfg.rgb.enabled {
        ("Lighting off".to_string(), String::new())
    } else {
        let eff = if pc.cfg.rgb.sync {
            crate::ui::widgets::gallery::short_label(pc.cfg.rgb.global.kind).to_string()
        } else {
            "Per-zone effects".into()
        };
        let fps =
            if pc.rgb.device.is_some() { format!(" · {:.0} fps", pc.rgb.fps) } else { " · preview only".into() };
        (eff, format!(" · {zones} zone{}{fps}", if zones == 1 { "" } else { "s" }))
    };
    let g = painter.layout_no_wrap(name, font(Ts::BodyStrong), p.text1);
    let gw = g.size().x;
    painter.galley(pos2(x, cy - g.size().y / 2.0), g, p.text1);
    painter.text(pos2(x + gw + 2.0, cy), Align2::LEFT_CENTER, rest, font(Ts::Caption), p.text3);
    icons::paint(
        painter,
        Rect::from_center_size(pos2(strip.right() - 18.0, cy), vec2(14.0, 14.0)),
        Icon::ChevronRight,
        p.accent,
    );
    painter.text(pos2(strip.right() - 30.0, cy), Align2::RIGHT_CENTER, "Lighting", font(Ts::Label), p.accent);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FanRole;
    use std::collections::HashMap;

    #[test]
    fn quarters_follow_the_chain() {
        // 64 LEDs on D_LED1: 16 per fan, 4 per quarter. Quarter 1 of front2 = LEDs 20..24.
        let mut px = vec![[0u8, 0, 0]; 64];
        for p in &mut px[20..24] {
            *p = [255, 0, 0];
        }
        let mut m = HashMap::new();
        m.insert("dled1".to_string(), px);
        let on = |_: &str| true;
        assert_eq!(segment_colour(&m, &on, "front2", 1), Some(Color32::from_rgb(255, 0, 0)));
        assert_eq!(segment_colour(&m, &on, "front2", 0), None);
        assert_eq!(segment_colour(&m, &on, "pump", 0), None, "D_LED2 has no preview");
        let off = |_: &str| false;
        assert_eq!(segment_colour(&m, &off, "front2", 1), None, "zone off: LEDs unlit");
    }

    #[test]
    fn callouts_never_assume_wiring() {
        let base = crate::ui::widgets::safety::tests_support::fan("mb.pwm0", "CPU_FAN", None, FanRole::Unknown);
        assert_eq!(anchor_for(&base), Some("cpu_fan_header"));
        let mut pump = crate::ui::widgets::safety::tests_support::fan(
            "mb.pwm2",
            "SYS_FAN2",
            Some(HeaderRole::Pump),
            FanRole::Pump,
        );
        assert_eq!(anchor_for(&pump), Some("pump"));
        pump.assigned = Some(HeaderRole::Unused);
        assert_eq!(anchor_for(&pump), None);
        // Every anchor the code can name exists in hero.json.
        for k in ["pump", "radiator", "front2", "cpu_fan_header", "sys_fan1_header", "sys_fan2_header"] {
            assert!(hero_map().anchors.contains_key(k), "{k}");
        }
    }
}
