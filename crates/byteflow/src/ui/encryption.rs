//! Encryption settings and queue (spec §9.3). Key operations need the login
//! password (desktop is password-only, so it unwraps the DEK for the one
//! operation, §3.7).

use std::sync::Arc;

use eframe::egui::{self, RichText};

use bf_app::encryption::{EncOp, EncPhase, EncStatus, EncTaskView, EncryptionEngine};
use bf_app::session::Session;
use bf_core::AppResult;
use bf_core::failures_ui::message_for;
use bf_core::format;

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

            ui.add_space(16.0);
            queue_ui(ui, ctx, enc);
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

/// Rows shown at most (newest first); the header counts all of them.
const MAX_ROWS: usize = 300;

/// The queue: a header with live counts and overall progress, then one card
/// holding a compact row per task (icon, name, step, slim progress bar,
/// action). Running first, then queued, failed, finished.
fn queue_ui(ui: &mut egui::Ui, ctx: &AppCtx, enc: &Arc<EncryptionEngine>) {
    let p = *theme::pal(ui);
    let mut tasks = enc.tasks();
    let count = |st: EncStatus| tasks.iter().filter(|t| t.task.status == st).count();
    let (running, queued, failed, done) = (count(EncStatus::Running), count(EncStatus::Queued), count(EncStatus::Failed), count(EncStatus::Completed));

    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            widgets::section(ui, "Queue");
            let summary = if tasks.is_empty() {
                "Nothing queued".to_string()
            } else {
                let mut parts = Vec::new();
                if running > 0 {
                    parts.push(format!("{running} running"));
                }
                if queued > 0 {
                    parts.push(format!("{queued} waiting"));
                }
                if failed > 0 {
                    parts.push(format!("{failed} failed"));
                }
                if done > 0 {
                    parts.push(format!("{done} done"));
                }
                parts.join(" · ")
            };
            ui.label(RichText::new(summary).size(12.5).color(p.text_muted));
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add(Btn::new(Kind::Ghost, "Clear finished").enabled(done > 0)).clicked() {
                enc.clear_finished();
            }
            let mut c = ctx.prefs().encryption_concurrency;
            egui::ComboBox::from_id_salt("encconc").selected_text(format!("{c} at a time")).width(120.0).show_ui(ui, |ui| {
                widgets::menu_style(ui);
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
    ui.add_space(10.0);

    if tasks.is_empty() {
        widgets::empty_state(ui, icons::SHIELD_CHECK, "Nothing to encrypt right now", "Files you encrypt, decrypt or re-key show up here with live progress.");
        return;
    }

    // Overall progress across everything still in the list.
    let total = tasks.len();
    let finished = done + failed;
    if running + queued > 0 {
        let partial: f32 = tasks.iter().filter(|t| t.task.status == EncStatus::Running).map(task_fraction).sum();
        let frac = (done as f32 + partial) / total as f32;
        widgets::progress(ui, frac, format!("{finished} of {total} finished"));
        ui.add_space(10.0);
    }

    let rank = |st: EncStatus| match st {
        EncStatus::Running => 0,
        EncStatus::Queued => 1,
        EncStatus::Failed => 2,
        EncStatus::Completed => 3,
    };
    tasks.reverse(); // newest first within each group
    tasks.sort_by_key(|t| rank(t.task.status));
    let shown = tasks.len().min(MAX_ROWS);

    theme::card(ui).inner_margin(egui::Margin::symmetric(6, 6)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        for (i, t) in tasks.iter().take(shown).enumerate() {
            if i > 0 {
                let r = ui.available_rect_before_wrap();
                ui.painter().hline((r.left() + 58.0)..=(r.right() - 12.0), r.top(), egui::Stroke::new(1.0, p.border));
            }
            task_row(ui, enc, t);
        }
        if tasks.len() > shown {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(58.0);
                widgets::hint(ui, &format!("{} more not shown", tasks.len() - shown));
            });
            ui.add_space(8.0);
        }
    });

    if running > 0 {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(150));
    }
}

/// Progress of one running task across its three steps (download,
/// transform, upload), 0..1.
fn task_fraction(t: &EncTaskView) -> f32 {
    let step = match t.phase {
        None | Some(EncPhase::Downloading) => 0.0,
        Some(EncPhase::Transforming) => 1.0,
        Some(EncPhase::Uploading) => 2.0,
    };
    let within = if t.total > 0 { (t.done as f32 / t.total as f32).clamp(0.0, 1.0) } else { 0.0 };
    (step + within) / 3.0
}

fn task_row(ui: &mut egui::Ui, enc: &Arc<EncryptionEngine>, t: &EncTaskView) {
    let p = *theme::pal(ui);
    let (op_icon, op_label, verb) = match t.task.op {
        EncOp::Encrypt => (icons::LOCK_SIMPLE, "Encrypt", "Encrypting"),
        EncOp::Decrypt => (icons::LOCK_OPEN, "Decrypt", "Decrypting"),
        EncOp::ReEncrypt => (icons::ARROWS_CLOCKWISE, "Re-key", "Re-encrypting"),
    };
    let tone = match t.task.status {
        EncStatus::Completed => Tone::Success,
        EncStatus::Failed => Tone::Danger,
        EncStatus::Queued => Tone::Neutral,
        EncStatus::Running => Tone::Violet,
    };
    egui::Frame::new().inner_margin(egui::Margin { left: 10, right: 8, top: 10, bottom: 10 }).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            widgets::icon_tile(ui, if t.task.status == EncStatus::Completed { icons::CHECK } else { op_icon }, tone, 34.0);
            // Actions are laid out first from the right so the text column
            // knows its width.
            let actions_w = 40.0;
            let text_w = (ui.available_width() - actions_w - 12.0).max(120.0);
            ui.allocate_ui_with_layout(egui::vec2(text_w, 34.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(text_w);
                ui.spacing_mut().item_spacing.y = 3.0;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let name_w = (text_w - 90.0).max(60.0);
                    ui.allocate_ui_with_layout(egui::vec2(name_w, 20.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.set_max_width(name_w);
                        bidi::name(ui, bf_core::media::leaf_name(&t.task.remote_key), theme::medium(14.0), p.text);
                    });
                });
                match t.task.status {
                    EncStatus::Running => {
                        let (n, step) = match t.phase {
                            None => (1, "Starting"),
                            Some(EncPhase::Downloading) => (1, "Downloading"),
                            Some(EncPhase::Transforming) => (2, verb),
                            Some(EncPhase::Uploading) => (3, "Uploading"),
                        };
                        let mut line = format!("Step {n} of 3 · {step}");
                        if t.total > 0 {
                            line.push_str(&format!(" · {} of {}", format::bytes(t.done), format::bytes(t.total)));
                        }
                        ui.label(RichText::new(line).size(12.5).color(p.text_muted));
                        widgets::progress_bar(ui, task_fraction(t), 4.0, p.primary);
                    }
                    EncStatus::Queued => {
                        ui.label(RichText::new(format!("{op_label} · waiting")).size(12.5).color(p.text_subtle));
                    }
                    EncStatus::Failed => {
                        let e = t.task.error.as_deref().unwrap_or("Failed");
                        ui.add(egui::Label::new(RichText::new(format!("{op_label} failed · {}", bidi::visual(e))).size(12.5).color(p.danger)).truncate());
                    }
                    EncStatus::Completed => {
                        let past = match t.task.op {
                            EncOp::Encrypt => "Encrypted",
                            EncOp::Decrypt => "Decrypted",
                            EncOp::ReEncrypt => "Re-encrypted",
                        };
                        ui.label(RichText::new(past).size(12.5).color(p.success));
                    }
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| match t.task.status {
                EncStatus::Queued => {
                    if widgets::icon_btn(ui, icons::X, "Cancel").clicked() {
                        enc.cancel_task(&t.task.id);
                    }
                }
                EncStatus::Failed => {
                    if widgets::icon_btn(ui, icons::ARROW_CLOCKWISE, "Retry").clicked() {
                        enc.retry_task(&t.task.id);
                    }
                }
                EncStatus::Running => {
                    ui.add(egui::Spinner::new().size(16.0).color(p.text_subtle));
                }
                EncStatus::Completed => {}
            });
        });
    });
}
