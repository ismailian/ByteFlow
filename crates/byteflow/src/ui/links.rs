//! Shared-link history (spec §12): records only (never the signed URL), with
//! live countdowns.

use std::sync::Arc;

use eframe::egui::{self, CornerRadius, RichText, Stroke};

use bf_app::session::Session;
use bf_core::format;

use crate::bidi;
use crate::ctx::AppCtx;
use crate::theme::{self, pal};
use crate::ui::icons;
use crate::ui::widgets::{self, Btn, Kind, Tone};

#[derive(Default)]
pub struct LinksPage;

impl LinksPage {
    pub fn ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx, session: &Arc<Session>) {
        let p = *pal(ui);
        let links = session.sharing.links();
        let now = bf_app::now_ms();
        let live = links.iter().filter(|l| l.expires_at > now).count();
        widgets::page_header(ui, "Shared links", &format!("{live} active · {} expired", links.len() - live), |ui| {
            if ui.add(Btn::new(Kind::Secondary, "Forget expired").icon(icons::TRASH).enabled(links.len() > live)).clicked() {
                let s = session.sharing.clone();
                ctx.spawn(async move {
                    let _ = s.forget_expired().await;
                });
            }
        });
        widgets::callout(
            ui,
            Tone::Neutral,
            icons::INFO,
            "Links are signed on this computer and can't be revoked before they expire without rotating the access key in the Cloudflare dashboard. Only the record is kept here — never the link itself.",
        );
        ui.add_space(12.0);
        if links.is_empty() {
            widgets::empty_state(ui, icons::LINK_SIMPLE, "No links issued yet", "Share a file from the Files page to create a time-limited download link.");
            return;
        }
        egui::Frame::new().fill(p.surface).stroke(Stroke::new(1.0, p.border)).corner_radius(CornerRadius::same(12)).inner_margin(egui::Margin::symmetric(14, 6)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
                for (i, l) in links.iter().enumerate() {
                    if i > 0 {
                        widgets::divider(ui);
                    }
                    ui.horizontal(|ui| {
                        ui.set_min_height(44.0);
                        widgets::file_tile(ui, &l.remote_key, false, 34.0);
                        ui.add_space(2.0);
                        ui.vertical(|ui| {
                            ui.set_max_width((ui.available_width() - 260.0).max(120.0));
                            bidi::name(ui, bf_core::media::leaf_name(&l.remote_key), theme::medium(14.0), p.text);
                            bidi::name(ui, &bidi::path(&format!("/{}", l.remote_key)), theme::regular(12.0), p.text_subtle);
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 10.0;
                            if widgets::icon_btn(ui, icons::TRASH, "Forget this record").clicked() {
                                let (s, id) = (session.sharing.clone(), l.id.clone());
                                ctx.spawn(async move {
                                    let _ = s.forget(&id).await;
                                });
                            }
                            if l.expires_at > now {
                                widgets::badge_icon(ui, icons::CLOCK, &format!("expires in {}", format::duration_secs(((l.expires_at - now) / 1000) as u64)), Tone::Success);
                            } else {
                                widgets::badge(ui, "expired", Tone::Neutral);
                            }
                            ui.label(RichText::new(format::local_time(l.created_at)).size(12.5).color(p.text_muted));
                        });
                    });
                }
            });
        });
        ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
    }
}
