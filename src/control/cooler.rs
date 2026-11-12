//! Cooling-failure watch for CPU / pump headers without a tach signal.
//!
//! A stopped pump or CPU fan on a header that reports no RPM can't be seen directly, so the watch
//! judges the temperature instead. It raises `ErrorCode::CoolingIneffective` when either holds:
//!
//! * **Near critical at full drive:** Tctl (median of 3) within
//!   [`NEAR_CRITICAL_MARGIN_C`](super::limits::NEAR_CRITICAL_MARGIN_C) of the critical temperature
//!   for [`NEAR_HOLD`] while the header runs at ≥ [`HIGH_DUTY`] % (commanded, or the BIOS curve's
//!   estimate). A working cooler at full speed stays clear of that on a Zen 4. The margin is
//!   defined, documented and tuned in [`crate::control::limits`].
//! * **Hot at low power:** Tctl ≥ [`IDLE_HOT_C`] (never above critical − 5 °C) for [`IDLE_HOLD`]
//!   while the CPU package draws < [`LOW_POWER_W`] W. Heat without load means the heat isn't removed.
//!
//! The alarm clears after Tctl stayed [`CLEAR_MARGIN_C`] below both thresholds for [`CLEAR_HOLD`].
//! Pure logic (no hardware access): the engine feeds it once per tick.

use std::time::{Duration, Instant};

pub const NEAR_HOLD: Duration = Duration::from_secs(60);
pub const HIGH_DUTY: f32 = 90.0;
pub const IDLE_HOT_C: f32 = 85.0;
pub const LOW_POWER_W: f32 = 30.0;
pub const IDLE_HOLD: Duration = Duration::from_secs(30);
pub const CLEAR_MARGIN_C: f32 = 5.0;
pub const CLEAR_HOLD: Duration = Duration::from_secs(60);

/// One tick's inputs.
#[derive(Clone, Copy, Debug)]
pub struct CoolerInput {
    /// Median-filtered CPU temperature.
    pub tctl: Option<f32>,
    /// CPU package power (W), if known.
    pub package_w: Option<f32>,
    /// Highest effective duty of the watched headers (commanded or BIOS estimate), if known.
    pub duty: Option<f32>,
    /// `General::cpu_critical`.
    pub cpu_critical: f32,
}

/// Why the alarm is up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    NearCritical,
    HotAtLowPower,
}

#[derive(Clone, Debug, Default)]
pub struct CoolerWatch {
    pub near_since: Option<Instant>,
    pub idle_hot_since: Option<Instant>,
    pub cool_since: Option<Instant>,
    pub active: Option<Trigger>,
}

impl CoolerWatch {
    pub fn near_threshold(cpu_critical: f32) -> f32 {
        super::limits::near_critical_c(cpu_critical)
    }

    pub fn idle_threshold(cpu_critical: f32) -> f32 {
        IDLE_HOT_C.min(cpu_critical - 5.0)
    }

    /// Advances the watch; returns the active trigger (also while it holds).
    pub fn update(&mut self, now: Instant, i: &CoolerInput) -> Option<Trigger> {
        let Some(t) = i.tctl.filter(|t| t.is_finite()) else {
            // Unknown temperature: no new evidence either way (the sensor-loss path handles it).
            self.near_since = None;
            self.idle_hot_since = None;
            self.cool_since = None;
            return self.active;
        };
        let near_t = Self::near_threshold(i.cpu_critical);
        let idle_t = Self::idle_threshold(i.cpu_critical);
        let held = |since: &mut Option<Instant>, cond: bool, hold: Duration| -> bool {
            if cond {
                now.duration_since(*since.get_or_insert(now)) >= hold
            } else {
                *since = None;
                false
            }
        };
        let near = held(&mut self.near_since, t >= near_t && i.duty.is_some_and(|d| d >= HIGH_DUTY), NEAR_HOLD);
        let idle =
            held(&mut self.idle_hot_since, t >= idle_t && i.package_w.is_some_and(|p| p < LOW_POWER_W), IDLE_HOLD);
        if near {
            self.active = Some(Trigger::NearCritical);
        } else if idle && self.active.is_none() {
            self.active = Some(Trigger::HotAtLowPower);
        }
        if self.active.is_some() {
            let cool = t < near_t.min(idle_t) - CLEAR_MARGIN_C;
            if held(&mut self.cool_since, cool, CLEAR_HOLD) {
                self.active = None;
                self.cool_since = None;
            }
        }
        self.active
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::sim_plant::{Rng, SensorModel, SimPlant, cycle, idle, render};

    /// Runs the plant with the CPU fan at `duty` for `secs` at 1 s polls; returns the first second at
    /// which the watch fired.
    fn run(
        p: &mut SimPlant,
        w: &mut CoolerWatch,
        secs: usize,
        duty: f64,
        load: impl Fn(f64, &mut Rng) -> (f64, f64),
    ) -> Option<(usize, Trigger)> {
        let t0 = Instant::now();
        let mut r = Rng(7);
        let mut hist = std::collections::VecDeque::new();
        for s in 0..secs {
            let (pc, pg) = load(p.t, &mut r);
            p.advance(1.0, pc, pg, [duty, 60.0, 60.0]);
            let rd = p.sense(pg);
            hist.push_back(rd.tctl as f32);
            if hist.len() > 3 {
                hist.pop_front();
            }
            let mut v: Vec<f32> = hist.iter().copied().collect();
            v.sort_by(f32::total_cmp);
            let med = v[v.len() / 2];
            let inp = CoolerInput {
                tctl: Some(med),
                package_w: Some(rd.p_cpu as f32),
                duty: Some(duty as f32),
                cpu_critical: 90.0,
            };
            if let Some(tr) = w.update(t0 + Duration::from_secs(s as u64), &inp) {
                return Some((s, tr));
            }
        }
        None
    }

    /// No false alarm across 3 h of the mixed workload even with the fan pinned at 100 % (the
    /// near-critical rule's worst case), nor under a sustained render.
    #[test]
    fn no_alarm_with_working_cooler() {
        let mut p = SimPlant::new(SensorModel::default());
        let mut w = CoolerWatch::default();
        assert_eq!(
            run(&mut p, &mut w, 3 * 3600, 100.0, |t, r| {
                let l = cycle(t, r, 1.0);
                (l.pc, l.pg)
            }),
            None
        );
        assert_eq!(
            run(&mut p, &mut w, 1800, 100.0, |_, _| {
                let l = render();
                (l.pc, l.pg)
            }),
            None
        );
        assert_eq!(
            run(&mut p, &mut w, 1800, 40.0, |_, r| {
                let l = idle(r, 1.0);
                (l.pc, l.pg)
            }),
            None
        );
    }

    /// A stopped pump (heatsink loses its forced convection): under load the near-critical rule
    /// fires, at idle the hot-at-low-power rule does.
    #[test]
    fn alarm_when_the_pump_stops() {
        let dead = |p: &mut SimPlant| {
            p.g1 = 0.0;
            p.g0 = 0.3;
        };
        let mut p = SimPlant::new(SensorModel::default());
        let mut w = CoolerWatch::default();
        assert_eq!(
            run(&mut p, &mut w, 600, 100.0, |_, _| {
                let l = render();
                (l.pc, l.pg)
            }),
            None
        );
        dead(&mut p);
        let (s, tr) = run(&mut p, &mut w, 1800, 100.0, |_, _| {
            let l = render();
            (l.pc, l.pg)
        })
        .expect("alarm under load");
        assert_eq!(tr, Trigger::NearCritical);
        assert!(s < 900, "fired after {s} s");

        let mut p = SimPlant::new(SensorModel::default());
        let mut w = CoolerWatch::default();
        dead(&mut p);
        let (_, tr) = run(&mut p, &mut w, 3600, 50.0, |_, r| {
            let l = idle(r, 1.0);
            (l.pc, l.pg)
        })
        .expect("alarm at idle");
        assert_eq!(tr, Trigger::HotAtLowPower);
    }

    #[test]
    fn holds_then_clears_with_hysteresis() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        // Temperatures follow the tunable margin (limits.rs): 82 °C threshold at the documented 8 °C.
        let near = CoolerWatch::near_threshold(90.0);
        let clear_below = near.min(CoolerWatch::idle_threshold(90.0)) - CLEAR_MARGIN_C;
        let hot = CoolerInput { tctl: Some(near + 2.0), package_w: Some(80.0), duty: Some(100.0), cpu_critical: 90.0 };
        let mut w = CoolerWatch::default();
        assert_eq!(w.update(at(0), &hot), None);
        assert_eq!(w.update(at(59), &hot), None, "needs 60 s");
        assert_eq!(w.update(at(60), &hot), Some(Trigger::NearCritical));
        // Low duty: not the near-critical rule.
        let mut w2 = CoolerWatch::default();
        let slow = CoolerInput { duty: Some(50.0), ..hot };
        assert_eq!(w2.update(at(0), &slow), None);
        assert_eq!(w2.update(at(120), &slow), None);
        // Unknown temperature keeps the alarm.
        assert_eq!(w.update(at(61), &CoolerInput { tctl: None, ..hot }), Some(Trigger::NearCritical));
        // Not CLEAR_MARGIN_C below the thresholds (79 °C vs 82 °C at the documented margin): stays.
        let warm = CoolerInput { tctl: Some(clear_below + 2.0), ..hot };
        assert_eq!(w.update(at(62), &warm), Some(Trigger::NearCritical));
        assert_eq!(w.update(at(200), &warm), Some(Trigger::NearCritical));
        let cool = CoolerInput { tctl: Some(clear_below - 7.0), ..hot };
        assert_eq!(w.update(at(201), &cool), Some(Trigger::NearCritical));
        assert_eq!(w.update(at(261), &cool), None, "cleared after 60 s cool");
        // Idle rule: hot at 20 W for 30 s.
        let mut w3 = CoolerWatch::default();
        let idle_hot = CoolerInput { tctl: Some(86.0), package_w: Some(20.0), duty: None, cpu_critical: 92.0 };
        assert_eq!(w3.update(at(0), &idle_hot), None);
        assert_eq!(w3.update(at(30), &idle_hot), Some(Trigger::HotAtLowPower));
        assert_eq!(CoolerWatch::idle_threshold(88.0), 83.0, "never above critical - 5");
    }
}
