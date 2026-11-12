//! Vendor-neutral GPU telemetry.
//!
//! * Temperature, fan RPM and power come from `D3DKMTQueryAdapterInfo(ADAPTERPERFDATA)`,
//!   the same source Task Manager uses (works for Intel Arc, NVIDIA and AMD, no admin).
//! * Utilization comes from the "GPU Engine" performance counters.

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::null_mut;

use windows_sys::Wdk::Graphics::Direct3D::*;
use windows_sys::Win32::Foundation::LUID;
use windows_sys::Win32::System::Performance::*;

use crate::util::win::{from_wide, wide};

pub struct GpuAdapter {
    handle: u32,
    pub luid: (i32, u32),
    pub name: String,
    /// PCI vendor id (0 = unknown).
    pub vendor: u32,
    perf_supported: bool,
}

pub const VENDOR_INTEL: u32 = 0x8086;

impl GpuAdapter {
    /// `D3DKMT_ADAPTER_PERFDATA::Power` is reported in watts (`gpu<N>.power` is a Power sensor)
    /// rather than the documented tenths of a percent.
    pub fn power_in_watts(&self) -> bool {
        self.vendor == VENDOR_INTEL
    }
}

#[derive(Clone, Debug, Default)]
pub struct GpuReading {
    pub temp_c: Option<f32>,
    pub fan_rpm: Option<f32>,
    /// Board power in W (smoothed), for adapters that report watts.
    pub power_w: Option<f32>,
    /// Board power in % of TBP, for adapters that report the documented percentage.
    pub power_pct: Option<f32>,
    /// Raw `Power` field of the last sample (diagnostics).
    pub power_raw: Option<u32>,
    pub load_pct: Option<f32>,
}

/// Highest plausible board power of an Intel Arc card (B580: 190 W TBP; A770: 225 W).
pub const INTEL_POWER_MAX_W: f32 = 300.0;
/// Smoothing time constant of the Intel power reading (the raw value swings 0.2-27 W at idle).
const POWER_TAU_S: f32 = 3.0;

/// One decoded `Power` sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PowerSample {
    Watts(f32),
    Percent(f32),
    /// Outside the plausible range of the vendor's unit: not used.
    Implausible,
}

/// Decodes `D3DKMT_ADAPTER_PERFDATA::Power`. Microsoft documents tenths of a percent; Intel's driver
/// fills milliwatts (40 idle samples on an Arc B580 averaged 11 500 = 11.5 W).
/// Values outside the vendor's plausible range are never passed on (they would reach the AI).
pub fn decode_power(vendor: u32, raw: u32) -> PowerSample {
    if vendor == VENDOR_INTEL {
        let w = raw as f32 / 1000.0;
        return if w <= INTEL_POWER_MAX_W { PowerSample::Watts(w) } else { PowerSample::Implausible };
    }
    if raw <= 1000 { PowerSample::Percent(raw as f32 / 10.0) } else { PowerSample::Implausible }
}

/// Exponential moving average with a time constant (first sample passes through).
#[derive(Clone, Copy, Debug, Default)]
pub struct Ema {
    value: Option<f32>,
}

impl Ema {
    pub fn update(&mut self, x: f32, dt_s: f32, tau_s: f32) -> f32 {
        let v = match self.value {
            Some(v) if dt_s.is_finite() && dt_s > 0.0 => v + (x - v) * (1.0 - (-dt_s / tau_s).exp()),
            Some(v) => v,
            None => x,
        };
        self.value = Some(v);
        v
    }
}

pub struct GpuMonitor {
    adapters: Vec<GpuAdapter>,
    pdh: Option<EngineLoad>,
    power_ema: Vec<Ema>,
    last_read: Option<std::time::Instant>,
    /// A sample in unknown units was logged for this adapter.
    odd_power_logged: Vec<bool>,
}

unsafe fn query<T>(handle: u32, ty: KMTQUERYADAPTERINFOTYPE, data: &mut T) -> bool {
    let mut q = D3DKMT_QUERYADAPTERINFO {
        hAdapter: handle,
        Type: ty,
        pPrivateDriverData: data as *mut T as *mut c_void,
        PrivateDriverDataSize: std::mem::size_of::<T>() as u32,
    };
    unsafe { D3DKMTQueryAdapterInfo(&mut q) == 0 }
}

impl GpuMonitor {
    pub fn new() -> Self {
        let mut adapters = Vec::new();
        unsafe {
            let mut e = D3DKMT_ENUMADAPTERS2::default();
            if D3DKMTEnumAdapters2(&mut e) == 0 && e.NumAdapters > 0 {
                let mut infos = vec![D3DKMT_ADAPTERINFO::default(); e.NumAdapters as usize];
                e.pAdapters = infos.as_mut_ptr();
                if D3DKMTEnumAdapters2(&mut e) == 0 {
                    for info in infos.iter().take(e.NumAdapters as usize) {
                        let mut ty = D3DKMT_ADAPTERTYPE::default();
                        let keep = query(info.hAdapter, KMTQAITYPE_ADAPTERTYPE, &mut ty) && {
                            let v = ty.Anonymous.Value;
                            v & 0x1 != 0 && v & 0x4 == 0 // render-capable, not a software device
                        };
                        if !keep {
                            D3DKMTCloseAdapter(&D3DKMT_CLOSEADAPTER { hAdapter: info.hAdapter });
                            continue;
                        }
                        let mut reg: D3DKMT_ADAPTERREGISTRYINFO = std::mem::zeroed();
                        let name = if query(info.hAdapter, KMTQAITYPE_ADAPTERREGISTRYINFO, &mut reg) {
                            from_wide(&reg.AdapterString)
                        } else {
                            "GPU".to_string()
                        };
                        let mut pd = D3DKMT_ADAPTER_PERFDATA::default();
                        let perf_supported = query(info.hAdapter, KMTQAITYPE_ADAPTERPERFDATA, &mut pd);
                        let mut ids = D3DKMT_QUERY_DEVICE_IDS::default();
                        let vendor = if query(info.hAdapter, KMTQAITYPE_PHYSICALADAPTERDEVICEIDS, &mut ids) {
                            ids.DeviceIds.VendorID
                        } else {
                            0
                        };
                        let LUID { LowPart, HighPart } = info.AdapterLuid;
                        adapters.push(GpuAdapter {
                            handle: info.hAdapter,
                            luid: (HighPart, LowPart),
                            name: if name.is_empty() { "GPU".into() } else { name },
                            vendor,
                            perf_supported,
                        });
                    }
                }
            }
        }
        for a in &adapters {
            log::info!(
                "GPU: {} (vendor {:04x}, luid {:x}:{:x}, telemetry: {}, power unit: {})",
                a.name,
                a.vendor,
                a.luid.0,
                a.luid.1,
                a.perf_supported,
                if a.power_in_watts() { "mW" } else { "0.1 %" }
            );
            if !a.perf_supported {
                log::info!(
                    "[{}] {}: the driver exposes no temperature/fan telemetry",
                    crate::error::ErrorCode::GpuSensor,
                    a.name
                );
            }
        }
        let n = adapters.len();
        GpuMonitor {
            adapters,
            pdh: EngineLoad::new(),
            power_ema: vec![Ema::default(); n],
            last_read: None,
            odd_power_logged: vec![false; n],
        }
    }

    pub fn adapters(&self) -> &[GpuAdapter] {
        &self.adapters
    }

    pub fn read(&mut self) -> Vec<GpuReading> {
        let loads = self.pdh.as_mut().map(|p| p.sample()).unwrap_or_default();
        let now = std::time::Instant::now();
        let dt = self.last_read.map(|t| now.duration_since(t).as_secs_f32()).unwrap_or(0.0);
        self.last_read = Some(now);
        let mut out = Vec::with_capacity(self.adapters.len());
        for (i, a) in self.adapters.iter().enumerate() {
            let mut r = GpuReading::default();
            if a.perf_supported {
                let mut pd = D3DKMT_ADAPTER_PERFDATA::default();
                if unsafe { query(a.handle, KMTQAITYPE_ADAPTERPERFDATA, &mut pd) } {
                    if pd.Temperature > 0 && pd.Temperature < 2000 {
                        r.temp_c = Some(pd.Temperature as f32 / 10.0);
                    }
                    r.fan_rpm = Some(pd.FanRPM as f32);
                    r.power_raw = Some(pd.Power);
                    // 0: the driver does not report power.
                    match (pd.Power > 0).then(|| decode_power(a.vendor, pd.Power)) {
                        None => {}
                        Some(PowerSample::Watts(w)) => r.power_w = Some(self.power_ema[i].update(w, dt, POWER_TAU_S)),
                        Some(PowerSample::Percent(p)) => r.power_pct = Some(p),
                        Some(PowerSample::Implausible) => {
                            if !std::mem::replace(&mut self.odd_power_logged[i], true) {
                                log::info!(
                                    "[{}] {}: GPU power value {} is outside the range of its unit ({}); ignored",
                                    crate::error::ErrorCode::ImplausibleReading,
                                    a.name,
                                    pd.Power,
                                    if a.power_in_watts() { "mW" } else { "0.1 %" }
                                );
                            }
                        }
                    }
                }
            }
            r.load_pct = loads.get(&a.luid).copied().or(if self.pdh.is_some() { Some(0.0) } else { None });
            out.push(r);
        }
        out
    }
}

impl Drop for GpuMonitor {
    fn drop(&mut self) {
        for a in &self.adapters {
            unsafe {
                D3DKMTCloseAdapter(&D3DKMT_CLOSEADAPTER { hAdapter: a.handle });
            }
        }
    }
}

// SAFETY: adapter handles are process-wide kernel handles; PDH handles are only used by the owner.
unsafe impl Send for GpuMonitor {}

/// "\GPU Engine(*)\Utilization Percentage" aggregated like Task Manager:
/// sum over processes per engine, then the busiest engine per adapter.
struct EngineLoad {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
    /// Typed (8-byte aligned) storage for PdhGetFormattedCounterArrayW; the API appends the
    /// instance name strings after the item array, so it is sized in bytes and rounded up.
    buf: Vec<PDH_FMT_COUNTERVALUE_ITEM_W>,
}

impl EngineLoad {
    fn new() -> Option<Self> {
        unsafe {
            let mut query: PDH_HQUERY = null_mut();
            if PdhOpenQueryW(std::ptr::null(), 0, &mut query) != 0 {
                return None;
            }
            let path = wide(r"\GPU Engine(*)\Utilization Percentage");
            let mut counter: PDH_HCOUNTER = null_mut();
            if PdhAddEnglishCounterW(query, path.as_ptr(), 0, &mut counter) != 0 {
                PdhCloseQuery(query);
                return None;
            }
            PdhCollectQueryData(query);
            Some(Self { query, counter, buf: Vec::new() })
        }
    }

    fn sample(&mut self) -> HashMap<(i32, u32), f32> {
        let mut per_engine: HashMap<(i32, u32, u32, u32), f64> = HashMap::new();
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return HashMap::new();
            }
            let mut size = 0u32;
            let mut count = 0u32;
            let rc = PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, null_mut());
            if rc != PDH_MORE_DATA || size == 0 {
                return HashMap::new();
            }
            let item = std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
            let want = (size as usize).div_ceil(item) + 1;
            if self.buf.len() < want {
                self.buf.resize(want, std::mem::zeroed());
            }
            // The buffer may be larger than `size`; tell PDH exactly how much it may use.
            size = (self.buf.len() * item) as u32;
            let items = self.buf.as_mut_ptr();
            if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, items) != 0 {
                return HashMap::new();
            }
            for i in 0..count as usize {
                let item = &*items.add(i);
                if item.FmtValue.CStatus != PDH_CSTATUS_VALID_DATA && item.FmtValue.CStatus != PDH_CSTATUS_NEW_DATA {
                    continue;
                }
                let name = pwstr_to_string(item.szName);
                if let Some(key) = parse_engine_instance(&name) {
                    *per_engine.entry(key).or_default() += item.FmtValue.Anonymous.doubleValue;
                }
            }
        }
        let mut per_adapter: HashMap<(i32, u32), f32> = HashMap::new();
        for ((hi, lo, _phys, _eng), v) in per_engine {
            let e = per_adapter.entry((hi, lo)).or_default();
            *e = e.max(v.min(100.0) as f32);
        }
        per_adapter
    }
}

impl Drop for EngineLoad {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}

unsafe fn pwstr_to_string(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0;
    unsafe {
        while *p.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
    }
}

/// Parses "pid_1234_luid_0x00000000_0x0000378D_phys_0_eng_3_engtype_3D".
fn parse_engine_instance(name: &str) -> Option<(i32, u32, u32, u32)> {
    let parts: Vec<&str> = name.split('_').collect();
    let find = |key: &str| parts.iter().position(|p| *p == key);
    let li = find("luid")?;
    let hi = u32::from_str_radix(parts.get(li + 1)?.trim_start_matches("0x"), 16).ok()? as i32;
    let lo = u32::from_str_radix(parts.get(li + 2)?.trim_start_matches("0x"), 16).ok()?;
    let phys = parts.get(find("phys")? + 1)?.parse().ok()?;
    let eng = parts.get(find("eng")? + 1)?.parse().ok()?;
    Some((hi, lo, phys, eng))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Intel reports milliwatts; the documented tenths of a percent stay
    /// for other vendors; nothing implausible is passed on.
    #[test]
    fn power_units_per_vendor() {
        // Raw idle samples of the Arc B580.
        assert_eq!(decode_power(VENDOR_INTEL, 11_500), PowerSample::Watts(11.5));
        assert_eq!(decode_power(VENDOR_INTEL, 179), PowerSample::Watts(0.179));
        assert_eq!(decode_power(VENDOR_INTEL, 190_000), PowerSample::Watts(190.0));
        assert_eq!(decode_power(VENDOR_INTEL, 900_000), PowerSample::Implausible);
        // Documented unit (NVIDIA, AMD, unknown vendor id 0).
        for v in [0x10DE, 0x1002, 0] {
            assert_eq!(decode_power(v, 946), PowerSample::Percent(94.6));
            assert_eq!(decode_power(v, 11_500), PowerSample::Implausible, "never 1150 %");
        }
    }

    #[test]
    fn power_smoothing() {
        let mut e = Ema::default();
        assert_eq!(e.update(10.0, 0.0, 3.0), 10.0, "first sample passes");
        // The B580's idle noise (0.2-27 W every 0.5 s) settles near its mean.
        let raw = [7.4, 10.4, 2.5, 9.6, 19.5, 26.8, 10.7, 0.2, 0.9, 6.5, 8.2, 22.9, 27.0, 22.4, 25.0, 1.3, 2.2, 4.0];
        let mut lo = f32::MAX;
        let mut hi = f32::MIN;
        for (i, w) in raw.iter().cycle().take(200).enumerate() {
            let v = e.update(*w, 0.5, 3.0);
            if i > 40 {
                lo = lo.min(v);
                hi = hi.max(v);
            }
        }
        assert!(lo > 4.0 && hi < 22.0, "smoothed range {lo}..{hi}");
        assert_eq!(e.update(f32::NAN, f32::NAN, 3.0), e.update(5.0, 0.0, 3.0), "bad dt keeps the value");
    }

    #[test]
    fn parses_engine_instance() {
        let k = super::parse_engine_instance("pid_7820_luid_0x00000000_0x0000378D_phys_0_eng_3_engtype_VideoDecode");
        assert_eq!(k, Some((0, 0x378D, 0, 3)));
    }
}
