# FanRGB

Fan control and RGB lighting for Gigabyte motherboards (ITE Super I/O + RGB Fusion 2 USB), written in Rust
as a native Windows app. A lightweight, safety-first replacement for GIGABYTE Control Center / RGB Fusion /
EasyTune.

Developed against a **Gigabyte B650M D3HP AX** (ITE IT8689E Super I/O, ITE IT5702 RGB controller), Ryzen 5
7500F and Intel Arc B580. The chip driver knows the ITE IT86xx/IT87xx family and the sensor/fan labels follow
Gigabyte's AM5 layout tables, so other Gigabyte boards with these chips may work, but only the board above has
been tested on real hardware so far (see *Status*).

## Status

Early and experimental: version 0.2.0 is a **beta**, distributed as a download but not yet as a signed GitHub
release. Before trying it, read [docs/HARDWARE.md](docs/HARDWARE.md) (what it was tested on) and follow
[docs/TESTING.md](docs/TESTING.md) (a safe step-by-step first run); to adapt it to your own PC, see
[docs/CUSTOMISING.md](docs/CUSTOMISING.md). The code had an independent security audit
before publication ([docs/SECURITY-AUDIT.md](docs/SECURITY-AUDIT.md)). Automated tests cover the control logic
against simulated hardware and a mock register file, though not every path (some tests are marked ignored). On
the reference PC above, the first real-hardware checks have passed (recovery from an unclean exit, fan
identification, replacing a running copy); the rest of the checklist (hardware failsafe, RGB after a power
drain, sleep/resume, watchdog) is still open, and no other board has been tested. Start with
`fanrgb.exe --diag` and the BIOS profile, and watch the first manual fan change.

## Features

**Fans**
- Reads every fan header (RPM, duty), board temperatures and voltages, AMD Zen CPU temperature (Tctl/Tdie,
  CCDs) and package power, and GPU temperature/load/fan for any vendor (D3DKMT, the Task Manager source).
- Modes per fan: **BIOS** (Smart Fan untouched), **Fixed**, **Curve** (editor, hysteresis, smoothing,
  multi-sensor source: hottest or average) and **AI**.
- Profiles: BIOS (the default), Silent, Balanced, Performance, AI Auto, Full Speed, plus your own; switchable
  from the window or the tray.
- **Curves run in the chip.** A curve whose only source is a chip temperature input (e.g. CPU) is programmed
  into the Super I/O, which then runs it by itself, also if the app hangs. Curves on other sources (GPU,
  several sensors) and Fixed/AI duties are written as a flat hardware curve that still carries a **chip-level
  failsafe**: the chip forces 100 % on its own when the CPU reaches the critical temperature (capped at
  90 °C). If the chip does not accept a curve, the app falls back to software control (FRG-E503).
- Calibration wizard: measures each fan's RPM curve, stop duty and start duty.
- Limits per fan: min/max duty, optional 0 RPM, separate ramp-up/ramp-down rates.

**Learning thermal controller ("AI" mode)**

No neural network and no language model. It is a small physical model of your PC's cooling, fitted online:
- **Grey-box model**: heatsink and case-air temperature nodes driven by CPU package power and GPU power, with
  learned heat-transfer coefficients that depend on fan airflow, plus a die offset (°C per W) and a GPU offset.
  Parameters are estimated with robust recursive least squares; every parameter has a physical meaning and is
  shown on the Intelligence page.
- **Decision**: every step it simulates the next 5 minutes for each CPU-fan / case-fan airflow pair and picks
  the quietest one that keeps the temperature under your target (model-predictive control).
- **Anti-hunting supervisor**: speeds rise at once, fall only after the lower need is sustained, with a dwell
  time and a deadband, so the fans do not keep changing pitch.
- **Safety floor**: the result can never go below a conservative floor curve, a limit latch drives 100 % above
  the hard limit, and a briefly missing input holds the last speed (after 5 s without the control
  temperature the fans go back to the BIOS). Until the model has passed open-loop validation
  runs it stays close to the floor; if its predictions go wrong later it is distrusted (FRG-E502) and falls
  back to the floor until it validates again. Learning pauses at ≥ 93 °C and without a measured fan speed.
- Learned models are saved and keyed to the hardware (CPU, temperature source, fans).

**Lighting (RGB Fusion 2 USB, ITE 048D:5702 / 8297 / 5711 / 8950)**
- Zones: D_LED1 and D_LED2 (5 V ARGB, per LED), LED_C (12 V RGB), optional onboard LEDs.
- Software effects: Static, Breathing, Spectrum, Rainbow, Gradient, Comet, Chase, Twinkle, Fire,
  Temperature-reactive, Load bar, Music visualizer (WASAPI loopback), Strobe.
- Hardware effects (run without the app): Static, Breathing, Flash, Double flash, Color cycle, Wave, Random,
  with "Save to controller".
- **Persistence through sleep and shutdown**: on exit, sleep and Windows shutdown the lighting is handed to
  the controller (default: a hardware color cycle; or keep last / off) and committed with the controller's
  save command; on resume the controller is re-initialised and the effect re-sent.
- **Auto-reconnect**: all USB I/O runs on a watchdog-guarded worker thread; a hung or unplugged controller
  never blocks the UI, and the app reconnects with back-off. A controller Windows reports as failed (USB
  error 43) is detected and reported (FRG-E401).
- **LED_C slot**: board revisions wire the 12 V header to controller division 1 or 4. The default drives both
  (an unused division is inert); Settings › Lighting › LED_C channel picks one. Use **Identify** on Lighting › Zone setup
  to see which one lights up.
- Per-zone effects or sync-all, LED count, color order fix, master brightness, overheat alert, live preview
  without a controller.

**System**
- Tray icon, close to tray, start with Windows (Task Scheduler, elevated, no UAC prompt at sign-in).
- Diagnostics page and report: stuck RGB controller, conflicting software (GCC, RGB Fusion, EasyTune, SIV,
  FanControl, OpenRGB, SignalRGB, iCUE, Armoury Crate/Aura), Super I/O register dump.
- Every user-visible problem carries a stable code (`FRG-E…`) with an explanation and, where possible, a fix
  button.

## Safety design
- Hardware access only through **PawnIO**, the signed open-source driver also used by LibreHardwareMonitor and
  FanControl. No WinRing0, no Gigabyte kernel drivers.
- Fans stay on the BIOS curve until you pick a non-BIOS mode, and **every mode except BIOS waits for one
  "Identify your fans" run**: until each fan header has a confirmed device (from the wizard, or set by hand in
  "What's connected?"), all headers stay on the BIOS. FanRGB trusts the device you confirm, so check it; a header marked "Not sure" never runs below 60 % (it could be a pump). The firmware fan registers (including the
  IT8689E "extra vectors" Smart Fan uses) are saved and restored on exit, profile change, sensor loss and
  "Return fans to BIOS" (tray menu, or Ctrl+Shift+B in the FanRGB window).
- **Crash watchdog**: a separate `--watchdog` process restores the saved firmware state if FanRGB is killed or
  crashes; a panic hook and the UI's `catch_unwind` hand fans back as well. The snapshot lives in an
  admin-only folder and is validated (process, boot, plausible values) before use. Fans without a chip
  failsafe are never put under software control while the watchdog is not running.
- Critical CPU/GPU temperature (FRG-E301): every controlled fan goes to 100 % until the temperature is 5 °C
  below critical for 10 s. A suspected cooler failure on a header without a speed signal (FRG-E305) runs the
  whole CPU chain at 100 %. The cooling-efficacy monitor (FRG-E309) only informs in v0.2 (alert and tray
  balloon, no forced speeds). The margins are in `src/control/limits.rs`. A temperature source that stops
  reporting for 5 s returns its fans to the BIOS (FRG-E300).
- Firmware overriding a setting (e.g. after resume) is detected and re-applied (FRG-E206); hardware curves are
  re-programmed after resume and every 5 minutes.
- Unsafe settings (from the UI or a hand-edited file) are clamped every cycle. Some minimums hold whatever
  the settings file says, because any program you run can edit it: the board's CPU headers never run below
  20 % and its PUMP header never below 30 % (no fan stop), and from 12 °C below the critical temperature
  every fan FanRGB drives rises to at least 75 % at 2 °C below it. Calibration tables are range-checked.
- Shares the standard `Global\Access_ISABUS.HTP.Method` / `Global\Access_PCI` mutexes with HWiNFO,
  LibreHardwareMonitor and others.
- The elevated process hardens itself (System32-only DLL search, Redirection Guard), keeps its state in an
  admin-only folder, treats `config.json` as untrusted input (size-limited read, link-refusing atomic
  writes) and starts browsers/Explorer/winget with your normal rights.

## Install
1. **Install PawnIO** (needed for fan control and CPU/board temperatures): `winget install namazso.PawnIO`
   or <https://pawnio.eu/>.
2. Build: `.\tools\dist.ps1` (runs `cargo build --release --locked`; MSVC; `rust-toolchain.toml` pins
   Rust 1.91.1, which rustup installs on first use). The program is **`dist\current\fanrgb.exe`**; earlier
   builds are kept in `dist\archive\`. Don't run the exe from `target\` (see [docs/BUILDS.md](docs/BUILDS.md)).
3. Run `fanrgb.exe --install` (asks for administrator rights). It copies the program to
   `%ProgramFiles%\FanRGB\fanrgb.exe` and registers start with Windows (minimized). **Why Program Files:**
   the startup task runs FanRGB elevated without a UAC prompt, so the file it starts must be one normal
   programs cannot replace; otherwise any program could get administrator rights by swapping the exe.
   Start with Windows is refused for a copy outside Program Files (FRG-E608).
4. Run **Identify your fans** (Cooling, or the prompt on Overview) while the PC is idle; fan modes other than
   BIOS start once every header is identified. Then pick a fan profile (start with **Balanced** or **AI
   Auto**). Optionally run **Calibrate** first for each fan with a speed signal (it is refused for pumps, unknown headers
   and headers without one), so the AI knows each fan's real airflow.

`fanrgb.exe --uninstall` stops FanRGB (fans back to BIOS), removes the startup task and the Program Files copy;
settings and logs are kept.

Close GIGABYTE Control Center, RGB Fusion, EasyTune, SIV and other fan/RGB tools: two programs writing the same
chip will fight.

### Publishing a release (maintainer)
The runbook is [docs/RELEASING.md](docs/RELEASING.md) (steps, canary pre-release and promotion, rollback, key
incidents); the keys are listed in [docs/KEYS.md](docs/KEYS.md). Changes are recorded in
[CHANGELOG.md](CHANGELOG.md).
Releases are signed with a minisign key (`tools/fanrgb-release`, driven by `tools/release.ps1`); the app only
installs a manifest signed by a key compiled into `src/updater/keys.rs`. `release.ps1` publishes only the
exact commit it built (`--target <sha>`), which must be clean, on `origin/main` and green in CI, and publishes
it as a pre-release unless `-Prerelease:$false`; `-PromoteLatest <tag>` makes a soaked pre-release the latest.
- The secret keys must be **password-protected**: anyone who can read an unencrypted key can sign an
  "update" that FanRGB installs with administrator rights. `release.ps1` refuses an unencrypted key.
  To protect existing keys once (asks for a new password twice, on the console, without echo; re-encrypts
  `primary.key` and `backup.key` in place after checking that the encrypted copy decrypts to the same key):
  `cargo run --release --locked --manifest-path tools\fanrgb-release\Cargo.toml -- protect-keys --dir %USERPROFILE%\.fanrgb-release`
  (in a console window: the password is read from the console, never from a pipe).
  New keys from `fanrgb-release keygen` are encrypted from the start.
- Signing (`tools\release.ps1`; notes from the version's CHANGELOG section, or `-NotesFile`) asks for the
  key's password on the console.
- Keep `backup.key` on offline media, and ideally sign on a PC that does not run FanRGB: the unencrypted
  bytes of a key that was protected later can survive in free disk space.
- `tools\release.ps1 -DryRun` tests the pipeline with a throwaway key pair in a temporary folder; it never
  reads the real keys, and it lists every check that would refuse a real release.

## Updates and network use
FanRGB sends no telemetry. It goes online only for the following, each from a separate helper process that runs
without administrator rights:
- **Self-update** (Settings › Updates; on by default, once a day): checks this project's GitHub releases. A
  release is installed only if its manifest is signed by one of the keys compiled into the app (see
  [docs/KEYS.md](docs/KEYS.md)) and is newer than anything installed before, and the downloaded exe matches the
  manifest's size and SHA-256. The old exe is kept as `fanrgb.exe.old`, and FanRGB rolls back by itself if the
  new version does not run steadily. The check needs no GitHub account or token: it uses GitHub's API without
  signing in, which allows 60 requests an hour from one internet address (a daily check makes about eight). An
  access token is optional: a fine-grained, read-only token pasted in Settings › Updates is only needed while the
  repository is private, or to raise that limit, and is stored encrypted for your Windows account. Either way the
  same signature and version checks decide what is installed. The same daily check also downloads a signed list
  of the latest Intel Arc, AMD chipset and Realtek LAN driver versions that this project publishes.
- **Driver check** (Updates page; the daily check is off until you turn it on, "Check now" works either way):
  lists your installed drivers and asks Windows Update for newer ones, which sends this PC's hardware IDs to
  Microsoft, as Windows Update itself does, and compares them with that signed list. FanRGB never installs
  drivers; it only tells you.

## Files
| What | Where |
| --- | --- |
| Settings | `%APPDATA%\FanRGB\config.json` (unknown values reset one by one; a file that is not JSON is kept as `config.json.bad`) |
| Logs, learned AI models, crash snapshot, diagnostics, cooling baseline, update and driver-check state (elevated) | `%ProgramData%\FanRGB` (admin-only) |
| Same, when running without administrator rights | `%LOCALAPPDATA%\FanRGB` |
| Demo mode settings and models | `demo\` inside the folder above (never the real files) |
| Update access token (optional, encrypted for your Windows account) and downloaded updates | `%LOCALAPPDATA%\FanRGB\update_token.bin`, `%LOCALAPPDATA%\FanRGB\update\` |

## Command line
| Option | Meaning |
| --- | --- |
| `--minimized` | Start hidden in the tray |
| `--demo` | Simulated fans, sensors and RGB controller (no admin rights, no hardware access) |
| `--no-elevate` | Do not ask for administrator rights |
| `--verbose` | Debug logging |
| `--diag` | Read-only hardware report (no fan writes) to the console and `diagnostics.txt` in the state folder; run it from an administrator prompt for the full report |
| `--exit` | Stop a running instance cleanly (fans go back to BIOS); stops a demo instance if no real one runs |
| `--exit --demo` | Stop only a running demo instance |
| `--install` / `--uninstall` | See *Install* |
| `--revert [--yes]` | Go back to the previous version the updater kept (`%ProgramFiles%\FanRGB\fanrgb.exe.old`). Asks first (a Yes/No dialog; `--yes` skips it), elevates, stops FanRGB (fans to BIOS during the swap), checks and swaps the file, records it so the version left is not offered again, and restarts FanRGB if it was running. One step back only. See [docs/RELEASING.md](docs/RELEASING.md#rollback) |
| `--reset-update-state [--yes]` | Clear the updater's downgrade/replay record (`%ProgramData%\FanRGB\update_state.json`, kept as a `.bak`) after reinstalling a known-good build; asks first, elevates |
| `--driver-check [--offline]` | Check for newer drivers now through Windows Update and print the result (no hardware access); `--offline` lists the installed drivers only |
| `--help`, `-h`, `/?` | Usage |

Unknown arguments are ignored. `--watchdog`, `--parent-handle`, `--state-dir`, `--restart-after`, `--replace`,
`--update-fetch`, `--catalog-fetch` and `--update-finish` are internal (crash watchdog, restart, replacing a
running copy, and the update helpers).

Starting FanRGB while it already runs brings the running window to the front. If the running copy is a
*different* exe (an older build, another folder), FanRGB asks whether to close that copy and start this one
instead ([docs/BUILDS.md](docs/BUILDS.md#starting-a-new-build-while-an-old-one-runs)). Two `fanrgb.exe` in
Task Manager are normal: the app and its crash watchdog.

## Error codes
| Code | Title | Severity |
| --- | --- | --- |
| FRG-E100 | PawnIO driver not installed | Error |
| FRG-E101 | Administrator rights needed | Warning |
| FRG-E102 | PawnIO module rejected | Error |
| FRG-E103 | Hardware call failed | Error |
| FRG-E104 | Hardware bus busy | Warning |
| FRG-E105 | PawnIO is outdated | Warning |
| FRG-E200 | No supported fan controller found | Error |
| FRG-E201 | Fan controller answered unexpectedly | Error |
| FRG-E202 | Fan controller write not confirmed | Error |
| FRG-E203 | Fan channel does not exist | Error |
| FRG-E204 | Fan is monitor-only | Warning |
| FRG-E205 | Could not return fan to BIOS control | Critical |
| FRG-E206 | Firmware changed a fan setting | Info |
| FRG-E207 | Fan not spinning | Critical |
| FRG-E208 | Fan calibration failed | Warning |
| FRG-E209 | Header identification stopped | Warning |
| FRG-E300 | Temperature sensor lost | Warning |
| FRG-E301 | Critical temperature | Critical |
| FRG-E302 | CPU temperature unavailable | Warning |
| FRG-E303 | GPU sensors unavailable | Info |
| FRG-E304 | Implausible sensor value | Warning |
| FRG-E305 | CPU cooling may have failed | Critical |
| FRG-E309 | Cooling performance degraded (pump or airflow failure?) | Critical |
| FRG-E400 | RGB controller not found | Info |
| FRG-E401 | RGB controller is not responding | Error |
| FRG-E402 | RGB controller is in use | Error |
| FRG-E403 | RGB write failed | Warning |
| FRG-E404 | RGB controller sent a bad reply | Warning |
| FRG-E405 | RGB controller disconnected | Warning |
| FRG-E406 | Unsupported RGB controller | Warning |
| FRG-E407 | Saving lighting to controller failed | Error |
| FRG-E408 | Audio capture unavailable | Warning |
| FRG-E409 | Windows Dynamic Lighting controls the LEDs | Warning |
| FRG-E410 | Could not take the LEDs back from Windows | Warning |
| FRG-E500 | Thermal model reset | Info |
| FRG-E501 | Thermal model file damaged | Warning |
| FRG-E502 | Learned model distrusted | Info |
| FRG-E503 | Hardware fan curve not used | Info |
| FRG-E504 | Fans handed back to BIOS | Info |
| FRG-E505 | Fan profile changed | Info |
| FRG-E600 | Settings could not be loaded | Warning |
| FRG-E601 | Settings could not be saved | Error |
| FRG-E602 | Start-with-Windows failed | Error |
| FRG-E603 | Already running | Info |
| FRG-E604 | Safety watchdog unavailable | Warning |
| FRG-E605 | Conflicting software running | Warning |
| FRG-E606 | File access failed | Warning |
| FRG-E607 | Startup marker blocked | Warning |
| FRG-E608 | Install FanRGB first | Warning |
| FRG-E609 | Installing FanRGB failed | Error |
| FRG-E610 | Data folder blocked, fan control is off | Error |
| FRG-E611 | Could not start the program without administrator rights | Warning |
| FRG-E612 | Could not restart FanRGB | Warning |
| FRG-E613 | winget is not available | Warning |
| FRG-E614 | Process protection incomplete | Warning |
| FRG-E700 | Update token unusable | Warning |
| FRG-E701 | Update token rejected | Warning |
| FRG-E702 | Could not check for updates | Warning |
| FRG-E703 | Update source is not the FanRGB repository | Error |
| FRG-E704 | Update signature invalid | Error |
| FRG-E705 | Update is not newer | Error |
| FRG-E706 | Update download damaged | Error |
| FRG-E707 | Update package invalid | Error |
| FRG-E708 | Installing the update failed | Error |
| FRG-E709 | Update rolled back | Warning |
| FRG-E710 | Update verified, not installed | Info |
| FRG-E711 | Update check could not run | Warning |
| FRG-E712 | Update needs FanRGB's data folder | Warning |
| FRG-E713 | Update changed, check again | Info |
| FRG-E714 | Update repository not reachable | Warning |
| FRG-E715 | GitHub rate limit reached | Info |
| FRG-E720 | Driver list incomplete | Warning |
| FRG-E721 | No internet connection | Info |
| FRG-E722 | Skipped on a metered connection | Info |
| FRG-E723 | Windows Update is turned off | Warning |
| FRG-E724 | Windows Update did not answer in time | Warning |
| FRG-E725 | Driver check failed | Warning |
| FRG-E726 | Driver check cancelled | Info |
| FRG-E727 | Could not get the vendor driver list | Warning |
| FRG-E728 | Vendor driver list refused | Error |
| FRG-E729 | Vendor driver list out of date | Info |
| FRG-E900 | Internal error | Error |
| FRG-E999 | Unexpected error | Error |

The full explanation of each code is shown in the app and defined in `src/error.rs`.

## Troubleshooting
- **RGB not detected / USB error 43 (FRG-E401)**: the ITE controller is hung, and a reboot does not clear it
  because the board keeps standby power. Shut down, switch the PSU off (or unplug it), press and hold the
  power button for 15 s, wait 30 s, boot. Then press **Reconnect** on the Lighting page.
- **Leftovers of GIGABYTE Control Center**: GCC installs kernel drivers (e.g. `gdrv3.sys`, an AMD Ryzen Master
  driver) that stay loaded after its UI is gone. Uninstall GCC from Settings › Apps and restart; remove a
  leftover driver by hand only if no other tool you use needs it.
- **Wrong colors on a strip**: Lighting › Colour calibration › colour order (most ARGB strips are GRB).
- **Fans don't react**: check that PawnIO is ready and FanRGB runs as administrator (Settings › Hardware),
  that no other fan software runs, and write a diagnostics report (register dump included).
- **"Settings could not be loaded" (FRG-E600)**: the message lists the values that were reset; if the file was
  not JSON at all, the original is in `config.json.bad`.

## Credits
Hardware knowledge from **LibreHardwareMonitor** (MPL-2.0), the **Linux it87** driver (GPL-2.0), **OpenRGB**
(GPL-2.0) and **PawnIO**. The embedded PawnIO modules (`assets/pawnio`) are the official signed binaries from
PawnIO.Modules 0.2.11, unmodified (LGPL-2.1, see `assets/pawnio/COPYING`; their source is in
`assets/pawnio/source`). Some RGB Fusion 2 protocol details and Smart Fan behaviour were learnt by observing
GIGABYTE's own software on this board, for interoperability; no GIGABYTE code or artwork is included.
UI font: **Inter** by Rasmus Andersson (SIL Open Font License 1.1, see `assets/fonts/OFL.txt`).

## Licence
FanRGB is licensed under the [Apache License 2.0](LICENSE), except two files derived from LibreHardwareMonitor
(`src/hw/superio/ite.rs` and `src/hw/superio/mod.rs`), which are under the
[Mozilla Public License 2.0](LICENSE-MPL-2.0). [NOTICE](NOTICE) lists the third-party parts that
`fanrgb.exe` includes and their licences: the PawnIO modules (LGPL-2.1), the Inter font (SIL OFL 1.1), egui's
built-in fonts, the HIDAPI C library and the Rust crates it is built from (texts in `licenses/`,
`THIRD-PARTY-LICENSES.html` and `THIRD-PARTY-LICENSES.txt`). Every release carries these files, and the app
shows all of the texts under Settings › About › Licences. Contributions are welcome: see
[CONTRIBUTING.md](CONTRIBUTING.md). To report a
security problem privately, see [SECURITY.md](SECURITY.md).

## Disclaimer
FanRGB changes how your fans run, through direct access to your motherboard's fan controller. It is provided
"as is", without warranty of any kind (see sections 7 and 8 of the licence). A fan-control mistake can
overheat hardware: watch your temperatures after every change, and if in doubt, stay on the BIOS profile (the
default).

FanRGB is an independent project and is not affiliated with, endorsed by or supported by GIGABYTE, ITE, AMD or
Intel. GIGABYTE, AORUS, RGB Fusion and GIGABYTE Control Center are trademarks of GIGA-BYTE Technology Co.,
Ltd.; other names are trademarks of their respective owners.

## Source layout
```
src/           the app (one exe, fanrgb.exe), see below
tools/         dist.ps1 (build into dist\), release.ps1 + fanrgb-release (signing), remap-paths.ps1, driver-catalog
docs/          BUILDS, RELEASING, KEYS (index: docs/README.md)
catalog/       signed vendor driver catalog (CI-maintained)
assets/        icon, manifest, fonts, PawnIO modules, design renders
dist/          (git-ignored) current\ = the exe to run, archive\ = earlier builds
target/        (git-ignored) cargo build cache only

src/hw/        PawnIO driver, Super I/O detection, ITE chip driver, AMD CPU, GPU, loads, simulator
src/control/   curves, learning thermal controller (model, MPC, supervisor), workload classes, calibration
src/rgb/       RGB Fusion 2 USB protocol, I/O worker + reconnect, effects, audio analysis
src/engine.rs  sensor loop, safety layer, profiles, crash snapshot
src/ui/        egui UI (Overview, Cooling, Intelligence, Lighting, Sensors, Diagnostics, Updates, Settings), tray
src/system.rs  single instance, install/autostart, watchdog, session events
src/updater/   signed self-update: unelevated fetch helper, verification, install, rollback
src/drivers/   driver inventory, Windows Update driver check, signed vendor driver catalog
src/error.rs   FRG-E error codes
```
