//! ITE SmartGuardian automatic fan curves, as GIGABYTE's own software uses them on IT8689E-class
//! ("V3") chips.
//!
//! The chip runs a piecewise-linear curve on its own: a main segment in bank 0
//! (Tstart/StartPWM/slope, Tfull) plus up to nine "extra vector" segments in banks 2/3, each with
//! its own start temperature, start PWM and slope. At or above Tfull the chip forces 100 %.
//! This module fits arbitrary (°C, %) points to that model, encodes/decodes the register image
//! and evaluates a curve (used by the simulator and for the duty estimate).

use serde::{Deserialize, Serialize};

/// Extra-vector segments per fan (bank 2 zones A-C, bank 3 zones D-I).
pub const MAX_EV: usize = 9;
/// Main segment + extra vectors.
pub const MAX_SEGMENTS: usize = 1 + MAX_EV;
/// Highest Tfull we ever program (the hardware failsafe must stay reachable).
pub const MAX_TFULL: u8 = 100;
/// Slope register limit in 1/8 PWM counts per °C (integer part kept below 0x80).
pub const MAX_SLOPE8: i16 = 127 * 8 + 7;
/// Ramp used by flat ("fixed duty") curves towards the failsafe temperature: 16 counts/°C
/// (≈6 %/°C, GIGABYTE's steepest preset slope), so the ramp spans at most 15 °C.
pub const FLAT_RAMP_SLOPE8: i16 = 16 * 8;

/// Index into [`CurveImage::sg`] (bank 0, base 0x60/0x68/0x70/0x78/0xA0/0xA8 + offset).
pub const SG_TOFF: usize = 0;
pub const SG_TSTART: usize = 1;
pub const SG_TFULL: usize = 2;
pub const SG_PWM: usize = 3;
pub const SG_SLOPE: usize = 4;
pub const SG_DT: usize = 5;
/// Bits of the ΔT register that hold the hysteresis (GCC writes 0..=3; the rest is kept).
pub const DT_MASK: u8 = 0x03;
/// Bank 2 offset of the main slope fraction (bits 6:4).
pub const B2_MAIN_FRAC: usize = 12;
/// Bank 3 offset of the extra-vector enable bits D..I (bits 0..5).
pub const B3_ENABLE: usize = 12;

/// Bank-0 SmartGuardian register bases per fan.
pub const SG_BASE: [u8; 6] = [0x60, 0x68, 0x70, 0x78, 0xA0, 0xA8];

/// Bank 2 / bank 3 per-fan block base (0x10 + 0x10 n).
pub fn ev_base(fan: usize) -> u8 {
    0x10 + 0x10 * fan as u8
}

/// Bank 3 start-PWM registers of zones D..I for `fan` (0x70, 0x76, 0x80, 0x86, 0x90, 0x96).
pub fn ev_pwm_base(fan: usize) -> u8 {
    0x70 + 0x10 * (fan / 2) as u8 + 6 * (fan % 2) as u8
}

/// Every curve register of one fan channel (everything we may modify, except the PWM control
/// register). Part of the crash snapshot, so the firmware curve can be restored exactly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurveImage {
    /// Bank 0: Toff, Tstart, Tfull, StartPWM, slope integer ("control"), ΔT.
    pub sg: [u8; 6],
    /// Bank 2, 0x10+0x10n ..= 0x1C+0x10n: zones A-C (start, slope int, frac/sign/enable, PWM) + main frac.
    pub bank2: [u8; 13],
    /// Bank 3, 0x10+0x10n ..= 0x1F+0x10n: zones D-I start/slope int, enable bits, frac/sign nibbles.
    pub bank3: [u8; 16],
    /// Bank 3 start PWM of zones D-I.
    pub bank3_pwm: [u8; 6],
}

/// One linear piece: from `start` °C the output is `pwm + slope8 / 8 * (T - start)` (PWM counts).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub start: u8,
    pub pwm: u8,
    /// Slope in 1/8 PWM counts per °C (negative only when decoded from firmware).
    pub slope8: i16,
}

/// A curve the chip can execute.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChipCurve {
    /// Below this temperature the fan is off (0 = never stops).
    pub toff: u8,
    /// `segs[0]` is the main curve (Tstart, StartPWM, slope); the rest are extra vectors with
    /// ascending start temperatures.
    pub segs: Vec<Segment>,
    /// At or above this temperature the chip runs the fan at 100 %.
    pub tfull: u8,
    /// Hysteresis ΔT (°C, 0..=3) for the bank-0 ΔT register; `None` keeps the image's value.
    /// [`decode`] leaves it `None` (it only describes the curve's shape).
    #[serde(default)]
    pub dt: Option<u8>,
}

impl ChipCurve {
    /// PWM (0..=255) the chip outputs at temperature `t` (°C).
    pub fn eval(&self, t: f32) -> f32 {
        if t >= self.tfull as f32 {
            return 255.0;
        }
        if self.toff > 0 && t < self.toff as f32 {
            return 0.0;
        }
        let Some(first) = self.segs.first() else { return 255.0 };
        if t < first.start as f32 {
            return first.pwm as f32;
        }
        let seg = self.segs.iter().rev().find(|s| t >= s.start as f32).unwrap_or(first);
        (seg.pwm as f32 + seg.slope8 as f32 / 8.0 * (t - seg.start as f32)).clamp(0.0, 255.0)
    }

    /// Checks that the curve is one we are willing to program: 100 % is reached at a sane
    /// temperature, the fan is not held off up to that point, and the segments are what
    /// [`encode`] can represent ([`fit`] and [`flat`] always produce such curves).
    pub fn check_programmable(&self) -> Result<(), String> {
        if !(1..=MAX_TFULL).contains(&self.tfull) {
            return Err(format!("full-speed temperature {} is outside 1..={MAX_TFULL}", self.tfull));
        }
        if self.toff >= self.tfull {
            return Err(format!("fan-off temperature {} is not below the full-speed temperature", self.toff));
        }
        if self.segs.is_empty() || self.segs.len() > MAX_SEGMENTS {
            return Err(format!("{} segments (1..={MAX_SEGMENTS} supported)", self.segs.len()));
        }
        if self.segs.windows(2).any(|w| w[0].start >= w[1].start) || self.segs.iter().any(|s| s.start >= self.tfull) {
            return Err("segment start temperatures are not ascending below the full-speed temperature".into());
        }
        if self.segs.iter().any(|s| !(0..=MAX_SLOPE8).contains(&s.slope8)) {
            return Err("segment slope out of range".into());
        }
        Ok(())
    }

    /// Duty in percent at `t`.
    pub fn duty_at(&self, t: f32) -> f32 {
        self.eval(t) / 2.55
    }

    /// Whether a curve is programmed: the main curve registers are not all zero.
    pub fn activated(&self) -> bool {
        let m = self.segs.first();
        self.toff > 0 || m.is_some_and(|s| s.start > 0 || s.pwm > 0 || s.slope8 > 0)
    }

    /// Plausible as a curve to hand back to the firmware: reaches 100 % at a sane temperature and
    /// does not keep the fan off up to that point.
    pub fn is_safe(&self) -> bool {
        (1..=MAX_TFULL).contains(&self.tfull) && self.toff < self.tfull && !self.segs.is_empty()
    }
}

fn pct_to_pwm(pct: f32) -> f32 {
    pct.clamp(0.0, 100.0) * 2.55
}

/// A "fixed duty" curve with a hardware failsafe: `duty_pct` everywhere, ramping at
/// [`FLAT_RAMP_SLOPE8`] so that it reaches 100 % at `full_speed_at`.
pub fn flat(duty_pct: f32, full_speed_at: f32) -> ChipCurve {
    let tfull = clamp_tfull(full_speed_at);
    let d = pct_to_pwm(if duty_pct.is_finite() { duty_pct } else { 100.0 }).round() as i32;
    let ramp = FLAT_RAMP_SLOPE8 as i32 / 8;
    let r = ((255 - d) / ramp).clamp(1, tfull as i32);
    let start = tfull as i32 - r;
    // d + ramp * (r - 1) <= 255 holds by construction, so the ramp never overflows before Tfull.
    let slope8 = if d >= 255 { 0 } else { FLAT_RAMP_SLOPE8 };
    ChipCurve { toff: 0, segs: vec![Segment { start: start as u8, pwm: d as u8, slope8 }], tfull, dt: None }
}

fn clamp_tfull(t: f32) -> u8 {
    if !t.is_finite() {
        return MAX_TFULL;
    }
    t.round().clamp(1.0, MAX_TFULL as f32) as u8
}

/// The polyline we want, with flat extrapolation (PWM counts).
struct Target {
    pts: Vec<(f32, f32)>,
}

impl Target {
    fn at(&self, t: f32) -> f32 {
        let p = &self.pts;
        if t <= p[0].0 {
            return p[0].1;
        }
        for w in p.windows(2) {
            if t <= w[1].0 {
                let dx = w[1].0 - w[0].0;
                if dx <= f32::EPSILON {
                    return w[1].1;
                }
                return w[0].1 + (w[1].1 - w[0].1) * (t - w[0].0) / dx;
            }
        }
        p[p.len() - 1].1
    }
}

/// Builds chip segments starting at the breakpoints `bps` (ascending, all < `tfull`).
fn build(target: &Target, bps: &[i32], tfull: i32) -> Vec<Segment> {
    bps.iter()
        .enumerate()
        .map(|(j, &start)| {
            let end = bps.get(j + 1).copied().unwrap_or(tfull);
            let pwm = target.at(start as f32).round().clamp(0.0, 255.0);
            let slope = (target.at(end as f32) - pwm) / (end - start) as f32;
            let mut slope8 = (slope * 8.0).round().clamp(0.0, MAX_SLOPE8 as f32) as i32;
            // Never let a segment run past 255 before the next one starts (the chip may not saturate).
            let span = end - 1 - start;
            if span > 0 {
                slope8 = slope8.min(((255.0 - pwm) * 8.0 / span as f32).floor() as i32);
            }
            Segment { start: start as u8, pwm: pwm as u8, slope8: slope8.max(0) as i16 }
        })
        .collect()
}

/// Worst deviation of the chip curve from the target over 0..tfull (undershoot = less cooling
/// counts double).
fn fit_error(target: &Target, curve: &ChipCurve) -> f32 {
    (0..curve.tfull as i32)
        .map(|t| {
            let d = curve.eval(t as f32) - target.at(t as f32);
            if d < 0.0 { -2.0 * d } else { d }
        })
        .fold(0.0, f32::max)
}

/// Fits (°C, duty %) points to at most `max_segments` chip segments with 100 % from
/// `full_speed_at`. Duties are forced non-decreasing (hotter never means slower).
pub fn fit(points: &[(f32, f32)], full_speed_at: f32, max_segments: usize) -> Result<ChipCurve, String> {
    if points.is_empty() {
        return Err("curve has no points".into());
    }
    if points.iter().any(|(t, d)| !t.is_finite() || !d.is_finite()) {
        return Err("curve contains a non-finite value".into());
    }
    let tfull = clamp_tfull(full_speed_at);
    let mut pts: Vec<(f32, f32)> = points.iter().map(|&(t, d)| (t, pct_to_pwm(d))).collect();
    pts.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut top = 0.0f32;
    for p in &mut pts {
        top = top.max(p.1);
        p.1 = top;
    }
    // Several points at one temperature are a vertical step: the chip gets the upper duty there
    // (after the envelope above, the last point of such a group carries it).
    let mut uniq: Vec<(f32, f32)> = Vec::with_capacity(pts.len());
    for p in pts {
        match uniq.last_mut() {
            Some(l) if l.0 == p.0 => l.1 = p.1,
            _ => uniq.push(p),
        }
    }
    let target = Target { pts: uniq };
    let tf = tfull as i32;
    let x0 = (target.pts[0].0.floor() as i32).clamp(0, tf - 1);
    // Breakpoints on both whole degrees around every point, so a rise between two points less than
    // 1 °C apart gets its own short segment instead of being smeared over the next segment.
    let mut bps: Vec<i32> = vec![x0];
    for p in &target.pts {
        for x in [p.0.floor() as i32, p.0.ceil() as i32] {
            if x > x0 && x < tf {
                bps.push(x);
            }
        }
    }
    bps.sort_unstable();
    bps.dedup();

    let max_segments = max_segments.max(1);
    let curve_of = |bps: &[i32]| ChipCurve { toff: 0, segs: build(&target, bps, tf), tfull, dt: None };
    let mut best = curve_of(&bps);
    // Error of the curve with every breakpoint: a breakpoint is only dropped "for free" if the
    // result is no worse than this (not than the running maximum, which one bad spot would inflate
    // until every breakpoint goes).
    let free = fit_error(&target, &best) + 0.01;
    let mut err = free;
    // Greedily drop breakpoints: always while over capacity, otherwise only when it costs nothing
    // (collinear points), which keeps register traffic down.
    while bps.len() > 1 {
        let mut cand: Option<(usize, f32, ChipCurve)> = None;
        for j in 1..bps.len() {
            let mut b = bps.clone();
            b.remove(j);
            let c = curve_of(&b);
            let e = fit_error(&target, &c);
            if cand.as_ref().is_none_or(|(_, ce, _)| e < *ce) {
                cand = Some((j, e, c));
            }
        }
        let Some((j, e, c)) = cand else { break };
        if bps.len() > max_segments || e <= free {
            bps.remove(j);
            best = c;
            err = err.max(e);
        } else {
            break;
        }
    }
    let _ = err;
    lift_to_target(&target, &mut best);
    Ok(best)
}

/// Largest shortfall (PWM counts) of the chip curve below the request that [`fit`] leaves alone
/// (1 % of full scale: register rounding).
const MAX_UNDERSHOOT: f32 = 2.55;

/// Raises segments that run below the requested curve (after breakpoints had to be dropped, or
/// where a slope was limited), so the chip never cools noticeably less than asked. A segment that
/// would pass 255 before the next one starts gets a flatter slope instead.
fn lift_to_target(target: &Target, curve: &mut ChipCurve) {
    let tf = curve.tfull as i32;
    for _ in 0..3 {
        for j in 0..curve.segs.len() {
            let end = curve.segs.get(j + 1).map(|s| s.start as i32).unwrap_or(tf);
            let s = curve.segs[j];
            let at = |t: i32| s.pwm as f32 + s.slope8 as f32 / 8.0 * (t - s.start as i32) as f32;
            let short = (s.start as i32..end).map(|t| target.at(t as f32) - at(t)).fold(0.0, f32::max);
            if short <= MAX_UNDERSHOOT {
                continue;
            }
            let pwm = (s.pwm as f32 + short.ceil()).min(255.0);
            let span = end - 1 - s.start as i32;
            let mut slope8 = s.slope8 as i32;
            if span > 0 {
                slope8 = slope8.min(((255.0 - pwm) * 8.0 / span as f32).floor() as i32);
            }
            curve.segs[j] = Segment { start: s.start, pwm: pwm as u8, slope8: slope8.max(0) as i16 };
        }
    }
}

/// Writes `curve` into a copy of `base` (the chip's current image). Bits we don't own and the data of
/// unused extra vectors are kept; unused extra vectors are disabled. Slopes must be non-negative.
pub fn encode(curve: &ChipCurve, base: &CurveImage) -> CurveImage {
    let mut img = *base;
    let main = curve.segs.first().copied().unwrap_or(Segment { start: 0, pwm: 255, slope8: 0 });
    let s8 = |s: &Segment| s.slope8.clamp(0, MAX_SLOPE8) as u16;
    img.sg[SG_TOFF] = curve.toff;
    img.sg[SG_TSTART] = main.start;
    img.sg[SG_TFULL] = curve.tfull;
    img.sg[SG_PWM] = main.pwm;
    img.sg[SG_SLOPE] = (s8(&main) >> 3) as u8;
    if let Some(dt) = curve.dt {
        img.sg[SG_DT] = (base.sg[SG_DT] & !DT_MASK) | dt.min(DT_MASK);
    }
    img.bank2[B2_MAIN_FRAC] = (base.bank2[B2_MAIN_FRAC] & !0x70) | (((s8(&main) & 7) as u8) << 4);
    let mut enable3 = 0u8;
    for z in 0..MAX_EV {
        let ev = curve.segs.get(z + 1);
        if z < 3 {
            let o = 4 * z;
            match ev {
                Some(s) => {
                    img.bank2[o] = s.start;
                    img.bank2[o + 1] = (s8(s) >> 3) as u8;
                    img.bank2[o + 3] = s.pwm;
                    img.bank2[o + 2] = (base.bank2[o + 2] & 0x70) | 0x80 | (s8(s) & 7) as u8;
                }
                None => img.bank2[o + 2] = base.bank2[o + 2] & !0x80,
            }
        } else {
            let d = z - 3;
            if let Some(s) = ev {
                img.bank3[2 * d] = s.start;
                img.bank3[2 * d + 1] = (s8(s) >> 3) as u8;
                img.bank3_pwm[d] = s.pwm;
                let r = 13 + d / 2;
                let nib = (s8(s) & 7) as u8; // sign bit (3) clear: positive slope
                img.bank3[r] =
                    if d % 2 == 0 { (img.bank3[r] & 0xF0) | nib } else { (img.bank3[r] & 0x0F) | (nib << 4) };
                enable3 |= 1 << d;
            }
        }
    }
    img.bank3[B3_ENABLE] = (base.bank3[B3_ENABLE] & 0xC0) | enable3;
    img
}

/// Reads the curve the image describes (enabled extra vectors in zone order).
pub fn decode(img: &CurveImage) -> ChipCurve {
    let slope = |int: u8, frac: u8, neg: bool| -> i16 {
        let v = (int as i16) * 8 + (frac & 7) as i16;
        if neg { -v } else { v }
    };
    let mut segs = vec![Segment {
        start: img.sg[SG_TSTART],
        pwm: img.sg[SG_PWM],
        slope8: slope(img.sg[SG_SLOPE] & 0x7F, img.bank2[B2_MAIN_FRAC] >> 4, false),
    }];
    for z in 0..MAX_EV {
        if z < 3 {
            let o = 4 * z;
            let f = img.bank2[o + 2];
            if f & 0x80 != 0 {
                segs.push(Segment {
                    start: img.bank2[o],
                    pwm: img.bank2[o + 3],
                    slope8: slope(img.bank2[o + 1] & 0x7F, f, f & 0x08 != 0),
                });
            }
        } else {
            let d = z - 3;
            if img.bank3[B3_ENABLE] & (1 << d) != 0 {
                let r = img.bank3[13 + d / 2];
                let nib = if d % 2 == 0 { r & 0x0F } else { r >> 4 };
                segs.push(Segment {
                    start: img.bank3[2 * d],
                    pwm: img.bank3_pwm[d],
                    slope8: slope(img.bank3[2 * d + 1] & 0x7F, nib, nib & 0x08 != 0),
                });
            }
        }
    }
    ChipCurve { toff: img.sg[SG_TOFF], segs, tfull: img.sg[SG_TFULL], dt: None }
}

/// GIGABYTE's "Normal" CPU preset (temps 0,20,30,40,50,60,65,70 → 25,25,51,60,65,68,80,100 %),
/// used when a fan must be handed back to the chip but no valid firmware curve is known.
pub fn safe_default() -> ChipCurve {
    let pts = [
        (0.0, 25.0),
        (20.0, 25.0),
        (30.0, 51.0),
        (40.0, 60.0),
        (50.0, 65.0),
        (60.0, 68.0),
        (65.0, 80.0),
        (70.0, 100.0),
    ];
    fit(&pts, 70.0, MAX_SEGMENTS).expect("static preset")
}

/// Temperature input used by [`safe_default`] (CPU on GIGABYTE AM4/AM5 boards).
pub const SAFE_DEFAULT_INPUT: u8 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    fn max_dev(curve: &ChipCurve, pts: &[(f32, f32)], upto: u8) -> f32 {
        let t = Target { pts: pts.iter().map(|&(t, d)| (t, pct_to_pwm(d))).collect() };
        (0..upto as i32).map(|x| (curve.eval(x as f32) - t.at(x as f32)).abs()).fold(0.0, f32::max)
    }

    #[test]
    fn fits_gcc_preset_exactly() {
        let pts = [
            (0.0, 25.0),
            (20.0, 25.0),
            (30.0, 51.0),
            (40.0, 60.0),
            (50.0, 65.0),
            (60.0, 68.0),
            (65.0, 80.0),
            (70.0, 100.0),
        ];
        let c = fit(&pts, 70.0, MAX_SEGMENTS).unwrap();
        assert_eq!(c.tfull, 70);
        assert!(c.segs.len() <= MAX_SEGMENTS);
        assert!(max_dev(&c, &pts, 70) <= 1.5, "deviation {}", max_dev(&c, &pts, 70));
        assert_eq!(c.eval(75.0), 255.0);
        // Starts strictly ascending.
        assert!(c.segs.windows(2).all(|w| w[0].start < w[1].start));
    }

    #[test]
    fn too_many_points_are_reduced() {
        let pts: Vec<(f32, f32)> =
            (0..20).map(|i| (20.0 + i as f32 * 3.0, 20.0 + (i as f32 * 0.3).powf(2.0) * 2.0)).collect();
        let c = fit(&pts, 90.0, MAX_SEGMENTS).unwrap();
        assert!(c.segs.len() <= MAX_SEGMENTS);
        assert!(max_dev(&c, &pts, 90) < 10.0, "deviation {}", max_dev(&c, &pts, 90));
        let one = fit(&pts, 90.0, 1).unwrap();
        assert_eq!(one.segs.len(), 1);
    }

    #[test]
    fn never_overflows_and_is_monotonic() {
        let pts = [(30.0, 30.0), (50.0, 100.0), (40.0, 90.0), (45.0, 10.0)];
        let c = fit(&pts, 85.0, MAX_SEGMENTS).unwrap();
        let mut prev = 0.0;
        for t in 0..=100 {
            let v = c.eval(t as f32);
            assert!(v >= prev - 1.5, "not monotonic at {t}");
            prev = v;
        }
        for s in &c.segs {
            assert!(s.slope8 >= 0);
        }
        // Raw arithmetic of every segment stays within 8 bits until the next segment / Tfull.
        for (i, s) in c.segs.iter().enumerate() {
            let end = c.segs.get(i + 1).map(|n| n.start).unwrap_or(c.tfull) as i32;
            let last = s.pwm as i32 + (s.slope8 as i32 * (end - 1 - s.start as i32)) / 8;
            assert!(last <= 255);
        }
    }

    #[test]
    fn flat_curve_has_failsafe_ramp() {
        let c = flat(30.0, 85.0);
        assert_eq!(c.tfull, 85);
        assert_eq!(c.segs.len(), 1);
        assert_eq!(c.segs[0].pwm, 77);
        assert_eq!(c.eval(40.0), 77.0);
        assert_eq!(c.eval(85.0), 255.0);
        assert!(c.eval(84.0) <= 255.0 && c.eval(84.0) > 200.0);
        assert_eq!(c.segs[0].slope8, FLAT_RAMP_SLOPE8);
        // Only StartPWM and Tstart change with the duty (cheap 1 Hz updates).
        let d = flat(31.0, 85.0);
        assert_eq!(d.segs[0].slope8, c.segs[0].slope8);
        let full = flat(100.0, 85.0);
        assert_eq!(full.eval(0.0), 255.0);
        let zero = flat(0.0, 85.0);
        assert_eq!(zero.eval(50.0), 0.0);
        assert!(zero.segs[0].start >= 85 - 16);
    }

    #[test]
    fn encode_decode_roundtrip() {
        let pts = [
            (10.0, 20.0),
            (30.0, 30.0),
            (35.0, 33.0),
            (40.0, 40.0),
            (45.0, 47.0),
            (50.0, 55.0),
            (55.0, 60.0),
            (60.0, 70.0),
            (65.0, 76.0),
            (70.0, 85.0),
            (75.0, 95.0),
        ];
        let c = fit(&pts, 80.0, MAX_SEGMENTS).unwrap();
        assert!(c.segs.len() >= 9, "uses bank 3 zones: {}", c.segs.len());
        let mut base = CurveImage::default();
        base.bank2[2] = 0x40; // foreign bit 6 must be preserved
        base.bank2[B2_MAIN_FRAC] = 0x0F;
        base.bank3[B3_ENABLE] = 0xC0;
        base.sg[SG_DT] = 3;
        let img = encode(&c, &base);
        assert_eq!(decode(&img), c);
        assert_eq!(img.bank2[2] & 0x40, 0x40);
        assert_eq!(img.bank2[B2_MAIN_FRAC] & 0x0F, 0x0F);
        assert_eq!(img.bank3[B3_ENABLE] & 0xC0, 0xC0);
        assert_eq!(img.sg[SG_DT], 3, "ΔT untouched");
        // Fewer segments disable the rest but keep their data.
        let small = fit(&[(30.0, 30.0), (60.0, 60.0)], 80.0, MAX_SEGMENTS).unwrap();
        let img2 = encode(&small, &img);
        assert_eq!(decode(&img2), small);
        assert_eq!(img2.bank3[0], img.bank3[0]);
        assert_eq!(img2.bank3[B3_ENABLE] & 0x3F, 0);
    }

    /// A curve's ΔT goes to bank-0 register 0x65+8n (low bits only).
    #[test]
    fn delta_t_is_encoded_when_set() {
        let mut base = CurveImage::default();
        base.sg[SG_DT] = 0x81;
        let mut c = fit(&[(30.0, 30.0), (60.0, 60.0)], 80.0, MAX_SEGMENTS).unwrap();
        assert_eq!(encode(&c, &base).sg[SG_DT], 0x81, "None keeps the firmware's value");
        c.dt = Some(2);
        assert_eq!(encode(&c, &base).sg[SG_DT], 0x82);
        c.dt = Some(9);
        assert_eq!(encode(&c, &base).sg[SG_DT], 0x83, "clamped to the register's 0..=3");
        let mut d = decode(&encode(&c, &base));
        assert_eq!(d.dt, None);
        d.dt = c.dt;
        assert_eq!(d, c);
    }

    #[test]
    fn register_addresses() {
        assert_eq!([0, 1, 2, 3, 4, 5].map(ev_pwm_base), [0x70, 0x76, 0x80, 0x86, 0x90, 0x96]);
        assert_eq!(ev_base(5), 0x60);
        assert!(safe_default().is_safe());
    }

    // ---- independent tester: audit FR-HW-001 ----

    /// The requested polyline as the engine means it (non-decreasing envelope, points sorted, a vertical
    /// step taking the upper duty), in PWM counts, built without `fit`'s own helpers.
    fn requested(points: &[(f32, f32)], t: f32) -> f32 {
        let mut p: Vec<(f32, f32)> = points.to_vec();
        p.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut top = 0.0f32;
        for q in &mut p {
            top = top.max(q.1);
            q.1 = top;
        }
        // Last point at or below t (so a step at one temperature counts with its upper duty), first
        // point above it.
        let duty = match (p.iter().rev().find(|q| q.0 <= t), p.iter().find(|q| q.0 > t)) {
            (None, _) => p[0].1,
            (Some(lo), None) => lo.1,
            (Some(lo), Some(hi)) => lo.1 + (hi.1 - lo.1) * (t - lo.0) / (hi.0 - lo.0),
        };
        duty.clamp(0.0, 100.0) * 2.55
    }

    /// Largest shortfall (PWM counts) of the chip curve below the request at the whole degrees the chip
    /// works in, below Tfull.
    fn undershoot(c: &ChipCurve, points: &[(f32, f32)]) -> (f32, i32) {
        (0..c.tfull as i32)
            .map(|t| (requested(points, t as f32) - c.eval(t as f32), t))
            .fold((0.0, -1), |a, b| if b.0 > a.0 { b } else { a })
    }

    /// FR-HW-001 (tester): steps between points less than 1 °C apart, or at one temperature (in either
    /// order), reach the chip at their full height; the audit's example (20 % to 50 °C, 80 % from
    /// 50.4 °C) gives 80 % from 51 °C, not 40 %.
    #[test]
    fn close_and_shared_temperature_steps_reach_the_chip() {
        let audit = [(30.0, 20.0), (50.0, 20.0), (50.4, 80.0), (90.0, 100.0)];
        let c = fit(&audit, 95.0, MAX_SEGMENTS).unwrap();
        for t in [51.0, 60.0, 70.0] {
            assert!(c.duty_at(t) >= 79.0, "{t} °C: {:.1} % ({c:?})", c.duty_at(t));
        }
        assert!(c.duty_at(45.0) >= 19.0 && c.duty_at(45.0) <= 25.0, "below the step: {:.1} %", c.duty_at(45.0));
        let cases: [&[(f32, f32)]; 6] = [
            &[(40.0, 20.0), (50.0, 20.0), (50.0, 80.0), (70.0, 90.0)],
            &[(40.0, 20.0), (50.0, 80.0), (50.0, 20.0), (70.0, 90.0)],
            &[(40.0, 25.0), (60.2, 25.0), (60.7, 95.0)],
            &[(35.5, 30.0), (35.6, 60.0), (35.7, 30.0), (36.1, 90.0), (80.0, 90.0)],
            &[(20.0, 10.0), (20.0, 50.0), (20.0, 100.0)],
            &[(55.9, 0.0), (56.0, 70.0), (56.95, 75.0), (57.0, 100.0)],
        ];
        for pts in cases {
            let c = fit(pts, 90.0, MAX_SEGMENTS).unwrap();
            c.check_programmable().unwrap();
            let (u, at) = undershoot(&c, pts);
            assert!(u <= MAX_UNDERSHOOT + 1.0, "{pts:?}: {u:.1} counts short at {at} °C: {c:?}");
        }
        // Order of the points does not matter.
        let a = fit(&[(50.0, 80.0), (40.0, 20.0), (50.0, 20.0)], 90.0, MAX_SEGMENTS).unwrap();
        let b = fit(&[(40.0, 20.0), (50.0, 20.0), (50.0, 80.0)], 90.0, MAX_SEGMENTS).unwrap();
        assert_eq!(a, b);
    }

    /// FR-HW-001 (tester): over thousands of reproducible random curves with sub-degree spacing,
    /// repeated temperatures, out-of-range temperatures and any number of points, the fitted chip curve
    /// is programmable, never wraps past 255 inside a segment, and never runs noticeably below the
    /// request (1 % register rounding plus one count).
    #[test]
    fn random_close_point_curves_never_undershoot() {
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f32 / (1u64 << 53) as f32
        };
        let mut worst = (0.0f32, Vec::new(), 0i32);
        for _ in 0..6000 {
            let n = 1 + (rnd() * 16.0) as usize;
            let mut pts: Vec<(f32, f32)> = Vec::with_capacity(n);
            let mut t = -5.0 + rnd() * 70.0;
            for _ in 0..n {
                // Mostly sub-degree steps, some repeats, some wide gaps.
                let r = rnd();
                t += if r < 0.25 {
                    0.0
                } else if r < 0.7 {
                    rnd() * 0.99
                } else {
                    rnd() * 20.0
                };
                pts.push(((t * 10.0).round() / 10.0, (rnd() * 100.0).round()));
            }
            if rnd() < 0.5 {
                pts.reverse();
            }
            let full = 40.0 + rnd() * 60.0;
            let c = fit(&pts, full, MAX_SEGMENTS).unwrap();
            c.check_programmable().unwrap_or_else(|e| panic!("{pts:?} @ {full}: {e}"));
            for (i, s) in c.segs.iter().enumerate() {
                let end = c.segs.get(i + 1).map(|n| n.start).unwrap_or(c.tfull) as i32;
                let last = s.pwm as i32 + (s.slope8 as i32 * (end - 1 - s.start as i32)) / 8;
                assert!(last <= 255, "{pts:?} @ {full}: segment {i} wraps: {c:?}");
            }
            let (u, at) = undershoot(&c, &pts);
            if u > worst.0 {
                worst = (u, pts.clone(), at);
            }
        }
        println!("worst undershoot over 6000 curves: {:.2} counts ({:.2} %)", worst.0, worst.0 / 2.55);
        assert!(
            worst.0 <= MAX_UNDERSHOOT + 1.0,
            "worst undershoot {:.1} counts ({:.1} %) at {} °C for {:?}",
            worst.0,
            worst.0 / 2.55,
            worst.2,
            worst.1
        );
    }
}
