//! Onboarding (spec §3.2): the login password, then R2 config with "Test
//! connection". The destruction password is optional since 0.4.0 and is
//! turned on from Settings, so a fresh install never asks for it here.

use eframe::egui::{self, CornerRadius, RichText, Sense, vec2};

use bf_app::auth::{AuthController, VaultState};
use bf_app::strength;
use bf_data::r2::R2Config;

use crate::ctx::{AppCtx, Task, secret, wipe_string};
use crate::theme::{self, pal};
use crate::ui::icons;
use crate::ui::unlock::brand_mark;
use crate::ui::widgets::{self, Btn, Kind, Tone};

#[derive(Default)]
pub struct Onboarding {
    login: String,
    login2: String,
    error: Option<String>,
    pw_task: Option<Task<Result<(), String>>>,

    account_id: String,
    access_key: String,
    secret_key: String,
    bucket: String,
    public_url: String,
    endpoint: String,
    test: Option<Task<Result<(), String>>>,
    test_result: Option<Result<(), String>>,
    save: Option<Task<Result<(), String>>>,
}

impl Drop for Onboarding {
    fn drop(&mut self) {
        for s in [&mut self.login, &mut self.login2, &mut self.secret_key] {
            wipe_string(s);
        }
    }
}

/// Centered column with the brand header, a step indicator and a card.
fn shell(ui: &mut egui::Ui, step: usize, title: &str, subtitle: &str, width: f32, add: impl FnOnce(&mut egui::Ui)) {
    let p = *pal(ui);
    egui::CentralPanel::default().frame(egui::Frame::new().fill(p.canvas)).show(ui, |ui| {
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.add_space(36.0);
            ui.vertical_centered(|ui| {
                let w = width.min(ui.available_width() - 32.0);
                ui.set_max_width(w);
                ui.horizontal(|ui| {
                    ui.add_space((w - 150.0) / 2.0);
                    brand_mark(ui, 34.0);
                    ui.add_space(4.0);
                    ui.label(RichText::new("ByteFlow").font(theme::bold(19.0)).color(p.text));
                });
                ui.add_space(22.0);
                // Step indicator.
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let seg = (w - 6.0) / 2.0;
                    for i in 1..=2 {
                        let (r, _) = ui.allocate_exact_size(vec2(seg, 4.0), Sense::hover());
                        ui.painter().rect_filled(r, CornerRadius::same(2), if i <= step { p.primary } else { p.surface_3 });
                    }
                });
                ui.add_space(8.0);
                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.label(RichText::new(format!("Step {step} of 2")).font(theme::semibold(12.0)).color(p.primary_text));
                    ui.label(RichText::new(title).font(theme::semibold(24.0)).color(p.text));
                    ui.add(egui::Label::new(RichText::new(subtitle).color(p.text_muted)).wrap());
                    ui.add_space(14.0);
                    theme::card(ui).inner_margin(22).show(ui, |ui| {
                        ui.set_width(w - 44.0);
                        add(ui);
                    });
                });
                ui.add_space(28.0);
            });
        });
    });
}

impl Onboarding {
    fn config(&self) -> R2Config {
        R2Config {
            account_id: self.account_id.clone(),
            access_key_id: self.access_key.clone(),
            secret_access_key: self.secret_key.clone(),
            bucket: self.bucket.clone(),
            public_url: Some(self.public_url.clone()),
            endpoint: Some(self.endpoint.clone()),
        }
        .normalized()
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, state: &VaultState) {
        match state {
            VaultState::NeedsPasswords => self.passwords(ui, ctx),
            _ => self.r2(ui, ctx),
        }
    }

    fn passwords(&mut self, ui: &mut egui::Ui, ctx: &AppCtx) {
        if let Some(r) = self.pw_task.as_ref().and_then(Task::poll) {
            self.pw_task = None;
            match r {
                Ok(()) => {
                    for s in [&mut self.login, &mut self.login2] {
                        wipe_string(s);
                    }
                    self.error = None;
                }
                Err(e) => self.error = Some(e),
            }
        }
        let busy = self.pw_task.is_some();
        shell(ui, 1, "Protect this vault", "One password opens the vault on this computer.", 540.0, |ui| {
            widgets::section(ui, "Login & recovery password");
            widgets::hint(ui, "Unlocks the vault. It is also your recovery path, so keep it safe — it can't be reset.");
            ui.add_space(6.0);
            widgets::field(ui, "Password", |ui| widgets::password_field(ui, &mut self.login, "At least 10 characters"));
            ui.add_space(-2.0);
            widgets::strength_bar(ui, strength::strength(&self.login));
            ui.add_space(4.0);
            widgets::field(ui, "Repeat password", |ui| widgets::password_field(ui, &mut self.login2, "Repeat password"));
            widgets::callout(ui, Tone::Neutral, icons::INFO, "Biometric unlock isn't available on desktop; the password is the only way in. An optional destruction password, which silently erases this vault, can be turned on later in Settings.");
            if let Some(e) = &self.error {
                ui.add_space(4.0);
                widgets::error_text(ui, e);
            }
            let valid = strength::validate_login(&self.login, &self.login2);
            if let Err(msg) = &valid
                && !self.login.is_empty()
                && !self.login2.is_empty()
            {
                ui.add_space(4.0);
                widgets::hint(ui, msg);
            }
            widgets::modal_footer(ui, |ui| {
                if ui.add(Btn::new(Kind::Primary, "Continue").icon(icons::ARROW_RIGHT).enabled(valid.is_ok() && !busy)).clicked() {
                    let (auth, l) = (ctx.auth.clone(), secret(&self.login));
                    self.pw_task = Some(ctx.task(async move { auth.set_login_password(l).await }));
                }
                if busy {
                    ui.spinner();
                }
            });
        });
    }

    fn r2(&mut self, ui: &mut egui::Ui, ctx: &AppCtx) {
        if let Some(r) = self.test.as_ref().and_then(Task::poll) {
            self.test = None;
            self.test_result = Some(r);
        }
        if let Some(r) = self.save.as_ref().and_then(Task::poll) {
            self.save = None;
            match r {
                Ok(()) => wipe_string(&mut self.secret_key),
                Err(e) => self.test_result = Some(Err(e)),
            }
        }
        let busy = self.test.is_some() || self.save.is_some();
        shell(ui, 2, "Connect your R2 bucket", "Credentials are sealed with AES-256-GCM under a key only your password can unwrap.", 580.0, |ui| {
            let half = (ui.available_width() - 12.0) / 2.0;
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                ui.vertical(|ui| {
                    ui.set_width(half);
                    widgets::field(ui, "Account ID", |ui| widgets::text_field(ui, &mut self.account_id, "Cloudflare account ID"));
                });
                ui.vertical(|ui| {
                    ui.set_width(half);
                    widgets::field(ui, "Bucket", |ui| widgets::text_field(ui, &mut self.bucket, "bucket-name"));
                });
            });
            widgets::field(ui, "Access Key ID", |ui| widgets::text_field(ui, &mut self.access_key, "R2 API token access key"));
            widgets::field(ui, "Secret Access Key", |ui| widgets::password_field(ui, &mut self.secret_key, "Never shown again after saving"));
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                ui.vertical(|ui| {
                    ui.set_width(half);
                    widgets::field(ui, "Public URL (optional)", |ui| widgets::text_field(ui, &mut self.public_url, "https://files.example.com"));
                });
                ui.vertical(|ui| {
                    ui.set_width(half);
                    widgets::field(ui, "Custom endpoint (optional)", |ui| widgets::text_field(ui, &mut self.endpoint, "https://<id>.eu.r2.cloudflarestorage.com"));
                });
            });
            let cfg = self.config();
            let valid = cfg.validate();
            match &self.test_result {
                Some(Ok(())) => widgets::callout(ui, Tone::Success, icons::CHECK_CIRCLE, "Connection works."),
                Some(Err(e)) => widgets::callout(ui, Tone::Danger, icons::WARNING_CIRCLE, e),
                None => {}
            }
            if let Err(e) = valid
                && !self.bucket.is_empty()
            {
                widgets::hint(ui, e);
            }
            widgets::modal_footer(ui, |ui| {
                if ui.add(Btn::new(Kind::Primary, "Save & open vault").icon(icons::ARROW_RIGHT).enabled(valid.is_ok() && !busy)).clicked() {
                    let (auth, c) = (ctx.auth.clone(), cfg.clone());
                    self.save = Some(ctx.task(async move { auth.save_config(c).await }));
                }
                if ui.add(Btn::new(Kind::Secondary, "Test connection").icon(icons::CLOUD_CHECK).enabled(valid.is_ok() && !busy)).clicked() {
                    let c = cfg.clone();
                    self.test_result = None;
                    self.test = Some(ctx.task(async move { AuthController::test_connection(&c).await }));
                }
                if busy {
                    ui.spinner();
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    if ui.add(Btn::new(Kind::Ghost, "Back").icon(icons::ARROW_LEFT).enabled(!busy)).clicked() {
                        ctx.auth.back_to_passwords();
                    }
                });
            });
        });
    }
}
