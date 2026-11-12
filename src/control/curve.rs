//! Piecewise-linear fan curves with smoothing and hysteresis.

use crate::config::CurveConfig;
use crate::util::ema_alpha;

/// Linear interpolation over points sorted by x; clamps outside the range.
pub fn interpolate(points: &[[f32; 2]], x: f32) -> f32 {
    match points {
        [] => 0.0,
        [only] => only[1],
        _ => {
            if x <= points[0][0] {
                return points[0][1];
            }
            for w in points.windows(2) {
                if x <= w[1][0] {
                    let span = (w[1][0] - w[0][0]).max(1e-6);
                    let t = (x - w[0][0]) / span;
                    return w[0][1] + t * (w[1][1] - w[0][1]);
                }
            }
            points[points.len() - 1][1]
        }
    }
}

/// Smallest rise (°C) that moves the applied temperature (deadband against noise; B15).
pub const RISE_DEADBAND: f32 = 0.5;

#[derive(Clone, Debug, Default)]
pub struct CurveState {
    raw: [f32; 3],
    raw_n: usize,
    smoothed: Option<f32>,
    applied: Option<f32>,
}

impl CurveState {
    /// Returns (duty %, effective temperature) for this tick.
    ///
    /// A median of the last three readings removes one-sample sensor spikes (Zen 4 Tctl), an EMA
    /// with `response_s` smooths, and a symmetric deadband around the applied temperature (rises
    /// need at least 0.5 °C, falls `hysteresis`) stops noise from walking the fan up.
    pub fn step(&mut self, cfg: &CurveConfig, temp: f32, dt: f32) -> (f32, f32) {
        if !temp.is_finite() {
            let t = self.applied.unwrap_or(100.0);
            return (interpolate(&cfg.points, t), t);
        }
        self.raw = [self.raw[1], self.raw[2], temp];
        self.raw_n = (self.raw_n + 1).min(3);
        let x = if self.raw_n >= 3 {
            let [a, b, c] = self.raw;
            a.max(b).min(a.min(b).max(c))
        } else {
            temp
        };
        let a = ema_alpha(dt, cfg.response_s.clamp(0.0, 60.0));
        let s = match self.smoothed {
            None => x,
            Some(prev) => prev + (x - prev) * a,
        };
        self.smoothed = Some(s);
        let applied = match self.applied {
            Some(prev) if s >= prev && s - prev < RISE_DEADBAND => prev,
            Some(prev) if s < prev && prev - s < cfg.hysteresis.max(0.0) => prev,
            _ => s,
        };
        self.applied = Some(applied);
        (interpolate(&cfg.points, applied), applied)
    }

    pub fn reset(&mut self) {
        *self = CurveState::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_and_clamps() {
        let p = [[40.0, 30.0], [60.0, 50.0], [80.0, 100.0]];
        assert_eq!(interpolate(&p, 20.0), 30.0);
        assert_eq!(interpolate(&p, 50.0), 40.0);
        assert_eq!(interpolate(&p, 70.0), 75.0);
        assert_eq!(interpolate(&p, 95.0), 100.0);
    }

    #[test]
    fn hysteresis_holds_on_small_drops() {
        // Premise update: readings now pass a median-of-3 spike filter, so each level is held for
        // three samples (as a real sensor would) and rises need 0.5 °C (symmetric deadband).
        let cfg = CurveConfig {
            points: vec![[40.0, 30.0], [80.0, 100.0]],
            hysteresis: 3.0,
            response_s: 0.0,
            ..Default::default()
        };
        let mut s = CurveState::default();
        let mut feed = |t: f32| (0..3).map(|_| s.step(&cfg, t, 1.0).1).last().unwrap();
        assert_eq!(feed(60.0), 60.0);
        assert_eq!(feed(58.0), 60.0, "2 degree drop is ignored");
        assert_eq!(feed(56.5), 56.5, "3.5 degree drop applies");
        assert_eq!(feed(57.0), 57.0, "rises of 0.5 degrees apply");
        assert_eq!(feed(57.3), 57.0, "smaller rises are noise");
    }

    #[test]
    fn single_sample_spike_is_ignored() {
        let cfg = CurveConfig {
            points: vec![[40.0, 30.0], [80.0, 100.0]],
            hysteresis: 2.0,
            response_s: 0.0,
            ..Default::default()
        };
        let mut s = CurveState::default();
        for _ in 0..5 {
            s.step(&cfg, 60.0, 1.0);
        }
        assert_eq!(s.step(&cfg, 68.0, 1.0).1, 60.0, "one-sample +8 degree spike");
        assert_eq!(s.step(&cfg, 60.0, 1.0).1, 60.0);
    }

    #[test]
    fn noise_does_not_ratchet_up() {
        let cfg = CurveConfig {
            points: vec![[40.0, 30.0], [80.0, 100.0]],
            hysteresis: 2.0,
            response_s: 3.0,
            ..Default::default()
        };
        let mut s = CurveState::default();
        let mut r = crate::hw::sim::Rng::new(3);
        let mut max_applied: f32 = 0.0;
        for _ in 0..3600 {
            let t = 60.0 + r.range(-1.0, 1.0);
            max_applied = max_applied.max(s.step(&cfg, t, 1.0).1);
        }
        assert!(max_applied < 61.0, "{max_applied}");
    }
}
