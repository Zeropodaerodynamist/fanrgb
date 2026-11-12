//! The original (v1) per-zone AI controller, kept only as a regression baseline for tests
//! ("keep a few of its runs as baselines so the improvement stays
//! measurable"). Verbatim model/MPC from the previous `ai.rs`, without persistence.
#![allow(dead_code)]

use crate::config::{AiConfig, AiStyle};
use crate::control::curve::interpolate;

pub const N: usize = 8;
const D_SCALE: f64 = 25.0;
const OBS_NOISE: f64 = 0.35;
const LAMBDA: f64 = 0.9995;
pub const HORIZON_S: f64 = 90.0;
const SIM_DT: f64 = 1.0;
const AIRFLOW_EXP: f64 = 0.7;
const SHADOW_WINDOW_S: f64 = 60.0;

const THETA_MIN: [f64; N] = [-0.5, 0.0, 0.0, 0.0, 0.002, 0.01, 0.0, 0.0];
const THETA_MAX: [f64; N] = [0.5, 3.0, 2.0, 80.0, 2.0, 3.0, 3.0, 2.0];
const PRIOR: [f64; N] = [0.0, 0.30, 0.05, 25.0, 0.06, 0.20, 0.20, 0.05];
const PRIOR_VAR: [f64; N] = [0.01, 0.09, 0.04, 400.0, 0.01, 0.04, 0.09, 0.04];

#[derive(Clone, Debug)]
pub struct ThermalModel {
    pub theta: [f64; N],
    pub p: [[f64; N]; N],
    pub samples: u64,
    pub err_1: f64,
    pub err_h: f64,
    pub err_h_n: u32,
    pub u_lo: f64,
    pub u_hi: f64,
}

impl Default for ThermalModel {
    fn default() -> Self {
        let mut p = [[0.0; N]; N];
        for i in 0..N {
            p[i][i] = PRIOR_VAR[i];
        }
        ThermalModel { theta: PRIOR, p, samples: 0, err_1: 0.0, err_h: 3.0, err_h_n: 0, u_lo: 1.0, u_hi: 0.0 }
    }
}

fn dot(a: &[f64; N], b: &[f64; N]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn airflow_term(u: f64) -> f64 {
    u.clamp(0.0, 1.0).powf(AIRFLOW_EXP)
}

impl ThermalModel {
    pub fn regressor(pc: f64, pg: f64, dpc: f64, t: f64, ta: f64, u: f64) -> [f64; N] {
        let d = (t - ta) / D_SCALE;
        let uk = airflow_term(u);
        [1.0, pc, pg, dpc, -d, -uk * d, uk * pc, uk * pg]
    }
    pub fn rate(&self, t: f64, ta: f64, pc: f64, pg: f64, u: f64) -> f64 {
        dot(&self.theta, &Self::regressor(pc, pg, 0.0, t, ta, u))
    }
    fn project(&mut self) {
        for i in 0..N {
            self.theta[i] = self.theta[i].clamp(THETA_MIN[i], THETA_MAX[i]);
        }
        self.theta[5] = self.theta[5].max(0.3 * self.theta[4]);
        self.theta[6] = self.theta[6].min(2.5 * self.theta[5]);
    }
    pub fn update(&mut self, x: &[f64; N], y: f64) -> f64 {
        let mut px = [0.0; N];
        for i in 0..N {
            px[i] = dot(&self.p[i], x);
        }
        let denom = OBS_NOISE + dot(x, &px);
        let e = y - dot(&self.theta, x);
        for i in 0..N {
            self.theta[i] += px[i] / denom * e;
        }
        for i in 0..N {
            for j in 0..N {
                self.p[i][j] = (self.p[i][j] - px[i] * px[j] / denom) / LAMBDA;
            }
        }
        for i in 0..N {
            for j in (i + 1)..N {
                let m = 0.5 * (self.p[i][j] + self.p[j][i]);
                self.p[i][j] = m;
                self.p[j][i] = m;
            }
        }
        for i in 0..N {
            let cap = PRIOR_VAR[i] * 4.0;
            if !(self.p[i][i] > 1e-12) {
                self.p[i][i] = 1e-12;
            }
            if self.p[i][i] > cap {
                let s = (cap / self.p[i][i]).sqrt();
                for j in 0..N {
                    self.p[i][j] *= s;
                    self.p[j][i] *= s;
                }
            }
        }
        if self.theta.iter().chain(self.p.iter().flatten()).any(|v| !v.is_finite()) {
            *self = ThermalModel::default();
            return 0.0;
        }
        self.project();
        self.samples += 1;
        e
    }
    fn note_airflow(&mut self, u: f64, dt: f64) {
        let decay = 0.0001 * dt;
        self.u_lo = (self.u_lo + decay).min(u);
        self.u_hi = (self.u_hi - decay).max(u);
    }
    pub fn rise_per_100w(&self, u: f64) -> f64 {
        let uk = airflow_term(u);
        let g = self.theta[4] + self.theta[5] * uk;
        D_SCALE * (self.theta[1] + self.theta[6] * uk) / g.max(1e-4)
    }
    pub fn confidence(&self) -> f32 {
        let c_samples = (self.samples as f64 / 300.0).min(1.0);
        let c_err = if self.err_h_n >= 2 { (1.0 - (self.err_h - 1.5) / 5.0).clamp(0.0, 1.0) } else { 0.25 };
        let c_exc = ((self.u_hi - self.u_lo) / 0.25).clamp(0.3, 1.0);
        (c_samples * c_err * c_exc) as f32
    }
    pub fn simulate(&self, t0: f64, ta: f64, pc: f64, pg: f64, u: f64, steps: usize, dt: f64, out: &mut Vec<f64>) {
        out.clear();
        let mut t = t0;
        for _ in 0..steps {
            t += dt * self.rate(t, ta, pc, pg, u);
            t = t.clamp(ta - 5.0, 150.0);
            out.push(t);
        }
    }
}

fn style_params(style: AiStyle) -> (f64, f64, f64) {
    match style {
        AiStyle::Silent => (60.0, 30.0, 4.0),
        AiStyle::Balanced => (20.0, 12.0, 2.0),
        AiStyle::Performance => (6.0, 5.0, 1.0),
    }
}

#[derive(Clone, Debug, Default)]
pub struct ZoneInputs {
    pub temp: f32,
    pub ambient: Option<f32>,
    pub cpu_power: Option<f32>,
    pub cpu_load: Option<f32>,
    pub gpu_load: Option<f32>,
    pub airflow: f32,
    pub airflow_min: f32,
    pub dt: f32,
}

#[derive(Clone, Debug, Default)]
pub struct Decision {
    pub airflow: f32,
    pub over_limit: bool,
}

#[derive(Clone, Debug, Default)]
struct Prev {
    t: f64,
    ta: f64,
    pc: f64,
    pg: f64,
    u: f64,
}

#[derive(Clone, Debug, Default)]
pub struct ZoneRuntime {
    t_learn: Option<f64>,
    t_decide: Option<f64>,
    pc_fast: Option<f64>,
    pc_pred: Option<f64>,
    pg_pred: Option<f64>,
    prev: Option<Prev>,
    shadow: Option<(f64, f64)>,
    u_cmd: f64,
}

fn ema(prev: Option<f64>, x: f64, dt: f64, tau: f64) -> f64 {
    match prev {
        None => x,
        Some(p) => p + (x - p) * if tau <= 1e-3 { 1.0 } else { 1.0 - (-dt / tau).exp() },
    }
}

pub fn safe_curve_duty(cfg: &AiConfig, temp: f32) -> f32 {
    interpolate(
        &[
            [cfg.target - 25.0, 25.0],
            [cfg.target - 8.0, 40.0],
            [cfg.target, 62.0],
            [(cfg.target + cfg.limit) * 0.5, 85.0],
            [cfg.limit, 100.0],
        ],
        temp,
    )
}

impl ZoneRuntime {
    pub fn step(&mut self, model: &mut ThermalModel, cfg: &AiConfig, inp: &ZoneInputs) -> Decision {
        let (noise_weight, predict_tau, observe_tau) = style_params(cfg.style);
        let dt = (inp.dt as f64).clamp(0.05, 10.0);
        let temp = inp.temp as f64;
        let ta = inp.ambient.map(|a| a as f64).filter(|a| (5.0..60.0).contains(a)).unwrap_or(30.0);
        let pc_raw =
            inp.cpu_power.map(|w| w as f64 / 100.0).or(inp.cpu_load.map(|l| l as f64 / 100.0 * 0.9)).unwrap_or(0.3);
        let pg = inp.gpu_load.map(|l| l as f64 / 100.0).unwrap_or(0.0);
        let u_meas = (inp.airflow as f64).clamp(0.0, 1.0);
        let u_min = (inp.airflow_min as f64).clamp(0.0, 1.0);
        let t_learn = ema(self.t_learn, temp, dt, 1.5);
        let pc_fast = ema(self.pc_fast, pc_raw, dt, 1.5);
        let t_decide = ema(self.t_decide, temp, dt, observe_tau);
        let pc_pred = ema(self.pc_pred, pc_raw, dt, predict_tau);
        let pg_pred = ema(self.pg_pred, pg, dt, predict_tau);
        self.t_learn = Some(t_learn);
        self.pc_fast = Some(pc_fast);
        self.t_decide = Some(t_decide);
        self.pc_pred = Some(pc_pred);
        self.pg_pred = Some(pg_pred);
        if let Some(prev) = &self.prev {
            if dt <= 5.0 {
                let y = (t_learn - prev.t) / dt;
                if y.abs() < 20.0 {
                    let dpc = (pc_fast - prev.pc) / dt;
                    let x = ThermalModel::regressor(prev.pc, prev.pg, dpc, prev.t, prev.ta, prev.u);
                    let e = model.update(&x, y);
                    model.err_1 += ((e * dt).abs() - model.err_1) * 0.02;
                    model.note_airflow(prev.u, dt);
                }
                let (mut st, mut elapsed) = self.shadow.unwrap_or((prev.t, 0.0));
                st += dt * model.rate(st, prev.ta, prev.pc, prev.pg, prev.u) + model.theta[3] * (pc_fast - prev.pc);
                elapsed += dt;
                if elapsed >= SHADOW_WINDOW_S {
                    let err = (st - t_learn).abs();
                    model.err_h = if model.err_h_n == 0 { err } else { model.err_h + (err - model.err_h) * 0.2 };
                    model.err_h_n += 1;
                    self.shadow = Some((t_learn, 0.0));
                } else {
                    self.shadow = Some((st, elapsed));
                }
            } else {
                self.shadow = None;
            }
        }
        self.prev = Some(Prev { t: t_learn, ta, pc: pc_fast, pg, u: u_meas });
        let confidence = model.confidence();
        if temp >= cfg.limit as f64 {
            self.u_cmd = 1.0;
            return Decision { airflow: 1.0, over_limit: true };
        }
        let u_prev = if self.u_cmd > 0.0 { self.u_cmd } else { u_meas };
        let jump = (model.theta[3] * (pc_fast - pc_pred)).clamp(-10.0, 10.0);
        let t0 = (t_decide - jump).max(ta);
        let steps = (HORIZON_S / SIM_DT) as usize;
        let (target, limit) = (cfg.target as f64, cfg.limit as f64);
        let mut traj = Vec::with_capacity(steps);
        let mut best = (f64::INFINITY, 1.0);
        let mut u = u_min;
        loop {
            model.simulate(t0, ta, pc_pred, pg_pred, u, steps, SIM_DT, &mut traj);
            let (mut wsum, mut over, mut peak) = (0.0, 0.0, t0);
            for (k, &tk) in traj.iter().enumerate() {
                let w = 0.2 + 0.8 * (k as f64 / steps as f64);
                let o = (tk - target).max(0.0);
                over += w * o * o;
                wsum += w;
                peak = peak.max(tk);
            }
            let mut cost = noise_weight * u * u + 10.0 * over / wsum + 2.0 * (u - u_prev).powi(2);
            if peak > limit {
                cost += 1e4 * (peak - limit).powi(2);
            }
            if cost < best.0 {
                best = (cost, u);
            }
            if u >= 1.0 {
                break;
            }
            u = (u + 0.01).min(1.0);
        }
        let u_mpc = best.1;
        let fb_duty = safe_curve_duty(cfg, t_decide as f32) as f64;
        let fb_air = if fb_duty <= 0.0 { 0.0 } else { (0.2 + 0.8 * fb_duty / 100.0).min(1.0) };
        let c = confidence as f64;
        let u_final = (c * u_mpc + (1.0 - c) * u_mpc.max(fb_air)).clamp(u_min, 1.0);
        self.u_cmd = u_final;
        Decision { airflow: u_final as f32, over_limit: false }
    }
}
