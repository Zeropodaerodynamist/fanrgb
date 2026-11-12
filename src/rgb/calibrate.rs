//! "Match colours" calibration: test patterns on the real LEDs and the guided white-balance
//! wizard.
//!
//! Split of responsibilities:
//! * [`TestPattern`] is what the lighting thread shows while a test runs
//!   ([`super::RgbCommand::TestColor`] / [`super::RgbCommand::EndTest`]): reference colours on
//!   chosen zones or segments, and the corrections under edit. It never touches the config, so
//!   ending the test (cancel, closing the screen, the `until` lease running out, app exit or
//!   sleep) brings back exactly the saved lighting.
//! * [`CalSession`] is the wizard state machine. It is plain data owned by the UI (sliders edit it
//!   directly, no round trip through the lighting thread), produces a [`TestPattern`] whenever it
//!   changes ([`CalSession::rev`]), and writes the result into the config only on
//!   [`CalSession::commit`].
//!
//! Nothing here writes the controller's calibration (`0x33` pin-define table, `0x5E` flash): the
//! table format is only partly known and a bad write is a brick risk. White balance
//! lives in software.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::config::{
    ColorCorrection, GAIN_RANGE, GAMMA_RANGE, MAX_SEGMENTS, MAX_ZONE_LEDS, RgbConfig, Segment, ZoneConfig,
    sanitize_segments,
};

/// A test pattern expires this long after the last user action (60 s). The UI
/// renews it by sending the pattern again.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// The lighting thread never keeps a test longer than this, whatever `until` says.
pub const MAX_TEST: Duration = Duration::from_secs(120);

/// First white-balance nudge; halved each time the user's answer reverses direction.
pub const FIRST_STEP: f32 = 0.08;
/// Smallest nudge (the search stops refining here).
pub const MIN_STEP: f32 = 0.01;
/// The wizard never pushes a gain below this (the sliders still allow `GAIN_RANGE`): a white that
/// needs less than 25 % of a channel points at a wrong colour order or a broken LED, not at
/// white balance.
pub const MIN_WB_GAIN: f32 = 0.25;
/// Gamma nudge for "too bright / too dark" on mid-grey; halved on reversal.
pub const GAMMA_STEP: f32 = 0.2;
pub const MIN_GAMMA_STEP: f32 = 0.05;

/// What one zone (or one segment of it) shows during a test.
#[derive(Clone, Debug, PartialEq)]
pub struct TestZone {
    pub zone: String,
    /// `None` = every LED of the zone, `Some(i)` = only `segments[i]` of the table the pattern was
    /// made from (the session's edit, as the UI numbers it).
    pub segment: Option<usize>,
    /// LEDs `from..to` of that segment, resolved when the pattern is made ([`CalSession::pattern`],
    /// else [`TestPattern::sanitized`] from the pattern's own corrections). The lighting thread
    /// lights segments by this range, never by index: it keeps the segment table sorted and
    /// cleaned, so `segments[i]` there can be another device. A segment entry
    /// without a range lights nothing.
    pub leds: Option<(u16, u16)>,
    /// Requested screen colour (sRGB). It goes through the zone's pipeline (correction under
    /// edit, segment gain, master brightness) like any effect colour.
    pub rgb: [u8; 3],
}

/// Correction of one zone while it is being edited.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ZoneCal {
    pub correction: ColorCorrection,
    pub segments: Vec<Segment>,
}

impl ZoneCal {
    pub fn of(z: &ZoneConfig) -> Self {
        ZoneCal { correction: z.correction, segments: z.segments.clone() }
    }
}

/// What the lighting thread shows while a calibration test runs.
#[derive(Clone, Debug, PartialEq)]
pub struct TestPattern {
    /// Zones / segments lit with a reference colour. LEDs of a listed zone that no entry covers
    /// are dark. Empty = no reference colours: the user's own lighting runs with `corrections`
    /// (preview of an edit on the real effect).
    pub zones: Vec<TestZone>,
    /// Corrections to use instead of the saved ones (zone id → correction under edit).
    pub corrections: BTreeMap<String, ZoneCal>,
    /// Zones not in `zones` go dark (true) or keep their normal lighting (false). Ignored when
    /// `zones` is empty.
    pub dark_others: bool,
    /// The test ends by itself at this time (lease; capped at [`MAX_TEST`] from receipt).
    pub until: Instant,
}

impl TestPattern {
    /// A plain test colour on one zone, other zones dark.
    #[allow(dead_code)] // UI API
    pub fn solid(zone: &str, rgb: [u8; 3], until: Instant) -> Self {
        TestPattern {
            zones: vec![TestZone { zone: zone.into(), segment: None, leds: None, rgb }],
            corrections: BTreeMap::new(),
            dark_others: true,
            until,
        }
    }

    /// Shows reference colours (not only a correction preview).
    pub fn has_colours(&self) -> bool {
        !self.zones.is_empty()
    }

    /// The correction a zone uses during the test.
    pub fn zone_cal<'a>(&'a self, zone: &str, z: &'a ZoneConfig) -> (ColorCorrection, &'a [Segment]) {
        match self.corrections.get(zone) {
            Some(c) => (c.correction, &c.segments),
            None => (z.correction, &z.segments),
        }
    }

    /// Requested colour of LED `led` of `zone`, or `None` when the test does not light it. The
    /// caller only asks for LEDs the header actually has (its configured LED count), so a segment
    /// reaching past them lights only the LEDs that exist.
    pub fn colour_at(&self, zone: &str, led: usize) -> Option<[u8; 3]> {
        // A segment entry wins over a whole-zone entry, so "zone white, pump segment red" works.
        let mut whole = None;
        for t in self.zones.iter().filter(|t| t.zone == zone) {
            match (t.segment, t.leds) {
                (Some(_), Some((from, to))) => {
                    if (from as usize..to as usize).contains(&led) {
                        return Some(t.rgb);
                    }
                }
                (Some(_), None) => {}
                (None, _) => whole = whole.or(Some(t.rgb)),
            }
        }
        whole
    }

    pub fn lights_zone(&self, zone: &str) -> bool {
        self.zones.iter().any(|t| t.zone == zone)
    }

    /// Sanitised copy as the lighting thread keeps it: `until` capped, segment entries resolved to
    /// LED ranges (from the table as sent, *before* it is cleaned and re-sorted), segment tables
    /// cleaned.
    pub fn sanitized(mut self, now: Instant) -> Self {
        self.until = self.until.min(now + MAX_TEST);
        self.zones.truncate(16);
        for t in &mut self.zones {
            if let Some(i) = t.segment {
                let r = t.leds.or_else(|| {
                    let s = self.corrections.get(&t.zone)?.segments.get(i)?;
                    Some((s.from_led, s.to_led))
                });
                t.leds = r.map(|(a, b)| (a.min(MAX_ZONE_LEDS), b.min(MAX_ZONE_LEDS))).filter(|(a, b)| a < b);
            }
        }
        for c in self.corrections.values_mut() {
            c.correction.sanitize();
            sanitize_segments(&mut c.segments, MAX_ZONE_LEDS);
        }
        self
    }
}

/// Reference colours (sRGB) for the wizard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Swatch {
    /// sRGB white = D65 white point on a calibrated screen.
    White,
    /// 50 % sRGB grey (128): about 22 % light, judges the transfer exponent and low-level tint.
    MidGrey,
    Red,
    Green,
    Blue,
    /// X-Rite ColorChecker "light skin" (194, 150, 130): a mixed colour that shows hue errors.
    Skin,
    #[allow(dead_code)] // UI API: a colour picked by the user
    Custom([u8; 3]),
}

impl Swatch {
    /// The guided order: white balance, mid-tone, then the check colours.
    pub const GUIDED: [Swatch; 6] =
        [Swatch::White, Swatch::MidGrey, Swatch::Red, Swatch::Green, Swatch::Blue, Swatch::Skin];

    pub fn rgb(self) -> [u8; 3] {
        match self {
            Swatch::White => [255, 255, 255],
            Swatch::MidGrey => [128, 128, 128],
            Swatch::Red => [255, 0, 0],
            Swatch::Green => [0, 255, 0],
            Swatch::Blue => [0, 0, 255],
            Swatch::Skin => [194, 150, 130],
            Swatch::Custom(c) => c,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Swatch::White => "White",
            Swatch::MidGrey => "Mid grey",
            Swatch::Red => "Red",
            Swatch::Green => "Green",
            Swatch::Blue => "Blue",
            Swatch::Skin => "Skin tone",
            Swatch::Custom(_) => "Custom",
        }
    }
}

/// Which LEDs a gain belongs to: a whole zone (its `ColorCorrection::gain`) or one segment of it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Target {
    pub zone: String,
    pub segment: Option<usize>,
}

impl Target {
    pub fn zone(zone: &str) -> Self {
        Target { zone: zone.into(), segment: None }
    }

    pub fn segment(zone: &str, i: usize) -> Self {
        Target { zone: zone.into(), segment: Some(i) }
    }
}

/// The user's answer to "how does it look?".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feedback {
    TooRed,
    TooGreen,
    TooBlue,
    /// Red + green too strong (warm / yellow white).
    TooYellow,
    /// Green + blue too strong.
    TooCyan,
    /// Red + blue too strong (magenta).
    TooPink,
    /// Mid grey looks brighter than the screen: raise the exponent.
    TooBright,
    /// Mid grey looks darker than the screen: lower the exponent.
    TooDark,
    /// "Looks white" / "They match": the current subject is done for this swatch.
    LooksRight,
}

impl Feedback {
    /// Channels this answer lowers (hue answers only).
    fn channels(self) -> &'static [usize] {
        match self {
            Feedback::TooRed => &[0],
            Feedback::TooGreen => &[1],
            Feedback::TooBlue => &[2],
            Feedback::TooYellow => &[0, 1],
            Feedback::TooCyan => &[1, 2],
            Feedback::TooPink => &[0, 2],
            _ => &[],
        }
    }

    /// The answer that means "you went too far".
    fn opposite(self) -> Option<Feedback> {
        Some(match self {
            Feedback::TooRed => Feedback::TooCyan,
            Feedback::TooCyan => Feedback::TooRed,
            Feedback::TooGreen => Feedback::TooPink,
            Feedback::TooPink => Feedback::TooGreen,
            Feedback::TooBlue => Feedback::TooYellow,
            Feedback::TooYellow => Feedback::TooBlue,
            Feedback::TooBright => Feedback::TooDark,
            Feedback::TooDark => Feedback::TooBright,
            Feedback::LooksRight => return None,
        })
    }

    fn is_gamma(self) -> bool {
        matches!(self, Feedback::TooBright | Feedback::TooDark)
    }
}

/// What a feedback click did.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FeedbackResult {
    /// Gains (or gamma) changed; `step` is the nudge used (for "fine-tuning" hints).
    Adjusted { gains: [f32; 3], gamma: f32, step: f32 },
    /// Nothing can move further in that direction (gain floor or gamma range); suggest the sliders
    /// or the colour-order check.
    AtLimit,
    /// "Looks right": this subject is confirmed for the current swatch.
    Confirmed,
}

/// One step of the guided flow.
#[derive(Clone, Debug, PartialEq)]
pub enum CalStep {
    /// White on `target`, hue answers adjust its gains.
    WhiteBalance { target: Target },
    /// Mid grey on `target`, "too bright / too dark" adjusts the zone's exponent.
    MidTone { target: Target },
    /// White on both; answers describe `subject` compared with `reference` and adjust the
    /// subject's gains (pump head vs. fans).
    Match { reference: Target, subject: Target },
    /// Red, green, blue and skin tone in turn on every target, for a final look.
    Check { targets: Vec<Target> },
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Search {
    step: f32,
    gamma_step: f32,
    last: Option<Feedback>,
    clicks: u32,
}

impl Default for Search {
    fn default() -> Self {
        Search { step: FIRST_STEP, gamma_step: GAMMA_STEP, last: None, clicks: 0 }
    }
}

#[derive(Clone, Debug)]
struct Snapshot {
    edit: BTreeMap<String, ZoneCal>,
    search: Search,
}

/// The "Match colours" wizard. Owned by the UI; see the module docs for the flow.
#[derive(Clone, Debug)]
pub struct CalSession {
    original: BTreeMap<String, ZoneCal>,
    edit: BTreeMap<String, ZoneCal>,
    steps: Vec<CalStep>,
    step: usize,
    swatch: Swatch,
    /// LEDs lit with the swatch.
    shown: Vec<Target>,
    /// Whose gains the answers change.
    subject: Target,
    search: Search,
    undo: Vec<Snapshot>,
    confirmed: Vec<(Target, Swatch)>,
    dark_others: bool,
    /// Show the user's own lighting with the edited corrections instead of a swatch.
    preview_effect: bool,
    last_activity: Instant,
    rev: u64,
}

/// Index in a cleaned table (`sanitize_segments`) of the segment that `raw` (an entry of the table as
/// typed) became: the same LED range after clamping. `None` if cleaning dropped it.
fn segment_index(cleaned: &[Segment], raw: &Segment) -> Option<usize> {
    let (from, to) = (raw.from_led.min(MAX_ZONE_LEDS), raw.to_led.min(MAX_ZONE_LEDS));
    cleaned.iter().position(|s| s.from_led == from && s.to_led == to)
}

fn clamp_gain(g: f32) -> f32 {
    if g.is_finite() { g.clamp(*GAIN_RANGE.start(), *GAIN_RANGE.end()) } else { 1.0 }
}

// UI API: the redesign's calibration screen uses the full set; the
// interim panel on the Lighting page only part of it.
#[allow(dead_code)]
impl CalSession {
    /// Starts a session for `targets` (the first is the reference: usually the case fans; later
    /// ones are matched against it, e.g. the pump head as a segment of the same header or as its
    /// own zone). Corrections are copied from `cfg`; nothing is shown until the UI sends
    /// [`CalSession::pattern`].
    ///
    /// The copied segment tables are cleaned exactly as the lighting thread and [`CalSession::commit`]
    /// clean them (sorted, overlaps dropped), and segment targets are re-numbered to match by LED
    /// range, so the LEDs a step lights, the gains it edits and what is saved are the same device
    /// even when the table was typed out of order. A segment that cleaning drops
    /// (empty, or overlapping an earlier one) is left out of the session. Use
    /// [`CalSession::segment`] for a target's name.
    pub fn start(cfg: &RgbConfig, targets: &[Target], now: Instant) -> Self {
        let mut original: BTreeMap<String, ZoneCal> = BTreeMap::new();
        for t in targets {
            let z = cfg.zones.get(&t.zone).cloned().unwrap_or_default();
            original.entry(t.zone.clone()).or_insert_with(|| {
                let mut c = ZoneCal::of(&z);
                sanitize_segments(&mut c.segments, MAX_ZONE_LEDS);
                c
            });
        }
        let mut remapped: Vec<Target> = Vec::new();
        for t in targets {
            let t = match t.segment {
                None => t.clone(),
                Some(i) => {
                    let raw = cfg.zones.get(&t.zone).and_then(|z| z.segments.get(i));
                    let cleaned = original.get(&t.zone).map(|c| c.segments.as_slice()).unwrap_or(&[]);
                    match raw.and_then(|r| segment_index(cleaned, r)) {
                        Some(j) => Target::segment(&t.zone, j),
                        None => {
                            log::info!(
                                "colour calibration: segment {} of {} is empty or overlaps another; skipped",
                                i + 1,
                                t.zone
                            );
                            continue;
                        }
                    }
                }
            };
            if !remapped.contains(&t) {
                remapped.push(t);
            }
        }
        if remapped.is_empty() {
            // Every segment target was dropped: balance the whole zone instead.
            if let Some(t) = targets.first() {
                remapped.push(Target::zone(&t.zone));
            }
        }
        let targets = remapped.as_slice();
        let mut steps = Vec::new();
        if let Some(reference) = targets.first() {
            steps.push(CalStep::WhiteBalance { target: Target::zone(&reference.zone) });
            steps.push(CalStep::MidTone { target: Target::zone(&reference.zone) });
            for t in &targets[1..] {
                steps.push(CalStep::Match { reference: reference.clone(), subject: t.clone() });
            }
            steps.push(CalStep::Check { targets: targets.to_vec() });
        }
        let subject = targets.first().cloned().unwrap_or_else(|| Target::zone("dled1"));
        let mut s = CalSession {
            edit: original.clone(),
            original,
            steps,
            step: 0,
            swatch: Swatch::White,
            shown: vec![Target::zone(&subject.zone)],
            subject,
            search: Search::default(),
            undo: Vec::new(),
            confirmed: Vec::new(),
            dark_others: true,
            preview_effect: false,
            last_activity: now,
            rev: 1,
        };
        s.enter_step(now);
        s
    }

    /// The segment a target names in the session's (cleaned) table; `None` for a whole zone.
    pub fn segment(&self, t: &Target) -> Option<&Segment> {
        self.edit.get(&t.zone)?.segments.get(t.segment?)
    }

    fn touch(&mut self, now: Instant) {
        self.last_activity = now;
        self.rev = self.rev.wrapping_add(1);
    }

    fn cal_mut(&mut self, zone: &str) -> &mut ZoneCal {
        self.edit.entry(zone.to_string()).or_default()
    }

    fn enter_step(&mut self, now: Instant) {
        self.search = Search::default();
        self.preview_effect = false;
        match self.steps.get(self.step).cloned() {
            Some(CalStep::WhiteBalance { target }) => {
                self.swatch = Swatch::White;
                self.shown = vec![target.clone()];
                self.subject = target;
            }
            Some(CalStep::MidTone { target }) => {
                self.swatch = Swatch::MidGrey;
                self.shown = vec![target.clone()];
                self.subject = target;
            }
            Some(CalStep::Match { reference, subject }) => {
                self.swatch = Swatch::White;
                self.shown = vec![reference, subject.clone()];
                self.subject = subject;
            }
            Some(CalStep::Check { targets }) => {
                self.swatch = Swatch::Red;
                self.shown = targets;
            }
            None => {}
        }
        self.touch(now);
    }

    // -- read access for the UI ---------------------------------------------------------------

    /// Bumped on every change: send [`CalSession::pattern`] again when it differs from the last
    /// one sent.
    pub fn rev(&self) -> u64 {
        self.rev
    }

    pub fn steps(&self) -> &[CalStep] {
        &self.steps
    }

    pub fn step_index(&self) -> usize {
        self.step
    }

    pub fn step(&self) -> Option<&CalStep> {
        self.steps.get(self.step)
    }

    pub fn swatch(&self) -> Swatch {
        self.swatch
    }

    pub fn subject(&self) -> &Target {
        &self.subject
    }

    pub fn shown(&self) -> &[Target] {
        &self.shown
    }

    /// Current nudge size (shrinks as the answers converge).
    pub fn step_size(&self) -> f32 {
        self.search.step
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn is_confirmed(&self, t: &Target, s: Swatch) -> bool {
        self.confirmed.iter().any(|(ct, cs)| ct == t && *cs == s)
    }

    /// The correction under edit for `zone`.
    pub fn zone(&self, zone: &str) -> Option<&ZoneCal> {
        self.edit.get(zone)
    }

    /// Gains of a target (zone gain, or the segment's own gain).
    pub fn gains(&self, t: &Target) -> [f32; 3] {
        let Some(c) = self.edit.get(&t.zone) else { return [1.0; 3] };
        match t.segment {
            None => c.correction.gain,
            Some(i) => c.segments.get(i).map(|s| s.gain).unwrap_or([1.0; 3]),
        }
    }

    /// Net gain a target's LEDs get (zone × segment), for the transfer chart.
    pub fn effective_gains(&self, t: &Target) -> [f32; 3] {
        let z = self.gains(&Target::zone(&t.zone));
        match t.segment {
            None => z,
            Some(_) => {
                let s = self.gains(t);
                [z[0] * s[0], z[1] * s[1], z[2] * s[2]]
            }
        }
    }

    /// Something differs from the saved config.
    pub fn is_dirty(&self) -> bool {
        self.edit != self.original
    }

    /// No user action for [`IDLE_TIMEOUT`]: the lighting thread has already ended the test (the
    /// UI shows "paused"; any action shows it again).
    pub fn idle(&self, now: Instant) -> bool {
        now.duration_since(self.last_activity) >= IDLE_TIMEOUT
    }

    /// What the LEDs should show now. Send it with `RgbCommand::TestColor` whenever
    /// [`CalSession::rev`] changed.
    pub fn pattern(&self) -> TestPattern {
        let rgb = self.swatch.rgb();
        let zones = if self.preview_effect {
            Vec::new()
        } else {
            self.shown
                .iter()
                .map(|t| TestZone {
                    zone: t.zone.clone(),
                    segment: t.segment,
                    leds: self.segment(t).map(|s| (s.from_led, s.to_led)),
                    rgb,
                })
                .collect()
        };
        TestPattern {
            zones,
            corrections: self.edit.clone(),
            dark_others: self.dark_others,
            until: self.last_activity + IDLE_TIMEOUT,
        }
    }

    // -- navigation -----------------------------------------------------------------------------

    pub fn next_step(&mut self, now: Instant) -> bool {
        if self.step + 1 >= self.steps.len() {
            return false;
        }
        self.step += 1;
        self.enter_step(now);
        true
    }

    pub fn prev_step(&mut self, now: Instant) -> bool {
        if self.step == 0 {
            return false;
        }
        self.step -= 1;
        self.enter_step(now);
        true
    }

    pub fn go_to_step(&mut self, i: usize, now: Instant) {
        if i < self.steps.len() {
            self.step = i;
            self.enter_step(now);
        }
    }

    /// Shows `swatch` on `targets` (free choice outside the guided steps; the subject is kept).
    pub fn show(&mut self, swatch: Swatch, targets: Vec<Target>, now: Instant) {
        self.swatch = swatch;
        if !targets.is_empty() {
            self.shown = targets;
        }
        self.preview_effect = false;
        self.search = Search::default();
        self.touch(now);
    }

    /// Selects the swatch only (same targets).
    pub fn set_swatch(&mut self, swatch: Swatch, now: Instant) {
        self.swatch = swatch;
        self.preview_effect = false;
        self.search = Search::default();
        self.touch(now);
    }

    /// Whose gains the answers and [`CalSession::set_gains`] change.
    pub fn set_subject(&mut self, t: Target, now: Instant) {
        if self.subject != t {
            self.subject = t;
            self.search = Search::default();
        }
        self.touch(now);
    }

    /// Compare `subject` against `reference`: both white, answers adjust the subject.
    pub fn compare(&mut self, reference: Target, subject: Target, now: Instant) {
        self.swatch = Swatch::White;
        self.shown = vec![reference, subject.clone()];
        self.subject = subject;
        self.preview_effect = false;
        self.search = Search::default();
        self.touch(now);
    }

    /// Other zones dark (default) or on their normal effect during the test.
    pub fn set_dark_others(&mut self, on: bool, now: Instant) {
        self.dark_others = on;
        self.touch(now);
    }

    /// Show the user's own effect with the corrections under edit (true) or the swatch (false).
    pub fn set_preview_effect(&mut self, on: bool, now: Instant) {
        self.preview_effect = on;
        self.touch(now);
    }

    pub fn preview_effect(&self) -> bool {
        self.preview_effect
    }

    /// Re-arms the idle lease without changing anything (e.g. the user is reading the screen).
    pub fn keep_alive(&mut self, now: Instant) {
        self.touch(now);
    }

    // -- edits ------------------------------------------------------------------------------------

    fn push_undo(&mut self) {
        self.undo.push(Snapshot { edit: self.edit.clone(), search: self.search });
        if self.undo.len() > 64 {
            self.undo.remove(0);
        }
    }

    pub fn undo(&mut self, now: Instant) -> bool {
        let Some(s) = self.undo.pop() else { return false };
        self.edit = s.edit;
        self.search = s.search;
        self.touch(now);
        true
    }

    fn write_gains(&mut self, t: &Target, g: [f32; 3]) -> bool {
        let g = g.map(clamp_gain);
        let c = self.cal_mut(&t.zone);
        match t.segment {
            None => c.correction.gain = g,
            Some(i) => match c.segments.get_mut(i) {
                Some(s) => s.gain = g,
                None => return false,
            },
        }
        true
    }

    /// Slider edit of a target's gains (clamped to `GAIN_RANGE`; not normalised).
    pub fn set_gains(&mut self, t: &Target, gains: [f32; 3], now: Instant) {
        if self.gains(t) == gains.map(clamp_gain) {
            return;
        }
        self.push_undo();
        self.write_gains(t, gains);
        self.touch(now);
    }

    pub fn set_gamma(&mut self, zone: &str, gamma: f32, now: Instant) {
        let g = if gamma.is_finite() { gamma.clamp(*GAMMA_RANGE.start(), *GAMMA_RANGE.end()) } else { 2.2 };
        if self.edit.get(zone).is_some_and(|c| c.correction.gamma == g) {
            return;
        }
        self.push_undo();
        self.cal_mut(zone).correction.gamma = g;
        self.touch(now);
    }

    /// Replaces a zone's segment table (from Zone setup: e.g. "Fans 0..72", "Pump 72..96").
    pub fn set_segments(&mut self, zone: &str, mut segments: Vec<Segment>, now: Instant) {
        sanitize_segments(&mut segments, MAX_ZONE_LEDS);
        segments.truncate(MAX_SEGMENTS);
        self.push_undo();
        self.cal_mut(zone).segments = segments;
        self.touch(now);
    }

    /// Gains back to 1 (and, for a zone, gamma back to 2.2).
    pub fn reset(&mut self, t: &Target, now: Instant) {
        self.push_undo();
        self.write_gains(t, [1.0; 3]);
        if t.segment.is_none() {
            self.cal_mut(&t.zone).correction.gamma = ColorCorrection::default().gamma;
        }
        self.search = Search::default();
        self.touch(now);
    }

    /// "Copy to AIO pump": `to` gets `from`'s gains (and exponent when both are whole zones).
    pub fn copy(&mut self, from: &Target, to: &Target, now: Instant) {
        self.push_undo();
        let g = self.gains(from);
        self.write_gains(to, g);
        if from.segment.is_none() && to.segment.is_none() {
            let gamma = self.edit.get(&from.zone).map(|c| c.correction.gamma).unwrap_or(2.2);
            self.cal_mut(&to.zone).correction.gamma = gamma;
        }
        self.touch(now);
    }

    /// Applies an answer to the current subject with a bounded step search: hue answers lower the
    /// offending channel(s) by the current step (multiplicative), then the gains are normalised so
    /// the strongest channel is 1 (keeps white as bright as possible; lowering two channels is the
    /// same as raising the third). An answer opposite to the previous one means the last step
    /// overshot, so the step halves (8 % → 4 % → 2 % → 1 %). No gain goes below [`MIN_WB_GAIN`].
    /// "Too bright / too dark" do the same for the zone's exponent on mid grey.
    pub fn feedback(&mut self, f: Feedback, now: Instant) -> FeedbackResult {
        if f == Feedback::LooksRight {
            let key = (self.subject.clone(), self.swatch);
            if !self.confirmed.contains(&key) {
                self.confirmed.push(key);
            }
            self.search = Search::default();
            self.touch(now);
            return FeedbackResult::Confirmed;
        }
        let reversed = self.search.last.is_some_and(|l| f.opposite() == Some(l));
        let mut search = self.search;
        if f.is_gamma() {
            if reversed {
                search.gamma_step = (search.gamma_step / 2.0).max(MIN_GAMMA_STEP);
            }
            let zone = self.subject.zone.clone();
            let cur = self.edit.get(&zone).map(|c| c.correction.gamma).unwrap_or(2.2);
            let want = if f == Feedback::TooBright { cur + search.gamma_step } else { cur - search.gamma_step };
            let g = want.clamp(*GAMMA_RANGE.start(), *GAMMA_RANGE.end());
            if (g - cur).abs() < 1e-4 {
                return FeedbackResult::AtLimit;
            }
            self.push_undo();
            self.cal_mut(&zone).correction.gamma = g;
            search.last = Some(f);
            search.clicks += 1;
            self.search = search;
            self.touch(now);
            return FeedbackResult::Adjusted {
                gains: self.gains(&self.subject.clone()),
                gamma: g,
                step: search.gamma_step,
            };
        }
        if reversed {
            search.step = (search.step / 2.0).max(MIN_STEP);
        }
        let t = self.subject.clone();
        let cur = self.gains(&t);
        let mut g = cur;
        for &ch in f.channels() {
            g[ch] *= 1.0 - search.step;
        }
        let m = g.iter().cloned().fold(0.0f32, f32::max);
        if m > 0.0 {
            g = g.map(|v| v / m);
        }
        // Bounded: a channel that would drop below the floor stays at the floor; if nothing moves
        // the search is at its limit.
        let lowered: Vec<usize> = f.channels().to_vec();
        if lowered.iter().all(|&ch| cur[ch] <= MIN_WB_GAIN + 1e-4) {
            return FeedbackResult::AtLimit;
        }
        g = g.map(|v| v.clamp(MIN_WB_GAIN, 1.0));
        if g.iter().zip(cur.iter()).all(|(a, b)| (a - b).abs() < 1e-5) {
            return FeedbackResult::AtLimit;
        }
        self.push_undo();
        self.write_gains(&t, g);
        search.last = Some(f);
        search.clicks += 1;
        self.search = search;
        self.touch(now);
        let gamma = self.edit.get(&t.zone).map(|c| c.correction.gamma).unwrap_or(2.2);
        FeedbackResult::Adjusted { gains: self.gains(&t), gamma, step: search.step }
    }

    // -- end ----------------------------------------------------------------------------------------

    /// Writes the edited corrections into `cfg` (only the zones in this session) and logs the chosen
    /// values. The caller commits the config (`Shared::edit_config` / the UI's commit), *then* sends
    /// `RgbCommand::EndTest`, so the LEDs never flash the old correction in between. Returns one
    /// line per zone for a toast or the log.
    pub fn commit(&self, cfg: &mut RgbConfig) -> Vec<String> {
        let mut lines = Vec::new();
        for (zone, c) in &self.edit {
            let z = cfg.zones.entry(zone.clone()).or_default();
            let mut cc = c.correction;
            cc.sanitize();
            // Same bound as the test and config load: a segment past the header's LED
            // count is kept as tested, not cut or dropped; the frame builder lights only LEDs that exist.
            let mut segs = c.segments.clone();
            sanitize_segments(&mut segs, MAX_ZONE_LEDS);
            z.correction = cc;
            z.segments = segs;
            let mut line = format!(
                "{zone}: gamma {:.2}, gain R {:.3} G {:.3} B {:.3}",
                cc.gamma, cc.gain[0], cc.gain[1], cc.gain[2]
            );
            for s in &z.segments {
                line.push_str(&format!(
                    "; segment \"{}\" LEDs {}..{} gain {:.3}/{:.3}/{:.3}",
                    s.name, s.from_led, s.to_led, s.gain[0], s.gain[1], s.gain[2]
                ));
                if s.to_led > z.led_count {
                    line.push_str(&format!(" (past the header's {} LEDs: set the LED count to light it)", z.led_count));
                }
            }
            log::info!("RGB colour calibration saved: {line}");
            lines.push(line);
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_pump_segment() -> RgbConfig {
        let mut cfg = RgbConfig::default();
        let z = ZoneConfig {
            led_count: 96,
            segments: vec![
                Segment { name: "Fans".into(), from_led: 0, to_led: 72, ..Default::default() },
                Segment { name: "Pump".into(), from_led: 72, to_led: 96, ..Default::default() },
            ],
            ..Default::default()
        };
        cfg.zones.insert("dled1".into(), z);
        cfg
    }

    #[test]
    fn guided_flow_and_patterns() {
        let t0 = Instant::now();
        let cfg = cfg_with_pump_segment();
        let fans = Target::segment("dled1", 0);
        let pump = Target::segment("dled1", 1);
        let mut s = CalSession::start(&cfg, &[fans.clone(), pump.clone()], t0);
        assert_eq!(s.steps().len(), 4);
        assert!(matches!(s.step(), Some(CalStep::WhiteBalance { target }) if *target == Target::zone("dled1")));
        let p = s.pattern();
        assert_eq!(p.zones, vec![TestZone { zone: "dled1".into(), segment: None, leds: None, rgb: [255; 3] }]);
        assert!(p.dark_others);
        assert_eq!(p.until, t0 + IDLE_TIMEOUT);
        assert!(!s.is_dirty());
        assert!(s.next_step(t0));
        assert_eq!(s.swatch(), Swatch::MidGrey);
        assert!(s.next_step(t0));
        // Match: both segments white, answers adjust the pump.
        assert_eq!(s.subject(), &pump);
        let p = s.pattern();
        assert_eq!(p.zones.len(), 2);
        assert_eq!(p.colour_at("dled1", 10), Some([255; 3]));
        assert_eq!(p.colour_at("dled1", 80), Some([255; 3]));
        assert!(s.next_step(t0));
        assert!(matches!(s.step(), Some(CalStep::Check { .. })));
        assert!(!s.next_step(t0));
        assert!(s.idle(t0 + IDLE_TIMEOUT));
    }

    /// Bounded step search: 8 % first, halving on each reversal, normalised, floored.
    #[test]
    fn feedback_step_search() {
        let t0 = Instant::now();
        let cfg = RgbConfig::default();
        let z = Target::zone("dled1");
        let mut s = CalSession::start(&cfg, std::slice::from_ref(&z), t0);
        let r = s.feedback(Feedback::TooBlue, t0);
        assert!(matches!(r, FeedbackResult::Adjusted { step, .. } if step == FIRST_STEP));
        let g = s.gains(&z);
        assert_eq!(g[0], 1.0);
        assert!((g[2] - 0.92).abs() < 1e-5, "{g:?}");
        // Overshoot: "too yellow" now halves the step and lowers R and G, renormalised to B.
        let r = s.feedback(Feedback::TooYellow, t0);
        assert!(matches!(r, FeedbackResult::Adjusted { step, .. } if (step - 0.04).abs() < 1e-6), "{r:?}");
        let g = s.gains(&z);
        assert_eq!(g[0], 1.0);
        assert_eq!(g[0], g[1]);
        assert!((g[2] - 0.92 / 0.96).abs() < 1e-4, "{g:?}");
        // Same direction keeps the step.
        let r = s.feedback(Feedback::TooYellow, t0);
        assert!(matches!(r, FeedbackResult::Adjusted { step, .. } if (step - 0.04).abs() < 1e-6));
        let g = s.gains(&z);
        assert!(g[0] == 1.0 && g[2] > 0.99, "blue almost back to full: {g:?}");
        // Many reversals: never below MIN_STEP.
        for i in 0..20 {
            s.feedback(if i % 2 == 0 { Feedback::TooGreen } else { Feedback::TooPink }, t0);
        }
        assert!((s.step_size() - MIN_STEP).abs() < 1e-6);
        // Undo goes back one click.
        let before = s.gains(&z);
        s.feedback(Feedback::TooRed, t0);
        assert!(s.undo(t0));
        assert_eq!(s.gains(&z), before);
        // Floor: keep pushing blue down.
        let mut last = FeedbackResult::Confirmed;
        for _ in 0..200 {
            last = s.feedback(Feedback::TooBlue, t0);
            if last == FeedbackResult::AtLimit {
                break;
            }
        }
        assert_eq!(last, FeedbackResult::AtLimit);
        assert!(s.gains(&z)[2] >= MIN_WB_GAIN - 1e-5, "{:?}", s.gains(&z));
        assert_eq!(s.feedback(Feedback::LooksRight, t0), FeedbackResult::Confirmed);
        assert!(s.is_confirmed(&z, Swatch::White));
    }

    #[test]
    fn mid_grey_answers_move_the_exponent() {
        let t0 = Instant::now();
        let mut s = CalSession::start(&RgbConfig::default(), &[Target::zone("dled2")], t0);
        s.next_step(t0);
        assert_eq!(s.swatch(), Swatch::MidGrey);
        s.feedback(Feedback::TooBright, t0);
        assert!((s.zone("dled2").unwrap().correction.gamma - 2.4).abs() < 1e-5);
        s.feedback(Feedback::TooDark, t0);
        assert!((s.zone("dled2").unwrap().correction.gamma - 2.3).abs() < 1e-5, "reversal halves the step");
        s.set_gamma("dled2", 3.0, t0);
        assert_eq!(s.feedback(Feedback::TooBright, t0), FeedbackResult::AtLimit);
    }

    /// Commit writes only on request; cancel = drop the session (config untouched).
    #[test]
    fn commit_writes_segments_and_gains() {
        let t0 = Instant::now();
        let mut cfg = cfg_with_pump_segment();
        let saved = cfg.clone();
        let pump = Target::segment("dled1", 1);
        let mut s = CalSession::start(&cfg, &[Target::segment("dled1", 0), pump.clone()], t0);
        s.compare(Target::segment("dled1", 0), pump.clone(), t0);
        s.feedback(Feedback::TooBlue, t0);
        s.set_gains(&Target::zone("dled1"), [1.0, 0.8, 0.9], t0);
        assert!(s.is_dirty());
        assert_eq!(cfg, saved, "nothing written before commit");
        let p = s.pattern();
        assert_eq!(p.corrections["dled1"].segments[1].gain[2], 0.92);
        let lines = s.commit(&mut cfg);
        assert_eq!(lines.len(), 1);
        let z = &cfg.zones["dled1"];
        assert_eq!(z.correction.gain, [1.0, 0.8, 0.9]);
        assert_eq!(z.segments[1].gain, [1.0, 1.0, 0.92]);
        assert_eq!(z.segments[0].gain, [1.0; 3]);
        // Copy to another zone.
        let mut s = CalSession::start(&cfg, &[Target::zone("dled1"), Target::zone("dled2")], t0);
        s.copy(&Target::zone("dled1"), &Target::zone("dled2"), t0);
        assert_eq!(s.gains(&Target::zone("dled2")), [1.0, 0.8, 0.9]);
        assert_eq!(s.effective_gains(&pump), [1.0, 0.8, 0.9 * 0.92]);
    }

    #[test]
    fn pattern_lookup_and_sanitising() {
        let now = Instant::now();
        let p = TestPattern {
            zones: vec![
                TestZone { zone: "dled1".into(), segment: None, leds: None, rgb: [1, 1, 1] },
                TestZone { zone: "dled1".into(), segment: Some(0), leds: Some((4, 8)), rgb: [9, 9, 9] },
            ],
            corrections: BTreeMap::new(),
            dark_others: false,
            until: now + Duration::from_secs(3600),
        };
        assert_eq!(p.colour_at("dled1", 0), Some([1, 1, 1]));
        assert_eq!(p.colour_at("dled1", 5), Some([9, 9, 9]), "segment entry wins");
        assert_eq!(p.colour_at("dled2", 5), None);
        assert_eq!(p.sanitized(now).until, now + MAX_TEST);
        let only_seg = TestPattern {
            zones: vec![TestZone { zone: "z".into(), segment: Some(0), leds: Some((4, 8)), rgb: [9; 3] }],
            ..TestPattern::solid("z", [0; 3], now)
        };
        assert_eq!(only_seg.colour_at("z", 0), None, "LEDs outside the segment stay dark");
        let unresolved = TestPattern {
            zones: vec![TestZone { zone: "z".into(), segment: Some(0), leds: None, rgb: [9; 3] }],
            ..TestPattern::solid("z", [0; 3], now)
        };
        assert_eq!(unresolved.sanitized(now).colour_at("z", 5), None, "no range, nothing lit");
    }

    /// Review L23: a table typed out of order (`[Pump 72..96, Fans 0..72]`) is cleaned (sorted) by
    /// the lighting thread. The session numbers targets in the cleaned table, and every pattern
    /// carries LED ranges, so balancing "Pump" lights the pump LEDs and its gains are saved on the
    /// pump segment, never on the fans.
    #[test]
    fn out_of_order_segments_light_and_save_the_right_leds() {
        let t0 = Instant::now();
        let mut cfg = RgbConfig::default();
        cfg.zones.insert(
            "dled1".into(),
            ZoneConfig {
                led_count: 96,
                segments: vec![
                    Segment { name: "Pump".into(), from_led: 72, to_led: 96, ..Default::default() },
                    Segment { name: "Fans".into(), from_led: 0, to_led: 72, ..Default::default() },
                ],
                ..Default::default()
            },
        );
        // As the UI numbers them: fans = 1, pump = 0.
        let mut s = CalSession::start(&cfg, &[Target::segment("dled1", 1), Target::segment("dled1", 0)], t0);
        s.next_step(t0);
        s.next_step(t0);
        let pump = s.subject().clone();
        assert_eq!(s.segment(&pump).unwrap().name, "Pump");
        // What the lighting thread does with the pattern: sanitize (sort), then light by range.
        let mut p = s.pattern().sanitized(t0);
        p.zones.retain(|z| z.segment == pump.segment);
        assert_eq!(p.colour_at("dled1", 80), Some([255; 3]), "pump LEDs lit");
        assert_eq!(p.colour_at("dled1", 10), None, "fans dark");
        // Gains land on the pump segment and are saved there.
        s.feedback(Feedback::TooBlue, t0);
        let pc = &s.pattern().sanitized(t0).corrections["dled1"].segments;
        assert_eq!((pc[1].from_led, pc[1].to_led), (72, 96));
        assert!(pc[1].gain[2] < 1.0 && pc[0].gain == [1.0; 3], "{pc:?}");
        s.commit(&mut cfg);
        let z = &cfg.zones["dled1"];
        let saved_pump = z.segments.iter().find(|x| x.name == "Pump").unwrap();
        let saved_fans = z.segments.iter().find(|x| x.name == "Fans").unwrap();
        assert!(saved_pump.gain[2] < 1.0 && saved_fans.gain == [1.0; 3], "{:?}", z.segments);
        assert_eq!((saved_pump.from_led, saved_pump.to_led), (72, 96));
        // A pattern made elsewhere with only an index resolves against the table it carries, as sent.
        let mut table = ZoneCal::of(&cfg.zones["dled1"]);
        table.segments.reverse();
        let raw = TestPattern {
            zones: vec![TestZone { zone: "dled1".into(), segment: Some(0), leds: None, rgb: [7; 3] }],
            corrections: [("dled1".to_string(), table)].into_iter().collect(),
            dark_others: true,
            until: t0 + IDLE_TIMEOUT,
        };
        let r = raw.sanitized(t0);
        assert_eq!(r.colour_at("dled1", 80), Some([7; 3]), "segments[0] as sent = the pump");
        assert_eq!(r.colour_at("dled1", 10), None);
        // Overlapping segment: dropped from the session (it would be dropped on save too).
        let mut cfg2 = cfg_with_pump_segment();
        cfg2.zones.get_mut("dled1").unwrap().segments[1].from_led = 60;
        let s2 = CalSession::start(&cfg2, &[Target::segment("dled1", 0), Target::segment("dled1", 1)], t0);
        assert!(matches!(s2.steps().last(), Some(CalStep::Check { targets }) if targets.len() == 1));
    }

    /// Review L24: a segment past the header's configured LED count survives Save (same bound as the
    /// test and config load) instead of being cut or dropped.
    #[test]
    fn commit_keeps_segments_past_the_led_count() {
        let t0 = Instant::now();
        let mut cfg = cfg_with_pump_segment();
        cfg.zones.get_mut("dled1").unwrap().led_count = 30;
        let s = CalSession::start(&cfg, &[Target::segment("dled1", 0), Target::segment("dled1", 1)], t0);
        let lines = s.commit(&mut cfg);
        let z = &cfg.zones["dled1"];
        assert_eq!(z.segments.len(), 2);
        assert_eq!((z.segments[0].from_led, z.segments[0].to_led), (0, 72));
        assert_eq!((z.segments[1].from_led, z.segments[1].to_led), (72, 96));
        assert!(lines[0].contains("past the header's 30 LEDs"), "{}", lines[0]);
    }
}
