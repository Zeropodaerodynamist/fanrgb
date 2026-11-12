//! Header identification wizard: finds out what is connected to each fan
//! header. Header by header it runs the header at full speed, then at a low test duty, while the
//! user watches and listens and every tach is recorded. The user answers what slowed down; the
//! answer becomes the header's role (`Config::fan_roles`, written by the engine).
//!
//! Safety:
//! * One header at a time; every other header stays on its normal control.
//! * Below 60 % for at most [`LOW_MAX_S`] (20 s) per test: any header whose role is not confirmed
//!   may carry the pump. A repeat on such a header first runs [`RECOVER_S`] at 100 %.
//! * Not started, and stopped, while the CPU is above [`MAX_CPU_C`] (70 °C) or its temperature is
//!   unknown; the engine also stops it for the critical latch, a cooling alarm, "Return fans to
//!   BIOS", a sleep gap, a failed write, and refuses calibration meanwhile.
//! * The engine hands the header back to its normal control (the BIOS under the BIOS preset) as
//!   soon as the wizard stops driving it; a crash or hang mid-test is covered by the crash snapshot
//!   and the watchdog like any other manual duty.
//!
//! Pure logic driven by `dt` (seconds): the engine feeds it once per tick.

use crate::config::HeaderRole;

/// Reference phase duty: everything on the header spins up audibly.
pub const FULL_DUTY: f32 = 100.0;
/// Test duty: clearly lower than any BIOS / preset duty at full speed.
pub const TEST_DUTY: f32 = 30.0;
/// Full-speed phase before each test (the last [`REF_S`] are the tach reference).
pub const HIGH_S: f32 = 6.0;
/// Test phase; the header goes back to normal control afterwards (or at once on an answer).
pub const LOW_S: f32 = 12.0;
/// Hard cap below [`PUMP_SAFE_DUTY`] per test (the duty is withdrawn at this point whatever the phase).
pub const LOW_MAX_S: f32 = 20.0;
/// Full speed before another test on a header that may carry the pump.
pub const RECOVER_S: f32 = 20.0;
/// A pump is safe at or above this duty for any length of time.
pub const PUMP_SAFE_DUTY: f32 = 60.0;
/// No identification above this CPU temperature (°C).
pub const MAX_CPU_C: f32 = 70.0;
const REF_S: f32 = 2.0;
const OBS_S: f32 = 4.0;
/// A tach that fell below this fraction of its full-speed reading "slowed down".
const DROP: f32 = 0.8;
const MIN_RPM: f32 = 200.0;
/// Full-speed RPM from which a tach looks like an AIO pump rather than a fan.
const PUMP_RPM: f32 = 2200.0;

/// What the user saw or heard slow down during the test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    RadiatorFans,
    CpuFans,
    CaseFans,
    /// A case-fan hub / splitter (fans of the case).
    CaseHub,
    Pump,
    Nothing,
    NotSure,
}

impl Answer {
    pub const ALL: [Answer; 7] = [
        Answer::RadiatorFans,
        Answer::CpuFans,
        Answer::CaseFans,
        Answer::CaseHub,
        Answer::Pump,
        Answer::Nothing,
        Answer::NotSure,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Answer::RadiatorFans => "Radiator fans",
            Answer::CpuFans => "CPU cooler fans",
            Answer::CaseFans => "Case fans",
            Answer::CaseHub => "Fans on a case-fan hub",
            Answer::Pump => "The AIO pump",
            Answer::Nothing => "Nothing changed",
            Answer::NotSure => "Not sure",
        }
    }

    /// The role the answer assigns.
    pub fn role(self) -> HeaderRole {
        match self {
            Answer::RadiatorFans => HeaderRole::RadiatorFans,
            Answer::CpuFans => HeaderRole::CpuFans,
            Answer::CaseFans | Answer::CaseHub => HeaderRole::CaseFans,
            Answer::Pump => HeaderRole::Pump,
            Answer::Nothing => HeaderRole::Unused,
            Answer::NotSure => HeaderRole::Unknown,
        }
    }

    /// Friendly name beyond the role's default (`None`: the role's default name).
    pub fn name(self) -> Option<&'static str> {
        match self {
            Answer::CaseHub => Some("Case fan hub"),
            _ => None,
        }
    }
}

/// A header the wizard steps through.
#[derive(Clone, Debug, PartialEq)]
pub struct Header {
    /// Fan index (engine / backend order).
    pub index: usize,
    pub id: String,
    /// Silkscreen name ("CPU_FAN").
    pub name: String,
    /// Its role is not confirmed as fans: it may carry the pump (20 s limit, recovery before a repeat).
    pub pump_suspect: bool,
}

/// What the tachs showed during the last test.
#[derive(Clone, Debug, PartialEq)]
pub enum Guess {
    /// The header's own tach followed: fans (or a pump with a tach) are on it.
    OwnSignal { rpm_full: f32, rpm_test: f32, pump_like: bool },
    /// Another header's tach followed: a hub or splitter whose speed signal goes to that header.
    OtherHeader { header: String, rpm_full: f32, rpm_test: f32 },
    /// No speed signal moved: a pump or fans without a tach wire, or nothing at all.
    NoSignal,
}

impl Guess {
    pub fn describe(&self) -> String {
        match self {
            Guess::OwnSignal { rpm_full, rpm_test, pump_like: true } => format!(
                "Its speed signal followed ({rpm_full:.0} → {rpm_test:.0} rpm). That speed is typical of an AIO pump."
            ),
            Guess::OwnSignal { rpm_full, rpm_test, .. } => {
                format!("Its speed signal followed ({rpm_full:.0} → {rpm_test:.0} rpm): fans are connected here.")
            }
            Guess::OtherHeader { header, rpm_full, rpm_test } => format!(
                "{header} slowed down ({rpm_full:.0} → {rpm_test:.0} rpm): this header probably feeds a hub or splitter \
                 whose speed signal goes to {header}."
            ),
            Guess::NoSignal => "No speed signal changed: fans or a pump without a tach wire, or nothing is connected. \
                 Listen and look: what slowed down?"
                .into(),
        }
    }
}

/// Wizard requests (UI → engine → wizard).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum IdentifyCmd {
    Start,
    Answer(Answer),
    /// Run the test on the current header again.
    Repeat,
    /// Keep the current header's role as it is and go on.
    Skip,
    /// Stop (while running) or close the finished wizard.
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    High { left: f32 },
    Low { elapsed: f32 },
    Ask,
    Done,
}

/// Phase as the UI shows it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IdentifyPhase {
    /// The header runs at 100 % (reference).
    #[default]
    FullSpeed,
    /// The header runs at the test duty: "what slowed down?".
    Test,
    /// Back on normal control, waiting for the answer.
    Ask,
    Done,
    /// Stopped early (see `error`).
    Stopped,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IdentifyResult {
    pub fan_id: String,
    pub header: String,
    /// `None`: skipped (role unchanged).
    pub answer: Option<Answer>,
}

/// What the UI renders.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IdentifyView {
    /// 1-based position of the current header, and how many there are.
    pub step: usize,
    pub total: usize,
    pub fan_id: String,
    pub header: String,
    pub phase: IdentifyPhase,
    /// Duty the wizard drives now (`None`: the header is on its normal control).
    pub duty: Option<f32>,
    pub seconds_left: f32,
    pub question: String,
    /// What the tachs suggest (after the test).
    pub guess: Option<String>,
    /// Answers accepted now (test running or waiting for the answer).
    pub can_answer: bool,
    pub results: Vec<IdentifyResult>,
    pub error: Option<String>,
    pub done: bool,
}

/// A role to write into the settings.
#[derive(Clone, Debug, PartialEq)]
pub struct RoleWrite {
    pub fan_id: String,
    pub role: HeaderRole,
    pub name: Option<&'static str>,
}

#[derive(Clone, Debug)]
pub struct Identify {
    headers: Vec<Header>,
    /// Silkscreen names of all fans (for "SYS_FAN1 slowed down").
    names: Vec<String>,
    pos: usize,
    phase: Phase,
    /// Seconds since the current header's last test ended (`None`: not tested yet).
    since_low: Option<f32>,
    /// Per fan: tach samples (s, rpm) of the reference and the test window.
    reference: Vec<Vec<f32>>,
    observed: Vec<Vec<f32>>,
    guess: Option<Guess>,
    results: Vec<IdentifyResult>,
    writes: Vec<RoleWrite>,
    pub error: Option<String>,
}

impl Identify {
    /// `headers`: the headers to test in order; `names`: silkscreen names of every fan index.
    pub fn new(headers: Vec<Header>, names: Vec<String>) -> Self {
        let n = names.len();
        let mut w = Identify {
            headers,
            names,
            pos: 0,
            phase: Phase::High { left: HIGH_S },
            since_low: None,
            reference: vec![Vec::new(); n],
            observed: vec![Vec::new(); n],
            guess: None,
            results: Vec::new(),
            writes: Vec::new(),
            error: None,
        };
        if w.headers.is_empty() {
            w.abort("No fan header to identify: none is connected and controllable.");
        }
        w
    }

    /// A wizard that could not start.
    pub fn refused(why: &str) -> Self {
        let mut w = Identify::new(Vec::new(), Vec::new());
        w.error = Some(why.to_string());
        w
    }

    pub fn done(&self) -> bool {
        self.phase == Phase::Done
    }

    /// Stopped early (`error` says why).
    pub fn stopped(&self) -> bool {
        self.done() && self.error.is_some()
    }

    /// Running (a header may be driven, or the wizard waits for an answer).
    pub fn active(&self) -> bool {
        !self.done()
    }

    pub fn abort(&mut self, why: &str) {
        if !self.done() {
            self.error = Some(why.to_string());
            self.phase = Phase::Done;
        }
    }

    fn current(&self) -> Option<&Header> {
        self.headers.get(self.pos)
    }

    /// The duty the wizard drives fan `fan` at now (`None`: normal control). Never below
    /// [`PUMP_SAFE_DUTY`] for longer than [`LOW_MAX_S`].
    pub fn duty_for(&self, fan: usize) -> Option<f32> {
        self.current().filter(|h| h.index == fan)?;
        match self.phase {
            Phase::High { .. } => Some(FULL_DUTY),
            Phase::Low { elapsed } if elapsed < LOW_MAX_S => Some(TEST_DUTY),
            _ => None,
        }
    }

    /// Seconds until the test duty ends (`None`: not in the test phase). The engine wakes up at this
    /// deadline whatever its poll interval, so the time below [`PUMP_SAFE_DUTY`] never depends on
    /// when the next poll happens to come.
    pub fn low_left(&self) -> Option<f32> {
        match self.phase {
            Phase::Low { elapsed } => Some((LOW_S.min(LOW_MAX_S) - elapsed).max(0.0)),
            _ => None,
        }
    }

    /// Advances by `dt` seconds. `rpm[i]`: fan i's tach reading if it has a speed signal.
    pub fn step(&mut self, dt: f32, rpm: &[Option<f32>]) {
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        if let Some(s) = &mut self.since_low {
            *s += dt;
        }
        match self.phase {
            Phase::High { left } => {
                let left = left - dt;
                if left <= REF_S {
                    record(&mut self.reference, rpm);
                }
                if left <= 0.0 {
                    for o in &mut self.observed {
                        o.clear();
                    }
                    self.phase = Phase::Low { elapsed: 0.0 };
                } else {
                    self.phase = Phase::High { left };
                }
            }
            Phase::Low { elapsed } => {
                // The samples of this tick were taken at the test duty set during the last tick.
                let elapsed = elapsed + dt;
                if elapsed >= LOW_S - OBS_S {
                    record(&mut self.observed, rpm);
                }
                if elapsed >= LOW_S || elapsed >= LOW_MAX_S {
                    self.end_low();
                    self.phase = Phase::Ask;
                } else {
                    self.phase = Phase::Low { elapsed };
                }
            }
            Phase::Ask | Phase::Done => {}
        }
    }

    fn end_low(&mut self) {
        self.since_low = Some(0.0);
        self.guess =
            self.current().map(|h| h.index).map(|own| guess(own, &self.names, &self.reference, &self.observed));
    }

    pub fn command(&mut self, c: IdentifyCmd) {
        if self.done() {
            return;
        }
        match c {
            IdentifyCmd::Start => {}
            IdentifyCmd::Cancel => self.abort("Cancelled."),
            IdentifyCmd::Answer(a) => {
                if matches!(self.phase, Phase::Low { .. }) {
                    self.end_low();
                }
                if !matches!(self.phase, Phase::Ask | Phase::Low { .. }) {
                    return; // the test has not started yet
                }
                if let Some(h) = self.current().cloned() {
                    self.writes.push(RoleWrite { fan_id: h.id.clone(), role: a.role(), name: a.name() });
                    self.results.push(IdentifyResult { fan_id: h.id, header: h.name, answer: Some(a) });
                }
                self.next();
            }
            IdentifyCmd::Skip => {
                if let Some(h) = self.current().cloned() {
                    self.results.push(IdentifyResult { fan_id: h.id, header: h.name, answer: None });
                }
                self.next();
            }
            IdentifyCmd::Repeat => {
                if matches!(self.phase, Phase::Low { .. }) {
                    self.end_low();
                }
                if self.phase == Phase::Ask || self.since_low.is_some() {
                    let suspect = self.current().is_some_and(|h| h.pump_suspect);
                    let recover = if suspect { RECOVER_S - self.since_low.unwrap_or(RECOVER_S) } else { 0.0 };
                    self.reset_samples();
                    self.guess = None;
                    self.phase = Phase::High { left: HIGH_S.max(recover + REF_S) };
                }
            }
        }
    }

    fn reset_samples(&mut self) {
        for v in self.reference.iter_mut().chain(self.observed.iter_mut()) {
            v.clear();
        }
    }

    fn next(&mut self) {
        self.pos += 1;
        self.since_low = None;
        self.guess = None;
        self.reset_samples();
        self.phase = if self.pos >= self.headers.len() { Phase::Done } else { Phase::High { left: HIGH_S } };
    }

    /// Roles answered since the last call (the engine writes them into the settings).
    pub fn take_writes(&mut self) -> Vec<RoleWrite> {
        std::mem::take(&mut self.writes)
    }

    pub fn view(&self) -> IdentifyView {
        let h = self.current();
        let fan = h.map(|h| h.index);
        let (phase, left) = match self.phase {
            _ if self.stopped() => (IdentifyPhase::Stopped, 0.0),
            Phase::High { left } => (IdentifyPhase::FullSpeed, left.max(0.0)),
            Phase::Low { elapsed } => (IdentifyPhase::Test, (LOW_S - elapsed).max(0.0)),
            Phase::Ask => (IdentifyPhase::Ask, 0.0),
            Phase::Done => (IdentifyPhase::Done, 0.0),
        };
        let name = h.map(|h| h.name.clone()).unwrap_or_default();
        let question = match phase {
            IdentifyPhase::FullSpeed => {
                format!("{name} runs at full speed. Watch and listen: it slows down in a moment.")
            }
            IdentifyPhase::Test | IdentifyPhase::Ask => {
                format!("{name} was turned down to {TEST_DUTY:.0} %. What slowed down?")
            }
            IdentifyPhase::Done => "All headers identified.".into(),
            IdentifyPhase::Stopped => "Identification stopped.".into(),
        };
        IdentifyView {
            step: (self.pos + 1).min(self.headers.len()),
            total: self.headers.len(),
            fan_id: h.map(|h| h.id.clone()).unwrap_or_default(),
            header: name,
            phase,
            duty: fan.and_then(|f| self.duty_for(f)),
            seconds_left: left,
            question,
            guess: self.guess.as_ref().map(Guess::describe),
            can_answer: matches!(self.phase, Phase::Low { .. } | Phase::Ask),
            results: self.results.clone(),
            error: self.error.clone(),
            done: self.done(),
        }
    }
}

fn record(into: &mut [Vec<f32>], rpm: &[Option<f32>]) {
    for (v, r) in into.iter_mut().zip(rpm) {
        if let Some(r) = r.filter(|r| r.is_finite() && *r >= 0.0) {
            v.push(r);
        }
    }
}

fn mean(v: &[f32]) -> Option<f32> {
    (!v.is_empty()).then(|| v.iter().sum::<f32>() / v.len() as f32)
}

/// Which tach followed the test on header `own`.
fn guess(own: usize, names: &[String], reference: &[Vec<f32>], observed: &[Vec<f32>]) -> Guess {
    let ratio = |i: usize| -> Option<(f32, f32)> {
        let (r, o) = (mean(&reference[i])?, mean(&observed[i])?);
        (r >= MIN_RPM).then_some((r, o))
    };
    if let Some((r, o)) = ratio(own).filter(|(r, o)| o / r < DROP) {
        return Guess::OwnSignal { rpm_full: r, rpm_test: o, pump_like: r >= PUMP_RPM };
    }
    let other = (0..reference.len())
        .filter(|&i| i != own)
        .filter_map(|i| ratio(i).map(|(r, o)| (i, r, o)))
        .filter(|(_, r, o)| o / r < DROP)
        .min_by(|a, b| (a.2 / a.1).total_cmp(&(b.2 / b.1)));
    match other {
        Some((i, r, o)) => Guess::OtherHeader {
            header: names.get(i).cloned().unwrap_or_else(|| format!("fan {i}")),
            rpm_full: r,
            rpm_test: o,
        },
        None => Guess::NoSignal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers() -> Vec<Header> {
        vec![
            Header { index: 0, id: "mb.pwm0".into(), name: "CPU_FAN".into(), pump_suspect: true },
            Header { index: 2, id: "mb.pwm2".into(), name: "SYS_FAN2".into(), pump_suspect: false },
        ]
    }

    fn names() -> Vec<String> {
        vec!["CPU_FAN".into(), "SYS_FAN1".into(), "SYS_FAN2".into()]
    }

    /// Steps `secs` at `dt` with tach readings from `rpm(duty of fan 0..3)`.
    fn run(
        w: &mut Identify,
        secs: f32,
        dt: f32,
        rpm: &dyn Fn([Option<f32>; 3]) -> [Option<f32>; 3],
    ) -> Vec<[Option<f32>; 3]> {
        let mut duties = Vec::new();
        let mut t = 0.0;
        while t < secs - 1e-3 {
            let d = [w.duty_for(0), w.duty_for(1), w.duty_for(2)];
            duties.push(d);
            w.step(dt, &rpm(d));
            t += dt;
        }
        duties
    }

    fn tach(d: [Option<f32>; 3]) -> [Option<f32>; 3] {
        // Fan 0: no tach. Fan 1: case fans at ~900 rpm normally. Fan 2: fans, 1500 rpm at 100 %.
        [None, Some(900.0), Some(d[2].map(|x| 300.0 + 12.0 * x).unwrap_or(900.0))]
    }

    #[test]
    fn steps_through_headers_and_writes_roles() {
        let mut w = Identify::new(headers(), names());
        assert_eq!(w.duty_for(0), Some(FULL_DUTY));
        assert_eq!(w.duty_for(2), None, "one header at a time");
        assert!(!w.view().can_answer);
        // An answer before the test is ignored.
        w.command(IdentifyCmd::Answer(Answer::Pump));
        assert!(w.take_writes().is_empty());
        run(&mut w, HIGH_S, 1.0, &tach);
        assert_eq!(w.duty_for(0), Some(TEST_DUTY));
        let v = w.view();
        assert!(v.can_answer && v.phase == IdentifyPhase::Test && v.question.contains("What slowed down"), "{v:?}");
        run(&mut w, LOW_S, 1.0, &tach);
        assert_eq!(w.duty_for(0), None, "back on normal control after the test");
        assert_eq!(w.view().phase, IdentifyPhase::Ask);
        assert!(w.view().guess.is_some_and(|g| g.contains("No speed signal")));
        w.command(IdentifyCmd::Answer(Answer::RadiatorFans));
        assert_eq!(
            w.take_writes(),
            vec![RoleWrite { fan_id: "mb.pwm0".into(), role: HeaderRole::RadiatorFans, name: None }]
        );
        // Second header: own tach follows.
        assert_eq!(w.view().step, 2);
        run(&mut w, HIGH_S + LOW_S, 1.0, &tach);
        let g = w.guess.clone().unwrap();
        assert!(matches!(g, Guess::OwnSignal { pump_like: false, .. }), "{g:?}");
        w.command(IdentifyCmd::Answer(Answer::CaseHub));
        assert_eq!(
            w.take_writes()[0],
            RoleWrite { fan_id: "mb.pwm2".into(), role: HeaderRole::CaseFans, name: Some("Case fan hub") }
        );
        assert!(w.done() && !w.stopped());
        let v = w.view();
        assert_eq!(v.phase, IdentifyPhase::Done);
        assert_eq!(v.results.len(), 2);
        // Commands after the end change nothing.
        w.command(IdentifyCmd::Answer(Answer::Pump));
        assert!(w.take_writes().is_empty());
    }

    #[test]
    fn answer_during_the_test_restores_at_once_and_skip_writes_nothing() {
        let mut w = Identify::new(headers(), names());
        run(&mut w, HIGH_S + 3.0, 1.0, &tach);
        assert_eq!(w.duty_for(0), Some(TEST_DUTY));
        w.command(IdentifyCmd::Answer(Answer::Pump));
        assert_eq!(w.duty_for(0), None);
        assert_eq!(w.take_writes()[0].role, HeaderRole::Pump);
        assert_eq!(w.duty_for(2), Some(FULL_DUTY), "next header");
        w.command(IdentifyCmd::Skip);
        assert!(w.take_writes().is_empty());
        assert!(w.done());
        assert_eq!(w.view().results[1].answer, None);
    }

    /// A header that may carry the pump is never below 60 % for more than 20 s, whatever the
    /// timing of answers, repeats and ticks (also with a slow 5 s tick).
    #[test]
    fn pump_suspect_is_never_low_for_more_than_20_s() {
        for dt in [0.25f32, 1.0, 2.5, 5.0] {
            let mut w = Identify::new(headers(), names());
            let mut low_run = 0.0f32;
            let mut low_in_window: std::collections::VecDeque<(f32, bool)> = Default::default();
            let mut t = 0.0f32;
            for k in 0..2000 {
                let d = w.duty_for(0);
                let low = d.is_some_and(|x| x < PUMP_SAFE_DUTY);
                low_run = if low { low_run + dt } else { 0.0 };
                assert!(low_run <= LOW_MAX_S + 1e-3, "dt {dt}: {low_run} s low at tick {k}");
                low_in_window.push_back((t, low));
                while low_in_window.front().is_some_and(|(t0, _)| t - t0 >= LOW_MAX_S + RECOVER_S) {
                    low_in_window.pop_front();
                }
                let total: f32 = low_in_window.iter().filter(|x| x.1).count() as f32 * dt;
                assert!(total <= LOW_MAX_S + dt + 1e-3, "dt {dt}: {total} s low within 40 s");
                w.step(dt, &tach([d, None, None]));
                t += dt;
                // Keep repeating the test on the first header.
                if k % 7 == 3 {
                    w.command(IdentifyCmd::Repeat);
                }
            }
            assert_eq!(w.view().step, 1, "still on the first header");
        }
        // Without answers the duty is withdrawn after the test, even if the wizard is never ticked
        // in between (one huge step).
        let mut w = Identify::new(headers(), names());
        w.step(HIGH_S, &[None; 3]);
        assert_eq!(w.duty_for(0), Some(TEST_DUTY));
        w.step(60.0, &[None; 3]);
        assert_eq!(w.duty_for(0), None);
        // The cap also holds for a phase that somehow outlived it.
        w.phase = Phase::Low { elapsed: LOW_MAX_S };
        assert_eq!(w.duty_for(0), None);
    }

    #[test]
    fn repeat_recovers_a_pump_suspect_first() {
        let mut w = Identify::new(headers(), names());
        run(&mut w, HIGH_S + LOW_S, 1.0, &tach);
        assert_eq!(w.view().phase, IdentifyPhase::Ask);
        w.command(IdentifyCmd::Repeat);
        let v = w.view();
        assert_eq!((v.phase, v.duty), (IdentifyPhase::FullSpeed, Some(FULL_DUTY)));
        assert!(v.seconds_left >= RECOVER_S, "{v:?}");
        // A confirmed fan header repeats right away.
        let mut w = Identify::new(headers()[1..].to_vec(), names());
        run(&mut w, HIGH_S + LOW_S, 1.0, &tach);
        w.command(IdentifyCmd::Repeat);
        assert_eq!(w.view().seconds_left, HIGH_S);
    }

    #[test]
    fn guesses_hub_and_pump_from_the_tachs() {
        // Header 0 drives fans whose tach goes to SYS_FAN1 (hub).
        let hub = |d: [Option<f32>; 3]| [Some(0.0), Some(d[0].map(|x| 300.0 + 12.0 * x).unwrap_or(900.0)), Some(800.0)];
        let mut w = Identify::new(headers(), names());
        run(&mut w, HIGH_S + LOW_S, 1.0, &hub);
        assert!(matches!(&w.guess, Some(Guess::OtherHeader { header, .. }) if header == "SYS_FAN1"), "{:?}", w.guess);
        // A fast tach on the header itself looks like a pump.
        let pump = |d: [Option<f32>; 3]| [Some(d[0].map(|x| 600.0 + 26.0 * x).unwrap_or(2600.0)), None, None];
        let mut w = Identify::new(headers(), names());
        run(&mut w, HIGH_S + LOW_S, 1.0, &pump);
        assert!(matches!(w.guess, Some(Guess::OwnSignal { pump_like: true, .. })), "{:?}", w.guess);
        assert!(w.view().guess.unwrap().contains("pump"));
    }

    #[test]
    fn abort_and_refusal() {
        let mut w = Identify::new(headers(), names());
        run(&mut w, HIGH_S + 2.0, 1.0, &tach);
        w.abort("CPU above 70 °C");
        assert!(w.stopped());
        assert_eq!(w.duty_for(0), None);
        assert_eq!(w.view().phase, IdentifyPhase::Stopped);
        let r = Identify::refused("busy");
        assert!(r.stopped() && r.view().error.as_deref() == Some("busy"));
        assert!(Identify::new(Vec::new(), names()).stopped());
        let mut c = Identify::new(headers(), names());
        c.command(IdentifyCmd::Cancel);
        assert!(c.stopped() && c.take_writes().is_empty());
    }

    #[test]
    fn answers_map_to_roles() {
        assert_eq!(Answer::Nothing.role(), HeaderRole::Unused);
        assert_eq!(Answer::NotSure.role(), HeaderRole::Unknown);
        assert_eq!(Answer::CaseHub.role(), HeaderRole::CaseFans);
        assert!(Answer::ALL.iter().all(|a| !a.label().is_empty()));
    }
}
