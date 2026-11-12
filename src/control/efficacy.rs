//! Cooling-efficacy monitor: notices a slowing or air-locked pump, or lost
//! radiator airflow, on coolers whose pump / fans report no speed, from temperatures and power alone.
//!
//! The observable is the power-normalised rise of the CPU over the board temperature,
//! `R_eff = mean(Tctl − mb.t0) / mean(P_pkg)` (K/W) over 60 s, compared with a baseline learned on
//! this PC per (CPU-fan duty bucket, package-power band). At the same power and fan duty a working
//! cooler gives the same `R_eff` whatever the workload; a failing pump raises it. It complements
//! FRG-E305 (`cooler.rs`), which judges absolute temperatures and catches fast and idle failures.
//!
//! False-positive guards (no alarm on heavy but normal use is the priority; tuned on the sim plant
//! with sustained all-core load, gaming bursts, idle and ambient drift, see the tests):
//! * Only **steady power** is judged: the 15 s and 180 s package-power averages agree within
//!   12 % + 3 W. After a load step the heatsink / coolant needs minutes to settle and Zen 4 Tctl jumps
//!   15-20 °C with boost, so transients never count.
//! * Not while `Tctl − mb.t0` falls faster than 0.4 °C/min (120 s least-squares trend): heat
//!   stored by an earlier, higher load is still leaving (an AIO's coolant takes 5-10 min), which
//!   inflates `R_eff`. A failing cooler never makes the CPU cooler, so this costs no detection.
//! * Only at ≥ [`MIN_POWER_W`] (idle readings are dominated by sensor offsets; a failure at idle is
//!   FRG-E305's hot-at-low-power rule).
//! * A baseline cell needs [`TRUST_S`] of steady data. An untrusted cell borrows the nearest trusted
//!   cell at a *lower* duty (which expects a higher `R_eff`), never a higher one.
//! * Degraded = `R_eff` ≥ [`ALARM_RATIO`] × baseline **and** ≥ [`ALARM_EXCESS_C`] above the
//!   baseline's prediction while the CPU fans run ≥ [`DUTY_GATE`] % (they already do what they can),
//!   for [`HOLD_S`]. (The review's 1.6× on the total `R_eff` would need +28 °C at 87 W, because the
//!   die-to-heatsink part dominates `R_eff`; the absolute excess term keeps 1.3× from reacting to
//!   small offsets at moderate power.)
//! * Learning continues slowly afterwards (30 min time constant), never from a window ≥ 1.15×
//!   baseline and never while the alarm is up, so a degrading cooler is not learned as normal.
//!
//! Pure logic driven by `dt` (no clock reads): the engine feeds it once per tick.

use std::collections::VecDeque;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Averaging window of one `R_eff` evaluation.
pub const WINDOW_S: f32 = 60.0;
/// Below this package power nothing is judged (or learned).
pub const MIN_POWER_W: f32 = 30.0;
const P_FAST_TAU_S: f32 = 15.0;
const P_SLOW_TAU_S: f32 = 180.0;
const STEADY_REL: f32 = 0.12;
const STEADY_ABS_W: f32 = 3.0;
/// Span of the temperature trend (least-squares slope of `Tctl − mb.t0`).
const TREND_S: f32 = 120.0;
/// Falling faster than this (°C/min) at steady power: heat stored by an earlier, higher load is
/// still leaving the heatsink / coolant, which inflates `R_eff`; such windows are not judged.
const TREND_MAX_FALL: f32 = 0.4;
/// Steady seconds a cell needs before it is used as a baseline.
pub const TRUST_S: f32 = 300.0;
const LEARN_TAU_S: f32 = 1800.0;
/// Windows this much above the baseline are not learned.
const LEARN_MAX_RATIO: f32 = 1.15;
pub const ALARM_RATIO: f32 = 1.3;
pub const ALARM_EXCESS_C: f32 = 8.0;
/// CPU-fan duty (%) at or above which a degraded cooler is reported.
pub const DUTY_GATE: f32 = 80.0;
pub const HOLD_S: f32 = 60.0;
const CLEAR_RATIO: f32 = 1.15;
const CLEAR_HOLD_S: f32 = 120.0;
/// With nothing to judge (idle, unsteady), the alarm clears after this long below `COOL_CLEAR_C`.
const COOL_CLEAR_S: f32 = 600.0;
const COOL_CLEAR_C: f32 = 60.0;
/// Learned baseline saved at most this often (and at shutdown).
const SAVE_EVERY_S: f32 = 600.0;
/// Duty buckets of 20 % (0-20 … 80-100).
pub const DUTY_BUCKETS: usize = 5;
/// Lower edges of the power bands (W).
pub const POWER_BANDS: [f32; 3] = [MIN_POWER_W, 50.0, 70.0];
const BANDS: usize = POWER_BANDS.len();

/// One tick's inputs.
#[derive(Clone, Copy, Debug, Default)]
pub struct EfficacyInput {
    /// Median-filtered CPU temperature (°C).
    pub tctl: Option<f32>,
    /// Board reference temperature (`mb.t0`, "System 1"), the ambient proxy.
    pub tref: Option<f32>,
    /// CPU package power (W).
    pub power_w: Option<f32>,
    /// Effective duty of the CPU cooling fans now (commanded or BIOS estimate, max over headers).
    pub duty: Option<f32>,
    /// A safety override (critical latch, FRG-E305) forces the CPU chain to 100 %: judge, but never
    /// learn, since the cooler may be the thing that failed.
    pub forced: bool,
}

/// One learned baseline cell.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Cell {
    /// Mean `R_eff` (K/W).
    pub r: f32,
    /// Steady seconds learned.
    pub secs: f32,
}

impl Cell {
    pub fn trusted(&self) -> bool {
        self.secs >= TRUST_S
    }
}

/// The learned baseline (`cooling_baseline.json`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Baseline {
    /// What the cells were learned for (CPU and the cooling headers with their roles). A different
    /// key starts over: a re-wired or re-identified cooler has a different baseline.
    pub key: String,
    /// `[duty bucket][power band]`.
    pub cells: [[Cell; BANDS]; DUTY_BUCKETS],
}

impl Baseline {
    fn sanitize(&mut self) {
        for c in self.cells.iter_mut().flatten() {
            if !(c.r.is_finite() && (0.01..=10.0).contains(&c.r) && c.secs.is_finite() && c.secs >= 0.0) {
                *c = Cell::default();
            }
            c.secs = c.secs.min(1.0e7);
        }
    }

    /// Trusted cells.
    pub fn learned(&self) -> usize {
        self.cells.iter().flatten().filter(|c| c.trusted()).count()
    }
}

/// One `R_eff` evaluation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Eval {
    pub r: f32,
    pub power_w: f32,
    pub duty: f32,
    pub baseline: Option<f32>,
    pub ratio: Option<f32>,
    /// How far the CPU runs above the baseline's prediction at this power (°C).
    pub excess_c: Option<f32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EfficacyState {
    /// Nothing judged recently (idle, changing load, a sensor missing).
    #[default]
    NotJudged,
    /// Judged, but no baseline for this power / duty yet.
    Learning,
    /// Within the learned range.
    Normal,
    /// FRG-E309.
    Degraded,
}

/// What the UI shows (CPU safety card, Diagnostics).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EfficacyView {
    pub state: EfficacyState,
    /// Last evaluation (≤ 2 min old).
    pub last: Option<Eval>,
    /// Trusted baseline cells (of `DUTY_BUCKETS × POWER_BANDS`).
    pub learned: usize,
    /// The alarm can fire (a CPU cooling header without a speed signal is present).
    pub alarm_enabled: bool,
}

impl EfficacyView {
    /// `cpu_cooling_verified`: `Some(true)` = recently judged within the learned range,
    /// `Some(false)` = degraded, `None` = no verdict.
    pub fn verified(&self) -> Option<bool> {
        match self.state {
            EfficacyState::Degraded => Some(false),
            EfficacyState::Normal => Some(true),
            _ => None,
        }
    }

    pub fn summary(&self) -> String {
        let learned = format!("{}/{} baseline cells learned", self.learned, DUTY_BUCKETS * BANDS);
        match (self.state, self.last) {
            (EfficacyState::NotJudged, _) | (_, None) => {
                format!("not judged right now (needs ≥ {MIN_POWER_W:.0} W steady load); {learned}")
            }
            (s, Some(e)) => {
                let base = match (e.baseline, e.ratio) {
                    (Some(b), Some(q)) => format!("baseline {b:.3} K/W, ×{q:.2}"),
                    _ => "no baseline yet".into(),
                };
                format!("{s:?}: R_eff {:.3} K/W at {:.0} W, {:.0} % duty ({base}); {learned}", e.r, e.power_w, e.duty)
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Sample {
    dt: f32,
    rise: f32,
    power: f32,
    duty: f32,
}

pub struct EfficacyMonitor {
    pub baseline: Baseline,
    /// File the baseline persists to (`None`: memory only, e.g. tests).
    path: Option<PathBuf>,
    dirty: bool,
    since_save: f32,
    /// Seconds fed so far.
    clock: f64,
    /// (clock, Tctl − mb.t0) over the last [`TREND_S`].
    trend: VecDeque<(f64, f32)>,
    p_fast: Option<f32>,
    p_slow: f32,
    p_age: f32,
    window: VecDeque<Sample>,
    window_s: f32,
    over_s: f32,
    ok_s: f32,
    quiet_s: f32,
    pub(crate) alarm: bool,
    alarm_enabled: bool,
    last: Option<Eval>,
    last_age: f32,
    /// Evaluations so far (Diagnostics, tests).
    pub(crate) evals: u64,
}

fn bucket(duty: f32) -> usize {
    ((duty / 20.0).floor().max(0.0) as usize).min(DUTY_BUCKETS - 1)
}

fn band(power: f32) -> Option<usize> {
    POWER_BANDS.iter().rposition(|&lo| power >= lo)
}

impl Default for EfficacyMonitor {
    fn default() -> Self {
        EfficacyMonitor {
            baseline: Baseline::default(),
            path: None,
            dirty: false,
            since_save: 0.0,
            clock: 0.0,
            trend: VecDeque::new(),
            p_fast: None,
            p_slow: 0.0,
            p_age: 0.0,
            window: VecDeque::new(),
            window_s: 0.0,
            over_s: 0.0,
            ok_s: 0.0,
            quiet_s: 0.0,
            alarm: false,
            alarm_enabled: false,
            last: None,
            last_age: f32::INFINITY,
            evals: 0,
        }
    }
}

impl EfficacyMonitor {
    /// Loads the baseline from `path` (missing or unreadable: start learning from scratch).
    pub fn load(path: PathBuf) -> Self {
        let mut baseline = match crate::util::fsx::read_to_string_limited(&path, 64 << 10) {
            Ok(t) => serde_json::from_str::<Baseline>(&t).unwrap_or_else(|e| {
                log::warn!("{}: not readable ({e}); cooling baseline relearned", path.display());
                Baseline::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Baseline::default(),
            Err(e) => {
                log::warn!("{}: {e}; cooling baseline relearned", path.display());
                Baseline::default()
            }
        };
        baseline.sanitize();
        EfficacyMonitor { baseline, path: Some(path), ..Default::default() }
    }

    /// Sets what the baseline is learned for; a different key starts over. Never while the alarm is
    /// up: the change waits until it cleared, so a role edit can't silence FRG-E309 (the
    /// caller sets the key every tick).
    pub fn set_key(&mut self, key: &str) {
        if self.baseline.key != key && !self.alarm {
            if !self.baseline.key.is_empty() && self.baseline.learned() > 0 {
                log::info!("cooling hardware or header roles changed: cooling-efficacy baseline relearned");
            }
            self.baseline = Baseline { key: key.to_string(), ..Default::default() };
            self.dirty = true;
            self.reset_dynamics();
            self.alarm = false;
        }
    }

    /// Sleep / resume or a stalled clock: averages and the window start over (the baseline stays).
    pub fn reset_dynamics(&mut self) {
        self.p_fast = None;
        self.trend.clear();
        self.p_age = 0.0;
        self.window.clear();
        self.window_s = 0.0;
        self.over_s = 0.0;
    }

    /// Least-squares slope of `Tctl − mb.t0` over the trend span (°C/min); `None` until it spans 90 s.
    fn trend_per_min(&self) -> Option<f32> {
        let (first, last) = (self.trend.front()?, self.trend.back()?);
        if last.0 - first.0 < 0.75 * TREND_S as f64 {
            return None;
        }
        let n = self.trend.len() as f64;
        let (mx, my) = self.trend.iter().fold((0.0, 0.0), |(a, b), &(x, y)| (a + x / n, b + y as f64 / n));
        let (mut sxy, mut sxx) = (0.0f64, 0.0f64);
        for &(x, y) in &self.trend {
            sxy += (x - mx) * (y as f64 - my);
            sxx += (x - mx) * (x - mx);
        }
        (sxx > 0.0).then(|| (sxy / sxx * 60.0) as f32)
    }

    /// The baseline for a cell: its own when trusted, else the nearest trusted lower-duty cell.
    fn baseline_for(&self, b: usize, p: usize) -> Option<f32> {
        (0..=b).rev().map(|i| self.baseline.cells[i][p]).find(Cell::trusted).map(|c| c.r)
    }

    /// Advances by `dt` seconds. `alarm_enabled`: a degraded result may raise FRG-E309 (false:
    /// judge and learn only). Returns whether the alarm is up.
    pub fn update(&mut self, dt: f32, i: &EfficacyInput, alarm_enabled: bool) -> bool {
        let dt = if dt.is_finite() { dt.clamp(0.0, 5.0) } else { 0.0 };
        self.since_save += dt;
        self.clock += dt as f64;
        self.last_age += dt;
        self.alarm_enabled = alarm_enabled;
        let fin = |v: Option<f32>| v.filter(|x| x.is_finite());
        let (Some(t), Some(tref), Some(p), Some(duty)) = (fin(i.tctl), fin(i.tref), fin(i.power_w), fin(i.duty)) else {
            self.window.clear();
            self.window_s = 0.0;
            self.trend.clear();
            return self.verdict(dt, None, i);
        };
        self.trend.push_back((self.clock, t - tref));
        while self.trend.front().is_some_and(|f| self.clock - f.0 > TREND_S as f64) {
            self.trend.pop_front();
        }
        // Package power averages: "steady" when the short one agrees with the long one.
        match self.p_fast {
            None => {
                self.p_fast = Some(p);
                self.p_slow = p;
                self.p_age = 0.0;
            }
            Some(f) => {
                let a = |tau: f32| 1.0 - (-dt / tau).exp();
                self.p_fast = Some(f + (p - f) * a(P_FAST_TAU_S));
                self.p_slow += (p - self.p_slow) * a(P_SLOW_TAU_S);
                self.p_age += dt;
            }
        }
        let pf = self.p_fast.unwrap_or(p);
        let steady = self.p_age >= P_SLOW_TAU_S
            && (pf - self.p_slow).abs() <= STEADY_REL * self.p_slow + STEADY_ABS_W
            && pf >= MIN_POWER_W;
        if !steady {
            self.window.clear();
            self.window_s = 0.0;
            return self.verdict(dt, None, i);
        }
        self.window.push_back(Sample { dt, rise: t - tref, power: p, duty });
        self.window_s += dt;
        while let Some(f) = self.window.front().copied() {
            if self.window_s - f.dt < WINDOW_S {
                break;
            }
            self.window.pop_front();
            self.window_s -= f.dt;
        }
        if self.window_s < WINDOW_S - 0.5 || self.trend_per_min().is_none_or(|s| s < -TREND_MAX_FALL) {
            return self.verdict(dt, None, i);
        }
        let (mut rise, mut pw, mut dy, mut span) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        for s in &self.window {
            rise += s.rise * s.dt;
            pw += s.power * s.dt;
            dy += s.duty * s.dt;
            span += s.dt;
        }
        let (p_mean, duty_mean) = (pw / span.max(1e-3), dy / span.max(1e-3));
        let Some(pb) = band(p_mean).filter(|_| pw > 0.0) else {
            return self.verdict(dt, None, i);
        };
        let r = rise / pw;
        let b = bucket(duty_mean);
        let base = self.baseline_for(b, pb);
        let eval = Eval {
            r,
            power_w: p_mean,
            duty: duty_mean,
            baseline: base,
            ratio: base.map(|x| r / x),
            excess_c: base.map(|x| (r - x) * p_mean),
        };
        // Learning (never while degraded, never from a window well above the baseline in use).
        if !self.alarm && !i.forced && r.is_finite() && (0.01..=10.0).contains(&r) {
            let cell = &mut self.baseline.cells[b][pb];
            if !cell.trusted() {
                if base.is_none_or(|x| r <= x * LEARN_MAX_RATIO) {
                    cell.r = if cell.secs <= 0.0 { r } else { cell.r + (r - cell.r) * dt / (cell.secs + dt) };
                    cell.secs += dt;
                    self.dirty = true;
                }
            } else if r <= cell.r * LEARN_MAX_RATIO {
                cell.r += (r - cell.r) * (dt / LEARN_TAU_S).min(1.0);
                cell.secs = (cell.secs + dt).min(1.0e7);
                self.dirty = true;
            }
        }
        self.verdict(dt, Some(eval), i)
    }

    fn verdict(&mut self, dt: f32, eval: Option<Eval>, i: &EfficacyInput) -> bool {
        if let Some(e) = eval {
            self.last = Some(e);
            self.last_age = 0.0;
            self.evals += 1;
        }
        if !self.alarm_enabled {
            self.alarm = false;
            self.over_s = 0.0;
            self.ok_s = 0.0;
            return false;
        }
        match eval {
            Some(e) => {
                self.quiet_s = 0.0;
                let degraded = e.ratio.is_some_and(|q| q >= ALARM_RATIO)
                    && e.excess_c.is_some_and(|x| x >= ALARM_EXCESS_C)
                    && i.duty.is_some_and(|d| d >= DUTY_GATE);
                self.over_s = if degraded { self.over_s + dt } else { 0.0 };
                if !self.alarm && self.over_s >= HOLD_S {
                    self.alarm = true;
                    self.ok_s = 0.0;
                } else if self.alarm {
                    // A window without a baseline (another power band) is no evidence either way.
                    match e.ratio {
                        Some(q) if q < CLEAR_RATIO => self.ok_s += dt,
                        Some(_) => self.ok_s = 0.0,
                        None => {}
                    }
                    if self.ok_s >= CLEAR_HOLD_S {
                        self.alarm = false;
                        self.ok_s = 0.0;
                    }
                }
            }
            None => {
                // Nothing to judge: the hold restarts; a raised alarm clears only once the CPU
                // stayed cool for a long time.
                self.over_s = 0.0;
                if self.alarm {
                    let cool = i.tctl.is_some_and(|t| t <= COOL_CLEAR_C);
                    self.quiet_s = if cool { self.quiet_s + dt } else { 0.0 };
                    if self.quiet_s >= COOL_CLEAR_S {
                        self.alarm = false;
                        self.quiet_s = 0.0;
                    }
                }
            }
        }
        self.alarm
    }

    pub fn view(&self) -> EfficacyView {
        let recent = self.last.filter(|_| self.last_age <= 120.0);
        let state = if self.alarm {
            EfficacyState::Degraded
        } else {
            match recent {
                None => EfficacyState::NotJudged,
                Some(e) if e.baseline.is_none() => EfficacyState::Learning,
                Some(_) => EfficacyState::Normal,
            }
        };
        EfficacyView { state, last: recent, learned: self.baseline.learned(), alarm_enabled: self.alarm_enabled }
    }

    /// Saves the baseline when it changed and the last save is ≥ 10 min ago (`force`: whenever it
    /// changed). `None` when nothing was due.
    pub fn save(&mut self, force: bool) -> Option<anyhow::Result<()>> {
        use crate::error::{ErrorCode, WithCode};
        let path = self.path.clone()?;
        if !self.dirty || (!force && self.since_save < SAVE_EVERY_S) {
            return None;
        }
        self.since_save = 0.0;
        self.dirty = false;
        let res = serde_json::to_string(&self.baseline)
            .code(ErrorCode::Io)
            .and_then(|s| crate::util::fsx::write_atomic(&path, s.as_bytes()).code(ErrorCode::Io));
        Some(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FanConfig, Preset};
    use crate::control::curve::CurveState;
    use crate::control::sim_plant::{Load, Rng, SensorModel, SimPlant, cycle, gaming, idle, render};
    use crate::model::FanRole;

    /// How the CPU fan is driven in a run.
    #[derive(Clone, Copy)]
    enum Fan {
        Fixed(f64),
        Curve(Preset),
    }

    struct Run {
        /// First second at which the alarm was up.
        alarm_at: Option<usize>,
        evals: usize,
        max_ratio: f32,
        peak_tctl: f64,
    }

    /// Runs the plant at 1 s polls with the CPU fan per `fan` (case fans at 60 %), feeding the
    /// monitor like the engine does (median-of-3 Tctl, rounded board temperature, noisy power).
    fn run(
        p: &mut SimPlant,
        m: &mut EfficacyMonitor,
        secs: usize,
        fan: Fan,
        mut load: impl FnMut(f64, &mut Rng) -> Load,
    ) -> Run {
        let mut r = Rng(11);
        let mut hist = VecDeque::new();
        let mut curve = CurveState::default();
        let fc = match fan {
            Fan::Curve(pr) => Some(FanConfig::preset(pr, FanRole::Cpu)),
            Fan::Fixed(_) => None,
        };
        let mut duty = match fan {
            Fan::Fixed(d) => d,
            Fan::Curve(_) => 40.0,
        };
        let mut out = Run { alarm_at: None, evals: 0, max_ratio: 0.0, peak_tctl: 0.0 };
        for s in 0..secs {
            let l = load(p.t, &mut r);
            p.advance(1.0, l.pc, l.pg, [duty, 60.0, 60.0]);
            let rd = p.sense(l.pg);
            hist.push_back(rd.tctl as f32);
            if hist.len() > 3 {
                hist.pop_front();
            }
            let mut v: Vec<f32> = hist.iter().copied().collect();
            v.sort_by(f32::total_cmp);
            let med = v[v.len() / 2];
            out.peak_tctl = out.peak_tctl.max(p.td);
            if let Some(fc) = &fc {
                duty = curve.step(&fc.curve, med, 1.0).0.max(fc.min_duty) as f64;
            }
            let inp = EfficacyInput {
                tctl: Some(med),
                tref: Some(rd.tsys as f32),
                power_w: Some(rd.p_cpu as f32),
                duty: Some(duty as f32),
                ..Default::default()
            };
            let before = m.evals;
            if m.update(1.0, &inp, true) && out.alarm_at.is_none() {
                out.alarm_at = Some(s);
            }
            if m.evals > before {
                out.evals += 1;
                if let Some(q) = m.last.and_then(|e| e.ratio) {
                    out.max_ratio = out.max_ratio.max(q);
                }
            }
        }
        out
    }

    /// A monitor that learned a few hours of the mixed cycle on the Balanced curve.
    fn learned() -> (SimPlant, EfficacyMonitor) {
        let mut p = SimPlant::new(SensorModel::default());
        let mut m = EfficacyMonitor::default();
        let r = run(&mut p, &mut m, 4 * 3600, Fan::Curve(Preset::Balanced), |t, r| cycle(t, r, 1.0));
        assert_eq!(r.alarm_at, None);
        (p, m)
    }

    /// 24 h of normal use: no alarm, and the monitor learned and judged.
    #[test]
    fn no_false_alarm_in_a_day_of_mixed_use() {
        let mut p = SimPlant::new(SensorModel::default());
        p.ta_drift = true;
        let mut m = EfficacyMonitor::default();
        let r = run(&mut p, &mut m, 24 * 3600, Fan::Curve(Preset::Balanced), |t, r| cycle(t, r, 1.0));
        assert_eq!(r.alarm_at, None, "max ratio {}", r.max_ratio);
        assert!(r.evals > 3600, "judged {} s", r.evals);
        eprintln!("mixed day: worst ratio {:.3}, judged {} s", r.max_ratio, r.evals);
        assert!(m.baseline.learned() >= 2, "{:?}", m.baseline);
        assert!(r.max_ratio < ALARM_RATIO * 0.95, "max ratio {}", r.max_ratio);
        let v = m.view();
        assert!(matches!(v.state, EfficacyState::Normal | EfficacyState::NotJudged), "{v:?}");
    }

    /// Sustained all-core load (2 h, curve and pinned 100 %), gaming with bursts, idle, Silent and
    /// Performance curves: never an alarm, also for a monitor that only ever saw all-core load.
    #[test]
    fn no_false_alarm_under_heavy_normal_loads() {
        let (mut p, mut m) = learned();
        for (secs, fan) in [
            (7200, Fan::Curve(Preset::Balanced)),
            (7200, Fan::Fixed(100.0)),
            (3600, Fan::Curve(Preset::Silent)),
            (3600, Fan::Curve(Preset::Performance)),
        ] {
            let r = run(&mut p, &mut m, secs, fan, |_, _| render());
            assert_eq!(r.alarm_at, None, "all-core: max ratio {}", r.max_ratio);
            eprintln!("all-core: worst ratio {:.3}", r.max_ratio);
        }
        let burst = |t: f64, r: &mut Rng| {
            let mut l = gaming(t);
            if r.f() < 0.05 {
                l.pc += 30.0;
            }
            l
        };
        for fan in [Fan::Curve(Preset::Balanced), Fan::Fixed(100.0), Fan::Curve(Preset::Silent)] {
            let r = run(&mut p, &mut m, 7200, fan, burst);
            assert_eq!(r.alarm_at, None, "gaming: max ratio {}", r.max_ratio);
            eprintln!("gaming bursts: worst ratio {:.3}", r.max_ratio);
        }
        let r = run(&mut p, &mut m, 7200, Fan::Fixed(100.0), |_, r| idle(r, 1.0));
        assert_eq!(r.alarm_at, None);
        // All-core load from a cold start (nothing learned at low duty yet).
        let mut p = SimPlant::new(SensorModel::default());
        let mut m = EfficacyMonitor::default();
        let r = run(&mut p, &mut m, 4 * 3600, Fan::Fixed(100.0), |_, _| render());
        assert_eq!(r.alarm_at, None, "max ratio {}", r.max_ratio);
        // Load alternating between all-core and gaming every 6 min with the fans pinned at 100 %.
        let r = run(&mut p, &mut m, 4 * 3600, Fan::Fixed(100.0), |t, _| {
            if (t / 360.0) as u64 % 2 == 0 { render() } else { gaming(t) }
        });
        assert_eq!(r.alarm_at, None, "max ratio {}", r.max_ratio);
    }

    /// Other plants: noisier sensors (Tctl σ 1 °C, more spikes, ambient drift), a slow AIO (coolant
    /// time constant ×4) and a CPU with a higher junction resistance: never an alarm.
    #[test]
    fn no_false_alarm_on_other_plants() {
        let variants: [(&str, fn(&mut SimPlant)); 3] = [
            ("noisy sensors", |p| {
                p.sensor = SensorModel { sigma: 1.0, spike_p: 0.05, quant: 0.125 };
                p.ta_drift = true;
            }),
            ("slow AIO", |p| p.c_h *= 4.0),
            ("high R_jc", |p| p.r_jc = 0.45),
        ];
        for (name, setup) in variants {
            let mut p = SimPlant::new(SensorModel::default());
            setup(&mut p);
            let mut m = EfficacyMonitor::default();
            let r = run(&mut p, &mut m, 12 * 3600, Fan::Curve(Preset::Balanced), |t, r| cycle(t, r, 1.0));
            assert_eq!(r.alarm_at, None, "{name}: max ratio {}", r.max_ratio);
            let mut worst = r.max_ratio;
            for fan in [Fan::Fixed(100.0), Fan::Curve(Preset::Silent)] {
                let r = run(&mut p, &mut m, 3 * 3600, fan, |t, r| cycle(t, r, 1.0));
                assert_eq!(r.alarm_at, None, "{name}: max ratio {}", r.max_ratio);
                worst = worst.max(r.max_ratio);
            }
            eprintln!("{name}: worst ratio {worst:.3}");
        }
    }

    /// AT-2b: the pump slows to a third of its cooling (heatsink conductance ÷ 3) under steady load:
    /// the alarm fires, well before the CPU would reach Tjmax. A stopped pump fires too.
    #[test]
    fn detects_a_weakened_or_stopped_pump() {
        for (name, fan, load) in [
            ("gaming, Balanced", Fan::Curve(Preset::Balanced), gaming as fn(f64) -> Load),
            ("all-core, Balanced", Fan::Curve(Preset::Balanced), |_| render()),
            ("all-core, 100 %", Fan::Fixed(100.0), |_| render()),
        ] {
            let (mut p, mut m) = learned();
            let r = run(&mut p, &mut m, 900, fan, |t, _| load(t));
            assert_eq!(r.alarm_at, None, "{name}: before the fault");
            p.g0 /= 3.0;
            p.g1 /= 3.0;
            let r = run(&mut p, &mut m, 1800, fan, |t, _| load(t));
            let at = r.alarm_at.unwrap_or_else(|| panic!("{name}: no alarm, max ratio {}", r.max_ratio));
            eprintln!("{name}: weakened pump detected after {at} s");
            assert!(at <= 600, "{name}: {at} s");
            assert_eq!(m.view().state, EfficacyState::Degraded);
            assert_eq!(m.view().verified(), Some(false));
        }
        let (mut p, mut m) = learned();
        run(&mut p, &mut m, 900, Fan::Fixed(100.0), |t, _| gaming(t));
        p.g1 = 0.0;
        p.g0 = 0.3;
        let r = run(&mut p, &mut m, 900, Fan::Fixed(100.0), |t, _| gaming(t));
        let at = r.alarm_at.expect("stopped pump");
        eprintln!("stopped pump detected after {at} s");
        assert!(at <= 300, "{at} s");
        // A slow AIO (coolant time constant ×4): slower, still well before Tjmax.
        let mut p = SimPlant::new(SensorModel::default());
        p.c_h *= 4.0;
        let mut m = EfficacyMonitor::default();
        run(&mut p, &mut m, 4 * 3600, Fan::Curve(Preset::Balanced), |t, r| cycle(t, r, 1.0));
        run(&mut p, &mut m, 1800, Fan::Fixed(100.0), |_, _| render());
        p.g0 /= 3.0;
        p.g1 /= 3.0;
        let r = run(&mut p, &mut m, 3600, Fan::Fixed(100.0), |_, _| render());
        let at = r.alarm_at.unwrap_or_else(|| panic!("slow AIO: no alarm, max ratio {}", r.max_ratio));
        eprintln!("slow AIO, all-core: weakened pump detected after {at} s");
        assert!(at <= 900, "{at} s");
    }

    /// The alarm is only raised where it is enabled, clears when the cooler recovers, and nothing
    /// is learned meanwhile.
    #[test]
    fn alarm_gating_clearing_and_no_learning_while_degraded() {
        let (mut p, mut m) = learned();
        let before = m.baseline.clone();
        run(&mut p, &mut m, 900, Fan::Fixed(100.0), |t, _| gaming(t));
        let (g0, g1) = (p.g0, p.g1);
        p.g0 /= 3.0;
        p.g1 /= 3.0;
        // Disabled: judged, never raised.
        let mut quiet = EfficacyInput::default();
        for _ in 0..1200 {
            let l = gaming(p.t);
            p.advance(1.0, l.pc, l.pg, [100.0, 60.0, 60.0]);
            let rd = p.sense(l.pg);
            quiet = EfficacyInput {
                tctl: Some(rd.tctl as f32),
                tref: Some(rd.tsys as f32),
                power_w: Some(rd.p_cpu as f32),
                duty: Some(100.0),
                ..Default::default()
            };
            assert!(!m.update(1.0, &quiet, false));
        }
        assert!(m.last.and_then(|e| e.ratio).is_some_and(|q| q >= ALARM_RATIO), "{:?}", m.last);
        assert!(m.update(1.0, &quiet, true) || (0..120).any(|_| m.update(1.0, &quiet, true)));
        let cells = m.baseline.clone();
        // Recovery: clears after the ratio stayed low for 2 min.
        p.g0 = g0;
        p.g1 = g1;
        let r = run(&mut p, &mut m, 1800, Fan::Fixed(100.0), |t, _| gaming(t));
        assert!(r.alarm_at.is_some(), "still up right after recovery");
        assert!(
            !m.alarm,
            "cleared after recovery: {:?} {:?} evals {} cells {:?}",
            m.last,
            m.view(),
            r.evals,
            m.baseline.cells[4]
        );
        // The degraded windows were not learned (the gaming cell was not raised).
        for (b, row) in cells.cells.iter().enumerate() {
            for (k, c) in row.iter().enumerate() {
                let was = before.cells[b][k];
                if was.trusted() {
                    assert!(c.r <= was.r * 1.05, "cell [{b}][{k}] learned the fault: {} -> {}", was.r, c.r);
                }
            }
        }
        // A missing sensor: nothing judged.
        let none = EfficacyInput { tref: None, ..quiet };
        for _ in 0..200 {
            m.update(1.0, &none, true);
        }
        assert_eq!(m.view().state, EfficacyState::NotJudged);
    }

    /// Windows taken while a safety override forces 100 % are judged, never
    /// learned (a fresh baseline must not learn a failing cooler as normal).
    #[test]
    fn forced_full_speed_is_never_learned() {
        let learning = |m: &EfficacyMonitor| m.baseline.cells.iter().flatten().any(|c| c.secs > 0.0);
        let steady =
            EfficacyInput { tctl: Some(70.0), tref: Some(30.0), power_w: Some(60.0), duty: Some(100.0), forced: true };
        let mut m = EfficacyMonitor::default();
        for _ in 0..3600 {
            m.update(1.0, &steady, true);
        }
        assert!(m.evals > 0, "judged");
        assert!(!learning(&m), "{:?}", m.baseline);
        for _ in 0..600 {
            m.update(1.0, &EfficacyInput { forced: false, ..steady }, true);
        }
        assert!(learning(&m), "learns once the override ends");
    }

    #[test]
    fn buckets_bands_persistence_and_key() {
        assert_eq!((bucket(0.0), bucket(19.9), bucket(20.0), bucket(85.0), bucket(100.0)), (0, 0, 1, 4, 4));
        assert_eq!((band(20.0), band(30.0), band(55.0), band(88.0)), (None, Some(0), Some(1), Some(2)));
        let dir = std::env::temp_dir().join(format!("fanrgb-efficacy-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("cooling_baseline.json");
        let _ = std::fs::remove_file(&path);
        let mut m = EfficacyMonitor::load(path.clone());
        m.set_key("cpu|mb.pwm0:Cpu");
        m.baseline.cells[3][2] = Cell { r: 0.52, secs: 900.0 };
        m.dirty = true;
        assert!(m.save(false).is_none(), "not due yet");
        assert!(m.save(true).unwrap().is_ok());
        let back = EfficacyMonitor::load(path.clone());
        assert_eq!(back.baseline, m.baseline);
        assert_eq!(back.baseline_for(4, 2), Some(0.52), "a higher duty borrows the lower trusted cell");
        assert_eq!(back.baseline_for(2, 2), None, "never a higher duty's");
        let mut back = back;
        // An active alarm is never cleared by a key change; it waits.
        back.alarm = true;
        back.set_key("cpu|mb.pwm0:Pump");
        assert!(back.alarm && back.baseline.key == "cpu|mb.pwm0:Cpu" && back.baseline.learned() > 0);
        back.alarm = false;
        back.set_key("cpu|mb.pwm0:Pump");
        assert_eq!(back.baseline.learned(), 0, "new key: relearn");
        std::fs::write(&path, r#"{"key":"k","cells":[[{"r":"x"}]]}"#).unwrap();
        assert_eq!(EfficacyMonitor::load(path.clone()).baseline.learned(), 0);
        let mut bad = Baseline::default();
        bad.cells[0][0] = Cell { r: f32::NAN, secs: 1000.0 };
        bad.cells[1][0] = Cell { r: 0.5, secs: -1.0 };
        bad.sanitize();
        assert_eq!(bad.learned(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
