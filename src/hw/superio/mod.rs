// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.
// Parts of this file (the ITE probe) are derived from LibreHardwareMonitor's LpcIO.cs:
// Copyright (C) LibreHardwareMonitor and Contributors.
// Partial Copyright (C) Michael Möller <mmoeller@openhardwaremonitor.org> and Contributors.
// All Rights Reserved.
// Changes and additions: Copyright 2026 The FanRGB Authors.

//! Super I/O discovery through PawnIO's `LpcIO` module.

pub mod ite;
pub mod sg;

use std::time::Duration;

use anyhow::Result;

use super::pawnio::{MODULE_LPCIO, PawnIo, PawnIoStatus};
use super::winmutex::GlobalMutex;
use crate::error::{ErrorCode, err};
use ite::{IteChip, IteModel};

/// Byte-wide port I/O, abstracted for testing.
pub trait PortIo: Send {
    fn inb(&mut self, port: u16) -> Result<u8>;
    fn outb(&mut self, port: u16, value: u8) -> Result<()>;
}

const CHIP_ID_REGISTER: u8 = 0x20;
const CHIP_VERSION_REGISTER: u8 = 0x22;
const DEVICE_SELECT_REGISTER: u8 = 0x07;
const BASE_ADDRESS_REGISTER: u8 = 0x60;
const IT87_ENVIRONMENT_CONTROLLER_LDN: u8 = 0x04;
/// GIGABYTE's software waits 2 ms after entering/leaving configuration mode and LDN selection.
const CONFIG_DELAY: Duration = Duration::from_millis(2);

/// One Super I/O configuration port (0x2E or 0x4E) backed by its own LpcIO instance.
pub struct LpcPort {
    pawn: PawnIo,
    pub register_port: u16,
}

impl LpcPort {
    pub fn open(slot: u8) -> std::result::Result<Self, PawnIoStatus> {
        let pawn = PawnIo::open("LpcIO", MODULE_LPCIO)?;
        pawn.call0("ioctl_select_slot", &[slot as u64]).map_err(|e| PawnIoStatus::Error(format!("{e:#}")))?;
        Ok(Self { pawn, register_port: if slot == 0 { 0x2E } else { 0x4E } })
    }

    fn value_port(&self) -> u16 {
        self.register_port + 1
    }

    pub fn sio_inb(&self, reg: u8) -> Result<u8> {
        Ok(self.pawn.call1("ioctl_superio_inb", &[reg as u64])? as u8)
    }

    pub fn sio_inw(&self, reg: u8) -> Result<u16> {
        Ok(self.pawn.call1("ioctl_superio_inw", &[reg as u64])? as u16)
    }

    pub fn sio_outb(&self, reg: u8, value: u8) -> Result<()> {
        self.pawn.call0("ioctl_superio_outb", &[reg as u64, value as u64])
    }

    pub fn find_bars(&self) -> Result<()> {
        self.pawn.call0("ioctl_find_bars", &[])
    }

    fn pio_out(&self, port: u16, value: u8) -> Result<()> {
        self.pawn.call0("ioctl_pio_outb", &[port as u64, value as u64])
    }

    fn pio_in(&self, port: u16) -> Result<u8> {
        Ok(self.pawn.call1("ioctl_pio_inb", &[port as u64])? as u8)
    }

    pub fn ite_enter(&self) -> Result<()> {
        let p = self.register_port;
        self.pio_out(p, 0x87)?;
        self.pio_out(p, 0x01)?;
        self.pio_out(p, 0x55)?;
        self.pio_out(p, if p == 0x4E { 0xAA } else { 0x55 })?;
        std::thread::sleep(CONFIG_DELAY);
        Ok(())
    }

    pub fn ite_exit(&self) -> Result<()> {
        // Secondary chips at 0x4E must stay in configuration mode (LHM / it87 / GCC behaviour).
        if self.register_port != 0x4E {
            self.pio_out(self.register_port, 0x02)?;
            self.pio_out(self.value_port(), 0x02)?;
            std::thread::sleep(CONFIG_DELAY);
        }
        Ok(())
    }

    fn nuvoton_enter(&self) -> Result<()> {
        self.pio_out(self.register_port, 0x87)?;
        self.pio_out(self.register_port, 0x87)
    }

    fn nuvoton_exit(&self) -> Result<()> {
        self.pio_out(self.register_port, 0xAA)
    }
}

impl PortIo for LpcPort {
    fn inb(&mut self, port: u16) -> Result<u8> {
        self.pio_in(port)
    }
    fn outb(&mut self, port: u16, value: u8) -> Result<()> {
        self.pio_out(port, value)
    }
}

pub struct Detection {
    pub chips: Vec<IteChip>,
    pub report: Vec<String>,
    pub status: PawnIoStatus,
}

/// Probes both configuration ports for ITE chips.
pub fn detect(isa: &GlobalMutex, gigabyte: bool) -> Detection {
    let mut det = Detection { chips: Vec::new(), report: Vec::new(), status: PawnIoStatus::Unknown };
    for slot in 0..2u8 {
        let port = match LpcPort::open(slot) {
            Ok(p) => p,
            Err(status) => {
                det.report.push(format!("LpcIO slot {slot}: {}", status.describe()));
                if det.chips.is_empty() {
                    det.status = status;
                }
                break;
            }
        };
        det.status = PawnIoStatus::Ready;
        let Some(guard) = isa.lock(250) else {
            det.report.push(format!(
                "[{}] ISA bus is busy (another monitoring tool holds the lock); skipped",
                ErrorCode::BusBusy
            ));
            continue;
        };
        match probe_ite(port, gigabyte) {
            Ok(Some(chip)) => {
                det.report.push(format!(
                    "Found {} (version {}) at {:#06x}, environment controller base {:#06x}{}",
                    chip.model.name(),
                    chip.version,
                    if slot == 0 { 0x2E } else { 0x4E },
                    chip.base,
                    if chip.hw_curves() { ", fan curves run on the chip (GIGABYTE SmartGuardian)" } else { "" }
                ));
                det.chips.push(chip);
            }
            Ok(None) => {}
            Err(e) => det.report.push(format!("Probe at slot {slot} failed: {e:#}")),
        }
        drop(guard);
    }
    if det.chips.is_empty() && det.status == PawnIoStatus::Ready {
        det.report.push(format!("[{}] No supported ITE Super I/O chip found", ErrorCode::NoSuperIo));
    }
    det
}

/// True if `id` is a chip id we know, i.e. the chip already answers in configuration mode.
fn known_ite(id: u16) -> bool {
    IteModel::from_chip_id(id).is_some()
}

fn probe_ite(port: LpcPort, gigabyte: bool) -> Result<Option<IteChip>> {
    let regp = port.register_port;
    // Like GIGABYTE's software: only enter configuration mode if the chip does not already answer
    // (another tool may be mid-session), and only leave it if we entered it.
    let mut chip_id = port.sio_inw(CHIP_ID_REGISTER).unwrap_or(0xFFFF);
    let mut entered = false;
    if !known_ite(chip_id) {
        // Never leave the chip in configuration mode on an error path (best effort: the same
        // driver call that failed may fail again).
        if let Err(e) = port.ite_enter() {
            let _ = port.ite_exit();
            return Err(e);
        }
        entered = true;
        chip_id = match port.sio_inw(CHIP_ID_REGISTER) {
            Ok(id) => id,
            Err(e) => {
                let _ = port.ite_exit();
                return Err(e);
            }
        };
    }
    let exit = |port: &LpcPort| -> Result<()> { if entered { port.ite_exit() } else { Ok(()) } };

    let Some(model) = IteModel::from_chip_id(chip_id) else {
        if chip_id != 0 && chip_id != 0xFFFF {
            exit(&port)?;
            log::info!("unknown ITE-style chip id {chip_id:#06x} at {regp:#04x}");
        } else {
            // Not ITE: identify Nuvoton/Winbond for the report only.
            if let Err(e) = port.nuvoton_enter() {
                let _ = port.nuvoton_exit();
                return Err(e);
            }
            let id = port.sio_inb(CHIP_ID_REGISTER).unwrap_or(0xFF);
            let rev = port.sio_inb(CHIP_ID_REGISTER + 1).unwrap_or(0xFF);
            port.nuvoton_exit()?;
            if id != 0xFF && id != 0 {
                log::warn!("Nuvoton/Winbond chip {id:#04x}{rev:02x} at {regp:#04x} is not supported by this build");
            }
        }
        return Ok(None);
    };

    let probed = (|| -> Result<(u16, u16, u8)> {
        port.find_bars()?;
        port.sio_outb(DEVICE_SELECT_REGISTER, IT87_ENVIRONMENT_CONTROLLER_LDN)?;
        std::thread::sleep(CONFIG_DELAY);
        let address = port.sio_inw(BASE_ADDRESS_REGISTER)?;
        std::thread::sleep(Duration::from_millis(1));
        let verify = port.sio_inw(BASE_ADDRESS_REGISTER)?;
        let version = port.sio_inb(CHIP_VERSION_REGISTER)? & 0x0F;
        Ok((address, verify, version))
    })();
    exit(&port)?;
    let (address, verify, version) = probed?;

    if address != verify || address < 0x100 || (address & 0xF007) != 0 {
        return Err(err(
            ErrorCode::ChipInvalid,
            format!("{} reports invalid base address {address:#06x}/{verify:#06x}", model.name()),
        ));
    }
    let chip = IteChip::new(Box::new(port), model, version, address, gigabyte)?;
    Ok(Some(chip))
}
