//! Lighting engine: renders effects and drives the Gigabyte RGB Fusion 2 USB controller.
//!
//! Threads: `fanrgb-rgb` renders frames (UI preview + desired controller state) and never
//! touches the device; `fanrgb-rgb-io` ([`link`]) owns the HID handle, runs the watchdog-guarded
//! I/O and the reconnect state machine. A hung or vanished controller therefore never blocks the
//! UI or the preview.

pub mod audio;
pub mod calibrate;
pub mod color;
pub mod dynlight;
pub mod effects;
pub mod fusion2;
pub mod link;
pub mod mock;
pub mod session;
pub mod transport;

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::time::{Duration, Instant, SystemTime};

use calibrate::TestPattern;
use color::{Dither, LedPipeline, Rgb, perceptual, smoothstep};
use effects::{AudioLevels, EffectState, FrameCtx, speed_level};
use fusion2::{ByteOrder, HwEffect, HwMode};
use link::{Link, LinkStatus, Timings, WorkerCmd};
use session::{Desired, HeaderOut, SingleOut};

pub use link::{RgbConnState, RgbErrorInfo};

use crate::config::{ColorCorrection, ColorOrder, Effect, EffectKind, RgbConfig, Segment, ZoneConfig};
use crate::engine::Shared;

#[derive(Clone, Debug, PartialEq)]
pub struct ZoneDesc {
    pub id: String,
    pub name: String,
    pub addressable: bool,
    pub max_leds: u16,
    /// Primary effect division on the controller.
    pub slot: u8,
    /// Every division this zone drives (LED_C: 1 and 4 until the user picks one).
    pub slots: Vec<u8>,
    /// ARGB header index (0 = D_LED1, 1 = D_LED2).
    pub header: Option<u8>,
    /// Only shown when "show all zones" is enabled.
    pub advanced: bool,
}

/// Zones with the default LED_C mapping (both candidate divisions).
pub fn standard_zones() -> Vec<ZoneDesc> {
    zones_for(None)
}

/// Zones of the B650M D3HP AX: D_LED1 = division 5, D_LED2 = division 6, and the
/// 12 V LED_C header on division 1 *or* 4 depending on the board revision (the LED-ID that
/// tells which needs a kernel SMI call, so it isn't read). `ledc_slot` = the user's choice.
pub fn zones_for(ledc_slot: Option<u8>) -> Vec<ZoneDesc> {
    let ledc: Vec<u8> = match ledc_slot {
        Some(s @ (1 | 4)) => vec![s],
        _ => vec![1, 4],
    };
    let single = |id: &str, name: &str, slot: u8| ZoneDesc {
        id: id.into(),
        name: name.into(),
        addressable: false,
        max_leds: 1,
        slot,
        slots: vec![slot],
        header: None,
        advanced: true,
    };
    let argb = |id: &str, name: &str, h: u8| ZoneDesc {
        id: id.into(),
        name: name.into(),
        addressable: true,
        max_leds: crate::config::MAX_ZONE_LEDS,
        slot: fusion2::SLOT_DLED1 + h,
        slots: vec![fusion2::SLOT_DLED1 + h],
        header: Some(h),
        advanced: false,
    };
    let mut v = vec![
        argb("dled1", "D_LED1 · ARGB 5V", 0),
        argb("dled2", "D_LED2 · ARGB 5V", 1),
        ZoneDesc {
            id: "ledc".into(),
            name: "LED_C · RGB 12V".into(),
            addressable: false,
            max_leds: 1,
            slot: ledc[0],
            slots: ledc.clone(),
            header: None,
            advanced: false,
        },
        single("led1", "Division 0 (onboard, if fitted)", 0),
    ];
    if !ledc.contains(&1) {
        v.push(single("led2", "Division 1 (LED_C on other revisions)", 1));
    }
    v.push(single("led3", "LED_CPU (if fitted)", 2));
    v.push(single("led4", "PCH LED (if fitted)", 3));
    if !ledc.contains(&4) {
        v.push(single("led5", "Division 4 (LED_C2 / LED_C on other revisions)", 4));
    }
    v.push(single("led8", "Division 7 (onboard, if fitted)", 7));
    v
}

#[derive(Clone, Debug, Default)]
pub struct RgbDeviceInfo {
    pub name: String,
    pub pid: u16,
    pub fw: String,
    pub chip_id: u32,
    pub calibration: String,
    /// `[CC 47]` keep-settings supported ("Save to controller").
    pub persistence: bool,
    pub support_flags: u8,
    /// Raw `[CC 48]` reply byte (1 = LampArray disabled, 99 = unsupported, else armed). Diagnostic
    /// only: the Lighting page goes by [`RgbStatus::dynamic_lighting`] (registry), see
    /// [`dynlight::FirmwareDl`].
    pub dynamic_lighting_raw: Option<u8>,
}

/// A calibration test that is showing on the LEDs (for "Showing on your LEDs now").
#[derive(Clone, Debug, PartialEq)]
pub struct TestStatus {
    /// Zones lit with reference colours (empty: the user's effect with the corrections under edit).
    pub zones: Vec<String>,
    /// The test ends by itself at this time unless renewed.
    pub until: Instant,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum AudioState {
    #[default]
    Off,
    Active(String),
    Unavailable(String),
}

#[derive(Clone, Debug, Default)]
pub struct RgbStatus {
    /// Set only while connected.
    pub device: Option<RgbDeviceInfo>,
    pub zones: Vec<ZoneDesc>,
    pub preview: HashMap<String, Vec<[u8; 3]>>,
    /// Human-readable error while not connected (`[FRG-E405] ...`), `None` while searching.
    pub error: Option<String>,
    pub audio: AudioState,
    /// Render rate.
    pub fps: f32,
    pub saved_to_flash: bool,

    /// Connection state machine.
    pub state: RgbConnState,
    /// Most recent error (code, detail, time, repeat count); kept after recovery for history.
    pub last_error: Option<RgbErrorInfo>,
    /// When the next connection attempt happens (not connected).
    pub next_retry: Option<Instant>,
    /// Failed attempts since the last successful connection.
    pub attempts: u32,
    /// Successful reconnections after a loss.
    pub reconnects: u32,
    pub write_errors: u64,
    /// Frame rate actually used for the controller (user fps capped by USB throughput).
    pub fps_cap: f32,
    /// Feature reports per changed frame.
    pub packets_per_frame: u32,
    /// Measured time per feature report (ms, 0 = not measured).
    pub packet_ms: f32,
    /// Lighting handed over to the controller for sleep / exit.
    pub suspended: bool,
    /// Divisions LED_C is driven on.
    pub ledc_slots: Vec<u8>,
    /// I/O threads stuck inside a hung HID call (> 0 means the controller hung).
    pub hung_io_threads: usize,
    /// The preview moves (an animated effect or a crossfade): the Lighting page needs frames.
    pub animated: bool,
    /// Simulated controller (`--demo`): the real device is never opened.
    pub simulated: bool,
    /// Bumped on every publish, so the UI deep-copies only new snapshots.
    pub rev: u64,
    /// Windows Dynamic Lighting (registry + firmware byte + contenders), see [`dynlight`].
    pub dynamic_lighting: dynlight::DynamicLightingStatus,
    /// A calibration test pattern is on the LEDs.
    pub test: Option<TestStatus>,
}

impl RgbStatus {
    pub fn summary(&self) -> String {
        match (&self.device, &self.state) {
            (Some(d), _) => format!("{} (USB 048D:{:04X}, firmware {})", d.name, d.pid, d.fw),
            (None, RgbConnState::Searching) => "Searching for the RGB controller…".into(),
            (None, st) => match &self.last_error {
                Some(e) => format!("RGB controller {}: {}", st.label(), e.message()),
                None => format!("RGB controller {}", st.label()),
            },
        }
    }
}

#[derive(Debug)]
pub enum RgbCommand {
    SaveToFlash,
    Reconnect,
    Identify(String),
    /// System suspend / session end: hand the lighting over to the controller (exit lighting +
    /// `[CC 47 01]`) and stop driving it. The sender (if any) is signalled when done.
    Suspend(Option<SyncSender<()>>),
    /// Resume from sleep: `[CC 47 00]`, full re-init, resend everything.
    Resume,
    /// Calibration: show test colours / preview corrections until `EndTest` or the pattern's
    /// `until`. Sending a new pattern replaces the old one and renews the lease.
    TestColor(TestPattern),
    /// End the calibration test: the saved lighting comes back unchanged.
    EndTest,
    /// "Let FanRGB control these LEDs": per-device `AmbientLightingEnabled` = 0 (see [`dynlight`]).
    /// The registry watch picks up any later change by itself.
    ClaimDynamicLighting,
}

/// Sends [`RgbCommand::Suspend`] and waits until the lighting was handed over (or the lighting
/// thread finished its shutdown), at most `timeout`. For the session watcher.
pub fn suspend_blocking(shared: &Shared, timeout: Duration) -> bool {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    shared.send_rgb(RgbCommand::Suspend(Some(tx)));
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(()) => return true,
            Err(RecvTimeoutError::Disconnected) => std::thread::sleep(Duration::from_millis(20)),
            Err(RecvTimeoutError::Timeout) => {}
        }
        if shared.rgb_done.load(Ordering::SeqCst) {
            return true;
        }
    }
    false
}

static PREVIEW_WANTED: AtomicBool = AtomicBool::new(false);

/// The UI shows the live lighting preview (window on screen, Lighting page). Only then does the
/// lighting thread render at full rate for an unconnected or hardware-effect controller.
pub fn set_preview_wanted(on: bool) {
    PREVIEW_WANTED.store(on, Ordering::Relaxed);
}

/// `demo`: drive a simulated IT5702 ([`mock`]) instead of opening the real controller, so demo and
/// screenshot runs never send `init()` (register clears) to the user's hardware.
pub fn spawn_with(shared: Arc<Shared>, rx: Receiver<RgbCommand>, demo: bool) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("fanrgb-rgb".into())
        .spawn(move || {
            let done = shared.clone();
            let factory: link::ConnectorFactory = if demo {
                log::info!("RGB: demo mode, using a simulated controller");
                let dev = mock::MockDev::simulated();
                Arc::new(move || Box::new(mock::MockConnector::new(vec![mock::Plug::Present(dev.clone())])))
            } else {
                Arc::new(|| Box::new(transport::HidApiConnector::default()))
            };
            let link = Link::start(factory, Timings::default());
            let mut engine = RgbEngine::new(shared, link);
            engine.simulated = demo;
            // Demo never touches the registry: a fixed, in-memory Dynamic Lighting state.
            let backend: Box<dyn dynlight::DlBackend> = if demo {
                Box::new(dynlight::FakeBackend::demo())
            } else {
                Box::new(dynlight::RegistryBackend::default())
            };
            engine.dl = Some(dynlight::DlMonitor::start(backend));
            engine.run(rx);
            done.rgb_done.store(true, Ordering::SeqCst);
        })
        .expect("spawn rgb thread")
}

/// The zone settings used for a zone without an entry in the config.
static DEFAULT_ZONE: std::sync::LazyLock<ZoneConfig> = std::sync::LazyLock::new(ZoneConfig::default);
static OFF_EFFECT: std::sync::LazyLock<Effect> =
    std::sync::LazyLock::new(|| Effect { kind: EffectKind::Off, ..Default::default() });

/// Borrowed, not cloned: a software effect renders 40 frames/s even with the UI closed.
fn effect_for<'a>(cfg: &'a RgbConfig, z: &'a ZoneConfig) -> &'a Effect {
    if !cfg.enabled || !z.enabled {
        return &OFF_EFFECT;
    }
    if cfg.sync { &cfg.global } else { &z.effect }
}

/// Firmware mode for a hardware effect. Wave runs on the ARGB headers' own division (mode 6);
/// a single-colour zone can't wave, so it cycles colours instead.
fn hw_mode(kind: EffectKind, addressable: bool) -> Option<HwMode> {
    Some(match kind {
        EffectKind::HwStatic => HwMode::Static,
        EffectKind::HwBreathing => HwMode::Pulse,
        EffectKind::HwFlash => HwMode::Blink,
        EffectKind::HwDoubleFlash => HwMode::DoubleFlash,
        EffectKind::HwColorCycle => HwMode::ColorCycle,
        EffectKind::HwWave if addressable => HwMode::Wave,
        EffectKind::HwWave => HwMode::ColorCycle,
        EffectKind::HwRandom => HwMode::Random,
        _ => return None,
    })
}

/// Firmware MaxBrightness for an effect brightness and the master slider (both perceptual).
pub fn hw_brightness(effect_brightness: f32, master: f32) -> u8 {
    (perceptual(effect_brightness) * perceptual(master) * 255.0).round() as u8
}

/// Effects that light every LED of a zone the same way (can use the whole-board sync path).
fn uniform_kind(k: EffectKind) -> bool {
    matches!(
        k,
        EffectKind::Off
            | EffectKind::Static
            | EffectKind::Breathing
            | EffectKind::Spectrum
            | EffectKind::Strobe
            | EffectKind::Temperature
    )
}

fn zone_len(z: &ZoneDesc, zcfg: &ZoneConfig) -> usize {
    if z.addressable { zcfg.led_count.clamp(1, z.max_leds) as usize } else { 1 }
}

fn user_order(o: ColorOrder) -> Option<ByteOrder> {
    match o {
        ColorOrder::Auto => None,
        o => fusion2::order_from_str(o.label()),
    }
}

/// Crossfade time between effects / colours.
const FADE_S: f32 = 0.3;

#[derive(Default)]
struct ZoneRt {
    state: EffectState,
    sig: Option<(EffectKind, Vec<[u8; 3]>)>,
    fade: Option<(Vec<Rgb>, f32)>,
    last: Vec<Rgb>,
    dither: Dither,
}

impl ZoneRt {
    /// Renders `e` for `n` LEDs, crossfading from the previous output when the effect or its
    /// colours change.
    fn render(&mut self, e: &Effect, ctx: &FrameCtx, n: usize, allow_fade: bool) -> Vec<Rgb> {
        if self.sig.as_ref().is_none_or(|(k, c)| *k != e.kind || *c != e.colors) {
            let kind_changed = self.sig.as_ref().is_none_or(|s| s.0 != e.kind);
            self.fade = (allow_fade && self.sig.is_some() && self.last.len() == n && !e.kind.is_hardware())
                .then(|| (self.last.clone(), 0.0));
            if kind_changed {
                self.state = EffectState::default();
            }
            self.sig = Some((e.kind, e.colors.clone()));
        }
        let mut out = vec![Rgb::BLACK; n];
        effects::render(e, ctx, &mut self.state, &mut out);
        let mut done = false;
        if let Some((from, t)) = self.fade.as_mut() {
            if from.len() != n {
                done = true;
            } else {
                *t += ctx.dt.max(0.0);
                let k = smoothstep(*t / FADE_S);
                for (o, f) in out.iter_mut().zip(from.iter()) {
                    *o = f.lerp(*o, k);
                }
                done = *t >= FADE_S;
            }
        }
        if done {
            self.fade = None;
        }
        self.last.clone_from(&out);
        out
    }

    fn fading(&self) -> bool {
        self.fade.is_some()
    }
}

pub struct FrameInput<'a> {
    pub cfg: &'a RgbConfig,
    pub zones: &'a [ZoneDesc],
    pub dt: f32,
    pub sensors: &'a HashMap<String, f32>,
    pub audio: Option<&'a AudioLevels>,
    /// Overheat alert active; the value is the blink phase (true = red).
    pub alert: Option<bool>,
    pub identify: Option<&'a str>,
    /// Calibration test (reference colours and / or corrections under edit).
    pub test: Option<&'a TestPattern>,
}

pub struct FrameOutput {
    pub previews: HashMap<String, Vec<[u8; 3]>>,
    pub desired: Desired,
    /// Something moves (preview needs frames).
    pub animated: bool,
    /// The controller needs a new frame every tick (moving software output).
    pub streaming: bool,
}

#[derive(Default)]
struct Renderer {
    zones: HashMap<String, ZoneRt>,
    sync: ZoneRt,
}

/// The correction and segment table a zone uses this frame: a running calibration test overrides
/// the saved ones. Segments exist only on addressable headers.
fn zone_cal<'a>(test: Option<&'a TestPattern>, z: &ZoneDesc, zcfg: &'a ZoneConfig) -> (ColorCorrection, &'a [Segment]) {
    let (cc, segs) = match test {
        Some(t) => t.zone_cal(&z.id, zcfg),
        None => (zcfg.correction, zcfg.segments.as_slice()),
    };
    (cc, if z.addressable { segs } else { &[] })
}

/// Device bytes of one zone through its pipeline, temporally dithered when asked (streamed
/// headers only). The flag says the next frame will differ (dither residual), so it must stream.
fn device_bytes(dither: &mut Dither, pipe: &LedPipeline, frame: &[Rgb], dithered: bool) -> (Vec<[u8; 3]>, bool) {
    if dithered {
        dither.quantize(&pipe.drives(frame))
    } else {
        dither.reset();
        (pipe.bytes(frame), false)
    }
}

/// The zone's runtime state, created on first use (no key allocation on every frame).
#[allow(clippy::map_entry)] // `entry` would allocate the key String every frame
fn zone_rt<'a>(zones: &'a mut HashMap<String, ZoneRt>, id: &str) -> &'a mut ZoneRt {
    if !zones.contains_key(id) {
        zones.insert(id.to_string(), ZoneRt::default());
    }
    zones.get_mut(id).expect("inserted above")
}

impl Renderer {
    fn frame(&mut self, inp: &FrameInput) -> FrameOutput {
        let cfg = inp.cfg;
        // Linear master for the device; the preview scales the encoded colour by the slider value
        // (the monitor's own decode makes that perceptual).
        let master = perceptual(cfg.brightness);
        let preview_k = if cfg.brightness.is_finite() { cfg.brightness.clamp(0.0, 1.0) } else { 0.0 };
        let ctx = FrameCtx { dt: inp.dt, sensors: inp.sensors, audio: inp.audio };
        let mut d = Desired::default();
        for z in inp.zones {
            if let Some(h) = z.header {
                let zcfg = cfg.zones.get(&z.id).unwrap_or(&DEFAULT_ZONE);
                d.led_counts[h as usize] = zone_len(z, zcfg) as u16;
                d.order_override[h as usize] = user_order(zcfg.color_order);
            }
        }
        // The sync record carries one colour and the firmware's own byte order for every zone, so it
        // can't honour a colour-order override.
        let any_override = d.order_override.iter().any(|o| o.is_some());
        let visible: Vec<(&ZoneDesc, &ZoneConfig, &Effect)> = inp
            .zones
            .iter()
            .filter(|z| !z.advanced || cfg.show_all_zones)
            .map(|z| {
                let zcfg = cfg.zones.get(&z.id).unwrap_or(&DEFAULT_ZONE);
                (z, zcfg, effect_for(cfg, zcfg))
            })
            .collect();
        let mut previews = HashMap::new();
        let all_same = !visible.is_empty() && visible.iter().all(|v| v.2 == visible[0].2);
        // Reference colours of a calibration test replace the effects (the overheat alert still wins).
        let test_colours = inp.test.filter(|t| t.has_colours());

        // Board-wide hardware wave.
        let whole_wave = inp.alert.is_none()
            && inp.identify.is_none()
            && test_colours.is_none()
            && cfg.enabled
            && cfg.sync
            && all_same
            && cfg.global.kind == EffectKind::HwWave;
        // Whole board shows one colour: GCC's sync path (one record + apply per frame) when the
        // corrected bytes allow it, else the same colour per zone.
        let sync_uniform = inp.alert.is_some()
            || (inp.identify.is_none() && test_colours.is_none() && all_same && uniform_kind(visible[0].2.kind));

        if sync_uniform && !whole_wave {
            let (e, allow_fade) = match inp.alert {
                Some(on) => (
                    Cow::Owned(Effect {
                        kind: EffectKind::Static,
                        colors: vec![if on { [255, 0, 0] } else { [0, 0, 0] }],
                        brightness: 1.0,
                        ..Default::default()
                    }),
                    false,
                ),
                None => (Cow::Borrowed(visible[0].2), true),
            };
            let px = self.sync.render(&e, &ctx, 1, allow_fade);
            // `c` is the screen colour (preview); each zone's LEDs get it through its own pipeline
            // (correction, segment gains, master). The overheat red is a pure primary and bypasses
            // brightness and correction.
            let alert = inp.alert.is_some();
            let c = if alert { px[0].to_u8() } else { px[0].scale(preview_k).to_u8() };
            let mut one: Option<[u8; 3]> = None;
            let mut same = true;
            let mut dither_moving = false;
            for (z, zcfg, _) in &visible {
                let n = zone_len(z, zcfg);
                let dev = if alert {
                    vec![c; n]
                } else {
                    let (cc, segs) = zone_cal(inp.test, z, zcfg);
                    let pipe = LedPipeline::new(cc, segs, master);
                    let rt = zone_rt(&mut self.zones, &z.id);
                    let (b, mv) =
                        device_bytes(&mut rt.dither, &pipe, &vec![px[0]; n], cfg.dither && z.header.is_some());
                    dither_moving |= mv;
                    b
                };
                previews.insert(z.id.clone(), vec![c; n]);
                // A uniform colour with segment gains (pump head vs. fans) is no longer one byte
                // triple: the header streams it per LED, and the sync record is off.
                match session::uniform(&dev) {
                    Some(u) => {
                        same &= one.is_none_or(|o| o == u);
                        one.get_or_insert(u);
                    }
                    None => same = false,
                }
                match z.header {
                    Some(h) => d.headers[h as usize] = HeaderOut::Direct(dev),
                    None => d.singles.extend(z.slots.iter().map(|s| (*s, SingleOut::Soft(dev[0])))),
                }
            }
            // One record for the whole board only when every zone gets the same bytes in the
            // controller's own order and nothing needs per-frame dithering; otherwise the zones are
            // driven one by one (streamed headers), so no correction is bypassed.
            d.sync_all = (same && !any_override && !dither_moving).then(|| one.unwrap_or(c));
            let moving = e.kind.is_animated() || alert || self.sync.fading();
            return FrameOutput { previews, desired: d, animated: moving, streaming: moving || dither_moving };
        }

        let mut animated = false;
        let mut streaming = false;
        for (z, zcfg, base) in &visible {
            let identifying = inp.identify == Some(z.id.as_str());
            let n = zone_len(z, zcfg);
            let (cc, segs) = zone_cal(inp.test, z, zcfg);
            let pipe = LedPipeline::new(cc, segs, master);
            let dithered = cfg.dither && z.header.is_some();
            let rt = zone_rt(&mut self.zones, &z.id);

            // Calibration test: reference colours (dark where the test lights nothing), no effect.
            if let Some(t) = test_colours.filter(|t| !identifying && (t.lights_zone(&z.id) || t.dark_others)) {
                let req: Vec<Rgb> =
                    (0..n).map(|i| t.colour_at(&z.id, i).map(Rgb::from_u8).unwrap_or(Rgb::BLACK)).collect();
                let (dev, mv) = device_bytes(&mut rt.dither, &pipe, &req, dithered);
                streaming |= mv;
                match z.header {
                    Some(h) => d.headers[h as usize] = HeaderOut::Direct(dev),
                    None => d.singles.extend(z.slots.iter().map(|s| (*s, SingleOut::Soft(dev[0])))),
                }
                previews.insert(z.id.clone(), req.iter().map(|c| c.scale(preview_k).to_u8()).collect());
                // Restart the zone's effect cleanly (no crossfade from the test colour) afterwards.
                rt.sig = None;
                rt.last.clear();
                continue;
            }

            let effect = if identifying {
                Cow::Owned(Effect {
                    kind: EffectKind::Strobe,
                    colors: vec![[255, 255, 255]],
                    speed: 0.6,
                    brightness: 1.0,
                    ..Default::default()
                })
            } else {
                Cow::Borrowed(*base)
            };
            let frame = rt.render(&effect, &ctx, n, !identifying);
            let fading = rt.fading();
            // Preview: screen colours. Device: the zone's pipeline.
            let px: Vec<[u8; 3]> = frame.iter().map(|c| c.scale(preview_k).to_u8()).collect();
            animated |= effect.kind.is_animated() || effect.kind.is_hardware() || fading || identifying;
            // A firmware Static can't carry segment gains: stream it instead.
            let segment_static = effect.kind == EffectKind::HwStatic && pipe.has_segment_gains();
            let hw = if identifying || segment_static {
                None
            } else if let Some(m) = hw_mode(effect.kind, z.addressable) {
                // Firmware colour cycle / wave / random make their own colours (not corrected);
                // Pulse / Flash get the zone correction but not segment gains.
                Some(HwEffect::new(
                    m,
                    Rgb::from_u8(effect.color(0)).to_led(&cc).to_u8(),
                    hw_brightness(effect.brightness, cfg.brightness),
                    speed_level(effect.speed),
                ))
            } else if effect.kind == EffectKind::Off && !fading {
                Some(HwEffect::off())
            } else {
                None
            };
            match (z.header, hw) {
                (Some(h), Some(e)) => {
                    rt.dither.reset();
                    d.headers[h as usize] = HeaderOut::Hw(e)
                }
                (Some(h), None) => {
                    let (dev, mv) = device_bytes(&mut rt.dither, &pipe, &frame, dithered);
                    streaming |= effect.kind.is_animated() || fading || identifying || mv;
                    d.headers[h as usize] = HeaderOut::Direct(dev)
                }
                (None, Some(e)) => d.singles.extend(z.slots.iter().map(|s| (*s, SingleOut::Hw(e)))),
                (None, None) => {
                    streaming |= effect.kind.is_animated() || fading || identifying;
                    let dev = pipe.bytes(&frame[..1]);
                    d.singles.extend(z.slots.iter().map(|s| (*s, SingleOut::Soft(dev[0]))))
                }
            }
            previews.insert(z.id.clone(), px);
        }
        if whole_wave {
            let g = &cfg.global;
            d.whole_wave = Some(HwEffect::new(
                HwMode::Wave,
                [0, 0, 0],
                hw_brightness(g.brightness, cfg.brightness),
                speed_level(g.speed),
            ));
        }
        FrameOutput { previews, desired: d, animated, streaming }
    }
}

/// Frame rate the controller can sustain for `d`: ~1.5 ms per feature report, and
/// at most 40 fps when every frame needs an apply (GCC paces those at 25 ms).
pub fn fps_cap(user_fps: u32, d: &Desired, packet_ms: f32) -> f32 {
    let user = user_fps.clamp(5, 60) as f32;
    let per = if packet_ms > 0.0 { packet_ms.max(0.5) } else { 1.5 };
    let packets = d.packets_per_frame() as f32;
    let mut cap = if packets > 0.0 { 1000.0 / (packets * per) } else { user };
    if d.applies_per_frame() {
        cap = cap.min(40.0);
    }
    user.min(cap).max(1.0)
}

/// A wall-clock or monotonic gap this large between frames means the PC slept.
const RESUME_GAP: Duration = Duration::from_secs(5);
/// A suspend that wasn't followed by sleep within this time is cancelled.
const SUSPEND_GIVE_UP: Duration = Duration::from_secs(30);

struct RgbEngine {
    shared: Arc<Shared>,
    link: Link,
    renderer: Renderer,
    audio: audio::AudioCapture,
    identify: Option<(String, Instant)>,
    suspended_at: Option<Instant>,
    frame_times: Vec<f32>,
    /// Last re-init after sleep (the resume command and the frame-gap detector both fire).
    last_resume: Option<Instant>,
    simulated: bool,
    rev: u64,
    /// Running calibration test (never stored in the config; dropping it restores the lighting).
    test: Option<TestPattern>,
    /// Windows Dynamic Lighting monitor (`None` in unit tests).
    dl: Option<dynlight::DlMonitor>,
}

impl RgbEngine {
    fn new(shared: Arc<Shared>, link: Link) -> Self {
        RgbEngine {
            shared,
            link,
            renderer: Renderer::default(),
            audio: audio::AudioCapture::default(),
            identify: None,
            suspended_at: None,
            frame_times: Vec::new(),
            last_resume: None,
            simulated: false,
            rev: 0,
            test: None,
            dl: None,
        }
    }

    fn config(&self) -> RgbConfig {
        self.shared.config.read().map(|c| c.rgb.clone()).unwrap_or_default()
    }

    /// Sensor ids the current lighting reads (temperature/load effects, the overheat alert).
    fn sensor_ids(cfg: &RgbConfig) -> Vec<String> {
        let mut ids = vec![crate::model::ids::CPU_TEMP.to_string()];
        let mut add = |e: &Effect| {
            if matches!(e.kind, EffectKind::Temperature | EffectKind::Load) && !ids.contains(&e.sensor) {
                ids.push(e.sensor.clone());
            }
        };
        add(&cfg.global);
        cfg.zones.values().for_each(|z| add(&z.effect));
        ids
    }

    fn run(mut self, rx: Receiver<RgbCommand>) {
        let mut last_frame = Instant::now();
        let mut last_wall = SystemTime::now();
        let mut last_publish = Instant::now() - Duration::from_secs(1);
        // Per-frame work is kept small: the config, zone list and sensor list are re-read only when
        // the configuration changes (a software effect streams 40 frames/s even with the UI closed).
        let mut cfg_rev = u64::MAX;
        let mut cfg = RgbConfig::default();
        let mut zones = Vec::new();
        let mut sensor_ids: Vec<String> = Vec::new();
        let mut sensors: HashMap<String, f32> = HashMap::new();
        let mut state_tick = u64::MAX;
        loop {
            if self.shared.quit.load(Ordering::SeqCst) {
                break;
            }
            loop {
                match rx.try_recv() {
                    Ok(cmd) => self.command(cmd),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return self.exit(),
                }
            }
            self.link.poll();
            let rev = self.shared.config_rev.load(Ordering::SeqCst);
            if rev != cfg_rev {
                cfg_rev = rev;
                cfg = self.config();
                zones = zones_for(cfg.ledc_slot);
                sensor_ids = Self::sensor_ids(&cfg);
                state_tick = u64::MAX;
            }
            let now = Instant::now();
            let wall = SystemTime::now();
            let gap = now.duration_since(last_frame).max(wall.duration_since(last_wall).unwrap_or_default());
            last_frame = now;
            last_wall = wall;
            if gap > RESUME_GAP {
                log::info!(
                    "RGB: {:.0} s gap between frames (sleep?): re-initialising the controller",
                    gap.as_secs_f32()
                );
                self.resume();
            } else if self.suspended_at.is_some_and(|t| t.elapsed() > SUSPEND_GIVE_UP) {
                log::info!("RGB: suspend was not followed by sleep; resuming");
                self.resume();
            }
            let dt = gap.as_secs_f32().clamp(0.001, 0.5);
            self.frame_times.push(dt);
            if self.frame_times.len() > 30 {
                self.frame_times.remove(0);
            }

            // Sensor values change once per engine tick; never wait for the engine's lock.
            if let Ok(st) = self.shared.state.try_read()
                && st.tick != state_tick
            {
                state_tick = st.tick;
                sensors.clear();
                for id in &sensor_ids {
                    if let Some(v) = st.value(id) {
                        sensors.insert(id.clone(), v);
                    }
                }
            }
            let needs_audio = cfg.enabled && {
                let uses = |e: &Effect| e.kind == EffectKind::Music;
                if cfg.sync { uses(&cfg.global) } else { cfg.zones.values().any(|z| uses(&z.effect)) }
            };
            self.audio.set_enabled(needs_audio);
            let audio_levels = self.audio.levels();
            let alert = (cfg.temp_alert
                && sensors.get(crate::model::ids::CPU_TEMP).is_some_and(|t| *t >= cfg.temp_alert_c))
            .then(|| (self.shared.elapsed() as f32).fract() < 0.5);
            let identify =
                self.identify.as_ref().filter(|(_, t)| t.elapsed() < Duration::from_secs(3)).map(|(id, _)| id.clone());
            self.expire_test(Instant::now());
            let out = self.renderer.frame(&FrameInput {
                cfg: &cfg,
                zones: &zones,
                dt,
                sensors: &sensors,
                audio: audio_levels.as_ref(),
                alert,
                identify: identify.as_deref(),
                test: self.test.as_ref(),
            });

            let (connected, packet_ms) = self.link.conn();
            let packets = out.desired.packets_per_frame() as u32;
            let fps = fps_cap(cfg.fps, &out.desired, packet_ms);
            if self.suspended_at.is_none() {
                self.link.post(out.desired);
            }

            // The live preview is only needed while the Lighting page is on screen.
            let preview = PREVIEW_WANTED.load(Ordering::Relaxed);
            if last_publish.elapsed() > Duration::from_millis(if preview { 30 } else { 500 }) {
                last_publish = Instant::now();
                let ls = self.link.status();
                self.publish(&ls, zones.clone(), out.previews, fps, packets, out.animated);
            }

            // Without a streaming effect the frames only feed the UI preview: idle while unseen.
            let fast = (connected && out.streaming && self.suspended_at.is_none()) || (preview && out.animated);
            let sleep = if fast {
                Duration::from_secs_f32(1.0 / fps)
            } else if !preview {
                Duration::from_millis(400)
            } else {
                Duration::from_millis(150)
            };
            match rx.recv_timeout(sleep) {
                Ok(cmd) => self.command(cmd),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        self.exit();
    }

    fn publish(
        &mut self,
        ls: &LinkStatus,
        zones: Vec<ZoneDesc>,
        preview: HashMap<String, Vec<[u8; 3]>>,
        fps_cap: f32,
        packets: u32,
        animated: bool,
    ) {
        self.rev = self.rev.wrapping_add(1);
        let fps = if self.frame_times.is_empty() {
            0.0
        } else {
            self.frame_times.len() as f32 / self.frame_times.iter().sum::<f32>()
        };
        let connected = ls.state == RgbConnState::Connected;
        let ledc_slots = zones.iter().find(|z| z.id == "ledc").map(|z| z.slots.clone()).unwrap_or_default();
        let dl_raw = ls.device.as_ref().filter(|_| connected).and_then(|d| d.dynamic_lighting_raw);
        let dynamic_lighting = match &self.dl {
            Some(m) => {
                m.set_firmware(dl_raw);
                m.status()
            }
            None => dynlight::DynamicLightingStatus { firmware_raw: dl_raw, ..Default::default() },
        };
        let status = RgbStatus {
            device: ls.device.as_ref().filter(|_| connected).map(|d| RgbDeviceInfo {
                name: if d.info.name.is_empty() { "RGB Fusion 2".into() } else { d.info.name.clone() },
                pid: d.pid,
                fw: d.info.fw.clone(),
                chip_id: d.info.chip_id,
                calibration: format!(
                    "D_LED1 {}, D_LED2 {}",
                    fusion2::order_from_calibration(d.info.cal_strip0).map(order_name).unwrap_or("unset".into()),
                    fusion2::order_from_calibration(d.info.cal_strip1).map(order_name).unwrap_or("unset".into())
                ),
                persistence: d.info.supports_persistence(),
                support_flags: d.info.support_flags,
                dynamic_lighting_raw: d.dynamic_lighting_raw,
            }),
            zones,
            preview,
            error: if connected { None } else { ls.last_error.as_ref().map(|e| e.message()) },
            audio: self.audio.state(),
            fps,
            saved_to_flash: ls.saved_to_flash,
            state: ls.state.clone(),
            last_error: ls.last_error.clone(),
            next_retry: ls.next_retry,
            attempts: ls.attempts,
            reconnects: ls.reconnects,
            write_errors: ls.write_errors,
            fps_cap,
            packets_per_frame: packets,
            packet_ms: ls.packet_ms,
            suspended: ls.suspended || self.suspended_at.is_some(),
            ledc_slots,
            hung_io_threads: ls.hung_threads,
            animated,
            simulated: self.simulated,
            rev: self.rev,
            dynamic_lighting,
            test: self
                .test
                .as_ref()
                .map(|t| TestStatus { zones: t.zones.iter().map(|z| z.zone.clone()).collect(), until: t.until }),
        };
        if let Ok(mut s) = self.shared.rgb.write() {
            *s = status;
        }
    }

    fn persist_cmd(&self, ack: Option<SyncSender<()>>) -> WorkerCmd {
        let cfg = self.config();
        WorkerCmd::Persist { policy: cfg.on_exit, cycle_brightness: hw_brightness(1.0, cfg.brightness), ack }
    }

    fn resume(&mut self) {
        self.suspended_at = None;
        // PBT_APMRESUMEAUTOMATIC and the frame-gap detector both report the same wake-up: one
        // re-init (`[CC 47 00]`, clear, 100 ms settle) is enough.
        if self.last_resume.is_some_and(|t| t.elapsed() < Duration::from_secs(5)) {
            return;
        }
        self.last_resume = Some(Instant::now());
        self.link.send(WorkerCmd::Resume);
    }

    /// The test's lease ran out (no user action for a minute, or the UI is gone): back to normal.
    fn expire_test(&mut self, now: Instant) {
        if self.test.as_ref().is_some_and(|t| now >= t.until) {
            self.test = None;
            log::info!("RGB: calibration test timed out; saved lighting restored");
        }
    }

    /// Ends a running calibration test and posts a frame of the saved lighting right away, so a
    /// following persist (exit, sleep) hands the *saved* lighting to the controller, never a test
    /// colour.
    fn end_test_now(&mut self, why: &str) {
        if self.test.take().is_none() {
            return;
        }
        log::info!("RGB: calibration test ended ({why}); saved lighting restored");
        let cfg = self.config();
        let zones = zones_for(cfg.ledc_slot);
        let sensors = HashMap::new();
        let out = self.renderer.frame(&FrameInput {
            cfg: &cfg,
            zones: &zones,
            dt: 0.001,
            sensors: &sensors,
            audio: None,
            alert: None,
            identify: None,
            test: None,
        });
        self.link.post(out.desired);
    }

    fn command(&mut self, cmd: RgbCommand) {
        match cmd {
            RgbCommand::TestColor(p) => {
                if self.suspended_at.is_some() {
                    log::info!("RGB: calibration test ignored while the lighting is handed over (sleep / exit)");
                    return;
                }
                if self.test.is_none() {
                    log::info!("RGB: calibration test started ({} zone(s))", p.zones.len());
                }
                self.test = Some(p.sanitized(Instant::now()));
            }
            RgbCommand::EndTest => {
                if self.test.take().is_some() {
                    log::info!("RGB: calibration test ended; saved lighting restored");
                }
            }
            RgbCommand::ClaimDynamicLighting => match &self.dl {
                Some(m) => m.claim(),
                None => log::warn!("RGB: Dynamic Lighting monitor not running"),
            },
            RgbCommand::Reconnect => {
                if !self.link.reconnect() {
                    log::warn!("RGB: reconnect not possible, requests are stuck in the USB driver");
                }
            }
            RgbCommand::Identify(zone) => self.identify = Some((zone, Instant::now())),
            RgbCommand::SaveToFlash => {
                if self.test.is_some() {
                    log::warn!("RGB: \"Save to controller\" ignored while a calibration test is showing");
                } else {
                    self.link.send(WorkerCmd::SaveToFlash)
                }
            }
            RgbCommand::Suspend(ack) => {
                if self.suspended_at.is_none() {
                    self.end_test_now("system suspend");
                    log::info!("RGB: suspending (handing lighting over to the controller)");
                    let c = self.persist_cmd(ack);
                    self.link.send(c);
                    self.suspended_at = Some(Instant::now());
                } else if let Some(ack) = ack {
                    let _ = ack.try_send(());
                }
            }
            RgbCommand::Resume => {
                log::info!("RGB: resume");
                self.resume();
            }
        }
    }

    /// Exit lighting + `[CC 47 01]` so it persists through shutdown.
    fn exit(&mut self) {
        if self.suspended_at.is_some() {
            return; // already handed over (suspend / session end)
        }
        self.end_test_now("app exit");
        let c = self.persist_cmd(None);
        let WorkerCmd::Persist { policy, cycle_brightness, .. } = c else { return };
        let ok = self
            .link
            .request(|ack| WorkerCmd::Persist { policy, cycle_brightness, ack: Some(ack) }, Duration::from_secs(2));
        if !ok {
            log::warn!("RGB: could not apply the exit lighting in time");
        }
    }
}

/// "GRB" for a byte order.
pub fn order_name(o: ByteOrder) -> String {
    let mut s = [' '; 3];
    s[o[0]] = 'R';
    s[o[1]] = 'G';
    s[o[2]] = 'B';
    s.iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(cfg: &'a RgbConfig, zones: &'a [ZoneDesc], sensors: &'a HashMap<String, f32>) -> FrameInput<'a> {
        FrameInput { cfg, zones, dt: 0.025, sensors, audio: None, alert: None, identify: None, test: None }
    }

    fn eff(kind: EffectKind, c: [u8; 3]) -> Effect {
        Effect { kind, colors: vec![c], brightness: 1.0, speed: 1.0, ..Default::default() }
    }

    fn cfg_with(global: Effect) -> RgbConfig {
        RgbConfig { brightness: 1.0, sync: true, global, ..Default::default() }
    }

    #[test]
    fn led_c_drives_both_candidate_slots_unless_chosen() {
        let z = zones_for(None);
        let ledc = z.iter().find(|z| z.id == "ledc").unwrap();
        assert_eq!(ledc.slots, vec![1, 4]);
        assert!(!ledc.advanced);
        assert!(!z.iter().any(|z| z.id == "led5" || z.id == "led2"), "slot 4 is part of LED_C");
        let z4 = zones_for(Some(4));
        assert_eq!(z4.iter().find(|z| z.id == "ledc").unwrap().slots, vec![4]);
        assert_eq!(z4.iter().find(|z| z.id == "led2").unwrap().slot, 1);
        let z1 = zones_for(Some(1));
        assert_eq!(z1.iter().find(|z| z.id == "led5").unwrap().slot, 4);
        assert_eq!(zones_for(Some(9)), z, "invalid choice = both");
        let d1 = z.iter().find(|z| z.id == "dled1").unwrap();
        assert_eq!((d1.slot, d1.header), (5, Some(0)));
        let d2 = z.iter().find(|z| z.id == "dled2").unwrap();
        assert_eq!((d2.slot, d2.header), (6, Some(1)));

        let cfg = cfg_with(eff(EffectKind::HwStatic, [255, 0, 0]));
        let s = HashMap::new();
        let out = Renderer::default().frame(&input(&cfg, &z, &s));
        let slots: Vec<u8> = out.desired.singles.iter().map(|(s, _)| *s).collect();
        assert_eq!(slots, vec![1, 4]);
    }

    #[test]
    fn uniform_software_sync_uses_the_sync_all_path() {
        let cfg = cfg_with(eff(EffectKind::Static, [255, 0, 0]));
        let z = standard_zones();
        let s = HashMap::new();
        let out = Renderer::default().frame(&input(&cfg, &z, &s));
        assert_eq!(out.desired.sync_all, Some([255, 0, 0]));
        assert_eq!(out.desired.packets_per_frame(), 2);
        assert_eq!(out.previews["dled1"].len(), 30);
        assert!(!out.streaming);
        // Master brightness is perceptual.
        let cfg = RgbConfig { brightness: 0.5, ..cfg };
        let out = Renderer::default().frame(&input(&cfg, &z, &s));
        assert_eq!(out.desired.sync_all, Some([55, 0, 0]));
    }

    #[test]
    fn rainbow_streams_and_hardware_wave_maps_per_zone() {
        let mut cfg = cfg_with(eff(EffectKind::Rainbow, [0, 0, 0]));
        cfg.sync = false;
        let wave = Effect { kind: EffectKind::HwWave, brightness: 1.0, speed: 1.0, ..Default::default() };
        cfg.zones.insert("dled2".into(), ZoneConfig { effect: wave.clone(), led_count: 60, ..Default::default() });
        cfg.zones.insert("ledc".into(), ZoneConfig { effect: wave, ..Default::default() });
        let z = standard_zones();
        let s = HashMap::new();
        let out = Renderer::default().frame(&input(&cfg, &z, &s));
        let d = &out.desired;
        assert_eq!(d.sync_all, None);
        assert!(matches!(&d.headers[0], HeaderOut::Direct(f) if f.len() == 30));
        assert!(matches!(d.headers[1], HeaderOut::Hw(HwEffect { mode: HwMode::Wave, level: 9, brightness: 255, .. })));
        assert!(d.singles.iter().all(|(_, o)| matches!(o, SingleOut::Hw(HwEffect { mode: HwMode::ColorCycle, .. }))));
        assert_eq!(d.led_counts, [30, 60]);
        assert!(out.streaming);
    }

    #[test]
    fn synced_hardware_wave_is_the_whole_board_wave() {
        let cfg = cfg_with(Effect { kind: EffectKind::HwWave, brightness: 1.0, speed: 0.0, ..Default::default() });
        let z = standard_zones();
        let s = HashMap::new();
        let out = Renderer::default().frame(&input(&cfg, &z, &s));
        assert_eq!(out.desired.whole_wave, Some(HwEffect::new(HwMode::Wave, [0, 0, 0], 255, 0)));
    }

    #[test]
    fn overheat_alert_and_identify() {
        let cfg = cfg_with(eff(EffectKind::Rainbow, [0, 0, 0]));
        let z = standard_zones();
        let s = HashMap::new();
        let mut r = Renderer::default();
        let out = r.frame(&FrameInput { alert: Some(true), ..input(&cfg, &z, &s) });
        assert_eq!(out.desired.sync_all, Some([255, 0, 0]));
        let cfg = cfg_with(eff(EffectKind::Static, [0, 0, 255]));
        let out = r.frame(&FrameInput { identify: Some("ledc"), ..input(&cfg, &z, &s) });
        assert_eq!(out.desired.sync_all, None, "identify drives one zone on its own");
        assert!(matches!(out.desired.headers[0], HeaderOut::Direct(_)));
    }

    #[test]
    fn crossfade_between_colours() {
        let mut cfg = cfg_with(eff(EffectKind::Static, [255, 0, 0]));
        cfg.sync = false;
        cfg.zones.insert(
            "dled1".into(),
            ZoneConfig { effect: eff(EffectKind::Static, [255, 0, 0]), led_count: 4, ..Default::default() },
        );
        cfg.zones
            .insert("dled2".into(), ZoneConfig { effect: eff(EffectKind::Rainbow, [0, 0, 0]), ..Default::default() });
        let z = standard_zones();
        let s = HashMap::new();
        let mut r = Renderer::default();
        r.frame(&input(&cfg, &z, &s));
        cfg.zones.get_mut("dled1").unwrap().effect = eff(EffectKind::Static, [0, 0, 255]);
        let out = r.frame(&FrameInput { dt: 0.15, ..input(&cfg, &z, &s) });
        let mid = out.previews["dled1"][0];
        assert!(mid[0] > 60 && mid[2] > 60, "halfway through the fade: {mid:?}");
        assert!(out.streaming);
        let out = r.frame(&FrameInput { dt: 0.2, ..input(&cfg, &z, &s) });
        assert_eq!(out.previews["dled1"][0], [0, 0, 255]);
    }

    #[test]
    fn off_fades_out_then_becomes_a_hardware_black() {
        let mut cfg = cfg_with(eff(EffectKind::Static, [255, 255, 255]));
        cfg.sync = false;
        cfg.zones
            .insert("dled2".into(), ZoneConfig { effect: eff(EffectKind::Rainbow, [0, 0, 0]), ..Default::default() });
        let z = standard_zones();
        let s = HashMap::new();
        let mut r = Renderer::default();
        r.frame(&input(&cfg, &z, &s));
        cfg.zones.insert("dled2".into(), ZoneConfig { enabled: false, ..Default::default() });
        let out = r.frame(&FrameInput { dt: 0.1, ..input(&cfg, &z, &s) });
        assert!(matches!(out.desired.headers[1], HeaderOut::Direct(_)), "fading out");
        let out = r.frame(&FrameInput { dt: 0.3, ..input(&cfg, &z, &s) });
        assert_eq!(out.desired.headers[1], HeaderOut::Hw(HwEffect::off()));
    }

    /// Colour correction reaches the controller bytes on every path (sync record, streamed frames,
    /// 12 V soft colours, hardware effect colours) and never the previews.
    #[test]
    fn correction_applies_to_device_bytes_only() {
        let red = [0xFF, 0x1E, 0x1E];
        let z = standard_zones();
        let s = HashMap::new();
        // Sync path: one corrected record, preview keeps the picker colour.
        let cfg = cfg_with(eff(EffectKind::Static, red));
        let out = Renderer::default().frame(&input(&cfg, &z, &s));
        assert_eq!(out.desired.sync_all, Some([255, 2, 2]));
        assert_eq!(out.previews["dled1"][0], red);
        // Per zone: streamed frame, 12 V soft colour and a hardware Static record.
        let mut cfg = cfg_with(eff(EffectKind::Static, red));
        cfg.sync = false;
        cfg.zones.insert("dled1".into(), ZoneConfig { effect: eff(EffectKind::Rainbow, [0; 3]), ..Default::default() });
        cfg.zones.insert("dled2".into(), ZoneConfig { effect: eff(EffectKind::HwStatic, red), ..Default::default() });
        cfg.zones.insert("ledc".into(), ZoneConfig { effect: eff(EffectKind::Static, red), ..Default::default() });
        let out = Renderer::default().frame(&input(&cfg, &z, &s));
        let d = &out.desired;
        assert!(matches!(&d.headers[1], HeaderOut::Hw(e) if e.color == [255, 2, 2]), "{:?}", d.headers[1]);
        assert!(d.singles.iter().all(|(_, o)| *o == SingleOut::Soft([255, 2, 2])), "{:?}", d.singles);
        assert_eq!(out.previews["ledc"][0], red);
        let HeaderOut::Direct(f) = &d.headers[0] else { panic!("{:?}", d.headers[0]) };
        let pv = &out.previews["dled1"];
        let k = (0..f.len()).find(|&i| pv[i].iter().any(|&v| v > 40 && v < 215)).expect("a mixed colour");
        assert_ne!(f[k], pv[k], "streamed bytes are corrected, the preview is not");
        // Gamma 1 with unity gains is the raw byte path.
        let mut raw = cfg_with(eff(EffectKind::Static, red));
        for id in ["dled1", "dled2", "ledc"] {
            let mut zc = ZoneConfig::default();
            zc.correction.gamma = 1.0;
            raw.zones.insert(id.into(), zc);
        }
        let out = Renderer::default().frame(&input(&raw, &z, &s));
        assert_eq!(out.desired.sync_all, Some(red));
        // Zones with different corrections can't share one sync record.
        raw.zones.get_mut("ledc").unwrap().correction.gain = [1.0, 0.5, 0.5];
        let out = Renderer::default().frame(&input(&raw, &z, &s));
        assert_eq!(out.desired.sync_all, None);
        assert!(out.desired.singles.iter().all(|(_, o)| *o == SingleOut::Soft([255, 15, 15])));
        assert!(matches!(&out.desired.headers[0], HeaderOut::Direct(f) if f[0] == red));
    }

    /// A colour-order override disables the sync record (which the
    /// firmware orders itself), also for the overheat alert.
    #[test]
    fn colour_order_override_disables_the_sync_record() {
        let mut cfg = cfg_with(eff(EffectKind::Static, [0, 0, 255]));
        cfg.zones.insert("dled1".into(), ZoneConfig { color_order: ColorOrder::Rgb, ..Default::default() });
        let z = standard_zones();
        let s = HashMap::new();
        let out = Renderer::default().frame(&input(&cfg, &z, &s));
        assert_eq!(out.desired.sync_all, None);
        assert_eq!(out.desired.order_override[0], Some([0, 1, 2]));
        assert!(matches!(&out.desired.headers[0], HeaderOut::Direct(f) if f[0] == [0, 0, 255]));
        let out = Renderer::default().frame(&FrameInput { alert: Some(true), ..input(&cfg, &z, &s) });
        assert_eq!(out.desired.sync_all, None);
        assert!(matches!(&out.desired.headers[0], HeaderOut::Direct(f) if f[0] == [255, 0, 0]));
    }

    fn pump_zone(gain: [f32; 3]) -> ZoneConfig {
        ZoneConfig {
            led_count: 10,
            segments: vec![
                Segment { name: "Fans".into(), from_led: 0, to_led: 6, ..Default::default() },
                Segment { name: "Pump".into(), from_led: 6, to_led: 10, gain },
            ],
            ..Default::default()
        }
    }

    /// A uniform colour with per-segment gains (pump head chained behind the fans)
    /// is streamed per LED instead of the sync record, and a firmware Static is streamed too.
    #[test]
    fn segment_gains_stream_uniform_colours_per_led() {
        let z = standard_zones();
        let s = HashMap::new();
        let mut cfg = cfg_with(eff(EffectKind::Static, [255, 255, 255]));
        cfg.zones.insert("dled1".into(), pump_zone([1.0; 3]));
        let out = Renderer::default().frame(&input(&cfg, &z, &s));
        assert_eq!(out.desired.sync_all, Some([255, 255, 255]), "unity segment gains keep the sync record");
        cfg.zones.insert("dled1".into(), pump_zone([1.0, 1.0, 0.5]));
        let out = Renderer::default().frame(&input(&cfg, &z, &s));
        let d = &out.desired;
        assert_eq!(d.sync_all, None);
        let HeaderOut::Direct(f) = &d.headers[0] else { panic!("{:?}", d.headers[0]) };
        assert_eq!((f.len(), f[0], f[5], f[6], f[9]), (10, [255; 3], [255; 3], [255, 255, 128], [255, 255, 128]));
        assert!(matches!(&d.headers[1], HeaderOut::Direct(f) if f[0] == [255; 3]));
        assert!(d.singles.iter().all(|(_, o)| *o == SingleOut::Soft([255; 3])));
        assert_eq!(out.previews["dled1"][7], [255; 3], "preview keeps the screen colour");
        assert!(!out.streaming, "static: sent on change + keep-alive only");
        // Firmware Static on a header with segments → streamed; on the other header it stays a record.
        cfg.sync = false;
        let mut zc = pump_zone([1.0, 1.0, 0.5]);
        zc.effect = eff(EffectKind::HwStatic, [255, 255, 255]);
        cfg.zones.insert("dled1".into(), zc);
        cfg.zones
            .insert("dled2".into(), ZoneConfig { effect: eff(EffectKind::HwStatic, [255; 3]), ..Default::default() });
        let out = Renderer::default().frame(&input(&cfg, &z, &s));
        assert!(matches!(&out.desired.headers[0], HeaderOut::Direct(f) if f[9] == [255, 255, 128]));
        assert!(matches!(out.desired.headers[1], HeaderOut::Hw(_)));
    }

    /// Calibration test: reference colours on chosen segments through the correction under edit,
    /// everything else dark, the overheat alert still wins, and ending the test restores the
    /// saved lighting exactly.
    #[test]
    fn test_pattern_shows_reference_colours_and_restores() {
        use calibrate::{TestZone, ZoneCal};
        let z = standard_zones();
        let s = HashMap::new();
        let mut cfg = cfg_with(eff(EffectKind::Static, [255, 0, 0]));
        cfg.zones.insert("dled1".into(), ZoneConfig { led_count: 10, ..Default::default() });
        let mut r = Renderer::default();
        let before = r.frame(&input(&cfg, &z, &s)).desired;
        assert_eq!(before.sync_all, Some([255, 0, 0]));
        let edit = pump_zone([1.0, 1.0, 0.5]);
        let t = TestPattern {
            zones: vec![TestZone { zone: "dled1".into(), segment: Some(1), leds: Some((6, 10)), rgb: [255, 255, 255] }],
            corrections: [("dled1".to_string(), ZoneCal::of(&edit))].into_iter().collect(),
            dark_others: true,
            until: Instant::now() + Duration::from_secs(60),
        };
        let out = r.frame(&FrameInput { test: Some(&t), ..input(&cfg, &z, &s) });
        let d = &out.desired;
        assert_eq!(d.sync_all, None);
        let HeaderOut::Direct(f) = &d.headers[0] else { panic!("{:?}", d.headers[0]) };
        assert_eq!(f[..6], [[0; 3]; 6], "fans dark while only the pump is tested");
        assert_eq!(f[6..], [[255, 255, 128]; 4], "pump white through its edited gain");
        assert!(matches!(&d.headers[1], HeaderOut::Direct(f) if f.iter().all(|c| *c == [0; 3])));
        assert!(d.singles.iter().all(|(_, o)| *o == SingleOut::Soft([0; 3])));
        assert_eq!(out.previews["dled1"][7], [255; 3]);
        // Preview of the edit on the user's own effect (no reference colours).
        let pv = TestPattern { zones: vec![], ..t.clone() };
        let white = RgbConfig { global: eff(EffectKind::Static, [255, 255, 255]), ..cfg.clone() };
        let out = Renderer::default().frame(&FrameInput { test: Some(&pv), ..input(&white, &z, &s) });
        assert_eq!(out.desired.sync_all, None);
        assert!(matches!(&out.desired.headers[0], HeaderOut::Direct(f) if f[0] == [255; 3] && f[9] == [255, 255, 128]));
        // Overheat alert wins over a test.
        let out = r.frame(&FrameInput { test: Some(&t), alert: Some(true), ..input(&cfg, &z, &s) });
        assert_eq!(out.desired.sync_all, Some([255, 0, 0]));
        // Test over: exactly the saved lighting again.
        let after = r.frame(&input(&cfg, &z, &s)).desired;
        assert_eq!(after, before);
    }

    /// Temporal dithering keeps a dark channel's average instead of rounding it away.
    #[test]
    fn dithering_keeps_dark_hues() {
        let z = standard_zones();
        let s = HashMap::new();
        let mut cfg = cfg_with(eff(EffectKind::Static, [255, 86, 30]));
        cfg.brightness = 0.2;
        let ideal =
            LedPipeline::new(ColorCorrection::default(), &[], perceptual(0.2)).drive(0, Rgb::from_u8([255, 86, 30]));
        let plain = Renderer::default().frame(&input(&cfg, &z, &s));
        assert!(plain.desired.sync_all.is_some() && !plain.streaming);
        cfg.dither = true;
        let mut r = Renderer::default();
        let (mut sum, n) = ([0.0f32; 3], 400);
        for _ in 0..n {
            let out = r.frame(&input(&cfg, &z, &s));
            assert_eq!(out.desired.sync_all, None, "dithered frames are streamed");
            assert!(out.streaming);
            let HeaderOut::Direct(f) = &out.desired.headers[0] else { panic!() };
            for k in 0..3 {
                sum[k] += f[3][k] as f32;
            }
        }
        let want = [ideal.r * 255.0, ideal.g * 255.0, ideal.b * 255.0];
        for k in 0..3 {
            assert!((sum[k] / n as f32 - want[k]).abs() < 0.02, "channel {k}: {} vs {}", sum[k] / n as f32, want[k]);
        }
        assert!(want[2] < 0.5, "blue would round to 0 without dithering");
    }

    /// Exit (or sleep) during a calibration test hands the *saved* lighting to the controller,
    /// never the test colour; the lease ends a forgotten test.
    #[test]
    fn exit_during_a_test_persists_the_saved_lighting() {
        use crate::rgb::mock::{MockConnector, MockDev, Plug};
        let mut cfg = crate::config::Config::default();
        cfg.rgb = RgbConfig {
            on_exit: crate::config::ExitLighting::KeepLast,
            ..cfg_with(eff(EffectKind::Static, [255, 0, 0]))
        };
        let shared = Shared::new(cfg);
        let dev = MockDev::new(0x03);
        let conn = MockConnector::new(vec![Plug::Present(dev.clone())]);
        let c = conn.clone();
        let link =
            Link::start(Arc::new(move || Box::new(c.clone()) as Box<dyn transport::Connector>), Timings::default());
        let mut e = RgbEngine::new(shared, link);
        let wait = |e: &mut RgbEngine, what: &str, f: &dyn Fn(&RgbEngine) -> bool| {
            let t0 = Instant::now();
            while !f(e) {
                e.link.poll();
                assert!(t0.elapsed() < Duration::from_secs(4), "timed out: {what}");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        wait(&mut e, "connected", &|e| e.link.status().state == RgbConnState::Connected);
        e.command(RgbCommand::TestColor(TestPattern::solid(
            "dled1",
            [255, 255, 255],
            Instant::now() + Duration::from_secs(30),
        )));
        let cfgr = e.config();
        let zones = zones_for(None);
        let s = HashMap::new();
        let out = e.renderer.frame(&FrameInput { test: e.test.as_ref(), ..input(&cfgr, &zones, &s) });
        assert!(matches!(&out.desired.headers[0], HeaderOut::Direct(f) if f[0] == [255; 3]));
        e.link.post(out.desired);
        wait(&mut e, "test colour streamed", &|_| {
            dev.sent().iter().any(|p| p[1] == 0x58 && p[5..8] == [255, 255, 255])
        });
        e.exit();
        assert!(e.test.is_none());
        let sent = dev.sent();
        assert_eq!(sent.last().unwrap()[..3], [0xCC, 0x47, 0x01], "keep-settings last");
        let rec = sent.iter().rev().find(|p| p[1] == 0x20).expect("sync record");
        assert_eq!(rec[2], 0xFF, "whole-board record");
        assert_eq!(rec[14..17], [0, 0, 255], "saved red (BB GG RR), not the test white");
        // Lease: an expired test is dropped.
        e.test = Some(TestPattern::solid("dled1", [1, 2, 3], Instant::now()));
        e.expire_test(Instant::now() + Duration::from_millis(1));
        assert!(e.test.is_none());
    }

    #[test]
    fn frame_rate_cap() {
        let big = Desired {
            headers: [HeaderOut::Direct(vec![[0; 3]; 1024]), HeaderOut::Direct(vec![[0; 3]; 1024])],
            ..Default::default()
        };
        let fps = fps_cap(60, &big, 0.0);
        assert!((fps - 1000.0 / (108.0 * 1.5)).abs() < 0.1, "{fps}");
        let small = Desired { headers: [HeaderOut::Direct(vec![[0; 3]; 30]), HeaderOut::Unused], ..Default::default() };
        assert_eq!(fps_cap(60, &small, 1.0), 60.0);
        let sync = Desired { sync_all: Some([1, 1, 1]), ..Default::default() };
        assert_eq!(fps_cap(60, &sync, 1.0), 40.0);
        assert_eq!(fps_cap(500, &Desired::default(), 0.0), 60.0);
    }
}
