//! A SQLite connection whose every query runs on tokio's blocking pool, so the
//! UI thread never touches SQLite (the desktop equivalent of drift's
//! `createInBackground`, §15).

use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::Connection;

use bf_core::{AppFailure, AppResult};

pub type Migrate = fn(&Connection) -> rusqlite::Result<()>;

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

pub fn sql_err(e: rusqlite::Error) -> AppFailure {
    match e {
        rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::DiskFull => {
            AppFailure::StorageFull { message: "database disk full".into() }
        }
        other => AppFailure::unknown(format!("database: {other}")),
    }
}

impl Db {
    pub fn open(path: &Path, migrate: Migrate) -> AppResult<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| AppFailure::io("create db dir", e))?;
        }
        let conn = Connection::open(path).map_err(sql_err)?;
        Self::init(conn, migrate)
    }

    pub fn open_in_memory(migrate: Migrate) -> AppResult<Self> {
        Self::init(Connection::open_in_memory().map_err(sql_err)?, migrate)
    }

    fn init(conn: Connection, migrate: Migrate) -> AppResult<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             PRAGMA temp_store = MEMORY;",
        )
        .map_err(sql_err)?;
        migrate(&conn).map_err(sql_err)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    /// Swap the underlying connection in place. Every clone of this `Db`
    /// follows, so the old file handle is closed even while other components
    /// still hold the `Db` (needed to delete the file during a wipe).
    pub fn swap_to(&self, path: Option<&Path>, migrate: Migrate) -> AppResult<()> {
        let conn = match path {
            Some(p) => {
                if let Some(dir) = p.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| AppFailure::io("create db dir", e))?;
                }
                Connection::open(p).map_err(sql_err)?
            }
            None => Connection::open_in_memory().map_err(sql_err)?,
        };
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;")
            .map_err(sql_err)?;
        migrate(&conn).map_err(sql_err)?;
        let old = std::mem::replace(&mut *self.conn.lock(), conn);
        drop(old);
        Ok(())
    }

    pub async fn call<R, F>(&self, f: F) -> AppResult<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Connection) -> rusqlite::Result<R> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || f(&mut conn.lock()).map_err(sql_err))
            .await
            .map_err(|_| AppFailure::unknown("database task panicked"))?
    }

    /// Synchronous variant for code already on a blocking thread.
    pub fn call_blocking<R>(&self, f: impl FnOnce(&mut Connection) -> rusqlite::Result<R>) -> AppResult<R> {
        f(&mut self.conn.lock()).map_err(sql_err)
    }
}
