//! The bucket-listing cache (spec §5) — a separate database, never merged with
//! the app DB. Browsing, stats, search, sort and counts are free local
//! queries against it.
//!
//! Prefix queries use a **key range** `[prefix, prefix + U+10FFFF)` rather
//! than `LIKE`, so `%`/`_` in a folder name can't widen the match.

use std::path::{Path, PathBuf};

use parking_lot::RwLock;
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::db::Db;
use bf_core::AppResult;

const MAX_CHAR: char = '\u{10FFFF}';

fn migrate(c: &Connection) -> rusqlite::Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS cached_entries (
            key TEXT PRIMARY KEY,
            is_folder INTEGER NOT NULL,
            parent_prefix TEXT NOT NULL,
            size INTEGER NOT NULL DEFAULT 0,
            etag TEXT,
            last_modified_ms INTEGER,
            encrypted INTEGER NOT NULL DEFAULT 0,
            key_epoch INTEGER
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS cached_parent ON cached_entries(parent_prefix, is_folder);
         CREATE INDEX IF NOT EXISTS cached_modified ON cached_entries(last_modified_ms);
         CREATE TABLE IF NOT EXISTS cache_meta (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            last_scan_started_at_ms INTEGER,
            last_scan_completed_at_ms INTEGER,
            last_scan_error TEXT
         );
         INSERT OR IGNORE INTO cache_meta (id) VALUES (1);",
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    pub key: String,
    pub is_folder: bool,
    pub parent_prefix: String,
    pub size: u64,
    pub etag: Option<String>,
    pub last_modified_ms: Option<i64>,
    pub encrypted: bool,
    pub key_epoch: Option<i64>,
}

impl CacheEntry {
    pub fn file(key: &str, size: u64, etag: Option<String>, last_modified_ms: Option<i64>) -> Self {
        Self {
            key: key.to_string(),
            is_folder: false,
            parent_prefix: bf_core::media::parent_prefix(key).to_string(),
            size,
            etag,
            last_modified_ms,
            encrypted: false,
            key_epoch: None,
        }
    }
    pub fn folder(key: &str) -> Self {
        Self {
            key: key.to_string(),
            is_folder: true,
            parent_prefix: bf_core::media::parent_prefix(key).to_string(),
            size: 0,
            etag: None,
            last_modified_ms: None,
            encrypted: false,
            key_epoch: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CacheMeta {
    pub last_scan_started_at_ms: Option<i64>,
    pub last_scan_completed_at_ms: Option<i64>,
    pub last_scan_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub files: u64,
    pub bytes: u64,
}

const COLS: &str = "key, is_folder, parent_prefix, size, etag, last_modified_ms, encrypted, key_epoch";

fn entry_from(r: &Row<'_>) -> rusqlite::Result<CacheEntry> {
    Ok(CacheEntry {
        key: r.get(0)?,
        is_folder: r.get::<_, i64>(1)? != 0,
        parent_prefix: r.get(2)?,
        size: r.get::<_, i64>(3)? as u64,
        etag: r.get(4)?,
        last_modified_ms: r.get(5)?,
        encrypted: r.get::<_, i64>(6)? != 0,
        key_epoch: r.get(7)?,
    })
}

fn upper(prefix: &str) -> String {
    format!("{prefix}{MAX_CHAR}")
}

fn upsert(c: &Connection, e: &CacheEntry) -> rusqlite::Result<()> {
    c.prepare_cached(&format!(
        "INSERT INTO cached_entries ({COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(key) DO UPDATE SET is_folder=?2, parent_prefix=?3, size=?4, etag=?5, last_modified_ms=?6, encrypted=?7, key_epoch=?8"
    ))?
    .execute(params![
        e.key,
        e.is_folder as i64,
        e.parent_prefix,
        e.size as i64,
        e.etag,
        e.last_modified_ms,
        e.encrypted as i64,
        e.key_epoch
    ])
    .map(|_| ())
}

/// Reopenable: the location is a preference (§5.6).
pub struct CacheDb {
    inner: RwLock<(Db, PathBuf)>,
}

impl CacheDb {
    pub fn open(path: &Path) -> AppResult<Self> {
        Ok(Self { inner: RwLock::new((Db::open(path, migrate)?, path.to_path_buf())) })
    }

    pub fn open_in_memory() -> AppResult<Self> {
        Ok(Self { inner: RwLock::new((Db::open_in_memory(migrate)?, PathBuf::new())) })
    }

    /// Close and reopen at a new location; an existing DB there is re-adopted.
    pub fn reopen(&self, path: &Path) -> AppResult<()> {
        let db = Db::open(path, migrate)?;
        *self.inner.write() = (db, path.to_path_buf());
        Ok(())
    }

    /// Close the file handle (swap to in-memory) so its directory can be
    /// emptied (wipe step 4). Call [`Self::reopen`] afterwards.
    pub fn close(&self) -> AppResult<()> {
        self.inner.read().0.swap_to(None, migrate)
    }

    pub fn path(&self) -> PathBuf {
        self.inner.read().1.clone()
    }

    fn db(&self) -> Db {
        self.inner.read().0.clone()
    }

    // ---------------------------------------------------------- queries --

    pub async fn children_of(&self, prefix: &str) -> AppResult<Vec<CacheEntry>> {
        let p = prefix.to_string();
        self.db()
            .call(move |c| {
                let mut s = c.prepare_cached(&format!(
                    "SELECT {COLS} FROM cached_entries WHERE parent_prefix = ?1 AND key <> ?1"
                ))?;
                s.query_map([p], entry_from)?.collect()
            })
            .await
    }

    pub async fn entry_by_key(&self, key: &str) -> AppResult<Option<CacheEntry>> {
        let k = key.to_string();
        self.db()
            .call(move |c| c.query_row(&format!("SELECT {COLS} FROM cached_entries WHERE key = ?1"), [k], entry_from).optional())
            .await
    }

    /// Every object (non-folder) key under a prefix, recursively.
    pub async fn object_keys_under(&self, prefix: &str) -> AppResult<Vec<String>> {
        let (lo, hi) = (prefix.to_string(), upper(prefix));
        self.db()
            .call(move |c| {
                let mut s = c.prepare_cached(
                    "SELECT key FROM cached_entries WHERE key >= ?1 AND key < ?2 AND is_folder = 0 ORDER BY key",
                )?;
                s.query_map([lo, hi], |r| r.get(0))?.collect()
            })
            .await
    }

    pub async fn all_entries(&self) -> AppResult<Vec<CacheEntry>> {
        self.db()
            .call(|c| {
                let mut s = c.prepare_cached(&format!("SELECT {COLS} FROM cached_entries"))?;
                s.query_map([], entry_from)?.collect()
            })
            .await
    }

    /// Recursive files + bytes under `prefix` (key range, not LIKE).
    pub async fn folder_usage(&self, prefix: &str) -> AppResult<Usage> {
        let (lo, hi) = (prefix.to_string(), upper(prefix));
        self.db()
            .call(move |c| {
                c.query_row(
                    "SELECT COUNT(*), COALESCE(SUM(size),0) FROM cached_entries
                     WHERE key >= ?1 AND key < ?2 AND is_folder = 0",
                    [lo, hi],
                    |r| Ok(Usage { files: r.get::<_, i64>(0)? as u64, bytes: r.get::<_, i64>(1)? as u64 }),
                )
            })
            .await
    }

    /// Recursive usage for many folders in one blocking hop.
    pub async fn folder_usages(&self, prefixes: Vec<String>) -> AppResult<Vec<Usage>> {
        self.db()
            .call(move |c| {
                let mut s = c.prepare_cached(
                    "SELECT COUNT(*), COALESCE(SUM(size),0) FROM cached_entries WHERE key >= ?1 AND key < ?2 AND is_folder = 0",
                )?;
                prefixes
                    .iter()
                    .map(|p| {
                        s.query_row([p.clone(), upper(p)], |r| {
                            Ok(Usage { files: r.get::<_, i64>(0)? as u64, bytes: r.get::<_, i64>(1)? as u64 })
                        })
                    })
                    .collect()
            })
            .await
    }

    /// Non-recursive file count.
    pub async fn file_count_under(&self, prefix: &str) -> AppResult<u64> {
        let p = prefix.to_string();
        self.db()
            .call(move |c| {
                c.query_row("SELECT COUNT(*) FROM cached_entries WHERE parent_prefix = ?1 AND is_folder = 0", [p], |r| {
                    r.get::<_, i64>(0).map(|n| n as u64)
                })
            })
            .await
    }

    pub async fn root_folders(&self) -> AppResult<Vec<String>> {
        self.db()
            .call(|c| {
                let mut s = c.prepare_cached("SELECT key FROM cached_entries WHERE parent_prefix = '' AND is_folder = 1")?;
                s.query_map([], |r| r.get(0))?.collect()
            })
            .await
    }

    /// Whole-bucket totals: (files, folders, bytes).
    pub async fn totals(&self) -> AppResult<(u64, u64, u64)> {
        self.db()
            .call(|c| {
                c.query_row(
                    "SELECT COALESCE(SUM(is_folder = 0),0), COALESCE(SUM(is_folder = 1),0), COALESCE(SUM(CASE WHEN is_folder = 0 THEN size ELSE 0 END),0) FROM cached_entries",
                    [],
                    |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as u64)),
                )
            })
            .await
    }

    pub async fn object_timestamps_since(&self, since_ms: i64) -> AppResult<Vec<i64>> {
        self.db()
            .call(move |c| {
                let mut s = c.prepare_cached(
                    "SELECT last_modified_ms FROM cached_entries WHERE is_folder = 0 AND last_modified_ms >= ?1",
                )?;
                s.query_map([since_ms], |r| r.get(0))?.collect()
            })
            .await
    }

    /// Two columns only, for the storage-by-type breakdown.
    pub async fn file_key_sizes(&self) -> AppResult<Vec<(String, u64)>> {
        self.db()
            .call(|c| {
                let mut s = c.prepare_cached("SELECT key, size FROM cached_entries WHERE is_folder = 0")?;
                s.query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?.collect()
            })
            .await
    }

    /// Newest first; undated objects sort last.
    pub async fn recent_files(&self, limit: u32) -> AppResult<Vec<CacheEntry>> {
        self.db()
            .call(move |c| {
                let mut s = c.prepare_cached(&format!(
                    "SELECT {COLS} FROM cached_entries WHERE is_folder = 0
                     ORDER BY last_modified_ms IS NULL, last_modified_ms DESC LIMIT ?1"
                ))?;
                s.query_map([limit], entry_from)?.collect()
            })
            .await
    }

    pub async fn meta(&self) -> AppResult<CacheMeta> {
        self.db()
            .call(|c| {
                c.query_row(
                    "SELECT last_scan_started_at_ms, last_scan_completed_at_ms, last_scan_error FROM cache_meta WHERE id = 1",
                    [],
                    |r| Ok(CacheMeta { last_scan_started_at_ms: r.get(0)?, last_scan_completed_at_ms: r.get(1)?, last_scan_error: r.get(2)? }),
                )
            })
            .await
    }

    pub async fn has_scanned(&self) -> AppResult<bool> {
        Ok(self.meta().await?.last_scan_completed_at_ms.is_some())
    }

    // -------------------------------------------------------- mutations --

    pub async fn upsert_entry(&self, e: CacheEntry) -> AppResult<()> {
        self.db().call(move |c| upsert(c, &e)).await
    }

    pub async fn upsert_entries(&self, es: Vec<CacheEntry>) -> AppResult<()> {
        self.db()
            .call(move |c| {
                let tx = c.transaction()?;
                for e in &es {
                    upsert(&tx, e)?;
                }
                tx.commit()
            })
            .await
    }

    pub async fn delete_entry(&self, key: &str) -> AppResult<()> {
        let k = key.to_string();
        self.db().call(move |c| c.execute("DELETE FROM cached_entries WHERE key = ?1", [k]).map(|_| ())).await
    }

    pub async fn delete_entries(&self, keys: Vec<String>) -> AppResult<()> {
        self.db()
            .call(move |c| {
                let tx = c.transaction()?;
                {
                    let mut s = tx.prepare_cached("DELETE FROM cached_entries WHERE key = ?1")?;
                    for k in &keys {
                        s.execute([k])?;
                    }
                }
                tx.commit()
            })
            .await
    }

    pub async fn delete_entry_and_descendants(&self, prefix: &str) -> AppResult<()> {
        let (lo, hi) = (prefix.to_string(), upper(prefix));
        self.db()
            .call(move |c| c.execute("DELETE FROM cached_entries WHERE key >= ?1 AND key < ?2", [lo, hi]).map(|_| ()))
            .await
    }

    pub async fn rename_entry(&self, from: &str, to: &str) -> AppResult<()> {
        let (f, t) = (from.to_string(), to.to_string());
        self.db()
            .call(move |c| {
                let tx = c.transaction()?;
                tx.execute("DELETE FROM cached_entries WHERE key = ?1", [&t])?;
                tx.execute(
                    "UPDATE cached_entries SET key = ?2, parent_prefix = ?3 WHERE key = ?1",
                    params![f, t, bf_core::media::parent_prefix(&t)],
                )?;
                tx.commit()
            })
            .await
    }

    /// Rewrite every key under `from` to live under `to`, including
    /// descendants' `parent_prefix`.
    pub async fn rename_prefix(&self, from: &str, to: &str) -> AppResult<()> {
        let (f, t) = (from.to_string(), to.to_string());
        self.db()
            .call(move |c| {
                let tx = c.transaction()?;
                let rows: Vec<CacheEntry> = {
                    let mut s = tx.prepare(&format!("SELECT {COLS} FROM cached_entries WHERE key >= ?1 AND key < ?2"))?;
                    s.query_map([f.clone(), upper(&f)], entry_from)?.collect::<rusqlite::Result<_>>()?
                };
                tx.execute("DELETE FROM cached_entries WHERE key >= ?1 AND key < ?2", [f.clone(), upper(&f)])?;
                for mut e in rows {
                    e.key = format!("{t}{}", &e.key[f.len()..]);
                    e.parent_prefix = bf_core::media::parent_prefix(&e.key).to_string();
                    upsert(&tx, &e)?;
                }
                tx.commit()
            })
            .await
    }

    pub async fn set_encrypted(&self, key: &str, encrypted: bool, epoch: Option<i64>) -> AppResult<()> {
        let k = key.to_string();
        self.db()
            .call(move |c| {
                c.execute(
                    "UPDATE cached_entries SET encrypted = ?2, key_epoch = ?3 WHERE key = ?1",
                    params![k, encrypted as i64, epoch],
                )
                .map(|_| ())
            })
            .await
    }

    /// Single transaction: a reader never sees a half-applied scan.
    pub async fn replace_all(&self, es: Vec<CacheEntry>, completed_at_ms: i64) -> AppResult<()> {
        self.db()
            .call(move |c| {
                let tx = c.transaction()?;
                tx.execute("DELETE FROM cached_entries", [])?;
                {
                    let mut s = tx.prepare_cached(&format!("INSERT OR REPLACE INTO cached_entries ({COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)"))?;
                    for e in &es {
                        s.execute(params![
                            e.key, e.is_folder as i64, e.parent_prefix, e.size as i64, e.etag,
                            e.last_modified_ms, e.encrypted as i64, e.key_epoch
                        ])?;
                    }
                }
                tx.execute(
                    "UPDATE cache_meta SET last_scan_completed_at_ms = ?1, last_scan_error = NULL WHERE id = 1",
                    [completed_at_ms],
                )?;
                tx.commit()
            })
            .await
    }

    pub async fn record_scan_started(&self, at: i64) -> AppResult<()> {
        self.db().call(move |c| c.execute("UPDATE cache_meta SET last_scan_started_at_ms = ?1 WHERE id = 1", [at]).map(|_| ())).await
    }

    pub async fn record_scan_error(&self, msg: String) -> AppResult<()> {
        self.db().call(move |c| c.execute("UPDATE cache_meta SET last_scan_error = ?1 WHERE id = 1", [msg]).map(|_| ())).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn queries_and_key_range() {
        let db = CacheDb::open_in_memory().unwrap();
        db.replace_all(
            vec![
                CacheEntry::folder("a%/"),
                CacheEntry::file("a%/x.txt", 10, None, Some(5)),
                CacheEntry::folder("a%/sub/"),
                CacheEntry::file("a%/sub/y.jpg", 20, None, Some(7)),
                CacheEntry::folder("ab/"),
                CacheEntry::file("ab/z.txt", 100, None, None),
                CacheEntry::file("root.bin", 1, None, Some(9)),
            ],
            1,
        )
        .await
        .unwrap();
        assert!(db.has_scanned().await.unwrap());
        // `%` must not widen to `ab/`.
        assert_eq!(db.folder_usage("a%/").await.unwrap(), Usage { files: 2, bytes: 30 });
        assert_eq!(db.children_of("a%/").await.unwrap().len(), 2);
        assert_eq!(db.file_count_under("a%/").await.unwrap(), 1);
        assert_eq!(db.totals().await.unwrap(), (4, 3, 131));
        let recent = db.recent_files(10).await.unwrap();
        assert_eq!(recent.first().unwrap().key, "root.bin");
        assert_eq!(recent.last().unwrap().key, "ab/z.txt", "undated last");

        db.rename_prefix("a%/", "moved/").await.unwrap();
        assert_eq!(db.object_keys_under("moved/").await.unwrap(), vec!["moved/sub/y.jpg", "moved/x.txt"]);
        assert_eq!(db.entry_by_key("moved/sub/y.jpg").await.unwrap().unwrap().parent_prefix, "moved/sub/");
        db.delete_entry_and_descendants("moved/").await.unwrap();
        assert_eq!(db.totals().await.unwrap(), (2, 1, 101));
        db.rename_entry("root.bin", "ab/root.bin").await.unwrap();
        assert_eq!(db.file_count_under("ab/").await.unwrap(), 2);
    }
}
