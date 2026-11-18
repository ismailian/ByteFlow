//! Upload flow (spec §6.6 steps 3–6) and the transfer-engine hooks that
//! patch the cache, generate thumbnails, and update sync manifests.

use std::path::PathBuf;
use std::sync::Arc;

use bf_core::failures_ui::message_for;
use bf_data::local::CacheDb;
use bf_data::local::app_db::TransferRow;
use bf_data::local::cache_db::CacheEntry;
use bf_data::r2::HeadInfo;

use crate::browser::BrowserController;
use crate::cache_service::BucketCacheService;
use crate::encryption::EncryptionEngine;
use crate::overview::OverviewController;
use crate::session::ClientHandle;
use crate::sync::SyncService;
use crate::thumbnails::ThumbnailService;
use crate::transfers::{EnqueueUpload, TransferEngine, TransferHooks};

#[derive(Debug, Clone)]
pub struct UploadRequest {
    pub local_path: PathBuf,
    pub remote_key: String,
    /// Delete the original once the upload is verified (§8).
    pub delete_source: bool,
    pub group_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct UploadReport {
    pub enqueued: usize,
    pub encrypted: usize,
    /// Encryption is on but the key couldn't be used: uploaded as plaintext,
    /// with a clear warning — never silently, never blocking (§6.6 step 3).
    pub plaintext_warning: bool,
    pub failures: Vec<(String, String)>,
}

pub struct UploadService {
    client: ClientHandle,
    enc: Arc<EncryptionEngine>,
    transfers: Arc<TransferEngine>,
    temp: PathBuf,
}

impl UploadService {
    pub fn new(client: ClientHandle, enc: Arc<EncryptionEngine>, transfers: Arc<TransferEngine>, temp: PathBuf) -> Arc<Self> {
        Arc::new(Self { client, enc, transfers, temp })
    }

    /// Overwrite detection: a failed HEAD counts as "doesn't exist", so a
    /// transient error never produces a spurious prompt.
    pub async fn exists(&self, key: &str) -> bool {
        self.client.get().head(key).await.is_ok()
    }

    pub async fn enqueue(&self, reqs: Vec<UploadRequest>, encrypt_wanted: bool) -> UploadReport {
        let mut report = UploadReport::default();
        // Resolve the upload encryption target once.
        let target = if encrypt_wanted { self.enc.upload_target() } else { None };
        report.plaintext_warning = encrypt_wanted && target.is_none();
        for r in reqs {
            let delete_source = r.delete_source.then(|| r.local_path.clone());
            let (body, ct) = (r.local_path.clone(), Some(bf_core::media::content_type_for(&r.remote_key).to_string()));
            let body = match &target {
                Some(t) => match self.enc.encrypt_to_temp(&r.local_path, t, &self.temp).await {
                    Ok(p) => {
                        // Before enqueue, so thumbnailing (at enqueue) stands down.
                        self.enc.mark_encrypted(&r.remote_key, t.epoch);
                        report.encrypted += 1;
                        p
                    }
                    Err(e) => {
                        // An encryption failure skips that file with an error.
                        report.failures.push((r.remote_key.clone(), message_for(&e)));
                        continue;
                    }
                },
                None => body,
            };
            let req = EnqueueUpload { local_path: body, remote_key: r.remote_key.clone(), content_type: ct, delete_source, group_id: r.group_id };
            match self.transfers.enqueue_upload(req).await {
                Ok(_) => report.enqueued += 1,
                Err(e) => report.failures.push((r.remote_key, message_for(&e))),
            }
        }
        report
    }
}

pub struct SessionTransferHooks {
    pub cache: Arc<CacheDb>,
    pub enc: Arc<EncryptionEngine>,
    pub thumbs: Arc<ThumbnailService>,
    pub browser: Arc<BrowserController>,
    pub overview: Arc<OverviewController>,
    pub sync: Arc<SyncService>,
}

impl TransferHooks for SessionTransferHooks {
    fn upload_enqueued(&self, row: &TransferRow) {
        self.thumbs.generate_from_file(&row.remote_key, std::path::Path::new(&row.local_path));
    }

    fn upload_completed(&self, row: &TransferRow, head: Option<&HeadInfo>) {
        let mut e = CacheEntry::file(
            &row.remote_key,
            head.map_or(row.total_bytes, |h| h.size),
            head.map(|h| h.etag.clone()).filter(|s| !s.is_empty()),
            head.and_then(|h| h.last_modified_ms).or(Some(crate::now_ms())),
        );
        if let Some(epoch) = self.enc.epoch_of(&row.remote_key) {
            e.encrypted = true;
            e.key_epoch = Some(epoch);
        }
        let (cache, browser, overview, sync, row) =
            (self.cache.clone(), self.browser.clone(), self.overview.clone(), self.sync.clone(), row.clone());
        tokio::spawn(async move {
            let _ = cache.upsert_entry(e).await;
            let _ = BucketCacheService::cache_ancestor_folders(&cache, &row.remote_key).await;
            sync.on_upload_completed(&row).await;
            browser.reload().await;
            overview.reload().await;
        });
    }

    fn download_completed(&self, row: &TransferRow) {
        self.thumbs.generate_from_file(&row.remote_key, std::path::Path::new(&row.local_path));
    }
}
