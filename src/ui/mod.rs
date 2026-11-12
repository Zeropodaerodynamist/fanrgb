//! egui user interface ("Quiet Instrument" design).
//!
//! Shell: sidebar / icon rail, top bar with the single profile switcher, critical banner, page body,
//! overlays (modal, command palette, toasts). Pages live in `pages/`, widgets in `widgets/`.

mod actions;
mod anim;
mod assets;
mod board_map;
mod fonts;
mod icons;
mod issues;
mod layout;
mod licences;
mod overlays;
mod pages;
mod theme;
mod tray;
mod widgets;
mod window;

pub use window::request_show;

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use egui::{Align2, Id, Key, KeyboardShortcut, Margin, Modifiers, Rect, Sense, Stroke, ViewportCommand, pos2, vec2};

use crate::config::{Config, EffectKind};
use crate::engine::{Command, EngineState, History, Shared};
use crate::error::{ErrorCode, Severity};
use crate::model::{SensorKind, ids};
use crate::rgb::RgbStatus;
use actions::{PaletteEntry, UiAction};
use anim::MotionPref;
use fonts::Ts;
use icons::Icon;
use issues::{IssueTracker, Transition};
use layout::Breakpoint;
use overlays::{Modal, PaletteState, Toast, ToastKind};
use theme::{R_MD, R_PILL, ThemePref, pal, pal_ctx, with_alpha};
use widgets::controls::{self, Button, Kind};
use widgets::text;

// ---------------------------------------------------------------------------------------
// Pages

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum Page {
    #[default]
    Overview,
    Cooling,
    Lighting,
    Intelligence,
    Sensors,
    Updates,
    Diagnostics,
    Settings,
}

impl Page {
    /// Sidebar order: the dashboard pages, a divider, then Updates, Diagnostics, Settings.
    pub const ALL: [Page; 8] = [
        Page::Overview,
        Page::Cooling,
        Page::Lighting,
        Page::Intelligence,
        Page::Sensors,
        Page::Updates,
        Page::Diagnostics,
        Page::Settings,
    ];

    /// Ctrl+digit shortcut (Updates is Ctrl+8, after the existing 1-7).
    pub fn key(self) -> Key {
        match self {
            Page::Overview => Key::Num1,
            Page::Cooling => Key::Num2,
            Page::Lighting => Key::Num3,
            Page::Intelligence => Key::Num4,
            Page::Sensors => Key::Num5,
            Page::Diagnostics => Key::Num6,
            Page::Settings => Key::Num7,
            Page::Updates => Key::Num8,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Page::Overview => "Overview",
            Page::Cooling => "Cooling",
            Page::Lighting => "Lighting",
            Page::Intelligence => "Intelligence",
            Page::Sensors => "Sensors",
            Page::Updates => "Updates",
            Page::Diagnostics => "Diagnostics",
            Page::Settings => "Settings",
        }
    }

    pub fn icon(self) -> Icon {
        match self {
            Page::Overview => Icon::Overview,
            Page::Cooling => Icon::Cooling,
            Page::Lighting => Icon::Lighting,
            Page::Intelligence => Icon::Intelligence,
            Page::Sensors => Icon::Sensors,
            Page::Updates => Icon::Update,
            Page::Diagnostics => Icon::Diagnostics,
            Page::Settings => Icon::Settings,
        }
    }

    pub fn shortcut(self) -> &'static str {
        match self {
            Page::Overview => "Ctrl+1",
            Page::Cooling => "Ctrl+2",
            Page::Lighting => "Ctrl+3",
            Page::Intelligence => "Ctrl+4",
            Page::Sensors => "Ctrl+5",
            Page::Updates => "Ctrl+8",
            Page::Diagnostics => "Ctrl+6",
            Page::Settings => "Ctrl+7",
        }
    }
}

// ---------------------------------------------------------------------------------------
// Persistent UI preferences (stored with the window state by eframe, not in config.json)

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct UiPrefs {
    pub theme: ThemePref,
    pub motion: MotionPref,
    pub zoom: f32,
    pub chart_range_s: u32,
    pub recent_colors: Vec<[u8; 3]>,
    pub last_page: Page,
    pub diffused_leds: bool,
    pub curve_rpm_axis: bool,
    pub show_board_map: bool,
    /// Spin the fan glyphs at a speed that follows the RPM (repaints ~30 times per second while
    /// a dashboard page is focused; off = the dashboard repaints once per data tick).
    pub spin_fans: bool,
    /// Saved window rectangle (eframe's own window persistence is off, see `window.rs`).
    pub placement: Option<window::Placement>,
    /// Overview shows the rendered rig with live LED colours (off: the board map instead).
    pub show_hero: bool,
    /// Lighting inspector tab (0 Effect, 1 Colour calibration, 2 Zone setup).
    pub lighting_tab: u8,
    /// Version whose update banner was dismissed ("Later"): the banner shows once per version.
    pub update_banner_dismissed: String,
    /// "Identify your fans" prompt dismissed (Cooling keeps the entry in its header menu).
    pub identify_prompt_dismissed: bool,
}

impl Default for UiPrefs {
    fn default() -> Self {
        UiPrefs {
            theme: ThemePref::System,
            motion: MotionPref::Auto,
            zoom: 1.0,
            chart_range_s: 300,
            recent_colors: Vec::new(),
            last_page: Page::Overview,
            diffused_leds: false,
            curve_rpm_axis: false,
            show_board_map: true,
            spin_fans: false,
            placement: None,
            show_hero: true,
            lighting_tab: 0,
            update_banner_dismissed: String::new(),
            identify_prompt_dismissed: false,
        }
    }
}

const PREFS_KEY: &str = "fanrgb.ui";

// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LogLevel {
    #[default]
    All,
    Info,
    Warn,
    Error,
}

/// Transient UI state shared with pages.
#[derive(Default)]
pub struct UiState {
    pub selected_fan: Option<String>,
    pub selected_zone: Option<String>,
    pub show_hidden_fans: bool,
    pub rename_fan: Option<(String, String)>,
    pub sensor_filter: String,
    pub sensor_kind: Option<SensorKind>,
    pub sensor_selected: Option<String>,
    pub sensor_collapsed: HashSet<String>,
    pub settings_filter: String,
    pub settings_jump: Option<usize>,
    pub log_level: LogLevel,
    pub log_follow: bool,
    pub ai_zone: usize,
    pub toasts: VecDeque<Toast>,
    next_toast: u64,
    pub modal: Option<Modal>,
    pub palette: Option<PaletteState>,
    pub pending: Vec<UiAction>,
    pub issues: IssueTracker,
    pub gallery: widgets::gallery::GalleryState,
    pub autostart_st: Option<crate::system::AutostartStatus>,
    pub autostart_rx: Option<std::sync::mpsc::Receiver<crate::system::AutostartStatus>>,
    pub rgb_reconnect: Option<Instant>,
    pub winget: Option<crate::util::win::UnelevatedChild>,
    pub winget_done: bool,
    pub focus_search: bool,
    pub behavior_open: bool,
    /// "Match colours" wizard in progress (edits live here until Save).
    pub calibration: Option<crate::rgb::calibrate::CalSession>,
    /// `CalSession::rev` last sent to the lighting thread.
    pub calibration_sent: u64,
    /// The test pattern was taken off the LEDs (page left, idle); edits kept.
    pub calibration_paused: bool,
    /// App self-update state and actions (see `crate::updater::Updater`).
    pub updates: crate::updater::Updater,
    /// Token being pasted in Settings › Updates (wiped when saved).
    pub update_token_draft: String,
}

impl UiState {
    pub fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        self.push_toast(kind, text.into(), None, None);
    }

    pub fn toast_action(&mut self, kind: ToastKind, text: impl Into<String>, label: &str, action: UiAction) {
        self.push_toast(kind, text.into(), None, Some((label.to_string(), action)));
    }

    pub fn toast_code(&mut self, kind: ToastKind, text: impl Into<String>, code: ErrorCode) {
        self.push_toast(kind, text.into(), Some(code), None);
    }

    fn push_toast(
        &mut self,
        kind: ToastKind,
        text: String,
        code: Option<ErrorCode>,
        action: Option<(String, UiAction)>,
    ) {
        self.next_toast += 1;
        self.toasts.push_back(Toast {
            id: self.next_toast,
            kind,
            text,
            code,
            action,
            remaining: kind.lifetime(),
            closing: None,
        });
    }

    /// Queries the Start-with-Windows state on a helper thread (COM call, 10-50 ms).
    pub fn refresh_autostart(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::system::autostart_status());
        });
        self.autostart_rx = Some(rx);
    }

    pub fn reconnecting(&self) -> bool {
        self.rgb_reconnect.is_some_and(|t| t.elapsed() < Duration::from_secs(5))
    }
}

pub struct PageCtx<'a> {
    pub shared: &'a Arc<Shared>,
    pub st: &'a EngineState,
    pub rgb: &'a RgbStatus,
    pub hist: &'a History,
    pub cfg: &'a mut Config,
    pub ui: &'a mut UiState,
    pub prefs: &'a mut UiPrefs,
    pub page: &'a mut Page,
    pub bp: Breakpoint,
    /// Window has focus (animations may run at full rate).
    pub focused: bool,
    /// Rendered design textures (loaded on first use, freed when idle).
    pub assets: &'a mut assets::Assets,
}

/// Longest a hidden (minimized / tray) window sleeps between frames when nothing woke it.
const HIDDEN_WAKE: Duration = Duration::from_secs(30);

pub fn run(shared: Arc<Shared>, minimized: bool) -> Result<(), String> {
    // Window / taskbar icon: the rendered app icon, the procedural one as fallback.
    let icon = assets::rgba_image(assets::WINDOW_ICON)
        .map(|img| egui::IconData {
            rgba: img.pixels.iter().flat_map(|c| c.to_srgba_unmultiplied()).collect(),
            width: img.size[0] as u32,
            height: img.size[1] as u32,
        })
        .unwrap_or_else(|| egui::IconData { rgba: crate::icon_art::render_icon_rgba(64), width: 64, height: 64 });
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FanRGB")
            .with_app_id("FanRGB")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([900.0, 600.0])
            .with_max_inner_size([window::MAX_SIDE_PX, window::MAX_SIDE_PX])
            .with_icon(icon),
        // eframe would save the minimized/tray geometry (0x0 at -32000); `UiPrefs::placement` instead.
        persist_window: false,
        // Elevated UI: keep window/UI state in the protected state folder.
        persistence_path: Some(crate::util::paths::ui_persistence_file()),
        ..Default::default()
    };
    eframe::run_native(
        "FanRGB",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, shared, minimized)) as Box<dyn eframe::App>)),
    )
    .map_err(|e| e.to_string())
}

pub struct App {
    shared: Arc<Shared>,
    page: Page,
    tray: Option<tray::Tray>,
    ui: UiState,
    prefs: UiPrefs,
    applied_prefs: Option<UiPrefs>,
    last_tray_sync: Instant,
    frames: u32,
    st: EngineState,
    st_tick: u64,
    cfg: Config,
    cfg_base: Config,
    cfg_rev: u64,
    rgb_error_sig: Option<String>,
    page_switched: f64,
    scrolled: bool,
    kb_nav: bool,
    nav_y: Option<f32>,
    last_critical: bool,
    /// Last RGB status snapshot (the UI never blocks on the RGB thread's lock).
    rgb: RgbStatus,
    /// History snapshot, refreshed once per engine tick (the engine's lock is never held during layout).
    hist: Arc<History>,
    hist_tick: u64,
    /// Pixels-per-point the max window size was last sent for.
    max_size_ppp: f32,
    /// Last frame was drawn on screen (vs. minimized / tray).
    was_shown: bool,
    assets: assets::Assets,
    /// Active profile the Identify check last looked at ([`App::prompt_identify_on_mode_change`]).
    seen_profile: usize,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, shared: Arc<Shared>, minimized: bool) -> Self {
        fonts::install(&cc.egui_ctx);
        let prefs: UiPrefs = cc.storage.and_then(|s| eframe::get_value(s, PREFS_KEY)).unwrap_or_default();
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            if let Ok(h) = cc.window_handle()
                && let RawWindowHandle::Win32(w) = h.as_raw()
            {
                crate::system::MAIN_HWND.store(w.hwnd.get(), Ordering::SeqCst);
                // Still hidden here: eframe shows the window after the first frame.
                window::init(w.hwnd.get(), minimized);
                if let Some(pl) = &prefs.placement
                    && window::restore_placement(pl)
                    && pl.maximized
                    && !minimized
                {
                    cc.egui_ctx.send_viewport_cmd(ViewportCommand::Maximized(true));
                }
            }
        }
        if let Ok(mut c) = shared.ui_ctx.lock() {
            *c = Some(cc.egui_ctx.clone());
        }
        let tray = tray::create(&shared, &cc.egui_ctx);
        let mut ui = UiState { log_follow: true, ..Default::default() };
        ui.refresh_autostart();
        let cfg = shared.config_snapshot();
        let cfg_rev = shared.config_rev.load(Ordering::SeqCst);
        let seen_profile = cfg.active_profile;
        App {
            page: prefs.last_page,
            shared,
            tray,
            ui,
            prefs,
            applied_prefs: None,
            last_tray_sync: Instant::now() - Duration::from_secs(10),
            frames: 0,
            st: EngineState::default(),
            st_tick: u64::MAX,
            cfg_base: cfg.clone(),
            cfg,
            cfg_rev,
            rgb_error_sig: None,
            page_switched: 0.0,
            scrolled: false,
            kb_nav: false,
            nav_y: None,
            last_critical: false,
            rgb: RgbStatus::default(),
            hist: Arc::new(History::default()),
            hist_tick: u64::MAX,
            max_size_ppp: 0.0,
            was_shown: false,
            assets: assets::Assets::default(),
            seen_profile,
        }
    }

    /// A fan mode other than BIOS chosen (tile, profile pill, tray, hotkey, Intelligence page) while
    /// modes wait for the header identification opens the Identify prompt;
    /// the engine keeps every header on the BIOS meanwhile. A change made from the tray
    /// while the window was hidden is caught on the first frame shown.
    fn prompt_identify_on_mode_change(&mut self) {
        let active = self.cfg.active_profile;
        if active == self.seen_profile {
            return;
        }
        self.seen_profile = active;
        if !self.st.mode_gate.is_empty()
            && self.cfg.profile().preset != crate::config::Preset::Bios
            && self.st.identify.is_none()
            && self.ui.modal.is_none()
        {
            self.ui.modal = Some(Modal::IdentifyHeaders { started: false });
        }
    }

    fn apply_prefs(&mut self, ctx: &egui::Context) {
        if self.applied_prefs.as_ref() == Some(&self.prefs) {
            return;
        }
        let old = self.applied_prefs.clone();
        if old.as_ref().is_none_or(|o| o.theme != self.prefs.theme) {
            theme::apply(ctx, self.prefs.theme);
            #[cfg(debug_assertions)]
            if std::env::var_os("FANRGB_DEBUG_EXPAND").is_some() {
                // Dev aid (debug builds): mark widgets that widen their parent (layout overflow hunting).
                ctx.all_styles_mut(|s| s.debug.show_expand_width = true);
            }
        }
        let reduce = match self.prefs.motion {
            MotionPref::Auto => !anim::system_animations_enabled(),
            MotionPref::On => true,
            MotionPref::Off => false,
        };
        anim::set_reduce_motion(ctx, reduce);
        anim::set_spin_fans(ctx, self.prefs.spin_fans);
        theme::set_animation_time(ctx, reduce);
        let z = self.prefs.zoom.clamp(0.9, 1.5);
        if (ctx.zoom_factor() - z).abs() > 0.001 {
            ctx.set_zoom_factor(z);
        }
        self.applied_prefs = Some(self.prefs.clone());
    }

    fn navigate(&mut self, ctx: &egui::Context, page: Page) {
        if self.page != page {
            self.page = page;
            self.prefs.last_page = page;
            self.page_switched = ctx.input(|i| i.time);
        }
    }

    /// Tray tooltip, header line, profile checks and warning icon (cheap: native calls only on change).
    fn sync_tray(&mut self) {
        let Some(tray) = &mut self.tray else { return };
        if self.last_tray_sync.elapsed() < Duration::from_millis(900) {
            return;
        }
        self.last_tray_sync = Instant::now();
        let (tip, header) = tray::texts(&self.st, &self.cfg);
        let state = match self.ui.issues.max_severity() {
            Some(Severity::Critical) => 2,
            Some(Severity::Error) | Some(Severity::Warning) if self.ui.issues.badge_count() > 0 => 1,
            _ => 0,
        };
        tray.sync(&self.cfg, &tip, &header, state);
    }

    /// Background helpers the UI waits for: the winget installer and the autostart query.
    fn poll_helpers(&mut self, ctx: &egui::Context) {
        if let Some(child) = &mut self.ui.winget {
            match child.try_wait() {
                Ok(Some(code)) => {
                    self.ui.winget = None;
                    self.ui.winget_done = true;
                    if code == 0 {
                        self.ui.toast_action(
                            ToastKind::Success,
                            "PawnIO installed. Restart FanRGB to use it.",
                            "Restart",
                            UiAction::RestartApp,
                        );
                    } else {
                        self.ui.toast(
                            ToastKind::Warning,
                            format!("The PawnIO installer exited with code {code}. See its console window."),
                        );
                    }
                }
                Ok(None) => ctx.request_repaint_after(Duration::from_millis(500)),
                Err(e) => {
                    log::warn!("winget: {e}");
                    self.ui.winget = None;
                }
            }
        }
        // App self-update: first/daily checks, helper process, verification and install jobs.
        if self.ui.updates.tick(self.cfg.updates.auto_check) {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        if let Some(rx) = &self.ui.autostart_rx {
            if let Ok(st) = rx.try_recv() {
                self.ui.autostart_st = Some(st);
                self.ui.autostart_rx = None;
            } else {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }
    }

    /// Caps the window at `window::MAX_SIDE_PX` physical pixels (the Intel Arc GL driver crashes at
    /// 16384 px). The subclass clamps every resize as well; this makes maximize/drag respect it.
    fn limit_window_size(&mut self, ctx: &egui::Context) {
        let ppp = ctx.pixels_per_point();
        if (ppp - self.max_size_ppp).abs() > 0.001 {
            self.max_size_ppp = ppp;
            let side = window::MAX_SIDE_PX / ppp.max(0.5);
            ctx.send_viewport_cmd(ViewportCommand::MaxInnerSize(vec2(side, side)));
        }
    }

    // ---- data snapshots (no per-frame deep clones) ----
    fn refresh_snapshots(&mut self) {
        // try_read: a worker stuck inside a hung driver call must never freeze the window.
        if let Ok(s) = self.shared.state.try_read()
            && (s.tick != self.st_tick || self.st_tick == u64::MAX)
        {
            self.st = s.clone();
            self.st_tick = s.tick;
        }
        if let Ok(r) = self.shared.rgb.try_read() {
            // Deep copy (preview strips, zone list) only when the lighting thread published.
            if r.rev != self.rgb.rev || r.rev == 0 {
                self.rgb = r.clone();
            }
        }
        if self.st_tick != self.hist_tick
            && let Ok(h) = self.shared.history.try_read()
        {
            self.hist = Arc::new(History { times: h.times.clone(), series: h.series.clone() });
            self.hist_tick = self.st_tick;
        }
        let rev = self.shared.config_rev.load(Ordering::SeqCst);
        if rev != self.cfg_rev {
            self.cfg = self.shared.config_snapshot();
            self.cfg_base = self.cfg.clone();
            self.cfg_rev = rev;
        }
    }

    fn commit_config(&mut self) {
        if self.cfg == self.cfg_base {
            return;
        }
        let mut cfg = self.cfg.clone();
        cfg.sanitize();
        let base = &self.cfg_base;
        self.shared.edit_config(|c| {
            // Keep calibrations the engine stored meanwhile.
            let engine_cals = std::mem::take(&mut c.calibrations);
            let (engine_roles, engine_names) = (c.fan_roles.clone(), c.fan_names.clone());
            let engine_modes = (c.profiles.clone(), c.active_profile);
            *c = cfg.clone();
            for (k, v) in engine_cals {
                c.calibrations.entry(k).or_insert(v);
            }
            // Header roles / names the identification wizard wrote meanwhile win over this UI's stale
            // copy, unless the UI changed that header itself.
            merge_engine_side(&mut c.fan_roles, &engine_roles, &base.fan_roles, &cfg.fan_roles);
            merge_engine_side(&mut c.fan_names, &engine_names, &base.fan_names, &cfg.fan_names);
            // A fan mode picked from the tray / hotkey since this UI's snapshot wins unless the UI
            // changed the mode itself: profiles and the active one, three-way.
            let (ep, ea) = engine_modes;
            if ep != base.profiles && cfg.profiles == base.profiles {
                c.profiles = ep;
            }
            if ea != base.active_profile && cfg.active_profile == base.active_profile {
                c.active_profile = ea;
            }
            if c.active_profile >= c.profiles.len() {
                c.active_profile = 0;
            }
        });
        self.cfg = self.shared.config_snapshot();
        self.cfg_base = self.cfg.clone();
        self.cfg_rev = self.shared.config_rev.load(Ordering::SeqCst);
    }

    fn update_issues(&mut self, rgb: &RgbStatus) {
        let uses_music = self.cfg.rgb.enabled
            && if self.cfg.rgb.sync {
                self.cfg.rgb.global.kind == EffectKind::Music
            } else {
                self.cfg.rgb.zones.values().any(|z| z.effect.kind == EffectKind::Music)
            };
        // Demo drives a simulated controller: the real device's USB health (reported by the engine)
        // is not what the user is looking at.
        let alerts: Vec<crate::model::Alert> = if rgb.simulated {
            self.st.alerts.iter().filter(|a| a.code != Some(ErrorCode::RgbHung)).cloned().collect()
        } else {
            self.st.alerts.clone()
        };
        let engine_hung = alerts.iter().any(|a| a.code == Some(ErrorCode::RgbHung));
        let rgb_list = issues::rgb_issues(rgb, uses_music, engine_hung);
        let sig = format!("{}|{:?}", self.st_tick, rgb_list);
        if self.rgb_error_sig.as_deref() == Some(sig.as_str()) {
            return;
        }
        self.rgb_error_sig = Some(sig);
        let transitions = self.ui.issues.update(&alerts, &rgb_list);
        for t in transitions {
            match t {
                Transition::Appeared(i) => match i.severity() {
                    Severity::Critical | Severity::Error => {
                        self.ui.toast_code(ToastKind::Error, i.title.clone(), i.code)
                    }
                    Severity::Warning if matches!(i.code, ErrorCode::RgbWriteFailed | ErrorCode::RgbDisconnected) => {
                        self.ui.toast_code(ToastKind::Warning, i.title.clone(), i.code)
                    }
                    _ => {}
                },
                Transition::Resolved(i) => {
                    if i.severity() > Severity::Info {
                        self.ui.toast(ToastKind::Success, format!("Resolved: {}", i.title));
                    } else if i.code == ErrorCode::RgbNotFound {
                        self.ui.toast(ToastKind::Success, "RGB controller connected");
                    }
                }
            }
        }
        let critical = self.st.alerts.iter().any(|a| a.code == Some(ErrorCode::CriticalTemperature));
        if critical != self.last_critical {
            self.last_critical = critical;
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let cmd = Modifiers::COMMAND;
        for page in Page::ALL {
            if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(cmd, page.key()))) {
                self.ui.pending.push(UiAction::Navigate(page));
            }
        }
        let sc = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(m, k)));
        if sc(cmd, Key::Comma) {
            self.ui.pending.push(UiAction::Navigate(Page::Settings));
        }
        if sc(cmd, Key::K) {
            self.ui.pending.push(UiAction::OpenPalette);
        }
        if sc(cmd, Key::P) {
            self.ui.pending.push(UiAction::OpenProfileMenu);
        }
        if sc(cmd | Modifiers::SHIFT, Key::B) {
            self.ui.pending.push(UiAction::EmergencyRelease);
        }
        // Fan modes: Alt+1..6 in tile order, Ctrl+Shift+M cycles.
        let mode_keys = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6];
        for (k, preset) in mode_keys.iter().zip(crate::config::Preset::MODES) {
            if sc(Modifiers::ALT, *k) {
                self.ui.pending.push(UiAction::ApplyPreset(preset));
            }
        }
        if sc(cmd | Modifiers::SHIFT, Key::M) {
            self.ui.pending.push(UiAction::CyclePreset);
        }
        if sc(cmd, Key::F) {
            self.ui.pending.push(UiAction::FocusSearch);
        }
        if sc(Modifiers::NONE, Key::F1) {
            self.ui.pending.push(UiAction::ShowShortcuts);
        }
        let (tab, pressed) = ctx.input(|i| (i.key_pressed(Key::Tab), i.pointer.any_pressed()));
        if tab {
            self.kb_nav = true;
        }
        if pressed {
            self.kb_nav = false;
        }
    }

    fn apply_actions(&mut self, ctx: &egui::Context) {
        let actions = std::mem::take(&mut self.ui.pending);
        for a in actions {
            match a {
                UiAction::Navigate(p) => self.navigate(ctx, p),
                UiAction::SelectFan(id) => {
                    self.ui.selected_fan = Some(id);
                    self.navigate(ctx, Page::Cooling);
                }
                UiAction::SelectZone(id) => {
                    self.ui.selected_zone = Some(id);
                    self.navigate(ctx, Page::Lighting);
                }
                UiAction::SetProfile(i) => {
                    if i < self.cfg.profiles.len() && i != self.cfg.active_profile {
                        let prev = self.cfg.active_profile;
                        self.cfg.active_profile = i;
                        self.cfg.general.welcome_done = true;
                        let name = self.cfg.profiles[i].name.clone();
                        self.ui.issues.log(issues::EventKind::Profile, None, format!("Profile: {name}"));
                        self.ui.toast_action(
                            ToastKind::Success,
                            format!("Switched to {name}"),
                            "Undo",
                            UiAction::RestoreProfile(prev),
                        );
                    }
                }
                UiAction::RestoreProfile(i) => {
                    if i < self.cfg.profiles.len() {
                        self.cfg.active_profile = i;
                    }
                }
                UiAction::ApplyPreset(_) | UiAction::CyclePreset => {
                    let prev = self.cfg.active_profile;
                    let preset = match a {
                        UiAction::ApplyPreset(p) => p,
                        _ => self.cfg.next_preset(),
                    };
                    self.cfg.apply_preset(preset);
                    self.cfg.general.welcome_done = true;
                    if self.st.emergency_released {
                        self.shared.send(Command::Resume);
                    }
                    let label = self.cfg.fan_mode().label;
                    self.ui.issues.log(issues::EventKind::Profile, None, format!("Fan mode: {label}"));
                    self.ui.toast_action(
                        ToastKind::Success,
                        format!("Fan mode: {label}"),
                        "Undo",
                        UiAction::RestoreProfile(prev),
                    );
                }
                UiAction::IdentifyHeaders => {
                    if self.ui.modal.is_none() {
                        self.ui.modal = Some(Modal::IdentifyHeaders { started: false });
                    }
                }
                UiAction::SetEffect(k) => {
                    self.cfg.rgb.global.kind = k;
                    self.navigate(ctx, Page::Lighting);
                }
                UiAction::EmergencyRelease => {
                    self.shared.send(Command::EmergencyRelease);
                    self.ui.issues.log(issues::EventKind::Emergency, None, "Fans returned to BIOS");
                    self.ui.toast_action(ToastKind::Info, "Fans returned to BIOS control", "Resume", UiAction::Resume);
                }
                UiAction::Resume => {
                    self.shared.send(Command::Resume);
                    self.ui.toast(ToastKind::Success, format!("Resumed {}", self.cfg.profile().name));
                }
                UiAction::WriteDiagnostics => {
                    self.shared.send(Command::WriteDiagnostics);
                    self.ui.toast(ToastKind::Info, "Writing diagnostics report…");
                }
                UiAction::CopyReport => {
                    let rgb = self.rgb.clone();
                    ctx.copy_text(issues::report_text(&self.st, &rgb, &self.ui.issues));
                    self.ui.toast(ToastKind::Success, "Report copied");
                }
                UiAction::ReconnectRgb => {
                    if self.rgb.hung_io_threads >= crate::rgb::link::MAX_STUCK {
                        // The lighting thread cannot start another I/O worker: say so instead of a spinner.
                        self.ui.toast_code(
                            ToastKind::Warning,
                            format!(
                                "{} requests to the RGB controller are stuck in the USB driver. Restart Windows (or shut down and drain power) to recover.",
                                self.rgb.hung_io_threads
                            ),
                            ErrorCode::RgbHung,
                        );
                    } else {
                        self.ui.rgb_reconnect = Some(Instant::now());
                    }
                    self.shared.send_rgb(crate::rgb::RgbCommand::Reconnect);
                }
                UiAction::ToggleLighting => self.cfg.rgb.enabled = !self.cfg.rgb.enabled,
                UiAction::OpenPalette => {
                    if self.ui.modal.is_none() {
                        self.ui.palette = Some(PaletteState::default());
                    }
                }
                UiAction::OpenProfileMenu => egui::Popup::open_id(ctx, Id::new("profile_popup")),
                UiAction::ShowShortcuts => self.ui.modal = Some(Modal::Shortcuts),
                UiAction::Fix(f, subject) => {
                    let st = std::mem::take(&mut self.st);
                    issues::run_fix(f, subject.as_deref(), &mut self.ui, &self.shared, &st, &self.cfg);
                    self.st = st;
                }
                UiAction::OpenLogFolder => {
                    let _ = crate::util::win::open_folder(&crate::util::paths::data_dir());
                }
                UiAction::SetTheme(t) => self.prefs.theme = t,
                UiAction::ResetSettings => {
                    // Elevated write into the user's %APPDATA%: only through fsx (no links followed,
                    // atomic replace), never a plain fs::write.
                    let backup = config_backup_path();
                    let saved = serde_json::to_vec_pretty(&self.cfg)
                        .map_err(std::io::Error::other)
                        .and_then(|b| crate::util::fsx::write_atomic(&backup, &b));
                    let cals = std::mem::take(&mut self.cfg.calibrations);
                    self.cfg = Config::default();
                    self.cfg.calibrations = cals;
                    self.ui.issues.log(issues::EventKind::Info, None, "Settings reset to defaults");
                    match saved {
                        Ok(()) => self.ui.toast(ToastKind::Success, "Settings reset · backup saved as config.json.bad"),
                        Err(e) => {
                            log::warn!("settings backup {} failed: {e}", backup.display());
                            self.ui.toast(ToastKind::Warning, "Settings reset · the backup could not be written")
                        }
                    }
                }
                UiAction::RestartApp => restart_app(&mut self.ui, &self.shared),
                UiAction::InstallPawnIo => self.ui.modal = Some(Modal::InstallPawnIo),
                UiAction::FocusSearch => {
                    if matches!(self.page, Page::Sensors | Page::Settings) {
                        self.ui.focus_search = true;
                    } else {
                        self.ui.pending.push(UiAction::OpenPalette);
                    }
                }
                UiAction::Calibrate(fan) => {
                    if let Some(f) = self.st.fans.iter().find(|f| f.index == fan) {
                        self.ui.modal = Some(Modal::Calibrate { fan, name: f.name.clone(), started: false });
                    }
                }
                UiAction::CheckForUpdates => {
                    self.ui.updates.check_now();
                    if let Some(svc) = crate::drivers::service() {
                        svc.check_now();
                    }
                    self.ui.toast(ToastKind::Info, "Checking for updates…");
                }
                UiAction::InstallUpdate => {
                    if self.ui.updates.state().status == crate::updater::Status::Available {
                        self.ui.updates.install(&self.shared);
                    }
                }
                UiAction::ShowWhatsNew => {
                    if self.ui.modal.is_none() {
                        self.ui.modal = Some(Modal::WhatsNew);
                    }
                }
                UiAction::CalibrateColours(zone) => {
                    self.ui.selected_zone = Some(zone);
                    self.prefs.lighting_tab = 1;
                    self.navigate(ctx, Page::Lighting);
                }
                UiAction::OpenDynamicLightingSettings => {
                    let _ = crate::util::win::open_url(crate::rgb::dynlight::SETTINGS_URI);
                }
            }
        }
    }

    // ---------------------------------------------------------------------------------------
    // Sidebar

    fn sidebar(&mut self, ctx: &egui::Context, bp: Breakpoint) {
        let p = pal_ctx(ctx);
        let rail = bp.rail();
        let w = anim::tween(ctx, Id::new("sidebar_w"), bp.sidebar_width(), anim::BASE);
        egui::SidePanel::left("nav")
            .exact_width(w)
            .resizable(false)
            .show_separator_line(true)
            .frame(egui::Frame::new().fill(p.sidebar).inner_margin(Margin { left: 8, right: 8, top: 10, bottom: 10 }))
            .show(ctx, |ui| {
                let rail = rail || w < 140.0;
                // Logo
                ui.horizontal(|ui| {
                    ui.set_min_height(44.0);
                    if !rail {
                        ui.add_space(8.0);
                    } else {
                        ui.add_space((ui.available_width() - 28.0) / 2.0);
                    }
                    let (r, _) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::hover());
                    match self.assets.app_icon(ctx) {
                        Some(t) => {
                            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                            ui.painter().image(t, r, uv, egui::Color32::WHITE);
                        }
                        None => {
                            ui.painter().circle_filled(r.center(), 14.0, p.accent_subtle);
                            icons::paint_fan(ui.painter(), r.shrink(6.0), 0.0, p.accent);
                        }
                    }
                    if !rail {
                        ui.add_space(4.0);
                        text::label(ui, "FanRGB", Ts::Headline, p.text1);
                    }
                });
                ui.add_space(12.0);
                let badge = self.ui.issues.badge_count();
                let max_sev = self.ui.issues.max_severity();
                let mut target_y = None;
                let updates_badge = pages::updates::badge_count(&self.ui);
                for page in Page::ALL {
                    if page == Page::Updates {
                        ui.add_space(6.0);
                        widgets::card::divider(ui);
                        ui.add_space(6.0);
                    }
                    let selected = self.page == page;
                    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::click());
                    let hover = anim::toggle(ctx, resp.id.with("h"), resp.hovered(), anim::FAST);
                    if selected {
                        ui.painter().rect_filled(rect, R_MD, p.accent_subtle);
                        target_y = Some(rect.center().y);
                    } else if hover > 0.0 {
                        ui.painter().rect_filled(rect, R_MD, with_alpha(p.surface2, hover));
                    }
                    let icon_c = if selected {
                        p.accent
                    } else if resp.hovered() {
                        p.text1
                    } else {
                        p.text2
                    };
                    let label_c = if selected || resp.hovered() { p.text1 } else { p.text2 };
                    let icon_x = if rail { rect.center().x } else { rect.left() + 22.0 };
                    icons::paint(
                        ui.painter(),
                        Rect::from_center_size(pos2(icon_x, rect.center().y), vec2(20.0, 20.0)),
                        page.icon(),
                        icon_c,
                    );
                    if !rail {
                        ui.painter().text(
                            pos2(rect.left() + 42.0, rect.center().y),
                            Align2::LEFT_CENTER,
                            page.label(),
                            fonts::font(Ts::Label),
                            label_c,
                        );
                        let badged =
                            (page == Page::Diagnostics && badge > 0) || (page == Page::Updates && updates_badge > 0);
                        if resp.hovered() && !badged {
                            ui.painter().text(
                                pos2(rect.right() - 10.0, rect.center().y),
                                Align2::RIGHT_CENTER,
                                page.shortcut(),
                                fonts::font(Ts::Caption),
                                p.text3,
                            );
                        }
                    }
                    if page == Page::Updates && updates_badge > 0 {
                        // Informational count (app update + driver offers): a subtle info pill.
                        let c = if rail {
                            pos2(rect.center().x + 11.0, rect.center().y - 9.0)
                        } else {
                            pos2(rect.right() - 18.0, rect.center().y)
                        };
                        let r = if rail { 7.0 } else { 10.0 };
                        ui.painter().circle_filled(c, r, p.info_subtle);
                        ui.painter().text(
                            c,
                            Align2::CENTER_CENTER,
                            updates_badge.to_string(),
                            fonts::tab(Ts::Caption),
                            p.info,
                        );
                    }
                    if page == Page::Diagnostics && badge > 0 {
                        let color = if max_sev >= Some(Severity::Error) { p.danger } else { p.warning };
                        let c = if rail {
                            pos2(rect.center().x + 11.0, rect.center().y - 9.0)
                        } else {
                            pos2(rect.right() - 18.0, rect.center().y)
                        };
                        let r = if rail { 7.0 } else { 9.0 };
                        ui.painter().circle_filled(c, r, color);
                        ui.painter().text(
                            c,
                            Align2::CENTER_CENTER,
                            badge.to_string(),
                            fonts::tab(Ts::Caption),
                            if p.dark { p.on_accent } else { egui::Color32::WHITE },
                        );
                    }
                    let resp = if rail {
                        resp.on_hover_text(format!("{}  ({})", page.label(), page.shortcut()))
                    } else {
                        resp
                    };
                    if resp.clicked() {
                        self.ui.pending.push(UiAction::Navigate(page));
                    }
                    let label = page.label();
                    resp.widget_info(|| {
                        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, label)
                    });
                }
                // sliding selection indicator
                if let Some(y) = target_y {
                    let yy = anim::tween(ctx, Id::new("nav_indicator"), y, anim::BASE);
                    self.nav_y = Some(yy);
                    let x = ui.min_rect().left() - 8.0;
                    ui.painter().rect_filled(Rect::from_center_size(pos2(x + 1.5, yy), vec2(3.0, 16.0)), 2, p.accent);
                }

                // Health footer (click → Diagnostics)
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    let info = &self.st.info;
                    let rgb_ok = self.rgb.device.is_some();
                    let rgb_hung = self.ui.issues.has(ErrorCode::RgbHung);
                    let fan_state = if !info.fan_control {
                        (p.danger, "Unavailable")
                    } else if self.st.emergency_released {
                        (p.warning, "BIOS")
                    } else if self.st.fans.iter().any(|f| f.controlled) {
                        (p.success, "Active")
                    } else {
                        (p.text3, "BIOS")
                    };
                    let light_state = if rgb_hung {
                        (p.danger, "Not responding")
                    } else if rgb_ok {
                        (p.success, if self.cfg.rgb.enabled { "Connected" } else { "Off" })
                    } else {
                        (p.text3, "No controller")
                    };
                    let drv = match &info.pawnio {
                        crate::hw::pawnio::PawnIoStatus::Ready => {
                            (p.success, if info.simulated { "Simulated" } else { "Ready" })
                        }
                        crate::hw::pawnio::PawnIoStatus::NotInstalled => (p.danger, "Not installed"),
                        crate::hw::pawnio::PawnIoStatus::AccessDenied => (p.warning, "Needs admin"),
                        crate::hw::pawnio::PawnIoStatus::Error(_) => (p.danger, "Error"),
                        crate::hw::pawnio::PawnIoStatus::Unknown => (p.text3, "Checking…"),
                    };
                    let rows = [("Driver", drv), ("Lighting", light_state), ("Fan control", fan_state)];
                    let mut update_chip: Option<Rect> = None;
                    let h = if rail { 3.0 * 18.0 + 8.0 } else { 3.0 * 26.0 + 30.0 };
                    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
                    if resp.hovered() {
                        ui.painter().rect_filled(rect, R_MD, p.surface2);
                    }
                    let painter = ui.painter();
                    if rail {
                        for (k, (_, (c, _))) in rows.iter().enumerate() {
                            painter.circle_filled(
                                pos2(rect.center().x, rect.bottom() - 12.0 - k as f32 * 18.0),
                                4.0,
                                *c,
                            );
                        }
                    } else {
                        painter.text(
                            pos2(rect.left() + 10.0, rect.bottom() - 10.0),
                            Align2::LEFT_BOTTOM,
                            format!("v{}", env!("CARGO_PKG_VERSION")),
                            fonts::font(Ts::Caption),
                            p.text3,
                        );
                        // "● 0.3.0 available": click → Updates (handled below).
                        if let Some(v) = pages::updates::available_version(&self.ui) {
                            let g = painter.layout_no_wrap(format!("{v} available"), fonts::font(Ts::Caption), p.info);
                            let chip = Rect::from_min_size(
                                pos2(rect.left() + 58.0, rect.bottom() - 30.0),
                                vec2(g.size().x + 26.0, 22.0),
                            );
                            painter.rect_filled(chip, R_PILL, p.info_subtle);
                            painter.circle_filled(pos2(chip.left() + 10.0, chip.center().y), 3.0, p.info);
                            painter.galley(pos2(chip.left() + 18.0, chip.center().y - g.size().y / 2.0), g, p.info);
                            update_chip = Some(chip);
                        }
                        for (k, (name, (c, v))) in rows.iter().enumerate() {
                            let y = rect.bottom() - 40.0 - k as f32 * 26.0;
                            painter.circle_filled(pos2(rect.left() + 14.0, y), 4.0, *c);
                            painter.text(
                                pos2(rect.left() + 26.0, y),
                                Align2::LEFT_CENTER,
                                *name,
                                fonts::font(Ts::Caption),
                                p.text2,
                            );
                            painter.text(
                                pos2(rect.right() - 8.0, y),
                                Align2::RIGHT_CENTER,
                                *v,
                                fonts::font(Ts::Caption),
                                p.text3,
                            );
                        }
                    }
                    let on_chip = update_chip.is_some_and(|c| resp.hover_pos().is_some_and(|p| c.contains(p)));
                    let resp = resp.on_hover_text(if on_chip { "Open Updates" } else { "Open Diagnostics" });
                    if resp.clicked() {
                        let target = if on_chip { Page::Updates } else { Page::Diagnostics };
                        self.ui.pending.push(UiAction::Navigate(target));
                    }
                    if info.simulated {
                        ui.add_space(4.0);
                        if rail {
                            let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::hover());
                            ui.painter().rect_filled(
                                Rect::from_center_size(r.center(), vec2(40.0, 18.0)),
                                R_PILL,
                                p.warning_subtle,
                            );
                            ui.painter().text(
                                r.center(),
                                Align2::CENTER_CENTER,
                                "DEMO",
                                fonts::font(Ts::Overline),
                                p.warning,
                            );
                        } else {
                            ui.horizontal(|ui| {
                                ui.add_space(6.0);
                                controls::dot_chip(ui, "Demo data", controls::Tone::Warning)
                                    .on_hover_text("Started with --demo: all sensors and fans are simulated");
                            });
                        }
                    }
                });
            });
    }

    // ---------------------------------------------------------------------------------------
    // Top bar

    fn topbar(&mut self, ctx: &egui::Context, bp: Breakpoint) {
        let p = pal_ctx(ctx);
        let gutter = bp.gutter() as i8;
        egui::TopBottomPanel::top("top")
            .exact_height(theme::TOPBAR_H)
            .show_separator_line(self.scrolled)
            .frame(egui::Frame::new().fill(p.canvas).inner_margin(Margin {
                left: gutter,
                right: gutter,
                top: 0,
                bottom: 0,
            }))
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    // Title / breadcrumb
                    let crumb = match self.page {
                        Page::Cooling => self
                            .ui
                            .selected_fan
                            .as_ref()
                            .and_then(|id| self.st.fans.iter().find(|f| &f.id == id))
                            .map(|f| f.name.clone()),
                        Page::Lighting => self
                            .ui
                            .selected_zone
                            .as_ref()
                            .and_then(|id| crate::rgb::standard_zones().into_iter().find(|z| &z.id == id))
                            .map(|z| z.name.split(" · ").next().unwrap_or("").to_string()),
                        _ => None,
                    };
                    // Narrow windows: just the page title (the breadcrumb would crowd the controls).
                    match crumb.filter(|_| bp != Breakpoint::Narrow) {
                        Some(c) => {
                            let r = ui.add(
                                egui::Label::new(text::rt(self.page.label(), Ts::Title, p.text2))
                                    .sense(Sense::click())
                                    .selectable(false),
                            );
                            if r.clicked() && self.page == Page::Lighting {
                                self.ui.selected_zone = None;
                            }
                            text::label(ui, "›", Ts::Title, p.text3);
                            text::label(ui, &c, Ts::Title, p.text1);
                        }
                        None => {
                            text::label(ui, self.page.label(), Ts::Title, p.text1);
                        }
                    }
                    if self.st.info.simulated {
                        ui.add_space(4.0);
                        controls::chip(ui, "DEMO", controls::Tone::Warning);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        // search
                        let narrow = bp == Breakpoint::Narrow;
                        let sr = if narrow {
                            controls::icon_button(ui, Icon::Search, "Command palette (Ctrl+K)")
                        } else {
                            search_button(ui)
                        };
                        if sr.clicked() {
                            self.ui.pending.push(UiAction::OpenPalette);
                        }
                        // issues
                        self.issues_button(ui);
                        // temps
                        if !(narrow && self.page == Page::Overview) {
                            let f = self.cfg.general.fahrenheit;
                            for (label, id, crit) in [
                                ("GPU", ids::GPU_TEMP, self.cfg.general.gpu_critical),
                                ("CPU", ids::CPU_TEMP, self.cfg.general.cpu_critical),
                            ] {
                                let v = self.st.value(id);
                                temp_chip(ui, label, v, f, crit);
                            }
                        }
                        self.profile_pill(ui);
                    });
                });
            });
    }

    fn issues_button(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui);
        let count = self.ui.issues.badge_count();
        let sev = self.ui.issues.max_severity();
        let (icon, color) = match sev {
            Some(Severity::Critical) | Some(Severity::Error) => (Icon::Error, p.danger),
            Some(Severity::Warning) => (Icon::Warning, p.warning),
            _ => (Icon::CheckCircle, p.text2),
        };
        let label = if count > 0 { count.to_string() } else { String::new() };
        let g = ui.painter().layout_no_wrap(label.clone(), fonts::tab(Ts::Label), color);
        let w = 32.0 + if count > 0 { g.size().x + 4.0 } else { 0.0 };
        let (rect, resp) = ui.allocate_exact_size(vec2(w, 32.0), Sense::click());
        let fill = if count > 0 && sev >= Some(Severity::Error) {
            p.danger_subtle
        } else if count > 0 {
            p.warning_subtle
        } else if resp.hovered() {
            p.surface2
        } else {
            egui::Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, R_MD, fill);
        icons::paint(
            ui.painter(),
            Rect::from_center_size(pos2(rect.left() + 16.0, rect.center().y), vec2(18.0, 18.0)),
            icon,
            color,
        );
        if count > 0 {
            ui.painter().galley(pos2(rect.left() + 28.0, rect.center().y - g.size().y / 2.0), g, color);
        }
        let resp = resp.on_hover_text(if count == 0 {
            "No active issues".to_string()
        } else {
            format!("{count} active issue{}", if count > 1 { "s" } else { "" })
        });
        let st = std::mem::take(&mut self.st);
        egui::Popup::from_toggle_button_response(&resp)
            .id(Id::new("issues_popup"))
            .width(380.0)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                ui.set_width(380.0);
                let p = pal(ui);
                text::label(ui, "Issues", Ts::Headline, p.text1);
                ui.add_space(4.0);
                let list: Vec<issues::Issue> = self.ui.issues.visible().cloned().collect();
                if list.is_empty() {
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                        icons::paint(ui.painter(), r, Icon::CheckCircle, p.success);
                        text::label(ui, "Everything is working.", Ts::Body, p.text2);
                    });
                }
                for i in list.iter().take(6) {
                    let sev = widgets::card::Sev::from_severity(i.severity());
                    let (_, strong) = controls::tone_colors(p, sev.tone());
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                        icons::paint(ui.painter(), r, sev.icon(), strong);
                        ui.vertical(|ui| {
                            ui.set_max_width(250.0);
                            ui.spacing_mut().item_spacing.y = 2.0;
                            text::para(ui, &i.title, Ts::BodyStrong, p.text1);
                            text::label(ui, i.code.id(), Ts::Mono, p.text3);
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let fix = i.code.fix();
                            if fix != crate::error::FixAction::None
                                && Button::new(Kind::Secondary, fix.label()).size(controls::Size::Sm).show(ui).clicked()
                            {
                                self.ui.pending.push(UiAction::Fix(fix, i.subject.clone()));
                                ui.close();
                            }
                        });
                    });
                    ui.add_space(6.0);
                }
                widgets::card::divider(ui);
                ui.add_space(2.0);
                if controls::link(ui, "Open Diagnostics →").clicked() {
                    self.ui.pending.push(UiAction::Navigate(Page::Diagnostics));
                    ui.close();
                }
            });
        self.st = st;
    }

    fn profile_pill(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui);
        let prof = self.cfg.profile();
        let mode_color = match prof.preset {
            crate::config::Preset::AiAuto => p.accent,
            crate::config::Preset::Bios => p.text3,
            crate::config::Preset::FullSpeed => p.warning,
            _ => p.text2,
        };
        let name = if self.st.emergency_released { "BIOS (released)".to_string() } else { self.cfg.fan_mode().label };
        let g = ui.painter().layout_no_wrap(name, fonts::font(Ts::Label), p.text1);
        let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 54.0, 32.0), Sense::click());
        let fill = if resp.hovered() { theme::mix(p.surface2, p.surface3, 0.6) } else { p.surface2 };
        ui.painter().rect_filled(rect, R_PILL, fill);
        ui.painter().rect_stroke(rect, R_PILL, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
        ui.painter().circle_filled(pos2(rect.left() + 14.0, rect.center().y), 4.0, mode_color);
        ui.painter().galley(pos2(rect.left() + 24.0, rect.center().y - g.size().y / 2.0), g, p.text1);
        icons::paint(
            ui.painter(),
            Rect::from_center_size(pos2(rect.right() - 16.0, rect.center().y), vec2(14.0, 14.0)),
            Icon::ChevronDown,
            p.text2,
        );
        let resp = resp.on_hover_text("Fan profile (Ctrl+P)");
        let profiles: Vec<(usize, String, crate::config::Preset)> = self
            .cfg
            .profiles
            .iter()
            .enumerate()
            .map(|(i, pr)| {
                // Built-in modes by their display label ("Auto (AI)"), custom profiles by name.
                let own = pr.preset != crate::config::Preset::Custom && self.cfg.preset_profile(pr.preset) == Some(i);
                (i, if own { pr.preset.label().to_string() } else { pr.name.clone() }, pr.preset)
            })
            .collect();
        let active = self.cfg.active_profile;
        egui::Popup::from_toggle_button_response(&resp).id(Id::new("profile_popup")).width(300.0).show(|ui| {
            let p = pal(ui);
            ui.set_width(300.0);
            for (i, name, preset) in &profiles {
                let (rect, r) = ui.allocate_exact_size(vec2(300.0, 48.0), Sense::click());
                if r.hovered() {
                    ui.painter().rect_filled(rect, R_MD, p.surface2);
                }
                ui.painter().text(
                    pos2(rect.left() + 12.0, rect.top() + 15.0),
                    Align2::LEFT_CENTER,
                    name,
                    fonts::font(Ts::BodyStrong),
                    p.text1,
                );
                ui.painter().text(
                    pos2(rect.left() + 12.0, rect.top() + 33.0),
                    Align2::LEFT_CENTER,
                    pages::profile_description(*preset),
                    fonts::font(Ts::Caption),
                    p.text2,
                );
                if *i == active && !self.st.emergency_released {
                    icons::paint(
                        ui.painter(),
                        Rect::from_center_size(pos2(rect.right() - 18.0, rect.center().y), vec2(16.0, 16.0)),
                        Icon::Check,
                        p.accent,
                    );
                }
                if r.clicked() {
                    self.ui.pending.push(UiAction::SetProfile(*i));
                    if self.st.emergency_released {
                        self.ui.pending.push(UiAction::Resume);
                    }
                    ui.close();
                }
            }
            widgets::card::divider(ui);
            if Button::new(Kind::Ghost, "Edit profiles…").icon(Icon::Pencil).min_width(300.0).show(ui).clicked() {
                self.ui.pending.push(UiAction::Navigate(Page::Cooling));
                ui.close();
            }
            if Button::new(Kind::Danger, "Return fans to BIOS")
                .icon(Icon::Power)
                .min_width(300.0)
                .show(ui)
                .on_hover_text("Ctrl+Shift+B")
                .clicked()
            {
                self.ui.pending.push(UiAction::EmergencyRelease);
                ui.close();
            }
        });
    }

    fn critical_banner(&mut self, ctx: &egui::Context, bp: Breakpoint) {
        let Some(issue) = self.ui.issues.get(ErrorCode::CriticalTemperature).cloned() else { return };
        let p = pal_ctx(ctx);
        let gutter = bp.gutter() as i8;
        egui::TopBottomPanel::top("critical")
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(p.canvas).inner_margin(Margin {
                left: gutter,
                right: gutter,
                top: 0,
                bottom: 8,
            }))
            .show(ctx, |ui| {
                let where_ = ui.painter().add(egui::Shape::Noop);
                let r =
                    egui::Frame::new().inner_margin(Margin { left: 14, right: 10, top: 8, bottom: 8 }).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let (ir, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
                            let pulse = if anim::reduce_motion(ctx) {
                                1.0
                            } else {
                                0.75 + 0.25 * (ui.input(|i| i.time) as f32 * 5.0).cos()
                            };
                            icons::paint(ui.painter(), ir, Icon::Critical, with_alpha(egui::Color32::WHITE, pulse));
                            let cpu = self.st.value(ids::CPU_TEMP);
                            let msg = format!(
                                "Critical temperature · CPU {} · all controlled fans at 100 % until it drops below {}",
                                widgets::fmt_temp(cpu, self.cfg.general.fahrenheit),
                                // The latch's real release point (it said critical − 8 °C, the
                                // calibration margin; FRG-E301 releases at critical − 5 °C).
                                widgets::fmt_temp(
                                    Some(crate::control::limits::critical_release_c(self.cfg.general.cpu_critical)),
                                    self.cfg.general.fahrenheit
                                )
                            );
                            text::label(ui, &msg, Ts::BodyStrong, egui::Color32::WHITE);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if controls::icon_button_sized(
                                    ui,
                                    Icon::Copy,
                                    "Copy details",
                                    26.0,
                                    Some(egui::Color32::WHITE),
                                )
                                .clicked()
                                {
                                    let rgb = self.rgb.clone();
                                    ctx.copy_text(issues::copy_details(&issue, &self.st, &rgb));
                                    self.ui.toast(ToastKind::Success, "Details copied");
                                }
                                text::label(ui, issue.code.id(), Ts::Mono, egui::Color32::from_white_alpha(220));
                            });
                        });
                    });
                let rect = r.response.rect;
                ui.painter().set(where_, egui::Shape::rect_filled(rect, R_MD, p.danger));
                ctx.request_repaint_after(Duration::from_millis(60));
            });
    }

    fn palette_entries(&self) -> Vec<PaletteEntry> {
        let mut v = Vec::new();
        for page in Page::ALL {
            v.push(PaletteEntry {
                icon: page.icon(),
                group: "Pages",
                title: page.label().into(),
                context: String::new(),
                hint: page.shortcut(),
                action: UiAction::Navigate(page),
            });
        }
        for f in self.st.fans.iter().filter(|f| f.visible) {
            v.push(PaletteEntry {
                icon: Icon::Cooling,
                group: "Fans",
                title: f.name.clone(),
                context: f.mode.clone(),
                hint: "",
                action: UiAction::SelectFan(f.id.clone()),
            });
        }
        for f in self.st.fans.iter().filter(|f| f.visible && f.calibration_refused().is_none()) {
            v.push(PaletteEntry {
                icon: Icon::Refresh,
                group: "Fans",
                title: format!("Calibrate {}", f.name),
                context: "measure RPM at every speed".into(),
                hint: "",
                action: UiAction::Calibrate(f.index),
            });
        }
        for z in crate::rgb::standard_zones().into_iter().filter(|z| !z.advanced) {
            v.push(PaletteEntry {
                icon: Icon::Led,
                group: "Lighting zones",
                title: z.name.clone(),
                context: String::new(),
                hint: "",
                action: UiAction::SelectZone(z.id.clone()),
            });
        }
        let mode = self.cfg.fan_mode().preset;
        for (n, preset) in crate::config::Preset::MODES.iter().enumerate() {
            v.push(PaletteEntry {
                icon: widgets::fan_modes::preset_icon(*preset),
                group: "Fan modes",
                title: format!("Fan mode: {}", preset.label()),
                context: if *preset == mode { "active".into() } else { preset.descriptor().into() },
                hint: ["Alt+1", "Alt+2", "Alt+3", "Alt+4", "Alt+5", "Alt+6"][n],
                action: UiAction::ApplyPreset(*preset),
            });
        }
        v.push(PaletteEntry {
            icon: Icon::Sun,
            group: "Fan modes",
            title: "Next fan mode".into(),
            context: self.cfg.next_preset().label().into(),
            hint: "Ctrl+Shift+M",
            action: UiAction::CyclePreset,
        });
        v.push(PaletteEntry {
            icon: Icon::Sun,
            group: "Fan modes",
            title: "Identify fan headers".into(),
            context: "what is connected where".into(),
            hint: "",
            action: UiAction::IdentifyHeaders,
        });
        for (i, pr) in self.cfg.profiles.iter().enumerate() {
            v.push(PaletteEntry {
                icon: Icon::Sun,
                group: "Profiles",
                title: format!("Profile: {}", pr.name),
                context: if i == self.cfg.active_profile { "active".into() } else { String::new() },
                hint: "",
                action: UiAction::SetProfile(i),
            });
        }
        for (_, kinds) in widgets::gallery::GROUPS {
            for k in kinds {
                v.push(PaletteEntry {
                    icon: Icon::Lighting,
                    group: "Effects",
                    title: format!("Effect: {}", k.label()),
                    context: String::new(),
                    hint: "",
                    action: UiAction::SetEffect(*k),
                });
            }
        }
        v.push(PaletteEntry {
            icon: Icon::Update,
            group: "Updates",
            title: "Check for updates".into(),
            context: "FanRGB and drivers".into(),
            hint: "",
            action: UiAction::CheckForUpdates,
        });
        if self.ui.updates.state().status == crate::updater::Status::Available {
            v.push(PaletteEntry {
                icon: Icon::Restart,
                group: "Updates",
                title: "Update & restart".into(),
                context: pages::updates::available_version(&self.ui).unwrap_or_default(),
                hint: "",
                action: UiAction::InstallUpdate,
            });
        }
        for z in crate::rgb::standard_zones().into_iter().filter(|z| !z.advanced) {
            let name = self.cfg.rgb.zones.get(&z.id).and_then(|c| c.name.clone()).unwrap_or_else(|| z.name.clone());
            v.push(PaletteEntry {
                icon: Icon::Pipette,
                group: "Lighting zones",
                title: format!("Colour calibration: {name}"),
                context: "match LED colours to the screen".into(),
                hint: "",
                action: UiAction::CalibrateColours(z.id.clone()),
            });
        }
        v.push(PaletteEntry {
            icon: Icon::Windows,
            group: "Actions",
            title: "Open Dynamic Lighting settings".into(),
            context: "Windows Settings".into(),
            hint: "",
            action: UiAction::OpenDynamicLightingSettings,
        });
        let acts: [(Icon, &str, &str, UiAction); 10] = [
            (Icon::Power, "Return fans to BIOS", "Ctrl+Shift+B", UiAction::EmergencyRelease),
            (Icon::Play, "Resume fan control", "", UiAction::Resume),
            (Icon::Report, "Write diagnostics report", "", UiAction::WriteDiagnostics),
            (Icon::Copy, "Copy report", "", UiAction::CopyReport),
            (Icon::Refresh, "Reconnect RGB controller", "", UiAction::ReconnectRgb),
            (Icon::Lighting, "Toggle lighting", "", UiAction::ToggleLighting),
            (Icon::Folder, "Open log folder", "", UiAction::OpenLogFolder),
            (Icon::Keyboard, "Keyboard shortcuts", "F1", UiAction::ShowShortcuts),
            (Icon::Moon, "Theme: dark", "", UiAction::SetTheme(ThemePref::Dark)),
            (Icon::Sun, "Theme: light", "", UiAction::SetTheme(ThemePref::Light)),
        ];
        for (icon, title, hint, action) in acts {
            v.push(PaletteEntry { icon, group: "Actions", title: title.into(), context: String::new(), hint, action });
        }
        v.push(PaletteEntry {
            icon: Icon::Settings,
            group: "Actions",
            title: "Theme: follow Windows".into(),
            context: String::new(),
            hint: "",
            action: UiAction::SetTheme(ThemePref::System),
        });
        if !self.st.info.fan_control || matches!(self.st.info.pawnio, crate::hw::pawnio::PawnIoStatus::NotInstalled) {
            v.push(PaletteEntry {
                icon: Icon::Download,
                group: "Actions",
                title: "Install PawnIO driver".into(),
                context: String::new(),
                hint: "",
                action: UiAction::InstallPawnIo,
            });
        }
        v
    }
}

fn search_button(ui: &mut egui::Ui) -> egui::Response {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(104.0, 32.0), Sense::click());
    ui.painter().rect_filled(rect, R_MD, if resp.hovered() { p.surface2 } else { egui::Color32::TRANSPARENT });
    ui.painter().rect_stroke(rect, R_MD, Stroke::new(1.0, p.border_subtle), egui::StrokeKind::Inside);
    icons::paint(
        ui.painter(),
        Rect::from_center_size(pos2(rect.left() + 16.0, rect.center().y), vec2(16.0, 16.0)),
        Icon::Search,
        p.text2,
    );
    ui.painter().text(
        pos2(rect.right() - 10.0, rect.center().y),
        Align2::RIGHT_CENTER,
        "Ctrl K",
        fonts::font(Ts::Caption),
        p.text3,
    );
    resp.on_hover_text("Command palette")
}

fn temp_chip(ui: &mut egui::Ui, label: &str, v: Option<f32>, f: bool, crit: f32) {
    let p = pal(ui);
    let val = widgets::fmt_deg(v, f);
    let g1 = ui.painter().layout_no_wrap(label.to_string(), fonts::font(Ts::Caption), p.text2);
    let g2 = ui.painter().layout_no_wrap(val, fonts::tab(Ts::Label), p.text1);
    let w = 12.0 + 8.0 + g1.size().x + 4.0 + g2.size().x.max(26.0) + 10.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 28.0), Sense::hover());
    let dot = v.map(|t| theme::thermal(p, t, crit)).unwrap_or(p.text3);
    ui.painter().circle_filled(pos2(rect.left() + 10.0, rect.center().y), 3.0, dot);
    ui.painter().galley(pos2(rect.left() + 18.0, rect.center().y - g1.size().y / 2.0), g1.clone(), p.text2);
    ui.painter().galley(pos2(rect.left() + 22.0 + g1.size().x, rect.center().y - g2.size().y / 2.0), g2, p.text1);
    let _ = resp.on_hover_text(format!("{label} temperature"));
}

/// Three-way merge of one settings map when the UI commits its copy: a key the engine changed since
/// the UI's snapshot (`base`), and the UI did not touch, keeps the engine's value.
fn merge_engine_side<V: Clone + PartialEq>(
    out: &mut std::collections::BTreeMap<String, V>,
    engine: &std::collections::BTreeMap<String, V>,
    base: &std::collections::BTreeMap<String, V>,
    ui: &std::collections::BTreeMap<String, V>,
) {
    let keys: std::collections::BTreeSet<&String> = engine.keys().chain(base.keys()).collect();
    for k in keys {
        let (e, b, u) = (engine.get(k), base.get(k), ui.get(k));
        if e != b && u == b {
            match e {
                Some(v) => {
                    out.insert(k.clone(), v.clone());
                }
                None => {
                    out.remove(k);
                }
            }
        }
    }
}

/// `config.json.bad` next to the active config file.
fn config_backup_path() -> std::path::PathBuf {
    crate::config::config_path().with_extension("json.bad")
}

/// Restarts the app through `system::restart_self` (no cmd.exe; the new instance waits for this one
/// to exit and is started unelevated unless this is the admin-only installed copy).
fn restart_app(ui: &mut UiState, shared: &Arc<Shared>) {
    match crate::system::restart_self() {
        Ok(()) => crate::system::request_exit(shared),
        Err(e) => {
            let code = crate::error::code_of(&e);
            ui.issues.log(issues::EventKind::Failure, Some(code), format!("Restart failed: {e}"));
            ui.toast_code(ToastKind::Error, format!("Could not restart FanRGB: {e}"), code)
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frames = self.frames.saturating_add(1);
        let quitting = self.shared.quit.load(Ordering::SeqCst);
        if quitting {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        } else if ctx.input(|i| i.viewport().close_requested()) {
            let to_tray = self.cfg.general.close_to_tray;
            if to_tray && self.tray.is_some() {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                // Minimize + tool-window style, never SW_HIDE (see window.rs).
                window::request_tray();
            } else {
                self.shared.quit.store(true, Ordering::SeqCst);
                self.shared.send(Command::Wake);
            }
        }
        window::repair_if_restored_externally();

        self.apply_prefs(ctx);
        self.refresh_snapshots();
        let rgb = std::mem::take(&mut self.rgb);
        self.update_issues(&rgb);
        self.rgb = rgb;
        self.sync_tray();
        self.poll_helpers(ctx);

        // Design textures not drawn for 30 s are freed (also while hidden: nothing is drawn then).
        self.assets.gc(Instant::now());
        let shown = window::shown();
        // The live LED preview feeds the Lighting page and the Overview hero (only while focused: an
        // unfocused Overview takes the 2 Hz preview the lighting thread publishes anyway).
        let hero_live = self.page == Page::Overview && self.prefs.show_hero && ctx.input(|i| i.focused);
        crate::rgb::set_preview_wanted(shown && (self.page == Page::Lighting || hero_live));
        if !(shown && self.page == Page::Lighting) && self.ui.calibration.is_some() && !self.ui.calibration_paused {
            // Leaving the calibration (other page, tray): the saved lighting comes back, the edits stay.
            self.shared.send_rgb(crate::rgb::RgbCommand::EndTest);
            self.ui.calibration_paused = true;
        }
        if !shown {
            // Minimized or in the tray: no layout and no animation. eframe still paints one (empty)
            // frame per wake-up, which is what lets its event loop idle. The engine wakes a hidden
            // window only when the alerts or the lighting's error state change (its tray signature),
            // so the tray icon's warning state stays current without a short timer; the fallback
            // covers anything else the issue list reads, the texture GC and the daily update check.
            // (The tooltip refreshes itself on hover, see `tray::refresh_on_hover`.)
            self.was_shown = false;
            if !quitting {
                ctx.request_repaint_after(HIDDEN_WAKE);
            }
            return;
        }
        if !self.was_shown {
            self.was_shown = true;
            // Data may be minutes old: no page fade on the first frame back.
            self.page_switched = f64::NEG_INFINITY;
        }
        self.limit_window_size(ctx);
        // Guard against a corrupted saved window size.
        if self.frames == 3
            && let Some(r) = ctx.input(|i| i.viewport().inner_rect)
            && (r.width() < 700.0 || r.height() < 450.0)
        {
            log::warn!("restored window was too small ({:.0}x{:.0}); resetting", r.width(), r.height());
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(vec2(1280.0, 820.0)));
        }

        self.handle_shortcuts(ctx);
        self.apply_actions(ctx);
        self.prompt_identify_on_mode_change();

        let bp = Breakpoint::from_window(ctx.content_rect().width());
        self.sidebar(ctx, bp);
        self.topbar(ctx, bp);
        self.critical_banner(ctx, bp);

        // ---- page ----
        let focused = ctx.input(|i| i.focused);
        let gutter = bp.gutter() as i8;
        let mut page = self.page;
        let hist = self.hist.clone();
        let fade = if anim::reduce_motion(ctx) {
            1.0
        } else {
            ((ctx.input(|i| i.time) - self.page_switched) as f32 / anim::SLOW).clamp(0.0, 1.0)
        };
        if fade < 1.0 {
            ctx.request_repaint();
        }
        let p = pal_ctx(ctx);
        let rgb = std::mem::take(&mut self.rgb);
        let out = egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(p.canvas).inner_margin(Margin { left: gutter, right: 4, top: 4, bottom: 0 }))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().id_salt(("page", self.page as u8)).auto_shrink([false, false]).show(
                    ui,
                    |ui| {
                        ui.set_opacity(anim::ease_out_cubic(fade));
                        // keep the right gutter inside the scroll area (scrollbar sits in the margin)
                        let w = ui.available_width() - (gutter as f32 - 4.0);
                        let max_w = 1480.0;
                        let content_w = w.min(max_w);
                        ui.set_max_width(content_w);
                        let mut pc = PageCtx {
                            shared: &self.shared,
                            st: &self.st,
                            rgb: &rgb,
                            hist: &hist,
                            cfg: &mut self.cfg,
                            ui: &mut self.ui,
                            prefs: &mut self.prefs,
                            page: &mut page,
                            bp,
                            focused,
                            assets: &mut self.assets,
                        };
                        match self.page {
                            Page::Overview => pages::overview::show(ui, &mut pc),
                            Page::Cooling => pages::cooling::show(ui, &mut pc),
                            Page::Lighting => pages::lighting::show(ui, &mut pc),
                            Page::Intelligence => pages::intelligence::show(ui, &mut pc),
                            Page::Sensors => pages::sensors::show(ui, &mut pc),
                            Page::Updates => pages::updates::show(ui, &mut pc),
                            Page::Diagnostics => pages::diagnostics::show(ui, &mut pc),
                            Page::Settings => pages::settings::show(ui, &mut pc),
                        }
                        ui.add_space(24.0);
                    },
                )
            });
        self.rgb = rgb;
        self.scrolled = out.inner.state.offset.y > 1.0;
        if page != self.page {
            self.navigate(ctx, page);
        }

        // ---- overlays ----
        if let Some(modal) = self.ui.modal.take() {
            let mut env = overlays::ModalEnv { shared: &self.shared, st: &self.st, cfg: &mut self.cfg };
            let keep = overlays::show_modal(ctx, &mut self.ui, &mut env, modal);
            if self.ui.modal.is_none() {
                self.ui.modal = keep;
            }
        }
        if let Some(mut ps) = self.ui.palette.take() {
            let entries = self.palette_entries();
            let (keep, picked) = overlays::show_palette(ctx, &mut ps, &entries);
            if keep {
                self.ui.palette = Some(ps);
            }
            if let Some(a) = picked {
                self.ui.pending.push(a);
            }
        }
        overlays::show_toasts(ctx, &mut self.ui);
        if self.kb_nav {
            overlays::paint_focus_ring(ctx);
        }
        // Actions raised by pages / overlays this frame.
        if !self.ui.pending.is_empty() {
            self.apply_actions(ctx);
            ctx.request_repaint();
        }

        // ---- commit config edits (on interaction frames, plus a cheap periodic check) ----
        let interacted = ctx.input(|i| !i.events.is_empty() || i.pointer.any_down() || i.pointer.any_released());
        if interacted || (self.frames.is_multiple_of(30) && self.cfg != self.cfg_base) {
            self.commit_config();
        }

        // ---- repaint policy ----
        // The engine wakes the UI once per data tick (1 Hz); tweens, toasts and the page fade
        // schedule their own frames while they run. Continuous animation only where it carries
        // information and only while the window is focused: the Lighting preview of a moving effect,
        // and the spinning fan glyphs when the user turned them on.
        let reduce = anim::reduce_motion(ctx);
        let animating = focused
            && !reduce
            && match self.page {
                Page::Lighting => self.rgb.animated,
                // The hero's LEDs follow the preview at the Lighting fps.
                Page::Overview => {
                    (self.rgb.animated && self.prefs.show_hero && self.rgb.preview.values().any(|v| !v.is_empty()))
                        || (self.prefs.spin_fans && self.st.fans.iter().any(|f| f.rpm.unwrap_or(0.0) > 50.0))
                }
                Page::Cooling => self.prefs.spin_fans && self.st.fans.iter().any(|f| f.rpm.unwrap_or(0.0) > 50.0),
                _ => false,
            };
        if animating {
            ctx.request_repaint_after(Duration::from_millis(33));
        } else {
            // Fallback if the engine stops ticking; normally its wake-up comes first.
            ctx.request_repaint_after(Duration::from_millis(1500));
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if let Some(pl) = window::placement() {
            self.prefs.placement = Some(pl);
        }
        eframe::set_value(storage, PREFS_KEY, &self.prefs);
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        let c = visuals.panel_fill;
        [c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0, 1.0]
    }
}
