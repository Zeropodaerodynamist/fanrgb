//! Lighting: "What do my LEDs look like and how do I change them?"

use std::collections::HashMap;

use egui::{Id, Rect, Sense, Ui, pos2, vec2};

use super::{PageCtx, board_for, fan_nodes, handle_card, led_nodes};
use crate::config::{Effect, EffectKind, ExitLighting, ZoneConfig};
use crate::error::ErrorCode;
use crate::model::SensorKind;
use crate::rgb::{AudioState, RgbCommand, ZoneDesc};
use crate::ui::assets::Thumb;
use crate::ui::fonts::{Ts, font};
use crate::ui::icons::{self, Icon};
use crate::ui::layout::{self, Breakpoint};
use crate::ui::overlays::ToastKind;
use crate::ui::theme::{R_MD, R_PILL, pal};
use crate::ui::widgets::board::{BoardMode, BoardView};
use crate::ui::widgets::card::{Sev, banner, card, divider, header};
use crate::ui::widgets::color::palette_row;
use crate::ui::widgets::controls::{self, Button, Kind, Size, Tone, segmented, slider_row};
use crate::ui::widgets::gallery::{gallery, short_label};
use crate::ui::widgets::issue_card::issue_card;
use crate::ui::widgets::led::led_strip;
use crate::ui::widgets::{calibration, text, thumb};

fn zones(pc: &PageCtx) -> Vec<ZoneDesc> {
    let all = if pc.rgb.zones.is_empty() { crate::rgb::standard_zones() } else { pc.rgb.zones.clone() };
    all.into_iter().filter(|z| !z.advanced || pc.cfg.rgb.show_all_zones).collect()
}

fn zone_title(z: &ZoneDesc, zc: &ZoneConfig) -> String {
    zc.name.clone().unwrap_or_else(|| z.name.split(" · ").next().unwrap_or(&z.name).to_string())
}

fn zone_kind(z: &ZoneDesc) -> &'static str {
    if z.addressable {
        "ARGB 5 V"
    } else if z.id == "ledc" {
        "RGB 12 V"
    } else {
        "Onboard"
    }
}

pub fn show(ui: &mut Ui, pc: &mut PageCtx) {
    let gap = pc.bp.gap();
    ui.add_space(4.0);
    status_banners(ui, pc);
    control_row(ui, pc);
    ui.add_space(gap * 0.75);
    let zs = zones(pc);
    if pc.ui.selected_zone.as_ref().is_none_or(|id| !zs.iter().any(|z| &z.id == id)) {
        pc.ui.selected_zone = zs.first().map(|z| z.id.clone());
    }
    // The test colours show only while the Colour calibration tab is open.
    if pc.prefs.lighting_tab != 1 {
        calibration::pause(pc);
    }
    if pc.bp == Breakpoint::Wide {
        let avail = ui.available_width();
        let left = 360.0;
        layout::row(ui, Id::new("light_split"), &[left, avail - left - gap], gap, |i, ui, _| {
            if i == 0 {
                master(ui, pc, &zs);
            } else {
                inspector(ui, pc, &zs);
            }
            0.0
        });
    } else {
        zone_chips(ui, pc, &zs);
        ui.add_space(gap * 0.75);
        inspector(ui, pc, &zs);
    }
    ui.add_space(gap);
    footer_row(ui, pc);
}

/// Multi-line controller details (hover on the status chip, Diagnostics).
pub fn controller_details(rgb: &crate::rgb::RgbStatus) -> String {
    let mut lines = Vec::new();
    match &rgb.device {
        Some(d) => {
            lines.push(format!("{} · USB 048D:{:04X} · firmware {}", d.name, d.pid, d.fw));
            lines.push(format!("Chip {:08X} · feature flags {:02X} · {}", d.chip_id, d.support_flags, d.calibration));
            lines.push(format!(
                "Keep settings through sleep: {}",
                if d.persistence { "supported" } else { "not supported" }
            ));
            lines.push(format!(
                "Firmware LampArray switch (0x48): {}",
                d.dynamic_lighting_raw
                    .map(|b| crate::rgb::dynlight::FirmwareDl::from_raw(b).label())
                    .unwrap_or_else(|| "not reported".into())
            ));
        }
        None => {
            lines.push(format!("Controller {}", rgb.state.label()));
            if let Some(t) = rgb.next_retry {
                let secs = t.saturating_duration_since(std::time::Instant::now()).as_secs();
                lines.push(format!("Next attempt in {secs} s (attempt {})", rgb.attempts + 1));
            }
        }
    }
    lines.push(format!("Windows {}", rgb.dynamic_lighting.summary()));
    if rgb.simulated {
        lines.push("Simulated controller (demo): the real device is not used".into());
    }
    lines.push(format!(
        "{:.0} fps (cap {:.0}) · {} packets/frame · {:.1} ms/packet",
        rgb.fps, rgb.fps_cap, rgb.packets_per_frame, rgb.packet_ms
    ));
    if !rgb.ledc_slots.is_empty() {
        let slots: Vec<String> = rgb.ledc_slots.iter().map(|s| s.to_string()).collect();
        lines.push(format!("LED_C driven on division {}", slots.join(" + ")));
    }
    if rgb.reconnects > 0 || rgb.write_errors > 0 {
        lines.push(format!("{} reconnects · {} write errors", rgb.reconnects, rgb.write_errors));
    }
    if rgb.hung_io_threads > 0 {
        lines.push(format!("{} request(s) stuck in the USB driver", rgb.hung_io_threads));
    }
    if rgb.suspended {
        lines.push("Handed over to the controller (sleep / exit lighting)".into());
    }
    if let Some(e) = &rgb.last_error {
        lines.push(format!("Last error: {}", e.message()));
    }
    lines.join("\n")
}

fn status_banners(ui: &mut Ui, pc: &mut PageCtx) {
    let gap = pc.bp.gap();
    // Dynamic Lighting: only when Windows can actually paint these LEDs (registry), or took them
    // back after "Let FanRGB control". The firmware's 0x48 byte alone is no reason (it reads
    // "armed" on this board while Windows has the device off).
    if let Some((title, body)) = pc.rgb.dynamic_lighting.banner() {
        banner(ui, Sev::Warning, &title, &body, |ui| {
            if Button::new(Kind::Primary, "Let FanRGB control these LEDs").size(Size::Sm).show(ui).clicked() {
                pc.shared.send_rgb(RgbCommand::ClaimDynamicLighting);
                pc.ui.toast(ToastKind::Info, "Switching Dynamic Lighting off for the RGB controller…");
            }
            if Button::new(Kind::Ghost, "Open Windows settings")
                .size(Size::Sm)
                .icon(Icon::ExternalLink)
                .show(ui)
                .clicked()
            {
                let _ = crate::util::win::open_url(crate::rgb::dynlight::SETTINGS_URI);
            }
        });
        ui.add_space(gap * 0.75);
    }
    let list: Vec<_> = pc
        .ui
        .issues
        .visible()
        .filter(|i| {
            (400..500).contains(&i.code.number())
                && i.code != ErrorCode::AudioCapture
                && i.code != ErrorCode::DynamicLightingActive
        })
        .cloned()
        .collect();
    for i in list {
        if i.code == ErrorCode::RgbNotFound {
            // Info: inline note, not an alarm.
            banner(
                ui,
                Sev::Info,
                "No RGB Fusion controller found",
                &format!("Changes are previewed and apply when it's connected ({}).", i.code.id()),
                |ui| {
                    if Button::new(Kind::Secondary, "Reconnect")
                        .size(Size::Sm)
                        .icon(Icon::Refresh)
                        .loading(pc.ui.reconnecting())
                        .show(ui)
                        .clicked()
                    {
                        pc.ui
                            .pending
                            .push(crate::ui::actions::UiAction::Fix(crate::error::FixAction::ReconnectRgb, None));
                    }
                },
            );
        } else {
            let r = issue_card(ui, &i, true, pc.ui.reconnecting());
            handle_card(ui, pc, &i, r);
        }
        ui.add_space(gap * 0.75);
    }
}

fn hw_effect_active(pc: &PageCtx) -> bool {
    if pc.cfg.rgb.sync {
        pc.cfg.rgb.global.kind.is_hardware()
    } else {
        pc.cfg.rgb.zones.values().any(|z| z.effect.kind.is_hardware())
    }
}

fn control_row(ui: &mut Ui, pc: &mut PageCtx) {
    let p = pal(ui);
    card(ui, |ui| {
        // The status chips go on the same line when they fit after the controls, else below.
        let need = status_chips_width(ui, pc);
        let mut one_line = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            controls::toggle(ui, &mut pc.cfg.rgb.enabled);
            text::label(ui, "Lighting", Ts::Headline, p.text1);
            ui.add_space(8.0);
            text::label(ui, "Brightness", Ts::Body, p.text2);
            let mut b = pc.cfg.rgb.brightness * 100.0;
            let w = if pc.bp == Breakpoint::Narrow { 110.0 } else { 120.0 };
            if controls::slider(ui, Id::new("rgb_bright"), &mut b, 0.0..=100.0, 1.0, w).changed() {
                pc.cfg.rgb.brightness = b / 100.0;
            }
            text::label(ui, format!("{b:.0} %"), Ts::BodyStrong, p.text1);
            ui.add_space(8.0);
            controls::toggle(ui, &mut pc.cfg.rgb.sync);
            text::label(ui, "Same effect everywhere", Ts::Body, p.text1);
            one_line = ui.available_width() >= need + 12.0;
            if one_line {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| status_chips(ui, pc));
            }
        });
        if !one_line {
            ui.add_space(6.0);
            // A horizontal first, so the right-to-left row is one line high (not the whole card).
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| status_chips(ui, pc));
            });
        }
    });
}

/// Width of [`status_chips`] (measured, so the control row can decide on one or two lines).
fn status_chips_width(ui: &Ui, pc: &PageCtx) -> f32 {
    let w = |s: &str| ui.painter().layout_no_wrap(s.to_string(), font(Ts::Caption), egui::Color32::WHITE).size().x;
    let dl = &pc.rgb.dynamic_lighting;
    let dl_label = if dl.conflict() { "Dynamic Lighting is controlling LEDs" } else { dl.state.label() };
    let ctrl = match &pc.rgb.device {
        Some(d) => format!("{} (demo) · 30 fps", d.name.split(['-', ' ']).next().unwrap_or("Controller")),
        None => format!("Controller {}", pc.rgb.state.label()),
    };
    let mut total = w(dl_label) + 34.0 + w(&ctrl) + 26.0 + 10.0;
    if pc.rgb.device.as_ref().is_some_and(|d| d.persistence) && hw_effect_active(pc) {
        total += 190.0;
    }
    total
}

/// Controller and Dynamic Lighting chips (+ "Save to controller" for onboard effects).
fn status_chips(ui: &mut Ui, pc: &mut PageCtx) {
    let persist = pc.rgb.device.as_ref().is_some_and(|d| d.persistence);
    let details = controller_details(pc.rgb);
    let _ = match &pc.rgb.device {
        Some(_) if pc.rgb.suspended => controls::dot_chip(ui, "Handed over to the controller", Tone::Neutral),
        Some(d) => {
            // Short form ("IT5702 · 30 fps"); the full name and firmware are in the tooltip.
            let chip = d.name.split(['-', ' ']).next().filter(|s| !s.is_empty()).unwrap_or("Controller");
            let demo = if pc.rgb.simulated { " (demo)" } else { "" };
            controls::dot_chip(ui, &format!("{chip}{demo} · {:.0} fps", pc.rgb.fps), Tone::Success)
        }
        None => controls::dot_chip(
            ui,
            &format!("Controller {}", pc.rgb.state.label()),
            if pc.ui.issues.has(ErrorCode::RgbHung) { Tone::Danger } else { Tone::Neutral },
        ),
    }
    .on_hover_text(details);
    // Dynamic Lighting chip: neutral when off, warning in conflict, muted when
    // not available. Click opens the Windows page.
    let dl = &pc.rgb.dynamic_lighting;
    let (tone, label) = if dl.conflict() {
        (Tone::Warning, "Dynamic Lighting is controlling LEDs")
    } else {
        (Tone::Neutral, dl.state.label())
    };
    if dl_chip(ui, label, tone, dl.state == crate::rgb::dynlight::DlState::Unavailable)
        .on_hover_text(dl.summary())
        .clicked()
    {
        pc.ui.pending.push(crate::ui::actions::UiAction::OpenDynamicLightingSettings);
    }
    if persist && hw_effect_active(pc) {
        if pc.rgb.saved_to_flash {
            controls::dot_chip(ui, "Saved", Tone::Success);
        }
        if Button::new(Kind::Secondary, "Save to controller")
            .size(Size::Sm)
            .icon(Icon::Download)
            .show(ui)
            .on_hover_text("Keep this onboard effect through sleep and power-off")
            .clicked()
        {
            pc.shared.send_rgb(RgbCommand::SaveToFlash);
            pc.ui.toast(ToastKind::Info, "Saving to the controller…");
        }
    }
}

/// Chip with the Windows glyph (Dynamic Lighting status); clickable.
fn dl_chip(ui: &mut Ui, label: &str, tone: Tone, muted: bool) -> egui::Response {
    let p = pal(ui);
    let (fill, fg) = controls::tone_colors(p, tone);
    let fg = if muted { p.text3 } else { fg };
    let g = ui.painter().layout_no_wrap(label.to_string(), font(Ts::Caption), fg);
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 34.0, 24.0), Sense::click());
    let fill = if resp.hovered() { crate::ui::theme::mix(fill, p.surface3, 0.5) } else { fill };
    ui.painter().rect_filled(rect, R_PILL, fill);
    icons::paint(
        ui.painter(),
        Rect::from_center_size(pos2(rect.left() + 14.0, rect.center().y), vec2(12.0, 12.0)),
        Icon::Windows,
        fg,
    );
    ui.painter().galley(pos2(rect.left() + 26.0, rect.center().y - g.size().y / 2.0), g, fg);
    resp
}

fn zone_thumb(z: &ZoneDesc, zc: &ZoneConfig) -> Thumb {
    let n = zc.name.clone().unwrap_or_default().to_lowercase();
    if n.contains("pump") || n.contains("aio") {
        Thumb::Pump
    } else if n.contains("fan") {
        Thumb::Fan140
    } else if !z.addressable {
        Thumb::Strip
    } else if z.id == "dled2" {
        // Default wiring (the hero uses the same): pump head on D_LED2.
        Thumb::Pump
    } else {
        Thumb::Fan140
    }
}

fn master(ui: &mut Ui, pc: &mut PageCtx, zs: &[ZoneDesc]) {
    let p = pal(ui);
    card(ui, |ui| {
        header(ui, "Zones", None, |ui| {
            text::label(
                ui,
                format!("{} header{}", zs.len(), if zs.len() == 1 { "" } else { "s" }),
                Ts::Caption,
                p.text2,
            );
            if pc.rgb.device.is_none() {
                controls::chip(ui, "Preview only", Tone::Neutral);
            }
        });
        ui.add_space(4.0);
        if pc.cfg.rgb.sync {
            text::label(ui, "All zones show the same effect", Ts::Caption, p.text2);
            ui.add_space(4.0);
        }
        for z in zs {
            zone_row(ui, pc, z);
            ui.add_space(4.0);
        }
        ui.add_space(8.0);
        dl_card(ui, pc);
    });
}

/// Windows Dynamic Lighting status at the bottom of the zone list (the conflict banner is above).
fn dl_card(ui: &mut Ui, pc: &mut PageCtx) {
    use crate::rgb::dynlight::DlState;
    let p = pal(ui);
    let dl = &pc.rgb.dynamic_lighting;
    let body = match dl.state {
        _ if dl.conflict() => "Windows can paint these LEDs too, so colours may flicker.",
        DlState::Controlling => "Windows can paint these LEDs too, so colours may flicker.",
        DlState::Off => "Off for these headers, so FanRGB is in control.",
        DlState::Unavailable => "Not available on this Windows (or no Dynamic Lighting device).",
        DlState::Unknown => "Checking…",
        DlState::UserUnknown => "Can't read the setting of the signed-in Windows user.",
    };
    egui::Frame::new().fill(p.surface2).corner_radius(R_MD).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
            icons::paint(ui.painter(), r, Icon::Windows, if dl.conflict() { p.warning } else { p.text2 });
            text::label(ui, "Windows Dynamic Lighting", Ts::BodyStrong, p.text1);
        });
        text::para(ui, body, Ts::Caption, p.text2).on_hover_text(dl.summary());
        if controls::link(ui, "Open Windows settings").clicked() {
            pc.ui.pending.push(crate::ui::actions::UiAction::OpenDynamicLightingSettings);
        }
    });
}

fn zone_row(ui: &mut Ui, pc: &mut PageCtx, z: &ZoneDesc) {
    let p = pal(ui);
    let mut zc = pc.cfg.rgb.zones.get(&z.id).cloned().unwrap_or_default();
    let before = zc.clone();
    let selected =
        (!pc.cfg.rgb.sync || pc.prefs.lighting_tab != 0) && pc.ui.selected_zone.as_deref() == Some(z.id.as_str());
    let top = ui.cursor().min;
    let where_ = ui.painter().add(egui::Shape::Noop);
    let preview = pc.rgb.preview.get(&z.id).cloned().unwrap_or_default();
    let lit = pc.cfg.rgb.enabled && zc.enabled;
    let inner = ui.vertical(|ui| {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            let (tr, _) = ui.allocate_exact_size(vec2(48.0, 48.0), Sense::hover());
            let ctx = ui.ctx().clone();
            let light = (lit && !preview.is_empty()).then(|| thumb::avg(&preview));
            thumb::paint(&ctx, ui.painter(), pc.assets, p, tr, zone_thumb(z, &zc), light, false);
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                text::label(ui, zone_title(z, &zc), Ts::BodyStrong, p.text1);
                let label = z.name.split(" · ").next().unwrap_or("");
                let mut sub = format!("{label} · {}", zone_kind(z));
                if z.addressable {
                    sub.push_str(&format!(" · {} LEDs", zc.led_count));
                }
                text::label(ui, sub, Ts::Caption, p.text2);
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                ui.add_space(8.0);
                controls::toggle(ui, &mut zc.enabled).on_hover_text("Zone on / off");
            });
        });
        if z.addressable || lit {
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.scope(|ui| {
                    ui.set_max_width(ui.available_width() - 8.0);
                    led_strip(ui, Id::new(("strip", &z.id)), &preview, 20.0, z.addressable, pc.prefs.diffused_leds);
                });
            });
        }
        if z.id == "ledc" {
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                icons::paint(ui.painter(), r, Icon::Warning, p.warning);
                text::label(ui, "12 V header: never connect 5 V ARGB here", Ts::Caption, p.warning);
            });
        }
        ui.add_space(8.0);
    });
    let rect = Rect::from_min_max(top, pos2(top.x + ui.available_width(), inner.response.rect.bottom()));
    let resp = ui.interact(rect, Id::new(("zone_row", &z.id)), Sense::click());
    let (fill, stroke) = if selected {
        (p.accent_subtle, Some(p.accent))
    } else if resp.hovered() {
        (p.surface2, None)
    } else {
        (egui::Color32::TRANSPARENT, None)
    };
    ui.painter().set(where_, egui::Shape::rect_filled(rect, R_MD, fill));
    if let Some(s) = stroke {
        ui.painter().rect_stroke(rect, R_MD, egui::Stroke::new(1.5, s), egui::StrokeKind::Inside);
    }
    let name = zone_title(z, &zc);
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, &name));
    if resp.clicked() {
        if pc.cfg.rgb.sync && pc.prefs.lighting_tab == 0 {
            pc.ui.toast(ToastKind::Info, "Turn off “Same effect everywhere” to give zones their own effect");
        }
        pc.ui.selected_zone = Some(z.id.clone());
    }
    if zc != before {
        pc.cfg.rgb.zones.insert(z.id.clone(), zc);
    }
}

fn zone_chips(ui: &mut Ui, pc: &mut PageCtx, zs: &[ZoneDesc]) {
    let p = pal(ui);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
        if controls::filter_chip(ui, "All zones", pc.cfg.rgb.sync).clicked() {
            pc.cfg.rgb.sync = true;
        }
        for z in zs {
            let zc = pc.cfg.rgb.zones.get(&z.id).cloned().unwrap_or_default();
            let sel = !pc.cfg.rgb.sync && pc.ui.selected_zone.as_deref() == Some(z.id.as_str());
            if controls::filter_chip(ui, &zone_title(z, &zc), sel).clicked() {
                pc.cfg.rgb.sync = false;
                pc.ui.selected_zone = Some(z.id.clone());
            }
        }
    });
    ui.add_space(6.0);
    let target =
        if pc.cfg.rgb.sync { zs.first() } else { zs.iter().find(|z| Some(&z.id) == pc.ui.selected_zone.as_ref()) };
    if let Some(z) = target {
        let preview = pc.rgb.preview.get(&z.id).cloned().unwrap_or_default();
        led_strip(ui, Id::new(("sel_strip", &z.id)), &preview, 40.0, z.addressable, pc.prefs.diffused_leds);
    }
    let _ = p;
}

/// Zone setup tab: name, LED count, Identify, LED_C slot, devices on the header, where it is.
fn zone_setup(ui: &mut Ui, pc: &mut PageCtx, zs: &[ZoneDesc]) {
    let p = pal(ui);
    let zid = pc.ui.selected_zone.clone().or_else(|| zs.first().map(|z| z.id.clone()));
    let Some(z) = zs.iter().find(|z| Some(&z.id) == zid.as_ref()).cloned() else { return };
    let mut zc = pc.cfg.rgb.zones.get(&z.id).cloned().unwrap_or_default();
    let before = zc.clone();
    ui.horizontal(|ui| {
        text::label(ui, "Zone", Ts::Label, p.text2);
        egui::ComboBox::from_id_salt("setup_zone").selected_text(zone_title(&z, &zc)).show_ui(ui, |ui| {
            for o in zs {
                let oc = pc.cfg.rgb.zones.get(&o.id).cloned().unwrap_or_default();
                if ui.selectable_label(o.id == z.id, zone_title(o, &oc)).clicked() {
                    pc.ui.selected_zone = Some(o.id.clone());
                }
            }
        });
        if Button::new(Kind::Secondary, "Identify")
            .size(Size::Sm)
            .icon(Icon::Eye)
            .show(ui)
            .on_hover_text("Flash this header white for 3 seconds")
            .clicked()
        {
            pc.shared.send_rgb(RgbCommand::Identify(z.id.clone()));
        }
    });
    if z.id == "ledc" {
        ui.add_space(6.0);
        banner(
            ui,
            Sev::Warning,
            "12 V header",
            "Connecting a 5 V ARGB strip here will damage it. Use D_LED1 / D_LED2 (3-pin) for addressable strips.",
            |_| {},
        );
        ui.add_space(4.0);
        super::settings::ledc_slot_row(ui, pc);
    }
    ui.add_space(6.0);
    let mut name = zc.name.clone().unwrap_or_default();
    ui.horizontal(|ui| {
        text::label(ui, "Name", Ts::Label, p.text2);
        if ui
            .add(
                egui::TextEdit::singleline(&mut name)
                    .hint_text(z.name.split(" · ").next().unwrap_or(""))
                    .desired_width(180.0),
            )
            .changed()
        {
            zc.name = if name.trim().is_empty() { None } else { Some(name.trim().to_string()) };
        }
    });
    // While a colour calibration of this zone is open, its own copy of the segments is what the
    // LEDs show; editing here would be overwritten on Done.
    let calibrating = pc.ui.calibration.as_ref().is_some_and(|s| s.zone(&z.id).is_some());
    if z.addressable {
        ui.horizontal(|ui| {
            text::label(ui, "LEDs", Ts::Label, p.text2);
            ui.add(egui::DragValue::new(&mut zc.led_count).range(1..=z.max_leds as u32).speed(0.3));
        });
        ui.add_enabled_ui(!calibrating, |ui| segments_editor(ui, &mut zc, z.max_leds));
        if calibrating {
            text::caption3(ui, "Finish or discard the colour calibration to edit the devices.");
        }
    }
    text::caption3(ui, "Colour order and white balance are in the Colour calibration tab.");
    if zc != before {
        pc.cfg.rgb.zones.insert(z.id.clone(), zc);
    }
    // Where the header is on the board.
    ui.add_space(10.0);
    let leds = led_nodes(pc.cfg, pc.rgb);
    let fans = fan_nodes(pc.st, false);
    let map = board_for(pc.st);
    let texture = map.and_then(|_| pc.assets.board(ui.ctx()));
    if let Some(id) = (BoardView {
        id: Id::new("light_board"),
        map,
        mode: BoardMode::Lighting,
        fans: &fans,
        leds: &leds,
        selected: Some(z.id.as_str()),
        show_ghosts: false,
        preview_only: pc.rgb.device.is_none(),
        texture,
    })
    .show(ui)
    {
        pc.ui.selected_zone = Some(id);
    }
}

/// "Devices on this header": LED ranges of chained devices (fans, then the pump head), each with
/// its own white balance (set by "Match colours").
fn segments_editor(ui: &mut Ui, zc: &mut ZoneConfig, max_leds: u16) {
    let p = pal(ui);
    ui.add_space(6.0);
    text::label(ui, "Devices on this header", Ts::BodyStrong, p.text1);
    text::caption3(
        ui,
        "Several devices chained on one header (case fans, then the AIO pump head)? Give each its LEDs so \
         \u{201c}Match colours\u{201d} can balance them against each other. LEDs count from 1 at the header.",
    );
    let mut remove = None;
    for (i, s) in zc.segments.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut s.name).hint_text("Name").desired_width(120.0));
            text::label(ui, "LEDs", Ts::Label, p.text2);
            let mut first = s.from_led as u32 + 1;
            let mut last = s.to_led as u32;
            ui.add(egui::DragValue::new(&mut first).range(1..=max_leds as u32).speed(0.3));
            text::label(ui, "to", Ts::Label, p.text2);
            ui.add(egui::DragValue::new(&mut last).range(first..=max_leds as u32).speed(0.3));
            s.from_led = (first - 1) as u16;
            s.to_led = last.max(first) as u16;
            if s.is_active() {
                text::label(
                    ui,
                    format!("gain {:.0}/{:.0}/{:.0} %", s.gain[0] * 100.0, s.gain[1] * 100.0, s.gain[2] * 100.0),
                    Ts::Caption,
                    p.text3,
                );
            }
            if Button::new(Kind::Ghost, "").size(Size::Sm).icon(Icon::Trash).show(ui).on_hover_text("Remove").clicked()
            {
                remove = Some(i);
            }
        });
    }
    if let Some(i) = remove {
        zc.segments.remove(i);
    }
    if zc.segments.len() < crate::config::MAX_SEGMENTS
        && Button::new(Kind::Secondary, "Add device").size(Size::Sm).icon(Icon::Plus).show(ui).clicked()
    {
        let from = zc.segments.iter().map(|s| s.to_led).max().unwrap_or(0).min(zc.led_count.saturating_sub(1));
        let n = zc.segments.len() + 1;
        zc.segments.push(crate::config::Segment {
            name: if n == 1 { "Fans".into() } else { format!("Device {n}") },
            from_led: from,
            to_led: zc.led_count.max(from + 1),
            ..Default::default()
        });
    }
}

const TABS: [&str; 3] = ["Effect", "Colour calibration", "Zone setup"];

/// Underlined tab bar; one focus stop, ←/→ switch.
fn tab_bar(ui: &mut Ui, tab: &mut u8, right: &str) {
    let p = pal(ui);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 40.0), Sense::hover());
    let id = Id::new("lighting_tabs");
    let resp = ui.interact(rect, id, Sense::click());
    let galleys: Vec<_> =
        TABS.iter().map(|t| ui.painter().layout_no_wrap(t.to_string(), font(Ts::Headline), p.text1)).collect();
    let mut x = rect.left();
    let mut spans = Vec::new();
    for g in &galleys {
        spans.push((x, x + g.size().x));
        x += g.size().x + 30.0;
    }
    if resp.clicked()
        && let Some(pos) = resp.interact_pointer_pos()
        && let Some(i) = spans.iter().position(|(a, b)| pos.x >= *a - 12.0 && pos.x <= *b + 12.0)
    {
        *tab = i as u8;
    }
    if resp.has_focus() {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(id, egui::EventFilter { horizontal_arrows: true, ..Default::default() })
        });
        if ui.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
            *tab = (*tab + 1).min(TABS.len() as u8 - 1);
        }
        if ui.input(|i| i.key_pressed(egui::Key::ArrowLeft)) {
            *tab = tab.saturating_sub(1);
        }
    }
    let hover =
        resp.hover_pos().and_then(|pos| spans.iter().position(|(a, b)| pos.x >= *a - 12.0 && pos.x <= *b + 12.0));
    let painter = ui.painter();
    painter.line_segment([rect.left_bottom(), rect.right_bottom()], egui::Stroke::new(1.0, p.border_subtle));
    for (i, g) in galleys.into_iter().enumerate() {
        let sel = i as u8 == *tab;
        let c = if sel || hover == Some(i) { p.text1 } else { p.text2 };
        let (a, b) = spans[i];
        painter.galley(pos2(a, rect.center().y - g.size().y / 2.0 - 2.0), g, c);
        if sel {
            painter.rect_filled(
                Rect::from_min_max(pos2(a, rect.bottom() - 2.5), pos2(b, rect.bottom())),
                1.0,
                p.accent,
            );
        }
    }
    if resp.has_focus() {
        let (a, b) = spans[*tab as usize];
        painter.rect_stroke(
            Rect::from_min_max(pos2(a - 6.0, rect.top() + 4.0), pos2(b + 6.0, rect.bottom() - 4.0)),
            R_MD,
            egui::Stroke::new(2.0, p.focus),
            egui::StrokeKind::Outside,
        );
    }
    if spans.last().is_some_and(|(_, b)| rect.right() - b > 120.0) {
        painter.text(
            pos2(rect.right(), rect.center().y),
            egui::Align2::RIGHT_CENTER,
            right,
            font(Ts::Caption),
            p.text2,
        );
    }
    let label = format!("Lighting tab: {}", TABS[*tab as usize]);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::RadioGroup, true, &label));
}

fn inspector(ui: &mut Ui, pc: &mut PageCtx, zs: &[ZoneDesc]) {
    let sync = pc.cfg.rgb.sync;
    let zid = pc.ui.selected_zone.clone().or_else(|| zs.first().map(|z| z.id.clone()));
    let mut tab = pc.prefs.lighting_tab.min(2);
    let right = zid
        .as_ref()
        .and_then(|id| zs.iter().find(|z| &z.id == id))
        .map(|z| {
            let title = zone_title(z, &pc.cfg.rgb.zones.get(&z.id).cloned().unwrap_or_default());
            let label = z.name.split(" · ").next().unwrap_or("").to_string();
            if tab == 0 && sync {
                "All zones".to_string()
            } else if title == label {
                title
            } else {
                format!("{title} · {label}")
            }
        })
        .unwrap_or_default();
    card(ui, |ui| {
        tab_bar(ui, &mut tab, &right);
        ui.add_space(10.0);
        match tab {
            0 => effect_tab(ui, pc, zs),
            1 => {
                let names: Vec<(String, String)> = zs
                    .iter()
                    .filter(|z| z.addressable)
                    .map(|z| (z.id.clone(), zone_title(z, &pc.cfg.rgb.zones.get(&z.id).cloned().unwrap_or_default())))
                    .collect();
                let zone = zid
                    .clone()
                    .filter(|id| names.iter().any(|(n, _)| n == id))
                    .or_else(|| names.first().map(|n| n.0.clone()));
                match zone {
                    Some(z) => calibration::calibration_tab(ui, pc, &z, &names),
                    None => {
                        let c = pal(ui).text2;
                        text::para(ui, "No addressable (ARGB) header to calibrate.", Ts::Body, c);
                    }
                };
            }
            _ => zone_setup(ui, pc, zs),
        }
    });
    if tab != pc.prefs.lighting_tab {
        pc.prefs.lighting_tab = tab;
    }
}

fn effect_tab(ui: &mut Ui, pc: &mut PageCtx, zs: &[ZoneDesc]) {
    let p = pal(ui);
    let sync = pc.cfg.rgb.sync;
    let zid = pc.ui.selected_zone.clone();
    let mut effect: Effect = if sync {
        pc.cfg.rgb.global.clone()
    } else {
        zid.as_ref().and_then(|id| pc.cfg.rgb.zones.get(id)).map(|z| z.effect.clone()).unwrap_or_default()
    };
    let before = effect.clone();
    if sync {
        text::label(ui, "Same effect on every zone", Ts::Caption, p.text3);
    } else if let Some(z) = zs.iter().find(|z| Some(&z.id) == zid.as_ref()) {
        text::label(
            ui,
            format!("Effect for {}", zone_title(z, &pc.cfg.rgb.zones.get(&z.id).cloned().unwrap_or_default())),
            Ts::Caption,
            p.text3,
        );
    }
    ui.add_space(2.0);
    let sensors: HashMap<String, f32> =
        pc.st.sensors.iter().filter_map(|s| s.value.map(|v| (s.desc.id.clone(), v))).collect();
    let animate = pc.focused && *pc.page == crate::ui::Page::Lighting;
    if let Some(k) = gallery(ui, Id::new("gallery"), effect.kind, &effect, &sensors, sync, &mut pc.ui.gallery, animate)
    {
        effect.kind = k;
    }
    divider(ui);
    ui.add_space(8.0);
    params(ui, pc, &mut effect);
    if effect != before {
        if sync {
            pc.cfg.rgb.global = effect;
        } else if let Some(id) = zid {
            pc.cfg.rgb.zones.entry(id).or_default().effect = effect;
        }
    }
}

fn params(ui: &mut Ui, pc: &mut PageCtx, e: &mut Effect) {
    let p = pal(ui);
    text::label(ui, short_label(e.kind), Ts::Headline, p.text1);
    if e.kind.is_hardware() {
        text::caption(ui, "Onboard effect: runs on the controller itself and keeps going when FanRGB is closed.");
    }
    ui.add_space(6.0);
    let needed = e.kind.uses_colors();
    let palette = matches!(e.kind, EffectKind::Gradient | EffectKind::Twinkle);
    if needed > 0 || palette {
        let want = if palette { e.colors.len().clamp(2, 6) } else { needed };
        let defaults = [[0, 200, 255], [170, 0, 255], [255, 40, 0], [0, 255, 120], [255, 200, 0], [255, 0, 120]];
        // Pad a display copy only: merely showing an effect must not edit (and save) the config.
        let mut colors = e.colors.clone();
        while colors.len() < want {
            colors.push(defaults[colors.len() % defaults.len()]);
        }
        let shown = colors.clone();
        let names: &[&str] = match e.kind {
            EffectKind::Temperature | EffectKind::Load => &["Low", "Mid", "High"],
            EffectKind::Music => &["Bass", "Treble"],
            _ => &[],
        };
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(150.0, 28.0), Sense::hover());
            let label = match e.kind {
                EffectKind::Temperature | EffectKind::Load => "Colors (low · mid · high)",
                EffectKind::Music => "Colors (bass · treble)",
                _ => "Colors",
            };
            ui.painter().text(pos2(r.left(), r.center().y), egui::Align2::LEFT_CENTER, label, font(Ts::Body), p.text2);
            palette_row(ui, Id::new("fx_colors"), &mut colors, want, palette, &mut pc.prefs.recent_colors, names);
        });
        if colors != shown {
            e.colors = colors;
        }
        ui.add_space(4.0);
    }
    if !matches!(
        e.kind,
        EffectKind::Off
            | EffectKind::Static
            | EffectKind::HwStatic
            | EffectKind::Temperature
            | EffectKind::Load
            | EffectKind::Music
    ) {
        let mut s = e.speed * 100.0;
        if slider_row(ui, Id::new("fx_speed"), "Speed", &mut s, 0.0..=100.0, 1.0, " %", 0) {
            e.speed = s / 100.0;
        }
    }
    if e.kind != EffectKind::Off {
        let mut b = e.brightness * 100.0;
        if slider_row(ui, Id::new("fx_bright"), "Brightness", &mut b, 5.0..=100.0, 1.0, " %", 0) {
            e.brightness = b / 100.0;
        }
    }
    if matches!(
        e.kind,
        EffectKind::Rainbow
            | EffectKind::Comet
            | EffectKind::Chase
            | EffectKind::Gradient
            | EffectKind::Fire
            | EffectKind::Load
    ) {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(150.0, 28.0), Sense::hover());
            ui.painter().text(
                pos2(r.left(), r.center().y),
                egui::Align2::LEFT_CENTER,
                "Direction",
                font(Ts::Body),
                p.text2,
            );
            segmented(ui, Id::new("fx_dir"), &mut e.reverse, &[(false, "→  Forward"), (true, "←  Reverse")], None);
        });
    }
    if matches!(e.kind, EffectKind::Temperature | EffectKind::Load) {
        let kind = if e.kind == EffectKind::Temperature { SensorKind::Temperature } else { SensorKind::Load };
        let sensors: Vec<(String, String)> = pc
            .st
            .sensors
            .iter()
            .filter(|s| s.desc.kind == kind)
            .map(|s| (s.desc.id.clone(), format!("{} · {}", super::short_group(&s.desc.group), s.desc.name)))
            .collect();
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(150.0, 28.0), Sense::hover());
            ui.painter().text(
                pos2(r.left(), r.center().y),
                egui::Align2::LEFT_CENTER,
                "Sensor",
                font(Ts::Body),
                p.text2,
            );
            let label = sensors
                .iter()
                .find(|(id, _)| *id == e.sensor)
                .map(|(_, l)| l.clone())
                .unwrap_or_else(|| e.sensor.clone());
            egui::ComboBox::from_id_salt("fx_sensor").width(220.0).selected_text(label).show_ui(ui, |ui| {
                for (sid, l) in &sensors {
                    ui.selectable_value(&mut e.sensor, sid.clone(), l);
                }
            });
            if let Some(v) = pc.st.value(&e.sensor) {
                text::label(ui, format!("now {v:.0}"), Ts::Caption, p.text2);
            }
        });
        let (lo_max, hi_max) = if kind == SensorKind::Temperature { (90.0, 110.0) } else { (90.0, 100.0) };
        slider_row(ui, Id::new("fx_low"), "Low end", &mut e.low, 0.0..=lo_max, 1.0, "", 0);
        let lo = e.low + 1.0;
        e.high = e.high.max(lo);
        slider_row(ui, Id::new("fx_high"), "High end", &mut e.high, lo..=hi_max, 1.0, "", 0);
    }
    if e.kind == EffectKind::Music {
        ui.add_space(4.0);
        match &pc.rgb.audio {
            AudioState::Active(dev) => {
                controls::dot_chip(ui, &format!("Listening to {dev}"), Tone::Success);
            }
            AudioState::Unavailable(err) => {
                let code = ErrorCode::AudioCapture;
                banner(ui, Sev::Warning, code.title(), &format!("{} ({err})", code.help()), |_| {});
            }
            AudioState::Off => {
                controls::dot_chip(ui, "Starting audio capture…", Tone::Neutral);
            }
        }
    }
    if e.kind == EffectKind::Strobe {
        ui.add_space(4.0);
        banner(
            ui,
            Sev::Warning,
            "Rapid flashing",
            "Strobe effects can trigger seizures in people with photosensitive epilepsy.",
            |_| {},
        );
    }
    if e.kind == EffectKind::HwWave {
        text::caption(ui, "Wave runs across the whole board, so per-zone settings are ignored.");
    }
}

fn footer_row(ui: &mut Ui, pc: &mut PageCtx) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        text::label(ui, "When FanRGB exits", Ts::Body, p.text2);
        let label = |e: ExitLighting| match e {
            ExitLighting::KeepLast => "Keep the last colors",
            ExitLighting::Off => "Turn LEDs off",
            ExitLighting::HardwareCycle => "Hardware color cycle",
        };
        egui::ComboBox::from_id_salt("rgb_exit").width(200.0).selected_text(label(pc.cfg.rgb.on_exit)).show_ui(
            ui,
            |ui| {
                for e in [ExitLighting::HardwareCycle, ExitLighting::KeepLast, ExitLighting::Off] {
                    ui.selectable_value(&mut pc.cfg.rgb.on_exit, e, label(e));
                }
            },
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let mut d = pc.prefs.diffused_leds;
            if controls::filter_chip(ui, "Diffused preview", d)
                .on_hover_text("Preview strips as a continuous diffused glow")
                .clicked()
            {
                d = !d;
                pc.prefs.diffused_leds = d;
            }
        });
    });
    let _ = R_PILL;
}
