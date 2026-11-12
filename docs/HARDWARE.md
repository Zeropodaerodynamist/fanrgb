# Supported hardware and requirements

FanRGB 0.2.0 is a **beta**. It drives your fans directly through the motherboard's fan chip, so read this page
before you run it, and start with the read-only report (`fanrgb.exe --diag`) on any PC that is not the
reference PC below.

## The reference PC (tested on real hardware)

This is the only PC FanRGB has been tested on with real fans and lights.

| Part | Exact model |
| --- | --- |
| Motherboard | GIGABYTE **B650M D3HP AX**, revision 1.3, BIOS F36d (AMI, 2025-09-18) |
| Fan and sensor chip (Super I/O) | ITE **IT8689E** |
| RGB controller | ITE **IT5702** (USB 048D:5702, RGB Fusion 2) |
| CPU | AMD **Ryzen 5 7500F** (Zen 4, AM5) |
| Graphics | Intel **Arc B580** 12 GB |
| Cooling as tested | AIO liquid cooler: pump on CPU_FAN (no speed signal), radiator fans on SYS_FAN1, case fans on SYS_FAN2 |
| Lighting as tested | ARGB fans on D_LED1, pump head on D_LED2 |
| Operating system | Windows 11 Pro (64-bit), build 26300 |
| Driver | PawnIO 2.2.0.0 with the PawnIO.Modules 0.2.11 modules built into FanRGB |

Fan headers on this board, as FanRGB names them, and the floor each keeps whatever the settings say:

| Header | Board label | Minimum in every mode except BIOS |
| --- | --- | --- |
| `mb.pwm0` | CPU_FAN | 20 % (30 % when it is set as the pump, 60 % while its device is unknown) |
| `mb.pwm1` | SYS_FAN1 | none of its own (the role decides) |
| `mb.pwm2` | SYS_FAN2 | none of its own |
| `mb.pwm3` | SYS_FAN3 | none of its own |
| `mb.pwm4` | CPU_OPT | 20 % |
| `mb.pwm5` | SYS_FAN4 / PUMP | 30 % |

Close to the critical CPU temperature every fan FanRGB drives also has a hard minimum (up to 75 %), and at the
critical temperature itself everything goes to 100 %.

### What was checked on it (0.2.0 beta, 12 November 2026)

The release exe ran on this PC in place of the installed copy, at idle (CPU 37 to 40 °C), and was switched
through every mode with its keyboard shortcuts. Fan speeds are the radiator fans (SYS_FAN1) and case fans
(SYS_FAN2); the pump has no speed signal.

| Check | Measured | Result |
| --- | --- | --- |
| BIOS baseline, FanRGB closed | 870 and 1,810 RPM | reference |
| Silent | 505 and 1,390 RPM | quieter than the BIOS |
| Balanced | 592 and 1,655 RPM | as set |
| Auto (AI), fans not calibrated | 670 to 780 and 1,550 to 1,710 RPM | as set |
| Full speed | 1,515 and 3,290 RPM (3.0 and 2.4 times Silent) | pass |
| BIOS mode | 768 and 1,667 RPM (0.88 and 0.92 of the baseline, CPU 2 °C cooler) | pass |
| Back to Silent | 505 and 1,316 RPM | pass |
| Pump floor | the CPU_FAN chip shows "Min 30 %" (checked on the previous build; the floor code is unchanged) | pass |
| Hand-back on exit | fan-control registers (bank 0 rows 0x10, 0x60, 0x70, 0xA0) identical to before | pass |
| Privacy | no account name or full account SID in the reports or the log | pass |

Not yet checked on real hardware: the chip's own failsafe at 90 °C, the crash watchdog's recovery,
sleep and resume, and the lighting after a power drain.

## Requirements

| | Needed |
| --- | --- |
| Windows | Windows 10 or 11, 64-bit (x64). Not ARM. |
| Rights | An administrator account: FanRGB asks for administrator rights (UAC) to reach the fan chip. |
| Driver | **PawnIO** (free, signed, open source): `winget install namazso.PawnIO`, or <https://pawnio.eu/>. Without it FanRGB can still show the GPU and run the lighting, but not fans or CPU and board temperatures. |
| CPU | AMD Ryzen (Zen, Zen 2 to Zen 5: family 17h, 19h, 1Ah) for CPU temperature and package power. Intel CPUs are not supported for CPU temperature. |
| Motherboard | A GIGABYTE board with an ITE Super I/O chip that FanRGB knows (list below). Only the B650M D3HP AX has been tested. |
| Graphics | Any vendor (temperature, load and fan through the same Windows source Task Manager uses). |
| Other fan or RGB software | Must be closed: GIGABYTE Control Center, RGB Fusion, EasyTune, SIV, FanControl, OpenRGB, SignalRGB, iCUE, Armoury Crate. Two programs writing to the same chip fight each other. FanRGB's Diagnostics page lists any it finds. |
| Disk | About 15 MB for the program, plus a few MB of settings, logs and learned models. |
| Network | None needed. The optional daily update check talks to GitHub only (see the README). |

## Chips FanRGB recognises

FanRGB only writes to a chip whose ID is on this closed list and whose vendor and configuration checks pass:

IT8613E, IT8620E, IT8625E, IT8628E, IT8631E, IT8638E, IT8655E, IT8665E, IT8686E, IT8688E, **IT8689E** (tested),
IT8696E, IT8705F, IT8712F, IT8716F, IT8718F, IT8720F, IT8721F, IT8726F, IT8728F, IT8771E, IT8772E, IT8790E,
IT8792E, IT87952E.

RGB Fusion 2 USB controllers: 048D:5702 (**tested**), 048D:8297, 048D:5711, 048D:8950.

Recognised is not the same as tested. Fan labels and sensor names follow GIGABYTE's AM5 layout, so on another
board a header can carry the wrong name. Check what `--diag` reports against the board manual and against
HWiNFO before you pick any mode other than BIOS.

## What FanRGB never does

- It never installs or loads a kernel driver of its own, and never uses WinRing0 or GIGABYTE's drivers.
- It never changes a fan until you pick a mode other than BIOS, and every mode except BIOS waits until each
  header has been identified.
- It never leaves the fans in manual mode when it exits, crashes or hangs: the chip's own curve comes back
  (crash watchdog, exit handling, hang watchdog).

See [TESTING.md](TESTING.md) for how to try it safely on your own PC.
