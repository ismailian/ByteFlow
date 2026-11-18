//! Process-lifetime services (survive lock/unlock): paths, secure storage,
//! the two databases, the thumbnail store, preferences.

use std::path::PathBuf;
use std::sync::Arc;

use bf_core::{AppFailure, AppResult, log_warn};
use bf_data::local::{AppDb, CacheDb, ThumbnailStore};
use bf_data::paths::{AppPaths, CACHE_DB_FILE};
use bf_data::secure::protector;
use bf_data::secure::{CredentialStore, FileSecureStore, KeyManager, NoHardwareKeyManager, SecureStore, VaultStateStore};

use crate::prefs::PrefStore;
use crate::{SharedNotifier, blocking};

pub struct Core {
    pub paths: AppPaths,
    pub secure: Arc<FileSecureStore>,
    pub credentials: Arc<CredentialStore>,
    pub vault: Arc<VaultStateStore>,
    pub prefs: Arc<PrefStore>,
    pub app_db: AppDb,
    pub cache_db: Arc<CacheDb>,
    pub thumbs: Arc<ThumbnailStore>,
    pub notifier: SharedNotifier,
    pub hw: Arc<dyn KeyManager>,
}

impl Core {
    pub async fn open(paths: AppPaths, notifier: SharedNotifier) -> AppResult<Arc<Self>> {
        blocking(move || Self::open_blocking(paths, notifier)).await
    }

    pub fn open_blocking(paths: AppPaths, notifier: SharedNotifier) -> AppResult<Arc<Self>> {
        let secure = Arc::new(FileSecureStore::new(paths.secure_store(), protector::platform()));
        let store: Arc<dyn SecureStore> = secure.clone();
        let hw: Arc<dyn KeyManager> = Arc::new(NoHardwareKeyManager);
        let credentials = Arc::new(CredentialStore::new(store.clone(), hw.clone()));
        let vault = Arc::new(VaultStateStore::new(store));
        let prefs = Arc::new(PrefStore::load(vault.clone()));
        let app_db = AppDb::open(&paths.app_db())?;
        let cache_db = Arc::new(open_cache_db(&paths, prefs.get().cache_db_dir)?);
        let thumbs = Arc::new(ThumbnailStore::open(&paths.thumbs()));
        Ok(Arc::new(Self { paths, secure, credentials, vault, prefs, app_db, cache_db, thumbs, notifier, hw }))
    }

    pub fn notify(&self) {
        self.notifier.changed();
    }

    pub fn cache_db_path(&self) -> PathBuf {
        self.cache_db.path()
    }

    /// Change the cache location: close, reopen (re-adopting an existing DB).
    pub async fn set_cache_dir(&self, dir: Option<PathBuf>) -> AppResult<()> {
        let target = match &dir {
            Some(d) => d.join(CACHE_DB_FILE),
            None => self.paths.default_cache_db_dir().join(CACHE_DB_FILE),
        };
        let cache = self.cache_db.clone();
        blocking(move || cache.reopen(&target)).await?;
        self.prefs.update(|p| p.cache_db_dir = dir)?;
        Ok(())
    }
}

/// An unreachable custom directory falls back to the default rather than
/// breaking browsing (§5.6).
fn open_cache_db(paths: &AppPaths, custom: Option<PathBuf>) -> AppResult<CacheDb> {
    if let Some(dir) = custom
        && dir.is_dir()
    {
        match CacheDb::open(&dir.join(CACHE_DB_FILE)) {
            Ok(db) => return Ok(db),
            Err(e) => log_warn!("custom cache dir unusable, falling back: {}", e.developer_message()),
        }
    }
    CacheDb::open(&paths.default_cache_db_dir().join(CACHE_DB_FILE))
        .map_err(|e| AppFailure::unknown(format!("cache db: {}", e.developer_message())))
}
