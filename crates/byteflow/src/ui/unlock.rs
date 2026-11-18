//! Unlock (spec §3.4): one neutrally labelled password field. No hint that a
//! destruction password exists; biometrics are never prompted automatically
//! (and aren't offered on desktop, §17.2).

use eframe::egui::{self, Align2, Color32, CornerRadius, RichText, Sense, vec2};

use crate::ctx::{AppCtx, secret, wipe_string};
use crate::theme::{self, pal};
use crate::ui::icons;
use crate::ui::widgets::{self, Btn, Kind, Tone};

#[derive(Default)]
pub struct Unlock {
    password: String,
    focused_once: bool,
}

impl Drop for Unlock {
    fn drop(&mut self) {
        wipe_string(&mut self.password);
    }
}

/// The brand mark: a rounded primary tile with the vault glyph.
pub fn brand_mark(ui: &mut egui::Ui, size: f32) {
    let p = pal(ui);
    let (r, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    ui.painter().rect_filled(r, CornerRadius::same((size * 0.28) as u8), p.primary);
    ui.painter().text(r.center(), Align2::CENTER_CENTER, icons::VAULT, theme::regular(size * 0.55), Color32::WHITE);
}

impl Unlock {
    pub fn ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, error: Option<&str>) {
        let busy = ctx.auth.busy();
        let p = *pal(ui);
        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.canvas)).show(ui, |ui| {
            let avail = ui.available_rect_before_wrap();
            let w = 400.0_f32.min(avail.width() - 32.0);
            let top = avail.top() + (avail.height() * 0.5 - 230.0).max(24.0);
            let rect = egui::Rect::from_min_size(egui::pos2(avail.center().x - w / 2.0, top), vec2(w, 600.0));
            let mut col = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::top_down(egui::Align::Center)));
            brand_mark(&mut col, 60.0);
            col.add_space(16.0);
            col.label(RichText::new("Unlock ByteFlow").font(theme::semibold(24.0)).color(p.text));
            col.add_space(2.0);
            col.label(RichText::new("Enter your password to open the vault.").color(p.text_muted));
            col.add_space(20.0);
            theme::card(&col).inner_margin(22).show(&mut col, |ui| {
                ui.set_width(w - 44.0);
                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                    let r = widgets::field(ui, "Password", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.password)
                                .password(true)
                                .hint_text("Password")
                                .margin(egui::Margin::symmetric(12, 10))
                                .desired_width(f32::INFINITY)
                                .interactive(!busy),
                        )
                    });
                    // Re-focus once the field is interactive again (after a failed try).
                    if !self.focused_once && !busy {
                        r.request_focus();
                        self.focused_once = true;
                    }
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if let Some(e) = error
                        && !busy
                    {
                        ui.add_space(2.0);
                        widgets::callout(ui, Tone::Danger, icons::WARNING_CIRCLE, e);
                    }
                    ui.add_space(10.0);
                    let label = if busy { "Unlocking…" } else { "Unlock" };
                    let clicked = ui
                        .add(Btn::new(Kind::Primary, label).icon(icons::LOCK_OPEN).enabled(!busy && !self.password.is_empty()).min_width(ui.available_width()))
                        .clicked();
                    if (clicked || enter) && !busy && !self.password.is_empty() {
                        let pw = secret(&self.password);
                        wipe_string(&mut self.password);
                        let auth = ctx.auth.clone();
                        ctx.spawn(async move { auth.submit_password(pw).await });
                        self.focused_once = false;
                    }
                    if busy {
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(RichText::new("Deriving your key — this takes a moment.").size(12.5).color(p.text_muted));
                        });
                    }
                });
            });
            col.add_space(16.0);
            col.label(RichText::new(format!("{}  The vault locks when idle, and when the computer locks or sleeps.", icons::SHIELD_CHECK)).size(12.0).color(p.text_subtle));
        });
    }
}
