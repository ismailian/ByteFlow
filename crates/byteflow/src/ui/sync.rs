//! Directory sync (spec §11): pairs, cancellable scan with live progress,
//! grouped review, explicit confirmation.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::{self, RichText};

use bf_app::session::Session;
use bf_app::sync::SyncClass;
use bf_core::AppResult;
use bf_core::failures_ui::message_for;
use bf_core::format;

use crate::bidi;
use crate::ctx::{AppCtx, Task};
use crate::theme;
use crate::ui::icons;
use crate::ui::widgets::{self, Btn, Kind};

#[derive(Default)]
pub struct SyncPage {
    new_dir: Option<PathBuf>,
    new_prefix: String,
    pick: Option<Task<Option<PathBuf>>>,
    adding: Option<Task<AppResult<()>>>,
    scan: Option<Task<AppResult<()>>>,
    selected: BTreeSet<String>,
    seeded_for: Option<String>,
    confirm: Option<Task<AppResult<bf_app::upload::UploadReport>>>,
    show_unchanged: bool,
}

impl SyncPage {
    pub fn ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, session: &Arc<Session>) {
        let sync = &session.sync;
        if let Some(p) = self.pick.as_ref().and_then(Task::poll) {
            self.pick = None;
            self.new_dir = p;
        }
        if let Some(r) = self.adding.as_ref().and_then(Task::poll) {
            self.adding = None;
            match r {
                Ok(()) => {
                    self.new_dir = None;
                    self.new_prefix.clear();
                }
                Err(e) => ctx.toasts.error(message_for(&e)),
            }
        }
        if let Some(r) = self.scan.as_ref().and_then(Task::poll) {
            self.scan = None;
            if let Err(e) = r
                && !e.is_cancelled()
            {
                ctx.toasts.error(message_for(&e));
            }
        }
        if let Some(r) = self.confirm.as_ref().and_then(Task::poll) {
            self.confirm = None;
            match r {
                Ok(rep) => {
                    if rep.plaintext_warning {
                        ctx.toasts.warn("Encryption is on but the key isn't available — files were uploaded as plaintext.");
                    }
                    ctx.toasts.ok(format!("Queued {} file(s) for upload.", rep.enqueued));
                }
                Err(e) => ctx.toasts.error(message_for(&e)),
            }
        }

        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            widgets::heading(ui, "Sync");
            widgets::hint(ui, "One-way (this computer to the bucket), and only after you review. Nothing is ever deleted on either side.");
            ui.add_space(6.0);
            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                widgets::section(ui, "Add a folder");
                ui.horizontal(|ui| {
                    if ui.add(Btn::new(Kind::Secondary, "Choose folder…").icon(icons::FOLDER_OPEN)).clicked() && self.pick.is_none() {
                        self.pick = Some(ctx.task(async move { tokio::task::spawn_blocking(|| rfd::FileDialog::new().pick_folder()).await.ok().flatten() }));
                    }
                    ui.label(self.new_dir.as_ref().map(|d| bidi::path(&d.display().to_string()).into_owned()).unwrap_or_else(|| "No folder chosen".into()));
                });
                ui.horizontal(|ui| {
                    ui.label("Bucket prefix");
                    let mut layouter = bidi::layouter(theme::regular(14.0), theme::pal(ui).text);
                    ui.add(egui::TextEdit::singleline(&mut self.new_prefix).hint_text("e.g. backups/photos").margin(egui::Margin::symmetric(10, 8)).desired_width(280.0).layouter(&mut layouter));
                    if widgets::primary_enabled(ui, self.new_dir.is_some() && self.adding.is_none(), "Add pair").clicked() {
                        let (s, d, p) = (sync.clone(), self.new_dir.clone().unwrap_or_default(), self.new_prefix.clone());
                        self.adding = Some(ctx.task(async move { s.add_pair(d, &p).await }));
                    }
                });
            });
            ui.add_space(6.0);
            let progress = sync.progress();
            for pair in sync.pairs() {
                theme::card(ui).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        widgets::icon_tile(ui, icons::FOLDER_OPEN, widgets::Tone::Primary, 34.0);
                        ui.label(RichText::new(bidi::path(&pair.local_dir_path)).font(theme::medium(14.0)).color(theme::pal(ui).text));
                        ui.label(RichText::new(icons::ARROW_RIGHT).color(theme::subtle(ui)));
                        ui.label(RichText::new(icons::DATABASE).color(theme::subtle(ui)));
                        ui.label(RichText::new(bidi::path(&format!("/{}", pair.remote_prefix))).font(theme::mono(13.0)).color(theme::muted(ui)));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Remove").clicked() {
                                let (s, id) = (sync.clone(), pair.id.clone());
                                ctx.spawn(async move {
                                    let _ = s.remove_pair(&id).await;
                                });
                            }
                            let scanning_this = progress.pair_id.as_deref() == Some(pair.id.as_str());
                            if scanning_this {
                                if ui.button("Cancel scan").clicked() {
                                    sync.cancel_scan();
                                }
                            } else if ui.add_enabled(!sync.scanning(), egui::Button::new("Scan")).clicked() {
                                let (s, id) = (sync.clone(), pair.id.clone());
                                self.scan = Some(ctx.task(async move { s.scan(&id).await }));
                            }
                        });
                    });
                    if let Some(t) = pair.last_scan_at {
                        widgets::hint(ui, &format!("Last scanned {}", format::relative_age(t, bf_app::now_ms())));
                    }
                    if progress.pair_id.as_deref() == Some(pair.id.as_str()) {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(format!("{} — {} files found", progress.phase, progress.files_seen));
                        });
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
                    }
                });
                ui.add_space(4.0);
            }
            if let Some(e) = sync.last_error() {
                widgets::error_text(ui, &e);
            }

            // ---- review
            if let Some(res) = sync.result() {
                if self.seeded_for.as_deref() != Some(res.pair_id.as_str()) {
                    // New and Modified are preselected.
                    self.selected = res.items.iter().filter(|i| matches!(i.class, SyncClass::New | SyncClass::Modified)).map(|i| i.rel.clone()).collect();
                    self.seeded_for = Some(res.pair_id.clone());
                }
                theme::card(ui).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    widgets::section(ui, "Review changes");
                    let groups = [(SyncClass::New, "New"), (SyncClass::Modified, "Modified"), (SyncClass::Unchanged, "Unchanged")];
                    for (class, label) in groups {
                        let items: Vec<_> = res.items.iter().filter(|i| i.class == class).collect();
                        if items.is_empty() || (class == SyncClass::Unchanged && !self.show_unchanged) {
                            if class == SyncClass::Unchanged && !items.is_empty() {
                                ui.checkbox(&mut self.show_unchanged, format!("Show {} unchanged", items.len()));
                            }
                            continue;
                        }
                        egui::CollapsingHeader::new(format!("{label} ({})", items.len())).default_open(class != SyncClass::Unchanged).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                if ui.small_button("Select all").clicked() {
                                    for i in &items {
                                        self.selected.insert(i.rel.clone());
                                    }
                                }
                                if ui.small_button("None").clicked() {
                                    for i in &items {
                                        self.selected.remove(&i.rel);
                                    }
                                }
                            });
                            egui::ScrollArea::vertical().id_salt(label).max_height(240.0).show_rows(ui, 22.0, items.len(), |ui, range| {
                                for it in &items[range] {
                                    ui.horizontal(|ui| {
                                        let mut on = self.selected.contains(&it.rel);
                                        if ui.checkbox(&mut on, bidi::path(&it.rel)).changed() {
                                            if on {
                                                self.selected.insert(it.rel.clone());
                                            } else {
                                                self.selected.remove(&it.rel);
                                            }
                                        }
                                        ui.label(RichText::new(format::bytes(it.size)).small().color(theme::muted(ui)));
                                    });
                                }
                            });
                        });
                    }
                    if !res.remote_only.is_empty() {
                        egui::CollapsingHeader::new(format!("Only in the bucket ({}) — informational, never deleted", res.remote_only.len())).show(ui, |ui| {
                            for r in res.remote_only.iter().take(500) {
                                ui.label(RichText::new(bidi::path(r)).small());
                            }
                        });
                    }
                    let bytes: u64 = res.items.iter().filter(|i| self.selected.contains(&i.rel)).map(|i| i.size).sum();
                    ui.horizontal(|ui| {
                        let n = self.selected.len();
                        if widgets::primary_enabled(ui, n > 0 && self.confirm.is_none(), &format!("Upload {n} file(s) · {}", format::bytes(bytes))).clicked() {
                            let (s, id, rels) = (sync.clone(), res.pair_id.clone(), self.selected.iter().cloned().collect::<Vec<_>>());
                            let encrypt = ctx.prefs().encrypt_new_uploads && session.encryption.has_key();
                            self.confirm = Some(ctx.task(async move { s.confirm(&id, &rels, encrypt).await }));
                            self.seeded_for = None;
                        }
                        if ui.button("Discard").clicked() {
                            sync.clear_result();
                            self.seeded_for = None;
                        }
                    });
                });
            }
        });
    }
}
