//! Phase 2: the Windows Update driver search.
//!
//! Two halves:
//! - **Helper** (`fanrgb --driver-check`, [`helper_main`]): an unelevated process that never touches
//!   hardware or the engine. It asks the Network List Manager whether the PC is online/metered, runs
//!   `IUpdateSearcher::Search("IsInstalled=0 and Type='Driver'")` against Windows Update with a time
//!   limit, and writes a [`CheckReport`] (JSON) to `--out` or prints it. Search only: no
//!   `IUpdateDownloader`/`IUpdateInstaller`, nothing is downloaded or installed.
//! - **App side** ([`run_helper`]): starts the helper with the desktop user's normal rights (in a
//!   kill-on-close job, so it never outlives the app), kills it after 120 s, and validates its report
//!   ([`super::parse_report`]). The elevated app never loads `wuapi.dll` and never opens a network
//!   connection for this.
//!
//! The report lives in the desktop user's `%LOCALAPPDATA%\FanRGB`, a folder that user (and any program
//! they run) controls. The elevated app reads it and removes it only through one verified handle
//! ([`crate::util::fsx::take_limited`]), never by name (a junction to `\RPC Control` plus an
//! object-manager link would turn a by-name delete into "delete any file"). Leftovers of interrupted
//! checks are swept by the next helper, with the user's own rights ([`sweep_leftovers`]).
//!
//! The COM calls sit behind [`UpdateSearch`] and [`NetworkProbe`], so [`run_check`] is tested with mocks.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{
    CheckError, CheckReport, MAX_REPORT_BYTES, REPORT_SCHEMA, WuOffer, ole_date_to_ymd, parse_report,
    version_from_title,
};
use crate::error::ErrorCode;

/// The app kills the helper after this long.
pub const HELPER_TIMEOUT: Duration = Duration::from_secs(120);
/// A report or temp file older than this belongs to no running check (see [`sweep_leftovers`]).
const LEFTOVER_AGE: Duration = Duration::from_secs(600);
/// The helper gives up on its own a little earlier, so it can still report the timeout.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(110);

pub const SEARCH_CRITERIA: &str = "IsInstalled=0 and Type='Driver'";

/// One update as the update agent describes it (before cleaning up).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RawUpdate {
    pub title: String,
    pub update_id: String,
    pub hardware_id: String,
    pub model: String,
    pub provider: String,
    pub manufacturer: String,
    pub class: String,
    /// `DriverVerDate` (OLE automation date).
    pub ver_date: Option<f64>,
}

impl RawUpdate {
    pub fn to_offer(&self) -> WuOffer {
        WuOffer {
            title: self.title.trim().to_string(),
            version: version_from_title(&self.title),
            date: self.ver_date.and_then(ole_date_to_ymd),
            hardware_id: self.hardware_id.trim().to_string(),
            model: self.model.trim().to_string(),
            provider: self.provider.trim().to_string(),
            manufacturer: self.manufacturer.trim().to_string(),
            class: self.class.trim().to_string(),
            update_id: self.update_id.trim().to_string(),
        }
    }
}

/// A failed search: the HRESULT (if any) and a message.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchError {
    pub hresult: Option<u32>,
    pub message: String,
}

/// The Windows Update search (mockable).
pub trait UpdateSearch {
    fn search(&self) -> Result<Vec<RawUpdate>, SearchError>;
}

/// What the Network List Manager says (`None`: unknown).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NetState {
    pub internet: Option<bool>,
    pub metered: Option<bool>,
}

pub trait NetworkProbe {
    fn probe(&self) -> NetState;
}

/// Maps update-agent / WinHTTP / Win32 HRESULTs to user-facing codes.
pub fn code_for_hresult(hr: u32) -> ErrorCode {
    match hr {
        // Service disabled, update agent unavailable, Windows Update disabled by policy / for this user.
        0x8007_0422 | 0x8024_0001 | 0x8024_002E | 0x8024_0025 | 0x8024_A000 | 0x8024_0020 => {
            ErrorCode::DriverCheckServiceOff
        }
        // Name not resolved, cannot connect, connection reset/aborted, endpoint unreachable, no network.
        0x8024_402C | 0x8007_2EE7 | 0x8007_2EFD | 0x8007_2EFE | 0x8007_2EFF | 0x8024_0438 | 0x8007_04CF
        | 0x8024_4022 => ErrorCode::DriverCheckOffline,
        // WinHTTP / HTTP request / gateway timeouts.
        0x8007_2EE2 | 0x8024_401C | 0x8024_4023 | 0x8024_0024 => ErrorCode::DriverCheckTimeout,
        // ERROR_CANCELLED, WU_E_CALL_CANCELLED.
        0x8007_04C7 | 0x8024_0004 => ErrorCode::DriverCheckCancelled,
        _ => ErrorCode::DriverCheckFailed,
    }
}

/// The whole check, given the two system interfaces. `auto`: the scheduled check, which does not run on
/// a metered connection ("Check now" does).
pub fn run_check(net: &dyn NetworkProbe, wu: &dyn UpdateSearch, auto: bool) -> Result<Vec<WuOffer>, CheckError> {
    let n = net.probe();
    if auto && n.metered == Some(true) {
        return Err(CheckError::new(ErrorCode::DriverCheckMetered, "automatic check skipped"));
    }
    match wu.search() {
        Ok(updates) => {
            let mut offers: Vec<WuOffer> = updates.iter().map(RawUpdate::to_offer).collect();
            offers.truncate(200);
            Ok(offers)
        }
        Err(e) => {
            let mut code = e.hresult.map(code_for_hresult).unwrap_or(ErrorCode::DriverCheckFailed);
            // The agent's error for "no network" varies; the Network List Manager knows better.
            if n.internet == Some(false) && code != ErrorCode::DriverCheckServiceOff {
                code = ErrorCode::DriverCheckOffline;
            }
            let hr = e.hresult.map(|h| format!("0x{h:08X} ")).unwrap_or_default();
            Err(CheckError::new(code, format!("{hr}{}", e.message).trim().to_string()))
        }
    }
}

// ---------------------------------------------------------------------------------------
// The real COM implementations (only ever called in the helper process)

mod com {
    use windows::Win32::Foundation::VARIANT_TRUE;
    use windows::Win32::Networking::NetworkListManager::{
        INetworkCostManager, INetworkListManager, NetworkListManager,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    };
    use windows::Win32::System::UpdateAgent::{
        IUpdateSession, IWindowsDriverUpdate, UpdateSession, orcSucceeded, orcSucceededWithErrors, ssWindowsUpdate,
    };
    use windows::core::{BSTR, Interface};

    use super::{NetState, NetworkProbe, RawUpdate, SEARCH_CRITERIA, SearchError, UpdateSearch};

    /// COM for the current thread (multithreaded apartment), uninitialised on drop.
    pub struct ComApartment(bool);

    impl ComApartment {
        pub fn enter() -> Self {
            // SAFETY: plain call; balanced by CoUninitialize in Drop only when it succeeded.
            let ok = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
            ComApartment(ok)
        }
    }

    impl Drop for ComApartment {
        fn drop(&mut self) {
            if self.0 {
                // SAFETY: balances the successful CoInitializeEx on this thread.
                unsafe { CoUninitialize() };
            }
        }
    }

    fn err(e: windows::core::Error) -> SearchError {
        SearchError { hresult: Some(e.code().0 as u32), message: e.message().trim().to_string() }
    }

    pub struct Nlm;

    impl NetworkProbe for Nlm {
        fn probe(&self) -> NetState {
            // SAFETY: COM is initialised on this thread (ComApartment); the interfaces are released on drop.
            unsafe {
                let Ok(nlm) = CoCreateInstance::<_, INetworkListManager>(&NetworkListManager, None, CLSCTX_ALL) else {
                    return NetState::default();
                };
                let internet = nlm.IsConnectedToInternet().ok().map(|v| v == VARIANT_TRUE);
                let metered = nlm.cast::<INetworkCostManager>().ok().and_then(|c| {
                    let mut cost = 0u32;
                    c.GetCost(&mut cost, std::ptr::null()).ok()?;
                    // FIXED | VARIABLE | OVERDATALIMIT | ROAMING.
                    Some(cost & (0x2 | 0x4 | 0x1_0000 | 0x4_0000) != 0)
                });
                NetState { internet, metered }
            }
        }
    }

    pub struct WindowsUpdate;

    impl UpdateSearch for WindowsUpdate {
        fn search(&self) -> Result<Vec<RawUpdate>, SearchError> {
            // SAFETY: COM is initialised on this thread; every interface is released on drop; BSTRs are owned.
            unsafe {
                let session: IUpdateSession =
                    CoCreateInstance(&UpdateSession, None, CLSCTX_INPROC_SERVER).map_err(err)?;
                let _ = session.SetClientApplicationID(&BSTR::from("FanRGB driver check"));
                let searcher = session.CreateUpdateSearcher().map_err(err)?;
                searcher.SetServerSelection(ssWindowsUpdate).map_err(err)?;
                searcher.SetOnline(VARIANT_TRUE).map_err(err)?;
                let result = searcher.Search(&BSTR::from(SEARCH_CRITERIA)).map_err(err)?;
                let rc = result.ResultCode().map_err(err)?;
                if rc != orcSucceeded && rc != orcSucceededWithErrors {
                    let message = format!("search ended with result code {}", rc.0);
                    return Err(SearchError { hresult: None, message });
                }
                let updates = result.Updates().map_err(err)?;
                let n = updates.Count().map_err(err)?.clamp(0, 500);
                let mut out = Vec::new();
                for i in 0..n {
                    let Ok(u) = updates.get_Item(i) else { continue };
                    let s = |b: windows::core::Result<BSTR>| b.map(|b| b.to_string()).unwrap_or_default();
                    let mut r = RawUpdate {
                        title: s(u.Title()),
                        update_id: u.Identity().ok().map(|id| s(id.UpdateID())).unwrap_or_default(),
                        ..Default::default()
                    };
                    if let Ok(d) = u.cast::<IWindowsDriverUpdate>() {
                        r.hardware_id = s(d.DriverHardwareID());
                        r.model = s(d.DriverModel());
                        r.provider = s(d.DriverProvider());
                        r.manufacturer = s(d.DriverManufacturer());
                        r.class = s(d.DriverClass());
                        r.ver_date = d.DriverVerDate().ok();
                    }
                    out.push(r);
                }
                Ok(out)
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Helper process (`fanrgb --driver-check`)

/// Command line of the helper.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HelperArgs {
    /// Write the report here (the app's per-check file). Without it the report is printed.
    pub out: Option<PathBuf>,
    pub nonce: String,
    /// Scheduled check (skipped on a metered connection).
    pub auto: bool,
    /// Inventory only: no Windows Update search (console use).
    pub offline: bool,
}

/// Runs the search on its own thread with a time limit.
fn search_with_timeout(auto: bool) -> Result<Vec<WuOffer>, CheckError> {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().name("wu-search".into()).spawn(move || {
        let _com = com::ComApartment::enter();
        let _ = tx.send(run_check(&com::Nlm, &com::WindowsUpdate, auto));
    });
    if let Err(e) = spawned {
        return Err(CheckError::new(ErrorCode::DriverCheckFailed, e.to_string()));
    }
    rx.recv_timeout(SEARCH_TIMEOUT).unwrap_or_else(|_| {
        Err(CheckError::new(ErrorCode::DriverCheckTimeout, format!("no answer within {} s", SEARCH_TIMEOUT.as_secs())))
    })
}

/// `fanrgb --driver-check`: returns the process exit code. Never touches hardware, the engine, the
/// settings or the elevated data folder. Refuses to run elevated: the search must not run with
/// administrator rights.
pub fn helper_main(a: &HelperArgs) -> i32 {
    if crate::util::win::is_elevated() {
        eprintln!(
            "[{}] --driver-check runs only without administrator rights; FanRGB starts it that way.",
            ErrorCode::UnelevatedLaunch
        );
        return 3;
    }
    let t0 = Instant::now();
    if let Some(dir) = a.out.as_deref().and_then(Path::parent) {
        sweep_leftovers(dir, std::time::SystemTime::now());
    }
    let result = if a.offline { Ok(Vec::new()) } else { search_with_timeout(a.auto) };
    let (offers, error) = match result {
        Ok(o) => (o, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    let report = CheckReport {
        schema: REPORT_SCHEMA,
        nonce: a.nonce.clone(),
        duration_ms: t0.elapsed().as_millis() as u64,
        offers,
        error,
    };
    let json = serde_json::to_string_pretty(&report).unwrap_or_default();
    match &a.out {
        Some(path) => match write_report(path, json.as_bytes()) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("[{}] could not write {}: {e}", ErrorCode::Io, path.display());
                1
            }
        },
        None => {
            // Console use: the inventory with this result applied, then the raw report.
            let mut st = super::current_state();
            if !a.offline {
                super::apply_offers(&mut st.rows, report.error.is_none().then_some(report.offers.as_slice()));
                st.last_checked = report.error.is_none().then(super::unix_now);
                st.error = report.error.clone();
            }
            println!("{}", crate::diag::drivers_section(&st));
            println!("Windows Update report ({} ms):\n{json}", report.duration_ms);
            0
        }
    }
}

/// A report (`driver-check-<nonce>.json`) or its temp file (`.driver-check-<nonce>.json.<pid>-<n>.tmp`).
fn is_leftover_name(name: &str) -> bool {
    let n = name.strip_prefix('.').unwrap_or(name);
    n.starts_with("driver-check-") && (n.ends_with(".json") || (name.starts_with('.') && n.ends_with(".tmp")))
}

/// Removes reports and temp files of earlier checks that nobody read: the app was closed or the helper
/// killed mid-check. Runs in the unelevated helper, with the user's own rights, and only
/// touches plain files with those names older than [`LEFTOVER_AGE`]. Returns how many were removed.
fn sweep_leftovers(dir: &Path, now: std::time::SystemTime) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let mut removed = 0;
    for e in entries.flatten().take(4096) {
        let name = e.file_name().to_string_lossy().to_string();
        // `file_type` does not follow links: a link with a matching name is left alone.
        if !is_leftover_name(&name) || !e.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age >= LEFTOVER_AGE));
        if old && std::fs::remove_file(e.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn write_report(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if !path.is_absolute() || path.extension().is_none_or(|e| e != "json") {
        return Err(std::io::Error::other("--out must be an absolute .json path"));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::util::fsx::write_atomic(path, bytes)
}

// ---------------------------------------------------------------------------------------
// App side

fn new_nonce() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    h.write_u32(std::process::id());
    format!("{:016x}", h.finish())
}

/// Starts `fanrgb --driver-check` unelevated, waits up to [`HELPER_TIMEOUT`] (then kills it), and returns
/// its validated report. A killed or crashed helper yields `DriverCheckTimeout` / `DriverCheckCancelled`;
/// a partial or foreign file is never accepted.
pub fn run_helper(auto: bool) -> Result<CheckReport, CheckError> {
    let fail = |code: ErrorCode, m: String| CheckError::new(code, m);
    let exe = std::env::current_exe().map_err(|e| fail(ErrorCode::DriverCheckFailed, e.to_string()))?;
    // The helper writes into the desktop user's own %LOCALAPPDATA%\FanRGB (it cannot write anywhere else
    // that we read from with more rights).
    let dir = crate::util::win::desktop_user_local_app_data()
        .map_err(|e| fail(crate::error::code_of(&e), format!("{e:#}")))?
        .join(crate::util::paths::APP_DIR);
    let nonce = new_nonce();
    let out = dir.join(format!("driver-check-{nonce}.json"));
    let out_s = out.to_string_lossy().to_string();
    let mut args = vec!["--driver-check", "--out", &out_s, "--nonce", &nonce];
    if auto {
        args.push("--auto");
    }
    // Tied: if FanRGB exits mid-check the helper ends with it instead of writing a report nobody reads.
    let mut child = crate::util::win::spawn_unelevated_child_tied(&exe, &args)
        .map_err(|e| fail(crate::error::code_of(&e), format!("{e:#}")))?;
    let deadline = Instant::now() + HELPER_TIMEOUT;
    let exit = loop {
        match child.try_wait() {
            Ok(Some(code)) => break code,
            Ok(None) if Instant::now() >= deadline => {
                child.kill();
                discard(&out);
                let secs = HELPER_TIMEOUT.as_secs();
                return Err(fail(ErrorCode::DriverCheckTimeout, format!("stopped after {secs} s")));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(250)),
            Err(e) => {
                child.kill();
                discard(&out);
                return Err(fail(ErrorCode::DriverCheckCancelled, e.to_string()));
            }
        }
    };
    // Read and removed through one verified handle, never by name.
    let bytes = crate::util::fsx::take_limited(&out, MAX_REPORT_BYTES);
    if exit != 0 {
        return Err(fail(ErrorCode::DriverCheckCancelled, format!("the check process ended with code {exit}")));
    }
    let bytes = bytes.map_err(|e| fail(ErrorCode::DriverCheckCancelled, format!("no result: {e}")))?;
    parse_report(&bytes, &nonce)
}

/// Removes a report nobody will read, through a verified handle. A link, a redirected
/// folder or a missing file is left alone; the next helper sweeps what remains.
fn discard(out: &Path) {
    if let Err(e) = crate::util::fsx::delete_checked(out) {
        log::warn!("[{}] driver check: not removing {}: {e}", ErrorCode::Io, out.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Net(NetState);
    impl NetworkProbe for Net {
        fn probe(&self) -> NetState {
            self.0
        }
    }

    struct Wu(Result<Vec<RawUpdate>, SearchError>, std::cell::Cell<u32>);
    impl UpdateSearch for Wu {
        fn search(&self) -> Result<Vec<RawUpdate>, SearchError> {
            self.1.set(self.1.get() + 1);
            self.0.clone()
        }
    }

    fn wu(r: Result<Vec<RawUpdate>, SearchError>) -> Wu {
        Wu(r, std::cell::Cell::new(0))
    }

    fn online() -> Net {
        Net(NetState { internet: Some(true), metered: Some(false) })
    }

    fn arc_update() -> RawUpdate {
        RawUpdate {
            title: "Intel Corporation - Display - 32.0.101.9150".into(),
            update_id: "6c3f0b1c-0000-4000-8000-000000000001".into(),
            hardware_id: r"PCI\VEN_8086&DEV_E20B&SUBSYS_4215172F".into(),
            model: "Intel(R) Arc(TM) B580 Graphics".into(),
            provider: "Intel Corporation".into(),
            manufacturer: "Intel Corporation".into(),
            class: "Display".into(),
            ver_date: Some(46_297.0), // 2026-10-02
        }
    }

    #[test]
    fn raw_update_to_offer() {
        let o = arc_update().to_offer();
        assert_eq!(o.version.as_deref(), Some("32.0.101.9150"));
        assert_eq!(o.date.as_deref(), Some("2026-10-02"));
        assert_eq!(o.class, "Display");
        let o = RawUpdate { title: "  x  ".into(), ver_date: Some(0.0), ..Default::default() }.to_offer();
        assert_eq!((o.title.as_str(), o.version, o.date), ("x", None, None));
    }

    #[test]
    fn check_success_and_errors() {
        let w = wu(Ok(vec![arc_update()]));
        let offers = run_check(&online(), &w, false).unwrap();
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].hardware_id, r"PCI\VEN_8086&DEV_E20B&SUBSYS_4215172F");
        // Service disabled.
        let w = wu(Err(SearchError { hresult: Some(0x8007_0422), message: "disabled".into() }));
        let e = run_check(&online(), &w, false).unwrap_err();
        assert_eq!(e.code, ErrorCode::DriverCheckServiceOff);
        assert!(e.detail.starts_with("0x80070422"), "{}", e.detail);
        // Offline: by HRESULT, or by the network state whatever the agent says.
        let w = wu(Err(SearchError { hresult: Some(0x8024_402C), message: String::new() }));
        assert_eq!(run_check(&online(), &w, false).unwrap_err().code, ErrorCode::DriverCheckOffline);
        let offline = Net(NetState { internet: Some(false), metered: None });
        let w = wu(Err(SearchError { hresult: Some(0x8024_4019), message: String::new() }));
        assert_eq!(run_check(&offline, &w, false).unwrap_err().code, ErrorCode::DriverCheckOffline);
        // Unknown HRESULT online: generic failure with the number.
        assert_eq!(run_check(&online(), &w, false).unwrap_err().code, ErrorCode::DriverCheckFailed);
        let w = wu(Err(SearchError { hresult: None, message: "search ended with result code 4".into() }));
        assert_eq!(run_check(&online(), &w, false).unwrap_err().code, ErrorCode::DriverCheckFailed);
    }

    #[test]
    fn metered_skips_only_the_automatic_check() {
        let metered = Net(NetState { internet: Some(true), metered: Some(true) });
        let w = wu(Ok(vec![arc_update()]));
        assert_eq!(run_check(&metered, &w, true).unwrap_err().code, ErrorCode::DriverCheckMetered);
        assert_eq!(w.1.get(), 0, "no search on a metered connection");
        assert_eq!(run_check(&metered, &w, false).unwrap().len(), 1, "Check now still runs");
        // Unknown network state does not block anything.
        assert!(run_check(&Net(NetState::default()), &w, true).is_ok());
    }

    #[test]
    fn hresult_mapping() {
        assert_eq!(code_for_hresult(0x8024_002E), ErrorCode::DriverCheckServiceOff);
        assert_eq!(code_for_hresult(0x8024_0438), ErrorCode::DriverCheckOffline);
        assert_eq!(code_for_hresult(0x8007_2EE7), ErrorCode::DriverCheckOffline);
        assert_eq!(code_for_hresult(0x8024_401C), ErrorCode::DriverCheckTimeout);
        assert_eq!(code_for_hresult(0x8007_04C7), ErrorCode::DriverCheckCancelled);
        assert_eq!(code_for_hresult(0x8000_4005), ErrorCode::DriverCheckFailed);
    }

    /// Review L25: the helper removes old reports/temp files of interrupted checks, nothing else.
    #[test]
    fn leftovers_are_swept_by_name_and_age() {
        assert!(is_leftover_name("driver-check-0123456789abcdef.json"));
        assert!(is_leftover_name(".driver-check-0123456789abcdef.json.4242-0.tmp"));
        for keep in ["driver_check.json", "config.json", "driver-check-x.tmp", "driver-check-x.json.txt", "x.json"] {
            assert!(!is_leftover_name(keep), "{keep}");
        }
        let dir = crate::util::paths::test_home().join(format!("drivers-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["driver-check-a.json", ".driver-check-a.json.1-0.tmp", "token.bin"] {
            std::fs::write(dir.join(f), b"{}").unwrap();
        }
        std::fs::create_dir(dir.join("driver-check-dir.json")).unwrap();
        let now = std::time::SystemTime::now();
        assert_eq!(sweep_leftovers(&dir, now), 0, "a running check's fresh report stays");
        assert_eq!(sweep_leftovers(&dir, now + LEFTOVER_AGE + Duration::from_secs(1)), 2);
        let mut left: Vec<String> =
            std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
        left.sort();
        assert_eq!(left, ["driver-check-dir.json", "token.bin"]);
        assert_eq!(sweep_leftovers(&dir.join("missing"), now), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn helper_report_file_rules() {
        let dir = crate::util::paths::test_home().join("drivers-helper");
        let _ = std::fs::create_dir_all(&dir);
        assert!(write_report(Path::new("relative.json"), b"{}").is_err());
        assert!(write_report(&dir.join("x.txt"), b"{}").is_err());
        let p = dir.join("sub").join("r.json");
        write_report(&p, b"{}").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"{}");
        assert_ne!(new_nonce(), new_nonce());
    }
}
