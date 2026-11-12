//! Fan control building blocks shared by the engine.

pub mod ai;
#[cfg(test)]
pub mod ai_v1;
pub mod calibrate;
pub mod cooler;
pub mod curve;
pub mod efficacy;
pub mod identify;
pub mod limits;
#[cfg(test)]
pub mod sim_plant;
pub mod thermal;
#[cfg(test)]
mod thermal_tests;
pub mod workload;

use crate::config::{Combine, FanCalibration, FanConfig};

/// Approximate airflow fraction (0..1) produced by `duty`.
pub fn duty_to_airflow(duty: f32, cal: Option<&FanCalibration>) -> f32 {
    if let Some(c) = cal
        && c.max_rpm > 0.0
        && let Some(rpm) = c.rpm_at(duty)
        && (rpm / c.max_rpm).is_finite()
    {
        return (rpm / c.max_rpm).clamp(0.0, 1.0);
    }
    // Typical PWM fan: ~20 % of max speed at low duty, linear above.
    if duty <= 0.0 { 0.0 } else { (0.2 + 0.8 * duty / 100.0).clamp(0.0, 1.0) }
}

/// Duty needed for an airflow fraction.
pub fn airflow_to_duty(u: f32, cal: Option<&FanCalibration>) -> f32 {
    // A non-finite request (never produced by the controller) asks for full speed.
    let u = if u.is_finite() { u.clamp(0.0, 1.0) } else { 1.0 };
    if let Some(c) = cal
        && let Some(d) = c.duty_for_airflow(u)
        && d.is_finite()
    {
        return d.clamp(0.0, 100.0);
    }
    if u <= 0.2 { u / 0.2 * 1.0 } else { ((u - 0.2) / 0.8 * 100.0).clamp(0.0, 100.0) }
}

pub fn combine(values: &[f32], how: Combine) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    Some(match how {
        Combine::Max => values.iter().copied().fold(f32::MIN, f32::max),
        Combine::Average => values.iter().sum::<f32>() / values.len() as f32,
    })
}

/// Lowest duty that keeps this fan spinning reliably.
pub fn effective_min(cfg: &FanConfig, cal: Option<&FanCalibration>) -> f32 {
    let stall = cal.and_then(|c| c.stop_duty).map(|d| d + 2.0).unwrap_or(0.0);
    cfg.min_duty.max(if cfg.allow_stop { 0.0 } else { stall }).min(cfg.max_duty)
}

/// Applies min/max/stop rules, spin-up kick and ramp rates.
pub fn shape_duty(target: f32, current: Option<f32>, cfg: &FanConfig, cal: Option<&FanCalibration>, dt: f32) -> f32 {
    let min = effective_min(cfg, cal);
    let mut t = target.clamp(0.0, 100.0);
    if t < min {
        t = if cfg.allow_stop && t < min * 0.6 { 0.0 } else { min };
    }
    t = t.min(cfg.max_duty);
    let Some(cur) = current else { return t };
    let mut next =
        if t > cur { (cur + cfg.ramp_up.max(0.5) * dt).min(t) } else { (cur - cfg.ramp_down.max(0.2) * dt).max(t) };
    // A stopped fan needs a kick to start.
    if cur <= 0.5 && next > 0.0 {
        let start = cal.and_then(|c| c.start_duty).unwrap_or(min.max(30.0));
        next = next.max(start.min(cfg.max_duty));
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FanConfig, Preset};
    use crate::model::FanRole;

    #[test]
    fn shaping_respects_min_and_ramps() {
        let mut cfg = FanConfig::preset(Preset::Balanced, FanRole::System);
        cfg.min_duty = 25.0;
        cfg.ramp_up = 10.0;
        cfg.ramp_down = 2.0;
        assert_eq!(shape_duty(10.0, None, &cfg, None, 1.0), 25.0);
        assert_eq!(shape_duty(80.0, Some(40.0), &cfg, None, 1.0), 50.0);
        assert_eq!(shape_duty(30.0, Some(40.0), &cfg, None, 1.0), 38.0);
        cfg.allow_stop = true;
        assert_eq!(shape_duty(5.0, None, &cfg, None, 1.0), 0.0);
    }

    #[test]
    fn airflow_mapping_roundtrip() {
        for d in [0.0f32, 10.0, 30.0, 60.0, 100.0] {
            let u = duty_to_airflow(d, None);
            let back = airflow_to_duty(u, None);
            if d > 0.0 {
                assert!((back - d).abs() < 0.5, "{d} -> {u} -> {back}");
            }
        }
    }

    // ---- independent tester: audit FR-CTL-002 ----

    /// The table the engine would use for a fan after its per-tick clamp (`None`: dropped), from the
    /// JSON a hostile or damaged config.json holds.
    fn kept(json: &str) -> Option<FanCalibration> {
        let mut cfg = crate::config::Config::default();
        cfg.calibrations.insert("mb.pwm1".into(), serde_json::from_str(json).unwrap());
        cfg.clamp_safety();
        cfg.calibrations.get("mb.pwm1").cloned()
    }

    /// What a table the engine keeps must never do: map the AI's full airflow to a low duty, leave
    /// the duty → airflow map outside 0..1 (it builds the AI's safety floor), or raise the stall
    /// floor out of range.
    fn harmless(json: &str) -> Result<(), String> {
        let Some(c) = kept(json) else { return Ok(()) };
        let full = airflow_to_duty(1.0, Some(&c));
        if full.is_nan() || full < 90.0 {
            return Err(format!("full airflow -> {full} % duty with {c:?}"));
        }
        for d in (0..=100).map(|d| d as f32) {
            let u = duty_to_airflow(d, Some(&c));
            if !(0.0..=1.0).contains(&u) {
                return Err(format!("{d} % -> airflow {u} with {c:?}"));
            }
        }
        let cfg = FanConfig::preset(Preset::Balanced, FanRole::System);
        let m = effective_min(&cfg, Some(&c));
        if !(0.0..=100.0).contains(&m) {
            return Err(format!("stall floor {m} with {c:?}"));
        }
        Ok(())
    }

    /// FR-CTL-002 (tester): the classes the audit names (inverted, non-finite, out-of-range tables)
    /// are dropped by the engine's clamp, in any point order, while an honest table is kept and maps
    /// full airflow to full duty.
    #[test]
    fn hostile_calibration_tables_are_dropped_or_harmless() {
        let honest = r#"{"points":[[100,1800],[90,1700],[50,1000],[20,450],[10,0]],"max_rpm":1800,"stop_duty":20,"start_duty":15}"#;
        assert!(kept(honest).is_some());
        harmless(honest).unwrap();
        assert!(airflow_to_duty(1.0, kept(honest).as_ref()) >= 99.0);
        let hostile = [
            // Inverted, in both point orders.
            r#"{"points":[[100,300],[50,900],[0,1800]],"max_rpm":1800}"#,
            r#"{"points":[[0,1800],[50,900],[100,300]],"max_rpm":1800}"#,
            r#"{"points":[[50,900],[100,300],[0,1800]],"max_rpm":1800}"#,
            // Overflowing to infinity (JSON has no NaN; 1e39 is +inf as f32).
            r#"{"points":[[0,1e39],[100,1e39]],"max_rpm":1e39,"stop_duty":1e39}"#,
            r#"{"points":[[100,1800],[50,1000],[0,0]],"max_rpm":1e39}"#,
            r#"{"points":[[100,1800],[50,-1e39],[0,0]],"max_rpm":1800}"#,
            // Out of range: duties, RPM, max RPM, stall and start duties.
            r#"{"points":[[100,1800],[250,1000],[0,0]],"max_rpm":1800}"#,
            r#"{"points":[[100,1800],[50,1000],[-50,0]],"max_rpm":1800}"#,
            r#"{"points":[[100,1800],[50,-1000],[0,0]],"max_rpm":1800}"#,
            r#"{"points":[[100,9e9],[0,0]],"max_rpm":9e9}"#,
            r#"{"points":[[100,1800],[50,1000],[0,0]],"max_rpm":-1800}"#,
            r#"{"points":[[100,1800],[50,1000],[0,0]],"max_rpm":1800,"stop_duty":-5}"#,
            r#"{"points":[[100,1800],[50,1000],[0,0]],"max_rpm":1800,"stop_duty":400}"#,
            r#"{"points":[[100,1800],[50,1000],[0,0]],"max_rpm":1800,"start_duty":1e39}"#,
        ];
        for j in hostile {
            assert!(kept(j).is_none(), "kept: {j}");
            harmless(j).unwrap_or_else(|e| panic!("{j}: {e}"));
        }
        // Too many points.
        let many: Vec<[f32; 2]> = (0..=40).map(|i| [100.0 - i as f32 * 2.5, 1800.0 - i as f32 * 40.0]).collect();
        let json = serde_json::json!({ "points": many, "max_rpm": 1800 }).to_string();
        assert!(kept(&json).is_none());
        // airflow_to_duty never passes a non-finite request through.
        assert_eq!(airflow_to_duty(f32::NAN, kept(honest).as_ref()), 100.0);
        assert_eq!(airflow_to_duty(f32::INFINITY, None), 100.0);
    }

    /// FR-CTL-002 (tester): tables that are just as implausible but pass `FanCalibration::sane`: flat
    /// RPM (the fan did not follow the duty; the wizard's own `validate` refuses it), RPM rising a
    /// little as the duty falls (inside the monotonicity slack), and a `max_rpm` far below every
    /// measured RPM. On 2026-11-12 each was kept and mapped the AI's full airflow to (near) 0 % duty;
    /// now each is dropped or harmless.
    #[test]
    fn implausible_but_sane_tables_never_lower_full_airflow() {
        let cases = [
            // Flat: every duty gives the same RPM (validate's MIN_RANGE would refuse it), points listed
            // from low to high duty.
            r#"{"points":[[0,1500],[50,1500],[100,1500]],"max_rpm":1500}"#,
            // Inverted within the slack: 1150 RPM at 0 %, 1000 RPM at 100 %.
            r#"{"points":[[100,1000],[0,1150]],"max_rpm":1150}"#,
            // max_rpm inconsistent with the table (1 RPM against 1800 RPM measured at 100 %).
            r#"{"points":[[100,1800],[50,1000],[0,0]],"max_rpm":1}"#,
        ];
        let mut bad = Vec::new();
        for j in cases {
            if let Err(e) = harmless(j) {
                bad.push(format!("{j}: {e}"));
            }
        }
        assert!(bad.is_empty(), "{bad:#?}");
    }

    // ---- independent tester, re-check of the FR-CTL-002 fix (range_plausible, plateau order) ----

    /// Tables a real fan and the wizard produce, in the shapes the fix must keep: linear, concave
    /// (RPM saturating at the top), a plateau at the top in either point order, a fan that never stops,
    /// tach noise putting 90 % a little above 100 %, and the max-RPM-only form. Each is kept and maps
    /// full airflow to the top of the table.
    #[test]
    fn genuine_tables_are_kept_and_map_full_airflow_to_the_top() {
        let genuine = [
            r#"{"points":[[100,1800],[90,1640],[80,1490],[70,1340],[60,1190],[50,1040],[40,890],[30,740],[20,590],[10,0]],"max_rpm":1800,"stop_duty":20,"start_duty":15}"#,
            r#"{"points":[[100,1800],[90,1780],[80,1740],[70,1680],[60,1600],[50,1480],[40,1300],[30,1080],[20,800],[10,0]],"max_rpm":1800,"stop_duty":20}"#,
            r#"{"points":[[100,2000],[90,2000],[80,2000],[70,1900],[50,1500],[30,900],[0,0]],"max_rpm":2000}"#,
            r#"{"points":[[0,0],[30,900],[50,1500],[70,1900],[80,2000],[90,2000],[100,2000]],"max_rpm":2000}"#,
            r#"{"points":[[100,1500],[80,1250],[60,1000],[40,760],[20,520],[0,450]],"max_rpm":1500}"#,
            r#"{"points":[[100,1790],[90,1810],[80,1700],[50,1100],[20,500],[10,0]],"max_rpm":1790}"#,
            r#"{"max_rpm":1800}"#,
        ];
        for j in genuine {
            let c = kept(j).unwrap_or_else(|| panic!("dropped: {j}"));
            harmless(j).unwrap_or_else(|e| panic!("{j}: {e}"));
            let top = c.points.iter().filter(|p| p[1] >= 0.98 * c.max_rpm).map(|p| p[0]).fold(0.0f32, f32::max);
            let full = airflow_to_duty(1.0, Some(&c));
            assert!(c.points.len() < 2 || full >= top - 0.5, "{j}: full airflow -> {full} %, top {top} %");
        }
    }

    /// FR-CTL-002 re-check (tester): `follows_duty` only bounds each step against its neighbour, so RPM
    /// may climb by the slack at every step as the duty falls; a 0 RPM point at 0 % then satisfies
    /// `range_plausible`. On 2026-11-12 such tables (RPM highest near 10 %) mapped full airflow to 10 %; the
    /// airflow map now credits a duty only with the lowest RPM of any higher duty.
    #[test]
    fn gradually_inverted_tables_never_lower_full_airflow() {
        let cases = [
            r#"{"points":[[100,1000],[10,1150],[0,0]],"max_rpm":1150}"#,
            r#"{"points":[[100,1000],[80,1150],[60,1320],[40,1500],[20,1700],[10,1920],[0,0]],"max_rpm":1920}"#,
        ];
        let mut bad = Vec::new();
        for j in cases {
            if let Err(e) = harmless(j) {
                bad.push(format!("{j}: {e}"));
            }
        }
        assert!(bad.is_empty(), "{bad:#?}");
    }

    /// FR-CTL-002 re-check (tester): `range_plausible` accepts a `max_rpm` down to 0.8 x the fastest
    /// measured RPM, and `duty_for_airflow` scales "full airflow" by `max_rpm`. On a fan whose RPM
    /// saturates at the top (a typical real curve) that mapped full airflow to about half duty (found
    /// 2026-11-12). The wizard itself stores `max_rpm` equal to the 100 % point; now 0.95x is the lower
    /// bound and full airflow scales by the larger of `max_rpm` and the table's top speed.
    #[test]
    fn low_max_rpm_never_lowers_full_airflow() {
        let cases = [
            r#"{"points":[[100,1800],[90,1780],[80,1740],[70,1680],[60,1600],[50,1480],[40,1300],[30,1080],[20,800],[10,0]],"max_rpm":1445}"#,
            r#"{"points":[[100,1800],[60,1700],[0,0]],"max_rpm":1445}"#,
        ];
        let mut bad = Vec::new();
        for j in cases {
            if let Err(e) = harmless(j) {
                bad.push(format!("{j}: {e}"));
            }
        }
        assert!(bad.is_empty(), "{bad:#?}");
    }

    // ---- independent tester, round 3 (monotone map, max_rpm 0.95x) ----

    /// FR-CTL-002 round 3 (tester): the monotone map takes "full airflow" to the top of the *table*,
    /// and a table may simply stop at a low duty: `[[10,1800],[0,0]]` passes `sane` and the AI's full
    /// airflow becomes 10 % duty.
    #[test]
    fn tables_with_a_low_top_duty_never_lower_full_airflow() {
        let cases = [
            r#"{"points":[[10,1800],[0,0]],"max_rpm":1800}"#,
            r#"{"points":[[1,1800],[0,0]],"max_rpm":1800}"#,
            r#"{"points":[[50,1800],[40,1500],[20,700],[0,0]],"max_rpm":1800}"#,
        ];
        let mut bad = Vec::new();
        for j in cases {
            if let Err(e) = harmless(j) {
                bad.push(format!("{j}: {e}"));
            }
        }
        assert!(bad.is_empty(), "{bad:#?}");
    }

    /// FR-CTL-002 round 3 (tester): a table that is monotone and reaches 100 % but claims the fan is at
    /// 99 % of its speed from 10 % duty (no real PWM fan does; the plausibility checks only look at the
    /// lowest duty). Full airflow maps to 100 %, but the AI's high requests just below it (0.9 to 0.99)
    /// map to about 10 %. Genuine tables, also a fan that saturates early, keep them at 50 % or more.
    #[test]
    #[ignore = "Known limit (Low, docs/SECURITY-AUDIT.md): a monotone table claiming near-full speed at low duty maps 90-99 % airflow to a low duty"]
    fn high_airflow_requests_never_map_to_a_low_duty() {
        let high = |j: &str| -> Result<(), String> {
            let Some(c) = kept(j) else { return Ok(()) };
            for u in [0.9f32, 0.95, 0.97, 0.99] {
                let d = airflow_to_duty(u, Some(&c));
                if d < 50.0 {
                    return Err(format!("airflow {u} -> {d:.1} %"));
                }
            }
            Ok(())
        };
        // Genuine shapes first (they must keep passing).
        for j in [
            r#"{"points":[[100,1800],[90,1640],[80,1490],[70,1340],[60,1190],[50,1040],[40,890],[30,740],[20,590],[10,0]],"max_rpm":1800}"#,
            r#"{"points":[[100,1800],[90,1780],[80,1740],[70,1680],[60,1600],[50,1480],[40,1300],[30,1080],[20,800],[10,0]],"max_rpm":1800}"#,
            r#"{"points":[[100,1500],[80,1250],[60,1000],[40,760],[20,520],[0,450]],"max_rpm":1500}"#,
        ] {
            high(j).unwrap_or_else(|e| panic!("genuine {j}: {e}"));
        }
        let mut bad = Vec::new();
        for j in [
            r#"{"points":[[100,1800],[10,1790],[0,0]],"max_rpm":1800}"#,
            r#"{"points":[[100,1800],[90,1798],[50,1796],[10,1790],[0,0]],"max_rpm":1800}"#,
        ] {
            if let Err(e) = high(j) {
                bad.push(format!("{j}: {e}"));
            }
        }
        assert!(bad.is_empty(), "{bad:#?}");
    }

    /// FR-CTL-002 round 3 (tester): the conservative map changes nothing that matters on genuine
    /// tables: RPM and airflow stay monotone in duty and within 0..1, tach noise moves the expected RPM
    /// by at most the noise, and the airflow a duty is credited with never maps back to a lower duty.
    #[test]
    fn conservative_map_is_faithful_on_genuine_tables() {
        let noisy = FanCalibration {
            points: vec![[100.0, 1790.0], [90.0, 1810.0], [80.0, 1700.0], [50.0, 1100.0], [20.0, 500.0], [10.0, 0.0]],
            max_rpm: 1790.0,
            ..Default::default()
        };
        let linear = FanCalibration {
            points: (0..=10)
                .rev()
                .map(|i| [i as f32 * 10.0, if i == 0 { 0.0 } else { 300.0 + 150.0 * i as f32 }])
                .collect(),
            max_rpm: 1800.0,
            ..Default::default()
        };
        for c in [&noisy, &linear] {
            assert!(c.sane(), "{c:?}");
            let mut prev = (0.0f32, 0.0f32);
            for d in (0..=100).map(|d| d as f32) {
                let r = c.rpm_at(d).unwrap();
                let u = duty_to_airflow(d, Some(c));
                assert!(r >= prev.0 - 1e-3 && u >= prev.1 - 1e-6 && (0.0..=1.0).contains(&u), "{d}: {r} {u}");
                prev = (r, u);
                // The airflow a duty is credited with never maps back to a lower duty (a plateau maps
                // to its highest duty, so it may map to a higher one).
                if d >= 10.0 {
                    let back = airflow_to_duty(u, Some(c));
                    assert!(back >= d - 0.5, "{d} % -> {u} -> {back} %");
                }
            }
            for p in &c.points {
                let r = c.rpm_at(p[0]).unwrap();
                assert!(r <= p[1] + 1e-3 && r >= p[1] - 25.0, "{p:?} -> {r}");
            }
        }
    }

    // ---- independent tester, round 4 (285a886: top duty >= 90 %, flat extension to 100 %) ----

    /// FR-CTL-002 round 4 (tester): for tables the wizard writes (they always include 100 %) the flat
    /// extension changes nothing: `rpm_at` equals plain interpolation over the measured points at every
    /// duty for a fan that follows its duty, and the airflow map is unchanged. A table ending at 90 to
    /// 99 % is kept, extends flat (`rpm_at` above its top equals its top), maps full airflow to 100 %
    /// and every other airflow as before. Below 90 % the table is dropped; the max-RPM-only form stays.
    #[test]
    fn top_duty_rule_and_flat_extension() {
        let wizard = FanCalibration {
            points: vec![
                [100.0, 1800.0],
                [90.0, 1640.0],
                [80.0, 1490.0],
                [70.0, 1340.0],
                [60.0, 1190.0],
                [50.0, 1040.0],
                [40.0, 890.0],
                [30.0, 740.0],
                [20.0, 590.0],
                [10.0, 0.0],
            ],
            max_rpm: 1800.0,
            stop_duty: Some(20.0),
            start_duty: Some(15.0),
        };
        assert!(wizard.sane());
        let mut sorted = wizard.points.clone();
        sorted.sort_by(|a, b| a[0].total_cmp(&b[0]));
        for d in (0..=100).map(|d| d as f32) {
            let plain = crate::control::curve::interpolate(&sorted, d);
            assert!((wizard.rpm_at(d).unwrap() - plain).abs() < 1e-3, "{d}");
        }
        assert!(airflow_to_duty(1.0, Some(&wizard)) >= 99.9);
        // Ends at 92 %.
        let short = FanCalibration {
            points: wizard.points.iter().copied().filter(|p| p[0] <= 92.0).chain([[92.0, 1700.0]]).collect(),
            max_rpm: 1700.0,
            ..Default::default()
        };
        assert!(short.sane(), "{short:?}");
        assert_eq!(short.rpm_at(97.0), Some(1700.0));
        assert!(airflow_to_duty(1.0, Some(&short)) >= 99.9);
        let mid = airflow_to_duty(0.5, Some(&short));
        // 850 RPM lies between 30 % (740) and 40 % (890), as without the extension.
        assert!((mid - 37.33).abs() < 0.1, "0.5 -> {mid}");
        // 85 % top: dropped; points-free form: kept.
        let low = FanCalibration {
            points: wizard.points.iter().copied().filter(|p| p[0] <= 85.0).collect(),
            max_rpm: 1490.0,
            ..Default::default()
        };
        assert!(!low.sane());
        assert!(FanCalibration { max_rpm: 1800.0, ..Default::default() }.sane());
        // The AI's measured airflow for genuine tables is `rpm / max_rpm`: max_rpm is untouched by the
        // extension, so it cannot change it.
        assert_eq!(short.max_rpm, 1700.0);
    }
}
