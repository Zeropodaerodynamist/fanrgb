//! Driver inventory and update check. Inform only: FanRGB never downloads or installs a driver.
//!
//! - Phase 1 ([`inventory`]): installed driver versions (SetupAPI), the BIOS (SMBIOS) and the AMD chipset
//!   package. Offline and read-only; runs in the app's own process on the service thread.
//! - Phase 2 ([`wu`], opt-in): a Windows Update Agent search for `IsInstalled=0 and Type='Driver'`. It
//!   runs in an **unelevated helper process** (`fanrgb --driver-check`) with a 120 s limit, so neither COM
//!   to the update agent nor network traffic ever happens in the elevated process that holds PawnIO.
//!   The helper writes a small JSON report; the app validates it like any other
//!   untrusted input and caches the result.
//!
//! - Vendor versions ([`catalog`]): a signed list of the latest vendor driver versions, built weekly by
//!   CI (never scraped by the app), downloaded by the unelevated update helper and re-verified here.
//!
//! The UI reads [`DriverState`] from [`service`] and offers "Open vendor page" for the pinned
//! [`DriverRow::vendor_url`] (allow-listed in `util::win::ALLOWED_URLS`).

pub mod catalog;
pub mod inventory;
pub mod wu;

use std::cmp::Ordering;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::ErrorCode;

// ---------------------------------------------------------------------------------------
// Pinned vendor pages ("Open vendor page" only; each is in `util::win::ALLOWED_URLS`).

/// GIGABYTE B650M D3HP AX rev 1.3 (Realtek 8852CE Wi-Fi): Wi-Fi, Bluetooth and audio drivers.
pub const URL_GIGABYTE_REV13_DRIVERS: &str =
    "https://www.gigabyte.com/Motherboard/B650M-D3HP-AX-rev-13/support#support-dl-driver";
pub const URL_GIGABYTE_REV13_BIOS: &str =
    "https://www.gigabyte.com/Motherboard/B650M-D3HP-AX-rev-13/support#support-dl-bios";
/// GIGABYTE B650M D3HP AX rev 1.0/1.2 (AMD RZ616 / MediaTek Wi-Fi).
pub const URL_GIGABYTE_REV10_DRIVERS: &str =
    "https://www.gigabyte.com/Motherboard/B650M-D3HP-AX-rev-10-12/support#support-dl-driver";
pub const URL_GIGABYTE_REV10_BIOS: &str =
    "https://www.gigabyte.com/Motherboard/B650M-D3HP-AX-rev-10-12/support#support-dl-bios";
/// Realtek "PCIe FE / GbE / 2.5GbE / 5G / 10G Family Controller Software" (RTL8125 Windows drivers).
pub const URL_REALTEK_LAN: &str = "https://www.realtek.com/Download/List?cate_id=584";
/// Intel Arc & Iris Xe Graphics - Windows (covers the Arc B580).
pub const URL_INTEL_ARC: &str =
    "https://www.intel.com/content/www/us/en/download/785597/intel-arc-graphics-windows.html";
/// AMD B650 chipset drivers (the "AMD Chipset Software" package).
pub const URL_AMD_B650: &str = "https://www.amd.com/en/support/downloads/drivers.html/chipsets/am5/b650.html";
pub const URL_PAWNIO: &str = "https://pawnio.eu/";

/// Every page a [`DriverRow`] can point at.
#[cfg_attr(not(test), allow(dead_code))] // checked against `ALLOWED_URLS` in the tests
pub const VENDOR_URLS: [&str; 8] = [
    URL_GIGABYTE_REV13_DRIVERS,
    URL_GIGABYTE_REV13_BIOS,
    URL_GIGABYTE_REV10_DRIVERS,
    URL_GIGABYTE_REV10_BIOS,
    URL_REALTEK_LAN,
    URL_INTEL_ARC,
    URL_AMD_B650,
    URL_PAWNIO,
];

// ---------------------------------------------------------------------------------------
// Data model

/// Row kinds, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum DriverCategory {
    Wifi,
    Bluetooth,
    Lan,
    Audio,
    Chipset,
    Gpu,
    /// The PawnIO driver FanRGB uses for the ITE Super I/O and the EC.
    SuperIo,
    Bios,
}

impl DriverCategory {
    pub fn label(self) -> &'static str {
        match self {
            DriverCategory::Wifi => "Wi-Fi",
            DriverCategory::Bluetooth => "Bluetooth",
            DriverCategory::Lan => "LAN",
            DriverCategory::Audio => "Audio",
            DriverCategory::Chipset => "Chipset",
            DriverCategory::Gpu => "Graphics",
            DriverCategory::SuperIo => "Super I/O",
            DriverCategory::Bios => "BIOS",
        }
    }

    /// Windows Update can offer drivers for this kind (so "nothing offered" means "up to date as far as
    /// Windows Update knows"). Never for the BIOS; not reliably for the AMD chipset package or PawnIO.
    pub fn covered_by_windows_update(self) -> bool {
        matches!(
            self,
            DriverCategory::Wifi
                | DriverCategory::Bluetooth
                | DriverCategory::Lan
                | DriverCategory::Audio
                | DriverCategory::Gpu
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum DriverStatus {
    /// Windows Update has nothing newer for this device.
    UpToDate,
    /// Windows Update offers a newer driver ([`DriverRow::latest`]).
    UpdateAvailable,
    /// Not checked yet, the check failed, or Windows Update does not cover this kind (BIOS, chipset
    /// package, PawnIO): only the installed version and the vendor page are shown.
    Unknown,
}

impl DriverStatus {
    pub fn label(self) -> &'static str {
        match self {
            DriverStatus::UpToDate => "Up to date",
            DriverStatus::UpdateAvailable => "Update available",
            DriverStatus::Unknown => "Unknown",
        }
    }
}

/// One line of the driver list.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DriverRow {
    pub category: DriverCategory,
    /// Device Manager name ("Realtek 8852CE WiFi 6E PCI-E NIC").
    pub device: String,
    pub provider: String,
    /// Installed driver version (BIOS: "F36d"; chipset package: "8.03.25.247").
    pub installed: String,
    /// `YYYY-MM-DD`.
    pub installed_date: Option<String>,
    /// `oem59.inf` (empty for the BIOS and the chipset package row).
    pub inf: String,
    /// Catalog signer ("Microsoft Windows Hardware Compatibility Publisher").
    pub signer: Option<String>,
    pub instance_id: String,
    /// Identical devices merged into this row (two AMD I2C controllers: 2).
    pub count: u32,
    /// Hardware and compatible IDs (upper case), for matching Windows Update offers.
    pub hardware_ids: Vec<String>,
    /// The newest Windows Update offer for this device, if any.
    pub latest: Option<WuOffer>,
    pub status: DriverStatus,
    pub vendor_url: Option<&'static str>,
    /// The AMD chipset installer's own version rather than a device.
    pub package: bool,
    /// Installer package name of a package row (`AMD Chipset Software`), for catalog matching.
    pub package_name: Option<String>,
    /// The motherboard, on the BIOS row (catalog matching by board model and revision).
    pub board: Option<BoardInfo>,
    /// The vendor catalog's entry for this device, if any (see [`catalog::apply_catalog`]).
    pub catalog: Option<catalog::CatalogMatch>,
    pub catalog_status: catalog::CatalogStatus,
}

/// SMBIOS board identity plus the revisions it can be (GIGABYTE does not report the revision; the
/// inventory infers it from the Wi-Fi module).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct BoardInfo {
    pub vendor: String,
    pub product: String,
    pub revisions: Vec<String>,
}

impl DriverRow {
    pub(crate) fn empty() -> Self {
        DriverRow {
            category: DriverCategory::Bios,
            device: String::new(),
            provider: String::new(),
            installed: String::new(),
            installed_date: None,
            inf: String::new(),
            signer: None,
            instance_id: String::new(),
            count: 1,
            hardware_ids: Vec::new(),
            latest: None,
            status: DriverStatus::Unknown,
            vendor_url: None,
            package: false,
            package_name: None,
            board: None,
            catalog: None,
            catalog_status: catalog::CatalogStatus::NoList,
        }
    }
}

/// A driver Windows Update offers (not installed).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WuOffer {
    /// "Intel Corporation - Display - 32.0.101.9150".
    pub title: String,
    /// Version parsed from the title, when it has one.
    pub version: Option<String>,
    /// `DriverVerDate` as `YYYY-MM-DD`.
    pub date: Option<String>,
    pub hardware_id: String,
    pub model: String,
    pub provider: String,
    pub manufacturer: String,
    pub class: String,
    pub update_id: String,
}

/// A coded failure of the inventory or the check.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckError {
    pub code: ErrorCode,
    pub detail: String,
}

impl CheckError {
    pub fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        CheckError { code, detail: detail.into() }
    }
}

impl std::fmt::Display for CheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.detail.is_empty() {
            write!(f, "[{}] {}", self.code, self.code.title())
        } else {
            write!(f, "[{}] {}: {}", self.code, self.code.title(), self.detail)
        }
    }
}

/// What the `--driver-check` helper writes (schema 1). The app treats it as untrusted input.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckReport {
    pub schema: u32,
    /// Echo of `--nonce`: a stale or planted file from another run is rejected.
    pub nonce: String,
    pub duration_ms: u64,
    pub offers: Vec<WuOffer>,
    /// Set when the search failed (then `offers` is empty).
    pub error: Option<CheckError>,
}

pub const REPORT_SCHEMA: u32 = 1;
/// Upper bounds for anything read back from the helper or the cache.
pub const MAX_REPORT_BYTES: u64 = 512 * 1024;
const MAX_OFFERS: usize = 256;
const MAX_FIELD: usize = 300;

/// Invisible or text-reordering characters: bidi embeddings, overrides and isolates
/// (U+202A-202E, U+2066-2069, U+200E/F, U+061C), zero-width and joiner characters, the BOM, soft
/// hyphen, the other Unicode format characters (Cf), variation selectors, tag characters, Hangul
/// fillers, interlinear annotation marks, private-use characters and noncharacters. Shown in an
/// elevated UI they could make one driver name look like another ("…exe" reversed).
fn is_hidden(c: char) -> bool {
    let u = c as u32;
    c.is_control()
        || matches!(
            u,
            0x00AD
                | 0x034F
                | 0x0600..=0x0605
                | 0x061C
                | 0x06DD
                | 0x070F
                | 0x0890..=0x0891
                | 0x08E2
                | 0x115F..=0x1160
                | 0x17B4..=0x17B5
                | 0x180B..=0x180F
                | 0x200B..=0x200F
                | 0x202A..=0x202E
                | 0x2060..=0x206F
                | 0x3164
                | 0xE000..=0xF8FF
                | 0xFDD0..=0xFDEF
                | 0xFE00..=0xFE0F
                | 0xFEFF
                | 0xFFA0
                | 0xFFF0..=0xFFFC
                | 0x110BD
                | 0x110CD
                | 0x13430..=0x1343F
                | 0x1BCA0..=0x1BCA3
                | 0x1D173..=0x1D17A
                | 0xE0000..=0xE0FFF
                | 0xF0000..
        )
        || u & 0xFFFE == 0xFFFE
}

/// Makes helper output safe to show: it comes from an unelevated process, so the elevated app treats
/// it as untrusted text. Drops control and invisible/reordering characters ([`is_hidden`]), turns
/// any other whitespace (line and paragraph separators, NBSP, ideographic space) into single plain
/// spaces, and keeps at most [`MAX_FIELD`] characters.
fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_FIELD * 4));
    let mut n = 0;
    for c in s.chars() {
        if n >= MAX_FIELD {
            break;
        }
        if c.is_whitespace() {
            if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
                n += 1;
            }
        } else if !is_hidden(c) {
            out.push(c);
            n += 1;
        }
    }
    out.trim_end().to_string()
}

fn clean_offer(o: &WuOffer) -> WuOffer {
    let opt = |s: &Option<String>| s.as_deref().map(clean).filter(|s| !s.is_empty());
    WuOffer {
        title: clean(&o.title),
        version: opt(&o.version).filter(|v| parse_version(v).is_some()),
        date: opt(&o.date).filter(|d| is_ymd(d)),
        hardware_id: clean(&o.hardware_id),
        model: clean(&o.model),
        provider: clean(&o.provider),
        manufacturer: clean(&o.manufacturer),
        class: clean(&o.class),
        update_id: clean(&o.update_id),
    }
}

/// Codes the helper may report; anything else becomes `DriverCheckFailed`.
fn helper_code(c: ErrorCode) -> ErrorCode {
    match c {
        ErrorCode::DriverCheckOffline
        | ErrorCode::DriverCheckMetered
        | ErrorCode::DriverCheckServiceOff
        | ErrorCode::DriverCheckTimeout
        | ErrorCode::DriverCheckFailed
        | ErrorCode::DriverCheckCancelled
        | ErrorCode::UnelevatedLaunch => c,
        _ => ErrorCode::DriverCheckFailed,
    }
}

/// Parses and validates a helper report: schema, nonce, size limits, field contents.
pub fn parse_report(bytes: &[u8], nonce: &str) -> Result<CheckReport, CheckError> {
    // serde's message can quote the helper's text (an unknown field name): cleaned like any field.
    let bad =
        |m: String| CheckError::new(ErrorCode::DriverCheckCancelled, clean(&format!("unusable helper result: {m}")));
    if bytes.len() as u64 > MAX_REPORT_BYTES {
        return Err(bad(format!("{} bytes", bytes.len())));
    }
    let r: CheckReport = serde_json::from_slice(bytes).map_err(|e| bad(e.to_string()))?;
    if r.schema != REPORT_SCHEMA {
        return Err(bad(format!("schema {}", r.schema)));
    }
    if nonce.is_empty() || r.nonce != nonce {
        return Err(bad("result belongs to another check".into()));
    }
    if r.offers.len() > MAX_OFFERS {
        return Err(bad(format!("{} offers", r.offers.len())));
    }
    Ok(CheckReport {
        schema: r.schema,
        nonce: r.nonce,
        duration_ms: r.duration_ms,
        offers: if r.error.is_some() { Vec::new() } else { r.offers.iter().map(clean_offer).collect() },
        error: r.error.map(|e| CheckError::new(helper_code(e.code), clean(&e.detail))),
    })
}

// ---------------------------------------------------------------------------------------
// Versions, dates and mapping offers to rows

/// `"32.0.101.9033"` → `[32, 0, 101, 9033]` (2-4 numeric parts).
pub fn parse_version(s: &str) -> Option<Vec<u64>> {
    let parts: Vec<u64> = s.trim().split('.').map(|p| p.parse::<u64>().ok()).collect::<Option<_>>()?;
    (2..=4).contains(&parts.len()).then_some(parts)
}

fn cmp_versions(a: &[u64], b: &[u64]) -> Ordering {
    let n = a.len().max(b.len());
    let get = |v: &[u64], i: usize| v.get(i).copied().unwrap_or(0);
    (0..n).map(|i| get(a, i).cmp(&get(b, i))).find(|o| o.is_ne()).unwrap_or(Ordering::Equal)
}

fn is_ymd(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// Version from a Windows Update driver title: the last token that looks like a driver version
/// ("Realtek Semiconductor Corp. - Net - 6001.16.180.0" → "6001.16.180.0"). Driver versions have four
/// parts, so a 3-4 part token wins over a shorter one anywhere in the title.
pub fn version_from_title(title: &str) -> Option<String> {
    let tokens: Vec<&str> = title
        .split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')')
        .map(|t| t.trim_start_matches(['v', 'V']))
        .filter(|t| t.contains('.'))
        .collect();
    let parts = |t: &str| parse_version(t).map(|v| v.len()).unwrap_or(0);
    let pick = |min: usize| tokens.iter().rev().find(|t| parts(t) >= min).map(|t| t.to_string());
    pick(3).or_else(|| pick(2))
}

/// Days since 1970-01-01 → `YYYY-MM-DD` (proleptic Gregorian).
pub fn days_to_ymd(days: i64) -> String {
    // H. Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

/// OLE automation DATE (days since 1899-12-30) → `YYYY-MM-DD`.
pub fn ole_date_to_ymd(d: f64) -> Option<String> {
    (d.is_finite() && d > 1.0 && d < 2_958_466.0).then(|| days_to_ymd(d.floor() as i64 - 25_569))
}

/// `"2026-10-19 11:05 UTC"`.
pub fn fmt_unix(t: u64) -> String {
    let days = (t / 86_400) as i64;
    let s = t % 86_400;
    format!("{} {:02}:{:02} UTC", days_to_ymd(days), s / 3600, (s % 3600) / 60)
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Is `offer` newer than what `row` has installed? Version first, then date; `None` if neither compares.
fn offer_is_newer(row: &DriverRow, offer: &WuOffer) -> Option<bool> {
    if let (Some(a), Some(b)) = (parse_version(&row.installed), offer.version.as_deref().and_then(parse_version)) {
        return Some(cmp_versions(&b, &a) == Ordering::Greater);
    }
    match (&row.installed_date, &offer.date) {
        (Some(a), Some(b)) => Some(b > a),
        _ => None,
    }
}

fn offer_matches(row: &DriverRow, offer: &WuOffer) -> bool {
    if row.package || row.category == DriverCategory::Bios {
        return false;
    }
    let hw = offer.hardware_id.trim();
    if !hw.is_empty() && row.hardware_ids.iter().any(|id| id.eq_ignore_ascii_case(hw)) {
        return true;
    }
    // Some offers carry only the model name.
    hw.is_empty() && !offer.model.is_empty() && offer.model.eq_ignore_ascii_case(&row.device)
}

/// Sets `latest` and `status` of every row from a Windows Update result. `offers == None`: not checked
/// (or no successful check yet): everything is `Unknown`. Never claims "outdated" for anything Windows
/// Update did not list; the BIOS, the chipset package and PawnIO stay `Unknown` unless offered.
pub fn apply_offers(rows: &mut [DriverRow], offers: Option<&[WuOffer]>) {
    for row in rows.iter_mut() {
        row.latest = None;
        row.status = DriverStatus::Unknown;
        let Some(offers) = offers else { continue };
        let best = offers.iter().filter(|o| offer_matches(row, o)).max_by(|a, b| {
            match (a.version.as_deref().and_then(parse_version), b.version.as_deref().and_then(parse_version)) {
                (Some(x), Some(y)) => cmp_versions(&x, &y),
                _ => a.date.cmp(&b.date),
            }
        });
        match best {
            Some(o) => {
                row.status = match offer_is_newer(row, o) {
                    Some(true) => DriverStatus::UpdateAvailable,
                    Some(false) => DriverStatus::UpToDate,
                    None => DriverStatus::Unknown,
                };
                row.latest = Some(o.clone());
            }
            None if row.category.covered_by_windows_update() && !row.package => row.status = DriverStatus::UpToDate,
            None => {}
        }
    }
}

// ---------------------------------------------------------------------------------------
// Cache of the last check (`driver_check.json` in the data folder; demo: the demo folder)

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CheckCache {
    pub schema: u32,
    /// UNIX time of the last check attempt (successful or not).
    pub last_attempt: u64,
    /// UNIX time of the last successful check; `offers` are from that check.
    pub last_success: Option<u64>,
    pub offers: Vec<WuOffer>,
    /// The last attempt's failure (cleared by a success).
    pub error: Option<CheckError>,
}

impl CheckCache {
    pub fn offers(&self) -> Option<&[WuOffer]> {
        self.last_success.map(|_| self.offers.as_slice())
    }

    /// Records the outcome of one check at `now`.
    pub fn record(&mut self, now: u64, result: Result<Vec<WuOffer>, CheckError>) {
        self.schema = REPORT_SCHEMA;
        self.last_attempt = now;
        match result {
            Ok(offers) => {
                self.last_success = Some(now);
                self.offers = offers;
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }
}

pub fn load_cache(path: &Path) -> CheckCache {
    let Ok(bytes) = crate::util::fsx::read_limited(path, MAX_REPORT_BYTES) else { return CheckCache::default() };
    match serde_json::from_slice::<CheckCache>(&bytes) {
        Ok(mut c) if c.offers.len() <= MAX_OFFERS => {
            c.offers = c.offers.iter().map(clean_offer).collect();
            c.error = c.error.map(|e| CheckError::new(helper_code(e.code), clean(&e.detail)));
            c
        }
        _ => {
            log::warn!("[{}] ignoring unreadable {}", ErrorCode::Io, path.display());
            CheckCache::default()
        }
    }
}

pub fn save_cache(path: &Path, c: &CheckCache) {
    let r = serde_json::to_vec_pretty(c)
        .map_err(std::io::Error::other)
        .and_then(|b| crate::util::fsx::write_atomic(path, &b));
    if let Err(e) = r {
        log::warn!("[{}] could not write {}: {e}", ErrorCode::Io, path.display());
    }
}

// ---------------------------------------------------------------------------------------
// Automatic check schedule

/// Earliest automatic check after boot (seconds).
pub const AUTO_MIN_UPTIME_S: u64 = 5 * 60;
pub const AUTO_INTERVAL_S: u64 = 24 * 3600;
/// Retry sooner after "offline" / "metered".
pub const AUTO_RETRY_S: u64 = 3600;

/// Whether the automatic (opt-in) check is due. `jitter_s` (0-3600) spreads checks out.
pub fn auto_due(enabled: bool, now: u64, uptime_s: u64, cache: &CheckCache, jitter_s: u64) -> bool {
    if !enabled || uptime_s < AUTO_MIN_UPTIME_S {
        return false;
    }
    if cache.last_attempt == 0 || now < cache.last_attempt {
        return true;
    }
    let transient = cache
        .error
        .as_ref()
        .is_some_and(|e| matches!(e.code, ErrorCode::DriverCheckOffline | ErrorCode::DriverCheckMetered));
    let wait = if transient { AUTO_RETRY_S } else { AUTO_INTERVAL_S + jitter_s };
    now - cache.last_attempt >= wait
}

// ---------------------------------------------------------------------------------------
// State for the UI and the service thread

/// What the UI shows. Get it with [`DriverService::snapshot`].
#[derive(Clone, Debug, Default)]
pub struct DriverState {
    /// Empty until the first inventory pass finished (`inventory_done`).
    pub rows: Vec<DriverRow>,
    pub inventory_done: bool,
    pub inventory_error: Option<CheckError>,
    pub inventory_ms: u64,
    /// A Windows Update check is running (in the helper process).
    pub checking: bool,
    /// UNIX time the running check started.
    pub check_started: Option<u64>,
    /// UNIX time of the last successful check (statuses are based on it).
    pub last_checked: Option<u64>,
    pub last_attempt: Option<u64>,
    /// The last attempt's failure.
    pub error: Option<CheckError>,
    /// Daily automatic check (setting `drivers.auto_check`, default off).
    pub auto_check: bool,
    /// The signed vendor catalog in use and its last download.
    pub catalog: CatalogInfo,
}

impl DriverState {
    /// Rows with something newer: offered by Windows Update or listed by the vendor catalog (the BIOS is
    /// shown but not counted: it is never a one-click update).
    pub fn updates_available(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| {
                r.status == DriverStatus::UpdateAvailable
                    || (r.catalog_status == catalog::CatalogStatus::Newer && r.category != DriverCategory::Bios)
            })
            .count()
    }
}

/// The catalog the rows were matched against, for the UI and diagnostics.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CatalogInfo {
    /// The catalog in use (verified, or the demo preview).
    pub loaded: Option<CatalogSummary>,
    /// Last time a downloaded catalog passed every check.
    pub fetched_at: Option<u64>,
    pub last_attempt: Option<u64>,
    /// The last download's failure (the loaded catalog stays in use).
    pub error: Option<CheckError>,
    /// An update token is saved (the catalog is downloaded with it; without one, anonymously).
    pub token_saved: bool,
    /// A download is running.
    pub fetching: bool,
    /// Demo instance: `loaded` is a built-in preview, nothing is downloaded.
    pub demo: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CatalogSummary {
    pub serial: u64,
    pub generated_at: u64,
    pub expires: u64,
    /// 0 = primary catalog key, 1 = backup.
    pub key: usize,
    /// minisign key id of the signing key.
    pub key_id: String,
    pub entries: usize,
}

impl CatalogInfo {
    /// Past `expires`: still shown, marked "list out of date".
    pub fn expired(&self, now: u64) -> bool {
        self.loaded.as_ref().is_some_and(|c| now > c.expires)
    }
}

enum Cmd {
    Refresh,
    Check {
        auto: bool,
    },
    /// Re-evaluate the automatic schedule (the setting changed).
    Wake,
    /// The update helper staged a catalog: re-verify and use it.
    CatalogStaged,
}

/// Owns the inventory/check worker thread.
pub struct DriverService {
    state: Arc<Mutex<DriverState>>,
    tx: Mutex<mpsc::Sender<Cmd>>,
    /// The worker thread runs (false if it never started, or ended, also by a panic).
    alive: Arc<AtomicBool>,
}

static SERVICE: OnceLock<DriverService> = OnceLock::new();

/// A check that has not reported after this long is shown as failed: the helper is killed
/// after [`wu::HELPER_TIMEOUT`], so only a stuck or dead worker gets here.
pub const CHECK_STALE_S: u64 = wu::HELPER_TIMEOUT.as_secs() + 180;

/// Starts the service (once; later calls return the running one). The first inventory pass starts
/// immediately on the worker thread.
pub fn start(auto_check: bool) -> &'static DriverService {
    SERVICE.get_or_init(|| spawn_service(auto_check, |st, rx| worker(st, rx, crate::config::driver_check_path())))
}

/// Ends "Checking…" when the worker thread ends for any reason, a panic included.
struct WorkerExit {
    state: Arc<Mutex<DriverState>>,
    alive: Arc<AtomicBool>,
}

impl Drop for WorkerExit {
    fn drop(&mut self) {
        self.alive.store(false, AtomicOrdering::SeqCst);
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if s.checking {
            s.checking = false;
            s.check_started = None;
            s.error = Some(CheckError::new(ErrorCode::DriverCheckFailed, "the driver check stopped unexpectedly"));
        }
        log::warn!("[{}] driver service stopped", ErrorCode::Internal);
    }
}

fn spawn_service(
    auto_check: bool,
    work: impl FnOnce(Arc<Mutex<DriverState>>, mpsc::Receiver<Cmd>) + Send + 'static,
) -> DriverService {
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(Mutex::new(DriverState { auto_check, ..Default::default() }));
    let alive = Arc::new(AtomicBool::new(true));
    let exit = WorkerExit { state: state.clone(), alive: alive.clone() };
    let st = state.clone();
    let spawned = std::thread::Builder::new().name("fanrgb-drivers".into()).spawn(move || {
        let _exit = exit;
        work(st, rx);
    });
    if let Err(e) = spawned {
        // The closure (and with it `exit`) was dropped: `alive` is false.
        log::warn!("[{}] driver service not started: {e}", ErrorCode::Internal);
    }
    DriverService { state, tx: Mutex::new(tx), alive }
}

/// The running service, if [`start`] was called.
pub fn service() -> Option<&'static DriverService> {
    SERVICE.get()
}

impl DriverService {
    pub fn snapshot(&self) -> DriverState {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner()).clone();
        end_if_stale(&mut s, unix_now());
        s
    }

    /// False if the command could not be delivered (the worker is gone).
    fn send(&self, c: Cmd) -> bool {
        self.alive.load(AtomicOrdering::SeqCst) && self.tx.lock().is_ok_and(|tx| tx.send(c).is_ok())
    }

    /// "Check now" (ignored while a check runs). Runs even on a metered connection. "Checking…" is
    /// shown only once the worker has the request.
    pub fn check_now(&self) {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        end_if_stale(&mut s, unix_now());
        if s.checking {
            return;
        }
        // Sent under the state lock, so the worker cannot finish and publish before `checking` is set.
        if self.send(Cmd::Check { auto: false }) {
            s.checking = true;
            s.check_started = Some(unix_now());
        } else {
            s.error = Some(CheckError::new(ErrorCode::DriverCheckFailed, "the driver service is not running"));
            log::warn!("[{}] driver check not started: the driver service is not running", ErrorCode::Internal);
        }
    }

    /// Re-reads the installed drivers (e.g. after the user updated one).
    #[allow(dead_code)] // Updates page API
    pub fn refresh(&self) {
        self.send(Cmd::Refresh);
    }

    pub fn set_auto(&self, on: bool) {
        if let Ok(mut s) = self.state.lock() {
            s.auto_check = on;
        }
        self.send(Cmd::Wake);
    }

    /// Called after `--update-fetch` exited: it also downloads the vendor catalog.
    pub fn catalog_staged(&self) {
        self.send(Cmd::CatalogStaged);
    }
}

/// A check running longer than [`CHECK_STALE_S`] (or started in the future: clock change) is over.
fn end_if_stale(s: &mut DriverState, now: u64) {
    if s.checking && s.check_started.is_none_or(|t| now < t || now - t > CHECK_STALE_S) {
        s.checking = false;
        s.check_started = None;
        s.error = Some(CheckError::new(ErrorCode::DriverCheckTimeout, format!("no result after {CHECK_STALE_S} s")));
    }
}

/// Inventory with the cached Windows Update result applied (for `--diag`, the report, the helper). With
/// the service running this is its snapshot (never blocks; empty until the first pass is done).
pub fn current_state() -> DriverState {
    if let Some(s) = service() {
        return s.snapshot();
    }
    let cache = load_cache(&crate::config::driver_check_path());
    let inv = inventory::collect();
    let cat = catalog::runtime::Runtime::new(
        crate::config::catalog_dir(),
        None,
        catalog::keys::TRUSTED.iter().map(|k| k.to_string()).collect(),
        crate::updater::is_disabled(),
        unix_now(),
    );
    state_from(inv, &cache, false, false, &cat)
}

fn state_from(
    inv: inventory::Inventory,
    cache: &CheckCache,
    auto_check: bool,
    checking: bool,
    cat: &catalog::runtime::Runtime,
) -> DriverState {
    let mut rows = inv.rows;
    apply_offers(&mut rows, cache.offers());
    catalog::apply_catalog(&mut rows, cat.catalog.as_ref());
    DriverState {
        rows,
        inventory_done: true,
        inventory_error: inv.error,
        inventory_ms: inv.took_ms,
        checking,
        check_started: None,
        last_checked: cache.last_success,
        last_attempt: (cache.last_attempt > 0).then_some(cache.last_attempt),
        error: cache.error.clone(),
        auto_check,
        catalog: cat.info.clone(),
    }
}

fn log_catalog(what: &str, res: &Result<catalog::store::Outcome, CheckError>, cat: &catalog::runtime::Runtime) {
    let serial = cat.info.loaded.as_ref().map(|c| c.serial).unwrap_or(0);
    match res {
        Ok(catalog::store::Outcome::New) => {
            log::info!("vendor driver catalog ({what}): serial {serial} verified and in use")
        }
        Ok(catalog::store::Outcome::Unchanged) => {
            log::info!("vendor driver catalog ({what}): serial {serial} unchanged")
        }
        Err(e) => log::warn!("vendor driver catalog ({what}): {e}"),
    }
}

fn worker(state: Arc<Mutex<DriverState>>, rx: mpsc::Receiver<Cmd>, cache_path: std::path::PathBuf) {
    let mut cache = load_cache(&cache_path);
    let mut cat = catalog::runtime::Runtime::for_app(unix_now());
    // Spread automatic checks of different PCs over an hour.
    let jitter = (std::process::id() as u64).wrapping_mul(2_654_435_761) % 3600;
    let publish = |cache: &CheckCache, checking: bool, cat: &catalog::runtime::Runtime| {
        let inv = inventory::collect();
        if let Some(e) = &inv.error {
            log::warn!("{e}");
        }
        let auto = state.lock().map(|s| s.auto_check).unwrap_or(false);
        let started = state.lock().ok().and_then(|s| s.check_started);
        let mut next = state_from(inv, cache, auto, checking, cat);
        if checking {
            next.check_started = started;
        }
        if let Ok(mut s) = state.lock() {
            *s = next;
        }
    };
    publish(&cache, false, &cat);
    log::info!("driver inventory: {} rows", state.lock().map(|s| s.rows.len()).unwrap_or(0));
    // The first round evaluates the automatic schedule right away, then every 5 minutes.
    let mut first = true;
    loop {
        let cmd = if std::mem::take(&mut first) {
            None
        } else {
            match rx.recv_timeout(Duration::from_secs(300)) {
                Ok(c) => Some(c),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        };
        let auto_on = state.lock().map(|s| s.auto_check).unwrap_or(false);
        let check = match cmd {
            Some(Cmd::Refresh) => {
                let checking = state.lock().map(|s| s.checking).unwrap_or(false);
                publish(&cache, checking, &cat);
                None
            }
            Some(Cmd::CatalogStaged) => {
                let res = catch_catalog(|| cat.evaluate_staged(unix_now()));
                log_catalog("update check", &res, &cat);
                let checking = state.lock().map(|s| s.checking).unwrap_or(false);
                publish(&cache, checking, &cat);
                None
            }
            Some(Cmd::Check { auto }) => {
                if !auto {
                    // "Check now" refreshes the vendor list first (a few seconds), then asks Windows Update.
                    if let Ok(mut s) = state.lock() {
                        s.catalog.fetching = true;
                    }
                    let res = catch_catalog(|| cat.fetch_now(unix_now()));
                    log_catalog("check now", &res, &cat);
                    publish(&cache, true, &cat);
                }
                Some(auto)
            }
            Some(Cmd::Wake) | None => {
                let uptime = crate::util::win::uptime_ms() / 1000;
                auto_due(auto_on, unix_now(), uptime, &cache, jitter).then_some(true)
            }
        };
        let Some(auto) = check else { continue };
        if let Ok(mut s) = state.lock() {
            s.checking = true;
            s.check_started.get_or_insert(unix_now());
        }
        log::info!("driver check started ({})", if auto { "automatic" } else { "manual" });
        // A panic in the check must not leave "Checking…" up or end the service.
        let result = std::panic::catch_unwind(|| wu::run_helper(auto))
            .unwrap_or_else(|_| Err(CheckError::new(ErrorCode::DriverCheckFailed, "the driver check crashed")))
            .and_then(|r| match r.error {
                Some(e) => Err(e),
                None => Ok(r.offers),
            });
        match &result {
            Ok(o) => log::info!("driver check: Windows Update offers {} driver(s)", o.len()),
            Err(e) => log::warn!("driver check: {e}"),
        }
        cache.record(unix_now(), result);
        save_cache(&cache_path, &cache);
        publish(&cache, false, &cat);
    }
}

/// A panic while evaluating the catalog must not end the driver service.
fn catch_catalog(
    f: impl FnOnce() -> Result<catalog::store::Outcome, CheckError>,
) -> Result<catalog::store::Outcome, CheckError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .unwrap_or_else(|_| Err(CheckError::new(ErrorCode::CatalogRejected, "checking the vendor list crashed")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(hwid: &str, title: &str, date: &str) -> WuOffer {
        WuOffer {
            title: title.into(),
            version: version_from_title(title),
            date: Some(date.into()),
            hardware_id: hwid.into(),
            model: String::new(),
            provider: String::new(),
            manufacturer: String::new(),
            class: String::new(),
            update_id: "x".into(),
        }
    }

    fn rows() -> Vec<DriverRow> {
        let (d, b, p) = inventory::tests::this_pc();
        inventory::classify(&d, Some(&b), &p)
    }

    fn row<'a>(rows: &'a [DriverRow], c: DriverCategory) -> &'a DriverRow {
        rows.iter().find(|r| r.category == c && !r.package).unwrap()
    }

    #[test]
    fn versions_and_titles() {
        assert_eq!(parse_version("32.0.101.9033"), Some(vec![32, 0, 101, 9033]));
        assert_eq!(parse_version("F36d"), None);
        assert_eq!(parse_version("1"), None);
        assert_eq!(cmp_versions(&[1, 2], &[1, 2, 0, 0]), Ordering::Equal);
        assert_eq!(cmp_versions(&[6001, 16, 180, 0], &[6001, 16, 172, 0]), Ordering::Greater);
        assert_eq!(
            version_from_title("Realtek Semiconductor Corp. - Net - 6001.16.180.0").as_deref(),
            Some("6001.16.180.0")
        );
        assert_eq!(version_from_title("Intel Corporation - Display - 32.0.101.9150").as_deref(), Some("32.0.101.9150"));
        assert_eq!(
            version_from_title("Realtek - Bluetooth - 1.10.1074.3002 (v1.2)").as_deref(),
            Some("1.10.1074.3002")
        );
        assert_eq!(version_from_title("Vendor - Extension - v2.1").as_deref(), Some("2.1"));
        assert_eq!(version_from_title("Advanced Micro Devices, Inc. - System - 7/18/2025 12:00:00 AM"), None);
        assert_eq!(days_to_ymd(0), "1970-01-01");
        assert_eq!(days_to_ymd(20_377), "2025-10-16");
        assert_eq!(days_to_ymd(-1), "1969-12-31");
        assert_eq!(ole_date_to_ymd(45_946.0).as_deref(), Some("2025-10-16"));
        assert_eq!(ole_date_to_ymd(f64::NAN), None);
        assert_eq!(fmt_unix(20_377 * 86_400 + 3_660), "2025-10-16 01:01 UTC");
    }

    #[test]
    fn mapping_marks_updates_and_up_to_date() {
        let mut r = rows();
        let offers = vec![
            // Newer Wi-Fi driver for the exact hardware ID (case differs).
            offer(
                r"pci\ven_10ec&dev_c852&subsys_c85210ec&rev_00",
                "Realtek Semiconductor Corp. - Net - 6001.16.180.0",
                "2026-03-01",
            ),
            // An older LAN driver than installed (Windows Update sometimes lists those).
            offer(r"PCI\VEN_10EC&DEV_8125&SUBSYS_E0001458&REV_05", "Realtek - Net - 1125.21.903.2024", "2024-09-03"),
            // Two GPU offers: the newer one wins.
            offer(
                r"PCI\VEN_8086&DEV_E20B&SUBSYS_4215172F&REV_00",
                "Intel Corporation - Display - 32.0.101.8800",
                "2026-05-01",
            ),
            offer(
                r"PCI\VEN_8086&DEV_E20B&SUBSYS_4215172F&REV_00",
                "Intel Corporation - Display - 32.0.101.9150",
                "2026-10-02",
            ),
            // An AMD component driver by compatible ID; title without a version, newer date.
            offer(r"ACPI\AMDI0010", "Advanced Micro Devices, Inc. - System - 7/18/2026", "2026-07-18"),
            // Something for a device that is not in the list.
            offer(r"USB\VID_FFFF&PID_0001", "Other - 1.0.0.1", "2026-01-01"),
        ];
        apply_offers(&mut r, Some(&offers));
        let wifi = row(&r, DriverCategory::Wifi);
        assert_eq!(wifi.status, DriverStatus::UpdateAvailable);
        assert_eq!(wifi.latest.as_ref().unwrap().version.as_deref(), Some("6001.16.180.0"));
        let lan = row(&r, DriverCategory::Lan);
        assert_eq!(lan.status, DriverStatus::UpToDate, "an older offer is not an update");
        assert!(lan.latest.is_some());
        let gpu = row(&r, DriverCategory::Gpu);
        assert_eq!(gpu.status, DriverStatus::UpdateAvailable);
        assert_eq!(gpu.latest.as_ref().unwrap().version.as_deref(), Some("32.0.101.9150"));
        let i2c = r.iter().find(|x| x.device == "AMD I2C Controller").unwrap();
        assert_eq!(i2c.status, DriverStatus::UpdateAvailable, "date fallback: 2026-07-18 > 2025-10-16");
        // Covered kinds without an offer are up to date; the rest stays unknown.
        assert_eq!(row(&r, DriverCategory::Bluetooth).status, DriverStatus::UpToDate);
        assert_eq!(row(&r, DriverCategory::Audio).status, DriverStatus::UpToDate);
        assert_eq!(row(&r, DriverCategory::Bios).status, DriverStatus::Unknown);
        assert_eq!(row(&r, DriverCategory::SuperIo).status, DriverStatus::Unknown);
        let pkg = r.iter().find(|x| x.package).unwrap();
        assert_eq!(pkg.status, DriverStatus::Unknown);
        assert_eq!(r.iter().find(|x| x.device == "AMD SMBus").unwrap().status, DriverStatus::Unknown);
        assert_eq!(r.iter().filter(|x| x.status == DriverStatus::UpdateAvailable).count(), 3);
        // Not checked: everything unknown and `latest` cleared.
        apply_offers(&mut r, None);
        assert!(r.iter().all(|x| x.status == DriverStatus::Unknown && x.latest.is_none()));
    }

    #[test]
    fn bios_is_never_matched() {
        let mut r = rows();
        let bios = row(&r, DriverCategory::Bios).clone();
        let mut o = offer("", "GIGABYTE - Firmware - 1.0.0.40", "2026-09-01");
        o.model = bios.device.clone();
        apply_offers(&mut r, Some(&[o]));
        assert_eq!(row(&r, DriverCategory::Bios).status, DriverStatus::Unknown);
        assert!(row(&r, DriverCategory::Bios).latest.is_none());
    }

    #[test]
    fn model_name_fallback() {
        let mut r = rows();
        let mut o = offer("", "Realtek - Bluetooth - 18.4032.2601.100", "2026-01-10");
        o.model = "realtek bluetooth adapter".into();
        apply_offers(&mut r, Some(&[o]));
        assert_eq!(row(&r, DriverCategory::Bluetooth).status, DriverStatus::UpdateAvailable);
    }

    fn report_json(nonce: &str) -> String {
        format!(
            r#"{{"schema":1,"nonce":"{nonce}","duration_ms":4100,"offers":[{{"title":"Intel Corporation - Display - 32.0.101.9150\u0007","version":"32.0.101.9150","date":"2026-10-02","hardware_id":"PCI\\VEN_8086&DEV_E20B","model":"Intel(R) Arc(TM) B580 Graphics","provider":"Intel Corporation","manufacturer":"Intel Corporation","class":"Display","update_id":"0b1c"}}],"error":null}}"#
        )
    }

    #[test]
    fn report_validation() {
        let r = parse_report(report_json("abc").as_bytes(), "abc").unwrap();
        assert_eq!(r.offers.len(), 1);
        assert_eq!(r.offers[0].title, "Intel Corporation - Display - 32.0.101.9150", "control characters stripped");
        // Wrong nonce, wrong schema, garbage, truncated (helper killed mid-write), unknown fields.
        assert_eq!(
            parse_report(report_json("abc").as_bytes(), "abd").unwrap_err().code,
            ErrorCode::DriverCheckCancelled
        );
        assert!(parse_report(report_json("abc").replace("\"schema\":1", "\"schema\":2").as_bytes(), "abc").is_err());
        assert!(parse_report(b"not json", "abc").is_err());
        let full = report_json("abc");
        assert!(parse_report(&full.as_bytes()[..full.len() / 2], "abc").is_err());
        assert!(parse_report(full.replace("\"error\":null", "\"error\":null,\"x\":1").as_bytes(), "abc").is_err());
        assert!(parse_report(b"{}", "").is_err());
        // A bogus version/date is dropped, not trusted.
        let r = parse_report(
            report_json("n")
                .replace("\"version\":\"32.0.101.9150\"", "\"version\":\"newest!\"")
                .replace("2026-10-02", "soon")
                .as_bytes(),
            "n",
        )
        .unwrap();
        assert_eq!(r.offers[0].version, None);
        assert_eq!(r.offers[0].date, None);
        // Error reports: only driver-check codes are accepted.
        let e = r#"{"schema":1,"nonce":"n","duration_ms":1,"offers":[],"error":{"code":"RestoreFailed","detail":"x"}}"#;
        assert_eq!(parse_report(e.as_bytes(), "n").unwrap().error.unwrap().code, ErrorCode::DriverCheckFailed);
        let e = r#"{"schema":1,"nonce":"n","duration_ms":1,"offers":[],"error":{"code":"DriverCheckOffline","detail":"0x8024402C"}}"#;
        assert_eq!(parse_report(e.as_bytes(), "n").unwrap().error.unwrap().code, ErrorCode::DriverCheckOffline);
        // Too large.
        let big = vec![b' '; MAX_REPORT_BYTES as usize + 1];
        assert!(parse_report(&big, "n").is_err());
    }

    /// Review L26: helper text cannot hide or reorder characters in the elevated UI or the report.
    #[test]
    fn helper_text_is_fully_sanitised() {
        // Right-to-left override ("Intel" U+202E "exe.gpj" shows as "Intelgpj.exe"), isolates, zero-width
        // characters, BOM, soft hyphen, tag characters, private use.
        let evil = "Intel\u{202E}exe.gpj\u{2066}x\u{2069}\u{200B}\u{200D}\u{FEFF}\u{00AD}\u{E0041}\u{E000}\u{FFFF}!";
        assert_eq!(clean(evil), "Intelexe.gpjx!");
        // Other whitespace becomes single plain spaces; ends are trimmed.
        assert_eq!(clean("\u{2028} a\u{00A0}\u{3000}\u{2029}b\t\r\nc  "), "a b c");
        // Ordinary text, accents and CJK pass.
        let plain = "Realtek Semiconductor Corp. – Net – 6001.16.180.0 (Görlitz 中文)";
        assert_eq!(clean(plain), plain);
        // Length cap counts kept characters only.
        let long = format!("{}{}", "\u{200B}".repeat(1000), "x".repeat(1000));
        assert_eq!(clean(&long).chars().count(), MAX_FIELD);
        // Through the report, including serde's error text that quotes an unknown field name.
        let r =
            parse_report(report_json("n").replace("B580 Graphics", "B580\u{202E}scihparG").as_bytes(), "n").unwrap();
        assert_eq!(r.offers[0].model, "Intel(R) Arc(TM) B580scihparG");
        let e = parse_report(br#"{"schema":1,"nonce":"n","duration_ms":1,"offers":[],"error":null,"a\u202Eb":1}"#, "n")
            .unwrap_err();
        assert!(!e.detail.contains('\u{202E}') && e.detail.contains("ab"), "{}", e.detail);
        let e = r#"{"schema":1,"nonce":"n","duration_ms":1,"offers":[],"error":{"code":"DriverCheckFailed","detail":"x\u2067y"}}"#;
        assert_eq!(parse_report(e.as_bytes(), "n").unwrap().error.unwrap().detail, "xy");
    }

    #[test]
    fn cache_round_trip_and_record() {
        let dir = crate::util::paths::test_home().join("drivers-cache");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("driver_check.json");
        let mut c = CheckCache::default();
        assert_eq!(c.offers(), None);
        c.record(1000, Ok(vec![offer("A", "x - 1.0.0.1", "2026-01-01")]));
        c.record(2000, Err(CheckError::new(ErrorCode::DriverCheckOffline, "")));
        assert_eq!(c.last_success, Some(1000));
        assert_eq!(c.last_attempt, 2000);
        assert_eq!(c.offers().unwrap().len(), 1, "a failed attempt keeps the last good result");
        save_cache(&p, &c);
        assert_eq!(load_cache(&p), c);
        std::fs::write(&p, b"{broken").unwrap();
        assert_eq!(load_cache(&p), CheckCache::default());
        assert_eq!(load_cache(&dir.join("missing.json")), CheckCache::default());
    }

    #[test]
    fn auto_schedule() {
        let day = AUTO_INTERVAL_S;
        let mut c = CheckCache::default();
        assert!(!auto_due(false, 10 * day, 3600, &c, 0), "off by default");
        assert!(!auto_due(true, 10 * day, 60, &c, 0), "not in the first 5 minutes after boot");
        assert!(auto_due(true, 10 * day, 3600, &c, 0), "never checked");
        c.record(10 * day, Ok(Vec::new()));
        assert!(!auto_due(true, 10 * day + 3600, 3600, &c, 0));
        assert!(!auto_due(true, 11 * day + 100, 3600, &c, 600), "jitter");
        assert!(auto_due(true, 11 * day + 600, 3600, &c, 600));
        c.record(20 * day, Err(CheckError::new(ErrorCode::DriverCheckMetered, "")));
        assert!(auto_due(true, 20 * day + AUTO_RETRY_S, 3600, &c, 0), "metered/offline retry hourly");
        c.record(30 * day, Err(CheckError::new(ErrorCode::DriverCheckFailed, "")));
        assert!(!auto_due(true, 30 * day + AUTO_RETRY_S, 3600, &c, 0), "real failures wait a day");
        assert!(auto_due(true, 30 * day - 5, 3600, &c, 0), "clock went backwards");
    }

    /// Review L27: "Checking…" ends whatever happens to the worker: it never ran, it died with a
    /// panic in the middle of a check, or it stopped answering.
    #[test]
    fn checking_always_ends() {
        let wait_until = |svc: &DriverService, f: &dyn Fn(&DriverState) -> bool| {
            let end = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < end {
                if f(&svc.snapshot()) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            false
        };
        // Worker gone at once: Check now does not show "Checking…".
        let svc = spawn_service(false, |_, rx| drop(rx));
        assert!(wait_until(&svc, &|_| !svc.alive.load(AtomicOrdering::SeqCst)));
        svc.check_now();
        let s = svc.snapshot();
        assert!(!s.checking && s.error.is_some());
        // Worker panics while a check runs.
        let svc = spawn_service(false, |_, rx| {
            if let Ok(Cmd::Check { .. }) = rx.recv() {
                panic!("simulated worker crash");
            }
        });
        svc.check_now();
        assert!(wait_until(&svc, &|s| !s.checking), "stuck on Checking…");
        assert_eq!(svc.snapshot().error.unwrap().code, ErrorCode::DriverCheckFailed);
        assert!(!svc.alive.load(AtomicOrdering::SeqCst));
        // Worker alive but silent: the deadline ends it, and a new check can start.
        let (hold_tx, hold_rx) = mpsc::channel::<()>();
        let svc = spawn_service(false, move |_, rx| {
            let _keep = rx;
            let _ = hold_rx.recv();
        });
        svc.check_now();
        assert!(svc.snapshot().checking);
        svc.state.lock().unwrap().check_started = Some(unix_now() - CHECK_STALE_S - 1);
        let s = svc.snapshot();
        assert!(!s.checking);
        assert_eq!(s.error.unwrap().code, ErrorCode::DriverCheckTimeout);
        svc.check_now();
        assert!(svc.snapshot().checking, "a new check starts after the stale one");
        drop(hold_tx);
        let mut s = DriverState { checking: true, check_started: Some(unix_now() + 3600), ..Default::default() };
        end_if_stale(&mut s, unix_now());
        assert!(!s.checking, "a start time in the future (clock change) does not keep it up");
    }

    #[test]
    fn vendor_urls_are_allow_listed() {
        for u in VENDOR_URLS {
            assert!(crate::util::win::ALLOWED_URLS.contains(&u), "{u}");
            assert!(crate::util::win::validate_open_target(u).is_ok(), "{u}");
            assert!(u.starts_with("https://") && !u.contains(['"', ' ']), "{u}");
        }
    }
}
