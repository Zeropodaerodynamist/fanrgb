# Customising FanRGB for your own PC

FanRGB was built for one PC ([HARDWARE.md](HARDWARE.md)), but it is open source so that anyone can adapt it.
This page covers what you can change without code, where the hardware knowledge lives when you do need code, the
safety rules every change must keep, and a ready-made prompt for an AI coding assistant.

## 1. Without touching code

Most PCs with a supported chip need no code changes at all. In the app:

- **What's connected** (Cooling): tell FanRGB what is on each header (CPU fans, radiator fans, case fans, pump,
  unused). The **Identify your fans** wizard does this for you.
- **Fan modes and your own profiles**: edit any curve (temperature source, points, hysteresis, smoothing,
  minimum and maximum duty), or use Auto (AI) with a temperature target.
- **Calibrate** each fan with a speed signal, so Auto (AI) knows its real airflow.
- **Safety settings** (Settings › Safety): the CPU emergency temperature (70 to 95 °C), where every fan FanRGB
  drives goes to 100 %.
- **Lighting**: zones, LED counts, colour order, per-zone colour correction, LED segments, which 12 V channel
  your board revision uses.

## 2. Adapting the code to another board

Check first with the read-only report (`fanrgb.exe --diag` from an administrator prompt, see
[TESTING.md](TESTING.md)). It tells you which chip, which RGB controller and which board FanRGB found.

| What differs on your PC | Where it lives |
| --- | --- |
| Fan header names, which header is CPU / system / pump, temperature input names, voltage scaling, which headers your board actually routes | `src/hw/board.rs` (`labels_for`, `is_three_fan_board`) |
| A Super I/O chip that is not recognised | `src/hw/superio/ite.rs` (the chip list; this file is MPL-2.0, keep its licence header) |
| RGB controller USB IDs, ARGB header divisions | `src/rgb/fusion2.rs` (`PIDS`, `dled_slots`), `src/rgb/transport.rs` |
| CPU temperature on a non-AMD CPU | `src/hw/amd.rs` reads AMD Zen only; an Intel CPU needs a new source |
| The built-in mode curves | `src/config.rs` (`FanConfig::preset`) |
| Safety thresholds | `src/control/limits.rs`: **do not lower these** |

The demo backend (`src/hw/sim.rs`, `--demo`) and the tests (`cargo test --release --locked`) let you develop
without touching your hardware.

## 3. Safety rules for every change

1. **Never write to a chip that is not positively identified.** Add a chip ID only with the register layout
   checked against its datasheet or a trusted open-source driver (LibreHardwareMonitor, Linux it87).
2. **Never weaken the safety design:** the board CPU and PUMP header minimums, the hard cooling floor near the
   critical temperature, the critical-temperature latch, the chip failsafe, the crash and hang watchdogs, and
   the hand-back to the BIOS on exit.
3. **Read-only first.** A new board starts with `--diag` and the BIOS mode; compare every reading with HWiNFO
   before any fan is driven.
4. **One header at a time** on real hardware, with temperatures in view, and **Return fans to BIOS**
   (tray menu, or Ctrl+Shift+B in the FanRGB window) within reach.
5. **Tests stay green:** `cargo fmt --all --check`, `cargo clippy --release --locked`, `cargo test --release
   --locked`. Add tests for what you change.
6. **Licences:** keep the Apache-2.0 and MPL-2.0 headers; do not copy code from GPL projects (reading them to
   learn how a chip works is fine).

## 4. A prompt for your AI coding assistant

Clone the repository, open it in your assistant (for example Claude Code), run the read-only report, and paste
this, filling in the brackets:

```text
I want to adapt FanRGB (this repository) to my PC. My PC: motherboard [maker and model, BIOS version],
CPU [model], graphics [model], cooling [air cooler or AIO, what is plugged into which fan header],
lighting [ARGB / RGB devices on which headers]. Attached is the output of `fanrgb.exe --diag`
(run from an administrator prompt, read-only): [paste diag.txt].

Please:
1. Read README.md, docs/HARDWARE.md, docs/TESTING.md, docs/CUSTOMISING.md and docs/SECURITY-AUDIT.md first.
2. From my diag report, tell me which chip and RGB controller FanRGB found, whether my board is already
   supported, and what is missing or mislabelled. If my chip or controller is not supported, say so plainly
   and tell me what information would be needed, instead of guessing register layouts.
3. Propose the smallest change (normally in src/hw/board.rs, or the chip list in src/hw/superio/ite.rs,
   or src/rgb/fusion2.rs), explain it, and wait for my go-ahead before editing.
4. Follow the safety rules in docs/CUSTOMISING.md section 3. Never lower or remove any safety floor, latch,
   failsafe or watchdog, and never change src/control/limits.rs.
5. Add or extend tests for the change and run: cargo fmt --all --check, cargo clippy --release --locked,
   cargo test --release --locked. Show me the results.
6. Never run fanrgb.exe in a fan-controlling mode yourself. Give me a step-by-step test plan for my PC
   based on docs/TESTING.md (demo, read-only report, BIOS mode, identify fans, one mode at a time, check the
   hand-back to the BIOS), and tell me exactly what to watch and when to stop.
7. Keep the licence headers (Apache-2.0, and MPL-2.0 in src/hw/superio/ite.rs and mod.rs).
```

## 5. Sharing your work

If your board works, please open a pull request with the change, your `--diag` report (it leaves out your Windows
account name, but read it before you share it) and the board model and BIOS version. Every tested board makes FanRGB safer for the
next person.
