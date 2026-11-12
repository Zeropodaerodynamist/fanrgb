//! Persistent user configuration (JSON in %APPDATA%\FanRGB\config.json).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::{FanRole, ids};

/// Lenient deserialization for settings written by other versions: an unknown enum value (renamed
/// preset, removed effect) or an unreadable sub-section falls back to the default of just that
/// value instead of resetting every setting. Every fallback is logged and collected for the
/// `ConfigLoad` notice.
mod lenient {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use serde::Deserialize;
    use serde::de::{DeserializeOwned, Deserializer};
    use serde_json::Value;

    thread_local! {
        static NOTES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    pub fn note(msg: String) {
        log::warn!("[{}] config.json: {msg}", crate::error::ErrorCode::ConfigLoad);
        NOTES.with(|n| n.borrow_mut().push(msg));
    }

    pub fn take_notes() -> Vec<String> {
        NOTES.with(|n| std::mem::take(&mut *n.borrow_mut()))
    }

    fn short(v: &Value) -> String {
        let s = v.to_string();
        if s.chars().count() > 60 { format!("{}…", s.chars().take(57).collect::<String>()) } else { s }
    }

    fn type_label<T>() -> &'static str {
        std::any::type_name::<T>().rsplit("::").next().unwrap_or("value")
    }

    /// A value that falls back to its type's default when it cannot be read.
    pub fn field<'de, D: Deserializer<'de>, T: DeserializeOwned + Default>(d: D) -> Result<T, D::Error> {
        let v = Value::deserialize(d)?;
        Ok(serde_json::from_value(v.clone()).unwrap_or_else(|e| {
            note(format!("{} {} could not be read ({e}); default used", type_label::<T>(), short(&v)));
            T::default()
        }))
    }

    /// A map whose unreadable entries are dropped (that key falls back to its defaults).
    pub fn map<'de, D: Deserializer<'de>, V: DeserializeOwned>(d: D) -> Result<BTreeMap<String, V>, D::Error> {
        let v = Value::deserialize(d)?;
        let Value::Object(m) = v else {
            note(format!("expected a map of {}, found {}; ignored", type_label::<V>(), short(&v)));
            return Ok(BTreeMap::new());
        };
        let mut out = BTreeMap::new();
        for (k, v) in m {
            match serde_json::from_value(v) {
                Ok(x) => {
                    out.insert(k, x);
                }
                Err(e) => note(format!("{} '{k}' could not be read ({e}); defaults used", type_label::<V>())),
            }
        }
        Ok(out)
    }

    /// A list whose unreadable entries become `T::default()` (indices such as the active profile
    /// keep pointing at the same entry).
    pub fn vec<'de, D: Deserializer<'de>, T: DeserializeOwned + Default>(d: D) -> Result<Vec<T>, D::Error> {
        let v = Value::deserialize(d)?;
        let Value::Array(a) = v else {
            note(format!("expected a list of {}, found {}; ignored", type_label::<T>(), short(&v)));
            return Ok(Vec::new());
        };
        Ok(a.into_iter()
            .enumerate()
            .map(|(i, v)| {
                serde_json::from_value(v).unwrap_or_else(|e| {
                    note(format!("{} #{} could not be read ({e}); default used", type_label::<T>(), i + 1));
                    T::default()
                })
            })
            .collect())
    }
}

/// `Deserialize` for a unit-variant enum that maps an unknown value to `$fallback` (logged)
/// instead of failing the whole file. Serialization stays derived (variant names as strings).
macro_rules! lenient_enum {
    ($t:ident, $fallback:ident, [$($v:ident),+ $(,)?]) => {
        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                // Exhaustiveness check: a new variant must be added to the list below.
                const _: fn($t) = |x| match x { $($t::$v => {}),+ };
                let v = serde_json::Value::deserialize(d)?;
                Ok(match v.as_str() {
                    $(Some(stringify!($v)) => $t::$v,)+
                    _ => {
                        lenient::note(format!(
                            "unknown {} value {}; using {}",
                            stringify!($t),
                            v,
                            stringify!($fallback)
                        ));
                        $t::$fallback
                    }
                })
            }
        }
    };
}

lenient_enum!(Preset, Custom, [Bios, Silent, Balanced, Performance, AiAuto, FullSpeed, Custom]);
// An unknown fan mode hands the fan to the BIOS (always safe).
lenient_enum!(FanMode, Bios, [Bios, Fixed, Curve, Ai]);
lenient_enum!(Combine, Max, [Max, Average]);
lenient_enum!(AiStyle, Balanced, [Silent, Balanced, Performance]);
lenient_enum!(
    EffectKind,
    Static,
    [
        Off,
        Static,
        Breathing,
        Spectrum,
        Rainbow,
        Gradient,
        Comet,
        Chase,
        Twinkle,
        Fire,
        Temperature,
        Load,
        Music,
        Strobe,
        HwStatic,
        HwBreathing,
        HwFlash,
        HwDoubleFlash,
        HwColorCycle,
        HwWave,
        HwRandom
    ]
);
lenient_enum!(ColorOrder, Auto, [Auto, Rgb, Rbg, Grb, Gbr, Brg, Bgr]);
lenient_enum!(ExitLighting, HardwareCycle, [KeepLast, Off, HardwareCycle]);
// An unknown role is treated as "something is connected, role unknown" (conservative floor).
lenient_enum!(HeaderRole, Unknown, [CpuFans, RadiatorFans, CaseFans, Pump, Unknown, Unused]);

// ---------------------------------------------------------------------------------------
// Storage location (demo mode must not touch the user's real files).

static STORAGE_DIR: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Redirects `config.json` and `ai_models.json` into `dir` (e.g. `--demo`). Call before
/// [`Config::load`] and before the engine starts.
pub fn redirect_storage(dir: PathBuf) {
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("cannot create {}: {e}", dir.display());
    }
    log::info!("settings and learned models are stored in {}", dir.display());
    if let Ok(mut d) = STORAGE_DIR.lock() {
        *d = Some(dir);
    }
}

fn redirected(name: &str) -> Option<PathBuf> {
    STORAGE_DIR.lock().ok().and_then(|d| d.as_ref().map(|d| d.join(name)))
}

/// The settings file in use (`%APPDATA%\FanRGB\config.json` unless redirected).
pub fn config_path() -> PathBuf {
    redirected("config.json").unwrap_or_else(crate::util::paths::config_file)
}

/// The last driver check's result (`driver_check.json` in the data folder unless redirected).
pub fn driver_check_path() -> PathBuf {
    redirected("driver_check.json").unwrap_or_else(|| crate::util::paths::data_dir().join("driver_check.json"))
}

/// Folder of the vendor driver catalog's replay state and last-good copy (the admin-only data folder
/// unless redirected; see `drivers::catalog::store`).
pub fn catalog_dir() -> PathBuf {
    STORAGE_DIR.lock().ok().and_then(|d| d.clone()).unwrap_or_else(crate::util::paths::data_dir)
}

/// The learned AI models file in use (`ai_models.json` in the data folder unless redirected).
pub fn ai_models_path() -> PathBuf {
    redirected("ai_models.json").unwrap_or_else(crate::util::paths::ai_models_file)
}

/// The learned cooling-efficacy baseline in use (`cooling_baseline.json`, data folder unless redirected).
pub fn cooling_baseline_path() -> PathBuf {
    redirected("cooling_baseline.json").unwrap_or_else(crate::util::paths::cooling_baseline_file)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    #[serde(deserialize_with = "lenient::field")]
    pub general: General,
    /// User-defined fan names keyed by fan id ("mb.pwm0").
    #[serde(deserialize_with = "lenient::map")]
    pub fan_names: BTreeMap<String, String>,
    /// Explicit visibility overrides keyed by fan id.
    #[serde(deserialize_with = "lenient::map")]
    pub fan_visible: BTreeMap<String, bool>,
    /// What is connected to each header, as the user (or the header wizard) confirmed it, keyed by
    /// fan id. Missing: not confirmed, the board table's guess applies (see [`effective_role`]).
    #[serde(deserialize_with = "lenient::map")]
    pub fan_roles: BTreeMap<String, HeaderRole>,
    #[serde(deserialize_with = "lenient::map")]
    pub calibrations: BTreeMap<String, FanCalibration>,
    #[serde(deserialize_with = "lenient::vec")]
    pub profiles: Vec<Profile>,
    #[serde(deserialize_with = "lenient::field")]
    pub active_profile: usize,
    #[serde(deserialize_with = "lenient::field")]
    pub rgb: RgbConfig,
    /// Settings of the learning (AI) controller shared by all fans.
    #[serde(deserialize_with = "lenient::field")]
    pub ai: AiGlobal,
    /// App self-update (the access token is not stored here, see `updater::token`).
    #[serde(deserialize_with = "lenient::field")]
    pub updates: UpdateConfig,
    /// Driver update check (inform only).
    #[serde(deserialize_with = "lenient::field")]
    pub drivers: DriverCheckConfig,
}

/// Driver update check settings.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DriverCheckConfig {
    /// Ask Windows Update for newer drivers once a day (opt-in: it sends the PC's hardware IDs to
    /// Microsoft, as Windows Update itself does). "Check now" works either way.
    pub auto_check: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: 1,
            general: General::default(),
            fan_names: BTreeMap::new(),
            fan_visible: BTreeMap::new(),
            fan_roles: BTreeMap::new(),
            calibrations: BTreeMap::new(),
            profiles: vec![
                Profile::new("BIOS (Smart Fan)", Preset::Bios),
                Profile::new("Silent", Preset::Silent),
                Profile::new("Balanced", Preset::Balanced),
                Profile::new("Performance", Preset::Performance),
                Profile::new("AI Auto", Preset::AiAuto),
                Profile::new("Full Speed", Preset::FullSpeed),
            ],
            active_profile: 0,
            rgb: RgbConfig::default(),
            ai: AiGlobal::default(),
            drivers: DriverCheckConfig::default(),
            updates: UpdateConfig::default(),
        }
    }
}

/// App self-update settings.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct UpdateConfig {
    /// Check at start and every 24 h once an update token is saved.
    pub auto_check: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        UpdateConfig { auto_check: true }
    }
}

/// PC-wide settings of the learning controller.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AiGlobal {
    /// GPU total board power (W) (Arc B580: 190). Only used for GPUs that report power as a
    /// percentage, and for the load-based estimate; Intel Arc reports watts directly.
    pub gpu_tbp_w: f32,
    /// CPU package power limit (W) used for workload classes and the power fallback (7500F: 88).
    pub cpu_ppt_w: f32,
    /// Learn per-workload / time-of-day "quiet ceilings" and prefer staying under them.
    pub quiet_ceiling: bool,
    /// Allow small, rare fan-speed probes (+8 % for 2 min) to learn the fan effect faster.
    pub probes: bool,
}

impl Default for AiGlobal {
    fn default() -> Self {
        AiGlobal { gpu_tbp_w: 190.0, cpu_ppt_w: 88.0, quiet_ceiling: true, probes: false }
    }
}

/// Highest critical temperature accepted for the CPU (Zen 4 Tjmax is 95 °C).
pub const CPU_CRITICAL_MAX: f32 = 95.0;
/// Highest AI limit for the CPU group (Zen 4 starts to pull power before Tjmax: B21).
pub const CPU_AI_LIMIT_MAX: f32 = 90.0;
pub const GPU_CRITICAL_MAX: f32 = 100.0;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct General {
    pub start_minimized: bool,
    pub close_to_tray: bool,
    pub poll_ms: u32,
    pub fahrenheit: bool,
    /// Any controlled fan goes to 100 % when the CPU reaches this temperature.
    pub cpu_critical: f32,
    pub gpu_critical: f32,
    pub watchdog: bool,
    pub show_all_headers: bool,
    pub welcome_done: bool,
    /// Critical alerts (cooling failure, critical temperature, sensor loss) also pop up from the
    /// tray icon, so they reach the user while the window is hidden.
    pub notifications: bool,
}

impl Default for General {
    fn default() -> Self {
        General {
            start_minimized: false,
            close_to_tray: true,
            poll_ms: 1000,
            fahrenheit: false,
            cpu_critical: 90.0,
            gpu_critical: 90.0,
            watchdog: true,
            show_all_headers: false,
            welcome_done: false,
            notifications: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub enum Preset {
    Bios,
    Silent,
    Balanced,
    Performance,
    AiAuto,
    FullSpeed,
    Custom,
}

impl Preset {
    /// The built-in fan modes in display, shortcut (Alt+1..6) and cycle order.
    pub const MODES: [Preset; 6] =
        [Preset::Silent, Preset::Balanced, Preset::Performance, Preset::FullSpeed, Preset::AiAuto, Preset::Bios];

    /// User-facing mode name ("Auto (AI)", not the stored profile name "AI Auto").
    pub fn label(self) -> &'static str {
        match self {
            Preset::Silent => "Silent",
            Preset::Balanced => "Balanced",
            Preset::Performance => "Performance",
            Preset::FullSpeed => "Full speed",
            Preset::AiAuto => "Auto (AI)",
            Preset::Bios => "BIOS",
            Preset::Custom => "Custom",
        }
    }

    /// One-word descriptor under the mode tile.
    pub fn descriptor(self) -> &'static str {
        match self {
            Preset::Silent => "Quietest",
            Preset::Balanced => "Everyday",
            Preset::Performance => "Coolest",
            Preset::FullSpeed => "Always 100 %",
            Preset::AiAuto => "Learns",
            Preset::Bios => "Board curve",
            Preset::Custom => "Your settings",
        }
    }

    /// Name of the profile [`Config::apply_preset`] creates when the user deleted the built-in one
    /// (the names `Config::default` uses).
    pub fn profile_name(self) -> &'static str {
        match self {
            Preset::Bios => "BIOS (Smart Fan)",
            Preset::AiAuto => "AI Auto",
            Preset::FullSpeed => "Full Speed",
            p => p.label(),
        }
    }

    /// The curve preset whose shape an AI style falls back to where the AI is not used.
    pub fn for_style(style: AiStyle) -> Preset {
        match style {
            AiStyle::Silent => Preset::Silent,
            AiStyle::Balanced => Preset::Balanced,
            AiStyle::Performance => Preset::Performance,
        }
    }
}

/// The active fan mode as the mode tiles / tray show it.
#[derive(Clone, Debug, PartialEq)]
pub struct FanModeView {
    /// Preset of the active profile (`Custom` for a user profile).
    pub preset: Preset,
    pub profile: usize,
    /// Stored profile name ("AI Auto", "My quiet one").
    pub name: String,
    /// Display label: the preset's ("Auto (AI)") or the custom profile's name.
    pub label: String,
    /// The profile stores per-fan changes on top of its preset ("Edited"; ignored under BIOS).
    pub edited: bool,
}

/// What the user (or the header wizard) says is connected to a fan header. It overrides the board
/// table's guess (`FanChannelDesc::role`), see [`effective_role`].
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, Hash)]
pub enum HeaderRole {
    /// Fans of a CPU air cooler.
    CpuFans,
    /// Fans on an AIO radiator (they cool the CPU like CPU fans).
    RadiatorFans,
    /// Case intake / exhaust fans, directly or through a hub.
    CaseFans,
    /// An AIO (or custom-loop) pump.
    Pump,
    /// Something is connected but what it is was not confirmed: conservative 60 % floor.
    Unknown,
    /// Nothing is connected: the header stays with the BIOS and is never driven by a mode. Safety
    /// still counts it while it may be the CPU cooler (present, CPU-chain board role): a header
    /// without a speed signal stays in the FRG-E305 / FRG-E309 watch, and with the critical latch
    /// or a cooling alarm it goes to 100 % like the rest of the CPU chain.
    Unused,
}

/// Lowest duty a header of unknown role gets in every preset except BIOS:
/// a radiator fan at 60 % is noise, a pump at 28 % is a slow-motion overheat.
pub const UNKNOWN_MIN_DUTY: f32 = 60.0;

impl HeaderRole {
    pub const ALL: [HeaderRole; 6] = [
        HeaderRole::CpuFans,
        HeaderRole::RadiatorFans,
        HeaderRole::CaseFans,
        HeaderRole::Pump,
        HeaderRole::Unknown,
        HeaderRole::Unused,
    ];

    pub fn label(self) -> &'static str {
        match self {
            HeaderRole::CpuFans => "CPU fans",
            HeaderRole::RadiatorFans => "Radiator fans",
            HeaderRole::CaseFans => "Case fans",
            HeaderRole::Pump => "AIO pump",
            HeaderRole::Unknown => "Unknown device",
            HeaderRole::Unused => "Unused",
        }
    }

    /// Friendly fan name a role assignment gives a header the user has not named.
    pub fn default_name(self) -> Option<&'static str> {
        match self {
            HeaderRole::Unknown | HeaderRole::Unused => None,
            r => Some(r.label()),
        }
    }

    /// How the engine treats the header (`None`: never driven).
    pub fn fan_role(self) -> Option<FanRole> {
        match self {
            HeaderRole::CpuFans | HeaderRole::RadiatorFans => Some(FanRole::Cpu),
            HeaderRole::CaseFans => Some(FanRole::System),
            HeaderRole::Pump => Some(FanRole::Pump),
            HeaderRole::Unknown => Some(FanRole::Unknown),
            HeaderRole::Unused => None,
        }
    }
}

/// The role the engine uses for a header: the user's assignment, else the board table's guess,
/// except that a present header without a speed signal whose role nobody confirmed is `Unknown`
/// (it could be the pump). A board "PUMP" header keeps its pump role
/// (the pump rules are stricter than the unknown ones). `Unused` keeps the board role (the header
/// is not driven by a mode, see [`Config::header_unused`]; the board role decides whether the
/// cooling-failure watch still counts it).
pub fn effective_role(
    assigned: Option<HeaderRole>,
    desc: &crate::model::FanChannelDesc,
    speed_signal: bool,
) -> FanRole {
    match assigned.and_then(HeaderRole::fan_role) {
        Some(r) => r,
        None if assigned.is_none() && desc.present && !speed_signal && desc.role != FanRole::Pump => FanRole::Unknown,
        None => desc.role,
    }
}

/// Why a header without a speed signal does not run under the AI (also shown by the UI).
pub const NO_SPEED_SIGNAL_AI: &str = "AI needs a fan with a speed (tach) signal on this header: it learns from \
     the measured airflow. This header runs the matching curve instead.";
/// Why a pump never runs under the AI.
pub const PUMP_AI: &str = "Pumps follow the CPU temperature on their own curve in the fan chip, never the AI \
     (without a speed signal there is nothing for it to learn from).";
/// Why a header of unknown role never runs under the AI.
pub const UNKNOWN_AI: &str = "AI is not used until you confirm what is connected to this header \
     (Identify headers). It runs the matching curve with a 60 % minimum meanwhile.";

/// The settings the engine runs for one header: the profile's entry with the role's safety rules
/// applied.
#[derive(Clone, Debug, PartialEq)]
pub struct HeaderPlan {
    pub fc: FanConfig,
    /// The profile asks for AI here, but the header can't use it (reason; the UI shows it).
    pub ai_refused: Option<&'static str>,
    /// Role floor in effect (pump / unknown role), `None` under BIOS.
    pub floor: Option<f32>,
    /// Floor from the board's own CPU / PUMP label, whatever role the settings give the header
    /// ([`HeaderPlan::with_board_floor`]); `None` under BIOS and on other headers.
    pub board_floor: Option<f32>,
}

/// Lowest duty a header the board itself labels CPU (CPU_FAN, CPU_OPT) gets in every mode except
/// BIOS, with no fan stop, whatever role the settings file gives it (audit FR-PRIV-005).
pub const CPU_HEADER_MIN_DUTY: f32 = 20.0;

impl HeaderPlan {
    /// * Pump: never AI (the Balanced pump curve instead), never below [`PUMP_MIN_DUTY`].
    /// * Unknown role: never AI (the style's curve instead), never below [`UNKNOWN_MIN_DUTY`].
    /// * No speed signal: never AI (the style's curve instead).
    ///
    /// BIOS mode stays BIOS. The floors also bind user curves and fixed duties.
    pub fn new(mut fc: FanConfig, role: FanRole, speed_signal: bool) -> HeaderPlan {
        if fc.mode == FanMode::Bios {
            return HeaderPlan { fc, ai_refused: None, floor: None, board_floor: None };
        }
        let mut ai_refused = None;
        if fc.mode == FanMode::Ai {
            let why = match role {
                FanRole::Pump => Some(PUMP_AI),
                FanRole::Unknown => Some(UNKNOWN_AI),
                _ if !speed_signal => Some(NO_SPEED_SIGNAL_AI),
                _ => None,
            };
            if let Some(why) = why {
                fc = if role == FanRole::Pump {
                    FanConfig::preset(Preset::AiAuto, role)
                } else {
                    let mut c = FanConfig::preset(Preset::for_style(fc.ai.style), role);
                    c.min_duty = c.min_duty.max(fc.min_duty);
                    c.max_duty = fc.max_duty.max(c.min_duty);
                    c
                };
                ai_refused = Some(why);
            }
        }
        let floor = match role {
            FanRole::Pump => Some(PUMP_MIN_DUTY),
            FanRole::Unknown => Some(UNKNOWN_MIN_DUTY),
            FanRole::Cpu | FanRole::System => None,
        };
        if let Some(f) = floor {
            fc.min_duty = fc.min_duty.max(f);
            fc.max_duty = fc.max_duty.max(fc.min_duty);
            fc.fixed_duty = fc.fixed_duty.max(f);
            fc.allow_stop = false;
        }
        HeaderPlan { fc, ai_refused, floor, board_floor: None }
    }

    /// Adds the floor of the board's own label (audit FR-PRIV-005): a PUMP header keeps
    /// [`PUMP_MIN_DUTY`] and a CPU header [`CPU_HEADER_MIN_DUTY`], with no fan stop, even when the
    /// settings give the header another role. `config.json` can be written by any program of the
    /// user, so the role it holds must not take the floor away from the header the board says
    /// carries the CPU cooler or the pump. Calibration and the Identify wizard keep their own
    /// rules (the engine applies this floor in normal running only). BIOS stays BIOS.
    pub fn with_board_floor(mut self, board_role: FanRole) -> HeaderPlan {
        if self.fc.mode == FanMode::Bios {
            return self;
        }
        let f = match board_role {
            FanRole::Pump => PUMP_MIN_DUTY,
            FanRole::Cpu => CPU_HEADER_MIN_DUTY,
            FanRole::System | FanRole::Unknown => return self,
        };
        self.fc.min_duty = self.fc.min_duty.max(f);
        self.fc.max_duty = self.fc.max_duty.max(self.fc.min_duty);
        self.fc.fixed_duty = self.fc.fixed_duty.max(f);
        self.fc.allow_stop = false;
        self.board_floor = Some(f);
        self
    }

    /// The lowest duty normal running may give this header: the role floor and the board floor.
    pub fn running_floor(&self) -> f32 {
        self.floor.unwrap_or(0.0).max(self.board_floor.unwrap_or(0.0))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Profile {
    pub name: String,
    pub preset: Preset,
    #[serde(deserialize_with = "lenient::map")]
    pub fans: BTreeMap<String, FanConfig>,
}

impl Default for Profile {
    fn default() -> Self {
        Profile::new("Custom", Preset::Custom)
    }
}

impl Profile {
    pub fn new(name: &str, preset: Preset) -> Self {
        Profile { name: name.into(), preset, fans: BTreeMap::new() }
    }

    /// Settings for `fan_id`, creating preset defaults on first use.
    pub fn fan_mut(&mut self, fan_id: &str, role: FanRole) -> &mut FanConfig {
        let preset = self.preset;
        self.fans.entry(fan_id.to_string()).or_insert_with(|| FanConfig::preset(preset, role))
    }

    /// The effective settings for `fan_id`. The BIOS preset is an invariant: every fan is left to the
    /// BIOS whatever per-fan overrides the profile stores (they stay on disk untouched; "Reset to
    /// preset" removes them), so "BIOS (Smart Fan)" always means "nothing is driven".
    pub fn fan(&self, fan_id: &str, role: FanRole) -> FanConfig {
        if self.preset == Preset::Bios {
            return FanConfig::preset(Preset::Bios, role);
        }
        self.fans.get(fan_id).cloned().unwrap_or_else(|| FanConfig::preset(self.preset, role))
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub enum FanMode {
    Bios,
    Fixed,
    Curve,
    Ai,
}

impl FanMode {
    pub fn label(self) -> &'static str {
        match self {
            FanMode::Bios => "BIOS",
            FanMode::Fixed => "Fixed",
            FanMode::Curve => "Curve",
            FanMode::Ai => "AI",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub enum Combine {
    Max,
    Average,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TempSource {
    pub sensors: Vec<String>,
    pub combine: Combine,
}

impl Default for TempSource {
    fn default() -> Self {
        TempSource { sensors: vec![ids::CPU_TEMP.into()], combine: Combine::Max }
    }
}

impl TempSource {
    pub fn key(&self) -> String {
        let mut s = self.sensors.clone();
        s.sort();
        format!("{:?}:{}", self.combine, s.join(","))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CurveConfig {
    #[serde(deserialize_with = "lenient::field")]
    pub source: TempSource,
    /// (temperature °C, duty %) sorted by temperature.
    #[serde(deserialize_with = "lenient::field")]
    pub points: Vec<[f32; 2]>,
    pub hysteresis: f32,
    /// Temperature smoothing time constant in seconds.
    pub response_s: f32,
}

impl Default for CurveConfig {
    fn default() -> Self {
        CurveConfig {
            source: TempSource::default(),
            points: vec![[40.0, 30.0], [60.0, 45.0], [75.0, 70.0], [85.0, 100.0]],
            hysteresis: 2.0,
            response_s: 3.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub enum AiStyle {
    Silent,
    Balanced,
    Performance,
}

impl AiStyle {
    pub fn label(self) -> &'static str {
        match self {
            AiStyle::Silent => "Silent",
            AiStyle::Balanced => "Balanced",
            AiStyle::Performance => "Performance",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AiConfig {
    #[serde(deserialize_with = "lenient::field")]
    pub source: TempSource,
    pub style: AiStyle,
    /// Temperature the AI tries not to exceed while keeping fans as slow as possible.
    pub target: f32,
    /// Hard ceiling: fans go to 100 % at or above this temperature.
    pub limit: f32,
}

impl Default for AiConfig {
    fn default() -> Self {
        AiConfig { source: TempSource::default(), style: AiStyle::Balanced, target: 75.0, limit: 88.0 }
    }
}

impl AiConfig {
    pub fn for_role(style: AiStyle, role: FanRole) -> Self {
        match role {
            FanRole::Cpu | FanRole::Pump | FanRole::Unknown => AiConfig {
                source: TempSource::default(),
                style,
                target: match style {
                    AiStyle::Silent => 80.0,
                    AiStyle::Balanced => 74.0,
                    AiStyle::Performance => 66.0,
                },
                limit: match style {
                    AiStyle::Silent => 90.0,
                    AiStyle::Balanced => 88.0,
                    AiStyle::Performance => 84.0,
                },
            },
            FanRole::System => AiConfig {
                source: TempSource { sensors: vec![ids::CPU_TEMP.into(), ids::GPU_TEMP.into()], combine: Combine::Max },
                style,
                target: match style {
                    AiStyle::Silent => 86.0,
                    AiStyle::Balanced => 80.0,
                    AiStyle::Performance => 72.0,
                },
                limit: match style {
                    AiStyle::Silent => 92.0,
                    AiStyle::Balanced => 90.0,
                    AiStyle::Performance => 86.0,
                },
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FanConfig {
    pub mode: FanMode,
    pub fixed_duty: f32,
    #[serde(deserialize_with = "lenient::field")]
    pub curve: CurveConfig,
    #[serde(deserialize_with = "lenient::field")]
    pub ai: AiConfig,
    pub min_duty: f32,
    pub max_duty: f32,
    /// Allow 0 % (fan stop) when the requested duty falls below `min_duty`.
    pub allow_stop: bool,
    /// Maximum change rates in %/s.
    pub ramp_up: f32,
    pub ramp_down: f32,
}

impl Default for FanConfig {
    fn default() -> Self {
        FanConfig::preset(Preset::Balanced, FanRole::System)
    }
}

/// Lowest duty a pump-role header ever gets outside BIOS mode, user curves and fixed duties included.
/// Above the 20-25 % where PWM pumps can stall; with no speed signal a stall would go unseen. The
/// reference AIO (no tach) ran for hours at about 25 % under 0.1 without trouble, so 30 % is margin.
pub const PUMP_MIN_DUTY: f32 = 30.0;

impl FanConfig {
    /// Preset defaults per role. Curves follow the CPU temperature, so the
    /// chip runs them (CPU input, hysteresis as the chip's ΔT: Silent 3, Balanced 2, Performance
    /// 1 °C). Case fans use `max(CPU, GPU)`: the engine runs the CPU part in the chip and lifts its
    /// floor from software for the GPU part. An unknown-role header gets the CPU curves with a
    /// [`UNKNOWN_MIN_DUTY`] floor and never the AI.
    pub fn preset(preset: Preset, role: FanRole) -> Self {
        if role == FanRole::Pump {
            return Self::pump_preset(preset);
        }
        let cpu = matches!(role, FanRole::Cpu | FanRole::Unknown);
        let source = if cpu {
            TempSource::default()
        } else {
            TempSource { sensors: vec![ids::CPU_TEMP.into(), ids::GPU_TEMP.into()], combine: Combine::Max }
        };
        let (points, hysteresis, response_s, ramp_up, ramp_down): (Vec<[f32; 2]>, f32, f32, f32, f32) =
            match (preset, cpu) {
                (Preset::Silent, true) => {
                    (vec![[40.0, 22.0], [55.0, 30.0], [70.0, 48.0], [80.0, 72.0], [88.0, 100.0]], 3.0, 6.0, 8.0, 2.0)
                }
                (Preset::Silent, false) => {
                    (vec![[45.0, 22.0], [60.0, 30.0], [75.0, 50.0], [85.0, 75.0], [92.0, 100.0]], 3.0, 8.0, 6.0, 2.0)
                }
                (Preset::Performance, true) => {
                    (vec![[35.0, 40.0], [50.0, 52.0], [65.0, 72.0], [75.0, 90.0], [80.0, 100.0]], 1.0, 2.0, 20.0, 6.0)
                }
                (Preset::Performance, false) => {
                    (vec![[35.0, 40.0], [50.0, 55.0], [65.0, 75.0], [75.0, 95.0], [80.0, 100.0]], 1.0, 3.0, 15.0, 5.0)
                }
                (_, true) => {
                    (vec![[40.0, 28.0], [55.0, 38.0], [68.0, 55.0], [78.0, 80.0], [86.0, 100.0]], 2.0, 4.0, 12.0, 3.0)
                }
                (_, false) => {
                    (vec![[40.0, 30.0], [55.0, 38.0], [70.0, 60.0], [80.0, 85.0], [88.0, 100.0]], 2.0, 5.0, 10.0, 3.0)
                }
            };
        let style = match preset {
            Preset::Silent => AiStyle::Silent,
            Preset::Performance => AiStyle::Performance,
            _ => AiStyle::Balanced,
        };
        let unknown = role == FanRole::Unknown;
        FanConfig {
            // (AI for an unknown-role header is turned into a curve by `HeaderPlan`, with the reason.)
            mode: match preset {
                Preset::Bios => FanMode::Bios,
                Preset::FullSpeed => FanMode::Fixed,
                Preset::AiAuto => FanMode::Ai,
                Preset::Silent | Preset::Balanced | Preset::Performance | Preset::Custom => FanMode::Curve,
            },
            fixed_duty: match preset {
                Preset::FullSpeed => 100.0,
                _ if unknown => UNKNOWN_MIN_DUTY,
                _ => 50.0,
            },
            curve: CurveConfig { source, points, hysteresis, response_s },
            ai: AiConfig::for_role(style, role),
            min_duty: if unknown { UNKNOWN_MIN_DUTY } else { 20.0 },
            max_duty: 100.0,
            allow_stop: false,
            ramp_up,
            ramp_down,
        }
    }

    /// Pump headers get their own CPU-temperature curves, run by the chip like the radiator fans:
    /// quiet and flat through idle and light load (so Tctl spikes don't change the pump's pitch),
    /// steep under sustained load, never below [`PUMP_MIN_DUTY`]. The chip keeps running the curve
    /// and its 100 % failsafe if FanRGB stops, so flow always rises with the CPU temperature. Never
    /// the AI (Auto uses the Balanced pump curve). Full speed is a flat 100 %; BIOS releases it.
    /// (0.2.0 ran pumps flat at 70/85/100 %: louder than the BIOS's own ~60 % at idle.)
    fn pump_preset(preset: Preset) -> Self {
        let (points, hysteresis) = match preset {
            Preset::Silent => (vec![[60.0, 35.0], [70.0, 50.0], [80.0, 80.0], [86.0, 100.0]], 3.0),
            Preset::Performance => (vec![[45.0, 65.0], [65.0, 85.0], [75.0, 100.0]], 1.0),
            _ => (vec![[55.0, 45.0], [68.0, 60.0], [78.0, 85.0], [84.0, 100.0]], 2.0),
        };
        FanConfig {
            mode: match preset {
                Preset::Bios => FanMode::Bios,
                Preset::FullSpeed => FanMode::Fixed,
                _ => FanMode::Curve,
            },
            fixed_duty: if preset == Preset::FullSpeed { 100.0 } else { 50.0 },
            curve: CurveConfig { source: TempSource::default(), points, hysteresis, response_s: 4.0 },
            ai: AiConfig::for_role(AiStyle::Balanced, FanRole::Pump),
            min_duty: PUMP_MIN_DUTY,
            max_duty: 100.0,
            allow_stop: false,
            ramp_up: 20.0,
            ramp_down: 3.0,
        }
    }
}

fn fin(v: f32, default: f32) -> f32 {
    if v.is_finite() { v } else { default }
}

impl TempSource {
    fn clamp_safety(&mut self) {
        self.sensors.retain(|s| !s.is_empty() && s.len() <= 64);
        self.sensors.truncate(8);
        if self.sensors.is_empty() {
            self.sensors = TempSource::default().sensors;
        }
    }
}

impl FanConfig {
    /// Clamps every per-fan value (unbounded `response_s`, hysteresis, ramps and AI
    /// limits could freeze or disable cooling).
    pub fn clamp_safety(&mut self) {
        self.min_duty = fin(self.min_duty, 20.0).clamp(0.0, 100.0);
        self.max_duty = fin(self.max_duty, 100.0).clamp(self.min_duty.max(20.0), 100.0);
        self.fixed_duty = fin(self.fixed_duty, 50.0).clamp(0.0, 100.0);
        self.ramp_up = fin(self.ramp_up, 10.0).clamp(1.0, 100.0);
        self.ramp_down = fin(self.ramp_down, 3.0).clamp(0.5, 100.0);
        let c = &mut self.curve;
        c.hysteresis = fin(c.hysteresis, 2.0).clamp(0.0, 15.0);
        c.response_s = fin(c.response_s, 3.0).clamp(0.0, 60.0);
        c.points.retain(|p| p[0].is_finite() && p[1].is_finite());
        for p in &mut c.points {
            p[0] = p[0].clamp(0.0, 120.0);
            p[1] = p[1].clamp(0.0, 100.0);
        }
        c.points.sort_by(|a, b| a[0].total_cmp(&b[0]));
        c.points.truncate(16);
        if c.points.is_empty() {
            c.points = CurveConfig::default().points;
        }
        c.source.clamp_safety();
        let a = &mut self.ai;
        a.target = fin(a.target, 75.0).clamp(40.0, 92.0);
        a.limit = fin(a.limit, 88.0).clamp(a.target + 3.0, 95.0);
        a.source.clamp_safety();
    }
}

/// Message of the last failed config load (shown as `ErrorCode::ConfigLoad`).
static LOAD_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub fn set_load_error(msg: Option<String>) {
    if let Ok(mut e) = LOAD_ERROR.lock() {
        *e = msg;
    }
}

pub fn load_error() -> Option<String> {
    LOAD_ERROR.lock().ok().and_then(|e| e.clone())
}

/// Measured fan characteristics from the calibration wizard.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct FanCalibration {
    /// (duty %, rpm) samples, descending duty.
    pub points: Vec<[f32; 2]>,
    pub max_rpm: f32,
    /// Lowest duty that keeps the fan spinning when ramping down.
    pub stop_duty: Option<f32>,
    /// Duty needed to start a stopped fan.
    pub start_duty: Option<f32>,
}

impl FanCalibration {
    /// The samples by ascending duty, each RPM lowered to the lowest RPM measured at any higher duty
    /// (audit FR-CTL-002). For a fan that follows its duty this changes nothing (tach noise aside);
    /// for a table it did not follow, FanRGB can then only assume *less* airflow at a duty, never
    /// more, so a bad table can only raise duties.
    fn monotone_points(&self) -> Vec<[f32; 2]> {
        let mut pts = self.points.clone();
        pts.sort_by(|a, b| a[0].total_cmp(&b[0]));
        let mut lowest_above = f32::INFINITY;
        for p in pts.iter_mut().rev() {
            lowest_above = lowest_above.min(p[1]);
            p[1] = lowest_above;
        }
        // Above the last sample the fan is assumed no faster, up to 100 %: full airflow (the top
        // speed, an RPM tie) then maps to 100 % duty, never to a table that stops short of it.
        if let Some(&[d, r]) = pts.last()
            && d < 100.0
        {
            pts.push([100.0, r]);
        }
        pts
    }

    /// Expected RPM at `duty` (linear interpolation over [`Self::monotone_points`]).
    pub fn rpm_at(&self, duty: f32) -> Option<f32> {
        if self.points.len() < 2 {
            return None;
        }
        Some(crate::control::curve::interpolate(&self.monotone_points(), duty))
    }

    /// Smallest duty that reaches at least `fraction` of max RPM. "Max RPM" is the larger of
    /// `max_rpm` and the table's own top speed, so full airflow always maps to the top of the table
    /// (a `max_rpm` below the measured speeds would otherwise map it to a low duty).
    pub fn duty_for_airflow(&self, fraction: f32) -> Option<f32> {
        if self.points.len() < 2 || self.max_rpm <= 0.0 {
            return None;
        }
        let mono = self.monotone_points();
        let top = mono.last().map_or(0.0, |p| p[1]);
        let scale = self.max_rpm.max(top);
        if !(scale.is_finite() && scale > 0.0) {
            return None;
        }
        let want = fraction.clamp(0.0, 1.0) * scale;
        let mut pts: Vec<[f32; 2]> = mono.iter().map(|p| [p[1], p[0]]).collect();
        // By RPM, and for the same RPM the highest duty first: `dedup_by` keeps the first of a run,
        // so a plateau maps to the duty that surely reaches it, never the lowest (audit FR-CTL-002).
        pts.sort_by(|a, b| a[0].total_cmp(&b[0]).then(b[1].total_cmp(&a[1])));
        pts.dedup_by(|a, b| (a[0] - b[0]).abs() < 1.0);
        Some(crate::control::curve::interpolate(&pts, want))
    }

    /// Whether a stored table is a plausible measurement (audit FR-CTL-002). The table comes from
    /// `config.json` and feeds the AI's airflow map, its safety floor and the stall floor, so a
    /// non-finite, out-of-range or inverted one (RPM falling as the duty rises), a flat one (the fan
    /// did not follow the duty, which the wizard itself refuses) or one whose `max_rpm` disagrees with
    /// its own measurements is never used: the built-in fan model applies instead. Tables with fewer
    /// than two points stay valid (they only carry `max_rpm`).
    pub fn sane(&self) -> bool {
        let within = |v: f32, lo: f32, hi: f32| v.is_finite() && (lo..=hi).contains(&v);
        self.points.len() <= 32
            && within(self.max_rpm, 0.0, 25_000.0)
            && self.points.iter().all(|p| within(p[0], 0.0, 100.0) && within(p[1], 0.0, 25_000.0))
            && self.stop_duty.is_none_or(|d| within(d, 0.0, 100.0))
            && self.start_duty.is_none_or(|d| within(d, 0.0, 100.0))
            && crate::control::calibrate::follows_duty(&self.points)
            && crate::control::calibrate::range_plausible(&self.points, self.max_rpm)
    }
}

// ---------------------------------------------------------------------------------------
// RGB

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub enum EffectKind {
    Off,
    Static,
    Breathing,
    Spectrum,
    Rainbow,
    Gradient,
    Comet,
    Chase,
    Twinkle,
    Fire,
    Temperature,
    Load,
    Music,
    Strobe,
    HwStatic,
    HwBreathing,
    HwFlash,
    HwDoubleFlash,
    HwColorCycle,
    HwWave,
    HwRandom,
}

impl EffectKind {
    pub const SOFTWARE: [EffectKind; 14] = [
        EffectKind::Off,
        EffectKind::Static,
        EffectKind::Breathing,
        EffectKind::Spectrum,
        EffectKind::Rainbow,
        EffectKind::Gradient,
        EffectKind::Comet,
        EffectKind::Chase,
        EffectKind::Twinkle,
        EffectKind::Fire,
        EffectKind::Temperature,
        EffectKind::Load,
        EffectKind::Music,
        EffectKind::Strobe,
    ];
    pub const HARDWARE: [EffectKind; 7] = [
        EffectKind::HwStatic,
        EffectKind::HwBreathing,
        EffectKind::HwFlash,
        EffectKind::HwDoubleFlash,
        EffectKind::HwColorCycle,
        EffectKind::HwWave,
        EffectKind::HwRandom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            EffectKind::Off => "Off",
            EffectKind::Static => "Static",
            EffectKind::Breathing => "Breathing",
            EffectKind::Spectrum => "Spectrum cycle",
            EffectKind::Rainbow => "Rainbow wave",
            EffectKind::Gradient => "Gradient",
            EffectKind::Comet => "Comet",
            EffectKind::Chase => "Theater chase",
            EffectKind::Twinkle => "Twinkle",
            EffectKind::Fire => "Fire",
            EffectKind::Temperature => "Temperature reactive",
            EffectKind::Load => "Load reactive",
            EffectKind::Music => "Music visualizer",
            EffectKind::Strobe => "Strobe",
            EffectKind::HwStatic => "Static (hardware)",
            EffectKind::HwBreathing => "Breathing (hardware)",
            EffectKind::HwFlash => "Flash (hardware)",
            EffectKind::HwDoubleFlash => "Double flash (hardware)",
            EffectKind::HwColorCycle => "Color cycle (hardware)",
            EffectKind::HwWave => "Wave (hardware)",
            EffectKind::HwRandom => "Random (hardware)",
        }
    }

    pub fn is_hardware(self) -> bool {
        EffectKind::HARDWARE.contains(&self)
    }

    pub fn is_animated(self) -> bool {
        EffectKind::SOFTWARE.contains(&self) && !matches!(self, EffectKind::Off | EffectKind::Static)
    }

    pub fn uses_colors(self) -> usize {
        match self {
            EffectKind::Static
            | EffectKind::Breathing
            | EffectKind::Comet
            | EffectKind::Chase
            | EffectKind::Strobe
            | EffectKind::HwStatic
            | EffectKind::HwBreathing
            | EffectKind::HwFlash
            | EffectKind::HwDoubleFlash => 1,
            EffectKind::Gradient | EffectKind::Twinkle | EffectKind::Music => 2,
            EffectKind::Temperature | EffectKind::Load => 3,
            _ => 0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Effect {
    pub kind: EffectKind,
    #[serde(deserialize_with = "lenient::field")]
    pub colors: Vec<[u8; 3]>,
    /// 0 (slow) ..= 1 (fast)
    pub speed: f32,
    pub brightness: f32,
    pub reverse: bool,
    /// Sensor for reactive effects.
    pub sensor: String,
    pub low: f32,
    pub high: f32,
}

impl Default for Effect {
    fn default() -> Self {
        Effect {
            kind: EffectKind::Rainbow,
            colors: vec![[0, 200, 255], [170, 0, 255], [255, 40, 0]],
            speed: 0.4,
            brightness: 1.0,
            reverse: false,
            sensor: ids::CPU_TEMP.into(),
            low: 40.0,
            high: 85.0,
        }
    }
}

impl Effect {
    pub fn color(&self, i: usize) -> [u8; 3] {
        self.colors.get(i).copied().unwrap_or([255, 255, 255])
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub enum ColorOrder {
    /// Use the calibration stored in the controller.
    Auto,
    Rgb,
    Rbg,
    Grb,
    Gbr,
    Brg,
    Bgr,
}

impl ColorOrder {
    pub const ALL: [ColorOrder; 7] = [
        ColorOrder::Auto,
        ColorOrder::Rgb,
        ColorOrder::Rbg,
        ColorOrder::Grb,
        ColorOrder::Gbr,
        ColorOrder::Brg,
        ColorOrder::Bgr,
    ];
    pub fn label(self) -> &'static str {
        match self {
            ColorOrder::Auto => "Auto (controller)",
            ColorOrder::Rgb => "RGB",
            ColorOrder::Rbg => "RBG",
            ColorOrder::Grb => "GRB",
            ColorOrder::Gbr => "GBR",
            ColorOrder::Brg => "BRG",
            ColorOrder::Bgr => "BGR",
        }
    }
}

/// Screen colour → LED drive correction of one zone, applied only to the bytes sent to the
/// controller (never to previews). Picker colours are sRGB-encoded; LED PWM is linear light, so
/// without the gamma step every mixed colour looks washed out on the LEDs.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ColorCorrection {
    /// Transfer exponent: 1.0 = raw bytes (GCC / OpenRGB behaviour), 2.2 ≈ sRGB.
    pub gamma: f32,
    /// White balance per channel (R, G, B), applied after the gamma step.
    pub gain: [f32; 3],
}

pub const GAMMA_RANGE: std::ops::RangeInclusive<f32> = 1.0..=3.0;
pub const GAIN_RANGE: std::ops::RangeInclusive<f32> = 0.05..=1.0;

impl Default for ColorCorrection {
    fn default() -> Self {
        ColorCorrection { gamma: 2.2, gain: [1.0, 1.0, 1.0] }
    }
}

impl ColorCorrection {
    /// Clamps into the supported ranges (NaN → default).
    pub fn sanitize(&mut self) {
        let d = ColorCorrection::default();
        self.gamma = fin(self.gamma, d.gamma).clamp(*GAMMA_RANGE.start(), *GAMMA_RANGE.end());
        for g in &mut self.gain {
            *g = fin(*g, 1.0).clamp(*GAIN_RANGE.start(), *GAIN_RANGE.end());
        }
    }
}

/// Most segments one ARGB header can carry (devices daisy-chained on one header).
pub const MAX_SEGMENTS: usize = 8;
/// Most LEDs one ARGB header addresses. The one bound for segment tables everywhere (config load,
/// calibration test, calibration save); the header's configured `led_count` is applied when the
/// frame is built, so a segment past it is kept (and lights once the count is raised).
pub const MAX_ZONE_LEDS: u16 = 1024;
/// Longest segment name kept.
pub const SEGMENT_NAME_MAX: usize = 40;

/// A run of LEDs on one ARGB header that belongs to one device (for example the AIO pump head
/// chained behind the case fans), with its own white balance on top of the zone's correction.
/// Only streamed (direct-mode) output can apply it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Segment {
    pub name: String,
    /// First LED (0-based, inclusive).
    pub from_led: u16,
    /// End LED (0-based, exclusive): the segment is `from_led..to_led`.
    pub to_led: u16,
    /// Per-channel gain (R, G, B) multiplied with the zone's gain, in linear LED drive.
    pub gain: [f32; 3],
}

impl Default for Segment {
    fn default() -> Self {
        Segment { name: String::new(), from_led: 0, to_led: 0, gain: [1.0, 1.0, 1.0] }
    }
}

impl Segment {
    pub fn contains(&self, led: usize) -> bool {
        (self.from_led as usize..self.to_led as usize).contains(&led)
    }

    /// Any gain below 1 (the segment changes the bytes).
    pub fn is_active(&self) -> bool {
        self.gain.iter().any(|g| *g < 1.0)
    }
}

/// Cleans a segment table: gains clamped, names trimmed, ranges clamped to `max_leds`, empty and
/// overlapping segments dropped (the earlier one wins), sorted by first LED, at most
/// [`MAX_SEGMENTS`].
pub fn sanitize_segments(segs: &mut Vec<Segment>, max_leds: u16) {
    for s in segs.iter_mut() {
        for g in &mut s.gain {
            *g = fin(*g, 1.0).clamp(*GAIN_RANGE.start(), *GAIN_RANGE.end());
        }
        s.name = s.name.trim().chars().take(SEGMENT_NAME_MAX).collect();
        s.from_led = s.from_led.min(max_leds);
        s.to_led = s.to_led.min(max_leds);
    }
    let mut kept: Vec<Segment> = Vec::new();
    for s in segs.drain(..) {
        if s.from_led < s.to_led && !kept.iter().any(|k| s.from_led < k.to_led && k.from_led < s.to_led) {
            kept.push(s);
        }
    }
    kept.sort_by_key(|s| s.from_led);
    kept.truncate(MAX_SEGMENTS);
    *segs = kept;
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ZoneConfig {
    pub enabled: bool,
    pub name: Option<String>,
    #[serde(deserialize_with = "lenient::field")]
    pub effect: Effect,
    /// LEDs on an addressable header (ARGB).
    pub led_count: u16,
    pub color_order: ColorOrder,
    /// Gamma / white balance for what this zone's LEDs receive.
    #[serde(deserialize_with = "lenient::field")]
    pub correction: ColorCorrection,
    /// Devices chained on this header with their own white balance (ARGB zones only).
    #[serde(deserialize_with = "lenient::vec")]
    pub segments: Vec<Segment>,
}

impl Default for ZoneConfig {
    fn default() -> Self {
        ZoneConfig {
            enabled: true,
            name: None,
            effect: Effect::default(),
            led_count: 30,
            color_order: ColorOrder::Auto,
            correction: ColorCorrection::default(),
            segments: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub enum ExitLighting {
    /// Leave the last frame on the controller.
    KeepLast,
    /// Turn all LEDs off.
    Off,
    /// Hand over to a hardware rainbow so lighting stays alive without the app.
    HardwareCycle,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RgbConfig {
    pub enabled: bool,
    pub brightness: f32,
    pub sync: bool,
    #[serde(deserialize_with = "lenient::field")]
    pub global: Effect,
    #[serde(deserialize_with = "lenient::map")]
    pub zones: BTreeMap<String, ZoneConfig>,
    pub fps: u32,
    pub on_exit: ExitLighting,
    pub temp_alert: bool,
    pub temp_alert_c: f32,
    pub show_all_zones: bool,
    /// Controller division of the 12 V LED_C header: 1 or 4 (depends on the board
    /// revision). `None` drives both candidate divisions (unused ones are inert).
    #[serde(default)]
    pub ledc_slot: Option<u8>,
    /// Temporal dithering of streamed (direct-mode) frames: dark colours keep their hue below
    /// one 8-bit step, at the cost of streaming every frame. Off by default (see
    /// `rgb::color::Dither`).
    #[serde(default)]
    pub dither: bool,
}

impl Default for RgbConfig {
    fn default() -> Self {
        RgbConfig {
            enabled: true,
            brightness: 0.85,
            sync: true,
            global: Effect::default(),
            zones: BTreeMap::new(),
            fps: 40,
            on_exit: ExitLighting::HardwareCycle,
            temp_alert: true,
            temp_alert_c: 90.0,
            show_all_zones: false,
            ledc_slot: None,
            dither: false,
        }
    }
}

// ---------------------------------------------------------------------------------------

/// Parses settings leniently: returns the config and a note for every value that fell back to its
/// default. Errors only if the text is not valid JSON (or not an object).
fn parse_lenient(text: &str) -> Result<(Config, Vec<String>), serde_json::Error> {
    let _ = lenient::take_notes();
    let parsed = serde_json::from_str::<Config>(text);
    let notes = lenient::take_notes();
    parsed.map(|c| (c, notes))
}

impl Config {
    pub fn load() -> Config {
        Self::load_from(&config_path())
    }

    /// Loads and sanitizes `path`. Values this version does not understand (unknown enum values,
    /// unreadable sections) fall back to their defaults one by one and are reported via
    /// [`load_error`]. Only a file that is not JSON at all is copied to `config.json.bad` (with
    /// `fsx::write_atomic`: no renames in the user-writable folder from the elevated process) and
    /// replaced by defaults (the engine raises `ErrorCode::ConfigLoad` for both).
    pub fn load_from(path: &std::path::Path) -> Config {
        match crate::util::fsx::read_to_string_limited(path, 1 << 20) {
            Ok(text) => match parse_lenient(&text) {
                Ok((mut c, notes)) => {
                    if !notes.is_empty() {
                        let shown: Vec<&str> = notes.iter().take(3).map(|s| s.as_str()).collect();
                        let more =
                            if notes.len() > 3 { format!(" (+{} more)", notes.len() - 3) } else { String::new() };
                        set_load_error(Some(format!(
                            "Some settings in {} were not understood and were reset to defaults: {}{more}",
                            path.display(),
                            shown.join("; ")
                        )));
                    }
                    c.sanitize();
                    c
                }
                Err(e) => {
                    let bad = path.with_extension("json.bad");
                    let kept = match crate::util::fsx::write_atomic(&bad, text.as_bytes()) {
                        Ok(()) => format!("; the damaged file was kept as {}", bad.display()),
                        Err(re) => format!("; could not keep a copy ({re})"),
                    };
                    let msg = format!("{} is invalid ({e}){kept}. Defaults are in use.", path.display());
                    log::error!("[{}] {msg}", crate::error::ErrorCode::ConfigLoad);
                    set_load_error(Some(msg));
                    Config::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => {
                let msg = format!("{} could not be read ({e}). Defaults are in use.", path.display());
                log::error!("[{}] {msg}", crate::error::ErrorCode::ConfigLoad);
                set_load_error(Some(msg));
                Config::default()
            }
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&config_path())
    }

    pub fn save_to(&self, path: &std::path::Path) -> anyhow::Result<()> {
        use crate::error::{ErrorCode, WithCode};
        let text = serde_json::to_string_pretty(self).code(ErrorCode::ConfigSave)?;
        crate::util::fsx::write_atomic(path, text.as_bytes()).code(ErrorCode::ConfigSave)?;
        Ok(())
    }

    /// Full validation after loading: clamps every value into a safe, meaningful range.
    pub fn sanitize(&mut self) {
        if self.profiles.is_empty() {
            self.profiles = Config::default().profiles;
        }
        if self.active_profile >= self.profiles.len() {
            self.active_profile = 0;
        }
        self.general.poll_ms = self.general.poll_ms.clamp(250, 5000);
        self.general.cpu_critical = fin(self.general.cpu_critical, 90.0).clamp(60.0, CPU_CRITICAL_MAX);
        self.general.gpu_critical = fin(self.general.gpu_critical, 90.0).clamp(60.0, GPU_CRITICAL_MAX);
        self.rgb.fps = self.rgb.fps.clamp(5, 60);
        for z in self.rgb.zones.values_mut() {
            z.correction.sanitize();
            sanitize_segments(&mut z.segments, MAX_ZONE_LEDS);
        }
        self.fan_roles.retain(|k, _| !k.is_empty() && k.len() <= 64);
        while self.fan_roles.len() > 64 {
            self.fan_roles.pop_last();
        }
        self.clamp_safety();
    }

    /// The subset of [`Config::sanitize`] that guards against unsafe values. The engine applies it to
    /// every config snapshot, so values set by the UI or a tampered file can never disable cooling.
    /// For the critical temperatures it only clamps in the dangerous (upper) direction.
    pub fn clamp_safety(&mut self) {
        self.general.poll_ms = self.general.poll_ms.clamp(250, 5000);
        self.general.cpu_critical = fin(self.general.cpu_critical, 90.0).min(CPU_CRITICAL_MAX);
        self.general.gpu_critical = fin(self.general.gpu_critical, 90.0).min(GPU_CRITICAL_MAX);
        self.ai.gpu_tbp_w = fin(self.ai.gpu_tbp_w, 190.0).clamp(30.0, 600.0);
        self.ai.cpu_ppt_w = fin(self.ai.cpu_ppt_w, 88.0).clamp(20.0, 400.0);
        if self.profiles.is_empty() {
            self.profiles = Config::default().profiles;
        }
        if self.active_profile >= self.profiles.len() {
            self.active_profile = 0;
        }
        for p in &mut self.profiles {
            for f in p.fans.values_mut() {
                f.clamp_safety();
            }
        }
        // Stored fan tables are input like everything else in the file (audit FR-CTL-002).
        self.calibrations.retain(|id, c| !id.is_empty() && id.len() <= 64 && c.sane());
        while self.calibrations.len() > 64 {
            self.calibrations.pop_last();
        }
    }

    pub fn profile(&self) -> &Profile {
        &self.profiles[self.active_profile.min(self.profiles.len() - 1)]
    }

    pub fn profile_mut(&mut self) -> &mut Profile {
        let i = self.active_profile.min(self.profiles.len() - 1);
        &mut self.profiles[i]
    }

    // ---- fan modes ----

    /// The profile that stands for `preset`: the first one with that preset.
    pub fn preset_profile(&self, preset: Preset) -> Option<usize> {
        self.profiles.iter().position(|p| p.preset == preset)
    }

    /// Makes `preset` the active fan mode in one call: selects its profile, recreating it (with the
    /// preset's defaults) if the user deleted it. `Custom` is not a mode and changes nothing.
    /// Returns the active profile index. The engine saves the change (debounced) and applies it on
    /// its next tick.
    pub fn apply_preset(&mut self, preset: Preset) -> usize {
        if preset == Preset::Custom {
            return self.active_profile;
        }
        let i = self.preset_profile(preset).unwrap_or_else(|| {
            self.profiles.push(Profile::new(preset.profile_name(), preset));
            self.profiles.len() - 1
        });
        self.active_profile = i;
        i
    }

    /// The mode after the active one in [`Preset::MODES`] order (wraps; from a custom profile the
    /// first mode).
    pub fn next_preset(&self) -> Preset {
        let cur = self.profile().preset;
        match Preset::MODES.iter().position(|&p| p == cur) {
            Some(i) => Preset::MODES[(i + 1) % Preset::MODES.len()],
            None => Preset::MODES[0],
        }
    }

    /// Hotkey "next fan mode": applies [`Config::next_preset`] and returns it.
    pub fn cycle_preset(&mut self) -> Preset {
        let p = self.next_preset();
        self.apply_preset(p);
        p
    }

    /// The active fan mode for the mode tiles, the tray and the state line.
    pub fn fan_mode(&self) -> FanModeView {
        let p = self.profile();
        FanModeView {
            preset: p.preset,
            profile: self.active_profile.min(self.profiles.len() - 1),
            name: p.name.clone(),
            label: if p.preset == Preset::Custom { p.name.clone() } else { p.preset.label().to_string() },
            edited: p.preset != Preset::Bios && p.preset != Preset::Custom && !p.fans.is_empty(),
        }
    }

    // ---- header roles ----

    /// The role the user / wizard confirmed for `fan_id` (`None`: not confirmed).
    pub fn header_role(&self, fan_id: &str) -> Option<HeaderRole> {
        self.fan_roles.get(fan_id).copied()
    }

    /// The user marked the header as having nothing connected: no mode drives it (see
    /// [`HeaderRole::Unused`] for the safety exception).
    pub fn header_unused(&self, fan_id: &str) -> bool {
        self.header_role(fan_id) == Some(HeaderRole::Unused)
    }

    /// Assigns (or with `None` clears) the role of a header. `name` sets the friendly fan name;
    /// without one, a header that has no user name, or still carries another role's default name,
    /// gets the new role's default ("Radiator fans").
    pub fn set_fan_role(&mut self, fan_id: &str, role: Option<HeaderRole>, name: Option<&str>) {
        match role {
            Some(r) => {
                self.fan_roles.insert(fan_id.to_string(), r);
            }
            None => {
                self.fan_roles.remove(fan_id);
            }
        }
        if let Some(n) = name.map(str::trim) {
            if n.is_empty() {
                self.fan_names.remove(fan_id);
            } else {
                self.fan_names.insert(fan_id.to_string(), n.chars().take(64).collect());
            }
            return;
        }
        let generated =
            self.fan_names.get(fan_id).is_none_or(|n| HeaderRole::ALL.iter().any(|r| r.default_name() == Some(n)));
        if generated {
            match role.and_then(HeaderRole::default_name) {
                Some(n) => {
                    self.fan_names.insert(fan_id.to_string(), n.to_string());
                }
                None => {
                    self.fan_names.remove(fan_id);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// audit FR-CTL-002: a fan table that is not a plausible measurement is never used for control.
    #[test]
    fn implausible_calibrations_are_dropped() {
        let good = FanCalibration {
            points: vec![[100.0, 1800.0], [50.0, 1000.0], [0.0, 0.0]],
            max_rpm: 1800.0,
            stop_duty: Some(20.0),
            start_duty: Some(25.0),
        };
        let inverted = FanCalibration {
            points: vec![[100.0, 300.0], [50.0, 900.0], [0.0, 1800.0]],
            max_rpm: 1800.0,
            ..Default::default()
        };
        // JSON has no NaN, but 1e39 overflows f32 to infinity.
        let overflow: FanCalibration =
            serde_json::from_str(r#"{"points":[[0,1e39],[100,1e39]],"max_rpm":1e39}"#).unwrap();
        assert!(overflow.max_rpm.is_infinite());
        assert!(good.sane() && !inverted.sane() && !overflow.sane());
        assert!(!FanCalibration { stop_duty: Some(f32::NAN), ..good.clone() }.sane());
        assert!(!FanCalibration { start_duty: Some(250.0), ..good.clone() }.sane());
        assert!(FanCalibration { max_rpm: 1800.0, ..Default::default() }.sane(), "max RPM only");
        let mut c = Config::default();
        c.calibrations.insert("mb.pwm0".into(), good.clone());
        c.calibrations.insert("mb.pwm1".into(), inverted);
        c.calibrations.insert("mb.pwm2".into(), overflow);
        c.clamp_safety();
        assert_eq!(c.calibrations.keys().collect::<Vec<_>>(), ["mb.pwm0"]);
        assert_eq!(c.calibrations["mb.pwm0"], good);
    }

    fn test_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fanrgb-config-tests-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    /// One unknown enum value (renamed preset / mode / effect) only resets that value;
    /// names, calibrations, other profiles and the active profile survive.
    #[test]
    fn unknown_enum_values_only_reset_that_value() {
        let mut cfg = Config::default();
        cfg.fan_names.insert("mb.pwm1".into(), "Front intake".into());
        cfg.calibrations.insert("mb.pwm0".into(), FanCalibration { max_rpm: 1800.0, ..Default::default() });
        cfg.active_profile = 2;
        cfg.general.poll_ms = 750;
        cfg.profile_mut().fan_mut("mb.pwm0", FanRole::Cpu).fixed_duty = 42.0;
        let mut v = serde_json::to_value(&cfg).unwrap();
        v["profiles"][1]["preset"] = "Turbo".into();
        v["profiles"][2]["fans"]["mb.pwm0"]["mode"] = "Quantum".into();
        v["rgb"]["global"]["kind"] = "Laser".into();
        v["profiles"][2]["fans"]["mb.pwm0"]["curve"]["source"]["combine"] = "Median".into();
        let (got, notes) = parse_lenient(&v.to_string()).unwrap();
        assert_eq!(notes.len(), 4, "{notes:?}");
        assert_eq!(got.fan_names, cfg.fan_names);
        assert_eq!(got.calibrations, cfg.calibrations);
        assert_eq!(got.active_profile, 2);
        assert_eq!(got.general.poll_ms, 750);
        assert_eq!(got.profiles[1].preset, Preset::Custom);
        assert_eq!(got.profiles[1].name, "Silent");
        let fc = got.profiles[2].fan("mb.pwm0", FanRole::Cpu);
        assert_eq!(fc.mode, FanMode::Bios, "unknown mode: the BIOS keeps the fan");
        assert_eq!(fc.fixed_duty, 42.0, "the rest of the fan's settings is kept");
        assert_eq!(fc.curve.source.combine, Combine::Max);
        assert_eq!(got.rgb.global.kind, EffectKind::Static);
        assert_eq!(got.profiles[3], cfg.profiles[3]);
    }

    /// A wrongly typed section only resets that section / entry.
    #[test]
    fn unreadable_sections_fall_back_individually() {
        let mut cfg = Config::default();
        cfg.fan_names.insert("mb.pwm1".into(), "Rear".into());
        cfg.profiles[2].fan_mut("mb.pwm1", FanRole::System).fixed_duty = 33.0;
        cfg.profiles[2].fan_mut("mb.pwm2", FanRole::System).fixed_duty = 44.0;
        let mut v = serde_json::to_value(&cfg).unwrap();
        v["general"]["poll_ms"] = "fast".into();
        v["profiles"][2]["fans"]["mb.pwm2"]["min_duty"] = serde_json::json!([1, 2]);
        v["profiles"][4] = serde_json::json!(17);
        let (got, notes) = parse_lenient(&v.to_string()).unwrap();
        assert_eq!(notes.len(), 3, "{notes:?}");
        assert_eq!(got.general, General::default());
        assert_eq!(got.fan_names, cfg.fan_names);
        assert_eq!(got.profiles.len(), cfg.profiles.len(), "indices kept");
        assert_eq!(got.profiles[2].fans["mb.pwm1"].fixed_duty, 33.0);
        assert!(!got.profiles[2].fans.contains_key("mb.pwm2"), "only the broken entry falls back");
        assert_eq!(got.profiles[4], Profile::default());
    }

    #[test]
    fn every_enum_value_round_trips() {
        let mut cfg = Config::default();
        for (i, k) in EffectKind::SOFTWARE.iter().chain(EffectKind::HARDWARE.iter()).enumerate() {
            cfg.rgb.zones.insert(
                format!("z{i}"),
                ZoneConfig { effect: Effect { kind: *k, ..Default::default() }, ..Default::default() },
            );
        }
        for (i, o) in ColorOrder::ALL.iter().enumerate() {
            cfg.rgb.zones.entry(format!("z{i}")).or_default().color_order = *o;
        }
        cfg.rgb.on_exit = ExitLighting::KeepLast;
        for (i, m) in [FanMode::Bios, FanMode::Fixed, FanMode::Curve, FanMode::Ai].into_iter().enumerate() {
            let f = cfg.profiles[i].fan_mut("mb.pwm0", FanRole::Cpu);
            f.mode = m;
            f.ai.style = [AiStyle::Silent, AiStyle::Balanced, AiStyle::Performance][i % 3];
            f.curve.source.combine = if i % 2 == 0 { Combine::Max } else { Combine::Average };
        }
        let (got, notes) = parse_lenient(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(got, cfg);
    }

    /// Only text that is not JSON at all is replaced by defaults and kept as `.bad`.
    #[test]
    fn not_json_keeps_a_bad_copy_but_unknown_values_do_not() {
        let dir = test_dir("bad");
        let path = dir.join("config.json");
        let _ = std::fs::remove_file(path.with_extension("json.bad"));
        let mut v = serde_json::to_value(Config::default()).unwrap();
        v["profiles"][0]["preset"] = "Renamed".into();
        v["active_profile"] = 3.into();
        std::fs::write(&path, v.to_string()).unwrap();
        let c = Config::load_from(&path);
        assert_eq!(c.active_profile, 3);
        assert!(!path.with_extension("json.bad").exists());
        std::fs::write(&path, "{ not json").unwrap();
        let c = Config::load_from(&path);
        assert_eq!(c, Config::default());
        assert_eq!(std::fs::read_to_string(path.with_extension("json.bad")).unwrap(), "{ not json");
    }

    /// An empty, test-private folder (removed and recreated).
    fn fresh_dir(name: &str) -> PathBuf {
        let d = test_dir(name);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = fresh_dir("roundtrip");
        let path = dir.join("config.json");
        let mut c = Config { active_profile: 4, ..Config::default() };
        c.fan_names.insert("mb.pwm0".into(), "Noctua".into());
        c.rgb.ledc_slot = Some(4);
        c.calibrations.insert(
            "mb.pwm0".into(),
            FanCalibration {
                points: vec![[100.0, 1850.0], [50.0, 1100.0]],
                max_rpm: 1850.0,
                stop_duty: Some(20.0),
                start_duty: Some(25.0),
            },
        );
        c.profile_mut().fan_mut("mb.pwm1", FanRole::System).fixed_duty = 42.0;
        c.save_to(&path).unwrap();
        let back = Config::load_from(&path);
        assert_eq!(back, c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A config.json written by an older build (before `ai`, `ledc_slot`, `gpu_critical`,
    /// `show_all_headers`, `welcome_done` and the per-fan ramps existed) must load without losing the
    /// user's profiles, names or calibrations.
    #[test]
    fn old_config_format_loads_with_new_fields_defaulted() {
        let dir = fresh_dir("old");
        let path = dir.join("config.json");
        let old = r#"{
            "version": 1,
            "general": { "start_minimized": true, "close_to_tray": false, "poll_ms": 1000,
                         "fahrenheit": false, "cpu_critical": 88.0, "watchdog": false },
            "fan_names": { "mb.pwm0": "CPU cooler" },
            "calibrations": { "mb.pwm0": { "points": [[100.0, 1800.0], [30.0, 600.0]], "max_rpm": 1800.0 } },
            "profiles": [
                { "name": "Mine", "preset": "Custom",
                  "fans": { "mb.pwm0": { "mode": "Fixed", "fixed_duty": 40.0 } } }
            ],
            "active_profile": 0,
            "rgb": { "enabled": true, "brightness": 0.5, "fps": 30,
                     "global": { "kind": "Static", "colors": [[255, 0, 0]] } }
        }"#;
        std::fs::write(&path, old).unwrap();
        let (_, notes) = parse_lenient(old).unwrap();
        assert!(notes.is_empty(), "missing fields are not 'not understood': {notes:?}");
        let c = Config::load_from(&path);
        assert!(c.general.start_minimized && !c.general.close_to_tray);
        assert_eq!(c.general.cpu_critical, 88.0);
        assert_eq!(c.general.gpu_critical, 90.0, "new field defaulted");
        assert_eq!(c.ai, AiGlobal::default(), "new section defaulted");
        assert_eq!(c.rgb.ledc_slot, None);
        assert_eq!(c.rgb.fps, 30);
        assert_eq!(c.rgb.global.kind, EffectKind::Static);
        assert_eq!(c.fan_names.get("mb.pwm0").map(String::as_str), Some("CPU cooler"));
        assert_eq!(c.calibrations["mb.pwm0"].max_rpm, 1800.0);
        assert_eq!(c.calibrations["mb.pwm0"].stop_duty, None);
        assert_eq!(c.profiles.len(), 1, "user profiles are kept, not replaced by the presets");
        let f = c.profile().fan("mb.pwm0", FanRole::Cpu);
        assert_eq!(f.mode, FanMode::Fixed);
        assert_eq!(f.fixed_duty, 40.0);
        assert!(f.ramp_up >= 1.0 && f.ramp_down >= 0.5 && f.min_duty >= 0.0, "missing per-fan values are sane");
        assert!(!path.with_extension("json.bad").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unknown enum value (e.g. a preset from a newer build, or a renamed variant) only resets
    /// that value: the rest of the file is kept, the file on disk is not touched and no `.bad` copy
    /// is made (that is only for text that is not JSON at all).
    #[test]
    fn unknown_enum_value_resets_only_that_value_via_load_from() {
        let dir = fresh_dir("unknown");
        let path = dir.join("config.json");
        let text = r#"{ "version": 2, "fan_names": { "mb.pwm0": "Kept" },
                        "profiles": [ { "name": "X", "preset": "Turbo" } ] }"#;
        std::fs::write(&path, text).unwrap();
        let (_, notes) = parse_lenient(text).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("Turbo"), "{notes:?}");
        let c = Config::load_from(&path);
        assert_eq!(c.fan_names.get("mb.pwm0").map(String::as_str), Some("Kept"));
        assert_eq!(c.profiles.len(), 1);
        assert_eq!(c.profiles[0].name, "X");
        assert_eq!(c.profiles[0].preset, Preset::Custom, "unknown preset falls back to Custom");
        assert!(!path.with_extension("json.bad").exists(), "no .bad copy for a readable file");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "original is not modified by load");
        // LOAD_ERROR is process-global (other tests may set it concurrently), so only the notes
        // above are asserted exactly.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_or_missing_file_uses_defaults() {
        let dir = fresh_dir("size");
        assert_eq!(Config::load_from(&dir.join("absent.json")), Config::default());
        let path = dir.join("config.json");
        let mut big = String::from("{\"fan_names\":{");
        for i in 0..40_000 {
            big.push_str(&format!("\"mb.pwm{i}\":\"xxxxxxxxxxxxxxxxxxxx\","));
        }
        big.push_str("\"z\":\"z\"}}");
        assert!(big.len() > 1 << 20);
        std::fs::write(&path, &big).unwrap();
        assert_eq!(Config::load_from(&path), Config::default(), "files over 1 MiB are refused");
        assert!(!path.with_extension("json.bad").exists(), "an unread file is not copied");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitize_repairs_out_of_range_values() {
        let mut c = Config { active_profile: 99, ..Config::default() };
        c.general.poll_ms = 10;
        c.general.cpu_critical = 20.0;
        c.general.gpu_critical = 500.0;
        c.rgb.fps = 0;
        let f = c.profiles[2].fan_mut("mb.pwm1", FanRole::System);
        f.curve.points = (0..40).rev().map(|i| [i as f32 * 3.0, 150.0]).collect();
        f.ai.target = 10.0;
        f.ai.limit = 0.0;
        f.min_duty = 80.0;
        f.max_duty = 10.0;
        c.sanitize();
        assert_eq!(c.active_profile, 0);
        assert_eq!(c.general.poll_ms, 250);
        assert_eq!(c.general.cpu_critical, 60.0);
        assert_eq!(c.general.gpu_critical, GPU_CRITICAL_MAX);
        assert_eq!(c.rgb.fps, 5);
        let f = c.profiles[2].fan("mb.pwm1", FanRole::System);
        assert_eq!(f.curve.points.len(), 16);
        assert!(f.curve.points.windows(2).all(|w| w[0][0] <= w[1][0]), "sorted by temperature");
        assert!(f.curve.points.iter().all(|p| p[1] <= 100.0));
        assert_eq!(f.ai.target, 40.0);
        assert!(f.ai.limit >= f.ai.target + 3.0);
        assert!(f.max_duty >= f.min_duty, "max {} < min {}", f.max_duty, f.min_duty);
        // An empty profile list gets the presets back.
        let mut e = Config { profiles: Vec::new(), active_profile: 3, ..Config::default() };
        e.sanitize();
        assert_eq!(e.profiles.len(), Config::default().profiles.len());
        assert!(e.active_profile < e.profiles.len());
    }

    /// A per-fan override stored under the BIOS preset (the user's config.json has
    /// `mb.pwm0: Curve` there) is kept on disk but never takes effect.
    #[test]
    fn bios_preset_ignores_stored_overrides_without_rewriting_them() {
        let mut cfg = Config::default();
        assert_eq!(cfg.profiles[0].preset, Preset::Bios);
        let f = cfg.profiles[0].fan_mut("mb.pwm0", FanRole::Cpu);
        f.mode = FanMode::Curve;
        let stored = f.clone();
        for role in [FanRole::Cpu, FanRole::Pump, FanRole::System] {
            assert_eq!(cfg.profiles[0].fan("mb.pwm0", role).mode, FanMode::Bios);
        }
        let back = parse_lenient(&serde_json::to_string(&cfg).unwrap()).unwrap().0;
        assert_eq!(back.profiles[0].fans["mb.pwm0"], stored, "user data is not rewritten");
        assert_eq!(back.profiles[0].fan("mb.pwm0", FanRole::Cpu).mode, FanMode::Bios);
        // The same override under another preset is used.
        cfg.profiles[2].fans.insert("mb.pwm0".into(), stored.clone());
        assert_eq!(cfg.profiles[2].fan("mb.pwm0", FanRole::Cpu), stored);
    }

    /// A pump gets its own CPU curve (never a fan curve, never AI), never below the pump floor.
    #[test]
    fn pump_presets_are_own_curves_above_the_floor() {
        for preset in [
            Preset::Bios,
            Preset::Silent,
            Preset::Balanced,
            Preset::Performance,
            Preset::AiAuto,
            Preset::FullSpeed,
            Preset::Custom,
        ] {
            let f = FanConfig::preset(preset, FanRole::Pump);
            match f.mode {
                FanMode::Bios => assert_eq!(preset, Preset::Bios),
                FanMode::Fixed => assert_eq!((preset, f.fixed_duty), (Preset::FullSpeed, 100.0)),
                FanMode::Curve => {
                    assert_eq!(f.curve.source, TempSource::default(), "{preset:?}: CPU input, so the chip runs it");
                    assert_ne!(f.curve, FanConfig::preset(preset, FanRole::Cpu).curve, "{preset:?}: not a fan curve");
                    // Monotonic and reaching 100 % by 86 °C.
                    assert!(f.curve.points.windows(2).all(|w| w[0][0] < w[1][0] && w[0][1] <= w[1][1]));
                    assert!(f.curve.points.last().is_some_and(|p| p[0] <= 86.0 && p[1] == 100.0), "{preset:?}");
                }
                m => panic!("{preset:?}: pump preset mode {m:?}"),
            }
            assert!(f.min_duty >= PUMP_MIN_DUTY, "{preset:?}");
            assert!(f.curve.points.iter().all(|p| p[1] >= PUMP_MIN_DUTY), "{preset:?}");
            // CPU fans keep their curves.
            let c = FanConfig::preset(preset, FanRole::Cpu);
            if matches!(preset, Preset::Silent | Preset::Balanced | Preset::Performance) {
                assert_eq!(c.mode, FanMode::Curve);
            }
        }
        // A user-customised pump entry is kept as it is.
        let mut cfg = Config::default();
        cfg.active_profile = 2;
        let mine =
            FanConfig { mode: FanMode::Curve, min_duty: 30.0, ..FanConfig::preset(Preset::Balanced, FanRole::Pump) };
        cfg.profile_mut().fans.insert("mb.pwm5".into(), mine.clone());
        cfg.sanitize();
        assert_eq!(cfg.profile().fan("mb.pwm5", FanRole::Pump), mine);
    }

    /// Colour correction: old files load with the default, out-of-range values are clamped.
    #[test]
    fn colour_correction_defaults_and_sanitizes() {
        let old = r#"{ "rgb": { "zones": { "dled1": { "led_count": 40, "color_order": "Rgb" } } } }"#;
        let (mut c, notes) = parse_lenient(old).unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        c.sanitize();
        let z = &c.rgb.zones["dled1"];
        assert_eq!((z.led_count, z.color_order), (40, ColorOrder::Rgb));
        assert_eq!(z.correction, ColorCorrection::default());
        assert_eq!(z.correction.gamma, 2.2);
        let mut bad = ColorCorrection { gamma: f32::NAN, gain: [5.0, -1.0, 0.5] };
        bad.sanitize();
        assert_eq!(bad, ColorCorrection { gamma: 2.2, gain: [1.0, 0.05, 0.5] });
        let mut lo = ColorCorrection { gamma: 0.2, ..Default::default() };
        lo.sanitize();
        assert_eq!(lo.gamma, 1.0);
        // A broken correction only resets the correction.
        let txt = r#"{ "rgb": { "zones": { "dled1": { "led_count": 40, "correction": "x" } } } }"#;
        let (c, notes) = parse_lenient(txt).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(c.rgb.zones["dled1"].led_count, 40);
    }

    /// One call selects a mode (recreating a deleted built-in profile); the cycle walks the six
    /// modes in tile order and leaves a custom profile for the first one; the result persists.
    #[test]
    fn apply_preset_selects_recreates_cycles_and_persists() {
        let mut c = Config::default();
        assert_eq!(c.apply_preset(Preset::Performance), 3);
        assert_eq!(c.fan_mode().label, "Performance");
        assert_eq!(c.apply_preset(Preset::Custom), 3, "Custom is not a mode");
        let order: Vec<Preset> = (0..6).map(|_| c.cycle_preset()).collect();
        assert_eq!(
            order,
            [Preset::FullSpeed, Preset::AiAuto, Preset::Bios, Preset::Silent, Preset::Balanced, Preset::Performance]
        );
        c.profiles.retain(|p| p.preset != Preset::AiAuto);
        c.active_profile = 0;
        let i = c.apply_preset(Preset::AiAuto);
        assert_eq!(
            (c.profiles[i].name.as_str(), c.profiles[i].preset, c.active_profile),
            ("AI Auto", Preset::AiAuto, i)
        );
        assert_eq!(c.fan_mode().label, "Auto (AI)");
        c.profiles.push(Profile::new("Mine", Preset::Custom));
        c.active_profile = c.profiles.len() - 1;
        let m = c.fan_mode();
        assert_eq!((m.preset, m.label.as_str(), m.edited), (Preset::Custom, "Mine", false));
        assert_eq!(c.next_preset(), Preset::Silent);
        // "Edited": a preset profile with per-fan overrides.
        c.apply_preset(Preset::Balanced);
        c.profile_mut().fan_mut("mb.pwm0", FanRole::Cpu).min_duty = 33.0;
        assert!(c.fan_mode().edited);
        let dir = fresh_dir("presets");
        let path = dir.join("config.json");
        c.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path).fan_mode(), c.fan_mode());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Header roles: old files load without them, unknown values fall back to `Unknown`, names
    /// follow the role unless the user named the header, and the effective role rules hold.
    #[test]
    fn header_roles_load_name_and_resolve() {
        let (c, notes) = parse_lenient(r#"{ "fan_names": { "mb.pwm0": "Mine" } }"#).unwrap();
        assert!(notes.is_empty() && c.fan_roles.is_empty());
        let (c, notes) = parse_lenient(r#"{ "fan_roles": { "mb.pwm0": "Pump", "mb.pwm1": "Hovercraft" } }"#).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(
            (c.header_role("mb.pwm0"), c.header_role("mb.pwm1")),
            (Some(HeaderRole::Pump), Some(HeaderRole::Unknown))
        );
        let mut c = Config::default();
        c.set_fan_role("mb.pwm0", Some(HeaderRole::RadiatorFans), None);
        assert_eq!(c.fan_names["mb.pwm0"], "Radiator fans");
        c.set_fan_role("mb.pwm0", Some(HeaderRole::Pump), None);
        assert_eq!(c.fan_names["mb.pwm0"], "AIO pump", "a generated name follows the role");
        c.set_fan_role("mb.pwm0", Some(HeaderRole::Pump), Some("Arctic pump"));
        c.set_fan_role("mb.pwm0", Some(HeaderRole::CaseFans), None);
        assert_eq!(c.fan_names["mb.pwm0"], "Arctic pump", "a user name is kept");
        c.set_fan_role("mb.pwm1", Some(HeaderRole::CaseFans), None);
        c.set_fan_role("mb.pwm1", Some(HeaderRole::Unused), None);
        assert!(!c.fan_names.contains_key("mb.pwm1") && c.header_unused("mb.pwm1"));
        c.set_fan_role("mb.pwm1", None, None);
        assert_eq!(c.header_role("mb.pwm1"), None);
        let back = parse_lenient(&serde_json::to_string(&c).unwrap()).unwrap().0;
        assert_eq!(back.fan_roles, c.fan_roles);
        // Effective role.
        let desc = |role, present, tach| crate::model::FanChannelDesc {
            id: "mb.pwm0".into(),
            default_name: "CPU_FAN".into(),
            rpm_sensor: None,
            controllable: true,
            role,
            present,
            tach_detected: tach,
            documented: Some(true),
        };
        assert_eq!(effective_role(None, &desc(FanRole::Cpu, true, true), true), FanRole::Cpu);
        assert_eq!(
            effective_role(None, &desc(FanRole::Cpu, true, false), false),
            FanRole::Unknown,
            "no tach, unconfirmed"
        );
        assert_eq!(effective_role(None, &desc(FanRole::Pump, true, false), false), FanRole::Pump, "board pump header");
        assert_eq!(effective_role(None, &desc(FanRole::Cpu, false, false), false), FanRole::Cpu, "nothing there");
        let rad = Some(HeaderRole::RadiatorFans);
        assert_eq!(effective_role(rad, &desc(FanRole::System, true, false), false), FanRole::Cpu);
        assert_eq!(effective_role(Some(HeaderRole::Unused), &desc(FanRole::Cpu, true, false), false), FanRole::Cpu);
    }

    /// What a header runs per role, and the chip ΔT per preset.
    #[test]
    fn header_plans_and_preset_hysteresis() {
        let ai = FanConfig::preset(Preset::AiAuto, FanRole::Cpu);
        let p = HeaderPlan::new(ai.clone(), FanRole::Cpu, true);
        assert_eq!((p.fc.mode, p.ai_refused, p.floor), (FanMode::Ai, None, None));
        let p = HeaderPlan::new(ai.clone(), FanRole::Cpu, false);
        assert_eq!((p.fc.mode, p.ai_refused), (FanMode::Curve, Some(NO_SPEED_SIGNAL_AI)));
        assert_eq!(p.fc.curve, FanConfig::preset(Preset::Balanced, FanRole::Cpu).curve, "the style's curve");
        let p = HeaderPlan::new(ai.clone(), FanRole::Pump, true);
        assert_eq!((p.fc.mode, p.ai_refused), (FanMode::Curve, Some(PUMP_AI)));
        assert_eq!(p.fc.curve, FanConfig::preset(Preset::Balanced, FanRole::Pump).curve, "the Balanced pump curve");
        let p = HeaderPlan::new(FanConfig::preset(Preset::AiAuto, FanRole::Unknown), FanRole::Unknown, true);
        assert_eq!((p.fc.mode, p.ai_refused, p.floor), (FanMode::Curve, Some(UNKNOWN_AI), Some(UNKNOWN_MIN_DUTY)));
        assert!(p.fc.min_duty >= UNKNOWN_MIN_DUTY && !p.fc.allow_stop);
        let low = FanConfig { mode: FanMode::Fixed, fixed_duty: 20.0, min_duty: 0.0, allow_stop: true, ..ai.clone() };
        let p = HeaderPlan::new(low.clone(), FanRole::Pump, true);
        assert_eq!((p.fc.fixed_duty, p.fc.min_duty, p.fc.allow_stop), (PUMP_MIN_DUTY, PUMP_MIN_DUTY, false));
        let p = HeaderPlan::new(FanConfig { mode: FanMode::Bios, ..low }, FanRole::Unknown, false);
        assert_eq!((p.fc.mode, p.floor), (FanMode::Bios, None), "BIOS stays BIOS");
        for (preset, dt) in [(Preset::Silent, 3), (Preset::Balanced, 2), (Preset::Performance, 1)] {
            for role in [FanRole::Cpu, FanRole::System, FanRole::Unknown] {
                let f = FanConfig::preset(preset, role);
                assert_eq!(crate::engine::chip_delta_t(f.curve.hysteresis), dt, "{preset:?} {role:?}");
                assert_eq!(f.mode, FanMode::Curve);
            }
        }
        let u = FanConfig::preset(Preset::Silent, FanRole::Unknown);
        assert_eq!(
            (u.min_duty, u.curve.source, u.curve.points.clone()),
            (UNKNOWN_MIN_DUTY, TempSource::default(), FanConfig::preset(Preset::Silent, FanRole::Cpu).curve.points)
        );
        assert_eq!(FanConfig::preset(Preset::FullSpeed, FanRole::Unknown).fixed_duty, 100.0);
        assert!(Preset::MODES.iter().all(|p| !p.label().is_empty() && !p.descriptor().is_empty()));
    }

    /// audit FR-PRIV-005 (independent tester): for every role the settings can give a header, every
    /// mode, with or without a speed signal, the board's CPU label adds the 20 % floor and its PUMP
    /// label the 30 % floor (no fan stop, minimum, fixed duty and maximum lifted), a stricter role
    /// floor is kept, other board labels add nothing, values above the floor are left alone, a
    /// non-finite entry from a hand-edited file still gets the floor, and BIOS is passed through.
    #[test]
    fn board_floor_binds_every_role_and_mode_except_bios() {
        let base = FanConfig::preset(Preset::Balanced, FanRole::System);
        let worst = |mode: FanMode| FanConfig {
            mode,
            fixed_duty: 0.0,
            min_duty: 0.0,
            max_duty: 0.0,
            allow_stop: true,
            ..base.clone()
        };
        for board in [FanRole::Cpu, FanRole::Pump, FanRole::System, FanRole::Unknown] {
            let board_floor = match board {
                FanRole::Cpu => Some(CPU_HEADER_MIN_DUTY),
                FanRole::Pump => Some(PUMP_MIN_DUTY),
                FanRole::System | FanRole::Unknown => None,
            };
            for role in [FanRole::Cpu, FanRole::System, FanRole::Pump, FanRole::Unknown] {
                for signal in [true, false] {
                    for mode in [FanMode::Fixed, FanMode::Curve, FanMode::Ai] {
                        let plain = HeaderPlan::new(worst(mode), role, signal);
                        let p = plain.clone().with_board_floor(board);
                        let want = plain.floor.unwrap_or(0.0).max(board_floor.unwrap_or(0.0));
                        let ctx = format!("board {board:?}, role {role:?}, signal {signal}, {mode:?}: {p:?}");
                        assert_eq!(
                            (p.floor, p.board_floor, p.ai_refused),
                            (plain.floor, board_floor, plain.ai_refused),
                            "{ctx}"
                        );
                        assert_eq!(p.running_floor(), want, "{ctx}");
                        assert_eq!(p.fc.mode, plain.fc.mode, "{ctx}");
                        if want > 0.0 {
                            assert!(!p.fc.allow_stop, "{ctx}");
                            assert!(p.fc.min_duty >= want && p.fc.fixed_duty >= want, "{ctx}");
                            assert!(p.fc.max_duty >= p.fc.min_duty, "{ctx}");
                        } else {
                            assert_eq!(p, plain, "{ctx}");
                        }
                    }
                }
            }
            // BIOS: passed through untouched.
            let bios = worst(FanMode::Bios);
            let p = HeaderPlan::new(bios.clone(), FanRole::System, true).with_board_floor(board);
            assert_eq!((&p.fc, p.floor, p.board_floor, p.running_floor()), (&bios, None, None, 0.0), "{board:?}");
        }
        // The stricter unknown-role floor stays in charge on a board CPU header.
        let p = HeaderPlan::new(worst(FanMode::Fixed), FanRole::Unknown, false).with_board_floor(FanRole::Cpu);
        assert_eq!(
            (p.floor, p.board_floor, p.running_floor(), p.fc.fixed_duty, p.fc.min_duty),
            (Some(UNKNOWN_MIN_DUTY), Some(CPU_HEADER_MIN_DUTY), UNKNOWN_MIN_DUTY, UNKNOWN_MIN_DUTY, UNKNOWN_MIN_DUTY)
        );
        // Above the floor nothing changes.
        let high = FanConfig {
            mode: FanMode::Fixed,
            fixed_duty: 55.0,
            min_duty: 35.0,
            max_duty: 90.0,
            allow_stop: false,
            ..base.clone()
        };
        assert_eq!(HeaderPlan::new(high.clone(), FanRole::System, true).with_board_floor(FanRole::Pump).fc, high);
        // NaN duties (the plan is built after `clamp_safety`, but must not depend on it).
        let nan = FanConfig { fixed_duty: f32::NAN, min_duty: f32::NAN, max_duty: f32::NAN, ..worst(FanMode::Fixed) };
        let p = HeaderPlan::new(nan, FanRole::System, true).with_board_floor(FanRole::Cpu);
        assert_eq!((p.fc.fixed_duty, p.fc.min_duty, p.fc.max_duty), (20.0, 20.0, 20.0), "{p:?}");
        assert_eq!(CPU_HEADER_MIN_DUTY, 20.0);
    }

    /// Segment tables: old files load without them; bad entries are cleaned, not fatal.
    #[test]
    fn segments_load_and_sanitize() {
        let old = r#"{ "rgb": { "zones": { "dled1": { "led_count": 40 } } } }"#;
        let (c, notes) = parse_lenient(old).unwrap();
        assert!(notes.is_empty() && c.rgb.zones["dled1"].segments.is_empty() && !c.rgb.dither);
        let txt = r#"{ "rgb": { "dither": true, "zones": { "dled1": { "led_count": 96, "segments": [
            { "name": "  Pump head  ", "from_led": 72, "to_led": 96, "gain": [1.0, 0.9, 7.0] },
            { "name": "Fans", "from_led": 0, "to_led": 72 },
            { "name": "overlap", "from_led": 70, "to_led": 80 },
            { "name": "empty", "from_led": 5, "to_led": 5 },
            "garbage"
        ] } } } }"#;
        let (mut c, notes) = parse_lenient(txt).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        c.sanitize();
        let s = &c.rgb.zones["dled1"].segments;
        assert_eq!(s.len(), 2, "{s:?}");
        assert_eq!((s[0].name.as_str(), s[0].from_led, s[0].to_led), ("Fans", 0, 72));
        assert_eq!((s[1].name.as_str(), s[1].gain), ("Pump head", [1.0, 0.9, 1.0]));
        assert!(c.rgb.dither);
        let mut many: Vec<Segment> =
            (0..20).map(|i| Segment { from_led: i * 2, to_led: i * 2 + 1, ..Default::default() }).collect();
        sanitize_segments(&mut many, 1024);
        assert_eq!(many.len(), MAX_SEGMENTS);
        let mut big = vec![Segment { from_led: 1000, to_led: 5000, ..Default::default() }];
        sanitize_segments(&mut big, 1024);
        assert_eq!(big[0].to_led, 1024);
    }

    /// Demo mode redirects config.json and ai_models.json.
    #[test]
    fn storage_can_be_redirected() {
        let dir = test_dir("demo");
        redirect_storage(dir.clone());
        assert_eq!(config_path(), dir.join("config.json"));
        assert_eq!(ai_models_path(), dir.join("ai_models.json"));
        let mut c = Config::default();
        c.general.poll_ms = 1234;
        c.save().unwrap();
        assert_eq!(Config::load().general.poll_ms, 1234);
        assert!(dir.join("config.json").exists());
    }
}
