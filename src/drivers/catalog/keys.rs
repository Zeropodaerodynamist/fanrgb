//! Driver-catalog signing keys (minisign public keys, base64 of `Ed` + key id + Ed25519 key).
//!
//! Separate from the release keys in `updater::keys` (key separation): the catalog is signed by a CI
//! job every week, so its key lives in a GitHub Actions secret, while the release keys never leave the
//! maintainer's PC. Holding the catalog key only lets someone make the driver list misleading (text,
//! versions, links to allow-listed vendor pages); it can never make FanRGB run or install anything.
//!
//! - `PRIMARY`: its secret half exists only in the Actions secret `CATALOG_MINISIGN_KEY` of the
//!   repository (set with `fanrgb-release catalog-keygen`, never written to disk).
//! - `BACKUP`: its secret half is kept offline by the maintainer (docs/KEYS.md), for rotation: if the
//!   CI secret leaks or is lost, sign with the backup key and ship a build with a new pair.

/// Signs the weekly catalog in CI (key id E9DA5FDE72C7DE0A).
pub const PRIMARY: &str = "RWQK3sdy3l/a6awgXgE2EkvUiLO4TaTEXMLjwRJxNFxs3fRxIdPAG5rN";
/// Spare key for rotation, kept offline (key id 07D52C62A5F07483).
pub const BACKUP: &str = "RWSDdPClYizVB9wNBbA96LA165JezU3j3uJXTomx0XSOjHxTa/g+MuvB";

/// Every key a catalog may be signed with.
pub const TRUSTED: [&str; 2] = [PRIMARY, BACKUP];
