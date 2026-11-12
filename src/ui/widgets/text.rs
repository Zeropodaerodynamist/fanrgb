//! Text helpers on the type scale. Pages never call `RichText::size()`.

use egui::text::LayoutJob;
use egui::{Color32, Label, Response, RichText, Sense, TextFormat, Ui, WidgetText};

use crate::ui::fonts::{Ts, font, tab};
use crate::ui::theme::pal;

pub fn rt(text: impl Into<String>, ts: Ts, color: Color32) -> RichText {
    RichText::new(text).font(font(ts)).color(color)
}

/// Non-selectable, non-wrapping label.
pub fn label(ui: &mut Ui, text: impl Into<String>, ts: Ts, color: Color32) -> Response {
    ui.add(Label::new(rt(text, ts, color)).selectable(false).extend())
}

/// Wrapping label.
pub fn para(ui: &mut Ui, text: impl Into<String>, ts: Ts, color: Color32) -> Response {
    ui.add(Label::new(rt(text, ts, color)).selectable(false).wrap())
}

pub fn caption(ui: &mut Ui, text: impl Into<String>) -> Response {
    let c = pal(ui).text2;
    para(ui, text, Ts::Caption, c)
}

pub fn caption3(ui: &mut Ui, text: impl Into<String>) -> Response {
    let c = pal(ui).text3;
    para(ui, text, Ts::Caption, c)
}

pub fn mono(ui: &mut Ui, text: impl Into<String>, color: Color32) -> Response {
    ui.add(Label::new(RichText::new(text).font(font(Ts::Mono)).color(color)).selectable(true).wrap())
}

/// Section label: semibold 11 pt, uppercase, +0.6 letter spacing.
pub fn overline_job(text: &str, color: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.append(
        &text.to_uppercase(),
        0.0,
        TextFormat { font_id: font(Ts::Overline), extra_letter_spacing: 0.6, color, ..Default::default() },
    );
    job
}

pub fn overline(ui: &mut Ui, text: &str) -> Response {
    let c = pal(ui).text3;
    ui.add(Label::new(WidgetText::LayoutJob(overline_job(text, c).into())).selectable(false))
}

/// "Value + small unit" pair, e.g. `1,180` + `rpm`, tabular value. Returns the allocated response.
pub fn value_unit(
    ui: &mut Ui,
    value: &str,
    unit: &str,
    ts: Ts,
    unit_ts: Ts,
    color: Color32,
    unit_color: Color32,
) -> Response {
    let g1 = ui.painter().layout_no_wrap(value.to_string(), tab(ts), color);
    let g2 = ui.painter().layout_no_wrap(unit.to_string(), font(unit_ts), unit_color);
    let gap = if unit.is_empty() { 0.0 } else { 3.0 };
    let size = egui::vec2(g1.size().x + gap + g2.size().x, g1.size().y.max(g2.size().y));
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let base_y = rect.bottom();
    let p = ui.painter();
    // Align baselines approximately: both galleys bottom-aligned minus descender share.
    p.galley(egui::pos2(rect.left(), base_y - g1.size().y), g1.clone(), color);
    let d = (g1.size().y - g2.size().y) * 0.18;
    p.galley(egui::pos2(rect.left() + g1.size().x + gap, base_y - g2.size().y - d), g2, unit_color);
    resp
}
