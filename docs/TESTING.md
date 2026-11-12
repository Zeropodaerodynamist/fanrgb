# Testing FanRGB on your own PC

FanRGB 0.2.0 is a beta, and it has only been tested on one PC ([HARDWARE.md](HARDWARE.md)). This page walks you
through trying it safely, one step at a time. Each step can be undone, and you can stop after any of them.

## Before you start

- Close every other fan or RGB program (GIGABYTE Control Center, RGB Fusion, EasyTune, SIV, FanControl,
  OpenRGB, SignalRGB, iCUE, Armoury Crate). Check the tray as well.
- Install PawnIO from an administrator prompt: `winget install namazso.PawnIO` (or <https://pawnio.eu/>).
- Optional but useful: keep [HWiNFO](https://www.hwinfo.com/) open in sensors-only mode, to compare readings.
- **How to get your BIOS fan control back at any time:**
  - tray icon › **Return fans to BIOS**, or Ctrl+Shift+B;
  - or run `fanrgb.exe --exit`;
  - or, if the app does not react, use Task Manager › End task on the main `fanrgb.exe` (not the one started with
    `--watchdog`). The watchdog hands the fans back to the BIOS within seconds.

## 1. Check the download

In PowerShell, in the folder you downloaded to:

```
Get-FileHash .\FanRGB-0.2.0-beta-x64.exe -Algorithm SHA256
```

The hash must match the one published next to the download. If it does not, do not run the file.

## 2. Look around without touching the hardware (2 minutes)

```
FanRGB-0.2.0-beta-x64.exe --demo
```

Demo mode simulates fans, sensors and an RGB controller. It needs no administrator rights and never touches the
hardware. Use it to learn the pages. `--exit --demo` closes it.

## 3. Read-only hardware report (5 minutes)

Open an **administrator** Command Prompt in the download folder and run:

```
FanRGB-0.2.0-beta-x64.exe --diag > diag-before.txt
```

This writes nothing to the fan chip. Open `diag-before.txt` and check:

- PawnIO says **Ready**, and the chip line names an ITE chip from the list in [HARDWARE.md](HARDWARE.md).
- Temperatures are within about 2 °C of HWiNFO, and fan speeds within about 5 %.
- The fan headers have the names your board manual uses.

**Stop here if** the chip is not recognised, the readings are clearly wrong, or the headers are named wrongly.
Please send the report (see *Reporting* below): it is exactly what is needed to support another board.

## 4. First start, still on the BIOS (10 minutes)

Run `FanRGB-0.2.0-beta-x64.exe` and accept the administrator prompt. FanRGB starts on the **BIOS** mode, so the
fans keep following your BIOS. Watch the Overview page for a few minutes: the temperatures and fan speeds should
match HWiNFO. Two `fanrgb.exe` in Task Manager are normal (the app and its crash watchdog).

## 5. Identify your fans (5 minutes, PC idle)

Cooling › **Identify your fans**. The wizard runs one header at a time at full speed and then slower, and asks what
changed. Answer honestly; choose **Not sure** if you cannot tell, which keeps that header at 60 % or more.
Pumps are never run slow for long. Fan modes other than BIOS only start once every header is identified.

## 6. Try the modes, one at a time (15 minutes)

Use the tiles on Overview or Alt+1 to Alt+6: Silent, Balanced, Performance, Full speed, Auto (AI), BIOS.

For each mode, wait a minute or two and check:

- the fans change speed as expected, and nothing stops that should not (a pump must never stop);
- the CPU temperature settles at a sensible value for your cooler at idle;
- no red alert appears on the Overview page.

Full speed should make every controlled fan loud within a few seconds. BIOS should bring back exactly the
behaviour you had before FanRGB.

## 7. Check the hand-back (5 minutes)

Close FanRGB from the tray (**Exit**) or with `fanrgb.exe --exit`. Wait a minute, then in the administrator prompt:

```
FanRGB-0.2.0-beta-x64.exe --diag > diag-after.txt
```

The fan register section of `diag-after.txt` should match `diag-before.txt`: FanRGB put your BIOS fan settings
back exactly as it found them.

## 8. Keep it (optional)

To run FanRGB at every sign-in: `FanRGB-0.2.0-beta-x64.exe --install` (administrator prompt). It copies itself to
`C:\Program Files\FanRGB\fanrgb.exe` and adds a start-up task. `fanrgb.exe --uninstall` removes both again
(settings and logs are kept). Run **Calibrate** for each fan with a speed signal before using Auto (AI).

## Reporting

If something looks wrong, return the fans to the BIOS first, then collect:

- `C:\ProgramData\FanRGB\diagnostics.txt` (in the app: Diagnostics › Save report);
- `C:\ProgramData\FanRGB\fanrgb.log`;
- `%APPDATA%\FanRGB\config.json`;
- your `diag-before.txt` and `diag-after.txt`;
- the motherboard model and BIOS version.

Open an issue on the GitHub repository with these attached. For a security problem, do not open an issue: follow
[SECURITY.md](../SECURITY.md).

## For developers: the automated tests

FanRGB builds with the Rust toolchain pinned in `rust-toolchain.toml` (rustup installs it on first use) and the
MSVC build tools. The same checks CI runs:

```
cargo fmt --all --check
cargo clippy --release --locked -- -D warnings -A clippy::needless_range_loop -A clippy::neg_cmp_op_on_partial_ord
cargo test --release --locked
cargo deny --locked check advisories licenses bans sources
cargo build --release --locked
```

The tests run the control logic against simulated hardware and a mock register file; they never touch the real
fan chip, so they are safe to run while FanRGB itself is running.
