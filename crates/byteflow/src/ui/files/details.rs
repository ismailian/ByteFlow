//! The Files page's right-hand details panel: a preview on top (photos), then
//! name, actions, file information and encryption status. With nothing
//! selected it is the bucket overview (space used, storage by file type,
//! uploads this week, largest folders, recent files) plus the current
//! folder; with several items, the selection.
//!
//! Photo previews: a stored thumbnail shows at once. A plaintext photo (up to
//! [`AUTO_MAX`]) is fetched into memory after a short pause on the selection,
//! which also seeds its thumbnail (§10.3 "image preview"). Encrypted photos
//! are only decrypted when asked ("Load preview"), and never thumbnailed.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, ColorImage, CornerRadius, RichText, Sense, Stroke, TextureHandle, TextureOptions, vec2};

use bf_app::encryption::EncOp;
use bf_app::session::Session;
use bf_core::AppResult;
use bf_core::format;
use bf_core::media::{MediaKind, media_kind_for_key};
use bf_data::local::cache_db::CacheEntry;

use super::FilesPage;
use crate::bidi;
use crate::ctx::{AppCtx, Task};
use crate::theme::{self, pal};
use crate::ui::icons;
use crate::ui::viewer::{decode_image, decrypt_to_memory, stream_to_vec};
use crate::ui::widgets::{self, Btn, Kind, Tone};

/// Largest plaintext photo fetched automatically for the panel preview.
const AUTO_MAX: u64 = 24 * 1024 * 1024;
const MANUAL_MAX: u64 = 96 * 1024 * 1024;
const PREVIEW_EDGE: u32 = 720;
const DEBOUNCE: Duration = Duration::from_millis(350);
const CACHE: usize = 8;

#[derive(Default)]
pub(super) struct DetailsPane {
    key: Option<String>,
    since: Option<Instant>,
    task: Option<Task<AppResult<ColorImage>>>,
    failed: bool,
    cache: HashMap<String, TextureHandle>,
    order: VecDeque<String>,
    bytes: Arc<AtomicU64>,
}

async fn load_preview(session: Arc<Session>, key: String, encrypted: bool, bytes: Arc<AtomicU64>) -> AppResult<ColorImage> {
    let data = if encrypted {
        decrypt_to_memory(&session, &key, MANUAL_MAX, &bytes).await?.0
    } else {
        stream_to_vec(&session, &key, AUTO_MAX, &bytes).await?
    };
    let (ci, img) = bf_app::blocking(move || decode_image(&data, PREVIEW_EDGE)).await?;
    if !encrypted {
        // The decoded frame is already in hand: reuse it for the thumbnail.
        session.thumbs.generate_from_image(&key, img);
    }
    Ok(ci)
}

fn exact_bytes(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl DetailsPane {
    fn select(&mut self, key: &str) {
        if self.key.as_deref() != Some(key) {
            self.key = Some(key.to_string());
            self.since = Some(Instant::now());
            self.task = None;
            self.failed = false;
        }
    }

    fn remember(&mut self, key: String, tex: TextureHandle) {
        self.cache.insert(key.clone(), tex);
        self.order.retain(|k| *k != key);
        self.order.push_back(key);
        while self.order.len() > CACHE {
            if let Some(old) = self.order.pop_front() {
                self.cache.remove(&old);
            }
        }
    }

    fn start(&mut self, ctx: &AppCtx, session: &Arc<Session>, key: &str, encrypted: bool) {
        self.failed = false;
        self.task = Some(ctx.task(load_preview(session.clone(), key.to_string(), encrypted, self.bytes.clone())));
    }
}

impl FilesPage {
    pub fn details_ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, session: &Arc<Session>) {
        let p = *pal(ui);
        self.thumbs.pump(ui.ctx());
        // Finished preview loads.
        if let Some(r) = self.details.task.as_ref().and_then(Task::poll) {
            self.details.task = None;
            match (r, self.details.key.clone()) {
                (Ok(img), Some(k)) => {
                    let t = ui.ctx().load_texture(format!("details:{k}"), img, TextureOptions::LINEAR);
                    self.details.remember(k, t);
                }
                _ => self.details.failed = true,
            }
        }

        let title = if self.selection.is_empty() { "Overview" } else { "Details" };
        ui.horizontal(|ui| {
            ui.label(RichText::new(title).font(theme::semibold(15.0)).color(p.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::icon_btn_ex(ui, icons::X, "Hide details", 28.0, false, true).clicked() {
                    Self::set_details_open(ctx, false);
                }
            });
        });
        ui.add_space(10.0);

        let data = session.browser.data();
        let selected: Vec<String> = self.selection.iter().cloned().collect();
        let find = |k: &str| data.folders.iter().chain(data.files.iter()).find(|e| e.key == k).cloned();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            match selected.as_slice() {
                [] => self.overview(ui, ctx, session, &data),
                [k] => match find(k) {
                    Some(e) => self.single(ui, ctx, session, &e),
                    None => self.overview(ui, ctx, session, &data),
                },
                many => {
                    let entries: Vec<CacheEntry> = many.iter().filter_map(|k| find(k)).collect();
                    self.multi(ui, ctx, session, &entries);
                }
            }
        });
    }

    fn preview_box(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, session: &Arc<Session>, e: &CacheEntry) {
        let p = *pal(ui);
        let w = ui.available_width();
        let h = (w * 0.72).clamp(150.0, 260.0);
        let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
        ui.painter().rect_filled(rect, CornerRadius::same(12), p.surface_2);
        let inner = rect.shrink(10.0);
        let encrypted = session.encryption.is_encrypted(&e.key);
        let locked = session.encryption.is_locked(&e.key);
        let is_image = !e.is_folder && media_kind_for_key(&e.key) == MediaKind::Image;

        let paint_fit = |ui: &egui::Ui, t: &TextureHandle| {
            let ts = t.size_vec2();
            let s = (inner.width() / ts.x).min(inner.height() / ts.y).min(2.0);
            let r = egui::Rect::from_center_size(inner.center(), ts * s);
            egui::Image::new(t).corner_radius(8).paint_at(ui, r);
        };
        let paint_tile = |ui: &egui::Ui| {
            let s = (h * 0.38).min(76.0);
            let tr = egui::Rect::from_center_size(rect.center(), vec2(s, s));
            widgets::paint_file_tile(ui, tr, &e.key, e.is_folder);
            if encrypted {
                let c = tr.right_bottom() - vec2(4.0, 4.0);
                ui.painter().circle(c, 13.0, p.success, Stroke::new(2.5, p.surface_2));
                ui.painter().text(c, Align2::CENTER_CENTER, icons::LOCK_SIMPLE, theme::regular(13.0), Color32::WHITE);
            }
        };

        if !is_image || locked {
            paint_tile(ui);
            return;
        }
        self.details.select(&e.key);
        if let Some(t) = self.details.cache.get(&e.key).cloned() {
            paint_fit(ui, &t);
            return;
        }
        let thumb = self.thumbs.get(ctx, session, &e.key);
        match &thumb {
            Some(t) => paint_fit(ui, t),
            None => paint_tile(ui),
        }
        if self.details.failed {
            // The thumbnail (if any) stays; only say so when there's nothing to show.
            if thumb.is_some() {
                return;
            }
            let r = egui::Rect::from_min_size(egui::pos2(rect.left() + 10.0, rect.bottom() - 34.0), vec2(rect.width() - 20.0, 24.0));
            ui.painter().text(r.center(), Align2::CENTER_CENTER, "Preview unavailable", theme::regular(12.0), p.text_subtle);
            return;
        }
        if self.details.task.is_some() {
            let c = egui::Rect::from_center_size(rect.right_top() + vec2(-22.0, 22.0), vec2(18.0, 18.0));
            ui.put(c, egui::Spinner::new().size(16.0));
            return;
        }
        if encrypted {
            // Decrypting writes a short-lived plaintext temp: only on request.
            let br = egui::Rect::from_center_size(egui::pos2(rect.center().x, rect.bottom() - 30.0), vec2(140.0, 30.0));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(br).layout(egui::Layout::top_down(egui::Align::Center)));
            if child.add(Btn::new(Kind::Secondary, "Load preview").icon(icons::LOCK_OPEN).small()).on_hover_text("Decrypt this photo in memory to preview it").clicked() {
                self.details.start(ctx, session, &e.key, true);
            }
        } else if e.size <= AUTO_MAX {
            let waited = self.details.since.map_or(Duration::MAX, |s| s.elapsed());
            if waited >= DEBOUNCE {
                self.details.start(ctx, session, &e.key, false);
            } else {
                ui.ctx().request_repaint_after(DEBOUNCE - waited);
            }
        }
    }

    fn single(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, session: &Arc<Session>, e: &CacheEntry) {
        let p = *pal(ui);
        let data = session.browser.data();
        let locked = session.encryption.is_locked(&e.key);
        self.preview_box(ui, ctx, session, e);
        ui.add_space(8.0);
        bidi::wrapped(ui, bf_core::media::leaf_name(&e.key), theme::semibold(16.0), p.text);
        ui.add_space(-4.0);
        ui.label(RichText::new(widgets::kind_label(&e.key, e.is_folder)).size(12.5).color(p.text_muted));
        ui.add_space(6.0);

        // Actions.
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if ui.add(Btn::new(Kind::Primary, "Open").icon(if e.is_folder { icons::FOLDER_OPEN } else { icons::EYE }).small().enabled(!locked)).clicked() {
                self.activate(ctx, session, e, locked);
            }
            ui.add_space(4.0);
            if widgets::icon_btn_ex(ui, icons::DOWNLOAD, "Download", 30.0, false, true).clicked() {
                self.download(ctx, session, vec![e.key.clone()]);
            }
            if !e.is_folder && widgets::icon_btn_ex(ui, icons::LINK_SIMPLE, "Share link", 30.0, false, true).clicked() {
                self.share(ctx, session, vec![e.key.clone()]);
            }
            if widgets::icon_btn_ex(ui, icons::PENCIL_SIMPLE, "Rename (F2)", 30.0, false, !locked).clicked() {
                self.open_rename(&e.key);
            }
            let more = ui.add_enabled_ui(!locked, |ui| {
                let r = widgets::icon_btn_ex(ui, icons::DOTS_THREE, "More actions", 30.0, false, !locked);
                egui::Popup::menu(&r).show(|ui| {
                    ui.set_min_width(200.0);
                    if !e.is_folder {
                        let enc = session.encryption.is_encrypted(&e.key);
                        if !enc && widgets::menu_item(ui, icons::LOCK_SIMPLE, "Encrypt", false).clicked() {
                            self.encrypt_op(ctx, session, vec![e.key.clone()], EncOp::Encrypt);
                            ui.close();
                        }
                        if enc && !session.encryption.is_external(&e.key) && widgets::menu_item(ui, icons::LOCK_OPEN, "Decrypt", false).clicked() {
                            self.encrypt_op(ctx, session, vec![e.key.clone()], EncOp::Decrypt);
                            ui.close();
                        }
                    }
                    if widgets::menu_item(ui, icons::FOLDER_OPEN, "Move to…", false).clicked() {
                        self.open_move(vec![e.key.clone()]);
                        ui.close();
                    }
                    widgets::menu_separator(ui);
                    if widgets::menu_item(ui, icons::TRASH, "Delete", true).clicked() {
                        self.open_delete(vec![e.key.clone()]);
                        ui.close();
                    }
                });
            });
            let _ = more;
        });
        ui.add_space(10.0);
        widgets::divider(ui);

        // Information.
        widgets::eyebrow(ui, "INFORMATION");
        ui.add_space(2.0);
        if e.is_folder {
            let u = data.counts.get(&e.key);
            widgets::kv(ui, "Items", RichText::new(u.map(|u| format!("{} files", exact_bytes(u.files))).unwrap_or_else(|| "—".into())).color(p.text));
            widgets::kv(ui, "Size", RichText::new(u.map(|u| format::bytes(u.bytes)).unwrap_or_else(|| "—".into())).color(p.text));
        } else {
            widgets::kv(ui, "Size", RichText::new(format::bytes(e.size)).color(p.text)).on_hover_text(format!("{} bytes", exact_bytes(e.size)));
            let when = e.last_modified_ms.map(format::local_time).unwrap_or_else(|| "Unknown".into());
            widgets::kv(ui, "Uploaded", RichText::new(when).color(p.text));
            if let Some(t) = e.last_modified_ms {
                ui.add_space(-6.0);
                ui.horizontal(|ui| {
                    ui.add_space(100.0);
                    ui.label(RichText::new(format::relative_age(t, bf_app::now_ms())).size(12.0).color(p.text_subtle));
                });
            }
            widgets::kv(ui, "Type", RichText::new(bf_core::media::content_type_for(&e.key)).color(p.text));
        }
        let parent = bf_core::media::parent_prefix(&e.key);
        widgets::kv(ui, "Location", RichText::new(bidi::path(&format!("/{parent}"))).color(p.text));
        if let Some(etag) = e.etag.as_deref().filter(|s| !s.is_empty()) {
            let short: String = etag.trim_matches('"').chars().take(18).collect();
            widgets::kv(ui, "ETag", RichText::new(short).font(theme::mono(12.0)).color(p.text_muted)).on_hover_text(etag.trim_matches('"'));
        }

        // Encryption.
        if !e.is_folder {
            ui.add_space(8.0);
            widgets::divider(ui);
            widgets::eyebrow(ui, "ENCRYPTION");
            ui.add_space(4.0);
            let enc = &session.encryption;
            let (icon, tone, title, body) = if locked {
                (icons::HOURGLASS, Tone::Violet, "Encryption task running".to_string(), "This file is being encrypted or decrypted. Most actions wait until it finishes.".to_string())
            } else if enc.is_external(&e.key) {
                (icons::KEY, Tone::Warn, "Encrypted elsewhere".to_string(), "Encrypted by the CLI or another device. Opening it needs that passphrase.".to_string())
            } else if enc.is_encrypted(&e.key) {
                let epoch = enc.epoch_of(&e.key).map(|n| format!(" Key epoch {n}.")).unwrap_or_default();
                (icons::SHIELD_CHECK, Tone::Success, "Encrypted".to_string(), format!("AES-256-GCM before upload: the bucket only holds ciphertext.{epoch}"))
            } else {
                let tip = if enc.has_key() { " You can encrypt it from More actions." } else { " Set a key on the Encryption page to encrypt files." };
                (icons::SHIELD, Tone::Neutral, "Not encrypted".to_string(), format!("Stored as plaintext in the bucket.{tip}"))
            };
            ui.horizontal_top(|ui| {
                widgets::icon_tile(ui, icon, tone, 34.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new(title).font(theme::semibold(13.5)).color(p.text));
                    ui.add_space(-4.0);
                    ui.add(egui::Label::new(RichText::new(body).size(12.5).color(p.text_muted)).wrap());
                });
            });
        }
    }

    fn multi(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, session: &Arc<Session>, entries: &[CacheEntry]) {
        let p = *pal(ui);
        let w = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(vec2(w, 150.0), Sense::hover());
        ui.painter().rect_filled(rect, CornerRadius::same(12), p.surface_2);
        // A small fan of the first three tiles.
        for (i, e) in entries.iter().take(3).enumerate().rev() {
            let off = (i as f32 - 1.0) * 26.0;
            let r = egui::Rect::from_center_size(rect.center() + vec2(off, 0.0), vec2(60.0, 60.0));
            ui.painter().rect(r.expand(3.0), CornerRadius::same(16), p.surface_2, Stroke::NONE, egui::StrokeKind::Outside);
            widgets::paint_file_tile(ui, r, &e.key, e.is_folder);
        }
        ui.add_space(8.0);
        let n = self.selection.len();
        ui.label(RichText::new(format!("{n} items selected")).font(theme::semibold(16.0)).color(p.text));
        let files: Vec<&CacheEntry> = entries.iter().filter(|e| !e.is_folder).collect();
        let folders = entries.len() - files.len();
        let bytes: u64 = files.iter().map(|e| e.size).sum();
        let enc = files.iter().filter(|e| session.encryption.is_encrypted(&e.key)).count();
        ui.add_space(4.0);
        widgets::divider(ui);
        widgets::eyebrow(ui, "SELECTION");
        widgets::kv(ui, "Files", RichText::new(files.len().to_string()).color(p.text));
        if folders > 0 {
            widgets::kv(ui, "Folders", RichText::new(folders.to_string()).color(p.text));
        }
        widgets::kv(ui, "Size", RichText::new(if folders > 0 { format!("{} + folders", format::bytes(bytes)) } else { format::bytes(bytes) }).color(p.text));
        widgets::kv(ui, "Encrypted", RichText::new(format!("{enc} of {}", files.len())).color(p.text));
        ui.add_space(10.0);
        let keys: Vec<String> = self.selection.iter().cloned().collect();
        let full = ui.available_width();
        if ui.add(Btn::new(Kind::Primary, "Download").icon(icons::DOWNLOAD).min_width(full)).clicked() {
            self.download(ctx, session, keys.clone());
        }
        ui.add_space(2.0);
        if !files.is_empty() && ui.add(Btn::new(Kind::Secondary, "Share links").icon(icons::LINK_SIMPLE).min_width(full)).clicked() {
            self.share(ctx, session, keys.clone());
        }
        ui.add_space(2.0);
        if ui.add(Btn::new(Kind::Ghost, "Clear selection").icon(icons::X).min_width(full)).clicked() {
            self.selection.clear();
        }
    }

    /// Nothing selected: the bucket at a glance (space used, the split by file
    /// type, uploads this week, the largest folders, the latest files), then
    /// the current folder when it isn't the root. Everything comes from the
    /// local listing cache.
    fn overview(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, session: &Arc<Session>, data: &bf_app::browser::BrowserData) {
        let p = *pal(ui);
        let o = session.overview.data();
        let bucket = session.config().bucket.clone();
        let now = bf_app::now_ms();

        ui.horizontal(|ui| {
            widgets::icon_tile(ui, icons::DATABASE, Tone::Primary, 38.0);
            ui.add_space(2.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                bidi::name(ui, &bucket, theme::semibold(15.0), p.text);
                let when = o.last_scan_ms.map(|t| format!("refreshed {}", format::relative_age(t, now))).unwrap_or_else(|| "not refreshed yet".into());
                ui.label(RichText::new(format!("Cloudflare R2 · {when}")).size(12.0).color(p.text_subtle));
            });
        });
        ui.add_space(12.0);
        if !o.has_scanned {
            widgets::callout(ui, Tone::Primary, icons::INFO, "Refresh once to build the local listing. The statistics are worked out on this computer after that.");
            return;
        }

        // Space used.
        ui.label(RichText::new(format::bytes(o.bytes)).font(theme::semibold(30.0)).color(p.text));
        ui.add_space(-6.0);
        ui.label(RichText::new(format!("used by {} and {}", count(o.files, "file"), count(o.folders, "folder"))).size(12.5).color(p.text_muted));
        ui.add_space(12.0);

        // Storage by type: a thin segmented bar, then one row per part.
        widgets::eyebrow(ui, "STORAGE BY TYPE");
        ui.add_space(2.0);
        let colors = theme::series(&p);
        let parts: Vec<(Color32, &str, u64, u64)> =
            o.composition.iter().enumerate().filter(|(_, (_, n, _))| *n > 0).map(|(i, (c, n, b))| (colors[i % colors.len()], c.label(), *n, *b)).collect();
        if parts.is_empty() {
            widgets::hint(ui, "No files in the bucket yet.");
        } else {
            let share = |b: u64| if o.bytes == 0 { 0.0 } else { b as f64 * 100.0 / o.bytes as f64 };
            let tip = |name: &str, n: u64, b: u64| format!("{name}: {} ({:.1}%) in {}", format::bytes(b), share(b), count(n, "file"));
            let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 8.0), Sense::hover());
            let segs = segment_widths(&parts.iter().map(|x| x.3).collect::<Vec<_>>(), bar.width(), 2.0, 3.0);
            let mut x = bar.left();
            let drawn: Vec<usize> = (0..parts.len()).filter(|i| segs[*i] > 0.0).collect();
            for (k, &i) in drawn.iter().enumerate() {
                let (color, name, n, b) = parts[i];
                let r = egui::Rect::from_min_size(egui::pos2(x, bar.top()), vec2(segs[i], bar.height()));
                let (first, last) = (k == 0, k + 1 == drawn.len());
                let cr = CornerRadius { nw: if first { 4 } else { 1 }, sw: if first { 4 } else { 1 }, ne: if last { 4 } else { 1 }, se: if last { 4 } else { 1 } };
                ui.painter().rect_filled(r, cr, color);
                ui.interact(r.expand2(vec2(0.0, 4.0)), ui.scope_id().with(("seg", i)), Sense::hover()).on_hover_text(tip(name, n, b));
                x += segs[i] + 2.0;
            }
            ui.add_space(6.0);
            for &(color, name, n, b) in &parts {
                let (row, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::hover());
                let sw = egui::Rect::from_center_size(egui::pos2(row.left() + 5.0, row.center().y), vec2(10.0, 10.0));
                ui.painter().rect_filled(sw, CornerRadius::same(3), color);
                let g = ui.painter().layout_no_wrap(name.to_owned(), theme::regular(13.5), p.text);
                let gw = g.size().x;
                ui.painter().galley(egui::pos2(row.left() + 18.0, row.center().y - g.size().y / 2.0), g, p.text);
                ui.painter().text(egui::pos2(row.left() + 26.0 + gw, row.center().y), Align2::LEFT_CENTER, count(n, "file"), theme::regular(12.0), p.text_subtle);
                ui.painter().text(egui::pos2(row.right(), row.center().y), Align2::RIGHT_CENTER, format::bytes(b), theme::medium(13.0), p.text);
                resp.on_hover_text(tip(name, n, b));
            }
        }

        // Uploads over the last seven days: one series, so no legend.
        ui.add_space(4.0);
        widgets::divider(ui);
        let week: u64 = o.histogram.iter().map(|(_, n)| *n).sum();
        ui.horizontal(|ui| {
            widgets::eyebrow(ui, "UPLOADS, LAST 7 DAYS");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(count(week, "file")).size(12.0).color(p.text_muted));
            });
        });
        ui.add_space(4.0);
        self.uploads_chart(ui, &o.histogram, colors[0]);

        // Largest folders at the root.
        if !o.root_folders.is_empty() {
            ui.add_space(4.0);
            widgets::divider(ui);
            widgets::eyebrow(ui, "LARGEST FOLDERS");
            ui.add_space(2.0);
            for (f, u) in o.root_folders.iter().take(5) {
                let resp = link_row(ui, f, true, bf_core::media::leaf_name(f), &format::bytes(u.bytes));
                if resp.on_hover_text(format!("{} in {}", format::bytes(u.bytes), count(u.files, "file"))).clicked() {
                    self.open_folder(ctx, session, f);
                }
            }
        }

        // The latest uploads: opening one shows it in its folder, selected.
        if !o.recent.is_empty() {
            ui.add_space(4.0);
            widgets::divider(ui);
            widgets::eyebrow(ui, "RECENT FILES");
            ui.add_space(2.0);
            for e in o.recent.iter().take(5) {
                let age = e.last_modified_ms.map(|t| format::relative_age(t, now)).unwrap_or_default();
                if link_row(ui, &e.key, false, bf_core::media::leaf_name(&e.key), &age).on_hover_text(bidi::path(&format!("/{}", e.key))).clicked() {
                    let parent = bf_core::media::parent_prefix(&e.key).to_string();
                    self.open_folder(ctx, session, &parent);
                    self.selection.insert(e.key.clone());
                }
            }
        }

        // The folder on screen, below the root.
        if !data.prefix.is_empty() {
            ui.add_space(4.0);
            widgets::divider(ui);
            widgets::eyebrow(ui, "THIS FOLDER");
            ui.add_space(2.0);
            let bytes: u64 = data.files.iter().map(|e| e.size).sum();
            let sub: u64 = data.folders.iter().filter_map(|f| data.counts.get(&f.key)).map(|u| u.bytes).sum();
            widgets::kv(ui, "Name", RichText::new(bidi::visual(bf_core::media::leaf_name(&data.prefix))).color(p.text));
            widgets::kv(ui, "Location", RichText::new(bidi::path(&format!("/{}", data.prefix))).color(p.text));
            widgets::kv(ui, "Contents", RichText::new(format!("{} · {}", count(data.folders.len() as u64, "folder"), count(data.files.len() as u64, "file"))).color(p.text));
            widgets::kv(ui, "Size", RichText::new(format::bytes(bytes + sub)).color(p.text)).on_hover_text("Files here plus everything in subfolders");
        }
        ui.add_space(12.0);
        widgets::hint(ui, "Select a file to see its details, encryption status and a preview.");
    }

    /// Column chart of uploads per day: bars on one baseline, 4 px rounded
    /// tops, hairline gridlines, day names under the bars, a count on the
    /// busiest day only, and every day's count on hover.
    fn uploads_chart(&self, ui: &mut egui::Ui, days: &[(String, u64)], color: Color32) {
        let p = *pal(ui);
        let h = 64.0;
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), h + 30.0), Sense::hover());
        let plot = egui::Rect::from_min_max(rect.min + vec2(0.0, 14.0), egui::pos2(rect.right(), rect.top() + 14.0 + h));
        let max = days.iter().map(|(_, n)| *n).max().unwrap_or(0);
        let grid = egui::Stroke::new(1.0, p.border.gamma_multiply(0.7));
        ui.painter().hline(plot.x_range(), plot.top(), grid);
        ui.painter().hline(plot.x_range(), plot.center().y, grid);
        ui.painter().hline(plot.x_range(), plot.bottom(), egui::Stroke::new(1.0, p.border_strong));
        if max == 0 {
            ui.painter().text(plot.center(), Align2::CENTER_CENTER, "No uploads this week", theme::regular(12.0), p.text_subtle);
        }
        let slot = plot.width() / days.len().max(1) as f32;
        let bar_w = (slot * 0.56).min(24.0);
        let busiest = days.iter().position(|(_, n)| *n == max && max > 0);
        for (i, (day, n)) in days.iter().enumerate() {
            let cx = plot.left() + slot * (i as f32 + 0.5);
            if *n > 0 {
                let bh = (h * (*n as f32 / max as f32)).max(3.0);
                let r = egui::Rect::from_min_max(egui::pos2(cx - bar_w / 2.0, plot.bottom() - bh), egui::pos2(cx + bar_w / 2.0, plot.bottom()));
                ui.painter().rect_filled(r, CornerRadius { nw: 4, ne: 4, sw: 0, se: 0 }, color);
                if busiest == Some(i) {
                    ui.painter().text(egui::pos2(cx, r.top() - 8.0), Align2::CENTER_CENTER, n.to_string(), theme::medium(11.0), p.text_muted);
                }
            }
            ui.painter().text(egui::pos2(cx, plot.bottom() + 10.0), Align2::CENTER_CENTER, day, theme::regular(11.0), p.text_subtle);
            let hit = egui::Rect::from_min_max(egui::pos2(cx - slot / 2.0, rect.top()), egui::pos2(cx + slot / 2.0, rect.bottom()));
            let resp = ui.interact(hit, ui.scope_id().with(("upload-day", i)), Sense::hover());
            if resp.hovered() {
                ui.painter().rect_filled(egui::Rect::from_min_max(egui::pos2(hit.left() + 2.0, plot.top()), egui::pos2(hit.right() - 2.0, plot.bottom())), CornerRadius::same(4), p.surface_2.gamma_multiply(0.6));
            }
            resp.on_hover_text(format!("{day}: {}", count(*n, "upload")));
        }
    }
}

/// "1 file", "1,204 files".
fn count(n: u64, what: &str) -> String {
    format!("{} {what}{}", exact_bytes(n), if n == 1 { "" } else { "s" })
}

/// Widths for a segmented bar: proportional to `values`, `gap` apart, zero
/// values drawing nothing, and every other part at least `min` wide (taken
/// from the widest part) so a small share stays visible.
fn segment_widths(values: &[u64], width: f32, gap: f32, min: f32) -> Vec<f32> {
    let shown = values.iter().filter(|v| **v > 0).count();
    let total: u64 = values.iter().sum();
    if shown == 0 || total == 0 {
        return vec![0.0; values.len()];
    }
    let room = (width - gap * (shown as f32 - 1.0)).max(0.0);
    let mut w: Vec<f32> = values.iter().map(|v| if *v == 0 { 0.0 } else { room * (*v as f32 / total as f32) }).collect();
    let short: f32 = w.iter().filter(|x| **x > 0.0 && **x < min).map(|x| min - x).sum();
    if short > 0.0 {
        for x in w.iter_mut().filter(|x| **x > 0.0 && **x < min) {
            *x = min;
        }
        if let Some(widest) = w.iter_mut().max_by(|a, b| a.total_cmp(b)) {
            *widest = (*widest - short).max(min);
        }
    }
    w
}

/// A clickable list row: type tile, name (truncated, right-to-left aware)
/// and a figure on the right.
fn link_row(ui: &mut egui::Ui, key: &str, is_folder: bool, name: &str, figure: &str) -> egui::Response {
    let p = *pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(8), p.surface_2);
    }
    widgets::paint_file_tile(ui, egui::Rect::from_center_size(egui::pos2(rect.left() + 16.0, rect.center().y), vec2(24.0, 24.0)), key, is_folder);
    let fig = ui.painter().layout_no_wrap(figure.to_owned(), theme::regular(12.5), p.text_muted);
    let fig_w = fig.size().x;
    ui.painter().galley(egui::pos2(rect.right() - 6.0 - fig_w, rect.center().y - fig.size().y / 2.0), fig, p.text_muted);
    let name_r = egui::Rect::from_min_max(egui::pos2(rect.left() + 36.0, rect.top()), egui::pos2(rect.right() - 14.0 - fig_w, rect.bottom()));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(name_r).layout(egui::Layout::left_to_right(egui::Align::Center)));
    bidi::name(&mut child, name, theme::regular(13.5), p.text);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_fill_the_bar_and_keep_small_parts_visible() {
        let w = segment_widths(&[900, 0, 98, 2], 200.0, 2.0, 3.0);
        assert_eq!(w[1], 0.0, "a zero part draws nothing");
        assert!(w[3] >= 3.0, "a tiny part stays visible");
        let total: f32 = w.iter().sum::<f32>() + 2.0 * 2.0;
        assert!((total - 200.0).abs() < 0.01, "{total}");
        assert_eq!(segment_widths(&[0, 0], 100.0, 2.0, 3.0), vec![0.0, 0.0]);
    }

    #[test]
    fn counts_read_naturally() {
        assert_eq!(count(1, "file"), "1 file");
        assert_eq!(count(1204, "file"), "1,204 files");
    }
}
