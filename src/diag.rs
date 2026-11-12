//! System diagnostics: RGB controller USB health, conflicting software, and the sections of the
//! diagnostics report.

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GETIDLIST_FILTER_ENUMERATOR, CM_GETIDLIST_FILTER_PRESENT, CM_Get_DevNode_Status, CM_Get_Device_ID_List_SizeW,
    CM_Get_Device_ID_ListW, CM_LOCATE_DEVNODE_NORMAL, CM_Locate_DevNodeW, CR_SUCCESS, DN_HAS_PROBLEM,
};
use windows_sys::Win32::System::Registry::HKEY;

use crate::config::{Config, ZoneConfig};
use crate::hw::HwInfo;
use crate::model::SensorDesc;
use crate::util::win::{HKEY_LOCAL_MACHINE, reg_get_dword, reg_get_string, reg_string_values, reg_subkeys, wide};

/// Known RGB Fusion 2 USB controllers (ITE).
pub const FUSION2_PIDS: [u16; 4] = [0x5702, 0x8297, 0x5711, 0x8950];

fn device_ids(enumerator: &str, present_only: bool) -> Vec<String> {
    let filter = wide(enumerator);
    let flags = CM_GETIDLIST_FILTER_ENUMERATOR | if present_only { CM_GETIDLIST_FILTER_PRESENT } else { 0 };
    let mut len = 0u32;
    unsafe {
        if CM_Get_Device_ID_List_SizeW(&mut len, filter.as_ptr(), flags) != CR_SUCCESS || len == 0 {
            return Vec::new();
        }
        let mut buf = vec![0u16; len as usize];
        if CM_Get_Device_ID_ListW(filter.as_ptr(), buf.as_mut_ptr(), len, flags) != CR_SUCCESS {
            return Vec::new();
        }
        buf.split(|&c| c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
    }
}

fn problem_code(instance: &str) -> Option<u32> {
    let id = wide(instance);
    let mut devinst = 0u32;
    unsafe {
        if CM_Locate_DevNodeW(&mut devinst, id.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) != CR_SUCCESS {
            return None;
        }
        let mut status = 0u32;
        let mut problem = 0u32;
        if CM_Get_DevNode_Status(&mut status, &mut problem, devinst, 0) != CR_SUCCESS {
            return None;
        }
        if status & DN_HAS_PROBLEM != 0 { Some(problem) } else { None }
    }
}

/// The port part of a USB instance id: "USB\VID_048D&PID_5702\8&F2CB6FA&0&16" -> "8&F2CB6FA&0&16".
fn port_suffix(instance: &str) -> &str {
    instance.rsplit('\\').next().unwrap_or("")
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RgbUsbHealth {
    pub controller_present: bool,
    pub controller_pid: Option<u16>,
    /// A controller was seen on this PC before (device history).
    pub previously_seen: bool,
    /// A USB device on the controller's former port fails to enumerate (code 43 etc.).
    pub failed_on_port: Option<(String, u32)>,
}

impl RgbUsbHealth {
    /// Windows' problem code for the controller (43 = descriptor request failed, ...), if the
    /// controller or a device on its former port failed to start.
    pub fn problem_code(&self) -> Option<u32> {
        if self.controller_present { None } else { self.failed_on_port.as_ref().map(|(_, c)| *c) }
    }

    pub fn summary(&self) -> Option<String> {
        if self.controller_present {
            return None;
        }
        if let Some((_, code)) = &self.failed_on_port {
            return Some(format!(
                "The RGB controller (ITE) is stuck: Windows reports USB error {code} on its port. \
                 Shut down, switch the PSU off (or unplug it), hold the power button for 15 s, \
                 wait 30 s and boot again. This fully resets the controller."
            ));
        }
        if self.previously_seen {
            return Some(
                "The RGB controller was detected on this PC before but is missing now. \
                 A full power drain (PSU off for 30 s) usually brings it back."
                    .into(),
            );
        }
        None
    }
}

pub fn rgb_usb_health() -> RgbUsbHealth {
    let present = device_ids("USB", true);
    let all = device_ids("USB", false);
    let is_ctrl = |id: &str| {
        let u = id.to_ascii_uppercase();
        u.contains("VID_048D") && FUSION2_PIDS.iter().any(|p| u.contains(&format!("PID_{p:04X}")))
    };
    let mut h = RgbUsbHealth::default();
    if let Some(id) = present.iter().find(|i| is_ctrl(i) && !i.to_ascii_uppercase().contains("&MI_")) {
        let problem = problem_code(id);
        h.controller_present = problem.is_none();
        // The controller enumerated but Windows stopped it (code 43 / 10 / ...).
        if let Some(code) = problem {
            h.failed_on_port = Some((id.clone(), code));
        }
        let u = id.to_ascii_uppercase();
        h.controller_pid = FUSION2_PIDS.iter().copied().find(|p| u.contains(&format!("PID_{p:04X}")));
    }
    let history: Vec<&String> = all.iter().filter(|i| is_ctrl(i)).collect();
    h.previously_seen = !history.is_empty();
    if !h.controller_present && h.failed_on_port.is_none() {
        for failed in present.iter().filter(|i| i.to_ascii_uppercase().starts_with("USB\\VID_0000&PID_000")) {
            if let Some(code) = problem_code(failed) {
                let suffix = port_suffix(failed);
                if history.iter().any(|old| port_suffix(old) == suffix) {
                    h.failed_on_port = Some((failed.clone(), code));
                    break;
                }
            }
        }
    }
    h
}

/// (process name, what it is, why it conflicts)
const CONFLICTS: &[(&str, &str, &str)] = &[
    ("gcc.exe", "GIGABYTE Control Center", "fights over fans and the RGB controller"),
    ("gigabytecontrolcenter.exe", "GIGABYTE Control Center", "fights over fans and the RGB controller"),
    ("rgbfusion.exe", "GIGABYTE RGB Fusion", "uses the same RGB controller"),
    ("rgbfusion2.exe", "GIGABYTE RGB Fusion", "uses the same RGB controller"),
    ("easytune.exe", "GIGABYTE EasyTune", "overrides fan settings"),
    ("easytuneengineservice.exe", "GIGABYTE EasyTune service", "overrides fan settings"),
    ("siv.exe", "GIGABYTE System Information Viewer", "overrides fan settings"),
    ("fancontrol.exe", "FanControl", "two fan controllers will fight"),
    ("openrgb.exe", "OpenRGB", "uses the same RGB controller"),
    ("signalrgb.exe", "SignalRGB", "uses the same RGB controller"),
    ("icue.exe", "Corsair iCUE", "may take over motherboard RGB"),
    ("armourycrate.service.exe", "ASUS Armoury Crate", "may take over motherboard RGB"),
    ("lightingservice.exe", "ASUS Aura", "may take over motherboard RGB"),
];

pub fn conflicting_software() -> Vec<(String, String)> {
    let running = crate::util::win::running_processes();
    let mut out: Vec<(String, String)> = Vec::new();
    for (exe, name, why) in CONFLICTS {
        if running.iter().any(|p| p == exe) && !out.iter().any(|(n, _)| n == name) {
            out.push((name.to_string(), why.to_string()));
        }
    }
    out
}

// ---------------------------------------------------------------------------------------
// Report sections shared by `--diag` (main.rs) and the in-app report (engine), so both files carry
// the same information in the same layout.

/// Aligns `rows` into columns sized to their content (columns in `right` are right-aligned); every
/// line starts with `indent`. A one-cell row is a heading and does not widen the first column.
pub fn table_aligned(rows: &[Vec<String>], indent: &str, right: &[usize]) -> String {
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|c| {
            rows.iter().filter(|r| r.len() > 1).filter_map(|r| r.get(c)).map(|s| s.chars().count()).max().unwrap_or(0)
        })
        .collect();
    let mut s = String::new();
    for r in rows {
        let mut line = String::from(indent);
        for (c, cell) in r.iter().enumerate() {
            let pad = widths[c].saturating_sub(cell.chars().count());
            if right.contains(&c) {
                line.push_str(&" ".repeat(pad));
                line.push_str(cell);
            } else {
                line.push_str(cell);
                line.push_str(&" ".repeat(pad));
            }
            line.push_str("  ");
        }
        s.push_str(line.trim_end());
        s.push('\n');
    }
    s
}

pub fn table(rows: &[Vec<String>], indent: &str) -> String {
    table_aligned(rows, indent, &[])
}

fn fmt_systemtime(st: &windows_sys::Win32::Foundation::SYSTEMTIME, date: bool) -> String {
    let time = format!("{:02}:{:02}:{:02}", st.wHour, st.wMinute, st.wSecond);
    if date { format!("{:04}-{:02}-{:02} {time}", st.wYear, st.wMonth, st.wDay) } else { time }
}

/// "2026-10-19 13:01:06 local (11:01:06 UTC)".
pub fn timestamp() -> String {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::{GetLocalTime, GetSystemTime};
    // SAFETY: SYSTEMTIME is plain data (all-zero is valid); both calls only write the given struct.
    let (local, utc) = unsafe {
        let mut l: SYSTEMTIME = std::mem::zeroed();
        let mut u: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut l);
        GetSystemTime(&mut u);
        (l, u)
    };
    format!("{} local ({} UTC)", fmt_systemtime(&local, true), fmt_systemtime(&utc, false))
}

/// "Windows 11 Pro 25H2 (build 26300.6725)".
pub fn windows_version() -> String {
    let k = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let build = reg_get_string(HKEY_LOCAL_MACHINE, k, "CurrentBuild");
    let mut name = reg_get_string(HKEY_LOCAL_MACHINE, k, "ProductName").unwrap_or_else(|| "Windows".into());
    // ProductName still says "Windows 10" on Windows 11.
    if build.as_deref().and_then(|b| b.parse::<u32>().ok()).is_some_and(|b| b >= 22000) {
        name = name.replace("Windows 10", "Windows 11");
    }
    let display = reg_get_string(HKEY_LOCAL_MACHINE, k, "DisplayVersion").map(|d| format!(" {d}")).unwrap_or_default();
    let ubr = reg_get_dword(HKEY_LOCAL_MACHINE, k, "UBR").map(|u| format!(".{u}")).unwrap_or_default();
    format!("{name}{display} (build {}{ubr})", build.unwrap_or_else(|| "?".into()))
}

/// Report title, writer, version and environment.
pub fn report_header(writer: &str, info: &HwInfo) -> String {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "?".into());
    let up = crate::util::win::uptime_ms() / 1000;
    let mut s = String::from("FanRGB diagnostics\n");
    s.push_str(&format!("Written:   {} by {writer} (pid {})\n", timestamp(), std::process::id()));
    s.push_str(&format!(
        "Version:   {} (git {}), {} build\n",
        env!("CARGO_PKG_VERSION"),
        env!("FANRGB_GIT"),
        if cfg!(debug_assertions) { "debug" } else { "release" }
    ));
    s.push_str(&format!(
        "Program:   {exe}{}\n",
        if crate::system::is_installed_copy() { " (installed copy)" } else { "" }
    ));
    s.push_str(&format!("Windows:   {}, up {}:{:02} h\n", windows_version(), up / 3600, up % 3600 / 60));
    s.push_str(&format!("Elevated:  {}; simulated hardware: {}\n", info.elevated, info.simulated));
    s
}

/// Hardware report lines, PawnIO driver and module pins.
pub fn hardware_section(info: &HwInfo) -> String {
    let mut s = String::from("\nHardware:\n");
    for l in &info.report {
        s.push_str(&format!("  {l}\n"));
    }
    s.push_str(&format!("  CPU temperature source: {}\n", info.cpu_temp_source));
    s.push_str(&format!(
        "\nPawnIO: driver {} ({})\n",
        info.pawnio_version.as_deref().unwrap_or("not installed"),
        info.pawnio.describe()
    ));
    let pins: Vec<String> =
        crate::hw::pawnio::MODULE_SHA256.iter().map(|(m, h)| format!("{m} sha256 {}…", &h[..12])).collect();
    s.push_str(&format!("  modules: PawnIO.Modules {} ({})\n", crate::hw::pawnio::MODULES_RELEASE, pins.join(", ")));
    s
}

/// Sensor table under group headings; columns sized to the content.
pub fn sensors_section(sensors: &[(SensorDesc, Option<f32>)]) -> String {
    let mut rows = Vec::new();
    let mut group = None;
    for (d, v) in sensors {
        if group != Some(&d.group) {
            group = Some(&d.group);
            rows.push(vec![format!("[{}]", d.group)]);
        }
        rows.push(vec![
            format!("  {}", d.id),
            d.name.clone(),
            v.map(|v| format!("{v:.2}")).unwrap_or_else(|| "-".into()),
            d.kind.unit().to_string(),
        ]);
    }
    format!("\nSensors:\n{}", table_aligned(&rows, "  ", &[2]))
}

/// Settings file, active profile and the safety settings.
pub fn config_section(cfg: &Config) -> String {
    let p = cfg.profile();
    let path = crate::config::config_path();
    let modified = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| format!(" (modified, unix {})", d.as_secs()))
        .unwrap_or_default();
    let mut s = format!("\nSettings: {}{modified}\n", path.display());
    let overrides = if p.fans.is_empty() {
        String::new()
    } else {
        let ids: Vec<&str> = p.fans.keys().map(String::as_str).collect();
        let bios = p.preset == crate::config::Preset::Bios;
        let note = if bios { " (ignored: the BIOS preset drives nothing)" } else { "" };
        format!("; per-fan overrides: {}{note}", ids.join(", "))
    };
    s.push_str(&format!("  active profile #{} '{}' (preset {:?}){overrides}\n", cfg.active_profile, p.name, p.preset));
    s.push_str(&format!(
        "  cpu_critical {:.0} °C, gpu_critical {:.0} °C, poll {} ms\n",
        cfg.general.cpu_critical, cfg.general.gpu_critical, cfg.general.poll_ms
    ));
    s
}

/// Whether something is on a fan header, as far as the tach tells.
pub fn connected_label(f: &crate::model::FanChannelDesc, seen_spinning: bool) -> String {
    match (f.present, f.tach_detected || seen_spinning) {
        (_, true) => "yes (tach)".into(),
        (true, false) => "no speed signal".into(),
        (false, false) => "not detected".into(),
    }
}

/// The role column: the effective role and where it comes from.
pub fn role_label(role: crate::model::FanRole, assigned: Option<crate::config::HeaderRole>) -> String {
    match assigned {
        Some(a) => format!("{role:?} ({}, confirmed)", a.label()),
        None if role == crate::model::FanRole::Unknown => "Unknown (not confirmed, 60 % min)".into(),
        None => format!("{role:?} (board guess)"),
    }
}

/// One header for [`cooling_section`].
pub struct CoolingHeader {
    /// Silkscreen name ("SYS_FAN2").
    pub header: String,
    pub role: crate::model::FanRole,
    pub assigned: Option<crate::config::HeaderRole>,
    pub present: bool,
    pub speed_signal: bool,
}

/// Notes on how a stopped pump or CPU fan would be noticed, with the "BIOS fan-fail warning" advice
/// for the pump header, and the cooling-efficacy state.
pub fn cooling_section(headers: &[CoolingHeader], efficacy: Option<&crate::control::efficacy::EfficacyView>) -> String {
    use crate::model::FanRole;
    let mut s = String::from("\nCooling notes:\n");
    let cpu_fan = headers.first().map(|h| h.header.clone()).unwrap_or_else(|| "CPU_FAN".into());
    let pumps: Vec<&CoolingHeader> = headers.iter().filter(|h| h.present && h.role == FanRole::Pump).collect();
    for p in &pumps {
        let src = if p.assigned.is_some() { "confirmed" } else { "board label" };
        s.push_str(&format!("  Pump on {} ({src}): ", p.header));
        if !p.speed_signal {
            s.push_str(&format!(
                "no speed signal, so neither the BIOS fan-fail warning nor FanRGB's stall alert (FRG-E207) can see it \
                 stop; only the temperature checks (FRG-E305, FRG-E309) can. If the pump has a tach lead, connect it to \
                 {cpu_fan} and enable \"CPU Fan Fail Warning\" in the BIOS (Smart Fan 6).\n"
            ));
        } else if p.header == cpu_fan {
            s.push_str("speed signal present on the CPU header; keep the BIOS \"CPU Fan Fail Warning\" enabled so the board alarms if it stops.\n");
        } else {
            s.push_str(&format!(
                "speed signal present (FanRGB alerts FRG-E207 if it stops while the app runs). BIOS fan-fail warning: \
                 enable the warning for {0} in Smart Fan 6, or move the pump's tach lead to {cpu_fan} and enable \"CPU \
                 Fan Fail Warning\", so the board itself alarms when the pump stops, even without FanRGB.\n",
                p.header
            ));
        }
    }
    if pumps.is_empty() {
        s.push_str(
            "  No header is identified as the AIO pump. A pump powered from SATA/Molex or a header FanRGB can't see is \
             only covered by the temperature checks (FRG-E305, FRG-E309); run \"Identify headers\" to assign it.\n",
        );
    }
    for h in headers.iter().filter(|h| h.present && !h.speed_signal && h.role != FanRole::Pump) {
        let what = if h.role == FanRole::Unknown {
            "role not confirmed: treated as possibly the pump (fan modes wait until every header is identified; \
             afterwards \"Not sure\" keeps a 60 % minimum; watched by FRG-E305/E309)"
        } else if h.role.cools_cpu() {
            "CPU cooling without a speed signal: watched by FRG-E305/E309; a stall can't be seen directly"
        } else {
            "no speed signal: a stall can't be detected"
        };
        s.push_str(&format!("  {}: {what}.\n", h.header));
        if h.header == cpu_fan {
            s.push_str(&format!(
                "    The BIOS \"CPU Fan Fail Warning\" can't work on {cpu_fan} without a speed signal (it is evidently off). \
                 Move a tach-bearing device (ideally the pump) to {cpu_fan} to restore it.\n"
            ));
        }
    }
    if let Some(e) = efficacy {
        s.push_str(&format!(
            "  Cooling efficacy (FRG-E309, inform only{}): {}\n",
            if e.alarm_enabled { "" } else { "; monitor only: every CPU cooling header reports its speed" },
            e.summary()
        ));
    }
    s
}

/// One fan channel of the report.
pub struct ChannelRow {
    pub id: String,
    pub header: String,
    pub role: String,
    pub connected: String,
    pub mode: String,
    pub source: String,
    pub duty: String,
    pub failsafe: String,
}

pub fn channels_section(rows: &[ChannelRow]) -> String {
    let head = ["channel", "header", "role", "connected", "mode", "source", "duty", "failsafe"];
    let mut t: Vec<Vec<String>> = vec![head.map(String::from).into()];
    for r in rows {
        t.push(vec![
            r.id.clone(),
            r.header.clone(),
            r.role.clone(),
            r.connected.clone(),
            r.mode.clone(),
            r.source.clone(),
            r.duty.clone(),
            r.failsafe.clone(),
        ]);
    }
    format!("\nFan channels:\n{}", table(&t, "  "))
}

fn zone_row(id: &str, name: &str, addressable: bool, z: &ZoneConfig, cfg: &Config) -> Vec<String> {
    let c = &z.correction;
    vec![
        id.to_string(),
        name.to_string(),
        if cfg.rgb.enabled && z.enabled { "on".into() } else { "off".into() },
        if addressable { z.led_count.to_string() } else { "-".into() },
        z.color_order.label().to_string(),
        format!("{:.2}", c.gamma),
        format!("{:.2}/{:.2}/{:.2}", c.gain[0], c.gain[1], c.gain[2]),
        (if cfg.rgb.sync { &cfg.rgb.global } else { &z.effect }).kind.label().to_string(),
    ]
}

/// RGB controller state and the per-zone settings.
pub fn rgb_section(cfg: &Config, rgb: &crate::rgb::RgbStatus) -> String {
    let mut s = format!("\nRGB: {}\n", rgb.summary());
    if let Some(d) = &rgb.device {
        s.push_str(&format!(
            "  calibration {}; chip id {:#010x}; keep-settings {}; flags {:#04x}\n",
            d.calibration, d.chip_id, d.persistence, d.support_flags
        ));
        s.push_str(&format!(
            "  0x48 Dynamic Lighting reply byte: {}\n",
            match d.dynamic_lighting_raw {
                Some(b) => format!("{b:#04x} = {}", crate::rgb::dynlight::FirmwareDl::from_raw(b).label()),
                None => "not queried (SuppCmdFlag bit 1 not set)".into(),
            }
        ));
    }
    s.push_str(&format!("  Windows {}\n", rgb.dynamic_lighting.summary()));
    if let Some(t) = &rgb.test {
        s.push_str(&format!("  calibration test showing on {:?}\n", t.zones));
    }
    s.push_str(&format!(
        "  state {}, {:.0} fps (cap {:.0}), {} packets/frame, {} write errors, {} reconnects\n",
        rgb.state.label(),
        rgb.fps,
        rgb.fps_cap,
        rgb.packets_per_frame,
        rgb.write_errors,
        rgb.reconnects
    ));
    s.push_str(&format!(
        "  master brightness {:.2}, sync {}, LED_C slot {} (driven on {:?})\n",
        cfg.rgb.brightness,
        cfg.rgb.sync,
        cfg.rgb.ledc_slot.map(|v| v.to_string()).unwrap_or_else(|| "auto (1 and 4)".into()),
        rgb.ledc_slots
    ));
    let head = ["zone", "name", "on", "LEDs", "colour order", "gamma", "gain R/G/B", "effect"];
    let mut rows: Vec<Vec<String>> = vec![head.map(String::from).into()];
    let mut segs = String::new();
    for z in crate::rgb::zones_for(cfg.rgb.ledc_slot) {
        if z.advanced && !cfg.rgb.show_all_zones && !cfg.rgb.zones.contains_key(&z.id) {
            continue;
        }
        let zc = cfg.rgb.zones.get(&z.id).cloned().unwrap_or_default();
        rows.push(zone_row(&z.id, &z.name, z.addressable, &zc, cfg));
        for sg in &zc.segments {
            segs.push_str(&format!(
                "  {} segment \"{}\": LEDs {}..{} (0-based, end exclusive), gain {:.2}/{:.2}/{:.2}\n",
                z.id, sg.name, sg.from_led, sg.to_led, sg.gain[0], sg.gain[1], sg.gain[2]
            ));
        }
    }
    s.push_str(&table(&rows, "  "));
    s.push_str(&segs);
    if cfg.rgb.dither {
        s.push_str("  temporal dithering on (streamed headers)\n");
    }
    s
}

/// Windows Dynamic Lighting settings (`Software\Microsoft\Lighting` in the desktop user's hive) for the
/// RGB controller.
#[derive(Clone, Debug, Default)]
pub struct DynamicLighting {
    /// Whose hive was read (`HKEY_USERS\<SID>` or `HKCU`).
    pub hive: String,
    /// The desktop user's hive could not be opened (why); nothing else is filled in then.
    pub hive_error: Option<String>,
    pub ambient: Option<u32>,
    pub foreground: Option<u32>,
    pub providers: Vec<String>,
    /// (device key, AmbientLightingEnabled, providers) of the ITE controller's LampArray devices.
    pub devices: Vec<(String, Option<u32>, Vec<String>)>,
}

const LIGHTING_KEY: &str = r"Software\Microsoft\Lighting";

fn providers(root: HKEY, key: &str) -> Vec<String> {
    let mut v = reg_string_values(root, key, 64);
    v.sort_by_key(|(n, _)| n.parse::<u32>().unwrap_or(u32::MAX));
    v.into_iter().map(|(_, d)| d).collect()
}

/// Reads the desktop user's settings (elevated, `HKCU` can be the administrator's hive).
pub fn dynamic_lighting() -> DynamicLighting {
    match crate::util::win::desktop_user_hive() {
        Ok(h) => DynamicLighting { hive: h.describe(), ..dynamic_lighting_in(h.root()) },
        Err(e) => DynamicLighting { hive_error: Some(format!("{e:#}")), ..Default::default() },
    }
}

fn dynamic_lighting_in(root: HKEY) -> DynamicLighting {
    let devices = reg_subkeys(root, &format!(r"{LIGHTING_KEY}\Devices"), 256)
        .into_iter()
        .filter(|k| k.to_ascii_uppercase().contains("VID_048D"))
        .map(|k| {
            let key = format!(r"{LIGHTING_KEY}\Devices\{k}");
            let ambient = reg_get_dword(root, &key, "AmbientLightingEnabled");
            let prov = providers(root, &format!(r"{key}\Providers"));
            (k, ambient, prov)
        })
        .collect();
    DynamicLighting {
        hive: String::new(),
        hive_error: None,
        ambient: reg_get_dword(root, LIGHTING_KEY, "AmbientLightingEnabled"),
        foreground: reg_get_dword(root, LIGHTING_KEY, "ControlledByForegroundApp"),
        providers: providers(root, &format!(r"{LIGHTING_KEY}\Providers")),
        devices,
    }
}

/// Installed MSIX packages of the desktop user (full names).
fn installed_packages() -> Vec<String> {
    let Ok(h) = crate::util::win::desktop_user_hive() else { return Vec::new() };
    reg_subkeys(
        h.root(),
        r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages",
        8192,
    )
}

/// The installed package (full name) of package family `family` ("Name_PublisherId"), if any.
pub fn package_for_family<'a>(family: &str, installed: &'a [String]) -> Option<&'a String> {
    let (name, publisher) = family.rsplit_once('_')?;
    installed.iter().find(|p| p.starts_with(&format!("{name}_")) && p.ends_with(&format!("_{publisher}")))
}

fn provider_state(p: &str, installed: &[String]) -> String {
    if !p.contains('_') {
        return format!("{p} (built in)");
    }
    match package_for_family(p, installed) {
        Some(full) => format!("{p} (package installed: {full})"),
        None => format!("{p} (package not installed: leftover registration)"),
    }
}

pub fn dynamic_lighting_section() -> String {
    let dl = dynamic_lighting();
    let installed = installed_packages();
    let flag = |v: Option<u32>| v.map(|v| if v != 0 { "on" } else { "off" }).unwrap_or("not set");
    if let Some(e) = &dl.hive_error {
        return format!("\nDynamic Lighting: unknown, the desktop user's registry was not found: {e}\n");
    }
    let mut s = format!(
        "\nDynamic Lighting (desktop user, {}): background control {}, foreground apps {}\n",
        dl.hive,
        flag(dl.ambient),
        flag(dl.foreground)
    );
    if !dl.providers.is_empty() {
        let p: Vec<String> = dl.providers.iter().map(|p| provider_state(p, &installed)).collect();
        s.push_str(&format!("  providers: {}\n", p.join("; ")));
    }
    if dl.devices.is_empty() {
        s.push_str("  no RGB Fusion (VID 048D) LampArray device registered\n");
    }
    for (k, ambient, prov) in &dl.devices {
        let p: Vec<String> = prov.iter().map(|p| provider_state(p, &installed)).collect();
        s.push_str(&format!("  {k}: background control {}; providers: {}\n", flag(*ambient), p.join("; ")));
    }
    s
}

/// Services / drivers that touch the fan or RGB controller: (name or prefix, prefix match, what it is).
const CONFLICT_SERVICES: &[(&str, bool, &str)] = &[
    ("GigabyteUpdateService", false, "GIGABYTE update service: can reinstall or start Control Center parts"),
    ("EasyTuneEngineService", false, "GIGABYTE EasyTune engine: overrides fan settings"),
    ("gdrv", true, "GIGABYTE low-level driver: direct hardware access for GCC / EasyTune"),
    ("AMDRyzenMaster", true, "AMD Ryzen Master driver: direct CPU / SMU access"),
    ("Razer Chroma SDK Service", false, "Razer Chroma: may contend for RGB"),
];

/// Running programs that may write the RGB controller through Windows Dynamic Lighting (LampArray).
const RGB_CONTENDERS: &[(&str, &str)] = &[
    ("razerwdl.exe", "Razer Synapse LampArray bridge"),
    ("razerappengine.exe", "Razer Synapse"),
    ("rzsmartlightingdevicemanager.exe", "Razer smart lighting"),
];

fn service_state(name: &str) -> &'static str {
    use windows_sys::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_MANAGER_CONNECT, SERVICE_QUERY_STATUS,
        SERVICE_RUNNING, SERVICE_STATUS,
    };
    let n = wide(name);
    // SAFETY: read-only SCM queries; every handle opened here is closed here; `st` is plain data.
    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return "installed";
        }
        let svc = OpenServiceW(scm, n.as_ptr(), SERVICE_QUERY_STATUS);
        let state = if svc.is_null() {
            "installed"
        } else {
            let mut st: SERVICE_STATUS = std::mem::zeroed();
            let s = if QueryServiceStatus(svc, &mut st) == 0 {
                "installed"
            } else if st.dwCurrentState == SERVICE_RUNNING {
                "running"
            } else {
                "stopped"
            };
            CloseServiceHandle(svc);
            s
        };
        CloseServiceHandle(scm);
        state
    }
}

/// Installed drivers (SetupAPI), BIOS and chipset package, with the last Windows Update result.
pub fn drivers_section(st: &crate::drivers::DriverState) -> String {
    use crate::drivers::{DriverStatus, fmt_unix};
    let mut s = format!("\nDrivers (inform only; inventory {} ms):\n", st.inventory_ms);
    if let Some(e) = &st.inventory_error {
        s.push_str(&format!("  {e}\n"));
    }
    let rows: Vec<Vec<String>> = st
        .rows
        .iter()
        .map(|r| {
            let status = match (&r.status, &r.latest) {
                (DriverStatus::UpdateAvailable, Some(o)) => format!(
                    "update: {} ({})",
                    o.version.as_deref().unwrap_or(&o.title),
                    o.date.as_deref().unwrap_or("?")
                ),
                (s, _) => s.label().to_lowercase(),
            };
            let vendor = match (&r.catalog, r.catalog_status) {
                (Some(m), s) => format!("{}: {} ({})", s.label().to_lowercase(), m.version, m.id),
                (None, s) => s.label().to_lowercase(),
            };
            vec![
                r.category.label().to_string(),
                if r.count > 1 { format!("{} (x{})", r.device, r.count) } else { r.device.clone() },
                r.installed.clone(),
                r.installed_date.clone().unwrap_or_else(|| "-".into()),
                r.provider.clone(),
                if r.inf.is_empty() { "-".into() } else { r.inf.clone() },
                if r.signer.as_deref().is_some_and(|x| x.contains("Hardware Compatibility")) {
                    "WHQL".into()
                } else {
                    r.signer.clone().unwrap_or_else(|| "-".into())
                },
                status,
                vendor,
            ]
        })
        .collect();
    s.push_str(&table(&rows, "  "));
    s.push_str(&catalog_lines(&st.catalog, crate::updater::now()));
    let checked = st.last_checked.map(fmt_unix).unwrap_or_else(|| "never".into());
    s.push_str(&format!(
        "  Windows Update: last checked {checked}; automatic daily check {}; {} update(s) offered\n",
        if st.auto_check { "on" } else { "off" },
        st.updates_available()
    ));
    if let Some(e) = &st.error {
        let at = st.last_attempt.map(fmt_unix).unwrap_or_default();
        s.push_str(&format!("  last attempt {at}: {e}\n"));
    }
    s
}

/// The vendor driver catalog in use: serial, age, expiry, signing key, last download.
pub fn catalog_lines(c: &crate::drivers::CatalogInfo, now: u64) -> String {
    use crate::drivers::fmt_unix;
    let mut s = String::new();
    match &c.loaded {
        Some(l) => {
            let age_days = now.saturating_sub(l.generated_at) / 86_400;
            s.push_str(&format!(
                "  Vendor catalog: serial {}, generated {} ({age_days} d ago), expires {}{}, {} entries, signed by {} key {}{}\n",
                l.serial,
                fmt_unix(l.generated_at),
                fmt_unix(l.expires),
                if c.expired(now) { " [OUT OF DATE]" } else { "" },
                l.entries,
                if l.key == 0 { "primary" } else { "backup" },
                l.key_id,
                if c.demo { " (demo preview, not downloaded)" } else { "" }
            ));
        }
        None => s.push_str(&format!(
            "  Vendor catalog: none loaded ({})\n",
            if c.demo { "demo" } else { "not downloaded yet" }
        )),
    }
    let fetched = c.fetched_at.map(fmt_unix).unwrap_or_else(|| "never".into());
    s.push_str(&format!(
        "  Vendor catalog: last verified download {fetched}; update token saved: {} (optional)\n",
        c.token_saved
    ));
    if let Some(e) = &c.error {
        let at = c.last_attempt.map(fmt_unix).unwrap_or_default();
        s.push_str(&format!("  Vendor catalog: last attempt {at}: {e}\n"));
    }
    s
}

/// Everything that may fight over the fan or RGB controller: running programs, services and
/// drivers, installed GIGABYTE software.
pub fn conflicts_section() -> String {
    let mut s = String::from("\nConflicting software:\n");
    let running = crate::util::win::running_processes();
    let mut any = false;
    for (name, why) in conflicting_software() {
        s.push_str(&format!("  running: {name} ({why})\n"));
        any = true;
    }
    for (exe, name) in RGB_CONTENDERS {
        if running.iter().any(|p| p == exe) {
            s.push_str(&format!("  running: {name} ({exe}): may contend for RGB\n"));
            any = true;
        }
    }
    let services = reg_subkeys(HKEY_LOCAL_MACHINE, r"SYSTEM\CurrentControlSet\Services", 4096);
    for (pat, prefix, why) in CONFLICT_SERVICES {
        let p = pat.to_ascii_lowercase();
        for svc in services.iter().filter(|k| {
            let k = k.to_ascii_lowercase();
            if *prefix { k.starts_with(&p) } else { k == p }
        }) {
            s.push_str(&format!("  service {svc}: {} ({why})\n", service_state(svc)));
            any = true;
        }
    }
    let gcc = std::path::Path::new(r"C:\Program Files\GIGABYTE\Control Center");
    if gcc.exists() {
        s.push_str(&format!("  installed: GIGABYTE Control Center at {}\n", gcc.display()));
        any = true;
    }
    if !any {
        s.push_str("  none found\n");
    }
    s
}

/// Removes what identifies the person from a report before it is written: users attach
/// `diagnostics.txt` to public issues. The Windows account name in profile paths
/// (`C:\Users\<name>\...`, also in the `\\`-escaped form of logged paths) becomes `<user>`, and the
/// machine / domain part of account SIDs (`S-1-5-21-a-b-c-rid`) becomes `<machine>`. Everything else
/// (hardware, versions, settings, log text) is kept.
pub fn scrub_personal(text: &str) -> String {
    const USERS: &str = "\\users\\";
    // ASCII-only lower-casing keeps byte offsets identical to `text`.
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    while let Some(i) = lower[pos..].find(USERS) {
        let mut start = pos + i + USERS.len();
        while text[start..].starts_with('\\') {
            start += 1; // escaped separator ("C:\\Users\\name")
        }
        out.push_str(&text[pos..start]);
        let tail = &text[start..];
        let end = tail.find(['\\', '"', '\'', '\r', '\n']).unwrap_or(tail.len());
        let name = &tail[..end];
        let shared = ["public", "default", "all users"].iter().any(|k| name.eq_ignore_ascii_case(k));
        out.push_str(if name.is_empty() || shared { name } else { "<user>" });
        pos = start + end;
    }
    out.push_str(&text[pos..]);
    scrub_sids(&out)
}

/// `S-1-5-21-<three machine or domain numbers>-<rid>` -> `S-1-5-21-<machine>-<rid>`.
fn scrub_sids(text: &str) -> String {
    const PREFIX: &str = "S-1-5-21-";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(PREFIX) {
        let after = &rest[i + PREFIX.len()..];
        let len = after.find(|c: char| !(c.is_ascii_digit() || c == '-')).unwrap_or(after.len());
        let groups: Vec<&str> = after[..len].split('-').collect();
        out.push_str(&rest[..i + PREFIX.len()]);
        if groups.len() >= 4 && groups[..4].iter().all(|g| !g.is_empty()) {
            out.push_str("<machine>-");
            out.push_str(&groups[3..].join("-"));
        } else {
            out.push_str(&after[..len]);
        }
        rest = &after[len..];
    }
    out.push_str(rest);
    out
}

/// The coded events (`[FRG-E…]`) among the last 400 log lines, then the last `n` lines.
pub fn log_section(n: usize) -> String {
    let lines = crate::util::logger::tail(400);
    let mut s = String::from("\nRecent coded events (log):\n");
    let coded: Vec<&String> = lines.iter().filter(|l| l.contains("[FRG-E")).collect();
    for l in coded.iter().skip(coded.len().saturating_sub(30)) {
        s.push_str(&format!("  {l}\n"));
    }
    if coded.is_empty() {
        s.push_str("  none\n");
    }
    s.push_str(&format!("\nLast {n} log lines:\n"));
    for l in lines.iter().skip(lines.len().saturating_sub(n)) {
        s.push_str(&format!("  {l}\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_do_not_name_the_user() {
        let report = "Program:   C:\\Users\\Aoife Byrne\\Downloads\\fanrgb.exe\n\
                      Settings: c:\\users\\aoife\\AppData\\Roaming\\FanRGB\\config.json (modified, unix 1)\n\
                      Dynamic Lighting (desktop user, HKEY_USERS\\S-1-5-21-1004336348-1177238915-682003330-1001): on\n\
                      log: state: Secure \"C:\\\\Users\\\\aoife\\\\x\" and C:\\Users\\Public\\y, S-1-5-18, C:\\Users\\bare";
        let s = scrub_personal(report);
        assert!(!s.to_lowercase().contains("aoife") && !s.contains("1004336348") && !s.contains("bare"), "{s}");
        assert!(s.contains("C:\\Users\\<user>\\Downloads\\fanrgb.exe"), "{s}");
        assert!(s.contains("c:\\users\\<user>\\AppData\\Roaming\\FanRGB\\config.json"), "{s}");
        assert!(s.contains("HKEY_USERS\\S-1-5-21-<machine>-1001"), "{s}");
        assert!(
            s.contains("C:\\\\Users\\\\<user>\\\\x") && s.contains("C:\\Users\\Public\\y") && s.contains("S-1-5-18"),
            "{s}"
        );
        // Nothing else changes.
        let plain = "FanRGB diagnostics\nHardware:\n  IT8689E at 0xa40\n  C:\\ProgramData\\FanRGB\\fanrgb.log\n";
        assert_eq!(scrub_personal(plain), plain);
    }

    /// Long "group / name" labels no longer shift the columns.
    #[test]
    fn table_columns_fit_the_longest_cell() {
        let rows = vec![
            vec!["mb.fan1".to_string(), "Motherboard (IT8689E) / System 1".into(), "711.00".into(), "RPM".into()],
            vec!["cpu.temp".to_string(), "CPU".into(), "45.25".into(), "°C".into()],
        ];
        let t = table_aligned(&rows, "  ", &[2]);
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], "  mb.fan1   Motherboard (IT8689E) / System 1  711.00  RPM");
        let pos = |l: &str, pat: &str| l[..l.find(pat).unwrap()].chars().count();
        assert_eq!(pos(lines[1], "CPU"), pos(lines[0], "Motherboard"));
        assert_eq!(pos(lines[1], "45.25") + 5, pos(lines[0], "711.00") + 6, "values right-aligned");
        assert_eq!(pos(lines[1], "°C"), pos(lines[0], "RPM"));
    }

    #[test]
    fn package_families_match_full_names() {
        let installed = vec![
            "RazerDynamicLighting_1.0.3.0_x64__qemfkr3nbbywc".to_string(),
            "Microsoft.Paint_11.2.0_x64__8wekyb3d8bbwe".into(),
        ];
        assert_eq!(
            package_for_family("RazerDynamicLighting_qemfkr3nbbywc", &installed).map(String::as_str),
            Some("RazerDynamicLighting_1.0.3.0_x64__qemfkr3nbbywc")
        );
        assert_eq!(package_for_family("65d483df-b37e-4fcf-94de-8b795233db63_1mmjbktjj1mkp", &installed), None);
        assert!(provider_state("WindowsLighting", &installed).contains("built in"));
        assert!(provider_state("65d483df-b37e-4fcf-94de-8b795233db63_1mmjbktjj1mkp", &installed).contains("leftover"));
    }

    /// The registry / SCM queries are read-only and work on this machine.
    #[test]
    fn environment_queries_work() {
        assert!(windows_version().contains("(build "));
        if crate::util::win::skip_without_unelevated_shell() {
            return;
        }
        // Review M4: the desktop user's hive (tests run unelevated: HKCU itself).
        let dl = dynamic_lighting();
        assert!(dl.hive_error.is_none(), "{:?}", dl.hive_error);
        if !crate::util::win::is_elevated() {
            assert_eq!(dl.hive, "HKCU");
        }
        assert!(dynamic_lighting_section().contains("(desktop user, "));
        assert!(conflicts_section().starts_with("\nConflicting software:"));
        assert!(timestamp().ends_with(" UTC)"));
    }
}
