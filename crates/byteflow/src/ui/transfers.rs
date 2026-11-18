//! Transfers page (spec §7): live progress, speed, ETA, and controls.

use std::sync::Arc;

use eframe::egui::{self, RichText};

use bf_app::prefs::TRANSFER_CONCURRENCY_OPTIONS;
use bf_app::session::Session;
use bf_app::transfers::{TransferKind, TransferStatus};
use bf_core::format;

use crate::bidi;
use crate::ctx::AppCtx;
use crate::theme::{self, pal};
use crate::ui::icons;
use crate::ui::widgets::{self, Btn, Kind, Tone};

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Filter {
    #[default]
    All,
    Active,
    Completed,
    Failed,
}

#[derive(Default)]
pub struct TransfersPage {
    filter: Filter,
}

fn status_tone(s: TransferStatus) -> Tone {
    match s {
        TransferStatus::Running => Tone::Primary,
        TransferStatus::Completed => Tone::Success,
        TransferStatus::Failed => Tone::Danger,
        TransferStatus::Paused => Tone::Warn,
        _ => Tone::Neutral,
    }
}

fn is_active(s: TransferStatus) -> bool {
    matches!(s, TransferStatus::Running | TransferStatus::Queued | TransferStatus::Paused)
}

impl TransfersPage {
    pub fn ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, session: &Arc<Session>) {
        let snap = session.transfers.snapshot();
        let p = *pal(ui);
        let subtitle = format!("{} running · {} queued · {}", snap.running, snap.queued, format::rate(snap.total_speed));
        widgets::page_header(ui, "Transfers", &subtitle, |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if ui.add(Btn::new(Kind::Secondary, "Clear finished").icon(icons::CHECK)).clicked() {
                let t = session.transfers.clone();
                ctx.spawn(async move { t.clear_finished().await });
            }
            if ui.add(Btn::new(Kind::Secondary, "Resume all").icon(icons::PLAY)).clicked() {
                let t = session.transfers.clone();
                ctx.spawn(async move { t.resume_all().await });
            }
            if ui.add(Btn::new(Kind::Secondary, "Pause all").icon(icons::PAUSE)).clicked() {
                let t = session.transfers.clone();
                ctx.spawn(async move { t.pause_all().await });
            }
            let mut c = session.transfers.concurrency();
            egui::ComboBox::from_id_salt("conc").selected_text(format!("{c} at a time")).width(120.0).show_ui(ui, |ui| {
                for n in TRANSFER_CONCURRENCY_OPTIONS {
                    ui.selectable_value(&mut c, n, format!("{n} at a time"));
                }
            });
            if c != session.transfers.concurrency() {
                session.transfers.set_concurrency(c);
                let _ = ctx.core.prefs.update(|p| p.transfer_concurrency = c);
            }
        });

        let count = |f: Filter| {
            snap.tasks
                .iter()
                .filter(|t| match f {
                    Filter::All => true,
                    Filter::Active => is_active(t.row.status),
                    Filter::Completed => t.row.status == TransferStatus::Completed,
                    Filter::Failed => t.row.status == TransferStatus::Failed,
                })
                .count()
        };
        widgets::tabs(
            ui,
            &mut self.filter,
            &[
                (Filter::All, "All", Some(count(Filter::All))),
                (Filter::Active, "Active", Some(count(Filter::Active))),
                (Filter::Completed, "Completed", Some(count(Filter::Completed))),
                (Filter::Failed, "Failed", Some(count(Filter::Failed))),
            ],
        );
        ui.add_space(12.0);

        let tasks: Vec<_> = snap
            .tasks
            .iter()
            .filter(|t| match self.filter {
                Filter::All => true,
                Filter::Active => is_active(t.row.status),
                Filter::Completed => t.row.status == TransferStatus::Completed,
                Filter::Failed => t.row.status == TransferStatus::Failed,
            })
            .collect();
        if tasks.is_empty() {
            let (title, body) = match self.filter {
                Filter::All => ("No transfers yet", "Uploads and downloads show up here with live progress. Closing the app pauses them; they resume on the next unlock."),
                Filter::Active => ("Nothing in progress", "Running, queued and paused transfers show up here."),
                Filter::Completed => ("Nothing finished yet", "Completed transfers stay here until you clear them."),
                Filter::Failed => ("No failures", "Transfers that fail after retrying show up here, with the reason."),
            };
            widgets::empty_state(ui, icons::ARROWS_DOWN_UP, title, body);
            return;
        }
        egui::ScrollArea::vertical().auto_shrink(false).show_rows(ui, 84.0, tasks.len(), |ui, range| {
            for t in &tasks[range] {
                let r = &t.row;
                theme::card(ui).inner_margin(egui::Margin::symmetric(16, 12)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let up = r.kind == TransferKind::Upload;
                        widgets::icon_tile(ui, if up { icons::UPLOAD } else { icons::DOWNLOAD }, if up { Tone::Primary } else { Tone::Violet }, 38.0);
                        ui.add_space(4.0);
                        ui.vertical(|ui| {
                            ui.set_max_width((ui.available_width() - 120.0).max(120.0));
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 8.0;
                                bidi::name(ui, bf_core::media::leaf_name(&r.remote_key), theme::medium(14.0), p.text);
                                widgets::badge(ui, r.status.as_str(), status_tone(r.status));
                                if r.delete_source_ref.is_some() && r.status != TransferStatus::Completed {
                                    widgets::badge_icon(ui, icons::TRASH, "deletes original", Tone::Warn);
                                }
                            });
                            let frac = if r.total_bytes > 0 { t.bytes as f32 / r.total_bytes as f32 } else { 0.0 };
                            if let Some(e) = &r.error {
                                ui.label(RichText::new(bidi::visual(e)).size(12.5).color(p.danger));
                            } else {
                                let mut label = format!("{} of {}", format::bytes(t.bytes), format::bytes(r.total_bytes));
                                if r.status == TransferStatus::Running {
                                    label.push_str(&format!(" · {}", format::rate(t.speed)));
                                    if let Some(eta) = t.eta_secs {
                                        label.push_str(&format!(" · {} left", format::duration_secs(eta)));
                                    }
                                }
                                ui.label(RichText::new(label).size(12.5).color(p.text_muted));
                                ui.add_space(2.0);
                                let color = match r.status {
                                    TransferStatus::Completed => p.success,
                                    TransferStatus::Paused => p.warn,
                                    _ => p.primary,
                                };
                                widgets::progress_bar(ui, frac, 5.0, color);
                            }
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            let id = r.id.clone();
                            let eng = session.transfers.clone();
                            match r.status {
                                TransferStatus::Running | TransferStatus::Queued => {
                                    let (e2, i2) = (eng.clone(), id.clone());
                                    if widgets::icon_btn(ui, icons::X, "Cancel").clicked() {
                                        ctx.spawn(async move { e2.cancel(&i2).await });
                                    }
                                    if widgets::icon_btn(ui, icons::PAUSE, "Pause").clicked() {
                                        ctx.spawn(async move { eng.pause(&id).await });
                                    }
                                }
                                TransferStatus::Paused | TransferStatus::Failed => {
                                    let (e2, i2) = (eng.clone(), id.clone());
                                    if widgets::icon_btn(ui, icons::X, "Cancel").clicked() {
                                        ctx.spawn(async move { e2.cancel(&i2).await });
                                    }
                                    if widgets::icon_btn(ui, if r.status == TransferStatus::Failed { icons::ARROW_CLOCKWISE } else { icons::PLAY }, "Resume").clicked() {
                                        ctx.spawn(async move { eng.resume(&id).await });
                                    }
                                }
                                _ => {
                                    if widgets::icon_btn(ui, icons::TRASH, "Remove from list").clicked() {
                                        ctx.spawn(async move { eng.remove(&id).await });
                                    }
                                }
                            }
                        });
                    });
                });
                ui.add_space(8.0);
            }
        });
        if snap.running > 0 {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
        }
    }
}
