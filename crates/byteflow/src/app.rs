//! The eframe application: routes on the vault state, enforces auto-lock,
//! capture exclusion and the privacy overlay, and renders toasts.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};

use bf_app::auth::{AuthController, VaultState};
use bf_app::core::Core;

use crate::bidi;
use crate::ctx::{AppCtx, ToastKind, Toasts};
use crate::platform;
use crate::platform::instance::InstanceGuard;
use crate::theme;
use crate::ui::icons;
use crate::ui::{home::Home, onboarding::Onboarding, shell::Shell, unlock::Unlock};

pub struct ByteFlowApp {
    ctx: AppCtx,
    onboarding: Onboarding,
    unlock: Unlock,
    home: Home,
    shell: Option<Shell>,
    last_activity: Instant,
    capture_applied: bool,
    os_locked: Arc<AtomicBool>,
    instance: InstanceGuard,
    theme_mode: Option<bf_app::prefs::ThemeMode>,
    session_id: usize,
}

impl ByteFlowApp {
    pub fn new(cc: &eframe::CreationContext<'_>, rt: tokio::runtime::Handle, core: Arc<Core>, instance: InstanceGuard) -> Self {
        theme::install_fonts(&cc.egui_ctx);
        let auth = AuthController::new(core.clone());
        let ctx = AppCtx { rt, auth: auth.clone(), core, egui: cc.egui_ctx.clone(), toasts: Toasts::default() };
        ctx.spawn(async move {
            auth.init().await;
            // dev-env demo: unlock the seeded vault straight away.
            #[cfg(feature = "dev-env")]
            if crate::demo::enabled() {
                auth.submit_password(zeroize::Zeroizing::new(crate::demo::PASSWORD.as_bytes().to_vec())).await;
                if crate::demo::env("BYTEFLOW_DEV_PAGE").as_deref() != Some("home") {
                    let _ = auth.open_bucket(bf_app::buckets::PRIMARY_BUCKET_ID).await;
                }
            }
        });
        let repaint = cc.egui_ctx.clone();
        let os_locked = platform::oslock::spawn(move || repaint.request_repaint());
        // Apply capture exclusion as early as possible.
        let capture_applied = platform::window::exclude_from_capture(cc);
        Self {
            ctx,
            onboarding: Onboarding::default(),
            unlock: Unlock::default(),
            home: Home::default(),
            shell: None,
            last_activity: Instant::now(),
            capture_applied,
            os_locked,
            instance,
            theme_mode: None,
            session_id: 0,
        }
    }

    fn lock_now(&mut self) {
        let auth = self.ctx.auth.clone();
        self.ctx.spawn(async move { auth.lock().await });
    }

    fn track_activity(&mut self, ctx: &egui::Context) {
        let (input, focused, minimized) = ctx.input(|i| {
            (
                !i.events.is_empty() || i.pointer.is_moving() || i.pointer.any_down() || !i.raw.dropped_files.is_empty(),
                i.viewport().focused.unwrap_or(true),
                i.viewport().minimized.unwrap_or(false),
            )
        });
        // A video playing in the focused, visible window counts as activity,
        // so the idle lock doesn't cut a film off. OS lock/sleep and
        // minimising still apply as before.
        let watching = platform::media::video_playing() && focused && !minimized;
        if input || watching {
            self.last_activity = Instant::now();
        }
    }

    fn privacy_overlay(&self, ctx: &egui::Context) {
        let (minimized, focused) = ctx.input(|i| (i.viewport().minimized.unwrap_or(false), i.viewport().focused.unwrap_or(true)));
        let blur_on_focus = self.ctx.prefs().privacy_blur_on_focus_loss;
        let unlocked = matches!(self.ctx.auth.state(), VaultState::Unlocked);
        if unlocked && (minimized || (blur_on_focus && !focused)) {
            let screen = ctx.content_rect();
            egui::Area::new(egui::Id::unique("privacy-overlay")).order(egui::Order::Foreground).fixed_pos(screen.min).show(ctx, |ui| {
                let p = theme::pal(ui);
                ui.painter().rect_filled(screen, 0.0, p.canvas);
                let c = screen.center();
                let tile = egui::Rect::from_center_size(c - egui::vec2(0.0, 26.0), egui::vec2(64.0, 64.0));
                ui.painter().rect_filled(tile, egui::CornerRadius::same(18), p.primary);
                ui.painter().text(tile.center(), egui::Align2::CENTER_CENTER, icons::LOCK_SIMPLE, theme::regular(30.0), Color32::WHITE);
                ui.painter().text(c + egui::vec2(0.0, 32.0), egui::Align2::CENTER_CENTER, "ByteFlow", theme::semibold(20.0), p.text);
                ui.painter().text(c + egui::vec2(0.0, 56.0), egui::Align2::CENTER_CENTER, "Contents hidden", theme::regular(13.0), p.text_muted);
            });
        }
    }

    fn toasts(&self, ctx: &egui::Context) {
        let toasts = self.ctx.toasts.take_live();
        if toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::unique("toasts"))
            .order(egui::Order::Tooltip)
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-20.0, -20.0))
            .show(ctx, |ui| {
                let p = *theme::pal(ui);
                ui.set_max_width(400.0);
                ui.spacing_mut().item_spacing.y = 8.0;
                for (text, kind) in toasts {
                    let (icon, color) = match kind {
                        ToastKind::Info => (icons::INFO, p.primary),
                        ToastKind::Success => (icons::CHECK_CIRCLE, p.success),
                        ToastKind::Warn => (icons::WARNING, p.warn),
                        ToastKind::Error => (icons::WARNING_CIRCLE, p.danger),
                    };
                    let r = egui::Frame::new()
                        .fill(p.surface)
                        .stroke(egui::Stroke::new(1.0, p.border))
                        .corner_radius(12)
                        .shadow(egui::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(if p.dark { 120 } else { 30 }) })
                        .inner_margin(egui::Margin { left: 16, right: 14, top: 11, bottom: 11 })
                        .show(ui, |ui| {
                            ui.set_min_width(260.0);
                            ui.horizontal_top(|ui| {
                                ui.spacing_mut().item_spacing.x = 10.0;
                                ui.label(RichText::new(icon).size(17.0).color(color));
                                bidi::wrapped(ui, &text, theme::regular(14.0), p.text);
                            });
                        });
                    // Accent edge.
                    let rect = r.response.rect;
                    let bar = egui::Rect::from_min_size(rect.min + egui::vec2(0.0, 10.0), egui::vec2(3.0, rect.height() - 20.0));
                    ui.painter().rect_filled(bar, egui::CornerRadius::same(2), color);
                }
            });
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

impl eframe::App for ByteFlowApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if !self.capture_applied {
            self.capture_applied = platform::window::exclude_from_capture(frame);
        }
        #[cfg(feature = "dev-env")]
        if let Some(z) = crate::demo::env("BYTEFLOW_DEV_ZOOM").and_then(|z| z.parse::<f32>().ok())
            && (ctx.zoom_factor() - z).abs() > 0.001
        {
            ctx.set_zoom_factor(z);
        }
        let mode = self.ctx.prefs().theme_mode;
        if self.theme_mode != Some(mode) {
            theme::apply(&ctx, mode);
            self.theme_mode = Some(mode);
        }
        if self.instance.focus_requested.swap(false, Ordering::SeqCst) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        self.track_activity(&ctx);

        let state = self.ctx.auth.state();
        // Immediate lock on OS session lock / sleep.
        if self.os_locked.swap(false, Ordering::SeqCst) && matches!(state, VaultState::Unlocked) {
            self.lock_now();
        }
        // Auto-lock after N idle seconds (covers minimised/backgrounded too).
        if matches!(state, VaultState::Unlocked) {
            let timeout = Duration::from_secs(self.ctx.prefs().auto_lock_seconds);
            let idle = self.last_activity.elapsed();
            if idle >= timeout {
                self.lock_now();
            } else {
                ctx.request_repaint_after(timeout - idle + Duration::from_millis(50));
            }
        }

        match state {
            VaultState::Loading | VaultState::Wiping => {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.centered_and_justified(|ui| ui.spinner());
                });
            }
            VaultState::NeedsPasswords | VaultState::NeedsConfig => {
                self.shell = None;
                self.unlock = Unlock::default();
                self.onboarding.ui(ui, &self.ctx, &state);
            }
            VaultState::Locked { error } => {
                if self.shell.take().is_some() {
                    self.ctx.toasts.clear();
                }
                self.onboarding = Onboarding::default();
                self.home = Home::default();
                self.unlock.ui(ui, &self.ctx, error.as_deref());
            }
            VaultState::Unlocked => {
                let Some(session) = self.ctx.session() else {
                    // No bucket open: the bucket grid.
                    if let Some(mut s) = self.shell.take() {
                        s.cleanup_temps();
                    }
                    if self.home.ui(ui, &self.ctx) {
                        self.lock_now();
                    }
                    self.privacy_overlay(&ctx);
                    self.toasts(&ctx);
                    return;
                };
                let id = Arc::as_ptr(&session) as usize;
                if self.shell.is_none() || self.session_id != id {
                    if let Some(mut old) = self.shell.take() {
                        old.cleanup_temps();
                    }
                    self.shell = Some(Shell::new(&self.ctx, &session));
                    self.session_id = id;
                    self.last_activity = Instant::now();
                }
                let shell = self.shell.as_mut().expect("set above");
                let lock = shell.ui(ui, &self.ctx, &session);
                if lock {
                    self.lock_now();
                } else if shell.take_switch_bucket() {
                    let auth = self.ctx.auth.clone();
                    self.ctx.spawn(async move { auth.close_bucket().await });
                }
            }
        }
        self.privacy_overlay(&ctx);
        self.toasts(&ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Flush progress/state and wipe session keys before the process ends.
        let auth = self.ctx.auth.clone();
        let _ = self.ctx.rt.block_on(async move {
            tokio::time::timeout(Duration::from_secs(3), auth.lock()).await
        });
        if let Some(s) = self.shell.as_mut() {
            s.cleanup_temps();
        }
    }
}
