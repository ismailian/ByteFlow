//! On-disk locations (spec §2.5, §17.1).
//!
//! **[DESKTOP decision]** Everything app-private lives under the OS *local*
//! (non-roaming) data dir — `%LOCALAPPDATA%\ByteFlow` on Windows — so nothing
//! roams to other machines in managed environments (§3.7, §17.1). The
//! user's Documents folder is never used and never wiped.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct AppPaths {
    /// App-support root (wiped, step 4).
    pub support: PathBuf,
    /// App cache root (wiped, step 4).
    pub cache: PathBuf,
    /// App temp root (wiped, step 4): `enc_work`, `enc_preview`, `enc_upload`.
    pub temp: PathBuf,
}

impl AppPaths {
    pub fn system() -> std::io::Result<Self> {
        let local = dirs::data_local_dir()
            .ok_or_else(|| std::io::Error::other("no local data directory"))?;
        let root = local.join("ByteFlow");
        let cache = dirs::cache_dir().map(|c| c.join("ByteFlow")).unwrap_or_else(|| root.join("cache"));
        // On Windows `cache_dir()` == local data dir; keep the cache inside the
        // app root so the two can't alias.
        let cache = if cache == root { root.join("cache") } else { cache };
        Self::at(root.join("support"), cache, std::env::temp_dir().join("byteflow"))
    }

    pub fn at(support: PathBuf, cache: PathBuf, temp: PathBuf) -> std::io::Result<Self> {
        for d in [&support, &cache, &temp] {
            std::fs::create_dir_all(d)?;
        }
        Ok(Self { support, cache, temp })
    }

    pub fn app_db(&self) -> PathBuf {
        self.support.join("r2vault.sqlite")
    }
    pub fn default_cache_db_dir(&self) -> PathBuf {
        self.support.clone()
    }
    pub fn encryption_state(&self) -> PathBuf {
        self.support.join("encryption_state.json")
    }
    pub fn thumbs(&self) -> PathBuf {
        self.support.join("thumbs")
    }
    /// A secondary bucket's private folder (multi-bucket, see
    /// `bf_app::buckets`). The primary bucket uses the support root itself.
    pub fn bucket_dir(&self, id: &str) -> PathBuf {
        self.support.join("buckets").join(id)
    }
    pub fn secure_store(&self) -> PathBuf {
        self.support.join("secure_store.bin")
    }
    pub fn instance_lock(&self) -> PathBuf {
        self.support.join("instance.lock")
    }
    pub fn temp_sub(&self, name: &str) -> PathBuf {
        let p = self.temp.join(name);
        let _ = std::fs::create_dir_all(&p);
        p
    }
    /// The directories wipe step 4 empties.
    pub fn scratch_directories(&self) -> Vec<&Path> {
        vec![self.temp.as_path(), self.support.as_path(), self.cache.as_path()]
    }
}

pub const CACHE_DB_FILE: &str = "bucket_cache.sqlite";

/// Default download folder on desktop: the OS Downloads folder
/// (**[DESKTOP decision]**, §17.1), falling back to home.
pub fn default_download_dir() -> PathBuf {
    dirs::download_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("."))
}

/// Size of a file plus its SQLite `-wal`/`-shm` siblings.
pub fn sqlite_size(main: &Path) -> u64 {
    ["", "-wal", "-shm"]
        .iter()
        .filter_map(|s| {
            let mut p = main.as_os_str().to_os_string();
            p.push(s);
            std::fs::metadata(PathBuf::from(p)).ok().map(|m| m.len())
        })
        .sum()
}

pub fn dir_size(dir: &Path) -> u64 {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok().map(|m| m.len()))
        .sum()
}
