# Contributing to FanRGB

Thank you for helping. FanRGB writes to the fan controller's registers with administrator rights, so the bar
for changes that touch hardware is high. Please read *Safety design* in the [README](README.md) first.

## Before you start

- For anything bigger than a small fix, open an issue first so we can agree the approach before you spend time
  on it.
- Bug reports: please include your board model and BIOS version, the Super I/O chip, and the output of
  `fanrgb.exe --diag` (read-only; run it from an administrator prompt for the full report). Look through it
  before posting: it contains file paths with your Windows user name and may contain your account's security ID.
- Security problems: don't open an issue. See [SECURITY.md](SECURITY.md).

## Build and test

You need Windows 10 or 11 (x64), the MSVC build tools and rustup; `rust-toolchain.toml` pins the compiler.
These are the checks CI runs, so please run them before opening a pull request:

```
cargo fmt --all --check
cargo clippy --release --locked -- -D warnings -A clippy::needless_range_loop -A clippy::neg_cmp_op_on_partial_ord
cargo clippy --release --all-targets --locked
cargo test --release --locked
cargo audit
cargo deny --locked check advisories licenses bans sources
```

For the driver catalog tooling:

```
python -I -m unittest discover -s tools/driver-catalog/tests -t tools/driver-catalog -v
```

`fanrgb.exe --demo` runs the app with simulated fans, sensors and RGB controller, without administrator rights
or any hardware access. Use it for UI work.

## Rules for changes

- Every fan register write goes through the engine's safety layer. Don't add a path that writes fan
  registers around it, and keep RGB controller I/O on the RGB worker thread.
- A change to a chip driver, fan control or the RGB protocol must say in the pull request which real hardware
  it was tested on (board, chip, BIOS version), or say plainly that it is untested on real hardware.
- New dependencies must pass `deny.toml`: permissive licences only (it also lists LGPL-2.1, for the embedded
  PawnIO modules), and no second TLS stack.
- Never copy GPL code (for example from the Linux `it87` driver or OpenRGB), and don't copy code under any
  other licence into FanRGB's Apache-2.0 files. Register addresses, bit layouts and protocol facts are
  fine; say where each came from in a comment.
- `src/hw/superio/ite.rs` and `src/hw/superio/mod.rs` contain code derived from LibreHardwareMonitor and
  are under MPL-2.0 (see their headers). Changes to those two files stay under MPL-2.0.
- Don't add vendor software, decompiled code, or whole saved copies of vendor web pages to the repository
  (a test fixture keeps only the parts of a page its parser needs).

## Licence

By contributing, you agree that your contribution is licensed under the Apache License 2.0, as section 5 of
[LICENSE](LICENSE) describes, or under the Mozilla Public License 2.0 for changes to the two MPL-2.0 files.
