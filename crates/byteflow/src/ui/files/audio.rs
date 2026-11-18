//! Audio in the details panel (0.4.0). Opening an audio file plays it right
//! here instead of in the full-window viewer: a player card replaces the
//! preview while the file is selected, and a compact "Now playing" strip
//! stays at the top of the panel while you browse elsewhere. Playback
//! streams from the bucket (`bf_app::stream`); it stops when the panel is
//! hidden, when you leave the Files page, or on lock.

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, CornerRadius, RichText, Sense, Stroke, vec2};

use bf_app::session::Session;
use bf_app::stream::MediaStream;
use bf_core::{AppFailure, AppResult};

use crate::bidi;
use crate::ctx::{AppCtx, Task};
use crate::platform::media::MediaPlayer;
use crate::theme::{self, pal};
use crate::ui::icons;
use crate::ui::viewer::{fmt_time, play_button, slider, stream_player};
use crate::ui::widgets;

const ORANGE: Color32 = Color32::from_rgb(0xF9, 0x73, 0x16);

pub(super) struct AudioDock {
    pub key: Option<String>,
    opening: Option<Task<AppResult<Arc<MediaStream>>>>,
    player: Option<MediaPlayer>,
    pub error: Option<String>,
    volume: f32,
    muted: bool,
    scrub: Option<f64>,
    last_seek: Option<Instant>,
}

impl Default for AudioDock {
    fn default() -> Self {
        Self { key: None, opening: None, player: None, error: None, volume: 1.0, muted: false, scrub: None, last_seek: None }
    }
}

/// What the dock asks the files page to do.
pub(super) enum DockAction {
    /// Needs the passphrase prompt (or has no built-in player): use the viewer.
    OpenViewer(String),
    Select(String),
}

impl AudioDock {
    pub fn supported() -> bool {
        MediaPlayer::supported()
    }

    pub fn is_active(&self) -> bool {
        self.key.is_some()
    }

    pub fn play(&mut self, ctx: &AppCtx, session: &Arc<Session>, key: &str) {
        if self.key.as_deref() == Some(key) && self.player.is_some() {
            if let Some(p) = &self.player {
                let st = p.status();
                if st.ended {
                    p.seek(0.0);
                }
                p.play();
            }
            return;
        }
        self.stop();
        self.key = Some(key.to_string());
        let (s, k) = (session.clone(), key.to_string());
        self.opening = Some(ctx.task(async move { MediaStream::open(&s, &k).await }));
    }

    pub fn stop(&mut self) {
        self.player = None;
        self.opening = None;
        self.key = None;
        self.error = None;
        self.scrub = None;
    }

    /// Poll the opening task; returns an action for the files page.
    pub fn pump(&mut self, egui_ctx: &egui::Context) -> Option<DockAction> {
        let r = self.opening.as_ref().and_then(Task::poll)?;
        self.opening = None;
        let key = self.key.clone()?;
        match r {
            Ok(stream) => match stream_player(stream, &key, egui_ctx.clone()) {
                Some(pl) => {
                    pl.set_volume(self.volume as f64);
                    pl.set_muted(self.muted);
                    self.player = Some(pl);
                    None
                }
                None => {
                    self.stop();
                    Some(DockAction::OpenViewer(key))
                }
            },
            Err(AppFailure::ExternalKey { .. }) => {
                self.stop();
                Some(DockAction::OpenViewer(key))
            }
            Err(AppFailure::Crypto { message }) if message.contains("legacy format") => {
                self.error = Some(message);
                None
            }
            Err(e) if e.is_cancelled() => {
                self.stop();
                None
            }
            Err(_) => {
                self.error = Some("This file couldn't be played.".into());
                None
            }
        }
    }

    fn status_line(&self) -> (String, bool) {
        if let Some(e) = &self.error {
            return (e.clone(), true);
        }
        let Some(pl) = &self.player else { return ("Connecting…".into(), false) };
        let st = pl.status();
        if let Some(e) = st.error {
            return (e, true);
        }
        let s = if !st.ready || (st.buffering && !st.paused) {
            "Buffering…"
        } else if st.ended {
            "Finished"
        } else if st.paused {
            "Paused"
        } else {
            "Playing"
        };
        (s.into(), false)
    }

    /// The full player card for the selected audio file `key`.
    pub fn card(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, session: &Arc<Session>, key: &str, locked: bool) {
        let p = *pal(ui);
        let mine = self.key.as_deref() == Some(key);
        let w = ui.available_width();
        let frame = egui::Frame::new().fill(p.surface_2).corner_radius(CornerRadius::same(12)).inner_margin(egui::Margin::same(14));
        frame.show(ui, |ui| {
            ui.set_width(w - 28.0);
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(48.0, 48.0), Sense::hover());
                ui.painter().rect_filled(r, CornerRadius::same(12), ORANGE.gamma_multiply(if p.dark { 0.22 } else { 0.14 }));
                let playing = mine && self.player.as_ref().is_some_and(|pl| {
                    let st = pl.status();
                    !st.paused && !st.ended
                });
                ui.painter().text(r.center(), Align2::CENTER_CENTER, if playing { icons::MUSIC_NOTES } else { icons::MUSIC_NOTE }, theme::regular(24.0), ORANGE);
                ui.add_space(4.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(RichText::new("AUDIO").font(theme::semibold(11.0)).color(p.text_subtle));
                    let (line, bad) = if mine { self.status_line() } else { ("Ready to play".into(), false) };
                    ui.add(egui::Label::new(RichText::new(line).size(13.0).color(if bad { p.danger } else { p.text_muted })).wrap());
                });
            });
            ui.add_space(10.0);
            if mine && self.player.is_some() {
                self.controls(ui, true);
            } else if mine {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Starting the stream…").size(12.5).color(p.text_muted));
                });
            } else {
                let r = ui.add(widgets::Btn::new(widgets::Kind::Primary, "Play").icon(icons::PLAY).enabled(!locked).min_width(ui.available_width()));
                if r.on_hover_text("Streams from the bucket; nothing is saved on this computer").clicked() {
                    self.play(ctx, session, key);
                }
            }
        });
    }

    /// The compact strip shown while another item (or none) is selected.
    /// Returns an action when the title was clicked (select that file).
    pub fn strip(&mut self, ui: &mut egui::Ui) -> Option<DockAction> {
        let key = self.key.clone()?;
        let p = *pal(ui);
        let mut action = None;
        let mut stop = false;
        let frame = egui::Frame::new()
            .fill(p.surface)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(egui::Margin { left: 10, right: 8, top: 8, bottom: 8 });
        frame.show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let st = self.player.as_ref().map(|pl| pl.status());
                let enabled = st.as_ref().is_some_and(|s| s.ready && s.error.is_none());
                let (icon, tip) = match &st {
                    Some(s) if s.ended => (icons::ARROW_COUNTER_CLOCKWISE, "Replay"),
                    Some(s) if s.paused => (icons::PLAY, "Play"),
                    Some(_) => (icons::PAUSE, "Pause"),
                    None => (icons::PLAY, "Starting…"),
                };
                if widgets::icon_btn_ex(ui, icon, tip, 30.0, false, enabled).clicked()
                    && let (Some(pl), Some(s)) = (&self.player, &st)
                {
                    toggle(pl, s);
                }
                let right = 34.0;
                let text_w = (ui.available_width() - right).max(40.0);
                let (r, resp) = ui.allocate_exact_size(vec2(text_w, 30.0), Sense::click());
                let resp = resp.on_hover_text("Show this file").on_hover_cursor(egui::CursorIcon::PointingHand);
                let mut col = ui.new_child(egui::UiBuilder::new().max_rect(r).layout(egui::Layout::top_down(egui::Align::Min)));
                col.spacing_mut().item_spacing.y = 0.0;
                col.label(RichText::new("NOW PLAYING").font(theme::semibold(10.0)).color(p.text_subtle));
                bidi::name(&mut col, bf_core::media::leaf_name(&key), theme::medium(13.0), p.text);
                if resp.clicked() {
                    action = Some(DockAction::Select(key.clone()));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::icon_btn_ex(ui, icons::X, "Stop", 28.0, false, true).clicked() {
                        stop = true;
                    }
                });
            });
            if let Some(pl) = &self.player {
                let st = pl.status();
                let dur = if st.duration.is_finite() && st.duration > 0.0 { st.duration } else { 0.0 };
                let frac = if dur > 0.0 { (st.position / dur) as f32 } else { 0.0 };
                ui.add_space(4.0);
                widgets::progress_bar(ui, frac.clamp(0.0, 1.0), 3.0, ORANGE);
                if !st.paused && !st.ended {
                    ui.ctx().request_repaint_after(Duration::from_millis(250));
                }
            }
        });
        if stop {
            self.stop();
        }
        action
    }

    fn controls(&mut self, ui: &mut egui::Ui, full: bool) {
        let Some(pl) = self.player.as_mut() else { return };
        let p = *pal(ui);
        let st = pl.status();
        let enabled = st.ready && st.error.is_none();
        let dur = if st.duration.is_finite() && st.duration > 0.0 { st.duration } else { 0.0 };
        let pos = self.scrub.unwrap_or(st.position);

        // Seek bar, full width, times below.
        let mut frac = if dur > 0.0 { (pos / dur) as f32 } else { 0.0 };
        let sr = slider(ui, &mut frac, ui.available_width(), enabled && dur > 0.0, ORANGE);
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
        ui.horizontal(|ui| {
            ui.label(RichText::new(fmt_time(pos)).font(theme::medium(12.0)).color(p.text_muted));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let d = if dur > 0.0 { fmt_time(dur) } else { "--:--".into() };
                ui.label(RichText::new(d).font(theme::medium(12.0)).color(p.text_muted));
            });
        });
        if !full {
            return;
        }
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if widgets::icon_btn_ex(ui, icons::SKIP_BACK, "Back 10 s", 32.0, false, enabled).clicked() {
                pl.seek((st.position - 10.0).max(0.0));
            }
            let (icon, tip) = if st.ended {
                (icons::ARROW_COUNTER_CLOCKWISE, "Replay")
            } else if st.paused {
                (icons::PLAY, "Play")
            } else {
                (icons::PAUSE, "Pause")
            };
            if play_button(ui, icon, tip, enabled).clicked() {
                toggle(pl, &st);
            }
            if widgets::icon_btn_ex(ui, icons::SKIP_FORWARD, "Forward 10 s", 32.0, false, enabled).clicked() && dur > 0.0 {
                pl.seek((st.position + 10.0).min(dur));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let vw = (ui.available_width() - 40.0).clamp(40.0, 90.0);
                let mut v = self.volume;
                let r = slider(ui, &mut v, vw, true, p.text_muted);
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
                if widgets::icon_btn_ex(ui, vicon, if self.muted { "Unmute" } else { "Mute" }, 30.0, false, true).clicked() {
                    self.muted = !self.muted;
                    pl.set_muted(self.muted);
                }
            });
        });
        if !st.paused && !st.ended || st.buffering {
            ui.ctx().request_repaint_after(Duration::from_millis(200));
        }
    }
}

fn toggle(pl: &MediaPlayer, st: &crate::platform::media::Status) {
    if st.ended {
        pl.seek(0.0);
        pl.play();
    } else if st.paused {
        pl.play();
    } else {
        pl.pause();
    }
}
