//! "Update available" prompt: checked once at launch, offered with
//! Download / Close. Download fetches and verifies the installer, then hands
//! it to `platform::update`, which installs it silently, restarts ByteFlow
//! and deletes the installer.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use eframe::egui::{self, RichText};

use bf_app::update::{self, Release};
use bf_core::AppResult;

use crate::ctx::{AppCtx, Task};
use crate::platform;
use crate::theme;
use crate::ui::icons;
use crate::ui::widgets::{self, ModalSpec, Tone};

const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Default)]
struct Progress {
    done: AtomicU64,
    /// 0 = unknown.
    total: AtomicU64,
}

enum State {
    Checking(Task<AppResult<Option<Release>>>),
    Offer(Release),
    Downloading { rel: Release, progress: Arc<Progress>, task: Task<AppResult<PathBuf>>, handle: tokio::task::JoinHandle<()> },
    Failed { rel: Release, error: String },
    Restarting,
    Idle,
}

pub struct Updater {
    state: State,
}

impl Updater {
    /// Starts the launch-time check (quietly ignored when offline).
    pub fn start(ctx: &AppCtx) -> Self {
        // dev-env demo/screenshot runs never show the prompt.
        // dev-env only: BYTEFLOW_DEV_FAKE_UPDATE=<version> shows the offer
        // without the network (for UI work).
        #[cfg(feature = "dev-env")]
        if let Some(v) = crate::demo::env("BYTEFLOW_DEV_FAKE_UPDATE") {
            let url = format!("https://github.com/ismailian/ByteFlow/releases/download/v{v}/ByteFlow-Setup-{v}-windows-x64.exe");
            return Self { state: State::Offer(Release { version: v, url, sha256: "0".repeat(64), size: Some(8_100_000) }) };
        }
        if cfg!(feature = "dev-env") && std::env::var_os("BYTEFLOW_DEV_DEMO").is_some() {
            return Self { state: State::Idle };
        }
        Self { state: State::Checking(ctx.task(update::check(CURRENT))) }
    }

    fn download(ctx: &AppCtx, rel: Release) -> State {
        let progress = Arc::new(Progress::default());
        let dir = ctx.core.paths.temp.join("updates");
        let (slot, p2, r2, egui_ctx) = (Arc::new(parking_lot::Mutex::new(None)), progress.clone(), rel.clone(), ctx.egui.clone());
        let task = Task::from_slot(slot.clone());
        let handle = ctx.rt.spawn(async move {
            let rp = egui_ctx.clone();
            let res = update::download(&r2, &dir, |done, total| {
                p2.done.store(done, Ordering::Relaxed);
                p2.total.store(total.unwrap_or(0), Ordering::Relaxed);
                rp.request_repaint();
            })
            .await;
            *slot.lock() = Some(res);
            egui_ctx.request_repaint();
        });
        State::Downloading { rel, progress, task, handle }
    }

    pub fn ui(&mut self, ctx: &AppCtx) {
        let egui_ctx = ctx.egui.clone();
        let state = std::mem::replace(&mut self.state, State::Idle);
        self.state = match state {
            State::Checking(t) => match t.poll() {
                Some(Ok(Some(rel))) => State::Offer(rel),
                Some(_) => State::Idle,
                None => State::Checking(t),
            },
            State::Offer(rel) => {
                let (mut download, mut close) = (false, false);
                let subtitle = format!("ByteFlow {} is ready. You have {CURRENT}.", rel.version);
                let spec = ModalSpec::new("update-offer", "Update available").subtitle(&subtitle).icon(icons::CLOUD_ARROW_DOWN, Tone::Primary).width(440.0);
                close |= widgets::modal(&egui_ctx, spec, |ui| {
                    let size = rel.size.map(|s| format!(" ({})", bf_core::format::bytes(s))).unwrap_or_default();
                    widgets::hint(ui, &format!("Download the installer{size}, install it and restart ByteFlow. Your vault and settings are kept."));
                    widgets::modal_footer(ui, |ui| {
                        download = widgets::btn(ui, widgets::Kind::Primary, icons::DOWNLOAD, "Download").clicked();
                        close |= widgets::secondary(ui, "Close").clicked();
                    });
                });
                if download {
                    Self::download(ctx, rel)
                } else if close {
                    State::Idle
                } else {
                    State::Offer(rel)
                }
            }
            State::Downloading { rel, progress, task, handle } => {
                if let Some(res) = task.poll() {
                    match res.map_err(|e| e.developer_message().to_owned()).and_then(|path| {
                        platform::update::install_and_restart(&path).map_err(|e| format!("could not start the installer: {e}"))
                    }) {
                        Ok(()) => {
                            egui_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            State::Restarting
                        }
                        Err(error) => State::Failed { rel, error },
                    }
                } else {
                    let mut cancel = false;
                    let title = format!("Downloading ByteFlow {}", rel.version);
                    let spec = ModalSpec::new("update-progress", &title).icon(icons::CLOUD_ARROW_DOWN, Tone::Primary).width(440.0).locked();
                    widgets::modal(&egui_ctx, spec, |ui| {
                        let done = progress.done.load(Ordering::Relaxed);
                        let total = progress.total.load(Ordering::Relaxed);
                        let frac = if total > 0 { done as f32 / total as f32 } else { 0.0 };
                        let text = if total > 0 {
                            format!("{} of {}", bf_core::format::bytes(done), bf_core::format::bytes(total))
                        } else {
                            bf_core::format::bytes(done)
                        };
                        widgets::progress(ui, frac, text);
                        ui.add_space(2.0);
                        widgets::hint(ui, "ByteFlow will close, install the update and open again.");
                        widgets::modal_footer(ui, |ui| {
                            cancel = widgets::secondary(ui, "Cancel").clicked();
                        });
                    });
                    if cancel {
                        handle.abort();
                        let _ = std::fs::remove_dir_all(ctx.core.paths.temp.join("updates"));
                        State::Idle
                    } else {
                        State::Downloading { rel, progress, task, handle }
                    }
                }
            }
            State::Failed { rel, error } => {
                let (mut retry, mut close) = (false, false);
                let spec = ModalSpec::new("update-failed", "Update failed").icon(icons::WARNING_CIRCLE, Tone::Danger).width(440.0);
                close |= widgets::modal(&egui_ctx, spec, |ui| {
                    ui.label(RichText::new(format!("Couldn't update to ByteFlow {}: {error}.", rel.version)).font(theme::regular(13.5)).color(theme::muted(ui)));
                    widgets::modal_footer(ui, |ui| {
                        retry = widgets::primary(ui, "Try again").clicked();
                        close |= widgets::secondary(ui, "Close").clicked();
                    });
                });
                if retry {
                    Self::download(ctx, rel)
                } else if close {
                    State::Idle
                } else {
                    State::Failed { rel, error }
                }
            }
            s @ (State::Restarting | State::Idle) => s,
        };
    }
}
