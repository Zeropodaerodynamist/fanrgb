//! Design tokens ("Quiet Instrument"): palette (dark + light), thermal ramp, data-series colors,
//! spacing, radii, elevation, and the egui style built from them.
//!
//! Every color is opaque; translucency only comes from `with_alpha` at paint time (premultiplied
//! correctly) and from the modal scrim.

use egui::epaint::Shadow;
use egui::{
    Color32, Context, CornerRadius, CursorIcon, Stroke, Theme, ThemePreference, Ui, Visuals, style::HandleShape,
};

// ---------------------------------------------------------------------------------------
// Spacing / sizing (logical points)

#[allow(dead_code)] // complete 4-pt grid
pub const S1: f32 = 4.0;
pub const S2: f32 = 8.0;
pub const S3: f32 = 12.0;
#[allow(dead_code)] // complete 4-pt grid
pub const S4: f32 = 16.0;
#[allow(dead_code)] // complete 4-pt grid
pub const S5: f32 = 20.0;
#[allow(dead_code)] // complete 4-pt grid
pub const S6: f32 = 24.0;
#[allow(dead_code)] // complete 4-pt grid
pub const S8: f32 = 32.0;

/// Fan-mode tile height on Overview; Cooling uses [`TILE_H_COMPACT`].
pub const TILE_H: f32 = 74.0;
pub const TILE_H_COMPACT: f32 = 64.0;
/// Rig render drawn width (aspect 440:480 of the asset).
pub const HERO_W: f32 = 404.0;

pub const TOPBAR_H: f32 = 52.0;

// Corner radii.
pub const R_XS: u8 = 4;
pub const R_SM: u8 = 6;
pub const R_MD: u8 = 8;
pub const R_LG: u8 = 12;
pub const R_XL: u8 = 16;
pub const R_PILL: u8 = 255;

pub fn r(v: u8) -> CornerRadius {
    CornerRadius::same(v)
}

// ---------------------------------------------------------------------------------------
// Palette

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub dark: bool,
    pub canvas: Color32,
    pub sidebar: Color32,
    pub surface1: Color32,
    pub surface2: Color32,
    pub surface3: Color32,
    pub inset: Color32,
    pub border_subtle: Color32,
    pub border: Color32,
    pub border_strong: Color32,
    pub text1: Color32,
    pub text2: Color32,
    pub text3: Color32,
    pub text_disabled: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub accent_press: Color32,
    pub accent_subtle: Color32,
    pub on_accent: Color32,
    pub focus: Color32,
    pub success: Color32,
    pub success_subtle: Color32,
    pub warning: Color32,
    pub warning_subtle: Color32,
    pub danger: Color32,
    pub danger_subtle: Color32,
    pub info: Color32,
    pub info_subtle: Color32,
    /// Modal backdrop (premultiplied, the only translucent token).
    pub scrim: Color32,
    /// Thermal ramp stops: cool, normal, warm, hot, critical.
    pub thermal: [Color32; 5],
    /// Fixed semantic data series: CPU, GPU, VRM, chipset, system, other.
    pub series: [Color32; 6],
    /// Fan series.
    pub fans: [Color32; 6],
    /// Hero stage gradient (top → bottom).
    pub stage_top: Color32,
    pub stage_bottom: Color32,
    /// Multiplier for additive LED light on the hero and thumbnails.
    pub led_gain: f32,
    /// Calibration channel colours (R, G, B sliders and transfer curves).
    pub channels: [Color32; 3],
}

const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

pub const DARK: Palette = Palette {
    dark: true,
    canvas: hex(0x0F1013),
    sidebar: hex(0x0B0C0E),
    surface1: hex(0x16181D),
    surface2: hex(0x1C1F25),
    surface3: hex(0x252830),
    inset: hex(0x0B0C0F),
    border_subtle: hex(0x23262E),
    border: hex(0x2F333C),
    border_strong: hex(0x3D424D),
    text1: hex(0xECEDF0),
    text2: hex(0xA1A6B2),
    // Spec value #767C88 measured 4.2:1 on surface1; lifted to pass 4.5:1 (see tests).
    text3: hex(0x878D98),
    text_disabled: hex(0x4B505A),
    accent: hex(0x7C83FF),
    accent_hover: hex(0x9399FF),
    accent_press: hex(0x666DF0),
    accent_subtle: hex(0x20223A),
    on_accent: hex(0x0C0D14),
    focus: hex(0x9399FF),
    success: hex(0x3FCF8E),
    success_subtle: hex(0x14261E),
    warning: hex(0xF2B13A),
    warning_subtle: hex(0x2A2214),
    danger: hex(0xFF5C6A),
    danger_subtle: hex(0x2C1619),
    info: hex(0x5AB0FF),
    info_subtle: hex(0x132233),
    scrim: Color32::from_rgba_premultiplied(0, 0, 0, 140),
    thermal: [hex(0x4CC3FF), hex(0x3FCF8E), hex(0xF2C94C), hex(0xFF9F43), hex(0xFF5C6A)],
    series: [hex(0x7C83FF), hex(0x3FC7C1), hex(0xF2B13A), hex(0xE879B9), hex(0x8BD450), hex(0x5AB0FF)],
    fans: [hex(0x7C83FF), hex(0x3FC7C1), hex(0xF2B13A), hex(0xE879B9), hex(0x8BD450), hex(0xC58BFF)],
    stage_top: hex(0x1A1D24),
    stage_bottom: hex(0x101115),
    led_gain: 1.0,
    channels: [hex(0xFF5C6A), hex(0x3FCF8E), hex(0x5AB0FF)],
};

pub const LIGHT: Palette = Palette {
    dark: false,
    canvas: hex(0xF5F6F8),
    sidebar: hex(0xEDEEF2),
    surface1: hex(0xFFFFFF),
    surface2: hex(0xF3F4F7),
    surface3: hex(0xE8EAEF),
    inset: hex(0xEFF0F3),
    border_subtle: hex(0xE6E8EC),
    border: hex(0xD7DAE0),
    border_strong: hex(0xBEC3CC),
    text1: hex(0x15171C),
    text2: hex(0x4F5563),
    // Spec value #7C828E measured 3.9:1 on white; darkened to pass 4.5:1 on all surfaces.
    text3: hex(0x646A77),
    text_disabled: hex(0xB4B8C0),
    accent: hex(0x4F57E6),
    accent_hover: hex(0x434BD6),
    accent_press: hex(0x3A41C2),
    accent_subtle: hex(0xECEDFD),
    on_accent: hex(0xFFFFFF),
    focus: hex(0x4F57E6),
    success: hex(0x137A4E),
    success_subtle: hex(0xE4F5EC),
    warning: hex(0x8F5B0A),
    warning_subtle: hex(0xFCF2E1),
    danger: hex(0xC92F3F),
    danger_subtle: hex(0xFDEBED),
    info: hex(0x2468BB),
    info_subtle: hex(0xE7F0FB),
    scrim: Color32::from_rgba_premultiplied(5, 6, 7, 89),
    thermal: [hex(0x1E8FD0), hex(0x1A9461), hex(0xB98A07), hex(0xD06A12), hex(0xD23646)],
    series: [hex(0x4F57E6), hex(0x16918C), hex(0xB27A10), hex(0xC2468F), hex(0x58A11F), hex(0x2B77D0)],
    fans: [hex(0x4F57E6), hex(0x16918C), hex(0xB27A10), hex(0xC2468F), hex(0x58A11F), hex(0x8C4FD6)],
    stage_top: hex(0xFFFFFF),
    stage_bottom: hex(0xECEEF2),
    led_gain: 0.9,
    channels: [hex(0xC92F3F), hex(0x137A4E), hex(0x2468BB)],
};

pub fn pal(ui: &Ui) -> &'static Palette {
    if ui.visuals().dark_mode { &DARK } else { &LIGHT }
}

pub fn pal_ctx(ctx: &Context) -> &'static Palette {
    if ctx.style().visuals.dark_mode { &DARK } else { &LIGHT }
}

/// `c` with straight alpha `a` (0..1), correctly premultiplied.
pub fn with_alpha(c: Color32, a: f32) -> Color32 {
    let a = a.clamp(0.0, 1.0);
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (a * 255.0).round() as u8)
}

/// Opaque mix of two colors in sRGB space (for hover fills etc.).
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

// ---------------------------------------------------------------------------------------
// Oklab (closed form, no dependency)

fn srgb_to_lin(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn lin_to_srgb(c: f32) -> u8 {
    let c = c.clamp(0.0, 1.0);
    let s = if c <= 0.003_130_8 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 };
    (s * 255.0).round().clamp(0.0, 255.0) as u8
}

pub fn to_oklab(c: Color32) -> [f32; 3] {
    let (r, g, b) = (srgb_to_lin(c.r()), srgb_to_lin(c.g()), srgb_to_lin(c.b()));
    let l = (0.412_221_46 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

pub fn from_oklab(lab: [f32; 3]) -> Color32 {
    let l_ = lab[0] + 0.396_337_78 * lab[1] + 0.215_803_76 * lab[2];
    let m_ = lab[0] - 0.105_561_346 * lab[1] - 0.063_854_17 * lab[2];
    let s_ = lab[0] - 0.089_484_18 * lab[1] - 1.291_485_5 * lab[2];
    let (l, m, s) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    Color32::from_rgb(
        lin_to_srgb(4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s),
        lin_to_srgb(-1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s),
        lin_to_srgb(-0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s),
    )
}

pub fn mix_oklab(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let (x, y) = (to_oklab(a), to_oklab(b));
    from_oklab([x[0] + (y[0] - x[0]) * t, x[1] + (y[1] - x[1]) * t, x[2] + (y[2] - x[2]) * t])
}

/// Thermal stop temperatures for a critical temperature `c`.
pub fn thermal_stops(c: f32) -> [f32; 5] {
    let c = c.max(60.0);
    [35.0, 50.0, (c - 22.0).max(51.0), (c - 10.0).max(52.0), (c - 3.0).max(53.0)]
}

/// Color of temperature `t` (°C) on the thermal ramp relative to the critical temperature.
pub fn thermal(p: &Palette, t: f32, critical: f32) -> Color32 {
    thermal_with(&p.thermal, &thermal_stops(critical), t)
}

pub fn thermal_with(colors: &[Color32; 5], stops: &[f32; 5], t: f32) -> Color32 {
    if !t.is_finite() || t <= stops[0] {
        return colors[0];
    }
    for i in 0..4 {
        if t <= stops[i + 1] {
            let f = (t - stops[i]) / (stops[i + 1] - stops[i]).max(0.01);
            return mix_oklab(colors[i], colors[i + 1], f);
        }
    }
    colors[4]
}

/// Semantic series for a sensor id.
pub fn series_for(p: &Palette, id: &str) -> Color32 {
    let i = if id.starts_with("cpu") {
        0
    } else if id.starts_with("gpu") {
        1
    } else if id == "mb.t4" || id == "mb.t5" {
        2
    } else if id == "mb.t1" {
        3
    } else if id == "mb.t0" {
        4
    } else {
        5
    };
    p.series[i]
}

pub fn fan_color(p: &Palette, index: usize) -> Color32 {
    p.fans[index % p.fans.len()]
}

// ---------------------------------------------------------------------------------------
// Elevation

pub fn shadow_e1(p: &Palette) -> Shadow {
    Shadow { offset: [0, 4], blur: 16, spread: 0, color: Color32::from_black_alpha(if p.dark { 115 } else { 30 }) }
}

pub fn shadow_e2(p: &Palette) -> Shadow {
    Shadow { offset: [0, 16], blur: 40, spread: 0, color: Color32::from_black_alpha(if p.dark { 150 } else { 46 }) }
}

pub fn shadow_e3(p: &Palette) -> Shadow {
    Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(if p.dark { 128 } else { 40 }) }
}

// ---------------------------------------------------------------------------------------
// Style

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum ThemePref {
    #[default]
    System,
    Dark,
    Light,
}

fn visuals(p: &Palette) -> Visuals {
    let mut v = if p.dark { Visuals::dark() } else { Visuals::light() };
    v.dark_mode = p.dark;
    v.override_text_color = None;
    v.weak_text_color = Some(p.text2);
    v.panel_fill = p.canvas;
    v.window_fill = p.surface1;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = r(R_XL);
    v.window_shadow = shadow_e2(p);
    v.popup_shadow = shadow_e1(p);
    v.menu_corner_radius = r(R_LG);
    v.extreme_bg_color = p.inset;
    v.text_edit_bg_color = Some(p.surface2);
    v.faint_bg_color = p.surface2;
    v.code_bg_color = p.surface3;
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warning;
    v.error_fg_color = p.danger;
    v.selection.bg_fill = if p.dark { hex(0x3A3F86) } else { hex(0xC9CCF8) };
    v.selection.stroke = Stroke::new(1.5, p.accent);
    v.text_cursor.stroke = Stroke::new(2.0, p.accent);
    v.slider_trailing_fill = true;
    v.handle_shape = HandleShape::Circle;
    v.interact_cursor = Some(CursorIcon::PointingHand);
    v.striped = false;
    v.indent_has_left_vline = false;
    v.collapsing_header_frame = false;
    v.disabled_alpha = 0.4;

    let hover = if p.dark { hex(0x22252C) } else { hex(0xEBEDF1) };
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.surface1;
    w.noninteractive.weak_bg_fill = p.surface1;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border_subtle);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text1);
    w.noninteractive.corner_radius = r(R_MD);
    w.noninteractive.expansion = 0.0;

    w.inactive.bg_fill = p.surface3;
    w.inactive.weak_bg_fill = p.surface2;
    w.inactive.bg_stroke = Stroke::new(1.0, p.border);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text1);
    w.inactive.corner_radius = r(R_MD);
    w.inactive.expansion = 0.0;

    w.hovered.bg_fill = p.surface3;
    w.hovered.weak_bg_fill = hover;
    w.hovered.bg_stroke = Stroke::new(1.0, p.border_strong);
    w.hovered.fg_stroke = Stroke::new(1.5, p.text1);
    w.hovered.corner_radius = r(R_MD);
    w.hovered.expansion = 0.0;

    w.active.bg_fill = p.surface3;
    w.active.weak_bg_fill = p.surface3;
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(1.5, p.text1);
    w.active.corner_radius = r(R_MD);
    w.active.expansion = 0.0;

    w.open = w.active;
    w.open.bg_stroke = Stroke::new(1.0, p.border_strong);
    v
}

fn style_for(p: &Palette) -> egui::Style {
    let mut s = egui::Style { visuals: visuals(p), ..Default::default() };
    super::fonts::apply_text_styles(&mut s);
    s.spacing.item_spacing = egui::vec2(S2, S2);
    s.spacing.button_padding = egui::vec2(S3, 6.0);
    s.spacing.interact_size = egui::vec2(40.0, 28.0);
    s.spacing.window_margin = egui::Margin::same(24);
    s.spacing.menu_margin = egui::Margin::same(6);
    s.spacing.slider_rail_height = 4.0;
    s.spacing.combo_height = 320.0;
    s.spacing.tooltip_width = 280.0;
    s.spacing.icon_width = 16.0;
    s.spacing.icon_width_inner = 10.0;
    s.spacing.scroll = egui::style::ScrollStyle::thin();
    s.interaction.tooltip_delay = 0.35;
    s.interaction.selectable_labels = false;
    s.animation_time = 0.16;
    s.explanation_tooltips = false;
    s.url_in_tooltip = false;
    s
}

/// Installs both themes and selects `pref`.
pub fn apply(ctx: &Context, pref: ThemePref) {
    ctx.set_style_of(Theme::Dark, style_for(&DARK));
    ctx.set_style_of(Theme::Light, style_for(&LIGHT));
    ctx.set_theme(match pref {
        ThemePref::System => ThemePreference::System,
        ThemePref::Dark => ThemePreference::Dark,
        ThemePref::Light => ThemePreference::Light,
    });
}

/// Disables egui's own widget animations when motion is reduced.
pub fn set_animation_time(ctx: &Context, reduce: bool) {
    let t = if reduce { 0.0 } else { 0.16 };
    ctx.all_styles_mut(|s| s.animation_time = t);
}

// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn lum(c: Color32) -> f32 {
        let f = |v: u8| srgb_to_lin(v);
        0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
    }

    pub fn contrast(a: Color32, b: Color32) -> f32 {
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn text_tokens_meet_wcag_aa() {
        for p in [&DARK, &LIGHT] {
            for bg in [p.canvas, p.sidebar, p.surface1, p.surface2, p.inset] {
                for (name, fg) in [("text1", p.text1), ("text2", p.text2), ("text3", p.text3)] {
                    let c = contrast(fg, bg);
                    assert!(c >= 4.5, "{name} {fg:?} on {bg:?}: {c:.2} (dark={})", p.dark);
                }
            }
            for (name, fg) in [
                ("accent", p.accent),
                ("danger", p.danger),
                ("warning", p.warning),
                ("success", p.success),
                ("info", p.info),
            ] {
                let c = contrast(fg, p.surface1);
                assert!(c >= 4.5, "{name} on surface1: {c:.2} (dark={})", p.dark);
            }
            // Status text on its own subtle fill (banners, chips).
            for (fg, bg) in [
                (p.danger, p.danger_subtle),
                (p.warning, p.warning_subtle),
                (p.success, p.success_subtle),
                (p.info, p.info_subtle),
            ] {
                assert!(contrast(fg, bg) >= 4.5, "{fg:?} on {bg:?}: {:.2}", contrast(fg, bg));
            }
            assert!(contrast(p.on_accent, p.accent) >= 4.5, "on_accent (dark={})", p.dark);
            assert!(contrast(p.text1, p.accent_subtle) >= 4.5);
        }
    }

    /// Theme additions: tiles, hero stage, calibration channels, neutral chips.
    #[test]
    fn redesign_pairs_meet_wcag() {
        for p in [&DARK, &LIGHT] {
            // Selected tile: name on accent_subtle, the accent icon / border as a 3:1 graphic.
            assert!(contrast(p.text1, p.accent_subtle) >= 4.5);
            assert!(contrast(p.accent, p.accent_subtle) >= 3.0, "accent icon on selected tile (dark={})", p.dark);
            // Hero stage: callout pills sit on surface1, the stage itself carries text2 / text3 labels.
            for bg in [p.stage_top, p.stage_bottom] {
                assert!(contrast(p.text2, bg) >= 4.5, "text2 on stage (dark={})", p.dark);
                assert!(contrast(p.text3, bg) >= 4.5, "text3 on stage (dark={})", p.dark);
            }
            // Neutral chips (e.g. "No Windows Update offer", "Running · no speed signal").
            assert!(contrast(p.text2, p.surface3) >= 4.5, "text2 on surface3 (dark={})", p.dark);
            // Calibration channel colours are slider fills and chart lines: 3:1 graphics.
            for c in p.channels {
                assert!(contrast(c, p.surface1) >= 3.0, "{c:?} on surface1 (dark={})", p.dark);
                assert!(contrast(c, p.inset) >= 3.0, "{c:?} on inset (dark={})", p.dark);
            }
        }
    }

    #[test]
    fn oklab_roundtrip() {
        for c in [hex(0x7C83FF), hex(0x000000), hex(0xFFFFFF), hex(0xF2B13A), hex(0x123456)] {
            let back = from_oklab(to_oklab(c));
            for (a, b) in [(c.r(), back.r()), (c.g(), back.g()), (c.b(), back.b())] {
                assert!((a as i32 - b as i32).abs() <= 1, "{c:?} -> {back:?}");
            }
        }
    }

    #[test]
    fn thermal_ramp_is_monotonic_and_clamped() {
        let p = &DARK;
        assert_eq!(thermal(p, 10.0, 92.0), p.thermal[0]);
        assert_eq!(thermal(p, 120.0, 92.0), p.thermal[4]);
        assert_eq!(thermal(p, 50.0, 92.0), p.thermal[1]);
        let s = thermal_stops(92.0);
        assert!(s.windows(2).all(|w| w[0] < w[1]));
        let s = thermal_stops(60.0);
        assert!(s.windows(2).all(|w| w[0] < w[1]), "{s:?}");
    }
}
