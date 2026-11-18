//! Shared components: buttons, segmented controls and tabs, badges, inputs,
//! progress, cards, empty states, the modal frame, menu items and file-type
//! tiles. Everything reads colours from [`theme::pal`].

use eframe::egui::{self, Align2, Color32, CornerRadius, Response, RichText, Sense, Stroke, Ui, Widget, WidgetInfo, vec2};

use bf_core::media::{MediaKind, media_kind_for_key};

use crate::bidi;
use crate::theme::{self, Palette, pal};
use crate::ui::icons;

// ------------------------------------------------------------------ tones --

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Primary,
    Success,
    Warn,
    Danger,
    Violet,
}

/// (foreground, soft background)
pub fn tone_colors(p: &Palette, tone: Tone) -> (Color32, Color32) {
    match tone {
        Tone::Neutral => (p.text_muted, p.surface_2),
        Tone::Primary => (p.primary_text, p.primary_soft),
        Tone::Success => (p.success, p.success_soft),
        Tone::Warn => (p.warn, p.warn_soft),
        Tone::Danger => (p.danger, p.danger_soft),
        Tone::Violet => (p.violet, p.violet_soft),
    }
}

// ---------------------------------------------------------------- buttons --

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Primary,
    Secondary,
    Ghost,
    Danger,
    /// Outlined, red text: destructive but not the main action.
    DangerOutline,
}

pub struct Btn<'a> {
    text: &'a str,
    icon: Option<&'a str>,
    kind: Kind,
    small: bool,
    enabled: bool,
    min_width: f32,
}

impl<'a> Btn<'a> {
    pub fn new(kind: Kind, text: &'a str) -> Self {
        Self { text, icon: None, kind, small: false, enabled: true, min_width: 0.0 }
    }
    pub fn icon(mut self, icon: &'a str) -> Self {
        self.icon = Some(icon);
        self
    }
    pub fn small(mut self) -> Self {
        self.small = true;
        self
    }
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
    pub fn min_width(mut self, w: f32) -> Self {
        self.min_width = w;
        self
    }
}

impl Widget for Btn<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        let p = *pal(ui);
        let (h, pad, fs, is) = if self.small { (28.0, 10.0, 13.0, 15.0) } else { (34.0, 14.0, 14.0, 16.0) };
        let gap = 7.0;
        let text_g = (!self.text.is_empty()).then(|| ui.painter().layout_no_wrap(self.text.to_owned(), theme::medium(fs), Color32::PLACEHOLDER));
        let icon_g = self.icon.map(|i| ui.painter().layout_no_wrap(i.to_owned(), theme::regular(is), Color32::PLACEHOLDER));
        let content_w = text_g.as_ref().map_or(0.0, |g| g.size().x)
            + icon_g.as_ref().map_or(0.0, |g| g.size().x)
            + if text_g.is_some() && icon_g.is_some() { gap } else { 0.0 };
        let w = (content_w + pad * 2.0).max(self.min_width).max(h);
        let sense = if self.enabled { Sense::click() } else { Sense::hover() };
        let (rect, resp) = ui.allocate_exact_size(vec2(w, h), sense);
        let text = self.text.to_owned();
        let enabled = self.enabled;
        resp.widget_info(|| WidgetInfo::labeled(egui::Role::Button, enabled, &text));
        if ui.is_rect_visible(rect) {
            let hovered = self.enabled && resp.hovered();
            let pressed = self.enabled && resp.is_pointer_button_down_on();
            let (mut fill, mut stroke, mut fg) = match self.kind {
                Kind::Primary => (if pressed || hovered { p.primary_hover } else { p.primary }, Stroke::NONE, p.on_primary),
                Kind::Danger => (if pressed || hovered { p.danger.gamma_multiply(0.85) } else { p.danger }, Stroke::NONE, Color32::WHITE),
                Kind::Secondary => (
                    if pressed { p.surface_3 } else if hovered { p.surface_2 } else { p.surface },
                    Stroke::new(1.0, p.border_strong),
                    p.text,
                ),
                Kind::Ghost => (
                    if pressed { p.surface_3 } else if hovered { p.surface_2 } else { Color32::TRANSPARENT },
                    Stroke::NONE,
                    if hovered { p.text } else { p.text_muted },
                ),
                Kind::DangerOutline => (
                    if pressed || hovered { p.danger_soft } else { p.surface },
                    Stroke::new(1.0, p.border_strong),
                    p.danger,
                ),
            };
            if !self.enabled {
                fill = fill.gamma_multiply(0.55);
                stroke.color = stroke.color.gamma_multiply(0.6);
                fg = fg.gamma_multiply(0.5);
            }
            let painter = ui.painter();
            painter.rect(rect, CornerRadius::same(8), fill, stroke, egui::StrokeKind::Inside);
            if resp.has_focus() {
                painter.rect_stroke(rect.expand(2.0), CornerRadius::same(10), Stroke::new(2.0, p.primary.gamma_multiply(0.55)), egui::StrokeKind::Outside);
            }
            let mut x = rect.center().x - content_w / 2.0;
            if let Some(g) = icon_g {
                let gw = g.size().x;
                painter.galley(egui::pos2(x, rect.center().y - g.size().y / 2.0), g, fg);
                x += gw + gap;
            }
            if let Some(g) = text_g {
                painter.galley(egui::pos2(x, rect.center().y - g.size().y / 2.0), g, fg);
            }
        }
        if self.enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
    }
}

pub fn btn(ui: &mut Ui, kind: Kind, icon: &str, text: &str) -> Response {
    let b = Btn::new(kind, text);
    ui.add(if icon.is_empty() { b } else { b.icon(icon) })
}

pub fn primary(ui: &mut Ui, text: &str) -> Response {
    ui.add(Btn::new(Kind::Primary, text))
}

pub fn primary_enabled(ui: &mut Ui, enabled: bool, text: &str) -> Response {
    ui.add(Btn::new(Kind::Primary, text).enabled(enabled))
}

pub fn secondary(ui: &mut Ui, text: &str) -> Response {
    ui.add(Btn::new(Kind::Secondary, text))
}

pub fn danger(ui: &mut Ui, enabled: bool, text: &str) -> Response {
    ui.add(Btn::new(Kind::Danger, text).enabled(enabled))
}

/// Square, icon-only ghost button with a tooltip (its accessible label).
pub fn icon_btn(ui: &mut Ui, icon: &str, tooltip: &str) -> Response {
    icon_btn_ex(ui, icon, tooltip, 32.0, false, true)
}

pub fn icon_btn_ex(ui: &mut Ui, icon: &str, tooltip: &str, size: f32, active: bool, enabled: bool) -> Response {
    let p = *pal(ui);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), sense);
    let label = tooltip.to_owned();
    resp.widget_info(|| WidgetInfo::labeled(egui::Role::Button, enabled, &label));
    if ui.is_rect_visible(rect) {
        let hovered = enabled && resp.hovered();
        let pressed = enabled && resp.is_pointer_button_down_on();
        let fill = if active {
            p.primary_soft
        } else if pressed {
            p.surface_3
        } else if hovered {
            p.surface_2
        } else {
            Color32::TRANSPARENT
        };
        let mut fg = if active { p.primary_text } else if hovered { p.text } else { p.text_muted };
        if !enabled {
            fg = fg.gamma_multiply(0.45);
        }
        ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
        if resp.has_focus() {
            ui.painter().rect_stroke(rect.expand(1.0), CornerRadius::same(9), Stroke::new(2.0, p.primary.gamma_multiply(0.55)), egui::StrokeKind::Outside);
        }
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, theme::regular(size * 0.55), fg);
    }
    let resp = if tooltip.is_empty() { resp } else { resp.on_hover_text(tooltip) };
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

// ------------------------------------------------------ segmented & tabs --

/// A pill-shaped segmented control. Returns true when the value changed.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str, &str)]) -> bool {
    let p = *pal(ui);
    let mut changed = false;
    let h = 32.0;
    let inner = egui::Frame::new()
        .fill(p.surface_2)
        .corner_radius(CornerRadius::same(9))
        .inner_margin(egui::Margin::same(3))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.horizontal(|ui| {
                for (v, icon, label) in options {
                    let selected = *value == *v;
                    let text = match (icon.is_empty(), label.is_empty()) {
                        (false, false) => format!("{icon}  {label}"),
                        (false, true) => icon.to_string(),
                        _ => label.to_string(),
                    };
                    let g = ui.painter().layout_no_wrap(text.clone(), theme::medium(13.0), Color32::PLACEHOLDER);
                    let w = g.size().x + 22.0;
                    let (rect, resp) = ui.allocate_exact_size(vec2(w, h - 6.0), Sense::click());
                    resp.widget_info(|| WidgetInfo::selected(egui::Role::Button, true, selected, &text));
                    let hovered = resp.hovered();
                    if selected {
                        ui.painter().rect(rect, CornerRadius::same(7), p.surface, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
                    } else if hovered {
                        ui.painter().rect_filled(rect, CornerRadius::same(7), p.surface_3.gamma_multiply(0.6));
                    }
                    let fg = if selected || hovered { p.text } else { p.text_muted };
                    ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);
                    if resp.clicked() && !selected {
                        *value = *v;
                        changed = true;
                    }
                    resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                }
            });
        });
    let _ = inner;
    changed
}

/// Underline tabs with optional counts. Returns true when the value changed.
pub fn tabs<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str, Option<usize>)]) -> bool {
    let p = *pal(ui);
    let mut changed = false;
    let top = ui.cursor().top();
    let r = ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (v, label, count) in options {
            let selected = *value == *v;
            let text_g = ui.painter().layout_no_wrap(label.to_string(), theme::medium(14.0), Color32::PLACEHOLDER);
            let count_g = count.map(|n| ui.painter().layout_no_wrap(n.to_string(), theme::medium(11.5), Color32::PLACEHOLDER));
            let cw = count_g.as_ref().map_or(0.0, |g| g.size().x + 12.0 + 6.0);
            let w = text_g.size().x + cw + 20.0;
            let (rect, resp) = ui.allocate_exact_size(vec2(w, 38.0), Sense::click());
            let label_s = label.to_string();
            resp.widget_info(|| WidgetInfo::selected(egui::Role::Button, true, selected, &label_s));
            let hovered = resp.hovered();
            let fg = if selected || hovered { p.text } else { p.text_muted };
            let tx = rect.left() + 10.0;
            ui.painter().galley(egui::pos2(tx, rect.center().y - text_g.size().y / 2.0 - 1.0), text_g.clone(), fg);
            if let Some(g) = count_g {
                let cx = tx + text_g.size().x + 6.0;
                let cr = egui::Rect::from_min_size(egui::pos2(cx, rect.center().y - 10.0), vec2(g.size().x + 12.0, 19.0));
                let (cf, cb) = if selected { (p.primary_text, p.primary_soft) } else { (p.text_muted, p.surface_2) };
                ui.painter().rect_filled(cr, CornerRadius::same(10), cb);
                ui.painter().galley(cr.center() - g.size() / 2.0, g, cf);
            }
            if selected {
                let bar = egui::Rect::from_min_max(egui::pos2(rect.left() + 6.0, rect.bottom() - 2.0), egui::pos2(rect.right() - 6.0, rect.bottom()));
                ui.painter().rect_filled(bar, CornerRadius::same(1), p.primary);
            }
            if resp.clicked() && !selected {
                *value = *v;
                changed = true;
            }
            resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        }
    });
    let bottom = r.response.rect.bottom();
    let full = ui.max_rect();
    ui.painter().hline(full.x_range(), bottom, Stroke::new(1.0, p.border));
    let _ = top;
    changed
}

// --------------------------------------------------------- badges & text --

pub fn badge(ui: &mut Ui, text: &str, tone: Tone) -> Response {
    badge_icon(ui, "", text, tone)
}

pub fn badge_icon(ui: &mut Ui, icon: &str, text: &str, tone: Tone) -> Response {
    let p = pal(ui);
    let (fg, bg) = tone_colors(p, tone);
    let s = match (icon.is_empty(), text.is_empty()) {
        (true, _) => text.to_string(),
        (false, true) => icon.to_string(),
        (false, false) => format!("{icon} {text}"),
    };
    let g = ui.painter().layout_no_wrap(s, theme::medium(11.5), fg);
    let size = vec2(g.size().x + 14.0, 20.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(10), bg);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);
    resp
}

/// Back-compat for callers that pass a raw colour.
pub fn pill(ui: &mut Ui, text: &str, color: Color32) {
    let g = ui.painter().layout_no_wrap(text.to_owned(), theme::medium(11.5), color);
    let (rect, _) = ui.allocate_exact_size(vec2(g.size().x + 14.0, 20.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(10), color.gamma_multiply(0.14));
    ui.painter().galley(rect.center() - g.size() / 2.0, g, color);
}

/// Page title (and optional subtitle) with right-aligned actions.
pub fn page_header(ui: &mut Ui, title: &str, subtitle: &str, actions: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(title).font(theme::semibold(22.0)).color(pal(ui).text));
            if !subtitle.is_empty() {
                ui.add_space(-2.0);
                ui.label(RichText::new(subtitle).color(pal(ui).text_muted));
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), actions);
    });
    ui.add_space(12.0);
}

pub fn heading(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::semibold(22.0)).color(pal(ui).text));
    ui.add_space(4.0);
}

pub fn section(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::semibold(15.0)).color(pal(ui).text));
    ui.add_space(2.0);
}

/// Small uppercase-ish group label (sidebars, details).
pub fn eyebrow(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::semibold(11.0)).color(pal(ui).text_subtle));
}

pub fn hint(ui: &mut Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(12.5).color(pal(ui).text_subtle)).wrap());
}

pub fn error_text(ui: &mut Ui, text: &str) {
    let c = pal(ui).danger;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(RichText::new(icons::WARNING_CIRCLE).color(c));
        ui.label(RichText::new(text).color(c));
    });
}

/// Soft, tinted notice box.
pub fn callout(ui: &mut Ui, tone: Tone, icon: &str, text: &str) {
    let p = pal(ui);
    let (fg, bg) = tone_colors(p, tone);
    egui::Frame::new().fill(bg).corner_radius(CornerRadius::same(10)).inner_margin(egui::Margin::symmetric(12, 10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(RichText::new(icon).size(16.0).color(fg));
            bidi::wrapped(ui, text, theme::regular(13.0), if tone == Tone::Neutral { p.text_muted } else { fg });
        });
    });
}

// ---------------------------------------------------------------- progress --

pub fn progress_bar(ui: &mut Ui, frac: f32, height: f32, color: Color32) -> Response {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let r = CornerRadius::same((height / 2.0) as u8);
    ui.painter().rect_filled(rect, r, p.surface_3);
    let f = frac.clamp(0.0, 1.0);
    if f > 0.0 {
        let fill = egui::Rect::from_min_size(rect.min, vec2((rect.width() * f).max(height), height));
        ui.painter().rect_filled(fill, r, color);
    }
    resp
}

pub fn progress(ui: &mut Ui, frac: f32, text: String) {
    let p = *pal(ui);
    ui.horizontal(|ui| {
        ui.label(RichText::new(text).size(12.5).color(p.text_muted));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(format!("{:.0}%", frac.clamp(0.0, 1.0) * 100.0)).font(theme::medium(12.5)).color(p.text_muted));
        });
    });
    progress_bar(ui, frac, 6.0, p.primary);
}

/// Strength meter 0–4.
pub fn strength_bar(ui: &mut Ui, score: u8) {
    let p = *pal(ui);
    let colors = [p.danger, p.danger, p.warn, p.success, p.success];
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for i in 0..4u8 {
            let (r, _) = ui.allocate_exact_size(vec2(44.0, 5.0), Sense::hover());
            let c = if i < score { colors[score as usize] } else { p.surface_3 };
            ui.painter().rect_filled(r, 3.0, c);
        }
        ui.add_space(6.0);
        ui.label(RichText::new(bf_app::strength::strength_label(score)).size(12.0).color(p.text_subtle));
    });
}

// ------------------------------------------------------------------ inputs --

fn input<'a>(value: &'a mut String, hint: &str) -> egui::TextEdit<'a> {
    egui::TextEdit::singleline(value).hint_text(hint).margin(egui::Margin::symmetric(10, 8)).desired_width(f32::INFINITY)
}

/// A single-line password input (copying out of it is disabled by egui).
pub fn password_field(ui: &mut Ui, value: &mut String, hint: &str) -> Response {
    ui.add(input(value, hint).password(true))
}

/// Arabic shows in visual order (see [`bidi::layouter`]).
pub fn text_field(ui: &mut Ui, value: &mut String, hint: &str) -> Response {
    let mut layouter = bidi::layouter(theme::regular(14.0), pal(ui).text);
    ui.add(input(value, hint).layouter(&mut layouter))
}

/// A visible label above a control (forms never rely on placeholders alone).
pub fn field<R>(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.label(RichText::new(label).font(theme::medium(13.0)).color(pal(ui).text));
    ui.add_space(-1.0);
    let r = add(ui);
    ui.add_space(6.0);
    r
}

/// Search box with a leading magnifier and a clear button.
pub fn search_field(ui: &mut Ui, value: &mut String, hint: &str, width: f32) -> Response {
    let p = *pal(ui);
    let id = ui.make_persistent_id(("search", hint));
    let focused = ui.memory(|m| m.has_focus(id));
    let stroke = if focused { Stroke::new(1.5, p.primary) } else { Stroke::new(1.0, p.border_strong) };
    let inner = egui::Frame::new()
        .fill(p.surface)
        .stroke(stroke)
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin { left: 10, right: 4, top: 0, bottom: 0 })
        .show(ui, |ui| {
            ui.set_width(width - 16.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(RichText::new(icons::MAGNIFYING_GLASS).size(15.0).color(p.text_subtle));
                let clear_w = if value.is_empty() { 0.0 } else { 26.0 };
                let mut layouter = bidi::layouter(theme::regular(14.0), p.text);
                let r = ui.add(
                    egui::TextEdit::singleline(value)
                        .id(id)
                        .layouter(&mut layouter)
                        .hint_text(hint)
                        // A custom frame ignores `.margin()`: pad through the frame.
                        .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(0, 7)))
                        .desired_width(ui.available_width() - clear_w),
                );
                if !value.is_empty() && icon_btn_ex(ui, icons::X, "Clear search", 24.0, false, true).clicked() {
                    value.clear();
                }
                r
            })
            .inner
        });
    inner.inner
}

// ------------------------------------------------------------------- cards --

/// Rounded square with a tinted icon.
pub fn icon_tile(ui: &mut Ui, icon: &str, tone: Tone, size: f32) -> Response {
    let p = pal(ui);
    let (fg, bg) = tone_colors(p, tone);
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same((size * 0.25) as u8), bg);
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, theme::regular(size * 0.5), fg);
    resp
}

pub fn empty_state(ui: &mut Ui, icon: &str, title: &str, body: &str) {
    let p = *pal(ui);
    ui.vertical_centered(|ui| {
        ui.add_space(48.0);
        let (rect, _) = ui.allocate_exact_size(vec2(64.0, 64.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 32.0, p.surface_2);
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, theme::regular(28.0), p.text_subtle);
        ui.add_space(10.0);
        ui.label(RichText::new(title).font(theme::semibold(16.0)).color(p.text));
        ui.add_space(2.0);
        ui.set_max_width(420.0);
        bidi::wrapped(ui, body, theme::regular(14.0), p.text_muted);
    });
}

/// Key/value row for detail panes.
pub fn kv(ui: &mut Ui, key: &str, value: impl Into<egui::WidgetText>) -> Response {
    let p = *pal(ui);
    ui.horizontal_top(|ui| {
        // Fixed, left-aligned key column (add_sized would centre it).
        let (rect, _) = ui.allocate_exact_size(vec2(92.0, 20.0), Sense::hover());
        ui.painter().text(rect.left_center(), Align2::LEFT_CENTER, key, theme::regular(12.5), p.text_subtle);
        ui.add(egui::Label::new(value).wrap())
    })
    .inner
}

pub fn divider(ui: &mut Ui) {
    let p = pal(ui);
    let r = ui.available_rect_before_wrap();
    ui.add_space(6.0);
    ui.painter().hline(r.x_range(), ui.cursor().top(), Stroke::new(1.0, p.border));
    ui.add_space(7.0);
}

// ------------------------------------------------------------------ modal --

pub struct ModalSpec<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub subtitle: &'a str,
    pub icon: &'a str,
    pub tone: Tone,
    pub width: f32,
    /// Esc / backdrop / close button dismiss the modal.
    pub dismissable: bool,
}

impl<'a> ModalSpec<'a> {
    pub fn new(id: &'a str, title: &'a str) -> Self {
        Self { id, title, subtitle: "", icon: "", tone: Tone::Primary, width: 460.0, dismissable: true }
    }
    pub fn icon(mut self, icon: &'a str, tone: Tone) -> Self {
        self.icon = icon;
        self.tone = tone;
        self
    }
    pub fn subtitle(mut self, s: &'a str) -> Self {
        self.subtitle = s;
        self
    }
    pub fn width(mut self, w: f32) -> Self {
        self.width = w;
        self
    }
    pub fn locked(mut self) -> Self {
        self.dismissable = false;
        self
    }
}

/// The app's modal frame: header (icon tile, title, subtitle, close), body.
/// Returns true when the user dismissed it (close button, Esc, backdrop).
pub fn modal(ctx: &egui::Context, spec: ModalSpec<'_>, body: impl FnOnce(&mut Ui)) -> bool {
    let p = theme::pal_ctx(ctx);
    let frame = egui::Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(16))
        .shadow(egui::Shadow { offset: [0, 16], blur: 48, spread: 0, color: Color32::from_black_alpha(if p.dark { 160 } else { 50 }) })
        .inner_margin(egui::Margin::ZERO);
    let backdrop = Color32::from_black_alpha(if p.dark { 150 } else { 96 });
    let mut closed = false;
    let resp = egui::Modal::new(egui::Id::unique(spec.id)).frame(frame).backdrop_color(backdrop).show(ctx, |ui| {
        ui.set_width(spec.width);
        ui.spacing_mut().item_spacing.y = 8.0;
        egui::Frame::new().inner_margin(egui::Margin { left: 22, right: 14, top: 18, bottom: 4 }).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                if !spec.icon.is_empty() {
                    icon_tile(ui, spec.icon, spec.tone, 38.0);
                    ui.add_space(4.0);
                }
                ui.vertical(|ui| {
                    ui.add_space(if spec.subtitle.is_empty() { 8.0 } else { 0.0 });
                    let w = spec.width - if spec.icon.is_empty() { 80.0 } else { 130.0 };
                    ui.set_max_width(w);
                    bidi::wrapped(ui, spec.title, theme::semibold(17.0), p.text);
                    if !spec.subtitle.is_empty() {
                        ui.add_space(-3.0);
                        bidi::wrapped(ui, spec.subtitle, theme::regular(13.0), p.text_muted);
                    }
                });
                if spec.dismissable {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                        if icon_btn_ex(ui, icons::X, "Close", 30.0, false, true).clicked() {
                            closed = true;
                        }
                    });
                }
            });
        });
        egui::Frame::new().inner_margin(egui::Margin { left: 22, right: 22, top: 10, bottom: 20 }).show(ui, |ui| {
            ui.set_width(spec.width - 44.0);
            body(ui);
        });
    });
    closed || (spec.dismissable && resp.should_close())
}

/// Right-aligned footer row for modal actions (add primary first).
pub fn modal_footer(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.add_space(10.0);
    // Inside a row, so the right-to-left layout is one row tall instead of
    // centring itself in all the remaining height.
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            add(ui);
        });
    });
}

// ------------------------------------------------------------------- menus --

/// Full-width menu row with a leading icon.
pub fn menu_item(ui: &mut Ui, icon: &str, label: &str, danger: bool) -> Response {
    let p = *pal(ui);
    // The menu's own width (set_min_width by the caller), not all the space
    // a popup is allowed to grow into.
    let w = ui.min_rect().width().max(200.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 32.0), Sense::click());
    let l = label.to_owned();
    resp.widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, &l));
    let hovered = resp.hovered();
    if hovered {
        ui.painter().rect_filled(rect, CornerRadius::same(6), if danger { p.danger_soft } else { p.surface_2 });
    }
    let fg = if danger { p.danger } else { p.text };
    let ic = if danger { p.danger } else if hovered { p.text } else { p.text_muted };
    ui.painter().text(egui::pos2(rect.left() + 18.0, rect.center().y), Align2::CENTER_CENTER, icon, theme::regular(16.0), ic);
    ui.painter().text(egui::pos2(rect.left() + 36.0, rect.center().y), Align2::LEFT_CENTER, label, theme::regular(14.0), fg);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn menu_separator(ui: &mut Ui) {
    let p = pal(ui);
    ui.add_space(2.0);
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.x_range(), ui.cursor().top(), Stroke::new(1.0, p.border));
    ui.add_space(3.0);
}

// ------------------------------------------------------------ file types --

/// Icon and tint for a key.
pub fn kind_style(key: &str, is_folder: bool) -> (&'static str, Color32) {
    if is_folder {
        return (icons::FOLDER_SIMPLE, Color32::from_rgb(0x3B, 0x82, 0xF6));
    }
    match media_kind_for_key(key) {
        MediaKind::Image => (icons::IMAGE, Color32::from_rgb(0xD9, 0x46, 0xEF)),
        MediaKind::Video => (icons::FILM_STRIP, Color32::from_rgb(0x8B, 0x5C, 0xF6)),
        MediaKind::Audio => (icons::MUSIC_NOTES, Color32::from_rgb(0xF9, 0x73, 0x16)),
        MediaKind::Text => match bf_core::media::extension(key).as_str() {
            "csv" | "tsv" => (icons::FILE_CSV, Color32::from_rgb(0x16, 0xA3, 0x4A)),
            "txt" | "md" | "log" => (icons::FILE_TEXT, Color32::from_rgb(0x64, 0x74, 0x8B)),
            _ => (icons::FILE_CODE, Color32::from_rgb(0x0E, 0xA5, 0xE9)),
        },
        MediaKind::None => match bf_core::media::extension(key).as_str() {
            "pdf" => (icons::FILE_PDF, Color32::from_rgb(0xEF, 0x44, 0x44)),
            "zip" | "7z" | "rar" | "gz" | "tar" | "xz" | "bz2" => (icons::FILE_ZIP, Color32::from_rgb(0xD9, 0x77, 0x06)),
            "doc" | "docx" | "odt" | "rtf" => (icons::FILE_DOC, Color32::from_rgb(0x25, 0x63, 0xEB)),
            "xls" | "xlsx" | "ods" => (icons::FILE_XLS, Color32::from_rgb(0x16, 0xA3, 0x4A)),
            "ppt" | "pptx" | "odp" => (icons::FILE_PPT, Color32::from_rgb(0xEA, 0x58, 0x0C)),
            _ => (icons::FILE, Color32::from_rgb(0x64, 0x74, 0x8B)),
        },
    }
}

/// "1 item" / "3 items".
pub fn items(n: u64) -> String {
    if n == 1 { "1 item".into() } else { format!("{n} items") }
}

/// A human label for the type ("JPEG image", "Folder").
pub fn kind_label(key: &str, is_folder: bool) -> String {
    if is_folder {
        return "Folder".into();
    }
    let ext = bf_core::media::extension(key);
    let up = ext.to_uppercase();
    match media_kind_for_key(key) {
        MediaKind::Image => format!("{up} image"),
        MediaKind::Video => format!("{up} video"),
        MediaKind::Audio => format!("{up} audio"),
        MediaKind::Text => format!("{up} text"),
        MediaKind::None if ext.is_empty() => "File".into(),
        MediaKind::None => format!("{up} file"),
    }
}

/// Tinted rounded tile with the type icon.
pub fn file_tile(ui: &mut Ui, key: &str, is_folder: bool, size: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    paint_file_tile(ui, rect, key, is_folder);
    resp
}

pub fn paint_file_tile(ui: &Ui, rect: egui::Rect, key: &str, is_folder: bool) {
    let p = pal(ui);
    let (icon, tint) = kind_style(key, is_folder);
    let bg = if p.dark { tint.gamma_multiply(0.18) } else { tint.gamma_multiply(0.11) };
    let s = rect.width().min(rect.height());
    ui.painter().rect_filled(rect, CornerRadius::same((s * 0.24).min(14.0) as u8), bg);
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, theme::regular(s * 0.55), tint);
}

