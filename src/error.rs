//! Stable, user-facing error codes.
//!
//! Every failure the user can see carries an [`ErrorCode`] (`FRG-E104`, ...) with a plain-language
//! explanation and, where possible, a [`FixAction`] the UI can offer as a button. Codes are stable:
//! never renumber one, only add new ones.
//!
//! Internally the app keeps using `anyhow::Result`; an [`AppError`] travels inside the `anyhow::Error`
//! and [`code_of`] recovers it anywhere up the stack (it walks the whole cause chain, so
//! `.context(...)` on top of a coded error keeps the code).

use std::fmt;

/// What the user can do about an error. The UI maps each to a button.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FixAction {
    None,
    InstallPawnIo,
    RestartAsAdmin,
    /// Shut down, switch the PSU off, hold power 15 s (clears a hung USB RGB controller).
    PowerCycle,
    ReconnectRgb,
    CloseConflictingSoftware,
    RecalibrateFan,
    OpenLogFolder,
    ResetAiModel,
    ResetSettings,
    /// Move a squatted/blocked `%ProgramData%\FanRGB` aside and create a protected one
    /// (`crate::system::repair_state_folder`).
    RepairStateFolder,
}

impl FixAction {
    pub fn label(self) -> &'static str {
        match self {
            FixAction::None => "",
            FixAction::InstallPawnIo => "Install PawnIO",
            FixAction::RestartAsAdmin => "Restart as administrator",
            FixAction::PowerCycle => "How to power-cycle",
            FixAction::ReconnectRgb => "Reconnect",
            FixAction::CloseConflictingSoftware => "Show conflicting apps",
            FixAction::RecalibrateFan => "Recalibrate fan",
            FixAction::OpenLogFolder => "Open log folder",
            FixAction::ResetAiModel => "Reset learned model",
            FixAction::ResetSettings => "Reset settings",
            FixAction::RepairStateFolder => "Repair data folder",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warning,
    Error,
    Critical,
}

macro_rules! error_codes {
    ($( $variant:ident = $num:literal, $sev:ident, $fix:ident, $title:literal, $help:literal; )*) => {
        /// Stable error codes. The number is shown to the user as `FRG-E<num>`.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        pub enum ErrorCode { $( $variant, )* }

        impl ErrorCode {
            #[allow(dead_code)] // tests and documentation tooling
            pub const ALL: &'static [ErrorCode] = &[$( ErrorCode::$variant, )*];
            pub fn number(self) -> u16 { match self { $( ErrorCode::$variant => $num, )* } }
            pub fn severity(self) -> Severity { match self { $( ErrorCode::$variant => Severity::$sev, )* } }
            pub fn fix(self) -> FixAction { match self { $( ErrorCode::$variant => FixAction::$fix, )* } }
            /// Short headline, e.g. for a toast.
            pub fn title(self) -> &'static str { match self { $( ErrorCode::$variant => $title, )* } }
            /// What it means and what to do, in plain language.
            pub fn help(self) -> &'static str { match self { $( ErrorCode::$variant => $help, )* } }
        }
    };
}

error_codes! {
    // 1xx: driver and privileges
    PawnIoMissing = 100, Error, InstallPawnIo, "PawnIO driver not installed",
        "Fan control and motherboard/CPU temperatures need the signed PawnIO driver. Install it (winget install namazso.PawnIO) and restart the app.";
    NotElevated = 101, Warning, RestartAsAdmin, "Administrator rights needed",
        "Talking to the fan controller requires administrator rights. Lighting and GPU/CPU load still work without them.";
    PawnIoModule = 102, Error, OpenLogFolder, "PawnIO module rejected",
        "The PawnIO driver refused to load a hardware module. Update PawnIO to the latest version.";
    PawnIoCall = 103, Error, OpenLogFolder, "Hardware call failed",
        "A PawnIO call to the hardware failed. If this repeats, write a diagnostics report.";
    BusBusy = 104, Warning, CloseConflictingSoftware, "Hardware bus busy",
        "Another program held the Super I/O or PCI bus for too long. Monitoring tools usually release it quickly; fan control software must not run at the same time.";
    PawnIoOutdated = 105, Warning, InstallPawnIo, "PawnIO is outdated",
        "The installed PawnIO is older than this app supports. Update it.";

    // 2xx: Super I/O and fans
    NoSuperIo = 200, Error, OpenLogFolder, "No supported fan controller found",
        "No ITE Super I/O chip was detected. Fans stay under BIOS control.";
    ChipInvalid = 201, Error, OpenLogFolder, "Fan controller answered unexpectedly",
        "The Super I/O chip returned an invalid ID or configuration. Another driver may be using it.";
    RegisterMismatch = 202, Error, OpenLogFolder, "Fan controller write not confirmed",
        "A register write did not read back correctly. Control of this fan was returned to the BIOS.";
    NoSuchFan = 203, Error, None, "Fan channel does not exist",
        "The requested fan header does not exist on this board.";
    FanReadOnly = 204, Warning, None, "Fan is monitor-only",
        "This fan channel can be monitored but not controlled on this board.";
    RestoreFailed = 205, Critical, OpenLogFolder, "Could not return fan to BIOS control",
        "Restoring the firmware fan settings failed. Reboot to be safe; the BIOS reapplies its settings at startup.";
    FirmwareOverride = 206, Info, None, "Firmware changed a fan setting",
        "The BIOS/EC reset a fan (often after sleep). The app re-applied its setting.";
    FanStalled = 207, Critical, RecalibrateFan, "Fan not spinning",
        "The fan reports 0 RPM although it should spin. Check the cable, or recalibrate if it uses a 0-RPM mode.";
    CalibrationFailed = 208, Warning, RecalibrateFan, "Fan calibration failed",
        "The fan did not respond as expected during calibration. Make sure nothing blocks it and try again.";
    IdentifyStopped = 209, Warning, None, "Header identification stopped",
        "The header identification wizard stopped (the CPU got warm, fans were handed to the BIOS, or it was cancelled). Every header is back on its normal control; roles you already confirmed are kept. Try again when the PC is idle.";

    // 3xx: sensors and thermals
    SensorLost = 300, Warning, None, "Temperature sensor lost",
        "A temperature source stopped reporting. Fans that depend on it were returned to BIOS control.";
    CriticalTemperature = 301, Critical, None, "Critical temperature",
        "A component reached its critical temperature. All controlled fans run at 100 % until it cools down.";
    CpuSensor = 302, Warning, OpenLogFolder, "CPU temperature unavailable",
        "Reading the AMD CPU temperature failed. Curves using the CPU fall back to the board's CPU sensor.";
    GpuSensor = 303, Info, None, "GPU sensors unavailable",
        "The GPU driver does not expose temperature or load. GPU-based curves are disabled.";
    ImplausibleReading = 304, Warning, None, "Implausible sensor value",
        "A sensor reported an impossible value and was ignored.";
    CoolingIneffective = 305, Critical, None, "CPU cooling may have failed",
        "The CPU runs hot although its fan or pump is driven hard (or the CPU is nearly idle), and that header has no speed signal, so a stopped fan or pump cannot be seen directly. FanRGB runs the CPU fans and pump at 100 %. Check that the CPU fan and the AIO pump are spinning.";
    CoolingDegraded = 309, Critical, None, "Cooling performance degraded (pump or airflow failure?)",
        "At the same power and fan speed the CPU now runs much hotter above the board temperature than FanRGB learned for this PC. A slowing or air-locked pump, a stopped radiator fan or a blocked filter look like this. This alert only informs (v0.2): FanRGB does not change the fan speeds for it. Check that the pump runs and the radiator fans spin, or choose Full speed until it clears.";

    // 4xx: RGB lighting
    RgbNotFound = 400, Info, ReconnectRgb, "RGB controller not found",
        "No GIGABYTE RGB Fusion 2 USB controller was found. Lighting preview still works.";
    RgbHung = 401, Error, PowerCycle, "RGB controller is not responding",
        "Windows reports the RGB controller as failed (USB error 43). Shut down, switch the PSU off for 30 s, then start again.";
    RgbOpenFailed = 402, Error, CloseConflictingSoftware, "RGB controller is in use",
        "The RGB controller could not be opened. Close other RGB software (RGB Fusion, OpenRGB, SignalRGB, GCC).";
    RgbWriteFailed = 403, Warning, ReconnectRgb, "RGB write failed",
        "Sending data to the RGB controller failed. The app will retry and reconnect automatically.";
    RgbBadReply = 404, Warning, ReconnectRgb, "RGB controller sent a bad reply",
        "The RGB controller returned a short or malformed report.";
    RgbDisconnected = 405, Warning, ReconnectRgb, "RGB controller disconnected",
        "The RGB controller went away. Reconnecting automatically.";
    RgbUnsupported = 406, Warning, None, "Unsupported RGB controller",
        "This RGB controller model or firmware is not supported; some features are disabled.";
    RgbFlashFailed = 407, Error, ReconnectRgb, "Saving lighting to controller failed",
        "Writing the effect to the controller's memory failed. The live effect still works.";
    AudioCapture = 408, Warning, None, "Audio capture unavailable",
        "The music visualizer could not open the default audio output.";
    DynamicLightingActive = 409, Warning, None, "Windows Dynamic Lighting controls the LEDs",
        "Windows Dynamic Lighting (or an app that uses it, such as Razer Synapse) is enabled for the RGB controller and can override FanRGB's colours. Use “Let FanRGB control these LEDs” on the Lighting page, or turn the device off in Windows Settings › Personalization › Dynamic Lighting.";
    DynamicLightingClaim = 410, Warning, OpenLogFolder, "Could not take the LEDs back from Windows",
        "FanRGB could not switch Dynamic Lighting off for the RGB controller in the registry. Turn it off in Windows Settings › Personalization › Dynamic Lighting instead.";

    // 5xx: learning controller
    AiModelReset = 500, Info, None, "Thermal model reset",
        "The learned thermal model was reset and will relearn your system.";
    AiModelCorrupt = 501, Warning, ResetAiModel, "Thermal model file damaged",
        "A saved thermal model could not be read and was replaced with a fresh one.";
    AiModelDistrust = 502, Info, ResetAiModel, "Learned model distrusted",
        "The learned thermal model predicted badly, so the AI fell back to its conservative safety curve until the model proves itself again. This happens after hardware or airflow changes.";
    HwCurveFallback = 503, Info, OpenLogFolder, "Hardware fan curve not used",
        "The fan controller did not accept a hardware curve for this fan, so the app runs the curve in software instead.";
    FansReleased = 504, Info, None, "Fans handed back to BIOS",
        "All fans run on the BIOS/firmware curves. Select a fan profile to resume control.";
    ProfileSwitched = 505, Info, None, "Fan profile changed",
        "The active fan profile was changed.";

    // 6xx: system
    ConfigLoad = 600, Warning, ResetSettings, "Settings could not be loaded",
        "Some settings could not be read. Values this version does not understand were reset to their defaults one by one; a file that is not valid JSON at all was kept as config.json.bad and replaced by defaults.";
    ConfigSave = 601, Error, OpenLogFolder, "Settings could not be saved",
        "Writing the settings file failed. Check disk space and permissions.";
    Autostart = 602, Error, OpenLogFolder, "Start-with-Windows failed",
        "Creating or removing the startup task failed.";
    AlreadyRunning = 603, Info, None, "Already running",
        "Another FanRGB instance is running; it was brought to the front.";
    Watchdog = 604, Warning, OpenLogFolder, "Safety watchdog unavailable",
        "The crash watchdog could not start. Fans are still restored on normal exit.";
    ConflictingSoftware = 605, Warning, CloseConflictingSoftware, "Conflicting software running",
        "Another fan or RGB program is running and may fight over the hardware.";
    Io = 606, Warning, OpenLogFolder, "File access failed",
        "A file could not be read or written.";
    InstanceConflict = 607, Warning, OpenLogFolder, "Startup marker blocked",
        "Another program created FanRGB's single-instance marker. FanRGB did not start a second copy; if this repeats, scan for malware and restart Windows.";
    InstallRequired = 608, Warning, None, "Install FanRGB first",
        "Start with Windows runs FanRGB with administrator rights, so it may only start a copy in Program Files that normal programs cannot replace. Install FanRGB there first.";
    InstallFailed = 609, Error, OpenLogFolder, "Installing FanRGB failed",
        "Copying FanRGB to Program Files failed. Make sure you approved the administrator prompt and try again.";
    StateFolder = 610, Error, RepairStateFolder, "Data folder blocked, fan control is off",
        "Another program created or locked FanRGB's data folder in ProgramData, so FanRGB cannot keep its crash-recovery record there. Without that record FanRGB does not take control of fans that have no hardware safety limit; they stay on the BIOS curve. Click Repair to move the blocked folder aside and create a protected one. If it keeps coming back, scan for malware and restart Windows.";
    UnelevatedLaunch = 611, Warning, None, "Could not start the program without administrator rights",
        "FanRGB runs as administrator and only starts other programs (browser, Explorer, winget, a restart of itself) with your normal rights through the Windows desktop. The desktop is not running, or it runs as administrator itself, so nothing was started. Start the program yourself.";
    RestartFailed = 612, Warning, None, "Could not restart FanRGB",
        "FanRGB could not start a new copy of itself. Exit FanRGB from the tray icon and start it again.";
    WingetUnavailable = 613, Warning, None, "winget is not available",
        "The Windows Package Manager (winget) was not found or failed a safety check. Download PawnIO from pawnio.eu instead.";
    Hardening = 614, Warning, OpenLogFolder, "Process protection incomplete",
        "Windows refused one of the protections FanRGB enables for itself (safe DLL loading or Redirection Guard). FanRGB still works; update Windows to get the full protection.";

    // 72x: driver inventory and update check (inform only; 70x is reserved for the app updater)
    DriverInventory = 720, Warning, None, "Driver list incomplete",
        "Windows did not return the list of installed drivers, so some rows may be missing. Device Manager shows the same information.";
    DriverCheckOffline = 721, Info, None, "No internet connection",
        "Windows Update could not be reached, so newer drivers could not be looked up. Installed versions are still shown; check again when you are online.";
    DriverCheckMetered = 722, Info, None, "Skipped on a metered connection",
        "The automatic driver check does not run on a metered connection. Use Check now to check anyway.";
    DriverCheckServiceOff = 723, Warning, None, "Windows Update is turned off",
        "The Windows Update service is disabled or blocked by a policy, so newer drivers cannot be looked up. Installed versions are still shown.";
    DriverCheckTimeout = 724, Warning, None, "Windows Update did not answer in time",
        "The driver check was stopped after 120 seconds. Windows Update may be busy with another search or an installation; try again later.";
    DriverCheckFailed = 725, Warning, OpenLogFolder, "Driver check failed",
        "Windows Update returned an error while looking for driver updates. The error number is in the details and the log.";
    DriverCheckCancelled = 726, Info, None, "Driver check cancelled",
        "The driver check stopped before it finished, so its result was not used. The previous result is still shown.";
    CatalogFetch = 727, Warning, None, "Could not get the vendor driver list",
        "FanRGB could not download its signed list of the latest vendor driver versions from the FanRGB repository (offline, GitHub unreachable, or the list is not published yet). The last list FanRGB verified is still shown; it tries again with the next update check.";
    CatalogRejected = 728, Error, None, "Vendor driver list refused",
        "The downloaded list of vendor driver versions was not signed with FanRGB's catalog key, is older than a list already seen (a replayed list), or contains invalid data. It was not used; the last verified list is still shown. Nothing is ever installed from this list.";
    CatalogOutOfDate = 729, Info, None, "Vendor driver list out of date",
        "The signed list of vendor driver versions has passed its expiry date, so vendors may have released newer drivers since. It is still shown; check again when you are online, or open the vendor page.";
    // 7xx: app self-update
    UpdateTokenMissing = 700, Warning, None, "Update token unusable",
        "The update access token saved for this Windows account could not be read, saved or removed. Paste it again in Settings › Updates, or choose Forget there to check without a token (a token is optional: it is only needed while the repository is private, or to raise GitHub's rate limit). Fan control is not affected.";
    UpdateAuth = 701, Warning, None, "Update token rejected",
        "GitHub refused the update token: it expired, was revoked or has no read access to the FanRGB repository. Create a new token and paste it in Settings › Updates, or choose Forget there to check without one. Until then FanRGB checks at most once a day. Fan control is not affected.";
    UpdateNetwork = 702, Warning, None, "Could not check for updates",
        "GitHub could not be reached or answered unexpectedly. FanRGB tries again later; fan control is not affected.";
    UpdateRepository = 703, Error, None, "Update source is not the FanRGB repository",
        "GitHub answered with a repository other than the one this FanRGB build trusts (its id differs). Nothing was downloaded or installed.";
    UpdateSignature = 704, Error, None, "Update signature invalid",
        "The update is not signed with a FanRGB release key, or the signature does not match. Nothing was installed.";
    UpdateDowngrade = 705, Error, None, "Update is not newer",
        "The offered version is not newer than the installed one, or it is older than a release FanRGB has already seen (a replayed release). Nothing was installed.";
    UpdateHash = 706, Error, None, "Update download damaged",
        "The downloaded program does not match the size or SHA-256 in the signed release manifest. Nothing was installed; check again later.";
    UpdatePackage = 707, Error, None, "Update package invalid",
        "The release has no signed manifest, the manifest is malformed or needs a newer installed version first, or the program is not a 64-bit Windows application. Nothing was installed.";
    UpdateInstall = 708, Error, OpenLogFolder, "Installing the update failed",
        "Replacing FanRGB in Program Files failed; the previous version was kept.";
    UpdateRolledBack = 709, Warning, OpenLogFolder, "Update rolled back",
        "The new version did not start correctly within 3 minutes, so FanRGB went back to the previous version. The log has the details.";
    UpdateDevBuild = 710, Info, None, "Update verified, not installed",
        "This FanRGB runs from a development folder, not from Program Files. The update was downloaded and verified, but only an installed FanRGB replaces itself.";
    UpdateHelper = 711, Warning, OpenLogFolder, "Update check could not run",
        "The update helper, which runs without administrator rights, could not be started or did not report back.";
    UpdateStateFolder = 712, Warning, RepairStateFolder, "Update needs FanRGB's data folder",
        "FanRGB is using a temporary data folder because its folder in ProgramData is blocked, so it cannot hand over to the new version safely. Nothing was installed. Click Repair, restart FanRGB, then install the update.";
    UpdateChanged = 713, Info, None, "Update changed, check again",
        "Another check replaced the update after it was shown, so a different release is waiting than the one you chose. Nothing was installed; check again and review the new version.";
    UpdateNotPublic = 714, Warning, None, "Update repository not reachable",
        "GitHub did not show the FanRGB repository to a check without a token (HTTP 401 or 404): the repository is private, or it was moved or removed. Nothing was downloaded. If you have a read-only access token for it, paste it in Settings › Updates. FanRGB checks at most once a day until then; fan control is not affected.";
    UpdateRateLimit = 715, Info, None, "GitHub rate limit reached",
        "GitHub limits how often one internet address may use its API without a token (60 requests an hour, shared with other programs on your network), and refused this check (HTTP 403 or 429). Nothing was downloaded; FanRGB tries again with the next daily check. A read-only access token in Settings › Updates raises the limit. Fan control is not affected.";

    // 9xx: internal
    Internal = 900, Error, OpenLogFolder, "Internal error",
        "Something unexpected happened. Fans were returned to safe settings. Please send the log file.";
    Unknown = 999, Error, OpenLogFolder, "Unexpected error",
        "An error without a specific code occurred. Details are in the log.";
}

impl ErrorCode {
    /// `FRG-E104`.
    pub fn id(self) -> String {
        format!("FRG-E{:03}", self.number())
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FRG-E{:03}", self.number())
    }
}

/// A coded error. `detail` is the technical message (register, Win32 error, ...).
#[derive(Debug, Clone)]
pub struct AppError {
    pub code: ErrorCode,
    pub detail: String,
}

impl AppError {
    pub fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        AppError { code, detail: detail.into() }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.detail.is_empty() {
            write!(f, "[{}] {}", self.code, self.code.title())
        } else {
            write!(f, "[{}] {}: {}", self.code, self.code.title(), self.detail)
        }
    }
}

impl std::error::Error for AppError {}

/// Builds an `anyhow::Error` carrying `code`.
pub fn err(code: ErrorCode, detail: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(AppError::new(code, detail))
}

/// `return Err(coded error)`, like `anyhow::bail!`.
#[macro_export]
macro_rules! bail_code {
    ($code:expr, $($arg:tt)*) => {
        return Err($crate::error::err($code, format!($($arg)*)))
    };
}

/// The first coded error in the cause chain, or [`ErrorCode::Unknown`].
pub fn code_of(e: &anyhow::Error) -> ErrorCode {
    // `anyhow::Error::downcast_ref` also sees an `AppError` attached with `.context(..)`, which the
    // cause chain hides inside anyhow's private wrapper.
    if let Some(a) = e.downcast_ref::<AppError>() {
        return a.code;
    }
    e.chain().find_map(|c| c.downcast_ref::<AppError>().map(|a| a.code)).unwrap_or(ErrorCode::Unknown)
}

/// Attaches a code to any error that does not already carry one.
pub trait WithCode<T> {
    fn code(self, code: ErrorCode) -> anyhow::Result<T>;
}

impl<T, E: Into<anyhow::Error>> WithCode<T> for Result<T, E> {
    fn code(self, code: ErrorCode) -> anyhow::Result<T> {
        self.map_err(|e| {
            let e: anyhow::Error = e.into();
            if code_of(&e) != ErrorCode::Unknown { e } else { e.context(AppError::new(code, "")) }
        })
    }
}

impl<T> WithCode<T> for Option<T> {
    fn code(self, code: ErrorCode) -> anyhow::Result<T> {
        self.ok_or_else(|| err(code, ""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_and_documented() {
        let mut seen = std::collections::HashSet::new();
        for c in ErrorCode::ALL {
            assert!(seen.insert(c.number()), "duplicate {}", c.id());
            assert!(!c.title().is_empty() && !c.help().is_empty());
        }
    }

    #[test]
    fn code_survives_context() {
        let e: anyhow::Result<()> = Err(err(ErrorCode::BusBusy, "ISA"));
        let e = anyhow::Context::context(e, "setting fan 2").unwrap_err();
        assert_eq!(code_of(&e), ErrorCode::BusBusy);
        assert_eq!(code_of(&anyhow::anyhow!("plain")), ErrorCode::Unknown);
    }

    #[test]
    fn with_code_keeps_inner_code() {
        let inner: anyhow::Result<()> = Err(err(ErrorCode::RgbHung, ""));
        assert_eq!(code_of(&inner.code(ErrorCode::RgbWriteFailed).unwrap_err()), ErrorCode::RgbHung);
        let io: Result<(), std::io::Error> = Err(std::io::Error::other("x"));
        assert_eq!(code_of(&io.code(ErrorCode::Io).unwrap_err()), ErrorCode::Io);
        assert_eq!(code_of(&None::<u8>.code(ErrorCode::NoSuchFan).unwrap_err()), ErrorCode::NoSuchFan);
    }

    #[test]
    fn id_format() {
        assert_eq!(ErrorCode::PawnIoMissing.id(), "FRG-E100");
        assert_eq!(ErrorCode::BusBusy.to_string(), "FRG-E104");
    }

    /// Independent tester (2026-11-06): README's error table lists exactly the codes defined here, with
    /// the same titles and severities (so FRG-E700's new title, FRG-E714 and FRG-E715 are documented).
    #[test]
    fn tester_readme_error_table_matches_the_codes() {
        let readme = include_str!("../README.md");
        let mut rows = std::collections::BTreeMap::new();
        for line in readme.lines() {
            let Some(rest) = line.strip_prefix("| FRG-E") else { continue };
            let cells: Vec<&str> = rest.split('|').map(str::trim).collect();
            let n: u16 = cells[0].parse().unwrap_or_else(|_| panic!("bad row {line}"));
            assert!(rows.insert(n, (cells[1].to_string(), cells[2].to_string())).is_none(), "FRG-E{n} twice");
        }
        for c in ErrorCode::ALL {
            let (title, sev) = rows.remove(&c.number()).unwrap_or_else(|| panic!("{} is not in README", c.id()));
            assert_eq!(title, c.title(), "{}", c.id());
            assert_eq!(sev, format!("{:?}", c.severity()), "{}", c.id());
        }
        assert!(rows.is_empty(), "README lists codes that do not exist: {rows:?}");
        // The new codes' help texts say what to do without a token.
        assert!(ErrorCode::UpdateNotPublic.help().contains("Settings › Updates"));
        assert!(ErrorCode::UpdateRateLimit.help().contains("Settings › Updates"));
        assert!(ErrorCode::UpdateTokenMissing.help().contains("Forget"));
    }
}
