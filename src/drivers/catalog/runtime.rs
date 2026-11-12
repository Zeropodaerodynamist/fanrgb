//! The catalog part of the driver service worker (elevated or user app): the catalog in use, its
//! download state, and the `--catalog-fetch` helper run.
//!
//! Cadence: the daily update check (`--update-fetch`) downloads the catalog too and then calls
//! [`crate::drivers::DriverService::catalog_staged`]; "Check now" on the driver list runs
//! `--catalog-fetch`. The update token is optional (without one the helper asks GitHub anonymously);
//! nothing is downloaded in a demo instance.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::store::{self, Outcome};
use super::{Catalog, Verified, key_id};
use crate::drivers::{CatalogInfo, CatalogSummary, CheckError};
use crate::error::ErrorCode;

/// The catalog helper is killed after this long (it makes three small requests).
pub const HELPER_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Runtime {
    /// The catalog rows are matched against.
    pub catalog: Option<Catalog>,
    pub info: CatalogInfo,
    /// Admin-only folder of the replay state and the cache.
    dir: PathBuf,
    /// The desktop user's staging folder (`None` until resolved, or in a demo).
    staged: Option<PathBuf>,
    /// The desktop user's `%LOCALAPPDATA%` (token presence); `None` in tests and demos.
    lad: Option<PathBuf>,
    keys: Vec<String>,
}

fn summary(v: &Verified, keys: &[String]) -> CatalogSummary {
    CatalogSummary {
        serial: v.catalog.serial,
        generated_at: v.catalog.generated_at,
        expires: v.catalog.expires,
        key: v.key,
        key_id: keys.get(v.key).map(|k| key_id(k)).unwrap_or_else(|| "?".into()),
        entries: v.catalog.entries.len(),
    }
}

impl Runtime {
    /// The real service: compiled-in keys, the data folder, the desktop user's staging folder.
    pub fn for_app(now: u64) -> Runtime {
        let demo = crate::updater::is_disabled();
        let lad = if demo { None } else { crate::util::win::desktop_user_local_app_data().ok() };
        let token_saved = lad.as_deref().is_some_and(crate::updater::token::exists_in);
        let staged = lad.as_deref().map(crate::updater::catalog_fetch::staged_in);
        let keys = super::keys::TRUSTED.iter().map(|k| k.to_string()).collect();
        let mut rt = Runtime::new(crate::config::catalog_dir(), staged, keys, demo, now);
        rt.info.token_saved = token_saved;
        rt.lad = lad;
        rt
    }

    /// Loads the cached catalog (re-verified) or, in a demo, the built-in preview.
    pub fn new(dir: PathBuf, staged: Option<PathBuf>, keys: Vec<String>, demo: bool, now: u64) -> Runtime {
        let mut rt =
            Runtime { catalog: None, info: CatalogInfo { demo, ..Default::default() }, dir, staged, lad: None, keys };
        if demo {
            let c = super::demo_catalog(now);
            rt.info.loaded = Some(CatalogSummary {
                serial: c.serial,
                generated_at: c.generated_at,
                expires: c.expires,
                key: 0,
                key_id: "demo".into(),
                entries: c.entries.len(),
            });
            rt.info.fetched_at = Some(c.generated_at);
            rt.catalog = Some(c);
            return rt;
        }
        let rec = store::load_record(&rt.dir);
        rt.info.fetched_at = rec.fetched_at;
        rt.info.last_attempt = rec.last_attempt;
        rt.info.error = rec.error;
        let keys: Vec<&str> = rt.keys.iter().map(String::as_str).collect();
        if let Some(v) = store::load_cached(&rt.dir, &keys, now) {
            rt.info.loaded = Some(summary(&v, &rt.keys));
            rt.catalog = Some(v.catalog);
        }
        rt
    }

    /// Re-verifies what a helper staged and switches to it when it passes (the previous catalog stays
    /// otherwise). Returns the outcome for the log.
    pub fn evaluate_staged(&mut self, now: u64) -> Result<Outcome, CheckError> {
        if self.info.demo {
            return Ok(Outcome::Unchanged);
        }
        let Some(staged) = self.staged.clone() else {
            return Err(CheckError::new(ErrorCode::CatalogFetch, "the desktop user's folder is not available"));
        };
        // The token may have been saved (or forgotten) since start: the update helper just ran with it.
        if let Some(lad) = &self.lad {
            self.info.token_saved = crate::updater::token::exists_in(lad);
        }
        let keys: Vec<&str> = self.keys.iter().map(String::as_str).collect();
        self.info.last_attempt = Some(now);
        match store::evaluate_staged(&staged, &self.dir, &keys, now) {
            Ok((v, outcome)) => {
                self.info.loaded = Some(summary(&v, &self.keys));
                self.info.fetched_at = Some(now);
                self.info.error = None;
                self.catalog = Some(v.catalog);
                Ok(outcome)
            }
            Err(e) => {
                self.info.error = Some(e.clone());
                Err(e)
            }
        }
    }

    /// "Check now": runs `fanrgb --catalog-fetch` unelevated (killed after [`HELPER_TIMEOUT`]) and
    /// evaluates its result. The helper uses the update token if one is saved, and none otherwise.
    pub fn fetch_now(&mut self, now: u64) -> Result<Outcome, CheckError> {
        if self.info.demo {
            return Ok(Outcome::Unchanged);
        }
        if let Some(lad) = &self.lad {
            self.info.token_saved = crate::updater::token::exists_in(lad);
        }
        if let Err(e) = run_helper() {
            self.info.last_attempt = Some(now);
            self.info.error = Some(e.clone());
            store::record_failure(&self.dir, &e, now);
            return Err(e);
        }
        self.evaluate_staged(now)
    }
}

/// Starts the catalog helper with the user's normal rights and waits for it (status file decides).
fn run_helper() -> Result<(), CheckError> {
    let fail = |m: String| CheckError::new(ErrorCode::CatalogFetch, m);
    let exe = std::env::current_exe().map_err(|e| fail(e.to_string()))?;
    let mut child = crate::util::win::spawn_unelevated_child_tied(&exe, &[crate::updater::catalog_fetch::FETCH_ARG])
        .map_err(|e| fail(format!("{e:#}")))?;
    let deadline = Instant::now() + HELPER_TIMEOUT;
    loop {
        match child.try_wait() {
            // Exit code 1 = the status file names the failure; read it like a success.
            Ok(Some(_)) => return Ok(()),
            Ok(None) if Instant::now() >= deadline => {
                child.kill();
                return Err(fail(format!("the catalog download did not finish within {} s", HELPER_TIMEOUT.as_secs())));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => {
                child.kill();
                return Err(fail(format!("cannot watch the catalog helper: {e}")));
            }
        }
    }
}
