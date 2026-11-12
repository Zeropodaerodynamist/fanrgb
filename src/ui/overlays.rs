//! Global overlays: toasts, modal dialogs (install PawnIO, power-cycle guide, conflicting apps,
//! confirmations, rename, calibration wizard, shortcuts, licences) and the command palette.

use std::sync::Arc;

use egui::{Align2, Context, Frame, Id, Key, Margin, Order, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use super::actions::{PaletteEntry, UiAction, fuzzy_score};
use super::fonts::{Ts, font, tab};
use super::icons::{self, Icon};
use super::theme::{R_LG, R_MD, R_XL, pal, pal_ctx, shadow_e2, shadow_e3};
use super::widgets::controls::{Button, Kind, kbd};
use super::widgets::text;
use super::{UiState, anim};
use crate::config::Config;
use crate::engine::{Command, EngineState, Shared};
use crate::error::ErrorCode;

// ---------------------------------------------------------------------------------------
// Toasts

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Success,
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug)]
pub struct Toast {
    pub id: u64,
    pub kind: ToastKind,
    pub text: String,
    pub code: Option<ErrorCode>,
    pub action: Option<(String, UiAction)>,
    /// Remaining seconds (paused while hovered).
    pub remaining: f32,
    pub closing: Option<f64>,
}

impl ToastKind {
    pub fn lifetime(self) -> f32 {
        match self {
            ToastKind::Success | ToastKind::Info => 3.5,
            ToastKind::Warning => 6.0,
            ToastKind::Error => 8.0,
        }
    }
}

pub fn show_toasts(ctx: &Context, st: &mut UiState) {
    if st.toasts.is_empty() {
        return;
    }
    let p = pal_ctx(ctx);
    let now = ctx.input(|i| i.time);
    let dt = ctx.input(|i| i.stable_dt).min(0.1);
    while st.toasts.len() > 3 {
        st.toasts.pop_front();
    }
    let mut actions = Vec::new();
    let mut remove = Vec::new();
    let screen = ctx.content_rect();
    let mut y = screen.bottom() - 16.0;
    let reduce = anim::reduce_motion(ctx);
    let count = st.toasts.len();
    for k in (0..count).rev() {
        let t = &mut st.toasts[k];
        let fade_in = anim::toggle(ctx, Id::new(("toast_in", t.id)), true, anim::SLOW);
        let out = t.closing.map(|c| ((now - c) as f32 / anim::SLOW).clamp(0.0, 1.0)).unwrap_or(0.0);
        if out >= 1.0 || (reduce && t.closing.is_some()) {
            remove.push(t.id);
            continue;
        }
        let alpha = fade_in * (1.0 - out);
        let slide = (1.0 - fade_in) * 8.0 + out * 8.0;
        let w = 360.0f32.min(screen.width() - 32.0);
        let (icon, color) = match t.kind {
            ToastKind::Success => (Icon::CheckCircle, p.success),
            ToastKind::Info => (Icon::Info, p.info),
            ToastKind::Warning => (Icon::Warning, p.warning),
            ToastKind::Error => (Icon::Error, p.danger),
        };
        let id = Id::new(("toast", t.id));
        let area = egui::Area::new(id)
            .order(Order::Foreground)
            .fixed_pos(pos2(screen.right() - 16.0 - w, y + slide))
            .pivot(Align2::LEFT_BOTTOM)
            .interactable(true);
        let resp = area.show(ctx, |ui| {
            ui.set_opacity(alpha);
            Frame::new()
                .fill(p.surface1)
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(R_LG)
                .shadow(shadow_e3(p))
                .inner_margin(Margin { left: 12, right: 8, top: 10, bottom: 10 })
                .show(ui, |ui| {
                    ui.set_width(w - 20.0);
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
                        icons::paint(ui.painter(), r, icon, color);
                        let text_w = ui.available_width() - 40.0 - if t.action.is_some() { 64.0 } else { 0.0 };
                        ui.vertical(|ui| {
                            ui.set_max_width(text_w);
                            text::para(ui, &t.text, Ts::Body, p.text1);
                            if let Some(c) = t.code {
                                text::label(ui, c.id(), Ts::Mono, p.text3);
                            }
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if super::widgets::controls::icon_button_sized(ui, Icon::Close, "Dismiss", 24.0, None)
                                .clicked()
                            {
                                t.closing.get_or_insert(now);
                            }
                            if let Some((label, a)) = &t.action
                                && Button::new(Kind::Ghost, label)
                                    .size(super::widgets::controls::Size::Sm)
                                    .show(ui)
                                    .clicked()
                            {
                                actions.push(a.clone());
                                t.closing.get_or_insert(now);
                            }
                        });
                    });
                })
                .response
                .rect
        });
        let hovered = resp.response.contains_pointer()
            || resp.inner.contains(ctx.pointer_hover_pos().unwrap_or(pos2(-1.0, -1.0)));
        if !hovered && t.closing.is_none() {
            t.remaining -= dt;
            if t.remaining <= 0.0 {
                t.closing = Some(now);
            }
        }
        y -= resp.inner.height() + 8.0;
    }
    st.toasts.retain(|t| !remove.contains(&t.id));
    st.pending.extend(actions);
    if !st.toasts.is_empty() {
        ctx.request_repaint_after(std::time::Duration::from_millis(if st.toasts.iter().any(|t| t.closing.is_some()) {
            16
        } else {
            200
        }));
    }
}

// ---------------------------------------------------------------------------------------
// Modals

#[derive(Clone, Debug, PartialEq)]
pub enum Modal {
    InstallPawnIo,
    PowerCycle,
    Conflicts,
    ConfirmResetModel {
        key: Option<String>,
        what: String,
    },
    ConfirmResetSettings,
    ConfirmDeleteProfile(usize),
    RenameProfile {
        index: usize,
        buf: String,
    },
    Calibrate {
        fan: usize,
        name: String,
        started: bool,
    },
    /// "Identify your fans": the header roles wizard.
    IdentifyHeaders {
        started: bool,
    },
    /// Release notes of the available update.
    WhatsNew,
    Shortcuts,
    /// The licence texts that come with fanrgb.exe; `doc` indexes `licences::DOCS`.
    Licenses {
        doc: usize,
    },
}

pub struct ModalEnv<'a> {
    pub shared: &'a Arc<Shared>,
    pub st: &'a EngineState,
    pub cfg: &'a mut Config,
}

fn modal_frame(ctx: &Context) -> Frame {
    let p = pal_ctx(ctx);
    Frame::new()
        .fill(p.surface1)
        .corner_radius(R_XL)
        .inner_margin(Margin::same(24))
        .shadow(shadow_e2(p))
        .stroke(Stroke::new(1.0, p.border))
}

/// Header with title and close button; returns true if closed.
fn modal_header(ui: &mut Ui, title: &str) -> bool {
    let p = pal(ui);
    let mut close = false;
    ui.horizontal(|ui| {
        text::label(ui, title, Ts::Headline, p.text1);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            close = super::widgets::controls::icon_button_sized(ui, Icon::Close, "Close (Esc)", 28.0, None).clicked();
        });
    });
    ui.add_space(4.0);
    close
}

fn footer(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.add_space(16.0);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        add(ui);
    });
}

fn numbered_step(ui: &mut Ui, n: usize, icon: Icon, title: &str, body: &str) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 18.0, p.surface2);
        icons::paint(ui.painter(), Rect::from_center_size(r.center(), vec2(18.0, 18.0)), icon, p.text1);
        ui.painter().circle_filled(r.left_top() + vec2(4.0, 4.0), 8.0, p.accent);
        ui.painter().text(
            r.left_top() + vec2(4.0, 4.0),
            Align2::CENTER_CENTER,
            n.to_string(),
            tab(Ts::Caption),
            p.on_accent,
        );
        ui.add_space(6.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            text::para(ui, title, Ts::BodyStrong, p.text1);
            text::para(ui, body, Ts::Caption, p.text2);
        });
    });
    ui.add_space(6.0);
}

/// Shows the current modal. Returns the modal to keep (None = closed).
pub fn show_modal(ctx: &Context, ui_state: &mut UiState, env: &mut ModalEnv, mut modal: Modal) -> Option<Modal> {
    let p = pal_ctx(ctx);
    let width: f32 = match modal {
        Modal::Calibrate { .. } | Modal::IdentifyHeaders { .. } | Modal::WhatsNew | Modal::Shortcuts => 560.0,
        // Room for 80 monospace columns, the width most licence texts are written for.
        Modal::Licenses { .. } => 640.0,
        _ => 440.0,
    };
    let mut keep = true;
    let opacity = anim::toggle(ctx, Id::new("modal_fade"), true, anim::SLOW);
    let resp = egui::Modal::new(Id::new("fanrgb_modal")).frame(modal_frame(ctx)).backdrop_color(p.scrim).show(ctx, |ui| {
        ui.multiply_opacity(opacity);
        ui.set_width(width.min(ctx.content_rect().width() - 64.0));
        let enter = ui.input(|i| i.key_pressed(Key::Enter));
        match &mut modal {
            Modal::InstallPawnIo => {
                keep &= !modal_header(ui, "Install the PawnIO driver");
                text::para(ui, "Fan control and motherboard/CPU temperatures need PawnIO, a small open-source driver signed by Microsoft. LibreHardwareMonitor and FanControl use it too; it replaces the vulnerable WinRing0 driver.", Ts::Body, p.text2);
                ui.add_space(12.0);
                let running = ui_state.winget.is_some();
                ui.horizontal(|ui| {
                    if Button::new(Kind::Primary, if running { "Installing…" } else { "Install with winget" }).icon(Icon::Download).loading(running).show(ui).clicked() && !running {
                        super::issues::start_winget_install(ui_state);
                    }
                    if Button::new(Kind::Secondary, "Download from pawnio.eu").icon(Icon::ExternalLink).show(ui).clicked() {
                        let _ = crate::util::win::open_url("https://pawnio.eu/");
                    }
                });
                ui.add_space(8.0);
                if ui_state.winget_done {
                    super::widgets::card::banner(ui, super::widgets::card::Sev::Info, "Installer finished", "Restart FanRGB to load the driver.", |ui| {
                        if Button::new(Kind::Primary, "Restart FanRGB").size(super::widgets::controls::Size::Sm).show(ui).clicked() {
                            ui_state.pending.push(UiAction::RestartApp);
                        }
                    });
                } else {
                    text::caption3(ui, "A console window opens and runs winget install --id namazso.PawnIO with your normal rights; Windows asks for permission to install the driver.");
                }
                footer(ui, |ui| {
                    if Button::new(Kind::Secondary, "Close").show(ui).clicked() {
                        keep = false;
                    }
                });
            }
            Modal::PowerCycle => {
                keep &= !modal_header(ui, "Reset the RGB controller");
                text::para(ui, "The RGB controller (USB error 43) only recovers when it loses standby power. A normal restart is not enough.", Ts::Body, p.text2);
                ui.add_space(12.0);
                numbered_step(ui, 1, Icon::Power, "Shut down Windows", "Use Start › Power › Shut down (not Restart).");
                numbered_step(ui, 2, Icon::Plug, "Switch the PSU off", "Flip the switch on the back of the power supply, or unplug the power cable.");
                numbered_step(ui, 3, Icon::Power, "Hold the power button for 15 s", "This drains the remaining standby power from the board.");
                numbered_step(ui, 4, Icon::Refresh, "Wait 30 s, then boot", "Switch the PSU back on and start the PC. FanRGB reconnects automatically.");
                footer(ui, |ui| {
                    if Button::new(Kind::Primary, "Done").show(ui).clicked() || enter {
                        keep = false;
                    }
                });
            }
            Modal::Conflicts => {
                keep &= !modal_header(ui, "Other fan or RGB apps");
                if env.st.conflicts.is_empty() {
                    text::para(ui, "No conflicting software is running right now.", Ts::Body, p.text2);
                } else {
                    text::para(ui, "These programs talk to the same hardware and can fight with FanRGB. Close them or turn off their startup entries. FanRGB never closes apps for you.", Ts::Body, p.text2);
                    ui.add_space(8.0);
                    for (name, why) in &env.st.conflicts {
                        super::widgets::card::card(ui, |ui| {
                            text::label(ui, name, Ts::BodyStrong, p.text1);
                            text::para(ui, format!("It {why}."), Ts::Caption, p.text2);
                        });
                        ui.add_space(6.0);
                    }
                }
                footer(ui, |ui| {
                    if Button::new(Kind::Secondary, "Close").show(ui).clicked() {
                        keep = false;
                    }
                    if Button::new(Kind::Secondary, "Open Task Manager").icon(Icon::ExternalLink).show(ui).clicked() {
                        // Through the unelevated shell like every other "open" (util::win): started
                        // directly, Task Manager would inherit this process's administrator token and
                        // its "Run new task" would be an administrator prompt without UAC.
                        crate::util::win::shell_open(&crate::util::win::system32().join("Taskmgr.exe").to_string_lossy());
                    }
                });
            }
            Modal::ConfirmResetModel { key, what } => {
                keep &= !modal_header(ui, "Reset learned model?");
                text::para(ui, format!("{what} will be deleted and relearned from scratch. Fans follow the safe curve while it learns."), Ts::Body, p.text2);
                let key = key.clone();
                footer(ui, |ui| {
                    if Button::new(Kind::DangerFill, "Reset").show(ui).clicked() || enter {
                        super::issues::reset_model(env.shared, key);
                        ui_state.toast(ToastKind::Success, "Thermal model reset");
                        keep = false;
                    }
                    if Button::new(Kind::Secondary, "Cancel").show(ui).clicked() {
                        keep = false;
                    }
                });
            }
            Modal::ConfirmResetSettings => {
                keep &= !modal_header(ui, "Reset all settings?");
                text::para(ui, "Profiles, curves, lighting and preferences go back to defaults. Your current settings are backed up to config.json.bad first. Fan calibrations are kept.", Ts::Body, p.text2);
                footer(ui, |ui| {
                    if Button::new(Kind::DangerFill, "Reset settings").show(ui).clicked() {
                        ui_state.pending.push(UiAction::ResetSettings);
                        keep = false;
                    }
                    if Button::new(Kind::Secondary, "Cancel").show(ui).clicked() {
                        keep = false;
                    }
                });
            }
            Modal::ConfirmDeleteProfile(i) => {
                let name = env.cfg.profiles.get(*i).map(|p| p.name.clone()).unwrap_or_default();
                keep &= !modal_header(ui, &format!("Delete “{name}”?"));
                text::para(ui, "This profile and its per-fan settings are removed. This can't be undone.", Ts::Body, p.text2);
                let i = *i;
                footer(ui, |ui| {
                    if Button::new(Kind::DangerFill, "Delete").show(ui).clicked() {
                        if env.cfg.profiles.len() > 1 && i < env.cfg.profiles.len() {
                            env.cfg.profiles.remove(i);
                            if env.cfg.active_profile >= env.cfg.profiles.len() || env.cfg.active_profile == i {
                                env.cfg.active_profile = 0;
                            }
                            ui_state.toast(ToastKind::Success, format!("Deleted “{name}”"));
                        }
                        keep = false;
                    }
                    if Button::new(Kind::Secondary, "Cancel").show(ui).clicked() {
                        keep = false;
                    }
                });
            }
            Modal::RenameProfile { index, buf } => {
                keep &= !modal_header(ui, "Rename profile");
                let r = ui.add(egui::TextEdit::singleline(buf).font(font(Ts::Body)).desired_width(f32::INFINITY).margin(Margin::symmetric(10, 8)));
                if !r.has_focus() && ui.memory(|m| m.focused().is_none()) {
                    r.request_focus();
                }
                let (i, name) = (*index, buf.trim().to_string());
                footer(ui, |ui| {
                    if (Button::new(Kind::Primary, "Save").show(ui).clicked() || enter) && !name.is_empty() {
                        if let Some(p) = env.cfg.profiles.get_mut(i) {
                            p.name = name.clone();
                        }
                        keep = false;
                    }
                    if Button::new(Kind::Secondary, "Cancel").show(ui).clicked() {
                        keep = false;
                    }
                });
            }
            Modal::Calibrate { fan, name, started } => {
                keep &= !modal_header(ui, &format!("Calibrate {name}"));
                let running = env.st.calibration.as_ref().filter(|c| &c.fan_name == name);
                let fan_id = env.st.fans.iter().find(|f| f.index == *fan).map(|f| f.id.clone()).unwrap_or_default();
                match (running, *started) {
                    (None, false) => {
                        text::para(ui, "The fan will speed up and slow down for about 90 seconds while FanRGB measures its RPM at every speed, where it stops and where it starts again. Run it while the PC is idle.", Ts::Body, p.text2);
                        ui.add_space(8.0);
                        text::caption3(ui, "If the CPU gets warm, calibration stops automatically.");
                        let released = env.st.emergency_released;
                        if released {
                            ui.add_space(8.0);
                            text::para(ui, "Fans are on the BIOS curve right now (returned to BIOS). Resume fan control to calibrate.", Ts::Body, p.warning);
                        }
                        let f = *fan;
                        footer(ui, |ui| {
                            let start = ui
                                .add_enabled_ui(!released, |ui| Button::new(Kind::Primary, "Start calibration").show(ui))
                                .inner;
                            if !released && (start.clicked() || enter) {
                                env.shared.send(Command::Calibrate(f));
                                *started = true;
                            }
                            if Button::new(Kind::Secondary, "Cancel").show(ui).clicked() {
                                keep = false;
                            }
                        });
                    }
                    (Some(cv), _) => {
                        *started = true;
                        text::para(ui, &cv.status, Ts::Body, if cv.error.is_some() { p.warning } else { p.text1 });
                        ui.add_space(6.0);
                        super::widgets::controls::progress(ui, cv.progress, p.accent, ui.available_width(), 6.0);
                        ui.add_space(4.0);
                        text::label(ui, format!("Holding {:.0} % · {} measurements", cv.duty, cv.points.len()), Ts::Caption, p.text2);
                        ui.add_space(8.0);
                        rpm_duty_chart(ui, &cv.points, 160.0);
                        footer(ui, |ui| {
                            if !cv.done && Button::new(Kind::Secondary, "Cancel").show(ui).clicked() {
                                env.shared.send(Command::CancelCalibration);
                            }
                        });
                    }
                    (None, true) => match env.cfg.calibrations.get(&fan_id).cloned() {
                        Some(c) if c.max_rpm > 0.0 => {
                            text::label(ui, "Calibration finished", Ts::BodyStrong, p.success);
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                super::widgets::controls::stat(ui, "Max speed", &format!("{} rpm", super::widgets::thousands(c.max_rpm)), p.text1, Ts::Headline);
                                ui.add_space(24.0);
                                super::widgets::controls::stat(ui, "Stops below", &c.stop_duty.map(|d| format!("{d:.0} %")).unwrap_or("never".into()), p.text1, Ts::Headline);
                                ui.add_space(24.0);
                                super::widgets::controls::stat(ui, "Starts at", &c.start_duty.map(|d| format!("{d:.0} %")).unwrap_or("—".into()), p.text1, Ts::Headline);
                            });
                            ui.add_space(8.0);
                            let mut pts = c.points.clone();
                            pts.sort_by(|a, b| a[0].total_cmp(&b[0]));
                            rpm_duty_chart(ui, &pts, 140.0);
                            let suggest = c.stop_duty.map(|d| (d + 4.0).round().min(60.0));
                            let fid = fan_id.clone();
                            footer(ui, |ui| {
                                if let Some(s) = suggest
                                    && Button::new(Kind::Primary, &format!("Use min speed {s:.0} %")).show(ui).on_hover_text("Stop speed plus a safety margin").clicked() {
                                        let role = env.st.fans.iter().find(|f| f.id == fid).map(|f| f.role).unwrap_or(crate::model::FanRole::System);
                                        env.cfg.profile_mut().fan_mut(&fid, role).min_duty = s;
                                        ui_state.toast(ToastKind::Success, format!("Minimum speed set to {s:.0} %"));
                                        keep = false;
                                    }
                                if Button::new(Kind::Secondary, "Done").show(ui).clicked() {
                                    keep = false;
                                }
                            });
                        }
                        _ => {
                            text::para(ui, "Calibration did not complete. Check the log for details, make sure nothing blocks the fan, and try again.", Ts::Body, p.warning);
                            footer(ui, |ui| {
                                if Button::new(Kind::Secondary, "Close").show(ui).clicked() {
                                    keep = false;
                                }
                            });
                        }
                    },
                }
            }
            Modal::IdentifyHeaders { started } => {
                let closed = modal_header(ui, "Identify your fans");
                identify_wizard(ui, env, started, enter, &mut keep);
                if closed {
                    keep = false;
                }
            }
            Modal::WhatsNew => {
                let (st, demo) = super::pages::updates::view(ui_state);
                let version = st.available.as_ref().map(|a| a.version.clone()).unwrap_or_else(|| st.current.clone());
                keep &= !modal_header(ui, &format!("What's new in FanRGB {version}"));
                if demo {
                    text::caption3(ui, "Demo preview: demo instances never check for or install updates.");
                    ui.add_space(4.0);
                }
                egui::ScrollArea::vertical().max_height(360.0).id_salt("whats_new").show(ui, |ui| {
                    // Plain text from the verified manifest: headings and bullets only, never links.
                    for line in st.notes.lines() {
                        let l = line.trim();
                        if l.is_empty() {
                            ui.add_space(6.0);
                        } else if let Some(h) = l.strip_prefix('#') {
                            text::para(ui, h.trim_start_matches('#').trim(), Ts::BodyStrong, p.text1);
                        } else if let Some(b) = l.strip_prefix(['-', '*', '•']) {
                            ui.horizontal_top(|ui| {
                                text::label(ui, "•", Ts::Body, p.text2);
                                text::para(ui, b.trim(), Ts::Body, p.text1);
                            });
                        } else {
                            text::para(ui, l, Ts::Body, p.text1);
                        }
                    }
                    if st.notes.trim().is_empty() {
                        text::para(ui, "No release notes were published with this version.", Ts::Body, p.text2);
                    }
                });
                footer(ui, |ui| {
                    if st.status == crate::updater::Status::Available
                        && Button::new(Kind::Primary, "Update & restart").icon(Icon::Restart).show(ui).clicked()
                    {
                        if demo {
                            ui_state.toast(ToastKind::Info, "Demo: nothing is downloaded or installed");
                        } else {
                            ui_state.pending.push(UiAction::InstallUpdate);
                        }
                        keep = false;
                    }
                    if Button::new(Kind::Secondary, "Close").show(ui).clicked() || enter {
                        keep = false;
                    }
                });
                if st.status == crate::updater::Status::Available {
                    text::caption3(ui, "Fans return to BIOS control for about 3 s while FanRGB restarts.");
                }
            }
            Modal::Shortcuts => {
                keep &= !modal_header(ui, "Keyboard shortcuts");
                shortcuts_table(ui);
                footer(ui, |ui| {
                    if Button::new(Kind::Secondary, "Close").show(ui).clicked() || enter {
                        keep = false;
                    }
                });
            }
            Modal::Licenses { doc } => {
                keep &= !modal_header(ui, "Licences");
                licences_view(ui, doc);
                let text = super::licences::DOCS[*doc].text;
                footer(ui, |ui| {
                    if Button::new(Kind::Secondary, "Close").show(ui).clicked() {
                        keep = false;
                    }
                    if Button::new(Kind::Ghost, "Copy text").icon(Icon::Copy).show(ui).clicked() {
                        ui.ctx().copy_text(text.replace('\r', ""));
                        ui_state.toast(ToastKind::Success, "Licence text copied");
                    }
                });
            }
        }
    });
    if resp.should_close() {
        keep = false;
    }
    if !keep && matches!(modal, Modal::Licenses { .. }) {
        // The wrapped lines of the last text shown (up to a few hundred KiB) are not kept after closing.
        ctx.data_mut(|d| d.remove::<LicenceLines>(Id::new(LICENCE_LINES)));
    }
    // Every way out of a started identification (button, ✕, Esc, click outside) stops the wizard, so
    // the header under test always goes back to its normal control.
    if !keep && matches!(modal, Modal::IdentifyHeaders { started: true }) {
        env.shared.send(Command::Identify(crate::control::identify::IdentifyCmd::Cancel));
    }
    if keep { Some(modal) } else { None }
}

/// RPM vs duty mini chart (calibration).
pub fn rpm_duty_chart(ui: &mut Ui, pts: &[[f32; 2]], h: f32) {
    let p = pal(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
    let plot = Rect::from_min_max(rect.min + vec2(40.0, 6.0), rect.max - vec2(6.0, 18.0));
    let painter = ui.painter_at(rect);
    painter.rect_filled(plot.expand(3.0), 6, p.inset);
    let max = pts.iter().map(|p| p[1]).fold(500.0f32, f32::max);
    let step = super::widgets::chart::nice_step(max, 3.0);
    let top = (max / step).ceil() * step;
    let mut g = 0.0;
    while g <= top + 1.0 {
        let y = plot.bottom() - g / top * plot.height();
        painter.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], Stroke::new(1.0, p.border_subtle));
        painter.text(
            pos2(plot.left() - 6.0, y),
            Align2::RIGHT_CENTER,
            super::widgets::thousands(g),
            tab(Ts::Caption),
            p.text3,
        );
        g += step;
    }
    for d in [0.0, 25.0, 50.0, 75.0, 100.0] {
        let x = plot.left() + d / 100.0 * plot.width();
        painter.text(pos2(x, plot.bottom() + 4.0), Align2::CENTER_TOP, format!("{d:.0}%"), tab(Ts::Caption), p.text3);
    }
    let mut sorted: Vec<[f32; 2]> = pts.to_vec();
    sorted.sort_by(|a, b| a[0].total_cmp(&b[0]));
    let line: Vec<egui::Pos2> = sorted
        .iter()
        .map(|q| pos2(plot.left() + q[0] / 100.0 * plot.width(), plot.bottom() - q[1] / top * plot.height()))
        .collect();
    for q in &line {
        painter.circle_filled(*q, 2.5, p.accent);
    }
    if line.len() > 1 {
        painter.add(egui::Shape::line(line, Stroke::new(1.75, p.accent)));
    }
}

pub const SHORTCUTS: &[(&str, &str)] = &[
    ("Ctrl+1 … Ctrl+7", "Overview, Cooling, Lighting, Intelligence, Sensors, Diagnostics, Settings"),
    ("Ctrl+8", "Updates"),
    ("Ctrl+,", "Settings"),
    ("Ctrl+K", "Command palette"),
    ("Ctrl+P", "Profile switcher"),
    ("Ctrl+F", "Search (Sensors, Settings)"),
    ("Ctrl+Shift+B", "Return fans to BIOS"),
    ("Alt+1 … Alt+6", "Fan mode: Silent, Balanced, Performance, Full speed, Auto (AI), BIOS"),
    ("Ctrl+Shift+M", "Next fan mode"),
    ("Ctrl+Z / Ctrl+Y", "Undo / redo curve edits"),
    ("Esc", "Close the topmost overlay / deselect"),
    ("F1", "This sheet"),
    ("Tab / Shift+Tab", "Move focus"),
    ("← →", "Fan-mode tiles, tabs, segmented controls, gallery, curve points"),
    ("Ctrl+arrows", "Move the selected curve point (Shift ×5)"),
    ("Del / Ins", "Remove / insert a curve point"),
];

pub fn shortcuts_table(ui: &mut Ui) {
    let p = pal(ui);
    egui::Grid::new("shortcuts").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
        for (k, what) in SHORTCUTS {
            kbd(ui, k);
            text::para(ui, *what, Ts::Body, p.text2);
            ui.end_row();
        }
    });
}

// ---------------------------------------------------------------------------------------
// Licences

/// Temp-memory key of the wrapped lines of the licence text on screen.
const LICENCE_LINES: &str = "licence_lines";
/// (document, columns, lines): re-wrapped only when either changes.
type LicenceLines = (usize, usize, Arc<Vec<String>>);

/// The Licences dialog body: a chip per document, what it covers, and the text in a scrollable monospace
/// view. The texts run to a few hundred KiB (the Rust crates' licences), so they are wrapped once into
/// lines and only the visible lines are laid out.
fn licences_view(ui: &mut Ui, doc: &mut usize) {
    use super::licences::{DOCS, wrap_lines};
    let p = pal(ui);
    *doc = (*doc).min(DOCS.len() - 1);
    text::para(
        ui,
        "fanrgb.exe contains FanRGB's own code (Apache-2.0, two files MPL-2.0) and third-party code and fonts \
         under their own licences. Every text that comes with it is here.",
        Ts::Caption,
        p.text2,
    );
    ui.add_space(8.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
        for (i, d) in DOCS.iter().enumerate() {
            if super::widgets::controls::filter_chip(ui, d.label, i == *doc).clicked() {
                *doc = i;
            }
        }
    });
    let d = &DOCS[*doc];
    ui.add_space(8.0);
    // Selectable, so the source code address can be copied.
    ui.add(egui::Label::new(text::rt(d.note(), Ts::Caption, p.text2)).selectable(true).wrap());
    ui.add_space(8.0);
    let height = (ui.ctx().content_rect().height() - 420.0).clamp(160.0, 400.0);
    Frame::new().fill(p.inset).corner_radius(R_MD).inner_margin(Margin::same(10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let mono = font(Ts::Mono);
        let (char_w, row_h) = ui.fonts_mut(|f| (f.glyph_width(&mono, '0'), f.row_height(&mono)));
        // One column of slack for the scroll bar and glyphs from fallback fonts.
        let cols = ((ui.available_width() - ui.spacing().scroll.bar_width) / char_w.max(1.0) - 1.0).max(20.0) as usize;
        let id = Id::new(LICENCE_LINES);
        let lines = match ui.data(|m| m.get_temp::<LicenceLines>(id)) {
            Some((d0, c0, lines)) if d0 == *doc && c0 == cols => lines,
            _ => {
                let lines = Arc::new(wrap_lines(d.text, cols));
                ui.data_mut(|m| m.insert_temp(id, (*doc, cols, lines.clone())));
                lines
            }
        };
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::vertical()
            .id_salt(("licence_text", *doc))
            .max_height(height)
            .auto_shrink([false, true])
            .show_rows(ui, row_h, lines.len(), |ui, rows| {
                for l in &lines[rows] {
                    // An empty label has no height: a space keeps blank lines one row tall.
                    let line = if l.is_empty() { " " } else { l.as_str() };
                    ui.add(
                        egui::Label::new(egui::RichText::new(line).font(mono.clone()).color(p.text2))
                            .selectable(true)
                            .extend(),
                    );
                }
            });
    });
}

// ---------------------------------------------------------------------------------------
// Command palette

#[derive(Default)]
pub struct PaletteState {
    pub query: String,
    pub selected: usize,
    pub opened: bool,
}

pub fn show_palette(ctx: &Context, ps: &mut PaletteState, entries: &[PaletteEntry]) -> (bool, Option<UiAction>) {
    let p = pal_ctx(ctx);
    let mut keep = true;
    let mut picked = None;
    let screen = ctx.content_rect();
    let w = 560.0f32.min(screen.width() - 48.0);
    let area =
        egui::Modal::default_area(Id::new("palette")).anchor(Align2::CENTER_TOP, vec2(0.0, screen.height() * 0.14));
    let resp = egui::Modal::new(Id::new("palette"))
        .area(area)
        .frame(modal_frame(ctx).inner_margin(Margin::same(8)))
        .backdrop_color(p.scrim)
        .show(ctx, |ui| {
            ui.set_width(w);
            // filter
            let mut scored: Vec<(i32, usize)> = entries
                .iter()
                .enumerate()
                .filter_map(|(i, e)| fuzzy_score(&ps.query, &format!("{} {}", e.title, e.context)).map(|s| (s, i)))
                .collect();
            if !ps.query.is_empty() {
                scored.sort_by(|a, b| b.0.cmp(&a.0));
            }
            scored.truncate(40);
            let (up, down, enter) =
                ui.input(|i| (i.key_pressed(Key::ArrowUp), i.key_pressed(Key::ArrowDown), i.key_pressed(Key::Enter)));
            if down {
                ps.selected = (ps.selected + 1).min(scored.len().saturating_sub(1));
            }
            if up {
                ps.selected = ps.selected.saturating_sub(1);
            }
            let te = egui::TextEdit::singleline(&mut ps.query)
                .id(Id::new("palette_q"))
                .font(font(Ts::Body))
                .hint_text(text::rt("Search pages, fans, profiles, effects, actions…", Ts::Body, p.text3))
                .frame(false)
                .desired_width(w - 44.0)
                .margin(Margin { left: 36, right: 8, top: 12, bottom: 12 });
            let r = ui.add(te);
            icons::paint(
                ui.painter(),
                Rect::from_center_size(pos2(r.rect.left() + 18.0, r.rect.center().y), vec2(18.0, 18.0)),
                Icon::Search,
                p.text3,
            );
            if !ps.opened {
                r.request_focus();
                ps.opened = true;
            }
            if r.changed() {
                ps.selected = 0;
            }
            let moved = up || down || r.changed();
            super::widgets::card::divider(ui);
            ui.add_space(4.0);
            egui::ScrollArea::vertical().max_height(360.0).id_salt("palette_list").show(ui, |ui| {
                if scored.is_empty() {
                    ui.add_space(12.0);
                    ui.vertical_centered(|ui| {
                        text::label(ui, "No matches", Ts::Body, p.text3);
                    });
                    ui.add_space(12.0);
                }
                let mut last_group = "";
                for (row, (_, i)) in scored.iter().enumerate() {
                    let e = &entries[*i];
                    if e.group != last_group && ps.query.is_empty() {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.add_space(10.0);
                            text::overline(ui, e.group);
                        });
                        last_group = e.group;
                    }
                    let sel = row == ps.selected;
                    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::click());
                    if sel {
                        ui.painter().rect_filled(rect, R_MD, p.accent_subtle);
                        // Only follow keyboard selection changes; otherwise the wheel could not scroll.
                        if moved {
                            ui.scroll_to_rect(rect, None);
                        }
                    } else if resp.hovered() {
                        ui.painter().rect_filled(rect, R_MD, p.surface2);
                    }
                    icons::paint(
                        ui.painter(),
                        Rect::from_center_size(pos2(rect.left() + 20.0, rect.center().y), vec2(16.0, 16.0)),
                        e.icon,
                        if sel { p.accent } else { p.text2 },
                    );
                    let g = ui.painter().layout_no_wrap(e.title.clone(), font(Ts::Body), p.text1);
                    let tw = g.size().x;
                    ui.painter().galley(pos2(rect.left() + 40.0, rect.center().y - g.size().y / 2.0), g, p.text1);
                    if !e.context.is_empty() {
                        ui.painter().text(
                            pos2(rect.left() + 48.0 + tw, rect.center().y),
                            Align2::LEFT_CENTER,
                            &e.context,
                            font(Ts::Caption),
                            p.text3,
                        );
                    }
                    if !e.hint.is_empty() {
                        ui.painter().text(
                            pos2(rect.right() - 10.0, rect.center().y),
                            Align2::RIGHT_CENTER,
                            e.hint,
                            font(Ts::Caption),
                            p.text3,
                        );
                    }
                    if resp.clicked() || (enter && sel) {
                        picked = Some(e.action.clone());
                    }
                }
            });
        });
    if resp.should_close() || picked.is_some() {
        keep = false;
    }
    (keep, picked)
}

/// Paints the visible focus ring for whatever widget has keyboard focus (keyboard navigation only).
pub fn paint_focus_ring(ctx: &Context) {
    let Some(id) = ctx.memory(|m| m.focused()) else { return };
    let Some(r) = ctx.read_response(id) else { return };
    if !r.rect.is_positive() {
        return;
    }
    let p = pal_ctx(ctx);
    let painter = ctx.layer_painter(egui::LayerId::new(Order::Foreground, Id::new("focus_ring_layer")));
    painter.rect_stroke(r.rect.expand(2.0), R_MD + 2, Stroke::new(2.0, p.focus), StrokeKind::Outside);
}

// ---------------------------------------------------------------------------------------
// "Identify your fans" (header roles wizard)

/// The answer grid's digit shortcuts (1-7) while answers are accepted.
const ANSWER_KEYS: [Key; 7] = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7];

fn identify_wizard(ui: &mut Ui, env: &mut ModalEnv, started: &mut bool, enter: bool, keep: &mut bool) {
    use crate::control::identify::{Answer, IdentifyCmd, IdentifyPhase, LOW_MAX_S, MAX_CPU_C, PUMP_SAFE_DUTY};
    let p = pal(ui);
    let view = env.st.identify.clone().filter(|_| *started);
    let Some(v) = view else {
        if *started {
            text::para(ui, "Starting…", Ts::Body, p.text2);
            return;
        }
        // ---- intro: what is known now ----
        text::para(
            ui,
            "FanRGB doesn't know yet what is plugged into each fan header. It runs each header at full speed, then \
             turns it down for a few seconds. Watch and listen, then say what slowed down.",
            Ts::Body,
            p.text2,
        );
        if !env.st.mode_gate.is_empty() {
            // Non-BIOS modes wait for this.
            ui.add_space(6.0);
            let mode = env.cfg.fan_mode();
            let line = if mode.preset == crate::config::Preset::Bios {
                "Fan modes other than BIOS start once every header is identified (or set by hand in \
                 \u{201c}What's connected?\u{201d})."
                    .to_string()
            } else {
                format!(
                    "{} waits for this: every fan stays on the BIOS until each header is identified (or set by hand \
                     in \u{201c}What's connected?\u{201d}).",
                    mode.label
                )
            };
            text::para(ui, line, Ts::Body, p.text1);
        }
        ui.add_space(10.0);
        for f in env.st.fans.iter().filter(|f| f.controllable && !f.unconnected) {
            ui.horizontal(|ui| {
                ui.set_min_height(28.0);
                text::label(ui, &f.header, Ts::BodyStrong, p.text1);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    match f.assigned {
                        Some(r) => {
                            super::widgets::controls::dot_chip(ui, r.label(), super::widgets::controls::Tone::Success)
                        }
                        None => super::widgets::controls::dot_chip(
                            ui,
                            "Not identified",
                            super::widgets::controls::Tone::Neutral,
                        ),
                    };
                    if f.no_speed_signal() {
                        text::label(ui, "no speed signal", Ts::Caption, p.text3);
                    }
                });
            });
        }
        ui.add_space(8.0);
        let cpu = env.st.value(crate::model::ids::CPU_TEMP);
        let hot = cpu.is_none_or(|t| t > MAX_CPU_C);
        text::caption3(
            ui,
            format!(
                "Run it while the PC is idle (CPU below {MAX_CPU_C:.0} °C). Every header goes back to its normal control \
                 after its test; one that may carry the pump is never below {PUMP_SAFE_DUTY:.0} % for more than {LOW_MAX_S:.0} s."
            ),
        );
        if hot {
            ui.add_space(4.0);
            text::para(
                ui,
                format!(
                    "The CPU is above {MAX_CPU_C:.0} °C or its temperature is unknown: the test will refuse to start."
                ),
                Ts::Caption,
                p.warning,
            );
        }
        footer(ui, |ui| {
            if Button::new(Kind::Primary, "Start").icon(Icon::Play).show(ui).clicked() || enter {
                env.shared.send(Command::Identify(IdentifyCmd::Start));
                *started = true;
            }
            if Button::new(Kind::Secondary, "Not now").show(ui).clicked() {
                *keep = false;
            }
        });
        return;
    };

    // ---- running ----
    if v.total > 0 && !v.done {
        ui.horizontal(|ui| {
            text::label(ui, format!("Header {} of {}", v.step, v.total), Ts::Caption, p.text2);
            ui.add_space(6.0);
            for k in 1..=v.total {
                let (r, _) = ui.allocate_exact_size(vec2(18.0, 6.0), Sense::hover());
                let c = if k < v.step {
                    p.accent
                } else if k == v.step {
                    p.accent_hover
                } else {
                    p.surface3
                };
                ui.painter().rect_filled(r, 3.0, c);
            }
        });
        ui.add_space(6.0);
        // Phase card: which header, what it does now, time left.
        let running = matches!(v.phase, IdentifyPhase::FullSpeed | IdentifyPhase::Test);
        Frame::new().fill(p.surface2).corner_radius(R_LG).inner_margin(Margin::same(14)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::hover());
                let angle = if running && !anim::reduce_motion(ui.ctx()) {
                    ui.input(|i| i.time) as f32 * if v.phase == IdentifyPhase::FullSpeed { 9.0 } else { 3.0 }
                } else {
                    0.0
                };
                icons::paint_fan(ui.painter(), r, angle, p.accent);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    text::label(ui, &v.header, Ts::Title, p.text1);
                    let phase = match v.phase {
                        IdentifyPhase::FullSpeed => {
                            format!("Full speed ({:.0} %) · {:.0} s", v.duty.unwrap_or(100.0), v.seconds_left.max(0.0))
                        }
                        IdentifyPhase::Test => format!(
                            "Turned down to {:.0} % · {:.0} s left",
                            v.duty.unwrap_or(30.0),
                            v.seconds_left.max(0.0)
                        ),
                        IdentifyPhase::Ask => "Back on normal control · waiting for your answer".into(),
                        IdentifyPhase::Done => "Done".into(),
                        IdentifyPhase::Stopped => "Stopped".into(),
                    };
                    text::label(ui, phase, Ts::Caption, p.text2);
                });
            });
            if running {
                ui.add_space(8.0);
                let total = if v.phase == IdentifyPhase::FullSpeed {
                    crate::control::identify::HIGH_S
                } else {
                    crate::control::identify::LOW_S
                };
                let frac = 1.0 - (v.seconds_left / total).clamp(0.0, 1.0);
                let w = ui.available_width();
                super::widgets::controls::progress(ui, frac, p.accent, w, 6.0);
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
        });
        ui.add_space(8.0);
    }
    text::para(ui, &v.question, Ts::Body, p.text1);
    if let Some(g) = &v.guess {
        ui.add_space(2.0);
        text::para(ui, g, Ts::Caption, p.text2);
    }
    if let Some(e) = &v.error {
        ui.add_space(4.0);
        text::para(ui, e, Ts::Body, p.warning);
    }
    if !v.done {
        ui.add_space(8.0);
        let digit =
            ui.input(|i| if i.modifiers.is_none() { ANSWER_KEYS.iter().position(|k| i.key_pressed(*k)) } else { None });
        let cols = 2;
        let w = (ui.available_width() - 8.0) / cols as f32;
        for row in Answer::ALL.chunks(cols) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                for a in row {
                    let n = Answer::ALL.iter().position(|x| x == a).unwrap_or(0);
                    let label = format!("{}  {}", n + 1, a.label());
                    let b = ui
                        .add_enabled_ui(v.can_answer, |ui| Button::new(Kind::Secondary, &label).min_width(w).show(ui))
                        .inner;
                    if v.can_answer && (b.clicked() || digit == Some(n)) {
                        env.shared.send(Command::Identify(IdentifyCmd::Answer(*a)));
                    }
                }
            });
        }
        if !v.can_answer {
            text::caption3(ui, "Answers open while the header is turned down.");
        }
    }
    if !v.results.is_empty() {
        ui.add_space(10.0);
        text::overline(ui, "Results");
        for r in &v.results {
            ui.horizontal(|ui| {
                text::label(ui, &r.header, Ts::Label, p.text1);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (what, tone) = match r.answer {
                        Some(a) => (a.role().label(), super::widgets::controls::Tone::Success),
                        None => ("Skipped (unchanged)", super::widgets::controls::Tone::Neutral),
                    };
                    super::widgets::controls::dot_chip(ui, what, tone);
                });
            });
        }
    }
    footer(ui, |ui| {
        if v.done {
            if Button::new(Kind::Primary, "Done").show(ui).clicked() || enter {
                *keep = false;
            }
        } else {
            if Button::new(Kind::Secondary, "Stop").show(ui).clicked() {
                // Stop and close: the header returns to its normal control (Cancel is sent on close).
                *keep = false;
            }
            if Button::new(Kind::Ghost, "Skip header").show(ui).clicked() {
                env.shared.send(Command::Identify(IdentifyCmd::Skip));
            }
            if v.can_answer && Button::new(Kind::Ghost, "Repeat").icon(Icon::Refresh).show(ui).clicked() {
                env.shared.send(Command::Identify(IdentifyCmd::Repeat));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Licences dialog body renders every document headless, with the app's fonts, at the dialog's
    /// width: about 80 monospace columns, and only the document on screen is kept wrapped.
    #[test]
    fn licences_view_renders_every_document() {
        let ctx = Context::default();
        crate::ui::fonts::install(&ctx);
        let input = || egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1280.0, 800.0))),
            ..Default::default()
        };
        // Fonts set with `set_fonts` take effect from the next pass.
        let _ = ctx.run(input(), |_| {});
        for i in 0..crate::ui::licences::DOCS.len() {
            let mut doc = i;
            // An area with the dialog's width, like the modal (`egui::Modal` is an area too).
            let out = ctx.run(input(), |ctx| {
                egui::Area::new(Id::new("licences_test")).show(ctx, |ui| {
                    ui.set_width(640.0);
                    licences_view(ui, &mut doc);
                });
            });
            assert_eq!(doc, i);
            assert!(!out.shapes.is_empty());
            let (d, cols, lines) = ctx.data(|m| m.get_temp::<LicenceLines>(Id::new(LICENCE_LINES))).expect("lines");
            assert_eq!(d, i);
            assert!((70..=100).contains(&cols), "{cols} columns");
            assert!(lines.len() > 5, "{}: {} lines", crate::ui::licences::DOCS[i].label, lines.len());
        }
        // An index out of range shows the last document instead of panicking.
        let mut doc = 99;
        let _ = ctx.run(input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| licences_view(ui, &mut doc));
        });
        assert_eq!(doc, crate::ui::licences::DOCS.len() - 1);
    }
}
