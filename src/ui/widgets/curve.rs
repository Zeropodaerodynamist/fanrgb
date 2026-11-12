//! Fan-curve editor v2.
//!
//! Fixes the old drag-pick bug: the grabbed point is chosen from the *press position* on the frame
//! the button goes down (not after egui's drag threshold), and it keeps its offset to the pointer.
//! Adds selection, keyboard editing, snapping + alignment guides, undo/redo, numeric point strip,
//! hysteresis band, clamp bands, critical zone, 60 s operating trail and requested-vs-actual.

use egui::{
    Align2, CursorIcon, EventFilter, Id, Key, LayerId, Order, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, pos2,
    vec2,
};

use crate::ui::fonts::{Ts, font, tab};
use crate::ui::theme::{R_PILL, R_SM, pal, shadow_e1, thermal, with_alpha};
use crate::util::c_to_f;

pub const T0: f32 = 20.0;
pub const T1: f32 = 100.0;
const HIT: f32 = 12.0;
const UNDO_CAP: usize = 50;

// ---------------------------------------------------------------------------------------
// Pure logic (unit tested)

pub mod logic {
    use super::{T0, T1, UNDO_CAP};
    use egui::Pos2;

    /// Index of the point closest to `at` within `radius` (screen space).
    pub fn pick(screen: &[Pos2], at: Pos2, radius: f32) -> Option<usize> {
        screen
            .iter()
            .enumerate()
            .map(|(i, p)| (i, p.distance(at)))
            .filter(|(_, d)| *d <= radius)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Snap {
        Fine,
        Normal,
        Coarse,
    }

    pub fn snap(v: f32, mode: Snap) -> f32 {
        match mode {
            Snap::Fine => (v * 10.0).round() / 10.0,
            Snap::Normal => v.round(),
            Snap::Coarse => (v / 5.0).round() * 5.0,
        }
    }

    /// Clamps a candidate value for point `i`: x stays ≥1° away from its neighbors, y in 0..100.
    pub fn constrain(points: &[[f32; 2]], i: usize, v: [f32; 2]) -> [f32; 2] {
        let lo = if i > 0 { points[i - 1][0] + 1.0 } else { T0 };
        let hi = if i + 1 < points.len() { points[i + 1][0] - 1.0 } else { T1 };
        [v[0].clamp(lo, hi.max(lo)), v[1].clamp(0.0, 100.0)]
    }

    /// Inserts a point keeping x order; refuses points within 1° of an existing one.
    pub fn insert(points: &mut Vec<[f32; 2]>, v: [f32; 2]) -> Option<usize> {
        let v = [v[0].clamp(T0, T1), v[1].clamp(0.0, 100.0)];
        if points.iter().any(|p| (p[0] - v[0]).abs() < 1.0) {
            return None;
        }
        let i = points.partition_point(|p| p[0] < v[0]);
        points.insert(i, v);
        Some(i)
    }

    /// Removes point `i` unless only two remain.
    pub fn remove(points: &mut Vec<[f32; 2]>, i: usize) -> bool {
        if points.len() <= 2 || i >= points.len() {
            return false;
        }
        points.remove(i);
        true
    }

    /// Midpoint between `i` and its successor (or a step to the right of the last point).
    pub fn midpoint_after(points: &[[f32; 2]], i: usize) -> [f32; 2] {
        match (points.get(i), points.get(i + 1)) {
            (Some(a), Some(b)) => [((a[0] + b[0]) / 2.0).round(), ((a[1] + b[1]) / 2.0).round()],
            (Some(a), None) => [(a[0] + 5.0).min(T1), a[1]],
            _ => [60.0, 50.0],
        }
    }

    /// Moves point `i` by (dx, dy), constrained.
    pub fn nudge(points: &mut [[f32; 2]], i: usize, dx: f32, dy: f32) -> bool {
        let Some(p) = points.get(i).copied() else { return false };
        let v = constrain(points, i, [p[0] + dx, p[1] + dy]);
        if v != p {
            points[i] = v;
            true
        } else {
            false
        }
    }

    /// Curve value with flat extension (same as the controller's interpolate).
    pub fn eval(points: &[[f32; 2]], t: f32) -> f32 {
        crate::control::curve::interpolate(points, t)
    }

    /// Duty another point sits at if `duty` is within 1 % of it (alignment guide target).
    pub fn align_target(points: &[[f32; 2]], skip: usize, duty: f32) -> Option<f32> {
        points.iter().enumerate().filter(|(i, _)| *i != skip).map(|(_, p)| p[1]).find(|d| (d - duty).abs() <= 1.0)
    }

    #[derive(Clone, Debug, Default, PartialEq)]
    pub struct Undo {
        pub past: Vec<Vec<[f32; 2]>>,
        pub future: Vec<Vec<[f32; 2]>>,
    }

    impl Undo {
        /// Records `before` as an undo step (no-op if unchanged from the last step).
        pub fn push(&mut self, before: Vec<[f32; 2]>) {
            if self.past.last() == Some(&before) {
                return;
            }
            self.past.push(before);
            if self.past.len() > UNDO_CAP {
                self.past.remove(0);
            }
            self.future.clear();
        }
        pub fn undo(&mut self, current: &mut Vec<[f32; 2]>) -> bool {
            match self.past.pop() {
                Some(prev) => {
                    self.future.push(std::mem::replace(current, prev));
                    true
                }
                None => false,
            }
        }
        pub fn redo(&mut self, current: &mut Vec<[f32; 2]>) -> bool {
            match self.future.pop() {
                Some(next) => {
                    self.past.push(std::mem::replace(current, next));
                    true
                }
                None => false,
            }
        }
    }
}

use logic::{Snap, Undo};

#[derive(Clone, Debug, Default)]
struct EditorState {
    selected: Option<usize>,
    dragging: Option<usize>,
    grab_offset: egui::Vec2,
    drag_before: Option<Vec<[f32; 2]>>,
    undo: Undo,
    ctx_target: Option<usize>,
    ctx_value: Option<[f32; 2]>,
    focus_strip: bool,
    strip_before: Option<Vec<[f32; 2]>>,
}

fn state(ui: &Ui, id: Id) -> EditorState {
    ui.data(|d| d.get_temp::<EditorState>(id.with("state"))).unwrap_or_default()
}

fn store(ui: &Ui, id: Id, st: EditorState) {
    ui.data_mut(|d| d.insert_temp(id.with("state"), st));
}

pub fn can_undo(ui: &Ui, id: Id) -> bool {
    !state(ui, id).undo.past.is_empty()
}

pub fn can_redo(ui: &Ui, id: Id) -> bool {
    !state(ui, id).undo.future.is_empty()
}

pub fn undo(ui: &Ui, id: Id, points: &mut Vec<[f32; 2]>) -> bool {
    let mut st = state(ui, id);
    let ok = st.undo.undo(points);
    st.selected = st.selected.filter(|i| *i < points.len());
    store(ui, id, st);
    ok
}

pub fn redo(ui: &Ui, id: Id, points: &mut Vec<[f32; 2]>) -> bool {
    let mut st = state(ui, id);
    let ok = st.undo.redo(points);
    st.selected = st.selected.filter(|i| *i < points.len());
    store(ui, id, st);
    ok
}

/// Records an external edit (preset applied etc.) as an undo step.
pub fn record(ui: &Ui, id: Id, before: Vec<[f32; 2]>) {
    let mut st = state(ui, id);
    st.undo.push(before);
    store(ui, id, st);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CurveEdit {
    pub changed: bool,
    /// The edit finished (drag released, key edit, field committed): write config / sort.
    pub committed: bool,
    pub reset_requested: bool,
}

pub struct CurveEditor<'a> {
    pub id: Id,
    pub height: f32,
    pub fahrenheit: bool,
    pub critical: f32,
    pub min_duty: f32,
    pub max_duty: f32,
    pub hysteresis: f32,
    pub live_temp: Option<f32>,
    pub actual_duty: Option<f32>,
    /// Last 60 s of (source temperature, actual duty), oldest first.
    pub trail: &'a [(f32, f32)],
    /// Calibration: duty → rpm, for the optional right axis and value tags.
    pub rpm_at: Option<&'a dyn Fn(f32) -> Option<f32>>,
    pub rpm_axis: bool,
    pub source_label: &'a str,
    /// Why actual differs from requested ("ramping: +10 %/s limit").
    pub explain: Option<String>,
}

impl CurveEditor<'_> {
    pub fn show(self, ui: &mut Ui, points: &mut Vec<[f32; 2]>) -> CurveEdit {
        let p = pal(ui);
        let id = self.id;
        let mut out = CurveEdit::default();
        let width = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(vec2(width, self.height), Sense::hover());
        let resp: Response = ui.interact(rect, id, Sense::click_and_drag());
        let right_pad = if self.rpm_axis && self.rpm_at.is_some() { 48.0 } else { 20.0 };
        let plot = Rect::from_min_max(rect.min + vec2(40.0, 10.0), rect.max - vec2(right_pad, 24.0));
        let to_screen = |v: [f32; 2]| {
            pos2(plot.left() + (v[0] - T0) / (T1 - T0) * plot.width(), plot.bottom() - v[1] / 100.0 * plot.height())
        };
        let to_value = |s: Pos2| {
            [T0 + (s.x - plot.left()) / plot.width() * (T1 - T0), (plot.bottom() - s.y) / plot.height() * 100.0]
        };
        let mut st = state(ui, id);
        st.selected = st.selected.filter(|i| *i < points.len());

        let (mods, pressed, released, press_origin, ptr) = ui.input(|i| {
            (
                i.modifiers,
                i.pointer.primary_pressed(),
                i.pointer.primary_released(),
                i.pointer.press_origin(),
                i.pointer.interact_pos(),
            )
        });
        let snap_mode = if mods.alt {
            Snap::Fine
        } else if mods.shift {
            Snap::Coarse
        } else {
            Snap::Normal
        };
        let screen: Vec<Pos2> = points.iter().map(|v| to_screen(*v)).collect();

        // ---- press: pick from the press position (not after the drag threshold) ----
        if pressed && resp.is_pointer_button_down_on() {
            resp.request_focus();
            if let Some(origin) = press_origin {
                match logic::pick(&screen, origin, HIT) {
                    Some(i) => {
                        st.dragging = Some(i);
                        st.selected = Some(i);
                        st.grab_offset = screen[i] - origin;
                        st.drag_before = Some(points.clone());
                    }
                    None => {
                        st.dragging = None;
                    }
                }
            }
        }
        // ---- drag ----
        let mut guide: Option<f32> = None;
        if let (Some(i), Some(pos), true) = (st.dragging, ptr, resp.is_pointer_button_down_on() || resp.dragged()) {
            let moved = press_origin.is_none_or(|o| o.distance(pos) > 1.0);
            if moved && i < points.len() {
                let raw = to_value(pos + st.grab_offset);
                let mut v = [logic::snap(raw[0], snap_mode), logic::snap(raw[1], snap_mode)];
                if !mods.alt
                    && let Some(d) = logic::align_target(points, i, v[1])
                {
                    v[1] = d;
                    guide = Some(d);
                }
                let v = logic::constrain(points, i, v);
                if points[i] != v {
                    points[i] = v;
                    out.changed = true;
                }
                ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
            }
        }
        if released && st.dragging.is_some() {
            if let Some(before) = st.drag_before.take()
                && &before != points
            {
                st.undo.push(before);
                out.committed = true;
            }
            st.dragging = None;
        }
        // ---- click on empty space deselects; double-click adds ----
        if resp.clicked()
            && st.dragging.is_none()
            && let Some(pos) = resp.interact_pointer_pos()
            && logic::pick(&screen, pos, HIT).is_none()
        {
            st.selected = None;
        }
        if resp.double_clicked()
            && let Some(pos) = resp.interact_pointer_pos()
            && logic::pick(&screen, pos, HIT).is_none()
            && plot.contains(pos)
        {
            let raw = to_value(pos);
            let before = points.clone();
            if let Some(i) = logic::insert(points, [logic::snap(raw[0], snap_mode), logic::snap(raw[1], snap_mode)]) {
                st.undo.push(before);
                st.selected = Some(i);
                out.changed = true;
                out.committed = true;
            }
        }
        // ---- right click: context menu ----
        if resp.secondary_clicked()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            st.ctx_target = logic::pick(&screen, pos, HIT);
            st.ctx_value = Some(to_value(pos));
            if st.ctx_target.is_some() {
                st.selected = st.ctx_target;
            }
        }
        // ---- wheel over the selected point ----
        if let (Some(sel), Some(h)) = (st.selected, resp.hover_pos())
            && sel < points.len()
            && screen[sel].distance(h) <= HIT * 1.5
        {
            let dy = ui.input(|i| i.raw_scroll_delta.y);
            if dy != 0.0 {
                let before = points.clone();
                if logic::nudge(points, sel, 0.0, dy.signum()) {
                    st.undo.push(before);
                    out.changed = true;
                    out.committed = true;
                }
                ui.input_mut(|i| {
                    i.raw_scroll_delta = egui::Vec2::ZERO;
                    i.smooth_scroll_delta = egui::Vec2::ZERO;
                });
            }
        }
        // ---- keyboard ----
        if resp.has_focus() {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    id,
                    EventFilter {
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: st.selected.is_some(),
                        tab: false,
                    },
                )
            });
            let keys = ui.input(|i| {
                [
                    Key::ArrowLeft,
                    Key::ArrowRight,
                    Key::ArrowUp,
                    Key::ArrowDown,
                    Key::Delete,
                    Key::Insert,
                    Key::N,
                    Key::Escape,
                    Key::Z,
                    Key::Y,
                    Key::Enter,
                    Key::Backspace,
                ]
                .map(|k| i.key_pressed(k))
            });
            let ctrl = mods.command;
            let step = if mods.shift { 5.0 } else { 1.0 };
            let before = points.clone();
            let mut edited = false;
            if ctrl && keys[8] {
                if mods.shift {
                    edited |= st.undo.redo(points)
                } else {
                    edited |= st.undo.undo(points)
                }
                st.selected = st.selected.filter(|i| *i < points.len());
                if edited {
                    out.changed = true;
                    out.committed = true;
                }
                edited = false;
            } else if ctrl && keys[9] {
                if st.undo.redo(points) {
                    out.changed = true;
                    out.committed = true;
                }
                st.selected = st.selected.filter(|i| *i < points.len());
            } else if let Some(sel) = st.selected.filter(|i| *i < points.len()) {
                if ctrl {
                    let (dx, dy) = (
                        if keys[0] {
                            -step
                        } else if keys[1] {
                            step
                        } else {
                            0.0
                        },
                        if keys[2] {
                            step
                        } else if keys[3] {
                            -step
                        } else {
                            0.0
                        },
                    );
                    if dx != 0.0 || dy != 0.0 {
                        edited |= logic::nudge(points, sel, dx, dy);
                    }
                } else if keys[0] {
                    st.selected = Some(sel.saturating_sub(1));
                } else if keys[1] {
                    st.selected = Some((sel + 1).min(points.len() - 1));
                } else if keys[2] || keys[3] {
                    edited |= logic::nudge(points, sel, 0.0, if keys[2] { step } else { -step });
                }
                if (keys[4] || keys[11]) && logic::remove(points, sel) {
                    edited = true;
                    st.selected = Some(sel.min(points.len() - 1));
                }
                if keys[5] || (keys[6] && !ctrl) {
                    let v = logic::midpoint_after(points, sel);
                    if let Some(i) = logic::insert(points, v) {
                        edited = true;
                        st.selected = Some(i);
                    }
                }
                if keys[7] {
                    st.selected = None;
                }
                if keys[10] {
                    st.focus_strip = true;
                }
            } else if (keys[0] || keys[1]) && !points.is_empty() {
                // Nothing selected: start at the point nearest to the live temperature.
                let t = self.live_temp.unwrap_or(50.0);
                let i = points
                    .iter()
                    .enumerate()
                    .min_by(|a, b| (a.1[0] - t).abs().total_cmp(&(b.1[0] - t).abs()))
                    .map(|(i, _)| i);
                st.selected = i;
            }
            if edited {
                st.undo.push(before);
                out.changed = true;
                out.committed = true;
            }
        }

        // ================= paint =================
        let painter = ui.painter_at(rect);
        painter.rect_filled(plot.expand(2.0), R_SM, p.inset);
        // grid
        for k in 0..=8 {
            let t = T0 + k as f32 * 10.0;
            let x = painter.round_to_pixel_center(to_screen([t, 0.0]).x);
            let major = (t as i32) % 20 == 0;
            painter.line_segment(
                [pos2(x, plot.top()), pos2(x, plot.bottom())],
                Stroke::new(1.0, if major { p.border } else { p.border_subtle }),
            );
            let label = if self.fahrenheit { format!("{:.0}°", c_to_f(t)) } else { format!("{t:.0}°") };
            painter.text(pos2(x, plot.bottom() + 6.0), Align2::CENTER_TOP, label, tab(Ts::Caption), p.text3);
        }
        for k in 0..=10 {
            let d = k as f32 * 10.0;
            let y = painter.round_to_pixel_center(to_screen([T0, d]).y);
            let major = k % 2 == 0;
            painter.line_segment(
                [pos2(plot.left(), y), pos2(plot.right(), y)],
                Stroke::new(1.0, if major { p.border } else { p.border_subtle }),
            );
            if major {
                painter.text(
                    pos2(plot.left() - 8.0, y),
                    Align2::RIGHT_CENTER,
                    format!("{d:.0}%"),
                    tab(Ts::Caption),
                    p.text3,
                );
                if let (true, Some(f)) = (self.rpm_axis, self.rpm_at)
                    && let Some(rpm) = f(d)
                {
                    painter.text(
                        pos2(plot.right() + 6.0, y),
                        Align2::LEFT_CENTER,
                        super::thousands(rpm),
                        tab(Ts::Caption),
                        p.text3,
                    );
                }
            }
        }
        // critical zone
        if self.critical < T1 {
            let x = to_screen([self.critical.max(T0), 0.0]).x;
            let zone = Rect::from_min_max(pos2(x, plot.top()), plot.max);
            painter.rect_filled(zone, 0.0, with_alpha(p.danger_subtle, 0.9));
            painter.line_segment(
                [pos2(x, plot.top()), pos2(x, plot.bottom())],
                Stroke::new(1.0, with_alpha(p.danger, 0.6)),
            );
            if zone.width() > 60.0 {
                painter.text(
                    pos2(zone.center().x, plot.top() + 6.0),
                    Align2::CENTER_TOP,
                    "100 % above",
                    font(Ts::Caption),
                    p.danger,
                );
            }
        }
        // clamp bands (hatched)
        let hatch = |band: Rect, label: &str| {
            if band.height() < 1.0 {
                return;
            }
            let hp = ui.painter_at(band);
            let c = with_alpha(p.text3, 0.18);
            let mut x = band.left() - band.height();
            while x < band.right() {
                hp.line_segment([pos2(x, band.bottom()), pos2(x + band.height(), band.top())], Stroke::new(1.0, c));
                x += 6.0;
            }
            if band.height() > 14.0 {
                let g = painter.layout_no_wrap(label.to_string(), font(Ts::Caption), p.text2);
                let r = Rect::from_min_size(
                    pos2(band.left() + 6.0, band.center().y - g.size().y / 2.0 - 1.0),
                    g.size() + vec2(8.0, 2.0),
                );
                painter.rect_filled(r, 4, p.inset);
                painter.galley(r.min + vec2(4.0, 1.0), g, p.text2);
            }
        };
        if self.min_duty > 0.5 {
            let y = to_screen([T0, self.min_duty]).y;
            hatch(Rect::from_min_max(pos2(plot.left(), y), plot.max), &format!("min {:.0} %", self.min_duty));
        }
        if self.max_duty < 99.5 {
            let y = to_screen([T0, self.max_duty]).y;
            hatch(Rect::from_min_max(plot.min, pos2(plot.right(), y)), &format!("max {:.0} %", self.max_duty));
        }
        let clamp = |d: f32| d.clamp(self.min_duty, self.max_duty.max(self.min_duty));

        // sample the effective curve
        let samples: Vec<(f32, f32)> =
            (0..=160).map(|k| T0 + (T1 - T0) * k as f32 / 160.0).map(|t| (t, clamp(logic::eval(points, t)))).collect();
        let line: Vec<Pos2> = samples.iter().map(|(t, d)| to_screen([*t, *d])).collect();
        // area under the curve
        {
            let mut mesh = egui::epaint::Mesh::default();
            let top_c = with_alpha(p.accent, 0.18);
            let bot_c = with_alpha(p.accent, 0.0);
            for w in line.windows(2) {
                let i = mesh.vertices.len() as u32;
                mesh.colored_vertex(w[0], top_c);
                mesh.colored_vertex(w[1], top_c);
                mesh.colored_vertex(pos2(w[1].x, plot.bottom()), bot_c);
                mesh.colored_vertex(pos2(w[0].x, plot.bottom()), bot_c);
                mesh.add_triangle(i, i + 1, i + 2);
                mesh.add_triangle(i, i + 2, i + 3);
            }
            painter.add(Shape::mesh(mesh));
        }
        // hysteresis band: falling temperatures keep the duty of up to `hysteresis` degrees higher.
        let mut band_hovered = false;
        if self.hysteresis > 0.05 {
            let mut mesh = egui::epaint::Mesh::default();
            let c = with_alpha(p.accent, 0.10);
            let mut upper = Vec::with_capacity(samples.len());
            for w in samples.windows(2) {
                let (t0, d0) = w[0];
                let (t1, d1) = w[1];
                let u0 = clamp(logic::eval(points, t0 + self.hysteresis));
                let u1 = clamp(logic::eval(points, t1 + self.hysteresis));
                let i = mesh.vertices.len() as u32;
                mesh.colored_vertex(to_screen([t0, d0]), c);
                mesh.colored_vertex(to_screen([t1, d1]), c);
                mesh.colored_vertex(to_screen([t1, u1]), c);
                mesh.colored_vertex(to_screen([t0, u0]), c);
                mesh.add_triangle(i, i + 1, i + 2);
                mesh.add_triangle(i, i + 2, i + 3);
                upper.push(to_screen([t0, u0]));
            }
            painter.add(Shape::mesh(mesh));
            painter.extend(Shape::dashed_line(&upper, Stroke::new(1.0, with_alpha(p.accent, 0.55)), 3.0, 3.0));
            if let Some(h) = resp.hover_pos() {
                let v = to_value(h);
                let (lo_d, hi_d) =
                    (clamp(logic::eval(points, v[0])), clamp(logic::eval(points, v[0] + self.hysteresis)));
                band_hovered = hi_d - lo_d > 0.5
                    && v[1] > lo_d + 0.3
                    && v[1] < hi_d - 0.3
                    && logic::pick(&screen, h, HIT).is_none();
            }
        }
        // operating trail
        let n = self.trail.len().max(1) as f32;
        for (k, (t, d)) in self.trail.iter().enumerate() {
            if t.is_finite() && d.is_finite() {
                let a = 0.5 * (k as f32 + 1.0) / n;
                painter.circle_filled(to_screen([t.clamp(T0, T1), d.clamp(0.0, 100.0)]), 2.0, with_alpha(p.text2, a));
            }
        }
        // curve
        painter.add(Shape::line(line, Stroke::new(2.0, p.accent)));
        // alignment guide
        if let Some(d) = guide {
            let y = to_screen([T0, d]).y;
            painter.extend(Shape::dashed_line(
                &[pos2(plot.left(), y), pos2(plot.right(), y)],
                Stroke::new(1.0, p.accent_hover),
                4.0,
                4.0,
            ));
        }
        // live operating point
        let tip_layer = ui.ctx().layer_painter(LayerId::new(Order::Tooltip, id.with("tags")));
        if let Some(t) = self.live_temp.filter(|t| t.is_finite()) {
            let tc = t.clamp(T0, T1);
            let x = to_screen([tc, 0.0]).x;
            painter.extend(Shape::dashed_line(
                &[pos2(x, plot.top()), pos2(x, plot.bottom())],
                Stroke::new(1.0, p.text3),
                3.0,
                3.0,
            ));
            let tl = if self.fahrenheit {
                format!("{} {:.1}°", self.source_label, c_to_f(t))
            } else {
                format!("{} {t:.1}°", self.source_label)
            };
            let g = painter.layout_no_wrap(tl, tab(Ts::Caption), p.text1);
            let mut r =
                Rect::from_min_size(pos2(x - g.size().x / 2.0 - 6.0, plot.top() + 2.0), g.size() + vec2(12.0, 4.0));
            r = r.translate(vec2((plot.left() - r.left()).max(0.0) + (plot.right() - r.right()).min(0.0), 0.0));
            if self.critical < T1 && x > to_screen([self.critical, 0.0]).x - 40.0 {
                r = r.translate(vec2(0.0, 18.0));
            }
            painter.rect_filled(r, R_PILL, p.surface3);
            painter.galley(r.min + vec2(6.0, 2.0), g, p.text1);
            let requested = clamp(logic::eval(points, t));
            let rq = to_screen([tc, requested]);
            painter.circle_stroke(rq, 5.0, Stroke::new(2.0, p.accent));
            if let Some(a) = self.actual_duty.filter(|d| d.is_finite()) {
                let ac = to_screen([tc, a.clamp(0.0, 100.0)]);
                if (a - requested).abs() > 2.0 {
                    painter.line_segment([rq, ac], Stroke::new(1.0, p.text3));
                }
                painter.circle_filled(ac, 5.0, p.text1);
                painter.circle_stroke(ac, 5.5, Stroke::new(2.0, thermal(p, t, self.critical)));
                if (a - requested).abs() > 2.0
                    && resp.hover_pos().is_some_and(|h| h.distance(ac) < 10.0 || h.distance(rq) < 10.0)
                {
                    let why = self.explain.clone().unwrap_or_else(|| "Smoothing and ramp limits".into());
                    tag(
                        &tip_layer,
                        p,
                        ac + vec2(12.0, -12.0),
                        &format!("Requested {requested:.0} % · actual {a:.0} %\n{why}"),
                        plot,
                    );
                }
            }
        }
        // points
        let hovered = resp.hover_pos().and_then(|h| logic::pick(&screen, h, HIT));
        for (i, s) in screen.iter().enumerate() {
            let sel = st.selected == Some(i);
            let hov = hovered == Some(i) || st.dragging == Some(i);
            if sel {
                painter.circle_filled(*s, 9.0, with_alpha(p.accent, 0.25));
                painter.circle_filled(*s, 5.5, p.accent);
                painter.circle_stroke(*s, 5.5, Stroke::new(2.0, p.surface1));
            } else {
                let r = if hov { 6.0 } else { 5.0 };
                painter.circle_filled(*s, r, p.surface1);
                painter.circle_stroke(*s, r, Stroke::new(2.0, p.accent));
            }
        }
        if hovered.is_some() && st.dragging.is_none() {
            ui.ctx().set_cursor_icon(CursorIcon::Grab);
        } else if st.dragging.is_none() && resp.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
        }
        // value tag of the selected/dragged point
        if let Some(i) = st.dragging.or(st.selected).or(hovered).filter(|i| *i < points.len()) {
            let v = points[i];
            let t = if self.fahrenheit { format!("{:.0} °F", c_to_f(v[0])) } else { format!("{:.0} °C", v[0]) };
            let rpm = self
                .rpm_at
                .and_then(|f| f(v[1]))
                .map(|r| format!("  ≈ {} rpm", super::thousands(r)))
                .unwrap_or_default();
            tag(&tip_layer, p, screen[i] + vec2(0.0, -16.0), &format!("{t} → {:.0} %{rpm}", v[1]), plot);
        }
        if band_hovered && let Some(h) = resp.hover_pos() {
            tag(
                &tip_layer,
                p,
                h + vec2(12.0, -12.0),
                &format!(
                    "Hysteresis band: the fan slows down only after\nthe temperature drops {:.0} °C",
                    self.hysteresis
                ),
                plot,
            );
        }

        // context menu
        let target = st.ctx_target;
        let ctx_value = st.ctx_value;
        let live = self.live_temp;
        type MenuEdit = Box<dyn FnOnce(&mut Vec<[f32; 2]>, &mut EditorState) -> bool>;
        let mut menu_edit: Option<MenuEdit> = None;
        resp.context_menu(|ui| {
            ui.set_min_width(180.0);
            match target {
                Some(i) => {
                    if ui.button("Edit value…").clicked() {
                        menu_edit = Some(Box::new(move |_, st| {
                            st.selected = Some(i);
                            st.focus_strip = true;
                            false
                        }));
                        ui.close();
                    }
                    if ui.add_enabled(live.is_some(), egui::Button::new("Move to current temperature")).clicked() {
                        menu_edit = Some(Box::new(move |pts, _| {
                            // The curve may have changed while the menu was open (tray profile switch).
                            let Some(p) = pts.get(i) else { return false };
                            let dx = live.unwrap_or(50.0).round() - p[0];
                            logic::nudge(pts, i, dx, 0.0)
                        }));
                        ui.close();
                    }
                    if ui.add_enabled(points.len() > 2, egui::Button::new("Delete point")).clicked() {
                        menu_edit = Some(Box::new(move |pts, st| {
                            let ok = logic::remove(pts, i);
                            st.selected = None;
                            ok
                        }));
                        ui.close();
                    }
                }
                None => {
                    if ui.button("Add point here").clicked() {
                        menu_edit = Some(Box::new(move |pts, st| {
                            let v = ctx_value.unwrap_or([60.0, 50.0]);
                            match logic::insert(pts, [v[0].round(), v[1].round()]) {
                                Some(i) => {
                                    st.selected = Some(i);
                                    true
                                }
                                None => false,
                            }
                        }));
                        ui.close();
                    }
                    if ui.button("Reset to preset").clicked() {
                        out.reset_requested = true;
                        ui.close();
                    }
                }
            }
        });
        if let Some(f) = menu_edit {
            let before = points.clone();
            if f(points, &mut st) {
                st.undo.push(before);
                out.changed = true;
                out.committed = true;
            }
        }
        let focused = resp.has_focus();
        let sel = st.selected;
        store(ui, id, st);
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, "Fan curve editor"));
        let _ = (focused, sel);
        out
    }
}

fn tag(painter: &egui::Painter, p: &crate::ui::theme::Palette, anchor: Pos2, text: &str, bounds: Rect) {
    let g = painter.layout(text.to_string(), tab(Ts::Caption), p.text1, 280.0);
    let size = g.size() + vec2(16.0, 8.0);
    let mut r = Rect::from_center_size(anchor - vec2(0.0, size.y / 2.0), size);
    r = r.translate(vec2(
        (bounds.left() - r.left()).max(0.0) + (bounds.right() - r.right()).min(0.0),
        (bounds.top() - 30.0 - r.top()).max(0.0),
    ));
    painter.add(shadow_e1(p).as_shape(r, 6));
    painter.rect_filled(r, 6, p.surface1);
    painter.rect_stroke(r, 6, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
    painter.galley(r.min + vec2(8.0, 4.0), g, p.text1);
}

/// Numeric point strip under the editor: one (°C, %) pair per point, selection synced both ways.
pub fn point_strip(ui: &mut Ui, id: Id, points: &mut Vec<[f32; 2]>, fahrenheit: bool) -> CurveEdit {
    let p = pal(ui);
    let mut out = CurveEdit::default();
    let mut st = state(ui, id);
    let n = points.len();
    let mut focus_req = st.focus_strip;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
        crate::ui::widgets::text::label(ui, "Points", Ts::Caption, p.text2);
        for i in 0..n {
            // A frame can't wrap itself: start a new row when the chip (2 × 44 + margins) won't fit,
            // otherwise the last chip widens the whole inspector.
            if ui.available_size_before_wrap().x < 124.0 {
                ui.end_row();
            }
            let selected = st.selected == Some(i);
            let frame = egui::Frame::new()
                .fill(if selected { p.accent_subtle } else { p.surface2 })
                .stroke(Stroke::new(1.0, if selected { p.accent } else { p.border_subtle }))
                .corner_radius(R_SM)
                .inner_margin(egui::Margin::symmetric(4, 2));
            frame.show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                ui.horizontal(|ui| {
                    let lo = if i > 0 { points[i - 1][0] + 1.0 } else { T0 };
                    let hi = if i + 1 < n { points[i + 1][0] - 1.0 } else { T1 };
                    let mut t = if fahrenheit { c_to_f(points[i][0]) } else { points[i][0] };
                    let (tlo, thi) = if fahrenheit { (c_to_f(lo), c_to_f(hi.max(lo))) } else { (lo, hi.max(lo)) };
                    let rt = ui.add_sized(
                        vec2(44.0, 22.0),
                        egui::DragValue::new(&mut t)
                            .range(tlo..=thi)
                            .speed(0.2)
                            .fixed_decimals(0)
                            .suffix("°")
                            .update_while_editing(false),
                    );
                    if focus_req && selected {
                        rt.request_focus();
                        focus_req = false;
                    }
                    let mut d = points[i][1];
                    let rd = ui.add_sized(
                        vec2(44.0, 22.0),
                        egui::DragValue::new(&mut d)
                            .range(0.0..=100.0)
                            .speed(0.2)
                            .fixed_decimals(0)
                            .suffix("%")
                            .update_while_editing(false),
                    );
                    for r in [&rt, &rd] {
                        if r.gained_focus() || r.drag_started() {
                            st.selected = Some(i);
                            st.strip_before = Some(points.clone());
                        }
                    }
                    let new_t = if fahrenheit { (t - 32.0) / 1.8 } else { t };
                    if rt.changed() || rd.changed() {
                        if st.strip_before.is_none() {
                            st.strip_before = Some(points.clone());
                        }
                        points[i] = logic::constrain(points, i, [new_t, d]);
                        out.changed = true;
                    }
                    let done = rt.drag_stopped()
                        || rd.drag_stopped()
                        || rt.lost_focus()
                        || rd.lost_focus()
                        || (rt.changed() && !rt.dragged() && !rt.has_focus())
                        || (rd.changed() && !rd.dragged() && !rd.has_focus());
                    if done
                        && let Some(before) = st.strip_before.take()
                        && &before != points
                    {
                        st.undo.push(before);
                        out.committed = true;
                    }
                });
            });
        }
        if ui.available_size_before_wrap().x < 30.0 {
            ui.end_row();
        }
        let add = crate::ui::widgets::controls::icon_button_sized(
            ui,
            crate::ui::icons::Icon::Plus,
            "Add a point",
            26.0,
            None,
        );
        if add.clicked() {
            let after = st.selected.unwrap_or(n.saturating_sub(1));
            let mut v = logic::midpoint_after(points, after);
            if points.iter().any(|q| (q[0] - v[0]).abs() < 1.0) {
                v = logic::midpoint_after(points, after.saturating_sub(1));
            }
            let before = points.clone();
            if let Some(i) = logic::insert(points, v) {
                st.undo.push(before);
                st.selected = Some(i);
                out.changed = true;
                out.committed = true;
            }
        }
    });
    st.focus_strip = focus_req && st.focus_strip;
    if st.focus_strip {
        st.focus_strip = false;
    }
    store(ui, id, st);
    out
}

#[cfg(test)]
mod tests {
    use super::logic::*;
    use egui::pos2;

    #[test]
    fn pick_uses_nearest_within_radius() {
        let pts = [pos2(10.0, 10.0), pos2(30.0, 10.0), pos2(40.0, 10.0)];
        assert_eq!(pick(&pts, pos2(12.0, 11.0), 12.0), Some(0));
        assert_eq!(pick(&pts, pos2(36.0, 10.0), 12.0), Some(2)); // 6 from #2, 6.0 from... nearest wins
        assert_eq!(pick(&pts, pos2(34.0, 10.0), 12.0), Some(1));
        assert_eq!(pick(&pts, pos2(100.0, 100.0), 12.0), None);
    }

    #[test]
    fn press_origin_pick_beats_post_threshold_position() {
        // Regression for the old bug: a fast drag that passes the drag threshold is already
        // several px away when egui reports drag_started; picking there grabs the wrong point.
        let pts = [pos2(100.0, 100.0), pos2(118.0, 100.0)];
        let press = pos2(101.0, 100.0);
        let after_threshold = pos2(111.0, 100.0);
        assert_eq!(pick(&pts, press, 12.0), Some(0));
        assert_eq!(pick(&pts, after_threshold, 12.0), Some(1));
    }

    #[test]
    fn constrain_keeps_order_and_bounds() {
        let pts = vec![[40.0, 30.0], [60.0, 50.0], [80.0, 90.0]];
        assert_eq!(constrain(&pts, 1, [30.0, 120.0]), [41.0, 100.0]);
        assert_eq!(constrain(&pts, 1, [95.0, -5.0]), [79.0, 0.0]);
        assert_eq!(constrain(&pts, 0, [5.0, 10.0]), [20.0, 10.0]);
        assert_eq!(constrain(&pts, 2, [150.0, 10.0]), [100.0, 10.0]);
    }

    #[test]
    fn insert_remove_and_midpoint() {
        let mut pts = vec![[40.0, 30.0], [80.0, 90.0]];
        assert_eq!(insert(&mut pts, [60.0, 50.0]), Some(1));
        assert_eq!(insert(&mut pts, [60.5, 10.0]), None);
        assert_eq!(midpoint_after(&pts, 0), [50.0, 40.0]);
        assert!(remove(&mut pts, 1));
        assert!(!remove(&mut pts, 0), "two points minimum");
        assert_eq!(pts.len(), 2);
    }

    #[test]
    fn nudge_respects_neighbors() {
        let mut pts = vec![[40.0, 30.0], [41.0, 50.0], [80.0, 90.0]];
        assert!(!nudge(&mut pts, 1, -1.0, 0.0));
        assert!(nudge(&mut pts, 1, 0.0, 5.0));
        assert_eq!(pts[1], [41.0, 55.0]);
    }

    #[test]
    fn snapping_modes() {
        assert_eq!(snap(42.36, Snap::Normal), 42.0);
        assert_eq!(snap(42.36, Snap::Coarse), 40.0);
        assert_eq!(snap(42.36, Snap::Fine), 42.4);
    }

    #[test]
    fn undo_redo_roundtrip() {
        let mut u = Undo::default();
        let a = vec![[40.0, 30.0], [80.0, 90.0]];
        let b = vec![[40.0, 35.0], [80.0, 90.0]];
        let mut cur = b.clone();
        u.push(a.clone());
        u.push(a.clone()); // duplicate ignored
        assert_eq!(u.past.len(), 1);
        assert!(u.undo(&mut cur));
        assert_eq!(cur, a);
        assert!(!u.undo(&mut cur));
        assert!(u.redo(&mut cur));
        assert_eq!(cur, b);
        // A new edit clears the redo stack.
        u.undo(&mut cur);
        u.push(cur.clone());
        assert!(u.future.is_empty());
    }

    #[test]
    fn undo_is_capped() {
        let mut u = Undo::default();
        for i in 0..80 {
            u.push(vec![[i as f32, 0.0]]);
        }
        assert_eq!(u.past.len(), super::UNDO_CAP);
        assert_eq!(u.past[0], vec![[30.0, 0.0]]);
    }

    #[test]
    fn alignment_guides() {
        let pts = [[40.0, 30.0], [60.0, 50.0], [80.0, 90.0]];
        assert_eq!(align_target(&pts, 2, 50.6), Some(50.0));
        assert_eq!(align_target(&pts, 1, 50.6), None);
    }
}
