//! CPU utilization (GetSystemTimes) and memory load.

use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::GetSystemTimes;

fn ft(f: FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

#[derive(Default)]
pub struct CpuLoad {
    last: Option<(u64, u64)>,
}

impl CpuLoad {
    /// Total CPU utilization in percent since the previous call.
    pub fn sample(&mut self) -> Option<f32> {
        let (mut idle, mut kernel, mut user) = unsafe { std::mem::zeroed::<(FILETIME, FILETIME, FILETIME)>() };
        if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
            return None;
        }
        let idle = ft(idle);
        let total = ft(kernel) + ft(user); // kernel time includes idle time
        let result = self.last.and_then(|(pi, pt)| {
            let dt = total.saturating_sub(pt);
            if dt == 0 {
                return None;
            }
            let di = idle.saturating_sub(pi);
            Some(((1.0 - di as f64 / dt as f64) * 100.0).clamp(0.0, 100.0) as f32)
        });
        self.last = Some((idle, total));
        result
    }
}

pub fn memory_load() -> Option<f32> {
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut m) } == 0 {
        return None;
    }
    Some(m.dwMemoryLoad as f32)
}
