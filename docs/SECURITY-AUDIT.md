# Security audit before publication (November 2026)

Before FanRGB's source was published, an independent security reviewer (Argus, an AI security agent working
in an isolated offline machine, with no part in writing FanRGB) audited the tree exactly as it would be
published. This page summarises what was found and what was done about it.

## Scope and method

- **Code:** the whole publishable tree (214 files): the app, the release tool, the driver-catalogue tooling, CI
  workflows, documentation and licences. No vendor material and no keys were part of it.
- **Threat model:** FanRGB runs with administrator rights and reaches the motherboard through the PawnIO driver.
  The attackers considered were a program running as the same user without administrator rights, bad data in
  any file the elevated app reads (settings, calibration, crash snapshot, learned models), a network attacker or
  anyone controlling what GitHub serves, a vendor web page feeding the driver catalogue, and a contributor with
  push access.
- **Method:** reading every `unsafe` block and every path from outside input to the hardware, proofs of concept
  on the unmodified code, and fuzzing: about 180,000 engine ticks over 3,000 hostile settings files, 524,000
  USB HID reports, 400,000 inputs to the update and catalogue parsers, and 200,000 cases of the fan-curve
  maths. Every fix was retested against its proof of concept.
- **Limits:** the auditor's machine runs Linux on ARM, so Windows-only code was reviewed by reading. The fixes
  were then applied and tested on Windows on the reference PC ([HARDWARE.md](HARDWARE.md)) by the maintainers,
  with a separate tester (see *Verification*).

## Results

**48 findings: none Critical, none High, 8 Medium, 25 Low, 15 informational.**

- **Memory safety:** no defect found in about 280 `unsafe` uses; no panic in any fuzzing run.
- **Hardware access:** no path from any outside input to a chip port, register index or driver call. Register
  indices are constants or small tables indexed by a bounded channel number, and only a closed list of chips
  is written to.
- **The weak spots were about values, not addresses:** a settings file that any program of the user can edit
  could hold fans low until the critical-temperature cut-out. Those are fixed (below).
- **Updates:** no way was found to get an unsigned or altered update or catalogue accepted.

### The 8 Medium findings

| ID | Finding | Status |
| --- | --- | --- |
| FR-CTL-001 | No minimum cooling below the critical cut-out that the settings cannot switch off | Fixed: a hard floor from 12 °C below critical, up to 75 % |
| FR-CTL-002 | Stored fan calibration tables were not validated | Fixed: range-checked, bad tables not used |
| FR-CTL-003 | With the CPU sensor lost, other fan rules carried on while the cut-out was blind | Fixed: fans go back to the BIOS |
| FR-HW-001 | Curves with points under 1 °C apart were written to the chip too low | Fixed |
| FR-PRIV-001 | The running-program check trusted the file it was judging | Fixed |
| FR-PRIV-003 | "Open Task Manager" started it with administrator rights | Fixed: started without them |
| FR-PUB-001 | Material that must never be published was not in `.gitignore` | Fixed; history checked clean; published with a fresh history |
| FR-UPD-001 | A signed release never expires and cannot be withdrawn | Known limit, see below |

Of the 48 findings, 33 are fixed, 2 are repository settings applied at publication, 9 are informational and
need no change, 1 did not apply to the repository, and 3 are known limits. The Low fixes include: the board's
CPU and pump headers keep a minimum speed whatever the settings file says, crash-snapshot values and chip
curves are checked before use, the diagnostics report hides the Windows account name, the update helper
accepts only read-only fine-grained tokens, and the catalogue fetcher caps decompressed size. The full list
is in the [CHANGELOG](../CHANGELOG.md) (0.2.0, *Security*).

## Known limits in 0.2.0

- **Release expiry (FR-UPD-001).** A signed release cannot be withdrawn. Until the signing-key ceremony is
  done, no GitHub release is published, so the in-app updater has nothing to offer. After it, releases will
  be signed for the updater only when promoted, and a signed freshness file will follow.
- **Shared hardware locks (FR-HW-006).** The system-wide locks FanRGB shares with HWiNFO and
  LibreHardwareMonitor can be held by any program. This fails safe (FanRGB hands the fans back to the BIOS);
  tighter permissions need compatibility testing with those tools first.
- **Stuck sensor (FR-CTL-007).** A CPU temperature that freezes at a believable value is not detected yet;
  the thresholds need measuring on real hardware.
- **Roles in the settings file.** The board's CPU and PUMP headers keep their minimum speed whatever role the
  settings give them, but a role changed in the file still takes such a header out of the cooling-failure
  alarms, and lets the Calibrate button (which the user has to press) sweep it down to 0 % for about a
  minute. Moving the roles into the administrator-only state folder is planned.
- **Implausible but consistent calibration tables.** A stored table that is monotone but claims a fan
  reaches nearly full speed at a very low duty makes Auto (AI) pick low duties for high airflow requests.
  Full airflow, the AI's safety rise and the hard floor still reach full speed. Tighter bounds risk refusing
  real fans with a high minimum speed, so this waits for measurements on more fans. Similarly, a calibration
  of a very slow-spinning fan that had not settled at each step can over-promise mid-range airflow by up to
  about 9 % of full speed; requests from about 94 % upwards still map to full duty.

## Verification on Windows

- Combined fixes applied on the reference PC: formatting, lints, the full test suite (516 tests), `cargo
  audit`, `cargo deny` and the release build all pass.
- A separate tester, which wrote none of the code, added its own tests for the fixes (about 40, including
  randomised sweeps of thousands of fan curves across critical temperatures from 60 to 95 °C) and re-ran
  every check. Over four rounds it found nine further holes in the fixes (in the calibration-table checks, the
  calibration wizard, and how the hard floor reaches curves the fan chip runs itself); each was fixed and
  re-tested until its verdict was a pass, with two Low items documented above. Final run: 558 tests passed,
  0 failed.
- Hardware: see [HARDWARE.md](HARDWARE.md) for the PC the beta was tested on.

## Reporting a problem

Please report security problems privately, as described in [SECURITY.md](../SECURITY.md).
