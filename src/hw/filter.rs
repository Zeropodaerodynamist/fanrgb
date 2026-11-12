//! Plausibility filter for temperature readings.
//!
//! Rejects values no PC sensor can report (ITE's 0x80 = -128 °C "no sensor", 127 °C, ≤ 0 °C,
//! NaN) and single-sample jumps of more than [`MAX_JUMP`] °C. A jump that persists for a second
//! sample is accepted, so a real step is delayed by one poll at most.

use std::time::{Duration, Instant};

use crate::error::ErrorCode;

/// Largest believable change between two consecutive samples.
pub const MAX_JUMP: f32 = 30.0;
/// After this many missing/rejected samples the reference value is forgotten.
const RESET_AFTER: u32 = 5;
const LOG_EVERY: Duration = Duration::from_secs(600);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Verdict {
    Ok(f32),
    /// Outside the physically possible range.
    OutOfRange(f32),
    /// Jumped too far from the previous sample.
    Jump {
        from: f32,
        to: f32,
    },
    Missing,
}

#[derive(Clone, Debug, Default)]
pub struct TempFilter {
    last: Option<f32>,
    pending: Option<f32>,
    misses: u32,
    last_log: Option<Instant>,
}

pub fn in_range(v: f32) -> bool {
    v.is_finite() && v > 0.0 && v < 127.0
}

impl TempFilter {
    pub fn check(&mut self, v: Option<f32>) -> Verdict {
        let Some(v) = v else {
            self.miss();
            return Verdict::Missing;
        };
        if !in_range(v) {
            self.miss();
            return Verdict::OutOfRange(v);
        }
        if let Some(l) = self.last
            && (v - l).abs() > MAX_JUMP
        {
            if self.pending.is_some_and(|p| (v - p).abs() <= MAX_JUMP / 3.0) {
                self.accept(v);
                return Verdict::Ok(v);
            }
            self.pending = Some(v);
            self.miss();
            return Verdict::Jump { from: l, to: v };
        }
        self.accept(v);
        Verdict::Ok(v)
    }

    fn accept(&mut self, v: f32) {
        self.last = Some(v);
        self.pending = None;
        self.misses = 0;
    }

    fn miss(&mut self) {
        self.misses += 1;
        if self.misses >= RESET_AFTER {
            self.last = None;
        }
    }

    /// Filters `v` for sensor `id`, logging rejected values (rate-limited per sensor).
    pub fn apply(&mut self, id: &str, v: Option<f32>) -> Option<f32> {
        let verdict = self.check(v);
        let msg = match verdict {
            Verdict::Ok(v) => return Some(v),
            Verdict::Missing => return None,
            Verdict::OutOfRange(v) => format!("{id}: {v:.1} °C is outside the possible range; ignored"),
            Verdict::Jump { from, to } => format!("{id}: jumped from {from:.1} to {to:.1} °C in one sample; ignored"),
        };
        if self.last_log.is_none_or(|t| t.elapsed() >= LOG_EVERY) {
            self.last_log = Some(Instant::now());
            log::warn!("[{}] {msg}", ErrorCode::ImplausibleReading);
        }
        None
    }
}

/// A chip input that reads invalid values for this long counts as lost (failsafe/curve input).
pub const INPUT_LOST_AFTER: Duration = Duration::from_secs(5);

/// Tracks whether a chip temperature input keeps reading invalid values (e.g. -128 °C when the
/// thermistor is missing), so a hardware failsafe or curve on it is known not to work.
#[derive(Clone, Debug, Default)]
pub struct InputHealth {
    bad_since: Option<Instant>,
    reported: bool,
}

impl InputHealth {
    /// Feeds one reading. Returns true exactly once when the input has just been invalid for
    /// longer than [`INPUT_LOST_AFTER`] (to raise one alert per episode).
    pub fn update(&mut self, valid: bool, now: Instant) -> bool {
        if valid {
            self.bad_since = None;
            self.reported = false;
            return false;
        }
        self.bad_since.get_or_insert(now);
        if self.lost(now) && !self.reported {
            self.reported = true;
            return true;
        }
        false
    }

    pub fn lost(&self, now: Instant) -> bool {
        self.bad_since.is_some_and(|t| now.saturating_duration_since(t) > INPUT_LOST_AFTER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_ite_sentinels_and_jumps() {
        let mut f = TempFilter::default();
        assert_eq!(f.check(Some(-128.0)), Verdict::OutOfRange(-128.0));
        assert_eq!(f.check(Some(127.0)), Verdict::OutOfRange(127.0));
        assert_eq!(f.check(Some(0.0)), Verdict::OutOfRange(0.0));
        assert_eq!(f.check(Some(f32::NAN)).clone_kind(), "range");
        assert_eq!(f.check(Some(45.0)), Verdict::Ok(45.0));
        assert_eq!(f.check(Some(46.0)), Verdict::Ok(46.0));
        // A single spike is dropped ...
        assert!(matches!(f.check(Some(110.0)), Verdict::Jump { .. }));
        assert_eq!(f.check(Some(47.0)), Verdict::Ok(47.0));
        // ... but a real step that persists is accepted on the second sample.
        assert!(matches!(f.check(Some(85.0)), Verdict::Jump { .. }));
        assert_eq!(f.check(Some(86.0)), Verdict::Ok(86.0));
        assert_eq!(f.apply("x", Some(-128.0)), None);
        assert_eq!(f.apply("x", Some(80.0)), Some(80.0));
    }

    #[test]
    fn forgets_reference_after_gap() {
        let mut f = TempFilter::default();
        f.check(Some(40.0));
        for _ in 0..RESET_AFTER {
            f.check(None);
        }
        assert_eq!(f.check(Some(90.0)), Verdict::Ok(90.0));
    }

    #[test]
    fn input_health_reports_a_lost_input_once() {
        let t0 = Instant::now();
        let mut h = InputHealth::default();
        assert!(!h.update(false, t0));
        assert!(!h.lost(t0 + Duration::from_secs(4)));
        assert!(h.update(false, t0 + Duration::from_secs(6)), "lost after 5 s");
        assert!(!h.update(false, t0 + Duration::from_secs(7)), "reported once");
        assert!(h.lost(t0 + Duration::from_secs(7)));
        assert!(!h.update(true, t0 + Duration::from_secs(8)));
        assert!(!h.lost(t0 + Duration::from_secs(8)));
    }

    impl Verdict {
        fn clone_kind(&self) -> &'static str {
            match self {
                Verdict::OutOfRange(_) => "range",
                Verdict::Jump { .. } => "jump",
                Verdict::Ok(_) => "ok",
                Verdict::Missing => "missing",
            }
        }
    }
}
