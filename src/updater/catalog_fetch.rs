//! Download of the signed driver catalog (`drivers::catalog`) by the unelevated helper.
//!
//! Same trust path as the release check: the optional access token is decrypted only here (without one the
//! requests are anonymous), every request goes to the repository pinned by its numeric id
//! (`/repositories/<id>` must confirm that id), WinHTTP with manual redirects and the token sent to
//! `api.github.com` only ([`super::http`]). The catalog is read
//! from the `catalog` branch through the contents API (raw media type), capped at
//! [`MAX_CATALOG_BYTES`], and its signature is checked before anything is staged. The elevated app
//! re-verifies the staged bytes and applies the replay rules (`drivers::catalog::store`).
//!
//! Runs as part of `fanrgb --update-fetch` (the daily update check) and alone as
//! `fanrgb --catalog-fetch` ("Check now" on the driver list).

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::fetch::{StatusError, api, auth, status_error, verify_repo};
use super::http::{self, Http};
use super::token::{self, Token};
use crate::drivers::catalog::{self, MAX_CATALOG_BYTES, MAX_SIGNATURE_BYTES};
use crate::error::{ErrorCode, code_of, err};
use crate::util::fsx;

/// Command-line flag of the catalog-only helper run.
pub const FETCH_ARG: &str = "--catalog-fetch";
/// Branch the CI job publishes to (never `main` or a code branch).
pub const BRANCH: &str = "catalog";
/// Paths in the repository.
pub const REMOTE_FILE: &str = "catalog/drivers.json";
pub const REMOTE_SIG: &str = "catalog/drivers.json.minisig";
/// Staged names in `%LOCALAPPDATA%\FanRGB\update\catalog\`.
pub const DIR: &str = "catalog";
pub const FILE: &str = "drivers.json";
pub const SIG_FILE: &str = "drivers.json.minisig";
pub const STATUS_FILE: &str = "status.json";
const MAX_STATUS: u64 = 16 * 1024;
const MAX_DETAIL: usize = 500;

/// Result of one catalog download, written by the helper and read (as untrusted input) by the app.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchStatus {
    pub schema: u32,
    /// Unix seconds.
    pub checked_at: u64,
    /// Serial of the staged catalog.
    #[serde(default)]
    pub serial: Option<u64>,
    #[serde(default)]
    pub error: Option<StatusError>,
}

/// `<LocalAppData>\FanRGB\update\catalog`.
pub fn staged_in(local_app_data: &Path) -> PathBuf {
    super::fetch::update_root_in(local_app_data).join(DIR)
}

fn contents_url(repo_id: u64, path: &str) -> String {
    api(&format!("/repositories/{repo_id}/contents/{path}?ref={BRANCH}"))
}

/// One file of the `catalog` branch, raw. `None`: not published (404 after the repository resolved).
fn get_file(h: &dyn Http, tok: Option<&Token>, repo_id: u64, path: &str, max: u64) -> Result<Option<Vec<u8>>> {
    let with_token = tok.is_some();
    let r = http::get_following(h, &contents_url(repo_id, path), "application/vnd.github.raw+json", auth(tok), max)
        .map_err(|e| match code_of(&e) {
            // Transport problems (unreachable, refused redirect, over the size cap) are catalog
            // download failures; token and repository problems keep their own codes.
            ErrorCode::UpdateAuth | ErrorCode::UpdateTokenMissing | ErrorCode::UpdateRepository => e,
            _ => err(ErrorCode::CatalogFetch, format!("{path}: {e:#}")),
        })?;
    match r.status {
        200 if r.body.len() as u64 <= max => Ok(Some(r.body)),
        200 => Err(err(ErrorCode::CatalogFetch, format!("{path} is larger than {max} bytes"))),
        404 => Ok(None),
        // Token, repository and rate-limit answers keep their own codes; anything else is a failed download.
        s => Err(match code_of(&status_error(s, path, with_token)) {
            ErrorCode::UpdateNetwork => err(ErrorCode::CatalogFetch, format!("{path}: HTTP {s}")),
            _ => status_error(s, path, with_token),
        }),
    }
}

/// Repository pin â†’ catalog + signature â†’ signature and content check â†’ stage. Returns the serial.
/// `tok`: `None` asks GitHub anonymously.
fn check(h: &dyn Http, tok: Option<&Token>, staged: &Path, repo_id: u64, keys: &[&str], now: u64) -> Result<u64> {
    verify_repo(h, tok, repo_id)?;
    let not_published =
        || err(ErrorCode::CatalogFetch, format!("not published yet: no driver catalog on the {BRANCH} branch"));
    let bytes = get_file(h, tok, repo_id, REMOTE_FILE, MAX_CATALOG_BYTES)?.ok_or_else(not_published)?;
    let sig = get_file(h, tok, repo_id, REMOTE_SIG, MAX_SIGNATURE_BYTES)?.ok_or_else(not_published)?;
    // Only avoids staging junk: the elevated side checks everything again, plus the serial.
    let v = catalog::verify(&bytes, &sig, keys, now).map_err(|e| err(e.code, e.detail))?;
    std::fs::create_dir_all(staged).map_err(|e| err(ErrorCode::Io, format!("{}: {e}", staged.display())))?;
    for (name, b) in [(SIG_FILE, &sig), (FILE, &bytes)] {
        fsx::write_atomic(&staged.join(name), b).map_err(|e| err(ErrorCode::Io, format!("{name}: {e}")))?;
    }
    Ok(v.catalog.serial)
}

/// Runs one download into `staged` and writes its status there (never fails: errors go in the status).
/// `tok` is [`token::load_from`]'s answer: `Ok(None)` downloads without a token.
pub fn fetch(
    h: &dyn Http,
    tok: Result<Option<Token>>,
    staged: &Path,
    repo_id: u64,
    keys: &[&str],
    now: u64,
) -> FetchStatus {
    let mut st = FetchStatus { schema: 1, checked_at: now, ..Default::default() };
    match tok.and_then(|t| check(h, t.as_ref(), staged, repo_id, keys, now)) {
        Ok(serial) => st.serial = Some(serial),
        Err(e) => st.error = Some(StatusError { code: code_of(&e).number(), detail: token::redact(&format!("{e:#}")) }),
    }
    st
}

/// A status for a run that never reached GitHub (no HTTP stack, or the release check already found the
/// token rejected or unreadable, the repository not public, or the rate limit hit).
pub fn failed(now: u64, code: ErrorCode, detail: &str) -> FetchStatus {
    FetchStatus {
        schema: 1,
        checked_at: now,
        serial: None,
        error: Some(StatusError { code: code.number(), detail: token::redact(detail) }),
    }
}

pub fn write_status(staged: &Path, st: &FetchStatus) -> Result<()> {
    std::fs::create_dir_all(staged).map_err(|e| err(ErrorCode::Io, format!("{}: {e}", staged.display())))?;
    let bytes = serde_json::to_vec_pretty(st).map_err(|e| err(ErrorCode::Io, e.to_string()))?;
    fsx::write_atomic(&staged.join(STATUS_FILE), &bytes).map_err(|e| err(ErrorCode::Io, e.to_string()))
}

/// The codes the helper may report: token, authentication, network, repository pin, repository not public,
/// rate limit, catalog, files.
fn helper_may_report(code: u16) -> bool {
    [
        ErrorCode::UpdateTokenMissing,
        ErrorCode::UpdateAuth,
        ErrorCode::UpdateNetwork,
        ErrorCode::UpdateRepository,
        ErrorCode::UpdateNotPublic,
        ErrorCode::UpdateRateLimit,
        ErrorCode::CatalogFetch,
        ErrorCode::CatalogRejected,
        ErrorCode::Io,
    ]
    .iter()
    .any(|c| c.number() == code)
}

/// Reads the helper's status (untrusted: size-capped, links refused, codes limited, text cleaned).
pub fn read_status(staged: &Path) -> Option<FetchStatus> {
    let bytes = fsx::read_limited(&staged.join(STATUS_FILE), MAX_STATUS).ok()?;
    let mut st: FetchStatus = serde_json::from_slice(&bytes).ok()?;
    if let Some(e) = &mut st.error {
        if !helper_may_report(e.code) {
            e.code = ErrorCode::CatalogFetch.number();
        }
        e.detail = super::fetch::display_text(&token::redact(&e.detail), MAX_DETAIL);
    }
    Some(st)
}

/// Body of `fanrgb --catalog-fetch`. Exit code 0 = staged, 1 = failed (see status.json), 3 = refused
/// (elevated).
pub fn run_helper() -> i32 {
    if crate::util::win::is_elevated() {
        eprintln!("{FETCH_ARG} must run without administrator rights");
        return 3;
    }
    use windows_sys::Win32::UI::Shell::FOLDERID_LocalAppData;
    let Some(lad) = crate::util::paths::known_folder(&FOLDERID_LocalAppData) else { return 1 };
    let staged = staged_in(&lad);
    let now = super::now();
    let st = match http::WinHttp::new() {
        Ok(h) => fetch(&h, token::load_from(&lad), &staged, super::REPO_ID, &catalog::keys::TRUSTED, now),
        Err(e) => failed(now, ErrorCode::CatalogFetch, &format!("{e:#}")),
    };
    let failed = st.error.is_some();
    if write_status(&staged, &st).is_err() {
        return 1;
    }
    if failed { 1 } else { 0 }
}
