//! Updates: FanRGB's own signed update, and an inform-only driver list.
//!
//! FanRGB installs only itself. Drivers link to vendor pages; the "Latest" column is FanRGB's signed
//! vendor list (`drivers::catalog`), and Windows Update's answer is shown as what it is ("No Windows
//! Update offer" is not "up to date": vendors are often ahead).

use egui::{Align2, Color32, Id, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use super::PageCtx;
use crate::drivers::catalog::CatalogStatus;
use crate::drivers::{DriverCategory, DriverRow, DriverState, DriverStatus};
use crate::ui::UiState;
use crate::ui::actions::UiAction;
use crate::ui::fonts::{Ts, font, tab};
use crate::ui::icons::{self, Icon};
use crate::ui::overlays::ToastKind;
use crate::ui::theme::{R_LG, R_MD, R_SM, pal};
use crate::ui::widgets::card::{Sev, banner as page_banner, card};
use crate::ui::widgets::controls::{self, Button, Kind, Size, Tone};
use crate::ui::widgets::text;
use crate::updater::{Status, UpdateUi};

/// Notes shown in the demo's preview of an available update (demo instances never check).
const DEMO_NOTES: &str = "- One-click fan modes: Silent, Balanced, Performance, Full speed, Auto (AI), BIOS\n\
- Driver check (inform only) and per-header LED colour calibration\n\
- CPU_FAN without a tach now shows 'running, no speed signal'";

/// The update state to render. Demo instances (updates off) show a labelled preview of an
/// available release so the screens can be reviewed; nothing can be installed from it.
pub fn view(ui: &UiState) -> (UpdateUi, bool) {
    let st = ui.updates.state().clone();
    if st.status == Status::Disabled {
        let next = {
            let v = crate::updater::manifest::Version::current();
            format!("{}.{}.0", v.0, v.1 + 1)
        };
        let demo = UpdateUi {
            available: Some(crate::updater::fetch::Offer {
                version: next,
                published_at: crate::updater::now().saturating_sub(86_400),
                notes: DEMO_NOTES.into(),
                size: 11_200_000,
                sha256: String::new(),
            }),
            notes: DEMO_NOTES.into(),
            status: Status::Available,
            last_checked: Some(crate::updater::now().saturating_sub(120)),
            ..st
        };
        return (demo, true);
    }
    (st, false)
}

pub fn available_version(ui: &UiState) -> Option<String> {
    let (st, _) = view(ui);
    (st.status == Status::Available).then(|| st.available.map(|a| a.version)).flatten()
}

/// Sidebar badge: the app update plus drivers with a newer version (vendor list or Windows Update).
pub fn badge_count(ui: &UiState) -> usize {
    let app = usize::from(available_version(ui).is_some());
    let drivers = crate::drivers::service().map(|s| s.snapshot().updates_available()).unwrap_or(0);
    app + drivers
}

/// "3 min ago" for a unix timestamp.
pub fn ago(unix: u64) -> String {
    let s = crate::updater::now().saturating_sub(unix);
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=172_799 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86_400),
    }
}

fn date(unix: u64) -> String {
    crate::drivers::days_to_ymd((unix / 86_400) as i64)
}

/// Release notes as plain lines (untrusted text: never links or markup), bullets stripped.
pub fn note_lines(notes: &str) -> Vec<String> {
    notes
        .lines()
        .map(|l| l.trim().trim_start_matches(['-', '*', '•']).trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

pub fn show(ui: &mut Ui, pc: &mut PageCtx) {
    let gap = pc.bp.gap();
    ui.add_space(4.0);
    app_card(ui, pc);
    ui.add_space(gap);
    drivers_card(ui, pc);
}

// ---------------------------------------------------------------------------------------
// App

fn app_card(ui: &mut Ui, pc: &mut PageCtx) {
    let p = pal(ui);
    let (st, demo) = view(pc.ui);
    let busy = pc.ui.updates.busy();
    if busy {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    }
    let failed = st.status == Status::Failed;
    let frame = egui::Frame::new()
        .fill(if failed { p.danger_subtle } else { p.surface1 })
        .stroke(Stroke::new(1.0, if failed { p.danger } else { p.border_subtle }))
        .corner_radius(R_LG)
        .inner_margin(egui::Margin::same(20));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_top(|ui| {
            // icon
            let (ir, _) = ui.allocate_exact_size(vec2(56.0, 56.0), Sense::hover());
            match pc.assets.app_icon(ui.ctx()) {
                Some(t) => {
                    ui.painter().rect_filled(ir, R_MD, p.surface2);
                    ui.painter().image(
                        t,
                        ir.shrink(4.0),
                        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                }
                None => icons::paint_fan(ui.painter(), ir, 0.0, p.accent),
            }
            ui.add_space(12.0);
            let right_w = 260.0f32.min(ui.available_width() * 0.4);
            let left_w = (ui.available_width() - right_w - 12.0).max(200.0);
            ui.vertical(|ui| {
                ui.set_width(left_w);
                ui.spacing_mut().item_spacing.y = 4.0;
                let (title, meta) = match st.status {
                    Status::Available => {
                        let a = st.available.clone().unwrap_or_else(|| crate::updater::fetch::Offer {
                            version: "?".into(),
                            published_at: 0,
                            notes: String::new(),
                            size: 0,
                            sha256: String::new(),
                        });
                        (
                            format!("FanRGB {} is available", a.version),
                            format!(
                                "You have {} · released {} · {:.1} MB · signed",
                                st.current,
                                date(a.published_at),
                                a.size as f64 / 1_000_000.0
                            ),
                        )
                    }
                    Status::Checking => ("Checking for updates…".into(), format!("You have {}", st.current)),
                    Status::Installing => (
                        "Checking signature and SHA-256…".into(),
                        "Then FanRGB swaps itself and restarts. Fans stay under FanRGB control until then.".into(),
                    ),
                    Status::UpToDate => (
                        format!("FanRGB {} is up to date", st.current),
                        "Signed releases from the FanRGB repository".into(),
                    ),
                    Status::VerifiedOnly => (
                        "Update verified".into(),
                        "This is a development build, so it was not replaced (FRG-E710).".into(),
                    ),
                    Status::Failed => {
                        let (code, _) =
                            st.error.clone().unwrap_or((crate::error::ErrorCode::UpdateHelper, String::new()));
                        (code.title().to_string(), format!("You're still on {} · fans were not affected", st.current))
                    }
                    Status::Idle => {
                        (format!("FanRGB {}", st.current), "Signed releases from the FanRGB repository".into())
                    }
                    Status::Disabled => (format!("FanRGB {}", st.current), "Updates are off in demo mode".into()),
                };
                ui.horizontal(|ui| {
                    text::label(ui, &title, Ts::Title, p.text1);
                    if demo {
                        controls::chip(ui, "Demo preview", Tone::Warning).on_hover_text(
                            "Demo instances never check or install; this shows what an available update looks like.",
                        );
                    }
                });
                text::para(ui, &meta, Ts::Caption, p.text2);
                match st.status {
                    Status::Available => {
                        ui.add_space(4.0);
                        for l in note_lines(&st.notes).iter().take(3) {
                            ui.horizontal_top(|ui| {
                                text::label(ui, "•", Ts::Body, p.text2);
                                text::para(ui, l, Ts::Body, p.text1);
                            });
                        }
                        if controls::link(ui, "Full release notes").clicked() {
                            pc.ui.pending.push(UiAction::ShowWhatsNew);
                        }
                    }
                    Status::Checking | Status::Installing => {
                        ui.add_space(6.0);
                        indeterminate(ui, left_w.min(420.0));
                    }
                    Status::Failed => {
                        if let Some((code, detail)) = &st.error {
                            ui.horizontal_wrapped(|ui| {
                                if controls::code_chip(ui, &code.id()).clicked() {
                                    ui.ctx().copy_text(code.id());
                                }
                                text::para(ui, code.help(), Ts::Caption, p.text2);
                            });
                            if !detail.is_empty() {
                                text::caption3(ui, detail.clone());
                            }
                            // Without a token: a private repository or the anonymous rate limit. A token
                            // helps with both, so offer the (optional) field right here.
                            if !st.token_saved
                                && !demo
                                && matches!(
                                    code,
                                    crate::error::ErrorCode::UpdateNotPublic | crate::error::ErrorCode::UpdateRateLimit
                                )
                            {
                                ui.add_space(6.0);
                                token_entry(ui, pc);
                            }
                        }
                    }
                    _ => {}
                }
                if let Some(r) = &st.last_result {
                    text::caption3(ui, format!("Last update: {r}"));
                }
            });
            ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
                ui.set_width(right_w);
                ui.spacing_mut().item_spacing.y = 8.0;
                match st.status {
                    Status::Available => {
                        if Button::new(Kind::Primary, "Update & restart").icon(Icon::Restart).show(ui).clicked() {
                            if demo {
                                pc.ui.toast(ToastKind::Info, "Demo: nothing is downloaded or installed");
                            } else {
                                pc.ui.pending.push(UiAction::InstallUpdate);
                            }
                        }
                        right_caption(ui, "Fans return to BIOS control for about 3 s while FanRGB restarts.");
                    }
                    Status::Failed => {
                        if Button::new(Kind::Secondary, "Retry").icon(Icon::Refresh).show(ui).clicked() {
                            pc.ui.updates.check_now();
                        }
                    }
                    _ => {}
                }
                ui.add_space(6.0);
                let checked =
                    st.last_checked.map(|t| format!("Checked {}", ago(t))).unwrap_or_else(|| "Not checked yet".into());
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let can = !busy && !demo && st.status != Status::Disabled;
                        if ui.add_enabled_ui(can, |ui| controls::link(ui, "Check now")).inner.clicked() {
                            pc.ui.updates.check_now();
                        }
                        text::label(ui, format!("{checked} ·"), Ts::Caption, p.text3);
                    });
                });
            });
        });
    });
}

fn right_caption(ui: &mut Ui, s: &str) {
    let p = pal(ui);
    let w = ui.available_width();
    let mut job = egui::text::LayoutJob::simple(s.to_string(), font(Ts::Caption), p.text2, w);
    job.halign = egui::Align::Max;
    let g = ui.painter().layout_job(job);
    let (r, _) = ui.allocate_exact_size(vec2(w, g.size().y), Sense::hover());
    ui.painter().galley(pos2(r.right(), r.top()), g, p.text2);
}

/// Indeterminate 6 pt bar (only for checks / signature verification).
fn indeterminate(ui: &mut Ui, w: f32) {
    let p = pal(ui);
    let (r, _) = ui.allocate_exact_size(vec2(w, 6.0), Sense::hover());
    ui.painter().rect_filled(r, 3.0, p.surface3);
    let t = if crate::ui::anim::reduce_motion(ui.ctx()) { 0.35 } else { (ui.input(|i| i.time) as f32 * 0.6).fract() };
    let seg = r.width() * 0.3;
    let x0 = r.left() + (r.width() + seg) * t - seg;
    let fill = Rect::from_min_max(pos2(x0.max(r.left()), r.top()), pos2((x0 + seg).min(r.right()), r.bottom()));
    if fill.width() > 1.0 {
        ui.painter().rect_filled(fill, 3.0, p.accent);
    }
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
}

/// Paste field for the optional read-only GitHub token (stored encrypted for this Windows account).
fn token_entry(ui: &mut Ui, pc: &mut PageCtx) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        let id = Id::new("update_token");
        let te = egui::TextEdit::singleline(&mut pc.ui.update_token_draft)
            .id(id)
            .password(true)
            .hint_text("github_pat_…")
            .desired_width(240.0);
        ui.add(te).on_hover_text(format!("Fine-grained token for {}, Contents: read-only", crate::updater::REPO));
        if Button::new(Kind::Primary, "Save").size(Size::Sm).show(ui).clicked() {
            match pc.ui.updates.save_token(&mut pc.ui.update_token_draft) {
                Ok(()) => pc.ui.toast(ToastKind::Info, "Saving the update token · then checking for updates"),
                Err(e) => pc.ui.toast(ToastKind::Error, e),
            }
            ui.ctx().data_mut(|d| d.remove::<egui::text_edit::TextEditState>(id));
        }
    });
    text::para(
        ui,
        "Optional: only needed while the repository is private, or to raise GitHub's rate limit. Kept encrypted \
         for this Windows account only; Settings › Updates can forget it.",
        Ts::Caption,
        p.text3,
    );
}

// ---------------------------------------------------------------------------------------
// Drivers

fn category_icon(c: DriverCategory) -> Icon {
    match c {
        DriverCategory::Wifi => Icon::Wifi,
        DriverCategory::Bluetooth => Icon::Bluetooth,
        DriverCategory::Lan => Icon::Lan,
        DriverCategory::Audio => Icon::Audio,
        DriverCategory::Chipset => Icon::Chip,
        DriverCategory::Gpu => Icon::Gpu,
        DriverCategory::SuperIo => Icon::Shield,
        DriverCategory::Bios => Icon::Bios,
    }
}

/// Windows Update's answer for a row (the "Windows Update" column). "No Windows Update offer" is
/// deliberately not "Up to date": vendors are often ahead of Windows Update.
pub fn status_chip(r: &DriverRow, checked: bool) -> (&'static str, Tone) {
    match (r.status, r.category) {
        (_, DriverCategory::Bios) => ("Unknown", Tone::Neutral),
        (DriverStatus::UpdateAvailable, _) => ("Update available", Tone::Info),
        (DriverStatus::UpToDate, _) => ("No Windows Update offer", Tone::Neutral),
        (DriverStatus::Unknown, c) if !checked && c.covered_by_windows_update() => ("Not checked yet", Tone::Neutral),
        (DriverStatus::Unknown, _) => ("Unknown", Tone::Neutral),
    }
}

/// The "Windows Update" column text: what it offers, "No Windows Update offer", or "—" where Windows
/// Update does not cover the row (BIOS, chipset package, PawnIO).
pub fn wu_text(r: &DriverRow, checked: bool) -> (String, Tone) {
    match status_chip(r, checked) {
        ("Update available", t) => {
            let v =
                r.latest.as_ref().and_then(|o| o.version.clone().or(o.date.clone())).unwrap_or_else(|| "newer".into());
            (format!("Offers {v}"), t)
        }
        ("Unknown", t) => ("—".into(), t),
        (s, t) => (s.into(), t),
    }
}

/// The row's status chip: the vendor list first (it is ahead of Windows Update), then Windows Update.
pub fn row_chip(r: &DriverRow, checked: bool) -> (&'static str, Tone) {
    let wu_offer = r.status == DriverStatus::UpdateAvailable && r.category != DriverCategory::Bios;
    match r.catalog_status {
        CatalogStatus::Newer => ("Newer available", Tone::Info),
        _ if wu_offer => ("Update available", Tone::Info),
        CatalogStatus::LatestKnown => ("Latest known", Tone::Success),
        CatalogStatus::Incomparable => ("Compare manually", Tone::Neutral),
        CatalogStatus::NotInList => ("Not in list", Tone::Neutral),
        CatalogStatus::NoList => status_chip(r, checked),
    }
}

/// Something newer is known for this row (sort order, button emphasis).
fn actionable(r: &DriverRow) -> bool {
    r.catalog_status == CatalogStatus::Newer
        || (r.status == DriverStatus::UpdateAvailable && r.category != DriverCategory::Bios)
}

/// Actionable rows first, BIOS last; stable otherwise.
fn sorted(rows: &[DriverRow]) -> Vec<&DriverRow> {
    let mut v: Vec<&DriverRow> = rows.iter().collect();
    v.sort_by_key(|r| match (r.category, actionable(r)) {
        (DriverCategory::Bios, _) => 2,
        (_, true) => 0,
        _ => 1,
    });
    v
}

/// The page to open for a row: the vendor list's download page when it names a newer version (already
/// checked against the vendor host allow-list), else the pinned vendor page.
fn page_for(r: &DriverRow) -> Option<String> {
    match (&r.catalog, r.catalog_status) {
        (Some(m), CatalogStatus::Newer | CatalogStatus::LatestKnown | CatalogStatus::Incomparable)
            if crate::drivers::catalog::url_allowed(&m.download_url) =>
        {
            Some(m.download_url.clone())
        }
        _ => r.vendor_url.map(str::to_string),
    }
}

/// One line about the vendor list under the card's description, or a banner when it is out of date.
fn catalog_line(ui: &mut Ui, pc: &mut PageCtx, st: &DriverState) {
    let p = pal(ui);
    let c = &st.catalog;
    let now = crate::updater::now();
    match &c.loaded {
        Some(l) if c.expired(now) => {
            ui.add_space(4.0);
            page_banner(
                ui,
                Sev::Warning,
                &format!("Vendor list out of date (checked {})", date(l.generated_at)),
                "FanRGB could not get a newer signed list, so vendors may have released newer drivers since. \
                 The versions below are from that date; the vendor page has the final word.",
                |_| {},
            );
            ui.add_space(6.0);
        }
        Some(l) => {
            let line = if c.demo {
                "Vendor list: demo preview (demo instances never download it)".to_string()
            } else {
                format!(
                    "Vendor list #{} · checked by FanRGB {} · signed ({} key)",
                    l.serial,
                    date(l.generated_at),
                    if l.key == 0 { "catalog" } else { "backup catalog" }
                )
            };
            text::caption3(ui, line);
        }
        None => {
            text::caption3(ui, "Vendor list not downloaded yet · Check now gets it");
        }
    }
    if let Some(e) = &c.error
        && !c.demo
    {
        muted_row(ui, pc, &format!("Couldn't update the vendor list · {} {}", e.code.id(), e.code.title()), true);
    }
    let _ = p;
}

fn drivers_card(ui: &mut Ui, pc: &mut PageCtx) {
    let p = pal(ui);
    let Some(svc) = crate::drivers::service() else {
        card(ui, |ui| {
            text::label(ui, "Drivers", Ts::Headline, p.text1);
            text::para(ui, "The driver list is not available in this instance.", Ts::Body, p.text2);
        });
        return;
    };
    let st: DriverState = svc.snapshot();
    if st.checking || !st.inventory_done {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    }
    let offers = st.updates_available();
    let checked = st.last_checked.is_some();
    card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.set_min_height(34.0);
            text::label(ui, "Drivers", Ts::Title, p.text1);
            ui.add_space(6.0);
            if offers > 0 {
                controls::dot_chip(ui, &format!("{offers} update{}", if offers == 1 { "" } else { "s" }), Tone::Info);
            }
            if st.catalog.demo {
                controls::chip(ui, "Demo preview", Tone::Warning)
                    .on_hover_text("Demo instances never download the vendor list; this shows what it looks like.");
            }
            let unknown = st
                .rows
                .iter()
                .filter(|r| {
                    (r.package || r.category != DriverCategory::Chipset)
                        && matches!(row_chip(r, checked).0, "Unknown" | "Not in list")
                })
                .count();
            if unknown > 0 && st.inventory_done {
                controls::dot_chip(ui, &format!("{unknown} not in list"), Tone::Neutral).on_hover_text(
                    "Neither the vendor list nor Windows Update has a version for these: check the vendor page.",
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = if st.checking { "Checking…" } else { "Check now" };
                if ui
                    .add_enabled_ui(!st.checking, |ui| {
                        Button::new(Kind::Secondary, label).icon(Icon::Refresh).loading(st.checking).show(ui)
                    })
                    .inner
                    .on_hover_text(
                        "Download FanRGB's signed vendor list, then ask Windows Update for newer drivers \
                         (sends this PC's hardware IDs to Microsoft, as Windows Update does)",
                    )
                    .clicked()
                {
                    svc.check_now();
                }
                if controls::toggle(ui, &mut pc.cfg.drivers.auto_check).clicked() {
                    svc.set_auto(pc.cfg.drivers.auto_check);
                }
                text::label(ui, "Check Windows Update daily", Ts::Label, p.text2);
            });
        });
        let when = match st.last_checked {
            Some(t) => format!("Windows Update checked {}.", ago(t)),
            None => "Windows Update not checked yet.".into(),
        };
        text::para(
            ui,
            format!(
                "Inform only: FanRGB never downloads or installs drivers. Installed versions come from Windows; \
                 “Latest” is FanRGB's signed list of the vendors' newest versions (checked weekly on their pages), \
                 and the vendor page has the final word. {when}"
            ),
            Ts::Caption,
            p.text2,
        );
        catalog_line(ui, pc, &st);
        ui.add_space(8.0);
        if let Some(e) = &st.error {
            muted_row(ui, pc, &format!("Couldn't check Windows Update · {} {}", e.code.id(), e.code.title()), true);
        }
        if let Some(e) = &st.inventory_error {
            muted_row(ui, pc, &format!("Couldn't read installed drivers · {} {}", e.code.id(), e.code.title()), false);
        }
        if !st.inventory_done {
            text::label(ui, "Reading installed drivers…", Ts::Body, p.text2);
            return;
        }
        // ---- table ----
        let w = ui.available_width();
        let narrow = w < 760.0;
        let wide = w >= 1080.0;
        // The "Vendor page" button has its own space at the right; six columns share the rest.
        let btn_w = 158.0;
        let cols: [f32; 6] = if narrow {
            [0.0, 0.42, 0.25, 0.0, 0.0, 0.33]
        } else if wide {
            [0.14, 0.24, 0.14, 0.17, 0.16, 0.15]
        } else {
            [0.0, 0.30, 0.16, 0.20, 0.17, 0.17]
        };
        let inline_class = cols[0] == 0.0;
        let table_w = |r: Rect| (r.width() - 24.0 - if narrow { 0.0 } else { btn_w }).max(200.0);
        let x_of = |k: usize, r: Rect| r.left() + 12.0 + cols[..k].iter().sum::<f32>() * table_w(r);
        let (hr, _) = ui.allocate_exact_size(vec2(w, 30.0), Sense::hover());
        ui.painter().rect_filled(hr, R_SM, p.surface2);
        for (k, h) in ["Component", "Device", "Installed", "Latest", "Windows Update", "Status"].iter().enumerate() {
            if cols[k] > 0.0 {
                ui.painter().galley(
                    pos2(x_of(k, hr), hr.center().y - 7.0),
                    ui.painter().layout_no_wrap(h.to_uppercase(), font(Ts::Overline), p.text3),
                    p.text3,
                );
            }
        }
        let show_parts_id = Id::new("drivers_show_chipset_parts");
        let show_parts: bool = ui.data(|d| d.get_temp(show_parts_id)).unwrap_or(false);
        let parts = st.rows.iter().filter(|r| r.category == DriverCategory::Chipset && !r.package).count();
        let has_package = st.rows.iter().any(|r| r.package);
        for r in sorted(&st.rows) {
            // The AMD chipset package stands for its components (mockup: one Chipset row); they fold out.
            if has_package && !show_parts && r.category == DriverCategory::Chipset && !r.package {
                continue;
            }
            let (row, resp) = ui.allocate_exact_size(vec2(w, 54.0), Sense::hover());
            if resp.hovered() {
                ui.painter().rect_filled(row, R_MD, p.surface2);
            }
            ui.painter().line_segment([row.left_bottom(), row.right_bottom()], Stroke::new(1.0, p.border_subtle));
            let cy = row.center().y;
            let tw = table_w(row);
            let painter = ui.painter();
            // component
            if !inline_class {
                let dx = x_of(0, row);
                let tile = Rect::from_min_size(pos2(dx, cy - 14.0), vec2(28.0, 28.0));
                painter.rect_filled(tile, 7.0, p.surface2);
                painter.rect_stroke(tile, 7.0, Stroke::new(1.0, p.border_subtle), StrokeKind::Inside);
                icons::paint(painter, tile.shrink(6.0), category_icon(r.category), p.text1);
                painter.text(
                    pos2(dx + 38.0, cy),
                    Align2::LEFT_CENTER,
                    r.category.label(),
                    font(Ts::BodyStrong),
                    p.text1,
                );
            }
            // device (+ category when there is no component column)
            let dev_x = x_of(1, row);
            let dev_w = (cols[1] * tw - 10.0).max(60.0);
            let name = if r.count > 1 { format!("{} (×{})", r.device, r.count) } else { r.device.clone() };
            let g = painter.layout(name, font(Ts::Body), p.text1, dev_w);
            let gh = g.size().y;
            let top = if inline_class { cy - gh / 2.0 - 7.0 } else { cy - gh / 2.0 };
            painter.galley(pos2(dev_x, top), g, p.text1);
            if inline_class {
                painter.text(
                    pos2(dev_x, top + gh + 2.0),
                    Align2::LEFT_TOP,
                    r.category.label(),
                    font(Ts::Caption),
                    p.text3,
                );
            }
            // installed
            let ix = x_of(2, row);
            painter.text(pos2(ix, cy - 8.0), Align2::LEFT_CENTER, &r.installed, tab(Ts::Body), p.text2);
            if let Some(d) = &r.installed_date {
                painter.text(pos2(ix, cy + 10.0), Align2::LEFT_CENTER, d, tab(Ts::Caption), p.text3);
            }
            // latest (vendor list): version, then vendor · release date
            if cols[3] > 0.0 {
                let lx = x_of(3, row);
                match (&r.catalog, r.catalog_status) {
                    (Some(m), s) => {
                        let c = if s == CatalogStatus::Newer { p.text1 } else { p.text2 };
                        painter.text(pos2(lx, cy - 8.0), Align2::LEFT_CENTER, &m.version, tab(Ts::BodyStrong), c);
                        painter.text(
                            pos2(lx, cy + 10.0),
                            Align2::LEFT_CENTER,
                            m.source_line(),
                            font(Ts::Caption),
                            p.text3,
                        );
                    }
                    (None, _) => {
                        painter.text(pos2(lx, cy), Align2::LEFT_CENTER, "—", tab(Ts::BodyStrong), p.text3);
                    }
                }
            }
            // Windows Update
            if cols[4] > 0.0 {
                let (t, tone) = wu_text(r, checked);
                let c = if tone == Tone::Info { controls::tone_colors(p, Tone::Info).1 } else { p.text2 };
                let g = painter.layout(t, font(Ts::Caption), c, (cols[4] * tw - 10.0).max(60.0));
                painter.galley(pos2(x_of(4, row), cy - g.size().y / 2.0), g, c);
            }
            // status chip
            let (label, tone) = row_chip(r, checked);
            let (fill, fg) = controls::tone_colors(p, tone);
            let sx = x_of(5, row);
            let sg = painter.layout_no_wrap(label.to_string(), font(Ts::Caption), fg);
            let chip = Rect::from_min_size(pos2(sx, cy - 11.0), vec2(sg.size().x + 26.0, 22.0));
            painter.rect_filled(chip, 11.0, fill);
            painter.circle_filled(pos2(chip.left() + 10.0, cy), 3.0, fg);
            painter.galley(pos2(chip.left() + 18.0, cy - sg.size().y / 2.0), sg, fg);
            let vendor_line = r.catalog.as_ref().map(|m| {
                format!(
                    "\nVendor list: {} · {} · vendor page checked {}",
                    crate::drivers::catalog::status_sentence(r),
                    m.source,
                    date(m.checked_at)
                )
            });
            let tip = format!(
                "{}\n{}{}{}{}",
                r.provider,
                if r.inf.is_empty() { String::new() } else { format!("{} · ", r.inf) },
                r.signer.as_deref().unwrap_or("signer unknown"),
                vendor_line.unwrap_or_default(),
                r.latest.as_ref().map(|o| format!("\nWindows Update: {}", o.title)).unwrap_or_default()
            );
            let _ = resp.on_hover_text(tip);
            if !narrow && let Some(url) = page_for(r) {
                let br = Rect::from_min_size(pos2(row.right() - btn_w, cy - 16.0), vec2(btn_w - 8.0, 32.0));
                let mut child = ui.new_child(
                    egui::UiBuilder::new().max_rect(br).layout(egui::Layout::right_to_left(egui::Align::Center)),
                );
                let kind = if actionable(r) { Kind::Secondary } else { Kind::Ghost };
                if Button::new(kind, "Vendor page")
                    .icon(Icon::ExternalLink)
                    .show(&mut child)
                    .on_hover_text(&url)
                    .clicked()
                {
                    let _ = crate::util::win::open_vendor_url(&url);
                }
            }
        }
        if has_package && parts > 0 {
            ui.add_space(6.0);
            let label = if show_parts {
                "Hide the chipset components".to_string()
            } else {
                format!("Show {parts} chipset components")
            };
            if controls::link(ui, &label).clicked() {
                ui.data_mut(|d| d.insert_temp(show_parts_id, !show_parts));
            }
        }
        ui.add_space(10.0);
        page_banner(
            ui,
            Sev::Warning,
            "BIOS updates reset fan settings and can brick the board if interrupted.",
            "Use Q-Flash from the BIOS after reading the vendor notes. FanRGB never updates the BIOS.",
            |_| {},
        );
    });
}

fn muted_row(ui: &mut Ui, pc: &mut PageCtx, msg: &str, retry: bool) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
        icons::paint(ui.painter(), r, Icon::Info, p.text3);
        text::label(ui, msg, Ts::Caption, p.text2);
        if retry
            && controls::link(ui, "Retry").clicked()
            && let Some(svc) = crate::drivers::service()
        {
            svc.check_now();
        }
    });
    let _ = pc;
    ui.add_space(4.0);
}

// ---------------------------------------------------------------------------------------
// Overview banner (once per version; ✕ = Later → sidebar chip + badge)

pub fn banner(ui: &mut Ui, pc: &mut PageCtx) {
    let (st, demo) = view(pc.ui);
    if st.status != Status::Available {
        return;
    }
    let Some(a) = st.available.clone() else { return };
    if pc.prefs.update_banner_dismissed == a.version {
        return;
    }
    let p = pal(ui);
    let t = crate::ui::anim::toggle(ui.ctx(), Id::new("update_banner_in"), true, crate::ui::anim::SLOW);
    ui.add_space(8.0 * (1.0 - t));
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 44.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, R_MD, crate::ui::theme::with_alpha(p.info_subtle, t));
    painter.rect_stroke(
        rect,
        R_MD,
        Stroke::new(1.0, crate::ui::theme::with_alpha(p.info, 0.6 * t)),
        StrokeKind::Inside,
    );
    icons::paint(
        painter,
        Rect::from_center_size(pos2(rect.left() + 22.0, rect.center().y), vec2(18.0, 18.0)),
        Icon::Update,
        p.info,
    );
    let title = format!("FanRGB {} is ready", a.version);
    let g = painter.layout_no_wrap(title, font(Ts::BodyStrong), p.text1);
    let tw = g.size().x;
    painter.galley(pos2(rect.left() + 42.0, rect.center().y - g.size().y / 2.0), g, p.text1);
    let summary = note_lines(&st.notes).first().cloned().unwrap_or_default();
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(Rect::from_min_max(
                pos2(rect.left() + 42.0 + tw + 12.0, rect.top()),
                pos2(rect.right() - 8.0, rect.bottom()),
            ))
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    let ui2 = &mut child;
    ui2.spacing_mut().item_spacing.x = 8.0;
    if controls::icon_button_sized(ui2, Icon::Close, "Later (the sidebar keeps a reminder)", 28.0, None).clicked() {
        pc.prefs.update_banner_dismissed = a.version.clone();
    }
    if Button::new(Kind::Primary, "Update & restart")
        .size(Size::Sm)
        .show(ui2)
        .on_hover_text("Fans return to BIOS control for about 3 s while FanRGB restarts.")
        .clicked()
    {
        if demo {
            pc.ui.toast(ToastKind::Info, "Demo: nothing is downloaded or installed");
        } else {
            pc.ui.pending.push(UiAction::InstallUpdate);
        }
    }
    if Button::new(Kind::Ghost, "What's new").size(Size::Sm).show(ui2).clicked() {
        pc.ui.pending.push(UiAction::ShowWhatsNew);
    }
    let room = ui2.available_width() - 8.0;
    if room > 80.0 {
        let p = pal(ui2);
        let g = ui2.painter().layout(summary, font(Ts::Body), p.text2, room);
        if g.rows.len() <= 1 {
            ui2.painter().galley(pos2(rect.left() + 42.0 + tw + 12.0, rect.center().y - g.size().y / 2.0), g, p.text2);
        }
    }
    ui.add_space(pc.bp.gap());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(category: DriverCategory, status: DriverStatus) -> DriverRow {
        DriverRow {
            category,
            device: "x".into(),
            provider: String::new(),
            installed: "1".into(),
            installed_date: None,
            inf: String::new(),
            signer: None,
            instance_id: String::new(),
            count: 1,
            hardware_ids: Vec::new(),
            latest: None,
            status,
            vendor_url: None,
            ..DriverRow::empty()
        }
    }

    #[test]
    fn no_offer_never_reads_up_to_date() {
        let (l, _) = status_chip(&row(DriverCategory::Gpu, DriverStatus::UpToDate), true);
        assert_eq!(l, "No Windows Update offer");
        let (l, t) = status_chip(&row(DriverCategory::Bios, DriverStatus::UpdateAvailable), true);
        assert!(l != "Update available" && t == Tone::Neutral, "BIOS is never 'Update available'");
    }

    fn with_catalog(mut r: DriverRow, s: CatalogStatus) -> DriverRow {
        r.catalog_status = s;
        r
    }

    #[test]
    fn vendor_list_status_and_windows_update_column() {
        // The vendor list decides the chip; Windows Update keeps its own column and wording.
        let r = with_catalog(row(DriverCategory::Gpu, DriverStatus::UpToDate), CatalogStatus::Newer);
        assert_eq!(row_chip(&r, true), ("Newer available", Tone::Info));
        assert_eq!(wu_text(&r, true).0, "No Windows Update offer");
        let r = with_catalog(row(DriverCategory::Gpu, DriverStatus::UpToDate), CatalogStatus::LatestKnown);
        assert_eq!(row_chip(&r, true).0, "Latest known");
        let r = with_catalog(row(DriverCategory::Audio, DriverStatus::UpToDate), CatalogStatus::NotInList);
        assert_eq!(row_chip(&r, true).0, "Not in list");
        // A Windows Update offer still shows when the list has nothing newer.
        let r = with_catalog(row(DriverCategory::Wifi, DriverStatus::UpdateAvailable), CatalogStatus::NotInList);
        assert_eq!(row_chip(&r, true).0, "Update available");
        assert_eq!(wu_text(&r, true).0, "Offers newer");
        // No list (not downloaded yet): the old Windows Update wording.
        let r = row(DriverCategory::Gpu, DriverStatus::UpToDate);
        assert_eq!(row_chip(&r, true).0, "No Windows Update offer");
        // Windows Update does not cover the BIOS: "—", never "Update available".
        let b = with_catalog(row(DriverCategory::Bios, DriverStatus::UpdateAvailable), CatalogStatus::NotInList);
        assert_eq!(wu_text(&b, true).0, "—");
        assert_eq!(row_chip(&b, true).0, "Not in list");
        let b = with_catalog(row(DriverCategory::Bios, DriverStatus::Unknown), CatalogStatus::Newer);
        assert_eq!(row_chip(&b, true).0, "Newer available");
    }

    #[test]
    fn vendor_page_prefers_a_valid_catalog_link() {
        let mut r = with_catalog(row(DriverCategory::Gpu, DriverStatus::UpToDate), CatalogStatus::Newer);
        r.vendor_url = Some(crate::drivers::URL_INTEL_ARC);
        r.catalog = Some(crate::drivers::catalog::CatalogMatch {
            id: "x".into(),
            vendor: "Intel".into(),
            component: "c".into(),
            version: "32.0.101.9150".into(),
            release_date: None,
            download_url:
                "https://www.intel.com/content/www/us/en/download/785597/929959/intel-arc-graphics-windows.html".into(),
            release_notes_url: None,
            source: "auto:intel-arc".into(),
            checked_at: 0,
        });
        assert!(page_for(&r).unwrap().contains("929959"));
        r.catalog.as_mut().unwrap().download_url = "https://evil.example/x".into();
        assert_eq!(page_for(&r).as_deref(), Some(crate::drivers::URL_INTEL_ARC), "falls back to the pinned page");
        r.catalog_status = CatalogStatus::NotInList;
        assert_eq!(page_for(&r).as_deref(), Some(crate::drivers::URL_INTEL_ARC));
    }

    #[test]
    fn actionable_first_bios_last() {
        let rows = vec![
            row(DriverCategory::Bios, DriverStatus::Unknown),
            row(DriverCategory::Lan, DriverStatus::UpToDate),
            row(DriverCategory::Gpu, DriverStatus::UpdateAvailable),
        ];
        let s = sorted(&rows);
        assert_eq!(s[0].category, DriverCategory::Gpu);
        assert_eq!(s[2].category, DriverCategory::Bios);
    }

    #[test]
    fn notes_are_plain_lines() {
        let l = note_lines("# 0.3.0\n- One\n* Two\n\n• Three [link](http://x)");
        assert_eq!(l, vec!["One", "Two", "Three [link](http://x)"]);
    }
}
