//! LED color picker: hue ring + saturation/value square (vertex-colored meshes), hex/RGB/HSV fields,
//! 12 LED-tuned presets and the 8 most recent colors.

use std::f32::consts::{PI, TAU};

use egui::epaint::Mesh;
use egui::{Color32, EventFilter, Id, Key, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, vec2};

use crate::ui::fonts::{Ts, font};
use crate::ui::theme::{R_SM, pal};
use crate::ui::widgets::controls::{self, segmented_sized};
use crate::ui::widgets::text;

// ---------------------------------------------------------------------------------------
// Conversions (unit tested)

/// sRGB bytes → (hue 0..1, saturation 0..1, value 0..1).
pub fn rgb_to_hsv(c: [u8; 3]) -> [f32; 3] {
    let (r, g, b) = (c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d <= 1e-6 {
        0.0
    } else if max == r {
        ((g - b) / d).rem_euclid(6.0) / 6.0
    } else if max == g {
        ((b - r) / d + 2.0) / 6.0
    } else {
        ((r - g) / d + 4.0) / 6.0
    };
    let s = if max <= 1e-6 { 0.0 } else { d / max };
    [h, s, max]
}

pub fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [u8; 3] {
    crate::rgb::color::Rgb::hsv(h, s.clamp(0.0, 1.0), v.clamp(0.0, 1.0)).to_u8()
}

/// Accepts `#RGB`, `RGB`, `#RRGGBB`, `RRGGBB` (case-insensitive, surrounding spaces ignored).
pub fn parse_hex(s: &str) -> Option<[u8; 3]> {
    let s = s.trim().trim_start_matches('#');
    let digits: Vec<u8> = s.chars().map(|c| c.to_digit(16).map(|d| d as u8)).collect::<Option<Vec<u8>>>()?;
    match digits.len() {
        3 => Some([digits[0] * 17, digits[1] * 17, digits[2] * 17]),
        6 => Some([digits[0] * 16 + digits[1], digits[2] * 16 + digits[3], digits[4] * 16 + digits[5]]),
        _ => None,
    }
}

pub fn to_hex(c: [u8; 3]) -> String {
    format!("#{:02X}{:02X}{:02X}", c[0], c[1], c[2])
}

/// LED-tuned presets (no near-black).
pub const PRESETS: [[u8; 3]; 12] = [
    [0xFF, 0xB4, 0x6B],
    [0xFF, 0xFF, 0xFF],
    [0xFF, 0x1E, 0x1E],
    [0xFF, 0x6A, 0x00],
    [0xFF, 0xB0, 0x00],
    [0xFF, 0xE6, 0x00],
    [0x7C, 0xFF, 0x00],
    [0x00, 0xFF, 0x5A],
    [0x00, 0xE5, 0xFF],
    [0x00, 0x50, 0xFF],
    [0x8A, 0x2B, 0xFF],
    [0xFF, 0x00, 0xC8],
];

pub const PRESET_NAMES: [&str; 12] =
    ["Warm white", "White", "Red", "Orange", "Amber", "Yellow", "Lime", "Green", "Cyan", "Blue", "Violet", "Magenta"];

/// Pushes `c` to the front of the recent list (deduplicated, max 8).
pub fn push_recent(recent: &mut Vec<[u8; 3]>, c: [u8; 3]) {
    recent.retain(|x| *x != c);
    recent.insert(0, c);
    recent.truncate(8);
}

// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum Fields {
    #[default]
    Rgb,
    Hsv,
}

#[derive(Clone, Debug, Default)]
struct PickerState {
    hsv: [f32; 3],
    last_rgb: Option<[u8; 3]>,
    hex: String,
    hex_err: bool,
    drag: u8, // 0 none, 1 ring, 2 square
    fields: Fields,
    original: Option<[u8; 3]>,
    was_open: bool,
}

fn c32(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

/// Swatch button that opens the picker popover. Returns true when the color changed.
pub fn swatch_button(ui: &mut Ui, id: Id, color: &mut [u8; 3], recent: &mut Vec<[u8; 3]>, tooltip: &str) -> bool {
    let p = pal(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::hover());
    let resp = ui.interact(rect, id, Sense::click());
    ui.painter().rect_filled(rect, R_SM, c32(*color));
    ui.painter().rect_stroke(
        rect,
        R_SM,
        Stroke::new(1.0, if resp.hovered() { p.border_strong } else { p.border }),
        StrokeKind::Inside,
    );
    let resp = if tooltip.is_empty() { resp } else { resp.on_hover_text(tooltip) };
    let mut changed = false;
    let popup_id = egui::Popup::default_response_id(&resp);
    let st_id = id.with("picker");
    let open = egui::Popup::from_toggle_button_response(&resp)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .gap(6.0)
        .show(|ui| {
            ui.set_width(272.0);
            changed |= picker(ui, st_id, color, recent);
        })
        .is_some();
    // Remember the color as "recent" when the popover closes after a change.
    let mut st: PickerState = ui.data(|d| d.get_temp(st_id)).unwrap_or_default();
    if open && !st.was_open {
        st.original = Some(*color);
    }
    if !open && st.was_open {
        if st.original.is_some_and(|o| o != *color) {
            push_recent(recent, *color);
        }
        st.original = None;
    }
    st.was_open = open;
    let _ = popup_id;
    ui.data_mut(|d| d.insert_temp(st_id, st));
    changed
}

/// The picker body (also usable inline).
pub fn picker(ui: &mut Ui, id: Id, color: &mut [u8; 3], recent: &mut [[u8; 3]]) -> bool {
    let p = pal(ui);
    let mut st: PickerState = ui.data(|d| d.get_temp(id)).unwrap_or_default();
    if st.last_rgb != Some(*color) {
        // External change: re-derive HSV but keep hue when grey/black.
        let hsv = rgb_to_hsv(*color);
        st.hsv = if hsv[1] < 1e-3 || hsv[2] < 1e-3 { [st.hsv[0], hsv[1], hsv[2]] } else { hsv };
        st.hex = to_hex(*color);
        st.hex_err = false;
        st.last_rgb = Some(*color);
    }
    let before = *color;

    // ---- wheel ----
    let side = 208.0;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), side), Sense::hover());
    let area = Rect::from_center_size(rect.center(), vec2(side, side));
    let resp = ui.interact(area, id.with("wheel"), Sense::click_and_drag());
    let c = area.center();
    let (ro, ri) = (side / 2.0, side / 2.0 - 18.0);
    let sq = 116.0;
    let sq_rect = Rect::from_center_size(c, vec2(sq, sq));
    let painter = ui.painter();

    let mut ring = Mesh::default();
    let n = 120;
    for k in 0..n {
        let (h0, h1) = (k as f32 / n as f32, (k + 1) as f32 / n as f32);
        let (a0, a1) = (h0 * TAU - PI / 2.0, h1 * TAU - PI / 2.0);
        let (c0, c1) = (c32(hsv_to_rgb(h0, 1.0, 1.0)), c32(hsv_to_rgb(h1, 1.0, 1.0)));
        let i = ring.vertices.len() as u32;
        ring.colored_vertex(c + vec2(a0.cos(), a0.sin()) * ri, c0);
        ring.colored_vertex(c + vec2(a0.cos(), a0.sin()) * ro, c0);
        ring.colored_vertex(c + vec2(a1.cos(), a1.sin()) * ro, c1);
        ring.colored_vertex(c + vec2(a1.cos(), a1.sin()) * ri, c1);
        ring.add_triangle(i, i + 1, i + 2);
        ring.add_triangle(i, i + 2, i + 3);
    }
    painter.add(Shape::mesh(ring));

    let mut sv = Mesh::default();
    let g = 12;
    for yi in 0..=g {
        for xi in 0..=g {
            let (s, v) = (xi as f32 / g as f32, 1.0 - yi as f32 / g as f32);
            sv.colored_vertex(sq_rect.min + vec2(s * sq, (1.0 - v) * sq), c32(hsv_to_rgb(st.hsv[0], s, v)));
        }
    }
    for yi in 0..g {
        for xi in 0..g {
            let i = (yi * (g + 1) + xi) as u32;
            let w = (g + 1) as u32;
            sv.add_triangle(i, i + 1, i + w + 1);
            sv.add_triangle(i, i + w + 1, i + w);
        }
    }
    painter.add(Shape::mesh(sv));

    // interaction
    let (pressed, ptr, origin) =
        ui.input(|i| (i.pointer.primary_pressed(), i.pointer.interact_pos(), i.pointer.press_origin()));
    if pressed && resp.is_pointer_button_down_on() {
        resp.request_focus();
        if let Some(o) = origin {
            let d = o.distance(c);
            st.drag = if sq_rect.expand(4.0).contains(o) {
                2
            } else if d >= ri - 6.0 && d <= ro + 6.0 {
                1
            } else {
                0
            };
        }
    }
    if resp.is_pointer_button_down_on() || resp.dragged() {
        if let Some(pos) = ptr {
            match st.drag {
                1 => {
                    let v = pos - c;
                    st.hsv[0] = ((v.y.atan2(v.x) + PI / 2.0) / TAU).rem_euclid(1.0);
                }
                2 => {
                    st.hsv[1] = ((pos.x - sq_rect.left()) / sq).clamp(0.0, 1.0);
                    st.hsv[2] = 1.0 - ((pos.y - sq_rect.top()) / sq).clamp(0.0, 1.0);
                }
                _ => {}
            }
        }
    } else {
        st.drag = 0;
    }
    if resp.has_focus() {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                resp.id,
                EventFilter { horizontal_arrows: true, vertical_arrows: true, ..Default::default() },
            )
        });
        let (shift, l, r, u, d) = ui.input(|i| {
            (
                i.modifiers.shift,
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
            )
        });
        if shift {
            let dh = if l || d {
                -5.0 / 360.0
            } else if r || u {
                5.0 / 360.0
            } else {
                0.0
            };
            st.hsv[0] = (st.hsv[0] + dh).rem_euclid(1.0);
        } else {
            if l || r {
                st.hsv[1] = (st.hsv[1] + if r { 0.01 } else { -0.01 }).clamp(0.0, 1.0);
            }
            if u || d {
                st.hsv[2] = (st.hsv[2] + if u { 0.01 } else { -0.01 }).clamp(0.0, 1.0);
            }
        }
    }
    if st.drag != 0 || resp.has_focus() {
        let new = hsv_to_rgb(st.hsv[0], st.hsv[1], st.hsv[2]);
        if new != *color {
            *color = new;
        }
    }
    // knobs
    let painter = ui.painter();
    let a = st.hsv[0] * TAU - PI / 2.0;
    let hk = c + vec2(a.cos(), a.sin()) * (ro + ri) / 2.0;
    painter.circle_filled(hk, 7.0, Color32::WHITE);
    painter.circle_stroke(hk, 7.5, Stroke::new(1.5, Color32::from_black_alpha(102)));
    painter.circle_filled(hk, 4.5, c32(hsv_to_rgb(st.hsv[0], 1.0, 1.0)));
    let sk = sq_rect.min + vec2(st.hsv[1] * sq, (1.0 - st.hsv[2]) * sq);
    painter.circle_stroke(sk, 6.0, Stroke::new(2.0, Color32::WHITE));
    painter.circle_stroke(sk, 7.5, Stroke::new(1.5, Color32::from_black_alpha(102)));

    ui.add_space(8.0);
    // ---- fields ----
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let (r, _) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::hover());
        ui.painter().rect_filled(r, R_SM, c32(*color));
        ui.painter().rect_stroke(r, R_SM, Stroke::new(1.0, p.border), StrokeKind::Inside);
        let te = egui::TextEdit::singleline(&mut st.hex)
            .font(font(Ts::Mono))
            .char_limit(7)
            .desired_width(84.0)
            .margin(egui::Margin::symmetric(8, 6));
        let tr = ui.add(te);
        if st.hex_err {
            ui.painter().rect_stroke(tr.rect, 8, Stroke::new(1.5, p.danger), StrokeKind::Outside);
        }
        if tr.changed() {
            st.hex_err = false;
        }
        let esc = ui.input(|i| i.key_pressed(Key::Escape));
        if tr.lost_focus() {
            if esc {
                st.hex = to_hex(*color);
                st.hex_err = false;
            } else {
                match parse_hex(&st.hex) {
                    Some(c) => {
                        *color = c;
                        st.hsv = rgb_to_hsv(c);
                        st.hex = to_hex(c);
                    }
                    None => st.hex_err = true,
                }
            }
        }
        segmented_sized(
            ui,
            id.with("fields"),
            &mut st.fields,
            &[(Fields::Rgb, "RGB"), (Fields::Hsv, "HSV")],
            None,
            28.0,
        );
    });
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        match st.fields {
            Fields::Rgb => {
                let mut ch = [color[0] as f32, color[1] as f32, color[2] as f32];
                let mut edited = false;
                for (k, name) in ["R", "G", "B"].iter().enumerate() {
                    text::label(ui, *name, Ts::Caption, p.text2);
                    edited |= controls::number_field(ui, &mut ch[k], 0.0..=255.0, 1.0, 0, "", 52.0).changed();
                }
                if edited {
                    *color = [ch[0] as u8, ch[1] as u8, ch[2] as u8];
                    st.hsv = rgb_to_hsv(*color);
                }
            }
            Fields::Hsv => {
                let mut v = [st.hsv[0] * 360.0, st.hsv[1] * 100.0, st.hsv[2] * 100.0];
                let mut edited = false;
                for (k, (name, max)) in [("H", 360.0), ("S", 100.0), ("V", 100.0)].iter().enumerate() {
                    text::label(ui, *name, Ts::Caption, p.text2);
                    edited |= controls::number_field(ui, &mut v[k], 0.0..=*max, 1.0, 0, "", 52.0).changed();
                }
                if edited {
                    st.hsv = [(v[0] / 360.0).rem_euclid(1.0), v[1] / 100.0, v[2] / 100.0];
                    *color = hsv_to_rgb(st.hsv[0], st.hsv[1], st.hsv[2]);
                }
            }
        }
    });
    ui.add_space(4.0);
    // ---- presets & recent ----
    text::overline(ui, "Presets");
    swatch_row(ui, id.with("pre"), &PRESETS, Some(&PRESET_NAMES), color, &mut st);
    if !recent.is_empty() {
        text::overline(ui, "Recent");
        let rec = recent.to_vec();
        swatch_row(ui, id.with("rec"), &rec, None, color, &mut st);
    }
    ui.add_space(2.0);
    text::caption3(ui, "LEDs show saturated colors brighter than your screen. Pastels may look white.");

    if *color != before {
        st.hex = to_hex(*color);
        st.last_rgb = Some(*color);
    }
    ui.data_mut(|d| d.insert_temp(id, st));
    *color != before
}

fn swatch_row(
    ui: &mut Ui,
    id: Id,
    colors: &[[u8; 3]],
    names: Option<&[&str; 12]>,
    color: &mut [u8; 3],
    st: &mut PickerState,
) {
    let p = pal(ui);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(2.0, 2.0);
        for (k, c) in colors.iter().enumerate() {
            let (r, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
            let resp: Response = ui.interact(r, id.with(k), Sense::click());
            ui.painter().rect_filled(r.shrink(1.0), 4, c32(*c));
            if *c == *color {
                ui.painter().rect_stroke(r, 5, Stroke::new(2.0, p.accent), StrokeKind::Inside);
            } else if resp.hovered() {
                ui.painter().rect_stroke(r, 5, Stroke::new(1.0, p.border_strong), StrokeKind::Inside);
            }
            let resp = match names {
                Some(n) => resp.on_hover_text(format!("{} · {}", n[k], to_hex(*c))),
                None => resp.on_hover_text(to_hex(*c)),
            };
            if resp.clicked() {
                *color = *c;
                st.hsv = rgb_to_hsv(*c);
            }
        }
    });
}

/// Row of swatches for an effect's colors with +/− for palette effects.
pub fn palette_row(
    ui: &mut Ui,
    id: Id,
    colors: &mut Vec<[u8; 3]>,
    count: usize,
    editable_len: bool,
    recent: &mut Vec<[u8; 3]>,
    names: &[&str],
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for k in 0..count.min(colors.len()) {
            let tip = names.get(k).copied().unwrap_or("");
            changed |= swatch_button(ui, id.with(k), &mut colors[k], recent, tip);
        }
        if editable_len {
            if colors.len() < 6
                && controls::icon_button_sized(ui, crate::ui::icons::Icon::Plus, "Add color", 28.0, None).clicked()
            {
                let last = colors.last().copied().unwrap_or([255, 255, 255]);
                let h = rgb_to_hsv(last);
                colors.push(hsv_to_rgb(h[0] + 0.15, h[1].max(0.8), h[2].max(0.8)));
                changed = true;
            }
            if colors.len() > 2
                && controls::icon_button_sized(ui, crate::ui::icons::Icon::Minus, "Remove last color", 28.0, None)
                    .clicked()
            {
                colors.pop();
                changed = true;
            }
        }
    });
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parse_and_format() {
        assert_eq!(parse_hex("#7C83FF"), Some([0x7C, 0x83, 0xFF]));
        assert_eq!(parse_hex("7c83ff"), Some([0x7C, 0x83, 0xFF]));
        assert_eq!(parse_hex("#fff"), Some([255, 255, 255]));
        assert_eq!(parse_hex(" f00 "), Some([255, 0, 0]));
        assert_eq!(parse_hex("#12345"), None);
        assert_eq!(parse_hex("#GGHHII"), None);
        assert_eq!(to_hex([0x7C, 0x83, 0xFF]), "#7C83FF");
    }

    #[test]
    fn hsv_roundtrip() {
        for c in PRESETS.iter().chain([[0, 0, 0], [128, 128, 128], [12, 200, 77], [255, 0, 1]].iter()) {
            let h = rgb_to_hsv(*c);
            let back = hsv_to_rgb(h[0], h[1], h[2]);
            for k in 0..3 {
                assert!((back[k] as i32 - c[k] as i32).abs() <= 1, "{c:?} -> {h:?} -> {back:?}");
            }
        }
        let red = rgb_to_hsv([255, 0, 0]);
        assert!(red[0].abs() < 1e-6 && (red[1] - 1.0).abs() < 1e-6 && (red[2] - 1.0).abs() < 1e-6);
        let blue = rgb_to_hsv([0, 0, 255]);
        assert!((blue[0] - 2.0 / 3.0).abs() < 1e-4);
        assert_eq!(rgb_to_hsv([0, 0, 0]), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn recents_dedup_and_cap() {
        let mut r = Vec::new();
        for i in 0..10u8 {
            push_recent(&mut r, [i, 0, 0]);
        }
        assert_eq!(r.len(), 8);
        assert_eq!(r[0], [9, 0, 0]);
        push_recent(&mut r, [5, 0, 0]);
        assert_eq!(r[0], [5, 0, 0]);
        assert_eq!(r.iter().filter(|c| **c == [5, 0, 0]).count(), 1);
    }

    #[test]
    fn presets_are_bright() {
        for c in PRESETS {
            assert!(rgb_to_hsv(c)[2] > 0.9, "{c:?} is too dark for LEDs");
        }
    }
}
