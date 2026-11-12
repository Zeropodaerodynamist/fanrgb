//! End-to-end tests of the update client against a local GitHub fixture (mock [`Http`]): helper
//! download → elevated re-verification → swap into a fake install folder → rollback. Release manifests
//! are signed with throwaway minisign keys; the real keys are never used in tests.

use std::cell::RefCell;
use std::path::PathBuf;

use super::fetch::{self, CheckStatus, Pins};
use super::http::{Http, Response};
use super::install;
use super::manifest::tests::{fake_exe, manifest_json};
use super::manifest::{Seen, Version, sha256_hex};
use super::token;
use crate::error::{ErrorCode, code_of};
use crate::util::fsx;

const TOKEN: &str = "github_pat_11TESTTOKEN0000000000_abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQ";
const REPO: u64 = 1_455_226_413;
const CURRENT: Version = Version(0, 1, 0);
const NOW: u64 = 1_800_000_000;

/// A minisign key pair: (public key base64, secret key).
struct Key {
    public: String,
    secret: minisign::SecretKey,
    pk: minisign::PublicKey,
}

fn key() -> Key {
    let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    Key { public: kp.pk.to_base64(), secret: kp.sk, pk: kp.pk }
}

fn sign(k: &Key, data: &[u8]) -> Vec<u8> {
    minisign::sign(Some(&k.pk), &k.secret, data, Some("fanrgb test"), None).unwrap().into_string().into_bytes()
}

/// The fake GitHub: answers the four kinds of request the helper makes and records them.
struct GitHub {
    reported_id: u64,
    /// (asset id, name, bytes served)
    assets: Vec<(u64, String, Vec<u8>)>,
    /// `releases/latest` answers 404 when false.
    has_release: bool,
    /// The answer to API requests without a token: 200 serves them like a public repository, 404 is a
    /// private one, 403 / 429 the anonymous rate limit.
    anonymous: u16,
    requests: RefCell<Vec<(String, Option<String>)>>,
}

impl GitHub {
    fn new(manifest: Vec<u8>, sig: Vec<u8>, exe: Vec<u8>) -> GitHub {
        GitHub {
            reported_id: REPO,
            assets: vec![
                (11, "release.json".into(), manifest),
                (12, "release.json.minisig".into(), sig),
                (13, "fanrgb.exe".into(), exe),
            ],
            has_release: true,
            anonymous: 404,
            requests: RefCell::new(vec![]),
        }
    }

    /// The same release in a public repository.
    fn public(manifest: Vec<u8>, sig: Vec<u8>, exe: Vec<u8>) -> GitHub {
        GitHub { anonymous: 200, ..GitHub::new(manifest, sig, exe) }
    }

    fn downloads_of(&self, id: u64) -> usize {
        let needle = format!("/releases/assets/{id}");
        self.requests.borrow().iter().filter(|(u, _)| u.ends_with(&needle)).count()
    }
}

fn json(v: serde_json::Value) -> Response {
    Response { status: 200, location: None, body: serde_json::to_vec(&v).unwrap() }
}

impl Http for GitHub {
    fn get(&self, url: &str, headers: &[(&str, &str)], max: u64) -> anyhow::Result<Response> {
        let auth = headers.iter().find(|(k, _)| *k == "Authorization").map(|(_, v)| v.to_string());
        self.requests.borrow_mut().push((url.to_string(), auth.clone()));
        let authed = auth.as_deref() == Some(&format!("Bearer {TOKEN}"));
        let api = format!("https://api.github.com/repositories/{REPO}");
        let r = if let Some(rest) = url.strip_prefix("https://release-assets.githubusercontent.com/") {
            // Pre-signed download: the token must never arrive here.
            assert!(auth.is_none(), "token sent to the asset host");
            let id: u64 = rest.split('?').next().unwrap().parse().unwrap();
            let bytes = self.assets.iter().find(|a| a.0 == id).map(|a| a.2.clone()).unwrap_or_default();
            Response { status: 200, location: None, body: bytes }
        } else if auth.is_none() && self.anonymous != 200 {
            Response { status: self.anonymous, location: None, body: br#"{"message":"Not Found"}"#.to_vec() }
        } else if auth.is_some() && !authed {
            Response { status: 401, location: None, body: br#"{"message":"Bad credentials"}"#.to_vec() }
        } else if url == api {
            json(serde_json::json!({
                "id": self.reported_id,
                "full_name": "Zeropodaerodynamist/fanrgb",
                "private": self.anonymous != 200
            }))
        } else if url == format!("{api}/releases/latest") {
            if !self.has_release {
                return Ok(Response { status: 404, ..Default::default() });
            }
            let assets: Vec<_> = self
                .assets
                .iter()
                .map(|(id, name, b)| serde_json::json!({ "id": id, "name": name, "size": b.len() }))
                .collect();
            json(serde_json::json!({ "tag_name": "v0.2.0", "assets": assets }))
        } else if let Some(id) = url.strip_prefix(&format!("{api}/releases/assets/")) {
            assert!(headers.contains(&("Accept", "application/octet-stream")));
            Response {
                status: 302,
                location: Some(format!("https://release-assets.githubusercontent.com/{id}?X-Amz-Signature=x")),
                body: vec![],
            }
        } else {
            Response { status: 404, ..Default::default() }
        };
        if r.body.len() as u64 > max {
            anyhow::bail!("body over limit");
        }
        Ok(r)
    }
}

struct Env {
    root: PathBuf,
    install: PathBuf,
    primary: Key,
    backup: Key,
}

impl Env {
    fn new(name: &str) -> Env {
        let base = fsx::tests::test_dir(name);
        let install = base.join("Program Files").join("FanRGB");
        std::fs::create_dir_all(&install).unwrap();
        std::fs::write(install.join("fanrgb.exe"), fake_exe(0x10, 3000)).unwrap();
        Env { root: base.join("update"), install, primary: key(), backup: key() }
    }

    fn keys(&self) -> [&str; 2] {
        [&self.primary.public, &self.backup.public]
    }

    /// A check with a saved token (`tok` that does not validate stands for one that cannot be read).
    fn fetch(&self, gh: &GitHub, tok: &str) -> CheckStatus {
        let t = token::validate(tok).map(Some).map_err(|e| crate::error::err(ErrorCode::UpdateTokenMissing, e));
        self.fetch_with(gh, t)
    }

    /// A check without a token (none saved).
    fn fetch_anonymous(&self, gh: &GitHub) -> CheckStatus {
        self.fetch_with(gh, Ok(None))
    }

    fn fetch_with(&self, gh: &GitHub, t: anyhow::Result<Option<token::Token>>) -> CheckStatus {
        let keys = self.keys();
        let pins = Pins { repo_id: REPO, keys: &keys, current: CURRENT, now: NOW };
        let st = fetch::fetch(gh, t, &self.root, &pins);
        fetch::write_status(&self.root, &st).unwrap();
        st
    }

    fn staged(&self) -> PathBuf {
        self.root.join(fetch::STAGED_DIR)
    }

    fn verify(&self, seen: Seen) -> anyhow::Result<install::Verified> {
        install::verify_staged(&self.staged(), &self.keys(), CURRENT, seen, NOW)
    }
}

/// A release 0.2.0 signed by `k`: (manifest, signature, exe).
fn release(k: &Key, version: &str, published: u64) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let exe = fake_exe(0x20, 5000);
    let m = manifest_json(version, published, &exe);
    let s = sign(k, &m);
    (m, s, exe)
}

fn err_code(st: &CheckStatus) -> Option<ErrorCode> {
    st.error.as_ref().map(|e| e.code())
}

#[test]
fn full_update_flow_then_rollback() {
    let env = Env::new("upd-e2e");
    let (m, s, exe) = release(&env.primary, "0.2.0", NOW - 60);
    let gh = GitHub::new(m.clone(), s, exe.clone());
    let st = env.fetch(&gh, TOKEN);
    assert_eq!(st.error, None);
    assert_eq!(st.repo.as_deref(), Some("Zeropodaerodynamist/fanrgb"));
    let offer = st.available.clone().unwrap();
    assert_eq!((offer.version.as_str(), offer.size), ("0.2.0", exe.len() as u64));
    assert_eq!(offer.sha256, sha256_hex(&exe), "the offer names the exact executable");
    // The status file round-trips for the UI.
    assert_eq!(fetch::read_status(&env.root).unwrap(), st);
    // Only the API host saw the token; the asset host never did (asserted inside the fixture).
    assert!(gh.requests.borrow().iter().all(|(u, a)| a.is_none() || u.starts_with("https://api.github.com/")));
    assert_eq!(gh.downloads_of(13), 1);

    // A second check re-uses the staged executable.
    let st2 = env.fetch(&gh, TOKEN);
    assert_eq!(st2.available, st.available);
    assert_eq!(gh.downloads_of(13), 1, "staged exe downloaded again");

    // Elevated side: re-verification passes, then the swap keeps the old version.
    let v = env.verify(Seen::default()).unwrap();
    assert_eq!(v.key, 0);
    let old = std::fs::read(env.install.join("fanrgb.exe")).unwrap();
    let old_sha = install::swap_in(&env.install, &v.exe, &v.release.manifest.asset.sha256).unwrap();
    assert_eq!(old_sha, sha256_hex(&old));
    assert_eq!(std::fs::read(env.install.join("fanrgb.exe")).unwrap(), exe);
    assert_eq!(std::fs::read(env.install.join("fanrgb.exe.old")).unwrap(), old);

    // The new version never reported healthy: roll back.
    install::swap_back(&env.install).unwrap();
    assert_eq!(std::fs::read(env.install.join("fanrgb.exe")).unwrap(), old);
    assert_eq!(std::fs::read(env.install.join("fanrgb.exe.failed")).unwrap(), exe);

    // After the update was recorded as installed (0.2.0) the same release is a downgrade.
    let seen = Seen { last_installed: Some(Version(0, 2, 0)), last_published: NOW - 60 };
    assert_eq!(code_of(&env.verify(seen).err().unwrap()), ErrorCode::UpdateDowngrade);
}

#[test]
fn backup_key_is_accepted() {
    let env = Env::new("upd-backup");
    let (m, s, exe) = release(&env.backup, "0.2.0", NOW);
    let st = env.fetch(&GitHub::new(m, s, exe), TOKEN);
    assert!(st.available.is_some(), "{st:?}");
    assert_eq!(env.verify(Seen::default()).unwrap().key, 1);
}

#[test]
fn tampered_or_foreign_signatures_are_refused() {
    // Manifest changed after signing (e.g. a different sha256 by someone holding only the token).
    let env = Env::new("upd-tamper");
    let (m, s, exe) = release(&env.primary, "0.2.0", NOW);
    let mut evil: serde_json::Value = serde_json::from_slice(&m).unwrap();
    evil["notes"] = serde_json::json!("Totally legit.");
    let gh = GitHub::new(serde_json::to_vec_pretty(&evil).unwrap(), s.clone(), exe.clone());
    let st = env.fetch(&gh, TOKEN);
    assert_eq!(err_code(&st), Some(ErrorCode::UpdateSignature));
    assert!(!env.staged().exists(), "nothing may be staged");
    assert_eq!(gh.downloads_of(13), 0, "exe downloaded despite a bad signature");

    // Corrupted signature bytes.
    let mut bad_sig = s.clone();
    let line2 = bad_sig.iter().position(|&b| b == b'\n').unwrap() + 5;
    bad_sig[line2] = if bad_sig[line2] == b'A' { b'B' } else { b'A' };
    assert_eq!(
        err_code(&env.fetch(&GitHub::new(m.clone(), bad_sig, exe.clone()), TOKEN)),
        Some(ErrorCode::UpdateSignature)
    );

    // Signed with a key this build does not trust.
    let stranger = key();
    let (m2, s2, exe2) = release(&stranger, "0.2.0", NOW);
    assert_eq!(err_code(&env.fetch(&GitHub::new(m2, s2, exe2), TOKEN)), Some(ErrorCode::UpdateSignature));

    // A garbage / missing signature asset.
    assert_eq!(
        err_code(&env.fetch(&GitHub::new(m.clone(), b"nope".to_vec(), exe.clone()), TOKEN)),
        Some(ErrorCode::UpdateSignature)
    );
    let mut gh = GitHub::new(m, s, exe);
    gh.assets.retain(|a| a.1 != "release.json.minisig");
    assert_eq!(err_code(&env.fetch(&gh, TOKEN)), Some(ErrorCode::UpdatePackage));
}

#[test]
fn staged_files_are_reverified_by_the_elevated_side() {
    // A process of the same user can rewrite the staging folder after the helper is done.
    let env = Env::new("upd-staged");
    let (m, s, exe) = release(&env.primary, "0.2.0", NOW);
    assert!(env.fetch(&GitHub::new(m.clone(), s.clone(), exe), TOKEN).available.is_some());
    // Swapped executable (same size): SHA-256 mismatch.
    std::fs::write(env.staged().join("fanrgb.exe"), fake_exe(0x66, 5000)).unwrap();
    assert_eq!(code_of(&env.verify(Seen::default()).err().unwrap()), ErrorCode::UpdateHash);
    // Edited manifest that names the swapped exe: signature mismatch.
    let swapped = fake_exe(0x66, 5000);
    std::fs::write(env.staged().join("release.json"), manifest_json("0.2.0", NOW, &swapped)).unwrap();
    assert_eq!(code_of(&env.verify(Seen::default()).err().unwrap()), ErrorCode::UpdateSignature);
    // Original manifest back, swapped exe still there: still refused.
    std::fs::write(env.staged().join("release.json"), &m).unwrap();
    assert_eq!(code_of(&env.verify(Seen::default()).err().unwrap()), ErrorCode::UpdateHash);
    let _ = s;
}

#[test]
fn wrong_sha_from_the_server_is_refused() {
    let env = Env::new("upd-sha");
    let (m, s, _) = release(&env.primary, "0.2.0", NOW);
    // Same size, other bytes (the release asset was replaced on GitHub).
    let gh = GitHub::new(m.clone(), s.clone(), fake_exe(0x99, 5000));
    let st = env.fetch(&gh, TOKEN);
    assert_eq!(err_code(&st), Some(ErrorCode::UpdateHash));
    assert!(!env.staged().join("fanrgb.exe").exists());
    // Other size: refused from the release listing, before downloading.
    let gh = GitHub::new(m, s, fake_exe(0x20, 6000));
    assert_eq!(err_code(&env.fetch(&gh, TOKEN)), Some(ErrorCode::UpdateHash));
    assert_eq!(gh.downloads_of(13), 0);
}

#[test]
fn downgrades_and_replays_are_refused() {
    let env = Env::new("upd-down");
    // Same version as running: up to date, nothing staged.
    let (m, s, exe) = release(&env.primary, "0.1.0", NOW);
    let st = env.fetch(&GitHub::new(m, s, exe), TOKEN);
    assert_eq!((st.error.clone(), st.available.clone()), (None, None));
    assert!(!env.staged().exists());
    // Older version, validly signed: also not offered.
    let (m, s, exe) = release(&env.primary, "0.0.9", NOW);
    assert_eq!(env.fetch(&GitHub::new(m, s, exe), TOKEN).available, None);

    // A newer release, then an older signed manifest replayed with a higher version number but an
    // earlier date (published before one the app has already seen).
    let (m, s, exe) = release(&env.primary, "0.3.0", NOW - 86_400);
    assert!(env.fetch(&GitHub::new(m, s, exe), TOKEN).available.is_some());
    let seen = Seen { last_installed: None, last_published: NOW };
    assert_eq!(code_of(&env.verify(seen).err().unwrap()), ErrorCode::UpdateDowngrade);
    // Installed 0.3.0 already (record), the same 0.3.0 is offered again.
    let seen = Seen { last_installed: Some(Version(0, 3, 0)), last_published: 0 };
    assert_eq!(code_of(&env.verify(seen).err().unwrap()), ErrorCode::UpdateDowngrade);
}

#[test]
fn wrong_repository_id_stops_the_check() {
    let env = Env::new("upd-repo");
    let (m, s, exe) = release(&env.primary, "0.2.0", NOW);
    let mut gh = GitHub::new(m, s, exe);
    gh.reported_id = REPO + 1;
    let st = env.fetch(&gh, TOKEN);
    assert_eq!(err_code(&st), Some(ErrorCode::UpdateRepository));
    assert_eq!(gh.requests.borrow().len(), 1, "continued after the repository id mismatch");
}

#[test]
fn bad_token_and_missing_release() {
    let env = Env::new("upd-auth");
    let (m, s, exe) = release(&env.primary, "0.2.0", NOW);
    let other = "github_pat_11WRONGTOKEN000000000_zyxwvutsrqponmlkjihgfedcba9876543210ZYXWVUTSRQPONMLKJIH";
    let st = env.fetch(&GitHub::new(m.clone(), s.clone(), exe.clone()), other);
    assert_eq!(err_code(&st), Some(ErrorCode::UpdateAuth));
    // A saved token that cannot be read: reported, and GitHub is not asked without it instead.
    let gh = GitHub::public(m.clone(), s.clone(), exe.clone());
    let st = env.fetch_with(&gh, Err(crate::error::err(ErrorCode::UpdateTokenMissing, "cannot be decrypted")));
    assert_eq!(err_code(&st), Some(ErrorCode::UpdateTokenMissing));
    assert!(gh.requests.borrow().is_empty());
    // No release published yet: up to date, with or without a token.
    let mut gh = GitHub::public(m, s, exe);
    gh.has_release = false;
    let st = env.fetch(&gh, TOKEN);
    assert_eq!((st.error, st.available), (None, None));
    let st = env.fetch_anonymous(&gh);
    assert_eq!((st.error, st.repo.as_deref(), st.available), (None, Some("Zeropodaerodynamist/fanrgb"), None));
}

/// A public repository: the whole check runs without a token, and no request carries an
/// `Authorization` header.
#[test]
fn public_repository_without_a_token() {
    let env = Env::new("upd-anon");
    let (m, s, exe) = release(&env.primary, "0.2.0", NOW - 60);
    let gh = GitHub::public(m, s, exe.clone());
    let st = env.fetch_anonymous(&gh);
    assert_eq!(st.error, None);
    assert_eq!(st.repo.as_deref(), Some("Zeropodaerodynamist/fanrgb"));
    let offer = st.available.clone().unwrap();
    assert_eq!((offer.version.as_str(), offer.sha256.clone()), ("0.2.0", sha256_hex(&exe)));
    assert!(gh.requests.borrow().iter().all(|(_, a)| a.is_none()), "a request carried a token");
    // The pinned repository id is resolved first, as with a token.
    assert_eq!(gh.requests.borrow()[0].0, format!("https://api.github.com/repositories/{REPO}"));
    assert_eq!(gh.downloads_of(13), 1);
    // A second check re-uses the staged executable.
    assert_eq!(env.fetch_anonymous(&gh).available, st.available);
    assert_eq!(gh.downloads_of(13), 1, "staged exe downloaded again");
    // The elevated side verifies the staged files exactly as it does after a check with a token.
    let v = env.verify(Seen::default()).unwrap();
    assert_eq!((v.key, v.exe.clone()), (0, exe));
}

/// Without a token: 401 / 404 mean the repository is not public (or not reachable), 403 / 429 the rate
/// limit. Each stops at the first request, before anything is downloaded or staged.
#[test]
fn errors_without_a_token_are_coded() {
    let env = Env::new("upd-anon-err");
    let (m, s, exe) = release(&env.primary, "0.2.0", NOW);
    for (status, code) in [
        (404, ErrorCode::UpdateNotPublic),
        (401, ErrorCode::UpdateNotPublic),
        (403, ErrorCode::UpdateRateLimit),
        (429, ErrorCode::UpdateRateLimit),
        (500, ErrorCode::UpdateNetwork),
    ] {
        let gh = GitHub { anonymous: status, ..GitHub::new(m.clone(), s.clone(), exe.clone()) };
        let st = env.fetch_anonymous(&gh);
        assert_eq!(err_code(&st), Some(code), "HTTP {status}");
        assert_eq!(gh.requests.borrow().len(), 1, "HTTP {status}: went on after the repository check");
        assert!(!env.staged().exists());
        let e = st.error.unwrap();
        assert!(e.detail.contains(&format!("HTTP {status}")), "{}", e.detail);
        // The status file keeps the code for the app (FRG-E714 / FRG-E715 are the helper's to report).
        assert_eq!(fetch::read_status(&env.root).unwrap().error.unwrap().code(), code);
    }
    // The same answers with a token keep their meaning from before (the token is the problem).
    for (status, code) in [(401, ErrorCode::UpdateAuth), (403, ErrorCode::UpdateAuth), (404, ErrorCode::UpdateAuth)] {
        assert_eq!(code_of(&fetch::status_error(status, "repository", true)), code, "HTTP {status}");
    }
    assert_eq!(code_of(&fetch::status_error(429, "repository", true)), ErrorCode::UpdateNetwork);
    assert_eq!(code_of(&fetch::status_error(502, "repository", false)), ErrorCode::UpdateNetwork);
}

/// The trust checks do not depend on the token: without one, a foreign repository id, a bad or foreign
/// signature, a changed executable and a downgrade are refused exactly as with one.
#[test]
fn security_holds_without_a_token() {
    let env = Env::new("upd-anon-sec");
    let (m, s, exe) = release(&env.primary, "0.2.0", NOW);
    // Another repository behind the pinned id.
    let gh = GitHub { reported_id: REPO + 1, ..GitHub::public(m.clone(), s.clone(), exe.clone()) };
    assert_eq!(err_code(&env.fetch_anonymous(&gh)), Some(ErrorCode::UpdateRepository));
    assert_eq!(gh.requests.borrow().len(), 1);
    // Manifest changed after signing.
    let mut evil: serde_json::Value = serde_json::from_slice(&m).unwrap();
    evil["notes"] = serde_json::json!("Totally legit.");
    let gh = GitHub::public(serde_json::to_vec_pretty(&evil).unwrap(), s.clone(), exe.clone());
    assert_eq!(err_code(&env.fetch_anonymous(&gh)), Some(ErrorCode::UpdateSignature));
    assert_eq!(gh.downloads_of(13), 0, "exe downloaded despite a bad signature");
    assert!(!env.staged().exists());
    // Signed with a key this build does not trust.
    let (m2, s2, exe2) = release(&key(), "0.2.0", NOW);
    assert_eq!(err_code(&env.fetch_anonymous(&GitHub::public(m2, s2, exe2))), Some(ErrorCode::UpdateSignature));
    // The asset was replaced on GitHub (same size, other bytes).
    let gh = GitHub::public(m.clone(), s.clone(), fake_exe(0x99, 5000));
    assert_eq!(err_code(&env.fetch_anonymous(&gh)), Some(ErrorCode::UpdateHash));
    assert!(!env.staged().join("fanrgb.exe").exists());
    // Not newer than the running version: not offered, nothing staged.
    let (m3, s3, exe3) = release(&env.primary, "0.1.0", NOW);
    let st = env.fetch_anonymous(&GitHub::public(m3, s3, exe3));
    assert_eq!((st.error, st.available), (None, None));
    assert!(!env.staged().exists());
    // A good one, then the staged exe is swapped by another process of the user: the elevated side refuses.
    assert!(env.fetch_anonymous(&GitHub::public(m, s, exe)).available.is_some());
    std::fs::write(env.staged().join("fanrgb.exe"), fake_exe(0x66, 5000)).unwrap();
    assert_eq!(code_of(&env.verify(Seen::default()).err().unwrap()), ErrorCode::UpdateHash);
}

/// Which failures wait a day before the next automatic check, and which skip the driver catalog in the
/// same helper run (an unreadable token also does, without delaying the next check: no request was made).
#[test]
fn failures_that_wait_a_day() {
    for c in [ErrorCode::UpdateAuth, ErrorCode::UpdateNotPublic, ErrorCode::UpdateRateLimit] {
        assert!(fetch::wait_a_day(c) && fetch::skip_catalog(c), "{c}");
    }
    assert!(!fetch::wait_a_day(ErrorCode::UpdateTokenMissing) && fetch::skip_catalog(ErrorCode::UpdateTokenMissing));
    for c in
        [ErrorCode::UpdateNetwork, ErrorCode::UpdateSignature, ErrorCode::UpdateRepository, ErrorCode::UpdateHelper]
    {
        assert!(!fetch::wait_a_day(c) && !fetch::skip_catalog(c), "{c}");
    }
}

#[test]
fn the_token_never_leaves_the_helper() {
    // Status file, error details and diagnostics/log text never contain the token, even when an error
    // message quotes it.
    let env = Env::new("upd-redact");
    let leaky = crate::error::err(ErrorCode::UpdateNetwork, format!("GET failed with Authorization: Bearer {TOKEN}"));
    let keys = env.keys();
    let pins = Pins { repo_id: REPO, keys: &keys, current: CURRENT, now: NOW };
    let (m, s, exe) = release(&env.primary, "0.2.0", NOW);
    let st = fetch::fetch(&GitHub::new(m, s, exe), Err(leaky), &env.root, &pins);
    fetch::write_status(&env.root, &st).unwrap();
    let file = std::fs::read_to_string(env.root.join(fetch::STATUS_FILE)).unwrap();
    assert!(!file.contains("TESTTOKEN"), "{file}");
    assert!(file.contains("Bearer ***"));
    // A status file written by someone else with a token in it is redacted when read.
    let mut forged = st.clone();
    forged.error.as_mut().unwrap().detail = format!("see {TOKEN}");
    std::fs::write(env.root.join(fetch::STATUS_FILE), serde_json::to_vec(&forged).unwrap()).unwrap();
    assert!(!fetch::read_status(&env.root).unwrap().error.unwrap().detail.contains("TESTTOKEN"));

    // The saved token is in the user's LocalAppData (DPAPI), and diagnostics sections and log lines
    // that mention it are redacted.
    let lad = env.root.parent().unwrap().join("LocalAppData");
    token::store_in(&lad, &token::validate(TOKEN).unwrap()).unwrap();
    let cfg = crate::config::Config::default();
    let mut report = crate::diag::config_section(&cfg);
    report.push_str(&crate::diag::log_section(50));
    assert!(!report.contains("TESTTOKEN"));
    let line = crate::util::logger::escape_controls(&token::redact(&format!("token {TOKEN} pasted"))).into_owned();
    assert!(!line.contains("TESTTOKEN"), "{line}");
    let cfg_json = serde_json::to_string(&cfg).unwrap();
    assert!(!cfg_json.contains("github_pat") && cfg_json.contains("\"updates\""));
}

#[test]
fn rollback_after_install_restores_the_previous_build() {
    let env = Env::new("upd-rollback");
    let (m, s, exe) = release(&env.primary, "0.2.0", NOW);
    assert!(env.fetch(&GitHub::new(m, s, exe), TOKEN).available.is_some());
    let v = env.verify(Seen::default()).unwrap();
    let before = std::fs::read(env.install.join("fanrgb.exe")).unwrap();
    install::swap_in(&env.install, &v.exe, &v.release.manifest.asset.sha256).unwrap();
    // The supervisor decides "unhealthy" (see install::Probation tests) and swaps back.
    install::swap_back(&env.install).unwrap();
    assert_eq!(sha256_hex(&std::fs::read(env.install.join("fanrgb.exe")).unwrap()), sha256_hex(&before));
    // The version never proved healthy, so it was never recorded as installed: the same release can be
    // installed again. Replays of older manifests stay refused (`last_published`).
    let path = env.root.join(install::STATE_FILE);
    let rec = install::UpdateRecord { rolled_back: Some("0.2.0".into()), last_published: NOW, ..Default::default() };
    install::save_record(&path, &rec).unwrap();
    let rec = install::load_record(&path);
    env.verify(rec.seen()).unwrap();
}

fn pending(to: &str) -> install::Pending {
    install::Pending { to: to.into(), from: "0.1.0".into(), at: NOW, ..Default::default() }
}

/// Code review v0.2 M5: `last_installed` is written only once the new version proved healthy; a start
/// that failed (crash, spawn error, timeout) leaves the release installable.
#[test]
fn only_a_healthy_update_counts_as_installed() {
    use install::Verdict;
    let env = Env::new("upd-m5");
    let (m, s, exe) = release(&env.primary, "0.2.0", NOW);
    assert!(env.fetch(&GitHub::new(m, s, exe), TOKEN).available.is_some());
    let p = pending("0.2.0");
    // State right after `apply` (hand-over started): nothing recorded as installed yet.
    let mut rec = install::UpdateRecord { last_published: NOW, pending: Some(p.clone()), ..Default::default() };
    env.verify(rec.seen()).unwrap();
    // The new version crashed: rollback, and the release is still installable.
    let v = install::judge(&rec, &p, 42, Some(Ok(0xC000_0005)), false);
    assert!(matches!(v, Verdict::Failed(_)), "{v:?}");
    assert!(matches!(install::judge(&rec, &p, 42, None, true), Verdict::Failed(_)), "timeout");
    assert_eq!(install::judge(&rec, &p, 42, None, false), Verdict::Running);
    rec.pending = None;
    env.verify(rec.seen()).unwrap();
    // Healthy: from now on the same release is a downgrade.
    let mut q = p.clone();
    q.healthy = true;
    q.healthy_pid = 42;
    rec.pending = Some(q);
    assert_eq!(install::judge(&rec, &p, 42, None, false), Verdict::Healthy);
    assert_eq!(install::judge(&rec, &p, 43, None, false), Verdict::Running, "health of another pid");
    // Health recorded just before the child exited still counts.
    assert_eq!(install::judge(&rec, &p, 42, Some(Ok(1)), false), Verdict::Healthy);
    install::finish_kept(&mut rec, &p, true);
    assert_eq!((rec.pending.as_ref(), rec.last_installed.as_deref()), (None, Some("0.2.0")));
    assert_eq!(code_of(&env.verify(rec.seen()).err().unwrap()), ErrorCode::UpdateDowngrade);
    // `record_installed` never lowers the recorded version.
    rec.record_installed("0.1.5");
    rec.record_installed("junk");
    assert_eq!(rec.last_installed.as_deref(), Some("0.2.0"));
}

/// Code review v0.2 L15: a new version closed cleanly during probation (exit code 0 and its own marker)
/// is kept, not rolled back, and not recorded as installed.
#[test]
fn clean_exit_during_probation_is_not_a_failure() {
    use install::Verdict;
    let p = pending("0.2.0");
    let mut q = p.clone();
    q.clean_exit_pid = 42;
    let mut rec = install::UpdateRecord { pending: Some(q), ..Default::default() };
    assert_eq!(install::judge(&rec, &p, 42, Some(Ok(0)), false), Verdict::ClosedCleanly);
    assert_eq!(install::judge(&rec, &p, 42, None, false), Verdict::Running, "marker alone ends nothing");
    assert!(matches!(install::judge(&rec, &p, 42, Some(Ok(101)), false), Verdict::Failed(_)), "panic exit");
    assert!(matches!(install::judge(&rec, &p, 43, Some(Ok(0)), false), Verdict::Failed(_)), "other pid");
    assert!(matches!(install::judge(&rec, &p, 42, Some(Err("x".into())), false), Verdict::Failed(_)));
    // Exit code 0 without the marker (e.g. an early `return` in main) is still a failure.
    let bare = install::UpdateRecord { pending: Some(p.clone()), ..Default::default() };
    assert!(matches!(install::judge(&bare, &p, 42, Some(Ok(0)), false), Verdict::Failed(_)));
    install::finish_kept(&mut rec, &p, false);
    assert_eq!(rec.pending, None);
    assert_eq!(rec.last_installed, None);
    assert!(rec.last_result.as_deref().is_some_and(|r| r.contains("closed before")));
    // The marker survives the record file (older records without it still load).
    let dir = fsx::tests::test_dir("upd-l15");
    let path = dir.join(install::STATE_FILE);
    std::fs::write(&path, br#"{"pending":{"to":"0.2.0","from":"0.1.0","at":1,"new_sha256":"","old_sha256":""}}"#)
        .unwrap();
    assert_eq!(install::load_record(&path).pending.unwrap().clean_exit_pid, 0);
}

/// Code review v0.2 M6: no update from a fallback (per-process) state folder.
#[test]
fn update_needs_the_persistent_state_folder() {
    use crate::util::paths::StateLocation;
    install::check_state_location(StateLocation::Secure).unwrap();
    for loc in [StateLocation::Fallback, StateLocation::Unavailable, StateLocation::User, StateLocation::Override] {
        let name = format!("{loc:?}");
        let e = install::check_state_location(loc).unwrap_err();
        assert_eq!(code_of(&e), ErrorCode::UpdateStateFolder, "{name}");
    }
}

/// Code review v0.2 L19: install exactly the release that was shown.
#[test]
fn install_refuses_a_release_other_than_the_one_shown() {
    let env = Env::new("upd-l19");
    let (m, s, exe) = release(&env.primary, "0.2.0", NOW);
    assert!(env.fetch(&GitHub::new(m, s, exe.clone()), TOKEN).available.is_some());
    let r = env.verify(Seen::default()).unwrap().release;
    let sha = sha256_hex(&exe);
    install::check_expected(&r, None).unwrap();
    install::check_expected(&r, Some(("0.2.0", &sha))).unwrap();
    install::check_expected(&r, Some(("0.2.0", &sha.to_uppercase()))).unwrap();
    for (v, h) in [("0.3.0", sha.as_str()), ("0.2.0", "00")] {
        let e = install::check_expected(&r, Some((v, h))).unwrap_err();
        assert_eq!(code_of(&e), ErrorCode::UpdateChanged);
    }
}

/// Code review v0.2 L20: the user-writable status file is display data only.
#[test]
fn status_file_is_sanitized_for_the_elevated_ui() {
    let mut st = CheckStatus {
        repo: Some("Evil/repo\u{202e}gnp.exe".into()),
        error: Some(fetch::StatusError {
            code: 999,
            detail: format!("bad\u{202e}txt.exe\u{200b} line\r\nnext Bearer {TOKEN} {}", "x".repeat(900)),
        }),
        ..Default::default()
    };
    fetch::sanitize_status(&mut st);
    assert_eq!(st.repo, None);
    let e = st.error.unwrap();
    assert_eq!(e.code(), ErrorCode::UpdateHelper, "codes the helper never writes are not shown as such");
    assert!(e.detail.starts_with("badtxt.exe line  next "), "{}", e.detail);
    assert!(!e.detail.contains(TOKEN));
    assert!(e.detail.chars().count() <= 500 && !e.detail.chars().any(|c| c.is_control()));
    for (code, kept) in [
        (701, true),
        (711, true),
        (714, true),
        (715, true),
        (606, true),
        (712, false),
        (713, false),
        (716, false),
        (300, false),
    ] {
        let mut st = CheckStatus {
            repo: Some("Zeropodaerodynamist/fanrgb".into()),
            error: Some(fetch::StatusError { code, detail: String::new() }),
            ..Default::default()
        };
        fetch::sanitize_status(&mut st);
        assert_eq!(st.error.unwrap().code == code, kept, "{code}");
        assert_eq!(st.repo.as_deref(), Some("Zeropodaerodynamist/fanrgb"));
    }
}

/// Code review v0.2 L21: a helper that cannot be watched is reported as such (and stopped), not as a
/// timeout; a hung one after the timeout.
#[test]
fn helper_wait_errors_are_reported() {
    use std::time::Duration;
    assert_eq!(super::helper_problem(Ok(None), Duration::from_secs(5)), None);
    assert_eq!(super::helper_problem(Ok(Some(1)), Duration::from_secs(5)), Some(None));
    let hung = super::helper_problem(Ok(None), Duration::from_secs(181)).unwrap().unwrap();
    assert!(hung.contains("did not finish"), "{hung}");
    let err = super::helper_problem(Err(std::io::Error::from_raw_os_error(6)), Duration::ZERO).unwrap().unwrap();
    assert!(err.contains("cannot watch") && !err.contains("in time"), "{err}");
}

#[test]
fn junction_in_staging_is_refused() {
    let env = Env::new("upd-junction");
    let (m, s, exe) = release(&env.primary, "0.2.0", NOW);
    assert!(env.fetch(&GitHub::new(m, s, exe), TOKEN).available.is_some());
    // Replace the staged folder by a junction to another folder holding the same files.
    let elsewhere = env.root.join("elsewhere");
    std::fs::rename(env.staged(), &elsewhere).unwrap();
    let ok = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(env.staged())
        .arg(&elsewhere)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("mklink /J unavailable; skipping");
        return;
    }
    assert!(env.verify(Seen::default()).is_err(), "followed a junction in the staging folder");
}

/// Real network, through WinHTTP: a public repository's release listing and a redirected asset
/// download (no token), the private FanRGB repository without a token (404, so a check without a token
/// reports FRG-E714; both flip once the repository is public), and, when `FANRGB_TEST_TOKEN` is set, a
/// full check of the pinned repository. Writes only below target/.
#[test]
#[ignore = "network; run with --ignored"]
fn live_github_through_winhttp() {
    use super::http::{self, WinHttp};
    let h = WinHttp::new().unwrap();
    // jedisct1/minisign (public): latest release + smallest asset through the 302 to the asset host.
    let json = "application/vnd.github+json";
    let r =
        http::get_following(&h, "https://api.github.com/repositories/37023739/releases/latest", json, None, 4 << 20)
            .unwrap();
    assert_eq!(r.status, 200);
    let rel: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    let assets = rel["assets"].as_array().unwrap();
    let asset = assets.iter().min_by_key(|a| a["size"].as_u64().unwrap()).unwrap().clone();
    let url = format!("https://api.github.com/repositories/37023739/releases/assets/{}", asset["id"]);
    let r = http::get_following(&h, &url, "application/octet-stream", None, 16 << 20).unwrap();
    assert_eq!((r.status, r.body.len() as u64), (200, asset["size"].as_u64().unwrap()));
    // The private repository is invisible without a token.
    let r =
        http::get_following(&h, &format!("https://api.github.com/repositories/{REPO}"), json, None, 1 << 20).unwrap();
    assert_eq!(r.status, 404);
    let pins = Pins { repo_id: REPO, keys: &super::keys::TRUSTED, current: Version::current(), now: super::now() };
    let st = fetch::fetch(&h, Ok(None), &Env::new("upd-live-anon").root, &pins);
    assert_eq!(err_code(&st), Some(ErrorCode::UpdateNotPublic), "{st:?}");
    if let Ok(t) = std::env::var("FANRGB_TEST_TOKEN") {
        let env = Env::new("upd-live");
        let tok = token::validate(&t).map(Some).map_err(|e| crate::error::err(ErrorCode::UpdateTokenMissing, e));
        let st = fetch::fetch(&h, tok, &env.root, &pins);
        eprintln!(
            "live check: repo {:?}, available {:?}, error {:?}",
            st.repo,
            st.available.as_ref().map(|a| &a.version),
            st.error.as_ref().map(|e| e.code())
        );
        assert_eq!(st.error, None);
        assert_eq!(st.repo.as_deref(), Some(super::REPO));
    }
}

/// The output of `tools/release.ps1` (folder in `FANRGB_RELEASE_DIR`) verifies with the keys compiled
/// into this build: the release tooling and the app agree on the format.
#[test]
#[ignore = "needs FANRGB_RELEASE_DIR from a tools/release.ps1 run"]
fn release_tool_output_verifies_with_compiled_keys() {
    let Some(dir) = std::env::var_os("FANRGB_RELEASE_DIR").map(PathBuf::from) else { return };
    let m = std::fs::read(dir.join("release.json")).unwrap();
    let s = std::fs::read(dir.join("release.json.minisig")).unwrap();
    let exe = std::fs::read(dir.join("fanrgb.exe")).unwrap();
    let key = super::manifest::verify_signature(&m, &s, &super::keys::TRUSTED).unwrap();
    let r = super::manifest::parse(&m).unwrap();
    super::manifest::verify_package(&r, &exe).unwrap();
    eprintln!("release {} signed by key #{key}, {} bytes: verified", r.version, exe.len());
    if r.version <= Version::current() {
        let e = super::manifest::check_offer(&r, Version::current(), Seen::default(), super::now()).unwrap_err();
        assert_eq!(code_of(&e), ErrorCode::UpdateDowngrade);
        eprintln!("release {} refused as not newer than {}", r.version, Version::current());
    }
}

// ---------------------------------------------------------------------------------------
// Independent tests (2026-11-06): self-update without a token, written separately from the
// change itself.

/// One recorded request: URL and every header.
type Seen2 = (String, Vec<(String, String)>);

/// Wraps the fake GitHub and records every request with all of its headers.
struct Recorder {
    inner: GitHub,
    seen: RefCell<Vec<Seen2>>,
}

impl Recorder {
    fn new(inner: GitHub) -> Recorder {
        Recorder { inner, seen: RefCell::new(vec![]) }
    }
}

impl Http for Recorder {
    fn get(&self, url: &str, headers: &[(&str, &str)], max: u64) -> anyhow::Result<Response> {
        let hs = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self.seen.borrow_mut().push((url.to_string(), hs));
        self.inner.get(url, headers, max)
    }
}

fn host_of(url: &str) -> String {
    super::http::split_https(url).unwrap().0
}

/// Without a token no request carries a credential of any kind (no `Authorization` header in any
/// spelling, no bearer value, nothing token-shaped), on the API host or on the asset host the download
/// is redirected to; every API request addresses the repository by its pinned id, never by name. With a
/// token, exactly the API requests carry it and the redirected download does not.
#[test]
fn tester_anonymous_check_sends_no_credentials() {
    let env = Env::new("tst-anon-headers");
    let keys = env.keys();
    let pins = Pins { repo_id: REPO, keys: &keys, current: CURRENT, now: NOW };
    let (m, s, exe) = release(&env.primary, "0.2.0", NOW - 60);
    let rec = Recorder::new(GitHub::public(m.clone(), s.clone(), exe.clone()));
    let st = fetch::fetch(&rec, Ok(None), &env.root, &pins);
    assert_eq!(st.error, None, "{st:?}");
    assert!(st.available.is_some());
    let seen = rec.seen.borrow();
    assert!(seen.iter().any(|(u, _)| host_of(u) == "release-assets.githubusercontent.com"), "no redirected download");
    for (url, headers) in seen.iter() {
        let host = host_of(url);
        assert!(host == "api.github.com" || host == "release-assets.githubusercontent.com", "{url}");
        if host == "api.github.com" {
            assert!(url.starts_with(&format!("https://api.github.com/repositories/{REPO}")), "{url}");
        }
        for (k, v) in headers {
            assert!(!k.eq_ignore_ascii_case("authorization"), "{url}: {k}");
            let lower = v.to_ascii_lowercase();
            assert!(
                !lower.contains("bearer")
                    && !lower.contains("token")
                    && !v.contains("github_pat")
                    && !v.contains("ghp_"),
                "{url}: {k}: {v}"
            );
        }
    }
    drop(seen);

    // The same release with a token: the download must happen again, so clear the staged copy.
    std::fs::remove_dir_all(env.staged()).unwrap();
    let rec = Recorder::new(GitHub::new(m, s, exe));
    let st = fetch::fetch(&rec, Ok(Some(token::validate(TOKEN).unwrap())), &env.root, &pins);
    assert_eq!(st.error, None, "{st:?}");
    let seen = rec.seen.borrow();
    assert!(seen.iter().any(|(u, _)| host_of(u) == "release-assets.githubusercontent.com"));
    for (url, headers) in seen.iter() {
        let auth: Vec<_> = headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("authorization")).collect();
        if host_of(url) == "api.github.com" {
            assert_eq!(auth.len(), 1, "{url}");
            assert_eq!(auth[0].1, format!("Bearer {TOKEN}"));
        } else {
            assert!(auth.is_empty(), "token sent to {url}");
        }
    }
}

/// `Updater::tick`: a check that GitHub refused without a token (repository not public, rate limit),
/// like a rejected token, moves the next automatic check a full day out, so a tokenless install does
/// not ask again and again; other failures keep the schedule they had. No helper process is started.
#[test]
fn tester_scheduler_waits_a_day_after_anonymous_refusals() {
    use std::time::{Duration, Instant};
    for (code, waits) in [
        (ErrorCode::UpdateNotPublic, true),
        (ErrorCode::UpdateRateLimit, true),
        (ErrorCode::UpdateAuth, true),
        (ErrorCode::UpdateNetwork, false),
        (ErrorCode::UpdateTokenMissing, false),
        (ErrorCode::UpdateSignature, false),
    ] {
        let planned = Instant::now() + Duration::from_secs(3600);
        // As after the first tick; the initial-state job reads the real user's folders, so not here.
        let mut u =
            super::Updater { started: true, next_auto: Some(planned), busy_jobs: 1, ..super::Updater::default() };
        u.jobs_tx
            .send(Box::new(move |ui: &mut super::UpdateUi| {
                ui.status = super::Status::Failed;
                ui.error = Some((code, String::new()));
            }))
            .unwrap();
        assert!(!u.tick(true), "{code}: still busy");
        assert!(u.helper.is_none(), "{code}: a check was started");
        assert_eq!(u.state().status, super::Status::Failed, "{code}");
        let next = u.next_auto.unwrap();
        if waits {
            assert!(next >= Instant::now() + Duration::from_secs(23 * 3600), "{code}: next automatic check too soon");
        } else {
            assert_eq!(next, planned, "{code}: schedule changed");
        }
        // Ticking again does not start anything either (the next check is not due).
        assert!(!u.tick(true));
        assert!(u.helper.is_none(), "{code}: a check was started on the second tick");
    }
}

/// A saved token that cannot be used (empty, damaged, plain text instead of a DPAPI blob, a blob that
/// decrypts to something that is not a token, too large, a folder in its place) is FRG-E700. The helper
/// then makes no request at all (no silent fall-back to anonymous requests), skips the driver catalog
/// in the same run, and both status files carry FRG-E700 to the app. The token text never reaches them.
#[test]
fn tester_unusable_saved_token_is_reported_not_bypassed() {
    let base = fsx::tests::test_dir("tst-bad-token");
    let env = Env::new("tst-bad-token-env");
    let (m, s, exe) = release(&env.primary, "0.2.0", NOW);
    let good = token::protect(TOKEN.as_bytes()).unwrap();
    let not_a_token = token::protect(b"hello, this is not a GitHub token").unwrap();
    let mut damaged = good.clone();
    let n = damaged.len();
    damaged[n / 2] ^= 0x55;
    let cases: [(&str, Option<Vec<u8>>); 6] = [
        ("empty", Some(vec![])),
        ("damaged blob", Some(damaged)),
        ("plain token text", Some(TOKEN.as_bytes().to_vec())),
        ("blob of a non-token", Some(not_a_token)),
        ("too large", Some(vec![0x42; 5000])),
        ("folder", None),
    ];
    for (i, (name, content)) in cases.into_iter().enumerate() {
        let lad = base.join(format!("lad{i}"));
        let file = token::token_file_in(&lad);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        match content {
            Some(bytes) => std::fs::write(&file, bytes).unwrap(),
            None => std::fs::create_dir_all(&file).unwrap(),
        }
        let tok = token::load_from(&lad);
        assert_eq!(tok.as_ref().err().map(code_of), Some(ErrorCode::UpdateTokenMissing), "{name}");
        let gh = GitHub::public(m.clone(), s.clone(), exe.clone());
        let st = env.fetch_with(&gh, tok);
        assert_eq!(err_code(&st), Some(ErrorCode::UpdateTokenMissing), "{name}");
        assert!(gh.requests.borrow().is_empty(), "{name}: asked GitHub without the saved token");
        assert!(!env.staged().exists(), "{name}");
        // What `run_helper` does next: the catalog is skipped with the same code, and the next automatic
        // check is not pushed back (no request was made).
        let e = st.error.clone().unwrap();
        assert!(fetch::skip_catalog(e.code()) && !fetch::wait_a_day(e.code()), "{name}");
        let staged = lad.join("catalog-staged");
        let cst = super::catalog_fetch::failed(NOW, e.code(), &e.detail);
        super::catalog_fetch::write_status(&staged, &cst).unwrap();
        let back = super::catalog_fetch::read_status(&staged).unwrap().error.unwrap();
        assert_eq!(back.code(), ErrorCode::UpdateTokenMissing, "{name}");
        assert_eq!(fetch::read_status(&env.root).unwrap().error.unwrap().code(), ErrorCode::UpdateTokenMissing);
        for f in [env.root.join(fetch::STATUS_FILE), staged.join(super::catalog_fetch::STATUS_FILE)] {
            let text = std::fs::read_to_string(&f).unwrap();
            assert!(!text.contains("TESTTOKEN"), "{name}: {text}");
        }
    }
}

/// Anti-downgrade and replay protection do not depend on the token: without one, a release that is not
/// newer is neither offered nor staged, and the elevated side refuses a staged manifest that is older
/// than one already seen or not newer than the last installed update.
#[test]
fn tester_downgrades_and_replays_refused_without_a_token() {
    let env = Env::new("tst-anon-replay");
    for v in ["0.1.0", "0.0.9"] {
        let (m, s, exe) = release(&env.primary, v, NOW);
        let gh = GitHub::public(m, s, exe);
        let st = env.fetch_anonymous(&gh);
        assert_eq!((st.error.clone(), st.available.clone()), (None, None), "{v}");
        assert!(!env.staged().exists(), "{v} was staged");
        assert_eq!(gh.downloads_of(13), 0, "{v}: exe downloaded");
    }
    let (m, s, exe) = release(&env.primary, "0.3.0", NOW - 86_400);
    assert!(env.fetch_anonymous(&GitHub::public(m, s, exe)).available.is_some());
    let replay = Seen { last_installed: None, last_published: NOW };
    assert_eq!(code_of(&env.verify(replay).err().unwrap()), ErrorCode::UpdateDowngrade);
    let installed = Seen { last_installed: Some(Version(0, 3, 0)), last_published: 0 };
    assert_eq!(code_of(&env.verify(installed).err().unwrap()), ErrorCode::UpdateDowngrade);
    // A staged manifest re-signed by a foreign key after the check: refused by the elevated side.
    let stranger = key();
    let staged_m = std::fs::read(env.staged().join(fetch::MANIFEST_NAME)).unwrap();
    std::fs::write(env.staged().join(fetch::SIGNATURE_NAME), sign(&stranger, &staged_m)).unwrap();
    assert_eq!(code_of(&env.verify(Seen::default()).err().unwrap()), ErrorCode::UpdateSignature);
}

/// The repository pin holds with and without a token: another repository behind the pinned id stops the
/// check after the first request, and nothing is ever requested by owner/name.
#[test]
fn tester_repository_pin_with_and_without_a_token() {
    let env = Env::new("tst-pin");
    let (m, s, exe) = release(&env.primary, "0.2.0", NOW);
    for with_token in [false, true] {
        for other in [REPO + 1, 1, 0] {
            let gh = GitHub { reported_id: other, ..GitHub::public(m.clone(), s.clone(), exe.clone()) };
            let st = if with_token { env.fetch(&gh, TOKEN) } else { env.fetch_anonymous(&gh) };
            assert_eq!(err_code(&st), Some(ErrorCode::UpdateRepository), "token {with_token}, id {other}");
            let reqs = gh.requests.borrow();
            assert_eq!(reqs.len(), 1, "token {with_token}: went on after the id mismatch");
            assert_eq!(reqs[0].0, format!("https://api.github.com/repositories/{REPO}"));
            assert!(!env.staged().exists());
        }
    }
}
