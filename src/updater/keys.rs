//! Release signing keys (minisign public keys, base64 of `Ed` + key id + Ed25519 key).
//!
//! The private halves are kept by the maintainer, password-protected (`fanrgb-release protect-keys`; see
//! docs/KEYS.md), never in this repository and never in CI.
//! `PRIMARY` signs every release; `BACKUP` exists so a lost or retired primary key can be replaced by a
//! release signed with the backup key that ships a new key pair, without any trust-on-first-use step.

/// Signs releases.
pub const PRIMARY: &str = "RWRewFpvHJMo5HzNt0cVkJszPiHguZjdJLXfF57rvN3ko3newPbMuFUR";
/// Spare key for rotation (kept offline, unused until the primary is retired).
pub const BACKUP: &str = "RWTrBEt116MMbOM0+pm/vDR0xOZZRk4mnF2ABB8z+aUqQBzanqxTFRNM";

/// Every key a release manifest may be signed with.
pub const TRUSTED: [&str; 2] = [PRIMARY, BACKUP];
