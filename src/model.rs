//! Types shared between the hardware layer, the control engine and the UI.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SensorKind {
    Temperature,
    Fan,
    Voltage,
    Load,
    Power,
    Duty,
}

impl SensorKind {
    pub fn unit(self) -> &'static str {
        match self {
            SensorKind::Temperature => "°C",
            SensorKind::Fan => "RPM",
            SensorKind::Voltage => "V",
            SensorKind::Load | SensorKind::Duty => "%",
            SensorKind::Power => "W",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SensorDesc {
    pub id: String,
    pub name: String,
    pub group: String,
    pub kind: SensorKind,
}

impl SensorDesc {
    pub fn new(id: impl Into<String>, name: impl Into<String>, group: impl Into<String>, kind: SensorKind) -> Self {
        Self { id: id.into(), name: name.into(), group: group.into(), kind }
    }
}

/// How the engine treats a header (from the board table, or the user's [`HeaderRole`] via
/// `config::effective_role`).
///
/// [`HeaderRole`]: crate::config::HeaderRole
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FanRole {
    /// Cools the CPU (CPU cooler / radiator fans).
    Cpu,
    /// AIO pump: fixed high duty, never a low curve or the AI.
    Pump,
    /// Case fans.
    System,
    /// Something without a confirmed role (a header with no speed signal, until the user says what
    /// it is): CPU curves with a 60 % floor, never the AI, watched like a CPU fan.
    Unknown,
}

impl FanRole {
    /// Part of the CPU cooling chain (CPU fans, pump, or possibly either).
    pub fn cools_cpu(self) -> bool {
        matches!(self, FanRole::Cpu | FanRole::Pump | FanRole::Unknown)
    }
}

#[derive(Clone, Debug)]
pub struct FanChannelDesc {
    /// Stable id such as "mb.pwm0".
    pub id: String,
    pub default_name: String,
    pub rpm_sensor: Option<String>,
    pub controllable: bool,
    pub role: FanRole,
    /// The header physically exists on this board (false: hidden unless it spins).
    pub present: bool,
    /// The tachometer showed a plausible RPM at startup (a fan is connected).
    pub tach_detected: bool,
    /// The header is in the board's official specification (`None`: layout unknown).
    pub documented: Option<bool>,
}

impl FanChannelDesc {
    /// Controllable but neither documented nor spinning: the UI shows it ghosted.
    pub fn possibly_unconnected(&self) -> bool {
        self.controllable && !self.present
    }
}

/// Well-known sensor ids used for defaults and the AI.
pub mod ids {
    pub const CPU_TEMP: &str = "cpu.temp";
    pub const CPU_POWER: &str = "cpu.power";
    pub const CPU_LOAD: &str = "cpu.load";
    pub const MEM_LOAD: &str = "mem.load";
    pub const GPU_TEMP: &str = "gpu0.temp";
    pub const GPU_LOAD: &str = "gpu0.load";
    /// Watts (`SensorKind::Power`) or percent of board power (`SensorKind::Load`), by GPU vendor.
    pub const GPU_POWER: &str = "gpu0.power";
    pub const SYSTEM_TEMP: &str = "mb.t0";
}

#[derive(Clone, Debug)]
pub struct SensorEntry {
    pub desc: SensorDesc,
    pub value: Option<f32>,
    pub min: Option<f32>,
    pub max: Option<f32>,
}

#[derive(Clone, Debug)]
pub struct FanState {
    pub id: String,
    pub index: usize,
    /// User-facing name: the user's name, else the board's header name.
    pub name: String,
    /// Silkscreen header name ("CPU_FAN"), for the subtitle.
    pub header: String,
    /// Effective role (the user's assignment, else the board's guess; `Unknown` for an unconfirmed
    /// header without a speed signal).
    pub role: FanRole,
    /// What the user / header wizard confirmed is connected (`None`: not confirmed).
    pub assigned: Option<crate::config::HeaderRole>,
    pub rpm: Option<f32>,
    /// Duty we are commanding (or reading back), `None` when the BIOS controls the fan.
    pub duty: Option<f32>,
    pub mode: String,
    pub controlled: bool,
    pub controllable: bool,
    pub visible: bool,
    /// The safety layer overrode the requested duty (critical temperature / sensor loss).
    pub failsafe: bool,
    /// The fan runs a curve inside the Super I/O chip (keeps working if the app stops).
    pub hw_curve: bool,
    /// The chip forces this fan to 100 % on its own above the failsafe temperature.
    pub hw_failsafe: bool,
    /// `FanChannelDesc::possibly_unconnected()`: the UI draws it as a ghost header.
    pub unconnected: bool,
    /// The header has a usable tach signal (`tach_detected` at start or seen spinning since). A
    /// present header without one drives a fan or pump whose speed is unknown ("running, no speed
    /// signal"): `rpm` is `None` instead of 0, it can't be calibrated or checked for a stall.
    pub speed_signal: bool,
    /// Duty the BIOS curve gives this fan now, estimated from the decoded firmware curve (BIOS
    /// control only; `duty` is `None` then).
    pub duty_est: Option<f32>,
    /// Lowest duty the role and the board's CPU / PUMP label allow in this mode (pump 30 %, unknown
    /// role 60 %, a board CPU header 20 %), `None` if none.
    pub floor: Option<f32>,
    /// The fan mode asks for AI here but the header can't use it: why (`config::NO_SPEED_SIGNAL_AI`,
    /// `PUMP_AI`, `UNKNOWN_AI`). The header runs a curve / fixed duty instead.
    pub ai_refused: Option<&'static str>,
    /// Software floor (%) currently lifted into the chip curve for the non-CPU part of a `max(CPU,
    /// GPU)` curve; `None` when no lift applies.
    pub gpu_lift: Option<f32>,
    /// The header identification wizard drives this header right now.
    pub identifying: bool,
}

impl FanState {
    /// Present, but no RPM can be read.
    pub fn no_speed_signal(&self) -> bool {
        !self.speed_signal && !self.unconnected
    }

    /// A stopped fan can be detected (FRG-E207): needs a speed signal.
    pub fn stall_alert(&self) -> bool {
        self.speed_signal
    }

    /// Why the calibration wizard can't run on this header (`None`: it can).
    pub fn calibration_refused(&self) -> Option<&'static str> {
        if !self.controllable {
            Some("This fan can only be monitored on this board.")
        } else if !self.speed_signal {
            Some(crate::engine::NO_SPEED_SIGNAL_CALIBRATION)
        } else {
            crate::engine::calibration_role_refused(self.assigned)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AlertLevel {
    Info,
    Warning,
    Critical,
}

#[derive(Clone, Debug)]
pub struct Alert {
    pub level: AlertLevel,
    pub title: String,
    pub detail: String,
    /// Stable error code shown to the user (with help text and a fix action), if any.
    pub code: Option<crate::error::ErrorCode>,
}

impl Alert {
    /// An alert whose level and default title come from `code`.
    pub fn coded(code: crate::error::ErrorCode, detail: impl Into<String>) -> Self {
        use crate::error::Severity;
        let level = match code.severity() {
            Severity::Info => AlertLevel::Info,
            Severity::Warning | Severity::Error => AlertLevel::Warning,
            Severity::Critical => AlertLevel::Critical,
        };
        Alert { level, title: code.title().into(), detail: detail.into(), code: Some(code) }
    }

    pub fn titled(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }
}
