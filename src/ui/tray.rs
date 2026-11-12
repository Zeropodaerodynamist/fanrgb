//! Notification-area icon and menu.
//!
//! Menu actions are executed directly in the event handler (on the UI thread's message
//! loop), so they work even while the main window is in the tray and egui is idle. The tooltip and
//! the menu's header line are refreshed when the pointer reaches the icon, so the window does not
//! have to wake up every second just to keep them current.

use std::cell::{Cell, RefCell};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::config::{Config, Preset};
use crate::engine::{Command, EngineState, Shared};

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
static CTX: OnceLock<egui::Context> = OnceLock::new();

/// What a "Fan mode" menu entry selects.
#[derive(Clone, Copy, PartialEq)]
enum ModeEntry {
    Preset(Preset),
    /// A profile that is not the built-in one of its preset (custom profiles, duplicates).
    Profile(usize),
}

struct Items {
    header: MenuItem,
    modes: Vec<(ModeEntry, CheckMenuItem)>,
    lighting: CheckMenuItem,
}

thread_local! {
    static ITEMS: RefCell<Option<Items>> = const { RefCell::new(None) };
    /// The icon (cheap `Rc` clone) for the hover refresh, which runs outside `App`.
    static ICON: RefCell<Option<TrayIcon>> = const { RefCell::new(None) };
    static LAST_HOVER: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Tooltip and menu header for the current state.
pub fn texts(st: &EngineState, cfg: &Config) -> (String, String) {
    let f = cfg.general.fahrenheit;
    let header = format!(
        "FanRGB — CPU {} · GPU {}",
        super::widgets::fmt_deg(st.value(crate::model::ids::CPU_TEMP), f),
        super::widgets::fmt_deg(st.value(crate::model::ids::GPU_TEMP), f)
    );
    let tip = format!("{header} · {}", cfg.fan_mode().label);
    (tip, header)
}

/// Pointer over the tray icon: refresh tooltip + header from the latest data (at most once a second,
/// never blocking on a worker lock).
fn refresh_on_hover() {
    let now = Instant::now();
    if LAST_HOVER.get().is_some_and(|t| now.duration_since(t) < Duration::from_secs(1)) {
        return;
    }
    LAST_HOVER.set(Some(now));
    let Some(shared) = SHARED.get() else { return };
    let (Ok(st), Ok(cfg)) = (shared.state.try_read(), shared.config.try_read()) else { return };
    let (tip, header) = texts(&st, &cfg);
    drop((st, cfg));
    ICON.with(|i| {
        if let Some(icon) = i.borrow().as_ref() {
            let _ = icon.set_tooltip(Some(&tip));
        }
    });
    ITEMS.with(|it| {
        if let Some(items) = it.borrow().as_ref() {
            items.header.set_text(&header);
        }
    });
}

pub struct Tray {
    icon: TrayIcon,
    signature: String,
    last_tooltip: String,
    last_header: String,
    icon_state: u8,
}

fn signature(cfg: &Config) -> String {
    cfg.profiles.iter().map(|p| format!("{}\u{2}{:?}", p.name, p.preset)).collect::<Vec<_>>().join("\u{1}")
}

/// The "Fan mode" submenu entries: the six built-in modes, then every other profile.
fn mode_entries(cfg: &Config) -> Vec<(ModeEntry, String)> {
    let mut v: Vec<(ModeEntry, String)> =
        Preset::MODES.iter().map(|&p| (ModeEntry::Preset(p), p.label().to_string())).collect();
    for (i, p) in cfg.profiles.iter().enumerate() {
        if p.preset == Preset::Custom || cfg.preset_profile(p.preset) != Some(i) {
            v.push((ModeEntry::Profile(i), p.name.clone()));
        }
    }
    v
}

fn is_active(cfg: &Config, e: ModeEntry) -> bool {
    match e {
        ModeEntry::Preset(p) => cfg.preset_profile(p) == Some(cfg.active_profile),
        ModeEntry::Profile(i) => i == cfg.active_profile,
    }
}

/// 32 px tray icon: the rendered app icon with the warning / critical badge on top;
/// the procedural icon if the PNG can't be decoded.
fn tray_rgba(state: u8) -> Vec<u8> {
    let mut rgba = super::assets::rgba_image(super::assets::TRAY_ICON)
        .filter(|img| img.size == [32, 32])
        .map(|img| img.pixels.iter().flat_map(|c| c.to_srgba_unmultiplied()).collect::<Vec<u8>>())
        .unwrap_or_else(|| crate::icon_art::render_icon_rgba(32));
    crate::icon_art::overlay_state(&mut rgba, 32, state);
    rgba
}

fn build_menu(cfg: &Config) -> Menu {
    let menu = Menu::new();
    let header = MenuItem::with_id("header", "FanRGB", false, None);
    let _ = menu.append(&header);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let sub = Submenu::with_id("modes", "Fan mode", true);
    let mut modes = Vec::new();
    for (k, (e, label)) in mode_entries(cfg).into_iter().enumerate() {
        if k == Preset::MODES.len() {
            let _ = sub.append(&PredefinedMenuItem::separator());
        }
        let id = match e {
            ModeEntry::Preset(p) => format!("mode:{p:?}"),
            ModeEntry::Profile(i) => format!("profile:{i}"),
        };
        let item = CheckMenuItem::with_id(id, &label, true, is_active(cfg, e), None);
        let _ = sub.append(&item);
        modes.push((e, item));
    }
    let _ = sub.append(&PredefinedMenuItem::separator());
    let _ = sub.append(&MenuItem::with_id("mode:next", "Next fan mode", true, None));
    let _ = menu.append(&sub);
    let release = MenuItem::with_id("release", "Return fans to BIOS", true, None);
    let _ = menu.append(&release);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let lighting = CheckMenuItem::with_id("lighting", "Lighting", true, cfg.rgb.enabled, None);
    let _ = menu.append(&lighting);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let open = MenuItem::with_id("open", "Open FanRGB", true, None);
    let _ = menu.append(&open);
    let exit = MenuItem::with_id("exit", "Exit", true, None);
    let _ = menu.append(&exit);
    ITEMS.with(|it| *it.borrow_mut() = Some(Items { header, modes, lighting }));
    menu
}

fn refresh_checks(cfg: &Config) {
    ITEMS.with(|it| {
        if let Some(items) = it.borrow().as_ref() {
            for (e, item) in &items.modes {
                item.set_checked(is_active(cfg, *e));
            }
            items.lighting.set_checked(cfg.rgb.enabled);
        }
    });
}

fn on_menu(id: &str) {
    let Some(shared) = SHARED.get() else { return };
    match id {
        "open" => super::window::request_show(),
        "exit" => crate::system::request_exit(shared),
        "release" => shared.send(Command::EmergencyRelease),
        "lighting" => shared.edit_config(|c| c.rgb.enabled = !c.rgb.enabled),
        "mode:next" => {
            shared.cycle_preset();
        }
        m if m.starts_with("mode:") => {
            if let Some(&p) = Preset::MODES.iter().find(|p| format!("mode:{p:?}") == m) {
                shared.apply_preset(p);
            }
        }
        p if p.starts_with("profile:") => {
            if let Ok(i) = p["profile:".len()..].parse::<usize>() {
                shared.edit_config(|c| {
                    if i < c.profiles.len() {
                        c.active_profile = i;
                    }
                });
            }
        }
        _ => {}
    }
    // A fan mode that waits for the header identification (every header stays on the BIOS): show
    // the window, whose first frame opens the Identify prompt.
    let mode_change = id.starts_with("mode:") || id.starts_with("profile:");
    let held = mode_change
        && shared.state.try_read().is_ok_and(|st| !st.mode_gate.is_empty())
        && shared.config.try_read().is_ok_and(|c| c.profile().preset != Preset::Bios);
    if held {
        super::window::request_show();
    }
    if let Ok(cfg) = shared.config.try_read() {
        refresh_checks(&cfg);
    }
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint();
    }
}

pub fn create(shared: &Arc<Shared>, ctx: &egui::Context) -> Option<Tray> {
    let _ = SHARED.set(shared.clone());
    let _ = CTX.set(ctx.clone());
    MenuEvent::set_event_handler(Some(|e: MenuEvent| on_menu(&e.id.0)));
    TrayIconEvent::set_event_handler(Some(|e: TrayIconEvent| match e {
        TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
        | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } => super::window::request_show(),
        TrayIconEvent::Enter { .. } | TrayIconEvent::Move { .. } => refresh_on_hover(),
        _ => {}
    }));

    let cfg = shared.config_snapshot();
    let icon = Icon::from_rgba(tray_rgba(0), 32, 32).ok()?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(build_menu(&cfg)))
        .with_menu_on_left_click(false)
        .with_tooltip("FanRGB")
        .with_icon(icon)
        .build();
    match tray {
        Ok(icon) => Some(Tray {
            icon: {
                ICON.with(|i| *i.borrow_mut() = Some(icon.clone()));
                // Critical alerts pop up from this icon (`notify`).
                crate::notify::set_target(icon.window_handle() as isize);
                icon
            },
            signature: signature(&cfg),
            last_tooltip: String::new(),
            last_header: String::new(),
            icon_state: 0,
        }),
        Err(e) => {
            log::warn!("tray icon unavailable: {e}");
            None
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        // The icon's window goes away with it.
        crate::notify::set_target(0);
    }
}

impl Tray {
    /// Refreshes menu checks, the header line (temps), tooltip and icon state
    /// (0 normal, 1 warning, 2 critical). Only touches native objects when something changed.
    pub fn sync(&mut self, cfg: &Config, tooltip: &str, header: &str, state: u8) {
        let sig = signature(cfg);
        if sig != self.signature {
            self.icon.set_menu(Some(Box::new(build_menu(cfg))));
            self.signature = sig;
            self.last_header.clear();
        }
        refresh_checks(cfg);
        if tooltip != self.last_tooltip {
            let _ = self.icon.set_tooltip(Some(tooltip));
            self.last_tooltip = tooltip.to_string();
        }
        if header != self.last_header {
            ITEMS.with(|it| {
                if let Some(items) = it.borrow().as_ref() {
                    items.header.set_text(header);
                }
            });
            self.last_header = header.to_string();
        }
        if state != self.icon_state
            && let Ok(icon) = Icon::from_rgba(tray_rgba(state), 32, 32)
        {
            let _ = self.icon.set_icon(Some(icon));
            self.icon_state = state;
        }
    }
}
