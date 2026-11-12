//! Workload classes (idle / light / gaming / CPU-heavy / mixed), per-class power statistics for
//! feed-forward forecasting, and the learned "quiet ceiling" histograms.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Workload {
    #[default]
    Idle,
    Light,
    Gaming,
    CpuHeavy,
    Mixed,
}

impl Workload {
    #[allow(dead_code)]
    pub const ALL: [Workload; 5] =
        [Workload::Idle, Workload::Light, Workload::Gaming, Workload::CpuHeavy, Workload::Mixed];
    pub fn index(self) -> usize {
        self as usize
    }
    pub fn label(self) -> &'static str {
        match self {
            Workload::Idle => "Idle",
            Workload::Light => "Light use",
            Workload::Gaming => "Gaming",
            Workload::CpuHeavy => "Rendering / CPU-heavy",
            Workload::Mixed => "Mixed load",
        }
    }
}

/// Per-class power statistics (W).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClassStat {
    pub pc: f64,
    pub pg: f64,
    pub pc90: f64,
    pub pg90: f64,
    /// Seconds observed in this class.
    pub secs: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClassStats {
    pub stats: [ClassStat; 5],
    /// Learned sustained CPU package power maximum (W), 0 = unknown.
    pub ppt: f64,
}

impl Default for ClassStats {
    fn default() -> Self {
        ClassStats { stats: [ClassStat::default(); 5], ppt: 0.0 }
    }
}

/// Seconds in a class before its statistics are used for feed-forward.
pub const CLASS_TRUST_S: f64 = 300.0;
const CHUNK_S: f64 = 10.0;

#[derive(Clone, Debug, Default)]
pub struct Classifier {
    acc_pc: f64,
    acc_pg: f64,
    acc_t: f64,
    chunks: [(f64, f64, f64); 3],
    n_chunks: usize,
    pub class: Workload,
    /// Seconds since the current class was entered.
    pub in_class_s: f64,
}

fn p90_step(q: &mut f64, x: f64, eta: f64) {
    // Streaming quantile: up-steps of 0.9·η, down-steps of 0.1·η settle where P(x > q) = 0.1.
    // (The report's `sign(x−q) + 1 − 2·0.9` would converge to the 10th percentile.)
    if *q == 0.0 {
        *q = x;
    } else {
        *q += eta * if x > *q { 0.9 } else { -0.1 } * 2.0;
    }
}

impl Classifier {
    pub fn classify(pc: f64, pg: f64, ppt: f64, tbp: f64) -> Workload {
        if pg >= 0.4 * tbp {
            Workload::Gaming
        } else if pc >= 0.7 * ppt && pg < 0.3 * tbp {
            Workload::CpuHeavy
        } else if pc < 30.0 && pg < 0.15 * tbp {
            Workload::Idle
        } else if pc < 45.0 && pg < 0.3 * tbp {
            Workload::Light
        } else {
            Workload::Mixed
        }
    }

    /// Accumulates power; every 10 s reclassifies from the 30 s mean and updates class stats.
    pub fn update(&mut self, st: &mut ClassStats, pc: f64, pg: f64, dt: f64, ppt_cfg: f64, tbp: f64) -> Workload {
        self.acc_pc += pc * dt;
        self.acc_pg += pg * dt;
        self.acc_t += dt;
        self.in_class_s += dt;
        if self.acc_t >= CHUNK_S {
            let c = (self.acc_pc / self.acc_t, self.acc_pg / self.acc_t, self.acc_t);
            self.chunks.rotate_left(1);
            self.chunks[2] = c;
            self.n_chunks = (self.n_chunks + 1).min(3);
            self.acc_pc = 0.0;
            self.acc_pg = 0.0;
            self.acc_t = 0.0;
            let used = &self.chunks[3 - self.n_chunks..];
            let tt: f64 = used.iter().map(|c| c.2).sum();
            let mpc = used.iter().map(|c| c.0 * c.2).sum::<f64>() / tt;
            let mpg = used.iter().map(|c| c.1 * c.2).sum::<f64>() / tt;
            // Learned sustained PPT: slowly decaying maximum of the 30 s means.
            if self.n_chunks >= 3 {
                st.ppt = if st.ppt <= 0.0 { mpc } else { (st.ppt - 0.002 * CHUNK_S).max(mpc) };
            }
            let ppt = if st.ppt > 40.0 { st.ppt.min(ppt_cfg.max(40.0)) } else { ppt_cfg };
            let cls = Self::classify(mpc, mpg, ppt, tbp.max(30.0));
            if cls != self.class {
                self.class = cls;
                self.in_class_s = 0.0;
            }
            let s = &mut st.stats[cls.index()];
            let a = (c.2 / 600.0).min(1.0);
            if s.secs <= 0.0 {
                s.pc = c.0;
                s.pg = c.1;
            } else {
                s.pc += (c.0 - s.pc) * a;
                s.pg += (c.1 - s.pg) * a;
            }
            p90_step(&mut s.pc90, c.0, 0.5);
            p90_step(&mut s.pg90, c.1, 0.5);
            s.secs += c.2;
        }
        self.class
    }

    /// Power forecast for each horizon step: the recent level plus the class's typical level
    /// approached with a 60 s time constant (only upward, capped at PPT / TBP).
    #[allow(clippy::too_many_arguments)]
    pub fn forecast(
        &self,
        st: &ClassStats,
        p_slow: f64,
        pg_slow: f64,
        ppt: f64,
        tbp: f64,
        step_s: f64,
        pc_out: &mut [f64],
        pg_out: &mut [f64],
    ) {
        let s = &st.stats[self.class.index()];
        let trusted = s.secs >= CLASS_TRUST_S;
        let (dpc, dpg) = if trusted { ((s.pc - p_slow).max(0.0), (s.pg - pg_slow).max(0.0)) } else { (0.0, 0.0) };
        let pc_cap = ppt.max(p_slow);
        let pg_cap = tbp.max(pg_slow);
        for k in 0..pc_out.len() {
            let f = 1.0 - (-((k + 1) as f64 * step_s) / 60.0).exp();
            pc_out[k] = (p_slow + dpc * f).min(pc_cap);
            pg_out[k] = (pg_slow + dpg * f).min(pg_cap);
        }
    }
}

// ---------------------------------------------------------------------------------------

pub const CEIL_BINS: usize = 20;
const CEIL_TAU_S: f64 = 14.0 * 86400.0;
/// Decayed seconds of history needed before a ceiling is used.
pub const CEIL_MIN_WEIGHT: f64 = 1800.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hist {
    pub counts: [f32; CEIL_BINS],
    pub t_last: f64,
}

/// Applied-airflow histograms per (class × 3-hour bucket × fan group): 5 × 8 × 2 × 20 counters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ceiling {
    pub hists: Vec<Hist>,
}

impl Default for Ceiling {
    fn default() -> Self {
        Ceiling { hists: vec![Hist::default(); 5 * 8 * 2] }
    }
}

impl Ceiling {
    fn idx(class: Workload, hour: u8, group: usize) -> usize {
        (class.index() * 8 + (hour as usize % 24) / 3) * 2 + group.min(1)
    }

    fn decay(h: &Hist, now: f64) -> f64 {
        if h.t_last <= 0.0 || now <= h.t_last { 1.0 } else { (-(now - h.t_last) / CEIL_TAU_S).exp() }
    }

    pub fn add(&mut self, class: Workload, hour: u8, group: usize, u: f64, w: f64, now: f64) {
        if self.hists.len() != 80 {
            self.hists = vec![Hist::default(); 80];
        }
        if !u.is_finite() || !w.is_finite() {
            return;
        }
        let h = &mut self.hists[Self::idx(class, hour, group)];
        let f = Self::decay(h, now) as f32;
        if f < 1.0 {
            for c in &mut h.counts {
                *c *= f;
            }
        }
        h.t_last = now.max(h.t_last);
        let b = ((u.clamp(0.0, 1.0) * CEIL_BINS as f64) as usize).min(CEIL_BINS - 1);
        h.counts[b] += w as f32;
    }

    /// p95 + 0.05 of the airflow usually applied in this situation, once enough history exists.
    pub fn ceiling(&self, class: Workload, hour: u8, group: usize, now: f64) -> Option<f64> {
        let h = self.hists.get(Self::idx(class, hour, group))?;
        let f = Self::decay(h, now);
        let total: f64 = h.counts.iter().map(|&c| c as f64).sum::<f64>() * f;
        if total < CEIL_MIN_WEIGHT {
            return None;
        }
        let mut acc = 0.0;
        for (i, &c) in h.counts.iter().enumerate() {
            acc += c as f64 * f;
            if acc >= 0.95 * total {
                return Some(((i + 1) as f64 / CEIL_BINS as f64 + 0.05).min(1.0));
            }
        }
        Some(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p90_converges() {
        let mut q = 0.0;
        let mut r = crate::hw::sim::Rng::new(7);
        for _ in 0..20000 {
            p90_step(&mut q, r.next_f32() as f64 * 100.0, 0.5);
        }
        assert!((q - 90.0).abs() < 4.0, "{q}");
    }

    #[test]
    fn ceiling_needs_history_and_tracks_p95() {
        let mut c = Ceiling::default();
        assert!(c.ceiling(Workload::Gaming, 21, 1, 1000.0).is_none());
        for k in 0..3600 {
            let u = if k % 20 == 0 { 0.62 } else { 0.48 };
            c.add(Workload::Gaming, 21, 1, u, 1.0, 1000.0 + k as f64);
        }
        let v = c.ceiling(Workload::Gaming, 22, 1, 5000.0).unwrap();
        assert!((0.5..=0.62).contains(&v), "{v}");
        assert!(c.ceiling(Workload::Gaming, 12, 1, 5000.0).is_none(), "other time bucket");
    }

    #[test]
    fn classes() {
        assert_eq!(Classifier::classify(22.0, 12.0, 88.0, 190.0), Workload::Idle);
        assert_eq!(Classifier::classify(55.0, 175.0, 88.0, 190.0), Workload::Gaming);
        assert_eq!(Classifier::classify(87.0, 18.0, 88.0, 190.0), Workload::CpuHeavy);
        assert_eq!(Classifier::classify(35.0, 30.0, 88.0, 190.0), Workload::Light);
    }
}
