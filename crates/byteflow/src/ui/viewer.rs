//! Built-in viewers (spec §10.1): photos, text/code/CSV, video and audio, in
//! a full-window overlay.
//!
//! * Photos and text: plaintext objects stream from R2 into memory (no disk
//!   cache, no temp file); a decoded photo also seeds the thumbnail.
//! * Video and audio **stream** in-app through the OS media stack
//!   (`platform::media`, Media Foundation on Windows): the player pulls
//!   ranged reads from the bucket (`bf_app::stream`), decrypting encrypted
//!   objects segment by segment in memory, so playback starts after the first
//!   blocks arrive and nothing is written to disk. Only "Open in default app"
//!   (or a platform without built-in playback) makes a private temp copy,
//!   deleted when the viewer closes (and on next launch / wipe).
//! * Encrypted objects go through `decrypt_to_temp` (header-first key
//!   resolution, passphrase retry); for photos/text the plaintext temp is
//!   deleted as soon as it is in memory.
//! * Other types have no built-in viewer: nothing is fetched unless the user
//!   asks to open it in the default app.
//! * Errors are static copy; raw error text (which can embed a URL) never
//!   surfaces.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, ColorImage, CornerRadius, RichText, Sense, Stroke, TextureHandle, TextureOptions, Vec2, vec2};
use tokio::io::AsyncWriteExt;

use bf_app::session::Session;
use bf_app::stream::MediaStream;
use bf_core::media::{MediaKind, media_kind_for_key};
use bf_core::{AppFailure, AppResult};

use crate::bidi;
use crate::ctx::{AppCtx, Task};
use crate::platform;
use crate::platform::media::{ByteSource, MediaPlayer, Source};
use crate::theme::{self, pal};
use crate::ui::icons;
use crate::ui::widgets::{self, Kind, Tone};

pub const TEXT_PREVIEW_MAX: u64 = 2 * 1024 * 1024;
const IMAGE_MEMORY_MAX: u64 = 96 * 1024 * 1024;
const IMAGE_MAX_EDGE: u32 = 4096;
const WRAP_MAX_LINES: usize = 4000;
const CSV_MAX_ROWS: usize = 5000;
const CSV_MAX_COLS: usize = 64;

// ------------------------------------------------------------- loading --

enum Loaded {
    Image(ColorImage, Arc<image::DynamicImage>),
    Text(String, bool),
    Stream(Arc<MediaStream>),
    External(PathBuf),
}

/// A bucket object as the player's byte source.
pub struct StreamSource(pub Arc<MediaStream>);

impl ByteSource for StreamSource {
    fn len(&self) -> u64 {
        self.0.len()
    }
    fn read_at(&self, off: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read_at(off, buf).map_err(|e| std::io::Error::other(e.developer_message()))
    }
    fn cancel(&self) {
        self.0.cancel();
    }
}

/// Open a streamed player for `stream` (Windows; `None` elsewhere).
pub fn stream_player(stream: Arc<MediaStream>, key: &str, repaint: egui::Context) -> Option<MediaPlayer> {
    let name = sanitize(bf_core::media::leaf_name(key));
    MediaPlayer::open_source(Source::Stream { source: Arc::new(StreamSource(stream)), name }, repaint, true)
}

pub fn decode_text(b: &[u8]) -> String {
    if b.starts_with(&[0xFF, 0xFE]) || b.starts_with(&[0xFE, 0xFF]) {
        let le = b[0] == 0xFF;
        let units: Vec<u16> = b[2..].chunks_exact(2).map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) }).collect();
        return String::from_utf16_lossy(&units);
    }
    let b = b.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(b);
    String::from_utf8_lossy(b).into_owned()
}

pub fn decode_image(bytes: &[u8], max_edge: u32) -> AppResult<(ColorImage, Arc<image::DynamicImage>)> {
    let mut r = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().map_err(|_| AppFailure::unknown("image"))?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(512 * 1024 * 1024);
    r.limits(limits);
    let img = r.decode().map_err(|_| AppFailure::unknown("image decode"))?;
    let img = if img.width() > max_edge || img.height() > max_edge { img.thumbnail(max_edge, max_edge) } else { img };
    let rgba = img.to_rgba8();
    let ci = ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw());
    Ok((ci, Arc::new(img)))
}

pub async fn stream_to_vec(session: &Session, key: &str, max: u64, bytes: &AtomicU64) -> AppResult<Vec<u8>> {
    let mut resp = session.client.get().get_stream(key, 0, Some(&session.cancel)).await?;
    let mut out = Vec::with_capacity(resp.content_length().unwrap_or(0).min(max) as usize);
    while let Some(c) = resp.chunk().await.map_err(|e| bf_data::r2::failure::from_transport(&e))? {
        out.extend_from_slice(&c);
        bytes.store(out.len() as u64, Ordering::Relaxed);
        if out.len() as u64 > max {
            return Err(AppFailure::StorageFull { message: "too large to preview".into() });
        }
    }
    Ok(out)
}

/// Decrypt to the private temp, read up to `max` bytes into memory, delete.
pub async fn decrypt_to_memory(session: &Session, key: &str, max: u64, bytes: &Arc<AtomicU64>) -> AppResult<(Vec<u8>, bool)> {
    let b = bytes.clone();
    let progress: bf_app::transfers::uploader::ProgressFn = Arc::new(move |n| b.store(n, Ordering::Relaxed));
    let path = session.encryption.decrypt_to_temp(key, Some(progress)).await?;
    let p2 = path.clone();
    let r = bf_app::blocking(move || {
        use std::io::Read;
        let mut f = std::fs::File::open(&p2).map_err(|e| AppFailure::io("open", e))?;
        let mut buf = vec![];
        f.by_ref().take(max).read_to_end(&mut buf).map_err(|e| AppFailure::io("read", e))?;
        let more = f.read(&mut [0u8; 1]).map(|n| n > 0).unwrap_or(false);
        Ok((buf, more))
    })
    .await;
    // Plaintext leaves the disk as soon as it is in memory.
    let _ = tokio::fs::remove_file(&path).await;
    r
}

async fn download_to_temp(session: &Session, key: &str, temp_dir: PathBuf, bytes: &AtomicU64) -> AppResult<PathBuf> {
    tokio::fs::create_dir_all(&temp_dir).await.map_err(|e| AppFailure::io("temp", e))?;
    let out = temp_dir.join(format!("{}-{}", bf_app::new_id(), sanitize(bf_core::media::leaf_name(key))));
    let mut resp = session.client.get().get_stream(key, 0, Some(&session.cancel)).await?;
    let f = tokio::fs::File::create(&out).await.map_err(|e| AppFailure::io("temp", e))?;
    let mut w = tokio::io::BufWriter::with_capacity(1 << 20, f);
    let mut n = 0u64;
    let res: AppResult<()> = async {
        while let Some(c) = resp.chunk().await.map_err(|e| bf_data::r2::failure::from_transport(&e))? {
            w.write_all(&c).await.map_err(|e| AppFailure::io("temp", e))?;
            n += c.len() as u64;
            bytes.store(n, Ordering::Relaxed);
        }
        w.flush().await.map_err(|e| AppFailure::io("temp", e))
    }
    .await;
    if let Err(e) = res {
        drop(w);
        let _ = tokio::fs::remove_file(&out).await;
        return Err(e);
    }
    Ok(out)
}

async fn load(session: Arc<Session>, key: String, encrypted: bool, temp_dir: PathBuf, bytes: Arc<AtomicU64>, external: bool) -> AppResult<Loaded> {
    let kind = media_kind_for_key(&key);
    let progress: bf_app::transfers::uploader::ProgressFn = {
        let b = bytes.clone();
        Arc::new(move |n| b.store(n, Ordering::Relaxed))
    };
    let to_temp = async {
        if encrypted {
            session.encryption.decrypt_to_temp(&key, Some(progress.clone())).await
        } else {
            download_to_temp(&session, &key, temp_dir.clone(), &bytes).await
        }
    };
    if external {
        return Ok(Loaded::External(to_temp.await?));
    }
    match (kind, encrypted) {
        (MediaKind::Image, false) => {
            let data = stream_to_vec(&session, &key, IMAGE_MEMORY_MAX, &bytes).await?;
            let (ci, img) = bf_app::blocking(move || decode_image(&data, IMAGE_MAX_EDGE)).await?;
            Ok(Loaded::Image(ci, img))
        }
        (MediaKind::Text, false) => {
            let data = session.client.get().get_range(&key, 0, TEXT_PREVIEW_MAX).await?;
            let truncated = data.len() as u64 >= TEXT_PREVIEW_MAX;
            Ok(Loaded::Text(decode_text(&data), truncated))
        }
        (MediaKind::Text, true) => {
            let (buf, more) = decrypt_to_memory(&session, &key, TEXT_PREVIEW_MAX, &bytes).await?;
            Ok(Loaded::Text(decode_text(&buf), more))
        }
        (MediaKind::Image, true) => {
            let (buf, _) = decrypt_to_memory(&session, &key, IMAGE_MEMORY_MAX, &bytes).await?;
            // Encrypted objects never get a thumbnail (decode only).
            let (ci, img) = bf_app::blocking(move || decode_image(&buf, IMAGE_MAX_EDGE)).await?;
            Ok(Loaded::Image(ci, img))
        }
        (MediaKind::Video | MediaKind::Audio, _) if MediaPlayer::supported() => Ok(Loaded::Stream(MediaStream::open(&session, &key).await?)),
        (MediaKind::Video | MediaKind::Audio, _) => Ok(Loaded::External(to_temp.await?)),
        (MediaKind::None, _) => Ok(Loaded::External(to_temp.await?)),
    }
}

fn sanitize(leaf: &str) -> String {
    leaf.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { '_' } else { c }).take(120).collect()
}

pub fn fmt_time(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "--:--".into();
    }
    let s = secs as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

// --------------------------------------------------------------- state --

struct TextDoc {
    lines: Vec<String>,
    /// Lines holding right-to-left text, in visual order (see `crate::bidi`).
    rtl: HashMap<usize, String>,
    /// Those lines broken for the wrap width they were last shown at.
    rtl_wrapped: RefCell<(f32, HashMap<usize, String>)>,
    truncated: bool,
    gutter: f32,
    csv: Option<Vec<Vec<String>>>,
    col_w: Vec<f32>,
}

enum State {
    Loading(Task<AppResult<Loaded>>, bool),
    Image(TextureHandle),
    Text(TextDoc),
    Media,
    External(PathBuf),
    Unsupported,
    NeedsPassphrase(bool),
    Error(String),
}

/// What the viewer asks the files page to do.
pub enum ViewerAction {
    Download(String),
    Share(String),
}

pub struct Viewer {
    pub key: String,
    encrypted: bool,
    size: u64,
    kind: MediaKind,
    state: State,
    bytes: Arc<AtomicU64>,
    temp: Option<PathBuf>,
    passphrase: String,
    pub open: bool,
    pub action: Option<ViewerAction>,
    // photo
    zoom: Option<f32>,
    pan: Vec2,
    rotation: u8,
    // text
    wrap: bool,
    table: bool,
    // media
    player: Option<MediaPlayer>,
    video_tex: Option<TextureHandle>,
    volume: f32,
    muted: bool,
    scrub: Option<f64>,
    last_seek: Option<Instant>,
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.cleanup();
    }
}

impl Viewer {
    pub fn open(ctx: &AppCtx, session: &Arc<Session>, key: &str, size: u64) -> Self {
        let kind = media_kind_for_key(key);
        let mut v = Self {
            key: key.to_string(),
            encrypted: session.encryption.is_encrypted(key),
            size,
            kind,
            state: State::Unsupported,
            bytes: Arc::new(AtomicU64::new(0)),
            temp: None,
            passphrase: String::new(),
            open: true,
            action: None,
            zoom: None,
            pan: Vec2::ZERO,
            rotation: 0,
            wrap: matches!(bf_core::media::extension(key).as_str(), "txt" | "md" | "log"),
            table: true,
            player: None,
            video_tex: None,
            volume: 1.0,
            muted: false,
            scrub: None,
            last_seek: None,
        };
        if kind != MediaKind::None {
            v.start(ctx, session, false);
        }
        v
    }

    /// dev-env builds only: show a local file (never deleted) in the viewer,
    /// for UI work without a bucket.
    #[cfg(feature = "dev-env")]
    pub fn open_local(egui_ctx: &egui::Context, path: &std::path::Path) -> Self {
        let key = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let kind = media_kind_for_key(&key);
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let mut v = Self {
            key: key.clone(),
            encrypted: false,
            size,
            kind,
            state: State::Unsupported,
            bytes: Arc::new(AtomicU64::new(0)),
            temp: None,
            passphrase: String::new(),
            open: true,
            action: None,
            zoom: None,
            pan: Vec2::ZERO,
            rotation: 0,
            wrap: matches!(bf_core::media::extension(&key).as_str(), "txt" | "md" | "log"),
            table: true,
            player: None,
            video_tex: None,
            volume: 1.0,
            muted: false,
            scrub: None,
            last_seek: None,
        };
        let bytes = std::fs::read(path).unwrap_or_default();
        v.state = match kind {
            MediaKind::Image => match decode_image(&bytes, IMAGE_MAX_EDGE) {
                Ok((ci, _)) => State::Image(egui_ctx.load_texture("viewer:dev", ci, TextureOptions::LINEAR)),
                Err(_) => State::Error("This file couldn't be opened.".into()),
            },
            MediaKind::Text => {
                let truncated = bytes.len() as u64 > TEXT_PREVIEW_MAX;
                let s = decode_text(&bytes[..bytes.len().min(TEXT_PREVIEW_MAX as usize)]);
                State::Text(text_doc(egui_ctx, &key, s, truncated))
            }
            MediaKind::Video | MediaKind::Audio => match MediaPlayer::open(path, egui_ctx.clone()) {
                Some(p) => {
                    v.player = Some(p);
                    State::Media
                }
                None => State::Unsupported,
            },
            MediaKind::None => State::Unsupported,
        };
        v
    }

    fn start(&mut self, ctx: &AppCtx, session: &Arc<Session>, external: bool) {
        self.bytes.store(0, Ordering::Relaxed);
        let temp = ctx.core.paths.temp_sub("preview");
        self.state = State::Loading(ctx.task(load(session.clone(), self.key.clone(), self.encrypted, temp, self.bytes.clone(), external)), external);
    }

    /// Stop playback (closing the file) and delete any plaintext temp copy.
    pub fn cleanup(&mut self) {
        self.player = None;
        if let Some(t) = self.temp.take() {
            let _ = std::fs::remove_file(t);
        }
    }

    fn open_external(&mut self, ctx: &AppCtx, session: &Arc<Session>) {
        match &self.temp {
            Some(p) => {
                if !platform::open_with_default_app(p) {
                    ctx.toasts.warn("No app is registered to open this file type.");
                }
            }
            None => self.start(ctx, session, true),
        }
    }

    fn finish_load(&mut self, egui_ctx: &egui::Context, ctx: &AppCtx, session: &Arc<Session>, r: AppResult<Loaded>, external: bool) {
        self.state = match r {
            Ok(Loaded::Image(ci, img)) => {
                if !self.encrypted {
                    session.thumbs.generate_from_image(&self.key, img);
                }
                State::Image(egui_ctx.load_texture(format!("viewer:{}", self.key), ci, TextureOptions::LINEAR))
            }
            Ok(Loaded::Text(s, t)) => State::Text(text_doc(egui_ctx, &self.key, s, t)),
            Ok(Loaded::Stream(s)) => match stream_player(s, &self.key, egui_ctx.clone()) {
                Some(pl) => {
                    pl.set_volume(self.volume as f64);
                    self.player = Some(pl);
                    State::Media
                }
                None => State::Error("This file couldn't be played.".into()),
            },
            Ok(Loaded::External(p)) => {
                self.temp = Some(p.clone());
                if !platform::open_with_default_app(&p) {
                    ctx.toasts.warn("No app is registered to open this file type.");
                }
                State::External(p)
            }
            Err(AppFailure::ExternalKey { .. }) => State::NeedsPassphrase(external),
            Err(AppFailure::Crypto { message }) if message.contains("legacy format") => State::Error(message),
            Err(AppFailure::StorageFull { .. }) => State::Error("This file is too large to preview here. Download it instead.".into()),
            Err(e) if e.is_cancelled() => State::Error("Cancelled.".into()),
            Err(_) => State::Error("This file couldn't be opened.".into()),
        };
    }

    pub fn ui(&mut self, egui_ctx: &egui::Context, ctx: &AppCtx, session: &Arc<Session>) {
        if let State::Loading(t, ext) = &self.state {
            let ext = *ext;
            if let Some(r) = t.poll() {
                self.finish_load(egui_ctx, ctx, session, r, ext);
            }
        }
        if let Some(pl) = &self.player
            && let Some(img) = pl.take_frame()
        {
            match &mut self.video_tex {
                Some(t) if t.size() == img.size => t.set(img, TextureOptions::LINEAR),
                _ => self.video_tex = Some(egui_ctx.load_texture("viewer:video", img, TextureOptions::LINEAR)),
            }
        }

        let p = *theme::pal_ctx(egui_ctx);
        let screen = egui_ctx.content_rect();
        let size = vec2((screen.width() - 56.0).max(520.0), (screen.height() - 56.0).max(360.0));
        let frame = egui::Frame::new()
            .fill(p.surface)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(16))
            .shadow(egui::Shadow { offset: [0, 18], blur: 50, spread: 0, color: Color32::from_black_alpha(if p.dark { 170 } else { 60 }) });
        let mut close = false;
        let resp = egui::Modal::new(egui::Id::unique("viewer")).frame(frame).backdrop_color(Color32::from_black_alpha(if p.dark { 170 } else { 120 })).show(egui_ctx, |ui| {
            ui.set_min_size(size);
            ui.set_max_size(size);
            ui.spacing_mut().item_spacing = vec2(8.0, 0.0);
            close = self.header(ui, ctx, session);
            let body = ui.available_rect_before_wrap();
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(body).layout(egui::Layout::top_down(egui::Align::Min)));
            child.set_clip_rect(body);
            self.body(&mut child, egui_ctx, ctx, session, body);
            ui.allocate_rect(body, Sense::hover());
        });
        if close || resp.should_close() {
            self.open = false;
            self.cleanup();
        }
    }

    fn header(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, session: &Arc<Session>) -> bool {
        let p = *pal(ui);
        let mut close = false;
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 64.0), Sense::hover());
        ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, p.border));
        let mut h = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(18.0, 0.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
        widgets::file_tile(&mut h, &self.key, false, 36.0);
        h.add_space(4.0);
        let name = bf_core::media::leaf_name(&self.key).to_string();
        let actions_w = 260.0;
        h.vertical(|ui| {
            ui.set_max_width((rect.width() - actions_w - 120.0).max(120.0));
            ui.add_space(12.0);
            bidi::name(ui, &name, theme::semibold(15.0), p.text);
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let meta = format!("{} · {}", widgets::kind_label(&self.key, false), bf_core::format::bytes(self.size));
                ui.label(RichText::new(meta).size(12.5).color(p.text_muted));
                if self.encrypted {
                    widgets::badge_icon(ui, icons::LOCK_SIMPLE, "Encrypted", Tone::Success);
                }
            });
        });
        h.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if widgets::icon_btn(ui, icons::X, "Close (Esc)").clicked() {
                close = true;
            }
            ui.add_space(6.0);
            let (r, _) = ui.allocate_exact_size(vec2(1.0, 22.0), Sense::hover());
            ui.painter().vline(r.center().x, r.y_range(), Stroke::new(1.0, p.border));
            ui.add_space(6.0);
            if widgets::icon_btn(ui, icons::DOWNLOAD, "Download").clicked() {
                self.action = Some(ViewerAction::Download(self.key.clone()));
            }
            if widgets::icon_btn(ui, icons::LINK_SIMPLE, "Share link").clicked() {
                self.action = Some(ViewerAction::Share(self.key.clone()));
            }
            if !matches!(self.state, State::Loading(..)) && widgets::icon_btn(ui, icons::ARROW_SQUARE_OUT, "Open in default app").clicked() {
                self.player = None; // release the file first
                self.open_external(ctx, session);
            }
            match &self.state {
                State::Image(_) => self.image_tools(ui),
                State::Text(doc) => {
                    let csv = doc.csv.is_some();
                    let can_wrap = doc.lines.len() <= WRAP_MAX_LINES;
                    ui.add_space(6.0);
                    if csv {
                        let mut t = self.table;
                        widgets::segmented(ui, &mut t, &[(false, icons::TEXT_ALIGN_LEFT, "Raw"), (true, icons::TABLE, "Table")]);
                        self.table = t;
                    }
                    if !(csv && self.table) {
                        let tip = if can_wrap { "Wrap long lines" } else { "Too many lines to wrap" };
                        if widgets::icon_btn_ex(ui, icons::TEXT_ALIGN_LEFT, tip, 32.0, self.wrap && can_wrap, can_wrap).clicked() {
                            self.wrap = !self.wrap;
                        }
                    }
                }
                _ => {}
            }
        });
        close
    }

    fn image_tools(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        if widgets::icon_btn(ui, icons::ARROW_CLOCKWISE, "Rotate").clicked() {
            self.rotation = (self.rotation + 1) % 4;
            self.pan = Vec2::ZERO;
        }
        if widgets::icon_btn_ex(ui, icons::CORNERS_IN, "Fit to window", 32.0, self.zoom.is_none(), true).clicked() {
            self.zoom = None;
            self.pan = Vec2::ZERO;
        }
        if widgets::icon_btn(ui, icons::MAGNIFYING_GLASS_PLUS, "Zoom in").clicked() {
            self.zoom = Some((self.zoom.unwrap_or(1.0) * 1.25).min(16.0));
        }
        let pct = self.zoom.map(|z| format!("{:.0}%", z * 100.0)).unwrap_or_else(|| "Fit".into());
        if ui.add(widgets::Btn::new(Kind::Ghost, &pct).small().min_width(58.0)).on_hover_text("Actual size").clicked() {
            self.zoom = Some(1.0);
            self.pan = Vec2::ZERO;
        }
        if widgets::icon_btn(ui, icons::MAGNIFYING_GLASS_MINUS, "Zoom out").clicked() {
            self.zoom = Some((self.zoom.unwrap_or(1.0) / 1.25).max(0.02));
        }
    }

    fn body(&mut self, ui: &mut egui::Ui, egui_ctx: &egui::Context, ctx: &AppCtx, session: &Arc<Session>, rect: egui::Rect) {
        let p = *pal(ui);
        let stage = matches!(self.state, State::Image(_)) || (matches!(self.state, State::Media) && self.kind == MediaKind::Video);
        if stage {
            ui.painter().rect_filled(rect, CornerRadius { nw: 0, ne: 0, sw: 16, se: 16 }, p.stage);
        }
        match &mut self.state {
            State::Loading(_, ext) => {
                let done = self.bytes.load(Ordering::Relaxed);
                let streams = matches!(self.kind, MediaKind::Video | MediaKind::Audio) && MediaPlayer::supported();
                let label = if *ext {
                    "Preparing a private copy…"
                } else if streams {
                    "Connecting…"
                } else if self.encrypted {
                    "Downloading and decrypting…"
                } else {
                    "Loading…"
                };
                centered(ui, rect, 340.0, |ui| {
                    ui.spinner();
                    ui.add_space(10.0);
                    ui.label(RichText::new(label).color(p.text_muted));
                    if self.size > 0 && done > 0 {
                        ui.add_space(8.0);
                        widgets::progress(ui, done as f32 / self.size as f32, format!("{} of {}", bf_core::format::bytes(done), bf_core::format::bytes(self.size)));
                    }
                });
                egui_ctx.request_repaint_after(Duration::from_millis(150));
            }
            State::Image(t) => {
                let t = t.clone();
                self.photo(ui, rect, &t);
            }
            State::Text(_) => self.text(ui, rect),
            State::Media => self.media(ui, egui_ctx, rect),
            State::External(path) => {
                let path = path.clone();
                centered(ui, rect, 380.0, |ui| {
                    widgets::icon_tile(ui, icons::ARROW_SQUARE_OUT, Tone::Primary, 56.0);
                    ui.add_space(12.0);
                    ui.label(RichText::new("Opened in your default app").font(theme::semibold(16.0)).color(p.text));
                    ui.add_space(4.0);
                    widgets::hint(ui, "A private temporary copy is used and deleted when you close this viewer.");
                    ui.add_space(12.0);
                    if widgets::btn(ui, Kind::Secondary, icons::ARROW_SQUARE_OUT, "Open again").clicked() {
                        platform::open_with_default_app(&path);
                    }
                });
            }
            State::Unsupported => {
                let mut open = false;
                let mut dl = false;
                centered(ui, rect, 400.0, |ui| {
                    widgets::file_tile(ui, &self.key, false, 64.0);
                    ui.add_space(12.0);
                    ui.label(RichText::new("No built-in viewer for this file type").font(theme::semibold(16.0)).color(p.text));
                    ui.add_space(4.0);
                    widgets::hint(ui, "Open it in the app registered for this type, or download it. Nothing is fetched until you choose.");
                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        let w = 2.0 * 150.0 + 8.0;
                        ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                        open = ui.add(widgets::Btn::new(Kind::Primary, "Open in default app").icon(icons::ARROW_SQUARE_OUT).min_width(150.0)).clicked();
                        dl = ui.add(widgets::Btn::new(Kind::Secondary, "Download").icon(icons::DOWNLOAD).min_width(150.0)).clicked();
                    });
                });
                if open {
                    self.start(ctx, session, true);
                }
                if dl {
                    self.action = Some(ViewerAction::Download(self.key.clone()));
                }
            }
            State::NeedsPassphrase(ext) => {
                let ext = *ext;
                let mut go = false;
                centered(ui, rect, 380.0, |ui| {
                    widgets::icon_tile(ui, icons::KEY, Tone::Warn, 56.0);
                    ui.add_space(12.0);
                    ui.label(RichText::new("Passphrase needed").font(theme::semibold(16.0)).color(p.text));
                    ui.add_space(4.0);
                    widgets::hint(ui, "This file was encrypted outside this vault (CLI or another device). Enter its passphrase to open it.");
                    ui.add_space(12.0);
                    let r = widgets::password_field(ui, &mut self.passphrase, "Passphrase");
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.add_space(8.0);
                    go = (ui.add(widgets::Btn::new(Kind::Primary, "Open").icon(icons::LOCK_OPEN).min_width(ui.available_width())).clicked() || enter)
                        && !self.passphrase.is_empty();
                });
                if go {
                    session.encryption.set_session_passphrase(crate::ctx::secret(&self.passphrase));
                    crate::ctx::wipe_string(&mut self.passphrase);
                    self.start(ctx, session, ext);
                }
            }
            State::Error(e) => {
                let e = e.clone();
                centered(ui, rect, 380.0, |ui| {
                    widgets::icon_tile(ui, icons::WARNING_CIRCLE, Tone::Danger, 56.0);
                    ui.add_space(12.0);
                    ui.label(RichText::new("Can't open this file").font(theme::semibold(16.0)).color(p.text));
                    ui.add_space(4.0);
                    widgets::hint(ui, &e);
                });
            }
        }
    }

    // --------------------------------------------------------------- photo --

    fn photo(&mut self, ui: &mut egui::Ui, rect: egui::Rect, tex: &TextureHandle) {
        let stage = rect.shrink(16.0);
        let resp = ui.interact(rect, ui.scope_id().with("photo-stage"), Sense::click_and_drag());
        let mut ts = tex.size_vec2();
        if self.rotation % 2 == 1 {
            ts = vec2(ts.y, ts.x);
        }
        let fit = (stage.width() / ts.x).min(stage.height() / ts.y).min(1.0);
        let mut zoom = self.zoom.unwrap_or(fit);
        if resp.hovered() {
            let (scroll, pinch, pointer) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta(), i.pointer.hover_pos()));
            let factor = if (pinch - 1.0).abs() > 1e-4 { pinch } else { (scroll * 0.0018).exp() };
            if (factor - 1.0).abs() > 1e-4 {
                let new = (zoom * factor).clamp(fit.min(0.02), 16.0);
                // Zoom around the pointer.
                if let Some(ptr) = pointer {
                    let c = stage.center() + self.pan;
                    let d = ptr - c;
                    self.pan += d - d * (new / zoom);
                }
                zoom = new;
                self.zoom = Some(zoom);
            }
        }
        if resp.dragged() {
            self.pan += resp.drag_delta();
            if self.zoom.is_none() {
                self.zoom = Some(zoom);
            }
        }
        if resp.double_clicked() {
            if self.zoom.is_some_and(|z| (z - fit).abs() > 1e-3) {
                self.zoom = None;
                self.pan = Vec2::ZERO;
            } else {
                self.zoom = Some(1.0);
            }
        }
        let shown = ts * zoom;
        // Keep the photo on screen.
        let lim = ((shown - stage.size()) / 2.0).max(Vec2::ZERO);
        self.pan = self.pan.clamp(-lim, lim);
        if resp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if resp.hovered() && (shown.x > stage.width() || shown.y > stage.height()) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
        let draw_size = if self.rotation % 2 == 1 { vec2(shown.y, shown.x) } else { shown };
        let r = egui::Rect::from_center_size(stage.center() + self.pan, draw_size);
        let img = egui::Image::new(tex).rotate(self.rotation as f32 * std::f32::consts::FRAC_PI_2, Vec2::splat(0.5));
        let painter_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect));
        let mut painter_ui = painter_ui;
        painter_ui.set_clip_rect(rect);
        img.paint_at(&painter_ui, r);
        // Dimensions chip.
        let p = *pal(ui);
        let [w, h] = tex.size();
        let chip = format!("{w} × {h}");
        let g = ui.painter().layout_no_wrap(chip, theme::medium(12.0), Color32::WHITE);
        let cr = egui::Rect::from_min_size(egui::pos2(rect.left() + 16.0, rect.bottom() - 16.0 - 24.0), vec2(g.size().x + 16.0, 24.0));
        ui.painter().rect_filled(cr, CornerRadius::same(12), Color32::from_black_alpha(140));
        ui.painter().galley(cr.center() - g.size() / 2.0, g, Color32::WHITE);
        let _ = p;
    }

    // ---------------------------------------------------------------- text --

    fn text(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        let State::Text(doc) = &self.state else { return };
        let p = *pal(ui);
        let inner = rect.shrink2(vec2(0.0, 0.0));
        let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::top_down(egui::Align::Min)));
        if doc.truncated {
            egui::Frame::new().inner_margin(egui::Margin::symmetric(18, 8)).fill(p.warn_soft).show(&mut ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(format!("{}  Showing the first 2 MiB. Download the file to see all of it.", icons::INFO)).size(12.5).color(p.warn));
            });
        }
        if let (Some(rows), true) = (&doc.csv, self.table) {
            csv_table(&mut ui, rows, &doc.col_w);
            return;
        }
        let mono = theme::mono(13.0);
        let row_h = ui.fonts_mut(|f| f.row_height(&mono)) + 2.0;
        let gutter = doc.gutter;
        let wrap = self.wrap && doc.lines.len() <= WRAP_MAX_LINES;
        let lines = &doc.lines;
        let paint_gutter = |ui: &mut egui::Ui, n: usize| {
            ui.add_sized([gutter, row_h], egui::Label::new(RichText::new(n.to_string()).font(mono.clone()).color(p.text_subtle)).halign(egui::Align::Max).selectable(false));
            ui.add_space(14.0);
        };
        egui::Frame::new().inner_margin(egui::Margin { left: 8, right: 18, top: 10, bottom: 10 }).show(&mut ui, |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            // Rows are exactly one text line tall (show_rows relies on it).
            ui.spacing_mut().interact_size.y = row_h;
            if wrap {
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    for (i, l) in lines.iter().enumerate() {
                        ui.horizontal_top(|ui| {
                            paint_gutter(ui, i + 1);
                            if doc.rtl.contains_key(&i) {
                                // Break the logical line, then reorder each piece.
                                let w = ui.available_width();
                                let mut cache = doc.rtl_wrapped.borrow_mut();
                                if cache.0 != w {
                                    *cache = (w, HashMap::new());
                                }
                                let shown = cache.1.entry(i).or_insert_with(|| bidi::wrap(ui, l, &mono, w, true).into_owned());
                                ui.add(egui::Label::new(RichText::new(shown.as_str()).font(mono.clone()).color(p.text)).wrap());
                            } else {
                                ui.add(egui::Label::new(RichText::new(l).font(mono.clone()).color(p.text)).wrap());
                            }
                        });
                    }
                });
            } else {
                egui::ScrollArea::both().auto_shrink(false).show_rows(ui, row_h, lines.len(), |ui, range| {
                    for i in range {
                        ui.horizontal(|ui| {
                            paint_gutter(ui, i + 1);
                            let line = doc.rtl.get(&i).unwrap_or(&lines[i]);
                            ui.add(egui::Label::new(RichText::new(line).font(mono.clone()).color(p.text)).extend());
                        });
                    }
                });
            }
        });
    }

    // --------------------------------------------------------------- media --

    fn media(&mut self, ui: &mut egui::Ui, egui_ctx: &egui::Context, rect: egui::Rect) {
        let Some(pl) = self.player.as_mut() else { return };
        let p = *pal(ui);
        let st = pl.status();
        let video = self.kind == MediaKind::Video;
        let controls_h = 64.0;
        let stage = egui::Rect::from_min_max(rect.min, egui::pos2(rect.right(), rect.bottom() - controls_h));
        let bar = egui::Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - controls_h), rect.max);

        // Keyboard shortcuts (no text field has focus inside the viewer).
        let keys = ui.input(|i| {
            (
                i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::M),
            )
        });
        let toggle = |pl: &MediaPlayer, st: &platform::media::Status| {
            if st.ended {
                pl.seek(0.0);
                pl.play();
            } else if st.paused {
                pl.play();
            } else {
                pl.pause();
            }
        };
        if !ui.ctx().egui_wants_keyboard_input() {
            if keys.0 {
                toggle(pl, &st);
            }
            if keys.1 || keys.2 {
                let d = if keys.1 { -5.0 } else { 5.0 };
                let t = (st.position + d).clamp(0.0, if st.duration.is_finite() { st.duration } else { f64::MAX });
                pl.seek(t);
            }
            if keys.3 || keys.4 {
                self.volume = (self.volume + if keys.3 { 0.1 } else { -0.1 }).clamp(0.0, 1.0);
                pl.set_volume(self.volume as f64);
            }
            if keys.5 {
                self.muted = !self.muted;
                pl.set_muted(self.muted);
            }
        }

        // Stage.
        if let Some(err) = &st.error {
            let err = err.clone();
            centered(ui, stage, 400.0, |ui| {
                widgets::icon_tile(ui, icons::WARNING_CIRCLE, Tone::Danger, 56.0);
                ui.add_space(12.0);
                ui.label(RichText::new("Can't play this file").font(theme::semibold(16.0)).color(if video { Color32::WHITE } else { p.text }));
                ui.add_space(4.0);
                ui.add(egui::Label::new(RichText::new(err).size(13.0).color(if video { Color32::from_gray(190) } else { p.text_muted })).wrap());
                ui.add_space(4.0);
                widgets::hint(ui, "Use “Open in default app” in the toolbar instead.");
            });
        } else if video {
            let ppp = ui.ctx().pixels_per_point();
            let area = stage.shrink(12.0);
            let [vw, vh] = st.video_size;
            if vw > 0 && vh > 0 {
                let aspect = vw as f32 / vh as f32;
                let mut sz = vec2(area.width(), area.width() / aspect);
                if sz.y > area.height() {
                    sz = vec2(area.height() * aspect, area.height());
                }
                let r = egui::Rect::from_center_size(area.center(), sz);
                pl.set_viewport((sz.x * ppp) as u32, (sz.y * ppp) as u32);
                if let Some(t) = &self.video_tex {
                    ui.painter().image(t.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                }
                let click = ui.interact(r, ui.scope_id().with("video-click"), Sense::click());
                if click.clicked() {
                    toggle(pl, &st);
                }
                if st.buffering && !st.paused {
                    buffering_badge(ui, r.center());
                    egui_ctx.request_repaint_after(Duration::from_millis(100));
                } else if st.paused && !st.ended && st.ready {
                    ui.painter().circle_filled(r.center(), 34.0, Color32::from_black_alpha(150));
                    ui.painter().text(r.center() + vec2(2.0, 0.0), Align2::CENTER_CENTER, icons::PLAY, theme::regular(30.0), Color32::WHITE);
                }
            } else {
                centered(ui, stage, 200.0, |ui| {
                    ui.spinner();
                });
            }
        } else {
            centered(ui, stage, 380.0, |ui| {
                let (r, _) = ui.allocate_exact_size(vec2(132.0, 132.0), Sense::hover());
                ui.painter().rect_filled(r, CornerRadius::same(28), Color32::from_rgb(0xF9, 0x73, 0x16).gamma_multiply(if p.dark { 0.2 } else { 0.12 }));
                ui.painter().text(r.center(), Align2::CENTER_CENTER, icons::MUSIC_NOTES, theme::regular(56.0), Color32::from_rgb(0xF9, 0x73, 0x16));
                ui.add_space(16.0);
                bidi::wrapped(ui, bf_core::media::leaf_name(&self.key), theme::semibold(17.0), p.text);
                ui.add_space(2.0);
                let state = if !st.ready || (st.buffering && !st.paused) {
                    "Buffering…"
                } else if st.ended {
                    "Finished"
                } else if st.paused {
                    "Paused"
                } else {
                    "Playing"
                };
                ui.label(RichText::new(state).color(p.text_muted));
            });
        }

        // Controls bar.
        ui.painter().rect_filled(bar, CornerRadius { nw: 0, ne: 0, sw: 16, se: 16 }, p.surface);
        ui.painter().hline(bar.x_range(), bar.top(), Stroke::new(1.0, p.border));
        let mut c = ui.new_child(egui::UiBuilder::new().max_rect(bar.shrink2(vec2(18.0, 0.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
        c.spacing_mut().item_spacing.x = 6.0;
        let enabled = st.ready && st.error.is_none();
        if widgets::icon_btn_ex(&mut c, icons::SKIP_BACK, "Back 10 s (←: 5 s)", 34.0, false, enabled).clicked() {
            pl.seek((st.position - 10.0).max(0.0));
        }
        let (icon, tip) = if st.ended {
            (icons::ARROW_COUNTER_CLOCKWISE, "Replay")
        } else if st.paused {
            (icons::PLAY, "Play (Space)")
        } else {
            (icons::PAUSE, "Pause (Space)")
        };
        if play_button(&mut c, icon, tip, enabled).clicked() {
            toggle(pl, &st);
        }
        if widgets::icon_btn_ex(&mut c, icons::SKIP_FORWARD, "Forward 10 s (→: 5 s)", 34.0, false, enabled).clicked() && st.duration.is_finite() {
            pl.seek((st.position + 10.0).min(st.duration));
        }
        c.add_space(8.0);
        let pos = self.scrub.unwrap_or(st.position);
        c.label(RichText::new(fmt_time(pos)).font(theme::medium(12.5)).color(p.text_muted));
        let vol_w = 150.0;
        let seek_w = (c.available_width() - vol_w - 70.0).max(80.0);
        let dur = if st.duration.is_finite() && st.duration > 0.0 { st.duration } else { 0.0 };
        let mut frac = if dur > 0.0 { (pos / dur) as f32 } else { 0.0 };
        let sr = slider(&mut c, &mut frac, seek_w, enabled && dur > 0.0, p.primary);
        if sr.dragged() || sr.clicked() {
            let t = frac as f64 * dur;
            self.scrub = Some(t);
            let due = self.last_seek.is_none_or(|l| l.elapsed() > Duration::from_millis(150));
            if due || sr.clicked() {
                pl.seek(t);
                self.last_seek = Some(Instant::now());
            }
        }
        if sr.drag_stopped() {
            if let Some(t) = self.scrub.take() {
                pl.seek(t);
            }
        } else if !sr.dragged() && !st.seeking {
            self.scrub = None;
        }
        c.label(RichText::new(fmt_time(dur.max(0.0)).replace("0:00", if dur > 0.0 { "0:00" } else { "--:--" })).font(theme::medium(12.5)).color(p.text_muted));
        c.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let mut v = self.volume;
            let r = slider(ui, &mut v, 90.0, true, p.text_muted);
            if r.changed() || r.dragged() || r.clicked() {
                self.volume = v;
                if self.muted && v > 0.0 {
                    self.muted = false;
                    pl.set_muted(false);
                }
                pl.set_volume(v as f64);
            }
            let vicon = if self.muted || self.volume == 0.0 {
                icons::SPEAKER_X
            } else if self.volume < 0.5 {
                icons::SPEAKER_LOW
            } else {
                icons::SPEAKER_HIGH
            };
            if widgets::icon_btn_ex(ui, vicon, if self.muted { "Unmute (M)" } else { "Mute (M)" }, 32.0, false, true).clicked() {
                self.muted = !self.muted;
                pl.set_muted(self.muted);
            }
        });
        if !st.paused && !st.ended {
            egui_ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

fn centered(ui: &mut egui::Ui, rect: egui::Rect, width: f32, add: impl FnOnce(&mut egui::Ui)) {
    let w = width.min(rect.width() - 32.0);
    let est_h = 220.0;
    let top = rect.center().y - est_h / 2.0;
    let r = egui::Rect::from_min_size(egui::pos2(rect.center().x - w / 2.0, top.max(rect.top() + 16.0)), vec2(w, rect.bottom() - top));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(r).layout(egui::Layout::top_down(egui::Align::Center)));
    child.spacing_mut().item_spacing.y = 0.0;
    add(&mut child);
}

/// A dark pill with a spinner, over the video while it waits for data.
fn buffering_badge(ui: &mut egui::Ui, center: egui::Pos2) {
    let r = egui::Rect::from_center_size(center, vec2(132.0, 40.0));
    ui.painter().rect_filled(r, CornerRadius::same(20), Color32::from_black_alpha(160));
    let mut s = ui.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_center_size(r.left_center() + vec2(24.0, 0.0), vec2(18.0, 18.0))));
    s.visuals_mut().widgets.inactive.fg_stroke.color = Color32::WHITE;
    s.add(egui::Spinner::new().size(16.0).color(Color32::WHITE));
    ui.painter().text(r.left_center() + vec2(42.0, 0.0), Align2::LEFT_CENTER, "Buffering…", theme::medium(13.0), Color32::WHITE);
}

pub(crate) fn play_button(ui: &mut egui::Ui, icon: &str, tip: &str, enabled: bool) -> egui::Response {
    let p = *pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(42.0, 42.0), if enabled { Sense::click() } else { Sense::hover() });
    let fill = if !enabled {
        p.primary.gamma_multiply(0.45)
    } else if resp.hovered() {
        p.primary_hover
    } else {
        p.primary
    };
    ui.painter().circle_filled(rect.center(), 21.0, fill);
    let nudge = if icon == icons::PLAY { vec2(1.5, 0.0) } else { Vec2::ZERO };
    ui.painter().text(rect.center() + nudge, Align2::CENTER_CENTER, icon, theme::regular(20.0), Color32::WHITE);
    let tip = tip.to_owned();
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, &tip));
    let resp = resp.on_hover_text(tip);
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// A thin track slider (0..=1) used for seeking and volume.
pub(crate) fn slider(ui: &mut egui::Ui, value: &mut f32, width: f32, enabled: bool, color: Color32) -> egui::Response {
    let p = *pal(ui);
    let (rect, mut resp) = ui.allocate_exact_size(vec2(width, 24.0), if enabled { Sense::click_and_drag() } else { Sense::hover() });
    if enabled
        && (resp.dragged() || resp.clicked())
        && let Some(pos) = resp.interact_pointer_pos()
    {
        let v = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        if (v - *value).abs() > f32::EPSILON {
            *value = v;
            resp.mark_changed();
        }
    }
    let track = egui::Rect::from_center_size(rect.center(), vec2(rect.width(), 4.0));
    ui.painter().rect_filled(track, CornerRadius::same(2), p.surface_3);
    let f = value.clamp(0.0, 1.0);
    let fill = egui::Rect::from_min_max(track.min, egui::pos2(track.left() + track.width() * f, track.bottom()));
    ui.painter().rect_filled(fill, CornerRadius::same(2), if enabled { color } else { p.border_strong });
    if enabled && (resp.hovered() || resp.dragged()) {
        ui.painter().circle(egui::pos2(fill.right(), track.center().y), 7.0, Color32::WHITE, Stroke::new(1.0, p.border_strong));
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

fn text_doc(egui_ctx: &egui::Context, key: &str, s: String, truncated: bool) -> TextDoc {
    let lines: Vec<String> = s.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l).replace('\t', "    ")).collect();
    let digits = lines.len().max(1).to_string().len() as f32;
    let char_w = egui_ctx.fonts_mut(|f| f.glyph_width(&theme::mono(13.0), '0'));
    let ext = bf_core::media::extension(key);
    let csv = match ext.as_str() {
        "csv" => Some(parse_delimited(&s, ',')),
        "tsv" => Some(parse_delimited(&s, '\t')),
        _ => None,
    };
    let col_w = csv
        .as_ref()
        .map(|rows| {
            let n = rows.iter().take(200).map(|r| r.len()).max().unwrap_or(0);
            (0..n)
                .map(|c| {
                    let chars = rows.iter().take(200).filter_map(|r| r.get(c)).map(|v| v.chars().count()).max().unwrap_or(4);
                    (chars as f32 * 7.4 + 24.0).clamp(64.0, 360.0)
                })
                .collect()
        })
        .unwrap_or_default();
    let rtl = lines.iter().enumerate().filter(|(_, l)| bidi::has_rtl(l)).map(|(i, l)| (i, bidi::visual_auto(l).into_owned())).collect();
    TextDoc { gutter: digits * char_w + 18.0, lines, rtl, rtl_wrapped: RefCell::default(), truncated, csv, col_w }
}

/// RFC-4180-ish: quotes, doubled quotes, delimiter, CR/LF. Bounded.
fn parse_delimited(s: &str, delim: char) -> Vec<Vec<String>> {
    let mut rows = vec![];
    let mut row = vec![];
    let mut cell = String::new();
    let mut quoted = false;
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if quoted {
            if c == '"' {
                if it.peek() == Some(&'"') {
                    cell.push('"');
                    it.next();
                } else {
                    quoted = false;
                }
            } else {
                cell.push(c);
            }
            continue;
        }
        match c {
            '"' if cell.is_empty() => quoted = true,
            c if c == delim => {
                if row.len() < CSV_MAX_COLS {
                    row.push(std::mem::take(&mut cell));
                } else {
                    cell.clear();
                }
            }
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut cell));
                rows.push(std::mem::take(&mut row));
                if rows.len() >= CSV_MAX_ROWS {
                    return rows;
                }
            }
            c => cell.push(c),
        }
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    rows
}

fn csv_table(ui: &mut egui::Ui, rows: &[Vec<String>], col_w: &[f32]) {
    let p = *pal(ui);
    if rows.is_empty() {
        widgets::empty_state(ui, icons::TABLE, "Empty table", "This file has no rows.");
        return;
    }
    let row_h = 30.0;
    let total_w: f32 = col_w.iter().sum::<f32>() + 56.0;
    egui::ScrollArea::both().auto_shrink(false).show_rows(ui, row_h, rows.len(), |ui, range| {
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        for i in range {
            let r = &rows[i];
            let (rect, _) = ui.allocate_exact_size(vec2(total_w.max(ui.available_width()), row_h), Sense::hover());
            if i == 0 {
                ui.painter().rect_filled(rect, CornerRadius::ZERO, p.surface_2);
            } else if i % 2 == 0 {
                ui.painter().rect_filled(rect, CornerRadius::ZERO, p.surface_2.gamma_multiply(0.45));
            }
            ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, p.border));
            let mut x = rect.left() + 12.0;
            let num = if i == 0 { String::new() } else { i.to_string() };
            ui.painter().text(egui::pos2(x + 28.0, rect.center().y), Align2::RIGHT_CENTER, num, theme::mono(12.0), p.text_subtle);
            x += 44.0;
            for (c, w) in col_w.iter().enumerate() {
                if let Some(v) = r.get(c) {
                    let font = if i == 0 { theme::semibold(13.0) } else { theme::regular(13.0) };
                    let clip = egui::Rect::from_min_max(egui::pos2(x, rect.top()), egui::pos2(x + w - 12.0, rect.bottom()));
                    ui.painter().with_clip_rect(clip.intersect(ui.clip_rect())).text(egui::pos2(x, rect.center().y), Align2::LEFT_CENTER, bidi::visual_auto(v), font, p.text);
                }
                x += w;
            }
        }
    });
}
