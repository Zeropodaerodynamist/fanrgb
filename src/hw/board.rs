//! Motherboard identification and per-board sensor labels.

use crate::model::FanRole;
use crate::util::win::{HKEY_LOCAL_MACHINE, reg_get_string};

use super::superio::ite::IteModel;

#[derive(Clone, Debug, Default)]
pub struct BoardInfo {
    pub manufacturer: String,
    pub product: String,
    pub bios: String,
}

impl BoardInfo {
    pub fn read() -> Self {
        let key = r"HARDWARE\DESCRIPTION\System\BIOS";
        let get = |v: &str| reg_get_string(HKEY_LOCAL_MACHINE, key, v).unwrap_or_default();
        BoardInfo {
            manufacturer: get("BaseBoardManufacturer"),
            product: get("BaseBoardProduct"),
            bios: get("BIOSVersion"),
        }
    }

    pub fn is_gigabyte(&self) -> bool {
        let m = self.manufacturer.to_ascii_lowercase();
        m.contains("gigabyte") || m.contains("aorus")
    }

    pub fn display(&self) -> String {
        if self.product.is_empty() {
            "Unknown motherboard".into()
        } else {
            format!("{} {}", short_vendor(&self.manufacturer), self.product)
        }
    }
}

fn short_vendor(m: &str) -> &str {
    let l = m.to_ascii_lowercase();
    if l.contains("gigabyte") {
        "Gigabyte"
    } else if l.contains("asus") {
        "ASUS"
    } else if l.contains("micro-star") || l.contains("msi") {
        "MSI"
    } else if l.contains("asrock") {
        "ASRock"
    } else {
        m
    }
}

pub struct VoltageLabel {
    pub name: &'static str,
    pub ri: f32,
    pub rf: f32,
}

pub struct SensorLabels {
    pub temps: Vec<String>,
    pub fans: Vec<(String, FanRole)>,
    pub volts: Vec<VoltageLabel>,
    /// Fan headers listed in the board's official specification (None: unknown). Headers whose
    /// tachometer shows a fan are treated as present as well.
    pub documented_headers: Option<Vec<usize>>,
    /// Temperature inputs that are not wired on this board (hidden).
    pub hidden_temps: Vec<usize>,
    /// Index of the temperature sensor that reads the CPU (fallback when SMN is unavailable).
    pub cpu_temp_index: Option<usize>,
}

const fn v(name: &'static str, ri: f32, rf: f32) -> VoltageLabel {
    VoltageLabel { name, ri, rf }
}

/// AM5 boards (600/800-series chipsets).
fn is_amd_board(product: &str) -> bool {
    ["B650", "X670", "A620", "B850", "X870", "B840"].iter().any(|m| product.contains(m))
}

/// Budget micro-ATX boards (B650M D3HP, DS3H, D2H, ...) that route only CPU_FAN, SYS_FAN1, SYS_FAN2
/// and lack the PCIEX16 temperature input (GIGABYTE group ids 90030406/90030506).
fn is_three_fan_board(product: &str) -> bool {
    product.contains("D3HP") || product.contains("DS3H") || product.contains("D2H")
}

/// Labels for the primary ITE chip. Layout from GIGABYTE's AM5 600-series placement tables
/// PWM 0 CPU, 1-3 System 1-3, 4 CPU OPT, 5 System 4 / pump;
/// temperatures 0 System 1, 1 PCH, 2 CPU, 3 PCIEX16, 4 VRM MOS, 5 VSOC MOS.
pub fn labels_for(board: &BoardInfo, chip: IteModel, temps: usize, fans: usize, volts: usize) -> SensorLabels {
    let product = board.product.to_ascii_uppercase();
    if board.is_gigabyte()
        && matches!(chip, IteModel::IT8689E | IteModel::IT8688E | IteModel::IT8686E | IteModel::IT8696E)
    {
        let amd = is_amd_board(&product);
        let temp_names: Vec<String> = if amd {
            ["System 1", "Chipset (PCH)", "CPU", "PCIe x16", "VRM MOS", "VSoC MOS"]
        } else {
            ["System 1", "Chipset (PCH)", "CPU", "PCIe x16", "VRM MOS", "System 2"]
        }
        .iter()
        .map(|s| s.to_string())
        .collect();
        let fan_names = vec![
            ("CPU_FAN".to_string(), FanRole::Cpu),
            ("SYS_FAN1".to_string(), FanRole::System),
            ("SYS_FAN2".to_string(), FanRole::System),
            ("SYS_FAN3".to_string(), FanRole::System),
            ("CPU_OPT".to_string(), FanRole::Cpu),
            ("SYS_FAN4 / PUMP".to_string(), FanRole::Pump),
        ];
        let volts_l = vec![
            v("Vcore", 0.0, 1.0),
            v("+3.3V", 29.4, 45.3),
            v("+12V", 10.0, 2.0),
            v("+5V", 15.0, 10.0),
            v(if amd { "Vcore SoC" } else { "VCCIN Aux" }, 0.0, 1.0),
            v(if amd { "Vcore Misc" } else { "VCC SA" }, 0.0, 1.0),
            // GIGABYTE reads VIN6 unscaled ("CPU VDDIO MEM" where labelled).
            v("VDDIO / DRAM", 0.0, 1.0),
            v("+3V Standby", 10.0, 10.0),
            v("CMOS Battery", 10.0, 10.0),
            v("AVCC3", 10.0, 10.0),
        ];
        let three = is_three_fan_board(&product);
        return SensorLabels {
            temps: pad(temp_names, temps, "Temperature"),
            fans: pad_fans(fan_names, fans),
            volts: volts_l.into_iter().take(volts).collect(),
            documented_headers: three.then(|| vec![0, 1, 2]),
            hidden_temps: if three && amd { vec![3] } else { Vec::new() },
            cpu_temp_index: Some(2),
        };
    }
    SensorLabels {
        temps: pad(vec![], temps, "Temperature"),
        fans: pad_fans(vec![], fans),
        volts: (0..volts).map(|_| v("Voltage", 0.0, 1.0)).collect(),
        documented_headers: None,
        hidden_temps: Vec::new(),
        cpu_temp_index: None,
    }
}

fn pad(mut names: Vec<String>, n: usize, prefix: &str) -> Vec<String> {
    names.truncate(n);
    while names.len() < n {
        names.push(format!("{prefix} #{}", names.len() + 1));
    }
    names
}

fn pad_fans(mut names: Vec<(String, FanRole)>, n: usize) -> Vec<(String, FanRole)> {
    names.truncate(n);
    while names.len() < n {
        let role = if names.is_empty() { FanRole::Cpu } else { FanRole::System };
        names.push((format!("Fan #{}", names.len() + 1), role));
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d3hp() -> BoardInfo {
        BoardInfo {
            manufacturer: "Gigabyte Technology Co., Ltd.".into(),
            product: "B650M D3HP AX".into(),
            bios: "F36d".into(),
        }
    }

    #[test]
    fn am5_layout_matches_gigabyte() {
        let l = labels_for(&d3hp(), IteModel::IT8689E, 6, 6, 10);
        let fans: Vec<&str> = l.fans.iter().map(|f| f.0.as_str()).collect();
        assert_eq!(fans, ["CPU_FAN", "SYS_FAN1", "SYS_FAN2", "SYS_FAN3", "CPU_OPT", "SYS_FAN4 / PUMP"]);
        assert_eq!(l.fans[4].1, FanRole::Cpu);
        assert_eq!(l.fans[5].1, FanRole::Pump);
        assert_eq!(l.temps[0], "System 1");
        assert_eq!(l.temps[2], "CPU");
        assert_eq!(l.temps[5], "VSoC MOS");
        assert_eq!(l.documented_headers, Some(vec![0, 1, 2]));
        assert_eq!(l.hidden_temps, vec![3]);
        assert_eq!(l.cpu_temp_index, Some(2));
        let vin6 = &l.volts[6];
        assert_eq!((vin6.ri, vin6.rf), (0.0, 1.0), "VIN6 unscaled");
    }
}
