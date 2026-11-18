//! Rename, move, delete, new folder — single and bulk (spec §4.3, §6.3–§6.5).
//!
//! **Every mutation that changes an object's key updates three indexes in the
//! same breath** (§5.5): the cache DB, the encryption index, and the
//! thumbnail store. Folder paths read `objectKeysUnder` *before* the cache
//! rows are rewritten. Folder operations are not atomic: a [`BulkResult`]
//! reports exactly what didn't move.

use std::collections::HashSet;
use std::sync::Arc;

use bytes::Bytes;
use futures::{StreamExt, stream};

use bf_core::failures_ui::message_for;
use bf_core::{AppFailure, AppResult};
use bf_data::local::CacheDb;
use bf_data::local::cache_db::CacheEntry;

use crate::cache_service::BucketCacheService;
use crate::encryption::EncryptionEngine;
use crate::session::ClientHandle;
use crate::thumbnails::ThumbnailService;

pub const COPY_CONCURRENCY: usize = 8;
/// Deleting more than this many objects needs a typed `DELETE` confirmation.
pub const TYPED_DELETE_THRESHOLD: usize = 50;

#[derive(Debug, Clone, Default)]
pub struct BulkResult {
    pub total: usize,
    pub failed: Vec<(String, String)>,
}

impl BulkResult {
    pub fn ok(&self) -> bool {
        self.failed.is_empty()
    }
}

pub type OpProgress = Arc<dyn Fn(usize, usize) + Send + Sync>;

pub fn validate_name(name: &str) -> Result<(), &'static str> {
    let n = name.trim();
    if n.is_empty() {
        return Err("Enter a name.");
    }
    if n.contains('/') || n.contains('\\') {
        return Err("Names can't contain slashes.");
    }
    if n == "." || n == ".." {
        return Err("That name isn't allowed.");
    }
    if n.len() > 512 {
        return Err("That name is too long.");
    }
    if n.chars().any(char::is_control) {
        return Err("Names can't contain control characters.");
    }
    Ok(())
}

pub struct FileOps {
    cache: Arc<CacheDb>,
    client: ClientHandle,
    enc: Arc<EncryptionEngine>,
    thumbs: Arc<ThumbnailService>,
}

impl FileOps {
    pub fn new(cache: Arc<CacheDb>, client: ClientHandle, enc: Arc<EncryptionEngine>, thumbs: Arc<ThumbnailService>) -> Arc<Self> {
        Arc::new(Self { cache, client, enc, thumbs })
    }

    fn check_unlocked(&self, key: &str) -> AppResult<()> {
        if self.enc.is_locked(key) {
            return Err(AppFailure::conflict("busy with an encryption task"));
        }
        Ok(())
    }

    // ------------------------------------------------------------ files --

    async fn move_key(&self, from: &str, to: &str) -> AppResult<()> {
        self.check_unlocked(from)?;
        let c = self.client.get();
        c.copy_object(from, to).await?;
        c.delete_object(from).await?;
        self.cache.rename_entry(from, to).await?;
        BucketCacheService::cache_ancestor_folders(&self.cache, to).await?;
        self.enc.on_renamed_or_moved(from, to);
        self.thumbs.on_renamed_or_moved(from);
        Ok(())
    }

    pub async fn rename_file(&self, key: &str, new_name: &str) -> AppResult<String> {
        validate_name(new_name).map_err(AppFailure::conflict)?;
        let to = format!("{}{}", bf_core::media::parent_prefix(key), new_name.trim());
        if to == key {
            return Ok(to);
        }
        self.move_key(key, &to).await?;
        Ok(to)
    }

    /// A no-op if already there.
    pub async fn move_file(&self, key: &str, dest_prefix: &str) -> AppResult<Option<String>> {
        if bf_core::media::parent_prefix(key) == dest_prefix {
            return Ok(None);
        }
        let to = format!("{dest_prefix}{}", bf_core::media::leaf_name(key));
        self.move_key(key, &to).await?;
        Ok(Some(to))
    }

    pub async fn delete_file(&self, key: &str) -> AppResult<()> {
        self.check_unlocked(key)?;
        self.client.get().delete_object(key).await?;
        self.cache.delete_entry(key).await?;
        self.enc.on_deleted(key);
        self.thumbs.remove(key);
        Ok(())
    }

    /// A zero-byte marker at `prefix/`.
    pub async fn create_folder(&self, parent: &str, name: &str) -> AppResult<String> {
        validate_name(name).map_err(AppFailure::conflict)?;
        let key = format!("{parent}{}/", name.trim());
        self.client.get().put_object(&key, Bytes::new(), None, None, None).await?;
        self.cache.upsert_entry(CacheEntry::folder(&key)).await?;
        BucketCacheService::cache_ancestor_folders(&self.cache, &key).await?;
        Ok(key)
    }

    // ---------------------------------------------------------- folders --

    async fn move_prefix(&self, from: &str, to: &str, progress: Option<OpProgress>) -> AppResult<BulkResult> {
        if to.starts_with(from) {
            return Err(AppFailure::conflict("can't move a folder into itself"));
        }
        let c = self.client.get();
        let keys: Vec<String> = c.list_all(from, None).await?.into_iter().map(|o| o.key).collect();
        let cached_before = self.cache.object_keys_under(from).await?;
        let total = keys.len();
        let locked = self.enc.locked_keys();
        let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let results: Vec<(String, AppResult<()>)> = stream::iter(keys)
            .map(|k| {
                let (c, done, progress, locked) = (c.clone(), done.clone(), progress.clone(), &locked);
                async move {
                    let dst = format!("{to}{}", &k[from.len()..]);
                    let r = if locked.contains(&k) {
                        Err(AppFailure::conflict("busy with an encryption task"))
                    } else {
                        c.copy_object(&k, &dst).await
                    };
                    let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if let Some(p) = &progress {
                        p(n, total);
                    }
                    (k, r)
                }
            })
            .buffer_unordered(COPY_CONCURRENCY)
            .collect()
            .await;
        let mut failed = vec![];
        let mut copied = vec![];
        for (k, r) in results {
            match r {
                Ok(()) => copied.push(k),
                Err(e) => failed.push((k, message_for(&e))),
            }
        }
        // Delete only originals that copied cleanly.
        for e in c.delete_objects(&copied).await.unwrap_or_default() {
            failed.push((e.key.clone(), format!("{}: {}", e.code, e.message)));
        }
        let failed_set: HashSet<&String> = failed.iter().map(|(k, _)| k).collect();
        // Thumbnails per key, read before the rewrite.
        for k in &cached_before {
            self.thumbs.on_renamed_or_moved(k);
        }
        if failed.is_empty() {
            self.cache.rename_prefix(from, to).await?;
            self.enc.on_folder_moved(from, to);
        } else {
            for k in copied.iter().filter(|k| !failed_set.contains(k)) {
                let dst = format!("{to}{}", &k[from.len()..]);
                let _ = self.cache.rename_entry(k, &dst).await;
                self.enc.on_renamed_or_moved(k, &dst);
            }
        }
        self.cache.upsert_entry(CacheEntry::folder(to)).await?;
        BucketCacheService::cache_ancestor_folders(&self.cache, to).await?;
        Ok(BulkResult { total, failed })
    }

    pub async fn rename_folder(&self, prefix: &str, new_name: &str, progress: Option<OpProgress>) -> AppResult<BulkResult> {
        validate_name(new_name).map_err(AppFailure::conflict)?;
        let to = format!("{}{}/", bf_core::media::parent_prefix(prefix), new_name.trim());
        if to == prefix {
            return Ok(BulkResult::default());
        }
        self.move_prefix(prefix, &to, progress).await
    }

    pub async fn move_folder(&self, prefix: &str, dest: &str, progress: Option<OpProgress>) -> AppResult<BulkResult> {
        if bf_core::media::parent_prefix(prefix) == dest {
            return Ok(BulkResult::default());
        }
        let to = format!("{dest}{}/", bf_core::media::leaf_name(prefix));
        self.move_prefix(prefix, &to, progress).await
    }

    pub async fn delete_folder(&self, prefix: &str, progress: Option<OpProgress>) -> AppResult<BulkResult> {
        let keys: Vec<String> = self.client.get().list_all(prefix, None).await?.into_iter().map(|o| o.key).collect();
        let cached_before = self.cache.object_keys_under(prefix).await?;
        let r = self.delete_keys(keys, progress).await?;
        for k in &cached_before {
            self.thumbs.remove(k);
        }
        if r.ok() {
            self.cache.delete_entry_and_descendants(prefix).await?;
            self.enc.on_folder_deleted(prefix);
        }
        Ok(r)
    }

    /// Batched delete (1000/call) with index patching.
    async fn delete_keys(&self, keys: Vec<String>, progress: Option<OpProgress>) -> AppResult<BulkResult> {
        let locked = self.enc.locked_keys();
        let (busy, keys): (Vec<String>, Vec<String>) = keys.into_iter().partition(|k| locked.contains(k));
        let total = keys.len() + busy.len();
        let mut failed: Vec<(String, String)> = busy.into_iter().map(|k| (k, "Busy with an encryption task.".into())).collect();
        let c = self.client.get();
        let mut done = 0;
        for chunk in keys.chunks(bf_data::r2::client::DELETE_BATCH) {
            let errs = match c.delete_objects(chunk).await {
                Ok(e) => e.into_iter().map(|e| (e.key, format!("{}: {}", e.code, e.message))).collect::<Vec<_>>(),
                Err(e) => chunk.iter().map(|k| (k.clone(), message_for(&e))).collect(),
            };
            let bad: HashSet<&String> = errs.iter().map(|(k, _)| k).collect();
            let ok: Vec<String> = chunk.iter().filter(|k| !bad.contains(k)).cloned().collect();
            let _ = self.cache.delete_entries(ok.clone()).await;
            for k in &ok {
                self.enc.on_deleted(k);
                self.thumbs.remove(k);
            }
            failed.extend(errs);
            done += chunk.len();
            if let Some(p) = &progress {
                p(done, total);
            }
        }
        Ok(BulkResult { total, failed })
    }

    // ------------------------------------------------------------- bulk --

    /// Expand a mixed selection: folders → every key beneath them.
    pub async fn expand(&self, selection: &[String]) -> AppResult<Vec<String>> {
        let mut out = vec![];
        let mut seen = HashSet::new();
        for k in selection {
            if k.ends_with('/') {
                for o in self.client.get().list_all(k, None).await? {
                    if seen.insert(o.key.clone()) {
                        out.push(o.key);
                    }
                }
            } else if seen.insert(k.clone()) {
                out.push(k.clone());
            }
        }
        Ok(out)
    }

    /// Count used to size the confirmation (typed `DELETE` above 50).
    pub async fn count_for_delete(&self, selection: &[String]) -> AppResult<usize> {
        let mut n = 0;
        for k in selection {
            n += if k.ends_with('/') { self.cache.folder_usage(k).await?.files as usize + 1 } else { 1 };
        }
        Ok(n)
    }

    pub async fn bulk_delete(&self, selection: &[String], progress: Option<OpProgress>) -> AppResult<BulkResult> {
        let keys = self.expand(selection).await?;
        let cached: Vec<String> = {
            let mut v = vec![];
            for k in selection.iter().filter(|k| k.ends_with('/')) {
                v.extend(self.cache.object_keys_under(k).await?);
            }
            v
        };
        let r = self.delete_keys(keys, progress).await?;
        for k in &cached {
            self.thumbs.remove(k);
        }
        if r.ok() {
            for f in selection.iter().filter(|k| k.ends_with('/')) {
                self.cache.delete_entry_and_descendants(f).await?;
                self.enc.on_folder_deleted(f);
            }
        }
        Ok(r)
    }

    pub async fn bulk_move(&self, selection: &[String], dest: &str, progress: Option<OpProgress>) -> AppResult<BulkResult> {
        let mut out = BulkResult::default();
        let n = selection.len();
        for (i, k) in selection.iter().enumerate() {
            if k.ends_with('/') {
                match self.move_folder(k, dest, None).await {
                    Ok(r) => {
                        out.total += r.total;
                        out.failed.extend(r.failed);
                    }
                    Err(e) => out.failed.push((k.clone(), message_for(&e))),
                }
            } else {
                out.total += 1;
                if let Err(e) = self.move_file(k, dest).await {
                    out.failed.push((k.clone(), message_for(&e)));
                }
            }
            if let Some(p) = &progress {
                p(i + 1, n);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn names() {
        assert!(super::validate_name("ok.txt").is_ok());
        assert!(super::validate_name("a/b").is_err());
        assert!(super::validate_name("..").is_err());
        assert!(super::validate_name("  ").is_err());
    }
}
