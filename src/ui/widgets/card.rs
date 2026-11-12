//! Cards, section headers, banners and empty/blocking states.

use egui::{Color32, Frame, Margin, Rect, Sense, Stroke, Ui, pos2, vec2};

use super::controls::{Tone, tone_colors};
use super::text;
use crate::ui::fonts::{Ts, font};
use crate::ui::icons::{self, Icon};
use crate::ui::theme::{R_LG, R_MD, R_XL, pal};

pub fn card_frame(ui: &Ui, pad: i8) -> Frame {
    let p = pal(ui);
    Frame::new()
        .fill(p.surface1)
        .stroke(Stroke::new(1.0, p.border_subtle))
        .corner_radius(R_LG)
        .inner_margin(Margin::same(pad))
}

/// Standard card (16 pt padding), full available width.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    card_frame(ui, 16)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// Card stretched to at least `min_outer_h`; returns its *natural* outer height (for `layout::row`).
pub fn card_eq(ui: &mut Ui, min_outer_h: f32, pad: i8, add: impl FnOnce(&mut Ui)) -> f32 {
    let mut natural = 0.0;
    card_frame(ui, pad).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let top = ui.min_rect().top();
        add(ui);
        natural = ui.min_rect().bottom() - top;
        let inner_min = min_outer_h - 2.0 * pad as f32 - 2.0;
        // Pad with an explicit allocation (`set_min_height` measures from the cursor in egui 0.33).
        let extra = inner_min - natural - ui.spacing().item_spacing.y;
        if extra > 0.5 {
            ui.allocate_space(egui::vec2(0.0, extra));
        }
    });
    natural + 2.0 * pad as f32 + 2.0
}

/// Card header row: headline title, optional caption subtitle, trailing content right-aligned.
pub fn header(ui: &mut Ui, title: &str, subtitle: Option<&str>, trailing: impl FnOnce(&mut Ui)) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        ui.set_min_height(28.0);
        text::label(ui, title, Ts::Headline, p.text1);
        if let Some(s) = subtitle {
            ui.add_space(2.0);
            text::label(ui, s, Ts::Caption, p.text2);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), trailing);
    });
}

/// Hairline divider (1 physical pixel).
pub fn divider(ui: &mut Ui) {
    let p = pal(ui);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 1.0), Sense::hover());
    let px = 1.0 / ui.ctx().pixels_per_point();
    let y = ui.painter().round_to_pixel_center(rect.center().y);
    ui.painter().line_segment([pos2(rect.left(), y), pos2(rect.right(), y)], Stroke::new(px, p.border_subtle));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sev {
    Info,
    Warning,
    Error,
    Critical,
}

impl Sev {
    pub fn icon(self) -> Icon {
        match self {
            Sev::Info => Icon::Info,
            Sev::Warning => Icon::Warning,
            Sev::Error => Icon::Error,
            Sev::Critical => Icon::Critical,
        }
    }
    pub fn tone(self) -> Tone {
        match self {
            Sev::Info => Tone::Info,
            Sev::Warning => Tone::Warning,
            Sev::Error | Sev::Critical => Tone::Danger,
        }
    }
    pub fn from_severity(s: crate::error::Severity) -> Self {
        match s {
            crate::error::Severity::Info => Sev::Info,
            crate::error::Severity::Warning => Sev::Warning,
            crate::error::Severity::Error => Sev::Error,
            crate::error::Severity::Critical => Sev::Critical,
        }
    }
}

/// Page-level banner: subtle fill, 2 pt left severity bar, icon, title + body, trailing actions.
pub fn banner(ui: &mut Ui, sev: Sev, title: &str, body: &str, actions: impl FnOnce(&mut Ui)) {
    let p = pal(ui);
    let (fill, fg) = tone_colors(p, sev.tone());
    let where_ = ui.painter().add(egui::Shape::Noop);
    let resp = Frame::new().inner_margin(Margin { left: 16, right: 12, top: 10, bottom: 10 }).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let (ir, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
            icons::paint(ui.painter(), ir, sev.icon(), fg);
            ui.add_space(4.0);
            let actions_w = 280.0f32.min(ui.available_width() * 0.45);
            let text_w = (ui.available_width() - actions_w).max(160.0);
            ui.vertical(|ui| {
                ui.set_max_width(text_w);
                ui.spacing_mut().item_spacing.y = 2.0;
                text::para(ui, title, Ts::BodyStrong, p.text1);
                if !body.is_empty() {
                    text::para(ui, body, Ts::Caption, p.text2);
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), actions);
        });
    });
    let rect = resp.response.rect;
    ui.painter().set(where_, egui::Shape::rect_filled(rect, R_MD, fill));
    let bar = Rect::from_min_max(rect.min, pos2(rect.left() + 3.0, rect.bottom()));
    ui.painter().rect_filled(bar, egui::CornerRadius { nw: R_MD, sw: R_MD, ne: 0, se: 0 }, fg);
}

/// Centered blocking/empty state card (max 520 pt): icon, title, ≤2 lines, buttons.
pub fn empty_state(
    ui: &mut Ui,
    icon: Icon,
    icon_color: Option<Color32>,
    title: &str,
    body: &str,
    add: impl FnOnce(&mut Ui),
) {
    let p = pal(ui);
    let w = ui.available_width().min(520.0);
    let margin = ((ui.available_width() - w) / 2.0).max(0.0);
    ui.add_space(24.0);
    ui.horizontal(|ui| {
        ui.add_space(margin);
        ui.vertical(|ui| {
            ui.set_width(w);
            Frame::new()
                .fill(p.surface1)
                .stroke(Stroke::new(1.0, p.border_subtle))
                .corner_radius(R_XL)
                .inner_margin(Margin::same(28))
                .show(ui, |ui| {
                    ui.set_width(w - 58.0);
                    ui.vertical_centered(|ui| {
                        let (ir, _) = ui.allocate_exact_size(vec2(56.0, 56.0), Sense::hover());
                        ui.painter().circle_filled(ir.center(), 28.0, p.surface2);
                        icons::paint(
                            ui.painter(),
                            Rect::from_center_size(ir.center(), vec2(28.0, 28.0)),
                            icon,
                            icon_color.unwrap_or(p.text2),
                        );
                        ui.add_space(8.0);
                        text::label(ui, title, Ts::Headline, p.text1);
                        ui.add_space(2.0);
                        let g = ui.painter().layout(body.to_string(), font(Ts::Body), p.text2, w - 80.0);
                        let (r, _) = ui.allocate_exact_size(vec2(w - 80.0, g.size().y), Sense::hover());
                        // Center each line.
                        let galley = ui.painter().layout_job({
                            let mut job =
                                egui::text::LayoutJob::simple(body.to_string(), font(Ts::Body), p.text2, w - 80.0);
                            job.halign = egui::Align::Center;
                            job
                        });
                        ui.painter().galley(pos2(r.center().x, r.top()), galley, p.text2);
                        ui.add_space(12.0);
                        add(ui);
                    });
                });
        });
    });
}
