//! Persistent, concurrency-limited transfer queue (spec §7, §8).
//!
//! * Rows live in the app DB (truth) and an in-memory mirror (for the UI).
//! * A pump starts queued tasks up to the concurrency limit; each task runs
//!   on its own tokio task with a child cancellation token (pause / cancel /
//!   lock all cancel cooperatively and drop the in-flight request).
//! * A 500 ms ticker publishes a snapshot with speed and ETA and writes
//!   progress to the DB — never per chunk.
//! * Only `upload` and `download` execute; other kinds are reserved.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::FutureExt;
use futures::future::BoxFuture;
use parking_lot::{Mutex, RwLock};
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use bf_core::failures_ui::message_for;
use bf_core::{AppFailure, AppResult, log_warn};
use bf_data::local::AppDb;
use bf_data::local::app_db::{TransferKind, TransferRow, TransferStatus};
use bf_data::r2::{CompletedPart, HeadInfo};

use super::retry::RetryPolicy;
use super::uploader::{self, MultipartResume, ResumeSink, SINGLE_PUT_MAX, UploadSpec};
use crate::session::ClientHandle;
use crate::{SharedNotifier, new_id, now_ms};

pub const THROTTLE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct TransferView {
    pub row: TransferRow,
    pub bytes: u64,
    pub speed: f64,
    pub eta_secs: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct TransferSnapshot {
    pub tasks: Vec<TransferView>,
    pub running: usize,
    pub queued: usize,
    pub total_speed: f64,
}

#[derive(Debug, Clone)]
pub enum TransferEvent {
    UploadCompleted(TransferRow),
    DownloadCompleted(TransferRow),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceDeletionOutcome {
    Deleted,
    Failed,
    Unverified,
}

#[derive(Debug, Clone)]
pub struct SourceDeletion {
    pub path: String,
    pub remote_key: String,
    pub outcome: SourceDeletionOutcome,
}

pub trait TransferHooks: Send + Sync {
    /// Fired at enqueue (thumbnail generation runs in parallel; the upload
    /// never waits on it).
    fn upload_enqueued(&self, _row: &TransferRow) {}
    fn upload_completed(&self, _row: &TransferRow, _head: Option<&HeadInfo>) {}
    fn download_completed(&self, _row: &TransferRow) {}
}

pub struct EnqueueUpload {
    pub local_path: PathBuf,
    pub remote_key: String,
    pub content_type: Option<String>,
    /// The original's path for verified auto-delete (§8). Not `local_path`,
    /// which may be an encrypted temp copy.
    pub delete_source: Option<PathBuf>,
    pub group_id: Option<String>,
}

#[derive(Default)]
struct Live {
    bytes: u64,
    last_bytes: u64,
    speed: f64,
}

pub struct TransferEngine {
    db: AppDb,
    client: ClientHandle,
    owned_temp: PathBuf,
    notifier: SharedNotifier,
    hooks: RwLock<Option<Arc<dyn TransferHooks>>>,
    concurrency: AtomicUsize,
    rows: Mutex<HashMap<String, TransferRow>>,
    running: Mutex<HashMap<String, CancellationToken>>,
    live: Mutex<HashMap<String, Live>>,
    snapshot: RwLock<Arc<TransferSnapshot>>,
    events: broadcast::Sender<TransferEvent>,
    deletions: broadcast::Sender<SourceDeletion>,
    cancel: CancellationToken,
    retry: RetryPolicy,
    dirty: AtomicBool,
    started: AtomicBool,
    /// Captured in `new` (inside the runtime): `pump` is also reached from
    /// sync UI-thread calls (`set_concurrency`), where `tokio::spawn` panics.
    rt: tokio::runtime::Handle,
}

impl TransferEngine {
    pub fn new(
        db: AppDb,
        client: ClientHandle,
        owned_temp: PathBuf,
        concurrency: usize,
        notifier: SharedNotifier,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            db,
            client,
            owned_temp,
            notifier,
            hooks: RwLock::new(None),
            concurrency: AtomicUsize::new(concurrency.clamp(1, 8)),
            rows: Mutex::new(HashMap::new()),
            running: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            snapshot: RwLock::new(Arc::new(TransferSnapshot::default())),
            events: broadcast::channel(256).0,
            deletions: broadcast::channel(64).0,
            cancel,
            retry: RetryPolicy::default(),
            dirty: AtomicBool::new(true),
            started: AtomicBool::new(false),
            rt: tokio::runtime::Handle::current(),
        })
    }

    pub fn set_hooks(&self, h: Arc<dyn TransferHooks>) {
        *self.hooks.write() = Some(h);
    }
    fn hooks(&self) -> Option<Arc<dyn TransferHooks>> {
        self.hooks.read().clone()
    }

    pub fn snapshot(&self) -> Arc<TransferSnapshot> {
        self.snapshot.read().clone()
    }
    pub fn events(&self) -> broadcast::Receiver<TransferEvent> {
        self.events.subscribe()
    }
    pub fn source_deletions(&self) -> broadcast::Receiver<SourceDeletion> {
        self.deletions.subscribe()
    }

    /// Launch recovery: interrupted tasks → queued; pending source deletes
    /// retried; ticker started; queue pumped.
    pub async fn start(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.db.reset_interrupted().await;
        if let Ok(rows) = self.db.transfers().await {
            let mut m = self.rows.lock();
            for r in rows {
                m.insert(r.id.clone(), r);
            }
        }
        let pending_deletes: Vec<TransferRow> = self
            .rows
            .lock()
            .values()
            .filter(|r| r.status == TransferStatus::Completed && r.delete_source_ref.is_some())
            .cloned()
            .collect();
        for r in pending_deletes {
            let me = self.clone();
            tokio::spawn(async move { me.verify_and_delete_source(&r).await });
        }
        let me = self.clone();
        tokio::spawn(async move { me.ticker().await });
        self.publish();
        self.pump();
    }

    pub fn concurrency(&self) -> usize {
        self.concurrency.load(Ordering::Relaxed)
    }

    /// Changing the limit at runtime pumps immediately.
    pub fn set_concurrency(self: &Arc<Self>, n: usize) {
        self.concurrency.store(n.clamp(1, 8), Ordering::Relaxed);
        self.pump();
    }

    // ----------------------------------------------------------- enqueue --

    pub async fn enqueue_upload(self: &Arc<Self>, req: EnqueueUpload) -> AppResult<String> {
        let len = tokio::fs::metadata(&req.local_path).await.map_err(|e| AppFailure::io("stat", e))?.len();
        let now = now_ms();
        let row = TransferRow {
            id: new_id(),
            kind: TransferKind::Upload,
            group_id: req.group_id,
            local_path: req.local_path.to_string_lossy().into_owned(),
            remote_key: req.remote_key,
            total_bytes: len,
            transferred_bytes: 0,
            status: TransferStatus::Queued,
            error: None,
            content_type: req.content_type,
            multipart_upload_id: None,
            completed_parts: vec![],
            retry_count: 0,
            delete_source_ref: req.delete_source.map(|p| p.to_string_lossy().into_owned()),
            created_at: now,
            updated_at: now,
        };
        self.db.insert_transfer(row.clone()).await?;
        self.rows.lock().insert(row.id.clone(), row.clone());
        if let Some(h) = self.hooks() {
            h.upload_enqueued(&row);
        }
        self.changed();
        self.pump();
        Ok(row.id)
    }

    /// Best-effort HEAD for the total; progress still works if it fails.
    pub async fn enqueue_download(self: &Arc<Self>, remote_key: &str, dest: &Path, group_id: Option<String>) -> AppResult<String> {
        let total = self.client.get().head(remote_key).await.map(|h| h.size).unwrap_or(0);
        let now = now_ms();
        let row = TransferRow {
            id: new_id(),
            kind: TransferKind::Download,
            group_id,
            local_path: dest.to_string_lossy().into_owned(),
            remote_key: remote_key.to_string(),
            total_bytes: total,
            transferred_bytes: 0,
            status: TransferStatus::Queued,
            error: None,
            content_type: None,
            multipart_upload_id: None,
            completed_parts: vec![],
            retry_count: 0,
            delete_source_ref: None,
            created_at: now,
            updated_at: now,
        };
        self.db.insert_transfer(row.clone()).await?;
        self.rows.lock().insert(row.id.clone(), row.clone());
        self.changed();
        self.pump();
        Ok(row.id)
    }

    // ---------------------------------------------------------- controls --

    pub async fn pause(self: &Arc<Self>, id: &str) {
        let changed = self.update_row(id, |r| {
            if matches!(r.status, TransferStatus::Running | TransferStatus::Queued) {
                r.status = TransferStatus::Paused;
                true
            } else {
                false
            }
        });
        if changed {
            if let Some(t) = self.running.lock().get(id) {
                t.cancel();
            }
            let _ = self.db.set_status(id, TransferStatus::Paused, None, now_ms()).await;
            self.changed();
        }
    }

    pub async fn resume(self: &Arc<Self>, id: &str) {
        let changed = self.update_row(id, |r| {
            if matches!(r.status, TransferStatus::Paused | TransferStatus::Failed) {
                r.status = TransferStatus::Queued;
                r.error = None;
                r.retry_count = 0;
                true
            } else {
                false
            }
        });
        if changed {
            let _ = self.db.set_status(id, TransferStatus::Queued, None, now_ms()).await;
            let _ = self.db.set_retry_count(id, 0).await;
            self.changed();
            self.pump();
        }
    }

    /// Cancel also aborts the multipart upload (no orphaned storage charges),
    /// for running and not-yet-running tasks alike.
    pub async fn cancel(self: &Arc<Self>, id: &str) {
        let mut snapshot = None;
        self.update_row(id, |r| {
            if r.status.is_terminal() {
                return false;
            }
            r.status = TransferStatus::Cancelled;
            snapshot = Some(r.clone());
            true
        });
        let Some(row) = snapshot else { return };
        if let Some(t) = self.running.lock().get(id) {
            t.cancel();
        }
        let _ = self.db.set_status(id, TransferStatus::Cancelled, None, now_ms()).await;
        if let Some(uid) = &row.multipart_upload_id {
            let _ = self.client.get().abort_multipart(&row.remote_key, uid).await;
            let _ = self.db.set_upload_id(id, None).await;
        }
        match row.kind {
            TransferKind::Download => {
                let _ = tokio::fs::remove_file(part_path(Path::new(&row.local_path))).await;
            }
            TransferKind::Upload => self.remove_owned_temp(&row.local_path).await,
            _ => {}
        }
        self.changed();
    }

    pub async fn remove(&self, id: &str) {
        let terminal = self.rows.lock().get(id).is_some_and(|r| r.status.is_terminal());
        if terminal {
            self.rows.lock().remove(id);
            let _ = self.db.delete_transfer(id).await;
            self.changed();
        }
    }

    pub async fn clear_finished(&self) {
        let _ = self.db.clear_finished().await;
        self.rows.lock().retain(|_, r| !(r.status.is_terminal() && r.delete_source_ref.is_none()));
        self.changed();
    }

    pub async fn pause_all(self: &Arc<Self>) {
        let ids: Vec<String> = self.rows.lock().values().filter(|r| !r.status.is_terminal()).map(|r| r.id.clone()).collect();
        for id in ids {
            self.pause(&id).await;
        }
    }

    pub async fn resume_all(self: &Arc<Self>) {
        let ids: Vec<String> = self.rows.lock().values().filter(|r| r.status == TransferStatus::Paused).map(|r| r.id.clone()).collect();
        for id in ids {
            self.resume(&id).await;
        }
    }

    // -------------------------------------------------------------- pump --

    fn pump(self: &Arc<Self>) {
        if self.cancel.is_cancelled() || !self.started.load(Ordering::SeqCst) {
            return;
        }
        let limit = self.concurrency();
        let mut running = self.running.lock();
        if running.len() >= limit {
            return;
        }
        let mut queued: Vec<TransferRow> = self
            .rows
            .lock()
            .values()
            .filter(|r| r.status == TransferStatus::Queued && !running.contains_key(&r.id))
            .cloned()
            .collect();
        queued.sort_by_key(|r| r.created_at);
        for mut row in queued.into_iter().take(limit - running.len()) {
            let token = self.cancel.child_token();
            running.insert(row.id.clone(), token.clone());
            row.status = TransferStatus::Running;
            row.error = None;
            if let Some(r) = self.rows.lock().get_mut(&row.id) {
                r.status = TransferStatus::Running;
                r.error = None;
            }
            self.live.lock().insert(row.id.clone(), Live { bytes: row.transferred_bytes, last_bytes: row.transferred_bytes, speed: 0.0 });
            let me = self.clone();
            self.rt.spawn(async move {
                let id = row.id.clone();
                let _ = me.db.set_status(&id, TransferStatus::Running, None, now_ms()).await;
                let inner = me.clone();
                let handle = tokio::spawn(async move { inner.run(row, token).await });
                if handle.await.is_err() {
                    // A crash inside a task → failed, never silent.
                    me.fail(&id, "Unexpected error".into()).await;
                }
                me.running.lock().remove(&id);
                me.live.lock().remove(&id);
                me.changed();
                me.pump();
            });
        }
        drop(running);
        self.changed();
    }

    async fn run(self: Arc<Self>, row: TransferRow, token: CancellationToken) {
        let mut attempt = row.retry_count;
        let mut reset_multipart_once = true;
        loop {
            attempt += 1;
            let current = self.rows.lock().get(&row.id).cloned().unwrap_or_else(|| row.clone());
            let res = match current.kind {
                TransferKind::Upload => self.do_upload(&current, &token).await.map(Some),
                TransferKind::Download => self.do_download(&current, &token).await.map(|_| None),
                _ => Err(AppFailure::unknown("unsupported transfer kind")),
            };
            match res {
                Ok(head) => {
                    self.complete(&row.id, head).await;
                    return;
                }
                Err(AppFailure::Cancelled) => return,
                Err(AppFailure::NotFound { .. })
                    if current.kind == TransferKind::Upload && current.multipart_upload_id.is_some() && reset_multipart_once =>
                {
                    // The stored multipart upload expired or was aborted: restart it.
                    reset_multipart_once = false;
                    self.update_row(&row.id, |r| {
                        r.multipart_upload_id = None;
                        r.completed_parts.clear();
                        r.transferred_bytes = 0;
                        true
                    });
                    let _ = self.db.set_upload_id(&row.id, None).await;
                    let _ = self.db.set_parts(&row.id, vec![], 0, now_ms()).await;
                }
                Err(e) if self.retry.should_retry(attempt, &e) => {
                    let _ = self.db.set_retry_count(&row.id, attempt).await;
                    self.update_row(&row.id, |r| {
                        r.retry_count = attempt;
                        true
                    });
                    let delay = self.retry.delay(attempt);
                    tokio::select! {
                        _ = token.cancelled() => return,
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
                Err(e) => {
                    log_warn!("transfer failed: {}", e.developer_message());
                    self.fail(&row.id, message_for(&e)).await;
                    if current.kind == TransferKind::Upload {
                        self.remove_owned_temp(&current.local_path).await;
                    }
                    return;
                }
            }
        }
    }

    fn progress_fn(self: &Arc<Self>, id: &str) -> uploader::ProgressFn {
        let me = Arc::downgrade(self);
        let id = id.to_string();
        Arc::new(move |abs| {
            if let Some(me) = me.upgrade() {
                if let Some(l) = me.live.lock().get_mut(&id) {
                    l.bytes = abs;
                }
                me.dirty.store(true, Ordering::Relaxed);
            }
        })
    }

    async fn do_upload(self: &Arc<Self>, row: &TransferRow, token: &CancellationToken) -> AppResult<HeadInfo> {
        let path = PathBuf::from(&row.local_path);
        let total = tokio::fs::metadata(&path).await.map_err(|e| AppFailure::io("stat upload source", e))?.len();
        if total != row.total_bytes {
            self.update_row(&row.id, |r| {
                r.total_bytes = total;
                true
            });
        }
        let client = self.client.get();
        let spec = UploadSpec {
            client: &client,
            key: &row.remote_key,
            path: &path,
            content_type: row.content_type.as_deref(),
            cancel: token,
            progress: self.progress_fn(&row.id),
        };
        if total <= SINGLE_PUT_MAX {
            uploader::upload_single(&spec, total).await?;
        } else {
            let sink = DbSink { engine: self.clone(), id: row.id.clone() };
            let resume = MultipartResume { upload_id: row.multipart_upload_id.clone(), parts: row.completed_parts.clone() };
            uploader::upload_multipart(&spec, total, resume, Some(&sink), false).await?;
        }
        // HEAD for the cache patch / auto-delete verification (best effort).
        Ok(client.head(&row.remote_key).await.unwrap_or(HeadInfo {
            size: total,
            etag: String::new(),
            content_type: row.content_type.clone(),
            last_modified_ms: Some(now_ms()),
        }))
    }

    /// Writes to `<path>.part`, resuming with `Range` and appending; renames
    /// atomically onto the final path on completion.
    async fn do_download(self: &Arc<Self>, row: &TransferRow, token: &CancellationToken) -> AppResult<()> {
        let dest = PathBuf::from(&row.local_path);
        if let Some(dir) = dest.parent() {
            tokio::fs::create_dir_all(dir).await.map_err(|e| AppFailure::io("create download dir", e))?;
        }
        let part = part_path(&dest);
        let mut existing = tokio::fs::metadata(&part).await.map(|m| m.len()).unwrap_or(0);
        let progress = self.progress_fn(&row.id);
        if existing > 0 && row.total_bytes > 0 && existing >= row.total_bytes {
            if existing > row.total_bytes {
                // Corrupt .part: start over.
                let _ = tokio::fs::remove_file(&part).await;
                existing = 0;
            } else {
                progress(existing);
                return finalize_download(&part, &dest).await;
            }
        }
        let client = self.client.get();
        let mut resp = client.get_stream(&row.remote_key, existing, Some(token)).await?;
        let resumed = existing > 0 && resp.status().as_u16() == 206;
        if existing > 0 && !resumed {
            existing = 0; // server ignored Range: rewrite from scratch
        }
        let len = resp.content_length().unwrap_or(0);
        let total = if resumed { existing + len } else { len };
        if total > 0 && total != row.total_bytes {
            self.update_row(&row.id, |r| {
                r.total_bytes = total;
                true
            });
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(resumed)
            .truncate(!resumed)
            .open(&part)
            .await
            .map_err(|e| AppFailure::io("open .part", e))?;
        let mut w = tokio::io::BufWriter::with_capacity(1 << 20, file);
        let mut done = existing;
        progress(done);
        loop {
            let chunk = tokio::select! {
                biased;
                _ = token.cancelled() => {
                    let _ = w.flush().await;
                    return Err(AppFailure::Cancelled);
                }
                c = resp.chunk() => c.map_err(|e| bf_data::r2::failure::from_transport(&e))?,
            };
            let Some(chunk) = chunk else { break };
            w.write_all(&chunk).await.map_err(|e| AppFailure::io("write download", e))?;
            done += chunk.len() as u64;
            progress(done);
        }
        w.flush().await.map_err(|e| AppFailure::io("flush download", e))?;
        let f = w.into_inner();
        f.sync_all().await.map_err(|e| AppFailure::io("sync download", e))?;
        drop(f);
        if total > 0 && done != total {
            return Err(AppFailure::network("download ended early"));
        }
        finalize_download(&part, &dest).await
    }

    async fn complete(self: &Arc<Self>, id: &str, head: Option<HeadInfo>) {
        let mut row = None;
        self.update_row(id, |r| {
            r.status = TransferStatus::Completed;
            r.transferred_bytes = r.total_bytes;
            r.error = None;
            row = Some(r.clone());
            true
        });
        let Some(row) = row else { return };
        let now = now_ms();
        let _ = self.db.set_progress(id, row.total_bytes, row.total_bytes, now).await;
        let _ = self.db.set_status(id, TransferStatus::Completed, None, now).await;
        match row.kind {
            TransferKind::Upload => {
                if let Some(h) = self.hooks() {
                    h.upload_completed(&row, head.as_ref());
                }
                let _ = self.events.send(TransferEvent::UploadCompleted(row.clone()));
                self.remove_owned_temp(&row.local_path).await;
                if row.delete_source_ref.is_some() {
                    self.verify_and_delete_source(&row).await;
                }
            }
            TransferKind::Download => {
                if let Some(h) = self.hooks() {
                    h.download_completed(&row);
                }
                let _ = self.events.send(TransferEvent::DownloadCompleted(row.clone()));
            }
            _ => {}
        }
        self.changed();
    }

    async fn fail(&self, id: &str, message: String) {
        let msg = message.clone();
        self.update_row(id, |r| {
            if r.status.is_terminal() || r.status == TransferStatus::Paused {
                return false;
            }
            r.status = TransferStatus::Failed;
            r.error = Some(msg.clone());
            true
        });
        let _ = self.db.set_status(id, TransferStatus::Failed, Some(message), now_ms()).await;
        self.changed();
    }

    /// Delete the original only once a HEAD shows the object at the uploaded
    /// size. One attempt per row; HEAD unreachable keeps the ref for the next
    /// launch. Every attempt is reported.
    async fn verify_and_delete_source(self: &Arc<Self>, row: &TransferRow) {
        let Some(src) = row.delete_source_ref.clone() else { return };
        let head = match self.client.get().head(&row.remote_key).await {
            Ok(h) => h,
            Err(e) if e.is_retryable() => return, // retry on next start()
            Err(_) => HeadInfo { size: u64::MAX, etag: String::new(), content_type: None, last_modified_ms: None },
        };
        let outcome = if head.size != row.total_bytes {
            SourceDeletionOutcome::Unverified
        } else {
            match tokio::fs::remove_file(&src).await {
                Ok(()) => SourceDeletionOutcome::Deleted,
                Err(_) => SourceDeletionOutcome::Failed,
            }
        };
        let _ = self.db.clear_delete_source_ref(&row.id).await;
        self.update_row(&row.id, |r| {
            r.delete_source_ref = None;
            true
        });
        let _ = self.deletions.send(SourceDeletion { path: src, remote_key: row.remote_key.clone(), outcome });
        self.changed();
    }

    async fn remove_owned_temp(&self, path: &str) {
        let p = Path::new(path);
        if p.starts_with(&self.owned_temp) {
            let _ = tokio::fs::remove_file(p).await;
        }
    }

    // ---------------------------------------------------------- progress --

    fn update_row(&self, id: &str, f: impl FnOnce(&mut TransferRow) -> bool) -> bool {
        let mut rows = self.rows.lock();
        match rows.get_mut(id) {
            Some(r) => {
                let changed = f(r);
                if changed {
                    r.updated_at = now_ms();
                }
                changed
            }
            None => false,
        }
    }

    fn changed(&self) {
        self.dirty.store(true, Ordering::Relaxed);
        self.publish();
        self.notifier.changed();
    }

    fn publish(&self) {
        let rows: Vec<TransferRow> = self.rows.lock().values().cloned().collect();
        let live = self.live.lock();
        let mut tasks: Vec<TransferView> = rows
            .into_iter()
            .map(|row| {
                let (bytes, speed) = match live.get(&row.id) {
                    Some(l) if row.status == TransferStatus::Running => (l.bytes, l.speed),
                    _ => (row.transferred_bytes, 0.0),
                };
                let eta_secs = (speed > 1.0 && row.total_bytes > bytes).then(|| ((row.total_bytes - bytes) as f64 / speed) as u64);
                TransferView { row, bytes, speed, eta_secs }
            })
            .collect();
        drop(live);
        tasks.sort_by(|a, b| {
            let rank = |s: TransferStatus| match s {
                TransferStatus::Running => 0,
                TransferStatus::Queued => 1,
                TransferStatus::Paused => 2,
                TransferStatus::Failed => 3,
                _ => 4,
            };
            rank(a.row.status).cmp(&rank(b.row.status)).then(b.row.created_at.cmp(&a.row.created_at))
        });
        let running = tasks.iter().filter(|t| t.row.status == TransferStatus::Running).count();
        let queued = tasks.iter().filter(|t| t.row.status == TransferStatus::Queued).count();
        let total_speed = tasks.iter().map(|t| t.speed).sum();
        *self.snapshot.write() = Arc::new(TransferSnapshot { tasks, running, queued, total_speed });
    }

    async fn ticker(self: Arc<Self>) {
        let mut last = Instant::now();
        let mut iv = tokio::time::interval(THROTTLE);
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = iv.tick() => {}
            }
            let dt = last.elapsed().as_secs_f64().max(0.001);
            last = Instant::now();
            let has_running = !self.running.lock().is_empty();
            if !has_running && !self.dirty.swap(false, Ordering::Relaxed) {
                continue;
            }
            self.dirty.store(false, Ordering::Relaxed);
            let updates: Vec<(String, u64)> = {
                let mut live = self.live.lock();
                live.iter_mut()
                    .map(|(id, l)| {
                        let inst = (l.bytes.saturating_sub(l.last_bytes)) as f64 / dt;
                        // Light smoothing so speed/ETA read less jittery.
                        l.speed = if l.speed == 0.0 { inst } else { l.speed * 0.6 + inst * 0.4 };
                        l.last_bytes = l.bytes;
                        (id.clone(), l.bytes)
                    })
                    .collect()
            };
            for (id, bytes) in &updates {
                let total = self.update_progress_mem(id, *bytes);
                let _ = self.db.set_progress(id, *bytes, total, now_ms()).await;
            }
            self.publish();
            self.notifier.changed();
        }
    }

    fn update_progress_mem(&self, id: &str, bytes: u64) -> u64 {
        let mut rows = self.rows.lock();
        match rows.get_mut(id) {
            Some(r) => {
                r.transferred_bytes = bytes.min(r.total_bytes.max(bytes));
                r.total_bytes
            }
            None => 0,
        }
    }

    /// Persist live progress (on lock/exit).
    pub async fn flush_progress(&self) {
        let updates: Vec<(String, u64)> = self.live.lock().iter().map(|(k, l)| (k.clone(), l.bytes)).collect();
        for (id, bytes) in updates {
            let total = self.update_progress_mem(&id, bytes);
            let _ = self.db.set_progress(&id, bytes, total, now_ms()).await;
        }
    }
}

struct DbSink {
    engine: Arc<TransferEngine>,
    id: String,
}

impl ResumeSink for DbSink {
    fn on_upload_id(&self, uid: String) -> BoxFuture<'_, AppResult<()>> {
        async move {
            self.engine.update_row(&self.id, |r| {
                r.multipart_upload_id = Some(uid.clone());
                true
            });
            self.engine.db.set_upload_id(&self.id, Some(uid)).await
        }
        .boxed()
    }
    fn on_parts(&self, parts: Vec<CompletedPart>, confirmed: u64) -> BoxFuture<'_, AppResult<()>> {
        async move {
            let p2 = parts.clone();
            self.engine.update_row(&self.id, |r| {
                r.completed_parts = p2;
                true
            });
            self.engine.db.set_parts(&self.id, parts, confirmed, now_ms()).await
        }
        .boxed()
    }
}

pub fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(".part");
    PathBuf::from(s)
}

async fn finalize_download(part: &Path, dest: &Path) -> AppResult<()> {
    tokio::fs::rename(part, dest).await.map_err(|e| AppFailure::io("finalize download", e))
}
