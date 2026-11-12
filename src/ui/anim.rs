//! Motion tokens, eased tweens and the reduce-motion switch.

use egui::{Context, Id};

pub const FAST: f32 = 0.10;
pub const BASE: f32 = 0.16;
pub const SLOW: f32 = 0.24;
pub const VALUE: f32 = 0.45;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum MotionPref {
    /// Follow Windows "Animation effects".
    #[default]
    Auto,
    /// Reduce motion.
    On,
    /// Full motion.
    Off,
}

/// Windows "Show animations in Windows" (Settings > Accessibility > Visual effects).
pub fn system_animations_enabled() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETCLIENTAREAANIMATION, SystemParametersInfoW};
    let mut on: i32 = 1;
    let ok = unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&mut on as *mut i32).cast(), 0) };
    ok == 0 || on != 0
}

fn flag_id() -> Id {
    Id::new("fanrgb.reduce_motion")
}

pub fn set_reduce_motion(ctx: &Context, on: bool) {
    ctx.data_mut(|d| d.insert_temp(flag_id(), on));
}

pub fn reduce_motion(ctx: &Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(flag_id())).unwrap_or(false)
}

fn spin_id() -> Id {
    Id::new("fanrgb.spin_fans")
}

/// Whether fan glyphs spin (user preference, off by default: spinning needs ~30 repaints/s).
pub fn set_spin_fans(ctx: &Context, on: bool) {
    ctx.data_mut(|d| d.insert_temp(spin_id(), on));
}

/// Rotation of a fan glyph for `rpm` right now: 0 unless spinning is enabled and motion allowed.
pub fn spin_angle(ctx: &Context, rpm: Option<f32>) -> f32 {
    let on = ctx.data(|d| d.get_temp::<bool>(spin_id())).unwrap_or(false);
    if !on || reduce_motion(ctx) {
        return 0.0;
    }
    ctx.input(|i| i.time) as f32 * super::widgets::gauge::spin_rate(rpm)
}

pub fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Tween {
    from: f32,
    to: f32,
    t0: f64,
    dur: f32,
}

/// Eased tween towards `target`. Restarts from the current value when the target changes and keeps
/// the frame loop alive only while in flight. Reduce motion makes it instant.
pub fn tween(ctx: &Context, id: Id, target: f32, dur: f32) -> f32 {
    let id = id.with("tween");
    if reduce_motion(ctx) || dur <= 0.0 || !target.is_finite() {
        ctx.data_mut(|d| d.insert_temp(id, Tween { from: target, to: target, t0: 0.0, dur }));
        return target;
    }
    let now = ctx.input(|i| i.time);
    let stored = ctx.data(|d| d.get_temp::<Tween>(id));
    let mut tw = stored.unwrap_or(Tween { from: target, to: target, t0: now, dur });
    if (tw.to - target).abs() > f32::EPSILON {
        let current = sample(&tw, now);
        tw = Tween { from: current, to: target, t0: now, dur };
    }
    let v = sample(&tw, now);
    let in_flight = tw.from != tw.to && now - tw.t0 < tw.dur as f64;
    if !in_flight {
        // Settled: collapse so later frames are cheap.
        tw.from = tw.to;
    }
    if stored != Some(tw) {
        ctx.data_mut(|d| d.insert_temp(id, tw));
    }
    if in_flight {
        ctx.request_repaint();
    }
    v
}

/// [`tween`] for live data: a change smaller than `min_step` jumps instead of animating. A 1 °C
/// tick every second is not worth half a second of full-rate repaints; a big jump still glides.
pub fn tween_data(ctx: &Context, id: Id, target: f32, dur: f32, min_step: f32) -> f32 {
    let key = id.with("tween");
    if let Some(tw) = ctx.data(|d| d.get_temp::<Tween>(key)) {
        let now = ctx.input(|i| i.time);
        let in_flight = tw.from != tw.to && now - tw.t0 < tw.dur as f64;
        if !in_flight && (tw.to - target).abs() < min_step {
            ctx.data_mut(|d| d.insert_temp(key, Tween { from: target, to: target, t0: now, dur }));
            return target;
        }
    }
    tween(ctx, id, target, dur)
}

fn sample(tw: &Tween, now: f64) -> f32 {
    if tw.from == tw.to || tw.dur <= 0.0 {
        return tw.to;
    }
    let t = ((now - tw.t0) as f32 / tw.dur).clamp(0.0, 1.0);
    tw.from + (tw.to - tw.from) * ease_out_cubic(t)
}

/// 0..1 eased toggle (wrapper over egui's bool animation, honoring reduce motion).
pub fn toggle(ctx: &Context, id: Id, on: bool, dur: f32) -> f32 {
    if reduce_motion(ctx) {
        return if on { 1.0 } else { 0.0 };
    }
    ctx.animate_bool_with_time_and_easing(id, on, dur, ease_out_cubic)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn easing_endpoints() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        assert!(ease_out_cubic(0.5) > 0.5);
    }

    #[test]
    fn tween_sampling() {
        let tw = Tween { from: 0.0, to: 10.0, t0: 1.0, dur: 1.0 };
        assert_eq!(sample(&tw, 1.0), 0.0);
        assert_eq!(sample(&tw, 2.5), 10.0);
        let mid = sample(&tw, 1.5);
        assert!(mid > 5.0 && mid < 10.0);
    }
}
