//! Windows Dynamic Lighting (LampArray) status and "Let FanRGB control these LEDs".
//!
//! The IT5702 is a composite device: MI_00 is a HID LampArray that Windows Dynamic Lighting (and
//! background providers such as Razer Synapse's `razerwdl.exe`) can paint, MI_01 is the vendor
//! interface FanRGB drives. Both reach the same LEDs through the same firmware, so a second
//! writer shows up as flicker or "slightly wrong" colours.
//!
//! What is read (read-only, the hive of the user at the desktop: `HKEY_USERS\<SID>` of the shell's
//! token when elevated, since the elevated `HKCU` can be another account with over-the-shoulder UAC
//! ([`crate::util::win::desktop_user_hive`]); if that user cannot be determined the state
//! is [`DlState::UserUnknown`], never a value from the administrator's hive):
//! * `Software\Microsoft\Lighting\AmbientLightingEnabled`: "Use Dynamic Lighting on my devices"
//!   (global background control), `ControlledByForegroundApp`, the global `Providers`;
//! * `Software\Microsoft\Lighting\Devices\<HID#VID_048D&PID_5702&MI_00#…>\AmbientLightingEnabled`
//!   and its `Providers` for every RGB Fusion 2 LampArray device;
//! * running LampArray bridges (Razer) and other RGB software;
//! * the firmware's `[CC 48]` reply byte (filled in from the controller by the lighting thread).
//!
//! The one write: [`DlMonitor::claim`] sets the per-device `AmbientLightingEnabled` DWORD to 0 on
//! existing device keys, exactly what GIGABYTE Control Center does when it turns Dynamic Lighting off
//! (it too writes only when the value differs).
//! GIGABYTE's software also watches `Software\Microsoft\Lighting\Devices` and re-forces 0;
//! FanRGB only warns ([`DynamicLightingStatus::retaken`]) and lets the
//! user decide. It never writes the global value, never creates keys and never sends a LampArray
//! or 0x48 write of its own. No package identity (MSIX) is involved.

use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::ErrorCode;
use crate::util::win::{Signal, pause};

pub const LIGHTING_KEY: &str = r"Software\Microsoft\Lighting";
pub const DEVICES_KEY: &str = r"Software\Microsoft\Lighting\Devices";
pub const AMBIENT_VALUE: &str = "AmbientLightingEnabled";
/// Windows Settings page for Dynamic Lighting.
pub const SETTINGS_URI: &str = "ms-settings:personalization-lighting";

/// Full re-read even without a registry notification (process list, missed notifications).
const POLL: Duration = Duration::from_secs(5);
/// Longest sleep between checks for a claim or quit when no wake-up event could be created.
const WAKE_FALLBACK: Duration = Duration::from_millis(500);
/// The process list is scanned at most this often.
const PROCESS_SCAN: Duration = Duration::from_secs(10);

/// Running programs that can write the controller through Dynamic Lighting / LampArray or
/// directly, with a short reason.
pub const CONTENDERS: &[(&str, &str, &str)] = &[
    ("razerwdl.exe", "Razer Synapse LampArray bridge", "paints LampArray devices for Razer Chroma"),
    ("razerappengine.exe", "Razer Synapse", "can drive Dynamic Lighting devices"),
    ("rzsmartlightingdevicemanager.exe", "Razer smart lighting", "can drive Dynamic Lighting devices"),
    ("gbt_dl_lib.exe", "GIGABYTE Dynamic Lighting helper", "calls LampArray.SetColor on the IT5702"),
    ("gcc.exe", "GIGABYTE Control Center", "drives the same RGB controller"),
    ("rgbfusion.exe", "RGB Fusion", "drives the same RGB controller"),
    ("openrgb.exe", "OpenRGB", "drives the same RGB controller"),
    ("signalrgb.exe", "SignalRGB", "drives the same RGB controller"),
];

/// Summary for the Lighting chip.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DlState {
    /// Not read yet.
    #[default]
    Unknown,
    /// Windows does not drive the controller (device or global switch off).
    Off,
    /// Windows Dynamic Lighting is enabled for the controller and may override FanRGB.
    Controlling,
    /// No Dynamic Lighting device is registered for the controller (Windows 10, or the LampArray
    /// interface is not set up).
    Unavailable,
    /// The desktop user's registry could not be found (no desktop shell, or it runs as another
    /// account), so the state is not known.
    UserUnknown,
}

impl DlState {
    pub fn label(self) -> &'static str {
        match self {
            DlState::Unknown => "Dynamic Lighting: checking",
            DlState::Off => "Dynamic Lighting: off",
            DlState::Controlling => "Dynamic Lighting: on",
            DlState::Unavailable => "Dynamic Lighting: not available",
            DlState::UserUnknown => "Dynamic Lighting: unknown (desktop user not found)",
        }
    }
}

/// What the firmware's `[CC 48]` reply byte says (1 = LampArray disabled, 99 = query
/// unsupported, anything else = LampArray armed, i.e. Windows *can* paint if it wants to).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirmwareDl {
    Disabled,
    Armed(u8),
    Unsupported,
}

impl FirmwareDl {
    pub fn from_raw(b: u8) -> Self {
        match b {
            1 => FirmwareDl::Disabled,
            99 => FirmwareDl::Unsupported,
            v => FirmwareDl::Armed(v),
        }
    }

    pub fn label(self) -> String {
        match self {
            FirmwareDl::Disabled => "LampArray disabled (0x48 reply 1)".into(),
            FirmwareDl::Armed(v) => format!("LampArray armed (0x48 reply {v})"),
            FirmwareDl::Unsupported => "not supported (0x48 reply 99)".into(),
        }
    }
}

/// One RGB Fusion 2 LampArray device key.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DlDevice {
    /// Subkey name under `Lighting\Devices` (the device interface path).
    pub key: String,
    /// Per-device `AmbientLightingEnabled` (None = not set: follows the global switch).
    pub ambient: Option<u32>,
    pub providers: Vec<String>,
}

/// Raw registry values (one read).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DlSnapshot {
    /// The desktop user's hive could not be opened (why); every other field is empty then.
    pub hive_error: Option<String>,
    pub global_ambient: Option<u32>,
    pub foreground: Option<u32>,
    pub providers: Vec<String>,
    pub devices: Vec<DlDevice>,
}

/// Everything the UI shows about Dynamic Lighting (`RgbStatus::dynamic_lighting`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DynamicLightingStatus {
    pub state: DlState,
    /// Global "Use Dynamic Lighting on my devices" (None = not set; Windows' default is on).
    pub global_ambient: Option<u32>,
    /// "Compatible apps in the foreground always control lighting".
    pub foreground: Option<u32>,
    pub providers: Vec<String>,
    /// RGB Fusion 2 (VID 048D) LampArray devices.
    pub devices: Vec<DlDevice>,
    /// Raw `[CC 48]` reply byte of the connected controller (None = not connected / not queried).
    pub firmware_raw: Option<u8>,
    /// Running programs that may write the LEDs: (exe, name, why).
    pub contenders: Vec<(String, String, String)>,
    /// "Let FanRGB control these LEDs" was used in this session.
    pub claimed: bool,
    /// After the claim, Windows (Settings or a provider) switched the device back on.
    pub retaken: bool,
    /// The registry watch is running (changes are seen within a second, else within [`POLL`]).
    pub watching: bool,
    /// Last claim failure (`FRG-E410` detail).
    pub claim_error: Option<String>,
    /// Demo: fixed values, no registry access.
    pub simulated: bool,
    /// Whose settings are shown (`HKEY_USERS\<SID>` or `HKCU`), or why they cannot be.
    pub hive: String,
}

impl DynamicLightingStatus {
    pub fn firmware(&self) -> Option<FirmwareDl> {
        self.firmware_raw.map(FirmwareDl::from_raw)
    }

    /// Short chip text, e.g. "Dynamic Lighting: off · firmware: LampArray armed".
    pub fn chip_label(&self) -> String {
        let mut s = self.state.label().to_string();
        if let Some(f) = self.firmware() {
            s.push_str(match f {
                FirmwareDl::Disabled => " · firmware: LampArray off",
                FirmwareDl::Armed(_) => " · firmware: LampArray armed",
                FirmwareDl::Unsupported => "",
            });
        }
        s
    }

    /// Show the conflict banner: Windows can paint the LEDs, or it took them back after a claim.
    /// The firmware byte alone never raises it (here the firmware
    /// says "armed" while Windows has the device off).
    pub fn conflict(&self) -> bool {
        self.state == DlState::Controlling || self.retaken
    }

    /// Banner title and text for the conflict state.
    pub fn banner(&self) -> Option<(String, String)> {
        if !self.conflict() {
            return None;
        }
        let who = if self.contenders.iter().any(|c| c.0.starts_with("razer") || c.0.starts_with("rz")) {
            " Razer Synapse is running and can paint them through Windows."
        } else {
            ""
        };
        let title = if self.retaken {
            "Windows took the LEDs back"
        } else {
            "Windows Dynamic Lighting is also driving these LEDs"
        };
        Some((
            title.into(),
            format!(
                "Dynamic Lighting is on for the RGB controller, so colours may flicker or look wrong.{who} \
                 “Let FanRGB control these LEDs” switches it off for this device only (Windows Settings › \
                 Personalization › Dynamic Lighting can switch it back)."
            ),
        ))
    }

    /// One line for the diagnostics report.
    pub fn summary(&self) -> String {
        let flag = |v: Option<u32>| match v {
            Some(0) => "off".to_string(),
            Some(1) => "on".to_string(),
            Some(v) => format!("{v}"),
            None => "not set".to_string(),
        };
        let mut s = format!(
            "{}; global background control {}, foreground apps {}",
            self.state.label(),
            flag(self.global_ambient),
            flag(self.foreground)
        );
        if !self.hive.is_empty() {
            s.push_str(&format!("; registry {}", self.hive));
        }
        for d in &self.devices {
            s.push_str(&format!("; device {} {}", short_key(&d.key), flag(d.ambient)));
        }
        match self.firmware() {
            Some(f) => s.push_str(&format!("; firmware {}", f.label())),
            None => s.push_str("; firmware not queried"),
        }
        if self.claimed {
            s.push_str(if self.retaken { "; claimed, then retaken by Windows" } else { "; claimed by FanRGB" });
        }
        if !self.contenders.is_empty() {
            let names: Vec<&str> = self.contenders.iter().map(|c| c.1.as_str()).collect();
            s.push_str(&format!("; running: {}", names.join(", ")));
        }
        if let Some(e) = &self.claim_error {
            s.push_str(&format!("; last claim failed: {e}"));
        }
        if self.simulated {
            s.push_str(" (demo: simulated)");
        }
        s
    }
}

/// "HID#VID_048D&PID_5702&MI_00#7&2f…" → "VID_048D&PID_5702&MI_00".
pub fn short_key(k: &str) -> &str {
    k.split('#').find(|p| p.to_ascii_uppercase().starts_with("VID_")).unwrap_or(k)
}

/// A device key that belongs to an RGB Fusion 2 controller (its key name contains `VID_048D&PID_5702`).
pub fn is_fusion_device(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    crate::diag::FUSION2_PIDS.iter().any(|pid| k.contains(&format!("VID_048D&PID_{pid:04X}")))
}

/// The chip state from the registry. Unset values follow Windows' defaults (on), so a missing
/// value never hides a possible conflict.
pub fn evaluate(snap: &DlSnapshot) -> DlState {
    if snap.hive_error.is_some() {
        return DlState::UserUnknown;
    }
    if snap.devices.is_empty() {
        return DlState::Unavailable;
    }
    let global_on = snap.global_ambient != Some(0);
    let device_on = snap.devices.iter().any(|d| d.ambient != Some(0));
    if global_on && device_on { DlState::Controlling } else { DlState::Off }
}

/// Registry access, process list and the watch (real, or simulated for tests and `--demo`).
pub trait DlBackend: Send {
    fn read(&mut self) -> DlSnapshot;
    /// Sets the per-device value on an existing device key.
    fn set_device_ambient(&mut self, key: &str, value: u32) -> anyhow::Result<()>;
    /// Lower-cased running executables.
    fn processes(&mut self) -> Vec<String>;
    /// Blocks up to `timeout` for a change, or until `wake` is set (a claim, shutdown); true =
    /// something changed.
    fn wait_change(&mut self, timeout: Duration, wake: Option<&Signal>) -> bool;
    fn watching(&self) -> bool;
    fn simulated(&self) -> bool {
        false
    }
    /// Whose registry is read (for the report).
    fn hive(&self) -> String {
        String::new()
    }
}

/// Reads the Dynamic Lighting values from a user hive (`root` = that hive's root key).
pub fn read_hive(root: windows_sys::Win32::System::Registry::HKEY) -> DlSnapshot {
    use crate::util::win::{reg_get_dword, reg_string_values, reg_subkeys};
    let providers = |key: &str| {
        let mut v = reg_string_values(root, key, 64);
        v.sort_by_key(|(n, _)| n.parse::<u32>().unwrap_or(u32::MAX));
        v.into_iter().map(|(_, d)| d).collect::<Vec<_>>()
    };
    let devices = reg_subkeys(root, DEVICES_KEY, 256)
        .into_iter()
        .filter(|k| is_fusion_device(k))
        .map(|k| {
            let key = format!(r"{DEVICES_KEY}\{k}");
            DlDevice {
                ambient: reg_get_dword(root, &key, AMBIENT_VALUE),
                providers: providers(&format!(r"{key}\Providers")),
                key: k,
            }
        })
        .collect();
    DlSnapshot {
        hive_error: None,
        global_ambient: reg_get_dword(root, LIGHTING_KEY, AMBIENT_VALUE),
        foreground: reg_get_dword(root, LIGHTING_KEY, "ControlledByForegroundApp"),
        providers: providers(&format!(r"{LIGHTING_KEY}\Providers")),
        devices,
    }
}

/// The real backend: the desktop user's hive, Toolhelp process list, `RegNotifyChangeKeyValue`.
#[derive(Default)]
pub struct RegistryBackend {
    /// Opened on first use and kept (the logged-on user does not change while FanRGB runs); retried
    /// on every read while it cannot be opened.
    hive: Option<crate::util::win::UserHive>,
    hive_error: Option<String>,
    watch: Option<crate::util::win::RegWatch>,
    tried: bool,
}

// SAFETY: the hive key and the watch's key and event handles are created and used only on the
// monitor thread that owns the backend (lazily, after the backend moved there); they are plain
// kernel handles without thread affinity.
unsafe impl Send for RegistryBackend {}

impl RegistryBackend {
    fn hive_root(&mut self) -> Option<windows_sys::Win32::System::Registry::HKEY> {
        if self.hive.is_none() {
            match crate::util::win::desktop_user_hive() {
                Ok(h) => {
                    log::info!("Dynamic Lighting: reading the desktop user's settings in {}", h.describe());
                    self.hive = Some(h);
                    self.hive_error = None;
                }
                Err(e) => {
                    let e = format!("{e:#}");
                    if self.hive_error.as_deref() != Some(e.as_str()) {
                        log::warn!("Dynamic Lighting: desktop user's registry not found: {e}");
                    }
                    self.hive_error = Some(e);
                }
            }
        }
        self.hive.as_ref().map(|h| h.root())
    }
}

impl DlBackend for RegistryBackend {
    fn read(&mut self) -> DlSnapshot {
        match self.hive_root() {
            Some(root) => read_hive(root),
            None => DlSnapshot { hive_error: self.hive_error.clone(), ..Default::default() },
        }
    }

    fn set_device_ambient(&mut self, key: &str, value: u32) -> anyhow::Result<()> {
        if !is_fusion_device(key) || key.contains('\\') {
            anyhow::bail!("refusing to write a non-RGB-Fusion key {key}");
        }
        let Some(root) = self.hive_root() else {
            anyhow::bail!("the desktop user's registry was not found: {}", self.hive_error.clone().unwrap_or_default());
        };
        crate::util::win::reg_set_dword(root, &format!(r"{DEVICES_KEY}\{key}"), AMBIENT_VALUE, value)
    }

    fn processes(&mut self) -> Vec<String> {
        crate::util::win::running_processes()
    }

    fn wait_change(&mut self, timeout: Duration, wake: Option<&Signal>) -> bool {
        if self.watch.is_none() && !self.tried {
            let Some(root) = self.hive_root() else {
                // Not known yet: poll, and try the watch again once the hive is found.
                pause(wake, timeout, WAKE_FALLBACK);
                return false;
            };
            self.tried = true;
            self.watch = crate::util::win::RegWatch::new(root, LIGHTING_KEY);
            if self.watch.is_none() {
                log::info!("Dynamic Lighting: {LIGHTING_KEY} cannot be watched (missing?); polling every 5 s");
            }
        }
        match self.watch.as_mut() {
            Some(w) => w.wait(timeout, wake),
            None => {
                pause(wake, timeout, WAKE_FALLBACK);
                false
            }
        }
    }

    fn watching(&self) -> bool {
        self.watch.is_some()
    }

    fn hive(&self) -> String {
        match (&self.hive, &self.hive_error) {
            (Some(h), _) => h.describe(),
            (None, Some(e)) => format!("not found ({e})"),
            (None, None) => String::new(),
        }
    }
}

/// Fixed state for `--demo` and tests: values live in memory, nothing touches the registry.
#[derive(Clone, Default)]
pub struct FakeBackend {
    pub snap: Arc<Mutex<DlSnapshot>>,
    pub running: Arc<Mutex<Vec<String>>>,
    pub writes: Arc<Mutex<Vec<(String, u32)>>>,
    pub fail_writes: Arc<Mutex<bool>>,
    /// Registry reads so far (tests: the monitor must block between polls, not spin).
    pub reads: Arc<std::sync::atomic::AtomicUsize>,
}

impl FakeBackend {
    /// What the reference PC showed: everything off, one IT5702 device.
    pub fn demo() -> Self {
        let f = FakeBackend::default();
        *f.snap.lock().unwrap_or_else(|p| p.into_inner()) = DlSnapshot {
            hive_error: None,
            global_ambient: Some(0),
            foreground: Some(0),
            providers: vec!["WindowsLighting".into()],
            devices: vec![DlDevice {
                key: "HID#VID_048D&PID_5702&MI_00#7&demo&0&0000#{47c9e1a5-0000-0000-0000-000000000000}".into(),
                ambient: Some(0),
                providers: vec!["WindowsLighting".into()],
            }],
        };
        f
    }
}

impl DlBackend for FakeBackend {
    fn read(&mut self) -> DlSnapshot {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.snap.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn set_device_ambient(&mut self, key: &str, value: u32) -> anyhow::Result<()> {
        if *self.fail_writes.lock().unwrap_or_else(|p| p.into_inner()) {
            anyhow::bail!("simulated registry failure");
        }
        self.writes.lock().unwrap_or_else(|p| p.into_inner()).push((key.into(), value));
        let mut s = self.snap.lock().unwrap_or_else(|p| p.into_inner());
        for d in s.devices.iter_mut().filter(|d| d.key == key) {
            d.ambient = Some(value);
        }
        Ok(())
    }

    fn processes(&mut self) -> Vec<String> {
        self.running.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn wait_change(&mut self, timeout: Duration, wake: Option<&Signal>) -> bool {
        // Nothing to watch: block until the next poll or a claim (no short-sleep loop, which woke
        // the demo's monitor thread 50 times a second).
        pause(wake, timeout, Duration::from_millis(20));
        false
    }

    fn watching(&self) -> bool {
        true
    }

    fn simulated(&self) -> bool {
        true
    }
}

/// State machine shared by the monitor thread and the tests.
pub struct DlCore {
    backend: Box<dyn DlBackend>,
    status: DynamicLightingStatus,
    last_scan: Option<Instant>,
    logged_controlling: bool,
}

impl DlCore {
    pub fn new(backend: Box<dyn DlBackend>) -> Self {
        let simulated = backend.simulated();
        DlCore {
            backend,
            status: DynamicLightingStatus { simulated, ..Default::default() },
            last_scan: None,
            logged_controlling: false,
        }
    }

    pub fn status(&self) -> &DynamicLightingStatus {
        &self.status
    }

    /// Re-reads everything and updates the derived flags. Logs `FRG-E409` once per episode.
    pub fn refresh(&mut self, now: Instant) {
        let snap = self.backend.read();
        let state = evaluate(&snap);
        self.status.hive = self.backend.hive();
        if self.last_scan.is_none_or(|t| now.duration_since(t) >= PROCESS_SCAN) {
            self.last_scan = Some(now);
            let running = self.backend.processes();
            self.status.contenders = CONTENDERS
                .iter()
                .filter(|(exe, _, _)| running.iter().any(|p| p == exe))
                .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
                .collect();
        }
        let st = &mut self.status;
        st.global_ambient = snap.global_ambient;
        st.foreground = snap.foreground;
        st.providers = snap.providers;
        st.devices = snap.devices;
        st.watching = self.backend.watching();
        if st.claimed {
            st.retaken = state == DlState::Controlling;
        }
        if state == DlState::Controlling && !self.logged_controlling {
            self.logged_controlling = true;
            log::warn!(
                "[{}] Windows Dynamic Lighting is enabled for the RGB controller ({}){}",
                ErrorCode::DynamicLightingActive,
                st.devices.iter().map(|d| short_key(&d.key)).collect::<Vec<_>>().join(", "),
                if st.retaken { " after FanRGB switched it off" } else { "" }
            );
        } else if state != DlState::Controlling && self.logged_controlling {
            self.logged_controlling = false;
            log::info!("Windows Dynamic Lighting is off for the RGB controller again");
        }
        st.state = state;
    }

    /// "Let FanRGB control these LEDs": per-device `AmbientLightingEnabled` = 0 on every RGB
    /// Fusion 2 device key whose value is not already 0 (as GCC). Never creates keys.
    pub fn claim(&mut self, now: Instant) -> anyhow::Result<usize> {
        let snap = self.backend.read();
        if let Some(e) = &snap.hive_error {
            let e = format!("the desktop user's registry was not found: {e}");
            self.status.claim_error = Some(e.clone());
            return Err(crate::error::err(ErrorCode::DynamicLightingClaim, e));
        }
        if snap.devices.is_empty() {
            let e = "no RGB Fusion 2 Dynamic Lighting device is registered in Windows".to_string();
            self.status.claim_error = Some(e.clone());
            return Err(crate::error::err(ErrorCode::DynamicLightingClaim, e));
        }
        let mut changed = 0;
        let mut first_err = None;
        for d in snap.devices.iter().filter(|d| d.ambient != Some(0)) {
            match self.backend.set_device_ambient(&d.key, 0) {
                Ok(()) => {
                    changed += 1;
                    log::info!(
                        "Dynamic Lighting switched off for {} (AmbientLightingEnabled {:?} -> 0)",
                        short_key(&d.key),
                        d.ambient
                    );
                }
                Err(e) => {
                    log::warn!("[{}] {}: {e:#}", ErrorCode::DynamicLightingClaim, short_key(&d.key));
                    first_err.get_or_insert(format!("{e:#}"));
                }
            }
        }
        self.status.claimed = true;
        self.status.claim_error = first_err.clone();
        self.refresh(now);
        if let Some(e) = first_err {
            return Err(crate::error::err(ErrorCode::DynamicLightingClaim, e));
        }
        if changed == 0 {
            log::info!("Dynamic Lighting was already off for the RGB controller");
        }
        Ok(changed)
    }

    fn wait(&mut self, timeout: Duration, wake: Option<&Signal>) -> bool {
        self.backend.wait_change(timeout, wake)
    }
}

enum DlCmd {
    Claim,
}

/// Background thread `fanrgb-dl`: reads at start, then on every registry change (or every 5 s)
/// and handles claims. Owned by the lighting thread, which copies [`DlMonitor::status`] into
/// `RgbStatus`.
pub struct DlMonitor {
    tx: Sender<DlCmd>,
    status: Arc<Mutex<DynamicLightingStatus>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    /// Ends the thread's wait at once for a claim or shutdown (otherwise it blocks until the next
    /// registry change or [`POLL`]).
    wake: Option<Arc<Signal>>,
}

impl Drop for DlMonitor {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(w) = &self.wake {
            w.set();
        }
    }
}

impl DlMonitor {
    pub fn start(backend: Box<dyn DlBackend>) -> DlMonitor {
        let (tx, rx) = std::sync::mpsc::channel();
        let status = Arc::new(Mutex::new(DynamicLightingStatus::default()));
        let st = status.clone();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let quit = stop.clone();
        let wake = Signal::new().map(Arc::new);
        let woken = wake.clone();
        let spawned = std::thread::Builder::new()
            .name("fanrgb-dl".into())
            .spawn(move || Self::run(DlCore::new(backend), rx, st, quit, woken));
        if let Err(e) = spawned {
            log::warn!("Dynamic Lighting monitor could not start: {e}");
        }
        DlMonitor { tx, status, stop, wake }
    }

    fn run(
        mut core: DlCore,
        rx: Receiver<DlCmd>,
        out: Arc<Mutex<DynamicLightingStatus>>,
        quit: Arc<std::sync::atomic::AtomicBool>,
        wake: Option<Arc<Signal>>,
    ) {
        let publish = |core: &DlCore| {
            let mut s = out.lock().unwrap_or_else(|p| p.into_inner());
            let fw = s.firmware_raw;
            *s = core.status().clone();
            s.firmware_raw = fw; // owned by the lighting thread
        };
        core.refresh(Instant::now());
        publish(&core);
        let mut last_full = Instant::now();
        loop {
            if quit.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            let mut dirty = false;
            loop {
                match rx.try_recv() {
                    Ok(DlCmd::Claim) => {
                        let _ = core.claim(Instant::now());
                        dirty = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
            // Blocks until a registry change, a claim / quit (`wake`) or the next full re-read; a
            // claim just handled is published first.
            let left = POLL.saturating_sub(last_full.elapsed()).max(Duration::from_millis(1));
            let changed = !dirty && core.wait(left, wake.as_deref());
            if changed || dirty || last_full.elapsed() >= POLL {
                last_full = Instant::now();
                core.refresh(Instant::now());
                publish(&core);
            }
        }
    }

    pub fn status(&self) -> DynamicLightingStatus {
        self.status.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The lighting thread reports the controller's `[CC 48]` reply byte.
    pub fn set_firmware(&self, raw: Option<u8>) {
        self.status.lock().unwrap_or_else(|p| p.into_inner()).firmware_raw = raw;
    }

    pub fn claim(&self) {
        let _ = self.tx.send(DlCmd::Claim);
        if let Some(w) = &self.wake {
            w.set();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(ambient: Option<u32>) -> DlDevice {
        DlDevice {
            key: "HID#VID_048D&PID_5702&MI_00#7&2f0&0&0000#{47c9e1a5-1f5a-4c2a-a5a8-e1c1c0a4d4c1}".into(),
            ambient,
            providers: vec!["WindowsLighting".into(), "RazerDynamicLighting_qemfkr3nbbywc".into()],
        }
    }

    #[test]
    fn state_follows_the_registry_not_the_firmware() {
        assert_eq!(evaluate(&DlSnapshot::default()), DlState::Unavailable);
        let off = DlSnapshot { global_ambient: Some(0), devices: vec![dev(Some(0))], ..Default::default() };
        assert_eq!(evaluate(&off), DlState::Off);
        let dev_off = DlSnapshot { global_ambient: Some(1), devices: vec![dev(Some(0))], ..Default::default() };
        assert_eq!(evaluate(&dev_off), DlState::Off);
        let global_off = DlSnapshot { global_ambient: Some(0), devices: vec![dev(Some(1))], ..Default::default() };
        assert_eq!(evaluate(&global_off), DlState::Off);
        let on = DlSnapshot { global_ambient: Some(1), devices: vec![dev(Some(1))], ..Default::default() };
        assert_eq!(evaluate(&on), DlState::Controlling);
        let unset = DlSnapshot { global_ambient: None, devices: vec![dev(None)], ..Default::default() };
        assert_eq!(evaluate(&unset), DlState::Controlling, "unset = Windows default (on)");

        // The reference PC: registry off, firmware reply "armed" → no banner.
        let st = DynamicLightingStatus { state: DlState::Off, firmware_raw: Some(0), ..Default::default() };
        assert!(!st.conflict());
        assert!(st.banner().is_none());
        assert_eq!(st.chip_label(), "Dynamic Lighting: off · firmware: LampArray armed");
        let st = DynamicLightingStatus { state: DlState::Controlling, firmware_raw: Some(1), ..Default::default() };
        assert!(st.conflict() && st.banner().is_some());
        assert_eq!(FirmwareDl::from_raw(99), FirmwareDl::Unsupported);
    }

    #[test]
    fn device_keys_are_matched_like_gcc() {
        assert!(is_fusion_device("HID#VID_048D&PID_5702&MI_00#7&2f0&0&0000#{x}"));
        assert!(is_fusion_device("hid#vid_048d&pid_5711&mi_00#1"));
        assert!(!is_fusion_device("HID#VID_1532&PID_0099&MI_00#1"));
        assert_eq!(short_key("HID#VID_048D&PID_5702&MI_00#7&2f0#{x}"), "VID_048D&PID_5702&MI_00");
    }

    /// Claim writes 0 only where needed, the watch notices Windows turning it back on, and
    /// turning it off again clears the warning.
    #[test]
    fn claim_then_retake_then_release() {
        let fake = FakeBackend::default();
        *fake.snap.lock().unwrap() = DlSnapshot {
            global_ambient: Some(1),
            devices: vec![dev(Some(1)), DlDevice { key: "HID#VID_048D&PID_5702&MI_00#other".into(), ..dev(Some(0)) }],
            ..Default::default()
        };
        *fake.running.lock().unwrap() = vec!["razerwdl.exe".into(), "explorer.exe".into()];
        let mut core = DlCore::new(Box::new(fake.clone()));
        let t0 = Instant::now();
        core.refresh(t0);
        assert_eq!(core.status().state, DlState::Controlling);
        assert_eq!(core.status().contenders.len(), 1);
        assert!(core.status().banner().unwrap().1.contains("Razer"));
        assert_eq!(core.claim(t0).unwrap(), 1);
        assert_eq!(fake.writes.lock().unwrap().len(), 1, "only the device that was on");
        assert_eq!(fake.writes.lock().unwrap()[0].1, 0);
        assert_eq!(core.status().state, DlState::Off);
        assert!(core.status().claimed && !core.status().retaken);
        // Windows Settings switches the device back on.
        fake.snap.lock().unwrap().devices[0].ambient = Some(1);
        core.refresh(t0);
        assert!(core.status().retaken);
        assert_eq!(core.status().banner().unwrap().0, "Windows took the LEDs back");
        fake.snap.lock().unwrap().devices[0].ambient = Some(0);
        core.refresh(t0);
        assert!(!core.status().retaken && !core.status().conflict());
        assert!(core.status().summary().contains("claimed by FanRGB"));
    }

    #[test]
    fn claim_failures_are_coded() {
        let fake = FakeBackend::default();
        let mut core = DlCore::new(Box::new(fake.clone()));
        let e = core.claim(Instant::now()).unwrap_err();
        assert_eq!(crate::error::code_of(&e), ErrorCode::DynamicLightingClaim);
        *fake.snap.lock().unwrap() =
            DlSnapshot { global_ambient: Some(1), devices: vec![dev(Some(1))], ..Default::default() };
        *fake.fail_writes.lock().unwrap() = true;
        let e = core.claim(Instant::now()).unwrap_err();
        assert_eq!(crate::error::code_of(&e), ErrorCode::DynamicLightingClaim);
        assert!(core.status().claim_error.is_some());
    }

    #[test]
    fn monitor_thread_publishes_and_claims() {
        let fake = FakeBackend::demo();
        fake.snap.lock().unwrap().devices[0].ambient = Some(1);
        fake.snap.lock().unwrap().global_ambient = Some(1);
        let m = DlMonitor::start(Box::new(fake.clone()));
        m.set_firmware(Some(0));
        let wait_for = |f: &dyn Fn(&DynamicLightingStatus) -> bool| {
            let end = Instant::now() + Duration::from_secs(5);
            while Instant::now() < end {
                if f(&m.status()) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            false
        };
        assert!(wait_for(&|s| s.state == DlState::Controlling));
        m.claim();
        assert!(wait_for(&|s| s.state == DlState::Off && s.claimed));
        assert_eq!(m.status().firmware_raw, Some(0), "firmware byte survives publishes");
        assert!(m.status().simulated);
    }

    /// The monitor thread blocks between polls (it used to wake 50 times a second in `--demo`) and a
    /// claim still wakes it at once.
    #[test]
    fn monitor_thread_blocks_until_poll_or_claim() {
        let fake = FakeBackend::demo();
        let reads = fake.reads.clone();
        let m = DlMonitor::start(Box::new(fake.clone()));
        let t0 = Instant::now();
        while reads.load(std::sync::atomic::Ordering::SeqCst) == 0 && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1, "only the start-up read before the first poll");
        fake.snap.lock().unwrap().devices[0].ambient = Some(1);
        fake.snap.lock().unwrap().global_ambient = Some(1);
        let t1 = Instant::now();
        m.claim();
        while !m.status().claimed && t1.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(m.status().claimed);
        assert!(t1.elapsed() < Duration::from_secs(2), "a claim does not wait for the next poll");
        assert_eq!(fake.writes.lock().unwrap().len(), 1);
    }

    /// Read-only: the real registry can be read on this machine (no writes in tests).
    #[test]
    fn registry_read_works() {
        if crate::util::win::skip_without_unelevated_shell() {
            return;
        }
        let mut b = RegistryBackend::default();
        let s = b.read();
        assert!(s.hive_error.is_none(), "{:?}", s.hive_error);
        assert!(s.devices.iter().all(|d| is_fusion_device(&d.key)));
        assert!(!b.hive().is_empty());
        // The desktop user's hive reads the same as this (unelevated) test process's HKCU.
        assert_eq!(read_hive(crate::util::win::HKEY_CURRENT_USER), s);
    }

    /// Review M4: when the desktop user's registry cannot be found the state is "unknown", never
    /// "off" or "not available" from another account's hive, no banner is raised and nothing is
    /// written.
    #[test]
    fn unknown_desktop_user_is_unknown_not_off() {
        let fake = FakeBackend::default();
        *fake.snap.lock().unwrap() = DlSnapshot { hive_error: Some("no desktop shell".into()), ..Default::default() };
        let mut core = DlCore::new(Box::new(fake.clone()));
        core.refresh(Instant::now());
        assert_eq!(core.status().state, DlState::UserUnknown);
        assert!(!core.status().conflict() && core.status().banner().is_none());
        assert!(core.status().chip_label().contains("unknown"));
        let e = core.claim(Instant::now()).unwrap_err();
        assert_eq!(crate::error::code_of(&e), ErrorCode::DynamicLightingClaim);
        assert!(fake.writes.lock().unwrap().is_empty());
    }
}
