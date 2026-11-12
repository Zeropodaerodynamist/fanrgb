//! Hardware access: real (PawnIO + D3DKMT + PDH) and simulated backends.

pub mod amd;
pub mod board;
pub mod filter;
pub mod gpu;
pub mod load;
pub mod pawnio;
pub mod sim;
pub mod superio;
pub mod winmutex;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, ErrorCode, err};
use crate::model::{FanChannelDesc, SensorDesc, SensorKind, ids};
use board::BoardInfo;
use pawnio::{PawnIo, PawnIoStatus};
use superio::ite::{IteChip, IteModel, IteReadings, SavedPwm};
use superio::sg;
use winmutex::GlobalMutex;

#[derive(Clone, Debug, Default)]
pub struct HwInfo {
    pub board: String,
    pub cpu: String,
    pub elevated: bool,
    pub pawnio: PawnIoStatus,
    pub pawnio_version: Option<String>,
    pub chips: Vec<String>,
    pub cpu_temp_source: String,
    pub gpus: Vec<String>,
    pub report: Vec<String>,
    pub simulated: bool,
    pub fan_control: bool,
}

/// Firmware fan state persisted while we control fans, so the watchdog (or the next
/// launch) can hand control back to the BIOS after a crash.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CrashSnapshot {
    /// Wall clock of the boot (fallback only: it moves when the clock is corrected).
    pub boot_time: u64,
    pub pid: u32,
    pub chips: Vec<ChipSnapshot>,
    /// Windows' boot counter when written (clock-independent boot identity).
    #[serde(default)]
    pub boot_id: Option<u32>,
    /// Milliseconds since boot when written (monotonic within a boot).
    #[serde(default)]
    pub uptime_ms: Option<u64>,
}

impl CrashSnapshot {
    /// A snapshot of `chips` stamped with this process and the current boot.
    pub fn now(chips: Vec<ChipSnapshot>) -> Self {
        let b = crate::system::boot_stamp_now();
        CrashSnapshot {
            boot_time: crate::util::win::boot_time_unix(),
            pid: std::process::id(),
            chips,
            boot_id: b.boot_id,
            uptime_ms: b.uptime_ms,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChipSnapshot {
    pub model: IteModel,
    pub base: u16,
    /// Per channel: PWM control value and every curve register we may modify (banks 0, 2, 3).
    pub channels: Vec<(usize, SavedPwm)>,
}

/// A fan curve executed by the Super I/O chip itself, so it keeps working if the app hangs,
/// crashes or is closed (this is how GIGABYTE's own software controls fans).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HwCurve {
    /// Chip temperature input that drives the curve (see [`HwBackend::chip_temp_input`];
    /// on Gigabyte AM5 boards 0 = System 1, 1 = PCH, 2 = CPU, 3 = PCIEX16, 4 = VRM MOS, 5 = VSOC MOS).
    pub temp_input: u8,
    /// (°C, duty %) points, ascending in temperature. The backend fits them to what the chip supports
    /// (up to 10 linear segments on IT8689E; duties are made non-decreasing).
    pub points: Vec<(f32, f32)>,
    /// At or above this temperature the chip runs the fan at 100 % regardless of the points
    /// (clamped to 1..=100 °C).
    pub full_speed_at: f32,
    /// SmartGuardian hysteresis ΔT in °C (0..=3); `None` keeps the firmware's value.
    #[serde(default)]
    pub delta_t: Option<u8>,
}

/// Default hardware failsafe of `set_fan`: CPU input (2 on GIGABYTE boards) at 85 °C.
pub const DEFAULT_FAILSAFE: (u8, f32) = (2, 85.0);

pub trait HwBackend: Send {
    fn info(&self) -> &HwInfo;
    fn sensors(&self) -> &[SensorDesc];
    fn fans(&self) -> &[FanChannelDesc];
    /// Reads all sensors; values are aligned with `sensors()`. Temperatures pass a plausibility
    /// filter (impossible values and one-sample jumps > 30 °C read as `None`).
    fn poll(&mut self) -> Vec<Option<f32>>;
    /// Runs `fan` at `duty_pct`. Backends that can should implement this as a flat hardware curve with
    /// a hardware failsafe ramp to 100 % (see [`HwBackend::set_failsafe`]), not as raw manual PWM.
    fn set_fan(&mut self, fan: usize, duty_pct: f32) -> Result<()>;
    /// Hands `fan` a curve the chip runs on its own. Errors with `FanReadOnly` if unsupported.
    fn set_fan_curve(&mut self, fan: usize, curve: &HwCurve) -> Result<()> {
        let _ = curve;
        Err(crate::error::err(
            crate::error::ErrorCode::FanReadOnly,
            format!("fan {fan}: hardware curves not supported"),
        ))
    }
    /// Chip temperature input and threshold used for the hardware failsafe of `set_fan`
    /// (the chip forces 100 % at or above it even if the app stops). Default: no-op.
    fn set_failsafe(&mut self, temp_input: u8, full_speed_at: f32) {
        let _ = (temp_input, full_speed_at);
    }
    /// The chip temperature input behind sensor `sensor_id`, if a hardware curve can use it.
    fn chip_temp_input(&self, sensor_id: &str) -> Option<u8> {
        let _ = sensor_id;
        None
    }
    /// True if `set_fan`/`set_fan_curve` keep a hardware failsafe active for this fan.
    fn has_hw_failsafe(&self, fan: usize) -> bool {
        let _ = fan;
        false
    }
    /// False if chip temperature input `input` of the chip that owns `fan` currently reads an
    /// invalid value (e.g. -128 °C: thermistor missing), so a hardware curve on it would be stuck.
    fn chip_input_valid(&self, fan: usize, input: u8) -> bool {
        let _ = (fan, input);
        true
    }
    /// Whether `set_fan` may fall back to software PWM (no hardware failsafe) when a fan's hardware
    /// curve cannot be written. The engine allows it only while crash protection (watchdog +
    /// persisted snapshot) is active. Default: no-op.
    fn set_software_fallback(&mut self, allowed: bool) {
        let _ = allowed;
    }
    fn release_fan(&mut self, fan: usize) -> Result<()>;
    /// Hands every fan back to the BIOS. Returns the fans whose release could **not** be verified
    /// (they are still held; retry with `release_fan`).
    fn release_all(&mut self) -> Vec<usize>;
    fn crash_snapshot(&self) -> Option<CrashSnapshot>;
    fn diagnostics_dump(&mut self) -> String {
        String::new()
    }
    /// Re-writes every fan setting we own (call ~2 s after resume from sleep: the BIOS reprograms
    /// the chip). Firmware overrides are also detected and repaired during `poll`. Returns the
    /// number of fans re-applied.
    fn reapply(&mut self) -> Result<usize> {
        Ok(0)
    }
    /// Notable hardware events since the last call (firmware overrides, failed register
    /// verifications, fans handed back to the BIOS), for alerts.
    fn take_events(&mut self) -> Vec<AppError> {
        Vec::new()
    }
    /// For a fan the BIOS controls: the duty its firmware curve gives now, estimated from the
    /// decoded curve and the current input temperature (`None`: unknown or not BIOS-controlled).
    fn estimated_duty(&self, fan: usize) -> Option<f32> {
        let _ = fan;
        None
    }
}

// ---------------------------------------------------------------------------------------

enum Src {
    CpuTemp,
    CpuCcd(usize),
    CpuPower,
    CpuLoad,
    MemLoad,
    ChipTemp(usize, usize),
    ChipFan(usize, usize),
    ChipDuty(usize, usize),
    ChipVolt(usize, usize, f32, f32),
    GpuTemp(usize),
    GpuLoad(usize),
    GpuFan(usize),
    GpuPower(usize),
}

pub struct RealHw {
    info: HwInfo,
    sensors: Vec<SensorDesc>,
    plan: Vec<Src>,
    fans: Vec<FanChannelDesc>,
    fan_map: Vec<(usize, usize)>,
    isa: GlobalMutex,
    pci: GlobalMutex,
    chips: Vec<IteChip>,
    chip_cpu_temp: Option<(usize, usize)>,
    amd: Option<amd::AmdCpu>,
    gpu: gpu::GpuMonitor,
    cpu_load: load::CpuLoad,
    last_chip: Vec<IteReadings>,
    last_amd: amd::AmdReadings,
    last_gpu: Vec<gpu::GpuReading>,
    chip_ok_at: std::time::Instant,
    amd_ok_at: std::time::Instant,
    /// One plausibility filter per sensor (used for temperatures).
    filters: Vec<filter::TempFilter>,
    /// Sensor id → temperature input of chip 0 (only when chip 0 runs hardware curves).
    temp_inputs: Vec<(String, u8)>,
    /// Failsafe of `set_fan`: (chip-0 temperature input, temperature).
    failsafe: (u8, f32),
    /// CPU temperature input of every chip (`None`: unknown), so chips other than chip 0 get a
    /// failsafe on their own input, not on chip 0's input number.
    chip_cpu_inputs: Vec<Option<u8>>,
    /// Health of each chip's failsafe input (a failsafe on an input reading -128 °C never triggers).
    failsafe_health: Vec<filter::InputHealth>,
    /// Consecutive ISA lock timeouts on writes.
    lock_failures: u32,
    events: Vec<AppError>,
}

/// Readings older than this are discarded so the safety layer notices sensor loss.
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(5);
/// A tachometer reading in this range means a fan is connected.
const PLAUSIBLE_RPM: std::ops::RangeInclusive<f32> = 150.0..=12000.0;
/// Lock timeouts after which a 100 % (critical) write bypasses the cooperative ISA lock.
const BYPASS_AFTER: u32 = 3;

impl RealHw {
    pub fn new() -> Self {
        let board = BoardInfo::read();
        let elevated = crate::util::win::is_elevated();
        let isa = GlobalMutex::new(winmutex::ISA_BUS);
        let pci = GlobalMutex::new(winmutex::PCI_BUS);
        let cpu_id = amd::cpuid();

        let mut info = HwInfo {
            board: board.display(),
            cpu: if cpu_id.brand.is_empty() { cpu_id.vendor.clone() } else { cpu_id.brand.clone() },
            elevated,
            pawnio_version: PawnIo::installed_version(),
            ..Default::default()
        };
        info.report.push(format!("Board: {} (BIOS {})", info.board, board.bios));
        info.report.push(format!("CPU: {} (family {:#x}, model {:#x})", info.cpu, cpu_id.family, cpu_id.model));
        info.report.push(format!(
            "Elevated: {elevated}; PawnIO installed: {}",
            info.pawnio_version.clone().unwrap_or_else(|| "no".into())
        ));

        let det = superio::detect(&isa, board.is_gigabyte());
        info.pawnio = det.status.clone();
        info.report.extend(det.report.iter().cloned());
        let mut chips = det.chips;
        for (ci, c) in chips.iter_mut().enumerate() {
            c.set_id_prefix(&if ci == 0 { "mb".to_string() } else { format!("mb{}", ci + 1) });
        }
        // One reading before building the layout: which temperature inputs are wired and which
        // headers have a fan.
        let initial: Vec<Option<IteReadings>> = {
            let guard = isa.lock(500);
            chips.iter_mut().map(|c| if guard.is_some() { c.update().ok() } else { None }).collect()
        };

        let amd = if AmdSupported::check(&cpu_id) && info.pawnio == PawnIoStatus::Ready {
            match amd::AmdCpu::open(cpu_id.clone()) {
                Ok(c) => Some(c),
                Err(e) => {
                    info.report.push(format!(
                        "[{}] AMD SMN access unavailable: {}",
                        ErrorCode::CpuSensor,
                        e.describe()
                    ));
                    None
                }
            }
        } else {
            None
        };

        let gpu = gpu::GpuMonitor::new();
        for a in gpu.adapters() {
            info.gpus.push(a.name.clone());
        }

        let mut hw = RealHw {
            info,
            sensors: Vec::new(),
            plan: Vec::new(),
            fans: Vec::new(),
            fan_map: Vec::new(),
            isa,
            pci,
            chips,
            chip_cpu_temp: None,
            amd,
            gpu,
            cpu_load: load::CpuLoad::default(),
            last_chip: Vec::new(),
            last_amd: amd::AmdReadings::default(),
            last_gpu: Vec::new(),
            chip_ok_at: std::time::Instant::now(),
            amd_ok_at: std::time::Instant::now(),
            filters: Vec::new(),
            temp_inputs: Vec::new(),
            failsafe: DEFAULT_FAILSAFE,
            chip_cpu_inputs: Vec::new(),
            failsafe_health: Vec::new(),
            lock_failures: 0,
            events: Vec::new(),
        };
        hw.build_layout(&board, &initial);
        hw
    }

    fn build_layout(&mut self, board: &BoardInfo, initial: &[Option<IteReadings>]) {
        let push = |s: &mut Vec<SensorDesc>, p: &mut Vec<Src>, d: SensorDesc, src: Src| {
            s.push(d);
            p.push(src);
        };
        let (mut sensors, mut plan) = (Vec::new(), Vec::new());

        // CPU
        let cpu_group = "CPU";
        let have_smn = self.amd.is_some();
        let mut chip_cpu = None;
        if let Some(chip) = self.chips.first() {
            let labels = board::labels_for(board, chip.model, chip.caps.temps, chip.caps.fans, chip.caps.volts);
            chip_cpu = labels.cpu_temp_index.map(|i| (0, i));
            if let Some((_, i)) = chip_cpu {
                self.failsafe.0 = i as u8;
            }
        }
        self.chip_cpu_temp = chip_cpu;
        let hw_curves0 = self.chips.first().is_some_and(|c| c.hw_curves());
        if have_smn || chip_cpu.is_some() {
            let name = if have_smn { "CPU (Tctl/Tdie)" } else { "CPU" };
            push(
                &mut sensors,
                &mut plan,
                SensorDesc::new(ids::CPU_TEMP, name, cpu_group, SensorKind::Temperature),
                Src::CpuTemp,
            );
            self.info.cpu_temp_source = if have_smn { "AMD SMN (Tctl)".into() } else { "Super I/O".into() };
            if let (true, Some((_, i))) = (hw_curves0, chip_cpu) {
                self.temp_inputs.push((ids::CPU_TEMP.to_string(), i as u8));
            }
        } else {
            self.info.cpu_temp_source = "unavailable".into();
        }
        if let Some(amd) = &mut self.amd {
            if let Ok(r) = amd.read(&self.pci) {
                for (i, _) in r.ccds.iter().enumerate() {
                    push(
                        &mut sensors,
                        &mut plan,
                        SensorDesc::new(
                            format!("cpu.ccd{}", i + 1),
                            format!("CCD{} (Tdie)", i + 1),
                            cpu_group,
                            SensorKind::Temperature,
                        ),
                        Src::CpuCcd(i),
                    );
                }
            }
            push(
                &mut sensors,
                &mut plan,
                SensorDesc::new(ids::CPU_POWER, "CPU Package", cpu_group, SensorKind::Power),
                Src::CpuPower,
            );
        }
        push(
            &mut sensors,
            &mut plan,
            SensorDesc::new(ids::CPU_LOAD, "CPU Total", cpu_group, SensorKind::Load),
            Src::CpuLoad,
        );
        push(
            &mut sensors,
            &mut plan,
            SensorDesc::new(ids::MEM_LOAD, "Memory", "System", SensorKind::Load),
            Src::MemLoad,
        );

        // Motherboard
        for (ci, chip) in self.chips.iter().enumerate() {
            let prefix = if ci == 0 { "mb".to_string() } else { format!("mb{}", ci + 1) };
            let group = if ci == 0 {
                format!("Motherboard ({})", chip.model.name())
            } else {
                format!("Motherboard #2 ({})", chip.model.name())
            };
            let labels = board::labels_for(board, chip.model, chip.caps.temps, chip.caps.fans, chip.caps.volts);
            self.chip_cpu_inputs
                .push(labels.cpu_temp_index.filter(|&i| i <= 5 && i < chip.caps.temps).map(|i| i as u8));
            self.failsafe_health.push(filter::InputHealth::default());
            let first = initial.get(ci).and_then(|r| r.as_ref());
            for (i, n) in labels.temps.iter().enumerate() {
                let is_cpu = labels.cpu_temp_index == Some(i);
                let raw = first.and_then(|r| r.temps.get(i).copied().flatten());
                let unwired = raw.is_some_and(|t| !filter::in_range(t));
                if !is_cpu && (labels.hidden_temps.contains(&i) || unwired) {
                    self.info.report.push(format!(
                        "{} temperature input {i} ({n}) not wired on this board{}; hidden",
                        chip.model.name(),
                        raw.map(|t| format!(" (reads {t})")).unwrap_or_default()
                    ));
                    continue;
                }
                let id = format!("{prefix}.t{i}");
                if ci == 0 && hw_curves0 && i <= 5 {
                    self.temp_inputs.push((id.clone(), i as u8));
                }
                push(
                    &mut sensors,
                    &mut plan,
                    SensorDesc::new(id, n.clone(), group.clone(), SensorKind::Temperature),
                    Src::ChipTemp(ci, i),
                );
            }
            for (i, (n, role)) in labels.fans.iter().enumerate() {
                let rpm_id = format!("{prefix}.fan{i}");
                push(
                    &mut sensors,
                    &mut plan,
                    SensorDesc::new(rpm_id.clone(), n.clone(), group.clone(), SensorKind::Fan),
                    Src::ChipFan(ci, i),
                );
                if i < chip.caps.pwms {
                    let documented = labels.documented_headers.as_ref().map(|h| h.contains(&i));
                    let tach_detected = first
                        .and_then(|r| r.fans.get(i).copied().flatten())
                        .is_some_and(|rpm| PLAUSIBLE_RPM.contains(&rpm));
                    let present =
                        documented == Some(true) || tach_detected || (documented.is_none() && chip.fan_present(i));
                    self.fans.push(FanChannelDesc {
                        id: format!("{prefix}.pwm{i}"),
                        default_name: n.clone(),
                        rpm_sensor: Some(rpm_id),
                        controllable: chip.controllable(),
                        role: *role,
                        present,
                        tach_detected,
                        documented,
                    });
                    self.fan_map.push((ci, i));
                }
            }
            for i in 0..chip.caps.pwms {
                let fan_name = labels.fans.get(i).map(|f| f.0.clone()).unwrap_or_else(|| format!("PWM {}", i + 1));
                push(
                    &mut sensors,
                    &mut plan,
                    SensorDesc::new(format!("{prefix}.duty{i}"), fan_name, group.clone(), SensorKind::Duty),
                    Src::ChipDuty(ci, i),
                );
            }
            for (i, l) in labels.volts.iter().enumerate() {
                push(
                    &mut sensors,
                    &mut plan,
                    SensorDesc::new(format!("{prefix}.v{i}"), l.name, group.clone(), SensorKind::Voltage),
                    Src::ChipVolt(ci, i, l.ri, l.rf),
                );
            }
            self.info.chips.push(format!(
                "{} @ {:#06x}{}",
                chip.model.name(),
                chip.base,
                if !chip.controllable() {
                    " (read-only)"
                } else if chip.hw_curves() {
                    " (hardware curves)"
                } else {
                    ""
                }
            ));
        }
        self.info.fan_control = self.fans.iter().any(|f| f.controllable);

        // GPUs
        for (gi, a) in self.gpu.adapters().iter().enumerate() {
            let group = a.name.clone();
            push(
                &mut sensors,
                &mut plan,
                SensorDesc::new(format!("gpu{gi}.temp"), "GPU Core", group.clone(), SensorKind::Temperature),
                Src::GpuTemp(gi),
            );
            push(
                &mut sensors,
                &mut plan,
                SensorDesc::new(format!("gpu{gi}.load"), "GPU Load", group.clone(), SensorKind::Load),
                Src::GpuLoad(gi),
            );
            push(
                &mut sensors,
                &mut plan,
                SensorDesc::new(format!("gpu{gi}.fan"), "GPU Fan", group.clone(), SensorKind::Fan),
                Src::GpuFan(gi),
            );
            push(
                &mut sensors,
                &mut plan,
                // Intel reports watts; other vendors the documented percentage of board power.
                SensorDesc::new(
                    format!("gpu{gi}.power"),
                    "GPU Power",
                    group.clone(),
                    if a.power_in_watts() { SensorKind::Power } else { SensorKind::Load },
                ),
                Src::GpuPower(gi),
            );
        }
        self.filters = vec![filter::TempFilter::default(); sensors.len()];
        self.sensors = sensors;
        self.plan = plan;
    }

    fn map(&self, fan: usize) -> Result<(usize, usize)> {
        self.fan_map.get(fan).copied().ok_or_else(|| err(ErrorCode::NoSuchFan, format!("fan {fan} does not exist")))
    }

    fn collect_events(&mut self) {
        for c in &mut self.chips {
            self.events.extend(c.take_events());
        }
    }

    /// Temperature input of chip `ci` that carries the `set_fan` failsafe.
    fn failsafe_input(&self, ci: usize) -> Option<u8> {
        failsafe_input_for(ci, self.failsafe.0, &self.chip_cpu_inputs)
    }

    /// Tracks whether each chip's failsafe input reads plausible values (after a chip poll).
    fn check_failsafe_inputs(&mut self) {
        let now = std::time::Instant::now();
        for ci in 0..self.chips.len().min(self.failsafe_health.len()) {
            let Some(input) = self.failsafe_input(ci) else { continue };
            // No fresh reading (bus busy, update failed): unknown, keep the previous state.
            let Some(r) = self.last_chip.get(ci).filter(|r| !r.temps.is_empty()) else { continue };
            let valid = r.temps.get(input as usize).copied().flatten().is_some_and(filter::in_range);
            if self.failsafe_health[ci].update(valid, now) {
                self.events.push(AppError::new(
                    ErrorCode::SensorLost,
                    format!(
                        "{} temperature input {input} (hardware failsafe) reads invalid values; the chip's own 100 % failsafe cannot trigger",
                        self.chips[ci].model.name()
                    ),
                ));
            }
        }
    }
}

/// The failsafe input for chip `ci`: chip 0 uses the configured input (an input number of chip 0),
/// other chips only their own CPU input (`None`: no hardware failsafe on that chip).
fn failsafe_input_for(ci: usize, chip0_input: u8, chip_cpu_inputs: &[Option<u8>]) -> Option<u8> {
    if ci == 0 { Some(chip0_input) } else { chip_cpu_inputs.get(ci).copied().flatten() }
}

struct AmdSupported;
impl AmdSupported {
    fn check(id: &amd::CpuId) -> bool {
        amd::AmdCpu::supported(id)
    }
}

impl HwBackend for RealHw {
    fn info(&self) -> &HwInfo {
        &self.info
    }

    fn sensors(&self) -> &[SensorDesc] {
        &self.sensors
    }

    fn fans(&self) -> &[FanChannelDesc] {
        &self.fans
    }

    fn poll(&mut self) -> Vec<Option<f32>> {
        if !self.chips.is_empty() {
            if let Some(_g) = self.isa.lock(50) {
                let mut readings = Vec::with_capacity(self.chips.len());
                for chip in &mut self.chips {
                    match chip.update() {
                        Ok(r) => readings.push(r),
                        Err(e) => {
                            log::debug!("{} update failed: {e:#}", chip.model.name());
                            readings.push(Default::default());
                        }
                    }
                }
                self.last_chip = readings;
                self.chip_ok_at = std::time::Instant::now();
            } else if self.chip_ok_at.elapsed() > STALE_AFTER {
                log::warn!(
                    "[{}] Super I/O busy for too long (another tool holds the ISA lock); readings dropped",
                    ErrorCode::BusBusy
                );
                self.last_chip.clear();
            }
            self.check_failsafe_inputs();
            self.collect_events();
        }
        if let Some(amd) = &mut self.amd {
            match amd.read(&self.pci) {
                Ok(r) => {
                    self.last_amd = r;
                    self.amd_ok_at = std::time::Instant::now();
                }
                Err(e) => {
                    log::debug!("AMD read failed: {e:#}");
                    if self.amd_ok_at.elapsed() > STALE_AFTER {
                        self.last_amd = amd::AmdReadings::default();
                    }
                }
            }
        }
        self.last_gpu = self.gpu.read();
        let cpu_load = self.cpu_load.sample();
        let mem = load::memory_load();

        let chip_val =
            |ci: usize, f: &dyn Fn(&IteReadings) -> Option<f32>| -> Option<f32> { self.last_chip.get(ci).and_then(f) };
        let chip_temp = |c: usize, i: usize| chip_val(c, &|r| r.temps.get(i).copied().flatten());
        let mut values: Vec<Option<f32>> =
            self.plan
                .iter()
                .map(|src| match *src {
                    Src::CpuTemp => self.last_amd.tctl.or_else(|| {
                        self.chip_cpu_temp.and_then(|(c, i)| chip_temp(c, i).filter(|t| filter::in_range(*t)))
                    }),
                    Src::CpuCcd(i) => self.last_amd.ccds.get(i).copied().flatten(),
                    Src::CpuPower => self.last_amd.package_power,
                    Src::CpuLoad => cpu_load,
                    Src::MemLoad => mem,
                    Src::ChipTemp(c, i) => chip_temp(c, i),
                    Src::ChipFan(c, i) => chip_val(c, &|r| r.fans.get(i).copied().flatten()),
                    Src::ChipDuty(c, i) => chip_val(c, &|r| r.duty.get(i).copied().flatten()),
                    Src::ChipVolt(c, i, ri, rf) => chip_val(c, &|r| {
                        r.volts.get(i).copied().flatten().map(|v| if rf > 0.0 { v + v * ri / rf } else { v })
                    }),
                    Src::GpuTemp(g) => self.last_gpu.get(g).and_then(|r| r.temp_c),
                    Src::GpuLoad(g) => self.last_gpu.get(g).and_then(|r| r.load_pct),
                    Src::GpuFan(g) => self.last_gpu.get(g).and_then(|r| r.fan_rpm),
                    Src::GpuPower(g) => self.last_gpu.get(g).and_then(|r| r.power_w.or(r.power_pct)),
                })
                .collect();
        for (i, v) in values.iter_mut().enumerate() {
            if self.sensors[i].kind == SensorKind::Temperature {
                *v = self.filters[i].apply(&self.sensors[i].id, *v);
            }
        }
        values
    }

    fn set_fan(&mut self, fan: usize, duty_pct: f32) -> Result<()> {
        let (ci, pi) = self.map(fan)?;
        let critical = !(duty_pct < 99.5);
        let guard = self.isa.lock(100);
        if guard.is_some() {
            self.lock_failures = 0;
        } else {
            self.lock_failures += 1;
            if !(critical && self.lock_failures >= BYPASS_AFTER) {
                return Err(err(ErrorCode::BusBusy, "the Super I/O lock is held by another program"));
            }
            log::warn!(
                "[{}] ISA lock unavailable for {} attempts; writing 100 % without it (thermal safety)",
                ErrorCode::BusBusy,
                self.lock_failures
            );
        }
        let fs = self.failsafe.1;
        let inp = self.failsafe_input(ci).unwrap_or(sg::SAFE_DEFAULT_INPUT);
        let r = self.chips[ci].set_duty(pi, duty_pct, inp, fs);
        drop(guard);
        self.collect_events();
        r
    }

    fn set_fan_curve(&mut self, fan: usize, curve: &HwCurve) -> Result<()> {
        let (ci, pi) = self.map(fan)?;
        let chip = &self.chips[ci];
        if !chip.controllable() || !chip.hw_curves() {
            return Err(err(
                ErrorCode::FanReadOnly,
                format!("fan {fan}: hardware curves not supported on {}", chip.model.name()),
            ));
        }
        if ci != 0 {
            // `temp_input` numbers come from `chip_temp_input`, i.e. chip 0.
            return Err(err(
                ErrorCode::FanReadOnly,
                format!("fan {fan}: hardware curves are only used on the first fan controller"),
            ));
        }
        let mut fitted = sg::fit(&curve.points, curve.full_speed_at, sg::MAX_SEGMENTS)
            .map_err(|e| err(ErrorCode::Internal, format!("fan {fan}: {e}")))?;
        fitted.dt = curve.delta_t;
        let Some(guard) = self.isa.lock(100) else {
            return Err(err(ErrorCode::BusBusy, "the Super I/O lock is held by another program"));
        };
        let r = self.chips[ci].set_curve(pi, curve.temp_input, &fitted);
        drop(guard);
        self.collect_events();
        r
    }

    fn set_failsafe(&mut self, temp_input: u8, full_speed_at: f32) {
        let temps = self.chips.first().map(|c| c.caps.temps).unwrap_or(0);
        if temp_input > 5 || temp_input as usize >= temps {
            log::warn!("ignoring failsafe on nonexistent temperature input {temp_input}");
            return;
        }
        let t = if full_speed_at.is_finite() {
            full_speed_at.clamp(40.0, sg::MAX_TFULL as f32)
        } else {
            DEFAULT_FAILSAFE.1
        };
        if (temp_input, t) != self.failsafe {
            log::info!("hardware failsafe: 100 % at {t:.0} °C on temperature input {temp_input}");
        }
        self.failsafe = (temp_input, t);
    }

    fn chip_temp_input(&self, sensor_id: &str) -> Option<u8> {
        self.temp_inputs.iter().find(|(id, _)| id == sensor_id).map(|(_, i)| *i)
    }

    fn has_hw_failsafe(&self, fan: usize) -> bool {
        let now = std::time::Instant::now();
        self.fan_map.get(fan).is_some_and(|&(ci, pi)| {
            self.chips[ci].has_hw_failsafe(pi)
                && self.failsafe_input(ci).is_some()
                && !self.failsafe_health.get(ci).is_some_and(|h| h.lost(now))
        })
    }

    fn chip_input_valid(&self, fan: usize, input: u8) -> bool {
        let Some(&(ci, _)) = self.fan_map.get(fan) else { return false };
        match self.last_chip.get(ci) {
            Some(r) if !r.temps.is_empty() => {
                r.temps.get(input as usize).copied().flatten().is_some_and(filter::in_range)
            }
            // No fresh reading: unknown (the engine's sensor-loss timer covers long outages).
            _ => true,
        }
    }

    fn set_software_fallback(&mut self, allowed: bool) {
        for c in &mut self.chips {
            c.set_software_fallback(allowed);
        }
    }

    fn release_fan(&mut self, fan: usize) -> Result<()> {
        let (ci, pi) = self.map(fan)?;
        // Handing a fan back to the BIOS is safety-critical: never let another program's lock
        // keep a fan in our hands.
        let guard = self.isa.lock(500);
        if guard.is_none() {
            log::warn!(
                "[{}] ISA lock held by another program; restoring BIOS fan control without it",
                ErrorCode::BusBusy
            );
        }
        let r = self.chips[ci].release(pi);
        drop(guard);
        self.collect_events();
        r
    }

    fn release_all(&mut self) -> Vec<usize> {
        let guard = self.isa.lock(2000);
        if guard.is_none() {
            log::warn!("ISA bus lock timed out while restoring BIOS fan control; restoring anyway");
        }
        let mut held = Vec::new();
        for (fan, &(ci, pi)) in self.fan_map.iter().enumerate() {
            if let Err(e) = self.chips[ci].release(pi) {
                log::error!("[{}] {e:#}", ErrorCode::RestoreFailed);
                held.push(fan);
            }
        }
        drop(guard);
        self.collect_events();
        held
    }

    fn crash_snapshot(&self) -> Option<CrashSnapshot> {
        let chips: Vec<ChipSnapshot> = self
            .chips
            .iter()
            .filter_map(|c| {
                let channels = c.saved_state();
                (!channels.is_empty()).then_some(ChipSnapshot { model: c.model, base: c.base, channels })
            })
            .collect();
        (!chips.is_empty()).then(|| CrashSnapshot::now(chips))
    }

    fn diagnostics_dump(&mut self) -> String {
        use superio::ite::ChanMode;
        let controlled: Vec<usize> = (0..self.fans.len())
            .filter(|&i| self.fan_map.get(i).is_some_and(|&(ci, pi)| self.chips[ci].mode(pi) != ChanMode::Firmware))
            .collect();
        // Printed once: the failsafe the chip carries for controlled channels (set by the engine from
        // `cpu_critical`), not per channel and not the built-in default.
        let mut s = if controlled.is_empty() {
            "Hardware failsafe: none applied (all channels on BIOS control)\n".to_string()
        } else {
            format!(
                "Hardware failsafe (applied to controlled channels): 100 % at {:.0} °C on input {}\n",
                self.failsafe.1, self.failsafe.0
            )
        };
        let head = ["channel", "header", "mode", "duty", "present", "tach", "documented", "failsafe_capable"];
        let mut rows = vec![head.map(String::from).to_vec()];
        for (i, f) in self.fans.iter().enumerate() {
            let Some(&(ci, pi)) = self.fan_map.get(i) else { continue };
            let chip = &self.chips[ci];
            let r = self.last_chip.get(ci);
            let read = r.and_then(|r| r.duty.get(pi).copied().flatten());
            let est = r.and_then(|r| r.duty_est.get(pi).copied().flatten());
            let duty = match (read, est) {
                (Some(d), _) => format!("{d:.0} %"),
                (None, Some(e)) => {
                    let input = chip.firmware_curve(pi).map(|(inp, _)| format!(", input {inp}")).unwrap_or_default();
                    format!("est. {e:.0} % (BIOS curve{input})")
                }
                (None, None) => "-".into(),
            };
            rows.push(vec![
                chip.label(pi),
                f.default_name.clone(),
                format!("{:?}", chip.mode(pi)),
                duty,
                f.present.to_string(),
                f.tach_detected.to_string(),
                f.documented.map(|d| d.to_string()).unwrap_or_else(|| "unknown".into()),
                self.has_hw_failsafe(i).to_string(),
            ]);
        }
        s.push_str(&crate::diag::table(&rows, "  "));
        if let Some(_g) = self.isa.lock(200) {
            for chip in &mut self.chips {
                s.push_str(&format!("== {} @ {:#06x} ==\n", chip.model.name(), chip.base));
                s.push_str(&chip.dump());
            }
        }
        s
    }

    fn reapply(&mut self) -> Result<usize> {
        let Some(guard) = self.isa.lock(500) else {
            return Err(err(ErrorCode::BusBusy, "the Super I/O lock is held by another program"));
        };
        let n: usize = self.chips.iter_mut().map(|c| c.reapply()).sum();
        drop(guard);
        self.collect_events();
        if n > 0 {
            log::info!("re-applied {n} fan setting(s)");
        }
        Ok(n)
    }

    fn take_events(&mut self) -> Vec<AppError> {
        std::mem::take(&mut self.events)
    }

    fn estimated_duty(&self, fan: usize) -> Option<f32> {
        let &(ci, pi) = self.fan_map.get(fan)?;
        if self.chips[ci].mode(pi) != superio::ite::ChanMode::Firmware {
            return None;
        }
        self.last_chip.get(ci).and_then(|r| r.duty_est.get(pi).copied().flatten())
    }
}

impl Drop for RealHw {
    fn drop(&mut self) {
        let held = self.release_all();
        if !held.is_empty() {
            log::error!(
                "[{}] fans {held:?} could not be handed back to the BIOS; the crash snapshot is kept for the watchdog",
                ErrorCode::RestoreFailed
            );
        }
    }
}

/// Restores BIOS fan control from a crash snapshot (watchdog / next start).
pub fn restore_crash_snapshot(snap: &CrashSnapshot) -> Result<usize> {
    let board = BoardInfo::read();
    let isa = GlobalMutex::new(winmutex::ISA_BUS);
    let mut det = superio::detect(&isa, board.is_gigabyte());
    if let Some(code) = det.status.code() {
        return Err(err(code, format!("cannot access hardware: {}", det.status.describe())));
    }
    if det.chips.is_empty() {
        return Err(err(ErrorCode::NoSuperIo, "no fan controller found to restore"));
    }
    let guard = isa.lock(2000);
    if guard.is_none() {
        log::warn!("ISA bus lock timed out during crash recovery; restoring anyway");
    }
    let mut restored = 0;
    let mut first_err = None;
    for cs in &snap.chips {
        match det.chips.iter_mut().find(|c| c.model == cs.model && c.base == cs.base) {
            Some(chip) => match chip.restore_saved(&cs.channels) {
                Ok(n) => restored += n,
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            },
            None => log::warn!("crash snapshot names {} @ {:#06x}, which was not found", cs.model.name(), cs.base),
        }
    }
    drop(guard);
    match first_err {
        Some(e) => Err(err(ErrorCode::RestoreFailed, format!("{e:#}"))),
        None => Ok(restored),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Chip 1 never gets chip 0's input number as its failsafe input.
    #[test]
    fn failsafe_input_is_per_chip() {
        let inputs = [Some(2), Some(0), None];
        assert_eq!(failsafe_input_for(0, 2, &inputs), Some(2));
        assert_eq!(failsafe_input_for(1, 2, &inputs), Some(0), "chip 1 uses its own CPU input");
        assert_eq!(failsafe_input_for(2, 2, &inputs), None, "unknown CPU input: no hardware failsafe");
        assert_eq!(failsafe_input_for(3, 2, &inputs), None);
    }

    /// The snapshot carries a clock-independent boot identity; old files load.
    #[test]
    fn crash_snapshot_carries_boot_stamp() {
        let s = CrashSnapshot::now(Vec::new());
        assert_eq!(s.pid, std::process::id());
        assert!(s.uptime_ms.is_some_and(|u| u > 0));
        let b = crate::system::boot_stamp_now();
        assert_eq!(s.boot_id, b.boot_id);
        let json = serde_json::to_value(&s).unwrap();
        assert!(json.get("boot_id").is_some() && json.get("uptime_ms").is_some(), "{json}");
        let old: CrashSnapshot = serde_json::from_str(r#"{"boot_time":1000,"pid":7,"chips":[]}"#).unwrap();
        assert_eq!((old.boot_id, old.uptime_ms), (None, None));
    }
}
