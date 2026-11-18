//! The top-level vault state machine (spec §2.4, §3.2–§3.6).
//!
//! ```text
//! Loading ─(wipe_pending?)→ resume wipe → NeedsPasswords
//!    ├─ configured → Locked ⇄ Unlocked ─ open bucket ⇄ close bucket
//!    │                 └─ destruction pwd / wipe-everything → Wiping → NeedsPasswords
//!    └─ not configured → NeedsPasswords → NeedsConfig → Unlocked (bucket open)
//! ```
//!
//! **Unlocked** means the vault is open: the DEK unwrapped the bucket list and
//! every bucket's file master keys (then was wiped). Exactly one bucket at a
//! time has a [`Session`]; with none, the UI shows the bucket grid.
//!
//! One password field, up to three outcomes; up to success the behaviour is
//! indistinguishable (both checks run Argon2id, whether or not a destruction
//! password is configured). A wipe ends on onboarding with **no** message —
//! it must look exactly like a fresh install.
//!
//! The destruction password is optional (0.4.0): onboarding only sets the
//! login password; Settings can turn the destruction password on, change it,
//! or turn it off. Installs that set one before 0.4.0 keep it.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::{Mutex, RwLock};

use bf_core::crypto::SecretBytes;
use bf_core::failures_ui::connection_diagnostic;
use bf_core::{AppFailure, AppResult, log_warn};
use bf_data::r2::{R2Client, R2Config};

use crate::buckets::{BucketEntry, PRIMARY_BUCKET_ID};
use crate::core::Core;
use crate::encryption::keys::LoadedKeys;
use crate::session::Session;
use crate::{blocking, new_id, strength, wiper};

pub const GENERIC_UNLOCK_ERROR: &str = "Incorrect. Try again.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultState {
    Loading,
    NeedsPasswords,
    NeedsConfig,
    Locked { error: Option<String> },
    Unlocked,
    Wiping,
}

pub struct AuthController {
    pub core: Arc<Core>,
    state: RwLock<VaultState>,
    /// The login password between onboarding steps — in memory only, wiped
    /// once the config is saved (and on drop).
    pending_login: Mutex<Option<SecretBytes>>,
    session: RwLock<Option<Arc<Session>>>,
    /// While unlocked: the connected buckets and each one's master keys
    /// (session keys, wiped on lock exactly like an open session's).
    buckets: RwLock<Vec<BucketEntry>>,
    keys: Mutex<HashMap<String, LoadedKeys>>,
    /// The bucket being opened, for the grid's spinner.
    opening: Mutex<Option<String>>,
    busy: AtomicBool,
}

impl AuthController {
    pub fn new(core: Arc<Core>) -> Arc<Self> {
        Arc::new(Self {
            core,
            state: RwLock::new(VaultState::Loading),
            pending_login: Mutex::new(None),
            session: RwLock::new(None),
            buckets: RwLock::new(Vec::new()),
            keys: Mutex::new(HashMap::new()),
            opening: Mutex::new(None),
            busy: AtomicBool::new(false),
        })
    }

    pub fn state(&self) -> VaultState {
        self.state.read().clone()
    }
    pub fn session(&self) -> Option<Arc<Session>> {
        self.session.read().clone()
    }
    pub fn busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }
    /// The connected buckets (empty while locked).
    pub fn buckets(&self) -> Vec<BucketEntry> {
        self.buckets.read().clone()
    }
    pub fn opening(&self) -> Option<String> {
        self.opening.lock().clone()
    }

    fn set(&self, s: VaultState) {
        *self.state.write() = s;
        self.core.notify();
    }

    fn begin(&self) -> bool {
        let ok = !self.busy.swap(true, Ordering::SeqCst);
        self.core.notify();
        ok
    }
    fn end(&self) {
        self.busy.store(false, Ordering::SeqCst);
        self.core.notify();
    }

    fn forget_unlocked(&self) {
        self.buckets.write().clear();
        self.keys.lock().clear();
    }

    pub async fn init(&self) {
        let core = self.core.clone();
        let configured = blocking(move || {
            if wiper::resume_if_pending(&core) {
                return Ok(false);
            }
            core.credentials.is_configured()
        })
        .await
        .unwrap_or(false);
        // An interrupted setup falls back cleanly to onboarding.
        self.set(if configured { VaultState::Locked { error: None } } else { VaultState::NeedsPasswords });
    }

    // -------------------------------------------------------- onboarding --

    /// Onboarding step 1: the login password (kept in memory until the
    /// config is saved). No destruction password is set here.
    pub async fn set_login_password(&self, login: SecretBytes) -> Result<(), String> {
        let l = String::from_utf8_lossy(&login).into_owned();
        strength::validate_login(&l, &l)?;
        *self.pending_login.lock() = Some(login);
        self.set(VaultState::NeedsConfig);
        Ok(())
    }

    pub fn back_to_passwords(&self) {
        *self.pending_login.lock() = None;
        self.set(VaultState::NeedsPasswords);
    }

    /// `ListObjectsV2 max-keys=1` with a targeted diagnostic.
    pub async fn test_connection(cfg: &R2Config) -> Result<(), String> {
        let cfg = cfg.normalized();
        cfg.validate().map_err(str::to_string)?;
        let c = R2Client::new(&cfg).map_err(|e| connection_diagnostic(&e))?;
        c.list_page("", None, None, 1).await.map(|_| ()).map_err(|e| connection_diagnostic(&e))
    }

    pub async fn save_config(&self, cfg: R2Config) -> Result<(), String> {
        let cfg = cfg.normalized();
        cfg.validate().map_err(str::to_string)?;
        let Some(login) = self.pending_login.lock().take() else {
            self.set(VaultState::NeedsPasswords);
            return Err("Please set your password again.".into());
        };
        if !self.begin() {
            *self.pending_login.lock() = Some(login);
            return Err("Busy.".into());
        }
        let (core, c2) = (self.core.clone(), cfg.clone());
        let r = blocking(move || {
            core.credentials.setup(&c2, &login, false)?;
            core.vault.mark_passwords_set()
            // `login` is dropped (zeroed) here.
        })
        .await;
        let out = match r {
            Ok(()) => {
                *self.buckets.write() = vec![BucketEntry { id: PRIMARY_BUCKET_ID.into(), config: cfg }];
                self.set(VaultState::Unlocked);
                self.start_session(PRIMARY_BUCKET_ID).await.map_err(|e| bf_core::failures_ui::message_for(&e))
            }
            Err(e) => Err(bf_core::failures_ui::message_for(&e)),
        };
        self.end();
        out
    }

    // ------------------------------------------------------------ unlock --

    pub async fn submit_password(&self, password: SecretBytes) {
        if !self.begin() {
            return;
        }
        let core = self.core.clone();
        enum Outcome {
            Destroy,
            Unlocked(Vec<BucketEntry>, HashMap<String, LoadedKeys>),
            Wrong,
        }
        let outcome = blocking(move || {
            // 1. Destruction hash first (constant time; Argon2id runs even
            //    when none is configured).
            if core.vault.is_destruction_password(&password)? {
                return Ok(Outcome::Destroy);
            }
            // 2. Derive KEK_pwd, unwrap the DEK, open the bucket list and
            //    every bucket's master keys.
            match core.credentials.unwrap_dek_with_password(&password) {
                Ok(dek) => {
                    let buckets = core.credentials.open_buckets(&dek)?;
                    let keys = buckets.iter().map(|b| (b.id.clone(), LoadedKeys::load(&core.credentials.scoped(&b.id), &dek))).collect();
                    Ok(Outcome::Unlocked(buckets, keys))
                    // DEK dropped (zeroed) here.
                }
                Err(_) => Ok(Outcome::Wrong),
            }
        })
        .await
        .unwrap_or(Outcome::Wrong);
        match outcome {
            Outcome::Destroy => self.run_wipe().await,
            Outcome::Unlocked(buckets, keys) => {
                *self.buckets.write() = buckets;
                *self.keys.lock() = keys;
                self.set(VaultState::Unlocked);
            }
            // 3. One generic, non-revealing message.
            Outcome::Wrong => self.set(VaultState::Locked { error: Some(GENERIC_UNLOCK_ERROR.into()) }),
        }
        self.end();
    }

    // ----------------------------------------------------------- buckets --

    /// Open a bucket (closing the one that is open). No password: its keys
    /// were unsealed at unlock.
    pub async fn open_bucket(&self, id: &str) -> Result<(), String> {
        if self.session().is_some_and(|s| s.bucket_id == id) {
            return Ok(());
        }
        if !matches!(self.state(), VaultState::Unlocked) {
            return Err("The vault is locked.".into());
        }
        {
            let mut o = self.opening.lock();
            if o.is_some() {
                return Err("Busy.".into());
            }
            *o = Some(id.to_string());
        }
        self.core.notify();
        self.close_bucket().await;
        let r = self.start_session(id).await;
        *self.opening.lock() = None;
        self.core.notify();
        r.map_err(|e| {
            log_warn!("bucket open failed: {}", e.developer_message());
            "This bucket couldn't be opened.".to_string()
        })
    }

    async fn start_session(&self, id: &str) -> AppResult<()> {
        let entry = self.buckets.read().iter().find(|b| b.id == id).cloned().ok_or_else(|| AppFailure::not_found("no such bucket"))?;
        let keys = self.keys.lock().get(id).cloned().unwrap_or_default();
        let (core, id2) = (self.core.clone(), id.to_string());
        let stores = blocking(move || core.bucket_stores(&id2)).await?;
        let s = Session::start(self.core.clone(), stores, entry.config.clone(), keys).await?;
        if matches!(self.state(), VaultState::Unlocked) {
            *self.session.write() = Some(s);
            self.core.notify();
        } else {
            // Locked while opening.
            s.shutdown().await;
        }
        Ok(())
    }

    /// Back to the bucket grid: stop the open bucket's work, keep its keys
    /// (including any set or rotated meanwhile) for when it is reopened.
    pub async fn close_bucket(&self) {
        let s = self.session.write().take();
        if let Some(s) = s {
            let keys = s.export_keys();
            if matches!(self.state(), VaultState::Unlocked) {
                self.keys.lock().insert(s.bucket_id.clone(), keys);
            }
            s.shutdown().await;
            self.core.notify();
        }
    }

    fn validate_new(cfg: &R2Config) -> AppResult<R2Config> {
        let cfg = cfg.normalized();
        cfg.validate().map_err(AppFailure::conflict)?;
        Ok(cfg)
    }

    fn same_bucket(a: &R2Config, b: &R2Config) -> bool {
        a.effective_endpoint() == b.effective_endpoint() && a.bucket == b.bucket
    }

    /// Connect another bucket. The login password reseals the bucket list.
    pub async fn add_bucket(&self, password: SecretBytes, cfg: R2Config) -> AppResult<String> {
        let cfg = Self::validate_new(&cfg)?;
        let id = new_id();
        let (id2, c2) = (id.clone(), cfg.clone());
        let list = self
            .with_dek(password, move |core, dek| {
                let mut list = core.credentials.open_buckets(&dek)?;
                if list.iter().any(|b| Self::same_bucket(&b.config, &c2)) {
                    return Err(AppFailure::conflict("This bucket is already connected."));
                }
                list.push(BucketEntry { id: id2, config: c2 });
                core.credentials.seal_buckets(&dek, &list)?;
                Ok(list)
            })
            .await?;
        *self.buckets.write() = list;
        self.keys.lock().insert(id.clone(), LoadedKeys::default());
        self.core.notify();
        Ok(id)
    }

    /// Change a bucket's credentials; an open session hot-swaps its client.
    pub async fn edit_bucket(&self, password: SecretBytes, id: &str, cfg: R2Config) -> AppResult<()> {
        let cfg = Self::validate_new(&cfg)?;
        let (id2, c2) = (id.to_string(), cfg.clone());
        let list = self
            .with_dek(password, move |core, dek| {
                let mut list = core.credentials.open_buckets(&dek)?;
                if list.iter().any(|b| b.id != id2 && Self::same_bucket(&b.config, &c2)) {
                    return Err(AppFailure::conflict("This bucket is already connected."));
                }
                let e = list.iter_mut().find(|b| b.id == id2).ok_or_else(|| AppFailure::not_found("no such bucket"))?;
                e.config = c2;
                core.credentials.seal_buckets(&dek, &list)?;
                Ok(list)
            })
            .await?;
        *self.buckets.write() = list;
        if let Some(s) = self.session().filter(|s| s.bucket_id == id) {
            s.swap_config(cfg)?;
        }
        self.core.notify();
        Ok(())
    }

    /// Disconnect a bucket and erase everything this computer holds for it
    /// (listing cache, queues, thumbnails, its encryption keys). Nothing in
    /// the bucket itself is touched. The last bucket can't be removed.
    pub async fn remove_bucket(&self, password: SecretBytes, id: &str) -> AppResult<()> {
        if self.buckets.read().len() <= 1 {
            return Err(AppFailure::conflict("The last bucket can't be removed."));
        }
        let id2 = id.to_string();
        let list = self
            .with_dek(password, move |core, dek| {
                let mut list = core.credentials.open_buckets(&dek)?;
                list.retain(|b| b.id != id2);
                core.credentials.seal_buckets(&dek, &list)?;
                Ok(list)
            })
            .await?;
        if self.session().is_some_and(|s| s.bucket_id == id) {
            self.close_bucket().await;
        }
        *self.buckets.write() = list;
        self.keys.lock().remove(id);
        let (core, id2) = (self.core.clone(), id.to_string());
        blocking(move || core.destroy_bucket(&id2)).await?;
        self.core.notify();
        Ok(())
    }

    /// Lock: tear the session graph down; session keys are wiped.
    pub async fn lock(&self) {
        let s = self.session.write().take();
        self.forget_unlocked();
        if let Some(s) = s {
            s.shutdown().await;
        }
        if matches!(self.state(), VaultState::Unlocked) {
            self.set(VaultState::Locked { error: None });
        }
    }

    pub async fn wipe_everything(&self) {
        if !self.begin() {
            return;
        }
        self.run_wipe().await;
        self.end();
    }

    async fn run_wipe(&self) {
        self.set(VaultState::Wiping);
        let s = self.session.write().take();
        self.forget_unlocked();
        if let Some(s) = s {
            s.shutdown().await;
        }
        *self.pending_login.lock() = None;
        let core = self.core.clone();
        let _ = blocking(move || {
            wiper::wipe(&core);
            Ok(())
        })
        .await;
        self.set(VaultState::NeedsPasswords);
    }

    // ------------------------------------------- password-gated actions --

    /// Run `f` with the DEK unwrapped by the login password. The DEK lives
    /// only for the duration of this one operation.
    pub async fn with_dek<T: Send + 'static>(
        &self,
        password: SecretBytes,
        f: impl FnOnce(&Core, SecretBytes) -> AppResult<T> + Send + 'static,
    ) -> AppResult<T> {
        let core = self.core.clone();
        blocking(move || {
            let dek = core.credentials.unwrap_dek_with_password(&password).map_err(|_| AppFailure::crypto(GENERIC_UNLOCK_ERROR))?;
            f(&core, dek)
        })
        .await
    }

    /// Unwrap the DEK and hand it to an async consumer (set/change key).
    pub async fn dek(&self, password: SecretBytes) -> AppResult<SecretBytes> {
        self.with_dek(password, |_, dek| Ok(dek)).await
    }

    /// R2 config edit of the open bucket: reseal under the same DEK and
    /// hot-swap the session.
    pub async fn edit_config(&self, password: SecretBytes, cfg: R2Config) -> AppResult<()> {
        let id = self.session().map(|s| s.bucket_id.clone()).ok_or_else(|| AppFailure::conflict("no bucket open"))?;
        self.edit_bucket(password, &id, cfg).await
    }

    pub async fn change_login_password(&self, current: SecretBytes, new: SecretBytes) -> AppResult<()> {
        let s = String::from_utf8_lossy(&new).into_owned();
        if s.chars().count() < strength::MIN_PASSWORD_LEN {
            return Err(AppFailure::conflict("too short"));
        }
        let vault = self.core.vault.clone();
        let n2 = zeroize::Zeroizing::new(new.to_vec());
        if blocking(move || vault.is_destruction_password(&n2)).await? {
            return Err(AppFailure::conflict("must differ from the destruction password"));
        }
        self.with_dek(current, move |core, dek| core.credentials.change_login_password(&dek, &new)).await
    }

    // -------------------------------------------- destruction password --

    pub async fn destruction_enabled(&self) -> bool {
        let vault = self.core.vault.clone();
        blocking(move || vault.has_destruction_password()).await.unwrap_or(false)
    }

    /// Turn the destruction password on, or change it. The login password
    /// authorises the change and must differ from the new password.
    pub async fn set_destruction_password(&self, login: SecretBytes, destruction: SecretBytes) -> AppResult<()> {
        let d = String::from_utf8_lossy(&destruction).into_owned();
        if d.chars().count() < strength::MIN_PASSWORD_LEN {
            return Err(AppFailure::conflict("too short"));
        }
        if *login == *destruction {
            return Err(AppFailure::conflict("must differ from the login password"));
        }
        self.with_dek(login, move |core, _dek| core.vault.set_destruction_password(&destruction)).await
    }

    /// Turn the destruction password off (login password required).
    pub async fn disable_destruction_password(&self, login: SecretBytes) -> AppResult<()> {
        self.with_dek(login, |core, _dek| core.vault.clear_destruction_password()).await
    }
}
