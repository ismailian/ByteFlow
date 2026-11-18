//! AES-256-GCM envelope (spec §3.1): `nonce(12) ‖ ciphertext ‖ tag(16)`,
//! random 96-bit nonce per seal, purpose-binding AAD. A wrong key, tampered
//! ciphertext, and wrong AAD all fail identically (no oracle).

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};

use super::secure_bytes::{SecretBytes, random_bytes, secret};
use crate::failures::{AppFailure, AppResult};

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;

pub const AAD_CREDENTIALS: &[u8] = b"r2vault.v1.credentials";
pub const AAD_DEK_PWD: &[u8] = b"r2vault.v1.dek.pwd";
pub const AAD_FILE_MASTER: &[u8] = b"r2vault.v1.file.master";
pub const AAD_FILE_KEY: &[u8] = b"r2vault.v1.file.key";
/// Secure-storage blob AAD (desktop-only store; see `bf-data::secure`).
pub const AAD_SECURE_STORE: &[u8] = b"byteflow.desktop.v1.secure_store";

pub fn external_key_aad(salt_id: &str) -> Vec<u8> {
    format!("r2vault.v1.file.external:{salt_id}").into_bytes()
}

pub struct Envelope;

impl Envelope {
    pub fn seal(plaintext: &[u8], key: &[u8], aad: &[u8]) -> AppResult<Vec<u8>> {
        let cipher = cipher(key)?;
        let nonce_bytes = random_bytes(NONCE_LEN);
        let nonce = nonce_from(&nonce_bytes)?;
        let ct = cipher
            .encrypt(&nonce, Payload { msg: plaintext, aad })
            .map_err(|_| AppFailure::crypto("seal failed"))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    pub fn open(sealed: &[u8], key: &[u8], aad: &[u8]) -> AppResult<SecretBytes> {
        if sealed.len() < NONCE_LEN + TAG_LEN {
            return Err(AppFailure::crypto("open failed"));
        }
        let cipher = cipher(key)?;
        let nonce = nonce_from(&sealed[..NONCE_LEN])?;
        cipher
            .decrypt(&nonce, Payload { msg: &sealed[NONCE_LEN..], aad })
            .map(secret)
            .map_err(|_| AppFailure::crypto("open failed"))
    }
}

pub(crate) fn cipher(key: &[u8]) -> AppResult<Aes256Gcm> {
    if key.len() != KEY_LEN {
        return Err(AppFailure::crypto("key must be 32 bytes"));
    }
    Aes256Gcm::new_from_slice(key).map_err(|_| AppFailure::crypto("bad key"))
}

pub(crate) fn nonce_from(b: &[u8]) -> AppResult<Nonce<aes_gcm::aead::consts::U12>> {
    Nonce::try_from(b).map_err(|_| AppFailure::crypto("bad nonce"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_tamper() {
        let key = random_bytes(32);
        let sealed = Envelope::seal(b"hello", &key, AAD_CREDENTIALS).unwrap();
        assert_eq!(sealed.len(), 12 + 5 + 16);
        assert_eq!(&**Envelope::open(&sealed, &key, AAD_CREDENTIALS).unwrap(), b"hello");

        // Wrong AAD, wrong key, tampered — all the same failure.
        let e1 = Envelope::open(&sealed, &key, AAD_DEK_PWD).unwrap_err();
        let e2 = Envelope::open(&sealed, &random_bytes(32), AAD_CREDENTIALS).unwrap_err();
        let mut t = sealed.clone();
        t[14] ^= 1;
        let e3 = Envelope::open(&t, &key, AAD_CREDENTIALS).unwrap_err();
        assert_eq!(e1, e2);
        assert_eq!(e2, e3);
        assert!(Envelope::seal(b"x", &[0u8; 16], b"").is_err());
    }
}
