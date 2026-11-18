//! Session keys (spec §9.2, §3.7): `epoch → master key`, the external
//! passphrase, and keys derived for external objects this session. Held in
//! memory only and wiped on lock (`clear_all`).

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use bf_core::crypto::SecretBytes;
use bf_core::crypto::file_crypto::KdfInfo;
use bf_core::{AppResult, log_warn};
use bf_data::secure::CredentialStore;
use bf_data::secure::credential_store::MasterSlot;

#[derive(Clone)]
pub struct EpochKey {
    pub key: SecretBytes,
    /// `None` only for an epoch predating v2 (encrypts as v1, §9.7).
    pub kdf: Option<KdfInfo>,
}

/// Master keys unsealed with the DEK at unlock, before the DEK is wiped.
/// Desktop is password-only (§17.2), so loading them here avoids a second
/// password prompt for every encryption operation; they are session keys and
/// are wiped on lock exactly as on mobile.
#[derive(Default, Clone)]
pub struct LoadedKeys {
    pub active: Option<EpochKey>,
    pub pending: Option<EpochKey>,
}

impl LoadedKeys {
    /// `cred` is the bucket's scoped view (`CredentialStore::scoped`).
    pub fn load(cred: &CredentialStore, dek: &[u8]) -> Self {
        let get = |slot| match cred.open_master(dek, slot) {
            Ok(Some(m)) => Some(EpochKey { key: m.key, kdf: m.kdf }),
            Ok(None) => None,
            Err(e) => {
                log_warn!("master key unseal failed: {}", e.developer_message());
                None
            }
        };
        Self { active: get(MasterSlot::Active), pending: get(MasterSlot::Pending) }
    }
}

#[derive(Default)]
pub struct EncryptionSession {
    epochs: RwLock<HashMap<i64, Arc<EpochKey>>>,
    passphrase: RwLock<Option<SecretBytes>>,
    external: RwLock<HashMap<String, SecretBytes>>,
}

impl EncryptionSession {
    pub fn get(&self, epoch: i64) -> Option<Arc<EpochKey>> {
        self.epochs.read().get(&epoch).cloned()
    }
    pub fn insert(&self, epoch: i64, k: EpochKey) {
        self.epochs.write().insert(epoch, Arc::new(k));
    }
    pub fn remove(&self, epoch: i64) {
        self.epochs.write().remove(&epoch);
    }
    pub fn loaded_epochs(&self) -> Vec<i64> {
        self.epochs.read().keys().copied().collect()
    }
    pub fn all(&self) -> Vec<(i64, Arc<EpochKey>)> {
        self.epochs.read().iter().map(|(e, k)| (*e, k.clone())).collect()
    }
    pub fn set_passphrase(&self, p: SecretBytes) {
        *self.passphrase.write() = Some(p);
    }
    pub fn passphrase(&self) -> Option<SecretBytes> {
        self.passphrase.read().as_ref().map(|p| zeroize::Zeroizing::new(p.to_vec()))
    }
    pub fn external_key(&self, salt_id: &str) -> Option<SecretBytes> {
        self.external.read().get(salt_id).map(|k| zeroize::Zeroizing::new(k.to_vec()))
    }
    pub fn put_external_key(&self, salt_id: &str, k: &[u8]) {
        self.external.write().insert(salt_id.to_string(), zeroize::Zeroizing::new(k.to_vec()));
    }
    /// Wipe everything (lock). `Zeroizing` buffers are zeroed on drop.
    pub fn clear_all(&self) {
        self.epochs.write().clear();
        *self.passphrase.write() = None;
        self.external.write().clear();
    }
}

pub fn ok<T>(r: AppResult<T>) -> Option<T> {
    r.ok()
}
