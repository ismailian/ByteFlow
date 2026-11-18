//! Sharded on-disk thumbnail cache (spec §10.4):
//! `<app-support>/thumbs/<first 2 hex>/<sha256(key)>.jpg`.
//!
//! App-private and non-configurable. Presence is a memory-only set loaded
//! once at startup; lookups never `stat`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use parking_lot::RwLock;
use sha2::{Digest, Sha256};

use crate::secure::store::atomic_write;
use bf_core::{AppFailure, AppResult};

pub struct ThumbnailStore {
    root: PathBuf,
    present: RwLock<HashSet<String>>,
}

fn hash(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

impl ThumbnailStore {
    /// Loads the presence set (blocking; call at startup off the UI thread).
    pub fn open(root: &Path) -> Self {
        let _ = fs::create_dir_all(root);
        let mut present = HashSet::new();
        if let Ok(shards) = fs::read_dir(root) {
            for shard in shards.flatten() {
                if let Ok(files) = fs::read_dir(shard.path()) {
                    for f in files.flatten() {
                        let name = f.file_name().to_string_lossy().into_owned();
                        if let Some(h) = name.strip_suffix(".jpg") {
                            present.insert(h.to_string());
                        }
                    }
                }
            }
        }
        Self { root: root.to_path_buf(), present: RwLock::new(present) }
    }

    pub fn path_for(&self, key: &str) -> PathBuf {
        let h = hash(key);
        self.root.join(&h[..2]).join(format!("{h}.jpg"))
    }

    /// In-memory lookup only.
    pub fn has(&self, key: &str) -> bool {
        self.present.read().contains(&hash(key))
    }

    pub fn read(&self, key: &str) -> Option<Vec<u8>> {
        if !self.has(key) {
            return None;
        }
        fs::read(self.path_for(key)).ok()
    }

    pub fn put(&self, key: &str, jpeg: &[u8]) -> AppResult<()> {
        let p = self.path_for(key);
        if let Some(d) = p.parent() {
            fs::create_dir_all(d).map_err(|e| AppFailure::io("thumb dir", e))?;
        }
        atomic_write(&p, jpeg)?;
        self.present.write().insert(hash(key));
        Ok(())
    }

    pub fn remove(&self, key: &str) {
        let h = hash(key);
        if self.present.write().remove(&h) {
            let _ = fs::remove_file(self.root.join(&h[..2]).join(format!("{h}.jpg")));
        }
    }

    pub fn clear(&self) -> AppResult<()> {
        self.present.write().clear();
        if self.root.exists() {
            fs::remove_dir_all(&self.root).map_err(|e| AppFailure::io("clear thumbs", e))?;
        }
        fs::create_dir_all(&self.root).map_err(|e| AppFailure::io("thumb dir", e))
    }

    pub fn count(&self) -> usize {
        self.present.read().len()
    }

    pub fn usage_bytes(&self) -> u64 {
        crate::paths::dir_size(&self.root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sharded_presence() {
        let d = tempfile::tempdir().unwrap();
        let s = ThumbnailStore::open(d.path());
        s.put("a/b c.jpg", b"jpeg").unwrap();
        assert!(s.has("a/b c.jpg"));
        let p = s.path_for("a/b c.jpg");
        assert_eq!(p.parent().unwrap().file_name().unwrap().len(), 2);
        let s2 = ThumbnailStore::open(d.path());
        assert!(s2.has("a/b c.jpg"));
        s2.remove("a/b c.jpg");
        assert!(!s2.has("a/b c.jpg") && !p.exists());
    }
}
