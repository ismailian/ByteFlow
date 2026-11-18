//! The bucket grid (multi-bucket, 0.4.0): shown after unlock, and whenever
//! the user leaves a bucket. One card per connected bucket (name, endpoint,
//! what the local listing knows about it), plus an "Add bucket" card.
//! Opening needs no password (keys were unsealed at unlock); adding, editing
//! and removing reseal the bucket list, so they ask for the login password.

use std::collections::HashMap;

use eframe::egui::{self, Align2, Color32, CornerRadius, RichText, Sense, Stroke, vec2};

use bf_app::auth::AuthController;
use bf_app::buckets::BucketEntry;
use bf_core::AppResult;
use bf_core::failures_ui::message_for;
use bf_core::format;
use bf_data::r2::R2Config;

use crate::bidi;
use crate::ctx::{AppCtx, Task, secret, wipe_string};
use crate::theme::{self, pal};
use crate::ui::icons;
use crate::ui::unlock::brand_mark;
use crate::ui::widgets::{self, Btn, Kind, ModalSpec, Tone};

const CARD_W: f32 = 300.0;
const CARD_H: f32 = 152.0;
const GAP: f32 = 16.0;
const HEADER_H: f32 = 52.0;

#[derive(Clone, Copy, Default)]
struct Stats {
    files: u64,
    bytes: u64,
    last_scan_ms: Option<i64>,
}

#[derive(Default)]
struct Form {
    account_id: String,
    access_key: String,
    secret_key: String,
    bucket: String,
    public_url: String,
    endpoint: String,
    password: String,
}

impl Form {
    fn from(c: &R2Config) -> Self {
        Self {
            account_id: c.account_id.clone(),
            access_key: c.access_key_id.clone(),
            // Never shown; blank keeps the stored secret.
            secret_key: String::new(),
            bucket: c.bucket.clone(),
            public_url: c.public_url.clone().unwrap_or_default(),
            endpoint: c.endpoint.clone().unwrap_or_default(),
            password: String::new(),
        }
    }
    fn config(&self, keep_secret: Option<&str>) -> R2Config {
        let secret = if self.secret_key.is_empty() { keep_secret.unwrap_or("").to_string() } else { self.secret_key.clone() };
        R2Config {
            account_id: self.account_id.clone(),
            access_key_id: self.access_key.clone(),
            secret_access_key: secret,
            bucket: self.bucket.clone(),
            public_url: Some(self.public_url.clone()),
            endpoint: Some(self.endpoint.clone()),
        }
        .normalized()
    }
}

impl Drop for Form {
    fn drop(&mut self) {
        wipe_string(&mut self.secret_key);
        wipe_string(&mut self.password);
    }
}

enum Dialog {
    Add(Form),
    Edit(String, Form),
    Remove(String, String),
}

#[derive(Default)]
pub struct Home {
    stats: HashMap<String, Stats>,
    stats_for: Vec<String>,
    stats_task: Option<Task<Vec<(String, Stats)>>>,
    open_task: Option<Task<Result<(), String>>>,
    dialog: Option<Dialog>,
    test: Option<Task<Result<(), String>>>,
    test_result: Option<Result<(), String>>,
    save: Option<Task<AppResult<()>>>,
}

fn host(c: &R2Config) -> String {
    let ep = c.effective_endpoint();
    ep.trim_start_matches("https://").split('/').next().unwrap_or("").to_string()
}

/// A short, readable location line ("Cloudflare R2 · EU"); the full endpoint
/// is in the card's tooltip.
fn location(c: &R2Config) -> String {
    let h = host(c);
    if !h.ends_with(".r2.cloudflarestorage.com") {
        return h;
    }
    let region = h.trim_end_matches(".r2.cloudflarestorage.com").split('.').nth(1).map(|r| r.to_uppercase());
    match region {
        Some(r) => format!("Cloudflare R2 · {r}"),
        None => "Cloudflare R2".into(),
    }
}

/// A closed rounded-rectangle path (for a dashed outline that matches the
/// cards' corners).
fn rounded_outline(r: egui::Rect, radius: f32) -> Vec<egui::Pos2> {
    use std::f32::consts::{FRAC_PI_2, PI};
    let c = [
        (egui::pos2(r.right() - radius, r.top() + radius), -FRAC_PI_2),
        (egui::pos2(r.right() - radius, r.bottom() - radius), 0.0),
        (egui::pos2(r.left() + radius, r.bottom() - radius), FRAC_PI_2),
        (egui::pos2(r.left() + radius, r.top() + radius), PI),
    ];
    let mut pts = vec![egui::pos2(r.left() + radius, r.top())];
    for (center, start) in c {
        for i in 0..=8 {
            let a = start + FRAC_PI_2 * i as f32 / 8.0;
            pts.push(center + radius * vec2(a.cos(), a.sin()));
        }
    }
    pts.push(pts[0]);
    pts
}

impl Home {
    /// Returns true when "Lock" was pressed.
    pub fn ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx) -> bool {
        let mut lock = false;
        self.poll(ctx);
        let buckets = ctx.auth.buckets();
        let ids: Vec<String> = buckets.iter().map(|b| b.id.clone()).collect();
        if self.stats_for != ids && self.stats_task.is_none() {
            self.stats_for = ids.clone();
            let core = ctx.core.clone();
            self.stats_task = Some(ctx.task(async move {
                let mut out = vec![];
                for id in ids {
                    let c2 = core.clone();
                    let i2 = id.clone();
                    let Ok(Ok(st)) = tokio::task::spawn_blocking(move || c2.bucket_stores(&i2)).await else { continue };
                    let (files, _, bytes) = st.cache_db.totals().await.unwrap_or_default();
                    let last_scan_ms = st.cache_db.meta().await.ok().and_then(|m| m.last_scan_completed_at_ms);
                    out.push((id, Stats { files, bytes, last_scan_ms }));
                }
                out
            }));
        }

        let p = *pal(ui);
        let opening = ctx.auth.opening();
        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.canvas)).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                // One centred column, exactly as wide as the card grid: the
                // header, the grid and the footnote all share its edges.
                let avail = ui.available_width();
                let cols = (((avail - 64.0 + GAP) / (CARD_W + GAP)).floor() as usize).clamp(1, 3);
                let w = cols as f32 * CARD_W + (cols - 1) as f32 * GAP;
                let rows = (buckets.len() + 1).div_ceil(cols) as f32;
                let content_h = HEADER_H + 28.0 + rows * CARD_H + (rows - 1.0) * GAP + 44.0;
                let view_h = ui.available_height();
                let top = ((view_h - content_h) * 0.42).max(36.0);
                let origin = ui.max_rect().min + vec2(((avail - w) / 2.0).max(16.0), top);
                let col_rect = egui::Rect::from_min_size(origin, vec2(w, content_h));
                let mut col = ui.new_child(egui::UiBuilder::new().max_rect(col_rect).layout(egui::Layout::top_down(egui::Align::Min)));
                col.spacing_mut().item_spacing = vec2(GAP, 0.0);

                // Header: brand + title on the left, Lock on the right edge.
                let (hr, _) = col.allocate_exact_size(vec2(w, HEADER_H), Sense::hover());
                let mut head = col.new_child(egui::UiBuilder::new().max_rect(hr).layout(egui::Layout::left_to_right(egui::Align::Center)));
                head.spacing_mut().item_spacing.x = 12.0;
                brand_mark(&mut head, 44.0);
                head.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(RichText::new("Your buckets").font(theme::semibold(24.0)).color(p.text));
                    let n = buckets.len();
                    ui.label(RichText::new(format!("{n} connected · choose one to open")).color(p.text_muted));
                });
                head.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(Btn::new(Kind::Secondary, "Lock").icon(icons::LOCK_SIMPLE)).on_hover_text("Lock the vault").clicked() {
                        lock = true;
                    }
                });
                col.add_space(28.0);

                let mut items: Vec<Option<&BucketEntry>> = buckets.iter().map(Some).collect();
                items.push(None); // the "Add bucket" card
                for (i, row) in items.chunks(cols).enumerate() {
                    if i > 0 {
                        col.add_space(GAP);
                    }
                    col.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = GAP;
                        for item in row {
                            match item {
                                Some(b) => self.card(ui, ctx, b, opening.as_deref(), buckets.len()),
                                None => self.add_card(ui),
                            }
                        }
                    });
                }
                col.add_space(24.0);
                col.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(RichText::new(icons::INFO).size(13.0).color(p.text_subtle));
                    ui.label(RichText::new("Each bucket keeps its own listing, transfers, sync pairs, links and encryption keys on this computer.").size(12.5).color(p.text_subtle));
                });
                // Reserve the column's height so the scroll area knows it.
                ui.allocate_rect(egui::Rect::from_min_size(ui.max_rect().min, vec2(avail, top + content_h + 24.0)), Sense::hover());
            });
        });
        self.dialogs(ui.ctx(), ctx, &buckets);
        lock
    }

    fn poll(&mut self, ctx: &AppCtx) {
        if let Some(v) = self.stats_task.as_ref().and_then(Task::poll) {
            self.stats_task = None;
            self.stats = v.into_iter().collect();
        }
        if let Some(r) = self.open_task.as_ref().and_then(Task::poll) {
            self.open_task = None;
            if let Err(e) = r {
                ctx.toasts.error(e);
            }
        }
        if let Some(r) = self.test.as_ref().and_then(Task::poll) {
            self.test = None;
            self.test_result = Some(r);
        }
        if let Some(r) = self.save.as_ref().and_then(Task::poll) {
            self.save = None;
            match r {
                Ok(()) => {
                    let msg = match &self.dialog {
                        Some(Dialog::Add(_)) => "Bucket added.",
                        Some(Dialog::Edit(..)) => "Credentials updated.",
                        _ => "Bucket removed from this computer.",
                    };
                    ctx.toasts.ok(msg);
                    self.dialog = None;
                    self.test_result = None;
                    self.stats_for.clear();
                }
                Err(e) => ctx.toasts.error(message_for(&e)),
            }
        }
    }

    fn card(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, b: &BucketEntry, opening: Option<&str>, count: usize) {
        let p = *pal(ui);
        let (rect, resp) = ui.allocate_exact_size(vec2(CARD_W, CARD_H), Sense::click());
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(b.config.effective_endpoint());
        let name = b.config.bucket.clone();
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Open bucket {name}")));
        let hovered = resp.hovered() || resp.has_focus();
        let busy = opening == Some(b.id.as_str());
        ui.painter().rect(
            rect,
            CornerRadius::same(14),
            p.surface,
            Stroke::new(if hovered { 1.5 } else { 1.0 }, if hovered || busy { p.primary } else { p.border }),
            egui::StrokeKind::Inside,
        );
        let inner = rect.shrink(18.0);
        let tile = egui::Rect::from_min_size(inner.min, vec2(40.0, 40.0));
        ui.painter().rect_filled(tile, CornerRadius::same(10), p.primary_soft);
        ui.painter().text(tile.center(), Align2::CENTER_CENTER, icons::DATABASE, theme::regular(20.0), p.primary_text);

        // Name + endpoint.
        let text_r = egui::Rect::from_min_max(egui::pos2(tile.right() + 12.0, inner.top()), egui::pos2(inner.right() - 34.0, inner.top() + 42.0));
        let mut col = ui.new_child(egui::UiBuilder::new().max_rect(text_r).layout(egui::Layout::top_down(egui::Align::Min)));
        col.spacing_mut().item_spacing.y = 1.0;
        bidi::name(&mut col, &name, theme::semibold(16.0), p.text);
        col.add(egui::Label::new(RichText::new(location(&b.config)).size(12.5).color(p.text_subtle)).truncate());

        // Menu.
        let menu_r = egui::Rect::from_min_size(egui::pos2(inner.right() - 28.0, inner.top()), vec2(28.0, 28.0));
        let mut m = ui.new_child(egui::UiBuilder::new().max_rect(menu_r));
        let mr = widgets::icon_btn_ex(&mut m, icons::DOTS_THREE, "Bucket options", 28.0, false, opening.is_none());
        let mut menu_used = mr.hovered() || mr.clicked();
        egui::Popup::menu(&mr).show(|ui| {
            ui.set_min_width(190.0);
            if widgets::menu_item(ui, icons::PENCIL_SIMPLE, "Edit credentials…", false).clicked() {
                self.dialog = Some(Dialog::Edit(b.id.clone(), Form::from(&b.config)));
                self.test_result = None;
                ui.close();
            }
            if count > 1 {
                widgets::menu_separator(ui);
                if widgets::menu_item(ui, icons::TRASH, "Remove from this computer…", true).clicked() {
                    self.dialog = Some(Dialog::Remove(b.id.clone(), String::new()));
                    ui.close();
                }
            }
            menu_used = true;
        });

        // Stats from the local listing cache.
        let st = self.stats.get(&b.id).copied();
        let bottom = egui::pos2(inner.left(), inner.bottom());
        let (line1, line2) = match st {
            Some(s) if s.last_scan_ms.is_some() => (
                format!("{} · {}", format::bytes(s.bytes), widgets::items(s.files).replace("item", "file")),
                format!("Listing refreshed {}", format::relative_age(s.last_scan_ms.unwrap_or(0), bf_app::now_ms())),
            ),
            Some(_) => ("Not listed yet".to_string(), "Opens and lists the bucket".to_string()),
            None => (String::new(), String::new()),
        };
        ui.painter().text(bottom - vec2(0.0, 20.0), Align2::LEFT_BOTTOM, line1, theme::semibold(14.0), p.text);
        ui.painter().text(bottom, Align2::LEFT_BOTTOM, line2, theme::regular(12.0), p.text_muted);
        if busy {
            let c = egui::pos2(inner.right() - 10.0, inner.bottom() - 10.0);
            let mut s = ui.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_center_size(c, vec2(20.0, 20.0))));
            s.spinner();
        } else if hovered {
            ui.painter().text(egui::pos2(inner.right(), inner.bottom()), Align2::RIGHT_BOTTOM, format!("Open {}", icons::ARROW_RIGHT), theme::semibold(13.0), p.primary_text);
        }
        if resp.clicked() && !menu_used && opening.is_none() && self.open_task.is_none() {
            let (auth, id) = (ctx.auth.clone(), b.id.clone());
            self.open_task = Some(ctx.task(async move { auth.open_bucket(&id).await }));
        }
    }

    fn add_card(&mut self, ui: &mut egui::Ui) {
        let p = *pal(ui);
        let (rect, resp) = ui.allocate_exact_size(vec2(CARD_W, CARD_H), Sense::click());
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Add bucket"));
        let hovered = resp.hovered() || resp.has_focus();
        if hovered {
            ui.painter().rect_filled(rect, CornerRadius::same(14), p.surface_2);
        }
        // Dashed outline.
        let color = if hovered { p.primary } else { p.border_strong };
        let pts = rounded_outline(rect.shrink(0.75), 14.0);
        ui.painter().extend(egui::Shape::dashed_line(&pts, Stroke::new(1.5, color), 6.0, 5.0));
        let c = rect.center();
        let circle = egui::Rect::from_center_size(c - vec2(0.0, 16.0), vec2(40.0, 40.0));
        ui.painter().circle_filled(circle.center(), 20.0, if hovered { p.primary } else { p.surface_3 });
        ui.painter().text(circle.center(), Align2::CENTER_CENTER, icons::PLUS, theme::regular(20.0), if hovered { Color32::WHITE } else { p.text_muted });
        ui.painter().text(c + vec2(0.0, 22.0), Align2::CENTER_CENTER, "Add bucket", theme::semibold(14.0), if hovered { p.primary_text } else { p.text });
        ui.painter().text(c + vec2(0.0, 42.0), Align2::CENTER_CENTER, "Connect another R2 bucket", theme::regular(12.0), p.text_subtle);
        if resp.clicked() {
            self.dialog = Some(Dialog::Add(Form::default()));
            self.test_result = None;
        }
    }

    fn form(ui: &mut egui::Ui, f: &mut Form, editing: bool) {
        let half = (ui.available_width() - 12.0) / 2.0;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            ui.vertical(|ui| {
                ui.set_width(half);
                widgets::field(ui, "Account ID", |ui| widgets::text_field(ui, &mut f.account_id, "Cloudflare account ID"));
            });
            ui.vertical(|ui| {
                ui.set_width(half);
                widgets::field(ui, "Bucket", |ui| widgets::text_field(ui, &mut f.bucket, "bucket-name"));
            });
        });
        widgets::field(ui, "Access Key ID", |ui| widgets::text_field(ui, &mut f.access_key, "R2 API token access key"));
        let hint = if editing { "Leave blank to keep the current secret" } else { "Never shown again after saving" };
        widgets::field(ui, "Secret Access Key", |ui| widgets::password_field(ui, &mut f.secret_key, hint));
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            ui.vertical(|ui| {
                ui.set_width(half);
                widgets::field(ui, "Public URL (optional)", |ui| widgets::text_field(ui, &mut f.public_url, "https://files.example.com"));
            });
            ui.vertical(|ui| {
                ui.set_width(half);
                widgets::field(ui, "Custom endpoint (optional)", |ui| widgets::text_field(ui, &mut f.endpoint, "https://<id>.eu.r2.cloudflarestorage.com"));
            });
        });
        widgets::divider(ui);
        widgets::field(ui, "Login password", |ui| widgets::password_field(ui, &mut f.password, "Required to save the bucket list"));
    }

    fn dialogs(&mut self, egui_ctx: &egui::Context, ctx: &AppCtx, buckets: &[BucketEntry]) {
        let busy = self.test.is_some() || self.save.is_some();
        let mut close = false;
        let remove_id = match &self.dialog {
            None => return,
            Some(Dialog::Remove(id, _)) => Some(id.clone()),
            _ => None,
        };
        match remove_id {
            None => {
                let edit_id = match &self.dialog {
                    Some(Dialog::Edit(id, _)) => Some(id.clone()),
                    _ => None,
                };
                let current = edit_id.as_ref().and_then(|id| buckets.iter().find(|b| &b.id == id)).map(|b| b.config.clone());
                let (title, sub, icon) = if edit_id.is_some() {
                    ("Edit bucket credentials", "Saved sealed under your vault key", icons::PENCIL_SIMPLE)
                } else {
                    ("Add a bucket", "Connect another Cloudflare R2 bucket", icons::DATABASE)
                };
                let spec = ModalSpec::new("bucket-form", title).subtitle(sub).icon(icon, Tone::Primary).width(560.0);
                let (test_result, test_busy) = (self.test_result.clone(), busy);
                let mut test_req: Option<R2Config> = None;
                let mut save_req: Option<(R2Config, String)> = None;
                let dismissed = widgets::modal(egui_ctx, spec, |ui| {
                    let Some(Dialog::Add(f) | Dialog::Edit(_, f)) = self.dialog.as_mut() else { return };
                    Self::form(ui, f, edit_id.is_some());
                    let cfg = f.config(current.as_ref().map(|c| c.secret_access_key.as_str()));
                    let valid = cfg.validate();
                    match &test_result {
                        Some(Ok(())) => widgets::callout(ui, Tone::Success, icons::CHECK_CIRCLE, "Connection works."),
                        Some(Err(e)) => widgets::callout(ui, Tone::Danger, icons::WARNING_CIRCLE, e),
                        None => {}
                    }
                    if let Err(e) = valid
                        && !f.bucket.is_empty()
                    {
                        widgets::hint(ui, e);
                    }
                    widgets::modal_footer(ui, |ui| {
                        let label = if edit_id.is_some() { "Save" } else { "Add bucket" };
                        if ui.add(Btn::new(Kind::Primary, label).icon(icons::CHECK).enabled(valid.is_ok() && !f.password.is_empty() && !test_busy)).clicked() {
                            save_req = Some((cfg.clone(), f.password.clone()));
                            wipe_string(&mut f.password);
                        }
                        if ui.add(Btn::new(Kind::Secondary, "Test connection").icon(icons::CLOUD_CHECK).enabled(valid.is_ok() && !test_busy)).clicked() {
                            test_req = Some(cfg.clone());
                        }
                        if test_busy {
                            ui.spinner();
                        }
                    });
                });
                if let Some(c) = test_req {
                    self.test_result = None;
                    self.test = Some(ctx.task(async move { AuthController::test_connection(&c).await }));
                }
                if let Some((c, mut pw)) = save_req {
                    let (auth, s) = (ctx.auth.clone(), secret(&pw));
                    wipe_string(&mut pw);
                    self.save = Some(match edit_id {
                        Some(id) => ctx.task(async move { auth.edit_bucket(s, &id, c).await }),
                        None => ctx.task(async move { auth.add_bucket(s, c).await.map(|_| ()) }),
                    });
                }
                close = dismissed && !busy;
            }
            Some(id) => {
                let name = buckets.iter().find(|b| b.id == id).map(|b| b.config.bucket.clone()).unwrap_or_default();
                let title = format!("Remove {name}?");
                let spec = ModalSpec::new("bucket-remove", &title).subtitle("Nothing in the bucket itself is touched").icon(icons::TRASH, Tone::Danger).width(460.0);
                let mut go: Option<(String, String)> = None;
                let dismissed = widgets::modal(egui_ctx, spec, |ui| {
                    let Some(Dialog::Remove(_, pw)) = self.dialog.as_mut() else { return };
                    widgets::callout(
                        ui,
                        Tone::Danger,
                        icons::WARNING,
                        "This computer forgets the bucket: its credentials, listing cache, transfer queue, sync pairs, share-link history, thumbnails and its encryption keys. If its files are encrypted, you will need the passphrase to open them after adding it again.",
                    );
                    ui.add_space(4.0);
                    widgets::field(ui, "Login password", |ui| widgets::password_field(ui, pw, "Required to remove"));
                    widgets::modal_footer(ui, |ui| {
                        if widgets::danger(ui, !pw.is_empty() && !busy, "Remove bucket").clicked() {
                            go = Some((id.clone(), pw.clone()));
                            wipe_string(pw);
                        }
                        if widgets::secondary(ui, "Cancel").clicked() {
                            close = true;
                        }
                        if busy {
                            ui.spinner();
                        }
                    });
                });
                if let Some((id, mut pw)) = go {
                    let (auth, s) = (ctx.auth.clone(), secret(&pw));
                    wipe_string(&mut pw);
                    self.save = Some(ctx.task(async move { auth.remove_bucket(s, &id).await }));
                }
                close |= dismissed && !busy;
            }
        }
        if close {
            self.dialog = None;
            self.test_result = None;
        }
    }
}
