//! Software lighting effects rendered by the app ("direct" mode), and previews of the
//! controller's built-in effects.
//!
//! Conventions: colours are sRGB-encoded 0..1 (screen colours) in R, G, B order; the per-zone
//! gamma / white balance ([`super::color::LedPipeline`]) and the wire order are applied at output;
//! `Effect::speed` is 0 (slow) ..= 1 (fast). Brightness sliders and fade envelopes scale the
//! *encoded* colour by their plain value: the device pipeline's transfer exponent (2.2) and the
//! monitor's sRGB decode both turn that into perceptual steps, so they must not be made
//! perceptual here as well (that applied the curve twice once gamma correction existed). Dim
//! "glow" levels are given in linear light through [`glow`]. "Forward" motion runs from LED 0
//! towards the end of the strip.
//! Every input is sanitised, so NaN/inf in a config or sensor can't reach the LEDs.

use std::collections::HashMap;
use std::f32::consts::TAU;

use super::color::{Rgb, palette};
use super::fusion2::speed as hw;
use crate::config::{Effect, EffectKind};

#[derive(Clone, Debug, Default)]
pub struct AudioLevels {
    /// Overall loudness 0..1 (auto-gained).
    pub level: f32,
    /// Spectrum bands (low to high), 0..1.
    pub bands: [f32; 16],
    /// Decaying beat pulse 0..1.
    pub beat: f32,
}

pub struct FrameCtx<'a> {
    pub dt: f32,
    pub sensors: &'a HashMap<String, f32>,
    pub audio: Option<&'a AudioLevels>,
}

/// Small xorshift PRNG (deterministic, allocation-free).
#[derive(Clone, Debug)]
struct Rng(u64);

impl Rng {
    fn next_f32(&mut self) -> f32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let v = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (v >> 40) as f32 / (1u64 << 24) as f32
    }
}

pub struct EffectState {
    rng: Rng,
    phase: f32,
    /// Fixed-step accumulator (Fire).
    acc: f32,
    heat: Vec<f32>,
    sparkle: Vec<(f32, usize)>,
    smooth: Option<f32>,
}

impl Default for EffectState {
    fn default() -> Self {
        EffectState { rng: Rng(0xC0FFEE), phase: 0.0, acc: 0.0, heat: Vec::new(), sparkle: Vec::new(), smooth: None }
    }
}

fn fin(v: f32, default: f32) -> f32 {
    if v.is_finite() { v } else { default }
}

fn colors(e: &Effect) -> Vec<Rgb> {
    if e.colors.is_empty() {
        vec![Rgb::new(1.0, 1.0, 1.0)]
    } else {
        e.colors.iter().map(|c| Rgb::from_u8(*c)).collect()
    }
}

/// GCC speed level 0 (slowest) ..= 9 (fastest) for the 0..1 speed slider.
pub fn speed_level(speed: f32) -> u8 {
    (fin(speed, 0.5).clamp(0.0, 1.0) * 9.0).round() as u8
}

/// Seconds per cycle for the speed slider (0 = slow, 1 = fast), exponential in between.
fn period(speed: f32, slow: f32, fast: f32) -> f32 {
    slow * (fast / slow).powf(fin(speed, 0.5).clamp(0.0, 1.0))
}

/// Advances `phase` by `delta` and wraps it into `0..wrap` (keeps f32 precision over days).
fn advance(phase: &mut f32, delta: f32, wrap: f32) {
    *phase = (*phase + fin(delta, 0.0)).rem_euclid(wrap);
    if !phase.is_finite() {
        *phase = 0.0;
    }
}

/// Simulation rate of the Fire effect (independent of the frame rate).
const FIRE_STEP: f32 = 1.0 / 32.0;

pub fn render(e: &Effect, ctx: &FrameCtx, st: &mut EffectState, out: &mut [Rgb]) {
    let n = out.len();
    if n == 0 {
        return;
    }
    let dt = fin(ctx.dt, 0.0).clamp(0.0, 0.5);
    let speed = fin(e.speed, 0.5).clamp(0.0, 1.0);
    let lv = speed_level(speed) as usize;
    let pal = colors(e);
    let c0 = pal[0];
    let dir = if e.reverse { -1.0 } else { 1.0 };
    let index_pos = |i: usize| -> f32 {
        let p = if n > 1 { i as f32 / n as f32 } else { 0.0 };
        if e.reverse { 1.0 - p } else { p }
    };

    match e.kind {
        EffectKind::Off => out.fill(Rgb::BLACK),
        EffectKind::Static | EffectKind::HwStatic => out.fill(c0),
        EffectKind::Breathing => {
            advance(&mut st.phase, dt / period(speed, 7.0, 1.2), 1.0);
            let l = 0.5 - 0.5 * (TAU * st.phase).cos();
            out.fill(c0.scale(l.max(glow(1.0 / 255.0))));
        }
        EffectKind::HwBreathing => {
            // Firmware Pulse: rise t0, fall t1, dark t2 (ms).
            let (t0, t1, t2) =
                (hw::PULSE[lv] as f32 / 1000.0, hw::PULSE[lv] as f32 / 1000.0, hw::PULSE_T2 as f32 / 1000.0);
            let total = t0 + t1 + t2;
            advance(&mut st.phase, dt / total, 1.0);
            let x = st.phase * total;
            let l = if x < t0 {
                x / t0
            } else if x < t0 + t1 {
                1.0 - (x - t0) / t1
            } else {
                0.0
            };
            out.fill(c0.scale(l));
        }
        EffectKind::Spectrum => {
            advance(&mut st.phase, dt / period(speed, 30.0, 2.0), 1.0);
            out.fill(Rgb::hsv(st.phase, 1.0, 1.0));
        }
        EffectKind::HwColorCycle => {
            // Seven colours, one step per t0.
            advance(&mut st.phase, dt / (7.0 * hw::CYCLE[lv].0 as f32 / 1000.0), 1.0);
            out.fill(Rgb::hsv(st.phase, 1.0, 1.0));
        }
        EffectKind::Rainbow | EffectKind::HwWave => {
            let p = if e.kind == EffectKind::HwWave {
                hw::WAVE[lv] as f32 * 40.0 / 1000.0
            } else {
                period(speed, 12.0, 0.8)
            };
            advance(&mut st.phase, dt / p, 1.0);
            let span = n.max(12) as f32;
            for (i, px) in out.iter_mut().enumerate() {
                *px = Rgb::hsv(i as f32 / span - st.phase * dir, 1.0, 1.0);
            }
        }
        EffectKind::Gradient => {
            if speed > 0.02 {
                advance(&mut st.phase, dt / period(speed, 40.0, 3.0), 1.0);
            }
            for (i, px) in out.iter_mut().enumerate() {
                *px = palette(&pal, index_pos(i) - st.phase, true);
            }
        }
        EffectKind::Comet => {
            advance(&mut st.phase, dt / period(speed, 6.0, 0.6), 1.0);
            if n == 1 {
                let l = (st.phase * TAU).sin().max(0.0);
                out[0] = c0.scale(l);
            } else {
                let tail = (n as f32 / 3.0).max(3.0);
                let span = n as f32 + tail;
                let head = st.phase * span;
                for (i, px) in out.iter_mut().enumerate() {
                    let pos = if e.reverse { (n - 1 - i) as f32 } else { i as f32 };
                    let d = head - pos;
                    let l = if (0.0..tail).contains(&d) { 1.0 - d / tail } else { 0.0 };
                    *px = c0.scale(l).max(c0.scale(glow(0.03)));
                }
            }
        }
        EffectKind::Chase => {
            advance(&mut st.phase, dt / period(speed, 1.2, 0.08), 3.0);
            let step = st.phase.floor() as i64;
            for (i, px) in out.iter_mut().enumerate() {
                let on = (i as i64 - step * dir as i64).rem_euclid(3) == 0;
                *px = if on { c0 } else { c0.scale(glow(0.04)) };
            }
        }
        EffectKind::Twinkle => {
            if st.sparkle.len() != n {
                st.sparkle = vec![(0.0, 0); n];
            }
            let rate = 0.6 + 6.0 * speed;
            for (i, px) in out.iter_mut().enumerate() {
                let s = &mut st.sparkle[i];
                s.0 = (s.0 - dt * (0.6 + 2.0 * speed)).max(0.0);
                if st.rng.next_f32() < rate * dt / n as f32 * 4.0 {
                    *s = (1.0, (st.rng.next_f32() * pal.len() as f32) as usize);
                }
                // The palette may have shrunk since the sparkle was born.
                *px = pal[s.1 % pal.len()].scale(s.0);
            }
        }
        EffectKind::Fire => {
            if st.heat.len() != n {
                st.heat = vec![0.0; n];
            }
            st.acc = (st.acc + dt).min(FIRE_STEP * 20.0);
            let cooling = 0.25 + 0.5 * (1.0 - speed);
            while st.acc >= FIRE_STEP {
                st.acc -= FIRE_STEP;
                if n == 1 {
                    st.heat[0] = 0.5 + 0.5 * st.rng.next_f32();
                    continue;
                }
                for h in st.heat.iter_mut() {
                    *h = (*h - st.rng.next_f32() * cooling * 0.25).max(0.0);
                }
                for k in (2..n).rev() {
                    st.heat[k] = (st.heat[k - 1] + st.heat[k - 2] * 2.0) / 3.0;
                }
                if st.rng.next_f32() < 0.55 + 0.4 * speed {
                    let y = ((st.rng.next_f32() * n.min(7) as f32) as usize).min(n - 1);
                    st.heat[y] = (st.heat[y] + 0.6 + st.rng.next_f32() * 0.4).min(1.0);
                }
            }
            let ramp = [Rgb::BLACK, Rgb::new(0.7, 0.0, 0.0), Rgb::new(1.0, 0.35, 0.0), Rgb::new(1.0, 0.85, 0.3)];
            for (i, px) in out.iter_mut().enumerate() {
                let idx = if e.reverse { n - 1 - i } else { i };
                *px = palette(&ramp, st.heat[idx], false);
            }
        }
        EffectKind::Temperature | EffectKind::Load => {
            let (low, high) = (fin(e.low, 40.0), fin(e.high, 85.0));
            let raw = ctx.sensors.get(&e.sensor).copied().filter(|v| v.is_finite());
            let f = raw.map(|v| ((v - low) / (high - low).max(0.1)).clamp(0.0, 1.0)).unwrap_or(0.0);
            let f = match st.smooth.filter(|p| p.is_finite()) {
                None => f,
                Some(p) => p + (f - p) * (1.0 - (-dt / 1.2).exp()),
            };
            st.smooth = Some(f);
            let pal3: Vec<Rgb> = if pal.len() >= 3 {
                pal.clone()
            } else {
                vec![Rgb::new(0.0, 0.5, 1.0), Rgb::new(0.0, 1.0, 0.3), Rgb::new(1.0, 0.0, 0.0)]
            };
            if e.kind == EffectKind::Load && n > 1 {
                let lit = f * n as f32;
                for (i, px) in out.iter_mut().enumerate() {
                    let pos = if e.reverse { (n - 1 - i) as f32 } else { i as f32 };
                    let l = (lit - pos).clamp(0.0, 1.0);
                    *px = palette(&pal3, pos / n as f32, false).scale(l.max(glow(0.05)));
                }
            } else {
                advance(&mut st.phase, dt, 2.0);
                let pulse = if f >= 0.999 { 0.7 + 0.3 * (st.phase * TAU * 1.5).cos() } else { 1.0 };
                out.fill(palette(&pal3, f, false).scale(pulse));
            }
        }
        EffectKind::Music => {
            let Some(a) = ctx.audio else {
                advance(&mut st.phase, dt / 4.0, 1.0);
                out.fill(c0.scale(0.15 + 0.15 * (TAU * st.phase).sin()));
                return;
            };
            let level = fin(a.level, 0.0).clamp(0.0, 1.0);
            let beat = fin(a.beat, 0.0).clamp(0.0, 1.0);
            let c1 = pal.get(1).copied().unwrap_or(Rgb::new(1.0, 0.0, 0.6));
            if n == 1 {
                out[0] = c0.lerp(c1, beat).scale((0.08 + level).min(1.0));
            } else {
                // Mirrored spectrum: bass in the middle, highs at the ends.
                let half = (n as f32 / 2.0).max(1.0);
                for (i, px) in out.iter_mut().enumerate() {
                    let d = (((i as f32 + 0.5) - n as f32 / 2.0).abs() / half).min(1.0);
                    let band = ((d * 15.0) as usize).min(15);
                    let v = fin(a.bands[band], 0.0).clamp(0.0, 1.0);
                    *px = c0.lerp(c1, d).scale((0.04 + v).min(1.0)).add(Rgb::new(1.0, 1.0, 1.0).scale(beat * 0.15));
                }
            }
        }
        EffectKind::Strobe => {
            advance(&mut st.phase, dt / period(speed, 1.5, 0.08), 1.0);
            out.fill(if st.phase < 0.18 { c0 } else { Rgb::BLACK });
        }
        EffectKind::HwFlash | EffectKind::HwDoubleFlash => {
            // Firmware Flash: 100 ms on (t0), 100 ms gap (t1), cycle t2.
            let double = e.kind == EffectKind::HwDoubleFlash;
            let total = if double { hw::DFLASH[lv] } else { hw::FLASH[lv] } as f32 / 1000.0;
            advance(&mut st.phase, dt / total, 1.0);
            let x = st.phase * total;
            let on = x < 0.1 || (double && (0.2..0.3).contains(&x));
            out.fill(if on { c0 } else { Rgb::BLACK });
        }
        EffectKind::HwRandom => {
            st.phase += dt * 4.0;
            if st.phase >= 1.0 || st.sparkle.len() != n {
                st.phase = 0.0;
                st.sparkle = (0..n).map(|_| (st.rng.next_f32(), 0)).collect();
            }
            for (i, px) in out.iter_mut().enumerate() {
                *px = Rgb::hsv(st.sparkle[i].0, 1.0, 1.0);
            }
        }
    }
    let b = fin(e.brightness, 1.0).clamp(0.0, 1.0);
    for px in out.iter_mut() {
        *px = if b < 0.999 { px.scale(b) } else { *px }.sanitized();
    }
}

/// Encoded-domain factor for a dim level given in linear light (`lin^(1/2.2)`), so a background
/// glow keeps its intended LED intensity after the 2.2 transfer step.
fn glow(lin: f32) -> f32 {
    lin.clamp(0.0, 1.0).powf(1.0 / 2.2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_kinds() -> Vec<EffectKind> {
        EffectKind::SOFTWARE.iter().chain(EffectKind::HARDWARE.iter()).copied().collect()
    }

    fn run(e: &Effect, n: usize, frames: usize, dt: f32) -> Vec<Rgb> {
        let sensors: HashMap<String, f32> = [("cpu.temp".to_string(), 70.0)].into_iter().collect();
        let audio = AudioLevels { level: 0.5, bands: [0.3; 16], beat: 0.8 };
        let mut st = EffectState::default();
        let mut out = vec![Rgb::BLACK; n];
        for _ in 0..frames {
            render(e, &FrameCtx { dt, sensors: &sensors, audio: Some(&audio) }, &mut st, &mut out);
        }
        out
    }

    fn ok(c: &Rgb) -> bool {
        [c.r, c.g, c.b].iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v))
    }

    #[test]
    fn every_effect_renders_finite_colors() {
        for kind in all_kinds() {
            for n in [1usize, 2, 30, 1024] {
                let e = Effect { kind, ..Default::default() };
                assert!(run(&e, n, 50, 0.033).iter().all(ok), "{kind:?} n={n}");
            }
        }
    }

    #[test]
    fn hostile_inputs_stay_finite() {
        let sensors: HashMap<String, f32> = [("cpu.temp".to_string(), f32::NAN)].into_iter().collect();
        let audio = AudioLevels { level: f32::NAN, bands: [f32::INFINITY; 16], beat: f32::NAN };
        for kind in all_kinds() {
            let e = Effect {
                kind,
                speed: f32::NAN,
                brightness: f32::INFINITY,
                low: f32::NAN,
                high: f32::NEG_INFINITY,
                colors: vec![],
                ..Default::default()
            };
            let mut st = EffectState::default();
            let mut out = vec![Rgb::BLACK; 8];
            for dt in [f32::NAN, 0.0, 1e9, 0.02] {
                render(&e, &FrameCtx { dt, sensors: &sensors, audio: Some(&audio) }, &mut st, &mut out);
                assert!(out.iter().all(ok), "{kind:?}");
            }
        }
    }

    /// The slider scales the encoded colour; the device's 2.2 step makes it perceptual (50 % →
    /// 22 % light), once, not twice.
    #[test]
    fn brightness_is_perceptual() {
        let e =
            Effect { kind: EffectKind::Static, colors: vec![[255, 255, 255]], brightness: 0.5, ..Default::default() };
        let c = run(&e, 1, 1, 0.02)[0];
        assert!((c.r - 0.5).abs() < 1e-6, "{c:?}");
        let led = c.to_led(&crate::config::ColorCorrection::default());
        assert!((led.r - 0.2176).abs() < 1e-3, "{led:?}");
        assert!((glow(0.2176) - 0.5).abs() < 1e-3);
        let e = Effect { brightness: 0.0, ..e };
        assert_eq!(run(&e, 1, 1, 0.02)[0], Rgb::BLACK);
    }

    #[test]
    fn colour_order_is_rgb() {
        let e = Effect { kind: EffectKind::Static, colors: vec![[255, 128, 0]], brightness: 1.0, ..Default::default() };
        assert_eq!(run(&e, 3, 1, 0.02)[2].to_u8(), [255, 128, 0]);
    }

    #[test]
    fn twinkle_survives_palette_shrinking() {
        let sensors = HashMap::new();
        let mut st = EffectState::default();
        let mut out = vec![Rgb::BLACK; 30];
        let mut e = Effect {
            kind: EffectKind::Twinkle,
            colors: vec![[1, 2, 3], [4, 5, 6], [7, 8, 9]],
            speed: 1.0,
            ..Default::default()
        };
        for _ in 0..200 {
            render(&e, &FrameCtx { dt: 0.05, sensors: &sensors, audio: None }, &mut st, &mut out);
        }
        e.colors = vec![[255, 0, 0]];
        render(&e, &FrameCtx { dt: 0.01, sensors: &sensors, audio: None }, &mut st, &mut out);
    }

    #[test]
    fn chase_moves_forward() {
        let e = Effect { kind: EffectKind::Chase, colors: vec![[255, 255, 255]], speed: 0.0, ..Default::default() };
        let sensors = HashMap::new();
        let mut st = EffectState::default();
        let mut out = vec![Rgb::BLACK; 6];
        render(&e, &FrameCtx { dt: 0.0, sensors: &sensors, audio: None }, &mut st, &mut out);
        assert!(out[0].r > 0.5);
        // One step (1.2 s at speed 0) later the lit LED is one further along.
        render(&e, &FrameCtx { dt: 0.4, sensors: &sensors, audio: None }, &mut st, &mut out);
        render(&e, &FrameCtx { dt: 0.4, sensors: &sensors, audio: None }, &mut st, &mut out);
        render(&e, &FrameCtx { dt: 0.41, sensors: &sensors, audio: None }, &mut st, &mut out);
        assert!(out[1].r > 0.5 && out[0].r < 0.5, "{out:?}");
    }

    #[test]
    fn fire_is_frame_rate_independent() {
        // Same simulated time at 16 and 64 fps: same number of simulation steps.
        let e = Effect { kind: EffectKind::Fire, ..Default::default() };
        let a = run(&e, 20, 32, 0.0625);
        let b = run(&e, 20, 128, 0.015625);
        let sum = |v: &[Rgb]| v.iter().map(|c| c.r + c.g).sum::<f32>();
        assert!((sum(&a) - sum(&b)).abs() < 1e-3);
    }

    #[test]
    fn speed_levels() {
        assert_eq!(speed_level(0.0), 0);
        assert_eq!(speed_level(1.0), 9);
        assert_eq!(speed_level(0.5), 5);
        assert_eq!(speed_level(f32::NAN), 5);
    }
}
