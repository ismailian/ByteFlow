//! Right-to-left text (Arabic script) for egui.
//!
//! egui 0.37 shapes text with harfrust, one run per font face: an Arabic word
//! gets its joining forms and comes out in right-to-left glyph order. It has
//! no bidirectional layout, though. Runs are placed left to right in string
//! order, so in "صور العائلة" (two Arabic runs split by a space from the Latin
//! font) the words come out swapped, numbers and punctuation land on the wrong
//! side, and brackets don't mirror.
//!
//! [`visual`] fixes that before egui sees the text. It reorders each line into
//! visual order with the Unicode Bidirectional Algorithm (UAX #9), then puts
//! every stretch that egui shapes right to left by itself (Arabic script,
//! drawn by the Arabic font) back into logical order, so the shaper still sees
//! whole words in reading order and joins the letters. Text without
//! right-to-left characters comes back untouched and borrowed, so the common
//! case costs one byte scan.
//!
//! * Labels (names, paths, messages) are left-to-right paragraphs like the rest
//!   of the UI, so "تقرير.pdf" keeps its extension on the right, as Explorer
//!   shows it. Text content ([`visual_auto`]: the text viewer, CSV cells) takes
//!   each line's direction from its first strong character.
//! * Wrapping and truncation must happen on logical text, before reordering:
//!   [`wrap`] breaks lines itself and [`fit`] elides at the logical end.
//! * Text inputs use [`layouter`]: the text shows in visual order with the
//!   same characters, so editing works; the caret follows egui's
//!   left-to-right glyph order (egui has no bidi caret).
//!
//! Only Arabic script has a bundled font. Hebrew and other right-to-left
//! scripts are still put in visual order, but render as missing glyphs.

use std::borrow::Cow;
use std::ops::Range;
use std::sync::Arc;

use eframe::egui::{self, Color32, FontId, Galley, RichText, Ui};
use unicode_bidi::{BidiClass, Level, ParagraphBidiInfo, bidi_class};
use unicode_segmentation::UnicodeSegmentation;

const LRM: char = '\u{200E}';
const RLM: char = '\u{200F}';

/// Characters that make text right-to-left: Hebrew, Arabic, Syriac, Thaana,
/// NKo, Samaritan and Mandaic with their extensions and presentation forms,
/// the right-to-left historic and Arabic-math ranges, and the explicit marks.
pub fn is_rtl_char(c: char) -> bool {
    matches!(c as u32, 0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF | 0x10800..=0x10FFF | 0x1E800..=0x1EFFF | 0x200F | 0x202B | 0x202E | 0x2067)
}

pub fn has_rtl(s: &str) -> bool {
    // Everything from U+0590 up starts with a UTF-8 lead byte of 0xD6 or more.
    s.bytes().any(|b| b >= 0xD6) && s.chars().any(is_rtl_char)
}

/// Drawn by the Arabic font, so egui's shaper orders them itself.
fn arabic_font(c: char) -> bool {
    matches!(c as u32, 0x0600..=0x06FF | 0x0750..=0x077F | 0x0870..=0x08FF | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF | 0x200C | 0x200D)
}

/// Arabic-font characters of the Common script (punctuation, tatweel, joiners):
/// a run of only these is shaped left to right.
fn script_common(c: char) -> bool {
    matches!(c, '\u{060C}' | '\u{061B}' | '\u{061C}' | '\u{061F}' | '\u{0640}' | '\u{06DD}' | '\u{08E2}' | '\u{FD3E}' | '\u{FD3F}' | '\u{200C}' | '\u{200D}')
}

/// Explicit directional formatting characters: they steer the algorithm and
/// are never drawn.
fn is_control(c: char) -> bool {
    matches!(c, LRM | RLM | '\u{061C}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Bidi_Mirrored pairs the Latin font draws (the Arabic font mirrors its own).
fn mirror(c: char) -> char {
    match c {
        '(' => ')',
        ')' => '(',
        '[' => ']',
        ']' => '[',
        '{' => '}',
        '}' => '{',
        '<' => '>',
        '>' => '<',
        '«' => '»',
        '»' => '«',
        '‹' => '›',
        '›' => '‹',
        '≤' => '≥',
        '≥' => '≤',
        _ => c,
    }
}

#[derive(Clone, Copy)]
enum Piece<'a> {
    Str(&'a str),
    Char(char),
}

impl Piece<'_> {
    fn first(&self) -> Option<char> {
        match self {
            Piece::Str(s) => s.chars().next(),
            Piece::Char(c) => Some(*c),
        }
    }
    fn any(&self, f: impl Fn(char) -> bool) -> bool {
        match self {
            Piece::Str(s) => s.chars().any(f),
            Piece::Char(c) => f(*c),
        }
    }
}

/// Appends `info.text[range]` (one line of the paragraph `info`) in visual
/// order, as egui must receive it.
fn reorder(info: &ParagraphBidiInfo<'_>, range: Range<usize>, strip: bool, out: &mut String) {
    let text = info.text;
    let push = |out: &mut String, s: &str| {
        if strip {
            out.extend(s.chars().filter(|c| !is_control(*c)));
        } else {
            out.push_str(s);
        }
    };
    if !info.has_rtl() || range.is_empty() {
        push(out, &text[range]);
        return;
    }
    let (levels, runs) = info.visual_runs(range);
    // Visual order by grapheme, so emoji sequences and combining marks stay whole.
    let mut v: Vec<Piece<'_>> = Vec::new();
    for run in runs {
        let s = &text[run.clone()];
        if levels[run.start].is_rtl() {
            for g in s.graphemes(true).rev() {
                let mut cs = g.chars();
                match (cs.next(), cs.next()) {
                    (Some(c), None) if mirror(c) != c => v.push(Piece::Char(mirror(c))),
                    _ => v.push(Piece::Str(g)),
                }
            }
        } else {
            v.extend(s.graphemes(true).map(Piece::Str));
        }
    }
    // egui shapes each Arabic stretch right to left by itself: hand it back in
    // logical order. (Arabic letters only ever sit at odd levels, so a stretch
    // is a reversed logical run plus non-joining Arabic-Indic digits.)
    let mut i = 0;
    while i < v.len() {
        if !v[i].first().is_some_and(arabic_font) {
            i += 1;
            continue;
        }
        let start = i;
        while i < v.len() && v[i].first().is_some_and(arabic_font) {
            i += 1;
        }
        if v[start..i].iter().any(|p| p.any(|c| arabic_font(c) && !script_common(c))) {
            v[start..i].reverse();
        }
    }
    for p in v {
        match p {
            Piece::Str(s) => push(out, s),
            Piece::Char(c) => out.push(c),
        }
    }
}

fn reorder_text(text: &str, base: Option<Level>, strip: bool) -> Cow<'_, str> {
    if !has_rtl(text) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let info = ParagraphBidiInfo::new(line, base);
        reorder(&info, 0..line.len(), strip, &mut out);
    }
    Cow::Owned(out)
}

/// A label's text in visual order (left-to-right paragraphs). Borrowed and
/// unchanged when it has no right-to-left characters.
pub fn visual(text: &str) -> Cow<'_, str> {
    reorder_text(text, Some(Level::ltr()), true)
}

/// Text content in visual order, each line taking its direction from its
/// first strong character.
pub fn visual_auto(text: &str) -> Cow<'_, str> {
    reorder_text(text, None, true)
}

/// A path (`/` or `\` separated) with its segments kept in left-to-right
/// order ("/مجلد/فرعي" reads parent → child like any other path), for
/// embedding in a label that goes through [`visual`], which drops the marks.
pub fn path_marks(s: &str) -> Cow<'_, str> {
    if has_rtl(s) { Cow::Owned(s.replace('/', "\u{200E}/\u{200E}").replace('\\', "\u{200E}\\\u{200E}")) } else { Cow::Borrowed(s) }
}

/// A path ready to show: [`path_marks`] then [`visual`].
pub fn path(s: &str) -> Cow<'_, str> {
    match path_marks(s) {
        Cow::Borrowed(b) => Cow::Borrowed(b),
        Cow::Owned(o) => Cow::Owned(visual(&o).into_owned()),
    }
}

// ------------------------------------------------------ wrap and truncate --

fn measurer<'u>(ui: &'u Ui, font: &'u FontId) -> impl Fn(&str) -> f32 + 'u {
    move |s: &str| ui.fonts_mut(|f| f.layout_no_wrap(s.to_owned(), font.clone(), Color32::PLACEHOLDER).size().x)
}

/// Greedy line breaking on logical text: after whitespace, `_`, `-` and `/`,
/// or between graphemes of a word wider than a line. Trailing whitespace is
/// left off each line.
fn break_lines(text: &str, width: f32, measure: &dyn Fn(&str) -> f32) -> Vec<Range<usize>> {
    let mut tokens = Vec::new();
    let mut start = 0;
    let mut it = text.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        let breaks = c.is_whitespace() || matches!(c, '_' | '-' | '/');
        // Keep a run of spaces with the word before it.
        if breaks && !it.peek().is_some_and(|(_, n)| n.is_whitespace() && c.is_whitespace()) {
            let end = i + c.len_utf8();
            tokens.push(start..end);
            start = end;
        }
    }
    if start < text.len() {
        tokens.push(start..text.len());
    }
    let mut lines = Vec::new();
    let (mut line_start, mut line_w) = (0, 0.0);
    for t in tokens {
        let tok = &text[t.clone()];
        let ink = measure(tok.trim_end());
        if line_w + ink > width && t.start > line_start {
            lines.push(line_start..t.start);
            (line_start, line_w) = (t.start, 0.0);
        }
        if ink > width {
            for (gi, g) in tok.grapheme_indices(true) {
                let gw = measure(g);
                if line_w + gw > width && t.start + gi > line_start {
                    lines.push(line_start..t.start + gi);
                    (line_start, line_w) = (t.start + gi, 0.0);
                }
                line_w += gw;
            }
        } else {
            line_w += measure(tok);
        }
    }
    lines.push(line_start..text.len());
    lines.into_iter().map(|r| r.start..r.start + text[r.clone()].trim_end().len()).collect()
}

fn wrap_with<'a>(text: &'a str, width: f32, auto: bool, measure: &dyn Fn(&str) -> f32) -> Cow<'a, str> {
    if !has_rtl(text) {
        return Cow::Borrowed(text);
    }
    let base = if auto { None } else { Some(Level::ltr()) };
    if !width.is_finite() || width <= 0.0 {
        return reorder_text(text, base, true);
    }
    let mut out = String::with_capacity(text.len() + 16);
    for (i, para) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let info = ParagraphBidiInfo::new(para, base);
        for (j, line) in break_lines(para, width, measure).into_iter().enumerate() {
            if j > 0 {
                out.push('\n');
            }
            reorder(&info, line, true, &mut out);
        }
    }
    Cow::Owned(out)
}

/// What a wrapping label `width` wide should show: lines broken on the
/// logical text, then each one reordered. Borrowed and unchanged when there
/// is no right-to-left text (egui wraps that itself).
pub fn wrap<'a>(ui: &Ui, text: &'a str, font: &FontId, width: f32, auto: bool) -> Cow<'a, str> {
    if !has_rtl(text) {
        return Cow::Borrowed(text);
    }
    // A hair narrower than egui's own wrap width, so it never re-breaks a line.
    wrap_with(text, width - 1.0, auto, &measurer(ui, font))
}

fn fit_with<'a>(text: &'a str, width: f32, measure: &dyn Fn(&str) -> f32) -> Cow<'a, str> {
    if !has_rtl(text) {
        return Cow::Borrowed(text);
    }
    let full = visual(text);
    if !width.is_finite() || measure(&full) <= width {
        return full;
    }
    let ends: Vec<usize> = text.grapheme_indices(true).map(|(i, _)| i).skip(1).chain([text.len()]).collect();
    // The first `k` graphemes plus "…". The ellipsis stays at the logical end:
    // after right-to-left text, an RLM pulls it to that run's left side.
    let shortened = |k: usize| -> String {
        let prefix = if k == 0 { "" } else { text[..ends[k - 1]].trim_end() };
        let rtl_end = prefix.chars().rev().map(bidi_class).find(|c| matches!(c, BidiClass::L | BidiClass::R | BidiClass::AL)).is_some_and(|c| c != BidiClass::L);
        let s = if rtl_end { format!("{prefix}…{RLM}") } else { format!("{prefix}…") };
        visual(&s).into_owned()
    };
    let (mut lo, mut hi) = (0, ends.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if measure(&shortened(mid)) <= width { lo = mid } else { hi = mid - 1 }
    }
    Cow::Owned(shortened(lo))
}

/// What a truncating label `width` wide should show: reordered and, if too
/// wide, cut at the logical end with "…". Borrowed and unchanged when there
/// is no right-to-left text (egui truncates that itself).
pub fn fit<'a>(ui: &Ui, text: &'a str, font: &FontId, width: f32) -> Cow<'a, str> {
    if !has_rtl(text) {
        return Cow::Borrowed(text);
    }
    fit_with(text, width, &measurer(ui, font))
}

// ----------------------------------------------------------------- widgets --

/// A one-line, truncated, non-selectable label for user text (file and folder
/// names, the bucket).
pub fn name(ui: &mut Ui, text: &str, font: FontId, color: Color32) -> egui::Response {
    let shown = fit(ui, text, &font, ui.available_width());
    ui.add(egui::Label::new(RichText::new(shown).font(font).color(color)).truncate().selectable(false))
}

/// A wrapping label for text that may hold Arabic.
pub fn wrapped(ui: &mut Ui, text: &str, font: FontId, color: Color32) -> egui::Response {
    let shown = wrap(ui, text, &font, ui.available_width(), false);
    ui.add(egui::Label::new(RichText::new(shown).font(font).color(color)).wrap())
}

/// A `TextEdit::layouter` for single-line inputs in `font` and `color`.
/// Right-to-left text shows in visual order; the characters (and so caret
/// indices) are the same, controls included.
pub fn layouter(font: FontId, color: Color32) -> impl FnMut(&Ui, &dyn egui::TextBuffer, f32) -> Arc<Galley> {
    move |ui: &Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
        let text = reorder_text(buf.as_str(), Some(Level::ltr()), false).into_owned();
        let mut job = egui::text::LayoutJob::simple_singleline(text, font.clone(), color);
        // As egui's own layouter: keep trailing spaces while typing.
        job.keep_trailing_whitespace = true;
        let line_h = ui.fonts_mut(|f| f.row_height(&font)) + ui.spacing().extra_text_line_spacing;
        for s in &mut job.sections {
            s.format.line_height = Some(line_h);
        }
        ui.fonts_mut(|f| f.layout_job(job))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_borrowed() {
        for s in ["hello world", "", "café — naïve ✓ 😀", "日本語"] {
            assert!(matches!(visual(s), Cow::Borrowed(_)), "{s}");
            assert!(matches!(visual_auto(s), Cow::Borrowed(_)), "{s}");
            assert!(matches!(path(s), Cow::Borrowed(_)), "{s}");
        }
        assert!(has_rtl("a ب c"));
        assert!(!has_rtl("Ünïcödé"));
    }

    #[test]
    fn arabic_words_come_out_in_visual_order() {
        // Each Arabic word stays in logical order (egui shapes it right to
        // left); the words themselves are swapped: first word on the right.
        assert_eq!(visual("صور العائلة"), "العائلة صور");
        assert_eq!(visual("كلمة"), "كلمة");
    }

    #[test]
    fn labels_are_left_to_right_paragraphs() {
        assert_eq!(visual("تقرير.pdf"), "تقرير.pdf");
        assert_eq!(visual("Report تقرير 2024.pdf"), "Report 2024 تقرير.pdf");
        assert_eq!(visual("Uploaded صور العائلة.zip"), "Uploaded العائلة صور.zip");
    }

    #[test]
    fn brackets_mirror_in_right_to_left_runs() {
        assert_eq!(visual("ملف (نسخة)"), "(نسخة) ملف");
    }

    #[test]
    fn content_takes_its_direction_from_the_first_strong_character() {
        assert_eq!(visual_auto("مرحبا!"), "!مرحبا");
        assert_eq!(visual("مرحبا!"), "مرحبا!");
        assert_eq!(visual_auto("سطر أول\nسطر ثان"), "أول سطر\nثان سطر");
    }

    #[test]
    fn arabic_indic_digits_and_marks_survive() {
        // Arabic-Indic digits are Arabic script, so egui shapes "٢١" right to
        // left and shows "١٢": the number keeps its reading order, on the left.
        assert_eq!(visual("صفحة ١٢"), "٢١ صفحة");
        // A Persian ZWNJ and harakat stay attached to their letters.
        assert_eq!(visual("می‌خواهم"), "می‌خواهم");
        assert_eq!(visual("كَتَبَ"), "كَتَبَ");
    }

    #[test]
    fn paths_keep_their_segment_order() {
        assert_eq!(path("/مجلد/فرعي"), "/مجلد/فرعي");
        assert_eq!(path("photos/رحلة الصيف/"), "photos/الصيف رحلة/");
        assert_eq!(visual(&format!("To {}", path_marks("/مجلد/فرعي"))), "To /مجلد/فرعي");
    }

    #[test]
    fn controls_are_dropped_from_labels_but_kept_for_inputs() {
        let s = "\u{200F}مرحبا\u{200E} ok";
        assert!(!visual(s).contains(['\u{200E}', '\u{200F}']));
        let kept = reorder_text(s, Some(Level::ltr()), false);
        assert_eq!(kept.chars().count(), s.chars().count());
    }

    #[test]
    fn emoji_sequences_stay_whole() {
        let s = "عائلة 👨‍👩‍👧 سعيدة";
        let v = visual(s);
        assert!(v.contains("👨‍👩‍👧"), "{v}");
        assert_eq!(v.chars().count(), s.chars().count());
    }

    /// One unit per character: enough to check the line logic.
    fn chars(s: &str) -> f32 {
        s.chars().count() as f32
    }

    #[test]
    fn wrapping_breaks_logical_text_then_reorders_each_line() {
        // Six words, three per line: the first line holds the first words.
        let w = wrap_with("واحد اثنان ثلاثة اربعة خمسة ستة", 17.0, false, &chars);
        assert_eq!(w, "ثلاثة اثنان واحد\nستة خمسة اربعة");
        // A word wider than the line is split between graphemes.
        let long = wrap_with("مممممممممم", 4.0, false, &chars);
        assert_eq!(long.lines().count(), 3);
        assert!(long.lines().all(|l| l.chars().count() <= 4));
        assert!(matches!(wrap_with("plain words", 3.0, false, &chars), Cow::Borrowed(_)));
    }

    #[test]
    fn fitting_elides_at_the_logical_end() {
        assert_eq!(fit_with("ملف قصير", 20.0, &chars), "قصير ملف");
        // Cut after the first word: the ellipsis ends the Arabic run, so it
        // sits on that run's left.
        let f = fit_with("كلمة طويلة جدا", 6.0, &chars);
        assert_eq!(f, "…كلمة");
        assert!(chars(&f) <= 6.0);
        // Cut inside Latin text after an Arabic name: ellipsis on the right.
        assert_eq!(fit_with("تقرير final version.pdf", 12.0, &chars), "تقرير final…");
        assert!(matches!(fit_with("plain", 2.0, &chars), Cow::Borrowed(_)));
    }
}
