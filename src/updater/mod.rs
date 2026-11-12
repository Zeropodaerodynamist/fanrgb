//! App self-update from the FanRGB GitHub repository.
//!
//! The daily check needs no account: without a saved access token the helper asks GitHub's API
//! anonymously (a public repository; 60 requests an hour per address). A token ([`token`]) is optional,
//! for a private repository or a higher rate limit, and changes nothing about what is trusted.
//!
//! Trust comes from an offline Ed25519 key, not from GitHub: a release is installed only if its
//! `release.json` carries a minisign signature by one of the two compiled-in keys ([`keys`]), names a
//! newer version than the running one and than any update installed before, is not older than a
//! manifest already seen, and the executable matches the manifest's size and SHA-256.
//!
//! Process split:
//! - `fanrgb --update-fetch` ([`fetch`]) runs with the user's normal rights, holds the access token if one
//!   is saved ([`token`]) and is the only code that opens a network connection ([`http`], WinHTTP).
//! - The elevated app never touches the network or the token's plaintext. It re-verifies the staged
//!   files ([`install::verify_staged`]), swaps the executable in `%ProgramFiles%\FanRGB` keeping
//!   `fanrgb.exe.old`, and hands over to the new version; `--update-finish` rolls back if the new
//!   version's engine does not run steadily within 180 s.
//!
//! UI API: [`Updater`] (one per UI, driven by [`Updater::tick`]) and its [`UpdateUi`] snapshot.

pub mod catalog_fetch;
pub mod fetch;
pub mod http;
pub mod install;
pub mod keys;
pub mod manifest;
pub mod token;

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::engine::Shared;
use crate::error::{ErrorCode, code_of};
use manifest::Version;

/// Owner/name of the release repository (for messages; requests use [`REPO_ID`]).
pub const REPO: &str = "Zeropodaerodynamist/fanrgb";
/// Numeric GitHub id of [`REPO`] (`gh api repos/Zeropodaerodynamist/fanrgb --jq .id`). Pinned: a
/// renamed, transferred or deleted-and-recreated repository with the same name has another id.
pub const REPO_ID: u64 = 1_455_226_413;
/// Command-line flag of the unelevated helper.
pub const FETCH_ARG: &str = "--update-fetch";

/// Lets `tools/release.ps1` check that the version it puts in the manifest is the one compiled into the
/// executable (a mismatch would offer the same update again after every install).
#[used]
static VERSION_MARKER: &str = concat!("FANRGB_VERSION=", env!("CARGO_PKG_VERSION"), ";");

const CHECK_EVERY: Duration = Duration::from_secs(24 * 3600);
/// First automatic check after start (lets start-up settle).
const FIRST_CHECK_AFTER: Duration = Duration::from_secs(20);
/// The helper is killed after this long.
const HELPER_TIMEOUT: Duration = Duration::from_secs(180);

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

static DISABLED: OnceLock<bool> = OnceLock::new();

/// Called once by `main`: demo instances never check for or install updates.
pub fn init(demo: bool) {
    let _ = DISABLED.set(demo);
    // Referenced from code, so the linker keeps the marker (`#[used]` alone does not stop /OPT:REF).
    std::hint::black_box(VERSION_MARKER);
}

fn disabled() -> bool {
    DISABLED.get().copied().unwrap_or(false)
}

/// Demo instance: no update check, no catalog download (the driver list shows a labelled preview).
pub fn is_disabled() -> bool {
    disabled()
}

/// What the update UI shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UpdateUi {
    /// Running version.
    pub current: String,
    /// A newer release that passed every check of the elevated side (signature, version, replay,
    /// size, SHA-256). Fields come from the verified manifest, not from the helper's status file.
    pub available: Option<fetch::Offer>,
    /// Release notes of `available` (plain text, ≤ 8 KiB).
    pub notes: String,
    pub status: Status,
    /// The last problem (code + technical detail, token-redacted).
    pub error: Option<(ErrorCode, String)>,
    /// Unix seconds of the last completed check.
    pub last_checked: Option<u64>,
    /// An access token is saved for the desktop user (optional: without one the check is anonymous).
    pub token_saved: bool,
    /// Repository GitHub resolved for the pinned id (shown after the first successful check).
    pub repo: Option<String>,
    /// One line about the last install / rollback (from the admin-only update record).
    pub last_result: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    /// No check has run since start (the first automatic one follows shortly when enabled).
    #[default]
    Idle,
    /// Demo instance: updates are off.
    Disabled,
    Checking,
    UpToDate,
    /// `UpdateUi::available` is set; [`Updater::install`] installs it.
    Available,
    Installing,
    /// Development build: the update was verified but not installed (`ErrorCode::UpdateDevBuild`).
    VerifiedOnly,
    /// See `UpdateUi::error`.
    Failed,
}

type Job = Box<dyn FnOnce(&mut UpdateUi) + Send>;

/// Drives checks and installs for the UI. Cheap to [`tick`](Updater::tick) every frame.
pub struct Updater {
    ui: UpdateUi,
    started: bool,
    jobs_tx: Sender<Job>,
    jobs_rx: Receiver<Job>,
    busy_jobs: usize,
    helper: Option<(crate::util::win::UnelevatedChild, Instant)>,
    next_auto: Option<Instant>,
    check_after_jobs: bool,
}

impl Default for Updater {
    fn default() -> Self {
        let (jobs_tx, jobs_rx) = channel();
        Updater {
            ui: UpdateUi { current: Version::current().to_string(), ..Default::default() },
            started: false,
            jobs_tx,
            jobs_rx,
            busy_jobs: 0,
            helper: None,
            next_auto: None,
            check_after_jobs: false,
        }
    }
}

/// The desktop user's `%LOCALAPPDATA%` (through the shell's token when elevated).
fn user_local_app_data() -> anyhow::Result<PathBuf> {
    crate::util::win::desktop_user_local_app_data()
}

fn fail(ui: &mut UpdateUi, e: &anyhow::Error) {
    ui.status = Status::Failed;
    ui.error = Some((code_of(e), token::redact(&format!("{e:#}"))));
}

impl Updater {
    pub fn state(&self) -> &UpdateUi {
        &self.ui
    }

    /// True while a check, verification or install runs (repaint periodically).
    pub fn busy(&self) -> bool {
        self.helper.is_some() || self.busy_jobs > 0
    }

    fn spawn_job(&mut self, f: impl FnOnce() -> Job + Send + 'static) {
        let tx = self.jobs_tx.clone();
        self.busy_jobs += 1;
        let spawned = std::thread::Builder::new().name("fanrgb-update".into()).spawn(move || {
            let job = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| {
                Box::new(|ui: &mut UpdateUi| {
                    ui.status = Status::Failed;
                    ui.error = Some((ErrorCode::Internal, "update worker panicked".into()));
                })
            });
            let _ = tx.send(job);
        });
        if spawned.is_err() {
            self.busy_jobs -= 1;
        }
    }

    /// Call regularly (every frame is fine). Starts the first and the daily automatic checks when
    /// `auto_check` is on (with or without a saved token), collects finished work. Returns [`Self::busy`].
    pub fn tick(&mut self, auto_check: bool) -> bool {
        if !self.started {
            self.started = true;
            if disabled() {
                self.ui.status = Status::Disabled;
                return false;
            }
            self.spawn_job(load_initial_state);
            self.next_auto = Some(Instant::now() + FIRST_CHECK_AFTER);
        }
        while let Ok(job) = self.jobs_rx.try_recv() {
            self.busy_jobs = self.busy_jobs.saturating_sub(1);
            job(&mut self.ui);
            if self.ui.status == Status::Failed && self.ui.error.as_ref().is_some_and(|(c, _)| fetch::wait_a_day(*c)) {
                // A rejected token, a repository that is not public, or GitHub's rate limit: never retry
                // automatically more than once a day.
                self.next_auto = Some(Instant::now() + CHECK_EVERY);
            }
        }
        if self.check_after_jobs && self.busy_jobs == 0 {
            self.check_after_jobs = false;
            // (A failed token save leaves `token_saved` false: no check, its error stays shown.)
            if self.ui.token_saved {
                self.check_now();
            }
        }
        self.poll_helper();
        if auto_check
            && !self.busy()
            && !matches!(self.ui.status, Status::Disabled | Status::Installing)
            && self.next_auto.is_some_and(|t| Instant::now() >= t)
        {
            self.next_auto = Some(Instant::now() + CHECK_EVERY);
            self.check_now();
        }
        self.busy()
    }

    /// "Check now": starts the unelevated helper (no-op while busy). It uses the saved token if there is
    /// one, and asks GitHub without one otherwise.
    pub fn check_now(&mut self) {
        if disabled() || self.busy() {
            return;
        }
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                self.ui.status = Status::Failed;
                self.ui.error = Some((ErrorCode::UpdateHelper, e.to_string()));
                return;
            }
        };
        match crate::util::win::spawn_unelevated_child(&exe, &[FETCH_ARG], false) {
            Ok(child) => {
                log::info!("update check started (helper pid {})", child.id());
                self.helper = Some((child, Instant::now()));
                self.ui.status = Status::Checking;
                self.ui.error = None;
            }
            Err(e) => {
                log::warn!("[{}] update helper: {e:#}", ErrorCode::UpdateHelper);
                self.ui.status = Status::Failed;
                self.ui.error = Some((ErrorCode::UpdateHelper, format!("{e:#}")));
            }
        }
    }

    fn poll_helper(&mut self) {
        let Some((child, started)) = self.helper.as_mut() else { return };
        let Some(problem) = helper_problem(child.try_wait(), started.elapsed()) else { return };
        if let Some((child, _)) = self.helper.take()
            && problem.is_some()
        {
            // Without waiting: this runs on the UI thread (`kill` would wait up to 10 s).
            // The helper's status file is not read after this.
            child.terminate();
        }
        match problem {
            None => {
                // The helper also staged the vendor driver list: the driver service re-verifies it.
                if let Some(d) = crate::drivers::service() {
                    d.catalog_staged();
                }
                self.spawn_job(evaluate_check)
            }
            Some(msg) => {
                log::warn!("[{}] {msg}", ErrorCode::UpdateHelper);
                self.ui.status = Status::Failed;
                self.ui.error = Some((ErrorCode::UpdateHelper, msg));
            }
        }
    }

    /// Validates and saves a pasted token for the desktop user (DPAPI), then checks. `pasted` is wiped
    /// in any case. Returns a message for the user on bad input.
    pub fn save_token(&mut self, pasted: &mut String) -> Result<(), String> {
        let t = token::validate(pasted);
        let mut bytes = std::mem::take(pasted).into_bytes();
        bytes.iter_mut().for_each(|b| *b = 0);
        let t = t.map_err(str::to_string)?;
        if disabled() {
            return Err("Updates are off in demo mode.".into());
        }
        // Shell-token lookup, impersonation, DPAPI and file I/O run on a worker, never on the UI thread;
        // the check starts when the save finished.
        self.check_after_jobs = true;
        self.spawn_job(move || {
            let res = user_local_app_data()
                .and_then(|lad| crate::util::win::as_desktop_user(move || token::store_in(&lad, &t)));
            Box::new(move |ui: &mut UpdateUi| match res {
                Ok(()) => {
                    log::info!("update token saved (DPAPI, desktop user)");
                    ui.token_saved = true;
                    ui.error = None;
                    ui.status = Status::Idle;
                }
                Err(e) => {
                    ui.status = Status::Failed;
                    ui.error = Some((
                        ErrorCode::UpdateTokenMissing,
                        format!("Could not save the token: {}", token::redact(&format!("{e:#}"))),
                    ));
                }
            })
        });
        Ok(())
    }

    /// Deletes the saved token ("Forget token"). Later checks ask GitHub without one; a verified offer
    /// stays (it was checked against the release keys, not the token).
    pub fn forget_token(&mut self) -> Result<(), String> {
        // Off the UI thread like `save_token`.
        self.spawn_job(|| {
            let res =
                user_local_app_data().and_then(|lad| crate::util::win::as_desktop_user(move || token::delete_in(&lad)));
            Box::new(move |ui: &mut UpdateUi| match res {
                Ok(()) => {
                    log::info!("update token removed");
                    ui.token_saved = false;
                    // A problem with the token that is gone no longer applies.
                    if ui.status == Status::Failed
                        && ui
                            .error
                            .as_ref()
                            .is_some_and(|(c, _)| matches!(c, ErrorCode::UpdateAuth | ErrorCode::UpdateTokenMissing))
                    {
                        ui.status = Status::Idle;
                        ui.error = None;
                    }
                }
                Err(e) => {
                    ui.status = Status::Failed;
                    ui.error = Some((ErrorCode::UpdateTokenMissing, format!("Could not remove the token: {e:#}")));
                }
            })
        });
        Ok(())
    }

    /// Installs `UpdateUi::available`: re-verifies the staged files and, on the installed copy, swaps
    /// the executable and restarts FanRGB into the new version (this process exits; fans return to the
    /// BIOS during the switch). On a development build it only verifies (`Status::VerifiedOnly`).
    pub fn install(&mut self, shared: &Arc<Shared>) {
        if disabled() || self.busy() {
            return;
        }
        // Exactly the release that is shown.
        let Some(expected) = self.ui.available.as_ref().map(|o| (o.version.clone(), o.sha256.clone())) else {
            return;
        };
        self.ui.status = Status::Installing;
        self.ui.error = None;
        let shared = shared.clone();
        self.spawn_job(move || {
            let res = user_local_app_data().and_then(|lad| {
                let staged = fetch::update_root_in(&lad).join(fetch::STAGED_DIR);
                install::apply(&shared, &staged, Some((&expected.0, &expected.1)))
            });
            Box::new(move |ui: &mut UpdateUi| match res {
                Ok(install::Applied::Restarting { version }) => {
                    ui.status = Status::Installing;
                    ui.last_result = Some(format!("installing {version}, restarting"));
                }
                Ok(install::Applied::VerifiedOnly { version }) => {
                    ui.status = Status::VerifiedOnly;
                    ui.error = Some((ErrorCode::UpdateDevBuild, format!("{version} verified")));
                }
                Err(e) => {
                    log::error!("{e:#}");
                    fail(ui, &e);
                }
            })
        });
    }
}

/// What a finished, failed or hung helper means: `None` = still running, `Some(None)` = finished (read
/// its status), `Some(Some(message))` = failed (a `try_wait` error is reported as
/// itself, not as a timeout, and the helper is stopped in both cases).
fn helper_problem(wait: std::io::Result<Option<u32>>, running_for: Duration) -> Option<Option<String>> {
    match wait {
        Ok(Some(_)) => Some(None),
        Ok(None) if running_for > HELPER_TIMEOUT => Some(Some(format!(
            "the update helper did not finish within {} s and was stopped",
            HELPER_TIMEOUT.as_secs()
        ))),
        Ok(None) => None,
        Err(e) => Some(Some(format!("cannot watch the update helper ({e}); it was stopped"))),
    }
}

/// Token presence, last check result and last install outcome, read off the UI thread.
fn load_initial_state() -> Job {
    let lad = user_local_app_data().ok();
    let token_saved = lad.as_deref().is_some_and(token::exists_in);
    let status = lad.as_deref().and_then(|l| fetch::read_status(&fetch::update_root_in(l)));
    let path = install::state_file();
    let mut rec = install::load_record(&path);
    let rolled_back = rec.rolled_back.take();
    if rolled_back.is_some() {
        // Shown once; `last_result` keeps the explanation.
        let _ = install::save_record(&path, &rec);
    }
    Box::new(move |ui: &mut UpdateUi| {
        ui.token_saved = token_saved;
        ui.status = Status::Idle;
        if let Some(st) = status {
            ui.last_checked = Some(st.checked_at);
            ui.repo = st.repo;
            if let Some(e) = st.error {
                // A token rejected, a repository not public or the rate limit hit less than a day ago: no
                // automatic check at this start either.
                if fetch::wait_a_day(e.code()) && now().saturating_sub(st.checked_at) < CHECK_EVERY.as_secs() {
                    ui.status = Status::Failed;
                }
                ui.error = Some((e.code(), e.detail));
            }
        }
        ui.last_result = rec.last_result.clone();
        if let Some(v) = rolled_back {
            ui.status = Status::Failed;
            ui.error = Some((ErrorCode::UpdateRolledBack, format!("{v} was rolled back")));
        }
    })
}

/// After the helper exited: read its status and re-verify a staged offer with the elevated side's
/// replay state (and record it as seen).
fn evaluate_check() -> Job {
    let lad = user_local_app_data();
    let result = lad.map(|lad| {
        let root = fetch::update_root_in(&lad);
        let st = fetch::read_status(&root);
        let verified =
            st.as_ref().and_then(|s| s.available.as_ref()).map(|_| verify_and_record(&root.join(fetch::STAGED_DIR)));
        (st, verified)
    });
    Box::new(move |ui: &mut UpdateUi| match result {
        Err(e) => fail(ui, &e),
        Ok((None, _)) => {
            ui.status = Status::Failed;
            ui.error = Some((ErrorCode::UpdateHelper, "the update helper wrote no result".into()));
        }
        Ok((Some(st), verified)) => {
            ui.last_checked = Some(st.checked_at);
            ui.repo = st.repo.clone().or(ui.repo.take());
            ui.available = None;
            ui.notes.clear();
            if let Some(e) = st.error {
                log::warn!("[{}] update check: {}", e.code(), e.detail);
                ui.status = Status::Failed;
                ui.error = Some((e.code(), e.detail));
                return;
            }
            ui.error = None;
            match verified {
                None => {
                    log::info!("update check: {} is up to date", ui.current);
                    ui.status = Status::UpToDate;
                }
                Some(Ok(offer)) => {
                    log::info!("update check: {} available (signature verified)", offer.version);
                    ui.notes = offer.notes.clone();
                    ui.available = Some(offer);
                    ui.status = Status::Available;
                }
                Some(Err(e)) => {
                    log::warn!("update check: staged update refused: {e:#}");
                    fail(ui, &e);
                }
            }
        }
    })
}

fn verify_and_record(staged: &std::path::Path) -> anyhow::Result<fetch::Offer> {
    let path = install::state_file();
    let mut rec = install::load_record(&path);
    let v = install::verify_staged(staged, &keys::TRUSTED, Version::current(), rec.seen(), now())?;
    let m = &v.release.manifest;
    if m.published_at > rec.last_published {
        rec.last_published = m.published_at;
        if let Err(e) = install::save_record(&path, &rec) {
            log::warn!("{e:#}");
        }
    }
    Ok(fetch::Offer {
        version: m.version.clone(),
        published_at: m.published_at,
        notes: m.notes.clone(),
        size: m.asset.size,
        sha256: m.asset.sha256.clone(),
    })
}

#[cfg(test)]
mod tests;
