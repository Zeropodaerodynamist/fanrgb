# Security policy

FanRGB runs with administrator rights, talks to the motherboard's fan controller through the PawnIO driver,
starts with Windows through a scheduled task and can update itself. A bug in any of those can reach further
than FanRGB, so please report security problems privately.

## Reporting a vulnerability

Use GitHub's private reporting: open this repository's **Security** tab and choose **Report a vulnerability**.
Please don't open a public issue, pull request or discussion for a security problem.

Helpful to include: the FanRGB version, your Windows version, what an attacker needs first (for example a
standard user account on the same PC), the steps to reproduce, and what they gain.

FanRGB is a small volunteer project. Expect an acknowledgement within a week, on a best-effort basis. A fix
ships as a signed release, and the advisory is published once that release is out.

## Supported versions

Only the latest release gets security fixes.

## In scope

- Gaining administrator rights or SYSTEM through FanRGB: its Program Files copy, the startup task, the
  watchdog, or files in `%ProgramData%\FanRGB`.
- The updater: anything that makes FanRGB install a release that is not signed by a key compiled into
  `src/updater/keys.rs` (see [docs/KEYS.md](docs/KEYS.md)), or install an older release than it should.
- The driver catalog: anything that makes FanRGB show or link to a host that is not in
  `catalog/allowed-hosts.json`, or accept a catalog not signed by a catalog key.
- Hardware safety: a way to leave fans stopped or below the safety floor, or to stop FanRGB handing the fans
  back to the BIOS when it exits or crashes.

## Out of scope

- The PawnIO driver and its modules: please report those to [PawnIO](https://pawnio.eu/).
- Attacks that already need administrator rights on the PC.
- Fan and RGB tools from other vendors fighting FanRGB for the same chip (see *Install* in the README).
