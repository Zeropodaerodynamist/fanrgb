pub mod fsx;
pub mod logger;
pub mod paths;
pub mod sec;
pub mod win;

/// Exponential smoothing factor for a time constant `tau` (seconds) and step `dt`.
pub fn ema_alpha(dt: f32, tau: f32) -> f32 {
    if tau <= 1e-3 { 1.0 } else { 1.0 - (-dt / tau).exp() }
}

pub fn c_to_f(c: f32) -> f32 {
    c * 9.0 / 5.0 + 32.0
}
