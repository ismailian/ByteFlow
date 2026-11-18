//! The top-level vault state machine (spec §2.4, §3.2–§3.6).
//!
//! ```text
//! Loading ─(wipe_pending?)→ resume wipe → NeedsPasswords
//!    ├─ configured → Locked ⇄ Unlocked(session)
//!    │                 └─ destruction pwd / wipe-everything → Wiping → NeedsPasswords
//!    └─ not configured → NeedsPasswords → NeedsConfig → Unlocked
//! ```
//!
//! One password field, three outcomes; up to success the behaviour is
//! indistinguishable (both checks run Argon2id). A wipe ends on onboarding
//! with **no** message — it must look exactly like a fresh install.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::{Mutex, RwLock};

use bf_core::crypto::SecretBytes;
use bf_core::failures_ui::connection_diagnostic;
use bf_core::{AppFailure, AppResult, log_warn};
use bf_data::r2::{R2Client, R2Config};

use crate::core::Core;
use crate::encryption::keys::LoadedKeys;
use crate::session::Session;
use crate::{blocking, strength, wiper};

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
    busy: AtomicBool,
}

impl AuthController {
    pub fn new(core: Arc<Core>) -> Arc<Self> {
        Arc::new(Self {
            core,
            state: RwLock::new(VaultState::Loading),
            pending_login: Mutex::new(None),
            session: RwLock::new(None),
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

    pub async fn set_passwords(&self, login: SecretBytes, destruction: SecretBytes) -> Result<(), String> {
        let (l, d) = (String::from_utf8_lossy(&login).into_owned(), String::from_utf8_lossy(&destruction).into_owned());
        strength::validate_pair(&l, &l, &d, &d)?;
        if !self.begin() {
            return Err("Busy.".into());
        }
        let vault = self.core.vault.clone();
        // The destruction hash is written immediately.
        let r = blocking(move || vault.set_destruction_password(&destruction)).await;
        self.end();
        r.map_err(|_| "Couldn't save. Try again.".to_string())?;
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
            return Err("Please set your passwords again.".into());
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
            Ok(()) => self.open_session(cfg, LoadedKeys::default()).await.map_err(|e| bf_core::failures_ui::message_for(&e)),
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
            Unlocked(Box<R2Config>, LoadedKeys),
            Wrong,
        }
        let outcome = blocking(move || {
            // 1. Destruction hash first (constant time).
            if core.vault.is_destruction_password(&password)? {
                return Ok(Outcome::Destroy);
            }
            // 2. Derive KEK_pwd, unwrap the DEK, open the blob.
            match core.credentials.unwrap_dek_with_password(&password) {
                Ok(dek) => {
                    let cfg = core.credentials.open_config(&dek)?;
                    let keys = LoadedKeys::load(&core.credentials, &dek);
                    Ok(Outcome::Unlocked(Box::new(cfg), keys))
                    // DEK dropped (zeroed) here.
                }
                Err(_) => Ok(Outcome::Wrong),
            }
        })
        .await
        .unwrap_or(Outcome::Wrong);
        match outcome {
            Outcome::Destroy => self.run_wipe().await,
            Outcome::Unlocked(cfg, keys) => {
                if let Err(e) = self.open_session(*cfg, keys).await {
                    log_warn!("session start failed: {}", e.developer_message());
                    self.set(VaultState::Locked { error: Some(GENERIC_UNLOCK_ERROR.into()) });
                }
            }
            // 3. One generic, non-revealing message.
            Outcome::Wrong => self.set(VaultState::Locked { error: Some(GENERIC_UNLOCK_ERROR.into()) }),
        }
        self.end();
    }

    async fn open_session(&self, cfg: R2Config, keys: LoadedKeys) -> AppResult<()> {
        let s = Session::start(self.core.clone(), cfg, keys).await?;
        *self.session.write() = Some(s);
        self.set(VaultState::Unlocked);
        Ok(())
    }

    /// Lock: tear the session graph down; session keys are wiped.
    pub async fn lock(&self) {
        let s = self.session.write().take();
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

    /// R2 config edit: reseal under the same DEK and hot-swap the session.
    pub async fn edit_config(&self, password: SecretBytes, cfg: R2Config) -> AppResult<()> {
        let cfg = cfg.normalized();
        cfg.validate().map_err(AppFailure::conflict)?;
        let c2 = cfg.clone();
        self.with_dek(password, move |core, dek| core.credentials.reseal_config(&dek, &c2)).await?;
        if let Some(s) = self.session() {
            s.swap_config(cfg)?;
        }
        Ok(())
    }

    pub async fn change_login_password(&self, current: SecretBytes, new: SecretBytes) -> AppResult<()> {
        let s = String::from_utf8_lossy(&new).into_owned();
        if s.chars().count() < strength::MIN_PASSWORD_LEN {
            return Err(AppFailure::conflict("too short"));
        }
        let vault = self.core.vault.clone();
        let n2 = zeroize::Zeroizing::new(new.to_vec());
        if blocking(move || vault.is_destruction_password(&n2)).await? {
            return Err(AppFailure::conflict("must differ from the other password"));
        }
        self.with_dek(current, move |core, dek| core.credentials.change_login_password(&dek, &new)).await
    }
}
