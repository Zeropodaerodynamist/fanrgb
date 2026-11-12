# FanRGB signing keys

Public information only: key IDs, public keys, where the secret halves are kept (in words) and how keys are
rotated. **No secret key, password or token is ever written to this repository**, to a CI log or to an issue.

All keys are [minisign](https://jedisct1.github.io/minisign/) Ed25519 keys made by `tools/fanrgb-release`. The key
ID is the 8-byte ID inside the public key (bytes 2-9 of the decoded key, shown as minisign prints it). The app trusts
exactly the public keys compiled into it; there is no trust-on-first-use path.

## Inventory

| Key | Signs | Key ID | Public key (compiled in) | Secret half kept |
|---|---|---|---|---|
| Release primary | every release manifest (`release.json`) | `E428931C6F5AC05E` | `RWRewFpvHJMo5HzNt0cVkJszPiHguZjdJLXfF57rvN3ko3newPbMuFUR` | maintainer PC, owner-only ACL, password-protected (`release.ps1` refuses an unencrypted key) |
| Release backup | only a rotation / emergency release | `6C0CA3D7754B04EB` | `RWTrBEt116MMbOM0+pm/vDR0xOZZRk4mnF2ABB8z+aUqQBzanqxTFRNM` | offline, on two separate media |
| Catalog primary | the weekly driver catalog (`catalog/drivers.json`) | `E9DA5FDE72C7DE0A` | `RWQK3sdy3l/a6awgXgE2EkvUiLO4TaTEXMLjwRJxNFxs3fRxIdPAG5rN` | only in the GitHub Actions secret `CATALOG_MINISIGN_KEY` (written from memory by `catalog-keygen`, never on disk) |
| Catalog backup | a catalog signed by hand while the CI key is rotated | `07D52C62A5F07483` | `RWSDdPClYizVB9wNBbA96LA165JezU3j3uJXTomx0XSOjHxTa/g+MuvB` | offline, like the release backup |

Compiled in: `src/updater/keys.rs` (release, `TRUSTED = [PRIMARY, BACKUP]`) and `src/drivers/catalog/keys.rs`
(catalog). The two pairs are separate on purpose: a catalog key can only make the driver list misleading (text,
versions, links to allow-listed vendor pages) and can never sign an update, and the reverse.

Not a signing key, but on the same trust path: the maintainer's GitHub account (2FA) and the `gh` CLI token on the
PC, and the optional fine-grained read-only token users may paste in Settings › Updates (only needed while the
repository is private, or for a higher GitHub rate limit; DPAPI-encrypted per user, never logged).
Git tags are meant to be signed with an SSH signing key (`git config gpg.format ssh`), which is separate from all
of the above.

## Key ceremony (once, about 30 minutes)

In a real console window (the password prompts read the console, never a pipe):

1. Protect the release keys and the catalog backup key, with **different** long passphrases from the password
   manager for release and catalog:
   ```
   cargo run --release --locked --manifest-path tools\fanrgb-release\Cargo.toml -- protect-keys --dir %USERPROFILE%\.fanrgb-release
   cargo run --release --locked --manifest-path tools\fanrgb-release\Cargo.toml -- protect-keys --dir %USERPROFILE%\.fanrgb-release --key catalog-backup
   ```
2. Check: `fanrgb-release pubkeys --dir %USERPROFILE%\.fanrgb-release` prints the public keys; they must equal the
   table above (and `keys.rs`).
3. Copy `backup.key`, `catalog-backup.key` and the `.pub` files to **two** offline media kept in different places
   (an encrypted USB stick, and a second one or a password manager's secure-file attachment). Restore one copy on
   another machine and check that it decrypts (`fanrgb-release sign` on a scratch file with `--pub`).
4. Delete `backup.key` and `catalog-backup.key` from the PC. Only the encrypted `primary.key` stays.
   On an SSD the old unencrypted bytes may survive in free space; if that matters, generate new backup pairs
   instead (`keygen` encrypts from the start) and rotate them in as below.
5. Note the date and the media (in words) in the inventory above; calendar a yearly "restore and decrypt the
   backups" check and the token expiry dates.

## Rotation

Planned (every 2 years, a lost backup medium, a new signing machine):

1. Generate a new pair (`fanrgb-release keygen --dir <new folder>`; encrypted).
2. Put the new public key into `keys.rs` **next to** the current one (`TRUSTED = [NEW, CURRENT]` while both are
   valid) and ship that build as a normal release, **signed with the current primary**.
3. After at least one more release that installed fine, make the new key the primary, sign with it, and drop the old
   one from `keys.rs` in the following release.
4. Update this file (key IDs, public keys, where kept).

Never rotate *through* the backup key unless the primary is lost or leaked: the backup exists for the emergency
path in [RELEASING.md](RELEASING.md#incident-a-key-or-token-leaked). A catalog key is rotated the same way
(`catalog-keygen` writes a new primary into the Actions secret; ship its public key in an app release first).

Users on a build that only trusts retired keys cannot update; they reinstall by hand (`fanrgb --install` from a
new build, then `fanrgb --reset-update-state` if the old record blocks the version).
