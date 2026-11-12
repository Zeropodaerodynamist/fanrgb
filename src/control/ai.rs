//! AI fan control manager: owns the learned plant models (versioned, per-hardware keys,
//! persisted to `ai_models.json` in the state folder: `%ProgramData%\FanRGB` when elevated,
//! `%LOCALAPPDATA%\FanRGB` otherwise) and the controller runtime.
//!
//! The controller itself (model, estimator, MPC, supervisor) is in [`crate::control::thermal`];
//! this file adds persistence, key/fingerprint handling and human-readable explanations.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use crate::control::thermal::*;
use crate::control::workload::{Ceiling, ClassStats};
use crate::error::ErrorCode;

/// Version of the `ai_models.json` envelope. v1 (unversioned map of per-zone `ThermalModel`s)
/// cannot be converted: its parameters had a different structure and are archived instead.
pub const MODEL_FILE_VERSION: u32 = 2;
/// Version of one saved model entry inside a v2 file (for future per-entry migrations).
pub const MODEL_SCHEMA: u32 = 1;

/// One estimator block. Values are stored as exact decimal strings (serde_json's float parser
/// can be off by one ULP); plain JSON numbers are accepted when loading.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedRls {
    #[serde(with = "exact")]
    pub theta: Vec<f64>,
    #[serde(with = "exact2")]
    pub p: Vec<Vec<f64>>,
    #[serde(default)]
    pub n: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Num {
    S(String),
    F(f64),
}

impl Num {
    fn of(v: f64) -> Num {
        Num::S(format!("{v:?}"))
    }
    fn get(&self) -> Result<f64, String> {
        match self {
            Num::F(v) => Ok(*v),
            Num::S(s) => s.parse::<f64>().map_err(|e| format!("bad number {s:?}: {e}")),
        }
    }
}

mod exact {
    use super::Num;
    use serde::{Deserialize, Deserializer, Serializer, de::Error, ser::SerializeSeq};
    pub fn serialize<S: Serializer>(v: &[f64], s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(v.len()))?;
        for x in v {
            seq.serialize_element(&Num::of(*x))?;
        }
        seq.end()
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<f64>, D::Error> {
        Vec::<Num>::deserialize(d)?.iter().map(|n| n.get().map_err(D::Error::custom)).collect()
    }
}

mod exact2 {
    use super::Num;
    use serde::{Deserialize, Deserializer, Serializer, de::Error, ser::SerializeSeq};
    pub fn serialize<S: Serializer>(v: &[Vec<f64>], s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(v.len()))?;
        for row in v {
            let r: Vec<Num> = row.iter().map(|x| Num::of(*x)).collect();
            seq.serialize_element(&r)?;
        }
        seq.end()
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Vec<f64>>, D::Error> {
        Vec::<Vec<Num>>::deserialize(d)?
            .iter()
            .map(|r| r.iter().map(|n| n.get().map_err(D::Error::custom)).collect())
            .collect()
    }
}

impl SavedRls {
    fn of<const N: usize>(r: &Rls<N>) -> Self {
        SavedRls { theta: r.theta.to_vec(), p: r.p.iter().map(|row| row.to_vec()).collect(), n: r.n }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedModel {
    pub schema: u32,
    pub fingerprint: Fingerprint,
    pub die: SavedRls,
    pub hs: SavedRls,
    pub case: SavedRls,
    pub gpu: SavedRls,
    pub w_h: f64,
    pub b_g: f64,
    pub n_inf: u32,
    pub val_err: f64,
    pub learn_samples: u64,
    pub learn_s: f64,
    pub distrusted: bool,
    pub classes: ClassStats,
    pub ceiling: Ceiling,
    /// Unix seconds of the last save.
    pub updated: u64,
}

impl SavedModel {
    pub fn from_model(m: &PlantModel) -> Self {
        SavedModel {
            schema: MODEL_SCHEMA,
            fingerprint: m.fingerprint.clone(),
            die: SavedRls::of(&m.die),
            hs: SavedRls::of(&m.hs),
            case: SavedRls::of(&m.case),
            gpu: SavedRls::of(&m.gpu),
            w_h: m.w_h,
            b_g: m.b_g,
            n_inf: m.n_inf,
            val_err: m.val_err,
            learn_samples: m.learn_samples,
            learn_s: m.learn_s,
            distrusted: m.distrusted,
            classes: m.classes.clone(),
            ceiling: m.ceiling.clone(),
            updated: unix_now(),
        }
    }

    pub fn to_model(&self) -> Result<PlantModel, String> {
        if self.schema > MODEL_SCHEMA {
            return Err(format!("schema {} is newer than this app ({MODEL_SCHEMA})", self.schema));
        }
        let mut m = PlantModel::default();
        if !m.die.import(&self.die.theta, &self.die.p, self.die.n)
            || !m.hs.import(&self.hs.theta, &self.hs.p, self.hs.n)
            || !m.case.import(&self.case.theta, &self.case.p, self.case.n)
            || !m.gpu.import(&self.gpu.theta, &self.gpu.p, self.gpu.n)
        {
            return Err("parameter block has the wrong size or non-finite values".into());
        }
        let fin = |v: f64, d: f64| if v.is_finite() { v } else { d };
        m.w_h = fin(self.w_h, 0.0).clamp(-0.02, 0.02);
        m.b_g = fin(self.b_g, 0.0).clamp(-30.0, 30.0);
        m.n_inf = self.n_inf;
        m.val_err = fin(self.val_err, 3.0).clamp(0.0, 50.0);
        m.learn_samples = self.learn_samples;
        m.learn_s = fin(self.learn_s, 0.0).max(0.0);
        m.distrusted = self.distrusted;
        m.classes = self.classes.clone();
        m.ceiling = self.ceiling.clone();
        if m.ceiling.hists.len() != Ceiling::default().hists.len() {
            m.ceiling = Ceiling::default();
        }
        m.fingerprint = self.fingerprint.clone();
        Ok(m)
    }
}

#[derive(Serialize, Deserialize)]
struct ModelFile {
    version: u32,
    models: BTreeMap<String, SavedModel>,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Stable key of a model: physics only (CPU, temperature source, fan sets) — never the AI style,
/// targets or calibration values (those are in the fingerprint).
pub fn model_key(fp: &Fingerprint) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |s: &str| {
        for b in s.bytes().chain(std::iter::once(0xFF)) {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    feed(&fp.cpu);
    feed(&fp.temp_src);
    for k in fp.cpu_fans.keys() {
        feed(k);
    }
    feed("|");
    for k in fp.case_fans.keys() {
        feed(k);
    }
    format!("m{h:016x}")
}

/// A learned parameter with a human name and unit, for the Intelligence page.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AiParam {
    pub name: String,
    pub value: f32,
    pub unit: String,
    /// One standard deviation of the estimate, when known.
    pub sd: Option<f32>,
    pub help: String,
}

fn param(name: &str, value: f64, unit: &str, sd: Option<f64>, help: &str) -> AiParam {
    AiParam { name: name.into(), value: value as f32, unit: unit.into(), sd: sd.map(|v| v as f32), help: help.into() }
}

/// Physical read-outs of a model.
pub fn explain(m: &PlantModel) -> Vec<AiParam> {
    let a = m.hs.theta[0];
    let sd_c = 100.0 * m.hs.sd(0) / (a * a).max(1e-9);
    let gain_cpu = m.hs_rise_per_100w(0.3) / m.hs_rise_per_100w(1.0).max(1e-6);
    let t = &m.case.theta;
    let gain_case = (t[2] + t[3]) / (t[2] + 0.3 * t[3]).max(1e-6);
    vec![
        param(
            "CPU die offset (R_jc)",
            m.r_jc(),
            "°C/W",
            Some(m.die.sd(0) / 100.0),
            "Instant jump of the CPU temperature per watt; why Ryzen temperatures look spiky.",
        ),
        param(
            "CPU rise per 100 W at 30 % airflow",
            m.rise_per_100w(0.3),
            "°C",
            None,
            "Steady die temperature above case air for 100 W of CPU power with the CPU fan at 30 %.",
        ),
        param(
            "CPU rise per 100 W at 100 % airflow",
            m.rise_per_100w(1.0),
            "°C",
            None,
            "Same at full CPU fan speed: the part fans cannot remove.",
        ),
        param(
            "Heatsink resistance at 30 % airflow",
            m.r_hs(0.3),
            "°C/W",
            None,
            "Heatsink-to-case-air resistance with the CPU fan at 30 %.",
        ),
        param(
            "Heatsink resistance at 100 % airflow",
            m.r_hs(1.0),
            "°C/W",
            None,
            "Heatsink-to-case-air resistance at full CPU fan speed.",
        ),
        param(
            "Heatsink heat capacity",
            m.c_h(),
            "J/K",
            Some(sd_c),
            "How much energy the cooler soaks up per degree; spikes shorter than the time constant are absorbed.",
        ),
        param(
            "Heatsink time constant at 30 % airflow",
            m.tau_hs(0.3),
            "s",
            None,
            "Time for the cooler to settle 63 % of the way after a load change, fans at 30 %.",
        ),
        param("Heatsink time constant at 100 % airflow", m.tau_hs(1.0), "s", None, "Same at full CPU fan speed."),
        param(
            "CPU fan airflow gain",
            gain_cpu,
            "×",
            Some(m.hs.sd(2) / m.hs.theta[2].max(1e-6) * (gain_cpu - 1.0)),
            "How much better the cooler works at 100 % than at 30 % CPU fan speed.",
        ),
        param(
            "Case air rise per 100 W at 30 % case airflow",
            25.0 * t[0] / (t[2] + 0.3 * t[3]).max(1e-4),
            "°C",
            None,
            "How much 100 W of CPU heat warms the case air with case fans at 30 %.",
        ),
        param(
            "Case air rise per 100 W at 100 % case airflow",
            m.case_rise_per_100w(1.0),
            "°C",
            None,
            "Same with case fans at full speed.",
        ),
        param(
            "Case fan airflow gain",
            gain_case,
            "×",
            None,
            "How much more heat the case fans remove at 100 % than at 30 %.",
        ),
        param(
            "Estimated room temperature",
            m.t_amb_est(0.5),
            "°C",
            None,
            "Inferred from the case-air model (no room sensor needed).",
        ),
        param(
            "GPU rise over case air per 100 W",
            m.r_g() * 100.0,
            "°C",
            Some(m.gpu.sd(0)),
            "GPU temperature above case air per 100 W of GPU power; case fans help the GPU only through case air.",
        ),
        param("GPU offset", m.b_g, "°C", None, "Slow correction for the GPU's own fan behaviour."),
        param(
            "Heatsink correction (integral action)",
            m.w_h * 60.0,
            "°C/min",
            None,
            "Learned residual that removes steady offsets the model would otherwise leave.",
        ),
    ]
}

/// "4 of 6 load changes predicted within 0.8 °C; fan effect known to ±9 %".
pub fn confidence_why(m: &PlantModel) -> String {
    let [cn, ce, cp] = m.confidence_factors();
    let fan_pct = 100.0 * m.hs.sd(2) / m.hs.theta[2].max(1e-6);
    let mut s = format!(
        "{} of {} load changes predicted (mean error {:.1} °C); CPU fan effect known to ±{:.0} %. Factors: count {:.0} %, error {:.0} %, fan effect {:.0} %.",
        m.n_inf.min(N_INF_FULL),
        N_INF_FULL,
        m.val_err,
        fan_pct,
        cn * 100.0,
        ce * 100.0,
        cp * 100.0
    );
    if m.distrusted {
        s.push_str(" The model recently disagreed with reality, so the safe curve is used until it is re-validated.");
    }
    s
}

/// Learned models plus the controller runtime.
pub struct AiManager {
    pub models: HashMap<String, PlantModel>,
    pub runtime: AiController,
    runtime_key: Option<String>,
    dirty: bool,
    path: PathBuf,
    /// Coded events for the engine (alerts / diagnostics ring); drained by the engine.
    pub notices: Vec<(ErrorCode, String)>,
}

impl Default for AiManager {
    fn default() -> Self {
        AiManager {
            models: HashMap::new(),
            runtime: AiController::default(),
            runtime_key: None,
            dirty: false,
            path: PathBuf::new(),
            notices: Vec::new(),
        }
    }
}

fn archive(path: &Path, suffix: &str) -> PathBuf {
    let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("ai_models");
    let to = path.with_file_name(format!("{name}.{suffix}.json.bak"));
    if let Err(e) = std::fs::rename(path, &to) {
        log::warn!("could not archive {}: {e}", path.display());
    }
    to
}

impl AiManager {
    pub fn load() -> Self {
        Self::load_from(crate::config::ai_models_path())
    }

    pub fn load_from(path: PathBuf) -> Self {
        let mut mgr = AiManager { path: path.clone(), ..Default::default() };
        let text = match crate::util::fsx::read_to_string_limited(&path, 16 << 20) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return mgr,
            Err(e) => {
                mgr.notices.push((ErrorCode::AiModelCorrupt, format!("cannot read {}: {e}", path.display())));
                return mgr;
            }
        };
        let value: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                let to = archive(&path, "bad");
                mgr.notices.push((
                    ErrorCode::AiModelCorrupt,
                    format!("learned models were not valid JSON ({e}); kept as {}", to.display()),
                ));
                return mgr;
            }
        };
        let version = value.get("version").and_then(|v| v.as_u64());
        match version {
            Some(v) if v == MODEL_FILE_VERSION as u64 => {}
            other => {
                let tag = other.map(|v| format!("v{v}")).unwrap_or_else(|| "v1".into());
                let to = archive(&path, &tag);
                mgr.notices.push((
                    ErrorCode::AiModelReset,
                    format!(
                        "learned models in the {tag} format were archived as {} (their parameters do not map onto the new physical model); relearning takes about an hour of normal use",
                        to.display()
                    ),
                ));
                return mgr;
            }
        }
        let Some(models) = value.get("models").and_then(|m| m.as_object()) else {
            let to = archive(&path, "bad");
            mgr.notices.push((
                ErrorCode::AiModelCorrupt,
                format!("learned model file has no model list; kept as {}", to.display()),
            ));
            return mgr;
        };
        let mut bad = Vec::new();
        for (k, v) in models {
            let parsed =
                serde_json::from_value::<SavedModel>(v.clone()).map_err(|e| e.to_string()).and_then(|s| s.to_model());
            match parsed {
                Ok(mut m) => {
                    // Trust is re-earned quickly after a restart (B24).
                    m.n_inf /= 2;
                    mgr.models.insert(k.clone(), m);
                }
                Err(e) => {
                    log::warn!("dropping learned model {k}: {e}");
                    bad.push(format!("{k}: {e}"));
                }
            }
        }
        if !bad.is_empty() {
            mgr.dirty = true;
            mgr.notices.push((
                ErrorCode::AiModelCorrupt,
                format!(
                    "{} damaged model entr{} dropped ({})",
                    bad.len(),
                    if bad.len() == 1 { "y" } else { "ies" },
                    bad.join("; ")
                ),
            ));
        }
        if !mgr.models.is_empty() {
            log::info!("loaded {} learned thermal model(s)", mgr.models.len());
        }
        mgr
    }

    #[allow(dead_code)]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Saves if anything changed (atomic tmp + rename).
    pub fn save(&mut self) -> anyhow::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.save_now()
    }

    pub fn save_now(&mut self) -> anyhow::Result<()> {
        use crate::error::WithCode;
        if self.path.as_os_str().is_empty() {
            self.dirty = false;
            return Ok(());
        }
        let file = ModelFile {
            version: MODEL_FILE_VERSION,
            models: self
                .models
                .iter()
                .filter(|(_, m)| m.is_finite())
                .map(|(k, m)| (k.clone(), SavedModel::from_model(m)))
                .collect(),
        };
        let s = serde_json::to_string(&file).code(ErrorCode::Io)?;
        crate::util::fsx::write_atomic(&self.path, s.as_bytes()).code(ErrorCode::Io)?;
        self.dirty = false;
        Ok(())
    }

    pub fn model(&self, key: &str) -> Option<&PlantModel> {
        self.models.get(key)
    }

    #[allow(dead_code)]
    pub fn active_key(&self) -> Option<&str> {
        self.runtime_key.as_deref()
    }

    /// One control step for the PC-wide model `key`.
    pub fn step(&mut self, key: &str, fp: &Fingerprint, inp: &SystemInputs, tg: &Targets) -> &Decision {
        if self.runtime_key.as_deref() != Some(key) {
            // Keep the supervisors' current outputs so a re-key does not jump the fans.
            let (cu, ku) = (self.runtime.cpu.clone(), self.runtime.case.clone());
            self.runtime = AiController::default();
            self.runtime.cpu = cu;
            self.runtime.case = ku;
            self.runtime_key = Some(key.to_string());
        }
        let m = self
            .models
            .entry(key.to_string())
            .or_insert_with(|| PlantModel { fingerprint: fp.clone(), ..Default::default() });
        if m.fingerprint != *fp {
            if m.apply_fingerprint(fp) {
                self.notices.push((
                    ErrorCode::AiModelReset,
                    "fan hardware or calibration changed: the learned fan effect is being re-validated (safe curve meanwhile)".into(),
                ));
            }
            self.dirty = true;
        }
        self.runtime.step(m, inp, tg);
        if self.runtime.learned {
            self.dirty = true;
        }
        if self.runtime.out.distrust_event {
            self.dirty = true;
            self.notices.push((
                ErrorCode::AiModelDistrust,
                format!(
                    "the learned model mispredicted by {:.1} °C; using the safe curve until it is re-validated",
                    m.last_shadow_err.max(m.val_err)
                ),
            ));
        }
        &self.runtime.out
    }

    /// Forgets one model (`Some(key)`, any key containing the model id) or all of them.
    pub fn reset(&mut self, key: Option<&str>) {
        match key {
            Some(k) => {
                let ids: Vec<String> = self
                    .models
                    .keys()
                    .filter(|m| k == m.as_str() || k.split('|').any(|p| p == m.as_str()))
                    .cloned()
                    .collect();
                for id in &ids {
                    self.models.remove(id);
                }
                if self.runtime_key.as_ref().is_some_and(|r| ids.contains(r)) {
                    self.runtime_key = None;
                }
            }
            None => {
                self.models.clear();
                self.runtime_key = None;
            }
        }
        self.notices.push((ErrorCode::AiModelReset, "learned thermal model reset by the user".into()));
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_ignores_calibration_values() {
        let mut fp =
            Fingerprint { cpu: "AMD Ryzen 5 7500F".into(), temp_src: "Max:cpu.temp".into(), ..Default::default() };
        fp.cpu_fans.insert("mb.pwm0".into(), 1850.0);
        let k1 = model_key(&fp);
        fp.cpu_fans.insert("mb.pwm0".into(), 1200.0);
        assert_eq!(k1, model_key(&fp));
        fp.case_fans.insert("mb.pwm1".into(), 0.0);
        assert_ne!(k1, model_key(&fp));
    }

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fanrgb-ai-tests-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A file from a newer build (v3) is archived, not parsed or overwritten; the next save writes a
    /// fresh v2 file next to the archive.
    #[test]
    fn future_version_is_archived_and_not_overwritten() {
        let dir = temp_dir("future");
        let path = dir.join("ai_models.json");
        let v3 = r#"{"version":3,"models":{"k":{"something":"new"}}}"#;
        std::fs::write(&path, v3).unwrap();
        let mut mgr = AiManager::load_from(path.clone());
        assert!(mgr.models.is_empty());
        let bak = dir.join("ai_models.v3.json.bak");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), v3);
        assert!(mgr.notices.iter().any(|(c, _)| *c == ErrorCode::AiModelReset));
        mgr.reset(None);
        mgr.save_now().unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["version"], MODEL_FILE_VERSION);
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), v3, "archive untouched by the save");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_json_and_missing_model_list_are_archived_as_bad() {
        let dir = temp_dir("bad");
        let path = dir.join("ai_models.json");
        std::fs::write(&path, "{ not json").unwrap();
        let mgr = AiManager::load_from(path.clone());
        assert!(mgr.models.is_empty() && !path.exists());
        assert!(dir.join("ai_models.bad.json.bak").exists());
        assert!(mgr.notices.iter().any(|(c, _)| *c == ErrorCode::AiModelCorrupt));
        std::fs::write(&path, r#"{"version":2}"#).unwrap();
        let mgr = AiManager::load_from(path.clone());
        assert!(mgr.models.is_empty());
        assert!(mgr.notices.iter().any(|(c, _)| *c == ErrorCode::AiModelCorrupt));
        assert_eq!(std::fs::read_to_string(dir.join("ai_models.bad.json.bak")).unwrap(), r#"{"version":2}"#);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v1 -> v2 twice (e.g. an old build ran again in between): the second archive replaces the
    /// first one (documents current behaviour: only the latest v1 file is kept).
    #[test]
    fn repeated_v1_archive_replaces_the_previous_backup() {
        let dir = temp_dir("v1twice");
        let path = dir.join("ai_models.json");
        std::fs::write(&path, r#"{"a":{"theta":[1],"p":[]}}"#).unwrap();
        let _ = AiManager::load_from(path.clone());
        std::fs::write(&path, r#"{"b":{"theta":[2],"p":[]}}"#).unwrap();
        let mgr = AiManager::load_from(path.clone());
        assert!(mgr.models.is_empty());
        let bak = std::fs::read_to_string(dir.join("ai_models.v1.json.bak")).unwrap();
        assert!(bak.contains("\"b\""), "{bak}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Nothing to save -> no file is created (a fresh install doesn't write an empty model file).
    #[test]
    fn clean_manager_does_not_write() {
        let dir = temp_dir("clean");
        let path = dir.join("ai_models.json");
        let mut mgr = AiManager::load_from(path.clone());
        mgr.save().unwrap();
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
