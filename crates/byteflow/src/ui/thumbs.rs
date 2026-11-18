//! Thumbnail textures for visible cells. Presence is an in-memory lookup
//! (no `stat`, no network); decoding runs off the UI thread; textures are
//! LRU-capped. Eligibility is re-checked every frame through the single
//! policy predicate, so a newly-encrypted object loses its cover at once.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};

use bf_app::session::Session;

use crate::ctx::AppCtx;

const MAX_TEXTURES: usize = 600;

pub struct ThumbCache {
    textures: HashMap<String, TextureHandle>,
    order: VecDeque<String>,
    pending: HashSet<String>,
    failed: HashSet<String>,
    tx: Sender<(String, Option<ColorImage>)>,
    rx: Receiver<(String, Option<ColorImage>)>,
}

impl Default for ThumbCache {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self { textures: HashMap::new(), order: VecDeque::new(), pending: HashSet::new(), failed: HashSet::new(), tx, rx }
    }
}

impl ThumbCache {
    pub fn pump(&mut self, egui_ctx: &egui::Context) {
        while let Ok((key, img)) = self.rx.try_recv() {
            self.pending.remove(&key);
            match img {
                Some(img) => {
                    let t = egui_ctx.load_texture(format!("thumb:{key}"), img, TextureOptions::LINEAR);
                    self.textures.insert(key.clone(), t);
                    self.order.push_back(key);
                    while self.order.len() > MAX_TEXTURES {
                        if let Some(old) = self.order.pop_front() {
                            self.textures.remove(&old);
                        }
                    }
                }
                None => {
                    self.failed.insert(key);
                }
            }
        }
    }

    pub fn get(&mut self, ctx: &AppCtx, session: &Arc<Session>, key: &str) -> Option<TextureHandle> {
        if !session.thumbs.has(key) {
            self.textures.remove(key);
            return None;
        }
        if let Some(t) = self.textures.get(key) {
            return Some(t.clone());
        }
        if self.pending.contains(key) || self.failed.contains(key) {
            return None;
        }
        self.pending.insert(key.to_string());
        let (tx, thumbs, k, repaint) = (self.tx.clone(), session.thumbs.clone(), key.to_string(), ctx.egui.clone());
        ctx.rt.spawn_blocking(move || {
            let img = thumbs.read(&k).and_then(|b| image::load_from_memory(&b).ok()).map(|i| {
                let rgba = i.to_rgba8();
                ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw())
            });
            let _ = tx.send((k, img));
            repaint.request_repaint();
        });
        None
    }
}
