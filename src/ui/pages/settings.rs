//! Settings: one grouped list with a section index; Ctrl+F filters rows.

use egui::{Align, Id, Rect, Sense, Ui, pos2, vec2};

use super::PageCtx;
use crate::system::AutostartStatus;
use crate::ui::actions::UiAction;
use crate::ui::anim::MotionPref;
use crate::ui::fonts::{Ts, font};
use crate::ui::icons::Icon;
use crate::ui::layout::{self, Breakpoint};
use crate::ui::overlays::{Modal, ToastKind, shortcuts_table};
use crate::ui::theme::{R_MD, ThemePref, pal};
use crate::ui::widgets::card::{card, divider};
use crate::ui::widgets::controls::{self, Button, Kind, Size, segmented, setting_row, toggle_row};
use crate::ui::widgets::text;

const SECTIONS: [&str; 8] = ["General", "Appearance", "Safety", "Hardware", "Lighting", "Keyboard", "Updates", "About"];

/// Row visibility under the filter.
pub fn row_matches(filter: &str, label: &str, helper: &str) -> bool {
    let f = filter.trim().to_lowercase();
    f.is_empty() || label.to_lowercase().contains(&f) || helper.to_lowercase().contains(&f)
}

pub fn show(ui: &mut Ui, pc: &mut PageCtx) {
    let p = pal(ui);
    ui.add_space(4.0);
    let wide = pc.bp != Breakpoint::Narrow;
    // Search + (narrow) index chips
    ui.horizontal_wrapped(|ui| {
        let r = controls::search_field(
            ui,
            Id::new("settings_search"),
            &mut pc.ui.settings_filter,
            "Search settings…",
            260.0,
        );
        if pc.ui.focus_search {
            r.request_focus();
            pc.ui.focus_search = false;
        }
        if !wide {
            for (i, s) in SECTIONS.iter().enumerate() {
                if controls::filter_chip(ui, s, false).clicked() {
                    pc.ui.settings_jump = Some(i);
                }
            }
        }
    });
    ui.add_space(pc.bp.gap());
    if wide {
        let avail = ui.available_width();
        let form = (avail - 196.0).min(680.0);
        layout::row(ui, Id::new("settings_split"), &[180.0, form], 16.0, |i, ui, _| {
            if i == 0 {
                index(ui, pc);
            } else {
                form_body(ui, pc);
            }
            0.0
        });
    } else {
        form_body(ui, pc);
    }
    let _ = p;
}

fn index(ui: &mut Ui, pc: &mut PageCtx) {
    let p = pal(ui);
    let current: usize = ui.data(|d| d.get_temp(Id::new("settings_current"))).unwrap_or(0);
    for (i, s) in SECTIONS.iter().enumerate() {
        let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
        let sel = i == current;
        if sel {
            ui.painter().rect_filled(r, R_MD, p.accent_subtle);
            ui.painter().rect_filled(
                Rect::from_min_size(pos2(r.left(), r.center().y - 8.0), vec2(3.0, 16.0)),
                2,
                p.accent,
            );
        } else if resp.hovered() {
            ui.painter().rect_filled(r, R_MD, p.surface2);
        }
        ui.painter().text(
            pos2(r.left() + 14.0, r.center().y),
            egui::Align2::LEFT_CENTER,
            *s,
            font(Ts::Label),
            if sel { p.text1 } else { p.text2 },
        );
        if resp.clicked() {
            pc.ui.settings_jump = Some(i);
        }
    }
}

struct Form {
    filter: String,
}

impl Form {
    fn show(&self, label: &str, helper: &str) -> bool {
        row_matches(&self.filter, label, helper)
    }
}

fn section(ui: &mut Ui, pc: &mut PageCtx, i: usize, any: bool, body: impl FnOnce(&mut Ui, &mut PageCtx)) {
    if !any {
        return;
    }
    ui.add_space(8.0);
    let r = text::overline(ui, SECTIONS[i]).rect;
    // Index highlight: the last section whose header is above the viewport's upper third.
    let clip = ui.clip_rect();
    if r.top() < clip.top() + clip.height() * 0.3 {
        ui.data_mut(|d| d.insert_temp(Id::new("settings_current"), i));
    }
    if pc.ui.settings_jump == Some(i) {
        ui.scroll_to_rect(r.expand(8.0), Some(Align::TOP));
        pc.ui.settings_jump = None;
    }
    ui.add_space(4.0);
    card(ui, |ui| body(ui, pc));
    ui.add_space(8.0);
}

fn form_body(ui: &mut Ui, pc: &mut PageCtx) {
    let f = Form { filter: pc.ui.settings_filter.clone() };
    ui.data_mut(|d| d.insert_temp(Id::new("settings_current"), 0usize));
    let any = |rows: &[(&str, &str)]| rows.iter().any(|(l, h)| f.show(l, h));

    // ---- General ----
    let gen_rows = [
        ("Start with Windows", "Runs elevated at sign-in, no UAC prompt"),
        ("Start minimized to tray", ""),
        ("Close button keeps FanRGB in the tray", "Fans stay controlled in the background"),
        ("Temperature unit", "°C °F fahrenheit celsius"),
        ("Sensor update interval", "poll"),
    ];
    section(ui, pc, 0, any(&gen_rows), |ui, pc| {
        if f.show(gen_rows[0].0, gen_rows[0].1) {
            autostart_row(ui, pc);
            divider(ui);
        }
        if f.show(gen_rows[1].0, gen_rows[1].1) {
            toggle_row(ui, gen_rows[1].0, "", &mut pc.cfg.general.start_minimized);
            divider(ui);
        }
        if f.show(gen_rows[2].0, gen_rows[2].1) {
            toggle_row(ui, gen_rows[2].0, gen_rows[2].1, &mut pc.cfg.general.close_to_tray);
            divider(ui);
        }
        if f.show(gen_rows[3].0, gen_rows[3].1) {
            setting_row(ui, gen_rows[3].0, "", 140.0, |ui| {
                segmented(
                    ui,
                    Id::new("units"),
                    &mut pc.cfg.general.fahrenheit,
                    &[(false, "°C"), (true, "°F")],
                    Some(120.0),
                );
            });
            divider(ui);
        }
        if f.show(gen_rows[4].0, gen_rows[4].1) {
            let mut v = pc.cfg.general.poll_ms as f32;
            setting_row(ui, gen_rows[4].0, "How often sensors are read and fans adjusted", 300.0, |ui| {
                let c = pal(ui).text1;
                text::label(ui, format!("{} ms", pc.cfg.general.poll_ms), Ts::Label, c);
                if controls::slider(ui, Id::new("poll"), &mut v, 250.0..=5000.0, 250.0, 200.0).changed() {
                    pc.cfg.general.poll_ms = v as u32;
                }
            });
        }
    });

    // ---- Appearance ----
    let app = [
        ("Theme", "dark light system"),
        ("Interface scale", "zoom size"),
        ("Reduce motion", "animations"),
        ("Spinning fan icons", "animation spin cpu"),
        ("Show my PC on Overview", "hero render rig picture board map"),
    ];
    section(ui, pc, 1, any(&app), |ui, pc| {
        if f.show(app[0].0, app[0].1) {
            setting_row(ui, "Theme", "", 260.0, |ui| {
                segmented(
                    ui,
                    Id::new("theme"),
                    &mut pc.prefs.theme,
                    &[(ThemePref::System, "System"), (ThemePref::Dark, "Dark"), (ThemePref::Light, "Light")],
                    Some(240.0),
                );
            });
            divider(ui);
        }
        if f.show(app[1].0, app[1].1) {
            let mut z = pc.prefs.zoom * 100.0;
            setting_row(ui, "Interface scale", "Multiplies with the Windows display scale", 300.0, |ui| {
                let c = pal(ui).text1;
                text::label(ui, format!("{z:.0} %"), Ts::Label, c);
                if controls::slider(ui, Id::new("zoom"), &mut z, 90.0..=150.0, 5.0, 200.0).drag_stopped()
                    || ui.input(|i| i.pointer.any_released())
                {
                    pc.prefs.zoom = z / 100.0;
                }
            });
            divider(ui);
        }
        if f.show(app[2].0, app[2].1) {
            setting_row(ui, "Reduce motion", "Auto follows Windows “Animation effects”", 260.0, |ui| {
                segmented(
                    ui,
                    Id::new("motion"),
                    &mut pc.prefs.motion,
                    &[(MotionPref::Auto, "Auto"), (MotionPref::On, "On"), (MotionPref::Off, "Off")],
                    Some(240.0),
                );
            });
            divider(ui);
        }
        if f.show(app[3].0, app[3].1) {
            toggle_row(
                ui,
                app[3].0,
                "Fan glyphs turn with the measured speed (redraws ~30×/s while focused: more CPU)",
                &mut pc.prefs.spin_fans,
            );
            divider(ui);
        }
        if f.show(app[4].0, app[4].1) {
            toggle_row(
                ui,
                app[4].0,
                "The rendered PC with its LEDs lit live. Off shows the board map instead (for a PC that doesn't look like this one).",
                &mut pc.prefs.show_hero,
            );
        }
    });

    // ---- Safety ----
    let saf = [
        ("CPU emergency temperature", "critical all fans 100"),
        ("GPU emergency temperature", "critical"),
        ("Crash watchdog", "safety"),
        ("Return all fans to BIOS now", "release emergency"),
    ];
    section(ui, pc, 2, any(&saf), |ui, pc| {
        let p = pal(ui);
        if f.show(saf[0].0, saf[0].1) {
            controls::slider_row(
                ui,
                Id::new("cpu_crit"),
                "CPU emergency",
                &mut pc.cfg.general.cpu_critical,
                70.0..=95.0,
                1.0,
                " °C",
                0,
            );
            text::caption(ui, "Every controlled fan runs at 100 % at or above this CPU temperature.");
            ui.add_space(6.0);
            divider(ui);
        }
        if f.show(saf[1].0, saf[1].1) {
            controls::slider_row(
                ui,
                Id::new("gpu_crit"),
                "GPU emergency",
                &mut pc.cfg.general.gpu_critical,
                70.0..=100.0,
                1.0,
                " °C",
                0,
            );
            ui.add_space(6.0);
            divider(ui);
        }
        if f.show(saf[2].0, saf[2].1) {
            let active = crate::system::watchdog_active();
            setting_row(
                ui,
                "Crash watchdog",
                "Always on: a helper process returns fans to the BIOS if FanRGB is killed",
                120.0,
                |ui| {
                    controls::dot_chip(
                        ui,
                        if active { "Running" } else { "Idle" },
                        if active { controls::Tone::Success } else { controls::Tone::Neutral },
                    );
                },
            );
            divider(ui);
            toggle_row(
                ui,
                "Alert notifications",
                "Cooling failure, critical temperature and lost sensors also pop up from the tray icon",
                &mut pc.cfg.general.notifications,
            );
            divider(ui);
        }
        if f.show(saf[3].0, saf[3].1) {
            setting_row(
                ui,
                "Return all fans to BIOS now",
                "Ctrl+Shift+B · pick a profile or Resume to take control again",
                200.0,
                |ui| {
                    if Button::new(Kind::Danger, "Return to BIOS").size(Size::Sm).icon(Icon::Power).show(ui).clicked() {
                        pc.ui.pending.push(UiAction::EmergencyRelease);
                    }
                },
            );
            ui.add_space(4.0);
            text::caption3(
                ui,
                "Always active: losing a temperature sensor returns that fan to BIOS control, and exiting FanRGB restores the exact BIOS fan settings.",
            );
        }
        let _ = p;
    });

    // ---- Hardware ----
    let hw = [
        ("Show all fan headers", "channels the board may not route"),
        ("Show onboard LED zones", "LED1 LED3"),
        ("Board map on Overview", "diagram"),
        ("Learning controller", "ai quiet ceiling probes power"),
    ];
    section(ui, pc, 3, any(&hw), |ui, pc| {
        if f.show(hw[0].0, hw[0].1) {
            toggle_row(
                ui,
                hw[0].0,
                "Also list channels your board may not route to a header",
                &mut pc.cfg.general.show_all_headers,
            );
            divider(ui);
        }
        if f.show(hw[1].0, hw[1].1) {
            toggle_row(ui, hw[1].0, "Advanced RGB zones on the board itself", &mut pc.cfg.rgb.show_all_zones);
            divider(ui);
        }
        if f.show(hw[2].0, hw[2].1) {
            toggle_row(ui, hw[2].0, "Show the vector board diagram on Overview", &mut pc.prefs.show_board_map);
            divider(ui);
        }
        if f.show("Learning controller", "ai quiet ceiling probes power") {
            toggle_row(
                ui,
                "Learn quiet ceilings",
                "Remember how quiet each workload and time of day can be, and prefer staying under it",
                &mut pc.cfg.ai.quiet_ceiling,
            );
            divider(ui);
            toggle_row(
                ui,
                "Allow learning probes",
                "Rare, small fan-speed probes (+8 % for 2 min) teach the model the fan effect faster",
                &mut pc.cfg.ai.probes,
            );
            divider(ui);
            controls::slider_row(
                ui,
                Id::new("cpu_ppt"),
                "CPU power limit",
                &mut pc.cfg.ai.cpu_ppt_w,
                35.0..=250.0,
                1.0,
                " W",
                0,
            );
            controls::slider_row(
                ui,
                Id::new("gpu_tbp"),
                "GPU board power",
                &mut pc.cfg.ai.gpu_tbp_w,
                50.0..=600.0,
                5.0,
                " W",
                0,
            );
            text::caption3(ui, "Power references for workload detection (Ryzen 5 7500F: 88 W, Arc B580: 190 W).");
        }
    });

    // ---- Lighting ----
    let li = [
        ("Frame rate", "fps lighting"),
        ("Overheat alert", "flash red cpu"),
        ("LED_C channel", "slot 12 V rgb header"),
    ];
    section(ui, pc, 4, any(&li), |ui, pc| {
        if f.show(li[0].0, li[0].1) {
            let mut fps = pc.cfg.rgb.fps as f32;
            if controls::slider_row(ui, Id::new("fps"), "Frame rate", &mut fps, 5.0..=60.0, 1.0, " fps", 0) {
                pc.cfg.rgb.fps = fps as u32;
            }
            if pc.rgb.fps_cap > 0.0 && (pc.rgb.fps_cap as u32) < pc.cfg.rgb.fps {
                text::caption(ui, format!("Limited to {:.0} fps by the controller's USB throughput.", pc.rgb.fps_cap));
            }
            ui.add_space(4.0);
            divider(ui);
        }
        if f.show(li[1].0, li[1].1) {
            toggle_row(
                ui,
                "Overheat alert",
                "Flash all LEDs red when the CPU gets too hot",
                &mut pc.cfg.rgb.temp_alert,
            );
            if pc.cfg.rgb.temp_alert {
                controls::slider_row(
                    ui,
                    Id::new("alert_c"),
                    "Alert at CPU",
                    &mut pc.cfg.rgb.temp_alert_c,
                    70.0..=100.0,
                    1.0,
                    " °C",
                    0,
                );
            }
            divider(ui);
        }
        if f.show(li[2].0, li[2].1) {
            ledc_slot_row(ui, pc);
        }
    });

    // ---- Keyboard ----
    section(ui, pc, 5, f.show("Keyboard shortcuts", "keys hotkeys"), |ui, _| {
        shortcuts_table(ui);
    });

    // ---- Updates ----
    section(ui, pc, 6, f.show("Updates", "update version token github check install"), updates_body);

    // ---- About ----
    section(ui, pc, 7, f.show("About FanRGB", "version licences licenses notice source folders reset"), |ui, pc| {
        let p = pal(ui);
        text::label(ui, format!("FanRGB {}", env!("CARGO_PKG_VERSION")), Ts::Headline, p.text1);
        text::para(
            ui,
            "Built on research from LibreHardwareMonitor (Super I/O, AMD SMN), the Linux it87 driver (Gigabyte Smart Fan), OpenRGB (RGB Fusion 2 protocol) and the PawnIO signed driver. Some protocol and Smart Fan behaviour was learnt by observing GIGABYTE's own software, for interoperability; no GIGABYTE code is included. Typeface: Inter (SIL OFL 1.1).",
            Ts::Caption,
            p.text2,
        );
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            if Button::new(Kind::Secondary, "Config folder").size(Size::Sm).icon(Icon::Folder).show(ui).clicked() {
                let _ = crate::util::win::open_folder(&crate::util::paths::config_dir());
            }
            if Button::new(Kind::Secondary, "Data & logs folder").size(Size::Sm).icon(Icon::Folder).show(ui).clicked() {
                let _ = crate::util::win::open_folder(&crate::util::paths::data_dir());
            }
            if Button::new(Kind::Secondary, "Licences").size(Size::Sm).icon(Icon::Report).show(ui).clicked() {
                pc.ui.modal = Some(Modal::Licenses { doc: 0 });
            }
            if Button::new(Kind::Danger, "Reset settings…").size(Size::Sm).show(ui).clicked() {
                pc.ui.modal = Some(Modal::ConfirmResetSettings);
            }
        });
    });
}

use super::updates::ago;

/// Settings › Updates (minimal: token, automatic checks, Check now, Install).
fn updates_body(ui: &mut Ui, pc: &mut PageCtx) {
    use crate::updater::Status;
    let p = pal(ui);
    let st = pc.ui.updates.state().clone();
    let busy = pc.ui.updates.busy();
    if st.status == Status::Disabled {
        text::caption(ui, "Updates are off in demo mode.");
        if controls::link(ui, "Open Updates").clicked() {
            pc.ui.pending.push(crate::ui::actions::UiAction::Navigate(crate::ui::Page::Updates));
        }
        return;
    }
    // Token (optional: without one the check asks GitHub anonymously)
    if st.token_saved {
        setting_row(ui, "Update access token", "Saved for this Windows account, encrypted with DPAPI", 120.0, |ui| {
            if Button::new(Kind::Secondary, "Forget").size(Size::Sm).show(ui).clicked() {
                match pc.ui.updates.forget_token() {
                    Ok(()) => pc.ui.toast(ToastKind::Info, "Removing the update token…"),
                    Err(e) => pc.ui.toast(ToastKind::Error, e),
                }
            }
        });
    } else {
        setting_row(
            ui,
            "Update access token (optional)",
            &format!(
                "Only needed while the repository is private, or to raise GitHub's rate limit: a read-only token \
                 for {} (fine-grained, Contents: read)",
                crate::updater::REPO
            ),
            300.0,
            |ui| {
                let id = Id::new("update_token");
                let te = egui::TextEdit::singleline(&mut pc.ui.update_token_draft)
                    .id(id)
                    .password(true)
                    .hint_text("github_pat_…")
                    .desired_width(200.0);
                ui.add(te);
                if Button::new(Kind::Primary, "Save").size(Size::Sm).show(ui).clicked() {
                    match pc.ui.updates.save_token(&mut pc.ui.update_token_draft) {
                        Ok(()) => pc.ui.toast(ToastKind::Info, "Saving the update token · then checking for updates"),
                        Err(e) => pc.ui.toast(ToastKind::Error, e),
                    }
                    // Drop the field's undo history (it holds copies of the text).
                    ui.ctx().data_mut(|d| d.remove::<egui::text_edit::TextEditState>(id));
                }
            },
        );
    }
    divider(ui);
    toggle_row(
        ui,
        "Check for updates automatically",
        "Daily, from the FanRGB GitHub repository only; no account needed, no telemetry",
        &mut pc.cfg.updates.auto_check,
    );
    divider(ui);
    if let Some(svc) = crate::drivers::service()
        && toggle_row(
            ui,
            "Include drivers in the check",
            "Ask Windows Update once a day (sends this PC's hardware IDs to Microsoft, as Windows Update does). Inform only.",
            &mut pc.cfg.drivers.auto_check,
        )
    {
        svc.set_auto(pc.cfg.drivers.auto_check);
    }
    divider(ui);
    // Status + actions
    let status = match st.status {
        Status::Checking => "Checking…".to_string(),
        Status::Installing => "Installing…".to_string(),
        Status::UpToDate => format!("FanRGB {} is up to date", st.current),
        Status::Available => {
            format!("FanRGB {} is available", st.available.as_ref().map(|a| a.version.as_str()).unwrap_or("?"))
        }
        Status::VerifiedOnly => "Update verified; a development build is not replaced".to_string(),
        _ => format!("FanRGB {}", st.current),
    };
    let checked =
        st.last_checked.map(|t| format!("Last checked {}", ago(t))).unwrap_or_else(|| "Not checked yet".into());
    setting_row(ui, &status, &checked, 200.0, |ui| {
        if st.status == Status::Available
            && Button::new(Kind::Primary, "Install and restart").size(Size::Sm).show(ui).clicked()
        {
            pc.ui.updates.install(pc.shared);
        }
        if Button::new(Kind::Secondary, "Check now").size(Size::Sm).loading(busy).show(ui).clicked() && !busy {
            pc.ui.updates.check_now();
        }
    });
    if st.status == Status::Available && !st.notes.is_empty() {
        text::para(ui, st.notes.clone(), Ts::Caption, p.text2);
    }
    if let Some((code, detail)) = &st.error {
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            if controls::code_chip(ui, &code.id()).clicked() {
                ui.ctx().copy_text(code.id());
            }
            text::label(ui, code.title(), Ts::Label, p.text1);
        });
        text::caption3(ui, code.help());
        if !detail.is_empty() {
            text::caption3(ui, detail.clone());
        }
    }
    if let Some(r) = &st.last_result {
        text::caption3(ui, format!("Last update: {r}"));
    }
}

fn autostart_row(ui: &mut Ui, pc: &mut PageCtx) {
    let p = pal(ui);
    let label = "Start with Windows";
    match pc.ui.autostart_st.clone() {
        None => {
            setting_row(ui, label, "Checking Task Scheduler…", 60.0, |ui| {
                controls::spinner(ui.painter(), ui.max_rect().center(), 7.0, p.text2, ui.input(|i| i.time));
            });
        }
        Some(AutostartStatus::Enabled) | Some(AutostartStatus::Disabled) => {
            let mut on = pc.ui.autostart_st == Some(AutostartStatus::Enabled);
            if toggle_row(ui, label, "Runs elevated at sign-in through Task Scheduler (no UAC prompt)", &mut on) {
                match crate::system::set_autostart(on) {
                    Ok(()) => pc.ui.toast(
                        ToastKind::Success,
                        if on { "FanRGB starts with Windows" } else { "Autostart disabled" },
                    ),
                    Err(e) => pc.ui.toast_code(
                        ToastKind::Error,
                        format!("Autostart failed: {e:#}"),
                        crate::error::code_of(&e),
                    ),
                }
                pc.ui.refresh_autostart();
            }
        }
        Some(AutostartStatus::NeedsInstall) => {
            setting_row(
                ui,
                label,
                "Autostart runs elevated, so FanRGB must be installed to Program Files first.",
                190.0,
                |ui| {
                    if Button::new(Kind::Primary, "Install and enable").size(Size::Sm).show(ui).clicked() {
                        install_autostart(pc);
                    }
                },
            );
        }
        Some(AutostartStatus::Insecure(cmd)) => {
            setting_row(
                ui,
                label,
                &format!("The startup task points outside the protected install folder ({cmd})."),
                250.0,
                |ui| {
                    if Button::new(Kind::Primary, "Reinstall").size(Size::Sm).show(ui).clicked() {
                        install_autostart(pc);
                    }
                    if Button::new(Kind::Secondary, "Disable").size(Size::Sm).show(ui).clicked() {
                        if let Err(e) = crate::system::set_autostart(false) {
                            pc.ui.toast(ToastKind::Error, format!("Could not remove the task: {e:#}"));
                        }
                        pc.ui.refresh_autostart();
                    }
                },
            );
        }
        Some(AutostartStatus::Unavailable(e)) => {
            setting_row(ui, label, &format!("Task Scheduler is unavailable: {e}"), 100.0, |ui| {
                if Button::new(Kind::Ghost, "Retry").size(Size::Sm).show(ui).clicked() {
                    pc.ui.refresh_autostart();
                }
            });
        }
    }
}

fn install_autostart(pc: &mut PageCtx) {
    match crate::system::install_and_enable_autostart() {
        Ok(path) => pc.ui.toast(ToastKind::Success, format!("Installed to {} · starts with Windows", path.display())),
        Err(e) => pc.ui.toast_code(ToastKind::Error, format!("Install failed: {e:#}"), crate::error::code_of(&e)),
    }
    pc.ui.refresh_autostart();
}

/// LED_C is wired to controller division 1 or 4 depending on board revision; "Identify" tells which.
pub fn ledc_slot_row(ui: &mut Ui, pc: &mut PageCtx) {
    setting_row(
        ui,
        "LED_C channel",
        "Board revisions differ. Use Identify on Lighting › Zone setup to see which one lights up.",
        260.0,
        |ui| {
            segmented(
                ui,
                Id::new("ledc_slot"),
                &mut pc.cfg.rgb.ledc_slot,
                &[(None, "Both"), (Some(1), "Slot 1"), (Some(4), "Slot 4")],
                Some(240.0),
            );
        },
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn filter_rows() {
        assert!(super::row_matches("", "Theme", ""));
        assert!(super::row_matches("them", "Theme", ""));
        assert!(super::row_matches("zoom", "Interface scale", "zoom size"));
        assert!(!super::row_matches("xyz", "Theme", "dark"));
    }
}
