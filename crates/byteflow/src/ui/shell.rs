//! The unlocked shell: left navigation sidebar, the current page, and (on
//! Files) the right-hand details panel.

use std::sync::Arc;

use eframe::egui::{self, Align2, Color32, CornerRadius, RichText, Sense, Stroke, vec2};
use tokio::sync::broadcast;

use bf_app::session::Session;
use bf_app::transfers::{SourceDeletion, SourceDeletionOutcome, TransferStatus};

use crate::ctx::AppCtx;
use crate::theme::{self, Palette, pal};
use crate::ui::icons;
use crate::bidi;
use crate::ui::{encryption, files, links, settings, sync, transfers};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Files,
    Transfers,
    Encryption,
    Sync,
    Links,
    Settings,
}

pub struct Shell {
    page: Page,
    files: files::FilesPage,
    transfers: transfers::TransfersPage,
    encryption: encryption::EncryptionPage,
    sync: sync::SyncPage,
    links: links::LinksPage,
    settings: settings::SettingsPage,
    deletions: broadcast::Receiver<SourceDeletion>,
}

/// A sidebar navigation row. Returns the click response.
fn nav_item(ui: &mut egui::Ui, p: &Palette, icon: &str, label: &str, selected: bool, badge: usize) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::click());
    let l = label.to_owned();
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, selected, &l));
    let hovered = resp.hovered();
    if selected {
        ui.painter().rect_filled(rect, CornerRadius::same(9), p.primary_soft);
        let bar = egui::Rect::from_min_size(egui::pos2(rect.left(), rect.center().y - 9.0), vec2(3.0, 18.0));
        ui.painter().rect_filled(bar, CornerRadius::same(2), p.primary);
    } else if hovered {
        ui.painter().rect_filled(rect, CornerRadius::same(9), p.surface_2);
    }
    let (fg, ic) = if selected { (p.primary_text, p.primary_text) } else if hovered { (p.text, p.text) } else { (p.text_muted, p.text_subtle) };
    ui.painter().text(egui::pos2(rect.left() + 22.0, rect.center().y), Align2::CENTER_CENTER, icon, theme::regular(18.0), ic);
    let font = if selected { theme::semibold(14.0) } else { theme::medium(14.0) };
    ui.painter().text(egui::pos2(rect.left() + 42.0, rect.center().y), Align2::LEFT_CENTER, label, font, fg);
    if badge > 0 {
        let g = ui.painter().layout_no_wrap(badge.to_string(), theme::semibold(11.0), Color32::PLACEHOLDER);
        let w = (g.size().x + 12.0).max(22.0);
        let br = egui::Rect::from_center_size(egui::pos2(rect.right() - 10.0 - w / 2.0, rect.center().y), vec2(w, 20.0));
        let (bf, bt) = if selected { (p.primary, Color32::WHITE) } else { (p.surface_3, p.text_muted) };
        ui.painter().rect_filled(br, CornerRadius::same(10), bf);
        ui.painter().galley(br.center() - g.size() / 2.0, g, bt);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn group_label(ui: &mut egui::Ui, p: &Palette, text: &str) {
    ui.add_space(14.0);
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        ui.label(RichText::new(text).font(theme::semibold(11.0)).color(p.text_subtle));
    });
    ui.add_space(2.0);
}

impl Shell {
    pub fn new(_ctx: &AppCtx, session: &Arc<Session>) -> Self {
        Self {
            page: Page::Files,
            files: files::FilesPage::default(),
            transfers: transfers::TransfersPage::default(),
            encryption: encryption::EncryptionPage::default(),
            sync: sync::SyncPage::default(),
            links: links::LinksPage,
            settings: settings::SettingsPage::default(),
            deletions: session.transfers.source_deletions(),
        }
    }

    pub fn cleanup_temps(&mut self) {
        self.files.cleanup_temps();
    }

    fn sidebar(&mut self, ui: &mut egui::Ui, session: &Arc<Session>, lock: &mut bool) {
        let p = *pal(ui);
        let snap = session.transfers.snapshot();
        ui.spacing_mut().item_spacing.y = 2.0;

        // Brand.
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            let (r, _) = ui.allocate_exact_size(vec2(34.0, 34.0), Sense::hover());
            ui.painter().rect_filled(r, CornerRadius::same(10), p.primary);
            ui.painter().text(r.center(), Align2::CENTER_CENTER, icons::VAULT, theme::regular(19.0), Color32::WHITE);
            ui.add_space(4.0);
            ui.label(RichText::new("ByteFlow").font(theme::bold(18.0)).color(p.text));
        });
        ui.add_space(14.0);

        // Bucket chip with listing status.
        let status = session.cache.status();
        let (dot, state) = if status.scanning {
            (p.warn, format!("Refreshing listing… {} objects", status.listed))
        } else if status.last_error.is_some() {
            (p.danger, "Last refresh failed".to_string())
        } else {
            (p.success, "Connected".to_string())
        };
        let cfg = session.config();
        let overview = session.overview.data();
        let chip = egui::Frame::new()
            .fill(p.surface_2)
            .corner_radius(CornerRadius::same(10))
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.label(RichText::new(icons::DATABASE).size(16.0).color(p.text_muted));
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.set_max_width(ui.available_width() - 18.0);
                        bidi::name(ui, &cfg.bucket, theme::semibold(13.0), p.text);
                        // Space used, from the local listing (R2 has no quota to fill).
                        let used = if overview.has_scanned { format!("{} used", bf_core::format::bytes(overview.bytes)) } else { "Cloudflare R2".to_string() };
                        ui.label(RichText::new(used).size(11.5).color(p.text_subtle));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                        ui.painter().circle_filled(r.center(), 4.0, dot);
                    });
                });
            });
        let totals = if overview.has_scanned { format!("\n{} in {} files", bf_core::format::bytes(overview.bytes), overview.files) } else { String::new() };
        chip.response.on_hover_text(format!("Cloudflare R2 · {state}{totals}\n{}", cfg.effective_endpoint()));

        let active = snap.running + snap.queued;
        let failed = snap.tasks.iter().filter(|t| t.row.status == TransferStatus::Failed).count();
        let enc_active = session.encryption.locked_keys().len();
        let mut go = None;
        group_label(ui, &p, "LIBRARY");
        if nav_item(ui, &p, icons::FOLDERS, "Files", self.page == Page::Files, 0).clicked() {
            go = Some(Page::Files);
        }
        group_label(ui, &p, "ACTIVITY");
        for (page, icon, label, badge) in [
            (Page::Transfers, icons::ARROWS_DOWN_UP, "Transfers", active + failed),
            (Page::Encryption, icons::SHIELD_CHECK, "Encryption", enc_active),
            (Page::Sync, icons::ARROWS_CLOCKWISE, "Sync", 0),
            (Page::Links, icons::LINK_SIMPLE, "Shared links", 0),
        ] {
            if nav_item(ui, &p, icon, label, self.page == page, badge).clicked() {
                go = Some(page);
            }
        }

        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(RichText::new(format!("ByteFlow v{VERSION}")).size(11.5).color(p.text_subtle));
            });
            ui.add_space(6.0);
            if nav_item(ui, &p, icons::LOCK_SIMPLE, "Lock vault", false, 0).on_hover_text("Lock now (also locks when idle, or when the computer locks)").clicked() {
                *lock = true;
            }
            if nav_item(ui, &p, icons::GEAR_SIX, "Settings", self.page == Page::Settings, 0).clicked() {
                go = Some(Page::Settings);
            }
            ui.add_space(6.0);
            // Live transfer card.
            if active > 0 {
                let (done, total) = snap
                    .tasks
                    .iter()
                    .filter(|t| matches!(t.row.status, TransferStatus::Running | TransferStatus::Queued))
                    .fold((0u64, 0u64), |(d, t), v| (d + v.bytes, t + v.row.total_bytes));
                let frac = if total > 0 { done as f32 / total as f32 } else { 0.0 };
                let card = egui::Frame::new()
                    .fill(p.surface)
                    .stroke(Stroke::new(1.0, p.border))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(egui::Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(icons::ARROWS_DOWN_UP).color(p.primary_text));
                            ui.label(RichText::new(format!("{active} transfer{}", if active == 1 { "" } else { "s" })).font(theme::semibold(13.0)).color(p.text));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(RichText::new(bf_core::format::rate(snap.total_speed)).size(12.0).color(p.text_muted));
                            });
                        });
                        ui.add_space(4.0);
                        crate::ui::widgets::progress_bar(ui, frac, 5.0, p.primary);
                    });
                if ui.interact(card.response.rect, egui::Id::unique("nav-transfer-card"), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    go = Some(Page::Transfers);
                }
                ui.add_space(4.0);
            }
        });
        if let Some(g) = go {
            self.page = g;
        }
    }

    /// Returns true when the user asked to lock.
    pub fn ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, session: &Arc<Session>) -> bool {
        // Every auto-delete attempt is reported — never silent (§8).
        while let Ok(d) = self.deletions.try_recv() {
            let name = bf_core::media::leaf_name(&d.remote_key).to_string();
            match d.outcome {
                SourceDeletionOutcome::Deleted => ctx.toasts.ok(format!("Uploaded and verified — deleted the original of {name}.")),
                SourceDeletionOutcome::Failed => ctx.toasts.warn(format!("Uploaded {name}, but the original couldn't be deleted. It was kept.")),
                SourceDeletionOutcome::Unverified => ctx.toasts.warn(format!("Couldn't verify {name} in the bucket, so the original was kept.")),
            }
        }

        let p = *pal(ui);
        let mut lock = false;
        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(244.0)
            .frame(egui::Frame::new().fill(p.surface).inner_margin(egui::Margin { left: 14, right: 14, top: 18, bottom: 14 }))
            .show(ui, |ui| self.sidebar(ui, session, &mut lock));

        if self.page == Page::Files && self.files.details_open(ctx) {
            egui::Panel::right("details")
                .resizable(true)
                .default_size(320.0)
                .size_range(280.0..=460.0)
                .frame(egui::Frame::new().fill(p.surface).inner_margin(egui::Margin { left: 18, right: 18, top: 18, bottom: 14 }))
                .show(ui, |ui| self.files.details_ui(ui, ctx, session));
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(p.canvas).inner_margin(egui::Margin { left: 28, right: 28, top: 22, bottom: 16 }))
            .show(ui, |ui| match self.page {
                Page::Files => self.files.ui(ui, ctx, session),
                Page::Transfers => self.transfers.ui(ui, ctx, session),
                Page::Encryption => self.encryption.ui(ui, ctx, session),
                Page::Sync => self.sync.ui(ui, ctx, session),
                Page::Links => self.links.ui(ui, ctx, session),
                Page::Settings => {
                    if self.settings.ui(ui, ctx, session) {
                        lock = true;
                    }
                }
            });
        // Files dropped anywhere upload into the current folder.
        let dropped: Vec<std::path::PathBuf> = ui.ctx().input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if !dropped.is_empty() {
            self.page = Page::Files;
            self.files.start_upload(ctx, session, dropped);
        }
        lock
    }
}
