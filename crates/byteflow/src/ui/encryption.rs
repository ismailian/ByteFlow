//! Encryption settings and queue (spec §9.3). Key operations need the login
//! password (desktop is password-only, so it unwraps the DEK for the one
//! operation, §3.7).

use std::sync::Arc;

use eframe::egui::{self, RichText};

use bf_app::encryption::{EncPhase, EncStatus};
use bf_app::session::Session;
use bf_core::AppResult;
use bf_core::failures_ui::message_for;

use crate::bidi;
use crate::ctx::{AppCtx, Task, secret, wipe_string};
use crate::theme;
use crate::ui::icons;
use crate::ui::widgets::{self, Btn, Kind, ModalSpec, Tone};

#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyAction {
    Set,
    Change,
}

#[derive(Default)]
pub struct EncryptionPage {
    form: Option<KeyAction>,
    pass1: String,
    pass2: String,
    login: String,
    err: Option<String>,
    busy: Option<Task<AppResult<String>>>,
    confirm_remove: bool,
    external_pass: String,
}

impl Drop for EncryptionPage {
    fn drop(&mut self) {
        for s in [&mut self.pass1, &mut self.pass2, &mut self.login, &mut self.external_pass] {
            wipe_string(s);
        }
    }
}

impl EncryptionPage {
    fn clear(&mut self) {
        for s in [&mut self.pass1, &mut self.pass2, &mut self.login] {
            wipe_string(s);
        }
        self.err = None;
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, session: &Arc<Session>) {
        if let Some(r) = self.busy.as_ref().and_then(Task::poll) {
            self.busy = None;
            match r {
                Ok(m) => {
                    ctx.toasts.ok(m);
                    self.form = None;
                    self.clear();
                }
                Err(e) => self.err = Some(message_for(&e)),
            }
        }
        let enc = &session.encryption;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            widgets::heading(ui, "Encryption");
            widgets::hint(ui, "Encrypts object contents with AES-256-GCM before upload, so the bucket only ever holds ciphertext. Files stay recoverable with just the passphrase (the CLI and the mobile app can open them).");
            ui.add_space(8.0);
            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                let status = if !enc.has_key() {
                    "No key set".to_string()
                } else if enc.removing() {
                    "Removing encryption…".into()
                } else if enc.rotating() {
                    "Changing key (re-encrypting)…".into()
                } else {
                    format!("Key set · epoch {}", enc.active_epoch().unwrap_or(0))
                };
                ui.horizontal(|ui| {
                    let (icon, tone) = if enc.has_key() { (icons::SHIELD_CHECK, Tone::Success) } else { (icons::SHIELD, Tone::Neutral) };
                    widgets::icon_tile(ui, icon, tone, 40.0);
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new(status).font(theme::semibold(15.0)).color(theme::pal(ui).text));
                        let sub = if enc.has_key() { "Uploads can be encrypted on this computer before they leave it" } else { "Set a key to encrypt files before upload" };
                        ui.label(RichText::new(sub).size(12.5).color(theme::muted(ui)));
                    });
                });
                ui.add_space(8.0);
                let mut on = ctx.prefs().encrypt_new_uploads;
                if ui.add_enabled(enc.has_key(), egui::Checkbox::new(&mut on, "Encrypt new uploads")).changed() {
                    let _ = ctx.core.prefs.update(|p| p.encrypt_new_uploads = on);
                }
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    let idle = self.busy.is_none() && !enc.rotating() && !enc.removing();
                    if !enc.has_key() {
                        if ui.add(Btn::new(Kind::Primary, "Set key…").icon(icons::KEY).enabled(idle)).clicked() {
                            self.form = Some(KeyAction::Set);
                        }
                    } else {
                        if ui.add(Btn::new(Kind::Secondary, "Change key…").icon(icons::KEY).enabled(idle)).clicked() {
                            self.form = Some(KeyAction::Change);
                        }
                        if ui.add(Btn::new(Kind::Secondary, "Encrypt all existing files").icon(icons::LOCK_SIMPLE).enabled(idle)).clicked() {
                            let e = enc.clone();
                            self.busy = Some(ctx.task(async move { e.encrypt_all_existing().await.map(|n| format!("Queued {n} file(s) for encryption.")) }));
                        }
                        if ui.add(Btn::new(Kind::DangerOutline, "Remove encryption…").icon(icons::LOCK_OPEN).enabled(idle)).clicked() {
                            self.confirm_remove = true;
                        }
                    }
                });
                if let Some(a) = self.form {
                    ui.separator();
                    widgets::section(ui, if a == KeyAction::Set { "Set encryption key" } else { "Change encryption key" });
                    widgets::hint(ui, "The passphrase is all that's needed to recover files — with any device. It can't be reset; if you lose it, encrypted files are lost.");
                    widgets::password_field(ui, &mut self.pass1, "New passphrase (min 10 characters)");
                    widgets::strength_bar(ui, bf_app::strength::strength(&self.pass1));
                    widgets::password_field(ui, &mut self.pass2, "Repeat passphrase");
                    widgets::password_field(ui, &mut self.login, "Your login password (to authorise)");
                    if let Some(e) = &self.err {
                        widgets::error_text(ui, e);
                    }
                    let valid = self.pass1.chars().count() >= 10 && self.pass1 == self.pass2 && !self.login.is_empty();
                    ui.horizontal(|ui| {
                        if widgets::primary_enabled(ui, valid && self.busy.is_none(), "Confirm").clicked() {
                            let (auth, e, pass, login) = (ctx.auth.clone(), enc.clone(), secret(&self.pass1), secret(&self.login));
                            self.busy = Some(ctx.task(async move {
                                let dek = auth.dek(login).await?;
                                match a {
                                    KeyAction::Set => e.set_key(pass, dek).await.map(|_| "Encryption key set.".to_string()),
                                    KeyAction::Change => e.change_key(pass, dek).await.map(|n| format!("Re-encrypting {n} file(s) with the new key.")),
                                }
                            }));
                        }
                        if ui.button("Cancel").clicked() {
                            self.form = None;
                            self.clear();
                        }
                        if self.busy.is_some() {
                            ui.spinner();
                            ui.label("Deriving key…");
                        }
                    });
                }
            });

            ui.add_space(8.0);
            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                widgets::section(ui, "Passphrase for files encrypted elsewhere");
                widgets::hint(ui, "Objects encrypted by the CLI or another device. Held in memory for this session only; keys that open objects are remembered sealed under your master key.");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.external_pass).password(true).hint_text("Passphrase").margin(egui::Margin::symmetric(10, 8)).desired_width(280.0));
                    if ui.add(Btn::new(Kind::Secondary, "Use this session").icon(icons::KEY).enabled(!self.external_pass.is_empty())).clicked() {
                        enc.set_session_passphrase(secret(&self.external_pass));
                        wipe_string(&mut self.external_pass);
                        ctx.toasts.ok("Passphrase held for this session.");
                    }
                });
            });

            ui.add_space(8.0);
            let tasks = enc.tasks();
            ui.horizontal(|ui| {
                widgets::section(ui, &format!("Queue ({})", tasks.len()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Clear finished").clicked() {
                        enc.clear_finished();
                    }
                    let mut c = ctx.prefs().encryption_concurrency;
                    egui::ComboBox::from_id_salt("encconc").selected_text(format!("{c} at a time")).show_ui(ui, |ui| {
                        for n in 1..=4 {
                            ui.selectable_value(&mut c, n, format!("{n} at a time"));
                        }
                    });
                    if c != ctx.prefs().encryption_concurrency {
                        let _ = ctx.core.prefs.update(|p| p.encryption_concurrency = c);
                        enc.kick();
                    }
                });
            });
            if tasks.is_empty() {
                widgets::hint(ui, "Nothing queued.");
            }
            for t in tasks.iter().rev().take(300) {
                ui.horizontal(|ui| {
                    let op = match t.task.op {
                        bf_app::encryption::EncOp::Encrypt => "encrypt",
                        bf_app::encryption::EncOp::Decrypt => "decrypt",
                        bf_app::encryption::EncOp::ReEncrypt => "re-encrypt",
                    };
                    widgets::pill(ui, op, theme::LOCKED);
                    bidi::name(ui, bf_core::media::leaf_name(&t.task.remote_key), theme::regular(14.0), theme::pal(ui).text);
                    match t.task.status {
                        EncStatus::Running => {
                            let phase = match t.phase {
                                Some(EncPhase::Downloading) => "downloading",
                                Some(EncPhase::Transforming) => "transforming",
                                Some(EncPhase::Uploading) => "uploading",
                                None => "starting",
                            };
                            if t.total > 0 {
                                ui.add(egui::ProgressBar::new(t.done as f32 / t.total as f32).desired_width(160.0).text(phase));
                            } else {
                                ui.spinner();
                                ui.label(phase);
                            }
                        }
                        EncStatus::Queued => {
                            ui.label(RichText::new("queued").color(theme::muted(ui)));
                            if ui.small_button("Cancel").clicked() {
                                enc.cancel_task(&t.task.id);
                            }
                        }
                        EncStatus::Failed => {
                            ui.label(RichText::new(t.task.error.as_deref().unwrap_or("failed")).color(theme::DANGER));
                            if ui.small_button("Retry").clicked() {
                                enc.retry_task(&t.task.id);
                            }
                        }
                        EncStatus::Completed => {
                            ui.label(RichText::new("done").color(theme::SUCCESS));
                        }
                    }
                });
            }
            if tasks.iter().any(|t| t.task.status == EncStatus::Running) {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
        });

        if self.confirm_remove {
            let spec = ModalSpec::new("rm-enc", "Remove encryption?").subtitle("The bucket will hold plaintext again").icon(icons::LOCK_OPEN, Tone::Danger).width(440.0);
            let dismissed = widgets::modal(ui.ctx(), spec, |ui| {
                widgets::callout(ui, Tone::Warn, icons::WARNING, "Every app-encrypted object will be downloaded, decrypted and re-uploaded as plaintext. Then the key is deleted.");
                widgets::modal_footer(ui, |ui| {
                    if widgets::danger(ui, true, "Remove encryption").clicked() {
                        match session.encryption.remove_encryption() {
                            Ok(n) => ctx.toasts.info(format!("Decrypting {n} file(s).")),
                            Err(e) => ctx.toasts.error(message_for(&e)),
                        }
                        self.confirm_remove = false;
                    }
                    if widgets::secondary(ui, "Cancel").clicked() {
                        self.confirm_remove = false;
                    }
                });
            });
            if dismissed {
                self.confirm_remove = false;
            }
        }
    }
}
