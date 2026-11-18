//! The unlocked session graph (spec §2.4). Every session-scoped service is
//! built from the unlocked config; locking drops the graph, cancels all
//! in-flight work, and wipes session keys.

use std::sync::Arc;

use parking_lot::RwLock;
use tokio_util::sync::CancellationToken;

use bf_core::AppResult;
use bf_data::r2::{R2Client, R2Config};

use crate::browser::BrowserController;
use crate::cache_service::BucketCacheService;
use crate::core::Core;
use crate::encryption::EncryptionEngine;
use crate::encryption::keys::LoadedKeys;
use crate::fileops::FileOps;
use crate::overview::OverviewController;
use crate::sharing::SharingService;
use crate::sync::SyncService;
use crate::thumbnails::ThumbnailService;
use crate::transfers::TransferEngine;
use crate::upload::UploadService;

/// Hot-swappable client: config edits swap it without rebuilding engines.
#[derive(Clone)]
pub struct ClientHandle(Arc<RwLock<Arc<R2Client>>>);

impl ClientHandle {
    pub fn new(c: R2Client) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(c))))
    }
    pub fn get(&self) -> Arc<R2Client> {
        self.0.read().clone()
    }
    pub fn set(&self, c: R2Client) {
        *self.0.write() = Arc::new(c);
    }
}

pub struct Session {
    pub core: Arc<Core>,
    config: RwLock<R2Config>,
    pub client: ClientHandle,
    pub transfers: Arc<TransferEngine>,
    pub encryption: Arc<EncryptionEngine>,
    pub cache: Arc<BucketCacheService>,
    pub thumbs: Arc<ThumbnailService>,
    pub browser: Arc<BrowserController>,
    pub overview: Arc<OverviewController>,
    pub ops: Arc<FileOps>,
    pub uploads: Arc<UploadService>,
    pub sync: Arc<SyncService>,
    pub sharing: Arc<SharingService>,
    pub cancel: CancellationToken,
}

impl Session {
    pub async fn start(core: Arc<Core>, config: R2Config, keys: LoadedKeys) -> AppResult<Arc<Self>> {
        let client = ClientHandle::new(R2Client::new(&config)?);
        let cancel = CancellationToken::new();
        let prefs = core.prefs.get();
        let notifier = core.notifier.clone();

        let encryption = EncryptionEngine::start(core.clone(), client.clone(), keys, cancel.child_token()).await;
        let thumbs = ThumbnailService::new(core.thumbs.clone(), encryption.clone(), notifier.clone(), cancel.child_token());
        let transfers = TransferEngine::new(
            core.app_db.clone(),
            client.clone(),
            core.paths.temp_sub("enc_upload"),
            prefs.transfer_concurrency,
            notifier.clone(),
            cancel.child_token(),
        );
        let cache = BucketCacheService::new(core.cache_db.clone(), client.clone(), encryption.clone(), thumbs.clone(), notifier.clone());
        let browser = BrowserController::new(core.cache_db.clone(), prefs.files_sort, notifier.clone());
        let overview = OverviewController::new(core.cache_db.clone(), notifier.clone());
        let ops = FileOps::new(core.cache_db.clone(), client.clone(), encryption.clone(), thumbs.clone());
        let uploads = UploadService::new(client.clone(), encryption.clone(), transfers.clone(), core.paths.temp_sub("enc_upload"));
        let sync = SyncService::new(core.app_db.clone(), client.clone(), uploads.clone(), encryption.clone(), notifier.clone());
        let sharing = SharingService::new(core.app_db.clone(), client.clone(), notifier.clone());

        let session = Arc::new(Self {
            core: core.clone(),
            config: RwLock::new(config),
            client,
            transfers,
            encryption,
            cache,
            thumbs,
            browser,
            overview,
            ops,
            uploads,
            sync,
            sharing,
            cancel,
        });
        session.wire();
        session.transfers.start().await;
        session.browser.reload().await;
        session.overview.reload().await;
        session.sharing.reload().await;
        session.sync.reload().await;
        Ok(session)
    }

    /// Cross-service hooks: §5.5 index patching, §10.3 thumbnail sources,
    /// sync manifest updates.
    fn wire(self: &Arc<Self>) {
        self.transfers.set_hooks(Arc::new(crate::upload::SessionTransferHooks {
            cache: self.core.cache_db.clone(),
            enc: self.encryption.clone(),
            thumbs: self.thumbs.clone(),
            browser: self.browser.clone(),
            overview: self.overview.clone(),
            sync: self.sync.clone(),
        }));
        let cache = self.core.cache_db.clone();
        self.encryption.set_index_hook(Arc::new(move |key: String, epoch: Option<i64>| {
            let cache = cache.clone();
            tokio::spawn(async move {
                let _ = cache.set_encrypted(&key, epoch.is_some(), epoch).await;
            });
        }));
        // Thumbnails follow the encryption index (one-way dependency).
        self.thumbs.subscribe_to_encryption();
        // Refresh views when a cache-patching operation completes.
        let (b, o) = (self.browser.clone(), self.overview.clone());
        self.cache.set_on_scan_complete(Arc::new(move || {
            let (b, o) = (b.clone(), o.clone());
            tokio::spawn(async move {
                b.reload().await;
                o.reload().await;
            });
        }));
    }

    pub fn config(&self) -> R2Config {
        self.config.read().clone()
    }

    /// Hot-swap credentials (config edit, §13).
    pub fn swap_config(&self, cfg: R2Config) -> AppResult<()> {
        self.client.set(R2Client::new(&cfg)?);
        *self.config.write() = cfg;
        Ok(())
    }

    /// Lock: stop everything, wipe keys.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        self.encryption.shutdown().await;
        self.transfers.flush_progress().await;
    }
}
