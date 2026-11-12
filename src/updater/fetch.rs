//! `fanrgb --update-fetch`: the unelevated helper that talks to GitHub.
//!
//! It runs with the desktop user's normal rights, decrypts the token if one is saved (without one it asks
//! GitHub anonymously, which a public repository allows), asks the GitHub API for the latest release of
//! the repository pinned by its numeric id, downloads `release.json`, its minisign signature and (if the
//! release is newer and correctly signed) the executable into `%LOCALAPPDATA%\FanRGB\update\staged\`, and
//! reports the result in `update\status.json`. The token changes nothing about what is trusted: the
//! repository id, the signature and every later check are the same either way.
//!
//! Everything it writes is untrusted to the elevated app, which re-verifies signature, version, replay
//! state, size and SHA-256 before installing (`install.rs`). The checks here only avoid downloading or
//! offering junk.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::http::{self, Http};
use super::manifest::{self, Version};
use super::token::{self, Token};
use crate::error::{ErrorCode, code_of, err};
use crate::util::fsx;

pub const STATUS_FILE: &str = "status.json";
pub const STAGED_DIR: &str = "staged";
pub const MANIFEST_NAME: &str = "release.json";
pub const SIGNATURE_NAME: &str = "release.json.minisig";
pub const STAGED_EXE: &str = "fanrgb.exe";
const MAX_API_JSON: u64 = 2 << 20;
pub const MAX_STATUS: u64 = 64 * 1024;

/// Result of one check, written by the helper to `status.json` and read by the UI.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckStatus {
    pub schema: u32,
    /// Unix seconds.
    pub checked_at: u64,
    /// Version of the helper that ran the check.
    pub current: String,
    /// `owner/name` of the pinned repository as GitHub reported it.
    #[serde(default)]
    pub repo: Option<String>,
    /// A newer, correctly signed release (staged next to this file).
    #[serde(default)]
    pub available: Option<Offer>,
    #[serde(default)]
    pub error: Option<StatusError>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    pub version: String,
    pub published_at: u64,
    pub notes: String,
    pub size: u64,
    /// SHA-256 of the executable (from the signed manifest): `Updater::install` installs exactly this.
    #[serde(default)]
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusError {
    /// `ErrorCode::number()`.
    pub code: u16,
    /// Technical detail, token-redacted.
    pub detail: String,
}

impl StatusError {
    pub fn code(&self) -> ErrorCode {
        ErrorCode::ALL.iter().copied().find(|c| c.number() == self.code).unwrap_or(ErrorCode::Unknown)
    }
}

/// What the helper checks against.
pub struct Pins<'a> {
    pub repo_id: u64,
    pub keys: &'a [&'a str],
    pub current: Version,
    pub now: u64,
}

#[derive(Deserialize)]
struct RepoJson {
    id: u64,
    full_name: String,
}

#[derive(Deserialize)]
struct ReleaseJson {
    #[serde(default)]
    assets: Vec<AssetJson>,
}

#[derive(Deserialize)]
struct AssetJson {
    id: u64,
    name: String,
    size: u64,
}

pub(super) fn api(path: &str) -> String {
    format!("https://{}{path}", http::API_HOST)
}

/// Maps an HTTP status of an API call to a coded error. `with_token`: the request carried the token.
pub(super) fn status_error(status: u16, what: &str, with_token: bool) -> anyhow::Error {
    match (status, with_token) {
        (401, true) => {
            err(ErrorCode::UpdateAuth, format!("{what}: HTTP 401, the token is invalid, expired or revoked"))
        }
        (403, true) => err(
            ErrorCode::UpdateAuth,
            format!("{what}: HTTP 403, the token lacks Contents: read, or the rate limit was hit"),
        ),
        (404, true) => {
            err(ErrorCode::UpdateAuth, format!("{what}: HTTP 404, the token has no access to the FanRGB repository"))
        }
        // Without a token GitHub shows a private (or moved, or deleted) repository as 404, sometimes 401.
        (401 | 404, false) => err(
            ErrorCode::UpdateNotPublic,
            format!("{what}: HTTP {status} without a token, the FanRGB repository is not public or not reachable"),
        ),
        (403 | 429, false) => err(
            ErrorCode::UpdateRateLimit,
            format!("{what}: HTTP {status} without a token, GitHub's rate limit for anonymous requests was hit"),
        ),
        (s, _) => err(ErrorCode::UpdateNetwork, format!("{what}: HTTP {s}")),
    }
}

/// The `Authorization` value for [`http::get_following`] (sent to the API host only).
pub(super) fn auth(tok: Option<&Token>) -> Option<&str> {
    tok.map(Token::as_str)
}

fn get_json<T: serde::de::DeserializeOwned>(
    h: &dyn Http,
    path: &str,
    tok: Option<&Token>,
    what: &str,
) -> Result<Option<T>> {
    let r = http::get_following(h, &api(path), "application/vnd.github+json", auth(tok), MAX_API_JSON)?;
    match r.status {
        200 => serde_json::from_slice(&r.body)
            .map(Some)
            .map_err(|e| err(ErrorCode::UpdateNetwork, format!("{what}: unexpected answer ({e})"))),
        404 if what == "latest release" => Ok(None),
        s => Err(status_error(s, what, tok.is_some())),
    }
}

fn download(h: &dyn Http, repo_id: u64, asset: &AssetJson, tok: Option<&Token>, max: u64) -> Result<Vec<u8>> {
    if asset.size > max {
        return Err(err(ErrorCode::UpdatePackage, format!("{} is {} bytes (limit {max})", asset.name, asset.size)));
    }
    let path = format!("/repositories/{repo_id}/releases/assets/{}", asset.id);
    let r = http::get_following(h, &api(&path), "application/octet-stream", auth(tok), max)?;
    if r.status != 200 {
        return Err(status_error(r.status, &asset.name, tok.is_some()));
    }
    Ok(r.body)
}

/// Resolves the pinned repository and returns its `owner/name`. The repository is addressed by its
/// numeric id, and GitHub must confirm that id: a renamed, transferred or deleted-and-recreated
/// "Zeropodaerodynamist/fanrgb" is a different repository. Runs first, with or without a token.
pub(super) fn verify_repo(h: &dyn Http, tok: Option<&Token>, repo_id: u64) -> Result<String> {
    let repo: RepoJson = get_json(h, &format!("/repositories/{repo_id}"), tok, "repository")?
        .ok_or_else(|| err(ErrorCode::UpdateNetwork, "no repository"))?;
    if repo.id != repo_id {
        return Err(err(
            ErrorCode::UpdateRepository,
            format!("GitHub answered with repository {} ({}) instead of {repo_id}", repo.id, repo.full_name),
        ));
    }
    Ok(repo.full_name)
}

/// The check itself. `Ok(None)`: up to date (or no release yet). `tok`: `None` asks GitHub anonymously.
fn check(h: &dyn Http, tok: Option<&Token>, staged: &Path, pins: &Pins) -> Result<(String, Option<Offer>)> {
    let full_name = verify_repo(h, tok, pins.repo_id)?;
    let repo = RepoJson { id: pins.repo_id, full_name };
    let Some(rel) =
        get_json::<ReleaseJson>(h, &format!("/repositories/{}/releases/latest", pins.repo_id), tok, "latest release")?
    else {
        return Ok((repo.full_name, None));
    };
    let find = |name: &str| rel.assets.iter().find(|a| a.name == name);
    let (Some(m_asset), Some(s_asset)) = (find(MANIFEST_NAME), find(SIGNATURE_NAME)) else {
        return Err(err(ErrorCode::UpdatePackage, "the latest release has no release.json + release.json.minisig"));
    };
    let m_bytes = download(h, pins.repo_id, m_asset, tok, manifest::MAX_MANIFEST)?;
    let s_bytes = download(h, pins.repo_id, s_asset, tok, manifest::MAX_SIGNATURE)?;
    manifest::verify_signature(&m_bytes, &s_bytes, pins.keys)?;
    let release = manifest::parse(&m_bytes)?;
    match manifest::check_offer(&release, pins.current, manifest::Seen::default(), pins.now) {
        Ok(()) => {}
        Err(e) if code_of(&e) == ErrorCode::UpdateDowngrade => {
            let _ = std::fs::remove_dir_all(staged);
            return Ok((repo.full_name, None));
        }
        Err(e) => return Err(e),
    }
    let exe_asset = find(&release.manifest.asset.name)
        .ok_or_else(|| err(ErrorCode::UpdatePackage, format!("the release has no {}", release.manifest.asset.name)))?;
    if exe_asset.size != release.manifest.asset.size {
        return Err(err(
            ErrorCode::UpdateHash,
            format!("asset is {} bytes, the manifest says {}", exe_asset.size, release.manifest.asset.size),
        ));
    }
    // Already downloaded by an earlier check?
    let staged_ok = fsx::read_limited(&staged.join(MANIFEST_NAME), manifest::MAX_MANIFEST).is_ok_and(|b| b == m_bytes)
        && fsx::read_limited(&staged.join(STAGED_EXE), manifest::MAX_EXE)
            .is_ok_and(|exe| manifest::verify_package(&release, &exe).is_ok());
    if !staged_ok {
        let exe = download(h, pins.repo_id, exe_asset, tok, release.manifest.asset.size)?;
        manifest::verify_package(&release, &exe)?;
        let _ = std::fs::remove_dir_all(staged);
        std::fs::create_dir_all(staged).map_err(|e| err(ErrorCode::Io, format!("{}: {e}", staged.display())))?;
        // The manifest goes last: a staged manifest implies a complete set.
        for (name, bytes) in [(STAGED_EXE, &exe), (SIGNATURE_NAME, &s_bytes), (MANIFEST_NAME, &m_bytes)] {
            fsx::write_atomic(&staged.join(name), bytes).map_err(|e| err(ErrorCode::Io, format!("{name}: {e}")))?;
        }
    }
    let m = release.manifest;
    Ok((
        repo.full_name,
        Some(Offer {
            version: m.version,
            published_at: m.published_at,
            notes: m.notes,
            size: m.asset.size,
            sha256: m.asset.sha256,
        }),
    ))
}

/// Runs one check and returns its status (never fails: errors become [`CheckStatus::error`]). `tok` is
/// [`token::load_from`]'s answer: `Ok(None)` checks without a token, an error (a saved token that cannot
/// be read) is reported as it is.
pub fn fetch(h: &dyn Http, tok: Result<Option<Token>>, root: &Path, pins: &Pins) -> CheckStatus {
    let mut st =
        CheckStatus { schema: 1, checked_at: pins.now, current: pins.current.to_string(), ..Default::default() };
    let res = tok.and_then(|t| check(h, t.as_ref(), &root.join(STAGED_DIR), pins));
    match res {
        Ok((repo, offer)) => {
            st.repo = Some(repo);
            st.available = offer;
        }
        Err(e) => {
            st.error = Some(StatusError { code: code_of(&e).number(), detail: token::redact(&format!("{e:#}")) });
        }
    }
    st
}

pub fn write_status(root: &Path, st: &CheckStatus) -> Result<()> {
    std::fs::create_dir_all(root).map_err(|e| err(ErrorCode::Io, format!("{}: {e}", root.display())))?;
    let bytes = serde_json::to_vec_pretty(st).map_err(|e| err(ErrorCode::Io, e.to_string()))?;
    fsx::write_atomic(&root.join(STATUS_FILE), &bytes).map_err(|e| err(ErrorCode::Io, e.to_string()))
}

/// Reads a status file. Any process of the user can write it, so to the elevated UI it is untrusted:
/// size-limited, links refused, error code limited to the codes the helper can
/// report, detail reduced to plain display text, repository name checked. Only the code may change
/// behaviour (FRG-E701 pauses automatic checks for a day; a process that can write this file can also
/// delete the token, so that is no new power). Offers are re-verified from the staged files.
pub fn read_status(root: &Path) -> Option<CheckStatus> {
    let bytes = fsx::read_limited(&root.join(STATUS_FILE), MAX_STATUS).ok()?;
    let mut st: CheckStatus = serde_json::from_slice(&bytes).ok()?;
    sanitize_status(&mut st);
    Some(st)
}

/// Longest error detail shown from a status file.
const MAX_DETAIL: usize = 500;

pub(crate) fn sanitize_status(st: &mut CheckStatus) {
    if let Some(e) = &mut st.error {
        if !helper_may_report(e.code) {
            e.code = ErrorCode::UpdateHelper.number();
        }
        e.detail = display_text(&token::redact(&e.detail), MAX_DETAIL);
    }
    st.repo = st.repo.take().filter(|r| is_repo_name(r));
    if let Some(o) = &mut st.available {
        o.notes = o.notes.chars().take(manifest::MAX_NOTES).collect();
    }
}

/// The codes `--update-fetch` writes: the update range up to FRG-E711, the two anonymous-request answers
/// (FRG-E714, FRG-E715) and file errors.
fn helper_may_report(code: u16) -> bool {
    (ErrorCode::UpdateTokenMissing.number()..=ErrorCode::UpdateHelper.number()).contains(&code)
        || [ErrorCode::UpdateNotPublic, ErrorCode::UpdateRateLimit, ErrorCode::Io].iter().any(|c| c.number() == code)
}

/// GitHub refused the check in a way that asking again soon will not fix (a rejected token, a repository
/// that is not public, the anonymous rate limit): the next automatic check waits a day, and
/// the helper does not go on to the driver catalog in the same run.
pub fn wait_a_day(code: ErrorCode) -> bool {
    matches!(code, ErrorCode::UpdateAuth | ErrorCode::UpdateNotPublic | ErrorCode::UpdateRateLimit)
}

/// The driver catalog download is skipped after the release check failed like this: GitHub refused it
/// ([`wait_a_day`]), or the saved token cannot be read (the catalog would fail the same way; no request
/// was made, so the next check is not delayed for it).
pub fn skip_catalog(code: ErrorCode) -> bool {
    wait_a_day(code) || code == ErrorCode::UpdateTokenMissing
}

/// `owner/name` as GitHub allows it.
fn is_repo_name(s: &str) -> bool {
    let mut parts = s.split('/');
    let ok = |p: Option<&str>| {
        p.is_some_and(|p| {
            !p.is_empty() && p.len() <= 100 && p.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
    };
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

/// Untrusted text for display: control characters become spaces; invisible formatting characters
/// (bidi overrides and isolates, zero-width characters, soft hyphen, BOM) are dropped, so the text
/// cannot reorder or hide what the UI shows; capped at `max` characters.
pub(crate) fn display_text(s: &str, max: usize) -> String {
    let invisible = |c: char| {
        matches!(c, '\u{ad}' | '\u{61c}' | '\u{180e}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}' | '\u{feff}' | '\u{fff9}'..='\u{fffb}')
    };
    s.chars().filter(|&c| !invisible(c)).map(|c| if c.is_control() { ' ' } else { c }).take(max).collect()
}

/// `<LocalAppData>\FanRGB\update`.
pub fn update_root_in(local_app_data: &Path) -> PathBuf {
    local_app_data.join(crate::util::paths::APP_DIR).join("update")
}

/// Body of `fanrgb --update-fetch`. Exit code 0 = checked (see status.json), 1 = check failed,
/// 3 = refused (elevated).
pub fn run_helper() -> i32 {
    // The network is never touched with administrator rights.
    if crate::util::win::is_elevated() {
        eprintln!("--update-fetch must run without administrator rights");
        return 3;
    }
    use windows_sys::Win32::UI::Shell::FOLDERID_LocalAppData;
    let Some(lad) = crate::util::paths::known_folder(&FOLDERID_LocalAppData) else { return 1 };
    let root = update_root_in(&lad);
    if std::fs::create_dir_all(&root).is_err() {
        return 1;
    }
    let pins =
        Pins { repo_id: super::REPO_ID, keys: &super::keys::TRUSTED, current: Version::current(), now: super::now() };
    let tok = token::load_from(&lad);
    let http = http::WinHttp::new();
    let st = match &http {
        Ok(h) => fetch(h, tok, &root, &pins),
        Err(e) => fetch_failed(&pins, err(code_of(e), format!("{e:#}"))),
    };
    let failed = st.error.is_some();
    let written = write_status(&root, &st);
    // The signed vendor driver list rides along with the daily update check: same token (or none), same
    // pinned repository. Its result does not change this exit code (the app reads its own status file).
    let staged = super::catalog_fetch::staged_in(&lad);
    let cst = match (&http, &st.error) {
        // A rejected or unreadable token, a repository that is not public, the rate limit: do not ask
        // GitHub again in this run.
        (_, Some(e)) if skip_catalog(e.code()) => super::catalog_fetch::failed(pins.now, e.code(), &e.detail),
        (Ok(h), _) => super::catalog_fetch::fetch(
            h,
            token::load_from(&lad),
            &staged,
            super::REPO_ID,
            &crate::drivers::catalog::keys::TRUSTED,
            pins.now,
        ),
        (Err(e), _) => super::catalog_fetch::failed(pins.now, ErrorCode::CatalogFetch, &format!("{e:#}")),
    };
    let _ = super::catalog_fetch::write_status(&staged, &cst);
    if written.is_err() {
        return 1;
    }
    if failed { 1 } else { 0 }
}

fn fetch_failed(pins: &Pins, e: anyhow::Error) -> CheckStatus {
    CheckStatus {
        schema: 1,
        checked_at: pins.now,
        current: pins.current.to_string(),
        error: Some(StatusError { code: code_of(&e).number(), detail: token::redact(&format!("{e:#}")) }),
        ..Default::default()
    }
}
