//! Diagnostics: "Is everything working, and if not, what do I do?"

use egui::{Color32, Id, Rect, Sense, Ui, pos2, vec2};

use super::{PageCtx, handle_card};
use crate::error::{ErrorCode, FixAction};
use crate::hw::pawnio::PawnIoStatus;
use crate::rgb::RgbConnState;
use crate::system::AutostartStatus;
use crate::ui::LogLevel;
use crate::ui::actions::UiAction;
use crate::ui::fonts::{Ts, font, tab};
use crate::ui::icons::{self, Icon};
use crate::ui::issues::{EventKind, fmt_time};
use crate::ui::layout::{self, Breakpoint};
use crate::ui::overlays::ToastKind;
use crate::ui::theme::{Palette, R_SM, pal};
use crate::ui::widgets::card::{card, card_eq, header};
use crate::ui::widgets::controls::{self, Button, Kind, Size};
use crate::ui::widgets::issue_card::issue_card;
use crate::ui::widgets::text;

#[derive(Clone, Copy, PartialEq)]
enum Health {
    Ok,
    Warn,
    Bad,
    Off,
}

fn health_color(p: &Palette, h: Health) -> Color32 {
    match h {
        Health::Ok => p.success,
        Health::Warn => p.warning,
        Health::Bad => p.danger,
        Health::Off => p.text3,
    }
}

pub fn show(ui: &mut Ui, pc: &mut PageCtx) {
    let p = pal(ui);
    let gap = pc.bp.gap();
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        text::para(
            ui,
            "Health of every subsystem, active issues with one-click fixes, and the event log.",
            Ts::Body,
            p.text2,
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if Button::new(Kind::Primary, "Write report")
                .icon(Icon::Report)
                .show(ui)
                .on_hover_text("Write a full diagnostics report (with register dump) to disk")
                .clicked()
            {
                pc.ui.pending.push(UiAction::WriteDiagnostics);
            }
            if Button::new(Kind::Secondary, "Copy report").icon(Icon::Copy).show(ui).clicked() {
                pc.ui.pending.push(UiAction::CopyReport);
            }
            if let Some(path) = &pc.st.diag_path
                && controls::link(ui, "Show report file").clicked()
            {
                let _ = crate::util::win::reveal_file(std::path::Path::new(path));
            }
        });
    });
    ui.add_space(gap);
    if pc.bp == Breakpoint::Narrow {
        health_card(ui, pc, 0.0);
        ui.add_space(gap);
        issues_col(ui, pc);
    } else {
        layout::row(ui, Id::new("diag_top"), &[1.0, 1.0], gap, |i, ui, min_h| {
            if i == 0 { health_card(ui, pc, min_h) } else { issues_col(ui, pc) }
        });
    }
    ui.add_space(gap);
    log_card(ui, pc);
    ui.add_space(gap);
    system_line(ui, pc);
}

fn health_row(ui: &mut Ui, h: Health, name: &str, value: &str, fix: Option<(&str, UiAction)>, pc: &mut PageCtx) {
    let p = pal(ui);
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(vec2(w, 34.0), Sense::hover());
    let c = health_color(p, h);
    let icon = match h {
        Health::Ok => Icon::CheckCircle,
        Health::Warn => Icon::Warning,
        Health::Bad => Icon::Error,
        Health::Off => Icon::Info,
    };
    icons::paint(ui.painter(), Rect::from_center_size(pos2(r.left() + 10.0, r.center().y), vec2(16.0, 16.0)), icon, c);
    ui.painter().text(pos2(r.left() + 28.0, r.center().y), egui::Align2::LEFT_CENTER, name, font(Ts::Body), p.text1);
    let vx = r.left() + (w * 0.42).max(150.0);
    let right_reserve = if fix.is_some() { 110.0 } else { 0.0 };
    let g =
        ui.painter().layout(value.to_string(), font(Ts::Caption), p.text2, (r.right() - vx - right_reserve).max(60.0));
    ui.painter().galley(pos2(vx, r.center().y - g.size().y / 2.0), g, p.text2);
    if let Some((label, action)) = fix {
        let br = Rect::from_min_max(pos2(r.right() - right_reserve, r.top() + 3.0), r.max - vec2(0.0, 3.0));
        let mut child =
            ui.new_child(egui::UiBuilder::new().max_rect(br).layout(egui::Layout::right_to_left(egui::Align::Center)));
        if Button::new(Kind::Secondary, label).size(Size::Sm).show(&mut child).clicked() {
            pc.ui.pending.push(action);
        }
    }
}

fn health_card(ui: &mut Ui, pc: &mut PageCtx, min_h: f32) -> f32 {
    let info = pc.st.info.clone();
    let rgb = pc.rgb.clone();
    card_eq(ui, min_h, 16, |ui| {
        header(ui, "Health", None, |_| {});
        ui.add_space(4.0);
        // Driver
        let (h, v, fix) = match &info.pawnio {
            PawnIoStatus::Ready => {
                (Health::Ok, format!("Ready · {}", info.pawnio_version.clone().unwrap_or_default()), None)
            }
            PawnIoStatus::NotInstalled => {
                (Health::Bad, "Not installed".into(), Some(("Install", UiAction::Fix(FixAction::InstallPawnIo, None))))
            }
            PawnIoStatus::AccessDenied => (
                Health::Warn,
                "Needs administrator rights".into(),
                Some(("Restart", UiAction::Fix(FixAction::RestartAsAdmin, None))),
            ),
            PawnIoStatus::Error(e) => (Health::Bad, e.clone(), None),
            PawnIoStatus::Unknown => (Health::Off, "Not checked yet".into(), None),
        };
        health_row(ui, h, "PawnIO driver", &v, fix, pc);
        let (h, v, fix) = if info.elevated {
            (Health::Ok, "Yes".to_string(), None)
        } else {
            (Health::Warn, "No".to_string(), Some(("Restart", UiAction::Fix(FixAction::RestartAsAdmin, None))))
        };
        health_row(ui, h, "Administrator", &v, fix, pc);
        let (h, v) = if info.chips.is_empty() {
            (Health::Bad, "Not detected".to_string())
        } else {
            (Health::Ok, info.chips.join(", "))
        };
        health_row(ui, h, "Fan controller", &v, None, pc);
        let (h, v) = if info.cpu_temp_source == "unavailable" || info.cpu_temp_source.is_empty() {
            (Health::Warn, "Unavailable".to_string())
        } else {
            (Health::Ok, info.cpu_temp_source.clone())
        };
        health_row(ui, h, "CPU temperature", &v, None, pc);
        let gpu_ok = pc.st.value(crate::model::ids::GPU_TEMP).is_some();
        let (h, v) = if gpu_ok {
            (Health::Ok, info.gpus.join(", "))
        } else {
            (
                Health::Off,
                if info.gpus.is_empty() {
                    "No GPU telemetry".to_string()
                } else {
                    format!("{} · no temperature", info.gpus.join(", "))
                },
            )
        };
        health_row(ui, h, "GPU telemetry", &v, None, pc);
        // RGB: connection state machine + last error
        let mut v = match &rgb.device {
            Some(d) => format!("{} · fw {} · {:.0} fps", d.name, d.fw, rgb.fps),
            None => rgb.state.label().to_string(),
        };
        if let Some(e) = &rgb.last_error
            && (rgb.device.is_none() || e.at.elapsed().map(|d| d.as_secs() < 600).unwrap_or(false))
        {
            v.push_str(&format!(
                " · last {} at {}{}",
                e.code,
                fmt_time(e.at),
                if e.count > 1 { format!(" ({}×)", e.count) } else { String::new() }
            ));
        }
        if rgb.reconnects > 0 {
            v.push_str(&format!(" · {} reconnects", rgb.reconnects));
        }
        if rgb.hung_io_threads > 0 {
            v.push_str(&format!(" · {} stuck in the USB driver", rgb.hung_io_threads));
        }
        if rgb.suspended {
            v.push_str(" · handed over (sleep/exit)");
        }
        let (h, fix) = match &rgb.state {
            RgbConnState::Connected => (Health::Ok, None),
            RgbConnState::Hung => (Health::Bad, Some(("How to fix", UiAction::Fix(FixAction::PowerCycle, None)))),
            RgbConnState::InUseByOtherApp => {
                (Health::Bad, Some(("Show apps", UiAction::Fix(FixAction::CloseConflictingSoftware, None))))
            }
            RgbConnState::Reconnecting { .. } => (Health::Warn, Some(("Reconnect", UiAction::ReconnectRgb))),
            RgbConnState::NotPresent => (Health::Off, Some(("Reconnect", UiAction::ReconnectRgb))),
            RgbConnState::Searching => (Health::Off, None),
        };
        health_row(ui, h, "RGB controller", &v, fix, pc);
        // Windows Dynamic Lighting: registry state + firmware byte.
        let dl = &rgb.dynamic_lighting;
        let h = match dl.state {
            _ if dl.conflict() => Health::Warn,
            crate::rgb::dynlight::DlState::Off => Health::Ok,
            _ => Health::Off,
        };
        let fix = dl.conflict().then_some(("Lighting", UiAction::Navigate(crate::ui::Page::Lighting)));
        let fix = Some(("Open settings", UiAction::OpenDynamicLightingSettings)).filter(|_| !dl.conflict()).or(fix);
        let v = dl.chip_label();
        health_row(ui, h, "Dynamic Lighting", v.trim_start_matches("Dynamic Lighting: "), fix, pc);
        // App version: current, latest, Check now → Updates.
        {
            use crate::updater::Status;
            let (us, demo) = crate::ui::pages::updates::view(pc.ui);
            let current = env!("CARGO_PKG_VERSION");
            let (h, v) = match us.status {
                _ if demo => (Health::Off, format!("{current} · updates off in demo")),
                Status::Available => (
                    Health::Warn,
                    format!(
                        "{current} · {} available",
                        us.available.as_ref().map(|a| a.version.as_str()).unwrap_or("?")
                    ),
                ),
                Status::UpToDate => (Health::Ok, format!("{current} · up to date")),
                Status::Failed => (
                    Health::Warn,
                    format!(
                        "{current} · last check failed ({})",
                        us.error.as_ref().map(|e| e.0.id()).unwrap_or_default()
                    ),
                ),
                Status::Checking | Status::Installing => (Health::Off, format!("{current} · checking…")),
                _ => (Health::Off, current.to_string()),
            };
            let fix = if us.status == Status::Available {
                ("Updates", UiAction::Navigate(crate::ui::Page::Updates))
            } else {
                ("Check now", UiAction::CheckForUpdates)
            };
            health_row(ui, h, "App version", &v, Some(fix), pc);
            if let Some(svc) = crate::drivers::service() {
                let ds = svc.snapshot();
                let n = ds.updates_available();
                let v = if !ds.inventory_done {
                    "Reading…".to_string()
                } else if n > 0 {
                    format!("{n} Windows Update offer{}", if n == 1 { "" } else { "s" })
                } else {
                    format!("{} drivers listed · inform only", ds.rows.len())
                };
                health_row(
                    ui,
                    if n > 0 { Health::Warn } else { Health::Off },
                    "Drivers",
                    &v,
                    Some(("Updates", UiAction::Navigate(crate::ui::Page::Updates))),
                    pc,
                );
            }
        }
        let (h, v, fix) = if pc.st.conflicts.is_empty() {
            (Health::Ok, "None".to_string(), None)
        } else {
            (
                Health::Warn,
                pc.st.conflicts.iter().map(|c| c.0.clone()).collect::<Vec<_>>().join(", "),
                Some(("Show", UiAction::Fix(FixAction::CloseConflictingSoftware, None))),
            )
        };
        health_row(ui, h, "Other fan/RGB apps", &v, fix, pc);
        let wd = crate::system::watchdog_active();
        let controls_fans = pc.st.fans.iter().any(|f| f.controlled);
        health_row(
            ui,
            if wd {
                Health::Ok
            } else if controls_fans {
                Health::Warn
            } else {
                Health::Off
            },
            "Crash watchdog",
            if wd { "Running" } else { "Not running" },
            None,
            pc,
        );
        let (h, v) = match &pc.ui.autostart_st {
            Some(AutostartStatus::Enabled) => (Health::Ok, "Enabled".to_string()),
            Some(AutostartStatus::Disabled) | Some(AutostartStatus::NeedsInstall) => (Health::Off, "Off".to_string()),
            Some(AutostartStatus::Insecure(cmd)) => {
                (Health::Warn, format!("Task points outside the install folder: {cmd}"))
            }
            Some(AutostartStatus::Unavailable(e)) => (Health::Warn, format!("Task Scheduler unavailable: {e}")),
            None => (Health::Off, "Checking…".to_string()),
        };
        health_row(
            ui,
            h,
            "Start with Windows",
            &v,
            Some(("Settings", UiAction::Navigate(crate::ui::Page::Settings))).filter(|_| h != Health::Ok),
            pc,
        );
    })
}

fn issues_col(ui: &mut Ui, pc: &mut PageCtx) -> f32 {
    let p = pal(ui);
    let list: Vec<_> = pc.ui.issues.active.clone();
    card(ui, |ui| {
        header(ui, &format!("Active issues ({})", list.len()), None, |_| {});
        ui.add_space(4.0);
        if list.is_empty() {
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
                icons::paint(ui.painter(), r, Icon::CheckCircle, p.success);
                text::label(ui, "No active issues. Everything is working.", Ts::Body, p.text2);
            });
        }
        for i in &list {
            let r = issue_card(ui, i, false, pc.ui.reconnecting());
            handle_card(ui, pc, i, r);
            ui.add_space(8.0);
        }
    });
    ui.add_space(pc.bp.gap());
    recent_card(ui, pc);
    0.0
}

fn recent_card(ui: &mut Ui, pc: &mut PageCtx) {
    let p = pal(ui);
    // Merge UI transitions and the engine's coded error ring, newest first.
    let mut rows: Vec<(std::time::SystemTime, Icon, Color32, String)> = Vec::new();
    for e in pc.ui.issues.events.iter() {
        let (icon, c) = match &e.kind {
            EventKind::Appeared(s) if *s >= crate::error::Severity::Error => (Icon::Error, p.danger),
            EventKind::Appeared(crate::error::Severity::Warning) => (Icon::Warning, p.warning),
            EventKind::Appeared(_) => (Icon::Info, p.info),
            EventKind::Resolved => (Icon::CheckCircle, p.success),
            EventKind::Emergency => (Icon::Power, p.warning),
            EventKind::Profile | EventKind::Info => (Icon::Dot, p.text2),
            EventKind::Failure => (Icon::Error, p.danger),
        };
        let text = match e.code {
            Some(code) => format!("{code} {}", e.text),
            None => e.text.clone(),
        };
        rows.push((e.at, icon, c, text));
    }
    for e in &pc.st.errors {
        // Demo drives a simulated controller; the real one's USB health is not relevant there.
        if pc.rgb.simulated && e.code == crate::error::ErrorCode::RgbHung {
            continue;
        }
        let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(e.time_unix);
        let c = match e.code.severity() {
            crate::error::Severity::Critical | crate::error::Severity::Error => p.danger,
            crate::error::Severity::Warning => p.warning,
            crate::error::Severity::Info => p.info,
        };
        let src = if e.source.is_empty() { String::new() } else { format!(" ({})", e.source) };
        rows.push((at, Icon::Diagnostics, c, format!("{} {}{}: {}", e.code, e.code.title(), src, e.detail)));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    card(ui, |ui| {
        header(ui, "Recent events", None, |_| {});
        ui.add_space(4.0);
        if rows.is_empty() {
            text::label(ui, "Nothing has happened yet.", Ts::Body, p.text3);
        }
        egui::ScrollArea::vertical().id_salt("recent_events").max_height(220.0).min_scrolled_height(200.0).show(
            ui,
            |ui| {
                for (at, icon, c, t) in rows.iter().take(100) {
                    ui.horizontal_top(|ui| {
                        text::label(ui, fmt_time(*at), Ts::Mono, p.text3);
                        let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                        icons::paint(ui.painter(), r, *icon, *c);
                        // A vertical child reports the wrapped height, so the scroll area isn't cut short.
                        ui.vertical(|ui| {
                            ui.set_max_width(ui.available_width());
                            text::para(ui, t, Ts::Caption, p.text2);
                        });
                    });
                }
            },
        );
    });
}

fn log_card(ui: &mut Ui, pc: &mut PageCtx) {
    let p = pal(ui);
    card(ui, |ui| {
        ui.horizontal(|ui| {
            text::label(ui, "Log", Ts::Headline, p.text1);
            ui.add_space(8.0);
            controls::segmented_sized(
                ui,
                Id::new("log_level"),
                &mut pc.ui.log_level,
                &[
                    (LogLevel::All, "All"),
                    (LogLevel::Info, "Info"),
                    (LogLevel::Warn, "Warn"),
                    (LogLevel::Error, "Error"),
                ],
                None,
                28.0,
            );
            ui.add_space(8.0);
            controls::toggle(ui, &mut pc.ui.log_follow);
            text::label(ui, "Follow", Ts::Label, p.text2);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if Button::new(Kind::Secondary, "Open folder").size(Size::Sm).icon(Icon::Folder).show(ui).clicked() {
                    pc.ui.pending.push(UiAction::OpenLogFolder);
                }
                if Button::new(Kind::Secondary, "Copy").size(Size::Sm).icon(Icon::Copy).show(ui).clicked() {
                    ui.ctx().copy_text(crate::util::logger::recent_lines().join("\n"));
                    pc.ui.toast(ToastKind::Success, "Log copied");
                }
            });
        });
        ui.add_space(6.0);
        let lines = crate::util::logger::recent_lines();
        let want = |l: &str| match pc.ui.log_level {
            LogLevel::All => true,
            LogLevel::Info => !l.contains(" DEBUG ") && !l.contains(" TRACE "),
            LogLevel::Warn => l.contains(" WARN ") || l.contains(" ERROR "),
            LogLevel::Error => l.contains(" ERROR "),
        };
        egui::Frame::new().fill(p.inset).corner_radius(R_SM).inner_margin(egui::Margin::same(8)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical()
                .id_salt("log_view")
                .max_height(if pc.bp == Breakpoint::Narrow { 200.0 } else { 260.0 })
                .stick_to_bottom(pc.ui.log_follow)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for l in lines.iter().filter(|l| want(l)) {
                        let c = if l.contains(" ERROR ") {
                            p.danger
                        } else if l.contains(" WARN ") {
                            p.warning
                        } else {
                            p.text2
                        };
                        ui.add(
                            egui::Label::new(egui::RichText::new(l).font(font(Ts::Mono)).color(c))
                                .selectable(true)
                                .wrap(),
                        );
                    }
                });
        });
    });
}

fn system_line(ui: &mut Ui, pc: &mut PageCtx) {
    let p = pal(ui);
    let i = &pc.st.info;
    let s = format!(
        "System  {} · {} · {} · FanRGB {}{}",
        i.board,
        i.cpu,
        i.gpus.join(", "),
        env!("CARGO_PKG_VERSION"),
        if i.simulated { " · demo" } else { "" }
    );
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        text::para(ui, &s, Ts::Caption, p.text3);
    });
    let _ = (tab, ErrorCode::Unknown);
}
