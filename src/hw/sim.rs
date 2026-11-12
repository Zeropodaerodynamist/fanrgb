//! Simulated hardware for `--demo` mode and tests: a lumped thermal model of a CPU
//! cooler, a GPU and a case, with a synthetic workload (idle, bursts, gaming sessions).

use anyhow::Result;

use super::superio::sg::{self, ChipCurve};
use super::{CrashSnapshot, DEFAULT_FAILSAFE, HwBackend, HwCurve, HwInfo};
use crate::error::{ErrorCode, err};
use crate::model::{FanChannelDesc, FanRole, SensorDesc, SensorKind, ids};

/// Small deterministic PRNG (xorshift64*).
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    pub fn next_f32(&mut self) -> f32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let v = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (v >> 40) as f32 / (1u64 << 24) as f32
    }
    pub fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.next_f32()
    }
}

/// Who drives a simulated fan: the "BIOS" curve, or a curve the simulated chip runs for us
/// (exactly like the IT8689E does with `set_fan`/`set_fan_curve`).
#[derive(Clone, Debug)]
enum SimCtl {
    Bios,
    Curve { input: u8, curve: ChipCurve },
}

struct SimFan {
    max_rpm: f32,
    stall_duty: f32,
    ctl: SimCtl,
    /// Duty the simulated chip outputs while it runs one of our curves.
    out: Option<f32>,
    rpm: f32,
}

impl SimFan {
    /// Steady-state RPM for a duty; PWM fans stop below their stall duty.
    fn target_rpm(&self, duty: f32) -> f32 {
        if duty < self.stall_duty { 0.0 } else { self.max_rpm * (0.18 + 0.82 * (duty / 100.0)).min(1.0) }
    }
}

pub struct SimHw {
    info: HwInfo,
    sensors: Vec<SensorDesc>,
    fans_desc: Vec<FanChannelDesc>,
    fans: Vec<SimFan>,
    rng: Rng,
    t: f32,
    dt: f32,
    ambient: f32,
    heatsink: f32,
    gpu_temp: f32,
    case_temp: f32,
    cpu_power: f32,
    gpu_load: f32,
    phase: Phase,
    phase_left: f32,
    burst_left: f32,
    failsafe: (u8, f32),
    /// Per fan: the header reports no tach although the fan spins (test hook).
    no_tach: [bool; 3],
    /// The CPU cooler lost its forced convection (test hook).
    cooler_failed: bool,
    /// Fixed CPU package power instead of the synthetic workload (test hook).
    pinned_power: Option<f32>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    Idle,
    Desktop,
    Gaming,
    Render,
}

impl SimHw {
    pub fn new(dt: f32) -> Self {
        let mut info = HwInfo {
            board: "Simulated board (demo mode)".into(),
            cpu: "Simulated 6-core CPU".into(),
            elevated: true,
            pawnio: super::pawnio::PawnIoStatus::Ready,
            pawnio_version: Some("demo".into()),
            chips: vec!["Simulated IT8689E".into()],
            cpu_temp_source: "simulation".into(),
            gpus: vec!["Simulated GPU".into()],
            simulated: true,
            fan_control: true,
            ..Default::default()
        };
        info.report.push("Demo mode: all sensors and fans are simulated".into());
        let g = "Motherboard (simulated)";
        let sensors = vec![
            SensorDesc::new(ids::CPU_TEMP, "CPU (Tctl/Tdie)", "CPU", SensorKind::Temperature),
            SensorDesc::new("cpu.ccd1", "CCD1 (Tdie)", "CPU", SensorKind::Temperature),
            SensorDesc::new(ids::CPU_POWER, "CPU Package", "CPU", SensorKind::Power),
            SensorDesc::new(ids::CPU_LOAD, "CPU Total", "CPU", SensorKind::Load),
            SensorDesc::new(ids::MEM_LOAD, "Memory", "System", SensorKind::Load),
            SensorDesc::new("mb.t0", "System", g, SensorKind::Temperature),
            SensorDesc::new("mb.t1", "Chipset", g, SensorKind::Temperature),
            SensorDesc::new("mb.t4", "VRM MOS", g, SensorKind::Temperature),
            SensorDesc::new("mb.fan0", "CPU_FAN", g, SensorKind::Fan),
            SensorDesc::new("mb.fan1", "SYS_FAN1", g, SensorKind::Fan),
            SensorDesc::new("mb.fan2", "SYS_FAN2", g, SensorKind::Fan),
            SensorDesc::new("mb.duty0", "CPU_FAN", g, SensorKind::Duty),
            SensorDesc::new("mb.duty1", "SYS_FAN1", g, SensorKind::Duty),
            SensorDesc::new("mb.duty2", "SYS_FAN2", g, SensorKind::Duty),
            SensorDesc::new("mb.v0", "Vcore", g, SensorKind::Voltage),
            SensorDesc::new("mb.v2", "+12V", g, SensorKind::Voltage),
            SensorDesc::new(ids::GPU_TEMP, "GPU Core", "Simulated GPU", SensorKind::Temperature),
            SensorDesc::new(ids::GPU_LOAD, "GPU Load", "Simulated GPU", SensorKind::Load),
        ];
        let mk = |id: &str, idx: usize, name: &str, role: FanRole| FanChannelDesc {
            id: id.into(),
            default_name: name.into(),
            rpm_sensor: Some(format!("mb.fan{idx}")),
            controllable: true,
            role,
            present: true,
            tach_detected: true,
            documented: Some(true),
        };
        let fans_desc = vec![
            mk("mb.pwm0", 0, "CPU_FAN", FanRole::Cpu),
            mk("mb.pwm1", 1, "SYS_FAN1", FanRole::System),
            mk("mb.pwm2", 2, "SYS_FAN2", FanRole::System),
        ];
        let fans = vec![
            SimFan { max_rpm: 1850.0, stall_duty: 14.0, ctl: SimCtl::Bios, out: None, rpm: 900.0 },
            SimFan { max_rpm: 1450.0, stall_duty: 18.0, ctl: SimCtl::Bios, out: None, rpm: 700.0 },
            SimFan { max_rpm: 1450.0, stall_duty: 18.0, ctl: SimCtl::Bios, out: None, rpm: 700.0 },
        ];
        SimHw {
            info,
            sensors,
            fans_desc,
            fans,
            rng: Rng::new(0x5EED_1234),
            t: 0.0,
            dt,
            ambient: 25.0,
            heatsink: 33.0,
            gpu_temp: 36.0,
            case_temp: 30.0,
            cpu_power: 22.0,
            gpu_load: 2.0,
            phase: Phase::Idle,
            phase_left: 40.0,
            burst_left: 0.0,
            failsafe: DEFAULT_FAILSAFE,
            no_tach: [false; 3],
            cooler_failed: false,
            pinned_power: None,
        }
    }

    fn bios_curve(t: f32) -> f32 {
        // A typical "Normal" Smart Fan curve.
        if t < 40.0 {
            30.0
        } else if t < 75.0 {
            30.0 + (t - 40.0) * 2.0
        } else {
            100.0
        }
    }

    fn step_workload(&mut self) {
        self.phase_left -= self.dt;
        if self.phase_left <= 0.0 {
            let r = self.rng.next_f32();
            self.phase = match self.phase {
                Phase::Idle => {
                    if r < 0.5 {
                        Phase::Desktop
                    } else if r < 0.85 {
                        Phase::Gaming
                    } else {
                        Phase::Render
                    }
                }
                _ => {
                    if r < 0.6 {
                        Phase::Idle
                    } else {
                        Phase::Desktop
                    }
                }
            };
            self.phase_left = match self.phase {
                Phase::Idle => self.rng.range(40.0, 120.0),
                Phase::Desktop => self.rng.range(60.0, 150.0),
                Phase::Gaming => self.rng.range(180.0, 420.0),
                Phase::Render => self.rng.range(90.0, 240.0),
            };
        }
        let (base, gpu) = match self.phase {
            Phase::Idle => (21.0, 2.0),
            Phase::Desktop => (28.0, 8.0),
            Phase::Gaming => (58.0 + 8.0 * (self.t * 0.05).sin(), 92.0),
            Phase::Render => (86.0, 15.0),
        };
        // Short bursts (app launches, background tasks).
        if self.burst_left > 0.0 {
            self.burst_left -= self.dt;
        } else if self.phase != Phase::Render && self.rng.next_f32() < 0.03 {
            self.burst_left = self.rng.range(1.0, 6.0);
        }
        let burst = if self.burst_left > 0.0 { 40.0 } else { 0.0 };
        let target = self.pinned_power.unwrap_or((base + burst + self.rng.range(-2.0, 2.0)).min(88.0));
        self.cpu_power += (target - self.cpu_power) * 0.6;
        self.gpu_load += (gpu - self.gpu_load) * 0.3;
    }

    fn airflow(&self, i: usize) -> f32 {
        self.fans[i].rpm / self.fans[i].max_rpm
    }

    fn step_physics(&mut self) {
        let dt = self.dt;
        // Fans spin toward their target RPM.
        let cpu_temp_est = self.heatsink + 0.33 * self.cpu_power;
        for i in 0..self.fans.len() {
            let duty = match &self.fans[i].ctl {
                SimCtl::Bios => Self::bios_curve(cpu_temp_est),
                SimCtl::Curve { input, curve } => curve.duty_at(self.chip_temp(*input).trunc()),
            };
            self.fans[i].out = matches!(self.fans[i].ctl, SimCtl::Curve { .. }).then_some(duty);
            let target = self.fans[i].target_rpm(duty);
            let rpm = self.fans[i].rpm;
            self.fans[i].rpm = rpm + (target - rpm) * (1.0 - (-dt / 1.5).exp());
        }
        let case_air = 0.5 * (self.airflow(1) + self.airflow(2));
        // Case air warms with total heat and cools with case airflow.
        let gpu_power = 15.0 + 160.0 * self.gpu_load / 100.0;
        let case_target = self.ambient + (self.cpu_power + gpu_power) * 0.045 / (0.35 + case_air);
        self.case_temp += (case_target - self.case_temp) * (1.0 - (-dt / 60.0).exp());
        // CPU heatsink: C dT/dt = P - G(u) (T - Tcase)
        let g = if self.cooler_failed { 0.3 } else { 1.1 + 4.2 * self.airflow(0).powf(0.8) };
        let c = 420.0;
        self.heatsink += dt * (self.cpu_power - g * (self.heatsink - self.case_temp)) / c;
        // GPU with its own fan curve; depends on case air.
        let gpu_target = self.case_temp + gpu_power * 0.22;
        self.gpu_temp += (gpu_target - self.gpu_temp) * (1.0 - (-dt / 25.0).exp());
    }
}

impl HwBackend for SimHw {
    fn info(&self) -> &HwInfo {
        &self.info
    }

    fn sensors(&self) -> &[SensorDesc] {
        &self.sensors
    }

    fn fans(&self) -> &[FanChannelDesc] {
        &self.fans_desc
    }

    fn poll(&mut self) -> Vec<Option<f32>> {
        self.t += self.dt;
        self.step_workload();
        self.step_physics();
        let noise = |r: &mut Rng, a: f32| r.range(-a, a);
        let tdie = self.heatsink + 0.33 * self.cpu_power + noise(&mut self.rng, 0.4);
        let cpu_load = ((self.cpu_power - 18.0) / 70.0 * 100.0).clamp(1.0, 100.0);
        let duty = |f: &SimFan| f.out;
        vec![
            Some(tdie),
            Some(tdie - 1.5),
            Some(self.cpu_power),
            Some(cpu_load),
            Some(41.0),
            Some(self.chip_temp(0)),
            Some(self.chip_temp(1)),
            Some(self.chip_temp(4)),
            Some(if self.no_tach[0] { 0.0 } else { self.fans[0].rpm }),
            Some(if self.no_tach[1] { 0.0 } else { self.fans[1].rpm }),
            Some(if self.no_tach[2] { 0.0 } else { self.fans[2].rpm }),
            duty(&self.fans[0]),
            duty(&self.fans[1]),
            duty(&self.fans[2]),
            Some(1.05 + self.cpu_power * 0.002),
            Some(12.1),
            Some(self.gpu_temp + noise(&mut self.rng, 0.3)),
            Some(self.gpu_load),
        ]
    }

    /// A flat curve the simulated chip runs, ramping to 100 % at the failsafe temperature.
    fn set_fan(&mut self, fan: usize, duty_pct: f32) -> Result<()> {
        let (input, at) = self.failsafe;
        let Some(f) = self.fans.get_mut(fan) else {
            return Err(err(ErrorCode::NoSuchFan, format!("fan {fan} does not exist")));
        };
        f.ctl = SimCtl::Curve { input, curve: sg::flat(duty_pct, at) };
        Ok(())
    }

    fn set_fan_curve(&mut self, fan: usize, curve: &HwCurve) -> Result<()> {
        if fan >= self.fans.len() {
            return Err(err(ErrorCode::NoSuchFan, format!("fan {fan} does not exist")));
        }
        if curve.temp_input > 5 {
            return Err(err(ErrorCode::Internal, format!("invalid temperature input {}", curve.temp_input)));
        }
        let mut fitted =
            sg::fit(&curve.points, curve.full_speed_at, sg::MAX_SEGMENTS).map_err(|e| err(ErrorCode::Internal, e))?;
        fitted.dt = curve.delta_t;
        self.fans[fan].ctl = SimCtl::Curve { input: curve.temp_input, curve: fitted };
        Ok(())
    }

    fn set_failsafe(&mut self, temp_input: u8, full_speed_at: f32) {
        if temp_input <= 5 {
            let t = if full_speed_at.is_finite() {
                full_speed_at.clamp(40.0, sg::MAX_TFULL as f32)
            } else {
                DEFAULT_FAILSAFE.1
            };
            self.failsafe = (temp_input, t);
        }
    }

    fn chip_temp_input(&self, sensor_id: &str) -> Option<u8> {
        match sensor_id {
            "mb.t0" => Some(0),
            "mb.t1" => Some(1),
            ids::CPU_TEMP => Some(2),
            "mb.t4" => Some(4),
            _ => None,
        }
    }

    fn has_hw_failsafe(&self, fan: usize) -> bool {
        fan < self.fans.len()
    }

    fn release_fan(&mut self, fan: usize) -> Result<()> {
        let Some(f) = self.fans.get_mut(fan) else {
            return Err(err(ErrorCode::NoSuchFan, format!("fan {fan} does not exist")));
        };
        f.ctl = SimCtl::Bios;
        f.out = None;
        Ok(())
    }

    fn release_all(&mut self) -> Vec<usize> {
        for f in &mut self.fans {
            f.ctl = SimCtl::Bios;
            f.out = None;
        }
        Vec::new()
    }

    fn crash_snapshot(&self) -> Option<CrashSnapshot> {
        None
    }

    fn reapply(&mut self) -> Result<usize> {
        Ok(self.fans.iter().filter(|f| matches!(f.ctl, SimCtl::Curve { .. })).count())
    }

    fn estimated_duty(&self, fan: usize) -> Option<f32> {
        let f = self.fans.get(fan)?;
        matches!(f.ctl, SimCtl::Bios).then(|| Self::bios_curve(self.heatsink + 0.33 * self.cpu_power))
    }
}

impl SimHw {
    /// Temperature inputs of the simulated IT8689E (GIGABYTE AM5 layout); 3 and 5 are not wired.
    fn chip_temp(&self, input: u8) -> f32 {
        match input {
            0 => self.case_temp + 2.0,
            1 => self.case_temp + 8.0,
            2 => self.heatsink + 0.33 * self.cpu_power,
            4 => self.case_temp + self.cpu_power * 0.2,
            _ => -128.0,
        }
    }

    /// Test hook: forces the CPU heatsink temperature.
    #[cfg(test)]
    pub fn set_heatsink(&mut self, t: f32) {
        self.heatsink = t;
    }

    /// Test hook: the channel description (present / tach / role) as the engine sees it.
    #[cfg(test)]
    pub fn fan_desc_mut(&mut self, fan: usize) -> &mut FanChannelDesc {
        &mut self.fans_desc[fan]
    }

    /// Test hook: fan `fan` keeps spinning but its header reports no tach (count 0xFFFF → 0 RPM),
    /// like CPU_FAN on the reference PC. The demo uses it for CPU_FAN.
    pub fn without_tach(mut self, fan: usize) -> Self {
        self.no_tach[fan] = true;
        self.fans_desc[fan].tach_detected = false;
        self
    }

    /// Test hook: the CPU cooler's forced convection stops (pump dead / CPU fan stalled).
    #[cfg(test)]
    pub fn fail_cooler(&mut self) {
        self.cooler_failed = true;
    }

    /// Test hook: a steady CPU package power instead of the synthetic workload.
    #[cfg(test)]
    pub fn pin_cpu_power(&mut self, w: f32) {
        self.pinned_power = Some(w);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_fan_runs_flat_curve_with_failsafe() {
        let mut hw = SimHw::new(1.0);
        assert!(hw.has_hw_failsafe(0));
        hw.set_fan(0, 40.0).unwrap();
        let v = hw.poll();
        let duty = v[11].unwrap();
        assert!((duty - 40.0).abs() < 0.3, "duty {duty}");
        // Way above the failsafe temperature the simulated chip runs 100 % on its own.
        hw.set_heatsink(120.0);
        let v = hw.poll();
        assert_eq!(v[11], Some(100.0));
        hw.release_fan(0).unwrap();
        assert_eq!(hw.poll()[11], None);
        assert_eq!(code_of_err(hw.set_fan(9, 50.0)), ErrorCode::NoSuchFan);
    }

    #[test]
    fn set_fan_curve_follows_points() {
        let mut hw = SimHw::new(1.0);
        let input = hw.chip_temp_input(ids::CPU_TEMP).unwrap();
        let curve =
            HwCurve { temp_input: input, points: vec![(0.0, 30.0), (100.0, 30.0)], full_speed_at: 95.0, delta_t: None };
        hw.set_fan_curve(1, &curve).unwrap();
        hw.set_heatsink(30.0);
        let v = hw.poll();
        assert!((v[12].unwrap() - 30.0).abs() < 0.3);
        assert_eq!(hw.reapply().unwrap(), 1);
        assert!(hw.chip_temp_input("mb.t3").is_none());
    }

    fn code_of_err(r: Result<()>) -> ErrorCode {
        crate::error::code_of(&r.unwrap_err())
    }
}
