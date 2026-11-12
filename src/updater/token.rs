//! The optional GitHub access token for the release repository.
//!
//! Without one, the helper asks GitHub's API anonymously, which works while the repository is public
//! (60 requests an hour per internet address; a daily check makes about eight). A token is needed only
//! while the repository is private, or to raise that limit.
//!
//! - A fine-grained personal access token restricted to the one repository with `Contents: Read`.
//! - Stored DPAPI-protected (`CryptProtectData`, *user* scope, fixed app entropy) in the desktop user's
//!   `%LOCALAPPDATA%\FanRGB\update_token.bin`: never in `%ProgramData%` (the elevated state folder, which
//!   every user can read), never in `config.json`, never in a log line or the diagnostics report.
//! - Only the unelevated `--update-fetch` helper decrypts it. The elevated UI encrypts and writes it while
//!   impersonating the desktop user (so DPAPI binds it to that user even with over-the-shoulder UAC),
//!   and otherwise only checks whether the file exists.
//! - [`redact`] masks anything token-shaped in text that leaves the helper (errors, status file).

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::error::{ErrorCode, WithCode, err};
use crate::util::fsx;

const TOKEN_FILE: &str = "update_token.bin";
/// Binds the DPAPI blob to FanRGB: another program running as the user can still decrypt it (DPAPI's
/// limit, see the threat model), but a blob from another app cannot be swapped in unnoticed.
const ENTROPY: &[u8] = b"FanRGB update token v1";
const MAX_BLOB: u64 = 4096;
/// GitHub token prefixes (fine-grained PATs, classic PATs, OAuth, user-to-server, server-to-server, refresh).
const PREFIXES: [&str; 6] = ["github_pat_", "ghp_", "gho_", "ghu_", "ghs_", "ghr_"];
/// The only kind of token [`validate`] accepts.
const FINE_GRAINED_PREFIX: &str = "github_pat_";

/// A token in memory; zeroed on drop and never printed by `Debug`.
pub struct Token(String);

impl Token {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        let mut v = std::mem::take(&mut self.0).into_bytes();
        v.iter_mut().for_each(|b| *b = 0);
        std::hint::black_box(&v);
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(***)")
    }
}

/// Checks what the user pasted: trims whitespace, requires a GitHub token prefix and only token
/// characters. Returns the cleaned token.
pub fn validate(pasted: &str) -> Result<Token, &'static str> {
    let t = pasted.trim();
    if t.is_empty() {
        return Err("Paste a GitHub access token.");
    }
    // Only a fine-grained token (repository-scoped, `Contents: Read`) may be saved: a classic PAT or an
    // OAuth token (`ghp_`, `gho_`, ...) usually carries account-wide `repo` write scope, and any process of
    // the user can decrypt the saved blob. [`redact`] still masks every token shape.
    if PREFIXES.iter().any(|p| *p != FINE_GRAINED_PREFIX && t.starts_with(p)) {
        return Err("This is a classic or OAuth GitHub token; create a fine-grained, read-only one (github_pat_).");
    }
    if !t.starts_with(FINE_GRAINED_PREFIX) {
        return Err("This is not a GitHub token (it should start with github_pat_).");
    }
    if t.len() < 30 || t.len() > 255 || !t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err("The token contains unexpected characters; copy it again from GitHub.");
    }
    Ok(Token(t.to_string()))
}

/// Replaces every token-shaped run (`github_pat_…`, `ghp_…`, …) and every `Bearer …` value in `s`.
pub fn redact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    'outer: while !rest.is_empty() {
        for p in PREFIXES.iter().copied().chain(["Bearer ", "bearer "]) {
            if rest.starts_with(p) {
                let tail = &rest[p.len()..];
                let n = tail.bytes().take_while(|b| b.is_ascii_alphanumeric() || *b == b'_').count();
                if n >= 4 {
                    out.push_str(p);
                    out.push_str("***");
                    rest = &tail[n..];
                    continue 'outer;
                }
            }
        }
        let c = rest.chars().next().unwrap_or(' ');
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

// ---------------------------------------------------------------------------------------
// DPAPI

fn blob(data: &[u8]) -> windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB {
    windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    }
}

/// Copies and frees a blob returned by DPAPI.
///
/// # Safety
/// `out` must have been filled by a successful CryptProtectData/CryptUnprotectData call.
unsafe fn take_blob(out: windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB) -> Vec<u8> {
    // SAFETY: per the contract, `pbData` points at `cbData` bytes allocated with LocalAlloc.
    unsafe {
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        std::ptr::write_bytes(out.pbData, 0, out.cbData as usize);
        windows_sys::Win32::Foundation::LocalFree(out.pbData as *mut core::ffi::c_void);
        v
    }
}

pub fn protect(plain: &[u8]) -> std::io::Result<Vec<u8>> {
    use windows_sys::Win32::Security::Cryptography::{CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData};
    let (input, entropy) = (blob(plain), blob(ENTROPY));
    let mut out = blob(&[]);
    // SAFETY: both input blobs point at live slices of the given lengths (DPAPI only reads them); `out`
    // receives a LocalAlloc'ed buffer that `take_blob` frees.
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the call succeeded.
    Ok(unsafe { take_blob(out) })
}

pub fn unprotect(data: &[u8]) -> std::io::Result<Vec<u8>> {
    use windows_sys::Win32::Security::Cryptography::{CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData};
    let (input, entropy) = (blob(data), blob(ENTROPY));
    let mut out = blob(&[]);
    // SAFETY: as in `protect`; no description is requested.
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the call succeeded.
    Ok(unsafe { take_blob(out) })
}

// ---------------------------------------------------------------------------------------
// Storage

/// `<LocalAppData>\FanRGB\update_token.bin` below the given LocalAppData folder.
pub fn token_file_in(local_app_data: &Path) -> PathBuf {
    local_app_data.join(crate::util::paths::APP_DIR).join(TOKEN_FILE)
}

/// Writes the encrypted token below `local_app_data` (run as the token's user: directly when not
/// elevated, or through [`crate::util::win::as_desktop_user`]).
pub fn store_in(local_app_data: &Path, token: &Token) -> Result<()> {
    let file = token_file_in(local_app_data);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).code(ErrorCode::Io)?;
    }
    let enc = protect(token.as_str().as_bytes()).code(ErrorCode::Io)?;
    fsx::write_atomic(&file, &enc).code(ErrorCode::Io)
}

/// Reads and decrypts the token (helper side, as the user). `Ok(None)`: no token saved, so the helper
/// asks GitHub without one. A saved token that cannot be read is an error (FRG-E700), not a silent fall
/// back to anonymous requests: the user saved it for a reason (a private repository).
pub fn load_from(local_app_data: &Path) -> Result<Option<Token>> {
    let file = token_file_in(local_app_data);
    let enc = match fsx::read_limited(&file, MAX_BLOB) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(err(ErrorCode::UpdateTokenMissing, format!("{}: {e}", file.display()))),
    };
    let mut plain = unprotect(&enc)
        .map_err(|e| err(ErrorCode::UpdateTokenMissing, format!("the saved token cannot be decrypted ({e})")))?;
    let text = String::from_utf8(std::mem::take(&mut plain)).unwrap_or_default();
    let t = validate(&text).map_err(|e| err(ErrorCode::UpdateTokenMissing, e));
    drop(Token(text));
    t.map(Some)
}

pub fn exists_in(local_app_data: &Path) -> bool {
    token_file_in(local_app_data).is_file()
}

/// Deletes the saved token (as its user).
pub fn delete_in(local_app_data: &Path) -> Result<()> {
    match std::fs::remove_file(token_file_in(local_app_data)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(err(ErrorCode::Io, e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAKE: &str = "github_pat_11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOP0123456789";

    #[test]
    fn pasted_tokens_are_validated() {
        assert_eq!(validate(&format!("  {FAKE}\r\n")).unwrap().as_str(), FAKE);
        // Classic and OAuth tokens are refused (usually neither read-only nor repository-scoped).
        for classic in ["ghp_0123456789abcdefghijklmnopqrstuvwxyzAB", "gho_0123456789abcdefghijklmnopqrstuvwxyzAB"] {
            assert!(validate(classic).unwrap_err().contains("fine-grained"), "{classic}");
        }
        for bad in ["", "   ", "hello", "github_pat_short", &format!("{FAKE} x"), &format!("{FAKE}\"")] {
            assert!(validate(bad).is_err(), "{bad:?}");
        }
        assert_eq!(format!("{:?}", validate(FAKE).unwrap()), "Token(***)");
    }

    #[test]
    fn redaction_masks_every_token_shape() {
        let s = format!("GET failed: Authorization: Bearer {FAKE} (token {FAKE}); ghp_abcdefgh1234 end");
        assert_eq!(redact("update token saved"), "update token saved");
        assert_eq!(redact("Authorization: Bearer abcd1234"), "Authorization: Bearer ***");
        let r = redact(&s);
        assert!(!r.contains("ABCDEFG0123456789"), "{r}");
        assert!(!r.contains("abcdefgh1234"), "{r}");
        assert!(r.contains("GET failed") && r.ends_with(" end"));
        assert_eq!(redact("fan 42 % – ünïcode"), "fan 42 % – ünïcode");
    }

    #[test]
    fn dpapi_round_trip_and_storage() {
        let home = crate::util::fsx::tests::test_dir("update-token");
        let t = validate(FAKE).unwrap();
        assert!(!exists_in(&home));
        // No token saved: not an error, the helper checks without one.
        assert!(load_from(&home).unwrap().is_none());
        store_in(&home, &t).unwrap();
        assert!(exists_in(&home));
        // Encrypted on disk: the token text never appears in the file.
        let raw = std::fs::read(token_file_in(&home)).unwrap();
        assert!(!raw.windows(20).any(|w| w == &FAKE.as_bytes()[..20]));
        assert_eq!(load_from(&home).unwrap().unwrap().as_str(), FAKE);
        // Another app's blob (other entropy) or a damaged file is refused (FRG-E700), not crashed on and
        // not silently replaced by a check without the token.
        let mut damaged = raw.clone();
        let n = damaged.len();
        damaged[n - 5] ^= 0xff;
        std::fs::write(token_file_in(&home), &damaged).unwrap();
        assert_eq!(crate::error::code_of(&load_from(&home).unwrap_err()), ErrorCode::UpdateTokenMissing);
        delete_in(&home).unwrap();
        delete_in(&home).unwrap();
        assert!(!exists_in(&home));
        assert!(load_from(&home).unwrap().is_none());
    }
}
