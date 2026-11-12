//! Thermal safety thresholds, in one place.
//!
//! Context, the reference PC: Ryzen 5 7500F, Tjmax 95 °C (= [`crate::config::CPU_CRITICAL_MAX`], the
//! highest `cpu_critical` the settings accept); the BIOS
//! Smart Fan curve already runs CPU_FAN at 100 % from 70 °C. A long all-core load on this cooler can
//! reach the high 80s, so with the values below:
//!
//! | Rule | Arms at | With `cpu_critical = 92` |
//! |---|---|---|
//! | FRG-E301 critical latch (median of 3 CPU readings) | `cpu_critical` | 92 °C |
//! | FRG-E301 latch release | below `cpu_critical - CRITICAL_RELEASE_MARGIN_C` for `CRITICAL_RELEASE_HOLD` | below 87 °C for 10 s |
//! | FRG-E305 "near critical at full drive" (tach-less CPU-chain header at ≥ 90 % for 60 s) | `cpu_critical - NEAR_CRITICAL_MARGIN_C` | 84 °C |
//! | Fan calibration abort | above `cpu_critical - CALIBRATION_ABORT_MARGIN_C` | 84 °C |
//!
//! E301 and E305 force the CPU chain to 100 %; FRG-E309 (cooling efficacy) only informs in v0.2.
//!
//! **Tuning (hardware test step 6).** If FRG-E305 fires with Tctl between the E305 threshold and
//! `cpu_critical` while HWiNFO shows the fans at 100 % and the CPU otherwise fine, it is a false
//! positive: change [`NEAR_CRITICAL_MARGIN_C`] (one line; e.g. `3.0` arms the rule within 3 °C of
//! the critical temperature, 89 °C at 92). The unit tests derive their temperatures from these
//! constants, so they follow. The alternative tuning, raising `cpu_critical` to 95 in Settings, needs
//! no code change.

use std::time::Duration;

/// FRG-E305 "near critical": the rule arms at `cpu_critical - NEAR_CRITICAL_MARGIN_C`.
pub const NEAR_CRITICAL_MARGIN_C: f32 = 8.0;

/// FRG-E301: the critical latch releases once the CPU (and GPU) stayed this far below their critical
/// temperature for [`CRITICAL_RELEASE_HOLD`].
pub const CRITICAL_RELEASE_MARGIN_C: f32 = 5.0;
/// FRG-E301: how long the temperatures must stay below the release point.
pub const CRITICAL_RELEASE_HOLD: Duration = Duration::from_secs(10);

/// A fan calibration (which sweeps a header down to 0 %) stops above `cpu_critical - this`.
pub const CALIBRATION_ABORT_MARGIN_C: f32 = 8.0;

/// Hard cooling floor (audit FR-CTL-001): from `cpu_critical - ENVELOPE_START_MARGIN_C` the lowest
/// duty any software-driven header may get rises linearly to [`ENVELOPE_MAX_DUTY`] at
/// `cpu_critical - ENVELOPE_FULL_MARGIN_C` and stays there until the FRG-E301 latch takes over with
/// 100 %. With `cpu_critical = 90`: nothing up to 78 °C, 37 % at 83 °C, 75 % from 88 °C. With the
/// default `cpu_critical` (or a higher one) every built-in preset curve lies above this line, so it
/// only binds settings that ask for less cooling than that close to the critical temperature.
pub const ENVELOPE_START_MARGIN_C: f32 = 12.0;
pub const ENVELOPE_FULL_MARGIN_C: f32 = 2.0;
pub const ENVELOPE_MAX_DUTY: f32 = 75.0;

// The rules stay ordered whatever the tuning: E305 arms below the E301 latch, which releases below
// its own arm point; calibration stops below the latch.
const _: () =
    assert!(NEAR_CRITICAL_MARGIN_C > 0.0 && CRITICAL_RELEASE_MARGIN_C > 0.0 && CALIBRATION_ABORT_MARGIN_C > 0.0);
const _: () = assert!(
    ENVELOPE_START_MARGIN_C > ENVELOPE_FULL_MARGIN_C && ENVELOPE_FULL_MARGIN_C > 0.0 && ENVELOPE_MAX_DUTY <= 100.0
);

/// The hard floor (duty %) at CPU temperature `cpu_t` (the median the critical latch uses). It
/// depends on nothing but the temperature and `cpu_critical` (which the settings cap at
/// `CPU_CRITICAL_MAX`): not on the profile, the header's role, calibration data or learned models.
/// Unknown temperature: no floor here (the engine hands software duties back to the BIOS then).
pub fn envelope_floor(cpu_t: Option<f32>, cpu_critical: f32) -> f32 {
    let Some(t) = cpu_t.filter(|t| t.is_finite()) else { return 0.0 };
    if !cpu_critical.is_finite() {
        return ENVELOPE_MAX_DUTY;
    }
    let lo = cpu_critical - ENVELOPE_START_MARGIN_C;
    let span = ENVELOPE_START_MARGIN_C - ENVELOPE_FULL_MARGIN_C;
    ((t - lo) / span * ENVELOPE_MAX_DUTY).clamp(0.0, ENVELOPE_MAX_DUTY)
}

/// Temperature at which FRG-E305's near-critical rule arms.
pub fn near_critical_c(cpu_critical: f32) -> f32 {
    cpu_critical - NEAR_CRITICAL_MARGIN_C
}

/// Temperature below which the FRG-E301 latch may release (after [`CRITICAL_RELEASE_HOLD`]).
pub fn critical_release_c(critical: f32) -> f32 {
    critical - CRITICAL_RELEASE_MARGIN_C
}

#[cfg(test)]
mod tests {
    use super::*;

    /// audit FR-CTL-001: the hard floor depends only on the CPU temperature and `cpu_critical`.
    #[test]
    fn envelope_floor_values() {
        assert_eq!(envelope_floor(Some(70.0), 90.0), 0.0);
        assert_eq!(envelope_floor(Some(78.0), 90.0), 0.0);
        assert_eq!(envelope_floor(Some(83.0), 90.0), 37.5);
        assert_eq!(envelope_floor(Some(88.0), 90.0), ENVELOPE_MAX_DUTY);
        assert_eq!(envelope_floor(Some(140.0), 90.0), ENVELOPE_MAX_DUTY);
        assert_eq!(envelope_floor(None, 90.0), 0.0);
        assert_eq!(envelope_floor(Some(f32::NAN), 90.0), 0.0);
        assert_eq!(envelope_floor(Some(50.0), f32::NAN), ENVELOPE_MAX_DUTY);
    }

    /// The documented values (change them deliberately, together with the table above).
    #[test]
    fn documented_values() {
        assert_eq!(near_critical_c(92.0), 92.0 - NEAR_CRITICAL_MARGIN_C);
        assert_eq!(critical_release_c(92.0), 87.0);
        assert_eq!(CRITICAL_RELEASE_HOLD, Duration::from_secs(10));
        assert!(near_critical_c(crate::config::CPU_CRITICAL_MAX) < crate::config::CPU_CRITICAL_MAX);
    }
}
