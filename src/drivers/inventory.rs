//! Phase 1: what is installed. Read-only and offline: SetupAPI device properties (the same values Device
//! Manager shows), the BIOS from the SMBIOS copy in `HKLM\HARDWARE\DESCRIPTION\System\BIOS`, and the AMD
//! chipset package version from the uninstall entries. No WMI, no network, nothing is changed.
//!
//! Reading (`read_*`) and choosing rows ([`classify`]) are separate so the selection rules can be tested
//! with fixtures.

use std::ptr::null_mut;
use std::time::Instant;

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_ALLCLASSES, DIGCF_PRESENT, HDEVINFO, SP_DEVINFO_DATA, SP_INF_SIGNER_INFO_V2_W, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInfo, SetupDiGetClassDevsW, SetupDiGetDevicePropertyW,
};
use windows_sys::Win32::Devices::Properties::{
    DEVPKEY_Device_Class, DEVPKEY_Device_CompatibleIds, DEVPKEY_Device_DriverDate, DEVPKEY_Device_DriverInfPath,
    DEVPKEY_Device_DriverProvider, DEVPKEY_Device_DriverVersion, DEVPKEY_Device_HardwareIds, DEVPKEY_Device_InstanceId,
    DEVPKEY_Device_Service, DEVPKEY_NAME,
};
use windows_sys::Win32::Foundation::DEVPROPKEY;

use super::{
    CheckError, DriverCategory, DriverRow, DriverStatus, URL_AMD_B650, URL_GIGABYTE_REV10_BIOS,
    URL_GIGABYTE_REV10_DRIVERS, URL_GIGABYTE_REV13_BIOS, URL_GIGABYTE_REV13_DRIVERS, URL_INTEL_ARC, URL_PAWNIO,
    URL_REALTEK_LAN, days_to_ymd,
};
use crate::error::ErrorCode;
use crate::util::win::{HKEY_LOCAL_MACHINE, from_wide, reg_get_string, reg_subkeys, wide};

/// One present device with an installed driver, as SetupAPI reports it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RawDevice {
    pub instance_id: String,
    /// Setup class name ("Net", "Bluetooth", "MEDIA", "Display", "System", ...).
    pub class: String,
    pub name: String,
    pub provider: String,
    pub version: String,
    /// `YYYY-MM-DD` (the INF's DriverVer date; Device Manager shows the same day).
    pub date: Option<String>,
    /// `oem59.inf` (in `%windir%\INF`).
    pub inf: String,
    pub service: String,
    pub hardware_ids: Vec<String>,
    pub compatible_ids: Vec<String>,
}

/// SMBIOS type 0/2 strings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BiosInfo {
    pub vendor: String,
    pub version: String,
    /// `YYYY-MM-DD`.
    pub date: Option<String>,
    pub board_vendor: String,
    pub board_product: String,
}

/// An installed program (uninstall entry), e.g. "AMD Chipset Software" 8.03.25.247.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub publisher: String,
    /// `YYYY-MM-DD` when the entry has an InstallDate.
    pub install_date: Option<String>,
}

/// Result of one inventory pass.
#[derive(Clone, Debug, Default)]
pub struct Inventory {
    pub rows: Vec<DriverRow>,
    pub error: Option<CheckError>,
    /// How long the pass took (SetupAPI + signature lookups).
    pub took_ms: u64,
}

/// Reads everything and builds the rows (with signers). Takes ~0.1-0.5 s; call it off the UI thread.
pub fn collect() -> Inventory {
    let t0 = Instant::now();
    let (devices, error) = match read_devices() {
        Ok(d) => (d, None),
        Err(e) => (Vec::new(), Some(CheckError::new(ErrorCode::DriverInventory, e))),
    };
    let mut rows = classify(&devices, read_bios().as_ref(), &read_packages());
    let mut signers: Vec<(String, Option<String>)> = Vec::new();
    for r in rows.iter_mut().filter(|r| !r.inf.is_empty()) {
        let key = r.inf.to_ascii_lowercase();
        let s = match signers.iter().find(|(k, _)| *k == key) {
            Some((_, s)) => s.clone(),
            None => {
                let s = signer_of(&r.inf);
                signers.push((key, s.clone()));
                s
            }
        };
        r.signer = s;
    }
    Inventory { rows, error, took_ms: t0.elapsed().as_millis() as u64 }
}

// ---------------------------------------------------------------------------------------
// Reading

/// Device information set, destroyed on drop.
struct DevInfoSet(HDEVINFO);

impl Drop for DevInfoSet {
    fn drop(&mut self) {
        // SAFETY: a valid set returned by SetupDiGetClassDevsW, destroyed once.
        unsafe {
            SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

/// Raw bytes and type of one device property (at most 64 KiB).
fn property(set: &DevInfoSet, data: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<Vec<u8>> {
    let mut ty = 0u32;
    let mut need = 0u32;
    // SAFETY: size query: null buffer with size 0; `ty`/`need` are valid out pointers; `data` belongs to `set`.
    unsafe { SetupDiGetDevicePropertyW(set.0, data, key, &mut ty, null_mut(), 0, &mut need, 0) };
    if need == 0 || need > 64 * 1024 {
        return None;
    }
    let mut buf = vec![0u8; need as usize];
    // SAFETY: `buf` is writable for `need` bytes, which is passed as its size.
    let ok = unsafe { SetupDiGetDevicePropertyW(set.0, data, key, &mut ty, buf.as_mut_ptr(), need, &mut need, 0) };
    (ok != 0).then(|| {
        buf.truncate(need as usize);
        buf
    })
}

fn utf16(bytes: &[u8]) -> Vec<u16> {
    bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()
}

fn prop_string(set: &DevInfoSet, data: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> String {
    property(set, data, key).map(|b| from_wide(&utf16(&b)).trim().to_string()).unwrap_or_default()
}

fn prop_list(set: &DevInfoSet, data: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Vec<String> {
    property(set, data, key)
        .map(|b| {
            utf16(&b).split(|&c| c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).take(64).collect()
        })
        .unwrap_or_default()
}

/// A FILETIME property as `YYYY-MM-DD` (UTC; driver dates are midnight UTC).
fn prop_date(set: &DevInfoSet, data: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<String> {
    let b = property(set, data, key)?;
    let ft = u64::from_le_bytes(b.get(..8)?.try_into().ok()?);
    filetime_to_ymd(ft)
}

/// FILETIME (100 ns since 1601-01-01) → `YYYY-MM-DD`.
pub fn filetime_to_ymd(ft: u64) -> Option<String> {
    const DAYS_1601_TO_1970: i64 = 134_774;
    if ft == 0 {
        return None;
    }
    let days = (ft / 10_000_000 / 86_400) as i64 - DAYS_1601_TO_1970;
    Some(days_to_ymd(days))
}

/// All present devices that have a driver (a version) installed.
pub fn read_devices() -> Result<Vec<RawDevice>, String> {
    let flags = DIGCF_ALLCLASSES | DIGCF_PRESENT;
    // SAFETY: no class GUID, enumerator or window; the returned set is owned by DevInfoSet.
    let h = unsafe { SetupDiGetClassDevsW(std::ptr::null(), std::ptr::null(), null_mut(), flags) };
    if h == -1 || h == 0 {
        return Err(format!("SetupDiGetClassDevs failed: {}", std::io::Error::last_os_error()));
    }
    let set = DevInfoSet(h);
    let mut out = Vec::new();
    for i in 0..4096u32 {
        let mut data = SP_DEVINFO_DATA { cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
        // SAFETY: `data` is a valid SP_DEVINFO_DATA with cbSize set; the loop ends when the index runs out.
        if unsafe { SetupDiEnumDeviceInfo(set.0, i, &mut data) } == 0 {
            break;
        }
        let version = prop_string(&set, &data, &DEVPKEY_Device_DriverVersion);
        if version.is_empty() {
            continue;
        }
        out.push(RawDevice {
            instance_id: prop_string(&set, &data, &DEVPKEY_Device_InstanceId),
            class: prop_string(&set, &data, &DEVPKEY_Device_Class),
            name: prop_string(&set, &data, &DEVPKEY_NAME),
            provider: prop_string(&set, &data, &DEVPKEY_Device_DriverProvider),
            version,
            date: prop_date(&set, &data, &DEVPKEY_Device_DriverDate),
            inf: prop_string(&set, &data, &DEVPKEY_Device_DriverInfPath),
            service: prop_string(&set, &data, &DEVPKEY_Device_Service),
            hardware_ids: prop_list(&set, &data, &DEVPKEY_Device_HardwareIds),
            compatible_ids: prop_list(&set, &data, &DEVPKEY_Device_CompatibleIds),
        });
    }
    Ok(out)
}

/// `MM/DD/YYYY` (SMBIOS) → `YYYY-MM-DD`.
pub fn smbios_date(s: &str) -> Option<String> {
    let mut it = s.trim().split('/');
    let (m, d, y) = (it.next()?.parse::<u32>().ok()?, it.next()?.parse::<u32>().ok()?, it.next()?.parse::<u32>().ok()?);
    ((1..=12).contains(&m) && (1..=31).contains(&d) && y >= 1990 && it.next().is_none())
        .then(|| format!("{y:04}-{m:02}-{d:02}"))
}

pub fn read_bios() -> Option<BiosInfo> {
    let k = r"HARDWARE\DESCRIPTION\System\BIOS";
    let get = |v: &str| reg_get_string(HKEY_LOCAL_MACHINE, k, v).unwrap_or_default();
    let version = get("BIOSVersion");
    if version.is_empty() {
        return None;
    }
    Some(BiosInfo {
        vendor: get("BIOSVendor"),
        version,
        date: smbios_date(&get("BIOSReleaseDate")),
        board_vendor: get("BaseBoardManufacturer"),
        board_product: get("BaseBoardProduct"),
    })
}

/// `20260702` → `2026-07-02`.
fn install_date(s: &str) -> Option<String> {
    let s = s.trim();
    (s.len() == 8 && s.bytes().all(|b| b.is_ascii_digit())).then(|| format!("{}-{}-{}", &s[..4], &s[4..6], &s[6..]))
}

/// AMD uninstall entries (64- and 32-bit views): the chipset package and its components.
pub fn read_packages() -> Vec<Package> {
    let mut out = Vec::new();
    for root in [
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
        r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
    ] {
        for sub in reg_subkeys(HKEY_LOCAL_MACHINE, root, 2000) {
            let k = format!(r"{root}\{sub}");
            let publisher = reg_get_string(HKEY_LOCAL_MACHINE, &k, "Publisher").unwrap_or_default();
            if !publisher.to_ascii_lowercase().contains("advanced micro devices") {
                continue;
            }
            let (Some(name), Some(version)) = (
                reg_get_string(HKEY_LOCAL_MACHINE, &k, "DisplayName"),
                reg_get_string(HKEY_LOCAL_MACHINE, &k, "DisplayVersion"),
            ) else {
                continue;
            };
            let install_date = reg_get_string(HKEY_LOCAL_MACHINE, &k, "InstallDate").and_then(|d| install_date(&d));
            out.push(Package { name, version, publisher, install_date });
        }
    }
    out
}

// windows-sys gates this one behind the large `Win32_System_Diagnostics_Debug` feature (for the
// alternate-platform argument, which is always null here).
#[link(name = "setupapi.dll", kind = "raw-dylib", modifiers = "+verbatim")]
unsafe extern "system" {
    fn SetupVerifyInfFileW(
        inf_name: *const u16,
        alt_platform_info: *const std::ffi::c_void,
        inf_signer_info: *mut SP_INF_SIGNER_INFO_V2_W,
    ) -> i32;
}

/// Who signed the driver package (`Microsoft Windows Hardware Compatibility Publisher` for WHQL).
/// Verifies the INF against its catalog, like Device Manager's "Digital Signer".
pub fn signer_of(inf: &str) -> Option<String> {
    // Only a bare INF name from the device's properties; never a path we did not build.
    if inf.is_empty() || inf.contains(['\\', '/', ':']) || inf.contains("..") {
        return None;
    }
    let windows = crate::util::win::system32().parent()?.to_path_buf();
    let path = wide(&windows.join("INF").join(inf).to_string_lossy());
    let size = std::mem::size_of::<SP_INF_SIGNER_INFO_V2_W>() as u32;
    let mut info = SP_INF_SIGNER_INFO_V2_W { cbSize: size, ..Default::default() };
    // SAFETY: `path` is NUL-terminated; `info` is a valid V2 structure with cbSize set.
    let ok = unsafe { SetupVerifyInfFileW(path.as_ptr(), std::ptr::null(), &mut info) };
    let s = from_wide(&info.DigitalSigner);
    (ok != 0 && !s.is_empty()).then_some(s)
}

// ---------------------------------------------------------------------------------------
// Choosing rows

fn lc(s: &str) -> String {
    s.to_ascii_lowercase()
}

/// `PCI\VEN_10EC&DEV_C852...` → `PCI`.
fn enumerator(instance: &str) -> String {
    instance.split('\\').next().unwrap_or("").to_ascii_uppercase()
}

/// The 4-hex-digit value after `tag` (`VEN_`, `DEV_`) in `id`.
fn id_field(id: &str, tag: &str) -> Option<String> {
    let u = id.to_ascii_uppercase();
    let i = u.find(tag)? + tag.len();
    u.get(i..i + 4).filter(|v| v.bytes().all(|b| b.is_ascii_hexdigit())).map(str::to_string)
}

fn is_wireless(d: &RawDevice) -> bool {
    let n = lc(&d.name);
    ["wi-fi", "wifi", "wireless", "802.11", "wlan"].iter().any(|k| n.contains(k))
}

fn is_microsoft(provider: &str) -> bool {
    lc(provider).starts_with("microsoft")
}

fn is_amd(provider: &str) -> bool {
    lc(provider).contains("advanced micro devices")
}

/// The category of a device, or `None` when it is not one of the rows we show.
pub fn category_of(d: &RawDevice) -> Option<DriverCategory> {
    let class = lc(&d.class);
    let en = enumerator(&d.instance_id);
    let ids = || d.hardware_ids.iter().chain(&d.compatible_ids);
    if ids().any(|i| i.eq_ignore_ascii_case(r"Root\PawnIO")) || d.service.eq_ignore_ascii_case("PawnIO") {
        return Some(DriverCategory::SuperIo);
    }
    match class.as_str() {
        "net" if en == "PCI" || en == "USB" => {
            Some(if is_wireless(d) { DriverCategory::Wifi } else { DriverCategory::Lan })
        }
        // The radio itself (USB/PCI), not the BTHENUM/BTHLE profile devices of paired peripherals.
        "bluetooth" if en == "USB" || en == "PCI" => Some(DriverCategory::Bluetooth),
        // The onboard codec; GPU HDMI/DP audio (Intel, AMD, NVIDIA) belongs to the graphics driver.
        "media" if en == "HDAUDIO" => {
            let ven = id_field(&d.instance_id, "VEN_");
            let display =
                matches!(ven.as_deref(), Some("8086" | "1002" | "10DE")) || lc(&d.name).contains("display audio");
            (!display).then_some(DriverCategory::Audio)
        }
        "display" if en == "PCI" && !is_microsoft(&d.provider) => Some(DriverCategory::Gpu),
        "system" | "securitydevices" if is_amd(&d.provider) => Some(DriverCategory::Chipset),
        _ => None,
    }
}

/// Builds the rows, in category order, merging identical devices (two "AMD I2C Controller"s are one
/// row with `count` 2). Statuses start as `Unknown`; [`super::apply_offers`] fills them in.
pub fn classify(devices: &[RawDevice], bios: Option<&BiosInfo>, packages: &[Package]) -> Vec<DriverRow> {
    let mut rows: Vec<DriverRow> = Vec::new();
    for d in devices {
        let Some(category) = category_of(d) else { continue };
        if let Some(r) = rows.iter_mut().find(|r| {
            r.category == category
                && r.device == d.name
                && r.installed == d.version
                && r.inf.eq_ignore_ascii_case(&d.inf)
        }) {
            r.count += 1;
            for id in d.hardware_ids.iter().chain(&d.compatible_ids) {
                if !r.hardware_ids.iter().any(|x| x.eq_ignore_ascii_case(id)) {
                    r.hardware_ids.push(id.to_ascii_uppercase());
                }
            }
            continue;
        }
        rows.push(DriverRow {
            category,
            device: d.name.clone(),
            provider: d.provider.clone(),
            installed: d.version.clone(),
            installed_date: d.date.clone(),
            inf: d.inf.clone(),
            signer: None,
            instance_id: d.instance_id.clone(),
            count: 1,
            hardware_ids: d.hardware_ids.iter().chain(&d.compatible_ids).map(|s| s.to_ascii_uppercase()).collect(),
            latest: None,
            status: DriverStatus::Unknown,
            vendor_url: None,
            ..DriverRow::empty()
        });
    }
    // The AMD chipset installer's own version (what AMD's download page lists), above its components.
    if let Some(p) = packages.iter().find(|p| p.name.eq_ignore_ascii_case("AMD Chipset Software")) {
        let date = p.install_date.clone().or_else(|| {
            packages
                .iter()
                .find(|q| q.version == p.version && q.name.eq_ignore_ascii_case("AMD_Chipset_Drivers"))
                .and_then(|q| q.install_date.clone())
        });
        rows.push(DriverRow {
            category: DriverCategory::Chipset,
            device: "AMD Chipset Software (package)".into(),
            provider: p.publisher.clone(),
            installed: p.version.clone(),
            installed_date: date,
            count: 1,
            package: true,
            package_name: Some(p.name.clone()),
            ..DriverRow::empty()
        });
    }
    if let Some(b) = bios {
        let vendor =
            if lc(&b.board_vendor).contains("gigabyte") { "GIGABYTE".to_string() } else { b.board_vendor.clone() };
        rows.push(DriverRow {
            category: DriverCategory::Bios,
            device: format!("{vendor} {}", b.board_product).trim().to_string(),
            provider: b.vendor.clone(),
            installed: b.version.clone(),
            installed_date: b.date.clone(),
            count: 1,
            ..DriverRow::empty()
        });
    }
    // Stable order: category, then the package row first, then by name.
    rows.sort_by(|a, b| a.category.cmp(&b.category).then(b.package.cmp(&a.package)).then(a.device.cmp(&b.device)));
    let board = BoardPages::for_board(bios, &rows);
    for r in &mut rows {
        r.vendor_url = vendor_url(r, &board);
        if r.category == DriverCategory::Bios
            && let Some(b) = bios
        {
            r.board = Some(super::BoardInfo {
                vendor: b.board_vendor.clone(),
                product: b.board_product.clone(),
                revisions: board.revisions(),
            });
        }
    }
    rows
}

/// Which pinned pages apply to this board.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct BoardPages {
    /// GIGABYTE B650M D3HP AX: `Some(true)` for rev 1.3 (Realtek Wi-Fi), `Some(false)` for rev 1.0/1.2
    /// (AMD RZ616 / MediaTek Wi-Fi); `None` for any other board.
    d3hp_rev13: Option<bool>,
    b650: bool,
}

impl BoardPages {
    fn for_board(bios: Option<&BiosInfo>, rows: &[DriverRow]) -> Self {
        let product = bios.map(|b| b.board_product.to_ascii_uppercase()).unwrap_or_default();
        let d3hp = product.replace(['-', ' '], "") == "B650MD3HPAX";
        let realtek_wifi =
            rows.iter().any(|r| r.category == DriverCategory::Wifi && lc(&r.provider).contains("realtek"));
        BoardPages { d3hp_rev13: d3hp.then_some(realtek_wifi), b650: product.contains("B650") }
    }

    /// The board revisions this can be (catalog matching): GIGABYTE's SMBIOS has no revision, so it
    /// comes from the Wi-Fi module like the support page. Empty for other boards.
    fn revisions(&self) -> Vec<String> {
        match self.d3hp_rev13 {
            Some(true) => vec!["1.3".into()],
            Some(false) => vec!["1.0".into(), "1.2".into()],
            None => Vec::new(),
        }
    }
}

fn vendor_url(r: &DriverRow, b: &BoardPages) -> Option<&'static str> {
    let gigabyte =
        b.d3hp_rev13.map(|rev13| if rev13 { URL_GIGABYTE_REV13_DRIVERS } else { URL_GIGABYTE_REV10_DRIVERS });
    match r.category {
        DriverCategory::Wifi | DriverCategory::Bluetooth | DriverCategory::Audio => gigabyte,
        DriverCategory::Lan if lc(&r.provider).contains("realtek") => Some(URL_REALTEK_LAN),
        DriverCategory::Lan => gigabyte,
        DriverCategory::Gpu if id_field(&r.instance_id, "VEN_").as_deref() == Some("8086") => Some(URL_INTEL_ARC),
        DriverCategory::Gpu => None,
        DriverCategory::Chipset if b.b650 => Some(URL_AMD_B650),
        DriverCategory::Chipset => gigabyte,
        DriverCategory::SuperIo => Some(URL_PAWNIO),
        DriverCategory::Bios => {
            b.d3hp_rev13.map(|rev13| if rev13 { URL_GIGABYTE_REV13_BIOS } else { URL_GIGABYTE_REV10_BIOS })
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn dev(instance: &str, class: &str, name: &str, provider: &str, version: &str, inf: &str, hwid: &str) -> RawDevice {
        RawDevice {
            instance_id: instance.into(),
            class: class.into(),
            name: name.into(),
            provider: provider.into(),
            version: version.into(),
            date: Some("2025-10-16".into()),
            inf: inf.into(),
            service: String::new(),
            hardware_ids: vec![hwid.into()],
            compatible_ids: Vec::new(),
        }
    }

    /// A reference PC (B650M D3HP AX rev 1.3, 7500F, Arc B580), from SetupAPI on 2026-10-19, plus the
    /// devices that must *not* become rows (the VPN adapter and the mouse are made-up stand-ins).
    #[rustfmt::skip]
    pub(crate) fn this_pc() -> (Vec<RawDevice>, BiosInfo, Vec<Package>) {
        let amd = "Advanced Micro Devices, Inc.";
        let devices = vec![
            dev(r"PCI\VEN_10EC&DEV_C852&SUBSYS_C85210EC&REV_00\4&1", "Net", "Realtek 8852CE WiFi 6E PCI-E NIC", "Realtek Semiconductor Corp.", "6001.16.172.0", "oem59.inf", r"PCI\VEN_10EC&DEV_C852&SUBSYS_C85210EC&REV_00"),
            dev(r"PCI\VEN_10EC&DEV_8125&SUBSYS_E0001458&REV_05\4&2", "Net", "Realtek PCIe 2.5GbE Family Controller", "Realtek", "1125.29.50.202", "oem79.inf", r"PCI\VEN_10EC&DEV_8125&SUBSYS_E0001458&REV_05"),
            dev(r"SWD\MSRRAS\MS_NDISWANIP", "Net", "WAN Miniport (IP)", "Microsoft", "10.0.26100.1", "netrasa.inf", "ms_ndiswanip"),
            dev(r"ROOT\NET\0000", "Net", "Example VPN Tunnel", "Example VPN Vendor", "1.2.0.0", "oem28.inf", "ExampleTun"),
            dev(r"USB\VID_0BDA&PID_0852\00E04C000001", "Bluetooth", "Realtek Bluetooth Adapter", "Realtek Semiconductor Corp.", "18.4032.2510.900", "oem60.inf", r"USB\VID_0BDA&PID_0852&REV_0000"),
            dev(r"BTHENUM\{0000110C-0000-1000-8000-00805F9B34FB}_LOCALMFG&005D\7&1", "Bluetooth", "Microsoft Bluetooth Avrcp Transport Driver", "Microsoft", "10.0.26100.9444", "microsoft_bluetooth_avrcptransport.inf", r"BTHENUM\{0000110c-0000-1000-8000-00805f9b34fb}"),
            dev(r"HDAUDIO\FUNC_01&VEN_10EC&DEV_0897&SUBSYS_1458A194&REV_1003\5&1", "MEDIA", "Realtek High Definition Audio", "Realtek Semiconductor Corp.", "6.0.9927.1", "oem64.inf", r"HDAUDIO\FUNC_01&VEN_10EC&DEV_0897&SUBSYS_1458A194&REV_1003"),
            dev(r"HDAUDIO\FUNC_01&VEN_8086&DEV_281E&SUBSYS_80860101&REV_1000\5&2", "MEDIA", "HD Audio Driver for Display Audio", "Intel Corporation", "32.0.101.9033", "oem83.inf", r"HDAUDIO\FUNC_01&VEN_8086&DEV_281E&SUBSYS_80860101&REV_1000"),
            dev(r"RTK_BT\{5DBB7054}\1", "MEDIA", "Realtek Bluetooth LE Audio Driver", "Realtek Semiconductor Corp.", "2.0.11.0", "oem50.inf", r"RTK_BT\{5DBB7054}"),
            dev(r"PCI\VEN_8086&DEV_E20B&SUBSYS_4215172F&REV_00\6&1", "Display", "Intel(R) Arc(TM) B580 Graphics", "Intel Corporation", "32.0.101.9033", "oem41.inf", r"PCI\VEN_8086&DEV_E20B&SUBSYS_4215172F&REV_00"),
            dev(r"PCI\VEN_1022&DEV_1649&SUBSYS_16491022&REV_00\4&3", "SecurityDevices", "AMD PSP 11.0 Device", "Advanced Micro Devices Inc.", "5.43.0.0", "oem15.inf", r"PCI\VEN_1022&DEV_1649&SUBSYS_16491022&REV_00"),
            dev(r"PCI\VEN_1022&DEV_790B&SUBSYS_50011458&REV_71\3&1", "System", "AMD SMBus", "Advanced Micro Devices, Inc", "5.12.0.44", "oem6.inf", r"PCI\VEN_1022&DEV_790B&SUBSYS_50011458&REV_71"),
            dev(r"ACPI\AMDI0010\0", "System", "AMD I2C Controller", amd, "1.2.0.126", "oem4.inf", r"ACPI\VEN_AMDI&DEV_0010"),
            dev(r"ACPI\AMDI0010\1", "System", "AMD I2C Controller", amd, "1.2.0.126", "oem4.inf", r"ACPI\AMDI0010"),
            dev(r"ACPI\AMDI0030\0", "System", "AMD GPIO Controller", "Advanced Micro Devices, Inc", "2.2.0.136", "oem13.inf", r"ACPI\VEN_AMDI&DEV_0030"),
            dev(r"ACPI\AMDI0052\0", "System", "AMD Provisioning Packages", amd, "8.0.0.62", "oem78.inf", r"ACPI\VEN_AMDI&DEV_0052"),
            dev(r"PCI\VEN_1022&DEV_14DE&SUBSYS_D0001458&REV_00\3&2", "System", "AMD PCI", "Advanced Micro Devices", "1.0.0.90", "oem5.inf", r"PCI\VEN_1022&DEV_14DE&SUBSYS_D0001458&REV_00"),
            dev(r"ACPI\AUTHENTICAMD_-_AMD64_FAMILY_25_MODEL_97_-_AMD_RYZEN_5_7500F_6-CORE_PROCESSOR\_0", "Processor", "AMD Ryzen 5 7500F 6-Core Processor", "Microsoft", "10.0.26100.9278", "cpu.inf", r"ACPI\AuthenticAMD"),
            dev(r"PCI\VEN_8086&DEV_E2F0&SUBSYS_00008086&REV_00\5&1", "System", "PCI Express Downstream Switch Port", "Intel", "1.1.1.1", "oem11.inf", r"PCI\VEN_8086&DEV_E2F0"),
            dev(r"ROOT\SOFTWAREDEVICE\0000", "SoftwareDevice", "PawnIO", "namazso", "2.2.0.0", "oem43.inf", r"Root\PawnIO"),
            dev(r"USB\VID_FFFF&PID_0001&MI_01\7&1", "HIDClass", "Example USB Mouse", "Example Peripherals", "10.0.15063.0", "oem74.inf", r"USB\VID_FFFF&PID_0001&REV_0200&MI_01"),
        ];
        let bios = BiosInfo {
            vendor: "American Megatrends International, LLC.".into(),
            version: "F36d".into(),
            date: Some("2025-09-18".into()),
            board_vendor: "Gigabyte Technology Co., Ltd.".into(),
            board_product: "B650M D3HP AX".into(),
        };
        let pkg = |n: &str, v: &str, d: Option<&str>| Package {
            name: n.into(),
            version: v.into(),
            publisher: amd.into(),
            install_date: d.map(str::to_string),
        };
        let packages = vec![
            pkg("AMD Chipset Software", "8.03.25.247", None),
            pkg("AMD_Chipset_Drivers", "8.03.25.247", Some("2026-07-02")),
            pkg("AMD PSP Driver", "5.40.0.0", Some("2026-07-02")),
        ];
        (devices, bios, packages)
    }

    #[test]
    fn classifies_this_pc() {
        let (devices, bios, packages) = this_pc();
        let rows = classify(&devices, Some(&bios), &packages);
        let pick = |c: DriverCategory| rows.iter().filter(|r| r.category == c).collect::<Vec<_>>();
        let wifi = pick(DriverCategory::Wifi);
        assert_eq!(wifi.len(), 1);
        assert_eq!(wifi[0].device, "Realtek 8852CE WiFi 6E PCI-E NIC");
        assert_eq!(wifi[0].vendor_url, Some(URL_GIGABYTE_REV13_DRIVERS));
        let lan = pick(DriverCategory::Lan);
        assert_eq!(lan.len(), 1, "virtual adapters are not LAN rows: {lan:?}");
        assert_eq!(lan[0].installed, "1125.29.50.202");
        assert_eq!(lan[0].vendor_url, Some(URL_REALTEK_LAN));
        let bt = pick(DriverCategory::Bluetooth);
        assert_eq!(bt.len(), 1);
        assert_eq!(bt[0].device, "Realtek Bluetooth Adapter");
        let audio = pick(DriverCategory::Audio);
        assert_eq!(audio.len(), 1, "HDMI audio belongs to the GPU: {audio:?}");
        assert_eq!(audio[0].installed, "6.0.9927.1");
        let gpu = pick(DriverCategory::Gpu);
        assert_eq!(gpu.len(), 1);
        assert_eq!(gpu[0].vendor_url, Some(URL_INTEL_ARC));
        let chipset = pick(DriverCategory::Chipset);
        assert_eq!(chipset[0].device, "AMD Chipset Software (package)", "package row first");
        assert_eq!(chipset[0].installed, "8.03.25.247");
        assert_eq!(chipset[0].installed_date.as_deref(), Some("2026-07-02"));
        assert!(chipset.iter().all(|r| r.vendor_url == Some(URL_AMD_B650)));
        let i2c: Vec<_> = chipset.iter().filter(|r| r.device == "AMD I2C Controller").collect();
        assert_eq!(i2c.len(), 1);
        assert_eq!(i2c[0].count, 2);
        assert!(i2c[0].hardware_ids.contains(&r"ACPI\AMDI0010".to_string()));
        assert_eq!(chipset.len(), 7, "package + PSP, SMBus, I2C, GPIO, provisioning, PCI: {chipset:?}");
        assert!(!rows.iter().any(|r| r.device.contains("Ryzen") || r.device.contains("Switch Port")));
        let sio = pick(DriverCategory::SuperIo);
        assert_eq!(sio.len(), 1);
        assert_eq!(sio[0].vendor_url, Some(URL_PAWNIO));
        let b = pick(DriverCategory::Bios);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].device, "GIGABYTE B650M D3HP AX");
        assert_eq!(b[0].installed, "F36d");
        assert_eq!(b[0].installed_date.as_deref(), Some("2025-09-18"));
        assert_eq!(b[0].vendor_url, Some(URL_GIGABYTE_REV13_BIOS));
        assert_eq!(rows.last().unwrap().category, DriverCategory::Bios);
        assert!(rows.iter().all(|r| r.status == DriverStatus::Unknown && r.latest.is_none()));
        // Every pinned page can actually be opened.
        for r in &rows {
            if let Some(u) = r.vendor_url {
                assert!(crate::util::win::ALLOWED_URLS.contains(&u), "{u} not allow-listed");
            }
        }
    }

    #[test]
    fn other_boards_get_no_gigabyte_pages() {
        let (devices, mut bios, packages) = this_pc();
        bios.board_product = "X670E AORUS MASTER".into();
        let rows = classify(&devices, Some(&bios), &packages);
        let bios_row = rows.iter().find(|r| r.category == DriverCategory::Bios).unwrap();
        assert_eq!(bios_row.vendor_url, None);
        assert_eq!(rows.iter().find(|r| r.category == DriverCategory::Wifi).unwrap().vendor_url, None);
        // A rev 1.0/1.2 board has the MediaTek (AMD RZ616) module.
        let (mut devices, bios, packages) = this_pc();
        devices[0].name = "MediaTek Wi-Fi 6E MT7922 (RZ616) 160MHz PCIe Adapter".into();
        devices[0].provider = "MediaTek, Inc.".into();
        let rows = classify(&devices, Some(&bios), &packages);
        assert_eq!(rows[0].vendor_url, Some(URL_GIGABYTE_REV10_DRIVERS));
        assert_eq!(rows.last().unwrap().vendor_url, Some(URL_GIGABYTE_REV10_BIOS));
    }

    #[test]
    fn date_parsing() {
        assert_eq!(smbios_date("09/18/2025").as_deref(), Some("2025-09-18"));
        assert_eq!(smbios_date("13/18/2025"), None);
        assert_eq!(smbios_date("garbage"), None);
        assert_eq!(install_date("20260702").as_deref(), Some("2026-07-02"));
        assert_eq!(install_date("2026-07"), None);
        // 2025-10-16 (day 20 377 after 1970-01-01) as FILETIME, at midnight and late in the day.
        let ft = (20_377 + 134_774) * 86_400 * 10_000_000u64;
        assert_eq!(filetime_to_ymd(ft).as_deref(), Some("2025-10-16"));
        assert_eq!(filetime_to_ymd(ft + 86_399 * 10_000_000).as_deref(), Some("2025-10-16"));
        assert_eq!(filetime_to_ymd(0), None);
    }

    #[test]
    fn signer_rejects_paths() {
        assert_eq!(signer_of(r"..\x.inf"), None);
        assert_eq!(signer_of(r"C:\Windows\INF\oem1.inf"), None);
        assert_eq!(signer_of(""), None);
    }

    /// Reads the real machine (read-only). Every Windows PC has a BIOS and some driver.
    #[test]
    fn live_inventory_reads_something() {
        let inv = collect();
        assert!(inv.error.is_none(), "{:?}", inv.error);
        assert!(inv.rows.iter().any(|r| r.category == DriverCategory::Bios));
        assert!(!read_devices().unwrap().is_empty());
    }
}
