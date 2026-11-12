//! Fan-mode tile row: Silent · Balanced · Performance · Full speed · Auto (AI) · BIOS,
//! then a "Custom ▾" tile that opens the profile list. One focus stop for the whole row: ←/→ move,
//! Enter/Space apply; Alt+1..6 work globally.

use egui::{Align2, Color32, EventFilter, Id, Key, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::config::{FanConfig, Preset, Profile};
use crate::model::FanRole;
use crate::ui::PageCtx;
use crate::ui::actions::UiAction;
use crate::ui::anim;
use crate::ui::fonts::{Ts, Weight, font, sized};
use crate::ui::icons::{self, Icon};
use crate::ui::overlays::{Modal, ToastKind};
use crate::ui::theme::{Palette, R_LG, R_MD, mix, pal};
use crate::ui::widgets::controls::{Button, Kind, Size};
use crate::ui::widgets::text;

pub fn preset_icon(p: Preset) -> Icon {
    match p {
        Preset::Silent => Icon::Leaf,
        Preset::Balanced => Icon::Balance,
        Preset::Performance => Icon::Bolt,
        Preset::FullSpeed => Icon::Gauge,
        Preset::AiAuto => Icon::Forecast,
        Preset::Bios => Icon::Bios,
        Preset::Custom => Icon::Pencil,
    }
}

/// A tile: one of the six modes, or the trailing Custom tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tile {
    Mode(Preset),
    Custom,
}

const TILES: [Tile; 7] = [
    Tile::Mode(Preset::Silent),
    Tile::Mode(Preset::Balanced),
    Tile::Mode(Preset::Performance),
    Tile::Mode(Preset::FullSpeed),
    Tile::Mode(Preset::AiAuto),
    Tile::Mode(Preset::Bios),
    Tile::Custom,
];

/// Why a mode can't be used right now (fan control blocked): the reason line doubles as the fix link.
fn unavailable(pc: &PageCtx, preset: Preset) -> Option<crate::error::ErrorCode> {
    if preset == Preset::Bios {
        return None;
    }
    crate::ui::pages::fan_block(pc.st, &pc.ui.issues)
}

/// Non-BIOS modes wait for the header identification (`EngineState::mode_gate`): the tile stays
/// selectable (choosing it opens the Identify prompt) and says why it does not drive the fans yet.
fn waits_for_identify(pc: &PageCtx, tile: Tile) -> bool {
    tile != Tile::Mode(Preset::Bios) && !pc.st.mode_gate.is_empty()
}

/// Caption of a tile that waits for the identification.
pub const IDENTIFY_FIRST_SHORT: &str = "Identify fans first";

fn reason_text(code: crate::error::ErrorCode) -> &'static str {
    match code {
        crate::error::ErrorCode::PawnIoMissing | crate::error::ErrorCode::PawnIoModule => "Needs PawnIO driver",
        crate::error::ErrorCode::NotElevated => "Needs administrator",
        _ => "Not available here",
    }
}

/// Mini curve glyph (34 × 20) drawn from the preset's real CPU curve.
fn paint_glyph(painter: &egui::Painter, r: Rect, preset: Preset, color: Color32) {
    let stroke = Stroke::new(1.5, color);
    let map = |t: f32, d: f32| pos2(r.left() + (t - 20.0) / 80.0 * r.width(), r.bottom() - d / 100.0 * r.height());
    match preset {
        Preset::FullSpeed => {
            painter.line_segment([pos2(r.left(), r.top() + 3.0), pos2(r.right(), r.top() + 3.0)], stroke);
        }
        Preset::AiAuto => {
            // Solid history, then forecast dashes towards a target dot.
            let pts = [map(22.0, 18.0), map(44.0, 34.0), map(58.0, 26.0)];
            painter.add(egui::Shape::line(pts.to_vec(), stroke));
            for (a, b) in [((64.0, 36.0), (72.0, 46.0)), ((78.0, 54.0), (86.0, 64.0))] {
                painter.line_segment([map(a.0, a.1), map(b.0, b.1)], stroke);
            }
            painter.circle_filled(map(94.0, 74.0), 2.2, color);
        }
        Preset::Bios => {
            let pts = [map(20.0, 12.0), map(52.0, 36.0), map(74.0, 58.0), map(100.0, 94.0)];
            painter.add(egui::Shape::line(pts.to_vec(), stroke));
        }
        p => {
            let fc = FanConfig::preset(p, FanRole::Cpu);
            let mut pts: Vec<Pos2> = vec![map(20.0, fc.curve.points.first().map(|q| q[1]).unwrap_or(30.0))];
            pts.extend(fc.curve.points.iter().map(|q| map(q[0].clamp(20.0, 100.0), q[1])));
            pts.push(map(100.0, fc.curve.points.last().map(|q| q[1]).unwrap_or(100.0)));
            painter.add(egui::Shape::line(pts, stroke));
        }
    }
}

fn tile_colors(p: &Palette, selected: f32, hover: f32) -> (Color32, Color32, f32) {
    let fill = mix(mix(p.surface1, p.surface2, hover), p.accent_subtle, selected);
    let border = mix(mix(p.border_subtle, p.border_strong, hover), p.accent, selected);
    let width = 1.0 + 0.5 * selected;
    (fill, border, width)
}

/// The tile row. `compact` = Cooling (64 pt tiles), else Overview (74 pt).
pub fn mode_bar(ui: &mut Ui, pc: &mut PageCtx, compact: bool) {
    let p = pal(ui);
    let id = Id::new("fan_mode_bar");
    let h = if compact { crate::ui::theme::TILE_H_COMPACT } else { crate::ui::theme::TILE_H };
    let gap = 8.0;
    let n = TILES.len();
    let avail = ui.available_width();
    // Narrow: a horizontal strip of 112 pt tiles (scrolls); otherwise equal widths.
    let equal_w = (avail - gap * (n as f32 - 1.0)) / n as f32;
    let tile_w = if equal_w < 112.0 { 112.0 } else { equal_w };
    let total_w = tile_w * n as f32 + gap * (n as f32 - 1.0);
    let mode = pc.cfg.fan_mode();
    let released = pc.st.emergency_released;
    let custom_count = pc.cfg.profiles.iter().filter(|pr| pr.preset == Preset::Custom).count();
    let selected_tile = if released {
        None
    } else if mode.preset == Preset::Custom {
        Some(n - 1)
    } else {
        TILES.iter().position(|t| *t == Tile::Mode(mode.preset))
    };

    let mut body = |ui: &mut Ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(total_w, h + 4.0), Sense::hover());
        let rect = Rect::from_min_size(rect.min + vec2(0.0, 2.0), vec2(total_w, h));
        let resp = ui.interact(rect, id, Sense::click());
        let cursor_key = id.with("cursor");
        let mut cursor: usize = ui.data(|d| d.get_temp(cursor_key)).unwrap_or(selected_tile.unwrap_or(1));
        let tile_rect =
            |i: usize| Rect::from_min_size(pos2(rect.left() + i as f32 * (tile_w + gap), rect.top()), vec2(tile_w, h));
        let hovered = resp.hover_pos().and_then(|pos| (0..n).find(|&i| tile_rect(i).contains(pos)));
        let mut activate: Option<usize> = None;
        if resp.clicked() {
            activate = resp.interact_pointer_pos().and_then(|pos| (0..n).find(|&i| tile_rect(i).contains(pos)));
            if let Some(i) = activate {
                cursor = i;
            }
        }
        if resp.has_focus() {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(id, EventFilter { horizontal_arrows: true, ..Default::default() })
            });
            let (left, right, home, end, enter) = ui.input(|i| {
                (
                    i.key_pressed(Key::ArrowLeft),
                    i.key_pressed(Key::ArrowRight),
                    i.key_pressed(Key::Home),
                    i.key_pressed(Key::End),
                    i.key_pressed(Key::Enter) || i.key_pressed(Key::Space),
                )
            });
            if left {
                cursor = cursor.saturating_sub(1);
            }
            if right {
                cursor = (cursor + 1).min(n - 1);
            }
            if home {
                cursor = 0;
            }
            if end {
                cursor = n - 1;
            }
            if enter {
                activate = Some(cursor);
            }
        }
        ui.data_mut(|d| d.insert_temp(cursor_key, cursor));

        let painter = ui.painter();
        for (i, tile) in TILES.iter().enumerate() {
            let r = tile_rect(i);
            if !ui.is_rect_visible(r) {
                continue;
            }
            let is_sel = selected_tile == Some(i);
            let sel = anim::toggle(ui.ctx(), id.with(("sel", i)), is_sel, anim::BASE);
            let hov = anim::toggle(ui.ctx(), id.with(("hov", i)), hovered == Some(i), anim::FAST);
            let blocked = match tile {
                Tile::Mode(pr) => unavailable(pc, *pr),
                Tile::Custom => None,
            };
            let (fill, border, bw) = tile_colors(p, sel, if blocked.is_some() { 0.0 } else { hov });
            painter.rect_filled(r, R_LG, fill);
            painter.rect_stroke(r, R_LG, Stroke::new(bw, border), StrokeKind::Inside);
            if resp.has_focus() && cursor == i {
                painter.rect_stroke(r.expand(3.0), R_LG + 3, Stroke::new(2.0, p.focus), StrokeKind::Outside);
            }
            let (name, desc, icon) = match tile {
                Tile::Mode(pr) => (pr.label().to_string(), pr.descriptor().to_string(), Some(preset_icon(*pr))),
                Tile::Custom => (
                    if mode.preset == Preset::Custom && !released { mode.label.clone() } else { "Custom".into() },
                    format!("{custom_count} saved"),
                    None,
                ),
            };
            let name_c = if blocked.is_some() { p.text_disabled } else { p.text1 };
            let icon_c = if blocked.is_some() {
                p.text_disabled
            } else if is_sel {
                p.accent
            } else {
                p.text2
            };
            let top = r.top() + if compact { 22.0 } else { 26.0 };
            let mut x = r.left() + 14.0;
            if let Some(ic) = icon {
                icons::paint(painter, Rect::from_center_size(pos2(x + 9.0, top), vec2(18.0, 18.0)), ic, icon_c);
                x += 26.0;
            }
            let max_name = r.right() - x - if *tile == Tile::Custom || is_sel { 26.0 } else { 8.0 };
            // 15 pt SemiBold, stepping down (never wrapping) when the tile is narrow.
            let mut g = painter.layout_no_wrap(name.clone(), sized(15.0, Weight::SemiBold, false), name_c);
            for size in [14.0, 13.0, 12.0] {
                if g.size().x <= max_name {
                    break;
                }
                g = painter.layout_no_wrap(name.clone(), sized(size, Weight::SemiBold, false), name_c);
            }
            painter.galley(pos2(x, top - g.size().y / 2.0), g, name_c);
            let bottom = r.bottom() - if compact { 20.0 } else { 22.0 };
            let (desc, desc_c) = match blocked {
                Some(code) => (reason_text(code).to_string(), p.accent),
                None if waits_for_identify(pc, *tile) => (IDENTIFY_FIRST_SHORT.to_string(), p.accent),
                None => (desc, p.text3),
            };
            painter.text(pos2(r.left() + 14.0, bottom), Align2::LEFT_CENTER, desc, font(Ts::Caption), desc_c);
            match tile {
                Tile::Mode(pr) if blocked.is_none() || *pr == Preset::Bios => {
                    let gr = Rect::from_min_size(pos2(r.right() - 48.0, bottom - 12.0), vec2(34.0, 20.0));
                    let gc = if is_sel { p.accent } else { p.text3 };
                    paint_glyph(painter, gr, *pr, gc);
                }
                Tile::Mode(_) => {}
                Tile::Custom => {
                    icons::paint(
                        painter,
                        Rect::from_center_size(pos2(r.right() - 22.0, top), vec2(14.0, 14.0)),
                        Icon::ChevronDown,
                        p.text2,
                    );
                }
            }
            if is_sel && *tile != Tile::Custom {
                let k = 0.6 + 0.4 * sel;
                let c = pos2(r.right() - 16.0, r.top() + 15.0);
                painter.circle_filled(c, 7.0 * k, p.accent);
                icons::paint(painter, Rect::from_center_size(c, vec2(10.0 * k, 10.0 * k)), Icon::Check, p.on_accent);
            }
        }
        // Tooltip with the shortcut for the hovered tile.
        let resp = if let Some(i) = hovered {
            let tip = match TILES[i] {
                Tile::Mode(pr) => {
                    let n = Preset::MODES.iter().position(|m| *m == pr).unwrap_or(0) + 1;
                    match unavailable(pc, pr) {
                        Some(code) => format!("{} · {} ({})", pr.label(), code.title(), code.id()),
                        None if waits_for_identify(pc, TILES[i]) => {
                            format!("{} · {}  (Alt+{n})", pr.label(), crate::engine::IDENTIFY_FIRST)
                        }
                        None => format!("{} · {}  (Alt+{n})", pr.label(), crate::ui::pages::profile_description(pr)),
                    }
                }
                Tile::Custom if waits_for_identify(pc, Tile::Custom) => {
                    format!("Your saved profiles · Ctrl+P · {}", crate::engine::IDENTIFY_FIRST)
                }
                Tile::Custom => "Your saved profiles · Ctrl+P".into(),
            };
            resp.on_hover_text(tip)
        } else {
            resp
        };
        let label = format!("Fan mode: {}", if released { "BIOS (released)".to_string() } else { mode.label.clone() });
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::RadioGroup, true, &label));

        let popup_id = Id::new("custom_profiles_popup");
        if let Some(i) = activate {
            match TILES[i] {
                Tile::Mode(pr) => match unavailable(pc, pr) {
                    Some(code) => pc.ui.pending.push(UiAction::Fix(code.fix(), None)),
                    None => {
                        if Some(i) != selected_tile {
                            // (A mode that waits for the identification opens its prompt: `Ui::frame`.)
                            pc.ui.pending.push(UiAction::ApplyPreset(pr));
                        } else if released {
                            pc.ui.pending.push(UiAction::Resume);
                        } else if waits_for_identify(pc, TILES[i]) {
                            pc.ui.pending.push(UiAction::IdentifyHeaders);
                        }
                    }
                },
                Tile::Custom => egui::Popup::toggle_id(ui.ctx(), popup_id),
            }
        }
        custom_popup(ui, pc, popup_id, tile_rect(n - 1));
    };
    if tile_w > equal_w + 0.5 {
        egui::ScrollArea::horizontal().id_salt(id.with("scroll")).show(ui, |ui| body(ui));
    } else {
        body(ui);
    }
}

/// Custom ▾: saved profiles plus New / Duplicate / Rename / Delete (moved here from the old
/// profile bar).
fn custom_popup(ui: &mut Ui, pc: &mut PageCtx, popup_id: Id, anchor: Rect) {
    let ctx = ui.ctx().clone();
    egui::Popup::new(popup_id, ctx, anchor, ui.layer_id())
        .open_memory(None)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .align(egui::RectAlign::BOTTOM_END)
        .gap(4.0)
        .width(280.0)
        .show(|ui| {
            let p = pal(ui);
            ui.set_width(280.0);
            text::overline(ui, "Saved profiles");
            let active = pc.cfg.active_profile;
            let customs: Vec<(usize, String)> = pc
                .cfg
                .profiles
                .iter()
                .enumerate()
                .filter(|(i, pr)| pr.preset == Preset::Custom || pc.cfg.preset_profile(pr.preset) != Some(*i))
                .map(|(i, pr)| (i, pr.name.clone()))
                .collect();
            if customs.is_empty() {
                text::para(ui, "No custom profiles yet. Duplicate a mode to start one.", Ts::Caption, p.text2);
            }
            for (i, name) in &customs {
                let (r, resp) = ui.allocate_exact_size(vec2(280.0, 34.0), Sense::click());
                if resp.hovered() {
                    ui.painter().rect_filled(r, R_MD, p.surface2);
                }
                ui.painter().text(
                    pos2(r.left() + 10.0, r.center().y),
                    Align2::LEFT_CENTER,
                    name,
                    font(Ts::Body),
                    p.text1,
                );
                if *i == active && !pc.st.emergency_released {
                    icons::paint(
                        ui.painter(),
                        Rect::from_center_size(pos2(r.right() - 16.0, r.center().y), vec2(14.0, 14.0)),
                        Icon::Check,
                        p.accent,
                    );
                }
                if resp.clicked() {
                    pc.ui.pending.push(UiAction::SetProfile(*i));
                    if pc.st.emergency_released {
                        pc.ui.pending.push(UiAction::Resume);
                    }
                    ui.close();
                }
            }
            crate::ui::widgets::card::divider(ui);
            let idx = pc.cfg.active_profile;
            let prof_name = pc.cfg.profile().name.clone();
            let is_custom = pc.cfg.profile().preset == Preset::Custom;
            if Button::new(Kind::Ghost, "Save current as custom…")
                .icon(Icon::Copy)
                .size(Size::Sm)
                .min_width(280.0)
                .show(ui)
                .clicked()
            {
                duplicate_active(pc);
                ui.close();
            }
            if Button::new(Kind::Ghost, "New empty profile")
                .icon(Icon::Plus)
                .size(Size::Sm)
                .min_width(280.0)
                .show(ui)
                .clicked()
            {
                pc.cfg.profiles.push(Profile::new("My profile", Preset::Custom));
                pc.cfg.active_profile = pc.cfg.profiles.len() - 1;
                pc.ui.modal = Some(Modal::RenameProfile { index: pc.cfg.active_profile, buf: "My profile".into() });
                ui.close();
            }
            if is_custom {
                if Button::new(Kind::Ghost, &format!("Rename “{prof_name}”…"))
                    .icon(Icon::Pencil)
                    .size(Size::Sm)
                    .min_width(280.0)
                    .show(ui)
                    .clicked()
                {
                    pc.ui.modal = Some(Modal::RenameProfile { index: idx, buf: prof_name.clone() });
                    ui.close();
                }
                if pc.cfg.profiles.len() > 1
                    && Button::new(Kind::Danger, &format!("Delete “{prof_name}”…"))
                        .icon(Icon::Trash)
                        .size(Size::Sm)
                        .min_width(280.0)
                        .show(ui)
                        .clicked()
                {
                    pc.ui.modal = Some(Modal::ConfirmDeleteProfile(idx));
                    ui.close();
                }
            }
        });
}

/// Copies the active profile (with every fan's effective settings) into a new custom profile.
pub fn duplicate_active(pc: &mut PageCtx) {
    let mut np = pc.cfg.profile().clone();
    for f in &pc.st.fans {
        let c = np.fan(&f.id, f.role);
        np.fans.entry(f.id.clone()).or_insert(c);
    }
    let base = if np.preset == Preset::Custom { np.name.clone() } else { np.preset.label().to_string() };
    np.name = format!("{base} (custom)");
    np.preset = Preset::Custom;
    pc.cfg.profiles.push(np);
    pc.cfg.active_profile = pc.cfg.profiles.len() - 1;
    let i = pc.cfg.active_profile;
    let name = pc.cfg.profiles[i].name.clone();
    pc.ui.modal = Some(Modal::RenameProfile { index: i, buf: name });
    pc.ui.toast(ToastKind::Success, "Saved as a custom profile");
}

/// Overline + caption above the Overview tiles.
pub fn section_header(ui: &mut Ui, right: &str) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        text::overline(ui, "Fan mode");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            text::label(ui, right, Ts::Caption, p.text3);
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiles_cover_every_mode_once_in_shortcut_order() {
        let modes: Vec<Preset> =
            TILES.iter().filter_map(|t| if let Tile::Mode(p) = t { Some(*p) } else { None }).collect();
        assert_eq!(modes, Preset::MODES.to_vec());
        assert_eq!(TILES.last(), Some(&Tile::Custom));
    }

    #[test]
    fn selected_tile_is_visibly_different_in_both_themes() {
        for p in [&crate::ui::theme::DARK, &crate::ui::theme::LIGHT] {
            let (f0, b0, w0) = tile_colors(p, 0.0, 0.0);
            let (f1, b1, w1) = tile_colors(p, 1.0, 0.0);
            assert_ne!(f0, f1);
            assert_eq!(b1, p.accent);
            assert!(w1 > w0);
            let _ = b0;
        }
    }
}
