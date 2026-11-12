//! Custom widgets built on the design tokens.

pub mod board;
pub mod calibration;
pub mod card;
pub mod chart;
pub mod color;
pub mod controls;
pub mod curve;
pub mod fan_modes;
pub mod gallery;
pub mod gauge;
pub mod hero;
pub mod issue_card;
pub mod led;
pub mod safety;
pub mod text;
pub mod thumb;

use crate::util::c_to_f;

pub fn fmt_temp(v: Option<f32>, fahrenheit: bool) -> String {
    match v {
        Some(t) if fahrenheit => format!("{:.0}°F", c_to_f(t)),
        Some(t) => format!("{t:.0}°C"),
        None => "—".into(),
    }
}

/// Temperature as a bare number + degree sign ("63°"), for compact chips.
pub fn fmt_deg(v: Option<f32>, fahrenheit: bool) -> String {
    match v {
        Some(t) if fahrenheit => format!("{:.0}°", c_to_f(t)),
        Some(t) => format!("{t:.0}°"),
        None => "—".into(),
    }
}

pub fn fmt_temp1(v: Option<f32>, fahrenheit: bool) -> String {
    match v {
        Some(t) if fahrenheit => format!("{:.1} °F", c_to_f(t)),
        Some(t) => format!("{t:.1} °C"),
        None => "—".into(),
    }
}

/// "1,180" style thousands separators.
pub fn thousands(v: f32) -> String {
    let n = v.round() as i64;
    let s = n.abs().to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3 + 1);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 { format!("-{out}") } else { out }
}

pub fn fmt_rpm(v: Option<f32>) -> String {
    match v {
        Some(r) => format!("{} rpm", thousands(r)),
        None => "—".into(),
    }
}

/// Local wall-clock time `secs_ago` seconds before now, as `HH:MM:SS`.
pub fn local_clock(secs_ago: f64) -> String {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let st = unsafe {
        let mut st = std::mem::zeroed();
        GetLocalTime(&mut st);
        st
    };
    let now = st.wHour as i64 * 3600 + st.wMinute as i64 * 60 + st.wSecond as i64;
    let t = (now - secs_ago.round() as i64).rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_format() {
        assert_eq!(thousands(0.0), "0");
        assert_eq!(thousands(980.0), "980");
        assert_eq!(thousands(1180.4), "1,180");
        assert_eq!(thousands(1234567.0), "1,234,567");
        assert_eq!(thousands(-2500.0), "-2,500");
    }
}
