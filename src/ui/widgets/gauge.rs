//! Arc gauge (240° sweep, thermal gradient), fan ring and fan glyph.

use std::f32::consts::{PI, TAU};

use egui::epaint::PathStroke;
use egui::{Align2, Color32, Id, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, pos2, vec2};

use crate::ui::anim;
use crate::ui::fonts::{Ts, Weight, font, sized};
use crate::ui::icons::{self, Icon};
use crate::ui::theme::{Palette, pal, thermal_stops, thermal_with, with_alpha};

const START: f32 = PI * 5.0 / 6.0; // 150°
const SWEEP: f32 = PI * 4.0 / 3.0; // 240°

fn arc_points(c: Pos2, r: f32, a0: f32, a1: f32) -> Vec<Pos2> {
    let step = 2f32.to_radians();
    let n = (((a1 - a0).abs() / step).ceil() as usize).max(2);
    (0..=n)
        .map(|i| {
            let a = a0 + (a1 - a0) * i as f32 / n as f32;
            c + vec2(a.cos(), a.sin()) * r
        })
        .collect()
}

pub struct Gauge<'a> {
    pub id: Id,
    pub size: f32,
    pub value: Option<f32>,
    pub lo: f32,
    pub hi: f32,
    /// Thermal ramp reference (°C) and critical tick.
    pub critical: f32,
    pub target: Option<f32>,
    pub label: &'a str,
    pub sub: &'a str,
    pub fahrenheit: bool,
    /// Locked variant (no access): caption under the lock icon.
    pub locked: Option<&'a str>,
}

impl Gauge<'_> {
    /// Returns (gauge response, locked-caption response if any).
    pub fn show(self, ui: &mut Ui) -> (Response, Option<Response>) {
        let p = pal(ui);
        let size = self.size;
        let h = size * 0.86;
        let (rect, _) = ui.allocate_exact_size(vec2(size, h), Sense::hover());
        let resp = ui.interact(rect, self.id, Sense::click());
        let c = pos2(rect.center().x, rect.top() + size * 0.5);
        let r = size * 0.42;
        let w = (size * 0.05).max(3.0);
        let painter = ui.painter();
        let mut locked_resp = None;

        if let Some(msg) = self.locked {
            painter
                .add(Shape::line(arc_points(c, r, START, START + SWEEP), Stroke::new(w, with_alpha(p.surface3, 0.5))));
            icons::paint(
                painter,
                Rect::from_center_size(c - vec2(0.0, size * 0.06), vec2(22.0, 22.0)),
                Icon::Lock,
                p.text3,
            );
            let g = painter.layout(msg.to_string(), font(Ts::Caption), p.accent, size * 0.8);
            let lr = Rect::from_center_size(c + vec2(0.0, size * 0.12), g.size());
            let lresp = ui.interact(lr.expand(4.0), self.id.with("locked"), Sense::click());
            let color = if lresp.hovered() { p.accent_hover } else { p.accent };
            ui.painter().galley(lr.min, g, color);
            locked_resp = Some(lresp);
            let painter = ui.painter();
            painter.text(pos2(c.x, rect.bottom() - 4.0), Align2::CENTER_BOTTOM, self.label, font(Ts::Label), p.text2);
            return (resp, locked_resp);
        }

        // Track.
        painter.add(Shape::line(arc_points(c, r, START, START + SWEEP), Stroke::new(w, p.surface3)));
        for a in [START, START + SWEEP] {
            painter.circle_filled(c + vec2(a.cos(), a.sin()) * r, w / 2.0, p.surface3);
        }

        let span = (self.hi - self.lo).max(1e-3);
        let shown = self.value.map(|v| anim::tween_data(ui.ctx(), self.id.with("v"), v, anim::VALUE, span * 0.04));
        let colors = p.thermal;
        let stops = thermal_stops(self.critical);
        let (lo, hi) = (self.lo, self.hi);
        let color_at = move |a: f32| -> Color32 {
            let f = ((a - START).rem_euclid(TAU) / SWEEP).clamp(0.0, 1.0);
            thermal_with(&colors, &stops, lo + f * (hi - lo))
        };

        if let Some(v) = shown {
            let f = ((v - self.lo) / span).clamp(0.0, 1.0);
            if f > 0.004 {
                let a1 = START + SWEEP * f;
                let center = c;
                let stroke = PathStroke::new_uv(w, move |_r, pos| {
                    let d = pos - center;
                    color_at(d.y.atan2(d.x))
                });
                painter.add(Shape::line(arc_points(c, r, START, a1), stroke));
                painter.circle_filled(c + vec2(START.cos(), START.sin()) * r, w / 2.0, color_at(START));
                painter.circle_filled(c + vec2(a1.cos(), a1.sin()) * r, w / 2.0, color_at(a1 - 0.001));
            }
        }

        // Ticks: target (accent), critical (danger, pulsing when close).
        let tick = |val: f32, color: Color32, len: f32| {
            let f = ((val - self.lo) / span).clamp(0.0, 1.0);
            let a = START + SWEEP * f;
            let dir = vec2(a.cos(), a.sin());
            ui.painter().line_segment([c + dir * (r - len / 2.0), c + dir * (r + len / 2.0)], Stroke::new(2.0, color));
        };
        if let Some(t) = self.target {
            tick(t, p.accent, w + 6.0);
        }
        let near = shown.is_some_and(|v| v >= self.critical - 3.0);
        let mut crit_alpha = 1.0;
        if near && !anim::reduce_motion(ui.ctx()) {
            let t = ui.input(|i| i.time) as f32;
            crit_alpha = 0.6 + 0.4 * (0.5 + 0.5 * (t * TAU / 1.2).cos());
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
        }
        tick(self.critical, with_alpha(p.danger, crit_alpha), w + 6.0);

        // Value.
        let painter = ui.painter();
        let big = sized((size * 0.24).round(), Weight::SemiBold, true);
        match shown {
            Some(v) => {
                let disp = if self.fahrenheit { crate::util::c_to_f(v) } else { v };
                let txt = format!("{disp:.0}");
                let g = painter.layout_no_wrap(txt, big, p.text1);
                let unit = painter.layout_no_wrap(
                    if self.fahrenheit { "°F".into() } else { "°C".into() },
                    font(Ts::Caption),
                    p.text2,
                );
                let total = g.size().x + 2.0 + unit.size().x;
                let top = c.y - g.size().y * 0.62;
                let x0 = c.x - total / 2.0;
                let gh = g.size().y;
                let gw = g.size().x;
                painter.galley(pos2(x0, top), g, p.text1);
                painter.galley(pos2(x0 + gw + 2.0, top + gh * 0.16), unit, p.text2);
            }
            None => {
                painter.text(c - vec2(0.0, size * 0.05), Align2::CENTER_CENTER, "—", big, p.text3);
            }
        }
        painter.text(pos2(c.x, c.y + size * 0.17), Align2::CENTER_CENTER, self.label, font(Ts::Label), p.text2);
        if !self.sub.is_empty() {
            painter.text(pos2(c.x, rect.bottom() - 2.0), Align2::CENTER_BOTTOM, self.sub, font(Ts::Caption), p.text3);
        }
        (resp, locked_resp)
    }
}

/// Fan ring: donut whose fill fraction is the duty, with a spinning glyph in the middle.
pub fn fan_ring(
    painter: &Painter,
    p: &Palette,
    center: Pos2,
    radius: f32,
    duty: Option<f32>,
    color: Color32,
    angle: f32,
) {
    let w = (radius * 0.19).max(2.0);
    painter.circle_stroke(center, radius, Stroke::new(w, p.surface3));
    if let Some(d) = duty {
        let f = (d / 100.0).clamp(0.0, 1.0);
        if f > 0.005 {
            let a0 = -PI / 2.0;
            let pts = arc_points(center, radius, a0, a0 + TAU * f);
            painter.add(Shape::line(pts, Stroke::new(w, color)));
        }
    }
    let g = radius * 1.1;
    icons::paint_fan(painter, Rect::from_center_size(center, vec2(g, g)), angle, color);
}

/// Visual rotation speed for a fan glyph (rad/s), capped at 4 rev/s.
pub fn spin_rate(rpm: Option<f32>) -> f32 {
    let rps = rpm.unwrap_or(0.0) / 60.0;
    (rps * 0.25).min(4.0) * TAU
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spin_rate_capped() {
        assert_eq!(spin_rate(None), 0.0);
        assert!(spin_rate(Some(600.0)) > 0.0);
        assert!((spin_rate(Some(100_000.0)) - 4.0 * TAU).abs() < 1e-3);
    }

    #[test]
    fn arc_has_points_every_two_degrees() {
        let pts = arc_points(Pos2::ZERO, 10.0, START, START + SWEEP);
        assert!(pts.len() >= 120);
        assert!((pts[0].distance(Pos2::ZERO) - 10.0).abs() < 1e-3);
    }
}
