//! Inter (SIL OFL 1.1, see assets/fonts/OFL.txt) and the type scale.
//!
//! egui rasterizes with `ab_glyph`, which applies no OpenType features, so `tnum` can't be switched
//! on at runtime. Instead `InterTab-*.ttf` are tiny digit-only fonts whose cmap points `0-9 . , : - +`
//! at Inter's tabular glyphs (built by assets/fonts/make_fonts.py). The `tab*` families list them
//! first, so live numbers never jitter while other characters fall back to Inter.

use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily, FontId, TextStyle};

static INTER_REGULAR: &[u8] = include_bytes!("../../assets/fonts/Inter-Regular.ttf");
static INTER_MEDIUM: &[u8] = include_bytes!("../../assets/fonts/Inter-Medium.ttf");
static INTER_SEMIBOLD: &[u8] = include_bytes!("../../assets/fonts/Inter-SemiBold.ttf");
static TAB_REGULAR: &[u8] = include_bytes!("../../assets/fonts/InterTab-Regular.ttf");
static TAB_MEDIUM: &[u8] = include_bytes!("../../assets/fonts/InterTab-Medium.ttf");
static TAB_SEMIBOLD: &[u8] = include_bytes!("../../assets/fonts/InterTab-SemiBold.ttf");

pub const OFL_TEXT: &str = include_str!("../../assets/fonts/OFL.txt");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weight {
    Regular,
    Medium,
    SemiBold,
}

fn family(w: Weight, tabular: bool) -> FontFamily {
    match (w, tabular) {
        (Weight::Regular, false) => FontFamily::Proportional,
        (Weight::Medium, false) => FontFamily::Name("medium".into()),
        (Weight::SemiBold, false) => FontFamily::Name("semibold".into()),
        (Weight::Regular, true) => FontFamily::Name("tab".into()),
        (Weight::Medium, true) => FontFamily::Name("tab-md".into()),
        (Weight::SemiBold, true) => FontFamily::Name("tab-sb".into()),
    }
}

pub fn install(ctx: &egui::Context) {
    let mut f = FontDefinitions::default();
    let fallbacks: Vec<String> = ["NotoEmoji-Regular", "emoji-icon-font"].iter().map(|s| s.to_string()).collect();
    for (name, bytes) in [
        ("inter", INTER_REGULAR),
        ("inter-md", INTER_MEDIUM),
        ("inter-sb", INTER_SEMIBOLD),
        ("intertab", TAB_REGULAR),
        ("intertab-md", TAB_MEDIUM),
        ("intertab-sb", TAB_SEMIBOLD),
    ] {
        f.font_data.insert(name.into(), Arc::new(FontData::from_static(bytes)));
    }
    let chain = |first: &[&str]| -> Vec<String> {
        let mut v: Vec<String> = first.iter().map(|s| s.to_string()).collect();
        v.extend(fallbacks.iter().cloned());
        v
    };
    f.families.insert(FontFamily::Proportional, chain(&["inter"]));
    f.families.insert(FontFamily::Name("medium".into()), chain(&["inter-md", "inter"]));
    f.families.insert(FontFamily::Name("semibold".into()), chain(&["inter-sb", "inter"]));
    f.families.insert(FontFamily::Name("tab".into()), chain(&["intertab", "inter"]));
    f.families.insert(FontFamily::Name("tab-md".into()), chain(&["intertab-md", "inter-md", "inter"]));
    f.families.insert(FontFamily::Name("tab-sb".into()), chain(&["intertab-sb", "inter-sb", "inter"]));
    // Monospace keeps egui's Hack, with Inter as fallback for symbols.
    if let Some(m) = f.families.get_mut(&FontFamily::Monospace) {
        m.push("inter".into());
    }
    ctx.set_fonts(f);
}

/// The type scale (logical points).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ts {
    DisplayXl,
    Display,
    /// 22 pt SemiBold: the CPU safety headline and the inspector duty.
    DisplaySm,
    Title,
    Headline,
    Body,
    BodyStrong,
    Label,
    Caption,
    Overline,
    Mono,
}

impl Ts {
    pub fn size(self) -> f32 {
        match self {
            Ts::DisplayXl => 40.0,
            Ts::Display => 28.0,
            Ts::DisplaySm => 22.0,
            Ts::Title => 20.0,
            Ts::Headline => 15.0,
            Ts::Body | Ts::BodyStrong => 14.0,
            Ts::Label => 13.0,
            Ts::Caption => 12.0,
            Ts::Overline => 11.0,
            Ts::Mono => 12.0,
        }
    }

    pub fn weight(self) -> Weight {
        match self {
            Ts::DisplayXl | Ts::Display | Ts::DisplaySm | Ts::Title | Ts::Headline | Ts::Overline => Weight::SemiBold,
            Ts::BodyStrong | Ts::Label => Weight::Medium,
            Ts::Body | Ts::Caption | Ts::Mono => Weight::Regular,
        }
    }
}

/// Font for a text style.
pub fn font(ts: Ts) -> FontId {
    if ts == Ts::Mono {
        return FontId::monospace(ts.size());
    }
    FontId::new(ts.size(), family(ts.weight(), false))
}

/// Font with tabular digits for live numbers.
pub fn tab(ts: Ts) -> FontId {
    if ts == Ts::Mono {
        return FontId::monospace(ts.size());
    }
    FontId::new(ts.size(), family(ts.weight(), true))
}

/// Arbitrary size/weight (gauges scale with their diameter).
pub fn sized(size: f32, w: Weight, tabular: bool) -> FontId {
    FontId::new(size, family(w, tabular))
}

pub fn apply_text_styles(s: &mut egui::Style) {
    s.text_styles = [
        (TextStyle::Heading, font(Ts::Title)),
        (TextStyle::Body, font(Ts::Body)),
        (TextStyle::Button, font(Ts::Label)),
        (TextStyle::Small, font(Ts::Caption)),
        (TextStyle::Monospace, font(Ts::Mono)),
        (TextStyle::Name("display_xl".into()), font(Ts::DisplayXl)),
        (TextStyle::Name("display".into()), font(Ts::Display)),
        (TextStyle::Name("display_sm".into()), tab(Ts::DisplaySm)),
        (TextStyle::Name("headline".into()), font(Ts::Headline)),
        (TextStyle::Name("body_strong".into()), font(Ts::BodyStrong)),
        (TextStyle::Name("overline".into()), font(Ts::Overline)),
        (TextStyle::Name("tabular".into()), tab(Ts::Body)),
    ]
    .into();
    s.drag_value_text_style = TextStyle::Name("tabular".into());
}

#[cfg(test)]
mod tests {
    #[test]
    fn bundled_fonts_are_truetype() {
        for bytes in
            [super::INTER_REGULAR, super::INTER_MEDIUM, super::INTER_SEMIBOLD, super::TAB_REGULAR, super::TAB_SEMIBOLD]
        {
            assert_eq!(&bytes[..4], &[0, 1, 0, 0], "not a TrueType sfnt");
        }
        assert!(super::OFL_TEXT.contains("SIL Open Font License"));
    }
}
