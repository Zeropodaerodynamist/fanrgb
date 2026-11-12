//! Background engine: polls sensors, runs fan control (curves / AI / safety), publishes
//! state for the UI and keeps the crash-recovery snapshot up to date.
//!
//! Output paths per fan:
//! * **Hardware curve**: Curve-mode fans whose single source sensor is a chip temperature input
//!   get their curve programmed into the Super I/O once (and again on change / resume); the chip
//!   then runs it by itself, also if the app hangs.
//! * **Duty** (`HwBackend::set_fan`): Fixed, AI and software curves (the backend implements a duty
//!   as a flat hardware curve with a failsafe ramp to 100 % at `set_failsafe`'s temperature).
//! * **Release**: BIOS mode, emergency, lost sensors.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use crate::config::{
    AiConfig, AiStyle, CPU_AI_LIMIT_MAX, Combine, Config, FanCalibration, FanConfig, FanMode, HeaderPlan, HeaderRole,
    Preset, effective_role,
};
pub use crate::control::ai::AiParam;
use crate::control::ai::{
    AiManager, AirMap, Decision, Fingerprint, GroupGoal, SupState, SystemInputs, Targets, model_key,
};
use crate::control::calibrate::{Calibration, CalibrationView};
use crate::control::cooler::{CoolerInput, CoolerWatch, Trigger};
use crate::control::curve::CurveState;
use crate::control::efficacy::{EfficacyInput, EfficacyMonitor, EfficacyView};
pub use crate::control::identify::IdentifyCmd;
use crate::control::identify::{Identify, IdentifyView};
use crate::control::{airflow_to_duty, combine, duty_to_airflow, effective_min, limits, shape_duty};
use crate::error::{ErrorCode, Severity, code_of};
use crate::hw::pawnio::PawnIoStatus;
use crate::hw::{HwBackend, HwCurve, HwInfo};
use crate::model::{Alert, FanChannelDesc, FanRole, FanState, SensorEntry, SensorKind, ids};

pub const HISTORY_LEN: usize = 900;
/// Error events kept for the Diagnostics page.
pub const ERROR_RING_LEN: usize = 200;
/// The same (code, source) is written to the log at most this often while it persists.
const RELOG_ACTIVE: Duration = Duration::from_secs(600);
/// …and at most this often when it flaps on and off.
const RELOG_FLAP: Duration = Duration::from_secs(60);
/// Hardware curves are re-programmed this often as a guard against firmware resets.
const HW_CURVE_REFRESH: Duration = Duration::from_secs(300);
/// Hardware failsafe temperature never exceeds this (Zen 4 Tjmax 95 °C, margin for the EC's own reading).
pub const HW_FAILSAFE_MAX_C: f32 = 90.0;

#[derive(Debug)]
pub enum Command {
    Calibrate(usize),
    CancelCalibration,
    ResetAi(Option<String>),
    WriteDiagnostics,
    /// Hand every fan back to the BIOS until the user picks a profile again.
    EmergencyRelease,
    /// Leave the emergency state and resume the active profile.
    Resume,
    /// No-op used to wake the engine (e.g. to notice a quit request immediately).
    Wake,
    /// The PC resumed from sleep/hibernate: reset filters, re-program hardware curves and the
    /// failsafe, never learn across the gap (sent by the session watcher on PBT_APMRESUMEAUTOMATIC).
    Resumed,
    /// Header identification wizard (start, answer, repeat, skip, cancel / close).
    Identify(IdentifyCmd),
}

#[derive(Clone, Debug, Default)]
pub struct AiZoneView {
    pub key: String,
    pub fans: Vec<String>,
    pub source: String,
    pub style: Option<AiStyle>,
    pub target: f32,
    pub limit: f32,
    pub temp: f32,
    pub airflow: f32,
    /// Optimizer choice before the supervisor / safety floor.
    pub model_airflow: f32,
    pub predicted_peak: f32,
    /// Forecast temperature 90 s ahead at the applied speed.
    pub predicted_end: f32,
    pub duty: f32,
    pub confidence: f32,
    /// Learned samples (5 s heatsink blocks).
    pub samples: u64,
    /// Mean open-loop error over informative 120 s validation runs (°C).
    pub err_h: f32,
    /// Forecast of `temp` at the applied speed, `forecast_step_s` apart (300 s).
    pub predicted: Vec<f32>,
    /// What the controller is doing and why, e.g. "Held 42 % — falling need not sustained 12/25 s."
    pub reason: String,
    /// Steady CPU temperature rise per 100 W at 30 / 60 / 100 % CPU airflow.
    pub rise: [f32; 3],
    /// Heatsink time constant at 60 % CPU airflow (s).
    pub time_constant: f32,
    // ---- v2 insight (Intelligence page) ----
    /// "CPU" (CPU / pump fans) or "Case" (system fans).
    pub group: String,
    /// Id of the learned PC model this group uses.
    pub model_key: String,
    /// Learned physical parameters with names, units and uncertainty.
    pub params: Vec<AiParam>,
    /// Confidence factors (count of validated load changes, prediction error, fan-effect certainty).
    pub conf_factors: [f32; 3],
    pub confidence_why: String,
    pub forecast_step_s: f32,
    /// Forecast of the CPU die and GPU (empty if no GPU sensor) at the applied speeds.
    pub predicted_die: Vec<f32>,
    pub predicted_gpu: Vec<f32>,
    /// Current workload class ("Idle", "Gaming", …).
    pub workload: String,
    /// Learned quiet ceiling (airflow 0..1) for this workload and time of day, if known.
    pub quiet_ceiling: Option<f32>,
    pub ceiling_active: bool,
    /// Safety floor (airflow) the learning cannot go below.
    pub floor: f32,
    pub latched: bool,
    pub stale: bool,
    pub distrusted: bool,
    pub probing: bool,
    /// Validated load changes so far (6 = full trust).
    pub n_inf: u32,
    /// Why learning is or is not happening right now.
    pub learning: String,
}

/// One coded error/notice for the Diagnostics page.
#[derive(Clone, Debug, PartialEq)]
pub struct ErrorEvent {
    /// Wall clock, unix seconds.
    pub time_unix: u64,
    pub code: ErrorCode,
    pub detail: String,
    /// What raised it (fan id, sensor id, "ai", "config", …).
    pub source: String,
}

#[derive(Clone, Debug, Default)]
pub struct EngineState {
    pub info: HwInfo,
    pub sensors: Vec<SensorEntry>,
    pub fans: Vec<FanState>,
    pub ai: Vec<AiZoneView>,
    pub calibration: Option<CalibrationView>,
    pub alerts: Vec<Alert>,
    pub tick: u64,
    pub poll_ms: f32,
    pub emergency_released: bool,
    pub diag_path: Option<String>,
    pub rgb_health: Option<String>,
    pub conflicts: Vec<(String, String)>,
    /// Last `ERROR_RING_LEN` coded events, oldest first.
    pub errors: Vec<ErrorEvent>,
    /// Bumped whenever `errors` changes.
    pub errors_rev: u64,
    /// Header identification wizard (running, or finished until closed).
    pub identify: Option<IdentifyView>,
    /// Headers (display names) that still need a confirmed role. Non-empty: every fan mode except
    /// BIOS leaves all headers on the BIOS until they are identified ([`IDENTIFY_FIRST`]).
    pub mode_gate: Vec<String>,
    /// Cooling-efficacy monitor (FRG-E309).
    pub efficacy: EfficacyView,
    /// `Some(true)` = the CPU temperature recently matched the learned cooling at this
    /// power and fan speed ("cooling works"), `Some(false)` = cooling degraded or failed (E305 /
    /// E309), `None` = no verdict.
    pub cpu_cooling_verified: Option<bool>,
}

impl EngineState {
    pub fn value(&self, id: &str) -> Option<f32> {
        self.sensors.iter().find(|s| s.desc.id == id).and_then(|s| s.value)
    }
    pub fn sensor(&self, id: &str) -> Option<&SensorEntry> {
        self.sensors.iter().find(|s| s.desc.id == id)
    }
}

#[derive(Default)]
pub struct History {
    pub times: VecDeque<f64>,
    pub series: HashMap<String, VecDeque<f32>>,
}

impl History {
    fn push(&mut self, t: f64, values: &[(String, Option<f32>)]) {
        if self.times.len() >= HISTORY_LEN {
            self.times.pop_front();
        }
        self.times.push_back(t);
        for (id, v) in values {
            let s = self.series.entry(id.clone()).or_insert_with(|| {
                let mut q = VecDeque::with_capacity(HISTORY_LEN);
                q.extend(std::iter::repeat_n(f32::NAN, self.times.len().saturating_sub(1)));
                q
            });
            if s.len() >= HISTORY_LEN {
                s.pop_front();
            }
            s.push_back(v.unwrap_or(f32::NAN));
        }
    }
}

pub struct Shared {
    pub config: RwLock<Config>,
    pub config_rev: AtomicU64,
    pub state: RwLock<EngineState>,
    pub history: RwLock<History>,
    pub rgb: RwLock<crate::rgb::RgbStatus>,
    pub quit: AtomicBool,
    /// Set by the engine / RGB threads once they finished their shutdown work.
    pub engine_done: AtomicBool,
    pub rgb_done: AtomicBool,
    pub ui_ctx: Mutex<Option<egui::Context>>,
    cmd_tx: Mutex<Option<Sender<Command>>>,
    rgb_tx: Mutex<Option<Sender<crate::rgb::RgbCommand>>>,
    started: Instant,
}

impl Shared {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Shared {
            config: RwLock::new(config),
            config_rev: AtomicU64::new(1),
            state: RwLock::new(EngineState::default()),
            history: RwLock::new(History::default()),
            rgb: RwLock::new(Default::default()),
            quit: AtomicBool::new(false),
            engine_done: AtomicBool::new(false),
            rgb_done: AtomicBool::new(false),
            ui_ctx: Mutex::new(None),
            cmd_tx: Mutex::new(None),
            rgb_tx: Mutex::new(None),
            started: Instant::now(),
        })
    }

    pub fn set_rgb_sender(&self, tx: Sender<crate::rgb::RgbCommand>) {
        if let Ok(mut t) = self.rgb_tx.lock() {
            *t = Some(tx);
        }
    }

    pub fn send_rgb(&self, cmd: crate::rgb::RgbCommand) {
        if let Ok(tx) = self.rgb_tx.lock()
            && let Some(tx) = tx.as_ref()
        {
            let _ = tx.send(cmd);
        }
    }

    /// Mutates the configuration and notifies the engine / RGB threads.
    pub fn edit_config(&self, f: impl FnOnce(&mut Config)) {
        if let Ok(mut c) = self.config.write() {
            f(&mut c);
        }
        self.config_rev.fetch_add(1, Ordering::SeqCst);
    }

    pub fn config_snapshot(&self) -> Config {
        self.config.read().map(|c| c.clone()).unwrap_or_default()
    }

    /// Applies a fan mode in one call (tray, hotkeys, other threads): selects the preset's profile
    /// ([`Config::apply_preset`]), resumes control if the fans were handed back to the BIOS, and
    /// wakes the engine so it takes effect at once. The engine saves `config.json` (debounced 1.5 s,
    /// and at exit). Returns the active profile index.
    pub fn apply_preset(&self, preset: Preset) -> usize {
        let mut i = 0;
        self.edit_config(|c| i = c.apply_preset(preset));
        self.send(Command::Resume);
        self.send(Command::Wake);
        i
    }

    /// Hotkey / tray "Next fan mode" ([`Config::cycle_preset`]). Returns the mode now active.
    pub fn cycle_preset(&self) -> Preset {
        let mut next = Preset::Balanced;
        self.edit_config(|c| next = c.cycle_preset());
        self.send(Command::Resume);
        self.send(Command::Wake);
        next
    }

    pub fn send(&self, cmd: Command) {
        if let Ok(tx) = self.cmd_tx.lock()
            && let Some(tx) = tx.as_ref()
        {
            let _ = tx.send(cmd);
        }
    }

    pub fn elapsed(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    /// Wakes the UI even while the window is hidden (used when quitting).
    pub fn force_repaint(&self) {
        if let Ok(ctx) = self.ui_ctx.lock()
            && let Some(ctx) = ctx.as_ref()
        {
            ctx.request_repaint();
        }
    }

    pub fn request_repaint(&self) {
        if !crate::system::main_window_visible() {
            return;
        }
        if let Ok(ctx) = self.ui_ctx.lock()
            && let Some(ctx) = ctx.as_ref()
        {
            ctx.request_repaint();
        }
    }
}

/// What the tray icon's warning state and the issue list depend on: the alerts (not their detail,
/// which carries live temperatures) and the lighting's connection / error / Dynamic Lighting state.
/// Hashed without allocating; the engine wakes a hidden UI only when it changes.
fn tray_signature(alerts: &[Alert], r: &crate::rgb::RgbStatus) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for a in alerts {
        std::mem::discriminant(&a.level).hash(&mut h);
        a.code.hash(&mut h);
        a.title.hash(&mut h);
    }
    std::mem::discriminant(&r.state).hash(&mut h);
    r.last_error.as_ref().map(|e| (e.code, e.count)).hash(&mut h);
    r.error.hash(&mut h);
    std::mem::discriminant(&r.audio).hash(&mut h);
    std::mem::discriminant(&r.dynamic_lighting.state).hash(&mut h);
    (r.dynamic_lighting.conflict(), r.dynamic_lighting.retaken, r.dynamic_lighting.contenders.len()).hash(&mut h);
    (r.hung_io_threads, r.simulated, r.device.is_some()).hash(&mut h);
    h.finish()
}

// ---------------------------------------------------------------------------------------
// Coded error log: bounded ring for the UI + rate-limited log lines.

struct KeyState {
    seen_tick: u64,
    active: bool,
    last_logged: Option<Instant>,
    suppressed: u32,
}

#[derive(Default)]
struct ErrorLog {
    ring: VecDeque<ErrorEvent>,
    rev: u64,
    keys: HashMap<(ErrorCode, String), KeyState>,
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl ErrorLog {
    /// Records that `code` is (still) happening at `source`. A new occurrence is logged once and
    /// added to the ring; a persisting one is re-logged at most every 10 min, a flapping one at
    /// most every minute.
    fn report(&mut self, tick: u64, now: Instant, code: ErrorCode, source: &str, detail: &str) {
        let k = self.keys.entry((code, source.to_string())).or_insert(KeyState {
            seen_tick: 0,
            active: false,
            last_logged: None,
            suppressed: 0,
        });
        let was_active = k.active && k.seen_tick + 1 >= tick;
        k.seen_tick = tick;
        k.active = true;
        let since = k.last_logged.map(|t| now.duration_since(t));
        let due = match since {
            None => true,
            Some(d) => d >= if was_active { RELOG_ACTIVE } else { RELOG_FLAP },
        };
        if !due {
            k.suppressed += 1;
            return;
        }
        let extra = if k.suppressed > 0 { format!(" ({} repeats suppressed)", k.suppressed) } else { String::new() };
        k.suppressed = 0;
        k.last_logged = Some(now);
        let line = format!("[{}] {} ({source}): {detail}{extra}", code, code.title());
        match code.severity() {
            Severity::Info => log::info!("{line}"),
            Severity::Warning => log::warn!("{line}"),
            Severity::Error | Severity::Critical => log::error!("{line}"),
        }
        if was_active {
            return; // still the same episode: log only, don't fill the ring
        }
        if self.ring.len() >= ERROR_RING_LEN {
            self.ring.pop_front();
        }
        self.ring.push_back(ErrorEvent {
            time_unix: unix_now(),
            code,
            detail: detail.to_string(),
            source: source.to_string(),
        });
        self.rev += 1;
    }

    /// A discrete event (not a condition): always logged and added to the ring.
    fn event(&mut self, code: ErrorCode, source: &str, detail: &str) {
        let line = format!("[{}] {} ({source}): {detail}", code, code.title());
        match code.severity() {
            Severity::Info => log::info!("{line}"),
            Severity::Warning => log::warn!("{line}"),
            Severity::Error | Severity::Critical => log::error!("{line}"),
        }
        if self.ring.len() >= ERROR_RING_LEN {
            self.ring.pop_front();
        }
        self.ring.push_back(ErrorEvent {
            time_unix: unix_now(),
            code,
            detail: detail.to_string(),
            source: source.to_string(),
        });
        self.rev += 1;
    }

    fn end_tick(&mut self, tick: u64) {
        for k in self.keys.values_mut() {
            if k.seen_tick < tick {
                k.active = false;
            }
        }
        if self.keys.len() > 512 {
            self.keys.retain(|_, k| k.active || k.last_logged.is_some_and(|t| t.elapsed() < RELOG_ACTIVE));
        }
    }
}

// ---------------------------------------------------------------------------------------

/// How long a one-shot notice stays visible after its last occurrence.
const NOTICE_FOR: Duration = Duration::from_secs(600);
/// Why a header without a tach signal can't be calibrated (also shown by the UI).
pub const NO_SPEED_SIGNAL_CALIBRATION: &str = "This header reports no speed signal (a fan or pump without a tach wire, a \
     splitter or hub that passes no tach, or a pump whose tach is on another header). Calibration measures RPM, so it \
     is not possible here; Fixed and Curve modes work without it.";
/// Why a pump is never calibrated (also shown by the UI).
pub const PUMP_CALIBRATION: &str = "Calibration runs the header down to 0 %, which would stop the pump. Pumps      follow their own temperature curve (never below 30 %) and need no calibration.";
/// Why a header nobody confirmed as fans is not calibrated (also shown by the UI).
pub const UNCONFIRMED_CALIBRATION: &str = "Calibration runs the header down to 0 % for about a minute, which is only      safe for fans. Confirm what is connected first (Identify headers, or set the header's role to CPU, radiator or      case fans).";
/// Why calibration is not started while a cooling alarm is up.
const ALARM_CALIBRATION: &str = "A CPU temperature or cooling alarm is active. Calibrate when it has cleared.";
/// What FRG-E309 does (and does not do) in v0.2: it informs, it never forces a duty.
/// Part of the alert text.
pub const EFFICACY_INFORM_ONLY: &str = "FanRGB does not change the fan speeds for this alert: check that the pump \
     and the CPU fans run (or choose Full speed) until it clears.";

/// Calibration sweeps a header down to 0 % (`control::calibrate`), below every role floor: only a
/// header the user confirmed as fans may run it, never a pump or an unconfirmed header that may
/// carry one. `None`: the role allows it.
pub fn calibration_role_refused(assigned: Option<HeaderRole>) -> Option<&'static str> {
    match assigned {
        Some(HeaderRole::CpuFans | HeaderRole::RadiatorFans | HeaderRole::CaseFans) => None,
        Some(HeaderRole::Pump) => Some(PUMP_CALIBRATION),
        _ => Some(UNCONFIRMED_CALIBRATION),
    }
}

/// A fan without a temperature source for this long goes back to the BIOS (sensor lost).
const SENSOR_LOST_AFTER: Duration = Duration::from_secs(5);
/// First retry of a hardware curve the chip did not verify; doubles up to `HW_CURVE_RETRY_MAX`.
const HW_CURVE_RETRY: Duration = Duration::from_secs(60);
const HW_CURVE_RETRY_MAX: Duration = Duration::from_secs(900);

/// A de-duplicated notice: the same (code, subject) only refreshes `last` and counts.
struct Notice {
    code: ErrorCode,
    subject: String,
    detail: String,
    last: Instant,
    count: u32,
}

/// The part of a detail text that names what it is about ("IT8689E PWM1: wrote …" → "IT8689E PWM1").
fn subject_of(detail: &str) -> String {
    let s = detail.split(": ").next().unwrap_or(detail);
    s.chars().take(80).collect()
}

/// Records a notice, merging repeats of the same (code, subject).
fn push_notice(list: &mut Vec<Notice>, code: ErrorCode, detail: &str, now: Instant) {
    let subject = subject_of(detail);
    match list.iter_mut().find(|n| n.code == code && n.subject == subject) {
        Some(n) => {
            n.count = n.count.saturating_add(1);
            n.last = now;
            n.detail = detail.to_string();
        }
        None => list.push(Notice { code, subject, detail: detail.to_string(), last: now, count: 1 }),
    }
}

/// Write access for publishing without blocking behind a reader for long (the UI may hold a read
/// guard for a frame, and must never be able to stall the engine before it hands fans back).
fn try_write_for<T>(lock: &RwLock<T>, max: Duration) -> Option<std::sync::RwLockWriteGuard<'_, T>> {
    let t0 = Instant::now();
    loop {
        match lock.try_write() {
            Ok(g) => return Some(g),
            Err(std::sync::TryLockError::Poisoned(p)) => return Some(p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) if t0.elapsed() < max => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(std::sync::TryLockError::WouldBlock) => return None,
        }
    }
}

#[derive(Default)]
struct FanRuntime {
    controlled: bool,
    duty: Option<f32>,
    last_write: Option<Instant>,
    curve: CurveState,
    missing_since: Option<Instant>,
    write_errors: u32,
    last_error: Option<(ErrorCode, String)>,
    seen_spinning: bool,
    stalled_since: Option<Instant>,
    /// Curve currently programmed into the chip (the chip runs this fan by itself).
    hw_curve: Option<HwCurve>,
    hw_curve_at: Option<Instant>,
    /// Last curve the chip rejected and when to try it again (`None`: not until the curve changes,
    /// e.g. unsupported). A read-back mismatch is retried with a growing back-off.
    hw_curve_failed: Option<(HwCurve, Option<Instant>)>,
    /// Consecutive hardware-curve mismatches (back-off exponent).
    hw_curve_failures: u32,
    /// RPM observed while commanded ≥ 95 % for a few seconds (max-RPM estimate without calibration).
    max_rpm_full: f32,
    full_since: Option<Instant>,
    /// Smoothing / deadband of the non-CPU part of a `max(CPU, …)` chip curve (the software floor).
    lift: CurveState,
}

/// What the profile and the header's role ask of one fan this tick (computed once per tick, shared
/// by the AI grouping and the per-fan loop).
#[derive(Clone, Debug)]
struct FanPlan {
    role: FanRole,
    assigned: Option<HeaderRole>,
    speed_signal: bool,
    /// Left to the BIOS: nothing connected, or the user marked the header unused.
    unused: bool,
    /// A non-BIOS mode is selected but waits for the header identification ([`mode_gate`]): the
    /// plan is the BIOS one.
    gated: bool,
    hp: HeaderPlan,
}

/// Non-BIOS fan modes wait for one header identification:
/// while any header the wizard would test (controllable, and present or seen spinning) has no
/// confirmed role, a pump on a header with a tach could be taken for a case fan and run a case curve.
/// Returns the indices of those headers; non-empty = every header stays on the BIOS under any mode
/// except BIOS (which is the same thing). `identifiable[i]`: the header is one the wizard tests.
fn mode_gate(identifiable: &[bool], assigned: &[Option<HeaderRole>]) -> Vec<usize> {
    identifiable.iter().zip(assigned).enumerate().filter(|(_, (id, a))| **id && a.is_none()).map(|(i, _)| i).collect()
}

/// Why a fan mode does not drive the headers yet ([`mode_gate`]; also shown by the UI).
pub const IDENTIFY_FIRST: &str = "Identify your fans first: until every fan header has a confirmed device, fan \
     modes leave all headers on the BIOS.";

/// Result of the periodic device / process scan: RGB USB health, conflicting programs (name, why).
type ScanResult = (Option<String>, Vec<(String, String)>);

/// Joins `job` if it finished (never waits).
fn take_finished<T>(job: &mut Option<std::thread::JoinHandle<T>>) -> Option<std::thread::Result<T>> {
    if job.as_ref().is_some_and(|j| j.is_finished()) { job.take().map(|j| j.join()) } else { None }
}

pub fn spawn(shared: Arc<Shared>, hw: Box<dyn HwBackend>) -> std::thread::JoinHandle<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    if let Ok(mut t) = shared.cmd_tx.lock() {
        *t = Some(tx);
    }
    std::thread::Builder::new()
        .name("fanrgb-engine".into())
        .spawn(move || Engine::new(shared, hw).run(rx))
        .expect("spawn engine thread")
}

/// What to do with one fan this tick (`None` = release to the BIOS).
enum Output {
    Duty(f32),
    HwCurve(HwCurve),
}

struct Engine {
    shared: Arc<Shared>,
    hw: Box<dyn HwBackend>,
    ai: AiManager,
    fans: Vec<FanRuntime>,
    calib: Option<Calibration>,
    minmax: HashMap<String, (f32, f32)>,
    last_tick: Instant,
    last_wall: SystemTime,
    seen_rev: u64,
    save_due: Option<Instant>,
    last_ai_save: Instant,
    last_profile: usize,
    /// The active profile's preset was BIOS last tick (a switch to it stops the wizard).
    last_bios: bool,
    controlled_sig: Vec<bool>,
    emergency: bool,
    last_diag_scan: Option<Instant>,
    /// The periodic RGB-USB / conflicting-software scan, on its own thread: SetupAPI can stall for
    /// longer than the hang watchdog's timeout while a driver install holds the PnP lock.
    /// Yields (RGB USB health, conflicts).
    scan_job: Option<std::thread::JoinHandle<ScanResult>>,
    /// A diagnostics report being completed and written off the engine thread (registry, service
    /// manager, Appx and SetupAPI queries); yields the written path or the error.
    diag_job: Option<std::thread::JoinHandle<Result<String, String>>>,
    rgb_health: Option<String>,
    conflicts: Vec<(String, String)>,
    diag_path: Option<String>,
    tick: u64,
    resume_pending: bool,
    crit_latched: bool,
    crit_ok_since: Option<Instant>,
    cpu_hist: VecDeque<f32>,
    failsafe_set: Option<(u8, f32)>,
    errlog: ErrorLog,
    /// One-shot notices shown as alerts for a while (AI model reset/distrust, hardware events, …),
    /// de-duplicated by (code, subject).
    sticky: Vec<Notice>,
    /// The AI's control temperature has been missing since then.
    ai_stale_since: Option<Instant>,
    /// The CPU temperature sensor exists but has been missing since then / for this many samples.
    cpu_missing_since: Option<Instant>,
    cpu_missing_n: u32,
    config_save_error: Option<String>,
    /// `hw.reapply()` is due at this time (~2 s after a resume).
    reapply_at: Option<Instant>,
    /// The crash snapshot could not be written: no new manual control without a hardware failsafe.
    /// Retried after a minute.
    snapshot_failed_at: Option<Instant>,
    /// `control_snapshot.json` (a field so tests can use their own file).
    snapshot_path: std::path::PathBuf,
    /// Start-up crash recovery failed (`system::takeover_blocked`): take no fan, never write or
    /// delete the snapshot that still holds the real BIOS state.
    takeover_blocked: Option<String>,
    /// Command channel (set by `run`), also drained right before publishing.
    rx: Option<Receiver<Command>>,
    /// Problems found at start (`system::startup_issues`): reported once into the error ring and
    /// kept as alerts.
    startup_issues: Vec<crate::error::AppError>,
    /// Cooling-failure watch for CPU / pump headers without a speed signal, and its state.
    cooler: CoolerWatch,
    cooling_alarm: Option<Trigger>,
    /// Cooling-efficacy monitor (FRG-E309) and whether its alarm is up. Inform-only in v0.2:
    /// the alarm raises the alert (and the tray balloon) and stops
    /// the voluntary low-duty tests (calibration, identification), but never forces a duty: the
    /// learned baseline needs real-world history first. FRG-E305 and FRG-E301 keep forcing.
    efficacy: EfficacyMonitor,
    efficacy_alarm: bool,
    /// Header identification wizard (kept after it finished until closed).
    identify: Option<Identify>,
    /// Names of the headers that keep non-BIOS modes on the BIOS ([`mode_gate`]); published.
    mode_gate: Vec<String>,
    /// A non-BIOS mode was held back by the gate last tick (logs the transitions once).
    gate_held: bool,
    /// Median-of-3 CPU temperature of the last tick (the wizard's start check).
    cpu_median: Option<f32>,
    /// Tray notifications for critical alerts.
    notifier: crate::notify::Notifier,
    /// Codes notified so far (newest last, bounded; Diagnostics and tests).
    notified: VecDeque<ErrorCode>,
    /// [`tray_signature`] of the last publish: a change wakes the UI even while it is hidden.
    tray_sig: u64,
    /// Ticks that panicked in a row (see `guarded`).
    panic_streak: u32,
}

impl Engine {
    fn new(shared: Arc<Shared>, hw: Box<dyn HwBackend>) -> Self {
        let n = hw.fans().len();
        {
            let mut st = shared.state.write().unwrap();
            st.info = hw.info().clone();
        }
        let last_profile = shared.config.read().map(|c| c.active_profile).unwrap_or(0);
        Engine {
            shared,
            hw,
            ai: AiManager::load(),
            fans: (0..n).map(|_| FanRuntime::default()).collect(),
            calib: None,
            minmax: HashMap::new(),
            last_tick: Instant::now(),
            last_wall: SystemTime::now(),
            seen_rev: 0,
            save_due: None,
            last_ai_save: Instant::now(),
            last_profile,
            last_bios: false,
            controlled_sig: vec![false; n],
            emergency: false,
            last_diag_scan: None,
            scan_job: None,
            diag_job: None,
            rgb_health: None,
            conflicts: Vec::new(),
            diag_path: None,
            tick: 0,
            resume_pending: false,
            crit_latched: false,
            crit_ok_since: None,
            cpu_hist: VecDeque::with_capacity(3),
            failsafe_set: None,
            errlog: ErrorLog::default(),
            sticky: Vec::new(),
            ai_stale_since: None,
            cpu_missing_since: None,
            cpu_missing_n: 0,
            config_save_error: None,
            reapply_at: None,
            snapshot_failed_at: None,
            snapshot_path: crate::util::paths::control_snapshot_file(),
            // Process-global start-up state; tests set these fields directly.
            takeover_blocked: if cfg!(test) { None } else { crate::system::takeover_blocked() },
            startup_issues: if cfg!(test) { Vec::new() } else { crate::system::startup_issues() },
            rx: None,
            cooler: CoolerWatch::default(),
            cooling_alarm: None,
            // Tests learn in memory only (the shared test home must not carry a baseline across tests).
            efficacy: if cfg!(test) {
                EfficacyMonitor::default()
            } else {
                EfficacyMonitor::load(crate::config::cooling_baseline_path())
            },
            efficacy_alarm: false,
            identify: None,
            mode_gate: Vec::new(),
            gate_held: false,
            cpu_median: None,
            notifier: Default::default(),
            notified: VecDeque::new(),
            tray_sig: 0,
            panic_streak: 0,
        }
    }

    fn run(mut self, rx: Receiver<Command>) {
        log::info!("engine started ({} sensors, {} fan channels)", self.hw.sensors().len(), self.hw.fans().len());
        self.rx = Some(rx);
        let mut next = Instant::now();
        loop {
            if self.shared.quit.load(Ordering::SeqCst) {
                break;
            }
            let now = Instant::now();
            if now < next {
                let Some(rx) = self.rx.as_ref() else { break };
                match rx.recv_timeout(next - now) {
                    Ok(cmd) => {
                        self.guarded("command", |e| e.handle(cmd));
                        continue;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            let poll_ms = self.shared.config.read().map(|c| c.general.poll_ms).unwrap_or(1000).clamp(250, 5000);
            next = Instant::now() + Duration::from_millis(poll_ms as u64);
            self.guarded("tick", Engine::tick);
            next = self.wake_at(next);
        }
        self.shutdown();
        self.shared.engine_done.store(true, Ordering::SeqCst);
    }

    /// Runs a tick or a command handler with panics contained: a panic hands
    /// every fan back to the BIOS and stops the identification wizard and a calibration, so neither
    /// drives its header again on the next tick; the engine itself keeps running (alarms, latch).
    fn guarded(&mut self, what: &str, f: impl FnOnce(&mut Engine)) {
        if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self))) {
            log::error!("[{}] engine {what} panicked: {e:?}; returning fans to BIOS", ErrorCode::Internal);
            let why = "Stopped: an internal error occurred (see the log). Every header is back on the BIOS.";
            if let Some(c) = self.calib.as_mut() {
                c.abort(why);
            }
            self.stop_identify(why);
            // A tick that fails again and again would otherwise take the fans and hand them back on
            // every poll (audit FR-CTL-004): after three in a row they stay with the BIOS until the
            // user selects a fan mode again.
            self.panic_streak = self.panic_streak.saturating_add(1);
            if self.panic_streak >= 3 && !self.emergency {
                log::error!(
                    "[{}] {} engine errors in a row: fans stay on the BIOS until a fan mode is selected",
                    ErrorCode::Internal,
                    self.panic_streak
                );
                self.emergency = true;
            }
            // The release itself must not take the engine thread down.
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.release_all())).is_err() {
                log::error!("[{}] handing the fans back failed; the watchdog covers it", ErrorCode::Internal);
            }
        } else if what == "tick" {
            self.panic_streak = 0;
        }
    }

    /// When the next tick is due: the poll interval's `next`, or earlier when the identification
    /// wizard's test duty ends before it (with a 5 s poll a late tick could
    /// otherwise hold a possible pump at 30 % for up to 27 s).
    fn wake_at(&self, next: Instant) -> Instant {
        match self.identify.as_ref().and_then(Identify::low_left) {
            Some(left) => next.min(self.last_tick + Duration::from_secs_f32(left)),
            None => next,
        }
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Calibrate(i) => {
                if let Some(f) = self.hw.fans().get(i) {
                    let name = self.fan_name(i);
                    let mut c = Calibration::new(i, f.id.clone(), name.clone());
                    // Measuring a fan we do not drive would store the BIOS curve as calibration.
                    if !f.controllable {
                        c.abort("This fan can only be monitored on this board, so it cannot be calibrated.");
                    } else if !f.tach_detected && !self.fans.get(i).is_some_and(|r| r.seen_spinning) {
                        c.abort(NO_SPEED_SIGNAL_CALIBRATION);
                    } else if let Some(why) = calibration_role_refused(self.shared.config_snapshot().header_role(&f.id))
                    {
                        c.abort(why);
                    } else if self.crit_latched || self.cooling_alarm.is_some() || self.efficacy_alarm {
                        c.abort(ALARM_CALIBRATION);
                    } else if self.emergency {
                        c.abort("Fans are handed back to the BIOS. Select a fan profile first, then calibrate.");
                    } else if self.takeover_blocked.is_some() {
                        c.abort(
                            "Fan control is off because the BIOS fan settings could not be restored. Reboot first.",
                        );
                    } else if self.identify.as_ref().is_some_and(Identify::active) {
                        c.abort("The header identification is running. Finish or cancel it first.");
                    } else if self.cpu_median.is_none() {
                        // Like the identification wizard (audit FR-CTL-006): the sweep to 0 % has a
                        // temperature guard, and without a CPU temperature that guard sees nothing.
                        c.abort("The CPU temperature is unknown, so calibrating (which stops the fan) is not safe.");
                    } else {
                        log::info!("starting calibration of {name}");
                    }
                    self.calib = Some(c);
                }
            }
            Command::CancelCalibration => {
                if let Some(c) = &mut self.calib {
                    c.abort("Cancelled");
                }
            }
            Command::ResetAi(key) => {
                self.ai.reset(key.as_deref());
                self.save_ai(true);
            }
            Command::WriteDiagnostics => self.write_diagnostics(),
            Command::EmergencyRelease => {
                if let Some(c) = &mut self.calib {
                    c.abort("Cancelled: fans were handed back to the BIOS.");
                }
                self.stop_identify("Cancelled: fans were handed back to the BIOS.");
                self.emergency = true;
                self.release_all();
            }
            Command::Resume => self.emergency = false,
            Command::Wake => {}
            Command::Resumed => {
                log::info!("resume from sleep: resetting filters, re-applying hardware curves and failsafe");
                self.resume_pending = true;
            }
            Command::Identify(c) => self.identify_command(c),
        }
    }

    /// A header the identification wizard tests (and that therefore needs a confirmed role before
    /// non-BIOS modes run, [`mode_gate`]): controllable, and on the board's list or seen spinning.
    fn identifiable(&self, i: usize, f: &FanChannelDesc) -> bool {
        f.controllable && (f.present || self.fans.get(i).is_some_and(|r| r.seen_spinning))
    }

    /// Stops a running identification wizard (logged once as FRG-E209).
    fn stop_identify(&mut self, why: &str) {
        if let Some(w) = self.identify.as_mut().filter(|w| w.active()) {
            w.abort(why);
            self.errlog.event(ErrorCode::IdentifyStopped, "identify", why);
        }
    }

    fn identify_command(&mut self, c: IdentifyCmd) {
        match c {
            IdentifyCmd::Start => {
                if self.identify.as_ref().is_some_and(Identify::active) {
                    return;
                }
                let cfg = self.shared.config_snapshot();
                let refuse = if self.emergency {
                    Some("Fans are handed back to the BIOS. Select a fan mode first.".to_string())
                } else if self.takeover_blocked.is_some() {
                    Some("Fan control is off because the BIOS fan settings could not be restored. Reboot first.".into())
                } else if self.calib.as_ref().is_some_and(|c| !c.done()) {
                    Some("A fan calibration is running. Wait until it is finished.".into())
                } else if self.crit_latched || self.cooling_alarm.is_some() || self.efficacy_alarm {
                    Some("A CPU temperature or cooling alarm is active.".into())
                } else if self.cpu_median.is_none_or(|t| t > crate::control::identify::MAX_CPU_C) {
                    Some(format!(
                        "The CPU is above {:.0} °C or its temperature is unknown. Try again when the PC is idle.",
                        crate::control::identify::MAX_CPU_C
                    ))
                } else {
                    None
                };
                let w = match refuse {
                    Some(why) => {
                        self.errlog.event(ErrorCode::IdentifyStopped, "identify", &format!("not started: {why}"));
                        Identify::refused(&why)
                    }
                    None => {
                        let descs = self.hw.fans();
                        let headers: Vec<crate::control::identify::Header> = descs
                            .iter()
                            .enumerate()
                            .filter(|(i, f)| self.identifiable(*i, f))
                            .map(|(i, f)| crate::control::identify::Header {
                                index: i,
                                id: f.id.clone(),
                                name: f.default_name.clone(),
                                // Until the user confirmed fans here, the header may carry the pump
                                // (also when marked unused: a near-silent pump sounds like nothing).
                                pump_suspect: !matches!(
                                    cfg.header_role(&f.id),
                                    Some(HeaderRole::CpuFans | HeaderRole::RadiatorFans | HeaderRole::CaseFans)
                                ),
                            })
                            .collect();
                        let names = descs.iter().map(|f| f.default_name.clone()).collect();
                        log::info!("header identification started ({} header(s))", headers.len());
                        Identify::new(headers, names)
                    }
                };
                self.identify = Some(w);
            }
            // Closing a finished wizard.
            IdentifyCmd::Cancel if self.identify.as_ref().is_some_and(Identify::done) => self.identify = None,
            IdentifyCmd::Cancel => self.stop_identify("Cancelled."),
            c => {
                if let Some(w) = &mut self.identify {
                    w.command(c);
                }
            }
        }
    }

    /// Handles commands that arrived while a tick was running.
    fn drain_commands(&mut self) {
        loop {
            let Some(cmd) = self.rx.as_ref().and_then(|rx| rx.try_recv().ok()) else { break };
            self.handle(cmd);
        }
    }

    fn fan_name(&self, i: usize) -> String {
        let f = &self.hw.fans()[i];
        self.shared
            .config
            .read()
            .ok()
            .and_then(|c| c.fan_names.get(&f.id).cloned())
            .unwrap_or_else(|| f.default_name.clone())
    }

    /// Hands every fan back to the BIOS. Fans whose release did not verify stay `controlled`, so
    /// every tick retries them. Returns true if all fans were released.
    fn release_all(&mut self) -> bool {
        let held = self.hw.release_all();
        for (i, f) in self.fans.iter_mut().enumerate() {
            f.duty = None;
            f.hw_curve = None;
            if held.contains(&i) {
                // Still ours: keep it so the release is retried (and the crash snapshot is kept).
                f.controlled = true;
            } else {
                f.controlled = false;
            }
        }
        if !held.is_empty() {
            let names: Vec<String> = held.iter().filter(|&&i| i < self.fans.len()).map(|&i| self.fan_name(i)).collect();
            let detail = format!(
                "{}: handing back to the BIOS did not verify; retrying. Reboot if this persists.",
                names.join(", ")
            );
            let now = Instant::now();
            self.errlog.report(self.tick, now, ErrorCode::RestoreFailed, "release", &detail);
            push_notice(&mut self.sticky, ErrorCode::RestoreFailed, &detail, now);
        }
        self.update_snapshot_file();
        held.is_empty()
    }

    fn shutdown(&mut self) {
        log::info!("engine stopping: returning all fans to BIOS control");
        let mut released = self.release_all();
        // Retry fans whose release failed (a busy bus or a colliding access is usually transient).
        for _ in 0..5 {
            if released {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
            for i in 0..self.fans.len() {
                if self.fans[i].controlled && self.hw.release_fan(i).is_ok() {
                    self.fans[i].controlled = false;
                }
            }
            released = !self.fans.iter().any(|f| f.controlled);
        }
        self.save_ai(true);
        if let Some(Err(e)) = self.efficacy.save(true) {
            log::warn!("[{}] saving the cooling baseline failed: {e:#}", code_of(&e));
        }
        // Delete the crash snapshot only when the hardware confirms that nothing is held any more;
        // otherwise the watchdog / next start must still restore the BIOS settings.
        if !self.hw.info().simulated && self.takeover_blocked.is_none() {
            let path = self.snapshot_path.clone();
            if self.hw.crash_snapshot().is_none() {
                let _ = std::fs::remove_file(path);
            } else {
                log::error!(
                    "[{}] some fans could not be handed back to the BIOS; keeping {} for the watchdog",
                    ErrorCode::RestoreFailed,
                    path.display()
                );
                self.controlled_sig.clear(); // force a rewrite with the current saved state
                self.update_snapshot_file();
            }
        }
        self.save_config_now();
    }

    fn save_ai(&mut self, force: bool) {
        let r = if force { self.ai.save_now() } else { self.ai.save() };
        if let Err(e) = r {
            self.errlog.report(
                self.tick,
                Instant::now(),
                code_of(&e),
                "ai models",
                &format!("saving learned models failed: {e:#}"),
            );
        }
    }

    fn save_config_now(&mut self) {
        let edited = self.save_due.is_some();
        // Never hold the config lock across disk I/O (fsync): the UI and the RGB thread take it.
        let snapshot = self.shared.config.read().ok().map(|c| c.clone());
        let res = snapshot.map(|c| c.save()).unwrap_or(Ok(()));
        match res {
            Ok(()) => {
                self.config_save_error = None;
                if edited {
                    crate::config::set_load_error(None);
                }
            }
            Err(e) => {
                let msg = format!("{e:#}");
                self.errlog.report(self.tick, Instant::now(), ErrorCode::ConfigSave, "config", &msg);
                self.config_save_error = Some(msg);
            }
        }
        self.save_due = None;
    }

    fn update_snapshot_file(&mut self) {
        if self.hw.info().simulated || self.takeover_blocked.is_some() {
            // Demo mode, or recovery failed at start (the file holds the real BIOS state): never read,
            // write or delete the crash snapshot.
            return;
        }
        let sig: Vec<bool> = self.fans.iter().map(|f| f.controlled).collect();
        if sig == self.controlled_sig {
            return;
        }
        self.controlled_sig = sig;
        let path = self.snapshot_path.clone();
        match self.hw.crash_snapshot() {
            Some(snap) => {
                let res = serde_json::to_string_pretty(&snap)
                    .map_err(std::io::Error::other)
                    .and_then(|s| crate::util::fsx::write_atomic(&path, s.as_bytes()));
                match res {
                    Ok(()) => self.snapshot_failed_at = None,
                    Err(e) => {
                        self.errlog.report(
                            self.tick,
                            Instant::now(),
                            ErrorCode::Io,
                            "crash snapshot",
                            &format!(
                                "{}: {e}; fans without a hardware failsafe are handed back to the BIOS",
                                path.display()
                            ),
                        );
                        self.snapshot_failed_at = Some(Instant::now());
                        // Retry the write on the next change.
                        self.controlled_sig.clear();
                        // Without a recoverable snapshot, a crash could strand fans in manual mode:
                        // give back every fan the chip does not protect by itself.
                        for i in 0..self.fans.len() {
                            if self.fans[i].controlled && !self.hw.has_hw_failsafe(i) && self.hw.release_fan(i).is_ok()
                            {
                                self.fans[i].controlled = false;
                                self.fans[i].duty = None;
                                self.fans[i].hw_curve = None;
                            }
                        }
                    }
                }
            }
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    fn write_diagnostics(&mut self) {
        let info = self.hw.info().clone();
        let cfg = self.shared.config_snapshot();
        let mut s = crate::diag::report_header("the running app (Diagnostics page)", &info);
        s.push_str(&crate::diag::hardware_section(&info));
        s.push_str(&crate::diag::config_section(&cfg));
        s.push_str(&crate::diag::drivers_section(&crate::drivers::current_state()));
        if let Ok(st) = self.shared.state.read() {
            let sensors: Vec<_> = st.sensors.iter().map(|e| (e.desc.clone(), e.value)).collect();
            s.push_str(&crate::diag::sensors_section(&sensors));
            let profile = cfg.profile();
            let descs = self.hw.fans();
            let rows: Vec<crate::diag::ChannelRow> = st
                .fans
                .iter()
                .filter_map(|f| Some((f, descs.get(f.index)?)))
                .map(|(f, d)| {
                    let fc = profile.fan(&f.id, f.role);
                    let duty = match (f.duty, f.duty_est) {
                        (Some(x), _) => format!("{x:.0} %{}", if f.controlled { "" } else { " (read)" }),
                        (None, Some(e)) => format!("est. {e:.0} % (BIOS curve)"),
                        (None, None) => "-".into(),
                    };
                    let failsafe = match (f.controlled, f.hw_failsafe, self.failsafe_set) {
                        (true, true, Some((input, t))) => format!("100 % at {t:.0} °C (input {input})"),
                        (true, true, None) => "chip".into(),
                        (true, false, _) => "software only".into(),
                        (false, _, _) => "-".into(),
                    };
                    crate::diag::ChannelRow {
                        id: f.id.clone(),
                        header: d.default_name.clone(),
                        role: crate::diag::role_label(f.role, f.assigned),
                        connected: crate::diag::connected_label(d, f.speed_signal),
                        mode: f.mode.clone(),
                        source: if fc.mode == FanMode::Curve { fc.curve.source.key() } else { "-".into() },
                        duty,
                        failsafe,
                    }
                })
                .collect();
            s.push_str(&crate::diag::channels_section(&rows));
            let cooling: Vec<crate::diag::CoolingHeader> = st
                .fans
                .iter()
                .map(|f| crate::diag::CoolingHeader {
                    header: f.header.clone(),
                    role: f.role,
                    assigned: f.assigned,
                    present: !f.unconnected,
                    speed_signal: f.speed_signal,
                })
                .collect();
            s.push_str(&crate::diag::cooling_section(&cooling, Some(&st.efficacy)));
            if let Some(w) = &st.identify {
                s.push_str(&format!(
                    "  Header identification: {:?}, {} of {} header(s) answered{}\n",
                    w.phase,
                    w.results.iter().filter(|r| r.answer.is_some()).count(),
                    w.total,
                    w.error.as_ref().map(|e| format!(" ({e})")).unwrap_or_default()
                ));
            }
            if !self.notified.is_empty() {
                let codes: Vec<String> = self.notified.iter().map(|c| c.to_string()).collect();
                s.push_str(&format!("  Tray notifications shown: {}\n", codes.join(", ")));
            }
            s.push_str("\nAI:\n");
            for z in &st.ai {
                s.push_str(&format!("  {} [{}] conf {:.2}: {}\n", z.group, z.model_key, z.confidence, z.reason));
                let peak =
                    |v: &[f32]| v.iter().copied().fold(None, |a: Option<f32>, b| Some(a.map_or(b, |a| a.max(b))));
                s.push_str(&format!(
                    "    airflow {:.2} (floor {:.2}), confidence factors {:?}, forecast peak die {:?} / GPU {:?}\n",
                    z.airflow,
                    z.floor,
                    z.conf_factors,
                    peak(&z.predicted_die),
                    peak(&z.predicted_gpu)
                ));
                for p in &z.params {
                    s.push_str(&format!("    {:<44} {:>9.3} {}\n", p.name, p.value, p.unit));
                }
            }
            s.push_str("\nActive alerts:\n");
            for a in &st.alerts {
                s.push_str(&format!(
                    "  [{:?}] {} {}: {}\n",
                    a.level,
                    a.code.map(|c| c.to_string()).unwrap_or_default(),
                    a.title,
                    a.detail
                ));
            }
            s.push_str("\nRecent errors:\n");
            for e in &st.errors {
                s.push_str(&format!("  {} {} {} ({}): {}\n", e.time_unix, e.code, e.code.title(), e.source, e.detail));
            }
        }
        if let Ok(rgb) = self.shared.rgb.read() {
            s.push_str(&crate::diag::rgb_section(&cfg, &rgb));
        }
        if self.diag_job.is_some() {
            log::info!("diagnostics report already being written");
            return;
        }
        // The hardware dump needs the backend (engine thread); the system-wide queries and the file
        // write run on a helper thread so a stalled query can't hold the engine tick.
        let superio = self.hw.diagnostics_dump();
        let job = std::thread::Builder::new().name("fanrgb-diag".into()).spawn(move || {
            if info.simulated {
                // Demo: the RGB controller is simulated too; the real controller's USB state is not ours.
                s.push_str("RGB USB health: not checked (simulated hardware)\n");
            } else {
                let h = crate::diag::rgb_usb_health();
                s.push_str(&format!("RGB USB health: {h:?}\n"));
            }
            s.push_str(&crate::diag::dynamic_lighting_section());
            s.push_str(&crate::diag::conflicts_section());
            s.push_str("\nSuper I/O:\n");
            s.push_str(&superio);
            s.push_str(&crate::diag::log_section(50));
            // The report is meant to be attached to issues: no account name, no machine SID.
            let s = crate::diag::scrub_personal(&s);
            let path = crate::util::paths::diag_file();
            match crate::util::fsx::write_atomic(&path, s.as_bytes()) {
                Ok(()) => Ok(path.display().to_string()),
                Err(e) => Err(format!("{}: {e}", path.display())),
            }
        });
        match job {
            Ok(j) => self.diag_job = Some(j),
            Err(e) => self.errlog.report(
                self.tick,
                Instant::now(),
                ErrorCode::Internal,
                "diagnostics",
                &format!("could not start the report thread: {e}"),
            ),
        }
    }

    /// Picks up the results of the helper threads that finished (diagnostics report, periodic scan).
    fn collect_jobs(&mut self, now: Instant) {
        if let Some(r) = take_finished(&mut self.diag_job) {
            match r {
                Ok(Ok(path)) => {
                    log::info!("diagnostics written to {path}");
                    self.diag_path = Some(path);
                }
                Ok(Err(e)) => self.errlog.report(self.tick, now, ErrorCode::Io, "diagnostics", &e),
                Err(_) => {
                    self.errlog.report(self.tick, now, ErrorCode::Internal, "diagnostics", "the report thread panicked")
                }
            }
        }
        if let Some(r) = take_finished(&mut self.scan_job) {
            match r {
                Ok((health, conflicts)) => {
                    self.rgb_health = health;
                    self.conflicts = conflicts;
                }
                Err(_) => log::error!("[{}] the device / process scan thread panicked", ErrorCode::Internal),
            }
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let wall = SystemTime::now();
        let dt_raw = now.duration_since(self.last_tick).as_secs_f64();
        let wall_dt = wall.duration_since(self.last_wall).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        self.last_tick = now;
        self.last_wall = wall;
        self.tick += 1;
        let tick = self.tick;
        // "Still alive" for the hang watchdog.
        crate::system::watchdog_heartbeat();
        // Health signal for an update on probation (updater::install::start_probation).
        crate::updater::install::engine_tick();

        // Config changes: debounce saving, reset curve state on profile switch.
        let rev = self.shared.config_rev.load(Ordering::SeqCst);
        if rev != self.seen_rev {
            if self.seen_rev != 0 {
                self.save_due = Some(now + Duration::from_millis(1500));
            }
            self.seen_rev = rev;
        }
        if self.save_due.is_some_and(|t| now >= t) {
            self.save_config_now();
        }
        let mut cfg = self.shared.config_snapshot();
        // Never trust values from the UI or a hand-edited file to be safe.
        cfg.clamp_safety();
        let poll_s = cfg.general.poll_ms as f64 / 1000.0;
        if cfg.active_profile != self.last_profile {
            self.last_profile = cfg.active_profile;
            self.emergency = false;
            for f in &mut self.fans {
                f.curve.reset();
            }
            let name = cfg.profile().name.clone();
            // Logged and kept in the Diagnostics event ring (Info).
            self.errlog.event(ErrorCode::ProfileSwitched, "profile", &format!("switched to '{name}'"));
        }
        // Choosing the BIOS mode (tile, tray, hotkey) promises that nothing is driven: a running
        // identification stops and its header goes back at once. A wizard
        // started under the BIOS mode keeps running.
        let bios = cfg.profile().preset == Preset::Bios;
        if bios && !self.last_bios {
            self.stop_identify("Stopped: the BIOS fan mode was selected. Every header is back on the BIOS.");
        }
        self.last_bios = bios;

        // Sleep/resume or a stalled clock: never treat the gap as one control interval.
        let gap_limit = (3.0 * poll_s).max(poll_s + 2.0);
        let gap = tick > 1 && (dt_raw > gap_limit || wall_dt > gap_limit + 30.0);
        let resume_event = std::mem::take(&mut self.resume_pending);
        let resumed = resume_event || gap;
        if resumed && tick > 1 {
            log::info!("{:.1} s since the last tick (wall clock {wall_dt:.0} s): treating as a gap/resume", dt_raw);
            if dt_raw >= crate::system::HANG_TIMEOUT.as_secs_f64() && !self.hw.info().simulated {
                // The hang watchdog may have handed the fans to the BIOS and removed the crash snapshot.
                self.errlog.event(
                    ErrorCode::Watchdog,
                    "engine",
                    &format!("no engine tick for {dt_raw:.0} s (stall or sleep); fan control is re-applied"),
                );
                // A stall (no resume from sleep): the watchdog restored the BIOS settings, so the
                // backend's record of what it wrote is stale. Re-apply before this tick's poll
                // verifies the channels, or that check reports our own recovery as a firmware
                // override (FRG-E206) and counts it toward giving the fan up.
                // After a resume the BIOS reprograms the chip first: the delayed re-apply handles it.
                if !resume_event {
                    match self.hw.reapply() {
                        Ok(n) => log::info!("re-applied {n} fan setting(s) after the stall"),
                        Err(e) => log::warn!("re-apply after the stall failed: {e:#}"),
                    }
                }
            }
            // Re-write the crash snapshot (the hang watchdog deletes it when it restores the BIOS).
            self.controlled_sig.clear();
            for f in &mut self.fans {
                f.curve.reset();
                f.hw_curve = None; // re-program (the firmware may have reloaded its own curves)
                f.last_write = None;
            }
            self.failsafe_set = None;
            self.cpu_hist.clear();
            self.save_ai(false);
            // The BIOS reprograms the chip after resume; re-apply our settings once it settled.
            self.reapply_at = Some(now + Duration::from_secs(2));
        }
        if self.reapply_at.is_some_and(|t| now >= t) {
            self.reapply_at = None;
            match self.hw.reapply() {
                Ok(n) => log::info!("re-applied {n} fan setting(s) after resume"),
                Err(e) => self.errlog.report(
                    tick,
                    now,
                    code_of(&e),
                    "hardware",
                    &format!("re-apply after resume failed: {e:#}"),
                ),
            }
        }
        // Hardware events (firmware overrides, failed verifications, fans handed back).
        for ev in self.hw.take_events() {
            self.errlog.report(tick, now, ev.code, "hardware", &ev.detail);
            if ev.code.severity() >= Severity::Warning || ev.code == ErrorCode::FirmwareOverride {
                push_notice(&mut self.sticky, ev.code, &ev.detail, now);
            }
        }
        let dt = (dt_raw as f32).clamp(0.05, 5.0);

        // Software PWM without a hardware failsafe only while crash protection is active
        // (watchdog alive and the crash snapshot persisted).
        let snapshot_blocked = self.snapshot_failed_at.is_some_and(|t| now.duration_since(t) < Duration::from_secs(60));
        let crash_protected = self.hw.info().simulated || (crate::system::watchdog_active() && !snapshot_blocked);
        self.hw.set_software_fallback(crash_protected);

        // Hardware failsafe for all `set_fan` duties (chip forces 100 % on its own).
        let failsafe_c = cfg.general.cpu_critical.min(HW_FAILSAFE_MAX_C);
        if let Some(input) = self.hw.chip_temp_input(ids::CPU_TEMP)
            && self.failsafe_set != Some((input, failsafe_c))
        {
            // The backend logs the value it applies (once per change).
            self.hw.set_failsafe(input, failsafe_c);
            self.failsafe_set = Some((input, failsafe_c));
        }

        let t0 = Instant::now();
        let values = self.hw.poll();
        let poll_ms = t0.elapsed().as_secs_f32() * 1000.0;

        let sensors = self.hw.sensors().to_vec();
        let mut val: HashMap<String, f32> = HashMap::new();
        let mut entries = Vec::with_capacity(sensors.len());
        for (d, v) in sensors.iter().zip(values.iter()) {
            let mut v = v.filter(|x| x.is_finite());
            if let Some(x) = v {
                let plausible = match d.kind {
                    SensorKind::Temperature => (-40.0..=150.0).contains(&x),
                    SensorKind::Power if d.id.starts_with("gpu") => (0.0..=GPU_POWER_MAX_W).contains(&x),
                    SensorKind::Power => (0.0..=2000.0).contains(&x),
                    SensorKind::Fan => (0.0..=25_000.0).contains(&x),
                    SensorKind::Load | SensorKind::Duty => (-1.0..=101.0).contains(&x),
                    SensorKind::Voltage => (-50.0..=50.0).contains(&x),
                };
                if !plausible {
                    self.errlog.report(
                        tick,
                        now,
                        ErrorCode::ImplausibleReading,
                        &d.id,
                        &format!("{} reported {x}", d.name),
                    );
                    v = None;
                }
            }
            if let Some(x) = v {
                val.insert(d.id.clone(), x);
                let mm = self.minmax.entry(d.id.clone()).or_insert((x, x));
                mm.0 = mm.0.min(x);
                mm.1 = mm.1.max(x);
            }
            let mm = self.minmax.get(&d.id).copied();
            entries.push(SensorEntry { desc: d.clone(), value: v, min: mm.map(|m| m.0), max: mm.map(|m| m.1) });
        }

        // ---- safety: critical temperature latch (median-of-3 CPU against single-sample spikes) ----
        let cpu_t = val.get(ids::CPU_TEMP).copied();
        let cpu_listed = sensors.iter().any(|s| s.id == ids::CPU_TEMP);
        if let Some(t) = cpu_t {
            if self.cpu_hist.len() >= 3 {
                self.cpu_hist.pop_front();
            }
            self.cpu_hist.push_back(t);
            self.cpu_missing_since = None;
            self.cpu_missing_n = 0;
        } else if cpu_listed {
            self.cpu_missing_since.get_or_insert(now);
            self.cpu_missing_n += 1;
            // Never judge the critical temperature on stale values.
            if self.cpu_missing_n >= 3 {
                self.cpu_hist.clear();
            }
        }
        // The CPU temperature is unknown for too long: the critical latch must not release on
        // the assumption that the CPU is cool.
        let cpu_unknown = self.cpu_missing_since.is_some_and(|t| now.duration_since(t) > SENSOR_LOST_AFTER);
        let cpu_crit_t = match self.cpu_hist.len() {
            0 => None,
            3 => {
                let (a, b, c) = (self.cpu_hist[0], self.cpu_hist[1], self.cpu_hist[2]);
                Some(a.max(b).min(a.min(b).max(c)))
            }
            _ => self.cpu_hist.iter().copied().reduce(f32::min),
        };
        let gpu_t = val
            .iter()
            .filter(|(k, _)| k.starts_with("gpu") && k.ends_with(".temp"))
            .map(|(_, v)| *v)
            .fold(None, |a: Option<f32>, b| Some(a.map_or(b, |a| a.max(b))));
        let over = cpu_crit_t.is_some_and(|t| t >= cfg.general.cpu_critical)
            || gpu_t.is_some_and(|t| t >= cfg.general.gpu_critical);
        // FRG-E301 thresholds: control::limits (one place, hardware test step 6).
        let cool = !cpu_unknown
            && cpu_crit_t.is_none_or(|t| t < limits::critical_release_c(cfg.general.cpu_critical))
            && gpu_t.is_none_or(|t| t < limits::critical_release_c(cfg.general.gpu_critical));
        if over {
            if !self.crit_latched {
                log::warn!("critical temperature (CPU {cpu_t:?}, GPU {gpu_t:?}): forcing controlled fans to 100 %");
            }
            self.crit_latched = true;
            self.crit_ok_since = None;
        } else if self.crit_latched {
            if cool {
                let since = *self.crit_ok_since.get_or_insert(now);
                if now.duration_since(since) >= limits::CRITICAL_RELEASE_HOLD {
                    self.crit_latched = false;
                    self.crit_ok_since = None;
                    log::info!(
                        "temperatures back below critical − {:.0} °C for {} s: normal control resumes",
                        limits::CRITICAL_RELEASE_MARGIN_C,
                        limits::CRITICAL_RELEASE_HOLD.as_secs()
                    );
                }
            } else {
                self.crit_ok_since = None;
            }
        }
        let critical = self.crit_latched;
        self.cpu_median = cpu_crit_t;

        let fan_descs = self.hw.fans().to_vec();
        let profile = cfg.profile().clone();
        let calibrating_fan = self.calib.as_ref().filter(|c| !c.done()).map(|c| c.fan);
        // Start-up recovery failed: no fan is taken at all.
        let blocked = self.takeover_blocked.is_some();
        // Effective role and settings per header.
        let mut plans = self.plan_fans(&cfg, &profile, &fan_descs, &val);

        // ---- non-BIOS modes wait for one header identification ----
        let identifiable: Vec<bool> = fan_descs.iter().enumerate().map(|(i, f)| self.identifiable(i, f)).collect();
        let assigned: Vec<Option<HeaderRole>> = plans.iter().map(|p| p.assigned).collect();
        let gate = mode_gate(&identifiable, &assigned);
        let held = !gate.is_empty() && profile.preset != Preset::Bios;
        if held {
            // Every header on the BIOS plan: nothing is driven by the mode (the wizard and a
            // calibration of a confirmed header still drive their own header).
            for p in &mut plans {
                p.hp = HeaderPlan::new(FanConfig::preset(Preset::Bios, p.role), p.role, p.speed_signal);
                p.gated = true;
            }
        }
        self.mode_gate = gate.iter().map(|&i| fan_descs[i].default_name.clone()).collect();
        if held != self.gate_held {
            if held {
                log::info!(
                    "fan mode '{}' waits for the header identification ({} not confirmed): every header stays on the BIOS",
                    profile.name,
                    self.mode_gate.join(", ")
                );
            } else if gate.is_empty() {
                log::info!("every fan header has a confirmed device: fan mode '{}' applies", profile.name);
            }
            self.gate_held = held;
        }

        // ---- calibration bookkeeping for max-RPM estimates ----
        for (i, f) in fan_descs.iter().enumerate() {
            let rpm = f.rpm_sensor.as_ref().and_then(|s| val.get(s).copied());
            let rt = &mut self.fans[i];
            if rt.controlled && rt.duty.is_some_and(|d| d >= 95.0) {
                let since = *rt.full_since.get_or_insert(now);
                if now.duration_since(since) >= Duration::from_secs(4)
                    && let Some(r) = rpm
                {
                    rt.max_rpm_full = rt.max_rpm_full.max(r);
                }
            } else {
                rt.full_since = None;
            }
        }

        // ---- AI (one PC-wide controller, CPU group + case group) ----
        let (zone_duty, ai_views) =
            self.run_ai(&cfg, &plans, &fan_descs, &val, calibrating_fan, dt_raw, poll_s, resumed);
        if now.duration_since(self.last_ai_save) > Duration::from_secs(120) {
            self.save_ai(false);
            self.last_ai_save = now;
        }
        for (code, detail) in std::mem::take(&mut self.ai.notices) {
            self.errlog.report(tick, now, code, "ai", &detail);
            push_notice(&mut self.sticky, code, &detail, now);
        }
        // The AI's control temperature is gone: after 5 s its fans go to the BIOS like curve fans
        // instead of holding a stale duty.
        if ai_views.iter().any(|z| z.stale) {
            self.ai_stale_since.get_or_insert(now);
        } else {
            self.ai_stale_since = None;
        }
        let ai_lost = self.ai_stale_since.is_some_and(|t| now.duration_since(t) > SENSOR_LOST_AFTER);

        // ---- calibration ----
        let mut calib_view = None;
        if let Some(c) = &mut self.calib {
            let i = c.fan;
            let rpm = fan_descs[i].rpm_sensor.as_ref().and_then(|s| val.get(s).copied());
            if critical || cpu_t.is_some_and(|t| t > cfg.general.cpu_critical - limits::CALIBRATION_ABORT_MARGIN_C) {
                c.abort("Stopped: the CPU got too warm. Try again when the PC is idle.");
            }
            // No current reading at all (sensor absent, or three samples missed): the guard above is
            // blind (audit FR-CTL-006), so do not wait for the 5 s of `cpu_unknown`.
            if cpu_unknown || cpu_crit_t.is_none() {
                c.abort("Stopped: the CPU temperature is unavailable, so calibrating is not safe.");
            }
            if self.emergency {
                c.abort("Cancelled: fans were handed back to the BIOS.");
            }
            // The alarms force the CPU chain to 100 %, and a header re-tagged as pump / unknown
            // mid-run must not continue the sweep to 0 %.
            if self.cooling_alarm.is_some() || self.efficacy_alarm {
                c.abort("Stopped: a CPU cooling alarm is active.");
            }
            if let Some(why) = calibration_role_refused(plans[i].assigned) {
                c.abort(&format!("Stopped: {why}"));
            }
            c.step(dt, rpm);
            calib_view = Some(c.view());
            if c.done() {
                if let Some(err) = &c.error {
                    self.errlog.report(tick, now, ErrorCode::CalibrationFailed, &c.fan_id, err);
                } else {
                    let result = c.result.clone();
                    let id = c.fan_id.clone();
                    log::info!("calibration of {} finished: {:?}", c.fan_name, result);
                    self.shared.edit_config(|cfg| {
                        cfg.calibrations.insert(id, result);
                    });
                }
            }
        }

        // ---- header identification wizard ----
        if self.identify.as_ref().is_some_and(Identify::active) {
            let max_c = crate::control::identify::MAX_CPU_C;
            let why = if critical {
                Some("Stopped: critical temperature.".to_string())
            } else if self.cooling_alarm.is_some() || self.efficacy_alarm {
                Some("Stopped: a CPU cooling alarm is active.".into())
            } else if self.emergency || blocked {
                Some("Stopped: fans were handed back to the BIOS.".into())
            } else if resumed && tick > 1 {
                Some("Stopped: the PC slept or FanRGB was stalled.".into())
            } else {
                match cpu_crit_t {
                    None => Some("Stopped: the CPU temperature is unavailable.".into()),
                    Some(t) if t > max_c => Some(format!(
                        "Stopped: the CPU reached {t:.0} °C (the limit is {max_c:.0} °C). Try again when the PC is idle."
                    )),
                    _ => None,
                }
            };
            match why {
                Some(why) => self.stop_identify(&why),
                None => {
                    let rpms: Vec<Option<f32>> = fan_descs
                        .iter()
                        .zip(&plans)
                        .map(|(f, p)| {
                            f.rpm_sensor.as_ref().and_then(|s| val.get(s).copied()).filter(|_| p.speed_signal)
                        })
                        .collect();
                    if let Some(w) = &mut self.identify {
                        // Raw seconds (never the clamped `dt`): the 20 s pump limit is wall time.
                        w.step((dt_raw as f32).max(0.05), &rpms);
                    }
                }
            }
        }
        for wr in self.identify.as_mut().map(Identify::take_writes).unwrap_or_default() {
            log::info!("header identification: {} is {:?}", wr.fan_id, wr.role);
            self.shared.edit_config(|c| c.set_fan_role(&wr.fan_id, Some(wr.role), wr.name));
        }

        // ---- per-fan control ----
        let mut fan_states = Vec::with_capacity(fan_descs.len());
        let mut alerts = Vec::new();
        // Highest effective duty of the CPU / pump headers without a speed signal (cooler watch).
        let mut watched_duty: Option<f32> = None;
        let mut watched: Vec<String> = Vec::new();
        // Effective duty of the CPU cooling fans and of the pump (cooling-efficacy monitor).
        let mut chain_duty: Option<f32> = None;
        let mut pump_duty: Option<f32> = None;
        for (i, f) in fan_descs.iter().enumerate() {
            let plan = &plans[i];
            let fc = plan.hp.fc.clone();
            let cal = cfg.calibrations.get(&f.id);
            let rpm = f.rpm_sensor.as_ref().and_then(|s| val.get(s).copied());
            let speed_signal = plan.speed_signal;
            let unused = plan.unused;
            // A CPU-chain header whose fan or pump can't be seen stopping: guarded by the cooler watch.
            // "Unused" only stops *driving* a header: one that is present and may carry the cooler
            // stays watched (a near-silent pump is easily answered "Nothing
            // changed" in the wizard).
            let cooler_watched = plan.role.cools_cpu() && f.present && !speed_signal;
            let mut failsafe = false;
            let mut sensor_lost = false;
            let mut mode_label = match (unused, plan.assigned) {
                (true, Some(HeaderRole::Unused)) => "BIOS (unused)".to_string(),
                (true, _) => "BIOS (nothing connected)".to_string(),
                (false, _) if plan.gated => "BIOS (identify fans first)".to_string(),
                (false, _) => fc.mode.label().to_string(),
            };
            let calibrating = calibrating_fan == Some(i);
            let identify_duty = self.identify.as_ref().and_then(|w| w.duty_for(i));
            let identifying = identify_duty.is_some();
            // An unused header that may still be the CPU cooler goes to 100 % with the critical
            // latch and the cooling alarm (FRG-E305) like the driven CPU chain, except under BIOS.
            // FRG-E309 is inform-only in v0.2.
            let unused_forced = unused
                && fc.mode != FanMode::Bios
                && f.present
                && plan.role.cools_cpu()
                && (critical || self.cooling_alarm.is_some());

            // Hardware curve eligibility: the CPU temperature (the chip's CPU input) is the source, or
            // part of a `max` source whose other part becomes a software floor.
            let hw_plan = if fc.mode == FanMode::Curve
                && f.controllable
                && !self.emergency
                && !calibrating
                && !identifying
                && !critical
            {
                self.hw_curve_for(&fc, cal, failsafe_c, cfg.general.cpu_critical)
            } else {
                None
            };
            let mut gpu_lift = None;
            let hw_curve = match hw_plan {
                Some((c, others)) if !others.is_empty() => {
                    let temps: Vec<f32> = others.iter().filter_map(|s| val.get(s).copied()).collect();
                    match combine(&temps, Combine::Max) {
                        Some(t) => {
                            let floor = lift_bucket(self.fans[i].lift.step(&fc.curve, t, dt).0).min(fc.max_duty);
                            let lifted = lift_hw_curve(&c, floor);
                            gpu_lift = (lifted != c).then_some(floor);
                            Some(lifted)
                        }
                        // The other sensor is gone: the CPU part keeps running in the chip.
                        None => {
                            self.fans[i].lift.reset();
                            Some(c)
                        }
                    }
                }
                plan => {
                    self.fans[i].lift.reset();
                    plan.map(|(c, _)| c)
                }
            };
            // A chip input that reads garbage (-128 °C: thermistor missing) or a lost CPU temperature
            // would leave the chip running the curve's minimum: such curves run in software (or
            // the fan goes to the BIOS once the source stays lost).
            let hw_src_ok = hw_curve
                .as_ref()
                .is_some_and(|c| val.contains_key(ids::CPU_TEMP) && self.hw.chip_input_valid(i, c.temp_input));

            let rt = &mut self.fans[i];
            // A rejected curve is tried again after its back-off, or as soon as the curve changes.
            let retry_hw = match (&rt.hw_curve_failed, &hw_curve) {
                (Some((failed, retry_at)), Some(c)) => failed != c || retry_at.is_some_and(|t| now >= t),
                _ => false,
            };
            if retry_hw {
                rt.hw_curve_failed = None;
            }
            let mut bypass_ramp = false;
            let mut source_lost = false;
            let skip = !f.controllable
                || self.emergency
                || blocked
                || (unused && !calibrating && !identifying && !unused_forced);
            let mut target: Option<Output> = if skip {
                None
            } else if calibrating {
                mode_label = "Calibrating".into();
                Some(Output::Duty(self.calib.as_ref().unwrap().duty()))
            } else if let Some(d) = identify_duty {
                mode_label = "Identifying".into();
                Some(Output::Duty(d))
            } else if unused_forced {
                Some(Output::Duty(100.0))
            } else {
                match fc.mode {
                    FanMode::Bios => None,
                    FanMode::Fixed => {
                        mode_label = format!("Fixed {:.0}%", fc.fixed_duty);
                        Some(Output::Duty(fc.fixed_duty))
                    }
                    // One label whether the chip or the app runs the curve (`FanState::hw_curve` tells).
                    FanMode::Curve => match hw_curve
                        .filter(|c| hw_src_ok && rt.hw_curve_failed.as_ref().is_none_or(|(failed, _)| failed != c))
                    {
                        Some(c) => Some(Output::HwCurve(c)),
                        None => {
                            let temps: Vec<f32> =
                                fc.curve.source.sensors.iter().filter_map(|s| val.get(s).copied()).collect();
                            combine(&temps, fc.curve.source.combine)
                                .map(|t| Output::Duty(rt.curve.step(&fc.curve, t, dt).0))
                        }
                    },
                    FanMode::Ai => {
                        mode_label = format!("AI · {}", fc.ai.style.label());
                        if ai_lost {
                            source_lost = true;
                            None
                        } else {
                            zone_duty.get(&i).map(|&(duty, bypass)| {
                                bypass_ramp = bypass;
                                Output::Duty(duty)
                            })
                        }
                    }
                }
            };

            // Sensor loss: give the fan back to the BIOS (which has its own sensors) after 5 s,
            // holding the last setting meanwhile.
            if target.is_some() {
                rt.missing_since = None;
            }
            let wants_control = f.controllable
                && !self.emergency
                && !blocked
                && (calibrating || identifying || (fc.mode != FanMode::Bios && !unused));
            if wants_control && target.is_none() {
                let since = *rt.missing_since.get_or_insert(now);
                if source_lost || now.duration_since(since) > SENSOR_LOST_AFTER {
                    failsafe = true;
                    sensor_lost = true;
                    mode_label = "BIOS (sensor lost)".into();
                } else if let Some(d) = rt.duty {
                    target = Some(Output::Duty(d));
                } else if let Some(c) = rt.hw_curve.clone() {
                    // The chip keeps running the programmed curve meanwhile.
                    target = Some(Output::HwCurve(c));
                }
            }

            // The CPU temperature sensor exists but stopped reporting (audit FR-CTL-003): the
            // critical latch below can no longer see a hot CPU, so no software duty is kept, whatever
            // its own source is (a fixed duty, a board or GPU sensor, the rest of a `max` source).
            // The header goes back to the BIOS, which has its own sensors. A critical latch or cooling
            // alarm that is already up keeps forcing its 100 % (below); a curve the chip runs on its
            // own CPU input is not affected.
            let alarm_forces = self.cooling_alarm.is_some() && plan.role.cools_cpu() && f.present;
            if cpu_unknown && !critical && !alarm_forces && matches!(target, Some(Output::Duty(_))) {
                target = None;
                failsafe = true;
                sensor_lost = true;
                mode_label = "BIOS (CPU temperature lost)".into();
            }

            if let Some(Output::Duty(t)) = target {
                target = Some(Output::Duty(
                    if identifying {
                        t
                    } else if calibrating {
                        // Defence in depth: only confirmed fans calibrate, and they have no floor.
                        t.max(plan.hp.floor.unwrap_or(0.0))
                    } else if bypass_ramp {
                        // Safety increases (AI floor / limit latch) are never ramp-limited.
                        shape_duty(t, None, &fc, cal, dt).max(rt.duty.unwrap_or(0.0).min(t))
                    } else {
                        shape_duty(t, rt.duty, &fc, cal, dt)
                    }
                    // A role floor (pump / unknown) or the board's CPU / PUMP floor is a safety
                    // bound, never ramped up to: after the wizard's test duty or a role change the
                    // header is at its floor at once.
                    .max(if identifying || calibrating { 0.0 } else { plan.hp.running_floor() }),
                ));
            }
            // Hard floor near the critical temperature (audit FR-CTL-001), applied after everything
            // the settings file can influence (mode, min / max duty, role floors, calibration, AI)
            // and never ramp-limited. The two wizards keep their own temperature guards.
            if let Some(Output::Duty(t)) = target
                && !calibrating
                && !identifying
            {
                let floor = limits::envelope_floor(cpu_crit_t, cfg.general.cpu_critical);
                if !(t >= floor) {
                    target = Some(Output::Duty(floor));
                    failsafe = true;
                }
            }
            if critical && target.is_some() {
                target = Some(Output::Duty(100.0));
                failsafe = true;
                mode_label = "Failsafe 100%".into();
            } else if target.is_some() && self.cooling_alarm.is_some() && plan.role.cools_cpu() && f.present {
                // Suspected cooler failure (FRG-E305, raised by the headers we can't see): the whole
                // CPU chain, fans and pump, at full speed, as the alert says
                // (E305 used to force only the tach-less headers). FRG-E309 does not force (v0.2).
                target = Some(Output::Duty(100.0));
                failsafe = true;
                mode_label = "Failsafe 100% (cooling)".into();
            }
            // Crash protection: software duty control (no chip failsafe) needs the watchdog and a
            // persisted crash snapshot, otherwise a crash could strand the fan.
            // Checked every tick, also for fans already controlled: a fan can lose its hardware
            // failsafe while we drive it.
            if matches!(target, Some(Output::Duty(_))) && !crash_protected && !self.hw.has_hw_failsafe(i) {
                let why = if snapshot_blocked {
                    "the crash snapshot cannot be saved"
                } else {
                    "the safety watchdog is not running"
                };
                self.errlog.report(
                    tick,
                    now,
                    ErrorCode::Watchdog,
                    &f.id,
                    &format!("not using manual control: {why} and this fan has no hardware failsafe"),
                );
                target = None;
                failsafe = true;
                mode_label = "BIOS (no crash protection)".into();
            }

            // Apply.
            let calib_duty = calibrating && matches!(target, Some(Output::Duty(_)));
            let identify_out = identifying && matches!(target, Some(Output::Duty(_)));
            let mut driven = false;
            match target {
                None => {
                    if rt.controlled {
                        match self.hw.release_fan(i) {
                            Ok(()) => {
                                rt.controlled = false;
                                rt.duty = None;
                                rt.hw_curve = None;
                            }
                            Err(e) => {
                                // Keep `controlled` so the release is retried.
                                let code = code_of(&e);
                                self.errlog.report(
                                    tick,
                                    now,
                                    ErrorCode::RestoreFailed,
                                    &f.id,
                                    &format!("release failed ({code}): {e:#}"),
                                );
                            }
                        }
                    }
                }
                Some(Output::HwCurve(c)) => {
                    let due = rt.hw_curve.as_ref() != Some(&c)
                        || rt.hw_curve_at.is_none_or(|t| now.duration_since(t) > HW_CURVE_REFRESH);
                    if due {
                        match self.hw.set_fan_curve(i, &c) {
                            Ok(()) => {
                                if rt.hw_curve.as_ref() != Some(&c) {
                                    log::info!(
                                        "{}: curve programmed into the fan controller (input {}, full speed at {:.0} °C)",
                                        f.id,
                                        c.temp_input,
                                        c.full_speed_at
                                    );
                                }
                                rt.controlled = true;
                                rt.duty = None;
                                rt.hw_curve = Some(c);
                                rt.hw_curve_at = Some(now);
                                rt.hw_curve_failures = 0;
                                rt.write_errors = 0;
                                rt.curve.reset();
                            }
                            Err(e) => {
                                let code = code_of(&e);
                                match code {
                                    // The chip did not verify the curve (and handed the fan to the
                                    // BIOS): run it in software, retry the hardware with a back-off.
                                    ErrorCode::RegisterMismatch => {
                                        rt.hw_curve_failures = rt.hw_curve_failures.saturating_add(1);
                                        let wait = HW_CURVE_RETRY
                                            .saturating_mul(1 << (rt.hw_curve_failures - 1).min(4))
                                            .min(HW_CURVE_RETRY_MAX);
                                        self.errlog.report(
                                            tick,
                                            now,
                                            ErrorCode::HwCurveFallback,
                                            &f.id,
                                            &format!(
                                                "{code}: {e:#}; running the curve in software, hardware retry in {} s",
                                                wait.as_secs()
                                            ),
                                        );
                                        rt.hw_curve_failed = Some((c, Some(now + wait)));
                                    }
                                    // Not possible for this fan / curve: software until the curve changes.
                                    ErrorCode::FanReadOnly | ErrorCode::Internal | ErrorCode::NoSuchFan => {
                                        self.errlog.report(
                                            tick,
                                            now,
                                            ErrorCode::HwCurveFallback,
                                            &f.id,
                                            &format!("{code}: {e:#}; running the curve in software"),
                                        );
                                        rt.hw_curve_failed = Some((c, None));
                                    }
                                    // Transient (bus busy, a failed driver call): nothing changed,
                                    // retry on the next tick.
                                    _ => self.errlog.report(
                                        tick,
                                        now,
                                        code,
                                        &f.id,
                                        &format!("programming the hardware curve failed: {e:#}; retrying"),
                                    ),
                                }
                                rt.hw_curve = None;
                            }
                        }
                    }
                }
                Some(Output::Duty(d)) => {
                    let stale = rt.last_write.is_none_or(|w| now.duration_since(w) > Duration::from_secs(15));
                    if !rt.controlled
                        || rt.hw_curve.is_some()
                        || rt.duty.is_none_or(|cur| (cur - d).abs() >= 0.4)
                        || stale
                    {
                        match self.hw.set_fan(i, d) {
                            Ok(()) => {
                                rt.controlled = true;
                                rt.duty = Some(d);
                                rt.last_write = Some(now);
                                rt.write_errors = 0;
                                rt.last_error = None;
                                rt.hw_curve = None;
                                driven = true;
                            }
                            Err(e) => {
                                rt.write_errors += 1;
                                let code = code_of(&e);
                                let detail = format!("setting {d:.0} % failed: {e:#}");
                                self.errlog.report(tick, now, code, &f.id, &detail);
                                rt.last_error = Some((code, detail));
                                if rt.write_errors > 5 {
                                    match self.hw.release_fan(i) {
                                        Ok(()) => {
                                            rt.controlled = false;
                                            rt.duty = None;
                                        }
                                        Err(e2) => self.errlog.report(
                                            tick,
                                            now,
                                            ErrorCode::RestoreFailed,
                                            &f.id,
                                            &format!("release after write errors failed: {e2:#}"),
                                        ),
                                    }
                                }
                            }
                        }
                    } else {
                        driven = rt.controlled;
                    }
                }
            }
            // Calibration only measures what it drives: stop if the fan is not running at the
            // wizard's duty (released, blocked by the crash-protection gate, write errors), so an
            // RPM table of the BIOS curve is never stored.
            if calibrating
                && !(calib_duty && driven)
                && let Some(c) = self.calib.as_mut().filter(|c| !c.done())
            {
                c.abort("Stopped: FanRGB could not drive this fan, so the measurement would be wrong.");
                if let Some(err) = &c.error {
                    self.errlog.report(tick, now, ErrorCode::CalibrationFailed, &c.fan_id, err);
                }
                calib_view = Some(c.view());
            }
            // The same for the identification wizard: the user would judge the wrong thing.
            if identifying
                && !(identify_out && driven)
                && let Some(w) = self.identify.as_mut().filter(|w| w.active())
            {
                let why = "Stopped: FanRGB could not drive this header (see Diagnostics).";
                w.abort(why);
                self.errlog.event(ErrorCode::IdentifyStopped, &f.id, why);
            }
            if rt.controlled && rt.hw_curve.is_some() && !matches!(fc.mode, FanMode::Curve) {
                rt.hw_curve = None;
            }

            let name = self_fan_name(&cfg, f);
            let duty_reading = f.rpm_sensor.as_ref().and_then(|_| {
                let duty_id = f.id.replace("pwm", "duty");
                val.get(&duty_id).copied()
            });
            let hw_run = rt.controlled && rt.hw_curve.is_some();
            let shown_duty = if rt.controlled && !hw_run { rt.duty } else { duty_reading };
            let duty_est = if rt.controlled { None } else { self.hw.estimated_duty(i) };
            let max_of = |acc: Option<f32>, d: Option<f32>| match (acc, d) {
                (Some(a), Some(d)) => Some(a.max(d)),
                (a, d) => a.or(d),
            };
            if cooler_watched {
                watched.push(name.clone());
                watched_duty = max_of(watched_duty, shown_duty.or(duty_est));
            }
            if f.present {
                match plan.role {
                    FanRole::Cpu | FanRole::Unknown => chain_duty = max_of(chain_duty, shown_duty.or(duty_est)),
                    FanRole::Pump => pump_duty = max_of(pump_duty, shown_duty.or(duty_est)),
                    FanRole::System => {}
                }
            }

            // Stall detection for fans we drive.
            if rt.controlled
                && rt.seen_spinning
                && shown_duty.is_some_and(|d| d >= 35.0)
                && rpm.is_some_and(|r| r < 50.0)
            {
                let since = *rt.stalled_since.get_or_insert(now);
                if now.duration_since(since) > Duration::from_secs(8) {
                    alerts.push(
                        Alert::coded(
                            ErrorCode::FanStalled,
                            "It is commanded above 35 % but reports 0 RPM. Check the fan and its cable.",
                        )
                        .titled(format!("{name} is not spinning")),
                    );
                }
            } else {
                rt.stalled_since = None;
            }
            if sensor_lost {
                alerts.push(
                    Alert::coded(
                        ErrorCode::SensorLost,
                        format!(
                            "The temperature source of {name} stopped reporting; the BIOS controls it until it returns."
                        ),
                    )
                    .titled(format!("{name}: temperature source lost")),
                );
            }
            if rt.write_errors >= 3
                && let Some((code, detail)) = &rt.last_error
            {
                alerts.push(Alert::coded(*code, detail.clone()).titled(format!("{name}: {}", code.title())));
            }

            let visible = cfg
                .fan_visible
                .get(&f.id)
                .copied()
                .unwrap_or(f.present || rt.seen_spinning || cfg.general.show_all_headers);
            fan_states.push(FanState {
                id: f.id.clone(),
                index: i,
                name,
                header: f.default_name.clone(),
                role: plan.role,
                assigned: plan.assigned,
                // No tach: the 0 the header reports is not a speed.
                rpm: rpm.filter(|_| speed_signal),
                duty: shown_duty,
                mode: if !f.controllable {
                    "Read-only".into()
                } else if self.emergency {
                    "BIOS (released)".into()
                } else if blocked {
                    "BIOS (recovery failed)".into()
                } else {
                    mode_label
                },
                controlled: rt.controlled,
                controllable: f.controllable,
                visible,
                failsafe,
                hw_curve: hw_run,
                hw_failsafe: rt.controlled && self.hw.has_hw_failsafe(i),
                unconnected: f.possibly_unconnected(),
                speed_signal,
                duty_est,
                floor: Some(plan.hp.running_floor()).filter(|f| *f > 0.0 && !unused),
                ai_refused: plan.hp.ai_refused.filter(|_| !unused),
                gpu_lift: gpu_lift.filter(|_| hw_run),
                identifying,
            });
        }
        self.update_snapshot_file();

        // ---- cooler watch for CPU / pump headers without a speed signal ----
        if watched.is_empty() {
            self.cooler = CoolerWatch::default();
            self.cooling_alarm = None;
        } else {
            let inp = CoolerInput {
                tctl: cpu_crit_t,
                package_w: val.get(ids::CPU_POWER).copied(),
                duty: watched_duty,
                cpu_critical: cfg.general.cpu_critical,
            };
            let was = self.cooling_alarm;
            self.cooling_alarm = self.cooler.update(now, &inp);
            if let Some(trigger) = self.cooling_alarm {
                let t = cpu_crit_t.map(|t| format!("{t:.0} °C")).unwrap_or_else(|| "unknown".into());
                let why = match trigger {
                    Trigger::NearCritical => format!(
                        "CPU at {t}, within {:.0} °C of the critical {:.0} °C, although {} runs at {:.0} %",
                        limits::NEAR_CRITICAL_MARGIN_C,
                        cfg.general.cpu_critical,
                        watched.join(", "),
                        watched_duty.unwrap_or(0.0)
                    ),
                    Trigger::HotAtLowPower => format!(
                        "CPU at {t} while it draws only {} W",
                        inp.package_w.map(|p| format!("{p:.0}")).unwrap_or_else(|| "?".into())
                    ),
                };
                if was.is_none() {
                    log::warn!(
                        "[{}] {why}; controlled CPU/pump headers forced to 100 %",
                        ErrorCode::CoolingIneffective
                    );
                }
                alerts.push(Alert::coded(
                    ErrorCode::CoolingIneffective,
                    format!(
                        "{why}. {} report no speed signal, so a stopped fan or pump can't be seen directly. Controlled CPU fans and pumps run at 100 %.",
                        watched.join(", ")
                    ),
                ));
            } else if was.is_some() {
                log::info!("CPU temperature back to normal: cooling alarm cleared");
            }
        }

        // ---- cooling efficacy: FRG-E309 ----
        // Keyed on what the user confirmed, never on the role derived from this run's speed signal
        // (an unassigned header flipping Unknown <-> Cpu with fan-stop wiped the
        // baseline every boot). Every present header that is, or may be, in the CPU chain.
        let chain: Vec<String> = fan_descs
            .iter()
            .zip(&plans)
            .filter(|(f, p)| f.present && p.assigned.and_then(HeaderRole::fan_role).is_none_or(FanRole::cools_cpu))
            .map(|(f, p)| format!("{}:{:?}", f.id, p.assigned))
            .collect();
        self.efficacy.set_key(&format!("{}|{}", self.hw.info().cpu, chain.join(",")));
        if resumed {
            self.efficacy.reset_dynamics();
        }
        let eff_in = EfficacyInput {
            tctl: cpu_crit_t,
            tref: val.get(ids::SYSTEM_TEMP).copied(),
            power_w: val.get(ids::CPU_POWER).copied(),
            // The CPU fans' duty; a cooler that is only a pump is judged at the pump's duty.
            duty: chain_duty.or(pump_duty),
            forced: critical || self.cooling_alarm.is_some(),
        };
        let was = self.efficacy_alarm;
        // The alarm is for coolers whose failure can't be seen on a tach (the same headers as FRG-E305).
        self.efficacy_alarm = self.efficacy.update(dt, &eff_in, !watched.is_empty());
        let eff_view = self.efficacy.view();
        if self.efficacy_alarm {
            let e = eff_view.last;
            let detail = match e {
                Some(e) => format!(
                    "At {:.0} W and {:.0} % fan duty the CPU runs {:.1} K/W above the board temperature; {:.2} K/W were \
                     learned for this PC ({:+.0} °C). {EFFICACY_INFORM_ONLY}",
                    e.power_w,
                    e.duty,
                    e.r,
                    e.baseline.unwrap_or(0.0),
                    e.excess_c.unwrap_or(0.0)
                ),
                None => {
                    format!("The CPU ran much hotter than learned for its power and fan speed. {EFFICACY_INFORM_ONLY}")
                }
            };
            if !was {
                log::warn!("[{}] {detail}", ErrorCode::CoolingDegraded);
            }
            alerts.push(Alert::coded(ErrorCode::CoolingDegraded, detail));
        } else if was {
            log::info!("cooling efficacy back within the learned range: FRG-E309 cleared");
        }
        if let Some(Err(e)) = self.efficacy.save(false) {
            self.errlog.report(tick, now, code_of(&e), "cooling baseline", &format!("saving failed: {e:#}"));
        }

        if self.calib.as_ref().is_some_and(|c| c.done()) {
            // Keep the final view visible; the next tick will drop the wizard.
            self.calib = None;
        }

        // ---- periodic diagnostics (helper thread, results picked up by `collect_jobs`) ----
        self.collect_jobs(now);
        if self.scan_job.is_none()
            && self.last_diag_scan.is_none_or(|t| now.duration_since(t) > Duration::from_secs(30))
        {
            self.last_diag_scan = Some(now);
            let rgb_present = self.shared.rgb.read().map(|r| r.device.is_some()).unwrap_or(false);
            // Demo/simulated: the RGB controller is simulated too, so the real controller's USB state
            // is not reported.
            let check_usb = !(rgb_present || self.hw.info().simulated);
            let job = std::thread::Builder::new().name("fanrgb-scan".into()).spawn(move || {
                let health = if check_usb { crate::diag::rgb_usb_health().summary() } else { None };
                (health, crate::diag::conflicting_software())
            });
            match job {
                Ok(j) => self.scan_job = Some(j),
                Err(e) => log::warn!("[{}] device / process scan not started: {e}", ErrorCode::Internal),
            }
        }

        // ---- alerts (every alert carries a code) ----
        let info = self.hw.info();
        if !info.simulated {
            match &info.pawnio {
                PawnIoStatus::NotInstalled => alerts.push(Alert::coded(
                    ErrorCode::PawnIoMissing,
                    "Fan control and CPU/motherboard temperatures need the signed PawnIO driver.",
                )),
                PawnIoStatus::AccessDenied => alerts.push(Alert::coded(
                    ErrorCode::NotElevated,
                    "Restart FanRGB as administrator to control fans and read motherboard sensors.",
                )),
                PawnIoStatus::Error(e) => alerts.push(Alert::coded(ErrorCode::PawnIoModule, e.clone())),
                _ => {}
            }
        }
        if critical {
            alerts.push(Alert::coded(
                ErrorCode::CriticalTemperature,
                format!(
                    "CPU {} / GPU {}: all controlled fans forced to 100 % until 5 °C below critical for 10 s.",
                    cpu_t.map(|t| format!("{t:.0}°C")).unwrap_or("-".into()),
                    gpu_t.map(|t| format!("{t:.0}°C")).unwrap_or("-".into())
                ),
            ));
        }
        if self.emergency {
            alerts.push(Alert::coded(ErrorCode::FansReleased, "Select a fan profile to resume control."));
        }
        // Start-up problems stay as alerts; like every alert they enter the event ring once per
        // episode (below). The RestoreFailed of a blocked take-over has its own alert.
        for e in &self.startup_issues {
            if !(e.code == ErrorCode::RestoreFailed && self.takeover_blocked.as_deref() == Some(e.detail.as_str())) {
                alerts.push(Alert::coded(e.code, e.detail.clone()));
            }
        }
        if let Some(why) = &self.takeover_blocked {
            alerts.push(Alert::coded(
                ErrorCode::RestoreFailed,
                format!("{why}. FanRGB does not control any fan until the PC is restarted."),
            ));
        }
        for (name, why) in &self.conflicts {
            alerts.push(
                Alert::coded(ErrorCode::ConflictingSoftware, format!("It {why}. Close it for reliable control."))
                    .titled(format!("{name} is running")),
            );
        }
        if let Some(h) = &self.rgb_health {
            alerts.push(Alert::coded(ErrorCode::RgbHung, h.clone()));
        }
        if let Some(e) = crate::config::load_error() {
            alerts.push(Alert::coded(ErrorCode::ConfigLoad, e));
        }
        if let Some(e) = &self.config_save_error {
            alerts.push(Alert::coded(ErrorCode::ConfigSave, e.clone()));
        }
        // One alert per (code, subject), with a repeat count.
        self.sticky.retain(|n| now.duration_since(n.last) < NOTICE_FOR);
        for n in &self.sticky {
            let detail = if n.count > 1 { format!("{} (×{})", n.detail, n.count) } else { n.detail.clone() };
            alerts.push(Alert::coded(n.code, detail));
        }
        for a in &alerts {
            if let Some(code) = a.code {
                self.errlog.report(tick, now, code, &a.title, &a.detail);
            }
        }
        self.errlog.end_tick(tick);
        // Critical alerts reach the user through the tray icon too.
        if let Some(b) = self.notifier.update(now, &alerts, cfg.general.notifications) {
            if self.notified.len() >= 16 {
                self.notified.pop_front();
            }
            self.notified.push_back(b.code);
            crate::notify::show(b);
        }

        // A fan release or quit requested during this tick (e.g. by a panicking UI that may still
        // hold a read guard) is handled before publishing, never after it.
        self.drain_commands();
        if self.shared.quit.load(Ordering::SeqCst) || crate::system::UI_PANICKED.load(Ordering::SeqCst) {
            return;
        }

        // ---- publish ----
        let hist_vals: Vec<(String, Option<f32>)> = entries.iter().map(|e| (e.desc.id.clone(), e.value)).collect();
        // Never block for long behind a UI read guard (a UI panic while holding one must not keep
        // the engine from reaching its shutdown and handing the fans back): skip a sample instead.
        if let Some(mut h) = try_write_for(&self.shared.history, Duration::from_millis(100)) {
            h.push(self.shared.elapsed(), &hist_vals);
        }
        // The hidden UI no longer wakes on a short timer: it is woken here when what its tray icon
        // shows (the issue list) may have changed.
        // (The lighting status is skipped, not compared as missing, while its thread holds the lock.)
        let tray_changed = match self.shared.rgb.try_read() {
            Ok(rgb) => {
                let sig = tray_signature(&alerts, &rgb);
                std::mem::replace(&mut self.tray_sig, sig) != sig
            }
            Err(_) => false,
        };
        if let Some(mut st) = try_write_for(&self.shared.state, Duration::from_millis(100)) {
            st.info = self.hw.info().clone();
            st.sensors = entries;
            st.fans = fan_states;
            st.ai = ai_views;
            st.calibration = calib_view;
            st.alerts = alerts;
            st.tick = self.tick;
            st.poll_ms = poll_ms;
            st.emergency_released = self.emergency;
            st.diag_path = self.diag_path.clone();
            st.rgb_health = self.rgb_health.clone();
            st.conflicts = self.conflicts.clone();
            st.identify = self.identify.as_ref().map(Identify::view);
            st.mode_gate = self.mode_gate.clone();
            st.cpu_cooling_verified = if self.cooling_alarm.is_some() { Some(false) } else { eff_view.verified() };
            st.efficacy = eff_view;
            if st.errors_rev != self.errlog.rev {
                st.errors = self.errlog.ring.iter().cloned().collect();
                st.errors_rev = self.errlog.rev;
            }
        }
        if tray_changed {
            self.shared.force_repaint();
        } else {
            self.shared.request_repaint();
        }
    }

    /// Effective role and settings of every header for this tick: the user's role assignment (or
    /// the board's guess, `Unknown` for an unconfirmed header without a speed signal), the profile's
    /// entry for that role, and the role's safety rules ([`HeaderPlan`]).
    fn plan_fans(
        &mut self,
        cfg: &Config,
        profile: &crate::config::Profile,
        fans: &[FanChannelDesc],
        val: &HashMap<String, f32>,
    ) -> Vec<FanPlan> {
        fans.iter()
            .enumerate()
            .map(|(i, f)| {
                let rpm = f.rpm_sensor.as_ref().and_then(|s| val.get(s).copied());
                if rpm.is_some_and(|r| r > 100.0) {
                    self.fans[i].seen_spinning = true;
                }
                let speed_signal = f.tach_detected || self.fans[i].seen_spinning;
                let assigned = cfg.header_role(&f.id);
                let role = effective_role(assigned, f, speed_signal);
                // Nothing connected (not documented, no tach): left to the BIOS unless the user configured
                // this header explicitly or assigned it a role; a
                // header the user marked unused is not driven (only forced to 100 % by the
                // cooling alarms while it may be the cooler).
                let unused = cfg.header_unused(&f.id)
                    || (assigned.is_none()
                        && !f.present
                        && !self.fans[i].seen_spinning
                        && !profile.fans.contains_key(&f.id));
                FanPlan {
                    role,
                    assigned,
                    speed_signal,
                    unused,
                    gated: false,
                    hp: HeaderPlan::new(profile.fan(&f.id, role), role, speed_signal).with_board_floor(f.role),
                }
            })
            .collect()
    }

    /// The hardware curve for a Curve-mode fan, if the chip can run it, and the sensors whose curve
    /// value software turns into a floor. A chip channel follows exactly one
    /// input, the CPU temperature here, so a curve on a board sensor (`mb.t0`, …) would have no CPU
    /// failsafe if the app hangs:
    /// * source `cpu.temp` alone: chip curve, no floor;
    /// * source `max(cpu.temp, others…)`: chip curve on the CPU input, floor from the others
    ///   (`curve(max(a, b)) = max(curve(a), curve(b))` for the non-decreasing curves the chip runs);
    /// * anything else runs in software as a flat chip curve that keeps the CPU failsafe ramp
    ///   (`set_fan`), whatever the fan's role.
    fn hw_curve_for(
        &self,
        fc: &FanConfig,
        cal: Option<&FanCalibration>,
        failsafe_c: f32,
        cpu_critical: f32,
    ) -> Option<(HwCurve, Vec<String>)> {
        let others = chip_curve_floor_sensors(&fc.curve.source)?;
        let input = self.hw.chip_temp_input(ids::CPU_TEMP)?;
        Some((envelope_hw_curve(&build_hw_curve(fc, cal, input, true, failsafe_c), cpu_critical), others))
    }

    /// Builds the inputs for the PC-wide AI controller, runs it and maps the result to duties.
    /// Returns fan index → (duty, bypass ramp limits) and the views for the UI.
    #[allow(clippy::too_many_arguments)]
    fn run_ai(
        &mut self,
        cfg: &Config,
        plans: &[FanPlan],
        fans: &[FanChannelDesc],
        val: &HashMap<String, f32>,
        calibrating_fan: Option<usize>,
        dt_raw: f64,
        poll_s: f64,
        resumed: bool,
    ) -> (HashMap<usize, (f32, bool)>, Vec<AiZoneView>) {
        let mut ai_grp: [Vec<usize>; 2] = [Vec::new(), Vec::new()];
        let mut phys: [Vec<usize>; 2] = [Vec::new(), Vec::new()];
        for (i, f) in fans.iter().enumerate() {
            let p = &plans[i];
            let g = if p.role.cools_cpu() { 0 } else { 1 };
            if f.present && !p.unused {
                phys[g].push(i);
            }
            // Pumps, unknown-role and tach-less headers never get AI mode (`HeaderPlan`); a header
            // the calibration or identification wizard drives is left out meanwhile.
            let busy = calibrating_fan == Some(i) || self.identify.as_ref().is_some_and(|w| w.duty_for(i).is_some());
            if f.controllable
                && !self.emergency
                && self.takeover_blocked.is_none()
                && !busy
                && p.hp.fc.mode == FanMode::Ai
                && !p.unused
            {
                ai_grp[g].push(i);
            }
        }
        // A header without a speed signal reports no usable RPM: left out of the measured airflow
        // (`HeaderPlan` keeps it out of the AI groups altogether).
        let speed_signal = |i: usize| plans[i].speed_signal;
        let cpu_cfg: Option<AiConfig> = ai_grp[0].first().map(|&i| plans[i].hp.fc.ai.clone());
        let case_cfg: Option<AiConfig> = ai_grp[1].first().map(|&i| plans[i].hp.fc.ai.clone());
        let style = cpu_cfg.as_ref().or(case_cfg.as_ref()).map(|c| c.style).unwrap_or(AiStyle::Balanced);
        let cpu_src = cpu_cfg.as_ref().map(|c| c.source.clone()).unwrap_or_default();
        let tctl =
            combine(&cpu_src.sensors.iter().filter_map(|s| val.get(s).copied()).collect::<Vec<_>>(), cpu_src.combine);
        if ai_grp[0].is_empty() && ai_grp[1].is_empty() && tctl.is_none() {
            return (HashMap::new(), Vec::new());
        }

        // Fingerprint / key: physics only (CPU, temperature source, physical fan sets).
        let max_rpm = |i: usize| -> Option<f32> {
            let cal = cfg.calibrations.get(&fans[i].id).filter(|c| c.max_rpm > 0.0).map(|c| c.max_rpm);
            cal.or_else(|| (self.fans[i].max_rpm_full > 300.0).then_some(self.fans[i].max_rpm_full))
        };
        let mut fp = Fingerprint { cpu: self.hw.info().cpu.clone(), temp_src: cpu_src.key(), ..Default::default() };
        for &i in &phys[0] {
            fp.cpu_fans.insert(fans[i].id.clone(), cfg.calibrations.get(&fans[i].id).map(|c| c.max_rpm).unwrap_or(0.0));
        }
        for &i in &phys[1] {
            fp.case_fans
                .insert(fans[i].id.clone(), cfg.calibrations.get(&fans[i].id).map(|c| c.max_rpm).unwrap_or(0.0));
        }
        let key = model_key(&fp);

        // Measured airflow per group: the AI fans, else every spinning / calibrated fan of the role.
        let measure = |g: usize| -> Option<f64> {
            let set: Vec<usize> = if !ai_grp[g].is_empty() {
                ai_grp[g].clone()
            } else {
                phys[g]
                    .iter()
                    .copied()
                    .filter(|&i| self.fans[i].seen_spinning || cfg.calibrations.contains_key(&fans[i].id))
                    .collect()
            };
            let samples: Vec<(Option<f32>, Option<f32>, bool)> = set
                .iter()
                .map(|&i| (fans[i].rpm_sensor.as_ref().and_then(|s| val.get(s).copied()), max_rpm(i), speed_signal(i)))
                .collect();
            measured_airflow(&samples)
        };
        let group_params = |g: usize| -> (f64, f64, AirMap) {
            let set = if ai_grp[g].is_empty() { &phys[g] } else { &ai_grp[g] };
            let mut u_min: f64 = 0.0;
            let mut k = 0.0;
            let mut map = [0.0f64; 11];
            for &i in set {
                let f = &fans[i];
                let fc = &plans[i].hp.fc;
                let cal = cfg.calibrations.get(&f.id);
                u_min = u_min.max(duty_to_airflow(effective_min(fc, cal), cal) as f64);
                k += (max_rpm(i).unwrap_or(1500.0) as f64 / 1500.0).powf(1.5);
                for (d, m) in map.iter_mut().enumerate() {
                    *m = m.max(duty_to_airflow(d as f32 * 10.0, cal) as f64);
                }
            }
            if set.is_empty() {
                return (0.36, if g == 0 { 1.0 } else { 1.6 }, AirMap::default());
            }
            (u_min, k, AirMap(map))
        };
        let (u_min_cpu, k_cpu, air_cpu) = group_params(0);
        let (u_min_case, k_case, air_case) = group_params(1);

        let tbp = cfg.ai.gpu_tbp_w as f64;
        let power_kind = self.hw.sensors().iter().find(|s| s.id == ids::GPU_POWER).map(|s| s.kind);
        let (p_gpu, p_gpu_est) =
            gpu_power_w(power_kind, val.get(ids::GPU_POWER).copied(), val.get(ids::GPU_LOAD).copied(), tbp);
        let inp = SystemInputs {
            tctl: tctl.map(|t| t as f64),
            tsys: val.get(ids::SYSTEM_TEMP).map(|&t| t as f64),
            tgpu: val.get(ids::GPU_TEMP).map(|&t| t as f64),
            p_cpu: val.get(ids::CPU_POWER).map(|&p| p as f64),
            cpu_load: val.get(ids::CPU_LOAD).map(|&l| l as f64),
            p_gpu,
            p_gpu_estimated: p_gpu_est,
            u_cpu: measure(0),
            u_case: measure(1),
            u_min_cpu,
            u_min_case,
            k_cpu,
            k_case,
            air_cpu,
            air_case,
            dt_raw,
            poll_s,
            resumed,
            calibrating: calibrating_fan.is_some(),
            wall_s: unix_now() as f64,
            local_hour: local_hour(),
        };
        let cpu_ai_cfg = cpu_cfg.clone().unwrap_or_else(|| AiConfig::for_role(style, FanRole::Cpu));
        let case_ai_cfg = case_cfg.clone().unwrap_or_else(|| AiConfig::for_role(style, FanRole::System));
        let cpu_limit = cpu_ai_cfg.limit.min(CPU_AI_LIMIT_MAX).min(cfg.general.cpu_critical - 1.0);
        let gpu_limit = case_ai_cfg.limit.min(cfg.general.gpu_critical - 1.0);
        let tg = Targets {
            cpu: GroupGoal {
                target: cpu_ai_cfg.target.min(cpu_limit - 3.0) as f64,
                limit: cpu_limit as f64,
                style: cpu_ai_cfg.style,
            },
            gpu: GroupGoal {
                target: case_ai_cfg.target.min(gpu_limit - 3.0) as f64,
                limit: gpu_limit as f64,
                style: case_ai_cfg.style,
            },
            cpu_ai: !ai_grp[0].is_empty(),
            case_ai: !ai_grp[1].is_empty(),
            quiet_ceiling: cfg.ai.quiet_ceiling,
            probes: cfg.ai.probes,
            ppt_w: cfg.ai.cpu_ppt_w as f64,
            tbp_w: tbp,
        };
        let d: Decision = self.ai.step(&key, &fp, &inp, &tg).clone();

        let mut out = HashMap::new();
        let mut views = Vec::new();
        let model = self.ai.model(&key).cloned().unwrap_or_default();
        for g in 0..2 {
            if ai_grp[g].is_empty() {
                continue;
            }
            let (u, opt, safety, state, floor, ceil) = if g == 0 {
                (d.u_cpu, d.opt_cpu, d.safety_cpu || d.latched || d.stale, d.state_cpu, d.floor_cpu, d.ceil_cpu)
            } else {
                (d.u_case, d.opt_case, d.safety_case || d.stale, d.state_case, d.floor_case, d.ceil_case)
            };
            let mut duty_shown = 0.0;
            for &i in &ai_grp[g] {
                let cal = cfg.calibrations.get(&fans[i].id);
                let duty = if d.latched && g == 0 { 100.0 } else { airflow_to_duty(u as f32, cal) };
                duty_shown = duty;
                out.insert(i, (duty, safety));
            }
            let acfg = if g == 0 { &cpu_ai_cfg } else { &case_ai_cfg };
            let use_gpu = g == 1 && !d.predicted_gpu.is_empty();
            let series = if use_gpu { &d.predicted_gpu } else { &d.predicted_die };
            let (target, limit) = if use_gpu { (tg.gpu.target, tg.gpu.limit) } else { (tg.cpu.target, tg.cpu.limit) };
            let temp = if use_gpu { inp.tgpu.unwrap_or(d.t_filtered) } else { d.t_filtered };
            views.push(AiZoneView {
                key: format!("{}|{key}|{}", acfg.source.key(), if g == 0 { "cpu" } else { "case" }),
                fans: ai_grp[g].iter().map(|&i| self_fan_name(cfg, &fans[i])).collect(),
                source: if use_gpu { "GPU + CPU via case air".into() } else { cpu_src.sensors.join(" / ") },
                style: Some(acfg.style),
                target: target as f32,
                limit: limit as f32,
                temp: temp as f32,
                airflow: u as f32,
                model_airflow: opt as f32,
                predicted_peak: series.iter().copied().fold(f32::MIN, f32::max).max(temp as f32),
                predicted_end: series.get(17).copied().unwrap_or(temp as f32),
                duty: duty_shown,
                confidence: d.confidence as f32,
                samples: model.learn_samples,
                err_h: model.val_err as f32,
                predicted: series.clone(),
                reason: describe(&state, u, &d),
                rise: [
                    model.rise_per_100w(0.3) as f32,
                    model.rise_per_100w(0.6) as f32,
                    model.rise_per_100w(1.0) as f32,
                ],
                time_constant: model.tau_hs(0.6) as f32,
                group: if g == 0 { "CPU".into() } else { "Case".into() },
                model_key: key.clone(),
                params: crate::control::ai::explain(&model),
                conf_factors: d.conf_factors.map(|v| v as f32),
                confidence_why: crate::control::ai::confidence_why(&model),
                forecast_step_s: crate::control::thermal::MPC_DT as f32,
                predicted_die: d.predicted_die.clone(),
                predicted_gpu: d.predicted_gpu.clone(),
                workload: d.class.label().into(),
                quiet_ceiling: ceil.map(|c| c as f32),
                ceiling_active: d.ceiling_active && ceil.is_some(),
                floor: floor as f32,
                latched: d.latched,
                stale: d.stale,
                distrusted: model.distrusted,
                probing: d.probing && g == 0,
                n_inf: model.n_inf,
                learning: d.learn.describe(),
            });
        }
        (out, views)
    }
}

fn describe(state: &SupState, u: f64, d: &Decision) -> String {
    let mut s = state.describe(u);
    if d.confidence < 0.35 && !d.latched && !d.stale {
        s.push_str(&format!(
            " Learning your cooling ({:.0} % confident): staying on the safe curve meanwhile.",
            d.confidence * 100.0
        ));
    }
    if d.probing {
        s.push_str(" (Short probe to measure the fan effect.)");
    }
    s
}

/// Maps a software curve onto a chip curve: min/max duty applied, full speed no later than the
/// failsafe for CPU-sourced curves.
pub fn build_hw_curve(
    fc: &FanConfig,
    cal: Option<&FanCalibration>,
    input: u8,
    cpu_source: bool,
    failsafe_c: f32,
) -> HwCurve {
    let min = if fc.allow_stop { 0.0 } else { effective_min(fc, cal) };
    let mut points: Vec<(f32, f32)> = fc.curve.points.iter().map(|p| (p[0], p[1].max(min).min(fc.max_duty))).collect();
    points.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut full = points
        .iter()
        .find(|p| p.1 >= 99.5)
        .map(|p| p.0)
        .unwrap_or_else(|| points.last().map(|p| p.0 + 10.0).unwrap_or(80.0));
    full = full.min(110.0);
    if cpu_source {
        full = full.min(failsafe_c);
    }
    HwCurve { temp_input: input, points, full_speed_at: full, delta_t: Some(chip_delta_t(fc.curve.hysteresis)) }
}

/// The curve's hysteresis as the chip's SmartGuardian ΔT (whole °C, 0..=3: the register's
/// range). The software deadband keeps the full value for software curves.
pub fn chip_delta_t(hysteresis: f32) -> u8 {
    if hysteresis.is_finite() { hysteresis.round().clamp(0.0, 3.0) as u8 } else { 2 }
}

/// For a curve the chip can run on its CPU input: the other sensors of its source, whose curve value
/// becomes a software floor (empty: none). `None`: the curve can't run in the chip (no CPU
/// temperature in the source, or an average).
pub fn chip_curve_floor_sensors(src: &crate::config::TempSource) -> Option<Vec<String>> {
    if !src.sensors.iter().any(|s| s == ids::CPU_TEMP) {
        return None;
    }
    let others: Vec<String> = src.sensors.iter().filter(|s| *s != ids::CPU_TEMP).cloned().collect();
    (others.is_empty() || src.combine == Combine::Max).then_some(others)
}

/// Step of the software floor lifted into a chip curve: the floor only moves in 5 % steps (to the
/// nearest step), which keeps re-programming the chip rare and idempotent. The ±2.5 % this costs is
/// well inside the curve deadband, and the chip's own CPU curve is never lowered.
pub const LIFT_STEP: f32 = 5.0;

pub fn lift_bucket(duty: f32) -> f32 {
    if duty.is_finite() { ((duty / LIFT_STEP).round() * LIFT_STEP).clamp(0.0, 100.0) } else { 100.0 }
}

/// `c` with every point raised to at least `floor`: the chip then outputs `max(curve(CPU), floor)`
/// at every temperature, and a hung app leaves a CPU-tracking curve with the failsafe in place.
pub fn lift_hw_curve(c: &HwCurve, floor: f32) -> HwCurve {
    let floor = if floor.is_finite() { floor.clamp(0.0, 100.0) } else { 100.0 };
    let points: Vec<(f32, f32)> = c.points.iter().map(|&(t, d)| (t, d.max(floor))).collect();
    let full = points.iter().find(|p| p.1 >= 99.5).map_or(c.full_speed_at, |p| p.0.min(c.full_speed_at));
    HwCurve { points, full_speed_at: full, ..c.clone() }
}

/// `c` (a curve the chip runs on its CPU input) with the hard cooling floor of audit FR-CTL-001 merged
/// in. That floor depends only on the CPU temperature and `cpu_critical`, so it can live in the chip
/// curve itself, which also keeps it there if the app hangs, with nothing reprogrammed as the
/// temperature moves.
///
/// The curve's own points are all kept, in their order (a step at one temperature stays a step for
/// the chip fit, audit FR-HW-001), each raised to the floor at its own temperature. Points are added
/// only where none is near: at the floor's two corners below `full_speed_at`, and at `full_speed_at`
/// itself (from there the chip runs 100 %; the stretch below must not be interpolated towards a later
/// point, nor held flat while the floor still rises), each at the higher of the curve and the floor. Between two points the chip interpolates linearly,
/// and on every such stretch both the curve and the floor are linear, so their maximum never rises
/// above that chord: the chip never runs below either.
pub fn envelope_hw_curve(c: &HwCurve, cpu_critical: f32) -> HwCurve {
    let mut own: Vec<(f32, f32)> = c.points.iter().copied().filter(|p| p.0.is_finite() && p.1.is_finite()).collect();
    if own.is_empty() {
        return c.clone();
    }
    own.sort_by(|a, b| a.0.total_cmp(&b.0)); // stable: the points of a step keep their order
    let floor = |t: f32| limits::envelope_floor(Some(t), cpu_critical);
    // The curve away from its own points (where it has one value; flat before the first and after
    // the last point, as the chip holds them).
    let user = |t: f32| -> f32 {
        let (first, last) = (own[0], own[own.len() - 1]);
        if t <= first.0 {
            return first.1;
        }
        if t >= last.0 {
            return last.1;
        }
        let i = own.windows(2).position(|w| t <= w[1].0).unwrap_or(0);
        let (a, b) = (own[i], own[i + 1]);
        if b.0 - a.0 <= f32::EPSILON { b.1.max(a.1) } else { a.1 + (b.1 - a.1) * (t - a.0) / (b.0 - a.0) }
    };
    let near = |t: f32| own.iter().any(|p| (p.0 - t).abs() < 0.05);
    let full = c.full_speed_at;
    let mut extra: Vec<f32> =
        [cpu_critical - limits::ENVELOPE_START_MARGIN_C, cpu_critical - limits::ENVELOPE_FULL_MARGIN_C]
            .into_iter()
            .filter(|t| t.is_finite() && (!full.is_finite() || *t < full))
            .collect();
    // Always a point at full speed (unless an own point is there): from it the chip runs 100 %, and
    // without it the stretch below could be interpolated towards a later own point, under the floor
    // (the chip's full speed is capped at 90 °C, below `cpu_critical - 2` once that is 92 or more).
    if full.is_finite() {
        extra.push(full);
    }
    extra.retain(|t| !near(*t));
    extra.sort_by(f32::total_cmp);
    extra.dedup_by(|a, b| (*a - *b).abs() < 0.05);
    let added: Vec<(f32, f32)> = extra.into_iter().map(|t| (t, user(t))).collect();
    let mut points: Vec<(f32, f32)> =
        own.into_iter().chain(added).map(|(t, d)| (t, d.max(floor(t)).clamp(0.0, 100.0))).collect();
    points.sort_by(|a, b| a.0.total_cmp(&b.0)); // stable; added points are never near an own point
    let full_at = points.iter().find(|p| p.1 >= 99.5).map_or(full, |p| p.0.min(full));
    HwCurve { points, full_speed_at: full_at, ..c.clone() }
}

/// Mean airflow fraction of a fan group from (rpm, max rpm, has a speed signal) per fan. Fans without
/// a speed signal, a reading or a known maximum are skipped; the others still count, so one tach-less
/// header doesn't blank the whole group.
fn measured_airflow(samples: &[(Option<f32>, Option<f32>, bool)]) -> Option<f64> {
    let used: Vec<f64> = samples
        .iter()
        .filter_map(|&(rpm, max, signal)| match (rpm, max) {
            (Some(r), Some(m)) if signal && m > 0.0 => Some((r / m).clamp(0.0, 1.05) as f64),
            _ => None,
        })
        .collect();
    (!used.is_empty()).then(|| used.iter().sum::<f64>() / used.len() as f64)
}

/// Plausible range of a GPU board-power sensor in watts.
pub const GPU_POWER_MAX_W: f32 = 600.0;

/// GPU board power for the AI (W) and whether it is an estimate. A watts sensor (Intel Arc) is used
/// as it is; a percentage sensor (the documented D3DKMT unit) is scaled by the configured board
/// power; without either, the load-based estimate is used.
fn gpu_power_w(kind: Option<SensorKind>, value: Option<f32>, load: Option<f32>, tbp_w: f64) -> (Option<f64>, bool) {
    match (kind, value) {
        (Some(SensorKind::Power), Some(w)) => (Some(w as f64), false),
        (Some(SensorKind::Load), Some(pct)) => (Some(pct as f64 * tbp_w / 100.0), false),
        _ => (load.map(|l| l as f64 * tbp_w * 0.85 / 100.0), true),
    }
}

fn self_fan_name(cfg: &Config, f: &crate::model::FanChannelDesc) -> String {
    cfg.fan_names.get(&f.id).cloned().unwrap_or_else(|| f.default_name.clone())
}

/// Local hour of day (for the quiet-ceiling time buckets).
fn local_hour() -> u8 {
    #[cfg(windows)]
    {
        // SAFETY: GetLocalTime only writes the provided SYSTEMTIME.
        unsafe {
            let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = std::mem::zeroed();
            windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut st);
            (st.wHour as u8).min(23)
        }
    }
    #[cfg(not(windows))]
    {
        ((unix_now() / 3600) % 24) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FanMode, Preset};
    use crate::hw::sim::SimHw;
    use crate::model::AlertLevel;

    /// The hidden UI is woken when the tray's warning state may change (an alert appears or goes, the
    /// lighting's connection or error changes), not when only a live value in an alert's text does.
    #[test]
    fn tray_signature_tracks_issues_not_live_values() {
        let hot = |t: f32| vec![Alert::coded(ErrorCode::CriticalTemperature, format!("CPU {t:.0}°C"))];
        let rgb = crate::rgb::RgbStatus::default();
        let base = tray_signature(&hot(96.0), &rgb);
        assert_eq!(base, tray_signature(&hot(97.0), &rgb), "detail only");
        assert_ne!(base, tray_signature(&[], &rgb), "alert gone");
        let two = [hot(96.0), vec![Alert::coded(ErrorCode::FansReleased, "x")]].concat();
        assert_ne!(base, tray_signature(&two, &rgb), "alert added");
        let mut lost = rgb.clone();
        lost.last_error = Some(crate::rgb::link::RgbErrorInfo {
            code: ErrorCode::RgbDisconnected,
            detail: String::new(),
            at: SystemTime::now(),
            count: 1,
        });
        assert_ne!(base, tray_signature(&hot(96.0), &lost), "lighting error");
    }

    /// This test process's private folder, `%TEMP%\fanrgb-engine-tests-<pid>`: two test runs (parallel
    /// worktrees, CI jobs) never share settings, snapshots or learned models.
    fn test_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("fanrgb-engine-tests-{}", std::process::id()))
    }

    fn isolated() {
        let dir = test_dir();
        let _ = std::fs::create_dir_all(&dir);
        // SAFETY: every test in this process sets the same value before touching any path.
        unsafe { std::env::set_var("FANRGB_HOME", &dir) };
    }

    fn engine(cfg: Config) -> Engine {
        engine_with(cfg, Box::new(SimHw::new(1.0)))
    }

    /// A test engine on a PC whose headers were identified once (non-BIOS modes are gated behind
    /// that): see [`identified`]. The gate itself is tested with
    /// [`engine_unidentified`].
    fn engine_with(cfg: Config, hw: Box<dyn HwBackend>) -> Engine {
        let cfg = identified(cfg, hw.as_ref());
        engine_unidentified(cfg, hw)
    }

    /// A test engine with the roles exactly as `cfg` has them (a fresh config: none confirmed).
    fn engine_unidentified(cfg: Config, hw: Box<dyn HwBackend>) -> Engine {
        isolated();
        let shared = Shared::new(cfg);
        Engine::new(shared, hw)
    }

    /// `cfg` after one identification whose answers keep every header's effective role: fans where
    /// the board says fans and a tach shows them, "Not sure" (Unknown) where no tach does, the pump on
    /// a pump header. Roles `cfg` already has are kept; names are not touched.
    fn identified(mut cfg: Config, hw: &dyn HwBackend) -> Config {
        for f in hw.fans().iter().filter(|f| f.controllable && f.present) {
            let role = match (f.role, f.tach_detected) {
                (FanRole::Pump, _) => HeaderRole::Pump,
                (FanRole::Cpu, true) => HeaderRole::CpuFans,
                (FanRole::System, true) => HeaderRole::CaseFans,
                _ => HeaderRole::Unknown,
            };
            cfg.fan_roles.entry(f.id.clone()).or_insert(role);
        }
        cfg
    }

    fn with_profile(preset: Preset) -> Config {
        let mut cfg = Config::default();
        cfg.active_profile = cfg.profiles.iter().position(|p| p.preset == preset).unwrap();
        cfg
    }

    /// `cfg` with every sim header confirmed as fans (calibration needs that).
    fn confirmed(mut cfg: Config) -> Config {
        cfg.set_fan_role("mb.pwm0", Some(HeaderRole::CpuFans), None);
        cfg.set_fan_role("mb.pwm1", Some(HeaderRole::CaseFans), None);
        cfg.set_fan_role("mb.pwm2", Some(HeaderRole::CaseFans), None);
        cfg
    }

    fn fans(e: &Engine) -> Vec<FanState> {
        e.shared.state.read().unwrap().fans.clone()
    }

    /// SimHw plus hardware curves / failsafe, recording what the engine programs.
    struct CurveHw {
        inner: SimHw,
        curves: Arc<Mutex<Vec<(usize, HwCurve)>>>,
        failsafe: Arc<Mutex<Option<(u8, f32)>>>,
        reject: bool,
    }

    impl HwBackend for CurveHw {
        fn info(&self) -> &HwInfo {
            self.inner.info()
        }
        fn sensors(&self) -> &[crate::model::SensorDesc] {
            self.inner.sensors()
        }
        fn fans(&self) -> &[crate::model::FanChannelDesc] {
            self.inner.fans()
        }
        fn poll(&mut self) -> Vec<Option<f32>> {
            self.inner.poll()
        }
        fn set_fan(&mut self, fan: usize, duty: f32) -> anyhow::Result<()> {
            self.inner.set_fan(fan, duty)
        }
        fn set_fan_curve(&mut self, fan: usize, curve: &HwCurve) -> anyhow::Result<()> {
            if self.reject {
                return Err(crate::error::err(ErrorCode::RegisterMismatch, "test"));
            }
            self.curves.lock().unwrap().push((fan, curve.clone()));
            Ok(())
        }
        fn set_failsafe(&mut self, input: u8, at: f32) {
            *self.failsafe.lock().unwrap() = Some((input, at));
        }
        fn chip_temp_input(&self, id: &str) -> Option<u8> {
            match id {
                "cpu.temp" => Some(2),
                "mb.t0" => Some(0),
                _ => None,
            }
        }
        fn has_hw_failsafe(&self, _fan: usize) -> bool {
            true
        }
        fn release_fan(&mut self, fan: usize) -> anyhow::Result<()> {
            self.inner.release_fan(fan)
        }
        fn release_all(&mut self) -> Vec<usize> {
            self.inner.release_all()
        }
        fn crash_snapshot(&self) -> Option<crate::hw::CrashSnapshot> {
            None
        }
    }

    fn curve_hw(reject: bool) -> (Box<CurveHw>, Arc<Mutex<Vec<(usize, HwCurve)>>>, Arc<Mutex<Option<(u8, f32)>>>) {
        let curves = Arc::new(Mutex::new(Vec::new()));
        let fs = Arc::new(Mutex::new(None));
        (Box::new(CurveHw { inner: SimHw::new(1.0), curves: curves.clone(), failsafe: fs.clone(), reject }), curves, fs)
    }

    #[test]
    fn bios_profile_never_takes_control() {
        let mut e = engine(with_profile(Preset::Bios));
        for _ in 0..5 {
            e.tick();
        }
        assert!(fans(&e).iter().all(|f| !f.controlled && f.duty.is_none()));
    }

    #[test]
    fn fixed_profile_controls_and_restores_on_shutdown() {
        let mut e = engine(with_profile(Preset::FullSpeed));
        e.tick();
        let f = fans(&e);
        assert!(f.iter().all(|f| f.controlled && f.duty == Some(100.0)), "{f:?}");
        e.shutdown();
        assert!(e.fans.iter().all(|f| !f.controlled));
    }

    #[test]
    fn critical_temperature_forces_full_speed() {
        let mut cfg = with_profile(Preset::Silent);
        cfg.general.cpu_critical = 60.0;
        cfg.sanitize(); // clamps to the 60 °C minimum; the simulated CPU idles below it
        let mut e = engine(cfg);
        e.tick();
        assert!(fans(&e).iter().all(|f| f.controlled && !f.failsafe));
        // Make every temperature "critical" by lowering the threshold below idle.
        e.shared.edit_config(|c| c.general.cpu_critical = 10.0);
        e.tick();
        let f = fans(&e);
        assert!(f.iter().all(|f| f.failsafe && f.duty == Some(100.0)), "{f:?}");
        assert!(e.shared.state.read().unwrap().alerts.iter().any(|a| a.level == AlertLevel::Critical));
        // Latched: raising the threshold again does not release before 10 s of cool-down.
        e.shared.edit_config(|c| c.general.cpu_critical = 90.0);
        e.tick();
        assert!(fans(&e).iter().all(|f| f.failsafe), "critical latch holds for 10 s");
    }

    #[test]
    fn lost_sensor_returns_fan_to_bios() {
        let mut cfg = with_profile(Preset::Balanced);
        let mut fc = cfg.profile().fan("mb.pwm1", crate::model::FanRole::System);
        fc.mode = FanMode::Curve;
        fc.curve.source.sensors = vec!["does.not.exist".into()];
        cfg.profile_mut().fans.insert("mb.pwm1".into(), fc);
        let mut e = engine(cfg);
        e.tick();
        assert!(!fans(&e)[1].failsafe, "grace period first");
        std::thread::sleep(Duration::from_millis(5200));
        e.tick();
        let f = &fans(&e)[1];
        assert!(f.failsafe && !f.controlled, "{f:?}");
        assert!(fans(&e)[0].controlled, "other fans keep working");
        let st = e.shared.state.read().unwrap();
        assert!(st.alerts.iter().any(|a| a.code == Some(ErrorCode::SensorLost)));
        assert!(st.errors.iter().any(|ev| ev.code == ErrorCode::SensorLost));
    }

    #[test]
    fn emergency_release_and_resume() {
        let mut e = engine(with_profile(Preset::Performance));
        e.tick();
        assert!(fans(&e).iter().all(|f| f.controlled));
        e.handle(Command::EmergencyRelease);
        e.tick();
        assert!(fans(&e).iter().all(|f| !f.controlled));
        assert!(e.shared.state.read().unwrap().alerts.iter().all(|a| a.code.is_some()), "every alert is coded");
        e.handle(Command::Resume);
        e.tick();
        assert!(fans(&e).iter().all(|f| f.controlled));
    }

    #[test]
    fn curve_fans_run_in_hardware_once_and_after_resume() {
        // Balanced curves: CPU fan follows cpu.temp (chip input) → hardware; case fans follow
        // max(cpu, gpu) → hardware on the CPU input with a software-lifted floor.
        let (hw, curves, fs) = curve_hw(false);
        let mut e = engine_with(with_profile(Preset::Balanced), hw);
        for _ in 0..5 {
            e.tick();
        }
        let f = fans(&e);
        assert!(f[0].hw_curve && f[0].controlled && f[0].hw_failsafe, "{:?}", f[0]);
        assert!(f[1].hw_curve && f[1].controlled && f[1].mode == "Curve", "{:?}", f[1]);
        let per_fan = |i: usize| curves.lock().unwrap().iter().filter(|c| c.0 == i).count();
        assert_eq!((per_fan(0), per_fan(1), per_fan(2)), (1, 1, 1), "programmed once, not every tick");
        let pushed = curves.lock().unwrap().clone();
        assert!(pushed.iter().all(|c| c.1.temp_input == 2 && c.1.full_speed_at <= 90.0), "{pushed:?}");
        assert_eq!(*fs.lock().unwrap(), Some((2, 90.0)), "failsafe from config (cpu_critical 90)");
        e.handle(Command::Resumed);
        e.tick();
        assert_eq!(per_fan(0), 2, "re-programmed after resume");
        // Changing the curve re-programs it.
        e.shared.edit_config(|c| {
            let fc = c.profile_mut().fan_mut("mb.pwm0", FanRole::Cpu);
            fc.curve.points[0][1] = 33.0;
        });
        e.tick();
        assert_eq!(per_fan(0), 3);
    }

    #[test]
    fn rejected_hardware_curve_falls_back_to_software() {
        let (hw, _curves, _) = curve_hw(true);
        let mut e = engine_with(with_profile(Preset::Balanced), hw);
        for _ in 0..3 {
            e.tick();
        }
        let f = fans(&e);
        assert!(!f[0].hw_curve && f[0].controlled && f[0].duty.is_some(), "{:?}", f[0]);
        let st = e.shared.state.read().unwrap();
        for id in ["mb.pwm0", "mb.pwm1", "mb.pwm2"] {
            let n = st.errors.iter().filter(|ev| ev.code == ErrorCode::HwCurveFallback && ev.source == id).count();
            assert_eq!(n, 1, "{id} logged once: {:?}", st.errors);
        }
    }

    #[test]
    fn ai_profile_drives_all_fans_with_insight() {
        let mut e = engine(with_profile(Preset::AiAuto));
        for _ in 0..30 {
            e.tick();
        }
        let f = fans(&e);
        assert!(f.iter().all(|f| f.controlled && f.duty.is_some()), "{f:?}");
        let st = e.shared.state.read().unwrap();
        assert_eq!(st.ai.len(), 2, "CPU and case group views");
        let z = &st.ai[0];
        assert!(!z.params.is_empty() && !z.reason.is_empty() && !z.confidence_why.is_empty());
        assert_eq!(z.predicted.len(), crate::control::thermal::MPC_STEPS);
        assert!(z.key.starts_with("Max:cpu.temp|"), "UI parses the source from the key: {}", z.key);
    }

    #[test]
    fn error_log_rate_limits() {
        let mut l = ErrorLog::default();
        let t0 = Instant::now();
        for k in 1..=100u64 {
            l.report(k, t0 + Duration::from_secs(k), ErrorCode::BusBusy, "mb.pwm0", "busy");
            l.end_tick(k);
        }
        assert_eq!(l.ring.len(), 1, "one episode, one event");
        // Gone, then back after 2 minutes: new episode.
        l.end_tick(101);
        l.report(250, t0 + Duration::from_secs(250), ErrorCode::BusBusy, "mb.pwm0", "busy");
        assert_eq!(l.ring.len(), 2);
        for k in 0..(ERROR_RING_LEN as u64 + 50) {
            l.report(300 + 2 * k, t0 + Duration::from_secs(300 + 200 * k), ErrorCode::Io, &format!("s{k}"), "x");
        }
        assert_eq!(l.ring.len(), ERROR_RING_LEN, "bounded");
    }

    /// SimHw that can pose as real hardware (`simulated = false`), with or without a chip failsafe,
    /// and with a crash snapshot that lists every fan currently under software control.
    struct SnapHw {
        inner: SimHw,
        info: HwInfo,
        hw_failsafe: bool,
        controlled: Vec<bool>,
    }

    impl SnapHw {
        fn new(simulated: bool, hw_failsafe: bool) -> Self {
            let inner = SimHw::new(1.0);
            let mut info = inner.info().clone();
            info.simulated = simulated;
            let n = inner.fans().len();
            SnapHw { inner, info, hw_failsafe, controlled: vec![false; n] }
        }
    }

    impl HwBackend for SnapHw {
        fn info(&self) -> &HwInfo {
            &self.info
        }
        fn sensors(&self) -> &[crate::model::SensorDesc] {
            self.inner.sensors()
        }
        fn fans(&self) -> &[crate::model::FanChannelDesc] {
            self.inner.fans()
        }
        fn poll(&mut self) -> Vec<Option<f32>> {
            self.inner.poll()
        }
        fn set_fan(&mut self, fan: usize, duty: f32) -> anyhow::Result<()> {
            self.controlled[fan] = true;
            self.inner.set_fan(fan, duty)
        }
        fn has_hw_failsafe(&self, _fan: usize) -> bool {
            self.hw_failsafe
        }
        fn release_fan(&mut self, fan: usize) -> anyhow::Result<()> {
            self.controlled[fan] = false;
            self.inner.release_fan(fan)
        }
        fn release_all(&mut self) -> Vec<usize> {
            self.controlled.iter_mut().for_each(|c| *c = false);
            self.inner.release_all()
        }
        fn crash_snapshot(&self) -> Option<crate::hw::CrashSnapshot> {
            let saved: crate::hw::superio::ite::SavedPwm = serde_json::from_str(r#"{"ctrl":128,"duty":64}"#).unwrap();
            let channels: Vec<_> =
                (0..self.controlled.len()).filter(|&i| self.controlled[i]).map(|i| (i, saved)).collect();
            (!channels.is_empty()).then(|| {
                crate::hw::CrashSnapshot::now(vec![crate::hw::ChipSnapshot {
                    model: crate::hw::superio::ite::IteModel::IT8689E,
                    base: 0xa40,
                    channels,
                }])
            })
        }
    }

    /// On real hardware, a fan without a chip failsafe is never put under software
    /// duty control while the crash watchdog is not running (tests never start one).
    #[test]
    fn no_software_control_without_watchdog_or_hw_failsafe() {
        assert!(!crate::system::watchdog_active());
        let mut e = engine_with(with_profile(Preset::FullSpeed), Box::new(SnapHw::new(false, false)));
        for _ in 0..3 {
            e.tick();
        }
        let f = fans(&e);
        assert!(f.iter().all(|f| !f.controlled && f.failsafe), "{f:?}");
        assert!(f.iter().all(|f| f.mode.contains("no crash protection")), "{f:?}");
        {
            let st = e.shared.state.read().unwrap();
            assert!(st.errors.iter().any(|ev| ev.code == ErrorCode::Watchdog), "{:?}", st.errors);
        }
        // With a chip failsafe the same profile takes control.
        let mut e = engine_with(with_profile(Preset::FullSpeed), Box::new(SnapHw::new(false, true)));
        e.tick();
        assert!(fans(&e).iter().all(|f| f.controlled && f.duty == Some(100.0)));
        e.shutdown();
    }

    /// Engine -> control_snapshot.json -> watchdog validator: the file the engine writes while it
    /// controls fans is accepted by `system::validate_snapshot` as the watchdog sees it (other pid),
    /// lists exactly the controlled channels, and nothing stays controlled after a clean shutdown.
    #[test]
    fn crash_snapshot_round_trips_through_the_watchdog_validator() {
        // Not simulated: a simulated backend never writes a snapshot (the test then only passed when
        // another engine test had left one in the shared test home).
        let mut e = engine_with(with_profile(Preset::FullSpeed), Box::new(SnapHw::new(false, true)));
        e.tick();
        assert!(fans(&e).iter().all(|f| f.controlled));
        let path = crate::util::paths::control_snapshot_file();
        // Other engine tests share the test home and delete this file when they start: re-write it
        // and read it back immediately, retrying a few times.
        let mut text = None;
        for _ in 0..50 {
            e.controlled_sig.clear();
            e.update_snapshot_file();
            if let Ok(t) = std::fs::read_to_string(&path)
                && t.contains("chips")
            {
                text = Some(t);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let text = text.expect("snapshot written while fans are controlled");
        let snap: crate::hw::CrashSnapshot = serde_json::from_str(&text).unwrap();
        assert_eq!(snap.pid, std::process::id());
        let idx: Vec<usize> = snap.chips[0].channels.iter().map(|(i, _)| *i).collect();
        assert_eq!(idx, vec![0, 1, 2]);
        assert!(snap.boot_id.is_some() || snap.uptime_ms.is_some(), "snapshot carries the boot stamp");
        let written = crate::system::BootStamp { boot_id: snap.boot_id, uptime_ms: snap.uptime_ms };
        let boot = crate::util::win::boot_time_unix();
        let now = crate::system::boot_stamp_now();
        let watchdog_pid = std::process::id().wrapping_add(1);
        let ok = crate::system::validate_snapshot(&snap, written, boot, now, Some(std::process::id()), watchdog_pid)
            .expect("the watchdog accepts the engine's own snapshot");
        assert_eq!(ok.chips[0].channels.len(), 3);
        // The watchdog of another FanRGB process leaves this snapshot alone.
        assert!(crate::system::validate_snapshot(&snap, written, boot, now, Some(1), watchdog_pid).is_err());
        e.shutdown();
        assert!(e.fans.iter().all(|f| !f.controlled));
        assert!(e.hw.crash_snapshot().is_none(), "nothing left to recover after a clean shutdown");
    }

    /// SimHw with the CPU heatsink pinned at 30 °C, so long runs (calibration) never reach the
    /// "CPU got too warm" abort whatever the simulated workload does.
    struct CoolHw(SimHw);

    impl HwBackend for CoolHw {
        fn info(&self) -> &HwInfo {
            self.0.info()
        }
        fn sensors(&self) -> &[crate::model::SensorDesc] {
            self.0.sensors()
        }
        fn fans(&self) -> &[crate::model::FanChannelDesc] {
            self.0.fans()
        }
        fn poll(&mut self) -> Vec<Option<f32>> {
            self.0.set_heatsink(30.0);
            self.0.poll()
        }
        fn set_fan(&mut self, fan: usize, duty: f32) -> anyhow::Result<()> {
            self.0.set_fan(fan, duty)
        }
        fn release_fan(&mut self, fan: usize) -> anyhow::Result<()> {
            self.0.release_fan(fan)
        }
        fn release_all(&mut self) -> Vec<usize> {
            self.0.release_all()
        }
        fn crash_snapshot(&self) -> Option<crate::hw::CrashSnapshot> {
            None
        }
    }

    fn calibrated(e: &Engine, id: &str) -> Option<FanCalibration> {
        e.shared.config.read().unwrap().calibrations.get(id).cloned()
    }

    /// Switching the profile while a fan is being calibrated: the wizard keeps that fan (100 % spin-up,
    /// then its steps), the other fans follow the new profile, and the result is stored.
    #[test]
    fn profile_switch_while_calibrating_keeps_the_wizard_in_charge() {
        let mut e = engine_with(confirmed(with_profile(Preset::Balanced)), Box::new(CoolHw(SimHw::new(1.0))));
        e.tick();
        e.handle(Command::Calibrate(0));
        e.tick();
        assert_eq!(fans(&e)[0].mode, "Calibrating");
        let bios = e.shared.config.read().unwrap().profiles.iter().position(|p| p.preset == Preset::Bios).unwrap();
        e.shared.edit_config(|c| c.active_profile = bios);
        e.tick();
        let f = fans(&e);
        assert_eq!(f[0].mode, "Calibrating", "{:?}", f[0]);
        assert!(f[0].controlled && f[0].duty == Some(100.0), "spin-up continues: {:?}", f[0]);
        assert!(!f[1].controlled && !f[2].controlled, "other fans follow the BIOS profile: {f:?}");
        let mut done = false;
        for _ in 0..5000 {
            e.tick();
            if e.calib.is_none() {
                done = true;
                break;
            }
        }
        assert!(done, "calibration finished");
        let errs: Vec<String> = e.shared.state.read().unwrap().errors.iter().map(|x| x.detail.clone()).collect();
        let cal = calibrated(&e, "mb.pwm0").unwrap_or_else(|| panic!("no result stored; errors {errs:?}"));
        assert!((cal.max_rpm - 1850.0).abs() < 150.0, "{cal:?}");
        e.tick();
        assert!(!fans(&e)[0].controlled, "after the wizard the BIOS profile applies");
    }

    /// "Release fans to BIOS" during a calibration must not store a calibration that was measured
    /// while the BIOS drove the fan (the wizard should be aborted).
    #[test]
    fn emergency_release_during_calibration_on_cool_hw_stores_nothing() {
        let mut e = engine_with(confirmed(with_profile(Preset::Balanced)), Box::new(CoolHw(SimHw::new(1.0))));
        e.tick();
        e.handle(Command::Calibrate(1));
        for _ in 0..20 {
            e.tick();
        }
        e.handle(Command::EmergencyRelease);
        for _ in 0..5000 {
            e.tick();
            if e.calib.is_none() {
                break;
            }
        }
        assert!(!fans(&e)[1].controlled, "fan released");
        assert!(
            calibrated(&e, "mb.pwm1").is_none(),
            "calibration measured under BIOS control was stored: {:?}",
            calibrated(&e, "mb.pwm1")
        );
    }

    #[test]
    fn unsafe_config_values_are_clamped_every_tick() {
        let mut cfg = with_profile(Preset::Balanced);
        cfg.general.cpu_critical = 105.0;
        let fc = cfg.profile_mut().fan_mut("mb.pwm1", FanRole::System);
        fc.curve.response_s = 1e30;
        fc.ai.limit = 105.0;
        let mut c2 = cfg.clone();
        c2.clamp_safety();
        assert_eq!(c2.general.cpu_critical, 95.0);
        let fc2 = c2.profile().fan("mb.pwm1", FanRole::System);
        assert!(fc2.curve.response_s <= 60.0 && fc2.ai.limit <= 95.0);
        let mut e = engine(cfg);
        e.tick();
        assert!(fans(&e)[1].controlled);
    }

    // ---- fix wave (safety and QA reviews) ----

    /// Knobs and recorders of [`FakeHw`].
    #[derive(Default)]
    struct Knobs {
        /// Pretend to be real hardware (crash-protection gate and snapshot file active).
        real: bool,
        no_failsafe: bool,
        release_fails: bool,
        set_fan_fails: bool,
        /// `set_fan_curve` fails with this code `curve_err_times` times.
        curve_err: Option<ErrorCode>,
        curve_err_times: u32,
        bad_input: bool,
        /// Pin the simulated CPU heatsink at 30 °C (long runs stay idle-cool).
        cool: bool,
        drop: Vec<&'static str>,
        /// `poll` and `diagnostics_dump` panic (engine panic containment).
        panic: bool,
        /// `reapply` calls.
        reapplies: u32,
        /// Returned by every `take_events` (detail gets a running number).
        repeat_event: Option<(ErrorCode, &'static str)>,
        events_sent: u32,
        set_fans: u32,
        releases: u32,
        curves: Vec<(usize, HwCurve)>,
        held: [bool; 3],
    }

    /// SimHw with failure injection; `crash_snapshot` is `Some` while a fan is held, like RealHw.
    struct FakeHw {
        inner: SimHw,
        info: HwInfo,
        k: Arc<Mutex<Knobs>>,
    }

    impl HwBackend for FakeHw {
        fn info(&self) -> &HwInfo {
            &self.info
        }
        fn sensors(&self) -> &[crate::model::SensorDesc] {
            self.inner.sensors()
        }
        fn fans(&self) -> &[crate::model::FanChannelDesc] {
            self.inner.fans()
        }
        fn poll(&mut self) -> Vec<Option<f32>> {
            let panic = self.k.lock().unwrap().panic;
            assert!(!panic, "injected poll panic");
            if self.k.lock().unwrap().cool {
                self.inner.set_heatsink(30.0);
            }
            let mut v = self.inner.poll();
            let k = self.k.lock().unwrap();
            for (i, s) in self.inner.sensors().iter().enumerate() {
                if k.drop.contains(&s.id.as_str()) {
                    v[i] = None;
                }
            }
            v
        }
        fn set_fan(&mut self, fan: usize, duty: f32) -> anyhow::Result<()> {
            let mut k = self.k.lock().unwrap();
            k.set_fans += 1;
            if k.set_fan_fails {
                return Err(crate::error::err(ErrorCode::PawnIoCall, "test"));
            }
            k.held[fan] = true;
            self.inner.set_fan(fan, duty)
        }
        fn set_fan_curve(&mut self, fan: usize, curve: &HwCurve) -> anyhow::Result<()> {
            let mut k = self.k.lock().unwrap();
            if let Some(code) = k.curve_err.filter(|_| k.curve_err_times > 0) {
                k.curve_err_times -= 1;
                return Err(crate::error::err(code, "test"));
            }
            k.curves.push((fan, curve.clone()));
            k.held[fan] = true;
            self.inner.set_fan_curve(fan, curve)
        }
        fn chip_temp_input(&self, id: &str) -> Option<u8> {
            self.inner.chip_temp_input(id)
        }
        fn chip_input_valid(&self, _fan: usize, _input: u8) -> bool {
            !self.k.lock().unwrap().bad_input
        }
        fn has_hw_failsafe(&self, _fan: usize) -> bool {
            !self.k.lock().unwrap().no_failsafe
        }
        fn release_fan(&mut self, fan: usize) -> anyhow::Result<()> {
            let mut k = self.k.lock().unwrap();
            k.releases += 1;
            if k.release_fails {
                return Err(crate::error::err(ErrorCode::RestoreFailed, "test"));
            }
            k.held[fan] = false;
            self.inner.release_fan(fan)
        }
        fn reapply(&mut self) -> anyhow::Result<usize> {
            self.k.lock().unwrap().reapplies += 1;
            Ok(0)
        }
        fn diagnostics_dump(&mut self) -> String {
            let panic = self.k.lock().unwrap().panic;
            assert!(!panic, "injected diagnostics panic");
            String::new()
        }
        fn release_all(&mut self) -> Vec<usize> {
            let mut k = self.k.lock().unwrap();
            k.releases += 1;
            if k.release_fails {
                return (0..3).filter(|&i| k.held[i]).collect();
            }
            k.held = [false; 3];
            self.inner.release_all()
        }
        fn crash_snapshot(&self) -> Option<crate::hw::CrashSnapshot> {
            let k = self.k.lock().unwrap();
            k.held.iter().any(|h| *h).then(|| crate::hw::CrashSnapshot::now(Vec::new()))
        }
        fn take_events(&mut self) -> Vec<crate::error::AppError> {
            let mut k = self.k.lock().unwrap();
            let Some((code, subject)) = k.repeat_event else { return Vec::new() };
            k.events_sent += 1;
            vec![crate::error::AppError::new(code, format!("{subject}: attempt {}", k.events_sent))]
        }
    }

    fn fake_engine(cfg: Config, knobs: Knobs) -> (Engine, Arc<Mutex<Knobs>>) {
        fake_engine_on(cfg, knobs, SimHw::new(1.0))
    }

    /// FakeHw on a SimHw whose descriptions the test shaped (pump role, no tach, …); the headers are
    /// identified as that SimHw describes them.
    fn fake_engine_on(cfg: Config, knobs: Knobs, inner: SimHw) -> (Engine, Arc<Mutex<Knobs>>) {
        fake_engine_impl(cfg, knobs, inner, true)
    }

    /// FakeHw on the default SimHw with the roles exactly as `cfg` has them (none confirmed).
    fn fake_engine_unidentified(cfg: Config, knobs: Knobs) -> (Engine, Arc<Mutex<Knobs>>) {
        fake_engine_impl(cfg, knobs, SimHw::new(1.0), false)
    }

    fn fake_engine_impl(cfg: Config, knobs: Knobs, inner: SimHw, identify: bool) -> (Engine, Arc<Mutex<Knobs>>) {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let mut info = inner.info().clone();
        info.simulated = !knobs.real;
        let k = Arc::new(Mutex::new(knobs));
        let hw: Box<dyn HwBackend> = Box::new(FakeHw { inner, info, k: k.clone() });
        let mut e = if identify { engine_with(cfg, hw) } else { engine_unidentified(cfg, hw) };
        let n = N.fetch_add(1, Ordering::SeqCst);
        e.snapshot_path = test_dir().join(format!("snapshot-{n}.json"));
        let _ = std::fs::remove_file(&e.snapshot_path);
        (e, k)
    }

    fn real() -> Knobs {
        Knobs { real: true, ..Default::default() }
    }

    fn alerts_with(e: &Engine, code: ErrorCode) -> Vec<Alert> {
        e.shared.state.read().unwrap().alerts.iter().filter(|a| a.code == Some(code)).cloned().collect()
    }

    fn ago(s: f32) -> Instant {
        Instant::now() - Duration::from_secs_f32(s)
    }

    /// A failed release keeps the fan controlled and the crash snapshot on disk.
    #[test]
    fn shutdown_keeps_snapshot_when_release_fails() {
        let (mut e, k) = fake_engine(with_profile(Preset::FullSpeed), Knobs { release_fails: true, ..real() });
        e.tick();
        assert!(fans(&e).iter().all(|f| f.controlled));
        assert!(e.snapshot_path.exists(), "snapshot written while controlled");
        e.shutdown();
        assert!(e.hw.crash_snapshot().is_some(), "hardware still holds the fans");
        assert!(e.fans.iter().all(|f| f.controlled), "not marked released");
        assert!(e.snapshot_path.exists(), "snapshot kept for the watchdog / next start");
        assert!(k.lock().unwrap().releases > 1, "release retried during shutdown");
        // Once the release works, the snapshot goes.
        k.lock().unwrap().release_fails = false;
        e.shutdown();
        assert!(e.fans.iter().all(|f| !f.controlled));
        assert!(!e.snapshot_path.exists());
    }

    /// A failed EmergencyRelease is retried every tick and alerted.
    #[test]
    fn failed_emergency_release_is_retried() {
        let (mut e, k) = fake_engine(with_profile(Preset::FullSpeed), Knobs { release_fails: true, ..real() });
        e.tick();
        e.handle(Command::EmergencyRelease);
        let n = k.lock().unwrap().releases;
        for _ in 0..3 {
            e.tick();
        }
        assert!(k.lock().unwrap().releases >= n + 3 * 3, "every held fan retried every tick");
        assert!(fans(&e).iter().all(|f| f.controlled), "still held, shown as such");
        assert!(!alerts_with(&e, ErrorCode::RestoreFailed).is_empty());
        assert!(e.snapshot_path.exists());
        k.lock().unwrap().release_fails = false;
        e.tick();
        assert!(fans(&e).iter().all(|f| !f.controlled));
        assert!(!e.snapshot_path.exists(), "snapshot removed once nothing is held");
    }

    /// AI fans go to the BIOS 5 s after their control temperature vanished.
    #[test]
    fn ai_fans_released_when_control_temperature_is_lost() {
        let (mut e, k) = fake_engine(with_profile(Preset::AiAuto), Knobs::default());
        for _ in 0..20 {
            e.tick();
        }
        assert!(fans(&e).iter().all(|f| f.controlled));
        k.lock().unwrap().drop = vec!["cpu.temp", "cpu.ccd1"];
        e.tick();
        assert!(fans(&e).iter().all(|f| f.controlled), "held during the grace period");
        e.ai_stale_since = Some(ago(5.5));
        e.tick();
        let f = fans(&e);
        assert!(f.iter().all(|f| !f.controlled && f.failsafe), "{f:?}");
        assert!(!alerts_with(&e, ErrorCode::SensorLost).is_empty());
    }

    /// audit FR-CTL-003: when the CPU temperature stops reporting, the critical latch is blind, so
    /// headers on a software duty that does not depend on it (fixed duties here) go to the BIOS too.
    #[test]
    fn software_duties_go_to_the_bios_when_the_cpu_temperature_is_lost() {
        let (mut e, k) = fake_engine(with_profile(Preset::FullSpeed), Knobs::default());
        for _ in 0..3 {
            e.tick();
        }
        assert!(fans(&e).iter().all(|f| f.controlled), "{:?}", fans(&e));
        k.lock().unwrap().drop = vec!["cpu.temp"];
        e.tick();
        assert!(fans(&e).iter().all(|f| f.controlled), "held during the grace period");
        e.cpu_missing_since = Some(ago(6.0));
        e.tick();
        let f = fans(&e);
        assert!(f.iter().all(|f| !f.controlled && f.failsafe), "{f:?}");
        assert!(!alerts_with(&e, ErrorCode::SensorLost).is_empty());
        // The sensor reports again: control resumes.
        k.lock().unwrap().drop.clear();
        e.tick();
        assert!(fans(&e).iter().all(|f| f.controlled), "{:?}", fans(&e));
    }

    /// The critical latch never releases on stale or unknown CPU values.
    #[test]
    fn critical_latch_holds_while_cpu_is_unknown() {
        let mut cfg = with_profile(Preset::Silent);
        cfg.general.cpu_critical = 10.0;
        let (mut e, k) = fake_engine(cfg, Knobs::default());
        e.tick();
        assert!(e.crit_latched);
        k.lock().unwrap().drop = vec!["cpu.temp"];
        e.shared.edit_config(|c| c.general.cpu_critical = 90.0);
        for _ in 0..3 {
            e.tick();
        }
        assert!(e.cpu_hist.is_empty(), "stale CPU samples dropped: {:?}", e.cpu_hist);
        e.cpu_missing_since = Some(ago(6.0));
        e.crit_ok_since = Some(ago(20.0));
        e.tick();
        assert!(e.crit_latched, "unknown CPU temperature is not 'cool'");
        // Back with a cool value: the normal 10 s release applies.
        k.lock().unwrap().drop.clear();
        e.tick();
        e.crit_ok_since = Some(ago(11.0));
        e.tick();
        assert!(!e.crit_latched);
    }

    /// A controlled fan that loses its hardware failsafe is released when the
    /// watchdog is not running (tests never spawn it).
    #[test]
    fn fan_losing_its_failsafe_is_released_without_crash_protection() {
        assert!(!crate::system::watchdog_active());
        let mut cfg = with_profile(Preset::Balanced);
        for (id, role) in [("mb.pwm0", FanRole::Cpu), ("mb.pwm1", FanRole::System), ("mb.pwm2", FanRole::System)] {
            let fc = cfg.profile_mut().fan_mut(id, role);
            fc.mode = FanMode::Fixed;
            fc.fixed_duty = 40.0;
        }
        let (mut e, k) = fake_engine(cfg, real());
        e.tick();
        assert!(fans(&e).iter().all(|f| f.controlled), "taken with a hardware failsafe");
        k.lock().unwrap().no_failsafe = true;
        e.tick();
        let f = fans(&e);
        assert!(f.iter().all(|f| !f.controlled && f.mode == "BIOS (no crash protection)"), "{f:?}");
        let n = k.lock().unwrap().set_fans;
        for _ in 0..5 {
            e.tick();
        }
        assert_eq!(k.lock().unwrap().set_fans, n, "no manual duties without crash protection");
    }

    /// A calibration requested while the fans are released is refused.
    #[test]
    fn calibration_refused_while_fans_are_released() {
        let mut e = engine(with_profile(Preset::Balanced));
        e.tick();
        e.handle(Command::EmergencyRelease);
        e.handle(Command::Calibrate(0));
        for _ in 0..50 {
            e.tick();
            if e.calib.is_none() {
                break;
            }
        }
        assert!(e.calib.is_none());
        assert!(e.shared.config_snapshot().calibrations.is_empty());
        assert!(e.shared.state.read().unwrap().errors.iter().any(|ev| ev.code == ErrorCode::CalibrationFailed));
    }

    /// Calibration (a sweep down to 0 %) only runs on headers confirmed as fans,
    /// never on a pump or an unconfirmed header, never while a cooling alarm is up, and stops when
    /// an alarm or a pump / unknown role appears mid-run.
    #[test]
    fn calibration_only_on_confirmed_fans_and_never_during_alarms() {
        let refused_with = |e: &mut Engine, fan: usize| -> String {
            e.handle(Command::Calibrate(fan));
            e.tick();
            assert!(e.calib.as_ref().is_none_or(|c| c.done()), "refused");
            e.shared.state.read().unwrap().calibration.clone().and_then(|v| v.error).unwrap_or_default()
        };
        // Unconfirmed header (the board's guess only), also shown by the UI before asking.
        let mut e = engine_unidentified(with_profile(Preset::Balanced), Box::new(SimHw::new(1.0)));
        e.tick();
        assert_eq!(fans(&e)[1].calibration_refused(), Some(UNCONFIRMED_CALIBRATION));
        assert_eq!(refused_with(&mut e, 1), UNCONFIRMED_CALIBRATION);
        // Confirmed pump / unknown device.
        let mut cfg = confirmed(with_profile(Preset::Balanced));
        cfg.set_fan_role("mb.pwm1", Some(HeaderRole::Pump), None);
        cfg.set_fan_role("mb.pwm2", Some(HeaderRole::Unknown), None);
        let mut e = engine(cfg);
        e.tick();
        assert_eq!(fans(&e)[1].calibration_refused(), Some(PUMP_CALIBRATION));
        assert_eq!(refused_with(&mut e, 1), PUMP_CALIBRATION);
        assert_eq!(refused_with(&mut e, 2), UNCONFIRMED_CALIBRATION);
        assert!(fans(&e)[1].duty.is_some_and(|d| d >= crate::config::PUMP_MIN_DUTY), "{:?}", fans(&e)[1]);
        // A cooling alarm refuses the start.
        let mut e = engine(confirmed(with_profile(Preset::Balanced)));
        e.tick();
        assert_eq!(fans(&e)[0].calibration_refused(), None);
        e.efficacy_alarm = true;
        e.handle(Command::Calibrate(0));
        assert!(e.calib.as_ref().is_some_and(|c| c.error.as_deref() == Some(ALARM_CALIBRATION)));
        // An alarm mid-run stops the sweep (E309 is armed by a tach-less CPU-chain header). E309 is
        // inform-only in v0.2: the header goes back to its mode at once, nothing is forced.
        let mut cfg = confirmed(with_profile(Preset::Balanced));
        cfg.set_fan_role("mb.pwm2", Some(HeaderRole::Unknown), None);
        let mut e = engine_with(cfg, Box::new(SimHw::new(1.0).without_tach(2)));
        e.tick();
        e.handle(Command::Calibrate(0));
        for _ in 0..40 {
            e.tick();
        }
        assert!(e.calib.as_ref().is_some_and(|c| !c.done()), "running");
        e.efficacy_alarm = true;
        e.efficacy.alarm = true;
        e.tick();
        e.tick();
        assert!(e.calib.is_none(), "stopped");
        let f = &fans(&e)[0];
        assert!(!f.failsafe && f.mode == "Curve" && f.controlled, "back to the Balanced curve: {f:?}");
        assert!(e.shared.config_snapshot().calibrations.is_empty());
        // Re-tagged as the pump mid-run: stopped before the sweep goes below the pump floor.
        let mut e = engine(confirmed(with_profile(Preset::Balanced)));
        e.tick();
        e.handle(Command::Calibrate(1));
        for _ in 0..5 {
            e.tick();
        }
        e.shared.edit_config(|c| c.set_fan_role("mb.pwm1", Some(HeaderRole::Pump), None));
        e.tick();
        assert!(e.calib.as_ref().is_none_or(|c| c.done()), "stopped");
        e.tick();
        assert!(fans(&e)[1].duty.is_some_and(|d| d >= crate::config::PUMP_MIN_DUTY), "{:?}", fans(&e)[1]);
        assert!(e.shared.config_snapshot().calibrations.is_empty());
    }

    /// EmergencyRelease stops a running calibration at once.
    #[test]
    fn emergency_release_during_calibration_stores_no_calibration() {
        let mut e = engine(confirmed(with_profile(Preset::Balanced)));
        e.tick();
        e.handle(Command::Calibrate(1));
        for _ in 0..10 {
            e.tick();
        }
        assert!(e.calib.as_ref().is_some_and(|c| !c.done()));
        e.handle(Command::EmergencyRelease);
        e.tick();
        assert!(e.calib.is_none(), "wizard stopped");
        let view = e.shared.state.read().unwrap().calibration.clone().unwrap();
        assert!(view.error.as_deref().is_some_and(|m| m.contains("handed back")), "{view:?}");
        for _ in 0..30 {
            e.tick();
        }
        assert!(e.shared.config_snapshot().calibrations.is_empty());
    }

    /// The wizard stops when its fan cannot be driven (write errors).
    #[test]
    fn calibration_stops_when_the_fan_is_not_driven() {
        let (mut e, k) = fake_engine(confirmed(with_profile(Preset::Balanced)), Knobs::default());
        e.tick();
        k.lock().unwrap().set_fan_fails = true;
        e.handle(Command::Calibrate(1));
        e.tick();
        e.tick();
        assert!(e.calib.is_none(), "stopped instead of measuring the BIOS curve");
        assert!(e.shared.config_snapshot().calibrations.is_empty());
        assert!(e.shared.state.read().unwrap().errors.iter().any(|ev| ev.code == ErrorCode::CalibrationFailed));
    }

    /// A busy bus does not latch the software fallback.
    #[test]
    fn busy_hardware_curve_is_retried_next_tick() {
        let knobs = Knobs { curve_err: Some(ErrorCode::BusBusy), curve_err_times: 1, ..Default::default() };
        let (mut e, k) = fake_engine(with_profile(Preset::Balanced), knobs);
        e.tick();
        assert!(!fans(&e)[0].hw_curve);
        e.tick();
        assert!(fans(&e)[0].hw_curve, "programmed on the next tick");
        assert_eq!(k.lock().unwrap().curves.iter().filter(|c| c.0 == 0).count(), 1);
        assert!(!e.shared.state.read().unwrap().errors.iter().any(|ev| ev.code == ErrorCode::HwCurveFallback));
    }

    /// A read-back mismatch runs the curve in software and retries the
    /// hardware curve after a back-off instead of never.
    #[test]
    fn mismatched_hardware_curve_is_retried_after_backoff() {
        let knobs = Knobs { curve_err: Some(ErrorCode::RegisterMismatch), curve_err_times: 1, ..Default::default() };
        let (mut e, _k) = fake_engine(with_profile(Preset::Balanced), knobs);
        for _ in 0..3 {
            e.tick();
        }
        let f = &fans(&e)[0];
        assert!(!f.hw_curve && f.controlled && f.duty.is_some(), "software meanwhile: {f:?}");
        let retry = e.fans[0].hw_curve_failed.as_ref().and_then(|(_, r)| *r).expect("retry scheduled");
        assert!(retry > Instant::now() + Duration::from_secs(30));
        e.fans[0].hw_curve_failed.as_mut().unwrap().1 = Some(ago(0.01));
        e.tick();
        assert!(fans(&e)[0].hw_curve, "hardware curve again after the back-off");
    }

    /// A chip channel follows one input, so a curve on a
    /// board sensor would have no CPU failsafe in hardware. Such curves run in software (flat chip
    /// curve with the CPU failsafe) for every role; CPU-sourced curves of any role run in the chip.
    #[test]
    fn board_sensor_curves_run_in_software_with_the_cpu_failsafe() {
        let mut cfg = with_profile(Preset::Balanced);
        cfg.profile_mut().fan_mut("mb.pwm0", FanRole::Cpu).curve.source.sensors = vec!["mb.t0".into()];
        cfg.profile_mut().fan_mut("mb.pwm1", FanRole::System).curve.source.sensors = vec!["mb.t0".into()];
        cfg.profile_mut().fan_mut("mb.pwm2", FanRole::System).curve.source.sensors = vec![ids::CPU_TEMP.into()];
        let (mut e, k) = fake_engine(cfg, Knobs::default());
        for _ in 0..3 {
            e.tick();
        }
        let f = fans(&e);
        for x in &f[..2] {
            assert!(!x.hw_curve && x.controlled && x.duty.is_some() && x.hw_failsafe, "{x:?}");
        }
        assert!(f[2].hw_curve, "a case fan on the CPU input runs in the chip: {:?}", f[2]);
        let curves = k.lock().unwrap().curves.clone();
        assert!(curves.iter().all(|c| c.0 == 2 && c.1.temp_input == 2 && c.1.full_speed_at <= 90.0), "{curves:?}");
    }

    /// The curve's hysteresis reaches the chip as ΔT (0..=3 °C).
    #[test]
    fn hardware_curve_carries_the_hysteresis() {
        let mut cfg = with_profile(Preset::Balanced);
        cfg.profile_mut().fan_mut("mb.pwm0", FanRole::Cpu).curve.hysteresis = 1.4;
        let (mut e, k) = fake_engine(cfg, Knobs::default());
        e.tick();
        assert_eq!(k.lock().unwrap().curves[0].1.delta_t, Some(1));
        e.shared.edit_config(|c| c.profile_mut().fan_mut("mb.pwm0", FanRole::Cpu).curve.hysteresis = 8.0);
        e.tick();
        assert_eq!(k.lock().unwrap().curves.last().unwrap().1.delta_t, Some(3), "clamped to the register range");
        assert_eq!(chip_delta_t(f32::NAN), 2);
    }

    /// No hardware curve on an invalid chip input; a hardware curve whose source
    /// stays lost goes to the BIOS with a SensorLost alert.
    #[test]
    fn hardware_curve_needs_a_valid_input() {
        let mut cfg = with_profile(Preset::Balanced);
        // Hardware curves only run on the CPU input.
        cfg.profile_mut().fan_mut("mb.pwm1", FanRole::System).curve.source.sensors = vec![ids::CPU_TEMP.into()];
        let (mut e, k) = fake_engine(cfg, Knobs { bad_input: true, ..Default::default() });
        e.tick();
        e.tick();
        let f = fans(&e);
        assert!(!f[0].hw_curve && f[0].controlled && f[0].duty.is_some(), "software curve: {:?}", f[0]);
        assert!(!f[1].hw_curve, "{:?}", f[1]);
        k.lock().unwrap().bad_input = false;
        e.tick();
        assert!(fans(&e)[1].hw_curve);
        // The source sensor disappears: held for the grace period, then the BIOS.
        k.lock().unwrap().drop = vec![ids::CPU_TEMP, "cpu.ccd1"];
        e.tick();
        assert!(fans(&e)[1].controlled, "grace period");
        e.fans[1].missing_since = Some(ago(6.0));
        e.tick();
        let f = &fans(&e)[1];
        assert!(!f.controlled && f.failsafe, "{f:?}");
        assert!(!alerts_with(&e, ErrorCode::SensorLost).is_empty());
    }

    /// The same event every tick is one alert with a count.
    #[test]
    fn repeated_events_are_one_alert_with_a_count() {
        let knobs = Knobs { repeat_event: Some((ErrorCode::RegisterMismatch, "IT8689E PWM2")), ..Default::default() };
        let (mut e, _k) = fake_engine(with_profile(Preset::Balanced), knobs);
        for _ in 0..50 {
            e.tick();
        }
        let a = alerts_with(&e, ErrorCode::RegisterMismatch);
        assert_eq!(a.len(), 1, "{a:?}");
        assert!(a[0].detail.ends_with("(×50)"), "{}", a[0].detail);
    }

    /// Profile switches appear in the Diagnostics event ring.
    #[test]
    fn profile_switch_is_an_event() {
        let mut e = engine(with_profile(Preset::Balanced));
        e.tick();
        let silent = e.shared.config_snapshot().profiles.iter().position(|p| p.preset == Preset::Silent).unwrap();
        e.shared.edit_config(|c| c.active_profile = silent);
        e.tick();
        let st = e.shared.state.read().unwrap();
        assert!(st.errors.iter().any(|ev| ev.code == ErrorCode::ProfileSwitched && ev.detail.contains("Silent")));
    }

    /// After a failed start-up recovery no fan is taken and the snapshot that
    /// still holds the real BIOS state is neither rewritten nor deleted.
    #[test]
    fn blocked_takeover_takes_no_fan_and_keeps_the_snapshot() {
        let (mut e, k) = fake_engine(with_profile(Preset::FullSpeed), real());
        let why = "crash recovery failed twice".to_string();
        e.takeover_blocked = Some(why.clone());
        e.startup_issues = vec![crate::error::AppError::new(ErrorCode::RestoreFailed, why.clone())];
        std::fs::create_dir_all(e.snapshot_path.parent().unwrap()).unwrap();
        std::fs::write(&e.snapshot_path, "bios state").unwrap();
        e.handle(Command::Calibrate(0));
        for _ in 0..3 {
            e.tick();
        }
        assert_eq!(k.lock().unwrap().set_fans, 0);
        assert!(k.lock().unwrap().curves.is_empty());
        let f = fans(&e);
        assert!(f.iter().all(|f| !f.controlled && f.mode == "BIOS (recovery failed)"), "{f:?}");
        assert_eq!(alerts_with(&e, ErrorCode::RestoreFailed).len(), 1, "one alert, not duplicated");
        assert!(e.shared.config_snapshot().calibrations.is_empty());
        e.handle(Command::EmergencyRelease);
        e.tick();
        e.shutdown();
        assert_eq!(std::fs::read_to_string(&e.snapshot_path).unwrap(), "bios state");
    }

    /// FIX-SEC N8/N9: start-up issues go into the event ring once and stay as alerts.
    #[test]
    fn startup_issues_are_reported_once() {
        let mut e = engine(with_profile(Preset::Balanced));
        e.startup_issues = vec![
            crate::error::AppError::new(ErrorCode::StateFolder, "C:\\ProgramData\\FanRGB could not be secured"),
            crate::error::AppError::new(ErrorCode::Hardening, "Redirection Guard"),
        ];
        for _ in 0..5 {
            e.tick();
        }
        let st = e.shared.state.read().unwrap();
        for code in [ErrorCode::StateFolder, ErrorCode::Hardening] {
            assert_eq!(st.errors.iter().filter(|ev| ev.code == code).count(), 1, "{code}");
            assert_eq!(st.alerts.iter().filter(|a| a.code == Some(code)).count(), 1, "{code}");
        }
    }

    /// A release / quit that arrives during a tick is handled before publishing;
    /// with quit set nothing is published (a UI read guard cannot hold the engine up).
    #[test]
    fn release_and_quit_are_handled_before_publishing() {
        let (mut e, k) = fake_engine(with_profile(Preset::FullSpeed), Knobs::default());
        let (tx, rx) = std::sync::mpsc::channel();
        e.rx = Some(rx);
        e.tick();
        assert!(fans(&e).iter().all(|f| f.controlled));
        tx.send(Command::EmergencyRelease).unwrap();
        e.tick();
        assert!(e.emergency);
        assert_eq!(k.lock().unwrap().held, [false; 3], "released within the same tick");
        let published = e.shared.state.read().unwrap().tick;
        e.shared.quit.store(true, Ordering::SeqCst);
        let shared = e.shared.clone();
        let ui_guard = shared.state.read().unwrap();
        e.tick();
        drop(ui_guard);
        assert_eq!(e.shared.state.read().unwrap().tick, published, "nothing published after quit");
    }

    /// The simulated (demo) backend never touches the real crash snapshot.
    #[test]
    fn demo_never_touches_the_crash_snapshot() {
        let (mut e, _k) = fake_engine(with_profile(Preset::FullSpeed), Knobs::default());
        std::fs::create_dir_all(e.snapshot_path.parent().unwrap()).unwrap();
        std::fs::write(&e.snapshot_path, "real snapshot").unwrap();
        e.tick();
        e.handle(Command::EmergencyRelease);
        e.tick();
        e.shutdown();
        assert_eq!(std::fs::read_to_string(&e.snapshot_path).unwrap(), "real snapshot");
    }

    // ---- fix wave 2026-10-19 (first hardware run) ----

    /// The user's config.json stores `mb.pwm0: Curve` under "BIOS (Smart Fan)".
    /// The BIOS preset still drives nothing and writes nothing.
    #[test]
    fn bios_profile_with_a_stored_override_takes_no_fan() {
        let mut cfg = with_profile(Preset::Bios);
        let fc = cfg.profile_mut().fan_mut("mb.pwm0", FanRole::Cpu);
        fc.mode = FanMode::Curve;
        cfg.profile_mut().fan_mut("mb.pwm1", FanRole::System).mode = FanMode::Ai;
        let (mut e, k) = fake_engine(cfg, real());
        for _ in 0..5 {
            e.tick();
        }
        assert!(fans(&e).iter().all(|f| !f.controlled && f.duty.is_none() && f.mode == "BIOS"), "{:?}", fans(&e));
        let k = k.lock().unwrap();
        assert_eq!((k.set_fans, k.curves.len()), (0, 0), "nothing written");
        assert!(!e.snapshot_path.exists(), "no crash snapshot");
        assert!(e.shared.state.read().unwrap().ai.is_empty(), "no AI group");
        assert_eq!(e.shared.config_snapshot().profiles[0].fans["mb.pwm0"].mode, FanMode::Curve, "user data kept");
    }

    /// A header with nothing on it (not documented, no tach) stays with
    /// the BIOS unless the user configured it; once it spins it is a normal fan.
    #[test]
    fn empty_headers_are_not_taken_over() {
        let mut hw = SimHw::new(1.0).without_tach(1).without_tach(2);
        for i in [1, 2] {
            let d = hw.fan_desc_mut(i);
            d.present = false;
            d.documented = Some(false);
        }
        let mut cfg = with_profile(Preset::FullSpeed);
        // Explicitly configured: taken.
        cfg.profile_mut().fan_mut("mb.pwm2", FanRole::System).fixed_duty = 100.0;
        let (mut e, k) = fake_engine(cfg, real());
        let info = e.hw.info().clone();
        e.hw = Box::new(FakeHw { inner: hw, info, k: k.clone() });
        // The test engine identified the sim's (present) headers; these two are empty now. (Headers
        // that are not present don't need a role for the modes to run.)
        e.shared.edit_config(|c| {
            c.fan_roles.remove("mb.pwm1");
            c.fan_roles.remove("mb.pwm2");
        });
        for _ in 0..3 {
            e.tick();
        }
        let f = fans(&e);
        assert!(f[0].controlled, "present header: {:?}", f[0]);
        assert!(!f[1].controlled && !f[1].failsafe && f[1].mode == "BIOS (nothing connected)", "{:?}", f[1]);
        assert!(f[2].controlled, "configured empty header is driven: {:?}", f[2]);
        assert!(!k.lock().unwrap().held[1], "nothing written to the empty header");
        // AI preset: an unconfigured empty header is not put in the AI group either.
        let mut hw = SimHw::new(1.0).without_tach(1);
        let d = hw.fan_desc_mut(1);
        d.present = false;
        d.documented = Some(false);
        let mut e = engine_with(with_profile(Preset::AiAuto), Box::new(hw));
        for _ in 0..5 {
            e.tick();
        }
        let f = fans(&e);
        assert!(!f[1].controlled && f[1].mode == "BIOS (nothing connected)", "{:?}", f[1]);
        assert!(f[0].controlled && f[2].controlled);
        let ai = &e.shared.state.read().unwrap().ai;
        let groups: Vec<&Vec<String>> = ai.iter().map(|z| &z.fans).collect();
        assert!(groups.iter().all(|g| !g.contains(&f[1].name)), "{groups:?}");
    }

    /// A pump header runs its own curve in the chip in every curve preset (Auto included), never
    /// below the pump floor.
    #[test]
    fn pump_header_runs_its_own_curve_above_the_floor() {
        for preset in [Preset::Silent, Preset::Balanced, Preset::Performance, Preset::AiAuto] {
            let mut hw = SimHw::new(1.0);
            hw.fan_desc_mut(2).role = FanRole::Pump;
            let mut e = engine_with(with_profile(preset), Box::new(hw));
            for _ in 0..30 {
                e.tick();
            }
            let f = &fans(&e)[2];
            let floor = crate::config::PUMP_MIN_DUTY;
            assert!(f.controlled && f.duty.is_some_and(|d| d >= floor), "{preset:?}: {f:?}");
            assert!(f.mode.starts_with("Curve") && f.hw_curve, "{preset:?}: {f:?}");
            assert_eq!(f.floor, Some(floor), "{preset:?}");
        }
    }

    /// CPU_FAN without a tach is "running, no speed signal": no 0 rpm, no
    /// calibration, still driven, and the AI keeps measuring the other fans.
    #[test]
    fn no_tach_header_is_running_without_speed_signal() {
        let mut e = engine_with(with_profile(Preset::Balanced), Box::new(SimHw::new(1.0).without_tach(0)));
        for _ in 0..3 {
            e.tick();
        }
        let f = fans(&e);
        assert!(!f[0].speed_signal && f[0].no_speed_signal() && f[0].rpm.is_none(), "{:?}", f[0]);
        assert!(f[0].controlled, "still driven");
        assert!(f[1].speed_signal && f[1].rpm.is_some());
        let raw = e.shared.state.read().unwrap().value("mb.fan0");
        assert_eq!(raw, Some(0.0), "the sensor keeps the raw reading");
        // Calibration is refused with the reason.
        e.handle(Command::Calibrate(0));
        e.tick();
        e.tick();
        assert!(e.calib.is_none());
        let errs = e.shared.state.read().unwrap().errors.clone();
        let refused = |x: &ErrorEvent| x.code == ErrorCode::CalibrationFailed && x.detail.contains("no speed signal");
        assert!(errs.iter().any(refused), "{errs:?}");
        // AI: the group's airflow is measured from the fans that have a signal.
        assert_eq!(measured_airflow(&[(Some(0.0), None, false), (Some(900.0), Some(1800.0), true)]), Some(0.5));
        assert_eq!(measured_airflow(&[(Some(0.0), Some(1800.0), false)]), None);
        assert_eq!(measured_airflow(&[(None, Some(1800.0), true), (Some(1800.0), Some(1800.0), true)]), Some(1.0));
    }

    /// audit FR-CTL-001: close to the critical temperature a header never gets less than the hard
    /// floor, whatever the profile says (here: the CPU fan fixed at 0 % with fan stop allowed).
    #[test]
    fn hard_floor_near_critical_binds_any_profile() {
        let mut cfg = with_profile(Preset::Balanced);
        cfg.general.cpu_critical = 95.0;
        let mut fc = cfg.profile().fan("mb.pwm0", FanRole::Cpu);
        fc.mode = FanMode::Fixed;
        fc.fixed_duty = 0.0;
        fc.min_duty = 0.0;
        fc.allow_stop = true;
        cfg.profile_mut().fans.insert("mb.pwm0".into(), fc);
        let mut sim = SimHw::new(1.0);
        sim.pin_cpu_power(20.0);
        // Tctl ≈ 80.4 + 0.33·20 ≈ 87 °C: 8 °C below the latch, 4 °C into the envelope.
        let heatsink = Arc::new(Mutex::new(80.4));
        let mut e = engine_with(cfg, Box::new(PinnedHw(sim, heatsink.clone())));
        for _ in 0..4 {
            e.tick();
        }
        let floor = limits::envelope_floor(e.cpu_median, 95.0);
        assert!(floor > 20.0 && floor < limits::ENVELOPE_MAX_DUTY, "{floor}");
        let f = &fans(&e)[0];
        assert!(f.controlled && f.duty.is_some_and(|d| d >= floor - 0.5), "{f:?}, floor {floor}");
        assert!(!e.crit_latched);
        // Without the heat the envelope adds nothing: the profile's 0 % meets only the board's CPU
        // header floor (audit FR-PRIV-005), from the first tick on.
        let mut cool = SimHw::new(1.0);
        cool.pin_cpu_power(20.0);
        let cfg = e.shared.config_snapshot();
        let mut e = engine_unidentified(cfg, Box::new(PinnedHw(cool, Arc::new(Mutex::new(40.0)))));
        for _ in 0..4 {
            e.tick();
        }
        assert_eq!(fans(&e)[0].duty, Some(crate::config::CPU_HEADER_MIN_DUTY), "{:?}", fans(&e)[0]);
    }

    /// SimHw with the CPU heatsink pinned (the cooler watch needs a sustained temperature).
    struct PinnedHw(SimHw, Arc<Mutex<f32>>);

    impl HwBackend for PinnedHw {
        fn info(&self) -> &HwInfo {
            self.0.info()
        }
        fn sensors(&self) -> &[crate::model::SensorDesc] {
            self.0.sensors()
        }
        fn fans(&self) -> &[crate::model::FanChannelDesc] {
            self.0.fans()
        }
        fn poll(&mut self) -> Vec<Option<f32>> {
            let t = *self.1.lock().unwrap();
            self.0.set_heatsink(t);
            self.0.poll()
        }
        fn set_fan(&mut self, fan: usize, duty: f32) -> anyhow::Result<()> {
            self.0.set_fan(fan, duty)
        }
        fn has_hw_failsafe(&self, fan: usize) -> bool {
            self.0.has_hw_failsafe(fan)
        }
        fn estimated_duty(&self, fan: usize) -> Option<f32> {
            self.0.estimated_duty(fan)
        }
        fn release_fan(&mut self, fan: usize) -> anyhow::Result<()> {
            self.0.release_fan(fan)
        }
        fn release_all(&mut self) -> Vec<usize> {
            self.0.release_all()
        }
        fn crash_snapshot(&self) -> Option<crate::hw::CrashSnapshot> {
            None
        }
    }

    /// A hot CPU at low package power with a tach-less CPU fan raises
    /// FRG-E305 (once per episode in the event ring), forces that fan to 100 % and clears again.
    #[test]
    fn cooling_alarm_for_a_tachless_cpu_fan() {
        let mut sim = SimHw::new(1.0).without_tach(0);
        sim.pin_cpu_power(20.0);
        sim.fail_cooler();
        // Tctl ≈ 80.4 + 0.33·20 ≈ 87 °C: above 85 at 20 W, below the 90 °C critical latch.
        let heatsink = Arc::new(Mutex::new(80.4));
        let mut e = engine_with(with_profile(Preset::Balanced), Box::new(PinnedHw(sim, heatsink.clone())));
        for _ in 0..3 {
            e.tick();
        }
        assert!(alerts_with(&e, ErrorCode::CoolingIneffective).is_empty(), "needs 30 s");
        assert!(!e.crit_latched);
        e.cooler.idle_hot_since = Some(ago(31.0));
        e.tick();
        let a = alerts_with(&e, ErrorCode::CoolingIneffective);
        assert_eq!(a.len(), 1, "{a:?}");
        assert_eq!(a[0].level, AlertLevel::Critical);
        // The alarm raised at the end of a tick drives the fans from the next one (≤ 1 poll later).
        e.tick();
        let f = &fans(&e)[0];
        assert!(f.failsafe && f.duty == Some(100.0) && f.mode.contains("cooling"), "{f:?}");
        assert!(!fans(&e)[1].failsafe, "fans with a tach are not touched");
        for _ in 0..20 {
            e.tick();
        }
        let n =
            e.shared.state.read().unwrap().errors.iter().filter(|x| x.code == ErrorCode::CoolingIneffective).count();
        assert_eq!(n, 1, "one event per episode");
        // Cooler back: clears after 60 s below threshold − 5 °C.
        *heatsink.lock().unwrap() = 40.0;
        e.tick();
        e.tick();
        e.cooler.cool_since = Some(ago(61.0));
        e.tick();
        assert!(alerts_with(&e, ErrorCode::CoolingIneffective).is_empty());
        e.tick();
        assert!(!fans(&e)[0].failsafe, "{:?}", fans(&e)[0]);
        // A fan with a tach is not watched at all.
        let mut sim = SimHw::new(1.0);
        sim.pin_cpu_power(20.0);
        let mut e = engine_with(with_profile(Preset::Balanced), Box::new(PinnedHw(sim, Arc::new(Mutex::new(80.4)))));
        e.tick();
        e.cooler.idle_hot_since = Some(ago(31.0));
        e.tick();
        assert!(alerts_with(&e, ErrorCode::CoolingIneffective).is_empty());
    }

    /// After an engine stall long enough for the hang watchdog, the settings are
    /// re-applied before the poll verifies them (no false FRG-E206); after a resume from sleep only
    /// the delayed re-apply runs (the BIOS reprograms the chip first).
    #[test]
    fn stall_reapplies_before_polling() {
        let (mut e, k) = fake_engine(with_profile(Preset::Balanced), real());
        e.tick();
        e.tick();
        e.last_tick = ago(31.0);
        e.tick();
        assert_eq!(k.lock().unwrap().reapplies, 1, "at once, before the poll");
        let (mut e, k) = fake_engine(with_profile(Preset::Balanced), real());
        e.tick();
        e.tick();
        e.last_tick = ago(31.0);
        e.handle(Command::Resumed);
        e.tick();
        assert_eq!(k.lock().unwrap().reapplies, 0, "resume: delayed");
        assert!(e.reapply_at.is_some());
    }

    /// FRG-E305 (raised by a tach-less pump) forces the whole CPU chain, also
    /// the radiator / CPU fans that have a tach, as its alert says; case fans stay on their curve.
    #[test]
    fn cooling_alarm_forces_the_whole_cpu_chain() {
        let mut sim = SimHw::new(1.0).without_tach(2);
        sim.fan_desc_mut(2).role = FanRole::Pump;
        sim.pin_cpu_power(20.0);
        sim.fail_cooler();
        let mut e = engine_with(with_profile(Preset::Balanced), Box::new(PinnedHw(sim, Arc::new(Mutex::new(80.4)))));
        for _ in 0..3 {
            e.tick();
        }
        e.cooler.idle_hot_since = Some(ago(31.0));
        e.tick();
        assert_eq!(alerts_with(&e, ErrorCode::CoolingIneffective).len(), 1);
        e.tick();
        let f = fans(&e);
        assert!(f[0].speed_signal && f[0].failsafe && f[0].duty == Some(100.0), "CPU fans with a tach: {:?}", f[0]);
        assert!(f[2].failsafe && f[2].duty == Some(100.0), "pump: {:?}", f[2]);
        assert!(!f[1].failsafe, "case fans: {:?}", f[1]);
    }

    /// The FRG-E309 baseline key does not change when an unassigned header's
    /// derived role flips with its speed signal (fan-stop / slow spin-up), only when roles are assigned.
    #[test]
    fn efficacy_key_ignores_the_per_run_speed_signal() {
        // Unassigned headers (the mode waits for the identification meanwhile; the roles and the
        // baseline key are derived all the same).
        let sim = SimHw::new(1.0).without_tach(1);
        let mut e = engine_unidentified(with_profile(Preset::Balanced), Box::new(sim));
        e.tick();
        assert_eq!(fans(&e)[1].role, FanRole::Unknown);
        let key = e.efficacy.baseline.key.clone();
        e.fans[1].seen_spinning = true;
        e.tick();
        assert_eq!(fans(&e)[1].role, FanRole::System, "the derived role flipped");
        assert_eq!(e.efficacy.baseline.key, key, "same baseline");
        e.shared.edit_config(|c| c.set_fan_role("mb.pwm1", Some(HeaderRole::CaseFans), None));
        e.tick();
        assert_ne!(e.efficacy.baseline.key, key, "a confirmed role is a new cooling setup");
    }

    /// A header below its role floor (after the wizard's 30 % test, or re-tagged
    /// as the pump while running low) is at the floor on the next tick, not after a ramp.
    #[test]
    fn role_floor_applies_at_once() {
        let mut cfg = with_profile(Preset::Balanced);
        cfg.set_fan_role("mb.pwm1", Some(HeaderRole::Pump), None);
        cfg.set_fan_role("mb.pwm2", Some(HeaderRole::Unknown), None);
        for fan in ["mb.pwm1", "mb.pwm2"] {
            let fc = cfg.profile_mut().fan_mut(fan, FanRole::System);
            fc.mode = FanMode::Fixed;
            fc.fixed_duty = 100.0;
            fc.ramp_up = 1.0;
        }
        let mut e = engine(cfg);
        e.tick();
        for i in [1, 2] {
            e.fans[i].duty = Some(30.0);
        }
        e.tick();
        let f = fans(&e);
        assert!(f[1].duty.is_some_and(|d| d >= crate::config::PUMP_MIN_DUTY), "{:?}", f[1]);
        assert!(f[2].duty.is_some_and(|d| d >= crate::config::UNKNOWN_MIN_DUTY), "{:?}", f[2]);
        assert!(f[1].duty.unwrap() < 100.0, "above the floor the ramp still applies: {:?}", f[1]);
    }

    /// A tach-less CPU-chain header marked "Unused" (the wizard's "Nothing
    /// changed" for a near-silent pump) is not driven, but stays in the FRG-E305 / FRG-E309 watch
    /// and goes to 100 % with FRG-E305 (E309 only informs in v0.2); never under the BIOS preset.
    #[test]
    fn unused_tachless_cpu_header_stays_watched() {
        let mut sim = SimHw::new(1.0).without_tach(0);
        sim.pin_cpu_power(20.0);
        sim.fail_cooler();
        let mut cfg = with_profile(Preset::Balanced);
        cfg.set_fan_role("mb.pwm0", Some(HeaderRole::Unused), None);
        let heatsink = Arc::new(Mutex::new(80.4));
        let mut e = engine_with(cfg, Box::new(PinnedHw(sim, heatsink.clone())));
        for _ in 0..3 {
            e.tick();
        }
        let f = &fans(&e)[0];
        assert!(!f.controlled && f.mode == "BIOS (unused)", "not driven: {f:?}");
        // FRG-E305 still watches it, and forces it.
        e.cooler.idle_hot_since = Some(ago(31.0));
        e.tick();
        assert_eq!(alerts_with(&e, ErrorCode::CoolingIneffective).len(), 1);
        e.tick();
        let f = &fans(&e)[0];
        assert!(f.controlled && f.failsafe && f.duty == Some(100.0) && f.mode.contains("cooling"), "{f:?}");
        // Cleared: back to the BIOS, not driven.
        *heatsink.lock().unwrap() = 40.0;
        e.tick();
        e.tick();
        e.cooler.cool_since = Some(ago(61.0));
        e.tick();
        e.tick();
        let f = &fans(&e)[0];
        assert!(!f.controlled && !f.failsafe && f.mode == "BIOS (unused)", "{f:?}");
        // FRG-E309 stays armed for it (it is the only tach-less CPU-chain header), inform-only in
        // v0.2: the alert, but the header is not driven.
        e.efficacy.alarm = true;
        e.tick();
        e.tick();
        assert_eq!(alerts_with(&e, ErrorCode::CoolingDegraded).len(), 1);
        let f = &fans(&e)[0];
        assert!(!f.controlled && !f.failsafe && f.mode == "BIOS (unused)", "{f:?}");
        // Under the BIOS preset nothing is forced.
        let mut cfg = with_profile(Preset::Bios);
        cfg.set_fan_role("mb.pwm0", Some(HeaderRole::Unused), None);
        let mut e = engine_with(cfg, Box::new(SimHw::new(1.0).without_tach(0)));
        e.tick();
        e.efficacy.alarm = true;
        for _ in 0..3 {
            e.tick();
        }
        assert!(fans(&e).iter().all(|f| !f.controlled), "{:?}", fans(&e));
    }

    /// The in-app report carries every section, BIOS-controlled fans show
    /// the firmware curve's estimate, the per-channel failsafe is the applied one.
    #[test]
    fn diagnostics_report_is_complete() {
        let mut cfg = with_profile(Preset::Balanced);
        cfg.profile_mut().fan_mut("mb.pwm2", FanRole::System).mode = FanMode::Bios;
        let mut e = engine(cfg);
        e.tick();
        e.tick();
        e.write_diagnostics();
        // Completed off the engine thread.
        assert!(e.diag_path.is_none() && e.diag_job.is_some());
        while !e.diag_job.as_ref().is_some_and(|j| j.is_finished()) {
            std::thread::sleep(Duration::from_millis(10));
        }
        e.tick();
        let path = e.diag_path.clone().expect("report written");
        let r = std::fs::read_to_string(path).unwrap();
        for want in [
            "Written:",
            " UTC)",
            "(git ",
            "Windows:",
            "PawnIO.Modules 0.2.11",
            "active profile #2 'Balanced'",
            "Sensors:",
            "Fan channels:",
            "est. ",
            "100 % at 90 °C (input 2)",
            "colour order",
            "gamma",
            "LED_C slot auto",
            "Dynamic Lighting",
            "Conflicting software:",
            "Active alerts:",
            "Recent coded events",
            "Last 50 log lines:",
        ] {
            assert!(r.contains(want), "missing {want:?} in:\n{r}");
        }
    }

    /// GPU power for the AI, per sensor unit.
    #[test]
    fn gpu_power_uses_watts_directly() {
        assert_eq!(gpu_power_w(Some(SensorKind::Power), Some(11.5), Some(3.0), 190.0), (Some(11.5), false));
        assert_eq!(gpu_power_w(Some(SensorKind::Load), Some(50.0), Some(3.0), 190.0), (Some(95.0), false));
        let (p, est) = gpu_power_w(Some(SensorKind::Power), None, Some(50.0), 190.0);
        assert!(est && (p.unwrap() - 80.75).abs() < 1e-9, "Intel value gated out: load estimate, never %·TBP");
        assert_eq!(gpu_power_w(None, None, None, 190.0), (None, true));
    }

    // ---- fan presets, roles, identification, efficacy, notifications (feat/presets) ----

    fn last_curve(k: &Arc<Mutex<Knobs>>, fan: usize) -> Option<HwCurve> {
        k.lock().unwrap().curves.iter().rev().find(|c| c.0 == fan).map(|c| c.1.clone())
    }

    /// Silent / Balanced / Performance run every fan curve in the chip on
    /// the CPU input with ΔT 3 / 2 / 1, the pump on its own curve likewise; Full speed is 100 %
    /// everywhere; Auto (AI) runs the pump on the Balanced pump curve; BIOS writes nothing.
    #[test]
    fn presets_map_to_the_chip_per_role() {
        let pump_sim = || {
            let mut hw = SimHw::new(1.0);
            hw.fan_desc_mut(2).role = FanRole::Pump;
            hw
        };
        for (preset, dt) in [(Preset::Silent, 3), (Preset::Balanced, 2), (Preset::Performance, 1)] {
            let (mut e, k) = fake_engine_on(with_profile(preset), Knobs::default(), pump_sim());
            for _ in 0..3 {
                e.tick();
            }
            let f = fans(&e);
            for i in [0, 1, 2] {
                let c = last_curve(&k, i).unwrap_or_else(|| panic!("{preset:?}: no chip curve for fan {i}"));
                assert!(f[i].hw_curve && f[i].mode == "Curve", "{preset:?}: {:?}", f[i]);
                assert_eq!((c.temp_input, c.delta_t), (2, Some(dt)), "{preset:?} fan {i}");
                assert!(c.full_speed_at <= 90.0);
            }
            assert_eq!(f[2].floor, Some(crate::config::PUMP_MIN_DUTY));
            let pump = FanConfig::preset(preset, FanRole::Pump).curve;
            let fan = FanConfig::preset(preset, FanRole::Cpu).curve;
            assert!(pump.points.iter().zip(&fan.points).any(|(a, b)| a != b), "{preset:?}: the pump's own curve");
        }
        let (mut e, _k) = fake_engine_on(with_profile(Preset::FullSpeed), Knobs::default(), pump_sim());
        e.tick();
        assert!(fans(&e).iter().all(|f| f.duty == Some(100.0)), "{:?}", fans(&e));
        let (mut e, _k) = fake_engine_on(with_profile(Preset::AiAuto), Knobs::default(), pump_sim());
        for _ in 0..5 {
            e.tick();
        }
        let f = fans(&e);
        assert!(f[2].hw_curve && f[2].mode.starts_with("Curve"), "{:?}", f[2]);
        assert_eq!(f[2].ai_refused, None, "the AI preset gives the pump its curve, nothing is refused");
        assert!(f[0].mode.starts_with("AI") && f[1].mode.starts_with("AI"), "{f:?}");
        let (mut e, k) = fake_engine_on(with_profile(Preset::Bios), real(), pump_sim());
        for _ in 0..3 {
            e.tick();
        }
        let k = k.lock().unwrap();
        assert_eq!((k.set_fans, k.curves.len()), (0, 0), "BIOS writes nothing");
    }

    /// SimHw with a settable GPU temperature and a cool CPU, recording chip curves.
    struct LiftHw {
        inner: SimHw,
        gpu: Arc<Mutex<f32>>,
        curves: Arc<Mutex<Vec<(usize, HwCurve)>>>,
    }

    impl HwBackend for LiftHw {
        fn info(&self) -> &HwInfo {
            self.inner.info()
        }
        fn sensors(&self) -> &[crate::model::SensorDesc] {
            self.inner.sensors()
        }
        fn fans(&self) -> &[crate::model::FanChannelDesc] {
            self.inner.fans()
        }
        fn poll(&mut self) -> Vec<Option<f32>> {
            self.inner.set_heatsink(30.0);
            let mut v = self.inner.poll();
            let g = self.inner.sensors().iter().position(|s| s.id == ids::GPU_TEMP).unwrap();
            v[g] = Some(*self.gpu.lock().unwrap());
            v
        }
        fn set_fan(&mut self, fan: usize, duty: f32) -> anyhow::Result<()> {
            self.inner.set_fan(fan, duty)
        }
        fn set_fan_curve(&mut self, fan: usize, curve: &HwCurve) -> anyhow::Result<()> {
            self.curves.lock().unwrap().push((fan, curve.clone()));
            self.inner.set_fan_curve(fan, curve)
        }
        fn chip_temp_input(&self, id: &str) -> Option<u8> {
            self.inner.chip_temp_input(id)
        }
        fn has_hw_failsafe(&self, fan: usize) -> bool {
            self.inner.has_hw_failsafe(fan)
        }
        fn release_fan(&mut self, fan: usize) -> anyhow::Result<()> {
            self.inner.release_fan(fan)
        }
        fn release_all(&mut self) -> Vec<usize> {
            self.inner.release_all()
        }
        fn crash_snapshot(&self) -> Option<crate::hw::CrashSnapshot> {
            None
        }
    }

    /// A GPU-only load raises the case fans' chip curve (StartPWM up)
    /// on the CPU input, in 5 % steps, without re-programming the chip on every tick or on sensor
    /// noise; the CPU fan's curve is not lifted. When the GPU cools the floor goes away again.
    #[test]
    fn case_fans_get_a_gpu_floor_in_the_chip_curve() {
        let gpu = Arc::new(Mutex::new(40.0f32));
        let curves = Arc::new(Mutex::new(Vec::new()));
        let hw = LiftHw { inner: SimHw::new(1.0), gpu: gpu.clone(), curves: curves.clone() };
        let mut e = engine_with(with_profile(Preset::Balanced), Box::new(hw));
        for _ in 0..20 {
            e.tick();
        }
        let f = fans(&e);
        assert!(f[1].hw_curve && f[1].gpu_lift.is_none(), "cool GPU: the plain CPU curve: {:?}", f[1]);
        let writes = |i: usize| curves.lock().unwrap().iter().filter(|c| c.0 == i).count();
        let base = curves.lock().unwrap().iter().find(|c| c.0 == 1).unwrap().1.clone();
        // GPU-only load: 80 °C on the case curve [70 °C 60 %, 80 °C 85 %] → floor 85 %.
        *gpu.lock().unwrap() = 80.0;
        for _ in 0..600 {
            e.tick();
        }
        let f = fans(&e);
        assert_eq!(f[1].gpu_lift, Some(85.0), "{:?}", f[1]);
        assert!(f[1].hw_curve && f[0].gpu_lift.is_none(), "{f:?}");
        let lifted = curves.lock().unwrap().iter().rev().find(|c| c.0 == 1).unwrap().1.clone();
        assert_eq!(lifted.temp_input, 2, "still the CPU input (failsafe in the chip)");
        assert!(lifted.points.iter().all(|p| p.1 >= 85.0) && lifted.points[0].1 > base.points[0].1, "{lifted:?}");
        assert!(lifted.full_speed_at <= 90.0);
        let n = writes(1);
        assert!(n <= 14, "one write per 5 % step at most: {n}");
        // Noise around 80 °C: no further writes.
        for k in 0..600 {
            *gpu.lock().unwrap() = if k % 2 == 0 { 79.6 } else { 80.4 };
            e.tick();
        }
        assert_eq!(writes(1), n, "noise does not re-program the chip");
        assert_eq!(writes(0), 1, "the CPU fan's curve never changed");
        // GPU cool again: the floor goes.
        *gpu.lock().unwrap() = 40.0;
        for _ in 0..800 {
            e.tick();
        }
        assert_eq!(fans(&e)[1].gpu_lift, None);
        assert_eq!(curves.lock().unwrap().iter().rev().find(|c| c.0 == 1).unwrap().1, base);
        assert!(chip_curve_floor_sensors(&crate::config::TempSource::default()) == Some(Vec::new()));
        let avg = crate::config::TempSource {
            sensors: vec!["cpu.temp".into(), "gpu0.temp".into()],
            combine: Combine::Average,
        };
        assert_eq!(chip_curve_floor_sensors(&avg), None, "an average can't be split");
        assert_eq!(
            (lift_bucket(30.0), lift_bucket(31.1), lift_bucket(32.6), lift_bucket(99.0), lift_bucket(f32::NAN)),
            (30.0, 30.0, 35.0, 100.0, 100.0)
        );
    }

    /// A header without a speed signal whose device is not known (the wizard's
    /// "Not sure") is `Unknown` (60 % floor, no AI); once the user says what it is it follows that
    /// role, but AI still needs a tach. (An unconfirmed header keeps every mode on the BIOS:
    /// `modes_wait_for_one_identification`.)
    #[test]
    fn unknown_role_floor_and_ai_refusal() {
        let mut e = engine_with(with_profile(Preset::Balanced), Box::new(SimHw::new(1.0).without_tach(0)));
        for _ in 0..3 {
            e.tick();
        }
        let f = &fans(&e)[0];
        let unknown_floor = Some(crate::config::UNKNOWN_MIN_DUTY);
        assert_eq!(
            (f.role, f.assigned, f.floor),
            (FanRole::Unknown, Some(HeaderRole::Unknown), unknown_floor),
            "{f:?}"
        );
        assert!(f.hw_curve && f.duty.is_some_and(|d| d >= 59.5), "the chip curve starts at 60 %: {f:?}");
        assert_eq!(fans(&e)[1].role, FanRole::System);
        // Auto (AI): the unknown header runs the curve instead, with the reason.
        e.shared.edit_config(|c| {
            c.apply_preset(Preset::AiAuto);
        });
        for _ in 0..5 {
            e.tick();
        }
        let f = fans(&e);
        assert_eq!(f[0].ai_refused, Some(crate::config::UNKNOWN_AI));
        assert!(f[0].mode == "Curve" && f[0].duty.is_some_and(|d| d >= 59.5), "{:?}", f[0]);
        assert!(f[1].mode.starts_with("AI") && f[1].ai_refused.is_none());
        let groups: Vec<Vec<String>> = e.shared.state.read().unwrap().ai.iter().map(|z| z.fans.clone()).collect();
        assert!(groups.iter().all(|g| !g.contains(&f[0].name)), "{groups:?}");
        // Confirmed as radiator fans: CPU role, no role floor (only the board's CPU_FAN floor, audit
        // FR-PRIV-005), but still no AI without a tach.
        e.shared.edit_config(|c| c.set_fan_role("mb.pwm0", Some(HeaderRole::RadiatorFans), None));
        e.tick();
        let f = &fans(&e)[0];
        let board_floor = Some(crate::config::CPU_HEADER_MIN_DUTY);
        assert_eq!((f.role, f.assigned, f.floor), (FanRole::Cpu, Some(HeaderRole::RadiatorFans), board_floor));
        assert_eq!(f.ai_refused, Some(crate::config::NO_SPEED_SIGNAL_AI));
        assert_eq!(f.name, "Radiator fans", "the role's friendly name");
        assert!(f.calibration_refused().is_some() && !f.stall_alert());
    }

    /// A header the user marked unused is never driven; a pump never runs the AI or below 60 % even
    /// with a stored custom entry asking for it.
    #[test]
    fn unused_headers_and_pump_rules_bind_custom_settings() {
        let mut cfg = with_profile(Preset::FullSpeed);
        cfg.set_fan_role("mb.pwm1", Some(HeaderRole::Unused), None);
        cfg.set_fan_role("mb.pwm2", Some(HeaderRole::Pump), None);
        let mut e = engine(cfg);
        e.tick();
        let f = fans(&e);
        assert!(!f[1].controlled && f[1].mode == "BIOS (unused)" && f[1].floor.is_none(), "{:?}", f[1]);
        assert_eq!((f[2].role, f[2].duty), (FanRole::Pump, Some(100.0)));
        assert_eq!(f[2].name, "AIO pump");
        // A custom profile asking for AI and a low duty on the pump.
        e.shared.edit_config(|c| {
            let mut p = crate::config::Profile::new("Mine", Preset::Custom);
            let ai = FanConfig { mode: FanMode::Ai, ..FanConfig::preset(Preset::Balanced, FanRole::Cpu) };
            p.fans.insert("mb.pwm2".into(), ai);
            c.profiles.push(p);
            c.active_profile = c.profiles.len() - 1;
        });
        e.tick();
        // (Duties ramp down at the pump's 3 %/s: the label shows the target.)
        let f = &fans(&e)[2];
        assert_eq!((f.mode.as_str(), f.ai_refused), ("Curve", Some(crate::config::PUMP_AI)), "{f:?}");
        e.shared.edit_config(|c| {
            let fc = c.profile_mut().fans.get_mut("mb.pwm2").unwrap();
            fc.mode = FanMode::Fixed;
            fc.fixed_duty = 25.0;
        });
        for _ in 0..400 {
            e.tick();
        }
        let f = &fans(&e)[2];
        assert_eq!(f.mode, format!("Fixed {:.0}%", crate::config::PUMP_MIN_DUTY), "the pump floor binds a custom duty");
        assert!(f.duty.is_some_and(|d| d >= crate::config::PUMP_MIN_DUTY), "{f:?}");
    }

    /// Ticks until the wizard drives `fan` at `duty` (each test tick advances it by 0.05 s).
    fn tick_until(e: &mut Engine, fan: usize, duty: f32) {
        for _ in 0..2000 {
            e.tick();
            if fans(e)[fan].identifying && fans(e)[fan].duty == Some(duty) {
                return;
            }
        }
        panic!("fan {fan} never at {duty}: {:?}", e.shared.state.read().unwrap().identify);
    }

    /// Switching to the BIOS mode mid-wizard stops the wizard and releases its
    /// header in the same tick; a wizard started under BIOS is not affected.
    #[test]
    fn bios_mode_chosen_mid_wizard_stops_it() {
        let (mut e, k) = fake_engine(with_profile(Preset::Balanced), Knobs { cool: true, ..real() });
        e.tick();
        e.handle(Command::Identify(IdentifyCmd::Start));
        e.tick();
        assert!(fans(&e)[0].identifying);
        let bios = e.shared.config_snapshot().profiles.iter().position(|p| p.preset == Preset::Bios).unwrap();
        e.shared.edit_config(|c| c.active_profile = bios);
        e.tick();
        assert!(e.identify.as_ref().is_some_and(|w| w.stopped()), "stopped");
        assert!(fans(&e).iter().all(|f| !f.controlled && !f.identifying), "{:?}", fans(&e));
        assert!(!k.lock().unwrap().held.iter().any(|h| *h));
        // Started under BIOS: keeps running.
        e.handle(Command::Identify(IdentifyCmd::Cancel));
        e.handle(Command::Identify(IdentifyCmd::Start));
        e.tick();
        e.tick();
        assert!(fans(&e)[0].identifying, "{:?}", e.identify.as_ref().map(Identify::view));
    }

    /// A panic in a command handler or in a tick is contained: the wizard stops
    /// (it does not re-drive its header), every fan goes back to the BIOS, the engine keeps running.
    #[test]
    fn engine_panics_stop_the_wizard_and_release() {
        for in_tick in [false, true] {
            let (mut e, k) = fake_engine(with_profile(Preset::Bios), Knobs { cool: true, ..real() });
            e.tick();
            e.handle(Command::Identify(IdentifyCmd::Start));
            e.tick();
            assert!(fans(&e)[0].identifying && k.lock().unwrap().held[0]);
            k.lock().unwrap().panic = true;
            if in_tick {
                e.guarded("tick", Engine::tick);
            } else {
                e.guarded("command", |e| e.handle(Command::WriteDiagnostics));
            }
            k.lock().unwrap().panic = false;
            assert!(e.identify.as_ref().is_some_and(|w| w.stopped()), "wizard stopped (in tick: {in_tick})");
            assert!(!k.lock().unwrap().held[0], "released");
            e.tick();
            e.tick();
            let f = &fans(&e)[0];
            assert!(!f.identifying && !f.controlled, "not driven again: {f:?}");
        }
        // A calibration is stopped the same way.
        let (mut e, k) = fake_engine(confirmed(with_profile(Preset::Balanced)), Knobs { cool: true, ..real() });
        e.tick();
        e.handle(Command::Calibrate(1));
        e.tick();
        k.lock().unwrap().panic = true;
        e.guarded("tick", Engine::tick);
        k.lock().unwrap().panic = false;
        assert!(e.calib.as_ref().is_some_and(|c| c.done() && c.error.is_some()));
        e.tick();
        e.tick();
        assert!(e.shared.config_snapshot().calibrations.is_empty());
    }

    /// audit FR-CTL-004: a tick that panics three times in a row leaves the fans with the BIOS until
    /// the user selects a fan mode again, instead of taking and releasing them on every poll.
    #[test]
    fn repeated_tick_panics_leave_the_fans_with_the_bios() {
        let (mut e, k) = fake_engine(with_profile(Preset::FullSpeed), Knobs::default());
        e.tick();
        assert!(fans(&e).iter().all(|f| f.controlled));
        k.lock().unwrap().panic = true;
        e.guarded("tick", Engine::tick);
        e.guarded("tick", Engine::tick);
        assert!(!e.emergency, "two in a row are tolerated");
        e.guarded("tick", Engine::tick);
        assert!(e.emergency);
        k.lock().unwrap().panic = false;
        e.guarded("tick", Engine::tick);
        assert!(fans(&e).iter().all(|f| !f.controlled), "{:?}", fans(&e));
        e.handle(Command::Resume);
        e.guarded("tick", Engine::tick);
        assert!(fans(&e).iter().all(|f| f.controlled), "{:?}", fans(&e));
        // A good tick in between resets the count.
        k.lock().unwrap().panic = true;
        e.guarded("tick", Engine::tick);
        e.guarded("tick", Engine::tick);
        k.lock().unwrap().panic = false;
        e.guarded("tick", Engine::tick);
        k.lock().unwrap().panic = true;
        e.guarded("tick", Engine::tick);
        assert!(!e.emergency);
    }

    /// The engine wakes at the end of the wizard's test phase, not at the next
    /// poll (5 s at the slowest), so the test duty ends on time.
    #[test]
    fn identify_test_phase_ends_on_a_deadline() {
        use crate::control::identify::{LOW_S, TEST_DUTY};
        let mut cfg = with_profile(Preset::Bios);
        cfg.general.poll_ms = 5000;
        let (mut e, _k) = fake_engine(cfg, Knobs { cool: true, ..Default::default() });
        e.tick();
        let far = e.last_tick + Duration::from_secs(60);
        assert_eq!(e.wake_at(far), far, "no wizard: the poll interval");
        e.handle(Command::Identify(IdentifyCmd::Start));
        e.tick();
        assert_eq!(e.wake_at(far), far, "full speed is not limited");
        tick_until(&mut e, 0, TEST_DUTY);
        let left = e.wake_at(far).duration_since(e.last_tick).as_secs_f32();
        assert!(left > 0.0 && left <= LOW_S, "{left}");
        // Woken at the deadline, that tick ends the test duty.
        let poll = e.last_tick + Duration::from_secs(5);
        assert!(e.wake_at(poll) <= poll);
        e.identify.as_mut().unwrap().step(LOW_S - 0.01, &[]);
        let wake = e.wake_at(poll);
        assert!(wake.duration_since(e.last_tick) < Duration::from_millis(50), "deadline before the poll");
        while Instant::now() < wake {
            std::thread::yield_now();
        }
        e.tick();
        let f = &fans(&e)[0];
        assert!(!f.identifying && f.duty != Some(TEST_DUTY), "{f:?}");
    }

    /// The wizard drives one header at a time (full speed, then the test duty),
    /// under the BIOS profile too, writes the answered roles, hands every header back (released,
    /// crash snapshot gone) and is covered by the crash snapshot while it drives.
    #[test]
    fn identify_wizard_on_the_sim() {
        use crate::control::identify::{Answer, FULL_DUTY, IdentifyPhase, TEST_DUTY};
        let (mut e, k) = fake_engine(with_profile(Preset::Bios), Knobs { cool: true, ..real() });
        e.tick();
        e.handle(Command::Identify(IdentifyCmd::Start));
        e.tick();
        let f = fans(&e);
        assert!(
            f[0].identifying && f[0].controlled && f[0].duty == Some(FULL_DUTY) && f[0].mode == "Identifying",
            "{f:?}"
        );
        assert!(!f[1].controlled && !f[2].controlled, "one header at a time");
        assert!(k.lock().unwrap().held[0] && e.snapshot_path.exists(), "crash snapshot while the wizard drives");
        // Calibration is refused meanwhile.
        e.handle(Command::Calibrate(1));
        e.tick();
        assert!(e.calib.as_ref().is_none_or(|c| c.done()));
        tick_until(&mut e, 0, TEST_DUTY);
        let v = e.shared.state.read().unwrap().identify.clone().unwrap();
        assert!(v.phase == IdentifyPhase::Test && v.can_answer && v.step == 1 && v.total == 3, "{v:?}");
        e.handle(Command::Identify(IdentifyCmd::Answer(Answer::Pump)));
        e.tick();
        let cfg = e.shared.config_snapshot();
        assert_eq!(cfg.header_role("mb.pwm0"), Some(HeaderRole::Pump));
        assert_eq!(cfg.fan_names.get("mb.pwm0").map(String::as_str), Some("AIO pump"));
        e.tick();
        let f = fans(&e);
        assert!(!f[0].controlled && !k.lock().unwrap().held[0], "back to the BIOS (BIOS profile): {:?}", f[0]);
        assert!(f[1].identifying && f[1].duty == Some(FULL_DUTY));
        // Stop: everything back, snapshot gone, one coded event.
        e.handle(Command::Identify(IdentifyCmd::Cancel));
        e.tick();
        assert!(fans(&e).iter().all(|f| !f.controlled && !f.identifying));
        assert_eq!(k.lock().unwrap().held, [false; 3]);
        assert!(!e.snapshot_path.exists(), "nothing held, no snapshot");
        {
            let st = e.shared.state.read().unwrap();
            assert_eq!(st.errors.iter().filter(|x| x.code == ErrorCode::IdentifyStopped).count(), 1);
            assert_eq!(st.identify.as_ref().map(|v| v.phase), Some(IdentifyPhase::Stopped));
        }
        // Closing the finished wizard.
        e.handle(Command::Identify(IdentifyCmd::Cancel));
        e.tick();
        assert!(e.shared.state.read().unwrap().identify.is_none());
    }

    /// On a fresh config no mode except BIOS drives anything until
    /// every header the wizard tests has a confirmed role (wizard or by hand). Every mode, also Full
    /// speed and a custom profile, leaves all headers on the BIOS (nothing written, no snapshot); the
    /// gate lists the unconfirmed headers; setting the last role by hand lifts it on the next tick.
    #[test]
    fn modes_wait_for_one_identification() {
        let (mut e, k) = fake_engine_unidentified(with_profile(Preset::Balanced), Knobs { cool: true, ..real() });
        let gate = |e: &Engine| e.shared.state.read().unwrap().mode_gate.clone();
        for preset in [Preset::Balanced, Preset::Silent, Preset::Performance, Preset::FullSpeed, Preset::AiAuto] {
            e.shared.edit_config(|c| {
                c.apply_preset(preset);
            });
            for _ in 0..3 {
                e.tick();
            }
            for f in fans(&e) {
                assert!(
                    !f.controlled && f.duty.is_none() && f.mode == "BIOS (identify fans first)",
                    "{preset:?}: {f:?}"
                );
            }
            assert_eq!(gate(&e), ["CPU_FAN", "SYS_FAN1", "SYS_FAN2"], "{preset:?}");
        }
        {
            let k = k.lock().unwrap();
            assert_eq!(k.held, [false; 3], "nothing written");
            assert!(k.curves.is_empty(), "no chip curve: {:?}", k.curves);
        }
        assert!(!e.snapshot_path.exists(), "nothing taken, no crash snapshot");
        assert!(e.shared.state.read().unwrap().ai.is_empty(), "no AI group");
        // A custom profile with an explicit fixed duty waits too.
        e.shared.edit_config(|c| {
            let mut p = crate::config::Profile::new("Mine", Preset::Custom);
            p.fan_mut("mb.pwm1", FanRole::System).mode = FanMode::Fixed;
            c.profiles.push(p);
            c.active_profile = c.profiles.len() - 1;
        });
        e.tick();
        assert!(fans(&e).iter().all(|f| !f.controlled), "{:?}", fans(&e));
        // Under the BIOS mode the headers are plain "BIOS"; the gate is still published (the UI's
        // tiles say why the other modes wait).
        e.shared.edit_config(|c| c.active_profile = 0);
        e.tick();
        assert!(fans(&e).iter().all(|f| !f.controlled && f.mode == "BIOS"), "{:?}", fans(&e));
        assert_eq!(gate(&e).len(), 3);
        // Two of three confirmed by hand ("What's connected?"): still waiting, for the last one.
        e.shared.edit_config(|c| {
            c.apply_preset(Preset::Balanced);
            c.set_fan_role("mb.pwm0", Some(HeaderRole::RadiatorFans), None);
            c.set_fan_role("mb.pwm1", Some(HeaderRole::CaseFans), None);
        });
        e.tick();
        assert!(fans(&e).iter().all(|f| !f.controlled), "{:?}", fans(&e));
        assert_eq!(gate(&e), ["SYS_FAN2"]);
        // "Not sure" counts as an answer: the mode applies (with the unknown-role floor there).
        e.shared.edit_config(|c| c.set_fan_role("mb.pwm2", Some(HeaderRole::Unknown), None));
        e.tick();
        let f = fans(&e);
        assert!(gate(&e).is_empty());
        assert!(f.iter().all(|f| f.controlled && f.mode == "Curve"), "{f:?}");
        assert_eq!(f[2].floor, Some(crate::config::UNKNOWN_MIN_DUTY));
        // Clearing a role ("Not set") brings the wait back at once.
        e.shared.edit_config(|c| c.set_fan_role("mb.pwm1", None, None));
        e.tick();
        assert!(fans(&e).iter().all(|f| !f.controlled), "{:?}", fans(&e));
        assert_eq!(gate(&e), ["SYS_FAN1"]);
    }

    /// The identification runs under a mode that waits for it (the reference PC: the old config had
    /// Balanced active): the header under test is driven by the wizard, the others stay on the BIOS,
    /// and once the last answer is in, the mode takes over. Headers that are not on the board and
    /// never spun don't hold the modes back.
    #[test]
    fn identify_wizard_lifts_the_wait() {
        use crate::control::identify::{Answer, FULL_DUTY, TEST_DUTY};
        let (mut e, k) = fake_engine_unidentified(with_profile(Preset::Balanced), Knobs { cool: true, ..real() });
        e.tick();
        e.handle(Command::Identify(IdentifyCmd::Start));
        for (fan, answer) in [(0, Answer::RadiatorFans), (1, Answer::CaseFans), (2, Answer::Pump)] {
            tick_until(&mut e, fan, TEST_DUTY);
            let f = fans(&e);
            for (i, g) in f.iter().enumerate() {
                if i != fan {
                    assert!(!g.controlled && !g.identifying, "others on the BIOS during header {fan}: {g:?}");
                }
            }
            assert!(!e.shared.state.read().unwrap().mode_gate.is_empty(), "still waiting at header {fan}");
            e.handle(Command::Identify(IdentifyCmd::Answer(answer)));
            e.tick();
        }
        for _ in 0..2 {
            e.tick();
        }
        assert!(e.shared.state.read().unwrap().mode_gate.is_empty());
        let f = fans(&e);
        assert!(f.iter().all(|f| f.controlled && !f.identifying), "Balanced applies: {f:?}");
        assert!(f[2].hw_curve && f[2].mode == "Curve", "the pump the wizard found runs its curve: {:?}", f[2]);
        assert!(k.lock().unwrap().held.iter().all(|h| *h));
        let _ = FULL_DUTY;
        // A header that is not on the board (and never spun) is not waited for.
        let mut hw = SimHw::new(1.0).without_tach(2);
        let d = hw.fan_desc_mut(2);
        d.present = false;
        d.documented = Some(false);
        let mut cfg = with_profile(Preset::Balanced);
        cfg.set_fan_role("mb.pwm0", Some(HeaderRole::CpuFans), None);
        cfg.set_fan_role("mb.pwm1", Some(HeaderRole::CaseFans), None);
        let mut e = engine_unidentified(cfg, Box::new(hw));
        for _ in 0..3 {
            e.tick();
        }
        assert!(e.shared.state.read().unwrap().mode_gate.is_empty());
        let f = fans(&e);
        assert!(f[0].controlled && f[1].controlled && !f[2].controlled, "{f:?}");
    }

    #[test]
    fn mode_gate_lists_identifiable_headers_without_a_role() {
        let none: Vec<usize> = Vec::new();
        assert_eq!(mode_gate(&[], &[]), none);
        assert_eq!(mode_gate(&[true, true, false], &[None, Some(HeaderRole::Unknown), None]), [0]);
        assert_eq!(mode_gate(&[true, true], &[Some(HeaderRole::Unused), Some(HeaderRole::Pump)]), none);
    }

    /// Never above 70 °C: refused when hot, stopped when it gets hot, stopped by "Return to BIOS".
    #[test]
    fn identify_wizard_respects_the_cpu_limit() {
        let heatsink = Arc::new(Mutex::new(75.0));
        let mut e = engine_with(with_profile(Preset::Balanced), Box::new(PinnedHw(SimHw::new(1.0), heatsink.clone())));
        for _ in 0..3 {
            e.tick();
        }
        e.handle(Command::Identify(IdentifyCmd::Start));
        e.tick();
        let v = e.shared.state.read().unwrap().identify.clone().unwrap();
        assert!(v.done && v.error.as_deref().is_some_and(|m| m.contains("70")), "{v:?}");
        assert!(fans(&e).iter().all(|f| !f.identifying));
        *heatsink.lock().unwrap() = 30.0;
        for _ in 0..3 {
            e.tick();
        }
        e.handle(Command::Identify(IdentifyCmd::Start));
        e.tick();
        assert!(fans(&e)[0].identifying);
        *heatsink.lock().unwrap() = 75.0;
        for _ in 0..3 {
            e.tick();
        }
        let v = e.shared.state.read().unwrap().identify.clone().unwrap();
        assert!(v.error.as_deref().is_some_and(|m| m.contains("reached")), "{v:?}");
        assert!(fans(&e).iter().all(|f| !f.identifying && f.mode != "Identifying"));
        // "Return fans to BIOS" stops it as well.
        *heatsink.lock().unwrap() = 30.0;
        for _ in 0..3 {
            e.tick();
        }
        e.handle(Command::Identify(IdentifyCmd::Start));
        e.tick();
        assert!(fans(&e)[0].identifying);
        e.handle(Command::EmergencyRelease);
        e.tick();
        assert!(fans(&e).iter().all(|f| !f.controlled && !f.identifying));
        assert!(e.identify.as_ref().is_some_and(|w| w.stopped()));
    }

    /// FRG-E309 (the detection itself is tested on the sim plant in `control::efficacy`) is
    /// inform-only in v0.2: a Critical alert that says so, the UI gets
    /// the verdict, the tray is notified once (and not at all with notifications off), but no header
    /// is forced: the CPU fans, the pump and the case fans keep their mode's duty.
    #[test]
    fn degraded_cooling_informs_and_notifies_once_without_forcing() {
        let mut sim = SimHw::new(1.0).without_tach(0);
        sim.fan_desc_mut(2).role = FanRole::Pump;
        let mut e = engine_with(with_profile(Preset::Balanced), Box::new(sim));
        e.tick();
        e.efficacy.alarm = true;
        for _ in 0..5 {
            e.tick();
        }
        let a = alerts_with(&e, ErrorCode::CoolingDegraded);
        assert_eq!(a.len(), 1, "{a:?}");
        assert_eq!(a[0].level, AlertLevel::Critical);
        assert!(a[0].detail.contains(EFFICACY_INFORM_ONLY), "{a:?}");
        let f = fans(&e);
        for f in &f {
            assert!(f.controlled && !f.failsafe && !f.mode.contains("cooling"), "not forced: {f:?}");
            assert_ne!(f.duty, Some(100.0), "{f:?}");
        }
        assert!(f[2].hw_curve && f[2].mode == "Curve", "the pump keeps its curve: {:?}", f[2]);
        assert_eq!(f[0].floor, Some(crate::config::UNKNOWN_MIN_DUTY), "the role floor still holds: {:?}", f[0]);
        {
            let st = e.shared.state.read().unwrap();
            assert_eq!(st.cpu_cooling_verified, Some(false));
            assert_eq!(st.efficacy.state, crate::control::efficacy::EfficacyState::Degraded);
        }
        for _ in 0..20 {
            e.tick();
        }
        assert_eq!(e.notified.iter().filter(|c| **c == ErrorCode::CoolingDegraded).count(), 1, "{:?}", e.notified);
        // Notifications off: nothing; on: the critical temperature notifies.
        for on in [false, true] {
            let mut cfg = with_profile(Preset::Balanced);
            cfg.general.notifications = on;
            cfg.general.cpu_critical = 10.0;
            let mut e = engine(cfg);
            for _ in 0..3 {
                e.tick();
            }
            assert!(!alerts_with(&e, ErrorCode::CriticalTemperature).is_empty());
            let want = if on { vec![ErrorCode::CriticalTemperature] } else { Vec::new() };
            assert_eq!(e.notified.iter().copied().collect::<Vec<_>>(), want);
        }
    }

    /// One call applies a preset (recreating a deleted built-in profile) and resumes after "Return
    /// to BIOS"; the hotkey cycle walks the six modes.
    #[test]
    fn apply_and_cycle_presets_through_shared() {
        let mut e = engine(with_profile(Preset::Balanced));
        let (tx, rx) = std::sync::mpsc::channel();
        *e.shared.cmd_tx.lock().unwrap() = Some(tx);
        e.rx = Some(rx);
        e.tick();
        e.handle(Command::EmergencyRelease);
        e.tick();
        assert!(fans(&e).iter().all(|f| !f.controlled));
        let i = e.shared.apply_preset(Preset::Performance);
        e.tick(); // the Resume command is handled at the end of this tick
        e.tick();
        let cfg = e.shared.config_snapshot();
        assert_eq!((cfg.active_profile, cfg.profile().preset), (i, Preset::Performance));
        assert!(!e.emergency && fans(&e).iter().all(|f| f.controlled), "resumed: {:?}", fans(&e));
        assert_eq!(e.shared.cycle_preset(), Preset::FullSpeed);
        assert_eq!(e.shared.config_snapshot().profile().preset, Preset::FullSpeed);
        e.shared.edit_config(|c| c.profiles.retain(|p| p.preset != Preset::Silent));
        let n = e.shared.config_snapshot().profiles.len();
        let i = e.shared.apply_preset(Preset::Silent);
        let cfg = e.shared.config_snapshot();
        assert_eq!((i, cfg.profiles.len(), cfg.profiles[i].name.as_str()), (n, n + 1, "Silent"));
    }

    // ---- independent tester: the board floor (audit FR-PRIV-005) ----

    /// `cfg` with `id`'s entry in the active profile set to the worst a hostile config.json can ask
    /// for: `mode` at 0 %, minimum and maximum 0, fan stop allowed.
    fn worst_entry(mut cfg: Config, id: &str, mode: FanMode) -> Config {
        let fc = cfg.profile_mut().fan_mut(id, FanRole::System);
        fc.mode = mode;
        fc.fixed_duty = 0.0;
        fc.min_duty = 0.0;
        fc.max_duty = 0.0;
        fc.allow_stop = true;
        cfg
    }

    /// A pinned, cool PC (Tctl ≈ 40 + 0.33 · 20 ≈ 47 °C): the envelope floor adds nothing.
    fn cool_pinned(sim: SimHw) -> (Box<PinnedHw>, Arc<Mutex<f32>>) {
        let mut sim = sim;
        sim.pin_cpu_power(20.0);
        let t = Arc::new(Mutex::new(40.0));
        (Box::new(PinnedHw(sim, t.clone())), t)
    }

    /// FR-PRIV-005 (tester): config.json turns the board's CPU_FAN into "Case fans" and asks for a
    /// fixed 0 % with minimum 0, maximum 0 and fan stop: the header runs at the CPU header floor from
    /// the first tick, stays there, is lifted back at once (not ramped) from a lower duty, a software
    /// curve asking for 0 % gets the floor too, the UI shows "Min 20 %", and near the critical
    /// temperature the FR-CTL-001 envelope still binds above it.
    #[test]
    fn board_cpu_header_as_case_fans_keeps_the_cpu_floor() {
        let floor = crate::config::CPU_HEADER_MIN_DUTY;
        let mut cfg = worst_entry(with_profile(Preset::Balanced), "mb.pwm0", FanMode::Fixed);
        cfg.general.cpu_critical = 95.0;
        cfg.set_fan_role("mb.pwm0", Some(HeaderRole::CaseFans), None);
        let (hw, heatsink) = cool_pinned(SimHw::new(1.0));
        let mut e = engine_with(cfg, hw);
        e.tick();
        let f = &fans(&e)[0];
        assert_eq!((f.role, f.assigned), (FanRole::System, Some(HeaderRole::CaseFans)), "{f:?}");
        assert!(f.controlled && f.duty == Some(floor) && f.floor == Some(floor), "{f:?}");
        assert_eq!(f.mode, format!("Fixed {floor:.0}%"));
        for _ in 0..50 {
            e.tick();
            assert_eq!(fans(&e)[0].duty, Some(floor), "{:?}", fans(&e)[0]);
        }
        e.fans[0].duty = Some(0.0);
        e.tick();
        assert!(fans(&e)[0].duty.is_some_and(|d| d >= floor), "lifted at once: {:?}", fans(&e)[0]);
        // A software curve (board sensor source) asking for 0 % at every temperature.
        e.shared.edit_config(|c| {
            let fc = c.profile_mut().fan_mut("mb.pwm0", FanRole::System);
            fc.mode = FanMode::Curve;
            fc.curve.source = crate::config::TempSource { sensors: vec!["mb.t0".into()], combine: Combine::Max };
            fc.curve.points = vec![[0.0, 0.0], [100.0, 0.0]];
        });
        for _ in 0..30 {
            e.tick();
            let f = &fans(&e)[0];
            assert!(!f.hw_curve && f.duty.is_some_and(|d| d >= floor), "{f:?}");
        }
        // Near the critical temperature the envelope (here ≈ 30 %) binds above the board floor.
        *heatsink.lock().unwrap() = 80.4;
        for _ in 0..4 {
            e.tick();
        }
        let env = limits::envelope_floor(e.cpu_median, 95.0);
        let f = &fans(&e)[0];
        assert!(env > floor + 5.0, "{env}");
        assert!(f.duty.is_some_and(|d| d >= env - 0.5), "{f:?}, envelope {env}");
    }

    /// FR-PRIV-005 (tester): the board's "SYS_FAN4 / PUMP" header assigned "Case fans" keeps the pump
    /// floor whatever the settings ask: a fixed 0 %, and the AI with minimum / maximum 0 and fan stop.
    #[test]
    fn board_pump_header_as_case_fans_keeps_the_pump_floor() {
        let floor = crate::config::PUMP_MIN_DUTY;
        let mut sim = SimHw::new(1.0);
        sim.fan_desc_mut(2).role = FanRole::Pump;
        let mut cfg = worst_entry(with_profile(Preset::Balanced), "mb.pwm2", FanMode::Fixed);
        cfg.set_fan_role("mb.pwm2", Some(HeaderRole::CaseFans), None);
        let (hw, _) = cool_pinned(sim);
        let mut e = engine_with(cfg, hw);
        for _ in 0..5 {
            e.tick();
        }
        let f = &fans(&e)[2];
        assert_eq!((f.role, f.duty, f.floor), (FanRole::System, Some(floor), Some(floor)), "{f:?}");
        assert!(f.controlled && f.mode == format!("Fixed {floor:.0}%"), "{f:?}");
        // The AI preset with the same hostile limits on that header.
        e.shared.edit_config(|c| {
            c.apply_preset(Preset::AiAuto);
            let fc = c.profile_mut().fan_mut("mb.pwm2", FanRole::System);
            fc.min_duty = 0.0;
            fc.max_duty = 0.0;
            fc.allow_stop = true;
        });
        let mut ai_seen = false;
        for _ in 0..200 {
            e.tick();
            let f = &fans(&e)[2];
            ai_seen |= f.mode.starts_with("AI");
            assert!(f.controlled && f.duty.is_some_and(|d| d >= floor), "{f:?}");
        }
        assert!(ai_seen, "the header ran under the AI: {:?}", fans(&e)[2]);
    }

    /// FR-PRIV-005 (tester): BIOS stays BIOS on the board's CPU and PUMP headers, as the whole preset
    /// and as one header's mode in a custom profile: nothing written, no floor shown.
    #[test]
    fn board_floor_never_takes_a_bios_header() {
        let pump_sim = || {
            let mut hw = SimHw::new(1.0);
            hw.fan_desc_mut(2).role = FanRole::Pump;
            hw
        };
        let mut cfg = worst_entry(with_profile(Preset::Bios), "mb.pwm0", FanMode::Fixed);
        cfg.set_fan_role("mb.pwm0", Some(HeaderRole::CaseFans), None);
        cfg.set_fan_role("mb.pwm2", Some(HeaderRole::CaseFans), None);
        let (mut e, k) = fake_engine_on(cfg, real(), pump_sim());
        for _ in 0..3 {
            e.tick();
        }
        assert!(fans(&e).iter().all(|f| !f.controlled && f.duty.is_none() && f.floor.is_none()), "{:?}", fans(&e));
        {
            let k = k.lock().unwrap();
            assert_eq!((k.set_fans, k.curves.len()), (0, 0), "BIOS writes nothing");
        }
        // Custom profile: CPU_FAN and the pump header on BIOS, SYS_FAN1 fixed.
        let mut cfg = with_profile(Preset::Balanced);
        let mut p = crate::config::Profile::new("Mine", Preset::Custom);
        for id in ["mb.pwm0", "mb.pwm2"] {
            p.fan_mut(id, FanRole::System).mode = FanMode::Bios;
        }
        p.fan_mut("mb.pwm1", FanRole::System).mode = FanMode::Fixed;
        cfg.profiles.push(p);
        cfg.active_profile = cfg.profiles.len() - 1;
        cfg.set_fan_role("mb.pwm0", Some(HeaderRole::CaseFans), None);
        let (mut e, k) = fake_engine_on(cfg, real(), pump_sim());
        for _ in 0..3 {
            e.tick();
        }
        let f = fans(&e);
        for i in [0, 2] {
            assert!(!f[i].controlled && f[i].duty.is_none() && f[i].floor.is_none(), "{:?}", f[i]);
        }
        assert!(f[1].controlled, "{:?}", f[1]);
        assert_eq!(k.lock().unwrap().held, [false, true, false]);
    }

    /// FR-PRIV-005 (tester): a chip-run curve (CPU input) asking for 0 % with fan stop is programmed
    /// with every point at the board floor or above (CPU header 20 %, PUMP header 30 %), also when a
    /// stored calibration would allow less, and the curve the chip fits from it never dips below.
    #[test]
    fn board_floor_lifts_the_chip_curve() {
        use crate::hw::superio::sg;
        let mut sim = SimHw::new(1.0);
        sim.fan_desc_mut(2).role = FanRole::Pump;
        let mut cfg = with_profile(Preset::Balanced);
        for id in ["mb.pwm0", "mb.pwm2"] {
            cfg = worst_entry(cfg, id, FanMode::Curve);
            let fc = cfg.profile_mut().fan_mut(id, FanRole::System);
            fc.max_duty = 100.0;
            fc.curve.source = crate::config::TempSource::default();
            fc.curve.points = vec![[30.0, 0.0], [60.0, 0.0], [85.0, 10.0]];
            cfg.set_fan_role(id, Some(HeaderRole::CaseFans), None);
        }
        let cal = FanCalibration {
            points: vec![[100.0, 1800.0], [50.0, 1000.0], [10.0, 300.0], [0.0, 0.0]],
            max_rpm: 1800.0,
            stop_duty: Some(5.0),
            start_duty: Some(10.0),
        };
        cfg.calibrations.insert("mb.pwm0".into(), cal.clone());
        let (mut e, k) = fake_engine_on(cfg.clone(), Knobs::default(), sim);
        for _ in 0..3 {
            e.tick();
        }
        let f = fans(&e);
        for (i, floor) in [(0, crate::config::CPU_HEADER_MIN_DUTY), (2, crate::config::PUMP_MIN_DUTY)] {
            assert!(f[i].hw_curve && f[i].floor == Some(floor), "{:?}", f[i]);
            let c = last_curve(&k, i).unwrap_or_else(|| panic!("no chip curve for fan {i}"));
            assert!(c.points.iter().all(|p| p.1 >= floor), "fan {i}: {c:?}");
            let chip = sg::fit(&c.points, c.full_speed_at, sg::MAX_SEGMENTS).unwrap();
            let low = (0..chip.tfull).map(|t| chip.duty_at(t as f32)).fold(100.0f32, f32::min);
            assert!(low >= floor - 1.0, "fan {i}: the chip runs {low:.1} %: {chip:?}");
        }
        // The same through `build_hw_curve` directly, with the calibration's lower stall floor.
        let raw = cfg.profile().fan("mb.pwm0", FanRole::System);
        let plan = HeaderPlan::new(raw.clone(), FanRole::System, true).with_board_floor(FanRole::Cpu);
        let c = build_hw_curve(&plan.fc, Some(&cal), 2, true, 90.0);
        assert!(c.points.iter().all(|p| p.1 >= crate::config::CPU_HEADER_MIN_DUTY), "{c:?}");
        let unfloored = build_hw_curve(&raw, Some(&cal), 2, true, 90.0);
        assert!(unfloored.points.iter().any(|p| p.1 < 1.0), "control: without the floor 0 % gets through");
    }

    /// FR-PRIV-005 (tester): calibration of a confirmed fan on the board's CPU header still sweeps
    /// below the CPU header floor (it finds the stall point there), and normal running gets the floor
    /// back once it is done.
    #[test]
    fn calibration_sweeps_a_board_cpu_header_below_its_floor() {
        let floor = crate::config::CPU_HEADER_MIN_DUTY;
        let cfg = worst_entry(confirmed(with_profile(Preset::Balanced)), "mb.pwm0", FanMode::Fixed);
        let mut e = engine_with(cfg, Box::new(CoolHw(SimHw::new(1.0))));
        e.tick();
        assert_eq!(fans(&e)[0].duty, Some(floor));
        e.handle(Command::Calibrate(0));
        let mut lowest = 100.0f32;
        let mut done = false;
        for _ in 0..5000 {
            e.tick();
            let f = &fans(&e)[0];
            if f.mode == "Calibrating" {
                lowest = lowest.min(f.duty.unwrap_or(100.0));
            }
            if e.calib.is_none() {
                done = true;
                break;
            }
        }
        assert!(done, "calibration finished");
        assert!(lowest < floor, "the sweep went down to {lowest} %");
        let cal = calibrated(&e, "mb.pwm0").expect("stored");
        assert!(cal.points.iter().any(|p| p[0] < floor) && cal.stop_duty.is_some(), "{cal:?}");
        for _ in 0..3 {
            e.tick();
        }
        let f = &fans(&e)[0];
        assert!(f.mode.starts_with("Fixed") && f.duty.is_some_and(|d| d >= floor), "{f:?}");
    }

    /// FR-PRIV-005 (tester): the Identify wizard drives its own test duty even on a header whose
    /// running floor (unknown device on the board's CPU header: 60 %, above the CPU header's 20 %) is
    /// higher, and the header is back at that floor when the wizard is cancelled.
    #[test]
    fn identify_wizard_is_not_held_at_the_running_floor() {
        use crate::control::identify::TEST_DUTY;
        let unknown = crate::config::UNKNOWN_MIN_DUTY;
        let mut cfg = worst_entry(with_profile(Preset::Balanced), "mb.pwm0", FanMode::Fixed);
        cfg.set_fan_role("mb.pwm0", Some(HeaderRole::Unknown), None);
        let (mut e, _k) = fake_engine(cfg, Knobs { cool: true, ..real() });
        e.tick();
        let f = &fans(&e)[0];
        assert_eq!((f.role, f.duty, f.floor), (FanRole::Unknown, Some(unknown), Some(unknown)), "{f:?}");
        e.handle(Command::Identify(IdentifyCmd::Start));
        tick_until(&mut e, 0, TEST_DUTY);
        assert!(TEST_DUTY < unknown && fans(&e)[0].identifying);
        e.handle(Command::Identify(IdentifyCmd::Cancel));
        e.tick();
        e.tick();
        let f = &fans(&e)[0];
        assert!(!f.identifying && f.duty.is_some_and(|d| d >= unknown), "{f:?}");
    }

    /// FR-PRIV-005 (tester): an unknown device on the board's CPU header (no tach, "Not sure") keeps the
    /// stricter 60 % floor, which the lower board floor never replaces; the UI shows 60 %.
    #[test]
    fn unknown_device_on_the_cpu_header_keeps_the_unknown_floor() {
        let unknown = crate::config::UNKNOWN_MIN_DUTY;
        let cfg = worst_entry(with_profile(Preset::Balanced), "mb.pwm0", FanMode::Fixed);
        let (hw, _) = cool_pinned(SimHw::new(1.0).without_tach(0));
        let mut e = engine_with(cfg, hw);
        for _ in 0..3 {
            e.tick();
        }
        let f = &fans(&e)[0];
        assert_eq!((f.role, f.assigned), (FanRole::Unknown, Some(HeaderRole::Unknown)), "{f:?}");
        assert_eq!((f.duty, f.floor), (Some(unknown), Some(unknown)), "{f:?}");
        assert_eq!(f.mode, format!("Fixed {unknown:.0}%"));
    }

    /// PinnedHw that also runs chip curves (the FR-CTL-001 / chip-curve interaction).
    struct PinnedChipHw(SimHw, Arc<Mutex<f32>>);

    impl HwBackend for PinnedChipHw {
        fn info(&self) -> &HwInfo {
            self.0.info()
        }
        fn sensors(&self) -> &[crate::model::SensorDesc] {
            self.0.sensors()
        }
        fn fans(&self) -> &[crate::model::FanChannelDesc] {
            self.0.fans()
        }
        fn poll(&mut self) -> Vec<Option<f32>> {
            let t = *self.1.lock().unwrap();
            self.0.set_heatsink(t);
            self.0.poll()
        }
        fn set_fan(&mut self, fan: usize, duty: f32) -> anyhow::Result<()> {
            self.0.set_fan(fan, duty)
        }
        fn set_fan_curve(&mut self, fan: usize, curve: &HwCurve) -> anyhow::Result<()> {
            self.0.set_fan_curve(fan, curve)
        }
        fn chip_temp_input(&self, id: &str) -> Option<u8> {
            self.0.chip_temp_input(id)
        }
        fn has_hw_failsafe(&self, fan: usize) -> bool {
            self.0.has_hw_failsafe(fan)
        }
        fn release_fan(&mut self, fan: usize) -> anyhow::Result<()> {
            self.0.release_fan(fan)
        }
        fn release_all(&mut self) -> Vec<usize> {
            self.0.release_all()
        }
        fn crash_snapshot(&self) -> Option<crate::hw::CrashSnapshot> {
            None
        }
    }

    /// FR-CTL-001 / FR-PRIV-005 interaction (tester): README says that near the critical temperature
    /// "every fan FanRGB drives" gets the envelope floor (75 % at 2 °C below it), so a curve the chip
    /// runs on its CPU input must carry it too: here a case fan's curve asking 0 % up to the chip
    /// failsafe (90 °C), with Tctl ≈ 87 °C where the envelope asks for ≈ 67 %. (Found missing on
    /// 2026-11-12; `envelope_hw_curve` now merges the floor into every chip curve.)
    #[test]
    fn envelope_floor_binds_chip_run_curves_too() {
        let mut cfg = worst_entry(with_profile(Preset::Balanced), "mb.pwm1", FanMode::Curve);
        cfg.general.cpu_critical = 90.0;
        let fc = cfg.profile_mut().fan_mut("mb.pwm1", FanRole::System);
        fc.max_duty = 100.0;
        fc.curve.source = crate::config::TempSource::default();
        fc.curve.points = vec![[30.0, 0.0], [85.0, 0.0]];
        let mut sim = SimHw::new(1.0);
        sim.pin_cpu_power(20.0);
        let mut e = engine_with(cfg, Box::new(PinnedChipHw(sim, Arc::new(Mutex::new(80.4)))));
        for _ in 0..6 {
            e.tick();
        }
        let env = limits::envelope_floor(e.cpu_median, 90.0);
        let f = &fans(&e)[1];
        assert!(!e.crit_latched && env > 50.0, "{env}");
        assert!(f.controlled && f.hw_curve, "runs in the chip: {f:?}");
        assert!(
            f.duty.is_some_and(|d| d >= env - 1.0),
            "chip curve at {:?} % against the envelope's {env:.0} %",
            f.duty
        );
    }

    // ---- independent tester: CPU temperature lost (audit FR-CTL-003) ----

    /// SimHw whose CPU temperature reading can be replaced (garbage, or gone).
    struct CpuReadingHw {
        inner: SimHw,
        cpu: Arc<Mutex<Option<Option<f32>>>>,
    }

    impl HwBackend for CpuReadingHw {
        fn info(&self) -> &HwInfo {
            self.inner.info()
        }
        fn sensors(&self) -> &[crate::model::SensorDesc] {
            self.inner.sensors()
        }
        fn fans(&self) -> &[crate::model::FanChannelDesc] {
            self.inner.fans()
        }
        fn poll(&mut self) -> Vec<Option<f32>> {
            self.inner.set_heatsink(35.0);
            let mut v = self.inner.poll();
            if let Some(r) = *self.cpu.lock().unwrap() {
                let i = self.inner.sensors().iter().position(|s| s.id == ids::CPU_TEMP).unwrap();
                v[i] = r;
            }
            v
        }
        fn set_fan(&mut self, fan: usize, duty: f32) -> anyhow::Result<()> {
            self.inner.set_fan(fan, duty)
        }
        fn set_fan_curve(&mut self, fan: usize, curve: &HwCurve) -> anyhow::Result<()> {
            self.inner.set_fan_curve(fan, curve)
        }
        fn chip_temp_input(&self, id: &str) -> Option<u8> {
            self.inner.chip_temp_input(id)
        }
        fn has_hw_failsafe(&self, fan: usize) -> bool {
            self.inner.has_hw_failsafe(fan)
        }
        fn release_fan(&mut self, fan: usize) -> anyhow::Result<()> {
            self.inner.release_fan(fan)
        }
        fn release_all(&mut self) -> Vec<usize> {
            self.inner.release_all()
        }
        fn crash_snapshot(&self) -> Option<crate::hw::CrashSnapshot> {
            None
        }
    }

    fn cpu_reading_engine(cfg: Config) -> (Engine, Arc<Mutex<Option<Option<f32>>>>) {
        let cpu = Arc::new(Mutex::new(None));
        (engine_with(cfg, Box::new(CpuReadingHw { inner: SimHw::new(1.0), cpu: cpu.clone() })), cpu)
    }

    /// FR-CTL-003 (tester): with the CPU temperature gone, software curves that still have another
    /// sensor (the case fans' `max(CPU, GPU)` curve, a board-sensor curve) are not kept running on what
    /// is left: after the 5 s grace they go to the BIOS with the reason shown and an alert. During the
    /// grace they hold; when the reading is back, control resumes.
    #[test]
    fn curves_on_other_sensors_go_to_the_bios_when_the_cpu_temperature_is_lost() {
        let mut cfg = with_profile(Preset::Balanced);
        let fc = cfg.profile_mut().fan_mut("mb.pwm1", FanRole::System);
        fc.curve.source = crate::config::TempSource { sensors: vec!["mb.t0".into()], combine: Combine::Max };
        let (mut e, cpu) = cpu_reading_engine(cfg);
        for _ in 0..3 {
            e.tick();
        }
        assert!(fans(&e).iter().all(|f| f.controlled), "{:?}", fans(&e));
        assert!(!fans(&e)[1].hw_curve && fans(&e)[2].hw_curve, "{:?}", fans(&e));
        *cpu.lock().unwrap() = Some(None);
        e.tick();
        assert!(fans(&e).iter().all(|f| f.controlled), "grace: {:?}", fans(&e));
        e.cpu_missing_since = Some(ago(6.0));
        e.tick();
        let f = fans(&e);
        for g in &f[1..] {
            assert!(!g.controlled && g.failsafe && g.mode == "BIOS (CPU temperature lost)", "{g:?}");
        }
        // CPU_FAN's curve runs in the chip on its own CPU input: it is never left on a software duty.
        assert!(!f[0].controlled || f[0].hw_curve, "{:?}", f[0]);
        assert!(!alerts_with(&e, ErrorCode::SensorLost).is_empty());
        *cpu.lock().unwrap() = None;
        for _ in 0..3 {
            e.tick();
        }
        assert!(fans(&e).iter().all(|f| f.controlled), "{:?}", fans(&e));
    }

    /// FR-CTL-003 (tester): a CPU sensor that keeps answering with garbage (NaN, -128 °C from a missing
    /// thermistor, 255 °C) counts as lost: fixed duties go to the BIOS after the grace too.
    #[test]
    fn implausible_cpu_readings_count_as_a_lost_temperature() {
        for garbage in [f32::NAN, -128.0, 255.0] {
            let mut cfg = with_profile(Preset::Balanced);
            for (id, role) in [("mb.pwm0", FanRole::Cpu), ("mb.pwm1", FanRole::System), ("mb.pwm2", FanRole::System)] {
                let fc = cfg.profile_mut().fan_mut(id, role);
                fc.mode = FanMode::Fixed;
                fc.fixed_duty = 35.0;
            }
            let (mut e, cpu) = cpu_reading_engine(cfg);
            for _ in 0..3 {
                e.tick();
            }
            assert!(fans(&e).iter().all(|f| f.controlled), "{garbage}: {:?}", fans(&e));
            *cpu.lock().unwrap() = Some(Some(garbage));
            for _ in 0..3 {
                e.tick();
            }
            assert!(e.cpu_missing_since.is_some(), "{garbage}: not seen as missing");
            e.cpu_missing_since = Some(ago(6.0));
            e.tick();
            let f = fans(&e);
            assert!(f.iter().all(|f| !f.controlled && f.failsafe), "{garbage}: {f:?}");
        }
    }

    /// FR-CTL-003 (tester): a critical latch that is already up keeps every header at 100 % while the
    /// CPU temperature is gone; it is not handed to the BIOS by the lost-temperature rule.
    #[test]
    fn critical_latch_keeps_full_speed_while_the_cpu_temperature_is_lost() {
        let mut cfg = with_profile(Preset::Balanced);
        for (id, role) in [("mb.pwm0", FanRole::Cpu), ("mb.pwm1", FanRole::System), ("mb.pwm2", FanRole::System)] {
            let fc = cfg.profile_mut().fan_mut(id, role);
            fc.mode = FanMode::Fixed;
            fc.fixed_duty = 30.0;
        }
        cfg.general.cpu_critical = 10.0;
        let (mut e, cpu) = cpu_reading_engine(cfg);
        e.tick();
        assert!(e.crit_latched && fans(&e).iter().all(|f| f.duty == Some(100.0)), "{:?}", fans(&e));
        *cpu.lock().unwrap() = Some(None);
        e.shared.edit_config(|c| c.general.cpu_critical = 90.0);
        for _ in 0..3 {
            e.tick();
        }
        e.cpu_missing_since = Some(ago(6.0));
        e.crit_ok_since = Some(ago(20.0));
        for _ in 0..3 {
            e.tick();
        }
        let f = fans(&e);
        assert!(e.crit_latched, "the latch holds while the CPU is unknown");
        assert!(f.iter().all(|f| f.controlled && f.duty == Some(100.0) && f.failsafe), "{f:?}");
    }

    // ---- independent tester: stored calibration tables (audit FR-CTL-002), end to end ----

    /// Mean duty of the case fans after `ticks` AI ticks with a hot GPU and a cool CPU, with `cal` (if
    /// any) stored for both case fans.
    fn ai_case_duty_with_hot_gpu(cal: Option<FanCalibration>, ticks: usize) -> f32 {
        let gpu = Arc::new(Mutex::new(88.0f32));
        let hw = LiftHw { inner: SimHw::new(1.0), gpu, curves: Arc::new(Mutex::new(Vec::new())) };
        let mut cfg = with_profile(Preset::AiAuto);
        if let Some(c) = cal {
            cfg.calibrations.insert("mb.pwm1".into(), c.clone());
            cfg.calibrations.insert("mb.pwm2".into(), c);
        }
        let mut e = engine_with(cfg, Box::new(hw));
        for _ in 0..ticks {
            e.tick();
        }
        let f = fans(&e);
        assert!(f[1].mode.starts_with("AI") && f[2].mode.starts_with("AI"), "{f:?}");
        (f[1].duty.unwrap_or(0.0) + f[2].duty.unwrap_or(0.0)) / 2.0
    }

    /// FR-CTL-002 (tester), end to end: with the GPU 1 °C under the case group's limit the AI asks
    /// for full airflow; an inverted table (which the clamp drops) changes nothing.
    #[test]
    fn inverted_table_does_not_hold_ai_case_fans_low() {
        let base = ai_case_duty_with_hot_gpu(None, 300);
        assert!(base >= 80.0, "baseline {base}");
        let inverted = FanCalibration {
            points: vec![[100.0, 300.0], [50.0, 900.0], [0.0, 1800.0]],
            max_rpm: 1800.0,
            ..Default::default()
        };
        let got = ai_case_duty_with_hot_gpu(Some(inverted), 300);
        assert!(got >= base - 5.0, "inverted table: {got} % against {base} %");
    }

    /// FR-CTL-002 (tester), end to end: the same with a flat table (same RPM at every duty). Found
    /// kept by `FanCalibration::sane` on 2026-11-12; since then the clamp drops it like the wizard does.
    #[test]
    fn flat_table_does_not_hold_ai_case_fans_low() {
        let base = ai_case_duty_with_hot_gpu(None, 300);
        let flat = FanCalibration {
            points: vec![[0.0, 1500.0], [50.0, 1500.0], [100.0, 1500.0]],
            max_rpm: 1500.0,
            ..Default::default()
        };
        assert!(!flat.sane(), "the clamp drops it");
        let got = ai_case_duty_with_hot_gpu(Some(flat), 300);
        assert!(got >= base - 5.0, "flat table: {got} % against {base} %");
    }

    // ---- independent tester, re-check of the fixes in fb636b2 ----

    /// FR-CTL-002 re-check (tester): real calibrations from the wizard on the simulated fans (all three
    /// headers, run one after the other) still pass the stricter clamp, survive `sanitize`, and map
    /// full airflow to the top of the table.
    #[test]
    fn wizard_tables_from_the_sim_pass_the_stricter_clamp() {
        let mut e = engine_with(confirmed(with_profile(Preset::Balanced)), Box::new(CoolHw(SimHw::new(1.0))));
        e.tick();
        for fan in 0..3 {
            e.handle(Command::Calibrate(fan));
            let mut done = false;
            for _ in 0..5000 {
                e.tick();
                if e.calib.is_none() {
                    done = true;
                    break;
                }
            }
            assert!(done, "fan {fan}: calibration finished");
        }
        let mut cfg = e.shared.config_snapshot();
        assert_eq!(cfg.calibrations.len(), 3, "{:?}", cfg.calibrations);
        for (id, c) in &cfg.calibrations {
            assert!(c.sane(), "{id}: {c:?}");
            assert!(crate::control::calibrate::validate(c).is_ok(), "{id}");
            assert!(airflow_to_duty(1.0, Some(c)) >= 99.0, "{id}: {c:?}");
        }
        cfg.sanitize();
        assert_eq!(cfg.calibrations.len(), 3, "sanitize keeps the wizard's tables");
    }

    /// FR-CTL-002 re-check (tester), end to end: tables that pass the new `sane` but still mislead the
    /// AI: RPM climbing by the slack at each step as the duty falls (then 0 RPM at 0 %), and a
    /// `max_rpm` 20 % below the fastest measured RPM on a saturating fan. With the GPU 1 °C under the
    /// case group's limit the case fans should still get about full speed.
    #[test]
    fn sane_but_misleading_tables_do_not_hold_ai_case_fans_low() {
        let base = ai_case_duty_with_hot_gpu(None, 300);
        let gradual = FanCalibration {
            points: vec![
                [100.0, 1000.0],
                [80.0, 1150.0],
                [60.0, 1320.0],
                [40.0, 1500.0],
                [20.0, 1700.0],
                [10.0, 1920.0],
                [0.0, 0.0],
            ],
            max_rpm: 1920.0,
            ..Default::default()
        };
        let low_max = FanCalibration {
            points: vec![
                [100.0, 1450.0],
                [90.0, 1435.0],
                [80.0, 1400.0],
                [70.0, 1355.0],
                [60.0, 1290.0],
                [50.0, 1190.0],
                [40.0, 1050.0],
                [30.0, 870.0],
                [20.0, 645.0],
                [10.0, 0.0],
            ],
            max_rpm: 1165.0,
            ..Default::default()
        };
        let mut bad = Vec::new();
        for (what, c) in [("gradually inverted", gradual), ("max_rpm 0.8 x fastest", low_max)] {
            // Found kept on 2026-11-12; since then the clamp drops the low-max_rpm table and maps the
            // gradual one conservatively. Kept or dropped, the case fans must not be held low.
            let got = ai_case_duty_with_hot_gpu(Some(c), 300);
            if got < base - 5.0 {
                bad.push(format!("{what}: {got:.0} % against {base:.0} %"));
            }
        }
        assert!(bad.is_empty(), "{bad:?}");
    }

    /// FR-CTL-002 re-check (tester), end to end: a table with only `max_rpm` (fewer than two points
    /// stay valid) at either end of the accepted range only scales the AI's measured airflow and fan
    /// capacity; it must not hold the case fans low either.
    #[test]
    fn max_rpm_only_tables_do_not_hold_ai_case_fans_low() {
        let base = ai_case_duty_with_hot_gpu(None, 300);
        for max_rpm in [1.0, 25_000.0] {
            let c = FanCalibration { max_rpm, ..Default::default() };
            assert!(c.sane());
            let got = ai_case_duty_with_hot_gpu(Some(c), 300);
            assert!(got >= base - 5.0, "max_rpm {max_rpm}: {got} % against {base} %");
        }
    }

    /// What the chip runs for `points` (°C, %): sorted, never falling with temperature, a step at one
    /// temperature taken at its upper duty, flat outside the points, 100 % from `full`.
    fn chip_target(points: &[(f32, f32)], full: f32, t: f32) -> f32 {
        if t >= full {
            return 100.0;
        }
        let mut p = points.to_vec();
        p.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut top = f32::MIN;
        for q in &mut p {
            top = top.max(q.1);
            q.1 = top;
        }
        match (p.iter().rev().find(|q| q.0 <= t), p.iter().find(|q| q.0 > t)) {
            (None, _) => p[0].1,
            (Some(lo), None) => lo.1,
            (Some(lo), Some(hi)) => lo.1 + (hi.1 - lo.1) * (t - lo.0) / (hi.0 - lo.0),
        }
    }

    /// Everything `envelope_hw_curve(c, crit)` gets wrong, checked every 0.1 °C as the chip's polyline
    /// and at every whole degree as the curve the chip actually fits: running below the user's curve or
    /// the floor, changing the curve where the floor is 0, or reaching full speed later.
    fn envelope_violations(c: &HwCurve, crit: f32) -> Vec<String> {
        use crate::hw::superio::sg;
        let m = envelope_hw_curve(c, crit);
        let mut v = Vec::new();
        if m.full_speed_at > c.full_speed_at + 1e-3 {
            v.push(format!("full speed later: {} > {}", m.full_speed_at, c.full_speed_at));
        }
        // Worst shortfall below max(user, floor) and worst change where the floor is 0.
        let mut short: Option<(f32, String)> = None;
        let mut changed: Option<(f32, String)> = None;
        for i in 0..=1300 {
            let t = -10.0 + i as f32 * 0.1;
            if t >= m.full_speed_at {
                break;
            }
            let user = chip_target(&c.points, c.full_speed_at, t);
            let env = limits::envelope_floor(Some(t), crit);
            let got = chip_target(&m.points, m.full_speed_at, t);
            let s = user.max(env) - got;
            if s > 0.01 && short.as_ref().is_none_or(|w| s > w.0) {
                short = Some((s, format!("{t:.1} °C: {got:.1} % below user {user:.1} % / floor {env:.1} %")));
            }
            let d = (got - user).abs();
            if t <= crit - limits::ENVELOPE_START_MARGIN_C && d > 0.01 && changed.as_ref().is_none_or(|w| d > w.0) {
                changed = Some((d, format!("{t:.1} °C: changed to {got:.1} % from {user:.1} % where the floor is 0")));
            }
        }
        v.extend(short.map(|w| w.1));
        v.extend(changed.map(|w| w.1));
        if let Ok(chip) = sg::fit(&m.points, m.full_speed_at, sg::MAX_SEGMENTS) {
            let mut worst: Option<(f32, String)> = None;
            for t in 0..chip.tfull {
                let t = t as f32;
                let want = chip_target(&c.points, c.full_speed_at, t).max(limits::envelope_floor(Some(t), crit));
                let s = want - chip.duty_at(t);
                if s > 1.5 && worst.as_ref().is_none_or(|w| s > w.0) {
                    worst = Some((s, format!("chip at {t} °C: {:.1} % below {want:.1} %", chip.duty_at(t))));
                }
            }
            v.extend(worst.map(|w| w.1));
        }
        v
    }

    /// The chip curve the engine builds for `points` (with the stated flags) at `crit`.
    fn user_hw_curve(points: &[[f32; 2]], allow_stop: bool, crit: f32) -> HwCurve {
        let mut fc = FanConfig::preset(Preset::Balanced, FanRole::System);
        fc.curve.points = points.to_vec();
        fc.min_duty = 0.0;
        fc.allow_stop = allow_stop;
        fc.clamp_safety();
        build_hw_curve(&fc, None, 2, true, crit.min(HW_FAILSAFE_MAX_C))
    }

    /// FR-CTL-001 re-check (tester): the envelope merged into chip curves keeps the user's curve and the
    /// floor for every built-in preset and role at the lowest, default and highest `cpu_critical`, and
    /// for odd curves (one point, unsorted, past full speed, below 0 °C, falling after 100 %, steep
    /// steps a degree apart), where the curve reaches full speed outside the floor's ramp. It never
    /// moves full speed later, leaves the curve alone below the floor, keeps the board floor, and the
    /// GPU lift on top never lowers it.
    #[test]
    fn envelope_chip_curve_keeps_the_user_curve_and_the_floor() {
        let mut bad = Vec::new();
        for crit in [60.0, 75.0, 90.0, 95.0] {
            for preset in [Preset::Silent, Preset::Balanced, Preset::Performance] {
                for role in [FanRole::Cpu, FanRole::System, FanRole::Pump, FanRole::Unknown] {
                    let fc = FanConfig::preset(preset, role);
                    let c = build_hw_curve(&fc, None, 2, true, f32::min(crit, HW_FAILSAFE_MAX_C));
                    for v in envelope_violations(&c, crit) {
                        bad.push(format!("{preset:?} {role:?} at {crit}: {v}"));
                    }
                }
            }
        }
        let odd: [(&[[f32; 2]], f32); 7] = [
            (&[[50.0, 30.0]], 90.0),
            (&[[70.0, 90.0], [30.0, 20.0], [50.0, 40.0], [88.0, 100.0]], 90.0),
            (&[[30.0, 20.0], [60.0, 50.0], [95.0, 60.0]], 90.0),
            (&[[-5.0, 10.0], [40.0, 40.0], [70.0, 100.0]], 75.0),
            (&[[40.0, 30.0], [60.0, 100.0], [80.0, 40.0]], 95.0),
            (&[[40.0, 20.0], [50.0, 20.0], [51.0, 80.0], [92.0, 95.0]], 95.0),
            (&[[30.0, 0.0], [85.0, 0.0]], 90.0),
        ];
        for (pts, crit) in odd {
            for v in envelope_violations(&user_hw_curve(pts, true, crit), crit) {
                bad.push(format!("{pts:?} at {crit}: {v}"));
            }
        }
        // Board floor (CPU header) and the GPU lift on top of the merged curve.
        let mut worst = FanConfig::preset(Preset::Balanced, FanRole::System);
        worst.curve.points = vec![[30.0, 0.0], [60.0, 0.0], [90.0, 100.0]];
        worst.min_duty = 0.0;
        worst.allow_stop = true;
        let plan = HeaderPlan::new(worst, FanRole::System, true).with_board_floor(FanRole::Cpu);
        let c = build_hw_curve(&plan.fc, None, 2, true, 90.0);
        let m = envelope_hw_curve(&c, 90.0);
        let lifted = lift_hw_curve(&m, 40.0);
        for i in 0..900 {
            let t = i as f32 * 0.1;
            let env = limits::envelope_floor(Some(t), 90.0);
            let user = chip_target(&c.points, c.full_speed_at, t);
            assert!(chip_target(&m.points, m.full_speed_at, t) >= crate::config::CPU_HEADER_MIN_DUTY.max(env) - 0.01);
            let l = chip_target(&lifted.points, lifted.full_speed_at, t);
            assert!(l >= user.max(env).max(40.0) - 0.01, "lift at {t:.1}: {l}");
        }
        assert!(bad.is_empty(), "{bad:#?}");
    }

    /// FR-CTL-001 re-check (tester): when a curve reaches full speed inside the floor's ramp
    /// (between `cpu_critical - 12` and `- 2`), the upper corner is left out and the chip runs the
    /// curve's last value flat up to full speed while the floor keeps rising. With `cpu_critical` 92
    /// or more that is every curve, since the chip's full speed is capped at 90 °C.
    #[test]
    fn envelope_holds_when_full_speed_is_inside_the_ramp() {
        let cases: [(&[[f32; 2]], f32); 4] = [
            (&[[30.0, 0.0], [75.0, 0.0]], 90.0),
            (&[[30.0, 0.0], [85.0, 0.0]], 95.0),
            (&[[30.0, 0.0], [85.0, 10.0]], 92.0),
            (&[[30.0, 25.0], [70.0, 30.0]], 85.0),
        ];
        let mut bad = Vec::new();
        for (pts, crit) in cases {
            for v in envelope_violations(&user_hw_curve(pts, true, crit), crit) {
                bad.push(format!("{pts:?} at {crit}: {v}"));
            }
        }
        assert!(bad.is_empty(), "{bad:#?}");
    }

    /// FR-HW-001 regression through FR-CTL-001's merge (tester): a step at one temperature (lower duty
    /// listed first) or two points less than 0.05 °C apart are collapsed to the lower duty before the
    /// chip fit sees them, so the chip runs far below the user's curve, even where the floor is 0.
    /// (The same step listed upper-first, or 1 °C apart, is fine.)
    #[test]
    fn envelope_merge_keeps_steps_at_one_temperature() {
        let cases: [&[[f32; 2]]; 3] = [
            &[[40.0, 20.0], [50.0, 20.0], [50.0, 80.0], [70.0, 90.0]],
            &[[40.0, 20.0], [50.0, 20.0], [50.03, 80.0], [70.0, 90.0]],
            &[[30.0, 0.0], [60.0, 0.0], [60.0, 70.0], [80.0, 100.0]],
        ];
        let mut bad = Vec::new();
        for pts in cases {
            for v in envelope_violations(&user_hw_curve(pts, true, 90.0), 90.0) {
                bad.push(format!("{pts:?}: {v}"));
            }
        }
        // Controls: upper duty first is kept; and without the merge the chip fit alone keeps the step
        // (FR-HW-001), so this is new with the merge.
        let ok = user_hw_curve(&[[40.0, 20.0], [50.0, 80.0], [50.0, 20.0], [70.0, 90.0]], true, 90.0);
        assert!(envelope_violations(&ok, 90.0).is_empty());
        let raw = user_hw_curve(&[[40.0, 20.0], [50.0, 20.0], [50.0, 80.0], [70.0, 90.0]], true, 90.0);
        let chip = crate::hw::superio::sg::fit(&raw.points, raw.full_speed_at, crate::hw::superio::sg::MAX_SEGMENTS);
        assert!(chip.unwrap().duty_at(51.0) >= 79.0, "the unmerged curve reaches the chip with its step");
        assert!(bad.is_empty(), "{bad:#?}");
    }

    /// End to end on the simulated chip (tester): the two envelope-merge problems above, as the duty the
    /// chip really runs. (a) `cpu_critical` 90, a case-fan curve at 0 % to 75 °C (full speed 85 °C),
    /// CPU at ≈ 83 °C: the floor asks for ≈ 37 %. (b) A step from 20 % to 80 % at 50 °C (lower duty
    /// listed first), CPU at ≈ 55 °C: the user's curve asks for ≈ 82 %.
    #[test]
    fn envelope_merge_problems_on_the_simulated_chip() {
        let run = |points: Vec<[f32; 2]>, crit: f32, heatsink: f32| -> (f32, f32, f32) {
            let mut cfg = worst_entry(with_profile(Preset::Balanced), "mb.pwm1", FanMode::Curve);
            cfg.general.cpu_critical = crit;
            let fc = cfg.profile_mut().fan_mut("mb.pwm1", FanRole::System);
            fc.max_duty = 100.0;
            fc.curve.source = crate::config::TempSource::default();
            fc.curve.points = points.clone();
            let mut sim = SimHw::new(1.0);
            sim.pin_cpu_power(20.0);
            let mut e = engine_with(cfg, Box::new(PinnedChipHw(sim, Arc::new(Mutex::new(heatsink)))));
            for _ in 0..6 {
                e.tick();
            }
            let f = &fans(&e)[1];
            assert!(f.controlled && f.hw_curve && !e.crit_latched, "{f:?}");
            let t = heatsink + 0.33 * 20.0;
            let user = chip_target(&points.iter().map(|p| (p[0], p[1])).collect::<Vec<_>>(), 200.0, t);
            (f.duty.unwrap_or(0.0), limits::envelope_floor(e.cpu_median, crit), user)
        };
        let mut bad = Vec::new();
        let (duty, env, _) = run(vec![[30.0, 0.0], [75.0, 0.0]], 90.0, 76.4);
        if duty < env - 1.0 {
            bad.push(format!("(a) chip {duty:.1} % against the floor's {env:.1} %"));
        }
        let (duty, _, user) = run(vec![[40.0, 20.0], [50.0, 20.0], [50.0, 80.0], [70.0, 90.0]], 90.0, 48.4);
        if duty < user - 2.0 {
            bad.push(format!("(b) chip {duty:.1} % against the user's {user:.1} %"));
        }
        assert!(bad.is_empty(), "{bad:?}");
    }

    // ---- independent tester, round 3 (33a37c1) ----

    /// FR-CTL-002 round 3 (tester), end to end: a table whose highest duty is 10 % passes the new `sane`
    /// and the monotone map takes full airflow to the top of the table. With the GPU 1 °C under the
    /// case group's limit the case fans should still get about full speed.
    #[test]
    fn low_top_duty_table_does_not_hold_ai_case_fans_low() {
        let base = ai_case_duty_with_hot_gpu(None, 300);
        let low_top =
            FanCalibration { points: vec![[10.0, 1450.0], [0.0, 0.0]], max_rpm: 1450.0, ..Default::default() };
        // Found kept on 2026-11-12; now dropped (a stored table must reach 90 % duty), and the
        // monotone map would take full airflow to 100 % anyway.
        assert!(!low_top.sane(), "the clamp drops it");
        let got = ai_case_duty_with_hot_gpu(Some(low_top), 300);
        assert!(got >= base - 5.0, "highest duty 10 %: {got:.0} % against {base:.0} %");
    }

    /// FR-CTL-002 round 3 (tester), end to end: genuine tables (shaped like the simulated case fans)
    /// leave the AI's full-speed answer to a hot GPU unchanged; and a table claiming 99 % of the speed
    /// from 10 % duty (which maps airflow 0.9 to 0.99 to about 10 %, see
    /// `control::tests::high_airflow_requests_never_map_to_a_low_duty`) does not hold the fans low in
    /// this case either, because the AI's safety rise asks for full airflow, which maps to 100 %.
    #[test]
    fn genuine_and_early_saturation_tables_with_a_hot_gpu() {
        let base = ai_case_duty_with_hot_gpu(None, 300);
        let genuine = FanCalibration {
            points: (0..=10)
                .rev()
                .map(|i| {
                    let d = i as f32 * 10.0;
                    [d, if d < 18.0 { 0.0 } else { 1450.0 * (0.18 + 0.82 * d / 100.0) }]
                })
                .collect(),
            max_rpm: 1450.0,
            stop_duty: Some(20.0),
            start_duty: Some(20.0),
        };
        let early = FanCalibration {
            points: vec![[100.0, 1450.0], [10.0, 1440.0], [0.0, 0.0]],
            max_rpm: 1450.0,
            ..Default::default()
        };
        for (what, c) in [("genuine", genuine), ("99 % speed from 10 %", early)] {
            assert!(c.sane(), "{what}: the clamp keeps it");
            let got = ai_case_duty_with_hot_gpu(Some(c), 300);
            assert!(got >= base - 5.0, "{what}: {got:.0} % against {base:.0} %");
        }
    }

    /// The configured curve as the app runs it in software (`curve::interpolate` over the points as
    /// stored: sorted by temperature, a step in its listed order, flat outside).
    fn literal_curve(points: &[(f32, f32)], t: f32) -> f32 {
        let p: Vec<[f32; 2]> = points.iter().map(|&(a, b)| [a, b]).collect();
        crate::control::curve::interpolate(&p, t)
    }

    /// FR-CTL-001 round 3 (tester): reproducible random curves (1 to 16 points, sub-degree spacing,
    /// repeated temperatures with the step in either order, unsorted, falling segments, points past
    /// full speed), random `cpu_critical` over its whole range (60 to 95 °C, extremes included),
    /// with or without the board floor and a GPU lift on top. Checked every 0.05 °C on the chip's
    /// polyline, between every added and own point: never below the configured curve, the floor, the
    /// board floor or the lift; for curves that never fall, never below what the chip ran before the
    /// merge; unchanged below the floor's start; full speed never later. Checked at every whole degree
    /// on the curve the chip actually fits: never more than 1.5 % below.
    fn envelope_random_property(crit_lo: f32, crit_hi: f32, seed: u64) -> Vec<String> {
        use crate::hw::superio::sg;
        let mut seed = seed;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f32 / (1u64 << 53) as f32
        };
        // Worst (shortfall, where) per check: polyline vs max(configured, floor, board floor); vs the
        // unmerged chip curve (curves that never fall); change below the floor's start; lift; chip fit.
        let mut worst: [(f32, String); 5] = Default::default();
        let mut note = |i: usize, s: f32, what: String| {
            if s > worst[i].0 {
                worst[i] = (s, what);
            }
        };
        for k in 0..1500 {
            let n = 1 + (rnd() * 16.0) as usize;
            let mut pts: Vec<[f32; 2]> = Vec::new();
            let mut t = 15.0 + rnd() * 60.0;
            let rising = rnd() < 0.5;
            let mut d = rnd() * 40.0;
            for _ in 0..n {
                let r = rnd();
                t += if r < 0.2 {
                    0.0
                } else if r < 0.5 {
                    rnd() * 0.3
                } else {
                    rnd() * 12.0
                };
                d = if rising { (d + rnd() * 25.0).min(100.0) } else { rnd() * 100.0 };
                pts.push([(t * 100.0).round() / 100.0, d.round()]);
            }
            // Shuffle (repeated temperatures then keep a random order, as a file may hold them).
            for i in (1..pts.len()).rev() {
                let j = (rnd() * (i + 1) as f32) as usize % (i + 1);
                pts.swap(i, j);
            }
            let crit = match k % 10 {
                0 => crit_lo,
                1 => crit_hi,
                _ => crit_lo + (rnd() * (crit_hi - crit_lo) * 10.0).round() / 10.0,
            };
            let mut fc = FanConfig::preset(Preset::Balanced, FanRole::System);
            fc.curve.points = pts.clone();
            fc.min_duty = 0.0;
            fc.allow_stop = rnd() < 0.5;
            fc.clamp_safety();
            let board = match k % 3 {
                0 => None,
                1 => Some(FanRole::Cpu),
                _ => Some(FanRole::Pump),
            };
            let mut plan = HeaderPlan::new(fc, FanRole::System, true);
            if let Some(b) = board {
                plan = plan.with_board_floor(b);
            }
            let board_floor = plan.running_floor();
            let c = build_hw_curve(&plan.fc, None, 2, true, f32::min(crit, HW_FAILSAFE_MAX_C));
            let m = envelope_hw_curve(&c, crit);
            let lift = if rnd() < 0.3 { lift_bucket(rnd() * 80.0) } else { 0.0 };
            let l = lift_hw_curve(&m, lift);
            let ctx = || format!("{pts:?} crit {crit} board {board:?} lift {lift}: {:?} -> {:?}", c.points, m.points);
            assert!(m.full_speed_at <= c.full_speed_at + 1e-3, "full speed later: {}", ctx());
            let monotone = c.points.windows(2).all(|w| w[1].1 >= w[0].1);
            for i in 0..=2600 {
                let t = -5.0 + i as f32 * 0.05;
                if t >= m.full_speed_at {
                    break;
                }
                let floor = limits::envelope_floor(Some(t), crit);
                let want = literal_curve(&c.points, t).max(floor).max(board_floor);
                let got = chip_target(&m.points, m.full_speed_at, t);
                note(0, want - got, format!("{t:.2} °C: {got:.2} % against {want:.2} %, {}", ctx()));
                if monotone {
                    let before = chip_target(&c.points, c.full_speed_at, t).max(floor);
                    note(1, before - got, format!("{t:.2} °C: {got:.2} % below the unmerged {before:.2} %, {}", ctx()));
                    if t < crit - limits::ENVELOPE_START_MARGIN_C {
                        // (An own point within 0.05 °C above the corner stands in for it: at most
                        // 0.05 °C of the floor's slope, 0.375 %, reaches below the corner.)
                        let plain = chip_target(&c.points, c.full_speed_at, t);
                        let off = if got < plain - 0.01 { plain - got + 0.4 } else { (got - plain).max(0.0) };
                        note(2, off, format!("{t:.2} °C changed {plain:.2} -> {got:.2} %, {}", ctx()));
                    }
                }
                if t < l.full_speed_at {
                    let lg = chip_target(&l.points, l.full_speed_at, t);
                    note(3, want.max(lift) - lg, format!("lift {lift} at {t:.2} °C: {lg:.2} %, {}", ctx()));
                }
            }
            if let Ok(chip) = sg::fit(&m.points, m.full_speed_at, sg::MAX_SEGMENTS) {
                for t in 0..chip.tfull {
                    let t = t as f32;
                    let want = literal_curve(&c.points, t).max(limits::envelope_floor(Some(t), crit)).max(board_floor);
                    let s = want - chip.duty_at(t);
                    note(4, s, format!("chip at {t} °C: {:.2} % against {want:.2} %, {}", chip.duty_at(t), ctx()));
                }
            }
        }
        // Allowed: 0.4 % on the polyline (corner stand-in), 1.5 % for the chip fit (1 % plus rounding).
        let limit = [0.4, 0.4, 0.4, 0.4, 1.5];
        let names = ["polyline", "unmerged", "below the floor", "lift", "chip fit"];
        println!("{:?}", worst.iter().map(|w| w.0).collect::<Vec<_>>());
        (0..5)
            .filter(|&i| worst[i].0 > limit[i])
            .map(|i| format!("{} {:.2} %: {}", names[i], worst[i].0, worst[i].1))
            .collect()
    }

    #[test]
    fn envelope_chip_curve_random_property() {
        let bad = envelope_random_property(60.0, 91.0, 0x9E37_79B9_7F4A_7C15);
        assert!(bad.is_empty(), "{bad:#?}");
    }

    /// The same with `cpu_critical` 92 to 95 °C, where the chip's full speed (capped at 90 °C) lies
    /// below the floor's upper corner (`cpu_critical - 2`): when the curve has points on both sides of
    /// full speed, neither the corner nor a point at full speed is added, and the chip interpolates from
    /// the last point below to the first point past it (raised to 75 %), under the floor's concave
    /// corner. E.g. 30 °C 0 %, 85 °C 0 %, 100 °C 10 % at `cpu_critical` 95: about 35 % at 89.9 °C
    /// where the floor asks for 52 %.
    #[test]
    fn envelope_random_property_at_cpu_critical_92_to_95() {
        let bad = envelope_random_property(92.0, 95.0, 0x2545_F491_4F6C_DD1D);
        let c = user_hw_curve(&[[30.0, 0.0], [85.0, 0.0], [100.0, 10.0]], true, 95.0);
        let explicit = envelope_violations(&c, 95.0);
        assert!(bad.is_empty() && explicit.is_empty(), "{bad:#?}\n{explicit:#?}");
    }

    /// The same end to end on the simulated chip: `cpu_critical` 95, a case-fan curve 30 °C 0 %,
    /// 85 °C 0 %, 100 °C 10 %, CPU at ≈ 89 °C (below the chip's 90 °C full speed and the latch): the
    /// floor asks for ≈ 45 %.
    #[test]
    fn envelope_at_cpu_critical_95_on_the_simulated_chip() {
        let mut cfg = worst_entry(with_profile(Preset::Balanced), "mb.pwm1", FanMode::Curve);
        cfg.general.cpu_critical = 95.0;
        let fc = cfg.profile_mut().fan_mut("mb.pwm1", FanRole::System);
        fc.max_duty = 100.0;
        fc.curve.source = crate::config::TempSource::default();
        fc.curve.points = vec![[30.0, 0.0], [85.0, 0.0], [100.0, 10.0]];
        let mut sim = SimHw::new(1.0);
        sim.pin_cpu_power(20.0);
        let mut e = engine_with(cfg, Box::new(PinnedChipHw(sim, Arc::new(Mutex::new(82.4)))));
        for _ in 0..6 {
            e.tick();
        }
        let f = &fans(&e)[1];
        let env = limits::envelope_floor(e.cpu_median, 95.0);
        assert!(f.controlled && f.hw_curve && !e.crit_latched && env > 40.0, "{f:?}, floor {env}");
        assert!(f.duty.is_some_and(|d| d >= env - 1.0), "chip at {:?} % against the floor's {env:.1} %", f.duty);
    }

    /// FR-CTL-001 round 3 (tester), for the record: on a curve that falls with temperature, the chip
    /// always ran the running maximum (more than configured); a point added at the floor's corner now
    /// takes the configured value there, so the chip runs lower than before, though never below the
    /// configured curve or the floor. (30 °C 60 %, 60 °C 30 %, 90 °C 100 %, `cpu_critical` 90.)
    #[test]
    fn falling_curves_now_follow_their_configured_value_at_the_corners() {
        let c = user_hw_curve(&[[30.0, 60.0], [60.0, 30.0], [90.0, 100.0]], true, 90.0);
        let m = envelope_hw_curve(&c, 90.0);
        let before = chip_target(&c.points, c.full_speed_at, 78.0);
        let after = chip_target(&m.points, m.full_speed_at, 78.0);
        let configured = literal_curve(&c.points, 78.0);
        assert!((before - 84.0).abs() < 0.1 && (after - 72.0).abs() < 0.1, "{before} -> {after}");
        assert!(after >= configured - 0.01 && after >= limits::envelope_floor(Some(78.0), 90.0));
    }

    // ---- independent tester, round 4 (285a886: a point at full speed always) ----

    /// FR-CTL-001 round 4 (tester): the random sweep again over the whole `cpu_critical` range (60 to
    /// 95 °C, both ends) with two fresh seeds.
    #[test]
    fn envelope_random_property_full_range_fresh_seeds() {
        let mut bad = envelope_random_property(60.0, 95.0, 0x5851_F42D_4C95_7F2D);
        bad.extend(envelope_random_property(88.0, 95.0, 0x1405_7B7E_F767_814F));
        assert!(bad.is_empty(), "{bad:#?}");
    }

    /// FR-CTL-001 round 4 (tester): edge cases of the full-speed point. Own points just below or just
    /// above full speed (within the 0.05 °C "near" rule and just outside it), a full speed that is not
    /// a whole degree (`cpu_critical` 89.6), a curve whose own 100 % point sets full speed, a single
    /// point, and a curve entirely past full speed: none dips below the configured curve or the floor,
    /// and a point at full speed is there whenever no own point is near it.
    #[test]
    fn envelope_full_speed_point_edge_cases() {
        let cases: [(&[[f32; 2]], f32); 9] = [
            (&[[30.0, 0.0], [89.97, 0.0], [100.0, 10.0]], 95.0),
            (&[[30.0, 0.0], [85.0, 0.0], [90.03, 0.0], [100.0, 10.0]], 95.0),
            (&[[30.0, 0.0], [89.9, 0.0], [100.0, 10.0]], 95.0),
            (&[[30.0, 0.0], [85.0, 0.0], [90.1, 0.0], [100.0, 10.0]], 95.0),
            (&[[30.0, 0.0], [85.0, 0.0], [100.0, 10.0]], 89.6),
            (&[[30.0, 0.0], [86.0, 100.0], [95.0, 20.0]], 95.0),
            (&[[60.0, 5.0]], 95.0),
            (&[[95.0, 0.0], [110.0, 20.0]], 95.0),
            (&[[30.0, 0.0], [85.0, 0.0], [100.0, 10.0]], 92.0),
        ];
        let mut bad = Vec::new();
        for (pts, crit) in cases {
            let c = user_hw_curve(pts, true, crit);
            let m = envelope_hw_curve(&c, crit);
            for v in envelope_violations(&c, crit) {
                bad.push(format!("{pts:?} at {crit}: {v}"));
            }
            let own_near = c.points.iter().any(|p| (p.0 - c.full_speed_at).abs() < 0.05);
            let has_full = m.points.iter().any(|p| p.0 == c.full_speed_at);
            if !own_near && !has_full {
                bad.push(format!("{pts:?} at {crit}: no point at full speed {}: {:?}", c.full_speed_at, m.points));
            }
        }
        assert!(bad.is_empty(), "{bad:#?}");
    }
}
