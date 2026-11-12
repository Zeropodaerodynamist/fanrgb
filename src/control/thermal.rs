//! Explainable grey-box thermal model, joint CPU/case-fan MPC and anti-hunting supervisor
//! ("v2" AI fan controller).
//!
//! # Model (per PC, not per zone)
//! ```text
//! dT_h/dt = a·P_c/100 − (b0 + b1·φ(u_cpu))·(T_h − T_c)/25 + w_h        heatsink   θ_h = [a, b0, b1]
//! dT_c/dt = c1·P_c/100 + c2·P_g/100 − (e0 + e1·u_case)·T_c/25 + k0 + k1·u_case   case air θ_c
//! T_die   = T_h + R_jc·P_c                                             die (algebraic)
//! T_gpu   = T_c + R_g·P_g + b_g                                        GPU offset over case air
//! φ(u)    = u^0.8,   u = RPM / max RPM
//! ```
//! Every parameter group is identified (robust RLS, time-based forgetting toward the prior) from
//! the signal mode that excites it: the die offset from power steps, the heatsink from 5 s block
//! means, the case node and GPU from 30 s blocks. A residual `w_h` gives integral action.
//!
//! # Decision
//! A 300 s / 5 s-step rollout plus an analytic steady state is evaluated for every (u_cpu, u_case)
//! on a 0.04 grid; the cost is loudness (Σ k·u^1.5) + over-target² + an L1 move cost + an optional
//! learned quiet ceiling. The result passes through a per-group supervisor (immediate rise, delayed
//! slow fall, dwell, deadband) and a safety envelope that learning can never lower (floor curve,
//! limit latch, stale-input hold). No allocation happens in `step` once the buffers are warm.

use crate::config::AiStyle;
use crate::control::workload::{Ceiling, ClassStats, Classifier, Workload};

pub const MPC_STEPS: usize = 60;
pub const MPC_DT: f64 = 5.0;
pub const GRID_STEP: f64 = 0.04;
/// Seconds without learning after a gap, resume or any invalid sample.
pub const SETTLE_S: f64 = 10.0;
/// Learning freezes at or above this die temperature (Zen 4 starts to pull power: B21).
pub const LEARN_TCTL_MAX: f64 = 93.0;
const TAU_MEM_FAST: f64 = 7200.0;
const TAU_MEM_SLOW: f64 = 14400.0;
/// Informative validation runs needed for full confidence.
pub const N_INF_FULL: u32 = 6;
/// Posterior s.d. of the airflow gain b1 at which the fan-effect confidence factor reaches 0
/// (report: 0.15; 0.25 lets a well-identified model (±15 %) reach ~0.8).
pub const C_P_SD_SCALE: f64 = 0.25;
const VAL_RUN_S: f64 = 120.0;
const SHADOW_S: f64 = 30.0;
/// Open-loop error (°C) above which the model is distrusted.
pub const DISTRUST_ERR: f64 = 4.0;
const LATCH_RELEASE_S: f64 = 10.0;
/// L1 move cost per unit airflow, relative to the loudness weight (report: absolute 0.6, which is
/// ~1/70 of the loudness gradient and gives no hysteresis on a flat optimum).
pub const MOVE_COST: f64 = 0.3;
const LATCH_HYST: f64 = 3.0;

#[inline]
fn ema(prev: Option<f64>, x: f64, dt: f64, tau: f64) -> f64 {
    match prev {
        None => x,
        Some(p) => p + (x - p) * (1.0 - (-dt / tau).exp()),
    }
}

/// Longest median window (samples) of the input conditioner.
const MED_MAX: usize = 15;

/// Median of the last `n` entries of `ring` (allocation-free).
fn median_tail(ring: &[f64; MED_MAX], n: usize) -> f64 {
    let mut buf = [0.0; MED_MAX];
    let n = n.clamp(1, MED_MAX);
    buf[..n].copy_from_slice(&ring[MED_MAX - n..]);
    buf[..n].sort_unstable_by(f64::total_cmp);
    if n % 2 == 1 { buf[n / 2] } else { 0.5 * (buf[n / 2 - 1] + buf[n / 2]) }
}

#[inline]
fn q_of(tau: f64, dt: f64) -> f64 {
    1.0 - (-dt / tau).exp()
}

/// Linear interpolation over points sorted by x, clamped outside.
pub fn interp(pts: &[(f64, f64)], x: f64) -> f64 {
    if x <= pts[0].0 {
        return pts[0].1;
    }
    for w in pts.windows(2) {
        if x <= w[1].0 {
            let span = (w[1].0 - w[0].0).max(1e-9);
            return w[0].1 + (x - w[0].0) / span * (w[1].1 - w[0].1);
        }
    }
    pts[pts.len() - 1].1
}

/// Fan law used for the heatsink conductance.
#[inline]
pub fn phi(u: f64) -> f64 {
    u.clamp(0.0, 1.0).powf(0.8)
}

// ---------------------------------------------------------------------------------------
// RLS

/// Fixed-size recursive least squares with prior-regularized, time-based forgetting (P relaxes
/// toward the prior and never exceeds it: no wind-up, no bursting) and Huber-weighted innovations.
#[derive(Clone, Debug, PartialEq)]
pub struct Rls<const N: usize> {
    pub theta: [f64; N],
    pub p: [[f64; N]; N],
    pub p0: [f64; N],
    pub prior: [f64; N],
    pub lo: [f64; N],
    pub hi: [f64; N],
    pub r: f64,
    pub n: u64,
    /// Normalized a-priori innovation |e|/sqrt(S) of the last update (consistency check).
    pub last_z: f64,
}

impl<const N: usize> Rls<N> {
    pub fn new(prior: [f64; N], sd: [f64; N], lo: [f64; N], hi: [f64; N], r: f64) -> Self {
        let mut p = [[0.0; N]; N];
        let mut p0 = [0.0; N];
        for i in 0..N {
            p[i][i] = sd[i] * sd[i];
            p0[i] = sd[i] * sd[i];
        }
        Rls { theta: prior, p, p0, prior, lo, hi, r, n: 0, last_z: 0.0 }
    }

    pub fn predict(&self, x: &[f64; N]) -> f64 {
        (0..N).map(|i| self.theta[i] * x[i]).sum()
    }

    /// One update; `q` = fraction of the way P relaxes toward the prior. Returns the a-priori
    /// innovation, or `None` if the sample was rejected (non-finite). Never resets learned state.
    pub fn update(&mut self, x: &[f64; N], y: f64, q: f64) -> Option<f64> {
        if !y.is_finite() || x.iter().any(|v| !v.is_finite()) || !q.is_finite() {
            return None;
        }
        let backup = (self.theta, self.p);
        let mut px = [0.0; N];
        for i in 0..N {
            px[i] = (0..N).map(|j| self.p[i][j] * x[j]).sum();
        }
        let s = self.r + (0..N).map(|i| x[i] * px[i]).sum::<f64>();
        let e = y - self.predict(x);
        let z = e.abs() / s.sqrt();
        self.last_z = z;
        let s_eff = if z > 3.0 { s * (z / 3.0) } else { s };
        for i in 0..N {
            self.theta[i] += px[i] / s_eff * e;
        }
        for i in 0..N {
            for j in 0..N {
                self.p[i][j] -= px[i] * px[j] / s_eff;
            }
        }
        let q = q.clamp(0.0, 1.0);
        for i in 0..N {
            for j in 0..N {
                let target = if i == j { self.p0[i] } else { 0.0 };
                self.p[i][j] += q * (target - self.p[i][j]);
            }
        }
        for i in 0..N {
            for j in (i + 1)..N {
                let m = 0.5 * (self.p[i][j] + self.p[j][i]);
                self.p[i][j] = m;
                self.p[j][i] = m;
            }
            self.p[i][i] = self.p[i][i].clamp(1e-12, self.p0[i]);
        }
        for i in 0..N {
            self.theta[i] = self.theta[i].clamp(self.lo[i], self.hi[i]);
        }
        if self.theta.iter().chain(self.p.iter().flatten()).any(|v| !v.is_finite()) {
            (self.theta, self.p) = backup;
            return None;
        }
        self.n += 1;
        Some(e)
    }

    pub fn sd(&self, i: usize) -> f64 {
        self.p[i][i].max(0.0).sqrt()
    }

    #[allow(dead_code)]
    pub fn trace(&self) -> f64 {
        (0..N).map(|i| self.p[i][i]).sum()
    }

    /// Uncertainty back to the prior, keeping the estimate (hardware changed).
    pub fn reprior(&mut self) {
        self.p = [[0.0; N]; N];
        for i in 0..N {
            self.p[i][i] = self.p0[i];
        }
    }

    /// Restores persisted state; rejects wrong sizes and non-finite values, clamps the rest.
    pub fn import(&mut self, theta: &[f64], p: &[Vec<f64>], n: u64) -> bool {
        if theta.len() != N || p.len() != N || p.iter().any(|r| r.len() != N) {
            return false;
        }
        if theta.iter().chain(p.iter().flatten()).any(|v| !v.is_finite()) {
            return false;
        }
        for i in 0..N {
            self.theta[i] = theta[i].clamp(self.lo[i], self.hi[i]);
            for j in 0..N {
                self.p[i][j] = p[i][j];
            }
            self.p[i][i] = self.p[i][i].clamp(1e-12, self.p0[i]);
        }
        self.n = n;
        true
    }
}

// ---------------------------------------------------------------------------------------
// Plant model

/// Hardware identity of a model; a large change re-priors the affected blocks (B24).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Fingerprint {
    pub cpu: String,
    pub temp_src: String,
    /// Calibrated max RPM per CPU-group fan (0 = not calibrated).
    pub cpu_fans: std::collections::BTreeMap<String, f32>,
    pub case_fans: std::collections::BTreeMap<String, f32>,
}

impl Fingerprint {
    fn changed(a: &std::collections::BTreeMap<String, f32>, b: &std::collections::BTreeMap<String, f32>) -> bool {
        if a.len() != b.len() {
            return true;
        }
        a.iter().any(|(k, &va)| match b.get(k) {
            None => true,
            Some(&vb) => va > 0.0 && vb > 0.0 && ((va - vb).abs() / va.max(vb)) > 0.10,
        })
    }
    /// (CPU group changed, case group changed)
    pub fn diff(&self, other: &Fingerprint) -> (bool, bool) {
        (Self::changed(&self.cpu_fans, &other.cpu_fans), Self::changed(&self.case_fans, &other.case_fans))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlantModel {
    /// Die offset `100·R_jc` (K per 100 W).
    pub die: Rls<1>,
    /// Heatsink `[a, b0, b1]`.
    pub hs: Rls<3>,
    /// Case air `[c1, c2, e0, e1, k0, k1]`.
    pub case: Rls<6>,
    /// GPU `100·R_g` (K per 100 W over case air).
    pub gpu: Rls<1>,
    /// Heatsink residual (K/s): integral action.
    pub w_h: f64,
    /// GPU bias over the model (°C).
    pub b_g: f64,
    /// Informative validation runs (load changes) predicted so far.
    pub n_inf: u32,
    /// EMA of the mean absolute die error over those runs (°C).
    pub val_err: f64,
    pub learn_samples: u64,
    /// Seconds of valid learning.
    pub learn_s: f64,
    /// Set when the model disagreed with reality (confidence forced to 0 until re-validated).
    pub distrusted: bool,
    pub last_shadow_err: f64,
    pub classes: ClassStats,
    pub ceiling: Ceiling,
    pub fingerprint: Fingerprint,
}

impl Default for PlantModel {
    fn default() -> Self {
        PlantModel {
            die: Rls::new([30.0], [12.0], [5.0], [60.0], 0.3 * 0.3),
            hs: Rls::new([0.25, 0.08, 0.25], [0.15, 0.06, 0.15], [0.03, 0.005, 0.02], [1.0, 0.5, 1.5], 0.02 * 0.02),
            case: Rls::new(
                [0.04, 0.03, 0.08, 0.2, 0.08, 0.2],
                [0.03, 0.03, 0.06, 0.15, 0.08, 0.2],
                [0.002, 0.0, 0.005, 0.0, -0.5, -0.5],
                [0.3, 0.3, 0.5, 1.5, 1.0, 2.0],
                0.006 * 0.006,
            ),
            gpu: Rls::new([25.0], [15.0], [5.0], [60.0], 1.0),
            w_h: 0.0,
            b_g: 0.0,
            n_inf: 0,
            val_err: 3.0,
            learn_samples: 0,
            learn_s: 0.0,
            distrusted: false,
            last_shadow_err: 0.0,
            classes: ClassStats::default(),
            ceiling: Ceiling::default(),
            fingerprint: Fingerprint::default(),
        }
    }
}

impl PlantModel {
    pub fn r_jc(&self) -> f64 {
        self.die.theta[0] / 100.0
    }
    pub fn r_g(&self) -> f64 {
        self.gpu.theta[0] / 100.0
    }
    /// Heatsink rate (K/s) without the residual.
    pub fn hs_rate(&self, th: f64, tc: f64, p: f64, u: f64) -> f64 {
        let t = &self.hs.theta;
        t[0] * p / 100.0 - (t[1] + t[2] * phi(u)) * (th - tc) / 25.0
    }
    pub fn case_rate(&self, tc: f64, p: f64, pg: f64, uc: f64) -> f64 {
        let t = &self.case.theta;
        t[0] * p / 100.0 + t[1] * pg / 100.0 - (t[2] + t[3] * uc) * tc / 25.0 + t[4] + t[5] * uc
    }
    pub fn t_amb_est(&self, uc: f64) -> f64 {
        let t = &self.case.theta;
        25.0 * (t[4] + t[5] * uc) / (t[2] + t[3] * uc).max(1e-4)
    }
    pub fn ss_case(&self, p: f64, pg: f64, uc: f64) -> f64 {
        let t = &self.case.theta;
        25.0 * ((t[0] * p + t[1] * pg) / 100.0 + t[4] + t[5] * uc) / (t[2] + t[3] * uc).max(1e-4)
    }
    pub fn ss_hs(&self, tc: f64, p: f64, u: f64) -> f64 {
        let t = &self.hs.theta;
        tc + 25.0 * t[0] * p / 100.0 / (t[1] + t[2] * phi(u)).max(1e-4)
    }
    /// Steady heatsink rise over case air per 100 W at CPU airflow `u`.
    pub fn hs_rise_per_100w(&self, u: f64) -> f64 {
        self.ss_hs(0.0, 100.0, u)
    }
    /// Steady die rise over case air per 100 W at CPU airflow `u` (heatsink + die offset).
    pub fn rise_per_100w(&self, u: f64) -> f64 {
        self.hs_rise_per_100w(u) + self.die.theta[0]
    }
    /// Case-air rise over ambient per 100 W of CPU heat at case airflow `uc`.
    pub fn case_rise_per_100w(&self, uc: f64) -> f64 {
        let t = &self.case.theta;
        25.0 * t[0] / (t[2] + t[3] * uc).max(1e-4)
    }
    pub fn tau_hs(&self, u: f64) -> f64 {
        let t = &self.hs.theta;
        25.0 / (t[1] + t[2] * phi(u)).max(1e-5)
    }
    /// Heatsink heat capacity (J/K).
    pub fn c_h(&self) -> f64 {
        100.0 / self.hs.theta[0].max(1e-6)
    }
    /// Heatsink thermal resistance (K/W) at airflow `u`.
    pub fn r_hs(&self, u: f64) -> f64 {
        let t = &self.hs.theta;
        25.0 / (self.c_h() * (t[1] + t[2] * phi(u)).max(1e-6))
    }

    /// (count, error, fan-effect) factors; the product is the confidence.
    pub fn confidence_factors(&self) -> [f64; 3] {
        let c_n = (self.n_inf as f64 / N_INF_FULL as f64).min(1.0);
        let c_e = (1.0 - (self.val_err - 1.5) / 3.0).clamp(0.0, 1.0);
        let c_p = (1.0 - self.hs.sd(2) / C_P_SD_SCALE).clamp(0.0, 1.0).max(0.2);
        [c_n, c_e, c_p]
    }

    pub fn confidence(&self) -> f64 {
        if self.distrusted {
            return 0.0;
        }
        let [a, b, c] = self.confidence_factors();
        a * b * c
    }

    /// Hardware changed: forget the uncertainty of the affected blocks and re-earn trust.
    pub fn apply_fingerprint(&mut self, fp: &Fingerprint) -> bool {
        let (cpu, case) = self.fingerprint.diff(fp);
        let changed = cpu || case;
        if cpu {
            self.hs.reprior();
            self.w_h = 0.0;
        }
        if case {
            self.case.reprior();
        }
        if changed {
            self.n_inf = 0;
        }
        self.fingerprint = fp.clone();
        changed
    }

    pub fn is_finite(&self) -> bool {
        let r = |t: &[f64]| t.iter().all(|v| v.is_finite());
        r(&self.die.theta)
            && r(&self.hs.theta)
            && r(&self.case.theta)
            && r(&self.gpu.theta)
            && self.w_h.is_finite()
            && self.b_g.is_finite()
    }
}

// ---------------------------------------------------------------------------------------
// Inputs / outputs

/// Airflow fraction at duty 0, 10, …, 100 % (max over the group's fans, from calibration).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AirMap(pub [f64; 11]);

impl Default for AirMap {
    fn default() -> Self {
        let mut a = [0.0; 11];
        for (i, v) in a.iter_mut().enumerate() {
            *v = if i == 0 { 0.0 } else { 0.2 + 0.8 * i as f64 / 10.0 };
        }
        AirMap(a)
    }
}

impl AirMap {
    pub fn at(&self, duty: f64) -> f64 {
        let x = (duty / 10.0).clamp(0.0, 10.0);
        let i = (x.floor() as usize).min(9);
        let f = x - i as f64;
        self.0[i] + (self.0[i + 1] - self.0[i]) * f
    }
}

#[derive(Clone, Debug)]
pub struct SystemInputs {
    /// CPU (die / Tctl) temperature, or the CPU group's chosen source.
    pub tctl: Option<f64>,
    /// Case / board air temperature (`mb.t0`).
    pub tsys: Option<f64>,
    pub tgpu: Option<f64>,
    /// Real CPU package power (W). `None` → fallback, learning frozen.
    pub p_cpu: Option<f64>,
    /// CPU load (%) for the power fallback.
    pub cpu_load: Option<f64>,
    /// GPU board power (W).
    pub p_gpu: Option<f64>,
    pub p_gpu_estimated: bool,
    /// Measured airflow (RPM / max RPM) of the CPU / case group, `None` if unknown.
    pub u_cpu: Option<f64>,
    pub u_case: Option<f64>,
    pub u_min_cpu: f64,
    pub u_min_case: f64,
    /// Loudness weights Σ (RPM_max/1500)^1.5 per group.
    pub k_cpu: f64,
    pub k_case: f64,
    pub air_cpu: AirMap,
    pub air_case: AirMap,
    /// Real elapsed seconds since the previous call (unclamped).
    pub dt_raw: f64,
    pub poll_s: f64,
    pub resumed: bool,
    pub calibrating: bool,
    /// Wall clock (unix seconds) and local hour, for the quiet-ceiling histogram.
    pub wall_s: f64,
    pub local_hour: u8,
}

impl Default for SystemInputs {
    fn default() -> Self {
        SystemInputs {
            tctl: None,
            tsys: None,
            tgpu: None,
            p_cpu: None,
            cpu_load: None,
            p_gpu: None,
            p_gpu_estimated: false,
            u_cpu: None,
            u_case: None,
            u_min_cpu: 0.36,
            u_min_case: 0.36,
            k_cpu: 1.0,
            k_case: 1.6,
            air_cpu: AirMap::default(),
            air_case: AirMap::default(),
            dt_raw: 1.0,
            poll_s: 1.0,
            resumed: false,
            calibrating: false,
            wall_s: 0.0,
            local_hour: 12,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GroupGoal {
    pub target: f64,
    pub limit: f64,
    pub style: AiStyle,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Targets {
    /// CPU die goals (from the CPU group, or defaults).
    pub cpu: GroupGoal,
    /// GPU goals (from the case group, or defaults).
    pub gpu: GroupGoal,
    pub cpu_ai: bool,
    pub case_ai: bool,
    pub quiet_ceiling: bool,
    pub probes: bool,
    pub ppt_w: f64,
    pub tbp_w: f64,
}

impl Default for Targets {
    fn default() -> Self {
        Targets {
            cpu: GroupGoal { target: 74.0, limit: 88.0, style: AiStyle::Balanced },
            gpu: GroupGoal { target: 80.0, limit: 88.0, style: AiStyle::Balanced },
            cpu_ai: true,
            case_ai: true,
            quiet_ceiling: true,
            probes: false,
            ppt_w: 88.0,
            tbp_w: 190.0,
        }
    }
}

pub struct StyleParams {
    pub noise_weight: f64,
    pub persist_s: f64,
    pub dwell_s: f64,
}

pub fn style_params(s: AiStyle) -> StyleParams {
    match s {
        AiStyle::Silent => StyleParams { noise_weight: 60.0, persist_s: 40.0, dwell_s: 60.0 },
        AiStyle::Balanced => StyleParams { noise_weight: 20.0, persist_s: 20.0, dwell_s: 30.0 },
        AiStyle::Performance => StyleParams { noise_weight: 6.0, persist_s: 10.0, dwell_s: 15.0 },
    }
}

/// Why the supervisor output is what it is (formatted for the UI by `describe`).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum SupState {
    #[default]
    Init,
    /// Following the optimum upward.
    Rising { want: f64 },
    /// The safety floor pushed the fan up quickly.
    SafetyRise { floor: f64 },
    /// Optimum inside the deadband.
    Hold { want: f64 },
    /// Lower optimum, waiting until the falling need is sustained.
    WaitPersist { want: f64, waited: f64, need: f64 },
    /// Lower optimum, minimum dwell after the last rise not over.
    WaitDwell { want: f64, since_up: f64, dwell: f64 },
    /// Easing down slowly.
    Falling { want: f64 },
    /// At or above the limit: 100 % until it cools down.
    Latched { limit: f64, below: f64 },
    /// Temperature input missing or invalid: holding.
    Stale { secs: f64 },
    /// Not in AI mode: follows the measured airflow.
    External,
}

impl SupState {
    pub fn describe(&self, u: f64) -> String {
        let pct = |x: f64| (x * 100.0).round();
        match *self {
            SupState::Init => format!("Starting at {:.0} % airflow.", pct(u)),
            SupState::Rising { want } => format!("Raising to {:.0} % airflow (needs {:.0} %).", pct(u), pct(want)),
            SupState::SafetyRise { floor } => {
                format!("Safety floor: raising quickly to {:.0} % airflow.", pct(floor.max(u)))
            }
            SupState::Hold { want } => {
                format!("Holding {:.0} %: the best speed ({:.0} %) is within the deadband.", pct(u), pct(want))
            }
            SupState::WaitPersist { want, waited, need } => format!(
                "Held {:.0} % — falling need ({:.0} %) not sustained {:.0}/{:.0} s.",
                pct(u),
                pct(want),
                waited,
                need
            ),
            SupState::WaitDwell { want, since_up, dwell } => format!(
                "Held {:.0} % — minimum time after a speed-up {:.0}/{:.0} s (then {:.0} %).",
                pct(u),
                since_up,
                dwell,
                pct(want)
            ),
            SupState::Falling { want } => {
                format!("Easing down slowly to {:.0} % airflow (now {:.0} %).", pct(want), pct(u))
            }
            SupState::Latched { limit, below } => {
                format!(
                    "At the {limit:.0} °C limit: full speed until below {:.0} °C for 10 s ({below:.0} s so far).",
                    limit - LATCH_HYST
                )
            }
            SupState::Stale { secs } => format!("Temperature input missing for {secs:.0} s: holding {:.0} %.", pct(u)),
            SupState::External => "Not in AI mode (measured only).".into(),
        }
    }
}

/// Per fan-group output supervisor: deadband, persistence, dwell, asymmetric slew.
#[derive(Clone, Debug, Default)]
pub struct FanSup {
    pub u: f64,
    pub init: bool,
    below_s: f64,
    since_up: f64,
    pub state: SupState,
}

impl FanSup {
    pub const UP_BAND: f64 = 0.03;
    pub const DOWN_BAND: f64 = 0.05;

    /// Returns the new command. `persist`/`dwell` in seconds.
    pub fn step(&mut self, u_opt: f64, floor: f64, dt: f64, persist: f64, dwell: f64) -> f64 {
        let floor = floor.clamp(0.0, 1.0);
        if !self.init {
            self.init = true;
            self.u = self.u.max(floor);
        }
        let want = u_opt.max(floor).min(1.0);
        self.since_up += dt;
        if floor > self.u + 1e-9 {
            self.u = (self.u + 0.20 * dt).min(want);
            self.since_up = 0.0;
            self.below_s = 0.0;
            self.state = SupState::SafetyRise { floor };
        } else if want > self.u + Self::UP_BAND {
            self.u = (self.u + 0.05 * dt).min(want);
            self.since_up = 0.0;
            self.below_s = 0.0;
            self.state = SupState::Rising { want };
        } else if want < self.u - Self::DOWN_BAND {
            self.below_s += dt;
            if self.below_s < persist {
                self.state = SupState::WaitPersist { want, waited: self.below_s, need: persist };
            } else if self.since_up < dwell {
                self.state = SupState::WaitDwell { want, since_up: self.since_up, dwell };
            } else {
                self.u = (self.u - 0.01 * dt).max(want + 0.02);
                self.state = SupState::Falling { want };
            }
        } else {
            self.below_s = 0.0;
            self.state = SupState::Hold { want };
        }
        self.u = self.u.clamp(floor.min(1.0), 1.0);
        self.u
    }

    pub fn force(&mut self, u: f64, state: SupState) -> f64 {
        self.init = true;
        if u > self.u {
            self.since_up = 0.0;
        }
        self.u = u.clamp(0.0, 1.0);
        self.below_s = 0.0;
        self.state = state;
        self.u
    }
}

/// Why learning is (not) happening right now.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum LearnState {
    #[default]
    Learning,
    Settling(f64),
    NoTemperature,
    NoCaseTemperature,
    NoPower,
    NoFanSpeed,
    Calibrating,
    TooHot,
}

impl LearnState {
    pub fn describe(&self) -> String {
        match *self {
            LearnState::Learning => "Learning from live data.".into(),
            LearnState::Settling(s) => format!("Waiting {s:.0} s for signals to settle after a gap or dropout."),
            LearnState::NoTemperature => "Paused: CPU temperature missing or implausible.".into(),
            LearnState::NoCaseTemperature => "Paused: board/case temperature missing.".into(),
            LearnState::NoPower => "Paused: real CPU power not available (estimates are never learned).".into(),
            LearnState::NoFanSpeed => "Paused: CPU fan speed unknown (needs an RPM reading and calibration).".into(),
            LearnState::Calibrating => "Paused while a fan is being calibrated.".into(),
            LearnState::TooHot => format!("Paused above {LEARN_TCTL_MAX:.0} °C (the CPU throttles there)."),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Decision {
    pub u_cpu: f64,
    pub u_case: f64,
    /// Raw optimum before the supervisor.
    pub opt_cpu: f64,
    pub opt_case: f64,
    pub floor_cpu: f64,
    pub floor_case: f64,
    /// Safety drove the output up this tick: the engine must not ramp-limit it.
    pub safety_cpu: bool,
    pub safety_case: bool,
    pub latched: bool,
    pub stale: bool,
    pub confidence: f64,
    pub conf_factors: [f64; 3],
    pub state_cpu: SupState,
    pub state_case: SupState,
    pub learn: LearnState,
    /// Forecast at the applied speeds, 5 s steps.
    pub predicted_die: Vec<f32>,
    pub predicted_gpu: Vec<f32>,
    pub predicted_peak: f64,
    pub predicted_end: f64,
    pub class: Workload,
    pub ceil_cpu: Option<f64>,
    pub ceil_case: Option<f64>,
    pub ceiling_active: bool,
    pub probing: bool,
    /// Model newly distrusted this tick (engine raises a notice).
    pub distrust_event: bool,
    pub t_filtered: f64,
}

#[derive(Clone, Copy, Debug, Default)]
struct Block {
    n: f64,
    t: f64,
    th: f64,
    tc: f64,
    p: f64,
    pg: f64,
    phi: f64,
    uc: f64,
    tg: f64,
}

impl Block {
    #[allow(clippy::too_many_arguments)]
    fn add(&mut self, dt: f64, th: f64, tc: f64, p: f64, pg: f64, phi: f64, uc: f64, tg: f64) {
        self.n += 1.0;
        self.t += dt;
        self.th += th;
        self.tc += tc;
        self.p += p;
        self.pg += pg;
        self.phi += phi;
        self.uc += uc;
        self.tg += tg;
    }
    fn mean(&self) -> Block {
        let n = self.n.max(1.0);
        Block {
            n: self.n,
            t: self.t,
            th: self.th / n,
            tc: self.tc / n,
            p: self.p / n,
            pg: self.pg / n,
            phi: self.phi / n,
            uc: self.uc / n,
            tg: self.tg / n,
        }
    }
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
struct ValRun {
    th: f64,
    tc: f64,
    el: f64,
    acc: f64,
    /// Parameters frozen at the start of the run ([a, b0, b1, R_jc, w_h]): a true open-loop test
    /// that the estimator cannot "pass" by adapting during the window.
    p: [f64; 5],
}

impl ValRun {
    fn start(m: &PlantModel, th: f64, tc: f64) -> Self {
        let t = &m.hs.theta;
        ValRun { th, tc, el: 0.0, acc: 0.0, p: [t[0], t[1], t[2], m.r_jc(), m.w_h] }
    }
    /// Advances the frozen-model heatsink by `dt`; returns the absolute die error against `tf`.
    fn advance(&mut self, dt: f64, tc: f64, pf: f64, u: f64, tf: f64) -> f64 {
        let [a, b0, b1, rj, wh] = self.p;
        self.th += dt * (a * pf / 100.0 - (b0 + b1 * phi(u)) * (self.th - tc) / 25.0 + wh);
        self.tc = tc;
        self.el += dt;
        let err = ((self.th + rj * pf) - tf).abs();
        self.acc += err * dt;
        err
    }
}

/// The whole AI controller runtime (filters, estimator, MPC buffers, supervisors).
/// The learned state lives in [`PlantModel`] so it can be persisted separately.
pub struct AiController {
    tq: [f64; MED_MAX],
    pq: [f64; MED_MAX],
    q_n: usize,
    tf: Option<f64>,
    pf: Option<f64>,
    tc: Option<f64>,
    tg: Option<f64>,
    p_slow: Option<f64>,
    pg_slow: Option<f64>,
    p_last: Option<f64>,
    p_age: f64,
    /// Learned CPU load → power map (EMA moments n, Σx, Σy, Σxx, Σxy) for the power fallback.
    lp: [f64; 5],
    settle: f64,
    clock: f64,
    // die-offset events: 8 samples at >= 1 s spacing
    ring: [(f64, f64, f64); 8],
    ring_n: usize,
    ring_last: f64,
    blk5: Block,
    prev5: Option<Block>,
    blk30: Block,
    prev30: Option<Block>,
    val: Option<ValRun>,
    /// Staggered 30 s open-loop predictions (one started every 5 s).
    shadows: [Option<ValRun>; 6],
    shadow_next: f64,
    /// Consecutive 30 s-horizon predictions that missed by more than DISTRUST_ERR.
    shadow_over: u32,
    p_ema60: Option<f64>,
    u_ema60: Option<f64>,
    latched: bool,
    latch_below: f64,
    steady_s: f64,
    probe_left: f64,
    probes: [f64; 2],
    pub cpu: FanSup,
    pub case: FanSup,
    pub classifier: Classifier,
    grid_cpu: Vec<f64>,
    grid_case: Vec<f64>,
    case_traj: Vec<f64>,
    pc_k: [f64; MPC_STEPS],
    pg_k: [f64; MPC_STEPS],
    pub out: Decision,
    pub skipped: u64,
    pub stale_s: f64,
    /// True when the last step changed learned state (for save-if-dirty).
    pub learned: bool,
}

impl Default for AiController {
    fn default() -> Self {
        AiController {
            tq: [0.0; MED_MAX],
            pq: [0.0; MED_MAX],
            q_n: 0,
            tf: None,
            pf: None,
            tc: None,
            tg: None,
            p_slow: None,
            pg_slow: None,
            p_last: None,
            p_age: 0.0,
            lp: [0.0; 5],
            settle: 0.0,
            clock: 0.0,
            ring: [(0.0, 0.0, 0.0); 8],
            ring_n: 0,
            ring_last: f64::NEG_INFINITY,
            blk5: Block::default(),
            prev5: None,
            blk30: Block::default(),
            prev30: None,
            val: None,
            shadows: [None; 6],
            shadow_next: 0.0,
            shadow_over: 0,
            p_ema60: None,
            u_ema60: None,
            latched: false,
            latch_below: 0.0,
            steady_s: 0.0,
            probe_left: 0.0,
            probes: [f64::NEG_INFINITY; 2],
            cpu: FanSup::default(),
            case: FanSup::default(),
            classifier: Classifier::default(),
            grid_cpu: Vec::with_capacity(32),
            grid_case: Vec::with_capacity(32),
            case_traj: Vec::with_capacity(32 * MPC_STEPS),
            pc_k: [0.0; MPC_STEPS],
            pg_k: [0.0; MPC_STEPS],
            out: Decision {
                predicted_die: Vec::with_capacity(MPC_STEPS),
                predicted_gpu: Vec::with_capacity(MPC_STEPS),
                ..Default::default()
            },
            skipped: 0,
            stale_s: 0.0,
            learned: false,
        }
    }
}

fn fill_grid(g: &mut Vec<f64>, u_min: f64) {
    g.clear();
    let mut u = u_min.clamp(0.0, 1.0);
    while u < 1.0 - 1e-9 {
        g.push(u);
        u += GRID_STEP;
    }
    g.push(1.0);
}

impl AiController {
    /// Forget filters, blocks and validation windows (gap, resume, invalid sample).
    fn learn_reset(&mut self) {
        self.ring_n = 0;
        self.ring_last = f64::NEG_INFINITY;
        self.blk5 = Block::default();
        self.prev5 = None;
        self.blk30 = Block::default();
        self.prev30 = None;
        self.val = None;
        self.shadows = [None; 6];
        self.shadow_over = 0;
        self.settle = SETTLE_S;
    }

    fn on_gap(&mut self) {
        self.q_n = 0;
        self.tf = None;
        self.pf = None;
        self.tc = None;
        self.tg = None;
        self.p_slow = None;
        self.pg_slow = None;
        self.p_last = None;
        self.p_ema60 = None;
        self.u_ema60 = None;
        self.latch_below = 0.0;
        self.learn_reset();
    }

    #[allow(dead_code)]
    pub fn filtered_temp(&self) -> Option<f64> {
        self.tf
    }

    pub fn step(&mut self, m: &mut PlantModel, inp: &SystemInputs, tg: &Targets) -> &Decision {
        self.learned = false;
        self.out.distrust_event = false;
        let poll = if inp.poll_s.is_finite() { inp.poll_s.clamp(0.1, 10.0) } else { 1.0 };
        let gap =
            inp.resumed || !inp.dt_raw.is_finite() || inp.dt_raw <= 0.0 || inp.dt_raw > (2.5 * poll).max(poll + 1.5);
        if gap {
            self.on_gap();
        }
        let dt = if gap { poll } else { inp.dt_raw.clamp(0.02, (3.0 * poll).max(poll + 1.5)) };
        self.clock += dt;

        // ---- validate ----
        let ok = |v: Option<f64>, lo: f64, hi: f64| v.filter(|x| x.is_finite() && *x >= lo && *x <= hi);
        let tctl = ok(inp.tctl, 10.0, 115.0);
        let tsys = ok(inp.tsys, 5.0, 70.0);
        let tgpu = ok(inp.tgpu, 10.0, 120.0);
        let p_cpu = ok(inp.p_cpu, 0.0, 250.0);
        let p_gpu = ok(inp.p_gpu, 0.0, 600.0);
        let u_cpu = ok(inp.u_cpu, 0.0, 1.05).map(|u| u.min(1.0));
        let u_case = ok(inp.u_case, 0.0, 1.05).map(|u| u.min(1.0));
        let u_min_cpu = if inp.u_min_cpu.is_finite() { inp.u_min_cpu.clamp(0.0, 0.9) } else { 0.4 };
        let u_min_case = if inp.u_min_case.is_finite() { inp.u_min_case.clamp(0.0, 0.9) } else { 0.4 };

        // ---- CPU power (fallbacks are used for control only, never learned) ----
        let load = ok(inp.cpu_load, 0.0, 100.0);
        let (p_now, p_real) = match p_cpu {
            Some(p) => {
                self.p_last = Some(p);
                self.p_age = 0.0;
                if let Some(l) = load {
                    // EMA (1 h) moments of (load, power) for a linear fallback map.
                    let a = q_of(3600.0, dt);
                    let lp = &mut self.lp;
                    lp[0] += (1.0 - lp[0]) * a;
                    lp[1] += (l - lp[1]) * a;
                    lp[2] += (p - lp[2]) * a;
                    lp[3] += (l * l - lp[3]) * a;
                    lp[4] += (l * p - lp[4]) * a;
                }
                (p, true)
            }
            None => {
                self.p_age += dt;
                // Learned map once ~5 min of paired samples with some load variation exist.
                let lp = self.lp;
                let n = lp[0].max(1e-9);
                let (mx, my) = (lp[1] / n, lp[2] / n);
                let var = lp[3] / n - mx * mx;
                let learned = (lp[0] > 0.08 && var > 25.0).then(|| {
                    let beta = ((lp[4] / n - mx * my) / var).clamp(0.0, 5.0);
                    move |l: f64| (my + beta * (l - mx)).clamp(5.0, tg.ppt_w.max(30.0))
                });
                match (self.p_last, load) {
                    (_, Some(l)) if learned.is_some() => (learned.unwrap()(l), false),
                    (Some(p), _) if self.p_age <= 60.0 => (p, false),
                    (_, Some(l)) => (20.0 + (tg.ppt_w - 20.0).max(0.0) * l / 100.0, false),
                    (Some(p), None) => (p, false),
                    (None, None) => (40.0, false),
                }
            }
        };
        let pg = p_gpu.unwrap_or(0.0);

        // ---- stale temperature: hold (no MPC, no learning) ----
        let Some(tctl) = tctl else {
            self.stale_s += dt;
            self.skipped += 1;
            self.learn_reset();
            let t_last = self.tf.unwrap_or(70.0);
            let (fc, fk) = self.floors(m, inp, tg, t_last + 3.0, self.tg.map(|t| t + 3.0), u_min_cpu, u_min_case);
            let st = SupState::Stale { secs: self.stale_s };
            let uc = if tg.cpu_ai { self.cpu.force(self.cpu.u.max(fc), st) } else { u_cpu.unwrap_or(self.cpu.u) };
            let uk = if tg.case_ai { self.case.force(self.case.u.max(fk), st) } else { u_case.unwrap_or(self.case.u) };
            let o = &mut self.out;
            o.u_cpu = uc;
            o.u_case = uk;
            o.floor_cpu = fc;
            o.floor_case = fk;
            o.safety_cpu = true;
            o.safety_case = true;
            o.stale = true;
            o.latched = self.latched;
            o.state_cpu = if tg.cpu_ai { st } else { SupState::External };
            o.state_case = if tg.case_ai { st } else { SupState::External };
            o.learn = LearnState::NoTemperature;
            o.confidence = m.confidence();
            o.conf_factors = m.confidence_factors();
            return &self.out;
        };
        if self.stale_s > 5.0 {
            // Long dropout (the engine handed the fans to the BIOS meanwhile): start over.
            self.on_gap();
        }
        self.stale_s = 0.0;

        // ---- conditioning: the same median (>= 3 samples, ~3 s) + EMA on temperature and power ----
        // Time-based so the noise reaching the decision does not depend on the poll interval.
        self.tq.rotate_left(1);
        self.pq.rotate_left(1);
        self.tq[MED_MAX - 1] = tctl;
        self.pq[MED_MAX - 1] = p_now;
        self.q_n = (self.q_n + 1).min(MED_MAX);
        let want = ((3.0 / dt).round() as usize).clamp(3, MED_MAX);
        let (tmed, pmed) = if self.q_n >= 3 {
            let n = want.min(self.q_n);
            (median_tail(&self.tq, n), median_tail(&self.pq, n))
        } else {
            (tctl, p_now)
        };
        let tau_f = 2.0 * dt.max(1.0);
        let tf = ema(self.tf, tmed, dt, tau_f);
        let pf = ema(self.pf, pmed, dt, tau_f);
        self.tf = Some(tf);
        self.pf = Some(pf);
        let tc = match tsys {
            Some(t) => ema(self.tc, t, dt, 15.0),
            None => self.tc.unwrap_or((tf - 15.0).clamp(20.0, 45.0)),
        };
        self.tc = Some(tc);
        if let Some(t) = tgpu {
            self.tg = Some(ema(self.tg, t, dt, 5.0));
        }
        // Forecast level: spike-free (median-3) power, quick to follow sustained rises, slow to drop.
        let tau_p = |prev: Option<f64>, x: f64| if prev.is_some_and(|p| x > p) { 5.0 } else { 15.0 };
        self.p_slow = Some(ema(self.p_slow, pmed, dt, tau_p(self.p_slow, pmed)));
        self.pg_slow = Some(ema(self.pg_slow, pg, dt, tau_p(self.pg_slow, pg)));
        self.settle = (self.settle - dt).max(0.0);

        // ---- learning gate ----
        let learn_state = if tsys.is_none() {
            LearnState::NoCaseTemperature
        } else if !p_real {
            LearnState::NoPower
        } else if u_cpu.is_none() {
            LearnState::NoFanSpeed
        } else if inp.calibrating {
            LearnState::Calibrating
        } else if tctl >= LEARN_TCTL_MAX {
            LearnState::TooHot
        } else if self.settle > 0.0 {
            LearnState::Settling(self.settle)
        } else {
            LearnState::Learning
        };
        if learn_state == LearnState::Learning {
            self.learn(
                m,
                inp,
                tf,
                pf,
                tc,
                pg,
                u_cpu.unwrap_or(0.5),
                u_case,
                p_gpu.is_some() && !inp.p_gpu_estimated,
                dt,
            );
        } else {
            self.skipped += 1;
            if !matches!(learn_state, LearnState::Settling(_)) {
                self.learn_reset();
            }
        }

        // ---- GPU bias (offset-free GPU prediction) ----
        if let (Some(tgf), Some(_)) = (self.tg, tgpu) {
            let b = tgf - (tc + m.r_g() * pg);
            m.b_g += (b.clamp(-30.0, 30.0) - m.b_g) * q_of(120.0, dt);
        }

        // ---- workload class + quiet ceiling statistics ----
        let class = self.classifier.update(&mut m.classes, p_now, pg, dt, tg.ppt_w, tg.tbp_w);
        if tg.cpu_ai {
            m.ceiling.add(class, inp.local_hour, 0, self.cpu.u, dt, inp.wall_s);
        }
        if tg.case_ai {
            m.ceiling.add(class, inp.local_hour, 1, self.case.u, dt, inp.wall_s);
        }

        // ---- decide ----
        let conf = m.confidence();
        let (floor_cpu, floor_case) = self.floors(m, inp, tg, tf, self.tg, u_min_cpu, u_min_case);
        let lim = tg.cpu.limit;
        if tf >= lim {
            self.latched = true;
            self.latch_below = 0.0;
        } else if self.latched {
            if tf < lim - LATCH_HYST {
                self.latch_below += dt;
                if self.latch_below >= LATCH_RELEASE_S {
                    self.latched = false;
                }
            } else {
                self.latch_below = 0.0;
            }
        }
        let u_cpu_ext = u_cpu.unwrap_or(if self.cpu.init { self.cpu.u } else { 0.5 });
        let u_case_ext = u_case.unwrap_or(if self.case.init { self.case.u } else { 0.5 });
        if !self.cpu.init {
            self.cpu.u = u_cpu_ext;
        }
        if !self.case.init {
            self.case.u = u_case_ext;
        }
        let spc = style_params(tg.cpu.style);
        let spk = style_params(tg.gpu.style);
        self.out.stale = false;
        self.out.class = class;
        self.out.learn = learn_state;
        self.out.confidence = conf;
        self.out.conf_factors = m.confidence_factors();
        self.out.floor_cpu = floor_cpu;
        self.out.floor_case = floor_case;
        self.out.t_filtered = tf;
        self.out.latched = self.latched;
        self.out.probing = false;

        // Quiet ceilings (learned), shown even when inactive.
        let hour = inp.local_hour;
        let ceil_cpu = if tg.quiet_ceiling && tg.cpu_ai {
            m.ceiling.ceiling(class, hour, 0, inp.wall_s).map(|c| c.max(floor_cpu))
        } else {
            None
        };
        let ceil_case = if tg.quiet_ceiling && tg.case_ai {
            m.ceiling.ceiling(class, hour, 1, inp.wall_s).map(|c| c.max(floor_case))
        } else {
            None
        };
        self.out.ceil_cpu = ceil_cpu;
        self.out.ceil_case = ceil_case;

        // Power forecast over the horizon (class feed-forward, never below the recent level).
        let p_hat = self.p_slow.unwrap_or(p_now);
        let pg_hat = self.pg_slow.unwrap_or(pg);
        self.classifier.forecast(&m.classes, p_hat, pg_hat, tg.ppt_w, tg.tbp_w, MPC_DT, &mut self.pc_k, &mut self.pg_k);
        let th0 = tf - m.r_jc() * pf;

        if self.latched {
            let uc = if tg.cpu_ai {
                self.cpu.force(1.0, SupState::Latched { limit: lim, below: self.latch_below })
            } else {
                u_cpu_ext
            };
            let uk = if tg.case_ai {
                let want = floor_case.max(self.case.u);
                self.case.step(want, floor_case, dt, 0.0, 0.0)
            } else {
                u_case_ext
            };
            self.out.u_cpu = uc;
            self.out.u_case = uk;
            self.out.opt_cpu = 1.0;
            self.out.opt_case = uk;
            self.out.safety_cpu = true;
            self.out.safety_case = true;
            self.out.state_cpu = if tg.cpu_ai { self.cpu.state } else { SupState::External };
            self.out.state_case = if tg.case_ai { self.case.state } else { SupState::External };
            self.out.ceiling_active = false;
            self.forecast(m, th0, tc, uc, uk, tg);
            return &self.out;
        }

        // Ceiling only applies when the current plan is comfortably under target.
        let (cpu_now, case_now) =
            (if tg.cpu_ai { self.cpu.u } else { u_cpu_ext }, if tg.case_ai { self.case.u } else { u_case_ext });
        self.forecast(m, th0, tc, cpu_now, case_now, tg);
        let under = self.out.predicted_peak <= tg.cpu.target
            && self.out.predicted_gpu.iter().all(|&g| (g as f64) <= tg.gpu.target);
        let ceil_ok = under && (ceil_cpu.is_some() || ceil_case.is_some());
        self.out.ceiling_active = ceil_ok;

        // ---- joint MPC ----
        if tg.cpu_ai {
            fill_grid(&mut self.grid_cpu, u_min_cpu);
        } else {
            self.grid_cpu.clear();
            self.grid_cpu.push(u_cpu_ext);
        }
        if tg.case_ai {
            fill_grid(&mut self.grid_case, u_min_case);
        } else {
            self.grid_case.clear();
            self.grid_case.push(u_case_ext);
        }
        let (best_c, best_k) = self.solve(
            m,
            th0,
            tc,
            tg,
            spc.noise_weight,
            spk.noise_weight,
            inp,
            if ceil_ok { (ceil_cpu, ceil_case) } else { (None, None) },
        );

        // ---- optional excitation probe ----
        let mut opt_c = best_c;
        let p_e = self.p_ema60.unwrap_or(pf);
        self.steady_s = if (pf - p_e).abs() < 5.0 { self.steady_s + dt } else { 0.0 };
        if self.probe_left > 0.0 {
            self.probe_left -= dt;
            opt_c = (opt_c + 0.08).min(1.0);
            self.out.probing = true;
        } else if tg.probes
            && tg.cpu_ai
            && conf < 0.6
            && self.steady_s >= 300.0
            && tf < tg.cpu.target - 6.0
            && !(22..24).contains(&hour)
            && hour >= 7
            && self.probes.iter().any(|&t| self.clock - t > 3600.0)
        {
            let slot = if self.probes[0] < self.probes[1] { 0 } else { 1 };
            self.probes[slot] = self.clock;
            self.probe_left = 120.0;
            self.steady_s = 0.0;
            log::info!("AI: excitation probe (+8 % CPU airflow for 120 s) to learn the fan effect");
        }

        let uc = if tg.cpu_ai {
            self.cpu.step(opt_c, floor_cpu, dt, spc.persist_s, spc.dwell_s)
        } else {
            self.cpu.u = u_cpu_ext;
            self.cpu.state = SupState::External;
            u_cpu_ext
        };
        let uk = if tg.case_ai {
            self.case.step(best_k, floor_case, dt, spk.persist_s * 1.5, spk.dwell_s * 2.0)
        } else {
            self.case.u = u_case_ext;
            self.case.state = SupState::External;
            u_case_ext
        };
        self.out.u_cpu = uc;
        self.out.u_case = uk;
        self.out.opt_cpu = opt_c;
        self.out.opt_case = best_k;
        self.out.safety_cpu = matches!(self.cpu.state, SupState::SafetyRise { .. });
        self.out.safety_case = matches!(self.case.state, SupState::SafetyRise { .. });
        self.out.state_cpu = self.cpu.state;
        self.out.state_case = self.case.state;
        if (uc - cpu_now).abs() > 1e-6 || (uk - case_now).abs() > 1e-6 {
            self.forecast(m, th0, tc, uc, uk, tg);
        }
        &self.out
    }

    /// Safety-envelope floors (airflow) for (CPU group, case group).
    #[allow(clippy::too_many_arguments)]
    fn floors(
        &self,
        m: &PlantModel,
        inp: &SystemInputs,
        tg: &Targets,
        t: f64,
        tgpu: Option<f64>,
        u_min_cpu: f64,
        u_min_case: f64,
    ) -> (f64, f64) {
        let c = m.confidence();
        let (tt, lim) = (tg.cpu.target, tg.cpu.limit);
        let conservative = |a: &AirMap, t: f64, target: f64, limit: f64| {
            interp(
                &[
                    (target - 25.0, a.at(25.0)),
                    (target - 8.0, a.at(40.0)),
                    (target, a.at(62.0)),
                    ((target + limit) * 0.5, a.at(85.0)),
                    (limit, 1.0),
                ],
                t,
            )
        };
        let len_cpu = interp(&[(lim - 14.0, u_min_cpu), (lim - 4.0, 0.6), (lim, 1.0)], t);
        let con_cpu = conservative(&inp.air_cpu, t, tt, lim);
        let floor_cpu = (c * len_cpu + (1.0 - c) * con_cpu).max(u_min_cpu).min(1.0);

        let (gt, gl) = (tg.gpu.target, tg.gpu.limit);
        let mut len_case = interp(&[(lim - 6.0, u_min_case), (lim, 0.8)], t);
        let mut con_case = conservative(&inp.air_case, t, tt, lim);
        if let Some(g) = tgpu {
            len_case = len_case.max(interp(&[(gl - 12.0, u_min_case), (gl, 1.0)], g));
            con_case = con_case.max(conservative(&inp.air_case, g, gt, gl));
        }
        let floor_case = (c * len_case + (1.0 - c) * con_case).max(u_min_case).min(1.0);
        (floor_cpu, floor_case)
    }

    #[allow(clippy::too_many_arguments)]
    fn learn(
        &mut self,
        m: &mut PlantModel,
        inp: &SystemInputs,
        tf: f64,
        pf: f64,
        tc: f64,
        pg: f64,
        u_cpu: f64,
        u_case: Option<f64>,
        gpu_ok: bool,
        dt: f64,
    ) {
        self.learned = true;
        m.learn_s += dt;
        // ---- die offset R_jc from power steps (8 samples at >= 1 s spacing) ----
        if self.clock - self.ring_last >= 0.999 {
            self.ring_last = self.clock;
            self.ring.rotate_left(1);
            self.ring[7] = (self.clock, tf, pf);
            self.ring_n += 1;
            if self.ring_n >= 8 {
                let r = &self.ring;
                let dp = r[7].2 - r[0].2;
                let early = r[2].2 - r[0].2;
                let late = (r[7].2 - r[3].2).abs();
                if dp.abs() > 8.0 && early.abs() > 0.5 * dp.abs() && late < 0.35 * dp.abs() {
                    let span = r[7].0 - r[0].0;
                    let dtt = r[7].1 - r[0].1;
                    let th_now = tf - m.r_jc() * pf;
                    let drift = span * m.hs_rate(th_now, tc, pf, u_cpu);
                    m.die.update(&[dp / 100.0], dtt - drift, q_of(TAU_MEM_FAST, span));
                    // A step is one event: don't count it again.
                    self.ring_n = 0;
                }
            }
        }
        // ---- heatsink: 5 s block means ----
        let th = tf - m.r_jc() * pf;
        let uc = u_case.unwrap_or(0.0);
        let tgf = self.tg.unwrap_or(tc);
        self.blk5.add(dt, th, tc, pf, pg, phi(u_cpu), uc, tgf);
        if self.blk5.t >= 5.0 - 1e-9 {
            let b = self.blk5.mean();
            if let Some(a) = self.prev5 {
                let span = 0.5 * (a.t + b.t);
                let y = (b.th - a.th) / span;
                let pm = 0.5 * (a.p + b.p);
                let d = 0.5 * ((a.th - a.tc) + (b.th - b.tc));
                let ph = 0.5 * (a.phi + b.phi);
                let x = [pm / 100.0, -d / 25.0, -ph * d / 25.0];
                let pred = m.hs.predict(&x);
                m.w_h = (m.w_h + (y - pred - m.w_h) * q_of(300.0, span)).clamp(-0.02, 0.02);
                if m.hs.update(&x, y, q_of(TAU_MEM_FAST, span)).is_some() {
                    m.learn_samples += 1;
                }
            }
            self.prev5 = Some(b);
            self.blk5 = Block::default();
        }
        // ---- case + GPU: 30 s blocks (case needs measured case airflow) ----
        if u_case.is_some() {
            self.blk30.add(dt, th, tc, pf, pg, 0.0, uc, tgf);
            if self.blk30.t >= 30.0 - 1e-9 {
                let b = self.blk30.mean();
                if let Some(a) = self.prev30 {
                    let span = 0.5 * (a.t + b.t);
                    let y = (b.tc - a.tc) / span;
                    let pm = 0.5 * (a.p + b.p);
                    let pgm = 0.5 * (a.pg + b.pg);
                    let um = 0.5 * (a.uc + b.uc);
                    let tcm = 0.5 * (a.tc + b.tc);
                    let x = [pm / 100.0, pgm / 100.0, -tcm / 25.0, -um * tcm / 25.0, 1.0, um];
                    m.case.update(&x, y, q_of(TAU_MEM_SLOW, span));
                    if gpu_ok && self.tg.is_some() && (b.tg - a.tg).abs() < 1.0 && pgm > 20.0 {
                        m.gpu.update(&[pgm / 100.0], b.tg - b.tc, q_of(TAU_MEM_SLOW, span));
                    }
                }
                self.prev30 = Some(b);
                self.blk30 = Block::default();
            }
        } else {
            self.blk30 = Block::default();
            self.prev30 = None;
        }
        // ---- validation on informative windows (120 s open loop) ----
        let p_e = ema(self.p_ema60, pf, dt, 60.0);
        let u_e = ema(self.u_ema60, u_cpu, dt, 60.0);
        if self.val.is_none()
            && ((pf - self.p_ema60.unwrap_or(pf)).abs() > 15.0 || (u_cpu - self.u_ema60.unwrap_or(u_cpu)).abs() > 0.1)
        {
            self.val = Some(ValRun::start(m, th, tc));
        }
        self.p_ema60 = Some(p_e);
        self.u_ema60 = Some(u_e);
        if let Some(mut v) = self.val {
            v.advance(dt, tc, pf, u_cpu, tf);
            if v.el >= VAL_RUN_S {
                let e = v.acc / v.el;
                let trusted = m.n_inf >= 2;
                m.val_err = if m.n_inf == 0 { e } else { m.val_err + (e - m.val_err) * 0.3 };
                m.n_inf = m.n_inf.saturating_add(1);
                self.val = None;
                self.p_ema60 = Some(pf);
                if e > DISTRUST_ERR && trusted {
                    self.distrust(m, e);
                } else if m.distrusted && m.n_inf >= 2 && m.val_err < 2.0 {
                    m.distrusted = false;
                }
            } else {
                self.val = Some(v);
            }
        }
        // ---- 30 s shadow: auto-distrust when the model disagrees with reality ----
        // Distrust when every 30 s-horizon prediction ending within 30 s missed by > 4 °C.
        if self.clock >= self.shadow_next {
            if let Some(slot) = self.shadows.iter_mut().find(|s| s.is_none()) {
                *slot = Some(ValRun::start(m, th, tc));
            }
            self.shadow_next = self.clock + SHADOW_S / 6.0;
        }
        for i in 0..self.shadows.len() {
            let Some(mut sh) = self.shadows[i] else { continue };
            let err = sh.advance(dt, tc, pf, u_cpu, tf);
            if sh.el >= SHADOW_S {
                m.last_shadow_err = err;
                self.shadow_over = if err > DISTRUST_ERR { self.shadow_over + 1 } else { 0 };
                self.shadows[i] = None;
            } else {
                self.shadows[i] = Some(sh);
            }
        }
        if self.shadow_over >= 6 && !m.distrusted && m.n_inf >= 2 {
            let e = m.last_shadow_err;
            self.distrust(m, e);
        }
        let _ = inp;
    }

    fn distrust(&mut self, m: &mut PlantModel, e: f64) {
        if !m.distrusted {
            log::warn!(
                "AI: model disagrees with reality ({e:.1} °C open-loop error); using the conservative floor until re-validated"
            );
            self.out.distrust_event = true;
        }
        m.distrusted = true;
        m.n_inf = 0;
        m.val_err = m.val_err.max(e);
    }

    /// Joint search over (u_cpu, u_case). Returns the optimum.
    #[allow(clippy::too_many_arguments)]
    fn solve(
        &mut self,
        m: &PlantModel,
        th0: f64,
        tc: f64,
        tg: &Targets,
        wn_cpu: f64,
        wn_case: f64,
        inp: &SystemInputs,
        ceil: (Option<f64>, Option<f64>),
    ) -> (f64, f64) {
        let steps = MPC_STEPS;
        let sdt = MPC_DT;
        let rj = m.r_jc();
        let lim = tg.cpu.limit;
        let have_gpu = self.tg.is_some();
        let gpu_r = m.r_g();
        let (k_cpu, k_case) = (inp.k_cpu.clamp(0.1, 10.0), inp.k_case.clamp(0.1, 10.0));
        let w_ceil_c = 3.0 * wn_cpu;
        let w_ceil_k = 3.0 * wn_case;
        let p_end = self.pc_k[steps - 1];
        let pg_end = self.pg_k[steps - 1];
        // case trajectories depend only on u_case
        self.case_traj.clear();
        for &uk in &self.grid_case {
            let mut t = tc;
            for k in 0..steps {
                t += sdt * m.case_rate(t, self.pc_k[k], self.pg_k[k], uk);
                self.case_traj.push(t);
            }
        }
        let mut wk = [0.0; MPC_STEPS];
        let mut wsum = 0.0;
        for (k, w) in wk.iter_mut().enumerate() {
            *w = 0.2 + 0.8 * k as f64 / steps as f64;
            wsum += *w;
        }
        let mut best = (f64::INFINITY, self.cpu.u, self.case.u);
        for (ki, &uk) in self.grid_case.iter().enumerate() {
            let tcs = &self.case_traj[ki * steps..(ki + 1) * steps];
            let tc_ss = m.ss_case(p_end, pg_end, uk);
            let mut g_cost = 0.0;
            if have_gpu {
                let mut g_over = 0.0;
                for (k, &tck) in tcs.iter().enumerate() {
                    let w = wk[k];
                    let o = (tck + gpu_r * self.pg_k[k] + m.b_g - tg.gpu.target).max(0.0);
                    g_over += w * o * o;
                }
                let g_ss = (tc_ss + gpu_r * pg_end + m.b_g - tg.gpu.target).max(0.0);
                g_cost = 10.0 * (g_over / wsum + 0.5 * g_ss * g_ss);
                let g_peak = tcs.iter().enumerate().fold(0.0f64, |a, (k, &t)| a.max(t + gpu_r * self.pg_k[k] + m.b_g));
                if g_peak > tg.gpu.limit {
                    g_cost += 1e4 * (g_peak - tg.gpu.limit).powi(2);
                }
            }
            let case_fixed = if tg.case_ai {
                wn_case * k_case * uk.powf(1.5)
                    + MOVE_COST * wn_case * (uk - self.case.u).abs()
                    + ceil.1.map_or(0.0, |c| w_ceil_k * (uk - c).max(0.0).powi(2))
            } else {
                0.0
            };
            for &uc in &self.grid_cpu {
                let mut th = th0;
                let mut over = 0.0;
                let mut peak: f64 = 0.0;
                // Same as hs_rate, with the fan-law term hoisted out of the loop.
                let (ha, g) = (m.hs.theta[0] / 100.0, (m.hs.theta[1] + m.hs.theta[2] * phi(uc)) / 25.0);
                for (k, &tck) in tcs.iter().enumerate() {
                    th += sdt * (ha * self.pc_k[k] - g * (th - tck) + m.w_h);
                    let td = th + rj * self.pc_k[k];
                    let w = wk[k];
                    let o = (td - tg.cpu.target).max(0.0);
                    over += w * o * o;
                    peak = peak.max(td);
                }
                let td_ss = m.ss_hs(tc_ss, p_end, uc) + rj * p_end + m.w_h * m.tau_hs(uc);
                let o_ss = (td_ss - tg.cpu.target).max(0.0);
                let mut j = 10.0 * (over / wsum + 0.5 * o_ss * o_ss) + g_cost + case_fixed;
                if tg.cpu_ai {
                    j += wn_cpu * k_cpu * uc.powf(1.5) + MOVE_COST * wn_cpu * (uc - self.cpu.u).abs();
                    if let Some(c) = ceil.0 {
                        j += w_ceil_c * (uc - c).max(0.0).powi(2);
                    }
                }
                if peak > lim {
                    j += 1e4 * (peak - lim).powi(2);
                }
                if j < best.0 {
                    best = (j, uc, uk);
                }
            }
        }
        (best.1, best.2)
    }

    /// Fills the forecast series at fixed speeds (5 s steps).
    fn forecast(&mut self, m: &PlantModel, th0: f64, tc0: f64, uc: f64, uk: f64, _tg: &Targets) {
        let o = &mut self.out;
        o.predicted_die.clear();
        o.predicted_gpu.clear();
        let (mut th, mut tc) = (th0, tc0);
        let rj = m.r_jc();
        let mut peak = f64::MIN;
        for k in 0..MPC_STEPS {
            tc += MPC_DT * m.case_rate(tc, self.pc_k[k], self.pg_k[k], uk);
            th += MPC_DT * (m.hs_rate(th, tc, self.pc_k[k], uc) + m.w_h);
            let td = th + rj * self.pc_k[k];
            peak = peak.max(td);
            o.predicted_die.push(td as f32);
            if self.tg.is_some() {
                o.predicted_gpu.push((tc + m.r_g() * self.pg_k[k] + m.b_g) as f32);
            }
        }
        o.predicted_peak = peak;
        o.predicted_end = o.predicted_die.last().copied().unwrap_or(0.0) as f64;
    }
}
