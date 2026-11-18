//! Key/value secure storage — the abstraction over the platform store
//! (spec §2.3 `secure_store.dart`).
//!
//! [`FileSecureStore`] keeps one OS-protected file (see [`super::protector`]),
//! written atomically (`.tmp` + fsync + rename) under a mutex, so concurrent
//! writers can't interleave. Values are bytes; the decrypted map is kept in
//! zeroizing buffers.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine;
use parking_lot::Mutex;

use super::protector::Protector;
use bf_core::crypto::SecretBytes;
use bf_core::crypto::secure_bytes::random_bytes;
use bf_core::{AppFailure, AppResult};

pub trait SecureStore: Send + Sync {
    fn read(&self, key: &str) -> AppResult<Option<SecretBytes>>;
    fn write(&self, key: &str, value: &[u8]) -> AppResult<()>;
    fn delete(&self, key: &str) -> AppResult<()>;
    fn keys(&self) -> AppResult<Vec<String>>;

    fn contains(&self, key: &str) -> AppResult<bool> {
        Ok(self.read(key)?.is_some())
    }
    fn read_string(&self, key: &str) -> AppResult<Option<String>> {
        Ok(self.read(key)?.map(|b| String::from_utf8_lossy(&b).into_owned()))
    }
    fn write_string(&self, key: &str, value: &str) -> AppResult<()> {
        self.write(key, value.as_bytes())
    }
    /// Wipe step 2: overwrite every key except `keep` with 32 random bytes
    /// (base64), then delete it. Each key is handled independently so one
    /// failure never halts the wipe.
    fn overwrite_and_delete_all(&self, keep: &str) -> Vec<AppFailure> {
        let mut errors = Vec::new();
        let keys = match self.keys() {
            Ok(k) => k,
            Err(e) => return vec![e],
        };
        for k in keys.iter().filter(|k| k.as_str() != keep) {
            let junk = base64::engine::general_purpose::STANDARD.encode(random_bytes(32));
            if let Err(e) = self.write(k, junk.as_bytes()) {
                errors.push(e);
            }
            if let Err(e) = self.delete(k) {
                errors.push(e);
            }
        }
        errors
    }
}

type Map = BTreeMap<String, SecretBytes>;

pub struct FileSecureStore {
    path: PathBuf,
    protector: Box<dyn Protector>,
    state: Mutex<Option<Map>>,
}

impl FileSecureStore {
    pub fn new(path: PathBuf, protector: Box<dyn Protector>) -> Self {
        Self { path, protector, state: Mutex::new(None) }
    }

    fn load(&self) -> AppResult<Map> {
        let raw = match fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
            Err(e) => return Err(AppFailure::io("read secure store", e)),
        };
        let plain = self.protector.unprotect(&raw)?;
        let enc: BTreeMap<String, String> = serde_json::from_slice(&plain)
            .map_err(|_| AppFailure::crypto("secure store corrupt"))?;
        let mut map = Map::new();
        for (k, v) in enc {
            let mut v = zeroize::Zeroizing::new(v);
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(v.as_bytes())
                .map_err(|_| AppFailure::crypto("secure store corrupt"))?;
            zeroize::Zeroize::zeroize(&mut *v);
            map.insert(k, zeroize::Zeroizing::new(bytes));
        }
        Ok(map)
    }

    fn persist(&self, map: &Map) -> AppResult<()> {
        let enc: BTreeMap<&str, zeroize::Zeroizing<String>> = map
            .iter()
            .map(|(k, v)| (k.as_str(), zeroize::Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(&**v))))
            .collect();
        let json = zeroize::Zeroizing::new(
            serde_json::to_vec(&enc.iter().map(|(k, v)| (*k, v.as_str())).collect::<BTreeMap<_, _>>())
                .map_err(|_| AppFailure::unknown("serialize secure store"))?,
        );
        let blob = self.protector.protect(&json)?;
        atomic_write(&self.path, &blob)
    }

    fn with_map<R>(&self, f: impl FnOnce(&mut Map) -> AppResult<(R, bool)>) -> AppResult<R> {
        let mut guard = self.state.lock();
        if guard.is_none() {
            *guard = Some(self.load()?);
        }
        let map = guard.as_mut().expect("loaded above");
        let (r, dirty) = f(map)?;
        if dirty {
            self.persist(map)?;
        }
        Ok(r)
    }

    /// Wipe: also destroy OS key material and remove the file.
    pub fn destroy_backing(&self) -> AppResult<()> {
        let mut guard = self.state.lock();
        *guard = Some(Map::new());
        if self.path.exists() {
            // Overwrite before unlinking (best effort on journaling filesystems).
            if let Ok(m) = fs::metadata(&self.path) {
                let _ = fs::write(&self.path, random_bytes(m.len() as usize));
            }
            fs::remove_file(&self.path).map_err(|e| AppFailure::io("remove secure store", e))?;
        }
        self.protector.destroy()
    }
}

impl SecureStore for FileSecureStore {
    fn read(&self, key: &str) -> AppResult<Option<SecretBytes>> {
        self.with_map(|m| Ok((m.get(key).map(|v| zeroize::Zeroizing::new(v.to_vec())), false)))
    }
    fn write(&self, key: &str, value: &[u8]) -> AppResult<()> {
        self.with_map(|m| {
            m.insert(key.to_string(), zeroize::Zeroizing::new(value.to_vec()));
            Ok(((), true))
        })
    }
    fn delete(&self, key: &str) -> AppResult<()> {
        self.with_map(|m| {
            let existed = m.remove(key).is_some();
            Ok(((), existed))
        })
    }
    fn keys(&self) -> AppResult<Vec<String>> {
        self.with_map(|m| Ok((m.keys().cloned().collect(), false)))
    }
}

pub fn atomic_write(path: &Path, data: &[u8]) -> AppResult<()> {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let res = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    res.map_err(|e| {
        let _ = fs::remove_file(&tmp);
        AppFailure::io("atomic write", e)
    })
}

/// In-memory store for tests.
#[derive(Default)]
pub struct MemorySecureStore {
    map: Mutex<Map>,
}

impl SecureStore for MemorySecureStore {
    fn read(&self, key: &str) -> AppResult<Option<SecretBytes>> {
        Ok(self.map.lock().get(key).map(|v| zeroize::Zeroizing::new(v.to_vec())))
    }
    fn write(&self, key: &str, value: &[u8]) -> AppResult<()> {
        self.map.lock().insert(key.into(), zeroize::Zeroizing::new(value.to_vec()));
        Ok(())
    }
    fn delete(&self, key: &str) -> AppResult<()> {
        self.map.lock().remove(key);
        Ok(())
    }
    fn keys(&self) -> AppResult<Vec<String>> {
        Ok(self.map.lock().keys().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure::protector::{NullProtector, platform};

    #[test]
    fn file_store_round_trip_and_wipe() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.bin");
        let s = FileSecureStore::new(p.clone(), platform());
        s.write("a", b"1").unwrap();
        s.write("wipe_pending", b"1").unwrap();
        s.write("b", &[0, 255]).unwrap();
        // Reopen from disk.
        let s2 = FileSecureStore::new(p.clone(), platform());
        assert_eq!(&**s2.read("b").unwrap().unwrap(), &[0, 255]);
        assert!(s2.overwrite_and_delete_all("wipe_pending").is_empty());
        assert_eq!(s2.keys().unwrap(), vec!["wipe_pending".to_string()]);
        s2.delete("wipe_pending").unwrap();
        assert!(s2.keys().unwrap().is_empty());
        let raw = fs::read(&p).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("wipe_pending") || cfg!(not(windows)));

        let s3 = FileSecureStore::new(d.path().join("n.bin"), Box::new(NullProtector));
        s3.write("k", b"v").unwrap();
        s3.destroy_backing().unwrap();
        assert!(!d.path().join("n.bin").exists());
    }
}
