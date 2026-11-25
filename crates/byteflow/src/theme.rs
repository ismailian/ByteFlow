//! Visual theme: fonts, palette tokens and egui visuals.
//!
//! * Text is Inter **Regular** (egui's default is Ubuntu Light).
//!   egui has no font weights, so Medium/SemiBold/Bold are separate named
//!   families ([`medium`], [`semibold`], [`bold`]).
//! * Icons are Phosphor glyphs (`ui::icons`), a fallback font after the text
//!   font, so they mix into any label.
//! * Arabic script (Arabic, Persian, Urdu…) is Noto Sans Arabic, the next
//!   fallback. One variable font serves all four weights through its `wght`
//!   axis. egui shapes it (joining forms); `crate::bidi` puts right-to-left
//!   text in visual order.
//! * Colours come from a light and a dark [`Palette`]; widgets read the
//!   active one with [`pal`].

use std::sync::Arc;

use eframe::egui::{self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Shadow, Stroke, TextStyle, Visuals};
use eframe::epaint::text::VariationCoords;

use bf_app::prefs::ThemeMode;

/// Semantic colour tokens.
#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    /// App canvas (behind cards).
    pub canvas: Color32,
    /// Sidebars, cards, modals, menus.
    pub surface: Color32,
    /// Subtle fills: hover, chips, input wells, tracks.
    pub surface_2: Color32,
    /// Pressed / stronger fill.
    pub surface_3: Color32,
    pub border: Color32,
    pub border_strong: Color32,
    pub text: Color32,
    pub text_muted: Color32,
    pub text_subtle: Color32,
    pub primary: Color32,
    pub primary_hover: Color32,
    pub primary_soft: Color32,
    pub primary_text: Color32,
    pub on_primary: Color32,
    pub danger: Color32,
    pub danger_soft: Color32,
    pub success: Color32,
    pub success_soft: Color32,
    pub warn: Color32,
    pub warn_soft: Color32,
    pub violet: Color32,
    pub violet_soft: Color32,
    /// Media viewer stage.
    pub stage: Color32,
}

const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

pub const LIGHT: Palette = Palette {
    dark: false,
    canvas: hex(0xF6F8FB),
    surface: hex(0xFFFFFF),
    surface_2: hex(0xF1F4F9),
    surface_3: hex(0xE5EAF2),
    border: hex(0xE3E8EF),
    border_strong: hex(0xCCD4E0),
    text: hex(0x0F172A),
    text_muted: hex(0x475569),
    text_subtle: hex(0x64748B),
    primary: hex(0x2563EB),
    primary_hover: hex(0x1D4ED8),
    primary_soft: hex(0xEAF1FF),
    primary_text: hex(0x1D4ED8),
    on_primary: hex(0xFFFFFF),
    danger: hex(0xDC2626),
    danger_soft: hex(0xFEF0F0),
    success: hex(0x15803D),
    success_soft: hex(0xEDFBF2),
    warn: hex(0xB45309),
    warn_soft: hex(0xFFF7E8),
    violet: hex(0x6D28D9),
    violet_soft: hex(0xF4F0FF),
    stage: hex(0x0B1020),
};

pub const DARK: Palette = Palette {
    dark: true,
    canvas: hex(0x0B1220),
    surface: hex(0x111A2C),
    surface_2: hex(0x182338),
    surface_3: hex(0x223049),
    border: hex(0x1F2B42),
    border_strong: hex(0x334561),
    text: hex(0xE6EBF3),
    text_muted: hex(0xA3B0C4),
    text_subtle: hex(0x8494AB),
    primary: hex(0x3B82F6),
    primary_hover: hex(0x60A5FA),
    primary_soft: hex(0x16264A),
    primary_text: hex(0x8AB4FF),
    on_primary: hex(0xFFFFFF),
    danger: hex(0xF26B6B),
    danger_soft: hex(0x2C1519),
    success: hex(0x4ADE80),
    success_soft: hex(0x10291C),
    warn: hex(0xFBBF24),
    warn_soft: hex(0x2C210B),
    violet: hex(0xB79CFF),
    violet_soft: hex(0x231B3F),
    stage: hex(0x05080F),
};

/// Categorical chart colours in fixed slot order (a category keeps its slot
/// whatever else is on screen). Each mode's set is validated against that
/// mode's panel surface: adjacent pairs stay apart under protan/deutan/tritan
/// simulation (ΔE ≥ 8.4) and for normal vision (ΔE ≥ 19). Three light slots
/// sit under 3:1 on white, so charts always label their parts in text.
pub fn series(p: &Palette) -> [Color32; 5] {
    if p.dark {
        [hex(0x3987E5), hex(0xD95926), hex(0x199E70), hex(0xC98500), hex(0xD55181)]
    } else {
        [hex(0x2A78D6), hex(0xEB6834), hex(0x1BAF7A), hex(0xEDA100), hex(0xE87BA4)]
    }
}

pub fn pal(ui: &egui::Ui) -> &'static Palette {
    if ui.visuals().dark_mode { &DARK } else { &LIGHT }
}

pub fn pal_ctx(ctx: &egui::Context) -> &'static Palette {
    if ctx.global_style().visuals.dark_mode { &DARK } else { &LIGHT }
}

pub fn muted(ui: &egui::Ui) -> Color32 {
    pal(ui).text_muted
}

pub fn subtle(ui: &egui::Ui) -> Color32 {
    pal(ui).text_subtle
}

// ------------------------------------------------------------------ fonts --

const MEDIUM: &str = "ui-medium";
const SEMIBOLD: &str = "ui-semibold";
const BOLD: &str = "ui-bold";

const ARABIC_VF: &[u8] = include_bytes!("../assets/fonts/NotoSansArabic-VF.ttf");
/// The Arabic face for each text family, as (family font, Arabic font, wght).
const ARABIC: [(&str, &str, f32); 4] =
    [("ui-regular", "arabic-regular", 400.0), (MEDIUM, "arabic-medium", 500.0), (SEMIBOLD, "arabic-semibold", 600.0), (BOLD, "arabic-bold", 700.0)];

/// The UI typeface: Inter (Regular, Medium, SemiBold, Bold). The bundled
/// files have their Private Use Area mappings removed (see the test below).
const UI_FONT_FILES: [&[u8]; 4] = [
    include_bytes!("../assets/fonts/Inter-Regular.ttf"),
    include_bytes!("../assets/fonts/Inter-Medium.ttf"),
    include_bytes!("../assets/fonts/Inter-SemiBold.ttf"),
    include_bytes!("../assets/fonts/Inter-Bold.ttf"),
];

pub fn regular(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}
pub fn medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(MEDIUM.into()))
}
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEMIBOLD.into()))
}
pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(BOLD.into()))
}
pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

/// Install Inter + Phosphor. Call once at startup.
pub fn install_fonts(ctx: &egui::Context) {
    ctx.set_fonts(font_definitions());
}

fn font_definitions() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let add = |fonts: &mut FontDefinitions, name: &str, bytes: &'static [u8]| {
        fonts.font_data.insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    };
    let [regular, medium, semibold, bold] = UI_FONT_FILES;
    add(&mut fonts, "ui-regular", regular);
    add(&mut fonts, MEDIUM, medium);
    add(&mut fonts, SEMIBOLD, semibold);
    add(&mut fonts, BOLD, bold);
    fonts.font_data.insert(
        "phosphor".to_owned(),
        Arc::new(FontData::from_static(include_bytes!("../assets/fonts/Phosphor.ttf")).tweak(egui::FontTweak {
            scale: 1.08,
            y_offset_factor: 0.0,
            ..Default::default()
        })),
    );
    for (_, name, wght) in ARABIC {
        // Noto's Arabic sits a little small next to the Latin UI font.
        let tweak = egui::FontTweak { scale: 1.06, coords: VariationCoords::new([(b"wght", wght)]), ..Default::default() };
        fonts.font_data.insert(name.to_owned(), Arc::new(FontData::from_static(ARABIC_VF).tweak(tweak)));
    }
    // egui's own fallbacks (symbols, emoji in file names) come last.
    let fallbacks: Vec<String> = fonts
        .families
        .get(&FontFamily::Proportional)
        .map(|v| v.iter().filter(|n| *n != "Ubuntu-Light").cloned().collect())
        .unwrap_or_default();
    let family = |text: &str| -> Vec<String> {
        let arabic = ARABIC.iter().find(|(t, _, _)| *t == text).map_or("arabic-regular", |(_, a, _)| a);
        let mut v = vec![text.to_owned(), "phosphor".to_owned(), arabic.to_owned()];
        v.extend(fallbacks.iter().cloned());
        v
    };
    fonts.families.insert(FontFamily::Proportional, family("ui-regular"));
    for name in [MEDIUM, SEMIBOLD, BOLD] {
        fonts.families.insert(FontFamily::Name(name.into()), family(name));
    }
    if let Some(m) = fonts.families.get_mut(&FontFamily::Monospace) {
        m.insert(1.min(m.len()), "phosphor".to_owned());
        m.insert(2.min(m.len()), "arabic-regular".to_owned());
    }
    // Ubuntu Light is replaced everywhere (egui's Monospace falls back to it
    // too). Every family must only name fonts that exist, or egui panics the
    // first time it lays out text in that family.
    fonts.font_data.remove("Ubuntu-Light");
    for list in fonts.families.values_mut() {
        for name in list.iter_mut() {
            if name == "Ubuntu-Light" {
                *name = "ui-regular".to_owned();
            }
        }
        let mut seen = std::collections::HashSet::new();
        list.retain(|n| seen.insert(n.clone()));
    }
    fonts
}

// ---------------------------------------------------------------- visuals --

pub fn is_dark(ctx: &egui::Context, mode: ThemeMode) -> bool {
    match mode {
        ThemeMode::Dark => true,
        ThemeMode::Light => false,
        ThemeMode::System => ctx.input(|i| i.raw.system_theme) == Some(egui::Theme::Dark),
    }
}

pub fn apply(ctx: &egui::Context, mode: ThemeMode) {
    let dark = is_dark(ctx, mode);
    let p = if dark { &DARK } else { &LIGHT };
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    let r = CornerRadius::same(8);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.surface;
    w.noninteractive.weak_bg_fill = p.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = r;

    w.inactive.bg_fill = p.surface_2;
    w.inactive.weak_bg_fill = p.surface;
    w.inactive.bg_stroke = Stroke::new(1.0, p.border_strong);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = r;
    w.inactive.expansion = 0.0;

    w.hovered.bg_fill = p.surface_3;
    w.hovered.weak_bg_fill = p.surface_2;
    w.hovered.bg_stroke = Stroke::new(1.0, p.border_strong);
    w.hovered.fg_stroke = Stroke::new(1.0, p.text);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;

    w.active.bg_fill = p.surface_3;
    w.active.weak_bg_fill = p.surface_3;
    w.active.bg_stroke = Stroke::new(1.0, p.primary);
    w.active.fg_stroke = Stroke::new(1.0, p.text);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;

    w.open.bg_fill = p.surface_2;
    w.open.weak_bg_fill = p.surface_2;
    w.open.bg_stroke = Stroke::new(1.0, p.border_strong);
    w.open.fg_stroke = Stroke::new(1.0, p.text);
    w.open.corner_radius = r;

    // Selected selectables: soft fill + primary text. Also the focus ring of
    // text inputs and the text-selection highlight.
    v.selection.bg_fill = if dark { hex(0x1D3363) } else { hex(0xDCE8FF) };
    v.selection.stroke = Stroke::new(1.5, p.primary);
    v.hyperlink_color = p.primary_text;
    v.panel_fill = p.canvas;
    v.window_fill = p.surface;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(14);
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_shadow = Shadow { offset: [0, 10], blur: 32, spread: 0, color: Color32::from_black_alpha(if dark { 140 } else { 38 }) };
    v.popup_shadow = Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(if dark { 110 } else { 28 }) };
    v.extreme_bg_color = p.surface_2;
    v.text_edit_bg_color = Some(p.surface);
    v.faint_bg_color = p.surface_2;
    v.code_bg_color = p.surface_2;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.danger;
    v.weak_text_color = Some(p.text_muted);
    v.interact_cursor = Some(egui::CursorIcon::PointingHand);
    v.slider_trailing_fill = true;
    v.collapsing_header_frame = false;
    v.indent_has_left_vline = false;
    v.striped = false;
    v.image_loading_spinners = false;
    ctx.set_visuals(v);

    ctx.global_style_mut(|s| {
        use TextStyle::*;
        s.text_styles = [
            (Small, regular(12.0)),
            (Body, regular(14.0)),
            (Button, medium(14.0)),
            (Heading, semibold(22.0)),
            (Monospace, mono(13.0)),
        ]
        .into();
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(12.0, 6.0);
        s.spacing.interact_size = egui::vec2(32.0, 32.0);
        s.spacing.icon_width = 16.0;
        s.spacing.icon_width_inner = 9.0;
        s.spacing.icon_spacing = 8.0;
        s.spacing.menu_margin = egui::Margin::same(6);
        s.spacing.window_margin = egui::Margin::same(18);
        s.spacing.combo_width = 140.0;
        s.spacing.slider_rail_height = 4.0;
        s.spacing.scroll = egui::style::ScrollStyle::floating();
        s.spacing.scroll.bar_width = 8.0;
        s.animation_time = 0.12;
    });
}

/// A bordered surface card.
pub fn card(ui: &egui::Ui) -> egui::Frame {
    let p = pal(ui);
    egui::Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(egui::Margin::same(18))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A family naming a missing font panics on first layout (it did, for
    /// Monospace → Ubuntu-Light), so check every family resolves.
    #[test]
    fn every_font_family_names_bundled_fonts() {
        let f = font_definitions();
        for (family, list) in &f.families {
            assert!(!list.is_empty(), "{family:?} is empty");
            for name in list {
                assert!(f.font_data.contains_key(name), "{family:?} names missing font {name}");
            }
        }
        for name in ["ui-regular", MEDIUM, SEMIBOLD, BOLD, "phosphor", "arabic-regular", "arabic-bold"] {
            assert!(f.font_data.contains_key(name), "{name} not bundled");
        }
        assert_eq!(f.families[&FontFamily::Proportional][0], "ui-regular");
        // Every text family (and Monospace) falls back to an Arabic face.
        for (family, list) in &f.families {
            assert!(list.iter().any(|n| n.starts_with("arabic-")), "{family:?} has no Arabic fallback");
        }
    }

    /// Phosphor icons live in the Private Use Area. The UI font comes first
    /// in every family, so any PUA glyph of its own would replace an icon
    /// (Inter ships 745; the bundled copies have those mappings removed).
    #[test]
    fn ui_font_leaves_the_private_use_area_to_icons() {
        use skrifa::MetadataProvider;
        for bytes in UI_FONT_FILES {
            let f = skrifa::FontRef::new(bytes).unwrap();
            let pua = f.charmap().mappings().filter(|(c, _)| (0xE000..=0xF8FF).contains(c)).count();
            assert_eq!(pua, 0, "the UI font maps {pua} Private Use Area code points");
        }
    }

    /// Arabic text used to render as tofu: the UI font has no Arabic
    /// glyphs, so the bundled Arabic font must cover the script (letters,
    /// presentation forms, Arabic-Indic digits, Persian letters) and keep a
    /// `wght` axis for the four weights.
    #[test]
    fn arabic_font_covers_the_script() {
        use skrifa::MetadataProvider;
        let ui_font = skrifa::FontRef::new(UI_FONT_FILES[0]).unwrap();
        let ar = skrifa::FontRef::new(ARABIC_VF).unwrap();
        let (ui_map, ar_map) = (ui_font.charmap(), ar.charmap());
        for c in "ابتثجحخدذرزسشصضطظعغفقكلمنهوي ءآأؤإئةى ٠١٢٣٤٥٦٧٨٩ ،؛؟ پچژگک ی".chars().filter(|c| *c != ' ') {
            assert!(ar_map.map(c).is_some(), "Arabic font lacks U+{:04X}", c as u32);
            assert!(ui_map.map(c).is_none(), "the UI font unexpectedly maps U+{:04X}", c as u32);
        }
        assert!(ar.axes().iter().any(|a| a.tag() == skrifa::Tag::new(b"wght") && a.min_value() <= 400.0 && a.max_value() >= 700.0));
    }
}
