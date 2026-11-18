//! Full bucket scan (spec §5.4). Single-flight; flat paginated walk; bounded
//! concurrent magic probes only for new/changed objects the engine doesn't
//! know; `replace_all` in one transaction; then reconcile + stale thumbnails.
//! On failure the cache is left untouched and reconcile never runs.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::{StreamExt, stream};
use parking_lot::RwLock;

use bf_core::crypto::file_crypto::{MAGIC_PROBE_BYTES, starts_with_magic};
use bf_core::failures_ui::message_for;
use bf_core::{AppFailure, AppResult};
use bf_data::local::CacheDb;
use bf_data::local::cache_db::CacheEntry;

use crate::encryption::EncryptionEngine;
use crate::session::ClientHandle;
use crate::thumbnails::ThumbnailService;
use crate::{SharedNotifier, now_ms};

pub const PROBE_CONCURRENCY: usize = 6;

#[derive(Debug, Clone, Default)]
pub struct ScanStatus {
    pub scanning: bool,
    pub listed: usize,
    pub probing: usize,
    pub last_error: Option<String>,
}

pub struct BucketCacheService {
    cache: Arc<CacheDb>,
    client: ClientHandle,
    encryption: Arc<EncryptionEngine>,
    thumbs: Arc<ThumbnailService>,
    notifier: SharedNotifier,
    in_flight: AtomicBool,
    status: RwLock<ScanStatus>,
    on_complete: RwLock<Option<Arc<dyn Fn() + Send + Sync>>>,
}

/// Every intermediate prefix of `key` (`a/b/c.txt` → `a/`, `a/b/`).
pub fn ancestor_prefixes(key: &str) -> Vec<String> {
    let mut out = vec![];
    let trimmed = key.trim_end_matches('/');
    for (i, c) in trimmed.char_indices() {
        if c == '/' {
            out.push(trimmed[..=i].to_string());
        }
    }
    out
}

impl BucketCacheService {
    pub fn new(
        cache: Arc<CacheDb>,
        client: ClientHandle,
        encryption: Arc<EncryptionEngine>,
        thumbs: Arc<ThumbnailService>,
        notifier: SharedNotifier,
    ) -> Arc<Self> {
        Arc::new(Self {
            cache,
            client,
            encryption,
            thumbs,
            notifier,
            in_flight: AtomicBool::new(false),
            status: RwLock::new(ScanStatus::default()),
            on_complete: RwLock::new(None),
        })
    }

    pub fn set_on_scan_complete(&self, f: Arc<dyn Fn() + Send + Sync>) {
        *self.on_complete.write() = Some(f);
    }

    pub fn status(&self) -> ScanStatus {
        self.status.read().clone()
    }

    fn set_status(&self, f: impl FnOnce(&mut ScanStatus)) {
        f(&mut self.status.write());
        self.notifier.changed();
    }

    /// A second call while one is running is a no-op.
    pub async fn full_scan(&self) -> AppResult<()> {
        if self.in_flight.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.set_status(|s| {
            s.scanning = true;
            s.listed = 0;
            s.probing = 0;
        });
        let r = self.scan_inner().await;
        if let Err(e) = &r {
            let msg = message_for(e);
            let _ = self.cache.record_scan_error(msg.clone()).await;
            self.set_status(|s| s.last_error = Some(msg));
        } else {
            self.set_status(|s| s.last_error = None);
        }
        self.set_status(|s| s.scanning = false);
        self.in_flight.store(false, Ordering::SeqCst);
        if let Some(f) = self.on_complete.read().clone() {
            f();
        }
        r
    }

    async fn scan_inner(&self) -> AppResult<()> {
        self.cache.record_scan_started(now_ms()).await?;
        let client = self.client.get();
        let objects = client.list_all("", None).await?;
        self.set_status(|s| s.listed = objects.len());

        let index = self.encryption.index_snapshot();
        let previous: HashMap<String, CacheEntry> =
            self.cache.all_entries().await?.into_iter().filter(|e| !e.is_folder).map(|e| (e.key.clone(), e)).collect();

        let mut folders: HashSet<String> = HashSet::new();
        let mut entries: Vec<CacheEntry> = Vec::with_capacity(objects.len());
        let mut to_probe: Vec<usize> = vec![];
        let mut found: HashMap<String, Option<i64>> = HashMap::new();
        let mut present: HashSet<String> = HashSet::with_capacity(objects.len());

        for o in &objects {
            for p in ancestor_prefixes(&o.key) {
                folders.insert(p);
            }
            if o.key.ends_with('/') {
                folders.insert(o.key.clone());
                continue;
            }
            present.insert(o.key.clone());
            let mut e = CacheEntry::file(&o.key, o.size, Some(o.etag.clone()), o.last_modified_ms);
            if let Some(epoch) = index.get(&o.key) {
                // 1. The engine's index: authoritative, free.
                e.encrypted = true;
                e.key_epoch = Some(*epoch);
            } else if let Some(prev) = previous.get(&o.key).filter(|p| p.etag.as_deref() == Some(o.etag.as_str())) {
                // 2. Same eTag as the cached row: trusted, no probe.
                e.encrypted = prev.encrypted;
                e.key_epoch = prev.key_epoch;
                if prev.encrypted {
                    found.insert(o.key.clone(), prev.key_epoch);
                }
            } else if o.size >= 6 {
                // 3. New/changed and unknown: probe after listing.
                to_probe.push(entries.len());
            }
            entries.push(e);
        }

        self.set_status(|s| s.probing = to_probe.len());
        let keys: Vec<(usize, String)> = to_probe.iter().map(|&i| (i, entries[i].key.clone())).collect();
        let probes: Vec<(usize, bool)> = stream::iter(keys)
            .map(|(i, k)| {
                let c = client.clone();
                async move {
                    // An unreadable object counts as not encrypted (harmless).
                    let enc = c.get_range(&k, 0, MAGIC_PROBE_BYTES).await.map(|b| starts_with_magic(&b)).unwrap_or(false);
                    (i, enc)
                }
            })
            .buffer_unordered(PROBE_CONCURRENCY)
            .collect()
            .await;
        for (i, enc) in probes {
            if enc {
                // A probe proves *that* it's encrypted, not under which epoch.
                entries[i].encrypted = true;
                entries[i].key_epoch = None;
                found.insert(entries[i].key.clone(), None);
            }
        }
        for (k, e) in &index {
            if present.contains(k) {
                found.insert(k.clone(), Some(*e));
            }
        }
        // Stale thumbnails: eTag changed, or the object is gone.
        let new_etags: HashMap<&str, &str> = objects.iter().map(|o| (o.key.as_str(), o.etag.as_str())).collect();
        let stale: Vec<String> = previous
            .iter()
            .filter(|(k, p)| match new_etags.get(k.as_str()) {
                None => true,
                Some(etag) => Some(*etag) != p.etag.as_deref(),
            })
            .map(|(k, _)| k.clone())
            .collect();

        entries.extend(folders.into_iter().map(|f| CacheEntry::folder(&f)));
        self.cache.replace_all(entries, now_ms()).await?;
        self.encryption.reconcile_from_scan(&found, &present);
        self.thumbs.retire(&stale);
        Ok(())
    }

    /// Upload completion etc.: add any missing ancestor folder rows.
    pub async fn cache_ancestor_folders(cache: &CacheDb, key: &str) -> AppResult<()> {
        let folders: Vec<CacheEntry> = ancestor_prefixes(key).iter().map(|p| CacheEntry::folder(p)).collect();
        if folders.is_empty() {
            return Ok(());
        }
        cache.upsert_entries(folders).await
    }
}

pub fn not_scanning() -> AppFailure {
    AppFailure::conflict("scan in progress")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ancestors() {
        assert_eq!(ancestor_prefixes("a/b/c.txt"), vec!["a/", "a/b/"]);
        assert_eq!(ancestor_prefixes("a/b/"), vec!["a/"]);
        assert!(ancestor_prefixes("c.txt").is_empty());
    }
}
