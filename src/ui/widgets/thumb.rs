//! Component thumbnails: a small render of the part, optionally lit with the zone's
//! current colour through its light mask (additive, like the hero).

use egui::{Color32, Context, Painter, Rect, pos2};

use crate::config::HeaderRole;
use crate::model::{FanRole, FanState};
use crate::ui::assets::{Assets, Thumb};
use crate::ui::theme::{Palette, R_MD};

const UV: Rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));

/// What the "What's connected" choice (or the effective role) shows.
pub fn thumb_for_role(assigned: Option<HeaderRole>, role: FanRole) -> Thumb {
    match assigned {
        Some(HeaderRole::Pump) => Thumb::Pump,
        Some(HeaderRole::RadiatorFans) => Thumb::Fan120,
        Some(HeaderRole::CpuFans) => Thumb::Fan120,
        Some(HeaderRole::CaseFans) => Thumb::Fan140,
        Some(HeaderRole::Unknown) | Some(HeaderRole::Unused) | None => match role {
            FanRole::Pump => Thumb::Pump,
            _ => Thumb::Fan120,
        },
    }
}

pub fn thumb_for(f: &FanState) -> Thumb {
    thumb_for_role(f.assigned, f.role)
}

/// Mean colour of a run of LEDs (black when empty).
pub fn avg(c: &[[u8; 3]]) -> Color32 {
    if c.is_empty() {
        return Color32::BLACK;
    }
    let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
    for x in c {
        r += x[0] as u32;
        g += x[1] as u32;
        b += x[2] as u32;
    }
    let n = c.len() as u32;
    Color32::from_rgb((r / n) as u8, (g / n) as u8, (b / n) as u8)
}

/// Scales a light colour by `k` (LED gain), staying opaque so `.additive()` adds it.
pub fn gain(c: Color32, k: f32) -> Color32 {
    let s = |v: u8| (v as f32 * k).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(s(c.r()), s(c.g()), s(c.b()))
}

/// Paints thumbnail `t` into `rect`; `light` lights its LED mask (unlit parts stay dark, like the
/// real hardware with lighting off). Unconfirmed parts are drawn dimmer (`dim`).
#[allow(clippy::too_many_arguments)]
pub fn paint(
    ctx: &Context,
    painter: &Painter,
    assets: &mut Assets,
    p: &Palette,
    rect: Rect,
    t: Thumb,
    light: Option<Color32>,
    dim: bool,
) {
    match assets.thumb(ctx, t) {
        Some((base, led)) => {
            let tint = if dim { Color32::from_white_alpha(150) } else { Color32::WHITE };
            painter.image(base, rect, UV, tint);
            if let (Some(mask), Some(c)) = (led, light) {
                painter.image(mask, rect, UV, gain(c, p.led_gain).additive());
            }
        }
        None => {
            painter.rect_filled(rect.shrink(2.0), R_MD, p.surface2);
            crate::ui::icons::paint_fan(painter, rect.shrink(rect.width() * 0.22), 0.0, p.text3);
        }
    }
}
