//! Color helpers (floating point sRGB, 0..1).

use crate::config::{ColorCorrection, GAIN_RANGE, GAMMA_RANGE, Segment};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rgb {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

impl Rgb {
    pub const BLACK: Rgb = Rgb { r: 0.0, g: 0.0, b: 0.0 };

    pub const fn new(r: f32, g: f32, b: f32) -> Self {
        Rgb { r, g, b }
    }

    pub fn from_u8(c: [u8; 3]) -> Self {
        Rgb::new(c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0)
    }

    pub fn to_u8(self) -> [u8; 3] {
        let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        [q(self.r), q(self.g), q(self.b)]
    }

    pub fn scale(self, k: f32) -> Self {
        Rgb::new(self.r * k, self.g * k, self.b * k)
    }

    pub fn lerp(self, o: Rgb, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        Rgb::new(self.r + (o.r - self.r) * t, self.g + (o.g - self.g) * t, self.b + (o.b - self.b) * t)
    }

    pub fn add(self, o: Rgb) -> Self {
        Rgb::new((self.r + o.r).min(1.0), (self.g + o.g).min(1.0), (self.b + o.b).min(1.0))
    }

    pub fn max(self, o: Rgb) -> Self {
        Rgb::new(self.r.max(o.r), self.g.max(o.g), self.b.max(o.b))
    }

    /// Screen colour (sRGB-encoded 0..1) → LED drive 0..1: per-channel `v^gamma · gain`. Black and,
    /// with unity gains, the primaries are unchanged. Only for bytes sent to the controller; the UI
    /// preview keeps the screen colour.
    pub fn to_led(self, cc: &ColorCorrection) -> Rgb {
        let fin = |v: f32, d: f32| if v.is_finite() { v } else { d };
        let g = fin(cc.gamma, 1.0).clamp(*GAMMA_RANGE.start(), *GAMMA_RANGE.end());
        let k = |v: f32, gain: f32| {
            fin(v, 0.0).clamp(0.0, 1.0).powf(g) * fin(gain, 1.0).clamp(*GAIN_RANGE.start(), *GAIN_RANGE.end())
        };
        Rgb::new(k(self.r, cc.gain[0]), k(self.g, cc.gain[1]), k(self.b, cc.gain[2])).sanitized()
    }

    /// Replaces NaN / infinite channels with 0 and clamps to 0..1.
    pub fn sanitized(self) -> Self {
        let f = |v: f32| if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 };
        Rgb::new(f(self.r), f(self.g), f(self.b))
    }

    pub fn hsv(h: f32, s: f32, v: f32) -> Self {
        let h = h.rem_euclid(1.0) * 6.0;
        let i = h.floor() as i32;
        let f = h - h.floor();
        let p = v * (1.0 - s);
        let q = v * (1.0 - s * f);
        let t = v * (1.0 - s * (1.0 - f));
        match i {
            0 => Rgb::new(v, t, p),
            1 => Rgb::new(q, v, p),
            2 => Rgb::new(p, v, t),
            3 => Rgb::new(p, q, v),
            4 => Rgb::new(t, p, v),
            _ => Rgb::new(v, p, q),
        }
    }
}

/// The device colour pipeline of one zone: screen colour (sRGB-encoded)
/// → transfer exponent (`gamma`, the EOTF) → zone white balance → segment white balance → master
/// brightness → 8-bit → (byte order, in the session) → wire.
///
/// The review writes the chain as gains → brightness → EOTF; every step after the EOTF is a
/// multiplication in linear light, so the two orders are the same map with the gains expressed
/// in linear drive (`g_linear = g_encoded^gamma`). Linear gains are what the transfer chart and
/// the stored `ColorCorrection::gain` mean, so they are kept. Used only for bytes sent to the
/// controller; previews keep the screen colour.
#[derive(Clone, Copy, Debug)]
pub struct LedPipeline<'a> {
    pub cc: ColorCorrection,
    pub segments: &'a [Segment],
    /// Linear master brightness (already perceptual).
    pub master: f32,
}

impl<'a> LedPipeline<'a> {
    pub fn new(cc: ColorCorrection, segments: &'a [Segment], master: f32) -> Self {
        LedPipeline { cc, segments, master: if master.is_finite() { master.clamp(0.0, 1.0) } else { 0.0 } }
    }

    /// Some LEDs get a different gain than others: a uniform colour is no longer one byte triple,
    /// so it has to be streamed per LED.
    pub fn has_segment_gains(&self) -> bool {
        self.segments.iter().any(Segment::is_active)
    }

    /// Segment gain of LED `i` (1, 1, 1 outside every segment).
    pub fn segment_gain(&self, i: usize) -> [f32; 3] {
        self.segments.iter().find(|s| s.contains(i)).map(|s| s.gain).unwrap_or([1.0; 3])
    }

    /// Linear LED drive 0..1 of LED `i` showing screen colour `c`.
    pub fn drive(&self, i: usize, c: Rgb) -> Rgb {
        let l = c.to_led(&self.cc);
        let k = self.segment_gain(i);
        let g = |v: f32| if v.is_finite() { v.clamp(*GAIN_RANGE.start(), *GAIN_RANGE.end()) } else { 1.0 };
        Rgb::new(l.r * g(k[0]), l.g * g(k[1]), l.b * g(k[2])).scale(self.master).sanitized()
    }

    /// Device bytes for a frame (one entry per LED, LED 0 first).
    pub fn bytes(&self, frame: &[Rgb]) -> Vec<[u8; 3]> {
        frame.iter().enumerate().map(|(i, c)| self.drive(i, *c).to_u8()).collect()
    }

    /// Linear drive values for a frame (input of [`Dither`]).
    pub fn drives(&self, frame: &[Rgb]) -> Vec<Rgb> {
        frame.iter().enumerate().map(|(i, c)| self.drive(i, *c)).collect()
    }
}

/// Temporal dithering (error diffusion over time, per LED and channel) for streamed frames.
///
/// The ideal drive of a dark channel often falls between two 8-bit steps (gamma 2.2 at 20 %
/// brightness turns a 30/255 component into 0.13 steps), so plain rounding drops it and the hue
/// shifts. With dithering each LED alternates between the neighbouring steps so the average is
/// right. Each LED starts at a different phase so neighbours do not toggle together.
///
/// Costs: the frame changes every tick, so the header is streamed at the full frame rate (one
/// 0x58/0x59 packet per 19 LEDs) even for a static colour, and the sync record cannot be used. At
/// 30-40 fps a 1-step toggle on a very dim channel may be visible as shimmer; that is why it is
/// opt-in (`RgbConfig::dither`) and needs a look on the real LEDs.
#[derive(Clone, Debug, Default)]
pub struct Dither {
    err: Vec<[f32; 3]>,
}

impl Dither {
    /// Quantises `drive` (linear 0..1). Returns the bytes and whether a residual remains (the next
    /// frame differs, so it must be sent).
    pub fn quantize(&mut self, drive: &[Rgb]) -> (Vec<[u8; 3]>, bool) {
        if self.err.len() != drive.len() {
            // Golden-ratio phases spread the toggles over the strip.
            self.err = (0..drive.len())
                .map(|i| {
                    let p = (i as f32 * 0.618_034).fract() - 0.5;
                    [p, (p + 0.333).rem_euclid(1.0) - 0.5, (p + 0.667).rem_euclid(1.0) - 0.5]
                })
                .collect();
        }
        let mut moving = false;
        let out = drive
            .iter()
            .zip(self.err.iter_mut())
            .map(|(c, e)| {
                let mut q = [0u8; 3];
                for (k, v) in [c.r, c.g, c.b].into_iter().enumerate() {
                    let want = if v.is_finite() { v.clamp(0.0, 1.0) * 255.0 } else { 0.0 };
                    let exact = want.fract() == 0.0;
                    let x = if exact { want } else { want + e[k] };
                    let b = x.round().clamp(0.0, 255.0);
                    e[k] = if exact { 0.0 } else { (x - b).clamp(-0.5, 0.5) };
                    moving |= !exact;
                    q[k] = b as u8;
                }
                q
            })
            .collect();
        (out, moving)
    }

    pub fn reset(&mut self) {
        self.err.clear();
    }
}

/// Perceptual brightness: a 0..1 slider / envelope value to linear LED intensity (gamma 2.2),
/// so 50 % on a slider looks half as bright instead of barely dimmer.
pub fn perceptual(x: f32) -> f32 {
    if x.is_finite() { x.clamp(0.0, 1.0).powf(2.2) } else { 0.0 }
}

/// Smooth 0..1 ease (for crossfades).
pub fn smoothstep(x: f32) -> f32 {
    let x = if x.is_finite() { x.clamp(0.0, 1.0) } else { 1.0 };
    x * x * (3.0 - 2.0 * x)
}

/// Samples a palette at `t` in 0..1 (optionally cyclic).
pub fn palette(colors: &[Rgb], t: f32, cyclic: bool) -> Rgb {
    match colors.len() {
        0 => Rgb::BLACK,
        1 => colors[0],
        n => {
            if cyclic {
                let x = t.rem_euclid(1.0) * n as f32;
                let i = x.floor() as usize % n;
                colors[i].lerp(colors[(i + 1) % n], x - x.floor())
            } else {
                let x = t.clamp(0.0, 1.0) * (n - 1) as f32;
                let i = (x.floor() as usize).min(n - 2);
                colors[i].lerp(colors[i + 1], x - i as f32)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perceptual_curve() {
        assert_eq!(perceptual(0.0), 0.0);
        assert_eq!(perceptual(1.0), 1.0);
        assert!((perceptual(0.5) - 0.2176).abs() < 1e-3);
        assert_eq!(perceptual(f32::NAN), 0.0);
        assert_eq!(perceptual(2.0), 1.0);
        assert_eq!(smoothstep(0.5), 0.5);
        assert_eq!(smoothstep(f32::NAN), 1.0);
    }

    #[test]
    fn conversions() {
        assert_eq!(Rgb::from_u8([255, 128, 0]).to_u8(), [255, 128, 0]);
        assert_eq!(Rgb::new(f32::NAN, 2.0, -1.0).sanitized(), Rgb::new(0.0, 1.0, 0.0));
        assert_eq!(Rgb::new(f32::NAN, 0.5, 0.0).to_u8()[0], 0);
        assert_eq!(Rgb::hsv(0.0, 1.0, 1.0).to_u8(), [255, 0, 0]);
        assert_eq!(Rgb::hsv(1.0 / 3.0, 1.0, 1.0).to_u8(), [0, 255, 0]);
        assert_eq!(Rgb::hsv(-1.0 / 3.0, 1.0, 1.0).to_u8(), [0, 0, 255]);
    }

    /// Pipeline: EOTF, zone gain, segment gain, master; equivalent to the review's
    /// gains → brightness → EOTF order with encoded gains.
    #[test]
    fn pipeline_order_and_segments() {
        let cc = ColorCorrection { gamma: 2.2, gain: [1.0, 0.8, 1.0] };
        let segs = [Segment { from_led: 2, to_led: 4, gain: [1.0, 1.0, 0.5], ..Default::default() }];
        let p = LedPipeline::new(cc, &segs, 0.5);
        assert!(p.has_segment_gains());
        let c = Rgb::from_u8([200, 200, 200]);
        let v = (200.0f32 / 255.0).powf(2.2);
        let d0 = p.drive(0, c);
        assert!((d0.r - v * 0.5).abs() < 1e-5 && (d0.g - v * 0.8 * 0.5).abs() < 1e-5 && (d0.b - v * 0.5).abs() < 1e-5);
        let d3 = p.drive(3, c);
        assert!((d3.b - v * 0.5 * 0.5).abs() < 1e-5, "segment gain on LED 3");
        assert_eq!(p.drive(4, c), d0, "end is exclusive");
        // Review order: (c · g^(1/γ) · m^(1/γ))^γ gives the same drive.
        let alt = (200.0f32 / 255.0 * 0.8f32.powf(1.0 / 2.2) * 0.5f32.powf(1.0 / 2.2)).powf(2.2);
        assert!((d0.g - alt).abs() < 1e-5);
        assert_eq!(p.bytes(&[c; 5]).len(), 5);
        let unity = [Segment { from_led: 0, to_led: 4, ..Default::default() }];
        assert!(!LedPipeline::new(cc, &unity, 1.0).has_segment_gains());
        assert_eq!(LedPipeline::new(cc, &[], f32::NAN).drive(0, c), Rgb::BLACK);
    }

    #[test]
    fn dither_averages_to_the_exact_value() {
        let mut d = Dither::default();
        let drive = [Rgb::new(0.3 / 255.0, 100.0 / 255.0, 2.7 / 255.0), Rgb::new(1.0, 0.0, 0.5 / 255.0)];
        let (mut s0, mut s1, n) = ([0u32; 3], [0u32; 3], 300);
        for _ in 0..n {
            let (q, moving) = d.quantize(&drive);
            assert!(moving);
            for k in 0..3 {
                s0[k] += q[0][k] as u32;
                s1[k] += q[1][k] as u32;
            }
        }
        let avg = |s: u32| s as f32 / n as f32;
        assert!((avg(s0[0]) - 0.3).abs() < 0.01 && avg(s0[1]) == 100.0 && (avg(s0[2]) - 2.7).abs() < 0.01);
        assert!(avg(s1[0]) == 255.0 && avg(s1[1]) == 0.0 && (avg(s1[2]) - 0.5).abs() < 0.01);
        // Exact values do not need streaming.
        let mut d = Dither::default();
        assert_eq!(d.quantize(&[Rgb::new(1.0, 0.0, 0.0)]), (vec![[255, 0, 0]], false));
    }

    /// The preset "Red" FF1E1E drives green/blue at about 1 % instead
    /// of 11.8 %, primaries and black stay exact, gains scale after the gamma step.
    #[test]
    fn led_correction() {
        let cc = ColorCorrection::default();
        let red = Rgb::from_u8([0xFF, 0x1E, 0x1E]).to_led(&cc);
        assert_eq!(red.r, 1.0);
        assert!(red.g > 0.005 && red.g < 0.02, "green drive {}", red.g);
        assert_eq!(red.g, red.b);
        assert_eq!(Rgb::from_u8([255, 0, 0]).to_led(&cc).to_u8(), [255, 0, 0]);
        assert_eq!(Rgb::from_u8([0, 255, 255]).to_led(&cc).to_u8(), [0, 255, 255]);
        assert_eq!(Rgb::BLACK.to_led(&cc), Rgb::BLACK);
        // The user's (255, 86, 30): green 33.7 % raw → about 9 %.
        let c = Rgb::from_u8([255, 86, 30]).to_led(&cc);
        assert!((c.g - 0.0915).abs() < 0.005, "{c:?}");
        // Gamma 1 = raw bytes (GCC behaviour).
        let raw = ColorCorrection { gamma: 1.0, ..Default::default() };
        assert_eq!(Rgb::from_u8([255, 86, 30]).to_led(&raw).to_u8(), [255, 86, 30]);
        // White balance.
        let wb = ColorCorrection { gamma: 1.0, gain: [1.0, 0.75, 0.5] };
        assert_eq!(Rgb::from_u8([255, 255, 255]).to_led(&wb).to_u8(), [255, 191, 128]);
        // Garbage in, safe out.
        let bad = ColorCorrection { gamma: f32::NAN, gain: [f32::INFINITY, -3.0, f32::NAN] };
        let o = Rgb::new(f32::NAN, 1.0, 1.0).to_led(&bad);
        assert!(o.r == 0.0 && o.g > 0.0 && o.g <= 1.0 && o.b == 1.0, "{o:?}");
    }
}
