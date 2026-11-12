//! Cooling: "What is each fan doing and how should it behave?"

use egui::{Id, Rect, Sense, Ui, pos2, vec2};

use super::{
    PageCtx, area_banners, board_for, combined, duty_id, emergency_banner, fan_block, fan_block_state, fan_nodes,
    led_nodes, source_short, temperature_sensors,
};
use crate::config::{AiConfig, AiStyle, Combine, FanConfig, FanMode, Preset, TempSource};
use crate::model::{FanRole, FanState};
use crate::ui::actions::UiAction;
use crate::ui::board_map;
use crate::ui::fonts::{Ts, font, tab};
use crate::ui::icons::{self, Icon};
use crate::ui::issues::Area;
use crate::ui::layout::{self, Breakpoint};
use crate::ui::overlays::{Modal, ToastKind};
use crate::ui::theme::{R_MD, R_PILL, pal};
use crate::ui::widgets::board::{BoardMode, BoardView};
use crate::ui::widgets::card::{Sev, banner, card, divider, header};
use crate::ui::widgets::chart::{at, window_start};
use crate::ui::widgets::controls::{self, Button, Kind, Size, Tone, segmented, slider_row};
use crate::ui::widgets::curve::{self, CurveEditor};
use crate::ui::widgets::{self, text};
use crate::ui::{Page, anim};

pub fn show(ui: &mut Ui, pc: &mut PageCtx) {
    let gap = pc.bp.gap();
    ui.add_space(4.0);
    if let Some(code) = fan_block(pc.st, &pc.ui.issues) {
        fan_block_state(ui, pc, code, "Fan control");
        return;
    }
    area_banners(ui, pc, &[Area::Cooling, Area::Sensors], &[]);
    emergency_banner(ui, pc);
    crate::ui::widgets::fan_modes::mode_bar(ui, pc, true);
    ui.add_space(6.0);
    state_line(ui, pc);
    ui.add_space(gap * 0.75);

    let fans: Vec<FanState> = pc.st.fans.iter().filter(|f| f.visible || pc.ui.show_hidden_fans).cloned().collect();
    if fans.is_empty() {
        crate::ui::widgets::card::empty_state(
            ui,
            Icon::Cooling,
            None,
            "No fan headers found yet",
            "FanRGB has not received any fan channels from the hardware layer. If this persists, write a diagnostics report.",
            |ui| {
                if Button::new(Kind::Secondary, "Open Diagnostics").show(ui).clicked() {
                    pc.ui.pending.push(UiAction::Navigate(Page::Diagnostics));
                }
            },
        );
        return;
    }
    if pc.ui.selected_fan.as_ref().is_none_or(|id| !fans.iter().any(|f| &f.id == id)) {
        pc.ui.selected_fan = fans.first().map(|f| f.id.clone());
    }
    let wide = pc.bp == Breakpoint::Wide;
    if wide {
        let avail = ui.available_width();
        let left = 300.0;
        layout::row(ui, Id::new("cool_split"), &[left, avail - left - gap], gap, |i, ui, _| {
            if i == 0 {
                master(ui, pc, &fans);
            } else {
                inspector(ui, pc, &fans);
            }
            0.0
        });
    } else {
        super::overview::identify_prompt(ui, pc);
        chip_strip(ui, pc, &fans);
        ui.add_space(gap * 0.75);
        inspector(ui, pc, &fans);
    }
}

// ---------------------------------------------------------------------------------------

/// Under the tiles: the selected mode, Preset / Custom, "Edited · what" and Reset / Save as custom.
/// Duplicate, rename and delete live in the Custom ▾ tile.
fn state_line(ui: &mut Ui, pc: &mut PageCtx) {
    let p = pal(ui);
    let mode = pc.cfg.fan_mode();
    let prof = pc.cfg.profile().clone();
    ui.horizontal(|ui| {
        ui.set_min_height(36.0);
        text::label(ui, &mode.label, Ts::Headline, p.text1);
        controls::chip(ui, if prof.preset == Preset::Custom { "Custom" } else { "Preset" }, Tone::Neutral);
        if mode.edited {
            let what: Vec<String> = prof
                .fans
                .keys()
                .map(|id| {
                    pc.st.fans.iter().find(|f| &f.id == id).map(|f| f.header.clone()).unwrap_or_else(|| id.clone())
                })
                .collect();
            let shown = if what.len() > 2 { format!("{} headers", what.len()) } else { what.join(", ") };
            controls::dot_chip(ui, &format!("Edited · {shown}"), Tone::Warning)
                .on_hover_text("This mode has per-fan changes. “Reset to preset” restores its defaults.");
        }
        if pc.st.emergency_released {
            controls::dot_chip(ui, "Not active: fans are under BIOS control", Tone::Warning);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if Button::new(Kind::Secondary, "Save as custom…")
                .show(ui)
                .on_hover_text("Copy this mode with every fan's settings into a new custom profile")
                .clicked()
            {
                crate::ui::widgets::fan_modes::duplicate_active(pc);
            }
            if prof.preset != Preset::Custom
                && prof.preset != Preset::Bios
                && ui
                    .add_enabled_ui(mode.edited, |ui| {
                        Button::new(Kind::Ghost, "Reset to preset").icon(Icon::Undo).show(ui)
                    })
                    .inner
                    .clicked()
            {
                pc.cfg.profile_mut().fans.clear();
                pc.ui.toast(ToastKind::Success, format!("{} reset to its defaults", mode.label));
            }
        });
    });
}

fn master(ui: &mut Ui, pc: &mut PageCtx, fans: &[FanState]) {
    let p = pal(ui);
    let gap = pc.bp.gap();
    let map = board_for(pc.st);
    let texture = map.and_then(|_| pc.assets.board(ui.ctx()));
    card(ui, |ui| {
        header(ui, "Board", map.map(|m| m.name), |_| {});
        ui.add_space(4.0);
        let nodes = fan_nodes(pc.st, pc.ui.show_hidden_fans);
        let leds = led_nodes(pc.cfg, pc.rgb);
        let sel = pc.ui.selected_fan.clone();
        if let Some(id) = (BoardView {
            id: Id::new("cool_board"),
            map,
            mode: BoardMode::Cooling,
            fans: &nodes,
            leds: &leds,
            selected: sel.as_deref(),
            show_ghosts: pc.cfg.general.show_all_headers,
            preview_only: false,
            texture,
        })
        .show(ui)
        {
            pc.ui.selected_fan = Some(id);
        }
    });
    ui.add_space(gap);
    card(ui, |ui| {
        header(ui, "Fans", None, |ui| {
            let more = controls::icon_button_sized(ui, Icon::More, "List options", 28.0, None);
            egui::Popup::menu(&more).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
                ui.set_min_width(200.0);
                ui.checkbox(&mut pc.ui.show_hidden_fans, "Show unused headers");
                ui.checkbox(&mut pc.cfg.general.show_all_headers, "Show all fan headers");
                if ui.button("Identify fans…").clicked() {
                    pc.prefs.identify_prompt_dismissed = false;
                    pc.ui.pending.push(UiAction::IdentifyHeaders);
                    ui.close();
                }
            });
        });
        ui.add_space(2.0);
        super::overview::identify_prompt(ui, pc);
        for f in fans {
            super::overview::fan_row(ui, pc, f);
        }
        let _ = p;
    });
}

/// "What's connected" picker: sets the header role, which picks the thumbnail,
/// the name (unless the user named it) and the safety rules (pump / unknown floors).
fn role_picker(ui: &mut Ui, pc: &mut PageCtx, fan: &FanState) {
    use crate::config::HeaderRole;
    let r = Button::new(Kind::Ghost, "What's connected?").size(Size::Sm).icon(Icon::ChevronDown).show(ui);
    egui::Popup::menu(&r).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
        let p = pal(ui);
        ui.set_min_width(260.0);
        text::overline(ui, &format!("On {}", fan.header));
        for role in HeaderRole::ALL {
            let help = match role {
                HeaderRole::RadiatorFans => "AIO radiator fans: cool the CPU",
                HeaderRole::CpuFans => "Air cooler fans: cool the CPU",
                HeaderRole::CaseFans => "Intake / exhaust, direct or on a hub",
                HeaderRole::Pump => "Own quiet curve on CPU temperature, never below 30 %",
                HeaderRole::Unknown => "Safe 60 % minimum until confirmed",
                HeaderRole::Unused => "Not driven (left to the BIOS); still watched for cooling failure",
            };
            let selected = fan.assigned == Some(role);
            let (rr, resp) = ui.allocate_exact_size(vec2(260.0, 40.0), Sense::click());
            if resp.hovered() || selected {
                ui.painter().rect_filled(rr, R_MD, if selected { p.accent_subtle } else { p.surface2 });
            }
            ui.painter().text(
                pos2(rr.left() + 10.0, rr.top() + 13.0),
                egui::Align2::LEFT_CENTER,
                role.label(),
                font(Ts::BodyStrong),
                p.text1,
            );
            ui.painter().text(
                pos2(rr.left() + 10.0, rr.top() + 29.0),
                egui::Align2::LEFT_CENTER,
                help,
                font(Ts::Caption),
                p.text2,
            );
            if resp.clicked() {
                pc.cfg.set_fan_role(&fan.id, Some(role), None);
                pc.ui.toast(ToastKind::Success, format!("{}: {}", fan.header, role.label()));
                ui.close();
            }
        }
        if fan.assigned.is_some() && ui.button("Forget (not confirmed)").clicked() {
            pc.cfg.set_fan_role(&fan.id, None, None);
            ui.close();
        }
        crate::ui::widgets::card::divider(ui);
        if ui.button("Identify with the wizard…").clicked() {
            pc.ui.pending.push(UiAction::IdentifyHeaders);
            ui.close();
        }
    });
}

fn chip_strip(ui: &mut Ui, pc: &mut PageCtx, fans: &[FanState]) {
    let p = pal(ui);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
        for f in fans {
            let sel = pc.ui.selected_fan.as_deref() == Some(f.id.as_str());
            let label = format!("{}  {}", f.name, f.rpm.map(widgets::thousands).unwrap_or_else(|| "—".into()));
            let g = ui.painter().layout_no_wrap(label, tab(Ts::Label), p.text1);
            let (r, resp) = ui.allocate_exact_size(vec2(g.size().x + 44.0, 34.0), Sense::click());
            let fill = if sel {
                p.accent_subtle
            } else if resp.hovered() {
                p.surface2
            } else {
                p.surface1
            };
            ui.painter().rect_filled(r, R_PILL, fill);
            ui.painter().rect_stroke(
                r,
                R_PILL,
                egui::Stroke::new(1.0, if sel { p.accent } else { p.border_subtle }),
                egui::StrokeKind::Inside,
            );
            let color = if f.controlled { p.accent } else { p.text3 };
            let angle = anim::spin_angle(ui.ctx(), f.rpm);
            icons::paint_fan(
                ui.painter(),
                Rect::from_center_size(pos2(r.left() + 18.0, r.center().y), vec2(18.0, 18.0)),
                angle,
                color,
            );
            ui.painter().galley(
                pos2(r.left() + 32.0, r.center().y - g.size().y / 2.0),
                g,
                if sel { p.text1 } else { p.text2 },
            );
            if resp.clicked() {
                pc.ui.selected_fan = Some(f.id.clone());
            }
        }
        let more = controls::icon_button_sized(ui, Icon::More, "List options", 32.0, None);
        egui::Popup::menu(&more).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
            ui.checkbox(&mut pc.ui.show_hidden_fans, "Show unused headers");
            ui.checkbox(&mut pc.cfg.general.show_all_headers, "Show all fan headers");
            ui.checkbox(&mut pc.prefs.show_board_map, "Show board map on Overview");
        });
    });
}

// ---------------------------------------------------------------------------------------

fn inspector(ui: &mut Ui, pc: &mut PageCtx, fans: &[FanState]) {
    let p = pal(ui);
    let Some(fan) = fans.iter().find(|f| Some(&f.id) == pc.ui.selected_fan.as_ref()).cloned() else { return };
    let f_unit = pc.cfg.general.fahrenheit;
    card(ui, |ui| {
        // ---- header: thumbnail, name (rename), header id + what's connected, big value ----
        ui.horizontal(|ui| {
            ui.set_min_height(48.0);
            let ctx = ui.ctx().clone();
            let (tr, _) = ui.allocate_exact_size(vec2(44.0, 44.0), Sense::hover());
            crate::ui::widgets::thumb::paint(
                &ctx,
                ui.painter(),
                pc.assets,
                p,
                tr,
                crate::ui::widgets::thumb::thumb_for(&fan),
                None,
                fan.assigned.is_none(),
            );
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.horizontal(|ui| match &mut pc.ui.rename_fan {
                    Some((id, buf)) if id == &fan.id => {
                        let r = ui.add(egui::TextEdit::singleline(buf).font(font(Ts::Headline)).desired_width(200.0));
                        if !r.has_focus() && !r.lost_focus() {
                            r.request_focus();
                        }
                        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if Button::new(Kind::Primary, "Save").size(Size::Sm).show(ui).clicked()
                            || (r.lost_focus() && enter)
                        {
                            let name = buf.trim().to_string();
                            if name.is_empty() || Some(name.as_str()) == board_map::fan_label(&fan.id) {
                                pc.cfg.fan_names.remove(&fan.id);
                            } else {
                                pc.cfg.fan_names.insert(fan.id.clone(), name);
                            }
                            pc.ui.rename_fan = None;
                        } else if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            pc.ui.rename_fan = None;
                        }
                    }
                    _ => {
                        text::label(ui, &fan.name, Ts::Title, p.text1);
                        if controls::icon_button_sized(ui, Icon::Pencil, "Rename", 26.0, None).clicked() {
                            pc.ui.rename_fan = Some((fan.id.clone(), fan.name.clone()));
                        }
                    }
                });
                ui.horizontal(|ui| {
                    let what = match fan.assigned {
                        Some(r) => r.label().to_string(),
                        None => "not identified".into(),
                    };
                    let sub = if fan.name != fan.header { format!("{} · {what}", fan.header) } else { what };
                    text::label(ui, sub, Ts::Caption, p.text2);
                    if fan.controllable && !fan.unconnected {
                        role_picker(ui, pc, &fan);
                    }
                });
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let more = controls::icon_button_sized(ui, Icon::More, "Fan options", 28.0, None);
                egui::Popup::menu(&more).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
                    let mut visible = fan.visible;
                    if ui.checkbox(&mut visible, "Show on Overview").changed() {
                        pc.cfg.fan_visible.insert(fan.id.clone(), visible);
                    }
                    if ui.button("Rename…").clicked() {
                        pc.ui.rename_fan = Some((fan.id.clone(), fan.name.clone()));
                        ui.close();
                    }
                    if ui.button("Identify fans…").clicked() {
                        pc.ui.pending.push(UiAction::IdentifyHeaders);
                        ui.close();
                    }
                });
                ui.add_space(8.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
                        if fan.no_speed_signal() {
                            // No tach: the duty is the hero number (neutral, never a warning).
                            let duty = fan.duty.or(fan.duty_est).map(|d| format!("{d:.0}")).unwrap_or("—".into());
                            text::value_unit(ui, &duty, "%", Ts::Display, Ts::Caption, p.text1, p.text2);
                            text::label(ui, "duty · no speed signal", Ts::Caption, p.text2)
                                .on_hover_text(super::NO_SPEED_SIGNAL_HELP);
                        } else {
                            text::value_unit(
                                ui,
                                &fan.rpm.map(widgets::thousands).unwrap_or("—".into()),
                                "rpm",
                                Ts::Display,
                                Ts::Caption,
                                p.text1,
                                p.text2,
                            );
                            let speed = match (fan.duty, fan.duty_est) {
                                (Some(d), _) => format!("{d:.0} % speed"),
                                (None, Some(e)) => format!("≈{e:.0} % (BIOS curve, est.)"),
                                (None, None) => "Speed set by BIOS".into(),
                            };
                            text::label(ui, speed, Ts::Caption, p.text2);
                        }
                    });
                });
            });
        });
        ui.add_space(6.0);
        // ---- status chips ----
        {
            use controls::ChipSpec as C;
            let tone = if fan.failsafe {
                Tone::Danger
            } else if fan.controlled {
                Tone::Accent
            } else {
                Tone::Neutral
            };
            let mut chips = vec![C::new(fan.mode.clone(), tone).dot()];
            if fan.failsafe {
                chips.push(
                    C::new("Safety 100 %", Tone::Danger)
                        .tip("The safety layer overrides this fan (critical temperature or lost sensor)."),
                );
            }
            if fan.no_speed_signal() {
                let stall =
                    if fan.stall_alert() { "" } else { "\n\nNo tach signal: a stalled fan or pump can't be detected." };
                chips.push(
                    C::new("Running · no speed signal", Tone::Neutral)
                        .tip(format!("{}{stall}", super::NO_SPEED_SIGNAL_HELP)),
                );
                if fan.role.cools_cpu() && fan.controllable {
                    chips.push(C::new("Guarded by CPU temperature", Tone::Success).tip(
                        "FanRGB checks that the CPU temperature follows this header's speed, and runs every fan at 100 % if cooling fails.",
                    ));
                }
            }
            if fan.hw_curve {
                chips.push(
                    C::new("Runs in hardware", Tone::Success)
                        .tip("The curve runs inside the Super I/O chip, so it keeps working even if FanRGB stops."),
                );
            }
            if fan.hw_failsafe {
                chips.push(
                    C::new("Hardware failsafe", Tone::Info)
                        .tip("The fan controller forces 100 % on its own above the failsafe temperature."),
                );
            }
            if !fan.controllable {
                chips.push(C::new("Read-only", Tone::Neutral));
            }
            if let Some(f) = fan.floor {
                chips.push(C::new(format!("Min {f:.0} %"), Tone::Info).tip(
                    "Pumps, the board's CPU and PUMP headers, and headers whose device is not identified yet, never run below this in any mode except BIOS.",
                ));
            }
            if let Some(l) = fan.gpu_lift {
                chips.push(
                    C::new(format!("GPU floor {l:.0} %"), Tone::Info)
                        .tip("The chip runs the CPU curve; FanRGB raises its minimum while the GPU is warm."),
                );
            }
            if let Some(why) = fan.ai_refused {
                chips.push(C::new("AI not used", Tone::Neutral).tip(why));
            }
            if fan.identifying {
                chips.push(C::new("Identifying…", Tone::Warning));
            }
            controls::chip_flow(ui, &chips, 6.0);
        }
        ui.add_space(8.0);
        if !fan.controllable {
            banner(
                ui,
                Sev::Info,
                crate::error::ErrorCode::FanReadOnly.title(),
                crate::error::ErrorCode::FanReadOnly.help(),
                |_| {},
            );
            return;
        }
        let role = fan.role;
        let cal = pc.cfg.calibrations.get(&fan.id).cloned();
        let mut fc: FanConfig = pc.cfg.profile().fan(&fan.id, role);
        let before = fc.clone();
        // The BIOS preset never drives a fan (whatever overrides it stores), so its fans aren't editable.
        let bios_profile = pc.cfg.profile().preset == Preset::Bios;
        let dim = pc.st.emergency_released || bios_profile;
        if bios_profile {
            text::caption(
                ui,
                "The BIOS profile leaves every fan to the BIOS. Pick another profile, or Duplicate this one into a custom profile, to set this fan.",
            );
            ui.add_space(6.0);
        }

        ui.add_enabled_ui(!dim, |ui| {
            // ---- mode ----
            ui.horizontal(|ui| {
                text::label(ui, "Mode", Ts::Label, p.text2);
                ui.add_space(8.0);
                segmented(ui, Id::new(("mode", &fan.id)), &mut fc.mode, &[(FanMode::Bios, "BIOS"), (FanMode::Fixed, "Fixed"), (FanMode::Curve, "Curve"), (FanMode::Ai, "AI")], Some(320.0f32.min(ui.available_width())));
            });
            ui.add_space(10.0);
            match fc.mode {
                FanMode::Bios => {
                    text::para(ui, "The motherboard's Smart Fan curve (set in the BIOS) controls this fan. FanRGB restores the BIOS settings exactly as they were.", Ts::Body, p.text2);
                }
                FanMode::Fixed => {
                    slider_row(ui, Id::new(("fixed", &fan.id)), "Speed", &mut fc.fixed_duty, 0.0..=100.0, 1.0, " %", 0);
                    if let Some(rpm) = cal.as_ref().and_then(|c| c.rpm_at(fc.fixed_duty)) {
                        text::label(ui, format!("≈ {} rpm (from calibration)", widgets::thousands(rpm)), Ts::Caption, p.text2);
                    }
                }
                FanMode::Curve => curve_body(ui, pc, &fan, &mut fc, cal.as_ref(), f_unit),
                FanMode::Ai => ai_body(ui, pc, &fan, &mut fc, role),
            }
            if fan.no_speed_signal() {
                ui.add_space(10.0);
                no_tach_card(ui, &fan, pc.cfg.general.cpu_critical);
            }
            // ---- behavior ----
            if fc.mode != FanMode::Bios {
                ui.add_space(10.0);
                divider(ui);
                behavior(ui, pc, &fan, &mut fc);
            }
        });
        ui.add_space(4.0);
        divider(ui);
        calibration(ui, pc, &fan, cal.as_ref());
        divider(ui);
        ui.add_space(8.0);
        apply_to(ui, pc, &fan, &fc);

        if fc != before {
            fc.curve.points.sort_by(|a, b| a[0].total_cmp(&b[0]));
            pc.cfg.profile_mut().fans.insert(fan.id.clone(), fc);
        }
    });
}

fn source_picker(ui: &mut Ui, pc: &PageCtx, id: &str, src: &mut TempSource) {
    let p = pal(ui);
    let sensors = temperature_sensors(pc.st);
    let summary = source_short(pc.st, src);
    ui.horizontal(|ui| {
        text::label(ui, "Source", Ts::Label, p.text2);
        egui::ComboBox::from_id_salt(id)
            .width(220.0)
            .selected_text(text::rt(summary, Ts::Body, p.text1))
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show_ui(ui, |ui| {
                for (sid, label) in &sensors {
                    let mut on = src.sensors.contains(sid);
                    let v = pc.st.value(sid).map(|t| format!("   {t:.0}°")).unwrap_or_default();
                    if ui.checkbox(&mut on, format!("{label}{v}")).changed() {
                        if on {
                            src.sensors.push(sid.clone());
                        } else if src.sensors.len() > 1 {
                            src.sensors.retain(|s| s != sid);
                        }
                    }
                }
            });
        if src.sensors.len() > 1 {
            controls::segmented_sized(
                ui,
                Id::new((id, "combine")),
                &mut src.combine,
                &[(Combine::Max, "Hottest"), (Combine::Average, "Average")],
                None,
                28.0,
            );
        }
    });
}

/// Last 60 s of (source temperature, duty) from the history, oldest first.
fn operating_trail(pc: &PageCtx, fan: &FanState, src: &TempSource) -> Vec<(f32, f32)> {
    let h = pc.hist;
    let n = h.times.len();
    let start = window_start(&h.times, 60.0);
    let duty = h.series.get(&duty_id(fan));
    let srcs: Vec<_> = src.sensors.iter().filter_map(|s| h.series.get(s)).collect();
    (start..n)
        .filter_map(|i| {
            let temps: Vec<f32> = srcs.iter().map(|s| at(n, s, i)).filter(|v| v.is_finite()).collect();
            let t = crate::control::combine(&temps, src.combine)?;
            let d = duty.map(|s| at(n, s, i)).filter(|v| v.is_finite())?;
            Some((t, d))
        })
        .collect()
}

fn curve_body(
    ui: &mut Ui,
    pc: &mut PageCtx,
    fan: &FanState,
    fc: &mut FanConfig,
    cal: Option<&crate::config::FanCalibration>,
    f_unit: bool,
) {
    let p = pal(ui);
    let eid = Id::new(("curve", &fan.id));
    ui.horizontal(|ui| {
        source_picker(ui, pc, &format!("curve_src_{}", fan.id), &mut fc.curve.source);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if cal.is_some_and(|c| c.points.len() >= 2) {
                let mut on = pc.prefs.curve_rpm_axis;
                if controls::filter_chip(ui, "rpm axis", on)
                    .on_hover_text("Show the RPM scale from calibration")
                    .clicked()
                {
                    on = !on;
                    pc.prefs.curve_rpm_axis = on;
                }
            }
            let can_redo = curve::can_redo(ui, eid);
            let can_undo = curve::can_undo(ui, eid);
            if ui
                .add_enabled_ui(can_redo, |ui| controls::icon_button_sized(ui, Icon::Redo, "Redo (Ctrl+Y)", 28.0, None))
                .inner
                .clicked()
            {
                curve::redo(ui, eid, &mut fc.curve.points);
            }
            if ui
                .add_enabled_ui(can_undo, |ui| controls::icon_button_sized(ui, Icon::Undo, "Undo (Ctrl+Z)", 28.0, None))
                .inner
                .clicked()
            {
                curve::undo(ui, eid, &mut fc.curve.points);
            }
            let presets = Button::new(Kind::Secondary, "Presets").size(Size::Sm).icon(Icon::ChevronDown).show(ui);
            let others: Vec<(String, String)> = pc
                .st
                .fans
                .iter()
                .filter(|o| o.id != fan.id && o.controllable)
                .map(|o| (o.id.clone(), o.name.clone()))
                .collect();
            egui::Popup::menu(&presets).show(|ui| {
                ui.set_min_width(180.0);
                for (pr, name) in
                    [(Preset::Silent, "Silent"), (Preset::Balanced, "Balanced"), (Preset::Performance, "Performance")]
                {
                    if ui.button(name).clicked() {
                        let before = fc.curve.points.clone();
                        fc.curve.points = FanConfig::preset(pr, fan.role).curve.points;
                        curve::record(ui, eid, before);
                    }
                }
                if ui.button("Flat 50 %").clicked() {
                    let before = fc.curve.points.clone();
                    fc.curve.points = vec![[30.0, 50.0], [90.0, 50.0]];
                    curve::record(ui, eid, before);
                }
                if !others.is_empty() {
                    ui.separator();
                    ui.menu_button("Copy from", |ui| {
                        for (oid, oname) in &others {
                            if ui.button(oname).clicked() {
                                let role =
                                    pc.st.fans.iter().find(|f| &f.id == oid).map(|f| f.role).unwrap_or(FanRole::System);
                                let before = fc.curve.points.clone();
                                fc.curve.points = pc.cfg.profile().fan(oid, role).curve.points;
                                curve::record(ui, eid, before);
                            }
                        }
                    });
                }
            });
        });
    });
    ui.add_space(6.0);
    let live = combined(pc.st, &fc.curve.source);
    let trail = operating_trail(pc, fan, &fc.curve.source);
    let requested =
        live.map(|t| crate::control::curve::interpolate(&fc.curve.points, t).clamp(fc.min_duty, fc.max_duty));
    let explain = match (requested, fan.duty) {
        (Some(r), Some(a)) if a + 2.0 < r => Some(format!("Speeding up at most {:.0} %/s (speed-up rate)", fc.ramp_up)),
        (Some(r), Some(a)) if a > r + 2.0 => {
            Some(format!("Slowing down at {:.1} %/s; hysteresis {:.0} °C", fc.ramp_down, fc.curve.hysteresis))
        }
        _ => None,
    };
    let rpm_fn = cal.cloned();
    let rpm_at = move |d: f32| rpm_fn.as_ref().and_then(|c| c.rpm_at(d));
    let height = if pc.bp == Breakpoint::Narrow { 260.0 } else { 340.0 };
    let src_label = source_short(pc.st, &fc.curve.source);
    let edit = CurveEditor {
        id: eid,
        height,
        fahrenheit: f_unit,
        critical: pc.cfg.general.cpu_critical,
        min_duty: fc.min_duty,
        max_duty: fc.max_duty,
        hysteresis: fc.curve.hysteresis,
        live_temp: live,
        actual_duty: fan.duty,
        trail: &trail,
        rpm_at: if cal.is_some() { Some(&rpm_at) } else { None },
        rpm_axis: pc.prefs.curve_rpm_axis,
        source_label: &src_label,
        explain,
    }
    .show(ui, &mut fc.curve.points);
    if edit.reset_requested {
        let before = fc.curve.points.clone();
        fc.curve.points = FanConfig::preset(
            if pc.cfg.profile().preset == Preset::Custom { Preset::Balanced } else { pc.cfg.profile().preset },
            fan.role,
        )
        .curve
        .points;
        curve::record(ui, eid, before);
    }
    ui.add_space(6.0);
    curve::point_strip(ui, eid, &mut fc.curve.points, f_unit);
    ui.add_space(2.0);
    text::caption3(
        ui,
        "Drag points · double-click to add · right-click for options · arrows select, Ctrl+arrows move, Del removes",
    );
    let _ = p;
}

fn ai_body(ui: &mut Ui, pc: &mut PageCtx, fan: &FanState, fc: &mut FanConfig, role: FanRole) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        text::label(ui, "Style", Ts::Label, p.text2);
        ui.add_space(8.0);
        let old = fc.ai.style;
        segmented(
            ui,
            Id::new(("ai_style", &fan.id)),
            &mut fc.ai.style,
            &[(AiStyle::Silent, "Silent"), (AiStyle::Balanced, "Balanced"), (AiStyle::Performance, "Performance")],
            Some(330.0f32.min(ui.available_width())),
        );
        if fc.ai.style != old {
            let d = AiConfig::for_role(fc.ai.style, role);
            fc.ai.target = d.target;
            fc.ai.limit = d.limit;
        }
    });
    ui.add_space(6.0);
    source_picker(ui, pc, &format!("ai_src_{}", fan.id), &mut fc.ai.source);
    ui.add_space(6.0);
    super::ai_target_sliders(ui, Id::new(("ai", &fan.id)), &mut fc.ai, super::ai_limit_cap(role));
    if let Some(z) = pc.st.ai.iter().find(|z| z.fans.contains(&fan.name)) {
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            controls::chip(
                ui,
                &format!("{:.0} % confident", z.confidence * 100.0),
                if z.confidence > 0.6 { Tone::Success } else { Tone::Warning },
            );
            text::para(ui, &z.reason, Ts::Body, p.text2);
        });
    }
    ui.add_space(4.0);
    if controls::link(ui, "Open in Intelligence →").clicked() {
        pc.ui.pending.push(UiAction::Navigate(Page::Intelligence));
    }
}

fn disclosure(ui: &mut Ui, open: &mut bool, title: &str, summary: &str) {
    let p = pal(ui);
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(r, R_MD, p.surface2);
    }
    icons::paint(
        ui.painter(),
        Rect::from_center_size(pos2(r.left() + 10.0, r.center().y), vec2(14.0, 14.0)),
        if *open { Icon::ChevronDown } else { Icon::ChevronRight },
        p.text2,
    );
    let g = ui.painter().layout_no_wrap(title.to_string(), font(Ts::BodyStrong), p.text1);
    let tw = g.size().x;
    ui.painter().galley(pos2(r.left() + 24.0, r.center().y - g.size().y / 2.0), g, p.text1);
    ui.painter().text(
        pos2(r.left() + 36.0 + tw, r.center().y),
        egui::Align2::LEFT_CENTER,
        summary,
        tab(Ts::Caption),
        p.text3,
    );
    if resp.clicked() {
        *open = !*open;
    }
}

fn behavior(ui: &mut Ui, pc: &mut PageCtx, fan: &FanState, fc: &mut FanConfig) {
    let summary = format!(
        "min {:.0} % · max {:.0} % · ramps {:.0}/{:.0} %/s{}",
        fc.min_duty,
        fc.max_duty,
        fc.ramp_up,
        fc.ramp_down,
        if fc.mode == FanMode::Curve {
            format!(" · hysteresis {:.0}° · response {:.0} s", fc.curve.hysteresis, fc.curve.response_s)
        } else {
            String::new()
        }
    );
    let mut open = pc.ui.behavior_open;
    disclosure(ui, &mut open, "Behavior", &summary);
    pc.ui.behavior_open = open;
    if !open {
        return;
    }
    ui.add_space(4.0);
    let id = |s: &str| Id::new((s, fan.id.as_str()));
    slider_row(ui, id("min"), "Minimum speed", &mut fc.min_duty, 0.0..=100.0, 1.0, " %", 0);
    slider_row(ui, id("max"), "Maximum speed", &mut fc.max_duty, 10.0..=100.0, 1.0, " %", 0);
    slider_row(ui, id("up"), "Speed-up rate", &mut fc.ramp_up, 1.0..=50.0, 0.5, " %/s", 1);
    slider_row(ui, id("down"), "Slow-down rate", &mut fc.ramp_down, 0.5..=20.0, 0.5, " %/s", 1);
    if fc.mode == FanMode::Curve {
        slider_row(ui, id("hyst"), "Hysteresis", &mut fc.curve.hysteresis, 0.0..=10.0, 0.5, " °C", 1);
        slider_row(ui, id("resp"), "Response time", &mut fc.curve.response_s, 0.0..=20.0, 0.5, " s", 1);
    }
    controls::toggle_row(
        ui,
        "Allow fan stop",
        "Lets the fan stop completely when the requested speed is very low (0 rpm mode).",
        &mut fc.allow_stop,
    );
}

/// Info-styled (never a warning) explainer for a header without a tach.
fn no_tach_card(ui: &mut Ui, fan: &FanState, critical: f32) {
    let p = pal(ui);
    let floor = fan.floor.map(|f| format!("{f:.0} %")).unwrap_or_else(|| "its minimum".into());
    egui::Frame::new().fill(p.info_subtle).corner_radius(R_MD).inner_margin(egui::Margin::same(14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_top(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
            icons::paint(ui.painter(), r, Icon::Info, p.info);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                text::para(ui, "This header doesn't report a speed — that's normal here", Ts::BodyStrong, p.text1);
                text::para(
                    ui,
                    format!(
                        "Splitters and many AIO fans have no tach wire. FanRGB never lets it drop below {floor} and checks \
                         the CPU temperature instead; above {critical:.0} °C every fan runs at 100 %."
                    ),
                    Ts::Body,
                    p.text2,
                );
            });
        });
    });
}

fn calibration(ui: &mut Ui, pc: &mut PageCtx, fan: &FanState, cal: Option<&crate::config::FanCalibration>) {
    let p = pal(ui);
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        text::label(ui, "Calibration", Ts::BodyStrong, p.text1);
        ui.add_space(8.0);
        match cal {
            Some(c) if c.max_rpm > 0.0 => {
                let s = format!(
                    "max {} rpm{}{}",
                    widgets::thousands(c.max_rpm),
                    c.stop_duty.map(|d| format!(" · stops below {d:.0} %")).unwrap_or_default(),
                    c.start_duty.map(|d| format!(" · starts at {d:.0} %")).unwrap_or_default()
                );
                text::label(ui, &s, Ts::Caption, p.text2);
            }
            _ => {
                text::label(ui, "Not calibrated", Ts::Caption, p.text3);
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let busy = pc.st.calibration.is_some();
            // Calibration measures RPM: impossible without a tach signal.
            let refused = fan.calibration_refused();
            let r = ui.add_enabled_ui(!busy && refused.is_none(), |ui| {
                Button::new(Kind::Secondary, if cal.is_some() { "Recalibrate" } else { "Calibrate" })
                    .size(Size::Sm)
                    .show(ui)
            });
            let r = match refused {
                Some(_) if !fan.speed_signal => r.inner.on_disabled_hover_text("Needs a speed signal"),
                Some(why) => r.inner.on_disabled_hover_text(why),
                None => r.inner,
            };
            if r.clicked() {
                pc.ui.modal = Some(Modal::Calibrate { fan: fan.index, name: fan.name.clone(), started: false });
            }
            if cal.is_some() && Button::new(Kind::Ghost, "Clear").size(Size::Sm).show(ui).clicked() {
                pc.cfg.calibrations.remove(&fan.id);
                let id = fan.id.clone();
                pc.shared.edit_config(|c| {
                    c.calibrations.remove(&id);
                });
            }
            if busy {
                controls::chip(ui, "Calibrating…", Tone::Info);
            }
        });
    });
    if let Some(why) = fan.calibration_refused() {
        text::caption3(
            ui,
            if fan.speed_signal {
                why.to_string()
            } else {
                "Needs a speed signal; stop/start duty use safe defaults.".to_string()
            },
        );
    } else if let Some(c) = cal.filter(|c| c.points.len() >= 2) {
        ui.add_space(4.0);
        crate::ui::overlays::rpm_duty_chart(ui, &c.points, 110.0);
    } else {
        text::caption(
            ui,
            "Measures RPM at every speed, where the fan stops and where it starts (about 90 s). The AI and the minimum-speed safety use the result.",
        );
    }
    ui.add_space(8.0);
}

fn apply_to(ui: &mut Ui, pc: &mut PageCtx, fan: &FanState, fc: &FanConfig) {
    let p = pal(ui);
    let targets: Vec<(String, String, FanRole)> = pc
        .st
        .fans
        .iter()
        .filter(|o| o.id != fan.id && o.controllable && o.visible)
        .map(|o| (o.id.clone(), o.name.clone(), o.role))
        .collect();
    if targets.is_empty() {
        return;
    }
    let r = Button::new(Kind::Secondary, "Apply to…").icon(Icon::Copy).size(Size::Sm).show(ui);
    let sel_id = Id::new(("apply_sel", &fan.id));
    egui::Popup::from_toggle_button_response(&r).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(
        |ui| {
            ui.set_min_width(240.0);
            let mut sel: Vec<String> = ui.data(|d| d.get_temp(sel_id)).unwrap_or_default();
            text::label(ui, format!("Copy {}'s settings to", fan.name), Ts::BodyStrong, p.text1);
            ui.add_space(4.0);
            for (id, name, _) in &targets {
                let mut on = sel.contains(id);
                if ui.checkbox(&mut on, name).changed() {
                    if on {
                        sel.push(id.clone());
                    } else {
                        sel.retain(|s| s != id);
                    }
                }
            }
            if ui.button("All system fans").clicked() {
                sel = targets.iter().filter(|t| t.2 == FanRole::System).map(|t| t.0.clone()).collect();
            }
            ui.add_space(6.0);
            if ui
                .add_enabled_ui(!sel.is_empty(), |ui| Button::new(Kind::Primary, "Apply").size(Size::Sm).show(ui))
                .inner
                .clicked()
            {
                for id in &sel {
                    pc.cfg.profile_mut().fans.insert(id.clone(), fc.clone());
                }
                pc.ui.toast(
                    ToastKind::Success,
                    format!("Applied to {} fan{}", sel.len(), if sel.len() == 1 { "" } else { "s" }),
                );
                sel.clear();
                ui.close();
            }
            ui.data_mut(|d| d.insert_temp(sel_id, sel));
        },
    );
}
