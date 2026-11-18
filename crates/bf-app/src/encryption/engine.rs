//! Encryption engine (spec §9.3–§9.10).
//!
//! Each task is a round trip: download to `<temp>/enc_work` → transform on
//! the blocking pool (rayon-parallel segments) → upload back to the **same
//! key** → update the index → delete the temps. Rotation and removal finalise
//! when the queue drains and are resumable across restarts.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use bf_core::crypto::file_crypto::{self, DEFAULT_CHUNK_SIZE, KDF_SALT_LEN, KEY_HEADER_PROBE_BYTES, KdfInfo, LEGACY_FORMAT_MESSAGE};
use bf_core::crypto::secure_bytes::random_bytes;
use bf_core::crypto::{PasswordHasher, SecretBytes};
use bf_core::failures_ui::message_for;
use bf_core::{AppFailure, AppResult, log_warn};
use bf_data::secure::credential_store::MasterSlot;

use super::keys::{EncryptionSession, EpochKey, LoadedKeys};
use super::state::{EXTERNAL_EPOCH, EncOp, EncStatus, EncryptTask, StateStore};
use crate::core::Core;
use crate::session::ClientHandle;
use crate::transfers::uploader::{self, UploadSpec};
use crate::{blocking, new_id, now_ms};

const PROGRESS_THROTTLE: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncPhase {
    Downloading,
    Transforming,
    Uploading,
}

#[derive(Debug, Clone)]
pub struct EncTaskView {
    pub task: EncryptTask,
    pub phase: Option<EncPhase>,
    pub done: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Default)]
pub struct BulkSplit {
    pub applies: Vec<String>,
    pub already: Vec<String>,
    pub locked: Vec<String>,
    pub external: Vec<String>,
}

pub struct UploadTarget {
    pub epoch: i64,
    pub key: Arc<EpochKey>,
}

#[derive(Clone, Copy)]
struct Live {
    phase: EncPhase,
    done: u64,
    total: u64,
}

type IndexHook = Arc<dyn Fn(String, Option<i64>) + Send + Sync>;

pub struct EncryptionEngine {
    core: Arc<Core>,
    client: ClientHandle,
    store: StateStore,
    session: EncryptionSession,
    running: Mutex<HashMap<String, CancellationToken>>,
    live: Mutex<HashMap<String, Live>>,
    version: watch::Sender<u64>,
    index_hook: RwLock<Option<IndexHook>>,
    cancel: CancellationToken,
    writer: Mutex<Option<tokio::task::JoinHandle<()>>>,
    last_notify: Mutex<Instant>,
    work_dir: PathBuf,
    preview_dir: PathBuf,
    seq: AtomicU64,
}

impl EncryptionEngine {
    pub async fn start(core: Arc<Core>, client: ClientHandle, keys: LoadedKeys, cancel: CancellationToken) -> Arc<Self> {
        let path = core.paths.encryption_state();
        let store = blocking(move || Ok(StateStore::load(path))).await.expect("state load is infallible");
        let session = EncryptionSession::default();
        store.write(|s| {
            // Tasks interrupted mid-run reset to queued (§9.5).
            for t in &mut s.queue {
                if t.status == EncStatus::Running {
                    t.status = EncStatus::Queued;
                }
            }
            // A master key with no recorded epoch (e.g. lost state file).
            if s.active_epoch.is_none() && keys.active.is_some() {
                s.active_epoch = Some(1);
            }
            if s.rekey_to_epoch.is_none() && keys.pending.is_some() {
                s.rekey_to_epoch = Some(s.active_epoch.unwrap_or(1) + 1);
            }
        });
        let (active, pending) = store.read(|s| (s.active_epoch, s.rekey_to_epoch));
        if let (Some(e), Some(k)) = (active, keys.active) {
            session.insert(e, k);
        }
        if let (Some(e), Some(k)) = (pending, keys.pending) {
            session.insert(e, k);
        }
        let work_dir = core.paths.temp_sub("enc_work");
        let preview_dir = core.paths.temp_sub("enc_preview");
        let engine = Arc::new(Self {
            core,
            client,
            store,
            session,
            running: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            version: watch::channel(0).0,
            index_hook: RwLock::new(None),
            cancel: cancel.clone(),
            writer: Mutex::new(None),
            last_notify: Mutex::new(Instant::now()),
            work_dir,
            preview_dir,
            seq: AtomicU64::new(0),
        });
        *engine.writer.lock() = Some(engine.store.spawn_writer(cancel));
        engine.pump();
        engine
    }

    pub fn set_index_hook(&self, h: IndexHook) {
        *self.index_hook.write() = Some(h);
    }

    /// Bumps on every index change (thumbnails subscribe, §10.4).
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
    }

    fn bump(&self) {
        self.version.send_modify(|v| *v += 1);
        self.core.notify();
    }

    fn index_changed(&self, key: &str, epoch: Option<i64>) {
        if let Some(h) = self.index_hook.read().clone() {
            h(key.to_string(), epoch);
        }
    }

    // ------------------------------------------------------------ queries --

    pub fn epoch_of(&self, key: &str) -> Option<i64> {
        self.store.read(|s| s.encrypted.get(key).copied())
    }
    pub fn is_encrypted(&self, key: &str) -> bool {
        self.epoch_of(key).is_some()
    }
    pub fn is_external(&self, key: &str) -> bool {
        self.epoch_of(key) == Some(EXTERNAL_EPOCH)
    }
    /// Keys with an active task: no actions allowed until it finishes.
    pub fn is_locked(&self, key: &str) -> bool {
        self.store.read(|s| s.is_active(key))
    }
    pub fn locked_keys(&self) -> HashSet<String> {
        self.store.read(|s| {
            s.queue
                .iter()
                .filter(|t| matches!(t.status, EncStatus::Queued | EncStatus::Running))
                .map(|t| t.remote_key.clone())
                .collect()
        })
    }
    pub fn encrypted_keys(&self) -> Vec<String> {
        self.store.read(|s| s.encrypted.keys().cloned().collect())
    }
    pub fn index_snapshot(&self) -> HashMap<String, i64> {
        self.store.read(|s| s.encrypted.iter().map(|(k, v)| (k.clone(), *v)).collect())
    }
    pub fn has_key(&self) -> bool {
        self.store.read(|s| s.active_epoch.is_some())
    }
    pub fn active_epoch(&self) -> Option<i64> {
        self.store.read(|s| s.active_epoch)
    }
    pub fn rotating(&self) -> bool {
        self.store.read(|s| s.rekey_to_epoch.is_some())
    }
    pub fn removing(&self) -> bool {
        self.store.read(|s| s.removing)
    }
    pub fn keys_loaded(&self) -> bool {
        self.active_epoch().is_some_and(|e| self.session.get(e).is_some())
    }

    pub fn tasks(&self) -> Vec<EncTaskView> {
        let live = self.live.lock().clone();
        self.store.read(|s| {
            s.queue
                .iter()
                .map(|t| {
                    let l = live.get(&t.id);
                    EncTaskView { task: t.clone(), phase: l.map(|l| l.phase), done: l.map_or(0, |l| l.done), total: l.map_or(0, |l| l.total) }
                })
                .collect()
        })
    }

    // ----------------------------------------------- settings operations --

    /// Set key (§9.2): passphrase → Argon2id with a fresh salt → master key,
    /// sealed under the DEK with its salt/params alongside.
    pub async fn set_key(self: &Arc<Self>, passphrase: SecretBytes, dek: SecretBytes) -> AppResult<()> {
        if self.has_key() {
            return Err(AppFailure::conflict("a key is already set; use change key"));
        }
        let epoch = self.store.read(|s| s.encrypted.values().copied().max().unwrap_or(0).max(0)) + 1;
        let (key, kdf) = self.derive_and_seal(passphrase, dek, MasterSlot::Active).await?;
        self.session.insert(epoch, EpochKey { key, kdf: Some(kdf) });
        self.store.write(|s| {
            s.active_epoch = Some(epoch);
            s.removing = false;
        });
        self.bump();
        self.kick();
        Ok(())
    }

    async fn derive_and_seal(&self, passphrase: SecretBytes, dek: SecretBytes, slot: MasterSlot) -> AppResult<(SecretBytes, KdfInfo)> {
        let cred = self.core.credentials.clone();
        blocking(move || {
            let salt = random_bytes(KDF_SALT_LEN);
            let params = cred.kdf_params();
            let key = PasswordHasher::derive_key(&passphrase, &salt, params)?;
            let kdf = KdfInfo { salt, params };
            cred.seal_master(&dek, slot, &key, &kdf)?;
            Ok((key, kdf))
        })
        .await
    }

    /// Change key (rotation): new key → pending slot → re-encrypt every
    /// app-encrypted object. Fails fast if a rotation is already running.
    pub async fn change_key(self: &Arc<Self>, passphrase: SecretBytes, dek: SecretBytes) -> AppResult<usize> {
        let (active, rekey, removing) = self.store.read(|s| (s.active_epoch, s.rekey_to_epoch, s.removing));
        let Some(active) = active else { return Err(AppFailure::conflict("no key set")) };
        if rekey.is_some() || removing {
            return Err(AppFailure::conflict("a key operation is already running"));
        }
        let new_epoch = active + 1;
        let (key, kdf) = self.derive_and_seal(passphrase, dek, MasterSlot::Pending).await?;
        self.session.insert(new_epoch, EpochKey { key, kdf: Some(kdf) });
        let n = self.store.write(|s| {
            s.rekey_to_epoch = Some(new_epoch);
            let keys: Vec<String> = s.app_encrypted().filter(|(_, e)| **e != new_epoch).map(|(k, _)| k.clone()).collect();
            let n = keys.len();
            for k in keys {
                push_task(s, &k, EncOp::ReEncrypt);
            }
            n
        });
        self.bump();
        self.kick();
        Ok(n)
    }

    /// Remove encryption: persisted `removing` flag + decrypt every
    /// app-encrypted object.
    pub fn remove_encryption(self: &Arc<Self>) -> AppResult<usize> {
        let n = self.store.write(|s| {
            s.removing = true;
            let keys: Vec<String> = s.app_encrypted().map(|(k, _)| k.clone()).collect();
            let n = keys.len();
            for k in keys {
                push_task(s, &k, EncOp::Decrypt);
            }
            n
        });
        self.bump();
        self.kick();
        Ok(n)
    }

    /// Lists the whole bucket and enqueues encrypt for every plaintext object.
    pub async fn encrypt_all_existing(self: &Arc<Self>) -> AppResult<usize> {
        if !self.has_key() {
            return Err(AppFailure::conflict("no key set"));
        }
        let objects = self.client.get().list_all("", Some(&self.cancel)).await?;
        let n = self.store.write(|s| {
            let mut n = 0;
            for o in objects.iter().filter(|o| !o.key.ends_with('/')) {
                if !s.encrypted.contains_key(&o.key) && !s.is_active(&o.key) {
                    push_task(s, &o.key, EncOp::Encrypt);
                    n += 1;
                }
            }
            n
        });
        self.bump();
        self.kick();
        Ok(n)
    }

    /// Split a selection into applies / already / locked / external (§6.5).
    pub fn split(&self, keys: &[String], op: EncOp) -> BulkSplit {
        self.store.read(|s| {
            let mut out = BulkSplit::default();
            for k in keys {
                let epoch = s.encrypted.get(k).copied();
                if s.is_active(k) {
                    out.locked.push(k.clone());
                } else if epoch == Some(EXTERNAL_EPOCH) && op != EncOp::Encrypt {
                    out.external.push(k.clone());
                } else if (op == EncOp::Encrypt) == epoch.is_some() {
                    out.already.push(k.clone());
                } else {
                    out.applies.push(k.clone());
                }
            }
            out
        })
    }

    pub fn enqueue(self: &Arc<Self>, keys: &[String], op: EncOp) -> BulkSplit {
        let split = self.split(keys, op);
        if !split.applies.is_empty() {
            self.store.write(|s| {
                for k in &split.applies {
                    push_task(s, k, op);
                }
            });
            self.bump();
            self.kick();
        }
        split
    }

    pub fn cancel_task(&self, id: &str) {
        self.store.write(|s| s.queue.retain(|t| !(t.id == id && t.status == EncStatus::Queued)));
        self.bump();
    }

    pub fn retry_task(self: &Arc<Self>, id: &str) {
        self.store.write(|s| {
            if let Some(t) = s.queue.iter_mut().find(|t| t.id == id && t.status == EncStatus::Failed) {
                t.status = EncStatus::Queued;
                t.error = None;
            }
        });
        self.bump();
        self.kick();
    }

    pub fn clear_finished(&self) {
        self.store.write(|s| s.queue.retain(|t| t.status != EncStatus::Completed));
        self.bump();
    }

    // -------------------------------------------------- encrypt on upload --

    /// During a rotation, returns the new epoch's key (§9.7).
    pub fn upload_target(&self) -> Option<UploadTarget> {
        if !self.core.prefs.get().encrypt_new_uploads {
            return None;
        }
        let epoch = self.store.read(|s| s.rekey_to_epoch.or(s.active_epoch))?;
        Some(UploadTarget { epoch, key: self.session.get(epoch)? })
    }

    pub async fn encrypt_to_temp(&self, src: &Path, target: &UploadTarget, out_dir: &Path) -> AppResult<PathBuf> {
        let out = out_dir.join(format!("{}.enc", new_id()));
        let (src, out2, key) = (src.to_path_buf(), out.clone(), target.key.clone());
        blocking(move || file_crypto::encrypt_file(&src, &out2, &key.key, DEFAULT_CHUNK_SIZE, key.kdf.as_ref(), &mut |_, _| true)).await?;
        Ok(out)
    }

    /// Called **before** enqueueing an encrypted upload, so thumbnail
    /// generation (which starts at enqueue) already sees it as encrypted.
    pub fn mark_encrypted(&self, key: &str, epoch: i64) {
        self.store.write(|s| {
            s.encrypted.insert(key.to_string(), epoch);
        });
        self.index_changed(key, Some(epoch));
        self.bump();
    }

    // ------------------------------------------------- index maintenance --

    pub fn on_renamed_or_moved(&self, from: &str, to: &str) {
        let moved = self.store.write(|s| {
            let e = s.encrypted.remove(from);
            if let Some(e) = e {
                s.encrypted.insert(to.to_string(), e);
            }
            e
        });
        if moved.is_some() {
            self.index_changed(to, moved);
            self.bump();
        }
    }

    pub fn on_folder_moved(&self, from: &str, to: &str) {
        let changed = self.store.write(|s| {
            let keys: Vec<String> = s.encrypted.range(from.to_string()..).take_while(|(k, _)| k.starts_with(from)).map(|(k, _)| k.clone()).collect();
            let mut out = vec![];
            for k in keys {
                if let Some(e) = s.encrypted.remove(&k) {
                    let nk = format!("{to}{}", &k[from.len()..]);
                    s.encrypted.insert(nk.clone(), e);
                    out.push((nk, e));
                }
            }
            out
        });
        for (k, e) in &changed {
            self.index_changed(k, Some(*e));
        }
        if !changed.is_empty() {
            self.bump();
        }
    }

    pub fn on_deleted(&self, key: &str) {
        if self.store.write(|s| s.encrypted.remove(key)).is_some() {
            self.bump();
        }
    }

    pub fn on_folder_deleted(&self, prefix: &str) {
        let n = self.store.write(|s| {
            let before = s.encrypted.len();
            s.encrypted.retain(|k, _| !k.starts_with(prefix));
            before - s.encrypted.len()
        });
        if n > 0 {
            self.bump();
        }
    }

    /// Backstop after a *completed* full scan (§9.10): adopt encrypted keys the
    /// index doesn't know, drop entries the bucket no longer holds. Stands
    /// down while a rotation/removal is in flight or any task is active.
    pub fn reconcile_from_scan(&self, found: &HashMap<String, Option<i64>>, present: &HashSet<String>) -> bool {
        let applied = self.store.write(|s| {
            if s.rekey_to_epoch.is_some() || s.removing || s.busy() {
                return false;
            }
            for (k, epoch) in found {
                if !s.encrypted.contains_key(k) {
                    let e = epoch.or(s.active_epoch).unwrap_or(EXTERNAL_EPOCH);
                    s.encrypted.insert(k.clone(), e);
                }
            }
            s.encrypted.retain(|k, _| present.contains(k));
            true
        });
        if applied {
            self.bump();
        }
        applied
    }

    // ------------------------------------------- opening encrypted objects --

    pub fn set_session_passphrase(&self, p: SecretBytes) {
        self.session.set_passphrase(p);
    }

    fn refile_external(&self, key: &str) {
        self.store.write(|s| {
            if let Some(e) = s.encrypted.get_mut(key) {
                *e = EXTERNAL_EPOCH;
            }
        });
        self.index_changed(key, Some(EXTERNAL_EPOCH));
        self.bump();
    }

    /// Header-first key resolution (§9.9): a 512 B ranged GET decides which
    /// key opens the object before anything is downloaded.
    pub async fn resolve_key(&self, key: &str) -> AppResult<SecretBytes> {
        let head = self.client.get().get_range(key, 0, KEY_HEADER_PROBE_BYTES).await?;
        let info = file_crypto::read_key_info_from_bytes(&head)?;
        let indexed = self.epoch_of(key);

        // 1. The index's app epoch (then any other loaded epoch, adopting it).
        if let Some(e) = indexed.filter(|e| *e != EXTERNAL_EPOCH)
            && let Some(k) = self.session.get(e)
            && file_crypto::opens_with_info(&info, &k.key)
        {
            return Ok(zeroize::Zeroizing::new(k.key.to_vec()));
        }
        for (e, k) in self.session.all() {
            if file_crypto::opens_with_info(&info, &k.key) {
                self.store.write(|s| {
                    s.encrypted.insert(key.to_string(), e);
                });
                self.index_changed(key, Some(e));
                return Ok(zeroize::Zeroizing::new(k.key.to_vec()));
            }
        }
        if indexed.is_some_and(|e| e != EXTERNAL_EPOCH) || indexed.is_none() {
            // One-way, self-healing correction.
            self.store.write(|s| {
                s.encrypted.insert(key.to_string(), EXTERNAL_EPOCH);
            });
            self.index_changed(key, Some(EXTERNAL_EPOCH));
            self.bump();
        }

        let Some(salt_id) = info.kdf_salt_id() else {
            return Err(AppFailure::crypto(LEGACY_FORMAT_MESSAGE));
        };
        // 2. Session-derived external keys.
        if let Some(k) = self.session.external_key(&salt_id)
            && file_crypto::opens_with_info(&info, &k)
        {
            return Ok(k);
        }
        // 3. The sealed external-key table (under the file master key).
        let master = self.active_epoch().and_then(|e| self.session.get(e));
        if let Some(m) = &master
            && let Ok(Some(k)) = self.core.credentials.external_key(&salt_id, &m.key)
            && file_crypto::opens_with_info(&info, &k)
        {
            self.session.put_external_key(&salt_id, &k);
            return Ok(k);
        }
        // 4. The session passphrase.
        let Some(pass) = self.session.passphrase() else { return Err(AppFailure::external_key()) };
        let info2 = info.clone();
        let derived = blocking(move || file_crypto::derive_key_for_info(&info2, &pass)).await;
        match derived {
            Ok(k) => {
                self.session.put_external_key(&salt_id, &k);
                if let Some(m) = master {
                    let cred = self.core.credentials.clone();
                    let (sid, kk) = (salt_id.clone(), k.to_vec());
                    let _ = blocking(move || cred.store_external_key(&sid, &kk, &m.key)).await;
                }
                Ok(k)
            }
            Err(_) => Err(AppFailure::external_key()),
        }
    }

    /// Preview path: resolve the key **before** downloading, download to
    /// `enc_preview/<id>.enc`, decrypt to `<id>-<leaf>`, delete the
    /// ciphertext. The caller deletes the plaintext when the viewer closes.
    pub async fn decrypt_to_temp(&self, key: &str, progress: Option<crate::transfers::uploader::ProgressFn>) -> AppResult<PathBuf> {
        let master = self.resolve_key(key).await?;
        let id = new_id();
        let enc = self.preview_dir.join(format!("{id}.enc"));
        let out = self.preview_dir.join(format!("{id}-{}", sanitize_leaf(bf_core::media::leaf_name(key))));
        let r = async {
            self.download_to(key, &enc, &self.cancel, progress.as_ref()).await?;
            let (e2, o2) = (enc.clone(), out.clone());
            blocking(move || file_crypto::decrypt_file(&e2, &o2, &master, &mut |_, _| true)).await
        }
        .await;
        let _ = tokio::fs::remove_file(&enc).await;
        r.map(|_| out)
    }

    /// Foreground download + decrypt into the owner's folder.
    pub async fn download_decrypted(&self, key: &str, dest: &Path, progress: Option<crate::transfers::uploader::ProgressFn>, cancel: &CancellationToken) -> AppResult<()> {
        let master = self.resolve_key(key).await?;
        let enc = self.work_dir.join(format!("{}.dl.enc", new_id()));
        let r = async {
            self.download_to(key, &enc, cancel, progress.as_ref()).await?;
            if let Some(d) = dest.parent() {
                tokio::fs::create_dir_all(d).await.map_err(|e| AppFailure::io("create dir", e))?;
            }
            let (e2, d2) = (enc.clone(), dest.to_path_buf());
            blocking(move || file_crypto::decrypt_file(&e2, &d2, &master, &mut |_, _| true)).await
        }
        .await;
        let _ = tokio::fs::remove_file(&enc).await;
        r
    }

    async fn download_to(&self, key: &str, path: &Path, cancel: &CancellationToken, progress: Option<&crate::transfers::uploader::ProgressFn>) -> AppResult<u64> {
        let mut resp = self.client.get().get_stream(key, 0, Some(cancel)).await?;
        let total = resp.content_length().unwrap_or(0);
        let f = tokio::fs::File::create(path).await.map_err(|e| AppFailure::io("create temp", e))?;
        let mut w = tokio::io::BufWriter::with_capacity(1 << 20, f);
        let mut done = 0u64;
        loop {
            let c = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(AppFailure::Cancelled),
                c = resp.chunk() => c.map_err(|e| bf_data::r2::failure::from_transport(&e))?,
            };
            let Some(c) = c else { break };
            w.write_all(&c).await.map_err(|e| AppFailure::io("write temp", e))?;
            done += c.len() as u64;
            if let Some(p) = progress {
                p(done);
            }
        }
        w.flush().await.map_err(|e| AppFailure::io("flush temp", e))?;
        if total > 0 && done != total {
            return Err(AppFailure::network("download ended early"));
        }
        Ok(done)
    }

    // ------------------------------------------------------------- queue --

    /// Resume parked tasks (e.g. once keys load).
    pub fn kick(self: &Arc<Self>) {
        self.pump();
    }

    fn needed_epochs(&self, t: &EncryptTask) -> Option<Vec<i64>> {
        self.store.read(|s| {
            let target = s.rekey_to_epoch.or(s.active_epoch);
            let current = s.encrypted.get(&t.remote_key).copied();
            match t.op {
                EncOp::Encrypt => Some(vec![target?]),
                EncOp::Decrypt => Some(vec![current?]),
                EncOp::ReEncrypt => Some(vec![current?, s.rekey_to_epoch?]),
            }
        })
    }

    fn pump(self: &Arc<Self>) {
        if self.cancel.is_cancelled() {
            return;
        }
        let limit = self.core.prefs.get().encryption_concurrency.clamp(1, 8);
        let mut running = self.running.lock();
        if running.len() >= limit {
            return;
        }
        let candidates: Vec<EncryptTask> = self.store.read(|s| {
            s.queue.iter().filter(|t| t.status == EncStatus::Queued && !running.contains_key(&t.id)).cloned().collect()
        });
        let mut started = 0;
        for t in candidates {
            if running.len() >= limit {
                break;
            }
            // Key parking: skip tasks whose epoch key isn't loaded.
            let Some(epochs) = self.needed_epochs(&t) else { continue };
            if epochs.iter().any(|e| *e == EXTERNAL_EPOCH || self.session.get(*e).is_none()) {
                continue;
            }
            let token = self.cancel.child_token();
            running.insert(t.id.clone(), token.clone());
            self.set_status(&t.id, EncStatus::Running, None);
            started += 1;
            let me = self.clone();
            tokio::spawn(async move {
                let id = t.id.clone();
                let inner = me.clone();
                let task = t.clone();
                let res = tokio::spawn(async move { inner.run_task(&task, &token).await }).await;
                let res = res.unwrap_or_else(|_| Err(AppFailure::unknown("task panicked")));
                me.live.lock().remove(&id);
                me.running.lock().remove(&id);
                match res {
                    Ok(()) => me.set_status(&id, EncStatus::Completed, None),
                    Err(AppFailure::Cancelled) => me.set_status(&id, EncStatus::Queued, None),
                    Err(e) => {
                        log_warn!("encryption task failed: {}", e.developer_message());
                        me.set_status(&id, EncStatus::Failed, Some(message_for(&e)));
                    }
                }
                me.bump();
                me.pump();
                me.on_idle().await;
            });
        }
        drop(running);
        if started > 0 {
            self.bump();
        }
    }

    fn set_status(&self, id: &str, st: EncStatus, err: Option<String>) {
        self.store.write(|s| {
            if let Some(t) = s.queue.iter_mut().find(|t| t.id == id) {
                t.status = st;
                t.error = err;
                t.updated_at_ms = now_ms();
            }
        });
    }

    fn report(&self, id: &str, phase: EncPhase, done: u64, total: u64) {
        self.live.lock().insert(id.to_string(), Live { phase, done, total });
        let mut last = self.last_notify.lock();
        if last.elapsed() >= PROGRESS_THROTTLE {
            *last = Instant::now();
            self.core.notify();
        }
    }

    async fn run_task(self: &Arc<Self>, t: &EncryptTask, cancel: &CancellationToken) -> AppResult<()> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let base = self.work_dir.join(format!("{}-{seq}", t.id));
        let (tin, tmid, tout) = (base.with_extension("in"), base.with_extension("mid"), base.with_extension("out"));
        let r = self.round_trip(t, cancel, &tin, &tmid, &tout).await;
        for p in [&tin, &tmid, &tout] {
            let _ = tokio::fs::remove_file(p).await;
        }
        r
    }

    async fn round_trip(self: &Arc<Self>, t: &EncryptTask, cancel: &CancellationToken, tin: &Path, tmid: &Path, tout: &Path) -> AppResult<()> {
        let key = t.remote_key.clone();
        let (target, current, rekey) = self.store.read(|s| (s.rekey_to_epoch.or(s.active_epoch), s.encrypted.get(&key).copied(), s.rekey_to_epoch));

        // Downloading.
        let me = self.clone();
        let id = t.id.clone();
        let total_hint = self.client.get().head(&key).await.map(|h| h.size).unwrap_or(0);
        self.report(&t.id, EncPhase::Downloading, 0, total_hint);
        let progress: crate::transfers::uploader::ProgressFn = Arc::new(move |d| me.report(&id, EncPhase::Downloading, d, total_hint));
        self.download_to(&key, tin, cancel, Some(&progress)).await?;

        // Transforming (indeterminate).
        self.report(&t.id, EncPhase::Transforming, 0, 0);
        let result_epoch: Option<i64> = match t.op {
            EncOp::Encrypt => {
                let e = target.ok_or_else(|| AppFailure::conflict("no key"))?;
                if file_crypto::is_encrypted_file(tin) {
                    // Already ciphertext (e.g. uploaded elsewhere): just index it.
                    let ext = self.object_epoch(tin);
                    return self.finish_index(&key, Some(ext.unwrap_or(EXTERNAL_EPOCH)));
                }
                let k = self.session.get(e).ok_or_else(|| AppFailure::crypto("key not loaded"))?;
                let (a, b) = (tin.to_path_buf(), tout.to_path_buf());
                blocking(move || file_crypto::encrypt_file(&a, &b, &k.key, DEFAULT_CHUNK_SIZE, k.kdf.as_ref(), &mut |_, _| true)).await?;
                Some(e)
            }
            EncOp::Decrypt => {
                let e = current.ok_or_else(|| AppFailure::not_found("not encrypted"))?;
                let k = self.checked_key(&key, tin, e)?;
                let (a, b) = (tin.to_path_buf(), tout.to_path_buf());
                blocking(move || file_crypto::decrypt_file(&a, &b, &k.key, &mut |_, _| true)).await?;
                None
            }
            EncOp::ReEncrypt => {
                let old = current.ok_or_else(|| AppFailure::not_found("not encrypted"))?;
                let new = rekey.ok_or_else(|| AppFailure::conflict("no rotation"))?;
                if old == new {
                    return Ok(());
                }
                let ok = self.checked_key(&key, tin, old)?;
                let nk = self.session.get(new).ok_or_else(|| AppFailure::crypto("key not loaded"))?;
                let (a, m, b) = (tin.to_path_buf(), tmid.to_path_buf(), tout.to_path_buf());
                blocking(move || {
                    file_crypto::decrypt_file(&a, &m, &ok.key, &mut |_, _| true)?;
                    file_crypto::encrypt_file(&m, &b, &nk.key, DEFAULT_CHUNK_SIZE, nk.kdf.as_ref(), &mut |_, _| true)
                })
                .await?;
                let _ = tokio::fs::remove_file(tmid).await;
                Some(new)
            }
        };

        // Uploading, back to the same key.
        let out_len = tokio::fs::metadata(tout).await.map_err(|e| AppFailure::io("stat", e))?.len();
        let me = self.clone();
        let id = t.id.clone();
        self.report(&t.id, EncPhase::Uploading, 0, out_len);
        let client = self.client.get();
        let spec = UploadSpec {
            client: &client,
            key: &key,
            path: tout,
            content_type: Some(bf_core::media::content_type_for(&key)),
            cancel,
            progress: Arc::new(move |d| me.report(&id, EncPhase::Uploading, d, out_len)),
        };
        uploader::upload_file_once(&spec).await?;
        self.finish_index(&key, result_epoch)
    }

    fn object_epoch(&self, path: &Path) -> Option<i64> {
        let info = file_crypto::read_key_info(path).ok()?;
        self.session.all().into_iter().find(|(_, k)| file_crypto::opens_with_info(&info, &k.key)).map(|(e, _)| e)
    }

    /// Verify the expected epoch key opens the object; otherwise it is
    /// external — re-file it and fail the task.
    fn checked_key(&self, key: &str, tin: &Path, epoch: i64) -> AppResult<Arc<EpochKey>> {
        let k = self.session.get(epoch).ok_or_else(|| AppFailure::crypto("key not loaded"))?;
        let info = file_crypto::read_key_info(tin)?;
        if !file_crypto::opens_with_info(&info, &k.key) {
            self.refile_external(key);
            return Err(AppFailure::crypto("object is encrypted with another key"));
        }
        Ok(k)
    }

    fn finish_index(&self, key: &str, epoch: Option<i64>) -> AppResult<()> {
        self.store.write(|s| match epoch {
            Some(e) => {
                s.encrypted.insert(key.to_string(), e);
            }
            None => {
                s.encrypted.remove(key);
            }
        });
        self.index_changed(key, epoch);
        self.bump();
        Ok(())
    }

    /// Rotation/removal finalisation when the queue drains (§9.4).
    async fn on_idle(self: &Arc<Self>) {
        if !self.running.lock().is_empty() || self.store.read(|s| s.busy()) {
            return;
        }
        let (active, rekey, removing, all_at_target, none_left) = self.store.read(|s| {
            let all = s.rekey_to_epoch.is_some_and(|n| s.app_encrypted().all(|(_, e)| *e == n));
            (s.active_epoch, s.rekey_to_epoch, s.removing, all, s.app_encrypted().next().is_none())
        });
        if let (Some(old), Some(new)) = (active, rekey)
            && all_at_target
        {
            let cred = self.core.credentials.clone();
            let (ok, nk) = (self.session.get(old), self.session.get(new));
            let r = blocking(move || {
                if let (Some(o), Some(n)) = (&ok, &nk) {
                    cred.reseal_external_table(&o.key, &n.key)?;
                }
                cred.promote_pending()
            })
            .await;
            match r {
                Ok(()) => {
                    self.store.write(|s| {
                        s.active_epoch = Some(new);
                        s.rekey_to_epoch = None;
                    });
                    self.session.remove(old);
                    self.bump();
                }
                Err(e) => log_warn!("rotation finalise failed: {}", e.developer_message()),
            }
        }
        if removing && none_left {
            let cred = self.core.credentials.clone();
            if blocking(move || cred.delete_all_master()).await.is_ok() {
                for e in self.session.loaded_epochs() {
                    self.session.remove(e);
                }
                self.store.write(|s| {
                    s.removing = false;
                    s.active_epoch = None;
                    s.rekey_to_epoch = None;
                });
                self.bump();
            }
        }
    }

    /// Lock: stop tasks, flush state, wipe keys.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        self.session.clear_all();
        let h = self.writer.lock().take();
        if let Some(h) = h {
            let _ = h.await;
        }
    }
}

fn push_task(s: &mut super::state::EncryptionState, key: &str, op: EncOp) {
    if s.is_active(key) {
        return;
    }
    s.queue.retain(|t| !(t.remote_key == key && t.status == EncStatus::Failed));
    s.queue.push(EncryptTask { id: new_id(), remote_key: key.to_string(), op, status: EncStatus::Queued, error: None, updated_at_ms: now_ms() });
}

/// Keep the extension (players infer the type) but never allow path tricks.
fn sanitize_leaf(leaf: &str) -> String {
    let s: String = leaf.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { '_' } else { c }).collect();
    let s = s.trim_matches(['.', ' ']).to_string();
    if s.is_empty() { "file".into() } else { s.chars().take(120).collect() }
}

#[cfg(test)]
mod tests {
    #[test]
    fn leaf_sanitized() {
        assert_eq!(super::sanitize_leaf("a:b?.txt"), "a_b_.txt");
        assert_eq!(super::sanitize_leaf(".."), "file");
    }
}
