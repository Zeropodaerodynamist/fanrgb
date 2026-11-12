//! FanRGB release tooling (run by `tools/release.ps1`, never shipped).
//!
//! ```text
//! fanrgb-release keygen       --dir <key folder> [--no-password]
//! fanrgb-release protect-keys --dir <key folder>
//! fanrgb-release pubkeys      --dir <key folder>
//! fanrgb-release manifest     --exe <fanrgb.exe> --version <x.y.z> --notes-file <file> --out <release.json>
//!                             [--min-version <x.y.z>] [--published-at <unix seconds>]
//! fanrgb-release sign         --key <secret .key> --pub <.pub> --file <release.json> [--require-encrypted]
//! fanrgb-release verify       --pubkey <base64> --file <release.json>
//! fanrgb-release protect-keys --dir <key folder> --key catalog-backup
//!
//! # driver catalog (separate key pair: a catalog key can never sign a release, and the reverse)
//! fanrgb-release catalog-keygen --dir <key folder> --repo <owner/name> [--secret CATALOG_MINISIGN_KEY]
//! fanrgb-release catalog-sign   --file <drivers.json> (--key-env <VAR> | --key <secret .key>)
//!                               [--expect-pubkey <base64>]
//! fanrgb-release catalog-verify --file <drivers.json> --pubkeys <base64>[,<base64>]
//! ```
//!
//! `catalog-keygen` creates the catalog key pairs. The primary secret key goes straight from memory into
//! the GitHub Actions secret through `gh secret set`'s stdin (never to disk, never printed, never on a
//! command line); the backup secret key is written owner-only to `<key folder>\catalog-backup.key` (for
//! rotation; protect it with `protect-keys --key catalog-backup`). `catalog-sign` is what CI runs: it
//! reads the secret key from an environment variable (never an argument) and writes the trusted comment
//! `fanrgb-catalog serial=<serial> drivers.json`, which the app requires. `catalog-sign`,
//! `catalog-verify`, `manifest` and `verify` also build on Linux; the rest is Windows-only.
//!
//! Keys use the minisign file format (Ed25519, prehashed signatures, scrypt-encrypted secret keys), so the
//! official `minisign` CLI can sign or verify with them as well. Secret keys are written with an explicit
//! owner-only DACL (protected, nothing inherited) and are never printed.
//!
//! Passwords: `keygen` encrypts both secret keys with a password unless
//! `--no-password` is given (only `release.ps1 -DryRun` does that, for a throwaway key); `protect-keys`
//! encrypts existing unencrypted keys in place; `sign` asks for the password of an encrypted key. Every
//! password is read from the console (`CONIN$`) with echo off, never from an argument, a pipe or the
//! environment, so it cannot end up in a script, the shell history or a process list.

// Linux builds (the CI signer) only use the catalog and verify commands.
#![cfg_attr(not(windows), allow(dead_code, unused_imports))]

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const MAX_NOTES: usize = 8 * 1024;
const MAX_EXE: u64 = 128 << 20;
const KEY_NAMES: [&str; 2] = ["primary", "backup"];
/// Secret keys `protect-keys` knows: the release keys and the catalog backup key.
const PROTECTABLE: [&str; 3] = ["primary", "backup", "catalog-backup"];
/// Options that take no value.
const FLAGS: [&str; 2] = ["no-password", "require-encrypted"];

type Res<T> = Result<T, String>;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().cloned() else {
        eprintln!(
            "usage: fanrgb-release keygen|protect-keys|pubkeys|manifest|sign|verify|catalog-keygen|catalog-sign|catalog-verify \
             ... (see the source header)"
        );
        return ExitCode::from(2);
    };
    let opts = match parse_opts(&args[1..]) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let res = match cmd.as_str() {
        #[cfg(windows)]
        "keygen" => keygen(&opts),
        #[cfg(windows)]
        "protect-keys" => protect_keys(&opts),
        #[cfg(windows)]
        "sign" => sign(&opts),
        #[cfg(windows)]
        "catalog-keygen" => catalog::keygen(&opts),
        "pubkeys" => pubkeys(&opts),
        "manifest" => manifest(&opts),
        "verify" => verify(&opts),
        "catalog-sign" => catalog::sign(&opts),
        "catalog-verify" => catalog::verify(&opts),
        other => Err(format!("unknown command {other:?} (key generation and release signing are Windows-only)")),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse_opts(args: &[String]) -> Res<HashMap<String, String>> {
    let mut m = HashMap::new();
    let mut it = args.iter();
    while let Some(k) = it.next() {
        let key = k.strip_prefix("--").ok_or_else(|| format!("unexpected argument {k:?}"))?;
        if FLAGS.contains(&key) {
            m.insert(key.to_string(), "yes".to_string());
            continue;
        }
        let v = it.next().ok_or_else(|| format!("{k} needs a value"))?;
        m.insert(key.to_string(), v.clone());
    }
    Ok(m)
}

fn opt<'a>(o: &'a HashMap<String, String>, k: &str) -> Res<&'a str> {
    o.get(k).map(String::as_str).ok_or_else(|| format!("--{k} is required"))
}

/// `MAJOR.MINOR.PATCH`, digits only (the app's parser is just as strict).
fn check_version(v: &str) -> Res<()> {
    let parts: Vec<&str> = v.split('.').collect();
    let ok = parts.len() == 3
        && parts.iter().all(|p| !p.is_empty() && p.len() <= 9 && p.bytes().all(|b| b.is_ascii_digit()));
    if ok { Ok(()) } else { Err(format!("version {v:?} is not MAJOR.MINOR.PATCH")) }
}

// ---------------------------------------------------------------------------------------
// Keys

#[cfg(windows)]
fn keygen(o: &HashMap<String, String>) -> Res<()> {
    let dir = PathBuf::from(opt(o, "dir")?);
    let sddl = owner_only_sddl()?;
    acl::create_dir(&dir, &sddl)?;
    for name in KEY_NAMES {
        if dir.join(format!("{name}.key")).exists() {
            return Err(format!("{} already exists; refusing to overwrite a signing key", dir.join(name).display()));
        }
    }
    // One password for both keys (the backup key belongs on another medium anyway).
    let password = if o.contains_key("no-password") {
        println!("warning: writing UNENCRYPTED secret keys (only for a throwaway dry-run key)");
        None
    } else {
        Some(new_password()?)
    };
    for name in KEY_NAMES {
        let kp = match &password {
            None => minisign::KeyPair::generate_unencrypted_keypair(),
            Some(p) => minisign::KeyPair::generate_encrypted_keypair(Some(p.to_minisign())),
        }
        .map_err(|e| e.to_string())?;
        let sk_box = kp
            .sk
            .to_box(Some(&format!("FanRGB release signing key ({name}); keep secret, back up offline")))
            .map_err(|e| e.to_string())?;
        acl::write_new(&dir.join(format!("{name}.key")), sk_box.into_string().as_bytes(), &sddl)?;
        let pk_box = kp.pk.to_box().map_err(|e| e.to_string())?;
        acl::write_new(&dir.join(format!("{name}.pub")), pk_box.into_string().as_bytes(), &sddl)?;
    }
    println!("Created {} (owner-only). Public keys to compile into src/updater/keys.rs:", dir.display());
    pubkeys(o)
}

/// `protect-keys`: asks for a new password (twice) and encrypts every unencrypted `*.key` in `--dir` in
/// place (or only `--key <name>`: `primary`, `backup`, `catalog-backup`). Keys that are already
/// encrypted are left alone; if there is nothing to protect it refuses.
#[cfg(windows)]
fn protect_keys(o: &HashMap<String, String>) -> Res<()> {
    let dir = PathBuf::from(opt(o, "dir")?);
    let only = match o.get("key") {
        Some(k) if PROTECTABLE.contains(&k.as_str()) => Some(k.as_str()),
        Some(k) => return Err(format!("--key must be one of {}, not {k:?}", PROTECTABLE.join(", "))),
        None => None,
    };
    let todo: Vec<&str> = keys_to_protect(&dir)?.into_iter().filter(|n| only.is_none_or(|o| o == *n)).collect();
    if todo.is_empty() {
        return Err(format!("{} is not an unencrypted key in {}", only.unwrap_or("?"), dir.display()));
    }
    println!("Encrypting {} with a password (scrypt, minisign format).", todo.join(", "));
    let password = new_password()?;
    let sddl = owner_only_sddl()?;
    for name in &todo {
        protect_key(&dir, name, &password, &sddl)?;
        println!("{name}.key is now password-protected");
    }
    println!(
        "Done. The unencrypted bytes may survive in free disk space: move backup.key to offline media and \
         keep FanRGB's signing keys off any PC that runs FanRGB."
    );
    Ok(())
}

/// Names of the keys in `dir` that are still unencrypted; an error if none are.
fn keys_to_protect(dir: &Path) -> Res<Vec<&'static str>> {
    let mut todo = Vec::new();
    for name in PROTECTABLE {
        let path = dir.join(format!("{name}.key"));
        if !path.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        match kdf_alg(&text)? {
            KDF_NONE => todo.push(name),
            KDF_SCRYPT => println!("{name}.key is already password-protected; left as it is"),
            other => return Err(format!("{}: unknown key encryption {other:?}", path.display())),
        }
    }
    if todo.is_empty() {
        return Err(format!("no unencrypted signing key in {}; nothing to protect", dir.display()));
    }
    Ok(todo)
}

/// Encrypts `<dir>\<name>.key` in place: the encrypted key goes to `<name>.key.protecting` (new file,
/// owner-only, links refused), is read back, decrypted with `password` and compared with the original
/// key (and used to sign a probe that the matching `.pub` must verify, when present) before it replaces
/// the original in one rename.
#[cfg(windows)]
fn protect_key(dir: &Path, name: &str, password: &Password, sddl: &str) -> Res<()> {
    let path = dir.join(format!("{name}.key"));
    let tmp = dir.join(format!("{name}.key.protecting"));
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let sk = minisign::SecretKeyBox::from_string(&text)
        .and_then(|b| b.into_unencrypted_secret_key())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let comment = text
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("untrusted comment: "))
        .unwrap_or("FanRGB release signing key")
        .to_string();
    let enc = encrypt_key_bytes(&sk.to_bytes(), password.as_bytes(), random_salt()?)?;
    let enc_box = minisign::SecretKey::from_bytes(&enc)
        .and_then(|k| k.to_box(Some(&comment)))
        .map_err(|e| e.to_string())?
        .into_string();
    acl::write_new(&tmp, enc_box.as_bytes(), sddl)?;
    let checked = (|| -> Res<()> {
        let back = std::fs::read_to_string(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        if back != enc_box {
            return Err(format!("{} does not read back as written", tmp.display()));
        }
        let dec = minisign::SecretKeyBox::from_string(&back)
            .and_then(|b| b.into_secret_key(Some(password.to_minisign())))
            .map_err(|e| format!("the encrypted key does not decrypt: {e}"))?;
        if dec != sk || dec.keynum() != sk.keynum() {
            return Err("the encrypted key decrypts to a different key".into());
        }
        let pub_path = dir.join(format!("{name}.pub"));
        if pub_path.exists() {
            let pk = minisign::PublicKey::from_file(&pub_path).map_err(|e| e.to_string())?;
            let probe = b"fanrgb-release protect-keys probe";
            let sig = minisign::sign(Some(&pk), &dec, &probe[..], None, None).map_err(|e| e.to_string())?;
            minisign::verify(&pk, &sig, std::io::Cursor::new(&probe[..]), true, false, false)
                .map_err(|e| format!("{} does not verify a signature of the protected key: {e}", pub_path.display()))?;
        }
        Ok(())
    })();
    if let Err(e) = checked {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    acl::replace(&tmp, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

const KDF_NONE: [u8; 2] = [0, 0];
const KDF_SCRYPT: [u8; 2] = *b"Sc";
/// minisign's scrypt limits (`OPSLIMIT`, `MEMLIMIT`), which map to N = 2^15, r = 8, p = 1.
const OPSLIMIT: u64 = 1_048_576;
const MEMLIMIT: u64 = 33_554_432;
/// Layout of a minisign secret key: sig_alg 2, kdf_alg 2, chk_alg 2, salt 32, opslimit 8, memlimit 8,
/// then keynum 8 + secret key 64 + checksum 32, which are encrypted (XOR with the scrypt stream).
const SK_LEN: usize = 158;
const SALT_AT: usize = 6;
const LIMITS_AT: usize = 38;
const SECRET_AT: usize = 54;

/// `kdf_alg` of a minisign secret key file (second line, base64).
fn kdf_alg(text: &str) -> Res<[u8; 2]> {
    let b64 = text.lines().nth(1).ok_or("not a minisign secret key (no key line)")?.trim();
    let head = base64_prefix(b64, 6).ok_or("not a minisign secret key (bad base64)")?;
    Ok([head[2], head[3]])
}

/// Decodes the first `n` bytes of standard base64 (enough to read the key header).
fn base64_prefix(s: &str, n: usize) -> Option<Vec<u8>> {
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
    let mut out = Vec::with_capacity(n + 2);
    for chunk in s.as_bytes().chunks(4) {
        if out.len() >= n || chunk.len() < 4 {
            break;
        }
        let v = (val(chunk[0])? << 18) | (val(chunk[1])? << 12) | (val(chunk[2])? << 6) | val(chunk[3])?;
        out.extend_from_slice(&[(v >> 16) as u8, (v >> 8) as u8, v as u8]);
    }
    (out.len() >= n).then(|| {
        out.truncate(n);
        out
    })
}

/// Turns the bytes of an unencrypted minisign secret key into the bytes of the same key encrypted with
/// `password`, exactly as `minisign` does it (so `minisign` and `into_secret_key` can decrypt it).
fn encrypt_key_bytes(plain: &[u8], password: &[u8], salt: [u8; 32]) -> Res<Vec<u8>> {
    if plain.len() != SK_LEN || plain[2..4] != KDF_NONE {
        return Err("not an unencrypted minisign secret key".into());
    }
    if password.is_empty() {
        return Err("empty password".into());
    }
    let mut out = plain.to_vec();
    out[2..4].copy_from_slice(&KDF_SCRYPT);
    out[SALT_AT..SALT_AT + 32].copy_from_slice(&salt);
    out[LIMITS_AT..LIMITS_AT + 8].copy_from_slice(&OPSLIMIT.to_le_bytes());
    out[LIMITS_AT + 8..LIMITS_AT + 16].copy_from_slice(&MEMLIMIT.to_le_bytes());
    let params = scrypt::Params::new(15, 8, 1, scrypt::Params::RECOMMENDED_LEN).map_err(|e| e.to_string())?;
    let mut stream = [0u8; SK_LEN - SECRET_AT];
    scrypt::scrypt(password, &salt, &params, &mut stream).map_err(|e| e.to_string())?;
    for (b, s) in out[SECRET_AT..].iter_mut().zip(stream.iter()) {
        *b ^= *s;
    }
    stream.iter_mut().for_each(|b| *b = 0);
    std::hint::black_box(&stream);
    Ok(out)
}

#[cfg(windows)]
fn random_salt() -> Res<[u8; 32]> {
    use windows_sys::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
    let mut salt = [0u8; 32];
    // SAFETY: `salt` is writable for the 32 bytes passed; no algorithm handle with the system RNG flag.
    let st = unsafe { BCryptGenRandom(std::ptr::null_mut(), salt.as_mut_ptr(), 32, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    if st != 0 {
        return Err(format!("BCryptGenRandom failed ({st:#x})"));
    }
    Ok(salt)
}

// ---------------------------------------------------------------------------------------
// Passwords

/// A password in memory; zeroed on drop. `minisign` takes passwords as `String` by value, so
/// [`Password::to_minisign`] hands it a copy that the crate drops without wiping (a limit of the crate).
struct Password(String);

impl Password {
    fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    fn to_minisign(&self) -> String {
        self.0.clone()
    }
}

impl Drop for Password {
    fn drop(&mut self) {
        let mut v = std::mem::take(&mut self.0).into_bytes();
        v.iter_mut().for_each(|b| *b = 0);
        std::hint::black_box(&v);
    }
}

/// minisign's limit.
const MAX_PASSWORD: usize = 1024;
const MIN_NEW_PASSWORD: usize = 12;

/// Asks twice for a new password (at least [`MIN_NEW_PASSWORD`] characters).
#[cfg(windows)]
fn new_password() -> Res<Password> {
    let p1 = console::read_password("New password for the signing keys: ")?;
    if p1.0.chars().count() < MIN_NEW_PASSWORD {
        return Err(format!("the password must have at least {MIN_NEW_PASSWORD} characters"));
    }
    let p2 = console::read_password("Same password again: ")?;
    if p1.0 != p2.0 {
        return Err("the passwords do not match".into());
    }
    Ok(p1)
}

/// The line the console returned, without its line end.
fn strip_line_end(s: &str) -> &str {
    s.trim_end_matches(['\r', '\n'])
}

#[cfg(windows)]
mod console {
    use super::{MAX_PASSWORD, Password, Res, strip_line_end};
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows_sys::Win32::System::Console::{
        ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, GetConsoleMode, ReadConsoleW, SetConsoleMode,
    };

    /// Reads one line from the console window itself (`CONIN$`, not stdin) with echo off. Fails when
    /// there is no console (a password is never taken from a pipe or a file).
    pub fn read_password(prompt: &str) -> Res<Password> {
        let name: Vec<u16> = "CONIN$".encode_utf16().chain(Some(0)).collect();
        // SAFETY: `name` is NUL-terminated; the handle is closed below.
        let h = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null_mut(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err("no console to read the password from; run this in an interactive terminal".into());
        }
        let mut mode = 0u32;
        // SAFETY: `h` is a console input handle; `mode` is a valid out pointer.
        if unsafe { GetConsoleMode(h, &mut mode) } == 0 {
            // SAFETY: closing our own handle once.
            unsafe { CloseHandle(h) };
            return Err("CONIN$ is not a console".into());
        }
        eprint!("{prompt}");
        let quiet = (mode & !ENABLE_ECHO_INPUT) | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT;
        let mut buf = vec![0u16; MAX_PASSWORD + 2];
        let mut line: Vec<u16> = Vec::with_capacity(MAX_PASSWORD + 2);
        // SAFETY: `h` is a console input handle; `buf` is writable for the length passed; the original
        // mode is restored before returning.
        let ok = unsafe {
            let mut ok = SetConsoleMode(h, quiet) != 0;
            while ok && !line.contains(&(b'\n' as u16)) && line.len() <= MAX_PASSWORD {
                let mut got = 0u32;
                ok = ReadConsoleW(h, buf.as_mut_ptr().cast(), buf.len() as u32, &mut got, null_mut()) != 0 && got > 0;
                if ok {
                    line.extend_from_slice(&buf[..got as usize]);
                }
            }
            SetConsoleMode(h, mode);
            CloseHandle(h);
            ok
        };
        eprintln!();
        buf.iter_mut().for_each(|w| *w = 0);
        let text = String::from_utf16(&line);
        line.iter_mut().for_each(|w| *w = 0);
        std::hint::black_box((&buf, &line));
        if !ok {
            return Err("reading the password failed".into());
        }
        let text = Password(text.map_err(|_| "the password is not valid text".to_string())?);
        let p = Password(strip_line_end(&text.0).to_string());
        if p.0.is_empty() {
            return Err("empty password".into());
        }
        if p.0.len() > MAX_PASSWORD {
            return Err(format!("the password is longer than {MAX_PASSWORD} bytes"));
        }
        Ok(p)
    }
}

fn pubkeys(o: &HashMap<String, String>) -> Res<()> {
    let dir = PathBuf::from(opt(o, "dir")?);
    for name in KEY_NAMES {
        let pk = minisign::PublicKey::from_file(dir.join(format!("{name}.pub"))).map_err(|e| e.to_string())?;
        println!("{name}: {}", pk.to_base64());
    }
    Ok(())
}

/// The current user's SID as an SDDL string granting only that user (and owned by it).
#[cfg(windows)]
fn owner_only_sddl() -> Res<String> {
    let sid = acl::current_user_sid()?;
    Ok(format!("O:{sid}D:P(A;OICI;FA;;;{sid})"))
}

// ---------------------------------------------------------------------------------------
// Manifest

fn manifest(o: &HashMap<String, String>) -> Res<()> {
    let exe = PathBuf::from(opt(o, "exe")?);
    let version = opt(o, "version")?;
    check_version(version)?;
    let min_version = o.get("min-version").cloned();
    if let Some(m) = &min_version {
        check_version(m)?;
    }
    let notes = std::fs::read_to_string(opt(o, "notes-file")?).map_err(|e| format!("notes: {e}"))?;
    // Windows editors and PowerShell often write a UTF-8 BOM.
    let notes = notes.trim_start_matches('\u{feff}').trim().replace("\r\n", "\n");
    if notes.len() > MAX_NOTES {
        return Err(format!("release notes are longer than {MAX_NOTES} bytes"));
    }
    let bytes = std::fs::read(&exe).map_err(|e| format!("{}: {e}", exe.display()))?;
    if bytes.len() as u64 > MAX_EXE || bytes.len() < 1024 || &bytes[..2] != b"MZ" {
        return Err(format!("{} does not look like a Windows executable", exe.display()));
    }
    let published_at = match o.get("published-at") {
        Some(s) => s.parse::<u64>().map_err(|_| "--published-at must be unix seconds".to_string())?,
        None => {
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_secs()
        }
    };
    let sha = {
        use sha2::Digest;
        let d = sha2::Sha256::digest(&bytes);
        d.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    let m = serde_json::json!({
        "schema": 1,
        "app": "fanrgb",
        "version": version,
        "min_version": min_version,
        "published_at": published_at,
        "notes": notes,
        "asset": { "name": "fanrgb.exe", "size": bytes.len(), "sha256": sha },
    });
    let mut text = serde_json::to_string_pretty(&m).map_err(|e| e.to_string())?;
    text.push('\n');
    let out = PathBuf::from(opt(o, "out")?);
    std::fs::write(&out, text).map_err(|e| format!("{}: {e}", out.display()))?;
    println!("wrote {} (version {version}, {} bytes, sha256 {sha})", out.display(), bytes.len());
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Signing

#[cfg(windows)]
fn sign(o: &HashMap<String, String>) -> Res<()> {
    let key = PathBuf::from(opt(o, "key")?);
    let pk = minisign::PublicKey::from_file(opt(o, "pub")?).map_err(|e| e.to_string())?;
    let file = PathBuf::from(opt(o, "file")?);
    let data = std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let version = serde_json::from_slice::<serde_json::Value>(&data)
        .ok()
        .and_then(|v| v.get("version").and_then(|v| v.as_str()).map(str::to_string))
        .ok_or("the file is not a FanRGB release manifest")?;
    let sk_text = std::fs::read_to_string(&key).map_err(|e| format!("{}: {e}", key.display()))?;
    let sk = open_secret_key(&sk_text, o.contains_key("require-encrypted"), || {
        console::read_password(&format!("Password for {}: ", key.display()))
    })?;
    drop(sk_text);
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let trusted = format!("fanrgb {version} {name}");
    let sig = minisign::sign(Some(&pk), &sk, data.as_slice(), Some(&trusted), Some("FanRGB release signature"))
        .map_err(|e| e.to_string())?;
    let out = sig_path(&file);
    std::fs::write(&out, sig.into_string()).map_err(|e| format!("{}: {e}", out.display()))?;
    println!("signed {} -> {}", file.display(), out.display());
    Ok(())
}

/// Opens a secret key file's text. An encrypted key asks `password` (the console prompt); an
/// unencrypted one is refused with `require_encrypted` (real releases) and warned about otherwise.
fn open_secret_key(text: &str, require_encrypted: bool, password: impl FnOnce() -> Res<Password>) -> Res<minisign::SecretKey> {
    let open = || minisign::SecretKeyBox::from_string(text).map_err(|e| e.to_string());
    match kdf_alg(text)? {
        KDF_NONE if require_encrypted => Err(
            "this signing key has no password; run `fanrgb-release protect-keys --dir <key folder>` first"
                .into(),
        ),
        KDF_NONE => {
            println!("warning: signing with an UNENCRYPTED key");
            open()?.into_unencrypted_secret_key().map_err(|e| e.to_string())
        }
        _ => {
            let p = password()?;
            open()?.into_secret_key(Some(p.to_minisign())).map_err(|e| e.to_string())
        }
    }
}

fn sig_path(file: &Path) -> PathBuf {
    let mut s = file.as_os_str().to_os_string();
    s.push(".minisig");
    PathBuf::from(s)
}

/// Verifies with the same crate and settings as the app (prehashed only, no legacy signatures).
fn verify(o: &HashMap<String, String>) -> Res<()> {
    let pk = minisign_verify::PublicKey::from_base64(opt(o, "pubkey")?.trim()).map_err(|e| e.to_string())?;
    let file = PathBuf::from(opt(o, "file")?);
    let data = std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let sig_text = std::fs::read_to_string(sig_path(&file)).map_err(|e| format!("signature: {e}"))?;
    let sig = minisign_verify::Signature::decode(&sig_text).map_err(|e| e.to_string())?;
    pk.verify(&data, &sig, false).map_err(|e| e.to_string())?;
    println!("OK: {} is signed by the given key ({})", file.display(), sig.trusted_comment());
    let _ = std::io::stdout().flush();
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Owner-only files (Win32)

#[cfg(windows)]
mod acl {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT,
        FlushFileBuffers, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW, WriteFile,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    struct Sd(PSECURITY_DESCRIPTOR);

    impl Sd {
        fn new(sddl: &str) -> Result<Sd, String> {
            let s: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
            let mut sd: PSECURITY_DESCRIPTOR = null_mut();
            // SAFETY: `s` is NUL-terminated; `sd` is a valid out pointer, freed in Drop.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(s.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut())
            };
            if ok == 0 {
                return Err(format!("bad SDDL: {}", std::io::Error::last_os_error()));
            }
            Ok(Sd(sd))
        }

        fn attrs(&self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.0,
                bInheritHandle: 0,
            }
        }
    }

    impl Drop for Sd {
        fn drop(&mut self) {
            // SAFETY: allocated by the system (LocalAlloc), freed once.
            unsafe { LocalFree(self.0) };
        }
    }

    pub fn current_user_sid() -> Result<String, String> {
        // SAFETY: token handle closed below; `buf` is 8-byte aligned and its size is passed; the SID
        // string is freed with LocalFree after copying.
        unsafe {
            let mut token: HANDLE = null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err("OpenProcessToken failed".into());
            }
            let mut buf = vec![0u64; 64];
            let mut len = 0u32;
            let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr() as *mut c_void, 512, &mut len);
            CloseHandle(token);
            if ok == 0 {
                return Err("GetTokenInformation failed".into());
            }
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut s: *mut u16 = null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut s) == 0 {
                return Err("ConvertSidToStringSidW failed".into());
            }
            let mut n = 0;
            while *s.add(n) != 0 {
                n += 1;
            }
            let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
            LocalFree(s as *mut c_void);
            Ok(out)
        }
    }

    /// Creates `dir` with the given DACL; an existing folder is accepted (its keys are never overwritten).
    pub fn create_dir(dir: &Path, sddl: &str) -> Result<(), String> {
        let sd = Sd::new(sddl)?;
        let sa = sd.attrs();
        let w = wide(dir);
        // SAFETY: `w` is NUL-terminated; `sa` points at a live descriptor.
        if unsafe { CreateDirectoryW(w.as_ptr(), &sa) } == 0 {
            // SAFETY: plain call.
            let e = unsafe { GetLastError() };
            if e != 183 {
                return Err(format!("creating {}: {}", dir.display(), std::io::Error::from_raw_os_error(e as i32)));
            }
            eprintln!("note: {} already exists; its ACL is left as it is", dir.display());
        }
        Ok(())
    }

    /// Writes a new file (never an existing one, never through a link) with the given DACL.
    pub fn write_new(path: &Path, bytes: &[u8], sddl: &str) -> Result<(), String> {
        let sd = Sd::new(sddl)?;
        let sa = sd.attrs();
        let w = wide(path);
        // SAFETY: `w` is NUL-terminated; `sa` points at a live descriptor; the handle is closed below.
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                GENERIC_WRITE,
                0,
                &sa,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(format!("creating {}: {}", path.display(), std::io::Error::last_os_error()));
        }
        let mut written = 0u32;
        // SAFETY: `h` is a valid file handle; `bytes` is readable for its length.
        let ok = unsafe {
            WriteFile(h, bytes.as_ptr(), bytes.len() as u32, &mut written, null_mut()) != 0 && FlushFileBuffers(h) != 0
        };
        // SAFETY: closing our own handle once.
        unsafe { CloseHandle(h) };
        if !ok || written as usize != bytes.len() {
            return Err(format!("writing {} failed", path.display()));
        }
        Ok(())
    }

    /// Replaces `to` with `from` in one rename (write-through). The file keeps `from`'s DACL.
    pub fn replace(from: &Path, to: &Path) -> Result<(), String> {
        let (f, t) = (wide(from), wide(to));
        // SAFETY: both are NUL-terminated wide paths alive for the call.
        if unsafe { MoveFileExW(f.as_ptr(), t.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) } == 0 {
            return Err(format!("replacing {}: {}", to.display(), std::io::Error::last_os_error()));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// Driver catalog

mod catalog {
    use super::*;

    /// `kind` of a driver catalog; anything else is not signed by `catalog-sign`.
    pub const KIND: &str = "fanrgb-driver-catalog";
    /// Prefix of the trusted comment the app requires (`drivers::catalog::TRUSTED_PREFIX`).
    pub const TRUSTED_PREFIX: &str = "fanrgb-catalog serial=";
    #[cfg(windows)]
    const DEFAULT_SECRET: &str = "CATALOG_MINISIGN_KEY";
    #[cfg(windows)]
    const FILES: [&str; 3] = ["catalog-backup.key", "catalog-primary.pub", "catalog-backup.pub"];

    /// The catalog's serial (`kind` must be a driver catalog, serial at least 1).
    pub fn serial_of(data: &[u8]) -> Res<u64> {
        let v: serde_json::Value = serde_json::from_slice(data).map_err(|e| format!("the catalog is not JSON: {e}"))?;
        if v.get("kind").and_then(|k| k.as_str()) != Some(KIND) {
            return Err(format!("not a FanRGB driver catalog (kind must be {KIND:?})"));
        }
        v.get("serial").and_then(|s| s.as_u64()).filter(|&s| s >= 1).ok_or_else(|| "the catalog has no serial >= 1".into())
    }

    /// The trusted comment the app checks: domain (`fanrgb-catalog`) + the signed serial.
    pub fn trusted_comment(serial: u64) -> String {
        format!("{TRUSTED_PREFIX}{serial} drivers.json")
    }

    /// Signs catalog bytes; returns the `.minisig` text.
    pub fn sign_bytes(data: &[u8], sk: &minisign::SecretKey) -> Res<String> {
        let serial = serial_of(data)?;
        let pk = minisign::PublicKey::from_secret_key(sk).map_err(|e| e.to_string())?;
        let sig = minisign::sign(Some(&pk), sk, data, Some(&trusted_comment(serial)), Some("FanRGB driver catalog signature"))
            .map_err(|e| e.to_string())?;
        Ok(sig.into_string())
    }

    /// Verifies like the app: prehashed only, a trusted key, a catalog trusted comment whose serial is
    /// the catalog's. Returns the index of the key that signed.
    pub fn verify_bytes(data: &[u8], sig_text: &str, pubkeys: &[&str]) -> Res<usize> {
        let serial = serial_of(data)?;
        let sig = minisign_verify::Signature::decode(sig_text).map_err(|e| format!("signature: {e}"))?;
        for (i, k) in pubkeys.iter().enumerate() {
            let pk = minisign_verify::PublicKey::from_base64(k.trim()).map_err(|e| format!("public key {i}: {e}"))?;
            match pk.verify(data, &sig, false) {
                Ok(()) => {
                    let want = trusted_comment(serial);
                    if sig.trusted_comment() != want {
                        return Err(format!("trusted comment {:?} is not {want:?}", sig.trusted_comment()));
                    }
                    return Ok(i);
                }
                Err(minisign_verify::Error::UnexpectedKeyId) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        Err("signed with none of the given keys".into())
    }

    /// `catalog-sign`: CI signs with `--key-env CATALOG_MINISIGN_KEY` (the secret key text in the
    /// environment of that one step); `--key <file>` signs with a key file (the backup key, during a
    /// rotation; an encrypted one asks for its password on the console).
    pub fn sign(o: &HashMap<String, String>) -> Res<()> {
        let file = PathBuf::from(opt(o, "file")?);
        let data = std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        let sk = match (o.get("key-env"), o.get("key")) {
            (Some(var), None) => {
                let mut text = std::env::var(var).map_err(|_| format!("environment variable {var} is not set"))?;
                let sk = open_secret_key(&text, false, || Err("an encrypted key cannot come from the environment".into()));
                // SAFETY: all-zero bytes are valid UTF-8.
                unsafe { text.as_bytes_mut() }.fill(0);
                sk?
            }
            (None, Some(path)) => {
                let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
                open_secret_key(&text, false, || read_password(path))?
            }
            _ => return Err("give exactly one of --key-env <VAR> or --key <secret .key>".into()),
        };
        let sig = sign_bytes(&data, &sk)?;
        if let Some(pk) = o.get("expect-pubkey") {
            verify_bytes(&data, &sig, &[pk.as_str()]).map_err(|e| format!("the new signature does not verify: {e}"))?;
        }
        let out = sig_path(&file);
        std::fs::write(&out, sig).map_err(|e| format!("{}: {e}", out.display()))?;
        println!("signed {} (serial {}) -> {}", file.display(), serial_of(&data)?, out.display());
        Ok(())
    }

    #[cfg(windows)]
    fn read_password(path: &str) -> Res<Password> {
        console::read_password(&format!("Password for {path}: "))
    }

    #[cfg(not(windows))]
    fn read_password(_path: &str) -> Res<Password> {
        Err("encrypted keys can only be opened on Windows".into())
    }

    /// `catalog-verify --file drivers.json --pubkeys <primary>,<backup>`.
    pub fn verify(o: &HashMap<String, String>) -> Res<()> {
        let file = PathBuf::from(opt(o, "file")?);
        let data = std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        let sig = std::fs::read_to_string(sig_path(&file)).map_err(|e| format!("signature: {e}"))?;
        let keys: Vec<&str> = opt(o, "pubkeys")?.split(',').map(str::trim).filter(|k| !k.is_empty()).collect();
        let i = verify_bytes(&data, &sig, &keys)?;
        println!("OK: {} (serial {}) is signed by catalog key #{i}", file.display(), serial_of(&data)?);
        Ok(())
    }

    /// `catalog-keygen`: two new unencrypted key pairs. The primary secret key is handed to
    /// `gh secret set <secret> --repo <repo> --app actions` on its stdin and never touches the disk; only
    /// when that succeeded are the backup secret key (owner-only) and both public keys written.
    #[cfg(windows)]
    pub fn keygen(o: &HashMap<String, String>) -> Res<()> {
        use std::process::{Command, Stdio};
        let dir = PathBuf::from(opt(o, "dir")?);
        let repo = opt(o, "repo")?;
        let secret = o.get("secret").map(String::as_str).unwrap_or(DEFAULT_SECRET);
        let name_ok = |s: &str, extra: &[u8]| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || extra.contains(&b));
        if !name_ok(secret, b"_") || !repo.split_once('/').is_some_and(|(a, b)| name_ok(a, b"-_.") && name_ok(b, b"-_.")) {
            return Err("--repo must be owner/name and --secret a plain secret name".into());
        }
        let sddl = owner_only_sddl()?;
        acl::create_dir(&dir, &sddl)?;
        for f in FILES {
            if dir.join(f).exists() {
                return Err(format!("{} already exists; refusing to replace catalog keys", dir.join(f).display()));
            }
        }
        let primary = minisign::KeyPair::generate_unencrypted_keypair().map_err(|e| e.to_string())?;
        let backup = minisign::KeyPair::generate_unencrypted_keypair().map_err(|e| e.to_string())?;
        println!("catalog primary public key: {}", primary.pk.to_base64());
        println!("catalog backup public key:  {}", backup.pk.to_base64());
        let pub_boxes = [
            ("catalog-primary.pub", primary.pk.to_box().map_err(|e| e.to_string())?.into_string()),
            ("catalog-backup.pub", backup.pk.to_box().map_err(|e| e.to_string())?.into_string()),
        ];
        let mut primary_box = primary
            .sk
            .to_box(Some("FanRGB driver-catalog signing key (primary; CI secret only)"))
            .map_err(|e| e.to_string())?
            .into_string();
        let sent = (|| -> Res<()> {
            let mut child = Command::new("gh")
                .args(["secret", "set", secret, "--repo", repo, "--app", "actions"])
                .stdin(Stdio::piped())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn()
                .map_err(|e| format!("cannot run gh: {e}"))?;
            let mut stdin = child.stdin.take().ok_or("no stdin for gh")?;
            stdin.write_all(primary_box.as_bytes()).map_err(|e| format!("writing to gh: {e}"))?;
            drop(stdin);
            let st = child.wait().map_err(|e| e.to_string())?;
            if st.success() { Ok(()) } else { Err(format!("gh secret set failed ({st})")) }
        })();
        // SAFETY: all-zero bytes are valid UTF-8.
        unsafe { primary_box.as_bytes_mut() }.fill(0);
        sent.map_err(|e| format!("{e}; no key file was written, run catalog-keygen again"))?;
        let backup_box = backup
            .sk
            .to_box(Some("FanRGB driver-catalog signing key (backup; rotation only, keep offline)"))
            .map_err(|e| e.to_string())?
            .into_string();
        let mut backup_box = backup_box;
        let written = acl::write_new(&dir.join("catalog-backup.key"), backup_box.as_bytes(), &sddl);
        // SAFETY: all-zero bytes are valid UTF-8.
        unsafe { backup_box.as_bytes_mut() }.fill(0);
        written?;
        for (name, text) in &pub_boxes {
            acl::write_new(&dir.join(name), text.as_bytes(), &sddl)?;
        }
        println!(
            "Set the Actions secret {secret} on {repo} and wrote {} (owner-only, NOT password-protected yet: \
             run `fanrgb-release protect-keys --dir {} --key catalog-backup`, then move it offline).",
            dir.join("catalog-backup.key").display(),
            dir.display()
        );
        println!("Compile both public keys into src/drivers/catalog/keys.rs.");
        Ok(())
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::catalog::*;

    fn cat(serial: u64) -> Vec<u8> {
        format!(r#"{{"schema_version":"1.0","kind":"{KIND}","serial":{serial},"entries":[]}}"#).into_bytes()
    }

    #[test]
    fn sign_and_verify_like_the_app() {
        let a = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let b = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let data = cat(7);
        let sig = sign_bytes(&data, &a.sk).unwrap();
        assert!(sig.contains("trusted comment: fanrgb-catalog serial=7 drivers.json"), "{sig}");
        let (pa, pb) = (a.pk.to_base64(), b.pk.to_base64());
        assert_eq!(verify_bytes(&data, &sig, &[&pb, &pa]).unwrap(), 1);
        assert!(verify_bytes(&data, &sig, &[&pb]).is_err(), "wrong key");
        // Tampered bytes, or a serial edited after signing.
        let mut t = data.clone();
        t[5] ^= 1;
        assert!(verify_bytes(&t, &sig, &[&pa]).is_err());
        assert!(verify_bytes(&cat(8), &sig, &[&pa]).is_err());
        // Something that is not a catalog is not signed.
        assert!(sign_bytes(br#"{"schema":1,"app":"fanrgb","version":"0.2.0"}"#, &a.sk).is_err());
        assert!(sign_bytes(&cat(0), &a.sk).is_err());
    }

    /// A release signature (trusted comment "fanrgb <version> release.json") never passes as a catalog
    /// signature, even with the same key.
    #[test]
    fn release_signatures_are_not_catalog_signatures() {
        let a = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let data = cat(3);
        let sig = minisign::sign(Some(&a.pk), &a.sk, data.as_slice(), Some("fanrgb 0.2.0 release.json"), None).unwrap();
        assert!(verify_bytes(&data, &sig.into_string(), &[&a.pk.to_base64()]).is_err());
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fanrgb-release-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn pw(s: &str) -> Password {
        Password(s.to_string())
    }

    fn write_unencrypted(dir: &Path, name: &str) -> minisign::KeyPair {
        let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        std::fs::write(dir.join(format!("{name}.key")), kp.sk.to_box(Some("test key")).unwrap().into_string()).unwrap();
        std::fs::write(dir.join(format!("{name}.pub")), kp.pk.to_box().unwrap().into_string()).unwrap();
        kp
    }

    #[test]
    fn flags_take_no_value() {
        let args: Vec<String> =
            ["--dir", "x", "--no-password", "--require-encrypted"].iter().map(|s| s.to_string()).collect();
        let o = parse_opts(&args).unwrap();
        assert_eq!(o["dir"], "x");
        assert!(o.contains_key("no-password") && o.contains_key("require-encrypted"));
        assert!(parse_opts(&["--dir".to_string()]).is_err());
    }

    #[test]
    fn line_end_and_base64_header() {
        assert_eq!(strip_line_end("secret\r\n"), "secret");
        assert_eq!(strip_line_end("se cret\n"), "se cret");
        assert_eq!(base64_prefix("AAECAwQF", 6), Some(vec![0, 1, 2, 3, 4, 5]));
        assert_eq!(base64_prefix("AAE", 3), None);
        assert_eq!(base64_prefix("A*EC", 3), None);
    }

    /// The re-encryption must produce exactly what minisign itself decrypts.
    #[test]
    fn encrypted_key_decrypts_with_minisign() {
        let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let enc = encrypt_key_bytes(&kp.sk.to_bytes(), b"correct horse battery", random_salt().unwrap()).unwrap();
        let text = minisign::SecretKey::from_bytes(&enc).unwrap().to_box(Some("c")).unwrap().into_string();
        assert_eq!(kdf_alg(&text).unwrap(), KDF_SCRYPT);
        let dec = minisign::SecretKeyBox::from_string(&text)
            .unwrap()
            .into_secret_key(Some("correct horse battery".into()))
            .unwrap();
        assert!(dec == kp.sk && dec.keynum() == kp.sk.keynum());
        assert!(
            minisign::SecretKeyBox::from_string(&text).unwrap().into_secret_key(Some("wrong".into())).is_err(),
            "a wrong password must not decrypt"
        );
        assert!(encrypt_key_bytes(&enc, b"x", [0; 32]).is_err(), "already encrypted");
        assert!(encrypt_key_bytes(&kp.sk.to_bytes(), b"", [0; 32]).is_err(), "empty password");
        assert!(encrypt_key_bytes(&[0u8; 10], b"x", [0; 32]).is_err());
    }

    #[test]
    fn protect_keys_in_place_and_refuses_twice() {
        let dir = temp_dir("protect");
        let primary = write_unencrypted(&dir, "primary");
        let backup = write_unencrypted(&dir, "backup");
        assert_eq!(keys_to_protect(&dir).unwrap(), vec!["primary", "backup"]);
        let sddl = owner_only_sddl().unwrap();
        let p = pw("a long test password");
        for name in KEY_NAMES {
            protect_key(&dir, name, &p, &sddl).unwrap();
            assert!(!dir.join(format!("{name}.key.protecting")).exists());
        }
        for (name, kp) in [("primary", &primary), ("backup", &backup)] {
            let text = std::fs::read_to_string(dir.join(format!("{name}.key"))).unwrap();
            assert_eq!(kdf_alg(&text).unwrap(), KDF_SCRYPT);
            assert!(text.starts_with("untrusted comment: test key"), "comment kept");
            let sk = open_secret_key(&text, true, || Ok(pw("a long test password"))).unwrap();
            assert!(sk == kp.sk);
            assert!(open_secret_key(&text, true, || Ok(pw("not it"))).is_err());
        }
        assert!(keys_to_protect(&dir).is_err(), "nothing left to protect");
        // A key whose .pub does not match is not replaced.
        let dir2 = temp_dir("protect-mismatch");
        write_unencrypted(&dir2, "primary");
        let other = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        std::fs::write(dir2.join("primary.pub"), other.pk.to_box().unwrap().into_string()).unwrap();
        assert!(protect_key(&dir2, "primary", &p, &sddl).is_err());
        let text = std::fs::read_to_string(dir2.join("primary.key")).unwrap();
        assert_eq!(kdf_alg(&text).unwrap(), KDF_NONE, "original left untouched");
        assert!(!dir2.join("primary.key.protecting").exists());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn real_release_refuses_an_unencrypted_key() {
        let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let text = kp.sk.to_box(None).unwrap().into_string();
        let asked = std::cell::Cell::new(false);
        let refused = open_secret_key(&text, true, || {
            asked.set(true);
            Ok(pw("x"))
        });
        assert!(refused.is_err() && !asked.get());
        // Dry run (throwaway key): allowed, no password asked.
        let sk = open_secret_key(&text, false, || panic!("no password for an unencrypted key")).unwrap();
        assert!(sk == kp.sk);
    }
}
