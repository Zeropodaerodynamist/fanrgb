//! Driver-catalog tests: format and field validation, signatures (tampered, wrong key, backup key,
//! domain separation from release signatures), serial replay, expiry, the URL allow-list, size caps,
//! text sanitisation, matching against this PC's inventory, and the helper → elevated path end to end
//! against a mock GitHub (like `updater::tests`). Signatures use throwaway minisign keys.

use std::cell::RefCell;
use std::path::PathBuf;

use serde_json::{Value, json};

use super::store::{self, Outcome};
use super::*;
use crate::drivers::{DriverCategory, DriverRow, inventory};
use crate::updater::catalog_fetch;
use crate::updater::http::{Http, Response};
use crate::util::fsx;

/// 2026-10-19 12:00 UTC.
pub(crate) const NOW: u64 = 1_792_411_200;
const TOKEN: &str = "github_pat_11TESTTOKEN0000000000_abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQ";
const REPO: u64 = 1_455_226_413;

pub(crate) struct Key {
    pub public: String,
    secret: minisign::SecretKey,
    pk: minisign::PublicKey,
}

pub(crate) fn key() -> Key {
    let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    Key { public: kp.pk.to_base64(), secret: kp.sk, pk: kp.pk }
}

fn sign_with_comment(k: &Key, data: &[u8], comment: &str) -> Vec<u8> {
    minisign::sign(Some(&k.pk), &k.secret, data, Some(comment), None).unwrap().into_string().into_bytes()
}

/// What `fanrgb-release catalog-sign` writes.
pub(crate) fn sign(k: &Key, data: &[u8]) -> Vec<u8> {
    let serial: Value = serde_json::from_slice(data).unwrap();
    sign_with_comment(k, data, &format!("fanrgb-catalog serial={} drivers.json", serial["serial"]))
}

fn ts(t: u64) -> String {
    format_timestamp(t)
}

fn entry(id: &str, version: &str, scheme: &str, m: Value, url: &str) -> Value {
    json!({
        "id": id, "vendor": "Vendor", "component": format!("{id} driver"), "match": m,
        "version": version, "version_scheme": scheme, "release_date": "2026-10-02",
        "download_url": url, "release_notes_url": null, "source": "auto:test", "checked_at": ts(NOW - 7200),
    })
}

/// A catalog for this PC (B650M D3HP AX rev 1.3, Arc B580, Realtek 2.5GbE).
pub(crate) fn sample(serial: u64) -> Value {
    json!({
        "schema_version": "1.0", "kind": KIND, "serial": serial,
        "generated_at": ts(NOW - 3600), "expires": ts(NOW + 21 * 86_400),
        "entries": [
            {
                "id": "intel-arc-graphics", "vendor": "Intel", "component": "Intel Arc Graphics driver",
                "match": { "categories": ["gpu"], "hardware_ids": ["PCI\\VEN_8086&DEV_E2*", "PCI\\VEN_8086&DEV_56*"] },
                "version": "32.0.101.9150", "version_scheme": "intel-gfx", "release_date": "2026-10-02",
                "download_url": crate::drivers::URL_INTEL_ARC,
                "release_notes_url": "https://downloadmirror.intel.com/930514/ReleaseNotes__101.9150_WHQL.pdf",
                "source": "auto:intel-arc", "checked_at": ts(NOW - 7200),
            },
            entry("amd-chipset-am5", "8.08.12.551", "amd-chipset",
                json!({ "categories": ["chipset"], "packages": ["AMD Chipset Software"] }), crate::drivers::URL_AMD_B650),
            entry("realtek-lan-netadaptercx", "11.031.50", "realtek-lan",
                json!({ "categories": ["lan"], "hardware_ids": ["PCI\\VEN_10EC&DEV_8125*"] }), crate::drivers::URL_REALTEK_LAN),
            entry("realtek-lan-ndis", "10.80.50", "realtek-lan",
                json!({ "categories": ["lan"], "hardware_ids": ["PCI\\VEN_10EC&DEV_8125*"] }), crate::drivers::URL_REALTEK_LAN),
            entry("gigabyte-b650m-d3hp-ax-r13-bios", "F36d", "gigabyte-bios",
                json!({ "categories": ["bios"], "board": { "vendor": "GIGABYTE", "product": "B650M D3HP AX", "revisions": ["1.3"] } }),
                crate::drivers::URL_GIGABYTE_REV13_BIOS),
            entry("gigabyte-b650m-d3hp-ax-r10-bios", "F40", "gigabyte-bios",
                json!({ "categories": ["bios"], "board": { "vendor": "GIGABYTE", "product": "B650M-D3HP-AX", "revisions": ["1.0", "1.2"] } }),
                crate::drivers::URL_GIGABYTE_REV10_BIOS),
        ]
    })
}

fn bytes(v: &Value) -> Vec<u8> {
    serde_json::to_vec_pretty(v).unwrap()
}

fn code(r: Result<impl std::fmt::Debug, CheckError>) -> ErrorCode {
    r.unwrap_err().code
}

fn this_pc() -> Vec<DriverRow> {
    let (d, b, p) = inventory::tests::this_pc();
    inventory::classify(&d, Some(&b), &p)
}

// ---------------------------------------------------------------------------------------
// Format

#[test]
fn sample_parses_with_every_field() {
    let c = parse(&bytes(&sample(4)), NOW).unwrap();
    assert_eq!(c.serial, 4);
    assert_eq!(c.generated_at, NOW - 3600);
    assert_eq!(c.entries.len(), 6);
    let intel = &c.entries[0];
    assert_eq!(
        (intel.vendor.as_str(), intel.version.as_str(), intel.scheme),
        ("Intel", "32.0.101.9150", Scheme::IntelGfx)
    );
    assert_eq!(intel.matcher.categories, vec![DriverCategory::Gpu]);
    assert_eq!(
        intel.release_notes_url.as_deref(),
        Some("https://downloadmirror.intel.com/930514/ReleaseNotes__101.9150_WHQL.pdf")
    );
    assert_eq!(intel.checked_at, NOW - 7200);
    assert!(!c.expired(NOW) && c.expired(NOW + 22 * 86_400));
}

#[test]
fn timestamps_and_dates_are_strict() {
    assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(parse_timestamp(&format_timestamp(NOW)), Some(NOW));
    assert_eq!(format_timestamp(NOW), "2026-10-19T12:00:00Z");
    for bad in [
        "2026-10-19 12:00:00Z",
        "2026-10-19T12:00:00",
        "2026-10-19T12:00:00+00:00",
        "2026-13-01T00:00:00Z",
        "2026-02-30T00:00:00Z",
        "2026-10-19T24:00:00Z",
        "2026-10-19T12:60:00Z",
        "１９７０-01-01T00:00:00Z",
    ] {
        assert_eq!(parse_timestamp(bad), None, "{bad}");
    }
    assert!(is_date("2024-02-29") && !is_date("2026-02-29") && !is_date("2026-1-01") && !is_date("26-10-19"));
}

fn with(f: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut v = sample(4);
    f(&mut v);
    bytes(&v)
}

#[test]
fn schema_versions_and_kind() {
    // A newer minor may add fields: accepted, unknown fields ignored.
    let b = with(|v| {
        v["schema_version"] = json!("1.7");
        v["new_top_level"] = json!({"x": 1});
        v["entries"][0]["new_field"] = json!([1, 2]);
    });
    assert!(parse(&b, NOW).is_ok());
    for sv in ["2.0", "0.9", "1", "1.x", "v1.0", "1.0.0", ""] {
        assert_eq!(code(parse(&with(|v| v["schema_version"] = json!(sv)), NOW)), ErrorCode::CatalogRejected, "{sv}");
    }
    assert!(parse(&with(|v| v["kind"] = json!("fanrgb-release")), NOW).is_err());
    assert!(parse(&with(|v| v["serial"] = json!(0)), NOW).is_err());
    assert!(parse(&with(|v| v["serial"] = json!(-1)), NOW).is_err());
    assert!(parse(b"not json", NOW).is_err());
    assert!(parse(&with(|v| v.as_object_mut().unwrap().remove("expires").map(drop).unwrap()), NOW).is_err());
}

#[test]
fn dates_bound_the_catalog() {
    // Dated in the future (beyond clock skew), expiry before generation, too long a validity.
    assert!(parse(&with(|v| v["generated_at"] = json!(ts(NOW + 3 * 86_400))), NOW).is_err());
    assert!(parse(&with(|v| v["generated_at"] = json!(ts(NOW + 3600))), NOW).is_ok(), "an hour of skew is fine");
    assert!(parse(&with(|v| v["expires"] = json!(ts(NOW - 7200))), NOW).is_err());
    assert!(parse(&with(|v| v["expires"] = json!(ts(NOW + 120 * 86_400))), NOW).is_err());
    // An entry checked after the catalog was generated is a CI bug.
    assert!(parse(&with(|v| v["entries"][0]["checked_at"] = json!(ts(NOW))), NOW).is_err());
    assert!(parse(&with(|v| v["entries"][0]["release_date"] = json!("2026-10-32")), NOW).is_err());
    // An expired catalog still parses (it is shown, marked out of date).
    let c = parse(&bytes(&sample(4)), NOW + 30 * 86_400).unwrap();
    assert!(c.expired(NOW + 30 * 86_400));
}

#[test]
fn entries_are_validated() {
    type Change = Box<dyn Fn(&mut Value)>;
    let bad: Vec<(&str, Change)> = vec![
        ("duplicate id", Box::new(|v| v["entries"][1]["id"] = json!("intel-arc-graphics"))),
        ("id charset", Box::new(|v| v["entries"][0]["id"] = json!("Intel Arc"))),
        ("version shape", Box::new(|v| v["entries"][0]["version"] = json!("32.0.101.9150 WHQL"))),
        ("unknown scheme", Box::new(|v| v["entries"][0]["version_scheme"] = json!("semver"))),
        ("source", Box::new(|v| v["entries"][0]["source"] = json!("scraped"))),
        ("source adapter", Box::new(|v| v["entries"][0]["source"] = json!("auto:Intel Arc"))),
        ("no key rule", Box::new(|v| v["entries"][0]["match"] = json!({ "categories": ["gpu"] }))),
        ("match-all pattern", Box::new(|v| v["entries"][0]["match"]["hardware_ids"] = json!(["*"]))),
        ("short literal", Box::new(|v| v["entries"][0]["match"]["hardware_ids"] = json!(["PCI\\*"]))),
        ("pattern charset", Box::new(|v| v["entries"][0]["match"]["hardware_ids"] = json!(["PCI\\VEN_8086 DEV"]))),
        ("unknown class", Box::new(|v| v["entries"][0]["match"]["categories"] = json!(["printer"]))),
        ("revision", Box::new(|v| v["entries"][4]["match"]["board"]["revisions"] = json!(["rev 1.3"]))),
        (
            "too many patterns",
            Box::new(|v| v["entries"][0]["match"]["hardware_ids"] = json!(vec!["PCI\\VEN_8086&DEV_E20B"; 65])),
        ),
    ];
    for (what, f) in bad {
        assert_eq!(code(parse(&with(f), NOW)), ErrorCode::CatalogRejected, "{what}");
    }
    let many: Vec<Value> = (0..201)
        .map(|i| entry(&format!("e{i}"), "1.0", "numeric", json!({"packages": ["P"]}), crate::drivers::URL_PAWNIO))
        .collect();
    assert!(parse(&with(|v| v["entries"] = json!(many)), NOW).is_err(), "more than 200 entries");
}

/// Review L26 rules for signed text: refused, not cleaned, so a broken or hostile source is noticed.
#[test]
fn control_and_invisible_characters_are_refused() {
    for evil in [
        "Intel\u{202E}lacitirc",
        "Intel\u{0007}",
        "Intel\nCorp",
        "Intel\tCorp",
        "In\u{200B}tel",
        "\u{FEFF}Intel",
        "Intel\u{2066}x",
        "   ",
    ] {
        assert!(parse(&with(|v| v["entries"][0]["vendor"] = json!(evil)), NOW).is_err(), "{evil:?}");
        assert!(parse(&with(|v| v["entries"][1]["match"]["packages"] = json!([evil])), NOW).is_err(), "{evil:?}");
    }
    assert!(parse(&with(|v| v["entries"][0]["component"] = json!("x".repeat(121))), NOW).is_err(), "oversized text");
    assert!(parse(&with(|v| v["entries"][0]["component"] = json!("Intel® Arc™ Graphics – Windows*")), NOW).is_ok());
    // serde's error text quoting catalog content is cleaned before display.
    let e =
        parse(b"{\"schema_version\":\"1.0\",\"kind\":\"fanrgb-driver-catalog\",\"a\xe2\x80\xaeb\":1", NOW).unwrap_err();
    assert!(!e.detail.contains('\u{202E}'));
}

#[test]
fn size_caps() {
    let mut v = sample(4);
    v["padding"] = json!("x".repeat(MAX_CATALOG_BYTES as usize));
    let big = bytes(&v);
    assert!(big.len() as u64 > MAX_CATALOG_BYTES);
    assert_eq!(code(parse(&big, NOW)), ErrorCode::CatalogRejected);
    let k = key();
    assert_eq!(code(verify(&big, &sign(&k, &big), &[&k.public], NOW)), ErrorCode::CatalogRejected);
    let b = bytes(&sample(4));
    let mut long_sig = sign(&k, &b);
    long_sig.extend(std::iter::repeat_n(b'\n', MAX_SIGNATURE_BYTES as usize));
    assert!(verify(&b, &long_sig, &[&k.public], NOW).is_err());
}

#[test]
fn url_allow_list() {
    for ok in [
        crate::drivers::URL_INTEL_ARC,
        crate::drivers::URL_AMD_B650,
        crate::drivers::URL_REALTEK_LAN,
        crate::drivers::URL_GIGABYTE_REV13_BIOS,
        "https://downloadmirror.intel.com/930514/ReleaseNotes__101.9150_WHQL.pdf",
        "https://pawnio.eu/",
    ] {
        assert!(url_allowed(ok), "{ok}");
    }
    for bad in [
        "http://www.intel.com/content/www/us/en/download/785597/x.html",
        "https://intel.com/x",
        "https://www.intel.com.evil.example/x",
        "https://evil.example/www.intel.com/",
        "https://www.intel.com@evil.example/x",
        "https://WWW.INTEL.COM/x",
        "https://www.intel.com:443/x",
        "https://www.intel.com:8443/x",
        "https://www.intel.com/a b",
        "https://www.intel.com/a\"b",
        "https://www.intel.com/%2e%2e/x",
        "https://www.intel.com/x'onload",
        "https://www.intel.com/<script>",
        "javascript:alert(1)",
        "file:///C:/Windows/System32/calc.exe",
        "ms-settings:personalization-lighting",
        "",
    ] {
        assert!(!url_allowed(bad), "{bad}");
    }
    assert!(!url_allowed(&format!("https://www.intel.com/{}", "a".repeat(300))), "too long");
    assert!(parse(&with(|v| v["entries"][0]["download_url"] = json!("https://evil.example/driver.exe")), NOW).is_err());
    assert!(
        parse(&with(|v| v["entries"][0]["release_notes_url"] = json!("http://downloadmirror.intel.com/x.pdf")), NOW)
            .is_err()
    );
}

/// The CI allow-list (`catalog/allowed-hosts.json`) is the app's list, and the pinned vendor pages are
/// on it.
#[test]
fn allowed_hosts_match_ci() {
    let ci: Value = serde_json::from_str(include_str!("../../../catalog/allowed-hosts.json")).unwrap();
    let ci: Vec<&str> = ci["hosts"].as_array().unwrap().iter().map(|h| h.as_str().unwrap()).collect();
    assert_eq!(ci, crate::util::win::VENDOR_HOSTS);
    for u in crate::drivers::VENDOR_URLS {
        assert!(url_allowed(u), "{u}");
    }
}

/// `catalog/schema.json` documents the same schemes and classes the code accepts, and the hand-written
/// entries in `catalog/manual.json` pass the app's own validation.
#[test]
fn schema_and_manual_entries_agree_with_the_code() {
    let schema: Value = serde_json::from_str(include_str!("../../../catalog/schema.json")).unwrap();
    let schemes = &schema["$defs"]["entry"]["properties"]["version_scheme"]["enum"];
    for s in schemes.as_array().unwrap() {
        assert!(serde_json::from_value::<Scheme>(s.clone()).is_ok(), "{s}");
    }
    assert_eq!(schemes.as_array().unwrap().len(), 5);
    for c in schema["$defs"]["match"]["properties"]["categories"]["items"]["enum"].as_array().unwrap() {
        assert!(category_from_id(c.as_str().unwrap()).is_some(), "{c}");
    }
    let manual: Value = serde_json::from_str(include_str!("../../../catalog/manual.json")).unwrap();
    let mut cat = sample(1);
    cat["entries"] = manual["entries"].clone();
    cat["generated_at"] = json!(ts(NOW));
    let c = parse(&bytes(&cat), NOW).unwrap();
    assert!(c.entries.iter().all(|e| e.source == "manual"));
}

// ---------------------------------------------------------------------------------------
// Signatures

#[test]
fn signature_accept_and_reject() {
    let (primary, backup, other) = (key(), key(), key());
    let keys = [primary.public.as_str(), backup.public.as_str()];
    let b = bytes(&sample(9));
    let v = verify(&b, &sign(&primary, &b), &keys, NOW).unwrap();
    assert_eq!((v.key, v.catalog.serial), (0, 9));
    assert_eq!(v.sha256, crate::updater::manifest::sha256_hex(&b));
    // The backup key is accepted (rotation).
    assert_eq!(verify(&b, &sign(&backup, &b), &keys, NOW).unwrap().key, 1);
    // Unknown key.
    assert_eq!(code(verify(&b, &sign(&other, &b), &keys, NOW)), ErrorCode::CatalogRejected);
    // One byte changed after signing (a version raised by someone holding only the token).
    let sig = sign(&primary, &b);
    let mut t = b.clone();
    let at = t.windows(4).position(|w| w == b"9150").unwrap();
    t[at] = b'9';
    t[at + 1] = b'9';
    assert_eq!(code(verify(&t, &sig, &keys, NOW)), ErrorCode::CatalogRejected);
    // Corrupted or garbage signature, empty signature.
    let mut s2 = sig.clone();
    let n = s2.len();
    s2[n / 2] ^= 0x01;
    assert!(verify(&b, &s2, &keys, NOW).is_err());
    assert!(verify(&b, b"untrusted comment: x\nRWQ=\n", &keys, NOW).is_err());
    assert!(verify(&b, b"", &keys, NOW).is_err());
    assert!(verify(&b, &[0xff, 0xfe], &keys, NOW).is_err());
}

#[test]
fn trusted_comment_binds_the_serial() {
    let k = key();
    let b = bytes(&sample(9));
    // Signed as serial 8: the serial in the file was changed, or the comment lies.
    let s = sign_with_comment(&k, &b, "fanrgb-catalog serial=8 drivers.json");
    assert!(code(verify(&b, &s, &[&k.public], NOW)) == ErrorCode::CatalogRejected);
    for c in ["fanrgb-catalog serial= drivers.json", "fanrgb-catalog serial=x9", "fanrgb-catalog", "catalog serial=9"] {
        assert!(verify(&b, &sign_with_comment(&k, &b, c), &[&k.public], NOW).is_err(), "{c}");
    }
}

/// Key and domain separation: a catalog key cannot sign an app release and a release key cannot sign
/// a catalog, also with one key used for both, and the compiled-in key sets do not overlap.
#[test]
fn catalog_and_release_signatures_never_cross() {
    use crate::updater::manifest;
    let (catalog_key, release_key) = (key(), key());
    let exe = manifest::tests::fake_exe(1, 2048);
    let release = manifest::tests::manifest_json("0.3.0", NOW, &exe);
    let release_sig = sign_with_comment(&release_key, &release, "fanrgb 0.3.0 release.json");
    let cat = bytes(&sample(3));
    let cat_sig = sign(&catalog_key, &cat);
    // Separate keys.
    assert!(manifest::verify_signature(&release, &release_sig, &[&release_key.public]).is_ok());
    assert!(manifest::verify_signature(&release, &release_sig, &[&catalog_key.public]).is_err());
    assert!(verify_signature(&release, &release_sig, &[&catalog_key.public]).is_err());
    assert!(manifest::verify_signature(&cat, &cat_sig, &[&release_key.public]).is_err());
    // Same key for both (never done, but even then): the trusted comments keep the domains apart.
    let shared = key();
    assert!(
        verify_signature(
            &release,
            &sign_with_comment(&shared, &release, "fanrgb 0.3.0 release.json"),
            &[&shared.public]
        )
        .is_err()
    );
    let e = manifest::verify_signature(&cat, &sign(&shared, &cat), &[&shared.public]).unwrap_err();
    assert_eq!(crate::error::code_of(&e), ErrorCode::UpdateSignature);
    // The compiled-in keys.
    for k in keys::TRUSTED {
        assert!(!crate::updater::keys::TRUSTED.contains(&k), "a catalog key is also a release key");
        assert!(minisign_verify::PublicKey::from_base64(k).is_ok(), "{k}");
    }
    assert_ne!(keys::PRIMARY, keys::BACKUP);
    // The compiled-in catalog keys do not verify a manifest signed for a release, and the reverse.
    assert!(verify_signature(&release, &release_sig, &keys::TRUSTED).is_err());
    assert!(manifest::verify_signature(&cat, &cat_sig, &crate::updater::keys::TRUSTED).is_err());
}

#[test]
fn key_ids_as_minisign_prints_them() {
    assert_eq!(key_id(keys::PRIMARY), "E9DA5FDE72C7DE0A");
    assert_eq!(key_id(keys::BACKUP), "07D52C62A5F07483");
    assert_eq!(key_id("x"), "?");
    let k = key();
    let id = k.pk.keynum().iter().rev().map(|b| format!("{b:02X}")).collect::<String>();
    assert_eq!(key_id(&k.public), id);
}

// ---------------------------------------------------------------------------------------
// Replay state and cache

fn store_dir(name: &str) -> PathBuf {
    fsx::tests::test_dir(name)
}

#[test]
fn serial_replay_is_refused() {
    let dir = store_dir("cat-replay");
    let k = key();
    let keys = [k.public.as_str()];
    let b5 = bytes(&sample(5));
    let (v, o) = store::accept(&dir, &b5, &sign(&k, &b5), &keys, NOW).unwrap();
    assert_eq!((v.catalog.serial, o), (5, Outcome::New));
    // The daily re-fetch of the same catalog.
    assert_eq!(store::accept(&dir, &b5, &sign(&k, &b5), &keys, NOW + 86_400).unwrap().1, Outcome::Unchanged);
    // An older signed catalog (replayed by someone who controls the account or the network).
    let b4 = bytes(&sample(4));
    let e = store::accept(&dir, &b4, &sign(&k, &b4), &keys, NOW).unwrap_err();
    assert!(e.code == ErrorCode::CatalogRejected && e.detail.contains("replayed"), "{e}");
    // The same serial with other content (two catalogs under one serial).
    let mut other = sample(5);
    other["entries"][0]["version"] = json!("32.0.101.9999");
    let ob = bytes(&other);
    let e = store::accept(&dir, &ob, &sign(&k, &ob), &keys, NOW).unwrap_err();
    assert!(e.detail.contains("already used"), "{e}");
    // Refusals change nothing: the cache still holds serial 5.
    assert_eq!(store::load_cached(&dir, &keys, NOW).unwrap().catalog.serial, 5);
    assert_eq!(store::load_record(&dir).last_serial, 5);
    // A newer one replaces it.
    let b6 = bytes(&sample(6));
    assert_eq!(store::accept(&dir, &b6, &sign(&k, &b6), &keys, NOW).unwrap().1, Outcome::New);
    assert_eq!(store::load_cached(&dir, &keys, NOW).unwrap().catalog.serial, 6);
}

#[test]
fn cache_is_reverified_on_load() {
    let dir = store_dir("cat-cache");
    let (k, other) = (key(), key());
    let keys = [k.public.as_str()];
    let b = bytes(&sample(2));
    store::accept(&dir, &b, &sign(&k, &b), &keys, NOW).unwrap();
    assert!(store::load_cached(&dir, &keys, NOW).is_some());
    // A key that is no longer trusted (rotated out), an edited cache, a cache that does not match the
    // recorded serial: not used.
    assert!(store::load_cached(&dir, &[&other.public], NOW).is_none());
    let mut edited = b.clone();
    edited[10] ^= 1;
    std::fs::write(dir.join(store::CACHE_FILE), &edited).unwrap();
    assert!(store::load_cached(&dir, &keys, NOW).is_none());
    let b1 = bytes(&sample(1));
    std::fs::write(dir.join(store::CACHE_FILE), &b1).unwrap();
    std::fs::write(dir.join(store::CACHE_SIG_FILE), sign(&k, &b1)).unwrap();
    assert!(store::load_cached(&dir, &keys, NOW).is_none(), "an older cached catalog than recorded");
    assert!(store::load_cached(&store_dir("cat-empty"), &keys, NOW).is_none());
}

#[test]
fn expired_catalog_is_kept_and_marked() {
    let dir = store_dir("cat-expired");
    let k = key();
    let b = bytes(&sample(3));
    store::accept(&dir, &b, &sign(&k, &b), &[&k.public], NOW).unwrap();
    let later = NOW + 40 * 86_400;
    let rt = runtime::Runtime::new(dir, None, vec![k.public.clone()], false, later);
    let info = &rt.info;
    assert!(rt.catalog.is_some(), "an expired list is still shown");
    assert!(info.expired(later) && !info.expired(NOW));
    assert_eq!(info.loaded.as_ref().unwrap().serial, 3);
}

// ---------------------------------------------------------------------------------------
// Matching

#[test]
fn glob_patterns() {
    assert!(glob_ci("PCI\\VEN_8086&DEV_E2*", "PCI\\VEN_8086&DEV_E20B&SUBSYS_4215172F&REV_00"));
    assert!(glob_ci("pci\\ven_8086&dev_e2*", "PCI\\VEN_8086&DEV_E20B"), "case-insensitive");
    assert!(glob_ci("PCI\\VEN_10EC&DEV_8125&SUBSYS_????1458*", "PCI\\VEN_10EC&DEV_8125&SUBSYS_E0001458&REV_05"));
    assert!(glob_ci("ACPI\\AMDI0010", "ACPI\\AMDI0010"));
    assert!(!glob_ci("ACPI\\AMDI0010", "ACPI\\AMDI00101"));
    assert!(!glob_ci("PCI\\VEN_8086&DEV_E2*", "PCI\\VEN_8086&DEV_56A0"));
    assert!(glob_ci("PCI\\VEN_8086*DEV_E20B*", "PCI\\VEN_8086&DEV_E20B&REV_00"));
    assert!(!glob_ci("PCI\\VEN_8086&DEV_E20B?", "PCI\\VEN_8086&DEV_E20B"));
}

#[test]
fn matches_this_pc() {
    let mut rows = this_pc();
    let c = parse(&bytes(&sample(1)), NOW).unwrap();
    apply_catalog(&mut rows, Some(&c));
    let find = |dev: &str| rows.iter().find(|r| r.device == dev).unwrap();
    let gpu = find("Intel(R) Arc(TM) B580 Graphics");
    assert_eq!(gpu.catalog_status, CatalogStatus::Newer);
    assert_eq!(status_sentence(gpu), "Newer driver available: 32.0.101.9150 (Intel, 2026-10-02)");
    assert_eq!(gpu.catalog.as_ref().unwrap().source_line(), "Intel · 2026-10-02");
    let pkg = rows.iter().find(|r| r.package).unwrap();
    assert_eq!(
        (pkg.catalog_status, pkg.catalog.as_ref().unwrap().version.as_str()),
        (CatalogStatus::Newer, "8.08.12.551")
    );
    // LAN: the NetAdapterCx entry compares (1125.29 < 11.031); the NDIS one is of another family.
    let lan = find("Realtek PCIe 2.5GbE Family Controller");
    assert_eq!(lan.catalog_status, CatalogStatus::Newer);
    assert_eq!(lan.catalog.as_ref().unwrap().id, "realtek-lan-netadaptercx");
    // BIOS F36d = the rev 1.3 entry; the rev 1.0/1.2 entry (F40) is for another board revision.
    let bios = rows.iter().find(|r| r.category == DriverCategory::Bios).unwrap();
    assert_eq!(bios.catalog_status, CatalogStatus::LatestKnown);
    assert_eq!(bios.catalog.as_ref().unwrap().id, "gigabyte-b650m-d3hp-ax-r13-bios");
    // Not in the list: Wi-Fi, Bluetooth, audio, PawnIO, the chipset components (folded under the package).
    for dev in
        ["Realtek 8852CE WiFi 6E PCI-E NIC", "Realtek Bluetooth Adapter", "PawnIO", "AMD SMBus", "AMD PSP 11.0 Device"]
    {
        assert_eq!(find(dev).catalog_status, CatalogStatus::NotInList, "{dev}");
        assert!(find(dev).catalog.is_none());
    }
    // Badge: GPU, package, LAN (the BIOS would not count).
    let st = crate::drivers::DriverState { rows: rows.clone(), ..Default::default() };
    assert_eq!(st.updates_available(), 3);
    // No catalog: everything back to NoList.
    apply_catalog(&mut rows, None);
    assert!(rows.iter().all(|r| r.catalog_status == CatalogStatus::NoList && r.catalog.is_none()));
}

#[test]
fn matching_rules() {
    let mut rows = this_pc();
    // A newer BIOS for rev 1.3; the device class must match too (an Intel pattern on a non-GPU class).
    let mut v = sample(1);
    v["entries"][4]["version"] = json!("F37");
    v["entries"][0]["match"]["categories"] = json!(["audio"]);
    // Provider filter: the Realtek LAN entries need provider "Realtek" (substring of "Realtek").
    v["entries"][2]["match"]["providers"] = json!(["realtek"]);
    v["entries"][3]["match"]["providers"] = json!(["Intel"]);
    let c = parse(&bytes(&v), NOW).unwrap();
    apply_catalog(&mut rows, Some(&c));
    let bios = rows.iter().find(|r| r.category == DriverCategory::Bios).unwrap();
    assert_eq!(bios.catalog_status, CatalogStatus::Newer);
    assert_eq!(status_sentence(bios), "Newer BIOS available: F37 (Vendor, 2026-10-02)");
    let gpu = rows.iter().find(|r| r.category == DriverCategory::Gpu).unwrap();
    assert_eq!(gpu.catalog_status, CatalogStatus::NotInList);
    let lan = rows.iter().find(|r| r.category == DriverCategory::Lan).unwrap();
    assert_eq!(lan.catalog.as_ref().unwrap().id, "realtek-lan-netadaptercx");
    // Only an entry of another driver family: shown, but "compare manually".
    let mut v = sample(1);
    v["entries"].as_array_mut().unwrap().remove(2);
    let c = parse(&bytes(&v), NOW).unwrap();
    apply_catalog(&mut rows, Some(&c));
    let lan = rows.iter().find(|r| r.category == DriverCategory::Lan).unwrap();
    assert_eq!(lan.catalog_status, CatalogStatus::Incomparable);
    assert!(status_sentence(lan).contains("compare it with the installed 1125.29.50.202"));
    // Two comparable entries: the newest wins.
    let mut v = sample(1);
    let mut older = v["entries"][0].clone();
    older["id"] = json!("intel-arc-older");
    older["version"] = json!("32.0.101.9000");
    v["entries"].as_array_mut().unwrap().push(older);
    let c = parse(&bytes(&v), NOW).unwrap();
    apply_catalog(&mut rows, Some(&c));
    let gpu = rows.iter().find(|r| r.category == DriverCategory::Gpu).unwrap();
    assert_eq!(gpu.catalog.as_ref().unwrap().version, "32.0.101.9150");
    // Installed newer than the list (a beta driver): latest known, not "newer".
    let mut rows = this_pc();
    rows.iter_mut().find(|r| r.category == DriverCategory::Gpu).unwrap().installed = "32.0.101.9200".into();
    apply_catalog(&mut rows, Some(&parse(&bytes(&sample(1)), NOW).unwrap()));
    assert_eq!(
        rows.iter().find(|r| r.category == DriverCategory::Gpu).unwrap().catalog_status,
        CatalogStatus::LatestKnown
    );
}

#[test]
fn other_boards_never_match_board_entries() {
    let (d, mut b, p) = inventory::tests::this_pc();
    b.board_product = "X670E AORUS MASTER".into();
    let mut rows = inventory::classify(&d, Some(&b), &p);
    apply_catalog(&mut rows, Some(&parse(&bytes(&sample(1)), NOW).unwrap()));
    assert_eq!(
        rows.iter().find(|r| r.category == DriverCategory::Bios).unwrap().catalog_status,
        CatalogStatus::NotInList
    );
    // Rev 1.0/1.2 (MediaTek Wi-Fi) gets the rev 1.0/1.2 entry.
    let (mut d, b, p) = inventory::tests::this_pc();
    d[0].name = "MediaTek Wi-Fi 6E MT7922 (RZ616) 160MHz PCIe Adapter".into();
    d[0].provider = "MediaTek, Inc.".into();
    let mut rows = inventory::classify(&d, Some(&b), &p);
    apply_catalog(&mut rows, Some(&parse(&bytes(&sample(1)), NOW).unwrap()));
    let bios = rows.iter().find(|r| r.category == DriverCategory::Bios).unwrap();
    assert_eq!((bios.catalog_status, bios.catalog.as_ref().unwrap().version.as_str()), (CatalogStatus::Newer, "F40"));
}

/// The first real catalog, built by the CI fetcher from the live vendor pages on 2026-10-19 (Intel and
/// Realtek fetched; AMD from `catalog/manual.json`; GIGABYTE blocked), is accepted by the app and gives
/// this PC the expected statuses.
#[test]
fn first_live_catalog_on_this_pc() {
    let raw = include_bytes!("../../../tools/driver-catalog/tests/fixtures/first-catalog.json");
    let c = parse(raw, parse_timestamp("2026-10-19T18:30:00Z").unwrap()).unwrap();
    assert_eq!((c.serial, c.entries.len()), (1, 4));
    let k = key();
    assert!(verify(raw, &sign(&k, raw), &[&k.public], parse_timestamp("2026-10-20T00:00:00Z").unwrap()).is_ok());
    let mut rows = this_pc();
    apply_catalog(&mut rows, Some(&c));
    let st = |cat: DriverCategory| rows.iter().find(|r| r.category == cat && !r.package).unwrap();
    assert_eq!(status_sentence(st(DriverCategory::Gpu)), "Newer driver available: 32.0.101.9150 (Intel, 2026-10-02)");
    assert_eq!(st(DriverCategory::Lan).catalog.as_ref().unwrap().version, "11.031.50");
    assert_eq!(st(DriverCategory::Lan).catalog_status, CatalogStatus::Newer);
    let pkg = rows.iter().find(|r| r.package).unwrap();
    assert_eq!(status_sentence(pkg), "Newer driver available: 8.08.12.551 (AMD, 2026-08-14)");
    assert_eq!(st(DriverCategory::Bios).catalog_status, CatalogStatus::NotInList);
    assert_eq!(st(DriverCategory::Wifi).catalog_status, CatalogStatus::NotInList);
}

/// Opt-in cross-check of the real pipeline: `fanrgb-release catalog-sign` output verified by the app's
/// verifier. `FANRGB_CATALOG_DRY=<folder with drivers.json, drivers.json.minisig, pub.txt>`; run with
/// `cargo test --release -- --ignored tool_signed_catalog`.
#[test]
#[ignore]
fn tool_signed_catalog_verifies_in_the_app() {
    let dir = PathBuf::from(std::env::var("FANRGB_CATALOG_DRY").expect("FANRGB_CATALOG_DRY"));
    let bytes = std::fs::read(dir.join("drivers.json")).unwrap();
    let sig = std::fs::read(dir.join("drivers.json.minisig")).unwrap();
    let public = std::fs::read_to_string(dir.join("pub.txt")).unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let now = parse_timestamp(v["generated_at"].as_str().unwrap()).unwrap() + 3600;
    let ok = verify(&bytes, &sig, &[keys::PRIMARY, public.trim()], now).unwrap();
    assert_eq!(ok.key, 1);
    assert!(verify(&bytes, &sig, &keys::TRUSTED, now).is_err(), "a throwaway key is not a compiled-in key");
}

#[test]
fn demo_preview_is_a_valid_catalog() {
    let c = demo_catalog(NOW);
    assert_eq!(c.entries.len(), 3);
    assert!(!c.expired(NOW));
    let rt = runtime::Runtime::new(store_dir("cat-demo"), None, vec![], true, NOW);
    assert!(rt.info.demo && rt.catalog.is_some());
}

// ---------------------------------------------------------------------------------------
// End to end: helper (mock GitHub) → staged files → elevated re-verification

/// The fake GitHub: the repository and the two files of the `catalog` branch.
struct GitHub {
    reported_id: u64,
    files: Vec<(String, Vec<u8>)>,
    /// Status for the catalog files (200 normally).
    file_status: u16,
    /// The answer to requests without a token: 200 serves them (a public repository), 404 is a private
    /// one, 403 / 429 the anonymous rate limit.
    anonymous: u16,
    requests: RefCell<Vec<(String, Option<String>)>>,
}

impl GitHub {
    fn new(catalog: Vec<u8>, sig: Vec<u8>) -> GitHub {
        GitHub {
            reported_id: REPO,
            files: vec![(catalog_fetch::REMOTE_FILE.into(), catalog), (catalog_fetch::REMOTE_SIG.into(), sig)],
            file_status: 200,
            anonymous: 404,
            requests: RefCell::new(vec![]),
        }
    }

    /// The same catalog in a public repository.
    fn public(catalog: Vec<u8>, sig: Vec<u8>) -> GitHub {
        GitHub { anonymous: 200, ..GitHub::new(catalog, sig) }
    }
}

impl Http for GitHub {
    fn get(&self, url: &str, headers: &[(&str, &str)], max: u64) -> anyhow::Result<Response> {
        let auth = headers.iter().find(|(k, _)| *k == "Authorization").map(|(_, v)| v.to_string());
        self.requests.borrow_mut().push((url.to_string(), auth.clone()));
        assert!(url.starts_with("https://api.github.com/"), "request outside the API host: {url}");
        match auth.as_deref() {
            None if self.anonymous != 200 => return Ok(Response { status: self.anonymous, ..Default::default() }),
            None => {}
            Some(a) if a != format!("Bearer {TOKEN}") => return Ok(Response { status: 401, ..Default::default() }),
            Some(_) => {}
        }
        let api = format!("https://api.github.com/repositories/{REPO}");
        let r = if url == api {
            Response {
                status: 200,
                location: None,
                body: serde_json::to_vec(&json!({ "id": self.reported_id, "full_name": "Zeropodaerodynamist/fanrgb" }))
                    .unwrap(),
            }
        } else if let Some(rest) = url.strip_prefix(&format!("{api}/contents/")) {
            let (path, query) = rest.split_once('?').unwrap();
            assert_eq!(query, "ref=catalog", "always the catalog branch");
            assert!(headers.contains(&("Accept", "application/vnd.github.raw+json")));
            match self.files.iter().find(|f| f.0 == path) {
                Some(f) if self.file_status == 200 => Response { status: 200, location: None, body: f.1.clone() },
                Some(_) => Response { status: self.file_status, ..Default::default() },
                None => Response { status: 404, ..Default::default() },
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
    staged: PathBuf,
    state: PathBuf,
    primary: Key,
    backup: Key,
}

impl Env {
    fn new(name: &str) -> Env {
        let base = fsx::tests::test_dir(name);
        let state = base.join("ProgramData");
        std::fs::create_dir_all(&state).unwrap();
        Env { staged: base.join("LocalAppData").join("catalog"), state, primary: key(), backup: key() }
    }

    fn keys(&self) -> Vec<String> {
        vec![self.primary.public.clone(), self.backup.public.clone()]
    }

    /// The unelevated helper's run with a saved token (`tok` that does not validate stands for a saved token
    /// that cannot be read).
    fn fetch(&self, gh: &GitHub, tok: &str) -> catalog_fetch::FetchStatus {
        let t = crate::updater::token::validate(tok)
            .map(Some)
            .map_err(|e| crate::error::err(ErrorCode::UpdateTokenMissing, e));
        self.fetch_with(gh, t)
    }

    /// The helper's run without a token (none saved).
    fn fetch_anonymous(&self, gh: &GitHub) -> catalog_fetch::FetchStatus {
        self.fetch_with(gh, Ok(None))
    }

    fn fetch_with(
        &self,
        gh: &GitHub,
        t: anyhow::Result<Option<crate::updater::token::Token>>,
    ) -> catalog_fetch::FetchStatus {
        let keys = self.keys();
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        let st = catalog_fetch::fetch(gh, t, &self.staged, REPO, &keys, NOW);
        catalog_fetch::write_status(&self.staged, &st).unwrap();
        st
    }

    /// The elevated side (the driver service's runtime).
    fn runtime(&self) -> runtime::Runtime {
        runtime::Runtime::new(self.state.clone(), Some(self.staged.clone()), self.keys(), false, NOW)
    }
}

fn st_code(st: &catalog_fetch::FetchStatus) -> Option<ErrorCode> {
    st.error.as_ref().map(|e| e.code())
}

#[test]
fn end_to_end_fetch_verify_apply() {
    let env = Env::new("cat-e2e");
    let b = bytes(&sample(12));
    let gh = GitHub::new(b.clone(), sign(&env.primary, &b));
    let st = env.fetch(&gh, TOKEN);
    assert_eq!((st.error.clone(), st.serial), (None, Some(12)));
    // Only the API host, always with the token, always the pinned repository id.
    for (u, a) in gh.requests.borrow().iter() {
        assert!(u.starts_with(&format!("https://api.github.com/repositories/{REPO}")), "{u}");
        assert!(a.is_some());
    }
    assert_eq!(gh.requests.borrow().len(), 3, "repository + catalog + signature");
    // The status file round-trips; nothing token-like is in it.
    let back = catalog_fetch::read_status(&env.staged).unwrap();
    assert_eq!(back, st);
    assert!(!std::fs::read_to_string(env.staged.join(catalog_fetch::STATUS_FILE)).unwrap().contains("github_pat_"));
    // Elevated side: verifies the staged bytes, records serial 12, matches the rows.
    let mut rt = env.runtime();
    assert!(rt.catalog.is_none());
    assert_eq!(rt.evaluate_staged(NOW).unwrap(), Outcome::New);
    let info = &rt.info;
    let s = info.loaded.as_ref().unwrap();
    assert_eq!((s.serial, s.key, s.entries), (12, 0, 6));
    assert_eq!(s.key_id, key_id(&env.primary.public));
    let mut rows = this_pc();
    apply_catalog(&mut rows, rt.catalog.as_ref());
    assert_eq!(rows.iter().find(|r| r.category == DriverCategory::Gpu).unwrap().catalog_status, CatalogStatus::Newer);
    // Again the next day: unchanged.
    env.fetch(&gh, TOKEN);
    assert_eq!(rt.evaluate_staged(NOW).unwrap(), Outcome::Unchanged);
    // A fresh start reads the verified cache.
    let rt2 = env.runtime();
    assert_eq!(rt2.catalog.as_ref().unwrap().serial, 12);
    assert_eq!(rt2.info.fetched_at, Some(NOW));
}

/// Diagnostics: serial, age, expiry, signing key id, last download and the per-row match results.
#[test]
fn diagnostics_section() {
    let env = Env::new("cat-diag");
    let b = bytes(&sample(12));
    env.fetch(&GitHub::new(b.clone(), sign(&env.backup, &b)), TOKEN);
    let mut rt = env.runtime();
    rt.evaluate_staged(NOW).unwrap();
    let s = crate::diag::catalog_lines(&rt.info, NOW + 2 * 86_400);
    assert!(s.contains("serial 12") && s.contains("(2 d ago)") && s.contains("6 entries"), "{s}");
    assert!(s.contains(&format!("backup key {}", key_id(&env.backup.public))), "{s}");
    assert!(!s.contains("OUT OF DATE"));
    assert!(crate::diag::catalog_lines(&rt.info, NOW + 30 * 86_400).contains("[OUT OF DATE]"));
    let mut rows = this_pc();
    apply_catalog(&mut rows, rt.catalog.as_ref());
    let st = crate::drivers::DriverState { rows, inventory_done: true, catalog: rt.info.clone(), ..Default::default() };
    let d = crate::diag::drivers_section(&st);
    assert!(d.contains("newer available: 32.0.101.9150 (intel-arc-graphics)"), "{d}");
    assert!(d.contains("latest known: F36d (gigabyte-b650m-d3hp-ax-r13-bios)"), "{d}");
    assert!(d.contains("not in list"), "{d}");
    // Nothing downloaded yet (the token is optional, so its absence is not the reason).
    let none = crate::drivers::CatalogInfo::default();
    let s = crate::diag::catalog_lines(&none, NOW);
    assert!(
        s.contains("none loaded (not downloaded yet)") && s.contains("update token saved: false (optional)"),
        "{s}"
    );
}

/// A public repository: the catalog downloads without a token, no request carries one, and the elevated
/// side verifies and applies it exactly as after a download with a token.
#[test]
fn end_to_end_without_a_token() {
    let env = Env::new("cat-anon");
    let b = bytes(&sample(14));
    let gh = GitHub::public(b.clone(), sign(&env.primary, &b));
    let st = env.fetch_anonymous(&gh);
    assert_eq!((st.error.clone(), st.serial), (None, Some(14)));
    let reqs = gh.requests.borrow();
    assert_eq!(reqs.len(), 3, "repository + catalog + signature");
    assert_eq!(reqs[0].0, format!("https://api.github.com/repositories/{REPO}"), "the pinned id comes first");
    for (u, a) in reqs.iter() {
        assert!(u.starts_with(&format!("https://api.github.com/repositories/{REPO}")), "{u}");
        assert!(a.is_none(), "a request carried a token");
    }
    let mut rt = env.runtime();
    assert_eq!(rt.evaluate_staged(NOW).unwrap(), Outcome::New);
    assert_eq!(rt.catalog.as_ref().unwrap().serial, 14);
    // The trust checks do not depend on the token: a foreign key, a tampered file or another repository
    // behind the pinned id are refused without one too.
    let other = key();
    let st = env.fetch_anonymous(&GitHub::public(b.clone(), sign(&other, &b)));
    assert_eq!(st_code(&st), Some(ErrorCode::CatalogRejected));
    let mut t = b.clone();
    t[20] ^= 1;
    assert_eq!(
        st_code(&env.fetch_anonymous(&GitHub::public(t, sign(&env.primary, &b)))),
        Some(ErrorCode::CatalogRejected)
    );
    let gh = GitHub { reported_id: 1, ..GitHub::public(b.clone(), sign(&env.primary, &b)) };
    assert_eq!(st_code(&env.fetch_anonymous(&gh)), Some(ErrorCode::UpdateRepository));
    assert_eq!(gh.requests.borrow().len(), 1);
}

#[test]
fn backup_key_end_to_end() {
    let env = Env::new("cat-backup");
    let b = bytes(&sample(2));
    env.fetch(&GitHub::new(b.clone(), sign(&env.backup, &b)), TOKEN);
    let mut rt = env.runtime();
    rt.evaluate_staged(NOW).unwrap();
    assert_eq!(rt.info.loaded.as_ref().unwrap().key, 1);
}

#[test]
fn hostile_staging_and_server_answers() {
    // Tampered on the server (token holder edits drivers.json): the helper stages nothing.
    let env = Env::new("cat-hostile");
    let b = bytes(&sample(3));
    let sig = sign(&env.primary, &b);
    let mut t = b.clone();
    t[20] ^= 1;
    let st = env.fetch(&GitHub::new(t, sig.clone()), TOKEN);
    assert_eq!(st_code(&st), Some(ErrorCode::CatalogRejected));
    assert!(!env.staged.join(catalog_fetch::FILE).exists());
    // Signed by a foreign key.
    let other = key();
    assert_eq!(st_code(&env.fetch(&GitHub::new(b.clone(), sign(&other, &b)), TOKEN)), Some(ErrorCode::CatalogRejected));
    // A good fetch, then the staged files are rewritten by another process of the user: re-verified
    // and refused by the elevated side; the error is recorded, the cached catalog stays.
    env.fetch(&GitHub::new(b.clone(), sig.clone()), TOKEN);
    let mut rt = env.runtime();
    rt.evaluate_staged(NOW).unwrap();
    let mut evil = sample(3);
    evil["entries"][0]["download_url"] = json!("https://evil.example/driver.exe");
    std::fs::write(env.staged.join(catalog_fetch::FILE), bytes(&evil)).unwrap();
    let e = rt.evaluate_staged(NOW).unwrap_err();
    assert_eq!(e.code, ErrorCode::CatalogRejected);
    assert_eq!(rt.catalog.as_ref().unwrap().serial, 3, "the verified catalog stays in use");
    assert_eq!(rt.info.error.as_ref().unwrap().code, ErrorCode::CatalogRejected);
    assert_eq!(store::load_record(&env.state).error.unwrap().code, ErrorCode::CatalogRejected);
    // A forged status file: codes outside the helper's set become FRG-E727, text is cleaned.
    std::fs::write(
        env.staged.join(catalog_fetch::STATUS_FILE),
        br#"{"schema":1,"checked_at":1,"serial":null,"error":{"code":205,"detail":"x\u202Ey github_pat_11AAAAAAAAAAAAAAAAAAAA_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#,
    )
    .unwrap();
    let e = rt.evaluate_staged(NOW).unwrap_err();
    assert_eq!(e.code, ErrorCode::CatalogFetch);
    assert!(!e.detail.contains('\u{202E}') && !e.detail.contains("github_pat_11AAAA"), "{}", e.detail);
    // A junction or oversized file in the staging folder is not read.
    std::fs::remove_file(env.staged.join(catalog_fetch::STATUS_FILE)).unwrap();
    assert_eq!(rt.evaluate_staged(NOW).unwrap_err().code, ErrorCode::CatalogFetch, "no status");
}

#[test]
fn github_failures_are_coded() {
    let env = Env::new("cat-gh");
    let b = bytes(&sample(3));
    let sig = sign(&env.primary, &b);
    // A saved token that cannot be read, a rejected token.
    assert_eq!(st_code(&env.fetch(&GitHub::new(b.clone(), sig.clone()), "")), Some(ErrorCode::UpdateTokenMissing));
    let bad = "github_pat_11WRONGTOKEN000000000_abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQ";
    assert_eq!(st_code(&env.fetch(&GitHub::new(b.clone(), sig.clone()), bad)), Some(ErrorCode::UpdateAuth));
    // No token: a private repository (401 / 404), the anonymous rate limit (403 / 429). Each stops at the
    // repository check.
    for (status, code) in [
        (404, ErrorCode::UpdateNotPublic),
        (401, ErrorCode::UpdateNotPublic),
        (403, ErrorCode::UpdateRateLimit),
        (429, ErrorCode::UpdateRateLimit),
    ] {
        let gh = GitHub { anonymous: status, ..GitHub::new(b.clone(), sig.clone()) };
        let st = env.fetch_anonymous(&gh);
        assert_eq!(st_code(&st), Some(code), "HTTP {status}");
        assert_eq!(gh.requests.borrow().len(), 1, "HTTP {status}");
        // The app's reader keeps these codes (they are the helper's to report).
        assert_eq!(catalog_fetch::read_status(&env.staged).unwrap().error.unwrap().code, code.number());
    }
    // Rate-limited on a catalog file after the repository answered: still the rate limit, not a failed
    // download; with a token the same answer keeps its meaning (FRG-E701).
    let mut gh = GitHub::public(b.clone(), sig.clone());
    gh.file_status = 403;
    assert_eq!(st_code(&env.fetch_anonymous(&gh)), Some(ErrorCode::UpdateRateLimit));
    assert_eq!(st_code(&env.fetch(&gh, TOKEN)), Some(ErrorCode::UpdateAuth));
    // Another repository behind the pinned id: stops after one request.
    let mut gh = GitHub::new(b.clone(), sig.clone());
    gh.reported_id = 1;
    assert_eq!(st_code(&env.fetch(&gh, TOKEN)), Some(ErrorCode::UpdateRepository));
    assert_eq!(gh.requests.borrow().len(), 1);
    // Not published yet (no catalog branch / file).
    let mut gh = GitHub::new(b.clone(), sig.clone());
    gh.files.clear();
    let st = env.fetch(&gh, TOKEN);
    assert_eq!(st_code(&st), Some(ErrorCode::CatalogFetch));
    assert!(st.error.unwrap().detail.contains("not published"));
    // Server error.
    let mut gh = GitHub::new(b.clone(), sig.clone());
    gh.file_status = 502;
    assert_eq!(st_code(&env.fetch(&gh, TOKEN)), Some(ErrorCode::CatalogFetch));
    // Larger than the 256 KiB cap: refused while downloading.
    let mut big = sample(3);
    big["padding"] = json!("x".repeat(MAX_CATALOG_BYTES as usize));
    let bb = bytes(&big);
    assert_eq!(
        st_code(&env.fetch(&GitHub::new(bb.clone(), sign(&env.primary, &bb)), TOKEN)),
        Some(ErrorCode::CatalogFetch)
    );
}

/// The redirect policy of the shared HTTP layer applies: a redirect off GitHub's asset hosts is refused
/// and the token never follows one.
#[test]
fn redirects_are_refused() {
    struct Redirect(RefCell<Vec<(String, bool)>>);
    impl Http for Redirect {
        fn get(&self, url: &str, headers: &[(&str, &str)], _max: u64) -> anyhow::Result<Response> {
            self.0.borrow_mut().push((url.into(), headers.iter().any(|(k, _)| *k == "Authorization")));
            if url.ends_with(&format!("repositories/{REPO}")) {
                return Ok(Response {
                    status: 200,
                    location: None,
                    body: format!(r#"{{"id":{REPO},"full_name":"a/b"}}"#).into_bytes(),
                });
            }
            Ok(Response { status: 302, location: Some("https://evil.example/drivers.json".into()), body: vec![] })
        }
    }
    let env = Env::new("cat-redirect");
    let h = Redirect(RefCell::new(vec![]));
    let keys = env.keys();
    let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
    let tok = crate::updater::token::validate(TOKEN)
        .map(Some)
        .map_err(|e| crate::error::err(ErrorCode::UpdateTokenMissing, e));
    let st = catalog_fetch::fetch(&h, tok, &env.staged, REPO, &keys, NOW);
    assert_eq!(st_code(&st), Some(ErrorCode::CatalogFetch));
    assert!(h.0.borrow().iter().all(|(u, _)| !u.contains("evil.example")), "the redirect was followed");
    // The same without a token.
    let h = Redirect(RefCell::new(vec![]));
    let st = catalog_fetch::fetch(&h, Ok(None), &env.staged, REPO, &keys, NOW);
    assert_eq!(st_code(&st), Some(ErrorCode::CatalogFetch));
    assert!(h.0.borrow().iter().all(|(u, a)| !u.contains("evil.example") && !a), "followed, or sent a token");
}

// ---------------------------------------------------------------------------------------
// Independent tests (2026-11-06): the catalog without a token.

/// A saved token that cannot be read stops the catalog download before any request (no silent
/// fall-back to anonymous requests), with FRG-E700.
#[test]
fn tester_unreadable_token_makes_no_catalog_request() {
    let env = Env::new("tst-cat-badtok");
    let b = bytes(&sample(5));
    let gh = GitHub::public(b.clone(), sign(&env.primary, &b));
    let st = env.fetch_with(&gh, Err(crate::error::err(ErrorCode::UpdateTokenMissing, "cannot be decrypted")));
    assert_eq!(st_code(&st), Some(ErrorCode::UpdateTokenMissing));
    assert!(gh.requests.borrow().is_empty(), "asked GitHub without the saved token");
    assert!(!env.staged.join(catalog_fetch::FILE).exists());
    assert_eq!(catalog_fetch::read_status(&env.staged).unwrap().error.unwrap().code(), ErrorCode::UpdateTokenMissing);
}

/// Without a token: GitHub answers the repository and the catalog, then rate-limits the signature
/// request. Nothing is staged (no catalog without its signature), the code is FRG-E715, and the elevated
/// side keeps the catalog it had and shows the code.
#[test]
fn tester_rate_limit_halfway_stages_nothing_and_keeps_the_old_catalog() {
    struct SigLimited(GitHub, u16);
    impl Http for SigLimited {
        fn get(&self, url: &str, headers: &[(&str, &str)], max: u64) -> anyhow::Result<Response> {
            let authed = headers.iter().any(|(k, _)| *k == "Authorization");
            if url.contains("drivers.json.minisig") && !authed {
                self.0.requests.borrow_mut().push((url.into(), None));
                return Ok(Response { status: self.1, ..Default::default() });
            }
            self.0.get(url, headers, max)
        }
    }
    let env = Env::new("tst-cat-halfway");
    let keys = env.keys();
    let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
    // A good anonymous download first, verified and applied by the elevated side.
    let b = bytes(&sample(7));
    let st = env.fetch_anonymous(&GitHub::public(b.clone(), sign(&env.primary, &b)));
    assert_eq!(st.serial, Some(7));
    let mut rt = env.runtime();
    assert_eq!(rt.evaluate_staged(NOW).unwrap(), Outcome::New);
    // The next one (serial 8) is rate-limited on its signature.
    let b8 = bytes(&sample(8));
    for (status, code) in [(429, ErrorCode::UpdateRateLimit), (403, ErrorCode::UpdateRateLimit)] {
        let h = SigLimited(GitHub::public(b8.clone(), sign(&env.primary, &b8)), status);
        let st = catalog_fetch::fetch(&h, Ok(None), &env.staged, REPO, &keys, NOW);
        catalog_fetch::write_status(&env.staged, &st).unwrap();
        assert_eq!(st_code(&st), Some(code), "HTTP {status}");
        assert_eq!(h.0.requests.borrow().len(), 3, "repository + catalog + signature");
        // The staged catalog is still serial 7's bytes, not serial 8's.
        assert_eq!(std::fs::read(env.staged.join(catalog_fetch::FILE)).unwrap(), b, "HTTP {status}");
        let e = rt.evaluate_staged(NOW).unwrap_err();
        assert_eq!(e.code, code, "HTTP {status}");
        assert_eq!(rt.catalog.as_ref().unwrap().serial, 7, "the verified catalog stays in use");
        assert_eq!(rt.info.error.as_ref().unwrap().code, code);
    }
    // A later good run clears the error.
    let st = env.fetch_anonymous(&GitHub::public(b8.clone(), sign(&env.primary, &b8)));
    assert_eq!(st.serial, Some(8));
    assert_eq!(rt.evaluate_staged(NOW).unwrap(), Outcome::New);
    assert!(rt.info.error.is_none());
}

/// Without a token, the catalog's answers map like the release check's: 401 / 404 on the repository is
/// "not public" (FRG-E714), 403 / 429 the rate limit (FRG-E715), anything else a failed download
/// (FRG-E727); a 404 on a file after the repository resolved is "not published yet", not "not public".
#[test]
fn tester_catalog_status_mapping_without_a_token() {
    let env = Env::new("tst-cat-map");
    let b = bytes(&sample(3));
    let sig = sign(&env.primary, &b);
    for (status, code) in
        [(500, ErrorCode::CatalogFetch), (502, ErrorCode::CatalogFetch), (418, ErrorCode::CatalogFetch)]
    {
        let mut gh = GitHub::public(b.clone(), sig.clone());
        gh.file_status = status;
        assert_eq!(st_code(&env.fetch_anonymous(&gh)), Some(code), "HTTP {status}");
    }
    let mut gh = GitHub::public(b.clone(), sig.clone());
    gh.files.clear();
    let st = env.fetch_anonymous(&gh);
    assert_eq!(st_code(&st), Some(ErrorCode::CatalogFetch));
    assert!(st.error.unwrap().detail.contains("not published"));
}
