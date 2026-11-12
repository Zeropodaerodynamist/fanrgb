//! Issue card: severity icon, title, code chip (copy), copy-details, plain-language help, one fix
//! button wired to the real `FixAction`, technical-details disclosure, dismiss for warnings/info.

use egui::{Frame, Id, Margin, Rect, Sense, Stroke, Ui, pos2, vec2};

use super::card::Sev;
use super::controls::{self, Button, Kind, Size};
use super::text;
use crate::error::{FixAction, Severity};
use crate::ui::fonts::Ts;
use crate::ui::icons::{self, Icon};
use crate::ui::issues::{Issue, fmt_time};
use crate::ui::theme::{R_MD, pal};

#[derive(Default)]
pub struct CardResult {
    pub fix: Option<FixAction>,
    pub reconnect: bool,
    pub open_logs: bool,
    pub copy_details: bool,
    pub copy_code: bool,
    pub dismiss: bool,
}

/// `compact`: banner density (one paragraph, no technical details).
pub fn issue_card(ui: &mut Ui, issue: &Issue, compact: bool, busy: bool) -> CardResult {
    let p = pal(ui);
    let mut out = CardResult::default();
    let sev = Sev::from_severity(issue.severity());
    let (subtle, strong) = controls::tone_colors(p, sev.tone());
    let critical = issue.severity() == Severity::Critical;
    let fill = if critical || compact { subtle } else { p.surface1 };
    let id = Id::new(("issue", issue.code.number(), issue.subject.clone()));
    let where_ = ui.painter().add(egui::Shape::Noop);
    let inner = Frame::new().inner_margin(Margin { left: 16, right: 12, top: 12, bottom: 12 }).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_top(|ui| {
            let (ir, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
            icons::paint(ui.painter(), ir, sev.icon(), strong);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                // title row
                ui.horizontal(|ui| {
                    let right_w = 150.0;
                    let title_w = (ui.available_width() - right_w).max(120.0);
                    ui.scope(|ui| {
                        ui.set_max_width(title_w);
                        text::para(ui, &issue.title, Ts::Headline, p.text1);
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        if issue.severity() <= Severity::Warning
                            && controls::icon_button_sized(ui, Icon::Close, "Hide until restart", 26.0, None).clicked()
                        {
                            out.dismiss = true;
                        }
                        if controls::icon_button_sized(ui, Icon::Copy, "Copy details", 26.0, None).clicked() {
                            out.copy_details = true;
                        }
                        if controls::code_chip(ui, &issue.code.id()).clicked() {
                            out.copy_code = true;
                        }
                    });
                });
                // explanation
                let mut help = issue.code.help().to_string();
                if let Some(s) = &issue.subject {
                    help.push_str(&format!(" Affects {s}."));
                }
                text::para(ui, &help, Ts::Body, p.text2);
                // actions
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let fix = issue.code.fix();
                    if fix != FixAction::None {
                        let kind = if critical || issue.severity() == Severity::Error {
                            Kind::Primary
                        } else {
                            Kind::Secondary
                        };
                        if Button::new(kind, fix.label())
                            .size(Size::Sm)
                            .loading(busy && fix == FixAction::ReconnectRgb)
                            .show(ui)
                            .clicked()
                        {
                            out.fix = Some(fix);
                        }
                    }
                    let rgb = (400..500).contains(&issue.code.number());
                    if rgb && fix != FixAction::ReconnectRgb && issue.code != crate::error::ErrorCode::AudioCapture {
                        if Button::new(Kind::Ghost, "Reconnect")
                            .size(Size::Sm)
                            .icon(Icon::Refresh)
                            .loading(busy)
                            .show(ui)
                            .clicked()
                        {
                            out.reconnect = true;
                        }
                    } else if !rgb
                        && fix != FixAction::OpenLogFolder
                        && issue.severity() >= Severity::Error
                        && !compact
                        && Button::new(Kind::Ghost, "Open log folder")
                            .size(Size::Sm)
                            .icon(Icon::Folder)
                            .show(ui)
                            .clicked()
                    {
                        out.open_logs = true;
                    }
                });
                if !compact && !issue.detail.is_empty() {
                    let open_id = id.with("details");
                    let open: bool = ui.data(|d| d.get_temp(open_id)).unwrap_or(false);
                    let (r, resp) = ui.allocate_exact_size(vec2(130.0, 20.0), Sense::click());
                    icons::paint(
                        ui.painter(),
                        Rect::from_center_size(pos2(r.left() + 7.0, r.center().y), vec2(14.0, 14.0)),
                        if open { Icon::ChevronDown } else { Icon::ChevronRight },
                        p.text3,
                    );
                    ui.painter().text(
                        pos2(r.left() + 18.0, r.center().y),
                        egui::Align2::LEFT_CENTER,
                        "Technical details",
                        crate::ui::fonts::font(Ts::Caption),
                        if resp.hovered() { p.text1 } else { p.text3 },
                    );
                    if resp.clicked() {
                        ui.data_mut(|d| d.insert_temp(open_id, !open));
                    }
                    if open {
                        Frame::new().fill(p.inset).corner_radius(6).inner_margin(Margin::same(8)).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            text::mono(ui, &issue.detail, p.text2);
                            text::label(
                                ui,
                                format!(
                                    "First seen {} · last {} · {}×",
                                    fmt_time(issue.first_seen),
                                    fmt_time(issue.last_seen),
                                    issue.count
                                ),
                                Ts::Caption,
                                p.text3,
                            );
                        });
                    }
                }
            });
        });
    });
    let rect = inner.response.rect;
    ui.painter().set(where_, egui::Shape::rect_filled(rect, R_MD, fill));
    if compact || critical {
        let bar = Rect::from_min_max(rect.min, pos2(rect.left() + 3.0, rect.bottom()));
        ui.painter().rect_filled(bar, egui::CornerRadius { nw: R_MD, sw: R_MD, ne: 0, se: 0 }, strong);
    } else {
        ui.painter().rect_stroke(rect, R_MD, Stroke::new(1.0, p.border_subtle), egui::StrokeKind::Inside);
    }
    out
}
