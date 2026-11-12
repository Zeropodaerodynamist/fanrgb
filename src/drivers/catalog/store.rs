//! The elevated side of the catalog: re-verification of what the unelevated helper staged, replay
//! protection and the last-good cache.
//!
//! Everything lives in FanRGB's admin-only data folder (`%ProgramData%\FanRGB`; the demo's redirected
//! folder in demo mode):
//! - `driver_catalog_state.json`: the highest serial accepted and the SHA-256 of that catalog;
//! - `driver_catalog.json` + `.minisig`: the last catalog that passed every check, re-verified on load.
//!
//! Serial rules: a higher serial is a new catalog; the same serial with the same bytes is the daily
//! re-fetch of an unchanged catalog; a lower serial, or the same serial with other bytes, is refused
//! (an older signed catalog replayed, or two different catalogs under one serial).

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{MAX_CATALOG_BYTES, MAX_SIGNATURE_BYTES, Verified, verify};
use crate::drivers::CheckError;
use crate::error::ErrorCode;
use crate::util::{fsx, paths};

pub const CACHE_FILE: &str = "driver_catalog.json";
pub const CACHE_SIG_FILE: &str = "driver_catalog.json.minisig";
pub const RECORD_FILE: &str = "driver_catalog_state.json";
const MAX_RECORD: u64 = 16 * 1024;

/// Replay state and the last fetch outcome.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CatalogRecord {
    pub schema: u32,
    /// Highest serial accepted so far.
    pub last_serial: u64,
    /// SHA-256 of the catalog accepted with `last_serial`.
    pub last_sha256: String,
    /// Key that signed it (0 = primary, 1 = backup).
    pub key: usize,
    /// Last time a downloaded catalog passed every check (new or unchanged).
    pub fetched_at: Option<u64>,
    /// Last fetch attempt, successful or not.
    pub last_attempt: Option<u64>,
    /// The last attempt's failure (cleared by a success).
    pub error: Option<CheckError>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A higher serial: cached and now in use.
    New,
    /// The catalog already in use, fetched again.
    Unchanged,
}

/// Reads a file of the data folder; in the admin-only folder it must be owned by Administrators/SYSTEM.
fn read_state_file(path: &Path, max: u64) -> std::io::Result<Vec<u8>> {
    if paths::state_dir_secure() { fsx::read_limited_admin_owned(path, max) } else { fsx::read_limited(path, max) }
}

pub fn load_record(dir: &Path) -> CatalogRecord {
    match read_state_file(&dir.join(RECORD_FILE), MAX_RECORD) {
        Ok(b) => serde_json::from_slice::<CatalogRecord>(&b)
            .map(|mut r| {
                r.error = r.error.map(|e| CheckError::new(e.code, crate::drivers::clean(&e.detail)));
                r
            })
            .unwrap_or_else(|e| {
                log::warn!("[{}] {} unreadable ({e}); starting fresh", ErrorCode::Io, RECORD_FILE);
                CatalogRecord::default()
            }),
        Err(_) => CatalogRecord::default(),
    }
}

fn save_record(dir: &Path, rec: &CatalogRecord) -> Result<(), CheckError> {
    let io = |e: std::io::Error| CheckError::new(ErrorCode::Io, format!("{RECORD_FILE}: {e}"));
    let bytes = serde_json::to_vec_pretty(rec).map_err(|e| CheckError::new(ErrorCode::Io, e.to_string()))?;
    fsx::write_atomic(&dir.join(RECORD_FILE), &bytes).map_err(io)
}

/// Accepts a downloaded catalog (`bytes` exactly as signed): signature, content, serial; then caches it
/// and records the serial. Nothing is changed when it is refused.
pub fn accept(
    dir: &Path,
    bytes: &[u8],
    sig: &[u8],
    keys: &[&str],
    now: u64,
) -> Result<(Verified, Outcome), CheckError> {
    let v = verify(bytes, sig, keys, now)?;
    let mut rec = load_record(dir);
    let serial = v.catalog.serial;
    if serial < rec.last_serial {
        return Err(CheckError::new(
            ErrorCode::CatalogRejected,
            format!("serial {serial} is older than the catalog already seen ({}): replayed list", rec.last_serial),
        ));
    }
    let outcome = if serial == rec.last_serial {
        if v.sha256 != rec.last_sha256 {
            return Err(CheckError::new(
                ErrorCode::CatalogRejected,
                format!("serial {serial} was already used by a different catalog"),
            ));
        }
        Outcome::Unchanged
    } else {
        Outcome::New
    };
    // The cache first, then the record: the record never names a catalog that is not on disk.
    let io = |name: &str, e: std::io::Error| CheckError::new(ErrorCode::Io, format!("{name}: {e}"));
    fsx::write_atomic(&dir.join(CACHE_FILE), bytes).map_err(|e| io(CACHE_FILE, e))?;
    fsx::write_atomic(&dir.join(CACHE_SIG_FILE), sig).map_err(|e| io(CACHE_SIG_FILE, e))?;
    rec.schema = 1;
    rec.last_serial = serial;
    rec.last_sha256 = v.sha256.clone();
    rec.key = v.key;
    rec.fetched_at = Some(now);
    rec.last_attempt = Some(now);
    rec.error = None;
    save_record(dir, &rec)?;
    Ok((v, outcome))
}

/// Records a failed fetch; the cached catalog stays in use.
pub fn record_failure(dir: &Path, e: &CheckError, now: u64) {
    let mut rec = load_record(dir);
    rec.schema = 1;
    rec.last_attempt = Some(now);
    rec.error = Some(e.clone());
    if let Err(e) = save_record(dir, &rec) {
        log::warn!("{e}");
    }
}

/// The cached catalog, re-verified (signature, content) and matching the recorded serial and hash.
pub fn load_cached(dir: &Path, keys: &[&str], now: u64) -> Option<Verified> {
    let bytes = read_state_file(&dir.join(CACHE_FILE), MAX_CATALOG_BYTES).ok()?;
    let sig = read_state_file(&dir.join(CACHE_SIG_FILE), MAX_SIGNATURE_BYTES).ok()?;
    let rec = load_record(dir);
    match verify(&bytes, &sig, keys, now) {
        Ok(v) if v.catalog.serial == rec.last_serial && v.sha256 == rec.last_sha256 => Some(v),
        Ok(v) => {
            log::warn!(
                "[{}] cached driver catalog (serial {}) does not match the recorded serial {}; ignored",
                ErrorCode::CatalogRejected,
                v.catalog.serial,
                rec.last_serial
            );
            None
        }
        Err(e) => {
            log::warn!("cached driver catalog ignored: {e}");
            None
        }
    }
}

/// After a helper run: reads its status and staged files from the desktop user's
/// `%LOCALAPPDATA%\FanRGB\update\catalog` (`staged`, writable by any process of that user, so hostile
/// input: size-capped, links refused, everything re-verified) and accepts them.
pub fn evaluate_staged(staged: &Path, dir: &Path, keys: &[&str], now: u64) -> Result<(Verified, Outcome), CheckError> {
    let res = (|| {
        let st = crate::updater::catalog_fetch::read_status(staged)
            .ok_or_else(|| CheckError::new(ErrorCode::CatalogFetch, "the catalog helper wrote no result"))?;
        if let Some(e) = st.error {
            return Err(CheckError::new(e.code(), e.detail));
        }
        let read = |name: &str, max: u64| {
            fsx::read_limited(&staged.join(name), max)
                .map_err(|e| CheckError::new(ErrorCode::CatalogFetch, format!("staged {name}: {e}")))
        };
        let bytes = read(crate::updater::catalog_fetch::FILE, MAX_CATALOG_BYTES)?;
        let sig = read(crate::updater::catalog_fetch::SIG_FILE, MAX_SIGNATURE_BYTES)?;
        accept(dir, &bytes, &sig, keys, now)
    })();
    if let Err(e) = &res {
        record_failure(dir, e, now);
    }
    res
}
