//! Effect gallery: tiles animated by the real effect renderer (`rgb::effects::render`), so every
//! preview is exactly what the hardware will show, in the user's own colors.

use std::collections::HashMap;

use egui::{EventFilter, Id, Key, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::config::{Effect, EffectKind};
use crate::rgb::color::Rgb;
use crate::rgb::effects::{AudioLevels, EffectState, FrameCtx, render};
use crate::ui::anim;
use crate::ui::fonts::{Ts, font};
use crate::ui::icons::{self, Icon};
use crate::ui::layout;
use crate::ui::theme::{R_LG, R_SM, pal};

pub const GROUPS: [(&str, &[EffectKind]); 4] = [
    ("Ambient", &[EffectKind::Static, EffectKind::Breathing, EffectKind::Spectrum, EffectKind::Gradient]),
    (
        "Motion",
        &[
            EffectKind::Rainbow,
            EffectKind::Comet,
            EffectKind::Chase,
            EffectKind::Twinkle,
            EffectKind::Fire,
            EffectKind::Strobe,
        ],
    ),
    ("Reactive", &[EffectKind::Temperature, EffectKind::Load, EffectKind::Music]),
    (
        "Onboard · runs without FanRGB",
        &[
            EffectKind::HwStatic,
            EffectKind::HwBreathing,
            EffectKind::HwFlash,
            EffectKind::HwDoubleFlash,
            EffectKind::HwColorCycle,
            EffectKind::HwWave,
            EffectKind::HwRandom,
        ],
    ),
];

pub fn short_label(k: EffectKind) -> &'static str {
    match k {
        EffectKind::Off => "Off",
        EffectKind::Static | EffectKind::HwStatic => "Static",
        EffectKind::Breathing | EffectKind::HwBreathing => "Breathing",
        EffectKind::Spectrum => "Spectrum",
        EffectKind::Rainbow => "Rainbow wave",
        EffectKind::Gradient => "Gradient",
        EffectKind::Comet => "Comet",
        EffectKind::Chase => "Theater chase",
        EffectKind::Twinkle => "Twinkle",
        EffectKind::Fire => "Fire",
        EffectKind::Temperature => "Temperature",
        EffectKind::Load => "Load",
        EffectKind::Music => "Music",
        EffectKind::Strobe => "Strobe",
        EffectKind::HwFlash => "Flash",
        EffectKind::HwDoubleFlash => "Double flash",
        EffectKind::HwColorCycle => "Color cycle",
        EffectKind::HwWave => "Wave",
        EffectKind::HwRandom => "Random",
    }
}

const LEDS: usize = 20;

#[derive(Default)]
pub struct GalleryState {
    tiles: Vec<(EffectKind, EffectState, Vec<Rgb>)>,
    last_time: Option<f64>,
    synth_t: f32,
    focus: Option<usize>,
}

impl GalleryState {
    fn tile(&mut self, k: EffectKind) -> &mut (EffectKind, EffectState, Vec<Rgb>) {
        if let Some(i) = self.tiles.iter().position(|t| t.0 == k) {
            return &mut self.tiles[i];
        }
        self.tiles.push((k, EffectState::default(), vec![Rgb::BLACK; LEDS]));
        self.tiles.last_mut().unwrap()
    }
}

/// Synthetic music levels for the Music tile.
fn synth_audio(t: f32) -> AudioLevels {
    let mut bands = [0.0f32; 16];
    for (i, b) in bands.iter_mut().enumerate() {
        *b = (0.35 + 0.35 * (t * 3.1 + i as f32 * 0.7).sin() * (t * 1.3 + i as f32 * 0.21).cos()).clamp(0.0, 1.0)
            * (1.0 - i as f32 / 22.0);
    }
    let beat = ((t * 2.0).fract() * -6.0).exp();
    AudioLevels { level: 0.5 + 0.3 * (t * 1.7).sin(), bands, beat }
}

/// Shows the gallery. `animate`: render new frames (false = keep the last frame, e.g. hidden or
/// reduce motion and not hovered). Returns the effect kind the user picked.
#[allow(clippy::too_many_arguments)]
pub fn gallery(
    ui: &mut Ui,
    id: Id,
    current: EffectKind,
    base: &Effect,
    sensors: &HashMap<String, f32>,
    allow_wave: bool,
    st: &mut GalleryState,
    animate: bool,
) -> Option<EffectKind> {
    let p = pal(ui);
    let now = ui.input(|i| i.time);
    let dt = st.last_time.map(|t| (now - t) as f32).unwrap_or(0.033).clamp(0.0, 0.1);
    st.last_time = Some(now);
    st.synth_t += dt;
    let audio = synth_audio(st.synth_t);
    let reduce = anim::reduce_motion(ui.ctx());
    let mut picked = None;
    let all: Vec<EffectKind> = GROUPS.iter().flat_map(|g| g.1.iter().copied()).collect();
    let mut index = 0usize;
    let gap = 10.0;
    let width = ui.available_width();
    let cols = layout::grid_columns(width, 136.0, gap, 2);
    let mut any_focus = false;
    // A tile on screen is moving: only then keep the frame loop going.
    let mut ran = false;
    for (group, kinds) in GROUPS {
        ui.add_space(4.0);
        crate::ui::widgets::text::overline(ui, group);
        ui.add_space(2.0);
        let group_start = index;
        layout::tile_grid(ui, kinds.len(), 136.0, 96.0, gap, |i, ui, rect| {
            let kind = kinds[i];
            let gi = group_start + i;
            let disabled = kind == EffectKind::HwWave && !allow_wave;
            let tid = id.with(("tile", gi));
            let resp = ui.interact(rect, tid, if disabled { Sense::hover() } else { Sense::click() });
            let visible = ui.is_rect_visible(rect);
            let selected = kind == current;
            // keyboard grid navigation
            if resp.has_focus() {
                any_focus = true;
                st.focus = Some(gi);
                ui.memory_mut(|m| {
                    m.set_focus_lock_filter(
                        tid,
                        EventFilter { horizontal_arrows: true, vertical_arrows: true, ..Default::default() },
                    )
                });
                let target = ui.input(|inp| {
                    if inp.key_pressed(Key::ArrowRight) {
                        Some(gi + 1)
                    } else if inp.key_pressed(Key::ArrowLeft) {
                        gi.checked_sub(1)
                    } else if inp.key_pressed(Key::ArrowDown) {
                        Some(gi + cols)
                    } else if inp.key_pressed(Key::ArrowUp) {
                        gi.checked_sub(cols)
                    } else {
                        None
                    }
                });
                if let Some(t) = target.filter(|t| *t < all.len()) {
                    ui.memory_mut(|m| m.request_focus(id.with(("tile", t))));
                }
            }
            if resp.clicked() && !disabled {
                picked = Some(kind);
            }
            if !visible {
                return;
            }
            let hovered = resp.hovered();
            let painter = ui.painter();
            let fill = if hovered && !disabled { p.surface2 } else { p.surface1 };
            painter.rect_filled(rect, R_LG, fill);
            painter.rect_stroke(
                rect,
                R_LG,
                if selected { Stroke::new(2.0, p.accent) } else { Stroke::new(1.0, p.border_subtle) },
                StrokeKind::Inside,
            );
            let preview = Rect::from_min_size(rect.min + vec2(8.0, 8.0), vec2(rect.width() - 16.0, 50.0));
            painter.rect_filled(preview, R_SM, p.inset);
            // render with the real renderer
            let run = animate && (!reduce || hovered);
            ran |= run && kind.is_animated();
            let tile = st.tile(kind);
            if run || tile.2.iter().all(|c| *c == Rgb::BLACK) {
                let mut e = base.clone();
                e.kind = kind;
                e.brightness = 1.0;
                if e.colors.is_empty() {
                    e.colors = vec![[0, 200, 255], [170, 0, 255], [255, 40, 0]];
                }
                let frame_dt = if tile.2.iter().all(|c| *c == Rgb::BLACK) { 0.4 } else { dt };
                let ctx = FrameCtx { dt: frame_dt, sensors, audio: Some(&audio) };
                render(&e, &ctx, &mut tile.1, &mut tile.2);
            }
            let colors: Vec<[u8; 3]> = tile.2.iter().map(|c| c.to_u8()).collect();
            let off = if p.dark { egui::Color32::from_rgb(26, 28, 34) } else { egui::Color32::from_rgb(214, 217, 223) };
            super::led::paint_dots(ui.painter(), preview.shrink2(vec2(4.0, 0.0)), &colors, off);
            let painter = ui.painter();
            // label row
            let ly = preview.bottom() + 12.0;
            let label_color = if disabled {
                p.text_disabled
            } else if selected {
                p.text1
            } else {
                p.text2
            };
            painter.text(
                pos2(rect.left() + 10.0, ly + 6.0),
                egui::Align2::LEFT_CENTER,
                short_label(kind),
                font(Ts::Label),
                label_color,
            );
            let badge = if kind.is_hardware() {
                Some(Icon::Chip)
            } else if matches!(kind, EffectKind::Temperature | EffectKind::Load | EffectKind::Music) {
                Some(Icon::Bolt)
            } else if kind == EffectKind::Strobe {
                Some(Icon::Warning)
            } else {
                None
            };
            if let Some(b) = badge {
                let c = if kind == EffectKind::Strobe { p.warning } else { p.text3 };
                icons::paint(
                    painter,
                    Rect::from_center_size(pos2(rect.right() - 16.0, ly + 6.0), vec2(14.0, 14.0)),
                    b,
                    c,
                );
            }
            if selected {
                let c = pos2(rect.right() - 12.0, rect.top() + 12.0);
                painter.circle_filled(c, 8.0, p.accent);
                icons::paint(painter, Rect::from_center_size(c, vec2(11.0, 11.0)), Icon::Check, p.on_accent);
            }
            let tip = if disabled {
                "Wave runs across the whole board. Turn on “Same effect on all zones” to use it."
            } else if kind.is_hardware() {
                "Onboard: runs on the controller itself, also when FanRGB is closed"
            } else if kind == EffectKind::Strobe {
                "Rapid flashing. Avoid if you are sensitive to flashing light."
            } else if matches!(kind, EffectKind::Temperature | EffectKind::Load | EffectKind::Music) {
                "Reacts to live data"
            } else {
                ""
            };
            if !tip.is_empty() {
                let _ = resp.on_hover_text(tip);
            }
        });
        index += kinds.len();
        ui.add_space(6.0);
    }
    if !any_focus {
        st.focus = None;
    }
    if ran {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
    }
    picked
}
