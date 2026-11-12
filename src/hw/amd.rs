//! AMD Zen (family 17h/19h/1Ah) temperatures and package power through PawnIO's
//! `AMDFamily17` module. Uses the register facts documented by Linux k10temp and LibreHardwareMonitor's
//! `Amd17Cpu.cs`.

use std::time::Instant;

use anyhow::Result;

use crate::error::{ErrorCode, err};

use super::pawnio::{MODULE_AMD17, PawnIo};
use super::winmutex::GlobalMutex;

const SMN_THM_TCON_CUR_TMP: u32 = 0x0005_9800;
const SMN_CCD_TEMP_ZEN2: u32 = 0x0005_9954;
const SMN_CCD_TEMP_ZEN4: u32 = 0x0005_9b08;
const TEMP_RANGE_SEL_MASK: u32 = 0x80000;
const TEMP_TJ_SEL_MASK: u32 = 0x30000;
const CCD_TEMP_VALID: u32 = 0x800;

const MSR_PWR_UNIT: u64 = 0xC001_0299;
const MSR_PKG_ENERGY_STAT: u64 = 0xC001_029B;

#[derive(Clone, Debug)]
pub struct CpuId {
    pub vendor: String,
    pub family: u32,
    pub model: u32,
    pub brand: String,
}

pub fn cpuid() -> CpuId {
    use std::arch::x86_64::__cpuid;
    #[allow(unused_unsafe)]
    let (v0, v1) = unsafe { (__cpuid(0), __cpuid(1)) };
    let mut vendor = Vec::new();
    for r in [v0.ebx, v0.edx, v0.ecx] {
        vendor.extend_from_slice(&r.to_le_bytes());
    }
    let base_family = (v1.eax >> 8) & 0xF;
    let base_model = (v1.eax >> 4) & 0xF;
    let ext_family = (v1.eax >> 20) & 0xFF;
    let ext_model = (v1.eax >> 16) & 0xF;
    let family = if base_family == 0xF { base_family + ext_family } else { base_family };
    let model = if base_family == 0xF || base_family == 0x6 { (ext_model << 4) | base_model } else { base_model };

    let mut brand = Vec::new();
    #[allow(unused_unsafe)]
    let max_ext = unsafe { __cpuid(0x8000_0000) }.eax;
    if max_ext >= 0x8000_0004 {
        for leaf in 0x8000_0002u32..=0x8000_0004 {
            #[allow(unused_unsafe)]
            let r = unsafe { __cpuid(leaf) };
            for reg in [r.eax, r.ebx, r.ecx, r.edx] {
                brand.extend_from_slice(&reg.to_le_bytes());
            }
        }
    }
    CpuId {
        vendor: String::from_utf8_lossy(&vendor).to_string(),
        family,
        model,
        brand: String::from_utf8_lossy(&brand).trim_matches(char::from(0)).trim().to_string(),
    }
}

#[derive(Clone, Debug, Default)]
pub struct AmdReadings {
    /// Control temperature (Tctl, offset corrected to Tdie where applicable).
    pub tctl: Option<f32>,
    pub ccds: Vec<Option<f32>>,
    /// Package power in watts (RAPL energy counter).
    pub package_power: Option<f32>,
}

pub struct AmdCpu {
    pawn: PawnIo,
    ccd_base: Option<u32>,
    ccd_count: usize,
    energy_unit_j: Option<f64>,
    last_energy: Option<(u32, Instant)>,
    tctl_offset: f32,
}

impl AmdCpu {
    pub fn supported(id: &CpuId) -> bool {
        id.vendor == "AuthenticAMD" && matches!(id.family, 0x17 | 0x19 | 0x1A)
    }

    pub fn open(id: CpuId) -> std::result::Result<Self, super::pawnio::PawnIoStatus> {
        let pawn = PawnIo::open("AMDFamily17", MODULE_AMD17)?;
        let ccd_base = match (id.family, id.model) {
            (0x17, 0x31) | (0x17, 0x71) | (0x19, 0x01) | (0x19, 0x21) => Some(SMN_CCD_TEMP_ZEN2),
            (0x19, 0x61) | (0x1A, 0x44) => Some(SMN_CCD_TEMP_ZEN4),
            _ => None,
        };
        // k10temp offsets for early Zen parts that report Tctl above Tdie.
        let tctl_offset = if ["1600X", "1700X", "1800X"].iter().any(|s| id.brand.contains(s)) {
            -20.0
        } else if id.brand.contains("2700X") {
            -10.0
        } else if id.brand.contains("Threadripper 19") || id.brand.contains("Threadripper 29") {
            -27.0
        } else {
            0.0
        };
        let mut cpu = AmdCpu { pawn, ccd_base, ccd_count: 0, energy_unit_j: None, last_energy: None, tctl_offset };
        if let Ok(unit) = cpu.read_msr(MSR_PWR_UNIT) {
            let esu = (unit >> 8) & 0x1F;
            cpu.energy_unit_j = Some(1.0 / (1u64 << esu) as f64);
        }
        Ok(cpu)
    }

    fn read_smn(&self, offset: u32) -> Result<u32> {
        Ok(self.pawn.call1("ioctl_read_smn", &[offset as u64])? as u32)
    }

    fn read_msr(&self, msr: u64) -> Result<u64> {
        self.pawn.call1("ioctl_read_msr", &[msr])
    }

    pub fn read(&mut self, pci: &GlobalMutex) -> Result<AmdReadings> {
        let mut out = AmdReadings::default();
        {
            let Some(_guard) = pci.lock(50) else {
                return Err(err(ErrorCode::BusBusy, "PCI bus busy (another tool holds the PCI lock)"));
            };
            let raw = self
                .read_smn(SMN_THM_TCON_CUR_TMP)
                .map_err(|e| err(ErrorCode::CpuSensor, format!("Tctl read: {e:#}")))?;
            let mut t = (raw >> 21) as f32 * 0.125;
            if raw & TEMP_RANGE_SEL_MASK != 0 || raw & TEMP_TJ_SEL_MASK == TEMP_TJ_SEL_MASK {
                t -= 49.0;
            }
            t += self.tctl_offset;
            if (0.0..150.0).contains(&t) {
                out.tctl = Some(t);
            }

            if let Some(base) = self.ccd_base {
                let probe = if self.ccd_count == 0 { 8 } else { self.ccd_count };
                let mut found = 0;
                for i in 0..probe {
                    let raw = self.read_smn(base + i as u32 * 4).unwrap_or(0);
                    if raw & CCD_TEMP_VALID != 0 {
                        let t = (raw & 0x7FF) as f32 * 0.125 - 49.0;
                        out.ccds.push(Some(t).filter(|t| (0.0..150.0).contains(t)));
                        found = i + 1;
                    } else {
                        out.ccds.push(None);
                    }
                }
                if self.ccd_count == 0 {
                    self.ccd_count = found;
                }
                out.ccds.truncate(self.ccd_count);
            }
        }

        if let Some(unit) = self.energy_unit_j
            && let Ok(e) = self.read_msr(MSR_PKG_ENERGY_STAT)
        {
            let e = e as u32;
            let now = Instant::now();
            if let Some((prev, at)) = self.last_energy {
                let dt = now.duration_since(at).as_secs_f64();
                if dt > 0.05 {
                    let delta = e.wrapping_sub(prev) as f64 * unit;
                    let w = delta / dt;
                    if (0.0..1000.0).contains(&w) {
                        out.package_power = Some(w as f32);
                    }
                }
            }
            self.last_energy = Some((e, now));
        }
        Ok(out)
    }
}
