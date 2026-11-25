//! The file browser (spec §6): cache-primary listing, search, sort, list/grid,
//! multi-select, single and bulk operations, the upload flow with overwrite
//! detection and auto-delete review, foreground encrypted downloads, the
//! built-in viewers and the details panel.

mod audio;
mod details;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use eframe::egui::{self, Align2, Color32, CornerRadius, RichText, Sense, Stroke, vec2};
use tokio_util::sync::CancellationToken;

use bf_app::browser::BrowserSort;
use bf_app::encryption::EncOp;
use bf_app::fileops::{BulkResult, TYPED_DELETE_THRESHOLD, validate_name};
use bf_app::session::Session;
use bf_app::sharing::IssuedLink;
use bf_app::upload::{UploadReport, UploadRequest};
use bf_core::failures_ui::message_for;
use bf_core::format;
use bf_core::{AppFailure, AppResult};
use bf_data::local::cache_db::{CacheEntry, Usage};

use crate::bidi;
use crate::ctx::{AppCtx, Task, secret, wipe_string};
use crate::platform;
use crate::theme::{self, pal};
use crate::ui::icons;
use crate::ui::thumbs::ThumbCache;
use crate::ui::viewer::{Viewer, ViewerAction};
use crate::ui::widgets::{self, Btn, Kind, ModalSpec, Tone};

const ROW_H: f32 = 46.0;
/// The context row (breadcrumbs, or the selection's actions) never changes height.
const CONTEXT_ROW_H: f32 = 40.0;

// ------------------------------------------------------------------ jobs --

/// A foreground operation with progress (bulk ops, encrypted downloads).
struct Job {
    title: String,
    done: Arc<AtomicUsize>,
    total: Arc<AtomicUsize>,
    bytes: Arc<AtomicU64>,
    cancel: CancellationToken,
    task: Task<JobDone>,
}

enum JobDone {
    Bulk(String, AppResult<BulkResult>),
    Message(Result<String, String>),
    Downloads { done: usize, failed: Vec<(String, String)>, remaining: Vec<(String, PathBuf)>, needs_passphrase: bool },
    Share(AppResult<Vec<IssuedLink>>),
}

// ---------------------------------------------------------------- modals --

enum Modal {
    NewFolder { name: String, err: Option<String>, task: Option<Task<AppResult<String>>> },
    Rename { key: String, is_folder: bool, name: String, err: Option<String> },
    Move { keys: Vec<String>, browse: String, folders: Option<Task<AppResult<Vec<String>>>>, list: Vec<String> },
    Delete { keys: Vec<String>, count: Option<Task<AppResult<usize>>>, n: Option<usize>, typed: String },
    Links { links: Vec<IssuedLink>, ttl: u64 },
    Report { title: String, lines: Vec<String> },
    Passphrase { text: String, retry: Vec<(String, PathBuf)> },
}

// ---------------------------------------------------------------- upload --

struct UploadItem {
    path: PathBuf,
    size: u64,
    key: String,
    delete_source: bool,
    exists: Option<bool>,
    skip: bool,
}

enum UploadStage {
    Review,
    Checking(Task<Vec<bool>>),
    Conflict { idx: usize, rename: String, checking: Option<Task<bool>>, err: Option<String> },
    /// Every conflict resolved; submitted on this frame.
    Ready,
    Submitting(Task<UploadReport>),
}

struct UploadFlow {
    items: Vec<UploadItem>,
    prefix: String,
    stage: UploadStage,
}

#[derive(Default)]
pub struct FilesPage {
    search: String,
    selection: BTreeSet<String>,
    anchor: Option<usize>,
    modal: Option<Modal>,
    job: Option<Job>,
    viewer: Option<Viewer>,
    thumbs: ThumbCache,
    upload: Option<UploadFlow>,
    pick: Option<Task<Option<Vec<PathBuf>>>>,
    scan: Option<Task<AppResult<()>>>,
    details: details::DetailsPane,
    audio: audio::AudioDock,
    #[cfg(feature = "dev-env")]
    dev_viewed: bool,
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if !p.exists() && !part_exists(&p) {
        return p;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 1..10_000 {
        let c = dir.join(format!("{stem} ({n}){ext}"));
        if !c.exists() && !part_exists(&c) {
            return c;
        }
    }
    p
}

fn part_exists(p: &Path) -> bool {
    let mut s = p.as_os_str().to_os_string();
    s.push(".part");
    PathBuf::from(s).exists()
}

/// Local-safe relative path from an R2 key segment list.
fn local_rel(rel: &str) -> PathBuf {
    rel.split('/')
        .filter(|s| !s.is_empty() && *s != "." && *s != "..")
        .map(|s| s.chars().map(|c| if matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { '_' } else { c }).collect::<String>())
        .collect()
}

fn folder_label(prefix: &str) -> String {
    if prefix.is_empty() { "the bucket root".into() } else { format!("/{}", bidi::path_marks(prefix)) }
}

/// Row/tile events collected while drawing, applied afterwards.
enum Hit {
    Click(usize, egui::Modifiers),
    Toggle(usize),
    Open(usize),
}

impl FilesPage {
    pub fn cleanup_temps(&mut self) {
        if let Some(v) = self.viewer.as_mut() {
            v.cleanup();
        }
    }

    /// Stop sidebar audio (panel hidden, or another page shown).
    pub fn stop_audio(&mut self) {
        if self.audio.is_active() {
            self.audio.stop();
        }
    }

    pub fn details_open(&self, ctx: &AppCtx) -> bool {
        ctx.prefs().files_details_open
    }

    fn set_details_open(ctx: &AppCtx, open: bool) {
        let _ = ctx.core.prefs.update(|p| p.files_details_open = open);
    }

    // --------------------------------------------------------- actions --

    fn refresh(&mut self, ctx: &AppCtx, session: &Arc<Session>) {
        if self.scan.is_some() {
            return;
        }
        let s = session.clone();
        self.scan = Some(ctx.task(async move { s.cache.full_scan().await }));
    }

    fn open_folder(&mut self, ctx: &AppCtx, session: &Arc<Session>, prefix: &str) {
        self.selection.clear();
        self.anchor = None;
        self.search.clear();
        let (b, p) = (session.browser.clone(), prefix.to_string());
        ctx.spawn(async move { b.open_folder(&p).await });
    }

    fn reload(&self, ctx: &AppCtx, session: &Arc<Session>) {
        let (b, o) = (session.browser.clone(), session.overview.clone());
        ctx.spawn(async move {
            b.reload().await;
            o.reload().await;
        });
    }

    pub fn start_upload(&mut self, ctx: &AppCtx, session: &Arc<Session>, paths: Vec<PathBuf>) {
        let prefix = session.browser.prefix();
        // Files only (folder upload goes through Sync, §6.6).
        let items: Vec<UploadItem> = paths
            .into_iter()
            .filter(|p| p.is_file())
            .map(|p| {
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                UploadItem { size: std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0), key: format!("{prefix}{name}"), path: p, delete_source: false, exists: None, skip: false }
            })
            .collect();
        if items.is_empty() {
            ctx.toasts.info("Only files can be uploaded here. Use Sync for folders.");
            return;
        }
        self.upload = Some(UploadFlow { items, prefix, stage: UploadStage::Review });
    }

    /// Native picker on a blocking thread so the UI keeps rendering.
    fn pick_files(&mut self, ctx: &AppCtx) {
        if self.pick.is_some() {
            return;
        }
        self.pick = Some(ctx.task(async move {
            tokio::task::spawn_blocking(|| rfd::FileDialog::new().set_title("Upload files").pick_files()).await.ok().flatten()
        }));
    }

    fn download(&mut self, ctx: &AppCtx, session: &Arc<Session>, selection: Vec<String>) {
        let s = session.clone();
        let base = session.browser.prefix();
        let dir = ctx.core.prefs.download_dir();
        let (done, total, bytes, cancel) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)), Arc::new(AtomicU64::new(0)), session.cancel.child_token());
        let (d2, t2, b2, c2) = (done.clone(), total.clone(), bytes.clone(), cancel.clone());
        let task = ctx.task(async move {
            // Folders expand to every key beneath them (from the cache).
            let mut plan: Vec<(String, PathBuf)> = vec![];
            for k in &selection {
                if k.ends_with('/') {
                    let parent = bf_core::media::parent_prefix(k);
                    for key in s.core.cache_db.object_keys_under(k).await.unwrap_or_default() {
                        plan.push((key.clone(), dir.join(local_rel(&key[parent.len()..]))));
                    }
                } else {
                    let rel = k.strip_prefix(&base).unwrap_or(bf_core::media::leaf_name(k));
                    plan.push((k.clone(), dir.join(local_rel(rel))));
                }
            }
            // Plaintext → background queue; encrypted → foreground decrypt
            // (the queue would leave unreadable ciphertext on disk).
            let (enc, plain): (Vec<_>, Vec<_>) = plan.into_iter().partition(|(k, _)| s.encryption.is_encrypted(k));
            let mut failed = vec![];
            for (k, dest) in &plain {
                let parent = dest.parent().map(Path::to_path_buf).unwrap_or_else(|| dir.clone());
                let _ = tokio::fs::create_dir_all(&parent).await;
                let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
                if let Err(e) = s.transfers.enqueue_download(k, &unique_path(&parent, &name), None).await {
                    failed.push((k.clone(), message_for(&e)));
                }
            }
            run_encrypted_downloads(s, enc, plain.len(), failed, d2, t2, b2, c2).await
        });
        self.job = Some(Job { title: "Preparing downloads…".into(), done, total, bytes, cancel, task });
    }

    fn retry_encrypted(&mut self, ctx: &AppCtx, session: &Arc<Session>, items: Vec<(String, PathBuf)>) {
        let s = session.clone();
        let (done, total, bytes, cancel) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)), Arc::new(AtomicU64::new(0)), session.cancel.child_token());
        let (d2, t2, b2, c2) = (done.clone(), total.clone(), bytes.clone(), cancel.clone());
        let task = ctx.task(async move { run_encrypted_downloads(s, items, 0, vec![], d2, t2, b2, c2).await });
        self.job = Some(Job { title: "Downloading and decrypting…".into(), done, total, bytes, cancel, task });
    }

    fn share(&mut self, ctx: &AppCtx, session: &Arc<Session>, keys: Vec<String>) {
        let files: Vec<String> = keys.into_iter().filter(|k| !k.ends_with('/')).collect();
        if files.is_empty() {
            ctx.toasts.info("Folders can't be shared. Select files instead.");
            return;
        }
        let ttl = ctx.prefs().default_link_ttl_seconds;
        let s = session.clone();
        let job = self.simple_job(ctx, "Creating links…", async move { JobDone::Share(s.sharing.create(&files, ttl).await) });
        self.job = Some(job);
    }

    fn simple_job(&self, ctx: &AppCtx, title: &str, f: impl std::future::Future<Output = JobDone> + Send + 'static) -> Job {
        Job {
            title: title.into(),
            done: Arc::new(AtomicUsize::new(0)),
            total: Arc::new(AtomicUsize::new(0)),
            bytes: Arc::new(AtomicU64::new(0)),
            cancel: CancellationToken::new(),
            task: ctx.task(f),
        }
    }

    fn bulk_job(&mut self, ctx: &AppCtx, title: &str, f: impl FnOnce(bf_app::fileops::OpProgress) -> std::pin::Pin<Box<dyn std::future::Future<Output = AppResult<BulkResult>> + Send>>) {
        let (done, total) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (d2, t2) = (done.clone(), total.clone());
        let progress: bf_app::fileops::OpProgress = Arc::new(move |d, t| {
            d2.store(d, Ordering::Relaxed);
            t2.store(t, Ordering::Relaxed);
        });
        let fut = f(progress);
        let t = title.to_string();
        let task = ctx.task(async move { JobDone::Bulk(t, fut.await) });
        self.job = Some(Job { title: title.into(), done, total, bytes: Arc::new(AtomicU64::new(0)), cancel: CancellationToken::new(), task });
        self.selection.clear();
    }

    fn encrypt_op(&mut self, ctx: &AppCtx, session: &Arc<Session>, keys: Vec<String>, op: EncOp) {
        if !session.encryption.has_key() {
            ctx.toasts.warn("Set an encryption key first (Encryption page).");
            return;
        }
        let files: Vec<String> = keys.into_iter().filter(|k| !k.ends_with('/')).collect();
        let split = session.encryption.enqueue(&files, op);
        let verb = if op == EncOp::Encrypt { "encrypt" } else { "decrypt" };
        let mut msg = format!("Queued {} to {verb}.", split.applies.len());
        // Skipped items are reported, never silently dropped (§6.5).
        if !split.already.is_empty() {
            msg.push_str(&format!(" {} already {verb}ed.", split.already.len()));
        }
        if !split.locked.is_empty() {
            msg.push_str(&format!(" {} busy.", split.locked.len()));
        }
        if !split.external.is_empty() {
            msg.push_str(&format!(" {} encrypted outside this vault (skipped).", split.external.len()));
        }
        ctx.toasts.info(msg);
        self.selection.clear();
    }

    fn open_rename(&mut self, key: &str) {
        self.modal = Some(Modal::Rename { key: key.to_string(), is_folder: key.ends_with('/'), name: bf_core::media::leaf_name(key).to_string(), err: None });
    }

    fn open_move(&mut self, keys: Vec<String>) {
        self.modal = Some(Modal::Move { keys, browse: String::new(), folders: None, list: vec![] });
    }

    fn open_delete(&mut self, keys: Vec<String>) {
        self.modal = Some(Modal::Delete { keys, count: None, n: None, typed: String::new() });
    }

    // ------------------------------------------------------------- ui --

    pub fn ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, session: &Arc<Session>) {
        let egui_ctx = ui.ctx().clone();
        self.thumbs.pump(&egui_ctx);
        self.poll_tasks(ctx, session);
        let data = session.browser.data();
        let locked = session.encryption.locked_keys();
        let scan_status = session.cache.status();
        let p = *pal(ui);

        self.toolbar(ui, ctx, session, &scan_status);
        ui.add_space(10.0);
        self.context_row(ui, ctx, session, &data, &locked);
        if let Some(e) = &scan_status.last_error {
            ui.add_space(6.0);
            widgets::callout(ui, Tone::Danger, icons::CLOUD_WARNING, e);
        }
        ui.add_space(10.0);

        // ---- listing
        let (folders, files) = data.filtered(&self.search);
        let entries: Vec<&CacheEntry> = folders.iter().chain(files.iter()).copied().collect();
        if !data.has_scanned && !data.loading {
            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                widgets::empty_state(
                    ui,
                    icons::CLOUD_ARROW_DOWN,
                    "This bucket isn't on this computer yet",
                    "Browsing is local and instant once the listing is cached. Nothing is fetched until you refresh.",
                );
                ui.add_space(14.0);
                ui.vertical_centered(|ui| {
                    if ui.add(Btn::new(Kind::Primary, "Refresh from bucket").icon(icons::ARROWS_CLOCKWISE)).clicked() {
                        self.refresh(ctx, session);
                    }
                });
                ui.add_space(30.0);
            });
        } else if entries.is_empty() {
            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                if self.search.is_empty() {
                    widgets::empty_state(ui, icons::FOLDER_OPEN, "This folder is empty", "Drop files anywhere on the window, or use Upload, to add files here.");
                    ui.add_space(14.0);
                    ui.vertical_centered(|ui| {
                        if ui.add(Btn::new(Kind::Primary, "Upload files").icon(icons::UPLOAD)).clicked() {
                            self.pick_files(ctx);
                        }
                    });
                } else {
                    widgets::empty_state(ui, icons::MAGNIFYING_GLASS, "No matches", &format!("Nothing in this folder matches “{}”.", self.search.trim()));
                }
                ui.add_space(30.0);
            });
        } else if ctx.prefs().files_grid_view {
            self.grid(ui, ctx, session, &entries, &data.counts, &locked);
        } else {
            let frame = egui::Frame::new().fill(p.surface).stroke(Stroke::new(1.0, p.border)).corner_radius(CornerRadius::same(12)).inner_margin(egui::Margin::same(0));
            frame.show(ui, |ui| {
                ui.set_width(ui.available_width());
                self.list(ui, ctx, session, &entries, &data.counts, &locked);
            });
        }

        #[cfg(feature = "dev-env")]
        if !self.dev_viewed {
            self.dev_viewed = true;
            if let Some(prefix) = crate::demo::env("BYTEFLOW_DEV_PREFIX") {
                self.open_folder(ctx, session, &prefix);
            }
            if let Some(key) = crate::demo::env("BYTEFLOW_DEV_SELECT") {
                self.selection.insert(key);
            }
            if let Some(p) = std::env::var_os("BYTEFLOW_DEV_VIEW") {
                self.viewer = Some(Viewer::open_local(&egui_ctx, Path::new(&p)));
            }
        }
        self.keyboard(ui, ctx, session, &entries);
        self.modals(&egui_ctx, ctx, session);
        self.upload_ui(&egui_ctx, ctx, session);
        self.job_ui(&egui_ctx);
        if let Some(v) = self.viewer.as_mut() {
            v.ui(&egui_ctx, ctx, session);
            let action = v.action.take();
            if !v.open {
                self.viewer = None;
            }
            match action {
                Some(ViewerAction::Download(k)) => self.download(ctx, session, vec![k]),
                Some(ViewerAction::Share(k)) => self.share(ctx, session, vec![k]),
                None => {}
            }
        }
    }

    /// Search and sort on the left, the folder's actions on the right. The
    /// buttons drop their labels when the row is narrow.
    fn toolbar(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, session: &Arc<Session>, scan: &bf_app::cache_service::ScanStatus) {
        let p = *pal(ui);
        let compact = ui.available_width() < 640.0;
        let scanning = scan.scanning || self.scan.is_some();
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let tip = |r: egui::Response, t: &str| if compact { r.on_hover_text(t) } else { r };
                if tip(ui.add(Btn::new(Kind::Primary, if compact { "" } else { "Upload" }).icon(icons::UPLOAD)), "Upload files to this folder").clicked() {
                    self.pick_files(ctx);
                }
                if tip(ui.add(Btn::new(Kind::Secondary, if compact { "" } else { "New folder" }).icon(icons::FOLDER_PLUS)), "New folder").clicked() {
                    self.modal = Some(Modal::NewFolder { name: String::new(), err: None, task: None });
                }
                if widgets::icon_btn_ex(ui, icons::ARROWS_CLOCKWISE, "Refresh from bucket", 34.0, false, !scanning).clicked() {
                    self.refresh(ctx, session);
                }
                if scanning {
                    let status = format!("Scanning… {} objects", scan.listed);
                    if !compact {
                        ui.label(RichText::new(&status).size(12.5).color(p.text_muted));
                    }
                    ui.spinner().on_hover_text(status);
                }
                // Search and sort take what's left, from the left edge.
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    // Never wider than what's left, so nothing slides under the
                    // buttons: when very narrow, sort shows only its icon.
                    let avail = ui.available_width();
                    let narrow = avail < 300.0;
                    let sort_w = if narrow { 24.0 } else { (avail * 0.4).clamp(120.0, 170.0) };
                    // The combo box adds its arrow and padding to `sort_w`.
                    let search_w = (avail - sort_w - 44.0 - 8.0).clamp(90.0, 360.0);
                    widgets::search_field(ui, &mut self.search, "Search this folder", search_w);
                    let mut sort = session.browser.sort();
                    let shown = if narrow { icons::SORT_ASCENDING.to_string() } else { format!("{}  {}", icons::SORT_ASCENDING, sort.label()) };
                    let combo = egui::ComboBox::from_id_salt("sort").selected_text(RichText::new(shown).color(p.text)).width(sort_w).height(320.0).show_ui(ui, |ui| {
                        widgets::menu_style(ui);
                        for s in BrowserSort::ALL {
                            ui.selectable_value(&mut sort, s, s.label());
                        }
                    });
                    if narrow {
                        combo.response.on_hover_text(format!("Sort: {}", sort.label()));
                    }
                    if sort != session.browser.sort() {
                        let b = session.browser.clone();
                        let _ = ctx.core.prefs.update(|p| p.files_sort = sort);
                        ctx.spawn(async move { b.set_sort(sort).await });
                    }
                });
            });
        });
    }

    /// The row between the toolbar and the listing: where you are (breadcrumbs,
    /// counts, view options) or, with a selection, what you can do with it.
    /// It is the same height either way, so the listing never moves when a
    /// selection starts or ends.
    fn context_row(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, session: &Arc<Session>, data: &bf_app::browser::BrowserData, locked: &HashSet<String>) {
        let p = *pal(ui);
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), CONTEXT_ROW_H), Sense::hover());
        let mut row = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::right_to_left(egui::Align::Center)));
        row.spacing_mut().item_spacing.x = 8.0;
        let selecting = !self.selection.is_empty();
        // View options on the right. With a selection they give way when that
        // lets the actions keep their labels, or when the row is very narrow.
        let show_view = !selecting || {
            let (full, view) = (Self::selection_full_width(&row, self.selection.len()), 150.0);
            full <= rect.width() - view || (full > rect.width() && rect.width() >= 560.0)
        };
        if show_view {
            let open = self.details_open(ctx);
            if widgets::icon_btn_ex(&mut row, icons::SIDEBAR_SIMPLE, if open { "Hide details" } else { "Show details" }, 34.0, open, true).clicked() {
                Self::set_details_open(ctx, !open);
            }
            let mut grid = ctx.prefs().files_grid_view;
            if widgets::segmented(&mut row, &mut grid, &[(true, icons::SQUARES_FOUR, ""), (false, icons::LIST_BULLETS, "")]) {
                let _ = ctx.core.prefs.update(|p| p.files_grid_view = grid);
            }
            row.add_space(4.0);
        }
        if !selecting && rect.width() >= 440.0 {
            let (nf, nfi) = (data.folders.len(), data.files.len());
            let mut parts = vec![];
            if nf > 0 {
                parts.push(format!("{nf} folder{}", if nf == 1 { "" } else { "s" }));
            }
            parts.push(format!("{nfi} file{}", if nfi == 1 { "" } else { "s" }));
            row.label(RichText::new(parts.join(" · ")).size(12.5).color(p.text_subtle));
            row.add_space(4.0);
        }
        row.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            if selecting {
                self.selection_bar(ui, ctx, session, locked);
            } else {
                self.breadcrumbs(ui, ctx, session, data);
            }
        });
    }

    /// The bucket root (house) and each folder down to this one. Folders in
    /// the middle fold into a "…" menu when the row is too narrow.
    fn breadcrumbs(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, session: &Arc<Session>, data: &bf_app::browser::BrowserData) {
        let p = *pal(ui);
        let crumbs = data.breadcrumbs();
        let at_root = crumbs.len() <= 1;
        let mut go: Option<String> = None;
        ui.spacing_mut().item_spacing.x = 4.0;
        if widgets::icon_btn_ex(ui, icons::HOUSE, "Bucket root", 30.0, at_root, true).clicked() && !at_root {
            go = Some(String::new());
        }
        let folders = &crumbs[1.min(crumbs.len())..];
        if !folders.is_empty() {
            let width = |ui: &egui::Ui, s: &str, f: egui::FontId| ui.painter().layout_no_wrap(s.to_owned(), f, Color32::PLACEHOLDER).size().x;
            let caret = |ui: &mut egui::Ui| {
                ui.label(RichText::new(icons::CARET_RIGHT).size(13.0).color(p.text_subtle));
            };
            let caret_w = width(ui, icons::CARET_RIGHT, theme::regular(13.0)) + 8.0;
            let last = folders.len() - 1;
            let names: Vec<String> = folders.iter().map(|(n, _)| bidi::visual(n).into_owned()).collect();
            let widths: Vec<f32> = names.iter().enumerate().map(|(i, s)| width(ui, s, if i == last { theme::semibold(14.0) } else { theme::medium(14.0) }) + 4.0).collect();
            let ellipsis_w = width(ui, "…", theme::medium(14.0)) + 4.0 + caret_w;
            let avail = ui.available_width();
            let needed = |hide: usize| widths[hide..].iter().map(|w| w + caret_w).sum::<f32>() + if hide > 0 { ellipsis_w } else { 0.0 };
            let mut hide = 0;
            while hide < last && needed(hide) > avail {
                hide += 1;
            }
            if hide > 0 {
                caret(ui);
                let r = ui.add(egui::Label::new(RichText::new("…").font(theme::medium(14.0)).color(p.text_muted)).sense(Sense::click()).selectable(false));
                let r = r.on_hover_text("Folders in between").on_hover_cursor(egui::CursorIcon::PointingHand);
                egui::Popup::menu(&r).show(|ui| {
                    ui.set_min_width(180.0);
                    for (name, prefix) in &folders[..hide] {
                        if widgets::menu_item(ui, icons::FOLDER_SIMPLE, &bidi::visual(name), false).clicked() {
                            go = Some(prefix.clone());
                            ui.close();
                        }
                    }
                });
            }
            for (i, (name, prefix)) in folders.iter().enumerate().skip(hide) {
                caret(ui);
                if i == last {
                    bidi::name(ui, name, theme::semibold(14.0), p.text);
                } else {
                    let r = ui.add(egui::Label::new(RichText::new(&names[i]).font(theme::medium(14.0)).color(p.text_muted)).sense(Sense::click()).selectable(false));
                    if r.hovered() {
                        ui.painter().hline(r.rect.x_range(), r.rect.bottom() - 1.0, Stroke::new(1.0, p.text_muted));
                    }
                    if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        go = Some(prefix.clone());
                    }
                }
            }
        }
        if let Some(g) = go {
            self.open_folder(ctx, session, &g);
        }
    }

    /// Width the selection bar needs to show every action with its label.
    fn selection_full_width(ui: &egui::Ui, n: usize) -> f32 {
        let width = |s: &str, f: egui::FontId| ui.painter().layout_no_wrap(s.to_owned(), f, Color32::PLACEHOLDER).size().x;
        // A small labelled button: text + 15 px icon + 7 px gap + 2 × 10 px padding.
        let btn = |s: &str| width(s, theme::medium(13.0)) + 42.0;
        let actions: f32 = ["Download", "Share", "Encrypt", "Decrypt", "Move"].iter().map(|s| btn(s) + 2.0).sum();
        12.0 + 30.0 + 6.0 + width(&format!("{n} selected"), theme::semibold(14.0)) + 12.0 + actions + 16.0 + btn("Delete")
    }

    /// Bulk actions for the selection, filling the context row. Labels drop
    /// to icons (with tooltips) when they don't fit.
    fn selection_bar(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, session: &Arc<Session>, locked: &HashSet<String>) {
        #[derive(Clone, Copy)]
        enum Act {
            Download,
            Share,
            Encrypt,
            Decrypt,
            Move,
            Delete,
        }
        let p = *pal(ui);
        let sel: Vec<String> = self.selection.iter().cloned().collect();
        let any_locked = sel.iter().any(|k| locked.contains(k));
        let has_files = sel.iter().any(|k| !k.ends_with('/'));
        let acts = [
            (Act::Download, icons::DOWNLOAD, "Download", true),
            (Act::Share, icons::LINK_SIMPLE, "Share", has_files),
            (Act::Encrypt, icons::LOCK_SIMPLE, "Encrypt", !any_locked && has_files),
            (Act::Decrypt, icons::LOCK_OPEN, "Decrypt", !any_locked && has_files),
            (Act::Move, icons::FOLDER_OPEN, "Move", !any_locked),
        ];
        let bar = ui.available_rect_before_wrap();
        ui.allocate_rect(bar, Sense::hover());
        ui.painter().rect(bar, CornerRadius::same(10), p.primary_soft, Stroke::new(1.0, p.primary.gamma_multiply(0.25)), egui::StrokeKind::Inside);
        let label = format!("{} selected", sel.len());
        let compact = Self::selection_full_width(ui, sel.len()) > bar.width();
        let mut act = None;
        let mut inner = ui.new_child(egui::UiBuilder::new().max_rect(bar.shrink2(vec2(6.0, 0.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
        inner.spacing_mut().item_spacing.x = 2.0;
        if widgets::icon_btn_ex(&mut inner, icons::X, "Clear selection (Esc)", 30.0, false, true).clicked() {
            self.selection.clear();
        }
        inner.add_space(4.0);
        let short = compact && bar.width() < 400.0;
        inner.label(RichText::new(if short { sel.len().to_string() } else { label.clone() }).font(theme::semibold(14.0)).color(p.primary_text)).on_hover_text(&label);
        inner.add_space(if compact { 8.0 } else { 12.0 });
        for (a, icon, text, enabled) in acts {
            let clicked = if compact {
                widgets::icon_btn_ex(&mut inner, icon, text, 30.0, false, enabled).clicked()
            } else {
                inner.add(Btn::new(Kind::Ghost, text).icon(icon).small().enabled(enabled)).clicked()
            };
            if clicked {
                act = Some(a);
            }
        }
        inner.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let r = ui.add(Btn::new(Kind::DangerOutline, if compact { "" } else { "Delete" }).icon(icons::TRASH).small().enabled(!any_locked));
            let r = if compact { r.on_hover_text("Delete") } else { r };
            if r.clicked() {
                act = Some(Act::Delete);
            }
        });
        match act {
            Some(Act::Download) => self.download(ctx, session, sel),
            Some(Act::Share) => self.share(ctx, session, sel),
            Some(Act::Encrypt) => self.encrypt_op(ctx, session, sel, EncOp::Encrypt),
            Some(Act::Decrypt) => self.encrypt_op(ctx, session, sel, EncOp::Decrypt),
            Some(Act::Move) => self.open_move(sel),
            Some(Act::Delete) => self.open_delete(sel),
            None => {}
        }
    }

    fn click(&mut self, idx: usize, key: &str, entries: &[&CacheEntry], mods: egui::Modifiers) {
        if mods.shift
            && let Some(a) = self.anchor
        {
            let (lo, hi) = (a.min(idx), a.max(idx));
            if !mods.command {
                self.selection.clear();
            }
            for e in &entries[lo..=hi] {
                self.selection.insert(e.key.clone());
            }
        } else if mods.command {
            if !self.selection.remove(key) {
                self.selection.insert(key.to_string());
            }
            self.anchor = Some(idx);
        } else {
            self.selection.clear();
            self.selection.insert(key.to_string());
            self.anchor = Some(idx);
        }
    }

    fn apply_hit(&mut self, ctx: &AppCtx, session: &Arc<Session>, hit: Hit, entries: &[&CacheEntry], locked: &HashSet<String>) {
        match hit {
            Hit::Open(i) => {
                let e = entries[i];
                self.activate(ctx, session, e, locked.contains(&e.key));
            }
            Hit::Click(i, mods) => self.click(i, &entries[i].key, entries, mods),
            Hit::Toggle(i) => {
                let k = &entries[i].key;
                if !self.selection.remove(k) {
                    self.selection.insert(k.clone());
                }
                self.anchor = Some(i);
            }
        }
    }

    fn activate(&mut self, ctx: &AppCtx, session: &Arc<Session>, e: &CacheEntry, locked: bool) {
        if e.is_folder {
            self.open_folder(ctx, session, &e.key);
        } else if locked {
            ctx.toasts.info("This file is busy with an encryption task. Try again when it finishes.");
        } else if bf_core::media::media_kind_for_key(&e.key) == bf_core::media::MediaKind::Audio && audio::AudioDock::supported() {
            // Audio plays in the details panel, not the full-window viewer.
            self.selection.clear();
            self.selection.insert(e.key.clone());
            Self::set_details_open(ctx, true);
            self.audio.play(ctx, session, &e.key);
        } else {
            self.viewer = Some(Viewer::open(ctx, session, &e.key, e.size));
        }
    }

    fn badges(ui: &mut egui::Ui, session: &Arc<Session>, key: &str, locked: bool) {
        if locked {
            widgets::badge_icon(ui, icons::HOURGLASS, "Encrypting", Tone::Violet);
        } else if session.encryption.is_external(key) {
            widgets::badge_icon(ui, icons::KEY, "External key", Tone::Warn);
        } else if session.encryption.is_encrypted(key) {
            widgets::badge_icon(ui, icons::LOCK_SIMPLE, "", Tone::Success).on_hover_text("Encrypted");
        }
    }

    /// Small rounded checkbox, painted.
    fn paint_check(ui: &egui::Ui, rect: egui::Rect, checked: bool, hovered: bool) {
        let p = pal(ui);
        if checked {
            ui.painter().rect_filled(rect, CornerRadius::same(5), p.primary);
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, icons::CHECK, theme::bold(12.0), Color32::WHITE);
        } else {
            let stroke = if hovered { p.primary } else { p.border_strong };
            ui.painter().rect(rect, CornerRadius::same(5), p.surface, Stroke::new(1.5, stroke), egui::StrokeKind::Inside);
        }
    }

    fn list(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, session: &Arc<Session>, entries: &[&CacheEntry], counts: &HashMap<String, Usage>, locked: &HashSet<String>) {
        let now = bf_app::now_ms();
        let p = *pal(ui);
        let any_sel = !self.selection.is_empty();
        // Narrow lists (small window, details open) drop the Uploaded column.
        let show_date = ui.available_width() >= 540.0;
        let (size_w, date_w) = if show_date { (120.0, 150.0) } else { (84.0, 0.0) };
        // Header.
        let (hr, _) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::hover());
        ui.painter().rect_filled(hr, CornerRadius { nw: 12, ne: 12, sw: 0, se: 0 }, p.surface_2.gamma_multiply(0.6));
        ui.painter().hline(hr.x_range(), hr.bottom() - 0.5, Stroke::new(1.0, p.border));
        let hf = theme::medium(12.0);
        let all_checked = !entries.is_empty() && entries.iter().all(|e| self.selection.contains(&e.key));
        let check_r = egui::Rect::from_center_size(egui::pos2(hr.left() + 25.0, hr.center().y), vec2(16.0, 16.0));
        let cr = ui.interact(check_r.expand(4.0), egui::Id::unique("files-check-all"), Sense::click());
        Self::paint_check(ui, check_r, all_checked, cr.hovered());
        if cr.on_hover_text(if all_checked { "Select none" } else { "Select all" }).clicked() {
            if all_checked {
                self.selection.clear();
            } else {
                self.selection = entries.iter().map(|e| e.key.clone()).collect();
            }
        }
        ui.painter().text(egui::pos2(hr.left() + 56.0, hr.center().y), Align2::LEFT_CENTER, "Name", hf.clone(), p.text_subtle);
        if show_date {
            ui.painter().text(egui::pos2(hr.right() - 20.0 - date_w, hr.center().y), Align2::LEFT_CENTER, "Size", hf.clone(), p.text_subtle);
            ui.painter().text(egui::pos2(hr.right() - 20.0, hr.center().y), Align2::RIGHT_CENTER, "Uploaded", hf, p.text_subtle);
        } else {
            ui.painter().text(egui::pos2(hr.right() - 20.0, hr.center().y), Align2::RIGHT_CENTER, "Size", hf, p.text_subtle);
        }

        let mut hit: Option<Hit> = None;
        egui::ScrollArea::vertical().auto_shrink(false).show_rows(ui, ROW_H, entries.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for i in range {
                let e = entries[i];
                let is_locked = locked.contains(&e.key);
                let selected = self.selection.contains(&e.key);
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
                let hovered = resp.hovered();
                if selected {
                    ui.painter().rect_filled(rect.shrink2(vec2(6.0, 2.0)), CornerRadius::same(8), p.primary_soft);
                } else if hovered {
                    ui.painter().rect_filled(rect.shrink2(vec2(6.0, 2.0)), CornerRadius::same(8), p.surface_2);
                }
                if i + 1 < entries.len() && !selected {
                    ui.painter().hline((rect.left() + 56.0)..=(rect.right() - 14.0), rect.bottom() - 0.5, Stroke::new(1.0, p.border.gamma_multiply(0.7)));
                }
                // Checkbox (shown on hover, when selected, or while selecting).
                let check_r = egui::Rect::from_center_size(egui::pos2(rect.left() + 25.0, rect.center().y), vec2(16.0, 16.0));
                let pointer_in_check = resp.hover_pos().is_some_and(|pp| check_r.expand(6.0).contains(pp));
                if selected || hovered || any_sel {
                    Self::paint_check(ui, check_r, selected, pointer_in_check);
                }
                // Icon or thumbnail.
                let icon_r = egui::Rect::from_min_size(egui::pos2(rect.left() + 46.0, rect.center().y - 16.0), vec2(32.0, 32.0));
                match (!e.is_folder).then(|| self.thumbs.get(ctx, session, &e.key)).flatten() {
                    Some(t) => {
                        egui::Image::new(&t).corner_radius(7).fit_to_exact_size(icon_r.size()).paint_at(ui, icon_r);
                    }
                    None => widgets::paint_file_tile(ui, icon_r, &e.key, e.is_folder),
                }
                // Name + badges.
                let name_r = egui::Rect::from_min_max(egui::pos2(icon_r.right() + 12.0, rect.top()), egui::pos2(rect.right() - 20.0 - date_w - size_w - 12.0, rect.bottom()));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(name_r).layout(egui::Layout::left_to_right(egui::Align::Center)));
                child.spacing_mut().item_spacing.x = 8.0;
                let name = bf_core::media::leaf_name(&e.key);
                bidi::name(&mut child, name, theme::regular(14.0), p.text);
                Self::badges(&mut child, session, &e.key, is_locked);
                // Size, date.
                let size = if e.is_folder {
                    counts.get(&e.key).map(|u| widgets::items(u.files)).unwrap_or_else(|| "—".into())
                } else {
                    format::bytes(e.size)
                };
                if show_date {
                    ui.painter().text(egui::pos2(rect.right() - 20.0 - date_w, rect.center().y), Align2::LEFT_CENTER, size, theme::regular(13.0), p.text_muted);
                    let when = e.last_modified_ms.map(|t| format::relative_age(t, now)).unwrap_or_else(|| "—".into());
                    ui.painter().text(egui::pos2(rect.right() - 20.0, rect.center().y), Align2::RIGHT_CENTER, when, theme::regular(13.0), p.text_muted);
                } else {
                    ui.painter().text(egui::pos2(rect.right() - 20.0, rect.center().y), Align2::RIGHT_CENTER, size, theme::regular(13.0), p.text_muted);
                }
                let resp = match e.last_modified_ms {
                    Some(t) => resp.on_hover_text_at_pointer(format::local_time(t)),
                    None => resp,
                };
                if resp.double_clicked() {
                    hit = Some(Hit::Open(i));
                } else if resp.clicked() {
                    hit = Some(if pointer_in_check { Hit::Toggle(i) } else { Hit::Click(i, ui.input(|inp| inp.modifiers)) });
                }
                resp.context_menu(|ui| self.context_menu(ui, ctx, session, e, is_locked));
            }
            ui.add_space(6.0);
        });
        if let Some(h) = hit {
            self.apply_hit(ctx, session, h, entries, locked);
        }
    }

    fn grid(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, session: &Arc<Session>, entries: &[&CacheEntry], counts: &HashMap<String, Usage>, locked: &HashSet<String>) {
        let p = *pal(ui);
        let gap = 14.0;
        let min_w = 164.0;
        let avail = ui.available_width() - 2.0;
        let per_row = ((avail + gap) / (min_w + gap)).floor().max(1.0) as usize;
        let tile_w = ((avail - gap * (per_row as f32 - 1.0)) / per_row as f32).floor();
        let tile = vec2(tile_w, tile_w * 0.68 + 62.0);
        let rows = entries.len().div_ceil(per_row);
        let any_sel = !self.selection.is_empty();
        let mut hit: Option<Hit> = None;
        egui::ScrollArea::vertical().auto_shrink(false).show_rows(ui, tile.y + gap, rows, |ui, range| {
            for r in range {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for i in r * per_row..((r + 1) * per_row).min(entries.len()) {
                        let e = entries[i];
                        let is_locked = locked.contains(&e.key);
                        let selected = self.selection.contains(&e.key);
                        let (rect, resp) = ui.allocate_exact_size(tile, Sense::click());
                        let hovered = resp.hovered();
                        let (fill, stroke) = if selected {
                            (p.primary_soft, Stroke::new(1.5, p.primary))
                        } else if hovered {
                            (p.surface, Stroke::new(1.0, p.border_strong))
                        } else {
                            (p.surface, Stroke::new(1.0, p.border))
                        };
                        ui.painter().rect(rect, CornerRadius::same(12), fill, stroke, egui::StrokeKind::Inside);
                        let media = egui::Rect::from_min_size(rect.min + vec2(8.0, 8.0), vec2(rect.width() - 16.0, rect.width() * 0.68 - 8.0));
                        ui.painter().rect_filled(media, CornerRadius::same(8), if selected { p.surface } else { p.surface_2 });
                        match (!e.is_folder).then(|| self.thumbs.get(ctx, session, &e.key)).flatten() {
                            Some(t) => {
                                let ts = t.size_vec2();
                                let s = (media.width() / ts.x).max(media.height() / ts.y);
                                let shown = ts * s;
                                // Cover: crop the overflow through UVs.
                                let u = ((shown.x - media.width()) / shown.x / 2.0).max(0.0);
                                let v = ((shown.y - media.height()) / shown.y / 2.0).max(0.0);
                                egui::Image::new(&t)
                                    .uv(egui::Rect::from_min_max(egui::pos2(u, v), egui::pos2(1.0 - u, 1.0 - v)))
                                    .corner_radius(8)
                                    .paint_at(ui, media);
                            }
                            None => {
                                let s = (media.height() * 0.55).min(64.0);
                                widgets::paint_file_tile(ui, egui::Rect::from_center_size(media.center(), vec2(s, s)), &e.key, e.is_folder);
                            }
                        }
                        // Checkbox overlay.
                        let check_r = egui::Rect::from_min_size(media.min + vec2(8.0, 8.0), vec2(18.0, 18.0));
                        let pointer_in_check = resp.hover_pos().is_some_and(|pp| check_r.expand(5.0).contains(pp));
                        if selected || hovered || any_sel {
                            Self::paint_check(ui, check_r, selected, pointer_in_check);
                        }
                        // Name, meta and badges.
                        let text_r = egui::Rect::from_min_max(egui::pos2(rect.left() + 12.0, media.bottom() + 8.0), egui::pos2(rect.right() - 12.0, rect.bottom() - 8.0));
                        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(text_r).layout(egui::Layout::top_down(egui::Align::Min)));
                        child.spacing_mut().item_spacing.y = 2.0;
                        bidi::name(&mut child, bf_core::media::leaf_name(&e.key), theme::medium(13.5), p.text);
                        child.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            let sub = if e.is_folder { counts.get(&e.key).map(|u| widgets::items(u.files)).unwrap_or_else(|| "Folder".into()) } else { format::bytes(e.size) };
                            ui.label(RichText::new(sub).size(12.0).color(p.text_subtle));
                            Self::badges(ui, session, &e.key, is_locked);
                        });
                        if resp.double_clicked() {
                            hit = Some(Hit::Open(i));
                        } else if resp.clicked() {
                            hit = Some(if pointer_in_check { Hit::Toggle(i) } else { Hit::Click(i, ui.input(|inp| inp.modifiers)) });
                        }
                        resp.context_menu(|ui| self.context_menu(ui, ctx, session, e, is_locked));
                    }
                });
                ui.add_space(gap);
            }
        });
        if let Some(h) = hit {
            self.apply_hit(ctx, session, h, entries, locked);
        }
    }

    fn context_menu(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, session: &Arc<Session>, e: &CacheEntry, locked: bool) {
        let key = e.key.clone();
        ui.set_min_width(210.0);
        if locked {
            ui.label(RichText::new(format!("{}  Busy with an encryption task", icons::HOURGLASS)).color(theme::muted(ui)));
            return;
        }
        if widgets::menu_item(ui, if e.is_folder { icons::FOLDER_OPEN } else { icons::EYE }, "Open", false).clicked() {
            self.activate(ctx, session, e, locked);
            ui.close();
        }
        if widgets::menu_item(ui, icons::DOWNLOAD, "Download", false).clicked() {
            self.download(ctx, session, vec![key.clone()]);
            ui.close();
        }
        if !e.is_folder {
            if widgets::menu_item(ui, icons::LINK_SIMPLE, "Share link", false).clicked() {
                self.share(ctx, session, vec![key.clone()]);
                ui.close();
            }
            let enc = session.encryption.is_encrypted(&key);
            if !enc && widgets::menu_item(ui, icons::LOCK_SIMPLE, "Encrypt", false).clicked() {
                self.encrypt_op(ctx, session, vec![key.clone()], EncOp::Encrypt);
                ui.close();
            }
            if enc && !session.encryption.is_external(&key) && widgets::menu_item(ui, icons::LOCK_OPEN, "Decrypt", false).clicked() {
                self.encrypt_op(ctx, session, vec![key.clone()], EncOp::Decrypt);
                ui.close();
            }
        }
        widgets::menu_separator(ui);
        if widgets::menu_item(ui, icons::PENCIL_SIMPLE, "Rename", false).clicked() {
            self.open_rename(&key);
            ui.close();
        }
        if widgets::menu_item(ui, icons::FOLDER_OPEN, "Move to…", false).clicked() {
            self.open_move(vec![key.clone()]);
            ui.close();
        }
        if !self.details_open(ctx) && widgets::menu_item(ui, icons::INFO, "Details", false).clicked() {
            self.selection.clear();
            self.selection.insert(key.clone());
            Self::set_details_open(ctx, true);
            ui.close();
        }
        widgets::menu_separator(ui);
        if widgets::menu_item(ui, icons::TRASH, "Delete", true).clicked() {
            self.open_delete(vec![key]);
            ui.close();
        }
    }

    fn keyboard(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, session: &Arc<Session>, entries: &[&CacheEntry]) {
        if self.modal.is_some() || self.upload.is_some() || self.viewer.is_some() || self.job.is_some() || ui.ctx().egui_wants_keyboard_input() {
            return;
        }
        let (back, all, del, esc, f2, enter) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::Backspace) || (i.modifiers.alt && i.key_pressed(egui::Key::ArrowLeft)),
                i.modifiers.command && i.key_pressed(egui::Key::A),
                i.key_pressed(egui::Key::Delete),
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::F2),
                i.key_pressed(egui::Key::Enter),
            )
        });
        if back {
            self.selection.clear();
            let b = session.browser.clone();
            ctx.spawn(async move {
                b.go_parent().await;
            });
        }
        if all {
            self.selection = entries.iter().map(|e| e.key.clone()).collect();
        }
        if esc {
            self.selection.clear();
        }
        let locked = session.encryption.locked_keys();
        if del && !self.selection.is_empty() && !self.selection.iter().any(|k| locked.contains(k)) {
            self.open_delete(self.selection.iter().cloned().collect());
        }
        if self.selection.len() == 1
            && let Some(k) = self.selection.iter().next().cloned()
            && !locked.contains(&k)
        {
            if f2 {
                self.open_rename(&k);
            } else if enter && let Some(e) = entries.iter().find(|e| e.key == k) {
                let e = (*e).clone();
                self.activate(ctx, session, &e, false);
            }
        }
    }

    fn poll_tasks(&mut self, ctx: &AppCtx, session: &Arc<Session>) {
        if let Some(r) = self.scan.as_ref().and_then(Task::poll) {
            self.scan = None;
            match r {
                Ok(()) => ctx.toasts.ok("Bucket listing refreshed."),
                Err(e) => ctx.toasts.error(message_for(&e)),
            }
        }
        if let Some(r) = self.pick.as_ref().and_then(Task::poll) {
            self.pick = None;
            if let Some(paths) = r {
                self.start_upload(ctx, session, paths);
            }
        }
        if let Some(r) = self.job.as_ref().and_then(|j| j.task.poll()) {
            self.job = None;
            match r {
                JobDone::Bulk(title, Ok(b)) => {
                    if b.ok() {
                        ctx.toasts.ok(format!("{title} — done ({} items).", b.total));
                    } else {
                        let mut lines: Vec<String> = b.failed.iter().take(200).map(|(k, m)| format!("{k} — {m}")).collect();
                        if b.failed.len() > 200 {
                            lines.push(format!("…and {} more", b.failed.len() - 200));
                        }
                        self.modal = Some(Modal::Report { title: format!("{title}: {} of {} didn't complete", b.failed.len(), b.total), lines });
                    }
                    self.reload(ctx, session);
                }
                JobDone::Bulk(_, Err(e)) => {
                    ctx.toasts.error(message_for(&e));
                    self.reload(ctx, session);
                }
                JobDone::Message(Ok(m)) => {
                    ctx.toasts.ok(m);
                    self.reload(ctx, session);
                }
                JobDone::Message(Err(m)) => ctx.toasts.error(m),
                JobDone::Downloads { done, failed, remaining, needs_passphrase } => {
                    if needs_passphrase {
                        self.modal = Some(Modal::Passphrase { text: String::new(), retry: remaining });
                    } else if failed.is_empty() {
                        if done > 0 {
                            ctx.toasts.ok(format!("{done} download(s) started or saved to {}.", ctx.core.prefs.download_dir().display()));
                        }
                    } else {
                        self.modal = Some(Modal::Report { title: format!("{} download(s) failed", failed.len()), lines: failed.iter().map(|(k, m)| format!("{k} — {m}")).collect() });
                    }
                }
                JobDone::Share(Ok(links)) => self.modal = Some(Modal::Links { links, ttl: ctx.prefs().default_link_ttl_seconds }),
                JobDone::Share(Err(e)) => ctx.toasts.error(message_for(&e)),
            }
        }
    }

    fn job_ui(&mut self, egui_ctx: &egui::Context) {
        let Some(job) = &self.job else { return };
        let title = job.title.clone();
        widgets::modal(egui_ctx, ModalSpec::new("files-job", &title).icon(icons::HOURGLASS, Tone::Primary).width(420.0).locked(), |ui| {
            let (d, t, b) = (job.done.load(Ordering::Relaxed), job.total.load(Ordering::Relaxed), job.bytes.load(Ordering::Relaxed));
            if t > 0 {
                let label = if b > 0 { format!("{d} of {t} · {}", format::bytes(b)) } else { format!("{d} of {t}") };
                widgets::progress(ui, d as f32 / t as f32, label);
            } else {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Working…").color(theme::muted(ui)));
                });
            }
            widgets::modal_footer(ui, |ui| {
                if widgets::secondary(ui, "Cancel").clicked() {
                    job.cancel.cancel();
                }
            });
        });
        egui_ctx.request_repaint_after(std::time::Duration::from_millis(150));
    }

    // ----------------------------------------------------------- modals --

    fn modals(&mut self, egui_ctx: &egui::Context, ctx: &AppCtx, session: &Arc<Session>) {
        let Some(mut modal) = self.modal.take() else { return };
        let mut keep = true;
        let prefix = session.browser.prefix();
        let where_ = folder_label(&prefix);
        let (title, subtitle, icon, tone, width): (String, String, &str, Tone, f32) = match &modal {
            Modal::NewFolder { .. } => ("New folder".into(), format!("Created in {where_}"), icons::FOLDER_PLUS, Tone::Primary, 440.0),
            Modal::Rename { is_folder, key, .. } => (
                if *is_folder { "Rename folder".into() } else { "Rename file".into() },
                bf_core::media::leaf_name(key).to_string(),
                icons::PENCIL_SIMPLE,
                Tone::Primary,
                440.0,
            ),
            Modal::Move { keys, .. } => (format!("Move {} item{}", keys.len(), if keys.len() == 1 { "" } else { "s" }), "Choose a destination folder".into(), icons::FOLDER_OPEN, Tone::Primary, 500.0),
            Modal::Delete { .. } => ("Delete permanently?".into(), "Deleted objects can't be recovered.".into(), icons::TRASH, Tone::Danger, 440.0),
            Modal::Links { links, ttl } => (
                if links.len() == 1 { "Share link ready".into() } else { format!("{} share links ready", links.len()) },
                format!("Valid for {}", format::duration_secs(*ttl)),
                icons::LINK_SIMPLE,
                Tone::Success,
                520.0,
            ),
            Modal::Report { title, .. } => (title.clone(), String::new(), icons::WARNING, Tone::Warn, 540.0),
            Modal::Passphrase { .. } => ("Passphrase needed".into(), "Some files were encrypted outside this vault".into(), icons::KEY, Tone::Warn, 440.0),
        };
        let spec = ModalSpec::new("files-modal", &title).subtitle(&subtitle).icon(icon, tone).width(width);
        let dismissed = widgets::modal(egui_ctx, spec, |ui| match &mut modal {
            Modal::NewFolder { name, err, task } => {
                let r = widgets::field(ui, "Folder name", |ui| widgets::text_field(ui, name, "e.g. Photos 2026"));
                if task.is_none() {
                    r.request_focus();
                }
                if let Some(e) = err {
                    widgets::error_text(ui, e);
                }
                if let Some(res) = task.as_ref().and_then(Task::poll) {
                    *task = None;
                    match res {
                        Ok(_) => {
                            keep = false;
                            self.reload(ctx, session);
                        }
                        Err(e) => *err = Some(message_for(&e)),
                    }
                }
                widgets::modal_footer(ui, |ui| {
                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (widgets::primary_enabled(ui, task.is_none(), "Create folder").clicked() || enter) && task.is_none() {
                        match validate_name(name) {
                            Err(e) => *err = Some(e.into()),
                            Ok(()) => {
                                let (ops, p, n) = (session.ops.clone(), prefix.clone(), name.clone());
                                *task = Some(ctx.task(async move { ops.create_folder(&p, &n).await }));
                            }
                        }
                    }
                    if widgets::secondary(ui, "Cancel").clicked() {
                        keep = false;
                    }
                    if task.is_some() {
                        ui.spinner();
                    }
                });
            }
            Modal::Rename { key, is_folder, name, err } => {
                widgets::field(ui, "New name", |ui| widgets::text_field(ui, name, "Name")).request_focus();
                if *is_folder {
                    widgets::hint(ui, "Renaming a folder copies every object under it to the new path, then deletes the old ones.");
                }
                if let Some(e) = err {
                    widgets::error_text(ui, e);
                }
                widgets::modal_footer(ui, |ui| {
                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if widgets::primary(ui, "Rename").clicked() || enter {
                        match validate_name(name) {
                            Err(e) => *err = Some(e.into()),
                            Ok(()) => {
                                let (ops, k, n, folder) = (session.ops.clone(), key.clone(), name.trim().to_string(), *is_folder);
                                keep = false;
                                if folder {
                                    self.bulk_job(ctx, "Renaming folder", move |p| Box::pin(async move { ops.rename_folder(&k, &n, Some(p)).await }));
                                } else {
                                    let job = self.simple_job(ctx, "Renaming…", async move {
                                        JobDone::Message(ops.rename_file(&k, &n).await.map(|_| "Renamed.".to_string()).map_err(|e| message_for(&e)))
                                    });
                                    self.job = Some(job);
                                }
                            }
                        }
                    }
                    if widgets::secondary(ui, "Cancel").clicked() {
                        keep = false;
                    }
                });
            }
            Modal::Move { keys, browse, folders, list } => {
                let p = *pal(ui);
                if folders.is_none() && list.is_empty() {
                    let (c, b) = (session.core.cache_db.clone(), browse.clone());
                    *folders = Some(ctx.task(async move { Ok(c.children_of(&b).await?.into_iter().filter(|e| e.is_folder).map(|e| e.key).collect()) }));
                }
                if let Some(r) = folders.as_ref().and_then(Task::poll) {
                    *folders = None;
                    *list = r.unwrap_or_default();
                    list.sort();
                    if list.is_empty() {
                        list.push(String::new()); // sentinel: loaded, no subfolders
                    }
                }
                ui.horizontal(|ui| {
                    let up = !browse.is_empty();
                    if widgets::icon_btn_ex(ui, icons::ARROW_UP, "Up one level", 30.0, false, up).clicked() {
                        *browse = bf_core::media::parent_prefix(browse).to_string();
                        list.clear();
                    }
                    ui.label(RichText::new(icons::FOLDER_OPEN).color(p.text_subtle));
                    ui.add(egui::Label::new(RichText::new(if browse.is_empty() { "Bucket root".to_string() } else { format!("/{browse}") }).font(theme::medium(14.0)).color(p.text)).truncate());
                });
                ui.add_space(4.0);
                egui::Frame::new().stroke(Stroke::new(1.0, p.border)).corner_radius(CornerRadius::same(10)).inner_margin(egui::Margin::same(4)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical().max_height(260.0).min_scrolled_height(160.0).auto_shrink([false, false]).show(ui, |ui| {
                        let mut go = None;
                        let real: Vec<&String> = list.iter().filter(|f| !f.is_empty()).collect();
                        if folders.is_some() {
                            ui.add_space(12.0);
                            ui.vertical_centered(|ui| ui.spinner());
                        } else if real.is_empty() {
                            ui.add_space(30.0);
                            ui.vertical_centered(|ui| ui.label(RichText::new("No subfolders here").color(p.text_subtle)));
                        }
                        for f in real {
                            let into_self = keys.iter().any(|k| k.ends_with('/') && f.starts_with(k.as_str()));
                            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), if into_self { Sense::hover() } else { Sense::click() });
                            if resp.hovered() && !into_self {
                                ui.painter().rect_filled(rect, CornerRadius::same(7), p.surface_2);
                            }
                            let alpha = if into_self { 0.4 } else { 1.0 };
                            widgets::paint_file_tile(ui, egui::Rect::from_center_size(egui::pos2(rect.left() + 22.0, rect.center().y), vec2(26.0, 26.0)), f, true);
                            ui.painter().text(egui::pos2(rect.left() + 44.0, rect.center().y), Align2::LEFT_CENTER, bidi::visual(bf_core::media::leaf_name(f)), theme::regular(14.0), p.text.gamma_multiply(alpha));
                            ui.painter().text(egui::pos2(rect.right() - 12.0, rect.center().y), Align2::RIGHT_CENTER, icons::CARET_RIGHT, theme::regular(14.0), p.text_subtle.gamma_multiply(alpha));
                            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                go = Some(f.clone());
                            }
                        }
                        if let Some(g) = go {
                            *browse = g;
                            list.clear();
                        }
                    });
                });
                widgets::modal_footer(ui, |ui| {
                    if ui.add(Btn::new(Kind::Primary, "Move here").icon(icons::ARROW_RIGHT)).clicked() {
                        let (ops, ks, dest) = (session.ops.clone(), keys.clone(), browse.clone());
                        keep = false;
                        self.bulk_job(ctx, "Moving", move |p| Box::pin(async move { ops.bulk_move(&ks, &dest, Some(p)).await }));
                    }
                    if widgets::secondary(ui, "Cancel").clicked() {
                        keep = false;
                    }
                });
            }
            Modal::Delete { keys, count, n, typed } => {
                if count.is_none() && n.is_none() {
                    let (ops, ks) = (session.ops.clone(), keys.clone());
                    *count = Some(ctx.task(async move { ops.count_for_delete(&ks).await }));
                }
                if let Some(r) = count.as_ref().and_then(Task::poll) {
                    *count = None;
                    *n = Some(r.unwrap_or(keys.len()));
                }
                match n {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(RichText::new("Counting objects…").color(theme::muted(ui)));
                        });
                    }
                    Some(total) => {
                        let what = if keys.len() == 1 { format!("“{}”", bf_core::media::leaf_name(&keys[0])) } else { format!("{} items", keys.len()) };
                        widgets::callout(ui, Tone::Danger, icons::WARNING, &format!("{what} — {total} object(s) — will be deleted from the bucket. This can't be undone."));
                        let needs_typed = *total > TYPED_DELETE_THRESHOLD;
                        if needs_typed {
                            ui.add_space(6.0);
                            widgets::field(ui, "Type DELETE to confirm", |ui| widgets::text_field(ui, typed, "DELETE"));
                        }
                        let ok = !needs_typed || typed.trim() == "DELETE";
                        widgets::modal_footer(ui, |ui| {
                            if ui.add(Btn::new(Kind::Danger, "Delete").icon(icons::TRASH).enabled(ok)).clicked() {
                                let (ops, ks) = (session.ops.clone(), keys.clone());
                                keep = false;
                                if ks.len() == 1 && !ks[0].ends_with('/') {
                                    let k = ks[0].clone();
                                    let job = self.simple_job(ctx, "Deleting…", async move {
                                        JobDone::Message(ops.delete_file(&k).await.map(|_| "Deleted.".to_string()).map_err(|e| message_for(&e)))
                                    });
                                    self.job = Some(job);
                                    self.selection.clear();
                                } else {
                                    self.bulk_job(ctx, "Deleting", move |p| Box::pin(async move { ops.bulk_delete(&ks, Some(p)).await }));
                                }
                            }
                            if widgets::secondary(ui, "Cancel").clicked() {
                                keep = false;
                            }
                        });
                    }
                }
            }
            Modal::Links { links, .. } => {
                let p = *pal(ui);
                widgets::callout(ui, Tone::Warn, icons::WARNING, "A link can't be revoked before it expires without rotating the access key in the Cloudflare dashboard.");
                ui.add_space(6.0);
                egui::Frame::new().stroke(Stroke::new(1.0, p.border)).corner_radius(CornerRadius::same(10)).inner_margin(egui::Margin::symmetric(10, 4)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                        for (i, l) in links.iter().enumerate() {
                            if i > 0 {
                                widgets::divider(ui);
                            }
                            ui.horizontal(|ui| {
                                widgets::file_tile(ui, &l.remote_key, false, 28.0);
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.add(Btn::new(Kind::Secondary, "Copy").icon(icons::COPY).small()).clicked() {
                                        let secs = ctx.prefs().clipboard_clear_seconds;
                                        platform::clipboard::copy_share_link(egui_ctx, &l.url, std::time::Duration::from_secs(secs));
                                        ctx.toasts.ok(if secs > 0 { format!("Copied. The clipboard clears in {secs}s.") } else { "Copied.".into() });
                                    }
                                    bidi::name(ui, bf_core::media::leaf_name(&l.remote_key), theme::regular(14.0), p.text);
                                });
                            });
                        }
                    });
                });
                widgets::modal_footer(ui, |ui| {
                    if widgets::primary(ui, "Done").clicked() {
                        keep = false;
                    }
                    if links.len() > 1 && ui.add(Btn::new(Kind::Secondary, "Copy all").icon(icons::COPY)).clicked() {
                        let all = links.iter().map(|l| l.url.as_str()).collect::<Vec<_>>().join("\n");
                        let secs = ctx.prefs().clipboard_clear_seconds;
                        platform::clipboard::copy_share_link(egui_ctx, &all, std::time::Duration::from_secs(secs));
                        ctx.toasts.ok("Copied all links.");
                    }
                });
            }
            Modal::Report { lines, .. } => {
                let p = *pal(ui);
                egui::Frame::new().fill(p.surface_2).corner_radius(CornerRadius::same(10)).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                        for l in lines.iter() {
                            bidi::wrapped(ui, l, theme::regular(12.5), p.text_muted);
                        }
                    });
                });
                widgets::modal_footer(ui, |ui| {
                    if widgets::primary(ui, "OK").clicked() {
                        keep = false;
                    }
                });
            }
            Modal::Passphrase { text, retry } => {
                widgets::hint(ui, "They were encrypted by the CLI or another device. Enter their passphrase to download them decrypted.");
                ui.add_space(4.0);
                widgets::field(ui, "Passphrase", |ui| widgets::password_field(ui, text, "Passphrase")).request_focus();
                widgets::modal_footer(ui, |ui| {
                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (widgets::primary(ui, "Continue").clicked() || enter) && !text.is_empty() {
                        session.encryption.set_session_passphrase(secret(text));
                        wipe_string(text);
                        let items = std::mem::take(retry);
                        keep = false;
                        self.retry_encrypted(ctx, session, items);
                    }
                    if widgets::secondary(ui, "Cancel").clicked() {
                        wipe_string(text);
                        keep = false;
                    }
                });
            }
        });
        if dismissed {
            if let Modal::Passphrase { text, .. } = &mut modal {
                wipe_string(text);
            }
            keep = false;
        }
        if keep && self.modal.is_none() {
            self.modal = Some(modal);
        }
    }

    // ----------------------------------------------------------- upload --

    fn upload_ui(&mut self, egui_ctx: &egui::Context, ctx: &AppCtx, session: &Arc<Session>) {
        let Some(mut flow) = self.upload.take() else { return };
        let mut keep = true;
        let prefs = ctx.prefs();
        let encrypt = prefs.encrypt_new_uploads && session.encryption.has_key();
        // Stage transitions driven by finished tasks.
        match &mut flow.stage {
            UploadStage::Checking(t) => {
                if let Some(exists) = t.poll() {
                    for (it, e) in flow.items.iter_mut().zip(exists) {
                        it.exists = Some(e);
                    }
                    flow.stage = next_conflict(&flow.items, 0);
                }
            }
            UploadStage::Conflict { idx, checking, err, rename } => {
                if let Some(exists) = checking.as_ref().and_then(Task::poll) {
                    *checking = None;
                    if exists {
                        // Renaming onto another existing name prompts again.
                        *err = Some(format!("“{rename}” also exists. Choose another name."));
                    } else {
                        let i = *idx;
                        flow.items[i].key = format!("{}{}", flow.prefix, rename.trim());
                        flow.items[i].exists = Some(false);
                        flow.stage = next_conflict(&flow.items, i + 1);
                    }
                }
            }
            UploadStage::Submitting(t) => {
                if let Some(r) = t.poll() {
                    keep = false;
                    if r.plaintext_warning {
                        ctx.toasts.warn("Encryption is on but the key isn't available — files were uploaded as plaintext.");
                    }
                    if !r.failures.is_empty() {
                        ctx.toasts.error(format!("{} file(s) couldn't be queued: {}", r.failures.len(), r.failures[0].1));
                    }
                    if r.enqueued > 0 {
                        let enc = if r.encrypted > 0 { format!(" ({} encrypted)", r.encrypted) } else { String::new() };
                        ctx.toasts.ok(format!("Uploading {} file(s){enc}.", r.enqueued));
                    }
                }
            }
            UploadStage::Review | UploadStage::Ready => {}
        }
        // Transitions chosen inside the modal are applied after it (the
        // stage is mutably borrowed while drawing).
        let mut next: Option<UploadStage> = None;
        let mut resolve: Option<(usize, bool)> = None; // (idx, skip)
        if keep {
            let n = flow.items.len();
            let (title, subtitle, icon, tone, dismissable) = match &flow.stage {
                UploadStage::Review => (format!("Upload {n} file{}", if n == 1 { "" } else { "s" }), format!("To {}", folder_label(&flow.prefix)), icons::UPLOAD, Tone::Primary, true),
                UploadStage::Conflict { idx, .. } => (
                    format!("“{}” already exists", bf_core::media::leaf_name(&flow.items[*idx].key)),
                    format!("In {}", folder_label(&flow.prefix)),
                    icons::WARNING,
                    Tone::Warn,
                    false,
                ),
                _ => (format!("Upload {n} file{}", if n == 1 { "" } else { "s" }), format!("To {}", folder_label(&flow.prefix)), icons::UPLOAD, Tone::Primary, false),
            };
            let mut spec = ModalSpec::new("upload", &title).subtitle(&subtitle).icon(icon, tone).width(580.0);
            if !dismissable {
                spec = spec.locked();
            }
            let dismissed = widgets::modal(egui_ctx, spec, |ui| {
                let p = *pal(ui);
                match &mut flow.stage {
                    UploadStage::Review => {
                        if encrypt {
                            widgets::callout(ui, Tone::Success, icons::LOCK_SIMPLE, "Files are encrypted on this computer before they're uploaded.");
                            ui.add_space(6.0);
                        }
                        egui::Frame::new().stroke(Stroke::new(1.0, p.border)).corner_radius(CornerRadius::same(10)).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            let (hr, _) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::hover());
                            ui.painter().rect_filled(hr, CornerRadius { nw: 10, ne: 10, sw: 0, se: 0 }, p.surface_2.gamma_multiply(0.6));
                            ui.painter().text(egui::pos2(hr.left() + 14.0, hr.center().y), Align2::LEFT_CENTER, "File", theme::medium(12.0), p.text_subtle);
                            ui.painter().text(egui::pos2(hr.right() - 14.0, hr.center().y), Align2::RIGHT_CENTER, "Delete original after upload", theme::medium(12.0), p.text_subtle);
                            egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
                                egui::Frame::new().inner_margin(egui::Margin::symmetric(10, 4)).show(ui, |ui| {
                                    for it in flow.items.iter_mut() {
                                        ui.horizontal(|ui| {
                                            ui.set_min_height(40.0);
                                            widgets::file_tile(ui, &it.key, false, 28.0);
                                            ui.vertical(|ui| {
                                                ui.set_max_width(360.0);
                                                bidi::name(ui, bf_core::media::leaf_name(&it.key), theme::regular(14.0), p.text);
                                                ui.label(RichText::new(format::bytes(it.size)).size(12.0).color(p.text_subtle));
                                            });
                                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                ui.add_space(4.0);
                                                ui.checkbox(&mut it.delete_source, "").on_hover_text("Delete the original once the upload is verified in the bucket");
                                            });
                                        });
                                    }
                                });
                            });
                        });
                        let n_del = flow.items.iter().filter(|i| i.delete_source).count();
                        let total: u64 = flow.items.iter().map(|i| i.size).sum();
                        if n_del > 0 {
                            ui.add_space(6.0);
                            widgets::callout(ui, Tone::Warn, icons::TRASH, &format!("{n_del} original file(s) will be deleted from this computer once each upload is verified in the bucket."));
                        }
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("Total {}", format::bytes(total))).color(p.text_muted));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.spacing_mut().item_spacing.x = 8.0;
                                let label = if n_del > 0 { format!("Upload & delete {n_del}") } else { "Upload".into() };
                                if ui.add(Btn::new(Kind::Primary, &label).icon(icons::UPLOAD)).clicked() {
                                    // Overwrite detection: HEAD every target first.
                                    let (up, keys) = (session.uploads.clone(), flow.items.iter().map(|i| i.key.clone()).collect::<Vec<_>>());
                                    next = Some(UploadStage::Checking(ctx.task(async move {
                                        use futures::StreamExt;
                                        futures::stream::iter(keys)
                                            .map(move |k| {
                                                let up = up.clone();
                                                async move { up.exists(&k).await }
                                            })
                                            .buffered(8)
                                            .collect::<Vec<bool>>()
                                            .await
                                    })));
                                }
                                if widgets::secondary(ui, "Cancel").clicked() {
                                    keep = false;
                                }
                            });
                        });
                    }
                    UploadStage::Checking(_) => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(RichText::new("Checking for existing files…").color(p.text_muted));
                        });
                    }
                    UploadStage::Conflict { idx, rename, checking, err } => {
                        widgets::hint(ui, "Replacing overwrites the object in the bucket. Or give this upload a new name, or skip it.");
                        ui.add_space(6.0);
                        widgets::field(ui, "Upload as", |ui| widgets::text_field(ui, rename, "New name"));
                        if let Some(e) = err {
                            widgets::error_text(ui, e);
                        }
                        let busy = checking.is_some();
                        widgets::modal_footer(ui, |ui| {
                            if widgets::primary_enabled(ui, !busy, "Use new name").clicked() {
                                match validate_name(rename) {
                                    Err(e) => *err = Some(e.into()),
                                    Ok(()) => {
                                        *err = None;
                                        let (up, k) = (session.uploads.clone(), format!("{}{}", flow.prefix, rename.trim()));
                                        *checking = Some(ctx.task(async move { up.exists(&k).await }));
                                    }
                                }
                            }
                            if ui.add(Btn::new(Kind::DangerOutline, "Replace").enabled(!busy)).clicked() {
                                resolve = Some((*idx, false));
                            } else if ui.add(Btn::new(Kind::Ghost, "Skip").enabled(!busy)).clicked() {
                                resolve = Some((*idx, true));
                            }
                            if busy {
                                ui.spinner();
                            }
                        });
                    }
                    UploadStage::Ready | UploadStage::Submitting(_) => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(RichText::new(if encrypt { "Encrypting and queueing…" } else { "Queueing…" }).color(p.text_muted));
                        });
                    }
                }
            });
            if dismissed {
                keep = false;
            }
        }
        if let Some(s) = next {
            flow.stage = s;
        }
        if let Some((i, skip)) = resolve {
            if skip {
                flow.items[i].skip = true;
            } else {
                flow.items[i].exists = Some(false);
            }
            flow.stage = next_conflict(&flow.items, i + 1);
        }
        // All conflicts resolved → submit.
        if keep && matches!(flow.stage, UploadStage::Ready) {
            let reqs: Vec<UploadRequest> = flow
                .items
                .iter()
                .filter(|i| !i.skip)
                .map(|i| UploadRequest { local_path: i.path.clone(), remote_key: i.key.clone(), delete_source: i.delete_source, group_id: None })
                .collect();
            if reqs.is_empty() {
                keep = false;
            } else {
                let up = session.uploads.clone();
                flow.stage = UploadStage::Submitting(ctx.task(async move { up.enqueue(reqs, encrypt).await }));
            }
        }
        if keep {
            self.upload = Some(flow);
        }
    }
}

/// The next unresolved conflict at or after `from`, or `Ready`.
fn next_conflict(items: &[UploadItem], from: usize) -> UploadStage {
    for (i, it) in items.iter().enumerate().skip(from) {
        if it.exists == Some(true) && !it.skip {
            return UploadStage::Conflict { idx: i, rename: suggest_name(bf_core::media::leaf_name(&it.key)), checking: None, err: None };
        }
    }
    // Was `Conflict { idx: usize::MAX }`, which the modal then indexed into
    // `items` before the submit check ran: an out-of-bounds panic (and, with
    // panic = "abort", a crash) on every upload without a name conflict.
    UploadStage::Ready
}

fn suggest_name(name: &str) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 => format!("{} (1){}", &name[..i], &name[i..]),
        _ => format!("{name} (1)"),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_encrypted_downloads(
    s: Arc<Session>,
    items: Vec<(String, PathBuf)>,
    queued_plain: usize,
    mut failed: Vec<(String, String)>,
    done: Arc<AtomicUsize>,
    total: Arc<AtomicUsize>,
    bytes: Arc<AtomicU64>,
    cancel: CancellationToken,
) -> JobDone {
    total.store(items.len(), Ordering::Relaxed);
    let mut ok = 0;
    let mut iter = items.into_iter().enumerate().peekable();
    while let Some((i, (k, dest))) = iter.next() {
        if cancel.is_cancelled() {
            break;
        }
        let parent = dest.parent().map(Path::to_path_buf).unwrap_or_default();
        let _ = tokio::fs::create_dir_all(&parent).await;
        let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
        let target = unique_path(&parent, &name);
        let b = bytes.clone();
        let progress: bf_app::transfers::uploader::ProgressFn = Arc::new(move |n| b.store(n, Ordering::Relaxed));
        match s.encryption.download_decrypted(&k, &target, Some(progress), &cancel).await {
            Ok(()) => ok += 1,
            Err(AppFailure::ExternalKey { .. }) => {
                // Prompt once, then continue with this and the rest.
                let mut remaining = vec![(k, dest)];
                remaining.extend(iter.map(|(_, x)| x));
                return JobDone::Downloads { done: ok + queued_plain, failed, remaining, needs_passphrase: true };
            }
            Err(e) if e.is_cancelled() => break,
            Err(e) => failed.push((k, message_for(&e))),
        }
        done.store(i + 1, Ordering::Relaxed);
    }
    JobDone::Downloads { done: ok + queued_plain, failed, remaining: vec![], needs_passphrase: false }
}
