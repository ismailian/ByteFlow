//! Process-lifetime services (survive lock/unlock): paths, secure storage,
//! the primary bucket's databases and thumbnail store, preferences, and the
//! stores of any other bucket opened this run (`crate::buckets`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;

use bf_core::{AppFailure, AppResult, log_warn};
use bf_data::local::{AppDb, CacheDb, ThumbnailStore};
use bf_data::paths::{AppPaths, CACHE_DB_FILE};
use bf_data::secure::protector;
use bf_data::secure::{CredentialStore, FileSecureStore, KeyManager, NoHardwareKeyManager, SecureStore, VaultStateStore};

use crate::buckets::{BucketStores, PRIMARY_BUCKET_ID, secondary_cache_path};
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
    /// The primary bucket's stores (the fields above, as one bundle).
    primary: Arc<BucketStores>,
    /// Secondary buckets' stores, opened on first use and kept for the run.
    secondary: Mutex<HashMap<String, Arc<BucketStores>>>,
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
        let primary = Arc::new(BucketStores {
            id: PRIMARY_BUCKET_ID.into(),
            app_db: app_db.clone(),
            cache_db: cache_db.clone(),
            thumbs: thumbs.clone(),
            encryption_state: paths.encryption_state(),
            credentials: credentials.clone(),
            dir: None,
        });
        Ok(Arc::new(Self {
            paths,
            secure,
            credentials,
            vault,
            prefs,
            app_db,
            cache_db,
            thumbs,
            notifier,
            hw,
            primary,
            secondary: Mutex::new(HashMap::new()),
        }))
    }

    /// Blocking: a bucket's stores, opening a secondary bucket's on first use.
    pub fn bucket_stores(&self, id: &str) -> AppResult<Arc<BucketStores>> {
        if id == PRIMARY_BUCKET_ID {
            return Ok(self.primary.clone());
        }
        if let Some(s) = self.secondary.lock().get(id) {
            return Ok(s.clone());
        }
        let s = Arc::new(BucketStores::open_secondary(&self.paths, &self.credentials, id, self.prefs.get().cache_db_dir.as_deref())?);
        Ok(self.secondary.lock().entry(id.to_string()).or_insert(s).clone())
    }

    /// Blocking: erase a bucket's local data (it was removed from the vault).
    pub fn destroy_bucket(&self, id: &str) -> AppResult<()> {
        let stores = self.bucket_stores(id)?;
        stores.destroy(&self.paths);
        self.secondary.lock().remove(id);
        Ok(())
    }

    /// Wipe: close and forget every secondary bucket's handles so the support
    /// folder can be emptied.
    pub fn close_secondary_buckets(&self) {
        for (_, s) in self.secondary.lock().drain() {
            s.close();
        }
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
        // Every opened secondary bucket's cache follows the preference.
        let others: Vec<Arc<BucketStores>> = self.secondary.lock().values().cloned().collect();
        let (paths, d2) = (self.paths.clone(), dir.clone());
        blocking(move || {
            for s in others {
                let p = secondary_cache_path(&paths, &s.id, d2.as_deref());
                if let Some(parent) = p.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(e) = s.cache_db.reopen(&p) {
                    log_warn!("move bucket cache: {}", e.developer_message());
                }
            }
            Ok(())
        })
        .await?;
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
