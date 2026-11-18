//! Directory sync (spec §11): one-way local → remote, prompt-driven. No
//! automatic uploads, no deletions, no automatic conflict resolution.
//!
//! [DESKTOP] Sync is the headline desktop feature, so the walk runs off the
//! async runtime (blocking pool), reports a live file counter, and is
//! cancellable — the `CLAUDE.md` requirements mobile left [PARTIAL].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use parking_lot::{Mutex, RwLock};

use bf_core::{AppFailure, AppResult};
use bf_data::local::AppDb;
use bf_data::local::app_db::{ManifestEntry, SyncPair, TransferRow};

use crate::encryption::EncryptionEngine;
use crate::session::ClientHandle;
use crate::upload::{UploadReport, UploadRequest, UploadService};
use crate::{SharedNotifier, blocking, new_id, now_ms};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyncClass {
    New,
    Modified,
    Unchanged,
    RemoteOnly,
}

#[derive(Debug, Clone)]
pub struct LocalFile {
    pub rel: String,
    pub size: u64,
    pub mtime_ms: i64,
}

/// The truth-table classifier (pure).
pub fn classify(local: &LocalFile, remote_size: Option<u64>, manifest: Option<&ManifestEntry>) -> SyncClass {
    let Some(rs) = remote_size else { return SyncClass::New };
    if rs != local.size {
        return SyncClass::Modified;
    }
    match manifest.and_then(|m| m.last_uploaded_mtime_ms) {
        None => SyncClass::Modified,
        Some(up) if local.mtime_ms > up => SyncClass::Modified,
        Some(_) => SyncClass::Unchanged,
    }
}

pub fn is_ignored(name: &str) -> bool {
    name.starts_with('.')
        || name.eq_ignore_ascii_case("Thumbs.db")
        || name.eq_ignore_ascii_case("desktop.ini")
        || name == ".DS_Store"
        || name.to_ascii_lowercase().ends_with(".tmp")
}

/// Estimated plaintext size of an app-written v2 object (16-byte salt,
/// 64 KiB chunks), so encrypted uploads don't classify as Modified forever.
pub fn plaintext_estimate(cipher: u64) -> u64 {
    const HEADER: u64 = 6 + 1 + 16 + 4 + 2 + 1 + 2 + 60 + 7 + 4;
    const SEG: u64 = 65_536 + 16;
    if cipher < HEADER + 16 {
        return cipher;
    }
    let body = cipher - HEADER;
    let segs = body.div_ceil(SEG).max(1);
    body - segs * 16
}

#[derive(Debug, Clone)]
pub struct SyncItem {
    pub rel: String,
    pub size: u64,
    pub mtime_ms: i64,
    pub class: SyncClass,
}

#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    pub pair_id: String,
    pub items: Vec<SyncItem>,
    pub remote_only: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ScanProgress {
    pub pair_id: Option<String>,
    pub files_seen: usize,
    pub phase: &'static str,
}

pub fn walk(root: &Path, cancel: &AtomicBool, seen: &AtomicUsize) -> AppResult<Vec<LocalFile>> {
    let mut out = vec![];
    let it = walkdir::WalkDir::new(root).follow_links(false).into_iter().filter_entry(|e| {
        e.depth() == 0 || !is_ignored(&e.file_name().to_string_lossy())
    });
    for entry in it {
        if cancel.load(Ordering::Relaxed) {
            return Err(AppFailure::Cancelled);
        }
        // Unstattable files are skipped, not fatal.
        let Ok(e) = entry else { continue };
        if !e.file_type().is_file() {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        let Ok(rel) = e.path().strip_prefix(root) else { continue };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_millis() as i64);
        out.push(LocalFile { rel, size: meta.len(), mtime_ms });
        seen.fetch_add(1, Ordering::Relaxed);
    }
    Ok(out)
}

pub fn normalize_prefix(p: &str) -> String {
    let t = p.trim().trim_matches('/');
    if t.is_empty() { String::new() } else { format!("{t}/") }
}

pub struct SyncService {
    db: AppDb,
    client: ClientHandle,
    uploads: Arc<UploadService>,
    enc: Arc<EncryptionEngine>,
    notifier: SharedNotifier,
    pairs: RwLock<Vec<SyncPair>>,
    cancel_flag: Mutex<Option<Arc<AtomicBool>>>,
    seen: Arc<AtomicUsize>,
    progress: RwLock<ScanProgress>,
    result: RwLock<Option<Arc<ScanResult>>>,
    last_error: RwLock<Option<String>>,
}

impl SyncService {
    pub fn new(db: AppDb, client: ClientHandle, uploads: Arc<UploadService>, enc: Arc<EncryptionEngine>, notifier: SharedNotifier) -> Arc<Self> {
        Arc::new(Self {
            db,
            client,
            uploads,
            enc,
            notifier,
            pairs: RwLock::new(vec![]),
            cancel_flag: Mutex::new(None),
            seen: Arc::new(AtomicUsize::new(0)),
            progress: RwLock::new(ScanProgress::default()),
            result: RwLock::new(None),
            last_error: RwLock::new(None),
        })
    }

    pub fn pairs(&self) -> Vec<SyncPair> {
        self.pairs.read().clone()
    }
    pub fn progress(&self) -> ScanProgress {
        let mut p = self.progress.read().clone();
        p.files_seen = self.seen.load(Ordering::Relaxed);
        p
    }
    pub fn result(&self) -> Option<Arc<ScanResult>> {
        self.result.read().clone()
    }
    pub fn last_error(&self) -> Option<String> {
        self.last_error.read().clone()
    }
    pub fn clear_result(&self) {
        *self.result.write() = None;
        self.notifier.changed();
    }

    pub async fn reload(&self) {
        if let Ok(p) = self.db.sync_pairs().await {
            *self.pairs.write() = p;
        }
        self.notifier.changed();
    }

    pub async fn add_pair(&self, local_dir: PathBuf, remote_prefix: &str) -> AppResult<()> {
        if !local_dir.is_dir() {
            return Err(AppFailure::not_found("folder not found"));
        }
        self.db
            .insert_sync_pair(SyncPair {
                id: new_id(),
                local_dir_path: local_dir.to_string_lossy().into_owned(),
                remote_prefix: normalize_prefix(remote_prefix),
                last_scan_at: None,
            })
            .await?;
        self.reload().await;
        Ok(())
    }

    pub async fn remove_pair(&self, id: &str) -> AppResult<()> {
        self.db.delete_sync_pair(id).await?;
        if self.result().is_some_and(|r| r.pair_id == id) {
            self.clear_result();
        }
        self.reload().await;
        Ok(())
    }

    pub fn cancel_scan(&self) {
        if let Some(f) = self.cancel_flag.lock().as_ref() {
            f.store(true, Ordering::Relaxed);
        }
    }

    pub fn scanning(&self) -> bool {
        self.cancel_flag.lock().is_some()
    }

    pub async fn scan(&self, pair_id: &str) -> AppResult<()> {
        let Some(pair) = self.pairs().into_iter().find(|p| p.id == pair_id) else {
            return Err(AppFailure::not_found("sync pair"));
        };
        let flag = Arc::new(AtomicBool::new(false));
        {
            let mut g = self.cancel_flag.lock();
            if g.is_some() {
                return Err(AppFailure::conflict("a scan is already running"));
            }
            *g = Some(flag.clone());
        }
        self.seen.store(0, Ordering::Relaxed);
        *self.progress.write() = ScanProgress { pair_id: Some(pair.id.clone()), files_seen: 0, phase: "Scanning folder" };
        *self.last_error.write() = None;
        self.notifier.changed();
        let r = self.scan_inner(&pair, flag).await;
        *self.cancel_flag.lock() = None;
        *self.progress.write() = ScanProgress::default();
        match r {
            Ok(res) => {
                let _ = self.db.set_pair_scanned(&pair.id, now_ms()).await;
                *self.result.write() = Some(Arc::new(res));
                self.reload().await;
                Ok(())
            }
            Err(e) => {
                if !e.is_cancelled() {
                    *self.last_error.write() = Some(bf_core::failures_ui::message_for(&e));
                }
                self.notifier.changed();
                Err(e)
            }
        }
    }

    async fn scan_inner(&self, pair: &SyncPair, flag: Arc<AtomicBool>) -> AppResult<ScanResult> {
        let root = PathBuf::from(&pair.local_dir_path);
        let (f2, seen) = (flag.clone(), self.seen.clone());
        let walk_task = tokio::spawn(blocking(move || walk(&root, &f2, &seen)));
        // List the remote side concurrently with the walk.
        self.progress.write().phase = "Scanning folder and bucket";
        let remote = self.client.get().list_all(&pair.remote_prefix, None).await;
        let local = walk_task.await.map_err(|_| AppFailure::unknown("walk panicked"))??;
        let remote = remote?;
        if flag.load(Ordering::Relaxed) {
            return Err(AppFailure::Cancelled);
        }
        let manifest: HashMap<String, ManifestEntry> =
            self.db.manifest(&pair.id).await?.into_iter().map(|m| (m.relative_path.clone(), m)).collect();
        let mut remote_map: HashMap<String, u64> = HashMap::new();
        for o in remote.iter().filter(|o| !o.key.ends_with('/')) {
            let rel = o.key[pair.remote_prefix.len()..].to_string();
            let size = if self.enc.is_encrypted(&o.key) { plaintext_estimate(o.size) } else { o.size };
            remote_map.insert(rel, size);
        }
        let mut items: Vec<SyncItem> = local
            .iter()
            .map(|l| SyncItem { rel: l.rel.clone(), size: l.size, mtime_ms: l.mtime_ms, class: classify(l, remote_map.get(&l.rel).copied(), manifest.get(&l.rel)) })
            .collect();
        items.sort_by(|a, b| a.rel.cmp(&b.rel));
        let local_set: std::collections::HashSet<&str> = local.iter().map(|l| l.rel.as_str()).collect();
        let mut remote_only: Vec<String> = remote_map.keys().filter(|k| !local_set.contains(k.as_str())).cloned().collect();
        remote_only.sort();
        Ok(ScanResult { pair_id: pair.id.clone(), items, remote_only })
    }

    /// Nothing uploads until the owner confirms.
    pub async fn confirm(&self, pair_id: &str, rels: &[String], encrypt: bool) -> AppResult<UploadReport> {
        let Some(pair) = self.pairs().into_iter().find(|p| p.id == pair_id) else {
            return Err(AppFailure::not_found("sync pair"));
        };
        let root = PathBuf::from(&pair.local_dir_path);
        let reqs = rels
            .iter()
            .map(|rel| UploadRequest {
                local_path: root.join(rel),
                remote_key: format!("{}{rel}", pair.remote_prefix),
                delete_source: false,
                group_id: Some(format!("sync:{}", pair.id)),
            })
            .collect();
        let r = self.uploads.enqueue(reqs, encrypt).await;
        self.clear_result();
        Ok(r)
    }

    /// Manifest updated on success.
    pub async fn on_upload_completed(&self, row: &TransferRow) {
        let Some(pair_id) = row.group_id.as_deref().and_then(|g| g.strip_prefix("sync:")) else { return };
        let Some(pair) = self.pairs().into_iter().find(|p| p.id == pair_id) else { return };
        let Some(rel) = row.remote_key.strip_prefix(&pair.remote_prefix) else { return };
        let local = Path::new(&pair.local_dir_path).join(rel);
        if let Ok(meta) = tokio::fs::metadata(&local).await {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis() as i64);
            let _ = self.db.record_uploaded(&pair.id, rel, meta.len(), mtime).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lf(size: u64, mtime: i64) -> LocalFile {
        LocalFile { rel: "a".into(), size, mtime_ms: mtime }
    }
    fn m(up: Option<i64>) -> ManifestEntry {
        ManifestEntry { relative_path: "a".into(), size: 1, mtime_ms: 0, last_uploaded_mtime_ms: up }
    }

    #[test]
    fn truth_table() {
        assert_eq!(classify(&lf(1, 5), None, None), SyncClass::New);
        assert_eq!(classify(&lf(1, 5), None, Some(&m(Some(9)))), SyncClass::New);
        assert_eq!(classify(&lf(1, 5), Some(2), Some(&m(Some(9)))), SyncClass::Modified);
        assert_eq!(classify(&lf(1, 5), Some(1), None), SyncClass::Modified);
        assert_eq!(classify(&lf(1, 5), Some(1), Some(&m(None))), SyncClass::Modified);
        assert_eq!(classify(&lf(1, 10), Some(1), Some(&m(Some(9)))), SyncClass::Modified);
        assert_eq!(classify(&lf(1, 9), Some(1), Some(&m(Some(9)))), SyncClass::Unchanged);
    }

    #[test]
    fn ignore_and_walk() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("sub/.git")).unwrap();
        std::fs::write(d.path().join("sub/a.txt"), b"x").unwrap();
        std::fs::write(d.path().join("sub/.git/obj"), b"x").unwrap();
        std::fs::write(d.path().join(".hidden"), b"x").unwrap();
        std::fs::write(d.path().join("Thumbs.db"), b"x").unwrap();
        std::fs::write(d.path().join("x.TMP"), b"x").unwrap();
        let files = walk(d.path(), &AtomicBool::new(false), &AtomicUsize::new(0)).unwrap();
        assert_eq!(files.iter().map(|f| f.rel.as_str()).collect::<Vec<_>>(), vec!["sub/a.txt"]);
    }

    #[test]
    fn plaintext_estimate_inverts_v2() {
        for p in [0u64, 1, 65_536, 65_537, 10_000_000] {
            let segs = p.div_ceil(65_536).max(1);
            let c = 103 + p + 16 * segs;
            assert_eq!(plaintext_estimate(c), p, "p={p}");
        }
        assert_eq!(normalize_prefix("/a/b"), "a/b/");
        assert_eq!(normalize_prefix(""), "");
    }
}
