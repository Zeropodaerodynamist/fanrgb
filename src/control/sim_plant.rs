//! Closed-loop test plant for the AI controller: a 3-node thermal
//! model of a Ryzen 5 7500F + tower cooler, case air heated by an Arc B580, realistic sensor
//! defects (Tctl noise and spikes, integer board temperature, RPM noise and dropouts) and the
//! engine's output pipeline (airflow → duty via calibration → `shape_duty` → 0.4 % write gate).
#![allow(dead_code)]

use crate::config::{FanCalibration, FanConfig, Preset};
use crate::control::thermal::{AiController, AirMap, Decision, PlantModel, SystemInputs, Targets};
use crate::control::{airflow_to_duty, duty_to_airflow, effective_min, shape_duty};
use crate::model::FanRole;

/// xorshift64 (deterministic).
#[derive(Clone)]
pub struct Rng(pub u64);

impl Rng {
    pub fn f(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn n(&mut self) -> f64 {
        let u1 = self.f().max(1e-12);
        let u2 = self.f();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

pub const PLANT_SEED: u64 = 0x1234_5678_9abc;
pub const LOAD_SEED: u64 = 99;

#[derive(Clone, Copy, Debug)]
pub struct SensorModel {
    pub sigma: f64,
    pub spike_p: f64,
    pub quant: f64,
}

impl Default for SensorModel {
    fn default() -> Self {
        SensorModel { sigma: 0.5, spike_p: 0.02, quant: 0.125 }
    }
}

/// One sensor poll.
#[derive(Clone, Copy, Debug, Default)]
pub struct Reading {
    pub tctl: f64,
    pub tsys: f64,
    pub tgpu: f64,
    pub p_cpu: f64,
    pub p_gpu: f64,
    pub rpm: [Option<f64>; 3],
}

#[derive(Clone)]
pub struct SimPlant {
    pub ta: f64,
    pub ta_drift: bool,
    pub th: f64,
    pub td: f64,
    pub tc: f64,
    pub tg: f64,
    pub rpm: [f64; 3],
    pub max: [f64; 3],
    pub stall: [f64; 3],
    pub c_h: f64,
    pub g0: f64,
    pub g1: f64,
    pub phi_exp: f64,
    pub r_jc: f64,
    pub c_c: f64,
    pub case_g0: f64,
    pub case_g1: f64,
    pub r_g: f64,
    pub pthr: f64,
    pub rpm_missing_p: f64,
    pub sensor: SensorModel,
    pub rng: Rng,
    pub t: f64,
    /// Last actual CPU power (after Zen 4 limiting).
    pub p_act: f64,
}

impl SimPlant {
    pub fn new(sensor: SensorModel) -> Self {
        SimPlant {
            ta: 24.0,
            ta_drift: false,
            th: 34.0,
            td: 42.0,
            tc: 29.0,
            tg: 38.0,
            rpm: [600.0, 450.0, 450.0],
            max: [1850.0, 1450.0, 1450.0],
            stall: [14.0, 18.0, 18.0],
            c_h: 400.0,
            g0: 1.2,
            g1: 4.3,
            phi_exp: 0.8,
            r_jc: 0.33,
            c_c: 3000.0,
            case_g0: 8.0,
            case_g1: 30.0,
            r_g: 0.22,
            pthr: 1.0,
            rpm_missing_p: 0.001,
            sensor,
            rng: Rng(PLANT_SEED),
            t: 0.0,
            p_act: 22.0,
        }
    }

    pub fn u_cpu(&self) -> f64 {
        self.rpm[0] / self.max[0]
    }
    pub fn u_case(&self) -> f64 {
        0.5 * (self.rpm[1] / self.max[1] + self.rpm[2] / self.max[2])
    }

    /// Integrates `span` seconds at 0.05 s substeps with fixed load and duties.
    pub fn advance(&mut self, span: f64, p_cpu: f64, p_gpu: f64, duty: [f64; 3]) {
        let n = (span / 0.05).round().max(1.0) as usize;
        let dt = span / n as f64;
        for _ in 0..n {
            for i in 0..3 {
                let tgt = if duty[i] < self.stall[i] { 0.0 } else { self.max[i] * (0.2 + 0.8 * duty[i] / 100.0) };
                let tau = if tgt > self.rpm[i] { 1.5 } else { 3.0 };
                self.rpm[i] += (tgt - self.rpm[i]) * (1.0 - (-dt / tau).exp());
            }
            if self.td > 95.0 {
                self.pthr = (self.pthr - 0.05 * dt).max(0.5);
            } else if self.td < 93.0 {
                self.pthr = (self.pthr + 0.02 * dt).min(1.0);
            }
            let p = p_cpu.min(88.0) * self.pthr;
            self.p_act = p;
            let ta = if self.ta_drift { self.ta + (self.t * std::f64::consts::TAU / 21600.0).sin() } else { self.ta };
            let (u_cpu, u_case) = (self.u_cpu(), self.u_case());
            let g_case = self.case_g0 + self.case_g1 * u_case;
            self.tc += dt * (p + 0.8 * p_gpu - g_case * (self.tc - ta)) / self.c_c;
            let g_hs = self.g0 + self.g1 * u_cpu.clamp(0.0, 1.0).powf(self.phi_exp);
            self.th += dt * (p - g_hs * (self.th - self.tc)) / self.c_h;
            let tdie_ss = self.th + self.r_jc * p;
            self.td += (tdie_ss - self.td) * (1.0 - (-dt / 1.0).exp());
            let tg_ss = self.tc + p_gpu * self.r_g;
            self.tg += (tg_ss - self.tg) * (1.0 - (-dt / 20.0).exp());
            self.t += dt;
        }
    }

    pub fn sense(&mut self, p_gpu: f64) -> Reading {
        let s = self.sensor;
        let mut tctl = self.td + s.sigma * self.rng.n();
        if self.rng.f() < s.spike_p {
            tctl += 3.0 + 5.0 * self.rng.f();
        }
        let tctl = (tctl / s.quant).round() * s.quant;
        let tsys = (self.tc + 1.5 + 0.2 * self.rng.n()).round();
        let tgpu = (self.tg + 0.3 * self.rng.n()).round();
        let p_cpu = (self.p_act + 0.5 * self.rng.n()).max(0.0);
        let mut rpm = [None; 3];
        for (i, r) in rpm.iter_mut().enumerate() {
            let v = (self.rpm[i] + 30.0 * (self.rng.f() - 0.5)).max(0.0);
            *r = if self.rng.f() < self.rpm_missing_p { None } else { Some(v) };
        }
        Reading { tctl, tsys, tgpu, p_cpu, p_gpu, rpm }
    }

    /// True steady die temperature at the given powers / airflows.
    pub fn ss_die(&self, p: f64, pg: f64, u_cpu: f64, u_case: f64) -> f64 {
        let tc = self.ta + (p + 0.8 * pg) / (self.case_g0 + self.case_g1 * u_case);
        tc + p / (self.g0 + self.g1 * u_cpu.powf(self.phi_exp)) + self.r_jc * p
    }
    /// True heatsink rise over case air per 100 W.
    pub fn true_hs_rise(&self, u: f64) -> f64 {
        100.0 / (self.g0 + self.g1 * u.powf(self.phi_exp))
    }
    /// True die rise over case air per 100 W (heatsink + R_jc).
    pub fn true_die_rise(&self, u: f64) -> f64 {
        self.true_hs_rise(u) + 100.0 * self.r_jc
    }
    pub fn true_tau(&self, u: f64) -> f64 {
        self.c_h / (self.g0 + self.g1 * u.powf(self.phi_exp))
    }
}

/// Workload at time t (W CPU requested, W GPU).
#[derive(Clone, Copy, Debug, Default)]
pub struct Load {
    pub pc: f64,
    pub pg: f64,
}

pub fn idle(r: &mut Rng, dt: f64) -> Load {
    Load { pc: 22.0 + if r.f() < 0.03 * dt { 35.0 } else { 0.0 }, pg: 12.0 }
}

/// Mixed cycle (1800 s): idle 300, gaming 600, render 500, desktop 400.
pub fn cycle(t: f64, r: &mut Rng, dt: f64) -> Load {
    let c = t % 1800.0;
    let burst = if r.f() < 0.03 * dt { 35.0 } else { 0.0 };
    if c < 300.0 {
        Load { pc: 22.0 + burst, pg: 12.0 }
    } else if c < 900.0 {
        Load { pc: 55.0 + 8.0 * (t * 0.05).sin(), pg: 175.0 }
    } else if c < 1400.0 {
        Load { pc: 87.0, pg: 20.0 }
    } else {
        Load { pc: 30.0 + burst, pg: 25.0 }
    }
}

pub fn render() -> Load {
    Load { pc: 87.0, pg: 15.0 }
}

pub fn gaming(t: f64) -> Load {
    Load { pc: 55.0 + 8.0 * (t * 0.05).sin(), pg: 175.0 }
}

/// Simulated calibration like the wizard would measure it.
pub fn sim_calibration(max: f64, stall: f64) -> FanCalibration {
    let mut c = FanCalibration { max_rpm: max as f32, ..Default::default() };
    let steps = [100.0, 90.0, 80.0, 70.0, 60.0, 50.0, 45.0, 40.0, 35.0, 30.0, 25.0, 20.0, 15.0, 10.0, 5.0, 0.0];
    let mut prev = 100.0;
    for d in steps {
        let rpm = if d < stall { 0.0 } else { max * (0.2 + 0.8 * d / 100.0) };
        c.points.push([d as f32, rpm as f32]);
        if rpm == 0.0 {
            c.stop_duty = Some(prev as f32);
            c.start_duty = Some((prev + 5.0) as f32);
            break;
        }
        prev = d;
    }
    c
}

/// Per-tick record.
#[derive(Clone, Debug, Default)]
pub struct Trace {
    pub t: Vec<f64>,
    pub die: Vec<f64>,
    pub gpu: Vec<f64>,
    pub duty_cpu: Vec<f64>,
    pub duty_case: Vec<f64>,
    pub u_cpu: Vec<f64>,
    pub u_case: Vec<f64>,
    pub floor_cpu: Vec<f64>,
    pub floor_case: Vec<f64>,
    pub conf: Vec<f64>,
    pub latched: Vec<bool>,
    pub writes_cpu: u32,
    pub loud: Vec<f64>,
}

/// Plant + controller + engine output stage.
pub struct Rig {
    pub plant: SimPlant,
    pub ctrl: AiController,
    pub model: PlantModel,
    pub tg: Targets,
    pub fc: [FanConfig; 3],
    pub cal: [FanCalibration; 3],
    pub duty: [f64; 3],
    pub lr: Rng,
    pub t: f64,
    pub last: Decision,
    pub last_inputs: SystemInputs,
}

impl Rig {
    pub fn new(sensor: SensorModel) -> Self {
        let plant = SimPlant::new(sensor);
        let cal = [
            sim_calibration(plant.max[0], plant.stall[0]),
            sim_calibration(plant.max[1], plant.stall[1]),
            sim_calibration(plant.max[2], plant.stall[2]),
        ];
        Rig {
            plant,
            ctrl: AiController::default(),
            model: PlantModel::default(),
            tg: Targets::default(),
            fc: [
                FanConfig::preset(Preset::AiAuto, FanRole::Cpu),
                FanConfig::preset(Preset::AiAuto, FanRole::System),
                FanConfig::preset(Preset::AiAuto, FanRole::System),
            ],
            cal,
            duty: [40.0, 40.0, 40.0],
            lr: Rng(LOAD_SEED),
            t: 0.0,
            last: Decision::default(),
            last_inputs: SystemInputs::default(),
        }
    }

    pub fn k(&self) -> (f64, f64) {
        let k = |m: f64| (m / 1500.0).powf(1.5);
        (k(self.plant.max[0]), k(self.plant.max[1]) + k(self.plant.max[2]))
    }

    fn air_map(&self, idx: &[usize]) -> AirMap {
        let mut a = [0.0; 11];
        for (d, v) in a.iter_mut().enumerate() {
            for &i in idx {
                *v = f64::max(*v, duty_to_airflow(d as f32 * 10.0, Some(&self.cal[i])) as f64);
            }
        }
        AirMap(a)
    }

    /// Builds the controller inputs from a sensor reading (what the engine does).
    pub fn inputs(&self, r: &Reading, poll: f64) -> SystemInputs {
        let u = |i: usize| r.rpm[i].map(|v| v / self.plant.max[i]);
        let u_case = match (u(1), u(2)) {
            (Some(a), Some(b)) => Some(0.5 * (a + b)),
            _ => None,
        };
        let umin =
            |i: usize| duty_to_airflow(effective_min(&self.fc[i], Some(&self.cal[i])), Some(&self.cal[i])) as f64;
        let (k_cpu, k_case) = self.k();
        SystemInputs {
            tctl: Some(r.tctl),
            tsys: Some(r.tsys),
            tgpu: Some(r.tgpu),
            p_cpu: Some(r.p_cpu),
            cpu_load: Some(((r.p_cpu - 20.0) / 68.0 * 100.0).clamp(0.0, 100.0)),
            p_gpu: Some(r.p_gpu),
            p_gpu_estimated: false,
            u_cpu: u(0),
            u_case,
            u_min_cpu: umin(0),
            u_min_case: umin(1).max(umin(2)),
            k_cpu,
            k_case,
            air_cpu: self.air_map(&[0]),
            air_case: self.air_map(&[1, 2]),
            dt_raw: poll,
            poll_s: poll,
            resumed: false,
            calibrating: false,
            wall_s: 1.0e9 + self.t,
            local_hour: ((self.t / 3600.0) as u64 % 24) as u8,
        }
    }

    /// One control tick of `poll` seconds. `fault` may modify the controller inputs.
    pub fn tick(&mut self, load: Load, poll: f64, fault: &mut dyn FnMut(&mut Rig, &mut SystemInputs)) -> bool {
        let pl = Load { pc: load.pc, pg: load.pg };
        self.plant.advance(poll, pl.pc, pl.pg, self.duty);
        self.t += poll;
        let r = self.plant.sense(pl.pg);
        let mut inp = self.inputs(&r, poll);
        fault(self, &mut inp);
        self.last_inputs = inp.clone();
        let d = self.ctrl.step(&mut self.model, &inp, &self.tg).clone();
        // engine output stage
        let mut wrote = false;
        for i in 0..3 {
            let (u, safety) = if i == 0 {
                (d.u_cpu, d.safety_cpu || d.latched || d.stale)
            } else {
                (d.u_case, d.safety_case || d.stale)
            };
            let t = if i == 0 && d.latched { 100.0 } else { airflow_to_duty(u as f32, Some(&self.cal[i])) };
            let cur = Some(self.duty[i] as f32);
            let n = if safety {
                shape_duty(t, None, &self.fc[i], Some(&self.cal[i]), poll as f32)
            } else {
                shape_duty(t, cur, &self.fc[i], Some(&self.cal[i]), poll as f32)
            } as f64;
            if (n - self.duty[i]).abs() >= 0.4 {
                self.duty[i] = n;
                if i == 0 {
                    wrote = true;
                }
            }
        }
        self.last = d;
        wrote
    }

    /// Runs `secs` with `load(t, rng, dt)`, recording from `measure_from` (seconds of this run).
    pub fn run(
        &mut self,
        secs: f64,
        poll: f64,
        load: &dyn Fn(f64, &mut Rng, f64) -> Load,
        measure_from: f64,
        fault: &mut dyn FnMut(&mut Rig, &mut SystemInputs),
    ) -> Trace {
        let mut tr = Trace::default();
        let start = self.t;
        let n = (secs / poll).round() as usize;
        let (k_cpu, k_case) = self.k();
        for _ in 0..n {
            let mut lr = self.lr.clone();
            let l = load(self.t - start, &mut lr, poll);
            self.lr = lr;
            let wrote = self.tick(l, poll, fault);
            if self.t - start >= measure_from {
                if wrote {
                    tr.writes_cpu += 1;
                }
                tr.t.push(self.t - start);
                tr.die.push(self.plant.td);
                tr.gpu.push(self.plant.tg);
                tr.duty_cpu.push(self.duty[0]);
                tr.duty_case.push(self.duty[1]);
                tr.u_cpu.push(self.plant.u_cpu());
                tr.u_case.push(self.plant.u_case());
                tr.floor_cpu.push(self.last.floor_cpu);
                tr.floor_case.push(self.last.floor_case);
                tr.conf.push(self.last.confidence);
                tr.latched.push(self.last.latched);
                tr.loud.push(k_cpu * self.plant.u_cpu().powf(1.5) + k_case * self.plant.u_case().powf(1.5));
            }
        }
        tr
    }
}

pub fn no_fault() -> impl FnMut(&mut Rig, &mut SystemInputs) {
    |_: &mut Rig, _: &mut SystemInputs| {}
}

// ---------------------------------------------------------------------------------------
// Metrics

/// Direction reversals larger than `thr`.
pub fn reversals(xs: &[f64], thr: f64) -> u32 {
    let mut r = 0;
    let mut dir = 0i32;
    let mut ext = xs.first().copied().unwrap_or(0.0);
    for &x in xs {
        if dir >= 0 {
            if x > ext {
                ext = x
            } else if ext - x > thr {
                if dir == 1 {
                    r += 1
                }
                dir = -1;
                ext = x
            }
        } else if x < ext {
            ext = x
        } else if x - ext > thr {
            r += 1;
            dir = 1;
            ext = x
        }
    }
    r
}

pub fn rev10(xs: &[f64], poll: f64) -> f64 {
    let minutes10 = xs.len() as f64 * poll / 600.0;
    reversals(xs, 1.0) as f64 / minutes10.max(1e-9)
}

pub fn t_over(die: &[f64], thr: f64, poll: f64) -> f64 {
    die.iter().filter(|&&d| d > thr).count() as f64 * poll
}

pub fn ma_over(die: &[f64], target: f64, window: usize) -> f64 {
    let mut best = f64::MIN;
    let mut sum = 0.0;
    for i in 0..die.len() {
        sum += die[i];
        if i >= window {
            sum -= die[i - window];
        }
        if i + 1 >= window {
            best = best.max(sum / window as f64 - target);
        }
    }
    best
}

pub fn mean(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len().max(1) as f64
}

pub fn std(xs: &[f64]) -> f64 {
    let m = mean(xs);
    (xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / xs.len().max(1) as f64).sqrt()
}

pub fn max(xs: &[f64]) -> f64 {
    xs.iter().copied().fold(f64::MIN, f64::max)
}
