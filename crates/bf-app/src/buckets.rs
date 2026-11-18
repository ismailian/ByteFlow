//! Per-bucket local state (multi-bucket, desktop 0.4.0).
//!
//! Every connected bucket has its own transfer/sync/link database, listing
//! cache, thumbnail cache, encryption index and file master keys. The primary
//! bucket (the one set up at onboarding) keeps the original locations —
//! `support/r2vault.sqlite`, `bucket_cache.sqlite`, `encryption_state.json`,
//! `thumbs/` and the unprefixed master-key names — so installs from before
//! multi-bucket open exactly as they did. Every other bucket lives under
//! `support/buckets/<id>/` (its cache under `<custom cache dir>/buckets/<id>/`
//! when the cache was moved) with `bucket.<id>.`-prefixed keys.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bf_core::{AppFailure, AppResult, log_warn};
use bf_data::local::{AppDb, CacheDb, ThumbnailStore};
use bf_data::paths::{AppPaths, CACHE_DB_FILE};
use bf_data::secure::CredentialStore;
pub use bf_data::secure::credential_store::{BucketEntry, PRIMARY_BUCKET_ID};

pub struct BucketStores {
    pub id: String,
    pub app_db: AppDb,
    pub cache_db: Arc<CacheDb>,
    pub thumbs: Arc<ThumbnailStore>,
    pub encryption_state: PathBuf,
    /// This bucket's view of the credential store (its file master keys and
    /// external-key table).
    pub credentials: Arc<CredentialStore>,
    /// `support/buckets/<id>` for a secondary bucket; `None` for the primary.
    pub dir: Option<PathBuf>,
}

pub const APP_DB_FILE: &str = "r2vault.sqlite";
pub const ENCRYPTION_STATE_FILE: &str = "encryption_state.json";

/// Where a secondary bucket's listing cache lives for a cache-dir preference.
pub fn secondary_cache_path(paths: &AppPaths, id: &str, custom: Option<&Path>) -> PathBuf {
    match custom {
        Some(d) => d.join("buckets").join(id).join(CACHE_DB_FILE),
        None => paths.bucket_dir(id).join(CACHE_DB_FILE),
    }
}

fn open_cache(path: &Path, fallback: &Path) -> AppResult<CacheDb> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match CacheDb::open(path) {
        Ok(db) => Ok(db),
        Err(e) => {
            log_warn!("bucket cache unusable, falling back: {}", e.developer_message());
            CacheDb::open(fallback).map_err(|e| AppFailure::unknown(format!("cache db: {}", e.developer_message())))
        }
    }
}

pub fn sqlite_files(main: &Path) -> Vec<PathBuf> {
    ["", "-wal", "-shm", "-journal"]
        .iter()
        .map(|s| {
            let mut p = main.as_os_str().to_os_string();
            p.push(s);
            PathBuf::from(p)
        })
        .collect()
}

fn remove_sqlite(main: &Path) {
    for f in sqlite_files(main) {
        if f.exists()
            && let Err(e) = std::fs::remove_file(&f)
        {
            log_warn!("remove bucket db: {e}");
        }
    }
}

impl BucketStores {
    /// Blocking: open (creating if needed) a secondary bucket's stores.
    pub fn open_secondary(paths: &AppPaths, base: &CredentialStore, id: &str, custom_cache: Option<&Path>) -> AppResult<Self> {
        if id == PRIMARY_BUCKET_ID || id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(AppFailure::conflict("bad bucket id"));
        }
        let dir = paths.bucket_dir(id);
        std::fs::create_dir_all(&dir).map_err(|e| AppFailure::io("bucket dir", e))?;
        let app_db = AppDb::open(&dir.join(APP_DB_FILE))?;
        let default_cache = dir.join(CACHE_DB_FILE);
        let cache_path = secondary_cache_path(paths, id, custom_cache.filter(|d| d.is_dir()));
        let cache_db = Arc::new(open_cache(&cache_path, &default_cache)?);
        let thumbs = Arc::new(ThumbnailStore::open(&dir.join("thumbs")));
        Ok(Self {
            id: id.to_string(),
            app_db,
            cache_db,
            thumbs,
            encryption_state: dir.join(ENCRYPTION_STATE_FILE),
            credentials: Arc::new(base.scoped(id)),
            dir: Some(dir),
        })
    }

    pub fn is_primary(&self) -> bool {
        self.dir.is_none()
    }

    /// Close the database handles (so the files can be deleted on Windows).
    pub fn close(&self) {
        if let Err(e) = self.app_db.close() {
            log_warn!("close bucket app db: {}", e.developer_message());
        }
        if let Err(e) = self.cache_db.close() {
            log_warn!("close bucket cache: {}", e.developer_message());
        }
    }

    /// Blocking: erase everything this computer holds for the bucket — its
    /// databases (wherever the cache lives), thumbnails, encryption index and
    /// file master keys. The primary bucket's databases are recreated empty
    /// at their original paths (the process keeps using those handles).
    pub fn destroy(&self, paths: &AppPaths) {
        let cache_path = self.cache_db.path();
        self.close();
        remove_sqlite(&cache_path);
        if let Err(e) = self.thumbs.clear() {
            log_warn!("clear bucket thumbs: {}", e.developer_message());
        }
        let _ = std::fs::remove_file(&self.encryption_state);
        if let Err(e) = self.credentials.delete_all_master() {
            log_warn!("delete bucket keys: {}", e.developer_message());
        }
        match &self.dir {
            Some(dir) => {
                if let Some(d) = cache_path.parent().filter(|d| !d.starts_with(dir)) {
                    let _ = std::fs::remove_dir(d);
                }
                if let Err(e) = std::fs::remove_dir_all(dir) {
                    log_warn!("remove bucket dir: {e}");
                }
            }
            None => {
                let app = paths.app_db();
                remove_sqlite(&app);
                if let Err(e) = self.app_db.reopen(&app) {
                    log_warn!("reopen app db: {}", e.developer_message());
                }
                if let Err(e) = self.cache_db.reopen(&cache_path) {
                    log_warn!("reopen cache: {}", e.developer_message());
                }
            }
        }
    }
}
