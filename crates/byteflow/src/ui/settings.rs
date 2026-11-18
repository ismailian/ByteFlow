//! Settings (spec §13).

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::{self, RichText};

use bf_app::prefs::{AUTO_LOCK_OPTIONS, LINK_TTL_PRESETS, ThemeMode};
use bf_app::session::Session;
use bf_core::AppResult;
use bf_core::failures_ui::message_for;
use bf_core::format;
use bf_data::r2::R2Config;

use crate::ctx::{AppCtx, Task, secret, wipe_string};
use crate::theme;
use crate::ui::icons;
use crate::ui::widgets::{self, Btn, Kind, ModalSpec, Tone};

#[derive(Default)]
pub struct SettingsPage {
    pick_download: Option<Task<Option<PathBuf>>>,
    pick_cache: Option<Task<Option<PathBuf>>>,
    cache_move: Option<Task<AppResult<()>>>,
    usage: Option<Task<(u64, u64)>>,
    usage_val: Option<(u64, u64)>,
    edit_cfg: Option<R2Config>,
    cfg_password: String,
    cfg_task: Option<Task<AppResult<()>>>,
    pw_current: String,
    pw_new: String,
    pw_new2: String,
    pw_task: Option<Task<AppResult<()>>>,
    wipe_open: bool,
    wipe_typed: String,
}

impl Drop for SettingsPage {
    fn drop(&mut self) {
        for s in [&mut self.cfg_password, &mut self.pw_current, &mut self.pw_new, &mut self.pw_new2] {
            wipe_string(s);
        }
        if let Some(c) = self.edit_cfg.as_mut() {
            wipe_string(&mut c.secret_access_key);
        }
    }
}

fn row(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(36.0);
        let (rect, _) = ui.allocate_exact_size(egui::vec2(230.0, 22.0), egui::Sense::hover());
        ui.painter().text(rect.left_center(), egui::Align2::LEFT_CENTER, label, theme::medium(14.0), theme::pal(ui).text);
        add(ui);
    });
}

impl SettingsPage {
    /// Returns true when "Lock now" was pressed.
    pub fn ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, session: &Arc<Session>) -> bool {
        let mut lock = false;
        self.poll(ctx);
        let p = ctx.prefs();
        if self.usage.is_none() && self.usage_val.is_none() {
            let core = ctx.core.clone();
            self.usage = Some(ctx.task(async move {
                let c2 = core.clone();
                tokio::task::spawn_blocking(move || (bf_data::paths::sqlite_size(&c2.cache_db_path()), c2.thumbs.usage_bytes())).await.unwrap_or((0, 0))
            }));
        }
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            widgets::heading(ui, "Settings");

            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                widgets::section(ui, "Security");
                row(ui, "Auto-lock after idle", |ui| {
                    let mut v = p.auto_lock_seconds;
                    egui::ComboBox::from_id_salt("autolock").selected_text(format::duration_secs(v)).show_ui(ui, |ui| {
                        for o in AUTO_LOCK_OPTIONS {
                            ui.selectable_value(&mut v, o, format::duration_secs(o));
                        }
                    });
                    if v != p.auto_lock_seconds {
                        let _ = ctx.core.prefs.update(|x| x.auto_lock_seconds = v);
                    }
                });
                widgets::hint(ui, "The vault also locks immediately when the computer locks or sleeps.");
                row(ui, "Hide contents when unfocused", |ui| {
                    let mut v = p.privacy_blur_on_focus_loss;
                    if ui.checkbox(&mut v, "Cover the window on every focus loss (always on minimise)").changed() {
                        let _ = ctx.core.prefs.update(|x| x.privacy_blur_on_focus_loss = v);
                    }
                });
                row(ui, "Clear copied links after", |ui| {
                    let mut v = p.clipboard_clear_seconds;
                    let label = |s: u64| if s == 0 { "Never".to_string() } else { format::duration_secs(s) };
                    egui::ComboBox::from_id_salt("clip").selected_text(label(v)).show_ui(ui, |ui| {
                        for o in [0u64, 30, 60, 120, 300] {
                            ui.selectable_value(&mut v, o, label(o));
                        }
                    });
                    if v != p.clipboard_clear_seconds {
                        let _ = ctx.core.prefs.update(|x| x.clipboard_clear_seconds = v);
                    }
                });
                widgets::hint(ui, "Copied links are excluded from Windows clipboard history and cloud clipboard.");
                ui.add_space(4.0);
                if ui.add(Btn::new(Kind::Secondary, "Lock now").icon(icons::LOCK_SIMPLE)).clicked() {
                    lock = true;
                }
                ui.separator();
                widgets::section(ui, "Change login password");
                ui.add_space(4.0);
                ui.scope(|ui| {
                    ui.set_max_width(460.0);
                    widgets::field(ui, "Current password", |ui| widgets::password_field(ui, &mut self.pw_current, "Current password"));
                    widgets::field(ui, "New password", |ui| widgets::password_field(ui, &mut self.pw_new, "At least 10 characters"));
                    ui.add_space(-2.0);
                    widgets::strength_bar(ui, bf_app::strength::strength(&self.pw_new));
                    ui.add_space(4.0);
                    widgets::field(ui, "Repeat new password", |ui| widgets::password_field(ui, &mut self.pw_new2, "Repeat new password"));
                });
                let ok = self.pw_new.chars().count() >= 10 && self.pw_new == self.pw_new2 && !self.pw_current.is_empty();
                if widgets::primary_enabled(ui, ok && self.pw_task.is_none(), "Change password").clicked() {
                    let (a, c, n) = (ctx.auth.clone(), secret(&self.pw_current), secret(&self.pw_new));
                    self.pw_task = Some(ctx.task(async move { a.change_login_password(c, n).await }));
                }
            });
            ui.add_space(8.0);

            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                widgets::section(ui, "Transfers & sharing");
                row(ui, "Default share-link validity", |ui| {
                    let mut v = p.default_link_ttl_seconds;
                    egui::ComboBox::from_id_salt("ttl").selected_text(format::duration_secs(v)).show_ui(ui, |ui| {
                        for o in LINK_TTL_PRESETS {
                            ui.selectable_value(&mut v, o, format::duration_secs(o));
                        }
                    });
                    if v != p.default_link_ttl_seconds {
                        let _ = ctx.core.prefs.update(|x| x.default_link_ttl_seconds = v);
                    }
                });
                row(ui, "Download folder", |ui| {
                    ui.label(RichText::new(ctx.core.prefs.download_dir().display().to_string()).monospace());
                    if ui.button("Change…").clicked() && self.pick_download.is_none() {
                        self.pick_download = Some(ctx.task(async move { tokio::task::spawn_blocking(|| rfd::FileDialog::new().pick_folder()).await.ok().flatten() }));
                    }
                    if p.download_dir.is_some() && ui.button("Reset").clicked() {
                        let _ = ctx.core.prefs.update(|x| x.download_dir = None);
                    }
                    if ui.button("Open").clicked() {
                        crate::platform::open_folder(&ctx.core.prefs.download_dir());
                    }
                });
            });
            ui.add_space(8.0);

            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                widgets::section(ui, "Storage on this computer");
                let (cache_sz, thumbs_sz) = self.usage_val.unwrap_or((0, 0));
                row(ui, "Bucket listing cache", |ui| {
                    let path = ctx.core.cache_db_path().display().to_string();
                    ui.add(egui::Label::new(format!("{} · {path}", format::bytes(cache_sz))).truncate()).on_hover_text(path);
                });
                ui.horizontal(|ui| {
                    ui.add_space(238.0);
                    if ui.add_enabled(self.cache_move.is_none(), egui::Button::new("Move…")).clicked() && self.pick_cache.is_none() {
                        self.pick_cache = Some(ctx.task(async move { tokio::task::spawn_blocking(|| rfd::FileDialog::new().pick_folder()).await.ok().flatten() }));
                    }
                    if p.cache_db_dir.is_some() && ui.add_enabled(self.cache_move.is_none(), egui::Button::new("Use default location")).clicked() {
                        let core = ctx.core.clone();
                        self.cache_move = Some(ctx.task(async move { core.set_cache_dir(None).await }));
                    }
                });
                widgets::hint(ui, "A cache in a custom folder survives reinstalling and is not erased by a wipe; it holds object names, sizes and dates only — never credentials or contents. Other programs running as you can read it.");
                row(ui, "Thumbnails", |ui| {
                    ui.label(format!("{} · {} cached", format::bytes(thumbs_sz), ctx.core.thumbs.count()));
                    if ui.button("Clear").clicked() {
                        let _ = ctx.core.thumbs.clear();
                        self.usage_val = None;
                    }
                });
                row(ui, "Theme", |ui| {
                    let mut t = p.theme_mode;
                    widgets::segmented(ui, &mut t, &[(ThemeMode::Light, icons::SUN, "Light"), (ThemeMode::Dark, icons::MOON, "Dark"), (ThemeMode::System, icons::MONITOR, "System")]);
                    if t != p.theme_mode {
                        let _ = ctx.core.prefs.update(|x| x.theme_mode = t);
                    }
                });
            });
            ui.add_space(8.0);

            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                widgets::section(ui, "R2 connection");
                let cfg = session.config();
                ui.label(format!("Bucket {} · Account {}", cfg.bucket, bf_core::logging::redact(&cfg.account_id)));
                ui.label(RichText::new(cfg.effective_endpoint()).small().color(theme::muted(ui)));
                match self.edit_cfg.as_mut() {
                    None => {
                        if ui.button("Edit credentials…").clicked() {
                            let mut c = cfg.clone();
                            // The stored secret is never revealed; leave blank to keep it.
                            wipe_string(&mut c.secret_access_key);
                            c.secret_access_key = String::new();
                            self.edit_cfg = Some(c);
                        }
                    }
                    Some(c) => {
                        egui::Grid::new("editcfg").num_columns(2).show(ui, |ui| {
                            ui.label("Account ID");
                            widgets::text_field(ui, &mut c.account_id, "");
                            ui.end_row();
                            ui.label("Access Key ID");
                            widgets::text_field(ui, &mut c.access_key_id, "");
                            ui.end_row();
                            ui.label("Secret Access Key");
                            widgets::password_field(ui, &mut c.secret_access_key, "Leave blank to keep the current secret");
                            ui.end_row();
                            ui.label("Bucket");
                            widgets::text_field(ui, &mut c.bucket, "");
                            ui.end_row();
                            ui.label("Custom endpoint");
                            let mut ep = c.endpoint.clone().unwrap_or_default();
                            widgets::text_field(ui, &mut ep, "Optional");
                            c.endpoint = Some(ep);
                            ui.end_row();
                            ui.label("Login password");
                            widgets::password_field(ui, &mut self.cfg_password, "Required to reseal");
                            ui.end_row();
                        });
                        let (mut save, mut cancel) = (false, false);
                        ui.horizontal(|ui| {
                            save = widgets::primary_enabled(ui, !self.cfg_password.is_empty() && self.cfg_task.is_none(), "Save").clicked();
                            cancel = ui.button("Cancel").clicked();
                        });
                        if save {
                            let mut new = c.clone();
                            if new.secret_access_key.is_empty() {
                                new.secret_access_key = cfg.secret_access_key.clone();
                            }
                            let (a, pw) = (ctx.auth.clone(), secret(&self.cfg_password));
                            wipe_string(&mut self.cfg_password);
                            self.cfg_task = Some(ctx.task(async move { a.edit_config(pw, new).await }));
                        } else if cancel {
                            self.edit_cfg = None;
                            wipe_string(&mut self.cfg_password);
                        }
                    }
                }
            });
            ui.add_space(8.0);

            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                widgets::section(ui, "Danger zone");
                widgets::hint(ui, "Erase all credentials, keys, queues, thumbnails and temporary files from this computer. Files in the bucket are not touched.");
                if ui.add(Btn::new(Kind::Danger, "Wipe everything…").icon(icons::TRASH)).clicked() {
                    self.wipe_open = true;
                }
            });
            ui.add_space(8.0);

            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                widgets::section(ui, "About");
                ui.horizontal(|ui| {
                    widgets::icon_tile(ui, icons::VAULT, Tone::Primary, 40.0);
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new(format!("ByteFlow Desktop {}", crate::ui::shell::VERSION)).font(theme::semibold(14.0)).color(theme::pal(ui).text));
                        ui.label(RichText::new("Release build · Plus Jakarta Sans and Noto Sans Arabic (OFL) · Phosphor Icons (MIT)").size(12.5).color(theme::muted(ui)));
                    });
                });
            });
        });

        if self.wipe_open {
            let spec = ModalSpec::new("wipe", "Wipe everything?").subtitle("Files in the bucket are not touched").icon(icons::TRASH, Tone::Danger).width(440.0);
            let dismissed = widgets::modal(ui.ctx(), spec, |ui| {
                widgets::callout(ui, Tone::Danger, icons::WARNING, "All credentials, keys, queues, thumbnails and temporary files on this computer are erased. This can't be undone.");
                ui.add_space(6.0);
                widgets::field(ui, "Type WIPE to confirm", |ui| widgets::text_field(ui, &mut self.wipe_typed, "WIPE"));
                widgets::modal_footer(ui, |ui| {
                    if widgets::danger(ui, self.wipe_typed.trim() == "WIPE", "Wipe everything").clicked() {
                        let a = ctx.auth.clone();
                        ctx.spawn(async move { a.wipe_everything().await });
                        self.wipe_open = false;
                        self.wipe_typed.clear();
                    }
                    if widgets::secondary(ui, "Cancel").clicked() {
                        self.wipe_open = false;
                        self.wipe_typed.clear();
                    }
                });
            });
            if dismissed {
                self.wipe_open = false;
                self.wipe_typed.clear();
            }
        }
        lock
    }

    fn poll(&mut self, ctx: &AppCtx) {
        if let Some(v) = self.usage.as_ref().and_then(Task::poll) {
            self.usage = None;
            self.usage_val = Some(v);
        }
        if let Some(r) = self.pick_download.as_ref().and_then(Task::poll) {
            self.pick_download = None;
            if let Some(d) = r {
                let _ = ctx.core.prefs.update(|x| x.download_dir = Some(d));
            }
        }
        if let Some(r) = self.pick_cache.as_ref().and_then(Task::poll) {
            self.pick_cache = None;
            if let Some(d) = r {
                let core = ctx.core.clone();
                self.cache_move = Some(ctx.task(async move { core.set_cache_dir(Some(d)).await }));
            }
        }
        if let Some(r) = self.cache_move.as_ref().and_then(Task::poll) {
            self.cache_move = None;
            self.usage_val = None;
            match r {
                Ok(()) => ctx.toasts.ok("Cache location updated."),
                Err(e) => ctx.toasts.error(message_for(&e)),
            }
        }
        if let Some(r) = self.cfg_task.as_ref().and_then(Task::poll) {
            self.cfg_task = None;
            match r {
                Ok(()) => {
                    self.edit_cfg = None;
                    ctx.toasts.ok("Credentials updated.");
                }
                Err(e) => ctx.toasts.error(message_for(&e)),
            }
        }
        if let Some(r) = self.pw_task.as_ref().and_then(Task::poll) {
            self.pw_task = None;
            match r {
                Ok(()) => {
                    for s in [&mut self.pw_current, &mut self.pw_new, &mut self.pw_new2] {
                        wipe_string(s);
                    }
                    ctx.toasts.ok("Login password changed.");
                }
                Err(e) => ctx.toasts.error(message_for(&e)),
            }
        }
    }
}
