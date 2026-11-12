//! Signed "latest vendor driver versions" catalog (`catalog/drivers.json` + `.minisig`).
//!
//! Windows Update lags the vendors (Intel Arc, AMD chipset, Realtek, GIGABYTE BIOS), and the app must
//! never scrape vendor sites itself. A weekly CI job does the lookup and
//! publishes a catalog signed with a dedicated **catalog** key on the repository's `catalog` branch.
//! The unelevated `--update-fetch` / `--catalog-fetch` helper downloads it, with the update token if one
//! is saved (`updater::catalog_fetch`); the elevated app re-verifies the exact bytes here, refuses replays
//! ([`store`]) and matches the entries to the inventory rows ([`apply_catalog`]).
//!
//! Key separation: the catalog keys ([`keys`]) are not the release keys (`updater::keys`), and the
//! trusted comment is domain-separated (`fanrgb-catalog serial=N …` here, `fanrgb <version> …` for a
//! release). Whoever holds the catalog key can only make the driver list say misleading things: the
//! catalog carries text, version numbers and links to allow-listed vendor hosts, never code, and FanRGB
//! never downloads or installs a driver.
//!
//! Format: `catalog/schema.json` (JSON Schema 2020-12). `schema_version` is `MAJOR.MINOR`: a different
//! major is refused, a newer minor may add fields (ignored here).

pub mod keys;
pub mod runtime;
pub mod store;
pub mod version;

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

pub use version::Scheme;

use super::{CheckError, DriverCategory, DriverRow};
use crate::error::ErrorCode;

/// The schema major this build understands.
pub const SCHEMA_MAJOR: u32 = 1;
/// `kind` of every catalog (a signed JSON for something else is not a catalog).
pub const KIND: &str = "fanrgb-driver-catalog";
/// Size caps for the catalog and its signature (download, staging and cache).
pub const MAX_CATALOG_BYTES: u64 = 256 * 1024;
pub const MAX_SIGNATURE_BYTES: u64 = 4 * 1024;
pub const MAX_ENTRIES: usize = 200;
const MAX_PATTERNS: usize = 64;
/// Longest short text (vendor, component, provider pattern, ...).
const MAX_TEXT: usize = 120;
const MAX_URL: usize = 300;
/// Prefix of the minisign trusted comment of a catalog signature.
pub const TRUSTED_PREFIX: &str = "fanrgb-catalog serial=";
/// A catalog dated further ahead than this is refused (clock skew allowance, as for releases).
const FUTURE_SLACK_S: u64 = 2 * 86_400;
/// `expires` may be at most this far after `generated_at`.
const MAX_VALIDITY_S: u64 = 90 * 86_400;

fn rejected(detail: impl Into<String>) -> CheckError {
    CheckError::new(ErrorCode::CatalogRejected, detail)
}

// ---------------------------------------------------------------------------------------
// Wire format (what CI writes). Unknown fields are ignored: a newer minor schema may add some.

#[derive(Deserialize)]
struct RawCatalog {
    schema_version: String,
    kind: String,
    serial: u64,
    generated_at: String,
    expires: String,
    entries: Vec<RawEntry>,
}

#[derive(Deserialize)]
struct RawEntry {
    id: String,
    vendor: String,
    component: String,
    #[serde(rename = "match")]
    matcher: RawMatch,
    version: String,
    version_scheme: Scheme,
    #[serde(default)]
    release_date: Option<String>,
    download_url: String,
    #[serde(default)]
    release_notes_url: Option<String>,
    source: String,
    checked_at: String,
}

#[derive(Deserialize, Default)]
struct RawMatch {
    #[serde(default)]
    hardware_ids: Vec<String>,
    #[serde(default)]
    providers: Vec<String>,
    #[serde(default)]
    categories: Vec<String>,
    #[serde(default)]
    packages: Vec<String>,
    #[serde(default)]
    board: Option<RawBoard>,
}

#[derive(Deserialize)]
struct RawBoard {
    vendor: String,
    product: String,
    #[serde(default)]
    revisions: Vec<String>,
}

// ---------------------------------------------------------------------------------------
// Validated catalog

#[derive(Clone, Debug, PartialEq)]
pub struct Catalog {
    pub serial: u64,
    /// Unix seconds.
    pub generated_at: u64,
    pub expires: u64,
    pub entries: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: String,
    pub vendor: String,
    pub component: String,
    pub matcher: Match,
    pub version: String,
    pub scheme: Scheme,
    /// `YYYY-MM-DD`.
    pub release_date: Option<String>,
    pub download_url: String,
    pub release_notes_url: Option<String>,
    /// `manual` or `auto:<adapter>`.
    pub source: String,
    /// When CI last confirmed this entry against its source (unix seconds).
    pub checked_at: u64,
}

/// Which inventory rows an entry describes. Every given rule must hold (AND); within a list any value
/// may match (OR). At least one of `hardware_ids`, `packages` or `board` is set.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Match {
    /// PnP hardware or compatible ID patterns, case-insensitive, `*` and `?` wildcards.
    pub hardware_ids: Vec<String>,
    /// Case-insensitive substrings of the driver's INF provider.
    pub providers: Vec<String>,
    /// FanRGB's device classes (`gpu`, `lan`, `chipset`, `bios`, ...).
    pub categories: Vec<DriverCategory>,
    /// Installer package names (`AMD Chipset Software`).
    pub packages: Vec<String>,
    pub board: Option<Board>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Board {
    /// Case-insensitive substring of the SMBIOS board manufacturer (`GIGABYTE`).
    pub vendor: String,
    /// SMBIOS board product, compared without spaces, hyphens and case (`B650M D3HP AX`).
    pub product: String,
    /// Board revisions (`1.3`); empty = any.
    pub revisions: Vec<String>,
}

impl Catalog {
    /// (The UI uses `CatalogInfo::expired`, the same rule on the summary.)
    #[cfg(test)]
    pub fn expired(&self, now: u64) -> bool {
        now > self.expires
    }
}

/// `"gpu"` → [`DriverCategory::Gpu`].
pub fn category_from_id(s: &str) -> Option<DriverCategory> {
    Some(match s {
        "wifi" => DriverCategory::Wifi,
        "bluetooth" => DriverCategory::Bluetooth,
        "lan" => DriverCategory::Lan,
        "audio" => DriverCategory::Audio,
        "chipset" => DriverCategory::Chipset,
        "gpu" => DriverCategory::Gpu,
        "superio" => DriverCategory::SuperIo,
        "bios" => DriverCategory::Bios,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------
// Field checks

/// Short signed text: refused (not cleaned) when too long or when it contains control, invisible or
/// reordering characters (the catalog is produced by our own CI; such text means a broken or hostile
/// source, and the whole catalog is refused).
fn check_text(field: &str, s: &str, max: usize) -> Result<(), CheckError> {
    if s.trim().is_empty() {
        return Err(rejected(format!("{field} is empty")));
    }
    if s.chars().count() > max {
        return Err(rejected(format!("{field} is longer than {max} characters")));
    }
    if s.chars().any(|c| super::is_hidden(c) || (c.is_whitespace() && c != ' ')) {
        return Err(rejected(format!("{field} contains control or invisible characters")));
    }
    Ok(())
}

fn is_id(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

fn is_source(s: &str) -> bool {
    s == "manual" || s.strip_prefix("auto:").is_some_and(|a| a.len() <= 32 && is_id(a))
}

/// `YYYY-MM-DD` with a plausible month and day.
pub fn is_date(s: &str) -> bool {
    ymd_to_days(s).is_some()
}

/// Days since 1970-01-01 of `YYYY-MM-DD` (H. Hinnant's days_from_civil).
fn ymd_to_days(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let t = s.get(r)?;
        t.bytes().all(|c| c.is_ascii_digit()).then(|| t.parse().ok()).flatten()
    };
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let mdays = [
        31,
        if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1970..=9999).contains(&y) || !(1..=12).contains(&m) || d < 1 || d > mdays[(m - 1) as usize] {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// Strict RFC 3339 UTC timestamp `YYYY-MM-DDTHH:MM:SSZ` → unix seconds.
pub fn parse_timestamp(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() != 20 || b[10] != b'T' || b[13] != b':' || b[16] != b':' || b[19] != b'Z' {
        return None;
    }
    let days = ymd_to_days(&s[..10])?;
    let two = |i: usize| -> Option<u64> {
        let t = &s[i..i + 2];
        t.bytes().all(|c| c.is_ascii_digit()).then(|| t.parse().ok()).flatten()
    };
    let (h, mi, se) = (two(11)?, two(14)?, two(17)?);
    if h > 23 || mi > 59 || se > 59 {
        return None;
    }
    u64::try_from(days).ok().map(|d| d * 86_400 + h * 3600 + mi * 60 + se)
}

/// Unix seconds → `YYYY-MM-DDTHH:MM:SSZ`.
pub fn format_timestamp(t: u64) -> String {
    let s = t % 86_400;
    format!("{}T{:02}:{:02}:{:02}Z", super::days_to_ymd((t / 86_400) as i64), s / 3600, (s % 3600) / 60, s % 60)
}

/// A link the catalog may carry and FanRGB may open: `https://` on an allow-listed vendor host
/// (`util::win::VENDOR_HOSTS`), default port, no user info, a conservative character set.
pub fn url_allowed(url: &str) -> bool {
    if url.len() > MAX_URL {
        return false;
    }
    let Some((host, path)) = crate::updater::http::split_https(url) else { return false };
    let raw_host = &url["https://".len()..].split(['/', '?', '#']).next().unwrap_or("");
    // No explicit port, no upper-case trickery: the host is written exactly as allow-listed.
    raw_host == &host
        && crate::util::win::VENDOR_HOSTS.contains(&host.as_str())
        && path.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~/?#&=+,;:".contains(&b))
}

/// Hardware-ID pattern: PnP ID characters plus `*`/`?`, and at least 8 literal characters before the
/// first wildcard (no "match everything" entry).
fn is_hwid_pattern(p: &str) -> bool {
    let literal = p.find(['*', '?']).unwrap_or(p.len());
    !p.is_empty()
        && p.len() <= 200
        && literal >= 8
        && p.bytes().all(|b| b.is_ascii_alphanumeric() || b"\\&_.{}-*?#".contains(&b))
}

/// Case-insensitive glob (`*` any run, `?` one character) over ASCII.
pub fn glob_ci(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi].eq_ignore_ascii_case(&t[ti])) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(&needle.to_lowercase())
}

/// Board product as compared: upper case without spaces and hyphens (`B650M-D3HP-AX` = `B650M D3HP AX`).
pub fn board_key(s: &str) -> String {
    s.chars().filter(|c| !matches!(c, ' ' | '-' | '_')).flat_map(char::to_uppercase).collect()
}

fn validate_match(id: &str, m: RawMatch) -> Result<Match, CheckError> {
    let f = |what: &str| format!("entry {id}: match.{what}");
    let lists = [m.hardware_ids.len(), m.providers.len(), m.categories.len(), m.packages.len()];
    if lists.iter().any(|&n| n > MAX_PATTERNS) {
        return Err(rejected(format!("entry {id}: more than {MAX_PATTERNS} match values")));
    }
    for p in &m.hardware_ids {
        if !is_hwid_pattern(p) {
            return Err(rejected(format!("{} has an invalid pattern", f("hardware_ids"))));
        }
    }
    for p in m.providers.iter().chain(&m.packages) {
        check_text(&f("providers/packages"), p, MAX_TEXT)?;
    }
    let categories = m
        .categories
        .iter()
        .map(|c| category_from_id(c).ok_or_else(|| rejected(format!("{} has unknown class {c:?}", f("categories")))))
        .collect::<Result<Vec<_>, _>>()?;
    let board = match m.board {
        None => None,
        Some(b) => {
            check_text(&f("board.vendor"), &b.vendor, MAX_TEXT)?;
            check_text(&f("board.product"), &b.product, MAX_TEXT)?;
            if b.revisions.len() > 16 {
                return Err(rejected(format!("{} has too many revisions", f("board"))));
            }
            for r in &b.revisions {
                if r.is_empty() || r.len() > 8 || !r.bytes().all(|c| c.is_ascii_digit() || c == b'.') {
                    return Err(rejected(format!("{} has an invalid revision", f("board"))));
                }
            }
            Some(Board { vendor: b.vendor, product: b.product, revisions: b.revisions })
        }
    };
    if m.hardware_ids.is_empty() && m.packages.is_empty() && board.is_none() {
        return Err(rejected(format!("entry {id}: match needs hardware_ids, packages or board")));
    }
    Ok(Match { hardware_ids: m.hardware_ids, providers: m.providers, categories, packages: m.packages, board })
}

/// Parses and validates catalog bytes (whose signature was already checked). `now` bounds the dates.
pub fn parse(bytes: &[u8], now: u64) -> Result<Catalog, CheckError> {
    if bytes.len() as u64 > MAX_CATALOG_BYTES {
        return Err(rejected(format!("catalog is {} bytes (limit {MAX_CATALOG_BYTES})", bytes.len())));
    }
    // serde's message can quote catalog text: shown only after cleaning.
    let raw: RawCatalog =
        serde_json::from_slice(bytes).map_err(|e| rejected(super::clean(&format!("catalog JSON: {e}"))))?;
    let major = raw.schema_version.split('.').next().and_then(|m| m.parse::<u32>().ok());
    let minor_ok =
        raw.schema_version.split('.').nth(1).is_some_and(|m| !m.is_empty() && m.bytes().all(|b| b.is_ascii_digit()));
    if major != Some(SCHEMA_MAJOR) || !minor_ok || raw.schema_version.split('.').count() != 2 {
        return Err(rejected(format!("unsupported schema_version {:?}", super::clean(&raw.schema_version))));
    }
    if raw.kind != KIND {
        return Err(rejected("not a FanRGB driver catalog"));
    }
    if raw.serial == 0 {
        return Err(rejected("serial must be at least 1"));
    }
    let ts = |field: &str, s: &str| {
        parse_timestamp(s).ok_or_else(|| rejected(format!("{field} is not YYYY-MM-DDTHH:MM:SSZ")))
    };
    let generated_at = ts("generated_at", &raw.generated_at)?;
    let expires = ts("expires", &raw.expires)?;
    if generated_at > now + FUTURE_SLACK_S {
        return Err(rejected("catalog is dated in the future"));
    }
    if expires <= generated_at || expires - generated_at > MAX_VALIDITY_S {
        return Err(rejected("expires must be after generated_at and at most 90 days later"));
    }
    if raw.entries.len() > MAX_ENTRIES {
        return Err(rejected(format!("{} entries (limit {MAX_ENTRIES})", raw.entries.len())));
    }
    let mut entries: Vec<Entry> = Vec::with_capacity(raw.entries.len());
    for e in raw.entries {
        if !is_id(&e.id) {
            return Err(rejected("an entry has an invalid id"));
        }
        let id = e.id;
        if entries.iter().any(|x| x.id == id) {
            return Err(rejected(format!("duplicate entry id {id}")));
        }
        check_text(&format!("entry {id}: vendor"), &e.vendor, MAX_TEXT)?;
        check_text(&format!("entry {id}: component"), &e.component, MAX_TEXT)?;
        if !e.version_scheme.is_valid(&e.version) {
            return Err(rejected(format!("entry {id}: version is not a valid {} version", e.version_scheme.id())));
        }
        if e.release_date.as_deref().is_some_and(|d| !is_date(d)) {
            return Err(rejected(format!("entry {id}: release_date is not YYYY-MM-DD")));
        }
        if !url_allowed(&e.download_url) {
            return Err(rejected(format!("entry {id}: download_url is not an allow-listed https vendor page")));
        }
        if e.release_notes_url.as_deref().is_some_and(|u| !url_allowed(u)) {
            return Err(rejected(format!("entry {id}: release_notes_url is not an allow-listed https vendor page")));
        }
        if !is_source(&e.source) {
            return Err(rejected(format!("entry {id}: source must be manual or auto:<adapter>")));
        }
        let checked_at = ts(&format!("entry {id}: checked_at"), &e.checked_at)?;
        if checked_at > generated_at {
            return Err(rejected(format!("entry {id}: checked_at is after generated_at")));
        }
        let matcher = validate_match(&id, e.matcher)?;
        entries.push(Entry {
            id,
            vendor: e.vendor,
            component: e.component,
            matcher,
            version: e.version,
            scheme: e.version_scheme,
            release_date: e.release_date,
            download_url: e.download_url,
            release_notes_url: e.release_notes_url,
            source: e.source,
            checked_at,
        });
    }
    Ok(Catalog { serial: raw.serial, generated_at, expires, entries })
}

// ---------------------------------------------------------------------------------------
// Signature

/// A catalog whose signature, trusted comment and content passed every check.
#[derive(Clone, Debug, PartialEq)]
pub struct Verified {
    pub catalog: Catalog,
    /// Index into the trusted keys (0 = primary, 1 = backup).
    pub key: usize,
    pub sha256: String,
}

/// Checks the minisign signature of `bytes` against `keys` (prehashed only) and that its trusted
/// comment is a catalog one. Returns the signing key's index and the serial named in the comment.
pub fn verify_signature(bytes: &[u8], signature: &[u8], keys: &[&str]) -> Result<(usize, u64), CheckError> {
    if signature.len() as u64 > MAX_SIGNATURE_BYTES {
        return Err(rejected("signature file too large"));
    }
    let text = std::str::from_utf8(signature).map_err(|_| rejected("signature file is not text"))?;
    let sig = minisign_verify::Signature::decode(text).map_err(|e| rejected(format!("signature: {e}")))?;
    let mut last = None;
    for (i, k) in keys.iter().enumerate() {
        let pk = minisign_verify::PublicKey::from_base64(k).map_err(|e| rejected(format!("catalog key {i}: {e}")))?;
        match pk.verify(bytes, &sig, false) {
            Ok(()) => {
                // Domain separation: a release signature (trusted comment "fanrgb <version> …") is never
                // a catalog signature, even under a key that signs both.
                let serial = sig
                    .trusted_comment()
                    .strip_prefix(TRUSTED_PREFIX)
                    .and_then(|r| r.split(' ').next())
                    .filter(|n| !n.is_empty() && n.len() <= 19 && n.bytes().all(|b| b.is_ascii_digit()))
                    .and_then(|n| n.parse::<u64>().ok())
                    .ok_or_else(|| rejected("the signature is not a driver-catalog signature"))?;
                return Ok((i, serial));
            }
            Err(minisign_verify::Error::UnexpectedKeyId) => {}
            Err(e) => last = Some(e.to_string()),
        }
    }
    Err(rejected(last.unwrap_or_else(|| "signed with a key this build does not trust for the catalog".into())))
}

/// Signature (on the exact bytes) → trusted comment → content. Replay is checked by [`store`].
pub fn verify(bytes: &[u8], signature: &[u8], keys: &[&str], now: u64) -> Result<Verified, CheckError> {
    if bytes.len() as u64 > MAX_CATALOG_BYTES {
        return Err(rejected(format!("catalog is {} bytes (limit {MAX_CATALOG_BYTES})", bytes.len())));
    }
    let (key, signed_serial) = verify_signature(bytes, signature, keys)?;
    let catalog = parse(bytes, now)?;
    if catalog.serial != signed_serial {
        return Err(rejected(format!("serial {} differs from the signed serial {signed_serial}", catalog.serial)));
    }
    Ok(Verified { catalog, key, sha256: crate::updater::manifest::sha256_hex(bytes) })
}

/// minisign key id (`3C5A…`, as `minisign` prints it) of a base64 public key.
pub fn key_id(public_b64: &str) -> String {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::new();
    for chunk in public_b64.as_bytes().chunks(4).take(4) {
        if chunk.len() < 4 {
            break;
        }
        let Some(v) = chunk.iter().try_fold(0u32, |acc, &c| val(c).map(|x| (acc << 6) | x)) else { break };
        out.extend_from_slice(&[(v >> 16) as u8, (v >> 8) as u8, v as u8]);
    }
    if out.len() < 10 {
        return "?".into();
    }
    out[2..10].iter().rev().map(|b| format!("{b:02X}")).collect()
}

// ---------------------------------------------------------------------------------------
// Matching inventory rows

/// What the catalog says about one row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum CatalogStatus {
    /// No catalog loaded (not fetched yet, or refused).
    #[default]
    NoList,
    /// The catalog has no entry for this device.
    NotInList,
    /// The installed version is the catalog's, or newer.
    LatestKnown,
    /// The vendor has a newer version ([`DriverRow::catalog`]).
    Newer,
    /// An entry matches, but its version cannot be compared with the installed one.
    Incomparable,
}

impl CatalogStatus {
    pub fn label(self) -> &'static str {
        match self {
            CatalogStatus::NoList => "No vendor list",
            CatalogStatus::NotInList => "Not in list",
            CatalogStatus::LatestKnown => "Latest known",
            CatalogStatus::Newer => "Newer available",
            CatalogStatus::Incomparable => "Compare manually",
        }
    }
}

/// The catalog entry shown for a row.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CatalogMatch {
    pub id: String,
    pub vendor: String,
    pub component: String,
    pub version: String,
    pub release_date: Option<String>,
    pub download_url: String,
    pub release_notes_url: Option<String>,
    pub source: String,
    pub checked_at: u64,
}

impl CatalogMatch {
    fn of(e: &Entry) -> Self {
        CatalogMatch {
            id: e.id.clone(),
            vendor: e.vendor.clone(),
            component: e.component.clone(),
            version: e.version.clone(),
            release_date: e.release_date.clone(),
            download_url: e.download_url.clone(),
            release_notes_url: e.release_notes_url.clone(),
            source: e.source.clone(),
            checked_at: e.checked_at,
        }
    }

    /// "Intel · 2026-10-02".
    pub fn source_line(&self) -> String {
        match &self.release_date {
            Some(d) => format!("{} · {d}", self.vendor),
            None => self.vendor.clone(),
        }
    }
}

/// "Newer driver available: 32.0.101.9150 (Intel, 2026-10-02)" and the other status sentences.
pub fn status_sentence(row: &DriverRow) -> String {
    let what = if row.category == DriverCategory::Bios { "BIOS" } else { "driver" };
    match (row.catalog_status, &row.catalog) {
        (CatalogStatus::Newer, Some(m)) => {
            let date = m.release_date.as_deref().map(|d| format!(", {d}")).unwrap_or_default();
            format!("Newer {what} available: {} ({}{date})", m.version, m.vendor)
        }
        (CatalogStatus::LatestKnown, Some(m)) => format!("Latest known ({} lists {})", m.vendor, m.version),
        (CatalogStatus::Incomparable, Some(m)) => {
            format!(
                "{} lists {}; compare it with the installed {} on the vendor page",
                m.vendor, m.version, row.installed
            )
        }
        (s, _) => s.label().to_string(),
    }
}

fn entry_matches(e: &Entry, row: &DriverRow) -> bool {
    let m = &e.matcher;
    if !m.categories.is_empty() && !m.categories.contains(&row.category) {
        return false;
    }
    if !m.providers.is_empty() && !m.providers.iter().any(|p| contains_ci(&row.provider, p)) {
        return false;
    }
    let mut keyed = false;
    if !m.hardware_ids.is_empty() {
        if row.package || row.category == DriverCategory::Bios {
            return false;
        }
        if !row.hardware_ids.iter().any(|id| m.hardware_ids.iter().any(|p| glob_ci(p, id))) {
            return false;
        }
        keyed = true;
    }
    if !m.packages.is_empty() {
        let Some(name) = row.package_name.as_deref() else { return false };
        if !m.packages.iter().any(|p| p.eq_ignore_ascii_case(name)) {
            return false;
        }
        keyed = true;
    }
    if let Some(b) = &m.board {
        let Some(rb) = &row.board else { return false };
        if !contains_ci(&rb.vendor, &b.vendor) || board_key(&rb.product) != board_key(&b.product) {
            return false;
        }
        if !b.revisions.is_empty() && !b.revisions.iter().any(|r| rb.revisions.contains(r)) {
            return false;
        }
        keyed = true;
    }
    keyed
}

/// Sets `catalog` and `catalog_status` of every row. `None`: no catalog (all `NoList`). Among several
/// matching entries the comparable ones win, and among those the newest version.
pub fn apply_catalog(rows: &mut [DriverRow], cat: Option<&Catalog>) {
    for row in rows.iter_mut() {
        row.catalog = None;
        row.catalog_status = CatalogStatus::NoList;
        let Some(cat) = cat else { continue };
        row.catalog_status = CatalogStatus::NotInList;
        let candidates: Vec<&Entry> = cat.entries.iter().filter(|e| entry_matches(e, row)).collect();
        let Some(first) = candidates.first() else { continue };
        let mut best: Option<(&Entry, Ordering)> = None;
        for e in &candidates {
            let Some(o) = e.scheme.compare(&row.installed, &e.version) else { continue };
            let better = match best {
                None => true,
                Some((b, _)) => {
                    b.scheme == e.scheme && e.scheme.compare_latest(&e.version, &b.version) == Some(Ordering::Greater)
                }
            };
            if better {
                best = Some((e, o));
            }
        }
        let (entry, status) = match best {
            Some((e, Ordering::Greater)) => (e, CatalogStatus::Newer),
            Some((e, _)) => (e, CatalogStatus::LatestKnown),
            None => (*first, CatalogStatus::Incomparable),
        };
        row.catalog = Some(CatalogMatch::of(entry));
        row.catalog_status = status;
    }
}

// ---------------------------------------------------------------------------------------
// Demo preview (demo instances never fetch; this shows what a signed list looks like)

/// A catalog for the demo's preview, dated `now` (validated like a real one; never signed).
pub fn demo_catalog(now: u64) -> Catalog {
    let ts = format_timestamp;
    let (gen_at, checked) = (ts(now.saturating_sub(3 * 86_400)), ts(now.saturating_sub(3 * 86_400)));
    let json = serde_json::json!({
        "schema_version": "1.0", "kind": KIND, "serial": 1,
        "generated_at": gen_at, "expires": ts(now + 18 * 86_400),
        "entries": [
            { "id": "intel-arc-graphics", "vendor": "Intel", "component": "Intel Arc Graphics driver",
              "match": { "categories": ["gpu"], "hardware_ids": ["PCI\\VEN_8086&DEV_56*", "PCI\\VEN_8086&DEV_E2*"] },
              "version": "32.0.101.9150", "version_scheme": "intel-gfx", "release_date": "2026-10-02",
              "download_url": crate::drivers::URL_INTEL_ARC, "source": "auto:intel-arc", "checked_at": checked },
            { "id": "amd-chipset-am5", "vendor": "AMD", "component": "AMD Chipset Software",
              "match": { "categories": ["chipset"], "packages": ["AMD Chipset Software"] },
              "version": "8.08.12.551", "version_scheme": "amd-chipset", "release_date": "2026-08-14",
              "download_url": crate::drivers::URL_AMD_B650, "source": "manual", "checked_at": checked },
            { "id": "realtek-lan-netadaptercx", "vendor": "Realtek", "component": "Realtek 2.5GbE NetAdapterCx driver",
              "match": { "categories": ["lan"], "hardware_ids": ["PCI\\VEN_10EC&DEV_8125*"] },
              "version": "11.031.50", "version_scheme": "realtek-lan", "release_date": "2026-10-05",
              "download_url": crate::drivers::URL_REALTEK_LAN, "source": "auto:realtek-lan", "checked_at": checked },
        ]
    });
    let bytes = serde_json::to_vec(&json).unwrap_or_default();
    parse(&bytes, now).unwrap_or(Catalog { serial: 1, generated_at: now, expires: now + 86_400, entries: Vec::new() })
}

#[cfg(test)]
pub(crate) mod tests;
