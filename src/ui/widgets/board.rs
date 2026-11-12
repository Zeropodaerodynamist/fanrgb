//! Vector board diagram: PCB landmarks, clickable fan headers with live rings and LED headers
//! glowing in their live preview color.

use egui::{Color32, Id, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::ui::board_map::{BoardMap, HeaderKind, HeaderPos, Landmark, Side};
use crate::ui::fonts::{Ts, font, tab};
use crate::ui::icons::{self, Icon};
use crate::ui::theme::{Palette, R_LG, R_MD, pal, with_alpha};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoardMode {
    Overview,
    Cooling,
    Lighting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FanNodeState {
    Controlled,
    Bios,
    ReadOnly,
    Failsafe,
}

#[derive(Clone, Debug)]
pub struct FanNode {
    pub id: String,
    pub name: String,
    pub rpm: Option<f32>,
    pub duty: Option<f32>,
    pub state: FanNodeState,
    pub mode: String,
    /// Channel the chip exposes but nothing seems connected to (`FanState::unconnected`).
    pub ghost: bool,
    /// Present but reports no speed: "—" in neutral grey, never a warning.
    pub no_tach: bool,
}

#[derive(Clone, Debug)]
pub struct LedNode {
    pub id: String,
    pub name: String,
    pub preview: Vec<[u8; 3]>,
    pub enabled: bool,
    pub effect: String,
    pub leds: u16,
}

pub struct BoardView<'a> {
    pub id: Id,
    pub map: Option<&'static BoardMap>,
    pub mode: BoardMode,
    pub fans: &'a [FanNode],
    pub leds: &'a [LedNode],
    pub selected: Option<&'a str>,
    pub show_ghosts: bool,
    pub preview_only: bool,
    /// Rendered PCB (`board_top@*.png`, exactly the map's 100 × 92 units); `None` draws the vector
    /// landmarks.
    pub texture: Option<egui::TextureId>,
}

fn avg_color(c: &[[u8; 3]]) -> Color32 {
    if c.is_empty() {
        return Color32::BLACK;
    }
    let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
    for x in c {
        r += x[0] as u32;
        g += x[1] as u32;
        b += x[2] as u32;
    }
    let n = c.len() as u32;
    Color32::from_rgb((r / n) as u8, (g / n) as u8, (b / n) as u8)
}

/// Where a header's label goes relative to its glyph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    Right,
    Left,
    Below,
    Above,
}

impl Place {
    fn preferred(side: Side) -> [Place; 4] {
        match side {
            Side::Right => [Place::Right, Place::Left, Place::Below, Place::Above],
            Side::Left => [Place::Left, Place::Right, Place::Below, Place::Above],
        }
    }
}

/// Label rectangle for a glyph centered at `c`.
fn label_rect(c: Pos2, glyph: egui::Vec2, text: egui::Vec2, place: Place) -> Rect {
    let gap = 6.0;
    let min = match place {
        Place::Right => pos2(c.x + glyph.x / 2.0 + gap, c.y - text.y / 2.0),
        Place::Left => pos2(c.x - glyph.x / 2.0 - gap - text.x, c.y - text.y / 2.0),
        Place::Below => pos2(c.x - text.x / 2.0, c.y + glyph.y / 2.0 + 2.0),
        Place::Above => pos2(c.x - text.x / 2.0, c.y - glyph.y / 2.0 - 2.0 - text.y),
    };
    Rect::from_min_size(min, text)
}

fn overlap(a: Rect, b: Rect) -> f32 {
    let i = a.intersect(b);
    if i.is_positive() { i.area() } else { 0.0 }
}

/// Picks a label position per header: the preferred side first, then the others, avoiding the board
/// edge, every glyph and the labels placed so far (least overlap wins if nothing is free).
fn place_labels(bounds: Rect, nodes: &[(Pos2, egui::Vec2, egui::Vec2, Side)]) -> Vec<Place> {
    let glyphs: Vec<Rect> = nodes.iter().map(|(c, g, _, _)| Rect::from_center_size(*c, *g).expand(3.0)).collect();
    let mut placed: Vec<Rect> = Vec::new();
    let mut out = Vec::new();
    for (i, (c, g, t, side)) in nodes.iter().enumerate() {
        let mut best = (f32::INFINITY, Place::preferred(*side)[0], Rect::NOTHING);
        for (rank, place) in Place::preferred(*side).into_iter().enumerate() {
            let r = label_rect(*c, *g, *t, place);
            let outside = r.area() - overlap(r, bounds.shrink(2.0));
            let hits: f32 = glyphs
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, gr)| overlap(r, *gr))
                .chain(placed.iter().map(|pr| overlap(r, pr.expand(2.0))))
                .sum();
            // Small bias so the preferred side wins ties.
            let cost = (outside + hits) * 10.0 + rank as f32;
            if cost < best.0 {
                best = (cost, place, r);
            }
            if outside + hits <= 0.0 {
                break;
            }
        }
        placed.push(best.2);
        out.push(best.1);
    }
    out
}

fn ring_color(p: &Palette, s: FanNodeState) -> Color32 {
    match s {
        FanNodeState::Controlled => p.accent,
        FanNodeState::Bios | FanNodeState::ReadOnly => p.text3,
        FanNodeState::Failsafe => p.danger,
    }
}

impl BoardView<'_> {
    /// Draws the board at the available width. Returns the id of a clicked header.
    pub fn show(self, ui: &mut Ui) -> Option<String> {
        match self.map {
            Some(map) => self.show_map(ui, map),
            None => self.show_strip(ui),
        }
    }

    fn show_map(&self, ui: &mut Ui, map: &BoardMap) -> Option<String> {
        let p = pal(ui);
        // Compact: the map is a navigation aid, cap it and center it.
        let avail = ui.available_width();
        let width = avail.min(420.0);
        let s = width / map.width;
        let (full, _) = ui.allocate_exact_size(vec2(avail, map.height * s), Sense::hover());
        let rect = Rect::from_min_size(pos2(full.center().x - width / 2.0, full.top()), vec2(width, map.height * s));
        let at = |x: f32, y: f32| rect.min + vec2(x * s, y * s);
        let painter = ui.painter_at(rect.expand(2.0));
        painter.rect_filled(rect, R_LG, p.inset);
        if let Some(t) = self.texture {
            painter.image(t, rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        }
        painter.rect_stroke(rect, R_LG, Stroke::new(1.0, p.border), StrokeKind::Inside);
        // Landmarks (context only; the rendered PCB already shows them).
        let stroke = Stroke::new(1.0, p.border);
        let features: &[crate::ui::board_map::Feature] = if self.texture.is_some() { &[] } else { map.features };
        for f in features {
            let r = Rect::from_min_max(at(f.rect.0, f.rect.1), at(f.rect.2, f.rect.3));
            match f.kind {
                Landmark::Socket => {
                    painter.rect_filled(r, 3.0, p.surface2);
                    painter.rect_stroke(r, 3.0, Stroke::new(1.0, p.border_strong), StrokeKind::Inside);
                    let inner = r.shrink(r.width() * 0.18);
                    painter.rect_stroke(inner, 2.0, stroke, StrokeKind::Inside);
                    let t = [
                        inner.left_bottom() + vec2(3.0, -3.0),
                        inner.left_bottom() + vec2(3.0, -9.0),
                        inner.left_bottom() + vec2(9.0, -3.0),
                    ];
                    painter.add(Shape::convex_polygon(t.to_vec(), p.border_strong, Stroke::NONE));
                }
                Landmark::Heatsink => {
                    painter.rect_filled(r, 2.0, with_alpha(p.surface2, 0.7));
                    let mut x = r.left() + 3.0;
                    while x < r.right() - 1.0 {
                        painter.line_segment(
                            [pos2(x, r.top() + 2.0), pos2(x, r.bottom() - 2.0)],
                            Stroke::new(1.0, p.border_subtle),
                        );
                        x += 3.0;
                    }
                }
                Landmark::Dimm | Landmark::Pcie | Landmark::M2 => {
                    painter.rect_filled(r, 1.5, p.surface2);
                    painter.rect_stroke(r, 1.5, stroke, StrokeKind::Inside);
                }
                _ => {
                    painter.rect_stroke(r, 2.0, stroke, StrokeKind::Inside);
                }
            }
        }

        // Header labels first (they matter), then landmark labels only where they don't collide.
        let shown: Vec<&HeaderPos> = map.headers.iter().filter(|hp| self.present(hp, 1.0)).collect();
        let geo: Vec<(Pos2, egui::Vec2, egui::Vec2, Side)> = shown
            .iter()
            .map(|hp| {
                let (g, t) = self.metrics(ui, hp, s);
                (at(hp.at.0, hp.at.1), g, t, hp.label_side)
            })
            .collect();
        let places = place_labels(rect, &geo);
        let occupied: Vec<Rect> = geo
            .iter()
            .zip(&places)
            .map(|((c, g, t, _), pl)| Rect::from_center_size(*c, *g).union(label_rect(*c, *g, *t, *pl)).expand(3.0))
            .collect();
        let mut feature_labels: Vec<Rect> = Vec::new();
        for f in features {
            if f.label.is_empty() || s < 2.6 {
                continue;
            }
            let r = Rect::from_min_max(at(f.rect.0, f.rect.1), at(f.rect.2, f.rect.3));
            let fid = font(Ts::Caption);
            let g = painter.layout_no_wrap(f.label.to_string(), egui::FontId::new(10.5, fid.family.clone()), p.text3);
            let fits = g.size().x + 4.0 < r.width() && g.size().y + 2.0 < r.height().max(14.0);
            let lr = Rect::from_center_size(r.center(), g.size());
            let free = !occupied.iter().chain(feature_labels.iter()).any(|o| o.intersects(lr));
            if (fits || matches!(f.kind, Landmark::Pcie)) && free && rect.shrink(2.0).contains_rect(lr) {
                painter.galley(lr.min, g, p.text3);
                feature_labels.push(lr);
            }
        }

        let mut clicked = None;
        for (hp, place) in shown.iter().zip(places) {
            if let Some(id) = self.node(ui, hp, at(hp.at.0, hp.at.1), s, 1.0, place) {
                clicked = Some(id);
            }
        }
        let ghosts: Vec<HeaderPos> = self
            .fans
            .iter()
            .filter(|f| f.ghost && !map.headers.iter().any(|h| h.id == f.id))
            .map(|f| {
                map.ghosts.iter().find(|g| g.id == f.id).copied().unwrap_or(HeaderPos {
                    id: leak(&f.id),
                    label: leak(&f.name),
                    kind: HeaderKind::Fan,
                    at: (0.0, 0.0),
                    label_side: Side::Right,
                })
            })
            .collect();
        if self.show_ghosts && !ghosts.is_empty() {
            ui.add_space(8.0);
            crate::ui::widgets::text::overline(ui, "Not on this board");
            let w = ui.available_width();
            let (strip, _) = ui.allocate_exact_size(vec2(w, 44.0), Sense::hover());
            ui.painter().rect_stroke(strip, R_MD, Stroke::new(1.0, p.border), StrokeKind::Inside);
            let step = (w / ghosts.len() as f32).max(90.0);
            for (k, g) in ghosts.iter().enumerate() {
                let c = pos2(strip.left() + 20.0 + step * k as f32, strip.center().y);
                if let Some(id) = self.node(ui, g, c, s.min(3.2), 0.3, Place::Right) {
                    clicked = Some(id);
                }
            }
        }
        clicked
    }

    /// Generic fallback: a row of header nodes, no PCB.
    fn show_strip(&self, ui: &mut Ui) -> Option<String> {
        let p = pal(ui);
        let w = ui.available_width();
        let count = self.fans.len() + self.leds.len();
        let per_row = ((w / 120.0).floor() as usize).max(1);
        let rows = count.div_ceil(per_row).max(1);
        let (rect, _) = ui.allocate_exact_size(vec2(w, rows as f32 * 56.0), Sense::hover());
        ui.painter().rect_filled(rect, R_LG, p.inset);
        let mut clicked = None;
        let fans: Vec<HeaderPos> = self
            .fans
            .iter()
            .map(|f| HeaderPos {
                id: leak(&f.id),
                label: leak(&f.name),
                kind: HeaderKind::Fan,
                at: (0.0, 0.0),
                label_side: Side::Right,
            })
            .collect();
        let leds: Vec<HeaderPos> = self
            .leds
            .iter()
            .map(|l| HeaderPos {
                id: leak(&l.id),
                label: leak(&l.name),
                kind: if l.leds > 1 { HeaderKind::Argb } else { HeaderKind::Rgb12 },
                at: (0.0, 0.0),
                label_side: Side::Right,
            })
            .collect();
        for (k, hp) in fans.iter().chain(leds.iter()).enumerate() {
            let (r, c) = (k / per_row, k % per_row);
            let pos = pos2(rect.left() + 24.0 + c as f32 * (w / per_row as f32), rect.top() + 28.0 + r as f32 * 56.0);
            if let Some(id) = self.node(ui, hp, pos, 3.0, 1.0, Place::Right) {
                clicked = Some(id);
            }
        }
        clicked
    }

    /// A fan header the backend doesn't report is not drawn (on the map).
    fn present(&self, hp: &HeaderPos, base_alpha: f32) -> bool {
        hp.kind != HeaderKind::Fan || base_alpha < 1.0 || self.fans.iter().any(|f| f.id == hp.id)
    }

    /// (glyph size, label block size) of a header node at scale `s`.
    fn metrics(&self, ui: &Ui, hp: &HeaderPos, s: f32) -> (egui::Vec2, egui::Vec2) {
        let is_fan = hp.kind == HeaderKind::Fan;
        let label_font = if s < 3.0 { font(Ts::Caption) } else { font(Ts::Label) };
        let r = (s * 3.6).clamp(9.0, 15.0);
        let g = ui.painter().layout_no_wrap(hp.label.to_string(), label_font, Color32::WHITE);
        let (w, h) = if is_fan {
            let rpm = self.fans.iter().find(|f| f.id == hp.id).map(|f| crate::ui::widgets::fmt_rpm(f.rpm));
            let g2 = ui.painter().layout_no_wrap(rpm.unwrap_or_default(), tab(Ts::Caption), Color32::WHITE);
            (g.size().x.max(g2.size().x), g.size().y + g2.size().y)
        } else {
            (g.size().x.max(36.0), g.size().y + 10.0)
        };
        let glyph = if is_fan { vec2(r * 2.0, r * 2.0) } else { vec2(16.0, 16.0) };
        (glyph, vec2(w, h))
    }

    fn node(&self, ui: &mut Ui, hp: &HeaderPos, c: Pos2, s: f32, base_alpha: f32, place: Place) -> Option<String> {
        let p = pal(ui);
        let is_fan = hp.kind == HeaderKind::Fan;
        let inert = match self.mode {
            BoardMode::Overview => false,
            BoardMode::Cooling => !is_fan,
            BoardMode::Lighting => is_fan,
        };
        let alpha = if inert { 0.4 } else { 1.0 } * base_alpha;
        let selected = self.selected == Some(hp.id);
        let small = s < 3.0;
        let label_font = if small { font(Ts::Caption) } else { font(Ts::Label) };
        let r = (s * 3.6).clamp(9.0, 15.0);
        let fan = self.fans.iter().find(|f| f.id == hp.id);
        let led = self.leds.iter().find(|l| l.id == hp.id);
        if is_fan && fan.is_none() && base_alpha >= 1.0 {
            return None; // channel not reported by the backend
        }
        let painter = ui.painter().clone();
        let fade = |c: Color32| with_alpha(c, alpha * (c.a() as f32 / 255.0));
        let label_color = if selected { p.text1 } else { p.text2 };
        let g = painter.layout_no_wrap(hp.label.to_string(), label_font, fade(label_color));
        let sub =
            if is_fan { fan.map(|f| crate::ui::widgets::fmt_rpm(f.rpm)).unwrap_or_default() } else { String::new() };
        let g2 = painter.layout_no_wrap(sub, tab(Ts::Caption), fade(p.text3));
        let text_w = if is_fan { g.size().x.max(g2.size().x) } else { g.size().x.max(36.0) };
        let glyph_w = if is_fan { r * 2.0 } else { 16.0 };
        let text_h = if is_fan { g.size().y + g2.size().y } else { g.size().y + 10.0 };
        let glyph_size = vec2(glyph_w, if is_fan { r * 2.0 } else { 16.0 });
        let text_rect = label_rect(c, glyph_size, vec2(text_w, text_h), place);
        // Centered labels (above / below) center their lines too.
        let centered = matches!(place, Place::Above | Place::Below);
        let line_x = |w: f32| if centered { text_rect.center().x - w / 2.0 } else { text_rect.left() };
        let glyph_rect = Rect::from_center_size(c, glyph_size);
        let hit = glyph_rect.union(text_rect).expand(4.0);

        let resp = if inert || base_alpha < 1.0 {
            ui.interact(hit, self.id.with(hp.id), Sense::hover())
        } else {
            ui.interact(hit, self.id.with(hp.id), Sense::click())
        };
        if resp.hovered() && !inert {
            painter.rect_filled(hit, R_MD, with_alpha(p.surface3, 0.8 * alpha));
        }
        // Dimmed headers of the other kind keep their glyph but drop the label (fixes the D_LED label
        // collision in Cooling).
        let show_label = !(inert && base_alpha >= 1.0);
        // Keep labels legible where they cross a landmark (PCIe slot, DIMMs) or the rendered PCB (which
        // stays dark in the light theme too, so the plate is needed even when selected / hovered).
        if show_label && (self.texture.is_some() || (!selected && (!resp.hovered() || inert))) {
            let a = if self.texture.is_some() { 0.92 } else { 0.85 };
            painter.rect_filled(text_rect.expand(3.0), 4.0, with_alpha(p.inset, a * base_alpha));
        }
        if selected {
            painter.rect_stroke(hit, R_MD, Stroke::new(2.0, p.accent), StrokeKind::Outside);
        }

        if is_fan {
            let state = fan.map(|f| f.state).unwrap_or(FanNodeState::Bios);
            let color = fade(ring_color(p, state));
            // No tach: the glyph spins at a speed estimated from the duty.
            let rpm = fan.and_then(|f| f.rpm.or_else(|| f.no_tach.then(|| f.duty.map(|d| d * 18.0)).flatten()));
            let angle = crate::ui::anim::spin_angle(ui.ctx(), rpm);
            crate::ui::widgets::gauge::fan_ring(
                &painter,
                p,
                glyph_rect.center(),
                r,
                fan.and_then(|f| f.duty),
                color,
                angle,
            );
            if state == FanNodeState::ReadOnly {
                painter.circle_stroke(glyph_rect.center(), r + 3.0, Stroke::new(1.0, fade(p.text3)));
                icons::paint(
                    &painter,
                    Rect::from_center_size(glyph_rect.right_top() + vec2(-1.0, 1.0), vec2(10.0, 10.0)),
                    Icon::Lock,
                    fade(p.text2),
                );
            }
            if state == FanNodeState::Failsafe {
                painter.circle_filled(glyph_rect.right_top(), 5.0, p.danger);
                icons::paint(
                    &painter,
                    Rect::from_center_size(glyph_rect.right_top(), vec2(8.0, 8.0)),
                    Icon::Warning,
                    Color32::WHITE,
                );
            }
            if show_label {
                let gy = g.size().y;
                let (w1, w2) = (g.size().x, g2.size().x);
                painter.galley(pos2(line_x(w1), text_rect.top()), g, fade(label_color));
                painter.galley(pos2(line_x(w2), text_rect.top() + gy), g2, fade(p.text3));
            }
        } else {
            let (preview, enabled) = led.map(|l| (l.preview.as_slice(), l.enabled)).unwrap_or((&[], false));
            let col = avg_color(preview);
            let lum = (col.r() as f32 * 0.3 + col.g() as f32 * 0.59 + col.b() as f32 * 0.11) / 255.0;
            let gc = glyph_rect.center();
            if enabled && lum > 0.02 {
                for (rad, a) in [(22.0, 0.08), (14.0, 0.14), (8.0, 0.22)] {
                    painter.circle_filled(
                        gc,
                        rad * (s / 3.0).clamp(0.7, 1.2),
                        with_alpha(col, a * alpha * (0.5 + lum)),
                    );
                }
            }
            let pins = if hp.kind == HeaderKind::Rgb12 { 4 } else { 3 };
            painter.rect_filled(Rect::from_center_size(gc, vec2(pins as f32 * 4.0 + 4.0, 7.0)), 1.5, fade(p.surface3));
            icons::paint_pins(&painter, gc, pins, 4.0, fade(if enabled { p.text2 } else { p.text3 }));
            let gy = g.size().y;
            let w1 = g.size().x;
            if show_label {
                painter.galley(pos2(line_x(w1), text_rect.top()), g, fade(label_color));
            }
            // mini strip of the live preview (6 LEDs) or one bar for 12 V
            let y = text_rect.top() + gy + 5.0;
            let x0 = line_x(if hp.kind == HeaderKind::Argb { 34.0 } else { 30.0 });
            if !show_label {
                // dimmed: glyph only
            } else if hp.kind == HeaderKind::Argb {
                for k in 0..6 {
                    let c = if preview.is_empty() {
                        [0, 0, 0]
                    } else {
                        preview[(k * preview.len() / 6).min(preview.len() - 1)]
                    };
                    let cc = if enabled { Color32::from_rgb(c[0], c[1], c[2]) } else { p.surface3 };
                    painter.circle_filled(pos2(x0 + 3.0 + k as f32 * 6.0, y), 2.2, fade(cc));
                }
            } else {
                let cc = if enabled { col } else { p.surface3 };
                painter.rect_filled(Rect::from_min_size(pos2(x0, y - 2.0), vec2(30.0, 4.0)), 2.0, fade(cc));
            }
        }

        if !inert {
            let resp = resp.on_hover_ui(|ui| {
                ui.set_max_width(240.0);
                if let Some(f) = fan {
                    crate::ui::widgets::text::label(ui, format!("{} · {}", hp.label, f.name), Ts::BodyStrong, p.text1);
                    crate::ui::widgets::text::label(ui, format!("Mode: {}", f.mode), Ts::Caption, p.text2);
                    let speed = if f.no_tach {
                        "Running · no speed signal".to_string()
                    } else {
                        crate::ui::widgets::fmt_rpm(f.rpm)
                    };
                    crate::ui::widgets::text::label(
                        ui,
                        format!("{} · {}", speed, f.duty.map(|d| format!("{d:.0} %")).unwrap_or_else(|| "BIOS".into())),
                        Ts::Caption,
                        p.text2,
                    );
                } else if let Some(l) = led {
                    let kind = if hp.kind == HeaderKind::Rgb12 { "RGB 12 V, 4-pin" } else { "ARGB 5 V, 3-pin" };
                    crate::ui::widgets::text::label(ui, hp.label, Ts::BodyStrong, p.text1);
                    crate::ui::widgets::text::label(ui, kind, Ts::Caption, p.text2);
                    if hp.kind == HeaderKind::Argb {
                        crate::ui::widgets::text::label(ui, format!("{} LEDs", l.leds), Ts::Caption, p.text2);
                    }
                    crate::ui::widgets::text::label(
                        ui,
                        format!("Effect: {}", if l.enabled { l.effect.as_str() } else { "Off" }),
                        Ts::Caption,
                        p.text2,
                    );
                    if self.preview_only {
                        crate::ui::widgets::text::label(ui, "Preview only (no controller)", Ts::Caption, p.warning);
                    }
                } else {
                    crate::ui::widgets::text::label(ui, hp.label, Ts::BodyStrong, p.text1);
                }
            });
            if resp.clicked() {
                return Some(hp.id.to_string());
            }
        }
        None
    }
}

/// Header strip labels need `'static` strs; these are tiny and bounded by the number of headers.
fn leak(s: &str) -> &'static str {
    thread_local! {
        static CACHE: std::cell::RefCell<std::collections::HashMap<String, &'static str>> = Default::default();
    }
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if let Some(v) = c.get(s) {
            return *v;
        }
        let v: &'static str = Box::leak(s.to_string().into_boxed_str());
        c.insert(s.to_string(), v);
        v
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::board_map::B650M_D3HP;

    /// The compact maps (Cooling 300 px, Overview 420 px) place every header label without touching
    /// another glyph or label and inside the board (the v2 screenshots had "B650SYS_FAN2" etc.).
    #[test]
    fn header_labels_do_not_collide() {
        for width in [300.0f32, 360.0, 420.0] {
            let m = &B650M_D3HP;
            let s = width / m.width;
            let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(width, m.height * s));
            let r = (s * 3.6).clamp(9.0, 15.0);
            let nodes: Vec<(Pos2, egui::Vec2, egui::Vec2, Side)> = m
                .headers
                .iter()
                .map(|h| {
                    let c = pos2(h.at.0 * s, h.at.1 * s);
                    if h.kind == HeaderKind::Fan {
                        (c, vec2(r * 2.0, r * 2.0), vec2(58.0, 30.0), h.label_side)
                    } else {
                        (c, vec2(16.0, 16.0), vec2(42.0, 24.0), h.label_side)
                    }
                })
                .collect();
            let places = place_labels(rect, &nodes);
            let labels: Vec<Rect> =
                nodes.iter().zip(&places).map(|((c, g, t, _), p)| label_rect(*c, *g, *t, *p)).collect();
            for (i, l) in labels.iter().enumerate() {
                assert!(rect.shrink(1.0).contains_rect(*l), "{width}: {} label leaves the board", m.headers[i].label);
                for (j, (c, g, _, _)) in nodes.iter().enumerate() {
                    if i != j {
                        assert!(
                            overlap(*l, Rect::from_center_size(*c, *g)) == 0.0,
                            "{width}: {} label covers {}",
                            m.headers[i].label,
                            m.headers[j].label
                        );
                    }
                }
                for (j, o) in labels.iter().enumerate() {
                    if i != j {
                        assert!(
                            overlap(*l, *o) == 0.0,
                            "{width}: labels {} / {}",
                            m.headers[i].label,
                            m.headers[j].label
                        );
                    }
                }
            }
        }
    }
}
