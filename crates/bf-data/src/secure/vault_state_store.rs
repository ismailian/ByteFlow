//! Destruction hash, flags, and preferences (spec §3.5, §13). Everything here
//! lives in secure storage, so a wipe erases it.

use std::sync::Arc;

use super::store::SecureStore;
use bf_core::crypto::secure_bytes::random_bytes;
use bf_core::crypto::{Argon2Params, PasswordHasher};
use bf_core::{AppFailure, AppResult};

pub mod keys {
    pub const DESTRUCTION_HASH: &str = "destruction_hash";
    pub const DESTRUCTION_SALT: &str = "destruction_salt";
    pub const WIPE_PENDING: &str = "wipe_pending";
    pub const PASSWORDS_SET: &str = "passwords_set";
}

pub struct VaultStateStore {
    store: Arc<dyn SecureStore>,
    kdf: Argon2Params,
}

impl VaultStateStore {
    pub fn new(store: Arc<dyn SecureStore>) -> Self {
        Self { store, kdf: Argon2Params::default() }
    }

    pub fn with_kdf(mut self, kdf: Argon2Params) -> Self {
        self.kdf = kdf;
        self
    }

    pub fn store(&self) -> &Arc<dyn SecureStore> {
        &self.store
    }

    /// Argon2id with its own, never-reused salt.
    pub fn set_destruction_password(&self, password: &[u8]) -> AppResult<()> {
        let salt = random_bytes(16);
        let hash = PasswordHasher::derive_key(password, &salt, self.kdf)?;
        self.store.write(keys::DESTRUCTION_SALT, &salt)?;
        self.store.write(keys::DESTRUCTION_HASH, &hash)
    }

    /// Whether a destruction password is configured. Optional since 0.4.0:
    /// installs that set one keep it; fresh installs start without one.
    pub fn has_destruction_password(&self) -> AppResult<bool> {
        Ok(self.store.contains(keys::DESTRUCTION_HASH)? && self.store.contains(keys::DESTRUCTION_SALT)?)
    }

    /// Turn the destruction password off (hash first, so a half-done delete
    /// can never leave a hash without its salt being checked against).
    pub fn clear_destruction_password(&self) -> AppResult<()> {
        self.store.delete(keys::DESTRUCTION_HASH)?;
        self.store.delete(keys::DESTRUCTION_SALT)
    }

    /// Constant-time check. Always runs Argon2id (even with no hash stored) so
    /// timing is comparable with the login path (§3.4).
    pub fn is_destruction_password(&self, password: &[u8]) -> AppResult<bool> {
        let salt = self.store.read(keys::DESTRUCTION_SALT)?;
        let hash = self.store.read(keys::DESTRUCTION_HASH)?;
        match (salt, hash) {
            (Some(s), Some(h)) => PasswordHasher::verify(password, &s, &h, self.kdf),
            _ => {
                let _ = PasswordHasher::derive_key(password, &[0u8; 16], self.kdf);
                Ok(false)
            }
        }
    }

    pub fn wipe_pending(&self) -> AppResult<bool> {
        self.store.contains(keys::WIPE_PENDING)
    }
    pub fn set_wipe_pending(&self) -> AppResult<()> {
        self.store.write(keys::WIPE_PENDING, b"1")
    }
    pub fn clear_wipe_pending(&self) -> AppResult<()> {
        self.store.delete(keys::WIPE_PENDING)
    }

    pub fn passwords_set(&self) -> AppResult<bool> {
        self.store.contains(keys::PASSWORDS_SET)
    }
    pub fn mark_passwords_set(&self) -> AppResult<()> {
        self.store.write(keys::PASSWORDS_SET, b"1")
    }

    pub fn pref(&self, key: &str) -> Option<String> {
        self.store.read_string(key).ok().flatten()
    }
    pub fn set_pref(&self, key: &str, value: Option<&str>) -> AppResult<()> {
        match value {
            Some(v) => self.store.write_string(key, v),
            None => self.store.delete(key),
        }
    }
}

pub fn ensure(cond: bool, msg: &str) -> AppResult<()> {
    if cond { Ok(()) } else { Err(AppFailure::unknown(msg)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure::MemorySecureStore;

    #[test]
    fn destruction_password() {
        let v = VaultStateStore::new(Arc::new(MemorySecureStore::default())).with_kdf(Argon2Params::test_weak());
        assert!(!v.is_destruction_password(b"x").unwrap());
        assert!(!v.has_destruction_password().unwrap());
        v.set_destruction_password(b"destroy-me-now").unwrap();
        assert!(v.has_destruction_password().unwrap());
        assert!(v.is_destruction_password(b"destroy-me-now").unwrap());
        assert!(!v.is_destruction_password(b"destroy-me-not").unwrap());
        v.clear_destruction_password().unwrap();
        assert!(!v.has_destruction_password().unwrap());
        assert!(!v.is_destruction_password(b"destroy-me-now").unwrap());
        v.set_pref("auto_lock_seconds", Some("60")).unwrap();
        assert_eq!(v.pref("auto_lock_seconds").as_deref(), Some("60"));
    }
}
