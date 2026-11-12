//! Sparklines and time-series charts reading the engine `History` in place (no per-frame clones).

use std::collections::VecDeque;

use egui::epaint::Mesh;
use egui::{Align2, Color32, Id, LayerId, Order, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, pos2, vec2};

use crate::engine::History;
use crate::ui::anim;
use crate::ui::fonts::{Ts, font, tab};
use crate::ui::theme::{R_SM, R_XS, pal, shadow_e1, with_alpha};

/// Value of series `s` at time index `i` (series are right-aligned with `times`).
#[inline]
pub fn at(times_len: usize, s: &VecDeque<f32>, i: usize) -> f32 {
    let off = times_len.saturating_sub(s.len());
    if i < off { f32::NAN } else { s.get(i - off).copied().unwrap_or(f32::NAN) }
}

/// First time index inside the last `secs` seconds.
pub fn window_start(times: &VecDeque<f64>, secs: f64) -> usize {
    let Some(&end) = times.back() else { return 0 };
    let t0 = end - secs;
    // times are monotonic: binary search on the two slices.
    let (a, b) = times.as_slices();
    if let Some(&last_a) = a.last()
        && last_a >= t0
    {
        return a.partition_point(|t| *t < t0);
    }
    a.len() + b.partition_point(|t| *t < t0)
}

/// "Nice" grid step for a range.
pub fn nice_step(range: f32, ticks: f32) -> f32 {
    let raw = (range / ticks).max(1e-6);
    let mag = 10f32.powf(raw.log10().floor());
    for m in [1.0, 2.0, 2.5, 5.0, 10.0] {
        if raw <= m * mag {
            return m * mag;
        }
    }
    10.0 * mag
}

/// Gradient area mesh between a polyline and `bottom` (premultiplied vertex colors).
fn area_mesh(points: &[Pos2], bottom: f32, color: Color32, alpha_top: f32) -> Mesh {
    let mut mesh = Mesh::default();
    let top = with_alpha(color, alpha_top);
    let bot = with_alpha(color, 0.0);
    for w in points.windows(2) {
        let i = mesh.vertices.len() as u32;
        mesh.colored_vertex(w[0], top);
        mesh.colored_vertex(w[1], top);
        mesh.colored_vertex(pos2(w[1].x, bottom), bot);
        mesh.colored_vertex(pos2(w[0].x, bottom), bot);
        mesh.add_triangle(i, i + 1, i + 2);
        mesh.add_triangle(i, i + 2, i + 3);
    }
    mesh
}

/// Sparkline of the last `secs` of `id` with gradient area and last-value dot.
#[allow(clippy::too_many_arguments)]
pub fn sparkline(
    ui: &mut Ui,
    hist: &History,
    id: &str,
    secs: f64,
    size: egui::Vec2,
    color: Color32,
    min_span: f32,
    map: impl Fn(f32) -> f32,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let Some(s) = hist.series.get(id) else { return resp };
    let n = hist.times.len();
    let start = window_start(&hist.times, secs);
    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
    for i in start..n {
        let v = map(at(n, s, i));
        if v.is_finite() {
            lo = lo.min(v);
            hi = hi.max(v);
        }
    }
    if lo > hi {
        return resp;
    }
    if hi - lo < min_span {
        let m = (hi + lo) / 2.0;
        lo = m - min_span / 2.0;
        hi = m + min_span / 2.0;
    }
    let (t_end, t0) = (*hist.times.back().unwrap_or(&0.0), hist.times.back().unwrap_or(&0.0) - secs);
    let to_pos = |i: usize, v: f32| {
        pos2(
            rect.left() + ((hist.times[i] - t0) / secs) as f32 * rect.width(),
            rect.bottom() - 2.0 - (v - lo) / (hi - lo) * (rect.height() - 4.0),
        )
    };
    let _ = t_end;
    let painter = ui.painter_at(rect.expand(3.0));
    let mut seg: Vec<Pos2> = Vec::with_capacity(n - start);
    let mut last = None;
    let flush = |seg: &mut Vec<Pos2>| {
        if seg.len() > 1 {
            painter.add(Shape::mesh(area_mesh(seg, rect.bottom(), color, 0.22)));
            painter.add(Shape::line(std::mem::take(seg), Stroke::new(1.5, color)));
        }
        seg.clear();
    };
    for i in start..n {
        let v = map(at(n, s, i));
        if v.is_finite() {
            let p = to_pos(i, v);
            seg.push(p);
            last = Some(p);
        } else {
            flush(&mut seg);
        }
    }
    flush(&mut seg);
    if let Some(p) = last {
        painter.circle_filled(p, 2.5, color);
    }
    resp
}

#[derive(Clone)]
pub struct ChartSeries {
    pub id: String,
    pub name: String,
    pub color: Color32,
}

#[derive(Clone)]
pub struct Threshold {
    pub value: f32,
    pub color: Color32,
    pub label: String,
}

pub struct TimeChart<'a> {
    pub id: Id,
    /// Charts sharing a group share the crosshair.
    pub group: Option<Id>,
    pub height: f32,
    pub range_s: f64,
    pub series: &'a [ChartSeries],
    pub thresholds: &'a [Threshold],
    pub unit: &'a str,
    pub decimals: usize,
    pub min_span: f32,
    pub floor: Option<f32>,
    /// Display mapping (e.g. °C → °F).
    pub map: fn(f32) -> f32,
}

#[derive(Clone, Copy, Default)]
struct YState {
    lo: f32,
    hi: f32,
    shrink_since: Option<f64>,
}

#[derive(Clone, Copy)]
struct Hover {
    t: f64,
    pass: u64,
}

pub fn hidden_key(chart: Id) -> Id {
    chart.with("hidden")
}

pub fn is_hidden(ui: &Ui, chart: Id, series: &str) -> bool {
    ui.data(|d| d.get_temp::<Vec<String>>(hidden_key(chart))).is_some_and(|v| v.iter().any(|s| s == series))
}

/// Legend chips: click toggles a series.
pub fn legend(ui: &mut Ui, chart: Id, series: &[ChartSeries]) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for s in series {
            let hidden = is_hidden(ui, chart, &s.id);
            let g = ui.painter().layout_no_wrap(s.name.clone(), font(Ts::Caption), p.text2);
            let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 26.0, 24.0), Sense::click());
            if resp.hovered() {
                ui.painter().rect_filled(rect, 12, p.surface2);
            }
            let c = pos2(rect.left() + 10.0, rect.center().y);
            if hidden {
                ui.painter().circle_stroke(c, 3.5, Stroke::new(1.2, s.color));
            } else {
                ui.painter().circle_filled(c, 4.0, s.color);
            }
            ui.painter().galley(
                pos2(rect.left() + 19.0, rect.center().y - g.size().y / 2.0),
                g,
                if hidden { p.text3 } else { p.text2 },
            );
            if resp.clicked() {
                let key = hidden_key(chart);
                ui.data_mut(|d| {
                    let v: &mut Vec<String> = d.get_temp_mut_or_default(key);
                    if let Some(i) = v.iter().position(|x| x == &s.id) {
                        v.remove(i);
                    } else {
                        v.push(s.id.clone());
                    }
                });
            }
            let _ = resp.on_hover_text(if hidden { "Show series" } else { "Hide series" });
        }
    });
}

impl TimeChart<'_> {
    pub fn show(self, ui: &mut Ui, hist: &History) -> Response {
        let p = pal(ui);
        let width = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(vec2(width, self.height), Sense::hover());
        let resp = ui.interact(rect, self.id, Sense::hover());
        if !ui.is_rect_visible(rect) {
            return resp;
        }
        let plot = Rect::from_min_max(rect.min + vec2(36.0, 6.0), rect.max - vec2(6.0, 20.0));
        let painter = ui.painter_at(rect);
        painter.rect_filled(plot.expand2(vec2(4.0, 4.0)), R_SM, p.inset);

        let n = hist.times.len();
        let start = window_start(&hist.times, self.range_s);
        let t_end = hist.times.back().copied().unwrap_or(0.0);
        let t0 = t_end - self.range_s;
        let visible: Vec<&ChartSeries> = self.series.iter().filter(|s| !is_hidden(ui, self.id, &s.id)).collect();

        // ---- y range (expands immediately, shrinks after 5 s, tweened) ----
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        for s in &visible {
            if let Some(data) = hist.series.get(&s.id) {
                for i in start..n {
                    let v = (self.map)(at(n, data, i));
                    if v.is_finite() {
                        lo = lo.min(v);
                        hi = hi.max(v);
                    }
                }
            }
        }
        let has_data = lo <= hi && n > start + 1;
        if !has_data {
            for k in 1..4 {
                let y = plot.top() + plot.height() * k as f32 / 4.0;
                painter.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], Stroke::new(1.0, p.border_subtle));
            }
            painter.text(plot.center(), Align2::CENTER_CENTER, "Collecting data…", font(Ts::Caption), p.text3);
            return resp;
        }
        for th in self.thresholds {
            let v = (self.map)(th.value);
            if v > hi && v - hi < (hi - lo).max(self.min_span) * 0.6 {
                hi = v;
            }
        }
        if hi - lo < self.min_span {
            let m = (hi + lo) / 2.0;
            lo = m - self.min_span / 2.0;
            hi = m + self.min_span / 2.0;
        }
        if let Some(f) = self.floor {
            lo = f;
            hi = hi.max(f + self.min_span);
        }
        let step = nice_step(hi - lo, 4.0);
        let mut tlo = (lo / step).floor() * step;
        let thi = ((hi / step).ceil() * step).max(tlo + step);
        if let Some(f) = self.floor {
            tlo = tlo.max(f);
        }
        let now = ui.input(|i| i.time);
        let key = self.id.with("y");
        let mut ys: YState = ui.data(|d| d.get_temp(key)).unwrap_or(YState { lo: tlo, hi: thi, shrink_since: None });
        if tlo < ys.lo || thi > ys.hi {
            ys.lo = ys.lo.min(tlo);
            ys.hi = ys.hi.max(thi);
            ys.shrink_since = None;
        } else if tlo > ys.lo || thi < ys.hi {
            match ys.shrink_since {
                None => ys.shrink_since = Some(now),
                Some(t) if now - t > 5.0 => {
                    ys.lo = tlo;
                    ys.hi = thi;
                    ys.shrink_since = None;
                }
                _ => {}
            }
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(1000));
        } else {
            ys.shrink_since = None;
        }
        ui.data_mut(|d| d.insert_temp(key, ys));
        let lo = anim::tween(ui.ctx(), self.id.with("ylo"), ys.lo, anim::VALUE);
        let hi = anim::tween(ui.ctx(), self.id.with("yhi"), ys.hi, anim::VALUE).max(lo + 1e-3);
        let to_y = |v: f32| plot.bottom() - (v - lo) / (hi - lo) * plot.height();
        let to_x = |t: f64| plot.left() + ((t - t0) / self.range_s) as f32 * plot.width();

        // ---- grid ----
        let step = nice_step(hi - lo, 4.0);
        let mut g = (lo / step).ceil() * step;
        while g <= hi + 1e-3 {
            let y = painter.round_to_pixel_center(to_y(g));
            painter.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], Stroke::new(1.0, p.border_subtle));
            painter.text(
                pos2(plot.left() - 8.0, y),
                Align2::RIGHT_CENTER,
                format!("{g:.0}"),
                tab(Ts::Caption),
                p.text3,
            );
            g += step;
        }
        let tick_s = if self.range_s <= 60.0 {
            15.0
        } else if self.range_s <= 300.0 {
            60.0
        } else {
            300.0
        };
        let mut k = 0.0;
        while k <= self.range_s + 0.1 {
            let x = to_x(t_end - k);
            let label = if k == 0.0 {
                "now".to_string()
            } else if tick_s < 60.0 {
                format!("-{k:.0}s")
            } else {
                format!("-{:.0}m", k / 60.0)
            };
            let align = if k == 0.0 {
                Align2::RIGHT_TOP
            } else if k >= self.range_s - 0.1 {
                Align2::LEFT_TOP
            } else {
                Align2::CENTER_TOP
            };
            painter.text(pos2(x, plot.bottom() + 6.0), align, label, font(Ts::Caption), p.text3);
            k += tick_s;
        }

        // ---- thresholds ----
        for th in self.thresholds {
            let v = (self.map)(th.value);
            if v < lo || v > hi {
                continue;
            }
            let y = to_y(v);
            painter.extend(Shape::dashed_line(
                &[pos2(plot.left(), y), pos2(plot.right(), y)],
                Stroke::new(1.0, with_alpha(th.color, 0.7)),
                4.0,
                4.0,
            ));
            let gal = painter.layout_no_wrap(th.label.clone(), font(Ts::Caption), th.color);
            let r = Rect::from_min_size(
                pos2(plot.right() - gal.size().x - 12.0, y - gal.size().y / 2.0 - 1.0),
                gal.size() + vec2(8.0, 2.0),
            );
            painter.rect_filled(r, R_XS, p.inset);
            painter.galley(r.min + vec2(4.0, 1.0), gal, th.color);
        }

        // ---- series (decimated per pixel column) ----
        let count = n - start;
        let cols = plot.width().max(1.0) as usize;
        for s in &visible {
            let Some(data) = hist.series.get(&s.id) else { continue };
            let stroke = Stroke::new(1.75, s.color);
            if count <= cols * 2 {
                let mut seg: Vec<Pos2> = Vec::with_capacity(count);
                for i in start..n {
                    let v = (self.map)(at(n, data, i));
                    if v.is_finite() {
                        seg.push(pos2(to_x(hist.times[i]), to_y(v)));
                    } else if seg.len() > 1 {
                        painter.add(Shape::line(std::mem::take(&mut seg), stroke));
                    } else {
                        seg.clear();
                    }
                }
                if seg.len() > 1 {
                    painter.add(Shape::line(seg, stroke));
                }
            } else {
                // min/max span + mean polyline per column.
                let mut mean_line: Vec<Pos2> = Vec::with_capacity(cols);
                let mut col = usize::MAX;
                let (mut mn, mut mx, mut sum, mut cnt) = (f32::MAX, f32::MIN, 0.0f32, 0u32);
                let span_c = with_alpha(s.color, 0.35);
                let emit = |col: usize, mn: f32, mx: f32, mean: f32, line: &mut Vec<Pos2>| {
                    let x = plot.left() + col as f32 + 0.5;
                    if mx - mn > 0.0 {
                        painter.line_segment([pos2(x, to_y(mn)), pos2(x, to_y(mx))], Stroke::new(1.0, span_c));
                    }
                    line.push(pos2(x, to_y(mean)));
                };
                for i in start..n {
                    let v = (self.map)(at(n, data, i));
                    let c = (((hist.times[i] - t0) / self.range_s) as f32 * plot.width()).clamp(0.0, cols as f32 - 1.0)
                        as usize;
                    if c != col {
                        if cnt > 0 {
                            emit(col, mn, mx, sum / cnt as f32, &mut mean_line);
                        }
                        col = c;
                        mn = f32::MAX;
                        mx = f32::MIN;
                        sum = 0.0;
                        cnt = 0;
                    }
                    if v.is_finite() {
                        mn = mn.min(v);
                        mx = mx.max(v);
                        sum += v;
                        cnt += 1;
                    }
                }
                if cnt > 0 {
                    emit(col, mn, mx, sum / cnt as f32, &mut mean_line);
                }
                if mean_line.len() > 1 {
                    painter.add(Shape::line(mean_line, stroke));
                }
            }
        }

        // ---- crosshair (shared within the group) ----
        let pass = ui.ctx().cumulative_pass_nr();
        let mut hover_t: Option<f64> = None;
        let mut own = false;
        if let Some(pos) = resp.hover_pos().filter(|p| plot.contains(*p)) {
            hover_t = Some(t0 + ((pos.x - plot.left()) / plot.width()) as f64 * self.range_s);
            own = true;
            if let Some(gid) = self.group {
                ui.data_mut(|d| d.insert_temp(gid.with("hover"), Hover { t: hover_t.unwrap(), pass }));
            }
        } else if let Some(gid) = self.group
            && let Some(h) = ui.data(|d| d.get_temp::<Hover>(gid.with("hover")))
            && pass.saturating_sub(h.pass) <= 1
        {
            hover_t = Some(h.t);
        }
        if let Some(ht) = hover_t {
            // nearest sample index
            let idx = {
                let (a, b) = hist.times.as_slices();
                let i = if a.last().is_some_and(|l| *l >= ht) {
                    a.partition_point(|t| *t < ht)
                } else {
                    a.len() + b.partition_point(|t| *t < ht)
                };
                let i = i.clamp(start, n - 1);
                if i > start && (hist.times[i] - ht).abs() > (hist.times[i - 1] - ht).abs() { i - 1 } else { i }
            };
            let x = to_x(hist.times[idx]);
            painter.line_segment([pos2(x, plot.top()), pos2(x, plot.bottom())], Stroke::new(1.0, p.border_strong));
            let mut rows: Vec<(f32, &ChartSeries)> = Vec::new();
            for s in &visible {
                if let Some(data) = hist.series.get(&s.id) {
                    let v = (self.map)(at(n, data, idx));
                    if v.is_finite() {
                        painter.circle_filled(pos2(x, to_y(v)), 4.0, s.color);
                        painter.circle_stroke(pos2(x, to_y(v)), 4.0, Stroke::new(1.5, p.inset));
                        rows.push((v, s));
                    }
                }
            }
            if own && !rows.is_empty() {
                rows.sort_by(|a, b| b.0.total_cmp(&a.0));
                let ago = t_end - hist.times[idx];
                let tl = ui.ctx().layer_painter(LayerId::new(Order::Tooltip, self.id.with("tip")));
                let title = super::local_clock(ago);
                let lines: Vec<(Color32, String, String)> = rows
                    .iter()
                    .map(|(v, s)| (s.color, s.name.clone(), format!("{v:.prec$}{}", self.unit, prec = self.decimals)))
                    .collect();
                let name_w = lines
                    .iter()
                    .map(|l| tl.layout_no_wrap(l.1.clone(), font(Ts::Caption), p.text2).size().x)
                    .fold(0.0, f32::max);
                let val_w = lines
                    .iter()
                    .map(|l| tl.layout_no_wrap(l.2.clone(), tab(Ts::Caption), p.text1).size().x)
                    .fold(0.0, f32::max);
                let w = (14.0 + name_w + 16.0 + val_w).max(110.0) + 16.0;
                let h = 8.0 + 18.0 + lines.len() as f32 * 18.0 + 6.0;
                let flip = x > plot.left() + plot.width() * 0.6;
                let left = if flip { x - 12.0 - w } else { x + 12.0 };
                let r = Rect::from_min_size(pos2(left, plot.top() + 4.0), vec2(w, h));
                tl.add(shadow_e1(p).as_shape(r, R_SM));
                tl.rect_filled(r, R_SM, p.surface1);
                tl.rect_stroke(r, R_SM, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
                tl.text(r.min + vec2(8.0, 8.0), Align2::LEFT_TOP, title, tab(Ts::Caption), p.text2);
                for (k, (c, name, val)) in lines.into_iter().enumerate() {
                    let y = r.top() + 8.0 + 18.0 + k as f32 * 18.0 + 8.0;
                    tl.circle_filled(pos2(r.left() + 12.0, y), 3.5, c);
                    tl.text(pos2(r.left() + 22.0, y), Align2::LEFT_CENTER, name, font(Ts::Caption), p.text2);
                    tl.text(pos2(r.right() - 8.0, y), Align2::RIGHT_CENTER, val, tab(Ts::Caption), p.text1);
                }
            }
        }
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_start_handles_wrapped_deque() {
        let mut t: VecDeque<f64> = VecDeque::with_capacity(4);
        for i in 0..4 {
            t.push_back(i as f64);
        }
        t.pop_front();
        t.push_back(4.0); // [1,2,3,4] possibly split across slices
        assert_eq!(window_start(&t, 10.0), 0);
        assert_eq!(window_start(&t, 1.5), 2); // t >= 2.5 -> 3.0 at index 2
        assert_eq!(window_start(&VecDeque::new(), 5.0), 0);
    }

    #[test]
    fn series_right_aligned() {
        let s: VecDeque<f32> = [1.0, 2.0].into_iter().collect();
        assert!(at(4, &s, 1).is_nan());
        assert_eq!(at(4, &s, 2), 1.0);
        assert_eq!(at(4, &s, 3), 2.0);
    }

    #[test]
    fn nice_steps() {
        assert_eq!(nice_step(40.0, 4.0), 10.0);
        assert_eq!(nice_step(9.0, 4.0), 2.5);
        assert_eq!(nice_step(2000.0, 4.0), 500.0);
    }
}
