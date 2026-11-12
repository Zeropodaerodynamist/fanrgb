//! Buttons, toggles, segmented controls, sliders, chips and fields.

use std::ops::RangeInclusive;

use egui::{
    Color32, CursorIcon, EventFilter, Id, Key, Pos2, Rect, Response, Sense, Stroke, StrokeKind, Ui, UiBuilder,
    WidgetInfo, WidgetType, pos2, vec2,
};

use crate::ui::anim;
use crate::ui::fonts::{Ts, font, tab};
use crate::ui::icons::{self, Icon};
use crate::ui::theme::{self, Palette, R_MD, R_PILL, R_SM, R_XS, mix, pal, with_alpha};

// ---------------------------------------------------------------------------------------
// Buttons

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Primary,
    Secondary,
    Ghost,
    Danger,
    DangerFill,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    Sm,
    Md,
    Lg,
}

impl Size {
    fn height(self) -> f32 {
        match self {
            Size::Sm => 28.0,
            Size::Md => 32.0,
            Size::Lg => 40.0,
        }
    }
    fn pad(self) -> f32 {
        match self {
            Size::Sm => 10.0,
            Size::Md => 12.0,
            Size::Lg => 16.0,
        }
    }
}

fn button_colors(p: &Palette, kind: Kind, hover: f32, press: bool) -> (Color32, Color32, Option<Color32>) {
    // (fill, text, stroke)
    match kind {
        Kind::Primary => {
            let fill = if press { p.accent_press } else { mix(p.accent, p.accent_hover, hover) };
            (fill, p.on_accent, None)
        }
        Kind::Secondary => {
            let hover_fill = if p.dark { theme::mix(p.surface2, p.surface3, 0.6) } else { p.surface3 };
            let fill = if press { p.surface3 } else { mix(p.surface2, hover_fill, hover) };
            (fill, p.text1, Some(mix(p.border, p.border_strong, hover)))
        }
        Kind::Ghost => {
            let fill = if press { p.surface3 } else { with_alpha(p.surface2, hover) };
            (fill, p.text1, None)
        }
        Kind::Danger => {
            let fill = if press { p.danger_subtle } else { mix(p.surface2, p.danger_subtle, hover) };
            (fill, p.danger, Some(p.border))
        }
        Kind::DangerFill => {
            let fill = mix(p.danger, theme::mix(p.danger, Color32::BLACK, 0.15), if press { 1.0 } else { hover * 0.6 });
            (fill, Color32::WHITE, None)
        }
    }
}

pub struct Button<'a> {
    pub kind: Kind,
    pub size: Size,
    pub icon: Option<Icon>,
    pub text: &'a str,
    pub min_width: f32,
    pub loading: bool,
}

impl<'a> Button<'a> {
    pub fn new(kind: Kind, text: &'a str) -> Self {
        Button { kind, size: Size::Md, icon: None, text, min_width: 0.0, loading: false }
    }
    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }
    pub fn size(mut self, size: Size) -> Self {
        self.size = size;
        self
    }
    pub fn min_width(mut self, w: f32) -> Self {
        self.min_width = w;
        self
    }
    pub fn loading(mut self, on: bool) -> Self {
        self.loading = on;
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let p = pal(ui);
        let galley = ui.painter().layout_no_wrap(self.text.to_string(), font(Ts::Label), p.text1);
        let pad = self.size.pad();
        let icon_w = if self.icon.is_some() || self.loading {
            if self.text.is_empty() { 18.0 } else { 16.0 + 6.0 }
        } else {
            0.0
        };
        let text_w = if self.text.is_empty() { 0.0 } else { galley.size().x };
        let w = (pad * 2.0 + icon_w + text_w).max(self.min_width).max(self.size.height());
        let (rect, resp) = ui.allocate_exact_size(vec2(w, self.size.height()), Sense::click());
        if ui.is_rect_visible(rect) {
            let hover = anim::toggle(ui.ctx(), resp.id.with("hover"), resp.hovered() && ui.is_enabled(), anim::FAST);
            let press = resp.is_pointer_button_down_on();
            let (fill, text, stroke) = button_colors(p, self.kind, hover, press);
            let painter = ui.painter();
            painter.rect_filled(rect, R_MD, fill);
            if let Some(s) = stroke {
                painter.rect_stroke(rect, R_MD, Stroke::new(1.0, s), StrokeKind::Inside);
            }
            let content_w = icon_w + text_w;
            let mut x = rect.center().x - content_w / 2.0;
            if self.loading {
                spinner(painter, pos2(x + 8.0, rect.center().y), 6.0, text, ui.input(|i| i.time));
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
                x += icon_w;
            } else if let Some(ic) = self.icon {
                let r = Rect::from_center_size(pos2(x + 8.0, rect.center().y), vec2(16.0, 16.0));
                icons::paint(painter, r, ic, text);
                x += icon_w;
            }
            if !self.text.is_empty() {
                painter.galley(pos2(x, rect.center().y - galley.size().y / 2.0), galley, text);
            }
        }
        let label = self.text.to_string();
        resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &label));
        resp
    }
}

/// Square ghost icon button with a tooltip.
pub fn icon_button(ui: &mut Ui, icon: Icon, tooltip: &str) -> Response {
    icon_button_sized(ui, icon, tooltip, 32.0, None)
}

pub fn icon_button_sized(ui: &mut Ui, icon: Icon, tooltip: &str, side: f32, color: Option<Color32>) -> Response {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(side, side), Sense::click());
    if ui.is_rect_visible(rect) {
        let hover = anim::toggle(ui.ctx(), resp.id.with("hover"), resp.hovered(), anim::FAST);
        let fill = if resp.is_pointer_button_down_on() { p.surface3 } else { with_alpha(p.surface2, hover) };
        ui.painter().rect_filled(rect, R_MD, fill);
        let c = color.unwrap_or_else(|| mix(p.text2, p.text1, hover));
        let isz = if side >= 32.0 { 18.0 } else { 16.0 };
        icons::paint(ui.painter(), Rect::from_center_size(rect.center(), vec2(isz, isz)), icon, c);
    }
    let t = tooltip.to_string();
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &t));
    if tooltip.is_empty() { resp } else { resp.on_hover_text(tooltip) }
}

/// Text-only link-style button (accent text, underline on hover).
pub fn link(ui: &mut Ui, text: &str) -> Response {
    let p = pal(ui);
    let g = ui.painter().layout_no_wrap(text.to_string(), font(Ts::Label), p.accent);
    let (rect, resp) = ui.allocate_exact_size(g.size(), Sense::click());
    let c = if resp.hovered() { p.accent_hover } else { p.accent };
    ui.painter().galley(rect.min, g, c);
    if resp.hovered() {
        ui.painter().line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, c));
    }
    resp
}

pub fn spinner(painter: &egui::Painter, center: Pos2, r: f32, color: Color32, time: f64) {
    let a0 = (time as f32 * 6.0) % std::f32::consts::TAU;
    let n = 20;
    let pts: Vec<Pos2> = (0..=n)
        .map(|i| {
            let a = a0 + i as f32 / n as f32 * 4.4;
            center + vec2(a.cos(), a.sin()) * r
        })
        .collect();
    painter.add(egui::Shape::line(pts, Stroke::new(1.6, color)));
}

// ---------------------------------------------------------------------------------------
// Toggle

fn paint_toggle(ui: &Ui, rect: Rect, on: bool, id: Id, hovered: bool, pressed: bool) {
    let p = pal(ui);
    let t = anim::toggle(ui.ctx(), id.with("knob"), on, anim::BASE);
    let painter = ui.painter();
    let track_off = if hovered { p.border_strong } else { p.surface3 };
    painter.rect_filled(rect, R_PILL, mix(track_off, if hovered { p.accent_hover } else { p.accent }, t));
    if t < 0.99 {
        painter.rect_stroke(rect, R_PILL, Stroke::new(1.0, with_alpha(p.border_strong, 1.0 - t)), StrokeKind::Inside);
    }
    let kr = 8.0;
    let x = egui::lerp((rect.left() + 2.0 + kr)..=(rect.right() - 2.0 - kr), t);
    let c = pos2(x, rect.center().y);
    painter.circle_filled(c + vec2(0.0, 0.8), kr, Color32::from_black_alpha(40));
    let knob =
        if pressed { Rect::from_center_size(c, vec2(18.0, 16.0)) } else { Rect::from_center_size(c, vec2(16.0, 16.0)) };
    painter.rect_filled(knob, R_PILL, Color32::WHITE);
}

/// 36×20 switch.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(vec2(36.0, 20.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        paint_toggle(ui, rect, *on, resp.id, resp.hovered(), resp.is_pointer_button_down_on());
    }
    let v = *on;
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, true, v, ""));
    resp
}

/// Settings row: label (+ helper) left, switch right; the whole row is the hit target.
pub fn toggle_row(ui: &mut Ui, label: &str, helper: &str, on: &mut bool) -> bool {
    let p = pal(ui);
    let h = if helper.is_empty() { 44.0 } else { 56.0 };
    let w = ui.available_width();
    let (rect, mut resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        if resp.hovered() {
            ui.painter().rect_filled(rect.expand2(vec2(6.0, 0.0)), R_MD, p.surface2);
        }
        paint_row_text(ui, rect, label, helper, 56.0);
        let tr = Rect::from_center_size(pos2(rect.right() - 18.0, rect.center().y), vec2(36.0, 20.0));
        paint_toggle(ui, tr, *on, resp.id, resp.hovered(), resp.is_pointer_button_down_on());
    }
    let (v, l) = (*on, label.to_string());
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, true, v, &l));
    resp.changed()
}

fn paint_row_text(ui: &Ui, rect: Rect, label: &str, helper: &str, right_reserve: f32) {
    let p = pal(ui);
    let max_w = (rect.width() - right_reserve).max(80.0);
    let g1 = ui.painter().layout(label.to_string(), font(Ts::Body), p.text1, max_w);
    if helper.is_empty() {
        ui.painter().galley(pos2(rect.left(), rect.center().y - g1.size().y / 2.0), g1, p.text1);
    } else {
        let g2 = ui.painter().layout(helper.to_string(), font(Ts::Caption), p.text2, max_w);
        let total = g1.size().y + 2.0 + g2.size().y;
        let y = rect.center().y - total / 2.0;
        let h1 = g1.size().y;
        ui.painter().galley(pos2(rect.left(), y), g1, p.text1);
        ui.painter().galley(pos2(rect.left(), y + h1 + 2.0), g2, p.text2);
    }
}

/// Settings row with an arbitrary control right-aligned.
pub fn setting_row<R>(ui: &mut Ui, label: &str, helper: &str, control_w: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    let h = if helper.is_empty() { 44.0 } else { 56.0 };
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    paint_row_text(ui, rect, label, helper, control_w + 16.0);
    let cr = Rect::from_min_max(pos2(rect.right() - control_w, rect.top()), rect.max);
    ui.scope_builder(UiBuilder::new().max_rect(cr).layout(egui::Layout::right_to_left(egui::Align::Center)), add).inner
}

// ---------------------------------------------------------------------------------------
// Segmented control

/// Pure helper: index after a keyboard step (wraps disabled).
pub fn step_index(current: usize, len: usize, key: Key) -> usize {
    match key {
        Key::ArrowLeft | Key::ArrowUp => current.saturating_sub(1),
        Key::ArrowRight | Key::ArrowDown => (current + 1).min(len.saturating_sub(1)),
        Key::Home => 0,
        Key::End => len.saturating_sub(1),
        _ => current,
    }
}

/// Segmented control; `width = None` fits the content. One focus stop, ←/→ move the selection.
pub fn segmented<T: PartialEq + Copy>(
    ui: &mut Ui,
    id: Id,
    value: &mut T,
    options: &[(T, &str)],
    width: Option<f32>,
) -> Response {
    segmented_sized(ui, id, value, options, width, 32.0)
}

pub fn segmented_sized<T: PartialEq + Copy>(
    ui: &mut Ui,
    id: Id,
    value: &mut T,
    options: &[(T, &str)],
    width: Option<f32>,
    height: f32,
) -> Response {
    let p = pal(ui);
    let n = options.len().max(1);
    let galleys: Vec<_> =
        options.iter().map(|(_, l)| ui.painter().layout_no_wrap(l.to_string(), font(Ts::Label), p.text1)).collect();
    let seg_w = match width {
        Some(w) => (w - 4.0) / n as f32,
        None => galleys.iter().map(|g| g.size().x).fold(0.0, f32::max) + 24.0,
    };
    let total = seg_w * n as f32 + 4.0;
    let (rect, _) = ui.allocate_exact_size(vec2(total, height), Sense::hover());
    let mut resp = ui.interact(rect, id, Sense::click());
    let current = options.iter().position(|(v, _)| v == value).unwrap_or(0);
    let mut new = current;
    if resp.clicked()
        && let Some(pos) = resp.interact_pointer_pos()
    {
        new = (((pos.x - rect.left() - 2.0) / seg_w).floor().max(0.0) as usize).min(n - 1);
    }
    if resp.has_focus() {
        ui.memory_mut(|m| m.set_focus_lock_filter(id, EventFilter { horizontal_arrows: true, ..Default::default() }));
        for key in [Key::ArrowLeft, Key::ArrowRight, Key::Home, Key::End] {
            if ui.input(|i| i.key_pressed(key)) {
                new = step_index(new, n, key);
            }
        }
    }
    if new != current && new < options.len() {
        *value = options[new].0;
        resp.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect_filled(rect, R_MD, p.inset);
        painter.rect_stroke(rect, R_MD, Stroke::new(1.0, p.border_subtle), StrokeKind::Inside);
        let idx = options.iter().position(|(v, _)| v == value).unwrap_or(0) as f32;
        let x = anim::tween(ui.ctx(), id.with("thumb"), idx, anim::BASE);
        let thumb =
            Rect::from_min_size(pos2(rect.left() + 2.0 + x * seg_w, rect.top() + 2.0), vec2(seg_w, height - 4.0));
        let thumb_fill = if p.dark { p.surface3 } else { p.surface1 };
        painter.rect_filled(
            thumb.translate(vec2(0.0, 1.0)),
            R_SM,
            Color32::from_black_alpha(if p.dark { 60 } else { 18 }),
        );
        painter.rect_filled(thumb, R_SM, thumb_fill);
        let hover_idx =
            resp.hover_pos().map(|pos| (((pos.x - rect.left() - 2.0) / seg_w).floor().max(0.0) as usize).min(n - 1));
        for (i, g) in galleys.into_iter().enumerate() {
            let selected = options[i].0 == *value;
            let c = if selected || hover_idx == Some(i) { p.text1 } else { p.text2 };
            let cx = rect.left() + 2.0 + seg_w * (i as f32 + 0.5);
            painter.galley(pos2(cx - g.size().x / 2.0, rect.center().y - g.size().y / 2.0), g, c);
        }
    }
    resp
}

// ---------------------------------------------------------------------------------------
// Slider

/// Pure helper: snap `v` to `step` within `range`.
pub fn snap(v: f32, range: &RangeInclusive<f32>, step: f32) -> f32 {
    let (lo, hi) = (*range.start(), *range.end());
    let v = v.clamp(lo, hi);
    if step > 0.0 { (lo + ((v - lo) / step).round() * step).clamp(lo, hi) } else { v }
}

/// Custom slider: 4 pt rail, accent fill, white knob. Arrows ±step (Shift ×10), PageUp/Down ±10 %,
/// Home/End.
pub fn slider(ui: &mut Ui, id: Id, value: &mut f32, range: RangeInclusive<f32>, step: f32, width: f32) -> Response {
    slider_tinted(ui, id, value, range, step, width, None)
}

/// [`slider`] with its own fill colour (calibration channel sliders).
pub fn slider_tinted(
    ui: &mut Ui,
    id: Id,
    value: &mut f32,
    range: RangeInclusive<f32>,
    step: f32,
    width: f32,
    fill_color: Option<Color32>,
) -> Response {
    let p = pal(ui);
    let h = 24.0;
    let (rect, _) = ui.allocate_exact_size(vec2(width.max(60.0), h), Sense::hover());
    let mut resp = ui.interact(rect, id, Sense::click_and_drag());
    let (lo, hi) = (*range.start(), *range.end());
    let span = (hi - lo).max(1e-6);
    let rail = Rect::from_min_max(
        pos2(rect.left() + 7.0, rect.center().y - 2.0),
        pos2(rect.right() - 7.0, rect.center().y + 2.0),
    );
    let old = *value;
    if (resp.dragged() || resp.clicked() || resp.drag_started())
        && ui.is_enabled()
        && let Some(pos) = resp.interact_pointer_pos()
    {
        let f = ((pos.x - rail.left()) / rail.width()).clamp(0.0, 1.0);
        *value = snap(lo + f * span, &range, step);
    }
    if resp.has_focus() {
        ui.memory_mut(|m| m.set_focus_lock_filter(id, EventFilter { horizontal_arrows: true, ..Default::default() }));
        let (shift, keys) = ui.input(|i| {
            (
                i.modifiers.shift,
                [
                    Key::ArrowLeft,
                    Key::ArrowRight,
                    Key::ArrowDown,
                    Key::ArrowUp,
                    Key::PageDown,
                    Key::PageUp,
                    Key::Home,
                    Key::End,
                ]
                .map(|k| i.key_pressed(k)),
            )
        });
        let st = if step > 0.0 { step } else { span / 100.0 } * if shift { 10.0 } else { 1.0 };
        let deltas = [-st, st, -st, st, -span * 0.1, span * 0.1];
        for (k, d) in deltas.iter().enumerate() {
            if keys[k] {
                *value = snap(*value + d, &range, step);
            }
        }
        if keys[6] {
            *value = lo;
        }
        if keys[7] {
            *value = hi;
        }
    }
    if *value != old {
        resp.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let f = ((*value - lo) / span).clamp(0.0, 1.0);
        painter.rect_filled(rail, R_PILL, p.surface3);
        let mut fill = rail;
        fill.set_right(rail.left() + rail.width() * f);
        if fill.width() > 0.5 {
            painter.rect_filled(
                fill,
                R_PILL,
                if ui.is_enabled() { fill_color.unwrap_or(p.accent) } else { p.text_disabled },
            );
        }
        let c = pos2(rail.left() + rail.width() * f, rail.center().y);
        let active = resp.hovered() || resp.dragged();
        let kr = if active { 8.0 } else { 7.0 };
        painter.circle_filled(c + vec2(0.0, 1.0), kr, Color32::from_black_alpha(50));
        painter.circle_filled(c, kr, Color32::WHITE);
        painter.circle_stroke(c, kr, Stroke::new(1.0, p.border_strong));
        if active {
            ui.ctx().set_cursor_icon(CursorIcon::Grab);
        }
    }
    let v = *value as f64;
    resp.widget_info(|| WidgetInfo::slider(true, v, ""));
    resp
}

/// Label column + slider + numeric field. Returns true when the value changed.
#[allow(clippy::too_many_arguments)]
pub fn slider_row(
    ui: &mut Ui,
    id: Id,
    label: &str,
    value: &mut f32,
    range: RangeInclusive<f32>,
    step: f32,
    suffix: &str,
    decimals: usize,
) -> bool {
    let p = pal(ui);
    let narrow = ui.available_width() < 420.0;
    let label_w = if narrow { 0.0 } else { 150.0 };
    let field_w = 76.0;
    let mut changed = false;
    if narrow {
        super::text::label(ui, label, Ts::Body, p.text2);
    }
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        if !narrow {
            let (r, _) = ui.allocate_exact_size(vec2(label_w, 24.0), Sense::hover());
            let g = ui.painter().layout(label.to_string(), font(Ts::Body), p.text2, label_w - 8.0);
            ui.painter().galley(pos2(r.left(), r.center().y - g.size().y / 2.0), g, p.text2);
        }
        let sw = (ui.available_width() - field_w - 12.0).max(80.0);
        changed |= slider(ui, id, value, range.clone(), step, sw).changed();
        let dv = egui::DragValue::new(value)
            .range(range.clone())
            .speed(if step > 0.0 { step } else { 0.1 })
            .fixed_decimals(decimals)
            .suffix(suffix)
            .update_while_editing(false);
        let r = ui.add_sized(vec2(field_w, 26.0), dv);
        changed |= r.changed();
    });
    changed
}

// ---------------------------------------------------------------------------------------
// Chips

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Accent,
    Success,
    Warning,
    Danger,
    Info,
}

pub fn tone_colors(p: &Palette, tone: Tone) -> (Color32, Color32) {
    match tone {
        Tone::Neutral => (p.surface3, p.text2),
        Tone::Accent => (p.accent_subtle, p.accent),
        Tone::Success => (p.success_subtle, p.success),
        Tone::Warning => (p.warning_subtle, p.warning),
        Tone::Danger => (p.danger_subtle, p.danger),
        Tone::Info => (p.info_subtle, p.info),
    }
}

/// Status chip (pill, 22 pt).
pub fn chip(ui: &mut Ui, text: &str, tone: Tone) -> Response {
    let p = pal(ui);
    let (fill, fg) = tone_colors(p, tone);
    let g = ui.painter().layout_no_wrap(text.to_string(), font(Ts::Caption), fg);
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 16.0, 22.0), Sense::hover());
    ui.painter().rect_filled(rect, R_PILL, fill);
    ui.painter().galley(pos2(rect.left() + 8.0, rect.center().y - g.size().y / 2.0), g, fg);
    resp
}

/// Chip with a leading status dot.
pub fn dot_chip(ui: &mut Ui, text: &str, tone: Tone) -> Response {
    let p = pal(ui);
    let (fill, fg) = tone_colors(p, tone);
    let g = ui.painter().layout_no_wrap(text.to_string(), font(Ts::Caption), fg);
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 26.0, 22.0), Sense::hover());
    ui.painter().rect_filled(rect, R_PILL, fill);
    ui.painter().circle_filled(pos2(rect.left() + 10.0, rect.center().y), 3.0, fg);
    ui.painter().galley(pos2(rect.left() + 18.0, rect.center().y - g.size().y / 2.0), g, fg);
    resp
}

/// One status chip for [`chip_flow`].
pub struct ChipSpec {
    pub text: String,
    pub tone: Tone,
    pub dot: bool,
    pub tip: Option<String>,
}

impl ChipSpec {
    pub fn new(text: impl Into<String>, tone: Tone) -> Self {
        ChipSpec { text: text.into(), tone, dot: false, tip: None }
    }
    pub fn dot(mut self) -> Self {
        self.dot = true;
        self
    }
    pub fn tip(mut self, t: impl Into<String>) -> Self {
        self.tip = Some(t.into());
        self
    }
}

/// Chips flowed into lines that never exceed the available width (measured up front, so a long
/// chip list can't widen the parent the way a wrapped layout can).
pub fn chip_flow(ui: &mut Ui, chips: &[ChipSpec], gap: f32) {
    let w = ui.available_width();
    let widths: Vec<f32> = chips
        .iter()
        .map(|c| {
            ui.painter().layout_no_wrap(c.text.clone(), font(Ts::Caption), Color32::WHITE).size().x
                + if c.dot { 26.0 } else { 16.0 }
        })
        .collect();
    let mut lines: Vec<Vec<usize>> = vec![Vec::new()];
    let mut x = 0.0;
    for (i, cw) in widths.iter().enumerate() {
        let last = lines.last_mut().expect("one line");
        if !last.is_empty() && x + gap + cw > w {
            lines.push(vec![i]);
            x = *cw;
        } else {
            x += if last.is_empty() { *cw } else { gap + cw };
            last.push(i);
        }
    }
    for line in lines.iter().filter(|l| !l.is_empty()) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for &i in line {
                let c = &chips[i];
                let r = if c.dot { dot_chip(ui, &c.text, c.tone) } else { chip(ui, &c.text, c.tone) };
                if let Some(t) = &c.tip {
                    r.on_hover_text(t);
                }
            }
        });
    }
}

/// Toggleable filter chip (24 pt).
pub fn filter_chip(ui: &mut Ui, text: &str, selected: bool) -> Response {
    let p = pal(ui);
    let g = ui.painter().layout_no_wrap(text.to_string(), font(Ts::Label), p.text1);
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 20.0, 28.0), Sense::click());
    let (fill, fg, stroke) = if selected {
        (p.accent_subtle, p.accent, p.accent)
    } else if resp.hovered() {
        (p.surface2, p.text1, p.border_strong)
    } else {
        (Color32::TRANSPARENT, p.text2, p.border)
    };
    ui.painter().rect_filled(rect, R_PILL, fill);
    ui.painter().rect_stroke(rect, R_PILL, Stroke::new(1.0, stroke), StrokeKind::Inside);
    ui.painter().galley(pos2(rect.left() + 10.0, rect.center().y - g.size().y / 2.0), g, fg);
    resp
}

/// `FRG-E401` code chip; click copies.
pub fn code_chip(ui: &mut Ui, code: &str) -> Response {
    let p = pal(ui);
    let g = ui.painter().layout_no_wrap(code.to_string(), font(Ts::Mono), p.text2);
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 12.0, 20.0), Sense::click());
    let fill = if resp.hovered() { p.border } else { p.surface3 };
    ui.painter().rect_filled(rect, R_XS, fill);
    ui.painter().galley(
        pos2(rect.left() + 6.0, rect.center().y - g.size().y / 2.0),
        g,
        if resp.hovered() { p.text1 } else { p.text2 },
    );
    resp.on_hover_text("Copy code")
}

/// Keyboard shortcut hint.
pub fn kbd(ui: &mut Ui, text: &str) -> Response {
    let p = pal(ui);
    let g = ui.painter().layout_no_wrap(text.to_string(), font(Ts::Caption), p.text2);
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 10.0, 20.0), Sense::hover());
    ui.painter().rect_filled(rect, R_XS, p.surface2);
    ui.painter().rect_stroke(rect, R_XS, Stroke::new(1.0, p.border), StrokeKind::Inside);
    ui.painter().galley(pos2(rect.left() + 5.0, rect.center().y - g.size().y / 2.0), g, p.text2);
    resp
}

/// Thin progress bar.
pub fn progress(ui: &mut Ui, frac: f32, color: Color32, width: f32, height: f32) -> Response {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    paint_bar(ui.painter(), rect, frac, color, p.surface3);
    resp
}

pub fn paint_bar(painter: &egui::Painter, rect: Rect, frac: f32, color: Color32, track: Color32) {
    painter.rect_filled(rect, R_PILL, track);
    let mut fill = rect;
    fill.set_width(rect.width() * frac.clamp(0.0, 1.0));
    if fill.width() >= rect.height().min(1.0) {
        painter.rect_filled(fill, R_PILL, color);
    }
}

// ---------------------------------------------------------------------------------------
// Fields

/// Search field with a leading icon.
pub fn search_field(ui: &mut Ui, id: Id, text: &mut String, hint: &str, width: f32) -> Response {
    let p = pal(ui);
    let te = egui::TextEdit::singleline(text)
        .id(id)
        .hint_text(super::text::rt(hint, Ts::Body, p.text3))
        .font(font(Ts::Body))
        .desired_width(width - 40.0)
        .margin(egui::Margin { left: 32, right: 8, top: 7, bottom: 7 });
    let resp = ui.add_sized(vec2(width, 32.0), te);
    icons::paint(
        ui.painter(),
        Rect::from_center_size(pos2(resp.rect.left() + 16.0, resp.rect.center().y), vec2(16.0, 16.0)),
        Icon::Search,
        p.text3,
    );
    resp
}

/// Small numeric field with tabular digits.
pub fn number_field(
    ui: &mut Ui,
    value: &mut f32,
    range: RangeInclusive<f32>,
    speed: f32,
    decimals: usize,
    suffix: &str,
    width: f32,
) -> Response {
    let dv = egui::DragValue::new(value)
        .range(range)
        .speed(speed)
        .fixed_decimals(decimals)
        .suffix(suffix)
        .update_while_editing(false);
    ui.add_sized(vec2(width, 26.0), dv)
}

/// A stat block: caption label over a tabular value.
pub fn stat(ui: &mut Ui, label: &str, value: &str, color: Color32, ts: Ts) {
    let p = pal(ui);
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        super::text::label(ui, label, Ts::Caption, p.text2);
        ui.add(egui::Label::new(egui::RichText::new(value).font(tab(ts)).color(color)).selectable(false));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_to_step() {
        let r = 0.0..=100.0;
        assert_eq!(snap(42.4, &r, 1.0), 42.0);
        assert_eq!(snap(42.6, &r, 5.0), 45.0);
        assert_eq!(snap(-3.0, &r, 1.0), 0.0);
        assert_eq!(snap(103.0, &r, 1.0), 100.0);
        assert_eq!(snap(0.333, &(0.0..=1.0), 0.0), 0.333);
    }

    #[test]
    fn segmented_keyboard_steps_clamp() {
        assert_eq!(step_index(0, 4, Key::ArrowLeft), 0);
        assert_eq!(step_index(0, 4, Key::ArrowRight), 1);
        assert_eq!(step_index(3, 4, Key::ArrowRight), 3);
        assert_eq!(step_index(2, 4, Key::Home), 0);
        assert_eq!(step_index(1, 4, Key::End), 3);
    }
}
