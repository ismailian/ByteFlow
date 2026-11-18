//! `r2vault.sqlite`: transfer queue, sync pairs + manifest, shared-link
//! records (spec §7.1, §11, §12). Wiped by the destruction wipe (step 3).

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::db::Db;
use crate::r2::CompletedPart;
use bf_core::AppResult;

fn migrate(c: &Connection) -> rusqlite::Result<()> {
    let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if v < 1 {
        c.execute_batch(
            "CREATE TABLE IF NOT EXISTS transfer_tasks (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                group_id TEXT,
                local_path TEXT NOT NULL,
                remote_key TEXT NOT NULL,
                total_bytes INTEGER NOT NULL DEFAULT 0,
                transferred_bytes INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL,
                error TEXT,
                content_type TEXT,
                multipart_upload_id TEXT,
                completed_parts TEXT NOT NULL DEFAULT '[]',
                retry_count INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS transfer_status ON transfer_tasks(status, created_at);
             CREATE TABLE IF NOT EXISTS sync_pairs (
                id TEXT PRIMARY KEY,
                local_dir_path TEXT NOT NULL,
                remote_prefix TEXT NOT NULL,
                last_scan_at INTEGER
             );
             CREATE TABLE IF NOT EXISTS sync_manifest (
                pair_id TEXT NOT NULL REFERENCES sync_pairs(id) ON DELETE CASCADE,
                relative_path TEXT NOT NULL,
                size INTEGER NOT NULL,
                mtime_ms INTEGER NOT NULL,
                last_uploaded_mtime_ms INTEGER,
                PRIMARY KEY (pair_id, relative_path)
             );
             CREATE TABLE IF NOT EXISTS shared_links (
                id TEXT PRIMARY KEY,
                remote_key TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL
             );
             PRAGMA user_version = 1;",
        )?;
    }
    if v < 2 {
        // Schema v2: source-delete handle (§8).
        c.execute_batch(
            "ALTER TABLE transfer_tasks ADD COLUMN delete_source_ref TEXT;
             PRAGMA user_version = 2;",
        )?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransferKind {
    Upload,
    Download,
    FolderUpload,
    FolderDownload,
    Move,
    Copy,
    Delete,
}

impl TransferKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Download => "download",
            Self::FolderUpload => "folderUpload",
            Self::FolderDownload => "folderDownload",
            Self::Move => "move",
            Self::Copy => "copy",
            Self::Delete => "delete",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "upload" => Self::Upload,
            "download" => Self::Download,
            "folderUpload" => Self::FolderUpload,
            "folderDownload" => Self::FolderDownload,
            "move" => Self::Move,
            "copy" => Self::Copy,
            _ => Self::Delete,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransferStatus {
    Queued,
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl TransferStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "paused" => Self::Paused,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            _ => Self::Cancelled,
        }
    }
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone)]
pub struct TransferRow {
    pub id: String,
    pub kind: TransferKind,
    pub group_id: Option<String>,
    pub local_path: String,
    pub remote_key: String,
    pub total_bytes: u64,
    pub transferred_bytes: u64,
    pub status: TransferStatus,
    pub error: Option<String>,
    pub content_type: Option<String>,
    pub multipart_upload_id: Option<String>,
    pub completed_parts: Vec<CompletedPart>,
    pub retry_count: u32,
    pub delete_source_ref: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

const TRANSFER_COLS: &str = "id, kind, group_id, local_path, remote_key, total_bytes, transferred_bytes, status, error, content_type, multipart_upload_id, completed_parts, retry_count, delete_source_ref, created_at, updated_at";

fn transfer_from(r: &Row<'_>) -> rusqlite::Result<TransferRow> {
    let parts: String = r.get(11)?;
    Ok(TransferRow {
        id: r.get(0)?,
        kind: TransferKind::parse(&r.get::<_, String>(1)?),
        group_id: r.get(2)?,
        local_path: r.get(3)?,
        remote_key: r.get(4)?,
        total_bytes: r.get::<_, i64>(5)? as u64,
        transferred_bytes: r.get::<_, i64>(6)? as u64,
        status: TransferStatus::parse(&r.get::<_, String>(7)?),
        error: r.get(8)?,
        content_type: r.get(9)?,
        multipart_upload_id: r.get(10)?,
        completed_parts: serde_json::from_str(&parts).unwrap_or_default(),
        retry_count: r.get::<_, i64>(12)? as u32,
        delete_source_ref: r.get(13)?,
        created_at: r.get(14)?,
        updated_at: r.get(15)?,
    })
}

#[derive(Debug, Clone)]
pub struct SyncPair {
    pub id: String,
    pub local_dir_path: String,
    pub remote_prefix: String,
    pub last_scan_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ManifestEntry {
    pub relative_path: String,
    pub size: u64,
    pub mtime_ms: i64,
    pub last_uploaded_mtime_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct SharedLink {
    pub id: String,
    pub remote_key: String,
    pub created_at: i64,
    pub expires_at: i64,
}

#[derive(Clone)]
pub struct AppDb {
    db: Db,
}

impl AppDb {
    pub fn open(path: &Path) -> AppResult<Self> {
        Ok(Self { db: Db::open(path, migrate)? })
    }
    pub fn open_in_memory() -> AppResult<Self> {
        Ok(Self { db: Db::open_in_memory(migrate)? })
    }

    /// Close the file (swap to in-memory) so it can be deleted (wipe step 3).
    pub fn close(&self) -> AppResult<()> {
        self.db.swap_to(None, migrate)
    }

    /// Reopen a fresh file after a wipe.
    pub fn reopen(&self, path: &Path) -> AppResult<()> {
        self.db.swap_to(Some(path), migrate)
    }

    // -------------------------------------------------------- transfers --

    pub async fn insert_transfer(&self, t: TransferRow) -> AppResult<()> {
        self.db
            .call(move |c| {
                c.execute(
                    &format!("INSERT INTO transfer_tasks ({TRANSFER_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)"),
                    params![
                        t.id, t.kind.as_str(), t.group_id, t.local_path, t.remote_key, t.total_bytes as i64,
                        t.transferred_bytes as i64, t.status.as_str(), t.error, t.content_type,
                        t.multipart_upload_id, serde_json::to_string(&t.completed_parts).unwrap_or_else(|_| "[]".into()),
                        t.retry_count as i64, t.delete_source_ref, t.created_at, t.updated_at
                    ],
                )
                .map(|_| ())
            })
            .await
    }

    pub async fn transfers(&self) -> AppResult<Vec<TransferRow>> {
        self.db
            .call(|c| {
                let mut s = c.prepare_cached(&format!("SELECT {TRANSFER_COLS} FROM transfer_tasks ORDER BY created_at"))?;
                s.query_map([], transfer_from)?.collect()
            })
            .await
    }

    pub async fn transfer(&self, id: &str) -> AppResult<Option<TransferRow>> {
        let id = id.to_string();
        self.db
            .call(move |c| {
                c.query_row(&format!("SELECT {TRANSFER_COLS} FROM transfer_tasks WHERE id = ?1"), [id], transfer_from)
                    .optional()
            })
            .await
    }

    pub async fn set_status(&self, id: &str, status: TransferStatus, error: Option<String>, now: i64) -> AppResult<()> {
        let id = id.to_string();
        self.db
            .call(move |c| {
                c.execute(
                    "UPDATE transfer_tasks SET status = ?2, error = ?3, updated_at = ?4 WHERE id = ?1",
                    params![id, status.as_str(), error, now],
                )
                .map(|_| ())
            })
            .await
    }

    pub async fn set_progress(&self, id: &str, transferred: u64, total: u64, now: i64) -> AppResult<()> {
        let id = id.to_string();
        self.db
            .call(move |c| {
                c.execute(
                    "UPDATE transfer_tasks SET transferred_bytes = ?2, total_bytes = ?3, updated_at = ?4 WHERE id = ?1",
                    params![id, transferred as i64, total as i64, now],
                )
                .map(|_| ())
            })
            .await
    }

    /// Persisted the moment it's created (§7.2).
    pub async fn set_upload_id(&self, id: &str, upload_id: Option<String>) -> AppResult<()> {
        let id = id.to_string();
        self.db
            .call(move |c| {
                c.execute("UPDATE transfer_tasks SET multipart_upload_id = ?2 WHERE id = ?1", params![id, upload_id])
                    .map(|_| ())
            })
            .await
    }

    /// Persisted after every part, together with progress.
    pub async fn set_parts(&self, id: &str, parts: Vec<CompletedPart>, transferred: u64, now: i64) -> AppResult<()> {
        let id = id.to_string();
        let json = serde_json::to_string(&parts).unwrap_or_else(|_| "[]".into());
        self.db
            .call(move |c| {
                c.execute(
                    "UPDATE transfer_tasks SET completed_parts = ?2, transferred_bytes = ?3, updated_at = ?4 WHERE id = ?1",
                    params![id, json, transferred as i64, now],
                )
                .map(|_| ())
            })
            .await
    }

    pub async fn set_retry_count(&self, id: &str, n: u32) -> AppResult<()> {
        let id = id.to_string();
        self.db
            .call(move |c| c.execute("UPDATE transfer_tasks SET retry_count = ?2 WHERE id = ?1", params![id, n as i64]).map(|_| ()))
            .await
    }

    pub async fn clear_delete_source_ref(&self, id: &str) -> AppResult<()> {
        let id = id.to_string();
        self.db
            .call(move |c| c.execute("UPDATE transfer_tasks SET delete_source_ref = NULL WHERE id = ?1", [id]).map(|_| ()))
            .await
    }

    /// Launch recovery: `queued`/`running` → `queued`.
    pub async fn reset_interrupted(&self) -> AppResult<usize> {
        self.db
            .call(|c| c.execute("UPDATE transfer_tasks SET status = 'queued' WHERE status IN ('queued','running')", []))
            .await
    }

    pub async fn delete_transfer(&self, id: &str) -> AppResult<()> {
        let id = id.to_string();
        self.db.call(move |c| c.execute("DELETE FROM transfer_tasks WHERE id = ?1", [id]).map(|_| ())).await
    }

    pub async fn clear_finished(&self) -> AppResult<usize> {
        self.db
            .call(|c| {
                c.execute(
                    "DELETE FROM transfer_tasks WHERE status IN ('completed','cancelled','failed') AND delete_source_ref IS NULL",
                    [],
                )
            })
            .await
    }

    // ------------------------------------------------------------- sync --

    pub async fn sync_pairs(&self) -> AppResult<Vec<SyncPair>> {
        self.db
            .call(|c| {
                let mut s = c.prepare_cached("SELECT id, local_dir_path, remote_prefix, last_scan_at FROM sync_pairs ORDER BY local_dir_path")?;
                s.query_map([], |r| {
                    Ok(SyncPair { id: r.get(0)?, local_dir_path: r.get(1)?, remote_prefix: r.get(2)?, last_scan_at: r.get(3)? })
                })?
                .collect()
            })
            .await
    }

    pub async fn insert_sync_pair(&self, p: SyncPair) -> AppResult<()> {
        self.db
            .call(move |c| {
                c.execute(
                    "INSERT INTO sync_pairs (id, local_dir_path, remote_prefix, last_scan_at) VALUES (?1,?2,?3,?4)",
                    params![p.id, p.local_dir_path, p.remote_prefix, p.last_scan_at],
                )
                .map(|_| ())
            })
            .await
    }

    /// Deleting a pair deletes its manifest (FK cascade + explicit delete).
    pub async fn delete_sync_pair(&self, id: &str) -> AppResult<()> {
        let id = id.to_string();
        self.db
            .call(move |c| {
                let tx = c.transaction()?;
                tx.execute("DELETE FROM sync_manifest WHERE pair_id = ?1", [&id])?;
                tx.execute("DELETE FROM sync_pairs WHERE id = ?1", [&id])?;
                tx.commit()
            })
            .await
    }

    pub async fn set_pair_scanned(&self, id: &str, at: i64) -> AppResult<()> {
        let id = id.to_string();
        self.db
            .call(move |c| c.execute("UPDATE sync_pairs SET last_scan_at = ?2 WHERE id = ?1", params![id, at]).map(|_| ()))
            .await
    }

    pub async fn manifest(&self, pair_id: &str) -> AppResult<Vec<ManifestEntry>> {
        let id = pair_id.to_string();
        self.db
            .call(move |c| {
                let mut s = c.prepare_cached(
                    "SELECT relative_path, size, mtime_ms, last_uploaded_mtime_ms FROM sync_manifest WHERE pair_id = ?1",
                )?;
                s.query_map([id], |r| {
                    Ok(ManifestEntry {
                        relative_path: r.get(0)?,
                        size: r.get::<_, i64>(1)? as u64,
                        mtime_ms: r.get(2)?,
                        last_uploaded_mtime_ms: r.get(3)?,
                    })
                })?
                .collect()
            })
            .await
    }

    pub async fn record_uploaded(&self, pair_id: &str, rel: &str, size: u64, mtime_ms: i64) -> AppResult<()> {
        let (id, rel) = (pair_id.to_string(), rel.to_string());
        self.db
            .call(move |c| {
                c.execute(
                    "INSERT INTO sync_manifest (pair_id, relative_path, size, mtime_ms, last_uploaded_mtime_ms)
                     VALUES (?1,?2,?3,?4,?4)
                     ON CONFLICT(pair_id, relative_path) DO UPDATE SET size = ?3, mtime_ms = ?4, last_uploaded_mtime_ms = ?4",
                    params![id, rel, size as i64, mtime_ms],
                )
                .map(|_| ())
            })
            .await
    }

    // ------------------------------------------------------ shared links --

    pub async fn insert_link(&self, l: SharedLink) -> AppResult<()> {
        self.db
            .call(move |c| {
                c.execute(
                    "INSERT INTO shared_links (id, remote_key, created_at, expires_at) VALUES (?1,?2,?3,?4)",
                    params![l.id, l.remote_key, l.created_at, l.expires_at],
                )
                .map(|_| ())
            })
            .await
    }

    pub async fn links(&self) -> AppResult<Vec<SharedLink>> {
        self.db
            .call(|c| {
                let mut s = c.prepare_cached("SELECT id, remote_key, created_at, expires_at FROM shared_links ORDER BY created_at DESC")?;
                s.query_map([], |r| Ok(SharedLink { id: r.get(0)?, remote_key: r.get(1)?, created_at: r.get(2)?, expires_at: r.get(3)? }))?
                    .collect()
            })
            .await
    }

    pub async fn delete_link(&self, id: &str) -> AppResult<()> {
        let id = id.to_string();
        self.db.call(move |c| c.execute("DELETE FROM shared_links WHERE id = ?1", [id]).map(|_| ())).await
    }

    pub async fn delete_expired_links(&self, now: i64) -> AppResult<usize> {
        self.db.call(move |c| c.execute("DELETE FROM shared_links WHERE expires_at < ?1", [now])).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str) -> TransferRow {
        TransferRow {
            id: id.into(),
            kind: TransferKind::Upload,
            group_id: None,
            local_path: "/tmp/a".into(),
            remote_key: "a".into(),
            total_bytes: 10,
            transferred_bytes: 0,
            status: TransferStatus::Running,
            error: None,
            content_type: None,
            multipart_upload_id: None,
            completed_parts: vec![],
            retry_count: 0,
            delete_source_ref: Some("/tmp/a".into()),
            created_at: 1,
            updated_at: 1,
        }
    }

    #[tokio::test]
    async fn transfers_and_recovery() {
        let db = AppDb::open_in_memory().unwrap();
        db.insert_transfer(row("t1")).await.unwrap();
        db.set_parts("t1", vec![CompletedPart { part_number: 1, etag: "e".into() }], 5, 2).await.unwrap();
        assert_eq!(db.reset_interrupted().await.unwrap(), 1);
        let t = db.transfer("t1").await.unwrap().unwrap();
        assert_eq!(t.status, TransferStatus::Queued);
        assert_eq!(t.completed_parts.len(), 1);
        assert_eq!(t.transferred_bytes, 5);
        assert_eq!(t.delete_source_ref.as_deref(), Some("/tmp/a"));
    }

    #[tokio::test]
    async fn sync_manifest_cascade() {
        let db = AppDb::open_in_memory().unwrap();
        db.insert_sync_pair(SyncPair { id: "p".into(), local_dir_path: "/d".into(), remote_prefix: "r/".into(), last_scan_at: None }).await.unwrap();
        db.record_uploaded("p", "a/b.txt", 3, 100).await.unwrap();
        assert_eq!(db.manifest("p").await.unwrap().len(), 1);
        db.delete_sync_pair("p").await.unwrap();
        assert!(db.manifest("p").await.unwrap().is_empty());
    }
}
