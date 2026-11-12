//! The signed release manifest (`release.json` + `release.json.minisig`) and the checks every update
//! must pass before a byte of it is installed:
//!
//! signature (one of the compiled-in keys) → schema → version strictly newer than the running build and
//! than the last installed update → `published_at` not older than the newest manifest already seen
//! (replay) and not in the future → executable size + SHA-256 → 64-bit Windows PE.

use std::cmp::Ordering;
use std::fmt;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::error::{ErrorCode, err};

pub const MAX_MANIFEST: u64 = 64 * 1024;
pub const MAX_SIGNATURE: u64 = 4 * 1024;
/// Upper bound for the executable (the release build is ~10 MB).
pub const MAX_EXE: u64 = 128 << 20;
pub const MAX_NOTES: usize = 8 * 1024;
/// A manifest dated further in the future than this is refused (clock skew allowance).
const FUTURE_SLACK_S: u64 = 2 * 86_400;

/// `MAJOR.MINOR.PATCH`, digits only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    pub fn parse(s: &str) -> Option<Version> {
        let mut it = s.split('.');
        let mut part = || -> Option<u32> {
            let p = it.next()?;
            if p.is_empty() || p.len() > 9 || !p.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            p.parse().ok()
        };
        let v = Version(part()?, part()?, part()?);
        it.next().is_none().then_some(v)
    }

    /// The version of this build.
    pub fn current() -> Version {
        Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Version(0, 0, 0))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

/// `release.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    /// Always `"fanrgb"`: a signature made for another product with the same key is not a FanRGB release.
    pub app: String,
    pub version: String,
    /// Installed versions below this cannot update directly (e.g. a settings migration needs a step).
    #[serde(default)]
    pub min_version: Option<String>,
    /// Unix seconds.
    pub published_at: u64,
    #[serde(default)]
    pub notes: String,
    pub asset: Asset,
}

/// A manifest that passed [`parse`]: the parsed versions next to the raw fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub manifest: Manifest,
    pub version: Version,
    pub min_version: Option<Version>,
}

fn package(detail: impl Into<String>) -> anyhow::Error {
    err(ErrorCode::UpdatePackage, detail)
}

fn safe_asset_name(n: &str) -> bool {
    (5..=64).contains(&n.len())
        && n.to_ascii_lowercase().ends_with(".exe")
        && n.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        && !n.starts_with('.')
}

/// Parses and validates the (already signature-checked) manifest bytes.
pub fn parse(bytes: &[u8]) -> Result<Release> {
    let m: Manifest = serde_json::from_slice(bytes).map_err(|e| package(format!("release.json: {e}")))?;
    if m.schema != 1 {
        return Err(package(format!("unsupported manifest schema {}", m.schema)));
    }
    if m.app != "fanrgb" {
        return Err(package(format!("manifest is for {:?}, not fanrgb", m.app)));
    }
    let version = Version::parse(&m.version).ok_or_else(|| package(format!("bad version {:?}", m.version)))?;
    let min_version = match &m.min_version {
        None => None,
        Some(s) => Some(Version::parse(s).ok_or_else(|| package(format!("bad min_version {s:?}")))?),
    };
    if m.notes.len() > MAX_NOTES {
        return Err(package("release notes too long"));
    }
    if !safe_asset_name(&m.asset.name) {
        return Err(package(format!("bad asset name {:?}", m.asset.name)));
    }
    if m.asset.size == 0 || m.asset.size > MAX_EXE {
        return Err(package(format!("implausible asset size {}", m.asset.size)));
    }
    if m.asset.sha256.len() != 64 || !m.asset.sha256.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(package("asset sha256 must be 64 lower-case hex digits"));
    }
    Ok(Release { manifest: m, version, min_version })
}

/// Checks the minisign signature of `manifest` against the trusted keys. Returns the index of the key
/// that signed it (0 = primary, 1 = backup). Legacy (non-prehashed) signatures are refused.
pub fn verify_signature(manifest: &[u8], signature: &[u8], keys: &[&str]) -> Result<usize> {
    let bad = |d: String| err(ErrorCode::UpdateSignature, d);
    let text = std::str::from_utf8(signature).map_err(|_| bad("signature file is not text".into()))?;
    let sig = minisign_verify::Signature::decode(text).map_err(|e| bad(format!("signature: {e}")))?;
    let mut last = None;
    for (i, k) in keys.iter().enumerate() {
        let pk = minisign_verify::PublicKey::from_base64(k).map_err(|e| bad(format!("public key {i}: {e}")))?;
        match pk.verify(manifest, &sig, false) {
            // Domain separation: a driver-catalog signature is never a release signature, even if
            // a key were ever used for both (the keys are separate: `drivers::catalog::keys`).
            Ok(()) if sig.trusted_comment().starts_with("fanrgb-catalog") => {
                return Err(bad("this is a driver-catalog signature, not a release signature".into()));
            }
            Ok(()) => return Ok(i),
            // Not this key: try the next one.
            Err(minisign_verify::Error::UnexpectedKeyId) => {}
            Err(e) => last = Some(e.to_string()),
        }
    }
    Err(bad(last.unwrap_or_else(|| "signed with a key this build does not trust".into())))
}

/// Downgrade / replay state kept by the elevated side (see `install::UpdateRecord`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Seen {
    /// Highest version installed by an update so far.
    pub last_installed: Option<Version>,
    /// Newest `published_at` of any manifest that passed verification.
    pub last_published: u64,
}

/// Is `r` an acceptable update for a build at `current`?
pub fn check_offer(r: &Release, current: Version, seen: Seen, now: u64) -> Result<()> {
    if r.version.cmp(&current) != Ordering::Greater {
        return Err(err(ErrorCode::UpdateDowngrade, format!("{} is not newer than the running {current}", r.version)));
    }
    if let Some(last) = seen.last_installed
        && r.version <= last
    {
        return Err(err(
            ErrorCode::UpdateDowngrade,
            format!("{} is not newer than the last installed update {last}", r.version),
        ));
    }
    if r.manifest.published_at < seen.last_published {
        return Err(err(
            ErrorCode::UpdateDowngrade,
            format!(
                "manifest published at {} is older than one already seen ({}): replayed release",
                r.manifest.published_at, seen.last_published
            ),
        ));
    }
    if r.manifest.published_at > now + FUTURE_SLACK_S {
        return Err(err(
            ErrorCode::UpdatePackage,
            format!("manifest is dated in the future ({})", r.manifest.published_at),
        ));
    }
    if let Some(min) = r.min_version
        && current < min
    {
        return Err(err(
            ErrorCode::UpdatePackage,
            format!("{} needs at least {min} installed; download it manually", r.version),
        ));
    }
    Ok(())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Size, SHA-256 and "is a 64-bit Windows program" checks of the downloaded executable.
pub fn verify_package(r: &Release, exe: &[u8]) -> Result<()> {
    if exe.len() as u64 != r.manifest.asset.size {
        return Err(err(
            ErrorCode::UpdateHash,
            format!("size {} differs from the manifest's {}", exe.len(), r.manifest.asset.size),
        ));
    }
    let got = sha256_hex(exe);
    if got != r.manifest.asset.sha256 {
        return Err(err(ErrorCode::UpdateHash, format!("SHA-256 {got} differs from the manifest")));
    }
    pe_x64_gui(exe).map_err(|why| err(ErrorCode::UpdatePackage, format!("not a 64-bit Windows program: {why}")))
}

/// MZ header → PE signature → AMD64 machine → PE32+ optional header → Windows GUI subsystem.
pub fn pe_x64_gui(b: &[u8]) -> Result<(), String> {
    let rd16 = |o: usize| b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]));
    let rd32 = |o: usize| b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]));
    if b.get(..2) != Some(b"MZ") {
        return Err("no MZ header".into());
    }
    let pe = rd32(0x3c).ok_or("truncated")? as usize;
    if pe > 4096 || b.get(pe..pe + 4) != Some(b"PE\0\0") {
        return Err("no PE header".into());
    }
    if rd16(pe + 4) != Some(0x8664) {
        return Err(format!("machine {:#06x} is not x64", rd16(pe + 4).unwrap_or(0)));
    }
    let opt = pe + 24;
    if rd16(opt) != Some(0x20b) {
        return Err("not PE32+".into());
    }
    // Subsystem at offset 68 of the PE32+ optional header: 2 = Windows GUI (the release build), 3 = console.
    match rd16(opt + 68) {
        Some(2) => Ok(()),
        s => Err(format!("subsystem {s:?} is not Windows GUI")),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::error::code_of;

    #[test]
    fn versions_are_strict() {
        assert_eq!(Version::parse("0.1.0"), Some(Version(0, 1, 0)));
        assert_eq!(Version::parse("10.20.30"), Some(Version(10, 20, 30)));
        for bad in ["", "1", "1.2", "1.2.3.4", "1.2.x", "v1.2.3", "1.2.3-rc1", "1..3", " 1.2.3", "+1.2.3", "1.2.3 "] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
        assert!(Version(0, 10, 0) > Version(0, 9, 99));
        assert!(Version(1, 0, 0) > Version(0, 99, 99));
        assert_eq!(Version(1, 2, 3).to_string(), "1.2.3");
        assert!(Version::current() >= Version(0, 1, 0));
    }

    pub(crate) fn manifest_json(version: &str, published_at: u64, exe: &[u8]) -> Vec<u8> {
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema": 1, "app": "fanrgb", "version": version, "min_version": null,
            "published_at": published_at, "notes": "Fixes.",
            "asset": { "name": "fanrgb.exe", "size": exe.len(), "sha256": sha256_hex(exe) },
        }))
        .unwrap()
    }

    /// A minimal x64 GUI PE header followed by padding (enough for [`pe_x64_gui`]).
    pub(crate) fn fake_exe(fill: u8, len: usize) -> Vec<u8> {
        let mut b = vec![fill; len.max(512)];
        b[..2].copy_from_slice(b"MZ");
        b[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        b[0x80..0x84].copy_from_slice(b"PE\0\0");
        b[0x84..0x86].copy_from_slice(&0x8664u16.to_le_bytes());
        b[0x98..0x9a].copy_from_slice(&0x20bu16.to_le_bytes());
        b[0x98 + 68..0x98 + 70].copy_from_slice(&2u16.to_le_bytes());
        b
    }

    #[test]
    fn manifest_validation() {
        let exe = fake_exe(1, 4096);
        let r = parse(&manifest_json("0.2.0", 1_760_000_000, &exe)).unwrap();
        assert_eq!(r.version, Version(0, 2, 0));
        verify_package(&r, &exe).unwrap();
        let mut v: serde_json::Value = serde_json::from_slice(&manifest_json("0.2.0", 1, &exe)).unwrap();
        for (path, val) in [
            ("schema", serde_json::json!(2)),
            ("app", serde_json::json!("other")),
            ("version", serde_json::json!("0.2")),
            ("min_version", serde_json::json!("x")),
        ] {
            let mut w = v.clone();
            w[path] = val;
            assert_eq!(
                code_of(&parse(&serde_json::to_vec(&w).unwrap()).unwrap_err()),
                ErrorCode::UpdatePackage,
                "{path}"
            );
        }
        for name in ["..\\fanrgb.exe", "fanrgb.dll", "a/b.exe", ".exe", "fan rgb.exe"] {
            v["asset"]["name"] = serde_json::json!(name);
            assert!(parse(&serde_json::to_vec(&v).unwrap()).is_err(), "{name}");
        }
        assert!(parse(b"not json").is_err());
    }

    #[test]
    fn package_checks() {
        let exe = fake_exe(7, 4096);
        let r = parse(&manifest_json("0.2.0", 1, &exe)).unwrap();
        let mut other = exe.clone();
        other[2000] ^= 1;
        assert_eq!(code_of(&verify_package(&r, &other).unwrap_err()), ErrorCode::UpdateHash);
        assert_eq!(code_of(&verify_package(&r, &exe[..4000]).unwrap_err()), ErrorCode::UpdateHash);
        // Right hash, wrong architecture.
        let mut arm = exe.clone();
        arm[0x84..0x86].copy_from_slice(&0xaa64u16.to_le_bytes());
        let r = parse(&manifest_json("0.2.0", 1, &arm)).unwrap();
        assert_eq!(code_of(&verify_package(&r, &arm).unwrap_err()), ErrorCode::UpdatePackage);
        assert!(pe_x64_gui(b"MZ").is_err());
        // The test binary itself is an x64 PE: GUI subsystem in release builds (like the app), console
        // in debug builds.
        let me = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        match pe_x64_gui(&me) {
            Ok(()) => assert!(!cfg!(debug_assertions)),
            Err(e) => assert!(e.contains("subsystem") && cfg!(debug_assertions), "{e}"),
        }
    }

    #[test]
    fn offers_must_be_newer_and_not_replayed() {
        let exe = fake_exe(1, 1024);
        let r = parse(&manifest_json("0.3.0", 1_000_000, &exe)).unwrap();
        let now = 1_000_100;
        let cur = Version(0, 2, 0);
        check_offer(&r, cur, Seen::default(), now).unwrap();
        let code = |res: Result<()>| code_of(&res.unwrap_err());
        assert_eq!(code(check_offer(&r, Version(0, 3, 0), Seen::default(), now)), ErrorCode::UpdateDowngrade);
        assert_eq!(code(check_offer(&r, Version(0, 4, 0), Seen::default(), now)), ErrorCode::UpdateDowngrade);
        let seen = Seen { last_installed: Some(Version(0, 3, 0)), last_published: 0 };
        assert_eq!(code(check_offer(&r, cur, seen, now)), ErrorCode::UpdateDowngrade);
        let seen = Seen { last_installed: None, last_published: 1_000_001 };
        assert_eq!(code(check_offer(&r, cur, seen, now)), ErrorCode::UpdateDowngrade);
        // The same manifest seen again is fine.
        check_offer(&r, cur, Seen { last_installed: None, last_published: 1_000_000 }, now).unwrap();
        // Dated far in the future.
        assert_eq!(code(check_offer(&r, cur, Seen::default(), 1_000_000 - 3 * 86_400)), ErrorCode::UpdatePackage);
        // min_version above the running build.
        let mut m = r.clone();
        m.min_version = Some(Version(0, 2, 5));
        assert_eq!(code(check_offer(&m, cur, Seen::default(), now)), ErrorCode::UpdatePackage);
    }
}
