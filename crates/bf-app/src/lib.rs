//! ByteFlow application layer: controllers and engines (auth, browser, cache,
//! encryption, thumbnails, transfers, sync, sharing, overview).
//!
//! Concurrency model: all I/O is async on a multi-threaded tokio runtime;
//! CPU-bound work (Argon2id, file crypto, image decode, directory walks) runs
//! on the blocking pool / rayon. State is published as cheap snapshots the UI
//! reads each frame, and every change pings a [`Notifier`] so the UI repaints
//! only when something actually changed.

pub mod auth;
pub mod browser;
pub mod buckets;
pub mod cache_service;
pub mod core;
pub mod encryption;
pub mod fileops;
pub mod overview;
pub mod prefs;
pub mod session;
pub mod sharing;
pub mod strength;
pub mod stream;
pub mod sync;
pub mod thumbnails;
pub mod transfers;
pub mod update;
pub mod upload;
pub mod wiper;

use std::sync::Arc;

/// Wakes the presentation layer (egui `request_repaint`).
pub trait Notifier: Send + Sync {
    fn changed(&self);
}

pub struct NoopNotifier;
impl Notifier for NoopNotifier {
    fn changed(&self) {}
}

pub type SharedNotifier = Arc<dyn Notifier>;

/// Run CPU-bound work on the blocking pool and flatten the join error.
pub async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> bf_core::AppResult<T> + Send + 'static,
) -> bf_core::AppResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| bf_core::AppFailure::unknown("background task panicked"))?
}

pub fn now_ms() -> i64 {
    bf_core::format::now_ms()
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
