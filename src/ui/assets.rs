//! Rendered design assets: the hero rig, the board texture, component
//! thumbnails and the app icon. Embedded with `include_bytes!`, decoded with `png` (already in the
//! dependency tree through eframe) on first use, uploaded once, and freed again when a group has not
//! been drawn for [`IDLE_FREE`] (a page left alone, or the window in the tray), so a hidden FanRGB
//! holds no design textures at all.

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use egui::{Color32, ColorImage, Context, TextureFilter, TextureHandle, TextureId, TextureOptions, TextureWrapMode};

/// A texture group not drawn for this long is dropped (GPU and RAM freed; it reloads on demand).
pub const IDLE_FREE: Duration = Duration::from_secs(30);

pub struct Png {
    x1: &'static [u8],
    x2: Option<&'static [u8]>,
}

macro_rules! png2 {
    ($n:literal) => {
        Png {
            x1: include_bytes!(concat!("../../assets/design/", $n, "@1x.png")),
            x2: Some(include_bytes!(concat!("../../assets/design/", $n, "@2x.png"))),
        }
    };
}

static HERO_BASE: Png = png2!("hero_base");
static BOARD_TOP: Png = png2!("board_top");
static HERO_LED: &[u8] = include_bytes!("../../assets/design/hero_led@1x.png");
static HERO_JSON: &str = include_str!("../../assets/design/hero.json");
static APP_ICON: &[u8] = include_bytes!("../../assets/design/icon_256.png");
/// Window icon (taskbar / Alt+Tab).
pub static WINDOW_ICON: &[u8] = include_bytes!("../../assets/design/icon_48.png");
/// Tray icon base (the status badges are painted on top).
pub static TRAY_ICON: &[u8] = include_bytes!("../../assets/design/icon_32.png");

const OPTS: TextureOptions = TextureOptions {
    magnification: TextureFilter::Linear,
    minification: TextureFilter::Linear,
    wrap_mode: TextureWrapMode::ClampToEdge,
    mipmap_mode: None,
};
/// For images drawn well below their pixel size (thumbnails at 34-52 pt, the 256 px app icon).
const OPTS_MIP: TextureOptions = TextureOptions { mipmap_mode: Some(TextureFilter::Linear), ..OPTS };

/// Decodes an embedded PNG to 8-bit samples: (size, bytes, colour type).
pub fn decode(bytes: &[u8]) -> Option<([usize; 2], Vec<u8>, png::ColorType)> {
    let mut d = png::Decoder::new(std::io::Cursor::new(bytes));
    d.set_transformations(png::Transformations::EXPAND);
    let mut r = d.read_info().ok()?;
    let mut buf = vec![0; r.output_buffer_size()?];
    let info = r.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    Some(([info.width as usize, info.height as usize], buf, info.color_type))
}

/// Straight-alpha RGBA image.
pub fn rgba_image(bytes: &[u8]) -> Option<ColorImage> {
    let (size, px, ct) = decode(bytes)?;
    match ct {
        png::ColorType::Rgba => Some(ColorImage::from_rgba_unmultiplied(size, &px)),
        png::ColorType::Rgb => Some(ColorImage::from_rgb(size, &px)),
        _ => None,
    }
}

/// Light-contribution mask (8-bit grey), stored opaque so `tint.additive()` adds `v · tint`.
fn light_image(bytes: &[u8]) -> Option<ColorImage> {
    let (size, px, ct) = decode(bytes)?;
    let pixels: Vec<Color32> = match ct {
        png::ColorType::Grayscale => px.iter().map(|&v| Color32::from_rgb(v, v, v)).collect(),
        png::ColorType::GrayscaleAlpha => px.chunks_exact(2).map(|c| Color32::from_rgb(c[0], c[0], c[0])).collect(),
        _ => return None,
    };
    Some(ColorImage::new(size, pixels))
}

// ---------------------------------------------------------------------------------------
// hero.json

#[derive(Clone, Debug, serde::Deserialize)]
pub struct HeroSeg {
    #[allow(dead_code)]
    pub key: String,
    /// "front1", "front2", "front3", "rear", "pump".
    pub fan: String,
    /// Quarter: 0 = 12 o'clock, then clockwise seen from the LED face.
    pub segment: usize,
    /// x0, y0, x1, y1 in 1x pixels of the base image.
    pub rect: [u16; 4],
    /// x, y, w, h in atlas pixels.
    pub atlas: [u16; 4],
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct HeroMap {
    pub size_1x: [f32; 2],
    pub atlas_size: [f32; 2],
    /// Normalised (0..1) anchor points of the callouts.
    pub anchors: BTreeMap<String, [f32; 2]>,
    pub segments: Vec<HeroSeg>,
}

pub fn hero_map() -> &'static HeroMap {
    static MAP: OnceLock<HeroMap> = OnceLock::new();
    MAP.get_or_init(|| {
        serde_json::from_str(HERO_JSON).unwrap_or(HeroMap {
            size_1x: [440.0, 480.0],
            atlas_size: [1.0, 1.0],
            anchors: BTreeMap::new(),
            segments: Vec::new(),
        })
    })
}

// ---------------------------------------------------------------------------------------
// Thumbnails

/// Component thumbnails (128 pt renders, drawn at 34-52 pt).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
// thumb_board / thumb_gpu / thumb_radiator are not drawn by any screen yet, so they are not embedded
// (−120 KB exe); add a variant here when a page needs them.
pub enum Thumb {
    Fan140,
    Fan120,
    Pump,
    Strip,
}

impl Thumb {
    fn base(self) -> &'static Png {
        static FAN140: Png = png2!("thumb_fan140");
        static FAN120: Png = png2!("thumb_fan120");
        static PUMP: Png = png2!("thumb_pump");
        static STRIP: Png = png2!("thumb_strip");
        match self {
            Thumb::Fan140 => &FAN140,
            Thumb::Fan120 => &FAN120,
            Thumb::Pump => &PUMP,
            Thumb::Strip => &STRIP,
        }
    }

    /// Light mask of the RGB parts (lit with the zone colour).
    fn led(self) -> Option<&'static Png> {
        static FAN140: Png = png2!("thumb_fan140_led");
        static PUMP: Png = png2!("thumb_pump_led");
        static STRIP: Png = png2!("thumb_strip_led");
        match self {
            Thumb::Fan140 => Some(&FAN140),
            Thumb::Pump => Some(&PUMP),
            Thumb::Strip => Some(&STRIP),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------------------

struct Slot {
    tex: TextureHandle,
    used: Instant,
}

/// Loaded design textures (owned by the `App`, one per process).
#[derive(Default)]
pub struct Assets {
    /// Textures were decoded from the @2x files (pixels_per_point ≥ 1.5).
    hi: bool,
    slots: HashMap<&'static str, Slot>,
    thumbs: HashMap<(Thumb, bool), Slot>,
    /// Decode failures (never retried; logged once).
    failed: Vec<&'static str>,
}

impl Assets {
    fn pick(ctx: &Context, p: &Png) -> &'static [u8] {
        if ctx.pixels_per_point() >= 1.5 { p.x2.unwrap_or(p.x1) } else { p.x1 }
    }

    /// Drops every texture when the scale crosses 1.5 (the other resolution is needed).
    fn check_scale(&mut self, ctx: &Context) {
        let hi = ctx.pixels_per_point() >= 1.5;
        if hi != self.hi {
            self.hi = hi;
            self.slots.clear();
            self.thumbs.clear();
        }
    }

    fn get(
        &mut self,
        ctx: &Context,
        name: &'static str,
        opts: TextureOptions,
        load: impl FnOnce() -> Option<ColorImage>,
    ) -> Option<TextureId> {
        self.check_scale(ctx);
        let now = Instant::now();
        if let Some(s) = self.slots.get_mut(name) {
            s.used = now;
            return Some(s.tex.id());
        }
        if self.failed.contains(&name) {
            return None;
        }
        let t0 = Instant::now();
        match load() {
            Some(img) => {
                let tex = ctx.load_texture(name, img, opts);
                log::debug!("design texture {name} loaded in {:.1} ms", t0.elapsed().as_secs_f32() * 1000.0);
                let id = tex.id();
                self.slots.insert(name, Slot { tex, used: now });
                Some(id)
            }
            None => {
                log::warn!("design asset {name} could not be decoded");
                self.failed.push(name);
                None
            }
        }
    }

    /// Hero base render (straight alpha) and its LED light atlas.
    pub fn hero(&mut self, ctx: &Context) -> Option<(TextureId, TextureId)> {
        let bytes = Self::pick(ctx, &HERO_BASE);
        let base = self.get(ctx, "hero_base", OPTS, || rgba_image(bytes))?;
        let led = self.get(ctx, "hero_led", OPTS, || light_image(HERO_LED))?;
        Some((base, led))
    }

    /// Rendered PCB, exactly the 100 × 92 board units of `board_map`.
    pub fn board(&mut self, ctx: &Context) -> Option<TextureId> {
        let bytes = Self::pick(ctx, &BOARD_TOP);
        self.get(ctx, "board_top", OPTS, || rgba_image(bytes))
    }

    /// FanRGB's 256 px icon (Updates card).
    pub fn app_icon(&mut self, ctx: &Context) -> Option<TextureId> {
        self.get(ctx, "app_icon", OPTS_MIP, || rgba_image(APP_ICON))
    }

    /// Thumbnail base and (for RGB parts) its light mask.
    pub fn thumb(&mut self, ctx: &Context, t: Thumb) -> Option<(TextureId, Option<TextureId>)> {
        self.check_scale(ctx);
        let base = self.thumb_part(ctx, t, false)?;
        let led = if t.led().is_some() { self.thumb_part(ctx, t, true) } else { None };
        Some((base, led))
    }

    fn thumb_part(&mut self, ctx: &Context, t: Thumb, led: bool) -> Option<TextureId> {
        let now = Instant::now();
        if let Some(s) = self.thumbs.get_mut(&(t, led)) {
            s.used = now;
            return Some(s.tex.id());
        }
        let png = if led { t.led()? } else { t.base() };
        let bytes = Self::pick(ctx, png);
        let img = if led { light_image(bytes) } else { rgba_image(bytes) }?;
        let tex = ctx.load_texture(format!("thumb_{t:?}_{led}"), img, OPTS_MIP);
        let id = tex.id();
        self.thumbs.insert((t, led), Slot { tex, used: now });
        Some(id)
    }

    /// Frees groups that were not drawn for [`IDLE_FREE`]. Cheap; call once per update (also while
    /// the window is hidden: then nothing is drawn, so everything goes after 30 s).
    pub fn gc(&mut self, now: Instant) {
        let old = |s: &Slot| now.duration_since(s.used) > IDLE_FREE;
        let before = self.slots.len() + self.thumbs.len();
        self.slots.retain(|_, s| !old(s));
        self.thumbs.retain(|_, s| !old(s));
        let freed = before - self.slots.len() - self.thumbs.len();
        if freed > 0 {
            log::debug!("freed {freed} idle design texture(s)");
        }
    }

    /// Number of textures held (diagnostics / tests).
    #[allow(dead_code)]
    pub fn loaded(&self) -> usize {
        self.slots.len() + self.thumbs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Textures load on first use, are reused, and go after [`IDLE_FREE`] without a draw (the
    /// tray / another page), so a hidden FanRGB holds none.
    #[test]
    fn idle_textures_are_freed() {
        let ctx = Context::default();
        let mut a = Assets::default();
        let (base, _) = a.hero(&ctx).expect("hero");
        assert_eq!(a.hero(&ctx).map(|h| h.0), Some(base), "reused, not reloaded");
        a.thumb(&ctx, Thumb::Fan140).expect("thumb");
        assert_eq!(a.loaded(), 4);
        a.gc(Instant::now());
        assert_eq!(a.loaded(), 4, "recently drawn: kept");
        a.gc(Instant::now() + IDLE_FREE + Duration::from_secs(1));
        assert_eq!(a.loaded(), 0);
    }

    #[test]
    fn embedded_assets_decode() {
        for p in [&HERO_BASE, &BOARD_TOP] {
            let a = rgba_image(p.x1).expect("1x");
            let b = rgba_image(p.x2.unwrap()).expect("2x");
            assert_eq!([a.size[0] * 2, a.size[1] * 2], b.size);
            assert!(a.size[0] <= 1024 && b.size[0] <= 1024 && b.size[1] <= 1024);
        }
        let led = light_image(HERO_LED).expect("atlas");
        let map = hero_map();
        assert_eq!(led.size, [map.atlas_size[0] as usize, map.atlas_size[1] as usize]);
        assert_eq!(map.segments.len(), 20);
        for s in &map.segments {
            assert!(s.atlas[0] as usize + s.atlas[2] as usize <= led.size[0]);
            assert!(s.atlas[1] as usize + s.atlas[3] as usize <= led.size[1]);
            assert!(s.rect[2] as f32 <= map.size_1x[0] && s.rect[3] as f32 <= map.size_1x[1]);
            assert!(s.segment < 4);
        }
        for k in ["pump", "radiator", "front2", "rear"] {
            assert!(map.anchors.contains_key(k), "{k}");
        }
        for t in [Thumb::Fan140, Thumb::Fan120, Thumb::Pump, Thumb::Strip] {
            assert!(rgba_image(t.base().x1).is_some(), "{t:?}");
            if let Some(l) = t.led() {
                assert!(light_image(l.x1).is_some(), "{t:?} led");
            }
        }
        assert!(rgba_image(APP_ICON).is_some() && rgba_image(WINDOW_ICON).is_some());
    }
}
