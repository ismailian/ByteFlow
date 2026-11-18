//! Credential blob, DEK wraps, file master keys and the external-key table
//! (spec §3.1–§3.2, §9.2, §9.9, §19.2).
//!
//! All operations are synchronous and CPU-heavy (Argon2id); the application
//! layer calls them from `spawn_blocking`.

use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine;

use super::key_manager::KeyManager;
use super::store::SecureStore;
use crate::r2::R2Config;
use bf_core::crypto::envelope::{
    AAD_CREDENTIALS, AAD_DEK_PWD, AAD_FILE_MASTER, Envelope, KEY_LEN, external_key_aad,
};
use bf_core::crypto::file_crypto::KdfInfo;
use bf_core::crypto::secure_bytes::{random_bytes, random_secret};
use bf_core::crypto::{Argon2Params, PasswordHasher, SecretBytes};
use bf_core::{AppFailure, AppResult};

pub mod keys {
    pub const BLOB: &str = "credential_blob";
    pub const WRAP_BIO: &str = "dek_wrap_bio";
    pub const WRAP_PWD: &str = "dek_wrap_pwd";
    pub const WRAP_PWD_SALT: &str = "dek_wrap_pwd_salt";
    pub const MASTER: &str = "enc_master_blob";
    pub const MASTER_PENDING: &str = "enc_master_pending";
    pub const MASTER_SALT: &str = "enc_master_kdf_salt";
    pub const MASTER_PARAMS: &str = "enc_master_kdf_params";
    pub const MASTER_PENDING_SALT: &str = "enc_master_pending_kdf_salt";
    pub const MASTER_PENDING_PARAMS: &str = "enc_master_pending_kdf_params";
    pub const EXTERNAL: &str = "enc_external_keys";
}

const SALT_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MasterSlot {
    Active,
    Pending,
}

impl MasterSlot {
    fn keys(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Active => (keys::MASTER, keys::MASTER_SALT, keys::MASTER_PARAMS),
            Self::Pending => (keys::MASTER_PENDING, keys::MASTER_PENDING_SALT, keys::MASTER_PENDING_PARAMS),
        }
    }
}

pub struct MasterKey {
    pub key: SecretBytes,
    /// `None` only for an epoch that predates v2 (falls back to v1, §9.7).
    pub kdf: Option<KdfInfo>,
}

pub struct CredentialStore {
    store: Arc<dyn SecureStore>,
    hw: Arc<dyn KeyManager>,
    kdf: Argon2Params,
}

impl CredentialStore {
    pub fn new(store: Arc<dyn SecureStore>, hw: Arc<dyn KeyManager>) -> Self {
        Self { store, hw, kdf: Argon2Params::default() }
    }

    /// Tests only: cheap Argon2.
    pub fn with_kdf(mut self, kdf: Argon2Params) -> Self {
        self.kdf = kdf;
        self
    }

    pub fn kdf_params(&self) -> Argon2Params {
        self.kdf
    }

    /// The blob exists **and** at least one wrap exists.
    pub fn is_configured(&self) -> AppResult<bool> {
        Ok(self.store.contains(keys::BLOB)?
            && (self.store.contains(keys::WRAP_PWD)? || self.store.contains(keys::WRAP_BIO)?))
    }

    pub fn has_bio_wrap(&self) -> AppResult<bool> {
        self.store.contains(keys::WRAP_BIO)
    }

    pub fn biometrics_available(&self) -> bool {
        self.hw.is_available()
    }

    /// Setup, in crash-safe order (§3.2): password wrap first, biometric wrap
    /// second (optional), blob last.
    pub fn setup(&self, config: &R2Config, login_password: &[u8], with_biometrics: bool) -> AppResult<()> {
        let dek = random_secret(KEY_LEN);
        let json = zeroize::Zeroizing::new(
            serde_json::to_vec(config).map_err(|_| AppFailure::unknown("serialize config"))?,
        );
        let blob = Envelope::seal(&json, &dek, AAD_CREDENTIALS)?;
        self.write_password_wrap(&dek, login_password)?;
        if with_biometrics && self.hw.is_available() {
            let wrap = self.hw.wrap(&dek)?;
            self.store.write(keys::WRAP_BIO, &wrap)?;
        }
        self.store.write(keys::BLOB, &blob)?;
        Ok(())
    }

    fn write_password_wrap(&self, dek: &[u8], password: &[u8]) -> AppResult<()> {
        let salt = random_bytes(SALT_LEN);
        let kek = PasswordHasher::derive_key(password, &salt, self.kdf)?;
        let wrap = Envelope::seal(dek, &kek, AAD_DEK_PWD)?;
        // Salt before wrap: a wrap without its salt would be unopenable.
        self.store.write(keys::WRAP_PWD_SALT, &salt)?;
        self.store.write(keys::WRAP_PWD, &wrap)
    }

    /// Derive `KEK_pwd` and unwrap the DEK. Wrong password → `CryptoFailure`.
    pub fn unwrap_dek_with_password(&self, password: &[u8]) -> AppResult<SecretBytes> {
        let salt = self.store.read(keys::WRAP_PWD_SALT)?;
        let wrap = self.store.read(keys::WRAP_PWD)?;
        let (Some(salt), Some(wrap)) = (salt, wrap) else {
            // Still burn an Argon2 so timing doesn't reveal the missing wrap.
            let _ = PasswordHasher::derive_key(password, &[0u8; SALT_LEN], self.kdf);
            return Err(AppFailure::crypto("no password wrap"));
        };
        let kek = PasswordHasher::derive_key(password, &salt, self.kdf)?;
        Envelope::open(&wrap, &kek, AAD_DEK_PWD)
    }

    pub fn unwrap_dek_with_biometrics(&self) -> AppResult<SecretBytes> {
        let wrap = self
            .store
            .read(keys::WRAP_BIO)?
            .ok_or_else(|| AppFailure::Biometric {
                reason: bf_core::BiometricFailureReason::NotAvailable,
                message: "no biometric wrap".into(),
            })?;
        self.hw.unwrap(&wrap)
    }

    pub fn open_config(&self, dek: &[u8]) -> AppResult<R2Config> {
        let blob = self.store.read(keys::BLOB)?.ok_or_else(|| AppFailure::not_found("no credential blob"))?;
        let json = Envelope::open(&blob, dek, AAD_CREDENTIALS)?;
        serde_json::from_slice(&json).map_err(|_| AppFailure::crypto("credential blob corrupt"))
    }

    /// Reseal the blob under the same DEK (config edit, §13).
    pub fn reseal_config(&self, dek: &[u8], config: &R2Config) -> AppResult<()> {
        let json = zeroize::Zeroizing::new(
            serde_json::to_vec(config).map_err(|_| AppFailure::unknown("serialize config"))?,
        );
        let blob = Envelope::seal(&json, dek, AAD_CREDENTIALS)?;
        self.store.write(keys::BLOB, &blob)
    }

    /// Rewrites only the password wrap.
    pub fn change_login_password(&self, dek: &[u8], new_password: &[u8]) -> AppResult<()> {
        self.write_password_wrap(dek, new_password)
    }

    pub fn delete_bio_key(&self) -> AppResult<()> {
        self.hw.delete_key()
    }

    // ---------------------------------------------------- file master key --

    pub fn has_master(&self, slot: MasterSlot) -> AppResult<bool> {
        self.store.contains(slot.keys().0)
    }

    pub fn seal_master(&self, dek: &[u8], slot: MasterSlot, key: &[u8], kdf: &KdfInfo) -> AppResult<()> {
        let (k_blob, k_salt, k_params) = slot.keys();
        let sealed = Envelope::seal(key, dek, AAD_FILE_MASTER)?;
        self.store.write(k_salt, &kdf.salt)?;
        self.store.write_string(k_params, &kdf.params.encode())?;
        self.store.write(k_blob, &sealed)
    }

    pub fn open_master(&self, dek: &[u8], slot: MasterSlot) -> AppResult<Option<MasterKey>> {
        let (k_blob, k_salt, k_params) = slot.keys();
        let Some(sealed) = self.store.read(k_blob)? else { return Ok(None) };
        let key = Envelope::open(&sealed, dek, AAD_FILE_MASTER)?;
        let salt = self.store.read(k_salt)?;
        let params = self.store.read_string(k_params)?.and_then(|p| Argon2Params::decode(&p));
        let kdf = match (salt, params) {
            (Some(s), Some(p)) => Some(KdfInfo { salt: s.to_vec(), params: p }),
            _ => None,
        };
        Ok(Some(MasterKey { key, kdf }))
    }

    /// Promote pending → active: a pure move, no unwrap (§9.4).
    pub fn promote_pending(&self) -> AppResult<()> {
        let (pb, ps, pp) = MasterSlot::Pending.keys();
        let (ab, as_, ap) = MasterSlot::Active.keys();
        let blob = self.store.read(pb)?.ok_or_else(|| AppFailure::not_found("no pending master"))?;
        match self.store.read(ps)? {
            Some(s) => self.store.write(as_, &s)?,
            None => self.store.delete(as_)?,
        }
        match self.store.read(pp)? {
            Some(p) => self.store.write(ap, &p)?,
            None => self.store.delete(ap)?,
        }
        self.store.write(ab, &blob)?;
        for k in [pb, ps, pp] {
            self.store.delete(k)?;
        }
        Ok(())
    }

    pub fn delete_pending(&self) -> AppResult<()> {
        let (b, s, p) = MasterSlot::Pending.keys();
        for k in [b, s, p] {
            self.store.delete(k)?;
        }
        Ok(())
    }

    /// Removal finalisation: master key, salt/params, external table.
    pub fn delete_all_master(&self) -> AppResult<()> {
        self.delete_pending()?;
        let (b, s, p) = MasterSlot::Active.keys();
        for k in [b, s, p, keys::EXTERNAL] {
            self.store.delete(k)?;
        }
        Ok(())
    }

    // ------------------------------------------------ external-key table --

    fn read_external_raw(&self) -> AppResult<BTreeMap<String, String>> {
        match self.store.read(keys::EXTERNAL)? {
            None => Ok(BTreeMap::new()),
            Some(b) => Ok(serde_json::from_slice(&b).unwrap_or_default()),
        }
    }

    pub fn external_key(&self, salt_id: &str, file_master: &[u8]) -> AppResult<Option<SecretBytes>> {
        let raw = self.read_external_raw()?;
        let Some(enc) = raw.get(salt_id) else { return Ok(None) };
        let sealed = base64::engine::general_purpose::STANDARD
            .decode(enc)
            .map_err(|_| AppFailure::crypto("external table corrupt"))?;
        Envelope::open(&sealed, file_master, &external_key_aad(salt_id)).map(Some)
    }

    pub fn store_external_key(&self, salt_id: &str, key: &[u8], file_master: &[u8]) -> AppResult<()> {
        let mut raw = self.read_external_raw()?;
        let sealed = Envelope::seal(key, file_master, &external_key_aad(salt_id))?;
        raw.insert(salt_id.to_string(), base64::engine::general_purpose::STANDARD.encode(sealed));
        self.store.write(keys::EXTERNAL, &serde_json::to_vec(&raw).map_err(|_| AppFailure::unknown("serialize"))?)
    }

    /// Re-seal the whole table from `old` to `new` master key (rotation).
    pub fn reseal_external_table(&self, old: &[u8], new: &[u8]) -> AppResult<()> {
        let raw = self.read_external_raw()?;
        let mut out = BTreeMap::new();
        for (id, enc) in raw {
            let Ok(sealed) = base64::engine::general_purpose::STANDARD.decode(&enc) else { continue };
            let Ok(k) = Envelope::open(&sealed, old, &external_key_aad(&id)) else { continue };
            let resealed = Envelope::seal(&k, new, &external_key_aad(&id))?;
            out.insert(id, base64::engine::general_purpose::STANDARD.encode(resealed));
        }
        self.store.write(keys::EXTERNAL, &serde_json::to_vec(&out).map_err(|_| AppFailure::unknown("serialize"))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure::{MemorySecureStore, NoHardwareKeyManager};

    fn cs() -> (CredentialStore, Arc<MemorySecureStore>) {
        let s = Arc::new(MemorySecureStore::default());
        (CredentialStore::new(s.clone(), Arc::new(NoHardwareKeyManager)).with_kdf(Argon2Params::test_weak()), s)
    }

    fn cfg() -> R2Config {
        R2Config { account_id: "a".into(), access_key_id: "k".into(), secret_access_key: "s".into(), bucket: "b".into(), public_url: None, endpoint: None }
    }

    #[test]
    fn setup_unlock_and_password_change() {
        let (c, _) = cs();
        assert!(!c.is_configured().unwrap());
        c.setup(&cfg(), b"login-password", true).unwrap();
        assert!(c.is_configured().unwrap());
        assert!(!c.has_bio_wrap().unwrap(), "no hardware backend → password-only");
        let dek = c.unwrap_dek_with_password(b"login-password").unwrap();
        assert_eq!(c.open_config(&dek).unwrap(), cfg());
        assert!(c.unwrap_dek_with_password(b"wrong").is_err());
        c.change_login_password(&dek, b"new-password!").unwrap();
        assert!(c.unwrap_dek_with_password(b"login-password").is_err());
        let dek2 = c.unwrap_dek_with_password(b"new-password!").unwrap();
        assert_eq!(*dek, *dek2);
    }

    #[test]
    fn master_slots_and_external_table() {
        let (c, _) = cs();
        c.setup(&cfg(), b"pw", false).unwrap();
        let dek = c.unwrap_dek_with_password(b"pw").unwrap();
        let k1 = random_bytes(32);
        let kdf = KdfInfo { salt: vec![1; 16], params: Argon2Params::default() };
        c.seal_master(&dek, MasterSlot::Active, &k1, &kdf).unwrap();
        let m = c.open_master(&dek, MasterSlot::Active).unwrap().unwrap();
        assert_eq!(*m.key, k1);
        assert_eq!(m.kdf, Some(kdf.clone()));

        let ext = random_bytes(32);
        c.store_external_key("SALT", &ext, &k1).unwrap();
        assert_eq!(*c.external_key("SALT", &k1).unwrap().unwrap(), ext);

        let k2 = random_bytes(32);
        c.seal_master(&dek, MasterSlot::Pending, &k2, &kdf).unwrap();
        c.reseal_external_table(&k1, &k2).unwrap();
        c.promote_pending().unwrap();
        assert!(!c.has_master(MasterSlot::Pending).unwrap());
        assert_eq!(*c.open_master(&dek, MasterSlot::Active).unwrap().unwrap().key, k2);
        assert_eq!(*c.external_key("SALT", &k2).unwrap().unwrap(), ext);
        assert!(c.external_key("SALT", &k1).is_err());
        c.delete_all_master().unwrap();
        assert!(!c.has_master(MasterSlot::Active).unwrap());
    }
}
