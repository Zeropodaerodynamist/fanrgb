// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.
// Parts of this file are derived from LibreHardwareMonitor's IT87XX.cs:
// Copyright (C) LibreHardwareMonitor and Contributors.
// Partial Copyright (C) Michael Möller <mmoeller@openhardwaremonitor.org> and Contributors.
// All Rights Reserved.
// Changes and additions: Copyright 2026 The FanRGB Authors.

//! ITE IT86xx/IT87xx environment controller (hardware monitor + fan control).
//!
//! Register layout follows LibreHardwareMonitor's `IT87XX.cs` (parts of this file are derived from it,
//! hence MPL-2.0), the Linux `it87` driver and, for IT8689E-class ("V3") chips on GIGABYTE boards,
//! the behaviour of GIGABYTE's own software.
//!
//! On V3 chips fans are never put into software PWM mode: like GIGABYTE Control Center we write
//! the chip's SmartGuardian automatic curve (bank 0 main curve + bank 2/3 extra vectors) and keep
//! the channel in automatic mode, so the chip keeps running a safe curve (with a 100 % failsafe
//! above Tfull) if the app hangs or crashes. A fixed duty is a flat curve with a ramp to 100 % at
//! the failsafe temperature. Raw software PWM is only used on older chips, or as a fallback when a
//! curve write does not verify.
//!
//! Every register write is read back; register values we already know to be in place are not
//! written again, which keeps bus traffic at 1 Hz polling low.

use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::PortIo;
use super::sg::{self, ChipCurve, CurveImage, SG_BASE, SG_PWM, SG_TFULL, SG_TSTART};
use crate::error::{AppError, ErrorCode, code_of, err};

const ADDRESS_REG_OFFSET: u16 = 0x05;
const DATA_REG_OFFSET: u16 = 0x06;

const REG_CONFIGURATION: u8 = 0x00;
const REG_BANK: u8 = 0x06;
const REG_FAN_TACH_DIVISOR: u8 = 0x0B;
const REG_FAN_16BIT: u8 = 0x0C;
/// Only written to undo changes made by older FanRGB builds (legacy crash snapshots).
const REG_FAN_MAIN_CTRL: u8 = 0x13;
const REG_TEMPERATURE_BASE: u8 = 0x29;
const REG_VENDOR_ID: u8 = 0x58;
const ITE_VENDOR_IDS: [u8; 2] = [0x90, 0x7F];

const REG_VIN: [u8; 13] = [0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x2f, 0x2c, 0x2d, 0x2e];
const REG_PWM_DUTY: [u8; 6] = [0x63, 0x6b, 0x73, 0x7b, 0xa3, 0xab];
const REG_FAN_TACH: [u8; 6] = [0x0d, 0x0e, 0x0f, 0x80, 0x82, 0x4c];
const REG_FAN_TACH_EXT: [u8; 6] = [0x18, 0x19, 0x1a, 0x81, 0x83, 0x4d];
const REG_FAN_TACH_ALT: [u8; 6] = [0x0d, 0x0e, 0x0f, 0x80, 0x82, 0x93];
const REG_FAN_TACH_EXT_ALT: [u8; 6] = [0x18, 0x19, 0x1a, 0x81, 0x83, 0x94];

/// PWM control register: 1 = SmartGuardian automatic mode.
const CTRL_AUTO: u8 = 0x80;
/// PWM control register: tachometer closed-loop (RPM) mode. Never set by us.
const CTRL_CLOSED_LOOP: u8 = 0x04;
/// Bits of the PWM control register GIGABYTE's software leaves alone (6, 1:0).
const CTRL_KEEP: u8 = 0x43;

/// Polls between full (all banks) verifications of a channel we drive.
const FULL_VERIFY_EVERY: u32 = 30;
/// Consecutive polls with a firmware override before the channel is given back to the BIOS.
const MAX_OVERRIDES: u8 = 5;
/// How long a channel stays with the BIOS after the firmware kept overriding it.
const GIVE_UP_FOR: Duration = Duration::from_secs(60);
/// Attempts of a verified curve write / a release before it counts as failed (a single read-back
/// can be corrupted by another program accessing the chip at the same time).
const WRITE_ATTEMPTS: u32 = 3;
/// First pause of hardware curves on a channel after its curve registers did not verify; doubles
/// with every further failure up to [`CURVE_BACKOFF_MAX`].
const CURVE_BACKOFF: Duration = Duration::from_secs(30);
const CURVE_BACKOFF_MAX: Duration = Duration::from_secs(600);
/// A failed release that polling retries is reported again every this many polls.
const RELEASE_REPORT_EVERY: u32 = 30;
/// Polls between re-reads of a BIOS-controlled channel's firmware curve (for the duty estimate).
const FW_CURVE_REFRESH: u32 = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IteModel {
    IT8613E,
    IT8620E,
    IT8625E,
    IT8628E,
    IT8631E,
    IT8638E,
    IT8655E,
    IT8665E,
    IT8686E,
    IT8688E,
    IT8689E,
    IT8696E,
    IT8705F,
    IT8712F,
    IT8716F,
    IT8718F,
    IT8720F,
    IT8721F,
    IT8726F,
    IT8728F,
    IT8771E,
    IT8772E,
    IT8790E,
    IT8792E,
    IT87952E,
}

impl IteModel {
    pub fn from_chip_id(id: u16) -> Option<Self> {
        use IteModel::*;
        Some(match id {
            0x8613 => IT8613E,
            0x8620 => IT8620E,
            0x8625 => IT8625E,
            0x8628 => IT8628E,
            0x8631 => IT8631E,
            0x8638 => IT8638E,
            0x8655 => IT8655E,
            0x8665 => IT8665E,
            0x8686 => IT8686E,
            0x8688 => IT8688E,
            0x8689 => IT8689E,
            0x8696 => IT8696E,
            0x8705 => IT8705F,
            0x8712 => IT8712F,
            0x8716 => IT8716F,
            0x8718 => IT8718F,
            0x8720 => IT8720F,
            0x8721 => IT8721F,
            0x8726 => IT8726F,
            0x8728 => IT8728F,
            0x8771 => IT8771E,
            0x8772 => IT8772E,
            0x8790 => IT8790E,
            0x8733 => IT8792E,
            0x8695 => IT87952E,
            _ => return None,
        })
    }

    pub fn name(self) -> String {
        format!("{self:?}")
    }

    /// Secondary chips that Gigabyte drives through an embedded controller.
    pub fn is_secondary_ec(self) -> bool {
        matches!(self, IteModel::IT8790E | IteModel::IT8792E | IteModel::IT87952E)
    }
}

#[derive(Clone, Debug)]
pub struct Caps {
    pub volts: usize,
    pub temps: usize,
    pub fans: usize,
    pub pwms: usize,
    /// 8-bit duty in the separate PWM duty registers (newer chips).
    pub ext_duty: bool,
    pub banks: u8,
    pub volt_gain: f32,
    pub fan16: bool,
    pub alt_6th_fan: bool,
    pub pwm_ctrl: Vec<u8>,
    /// Address readback does not echo (IT8688E quirk).
    pub no_addr_verify: bool,
    pub force_bank0: bool,
    /// IT8689E-class register layout with 4 banks and SmartGuardian extra vectors in banks 2/3
    /// (GIGABYTE treats IT8689E, IT8696E and IT8688E version 2 alike).
    pub v3: bool,
}

impl Caps {
    pub fn for_model(m: IteModel, version: u8) -> Caps {
        use IteModel::*;
        let (volts, temps, fans, pwms) = match m {
            IT8613E => (10, 4, 5, 4),
            IT8625E => (7, 3, 6, 6),
            IT8628E => (10, 6, 6, 6),
            IT8631E | IT8638E => (9, 2, 2, 2),
            IT8665E => (9, 6, 6, 6),
            IT8686E => (10, 7, 6, 5),
            IT8688E => (11, 6, 6, 5),
            IT8689E | IT8696E => (10, 6, 6, 6),
            IT87952E | IT8655E | IT8792E | IT8705F => (9, if m == IT8655E { 6 } else { 3 }, 3, 3),
            IT8620E => (9, 3, 5, 5),
            _ => (9, 3, 5, 3),
        };
        let ext_duty = matches!(
            m,
            IT8721F
                | IT8728F
                | IT8665E
                | IT8686E
                | IT8688E
                | IT8689E
                | IT87952E
                | IT8628E
                | IT8625E
                | IT8620E
                | IT8613E
                | IT8792E
                | IT8655E
                | IT8631E
                | IT8638E
                | IT8696E
                | IT8772E
        );
        let volt_gain = match m {
            IT8613E | IT8620E | IT8628E | IT8631E | IT8638E | IT8721F | IT8728F | IT8771E | IT8772E | IT8686E
            | IT8688E | IT8689E | IT8696E => 0.012,
            IT8625E | IT8792E | IT87952E => 0.011,
            IT8655E | IT8665E => 0.0109,
            _ => 0.016,
        };
        let pwm_ctrl = match m {
            IT8665E | IT8625E => vec![0x15, 0x16, 0x17, 0x1e, 0x1f, 0x92],
            IT8792E => vec![0x15, 0x16, 0x17],
            _ => vec![0x15, 0x16, 0x17, 0x7f, 0xa7, 0xaf],
        };
        let v3 = matches!(m, IT8689E | IT8696E) || (m == IT8688E && version == 2);
        Caps {
            volts,
            temps,
            fans,
            pwms: pwms.min(pwm_ctrl.len()),
            ext_duty,
            banks: if v3 { 4 } else { 1 },
            volt_gain,
            fan16: (m != IT8705F || version >= 3) && (m != IT8712F || version >= 8),
            alt_6th_fan: matches!(m, IT8665E | IT8625E),
            pwm_ctrl,
            no_addr_verify: m == IT8688E && version != 2,
            force_bank0: matches!(m, IT8665E | IT8655E),
            v3,
        }
    }
}

/// Firmware register values captured before taking control of a channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedPwm {
    pub ctrl: u8,
    pub duty: u8,
    /// Bit `index` of FAN_MAIN_CTRL. Only present in snapshots of older builds, which changed it.
    #[serde(default)]
    pub main_bit: Option<bool>,
    /// Gigabyte extra-vector enable registers (bank2 x3, bank3 x1); kept for older snapshots.
    #[serde(default)]
    pub extvec: Option<[u8; 4]>,
    /// Every SmartGuardian curve register of the channel in banks 0, 2 and 3 (V3 chips).
    #[serde(default)]
    pub image: Option<CurveImage>,
}

/// Who runs a channel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChanMode {
    /// BIOS/EC settings, untouched.
    #[default]
    Firmware,
    /// Our curve, executed by the chip (automatic mode).
    Curve,
    /// Software PWM at a fixed duty (old chips, or fallback).
    Manual,
}

#[derive(Clone, Debug, Default)]
struct Chan {
    saved: Option<SavedPwm>,
    /// The saved state came from a crash-snapshot file and is validated before use.
    untrusted: bool,
    mode: ChanMode,
    /// Register state last read or written-and-verified (ctrl, image); V3 chips with extra vectors.
    known: Option<(u8, CurveImage)>,
    /// (ctrl, duty) last read or written-and-verified; other chips.
    known_manual: Option<(u8, u8)>,
    /// What the channel should look like while we drive it (V3 path).
    target: Option<(u8, CurveImage)>,
    /// Temperature input and curve the chip runs (duty estimate).
    curve: Option<(u8, ChipCurve)>,
    manual_duty: Option<u8>,
    overrides: u8,
    polls: u32,
    /// Consecutive curve write failures (read-back mismatch); drives the back-off.
    curve_failures: u32,
    /// Hardware curves are paused on this channel until then (after a curve write did not verify).
    curve_retry_at: Option<Instant>,
    manual_failed: bool,
    gave_up_at: Option<Instant>,
    /// Handing the channel back to the BIOS failed: `saved` is kept and polling retries the release
    /// (it never re-applies our own settings, which would look like a firmware override).
    release_pending: bool,
    release_failures: u32,
    /// BIOS-controlled channel: (control value it was read with, temperature input, decoded firmware
    /// curve) and the poll it was read at, for the duty estimate.
    fw_curve: Option<(u8, u8, ChipCurve)>,
    fw_curve_poll: u32,
}

impl Chan {
    /// Hardware curves are paused after a failed curve write.
    fn curve_paused(&self) -> bool {
        self.curve_retry_at.is_some_and(|t| Instant::now() < t)
    }
}

#[derive(Clone, Debug, Default)]
pub struct IteReadings {
    pub volts: Vec<Option<f32>>,
    /// Raw temperatures (the plausibility filter in `hw::filter` decides what is valid).
    pub temps: Vec<Option<f32>>,
    pub fans: Vec<Option<f32>>,
    /// Duty in percent: the written duty for software-mode channels, the curve's output for our
    /// hardware curves, `None` when the BIOS curve runs.
    pub duty: Vec<Option<f32>>,
    /// While the BIOS curve runs: its output at the current input temperature, from the decoded
    /// firmware curve (an estimate; `None` otherwise or when unknown).
    pub duty_est: Vec<Option<f32>>,
}

pub struct IteChip {
    io: Box<dyn PortIo>,
    pub model: IteModel,
    pub version: u8,
    pub base: u16,
    pub caps: Caps,
    addr_port: u16,
    data_port: u16,
    fan_disabled: [bool; 6],
    extvec: bool,
    controllable: bool,
    chans: Vec<Chan>,
    events: Vec<AppError>,
    /// The app's id prefix for this chip's channels ("mb", "mb2"), so log lines name both the chip's
    /// 1-based PWM number and the app's 0-based id.
    id_prefix: String,
    polls: u32,
    /// A fan whose hardware curve does not verify may fall back to software PWM (no hardware
    /// failsafe) only while crash protection (watchdog + snapshot) is active; otherwise such a fan
    /// goes back to the BIOS.
    software_fallback: bool,
}

/// PWM control value for automatic mode on temperature input `src`, keeping the bits GIGABYTE's
/// software keeps (only from a sane firmware value).
fn ctrl_auto(saved: u8, src: u8) -> u8 {
    let keep = if saved & CTRL_AUTO != 0 { saved & CTRL_KEEP } else { 0 };
    CTRL_AUTO | keep | ((src & 7) << 3)
}

fn valid_src(ctrl: u8) -> u8 {
    let s = (ctrl >> 3) & 7;
    if s <= 5 { s } else { sg::SAFE_DEFAULT_INPUT }
}

fn plausible_temp(t: f32) -> bool {
    crate::hw::filter::in_range(t)
}

impl IteChip {
    /// Validates the environment controller at `base` and prepares the driver.
    pub fn new(io: Box<dyn PortIo>, model: IteModel, version: u8, base: u16, gigabyte: bool) -> Result<Self> {
        let caps = Caps::for_model(model, version);
        let extvec = gigabyte && caps.v3;
        // Gigabyte's secondary IT879x chips are driven by an EC; writing them from here
        // would fight the firmware, so they are monitored read-only.
        let controllable = !(gigabyte && model.is_secondary_ec());
        let mut chip = IteChip {
            io,
            model,
            version,
            base,
            caps,
            addr_port: base + ADDRESS_REG_OFFSET,
            data_port: base + DATA_REG_OFFSET,
            fan_disabled: [false; 6],
            extvec,
            controllable,
            chans: vec![Chan::default(); 6],
            events: Vec::new(),
            id_prefix: "mb".into(),
            polls: 0,
            software_fallback: false,
        };

        let vendor = chip.read(REG_VENDOR_ID)?;
        if !ITE_VENDOR_IDS.contains(&vendor) {
            return Err(err(
                ErrorCode::ChipInvalid,
                format!("{} at {:#06x}: unexpected vendor id {:#04x}", model.name(), base, vendor),
            ));
        }
        let cfg = chip.read(REG_CONFIGURATION)?;
        if cfg & 0x10 == 0 && !matches!(model, IteModel::IT8655E | IteModel::IT8665E) {
            return Err(err(
                ErrorCode::ChipInvalid,
                format!("{} at {:#06x}: configuration register {:#04x} looks invalid", model.name(), base, cfg),
            ));
        }
        if chip.caps.fan16 {
            let modes = chip.read(REG_FAN_16BIT)?;
            if chip.caps.fans >= 5 {
                chip.fan_disabled[3] = modes & (1 << 4) == 0;
                chip.fan_disabled[4] = modes & (1 << 5) == 0;
            }
            if chip.caps.fans >= 6 {
                chip.fan_disabled[5] = if model == IteModel::IT8665E {
                    chip.read(REG_FAN_TACH_DIVISOR)? & (1 << 3) == 0
                } else {
                    modes & (1 << 2) == 0
                };
            }
        }
        Ok(chip)
    }

    pub fn controllable(&self) -> bool {
        self.controllable
    }

    /// The chip can run our curves itself (GIGABYTE IT8689E-class chips).
    pub fn hw_curves(&self) -> bool {
        self.extvec && self.controllable
    }

    /// `set_duty`/`set_curve` keep the chip's own 100 % failsafe active on this channel.
    pub fn has_hw_failsafe(&self, idx: usize) -> bool {
        self.hw_curves()
            && idx < self.caps.pwms
            && !self.chans[idx].curve_paused()
            && self.chans[idx].mode != ChanMode::Manual
    }

    /// Allows or forbids the software-PWM fallback of `set_duty` (see `software_fallback`).
    pub fn set_software_fallback(&mut self, allowed: bool) {
        self.software_fallback = allowed;
    }
    #[allow(dead_code)]
    pub fn mode(&self, idx: usize) -> ChanMode {
        self.chans.get(idx).map(|c| c.mode).unwrap_or_default()
    }

    pub fn fan_present(&self, idx: usize) -> bool {
        idx < self.caps.fans && !self.fan_disabled[idx]
    }

    /// Notable events since the last call (firmware overrides, failed verifications).
    pub fn take_events(&mut self) -> Vec<AppError> {
        std::mem::take(&mut self.events)
    }

    /// Sets the app's channel id prefix ("mb" for the first chip, "mb2", …).
    pub fn set_id_prefix(&mut self, prefix: &str) {
        self.id_prefix = prefix.to_string();
    }

    /// "IT8689E PWM1 (mb.pwm0)": the chip's 1-based name and the app's 0-based id.
    pub fn label(&self, idx: usize) -> String {
        format!("{} PWM{} ({}.pwm{idx})", self.model.name(), idx + 1, self.id_prefix)
    }

    /// The decoded firmware curve of a BIOS-controlled channel (input, curve), if read.
    pub fn firmware_curve(&self, idx: usize) -> Option<(u8, ChipCurve)> {
        self.chans.get(idx).and_then(|c| c.fw_curve.as_ref()).map(|(_, i, c)| (*i, c.clone()))
    }

    // ---- raw register access -------------------------------------------------

    fn read(&mut self, reg: u8) -> Result<u8> {
        for _ in 0..2 {
            self.io.outb(self.addr_port, reg)?;
            let value = self.io.inb(self.data_port)?;
            if self.caps.no_addr_verify || self.io.inb(self.addr_port)? == reg {
                return Ok(value);
            }
        }
        Err(err(
            ErrorCode::ChipInvalid,
            format!(
                "{} register {reg:#04x}: address read-back mismatch (another program may be using the chip)",
                self.model.name()
            ),
        ))
    }

    fn write(&mut self, reg: u8, value: u8) -> Result<()> {
        // Make sure the address latch holds `reg` before the data write: another program accessing
        // the chip in between would otherwise send the value to a different register.
        let mut latched = false;
        for _ in 0..2 {
            self.io.outb(self.addr_port, reg)?;
            if self.caps.no_addr_verify || self.io.inb(self.addr_port)? == reg {
                latched = true;
                break;
            }
        }
        if !latched {
            return Err(err(
                ErrorCode::ChipInvalid,
                format!(
                    "{} register {reg:#04x}: address read-back mismatch before a write (another program may be using the chip)",
                    self.model.name()
                ),
            ));
        }
        self.io.outb(self.data_port, value)?;
        let _ = self.io.inb(self.addr_port)?;
        Ok(())
    }

    /// Writes and reads back; a different value is a `RegisterMismatch`.
    fn write_verified(&mut self, reg: u8, value: u8) -> Result<()> {
        self.write(reg, value)?;
        let got = self.read(reg)?;
        if got != value {
            return Err(err(
                ErrorCode::RegisterMismatch,
                format!("{} register {reg:#04x}: wrote {value:#04x}, read back {got:#04x}", self.model.name()),
            ));
        }
        Ok(())
    }

    /// Writes a PWM control register. On V3 chips we never set closed-loop mode or a nonexistent
    /// temperature input (e.g. the 0x7F some tools write); only an automatic-mode value taken from
    /// the firmware is restored verbatim.
    fn write_ctrl(&mut self, idx: usize, v: u8) -> Result<()> {
        if self.caps.v3 {
            let sane = v & CTRL_CLOSED_LOOP == 0 && (v >> 3) & 7 <= 5;
            // Only a value we read from the chip ourselves counts as firmware state (not one from a
            // crash-snapshot file).
            let firmware =
                v & CTRL_AUTO != 0 && !self.chans[idx].untrusted && self.chans[idx].saved.is_some_and(|s| s.ctrl == v);
            if !sane && !firmware {
                return Err(err(
                    ErrorCode::Internal,
                    format!("{}: refusing to write unsafe PWM control value {v:#04x}", self.label(idx)),
                ));
            }
        }
        let reg = self.caps.pwm_ctrl[idx];
        self.write_verified(reg, v)
    }

    /// Selects `bank`, always comparing against the bank currently selected (never assumes 0).
    fn select_bank(&mut self, bank: u8) -> Result<()> {
        if self.caps.banks <= 1 && !self.caps.force_bank0 {
            if bank == 0 {
                return Ok(());
            }
            return Err(err(ErrorCode::Internal, format!("{} has no register bank {bank}", self.model.name())));
        }
        let v = self.read(REG_BANK)?;
        let nv = (v & 0x9F) | ((bank & 0x03) << 5);
        if nv != v {
            self.write(REG_BANK, nv)?;
        }
        Ok(())
    }

    fn ensure_bank0(&mut self) -> Result<()> {
        self.select_bank(0)
    }

    /// Runs `f` with `bank` selected and always returns to bank 0.
    fn with_bank<T>(&mut self, bank: u8, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let r = self.select_bank(bank).and_then(|_| f(self));
        let back = self.select_bank(0);
        let v = r?;
        back?;
        Ok(v)
    }

    fn extvec_regs(idx: usize) -> [(u8, u8); 4] {
        let n = (idx as u8) * 0x10;
        [(2, 0x12 + n), (2, 0x16 + n), (2, 0x1a + n), (3, 0x1c + n)]
    }

    /// Reads the PWM control register and every curve register of channel `idx` (all banks).
    fn read_image(&mut self, idx: usize) -> Result<(u8, CurveImage)> {
        self.ensure_bank0()?;
        let ctrl = self.read(self.caps.pwm_ctrl[idx])?;
        let mut img = CurveImage::default();
        let sgb = SG_BASE[idx];
        for k in 0..6 {
            img.sg[k] = self.read(sgb + k as u8)?;
        }
        let b = sg::ev_base(idx);
        let pb = sg::ev_pwm_base(idx);
        self.with_bank(2, |c| {
            for k in 0..13 {
                img.bank2[k] = c.read(b + k as u8)?;
            }
            Ok(())
        })?;
        self.with_bank(3, |c| {
            for k in 0..16 {
                img.bank3[k] = c.read(b + k as u8)?;
            }
            for k in 0..6 {
                img.bank3_pwm[k] = c.read(pb + k as u8)?;
            }
            Ok(())
        })?;
        Ok((ctrl, img))
    }

    /// Brings channel `idx` to `ctrl`/`target`, writing only registers that differ from the known
    /// state, in GIGABYTE's order: disable extra vectors whose data changes, bank 0 curve, bank 2,
    /// bank 3 (enable bits last), PWM control register last. Every write is verified.
    fn write_image(&mut self, idx: usize, ctrl: u8, target: &CurveImage) -> Result<()> {
        let (mut kc, mut k) = match self.chans[idx].known {
            Some(x) => x,
            None => self.read_image(idx)?,
        };
        let r = self.write_image_inner(idx, &mut kc, &mut k, ctrl, target);
        self.chans[idx].known = if r.is_ok() { Some((kc, k)) } else { None };
        r
    }

    fn write_image_inner(
        &mut self,
        idx: usize,
        kc: &mut u8,
        k: &mut CurveImage,
        ctrl: u8,
        t: &CurveImage,
    ) -> Result<()> {
        let sgb = SG_BASE[idx];
        let b = sg::ev_base(idx);
        let pb = sg::ev_pwm_base(idx);
        const FRAC2: [usize; 3] = [2, 6, 10];
        let mask2 = |i: usize| if FRAC2.contains(&i) { 0x7Fu8 } else { 0xFF };
        let ev_data_changed = (0..12).any(|i| (k.bank2[i] ^ t.bank2[i]) & mask2(i) != 0)
            || (0..16).any(|i| i != sg::B3_ENABLE && k.bank3[i] != t.bank3[i])
            || k.bank3_pwm != t.bank3_pwm;
        self.ensure_bank0()?;

        // 1. Disable enabled extra vectors before their data changes.
        if ev_data_changed {
            if FRAC2.iter().any(|&i| k.bank2[i] & 0x80 != 0) {
                self.with_bank(2, |c| {
                    for i in FRAC2 {
                        if k.bank2[i] & 0x80 != 0 {
                            let v = k.bank2[i] & !0x80;
                            c.write_verified(b + i as u8, v)?;
                            k.bank2[i] = v;
                        }
                    }
                    Ok(())
                })?;
            }
            if k.bank3[sg::B3_ENABLE] & 0x3F != 0 {
                self.with_bank(3, |c| {
                    let v = k.bank3[sg::B3_ENABLE] & !0x3F;
                    c.write_verified(b + sg::B3_ENABLE as u8, v)?;
                    k.bank3[sg::B3_ENABLE] = v;
                    Ok(())
                })?;
            }
        }

        // 2. Bank 0 curve registers.
        for i in 0..6 {
            if k.sg[i] != t.sg[i] {
                self.write_verified(sgb + i as u8, t.sg[i])?;
                k.sg[i] = t.sg[i];
            }
        }

        // 3. Bank 2: main slope fraction, zones A-C (the register with the enable bit last).
        const ORDER2: [usize; 13] = [12, 0, 1, 3, 2, 4, 5, 7, 6, 8, 9, 11, 10];
        if ORDER2.iter().any(|&i| k.bank2[i] != t.bank2[i]) {
            self.with_bank(2, |c| {
                for i in ORDER2 {
                    if k.bank2[i] != t.bank2[i] {
                        c.write_verified(b + i as u8, t.bank2[i])?;
                        k.bank2[i] = t.bank2[i];
                    }
                }
                Ok(())
            })?;
        }

        // 4. Bank 3: zones D-I, start PWMs, slope fractions, enable bits last.
        if k.bank3 != t.bank3 || k.bank3_pwm != t.bank3_pwm {
            self.with_bank(3, |c| {
                for i in (0..16).filter(|&i| i != sg::B3_ENABLE) {
                    if k.bank3[i] != t.bank3[i] {
                        c.write_verified(b + i as u8, t.bank3[i])?;
                        k.bank3[i] = t.bank3[i];
                    }
                }
                for d in 0..6 {
                    if k.bank3_pwm[d] != t.bank3_pwm[d] {
                        c.write_verified(pb + d as u8, t.bank3_pwm[d])?;
                        k.bank3_pwm[d] = t.bank3_pwm[d];
                    }
                }
                let e = sg::B3_ENABLE;
                if k.bank3[e] != t.bank3[e] {
                    c.write_verified(b + e as u8, t.bank3[e])?;
                    k.bank3[e] = t.bank3[e];
                }
                Ok(())
            })?;
        }

        // 5. Automatic mode and temperature input.
        if *kc != ctrl {
            self.write_ctrl(idx, ctrl)?;
            *kc = ctrl;
        }
        Ok(())
    }

    // ---- monitoring ----------------------------------------------------------

    pub fn update(&mut self) -> Result<IteReadings> {
        self.ensure_bank0()?;
        let mut r = IteReadings::default();

        for i in 0..self.caps.volts.min(REG_VIN.len()) {
            let v = self.read(REG_VIN[i]).ok().map(|raw| raw as f32 * self.caps.volt_gain);
            r.volts.push(v.filter(|v| *v > 0.0));
        }

        for i in 0..self.caps.temps {
            let t = self.read(REG_TEMPERATURE_BASE + i as u8).ok().map(|raw| raw as i8 as f32);
            r.temps.push(t);
        }

        for i in 0..self.caps.fans.min(6) {
            if self.fan_disabled[i] {
                r.fans.push(None);
                continue;
            }
            let rpm = if self.caps.fan16 {
                let (lo_reg, hi_reg) = if self.caps.alt_6th_fan {
                    (REG_FAN_TACH_ALT[i], REG_FAN_TACH_EXT_ALT[i])
                } else {
                    (REG_FAN_TACH[i], REG_FAN_TACH_EXT[i])
                };
                match (self.read(lo_reg), self.read(hi_reg)) {
                    (Ok(lo), Ok(hi)) => {
                        let count = lo as u32 | ((hi as u32) << 8);
                        if count > 0x3f {
                            Some(if count < 0xffff { 1.35e6 / (count as f32 * 2.0) } else { 0.0 })
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            } else {
                match self.read(REG_FAN_TACH[i]) {
                    Ok(count) if count > 0 => {
                        let divisor = if i < 2 {
                            let d = self.read(REG_FAN_TACH_DIVISOR).unwrap_or(0);
                            1u32 << ((d >> (3 * i)) & 0x7)
                        } else {
                            2
                        };
                        Some(if count < 0xff { 1.35e6 / (count as f32 * divisor as f32) } else { 0.0 })
                    }
                    _ => None,
                }
            };
            r.fans.push(rpm);
        }

        self.polls = self.polls.wrapping_add(1);
        for i in 0..self.caps.pwms {
            let temp_of =
                |r: &IteReadings, src: u8| r.temps.get(src as usize).copied().flatten().filter(|t| plausible_temp(*t));
            let mut est = None;
            let duty = if self.chans[i].mode == ChanMode::Curve {
                self.chans[i].curve.as_ref().and_then(|(src, c)| temp_of(&r, *src).map(|t| c.duty_at(t)))
            } else {
                match self.read(self.caps.pwm_ctrl[i]) {
                    Ok(ctrl) if ctrl & 0x80 == 0 => {
                        if self.caps.ext_duty {
                            self.read(REG_PWM_DUTY[i]).ok().map(|d| d as f32 * 100.0 / 255.0)
                        } else {
                            Some((ctrl & 0x7F) as f32 * 100.0 / 127.0)
                        }
                    }
                    Ok(ctrl) => {
                        if self.chans[i].mode == ChanMode::Firmware {
                            est = self
                                .firmware_estimate(i, ctrl)
                                .and_then(|(src, c)| temp_of(&r, src).map(|t| c.duty_at(t)));
                        }
                        None
                    }
                    _ => None,
                }
            };
            r.duty.push(duty);
            r.duty_est.push(est);
        }

        self.verify_channels();
        Ok(r)
    }

    /// The BIOS curve of automatic-mode channel `i` (read with control value `ctrl`): decoded from
    /// the registers on first use, when `ctrl` changes and every [`FW_CURVE_REFRESH`] polls. Only on
    /// chips whose curve registers we know (V3 with extra vectors).
    fn firmware_estimate(&mut self, i: usize, ctrl: u8) -> Option<(u8, ChipCurve)> {
        if !self.extvec {
            return None;
        }
        let due = match &self.chans[i].fw_curve {
            Some((c, _, _)) => *c != ctrl || self.polls.wrapping_sub(self.chans[i].fw_curve_poll) >= FW_CURVE_REFRESH,
            None => true,
        };
        if due {
            self.chans[i].fw_curve_poll = self.polls;
            self.chans[i].fw_curve = match self.read_image(i) {
                Ok((c, img)) if c == ctrl && (ctrl >> 3) & 7 <= 5 => Some((ctrl, (ctrl >> 3) & 7, sg::decode(&img))),
                _ => None,
            };
        }
        self.firmware_curve(i)
    }

    /// Detects the firmware changing a channel we drive (typically after resume from sleep) and
    /// re-applies our settings. Cheap bank-0 check every poll, all banks every
    /// [`FULL_VERIFY_EVERY`] polls.
    fn verify_channels(&mut self) {
        for i in 0..self.caps.pwms {
            if self.chans[i].release_pending {
                // Our own leftover state after a failed release is not a firmware override:
                // retry handing the channel back, never re-apply our settings.
                self.retry_release(i);
                continue;
            }
            if self.chans[i].mode == ChanMode::Firmware || self.chans[i].saved.is_none() {
                continue;
            }
            self.chans[i].polls = self.chans[i].polls.wrapping_add(1);
            let full = self.chans[i].polls.is_multiple_of(FULL_VERIFY_EVERY);
            match self.channel_matches(i, full) {
                Ok(true) => self.chans[i].overrides = 0,
                Ok(false) => self.handle_override(i),
                Err(e) => log::debug!("{}: verification read failed: {e:#}", self.label(i)),
            }
        }
    }

    fn channel_matches(&mut self, i: usize, full: bool) -> Result<bool> {
        if self.extvec {
            let Some((kc, k)) = self.chans[i].known else { return Ok(false) };
            if full {
                let (c, img) = self.read_image(i)?;
                return Ok(c == kc && img == k);
            }
            if self.read(self.caps.pwm_ctrl[i])? != kc {
                return Ok(false);
            }
            for s in [SG_TSTART, SG_TFULL, SG_PWM] {
                if self.read(SG_BASE[i] + s as u8)? != k.sg[s] {
                    return Ok(false);
                }
            }
            Ok(true)
        } else {
            let Some((kc, kd)) = self.chans[i].known_manual else { return Ok(false) };
            if self.read(self.caps.pwm_ctrl[i])? != kc {
                return Ok(false);
            }
            if self.caps.ext_duty && self.read(REG_PWM_DUTY[i])? != kd {
                return Ok(false);
            }
            Ok(true)
        }
    }

    fn retry_release(&mut self, i: usize) {
        let label = self.label(i);
        match self.release(i) {
            Ok(()) => {
                log::info!("{label}: handed back to the BIOS on retry");
            }
            Err(e) => {
                let n = self.chans[i].release_failures;
                if n == 2 || n.is_multiple_of(RELEASE_REPORT_EVERY) {
                    self.events.push(AppError::new(
                        ErrorCode::RestoreFailed,
                        format!("{label}: handing the fan back to the BIOS still fails ({n} attempts): {e:#}"),
                    ));
                }
            }
        }
    }

    fn handle_override(&mut self, i: usize) {
        self.chans[i].overrides = self.chans[i].overrides.saturating_add(1);
        let n = self.chans[i].overrides;
        let label = self.label(i);
        if n >= MAX_OVERRIDES {
            let detail =
                format!("{label}: the firmware keeps overriding our fan settings; the fan was handed back to the BIOS");
            log::warn!("[{}] {detail}", ErrorCode::RegisterMismatch);
            let r = self.release(i);
            self.chans[i].gave_up_at = Some(Instant::now());
            self.events.push(AppError::new(ErrorCode::RegisterMismatch, detail));
            if let Err(e) = r {
                self.events.push(AppError::new(ErrorCode::RestoreFailed, format!("{e:#}")));
            }
            return;
        }
        let detail = format!("{label}: the BIOS/EC changed the fan settings; re-applied");
        log::info!("[{}] {detail}", ErrorCode::FirmwareOverride);
        if n == 1 {
            self.events.push(AppError::new(ErrorCode::FirmwareOverride, detail));
        }
        self.chans[i].known = None;
        self.chans[i].known_manual = None;
        if let Err(e) = self.reapply_channel(i) {
            let _ = if self.chans[i].mode == ChanMode::Curve { self.fail_curve(i, e) } else { self.fail_manual(i, e) };
        }
    }

    fn reapply_channel(&mut self, i: usize) -> Result<()> {
        self.ensure_bank0()?;
        if self.extvec {
            let Some((ctrl, img)) = self.chans[i].target else { return Ok(()) };
            self.write_image(i, ctrl, &img)
        } else if let Some(d) = self.chans[i].manual_duty {
            self.apply_manual(i, d)
        } else {
            Ok(())
        }
    }

    /// Re-applies every channel we drive (call after resume from sleep: the BIOS reprograms the
    /// chip). Channels the firmware fought earlier may be taken again. Returns the channel count.
    pub fn reapply(&mut self) -> usize {
        let mut n = 0;
        for i in 0..self.caps.pwms {
            self.chans[i].gave_up_at = None;
            if self.chans[i].release_pending {
                self.retry_release(i);
                continue;
            }
            if self.chans[i].mode == ChanMode::Firmware {
                continue;
            }
            self.chans[i].known = None;
            self.chans[i].known_manual = None;
            match self.reapply_channel(i) {
                Ok(()) => n += 1,
                Err(e) => {
                    let _ = if self.chans[i].mode == ChanMode::Curve {
                        self.fail_curve(i, e)
                    } else {
                        self.fail_manual(i, e)
                    };
                }
            }
        }
        n
    }

    // ---- control ---------------------------------------------------------------

    fn check_writable(&self, idx: usize) -> Result<()> {
        if !self.controllable {
            return Err(err(
                ErrorCode::FanReadOnly,
                format!("{} is monitored read-only on this board", self.model.name()),
            ));
        }
        if idx >= self.caps.pwms {
            return Err(err(
                ErrorCode::NoSuchFan,
                format!("{}: PWM channel {} does not exist", self.model.name(), idx + 1),
            ));
        }
        if self.chans[idx].gave_up_at.is_some_and(|t| t.elapsed() < GIVE_UP_FOR) {
            return Err(err(
                ErrorCode::RegisterMismatch,
                format!("{}: the firmware kept overriding this fan; it stays with the BIOS for now", self.label(idx)),
            ));
        }
        Ok(())
    }

    /// Records the firmware state of channel `idx` before the first write.
    fn take_over(&mut self, idx: usize) -> Result<()> {
        if self.chans[idx].saved.is_some() {
            return Ok(());
        }
        self.ensure_bank0()?;
        let snap = if self.extvec {
            let (ctrl, img) = self.read_image(idx)?;
            self.chans[idx].known = Some((ctrl, img));
            SavedPwm {
                ctrl,
                duty: img.sg[SG_PWM],
                main_bit: None,
                extvec: Some([img.bank2[2], img.bank2[6], img.bank2[10], img.bank3[sg::B3_ENABLE]]),
                image: Some(img),
            }
        } else {
            let ctrl = self.read(self.caps.pwm_ctrl[idx])?;
            let duty = if self.caps.ext_duty { self.read(REG_PWM_DUTY[idx])? } else { ctrl & 0x7F };
            self.chans[idx].known_manual = Some((ctrl, duty));
            SavedPwm { ctrl, duty, main_bit: None, extvec: None, image: None }
        };
        if snap.ctrl & CTRL_AUTO == 0 {
            log::warn!(
                "{}: already in software mode (ctrl={:#04x}) when taking control, probably left over from a crash; \
                 it will be handed back in automatic mode",
                self.label(idx),
                snap.ctrl
            );
        }
        log::info!(
            "{}: taking control (firmware ctrl={:#04x} start PWM={:#04x} curve={:?})",
            self.label(idx),
            snap.ctrl,
            snap.duty,
            snap.image.map(|i| sg::decode(&i))
        );
        self.chans[idx].saved = Some(snap);
        self.chans[idx].untrusted = false;
        Ok(())
    }

    /// Hands channel `idx` a curve the chip runs itself, driven by temperature input `input`.
    pub fn set_curve(&mut self, idx: usize, input: u8, curve: &ChipCurve) -> Result<()> {
        self.check_writable(idx)?;
        if !self.hw_curves() {
            return Err(err(
                ErrorCode::FanReadOnly,
                format!("{}: hardware curves are not supported on this chip", self.label(idx)),
            ));
        }
        if let Some(e) = self.curve_pause_error(idx) {
            return Err(e);
        }
        if input > 5 || input as usize >= self.caps.temps {
            return Err(err(ErrorCode::Internal, format!("{}: invalid temperature input {input}", self.label(idx))));
        }
        if let Err(why) = curve.check_programmable() {
            return Err(err(ErrorCode::Internal, format!("{}: refusing to program curve: {why}", self.label(idx))));
        }
        match self.apply_curve_retrying(idx, input, curve) {
            Ok(()) => {
                self.curve_ok(idx);
                Ok(())
            }
            Err(e) => Err(self.fail_curve(idx, e)),
        }
    }

    /// The error to return while hardware curves are paused on channel `idx`.
    fn curve_pause_error(&self, idx: usize) -> Option<anyhow::Error> {
        let at = self.chans[idx].curve_retry_at.filter(|t| Instant::now() < *t)?;
        Some(err(
            ErrorCode::RegisterMismatch,
            format!(
                "{}: curve registers did not verify; hardware curves paused for {} s",
                self.label(idx),
                at.saturating_duration_since(Instant::now()).as_secs() + 1
            ),
        ))
    }

    /// A curve write verified: clear the failure back-off and any pending release.
    fn curve_ok(&mut self, idx: usize) {
        let ch = &mut self.chans[idx];
        ch.curve_failures = 0;
        ch.curve_retry_at = None;
        ch.release_pending = false;
        ch.release_failures = 0;
    }

    /// `apply_curve` with up to [`WRITE_ATTEMPTS`] attempts: a read-back corrupted by a concurrent
    /// access to the chip is not a reason to give up hardware curves.
    fn apply_curve_retrying(&mut self, idx: usize, input: u8, curve: &ChipCurve) -> Result<()> {
        let mut attempt = 1;
        loop {
            match self.apply_curve(idx, input, curve) {
                Ok(()) => return Ok(()),
                Err(e)
                    if attempt < WRITE_ATTEMPTS
                        && matches!(code_of(&e), ErrorCode::RegisterMismatch | ErrorCode::ChipInvalid) =>
                {
                    log::warn!("{}: curve write attempt {attempt} failed ({e:#}); retrying", self.label(idx));
                    self.chans[idx].known = None;
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn apply_curve(&mut self, idx: usize, input: u8, curve: &ChipCurve) -> Result<()> {
        self.ensure_bank0()?;
        self.take_over(idx)?;
        let saved = self.chans[idx].saved.ok_or_else(|| err(ErrorCode::Internal, "channel not saved"))?;
        let base = match self.chans[idx].known {
            Some((_, img)) => img,
            None => {
                let k = self.read_image(idx)?;
                self.chans[idx].known = Some(k);
                k.1
            }
        };
        let img = sg::encode(curve, &base);
        let ctrl = ctrl_auto(saved.ctrl, input);
        let ch = &mut self.chans[idx];
        ch.mode = ChanMode::Curve;
        ch.target = Some((ctrl, img));
        ch.curve = Some((input, curve.clone()));
        ch.manual_duty = None;
        self.write_image(idx, ctrl, &img)
    }

    /// A curve write failed: give the fan back to the BIOS. Returns the error to report.
    fn fail_curve(&mut self, idx: usize, e: anyhow::Error) -> anyhow::Error {
        let code = code_of(&e);
        log::error!("{}: hardware curve write failed ({e:#}); returning the fan to the BIOS", self.label(idx));
        if code == ErrorCode::RegisterMismatch {
            // Pause hardware curves on this channel with an exponential back-off (never for good:
            // the mismatch may have been transient).
            let ch = &mut self.chans[idx];
            ch.curve_failures = ch.curve_failures.saturating_add(1);
            let pause = CURVE_BACKOFF.saturating_mul(1 << (ch.curve_failures - 1).min(5)).min(CURVE_BACKOFF_MAX);
            ch.curve_retry_at = Some(Instant::now() + pause);
            log::warn!("{}: hardware curves paused for {} s", self.label(idx), pause.as_secs());
        }
        self.events.push(AppError::new(
            if code == ErrorCode::Unknown { ErrorCode::RegisterMismatch } else { code },
            format!("{}: {e:#}", self.label(idx)),
        ));
        match self.release(idx) {
            Ok(()) => e,
            Err(r) => {
                self.events.push(AppError::new(ErrorCode::RestoreFailed, format!("{r:#}")));
                r
            }
        }
    }

    fn fail_manual(&mut self, idx: usize, e: anyhow::Error) -> anyhow::Error {
        let code = code_of(&e);
        log::error!("{}: PWM write failed ({e:#}); returning the fan to the BIOS", self.label(idx));
        if code == ErrorCode::RegisterMismatch {
            self.chans[idx].manual_failed = true;
        }
        self.events.push(AppError::new(
            if code == ErrorCode::Unknown { ErrorCode::RegisterMismatch } else { code },
            format!("{}: {e:#}", self.label(idx)),
        ));
        match self.release(idx) {
            Ok(()) => e,
            Err(r) => {
                self.events.push(AppError::new(ErrorCode::RestoreFailed, format!("{r:#}")));
                r
            }
        }
    }

    /// Runs channel `idx` at `duty_pct` (0..100). On chips with hardware curves this is a flat
    /// curve that ramps to 100 % at `failsafe_temp` °C on temperature input `failsafe_input`, so a
    /// hung or crashed app still leaves a safe curve. Raw software PWM is the fallback.
    pub fn set_duty(&mut self, idx: usize, duty_pct: f32, failsafe_input: u8, failsafe_temp: f32) -> Result<()> {
        self.check_writable(idx)?;
        let duty_pct = if duty_pct.is_finite() { duty_pct.clamp(0.0, 100.0) } else { 100.0 };
        if self.hw_curves() {
            if !self.chans[idx].curve_paused() {
                let input = if failsafe_input <= 5 && (failsafe_input as usize) < self.caps.temps {
                    failsafe_input
                } else {
                    sg::SAFE_DEFAULT_INPUT
                };
                let curve = sg::flat(duty_pct, failsafe_temp);
                match self.apply_curve_retrying(idx, input, &curve) {
                    Ok(()) => {
                        self.curve_ok(idx);
                        return Ok(());
                    }
                    Err(e) => {
                        let code = code_of(&e);
                        let e = self.fail_curve(idx, e);
                        if code != ErrorCode::RegisterMismatch || code_of(&e) == ErrorCode::RestoreFailed {
                            return Err(e);
                        }
                    }
                }
            }
            // The hardware curve is paused on this channel. Software PWM has no hardware failsafe:
            // only use it while crash protection is active, otherwise the BIOS keeps the fan.
            if !self.software_fallback {
                if self.chans[idx].mode == ChanMode::Manual {
                    self.release(idx)?;
                }
                let why = self.curve_pause_error(idx).map(|e| format!("{e:#}")).unwrap_or_default();
                return Err(err(
                    ErrorCode::RegisterMismatch,
                    format!("{why}; software PWM without crash protection is not used, the BIOS keeps this fan"),
                ));
            }
            if self.chans[idx].mode != ChanMode::Manual {
                log::warn!("{}: falling back to software PWM (no hardware failsafe)", self.label(idx));
            }
        }
        if self.chans[idx].manual_failed {
            return Err(err(
                ErrorCode::RegisterMismatch,
                format!("{}: PWM registers did not verify earlier; the fan stays with the BIOS", self.label(idx)),
            ));
        }
        let duty = (duty_pct * 2.55).round() as u8;
        let r = self.ensure_bank0().and_then(|_| self.take_over(idx)).and_then(|_| self.apply_manual(idx, duty));
        match r {
            Ok(()) => {
                self.chans[idx].release_pending = false;
                self.chans[idx].release_failures = 0;
                Ok(())
            }
            Err(e) => Err(self.fail_manual(idx, e)),
        }
    }

    /// Software PWM at `duty` (0..255). Extra vectors are disabled once (they are already off on
    /// later calls), FAN_MAIN_CTRL is not touched, and closed-loop mode is never set.
    fn apply_manual(&mut self, idx: usize, duty: u8) -> Result<()> {
        let saved = self.chans[idx].saved.ok_or_else(|| err(ErrorCode::Internal, "channel not saved"))?;
        {
            let ch = &mut self.chans[idx];
            ch.mode = ChanMode::Manual;
            ch.manual_duty = Some(duty);
            ch.curve = None;
        }
        if self.extvec {
            let base = match self.chans[idx].known {
                Some((_, img)) => img,
                None => {
                    let k = self.read_image(idx)?;
                    self.chans[idx].known = Some(k);
                    k.1
                }
            };
            let mut img = base;
            img.sg[SG_PWM] = duty;
            for i in [2, 6, 10] {
                img.bank2[i] &= !0x80;
            }
            img.bank3[sg::B3_ENABLE] &= 0xC0;
            let keep = if saved.ctrl & CTRL_AUTO != 0 { saved.ctrl & CTRL_KEEP } else { 0 };
            let ctrl = keep | (valid_src(saved.ctrl) << 3);
            self.chans[idx].target = Some((ctrl, img));
            return self.write_image(idx, ctrl, &img);
        }
        let (kc, kd) = match self.chans[idx].known_manual {
            Some(x) => x,
            None => {
                let c = self.read(self.caps.pwm_ctrl[idx])?;
                let d = if self.caps.ext_duty { self.read(REG_PWM_DUTY[idx])? } else { c & 0x7F };
                (c, d)
            }
        };
        self.chans[idx].known_manual = None;
        let (ctrl, d) = if self.caps.ext_duty {
            if kd != duty {
                self.write_verified(REG_PWM_DUTY[idx], duty)?;
            }
            (saved.ctrl & !(CTRL_AUTO | CTRL_CLOSED_LOOP), duty)
        } else {
            (duty >> 1, duty >> 1)
        };
        if kc != ctrl {
            self.write_ctrl(idx, ctrl)?;
        }
        self.chans[idx].known_manual = Some((ctrl, d));
        Ok(())
    }

    /// The firmware state to hand back for a V3 channel. A channel saved in software mode (left
    /// over from a crash) or with an invalid control value goes back to automatic mode with its
    /// saved curve if that curve is valid, otherwise with a safe default curve.
    fn restore_target(&self, idx: usize, saved: &SavedPwm, img: &CurveImage) -> (u8, CurveImage) {
        let curve = sg::decode(img);
        let src = (saved.ctrl >> 3) & 7;
        let untrusted = self.chans[idx].untrusted;
        // From a crash-snapshot file, closed-loop (tachometer) mode is not accepted either.
        let ctrl_ok = saved.ctrl & CTRL_AUTO != 0 && src <= 5 && (!untrusted || saved.ctrl & CTRL_CLOSED_LOOP == 0);
        let trusted = !untrusted || curve.is_safe();
        if ctrl_ok && trusted {
            return (saved.ctrl, *img);
        }
        if curve.activated() && curve.is_safe() {
            log::warn!(
                "{}: saved state was in software mode; handing it back in automatic mode with its curve",
                self.label(idx)
            );
            return (ctrl_auto(if ctrl_ok { saved.ctrl } else { 0 }, valid_src(saved.ctrl)), *img);
        }
        log::warn!("{}: no valid firmware curve to restore; writing a safe default curve", self.label(idx));
        (ctrl_auto(0, sg::SAFE_DEFAULT_INPUT), sg::encode(&sg::safe_default(), img))
    }

    fn restore_channel(&mut self, idx: usize, saved: &SavedPwm) -> Result<()> {
        if self.extvec
            && let Some(img) = saved.image
        {
            let (ctrl, target) = self.restore_target(idx, saved, &img);
            return self.write_image(idx, ctrl, &target);
        }
        // Older snapshot format, or a chip without extra vectors.
        let mut ctrl = saved.ctrl;
        let mut use_default = false;
        if self.caps.v3
            && self.chans[idx].untrusted
            && ctrl & CTRL_AUTO != 0
            && (ctrl & CTRL_CLOSED_LOOP != 0 || (ctrl >> 3) & 7 > 5)
        {
            // An automatic-mode value from a crash-snapshot file that the firmware would not use.
            log::warn!(
                "{}: snapshot PWM control value {ctrl:#04x} is not valid; using automatic mode",
                self.label(idx)
            );
            ctrl = ctrl_auto(0, valid_src(ctrl));
        }
        if ctrl & CTRL_AUTO == 0 && self.caps.ext_duty && idx < SG_BASE.len() && self.caps.pwm_ctrl[0] == 0x15 {
            // Is there a curve to go back to (the main curve registers not all zero)?
            let sgb = SG_BASE[idx];
            let regs = [self.read(sgb)?, self.read(sgb + 1)?, self.read(sgb + 3)?, self.read(sgb + 4)?];
            if regs.iter().any(|&r| r != 0) {
                ctrl = if self.caps.v3 { ctrl_auto(0, valid_src(ctrl)) } else { ctrl | CTRL_AUTO };
                log::warn!("{}: saved state was in software mode; handing it back in automatic mode", self.label(idx));
            } else if self.extvec {
                use_default = true;
            } else {
                log::warn!("{}: saved state is software mode without a curve; restored as saved", self.label(idx));
            }
        }
        if self.caps.ext_duty {
            self.write_verified(REG_PWM_DUTY[idx], saved.duty)?;
        }
        if !use_default {
            self.write_ctrl(idx, ctrl)?;
        }
        if let Some(bit) = saved.main_bit {
            let m = self.read(REG_FAN_MAIN_CTRL)?;
            let nm = if bit { m | (1 << idx) } else { m & !(1 << idx) };
            if nm != m {
                self.write_verified(REG_FAN_MAIN_CTRL, nm)?;
            }
        }
        if let (Some(ev), true) = (saved.extvec, self.caps.banks > 1) {
            for (k, (bank, reg)) in Self::extvec_regs(idx).iter().enumerate() {
                self.with_bank(*bank, |c| c.write_verified(*reg, ev[k]))?;
            }
        }
        if use_default {
            log::warn!("{}: no valid firmware curve to restore; writing a safe default curve", self.label(idx));
            self.chans[idx].known = None;
            let (_, img) = self.read_image(idx)?;
            let target = sg::encode(&sg::safe_default(), &img);
            self.write_image(idx, ctrl_auto(0, sg::SAFE_DEFAULT_INPUT), &target)?;
        }
        Ok(())
    }

    /// Restores the firmware (BIOS Smart Fan) settings for channel `idx` (up to [`WRITE_ATTEMPTS`]
    /// attempts). On failure the saved state is kept, the channel is marked release-pending and
    /// polling retries the release (the caller may retry as well).
    pub fn release(&mut self, idx: usize) -> Result<()> {
        let Some(saved) = self.chans.get(idx).and_then(|c| c.saved) else { return Ok(()) };
        let mut last = None;
        for attempt in 1..=WRITE_ATTEMPTS {
            match self.ensure_bank0().and_then(|_| self.restore_channel(idx, &saved)) {
                Ok(()) => {
                    last = None;
                    break;
                }
                Err(e) => {
                    log::warn!("{}: release attempt {attempt} failed: {e:#}", self.label(idx));
                    self.chans[idx].known = None;
                    self.chans[idx].known_manual = None;
                    last = Some(e);
                }
            }
        }
        if let Some(e) = last {
            let ch = &mut self.chans[idx];
            ch.release_pending = true;
            ch.release_failures = ch.release_failures.saturating_add(1);
            return Err(err(ErrorCode::RestoreFailed, format!("{}: {e:#}", self.label(idx))));
        }
        let ch = &mut self.chans[idx];
        let keep = Chan {
            curve_failures: ch.curve_failures,
            curve_retry_at: ch.curve_retry_at,
            manual_failed: ch.manual_failed,
            gave_up_at: ch.gave_up_at,
            ..Default::default()
        };
        *ch = keep;
        log::info!("{}: returned to BIOS control", self.label(idx));
        Ok(())
    }

    #[cfg(test)]
    pub fn release_all(&mut self) -> Result<()> {
        let mut first = None;
        for i in 0..self.caps.pwms {
            if let Err(e) = self.release(i) {
                log::error!("[{}] {e:#}", ErrorCode::RestoreFailed);
                first.get_or_insert(e);
            }
        }
        first.map_or(Ok(()), Err)
    }

    /// Saved firmware state of every controlled channel (for crash recovery).
    pub fn saved_state(&self) -> Vec<(usize, SavedPwm)> {
        self.chans.iter().enumerate().filter_map(|(i, c)| c.saved.map(|s| (i, s))).collect()
    }

    /// Re-applies a saved firmware state from a crash snapshot (watchdog / next launch).
    /// Returns the number of channels restored.
    pub fn restore_saved(&mut self, state: &[(usize, SavedPwm)]) -> Result<usize> {
        if !self.controllable {
            log::warn!("{}: crash snapshot names a monitor-only chip; ignored", self.model.name());
            return Ok(0);
        }
        let mut n = 0;
        let mut first = None;
        for (idx, saved) in state {
            if *idx < self.caps.pwms {
                // Same rule as the snapshot validator in `system`: firmware state is automatic mode
                // or a fixed duty of at least 50 %. Software mode below that would leave the fan
                // (nearly) stopped with no failsafe, so it is never written from a snapshot file.
                if saved.ctrl & CTRL_AUTO == 0 && saved.duty < 0x80 {
                    log::warn!(
                        "{}: crash snapshot holds software mode at duty {:#04x}; not restored",
                        self.label(*idx),
                        saved.duty
                    );
                    continue;
                }
                let ch = &mut self.chans[*idx];
                ch.saved = Some(*saved);
                ch.untrusted = true;
                ch.known = None;
                ch.known_manual = None;
                match self.release(*idx) {
                    Ok(()) => n += 1,
                    Err(e) => {
                        log::error!("{}: restore failed: {e:#}", self.label(*idx));
                        first.get_or_insert(e);
                    }
                }
            }
        }
        first.map_or(Ok(n), Err)
    }

    /// Full register dump of all banks (diagnostics).
    pub fn dump(&mut self) -> String {
        let mut s = String::new();
        for bank in 0..self.caps.banks.max(1) {
            if self.select_bank(bank).is_err() {
                continue;
            }
            s.push_str(&format!("Bank {bank}\n      00 01 02 03 04 05 06 07 08 09 0A 0B 0C 0D 0E 0F\n"));
            for row in 0..=0xAu8 {
                s.push_str(&format!(" {:02X}  ", row << 4));
                for col in 0..16u8 {
                    match self.read((row << 4) | col) {
                        Ok(v) => s.push_str(&format!(" {v:02X}")),
                        Err(_) => s.push_str(" ??"),
                    }
                }
                s.push('\n');
            }
        }
        let _ = self.select_bank(0);
        for (i, c) in self.chans.iter().enumerate().take(self.caps.pwms) {
            if c.mode != ChanMode::Firmware || c.curve_failures > 0 || c.manual_failed || c.release_pending {
                s.push_str(&format!(
                    "{}: {:?} curve={:?} curve_failures={} curve_paused={} manual_failed={} release_pending={}\n",
                    self.label(i),
                    c.mode,
                    c.curve,
                    c.curve_failures,
                    c.curve_paused(),
                    c.manual_failed,
                    c.release_pending
                ));
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    /// Simulated ITE environment controller with 4 banks behind an address/data port pair.
    pub struct MockEc {
        pub banks: [[u8; 256]; 4],
        pub addr: u8,
        /// Every data write: (bank, register, value).
        pub writes: Vec<(usize, u8, u8)>,
        /// Registers that ignore writes (bank, register).
        pub stuck: HashSet<(usize, u8)>,
        /// Writes of one specific value that are ignored (bank, register, value).
        pub reject: HashSet<(usize, u8, u8)>,
        /// Registers that ignore the next write only (a transient bus collision).
        pub flaky: HashSet<(usize, u8)>,
    }

    impl Default for MockEc {
        fn default() -> Self {
            MockEc {
                banks: [[0; 256]; 4],
                addr: 0,
                writes: Vec::new(),
                stuck: HashSet::new(),
                reject: HashSet::new(),
                flaky: HashSet::new(),
            }
        }
    }

    struct MockPort(Arc<Mutex<MockEc>>, u16);

    impl MockEc {
        fn bank(&self) -> usize {
            ((self.banks[0][REG_BANK as usize] >> 5) & 3) as usize
        }
        fn loc(&self, r: u8) -> (usize, usize) {
            // 0x10..0xAF is banked, everything else lives in bank 0.
            let b = if (0x10..0xB0).contains(&r) { self.bank() } else { 0 };
            (b, r as usize)
        }
        fn snapshot(&self) -> [[u8; 256]; 4] {
            self.banks
        }
    }

    impl PortIo for MockPort {
        fn inb(&mut self, port: u16) -> Result<u8> {
            let ec = self.0.lock().unwrap();
            if port == self.1 + 5 {
                Ok(ec.addr)
            } else {
                let (b, r) = ec.loc(ec.addr);
                Ok(ec.banks[b][r])
            }
        }
        fn outb(&mut self, port: u16, v: u8) -> Result<()> {
            let mut ec = self.0.lock().unwrap();
            if port == self.1 + 5 {
                ec.addr = v;
            } else {
                let (b, r) = ec.loc(ec.addr);
                ec.writes.push((b, r as u8, v));
                if ec.flaky.remove(&(b, r as u8)) {
                    return Ok(());
                }
                if !ec.stuck.contains(&(b, r as u8)) && !ec.reject.contains(&(b, r as u8, v)) {
                    ec.banks[b][r] = v;
                }
            }
            Ok(())
        }
    }

    fn setup(e: &mut MockEc) {
        e.banks[0][REG_VENDOR_ID as usize] = 0x90;
        e.banks[0][REG_CONFIGURATION as usize] = 0x11;
        e.banks[0][REG_FAN_16BIT as usize] = 0x37;
        e.banks[0][REG_FAN_MAIN_CTRL as usize] = 0x77;
        // CPU fan in automatic mode with temp input 0 (bits 5:3) and bit 1 set, BIOS curve:
        // Toff 0, Tstart 40, Tfull 70, start PWM 0x40, slope 4 + 2/8, ΔT 3.
        e.banks[0][0x15] = 0x82;
        e.banks[0][0x16] = 0x80;
        e.banks[0][0x60..0x66].copy_from_slice(&[0, 40, 70, 0x40, 4, 3]);
        e.banks[0][0x68..0x6E].copy_from_slice(&[0, 30, 60, 0x50, 3, 1]);
        e.banks[2][0x1C] = 0x2B; // main frac 2 + foreign bits
        // 1500 rpm on fan 1: count = 1.35e6 / (2*1500) = 450
        e.banks[0][0x0d] = (450 & 0xff) as u8;
        e.banks[0][0x18] = (450 >> 8) as u8;
        e.banks[0][0x29] = 35;
        e.banks[0][0x2b] = 52;
        e.banks[0][0x2c] = 0x80;
        // Gigabyte extra vectors enabled for channel 0 (zones A, B, C, D..I) with data.
        e.banks[2][0x10..0x1C].copy_from_slice(&[45, 5, 0x85, 0x60, 50, 6, 0x80, 0x80, 55, 7, 0x81, 0xA0]);
        e.banks[3][0x10..0x20].copy_from_slice(&[58, 8, 59, 8, 60, 9, 61, 9, 62, 9, 63, 9, 0x3f, 0x11, 0x22, 0x33]);
        e.banks[3][0x70..0x76].copy_from_slice(&[0xB0, 0xB8, 0xC0, 0xC8, 0xD0, 0xD8]);
    }

    fn make(model: IteModel, version: u8, gigabyte: bool) -> (IteChip, Arc<Mutex<MockEc>>) {
        let ec = Arc::new(Mutex::new(MockEc::default()));
        setup(&mut ec.lock().unwrap());
        let chip = IteChip::new(Box::new(MockPort(ec.clone(), 0xA40)), model, version, 0xA40, gigabyte).unwrap();
        ec.lock().unwrap().writes.clear();
        (chip, ec)
    }

    fn make_chip(gigabyte: bool) -> (IteChip, Arc<Mutex<MockEc>>) {
        make(IteModel::IT8689E, 1, gigabyte)
    }

    fn image_of(ec: &Arc<Mutex<MockEc>>, idx: usize) -> (u8, CurveImage) {
        let e = ec.lock().unwrap();
        let mut img = CurveImage::default();
        let sgb = SG_BASE[idx] as usize;
        img.sg.copy_from_slice(&e.banks[0][sgb..sgb + 6]);
        let b = sg::ev_base(idx) as usize;
        img.bank2.copy_from_slice(&e.banks[2][b..b + 13]);
        img.bank3.copy_from_slice(&e.banks[3][b..b + 16]);
        let pb = sg::ev_pwm_base(idx) as usize;
        img.bank3_pwm.copy_from_slice(&e.banks[3][pb..pb + 6]);
        (e.banks[0][[0x15, 0x16, 0x17, 0x7f, 0xa7, 0xaf][idx]], img)
    }

    fn assert_ctrl_writes_safe(ec: &Arc<Mutex<MockEc>>) {
        let e = ec.lock().unwrap();
        for &(b, r, v) in &e.writes {
            if b == 0 && [0x15, 0x16, 0x17, 0x7f, 0xa7, 0xaf].contains(&r) {
                assert_ne!(v, 0x7F, "0x7F written to control register {r:#04x}");
                assert_eq!(v & CTRL_CLOSED_LOOP, 0, "closed-loop bit written to {r:#04x}: {v:#04x}");
                assert!((v >> 3) & 7 <= 5, "invalid temperature input written to {r:#04x}: {v:#04x}");
            }
        }
    }

    #[test]
    fn reads_sensors() {
        let (mut chip, _ec) = make_chip(true);
        let r = chip.update().unwrap();
        assert_eq!(r.temps[0], Some(35.0));
        assert_eq!(r.temps[2], Some(52.0));
        assert_eq!(r.temps[3], Some(-128.0), "raw value, the plausibility filter rejects it");
        assert!((r.fans[0].unwrap() - 1500.0).abs() < 1.0);
        assert_eq!(r.duty[0], None, "automatic channel reports no duty");
    }

    /// BIOS-controlled channels get a duty estimate from their decoded
    /// firmware curve (not a reading: `duty` stays `None`), read once and cached.
    #[test]
    fn bios_channels_get_a_duty_estimate_from_the_firmware_curve() {
        let (mut chip, ec) = make_chip(true);
        let r = chip.update().unwrap();
        // PWM1: input 0 (35 °C) is below Tstart 40 → StartPWM 0x40.
        assert_eq!(r.duty[0], None);
        assert!((r.duty_est[0].unwrap() - 64.0 / 2.55).abs() < 0.1, "{:?}", r.duty_est);
        // PWM2: Tstart 30, PWM 0x50, slope 3/°C → 80 + 15 counts at 35 °C.
        assert!((r.duty_est[1].unwrap() - 95.0 / 2.55).abs() < 0.1, "{:?}", r.duty_est);
        assert_eq!(chip.firmware_curve(0).unwrap().0, 0);
        let writes = ec.lock().unwrap().writes.iter().filter(|w| w.1 != REG_BANK).count();
        assert_eq!(writes, 0, "estimating writes nothing but the bank select");
        // Cached: the next poll reads no bank-2 register.
        ec.lock().unwrap().writes.clear();
        chip.update().unwrap();
        assert!(ec.lock().unwrap().writes.is_empty(), "no bank switching on a cached poll");
        // A channel we drive reports its real duty, no estimate.
        chip.set_duty(0, 50.0, 2, 85.0).unwrap();
        let r = chip.update().unwrap();
        assert!(r.duty[0].is_some() && r.duty_est[0].is_none());
        assert!(chip.label(0).ends_with("PWM1 (mb.pwm0)"), "{}", chip.label(0));
    }

    /// The curve's hysteresis is written to ΔT (0x65 for PWM1) and the
    /// firmware value comes back on release.
    #[test]
    fn curve_hysteresis_is_written_and_restored() {
        let (mut chip, ec) = make_chip(true);
        let before = ec.lock().unwrap().snapshot();
        let mut curve = sg::fit(&[(30.0, 30.0), (60.0, 60.0)], 85.0, sg::MAX_SEGMENTS).unwrap();
        curve.dt = Some(1);
        chip.set_curve(0, 2, &curve).unwrap();
        assert_eq!(ec.lock().unwrap().banks[0][0x65], 1);
        curve.dt = Some(2);
        chip.set_curve(0, 2, &curve).unwrap();
        assert_eq!(ec.lock().unwrap().banks[0][0x65], 2);
        chip.release(0).unwrap();
        assert_eq!(ec.lock().unwrap().banks[0][0x65], 3, "BIOS ΔT restored");
        assert_eq!(ec.lock().unwrap().snapshot(), before);
    }

    #[test]
    fn caps_v3_layout() {
        assert!(Caps::for_model(IteModel::IT8689E, 1).v3);
        assert!(Caps::for_model(IteModel::IT8688E, 2).v3);
        assert_eq!(Caps::for_model(IteModel::IT8688E, 2).banks, 4);
        assert!(!Caps::for_model(IteModel::IT8688E, 2).no_addr_verify);
        assert!(!Caps::for_model(IteModel::IT8688E, 1).v3);
        assert!(Caps::for_model(IteModel::IT8688E, 1).no_addr_verify);
        assert!(!Caps::for_model(IteModel::IT8686E, 1).v3);
    }

    #[test]
    fn flat_duty_is_a_hardware_curve_with_failsafe() {
        let (mut chip, ec) = make_chip(true);
        let before = ec.lock().unwrap().snapshot();
        chip.set_duty(0, 50.0, 2, 85.0).unwrap();
        assert!(chip.has_hw_failsafe(0));
        let (ctrl, img) = image_of(&ec, 0);
        assert_eq!(ctrl, 0x80 | (2 << 3) | 0x02, "automatic mode on the CPU input, firmware bits kept");
        let c = sg::decode(&img);
        assert_eq!(c, sg::flat(50.0, 85.0));
        assert_eq!(img.sg[SG_PWM], 128);
        assert_eq!(img.sg[SG_TFULL], 85);
        assert_eq!(img.bank2[2] & 0x80, 0, "extra vectors disabled");
        assert_eq!(img.bank3[sg::B3_ENABLE] & 0x3F, 0);
        assert_eq!(img.bank3[sg::B3_ENABLE] & 0xC0, 0);
        {
            let e = ec.lock().unwrap();
            assert_eq!(e.bank(), 0, "bank 0 selected afterwards");
            assert_eq!(e.banks[0][REG_FAN_MAIN_CTRL as usize], 0x77, "0x13 untouched");
            assert!(!e.writes.iter().any(|w| w.0 == 0 && w.1 == REG_FAN_MAIN_CTRL));
        }
        // The chip runs it: at 52 °C on input 2 the duty estimate is the flat duty.
        let r = chip.update().unwrap();
        assert!((r.duty[0].unwrap() - 50.2).abs() < 0.5);
        assert_ctrl_writes_safe(&ec);

        chip.release(0).unwrap();
        assert_eq!(ec.lock().unwrap().snapshot(), before, "every bank restored exactly");
        assert_eq!(chip.mode(0), ChanMode::Firmware);
    }

    #[test]
    fn curve_roundtrip_and_exact_restore() {
        let (mut chip, ec) = make_chip(true);
        let before = ec.lock().unwrap().snapshot();
        let pts = [
            (25.0, 20.0),
            (35.0, 25.0),
            (45.0, 35.0),
            (55.0, 50.0),
            (60.0, 60.0),
            (65.0, 70.0),
            (70.0, 80.0),
            (75.0, 90.0),
        ];
        let curve = sg::fit(&pts, 85.0, sg::MAX_SEGMENTS).unwrap();
        assert!(curve.segs.len() > 4, "uses bank 2 and bank 3 vectors");
        chip.set_curve(0, 2, &curve).unwrap();
        let (ctrl, img) = image_of(&ec, 0);
        assert_eq!(ctrl & 0xB8, 0x80 | (2 << 3));
        assert_eq!(sg::decode(&img), curve, "registers decode to the curve we fitted");
        assert_eq!(img.bank2[sg::B2_MAIN_FRAC] & 0x8F, 0x0B, "foreign bits of the slope-fraction register kept");
        assert_eq!(img.sg[sg::SG_DT], 3, "ΔT untouched");
        assert_ctrl_writes_safe(&ec);
        chip.release(0).unwrap();
        assert_eq!(ec.lock().unwrap().snapshot(), before);
    }

    #[test]
    fn writes_are_idempotent() {
        let (mut chip, ec) = make_chip(true);
        chip.set_duty(0, 40.0, 2, 85.0).unwrap();
        ec.lock().unwrap().writes.clear();
        chip.set_duty(0, 40.0, 2, 85.0).unwrap();
        assert!(ec.lock().unwrap().writes.is_empty(), "same duty: no writes at all");
        chip.set_duty(0, 41.0, 2, 85.0).unwrap();
        {
            let e = ec.lock().unwrap();
            let regs: Vec<u8> = e.writes.iter().map(|w| w.1).collect();
            assert!(regs.iter().all(|r| *r == 0x61 || *r == 0x63), "only Tstart/StartPWM rewritten: {regs:x?}");
            assert!(!e.writes.iter().any(|w| w.1 == REG_BANK), "no bank switching");
        }
        // Polling does not write while nothing changed.
        ec.lock().unwrap().writes.clear();
        for _ in 0..(FULL_VERIFY_EVERY + 2) {
            chip.update().unwrap();
        }
        assert!(
            !ec.lock().unwrap().writes.iter().any(|w| w.1 != REG_BANK),
            "polling only switches banks for verification"
        );
    }

    #[test]
    fn firmware_override_is_reapplied() {
        let (mut chip, ec) = make_chip(true);
        chip.set_duty(0, 60.0, 2, 85.0).unwrap();
        let want = image_of(&ec, 0);
        // Resume from sleep: the BIOS reprograms its own curve.
        {
            let mut e = ec.lock().unwrap();
            e.banks[0][0x15] = 0x82;
            e.banks[0][0x60..0x66].copy_from_slice(&[0, 40, 70, 0x40, 4, 3]);
            e.banks[2][0x12] = 0x85;
        }
        chip.update().unwrap();
        assert_eq!(image_of(&ec, 0), want, "curve re-applied, including bank 2");
        let ev = chip.take_events();
        assert!(ev.iter().any(|e| e.code == ErrorCode::FirmwareOverride));
        // Explicit re-apply (resume notification) also works.
        ec.lock().unwrap().banks[0][0x63] = 0x10;
        assert_eq!(chip.reapply(), 1);
        assert_eq!(image_of(&ec, 0), want);
    }

    #[test]
    fn persistent_override_gives_fan_back() {
        let (mut chip, ec) = make_chip(true);
        chip.set_duty(0, 60.0, 2, 85.0).unwrap();
        ec.lock().unwrap().stuck.insert((0, 0x15));
        for _ in 0..3 {
            ec.lock().unwrap().banks[0][0x15] = 0x82;
            chip.update().unwrap();
        }
        // Writes to 0x15 no longer stick: the curve fails verification and the fan is released.
        assert_eq!(chip.mode(0), ChanMode::Firmware);
        assert!(!chip.has_hw_failsafe(0) || chip.chans[0].gave_up_at.is_some());
        assert!(chip.take_events().iter().any(|e| e.code == ErrorCode::RegisterMismatch));
    }

    #[test]
    fn curve_mismatch_falls_back_to_manual_then_bios() {
        let (mut chip, ec) = make_chip(true);
        // Crash protection (watchdog + snapshot) active: the software fallback is allowed.
        chip.set_software_fallback(true);
        let before = ec.lock().unwrap().snapshot();
        // Bank-2 slope fraction register does not take writes.
        ec.lock().unwrap().stuck.insert((2, 0x1C));
        chip.set_duty(0, 33.0, 2, 85.0).unwrap();
        assert!(!chip.has_hw_failsafe(0));
        assert_eq!(chip.mode(0), ChanMode::Manual);
        let (ctrl, img) = image_of(&ec, 0);
        assert_eq!(ctrl & 0x80, 0);
        assert_eq!(ctrl, 0x02, "software mode keeps the temperature input, never 0x7F");
        assert_eq!(img.sg[SG_PWM], 84);
        assert!(chip.set_curve(0, 2, &sg::flat(20.0, 80.0)).is_err());
        // Manual writes failing as well: the fan goes back to the BIOS with a coded error.
        ec.lock().unwrap().reject.insert((0, 0x63, 179));
        let e = chip.set_duty(0, 70.0, 2, 85.0).unwrap_err();
        assert_eq!(code_of(&e), ErrorCode::RegisterMismatch);
        assert_eq!(chip.mode(0), ChanMode::Firmware);
        let after = ec.lock().unwrap().snapshot();
        assert_eq!(after[0][0x15], before[0][0x15]);
        assert_eq!(after[2][0x12], before[2][0x12]);
        assert_ctrl_writes_safe(&ec);
    }

    #[test]
    fn stale_manual_state_is_released_to_automatic() {
        // A previous crash left PWM1 in software mode with LHM's 0x7F.
        let (mut chip, ec) = make_chip(true);
        ec.lock().unwrap().banks[0][0x15] = 0x7F;
        chip.set_duty(0, 30.0, 2, 85.0).unwrap();
        chip.release(0).unwrap();
        let (ctrl, img) = image_of(&ec, 0);
        assert_eq!(ctrl, 0x80 | (2 << 3), "automatic mode, sane temperature input");
        assert_eq!(img.sg[SG_TFULL], 70, "saved BIOS curve kept");
        assert_ctrl_writes_safe(&ec);

        // Same without any usable curve: a safe default curve is written.
        let (mut chip, ec) = make_chip(true);
        {
            let mut e = ec.lock().unwrap();
            e.banks[0][0x15] = 0x02;
            e.banks[0][0x60..0x66].copy_from_slice(&[0, 0, 0, 0, 0, 0]);
            e.banks[2][0x1C] = 0;
        }
        chip.set_duty(0, 30.0, 2, 85.0).unwrap();
        chip.release(0).unwrap();
        let (ctrl, img) = image_of(&ec, 0);
        assert_eq!(ctrl, 0x80 | (sg::SAFE_DEFAULT_INPUT << 3));
        assert_eq!(sg::decode(&img), sg::safe_default());
    }

    #[test]
    fn crash_snapshot_restores_all_banks() {
        let (mut chip, ec) = make_chip(true);
        let before = ec.lock().unwrap().snapshot();
        let curve =
            sg::fit(&[(20.0, 20.0), (40.0, 30.0), (50.0, 45.0), (60.0, 60.0), (70.0, 80.0)], 80.0, sg::MAX_SEGMENTS)
                .unwrap();
        chip.set_curve(0, 2, &curve).unwrap();
        chip.set_duty(1, 25.0, 2, 85.0).unwrap();
        let state = chip.saved_state();
        assert_eq!(state.len(), 2);
        let json = serde_json::to_string(&state).unwrap();
        let state: Vec<(usize, SavedPwm)> = serde_json::from_str(&json).unwrap();
        // A fresh driver instance (watchdog) restores from the snapshot.
        drop(chip);
        let mut chip2 = IteChip::new(Box::new(MockPort(ec.clone(), 0xA40)), IteModel::IT8689E, 1, 0xA40, true).unwrap();
        assert_eq!(chip2.restore_saved(&state).unwrap(), 2);
        assert_eq!(ec.lock().unwrap().snapshot(), before);
    }

    #[test]
    fn legacy_snapshot_still_restores() {
        let (mut chip, ec) = make_chip(true);
        ec.lock().unwrap().banks[0][0x15] = 0x02;
        ec.lock().unwrap().banks[2][0x12] = 0x05;
        let json = r#"[[0,{"ctrl":130,"duty":64,"main_bit":true,"extvec":[133,128,129,63]}]]"#;
        let state: Vec<(usize, SavedPwm)> = serde_json::from_str(json).unwrap();
        assert_eq!(chip.restore_saved(&state).unwrap(), 1);
        let e = ec.lock().unwrap();
        assert_eq!(e.banks[0][0x15], 0x82);
        assert_eq!(e.banks[2][0x12], 0x85);
        assert_eq!(e.bank(), 0);
    }

    #[test]
    fn it8688e_rev2_returns_to_bank0() {
        let (mut chip, ec) = make(IteModel::IT8688E, 2, true);
        assert!(chip.hw_curves());
        // Something left the chip in bank 2.
        {
            let mut e = ec.lock().unwrap();
            e.banks[0][REG_BANK as usize] = 2 << 5;
        }
        let r = chip.update().unwrap();
        assert_eq!(ec.lock().unwrap().bank(), 0);
        assert_eq!(r.temps[0], Some(35.0));
        chip.set_duty(0, 50.0, 2, 85.0).unwrap();
        let e = ec.lock().unwrap();
        assert_eq!(e.bank(), 0);
        assert_eq!(e.banks[0][0x63], 128, "duty landed in bank 0");
        assert_eq!(e.banks[0][0x15] & 0x80, 0x80);
        assert_eq!(e.banks[2][0x13], 0x60, "bank-2 data not clobbered");
    }

    #[test]
    fn non_gigabyte_uses_software_pwm_and_leaves_vectors_alone() {
        let (mut chip, ec) = make_chip(false);
        assert!(!chip.hw_curves());
        chip.set_duty(0, 30.0, 2, 85.0).unwrap();
        {
            let e = ec.lock().unwrap();
            assert_eq!(e.banks[2][0x12], 0x85);
            assert_eq!(e.banks[0][0x15], 0x02);
            assert_eq!(e.banks[0][0x63], 77);
            assert_eq!(e.banks[0][REG_FAN_MAIN_CTRL as usize], 0x77);
        }
        ec.lock().unwrap().writes.clear();
        chip.set_duty(0, 30.0, 2, 85.0).unwrap();
        assert!(ec.lock().unwrap().writes.is_empty());
        assert_eq!(code_of(&chip.set_curve(0, 2, &sg::flat(20.0, 80.0)).unwrap_err()), ErrorCode::FanReadOnly);
        // Resume: firmware restores automatic mode, we re-apply.
        ec.lock().unwrap().banks[0][0x15] = 0x82;
        chip.update().unwrap();
        assert_eq!(ec.lock().unwrap().banks[0][0x15], 0x02);
        chip.release_all().unwrap();
        assert_eq!(ec.lock().unwrap().banks[0][0x15], 0x82);
        assert_eq!(ec.lock().unwrap().banks[0][0x63], 0x40);
        assert_ctrl_writes_safe(&ec);
    }

    #[test]
    fn coded_errors() {
        let (mut chip, _ec) = make_chip(true);
        assert_eq!(code_of(&chip.set_duty(7, 50.0, 2, 85.0).unwrap_err()), ErrorCode::NoSuchFan);
        let ec = Arc::new(Mutex::new(MockEc::default()));
        let e = IteChip::new(Box::new(MockPort(ec, 0xA40)), IteModel::IT8689E, 1, 0xA40, true).err().unwrap();
        assert_eq!(code_of(&e), ErrorCode::ChipInvalid);
    }

    /// After a failed release, polling retries the release and never re-applies
    /// our own curve as if the firmware had overridden it.
    #[test]
    fn failed_release_is_retried_not_reapplied() {
        let (mut chip, ec) = make_chip(true);
        chip.set_duty(0, 30.0, 2, 85.0).unwrap();
        assert_eq!(ec.lock().unwrap().banks[0][0x63], 77);
        // StartPWM ignores writes: the release cannot verify.
        ec.lock().unwrap().stuck.insert((0, 0x63));
        assert_eq!(code_of(&chip.release(0).unwrap_err()), ErrorCode::RestoreFailed);
        let _ = chip.take_events();
        for _ in 0..3 {
            chip.update().unwrap();
        }
        assert_eq!(ec.lock().unwrap().banks[0][0x62], 70, "BIOS Tfull stays; our 85 is not written back");
        let ev = chip.take_events();
        assert!(!ev.iter().any(|x| x.code == ErrorCode::FirmwareOverride), "{ev:?}");
        assert!(ev.iter().any(|x| x.code == ErrorCode::RestoreFailed), "{ev:?}");
        assert!(!chip.saved_state().is_empty(), "saved BIOS state kept for the crash snapshot");
        // The register works again: the next poll completes the release.
        ec.lock().unwrap().stuck.clear();
        chip.update().unwrap();
        assert_eq!(ec.lock().unwrap().banks[0][0x63], 0x40);
        assert_eq!(chip.mode(0), ChanMode::Firmware);
        assert!(chip.saved_state().is_empty());
        assert!(!chip.take_events().iter().any(|x| x.code == ErrorCode::FirmwareOverride));
    }

    /// One corrupted read-back does not disable hardware curves.
    #[test]
    fn transient_mismatch_keeps_hardware_curve() {
        let (mut chip, ec) = make_chip(true);
        ec.lock().unwrap().flaky.insert((0, 0x63));
        chip.set_duty(0, 50.0, 2, 85.0).unwrap();
        assert_eq!(chip.mode(0), ChanMode::Curve);
        assert!(chip.has_hw_failsafe(0));
        assert_eq!(ec.lock().unwrap().banks[0][0x63], 128);
    }

    /// A persistent mismatch pauses hardware curves with a back-off (not forever)
    /// and, without crash protection, hands the fan to the BIOS instead of software PWM.
    #[test]
    fn curve_failure_backs_off_and_never_uses_unprotected_software_pwm() {
        let (mut chip, ec) = make_chip(true);
        let before = ec.lock().unwrap().snapshot();
        ec.lock().unwrap().stuck.insert((2, 0x1C));
        let e = chip.set_duty(0, 33.0, 2, 85.0).unwrap_err();
        assert_eq!(code_of(&e), ErrorCode::RegisterMismatch);
        assert_eq!(chip.mode(0), ChanMode::Firmware);
        assert!(!chip.has_hw_failsafe(0));
        assert_eq!(ec.lock().unwrap().snapshot(), before, "BIOS curve restored");
        // Paused: refused without touching the chip.
        ec.lock().unwrap().writes.clear();
        assert!(chip.set_duty(0, 40.0, 2, 85.0).is_err());
        assert!(ec.lock().unwrap().writes.iter().all(|w| w.1 == REG_BANK), "no fan register written");
        // The register works again and the pause is over: back to a hardware curve.
        ec.lock().unwrap().stuck.clear();
        chip.chans[0].curve_retry_at = Some(Instant::now() - Duration::from_millis(1));
        chip.set_duty(0, 40.0, 2, 85.0).unwrap();
        assert_eq!(chip.mode(0), ChanMode::Curve);
        assert!(chip.has_hw_failsafe(0));
        assert_eq!(chip.chans[0].curve_failures, 0);
        assert_ctrl_writes_safe(&ec);
    }
}
