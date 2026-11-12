//! Lighting › Colour calibration over `rgb::calibrate::CalSession`.
//!
//! Test colours go to the real LEDs through the same pipeline as effects, while the tab is open.
//! The guided path ("How does white look on the fans?") comes first; sliders, the colour order and
//! the transfer chart are there for fine control. Nothing is saved until Done; leaving the tab or
//! the page brings the user's lighting back (the edits stay until Done / Discard).

use std::time::Instant;

use egui::{Align2, Color32, Id, Rect, Sense, Shape, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::config::{ColorCorrection, ColorOrder, GAIN_RANGE, GAMMA_RANGE};
use crate::rgb::RgbCommand;
use crate::rgb::calibrate::{CalSession, Feedback, FeedbackResult, Swatch, Target};
use crate::rgb::color::Rgb;
use crate::ui::PageCtx;
use crate::ui::fonts::{Ts, font, tab};
use crate::ui::icons::Icon;
use crate::ui::overlays::ToastKind;
use crate::ui::theme::{R_MD, R_SM, pal};
use crate::ui::widgets::controls::{self, Button, Kind, Size, Tone};
use crate::ui::widgets::text;

/// The 8 test swatches. Mid grey and skin tone stand where the mockup had "Warm" and "Amber": the
/// guided exponent check needs grey, and skin tone is the mixed colour that shows hue errors best.
pub const SWATCHES: [(Swatch, &str); 8] = [
    (Swatch::White, "White"),
    (Swatch::MidGrey, "Grey"),
    (Swatch::Red, "Red"),
    (Swatch::Green, "Green"),
    (Swatch::Blue, "Blue"),
    (Swatch::Custom([0, 255, 255]), "Cyan"),
    (Swatch::Custom([255, 0, 255]), "Magenta"),
    (Swatch::Skin, "Skin"),
];

fn c32(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

/// What the LEDs are sent for `req` after calibration, re-encoded for the screen (drive^(1/2.2)),
/// so the thin strip under each swatch shows the correction's effect.
pub fn sent_for_display(req: [u8; 3], cc: &ColorCorrection) -> [u8; 3] {
    let d = Rgb::from_u8(req).to_led(cc);
    let e = |v: f32| v.clamp(0.0, 1.0).powf(1.0 / 2.2);
    Rgb::new(e(d.r), e(d.g), e(d.b)).to_u8()
}

/// Targets for a zone: its devices when it has two or more segments, else the zone, plus every
/// other addressable header (so "Copy to <other>" and matching across headers work).
pub fn targets_for(pc: &PageCtx, zone: &str, others: &[String]) -> Vec<Target> {
    let zc = pc.cfg.rgb.zones.get(zone).cloned().unwrap_or_default();
    let mut v: Vec<Target> = if zc.segments.len() >= 2 {
        (0..zc.segments.len()).map(|i| Target::segment(zone, i)).collect()
    } else {
        vec![Target::zone(zone)]
    };
    for o in others.iter().filter(|o| o.as_str() != zone) {
        v.push(Target::zone(o));
    }
    v
}

/// Ends the test (lighting back) and keeps the edits.
pub fn pause(pc: &mut PageCtx) {
    if pc.ui.calibration.is_some() && !pc.ui.calibration_paused {
        pc.shared.send_rgb(RgbCommand::EndTest);
        pc.ui.calibration_paused = true;
    }
}

fn finish(pc: &mut PageCtx, save: bool) {
    if let Some(s) = pc.ui.calibration.take()
        && save
        && s.is_dirty()
    {
        // Config first (the lighting thread sees the new revision no later than EndTest), so the
        // LEDs never flash the old correction in between.
        let lines = s.commit(&mut pc.cfg.rgb);
        let zones = pc.cfg.rgb.zones.clone();
        pc.shared.edit_config(|c| {
            for (id, zc) in zones {
                c.rgb.zones.insert(id, zc);
            }
        });
        pc.ui.toast(
            ToastKind::Success,
            format!("Colour calibration saved ({} zone{})", lines.len(), if lines.len() == 1 { "" } else { "s" }),
        );
    }
    pc.shared.send_rgb(RgbCommand::EndTest);
    pc.ui.calibration_paused = false;
}

/// The tab body. `zone` = the selected zone id; `names` = (zone id, display title) of the
/// addressable zones; `title` gives a target's label.
pub fn calibration_tab(ui: &mut Ui, pc: &mut PageCtx, zone: &str, names: &[(String, String)]) {
    let p = pal(ui);
    let now = Instant::now();
    let others: Vec<String> = names.iter().map(|(id, _)| id.clone()).collect();
    let label_of = |pc: &PageCtx, t: &Target| -> String {
        let zc = pc.cfg.rgb.zones.get(&t.zone).cloned().unwrap_or_default();
        match t.segment {
            // During a session its own (sanitized) segment table is what the target indexes.
            Some(i) => pc
                .ui
                .calibration
                .as_ref()
                .and_then(|s| s.segment(t))
                .or_else(|| zc.segments.get(i))
                .map(|s| s.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or(format!("Device {}", i + 1)),
            None => {
                names.iter().find(|(id, _)| id == &t.zone).map(|(_, n)| n.clone()).unwrap_or_else(|| t.zone.clone())
            }
        }
    };
    if !names.iter().any(|(id, _)| id == zone) {
        text::para(
            ui,
            "Colour calibration is for the addressable (ARGB) headers. Pick D_LED1 or D_LED2.",
            Ts::Body,
            p.text2,
        );
        return;
    }
    // ---- session for this zone ----
    let session_zone = pc.ui.calibration.as_ref().and_then(|s| s.steps().first().map(|_| s.subject().zone.clone()));
    let first_zone = pc.ui.calibration.as_ref().map(|s| {
        // The reference (first target) is the zone the session was started for.
        match s.steps().first() {
            Some(crate::rgb::calibrate::CalStep::WhiteBalance { target }) => target.zone.clone(),
            _ => session_zone.clone().unwrap_or_default(),
        }
    });
    match (&first_zone, pc.ui.calibration.as_ref().is_some_and(|s| s.is_dirty())) {
        (Some(z), true) if z != zone => {
            let other = names.iter().find(|(id, _)| id == z).map(|(_, n)| n.clone()).unwrap_or_else(|| z.clone());
            crate::ui::widgets::card::banner(
                ui,
                crate::ui::widgets::card::Sev::Info,
                &format!("Unsaved calibration on {other}"),
                "Save it or discard it before calibrating another header.",
                |ui| {
                    if Button::new(Kind::Primary, "Save").size(Size::Sm).show(ui).clicked() {
                        finish(pc, true);
                    }
                    if Button::new(Kind::Ghost, "Discard").size(Size::Sm).show(ui).clicked() {
                        finish(pc, false);
                    }
                },
            );
            return;
        }
        (Some(z), false) if z != zone => {
            finish(pc, false);
        }
        _ => {}
    }
    if pc.ui.calibration.is_none() {
        let targets = targets_for(pc, zone, &others);
        pc.ui.calibration = Some(CalSession::start(&pc.cfg.rgb, &targets, now));
        pc.ui.calibration_sent = 0;
        pc.ui.calibration_paused = false;
    }
    let targets = targets_for(pc, zone, &others);
    let names_t: Vec<(Target, String)> = targets.iter().map(|t| (t.clone(), label_of(pc, t))).collect();
    let name = |t: &Target| names_t.iter().find(|(x, _)| x == t).map(|(_, n)| n.clone()).unwrap_or_default();
    let paused = pc.ui.calibration_paused || pc.ui.calibration.as_ref().is_some_and(|s| s.idle(now));
    let mut end: Option<bool> = None;
    let mut at_limit = false;
    let mut order_change: Option<ColorOrder> = None;
    let mut copied: Option<String> = None;
    let zc_order = pc.cfg.rgb.zones.get(zone).map(|z| z.color_order).unwrap_or(ColorOrder::Auto);
    {
        let Some(s) = pc.ui.calibration.as_mut() else { return };
        let subject = s.subject().clone();
        let reference = targets[0].clone();
        let gains = s.gains(&subject);
        let eff = s.effective_gains(&subject);
        let gamma = s.zone(&subject.zone).map(|c| c.correction.gamma).unwrap_or(2.2);
        let cc_eff = ColorCorrection { gamma, gain: eff };

        // ---- test colours ----
        ui.horizontal(|ui| {
            text::label(ui, "Test colours", Ts::Headline, p.text1);
            ui.add_space(4.0);
            if paused {
                controls::dot_chip(ui, "Paused · your lighting is back", Tone::Neutral);
                if Button::new(Kind::Ghost, "Show on LEDs").size(Size::Sm).icon(Icon::Eye).show(ui).clicked() {
                    s.keep_alive(now);
                    pc.ui.calibration_paused = false;
                }
            } else {
                controls::dot_chip(ui, "Showing on your LEDs now", Tone::Info)
                    .on_hover_text("The test ends after 60 s without input, when you leave this tab, and on Done.");
            }
        });
        ui.add_space(6.0);
        let w = ui.available_width();
        let gap = 8.0;
        let tw = ((w - gap * 7.0) / 8.0).max(40.0);
        let (row, _) = ui.allocate_exact_size(vec2(w, 76.0), Sense::hover());
        for (k, (sw, label)) in SWATCHES.iter().enumerate() {
            let r = Rect::from_min_size(pos2(row.left() + k as f32 * (tw + gap), row.top()), vec2(tw, 76.0));
            let resp = ui.interact(r, Id::new(("swatch", k)), Sense::click());
            let sel = s.swatch() == *sw;
            let fill = if resp.hovered() { p.surface2 } else { p.surface1 };
            ui.painter().rect_filled(r, R_MD, fill);
            ui.painter().rect_stroke(
                r,
                R_MD,
                Stroke::new(
                    if sel { 1.5 } else { 1.0 },
                    if sel {
                        p.accent
                    } else if resp.has_focus() {
                        p.focus
                    } else {
                        p.border_subtle
                    },
                ),
                StrokeKind::Inside,
            );
            let big = Rect::from_min_size(r.min + vec2(6.0, 6.0), vec2(tw - 12.0, 30.0));
            ui.painter().rect_filled(big, R_SM, c32(sw.rgb()));
            ui.painter().rect_stroke(big, R_SM, Stroke::new(1.0, p.border), StrokeKind::Inside);
            let strip = Rect::from_min_size(pos2(big.left(), big.bottom() + 2.0), vec2(big.width(), 8.0));
            ui.painter().rect_filled(strip, 2.0, c32(sent_for_display(sw.rgb(), &cc_eff)));
            ui.painter().text(
                pos2(r.center().x, r.bottom() - 13.0),
                Align2::CENTER_CENTER,
                *label,
                font(Ts::Caption),
                p.text1,
            );
            resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, sel, *label));
            let tip = if Swatch::GUIDED.contains(sw) {
                format!("{} · part of the guided check", sw.label())
            } else {
                format!("{label} · extra check colour")
            };
            let resp = resp.on_hover_text(tip);
            if resp.clicked() {
                s.show(*sw, targets.clone(), now);
                pc.ui.calibration_paused = false;
            }
        }
        text::label(
            ui,
            "Big swatch: the colour you pick · thin strip: what the LEDs are sent after calibration",
            Ts::Caption,
            p.text3,
        );
        ui.add_space(6.0);

        // ---- who is adjusted (several devices / headers) ----
        if targets.len() > 1 {
            ui.horizontal_wrapped(|ui| {
                text::label(ui, "Adjusting", Ts::Label, p.text2);
                for t in &targets {
                    if controls::filter_chip(ui, &name(t), *t == subject).clicked() {
                        if *t == reference {
                            s.set_subject(t.clone(), now);
                        } else {
                            s.compare(reference.clone(), t.clone(), now);
                        }
                    }
                }
            });
            ui.add_space(4.0);
        }

        // ---- the guided question ----
        let comparing = subject != reference;
        let (question, answers): (String, Vec<(&str, Feedback)>) = match s.swatch() {
            Swatch::White if comparing => (
                format!("Compared with {}, {} looks…", name(&reference), name(&subject)),
                vec![
                    ("Too blue", Feedback::TooBlue),
                    ("Too green", Feedback::TooGreen),
                    ("Too pink", Feedback::TooPink),
                    ("Too yellow", Feedback::TooYellow),
                    ("They match", Feedback::LooksRight),
                ],
            ),
            Swatch::White => (
                format!("How does white look on {}?", name(&subject)),
                vec![
                    ("Too blue", Feedback::TooBlue),
                    ("Too green", Feedback::TooGreen),
                    ("Too pink", Feedback::TooPink),
                    ("Too yellow", Feedback::TooYellow),
                    ("Looks white", Feedback::LooksRight),
                ],
            ),
            Swatch::MidGrey => (
                "Compared with the grey swatch, the LEDs look…".to_string(),
                vec![
                    ("Too bright", Feedback::TooBright),
                    ("Too dark", Feedback::TooDark),
                    ("Looks right", Feedback::LooksRight),
                ],
            ),
            _ => ("Does it look like the swatch?".to_string(), vec![("Looks right", Feedback::LooksRight)]),
        };
        ui.horizontal_wrapped(|ui| {
            text::label(ui, &question, Ts::BodyStrong, p.text1);
            ui.add_space(6.0);
            for (label, f) in answers {
                let kind = if f == Feedback::LooksRight { Kind::Primary } else { Kind::Secondary };
                if Button::new(kind, label).show(ui).clicked() {
                    pc.ui.calibration_paused = false;
                    match s.feedback(f, now) {
                        FeedbackResult::AtLimit => at_limit = true,
                        FeedbackResult::Confirmed => {
                            // "Looks white" ends the guide step and moves to the next swatch.
                            let i = SWATCHES.iter().position(|(x, _)| *x == s.swatch()).unwrap_or(0);
                            if i + 1 < SWATCHES.len() {
                                s.show(SWATCHES[i + 1].0, targets.clone(), now);
                            }
                        }
                        FeedbackResult::Adjusted { .. } => {}
                    }
                }
            }
        });
        ui.add_space(10.0);

        // ---- sliders + transfer chart ----
        let chart_w = (ui.available_width() * 0.38).clamp(160.0, 330.0);
        let narrow = ui.available_width() < 560.0;
        ui.horizontal_top(|ui| {
            let left_w = if narrow {
                ui.available_width()
            } else {
                ui.available_width() - chart_w - 16.0 - 2.0 * ui.spacing().item_spacing.x
            };
            ui.vertical(|ui| {
                ui.set_width(left_w);
                text::overline(ui, "White balance");
                let mut g = gains;
                let mut changed = false;
                for (k, ch) in ["Red", "Green", "Blue"].iter().enumerate() {
                    let mut v = g[k] * 100.0;
                    if channel_row(
                        ui,
                        Id::new(("cal_gain", k)),
                        ch,
                        &mut v,
                        *GAIN_RANGE.start() * 100.0..=*GAIN_RANGE.end() * 100.0,
                        1.0,
                        " %",
                        Some(p.channels[k]),
                    ) {
                        g[k] = v / 100.0;
                        changed = true;
                    }
                }
                if changed {
                    s.set_gains(&subject, g, now);
                    pc.ui.calibration_paused = false;
                }
                ui.add_space(6.0);
                text::overline(ui, "Response");
                let mut gm = gamma;
                if channel_row(
                    ui,
                    Id::new("cal_gamma"),
                    "Gamma",
                    &mut gm,
                    *GAMMA_RANGE.start()..=*GAMMA_RANGE.end(),
                    0.05,
                    "",
                    None,
                ) {
                    s.set_gamma(&subject.zone, gm, now);
                    pc.ui.calibration_paused = false;
                }
            });
            if !narrow {
                ui.add_space(16.0);
                transfer_chart(ui, &cc_eff, chart_w);
            }
        });
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            text::label(ui, "Colour order", Ts::Body, p.text2);
            ui.add_space(8.0);
            let mut o = zc_order;
            let opts: Vec<(ColorOrder, &str)> =
                ColorOrder::ALL.iter().map(|c| (*c, if *c == ColorOrder::Auto { "Auto" } else { c.label() })).collect();
            let w = (ui.available_width()).min(7.0 * 58.0);
            if controls::segmented(ui, Id::new(("cal_order", zone)), &mut o, &opts, Some(w))
                .on_hover_text(
                    "Red shows green? The LEDs expect another byte order. Auto uses the controller's calibration.",
                )
                .changed()
            {
                order_change = Some(o);
            }
        });
        ui.add_space(12.0);
        crate::ui::widgets::card::divider(ui);
        ui.add_space(8.0);
        // ---- footer ----
        ui.horizontal(|ui| {
            if Button::new(Kind::Ghost, "Reset")
                .icon(Icon::Undo)
                .show(ui)
                .on_hover_text("Gains back to 100 % and gamma to 2.2 for this device")
                .clicked()
            {
                s.reset(&subject, now);
            }
            if s.can_undo() && Button::new(Kind::Ghost, "Undo").show(ui).clicked() {
                s.undo(now);
            }
            let mut pv = s.preview_effect();
            if ui.checkbox(&mut pv, "Preview on my effect").changed() {
                s.set_preview_effect(pv, now);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                if Button::new(Kind::Primary, "Done")
                    .show(ui)
                    .on_hover_text("Save and bring your lighting back")
                    .clicked()
                {
                    end = Some(true);
                }
                if s.is_dirty() && Button::new(Kind::Ghost, "Discard").show(ui).clicked() {
                    end = Some(false);
                }
                if let Some(other) = targets.iter().find(|t| **t != subject)
                    && Button::new(Kind::Secondary, &format!("Copy to {}", name(other))).show(ui).clicked()
                {
                    s.copy(&subject, other, now);
                    copied = Some(format!("{}'s white balance copied to {}", name(&subject), name(other)));
                }
            });
        });
    }
    if let Some(msg) = copied {
        pc.ui.toast(ToastKind::Info, msg);
    }
    if let Some(o) = order_change {
        pc.cfg.rgb.zones.entry(zone.to_string()).or_default().color_order = o;
    }
    if at_limit {
        pc.ui.toast(
            ToastKind::Info,
            "That is as far as white balance goes; check the colour order if it still looks wrong.",
        );
    }
    match end {
        Some(save) => {
            finish(pc, save);
            pc.prefs.lighting_tab = 0;
        }
        None => {
            if let Some(s) = pc.ui.calibration.as_ref()
                && s.rev() != pc.ui.calibration_sent
                && !pc.ui.calibration_paused
            {
                pc.ui.calibration_sent = s.rev();
                pc.shared.send_rgb(RgbCommand::TestColor(s.pattern()));
            }
            // Notice the 60 s idle pause without a busy loop.
            ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
        }
    }
}

/// Label · tinted slider · value box.
#[allow(clippy::too_many_arguments)]
fn channel_row(
    ui: &mut Ui,
    id: Id,
    label: &str,
    v: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    step: f32,
    suffix: &str,
    color: Option<Color32>,
) -> bool {
    let p = pal(ui);
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        let (r, _) = ui.allocate_exact_size(vec2(84.0, 26.0), Sense::hover());
        ui.painter().text(pos2(r.left(), r.center().y), Align2::LEFT_CENTER, label, font(Ts::Body), p.text2);
        let sw = (ui.available_width() - 84.0).max(80.0);
        changed |= controls::slider_tinted(ui, id, v, range.clone(), step, sw, color).changed();
        let dv = egui::DragValue::new(v)
            .range(range)
            .speed(step)
            .fixed_decimals(if step < 1.0 { 1 } else { 0 })
            .suffix(suffix);
        changed |= ui.add_sized(vec2(70.0, 26.0), dv).changed();
    });
    changed
}

/// "Sent vs requested": `sent = gain · requested^gamma` per channel (from `Rgb::to_led`), against
/// the y = x reference.
fn transfer_chart(ui: &mut Ui, cc: &ColorCorrection, w: f32) {
    let p = pal(ui);
    let (r, _) = ui.allocate_exact_size(vec2(w, w * 0.9), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(r, R_MD, p.inset);
    painter.text(r.min + vec2(12.0, 10.0), Align2::LEFT_TOP, "Sent vs requested", font(Ts::Caption), p.text2);
    let plot = Rect::from_min_max(r.min + vec2(18.0, 32.0), r.max - vec2(14.0, 14.0));
    let at = |x: f32, y: f32| pos2(plot.left() + x * plot.width(), plot.bottom() - y * plot.height());
    painter.line_segment([at(0.0, 0.0), at(1.0, 1.0)], Stroke::new(1.0, p.text3));
    for ch in 0..3 {
        let pts: Vec<_> = (0..=40)
            .map(|i| {
                let x = i as f32 / 40.0;
                let d = Rgb::new(x, x, x).to_led(cc);
                at(x, [d.r, d.g, d.b][ch])
            })
            .collect();
        painter.add(Shape::line(pts, Stroke::new(1.6, p.channels[ch])));
    }
    painter.text(
        at(1.0, 0.0) + vec2(0.0, 2.0),
        Align2::RIGHT_TOP,
        format!("γ {:.2}", cc.gamma),
        tab(Ts::Caption),
        p.text3,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_shows_the_correction() {
        let neutral = ColorCorrection { gamma: 2.2, gain: [1.0; 3] };
        // Gamma 2.2 then re-encoded with 1/2.2: unchanged on screen.
        let w = sent_for_display([255, 255, 255], &neutral);
        assert_eq!(w, [255, 255, 255]);
        let warm = ColorCorrection { gamma: 2.2, gain: [1.0, 0.82, 0.74] };
        let s = sent_for_display([255, 255, 255], &warm);
        assert!(s[0] > s[1] && s[1] > s[2], "{s:?}");
        let g = sent_for_display([128, 128, 128], &neutral);
        assert!((g[0] as i32 - 128).abs() <= 2);
    }

    #[test]
    fn eight_distinct_swatches_starting_with_white() {
        assert_eq!(SWATCHES[0].0, Swatch::White);
        for (i, a) in SWATCHES.iter().enumerate() {
            for b in &SWATCHES[i + 1..] {
                assert_ne!(a.0.rgb(), b.0.rgb());
            }
        }
    }
}
